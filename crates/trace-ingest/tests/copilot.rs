//! GitHub Copilot CLI `events.jsonl`: the conformance scenario, checked without per-call input tokens (the
//! log never records them; see `src/copilot.rs`), and the format's quirks.

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

fn event(n: &mut usize, secs: f64, kind: &str, agent: Option<&str>, data: Value) -> Value {
    *n += 1;
    let mut v = json!({"type": kind, "id": format!("e{n}"), "parentId": (*n > 1).then(|| format!("e{}", *n - 1)), "timestamp": iso(secs), "data": data});
    if let Some(a) = agent {
        v["agentId"] = json!(a);
    }
    v
}

/// The scenario as Copilot CLI writes it: each model call is its own `assistant.turn_start`/`turn_end`
/// pair under one `user.message`; the sub-agent's messages and tools carry its `agentId`, and its turn
/// starts are not written. The failing shell command reports a non-zero `shellExecution.exitCode`.
fn scenario(d: &Path) -> PathBuf {
    let n = &mut 0;
    let msg = |id: &str, calls: &[(&str, &str)]| {
        let reqs: Vec<_> = calls.iter().map(|(c, name)| json!({"toolCallId": c, "name": name, "type": "function"})).collect();
        json!({"messageId": id, "model": "claude-sonnet-4.5", "content": "(text)", "outputTokens": 20, "toolRequests": reqs})
    };
    let start = |c: &str, name: &str| json!({"toolCallId": c, "toolName": name, "arguments": {"command": "(input)"}});
    let done = |c: &str| json!({"toolCallId": c, "success": true, "result": {"content": "(output)"}});
    let sub = Some("c_task");
    let lines = vec![
        event(n, 0., "session.start", None, json!({"sessionId": "cp", "version": 1, "producer": "copilot-agent", "copilotVersion": "1.0.63", "startTime": iso(0.), "context": {"cwd": "/work/conf"}})),
        event(n, 0., "user.message", None, json!({"content": "(prompt)", "interactionId": "i1"})),
        event(n, 0., "assistant.turn_start", None, json!({"turnId": "0", "interactionId": "i1"})),
        event(n, 2., "assistant.message", None, msg("m1", &[("c_read", "view")])),
        event(n, 2., "tool.execution_start", None, start("c_read", "view")),
        event(n, 3., "tool.execution_complete", None, done("c_read")),
        event(n, 3., "assistant.turn_end", None, json!({"turnId": "0"})),
        event(n, 3., "assistant.turn_start", None, json!({"turnId": "1", "interactionId": "i1"})),
        event(n, 4., "assistant.message", None, msg("m2", &[("c_bash", "bash")])),
        event(n, 4., "tool.execution_start", None, start("c_bash", "bash")),
        event(n, 5., "tool.execution_complete", None, json!({"toolCallId": "c_bash", "success": true, "shellExecution": {"exitCode": 1}})),
        event(n, 5., "assistant.turn_end", None, json!({"turnId": "1"})),
        event(n, 5., "assistant.turn_start", None, json!({"turnId": "2", "interactionId": "i1"})),
        event(n, 6., "assistant.message", None, msg("m3", &[("c_task", "task")])),
        event(n, 6., "tool.execution_start", None, start("c_task", "task")),
        event(n, 6., "subagent.started", sub, json!({"toolCallId": "c_task", "agentName": "helper", "agentDisplayName": "Helper", "agentDescription": "(text)"})),
        event(n, 8., "assistant.message", sub, msg("m4", &[("c_grep", "grep")])),
        event(n, 8., "tool.execution_start", sub, start("c_grep", "grep")),
        event(n, 8.5, "tool.execution_complete", sub, done("c_grep")),
        event(n, 9.5, "assistant.message", sub, msg("m5", &[])),
        event(n, 9.6, "subagent.completed", sub, json!({"toolCallId": "c_task", "agentName": "helper", "agentDisplayName": "Helper"})),
        event(n, 10., "tool.execution_complete", None, done("c_task")),
        event(n, 10., "assistant.turn_end", None, json!({"turnId": "2"})),
        event(n, 10., "assistant.turn_start", None, json!({"turnId": "3", "interactionId": "i1"})),
        event(n, 11., "assistant.message", None, msg("m6", &[])),
        event(n, 11., "assistant.turn_end", None, json!({"turnId": "3"})),
    ];
    let p = d.join("session-state/cp/events.jsonl");
    write_lines(&p, &lines);
    p
}

