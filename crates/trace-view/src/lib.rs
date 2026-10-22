//! The trace document shared by the TUI and the GUI: tree flattening, the time viewport, value formatting,
//! and `TraceDoc`, which owns live/pause/replay, collapse, selection and search. No UI toolkit types live here;
//! a viewport "cell" is whatever unit the frontend draws in (terminal columns, pixels).

mod doc;

pub use doc::{EXPAND_ALL_LIMIT, Mode, TraceDoc, export_span, matcher, wall_ns};
use std::collections::HashSet;
use trace_core::{Ns, Span, SpanId, TraceState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Span(SpanId),
    /// Group header above spans whose parent has not arrived.
    UnresolvedHeader,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub depth: u16,
    pub has_children: bool,
    pub collapsed: bool,
    pub retry: bool,
}

/// Visible rows in (start, id) order. Collapsed parents hide their descendants.
/// With `keep`, a span shows when it or any descendant matches, so ancestors stay for context.
pub fn flatten(state: &TraceState, collapsed: &HashSet<SpanId>, keep: Option<&dyn Fn(&Span) -> bool>) -> Vec<Row> {
    let mut out = Vec::new();
    for id in state.roots() {
        visit(state, id, 0, collapsed, keep, &mut out);
    }
    let unresolved = state.unresolved();
    if !unresolved.is_empty() {
        let header = out.len();
        out.push(Row { kind: RowKind::UnresolvedHeader, depth: 0, has_children: true, collapsed: false, retry: false });
        let mut any = false;
        for id in unresolved {
            any |= visit(state, id, 1, collapsed, keep, &mut out);
        }
        if !any {
            out.truncate(header);
        }
    }
    out
}

struct Frame<'a> {
    kids: &'a [SpanId],
    next: usize,
    depth: u16,
    hide_kids: bool,
    mark: usize,
    any: bool,
}

