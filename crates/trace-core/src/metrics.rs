use crate::event::*;
use crate::state::{Span, TraceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed {
    pub ns: Ns,
    /// End precedes start: clocks are skewed, so the duration was clamped to zero.
    pub skewed: bool,
}

impl Span {
    /// End, or the view clock for an open span. An open span in an ended session stops at the session end.
    pub fn effective_end(&self, session_end: Option<Ns>, now: Ns) -> Ns {
        self.ended_at_utc_ns.or(session_end).map_or(now, |e| e.min(now.max(self.started_at_utc_ns)))
    }

    pub fn elapsed(&self, session_end: Option<Ns>, now: Ns) -> Elapsed {
        let d = self.effective_end(session_end, now) - self.started_at_utc_ns;
        Elapsed { ns: d.max(0), skewed: d < 0 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_actual_usd_micros: u64,
    pub cost_estimate_usd_micros: u64,
    /// Billable records whose cost is unknown. Any > 0 means the sum is a lower bound.
    pub cost_unknown_records: usize,
    pub billable_records: usize,
    pub failed_spans: usize,
}

impl Totals {
    pub fn cost_usd_micros(&self) -> Option<u64> {
        (self.billable_records > self.cost_unknown_records).then(|| self.cost_actual_usd_micros + self.cost_estimate_usd_micros)
    }
}

impl TraceState {
    /// Earliest point on the shared time axis.
    pub fn axis_start(&self) -> Option<Ns> {
        let first_span = self.spans.values().map(|s| s.started_at_utc_ns).min();
        match (self.session.started_at_utc_ns, first_span) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Aggregated from each span's own usage. Ancestors never contribute, so nothing is counted twice.
    pub fn totals(&self) -> Totals {
        let mut t = Totals::default();
        for s in self.spans.values() {
            if s.status == SpanStatus::Failed {
                t.failed_spans += 1;
            }
            let Some(u) = &s.usage else { continue };
            t.billable_records += 1;
            t.tokens_input += u.tokens_input.unwrap_or(0);
            t.tokens_output += u.tokens_output.unwrap_or(0);
            t.cache_read += u.cache_read.unwrap_or(0);
            t.cache_write += u.cache_write.unwrap_or(0);
            match (u.cost_usd_micros, u.cost_provenance) {
                (Some(c), CostProvenance::Actual) => t.cost_actual_usd_micros += c,
                (Some(c), CostProvenance::Estimate) => t.cost_estimate_usd_micros += c,
                _ => t.cost_unknown_records += 1,
            }
        }
        t
    }

    /// Greatest completed span among `kinds` (LLM and tool by default).
    pub fn slowest(&self, kinds: &[SpanKind]) -> Option<&Span> {
        self.spans
            .values()
            .filter(|s| kinds.contains(&s.kind))
            .filter_map(|s| Some((s.ended_at_utc_ns? - s.started_at_utc_ns, s)))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.span_id.cmp(&a.1.span_id)))
            .map(|(_, s)| s)
    }

    pub fn wall_clock(&self, now: Ns) -> Ns {
        let start = self.axis_start().unwrap_or(now);
        let end = self.session.ended_at_utc_ns.unwrap_or(now);
        (end - start).max(0)
    }
}

pub const SLOWEST_DEFAULT_KINDS: &[SpanKind] = &[SpanKind::Llm, SpanKind::Tool];
