//! Qwen Code session JSONL (`$QWEN_HOME/projects/<sanitized cwd>/chats/<session>.jsonl`, `QWEN_HOME`
//! defaulting to `~/.qwen`) to canonical events. Metadata only: no prompt, response, thinking or tool
//! payload text is copied (the telemetry records carry `response_text` and `function_args`; both are skipped).
//!
//! Records are Gemini `Content` in Claude-Code-like envelopes (`uuid`, `sessionId`, `timestamp`, `type`
//! user / assistant / tool_result / system), and `system` records of subtype `ui_telemetry` mirror the
//! CLI's telemetry events. Spans: `agent:<session>` root, `turn:<promptId>` from a user prompt until an
//! assistant message that asks for no tool (the CLI writes no turn-end record), `llm:<record uuid>` per
//! assistant record (its start from the `qwen-code.api_response` telemetry just before it, whose
//! `duration_ms` covers the request), `tool:<call id>` from the `functionCall` part to its `tool_result`
//! record (failed when the result carries an error, which includes a non-zero shell exit). A call routed
//! through the deferred-tool bridge `tool_call` is named after the tool it calls.
//!
//! Sub-agents: an `agent` call starts `subagent:<call id>` under its tool span, and a foreground call's
//! result ends it. Each sub-agent also writes `projects/<cwd>/subagents/<session>/agent-<agent id>.jsonl`
//! (`isSidechain`), but that transcript records neither the model nor when a call started, so those
//! records are skipped; the parent file's telemetry has both. Its `api_response` and `tool_call` events
//! that name a sub-agent (`subagent_id`, or a `<session>#<agent id>#<n>` prompt id) become `llm:` and
//! `tool:` spans inside it, written when each completes. Qwen Code builds the agent id as
//! `<subagent name>-<call id>`.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

pub const SNAPSHOT: bool = false;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Qwen { harness, state: RefCell::default() })
}

pub fn read(_path: &std::path::Path, since: i64, _harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    Ok((Vec::new(), since))
}

struct Qwen {
    harness: &'static str,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    prev_ts: Option<Ns>,
    session_started: bool,
    turn: Option<String>,
    turn_open: bool,
    turns: u64,
    /// Start of the main agent's model call that the next assistant record ends.
    llm_start: Option<Ns>,
    /// `agent` calls whose result ends their sub-agent.
    agent_calls: HashSet<String>,
    /// Sub-agent ids seen in telemetry, and the call id that names each one's span.
    agents: HashMap<String, String>,
}

/// `agent` today; `task` in earlier releases.
const AGENT_TOOLS: [&str; 2] = ["agent", "task"];

const MS: Ns = 1_000_000;

