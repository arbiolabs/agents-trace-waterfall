use crate::Cli;
use crate::present::{self, Group, LabelAt};
use crate::theme::{KindColors, Palette, Pill};
use chrono::{Local, TimeZone};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Icon, Sizable, Theme, ThemeMode};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trace_core::{Ns, SLOWEST_DEFAULT_KINDS, SPEEDS, SessionId, SessionStatus, Span, SpanId, SpanKind, SpanStatus};
use trace_storage::{SessionFilter, SessionSummary, Store};
use trace_view::{Mode, RowKind, TraceDoc, Viewport, format_cost, format_duration, format_tokens, tick_times};

const SIDEBAR_W: f32 = 260.;
const HEADER_H: f32 = 56.;
const MAIN_PAD: f32 = 12.;
const ROW_H: f32 = 32.;
const BAR_H: f32 = 16.;
const AXIS_H: f32 = 30.;
const INDENT: f32 = 14.;
const TL_PAD_R: f32 = 20.;
const POPOVER_W: f32 = 330.;
const REVEAL: Duration = Duration::from_millis(360);
const POLL: Duration = Duration::from_millis(100);
const WATCH: Duration = Duration::from_secs(2);
const LABEL_SIZE: f32 = 11.5;

fn tnum() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}

/// The cubic-bezier(0.22, 1, 0.36, 1) curve from the design, close enough as an ease-out quint.
fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0., 1.)).powi(5)
}

/// Pixel geometry of the trace card for one frame.
#[derive(Clone, Copy)]
struct Geo {
    name_w: f32,
    tl_w: f32,
    vp: Viewport,
    now: Ns,
    origin: Ns,
}

impl Geo {
    fn x(&self, t: Ns) -> f32 {
        if t < self.vp.start_ns {
            return -((self.vp.start_ns - t) as f64 / self.vp.ns_per_cell) as f32;
        }
        self.vp.col_of(t) as f32
    }
}

pub struct TraceApp {
    store: Store,
    capture: trace_ingest::Capture,
    sessions: Vec<SessionSummary>,
    journal_seq: i64,
    doc: Option<TraceDoc>,
    popover: bool,
    hovered: Option<usize>,
    list: UniformListScrollHandle,
    focus: FocusHandle,
    session_search: Entity<InputState>,
    span_search: Entity<InputState>,
    span_search_open: bool,
    forced_dark: Option<bool>,
    last_frame: Instant,
    last_poll: Instant,
    seen: HashMap<SpanId, Instant>,
    /// The user zoomed, so following keeps their scale instead of fitting the whole trace.
    zoomed: bool,
    reveal_until: Instant,
    error: Option<String>,
    scrubber: Rc<Cell<Bounds<Pixels>>>,
    _subs: Vec<Subscription>,
}

