//! OpenCode sessions to canonical events, from `~/.local/share/opencode/opencode.db` (OpenCode 2.x's
//! `session_v2` and `session_message` tables, and the `session`, `message` and `part` tables of earlier
//! SQLite versions) or from the `~/.local/share/opencode/storage` folder of JSON files that 1.x used
//! before SQLite. Metadata only: no prompt, message, reasoning, shell or tool payload text is copied.
//!
//! OpenCode keeps sessions in SQLite, not a transcript file, so [`read_db`] turns each changed session
//! into JSON lines (one `session` line, then its messages in order) and feeds them through the adapter.
//! A session is re-read whole whenever it changes; event ids derive from OpenCode's ids, so the store
//! drops what it already has.
//!
//! Spans: `agent:<session>` root, `turn:<message id>` per user message, `llm:<id>` per model call (an
//! assistant message in 2.x, a step in earlier versions, each with OpenCode's own cost estimate), and
//! `tool:<call id>` per tool call, or per shell command the user ran. A turn ends with the 2.x `idle`
//! message's outcome, or in earlier versions with an assistant message that finishes without calling
//! a tool.
//!
//! Sub-agents: a sub-agent is a child session. Its events join the root session inside
//! `subagent:<child>`, nested under the parent's tool call that names the child in its metadata.

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};

/// Also reads Kilo Code (a fork that keeps OpenCode's tables in `~/.local/share/kilo/kilo.db`);
/// `harness` names which one recorded the session.
pub struct OpenCode {
    state: RefCell<State>,
    harness: &'static str,
}

impl Default for OpenCode {
    fn default() -> Self {
        Self::recorded_by("opencode")
    }
}

impl OpenCode {
    pub fn recorded_by(harness: &'static str) -> Self {
        Self { state: RefCell::default(), harness }
    }
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
    turn: Option<String>,
    turns: u64,
}