impl TraceAdapter for Qwen {
    fn source_info(&self) -> SourceInfo {
        let h = self.harness;
        SourceInfo {
            source_id: h.into(),
            name: h.into(),
            harness: h.into(),
            unobservable: [
                "LLM call start time when no api_response telemetry precedes the response (then inferred from the preceding record)",
                "time to first token",
                "cost (no prices recorded)",
                "prompt, assistant, thinking and tool input/output text (excluded by design)",
                "tool execution start of the main agent (taken from the requesting assistant message)",
                "turn end (no end marker; taken from the first assistant message that asks for no tool)",
                "end of a background sub-agent (its completion notification names no agent)",
                "sub-agent spans before each call completes (sub-agent telemetry is written on completion)",
                "side model calls without an assistant record (session titles, memory extraction)",
                "session end (no end marker)",
                "host",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        if raw.line.trim().is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&raw.line)?;
        if v["isSidechain"] == true {
            return Ok(Vec::new());
        }
        let mut st = self.state.borrow_mut();
        let Some(ts) = v["timestamp"].as_str().and_then(parse_utc).filter(|t| VALID_TIME_NS.contains(t)) else {
            return Ok(Vec::new());
        };
        let prev = st.prev_ts.replace(ts).unwrap_or(ts);
        let (Some(sid), Some(uuid)) = (v["sessionId"].as_str(), v["uuid"].as_str()) else {
            return Ok(Vec::new());
        };
        let h = self.harness;
        let mut e = Emit {
            source: SourceId(format!("{h}:{sid}")),
            session: SessionId(sid.to_owned()),
            observed: raw.observed_at_utc_ns,
            seq: st.seq,
            out: Vec::new(),
        };
        let root = span("agent", sid);

        if !st.session_started
            && let Some(cwd) = v["cwd"].as_str()
        {
            st.session_started = true;
            let project = std::path::Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
            let mut labels = Attributes::new();
            if let Some(ver) = v["version"].as_str() {
                labels.insert("qwen_code_version".into(), json!(ver));
            }
            let body = EventBody::SessionStarted {
                project_id: project.to_owned(),
                working_directory: cwd.to_owned(),
                source_adapter: h.into(),
                harness: h.into(),
                host: String::new(),
                labels,
            };
            e.push(format!("session:{sid}"), ts, None, body);
            e.start(&root, ts, None, SpanKind::Agent, h, |s| s.agent_name = Some("main".into()));
        }

        let parts = v["message"]["parts"].as_array().map(Vec::as_slice).unwrap_or_default();
        let parent = match &st.turn {
            Some(t) if st.turn_open => span("turn", t),
            _ => root.clone(),
        };

        match (v["type"].as_str(), v["subtype"].as_str()) {
            (Some("user"), None) => {
                st.llm_start = None;
                let key = v["promptId"].as_str().unwrap_or(uuid);
                if st.turn.as_deref() != Some(key) {
                    if let Some(old) = st.turn.take().filter(|_| st.turn_open) {
                        let t = span("turn", &old);
                        e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
                    }
                    st.turns += 1;
                    let name = format!("turn {}", st.turns);
                    e.start(&span("turn", key), ts, Some(&root), SpanKind::Turn, &name, |_| {});
                    st.turn = Some(key.to_owned());
                    st.turn_open = true;
                }
            }
            (Some("system"), Some("ui_telemetry")) => {
                let ev = &v["systemPayload"]["uiEvent"];
                let at = ev["event.timestamp"].as_str().and_then(parse_utc).unwrap_or(ts);
                let took = ev["duration_ms"].as_i64().unwrap_or(0) * MS;
                if ev["event.name"] == "qwen-code.api_response" && ev["subagent_id"].is_null() {
                    st.llm_start = Some(at - took);
                } else if ev["event.name"] == "qwen-code.api_response"
                    && let Some(agent) = ev["subagent_id"].as_str()
                {
                    let call = ev["subagent_name"].as_str().and_then(|n| agent.strip_prefix(n)?.strip_prefix('-')).unwrap_or(agent);
                    st.agents.insert(agent.to_owned(), call.to_owned());
                    let llm = span("llm", uuid);
                    let model = ev["model"].as_str();
                    e.start(&llm, at - took, Some(&span("subagent", call)), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
                        s.model = model.map(str::to_owned)
                    });
                    let u = usage(&ev["input_token_count"], &ev["cached_content_token_count"], &ev["total_token_count"], &ev["output_token_count"]);
                    e.push(format!("{uuid}:usage"), at, Some(&llm), EventBody::UsageReported { usage: u, is_final: true });
                    e.end(format!("{uuid}:end"), &llm, at, SpanStatus::Succeeded, None);
                } else if ev["event.name"] == "qwen-code.tool_call"
                    && let Some(agent) = ev["prompt_id"].as_str().and_then(prompt_agent)
                    && let Some(call) = st.agents.get(agent)
                    && let (Some(id), Some(name)) = (ev["call_id"].as_str(), ev["function_name"].as_str())
                {
                    let start = ev["started_at_ms"].as_i64().map_or(at - took, |t| t * MS);
                    let tool = span("tool", id);
                    e.start(&tool, start, Some(&span("subagent", call)), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                    let (status, error) = outcome(ev["success"] == false);
                    e.end(format!("end:{tool}"), &tool, at, status, error);
                    if AGENT_TOOLS.contains(&name) {
                        let sub = span("subagent", id);
                        e.start(&sub, start, Some(&tool), SpanKind::Subagent, "subagent", |_| {});
                        e.end(format!("end:{sub}"), &sub, at, status, error);
                    }
                }
            }
            (Some("assistant"), _) => {
                let calls: Vec<_> = parts
                    .iter()
                    .filter_map(|p| {
                        let c = &p["functionCall"];
                        let name = c["name"].as_str()?;
                        let name = if name == "tool_call" { c["args"]["name"].as_str().unwrap_or(name) } else { name };
                        Some((c["id"].as_str()?, name))
                    })
                    .collect();
                let llm = span("llm", uuid);
                let model = v["model"].as_str();
                let start = st.llm_start.take();
                e.start(&llm, start.unwrap_or(prev).min(ts), Some(&parent), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
                    s.model = model.map(str::to_owned);
                    if start.is_none() {
                        s.attributes.insert("start_inferred".into(), json!(true));
                    }
                });
                let u = &v["usageMetadata"];
                if u.is_object() {
                    let u = usage(&u["promptTokenCount"], &u["cachedContentTokenCount"], &u["totalTokenCount"], &u["candidatesTokenCount"]);
                    e.push(format!("{uuid}:usage"), ts, Some(&llm), EventBody::UsageReported { usage: u, is_final: true });
                }
                e.end(format!("{uuid}:end"), &llm, ts, SpanStatus::Succeeded, None);
                for &(id, name) in &calls {
                    let tool = span("tool", id);
                    e.start(&tool, ts, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                    if AGENT_TOOLS.contains(&name) {
                        st.agent_calls.insert(id.to_owned());
                        e.start(&span("subagent", id), ts, Some(&tool), SpanKind::Subagent, "subagent", |_| {});
                    }
                }
                if calls.is_empty()
                    && st.turn_open
                    && let Some(t) = st.turn.as_deref()
                {
                    let t = span("turn", t);
                    e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
                    st.turn_open = false;
                }
            }
            (Some("tool_result"), _) => {
                let result = &v["toolCallResult"];
                for p in parts {
                    let r = &p["functionResponse"];
                    let Some(id) = r["id"].as_str().or(result["callId"].as_str()) else {
                        continue;
                    };
                    let (status, error) = outcome(result["status"] == "error" || r["response"].get("error").is_some());
                    let tool = span("tool", id);
                    e.end(format!("end:{tool}"), &tool, ts, status, error);
                    if st.agent_calls.remove(id) && result["resultDisplay"]["executionMode"] != "background" {
                        let sub = span("subagent", id);
                        e.end(format!("end:{sub}"), &sub, ts, status, error);
                    }
                }
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

/// Usage from Qwen Code's counts: `prompt` includes the cached tokens, and Gemini counts thoughts apart
/// from candidates while the OpenAI converter counts them inside, so total minus prompt is the output.
fn usage(prompt: &Value, cached: &Value, total: &Value, candidates: &Value) -> Usage {
    let (prompt, cached) = (prompt.as_u64(), cached.as_u64());
    Usage {
        tokens_input: prompt.map(|p| p.saturating_sub(cached.unwrap_or(0))),
        tokens_output: total.as_u64().zip(prompt).map(|(t, p)| t.saturating_sub(p)).or(candidates.as_u64()),
        cache_read: cached,
        cache_write: None,
        cost_usd_micros: None,
        cost_provenance: CostProvenance::Unknown,
    }
}

fn outcome(failed: bool) -> (SpanStatus, Option<&'static str>) {
    if failed {
        (SpanStatus::Failed, Some("tool reported an error (output not captured)"))
    } else {
        (SpanStatus::Succeeded, None)
    }
}

/// The agent id in a sub-agent's prompt id, `<session>#<agent id>#<n>`; the main agent's is `<session>########<n>`.
fn prompt_agent(prompt_id: &str) -> Option<&str> {
    match prompt_id.split('#').filter(|s| !s.is_empty()).collect::<Vec<_>>()[..] {
        [_, agent, _] => Some(agent),
        _ => None,
    }
}