/// Iterative pre-order walk. Returns whether the subtree contains a kept span.
fn visit(
    state: &TraceState,
    root: &SpanId,
    depth: u16,
    collapsed: &HashSet<SpanId>,
    keep: Option<&dyn Fn(&Span) -> bool>,
    out: &mut Vec<Row>,
) -> bool {
    let mut stack: Vec<Frame> = Vec::new();
    let mut enter = Some((root, depth, false));
    loop {
        if let Some((id, depth, hidden)) = enter.take() {
            let span = &state.spans[id];
            let kids = state.children(id);
            let is_collapsed = !kids.is_empty() && collapsed.contains(id);
            let mark = out.len();
            if !hidden {
                out.push(Row { kind: RowKind::Span(id.clone()), depth, has_children: !kids.is_empty(), collapsed: is_collapsed, retry: span.retry_of.is_some() });
            }
            let hide_kids = hidden || is_collapsed;
            let kids = if keep.is_none() && hide_kids { &[] } else { kids };
            stack.push(Frame { kids, next: 0, depth, hide_kids, mark, any: keep.is_none_or(|k| k(span)) });
        }
        let top = stack.last_mut().expect("stack holds the root until it returns");
        if let Some(kid) = top.kids.get(top.next) {
            top.next += 1;
            enter = Some((kid, top.depth.saturating_add(1), top.hide_kids));
            continue;
        }
        let done = stack.pop().expect("non-empty");
        if !done.any {
            out.truncate(done.mark);
        }
        match stack.last_mut() {
            Some(parent) => parent.any |= done.any,
            None => return done.any,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub start_ns: Ns,
    pub ns_per_cell: f64,
}

impl Viewport {
    const MIN_NS_PER_CELL: f64 = 1e-3;

    pub fn fit(start: Ns, end: Ns, width: u16) -> Self {
        Self { start_ns: start, ns_per_cell: end.saturating_sub(start).max(1) as f64 / f64::from(width.max(1)) }
    }

    /// `factor` < 1 zooms in. `anchor_ns` stays at the same column.
    pub fn zoom(&mut self, factor: f64, anchor_ns: Ns) {
        let npc = (self.ns_per_cell * factor).max(Self::MIN_NS_PER_CELL);
        let real = npc / self.ns_per_cell;
        self.start_ns = anchor_ns - ((anchor_ns - self.start_ns) as f64 * real).round() as Ns;
        self.ns_per_cell = npc;
    }

    pub fn pan(&mut self, cells: f64) {
        self.start_ns += (cells * self.ns_per_cell).round() as Ns;
    }

    pub fn end_ns(&self, width: u16) -> Ns {
        self.start_ns.saturating_add((f64::from(width) * self.ns_per_cell).round() as Ns)
    }

    pub fn col_of(&self, t: Ns) -> f64 {
        t.saturating_sub(self.start_ns) as f64 / self.ns_per_cell
    }
}

/// Half-open cell range `[start_x, end_x)` of a bar, clipped to `[0, width)`.
/// Any visible span gets at least one cell. `None` when entirely off-screen.
pub fn bar_span(start: Ns, end: Ns, vp: &Viewport, width: u16) -> Option<(u16, u16)> {
    let end = end.max(start);
    let (vs, ve) = (vp.start_ns, vp.end_ns(width));
    let visible = if start == end { (vs..=ve).contains(&start) } else { start < ve && end > vs };
    if width == 0 || !visible {
        return None;
    }
    let w = f64::from(width);
    let sx = vp.col_of(start).floor().clamp(0.0, w - 1.0) as u16;
    let ex = vp.col_of(end).ceil().clamp(f64::from(sx) + 1.0, w) as u16;
    Some((sx, ex))
}

const TICK_STEPS_S: [i64; 12] = [10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 21600];
const MIN_TICK_GAP: f64 = 10.0;

/// Tick columns and labels relative to `origin_ns` (the axis start), spaced at least 10 cells apart.
pub fn ticks(vp: &Viewport, width: u16, origin_ns: Ns) -> Vec<(u16, String)> {
    let nice = (0..10).flat_map(|k| [1, 2, 5].map(|m| m * 10i64.pow(k))).chain(TICK_STEPS_S.map(|s| s * 1_000_000_000));
    let Some(step) = nice.into_iter().find(|&s| s as f64 / vp.ns_per_cell >= MIN_TICK_GAP) else { return Vec::new() };
    let mut k = vp.start_ns.saturating_sub(origin_ns).div_euclid(step);
    let mut times = Vec::new();
    while let Some(t) = k.checked_mul(step).and_then(|o| origin_ns.checked_add(o)) {
        let col = vp.col_of(t).floor();
        if col >= f64::from(width) {
            break;
        }
        if col >= 0.0 {
            times.push((col as u16, t));
        }
        k += 1;
    }
    let label = |t: Ns| format_duration(t.saturating_sub(origin_ns));
    let mut out: Vec<_> = times.iter().map(|&(c, t)| (c, label(t))).collect();
    // Deep into a long session the formatter drops seconds, so fine ticks would all read alike;
    // then label the first tick and show the rest as offsets from it.
    if out.windows(2).any(|w| w[0].1 == w[1].1) {
        let first = times[0].1;
        for (o, &(_, t)) in out.iter_mut().zip(&times).skip(1) {
            o.1 = format!("+{}", format_duration(t - first));
        }
    }
    out
}

fn one_decimal(v: f64) -> String {
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    s.strip_suffix(".0").map_or(s.clone(), str::to_owned)
}

/// `0s`, `850ns`, `12.5µs`, `250ms`, `1.5s`, `2m`, `2m30s`, `1h5m`.
pub fn format_duration(ns: Ns) -> String {
    let sign = if ns < 0 { "-" } else { "" };
    format!("{sign}{}", format_abs(ns.unsigned_abs()))
}

fn format_abs(ns: u64) -> String {
    let f = ns as f64;
    match ns {
        0 => "0s".into(),
        1..1_000 => format!("{ns}ns"),
        1_000..1_000_000 => format!("{}µs", one_decimal(f / 1e3)),
        1_000_000..1_000_000_000 => format!("{}ms", one_decimal(f / 1e6)),
        1_000_000_000..60_000_000_000 => format!("{}s", one_decimal(f / 1e9)),
        _ => {
            let s = ns / 1_000_000_000;
            let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
            match (h, s) {
                (0, 0) => format!("{m}m"),
                (0, _) => format!("{m}m{s}s"),
                _ if m == 0 => format!("{h}h"),
                _ => format!("{h}h{m}m"),
            }
        }
    }
}

/// Unknown cost renders as `—`.
pub fn format_cost(micros: Option<u64>) -> String {
    match micros {
        None => "—".into(),
        Some(m) if m < 10_000 => format!("${:.4}", m as f64 / 1e6),
        Some(m) => format!("${:.2}", m as f64 / 1e6),
    }
}

/// `999`, `12.3k`, `1.5M`.
pub fn format_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", one_decimal(n as f64 / 1e3)),
        _ => format!("{}M", one_decimal(n as f64 / 1e6)),
    }
}
