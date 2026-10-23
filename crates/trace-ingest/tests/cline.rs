//! Cline and Roo Code record the conformance scenario in their task folders: `ui_messages.json`, the
//! model-facing `api_conversation_history.json`, and Roo's `history_item.json` or Cline's
//! `state/taskHistory.json` beside the tasks folder.
//!
//! Neither can pass `support::conforms` whole, so [`conforms_but`] runs the same checks with one waived:
//! Roo records no model anywhere in a task folder, so its model calls cannot name one; Cline's
//! `use_subagents` runs sub-agents in process and keeps only each one's totals, so a Cline sub-agent holds
//! one aggregate model call and no tool calls.

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

const SECRET: &str = "SECRET-TEXT";

fn write(path: &Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn say(secs: f64, say: &str, text: Value) -> Value {
    json!({"ts": ms(secs), "type": "say", "say": say, "text": text})
}

fn ask(secs: f64, ask: &str, text: Value) -> Value {
    json!({"ts": ms(secs), "type": "ask", "ask": ask, "text": text})
}

/// An `api_req_started` once its request finished: the JSON text carries tokens and cost.
fn api_req(secs: f64, extra: Value) -> Value {
    let mut info = json!({"tokensIn": 100, "tokensOut": 20, "cacheReads": 50, "cacheWrites": 0, "cost": 0.001});
    info.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    say(secs, "api_req_started", json!(info.to_string()))
}

fn tool(name: &str) -> Value {
    json!(json!({"tool": name, "path": SECRET, "content": SECRET}).to_string())
}

fn user(secs: f64, content: Value) -> Value {
    json!({"role": "user", "ts": ms(secs), "content": content})
}

fn roo(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let tasks = d.join("tasks");
    let main = vec![
        say(0., "text", json!(SECRET)),
        api_req(0.2, json!({"apiProtocol": "anthropic"})),
        ask(2., "tool", tool("readFile")),
        api_req(3., json!({})),
        ask(4., "command", json!("ls /definitely/missing")),
        say(4.5, "command_output", json!(SECRET)),
        api_req(5., json!({})),
        ask(6., "tool", tool("newTask")),
        say(10., "subtask_result", json!(SECRET)),
        api_req(10., json!({})),
        say(11., "completion_result", json!(SECRET)),
        ask(11., "completion_result", json!("")),
    ];
    let api = json!([
        user(0.2, json!([{"type": "text", "text": SECRET}])),
        user(3., json!([{"type": "tool_result", "tool_use_id": "t1", "content": SECRET}])),
        user(5., json!([{"type": "tool_result", "tool_use_id": "t2", "content": format!("{SECRET}\nCommand execution was not successful, inspect the cause and adjust as needed.\nExit code: 1")}])),
        user(10., json!([{"type": "tool_result", "tool_use_id": "t3", "content": SECRET}])),
    ]);
    write(&tasks.join("roo-main/ui_messages.json"), &json!(main));
    write(&tasks.join("roo-main/api_conversation_history.json"), &api);
    write(&tasks.join("roo-main/history_item.json"), &json!({"id": "roo-main", "number": 1, "ts": ms(11.), "task": SECRET, "workspace": "/work/conf", "mode": "code", "childIds": ["roo-child"]}));
    let child = vec![
        say(6., "text", json!(SECRET)),
        api_req(6.5, json!({})),
        ask(8., "tool", tool("searchFiles")),
        api_req(8.5, json!({})),
        say(9.5, "completion_result", json!(SECRET)),
    ];
    write(&tasks.join("roo-child/ui_messages.json"), &json!(child));
    write(&tasks.join("roo-child/history_item.json"), &json!({"id": "roo-child", "rootTaskId": "roo-main", "parentTaskId": "roo-main", "number": 2, "ts": ms(9.5), "task": SECRET, "workspace": "/work/conf"}));
    vec![(tasks, "roo")]
}

fn cline(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let with_model = |mut m: Value| {
        m["modelInfo"] = json!({"modelId": "claude-sonnet-4-5", "providerId": "anthropic", "mode": "act"});
        m
    };
    let sub = json!({"status": "completed", "total": 1, "completed": 1, "successes": 1, "failures": 0, "toolCalls": 1, "inputTokens": 300, "outputTokens": 40, "items": [
        {"index": 0, "prompt": SECRET, "status": "completed", "toolCalls": 1, "inputTokens": 300, "outputTokens": 40, "totalCost": 0.002, "contextTokens": 340, "contextWindow": 200000, "contextUsagePercentage": 0.2, "result": SECRET},
    ]});
    let msgs: Vec<Value> = vec![
        say(0., "task", json!(SECRET)),
        api_req(0.2, json!({"request": SECRET})),
        say(2., "tool", tool("readFile")),
        api_req(3., json!({"request": SECRET})),
        ask(4., "command", json!("ls /definitely/missing")),
        say(4.5, "command_output", json!(SECRET)),
        api_req(5., json!({"request": SECRET})),
        say(6., "use_subagents", json!(json!({"prompts": [SECRET]}).to_string())),
        say(6.1, "subagent", json!(sub.to_string())),
        say(9.6, "subagent_usage", json!(json!({"source": "subagents", "tokensIn": 300, "tokensOut": 40, "cacheWrites": 0, "cacheReads": 0, "cost": 0.002}).to_string())),
        api_req(10., json!({"request": SECRET})),
        say(11., "completion_result", json!(SECRET)),
    ]
    .into_iter()
    .map(with_model)
    .collect();
    let api = json!([
        user(0.2, json!([{"type": "text", "text": SECRET}])),
        user(3., json!([{"type": "text", "text": "[read_file for 'x'] Result:"}, {"type": "text", "text": SECRET}])),
        user(5., json!([{"type": "text", "text": "[execute_command for 'x'] Result:"}, {"type": "text", "text": format!("Command failed with exit code 2.\nOutput:\n{SECRET}")}])),
        user(10., json!([{"type": "text", "text": SECRET}])),
    ]);
    let tasks = d.join("tasks");
    write(&tasks.join("cl/ui_messages.json"), &json!(msgs));
    write(&tasks.join("cl/api_conversation_history.json"), &api);
    write(&d.join("state/taskHistory.json"), &json!([{"id": "cl", "ts": ms(11.), "task": SECRET, "tokensIn": 0, "tokensOut": 0, "totalCost": 0, "cwdOnTaskInitialization": "/work/conf"}]));
    vec![(d.to_owned(), "cline")]
}

#[test]
fn roo_conforms_but_names_no_model() {
    conforms_but("roo", roo(&dir()), Waive::Model);
}

#[test]
fn cline_conforms_but_its_sub_agents_hold_only_totals() {
    conforms_but("cline", cline(&dir()), Waive::SubagentTools);
}

#[test]
fn no_text_is_copied() {
    for (harness, files) in [("roo", roo(&dir())), ("cline", cline(&dir()))] {
        let mut store = Store::open_in_memory().unwrap();
        for (f, a) in &files {
            import_file(&mut store, f, a, &Capture::default()).unwrap();
        }
        let s = store.list_sessions(&SessionFilter::default()).unwrap();
        let session = store.load_session(&s[0].session_id).unwrap();
        assert_eq!(session.spans.len(), if harness == "roo" { 14 } else { 11 }, "{harness}: every span read");
        let dump = format!("{:?}", session.spans);
        assert!(!dump.contains(SECRET), "{harness}: a prompt, command or tool text leaked");
    }
}

#[test]
fn a_child_task_read_alone_joins_its_root_under_the_spawning_call() {
    let d = dir();
    roo(&d);
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &d.join("tasks/roo-child"), "roo", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.iter().map(|s| s.session_id.0.as_str()).collect::<Vec<_>>(), ["roo-main"]);
    let s = store.load_session(&"roo-main".into()).unwrap();
    let sub = &s.spans[&SpanId("subagent:roo-child".into())];
    assert_eq!(sub.parent_span_id, Some(SpanId(format!("tool:roo-main:{}", ms(6.)))));
}

