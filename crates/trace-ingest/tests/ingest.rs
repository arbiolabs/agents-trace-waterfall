use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use trace_adapter_sdk::{Client, MAX_LINE_BYTES, read_jsonl};
use trace_core::*;
use trace_ingest::{Capture, CollectorConfig, import_file, run_collector_until};
use trace_storage::{AppendOutcome, SessionFilter, Store};

const YEAR: Duration = Duration::from_secs(365 * 86_400);
/// 2026-10-22T10:00:00Z, the first timestamp in the claude-code fixture.
const T0: Ns = 1_792_663_200_000_000_000;
const MS: Ns = 1_000_000;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Env {
    dir: PathBuf,
    db: PathBuf,
    sock: PathBuf,
}

fn env() -> Env {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("tw-ingest-{pid}-{n}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Env {
        db: dir.join("trace.db"),
        sock: PathBuf::from(format!("/tmp/tw-{pid}-{n}.sock")),
        dir,
    }
}

fn config(env: &Env, tail: &[(&str, &Path)], stale_after: Duration) -> CollectorConfig {
    CollectorConfig {
        capture: Default::default(),
        socket_path: env.sock.clone(),
        db_path: env.db.clone(),
        stale_after,
        tail: tail
            .iter()
            .map(|(a, p)| (a.to_string(), p.to_path_buf()))
            .collect(),
    }
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap().unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

fn start(cfg: CollectorConfig) -> Running {
    let sock = cfg.socket_path.clone();
    let (tx, rx) = oneshot::channel();
    let thread = std::thread::spawn(move || run_collector_until(cfg, rx));
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(&sock).is_err() {
        assert!(
            !thread.is_finished(),
            "collector exited: {:?}",
            thread.join().unwrap().err()
        );
        assert!(Instant::now() < deadline, "collector did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    Running {
        stop: Some(tx),
        thread: Some(thread),
    }
}

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code")
}

fn read_events(rel: &str) -> Vec<CanonicalEvent> {
    read_jsonl(&std::fs::read(repo_root().join(rel)).unwrap()[..]).unwrap()
}

/// Sends each line and collects one reply per line until the server closes.
fn raw(sock: &Path, lines: &[String]) -> Vec<Value> {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut replies = Vec::new();
    for l in lines {
        if s.write_all(l.as_bytes()).is_err() {
            break;
        }
        let mut reply = String::new();
        if r.read_line(&mut reply).unwrap() == 0 {
            break;
        }
        replies.push(serde_json::from_str(&reply).unwrap());
    }
    replies
}

fn hello(source: &str) -> String {
    format!(
        "{}\n",
        json!({"type": "hello", "protocol": 1, "source_id": source})
    )
}

fn only_session(store: &Store) -> trace_storage::SessionSummary {
    let mut s = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(s.len(), 1);
    s.remove(0)
}

fn journal_len(store: &Store) -> usize {
    store.events_since(0, None).unwrap().len()
}

#[test]
fn streams_acks_and_restart_does_not_duplicate() {
    let env = env();
    let events = read_events("examples/sample-trace.jsonl");
    let source = events[0].source_id.clone();
    let c = start(config(&env, &[], YEAR));
    let mut client = Client::new(&env.sock, source.clone(), 1000);
    client.send(&events).unwrap();
    assert_eq!(client.flush(Duration::from_secs(5)).unwrap(), Some(70));

    let err = run_collector_until(config(&env, &[], YEAR), oneshot::channel().1).unwrap_err();
    assert_eq!(
        err.to_string(),
        format!(
            "another collector is already listening on {}",
            env.sock.display()
        )
    );
    c.stop();
    assert!(!env.sock.exists(), "clean shutdown removes the socket");

    drop(std::os::unix::net::UnixListener::bind(&env.sock).unwrap());
    assert!(
        env.sock.exists(),
        "a crashed collector leaves its socket behind"
    );
    let c = start(config(&env, &[], YEAR));
    assert_eq!(
        raw(&env.sock, &[hello(&source.0)]),
        vec![json!({"type": "welcome", "protocol": 1, "cursor": 70})]
    );
    let resend = format!("{}\n", json!({"type": "events", "events": events}));
    assert_eq!(
        raw(&env.sock, &[hello("another-source"), resend])[1],
        json!({"type": "ack", "cursor": null})
    );
    c.stop();

    let mut store = Store::open(&env.db).unwrap();
    let s = only_session(&store);
    assert_eq!((s.status, s.span_count), (SessionStatus::Completed, 21));
    assert_eq!(journal_len(&store), 70);
    assert_eq!(
        store.append(&events).unwrap(),
        AppendOutcome {
            inserted: 0,
            duplicates: 70
        }
    );
}

#[test]
fn reconnect_backfills_after_collector_dies_mid_stream() {
    let env = env();
    let events = read_events("examples/sample-trace.jsonl");
    let c = start(config(&env, &[], YEAR));
    let mut client = Client::new(&env.sock, events[0].source_id.clone(), 1000);
    client.send(&events[..30]).unwrap();
    assert_eq!(client.flush(Duration::from_secs(5)).unwrap(), Some(30));
    c.stop();

    client.send(&events[30..]).unwrap();
    assert!(client.flush(Duration::from_millis(200)).is_err());
    assert_eq!(client.pending(), 40);
    let c = start(config(&env, &[], YEAR));
    assert_eq!(client.flush(Duration::from_secs(10)).unwrap(), Some(70));
    c.stop();

    let store = Store::open(&env.db).unwrap();
    assert_eq!(journal_len(&store), 70);
    assert_eq!(only_session(&store).status, SessionStatus::Completed);
}

#[test]
fn restart_reconciles_silent_live_session_as_interrupted() {
    let env = env();
    {
        let mut store = Store::open(&env.db).unwrap();
        store
            .append(&read_events("tests/fixtures/interrupted.jsonl"))
            .unwrap();
        assert_eq!(only_session(&store).status, SessionStatus::Live);
    }
    let c = start(config(&env, &[], Duration::from_secs(60)));
    let store = Store::open(&env.db).unwrap();
    wait_for("interrupted status", || {
        only_session(&store).status == SessionStatus::Interrupted
    });
    c.stop();
}

#[test]
fn protocol_errors_close_the_connection() {
    let env = env();
    let c = start(config(&env, &[], YEAR));
    let unsupported = format!(
        "{}\n",
        json!({"type": "hello", "protocol": 2, "source_id": "s"})
    );
    assert_eq!(
        raw(&env.sock, &[unsupported]),
        vec![
            json!({"type": "error", "message": "unsupported protocol 2; this collector speaks 1"})
        ]
    );
    let oversize = format!("{}\n", "x".repeat(MAX_LINE_BYTES));
    let replies = raw(&env.sock, &[hello("s"), oversize, hello("s")]);
    assert_eq!(
        replies,
        vec![
            json!({"type": "welcome", "protocol": 1, "cursor": null}),
            json!({"type": "error", "message": "line exceeds 1048576 bytes"})
        ]
    );
    c.stop();
}

fn import_fixture(store: &mut Store) -> AppendOutcome {
    let a = import_file(store, &fixture().join("sess-1.jsonl"), "claude-code", &Capture::default()).unwrap();
    let b = import_file(
        store,
        &fixture().join("sess-1/subagents/agent-agent1.jsonl"),
        "claude-code",
        &Capture::default(),
    )
    .unwrap();
    AppendOutcome {
        inserted: a.inserted + b.inserted,
        duplicates: a.duplicates + b.duplicates,
    }
}

#[test]
fn claude_code_transcript_becomes_metadata_only_spans() {
    let mut store = Store::open_in_memory().unwrap();
    assert_eq!(
        import_fixture(&mut store),
        AppendOutcome {
            inserted: 35,
            duplicates: 0
        }
    );
    let s = store.load_session(&"sess-1".into()).unwrap();
    assert_eq!(
        (
            s.session.project_id.as_str(),
            s.session.working_directory.as_str(),
            s.session.harness.as_str(),
            s.session.status
        ),
        (
            "demo-proj",
            "/work/demo-proj",
            "claude-code",
            SessionStatus::Live
        )
    );

    let span = |id: &str| {
        s.spans
            .get(&SpanId::from(id))
            .unwrap_or_else(|| panic!("no span {id}"))
    };
    let mut kinds: Vec<_> = s
        .spans
        .values()
        .map(|sp| format!("{:?}", sp.kind))
        .collect();
    kinds.sort();
    assert_eq!(
        kinds,
        [
            "Agent", "Llm", "Llm", "Llm", "Llm", "Llm", "Llm", "Subagent", "Tool", "Tool", "Tool",
            "Turn", "Turn"
        ]
    );

    let m1 = span("llm:m1");
    assert_eq!(
        (m1.started_at_utc_ns, m1.ended_at_utc_ns, m1.status),
        (T0 + 500 * MS, Some(T0 + 2500 * MS), SpanStatus::Succeeded)
    );
    assert_eq!(
        (
            m1.parent_span_id.as_ref().unwrap().0.as_str(),
            m1.model.as_deref()
        ),
        ("turn:p1", Some("claude-opus-4-1"))
    );
    let usage = Usage {
        tokens_input: Some(3),
        tokens_output: Some(10),
        cache_read: Some(100),
        cache_write: Some(50),
        // Claude Code reports no cost, so Opus 4.1's list prices estimate it: 3×$15 + 10×$75 + 100×$1.50 + 50×$18.75 per MTok.
        cost_usd_micros: Some(1883),
        cost_provenance: CostProvenance::Estimate,
    };
    assert_eq!((m1.usage.as_ref(), m1.usage_final), (Some(&usage), true));

    let bash = span("tool:toolu_1");
    assert_eq!(
        (bash.name.as_str(), bash.tool_name.as_deref(), bash.status),
        ("Bash", Some("Bash"), SpanStatus::Failed)
    );
    assert_eq!(
        (bash.started_at_utc_ns, bash.ended_at_utc_ns),
        (T0 + 2500 * MS, Some(T0 + 4000 * MS))
    );
    assert_eq!(
        bash.error.as_deref(),
        Some("tool reported an error (output not captured)")
    );

    let sub = span("subagent:agent1");
    assert_eq!(sub.parent_span_id, Some("tool:toolu_2".into()));
    assert_eq!(
        (sub.started_at_utc_ns, sub.ended_at_utc_ns, sub.status),
        (
            T0 + 6100 * MS,
            Some(T0 + 19_000 * MS),
            SpanStatus::Succeeded
        )
    );
    assert_eq!(
        span("llm:m5").parent_span_id,
        Some("subagent:agent1".into())
    );
    assert_eq!(
        span("tool:toolu_3").parent_span_id,
        Some("subagent:agent1".into())
    );
    assert_eq!(span("tool:toolu_2").status, SpanStatus::Succeeded);

    let (t1, t2) = (span("turn:p1"), span("turn:p2"));
    assert_eq!(
        (t1.name.as_str(), t1.started_at_utc_ns, t1.ended_at_utc_ns),
        ("turn 1", T0, Some(T0 + 22_100 * MS))
    );
    assert_eq!(
        (
            t2.name.as_str(),
            t2.status,
            span("llm:m4").parent_span_id.clone()
        ),
        ("turn 2", SpanStatus::Running, Some("turn:p2".into()))
    );
    assert_eq!(span("agent:sess-1").status, SpanStatus::Running);

    for e in store.session_events(&"sess-1".into()).unwrap() {
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("SECRET"), "payload text leaked: {json}");
    }
    assert_eq!(
        import_fixture(&mut store),
        AppendOutcome {
            inserted: 0,
            duplicates: 35
        }
    );
}

