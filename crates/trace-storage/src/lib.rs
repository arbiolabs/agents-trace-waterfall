//! SQLite journal of canonical events plus projections rebuildable from it.

mod redact;

pub use redact::redact;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use trace_core::*;

const MIGRATIONS: &[&str] = &[r#"
-- ponytail: no snapshots table; replay checkpoints live in memory in trace_core::Replay.
CREATE TABLE events (
    journal_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    source_seq INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    span_id TEXT,
    event_type TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    observed_at INTEGER NOT NULL,
    json TEXT NOT NULL,
    UNIQUE (source_id, event_id)
);
CREATE INDEX events_by_session ON events (session_id, journal_seq);
CREATE INDEX events_by_source_seq ON events (source_id, source_seq);
CREATE TABLE source_cursors (
    source_id TEXT PRIMARY KEY,
    first_seq INTEGER NOT NULL,
    cursor INTEGER NOT NULL
);
CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    working_directory TEXT NOT NULL,
    source_adapter TEXT NOT NULL,
    harness TEXT NOT NULL,
    host TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    last_event_at INTEGER,
    model TEXT,
    tokens_input INTEGER NOT NULL,
    tokens_output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cost_actual_usd_micros INTEGER NOT NULL,
    cost_estimate_usd_micros INTEGER NOT NULL,
    cost_unknown_records INTEGER NOT NULL,
    billable_records INTEGER NOT NULL,
    failed_spans INTEGER NOT NULL,
    span_count INTEGER NOT NULL
);
CREATE INDEX sessions_by_started ON sessions (started_at);
CREATE INDEX sessions_by_status ON sessions (status, last_event_at);
CREATE INDEX sessions_by_project ON sessions (project_id, started_at);
CREATE TABLE spans (
    session_id TEXT NOT NULL,
    span_id TEXT NOT NULL,
    parent_span_id TEXT,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    model TEXT,
    tool_name TEXT,
    PRIMARY KEY (session_id, span_id)
);
CREATE TABLE usage (
    session_id TEXT NOT NULL,
    span_id TEXT NOT NULL,
    tokens_input INTEGER,
    tokens_output INTEGER,
    cache_read INTEGER,
    cache_write INTEGER,
    cost_usd_micros INTEGER,
    cost_provenance TEXT NOT NULL,
    is_final INTEGER NOT NULL,
    PRIMARY KEY (session_id, span_id)
);
CREATE TABLE payloads (
    payload_ref TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    stored_at INTEGER NOT NULL,
    bytes BLOB NOT NULL
);
CREATE INDEX payloads_by_session ON payloads (session_id);
CREATE INDEX payloads_by_stored ON payloads (stored_at);
"#, r#"
-- The user's notes on spans. Not journal events: a trace records what ran, notes are about it.
CREATE TABLE annotations (
    session_id TEXT NOT NULL,
    span_id TEXT NOT NULL,
    text TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, span_id)
);
"#];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AppendOutcome {
    pub inserted: usize,
    pub duplicates: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurgeOutcome {
    pub sessions: usize,
    pub payloads: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    pub status: Option<SessionStatus>,
    pub project: Option<String>,
    /// Matches `source_adapter`.
    pub source: Option<String>,
    /// Case-insensitive substring of id, project, directory, harness, host or model.
    pub text: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub project_id: String,
    pub working_directory: String,
    pub source_adapter: String,
    pub harness: String,
    pub host: String,
    pub status: SessionStatus,
    pub started_at_utc_ns: Option<Ns>,
    pub ended_at_utc_ns: Option<Ns>,
    pub last_event_at_utc_ns: Option<Ns>,
    /// Most used LLM model.
    pub model: Option<String>,
    pub totals: Totals,
    pub span_count: usize,
    pub turn_count: usize,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        create_private(path)?;
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        let store = Self::init(conn)?;
        restrict_permissions(path)?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))?;
        // FULL so an acked commit survives power loss, not only a process crash (docs/protocol.md).
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "secure_delete", "ON")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i as i64 + 1)?;
            tx.commit()?;
        }
        Ok(())
    }

    /// One transaction. Idempotent on (source_id, event_id): duplicates are skipped and counted.
    /// Redacts known secrets in string values before writing. Updates projections for touched sessions.
    pub fn append(&mut self, events: &[CanonicalEvent]) -> Result<AppendOutcome> {
        let tx = self.conn.transaction()?;
        let mut out = AppendOutcome::default();
        let mut sessions = BTreeSet::new();
        let mut sources: BTreeMap<&SourceId, u64> = BTreeMap::new();
        for e in events {
            let r = redacted(e)?;
            let n = tx.execute(
                "INSERT OR IGNORE INTO events (source_id, event_id, source_seq, session_id, span_id, event_type, occurred_at, observed_at, json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    r.source_id.0,
                    r.event_id.0,
                    r.source_seq as i64,
                    r.session_id.0,
                    r.span_id.as_ref().map(|s| &s.0),
                    r.body.type_name(),
                    r.occurred_at_utc_ns,
                    r.observed_at_utc_ns,
                    serde_json::to_string(&r)?,
                ],
            )?;
            if n == 0 {
                out.duplicates += 1;
                continue;
            }
            out.inserted += 1;
            sessions.insert(r.session_id);
            let min = sources.entry(&e.source_id).or_insert(e.source_seq);
            *min = (*min).min(e.source_seq);
        }
        for (source, min) in sources {
            advance_cursor(&tx, source, min as i64)?;
        }
        for id in &sessions {
            project(&tx, id)?;
        }
        tx.commit()?;
        Ok(out)
    }

    /// Highest source_seq such that every seq from the source's lowest seen seq up to it is persisted.
    pub fn source_cursor(&self, source: &SourceId) -> Result<Option<u64>> {
        Ok(cursor_row(&self.conn, source)?.map(|(_, c)| c as u64))
    }

    /// Missing seq ranges (inclusive) above the contiguous cursor.
    pub fn source_gaps(&self, source: &SourceId) -> Result<Vec<(u64, u64)>> {
        let Some(mut prev) = self.source_cursor(source)? else { return Ok(Vec::new()) };
        let mut gaps = Vec::new();
        for seq in seqs_above(&self.conn, source, prev as i64)? {
            let seq = seq as u64;
            if seq > prev + 1 {
                gaps.push((prev + 1, seq - 1));
            }
            prev = seq;
        }
        Ok(gaps)
    }

    pub fn list_sessions(&self, filter: &SessionFilter) -> Result<Vec<SessionSummary>> {
        let mut sql = String::from(
            "SELECT *, (SELECT COUNT(*) FROM spans p WHERE p.session_id = sessions.session_id AND p.kind = 'turn') FROM sessions WHERE 1 = 1",
        );
        let mut args: Vec<String> = Vec::new();
        let mut bind = |sql: &mut String, clause: &str, arg: String| {
            args.push(arg);
            sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
        };
        if let Some(s) = filter.status {
            bind(&mut sql, " AND status = ?", wire_name(&s));
        }
        if let Some(p) = &filter.project {
            bind(&mut sql, " AND project_id = ?", p.clone());
        }
        if let Some(s) = &filter.source {
            bind(&mut sql, " AND source_adapter = ?", s.clone());
        }
        if let Some(t) = &filter.text {
            let cols = ["session_id", "project_id", "working_directory", "harness", "host", "COALESCE(model, '')"];
            let any = cols.map(|c| format!("instr(lower({c}), ?) > 0")).join(" OR ");
            bind(&mut sql, &format!(" AND ({any})"), t.to_lowercase());
        }
        sql.push_str(" ORDER BY COALESCE(started_at, last_event_at) DESC, session_id");
        if let Some(n) = filter.limit {
            sql.push_str(&format!(" LIMIT {n}"));
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(&args), summary_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Events in replay order.
    pub fn session_events(&self, id: &SessionId) -> Result<Vec<CanonicalEvent>> {
        session_events(&self.conn, id)
    }

    pub fn load_session(&self, id: &SessionId) -> Result<TraceState> {
        Ok(TraceState::from_events(id.clone(), &self.session_events(id)?))
    }

    /// Journal rows after `after`, optionally for one session. The live view polls this.
    pub fn events_since(&self, after: i64, session: Option<&SessionId>) -> Result<Vec<(i64, CanonicalEvent)>> {
        let mut stmt = self.conn.prepare(
            "SELECT journal_seq, json FROM events WHERE journal_seq > ?1 AND (?2 IS NULL OR session_id = ?2) ORDER BY journal_seq",
        )?;
        let rows = stmt.query_map(params![after, session.map(|s| &s.0)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|r| {
            let (seq, json) = r?;
            Ok((seq, serde_json::from_str(&json)?))
        })
        .collect()
    }

    pub fn max_journal_seq(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(journal_seq), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Ends stale `live` sessions with a synthetic `session.ended {status: interrupted}` at their last
    /// event time. The event id is derived from the session and that time, so re-running is idempotent
    /// while a session revived by later activity can be interrupted again.
    pub fn reconcile_interrupted(&mut self, stale_before_ns: Ns, now_ns: Ns) -> Result<Vec<SessionId>> {
        let stale: Vec<(String, Ns)> = {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, last_event_at FROM sessions WHERE status = ?1 AND last_event_at < ?2 ORDER BY session_id",
            )?;
            let rows = stmt.query_map(params![wire_name(&SessionStatus::Live), stale_before_ns], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let events: Vec<_> = stale
            .iter()
            .map(|(id, last)| CanonicalEvent {
                schema_version: SCHEMA_VERSION,
                source_id: COLLECTOR_SOURCE.into(),
                source_seq: 0,
                event_id: EventId(format!("interrupted:{id}:{last}")),
                observed_at_utc_ns: now_ns,
                occurred_at_utc_ns: *last,
                session_id: id.as_str().into(),
                span_id: None,
                body: EventBody::SessionEnded { status: SessionStatus::Interrupted },
            })
            .collect();
        self.append(&events)?;
        Ok(events.into_iter().map(|e| e.session_id).collect())
    }

    /// Deletes whole sessions that ended before `sessions_before_ns`, and separately
    /// payload blobs stored before `payloads_before_ns`.
    pub fn purge(&mut self, sessions_before_ns: Option<Ns>, payloads_before_ns: Option<Ns>) -> Result<PurgeOutcome> {
        let tx = self.conn.transaction()?;
        let mut out = PurgeOutcome::default();
        if let Some(cutoff) = sessions_before_ns {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT session_id FROM sessions WHERE ended_at < ?1")?;
                stmt.query_map([cutoff], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
            };
            for id in &ids {
                for table in ["events", "spans", "usage", "payloads", "annotations", "sessions"] {
                    tx.execute(&format!("DELETE FROM {table} WHERE session_id = ?1"), [id])?;
                }
            }
            out.sessions = ids.len();
        }
        if let Some(cutoff) = payloads_before_ns {
            out.payloads = tx.execute("DELETE FROM payloads WHERE stored_at < ?1", [cutoff])?;
        }
        tx.commit()?;
        Ok(out)
    }

    /// Stores an opt-in payload after redaction.
    // ponytail: stored uncompressed; add zstd/deflate when payload volume shows up in db size.
    pub fn put_payload(&mut self, payload_ref: &str, session: &SessionId, bytes: &[u8]) -> Result<()> {
        let stored_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;
        self.conn.execute(
            "INSERT OR REPLACE INTO payloads (payload_ref, session_id, stored_at, bytes) VALUES (?1, ?2, ?3, ?4)",
            params![payload_ref, session.0, stored_at, redact::redact_bytes(bytes).as_ref()],
        )?;
        Ok(())
    }

    pub fn get_payload(&self, payload_ref: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.conn.query_row("SELECT bytes FROM payloads WHERE payload_ref = ?1", [payload_ref], |r| r.get(0)).optional()?)
    }

    /// The session's span notes.
    pub fn annotations(&self, session: &SessionId) -> Result<BTreeMap<SpanId, String>> {
        let mut stmt = self.conn.prepare("SELECT span_id, text FROM annotations WHERE session_id = ?1")?;
        let rows = stmt.query_map([&session.0], |r| Ok((SpanId(r.get(0)?), r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the span's note after redaction; blank text removes it.
    pub fn set_annotation(&mut self, session: &SessionId, span: &SpanId, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            self.conn.execute("DELETE FROM annotations WHERE session_id = ?1 AND span_id = ?2", params![session.0, span.0])?;
            return Ok(());
        }
        let updated_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as i64;
        self.conn.execute(
            "INSERT OR REPLACE INTO annotations (session_id, span_id, text, updated_at) VALUES (?1, ?2, ?3, ?4)",
            params![session.0, span.0, redact::redact(text), updated_at],
        )?;
        Ok(())
    }

    /// Recomputes every projection from the journal.
    pub fn rebuild_projections(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM sessions; DELETE FROM spans; DELETE FROM usage;")?;
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT DISTINCT session_id FROM events")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for id in ids {
            project(&tx, &SessionId(id))?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// `$TRACE_WATERFALL_DB`, else `~/.local/share/trace-waterfall/traces.db`.
pub fn default_db_path() -> PathBuf {
    if let Some(p) = std::env::var_os("TRACE_WATERFALL_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".local/share/trace-waterfall/traces.db")
}

fn redacted(e: &CanonicalEvent) -> Result<CanonicalEvent> {
    let mut v = serde_json::to_value(e)?;
    if let Some(body) = v.get_mut("body") {
        redact_value(body);
    }
    Ok(serde_json::from_value(v)?)
}

fn redact_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            let r = match redact(s) {
                Cow::Owned(r) => r,
                Cow::Borrowed(_) => return,
            };
            *s = r;
        }
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        Value::Object(o) => o.values_mut().for_each(redact_value),
        _ => {}
    }
}

/// snake_case serde name of a trace-core enum.
fn session_events(conn: &Connection, id: &SessionId) -> Result<Vec<CanonicalEvent>> {
    let mut stmt = conn.prepare("SELECT json FROM events WHERE session_id = ?1")?;
    let rows = stmt.query_map([&id.0], |r| r.get::<_, String>(0))?;
    let mut events = rows.map(|r| Ok(serde_json::from_str(&r?)?)).collect::<Result<Vec<CanonicalEvent>>>()?;
    sort_events(&mut events);
    Ok(events)
}

fn cursor_row(conn: &Connection, source: &SourceId) -> Result<Option<(i64, i64)>> {
    Ok(conn
        .query_row("SELECT first_seq, cursor FROM source_cursors WHERE source_id = ?1", [&source.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

fn seqs_above(conn: &Connection, source: &SourceId, after: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT source_seq FROM events WHERE source_id = ?1 AND source_seq > ?2 ORDER BY source_seq")?;
    Ok(stmt.query_map(params![source.0, after], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
}

/// Highest seq reachable from `from` without a hole.
fn walk(conn: &Connection, source: &SourceId, mut from: i64) -> Result<i64> {
    for seq in seqs_above(conn, source, from)? {
        if seq != from + 1 {
            break;
        }
        from = seq;
    }
    Ok(from)
}

fn advance_cursor(conn: &Connection, source: &SourceId, batch_min: i64) -> Result<()> {
    let (first, cursor) = match cursor_row(conn, source)? {
        None => (batch_min, walk(conn, source, batch_min)?),
        Some((first, old)) if batch_min >= first => (first, walk(conn, source, old)?),
        Some((first, old)) => {
            let c = walk(conn, source, batch_min)?;
            (batch_min, if c + 1 >= first { walk(conn, source, c.max(old))? } else { c })
        }
    };
    conn.execute(
        "INSERT OR REPLACE INTO source_cursors (source_id, first_seq, cursor) VALUES (?1, ?2, ?3)",
        params![source.0, first, cursor],
    )?;
    Ok(())
}

/// Refolds one session's journal and replaces its projection rows.
fn project(conn: &Connection, id: &SessionId) -> Result<()> {
    // ponytail: O(session events) refold on every append touching the session; fine to ~10k events per
    // session. Upgrade path: keep TraceState per live session in memory and apply only the new events.
    let st = TraceState::from_events(id.clone(), &session_events(conn, id)?);
    conn.execute("DELETE FROM spans WHERE session_id = ?1", [&id.0])?;
    conn.execute("DELETE FROM usage WHERE session_id = ?1", [&id.0])?;
    let mut model_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for s in st.spans.values() {
        conn.execute(
            "INSERT INTO spans (session_id, span_id, parent_span_id, name, kind, status, started_at, ended_at, model, tool_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id.0,
                s.span_id.0,
                s.parent_span_id.as_ref().map(|p| &p.0),
                s.name,
                wire_name(&s.kind),
                wire_name(&s.status),
                s.started_at_utc_ns,
                s.ended_at_utc_ns,
                s.model,
                s.tool_name,
            ],
        )?;
        if let Some(u) = &s.usage {
            let n = |v: Option<u64>| v.map(|v| v as i64);
            conn.execute(
                "INSERT INTO usage (session_id, span_id, tokens_input, tokens_output, cache_read, cache_write, cost_usd_micros, cost_provenance, is_final)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id.0,
                    s.span_id.0,
                    n(u.tokens_input),
                    n(u.tokens_output),
                    n(u.cache_read),
                    n(u.cache_write),
                    n(u.cost_usd_micros),
                    wire_name(&u.cost_provenance),
                    s.usage_final,
                ],
            )?;
        }
        if let (SpanKind::Llm, Some(m)) = (s.kind, &s.model) {
            *model_counts.entry(m).or_default() += 1;
        }
    }
    let model = model_counts.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0))).map(|(m, _)| m);
    let t = st.totals();
    let s = &st.session;
    conn.execute(
        "INSERT OR REPLACE INTO sessions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            id.0,
            s.project_id,
            s.working_directory,
            s.source_adapter,
            s.harness,
            s.host,
            wire_name(&s.status),
            s.started_at_utc_ns,
            s.ended_at_utc_ns,
            st.last_occurred_at_utc_ns,
            model,
            t.tokens_input as i64,
            t.tokens_output as i64,
            t.cache_read as i64,
            t.cache_write as i64,
            t.cost_actual_usd_micros as i64,
            t.cost_estimate_usd_micros as i64,
            t.cost_unknown_records as i64,
            t.billable_records as i64,
            t.failed_spans as i64,
            st.spans.len() as i64,
        ],
    )?;
    Ok(())
}

fn summary_row(r: &rusqlite::Row) -> rusqlite::Result<SessionSummary> {
    let u = |i: usize| r.get::<_, i64>(i).map(|v| v as u64);
    let status: String = r.get(6)?;
    Ok(SessionSummary {
        session_id: SessionId(r.get(0)?),
        project_id: r.get(1)?,
        working_directory: r.get(2)?,
        source_adapter: r.get(3)?,
        harness: r.get(4)?,
        host: r.get(5)?,
        status: serde_json::from_value(Value::String(status)).unwrap_or(SessionStatus::Unknown),
        started_at_utc_ns: r.get(7)?,
        ended_at_utc_ns: r.get(8)?,
        last_event_at_utc_ns: r.get(9)?,
        model: r.get(10)?,
        totals: Totals {
            tokens_input: u(11)?,
            tokens_output: u(12)?,
            cache_read: u(13)?,
            cache_write: u(14)?,
            cost_actual_usd_micros: u(15)?,
            cost_estimate_usd_micros: u(16)?,
            cost_unknown_records: u(17)? as usize,
            billable_records: u(18)? as usize,
            failed_spans: u(19)? as usize,
        },
        span_count: u(20)? as usize,
        turn_count: u(21)? as usize,
    })
}

#[cfg(unix)]
fn create_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        if std::fs::exists(&p)? {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private(_: &Path) -> Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_: &Path) -> Result<()> {
    Ok(())
}