impl TraceApp {
    pub fn new(
        store: Store,
        capture: trace_ingest::Capture,
        forced_dark: Option<bool>,
        cli: &Cli,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions"));
        let span_search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter spans"));
        let subs = vec![
            cx.subscribe(&session_search, |this: &mut Self, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh_sessions(cx);
                    cx.notify();
                }
            }),
            cx.subscribe_in(&span_search, window, |this: &mut Self, state, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    if let Some(doc) = &mut this.doc {
                        doc.search = state.read(cx).value().to_string();
                        doc.refresh();
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.focus.focus(window, cx),
                _ => {}
            }),
            cx.observe_window_appearance(window, |this: &mut Self, window, cx| {
                apply_theme(this.dark(window), cx);
                cx.notify();
            }),
        ];
        let mut this = Self {
            store,
            capture,
            sessions: Vec::new(),
            journal_seq: 0,
            doc: None,
            popover: false,
            hovered: None,
            list: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            session_search,
            span_search,
            span_search_open: false,
            forced_dark,
            last_frame: Instant::now(),
            last_poll: Instant::now(),
            seen: HashMap::new(),
            zoomed: false,
            reveal_until: Instant::now(),
            error: None,
            scrubber: Rc::new(Cell::new(Bounds::default())),
            _subs: subs,
        };
        apply_theme(this.dark(window), cx);
        this.refresh_sessions(cx);
        this.journal_seq = this.store.max_journal_seq().unwrap_or(0);
        let target = match &cli.open {
            Some(prefix) => trace_view::resolve_session(&this.store, prefix).map(Some),
            None => Ok(this.sessions.first().map(|s| s.session_id.clone())),
        };
        match target {
            Ok(Some(id)) => this.open(id, cli.replay),
            Ok(None) => {}
            Err(e) => this.error = Some(format!("{e:#}")),
        }
        if let (Some(needle), Some(doc)) = (&cli.select, &mut this.doc) {
            let needle = needle.to_lowercase();
            let hit = doc.rows.iter().position(|r| matches!(&r.kind, RowKind::Span(id) if doc.state.spans[id].name.to_lowercase().contains(&needle)));
            if let Some(i) = hit {
                doc.select(i);
                this.list.scroll_to_item(i, ScrollStrategy::Center);
            }
        }
        this.popover = cli.popover && this.doc.as_ref().is_some_and(|d| d.selected.is_some());
        this.focus.focus(window, cx);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(WATCH).await;
                if this.update(cx, |this, cx| this.watch(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn dark(&self, window: &Window) -> bool {
        self.forced_dark.unwrap_or_else(|| matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark))
    }

    fn refresh_sessions(&mut self, cx: &App) {
        let text = self.session_search_text(cx);
        let filter = SessionFilter { text: (!text.is_empty()).then_some(text), limit: Some(300), ..Default::default() };
        match self.store.list_sessions(&filter) {
            Ok(s) => self.sessions = s,
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn session_search_text(&self, cx: &App) -> String {
        self.session_search.read(cx).value().trim().to_owned()
    }

    /// New journal rows since the last look refresh the sidebar; the open document polls on its own.
    fn watch(&mut self, cx: &mut Context<Self>) {
        let seq = self.store.max_journal_seq().unwrap_or(self.journal_seq);
        if seq != self.journal_seq {
            self.journal_seq = seq;
            self.refresh_sessions(cx);
            cx.notify();
        }
    }

    fn open(&mut self, id: SessionId, replay: bool) {
        let doc = if replay { TraceDoc::open_replay(&self.store, id, 1.0, false) } else { TraceDoc::open(&self.store, id) };
        match doc {
            Ok(doc) => {
                let now = Instant::now();
                self.seen = doc.state.spans.keys().map(|k| (k.clone(), now)).collect();
                self.reveal_until = now + REVEAL;
                self.doc = Some(doc);
                self.zoomed = false;
                self.popover = false;
                self.hovered = None;
                self.error = None;
                self.last_frame = now;
                self.list.scroll_to_item(0, ScrollStrategy::Top);
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Advances clocks and polls the journal. Returns whether another frame is needed.
    fn tick(&mut self) -> bool {
        let now = Instant::now();
        let dt = now - self.last_frame;
        self.last_frame = now;
        let Some(doc) = &mut self.doc else { return false };
        let res = match &doc.mode {
            Mode::Live { .. } if self.last_poll.elapsed() >= POLL => {
                self.last_poll = now;
                doc.advance(&self.store, dt)
            }
            Mode::Live { .. } => {
                doc.tick_clock();
                Ok(())
            }
            Mode::Replay { .. } => doc.advance(&self.store, dt),
            Mode::Static => Ok(()),
        };
        if let Err(e) = res {
            self.error = Some(format!("{e:#}"));
        }
        if doc.state.spans.len() != self.seen.len() {
            for id in doc.state.spans.keys() {
                if !self.seen.contains_key(id) {
                    self.seen.insert(id.clone(), now);
                    self.reveal_until = now + REVEAL;
                }
            }
        }
        let running = match &doc.mode {
            Mode::Live { paused_at, .. } => paused_at.is_none(),
            Mode::Replay { clock, .. } => clock.playing,
            Mode::Static => false,
        };
        running || now < self.reveal_until
    }

    fn import(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let before: Vec<SessionId> = self.sessions.iter().map(|s| s.session_id.clone()).collect();
        for p in paths.iter().filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))) {
            let res = trace_ingest::import_file(&mut self.store, p, "canonical", &self.capture)
                .or_else(|_| trace_ingest::import_file(&mut self.store, p, "claude-code", &self.capture));
            if let Err(e) = res {
                self.error = Some(format!("{}: {e:#}", p.display()));
            }
        }
        self.refresh_sessions(cx);
        if let Some(new) = self.sessions.iter().find(|s| !before.contains(&s.session_id)).map(|s| s.session_id.clone()) {
            self.open(new, false);
        }
        cx.notify();
    }

    fn prompt_import(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |this, cx| this.import(paths, cx)).ok();
            }
        })
        .detach();
    }

    fn toggle_replay(&mut self) {
        let Some(doc) = &self.doc else { return };
        let id = doc.session_id().clone();
        let replaying = matches!(doc.mode, Mode::Replay { .. });
        self.open(id, !replaying);
    }

    fn toggle_follow(&mut self) {
        if let Some(TraceDoc { mode: Mode::Live { following, .. }, .. }) = &mut self.doc {
            *following = !*following;
            self.zoomed &= !*following;
        }
    }

    fn select(&mut self, i: usize) {
        if let Some(doc) = &mut self.doc {
            doc.select(i);
            self.list.scroll_to_item(doc.selected_row, ScrollStrategy::Top);
        }
    }

    /// Pans by `px` pixels of timeline, which also stops following.
    fn pan_px(&mut self, px: f32) {
        let Some(doc) = &mut self.doc else { return };
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        if let Some(vp) = &mut doc.viewport {
            vp.pan(f64::from(px));
        }
    }

    fn zoom_at(&mut self, factor: f64, anchor_x: f32) {
        self.zoomed = true;
        let Some(doc) = &mut self.doc else { return };
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        if let Some(vp) = &mut doc.viewport {
            let anchor = vp.start_ns + (f64::from(anchor_x) * vp.ns_per_cell) as Ns;
            vp.zoom(factor, anchor);
        }
    }

    fn zoom_to_span(&mut self, id: &SpanId) {
        self.zoomed = true;
        let Some(doc) = &mut self.doc else { return };
        let Some(s) = doc.state.spans.get(id) else { return };
        let end = s.effective_end(doc.state.session.ended_at_utc_ns, doc.now);
        let pad = ((end - s.started_at_utc_ns) / 10).max(1_000_000);
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        doc.viewport = Some(Viewport::fit(s.started_at_utc_ns - pad, end + pad, doc.timeline_width.max(1)));
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let typing = self.span_search.read(cx).focus_handle(cx).is_focused(window) || self.session_search.read(cx).focus_handle(cx).is_focused(window);
        if typing {
            if ks.key == "escape" {
                self.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }
        let named = ["up", "down", "left", "right", "enter", "escape", "space", "home", "end"];
        let key = if named.contains(&ks.key.as_str()) { ks.key.clone() } else { ks.key_char.clone().unwrap_or_else(|| ks.key.clone()) };
        if ks.modifiers.platform && ks.key == "f" {
            self.open_span_search(window, cx);
            return;
        }
        if ks.modifiers.platform || ks.modifiers.control {
            return;
        }
        if key == "escape" && self.popover {
            self.popover = false;
            cx.notify();
            return;
        }
        let Some(doc) = &mut self.doc else { return };
        match key.as_str() {
            "j" | "down" => doc.move_by(1),
            "k" | "up" => doc.move_by(-1),
            "h" | "left" => doc.left(),
            "l" | "right" => doc.right(),
            "g" | "home" => doc.select(0),
            "G" | "end" => doc.select(usize::MAX),
            "enter" => self.popover = doc.selected.is_some(),
            "escape" if !doc.search.is_empty() => {
                self.span_search.update(cx, |s, cx| s.set_value("", window, cx));
                self.span_search_open = false;
            }
            "e" => self.error = doc.toggle_descendants(),
            "+" | "=" | "-" => {
                doc.zoom(if key == "-" { 2.0 } else { 0.5 });
                self.zoomed = true;
            }
            "H" if matches!(doc.mode, Mode::Replay { .. }) => doc.seek_by(-1),
            "L" if matches!(doc.mode, Mode::Replay { .. }) => doc.seek_by(1),
            "H" => doc.pan(-1.0),
            "L" => doc.pan(1.0),
            " " | "space" => {
                self.last_frame = Instant::now();
                doc.toggle_pause();
            }
            "f" => self.toggle_follow(),
            "[" => doc.seek(|r, c| r.prev_event_time(c.position)),
            "]" => doc.seek(|r, c| r.next_event_time(c.position)),
            "<" | ">" => {
                if let Mode::Replay { clock, .. } = &mut doc.mode {
                    if key == "<" { clock.slower() } else { clock.faster() }
                }
            }
            "/" => self.open_span_search(window, cx),
            "r" => self.toggle_replay(),
            _ => return,
        }
        if let Some(doc) = &self.doc {
            self.list.scroll_to_item(doc.selected_row, ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn open_span_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.span_search_open = true;
        self.span_search.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }
}

fn apply_theme(dark: bool, cx: &mut App) {
    let p = Palette::new(dark);
    Theme::change(if dark { ThemeMode::Dark } else { ThemeMode::Light }, None, cx);
    Theme::update(cx, |t| {
        t.font_family = ".SystemUIFont".into();
        t.font_size = px(13.);
        t.radius = px(8.);
        t.shadow = false;
        t.background = p.card;
        t.foreground = p.ink;
        t.muted_foreground = p.muted;
        t.border = p.line;
        t.input = p.line;
        t.ring = p.accent;
        t.caret = p.accent;
        t.primary = p.accent;
        t.primary_hover = p.accent.opacity(0.9);
        t.primary_active = p.accent.opacity(0.8);
        t.primary_foreground = gpui_kit::white();
        t.button_primary = p.accent;
        t.button_primary_hover = p.accent.opacity(0.9);
        t.button_primary_active = p.accent.opacity(0.8);
        t.button_primary_foreground = gpui_kit::white();
        t.selection = p.accent.opacity(0.25);
    });
}

impl Render for TraceApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.tick() {
            window.request_animation_frame();
        }
        let pal = Palette::new(self.dark(window));
        let vw = window.viewport_size().width.to_f64() as f32;
        div()
            .id("root")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.import(paths.paths().to_vec(), cx)))
            .size_full()
            .flex()
            .bg(pal.tray)
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .text_color(pal.ink)
            .child(self.render_sidebar(&pal, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .pr(px(MAIN_PAD))
                    .pb(px(MAIN_PAD))
                    .child(self.render_header(&pal, window, cx))
                    .child(match self.doc.is_some() {
                        true => self.render_card(&pal, vw, window, cx).into_any_element(),
                        false => self.render_empty(&pal, cx).into_any_element(),
                    }),
            )
            .when_some(self.error.clone(), |el, e| el.child(toast(&pal, e, cx)))
    }
}

