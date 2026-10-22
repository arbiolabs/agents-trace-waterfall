use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use trace_core::*;
use trace_storage::*;

fn temp_db(name: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "trace-storage-{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("nested").join("traces.db")
}

fn ev(src: &str, seq: u64, session: &str, span: Option<&str>, t: Ns, body: EventBody) -> CanonicalEvent {
    CanonicalEvent {
        schema_version: SCHEMA_VERSION,
        source_id: src.into(),
        source_seq: seq,
        event_id: EventId(format!("{src}-{seq}")),
        observed_at_utc_ns: t,
        occurred_at_utc_ns: t,
        session_id: session.into(),
        span_id: span.map(Into::into),
        body,
    }
}

fn started(project: &str, adapter: &str) -> EventBody {
    EventBody::SessionStarted {
        project_id: project.into(),
        working_directory: format!("/work/{project}"),
        source_adapter: adapter.into(),
        harness: "claude-code".into(),
        host: "devbox".into(),
        labels: Default::default(),
    }
}

fn span(parent: Option<&str>, name: &str, kind: SpanKind, model: Option<&str>) -> EventBody {
    EventBody::SpanStarted {
        parent_span_id: parent.map(Into::into),
        name: name.into(),
        kind,
        model: model.map(Into::into),
        provider: None,
        tool_name: None,
        agent_name: None,
        retry_of: None,
        input_preview: None,
        attributes: Default::default(),
    }
}

fn ended(status: SpanStatus) -> EventBody {
    EventBody::SpanEnded { status, output_preview: None, error: None }
}

fn usage(input: u64, output: u64, cost: u64, provenance: CostProvenance) -> EventBody {
    EventBody::UsageReported {
        usage: Usage {
            tokens_input: Some(input),
            tokens_output: Some(output),
            cost_usd_micros: Some(cost),
            cost_provenance: provenance,
            ..Default::default()
        },
        is_final: true,
    }
}

/// Session `s1` in project `alpha`: agent > (llm, tool failed, llm), completed at t=100.
fn fixture(session: &str, project: &str, src: &str) -> Vec<CanonicalEvent> {
    let e = |seq, span, t, body| ev(src, seq, session, span, t, body);
    vec![
        e(1, None, 10, started(project, "claude-code")),
        e(2, Some("agent"), 11, span(None, "agent", SpanKind::Agent, None)),
        e(3, Some("llm1"), 12, span(Some("agent"), "plan", SpanKind::Llm, Some("opus"))),
        e(4, Some("llm1"), 20, usage(100, 50, 1500, CostProvenance::Actual)),
        e(5, Some("llm1"), 21, ended(SpanStatus::Succeeded)),
        e(6, Some("tool"), 22, span(Some("agent"), "bash", SpanKind::Tool, None)),
        e(7, Some("tool"), 30, ended(SpanStatus::Failed)),
        e(8, Some("llm2"), 31, span(Some("agent"), "fix", SpanKind::Llm, Some("opus"))),
        e(9, Some("llm2"), 40, usage(10, 5, 500, CostProvenance::Estimate)),
        e(10, Some("llm2"), 41, ended(SpanStatus::Succeeded)),
        e(11, Some("agent"), 99, ended(SpanStatus::Succeeded)),
        e(12, None, 100, EventBody::SessionEnded { status: SessionStatus::Completed }),
    ]
}

fn expected_totals() -> Totals {
    Totals {
        tokens_input: 110,
        tokens_output: 55,
        cache_read: 0,
        cache_write: 0,
        cost_actual_usd_micros: 1500,
        cost_estimate_usd_micros: 500,
        cost_unknown_records: 0,
        billable_records: 2,
        failed_spans: 1,
    }
}

#[test]
fn duplicate_append_is_idempotent_across_reopen() {
    let path = temp_db("dup");
    let events = fixture("s1", "alpha", "src");
    {
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.append(&events).unwrap(), AppendOutcome { inserted: 12, duplicates: 0 });
        assert_eq!(store.append(&events).unwrap(), AppendOutcome { inserted: 0, duplicates: 12 });
    }
    let mut store = Store::open(&path).unwrap();
    assert_eq!(store.append(&events[..3]).unwrap(), AppendOutcome { inserted: 0, duplicates: 3 });
    assert_eq!(store.max_journal_seq().unwrap(), 12);
    assert_eq!(store.events_since(10, Some(&"s1".into())).unwrap().iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![11, 12]);
    assert_eq!(store.events_since(0, Some(&"other".into())).unwrap().len(), 0);
    assert_eq!(store.load_session(&"s1".into()).unwrap().event_count, 12);
}

