//! OpenTelemetry traces: the conformance scenario in GenAI-semantic-conventions OTLP JSON through the `otlp`
//! adapter, and a real Goose 1.55 run (protobuf, as Goose exports it) through the collector's receiver.

mod support;

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, CollectorConfig, import_file, run_collector_until};
use trace_storage::{SessionFilter, Store};

fn kv(k: &str, v: Value) -> Value {
    let value = match v {
        Value::String(s) => json!({"stringValue": s}),
        // OTLP JSON writes 64-bit integers as strings.
        Value::Number(n) => json!({"intValue": n.to_string()}),
        other => panic!("{other}"),
    };
    json!({"key": k, "value": value})
}

/// One span: id, parent, name, seconds, attributes, failed.
fn otel_span(id: &str, parent: &str, name: &str, from: f64, to: f64, attrs: &[(&str, Value)], failed: bool) -> Value {
    let ns = |s: f64| (ms(s) * 1_000_000).to_string();
    json!({
        "traceId": "5b8efff798038103d269b633813fc60c", "spanId": id, "parentSpanId": parent, "name": name, "kind": 1,
        "startTimeUnixNano": ns(from), "endTimeUnixNano": ns(to),
        "attributes": attrs.iter().map(|(k, v)| kv(k, v.clone())).collect::<Vec<_>>(),
        "status": if failed { json!({"code": 2, "message": "(error text)"}) } else { json!({}) },
    })
}

fn request(spans: Vec<Value>) -> Value {
    let resource = [("service.name", json!("conf-agent")), ("process.working_directory", json!("/work/conf"))];
    json!({"resourceSpans": [{
        "resource": {"attributes": resource.iter().map(|(k, v)| kv(k, v.clone())).collect::<Vec<_>>()},
        "scopeSpans": [{"scope": {"name": "conf"}, "spans": spans}],
    }]})
}

