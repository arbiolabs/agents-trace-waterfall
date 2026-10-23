//! OpenClaw sessions, from each agent's `~/.openclaw/agents/<agent>/agent/openclaw-agent.sqlite`
//! (`$OPENCLAW_STATE_DIR` moves `~/.openclaw`); point it at that file, or at the state folder to read
//! every agent's. OpenClaw runs the pi agent, and its transcripts are pi session entries (format
//! version 4): `transcript_events` holds one per row, ordered by `seq` within a session window
//! (`session_windows`, one per `/new` or reset of a session key), as JSON text or, for larger entries,
//! a zstd frame. [`read`] feeds each changed window's entries through the pi adapter, so the spans and
//! blind spots are pi's, after [`as_pi`] fills in what OpenClaw records differently (model call start,
//! shell exit codes). Entries OpenClaw adds (`model_change`, `custom`, ...) are skipped. OpenClaw's own
//! follow-up prompts (a retry after a model timeout, a request for a final answer) are user messages
//! too, and open turns of their own. A window is re-read whole whenever it gets a new entry; event ids
//! derive from entry and call ids, so the store drops what it already has. Metadata only: no prompt,
//! message, thinking or tool payload text is copied.
//!
//! Sub-agents: `sessions_spawn` starts a child session and returns at once with its key in the tool
//! result's `details.childSessionKey`; the child's window names the parent's key in `spawned_by`. The
//! child joins the root session as `subagent:<window>` under that tool call, and outlives it.

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::CanonicalEvent;

use crate::emit::parse_utc;
use crate::pi::Pi;

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(OpenClaw(Pi::recorded_by(harness)))
}

/// pi's adapter, with OpenClaw's own blind spot.
struct OpenClaw(Pi);

impl TraceAdapter for OpenClaw {
    fn source_info(&self) -> SourceInfo {
        let mut info = self.0.source_info();
        info.unobservable.insert(0, "LLM call start (taken from when the previous message was written)".into());
        info
    }

    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        self.0.normalize(raw)
    }
}

/// Events for every session window with entries written after `since_ms` (epoch milliseconds), and the
/// newest write seen.
pub fn read(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.exists(), "{} does not exist; point it at an agent's openclaw-agent.sqlite, or the ~/.openclaw folder", path.display());
    let dbs = if path.is_dir() { agent_dbs(path) } else { vec![path.to_owned()] };
    let (mut events, mut newest) = (Vec::new(), since_ms);
    for db in dbs {
        let (e, n) = read_db(&db, since_ms, harness)?;
        events.extend(e);
        newest = newest.max(n);
    }
    Ok((events, newest))
}

/// `agents/<agent>/agent/openclaw-agent.sqlite` under the state folder, sorted.
fn agent_dbs(state: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(state.join("agents")).into_iter().flatten().flatten().map(|e| e.path().join("agent/openclaw-agent.sqlite")).filter(|p| p.is_file()).collect();
    out.sort();
    out
}

// ponytail: a sub-agent spawned into another agent (`sessions_spawn` with `agentId`) lives in that
// agent's database and stays its own session; search every agent's database if that shows.
fn read_db(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(2))?;
    let changed: Vec<(String, i64)> = conn
        .prepare("SELECT session_id, MAX(created_at) FROM transcript_events GROUP BY session_id HAVING MAX(created_at) > ?1 ORDER BY session_id")?
        .query_map([since_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()
        .with_context(|| format!("{} has no OpenClaw transcript_events table", path.display()))?;
    let newest = changed.iter().map(|(_, t)| *t).max().unwrap_or(since_ms).max(since_ms);
    let mut events = Vec::new();
    for (window, _) in changed {
        let adapter = match spawner(&conn, &window, 0)? {
            Some((root, call)) => Pi::spawned_under(harness, root, call),
            None => Pi::recorded_by(harness),
        };
        let mut asked: Option<i64> = None;
        for line in entries(&conn, &window)? {
            let Ok(mut v) = serde_json::from_str::<Value>(&line) else { continue };
            as_pi(&mut v, &mut asked);
            events.extend(adapter.normalize(RawEvent { line: v.to_string(), observed_at_utc_ns: system_now_ns() })?);
        }
    }
    Ok((events, newest))
}

/// Where OpenClaw's entries differ from pi's. An assistant message's `timestamp` is when the reply was
/// written, not when it was asked for, and there is no `durationMs`: the call is taken to start when the
/// previous message was written (`asked`, epoch ms). A shell command that exits non-zero is no error
/// to OpenClaw (`isError` false, `details.exitCode` set): it is one here.
fn as_pi(v: &mut Value, asked: &mut Option<i64>) {
    let written = v["timestamp"].as_str().and_then(parse_utc).map(|ns| ns / 1_000_000);
    let m = &mut v["message"];
    match m["role"].as_str() {
        Some("assistant") if m.get("durationMs").is_none() => {
            if let (Some(start), Some(end)) = (*asked, m["timestamp"].as_i64()) {
                m["timestamp"] = json!(start.min(end));
                m["durationMs"] = json!((end - start).max(0));
            }
        }
        Some("toolResult") if m["details"]["exitCode"].as_i64().is_some_and(|c| c != 0) => m["isError"] = json!(true),
        _ => {}
    }
    if m["role"].is_string() {
        *asked = written.or(*asked);
    }
}

/// A window's transcript entries in order, as JSON lines.
fn entries(conn: &Connection, window: &str) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT event_json, event_zstd FROM transcript_events WHERE session_id = ?1 ORDER BY seq")?;
    let rows = stmt.query_map([window], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        match row? {
            (Some(json), _) => out.push(json),
            (None, Some(zstd)) => {
                let mut json = String::new();
                ruzstd::decoding::StreamingDecoder::new(zstd.as_slice()).map_err(|e| anyhow::anyhow!("{window}: {e}"))?.read_to_string(&mut json).with_context(|| format!("{window}: decompressing an entry"))?;
                out.push(json);
            }
            (None, None) => {}
        }
    }
    Ok(out)
}

/// For a sub-agent's window: the root session it joins and the parent's `sessions_spawn` call that
/// started it. None for a window nobody spawned, or whose spawning call cannot be found.
// ponytail: a child read before its parent's `sessions_spawn` result is written stays its own session
// until it changes again; defer such windows a few polls if that shows.
fn spawner(conn: &Connection, window: &str, depth: u8) -> anyhow::Result<Option<(String, String)>> {
    // Bounded, so a corrupt cycle cannot hang the collector.
    if depth > 8 {
        return Ok(None);
    }
    let Some((key, parent)) = conn
        .query_row("SELECT session_key, coalesce(spawned_by, parent_session_key) FROM session_windows WHERE session_id = ?1", [window], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .optional()?
        .and_then(|(k, p)| Some((k, p?)))
    else {
        return Ok(None);
    };
    let parents: Vec<String> = conn.prepare("SELECT session_id FROM session_windows WHERE session_key = ?1 ORDER BY created_at")?.query_map([&parent], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for p in parents {
        let call = entries(conn, &p)?.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).find_map(|v| {
            let m = &v["message"];
            (m["role"] == "toolResult" && m["details"]["childSessionKey"].as_str() == Some(&key)).then(|| m["toolCallId"].as_str().map(str::to_owned)).flatten()
        });
        if let Some(call) = call {
            // A parent that is itself a sub-agent passes on its own root.
            let root = spawner(conn, &p, depth + 1)?.map_or(p, |(root, _)| root);
            return Ok(Some((root, call)));
        }
    }
    Ok(None)
}
