//! Grok Build: the conformance scenario as Grok records it, and two live recordings (Grok 1.0.67 against
//! a local qwen3:8b), their text redacted. Grok writes token usage per turn, so the scenario is checked with
//! [`per_turn_usage`]: model calls carry no usage, the turns do.

mod per_turn_usage;
mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// One `updates.jsonl` line: `update` at `secs`, from the model call whose stream started at `stream`.
fn line(session: &str, n: usize, secs: f64, stream: Option<f64>, update: Value) -> Value {
    let mut meta = json!({"eventId": format!("{session}-{n}"), "agentTimestampMs": ms(secs)});
    if let Some(s) = stream {
        meta["streamStartMs"] = json!(ms(s));
    }
    let method = if update["sessionUpdate"].as_str().is_some_and(|k| k.starts_with("subagent") || k == "turn_completed") { "_x.ai/session/update" } else { "session/update" };
    json!({"timestamp": ms(secs) / 1000, "method": method, "params": {"sessionId": session, "update": update, "_meta": meta}})
}

fn write_session(sessions: &Path, id: &str, secs: f64, kind: &str, updates: Vec<(f64, Option<f64>, Value)>) -> PathBuf {
    let d = sessions.join("%2Fwork%2Fconf").join(id);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("summary.json"), json!({"info": {"id": id, "cwd": "/work/conf"}, "created_at": iso(secs), "session_kind": kind, "agent_name": if kind == "subagent" { "general-purpose" } else { "grok-build" }}).to_string()).unwrap();
    let lines: Vec<Value> = updates.into_iter().enumerate().map(|(n, (secs, stream, u))| line(id, n + 1, secs, stream, u)).collect();
    write_lines(&d.join("updates.jsonl"), &lines);
    d
}

fn grok(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let sessions = d.join("sessions");
    let prompt = || json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "(prompt)"}, "_meta": {"modelId": "grok-4.5", "promptIndex": 0}});
    let call = |id: &str, name: &str| json!({"sessionUpdate": "tool_call", "toolCallId": id, "title": name, "rawInput": {}, "_meta": {"x.ai/tool": {"name": name}}});
    let done = |id: &str, status: &str| json!({"sessionUpdate": "tool_call_update", "toolCallId": id, "status": status});
    let text = || json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "(text)"}});
    let completed = |input: u64, cached: u64, output: u64| json!({"sessionUpdate": "turn_completed", "stop_reason": "end_turn", "usage": {"inputTokens": input, "cachedReadTokens": cached, "outputTokens": output, "cacheCreationTokens": 0}});
    write_session(&sessions, "gk", 0., "headless", vec![
        (0., None, prompt()),
        (2., Some(1.5), call("c_read", "read_file")),
        (3., Some(1.5), done("c_read", "completed")),
        (4., Some(3.5), call("c_bash", "run_terminal_command")),
        // As in a live run: a shell command that exits 1 completes, with the exit code in its raw output.
        (5., Some(3.5), json!({"sessionUpdate": "tool_call_update", "toolCallId": "c_bash", "status": "completed", "rawOutput": {"type": "Bash", "exit_code": 1}})),
        (6., Some(5.5), call("c_task", "spawn_subagent")),
        (6., None, json!({"sessionUpdate": "subagent_spawned", "subagent_id": "gk-helper", "child_session_id": "gk-helper", "parent_session_id": "gk"})),
        (10., Some(5.5), done("c_task", "completed")),
        (11., Some(10.5), text()),
        (11., None, completed(600, 200, 80)),
    ]);
    let parent = sessions.join("%2Fwork%2Fconf/gk");
    std::fs::create_dir_all(parent.join("subagents/gk-helper")).unwrap();
    std::fs::write(parent.join("subagents/gk-helper/meta.json"), json!({"subagent_id": "gk-helper", "parent_session_id": "gk", "child_session_id": "gk-helper", "status": "completed"}).to_string()).unwrap();
    write_session(&sessions, "gk-helper", 6., "subagent", vec![
        (6., None, prompt()),
        (8., Some(7.5), call("c_grep", "grep")),
        (8.5, Some(7.5), done("c_grep", "completed")),
        (9.5, Some(9.), text()),
        (9.5, None, completed(300, 100, 40)),
    ]);
    vec![(sessions, "grok")]
}

#[test]
fn grok_conforms_with_usage_per_turn() {
    per_turn_usage::conforms_with_usage_on("grok", grok(&dir()), &[SpanKind::Turn], (400 + 200, 80 + 40));
}

