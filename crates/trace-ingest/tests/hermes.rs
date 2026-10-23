//! Hermes Agent: the conformance scenario as Hermes records it in `state.db`. Hermes keeps token usage per
//! session, so the scenario is checked with [`per_turn_usage`]: model calls carry no usage, the agent and
//! the sub-agent carry their sessions' totals.

mod per_turn_usage;
mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// The schema columns the adapter reads, as `hermes_state_common.py` creates them.
const SCHEMA: &str = "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT, started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
     input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0, cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0, cwd TEXT, estimated_cost_usd REAL, actual_cost_usd REAL);
     CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
     effect_disposition TEXT, timestamp REAL NOT NULL, token_count INTEGER, finish_reason TEXT);";

fn secs(s: f64) -> f64 {
    ms(s) as f64 / 1000.
}

fn session(c: &rusqlite::Connection, id: &str, parent: Option<&str>, start: f64, end: f64, tokens: (i64, i64)) {
    c.execute(
        "INSERT INTO sessions (id, source, model, parent_session_id, started_at, ended_at, end_reason, input_tokens, output_tokens, cache_read_tokens, cwd, estimated_cost_usd) VALUES (?1, 'cli', 'claude-sonnet-4-5', ?2, ?3, ?4, 'completed', ?5, ?6, 50, '/work/conf', 0.0)",
        rusqlite::params![id, parent, secs(start), secs(end), tokens.0, tokens.1],
    )
    .unwrap();
}

