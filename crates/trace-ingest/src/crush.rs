//! Crush (charmbracelet/crush) sessions to canonical events, from a project's `.crush/crush.db`, or from
//! every project the global `~/.local/share/crush/projects.json` lists (`$CRUSH_GLOBAL_DATA`). Metadata
//! only: no prompt, message, reasoning or tool payload text is copied.
//!
//! Crush keeps sessions in SQLite: `sessions` (with `parent_session_id`) and `messages` (`role`, `model`,
//! `provider`, and `parts`, a JSON list of `{type, data}`), every time in whole seconds. A session is
//! re-read whole whenever it changes; event ids derive from Crush's ids, so the store drops what it
//! already has.
//!
//! Spans: `agent:<session>` root, `turn:<message id>` from a user message to the assistant message that
//! finishes for any reason but `tool_use`, `llm:<message id>` per assistant message, and
//! `tool:<session>:<call id>` per tool call, ending with the tool message that answers it. Crush stamps
//! an assistant message finished only after its tools ran, so a call that runs tools ends where its first
//! tool started: the shell command's own millisecond `start_time`, the sub-agent's creation, or else the
//! first tool result. A shell command that exits non-zero is no error to Crush; its result ends with
//! `Exit code <n>`, and that is a failure.
//!
//! Usage: Crush records no tokens per call. A session keeps its cost so far (its sub-agents' added in as
//! each finishes), reported on the agent span; its token columns hold only the latest step's prompt
//! and output, so they are left out.
//!
//! Sub-agents: the agent tool runs a child session whose id is `<message id>$$<call id>` (a bare call id
//! is read the same way). Its events join the root session inside `subagent:<child>`, under that call.

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Crush(harness))
}

