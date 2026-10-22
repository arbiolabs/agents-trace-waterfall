//! Waterfall widget, time axis, tree flattening and clipping. Pure: the caller owns app state.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::StatefulWidget;
use std::collections::HashSet;
use trace_core::{Ns, Span, SpanId, SpanKind, SpanStatus, TraceState};

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
        visit(state, id, 0, false, collapsed, keep, &mut out);
    }
    let unresolved = state.unresolved();
    if !unresolved.is_empty() {
        let header = out.len();
        out.push(Row { kind: RowKind::UnresolvedHeader, depth: 0, has_children: true, collapsed: false, retry: false });
        let mut any = false;
        for id in unresolved {
            any |= visit(state, id, 1, false, collapsed, keep, &mut out);
        }
        if !any {
            out.truncate(header);
        }
    }
    out
}

/// Returns whether this subtree contains a kept span. `hidden` means an ancestor is collapsed.
fn visit(
    state: &TraceState,
    id: &SpanId,
    depth: u16,
    hidden: bool,
    collapsed: &HashSet<SpanId>,
    keep: Option<&dyn Fn(&Span) -> bool>,
    out: &mut Vec<Row>,
) -> bool {
    let span = &state.spans[id];
    let kids = state.children(id);
    let is_collapsed = !kids.is_empty() && collapsed.contains(id);
    let mark = out.len();
    if !hidden {
        out.push(Row { kind: RowKind::Span(id.clone()), depth, has_children: !kids.is_empty(), collapsed: is_collapsed, retry: span.retry_of.is_some() });
    }
    let Some(keep) = keep else {
        if !hidden && !is_collapsed {
            for k in kids {
                visit(state, k, depth + 1, false, collapsed, None, out);
            }
        }
        return true;
    };
    let mut any = keep(span);
    for k in kids {
        any |= visit(state, k, depth + 1, hidden || is_collapsed, collapsed, Some(keep), out);
    }
    if !any {
        out.truncate(mark);
    }
    any
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub start_ns: Ns,
    pub ns_per_cell: f64,
}

impl Viewport {
    const MIN_NS_PER_CELL: f64 = 1e-3;

    pub fn fit(start: Ns, end: Ns, width: u16) -> Self {
        Self { start_ns: start, ns_per_cell: (end - start).max(1) as f64 / f64::from(width.max(1)) }
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
        self.start_ns + (f64::from(width) * self.ns_per_cell).round() as Ns
    }

    pub fn col_of(&self, t: Ns) -> f64 {
        (t - self.start_ns) as f64 / self.ns_per_cell
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
    let mut k = (vp.start_ns - origin_ns).div_euclid(step);
    let mut out = Vec::new();
    loop {
        let t = origin_ns + k * step;
        let col = vp.col_of(t).floor();
        if col >= f64::from(width) {
            return out;
        }
        if col >= 0.0 {
            out.push((col as u16, format_duration(t - origin_ns)));
        }
        k += 1;
    }
}

fn one_decimal(v: f64) -> String {
    let s = if v >= 100.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    s.strip_suffix(".0").map_or(s.clone(), str::to_owned)
}

/// `0s`, `850ns`, `12.5µs`, `250ms`, `1.5s`, `2m`, `2m30s`, `1h5m`.
pub fn format_duration(ns: Ns) -> String {
    if ns < 0 {
        return format!("-{}", format_duration(-ns));
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyphs {
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub glyphs: Glyphs,
    /// false = monochrome; statuses stay distinguishable by glyph.
    pub color: bool,
}

impl Default for Theme {
    fn default() -> Self {
        Self { glyphs: Glyphs::Unicode, color: true }
    }
}

impl Theme {
    fn pick(&self, unicode: char, ascii: char) -> char {
        match self.glyphs {
            Glyphs::Unicode => unicode,
            Glyphs::Ascii => ascii,
        }
    }

    fn fg(&self, c: Color) -> Style {
        if self.color { Style::new().fg(c) } else { Style::new() }
    }
}

pub fn kind_glyph(kind: SpanKind, theme: &Theme) -> char {
    match kind {
        SpanKind::Agent => theme.pick('◆', 'A'),
        SpanKind::Turn => theme.pick('◇', 'U'),
        SpanKind::Llm => theme.pick('●', 'L'),
        SpanKind::Tool => theme.pick('■', 'T'),
        SpanKind::Subagent => theme.pick('◈', 'S'),
        SpanKind::Wait => theme.pick('◌', 'W'),
        SpanKind::Custom => theme.pick('○', 'C'),
    }
}

pub fn status_glyph(status: SpanStatus, theme: &Theme) -> char {
    match status {
        SpanStatus::Running => theme.pick('▶', '>'),
        SpanStatus::Succeeded => theme.pick('✓', 'o'),
        SpanStatus::Failed => theme.pick('✗', 'x'),
        SpanStatus::Cancelled => theme.pick('⊘', '/'),
        SpanStatus::Unknown => '?',
    }
}

fn kind_color(kind: SpanKind) -> Color {
    match kind {
        SpanKind::Agent | SpanKind::Turn => Color::Rgb(112, 128, 144),
        SpanKind::Llm => Color::Rgb(167, 139, 250),
        SpanKind::Tool => Color::Rgb(45, 212, 191),
        SpanKind::Wait => Color::Rgb(251, 191, 36),
        SpanKind::Subagent => Color::Rgb(96, 165, 250),
        SpanKind::Custom => Color::Gray,
    }
}

const FAILED: Color = Color::Rgb(248, 113, 113);
const NOW: Color = Color::Yellow;

/// Selection state. `render` clamps `selected` and scrolls `offset` to keep it visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaterfallState {
    pub offset: usize,
    pub selected: usize,
}

/// One header line of tick labels, then one line per row: tree column, `│`, shared timeline.
pub struct WaterfallWidget<'a> {
    pub state: &'a TraceState,
    pub rows: &'a [Row],
    /// View clock: running bars extend here and the now cursor sits here.
    pub now: Ns,
    pub viewport: Viewport,
    /// Tick labels are relative to this, normally `TraceState::axis_start`.
    pub origin_ns: Ns,
    pub theme: &'a Theme,
    /// Shrinks to half the area on narrow terminals.
    pub tree_width: u16,
    /// Unfocused selection is bold instead of reversed.
    pub focused: bool,
}

impl StatefulWidget for WaterfallWidget<'_> {
    type State = WaterfallState;