fn load(path: &Path) -> trace_core::TraceState {
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, path, "grok", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1, "the sub-agent joins its parent's session");
    store.load_session(&sessions[0].session_id).unwrap()
}

/// Spans of `kind`, by id.
fn ids(s: &trace_core::TraceState, kind: SpanKind) -> Vec<String> {
    let mut v: Vec<_> = s.spans.values().filter(|x| x.kind == kind).map(|x| x.span_id.0.clone()).collect();
    v.sort();
    v
}

#[test]
fn a_live_recording_reads_as_turns_calls_tools_and_a_nested_subagent() {
    let s = load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/live"));
    let (main, child) = ("01a16a19-387c-7d33-837d-6e7fefec3930", "01a16a1b-0f18-71d3-ac68-24b69fbbf21f");
    // The prompt, then two turns Grok started itself when the background shell command and the sub-agent finished.
    assert_eq!(ids(&s, SpanKind::Turn), [format!("turn:{main}:0"), format!("turn:{main}:1"), format!("turn:{main}:2"), format!("turn:{child}:0")]);
    let calls_in = |turn: &str| s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref().is_some_and(|p| p.0 == turn)).count();
    // usage.json counts 3, 1 and 1 model calls in the main turns and 2 in the sub-agent's.
    assert_eq!([calls_in(&format!("turn:{main}:0")), calls_in(&format!("turn:{main}:1")), calls_in(&format!("turn:{main}:2")), calls_in(&format!("turn:{child}:0"))], [3, 1, 1, 2]);

    let sub = &s.spans[&SpanId(format!("subagent:{child}"))];
    assert_eq!(sub.parent_span_id, Some(SpanId(format!("tool:{main}:call_lnujgqx0"))), "under the spawn_subagent call");
    assert_eq!(sub.status, SpanStatus::Succeeded);

    let mut failed: Vec<_> = s.spans.values().filter(|x| x.status == SpanStatus::Failed).map(|x| (x.span_id.0.as_str(), x.tool_name.as_deref())).collect();
    failed.sort();
    assert_eq!(failed, [
        (format!("tool:{main}:task:01a16a1b-0f1d-7f73-bd51-b8d322e97c4e").as_str(), Some("run_terminal_command")),
        (format!("tool:{child}:call_5yrfq3ds").as_str(), Some("list_dir")),
    ], "the missing directory the sub-agent listed, and the shell command Grok ran in the background, which exited 1");

    // turn_completed usage for the first turn: 63668 input of which 20681 cached, 1692 output.
    let u = s.spans[&SpanId(format!("turn:{main}:0"))].usage.clone().unwrap();
    assert_eq!((u.tokens_input, u.cache_read, u.tokens_output), (Some(63668 - 20681), Some(20681), Some(1692)));
    let t = s.totals();
    assert_eq!((t.tokens_input, t.tokens_output), (42987 + 21842 + 347 + 15252, 1692 + 284 + 325 + 383), "the three main turns and the sub-agent's");
}

#[test]
fn a_live_foreground_command_that_exits_non_zero_fails() {
    let s = load(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok/live-foreground"));
    let tool = &s.spans[&SpanId("tool:01a16a38-dd90-79b2-8395-af30e0b16711:call_l0lrpdqk".into())];
    assert_eq!((tool.tool_name.as_deref(), tool.status), (Some("run_terminal_command"), SpanStatus::Failed));
}

#[test]
fn a_model_call_starts_where_the_agent_last_finished_not_at_its_first_streamed_token() {
    let s = load(&grok(&dir())[0].0);
    let llm = |stream: f64| s.spans[&SpanId(format!("llm:gk:{}", ms(stream)))].started_at_utc_ns;
    // The first call starts with the prompt; the second once the read finished, though its stream began at 3.5.
    assert_eq!((llm(1.5), llm(3.5)), (ms(0.) * 1_000_000, ms(3.) * 1_000_000));
}

#[test]
fn rereading_a_session_yields_the_same_events() {
    let files = grok(&dir());
    let mut store = Store::open_in_memory().unwrap();
    let first = import_file(&mut store, &files[0].0, "grok", &Capture::default()).unwrap();
    let again = import_file(&mut store, &files[0].0, "grok", &Capture::default()).unwrap();
    assert_eq!((again.inserted, again.duplicates), (0, first.inserted));
}
