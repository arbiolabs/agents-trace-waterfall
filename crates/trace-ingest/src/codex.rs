//! Codex CLI rollout JSONL (`~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`, compressed to
//! `.jsonl.zst` once old, and moved to `~/.codex/archived_sessions` when archived) to canonical events. Metadata only: no prompt, message, reasoning, command or tool output text is copied.
//!
//! Spans: `agent:<thread>` root, `turn:<turn id>` from `task_started` to `task_complete` or
//! `turn_aborted`, `llm:<response id>` per model response (ended by its usage record; it starts where
//! the model last got control: the turn start or the latest tool output), `tool:<call id>` from a
//! function or custom tool call to its output, and `item:<item id>` for each command, file change, MCP
//! call or web search Codex reports with its own start and end, nested under the open tool call. Event
//! ids derive from the rollout's ids and `source_seq` counts events from the top of the file.
//!
//! Sub-agents: a sub-agent thread writes its own rollout, but its events join the root session. The
//! parent opens `subagent:<thread>` under its `spawn_agent` call when Codex reports the agent started
//! and ends it when the agent completes; the child's turns nest inside it. A child rollout begins with a
//! copy of the parent's history (it is forked from the parent), which is skipped.

use serde_json::{Value, json};
use std::cell::RefCell;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

#[derive(Default)]
pub struct Codex {
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    thread: Option<String>,
    /// The root session's id; a sub-agent's events join it.
    session: Option<String>,
    subagent: bool,
    /// Inside the parent history a sub-agent rollout starts with.
    forked: bool,
    model: Option<String>,
    provider: Option<String>,
    turn: Option<String>,
    turns: u64,
    /// Where the next model response started: the turn start or the latest tool output.
    llm_from: Option<Ns>,
    responses: u64,
    /// Newer rollouts write `token_usage_record` per response; older ones only `token_count`.
    usage_records: bool,
    open_call: Option<String>,
}

