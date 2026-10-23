//! Collector daemon, Unix socket protocol server, normalization and adapters.

mod claude_code;
mod cline;
mod codex;
mod collector;
mod copilot;
mod crush;
mod emit;
mod gemini;
mod grok;
mod hermes;
mod kilo;
mod kimi;
mod kimi_code;
mod openclaw;
mod opencode;
mod otlp;
mod pi;
mod qwen;

pub use collector::run_collector_until;

use anyhow::{Context, ensure};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use trace_adapter_sdk::{
    MAX_LINE_BYTES, PROTOCOL_VERSION, RawEvent, SourceInfo, TraceAdapter, system_now_ns,
};
use trace_core::{CanonicalEvent, EventBody, SessionId};
use trace_storage::{AppendOutcome, SessionFilter, Store};

pub struct CollectorConfig {
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
    /// Live sessions silent for longer than this are reconciled as interrupted.
    pub stale_after: Duration,
    /// Files to tail with a named adapter, e.g. ("claude-code", ~/.claude/projects).
    pub tail: Vec<(String, PathBuf)>,
    pub capture: Capture,
    /// Where the OTLP/HTTP trace receiver listens (loopback only), if anywhere.
    pub otlp: Option<std::net::SocketAddr>,
}

/// Opt-in payload capture (docs/privacy.md). Without it the collector strips input and output previews.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Capture {
    pub payloads: bool,
    pub projects: Vec<String>,
}

impl Capture {
    /// `projects` maps sessions to projects as `session.started` events pass through.
    // ponytail: the map lives in memory, so after a restart previews of already-started sessions are stripped
    // (the safe side); look the project up in the store if that matters.
    pub fn strip(&self, projects: &mut HashMap<SessionId, String>, events: &mut [CanonicalEvent]) {
        for e in events {
            if let EventBody::SessionStarted { project_id, .. } = &e.body {
                projects.insert(e.session_id.clone(), project_id.clone());
            }
            if self.payloads || projects.get(&e.session_id).is_some_and(|p| self.projects.contains(p)) {
                continue;
            }
            match &mut e.body {
                EventBody::SpanStarted { input_preview, .. } => *input_preview = None,
                EventBody::SpanEnded { output_preview, .. } => *output_preview = None,
                _ => {}
            }
        }
    }
}

/// Where each harness keeps its sessions by default, for those present on this machine: what the collector
/// tails when the config names nothing.
pub fn default_tails() -> Vec<(String, PathBuf)> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    [
        ("claude-code", ".claude/projects"),
        ("codex", ".codex/sessions"),
        ("codex", ".codex/archived_sessions"),
        ("pi", ".pi/agent/sessions"),
        ("omp", ".omp/agent/sessions"),
        ("opencode", ".local/share/opencode/opencode.db"),
        ("kilo", ".local/share/kilo/kilo.db"),
        ("gemini-cli", ".gemini/tmp"),
        ("qwen-code", ".qwen/projects"),
        ("copilot", ".copilot/session-state"),
        ("cline", "Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/tasks"),
        ("cline", ".cline/data"),
        ("roo", "Library/Application Support/Code/User/globalStorage/rooveterinaryinc.roo-cline/tasks"),
        ("kimi", ".kimi"),
        ("kimi-code", ".kimi-code"),
        ("crush", ".local/share/crush/projects.json"),
        ("openclaw", ".openclaw"),
        ("hermes", ".hermes/state.db"),
        ("grok", ".grok/sessions"),
    ]
    .into_iter()
    .map(|(a, p)| (a.to_owned(), home.join(p)))
    .filter(|(_, p)| p.exists())
    .collect()
}

/// Runs until SIGINT/SIGTERM. Blocking; owns its own Tokio runtime.
pub fn run_collector(cfg: CollectorConfig) -> anyhow::Result<()> {
    collector::run_until_signal(cfg)
}

