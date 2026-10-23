//! Claude Code transcript JSONL (`~/.claude/projects/<project>/<session>.jsonl` and
//! `<session>/subagents/agent-<id>.jsonl`) to canonical events. Metadata only: no prompt,
//! assistant, thinking or tool payload text is copied.
//!
//! Spans: `agent:<session>` root, `turn:<promptId>` per user prompt (`turn N`; a background task's
//! notification, a `!` shell command or a compaction summary also starts one, named for what it is), `llm:<message id>` per API
//! response (one response spans several transcript lines, one per content block), `tool:<tool_use id>`
//! ended by its tool_result, `subagent:<agentId>` started by the parent transcript when the Agent
//! tool result names the agent, named by the short description its Agent call gave it, with its
//! agent type. Event ids derive from transcript uuids and ids, and `source_seq`
//! counts events from the top of the file, so re-reading a file yields identical events.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

#[derive(Default)]
pub struct ClaudeCode {
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
    llm_message: Option<String>,
    tool_starts: HashMap<String, Ns>,
    /// Agent tool calls by id: the description and agent type they launched a sub-agent with.
    agent_calls: HashMap<String, (Option<String>, Option<String>)>,
    /// Sub-agent descriptions by agent id, for naming the notifications they send back.
    agent_names: HashMap<String, String>,
}

/// What started a turn, from a user record that is not a tool result: a typed prompt (None), or the
/// name of the record that stands in for one. Only the record's kind is read, never its text.
fn turn_origin(v: &Value, names: &HashMap<String, String>) -> Option<String> {
    let content = &v["message"]["content"];
    let text = content.as_str().or_else(|| content.as_array()?.iter().find_map(|b| b["text"].as_str())).unwrap_or_default().trim_start();
    if v["origin"]["kind"] == "task-notification" || text.starts_with("<task-notification>") {
        // The task id is an id, not content: it names which sub-agent reported back.
        let id = text.split_once("<task-id>").and_then(|(_, r)| r.split_once("</task-id>")).map(|(id, _)| id.trim());
        return Some(match id.and_then(|id| names.get(id)) {
            Some(name) => format!("Notification · {name}"),
            None => "Task notification".into(),
        });
    }
    if v["isCompactSummary"] == true {
        return Some("Context compacted".into());
    }
    if text.starts_with("<bash-input>") {
        return Some("Shell command".into());
    }
    if text.starts_with("<command-name>") || text.starts_with("<local-command") {
        return Some("Command".into());
    }
    None
}

/// A sub-agent's description as a span name: one line, at most 80 characters.
fn short_label(s: &str) -> Option<String> {
    let line = s.lines().next()?.trim();
    (!line.is_empty()).then(|| line.chars().take(80).collect())
}

