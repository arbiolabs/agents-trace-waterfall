//! The conformance check for harnesses that record token usage per turn or per session, never per model
//! call (Grok Build, Hermes Agent): `support::conforms` asserts that every model call reports its own
//! token counts, which such a recording cannot hold. Everything else is asserted unchanged.
#![allow(dead_code)]

use crate::support::MS;
use std::path::PathBuf;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// `support::conforms`, with "each model call reports token counts" replaced by: model calls report no
/// usage of their own, the span `usage_on` (a turn, the agent, a sub-agent) carries the recorded tokens,
/// and the session's totals add up to `tokens` (input, output).
pub fn conforms_with_usage_on(harness: &str, files: Vec<(PathBuf, &'static str)>, usage_on: &[SpanKind], tokens: (u64, u64)) {
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
        assert!(llm.usage.is_none(), "{harness}: {} reports no usage the harness did not record", llm.span_id);
    }
    for x in s.spans.values().filter(|x| x.usage.is_some()) {
        assert!(usage_on.contains(&x.kind), "{harness}: usage sits on {}", x.span_id);
    }
    let totals = s.totals();
    assert_eq!((totals.tokens_input, totals.tokens_output), tokens, "{harness}: recorded tokens, each counted once");

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
        assert!(x.started_at_utc_ns + MS >= p.started_at_utc_ns, "{harness}: {} starts within {}", x.span_id, p.span_id);
        if x.kind != SpanKind::Subagent {
            assert!(end <= p.ended_at_utc_ns.unwrap() + MS, "{harness}: {} ends within {}", x.span_id, p.span_id);
        }
    }
    let mut calls: Vec<_> = s.spans.values().filter(|x| x.kind == SpanKind::Llm && x.parent_span_id.as_ref() == Some(&turn.span_id)).map(|x| (x.started_at_utc_ns, x.ended_at_utc_ns.unwrap())).collect();
    calls.sort();
    assert!(calls.windows(2).all(|w| w[0].1 <= w[1].0 + MS), "{harness}: the main agent's model calls do not overlap: {calls:?}");
}
