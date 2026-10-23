//! One scenario, recorded the way each harness records it, must come out as the same trace shape: so a
//! feature built on the trace (waterfall, folds, replay, costs, sub-agent nesting) works alike for all.
//!
//! The scenario, in seconds after the session starts: a prompt at 0; a model call to 2 that asks to read a
//! file (2–3); a model call to 4 that runs a shell command, which fails (4–5); a model call to 6 that
//! starts a sub-agent (6–9.5: a model call that greps, then a final model call); a closing model call
//! 10–11 that ends the turn.
//!
//! Each fixture records a failure the way that harness really does, as seen in live runs: Claude Code and pi
//! flag the tool result an error, Codex reports the command's exit code, OpenCode marks the shell call
//! completed with a non-zero `metadata.exit`.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

const MS: Ns = 1_000_000;
/// 2026-10-23T12:00:00Z.
const T0_MS: i64 = 1_792_756_800_000;

fn dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("tw-conformance-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn iso(secs: f64) -> String {
    let ms = (secs * 1000.).round() as i64;
    format!("2026-10-23T12:00:{:02}.{:03}Z", ms / 1000, ms % 1000)
}

fn ms(secs: f64) -> i64 {
    T0_MS + (secs * 1000.).round() as i64
}

fn write_lines(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n").unwrap();
}