/// Runs the collector inside another program until that program exits, leaving its signals alone.
pub fn run_collector_in_process(cfg: CollectorConfig) -> anyhow::Result<()> {
    let (_keep, stop) = tokio::sync::oneshot::channel();
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

/// Adapters by name. Some sources are not appended transcripts (OpenCode's SQLite, for one): those also
/// have a [`snapshot_reader`], and their line adapter reads the lines that reader makes.
pub fn adapter_by_name(name: &str) -> Option<Box<dyn TraceAdapter>> {
    match name {
        "canonical" => Some(Box::new(Canonical)),
        "claude-code" => Some(Box::new(claude_code::ClaudeCode::default())),
        "codex" => Some(Box::new(codex::Codex::default())),
        "pi" => Some(Box::new(pi::Pi::default())),
        "omp" => Some(Box::new(pi::Pi::recorded_by("omp"))),
        "opencode" => Some(Box::new(opencode::OpenCode::default())),
        "gemini-cli" => Some(gemini::adapter("gemini-cli")),
        "qwen-code" => Some(qwen::adapter("qwen-code")),
        "kilo" => Some(kilo::adapter("kilo")),
        "openclaw" => Some(openclaw::adapter("openclaw")),
        "copilot" => Some(copilot::adapter("copilot")),
        "cline" => Some(cline::adapter("cline")),
        "roo" => Some(cline::adapter("roo")),
        "kimi" => Some(kimi::adapter("kimi")),
        "kimi-code" => Some(kimi_code::adapter("kimi-code")),
        "crush" => Some(crush::adapter("crush")),
        "hermes" => Some(hermes::adapter("hermes")),
        "grok" => Some(grok::adapter("grok")),
        "otlp" => Some(Box::new(otlp::Otlp::default())),
        _ => None,
    }
}

pub const ADAPTERS: &[&str] = &["canonical", "claude-code", "codex", "pi", "omp", "opencode", "gemini-cli", "qwen-code", "kilo", "openclaw", "copilot", "cline", "roo", "kimi", "kimi-code", "crush", "hermes", "grok", "otlp"];

/// Reads a source that is not an appended transcript (a database, a JSON file rewritten in place): every
/// session changed since the cursor, and the new cursor.
pub(crate) type SnapshotRead = Box<dyn Fn(&Path, i64) -> anyhow::Result<(Vec<CanonicalEvent>, i64)>>;

/// The snapshot reader of an adapter whose source is read whole, or None for a JSONL transcript.
pub(crate) fn snapshot_reader(adapter: &str) -> Option<SnapshotRead> {
    macro_rules! snapshot {
        ($m:ident, $name:expr) => {
            $m::SNAPSHOT.then(|| Box::new(move |p: &Path, since: i64| $m::read(p, since, $name)) as SnapshotRead)
        };
    }
    match adapter {
        "opencode" => Some(Box::new(opencode::read_db)),
        "gemini-cli" => snapshot!(gemini, "gemini-cli"),
        "qwen-code" => snapshot!(qwen, "qwen-code"),
        "kilo" => snapshot!(kilo, "kilo"),
        "openclaw" => snapshot!(openclaw, "openclaw"),
        "copilot" => snapshot!(copilot, "copilot"),
        "cline" => snapshot!(cline, "cline"),
        "roo" => snapshot!(cline, "roo"),
        "kimi" => snapshot!(kimi, "kimi"),
        "kimi-code" => snapshot!(kimi_code, "kimi-code"),
        "crush" => snapshot!(crush, "crush"),
        "hermes" => snapshot!(hermes, "hermes"),
        "grok" => snapshot!(grok, "grok"),
        _ => None,
    }
}

/// Import a JSONL file through the named adapter into the store, applying the same capture policy as the collector.
pub fn import_file(store: &mut Store, path: &Path, adapter: &str, capture: &Capture) -> anyhow::Result<AppendOutcome> {
    let a = adapter_by_name(adapter).with_context(|| {
        format!(
            "unknown adapter {adapter:?}; known: {}",
            ADAPTERS.join(", ")
        )
    })?;
    if let Some(read) = snapshot_reader(adapter) {
        let (mut events, _) = read(path, i64::MIN)?;
        capture.strip(&mut HashMap::new(), &mut events);
        return store.append(&events);
    }
    let reader = open_transcript(path)?;
    let mut out = AppendOutcome::default();
    let mut batch = Vec::new();
    let mut projects = HashMap::new();
    let mut flush = |batch: &mut Vec<CanonicalEvent>| -> anyhow::Result<()> {
        capture.strip(&mut projects, batch);
        let r = store.append(batch)?;
        out.inserted += r.inserted;
        out.duplicates += r.duplicates;
        batch.clear();
        Ok(())
    };
    for (i, line) in BufReader::new(reader).lines().enumerate() {
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

/// A transcript's bytes: zstd-compressed (`*.jsonl.zst`, as Codex keeps old rollouts) or plain.
pub(crate) fn open_transcript(path: &Path) -> anyhow::Result<Box<dyn std::io::Read>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    if path.extension().is_some_and(|e| e == "zst") {
        let decoder = ruzstd::decoding::StreamingDecoder::new(file).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        return Ok(Box::new(decoder));
    }
    Ok(Box::new(file))
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
        e.validate().map_err(anyhow::Error::msg)?;
        Ok(vec![e])
    }
}
