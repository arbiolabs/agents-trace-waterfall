//! Crush: the conformance scenario written the way Crush 0.98 writes its project database (checked
//! against a live run on a local model), and the format's quirks.
//!
//! Crush cannot pass `support::conforms` as is: it records no token usage per model call (only a
//! running cost per session), so the check that every LLM span reports token counts cannot hold, and
//! [`conforms_without_call_usage`] is that check without it. Crush also stamps whole seconds, so the
//! scenario's half seconds land on the second before.

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

const SCHEMA: &str = "
CREATE TABLE sessions (id TEXT PRIMARY KEY, parent_session_id TEXT, title TEXT NOT NULL, message_count INTEGER NOT NULL DEFAULT 0, prompt_tokens INTEGER NOT NULL DEFAULT 0, completion_tokens INTEGER NOT NULL DEFAULT 0, cost REAL NOT NULL DEFAULT 0.0, updated_at INTEGER NOT NULL, created_at INTEGER NOT NULL, summary_message_id TEXT, todos TEXT, channel TEXT);
CREATE TABLE messages (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL, parts TEXT NOT NULL DEFAULT '[]', model TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, finished_at INTEGER, provider TEXT, is_summary_message INTEGER DEFAULT 0 NOT NULL);
";

fn s(secs: f64) -> i64 {
    ms(secs) / 1000
}

fn part(kind: &str, data: Value) -> Value {
    json!({"type": kind, "data": data})
}

fn finish(reason: &str, secs: f64) -> Value {
    part("finish", json!({"reason": reason, "time": s(secs)}))
}

fn call(id: &str, name: &str) -> Value {
    part("tool_call", json!({"id": id, "name": name, "input": "{}", "provider_executed": false, "finished": true}))
}

fn result(id: &str, name: &str, content: &str, metadata: Value) -> Value {
    part("tool_result", json!({"tool_call_id": id, "name": name, "content": content, "data": "", "mime_type": "", "metadata": metadata.to_string(), "is_error": false}))
}