#[test]
fn reopened_file_db_matches_reducer_offline() {
    let path = temp_db("reopen");
    let mut events = fixture("s1", "alpha", "src");
    {
        let mut store = Store::open(&path).unwrap();
        let reversed: Vec<_> = events.iter().rev().cloned().collect();
        store.append(&reversed).unwrap();
    }
    let store = Store::open(&path).unwrap();
    let loaded = store.load_session(&"s1".into()).unwrap();
    sort_events(&mut events);
    assert_eq!(loaded, TraceState::from_events("s1".into(), &events));
    assert_eq!(loaded.totals(), expected_totals());
    assert_eq!(loaded.session.status, SessionStatus::Completed);
}

#[test]
fn out_of_order_appends_resolve_in_replay_order() {
    let mut store = Store::open_in_memory().unwrap();
    let events = fixture("s1", "alpha", "src");
    store.append(&events[6..]).unwrap();
    let partial = store.load_session(&"s1".into()).unwrap();
    assert_eq!(partial.spans.keys().map(|k| k.0.as_str()).collect::<Vec<_>>(), vec!["llm2"]);
    assert_eq!(partial.unresolved(), vec![&SpanId::from("llm2")]);
    store.append(&events[..6]).unwrap();
    let ids: Vec<_> = store.session_events(&"s1".into()).unwrap().into_iter().map(|e| e.event_id.0).collect();
    assert_eq!(ids, (1..=12).map(|i| format!("src-{i}")).collect::<Vec<_>>());
    let full = store.load_session(&"s1".into()).unwrap();
    assert_eq!(full.spans[&SpanId::from("tool")].status, SpanStatus::Failed);
    assert_eq!(full.unresolved().len(), 0);
    assert_eq!(full.children(&"agent".into()), &["llm1".into(), "tool".into(), "llm2".into()]);
}

#[test]
fn contiguous_cursor_and_gaps() {
    let mut store = Store::open_in_memory().unwrap();
    let src = SourceId::from("s");
    let hb = |seq| ev("s", seq, "x", None, seq as Ns, EventBody::Heartbeat {});
    assert_eq!(store.source_cursor(&src).unwrap(), None);
    assert_eq!(store.source_gaps(&src).unwrap(), vec![]);
    store.append(&[hb(3), hb(5), hb(6), hb(9)]).unwrap();
    assert_eq!(store.source_cursor(&src).unwrap(), Some(3));
    assert_eq!(store.source_gaps(&src).unwrap(), vec![(4, 4), (7, 8)]);
    store.append(&[hb(4)]).unwrap();
    assert_eq!(store.source_cursor(&src).unwrap(), Some(6));
    assert_eq!(store.source_gaps(&src).unwrap(), vec![(7, 8)]);
    store.append(&[hb(1)]).unwrap();
    assert_eq!(store.source_cursor(&src).unwrap(), Some(1));
    assert_eq!(store.source_gaps(&src).unwrap(), vec![(2, 2), (7, 8)]);
    store.append(&[hb(2), hb(7), hb(8)]).unwrap();
    assert_eq!(store.source_cursor(&src).unwrap(), Some(9));
    assert_eq!(store.source_gaps(&src).unwrap(), vec![]);
}

