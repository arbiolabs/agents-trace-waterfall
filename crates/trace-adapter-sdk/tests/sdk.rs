use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use trace_adapter_sdk::*;
use trace_core::*;

const T0: Ns = 1_700_000_000_000_000_000;

fn recorder(step_ns: Ns) -> Recorder {
    let t = Arc::new(AtomicI64::new(T0));
    Recorder::with_clock("src".into(), "sess".into(), move || t.fetch_add(step_ns, Ordering::SeqCst))
}

fn heartbeats(n: usize) -> Vec<CanonicalEvent> {
    let mut r = recorder(1_000_000);
    for _ in 0..n {
        r.heartbeat();
    }
    r.take()
}

#[test]
fn recorder_numbers_events_and_they_reduce() {
    let mut r = recorder(1_000_000);
    r.session_started("proj", "/work", "test-harness");
    let root = r.span_started(SpanStart::new(SpanKind::Agent, "root"));
    let llm = r.span_started(SpanStart { parent: Some(root.clone()), model: Some("m1".into()), ..SpanStart::new(SpanKind::Llm, "call") });
    r.usage(&llm, Usage { tokens_input: Some(10), ..Usage::default() }, false);
    r.usage(&llm, Usage { tokens_input: Some(42), ..Usage::default() }, true);
    r.span_event(&llm, "first_token", Attributes::new());
    r.artifact(&root, "src/main.rs", Some("edit"));
    r.span_ended(&llm, SpanStatus::Failed, Some("boom"));
    r.span_ended(&root, SpanStatus::Succeeded, None);
    r.session_ended(SessionStatus::Completed);
    let events = r.take();

    assert_eq!(events.iter().map(|e| e.source_seq).collect::<Vec<_>>(), (1..=10).collect::<Vec<_>>());
    assert!(events.windows(2).all(|w| w[0].event_id < w[1].event_id));
    assert_eq!(events[0].occurred_at_utc_ns, T0);
    assert_eq!(events[2].span_id.as_ref(), Some(&llm));
    assert_eq!(events.iter().map(|e| e.body.type_name()).collect::<Vec<_>>(), [
        "session.started", "span.started", "span.started", "usage.reported", "usage.reported",
        "span.event", "artifact.recorded", "span.ended", "span.ended", "session.ended"
    ]);

    let mut buf = Vec::new();
    write_jsonl(&mut buf, &events).unwrap();
    assert_eq!(read_jsonl(&buf[..]).unwrap(), events);

    let s = TraceState::from_events("sess".into(), &events);
    assert_eq!(s.session.status, SessionStatus::Completed);
    assert_eq!(s.session.source_adapter, "src");
    assert_eq!(s.spans[&llm].parent_span_id.as_ref(), Some(&root));
    assert_eq!(s.spans[&llm].error.as_deref(), Some("boom"));
    assert_eq!(s.spans[&llm].usage.as_ref().unwrap().tokens_input, Some(42));
    assert_eq!(s.spans[&root].artifacts[0].path, "src/main.rs");
    assert!(r.take().is_empty());
}

#[test]
fn recorder_ids_stay_monotonic_when_clock_steps_back() {
    let events = {
        let mut r = recorder(-5_000_000);
        for _ in 0..5 {
            r.heartbeat();
        }
        r.take()
    };
    assert!(events.windows(2).all(|w| w[0].event_id < w[1].event_id));
    let other = Recorder::with_clock("src".into(), "other-sess".into(), || T0).span_started(SpanStart::new(SpanKind::Agent, "a"));
    let same = recorder(0).span_started(SpanStart::new(SpanKind::Agent, "a"));
    assert_ne!(other, same, "span ids differ across sessions at the same instant");
}

#[test]
fn jsonl_skips_blank_lines_and_names_bad_line() {
    let events = heartbeats(2);
    let mut buf = Vec::new();
    write_jsonl(&mut buf, &events).unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert_eq!(text.lines().count(), 2);
    assert_eq!(read_jsonl(format!("\n{text}\n").as_bytes()).unwrap(), events);

    let err = read_jsonl(format!("{text}{{\"event_type\":\"nope\"}}\n").as_bytes()).unwrap_err();
    assert!(format!("{err:#}").starts_with("line 3: "), "{err:#}");
}