fn toast(pal: &Palette, msg: String, cx: &mut Context<TraceApp>) -> impl IntoElement {
    div().absolute().bottom(px(24.)).left(px(SIDEBAR_W + 24.)).child(
        div()
            .id("toast")
            .max_w(px(560.))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(10.))
            .bg(pal.error.bg)
            .border_1()
            .border_color(pal.error.edge)
            .text_color(pal.error.ink)
            .text_size(px(12.5))
            .child(Icon::new(IconName::TriangleAlert).size(px(14.)).text_color(pal.error.ink))
            .child(div().flex_1().line_clamp(3).child(msg))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.error = None;
                cx.notify();
            })),
    )
}

// ── Sidebar ────────────────────────────────────────────────────────────────

impl TraceApp {
    fn render_sidebar(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let today = Local::now().date_naive().and_hms_opt(0, 0, 0).and_then(|d| d.and_local_timezone(Local).earliest()).map_or(0, |d| d.timestamp_nanos_opt().unwrap_or(0));
        let open_id = self.doc.as_ref().map(|d| d.session_id().clone());
        let mut groups: Vec<(Group, Vec<&SessionSummary>)> = Vec::new();
        for s in &self.sessions {
            let g = present::group(s.status == SessionStatus::Live, s.started_at_utc_ns.or(s.last_event_at_utc_ns), today);
            match groups.iter_mut().find(|(k, _)| *k == g) {
                Some((_, v)) => v.push(s),
                None => groups.push((g, vec![s])),
            }
        }
        groups.sort_by_key(|(g, _)| *g);
        let mut list = div().id("sessions").flex_1().min_h(px(0.)).overflow_y_scroll().px(px(10.)).pb(px(10.));
        for (g, items) in groups {
            list = list.child(
                div().pt(px(14.)).pb(px(6.)).px(px(10.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.muted).child(g.title()),
            );
            for s in items {
                list = list.child(self.render_session_row(pal, s, open_id.as_ref() == Some(&s.session_id), today, cx));
            }
        }
        if self.sessions.is_empty() && !self.session_search_text(cx).is_empty() {
            list = list.child(div().px(px(10.)).pt(px(14.)).text_size(px(12.5)).text_color(pal.muted).child("No matching sessions"));
        }
        div()
            .w(px(SIDEBAR_W))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .child(div().h(px(HEADER_H)).flex_shrink_0())
            .child(
                div().px(px(14.)).pb(px(4.)).child(
                    div()
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .px(px(8.))
                        .rounded(px(8.))
                        .bg(if pal.dark { pal.chip } else { pal.card })
                        .border_1()
                        .border_color(pal.line)
                        .text_size(px(12.5))
                        .child(Input::new(&self.session_search).appearance(false).small().prefix(Icon::new(IconName::Search).size(px(13.)).text_color(pal.muted))),
                ),
            )
            .child(list)
            .child(
                div().px(px(14.)).pb(px(14.)).pt(px(6.)).child(
                    div()
                        .id("import")
                        .h(px(30.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .px(px(10.))
                        .rounded(px(8.))
                        .text_size(px(12.5))
                        .text_color(pal.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.hover.opacity(3.)))
                        .child(Icon::new(IconName::FileUp).size(px(14.)).text_color(pal.muted))
                        .child("Import JSONL…")
                        .on_click(cx.listener(|this, _, _, cx| this.prompt_import(cx))),
                ),
            )
    }

    fn render_session_row(&self, pal: &Palette, s: &SessionSummary, selected: bool, today: Ns, cx: &mut Context<Self>) -> impl IntoElement {
        let name = if !s.project_id.is_empty() {
            s.project_id.clone()
        } else {
            s.working_directory.rsplit('/').find(|p| !p.is_empty()).unwrap_or("Untitled session").to_owned()
        };
        let start = s.started_at_utc_ns.or(s.last_event_at_utc_ns);
        let when = start.and_then(|t| Local.timestamp_opt(t / 1_000_000_000, 0).single()).map(|t| {
            if start.unwrap_or(0) >= today { t.format("%H:%M").to_string() } else { t.format("%b %-d").to_string() }
        });
        let dur = match (s.started_at_utc_ns, s.ended_at_utc_ns.or(s.last_event_at_utc_ns)) {
            (Some(a), Some(b)) => Some(format_duration(b - a)),
            _ => None,
        };
        let meta = [when, dur, s.totals.cost_usd_micros().map(|c| format_cost(Some(c)))].into_iter().flatten().collect::<Vec<_>>().join(" · ");
        let dot = match s.status {
            SessionStatus::Live => pal.accent,
            SessionStatus::Completed => pal.ok.fg,
            SessionStatus::Failed => pal.error.mark,
            SessionStatus::Interrupted => pal.slow.fg,
            SessionStatus::Unknown => pal.faint,
        };
        let id = s.session_id.clone();
        let live = s.status == SessionStatus::Live;
        div()
            .id(SharedString::from(format!("session-{}", s.session_id)))
            .flex()
            .gap(px(10.))
            .px(px(10.))
            .py(px(7.))
            .rounded(px(9.))
            .cursor_pointer()
            .when(selected, |el| el.bg(pal.card).shadow(vec![soft_shadow(pal, 0.06, 1., 3.)]).border_1().border_color(pal.line))
            .when(!selected, |el| el.border_1().border_color(transparent_black()).hover(|st| st.bg(pal.hover.opacity(3.))))
            .child(div().pt(px(5.)).child(status_dot(dot, live, ("sdot", s.session_id.0.len() + s.span_count))))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(pal.ink).child(name))
                    .child(div().truncate().text_size(px(11.5)).text_color(pal.muted).font_features(tnum()).child(meta)),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open(id.clone(), false);
                cx.notify();
            }))
    }
}

