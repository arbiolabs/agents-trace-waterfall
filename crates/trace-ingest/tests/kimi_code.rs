//! Kimi Code CLI: the conformance scenario written the way Kimi Code 2.3 writes its data folder (wire
//! protocol 1.5, checked against a live run on a local model), and the format's quirks.

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

fn rec(secs: f64, kind: &str, agent: &str, fields: Value) -> Value {
    let mut v = json!({"type": kind, "agentId": agent, "time": ms(secs)});
    v.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    v
}

fn loop_event(secs: f64, agent: &str, event: Value) -> Value {
    rec(secs, "context.append_loop_event", agent, json!({"event": event}))
}

/// A model call from `start` whose stream ends at `streamed`, its step ending (after its tools) at `end`.
fn step(agent: &str, uuid: &str, start: f64, streamed: f64, end: f64, tools: Vec<Value>, finish: &str) -> Vec<Value> {
    let mut out = vec![
        loop_event(start, agent, json!({"type": "step.begin", "uuid": uuid, "turnId": "0", "step": 1})),
        rec(start, "llm.request", agent, json!({"kind": "loop", "provider": "kimi", "model": "kimi-k2-turbo-preview", "modelAlias": "k2", "toolSelect": false, "systemPromptHash": "h", "toolsHash": "h", "messageCount": 3, "turnStep": "0.1"})),
    ];
    out.extend(tools);
    let latency = ((streamed - start) * 1000.).round() as i64;
    out.push(loop_event(end, agent, json!({"type": "step.end", "uuid": uuid, "turnId": "0", "step": 1, "finishReason": finish, "usage": {"inputOther": 100, "output": 20, "inputCacheRead": 50, "inputCacheCreation": 0}, "llmFirstTokenLatencyMs": latency / 2, "llmStreamDurationMs": latency - latency / 2})));
    out
}

fn call(secs: f64, agent: &str, id: &str, name: &str) -> Value {
    loop_event(secs, agent, json!({"type": "tool.call", "uuid": format!("u-{id}"), "toolCallId": id, "name": name, "args": {}}))
}

fn result(secs: f64, agent: &str, id: &str, error: bool) -> Value {
    let mut r = json!({"output": "(output)", "durationMs": 1});
    if error {
        r["isError"] = json!(true);
    }
    loop_event(secs, agent, json!({"type": "tool.result", "parentUuid": format!("u-{id}"), "toolCallId": id, "result": r}))
}

fn turn(secs: f64, agent: &str) -> Value {
    rec(secs, "turn.prompt", agent, json!({"input": [{"type": "text", "text": "(prompt)"}], "origin": {"kind": "user"}, "turnId": 0}))
}

fn ended(secs: f64, agent: &str, reason: &str) -> Value {
    rec(secs, "turn.ended", agent, json!({"turnId": 0, "reason": reason}))
}

/// `<dir>/sessions/wd_conf_x/<sid>` with `state.json` and one wire file per agent.
fn session(d: &Path, sid: &str, agents: Vec<(&str, Vec<Value>)>) -> PathBuf {
    let dir = d.join("sessions/wd_conf_0123456789ab").join(sid);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), json!({"id": sid, "version": 2, "cwd": "/work/conf", "createdAt": ms(0.)}).to_string()).unwrap();
    for (agent, records) in agents {
        let mut lines = vec![json!({"type": "metadata", "protocol_version": "1.5", "created_at": ms(0.)})];
        lines.extend(records);
        write_lines(&dir.join("agents").join(agent).join("wire.jsonl"), &lines);
    }
    d.to_owned()
}