#[test]
fn tailing_resumes_after_restart_without_duplicates() {
    let env = env();
    let projects = env.dir.join("projects");
    std::fs::create_dir_all(projects.join("sess-1/subagents")).unwrap();
    std::fs::copy(
        fixture().join("sess-1/subagents/agent-agent1.jsonl"),
        projects.join("sess-1/subagents/agent-agent1.jsonl"),
    )
    .unwrap();
    let main = std::fs::read_to_string(fixture().join("sess-1.jsonl")).unwrap();
    let lines: Vec<_> = main.lines().collect();
    let path = projects.join("sess-1.jsonl");
    // Six full lines and a seventh still being written.
    std::fs::write(&path, format!("{}\n{}", lines[..6].join("\n"), lines[6])).unwrap();

    let tail = [("claude-code", projects.as_path())];
    let has = |store: &Store, id: &str| {
        store
            .load_session(&"sess-1".into())
            .unwrap()
            .spans
            .contains_key(&SpanId::from(id))
    };
    let c = start(config(&env, &tail, YEAR));
    let store = Store::open(&env.db).unwrap();
    wait_for("first lines tailed", || {
        has(&store, "tool:toolu_1") && has(&store, "llm:m6")
    });
    std::thread::sleep(Duration::from_millis(1200));
    assert!(!has(&store, "llm:m2"), "a partial line is not consumed");
    c.stop();

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("\n{}\n", lines[7..].join("\n")).as_bytes())
        .unwrap();
    let c = start(config(&env, &tail, YEAR));
    wait_for("rest tailed", || has(&store, "llm:m4"));
    c.stop();

    let mut imported = Store::open_in_memory().unwrap();
    import_fixture(&mut imported);
    let expected = imported.load_session(&"sess-1".into()).unwrap();
    assert_eq!(
        store.load_session(&"sess-1".into()).unwrap().spans,
        expected.spans,
        "tailed in two runs equals one import"
    );
    assert_eq!(journal_len(&store), 35);

    let offsets = env.dir.join("trace.db.tail-offsets.json");
    std::fs::remove_file(&offsets).unwrap();
    let c = start(config(&env, &tail, YEAR));
    wait_for("both files re-read", || {
        std::fs::read_to_string(&offsets)
            .is_ok_and(|s| s.contains("agent-agent1") && s.contains("sess-1.jsonl"))
    });
    c.stop();
    assert_eq!(journal_len(&store), 35, "re-reading without offsets dedups");
}

/// `TW_REAL_TRANSCRIPT=<path> cargo test -p trace-ingest -- --ignored real_transcript --nocapture`
#[test]
#[ignore]
fn real_transcript() {
    let path = PathBuf::from(std::env::var("TW_REAL_TRANSCRIPT").expect("set TW_REAL_TRANSCRIPT"));
    let mut store = Store::open_in_memory().unwrap();
    let mut files = vec![path.clone()];
    if let Ok(dir) = std::fs::read_dir(path.with_extension("").join("subagents")) {
        files.extend(
            dir.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl")),
        );
    }
    for f in &files {
        println!(
            "{}: {:?}",
            f.display(),
            import_file(&mut store, f, "claude-code", &Capture::default()).unwrap()
        );
    }
    for s in store.list_sessions(&SessionFilter::default()).unwrap() {
        let state = store.load_session(&s.session_id).unwrap();
        let mut kinds = std::collections::BTreeMap::new();
        for sp in state.spans.values() {
            *kinds
                .entry(format!("{:?}/{:?}", sp.kind, sp.status))
                .or_insert(0) += 1;
        }
        println!(
            "{} {:?} spans={} unresolved={} totals={:?}\n{kinds:#?}",
            s.session_id,
            s.status,
            s.span_count,
            state.unresolved().len(),
            s.totals
        );
    }
}

