//! OpenTelemetry traces to canonical events, for harnesses that export spans themselves: Goose, and any
//! exporter following the GenAI semantic conventions (`gen_ai.*`) or OpenInference (`llm.*`,
//! `openinference.span.kind`). Two ways in: the collector's OTLP/HTTP receiver (`POST /v1/traces`, OTLP
//! JSON or protobuf; opt in with `otlp = "127.0.0.1:4318"` in the config, then point the harness's
//! `OTEL_EXPORTER_OTLP_ENDPOINT` at it), and the `otlp` adapter, which imports a file of OTLP JSON
//! requests, one per line. Metadata only: no prompt, completion, tool argument or result attribute, and no
//! status message, is copied.
//!
//! Sessions: a trace belongs to the conversation (`gen_ai.conversation.id`, or OpenInference `session.id`)
//! of its root span, or else of its earliest span that names one, or else to a session of its own; named
//! `<service.name>:<id>`, so `service.name` is the harness. Each session gets an `agent:<session>` root; a
//! trace's root span becomes a turn under it.
//!
//! Spans keep their OpenTelemetry parent, as `otel:<span id>`. The kind comes from
//! `gen_ai.operation.name` (chat, text_completion, generate_content, embeddings: llm; execute_tool: tool;
//! invoke_agent: sub-agent when nested), else `openinference.span.kind`, else the span name; anything else
//! is a custom span. An llm span with another inside it is a wrapper (Goose wraps each provider call in a
//! second `chat` span repeating its usage) and becomes a custom span, so usage counts once. Token usage
//! from `gen_ai.usage.*` or `llm.token_count.*`, on llm spans only (Goose also totals it on the turn);
//! input tokens include cache reads in both conventions, so cache reads are subtracted. A span whose status
//! is ERROR, or that names an `error.type` (as Goose's failed tool calls do), failed.
//!
//! Sub-agents: besides nested `invoke_agent` spans, a span naming another conversation than its trace's,
//! under the same parent as the trace's own conversation (Goose runs a `delegate` sub-agent that way, in
//! the parent's trace, with no span of its own), is moved into a `subagent:<conversation>` span under that
//! parent. Goose does not link the sub-agent to the `delegate` call that started it, so it nests under the
//! turn instead, and ends with its latest span.

use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::collector::{Cmd, ask};
use crate::emit::{Emit, span};

#[derive(Default)]
pub struct Otlp {
    state: RefCell<State>,
}

/// What converting a request needs from those before it. Exporters send a span when it ends, so a parent
/// arrives with its children or after them.
#[derive(Default)]
pub(crate) struct State {
    seq: u64,
    /// The session of each trace seen.
    traces: HashMap<String, String>,
    /// The conversation of each span that names one, or has an llm span inside that does (Goose's wrapper
    /// `chat` spans name none).
    convs: HashMap<String, String>,
    /// Parents of the trace's own conversation's spans; another conversation's span under one is a sub-agent's.
    main_parents: HashSet<String>,
    /// Parents of llm spans: wrappers, when they arrive.
    llm_parents: HashSet<String>,
}

impl TraceAdapter for Otlp {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: "otlp".into(),
            name: "otlp".into(),
            harness: String::new(),
            unobservable: [
                "work still running (exporters send a span when it ends)",
                "working directory and project, unless the resource sets process.working_directory",
                "which tool call started a sub-agent, when the exporter does not nest it (Goose)",
                "cost (no prices applied)",
                "prompt, completion, tool input/output and error message text (excluded by design)",
                "session end (no end marker; idle sessions are reconciled as interrupted)",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        if raw.line.trim().is_empty() {
            return Ok(Vec::new());
        }
        Ok(convert(&serde_json::from_str(&raw.line)?, raw.observed_at_utc_ns, &mut self.state.borrow_mut()))
    }
}

struct Span<'a> {
    s: &'a Value,
    a: HashMap<&'a str, Value>,
    id: &'a str,
    trace: &'a str,
    parent: Option<&'a str>,
    start: Ns,
    end: Ns,
}