struct Crush(&'static str);

impl TraceAdapter for Crush {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.0.into(),
            name: self.0.into(),
            harness: self.0.into(),
            unobservable: [
                "token usage per LLM call (Crush keeps none; the session's cost lands on its agent span)",
                "LLM call end when it runs tools (taken from when its first tool started)",
                "tool start except shell commands (taken from the end of the call that asked for it)",
                "sub-second timing (Crush stores whole seconds)",
                "title generation calls",
                "prompt, message, reasoning and tool input/output text (excluded by design)",
                "host",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, _raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        anyhow::bail!("{} sessions are read from crush.db or projects.json, not line by line", self.0)
    }
}

/// Events for every session changed at or after `since` (epoch milliseconds), and the newest change seen.
// ponytail: Crush stamps whole seconds, so the newest second is re-read every poll (the store drops the
// duplicates); a `>` would miss writes landing later in that second.
pub fn read(path: &Path, since: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.exists(), "{} does not exist; point it at a project's .crush/crush.db or Crush's projects.json", path.display());
    let dbs: Vec<(PathBuf, String)> = if path.extension().is_some_and(|e| e == "json") {
        let registry: Value = serde_json::from_slice(&std::fs::read(path)?).with_context(|| format!("parsing {}", path.display()))?;
        registry["projects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((Path::new(p["data_dir"].as_str()?).join("crush.db"), p["path"].as_str()?.to_owned())))
            .filter(|(db, _)| db.is_file())
            .collect()
    } else {
        let project = path.parent().and_then(Path::parent).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        vec![(path.to_owned(), project)]
    };
    let mut newest = since;
    let mut events = Vec::new();
    for (db, project) in dbs {
        let (mut evs, n) = read_db(&db, since, harness, &project).with_context(|| format!("reading {}", db.display()))?;
        events.append(&mut evs);
        newest = newest.max(n);
    }
    Ok((events, newest))
}

fn read_db(path: &Path, since: i64, harness: &str, project: &str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let since_s = since.div_euclid(1000);
    let mut changed: Vec<(String, i64)> = Vec::new();
    for sql in [
        "SELECT id, updated_at FROM sessions WHERE updated_at >= ?1",
        "SELECT session_id, MAX(updated_at) FROM messages WHERE updated_at >= ?1 GROUP BY session_id",
    ] {
        let mut stmt = conn.prepare(sql)?;
        for row in stmt.query_map([since_s], |r| Ok((r.get(0)?, r.get(1)?)))? {
            changed.push(row?);
        }
    }
    let newest = changed.iter().map(|(_, t)| t.saturating_mul(1000)).max().unwrap_or(since).max(since);
    let mut ids: Vec<String> = changed.into_iter().map(|(id, _)| id).collect();
    ids.sort();
    ids.dedup();
    let mut events = Vec::new();
    for id in ids.iter().filter(|id| !id.starts_with("title-")) {
        session(&conn, id, harness, project, &mut events)?;
    }
    Ok((events, newest))
}

fn session(conn: &Connection, id: &str, harness: &str, project: &str, events: &mut Vec<CanonicalEvent>) -> anyhow::Result<()> {
    let row = |id: &str| -> rusqlite::Result<Option<(Option<String>, i64, f64)>> {
        conn.query_row("SELECT parent_session_id, created_at, cost FROM sessions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()
    };
    let Some((parent, created, cost)) = row(id)? else { return Ok(()) };
    // Bounded, so a corrupt cycle cannot hang the collector.
    let mut root = id.to_owned();
    for _ in 0..64 {
        match row(&root)?.and_then(|(p, ..)| p) {
            Some(up) => root = up,
            None => break,
        }
    }
    let mut e = Emit { source: SourceId(format!("{harness}:{id}")), session: SessionId(root.clone()), observed: system_now_ns(), seq: 0, out: Vec::new() };
    let start = secs(created);
    let top = if parent.is_some() { span("subagent", id) } else { span("agent", id) };
    if parent.is_some() {
        let under = span("tool", &format!("{root}:{}", call_of(id)));
        e.start(&top, start, Some(&under), SpanKind::Subagent, "agent", |s| s.agent_name = Some("agent".into()));
    } else {
        let body = EventBody::SessionStarted {
            project_id: Path::new(project).file_name().and_then(|n| n.to_str()).unwrap_or(project).to_owned(),
            working_directory: project.to_owned(),
            source_adapter: harness.to_owned(),
            harness: harness.to_owned(),
            host: String::new(),
            labels: Attributes::new(),
        };
        e.push(format!("session:{id}"), start, None, body);
        e.start(&top, start, None, SpanKind::Agent, harness, |s| s.agent_name = Some("main".into()));
        if cost > 0. {
            let micros = (cost * 1e6).round() as u64;
            let usage = Usage { cost_usd_micros: Some(micros), cost_provenance: CostProvenance::Estimate, ..Usage::default() };
            // The cost grows as the session goes on; each new total is its own event and replaces the last.
            e.push(format!("{top}:cost:{micros}"), start, Some(&top), EventBody::UsageReported { usage, is_final: true });
        }
    }

    let mut stmt = conn.prepare("SELECT id, role, parts, model, provider, created_at FROM messages WHERE session_id = ?1 ORDER BY created_at, rowid")?;
    let messages: Vec<Message> = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(Value::Null), r.get(3)?, r.get(4)?, r.get(5)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // Each call's start and end where Crush recorded them, and whether it failed.
    let mut results: HashMap<String, (Option<Ns>, Ns, bool)> = HashMap::new();
    for (_, role, parts, .., at) in &messages {
        for d in parts_of(parts, "tool_result").filter(|_| role == "tool") {
            let Some(call) = d["tool_call_id"].as_str() else { continue };
            let meta: Value = d["metadata"].as_str().and_then(|m| serde_json::from_str(m).ok()).unwrap_or(Value::Null);
            let ms = |k: &str| meta[k].as_i64().and_then(|t| t.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t));
            let failed = d["is_error"].as_bool() == Some(true) || (d["name"] == "bash" && exited_non_zero(d["content"].as_str().unwrap_or_default()));
            results.insert(call.to_owned(), (ms("start_time"), ms("end_time").unwrap_or(secs(*at)), failed));
        }
    }
    let mut children: HashMap<String, Ns> = HashMap::new();
    let mut kids = conn.prepare("SELECT id, created_at FROM sessions WHERE parent_session_id = ?1")?;
    for row in kids.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (child, at) = row?;
        children.insert(call_of(&child).to_owned(), secs(at));
    }

    let mut turn: Option<SpanId> = None;
    let mut turns = 0;
    let mut ended = false;
    for (mid, role, parts, model, provider, created) in &messages {
        let at = secs(*created);
        match role.as_str() {
            "user" => {
                if let Some(t) = turn.take() {
                    e.end(format!("end:{t}"), &t, at, SpanStatus::Cancelled, None);
                }
                turns += 1;
                let t = span("turn", mid);
                e.start(&t, at, Some(&top), SpanKind::Turn, &format!("turn {turns}"), |_| {});
                turn = Some(t);
            }
            "assistant" => {
                let within = turn.clone().unwrap_or_else(|| top.clone());
                let llm = span("llm", mid);
                e.start(&llm, at, Some(&within), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                    s.model = model.clone();
                    s.provider = provider.clone();
                });
                let calls: Vec<(&str, &str)> = parts_of(parts, "tool_call").filter_map(|d| Some((d["id"].as_str()?, d["name"].as_str()?))).collect();
                let finish = parts_of(parts, "finish").next();
                let finished = finish.and_then(|f| f["time"].as_i64()).filter(|t| *t > 0).map(secs);
                let tool_start = |call: &str| results.get(call).and_then(|r| r.0).or_else(|| children.get(call).copied()).or_else(|| results.get(call).map(|r| r.1));
                let end = calls.iter().filter_map(|(c, _)| tool_start(c)).chain(finished).min().map(|t| t.max(at));
                let (status, error) = match finish.and_then(|f| f["reason"].as_str()) {
                    Some("error") => (SpanStatus::Failed, Some(finish.and_then(|f| f["message"].as_str()).unwrap_or("model call failed"))),
                    Some("canceled") => (SpanStatus::Cancelled, None),
                    _ => (SpanStatus::Succeeded, None),
                };
                if let Some(end) = end {
                    e.end(format!("end:{llm}"), &llm, end, status, error);
                }
                for (call, name) in calls {
                    let tool = span("tool", &format!("{root}:{call}"));
                    let started = results.get(call).and_then(|r| r.0).or(end).unwrap_or(at);
                    e.start(&tool, started, Some(&within), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                    if let Some(&(_, done, failed)) = results.get(call) {
                        let (status, error) = if failed { (SpanStatus::Failed, Some("tool reported an error (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                        e.end(format!("end:{tool}"), &tool, done.max(started), status, error);
                    }
                }
                let ends_turn = finish.and_then(|f| f["reason"].as_str()).is_some_and(|r| r != "tool_use");
                if let Some(done) = finished.filter(|_| ends_turn) {
                    if let Some(t) = turn.take() {
                        e.end(format!("end:{t}"), &t, done, status, error);
                    }
                    // A sub-agent ends with its first run; a resumed one's later turns nest under it all the same.
                    if parent.is_some() && !ended {
                        ended = true;
                        e.end(format!("end:{top}"), &top, done, status, error);
                    }
                }
            }
            _ => {}
        }
    }
    events.append(&mut e.out);
    Ok(())
}

/// id, role, parts, model, provider, created_at.
type Message = (String, String, Value, Option<String>, Option<String>, i64);

/// The data of each part of `kind`.
fn parts_of<'a>(parts: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    parts.as_array().into_iter().flatten().filter(move |p| p["type"] == kind).map(|p| &p["data"])
}

/// The tool call that started a sub-agent session `<message id>$$<call id>`.
fn call_of(child: &str) -> &str {
    child.rsplit_once("$$").map_or(child, |(_, call)| call)
}

/// Crush appends `Exit code <n>` to a failed command's output, before the `<cwd>` line it always adds.
// ponytail: a command that itself prints such a last line reads as failed.
fn exited_non_zero(content: &str) -> bool {
    content.lines().rev().map(str::trim).find(|l| !l.is_empty() && !l.starts_with("<cwd>")).and_then(|l| l.strip_prefix("Exit code ")).is_some_and(|n| n != "0")
}

fn secs(s: i64) -> Ns {
    s.saturating_mul(1_000_000_000).clamp(0, VALID_TIME_NS.end - 1)
}
