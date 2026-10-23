//! Grok Build (xAI's `grok` CLI, npm `@xai-official/grok`) sessions to canonical events, from
//! `~/.grok/sessions/<url-encoded cwd>/<session id>/` (`$GROK_HOME/sessions`). Each session folder holds
//! `summary.json` (cwd, creation time, agent) and `updates.jsonl`, the ACP `session/update` stream Grok
//! appends as the session runs: `{"timestamp", "method", "params": {"sessionId", "update": {"sessionUpdate",
//! ...}, "_meta": {"eventId", "agentTimestampMs", "streamStartMs", ...}}}`. Metadata only: no prompt,
//! message, thought, command or tool input/output text is copied.
//!
//! The lines name neither the working directory nor a sub-agent's parent, so [`read`] walks the sessions
//! folder and feeds each changed session through the adapter whole: one `session` line built from
//! `summary.json` (and, for a sub-agent, its parent's `subagents/<child>/meta.json`), then its updates.
//! Event ids derive from Grok's session, prompt, stream and tool call ids, so re-reading a session that
//! grew yields the same events and the store drops what it already has.
//!
//! Spans: `agent:<session>` root; `turn:<session>:<prompt index>` from a `user_message_chunk` to the
//! `turn_completed` that ends it, with the turn's token usage; `llm:<session>:<streamStartMs>` per model
//! call, the updates it streamed (text, thoughts, tool calls) sharing one `streamStartMs`;
//! `tool:<session>:<call id>` from `tool_call` to the `tool_call_update` with a final status, failed
//! when that status is `failed` or a shell command's `rawOutput.exit_code` is non-zero; and
//! `tool:<session>:task:<id>` for a shell command the agent moved to the background, from the task's own
//! start and end times, failed on a non-zero exit.
//!
//! Sub-agents: each is a child session in the same tree. Its events join the root session inside
//! `subagent:<child>`, nested under the parent's `spawn_subagent` call that its `subagent_spawned` update
//! follows, and the sub-agent ends with its first finished turn.
//!
//! Gaps: Grok records tokens per turn, not per model call, so model calls carry no usage of their own; a
//! model call's request start is not written either, so it starts where the agent last finished
//! something (the prompt, a previous call, a tool), as Grok's own agent loop does.

use anyhow::Context;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Grok { harness, state: RefCell::default() })
}

struct Grok {
    harness: &'static str,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    /// This session's id, and the root session its events join (itself unless it is a sub-agent).
    own: Option<String>,
    root_session: Option<String>,
    /// The span this session's turns nest under: `agent:<id>`, or `subagent:<id>` for a sub-agent.
    top: Option<SpanId>,
    subagent: bool,
    model: Option<String>,
    turn: Option<SpanId>,
    prompt: Option<i64>,
    turns: u64,
    /// The open model call: its stream, span and the time of its latest streamed update.
    llm: Option<(i64, SpanId, Ns)>,
    /// When the agent last finished something; the next model call's request starts there.
    boundary: Ns,
    /// Tool names by call id, and the call and turn that moved each background task.
    tools: HashMap<String, String>,
    tasks: HashMap<String, (String, SpanId)>,
}