fn soft_shadow(pal: &Palette, alpha: f32, y: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color: hsla(0., 0., 0., if pal.dark { alpha * 3. } else { alpha }),
        offset: point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
        inset: false,
    }
}

fn status_dot(color: Hsla, pulse: bool, id: impl Into<ElementId>) -> AnyElement {
    let dot = div().size(px(7.)).rounded_full().bg(color);
    if !pulse {
        return dot.into_any_element();
    }
    div()
        .size(px(7.))
        .relative()
        .child(
            div()
                .absolute()
                .inset_0()
                .rounded_full()
                .bg(color)
                .with_animation(id, Animation::new(Duration::from_millis(1400)).repeat(), move |el, t| {
                    let r = 3.5 + 4.0 * t;
                    el.top(px(3.5 - r)).left(px(3.5 - r)).size(px(2. * r)).opacity(0.45 * (1. - t))
                }),
        )
        .child(div().absolute().inset_0().rounded_full().bg(color))
        .into_any_element()
}

fn pill(p: Pill, label: &str, dot: Option<(bool, ElementId)>) -> impl IntoElement {
    div()
        .h(px(24.))
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .rounded_full()
        .bg(p.bg)
        .text_color(p.fg)
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .when_some(dot, |el, (pulse, id)| el.child(status_dot(p.fg, pulse, id)))
        .child(label.to_owned())
}

// ── Header ─────────────────────────────────────────────────────────────────

impl TraceApp {
    fn render_header(&self, pal: &Palette, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut right = div().flex().items_center().gap(px(8.));
        let mut subtitle = String::new();
        if let Some(doc) = &self.doc {
            let s = &doc.state.session;
            let model = self.sessions.iter().find(|x| &x.session_id == doc.session_id()).and_then(|x| x.model.as_deref()).map(present::pretty_model);
            subtitle = [Some(s.project_id.clone()).filter(|p| !p.is_empty()), Some(s.harness.clone()).filter(|h| !h.is_empty()), model]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            if self.span_search_open || !doc.search.is_empty() {
                right = right.child(
                    div()
                        .w(px(220.))
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .px(px(8.))
                        .rounded(px(8.))
                        .bg(pal.card)
                        .border_1()
                        .border_color(if self.span_search.read(cx).focus_handle(cx).is_focused(window) { pal.accent.opacity(0.6) } else { pal.line })
                        .text_size(px(12.5))
                        .child(Input::new(&self.span_search).appearance(false).small().cleanable(true).prefix(Icon::new(IconName::ListFilter).size(px(13.)).text_color(pal.muted))),
                );
            }
            let (p, label, dot) = match &doc.mode {
                Mode::Live { paused_at: Some(_), .. } => (Pill { bg: pal.chip, fg: pal.text }, "Paused", Some(false)),
                Mode::Live { .. } => (pal.brand, "Live", Some(true)),
                Mode::Replay { .. } => (pal.brand, "Replay", None),
                Mode::Static => match s.status {
                    SessionStatus::Failed => (Pill { bg: pal.error.bg, fg: pal.error.ink }, "Failed", Some(false)),
                    SessionStatus::Interrupted => (pal.slow, "Interrupted", Some(false)),
                    SessionStatus::Unknown => (Pill { bg: pal.chip, fg: pal.text }, "Unknown", Some(false)),
                    _ => (pal.ok, "Done", Some(false)),
                },
            };
            right = right.child(pill(p, label, dot.map(|pulse| (pulse, ElementId::from("hdot")))));
            if let Mode::Live { following, .. } = &doc.mode {
                let on = *following;
                right = right.child(
                    tool_button("follow", IconName::Crosshair, Some("Follow"), on, pal)
                        .tooltip(|w, cx| Tooltip::new("Keep now in view  F").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_follow();
                            cx.notify();
                        })),
                );
            }
            let replaying = matches!(doc.mode, Mode::Replay { .. });
            right = right.child(
                tool_button("replay", if replaying { IconName::X } else { IconName::RotateCcw }, Some(if replaying { "Exit replay" } else { "Replay" }), false, pal)
                    .tooltip(|w, cx| Tooltip::new("Replay from the start  R").build(w, cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_replay();
                        cx.notify();
                    })),
            );
        }
        div()
            .h(px(HEADER_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(6.))
            .child(Icon::new(IconName::Sparkle).size(px(16.)).text_color(pal.accent))
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child("Trace"))
            .child(div().flex_1().min_w(px(0.)).truncate().text_size(px(13.)).text_color(pal.muted).child(subtitle))
            .child(right)
    }
}

fn tool_button(id: &'static str, icon: IconName, label: Option<&str>, on: bool, pal: &Palette) -> Stateful<Div> {
    let fg = if on { pal.brand.fg } else { pal.text };
    div()
        .id(id)
        .h(px(28.))
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(if label.is_some() { 10. } else { 7. }))
        .rounded(px(8.))
        .border_1()
        .cursor_pointer()
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .when(on, |el| el.bg(pal.brand.bg).border_color(pal.brand.fg.opacity(0.18)))
        .when(!on, |el| el.bg(pal.card).border_color(pal.line).shadow(vec![soft_shadow(pal, 0.04, 1., 2.)]).hover(|s| s.bg(pal.wash)))
        .child(Icon::new(icon).size(px(13.)).text_color(fg))
        .when_some(label, |el, l| el.child(l.to_owned()))
}

// ── Trace card ─────────────────────────────────────────────────────────────

impl TraceApp {
    fn geo(&mut self, vw: f32) -> Option<Geo> {
        let zoomed = self.zoomed;
        let doc = self.doc.as_mut()?;
        let card_w = vw - SIDEBAR_W - MAIN_PAD - 2.;
        let name_w = (card_w * 0.3).clamp(230., 300.);
        let tl_w = (card_w - name_w - TL_PAD_R).max(40.);
        if doc.viewport.is_none() {
            let fit_w = if matches!(doc.mode, Mode::Live { .. }) { tl_w } else { tl_w * 0.86 };
            doc.viewport = Some(doc.fit(fit_w as u16));
        }
        let mut vp = doc.layout(tl_w as u16);
        if !zoomed && matches!(doc.mode, Mode::Live { following: true, .. }) {
            vp = Viewport::fit(doc.state.axis_start().unwrap_or(doc.now), doc.now, (tl_w * 0.9) as u16);
            doc.viewport = Some(vp);
        }
        let origin = match &doc.mode {
            Mode::Replay { replay, .. } => replay.start().unwrap_or(0),
            _ => doc.state.axis_start().unwrap_or(doc.now),
        };
        Some(Geo { name_w, tl_w, vp, now: doc.now, origin })
    }

