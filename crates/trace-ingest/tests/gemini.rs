//! Gemini CLI: the conformance scenario in Gemini CLI's format, and a session recorded by Gemini CLI 0.65.0
//! itself (`tests/fixtures/gemini-cli/live`: scripted model replies through `--fake-responses`, real tool
//! runs and recording; long texts elided).

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// The scenario as Gemini CLI writes it: each reply appended once when its stream ends and again with its
/// tool calls once they finish, tool results sent back as a user message, the session context recorded
/// as a user message before the prompt, and the sub-agent in a file of its own under the parent's id.
fn gemini(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let header = |id: &str, secs: f64, kind: &str| json!({"sessionId": id, "projectHash": "9f2c", "startTime": iso(secs), "lastUpdated": iso(secs), "kind": kind});
    let set = |secs: f64| json!({"$set": {"lastUpdated": iso(secs)}});
    let user = |id: &str, secs: f64, content: Value| json!({"id": id, "timestamp": iso(secs), "type": "user", "content": content});
    let reply = |id: &str, secs: f64, text: &str| {
        json!({"id": id, "timestamp": iso(secs), "type": "gemini", "content": text, "thoughts": [], "model": "gemini-2.5-pro",
               "tokens": {"input": 150, "output": 15, "cached": 50, "thoughts": 5, "tool": 0, "total": 170}})
    };
    let output = |call: &str, name: &str, out: &str| json!([{"functionResponse": {"id": call, "name": name, "response": {"output": out}}}]);
    let with_call = |mut r: Value, call: &str, name: &str, done: f64, status: &str, out: &str| {
        r["toolCalls"] = json!([{"id": call, "name": name, "args": {}, "result": output(call, name, out), "status": status, "timestamp": iso(done)}]);
        r
    };
    let sent = |id: &str, secs: f64, call: &str, name: &str| user(id, secs, output(call, name, "(output)"));
    let mut invoke = with_call(reply("g3", 6., ""), "c_agent", "invoke_agent", 10., "success", "(output)");
    invoke["toolCalls"][0]["agentId"] = json!("helper");
    invoke["toolCalls"][0]["args"] = json!({"agent_name": "generalist", "prompt": "(task)"});
    let shell_out = "<untrusted_context>\nOutput: (text)\nExit Code: 1\n</untrusted_context>";
    let main = vec![
        header("gm", 0., "main"),
        user("ctx", 0., json!([{"text": "(session context)"}])),
        set(0.),
        user("p1", 0., json!([{"text": "(prompt)"}])),
        set(0.),
        reply("g1", 2., ""),
        set(2.),
        with_call(reply("g1", 2., ""), "c_read", "read_file", 3., "success", "(text)"),
        sent("r1", 3., "c_read", "read_file"),
        reply("g2", 4., ""),
        with_call(reply("g2", 4., ""), "c_shell", "run_shell_command", 5., "success", shell_out),
        sent("r2", 5., "c_shell", "run_shell_command"),
        reply("g3", 6., ""),
        invoke,
        sent("r3", 10., "c_agent", "invoke_agent"),
        reply("g4", 11., "(answer)"),
        set(11.),
    ];
    let sub = vec![
        header("helper", 6., "subagent"),
        user("su1", 6., json!([{"text": "(task)"}])),
        reply("sg1", 8., ""),
        with_call(reply("sg1", 8., ""), "c_grep", "grep_search", 8.5, "success", "(text)"),
        sent("sr1", 8.5, "c_grep", "grep_search"),
        reply("sg2", 9.5, ""),
        with_call(reply("sg2", 9.5, ""), "c_done", "complete_task", 9.6, "success", "(text)"),
        json!({"$set": {"summary": "(text)"}}),
    ];
    let project = d.join("tmp/conf");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".project_root"), "/work/conf").unwrap();
    let (m, s) = (project.join("chats/session-2026-10-23T12-00-gm.jsonl"), project.join("chats/gm/helper.jsonl"));
    write_lines(&m, &main);
    write_lines(&s, &sub);
    vec![(m, "gemini-cli"), (s, "gemini-cli")]
}

#[test]
fn gemini_conforms() {
    conforms("gemini-cli", gemini(&dir()));
}

fn load(files: &[PathBuf]) -> (Store, TraceState) {
    let mut store = Store::open_in_memory().unwrap();
    for f in files {
        import_file(&mut store, f, "gemini-cli", &Capture::default()).unwrap();
    }
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1, "the sub-agent joins its parent's session");
    let s = store.load_session(&sessions[0].session_id).unwrap();
    (store, s)
}

