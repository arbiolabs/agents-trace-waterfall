//! GitHub Copilot CLI session log (`~/.copilot/session-state/<session>/events.jsonl`, one typed event per
//! line: `{type, id, parentId, timestamp, data, agentId?}`) to canonical events. Metadata only: no prompt,
//! message, reasoning, command or tool output text is copied.
//!
//! Spans: `agent:<session>` root, `turn:<interaction id>` per `user.message` (ended by every main-agent
//! `assistant.turn_end`, so the latest one wins), `llm:<message id>` per `assistant.message`, from where the
//! model last got control (`assistant.turn_start`, the agent's latest tool result, or its start) to the
//! message, and `tool:<call id>` from `tool.execution_start` to `tool.execution_complete`, failed when it
//! reports `success: false` or a non-zero exit code (`shellExecution`, or a terminal block of the result,
//! whose text is not read). Copilot's own "turn" is one model call and its tools; a user prompt spans
//! several. Event ids derive from Copilot's ids and `source_seq` counts events from the top of the file.
//! `session.shutdown` is not mapped: a resumed session appends to the same file after it.
//!
//! Sub-agents: their messages and tool calls land in the same file, tagged with `data.parentToolCallId`
//! (deprecated) or the sub-agent's `agentId`, which `subagent.started` ties to its spawning call (1.0.63
//! uses the call id itself). `subagent:<call id>` opens under that tool call at `subagent.started` and
//! ends at `subagent.completed` or `subagent.failed`. A sub-agent's own turn starts are never written.
//!
//! Tokens: the log keeps only `outputTokens` on `assistant.message`, and builds from 1.0.8x drop even that.
//! Per-call input and cache tokens are ephemeral `assistant.usage` events, never written here; they reach
//! disk only as rows of `~/.copilot/session-store.db` `assistant_usage_events` (no message id to join on)
//! and as per-model session totals on `session.shutdown`. Neither is read. For exact per-call usage, run
//! Copilot with `COPILOT_OTEL_ENABLED=1` and receive its OpenTelemetry export instead.

use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

pub const SNAPSHOT: bool = false;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Copilot { harness, state: RefCell::default() })
}

pub fn read(_path: &Path, since: i64, _harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    Ok((Vec::new(), since))
}

struct Copilot {
    harness: &'static str,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    session: Option<String>,
    model: Option<String>,
    turn: Option<String>,
    turns: u64,
    /// Sub-agent `agentId` to the tool call that spawned it.
    agents: HashMap<String, String>,
    /// Where each agent's next model call started, keyed by its spawning call ("" for the main agent).
    llm_from: HashMap<String, Ns>,
}