    fn render_card(&mut self, pal: &Palette, vw: f32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.geo(vw).expect("render_card runs with a document");
        let doc = self.doc.as_ref().expect("checked by caller");
        let ticks = tick_times(&g.vp, f64::from(g.tl_w), g.origin, 90.);
        let show_now = !matches!(doc.mode, Mode::Static);
        let now_x = g.x(g.now);
        let count = doc.rows.len();
        let grid = div().absolute().inset_0().children(ticks.iter().map(|(t, _)| {
            div().absolute().top_0().bottom_0().left(px(g.name_w + g.x(*t).round())).w(px(1.)).bg(pal.grid)
        }));
        let axis = div()
            .h(px(AXIS_H))
            .flex_shrink_0()
            .relative()
            .border_b_1()
            .border_color(pal.line)
            .child(div().absolute().left(px(16.)).top_0().h_full().flex().items_center().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(pal.muted).child("Span"))
            .children(ticks.iter().map(|(t, l)| {
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .left(px(g.name_w + g.x(*t).round() + 5.))
                    .text_size(px(11.))
                    .text_color(pal.muted)
                    .font_features(tnum())
                    .child(l.clone())
            }));
        let body = div()
            .id("body")
            .flex_1()
            .min_h(px(0.))
            .relative()
            .on_scroll_wheel(cx.listener(move |this, ev: &ScrollWheelEvent, _, cx| {
                let d = ev.delta.pixel_delta(px(ROW_H));
                let (dx, dy) = (d.x.to_f64() as f32, d.y.to_f64() as f32);
                let x = ev.position.x.to_f64() as f32 - SIDEBAR_W - 1. - g.name_w;
                if ev.modifiers.platform {
                    this.zoom_at(f64::from((1.0 - dy * 0.01).clamp(0.5, 2.0)), x);
                } else if dx.abs() > dy.abs() {
                    this.pan_px(-dx);
                } else {
                    return;
                }
                cx.notify();
            }))
            .on_pinch(cx.listener(move |this, ev: &PinchEvent, _, cx| {
                let x = ev.position.x.to_f64() as f32 - SIDEBAR_W - 1. - g.name_w;
                this.zoom_at(f64::from(1.0 / (1.0 + ev.delta).max(0.2)), x);
                cx.notify();
            }))
            .child(grid)
            .when(show_now && (0.0..=g.tl_w).contains(&now_x), |el| {
                el.child(div().absolute().top_0().bottom_0().left(px(g.name_w + now_x.round())).w(px(1.)).bg(pal.now))
            })
            .child(
                uniform_list("rows", count, cx.processor(move |this, range: std::ops::Range<usize>, window, cx| range.map(|ix| this.render_row(ix, g, window, cx)).collect::<Vec<_>>()))
                    .track_scroll(&self.list)
                    .size_full()
                    .pb(px(if matches!(doc.mode, Mode::Replay { .. }) { 72. } else { 8. })),
            )
            
            .child(gpui_kit::component::scroll::Scrollbar::vertical(&self.list));
        let stats = self.render_stats(pal);
        let transport = matches!(doc.mode, Mode::Replay { .. }).then(|| self.render_transport(pal, &g, cx));
        let _ = window;
        div()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .relative()
            .bg(pal.card)
            .rounded(px(16.))
            .border_1()
            .border_color(pal.line)
            .shadow(vec![soft_shadow(pal, 0.04, 1., 3.)])
            .overflow_hidden()
            .child(stats)
            .child(axis)
            .child(body)
            .when(show_now && (0.0..=g.tl_w).contains(&now_x), |el| {
                let x = g.name_w + now_x.round();
                el.child(div().absolute().top(px(STATS_H + AXIS_H - 3.)).left(px(x - 3. + 0.5)).size(px(6.)).rounded_full().bg(pal.accent))
                    .child(div().absolute().top(px(STATS_H + AXIS_H - 10.)).bottom(px(0.)).left(px(x)).w(px(1.)).bg(pal.now).h(px(10.)))
            })
            .children(transport)
    }