impl TraceAdapter for OpenCode {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.harness.into(),
            name: self.harness.into(),
            harness: self.harness.into(),
            unobservable: [
                "LLM call end in 2.x when a step runs tools (taken from when its stream finished)",
                "cost provenance (OpenCode's own estimate from model prices)",
                "prompt, message, reasoning, shell and tool input/output text (excluded by design)",
                "step start and end in 1.x JSON storage (taken from the parts around each step)",
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
        // OpenCode stamps everything in epoch milliseconds.
        let d = &v["data"];
        let Some(ts) = ms(&v["time_created"]).or_else(|| ms(&d["time"]["created"])) else { return Ok(Vec::new()) };
        let mut e = Emit { source: SourceId(format!("{}:{own}", self.harness)), session: SessionId(root_session.clone()), observed: raw.observed_at_utc_ns, seq: st.seq, out: Vec::new() };
        let parent = st.turn.as_deref().map_or_else(|| top.clone(), |t| span("turn", t));
        let id = v["id"].as_str().unwrap_or_default();

        match (v["t"].as_str(), v["type"].as_str()) {
            (Some("session"), _) if st.subagent => {
                // Nested sub-agents sit under their own parent's span.
                let parent_session = v["parent_id"].as_str().unwrap_or(&root_session);
                let under = match v["parent_tool"].as_str() {
                    Some(call) => span("tool", call),
                    None if parent_session == root_session => span("agent", &root_session),
                    None => span("subagent", parent_session),
                };
                let agent = v["agent"].as_str().unwrap_or("subagent").to_owned();
                e.start(&top, ts, Some(&under), SpanKind::Subagent, &agent, |s| s.agent_name = Some(agent.clone()));
            }
            (Some("session"), _) => {
                let cwd = v["directory"].as_str().unwrap_or_default();
                let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
                let mut labels = Attributes::new();
                if let Some(ver) = v["version"].as_str() {
                    labels.insert(format!("{}_version", self.harness), json!(ver));
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
                let agent = v["agent"].as_str().unwrap_or("main").to_owned();
                e.start(&top, ts, None, SpanKind::Agent, self.harness, |s| s.agent_name = Some(agent));
            }
            (Some("message"), Some("user")) => start_turn(&mut st, &mut e, &top, id, ts),
            (Some("message"), Some("assistant")) => {
                let llm = span("llm", id);
                model_call(&mut e, &llm, &parent, ts, d["model"]["id"].as_str(), d["model"]["providerID"].as_str());
                for c in d["content"].as_array().into_iter().flatten().filter(|c| c["type"] == "tool") {
                    let t = &c["time"];
                    tool(&mut e, &parent, c["id"].as_str(), c["name"].as_str(), ms(&t["ran"]).or_else(|| ms(&t["created"])).unwrap_or(ts), ms(&t["completed"]), &c["state"]);
                }
                if let Some(end) = ms(&d["time"]["streamed"]).or_else(|| ms(&d["time"]["completed"])) {
                    let error = error_text(&d["error"]);
                    let status = if error.is_some() || d["finish"] == "error" { SpanStatus::Failed } else { SpanStatus::Succeeded };
                    usage(&mut e, &llm, end, &d["tokens"], &d["cost"]);
                    e.end(format!("end:{llm}"), &llm, end, status, error);
                }
            }
            (Some("message"), Some("shell")) => {
                let state = json!({"status": if d["time"]["completed"].is_null() { "running" } else { "completed" }, "metadata": {"exit": d["exit"]}});
                tool(&mut e, &parent, Some(id), Some("shell"), ts, ms(&d["time"]["completed"]), &state);
            }
            // Compacting the context is a model call of its own, with its own tokens and cost.
            (Some("message"), Some("compaction")) if matches!(d["status"].as_str(), Some("completed" | "failed")) => {
                let llm = span("llm", id);
                model_call(&mut e, &llm, &parent, ts, d["model"]["id"].as_str(), d["model"]["providerID"].as_str());
                let end = ms(&v["time_updated"]).unwrap_or(ts).max(ts);
                usage(&mut e, &llm, end, &d["tokens"], &d["cost"]);
                let error = error_text(&d["error"]);
                e.end(format!("end:{llm}"), &llm, end, if d["status"] == "failed" { SpanStatus::Failed } else { SpanStatus::Succeeded }, error);
            }
            (Some("message"), Some("idle")) => {
                let status = match d["outcome"].as_str() {
                    Some("failed") => SpanStatus::Failed,
                    Some("interrupted") => SpanStatus::Cancelled,
                    _ => SpanStatus::Succeeded,
                };
                end_turn(&mut st, &mut e, &top, ts, status, error_text(&d["error"]));
            }
            // Before 2.0: one assistant message holds the steps of a reply, each a model call, and its tools.
            (Some("message1"), _) if d["role"] == "user" => start_turn(&mut st, &mut e, &top, id, ts),
            (Some("message1"), _) if d["role"] == "assistant" => {
                let (model, provider) = (d["modelID"].as_str(), d["providerID"].as_str());
                let parts = v["parts"].as_array().map(Vec::as_slice).unwrap_or_default();
                let mut step: Option<SpanId> = None;
                let mut steps = 0;
                for p in parts {
                    let (pd, at) = (&p["data"], ms(&p["time_created"]).unwrap_or(ts));
                    match pd["type"].as_str() {
                        Some("step-start") => {
                            steps += 1;
                            let llm = span("llm", p["id"].as_str().unwrap_or(id));
                            model_call(&mut e, &llm, &parent, at, model, provider);
                            step = Some(llm);
                        }
                        Some("step-finish") => {
                            if let Some(llm) = step.take() {
                                usage(&mut e, &llm, at, &pd["tokens"], &pd["cost"]);
                                e.end(format!("end:{llm}"), &llm, at, SpanStatus::Succeeded, None);
                            }
                        }
                        Some("tool") => {
                            let t = &pd["state"]["time"];
                            tool(&mut e, &parent, pd["callID"].as_str(), pd["tool"].as_str(), ms(&t["start"]).unwrap_or(at), ms(&t["end"]), &pd["state"]);
                        }
                        _ => {}
                    }
                }
                let done = ms(&d["time"]["completed"]);
                let error = error_text(&d["error"]);
                let status = if error.is_some() { SpanStatus::Failed } else { SpanStatus::Succeeded };
                if steps == 0 {
                    let llm = span("llm", id);
                    model_call(&mut e, &llm, &parent, ts, model, provider);
                    step = Some(llm);
                    if let Some(end) = done {
                        usage(&mut e, step.as_ref().expect("set above"), end, &d["tokens"], &d["cost"]);
                    }
                }
                // A step the reply never finished ends with the reply, failed if the reply failed.
                if let (Some(llm), Some(end)) = (step, done) {
                    e.end(format!("end:{llm}"), &llm, end, status, error);
                }
                if let Some(end) = done.filter(|_| d["finish"] != "tool-calls") {
                    end_turn(&mut st, &mut e, &top, end, status, error);
                }
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

/// 2.x errors carry `message`; earlier versions nest it under `data`.
fn error_text(e: &Value) -> Option<&str> {
    e["message"].as_str().or_else(|| e["data"]["message"].as_str()).or_else(|| e["name"].as_str())
}

fn start_turn(st: &mut State, e: &mut Emit, top: &SpanId, id: &str, ts: Ns) {
    if let Some(old) = st.turn.take() {
        let t = span("turn", &old);
        e.end(format!("end:{t}"), &t, ts, SpanStatus::Succeeded, None);
    }
    st.turns += 1;
    e.start(&span("turn", id), ts, Some(top), SpanKind::Turn, &format!("turn {}", st.turns), |_| {});
    st.turn = Some(id.to_owned());
}

/// Ends the open turn; a sub-agent's first finished run also ends its span.
fn end_turn(st: &mut State, e: &mut Emit, top: &SpanId, at: Ns, status: SpanStatus, error: Option<&str>) {
    if let Some(old) = st.turn.take() {
        let t = span("turn", &old);
        e.end(format!("end:{t}"), &t, at, status, error);
    }
    if st.subagent {
        e.end(format!("end:{top}"), top, at, status, error);
    }
}

fn model_call(e: &mut Emit, llm: &SpanId, parent: &SpanId, at: Ns, model: Option<&str>, provider: Option<&str>) {
    e.start(llm, at, Some(parent), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
        s.model = model.map(str::to_owned);
        s.provider = provider.map(str::to_owned);
    });
}

fn usage(e: &mut Emit, llm: &SpanId, at: Ns, t: &Value, cost: &Value) {
    if !t.is_object() {
        return;
    }
    let cost = cost.as_f64().filter(|c| *c > 0.).map(|c| (c * 1e6).round() as u64);
    let usage = Usage {
        tokens_input: t["input"].as_u64(),
        tokens_output: t["output"].as_u64(),
        cache_read: t["cache"]["read"].as_u64(),
        cache_write: t["cache"]["write"].as_u64(),
        cost_usd_micros: cost,
        cost_provenance: if cost.is_some() { CostProvenance::Estimate } else { CostProvenance::Unknown },
    };
    e.push(format!("{llm}:usage"), at, Some(llm), EventBody::UsageReported { usage, is_final: true });
}

/// A tool call from its start; it ends only once its status is final. A shell command that ran to a
/// non-zero exit is `completed` in OpenCode's terms, with the exit code in `state.metadata.exit`: that
/// is a failure.
fn tool(e: &mut Emit, parent: &SpanId, call: Option<&str>, name: Option<&str>, at: Ns, end: Option<Ns>, state: &Value) {
    let (Some(call), Some(name)) = (call, name) else { return };
    let tool = span("tool", call);
    e.start(&tool, at, Some(parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
    let nonzero = state["metadata"]["exit"].as_i64().is_some_and(|x| x != 0);
    let (status, error) = match state["status"].as_str() {
        Some("completed") if nonzero => (SpanStatus::Failed, Some("command exited non-zero (output not captured)")),
        Some("completed") => (SpanStatus::Succeeded, None),
        Some("error") => (SpanStatus::Failed, Some("tool reported an error (output not captured)")),
        _ => return,
    };
    e.end(format!("end:{tool}"), &tool, end.unwrap_or(at), status, error);
}

/// Events for every session changed after `since_ms` (epoch milliseconds), and the newest change seen.
// ponytail: a write landing in the same millisecond as the previous poll's newest is picked up with the
// session's next change; poll with `>=` if that delay ever shows.
pub fn read_db(path: &Path, since_ms: i64) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    read_db_as(path, since_ms, "opencode")
}

/// [`read_db`] for a fork that keeps OpenCode's tables, its sessions recorded by `harness`.
pub fn read_db_as(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.exists(), "{} does not exist; point it at opencode.db, or the storage folder of OpenCode 1.x", path.display());
    if path.is_dir() {
        return read_storage(path, since_ms, harness);
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let table = |name: &str| -> rusqlite::Result<bool> { conn.query_row("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)", [name], |r| r.get(0)) };
    let v2 = table("session_v2")?;
    let v1 = table("session")? && table("message")? && table("part")?;
    anyhow::ensure!(v1 || v2, "{} has no OpenCode session tables", path.display());
    let mut changed: Vec<(String, i64)> = Vec::new();
    let mut collect = |sql: &str| -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(sql)?;
        for row in stmt.query_map([since_ms], |r| Ok((r.get(0)?, r.get(1)?)))? {
            changed.push(row?);
        }
        Ok(())
    };
    if v2 {
        collect("SELECT id, time_updated FROM session_v2 WHERE time_updated > ?1")?;
        collect("SELECT session_id, MAX(time_updated) FROM session_message WHERE time_updated > ?1 GROUP BY session_id")?;
    }
    if v1 {
        collect("SELECT id, time_updated FROM session WHERE time_updated > ?1")?;
        collect("SELECT session_id, MAX(time_updated) FROM message WHERE time_updated > ?1 GROUP BY session_id")?;
        collect("SELECT session_id, MAX(time_updated) FROM part WHERE time_updated > ?1 GROUP BY session_id")?;
    }
    let newest = changed.iter().map(|(_, t)| *t).max().unwrap_or(since_ms).max(since_ms);
    let mut ids: Vec<String> = changed.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids.dedup();
    let mut events = Vec::new();
    for id in ids {
        // 2.x migrates sessions into session_v2; a session found there is read from there only.
        let Some((info, is_v2)) = session_info(&conn, &id, v2, v1)? else { continue };
        let mut line = info;
        if let Some(parent) = line["parent_id"].as_str().map(str::to_owned) {
            line["root"] = json!(root_of(&conn, &parent, v2, v1)?);
            line["parent_tool"] = json!(spawning_call(&conn, &parent, &id, v2, v1)?);
        }
        let adapter = OpenCode::recorded_by(harness);
        let mut feed = |line: Value| -> anyhow::Result<()> {
            events.extend(adapter.normalize(RawEvent { line: line.to_string(), observed_at_utc_ns: system_now_ns() })?);
            Ok(())
        };
        feed(line)?;
        if is_v2 {
            // A fork starts with copies of its parent's rows; only what it did itself is its own.
            let copied_through = fork_copied_through(&conn, &id)?.unwrap_or(i64::MIN);
            let mut stmt = conn.prepare("SELECT id, type, time_created, time_updated, data FROM session_message WHERE session_id = ?1 AND seq > ?2 ORDER BY seq")?;
            let rows = stmt.query_map(params![id, copied_through], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)))?;
            for row in rows {
                let (mid, kind, created, updated, data) = row?;
                feed(json!({"t": "message", "id": mid, "type": kind, "time_created": created, "time_updated": updated, "data": parse(&data)}))?;
            }
        } else {
            let mut msgs = conn.prepare("SELECT id, time_created, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
            let mut parts = conn.prepare("SELECT id, time_created, data FROM part WHERE message_id = ?1 ORDER BY id")?;
            let rows: Vec<(String, i64, String)> = msgs.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
            for (mid, created, data) in rows {
                let ps: Vec<Value> = parts
                    .query_map(params![mid], |r| Ok(json!({"id": r.get::<_, String>(0)?, "time_created": r.get::<_, i64>(1)?, "data": parse(&r.get::<_, String>(2)?)})))?
                    .collect::<rusqlite::Result<_>>()?;
                feed(json!({"t": "message1", "id": mid, "time_created": created, "data": parse(&data), "parts": ps}))?;
            }
        }
    }
    Ok((events, newest))
}

fn parse(data: &str) -> Value {
    serde_json::from_str(data).unwrap_or(Value::Null)
}

/// For a forked 2.x session, the last `seq` that is a copy of the parent's history: the boundary message's
/// own `seq` for a `through` fork, the parent's last `seq` before it for a `before` fork. None when the
/// session is no fork or the boundary cannot be resolved, so nothing is dropped on a guess (as ccusage).
fn fork_copied_through(conn: &Connection, id: &str) -> rusqlite::Result<Option<i64>> {
    let has = |col: &str| -> rusqlite::Result<bool> { conn.query_row("SELECT EXISTS (SELECT 1 FROM pragma_table_info('session_v2') WHERE name = ?1)", [col], |r| r.get(0)) };
    if !has("fork_session_id")? || !has("fork_boundary")? {
        return Ok(None);
    }
    let Some((parent, boundary)) = conn
        .query_row("SELECT fork_session_id, fork_boundary FROM session_v2 WHERE id = ?1", [id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))
        .optional()?
        .and_then(|(p, b)| p.zip(b))
    else {
        return Ok(None);
    };
    let boundary = parse(&boundary);
    let (Some(kind), Some(message)) = (boundary["type"].as_str(), boundary["messageID"].as_str()) else { return Ok(None) };
    let Some(seq) = conn.query_row("SELECT seq FROM session_message WHERE session_id = ?1 AND id = ?2", params![parent, message], |r| r.get::<_, i64>(0)).optional()? else {
        return Ok(None);
    };
    match kind {
        "through" => Ok(Some(seq)),
        "before" => conn.query_row("SELECT MAX(seq) FROM session_message WHERE session_id = ?1 AND seq < ?2", params![parent, seq], |r| r.get(0)),
        _ => Ok(None),
    }
}

/// The session's `session` line, and whether it lives in the 2.x tables.
fn session_info(conn: &Connection, id: &str, v2: bool, v1: bool) -> rusqlite::Result<Option<(Value, bool)>> {
    let row = |r: &rusqlite::Row| Ok(json!({"t": "session", "id": id, "directory": r.get::<_, String>(0)?, "version": r.get::<_, String>(1)?, "parent_id": r.get::<_, Option<String>>(2)?, "agent": r.get::<_, Option<String>>(3)?, "time_created": r.get::<_, i64>(4)?}));
    if v2 && let Some(info) = conn.query_row("SELECT directory, version, parent_id, agent, time_created FROM session_v2 WHERE id = ?1", [id], row).optional()? {
        return Ok(Some((info, true)));
    }
    if v1 && let Some(info) = conn.query_row("SELECT directory, version, parent_id, agent, time_created FROM session WHERE id = ?1", [id], row).optional()? {
        return Ok(Some((info, false)));
    }
    Ok(None)
}

/// The top of `id`'s parent chain.
fn root_of(conn: &Connection, id: &str, v2: bool, v1: bool) -> rusqlite::Result<String> {
    let mut at = id.to_owned();
    // Bounded, so a corrupt cycle cannot hang the collector.
    for _ in 0..64 {
        match session_info(conn, &at, v2, v1)?.and_then(|(info, _)| info["parent_id"].as_str().map(str::to_owned)) {
            Some(up) => at = up,
            None => break,
        }
    }
    Ok(at)
}

/// The parent's tool call whose metadata names `child`: the sub-agent call that started it.
fn spawning_call(conn: &Connection, parent: &str, child: &str, v2: bool, v1: bool) -> rusqlite::Result<Option<String>> {
    let names_child = |state: &Value| names_session(state, child);
    let like = format!("%{child}%");
    if v2 {
        let mut stmt = conn.prepare("SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' AND data LIKE ?2")?;
        for data in stmt.query_map(params![parent, like], |r| r.get::<_, String>(0))? {
            let found = parse(&data?)["content"].as_array().into_iter().flatten().find(|c| c["type"] == "tool" && names_child(&c["state"])).and_then(|c| c["id"].as_str().map(str::to_owned));
            if found.is_some() {
                return Ok(found);
            }
        }
    }
    if v1 {
        let mut stmt = conn.prepare("SELECT data FROM part WHERE session_id = ?1 AND data LIKE ?2")?;
        for data in stmt.query_map(params![parent, like], |r| r.get::<_, String>(0))? {
            let p = parse(&data?);
            if p["type"] == "tool" && names_child(&p["state"]) {
                return Ok(p["callID"].as_str().map(str::to_owned));
            }
        }
    }
    Ok(None)
}

/// Whether a tool state's metadata names the session `child` (2.x writes `sessionID`, earlier versions `sessionId`).
fn names_session(state: &Value, child: &str) -> bool {
    [&state["metadata"]["sessionID"], &state["metadata"]["sessionId"]].iter().any(|x| x.as_str() == Some(child))
}

/// OpenCode 1.x storage: `session/<project>/<session>.json`, `message/<session>/<message>.json` and
/// `part/<message>/<part>.json`, the same messages and parts its later SQLite tables hold. A session
/// changed when any of its files did.
// ponytail: every poll stats each session's files; index by mtime if a large 1.x history makes polls slow.
fn read_storage(dir: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(dir.join("session").is_dir(), "{} is not an OpenCode storage folder (no session/)", dir.display());
    let mut sessions: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for project in json_files_or_dirs(&dir.join("session"), true) {
        for f in json_files_or_dirs(&project, false) {
            if let Some(info) = read_json(&f).filter(|i| i["id"].is_string()) {
                sessions.insert(info["id"].as_str().expect("filtered").to_owned(), json!({"info": info, "file": f}));
            }
        }
    }
    let messages_of = |sid: &str| -> Vec<(PathBufValue, Vec<PathBufValue>)> {
        json_files_or_dirs(&dir.join("message").join(sid), false)
            .into_iter()
            .filter_map(|f| {
                let m = read_json(&f)?;
                let parts = json_files_or_dirs(&dir.join("part").join(m["id"].as_str()?), false).into_iter().filter_map(|p| Some((p.clone(), read_json(&p)?))).collect();
                Some(((f, m), parts))
            })
            .collect()
    };
    let mut newest = since_ms;
    let mut events = Vec::new();
    let mut ids: Vec<&String> = sessions.keys().collect();
    ids.sort();
    for id in ids {
        let msgs = messages_of(id);
        let changed = std::iter::once(Path::new(sessions[id]["file"].as_str().unwrap_or_default()))
            .chain(msgs.iter().flat_map(|((f, _), parts)| std::iter::once(f.as_path()).chain(parts.iter().map(|(p, _)| p.as_path()))))
            .map(mtime_ms)
            .max()
            .unwrap_or(0);
        if changed <= since_ms {
            continue;
        }
        newest = newest.max(changed);
        let info = &sessions[id]["info"];
        let mut line = json!({"t": "session", "id": id, "directory": info["directory"], "version": info["version"], "parent_id": info["parentID"], "agent": null, "time_created": info["time"]["created"]});
        if let Some(parent) = info["parentID"].as_str() {
            let mut root = parent.to_owned();
            for _ in 0..64 {
                match sessions.get(&root).and_then(|s| s["info"]["parentID"].as_str()) {
                    Some(up) => root = up.to_owned(),
                    None => break,
                }
            }
            let call = messages_of(parent).into_iter().flat_map(|(_, parts)| parts).find(|(_, p)| p["type"] == "tool" && names_session(&p["state"], id)).and_then(|(_, p)| p["callID"].as_str().map(str::to_owned));
            line["root"] = json!(root);
            line["parent_tool"] = json!(call);
        }
        let adapter = OpenCode::recorded_by(harness);
        let mut feed = |line: Value| -> anyhow::Result<()> {
            events.extend(adapter.normalize(RawEvent { line: line.to_string(), observed_at_utc_ns: system_now_ns() })?);
            Ok(())
        };
        feed(line)?;
        let mut msgs: Vec<_> = msgs.into_iter().map(|((_, m), parts)| (m, parts.into_iter().map(|(_, p)| p).collect::<Vec<_>>())).collect();
        msgs.sort_by(|(a, _), (b, _)| (a["time"]["created"].as_i64(), a["id"].as_str()).cmp(&(b["time"]["created"].as_i64(), b["id"].as_str())));
        for (m, mut parts) in msgs {
            let created = m["time"]["created"].as_i64().unwrap_or_default();
            parts.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            let parts = timed_parts(created, parts);
            feed(json!({"t": "message1", "id": m["id"], "time_created": created, "data": m, "parts": parts}))?;
        }
    }
    Ok((events, newest))
}

type PathBufValue = (std::path::PathBuf, Value);

/// Parts as the SQLite tables hold them, with the creation time the JSON files lack: a step starts
/// where the reply last got to, and finishes at the latest end any part inside it reports.
fn timed_parts(created: i64, parts: Vec<Value>) -> Vec<Value> {
    let mut reached = created;
    parts
        .into_iter()
        .map(|p| {
            let (start, end) = match p["type"].as_str() {
                Some("tool") => (p["state"]["time"]["start"].as_i64(), p["state"]["time"]["end"].as_i64()),
                _ => (p["time"]["start"].as_i64(), p["time"]["end"].as_i64()),
            };
            let at = match p["type"].as_str() {
                Some("step-start" | "step-finish") => reached,
                _ => start.unwrap_or(reached),
            };
            reached = reached.max(end.or(start).unwrap_or(reached));
            json!({"id": p["id"], "time_created": at, "data": p})
        })
        .collect()
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// The `*.json` files in `dir`, or with `dirs` its subfolders, sorted; nothing if `dir` is missing.
fn json_files_or_dirs(dir: &Path, dirs: bool) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| if dirs { p.is_dir() } else { p.extension().is_some_and(|e| e == "json") })
        .collect();
    out.sort();
    out
}