fn claude_code(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let base = |uuid: &str, secs: f64| json!({"uuid": uuid, "sessionId": "cc", "cwd": "/work/conf", "isSidechain": false, "timestamp": iso(secs)});
    let with = |mut v: Value, extra: Value| {
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    let assistant = |uuid: &str, secs: f64, id: &str, stop: &str, content: Value| {
        with(base(uuid, secs), json!({"type": "assistant", "message": {"id": id, "model": "claude-sonnet-4-5", "stop_reason": stop, "content": content, "usage": {"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 50}}}))
    };
    let result = |uuid: &str, secs: f64, call: &str, error: bool| {
        with(base(uuid, secs), json!({"type": "user", "promptId": "p1", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call, "is_error": error}]}}))
    };
    let tool = |id: &str, name: &str| json!([{"type": "tool_use", "id": id, "name": name}]);
    let main = vec![
        with(base("u1", 0.), json!({"type": "user", "promptId": "p1", "message": {"role": "user", "content": "(prompt)"}})),
        assistant("a1", 2., "m1", "tool_use", tool("t_read", "Read")),
        result("r1", 3., "t_read", false),
        assistant("a2", 4., "m2", "tool_use", tool("t_bash", "Bash")),
        result("r2", 5., "t_bash", true),
        assistant("a3", 6., "m3", "tool_use", tool("t_task", "Agent")),
        with(result("r3", 10., "t_task", false), json!({"toolUseResult": {"agentId": "helper", "status": "completed"}})),
        assistant("a4", 11., "m4", "end_turn", json!([])),
        with(base("s1", 11.), json!({"type": "system", "subtype": "turn_duration"})),
    ];
    let side = |v: Value| with(v, json!({"isSidechain": true, "agentId": "helper"}));
    let sub = vec![
        side(with(base("su1", 6.), json!({"type": "user", "promptId": "ps", "message": {"role": "user", "content": "(task)"}}))),
        side(assistant("sa1", 8., "m5", "tool_use", tool("t_grep", "Grep"))),
        side(result("sr1", 8.5, "t_grep", false)),
        side(assistant("sa2", 9.5, "m6", "end_turn", json!([]))),
    ];
    let (m, s) = (d.join("cc.jsonl"), d.join("cc/subagents/agent-helper.jsonl"));
    write_lines(&m, &main);
    write_lines(&s, &sub);
    vec![(m, "claude-code"), (s, "claude-code")]
}

fn codex(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let line = |secs: f64, kind: &str, payload: Value| json!({"timestamp": iso(secs), "type": kind, "payload": payload});
    let usage = |secs: f64, resp: &str| line(secs, "token_usage_record", json!({"response_id": resp, "usage": {"input_tokens": 150, "cached_input_tokens": 50, "output_tokens": 20}}));
    let call = |secs: f64, id: &str, name: &str| line(secs, "response_item", json!({"type": "function_call", "call_id": id, "name": name}));
    let output = |secs: f64, id: &str| line(secs, "response_item", json!({"type": "function_call_output", "call_id": id}));
    let started = |secs: f64, turn: &str, by_agent: bool| {
        line(secs, "event_msg", json!({"type": "task_started", "turn_id": turn, "turn_attribution": {"initiating_agent_path": if by_agent { json!("/root") } else { Value::Null }}}))
    };
    let parent = vec![
        line(0., "session_meta", json!({"id": "cx", "session_id": "cx", "cwd": "/work/conf", "model_provider": "openai", "source": "exec"})),
        started(0., "turn-1", false),
        line(0., "turn_context", json!({"model": "gpt-5"})),
        call(2., "c_read", "read_file"),
        usage(2., "r1"),
        output(3., "c_read"),
        call(4., "c_bash", "shell"),
        usage(4., "r2"),
        line(4.9, "event_msg", json!({"type": "item_completed", "item": {"type": "CommandExecution", "id": "x_bash", "status": "failed", "exit_code": 1}, "started_at_ms": ms(4.1), "completed_at_ms": ms(4.9)})),
        output(5., "c_bash"),
        call(6., "c_spawn", "spawn_agent"),
        usage(6., "r3"),
        line(6., "event_msg", json!({"type": "item_completed", "item": {"type": "SubAgentActivity", "id": "c_spawn", "kind": "started", "agent_thread_id": "cx-helper", "agent_path": "/root/helper", "model": "gpt-5"}})),
        output(6.1, "c_spawn"),
        call(6.2, "c_wait", "wait_agent"),
        line(9.6, "event_msg", json!({"type": "item_completed", "item": {"type": "SubAgentActivity", "id": "done", "kind": "completed", "agent_thread_id": "cx-helper"}})),
        output(10., "c_wait"),
        usage(11., "r4"),
        line(11., "event_msg", json!({"type": "task_complete", "turn_id": "turn-1"})),
    ];
    let child = vec![
        line(6., "session_meta", json!({"id": "cx-helper", "session_id": "cx", "cwd": "/work/conf", "model_provider": "openai", "source": {"subagent": {"thread_spawn": {"parent_thread_id": "cx", "agent_path": "/root/helper"}}}})),
        line(6., "event_msg", json!({"type": "thread_settings_applied", "thread_id": "cx-helper"})),
        started(6., "turn-2", true),
        line(6., "turn_context", json!({"model": "gpt-5"})),
        call(8., "c_grep", "grep"),
        usage(8., "r5"),
        output(8.5, "c_grep"),
        usage(9.5, "r6"),
        line(9.5, "event_msg", json!({"type": "task_complete", "turn_id": "turn-2"})),
    ];
    let (p, c) = (d.join("rollout-cx.jsonl"), d.join("rollout-cx-helper.jsonl"));
    write_lines(&p, &parent);
    write_lines(&c, &child);
    vec![(p, "codex"), (c, "codex")]
}

fn pi(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let usage = json!({"input": 100, "output": 20, "cacheRead": 50, "cacheWrite": 0, "totalTokens": 170, "cost": {"total": 0.001}});
    let assistant = |start: f64, end: f64, stop: &str, content: Value| {
        json!({"role": "assistant", "content": content, "provider": "anthropic", "model": "claude-sonnet-4-5", "usage": usage, "stopReason": stop, "timestamp": ms(start), "durationMs": ((end - start) * 1000.) as i64})
    };
    let call = |id: &str, name: &str| json!([{"type": "toolCall", "id": id, "name": name, "arguments": {}}]);
    let result = |secs: f64, id: &str, error: bool| json!({"role": "toolResult", "toolCallId": id, "toolName": "t", "content": [], "isError": error, "timestamp": ms(secs)});
    let user = |secs: f64| json!({"role": "user", "content": [], "timestamp": ms(secs)});
    let sub = json!({"agent": "helper", "model": "claude-sonnet-4-5", "exitCode": 0, "stopReason": "stop", "messages": [
        user(6.), assistant(6.5, 8., "toolUse", call("s_grep", "grep")), result(8.5, "s_grep", false), assistant(8.5, 9.5, "stop", json!([])),
    ]});
    let mut task_result = result(10., "p_task", false);
    task_result["details"] = json!({"mode": "single", "results": [sub]});
    let messages = [
        (0., user(0.)),
        (2., assistant(0.5, 2., "toolUse", call("p_read", "read"))),
        (3., result(3., "p_read", false)),
        (4., assistant(3., 4., "toolUse", call("p_bash", "bash"))),
        (5., result(5., "p_bash", true)),
        (6., assistant(5., 6., "toolUse", call("p_task", "subagent"))),
        (10., task_result),
        (11., assistant(10., 11., "stop", json!([]))),
    ];
    let mut lines = vec![json!({"type": "session", "version": 3, "id": "pi", "timestamp": iso(0.), "cwd": "/work/conf"})];
    let mut parent = Value::Null;
    for (i, (secs, m)) in messages.into_iter().enumerate() {
        let id = format!("e{i}");
        lines.push(json!({"type": "message", "id": id, "parentId": parent, "timestamp": iso(secs), "message": m}));
        parent = json!(id);
    }
    let p = d.join("pi.jsonl");
    write_lines(&p, &lines);
    vec![(p, "pi")]
}

/// Oh My Pi keeps pi's format with its own dialect: a `title` line before the header, and `duration` (fractional
/// milliseconds) with `completedAt` where pi writes `durationMs`.
fn omp(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let (src, _) = pi(d).remove(0);
    let mut lines = vec![json!({"type": "title", "title": "(text)"})];
    for line in std::fs::read_to_string(&src).unwrap().lines() {
        let mut v: Value = serde_json::from_str(line).unwrap();
        let m = &mut v["message"];
        if let Some(d) = m.get("durationMs").cloned().filter(|_| m["role"] == "assistant") {
            m.as_object_mut().unwrap().remove("durationMs");
            m["duration"] = json!(d.as_f64().unwrap() + 0.4);
            m["completedAt"] = json!(m["timestamp"].as_i64().unwrap() + d.as_i64().unwrap());
        }
        lines.push(v);
    }
    let p = d.join("omp.jsonl");
    write_lines(&p, &lines);
    vec![(p, "omp")]
}

fn opencode(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let db = d.join("opencode.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute_batch(
        "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL, version TEXT NOT NULL, parent_id TEXT, agent TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);",
    )
    .unwrap();
    let tool = |id: &str, name: &str, ran: f64, done: f64, status: &str, meta: Value| {
        json!({"type": "tool", "id": id, "name": name, "state": {"status": status, "input": {}, "metadata": meta}, "time": {"created": ms(ran), "ran": ms(ran), "completed": ms(done)}})
    };
    let assistant = |start: f64, end: f64, tools: Value| {
        json!({"time": {"created": ms(start), "streamed": ms(end), "completed": ms(end)}, "model": {"id": "claude-sonnet-4-5", "providerID": "anthropic"}, "content": tools, "cost": 0.001, "tokens": {"input": 100, "output": 20, "reasoning": 0, "cache": {"read": 50, "write": 0}}})
    };
    let rows: Vec<(&str, &str, &str, f64, Value)> = vec![
        ("oc", "m1", "user", 0., json!({})),
        ("oc", "m2", "assistant", 0.5, assistant(0.5, 2., json!([tool("o_read", "read", 2., 3., "completed", json!({}))]))),
        ("oc", "m3", "assistant", 3., assistant(3., 4., json!([tool("o_bash", "shell", 4., 5., "completed", json!({"exit": 1}))]))),
        ("oc", "m4", "assistant", 5., assistant(5., 6., json!([tool("o_task", "subagent", 6., 10., "completed", json!({"sessionID": "oc-helper"}))]))),
        ("oc", "m5", "assistant", 10., assistant(10., 11., json!([]))),
        ("oc", "m6", "idle", 11., json!({"outcome": "succeeded"})),
        ("oc-helper", "h1", "user", 6., json!({})),
        ("oc-helper", "h2", "assistant", 6.5, assistant(6.5, 8., json!([tool("o_grep", "grep", 8., 8.5, "completed", json!({}))]))),
        ("oc-helper", "h3", "assistant", 8.5, assistant(8.5, 9.5, json!([]))),
        ("oc-helper", "h4", "idle", 9.5, json!({"outcome": "succeeded"})),
    ];
    c.execute("INSERT INTO session_v2 VALUES ('oc', '/work/conf', '2.1.3', NULL, 'build', ?1, ?2)", [ms(0.), ms(11.)]).unwrap();
    c.execute("INSERT INTO session_v2 VALUES ('oc-helper', '/work/conf', '2.1.3', 'oc', 'helper', ?1, ?2)", [ms(6.), ms(9.5)]).unwrap();
    for (seq, (sid, id, kind, at, data)) in rows.into_iter().enumerate() {
        c.execute("INSERT INTO session_message VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)", rusqlite::params![id, sid, kind, seq as i64, ms(at), data.to_string()]).unwrap();
    }
    vec![(db, "opencode")]
}

/// The shape every harness's recording of the scenario must have.
fn conforms(harness: &str, files: Vec<(PathBuf, &'static str)>) {
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
        assert!(llm.model.is_some(), "{harness}: {} names its model", llm.span_id);
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
    assert!(s.spans.values().any(|x| x.kind == SpanKind::Tool && inside(&x.span_id)), "{harness}: the sub-agent's tool calls sit inside it");
    assert_eq!(sub.status, SpanStatus::Succeeded, "{harness}: the sub-agent finished");

    for x in s.spans.values().filter(|x| x.span_id != root.span_id) {
        let end = x.ended_at_utc_ns.unwrap_or_else(|| panic!("{harness}: {} ended", x.span_id));
        assert!(end >= x.started_at_utc_ns, "{harness}: {} ends after it starts", x.span_id);
        let Some(p) = x.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).filter(|p| p.span_id != root.span_id) else { continue };
        // A sub-agent can outlive the call that started it (Codex's spawn_agent returns at once).
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{harness}: {} starts within {}", x.span_id, p.span_id);
        if x.kind != SpanKind::Subagent {
            assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{harness}: {} ends within {}", x.span_id, p.span_id);
        }
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert!(calls.windows(2).all(|w| w[0].1 <= w[1].0 + MS), "{harness}: the main agent's model calls do not overlap: {calls:?}");
}

#[test]
fn claude_code_conforms() {
    conforms("claude-code", claude_code(&dir()));
}

#[test]
fn codex_conforms() {
    conforms("codex", codex(&dir()));
}

#[test]
fn pi_conforms() {
    conforms("pi", pi(&dir()));
}

#[test]
fn omp_conforms() {
    conforms("omp", omp(&dir()));
}

#[test]
fn opencode_conforms() {
    conforms("opencode", opencode(&dir()));
}