/// An `ExportTraceServiceRequest` in OTLP JSON form (ids hex, 64-bit integers as strings or numbers).
pub(crate) fn convert(req: &Value, observed: Ns, st: &mut State) -> Vec<CanonicalEvent> {
    // ponytail: memory is bounded by forgetting it all; a trace or sub-agent split across the reset may land
    // in a second session, a wrapper count its usage twice. Expire per trace if long-running receivers show it.
    if st.traces.len() + st.convs.len() + st.main_parents.len() + st.llm_parents.len() > 400_000 {
        *st = State { seq: st.seq, ..State::default() };
    }
    let mut e = Emit { source: SourceId("otlp".into()), session: SessionId(String::new()), observed, seq: st.seq, out: Vec::new() };
    for rs in list(&req["resourceSpans"]) {
        let res = attrs(&rs["resource"]["attributes"]);
        let harness = text(&res, "service.name").unwrap_or("otlp");
        let conversation = |a: &HashMap<&str, Value>| text(a, "gen_ai.conversation.id").or(text(a, "session.id")).map(|c| format!("{harness}:{c}"));
        let mut spans: Vec<_> = list(&rs["scopeSpans"])
            .flat_map(|ss| list(&ss["spans"]))
            .filter_map(|s| {
                Some(Span {
                    s,
                    a: attrs(&s["attributes"]),
                    id: s["spanId"].as_str().filter(|i| !i.is_empty())?,
                    trace: s["traceId"].as_str().unwrap_or_default(),
                    parent: s["parentSpanId"].as_str().filter(|p| !p.is_empty()),
                    start: nanos(&s["startTimeUnixNano"])?,
                    end: nanos(&s["endTimeUnixNano"])?,
                })
            })
            .collect();
        // Children end first, so by end time each span's conversation reaches its parent before the parent's own turn.
        spans.sort_by_key(|x| x.end);
        for x in &spans {
            if let Some(c) = conversation(&x.a) {
                st.convs.insert(x.id.to_owned(), c);
            }
            let Some(p) = x.parent.filter(|_| kind_of(&x.a, "", false) == SpanKind::Llm) else { continue };
            st.llm_parents.insert(p.to_owned());
            if let Some(c) = st.convs.get(x.id).cloned() {
                st.convs.entry(p.to_owned()).or_insert(c);
            }
        }
        // Roots first, then by start, so a trace takes its root's conversation, or its earliest one.
        spans.sort_by_key(|x| (x.parent.is_some(), x.start));
        for x in spans.iter().filter(|x| st.convs.contains_key(x.id)).chain(&spans) {
            let sid = st.convs.get(x.id).cloned().unwrap_or_else(|| format!("{harness}:{}", x.trace));
            st.traces.entry(x.trace.to_owned()).or_insert(sid);
        }
        spans.sort_by_key(|x| x.start);
        for x in &spans {
            if let Some(p) = x.parent.filter(|_| st.convs.get(x.id) == Some(&st.traces[x.trace])) {
                st.main_parents.insert(p.to_owned());
            }
        }
        let mut started = HashSet::new();
        for x in &spans {
            let sid = st.traces[x.trace].clone();
            e.session = SessionId(sid.clone());
            let root = span("agent", &sid);
            // ponytail: a session starts at the earliest span of the first request naming it; a span sent
            // later may start before it. The waterfall places spans by their own times, so it only shifts the root.
            if started.insert(sid.clone()) {
                let cwd = text(&res, "process.working_directory").unwrap_or_default();
                let project = std::path::Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let mut labels = Attributes::new();
                if let Some(v) = text(&res, "service.version") {
                    labels.insert("service_version".into(), json!(v));
                }
                let body = EventBody::SessionStarted {
                    project_id: project.to_owned(),
                    working_directory: cwd.to_owned(),
                    source_adapter: "otlp".into(),
                    harness: harness.to_owned(),
                    host: text(&res, "host.name").unwrap_or_default().to_owned(),
                    labels,
                };
                e.push(format!("session:{sid}"), x.start, None, body);
                e.start(&root, x.start, None, SpanKind::Agent, harness, |s| s.agent_name = Some("main".into()));
            }
            let a = &x.a;
            let me = span("otel", x.id);
            let mut parent = x.parent.map_or_else(|| root.clone(), |p| span("otel", p));
            if let Some(c) = st.convs.get(x.id).filter(|c| **c != sid && x.parent.is_some_and(|p| st.main_parents.contains(p))) {
                let sub = span("subagent", c);
                let name = text(a, "gen_ai.agent.name").unwrap_or("subagent");
                e.start(&sub, x.start, Some(&parent), SpanKind::Subagent, name, |s| s.agent_name = Some(c.clone()));
                e.end(format!("end:{sub}:{}", x.id), &sub, x.end, SpanStatus::Succeeded, None);
                parent = sub;
            }
            let name = x.s["name"].as_str().unwrap_or("span");
            let kind = match kind_of(a, name, x.parent.is_none()) {
                SpanKind::Llm if st.llm_parents.contains(x.id) => SpanKind::Custom,
                k => k,
            };
            let model = text(a, "gen_ai.response.model").or(text(a, "gen_ai.request.model")).or(text(a, "llm.model_name"));
            let tool = text(a, "gen_ai.tool.name").or(text(a, "tool.name")).or((kind == SpanKind::Tool).then_some(name));
            e.start(&me, x.start, Some(&parent), kind, name, |f| {
                f.model = model.map(str::to_owned);
                f.provider = text(a, "gen_ai.provider.name").or(text(a, "gen_ai.system")).or(text(a, "llm.provider")).map(str::to_owned);
                f.tool_name = tool.map(str::to_owned);
                f.agent_name = text(a, "gen_ai.agent.name").map(str::to_owned);
            });
            let int = |k: &str| a.get(k).and_then(Value::as_u64);
            let input = int("gen_ai.usage.input_tokens").or(int("llm.token_count.prompt"));
            let output = int("gen_ai.usage.output_tokens").or(int("llm.token_count.completion"));
            if kind == SpanKind::Llm && (input.is_some() || output.is_some()) {
                let cached = int("gen_ai.usage.cache_read.input_tokens").or(int("llm.token_count.prompt_details.cache_read"));
                let usage = Usage {
                    tokens_input: input.map(|i| i.saturating_sub(cached.unwrap_or(0))),
                    tokens_output: output,
                    cache_read: cached,
                    cache_write: int("gen_ai.usage.cache_creation.input_tokens").or(int("llm.token_count.prompt_details.cache_write")),
                    cost_usd_micros: None,
                    cost_provenance: CostProvenance::Unknown,
                };
                e.push(format!("{me}:usage"), x.end, Some(&me), EventBody::UsageReported { usage, is_final: true });
            }
            let code = &x.s["status"]["code"];
            if code == 2 || code == "STATUS_CODE_ERROR" || a.contains_key("error.type") {
                let error = text(a, "error.type").unwrap_or("span reported an error (message not captured)");
                e.end(format!("end:{me}"), &me, x.end, SpanStatus::Failed, Some(error));
            } else {
                e.end(format!("end:{me}"), &me, x.end, SpanStatus::Succeeded, None);
            }
        }
    }
    st.seq = e.seq;
    e.out
}

