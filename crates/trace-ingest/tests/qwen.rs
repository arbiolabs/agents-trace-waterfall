//! Qwen Code: the conformance scenario in its native format, and its quirks. Record shapes follow a live
//! run of Qwen Code 0.26.0 against a local model (`tests/fixtures/qwen-code/live.jsonl`, text redacted).

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

fn with(mut v: Value, extra: Value) -> Value {
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

fn base(uuid: &str, secs: f64, kind: &str) -> Value {
    json!({"uuid": uuid, "parentUuid": null, "sessionId": "qw", "timestamp": iso(secs), "type": kind, "cwd": "/work/conf", "version": "0.26.0"})
}

fn telemetry(uuid: &str, secs: f64, event: Value) -> Value {
    let event = with(event, json!({"event.timestamp": iso(secs)}));
    with(base(uuid, secs, "system"), json!({"subtype": "ui_telemetry", "systemPayload": {"uiEvent": event}}))
}

const MAIN: &str = "qw########0";
const SUB: &str = "qw#general-purpose-call_task#0";

/// Written as each model call completes, the main agent's and each sub-agent's alike.
fn api_response(uuid: &str, secs: f64, took: f64, prompt: &str) -> Value {
    let mut e = json!({"event.name": "qwen-code.api_response", "model": "qwen3:8b", "duration_ms": (took * 1000.) as i64, "input_token_count": 150, "output_token_count": 20, "cached_content_token_count": 50, "thoughts_token_count": 15, "total_token_count": 170, "response_text": "(answer)", "prompt_id": prompt});
    if prompt == SUB {
        e = with(e, json!({"subagent_name": "general-purpose", "subagent_id": "general-purpose-call_task", "subagent_type": "general-purpose"}));
    }
    telemetry(uuid, secs, e)
}

fn tool_call(uuid: &str, secs: f64, start: f64, id: &str, name: &str, ok: bool, prompt: &str) -> Value {
    let status = if ok { "success" } else { "error" };
    telemetry(uuid, secs, json!({"event.name": "qwen-code.tool_call", "call_id": id, "function_name": name, "function_args": {"pattern": "(input)"}, "duration_ms": ((secs - start) * 1000.) as i64, "started_at_ms": ms(start), "status": status, "success": ok, "prompt_id": prompt}))
}

fn assistant(uuid: &str, secs: f64, calls: &[(&str, &str)]) -> Value {
    let mut parts = vec![json!({"text": "(answer)"}), json!({"text": "(thinking)", "thought": true})];
    parts.extend(calls.iter().map(|(id, name)| json!({"functionCall": {"id": id, "name": name, "args": {}}})));
    let usage = json!({"promptTokenCount": 150, "candidatesTokenCount": 20, "thoughtsTokenCount": 15, "totalTokenCount": 170, "cachedContentTokenCount": 50});
    with(base(uuid, secs, "assistant"), json!({"model": "qwen3:8b", "message": {"role": "model", "parts": parts}, "usageMetadata": usage}))
}

fn result(uuid: &str, secs: f64, id: &str, name: &str, ok: bool) -> Value {
    let (response, status) = if ok { (json!({"output": "(output)"}), "success") } else { (json!({"error": "(output)"}), "error") };
    with(base(uuid, secs, "tool_result"), json!({"message": {"role": "user", "parts": [{"functionResponse": {"id": id, "name": name, "response": response}}]}, "toolCallResult": {"callId": id, "status": status}}))
}

fn qwen(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let main = vec![
        with(base("u1", 0., "user"), json!({"promptId": MAIN, "message": {"role": "user", "parts": [{"text": "(prompt)"}]}})),
        api_response("t1", 2., 2., MAIN),
        assistant("a1", 2., &[("call_read", "read_file")]),
        result("r1", 3., "call_read", "read_file", true),
        api_response("t2", 4., 1., MAIN),
        assistant("a2", 4., &[("call_bash", "run_shell_command")]),
        result("r2", 5., "call_bash", "run_shell_command", false),
        api_response("t3", 6., 1., MAIN),
        assistant("a3", 6., &[("call_task", "agent")]),
        api_response("t4", 8., 1.9, SUB),
        tool_call("t5", 8.5, 8., "call_grep", "grep_search", true, SUB),
        api_response("t6", 9.5, 1., SUB),
        tool_call("t7", 10., 6., "call_task", "agent", true, MAIN),
        with(result("r3", 10., "call_task", "agent", true), json!({"toolCallResult": {"callId": "call_task", "status": "success", "resultDisplay": {"type": "task_execution", "executionMode": "foreground", "status": "completed"}}})),
        api_response("t8", 11., 1., MAIN),
        assistant("a4", 11., &[]),
    ];
    // The sub-agent's own transcript: skipped, its spans come from the parent's telemetry.
    let side: Vec<_> = [
        with(base("su1", 6., "user"), json!({"message": {"role": "user", "parts": [{"text": "(task)"}]}})),
        with(base("sa1", 8., "assistant"), json!({"message": {"role": "model", "parts": [{"text": "(answer)"}]}, "usageMetadata": {"promptTokenCount": 150, "totalTokenCount": 170}})),
        with(base("sa1c", 8., "assistant"), json!({"message": {"role": "model", "parts": [{"functionCall": {"id": "call_grep", "name": "grep_search", "args": {}}}]}})),
        result("sr1", 8.5, "call_grep", "grep_search", true),
    ]
    .into_iter()
    .map(|v| with(v, json!({"agentId": "general-purpose-call_task", "agentName": "general-purpose", "isSidechain": true})))
    .collect();
    let (m, s) = (d.join("chats/qw.jsonl"), d.join("subagents/qw/agent-general-purpose-call_task.jsonl"));
    write_lines(&m, &main);
    write_lines(&s, &side);
    vec![(m, "qwen-code"), (s, "qwen-code")]
}

#[test]
fn qwen_code_conforms() {
    conforms("qwen-code", qwen(&dir()));
}

fn load(path: &Path) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, path, "qwen-code", &Capture::default()).unwrap();
    let id = store.list_sessions(&SessionFilter::default()).unwrap().remove(0).session_id;
    store.load_session(&id).unwrap()
}