impl TraceAdapter for ClaudeCode {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: "claude-code".into(),
            name: "claude-code".into(),
            harness: "claude-code".into(),
            unobservable: [
                "LLM call start time (inferred from the preceding transcript line)",
                "time to first token",
                "cost (no prices applied; provenance unknown)",
                "prompt, assistant, thinking and tool input/output text (excluded by design)",
                "tool execution start (taken from the requesting assistant message)",
                "session end (no end marker; idle sessions are reconciled as interrupted)",
                "subagent parent link and end (only when the parent transcript records the launch and the agent ends its turn)",
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
        let mut st = self.state.borrow_mut();
        let Some(ts) = v["timestamp"].as_str().and_then(parse_utc).filter(|t| VALID_TIME_NS.contains(t)) else {
            return Ok(Vec::new());
        };
        let prev = st.prev_ts.replace(ts).unwrap_or(ts);
        let (Some(sid), Some(uuid)) = (v["sessionId"].as_str(), v["uuid"].as_str()) else {
            return Ok(Vec::new());
        };
        let agent = v["agentId"].as_str().filter(|_| v["isSidechain"] == true);
        let source = match agent {
            Some(a) => format!("claude-code:{sid}:{a}"),
            None => format!("claude-code:{sid}"),
        };
        let mut e = Emit {
            source: SourceId(source),
            session: SessionId(sid.to_owned()),
            observed: raw.observed_at_utc_ns,
            seq: st.seq,
            out: Vec::new(),
        };
        let root = span("agent", sid);

        if agent.is_none()
            && !st.session_started
            && let Some(cwd) = v["cwd"].as_str()
        {
            st.session_started = true;
            let project = std::path::Path::new(cwd)
                .file_name()
                .map_or(cwd, |n| n.to_str().unwrap_or(cwd));
            let mut labels = Attributes::new();
            if let Some(ver) = v["version"].as_str() {
                labels.insert("claude_code_version".into(), json!(ver));
            }
            let body = EventBody::SessionStarted {
                project_id: project.to_owned(),
                working_directory: cwd.to_owned(),
                source_adapter: "claude-code".into(),
                harness: "claude-code".into(),
                host: String::new(),
                labels,
            };
            e.push(format!("session:{sid}"), ts, None, body);
            e.start(&root, ts, None, SpanKind::Agent, "claude-code", |s| {
                s.agent_name = Some("main".into())
            });
        }

        let content = v["message"]["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let blocks = |kind: &'static str| content.iter().filter(move |b| b["type"] == kind);
        let parent = match (agent, &st.turn) {
            (Some(a), _) => span("subagent", a),
            (None, Some(t)) if st.turn_open => span("turn", t),
            _ => root.clone(),
        };

        match v["type"].as_str() {
            Some("user") => {
                let results: Vec<_> = blocks("tool_result").collect();
                for r in &results {
                    let Some(id) = r["tool_use_id"].as_str() else {
                        continue;
                    };
                    let tool = span("tool", id);
                    let failed = r["is_error"] == true;
                    let (status, error) = if failed {
                        (
                            SpanStatus::Failed,
                            Some("tool reported an error (output not captured)"),
                        )
                    } else {
                        (SpanStatus::Succeeded, None)
                    };
                    e.end(format!("end:{tool}"), &tool, ts, status, error);
                    if let Some(a) = v["toolUseResult"]["agentId"].as_str() {
                        let at = st.tool_starts.get(id).copied().unwrap_or(ts);
                        let (description, kind) = st.agent_calls.get(id).cloned().unwrap_or_default();
                        if let Some(d) = &description {
                            st.agent_names.insert(a.to_owned(), d.clone());
                        }
                        e.start(
                            &span("subagent", a),
                            at,
                            Some(&tool),
                            SpanKind::Subagent,
                            description.as_deref().unwrap_or("subagent"),
                            |s| s.agent_name = Some(kind.unwrap_or_else(|| a.to_owned())),
                        );
                    }
                }
                let key = v["promptId"].as_str().unwrap_or(uuid);
                let text = v["message"]["content"].as_str().or_else(|| content.iter().find_map(|b| b["text"].as_str())).unwrap_or_default();
                // A shell command's output arrives as its own record: it belongs to the command's turn.
                let output = ["<bash-stdout>", "<bash-stderr>", "<local-command-stdout>"].iter().any(|t| text.trim_start().starts_with(t));
                if results.is_empty()
                    && agent.is_none()
                    && v["isMeta"] != true
                    && !output
                    && st.turn.as_deref() != Some(key)
                {
                    if let Some(old) = st.turn.take().filter(|_| st.turn_open) {
                        let t = span("turn", &old);
                        e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
                    }
                    let name = turn_origin(&v, &st.agent_names).unwrap_or_else(|| {
                        st.turns += 1;
                        format!("turn {}", st.turns)
                    });
                    e.start(&span("turn", key), ts, Some(&root), SpanKind::Turn, &name, |_| {});
                    st.turn = Some(key.to_owned());
                    st.turn_open = true;
                }
            }
            Some("assistant") => {
                let msg = &v["message"];
                let mid = msg["id"].as_str().unwrap_or(uuid);
                let llm = span("llm", mid);
                if st.llm_message.as_deref() != Some(mid) {
                    st.llm_message = Some(mid.to_owned());
                    let model = msg["model"].as_str();
                    e.start(
                        &llm,
                        prev,
                        Some(&parent),
                        SpanKind::Llm,
                        model.unwrap_or("llm call"),
                        |s| {
                            s.model = model.map(str::to_owned);
                            s.provider = Some("anthropic".into());
                            s.attributes.insert("start_inferred".into(), json!(true));
                        },
                    );
                }
                let u = &msg["usage"];
                if u.is_object() {
                    let usage = Usage {
                        tokens_input: u["input_tokens"].as_u64(),
                        tokens_output: u["output_tokens"].as_u64(),
                        cache_read: u["cache_read_input_tokens"].as_u64(),
                        cache_write: u["cache_creation_input_tokens"].as_u64(),
                        cost_usd_micros: None,
                        cost_provenance: CostProvenance::Unknown,
                    };
                    e.push(
                        format!("{uuid}:usage"),
                        ts,
                        Some(&llm),
                        EventBody::UsageReported {
                            usage,
                            is_final: true,
                        },
                    );
                }
                let status = if v["isApiErrorMessage"] == true {
                    SpanStatus::Failed
                } else {
                    SpanStatus::Succeeded
                };
                e.end(format!("{uuid}:end"), &llm, ts, status, None);
                for b in blocks("tool_use") {
                    let (Some(id), Some(name)) = (b["id"].as_str(), b["name"].as_str()) else {
                        continue;
                    };
                    st.tool_starts.insert(id.to_owned(), ts);
                    if matches!(name, "Agent" | "Task") {
                        let input = &b["input"];
                        st.agent_calls.insert(id.to_owned(), (input["description"].as_str().and_then(short_label), input["subagent_type"].as_str().map(str::to_owned)));
                    }
                    e.start(
                        &span("tool", id),
                        ts,
                        Some(&parent),
                        SpanKind::Tool,
                        name,
                        |s| s.tool_name = Some(name.to_owned()),
                    );
                }
                if let Some(a) = agent
                    && msg["stop_reason"] == "end_turn"
                {
                    e.end(
                        format!("{uuid}:subagent-end"),
                        &span("subagent", a),
                        ts,
                        SpanStatus::Succeeded,
                        None,
                    );
                }
            }
            Some("system")
                if v["subtype"] == "turn_duration" && agent.is_none() && st.turn_open =>
            {
                st.turn_open = false;
                let t = span("turn", st.turn.as_deref().expect("open turn has a key"));
                e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

#[cfg(test)]
mod tests {
    use crate::emit::parse_utc;

    #[test]
    fn parses_claude_timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_utc("2026-01-01T00:00:00Z"),
            Some(1_767_225_600_000_000_000)
        );
        assert_eq!(
            parse_utc("2026-10-22T15:27:19.656Z"),
            Some(1_792_682_839_656_000_000)
        );
        assert_eq!(
            parse_utc("2024-02-29T23:59:59.000000001Z"),
            Some(1_709_251_199_000_000_001)
        );
        assert_eq!(parse_utc("2026-10-22 15:27:19Z"), None);
        assert_eq!(parse_utc("2026-10-22T15:27:19"), None);
        assert_eq!(parse_utc("2262-04-11T23:47:16Z"), Some(9_223_372_036_000_000_000));
        assert_eq!(parse_utc("2263-01-01T00:00:00Z"), None);
        assert_eq!(parse_utc("99999999999999-01-01T00:00:00Z"), None);
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
    }
}