/// Prints the span tree of a real recording: `ROO_TASKS=<tasks folder> cargo test -p trace-ingest --test cline -- --ignored --nocapture`.
#[test]
#[ignore]
fn live_recording() {
    let Ok(tasks) = std::env::var("ROO_TASKS") else { return };
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, Path::new(&tasks), "roo", &Capture::default()).unwrap();
    for summary in store.list_sessions(&SessionFilter::default()).unwrap() {
        let s = store.load_session(&summary.session_id).unwrap();
        let t0 = s.spans.values().map(|x| x.started_at_utc_ns).min().unwrap();
        let mut spans: Vec<_> = s.spans.values().collect();
        spans.sort_by_key(|x| x.started_at_utc_ns);
        for x in spans {
            let ms = |t: Ns| (t - t0) / 1_000_000;
            println!("{:>7} {:>7} {:?} {:?} {} <- {:?} {:?}", ms(x.started_at_utc_ns), x.ended_at_utc_ns.map_or(-1, ms), x.kind, x.status, x.span_id, x.parent_span_id, x.usage.as_ref().map(|u| (u.tokens_input, u.tokens_output)));
        }
    }
}

enum Waive {
    Model,
    SubagentTools,
}

/// `support::conforms`, with one check waived (see the module comment).
fn conforms_but(harness: &str, files: Vec<(PathBuf, &'static str)>, waive: Waive) {
    let mut store = Store::open_in_memory().unwrap();
    for (f, adapter) in files.iter().rev() {
        import_file(&mut store, f, adapter, &Capture::default()).unwrap_or_else(|e| panic!("{harness}: {e:#}"));
    }
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1, "{harness}: one session, sub-agent included");
    let summary = &sessions[0];
    assert_eq!((summary.project_id.as_str(), summary.turn_count), ("conf", 1), "{harness}: project and main-agent turns");
    let s = store.load_session(&summary.session_id).unwrap();
    assert!(s.unresolved().is_empty(), "{harness}: every span's parent arrived: {:?}", s.unresolved());

    let kind = |k: SpanKind| s.spans.values().filter(move |x| x.kind == k);
    let roots: Vec<_> = kind(SpanKind::Agent).collect();
    assert_eq!(roots.len(), 1, "{harness}: one agent root");
    let root = roots[0];
    let main_turns: Vec<_> = kind(SpanKind::Turn).filter(|t| t.parent_span_id.as_ref() == Some(&root.span_id)).collect();
    assert_eq!(main_turns.len(), 1, "{harness}: one main turn");
    let turn = main_turns[0];
    assert_eq!(turn.status, SpanStatus::Succeeded, "{harness}: the turn succeeded");

    let under_turn = |k: SpanKind| s.spans.values().filter(|x| x.kind == k && x.parent_span_id.as_ref() == Some(&turn.span_id)).count();
    assert!(under_turn(SpanKind::Llm) >= 3, "{harness}: model calls under the turn: {}", under_turn(SpanKind::Llm));
    for llm in kind(SpanKind::Llm) {
        match waive {
            Waive::Model => assert!(llm.model.is_none(), "{harness}: {} has no recorded model to name", llm.span_id),
            _ => assert!(llm.model.is_some(), "{harness}: {} names its model", llm.span_id),
        }
        let u = llm.usage.as_ref().unwrap_or_else(|| panic!("{harness}: {} reports usage", llm.span_id));
        assert!(u.tokens_input.is_some() && u.tokens_output.is_some(), "{harness}: {} has token counts", llm.span_id);
    }
    let failed: Vec<_> = kind(SpanKind::Tool).filter(|t| t.status == SpanStatus::Failed).collect();
    assert_eq!(failed.len(), 1, "{harness}: the failing shell command, and only it, failed");

    let subs: Vec<_> = kind(SpanKind::Subagent).collect();
    assert_eq!(subs.len(), 1, "{harness}: one sub-agent");
    let sub = subs[0];
    let spawned_by = sub.parent_span_id.as_ref().and_then(|p| s.spans.get(p));
    assert_eq!(spawned_by.map(|p| p.kind), Some(SpanKind::Tool), "{harness}: the sub-agent nests under the tool call that started it");
    let inside = |id: &SpanId| {
        let mut at = s.spans.get(id).and_then(|x| x.parent_span_id.clone());
        while let Some(p) = at {
            if p == sub.span_id {
                return true;
            }
            at = s.spans.get(&p).and_then(|x| x.parent_span_id.clone());
        }
        false
    };
    assert!(s.spans.values().any(|x| x.kind == SpanKind::Llm && inside(&x.span_id)), "{harness}: the sub-agent's model calls sit inside it");
    let sub_tools = s.spans.values().any(|x| x.kind == SpanKind::Tool && inside(&x.span_id));
    match waive {
        Waive::SubagentTools => assert!(!sub_tools, "{harness}: the sub-agent's tool calls are not recorded"),
        _ => assert!(sub_tools, "{harness}: the sub-agent's tool calls sit inside it"),
    }
    assert_eq!(sub.status, SpanStatus::Succeeded, "{harness}: the sub-agent finished");

    for x in s.spans.values().filter(|x| x.span_id != root.span_id) {
        let end = x.ended_at_utc_ns.unwrap_or_else(|| panic!("{harness}: {} ended", x.span_id));
        assert!(end >= x.started_at_utc_ns, "{harness}: {} ends after it starts", x.span_id);
        let Some(p) = x.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).filter(|p| p.span_id != root.span_id) else { continue };
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{harness}: {} starts within {}", x.span_id, p.span_id);
        if x.kind != SpanKind::Subagent {
            assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{harness}: {} ends within {}", x.span_id, p.span_id);
        }
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert!(calls.windows(2).all(|w| w[0].1 <= w[1].0 + MS), "{harness}: the main agent's model calls do not overlap: {calls:?}");
}