#[test]
fn doctor_reports_live_collector_and_adapter_blind_spots() {
    let env = env();
    let c = start(config(&env, &[], YEAR));
    let checks = trace_ingest::doctor(&env.sock, &env.db);
    c.stop();
    let summary: Vec<_> = checks.iter().map(|c| (c.ok, c.name.as_str())).collect();
    assert_eq!(
        summary,
        [
            (true, "storage"),
            (true, "database permissions"),
            (true, "socket"),
            (true, "collector"),
            (true, "adapter canonical"),
            (true, "adapter claude-code"),
            (true, "adapter codex"),
            (true, "adapter pi"),
            (true, "adapter omp"),
            (true, "adapter opencode")
        ]
    );
    assert_eq!(checks[3].detail, "answers hello with welcome (protocol 1)");
    assert!(
        checks[5]
            .detail
            .starts_with("cannot observe: LLM call start time"),
        "{}",
        checks[5].detail
    );
    let down = trace_ingest::doctor(&env.sock, &env.db);
    assert_eq!(
        (down[2].ok, down[3].ok),
        (false, false),
        "socket removed after shutdown"
    );
}

#[test]
fn collector_strips_previews_unless_project_opted_in() {
    let env = env();
    let mut cfg = config(&env, &[], YEAR);
    cfg.capture.projects = vec!["opted-in".into()];
    let c = start(cfg);
    let session = |sid: &str, project: &str| -> Vec<CanonicalEvent> {
        let mut seq = 0;
        let mut ev = |span: Option<&str>, body: Value| {
            seq += 1;
            let mut v = json!({"source_id": format!("src-{sid}"), "source_seq": seq, "event_id": format!("{sid}-{seq}"),
                "observed_at_utc_ns": seq, "occurred_at_utc_ns": seq, "session_id": sid, "span_id": span});
            v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
            serde_json::from_value::<CanonicalEvent>(v).unwrap()
        };
        vec![
            ev(None, json!({"event_type": "session.started", "body": {"project_id": project}})),
            ev(Some("x"), json!({"event_type": "span.started", "body": {"name": "x", "kind": "tool", "input_preview": "IN"}})),
            ev(Some("x"), json!({"event_type": "span.ended", "body": {"status": "succeeded", "output_preview": "OUT"}})),
        ]
    };
    for (sid, project) in [("a", "opted-in"), ("b", "other")] {
        let events = session(sid, project);
        let mut client = Client::new(&env.sock, events[0].source_id.clone(), 100);
        client.send(&events).unwrap();
        assert_eq!(client.flush(Duration::from_secs(5)).unwrap(), Some(3));
    }
    c.stop();
    let store = Store::open(&env.db).unwrap();
    let previews = |sid: &str| {
        let x = &store.load_session(&sid.into()).unwrap().spans[&SpanId::from("x")];
        (x.input_preview.clone(), x.output_preview.clone())
    };
    assert_eq!(previews("a"), (Some("IN".into()), Some("OUT".into())));
    assert_eq!(previews("b"), (None, None));
}

#[test]
fn collector_rejects_frame_with_out_of_range_timestamp() {
    let env = env();
    let c = start(config(&env, &[], YEAR));
    let event = json!({"source_id": "s", "source_seq": 1, "event_id": "e1", "observed_at_utc_ns": -1,
        "occurred_at_utc_ns": 1, "session_id": "sess", "event_type": "heartbeat", "body": {}});
    let frame = format!("{}\n", json!({"type": "events", "events": [event]}));
    assert_eq!(
        raw(&env.sock, &[hello("s"), frame]),
        vec![
            json!({"type": "welcome", "protocol": 1, "cursor": null}),
            json!({"type": "error", "message": "event e1: observed_at_utc_ns -1 is outside 1970-01-01..2200-01-01"})
        ]
    );
    c.stop();
    assert_eq!(journal_len(&Store::open(&env.db).unwrap()), 0);
}

#[test]
fn import_applies_capture_policy() {
    let env = env();
    let file = env.dir.join("previews.jsonl");
    let lines = [
        json!({"source_id": "s", "source_seq": 1, "event_id": "e1", "observed_at_utc_ns": 1, "occurred_at_utc_ns": 1,
            "session_id": "sess", "event_type": "session.started", "body": {"project_id": "p"}}),
        json!({"source_id": "s", "source_seq": 2, "event_id": "e2", "observed_at_utc_ns": 2, "occurred_at_utc_ns": 2,
            "session_id": "sess", "span_id": "x", "event_type": "span.started", "body": {"name": "x", "kind": "tool", "input_preview": "IN"}}),
        json!({"source_id": "s", "source_seq": 3, "event_id": "e3", "observed_at_utc_ns": 3, "occurred_at_utc_ns": 3,
            "session_id": "sess", "span_id": "x", "event_type": "span.ended", "body": {"status": "succeeded", "output_preview": "OUT"}}),
    ];
    std::fs::write(&file, lines.map(|l| format!("{l}\n")).concat()).unwrap();
    let previews = |capture: Capture| {
        let mut store = Store::open_in_memory().unwrap();
        import_file(&mut store, &file, "canonical", &capture).unwrap();
        let x = &store.load_session(&"sess".into()).unwrap().spans[&SpanId::from("x")];
        (x.input_preview.clone(), x.output_preview.clone())
    };
    assert_eq!(previews(Capture::default()), (None, None));
    assert_eq!(previews(Capture { projects: vec!["p".into()], ..Capture::default() }), (Some("IN".into()), Some("OUT".into())));
}

#[test]
fn reserved_collector_source_is_refused() {
    let env = env();
    let c = start(config(&env, &[], YEAR));
    let reply = raw(&env.sock, &[hello("collector")]);
    c.stop();
    assert_eq!(reply, vec![json!({"type": "error", "message": "source_id \"collector\" is reserved for the collector"})]);
}

#[test]
fn exported_journal_with_interruption_reimports() {
    let env = env();
    let mut store = Store::open(&env.db).unwrap();
    let mut events = read_events("tests/fixtures/interrupted.jsonl");
    store.append(&events).unwrap();
    let last = events.iter().map(|e| e.occurred_at_utc_ns).max().unwrap();
    assert_eq!(store.reconcile_interrupted(last + 1, last + 2).unwrap().len(), 1);
    let sid = events[0].session_id.clone();
    events = store.session_events(&sid).unwrap();
    let file = env.dir.join("export.jsonl");
    trace_adapter_sdk::write_jsonl(std::fs::File::create(&file).unwrap(), &events).unwrap();

    let mut fresh = Store::open(&env.dir.join("fresh.db")).unwrap();
    let out = import_file(&mut fresh, &file, "canonical", &Default::default()).unwrap();
    assert_eq!((out.inserted, out.duplicates), (events.len(), 0));
    assert_eq!(fresh.load_session(&sid).unwrap().session.status, SessionStatus::Interrupted);
}

fn harness_fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel)
}

/// `2026-10-23T12:mm:ss.fffZ` in nanoseconds.
fn at(min: i64, secs: f64) -> Ns {
    1_792_756_800_000_000_000 + min * 60_000 * MS + (secs * 1000.).round() as i64 * MS
}

fn span_of<'a>(s: &'a TraceState, id: &str) -> &'a Span {
    s.spans.get(&SpanId::from(id)).unwrap_or_else(|| panic!("no span {id}: {:?}", s.spans.keys().collect::<Vec<_>>()))
}

fn sorted_kinds(s: &TraceState) -> Vec<String> {
    let mut k: Vec<_> = s.spans.values().map(|sp| format!("{:?}", sp.kind)).collect();
    k.sort();
    k
}