struct FakeCollector {
    store: Arc<Mutex<Vec<CanonicalEvent>>>,
    duplicates: Arc<Mutex<usize>>,
    connections: Arc<Mutex<usize>>,
}

fn socket_path(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("tw-sdk-{}-{name}.sock", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

/// Persists, dedups by event_id, reports the highest contiguous seq. On the first connection it
/// persists frame `crash_on_frame` and closes without acking.
fn fake_collector(path: &PathBuf, crash_on_frame: Option<usize>) -> FakeCollector {
    let listener = UnixListener::bind(path).unwrap();
    let fc = FakeCollector { store: Default::default(), duplicates: Default::default(), connections: Default::default() };
    let (store, dups, conns) = (fc.store.clone(), fc.duplicates.clone(), fc.connections.clone());
    std::thread::spawn(move || {
        for (conn_no, stream) in listener.incoming().enumerate() {
            *conns.lock().unwrap() += 1;
            let mut out = stream.unwrap();
            let mut lines = BufReader::new(out.try_clone().unwrap()).lines();
            let cursor = || {
                let seqs: HashSet<u64> = store.lock().unwrap().iter().map(|e| e.source_seq).collect();
                let c = (1..).take_while(|s| seqs.contains(s)).last();
                json!(c)
            };
            let hello: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            assert_eq!(hello, json!({"type": "hello", "protocol": 1, "source_id": "src"}));
            writeln!(out, "{}", json!({"type": "welcome", "protocol": 1, "cursor": cursor()})).unwrap();
            for (frame_no, line) in lines.enumerate() {
                let Ok(line) = line else { break };
                let frame: Value = serde_json::from_str(&line).unwrap();
                let events: Vec<CanonicalEvent> = serde_json::from_value(frame["events"].clone()).unwrap();
                {
                    let mut s = store.lock().unwrap();
                    for e in events {
                        if s.iter().any(|x| x.event_id == e.event_id) {
                            *dups.lock().unwrap() += 1;
                        } else {
                            s.push(e);
                        }
                    }
                }
                if conn_no == 0 && Some(frame_no) == crash_on_frame {
                    break;
                }
                writeln!(out, "{}", json!({"type": "ack", "cursor": cursor()})).unwrap();
            }
        }
    });
    fc
}

#[test]
fn client_reconnects_and_resends_after_cursor() {
    let path = socket_path("reconnect");
    let fc = fake_collector(&path, Some(1));
    let events = heartbeats(30);
    let mut c = Client::new(&path, "src".into(), 100);

    c.send(&events[..10]).unwrap();
    assert_eq!((c.pending(), c.cursor()), (0, Some(10)));
    c.send(&events[10..20]).unwrap();
    assert_eq!(c.pending(), 10, "frame persisted but unacked stays buffered");
    c.send(&events[20..]).unwrap();

    assert_eq!(c.flush(Duration::from_secs(5)).unwrap(), Some(30));
    assert_eq!(*fc.connections.lock().unwrap(), 2);
    assert_eq!(*fc.duplicates.lock().unwrap(), 0, "events at or below the welcome cursor are not resent");
    assert_eq!(*fc.store.lock().unwrap(), events);
    assert_eq!(c.dropped(), 0);
}

#[test]
fn client_counts_dropped_events_on_overflow() {
    let path = socket_path("overflow");
    let events = heartbeats(5);
    let mut c = Client::new(&path, "src".into(), 3);
    c.send(&events).unwrap();
    assert_eq!((c.dropped(), c.pending()), (2, 3));
    let err = c.flush(Duration::from_millis(100)).unwrap_err().to_string();
    assert!(err.starts_with("3 events unacked after 100ms (2 dropped)"), "{err}");

    let fc = fake_collector(&path, None);
    assert_eq!(c.flush(Duration::from_secs(5)).unwrap(), None, "gap at seq 1-2 keeps the cursor null");
    assert_eq!(fc.store.lock().unwrap().iter().map(|e| e.source_seq).collect::<Vec<_>>(), [3, 4, 5]);
    assert_eq!(c.dropped(), 2);
}

#[test]
fn client_rejects_foreign_source() {
    let mut c = Client::new(socket_path("foreign"), "other".into(), 3);
    assert!(c.send(&heartbeats(1)).is_err());
    assert_eq!(c.pending(), 0);
}
