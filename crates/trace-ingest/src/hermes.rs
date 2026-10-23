//! Hermes Agent (Nous Research) sessions to canonical events, from `~/.hermes/state.db` (`$HERMES_HOME`):
//! the `sessions` table (model, working directory, start and end, token and cost totals, parent session)
//! and the `messages` table (role, `tool_calls` JSON, `tool_call_id`, `timestamp` in epoch seconds).
//! Metadata only: no prompt, message, reasoning or tool payload text is copied. A tool result's text is
//! read only to tell whether the tool failed.
//!
//! Hermes keeps sessions in SQLite, so [`read`] turns each changed session into JSON lines (one `session`
//! line, its messages in order, an `end` line once it ended) and feeds them through the adapter. A
//! session is re-read whole whenever it changes; event ids derive from Hermes's ids.
//!
//! Spans: `agent:<session>` root, carrying the session's token totals; `turn:<message id>` from a user
//! message to the assistant message that answers without calling a tool; `llm:<message id>` per
//! assistant message, from the message before it (Hermes stamps each row as it is added, so the
//! request starts at the prompt or the last tool result); `tool:<session>:<call id>` from the assistant message
//! that asked for it to its result, failed when the result carries an `error` or a non-zero
//! `exit_code`, cancelled when Hermes recorded it as interrupted. A call through Hermes's `tool_call`
//! bridge (its deferred-tool search) is named after the tools it ran.
//!
//! Sub-agents: `delegate_task` runs each child agent as a session whose `parent_session_id` names the
//! parent. A child that started while a `delegate_task` call of its parent was running joins the root
//! session as `subagent:<child>` under that call, with the child's token totals; any other child (a
//! compression continuation, a branch) stays its own session.
//!
//! Gaps: Hermes records tokens and cost per session, not per model call, so model calls carry no usage
//! of their own; nor which model answered each message, so all take the session's model.

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Hermes { harness, state: RefCell::default() })
}

struct Hermes {
    harness: &'static str,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    seq: u64,
    own: Option<String>,
    root_session: Option<String>,
    top: Option<SpanId>,
    subagent: bool,
    model: Option<String>,
    turn: Option<SpanId>,
    turns: u64,
    /// When the previous message was written: the next model call's request starts there.
    last: Ns,
}

impl TraceAdapter for Hermes {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.harness.into(),
            name: self.harness.into(),
            harness: self.harness.into(),
            unobservable: [
                "token usage and cost per model call (Hermes records them per session; they sit on the agent or sub-agent span)",
                "model per call (every call takes the session's model)",
                "tool execution start (taken from the requesting assistant message)",
                "prompt, message, reasoning and tool input/output text (excluded by design)",
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
            st.model = v["model"].as_str().map(str::to_owned);
        }
        let (Some(own), Some(root_session), Some(top)) = (st.own.clone(), st.root_session.clone(), st.top.clone()) else { return Ok(Vec::new()) };
        let Some(ts) = secs(&v["ts"]) else { return Ok(Vec::new()) };
        let mut e = Emit { source: SourceId(format!("{}:{own}", self.harness)), session: SessionId(root_session.clone()), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };
        let model = st.model.clone();