/// A message row; `calls` are (call id, tool name), as OpenAI-style `tool_calls`.
fn message(c: &rusqlite::Connection, sid: &str, role: &str, at: f64, calls: &[(&str, &str)], result_of: Option<(&str, Value)>) {
    let calls = (!calls.is_empty()).then(|| json!(calls.iter().map(|(id, name)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": "{}"}})).collect::<Vec<_>>()).to_string());
    let (call_id, content) = result_of.map_or((None, Some("(text)".to_owned())), |(id, v)| (Some(id.to_owned()), Some(v.to_string())));
    let finish = if role == "assistant" { Some(if calls.is_some() { "tool_calls" } else { "stop" }) } else { None };
    c.execute(
        "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, timestamp, finish_reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![sid, role, content, call_id, calls, secs(at), finish],
    )
    .unwrap();
}

fn hermes(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let db = d.join("state.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    session(&c, "hm", None, 0., 11.5, (600, 80));
    session(&c, "hm-helper", Some("hm"), 6., 9.5, (200, 40));
    message(&c, "hm", "user", 0., &[], None);
    message(&c, "hm", "assistant", 2., &[("h_read", "read_file")], None);
    message(&c, "hm", "tool", 3., &[], Some(("h_read", json!({"content": "(file)"}))));
    message(&c, "hm", "assistant", 4., &[("h_bash", "terminal")], None);
    message(&c, "hm", "tool", 5., &[], Some(("h_bash", json!({"output": "(output)", "exit_code": 1, "error": null}))));
    message(&c, "hm", "assistant", 6., &[("h_task", "delegate_task")], None);
    message(&c, "hm-helper", "user", 6., &[], None);
    message(&c, "hm-helper", "assistant", 8., &[("h_grep", "search_files")], None);
    message(&c, "hm-helper", "tool", 8.5, &[], Some(("h_grep", json!({"matches": []}))));
    message(&c, "hm-helper", "assistant", 9.5, &[], None);
    message(&c, "hm", "tool", 10., &[], Some(("h_task", json!({"results": []}))));
    message(&c, "hm", "assistant", 11., &[], None);
    vec![(db, "hermes")]
}

#[test]
fn hermes_conforms_with_usage_per_session() {
    per_turn_usage::conforms_with_usage_on("hermes", hermes(&dir()), &[SpanKind::Agent, SpanKind::Subagent], (600 + 200, 80 + 40));
}

/// A live recording (Hermes from source at e04614a, against a local qwen3:8b with tool search off), its
/// text removed: read a file, a shell command that exited 1, a `delegate_task` whose child searched files.
#[test]
fn a_live_recording_conforms_too() {
    let db = dir().join("state.db");
    let dump = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes/live.sql")).unwrap();
    rusqlite::Connection::open(&db).unwrap().execute_batch(&dump).unwrap();
    per_turn_usage::conforms_with_usage_on("hermes (live)", vec![(db.clone(), "hermes")], &[SpanKind::Agent, SpanKind::Subagent], (31747 + 10362, 901 + 576));
    let s = load(&db);
    let main = "local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b";
    let sub = &s.spans[&SpanId("subagent:20261023_211601_3deb47".into())];
    assert_eq!(sub.parent_span_id, Some(SpanId(format!("tool:{main}:call_qm8wow0d"))), "under the delegate_task call");
    let failed: Vec<_> = s.spans.values().filter(|x| x.status == SpanStatus::Failed).map(|x| x.tool_name.as_deref()).collect();
    assert_eq!(failed, [Some("terminal")]);
}

fn load(db: &Path) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, db, "hermes", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1);
    store.load_session(&sessions[0].session_id).unwrap()
}

#[test]
fn a_tool_error_fails_the_call_even_with_notes_after_the_json() {
    let d = dir();
    let db = hermes(&d).remove(0).0;
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute("UPDATE messages SET content = ?1 WHERE tool_call_id = 'h_read'", [r#"{"error": "no such file"}

[Tool loop warning: (text)]"#]).unwrap();
    let s = load(&db);
    assert_eq!(s.spans[&SpanId("tool:hm:h_read".into())].status, SpanStatus::Failed);
}

#[test]
fn a_call_through_the_tool_call_bridge_is_named_after_the_tools_it_ran() {
    let d = dir();
    let db = hermes(&d).remove(0).0;
    let c = rusqlite::Connection::open(&db).unwrap();
    let bridged = json!([{"id": "h_task", "type": "function", "function": {"name": "tool_call", "arguments": json!({"calls": [{"name": "delegate_task", "arguments": {}}]}).to_string()}}]);
    c.execute("UPDATE messages SET tool_calls = ?1 WHERE tool_calls LIKE '%h_task%'", [bridged.to_string()]).unwrap();
    let s = load(&db);
    assert_eq!(s.spans[&SpanId("tool:hm:h_task".into())].tool_name.as_deref(), Some("delegate_task"));
    assert_eq!(s.spans[&SpanId("subagent:hm-helper".into())].parent_span_id, Some(SpanId("tool:hm:h_task".into())), "still the sub-agent's spawner");
}

#[test]
fn a_child_session_no_delegate_call_started_stays_its_own_session() {
    let d = dir();
    let db = hermes(&d).remove(0).0;
    let c = rusqlite::Connection::open(&db).unwrap();
    // A compression continuation: the parent's child, started after the parent's last call finished.
    session(&c, "hm-next", Some("hm"), 12., 13., (10, 1));
    message(&c, "hm-next", "user", 12., &[], None);
    message(&c, "hm-next", "assistant", 13., &[], None);
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &db, "hermes", &Capture::default()).unwrap();
    let mut ids: Vec<_> = store.list_sessions(&SessionFilter::default()).unwrap().into_iter().map(|s| s.session_id.0).collect();
    ids.sort();
    assert_eq!(ids, ["hm", "hm-next"]);
}

#[test]
fn reading_again_after_the_totals_grew_reports_the_new_totals() {
    let d = dir();
    let db = hermes(&d).remove(0).0;
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &db, "hermes", &Capture::default()).unwrap();
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute("UPDATE sessions SET input_tokens = 900, ended_at = ?1 WHERE id = 'hm'", [secs(12.)]).unwrap();
    import_file(&mut store, &db, "hermes", &Capture::default()).unwrap();
    let s = store.load_session(&SessionId("hm".into())).unwrap();
    assert_eq!(s.spans[&SpanId("agent:hm".into())].usage.as_ref().and_then(|u| u.tokens_input), Some(900));
}
