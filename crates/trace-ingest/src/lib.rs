//! Collector daemon, Unix socket protocol server, normalization and adapters.

mod claude_code;
mod collector;

pub use collector::run_collector_until;

use anyhow::{Context, ensure};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use trace_adapter_sdk::{
    MAX_LINE_BYTES, PROTOCOL_VERSION, RawEvent, SourceInfo, TraceAdapter, system_now_ns,
};
use trace_core::{CanonicalEvent, EventBody, SCHEMA_VERSION};
use trace_storage::{AppendOutcome, SessionFilter, Store};

pub struct CollectorConfig {
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
    /// Live sessions silent for longer than this are reconciled as interrupted.
    pub stale_after: Duration,
    /// Files to tail with a named adapter, e.g. ("claude-code", ~/.claude/projects).
    pub tail: Vec<(String, PathBuf)>,
}

/// Runs until SIGINT/SIGTERM. Blocking; owns its own Tokio runtime.
pub fn run_collector(cfg: CollectorConfig) -> anyhow::Result<()> {
    let (_never, stop) = tokio::sync::oneshot::channel();
    run_collector_until(cfg, stop)
}

pub struct Check {
    pub ok: bool,
    pub name: String,
    pub detail: String,
}

fn check(name: &str, r: anyhow::Result<String>) -> Check {
    let (ok, detail) = match r {
        Ok(d) => (true, d),
        Err(e) => (false, format!("{e:#}")),
    };
    Check {
        ok,
        name: name.to_owned(),
        detail,
    }
}

/// Socket reachable and speaks the protocol, storage opens and migrates, adapters listed with their blind spots.
pub fn doctor(socket_path: &Path, db_path: &Path) -> Vec<Check> {
    let mut checks = vec![
        check(
            "storage",
            (|| {
                let store = Store::open(db_path)?;
                let n = store.list_sessions(&SessionFilter::default())?.len();
                Ok(format!(
                    "{} opens and migrates, {n} sessions",
                    db_path.display()
                ))
            })(),
        ),
        check(
            "database permissions",
            (|| {
                let mode = std::fs::metadata(db_path)?.permissions().mode() & 0o777;
                ensure!(
                    mode == 0o600,
                    "{} has mode {mode:o}, expected 600",
                    db_path.display()
                );
                Ok("600".to_owned())
            })(),
        ),
        check(
            "socket",
            (|| {
                ensure!(
                    socket_path.exists(),
                    "{} does not exist; run `trace-tui collector start`",
                    socket_path.display()
                );
                Ok(socket_path.display().to_string())
            })(),
        ),
        check("collector", hello(socket_path)),
    ];
    for name in ADAPTERS {
        let info = adapter_by_name(name)
            .expect("ADAPTERS names known adapters")
            .source_info();
        let detail = if info.unobservable.is_empty() {
            "observes everything its source sends".to_owned()
        } else {
            format!("cannot observe: {}", info.unobservable.join("; "))
        };
        checks.push(Check {
            ok: true,
            name: format!("adapter {name}"),
            detail,
        });
    }
    checks
}

fn hello(socket_path: &Path) -> anyhow::Result<String> {
    let mut s = UnixStream::connect(socket_path)
        .with_context(|| format!("connect {}", socket_path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(
        s,
        "{}",
        json!({"type": "hello", "protocol": PROTOCOL_VERSION, "source_id": "trace-doctor"})
    )?;
    let mut line = String::new();
    BufReader::new(std::io::Read::take(s, MAX_LINE_BYTES as u64)).read_line(&mut line)?;
    let reply: serde_json::Value =
        serde_json::from_str(&line).with_context(|| format!("bad reply {line:?}"))?;
    ensure!(
        reply["type"] == "welcome" && reply["protocol"] == PROTOCOL_VERSION,
        "unexpected reply {reply}"
    );
    Ok(format!(
        "answers hello with welcome (protocol {PROTOCOL_VERSION})"
    ))
}

/// "canonical" (pass-through JSONL) and "claude-code".
pub fn adapter_by_name(name: &str) -> Option<Box<dyn TraceAdapter>> {
    match name {
        "canonical" => Some(Box::new(Canonical)),
        "claude-code" => Some(Box::new(claude_code::ClaudeCode::default())),
        _ => None,
    }
}

pub const ADAPTERS: &[&str] = &["canonical", "claude-code"];

/// Import a JSONL file through the named adapter into the store.
pub fn import_file(store: &mut Store, path: &Path, adapter: &str) -> anyhow::Result<AppendOutcome> {
    let a = adapter_by_name(adapter).with_context(|| {
        format!(
            "unknown adapter {adapter:?}; known: {}",
            ADAPTERS.join(", ")
        )
    })?;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = AppendOutcome::default();
    let mut batch = Vec::new();
    let mut flush = |batch: &mut Vec<CanonicalEvent>| -> anyhow::Result<()> {
        let r = store.append(batch)?;
        out.inserted += r.inserted;
        out.duplicates += r.duplicates;
        batch.clear();
        Ok(())
    };
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let raw = RawEvent {
            line: line?,
            observed_at_utc_ns: system_now_ns(),
        };
        batch.extend(
            a.normalize(raw)
                .with_context(|| format!("{}:{}", path.display(), i + 1))?,
        );
        if batch.len() >= 1000 {
            flush(&mut batch)?;
        }
    }
    flush(&mut batch)?;
    Ok(out)
}

/// Envelope checks for events from outside the process (socket frames, canonical JSONL).
pub(crate) fn validate(e: &CanonicalEvent) -> anyhow::Result<()> {
    ensure!(
        e.schema_version == SCHEMA_VERSION,
        "event {}: schema_version {} is not {SCHEMA_VERSION}",
        e.event_id,
        e.schema_version
    );
    ensure!(
        !e.source_id.0.is_empty() && !e.event_id.0.is_empty() && !e.session_id.0.is_empty(),
        "source_id, event_id and session_id must be non-empty"
    );
    ensure!(
        e.source_seq >= 1,
        "event {}: source_seq starts at 1",
        e.event_id
    );
    let session_level = matches!(
        e.body,
        EventBody::SessionStarted { .. } | EventBody::SessionEnded { .. } | EventBody::Heartbeat {}
    );
    ensure!(
        session_level || e.span_id.is_some(),
        "event {}: {} needs a span_id",
        e.event_id,
        e.body.type_name()
    );
    Ok(())
}

struct Canonical;

impl TraceAdapter for Canonical {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: "canonical".into(),
            name: "canonical".into(),
            harness: String::new(),
            unobservable: Vec::new(),
        }
    }

    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        if raw.line.trim().is_empty() {
            return Ok(Vec::new());
        }
        let e: CanonicalEvent = serde_json::from_str(&raw.line)?;
        validate(&e)?;
        Ok(vec![e])
    }
}