        match v["t"].as_str() {
            Some("session") => {
                if st.subagent {
                    let under = v["parent_tool"].as_str().map_or_else(|| span("agent", &root_session), |call| span("tool", &format!("{}:{call}", v["parent"].as_str().unwrap_or_default())));
                    e.start(&top, ts, Some(&under), SpanKind::Subagent, "delegate_task", |s| {
                        s.agent_name = Some("subagent".into());
                        s.model = model.clone();
                    });
                } else {
                    let cwd = v["cwd"].as_str().unwrap_or_default();
                    let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                    let mut labels = Attributes::new();
                    if let Some(source) = v["source"].as_str() {
                        labels.insert("hermes_source".into(), json!(source));
                    }
                    let body = EventBody::SessionStarted {
                        project_id: project.to_owned(),
                        working_directory: cwd.to_owned(),
                        source_adapter: self.harness.into(),
                        harness: self.harness.into(),
                        host: String::new(),
                        labels,
                    };
                    e.push(format!("session:{own}"), ts, None, body);
                    e.start(&top, ts, None, SpanKind::Agent, self.harness, |s| {
                        s.agent_name = Some("main".into());
                        s.model = model.clone();
                    });
                }
                usage(&mut e, &top, ts, &v["usage"]);
                st.last = ts;
            }
            Some("message") => {
                let id = v["id"].as_i64().unwrap_or_default().to_string();
                let parent = st.turn.clone().unwrap_or_else(|| top.clone());
                match v["role"].as_str() {
                    Some("user") => {
                        if let Some(old) = st.turn.take() {
                            e.end(format!("end:{old}"), &old, ts, SpanStatus::Succeeded, None);
                        }
                        st.turns += 1;
                        let turn = span("turn", &format!("{own}:{id}"));
                        e.start(&turn, ts, Some(&top), SpanKind::Turn, &format!("turn {}", st.turns), |_| {});
                        st.turn = Some(turn);
                    }
                    Some("assistant") => {
                        let llm = span("llm", &format!("{own}:{id}"));
                        let name = model.clone().unwrap_or_else(|| "llm call".into());
                        e.start(&llm, st.last.min(ts), Some(&parent), SpanKind::Llm, &name, |s| s.model = model.clone());
                        e.end(format!("end:{llm}"), &llm, ts, SpanStatus::Succeeded, None);
                        let calls = v["tool_calls"].as_array().map(Vec::as_slice).unwrap_or_default();
                        for c in calls {
                            let (Some(call), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else { continue };
                            let tool = span("tool", &format!("{own}:{call}"));
                            e.start(&tool, ts, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                        }
                        if calls.is_empty()
                            && let Some(turn) = st.turn.take()
                        {
                            e.end(format!("end:{turn}"), &turn, ts, SpanStatus::Succeeded, None);
                            if st.subagent {
                                e.end(format!("end:{top}"), &top, ts, SpanStatus::Succeeded, None);
                            }
                        }
                    }
                    Some("tool") => {
                        if let Some(call) = v["tool_call_id"].as_str() {
                            let tool = span("tool", &format!("{own}:{call}"));
                            let (status, error) = match v["outcome"].as_str() {
                                Some("failed") => (SpanStatus::Failed, Some("tool reported an error (output not captured)")),
                                Some("interrupted") => (SpanStatus::Cancelled, None),
                                _ => (SpanStatus::Succeeded, None),
                            };
                            e.end(format!("end:{tool}"), &tool, ts, status, error);
                        }
                    }
                    _ => {}
                }
                st.last = ts;
            }
            // The session ended with a turn still open: it was cut short.
            Some("end") => {
                if let Some(turn) = st.turn.take() {
                    e.end(format!("end:{turn}"), &turn, ts, SpanStatus::Cancelled, v["reason"].as_str());
                    if st.subagent {
                        e.end(format!("end:{top}"), &top, ts, SpanStatus::Cancelled, v["reason"].as_str());
                    }
                }
            }
            _ => {}
        }
        st.seq = e.seq;
        Ok(e.out)
    }
}

/// Epoch seconds (Hermes's REAL timestamps) to nanoseconds.
fn secs(x: &Value) -> Option<Ns> {
    x.as_f64().map(|s| (s * 1e9).round() as Ns).filter(|t| VALID_TIME_NS.contains(t))
}

/// The session's token totals, reported again whenever they change (the latest final report wins).
fn usage(e: &mut Emit, top: &SpanId, at: Ns, u: &Value) {
    let n = |k: &str| u[k].as_u64().filter(|x| *x > 0);
    if n("input").is_none() && n("output").is_none() {
        return;
    }
    let (cost, provenance) = match (u["actual"].as_f64().filter(|c| *c > 0.), u["estimated"].as_f64().filter(|c| *c > 0.)) {
        (Some(c), _) => (Some((c * 1e6).round() as u64), CostProvenance::Actual),
        (None, Some(c)) => (Some((c * 1e6).round() as u64), CostProvenance::Estimate),
        _ => (None, CostProvenance::Unknown),
    };
    let usage = Usage { tokens_input: n("input"), tokens_output: n("output"), cache_read: n("cache_read"), cache_write: n("cache_write"), cost_usd_micros: cost, cost_provenance: provenance };
    let id = format!("{top}:usage:{}:{}:{}:{}:{}", usage.tokens_input.unwrap_or(0), usage.tokens_output.unwrap_or(0), usage.cache_read.unwrap_or(0), usage.cache_write.unwrap_or(0), cost.unwrap_or(0));
    e.push(id, at, Some(top), EventBody::UsageReported { usage, is_final: true });
}

/// Events for every session changed after `since_ms` (epoch milliseconds of its latest start, end or
/// message), and the newest change seen.
// ponytail: totals Hermes writes after a session's last message and before it ends show up when it ends;
// add `last_activity_at` to the change time if that delay matters. A write within the same millisecond
// as the previous poll's newest waits for the session's next change.
pub fn read(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.is_file(), "{} does not exist; point it at ~/.hermes/state.db", path.display());
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let since = since_ms as f64 / 1000.;
    let changed: Vec<(String, f64)> = conn
        .prepare(
            "SELECT s.id, MAX(s.started_at, COALESCE(s.ended_at, 0), COALESCE((SELECT MAX(m.timestamp) FROM messages m WHERE m.session_id = s.id), 0)) AS t
             FROM sessions s WHERE t > ?1 ORDER BY s.started_at, s.id",
        )?
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Rounded up, so a session is not re-read for a fraction of a millisecond the cursor dropped.
    let newest = changed.iter().map(|(_, t)| (t * 1000.).ceil() as i64).max().unwrap_or(since_ms).max(since_ms);
    let mut events = Vec::new();
    for (id, _) in changed {
        let Some(mut line) = session_line(&conn, &id)? else { continue };
        let mut lines = Vec::new();
        if let Some((parent, call)) = spawned_by(&conn, &line)? {
            line["root"] = json!(root_of(&conn, &parent)?);
            line["parent_tool"] = json!(call);
        }
        let ended = line["ended"].clone();
        let reason = line["end_reason"].clone();
        lines.push(line);
        let mut stmt = conn.prepare("SELECT id, role, timestamp, tool_calls, tool_call_id, effect_disposition, content FROM messages WHERE session_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map(params![id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?, r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?))
        })?;
        for row in rows {
            let (mid, role, ts, calls, call_id, disposition, content) = row?;
            let calls: Vec<Value> = calls
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .and_then(|c| c.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .map(|c| json!({"id": c["id"], "name": call_name(c)}))
                .collect();
            let outcome = match (disposition, content.as_deref().map(tool_failed)) {
                (Some(_), _) => "interrupted",
                (None, Some(true)) => "failed",
                _ => "ok",
            };
            lines.push(json!({"t": "message", "id": mid, "role": role, "ts": ts, "tool_calls": calls, "tool_call_id": call_id, "outcome": outcome}));
        }
        // A sub-agent's first message can be stamped a moment before its session row.
        if let Some(first) = lines.get(1).and_then(|m| m["ts"].as_f64()).filter(|f| lines[0]["ts"].as_f64().is_some_and(|s| *f < s)) {
            lines[0]["ts"] = json!(first);
        }
        if !ended.is_null() {
            lines.push(json!({"t": "end", "ts": ended, "reason": reason}));
        }
        let adapter = adapter(harness);
        for l in lines {
            events.extend(adapter.normalize(RawEvent { line: l.to_string(), observed_at_utc_ns: system_now_ns() })?);
        }
    }
    Ok((events, newest))
}

