//! Kimi CLI: the conformance scenario written the way kimi-cli 1.51 writes its share folder (checked
//! against live runs on a local model), and the format's quirks.

mod support;

use md5::{Digest, Md5};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

const CWD: &str = "/work/conf";

fn rec(secs: f64, kind: &str, payload: Value) -> Value {
    json!({"timestamp": ms(secs) as f64 / 1000., "message": {"type": kind, "payload": payload}})
}

fn usage() -> Value {
    json!({"token_usage": {"input_other": 100, "output": 20, "input_cache_read": 50, "input_cache_creation": 0}, "message_id": "chatcmpl-1", "context_tokens": 170})
}

fn call(secs: f64, id: &str, name: &str) -> Value {
    rec(secs, "ToolCall", json!({"type": "function", "id": id, "function": {"name": name, "arguments": "{}"}, "extras": null}))
}

fn result(secs: f64, id: &str, error: bool) -> Value {
    rec(secs, "ToolResult", json!({"tool_call_id": id, "return_value": {"is_error": error, "output": "", "message": "", "display": [], "extras": null}}))
}

/// A share folder: config with a default model, `kimi.json` naming the work dir, and one session.
fn share(d: &Path, sid: &str, main: Vec<Value>) -> PathBuf {
    std::fs::write(d.join("config.toml"), "default_model = \"k2\"\n\n[models.k2]\nprovider = \"moonshot\"\nmodel = \"kimi-k2-turbo-preview\"\nmax_context_size = 262144\n\n[models.small]\nprovider = \"moonshot\"\nmodel = \"kimi-k2-mini\"\nmax_context_size = 131072\n").unwrap();
    std::fs::write(d.join("kimi.json"), json!({"work_dirs": [{"path": CWD, "kaos": "local", "last_session_id": sid}]}).to_string()).unwrap();
    let dir = session_dir(d, sid);
    let mut lines = vec![json!({"type": "metadata", "protocol_version": "1.1"})];
    lines.extend(main);
    write_lines(&dir.join("wire.jsonl"), &lines);
    std::fs::create_dir_all(dir.join("subagents")).unwrap();
    d.to_owned()
}

fn session_dir(d: &Path, sid: &str) -> PathBuf {
    d.join("sessions").join(format!("{:x}", Md5::digest(CWD.as_bytes()))).join(sid)
}

fn subagent(d: &Path, sid: &str, agent: &str, meta: Value, lines: Vec<Value>) {
    let dir = session_dir(d, sid).join("subagents").join(agent);
    let mut all = vec![json!({"type": "metadata", "protocol_version": "1.1"})];
    all.extend(lines);
    write_lines(&dir.join("wire.jsonl"), &all);
    std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
}

fn conformance(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let sub_event = |secs: f64, kind: &str, payload: Value| rec(secs, "SubagentEvent", json!({"parent_tool_call_id": "c_agent", "agent_id": "a1", "subagent_type": "explore", "event": {"type": kind, "payload": payload}}));
    let main = vec![
        rec(0., "TurnBegin", json!({"user_input": "(prompt)"})),
        rec(0., "StepBegin", json!({"n": 1})),
        call(1.9, "c_read", "ReadFile"),
        rec(2., "StatusUpdate", usage()),
        result(3., "c_read", false),
        rec(3., "StepBegin", json!({"n": 2})),
        call(3.9, "c_shell", "Shell"),
        rec(4., "StatusUpdate", usage()),
        result(5., "c_shell", true),
        rec(5., "StepBegin", json!({"n": 3})),
        call(6., "c_agent", "Agent"),
        rec(6., "StatusUpdate", usage()),
        sub_event(6., "TurnBegin", json!({"user_input": "(task)"})),
        sub_event(9.5, "TurnEnd", json!({})),
        result(10., "c_agent", false),
        rec(10., "StepBegin", json!({"n": 4})),
        rec(11., "StatusUpdate", usage()),
        rec(11., "TurnEnd", json!({})),
    ];
    let s = share(d, "ks", main);
    subagent(d, "ks", "a1", json!({"agent_id": "a1", "subagent_type": "explore", "launch_spec": {"effective_model": null}}), vec![
        rec(6., "TurnBegin", json!({"user_input": "(task)"})),
        rec(6., "StepBegin", json!({"n": 1})),
        call(7.9, "s_grep", "Grep"),
        rec(8., "StatusUpdate", usage()),
        result(8.5, "s_grep", false),
        rec(8.5, "StepBegin", json!({"n": 2})),
        rec(9.5, "StatusUpdate", usage()),
        rec(9.5, "TurnEnd", json!({})),
    ]);
    vec![(s, "kimi")]
}