#[test]
fn live_recording_reads_as_recorded() {
    let s = load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/qwen-code/live.jsonl")));
    let get = |id: &str| s.spans.get(&SpanId(id.into())).unwrap_or_else(|| panic!("{id}"));
    let shell = get("tool:call_9iteaf4h");
    assert_eq!((shell.name.as_str(), shell.status), ("run_shell_command", SpanStatus::Failed));
    assert_eq!(get("tool:call_pu8wiv2u").status, SpanStatus::Succeeded);
    let sub = get("subagent:call_zm7h3o5c");
    assert_eq!(sub.parent_span_id, Some(SpanId("tool:call_zm7h3o5c".into())));
    assert_eq!(sub.status, SpanStatus::Succeeded);
    let grep = get("tool:call_j3nx1wdl");
    assert_eq!((grep.parent_span_id.as_ref(), grep.started_at_utc_ns), (Some(&sub.span_id), 1_792_782_455_360 * MS));
    let sub_calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&sub.span_id)).collect();
    assert_eq!(sub_calls.len(), 2, "the sub-agent's two model calls");
    assert!(sub_calls.iter().all(|x| x.model.as_deref() == Some("qwen3:8b")));
    // The first main call: 236 694 ms long (its record lands 9 ms after the telemetry), 22 254 prompt tokens of which 13 828 cached, 22 984 in total.
    let first = get("llm:5aabff63-0cad-478b-9660-a92d7635a0b9");
    assert_eq!(first.ended_at_utc_ns.unwrap() - first.started_at_utc_ns, 236_694 * MS + 9 * MS);
    let u = first.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.tokens_output, u.cache_read), (Some(8_426), Some(730), Some(13_828)));
    let turns: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Turn).collect();
    assert_eq!((turns.len(), turns[0].status), (1, SpanStatus::Succeeded));
}

#[test]
fn bridge_calls_are_named_after_their_target() {
    let d = dir();
    let call = json!({"functionCall": {"id": "c1", "name": "tool_call", "args": {"name": "agent", "arguments": {}}}});
    let lines = [
        with(base("u1", 0., "user"), json!({"promptId": MAIN})),
        with(base("a1", 1., "assistant"), json!({"model": "m", "message": {"role": "model", "parts": [call]}})),
        result("r1", 2., "c1", "tool_call", true),
    ];
    write_lines(&d.join("s.jsonl"), &lines);
    let s = load(&d.join("s.jsonl"));
    assert_eq!(s.spans[&SpanId("tool:c1".into())].name, "agent");
    assert_eq!(s.spans[&SpanId("subagent:c1".into())].status, SpanStatus::Succeeded);
}

#[test]
fn background_sub_agents_outlive_their_launch() {
    let d = dir();
    let display = json!({"toolCallResult": {"callId": "c1", "status": "success", "resultDisplay": {"type": "task_execution", "executionMode": "background", "status": "background"}}});
    let lines = [
        with(base("u1", 0., "user"), json!({"promptId": MAIN})),
        assistant("a1", 1., &[("c1", "agent")]),
        with(result("r1", 2., "c1", "agent", true), display),
    ];
    write_lines(&d.join("s.jsonl"), &lines);
    let s = load(&d.join("s.jsonl"));
    assert_eq!(s.spans[&SpanId("tool:c1".into())].status, SpanStatus::Succeeded);
    assert_eq!(s.spans[&SpanId("subagent:c1".into())].status, SpanStatus::Running);
}
