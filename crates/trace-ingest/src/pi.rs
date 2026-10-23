//! pi coding agent session JSONL (`~/.pi/agent/sessions/<cwd>/<time>_<session>.jsonl`, format version 3)
//! to canonical events. Metadata only: no prompt, message, thinking or tool payload text is copied.
//!
//! Spans: `agent:<session>` root, `turn:<entry id>` from a user message until an assistant message that
//! stops for any reason but a tool call, `llm:<entry id>` per assistant message (pi records when the
//! request started and how long it took, and its own cost estimate), and `tool:<call id>` from the
//! requesting assistant message to its tool result. Event ids derive from entry and call ids, and
//! `source_seq` counts events from the top of the file.
//!
//! Branches: entries form a tree, and going back to continue from an earlier message starts a branch in
//! the same file. A turn that does is named `turn N (branch)`; turns stay in time order.
//!
//! Sub-agents: pi has none built in, and packages add them two ways. pi's own sub-agent extension runs
//! each agent as a separate pi process without a session file and records the agent's messages in the
//! tool result's `details.results[].messages`; each result becomes `subagent:<call>:<n>` under the tool
//! call, once the tool result is written. Packages that give each agent its own session start it from
//! the parent (`parentSession` in its header names the parent's file): a session that starts while a
//! tool call of its parent is running is that call's sub-agent, and joins the root session as
//! `subagent:<session>` under it, live. A session started between turns is a fork or clone and stays
//! its own session. Either way, the parent history such a session begins with is skipped.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

/// Also reads Oh My Pi (`omp`, a pi fork that keeps the same session format in `~/.omp/agent/sessions`);
/// `harness` names which one recorded the session.
pub struct Pi {
    state: RefCell<State>,
    harness: &'static str,
}

impl Default for Pi {
    fn default() -> Self {
        Self::recorded_by("pi")
    }
}

impl Pi {
    pub fn recorded_by(harness: &'static str) -> Self {
        Self { state: RefCell::default(), harness }
    }
}

#[derive(Default)]
struct State {
    seq: u64,
    session: Option<String>,
    /// The session its events join (itself unless it is a sub-agent) and the span its turns nest under.
    root_session: Option<String>,
    top: Option<SpanId>,
    subagent: bool,
    /// When a session started from another one began; entries before it are the copied parent history.
    copied_before: Option<Ns>,
    /// The latest conversation entry; a user message that continues from another one starts a branch.
    leaf: Option<String>,
    turn: Option<String>,
    turns: u64,
    /// When each open tool call started: the end of the model call that asked for it.
    tool_starts: HashMap<String, Ns>,
}