fn load(p: &Path) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, p, "copilot", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1);
    store.load_session(&sessions[0].session_id).unwrap()
}

fn spans(s: &TraceState, k: SpanKind) -> Vec<&Span> {
    s.spans.values().filter(|x| x.kind == k).collect()
}

/// `support::conforms` minus one assertion: Copilot writes no input tokens per model call, so an LLM span
/// carries its model and output tokens only. Everything else the shared check asserts holds here.
#[test]
fn copilot_conforms_except_input_tokens() {
    let p = scenario(&dir());
    let s = load(&p);
    assert_eq!(s.session.project_id, "conf");
    assert!(s.unresolved().is_empty(), "every span's parent arrived: {:?}", s.unresolved());
    let root = &spans(&s, SpanKind::Agent)[..];
    assert_eq!(root.len(), 1);
    let turns = spans(&s, SpanKind::Turn);
    assert_eq!(turns.len(), 1);
    let turn = turns[0];
    assert_eq!((turn.status, turn.ended_at_utc_ns), (SpanStatus::Succeeded, Some(ms(11.) * 1_000_000)));

    let under = |p: &Span, k: SpanKind| s.spans.values().filter(|x| x.kind == k && x.parent_span_id.as_ref() == Some(&p.span_id)).count();
    assert_eq!(under(turn, SpanKind::Llm), 4);
    for llm in spans(&s, SpanKind::Llm) {
        assert_eq!(llm.model.as_deref(), Some("claude-sonnet-4.5"));
        let u = llm.usage.as_ref().unwrap();
        assert_eq!((u.tokens_input, u.tokens_output), (None, Some(20)), "{}", llm.span_id);
    }
    let failed: Vec<_> = spans(&s, SpanKind::Tool).into_iter().filter(|t| t.status == SpanStatus::Failed).map(|t| t.span_id.0.as_str()).collect();
    assert_eq!(failed, ["tool:c_bash"]);

    let subs = spans(&s, SpanKind::Subagent);
    assert_eq!(subs.len(), 1);
    let sub = subs[0];
    assert_eq!((sub.parent_span_id.as_ref().map(|p| p.0.as_str()), sub.status), (Some("tool:c_task"), SpanStatus::Succeeded));
    assert_eq!((under(sub, SpanKind::Llm), under(sub, SpanKind::Tool)), (2, 1));

    for x in s.spans.values().filter(|x| x.span_id != root[0].span_id) {
        let end = x.ended_at_utc_ns.unwrap_or_else(|| panic!("{} ended", x.span_id));
        assert!(end >= x.started_at_utc_ns, "{} ends after it starts", x.span_id);
        let Some(p) = x.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).filter(|p| p.span_id != root[0].span_id) else { continue };
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{} starts within {}", x.span_id, p.span_id);
        assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{} ends within {}", x.span_id, p.span_id);
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert_eq!(calls.iter().map(|(a, b)| ((a / MS - T0_MS) as f64 / 1000., (b / MS - T0_MS) as f64 / 1000.)).collect::<Vec<_>>(), [(0., 2.), (3., 4.), (5., 6.), (10., 11.)]);
}

/// Pins the deviation: the shared check fails on input tokens and nothing earlier.
#[test]
fn shared_check_fails_only_for_missing_input_tokens() {
    let p = scenario(&dir());
    let err = std::panic::catch_unwind(|| conforms("copilot", vec![(p, "copilot")])).unwrap_err();
    let msg = err.downcast_ref::<String>().unwrap();
    assert!(msg.contains("has token counts"), "{msg}");
}

