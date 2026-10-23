//! Ratatui waterfall widget and theme. Layout math and formatting come from `trace-view`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::StatefulWidget;
use std::collections::BTreeMap;
use trace_core::{Ns, SpanId, SpanKind, SpanStatus, TraceState};
use trace_view::{Row, RowKind, Viewport, bar_span, compact_duration, detail, format_cost, format_duration, ticks, title};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyphs {
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    Color,
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub glyphs: Glyphs,
    pub palette: Palette,
}

impl Default for Theme {
    fn default() -> Self {
        Self { glyphs: Glyphs::Unicode, palette: Palette::Color }
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
        match self.palette {
            Palette::Color => Style::new().fg(c),
            Palette::Mono => Style::new(),
        }
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
    /// Clamped to the area. The full area width draws the tree alone, with no timeline.
    pub tree_width: u16,
    /// Unfocused selection is bold instead of reversed.
    pub focused: bool,
    /// The user's span notes; a noted row carries a mark after its title.
    pub notes: Option<&'a BTreeMap<SpanId, String>>,
    /// Idle stretches the viewport folds out, marked where they fall in view.
    pub idle: &'a [(Ns, Ns)],
}

impl StatefulWidget for WaterfallWidget<'_> {
    type State = WaterfallState;

    fn render(self, area: Rect, buf: &mut Buffer, st: &mut WaterfallState) {
        if area.is_empty() {
            return;
        }
        let th = self.theme;
        let tw = self.tree_width.min(area.width);
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

        // Folds first: a dotted line down the timeline and the wait on the tick line, `┊17h`. Folds closer
        // than a label merge, their waits added; tick labels take only what the folds leave free.
        let mut folds: Vec<(u16, Ns)> = Vec::new();
        for &(a, b) in self.idle {
            let col = self.viewport.col_of(a);
            if !(0.0..f64::from(tl_w)).contains(&col) {
                continue;
            }
            let x = tl_x + col as u16;
            match folds.last_mut() {
                Some((last, total)) if x < *last + 6 => *total += b - a,
                _ => folds.push((x, b - a)),
            }
        }
        let dim = Style::new().add_modifier(Modifier::DIM);
        let mut taken: Vec<(u16, u16)> = Vec::new();
        for &(x, idle) in &folds {
            let label = format!("{}{}", th.pick('┊', ':'), compact_duration(idle));
            let end = buf.set_stringn(x, area.y, &label, usize::from(area.right() - x), th.fg(NOW).add_modifier(Modifier::DIM)).0;
            taken.push((x, end));
        }
        let mut label_end = tl_x;
        for (x, label) in ticks(&self.viewport, tl_w, self.origin_ns) {
            let x = tl_x + x;
            let end = x + label.chars().count() as u16;
            if x >= label_end && taken.iter().all(|&(a, b)| end < a || x > b) {
                label_end = buf.set_stringn(x, area.y, &label, usize::from(area.right() - x), dim).0 + 1;
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
                title(span, &self.state.session),
                indent = usize::from(row.depth) * 2
            );
            let glyph_style = th.fg(if failed { FAILED } else { kind_color(span.kind) });
            let mut x = buf.set_stringn(area.x, y, &left, usize::from(name_w), Style::new()).0;
            let room = |x: u16| usize::from((area.x + name_w).saturating_sub(x));
            if self.notes.is_some_and(|n| n.contains_key(id)) {
                x = buf.set_stringn(x, y, format!(" {}", th.pick('✎', '*')), room(x), dim).0;
            }
            if let Some(d) = detail(span, self.state) {
                buf.set_stringn(x, y, format!(" {d}"), room(x), dim);
            }
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
            for &(x, _) in &folds {
                if buf[(x, y)].symbol() == " " {
                    buf[(x, y)].set_char(th.pick('┊', ':')).set_style(dim);
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