impl TraceAdapter for Pi {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.harness.into(),
            name: self.harness.into(),
            harness: self.harness.into(),
            unobservable: [
                "tool execution start (taken from the requesting assistant message)",
                "cost provenance (pi's own estimate from model prices)",
                "prompt, message, thinking and tool input/output text (excluded by design)",
                "sub-agents of pi's sub-agent extension until their tool result is written",
                "sub-agents that keep neither a session file nor their messages in the tool result",
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
        let mut st = self.state.borrow_mut();
        let mut spawned_from: Option<String> = None;
        if v["type"] == "session" && st.session.is_none() {
            let Some(own) = v["id"].as_str() else { return Ok(Vec::new()) };
            st.session = Some(own.to_owned());
            st.root_session = Some(own.to_owned());
            st.top = Some(span("agent", own));
            if let Some(parent) = v["parentSession"].as_str() {
                st.copied_before = Some(ts);
                if let Some((root, call)) = spawned_by(parent, ts, 0) {
                    st.subagent = true;
                    st.root_session = Some(root);
                    st.top = Some(span("subagent", own));
                    spawned_from = Some(call);
                }
            }
        }
        let (Some(sid), Some(root_session), Some(root)) = (st.session.clone(), st.root_session.clone(), st.top.clone()) else { return Ok(Vec::new()) };
        if v["type"] != "session" && st.copied_before.is_some_and(|start| ts < start) {
            return Ok(Vec::new());
        }
        let mut e = Emit { source: SourceId(format!("{}:{sid}", self.harness)), session: SessionId(root_session), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };
        let parent = st.turn.as_deref().map_or_else(|| root.clone(), |t| span("turn", t));
        let m = &v["message"];

        let branched = v["type"] == "message"
            && m["role"] == "user"
            && st.leaf.is_some()
            && v["parentId"].as_str() != st.leaf.as_deref();
        // Usage entries sit outside the conversation tree.
        if v["type"] != "usage"
            && let Some(id) = v["id"].as_str()
        {
            st.leaf = Some(id.to_owned());
        }
        match (v["type"].as_str(), m["role"].as_str()) {
            (Some("session"), _) if st.subagent => {
                let under = spawned_from.map(|c| span("tool", &c));
                e.start(&root, ts, under.as_ref(), SpanKind::Subagent, "subagent", |s| s.agent_name = Some(sid.clone()));
            }
            (Some("session"), _) => {
                let cwd = v["cwd"].as_str().unwrap_or_default();
                let project = std::path::Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let mut labels = Attributes::new();
                if let Some(parent) = v["parentSession"].as_str() {
                    labels.insert("pi_parent_session".into(), json!(parent));
                }
                let body = EventBody::SessionStarted {
                    project_id: project.to_owned(),
                    working_directory: cwd.to_owned(),
                    source_adapter: self.harness.into(),
                    harness: self.harness.into(),
                    host: String::new(),
                    labels,
                };
                e.push(format!("session:{sid}"), ts, None, body);
                e.start(&root, ts, None, SpanKind::Agent, self.harness, |s| s.agent_name = Some("main".into()));
            }
            (Some("message"), Some("user")) => {
                let Some(id) = v["id"].as_str() else { return Ok(e.out) };
                if let Some(old) = st.turn.take() {
                    let t = span("turn", &old);
                    e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
                }
                st.turns += 1;
                let name = if branched { format!("turn {} (branch)", st.turns) } else { format!("turn {}", st.turns) };
                e.start(&span("turn", id), ts, Some(&root), SpanKind::Turn, &name, |s| {
                    if branched {
                        s.attributes.insert("branch_from".into(), json!(v["parentId"]));
                    }
                });
                st.turn = Some(id.to_owned());
            }
            (Some("message"), Some("assistant")) => {
                let Some(id) = v["id"].as_str() else { return Ok(e.out) };
                let (end, status, stop) = assistant(&mut e, m, id, "", &parent, ts, &mut st.tool_starts);
                if !matches!(stop, "toolUse" | "pending" | "deferred")
                    && let Some(old) = st.turn.take()
                {
                    let t = span("turn", &old);
                    e.end(format!("end:{t}"), &t, end, status, None);
                    // A sub-agent ends with each turn it finishes; the latest end wins.
                    if st.subagent {
                        e.end(format!("end:{root}:{old}"), &root, end, status, None);
                    }
                }
            }
            (Some("message"), Some("toolResult")) => {
                let Some(call) = m["toolCallId"].as_str() else { return Ok(e.out) };
                let tool = span("tool", call);
                let (status, error) = if m["isError"] == true { (SpanStatus::Failed, Some("tool reported an error (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                let at = tool_end(m, st.tool_starts.remove(call), ts);
                e.end(format!("end:{tool}"), &tool, at, status, error);
                for (n, r) in m["details"]["results"].as_array().into_iter().flatten().enumerate() {
                    subagent(&mut e, r, call, n, &tool);
                }
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

/// One assistant message as a model call, with its tool calls started at its end. `scope` keeps a
/// sub-agent's ids apart from the session's. Returns the call's end, status and stop reason.
/// A tool call's end: its start plus the `durationMs` pi measured for it. The result's own timestamp is
/// only the fallback: pi runs a batch of tools in parallel and writes every result once the batch is
/// done, so each timestamp is roughly the batch's end.
fn tool_end(m: &Value, start: Option<Ns>, fallback: Ns) -> Ns {
    let ms = |x: &Value| x.as_i64().and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
    let written = ms(&m["timestamp"]).unwrap_or(fallback);
    start.zip(m["durationMs"].as_i64()).and_then(|(s, d)| s.checked_add(d * 1_000_000)).map_or(written, |end| end.min(written))
}

#[allow(clippy::too_many_arguments)]
fn assistant<'a>(e: &mut Emit, m: &'a Value, id: &str, scope: &str, parent: &SpanId, fallback: Ns, starts: &mut HashMap<String, Ns>) -> (Ns, SpanStatus, &'a str) {
    let ms = |x: &Value| x.as_i64().and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
    let llm = span("llm", &format!("{scope}{id}"));
    let start = ms(&m["timestamp"]).unwrap_or(fallback).min(fallback);
    // pi writes `durationMs`; Oh My Pi writes `duration` (fractional milliseconds) and `completedAt`.
    let duration = m["durationMs"].as_f64().or_else(|| m["duration"].as_f64());
    let end = duration.and_then(|d| start.checked_add((d * 1e6) as Ns)).or_else(|| ms(&m["completedAt"])).unwrap_or(fallback);
    let model = m["model"].as_str();
    e.start(&llm, start, Some(parent), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
        s.model = model.map(str::to_owned);
        s.provider = m["provider"].as_str().map(str::to_owned);
    });
    let u = &m["usage"];
    if u.is_object() {
        let cost = u["cost"]["total"].as_f64().filter(|c| *c > 0.).map(|c| (c * 1e6).round() as u64);
        let usage = Usage {
            tokens_input: u["input"].as_u64(),
            tokens_output: u["output"].as_u64(),
            cache_read: u["cacheRead"].as_u64(),
            cache_write: u["cacheWrite"].as_u64(),
            cost_usd_micros: cost,
            cost_provenance: if cost.is_some() { CostProvenance::Estimate } else { CostProvenance::Unknown },
        };
        e.push(format!("{llm}:usage"), end, Some(&llm), EventBody::UsageReported { usage, is_final: true });
    }
    let stop = m["stopReason"].as_str().unwrap_or("stop");
    let (status, error) = match stop {
        "error" => (SpanStatus::Failed, Some(m["errorMessage"].as_str().unwrap_or("the model call failed"))),
        "aborted" => (SpanStatus::Cancelled, None),
        _ => (SpanStatus::Succeeded, None),
    };
    e.end(format!("end:{llm}"), &llm, end, status, error);
    for c in m["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "toolCall") {
        let (Some(call), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else { continue };
        e.start(&span("tool", &format!("{scope}{call}")), end, Some(parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
        starts.insert(call.to_owned(), end);
    }
    (end, status, stop)
}

/// One sub-agent run the sub-agent extension recorded in a tool result: its model calls and tools under
/// `subagent:<call>:<n>`, from its first message to its last.
fn subagent(e: &mut Emit, r: &Value, call: &str, n: usize, tool: &SpanId) {
    let Some(messages) = r["messages"].as_array() else { return };
    let ms = |x: &Value| x.as_i64().and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
    let times: Vec<Ns> = messages.iter().filter_map(|m| ms(&m["timestamp"])).collect();
    let (Some(&first), Some(&last)) = (times.iter().min(), times.iter().max()) else { return };
    let scope = format!("{call}:{n}:");
    let sub = span("subagent", &format!("{call}:{n}"));
    let agent = r["agent"].as_str().unwrap_or("subagent");
    e.start(&sub, first, Some(tool), SpanKind::Subagent, agent, |s| {
        s.agent_name = Some(agent.to_owned());
        s.model = r["model"].as_str().map(str::to_owned);
    });
    let mut end = last;
    let mut starts = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        match m["role"].as_str() {
            Some("assistant") => end = end.max(assistant(e, m, &i.to_string(), &scope, &sub, ms(&m["timestamp"]).unwrap_or(last), &mut starts).0),
            Some("toolResult") => {
                let Some(id) = m["toolCallId"].as_str() else { continue };
                let t = span("tool", &format!("{scope}{id}"));
                let (status, error) = if m["isError"] == true { (SpanStatus::Failed, Some("tool reported an error (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                e.end(format!("end:{t}"), &t, tool_end(m, starts.remove(id), last), status, error);
            }
            _ => {}
        }
    }
    let status = match r["stopReason"].as_str() {
        _ if r["exitCode"].as_i64().is_some_and(|c| c != 0) => SpanStatus::Failed,
        Some("error") => SpanStatus::Failed,
        Some("aborted") => SpanStatus::Cancelled,
        _ => SpanStatus::Succeeded,
    };
    let error = (status == SpanStatus::Failed).then(|| r["errorMessage"].as_str().unwrap_or("the sub-agent failed"));
    e.end(format!("end:{sub}"), &sub, end.max(first), status, error);
}

/// For a session started from the one in `parent_path`: the root session it joins and the parent's tool
/// call that was running when it started at `at`. None for a fork or clone made between turns, or a
/// parent file that cannot be read.
fn spawned_by(parent_path: &str, at: Ns, depth: u8) -> Option<(String, String)> {
    if depth > 8 {
        return None;
    }
    let ms = |x: &Value| x.as_i64().and_then(|m| m.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
    let text = std::fs::read_to_string(parent_path).ok()?;
    let mut entries = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let header = entries.next().filter(|h| h["type"] == "session")?;
    let parent = header["id"].as_str()?.to_owned();
    let mut started: Vec<(Ns, String)> = Vec::new();
    let mut ended: HashMap<String, Ns> = HashMap::new();
    for v in entries {
        let (m, ts) = (&v["message"], v["timestamp"].as_str().and_then(parse_utc));
        match m["role"].as_str() {
            Some("assistant") => {
                let start = ms(&m["timestamp"]).or(ts);
                let end = start.zip(m["durationMs"].as_i64()).and_then(|(s, d)| s.checked_add(d * 1_000_000)).or(ts);
                for c in m["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "toolCall") {
                    if let (Some(end), Some(id)) = (end, c["id"].as_str()) {
                        started.push((end, id.to_owned()));
                    }
                }
            }
            Some("toolResult") => {
                if let (Some(id), Some(t)) = (m["toolCallId"].as_str(), ms(&m["timestamp"]).or(ts)) {
                    ended.insert(id.to_owned(), t);
                }
            }
            _ => {}
        }
    }
    let call = started.into_iter().filter(|(s, id)| *s <= at && ended.get(id).is_none_or(|e| *e >= at)).max_by_key(|(s, _)| *s)?.1;
    // A parent that is itself a sub-agent passes on its own root.
    let parent_start = header["timestamp"].as_str().and_then(parse_utc)?;
    let root = header["parentSession"].as_str().and_then(|p| spawned_by(p, parent_start, depth + 1)).map_or(parent, |(root, _)| root);
    Some((root, call))
}