impl TraceAdapter for Copilot {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.harness.into(),
            name: self.harness.into(),
            harness: self.harness.into(),
            unobservable: [
                "input and cache tokens per model call (ephemeral; cost estimates cover output tokens only)",
                "output tokens on Copilot CLI 1.0.8x and later",
                "LLM call start time (inferred from the turn start or the preceding tool result)",
                "cost (Copilot bills in premium requests and AI credits, not recorded per call)",
                "prompt, message, reasoning, command and tool input/output text (excluded by design)",
                "session end (no end marker read; idle sessions are reconciled as interrupted)",
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
        let Some(ts) = v["timestamp"].as_str().and_then(parse_utc).filter(|t| VALID_TIME_NS.contains(t)) else {
            return Ok(Vec::new());
        };
        let d = &v["data"];
        let mut st = self.state.borrow_mut();
        if v["type"] == "session.start" && st.session.is_none() {
            st.session = d["sessionId"].as_str().map(str::to_owned);
        }
        let Some(session) = st.session.clone() else { return Ok(Vec::new()) };
        let mut e = Emit { source: SourceId(format!("{}:{session}", self.harness)), session: SessionId(session.clone()), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };
        let root = span("agent", &session);
        // The spawning call of the sub-agent this event came from, or "" for the main agent.
        let agent = match (d["parentToolCallId"].as_str(), v["agentId"].as_str()) {
            (Some(call), _) => call.to_owned(),
            (None, Some(id)) => st.agents.get(id).map_or(id, String::as_str).to_owned(),
            (None, None) => String::new(),
        };
        let parent = if agent.is_empty() { st.turn.as_deref().map_or_else(|| root.clone(), |t| span("turn", t)) } else { span("subagent", &agent) };

        match v["type"].as_str().unwrap_or_default() {
            "session.start" => {
                let cwd = d["context"]["cwd"].as_str().unwrap_or_default();
                let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let mut labels = Attributes::new();
                if let Some(x) = d["copilotVersion"].as_str() {
                    labels.insert("copilot_version".into(), x.into());
                }
                st.model = d["selectedModel"].as_str().map(str::to_owned);
                let body = EventBody::SessionStarted {
                    project_id: project.to_owned(),
                    working_directory: cwd.to_owned(),
                    source_adapter: self.harness.into(),
                    harness: self.harness.into(),
                    host: String::new(),
                    labels,
                };
                e.push(format!("session:{session}"), ts, None, body);
                e.start(&root, ts, None, SpanKind::Agent, self.harness, |s| s.agent_name = Some("main".into()));
            }
            "session.model_change" => st.model = d["newModel"].as_str().map(str::to_owned).or(st.model.take()),
            "user.message" if agent.is_empty() => {
                let id = d["interactionId"].as_str().or(v["id"].as_str()).unwrap_or_default().to_owned();
                if st.turn.as_ref() == Some(&id) {
                    return Ok(e.out);
                }
                st.turns += 1;
                e.start(&span("turn", &id), ts, Some(&root), SpanKind::Turn, &format!("turn {}", st.turns), |_| {});
                st.turn = Some(id);
                st.llm_from.insert(agent, ts);
            }
            "assistant.turn_start" => {
                st.model = d["model"].as_str().map(str::to_owned).or(st.model.take());
                st.llm_from.insert(agent, ts);
            }
            "assistant.turn_end" if agent.is_empty() => {
                if let Some(t) = st.turn.as_deref().map(|t| span("turn", t)) {
                    let id = d["turnId"].as_str().unwrap_or_default();
                    e.end(format!("end:{t}:{id}"), &t, ts, SpanStatus::Succeeded, None);
                }
            }
            "abort" if agent.is_empty() => {
                if let Some(t) = st.turn.as_deref().map(|t| span("turn", t)) {
                    e.end(format!("end:{t}:abort"), &t, ts, SpanStatus::Cancelled, None);
                }
            }
            "assistant.message" => {
                let Some(id) = d["messageId"].as_str() else { return Ok(e.out) };
                let llm = span("llm", id);
                let model = d["model"].as_str().map(str::to_owned).or(st.model.clone());
                let from = st.llm_from.get(&agent).copied().unwrap_or(ts).min(ts);
                e.start(&llm, from, Some(&parent), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                    s.model = model.clone();
                    s.attributes.insert("start_inferred".into(), true.into());
                });
                if let Some(out) = d["outputTokens"].as_u64() {
                    let usage = Usage { tokens_output: Some(out), ..Usage::default() };
                    e.push(format!("{llm}:usage"), ts, Some(&llm), EventBody::UsageReported { usage, is_final: true });
                }
                e.end(format!("end:{llm}"), &llm, ts, SpanStatus::Succeeded, None);
            }
            "tool.execution_start" | "tool.user_requested" => {
                let (Some(call), Some(name)) = (d["toolCallId"].as_str(), d["toolName"].as_str()) else { return Ok(e.out) };
                e.start(&span("tool", call), ts, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
            }
            "tool.execution_complete" => {
                let Some(call) = d["toolCallId"].as_str() else { return Ok(e.out) };
                let tool = span("tool", call);
                let exit = d["shellExecution"]["exitCode"].as_i64().or_else(|| d["result"]["contents"].as_array()?.iter().find_map(|c| c["exitCode"].as_i64()));
                let failed = d["success"] == false || exit.is_some_and(|c| c != 0);
                let (status, error) = if failed { (SpanStatus::Failed, Some("tool reported a failure (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                e.end(format!("end:{tool}"), &tool, ts, status, error);
                st.llm_from.entry(agent).and_modify(|t| *t = (*t).max(ts)).or_insert(ts);
            }
            "subagent.started" => {
                let Some(call) = d["toolCallId"].as_str() else { return Ok(e.out) };
                if let Some(id) = v["agentId"].as_str() {
                    st.agents.insert(id.to_owned(), call.to_owned());
                }
                let name = d["agentName"].as_str().unwrap_or("subagent");
                e.start(&span("subagent", call), ts, Some(&span("tool", call)), SpanKind::Subagent, name, |s| {
                    s.agent_name = Some(name.to_owned());
                    s.model = d["model"].as_str().map(str::to_owned);
                });
                st.llm_from.insert(call.to_owned(), ts);
            }
            kind @ ("subagent.completed" | "subagent.failed") => {
                let Some(call) = d["toolCallId"].as_str() else { return Ok(e.out) };
                let status = match kind {
                    "subagent.failed" => SpanStatus::Failed,
                    _ if d["cancelled"] == true => SpanStatus::Cancelled,
                    _ => SpanStatus::Succeeded,
                };
                let sub = span("subagent", call);
                e.end(format!("end:{sub}"), &sub, ts, status, None);
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}
