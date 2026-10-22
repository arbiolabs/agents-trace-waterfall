use crate::app::{App, Focus, Inspector, Mode, Screen, Tab, View};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span as TSpan};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row as TRow, StatefulWidget, Table, Wrap,
};
use trace_core::{CostProvenance, Ns, SLOWEST_DEFAULT_KINDS, SessionStatus, wire_name};
use trace_render::{Glyphs, Theme, WaterfallWidget, format_cost, format_duration, format_tokens};

const WIDE: u16 = 120;

impl App {
    pub fn render(&mut self, f: &mut Frame) {
        render(self, f);
    }
}
const COMPACT: u16 = 80;

pub fn render(app: &mut App, f: &mut Frame) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    match app.screen {
        Screen::Sessions => sessions(app, f, main),
        Screen::Waterfall => waterfall(app, f, main),
        Screen::Help => f.render_widget(
            Paragraph::new(HELP.lines().map(Line::from).collect::<Vec<_>>())
                .wrap(Wrap { trim: false })
                .scroll((app.help_scroll, 0))
                .block(Block::bordered().title(" Help ")),
            main,
        ),
    }
    let text = match (&app.prompt, &app.message) {
        (Some(p), _) => format!("/{p}▏  Enter keep · Esc clear"),
        (None, Some(m)) => m.clone(),
        (None, None) => hints(app),
    };
    f.render_widget(
        Paragraph::new(text).style(Style::new().add_modifier(Modifier::REVERSED)),
        footer,
    );
}

fn hints(app: &App) -> String {
    match (app.screen, &app.view) {
        (Screen::Help, _) => "j/k scroll · Esc/? back · q quit".into(),
        (Screen::Sessions, _) => {
            let status = app.filter.status.map_or("all".into(), |s| wire_name(&s));
            format!("[{status}] j/k move · Enter open · / filter · F status · ? help · q quit")
        }
        (Screen::Waterfall, None) => String::new(),
        (Screen::Waterfall, Some(v)) => {
            let keys = match (v.focus, &v.mode) {
                (Focus::Inspector, _) => {
                    "h/l tab · j/k scroll · y copy · x export · Tab tree · Esc close"
                }
                (Focus::Timeline, _) => "h/l pan · j/k move · +/- zoom · Tab inspector · ? help",
                (_, Mode::Live { .. }) => {
                    "j/k h/l · Enter inspect · Space pause · f follow · +/- H/L · / search · e all · ? help"
                }
                (_, Mode::Replay { .. }) => {
                    "Space play · [ ] step · < > speed · H/L seek · Enter inspect · r exit replay · ? help"
                }
                (_, Mode::Static) => {
                    "j/k h/l · Enter inspect · +/- H/L · / search · e all · r replay · s sessions · ? help"
                }
            };
            format!("{} │ {keys}", v.status())
        }
    }
}

fn session_glyph(s: SessionStatus, th: &Theme) -> &'static str {
    let ascii = th.glyphs == Glyphs::Ascii;
    match s {
        SessionStatus::Live => {
            if ascii {
                ">"
            } else {
                "▶"
            }
        }
        SessionStatus::Completed => {
            if ascii {
                "o"
            } else {
                "✓"
            }
        }
        SessionStatus::Failed => {
            if ascii {
                "x"
            } else {
                "✗"
            }
        }
        SessionStatus::Interrupted => {
            if ascii {
                "!"
            } else {
                "⊘"
            }
        }
        SessionStatus::Unknown => "?",
    }
}

struct Col {
    header: &'static str,
    width: u16,
    wide_only: bool,
}

const fn col(header: &'static str, width: u16, wide_only: bool) -> Col {
    Col {
        header,
        width,
        wide_only,
    }
}

const SESSION_COLS: [Col; 10] = [
    col("", 1, false),
    col("id", 10, false),
    col("project", 14, false),
    col("source", 14, true),
    col("started", 19, true),
    col("dur", 7, false),
    col("model", 14, true),
    col("tok in/out", 11, false),
    col("cost", 8, false),
    col("err", 3, false),
];

