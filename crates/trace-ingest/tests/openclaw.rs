//! OpenClaw: the conformance scenario as `openclaw-agent.sqlite` rows, shaped like a live run of
//! `openclaw agent --local` 2026.10.6 against Ollama. Each session window's pi entries sit in
//! `transcript_events` by `seq`, larger ones as zstd frames; `sessions_spawn` returns at once with the
//! child's key, the parent waits for it with `agents_wait`, and the child's window names the parent's key
//! in `spawned_by`.

mod support;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// OpenClaw's tables as of 2026.10.6, trimmed to the columns that matter and the ones that cannot be null.
const SCHEMA: &str = "
CREATE TABLE session_windows (session_id TEXT NOT NULL PRIMARY KEY, session_key TEXT NOT NULL, created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL, parent_session_key TEXT, spawned_by TEXT);
CREATE TABLE transcript_events (session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT, created_at INTEGER NOT NULL,
  event_zstd BLOB, event_utf8_bytes INTEGER, navigation_json TEXT, PRIMARY KEY (session_id, seq));";

/// Entries this long are stored compressed, as OpenClaw does past its threshold.
const COMPRESS_FROM: usize = 400;

fn usage() -> Value {
    json!({"input": 100, "output": 20, "cacheRead": 50, "cacheWrite": 0, "totalTokens": 170, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

/// A reply written at `end`; OpenClaw records no request start or duration.
fn assistant(end: f64, stop: &str, calls: &[(&str, &str)]) -> Value {
    let content: Vec<Value> = calls.iter().map(|(id, name)| json!({"type": "toolCall", "id": id, "name": name, "arguments": {}})).collect();
    json!({"role": "assistant", "content": content, "api": "ollama", "provider": "ollama", "model": "qwen3:8b", "usage": usage(), "stopReason": stop, "timestamp": ms(end)})
}

fn result(secs: f64, id: &str, name: &str, error: bool, details: Value) -> Value {
    json!({"role": "toolResult", "toolCallId": id, "toolName": name, "content": [{"type": "text", "text": "(output)"}], "details": details, "isError": error, "timestamp": ms(secs)})
}

fn user(secs: f64) -> Value {
    json!({"role": "user", "content": "(prompt)", "timestamp": ms(secs)})
}

/// One window: its header, a model change, then each message at its time, chained by `parentId`.
fn window(c: &Connection, id: &str, key: &str, spawned_by: Option<&str>, start: f64, messages: Vec<(f64, Value)>) {
    let end = messages.last().map_or(start, |(t, _)| *t);
    c.execute("INSERT INTO session_windows VALUES (?1, ?2, ?3, ?4, ?5, ?5)", params![id, key, ms(start), ms(end), spawned_by]).unwrap();
    let mut entries = vec![
        (start, json!({"type": "session", "version": 4, "id": id, "timestamp": iso(start), "cwd": "/work/conf"})),
        (start, json!({"type": "model_change", "provider": "ollama", "modelId": "qwen3:8b", "id": format!("{id}-mc"), "parentId": null, "timestamp": iso(start)})),
    ];
    let mut parent = json!(format!("{id}-mc"));
    for (i, (secs, m)) in messages.into_iter().enumerate() {
        let eid = format!("{id}-{i}");
        entries.push((secs, json!({"type": "message", "id": eid, "parentId": parent, "timestamp": iso(secs), "message": m})));
        parent = json!(eid);
    }
    for (seq, (secs, e)) in entries.into_iter().enumerate() {
        let text = e.to_string();
        let (plain, packed) = if text.len() >= COMPRESS_FROM { (None, Some(ruzstd::encoding::compress_to_vec(text.as_bytes(), ruzstd::encoding::CompressionLevel::Fastest))) } else { (Some(&text), None) };
        let nav = packed.as_ref().map(|_| "{\"version\":1}");
        c.execute("INSERT INTO transcript_events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![id, seq as i64, plain, ms(secs), packed, text.len() as i64, nav]).unwrap();
    }
}

fn openclaw(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let path = d.join("openclaw-agent.sqlite");
    let c = Connection::open(&path).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    let spawned = json!({"status": "accepted", "childSessionKey": "agent:main:subagent:helper", "runId": "run-1"});
    window(&c, "w-main", "agent:main:main", None, 0., vec![
        (0., user(0.)),
        (2., assistant(2., "toolUse", &[("c_read", "read")])),
        (3., result(3., "c_read", "read", false, json!({"kind": "text"}))),
        (4., assistant(4., "toolUse", &[("c_exec", "exec")])),
        (5., result(5., "c_exec", "exec", false, json!({"status": "completed", "exitCode": 1, "exitReason": "exit", "durationMs": 618}))),
        (6., assistant(6., "toolUse", &[("c_spawn", "sessions_spawn")])),
        (6.1, result(6.1, "c_spawn", "sessions_spawn", false, spawned)),
        (6.2, assistant(6.2, "toolUse", &[("c_wait", "agents_wait")])),
        (10., result(10., "c_wait", "agents_wait", false, json!({"status": "completed"}))),
        (11., assistant(11., "stop", &[])),
    ]);
    window(&c, "w-helper", "agent:main:subagent:helper", Some("agent:main:main"), 6., vec![
        (6., user(6.)),
        (8., assistant(8., "toolUse", &[("s_grep", "grep")])),
        (8.5, result(8.5, "s_grep", "grep", false, json!({}))),
        (9.5, assistant(9.5, "stop", &[])),
    ]);
    let packed: i64 = c.query_row("SELECT count(*) FROM transcript_events WHERE event_json IS NULL", [], |r| r.get(0)).unwrap();
    assert!(packed > 0, "some entries are compressed");
    vec![(path, "openclaw")]
}

#[test]
fn openclaw_conforms() {
    conforms("openclaw", openclaw(&dir()));
}

#[test]
fn sessions_are_openclaws_and_carry_no_text() {
    let files = openclaw(&dir());
    let mut store = Store::open_in_memory().unwrap();
    let first = import_file(&mut store, &files[0].0, "openclaw", &Capture { payloads: true, projects: Vec::new() }).unwrap();
    let again = import_file(&mut store, &files[0].0, "openclaw", &Capture::default()).unwrap();
    assert_eq!((again.inserted, again.duplicates), (0, first.inserted), "re-reading the database adds nothing");
    let summary = &store.list_sessions(&SessionFilter::default()).unwrap()[0];
    assert_eq!((summary.harness.as_str(), summary.session_id.0.as_str()), ("openclaw", "w-main"));
    let s = store.load_session(&summary.session_id).unwrap();
    let sub = s.spans.values().find(|x| x.kind == SpanKind::Subagent).unwrap();
    assert_eq!(sub.parent_span_id.as_ref().map(|p| p.0.as_str()), Some("tool:c_spawn"), "the sub-agent nests under sessions_spawn, not agents_wait");
    for x in s.spans.values() {
        assert_eq!((&x.input_preview, &x.output_preview), (&None, &None), "{} carries no text even with payload capture on", x.span_id);
    }
}

#[test]
fn reading_a_state_folder_reads_every_agent() {
    let d = dir();
    let agent = d.join("agents/main/agent");
    std::fs::create_dir_all(&agent).unwrap();
    let (db, _) = openclaw(&agent).remove(0);
    assert!(db.ends_with("agents/main/agent/openclaw-agent.sqlite"));
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &d, "openclaw", &Capture::default()).unwrap();
    assert_eq!(store.list_sessions(&SessionFilter::default()).unwrap().len(), 1);
}