    fn render_stats(&self, pal: &Palette) -> impl IntoElement {
        let doc = self.doc.as_ref().expect("stats need a document");
        let t = doc.state.totals();
        let count = |k: SpanKind| doc.state.spans.values().filter(|s| s.kind == k).count();
        let cost = match (t.cost_usd_micros(), t.cost_unknown_records > 0) {
            (Some(c), true) => format!("≥ {}", format_cost(Some(c))),
            (c, _) => format_cost(c),
        };
        let cells = [
            ("Duration", format_duration(doc.state.wall_clock(doc.now)), pal.ink),
            ("Cost", cost, pal.ink),
            ("Tokens", format_tokens(t.tokens_input + t.tokens_output), pal.ink),
            ("LLM calls", count(SpanKind::Llm).to_string(), pal.ink),
            ("Tool calls", count(SpanKind::Tool).to_string(), pal.ink),
            ("Errors", t.failed_spans.to_string(), if t.failed_spans > 0 { pal.error.ink } else { pal.ink }),
        ];
        div().h(px(STATS_H)).flex_shrink_0().flex().border_b_1().border_color(pal.line).children(cells.into_iter().enumerate().map(|(i, (label, value, color))| {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(3.))
                .px(px(16.))
                .when(i > 0, |el| el.border_l_1().border_color(pal.line))
                .child(div().text_size(px(12.)).text_color(pal.muted).truncate().child(label))
                .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).text_color(color).font_features(tnum()).truncate().child(value))
        }))
    }

    fn render_row(&mut self, ix: usize, g: Geo, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = Palette::new(self.dark(window));
        let doc = self.doc.as_ref().expect("rows need a document");
        let row = doc.rows[ix].clone();
        let RowKind::Span(id) = &row.kind else {
            return div()
                .h(px(ROW_H))
                .flex()
                .items_center()
                .pl(px(16.))
                .text_size(px(12.))
                .text_color(pal.muted)
                .child("Waiting for parent spans")
                .into_any_element();
        };
        let s = &doc.state.spans[id];
        let selected = doc.selected.as_ref() == Some(id);
        let hovered = self.hovered == Some(ix);
        let live = !matches!(doc.mode, Mode::Static);
        let running = live && s.ended_at_utc_ns.is_none() && s.status == SpanStatus::Running;
        let tone = present::tone(s);
        let kc = pal.kind(tone);
        let (icon, ink) = present::glyph(s);
        let detail = present::detail(s, &doc.state.session.harness);
        let indent = 12. + f32::from(row.depth) * INDENT;
        let hidden = if row.collapsed { present::descendants(&doc.state, id, 100_000) } else { Vec::new() };
        let slowest = doc.state.slowest(SLOWEST_DEFAULT_KINDS).map(|x| &x.span_id) == Some(id);

        let mut name = div()
            .w(px(g.name_w))
            .flex_shrink_0()
            .h_full()
            .relative()
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(indent))
            .pr(px(10.))
            .children((0..row.depth).map(|d| div().absolute().top_0().bottom_0().left(px(12. + f32::from(d) * INDENT + 6.5)).w(px(1.)).bg(pal.guide)));
        let chevron_id = id.clone();
        name = name.child(
            div()
                .id(("chev", ix))
                .size(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(row.has_children, |el| {
                    el.cursor_pointer()
                        .child(Icon::new(if row.collapsed { IconName::ChevronRight } else { IconName::ChevronDown }).size(px(12.)).text_color(pal.muted))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(doc) = &mut this.doc {
                                if !doc.collapsed.remove(&chevron_id) {
                                    doc.collapsed.insert(chevron_id.clone());
                                }
                                doc.refresh();
                            }
                            cx.notify();
                        }))
                }),
        );
        name = name
            .child(icon_tile(icon, pal.ink_of(ink), 20., &pal))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(relative(0.8))
                            .truncate()
                            .text_size(px(13.))
                            .when(row.has_children, |el| el.font_weight(FontWeight::MEDIUM))
                            .text_color(pal.ink)
                            .child(present::title(s)),
                    )
                    .when_some(detail, |el, d| el.child(div().flex_shrink(8.).min_w(px(0.)).max_w(px(160.)).truncate().text_size(px(12.5)).text_color(pal.muted).child(d)))
                    .when(row.collapsed, |el| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .h(px(18.))
                                .px(px(6.))
                                .flex()
                                .items_center()
                                .rounded(px(6.))
                                .bg(pal.chip)
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(pal.muted)
                                .font_features(tnum())
                                .child(format!("+{}", hidden.len())),
                        )
                    })
                    .when(running, |el| el.child(Spinner::new().color(pal.accent).with_size(px(12.))))
                    .when(s.status == SpanStatus::Failed, |el| el.child(Icon::new(IconName::TriangleAlert).size(px(13.)).text_color(pal.error.ink))),
            );

        let timeline = self.render_bar(ix, s, &g, kc, &pal, hovered, slowest, &hidden, window);
        let click_id = id.clone();
        let mut el = div()
            .id(("row", ix))
            .h(px(ROW_H))
            .w_full()
            .relative()
            .flex()
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .top(px(1.))
                    .bottom(px(1.))
                    .left(px(6.))
                    .right(px(6.))
                    .rounded(px(8.))
                    .when(hovered && !selected, |el| el.bg(pal.hover))
                    .when(selected, |el| el.bg(pal.selected).border_1().border_color(pal.selected_ring)),
            )
            .child(name)
            .child(timeline)
            .on_hover(cx.listener(move |this, h: &bool, _, cx| {
                let next = if *h { Some(ix) } else if this.hovered == Some(ix) { None } else { this.hovered };
                if next != this.hovered {
                    this.hovered = next;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.select(ix);
                if ev.click_count() >= 2 {
                    this.zoom_to_span(&click_id);
                } else {
                    this.popover = true;
                }
                this.focus.focus(window, cx);
                cx.notify();
            }));
        if selected && self.popover {
            let end = s.effective_end(doc.state.session.ended_at_utc_ns, g.now);
            let x = g.name_w + present::popover_x(g.x(s.started_at_utc_ns), g.x(end), g.tl_w + TL_PAD_R, POPOVER_W);
            el = el.child(
                deferred(
                    anchored()
                        .position_mode(AnchoredPositionMode::Local)
                        .position(point(px(x), px(ROW_H + 4.)))
                        .snap_to_window_with_margin(px(12.))
                        .child(self.render_popover(s, &pal, cx)),
                )
                .with_priority(1),
            );
        }
        el.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_bar(&self, ix: usize, s: &Span, g: &Geo, kc: KindColors, pal: &Palette, hovered: bool, slowest: bool, hidden: &[&Span], window: &mut Window) -> impl IntoElement + use<> {
        let doc = self.doc.as_ref().expect("bars need a document");
        let session_end = doc.state.session.ended_at_utc_ns;
        let end = s.effective_end(session_end, g.now);
        let open = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        let x0 = g.x(s.started_at_utc_ns);
        let x1 = g.x(end);
        let reveal = self.seen.get(&s.span_id).map_or(1., |t| ease(t.elapsed().as_secs_f32() / REVEAL.as_secs_f32()));
        let x1 = x0 + (x1 - x0) * reveal;
        let mut cell = div().w(px(g.tl_w)).h_full().relative().overflow_hidden();
        if x1 < -2. || x0 > g.tl_w + 2. {
            return cell;
        }
        let bx = x0.max(-6.).round();
        let bw = (x1.min(g.tl_w + 6.) - bx).max(3.).round();
        let top = (ROW_H - BAR_H) / 2.;
        let mut bar = div()
            .absolute()
            .left(px(bx))
            .top(px(top))
            .w(px(bw))
            .h(px(BAR_H))
            .rounded(px(5.))
            .border_1()
            .overflow_hidden()
            .border_color(if hovered { kc.mark } else { kc.edge });
        bar = if open {
            bar.bg(linear_gradient(90., linear_color_stop(kc.bg, 0.55), linear_color_stop(kc.bg.opacity(0.0), 1.))).border_color(kc.edge.opacity(0.7)).child(
                div().absolute().top_0().bottom_0().w(px(60.)).bg(linear_gradient(90., linear_color_stop(kc.mark.opacity(0.), 0.), linear_color_stop(kc.mark.opacity(0.18), 1.))).with_animation(
                    ("sweep", ix),
                    Animation::new(Duration::from_millis(1500)).repeat(),
                    move |el, t| el.left(px(-60. + (bw + 60.) * ease(t))),
                ),
            )
        } else {
            bar.bg(kc.bg)
        };
        for k in hidden.iter().filter(|k| matches!(k.kind, SpanKind::Llm | SpanKind::Tool) || k.status == SpanStatus::Failed).take(400) {
            let kx0 = g.x(k.started_at_utc_ns) - bx;
            let kx1 = g.x(k.effective_end(session_end, g.now)) - bx;
            if kx1 < 0. || kx0 > bw {
                continue;
            }
            let mark = pal.kind(present::tone(k)).mark;
            bar = bar.child(div().absolute().top(px((BAR_H - 2. - 6.) / 2.)).left(px(kx0.max(1.))).w(px((kx1 - kx0.max(1.)).max(2.))).h(px(6.)).rounded(px(2.)).bg(mark.opacity(0.6)));
        }
        cell = cell.child(bar);
        let label = present::bar_label(s, session_end, g.now);
        let lw = text_width(window, &label, LABEL_SIZE);
        let tag_w = if slowest { 58. } else { 0. };
        let at = present::label_at(bx, bw, lw + tag_w, g.tl_w);
        let lx = match at {
            LabelAt::After => bx + bw + present::LABEL_GAP,
            LabelAt::Inside => bx + bw - 8. - lw,
            LabelAt::Before => bx - present::LABEL_GAP - lw - tag_w,
        };
        let color = if at == LabelAt::Inside { kc.ink } else { pal.muted };
        cell = cell.child(
            div()
                .absolute()
                .left(px(lx))
                .top_0()
                .h_full()
                .flex()
                .items_center()
                .gap(px(8.))
                .whitespace_nowrap()
                .text_size(px(LABEL_SIZE))
                .text_color(color)
                .font_features(tnum())
                .child(label)
                .when(slowest && at != LabelAt::Inside, |el| {
                    el.child(
                        div()
                            .h(px(17.))
                            .px(px(6.))
                            .flex()
                            .items_center()
                            .rounded(px(5.))
                            .bg(pal.slow.bg)
                            .text_color(pal.slow.fg)
                            .text_size(px(10.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Slowest"),
                    )
                }),
        );
        cell
    }

    fn render_popover(&self, s: &Span, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let doc = self.doc.as_ref().expect("popover needs a document");
        let (icon, ink) = present::glyph(s);
        let sub = [Some(present::kind_label(s.kind).to_owned()), s.model.as_deref().map(present::pretty_model).or_else(|| present::detail(s, &doc.state.session.harness))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let open = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        let (status, label) = match s.status {
            SpanStatus::Failed => (Pill { bg: pal.error.bg, fg: pal.error.ink }, "Failed"),
            SpanStatus::Succeeded => (pal.ok, "Done"),
            SpanStatus::Cancelled => (Pill { bg: pal.chip, fg: pal.text }, "Cancelled"),
            SpanStatus::Running if open => (pal.brand, "Running"),
            _ => (Pill { bg: pal.chip, fg: pal.text }, "Unknown"),
        };
        let stats = present::popover_stats(&doc.state, s, doc.now);
        let cell = |(k, v): &(&'static str, String)| {
            div()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(div().text_size(px(11.)).text_color(pal.muted).child(*k))
                .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).font_features(tnum()).truncate().child(v.clone()))
        };
        let section = |title: &'static str, body: Option<String>| {
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .child(div().text_size(px(11.5)).font_weight(FontWeight::MEDIUM).text_color(pal.muted).child(title))
                .child(match body.filter(|b| !b.trim().is_empty()) {
                    Some(b) => div().text_size(px(12.5)).line_height(px(18.)).text_color(pal.text).line_clamp(4).child(b.chars().take(600).collect::<String>()),
                    None => div().text_size(px(12.5)).text_color(pal.faint).child("Not captured"),
                })
        };
        let start = s.started_at_utc_ns;
        div()
            .id("popover")
            .occlude()
            .w(px(POPOVER_W))
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(16.))
            .rounded(px(16.))
            .bg(pal.pop)
            .border_1()
            .border_color(pal.line)
            .shadow(vec![soft_shadow(pal, 0.12, 16., 40.), soft_shadow(pal, 0.06, 2., 6.)])
            .cursor_default()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.popover = false;
                cx.notify();
            }))
            .on_click(|_, _, cx| cx.stop_propagation())
            .with_animation("pop", Animation::new(Duration::from_millis(180)).with_easing(ease), |el, t| el.opacity(t).mt(px(6. * (1. - t))))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(icon_tile(icon, pal.ink_of(ink), 28., pal))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child(present::title(s)))
                            .child(div().truncate().text_size(px(12.)).text_color(pal.muted).child(sub)),
                    )
                    .child(pill(status, label, None))
                    .child(
                        div()
                            .id("close")
                            .size(px(24.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(7.))
                            .cursor_pointer()
                            .hover(|st| st.bg(pal.chip))
                            .child(Icon::new(IconName::X).size(px(14.)).text_color(pal.muted))
                            .on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.popover = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(pal.wash)
                    .child(div().flex().gap(px(10.)).children(stats[..3].iter().map(cell)))
                    .child(div().flex().gap(px(10.)).children(stats[3..].iter().map(cell))),
            )
            .when_some(s.error.clone().or_else(|| (s.status == SpanStatus::Failed).then(|| "Failed without an error message".into())), |el, e| {
                el.child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .p(px(10.))
                        .rounded(px(10.))
                        .bg(pal.error.bg)
                        .text_size(px(12.5))
                        .text_color(pal.error.ink)
                        .child(Icon::new(IconName::TriangleAlert).size(px(14.)).text_color(pal.error.ink))
                        .child(div().flex_1().line_clamp(4).child(e)),
                )
            })
            .child(section("Input", s.input_preview.clone()))
            .child(section("Output", s.output_preview.clone()))
            .when(!s.events.is_empty(), |el| {
                el.child(list_section(
                    pal,
                    format!("Events  {}", s.events.len()),
                    s.events.iter().take(4).map(|e| (e.name.clone(), format!("+{}", format_duration(e.timestamp_utc_ns - start)))).collect(),
                ))
            })
            .when(!s.artifacts.is_empty(), |el| {
                el.child(list_section(
                    pal,
                    format!("Files  {}", s.artifacts.len()),
                    s.artifacts.iter().take(4).map(|a| (a.path.clone(), a.change_summary.clone().unwrap_or_default())).collect(),
                ))
            })
    }

    fn render_transport(&self, pal: &Palette, g: &Geo, cx: &mut Context<Self>) -> impl IntoElement {
        let doc = self.doc.as_ref().expect("transport needs a document");
        let Mode::Replay { replay, clock } = &doc.mode else { unreachable!("transport renders only in replay") };
        let (start, end) = (replay.start().unwrap_or(0), replay.end().unwrap_or(0));
        let frac = ((clock.position - start) as f64 / (end - start).max(1) as f64) as f32;
        let speed_ix = clock.speed_index;
        let playing = clock.playing;
        let _ = g;
        let icon_btn = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .size(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .cursor_pointer()
                .hover(|s| s.bg(pal.chip))
                .child(Icon::new(icon).size(px(15.)).text_color(pal.ink))
                .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
        };
        let scrub = self.scrubber.clone();
        let seek_to = move |this: &mut Self, x: Pixels| {
            let b = scrub.get();
            let f = ((x - b.origin.x).to_f64() / b.size.width.to_f64().max(1.)).clamp(0., 1.);
            if let Some(doc) = &mut this.doc {
                doc.seek(|r, _| Some(r.start().unwrap_or(0) + (f * (r.end().unwrap_or(0) - r.start().unwrap_or(0)) as f64) as Ns));
            }
        };
        let seek_down = seek_to.clone();
        let capture = self.scrubber.clone();
        div().absolute().bottom(px(18.)).left_0().right_0().flex().justify_center().child(
            div()
                .id("transport")
                .occlude()
                .h(px(46.))
                .flex()
                .items_center()
                .gap(px(4.))
                .pl(px(8.))
                .pr(px(16.))
                .rounded_full()
                .bg(pal.pop)
                .border_1()
                .border_color(pal.line)
                .shadow(vec![soft_shadow(pal, 0.10, 8., 24.), soft_shadow(pal, 0.05, 1., 3.)])
                .child(icon_btn("back", IconName::SkipBack, "Previous event  [").on_click(cx.listener(|this, _, _, cx| {
                    if let Some(doc) = &mut this.doc {
                        doc.seek(|r, c| r.prev_event_time(c.position));
                    }
                    cx.notify();
                })))
                .child(
                    div()
                        .id("play")
                        .size(px(34.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(pal.accent)
                        .cursor_pointer()
                        .child(Icon::new(if playing { IconName::Pause } else { IconName::Play }).size(px(15.)).text_color(white()))
                        .tooltip(|w, cx| Tooltip::new("Play or pause  Space").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.last_frame = Instant::now();
                            if let Some(doc) = &mut this.doc {
                                doc.toggle_pause();
                            }
                            cx.notify();
                        })),
                )
                .child(icon_btn("fwd", IconName::SkipForward, "Next event  ]").on_click(cx.listener(|this, _, _, cx| {
                    if let Some(doc) = &mut this.doc {
                        doc.seek(|r, c| r.next_event_time(c.position));
                    }
                    cx.notify();
                })))
                .child(div().w(px(1.)).h(px(20.)).mx(px(8.)).bg(pal.line))
                .child(div().flex().items_center().p(px(2.)).rounded(px(8.)).bg(pal.chip).children(SPEEDS.iter().enumerate().map(|(i, sp)| {
                    let on = i == speed_ix;
                    div()
                        .id(("speed", i))
                        .h(px(22.))
                        .px(px(7.))
                        .flex()
                        .items_center()
                        .rounded(px(6.))
                        .text_size(px(11.5))
                        .font_features(tnum())
                        .font_weight(if on { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                        .text_color(if on { pal.ink } else { pal.muted })
                        .cursor_pointer()
                        .when(on, |el| el.bg(pal.card).shadow(vec![soft_shadow(pal, 0.08, 1., 2.)]))
                        .child(format!("{sp}×"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(TraceDoc { mode: Mode::Replay { clock, .. }, .. }) = &mut this.doc {
                                clock.speed_index = i;
                            }
                            cx.notify();
                        }))
                })))
                .child(div().w(px(1.)).h(px(20.)).mx(px(8.)).bg(pal.line))
                .child(
                    div()
                        .id("scrubber")
                        .w(px(180.))
                        .h(px(20.))
                        .flex()
                        .items_center()
                        .relative()
                        .cursor_pointer()
                        .child(canvas(move |b, _, _| capture.set(b), |_, _, _, _| {}).absolute().inset_0())
                        .child(div().w_full().h(px(4.)).rounded_full().bg(pal.chip))
                        .child(div().absolute().left_0().top(px(8.)).h(px(4.)).w(px(180. * frac)).rounded_full().bg(pal.accent))
                        .child(div().absolute().top(px(4.)).left(px(180. * frac - 6.)).size(px(12.)).rounded_full().bg(pal.card).border_1().border_color(pal.line).shadow(vec![soft_shadow(pal, 0.15, 1., 3.)]))
                        .on_mouse_down(MouseButton::Left, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                            seek_down(this, ev.position.x);
                            cx.notify();
                        }))
                        .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _, cx| {
                            if ev.pressed_button == Some(MouseButton::Left) {
                                seek_to(this, ev.position.x);
                                cx.notify();
                            }
                        })),
                )
                .child(
                    div()
                        .ml(px(12.))
                        .text_size(px(12.))
                        .font_features(tnum())
                        .text_color(pal.text)
                        .child(format!("{} / {}", clock_time(clock.position - start), clock_time(end - start))),
                ),
        )
    }

    fn render_empty(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex_1().flex().items_center().justify_center().rounded(px(16.)).bg(pal.card).border_1().border_color(pal.line).child(
            div()
                .w(px(400.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(14.))
                .child(icon_tile(IconName::Activity, pal.accent, 44., pal))
                .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child("No traces yet"))
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .text_color(pal.text)
                        .text_center()
                        .child("Start the collector and run an agent, or import a JSONL trace. You can also drop a .jsonl file anywhere in this window."),
                )
                .child(
                    div()
                        .px(px(10.))
                        .py(px(6.))
                        .rounded(px(8.))
                        .bg(pal.wash)
                        .border_1()
                        .border_color(pal.line)
                        .font_family("Menlo")
                        .text_size(px(12.))
                        .text_color(pal.ink)
                        .child("trace-tui collector start"),
                )
                .child(
                    Button::new("import-empty")
                        .label("Import JSONL…")
                        .icon(Icon::new(IconName::FileUp))
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| this.prompt_import(cx))),
                ),
        )
    }
}