#[test]
fn reconcile_interrupted_is_idempotent() {
    let mut store = Store::open_in_memory().unwrap();
    let stale = fixture("stale", "alpha", "a");
    let fresh = fixture("fresh", "alpha", "b");
    store.append(&stale[..8]).unwrap();
    store.append(&fresh[..9]).unwrap();
    assert_eq!(store.reconcile_interrupted(35, 1_000).unwrap(), vec![SessionId::from("stale")]);
    assert_eq!(store.reconcile_interrupted(35, 2_000).unwrap(), vec![]);
    let st = store.load_session(&"stale".into()).unwrap();
    assert_eq!(st.session.status, SessionStatus::Interrupted);
    assert_eq!(st.session.ended_at_utc_ns, Some(31));
    assert_eq!(store.load_session(&"fresh".into()).unwrap().session.status, SessionStatus::Live);
    let interrupted = store.list_sessions(&SessionFilter { status: Some(SessionStatus::Interrupted), ..Default::default() }).unwrap();
    assert_eq!(interrupted.iter().map(|s| s.session_id.0.as_str()).collect::<Vec<_>>(), vec!["stale"]);
    let synthetic = store.session_events(&"stale".into()).unwrap().pop().unwrap();
    assert_eq!((synthetic.source_id.0.as_str(), synthetic.event_id.0.as_str()), ("collector", "interrupted:stale"));
    assert_eq!(store.max_journal_seq().unwrap(), 18);
}

#[test]
fn redact_known_secret_shapes() {
    let cases = [
        ("key=sk-ant-api03-abcdefghijklmnopqrstuvwxyz rest", "key=[REDACTED] rest"),
        ("aws AKIAABCDEFGHIJKLMNOP.", "aws [REDACTED]."),
        ("Authorization: Bearer abc.def-ghi_123/xyz=", "Authorization: Bearer [REDACTED]"),
        ("t ghp_abcdefghijklmnopqrstuvwxyz0123 e", "t [REDACTED] e"),
        ("github_pat_11ABCDEFG0123456789_abcdefghij", "[REDACTED]"),
        ("slack xoxb-1234567890-abcdef!", "slack [REDACTED]!"),
        ("a -----BEGIN RSA PRIVATE KEY-----\nMIIEow\nIBAAK\n-----END RSA PRIVATE KEY----- b", "a [REDACTED] b"),
        ("x -----BEGIN PRIVATE KEY-----\nunterminated", "x [REDACTED]"),
        ("é sk-abcdefghijklmnopqrstu ü", "é [REDACTED] ü"),
    ];
    for (input, want) in cases {
        assert_eq!(redact(input), want, "input: {input:?}");
    }
    for clean in ["task-something-long-enough-for-a-key", "sk-learn", "-----BEGIN CERTIFICATE-----", "Bearer x"] {
        assert!(matches!(redact(clean), std::borrow::Cow::Borrowed(_)), "{clean:?}");
    }
}

#[test]
fn secrets_never_reach_the_db_file() {
    let path = temp_db("redact");
    let secret = "sk-ant-api03-SECRETSECRETSECRET1234";
    let aws = "AKIAZZZZZZZZZZZZZZZZ";
    {
        let mut store = Store::open(&path).unwrap();
        let mut attrs = Attributes::new();
        attrs.insert("env".into(), serde_json::json!({ "nested": [format!("AWS={aws}")] }));
        let body = EventBody::SpanStarted {
            parent_span_id: None,
            name: format!("call with {secret}"),
            kind: SpanKind::Llm,
            model: None,
            provider: None,
            tool_name: None,
            agent_name: None,
            retry_of: None,
            input_preview: Some(format!("Authorization: Bearer {secret}")),
            attributes: attrs,
        };
        store.append(&[ev("src", 1, "s1", Some("llm"), 5, body)]).unwrap();
        store.put_payload("p1", &"s1".into(), format!("stdout {secret}").as_bytes()).unwrap();
        let st = store.load_session(&"s1".into()).unwrap();
        assert_eq!(st.spans[&SpanId::from("llm")].name, "call with [REDACTED]");
        assert_eq!(st.spans[&SpanId::from("llm")].input_preview.as_deref(), Some("Authorization: Bearer [REDACTED]"));
        assert_eq!(store.get_payload("p1").unwrap().unwrap(), b"stdout [REDACTED]");
    }
    let mut bytes = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        bytes.extend(std::fs::read(&p).unwrap_or_default());
    }
    let hay = String::from_utf8_lossy(&bytes);
    assert!(hay.contains("[REDACTED]"));
    assert!(!hay.contains("SECRETSECRETSECRET"));
    assert!(!hay.contains(aws));
}