fn conformance(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let m = "main";
    let mut main = vec![turn(0., m)];
    main.extend(step(m, "s1", 0.5, 2., 3., vec![call(2., m, "c_read", "Read"), result(3., m, "c_read", false)], "tool_use"));
    main.extend(step(m, "s2", 3., 4., 5., vec![call(4., m, "c_bash", "Bash"), result(5., m, "c_bash", true)], "tool_use"));
    main.extend(step(m, "s3", 5., 6., 10., vec![
        call(6., m, "c_agent", "Agent"),
        rec(6., "subagent.spawned", m, json!({"subagentId": "agent-0", "subagentName": "explore", "parentToolCallId": "c_agent", "parentAgentId": "main", "runInBackground": false, "model": "k2"})),
        rec(6., "subagent.started", m, json!({"subagentId": "agent-0"})),
        rec(9.5, "subagent.completed", m, json!({"subagentId": "agent-0", "resultSummary": "(summary)"})),
        result(10., m, "c_agent", false),
    ], "tool_use"));
    main.extend(step(m, "s4", 10., 11., 11., vec![], "end_turn"));
    main.push(ended(11., m, "completed"));
    let a = "agent-0";
    let mut sub = vec![turn(6., a)];
    sub.extend(step(a, "h1", 6.5, 8., 8.5, vec![call(8., a, "c_grep", "Grep"), result(8.5, a, "c_grep", false)], "tool_use"));
    sub.extend(step(a, "h2", 8.5, 9.5, 9.5, vec![], "end_turn"));
    sub.push(ended(9.5, a, "completed"));
    vec![(session(d, "session_kc", vec![("main", main), ("agent-0", sub)]), "kimi-code")]
}

fn load(path: &Path) -> TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, path, "kimi-code", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    store.load_session(&sessions[0].session_id).unwrap()
}

fn secs_of(s: &TraceState, id: &str) -> (i64, Option<i64>) {
    let x = &s.spans[&SpanId::from(id)];
    let rel = |t: Ns| (t - T0_MS * MS) / MS;
    (rel(x.started_at_utc_ns), x.ended_at_utc_ns.map(rel))
}

#[test]
fn kimi_code_conforms() {
    conforms("kimi-code", conformance(&dir()));
}

/// A step's `step.end` comes after its tools ran; the model call itself ends when its stream did.
#[test]
fn model_call_ends_with_its_stream_and_names_its_model() {
    let d = dir();
    conformance(&d);
    let s = load(&d);
    assert_eq!(secs_of(&s, "llm:s3"), (5000, Some(6000)));
    let llm = &s.spans[&SpanId::from("llm:s3")];
    assert_eq!((llm.model.as_deref(), llm.provider.as_deref()), (Some("kimi-k2-turbo-preview"), Some("kimi")));
    assert_eq!(llm.usage.as_ref().map(|u| (u.tokens_input, u.cache_read)), Some((Some(100), Some(50))));
    assert_eq!(s.spans[&SpanId::from("subagent:session_kc:agent-0")].agent_name.as_deref(), Some("explore"));
}

/// A background agent returns at once and outlives its call; a cancelled sub-agent and an interrupted
/// step say so.
#[test]
fn background_agent_outlives_its_call_and_cancellation_shows() {
    let d = dir();
    let m = "main";
    let mut main = vec![turn(0., m)];
    main.extend(step(m, "b1", 0., 1., 1.2, vec![
        call(1., m, "c_bg", "Agent"),
        rec(1.1, "subagent.spawned", m, json!({"subagentId": "agent-0", "subagentName": "coder", "parentToolCallId": "c_bg", "runInBackground": true})),
        result(1.2, m, "c_bg", false),
    ], "tool_use"));
    main.extend(step(m, "b2", 1.2, 2., 2., vec![], "interrupted"));
    main.push(ended(2., m, "cancelled"));
    main.push(rec(30., "subagent.cancelled", m, json!({"subagentId": "agent-0"})));
    let a = "agent-0";
    let mut sub = vec![turn(1.1, a)];
    sub.extend(step(a, "x1", 1.1, 20., 20., vec![], "end_turn"));
    session(&d, "session_bg", vec![("main", main), ("agent-0", sub)]);
    let s = load(&d);
    assert_eq!(secs_of(&s, "subagent:session_bg:agent-0"), (1100, Some(30_000)));
    let sub = &s.spans[&SpanId::from("subagent:session_bg:agent-0")];
    assert_eq!((sub.parent_span_id.as_ref().map(|p| p.0.as_str()), sub.status), (Some("tool:session_bg:c_bg"), SpanStatus::Cancelled));
    assert_eq!(s.spans[&SpanId::from("llm:b2")].status, SpanStatus::Cancelled);
    assert_eq!(s.spans[&SpanId::from("turn:session_bg:main:2")].status, SpanStatus::Cancelled);
}
