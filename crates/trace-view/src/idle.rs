//! Idle stretches: where nothing ran in a long session, and how to jump across them.

use trace_core::{Ns, TraceState};

/// The overview's right-edge marker while a live session is idle: the gap from its last activity to now.
/// Idle stretches at least this long, and at least a fiftieth of the session, squeeze in the overview and
/// draw as bands on the timeline.
pub const IDLE_GAP_MIN: Ns = 10 * 60 * 1_000_000_000;

/// The most idle stretches the overview squeezes; the longest win.
pub const MAX_GAPS: usize = 24;

/// Stretches of `(a, b)` where no leaf span ran, in time order. Leaves are the activity: the agent and turn
/// spans around them can stay open across idle time.
pub fn idle_gaps(state: &TraceState, (a, b): (Ns, Ns), session_end: Option<Ns>, now: Ns) -> Vec<(Ns, Ns)> {
    let mut busy: Vec<(Ns, Ns)> =
        state.spans.values().filter(|s| state.children(&s.span_id).is_empty()).map(|s| (s.started_at_utc_ns, s.effective_end(session_end, now))).collect();
    busy.sort_unstable();
    let min = IDLE_GAP_MIN.max((b - a) / 50);
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

/// Where Next or Previous Activity moves a window of `len` that starts at `start`: just before the end of
/// the next idle stretch, or so the previous one's start sits near the right edge.
pub fn skip_idle(gaps: &[(Ns, Ns)], start: Ns, len: Ns, forward: bool) -> Option<Ns> {
    let pad = len / 20;
    if forward {
        gaps.iter().find(|g| g.1 > start + pad).map(|g| g.1 - pad)
    } else {
        gaps.iter().rev().find(|g| g.1 <= start + pad && g.0 + pad - len < start).map(|g| g.0 + pad - len)
    }
}