#[cfg(unix)]
#[test]
fn db_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let path = temp_db("perms");
    let mut store = Store::open(&path).unwrap();
    store.append(&fixture("s1", "alpha", "src")).unwrap();
    let mode = |suffix: &str| {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o777).ok()
    };
    assert_eq!(mode(""), Some(0o600));
    assert_eq!(mode("-wal"), Some(0o600));
    assert_eq!(mode("-shm"), Some(0o600));
}

#[test]
fn purge_sessions_and_payloads_independently() {
    let mut store = Store::open_in_memory().unwrap();
    store.append(&fixture("done", "alpha", "a")).unwrap();
    store.append(&fixture("live", "alpha", "b")[..11]).unwrap();
    store.put_payload("pd", &"done".into(), b"done-out").unwrap();
    store.put_payload("pl", &"live".into(), b"live-out").unwrap();

    assert_eq!(store.purge(Some(50), None).unwrap(), PurgeOutcome { sessions: 0, payloads: 0 });
    assert_eq!(store.purge(Some(101), None).unwrap(), PurgeOutcome { sessions: 1, payloads: 0 });
    let ids: Vec<_> = store.list_sessions(&SessionFilter::default()).unwrap().into_iter().map(|s| s.session_id.0).collect();
    assert_eq!(ids, vec!["live"]);
    assert_eq!(store.session_events(&"done".into()).unwrap().len(), 0);
    assert_eq!(store.get_payload("pd").unwrap(), None);
    assert_eq!(store.get_payload("pl").unwrap().unwrap(), b"live-out");

    assert_eq!(store.purge(None, Some(i64::MAX)).unwrap(), PurgeOutcome { sessions: 0, payloads: 1 });
    assert_eq!(store.get_payload("pl").unwrap(), None);
    assert_eq!(store.session_events(&"live".into()).unwrap().len(), 11);
}

#[test]
fn list_sessions_filters_and_totals() {
    let mut store = Store::open_in_memory().unwrap();
    store.append(&fixture("s1", "alpha", "a")).unwrap();
    let mut beta = fixture("s2", "beta", "b");
    for e in &mut beta {
        e.occurred_at_utc_ns += 1_000;
        if let EventBody::SessionStarted { source_adapter, .. } = &mut e.body {
            *source_adapter = "codex".into();
        }
    }
    store.append(&beta[..5]).unwrap();

    let all = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(all.iter().map(|s| s.session_id.0.as_str()).collect::<Vec<_>>(), vec!["s2", "s1"]);
    let s1 = &all[1];
    assert_eq!(
        *s1,
        SessionSummary {
            session_id: "s1".into(),
            project_id: "alpha".into(),
            working_directory: "/work/alpha".into(),
            source_adapter: "claude-code".into(),
            harness: "claude-code".into(),
            host: "devbox".into(),
            status: SessionStatus::Completed,
            started_at_utc_ns: Some(10),
            ended_at_utc_ns: Some(100),
            last_event_at_utc_ns: Some(100),
            model: Some("opus".into()),
            totals: expected_totals(),
            span_count: 4,
        }
    );
    assert_eq!(all[0].status, SessionStatus::Live);
    assert_eq!(all[0].totals.cost_actual_usd_micros, 1500);
    assert_eq!(all[0].span_count, 2);

    let ids = |f: SessionFilter| store.list_sessions(&f).unwrap().into_iter().map(|s| s.session_id.0).collect::<Vec<_>>();
    assert_eq!(ids(SessionFilter { status: Some(SessionStatus::Live), ..Default::default() }), vec!["s2"]);
    assert_eq!(ids(SessionFilter { project: Some("alpha".into()), ..Default::default() }), vec!["s1"]);
    assert_eq!(ids(SessionFilter { source: Some("codex".into()), ..Default::default() }), vec!["s2"]);
    assert_eq!(ids(SessionFilter { text: Some("WORK/BE".into()), ..Default::default() }), vec!["s2"]);
    assert_eq!(ids(SessionFilter { text: Some("OPUS".into()), ..Default::default() }), vec!["s2", "s1"]);
    assert_eq!(ids(SessionFilter { limit: Some(1), ..Default::default() }), vec!["s2"]);

    store.rebuild_projections().unwrap();
    assert_eq!(store.list_sessions(&SessionFilter::default()).unwrap(), all);
}
