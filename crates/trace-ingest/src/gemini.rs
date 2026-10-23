//! Gemini CLI: not implemented yet. A harness adapter exposes three things, all used by `lib.rs`:
//! - [`adapter`]: a line-by-line [`TraceAdapter`] for transcripts appended as JSONL;
//! - [`SNAPSHOT`]: true when the source is instead read whole (a database, a JSON file rewritten in place);
//! - [`read`]: for snapshot sources, every session changed since `since` (an adapter-defined cursor, e.g.
//!   epoch milliseconds) as canonical events, and the new cursor.
//! `harness` names who recorded the session, so one parser can serve a fork under its own name.

use std::path::Path;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter};
use trace_core::CanonicalEvent;

pub const SNAPSHOT: bool = false;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Stub(harness))
}

pub fn read(_path: &Path, since: i64, _harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    Ok((Vec::new(), since))
}

struct Stub(&'static str);

impl TraceAdapter for Stub {
    fn source_info(&self) -> SourceInfo {
        SourceInfo { source_id: self.0.into(), name: self.0.into(), harness: self.0.into(), unobservable: vec!["everything (adapter not implemented yet)".into()] }
    }

    fn normalize(&self, _raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        Ok(Vec::new())
    }
}