impl TraceAdapter for Grok {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.harness.into(),
            name: self.harness.into(),
            harness: self.harness.into(),
            unobservable: [
                "token usage per model call (Grok records it per turn; it sits on the turn span)",
                "LLM call request start (taken from when the agent last finished something)",
                "cost (Grok records none; estimated from model prices)",
                "prompt, message, thought, command and tool input/output text (excluded by design)",
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
        if v["t"] == "session" && st.own.is_none() {
            let Some(own) = v["id"].as_str() else { return Ok(Vec::new()) };
            let root = v["root"].as_str().unwrap_or(own);
            st.subagent = root != own;
            st.top = Some(if st.subagent { span("subagent", own) } else { span("agent", own) });
            st.own = Some(own.to_owned());
            st.root_session = Some(root.to_owned());
        }
        let (Some(own), Some(root_session), Some(top)) = (st.own.clone(), st.root_session.clone(), st.top.clone()) else { return Ok(Vec::new()) };
        let mut e = Emit { source: SourceId(format!("{}:{own}", self.harness)), session: SessionId(root_session.clone()), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };

        if v["t"] == "session" {
            let Some(ts) = v["created"].as_str().and_then(parse_utc) else { return Ok(Vec::new()) };
            let agent = v["agent"].as_str().unwrap_or(if st.subagent { "subagent" } else { "main" }).to_owned();
            if st.subagent {
                let under = v["parent_tool"].as_str().map_or_else(|| span("agent", &root_session), |call| span("tool", &format!("{}:{call}", v["parent"].as_str().unwrap_or(&root_session))));
                e.start(&top, ts, Some(&under), SpanKind::Subagent, &agent, |s| s.agent_name = Some(agent.clone()));
            } else {
                let cwd = v["cwd"].as_str().unwrap_or_default();
                let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let body = EventBody::SessionStarted {
                    project_id: project.to_owned(),
                    working_directory: cwd.to_owned(),
                    source_adapter: self.harness.into(),
                    harness: self.harness.into(),
                    host: String::new(),
                    labels: Attributes::new(),
                };
                e.push(format!("session:{own}"), ts, None, body);
                e.start(&top, ts, None, SpanKind::Agent, self.harness, |s| s.agent_name = Some(agent));
            }
            st.boundary = ts;
            st.seq = e.seq;
            return Ok(e.out);
        }

        let (u, meta) = (&v["params"]["update"], &v["params"]["_meta"]);
        let Some(ts) = ms(&meta["agentTimestampMs"]).or_else(|| v["timestamp"].as_i64().and_then(|s| s.checked_mul(1_000_000_000))) else { return Ok(Vec::new()) };
        let parent = st.turn.clone().unwrap_or_else(|| top.clone());
        match u["sessionUpdate"].as_str().unwrap_or_default() {
            "user_message_chunk" => {
                // A prompt arrives in chunks; the first one of a new prompt index starts its turn.
                let index = u["_meta"]["promptIndex"].as_i64().unwrap_or(st.turns as i64);
                if st.turn.is_some() && st.prompt == Some(index) {
                    st.seq = e.seq;
                    return Ok(e.out);
                }
                end_llm(&mut st, &mut e);
                if let Some(old) = st.turn.take() {
                    e.end(format!("end:{old}"), &old, ts, SpanStatus::Succeeded, None);
                }
                st.model = u["_meta"]["modelId"].as_str().map(str::to_owned).or(st.model.take());
                st.turns += 1;
                let turn = span("turn", &format!("{own}:{index}"));
                let model = st.model.clone();
                e.start(&turn, ts, Some(&top), SpanKind::Turn, &format!("turn {}", st.turns), |s| s.model = model);
                st.turn = Some(turn);
                st.prompt = Some(index);
                st.boundary = ts;
            }
            kind @ ("agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "plan") => {
                if let Some(stream) = meta["streamStartMs"].as_i64() {
                    if st.llm.as_ref().is_none_or(|(s, _, _)| *s != stream) {
                        end_llm(&mut st, &mut e);
                        let llm = span("llm", &format!("{own}:{stream}"));
                        let start = st.boundary.min(ms(&json!(stream)).unwrap_or(ts));
                        let model = st.model.clone();
                        let name = model.clone().unwrap_or_else(|| "llm call".into());
                        e.start(&llm, start, Some(&parent), SpanKind::Llm, &name, |s| s.model = model);
                        st.llm = Some((stream, llm, ts));
                    }
                    if let Some((_, _, last)) = st.llm.as_mut() {
                        *last = (*last).max(ts);
                    }
                }
                if kind == "tool_call"
                    && let Some(call) = u["toolCallId"].as_str()
                {
                    let name = u["_meta"]["x.ai/tool"]["name"].as_str().or(u["title"].as_str()).unwrap_or("tool").to_owned();
                    let tool = span("tool", &format!("{own}:{call}"));
                    e.start(&tool, ts, Some(&parent), SpanKind::Tool, &name, |s| s.tool_name = Some(name.clone()));
                    st.tools.insert(call.to_owned(), name);
                    end_tool(&mut st, &mut e, &own, call, u, ts);
                }
            }
            "tool_call_update" => {
                if let Some(call) = u["toolCallId"].as_str() {
                    end_tool(&mut st, &mut e, &own, call, u, ts);
                }
            }
            "task_backgrounded" => {
                if let (Some(task), Some(call)) = (u["task_id"].as_str(), u["tool_call_id"].as_str()) {
                    let name = st.tools.get(call).cloned().unwrap_or_else(|| "shell".into());
                    st.tasks.insert(task.to_owned(), (name, parent));
                }
            }
            "task_completed" => {
                let t = &u["task_snapshot"];
                let at = |x: &Value| x["secs_since_epoch"].as_i64().and_then(|s| s.checked_mul(1_000_000_000)).and_then(|s| s.checked_add(x["nanos_since_epoch"].as_i64().unwrap_or(0)));
                if let (Some(task), Some(start)) = (t["task_id"].as_str(), at(&t["start_time"])) {
                    let (name, under) = st.tasks.get(task).cloned().unwrap_or_else(|| ("shell".into(), parent));
                    let tool = span("tool", &format!("{own}:task:{task}"));
                    e.start(&tool, start, Some(&under), SpanKind::Tool, &name, |s| s.tool_name = Some(name.clone()));
                    let (status, error) = match t["exit_code"].as_i64() {
                        _ if t["explicitly_killed"] == true => (SpanStatus::Cancelled, None),
                        Some(0) => (SpanStatus::Succeeded, None),
                        Some(_) => (SpanStatus::Failed, Some("command exited non-zero (output not captured)")),
                        None if t["signal"].is_null() => (SpanStatus::Succeeded, None),
                        None => (SpanStatus::Failed, Some("command killed by a signal")),
                    };
                    e.end(format!("end:{tool}"), &tool, at(&t["end_time"]).unwrap_or(ts).max(start), status, error);
                }
            }
            "turn_completed" => {
                end_llm(&mut st, &mut e);
                if let Some(turn) = st.turn.take() {
                    let usage = &u["usage"];
                    if usage.is_object() {
                        let cached = usage["cachedReadTokens"].as_u64();
                        let usage = Usage {
                            // Like OpenAI's prompt tokens, `inputTokens` includes the cached ones.
                            tokens_input: usage["inputTokens"].as_u64().map(|i| i.saturating_sub(cached.unwrap_or(0))),
                            tokens_output: usage["outputTokens"].as_u64(),
                            cache_read: cached,
                            cache_write: usage["cacheCreationTokens"].as_u64(),
                            cost_usd_micros: None,
                            cost_provenance: CostProvenance::Unknown,
                        };
                        e.push(format!("{turn}:usage"), ts, Some(&turn), EventBody::UsageReported { usage, is_final: true });
                    }
                    let (status, error) = match u["stop_reason"].as_str() {
                        Some("cancelled") => (SpanStatus::Cancelled, None),
                        Some("refusal") => (SpanStatus::Failed, Some("the model refused")),
                        _ => (SpanStatus::Succeeded, None),
                    };
                    e.end(format!("end:{turn}"), &turn, ts, status, error);
                    if st.subagent {
                        e.end(format!("end:{top}"), &top, ts, status, error);
                    }
                }
                st.boundary = ts;
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

fn ms(x: &Value) -> Option<Ns> {
    x.as_i64().and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t))
}

/// Ends the open model call at its latest streamed update.
fn end_llm(st: &mut State, e: &mut Emit) {
    if let Some((_, llm, last)) = st.llm.take() {
        e.end(format!("end:{llm}"), &llm, last, SpanStatus::Succeeded, None);
        st.boundary = st.boundary.max(last);
    }
}

/// Ends a tool call once its ACP status is final (`completed`, `failed`, `cancelled`). A shell command
/// that ran to a non-zero exit or timed out is `completed` in Grok's terms, with its `exit_code` in
/// `rawOutput`: that is a failure.
fn end_tool(st: &mut State, e: &mut Emit, own: &str, call: &str, u: &Value, at: Ns) {
    let out = &u["rawOutput"];
    let nonzero = out["exit_code"].as_i64().is_some_and(|c| c != 0) || out["timed_out"] == true;
    let (status, error) = match u["status"].as_str().map(str::to_ascii_lowercase).as_deref() {
        Some("completed") if nonzero => (SpanStatus::Failed, Some("command exited non-zero (output not captured)")),
        Some("completed") => (SpanStatus::Succeeded, None),
        Some("failed") => (SpanStatus::Failed, Some("tool reported an error (output not captured)")),
        Some("cancelled" | "canceled") => (SpanStatus::Cancelled, None),
        _ => return,
    };
    let tool = span("tool", &format!("{own}:{call}"));
    e.end(format!("end:{tool}"), &tool, at, status, error);
    st.boundary = st.boundary.max(at);
}

/// Events for every session whose `updates.jsonl` changed after `since_ms` (epoch milliseconds of its
/// modification time), and the newest change seen. `path` is `~/.grok`, its `sessions` folder, or one
/// session folder.
// ponytail: a changed session is re-read whole, and every poll stats every session folder; keep byte
// offsets per session if long sessions make polls slow.
pub fn read(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.exists(), "{} does not exist; point it at ~/.grok/sessions", path.display());
    let root = if path.join("sessions").is_dir() { path.join("sessions") } else { path.to_owned() };
    let sessions: Vec<PathBuf> = if root.join("updates.jsonl").is_file() {
        vec![root.clone()]
    } else {
        subdirs(&root).iter().flat_map(|g| subdirs(g)).filter(|d| d.join("updates.jsonl").is_file()).collect()
    };
    // Sub-agents: each parent lists its children under `subagents/<child>/`.
    let by_id: HashMap<String, &PathBuf> = sessions.iter().filter_map(|d| Some((d.file_name()?.to_str()?.to_owned(), d))).collect();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for d in &sessions {
        for child in subdirs(&d.join("subagents")) {
            if let (Some(c), Some(p)) = (child.file_name().and_then(|n| n.to_str()), d.file_name().and_then(|n| n.to_str())) {
                parent_of.insert(c.to_owned(), p.to_owned());
            }
        }
    }
    let mut newest = since_ms;
    let mut events = Vec::new();
    for dir in &sessions {
        let changed = mtime_ms(&dir.join("updates.jsonl"));
        if changed <= since_ms {
            continue;
        }
        newest = newest.max(changed);
        let Some(id) = dir.file_name().and_then(|n| n.to_str()) else { continue };
        let summary = read_json(&dir.join("summary.json")).unwrap_or_default();
        let mut line = json!({"t": "session", "id": id, "cwd": summary["info"]["cwd"], "created": summary["created_at"], "agent": summary["agent_name"]});
        if let Some(parent) = parent_of.get(id) {
            let mut root = parent.clone();
            // Bounded, so a corrupt cycle cannot hang the collector.
            for _ in 0..64 {
                match parent_of.get(&root) {
                    Some(up) => root = up.clone(),
                    None => break,
                }
            }
            line["root"] = json!(root);
            line["parent"] = json!(parent);
            line["parent_tool"] = json!(by_id.get(parent).and_then(|p| spawning_call(&p.join("updates.jsonl"), id)));
        }
        let adapter = adapter(harness);
        let mut feed = |line: &str| -> anyhow::Result<()> {
            events.extend(adapter.normalize(RawEvent { line: line.to_owned(), observed_at_utc_ns: system_now_ns() })?);
            Ok(())
        };
        feed(&line.to_string())?;
        let text = std::fs::read_to_string(dir.join("updates.jsonl")).with_context(|| format!("reading {}", dir.display()))?;
        // The last line may still be half written; the next poll reads it whole.
        for l in text.lines().filter(|l| serde_json::from_str::<Value>(l).is_ok()) {
            feed(l)?;
        }
    }
    Ok((events, newest))
}

/// The parent's `spawn_subagent` call that started `child`: Grok writes each `subagent_spawned` after
/// the call that asked for it, so spawns pair with those calls in order.
fn spawning_call(updates: &Path, child: &str) -> Option<String> {
    let text = std::fs::read_to_string(updates).ok()?;
    let mut pending = std::collections::VecDeque::new();
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let u = &v["params"]["update"];
        match u["sessionUpdate"].as_str() {
            Some("tool_call") if u["_meta"]["x.ai/tool"]["name"] == "spawn_subagent" || u["title"] == "spawn_subagent" => pending.push_back(u["toolCallId"].as_str()?.to_owned()),
            Some("subagent_spawned") => {
                let call = pending.pop_front();
                if u["child_session_id"] == child || u["subagent_id"] == child {
                    return call;
                }
            }
            _ => {}
        }
    }
    None
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    out
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}