#[test]
fn rereading_is_idempotent_and_copies_no_text() {
    let p = scenario(&dir());
    let mut store = Store::open_in_memory().unwrap();
    let first = import_file(&mut store, &p, "copilot", &Capture::default()).unwrap();
    let again = import_file(&mut store, &p, "copilot", &Capture::default()).unwrap();
    assert_eq!((again.inserted, again.duplicates), (0, first.inserted));
    let a = trace_ingest::adapter_by_name("copilot").unwrap();
    let text: String = std::fs::read_to_string(&p).unwrap().lines().flat_map(|l| a.normalize(trace_adapter_sdk::RawEvent { line: l.into(), observed_at_utc_ns: 0 }).unwrap()).map(|e| serde_json::to_string(&e).unwrap()).collect();
    for secret in ["(prompt)", "(text)", "(input)", "(output)"] {
        assert!(!text.contains(secret), "{secret} leaked");
    }
}

/// Newer builds: no `outputTokens`, a sub-agent `agentId` distinct from its spawning call, and failures
/// reported as `success: false` or in a terminal block's exit code.
#[test]
fn newer_builds_and_failure_shapes() {
    let n = &mut 0;
    let a = Some("agent-7");
    let lines = vec![
        event(n, 0., "session.start", None, json!({"sessionId": "cp2", "copilotVersion": "1.0.103", "selectedModel": "gpt-5-mini", "context": {"cwd": "/work/conf"}})),
        event(n, 0., "user.message", None, json!({"interactionId": "i1"})),
        event(n, 0., "assistant.turn_start", None, json!({"turnId": "0"})),
        event(n, 1., "assistant.message", None, json!({"messageId": "m1"})),
        event(n, 1., "tool.execution_start", None, json!({"toolCallId": "c_view", "toolName": "view"})),
        event(n, 2., "tool.execution_complete", None, json!({"toolCallId": "c_view", "success": false, "error": {"message": "(text)"}})),
        event(n, 2., "tool.execution_start", None, json!({"toolCallId": "c_sh", "toolName": "bash"})),
        event(n, 3., "tool.execution_complete", None, json!({"toolCallId": "c_sh", "success": true, "result": {"contents": [{"type": "terminal", "text": "(output)", "exitCode": 2}]}})),
        event(n, 3., "tool.execution_start", None, json!({"toolCallId": "c_task", "toolName": "task"})),
        event(n, 3., "subagent.started", a, json!({"toolCallId": "c_task", "agentName": "explore"})),
        event(n, 4., "assistant.message", a, json!({"messageId": "m2", "model": "claude-haiku-4.5"})),
        event(n, 5., "subagent.failed", a, json!({"toolCallId": "c_task", "agentName": "explore", "error": "(text)"})),
        event(n, 5., "tool.execution_complete", None, json!({"toolCallId": "c_task", "success": true})),
        event(n, 6., "assistant.turn_end", None, json!({"turnId": "0"})),
    ];
    let p = dir().join("events.jsonl");
    write_lines(&p, &lines);
    let s = load(&p);
    let get = |id: &str| &s.spans[&SpanId(id.into())];
    assert_eq!((get("llm:m1").model.as_deref(), get("llm:m1").usage.is_none()), (Some("gpt-5-mini"), true));
    assert_eq!(get("llm:m2").parent_span_id, Some(SpanId("subagent:c_task".into())));
    assert_eq!(get("llm:m2").started_at_utc_ns, ms(3.) * 1_000_000);
    let status = |id: &str| get(id).status;
    assert_eq!([status("tool:c_view"), status("tool:c_sh"), status("tool:c_task"), status("subagent:c_task")], [SpanStatus::Failed, SpanStatus::Failed, SpanStatus::Succeeded, SpanStatus::Failed]);
}
