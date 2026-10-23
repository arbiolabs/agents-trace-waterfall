//! Kilo Code CLI (`@kilocode/cli`, an OpenCode fork) sessions, from `~/.local/share/kilo/kilo.db`
//! (`$XDG_DATA_HOME/kilo/kilo.db`; `$KILO_DB` overrides it, and dev channels use `kilo-<channel>.db`).
//! Kilo keeps OpenCode's earlier SQLite tables, `session`, `message` and `part`, and writes each reply as
//! a 1.x-style message whose parts are its steps and tool calls, so [`crate::opencode`] reads it as is:
//! the spans, sub-agents (child sessions named by the task tool's `metadata.sessionId`), failed shell
//! commands (`metadata.exit`) and blind spots are OpenCode's.
//!
//! Kilo also has OpenCode 2.x's `session_message` table, but keyed to `session` (no `session_v2`): its
//! event projector writes only some events there, such as the user's own shell commands, which the
//! `message` and `part` rows hold too. Those rows are not read.

use std::path::Path;
use trace_adapter_sdk::TraceAdapter;
use trace_core::CanonicalEvent;

use crate::opencode::{OpenCode, read_db_as};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(OpenCode::recorded_by(harness))
}

pub fn read(path: &Path, since: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    read_db_as(path, since, harness)
}
