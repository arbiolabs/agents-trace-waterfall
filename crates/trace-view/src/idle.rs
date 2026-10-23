//! Idle stretches: where nothing ran in a long session, folded out of the timeline.

use trace_core::{Ns, TraceState};

/// Idle stretches at least this long fold out of the timeline and replay, and squeeze in the overview.
pub const IDLE_GAP_MIN: Ns = 10 * 60 * 1_000_000_000;

/// The most idle stretches folded; the longest win. A two-week session has a few dozen.
pub const MAX_GAPS: usize = 64;

/// Stretches of `(a, b)` where no leaf span ran, in time order. Leaves are the activity: the agent and turn
/// spans around them can stay open across idle time.
pub fn idle_gaps(state: &TraceState, (a, b): (Ns, Ns), session_end: Option<Ns>, now: Ns) -> Vec<(Ns, Ns)> {
    let mut busy: Vec<(Ns, Ns)> =
        state.spans.values().filter(|s| state.children(&s.span_id).is_empty()).map(|s| (s.started_at_utc_ns, s.effective_end(session_end, now))).collect();
    busy.sort_unstable();
    let min = IDLE_GAP_MIN;
    let mut gaps = Vec::new();
    let mut reach = a;
    for (start, end) in busy.into_iter().chain([(b, b)]) {
        if start.min(b) - reach >= min {
            gaps.push((reach, start.min(b)));
        }
        reach = reach.max(end);
    }
    gaps.sort_unstable_by_key(|&(x, y)| std::cmp::Reverse(y - x));
    gaps.truncate(MAX_GAPS);
    gaps.sort_unstable();
    gaps
}


/// Idle stretches taken out of the timeline: folded time is real time less every stretch before it, so
/// the waterfall spends no width on waiting. Copy, so a `Viewport` can carry it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Folds {
    gaps: [(Ns, Ns); MAX_GAPS],
    n: usize,
}

impl Default for Folds {
    fn default() -> Self {
        Self { gaps: [(0, 0); MAX_GAPS], n: 0 }
    }
}

impl Folds {
    /// `gaps` in time order, as [`idle_gaps`] returns them; past [`MAX_GAPS`] the rest stay unfolded.
    pub fn new(gaps: &[(Ns, Ns)]) -> Self {
        let n = gaps.len().min(MAX_GAPS);
        let mut arr = [(0, 0); MAX_GAPS];
        arr[..n].copy_from_slice(&gaps[..n]);
        Self { gaps: arr, n }
    }

    pub fn gaps(&self) -> &[(Ns, Ns)] {
        &self.gaps[..self.n]
    }

    /// `t` with the idle time before it removed; a time inside a stretch folds onto its start.
    pub fn fold(&self, t: Ns) -> Ns {
        let mut cut: Ns = 0;
        for &(a, b) in self.gaps() {
            if t <= a {
                break;
            }
            cut = cut.saturating_add(t.min(b) - a);
        }
        t.saturating_sub(cut)
    }

    /// The earliest real time that folds to `f`: a stretch's fold point unfolds to where the waiting began.
    pub fn unfold(&self, f: Ns) -> Ns {
        let mut t = f;
        for &(a, b) in self.gaps() {
            if t <= a {
                break;
            }
            t = t.saturating_add(b - a);
        }
        t
    }

    /// The stretch that `t` falls strictly inside.
    pub fn inside(&self, t: Ns) -> Option<(Ns, Ns)> {
        self.gaps().iter().copied().find(|&(a, b)| a < t && t < b)
    }
}

/// The idle stretches a session's timeline folds: between activity, never after the last of it, so a live
/// session still shows time passing since its last event.
pub fn folds_for(state: &TraceState, now: Ns) -> Folds {
    let (Some(start), Some(last)) = (state.axis_start(), state.last_occurred_at_utc_ns) else { return Folds::default() };
    Folds::new(&idle_gaps(state, (start, last), state.session.ended_at_utc_ns, now))
}

/// `47h`, `3d`, `35m`: an idle stretch's length in the few characters a fold mark has.
pub fn compact_duration(ns: Ns) -> String {
    let m = ns / 60_000_000_000;
    match m {
        m if m >= 48 * 60 => format!("{}d", m / (24 * 60)),
        m if m >= 60 => format!("{}h", m / 60),
        0 => crate::format_duration(ns),
        m => format!("{m}m"),
    }
}