impl TraceAdapter for Codex {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: "codex".into(),
            name: "codex".into(),
            harness: "codex".into(),
            unobservable: [
                "LLM call start time (inferred from the turn start or the preceding tool output)",
                "cost (no prices applied; provenance unknown)",
                "prompt, message, reasoning, command and tool input/output text (excluded by design)",
                "session end (no end marker; idle sessions are reconciled as interrupted)",
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
        let p = &v["payload"];
        let mut st = self.state.borrow_mut();
        if v["type"] == "session_meta" {
            match &st.thread {
                None => {
                    st.thread = p["id"].as_str().map(str::to_owned);
                    // Only a spawned thread has a parent to nest under; review and compaction threads
                    // (`source.subagent.review`, `.compact`, ...) stand as their own sessions.
                    st.subagent = p["source"]["subagent"]["thread_spawn"].is_object();
                    let own = p["id"].as_str();
                    st.session = if st.subagent { p["session_id"].as_str().or(own) } else { own }.map(str::to_owned);
                    st.provider = p["model_provider"].as_str().map(str::to_owned);
                }
                Some(own) if p["id"].as_str() != Some(own) => st.forked = true,
                Some(_) => {}
            }
        }
        let Some(thread) = st.thread.clone() else { return Ok(Vec::new()) };
        if st.forked {
            // The agent's own work starts with its thread settings, or a turn another agent started.
            let own = (p["type"] == "thread_settings_applied" && p["thread_id"].as_str() == Some(&thread))
                || (p["type"] == "task_started" && p["turn_attribution"]["initiating_agent_path"].is_string());
            if !own {
                return Ok(Vec::new());
            }
            st.forked = false;
        }
        let session = st.session.clone().unwrap_or_else(|| thread.clone());
        let mut e = Emit { source: SourceId(format!("codex:{thread}")), session: SessionId(session), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };
        let root = if st.subagent { span("subagent", &thread) } else { span("agent", &thread) };
        let parent = st.turn.as_deref().map_or_else(|| root.clone(), |t| span("turn", t));

        match (v["type"].as_str(), p["type"].as_str()) {
            (Some("session_meta"), _) if st.subagent || p["id"].as_str() != Some(&thread) => {}
            (Some("session_meta"), _) => {
                let cwd = p["cwd"].as_str().unwrap_or_default();
                let project = std::path::Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let mut labels = Attributes::new();
                for (k, label) in [("cli_version", "codex_version"), ("originator", "codex_originator")] {
                    if let Some(x) = p[k].as_str() {
                        labels.insert(label.into(), json!(x));
                    }
                }
                let at = p["timestamp"].as_str().and_then(parse_utc).unwrap_or(ts).min(ts);
                let body = EventBody::SessionStarted {
                    project_id: project.to_owned(),
                    working_directory: cwd.to_owned(),
                    source_adapter: "codex".into(),
                    harness: "codex".into(),
                    host: String::new(),
                    labels,
                };
                e.push(format!("session:{thread}"), at, None, body);
                e.start(&root, at, None, SpanKind::Agent, "codex", |s| s.agent_name = Some("main".into()));
            }
            (Some("turn_context"), _) => st.model = p["model"].as_str().map(str::to_owned).or(st.model.take()),
            (Some("event_msg"), Some("task_started")) => {
                let Some(id) = p["turn_id"].as_str() else { return Ok(e.out) };
                if let Some(old) = st.turn.take() {
                    let t = span("turn", &old);
                    e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
                }
                st.turns += 1;
                e.start(&span("turn", id), ts, Some(&root), SpanKind::Turn, &format!("turn {}", st.turns), |_| {});
                st.turn = Some(id.to_owned());
                st.llm_from = Some(ts);
            }
            (Some("event_msg"), Some(kind @ ("task_complete" | "turn_aborted"))) => {
                let status = if kind == "task_complete" { SpanStatus::Succeeded } else { SpanStatus::Cancelled };
                if let Some(old) = st.turn.take() {
                    let t = span("turn", &old);
                    e.end(format!("end:{t}"), &t, ts, status, None);
                    // A sub-agent ends with each turn it finishes, so it closes even when the parent never
                    // reports it done; the latest end wins, and a later turn reopens nothing it hides.
                    if st.subagent {
                        e.end(format!("end:{root}:{old}"), &root, ts, status, None);
                    }
                }
            }
            (Some("response_item"), Some(kind @ ("function_call" | "custom_tool_call" | "local_shell_call"))) => {
                let Some(call) = p["call_id"].as_str().or(p["id"].as_str()) else { return Ok(e.out) };
                let name = p["name"].as_str().unwrap_or(if kind == "local_shell_call" { "shell" } else { "tool" });
                e.start(&span("tool", call), ts, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                st.open_call = Some(call.to_owned());
            }
            (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
                let Some(call) = p["call_id"].as_str() else { return Ok(e.out) };
                let tool = span("tool", call);
                e.end(format!("end:{tool}"), &tool, ts, SpanStatus::Succeeded, None);
                if st.open_call.as_deref() == Some(call) {
                    st.open_call = None;
                }
                st.llm_from = Some(ts);
            }
            (Some("event_msg"), Some("item_completed")) if p["item"]["type"] == "SubAgentActivity" => {
                let item = &p["item"];
                let (Some(agent), Some(kind)) = (item["agent_thread_id"].as_str(), item["kind"].as_str()) else { return Ok(e.out) };
                let sub = span("subagent", agent);
                if kind == "started" {
                    let path = item["agent_path"].as_str().unwrap_or("subagent");
                    let name = path.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("subagent");
                    let under = item["id"].as_str().map_or_else(|| parent.clone(), |c| span("tool", c));
                    e.start(&sub, ts, Some(&under), SpanKind::Subagent, name, |s| {
                        s.agent_name = Some(path.to_owned());
                        s.model = item["model"].as_str().map(str::to_owned);
                    });
                } else {
                    let status = match kind {
                        "completed" => SpanStatus::Succeeded,
                        "errored" | "failed" => SpanStatus::Failed,
                        _ => SpanStatus::Cancelled,
                    };
                    e.end(format!("end:{sub}"), &sub, ts, status, None);
                }
            }
            (Some("event_msg"), Some(kind @ ("item_started" | "item_completed"))) => {
                let item = &p["item"];
                let Some((id, name)) = item["id"].as_str().zip(item_tool_name(item)) else { return Ok(e.out) };
                // Older items write 0 for a time they did not record.
                let ms = |k: &str| p[k].as_i64().filter(|m| *m > 0).and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
                let s = span("item", id);
                let under = st.open_call.as_deref().map_or_else(|| parent.clone(), |c| span("tool", c));
                e.start(&s, ms("started_at_ms").unwrap_or(ts), Some(&under), SpanKind::Tool, &name, |f| f.tool_name = Some(name.clone()));
                if kind == "item_completed" {
                    let failed = matches!(item["status"].as_str(), Some("failed" | "declined")) || item["exit_code"].as_i64().is_some_and(|c| c != 0);
                    let (status, error) = if failed { (SpanStatus::Failed, Some("tool reported a failure (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                    e.end(format!("end:{s}"), &s, ms("completed_at_ms").unwrap_or(ts), status, error);
                }
            }
            (Some("token_usage_record"), _) => {
                st.usage_records = true;
                let id = p["response_id"].as_str().map_or_else(|| format!("{thread}:{}", st.responses + 1), str::to_owned);
                response(&mut st, &mut e, &parent, &id, &p["usage"], ts);
            }
            (Some("event_msg"), Some("token_count")) if !st.usage_records && p["info"]["last_token_usage"].is_object() => {
                let id = format!("{thread}:{}", st.responses + 1);
                response(&mut st, &mut e, &parent, &id, &p["info"]["last_token_usage"], ts);
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

/// One model response: from where the model got control to its usage record, with OpenAI's token split
/// (`input_tokens` includes the cached ones) mapped onto fresh input and cache reads.
fn response(st: &mut State, e: &mut Emit, parent: &SpanId, id: &str, u: &Value, ts: Ns) {
    st.responses += 1;
    let llm = span("llm", id);
    let model = st.model.clone();
    e.start(&llm, st.llm_from.unwrap_or(ts).min(ts), Some(parent), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
        s.model = model.clone();
        s.provider = st.provider.clone();
        s.attributes.insert("start_inferred".into(), json!(true));
    });
    let input = u["input_tokens"].as_u64();
    let cached = u["cached_input_tokens"].as_u64();
    let usage = Usage {
        tokens_input: input.map(|i| i.saturating_sub(cached.unwrap_or(0))),
        tokens_output: u["output_tokens"].as_u64(),
        cache_read: cached,
        cache_write: u["cache_write_input_tokens"].as_u64(),
        cost_usd_micros: None,
        cost_provenance: CostProvenance::Unknown,
    };
    e.push(format!("{llm}:usage"), ts, Some(&llm), EventBody::UsageReported { usage, is_final: true });
    e.end(format!("end:{llm}"), &llm, ts, SpanStatus::Succeeded, None);
    st.llm_from = Some(ts);
}

/// The tool a thread item stands for, or None for conversation items. MCP calls read `mcp__server__tool`
/// like Claude Code's, so both frontends show the tool with its server.
fn item_tool_name(item: &Value) -> Option<String> {
    Some(match item["type"].as_str()? {
        // Sub-agent items repeat what the spawn and wait calls and the sub-agent span already show.
        "UserMessage" | "AgentMessage" | "Reasoning" | "Plan" | "TodoList" | "ContextCompaction" | "SubAgentActivity" | "CollabAgentToolCall" => return None,
        "CommandExecution" => "shell".into(),
        "FileChange" => "apply_patch".into(),
        "WebSearch" => "web_search".into(),
        "McpToolCall" => format!("mcp__{}__{}", item["server"].as_str()?, item["tool"].as_str()?),
        other => {
            // `ImageView` → `image_view`.
            let mut out = String::new();
            for (i, c) in other.chars().enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            }
            out
        }
    })
}
