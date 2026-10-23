//! Harness integration: adapter contract, event recorder, JSONL helpers and the collector socket client.
//! Std only: no async runtime, storage or TUI dependency.

mod client;
mod recorder;

pub use client::*;
pub use recorder::*;

use anyhow::Context;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use trace_core::{CanonicalEvent, Ns, SourceId};

pub struct SourceInfo {
    pub source_id: SourceId,
    pub name: String,
    pub harness: String,
    /// Timing, usage and payload fields this adapter cannot observe, disclosed to the user.
    pub unobservable: Vec<String>,
}

pub struct RawEvent {
    pub line: String,
    pub observed_at_utc_ns: Ns,
}

pub trait TraceAdapter {
    fn source_info(&self) -> SourceInfo;
    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>>;
}

/// Parses one canonical event per line. Skips blank lines; errors name the 1-based line.
pub fn read_jsonl(r: impl BufRead) -> anyhow::Result<Vec<CanonicalEvent>> {
    let mut out = Vec::new();
    for (i, line) in r.lines().enumerate() {
        let line = line.with_context(|| format!("line {}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).with_context(|| format!("line {}", i + 1))?);
    }
    Ok(out)
}

pub fn write_jsonl(mut w: impl Write, events: &[CanonicalEvent]) -> anyhow::Result<()> {
    for e in events {
        serde_json::to_writer(&mut w, e)?;
        w.write_all(b"\n")?;
    }
    w.flush()?;
    Ok(())
}

/// `$AGENTS_TRACE_WATERFALL_SOCKET`, else `$XDG_RUNTIME_DIR/agents-trace-waterfall.sock`,
/// else `~/.local/state/agents-trace-waterfall/collector.sock`.
pub fn default_socket_path() -> PathBuf {
    socket_path_from(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
}

fn socket_path_from(var: impl Fn(&str) -> Option<PathBuf>) -> PathBuf {
    if let Some(p) = var("AGENTS_TRACE_WATERFALL_SOCKET") {
        return p;
    }
    if let Some(d) = var("XDG_RUNTIME_DIR") {
        return d.join("agents-trace-waterfall.sock");
    }
    var("HOME").unwrap_or_default().join(".local/state/agents-trace-waterfall/collector.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_precedence() {
        let env = |pairs: &'static [(&str, &str)]| move |k: &str| pairs.iter().find(|p| p.0 == k).map(|p| PathBuf::from(p.1));
        assert_eq!(socket_path_from(env(&[("AGENTS_TRACE_WATERFALL_SOCKET", "/a.sock"), ("XDG_RUNTIME_DIR", "/run")])), PathBuf::from("/a.sock"));
        assert_eq!(socket_path_from(env(&[("XDG_RUNTIME_DIR", "/run"), ("HOME", "/h")])), PathBuf::from("/run/agents-trace-waterfall.sock"));
        assert_eq!(socket_path_from(env(&[("HOME", "/h")])), PathBuf::from("/h/.local/state/agents-trace-waterfall/collector.sock"));
    }
}
