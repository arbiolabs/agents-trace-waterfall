//! Shared by the conformance tests: the scenario's clock and the shape every harness's recording must have.
#![allow(dead_code)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

pub const MS: Ns = 1_000_000;
/// 2026-10-23T12:00:00Z.
pub const T0_MS: i64 = 1_792_756_800_000;

pub fn dir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = std::env::temp_dir().join(format!("tw-conformance-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn iso(secs: f64) -> String {
    let ms = (secs * 1000.).round() as i64;
    format!("2026-10-23T12:00:{:02}.{:03}Z", ms / 1000, ms % 1000)
}

pub fn ms(secs: f64) -> i64 {
    T0_MS + (secs * 1000.).round() as i64
}

pub fn write_lines(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n").unwrap();
}

/// The shape every harness's recording of the scenario must have.
pub fn conforms(harness: &str, files: Vec<(PathBuf, &'static str)>) {
    let mut store = Store::open_in_memory().unwrap();
    for (f, adapter) in files.iter().rev() {
        import_file(&mut store, f, adapter, &Capture::default()).unwrap_or_else(|e| panic!("{harness}: {e:#}"));
    }
    let sessions = store.list_sessions(&SessionFilter::default()).unwrap();
    assert_eq!(sessions.len(), 1, "{harness}: one session, sub-agent included");
    let summary = &sessions[0];
    assert_eq!((summary.project_id.as_str(), summary.turn_count), ("conf", 1), "{harness}: project and main-agent turns");
    let s = store.load_session(&summary.session_id).unwrap();
    assert!(s.unresolved().is_empty(), "{harness}: every span's parent arrived: {:?}", s.unresolved());

    let kind = |k: SpanKind| s.spans.values().filter(move |x| x.kind == k);
    let roots: Vec<_> = kind(SpanKind::Agent).collect();
    assert_eq!(roots.len(), 1, "{harness}: one agent root");
    let root = roots[0];
    let main_turns: Vec<_> = kind(SpanKind::Turn).filter(|t| t.parent_span_id.as_ref() == Some(&root.span_id)).collect();
    assert_eq!(main_turns.len(), 1, "{harness}: one main turn");
    let turn = main_turns[0];
    assert_eq!(turn.status, SpanStatus::Succeeded, "{harness}: the turn succeeded");

    let under_turn = |k: SpanKind| s.spans.values().filter(|x| x.kind == k && x.parent_span_id.as_ref() == Some(&turn.span_id)).count();
    assert!(under_turn(SpanKind::Llm) >= 3, "{harness}: model calls under the turn: {}", under_turn(SpanKind::Llm));
    for llm in kind(SpanKind::Llm) {
        assert!(llm.model.is_some(), "{harness}: {} names its model", llm.span_id);
        let u = llm.usage.as_ref().unwrap_or_else(|| panic!("{harness}: {} reports usage", llm.span_id));
        assert!(u.tokens_input.is_some() && u.tokens_output.is_some(), "{harness}: {} has token counts", llm.span_id);
    }
    let failed: Vec<_> = kind(SpanKind::Tool).filter(|t| t.status == SpanStatus::Failed).collect();
    assert_eq!(failed.len(), 1, "{harness}: the failing shell command, and only it, failed");

    let subs: Vec<_> = kind(SpanKind::Subagent).collect();
    assert_eq!(subs.len(), 1, "{harness}: one sub-agent");
    let sub = subs[0];
    let spawned_by = sub.parent_span_id.as_ref().and_then(|p| s.spans.get(p));
    assert_eq!(spawned_by.map(|p| p.kind), Some(SpanKind::Tool), "{harness}: the sub-agent nests under the tool call that started it");
    let inside = |id: &SpanId| {
        let mut at = s.spans.get(id).and_then(|x| x.parent_span_id.clone());
        while let Some(p) = at {
            if p == sub.span_id {
                return true;
            }
            at = s.spans.get(&p).and_then(|x| x.parent_span_id.clone());
        }
        false
    };
    assert!(s.spans.values().any(|x| x.kind == SpanKind::Llm && inside(&x.span_id)), "{harness}: the sub-agent's model calls sit inside it");
    assert!(s.spans.values().any(|x| x.kind == SpanKind::Tool && inside(&x.span_id)), "{harness}: the sub-agent's tool calls sit inside it");
    assert_eq!(sub.status, SpanStatus::Succeeded, "{harness}: the sub-agent finished");

    for x in s.spans.values().filter(|x| x.span_id != root.span_id) {
        let end = x.ended_at_utc_ns.unwrap_or_else(|| panic!("{harness}: {} ended", x.span_id));
        assert!(end >= x.started_at_utc_ns, "{harness}: {} ends after it starts", x.span_id);
        let Some(p) = x.parent_span_id.as_ref().and_then(|p| s.spans.get(p)).filter(|p| p.span_id != root.span_id) else { continue };
        // A sub-agent can outlive the call that started it (Codex's spawn_agent returns at once).
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{harness}: {} starts within {}", x.span_id, p.span_id);
        if x.kind != SpanKind::Subagent {
            assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{harness}: {} ends within {}", x.span_id, p.span_id);
        }
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert!(calls.windows(2).all(|w| w[0].1 <= w[1].0 + MS), "{harness}: the main agent's model calls do not overlap: {calls:?}");
}

