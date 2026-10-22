//! Collector daemon, Unix socket protocol server, normalization and adapters.

use std::path::{Path, PathBuf};
use std::time::Duration;
use trace_adapter_sdk::TraceAdapter;
use trace_storage::{AppendOutcome, Store};

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
    let _ = cfg;
    anyhow::bail!("collector not built yet")
}

pub struct Check {
    pub ok: bool,
    pub name: String,
    pub detail: String,
}

/// Socket reachable and speaks the protocol, storage opens and migrates, adapters listed with their blind spots.
pub fn doctor(socket_path: &Path, db_path: &Path) -> Vec<Check> {
    let _ = (socket_path, db_path);
    Vec::new()
}

/// "canonical" (pass-through JSONL) and "claude-code".
pub fn adapter_by_name(name: &str) -> Option<Box<dyn TraceAdapter>> {
    let _ = name;
    None
}

pub const ADAPTERS: &[&str] = &["canonical", "claude-code"];

/// Import a JSONL file through the named adapter into the store.
pub fn import_file(store: &mut Store, path: &Path, adapter: &str) -> anyhow::Result<AppendOutcome> {
    let _ = (store, path, adapter);
    anyhow::bail!("import not built yet")
}