fn sessions(app: &mut App, f: &mut Frame, area: Rect) {
    let wide = area.width >= 100;
    let cols = || SESSION_COLS.iter().filter(move |c| wide || !c.wide_only);
    let rows = app.sessions.iter().map(|s| {
        let end = s.ended_at_utc_ns.or(s.last_event_at_utc_ns);
        let dur = match (s.started_at_utc_ns, end) {
            (Some(a), Some(b)) => format_duration(b.saturating_sub(a).max(0)),
            _ => "—".into(),
        };
        let cells = [
            session_glyph(s.status, &app.theme).to_owned(),
            s.session_id.0.chars().take(10).collect(),
            s.project_id.clone(),
            s.source_adapter.clone(),
            s.started_at_utc_ns.map_or("—".into(), utc),
            dur,
            s.model.clone().unwrap_or_else(|| "—".into()),
            format!(
                "{}/{}",
                format_tokens(s.totals.tokens_input),
                format_tokens(s.totals.tokens_output)
            ),
            format_cost(s.totals.cost_usd_micros()),
            s.totals.failed_spans.to_string(),
        ];
        TRow::new(
            SESSION_COLS
                .iter()
                .zip(cells)
                .filter(|(c, _)| wide || !c.wide_only)
                .map(|(_, cell)| cell),
        )
    });
    let widths = cols().map(|c| Constraint::Length(c.width));
    let title = format!(
        " Sessions ({}) status:{} filter:{} ",
        app.sessions.len(),
        app.filter.status.map_or("all".into(), |s| wire_name(&s)),
        app.filter.text.as_deref().unwrap_or("—")
    );
    let table = Table::new(rows, widths)
        .header(
            TRow::new(cols().map(|c| Cell::from(c.header)))
                .style(Style::new().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .block(Block::bordered().title(title));
    if app.sessions.is_empty() {
        let msg = Paragraph::new(
            "No sessions. Import one with `trace-tui import <file>` or start the collector.",
        )
        .block(Block::bordered().title(" Sessions "));
        f.render_widget(msg, area);
    } else {
        f.render_stateful_widget(table, area, &mut app.table);
    }
}

fn waterfall(app: &mut App, f: &mut Frame, area: Rect) {
    let theme = app.theme;
    let Some(v) = &mut app.view else { return };
    v.tick_clock();
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    f.render_widget(Paragraph::new(summary(v)), head);

    let w = body.width;
    if w < COMPACT {
        match v.focus {
            Focus::Inspector => inspector(v, f, body),
            Focus::Timeline => draw_waterfall(v, &theme, f.buffer_mut(), body, 14.min(w / 2)),
            Focus::Tree => draw_waterfall(v, &theme, f.buffer_mut(), body, w),
        }
        return;
    }
    let open = v.inspector.is_some();
    if w < WIDE {
        draw_waterfall(v, &theme, f.buffer_mut(), body, 28);
        if open {
            let pop = Rect {
                x: body.x + 2,
                y: body.y + body.height / 4,
                width: w - 4,
                height: body.height - body.height / 4,
            };
            f.render_widget(Clear, pop);
            inspector(v, f, pop);
        }
        return;
    }
    let (wf_area, ins_area) = match (open, body.height >= 30) {
        (false, _) => (body, None),
        (true, true) => {
            let [a, b] = Layout::vertical([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(body);
            (a, Some(b))
        }
        (true, false) => {
            let [a, b] =
                Layout::horizontal([Constraint::Min(60), Constraint::Length(50)]).areas(body);
            (a, Some(b))
        }
    };
    draw_waterfall(v, &theme, f.buffer_mut(), wf_area, 36.min(wf_area.width / 2));
    if let Some(a) = ins_area {
        inspector(v, f, a);
    }
}

/// `tree_width` equal to `area.width` draws the tree alone and leaves the timeline state untouched.
fn draw_waterfall(v: &mut View, theme: &Theme, buf: &mut Buffer, area: Rect, tree_width: u16) {
    let tl_w = area.width.saturating_sub(tree_width.saturating_add(1));
    if tl_w > 0 {
        v.timeline_width = tl_w;
        let mut vp = v.viewport.unwrap_or_else(|| v.fit(tl_w));
        if let Mode::Live {
            following: true, ..
        } = v.mode
        {
            vp.start_ns = v
                .now
                .saturating_sub((f64::from(tl_w) * vp.ns_per_cell * 0.95) as Ns);
        }
        v.viewport = Some(vp);
    }
    let vp = v.viewport.unwrap_or_else(|| v.fit(1));
    let origin = v.state.axis_start().unwrap_or(v.now);
    let widget = WaterfallWidget {
        state: &v.state,
        rows: &v.rows,
        now: v.now,
        viewport: vp,
        origin_ns: origin,
        theme,
        tree_width,
        focused: v.focus != Focus::Inspector,
    };
    widget.render(area, buf, &mut v.wf);
}

fn summary(v: &View) -> Line<'static> {
    let st = &v.state;
    let t = st.totals();
    let mut prov = Vec::new();
    if t.cost_actual_usd_micros > 0 {
        prov.push("actual".to_owned());
    }
    if t.cost_estimate_usd_micros > 0 {
        prov.push("est".to_owned());
    }
    if t.cost_unknown_records > 0 {
        prov.push(format!("{} unknown", t.cost_unknown_records));
    }
    let slowest = st.slowest(SLOWEST_DEFAULT_KINDS).map_or("—".into(), |s| {
        format!(
            "{} {}",
            s.name,
            format_duration(
                s.ended_at_utc_ns
                    .unwrap_or(s.started_at_utc_ns)
                    .saturating_sub(s.started_at_utc_ns),
            )
        )
    });
    let search = if v.search.is_empty() {
        String::new()
    } else {
        format!(" │ /{}", v.search)
    };
    Line::from(vec![
        TSpan::styled(
            format!("{} ", wire_name(&st.session.status)),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        TSpan::raw(format!(
            "wall {} │ tok {}/{} │ cost {} ({}) │ slowest {slowest} │ failed {} pending {} unresolved {}{search}",
            format_duration(st.wall_clock(v.now)),
            format_tokens(t.tokens_input),
            format_tokens(t.tokens_output),
            format_cost(t.cost_usd_micros()),
            if prov.is_empty() {
                "none".into()
            } else {
                prov.join("+")
            },
            t.failed_spans,
            st.pending_count(),
            st.unresolved().len(),
        )),
    ])
}

fn inspector(v: &View, f: &mut Frame, area: Rect) {
    let ins = v.inspector.unwrap_or(Inspector {
        tab: Tab::Overview,
        scroll: 0,
    });
    let mut title = vec![TSpan::raw(" ")];
    for t in Tab::ALL {
        let style = if t == ins.tab {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        title.push(TSpan::styled(format!("{t:?}"), style));
        title.push(TSpan::raw(" "));
    }
    let border = if v.focus == Focus::Inspector {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Line::from(title));
    let lines = inspector_lines(v, ins.tab);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((ins.scroll, 0))
            .block(block),
        area,
    );
}

const NOT_CAPTURED: &str = "not captured";

pub fn inspector_lines(v: &View, tab: Tab) -> Vec<Line<'static>> {
    let Some(s) = v.selected_span() else {
        return vec![Line::from("No span selected.")];
    };
    let st = &v.state;
    let origin = st.axis_start().unwrap_or(s.started_at_utc_ns);
    let at = |t: Ns| format!("{} (+{})", utc(t), format_duration(t.saturating_sub(origin)));
    let opt = |o: &Option<String>| o.clone().unwrap_or_else(|| "—".into());
    let mut out: Vec<String> = Vec::new();
    match tab {
        Tab::Overview => {
            let el = s.elapsed(st.session.ended_at_utc_ns, v.now);
            let skew = if el.skewed {
                "  ⚠ end precedes start (clock skew), clamped to 0"
            } else {
                ""
            };
            let parent =
                s.parent_span_id
                    .as_ref()
                    .map_or("—".into(), |p| match st.spans.get(p) {
                        Some(ps) => format!("{} ({p})", ps.name),
                        None => format!("{p} (not arrived)"),
                    });
            out.extend([
                format!("{}  [{} · {}]", s.name, wire_name(&s.kind), wire_name(&s.status)),
                format!("span      {}", s.span_id),
                format!("parent    {parent}"),
                format!(
                    "retry of  {}",
                    s.retry_of.as_ref().map_or("—".into(), ToString::to_string)
                ),
                format!("children  {}", st.children(&s.span_id).len()),
                format!("started   {}", at(s.started_at_utc_ns)),
                format!(
                    "ended     {}",
                    s.ended_at_utc_ns.map_or("running".into(), at)
                ),
                format!("duration  {}{skew}", format_duration(el.ns)),
                format!("model     {}  provider {}", opt(&s.model), opt(&s.provider)),
                format!(
                    "tool      {}  agent {}",
                    opt(&s.tool_name),
                    opt(&s.agent_name)
                ),
                format!("source    {}", s.source_id),
            ]);
            match &s.usage {
                None => out.push("usage     —".into()),
                Some(u) => {
                    let n = |x: Option<u64>| x.map_or("—".into(), format_tokens);
                    out.push(format!(
                        "usage     in {} out {} cache r {} w {}{}",
                        n(u.tokens_input),
                        n(u.tokens_output),
                        n(u.cache_read),
                        n(u.cache_write),
                        if s.usage_final { "" } else { " (provisional)" }
                    ));
                    let prov = match u.cost_provenance {
                        CostProvenance::Actual => "actual charge",
                        CostProvenance::Estimate => "model-price estimate",
                        CostProvenance::Unknown => "unknown",
                    };
                    out.push(format!(
                        "cost      {} ({prov})",
                        format_cost(u.cost_usd_micros)
                    ));
                }
            }
            if !s.attributes.is_empty() {
                out.push("attributes".into());
                out.extend(s.attributes.iter().map(|(k, val)| format!("  {k} = {val}")));
            }
        }
        Tab::Input => out.push(
            s.input_preview
                .clone()
                .unwrap_or_else(|| NOT_CAPTURED.into()),
        ),
        Tab::Output => out.push(
            s.output_preview
                .clone()
                .unwrap_or_else(|| NOT_CAPTURED.into()),
        ),
        Tab::Error => out.push(match (&s.error, s.status) {
            (Some(e), _) => e.clone(),
            (None, trace_core::SpanStatus::Failed) => NOT_CAPTURED.into(),
            (None, _) => "no error".into(),
        }),
        Tab::Events if s.events.is_empty() => out.push("no events".into()),
        Tab::Events => {
            for e in &s.events {
                let attrs = if e.attributes.is_empty() {
                    String::new()
                } else {
                    format!(" {}", serde_json::Value::Object(e.attributes.clone()))
                };
                let payload = e
                    .payload_ref
                    .as_ref()
                    .map_or(String::new(), |p| format!(" payload={p}"));
                out.push(format!(
                    "+{} {}{attrs}{payload}",
                    format_duration(e.timestamp_utc_ns.saturating_sub(s.started_at_utc_ns)),
                    e.name
                ));
            }
        }
        Tab::Files if s.artifacts.is_empty() => out.push("no files recorded".into()),
        Tab::Files => {
            for a in &s.artifacts {
                out.push(format!(
                    "{}  {}  {}",
                    a.path,
                    opt(&a.hash),
                    opt(&a.change_summary)
                ));
            }
        }
    }
    out.into_iter().map(Line::from).collect()
}

/// `YYYY-MM-DD HH:MM:SS` UTC.
pub fn utc(ns: Ns) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

const HELP: &str = "\
Sessions
  j/k ↑/↓      move            Enter   open waterfall
  /            filter text     F       cycle status filter
  g/G          first/last      q       quit

Waterfall
  j/k ↑/↓      previous/next visible span
  h/l ←/→      collapse / parent, expand / first child (Timeline focus: pan)
  g/G          first/last span
  e            toggle expansion of all descendants (up to 5000 spans)
  Enter        open inspector        Tab   cycle focus tree / timeline / inspector
  Esc          clear search, close inspector, back to sessions
  +/-          zoom in/out (anchored at selection, or now when following)
  H/L          pan back/forward (replay: seek)
  Space        pause/resume live view (events queue, none dropped); play/pause replay
  f            follow now (live)
  [ ]          previous/next event (replay)
  < >          slower/faster replay (0.25x .. 10x)
  r            start replay at session start / leave replay
  /            search: words match name, kind, status, model, tool;
               scope with kind:tool status:failed model:x tool:y name:z
  s            sessions screen       ?     help      q  quit

Inspector (focused)
  h/l ←/→      previous/next tab: Overview Input Output Events Error Files
  j/k          scroll
  y            copy span JSON (OSC52)
  x            export span JSON to ./span-<id>.json  (spec says `e`; `e` is expand-all)
";

#[cfg(test)]
mod tests {
    #[test]
    fn utc_formats_known_instants() {
        assert_eq!(super::utc(0), "1970-01-01 00:00:00");
        assert_eq!(super::utc(1_767_225_600_000_000_000), "2026-01-01 00:00:00");
        assert_eq!(super::utc(951_782_400_000_000_000), "2000-02-29 00:00:00");
    }
}