const STATS_H: f32 = 64.;

fn list_section(pal: &Palette, title: String, items: Vec<(String, String)>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(div().text_size(px(11.5)).font_weight(FontWeight::MEDIUM).text_color(pal.muted).child(title))
        .children(items.into_iter().map(|(a, b)| {
            div()
                .flex()
                .gap(px(8.))
                .text_size(px(12.))
                .child(div().flex_1().min_w(px(0.)).truncate().text_color(pal.text).child(a))
                .child(div().flex_shrink_0().text_color(pal.muted).font_features(tnum()).child(b))
        }))
}

fn icon_tile(icon: IconName, color: Hsla, size: f32, pal: &Palette) -> impl IntoElement {
    div()
        .size(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px((size * 0.3).round()))
        .bg(pal.card)
        .border_1()
        .border_color(pal.line)
        .shadow(vec![soft_shadow(pal, 0.04, 1., 1.)])
        .child(Icon::new(icon).size(px((size * 0.6).round())).text_color(color))
}

fn clock_time(ns: Ns) -> String {
    let s = ns.max(0) / 1_000_000_000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn text_width(window: &mut Window, text: &str, size: f32) -> f32 {
    let mut f = font(".SystemUIFont");
    f.features = tnum();
    let run = TextRun { len: text.len(), font: f, color: black(), background_color: None, underline: None, strikethrough: None };
    window.text_system().shape_line(text.to_owned().into(), px(size), &[run], None).width.to_f64() as f32
}