/// `<dir>/conf/.crush/crush.db` holding `sessions` (id, parent, created, cost) and `messages`
/// (id, session, role, created, parts).
fn db(d: &Path, sessions: &[(&str, Option<&str>, f64, f64)], messages: &[(&str, &str, &str, f64, Value)]) -> PathBuf {
    let path = d.join("conf/.crush/crush.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute_batch(SCHEMA).unwrap();
    for (id, parent, at, cost) in sessions {
        c.execute("INSERT INTO sessions (id, parent_session_id, title, cost, created_at, updated_at) VALUES (?1, ?2, '(title)', ?3, ?4, ?4)", rusqlite::params![id, parent, cost, s(*at)]).unwrap();
    }
    for (id, sid, role, at, parts) in messages {
        let (model, provider) = if *role == "assistant" { (Some("claude-sonnet-4-5"), Some("anthropic")) } else { (None, None) };
        c.execute("INSERT INTO messages (id, session_id, role, parts, model, provider, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)", rusqlite::params![id, sid, role, parts.to_string(), model, provider, s(*at)]).unwrap();
    }
    path
}

const SUB: &str = "m4$$c_agent";

fn conformance(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let user = json!([part("text", json!({"text": "(prompt)"})), finish("stop", 0.)]);
    let bash_meta = json!({"start_time": ms(4.), "end_time": ms(5.), "output": "(output)", "description": "", "working_directory": "/work/conf"});
    let db = db(
        d,
        &[("cr", None, 0., 0.0123), (SUB, Some("cr"), 6., 0.004)],
        &[
            ("m1", "cr", "user", 0., user.clone()),
            ("m2", "cr", "assistant", 0.5, json!([call("c_read", "view"), finish("tool_use", 3.)])),
            ("m3", "cr", "tool", 3., json!([result("c_read", "view", "(file)", json!({}))])),
            ("m4", "cr", "assistant", 3., json!([call("c_bash", "bash"), finish("tool_use", 5.)])),
            ("m5", "cr", "tool", 5., json!([result("c_bash", "bash", "\n(stderr)\n\nExit code 1\n\n<cwd>/work/conf</cwd>", bash_meta)])),
            ("m6", "cr", "assistant", 5., json!([call("c_agent", "agent"), finish("tool_use", 10.)])),
            ("h1", SUB, "user", 6., user.clone()),
            ("h2", SUB, "assistant", 6.5, json!([call("c_grep", "grep"), finish("tool_use", 8.5)])),
            ("h3", SUB, "tool", 8.5, json!([result("c_grep", "grep", "(matches)", json!({}))])),
            ("h4", SUB, "assistant", 8.5, json!([part("text", json!({"text": "(answer)"})), finish("end_turn", 9.5)])),
            ("m7", "cr", "tool", 10., json!([result("c_agent", "agent", "(answer)", json!({}))])),
            ("m8", "cr", "assistant", 10., json!([part("text", json!({"text": "(answer)"})), finish("end_turn", 11.)])),
        ],
    );
    let registry = d.join("projects.json");
    let project = db.parent().unwrap().parent().unwrap();
    std::fs::write(&registry, json!({"projects": [{"path": project, "data_dir": db.parent().unwrap(), "last_accessed": iso(11.)}]}).to_string()).unwrap();
    vec![(registry, "crush")]
}

/// `support::conforms` but for the per-call token counts Crush does not record.
fn conforms_without_call_usage(files: Vec<(PathBuf, &'static str)>) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    for (f, adapter) in &files {
        import_file(&mut store, f, adapter, &Capture::default()).unwrap();
    }
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1, "one session, sub-agent included");
    assert_eq!((sessions[0].project_id.as_str(), sessions[0].turn_count), ("conf", 1));
    let s = store.load_session(&sessions[0].session_id).unwrap();
    assert!(s.unresolved().is_empty(), "every span's parent arrived: {:?}", s.unresolved());
    let kind = |k: SpanKind| s.spans.values().filter(move |x| x.kind == k);
    let root = kind(SpanKind::Agent).next().unwrap();
    let turns: Vec<_> = kind(SpanKind::Turn).filter(|t| t.parent_span_id.as_ref() == Some(&root.span_id)).collect();
    assert_eq!(turns.len(), 1);
    let turn = turns[0];
    assert_eq!(turn.status, SpanStatus::Succeeded);
    let under_turn = |k: SpanKind| s.spans.values().filter(|x| x.kind == k && x.parent_span_id.as_ref() == Some(&turn.span_id)).count();
    assert!(under_turn(SpanKind::Llm) >= 3);
    assert!(kind(SpanKind::Llm).all(|l| l.model.is_some()), "every model call names its model");
    assert_eq!(kind(SpanKind::Tool).filter(|t| t.status == SpanStatus::Failed).count(), 1, "the failing shell command, and only it, failed");
    let sub = kind(SpanKind::Subagent).next().unwrap();
    assert_eq!(kind(SpanKind::Subagent).count(), 1);
    assert_eq!(sub.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).map(|p| p.kind), Some(SpanKind::Tool), "nests under its tool call");
    let inside = |x: &Span| {
        let mut at = x.parent_span_id.clone();
        while let Some(p) = at {
            if p == sub.span_id {
                return true;
            }
            at = s.spans.get(&p).and_then(|y| y.parent_span_id.clone());
        }
        false
    };
    assert!(kind(SpanKind::Llm).any(inside) && kind(SpanKind::Tool).any(inside), "the sub-agent's calls sit inside it");
    assert_eq!(sub.status, SpanStatus::Succeeded);
    for x in s.spans.values().filter(|x| x.span_id != root.span_id) {
        let end = x.ended_at_utc_ns.unwrap_or_else(|| panic!("{} ended", x.span_id));
        assert!(end >= x.started_at_utc_ns, "{} ends after it starts", x.span_id);
        let Some(p) = x.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).filter(|p| p.span_id != root.span_id) else { continue };
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{} starts within {}", x.span_id, p.span_id);
        if x.kind != SpanKind::Subagent {
            assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{} ends within {}", x.span_id, p.span_id);
        }
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert!(calls.windows(2).all(|w| w[0].1 <= w[1].0 + MS), "the main agent's model calls do not overlap: {calls:?}");
    s
}

#[test]
fn crush_conforms_without_per_call_usage() {
    conforms_without_call_usage(conformance(&dir()));
}

/// The session's running cost (sub-agents included) lands on the agent span, the only usage there is.
#[test]
fn session_cost_lands_on_the_agent_span() {
    let d = dir();
    conformance(&d);
    let s = conforms_without_call_usage(vec![(d.join("conf/.crush/crush.db"), "crush")]);
    let root = &s.spans[&SpanId::from("agent:cr")];
    let u = root.usage.as_ref().unwrap();
    assert_eq!((u.cost_usd_micros, u.cost_provenance, u.tokens_input), (Some(12_300), CostProvenance::Estimate, None));
    assert!(s.spans.values().filter(|x| x.span_id != root.span_id).all(|x| x.usage.is_none()));
    assert_eq!(s.totals().cost_usd_micros(), Some(12_300));
}

/// A call that runs tools is stamped finished only after they ran: it ends where the shell command
/// started, or where the sub-agent was created.
#[test]
fn a_call_ends_where_its_tools_start() {
    let d = dir();
    let s = conforms_without_call_usage(conformance(&d));
    let at = |id: &str| {
        let x = &s.spans[&SpanId::from(id)];
        ((x.started_at_utc_ns - T0_MS * MS) / MS, (x.ended_at_utc_ns.unwrap() - T0_MS * MS) / MS)
    };
    assert_eq!(at("llm:m4"), (3000, 4000));
    assert_eq!(at("tool:cr:c_bash"), (4000, 5000));
    assert_eq!(at("llm:m6"), (5000, 6000));
    assert_eq!(at("tool:cr:c_agent"), (6000, 10_000));
    assert_eq!(at(&format!("subagent:{SUB}")), (6000, 9000));
}
