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
        cost_usd_micros: None,
        cost_provenance: CostProvenance::Unknown,
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
            (true, "adapter claude-code")
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