    fn render(self, area: Rect, buf: &mut Buffer, st: &mut WaterfallState) {
        if area.is_empty() {
            return;
        }
        let th = self.theme;
        let tw = self.tree_width.min(area.width / 2);
        let sep_x = area.x + tw;
        let tl_x = sep_x + 1;
        let tl_w = area.width.saturating_sub(tw + 1);
        let body_h = usize::from(area.height - 1);

        st.selected = st.selected.min(self.rows.len().saturating_sub(1));
        if body_h > 0 {
            st.offset = st.offset.min(st.selected).max((st.selected + 1).saturating_sub(body_h));
        }

        let col = self.viewport.col_of(self.now);
        let now_x = (col >= 0.0 && col < f64::from(tl_w)).then(|| tl_x + col as u16);
        let now_style = th.fg(NOW);
        let sep = th.pick('│', '|');

        let mut label_end = tl_x;
        for (x, label) in ticks(&self.viewport, tl_w, self.origin_ns) {
            let x = tl_x + x;
            if x >= label_end {
                label_end = buf.set_stringn(x, area.y, &label, usize::from(area.right() - x), Style::new().add_modifier(Modifier::DIM)).0 + 1;
            }
        }

        for dy in 0..area.height {
            let y = area.y + dy;
            if sep_x < area.right() {
                buf[(sep_x, y)].set_char(sep);
            }
            if let Some(x) = now_x
                && buf[(x, y)].symbol() == " "
            {
                buf[(x, y)].set_char(th.pick('┊', ':')).set_style(now_style);
            }
        }

        for (i, row) in self.rows.iter().skip(st.offset).take(body_h).enumerate() {
            let y = area.y + 1 + i as u16;
            let RowKind::Span(id) = &row.kind else {
                buf.set_stringn(area.x, y, "  Unresolved", usize::from(tw), Style::new().add_modifier(Modifier::ITALIC));
                self.highlight(buf, area, y, st.offset + i == st.selected);
                continue;
            };
            let span = &self.state.spans[id];
            let session_end = self.state.session.ended_at_utc_ns;
            let failed = span.status == SpanStatus::Failed;

            let mut meta = format_duration(span.elapsed(session_end, self.now).ns);
            if let Some(u) = &span.usage {
                meta = format!("{} {meta}", format_cost(u.cost_usd_micros));
            }
            let meta_w = meta.chars().count() as u16;
            let name_w = if meta_w + 8 <= tw {
                buf.set_string(area.x + tw - meta_w, y, &meta, Style::new().add_modifier(Modifier::DIM));
                tw - meta_w - 1
            } else {
                tw
            };
            let fold = match (row.has_children, row.collapsed) {
                (false, _) => ' ',
                (true, false) => th.pick('▾', '-'),
                (true, true) => th.pick('▸', '+'),
            };
            let retry = if row.retry { th.pick('↻', 'r') } else { ' ' };
            let left = format!(
                "{:indent$}{fold}{}{}{retry}{}",
                "",
                kind_glyph(span.kind, th),
                status_glyph(span.status, th),
                span.name,
                indent = usize::from(row.depth) * 2
            );
            let glyph_style = th.fg(if failed { FAILED } else { kind_color(span.kind) });
            buf.set_stringn(area.x, y, &left, usize::from(name_w), Style::new());
            let gx = area.x.saturating_add(row.depth.saturating_mul(2)) + 1;
            for x in gx..(gx + 2).min(area.x + name_w) {
                buf[(x, y)].set_style(glyph_style);
            }

            let bar_end = span.effective_end(session_end, self.now);
            if let Some((sx, ex)) = bar_span(span.started_at_utc_ns, bar_end, &self.viewport, tl_w) {
                let fill = match span.status {
                    SpanStatus::Running => th.pick('▓', '='),
                    SpanStatus::Failed | SpanStatus::Cancelled => th.pick('░', '.'),
                    SpanStatus::Succeeded | SpanStatus::Unknown => th.pick('█', '#'),
                };
                for x in sx..ex {
                    let ch = if failed && x + 1 == ex { 'x' } else { fill };
                    buf[(tl_x + x, y)].set_char(ch).set_style(glyph_style);
                }
            }
            self.highlight(buf, area, y, st.offset + i == st.selected);
        }
    }
}

impl WaterfallWidget<'_> {
    fn highlight(&self, buf: &mut Buffer, area: Rect, y: u16, selected: bool) {
        if selected {
            let m = if self.focused { Modifier::REVERSED } else { Modifier::BOLD };
            buf.set_style(Rect::new(area.x, y, area.width, 1), Style::new().add_modifier(m));
        }
    }
}