/// The scenario as a GenAI-semconv agent exports it: spans in end order, the root last in a second request.
/// The sub-agent is an `invoke_agent` span inside the `task` tool call, with a conversation of its own.
fn scenario(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let main = |extra: &[(&'static str, Value)]| [&[("gen_ai.conversation.id", json!("conv-main"))], extra].concat();
    let sub = |extra: &[(&'static str, Value)]| [&[("gen_ai.conversation.id", json!("conv-sub"))], extra].concat();
    let chat = |extra: Vec<(&'static str, Value)>| {
        let mut a = vec![("gen_ai.operation.name", json!("chat")), ("gen_ai.provider.name", json!("openai")), ("gen_ai.request.model", json!("gpt-5")), ("gen_ai.response.model", json!("gpt-5-2025-08-07"))];
        a.extend([("gen_ai.usage.input_tokens", json!(150)), ("gen_ai.usage.output_tokens", json!(20)), ("gen_ai.usage.cache_read.input_tokens", json!(50))]);
        a.extend([("gen_ai.input.messages", json!("(prompt)")), ("gen_ai.output.messages", json!("(completion)"))]);
        a.extend(extra);
        a
    };
    let tool = |name: &str| vec![("gen_ai.operation.name", json!("execute_tool")), ("gen_ai.tool.name", json!(name)), ("gen_ai.tool.call.arguments", json!("(arguments)"))];
    let r = "a000000000000001";
    let first = vec![
        otel_span("a000000000000011", r, "chat gpt-5", 0.1, 2., &chat(main(&[])), false),
        otel_span("a000000000000012", r, "execute_tool read_file", 2., 3., &main(&tool("read_file")), false),
        otel_span("a000000000000013", r, "chat gpt-5", 3., 4., &chat(main(&[])), false),
        otel_span("a000000000000014", r, "execute_tool shell", 4., 5., &main(&tool("shell")), true),
        otel_span("a000000000000015", r, "chat gpt-5", 5., 6., &chat(main(&[])), false),
        otel_span("a000000000000021", "a000000000000020", "chat gpt-5", 6.1, 8., &chat(sub(&[])), false),
        otel_span("a000000000000022", "a000000000000020", "execute_tool grep", 8., 8.5, &sub(&tool("grep")), false),
        otel_span("a000000000000023", "a000000000000020", "chat gpt-5", 8.5, 9.5, &chat(sub(&[])), false),
    ];
    let agent = |name: &str| vec![("gen_ai.operation.name", json!("invoke_agent")), ("gen_ai.agent.name", json!(name))];
    let second = vec![
        otel_span("a000000000000020", "a000000000000016", "invoke_agent helper", 6.1, 9.5, &sub(&agent("helper")), false),
        otel_span("a000000000000016", r, "execute_tool task", 6., 9.6, &main(&tool("task")), false),
        otel_span("a000000000000017", r, "chat gpt-5", 10., 11., &chat(main(&[])), false),
        otel_span(r, "", "invoke_agent conf", 0., 11., &main(&agent("conf")), false),
    ];
    let p = d.join("otlp.jsonl");
    write_lines(&p, &[request(first), request(second)]);
    vec![(p, "otlp")]
}

#[test]
fn otlp_conforms() {
    conforms("otlp", scenario(&dir()));
}

#[test]
fn gen_ai_attributes_map_to_the_span_and_no_content_is_kept() {
    let files = scenario(&dir());
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &files[0].0, "otlp", &Capture { payloads: true, projects: Vec::new() }).unwrap();
    let again = import_file(&mut store, &files[0].0, "otlp", &Capture::default()).unwrap();
    assert_eq!(again.inserted, 0, "re-reading yields the same events");
    let s = store.load_session(&SessionId("conf-agent:conv-main".into())).unwrap();
    let llm = &s.spans[&SpanId("otel:a000000000000011".into())];
    assert_eq!((llm.model.as_deref(), llm.kind), (Some("gpt-5-2025-08-07"), SpanKind::Llm));
    let u = llm.usage.as_ref().unwrap();
    assert_eq!((u.tokens_input, u.cache_read, u.tokens_output), (Some(100), Some(50), Some(20)), "input tokens include cache reads");
    let shell = &s.spans[&SpanId("otel:a000000000000014".into())];
    assert_eq!((shell.tool_name.as_deref(), shell.status), (Some("shell"), SpanStatus::Failed));
    let journal: String = store.session_events(&SessionId("conf-agent:conv-main".into())).unwrap().iter().map(|e| serde_json::to_string(e).unwrap()).collect();
    for content in ["(prompt)", "(completion)", "(arguments)", "(error text)"] {
        assert!(!journal.contains(content), "{content} leaked into the journal");
    }
}

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

/// A request with a content length, or with the body in two chunks as Node exporters send it.
fn post(addr: SocketAddr, path: &str, content_type: &str, body: &[u8], chunked: bool) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let length = if chunked { "Transfer-Encoding: chunked".to_owned() } else { format!("Content-Length: {}", body.len()) };
    write!(s, "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: {content_type}\r\n{length}\r\nConnection: close\r\n\r\n").unwrap();
    if chunked {
        let (a, b) = body.split_at(body.len() / 2);
        write!(s, "{:x}\r\n", a.len()).unwrap();
        s.write_all(a).unwrap();
        write!(s, "\r\n{:x};ext=1\r\n", b.len()).unwrap();
        s.write_all(b).unwrap();
        write!(s, "\r\n0\r\n\r\n").unwrap();
    } else {
        s.write_all(body).unwrap();
    }
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    let code = reply.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (code, reply)
}

/// A collector with the OTLP receiver on, fed `requests`; returns its store once stopped.
fn receive(requests: &[(&str, &str, Vec<u8>, bool)], expect: &[u16]) -> Store {
    let d = dir();
    let addr = free_port();
    let db = d.join("trace.db");
    let cfg = CollectorConfig {
        socket_path: PathBuf::from(format!("/tmp/tw-otlp-{}-{}.sock", std::process::id(), addr.port())),
        db_path: db.clone(),
        stale_after: Duration::from_secs(3600),
        tail: Vec::new(),
        capture: Capture::default(),
        otlp: Some(addr),
    };
    let (stop, rx) = tokio::sync::oneshot::channel();
    let running = std::thread::spawn(move || run_collector_until(cfg, rx));
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline && !running.is_finished(), "receiver did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    for ((path, content_type, body, chunked), want) in requests.iter().zip(expect) {
        let (code, reply) = post(addr, path, content_type, body, *chunked);
        assert_eq!(code, *want, "{path} {content_type}: {reply}");
    }
    stop.send(()).unwrap();
    running.join().unwrap().unwrap();
    Store::open(&db).unwrap()
}

#[test]
fn receiver_takes_json_and_refuses_what_it_cannot_read() {
    let lines: Vec<Vec<u8>> = std::fs::read_to_string(&scenario(&dir())[0].0).unwrap().lines().map(|l| l.as_bytes().to_vec()).collect();
    let store = receive(
        &[
            ("/v1/traces", "application/json", lines[0].clone(), false),
            ("/v1/traces", "application/json; charset=utf-8", lines[1].clone(), true),
            ("/v1/logs", "application/json", b"{}".to_vec(), false),
            ("/v1/traces", "text/plain", lines[0].clone(), false),
            ("/v1/traces", "application/x-protobuf", vec![0x0a, 0x05, 0x00], false),
        ],
        &[200, 200, 404, 415, 400],
    );
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(store.load_session(&sessions[0].session_id).unwrap().unresolved().is_empty());
}

/// Goose 1.55 with Ollama (qwen3:8b), `OTEL_EXPORTER_OTLP_ENDPOINT` at a capture server: `cat` a file, `ls` a
/// missing directory, then `delegate` a sub-agent that runs `wc -l`. Five trace exports, scrubbed of host and
/// user name; Goose puts no prompt or tool text on its spans.
#[test]
fn goose_run_received_as_protobuf() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/otlp/goose");
    let requests: Vec<_> = (0..5).map(|i| ("/v1/traces", "application/x-protobuf", std::fs::read(fixtures.join(format!("{i}.pb"))).unwrap(), i % 2 == 1)).collect();
    let store = receive(&requests, &[200; 5]);
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.iter().map(|s| s.session_id.0.as_str()).collect::<Vec<_>>(), ["goose:20261023_2"], "the sub-agent joins its parent's session");
    let s = store.load_session(&sessions[0].session_id).unwrap();
    assert!(s.unresolved().is_empty(), "{:?}", s.unresolved());
    let of = |k: SpanKind| s.spans.values().filter(move |x| x.kind == k);
    assert_eq!(of(SpanKind::Turn).count(), 1);
    let failed: Vec<_> = of(SpanKind::Tool).filter(|t| t.status == SpanStatus::Failed).collect();
    assert_eq!(failed.len(), 1, "the ls of a missing directory");
    assert_eq!(failed[0].error.as_deref(), Some("tool_error"));
    let mut tools: Vec<_> = of(SpanKind::Tool).filter_map(|t| t.tool_name.as_deref()).collect();
    tools.sort();
    assert_eq!(tools, ["delegate", "recipe__final_output", "shell", "shell", "shell", "shell"]);

    // Goose wraps each provider call in a second `chat` span repeating its usage: one llm span per call.
    let llms: Vec<_> = of(SpanKind::Llm).collect();
    assert_eq!(llms.len(), 5, "the main agent's two calls and the sub-agent's three");
    assert!(llms.iter().all(|l| l.name == "stream_response_from_provider" && l.model.as_deref() == Some("qwen3:8b") && l.usage.is_some()));
    assert!(s.spans.values().filter(|x| x.kind != SpanKind::Llm).all(|x| x.usage.is_none()), "usage counts once");

    let sub = of(SpanKind::Subagent).next().expect("the delegated sub-agent");
    assert_eq!(sub.parent_span_id.as_ref().map(|p| s.spans[p].kind), Some(SpanKind::Turn));
    let inside = |k: SpanKind| {
        of(k).filter(|x| {
            let mut at = x.parent_span_id.clone();
            while let Some(p) = at {
                if p == sub.span_id {
                    return true;
                }
                at = s.spans[&p].parent_span_id.clone();
            }
            false
        })
        .count()
    };
    assert_eq!((inside(SpanKind::Llm), inside(SpanKind::Tool)), (3, 2), "its model calls, shell and final output");
}