fn kind_of(a: &HashMap<&str, Value>, name: &str, root: bool) -> SpanKind {
    let op = text(a, "gen_ai.operation.name").or(text(a, "openinference.span.kind")).unwrap_or(name).to_ascii_lowercase();
    match op.as_str() {
        "chat" | "text_completion" | "generate_content" | "embeddings" | "llm" | "embedding" => SpanKind::Llm,
        // Older Goose names its tool span `dispatch_tool_call`.
        "execute_tool" | "tool" | "dispatch_tool_call" => SpanKind::Tool,
        _ if root => SpanKind::Turn,
        "invoke_agent" | "agent" => SpanKind::Subagent,
        _ => SpanKind::Custom,
    }
}

fn list(v: &Value) -> impl DoubleEndedIterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// OTLP key-values flattened to scalars; integers arrive as JSON strings. The first of a repeated key wins
/// (Goose records a sub-agent's `gen_ai.agent.name` on its parent's turn after the parent's own).
fn attrs(v: &Value) -> HashMap<&str, Value> {
    list(v)
        .rev()
        .filter_map(|kv| {
            let v = &kv["value"];
            let x = if let Some(s) = v.get("stringValue") {
                s.clone()
            } else if let Some(i) = v.get("intValue") {
                i.as_str().and_then(|s| s.parse::<i64>().ok()).map_or_else(|| i.clone(), Value::from)
            } else {
                v.get("doubleValue").or(v.get("boolValue")).cloned().unwrap_or(Value::Null)
            };
            Some((kv["key"].as_str()?, x))
        })
        .collect()
}