fn load(path: &Path) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, path, "kimi", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    store.load_session(&sessions[0].session_id).unwrap()
}

#[test]
fn kimi_conforms() {
    conforms("kimi", conformance(&dir()));
}

#[test]
fn names_the_project_and_the_configured_model_and_tokens() {
    let d = dir();
    conformance(&d);
    let s = load(&d);
    let llm = s.spans.values().find(|x| x.kind == SpanKind::Llm).unwrap();
    assert_eq!((llm.model.as_deref(), llm.provider.as_deref()), (Some("kimi-k2-turbo-preview"), Some("moonshot")));
    let u = llm.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.tokens_output, u.cache_read), (Some(100), Some(20), Some(50)));
    let shell = &s.spans[&SpanId::from("tool:ks:c_shell")];
    assert_eq!(shell.status, SpanStatus::Failed);
}

/// A background agent returns at once; its task's spec names the call that started it, and the agent's
/// own `meta.json` the model alias it runs on.
#[test]
fn background_agent_nests_under_its_call_through_the_task_spec() {
    let d = dir();
    share(&d, "bg", vec![
        rec(0., "TurnBegin", json!({"user_input": "(prompt)"})),
        rec(0., "StepBegin", json!({"n": 1})),
        call(1., "c_bg", "Agent"),
        rec(1., "StatusUpdate", usage()),
        result(1.1, "c_bg", false),
        rec(1.2, "StepBegin", json!({"n": 2})),
        rec(2., "StatusUpdate", usage()),
        rec(2., "TurnEnd", json!({})),
    ]);
    let task = session_dir(&d, "bg").join("tasks/agent-x");
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(task.join("spec.json"), json!({"kind": "agent", "tool_call_id": "c_bg", "kind_payload": {"agent_id": "b1", "launch_mode": "background"}}).to_string()).unwrap();
    subagent(&d, "bg", "b1", json!({"subagent_type": "coder", "launch_spec": {"effective_model": "small"}}), vec![
        rec(1.05, "TurnBegin", json!({"user_input": "(task)"})),
        rec(1.05, "StepBegin", json!({"n": 1})),
        rec(30., "StatusUpdate", usage()),
        rec(30., "TurnEnd", json!({})),
    ]);
    let s = load(&d);
    let sub = &s.spans[&SpanId::from("subagent:bg:b1")];
    assert_eq!(sub.parent_span_id, Some(SpanId::from("tool:bg:c_bg")));
    assert_eq!((sub.agent_name.as_deref(), sub.status), (Some("coder"), SpanStatus::Succeeded));
    let inner = s.spans.values().find(|x| x.kind == SpanKind::Llm && x.span_id.0.starts_with("llm:bg:b1:")).unwrap();
    assert_eq!(inner.model.as_deref(), Some("kimi-k2-mini"));
}

/// An interrupted step fails its call; the turn it broke has no `TurnEnd`, so the next prompt cancels it.
#[test]
fn interrupted_turn_is_cancelled_by_the_next_prompt() {
    let d = dir();
    share(&d, "ix", vec![
        rec(0., "TurnBegin", json!({"user_input": "(prompt)"})),
        rec(0., "StepBegin", json!({"n": 1})),
        rec(1., "StepInterrupted", json!({})),
        rec(5., "TurnBegin", json!({"user_input": "(again)"})),
        rec(5., "StepBegin", json!({"n": 1})),
        rec(6., "StatusUpdate", usage()),
        rec(6., "TurnEnd", json!({})),
    ]);
    let s = load(&d);
    let status = |id: &str| s.spans[&SpanId::from(id)].status;
    assert_eq!(status("llm:ix:3"), SpanStatus::Failed);
    assert_eq!(status("turn:ix:2"), SpanStatus::Cancelled);
    assert_eq!(status("turn:ix:5"), SpanStatus::Succeeded);
}