#[test]
fn codex_rollout_becomes_turns_responses_and_nested_tools() {
    let mut store = Store::open_in_memory().unwrap();
    let path = harness_fixture("codex/rollout.jsonl");
    let first = import_file(&mut store, &path, "codex", &Capture::default()).unwrap();
    assert_eq!(import_file(&mut store, &path, "codex", &Capture::default()).unwrap(), AppendOutcome { inserted: 0, duplicates: first.inserted }, "re-reading yields identical events");
    let s = store.load_session(&"01a168be-ebc0-7652-9efb-c75c48a4b669".into()).unwrap();
    assert_eq!((s.session.project_id.as_str(), s.session.working_directory.as_str(), s.session.harness.as_str()), ("demo-proj", "/work/demo-proj", "codex"));
    assert_eq!(sorted_kinds(&s), ["Agent", "Llm", "Llm", "Llm", "Tool", "Tool", "Tool", "Tool", "Tool", "Turn", "Turn"]);

    let turn1 = "turn:01a168be-f574-7ff2-855f-b39283e2710b";
    let llm1 = span_of(&s, "llm:resp_071fde928b486bfb016adb53b5917c81919b02c68b002cddc2");
    assert_eq!((llm1.started_at_utc_ns, llm1.ended_at_utc_ns), (at(31, 47.598), Some(at(31, 50.990))), "a response starts with its turn");
    assert_eq!((llm1.parent_span_id.as_ref().unwrap().0.as_str(), llm1.model.as_deref(), llm1.provider.as_deref()), (turn1, Some("gpt-6-luna"), Some("openai")));
    let llm2 = span_of(&s, "llm:resp_071fde928b486bfb016adb53be6c00819185670a8974caade3");
    assert_eq!(llm2.started_at_utc_ns, at(31, 58.199), "the next response starts at the tool output");
    let exec = span_of(&s, "item:exec-1d0c3839-d0f8-440d-8d15-ff954ff615ab");
    assert_eq!((exec.name.as_str(), exec.parent_span_id.as_ref().unwrap().0.as_str()), ("shell", "tool:call_f767f4e277004dea82d0180a0146700a"));
    assert_eq!((exec.started_at_utc_ns, exec.ended_at_utc_ns), (at(31, 55.209), Some(at(31, 58.187))), "commands keep Codex's own start and end");
    assert_eq!(span_of(&s, turn1).status, SpanStatus::Succeeded);

    let llm3 = span_of(&s, "llm:resp-3");
    let usage = llm3.usage.as_ref().unwrap();
    assert_eq!((usage.tokens_input, usage.cache_read, usage.tokens_output), (Some(200), Some(800), Some(20)), "cached tokens split out of OpenAI's input count");
    assert_eq!(span_of(&s, "item:mcp-1").name, "mcp__github__create_issue");
    assert_eq!(span_of(&s, "item:mcp-1").parent_span_id.as_ref().unwrap().0, "tool:call-2");
    assert_eq!(span_of(&s, "item:exec-2").status, SpanStatus::Failed);
    assert_eq!(span_of(&s, "turn:turn-2").status, SpanStatus::Cancelled);
}

#[test]
fn pi_session_becomes_turns_timed_calls_and_tools() {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &harness_fixture("pi/session.jsonl"), "pi", &Capture::default()).unwrap();
    let s = store.load_session(&"pi-sess-1".into()).unwrap();
    assert_eq!((s.session.project_id.as_str(), s.session.harness.as_str()), ("demo-proj", "pi"));
    assert_eq!(sorted_kinds(&s), ["Agent", "Llm", "Llm", "Llm", "Llm", "Llm", "Llm", "Llm", "Subagent", "Tool", "Tool", "Tool", "Turn", "Turn", "Turn"]);

    let a1 = span_of(&s, "llm:a1");
    assert_eq!((a1.started_at_utc_ns, a1.ended_at_utc_ns), (at(0, 1.1), Some(at(0, 3.4))), "pi records the request start and its duration");
    let u = a1.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.tokens_output, u.cache_read, u.cache_write, u.cost_usd_micros, u.cost_provenance), (Some(10), Some(40), Some(900), Some(100), Some(4175), CostProvenance::Estimate));
    let tool = span_of(&s, "tool:tc1");
    assert_eq!((tool.name.as_str(), tool.started_at_utc_ns, tool.ended_at_utc_ns, tool.status), ("read", at(0, 3.4), Some(at(0, 3.9)), SpanStatus::Succeeded), "it ends by its own 500ms, not when the result was written");
    assert_eq!(tool.parent_span_id.as_ref().unwrap().0, "turn:u1");
    let t1 = span_of(&s, "turn:u1");
    assert_eq!((t1.ended_at_utc_ns, t1.status), (Some(at(0, 5.9)), SpanStatus::Succeeded), "the turn ends with the call that stops without a tool");
    let a3 = span_of(&s, "llm:a3");
    assert_eq!((a3.status, a3.error.as_deref()), (SpanStatus::Failed, Some("rate limited")));
    assert_eq!(span_of(&s, "turn:u2").status, SpanStatus::Failed);
    assert_eq!((span_of(&s, "turn:u2").name.as_str(), span_of(&s, "turn:u3").name.as_str()), ("turn 2", "turn 3 (branch)"), "going back to an earlier message starts a branch");

    let sub = span_of(&s, "subagent:tc2:0");
    assert_eq!((sub.name.as_str(), sub.kind, sub.parent_span_id.as_ref().unwrap().0.as_str()), ("scout", SpanKind::Subagent, "tool:tc2"));
    assert_eq!((sub.started_at_utc_ns, sub.ended_at_utc_ns, sub.status), (at(0, 21.0), Some(at(0, 24.0)), SpanStatus::Succeeded), "from its first message to its last call's end");
    let grep = span_of(&s, "tool:tc2:0:s1");
    assert_eq!((grep.name.as_str(), grep.started_at_utc_ns, grep.ended_at_utc_ns), ("grep", at(0, 22.0), Some(at(0, 22.5))));
    assert_eq!(grep.parent_span_id.as_ref().unwrap().0, "subagent:tc2:0");
    let call = span_of(&s, "llm:tc2:0:3");
    assert_eq!((call.model.as_deref(), call.usage.as_ref().unwrap().cost_usd_micros), (Some("claude-haiku-5-5"), Some(500)));
}

/// An OpenCode 2.x database with the two tables the adapter reads, holding the rows OpenCode wrote for a
/// run that hit its free-usage limit.
fn opencode_db(path: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).unwrap();
    c.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, version TEXT NOT NULL, parent_id TEXT, agent TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         INSERT INTO session_v2 VALUES ('ses_fail', '/work/demo-proj', '2.1.3', NULL, NULL, 1792758724921, 1792758725769);
         INSERT INTO session_message VALUES ('msg_u', 'ses_fail', 'user', 4, 1792758725142, 1792758725142, '{\"time\":{\"created\":1792758725142},\"text\":\"(text)\",\"files\":[]}');
         INSERT INTO session_message VALUES ('msg_a', 'ses_fail', 'assistant', 5, 1792758725157, 1792758725769, '{\"time\":{\"created\":1792758725157,\"completed\":1792758725769},\"agent\":\"build\",\"model\":{\"id\":\"big-pickle\",\"providerID\":\"opencode\"},\"content\":[],\"finish\":\"error\",\"error\":{\"type\":\"provider.free-limit\",\"message\":\"Free usage limit reached.\",\"status\":429}}');
         INSERT INTO session_message VALUES ('msg_i', 'ses_fail', 'idle', 7, 1792758725771, 1792758725771, '{\"time\":{\"created\":1792758725771},\"outcome\":\"failed\",\"error\":{\"type\":\"provider.free-limit\",\"message\":\"Free usage limit reached.\"}}');",
    )
    .unwrap();
    c
}