fn text<'a>(a: &'a HashMap<&str, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn nanos(v: &Value) -> Option<Ns> {
    v.as_str().and_then(|s| s.parse().ok()).or(v.as_i64()).filter(|t| VALID_TIME_NS.contains(t))
}

/// Protobuf-encoded `ExportTraceServiceRequest` to its OTLP JSON form, decoding only the fields [`convert`]
/// reads. Hand-rolled: the four wire types fit in a few lines, where prost and opentelemetry-proto would
/// add a code generator's worth of dependencies.
pub(crate) fn decode_protobuf(b: &[u8]) -> anyhow::Result<Value> {
    let rs = fields(b)?.into_iter().filter(|(n, _)| *n == 1).map(|(_, w)| resource_spans(w.bytes()?)).collect::<anyhow::Result<Vec<_>>>()?;
    Ok(json!({"resourceSpans": rs}))
}

enum Wire<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

impl<'a> Wire<'a> {
    fn bytes(&self) -> anyhow::Result<&'a [u8]> {
        match self {
            Wire::Bytes(b) => Ok(b),
            Wire::Int(_) => bail!("expected a length-delimited field"),
        }
    }
}

fn fields(mut b: &[u8]) -> anyhow::Result<Vec<(u64, Wire<'_>)>> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(b.len() >= n, "truncated protobuf");
        let (head, rest) = b.split_at(n);
        *b = rest;
        Ok(head)
    }
    fn varint(b: &mut &[u8]) -> anyhow::Result<u64> {
        let mut x = 0;
        for shift in (0..64).step_by(7) {
            let byte = take(b, 1)?[0];
            x |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(x);
            }
        }
        bail!("varint too long")
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = varint(&mut b)?;
        let w = match key & 7 {
            0 => Wire::Int(varint(&mut b)?),
            1 => Wire::Int(u64::from_le_bytes(take(&mut b, 8)?.try_into()?)),
            2 => {
                let n = varint(&mut b)?;
                Wire::Bytes(take(&mut b, usize::try_from(n)?)?)
            }
            5 => Wire::Int(u32::from_le_bytes(take(&mut b, 4)?.try_into()?).into()),
            t => bail!("unsupported protobuf wire type {t}"),
        };
        out.push((key >> 3, w));
    }
    Ok(out)
}