/// A tool result Hermes's tools write as JSON: failed when it carries an `error`, or a shell command's
/// non-zero `exit_code`. Hermes may append notes after the JSON, so only the leading value is read. Plain
/// text results are successes.
fn tool_failed(content: &str) -> bool {
    let Some(Ok(v)) = serde_json::Deserializer::from_str(content).into_iter::<Value>().next() else { return false };
    let error = v["error"].as_str().is_some_and(|e| !e.is_empty());
    error || v["exit_code"].as_i64().is_some_and(|c| c != 0)
}

/// A tool call's name. With many tools Hermes offers them through its `tool_call` bridge, whose arguments
/// name the tools it runs (`{"calls": [{"name", "arguments"}]}`, or one `{"name", "arguments"}`).
fn call_name(c: &Value) -> Option<String> {
    let name = c["function"]["name"].as_str()?;
    if name != "tool_call" {
        return Some(name.to_owned());
    }
    let parse = |x: &Value| x.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()).unwrap_or_else(|| x.clone());
    let args = parse(&c["function"]["arguments"]);
    let calls = match parse(&args["calls"]) {
        Value::Array(a) => a,
        Value::Null => vec![args.clone()],
        one => vec![one],
    };
    let inner: Vec<&str> = calls.iter().filter_map(|c| c["name"].as_str()).collect();
    Some(if inner.is_empty() { name.to_owned() } else { inner.join(", ") })
}