/// Qwen Code 0.26 with Ollama (qwen3:8b), `QWEN_TELEMETRY_OTLP_PROTOCOL=http` (OTLP JSON, sent chunked), run
/// with `--max-tool-calls 4` until the budget stopped it: five trace exports, one request per line. Qwen Code
/// exports its interaction and model calls as spans (tool calls only as logs), each call with an HTTP span inside.
#[test]
fn qwen_code_export_imports_from_a_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/otlp/qwen-code.jsonl");
    let mut store = Store::open_in_memory().unwrap();
    import_file(&mut store, &path, "otlp", &Capture::default()).unwrap();
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].session_id.0.starts_with("qwen-code:"));
    let s = store.load_session(&sessions[0].session_id).unwrap();
    assert!(s.unresolved().is_empty());
    let turn = s.spans.values().find(|x| x.kind == SpanKind::Turn).unwrap();
    assert_eq!((turn.status, turn.error.as_deref()), (SpanStatus::Failed, Some("run_budget_exceeded")));
    let llms: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm).collect();
    assert_eq!(llms.len(), 5);
    assert!(llms.iter().all(|l| l.parent_span_id.as_ref() == Some(&turn.span_id) && l.usage.as_ref().is_some_and(|u| u.cache_read.is_some())));
    assert_eq!(s.spans.values().filter(|x| x.kind == SpanKind::Custom && x.name == "POST").count(), 5);
}