/// A run that reads a file: the assistant step is still streaming when written, and completes later.
fn opencode_run(c: &rusqlite::Connection, done: bool) {
    let tool = if done {
        r#"{"type":"tool","id":"call_1","name":"read","state":{"status":"completed","input":{},"content":[]},"time":{"created":1000,"ran":1200,"completed":1500}}"#
    } else {
        r#"{"type":"tool","id":"call_1","name":"read","state":{"status":"running","input":{},"metadata":{}},"time":{"created":1000,"ran":1200}}"#
    };
    let time = if done { r#"{"created":500,"streamed":1000,"completed":1600}"# } else { r#"{"created":500}"# };
    let assistant = format!(r#"{{"time":{time},"agent":"build","model":{{"id":"claude-sonnet-5-5","providerID":"anthropic"}},"content":[{tool}],"cost":0.0042,"tokens":{{"input":12,"output":30,"reasoning":0,"cache":{{"read":2000,"write":100}}}}}}"#);
    let base = 1_792_760_000_000i64;
    // Offsets above are milliseconds after `base`.
    let assistant = ["500", "1000", "1200", "1500", "1600"].iter().fold(assistant, |a, n| a.replace(&format!(":{n}"), &format!(":{}", base + n.parse::<i64>().unwrap())));
    let updated = base + if done { 1700 } else { 1300 };
    c.execute("INSERT OR REPLACE INTO session_v2 VALUES ('ses_ok', '/work/demo-proj', '2.1.3', NULL, 'build', ?1, ?2)", rusqlite::params![base, updated]).unwrap();
    c.execute("INSERT OR REPLACE INTO session_message VALUES ('msg_u2', 'ses_ok', 'user', 1, ?1, ?1, '{}')", [base + 100]).unwrap();
    c.execute("INSERT OR REPLACE INTO session_message VALUES ('msg_a2', 'ses_ok', 'assistant', 2, ?1, ?2, ?3)", rusqlite::params![base + 500, updated, assistant]).unwrap();
    if done {
        c.execute("INSERT OR REPLACE INTO session_message VALUES ('msg_i2', 'ses_ok', 'idle', 3, ?1, ?1, '{\"outcome\":\"succeeded\"}')", [base + 1700]).unwrap();
    }
}

#[test]
fn opencode_database_imports_failed_and_finished_runs() {
    let env = env();
    let db = env.dir.join("opencode.db");
    let c = opencode_db(&db);
    opencode_run(&c, true);
    let mut store = Store::open_in_memory().unwrap();
    let first = import_file(&mut store, &db, "opencode", &Capture::default()).unwrap();
    assert_eq!(import_file(&mut store, &db, "opencode", &Capture::default()).unwrap().inserted, 0, "{first:?}");

    let s = store.load_session(&"ses_fail".into()).unwrap();
    assert_eq!((s.session.project_id.as_str(), s.session.harness.as_str()), ("demo-proj", "opencode"));
    let llm = span_of(&s, "llm:msg_a");
    assert_eq!((llm.status, llm.model.as_deref(), llm.error.as_deref()), (SpanStatus::Failed, Some("big-pickle"), Some("Free usage limit reached.")));
    assert_eq!(span_of(&s, "turn:msg_u").status, SpanStatus::Failed, "the idle message's outcome ends the turn");

    let s = store.load_session(&"ses_ok".into()).unwrap();
    let base = 1_792_760_000_000 * MS;
    let llm = span_of(&s, "llm:msg_a2");
    assert_eq!((llm.started_at_utc_ns, llm.ended_at_utc_ns), (base + 500 * MS, Some(base + 1000 * MS)), "the call ends when its stream does");
    let u = llm.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.cache_read, u.cost_usd_micros, u.cost_provenance), (Some(12), Some(2000), Some(4200), CostProvenance::Estimate));
    let tool = span_of(&s, "tool:call_1");
    assert_eq!((tool.name.as_str(), tool.started_at_utc_ns, tool.ended_at_utc_ns), ("read", base + 1200 * MS, Some(base + 1500 * MS)));
    assert_eq!(span_of(&s, "turn:msg_u2").status, SpanStatus::Succeeded);
}

#[test]
fn collector_tails_an_opencode_database_as_messages_complete() {
    let env = env();
    let db = env.dir.join("opencode.db");
    let c = opencode_db(&db);
    opencode_run(&c, false);
    let running = start(config(&env, &[("opencode", db.as_path())], YEAR));
    let reader = Store::open(&env.db).unwrap();
    let tool = |r: &Store| r.load_session(&"ses_ok".into()).ok().and_then(|s| s.spans.get(&SpanId::from("tool:call_1")).cloned());
    wait_for("the running tool call", || tool(&reader).is_some());
    assert_eq!(tool(&reader).unwrap().ended_at_utc_ns, None, "still running");
    opencode_run(&c, true);
    wait_for("the tool call to end", || tool(&reader).is_some_and(|t| t.ended_at_utc_ns.is_some()));
    assert_eq!(reader.load_session(&"ses_fail".into()).unwrap().spans.len(), 3);
    running.stop();
}

#[test]
fn codex_sub_agent_nests_under_its_spawn_call_in_the_root_session() {
    let mut store = Store::open_in_memory().unwrap();
    // The child first: its spans wait for the parent's sub-agent span, then resolve.
    for f in ["codex/subagent-child.jsonl", "codex/subagent-parent.jsonl"] {
        import_file(&mut store, &harness_fixture(f), "codex", &Capture::default()).unwrap();
    }
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.iter().map(|s| s.session_id.0.as_str()).collect::<Vec<_>>(), ["01a168cc-d3fc-71f3-95ac-adf7636d82c0"], "the sub-agent joins the root session");
    let s = store.load_session(&sessions[0].session_id).unwrap();
    assert!(s.unresolved().is_empty());
    let sub = span_of(&s, "subagent:01a168cc-df54-7272-97f3-5ad6babb84e8");
    assert_eq!((sub.name.as_str(), sub.kind, sub.status), ("readme_sentence", SpanKind::Subagent, SpanStatus::Succeeded));
    assert_eq!(sub.parent_span_id.as_ref().unwrap().0, "tool:call_53aa098bd4974b90969deff281c602d7");
    assert_eq!(span_of(&s, "tool:call_53aa098bd4974b90969deff281c602d7").name, "spawn_agent");
    let child_turn = span_of(&s, "turn:01a168cc-df7c-7443-9141-6a06eda97364");
    assert_eq!(child_turn.parent_span_id.as_ref().unwrap().0, "subagent:01a168cc-df54-7272-97f3-5ad6babb84e8");
    let turns = s.spans.values().filter(|sp| sp.kind == SpanKind::Turn).count();
    assert_eq!(turns, 2, "the parent history copied into the child rollout is skipped");
    assert_eq!(s.spans.values().filter(|sp| sp.kind == SpanKind::Llm).count(), 5);
    assert_eq!(sessions[0].turn_count, 1, "the sidebar counts the main agent's turns, not the sub-agent's");
}