fn session_line(conn: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    conn.query_row(
        "SELECT source, model, parent_session_id, started_at, ended_at, end_reason, cwd, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, estimated_cost_usd, actual_cost_usd FROM sessions WHERE id = ?1",
        [id],
        |r| {
            Ok(json!({
                "t": "session", "id": id, "source": r.get::<_, Option<String>>(0)?, "model": r.get::<_, Option<String>>(1)?,
                "parent": r.get::<_, Option<String>>(2)?, "ts": r.get::<_, f64>(3)?, "ended": r.get::<_, Option<f64>>(4)?,
                "end_reason": r.get::<_, Option<String>>(5)?, "cwd": r.get::<_, Option<String>>(6)?,
                "usage": {"input": r.get::<_, Option<i64>>(7)?, "output": r.get::<_, Option<i64>>(8)?, "cache_read": r.get::<_, Option<i64>>(9)?,
                          "cache_write": r.get::<_, Option<i64>>(10)?, "estimated": r.get::<_, Option<f64>>(11)?, "actual": r.get::<_, Option<f64>>(12)?},
            }))
        },
    )
    .optional()
}

/// For a child session, its parent and the parent's `delegate_task` call that was running when the child
/// started; None when no such call ran (a compression continuation or a branch, not a sub-agent).
fn spawned_by(conn: &Connection, session: &Value) -> rusqlite::Result<Option<(String, String)>> {
    let (Some(parent), Some(started)) = (session["parent"].as_str(), session["ts"].as_f64()) else { return Ok(None) };
    let mut stmt = conn.prepare("SELECT timestamp, tool_calls FROM messages WHERE session_id = ?1 AND role = 'assistant' AND tool_calls LIKE '%delegate_task%' AND timestamp <= ?2 ORDER BY id DESC")?;
    let asked: Vec<(f64, String)> = stmt.query_map(params![parent, started], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    for (_, calls) in asked {
        let calls: Value = serde_json::from_str(&calls).unwrap_or_default();
        for c in calls.as_array().into_iter().flatten().filter(|c| call_name(c).is_some_and(|n| n.split(", ").any(|n| n == "delegate_task"))) {
            let Some(call) = c["id"].as_str() else { continue };
            let done: Option<f64> = conn.query_row("SELECT MIN(timestamp) FROM messages WHERE session_id = ?1 AND role = 'tool' AND tool_call_id = ?2", params![parent, call], |r| r.get(0))?;
            if done.is_none_or(|d| d >= started) {
                return Ok(Some((parent.to_owned(), call.to_owned())));
            }
        }
    }
    Ok(None)
}

/// The top of `id`'s chain of sub-agent parents.
fn root_of(conn: &Connection, id: &str) -> rusqlite::Result<String> {
    let mut at = id.to_owned();
    // Bounded, so a corrupt cycle cannot hang the collector.
    for _ in 0..64 {
        let Some(info) = session_line(conn, &at)? else { break };
        match spawned_by(conn, &info)? {
            Some((up, _)) => at = up,
            None => break,
        }
    }
    Ok(at)
}