fn resource_spans(b: &[u8]) -> anyhow::Result<Value> {
    let (mut attributes, mut spans) = (Vec::new(), Vec::new());
    for (n, w) in fields(b)? {
        match n {
            1 => attributes.extend(key_values(w.bytes()?, 1)?),
            2 => {
                for (n, w) in fields(w.bytes()?)? {
                    if n == 2 {
                        spans.push(span_message(w.bytes()?)?);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(json!({"resource": {"attributes": attributes}, "scopeSpans": [{"spans": spans}]}))
}

fn span_message(b: &[u8]) -> anyhow::Result<Value> {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut s = serde_json::Map::new();
    let mut attributes = Vec::new();
    for (n, w) in fields(b)? {
        match (n, w) {
            (1, w) => drop(s.insert("traceId".into(), hex(w.bytes()?).into())),
            (2, w) => drop(s.insert("spanId".into(), hex(w.bytes()?).into())),
            (4, w) => drop(s.insert("parentSpanId".into(), hex(w.bytes()?).into())),
            (5, w) => drop(s.insert("name".into(), String::from_utf8_lossy(w.bytes()?).into())),
            (7, Wire::Int(t)) => drop(s.insert("startTimeUnixNano".into(), t.into())),
            (8, Wire::Int(t)) => drop(s.insert("endTimeUnixNano".into(), t.into())),
            (9, w) => attributes.push(key_value(w.bytes()?)?),
            (15, w) => {
                let code = fields(w.bytes()?)?.into_iter().find_map(|(n, w)| match (n, w) {
                    (3, Wire::Int(c)) => Some(c),
                    _ => None,
                });
                s.insert("status".into(), json!({"code": code.unwrap_or(0)}));
            }
            _ => {}
        }
    }
    s.insert("attributes".into(), attributes.into());
    Ok(s.into())
}

/// The `KeyValue`s at field `n` of a message.
fn key_values(b: &[u8], n: u64) -> anyhow::Result<Vec<Value>> {
    fields(b)?.into_iter().filter(|(f, _)| *f == n).map(|(_, w)| key_value(w.bytes()?)).collect()
}

fn key_value(b: &[u8]) -> anyhow::Result<Value> {
    let (mut key, mut value) = (String::new(), json!({}));
    for (n, w) in fields(b)? {
        match n {
            1 => key = String::from_utf8_lossy(w.bytes()?).into_owned(),
            2 => {
                for (n, w) in fields(w.bytes()?)? {
                    value = match (n, w) {
                        (1, w) => json!({"stringValue": String::from_utf8_lossy(w.bytes()?)}),
                        (2, Wire::Int(x)) => json!({"boolValue": x != 0}),
                        (3, Wire::Int(x)) => json!({"intValue": x as i64}),
                        (4, Wire::Int(x)) => json!({"doubleValue": f64::from_bits(x)}),
                        // Arrays, maps and bytes: no attribute this module reads has one.
                        _ => continue,
                    };
                }
            }
            _ => {}
        }
    }
    Ok(json!({"key": key, "value": value}))
}

const MAX_BODY: usize = 32 << 20;

/// The OTLP/HTTP receiver: answers `POST /v1/traces` on each connection until it closes.
pub(crate) async fn serve(listener: TcpListener, tx: mpsc::Sender<Cmd>) {
    let st = Arc::new(Mutex::new(State::default()));
    loop {
        match listener.accept().await {
            Ok((stream, _)) => drop(tokio::spawn(connection(stream, tx.clone(), st.clone()))),
            Err(e) => eprintln!("collector: OTLP accept: {e}"),
        }
    }
}

async fn connection(stream: TcpStream, tx: mpsc::Sender<Cmd>, st: Arc<Mutex<State>>) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    loop {
        let (status, content_type, body, close) = match request(&mut r).await {
            Ok(None) => return,
            Ok(Some(req)) => match export(&req, &tx, &st).await {
                Ok(()) if req.content_type == "application/json" => ("200 OK", req.content_type, b"{}".to_vec(), req.close),
                Ok(()) => ("200 OK", req.content_type, Vec::new(), req.close),
                Err((status, e)) => (status, "text/plain".into(), format!("{e:#}").into_bytes(), req.close),
            },
            Err(e) => ("400 Bad Request", "text/plain".into(), format!("{e:#}").into_bytes(), true),
        };
        let connection = if close { "close" } else { "keep-alive" };
        let head = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n", body.len());
        if w.write_all(head.as_bytes()).await.is_err() || w.write_all(&body).await.is_err() || close {
            return;
        }
    }
}

struct Request {
    path: String,
    content_type: String,
    body: Vec<u8>,
    close: bool,
}

type Failure = (&'static str, anyhow::Error);

async fn export(req: &Request, tx: &mpsc::Sender<Cmd>, st: &Mutex<State>) -> Result<(), Failure> {
    if req.path != "/v1/traces" {
        return Err(("404 Not Found", anyhow::anyhow!("only traces are received, at /v1/traces")));
    }
    // Requiring the OTLP content type also keeps browsers out: a page cannot send it without a CORS preflight.
    let otlp = match req.content_type.as_str() {
        "application/json" => serde_json::from_slice(&req.body).context("OTLP JSON"),
        "application/x-protobuf" => decode_protobuf(&req.body).context("OTLP protobuf"),
        other => return Err(("415 Unsupported Media Type", anyhow::anyhow!("content type {other:?}; send application/json or application/x-protobuf"))),
    }
    .map_err(|e| ("400 Bad Request", e))?;
    let events = convert(&otlp, system_now_ns(), &mut st.lock().unwrap_or_else(|p| p.into_inner()));
    ask(tx, |reply| Cmd::Append { events, source: None, reply }).await.map(drop).map_err(|e| ("503 Service Unavailable", e))
}

/// One HTTP/1.1 request, or `None` when the client closed the connection.
async fn request(r: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> anyhow::Result<Option<Request>> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        (&mut *r).take(64 << 10).read_until(b'\n', &mut head).await?;
        if head.len() == before {
            ensure!(head.is_empty(), "connection closed mid-request");
            return Ok(None);
        }
        ensure!(head.len() < 64 << 10, "request head too large");
        if matches!(&head[before..], b"\r\n" | b"\n") {
            break;
        }
    }
    let head = String::from_utf8(head)?;
    let mut lines = head.lines();
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (start.next().unwrap_or_default(), start.next().unwrap_or_default().to_owned());
    ensure!(method == "POST", "method {method}; OTLP/HTTP exports with POST");
    let mut req = Request { path, content_type: String::new(), body: Vec::new(), close: false };
    let (mut length, mut chunked) = (0, false);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = v.parse().context("content-length")?,
            "content-type" => req.content_type = v.split(';').next().unwrap_or_default().trim().to_ascii_lowercase(),
            "connection" => req.close = v.eq_ignore_ascii_case("close"),
            // ponytail: no gzip; exporters send uncompressed unless OTEL_EXPORTER_OTLP_COMPRESSION is set.
            "content-encoding" if v != "identity" => bail!("content encoding {v} is not supported; unset OTEL_EXPORTER_OTLP_COMPRESSION"),
            // Node's exporters (Qwen Code, Gemini CLI) stream the body in chunks.
            "transfer-encoding" if v.eq_ignore_ascii_case("chunked") => chunked = true,
            "transfer-encoding" => bail!("transfer encoding {v} is not supported"),
            _ => {}
        }
    }
    if !chunked {
        ensure!(length <= MAX_BODY, "body of {length} bytes exceeds {MAX_BODY}");
        req.body = vec![0; length];
        r.read_exact(&mut req.body).await?;
        return Ok(Some(req));
    }
    loop {
        let mut size = String::new();
        (&mut *r).take(1 << 10).read_line(&mut size).await?;
        let size = usize::from_str_radix(size.split(';').next().unwrap_or_default().trim(), 16).context("chunk size")?;
        ensure!(req.body.len() + size <= MAX_BODY, "body exceeds {MAX_BODY} bytes");
        let at = req.body.len();
        req.body.resize(at + size, 0);
        r.read_exact(&mut req.body[at..]).await?;
        // The chunk's CRLF; after the last chunk, trailers up to an empty line.
        let mut line = String::new();
        loop {
            line.clear();
            (&mut *r).take(64 << 10).read_line(&mut line).await?;
            if size > 0 || line.trim().is_empty() {
                break;
            }
        }
        if size == 0 {
            return Ok(Some(req));
        }
    }
}