#[test]
fn codex_sub_agent_closes_with_its_own_turn_when_the_parent_never_reports_it() {
    let env = env();
    let parent = std::fs::read_to_string(harness_fixture("codex/subagent-parent.jsonl")).unwrap();
    let without: Vec<&str> = parent.lines().filter(|l| !l.contains(r#""kind":"completed""#)).collect();
    let trimmed = env.dir.join("parent.jsonl");
    std::fs::write(&trimmed, without.join("\n") + "\n").unwrap();
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &trimmed, "codex", &Capture::default()).unwrap();
    import_file(&mut store, &harness_fixture("codex/subagent-child.jsonl"), "codex", &Capture::default()).unwrap();
    let s = store.load_session(&"01a168cc-d3fc-71f3-95ac-adf7636d82c0".into()).unwrap();
    let sub = span_of(&s, "subagent:01a168cc-df54-7272-97f3-5ad6babb84e8");
    assert_eq!((sub.ended_at_utc_ns, sub.status), (Some(at(47, 3.207)), SpanStatus::Succeeded));
}

#[test]
fn opencode_sub_agent_session_nests_under_its_task_call() {
    let env = env();
    let db = env.dir.join("opencode.db");
    let c = opencode_db(&db);
    let b = 1_792_770_000_000i64;
    let task = format!(r#"{{"time":{{"created":{},"streamed":{},"completed":{}}},"model":{{"id":"m","providerID":"p"}},"content":[{{"type":"tool","id":"call_task","name":"subagent","state":{{"status":"completed","input":{{}},"content":[],"metadata":{{"sessionID":"ses_child","status":"completed"}}}},"time":{{"created":{},"ran":{},"completed":{}}}}}]}}"#, b + 100, b + 200, b + 5000, b + 200, b + 300, b + 4900);
    let child = format!(r#"{{"time":{{"created":{},"streamed":{},"completed":{}}},"model":{{"id":"m","providerID":"p"}},"content":[]}}"#, b + 1000, b + 3000, b + 3000);
    c.execute_batch(&format!(
        "INSERT INTO session_v2 VALUES ('ses_parent', '/work/demo-proj', '2.1.3', NULL, 'build', {b}, {u});
         INSERT INTO session_v2 VALUES ('ses_child', '/work/demo-proj', '2.1.3', 'ses_parent', 'explore', {c0}, {u});
         INSERT INTO session_message VALUES ('p_u', 'ses_parent', 'user', 1, {b}, {b}, '{{}}');
         INSERT INTO session_message VALUES ('p_a', 'ses_parent', 'assistant', 2, {a}, {u}, '{task}');
         INSERT INTO session_message VALUES ('c_u', 'ses_child', 'user', 1, {c0}, {c0}, '{{}}');
         INSERT INTO session_message VALUES ('c_a', 'ses_child', 'assistant', 2, {c1}, {c2}, '{child}');
         INSERT INTO session_message VALUES ('c_i', 'ses_child', 'idle', 3, {c3}, {c3}, '{{\"outcome\":\"succeeded\"}}');",
        u = b + 6000, a = b + 100, c0 = b + 400, c1 = b + 1000, c2 = b + 3000, c3 = b + 3100,
    ))
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &db, "opencode", &Capture::default()).unwrap();
    assert!(store.load_session(&"ses_child".into()).unwrap().spans.is_empty(), "the child's events join its root session");
    let s = store.load_session(&"ses_parent".into()).unwrap();
    let sub = span_of(&s, "subagent:ses_child");
    assert_eq!((sub.name.as_str(), sub.kind, sub.parent_span_id.as_ref().unwrap().0.as_str()), ("explore", SpanKind::Subagent, "tool:call_task"));
    assert_eq!((sub.started_at_utc_ns, sub.ended_at_utc_ns, sub.status), ((b + 400) * MS, Some((b + 3100) * MS), SpanStatus::Succeeded));
    assert_eq!(span_of(&s, "turn:c_u").parent_span_id.as_ref().unwrap().0, "subagent:ses_child");
    assert_eq!(span_of(&s, "llm:c_a").parent_span_id.as_ref().unwrap().0, "turn:c_u");
}

#[test]
fn opencode_before_2_reads_steps_tools_and_sub_agents() {
    let env = env();
    let db = env.dir.join("opencode.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    let b = 1_792_780_000_000i64;
    let part = |id: &str, msg: &str, sess: &str, at: i64, data: String| format!("INSERT INTO part VALUES ('{id}', '{msg}', '{sess}', {at}, {at}, '{data}');");
    let tool = |call: &str, name: &str, status: &str, s: i64, e: i64, meta: &str| format!(r#"{{"type":"tool","callID":"{call}","tool":"{name}","state":{{"status":"{status}","input":{{}},"output":"","title":"","metadata":{{{meta}}},"time":{{"start":{s},"end":{e}}}}}}}"#);
    let finish = |cost: f64| format!(r#"{{"type":"step-finish","reason":"stop","cost":{cost},"tokens":{{"input":5,"output":7,"reasoning":0,"cache":{{"read":100,"write":0}}}}}}"#);
    let assistant = |done: i64, finish: &str| format!(r#"{{"role":"assistant","time":{{"created":{b},"completed":{done}}},"modelID":"claude-sonnet-5-5","providerID":"anthropic","agent":"build","cost":0,"tokens":{{"input":0,"output":0,"reasoning":0,"cache":{{"read":0,"write":0}}}},"finish":"{finish}"}}"#);
    let mut sql = String::from(
        "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, version TEXT NOT NULL, parent_id TEXT, agent TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    );
    sql += &format!("INSERT INTO session VALUES ('ses_old', '/work/demo-proj', '1.4.0', NULL, NULL, {b}, {});", b + 9000);
    sql += &format!("INSERT INTO session VALUES ('ses_old_child', '/work/demo-proj', '1.4.0', 'ses_old', 'general', {}, {});", b + 1500, b + 9000);
    sql += &format!("INSERT INTO message VALUES ('msg_1u', 'ses_old', {b}, {b}, '{{\"role\":\"user\",\"time\":{{\"created\":{b}}}}}');");
    sql += &format!("INSERT INTO message VALUES ('msg_1a', 'ses_old', {}, {}, '{}');", b + 100, b + 6000, assistant(b + 6000, "stop"));
    sql += &part("prt_01", "msg_1a", "ses_old", b + 100, r#"{"type":"step-start"}"#.into());
    sql += &part("prt_02", "msg_1a", "ses_old", b + 1000, tool("call_task", "task", "completed", b + 1000, b + 4000, r#""sessionId":"ses_old_child""#));
    sql += &part("prt_03", "msg_1a", "ses_old", b + 1000, finish(0.002));
    sql += &part("prt_04", "msg_1a", "ses_old", b + 4100, r#"{"type":"step-start"}"#.into());
    sql += &part("prt_05", "msg_1a", "ses_old", b + 6000, finish(0.001));
    sql += &format!("INSERT INTO message VALUES ('msg_2u', 'ses_old_child', {0}, {0}, '{{\"role\":\"user\",\"time\":{{\"created\":{0}}}}}');", b + 1500);
    sql += &format!("INSERT INTO message VALUES ('msg_2a', 'ses_old_child', {}, {}, '{}');", b + 1600, b + 3900, assistant(b + 3900, "stop"));
    c.execute_batch(&sql).unwrap();

    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &db, "opencode", &Capture::default()).unwrap();
    let s = store.load_session(&"ses_old".into()).unwrap();
    let step1 = span_of(&s, "llm:prt_01");
    assert_eq!((step1.started_at_utc_ns, step1.ended_at_utc_ns), ((b + 100) * MS, Some((b + 1000) * MS)), "each step is a model call");
    let u = step1.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.cache_read, u.cost_usd_micros), (Some(5), Some(100), Some(2000)));
    assert_eq!(span_of(&s, "llm:prt_04").ended_at_utc_ns, Some((b + 6000) * MS));
    let task = span_of(&s, "tool:call_task");
    assert_eq!((task.name.as_str(), task.started_at_utc_ns, task.ended_at_utc_ns), ("task", (b + 1000) * MS, Some((b + 4000) * MS)));
    assert_eq!(span_of(&s, "turn:msg_1u").ended_at_utc_ns, Some((b + 6000) * MS), "the reply that finishes without a tool call ends the turn");
    let sub = span_of(&s, "subagent:ses_old_child");
    assert_eq!((sub.name.as_str(), sub.parent_span_id.as_ref().unwrap().0.as_str(), sub.ended_at_utc_ns), ("general", "tool:call_task", Some((b + 3900) * MS)));
    assert_eq!(span_of(&s, "llm:msg_2a").parent_span_id.as_ref().unwrap().0, "turn:msg_2u");
}

/// A pi session file: the header, then entries at `(seconds after 12:00, json)`.
fn pi_file(path: &Path, header: Value, entries: &[(f64, Value)]) {
    let ms0 = 1_792_756_800_000i64;
    let mut out = vec![header.to_string()];
    for (i, (secs, msg)) in entries.iter().enumerate() {
        let mut m = msg.clone();
        let at = ms0 + (secs * 1000.).round() as i64;
        m["timestamp"] = json!(at);
        out.push(json!({"type": "message", "id": format!("{}-{i}", path.file_stem().unwrap().to_str().unwrap()), "parentId": null, "timestamp": format!("2026-10-23T12:00:{secs:06.3}Z"), "message": m}).to_string());
    }
    std::fs::write(path, out.join("\n") + "\n").unwrap();
}

#[test]
fn pi_session_started_during_a_tool_call_is_its_sub_agent_and_a_fork_is_not() {
    let env = env();
    let (parent, child, fork) = (env.dir.join("parent.jsonl"), env.dir.join("child.jsonl"), env.dir.join("fork.jsonl"));
    let user = json!({"role": "user", "content": []});
    let call = |id: &str| json!({"role": "assistant", "content": [{"type": "toolCall", "id": id, "name": "delegate", "arguments": {}}], "model": "m", "provider": "p", "stopReason": "toolUse", "durationMs": 1000});
    let reply = json!({"role": "assistant", "content": [], "model": "m", "provider": "p", "stopReason": "stop", "durationMs": 2000});
    pi_file(&parent, json!({"type": "session", "version": 3, "id": "pi-parent", "timestamp": "2026-10-23T12:00:00.000Z", "cwd": "/work/demo-proj"}),
        &[(0.5, user.clone()), (1.0, call("tc_d")), (9.0, json!({"role": "toolResult", "toolCallId": "tc_d", "toolName": "delegate", "content": [], "isError": false})), (9.5, reply.clone())]);
    let header = |id: &str, at: &str| json!({"type": "session", "version": 3, "id": id, "timestamp": at, "cwd": "/work/demo-proj", "parentSession": parent.to_str().unwrap()});
    // Both start with a copy of the parent's first prompt.
    pi_file(&child, header("pi-child", "2026-10-23T12:00:03.000Z"), &[(0.5, user.clone()), (3.5, user.clone()), (4.0, reply.clone())]);
    pi_file(&fork, header("pi-fork", "2026-10-23T12:00:20.000Z"), &[(0.5, user.clone()), (20.5, user.clone()), (21.0, reply.clone())]);

    let mut store = Store::open_in_memory().unwrap();
    for f in [&child, &parent, &fork] {
        import_file(&mut store, f, "pi", &Capture::default()).unwrap();
    }
    let mut ids: Vec<_> = store.list_sessions(&SessionFilter::default()).unwrap().into_iter().map(|s| s.session_id.0).collect();
    ids.sort();
    assert_eq!(ids, ["pi-fork", "pi-parent"], "the sub-agent joins its parent's session; the fork keeps its own");

    let s = store.load_session(&"pi-parent".into()).unwrap();
    assert!(s.unresolved().is_empty());
    let sub = span_of(&s, "subagent:pi-child");
    assert_eq!((sub.kind, sub.parent_span_id.as_ref().unwrap().0.as_str()), (SpanKind::Subagent, "tool:tc_d"));
    assert_eq!((sub.started_at_utc_ns, sub.ended_at_utc_ns), (at(0, 3.0), Some(at(0, 6.0))), "it ends with its last turn");
    let child_turns: Vec<_> = s.spans.values().filter(|sp| sp.kind == SpanKind::Turn && sp.parent_span_id.as_ref().is_some_and(|p| p.0 == "subagent:pi-child")).collect();
    assert_eq!(child_turns.len(), 1, "the copied parent history is skipped");

    let f = store.load_session(&"pi-fork".into()).unwrap();
    assert_eq!(f.spans.values().filter(|sp| sp.kind == SpanKind::Turn).count(), 1, "the fork skips its copied history too");
}

#[test]
fn opencode_1_json_storage_reads_steps_tools_and_sub_agents() {
    let env = env();
    let dir = env.dir.join("storage");
    let b = 1_792_790_000_000i64;
    let put = |rel: &str, v: Value| {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v.to_string()).unwrap();
    };
    put("session/proj/ses_a.json", json!({"id": "ses_a", "projectID": "proj", "directory": "/work/demo-proj", "title": "(text)", "version": "1.0.150", "time": {"created": b, "updated": b + 9000}}));
    put("session/proj/ses_b.json", json!({"id": "ses_b", "projectID": "proj", "parentID": "ses_a", "directory": "/work/demo-proj", "title": "(text)", "version": "1.0.150", "time": {"created": b + 2000, "updated": b + 5000}}));
    put("message/ses_a/msg_au.json", json!({"id": "msg_au", "sessionID": "ses_a", "role": "user", "time": {"created": b}}));
    put("message/ses_a/msg_aa.json", json!({"id": "msg_aa", "sessionID": "ses_a", "role": "assistant", "time": {"created": b + 100, "completed": b + 8000}, "modelID": "claude-sonnet-5-5", "providerID": "anthropic", "cost": 0.004, "tokens": {"input": 9, "output": 30, "reasoning": 0, "cache": {"read": 500, "write": 0}}, "finish": "stop"}));
    let step_finish = json!({"type": "step-finish", "reason": "stop", "cost": 0.002, "tokens": {"input": 4, "output": 10, "reasoning": 0, "cache": {"read": 250, "write": 0}}});
    put("part/msg_aa/prt_01.json", json!({"id": "prt_01", "type": "step-start"}));
    put("part/msg_aa/prt_02.json", json!({"id": "prt_02", "type": "text", "text": "(text)", "time": {"start": b + 300, "end": b + 1500}}));
    put("part/msg_aa/prt_03.json", json!({"id": "prt_03", "type": "tool", "callID": "call_task", "tool": "task", "state": {"status": "completed", "input": {}, "output": "", "title": "", "metadata": {"sessionId": "ses_b"}, "time": {"start": b + 1600, "end": b + 6000}}}));
    put("part/msg_aa/prt_04.json", { let mut f = step_finish.clone(); f["id"] = json!("prt_04"); f });
    put("part/msg_aa/prt_05.json", json!({"id": "prt_05", "type": "step-start"}));
    put("part/msg_aa/prt_06.json", json!({"id": "prt_06", "type": "text", "text": "(text)", "time": {"start": b + 6500, "end": b + 8000}}));
    put("part/msg_aa/prt_07.json", { let mut f = step_finish.clone(); f["id"] = json!("prt_07"); f });
    put("message/ses_b/msg_bu.json", json!({"id": "msg_bu", "sessionID": "ses_b", "role": "user", "time": {"created": b + 2000}}));
    put("message/ses_b/msg_ba.json", json!({"id": "msg_ba", "sessionID": "ses_b", "role": "assistant", "time": {"created": b + 2100, "completed": b + 5000}, "modelID": "claude-haiku-5-5", "providerID": "anthropic", "cost": 0.0, "tokens": {"input": 1, "output": 2, "reasoning": 0, "cache": {"read": 0, "write": 0}}, "finish": "stop"}));

    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &dir, "opencode", &Capture::default()).unwrap();
    let ids: Vec<_> = store.list_sessions(&SessionFilter::default()).unwrap().into_iter().map(|s| s.session_id.0).collect();
    assert_eq!(ids, ["ses_a"]);
    let s = store.load_session(&"ses_a".into()).unwrap();
    let step1 = span_of(&s, "llm:prt_01");
    assert_eq!((step1.started_at_utc_ns, step1.ended_at_utc_ns), ((b + 100) * MS, Some((b + 6000) * MS)), "a step runs from where the reply got to until its parts' latest end");
    assert_eq!(step1.usage.as_ref().unwrap().cost_usd_micros, Some(2000));
    let step2 = span_of(&s, "llm:prt_05");
    assert_eq!((step2.started_at_utc_ns, step2.ended_at_utc_ns), ((b + 6000) * MS, Some((b + 8000) * MS)));
    let task = span_of(&s, "tool:call_task");
    assert_eq!((task.started_at_utc_ns, task.ended_at_utc_ns), ((b + 1600) * MS, Some((b + 6000) * MS)));
    let sub = span_of(&s, "subagent:ses_b");
    assert_eq!((sub.parent_span_id.as_ref().unwrap().0.as_str(), sub.ended_at_utc_ns), ("tool:call_task", Some((b + 5000) * MS)));
    assert_eq!(span_of(&s, "llm:msg_ba").model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(span_of(&s, "turn:msg_au").ended_at_utc_ns, Some((b + 8000) * MS));
}

#[test]
fn codex_reads_compressed_rollouts_and_keeps_review_threads_apart() {
    let env = env();
    let plain = std::fs::read(harness_fixture("codex/rollout.jsonl")).unwrap();
    let packed = env.dir.join("rollout-old.jsonl.zst");
    std::fs::write(&packed, ruzstd::encoding::compress_to_vec(&plain[..], ruzstd::encoding::CompressionLevel::Fastest)).unwrap();
    let mut a = Store::open_in_memory().unwrap();
    import_file(&mut a, &harness_fixture("codex/rollout.jsonl"), "codex", &Capture::default()).unwrap();
    let mut b = Store::open_in_memory().unwrap();
    import_file(&mut b, &packed, "codex", &Capture::default()).unwrap();
    let id = SessionId::from("01a168be-ebc0-7652-9efb-c75c48a4b669");
    assert_eq!(a.load_session(&id).unwrap().spans, b.load_session(&id).unwrap().spans, "a .jsonl.zst rollout reads like its plain copy");

    // A review thread is a sub-agent with no parent: it stands as its own session.
    let review = env.dir.join("rollout-review.jsonl");
    let meta = json!({"timestamp": "2026-10-23T12:00:00.000Z", "type": "session_meta", "payload": {"id": "rev", "session_id": "other", "cwd": "/work/demo-proj", "source": {"subagent": "review"}}});
    let started = json!({"timestamp": "2026-10-23T12:00:01.000Z", "type": "event_msg", "payload": {"type": "task_started", "turn_id": "t"}});
    std::fs::write(&review, format!("{meta}\n{started}\n")).unwrap();
    let mut c = Store::open_in_memory().unwrap();
    import_file(&mut c, &review, "codex", &Capture::default()).unwrap();
    let s = c.load_session(&"rev".into()).unwrap();
    assert!(s.unresolved().is_empty() && s.spans.contains_key(&SpanId::from("agent:rev")), "{:?}", s.spans.keys().collect::<Vec<_>>());
}

#[test]
fn pi_parallel_tools_end_by_their_own_duration() {
    let env = env();
    let ms0 = 1_792_756_800_000i64;
    let lines = [
        json!({"type": "session", "version": 3, "id": "par", "timestamp": "2026-10-23T12:00:00.000Z", "cwd": "/work/demo-proj"}),
        json!({"type": "message", "id": "u", "parentId": null, "timestamp": "2026-10-23T12:00:00.000Z", "message": {"role": "user", "content": [], "timestamp": ms0}}),
        json!({"type": "message", "id": "a", "parentId": "u", "timestamp": "2026-10-23T12:00:01.000Z", "message": {"role": "assistant", "content": [{"type": "toolCall", "id": "fast", "name": "read"}, {"type": "toolCall", "id": "slow", "name": "bash"}], "model": "m", "stopReason": "toolUse", "timestamp": ms0 + 500, "durationMs": 500}}),
        // Both results land when the batch is done, at 4s; their own durations are 0.2s and 3s.
        json!({"type": "message", "id": "r1", "parentId": "a", "timestamp": "2026-10-23T12:00:04.000Z", "message": {"role": "toolResult", "toolCallId": "fast", "content": [], "isError": false, "timestamp": ms0 + 4_000, "durationMs": 200}}),
        json!({"type": "message", "id": "r2", "parentId": "r1", "timestamp": "2026-10-23T12:00:04.000Z", "message": {"role": "toolResult", "toolCallId": "slow", "content": [], "isError": false, "timestamp": ms0 + 4_000, "durationMs": 3_000}}),
    ];
    let f = env.dir.join("par.jsonl");
    std::fs::write(&f, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n").unwrap();
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &f, "pi", &Capture::default()).unwrap();
    let s = store.load_session(&"par".into()).unwrap();
    let end = |id: &str| span_of(&s, id).ended_at_utc_ns.unwrap();
    assert_eq!((end("tool:fast"), end("tool:slow")), ((ms0 + 1_200) * MS, (ms0 + 4_000) * MS), "each tool ends by its own duration, not the batch's");
}

#[test]
fn opencode_fork_skips_copied_history_and_compaction_is_a_model_call() {
    let env = env();
    let db = env.dir.join("opencode.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    let b = 1_792_800_000_000i64;
    c.execute_batch(&format!(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, version TEXT NOT NULL, parent_id TEXT, agent TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, fork_session_id TEXT, fork_boundary TEXT);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
         INSERT INTO session_v2 VALUES ('orig', '/work/demo-proj', '2.1.3', NULL, NULL, {b}, {b9}, NULL, NULL);
         INSERT INTO session_v2 VALUES ('fork', '/work/demo-proj', '2.1.3', NULL, NULL, {b5}, {b9}, 'orig', '{{\"type\":\"through\",\"messageID\":\"o2\"}}');
         INSERT INTO session_message VALUES ('o1', 'orig', 'user', 1, {b}, {b}, '{{}}');
         INSERT INTO session_message VALUES ('o2', 'orig', 'idle', 2, {b1}, {b1}, '{{\"outcome\":\"succeeded\"}}');
         INSERT INTO session_message VALUES ('f1', 'fork', 'user', 1, {b}, {b}, '{{}}');
         INSERT INTO session_message VALUES ('f2', 'fork', 'idle', 2, {b1}, {b1}, '{{\"outcome\":\"succeeded\"}}');
         INSERT INTO session_message VALUES ('f3', 'fork', 'user', 3, {b5}, {b5}, '{{}}');
         INSERT INTO session_message VALUES ('f4', 'fork', 'compaction', 4, {b6}, {b7}, '{{\"status\":\"completed\",\"reason\":\"auto\",\"summary\":\"(text)\",\"model\":{{\"id\":\"claude-sonnet-4-5\",\"providerID\":\"anthropic\"}},\"cost\":0.01,\"tokens\":{{\"input\":5000,\"output\":300,\"reasoning\":0,\"cache\":{{\"read\":0,\"write\":0}}}}}}');
         INSERT INTO session_message VALUES ('f5', 'fork', 'idle', 5, {b8}, {b8}, '{{\"outcome\":\"succeeded\"}}');",
        b1 = b + 1000, b5 = b + 5000, b6 = b + 6000, b7 = b + 7000, b8 = b + 8000, b9 = b + 9000,
    ))
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &db, "opencode", &Capture::default()).unwrap();
    let s = store.load_session(&"fork".into()).unwrap();
    let turns: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Turn).map(|x| x.span_id.0.as_str()).collect();
    assert_eq!(turns, ["turn:f3"], "the copied history is the parent's, not the fork's");
    let compaction = span_of(&s, "llm:f4");
    assert_eq!((compaction.started_at_utc_ns, compaction.ended_at_utc_ns, compaction.usage.as_ref().unwrap().cost_usd_micros), ((b + 6000) * MS, Some((b + 7000) * MS), Some(10_000)));
}