#[test]
fn reads_a_live_recording() {
    let live = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gemini-cli/live/chats");
    let main = live.join("session-2026-10-23T18-49-dd5f56c5.jsonl");
    let sub = live.join("dd5f56c5-ce6f-43e1-bc6b-a444b6ad4694/906787f6-9ab4-4306-b575-09042444ad1f.jsonl");
    let (mut store, s) = load(&[sub.clone(), main.clone()]);
    assert!(s.unresolved().is_empty(), "{:?}", s.unresolved());
    let summary = &store.list_sessions(&SessionFilter::default()).unwrap()[0];
    assert_eq!((summary.project_id.as_str(), summary.turn_count), ("conf", 1), "the session context joins the prompt's turn");

    let tools: Vec<(&str, SpanStatus, &str)> = {
        let mut t: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Tool).map(|x| (x.name.as_str(), x.status, x.parent_span_id.as_ref().unwrap().0.split(':').next().unwrap())).collect();
        t.sort_by_key(|x| x.0);
        t
    };
    assert_eq!(
        tools,
        vec![
            ("complete_task", SpanStatus::Succeeded, "subagent"),
            ("grep_search", SpanStatus::Succeeded, "subagent"),
            ("invoke_agent", SpanStatus::Succeeded, "turn"),
            ("read_file", SpanStatus::Succeeded, "turn"),
            ("run_shell_command", SpanStatus::Failed, "turn"),
        ]
    );
    let sub_span = s.spans.values().find(|x| x.kind == SpanKind::Subagent).unwrap();
    assert_eq!((sub_span.name.as_str(), sub_span.status), ("generalist", SpanStatus::Succeeded));
    let llms: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm).collect();
    assert_eq!(llms.len(), 6, "four replies in the main agent, two in the sub-agent");
    let u = llms[0].usage.as_ref().unwrap();
    assert_eq!((llms[0].model.as_deref(), u.tokens_output, u.cache_read), (Some("gemini-3.1-pro-preview"), Some(25), Some(50)));
    let turn = s.spans.values().find(|x| x.kind == SpanKind::Turn).unwrap();
    let final_reply = llms.iter().filter(|x| x.parent_span_id.as_ref() == Some(&turn.span_id)).filter_map(|x| x.ended_at_utc_ns).max();
    assert_eq!((turn.status, turn.ended_at_utc_ns), (SpanStatus::Succeeded, final_reply), "the turn ends with the final reply");

    for f in [&main, &sub] {
        let again = import_file(&mut store, f, "gemini-cli", &Capture::default()).unwrap();
        assert_eq!(again.inserted, 0, "re-reading {} adds nothing", f.display());
    }
}

/// While a reply's tool calls run, the reply has no text and no tool calls yet: its turn stays open.
#[test]
fn a_reply_waiting_for_its_tools_keeps_the_turn_open() {
    let d = dir();
    let p = d.join("chats/session-x.jsonl");
    write_lines(&p, &[
        json!({"sessionId": "w", "projectHash": "h", "startTime": iso(0.)}),
        json!({"id": "p", "timestamp": iso(0.), "type": "user", "content": [{"text": "(prompt)"}]}),
        json!({"id": "g", "timestamp": iso(2.), "type": "gemini", "content": "", "model": "gemini-2.5-pro", "tokens": {"input": 10, "output": 2, "cached": 0}}),
    ]);
    let (_, s) = load(&[p]);
    let turn = s.spans.values().find(|x| x.kind == SpanKind::Turn).unwrap();
    assert_eq!(turn.status, SpanStatus::Running);
}

/// Versions before the JSONL log kept the whole record as one JSON object, rewritten in place.
#[test]
fn reads_a_legacy_json_session() {
    let d = dir();
    let p = d.join("chats/session-2025-09-01T10-00-abc.json");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    let session = json!({"sessionId": "old", "projectHash": "h", "startTime": iso(0.), "lastUpdated": iso(3.), "messages": [
        {"id": "p", "timestamp": iso(0.), "type": "user", "content": "(prompt)"},
        {"id": "g", "timestamp": iso(2.), "type": "gemini", "content": "", "model": "gemini-2.5-flash", "tokens": {"input": 10, "output": 2, "cached": 0, "thoughts": 0, "tool": 0, "total": 12},
         "toolCalls": [{"id": "c", "name": "run_shell_command", "args": {}, "status": "success", "timestamp": iso(3.),
                        "result": [{"functionResponse": {"id": "c", "name": "run_shell_command", "response": {"output": "Command: (text)\nExit Code: 0"}}}]}]},
        {"id": "g2", "timestamp": iso(4.), "type": "gemini", "content": "(answer)", "model": "gemini-2.5-flash", "tokens": null},
    ]});
    std::fs::write(&p, serde_json::to_string_pretty(&session).unwrap()).unwrap();
    let (_, s) = load(&[p]);
    let tool = s.spans.values().find(|x| x.kind == SpanKind::Tool).unwrap();
    assert_eq!(tool.status, SpanStatus::Succeeded, "exit code 0");
    let turn = s.spans.values().find(|x| x.kind == SpanKind::Turn).unwrap();
    assert_eq!((turn.status, turn.ended_at_utc_ns), (SpanStatus::Succeeded, Some(ms(4.) * 1_000_000)));
}
