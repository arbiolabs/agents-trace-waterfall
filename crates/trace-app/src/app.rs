use crate::Cli;
use crate::present::{self, Change, Edge, Grab, Group, LabelAt, Tone, ease};
use crate::theme::{KindColors, Palette};
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
use trace_view::{Mode, Row, RowKind, TraceDoc, Viewport, format_cost, format_duration, format_tokens, tick_times};

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
const OVERVIEW_H: f32 = 40.;
const STRIP_H: f32 = 28.;
const SIDEBAR_AUTO_BELOW: f32 = 1100.;
const SIDEBAR_ANIM: Duration = Duration::from_millis(240);
const FOLD_ANIM: Duration = Duration::from_millis(240);
const POP_IN: Duration = Duration::from_millis(180);
const POP_OUT: Duration = Duration::from_millis(160);
/// Left edge of the sidebar toggle, just right of the traffic lights.
const TOGGLE_X: f32 = 84.;
const BENCH_FRAMES: usize = 300;
const COLLECTOR_CMD: &str = "trace-tui collector start";
const COPIED_FOR: Duration = Duration::from_millis(1200);

fn tnum() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".into(), 1)]))
}


/// Pixel geometry of the trace card for one frame.
#[derive(Clone, Copy)]
struct Geo {
    /// Window x of the card's inner left edge.
    left: f32,
    name_w: f32,
    tl_w: f32,
    vp: Viewport,
    now: Ns,
    origin: Ns,
    /// The whole session, as the overview strip shows it.
    overview: (Ns, Ns),
}

impl Geo {
    fn x(&self, t: Ns) -> f32 {
        if t < self.vp.start_ns {
            return -((self.vp.start_ns - t) as f64 / self.vp.ns_per_cell) as f32;
        }
        self.vp.col_of(t) as f32
    }

    /// Visible time window.
    fn window(&self) -> (Ns, Ns) {
        (self.vp.start_ns, self.vp.end_ns(self.tl_w as u16))
    }

    /// Overview strip x of `t`.
    fn ov_x(&self, t: Ns) -> f32 {
        let (a, b) = self.overview;
        ((t - a) as f64 / (b - a).max(1) as f64 * f64::from(self.tl_w)) as f32
    }

    fn ov_t(&self, x: f32) -> Ns {
        let (a, b) = self.overview;
        a + (f64::from(x / self.tl_w) * (b - a) as f64) as Ns
    }
}

/// What a press on the overview strip grabbed.
#[derive(Clone, Copy)]
enum LensDrag {
    Move { grab_ns: Ns },
    Start,
    End,
}

/// A fold in flight: the toggled parent and every row before and after, marked by what the fold does to it.
struct FoldAnim {
    id: SpanId,
    start: Instant,
    rows: Vec<(Row, Change)>,
}

/// The detail popover's lifecycle; it stays mounted while it fades out.
#[derive(Clone, Copy)]
enum Pop {
    Closed,
    Open(Instant),
    Closing(Instant),
}

/// `--bench-scroll`: frame intervals and render build times in milliseconds.
struct Bench {
    last: Instant,
    intervals: Vec<f32>,
    builds: Vec<f32>,
    rows: Vec<f32>,
    /// `--bench-fold`: toggle a fold every few frames instead of scrolling. Holds the parent once picked.
    fold: Option<Option<SpanId>>,
}

pub struct TraceApp {
    store: Store,
    capture: trace_ingest::Capture,
    sessions: Vec<SessionSummary>,
    journal_seq: i64,
    doc: Option<TraceDoc>,
    popover: Pop,
    /// Popover content height from the last frame, so the open and close animations can clip it.
    pop_h: Rc<Cell<f32>>,
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
    /// The user's explicit sidebar choice; `None` follows the window width.
    sidebar_pref: Option<bool>,
    /// Width animation: from, to, started.
    sidebar_anim: (f32, f32, Instant),
    fold_anim: Option<FoldAnim>,
    /// `--freeze-anim`: every transition holds at this progress.
    freeze: Option<f32>,
    reduce_motion: bool,
    lens_drag: Option<LensDrag>,
    /// `--viewport` seconds, applied on first layout.
    initial_window: Option<(f64, f64)>,
    bench: Option<Bench>,
    /// When the empty state's command was last copied, for the brief check mark.
    copied_at: Option<Instant>,
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
            popover: Pop::Closed,
            pop_h: Rc::new(Cell::new(0.)),
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
            sidebar_pref: None,
            sidebar_anim: (0., 0., Instant::now() - SIDEBAR_ANIM),
            fold_anim: None,
            freeze: cli.freeze_anim,
            reduce_motion: cx.reduce_motion(),
            lens_drag: None,
            initial_window: cli.viewport,
            bench: (cli.bench_scroll || cli.bench_fold).then(|| Bench { last: Instant::now(), intervals: Vec::new(), builds: Vec::new(), rows: Vec::new(), fold: cli.bench_fold.then_some(None) }),
            copied_at: None,
            _subs: subs,
        };
        let w = this.sidebar_target(window.viewport_size().width.to_f64() as f32);
        this.sidebar_anim.0 = w;
        this.sidebar_anim.1 = w;
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
            let mut hits: Vec<&Span> = doc.state.spans.values().filter(|s| s.name.to_lowercase().contains(&needle)).collect();
            hits.sort_by(|a, b| (a.started_at_utc_ns, &a.span_id).cmp(&(b.started_at_utc_ns, &b.span_id)));
            if let Some(id) = hits.first().map(|s| s.span_id.clone()) {
                let mut up = doc.state.spans[&id].parent_span_id.clone();
                while let Some(p) = up {
                    doc.collapsed.remove(&p);
                    up = doc.state.spans.get(&p).and_then(|s| s.parent_span_id.clone());
                }
                doc.refresh();
                if let Some(i) = doc.rows.iter().position(|r| r.kind == RowKind::Span(id.clone())) {
                    doc.select(i);
                    this.list.scroll_to_item(i, ScrollStrategy::Center);
                }
            }
        }
        if cli.popover && this.doc.as_ref().is_some_and(|d| d.selected.is_some()) {
            this.popover = Pop::Open(Instant::now());
        } else if let (Some(_), Some(_), Some(id)) = (cli.freeze_anim, &cli.select, this.doc.as_ref().and_then(|d| d.selected.clone())) {
            this.toggle_fold(&id);
        }
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
            Ok(mut doc) => {
                doc.collapsed.extend(present::default_folds(&doc.state));
                doc.refresh();
                let now = Instant::now();
                self.seen = doc.state.spans.keys().map(|k| (k.clone(), now)).collect();
                self.reveal_until = now + REVEAL;
                self.doc = Some(doc);
                self.zoomed = false;
                self.popover = Pop::Closed;
                self.hovered = None;
                self.fold_anim = None;
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
        if self.fold_anim.as_ref().is_some_and(|f| self.progress(f.start, FOLD_ANIM) >= 1.) {
            self.fold_anim = None;
        }
        if let Pop::Closing(t) = self.popover
            && self.progress(t, POP_OUT) >= 1.
        {
            self.popover = Pop::Closed;
        }
        let popping = match self.popover {
            Pop::Open(t) => self.progress(t, POP_IN) < 1.,
            Pop::Closing(_) => true,
            Pop::Closed => false,
        };
        running || now < self.reveal_until || self.fold_anim.is_some() || popping
    }

    /// Linear progress of a transition that began at `start`, held by `--freeze-anim` and finished at once under Reduce Motion.
    fn progress(&self, start: Instant, dur: Duration) -> f32 {
        if self.reduce_motion {
            return 1.;
        }
        self.freeze.unwrap_or_else(|| start.elapsed().as_secs_f32() / dur.as_secs_f32()).clamp(0., 1.)
    }

    /// How far the popover is shown, eased: 0 hidden, 1 fully open.
    fn pop_shown(&self) -> f32 {
        match self.popover {
            Pop::Open(t) => ease(self.progress(t, POP_IN)),
            Pop::Closing(t) => 1. - ease(self.progress(t, POP_OUT)),
            Pop::Closed => 0.,
        }
    }

    fn close_popover(&mut self) {
        if let Pop::Open(_) = self.popover {
            self.popover = Pop::Closing(Instant::now());
        }
    }

    /// Animates the rows a fold hid or showed, given the rows from before it.
    fn start_fold(&mut self, id: SpanId, old: Vec<Row>) {
        let Some(doc) = &self.doc else { return };
        if old.len() != doc.rows.len() {
            self.fold_anim = Some(FoldAnim { id, start: Instant::now(), rows: present::fold_merge(&old, &doc.rows) });
        }
    }

    fn sidebar_target(&mut self, vw: f32) -> f32 {
        let wide = vw >= SIDEBAR_AUTO_BELOW;
        if wide && self.sidebar_pref == Some(true) {
            self.sidebar_pref = None;
        }
        if self.sidebar_pref.unwrap_or(wide) { SIDEBAR_W } else { 0. }
    }

    /// Current sidebar width, and whether it is still animating.
    fn sidebar_w(&mut self, vw: f32) -> (f32, bool) {
        let target = self.sidebar_target(vw);
        let (from, to, t0) = self.sidebar_anim;
        let t = t0.elapsed().as_secs_f32() / SIDEBAR_ANIM.as_secs_f32();
        let cur = from + (to - from) * ease(t);
        if target != to {
            self.sidebar_anim = (cur, target, Instant::now());
            return (cur, true);
        }
        (cur, t < 1.)
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_pref = Some(self.sidebar_anim.1 == 0.);
    }

    fn toggle_fold(&mut self, id: &SpanId) {
        let Some(doc) = &mut self.doc else { return };
        let old = doc.rows.clone();
        if !doc.collapsed.remove(id) {
            doc.collapsed.insert(id.clone());
        }
        doc.refresh();
        self.start_fold(id.clone(), old);
    }

    /// Shows `[a, b]` across the timeline, which stops following.
    fn set_window(&mut self, a: Ns, b: Ns, tl_w: f32) {
        self.zoomed = true;
        let Some(doc) = &mut self.doc else { return };
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        doc.viewport = Some(Viewport::fit(a, b.max(a + 1), tl_w as u16));
    }

    fn lens_down(&mut self, x: f32, g: Geo) {
        let Some(vp) = self.doc.as_ref().and_then(|d| d.viewport) else { return };
        let (a, b) = (vp.start_ns, vp.end_ns(g.tl_w as u16));
        let t = g.ov_t(x);
        self.lens_drag = Some(match present::lens_grab(x, present::lens_rect(g.ov_x(a), g.ov_x(b), g.tl_w)) {
            Grab::Start => LensDrag::Start,
            Grab::End => LensDrag::End,
            Grab::Inside => LensDrag::Move { grab_ns: t - a },
            Grab::Outside => LensDrag::Move { grab_ns: (b - a) / 2 },
        });
        self.lens_move(x, g);
    }

    fn lens_move(&mut self, x: f32, g: Geo) {
        let (Some(drag), Some(vp)) = (self.lens_drag, self.doc.as_ref().and_then(|d| d.viewport)) else { return };
        let (a, b) = (vp.start_ns, vp.end_ns(g.tl_w as u16));
        let t = g.ov_t(x);
        let min = ((g.overview.1 - g.overview.0) / 2000).max(1_000_000);
        let (a, b) = match drag {
            LensDrag::Move { grab_ns } => (t - grab_ns, t - grab_ns + (b - a)),
            LensDrag::Start => (t.min(b - min), b),
            LensDrag::End => (a, t.max(a + min)),
        };
        self.set_window(a, b, g.tl_w);
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

    /// Back to following now at the fitted scale.
    fn jump_to_now(&mut self) {
        if let Some(TraceDoc { mode: Mode::Live { following, .. }, .. }) = &mut self.doc {
            *following = true;
            self.zoomed = false;
        }
    }

    /// Brings an off-screen span into view at the current scale, or fits it when it is longer than the view.
    fn pan_to_span(&mut self, id: &SpanId, tl_w: f32) {
        let Some(doc) = &self.doc else { return };
        let (Some(s), Some(vp)) = (doc.state.spans.get(id), doc.viewport) else { return };
        let end = s.effective_end(doc.state.session.ended_at_utc_ns, doc.now);
        let len = vp.end_ns(tl_w as u16) - vp.start_ns;
        if (end - s.started_at_utc_ns) as f64 > len as f64 * 0.8 {
            self.zoom_to_span(id);
            return;
        }
        let a = s.started_at_utc_ns - (len - (end - s.started_at_utc_ns)) / 2;
        self.set_window(a, a + len, tl_w);
    }

    fn copy_command(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(COLLECTOR_CMD.into()));
        self.copied_at = Some(Instant::now());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
        cx.notify();
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
            self.list.scroll_to_item(doc.selected_row, ScrollStrategy::Nearest);
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
        if ks.modifiers.platform && (ks.key == "\\" || (ks.modifiers.alt && ks.key == "s")) {
            self.toggle_sidebar();
            cx.notify();
            return;
        }
        if ks.modifiers.platform && ks.key == "c" && self.doc.is_none() {
            self.copy_command(cx);
            return;
        }
        if ks.modifiers.platform || ks.modifiers.control {
            return;
        }
        if key == "escape" && matches!(self.popover, Pop::Open(_)) {
            self.close_popover();
            cx.notify();
            return;
        }
        let Some(doc) = &mut self.doc else { return };
        let before = matches!(key.as_str(), "h" | "left" | "l" | "right" | "e").then(|| (doc.rows.clone(), doc.selected.clone()));
        match key.as_str() {
            "j" | "down" => doc.move_by(1),
            "k" | "up" => doc.move_by(-1),
            "h" | "left" => doc.left(),
            "l" | "right" => doc.right(),
            "g" | "home" => doc.select(0),
            "G" | "end" => doc.select(usize::MAX),
            "enter" if doc.selected.is_some() => self.popover = Pop::Open(Instant::now()),
            "escape" if !doc.search.is_empty() => {
                self.span_search.update(cx, |s, cx| s.set_value("", window, cx));
                self.span_search_open = false;
            }
            "e" => self.error = doc.toggle_descendants(),
            "+" | "=" | "-" => {
                doc.zoom(if key == "-" { 2.0 } else { 0.5 });
                if let Mode::Live { following, .. } = &mut doc.mode {
                    *following = false;
                }
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
        if let Some((old, Some(id))) = before {
            self.start_fold(id, old);
        }
        if let Some(doc) = &self.doc {
            self.list.scroll_to_item(doc.selected_row, ScrollStrategy::Nearest);
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
        let built = Instant::now();
        self.bench_fold();
        let vw = window.viewport_size().width.to_f64() as f32;
        let (sw, sliding) = self.sidebar_w(vw);
        if self.tick() || sliding || self.bench.is_some() {
            window.request_animation_frame();
        }
        let pal = Palette::new(self.dark(window));
        let main_pl = MAIN_PAD * (1. - sw / SIDEBAR_W);
        let card_left = sw + main_pl;
        let root = div()
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
            .child(div().w(px(sw)).flex_shrink_0().h_full().overflow_hidden().child(div().ml(px(sw - SIDEBAR_W)).h_full().child(self.render_sidebar(&pal, cx))))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .pl(px(main_pl))
                    .pr(px(MAIN_PAD))
                    .pb(px(MAIN_PAD))
                    .child(self.render_header(&pal, (TOGGLE_X + 28. + 10. - card_left - 6.).max(0.), window, cx))
                    .child(match self.doc.is_some() {
                        true => self.render_card(&pal, vw, card_left, window, cx).into_any_element(),
                        false => self.render_empty(&pal, cx).into_any_element(),
                    }),
            )
            .child(self.render_sidebar_toggle(&pal, cx))
            .when_some(self.error.clone(), |el, e| el.child(toast(&pal, e, card_left, cx)));
        self.bench_step(built, cx);
        root
    }
}

fn toast(pal: &Palette, msg: String, left: f32, cx: &mut Context<TraceApp>) -> impl IntoElement {
    div().absolute().bottom(px(24.)).left(px(left + 12.)).child(
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
        let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
        // We never store prompts, so what tells sessions apart is how they ran.
        let turns = match s.turn_count {
            0 => None,
            1 => Some("1 turn".to_owned()),
            n => Some(format!("{n} turns")),
        };
        let how = join(vec![Some(s.harness.clone()).filter(|h| !h.is_empty()), s.model.as_deref().map(present::pretty_model), turns]);
        let meta = join(vec![when, dur, s.totals.cost_usd_micros().map(|c| format_cost(Some(c)))]);
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
                    .when(!how.is_empty(), |el| el.child(div().truncate().text_size(px(11.5)).text_color(pal.text).child(how)))
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

/// A status capsule: card fill, line ring, soft shadow, with the status color only on the dot and text.
fn pill(pal: &Palette, color: Hsla, label: String, pulse: bool, id: impl Into<ElementId>, h: f32) -> impl IntoElement {
    div()
        .h(px(h))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(10.))
        .rounded_full()
        .bg(pal.card)
        .border_1()
        .border_color(pal.line)
        .shadow(vec![soft_shadow(pal, 0.05, 1., 2.)])
        .text_color(color)
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .font_features(tnum())
        .child(status_dot(color, pulse, id))
        .child(label)
}

// ── Header ─────────────────────────────────────────────────────────────────

impl TraceApp {
    fn render_sidebar_toggle(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-toggle")
            .absolute()
            .left(px(TOGGLE_X))
            .top(px((HEADER_H - 28.) / 2.))
            .size(px(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(pal.hover.opacity(4.)))
            .child(Icon::new(IconName::PanelLeft).size(px(16.)).text_color(pal.text))
            .tooltip(|w, cx| Tooltip::new("Toggle sidebar  ⌘⌥S").build(w, cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.toggle_sidebar();
                cx.notify();
            }))
    }

    fn bench_fold(&mut self) {
        // The biggest subtree near the top, folded and unfolded every 18 frames (about 300ms at 60fps).
        // Runs before the frame builds, so the list never lays out rows its count no longer matches.
        let Some(b) = &mut self.bench else { return };
        let frame = b.intervals.len();
        let (Some(target), Some(doc)) = (&mut b.fold, &self.doc) else { return };
        let id = target.get_or_insert_with(|| {
            let size = |i: usize| doc.rows[i + 1..].iter().take_while(|r| r.depth > doc.rows[i].depth).count();
            let i = (0..doc.rows.len().min(30)).max_by_key(|&i| size(i)).unwrap_or(0);
            match &doc.rows[i].kind {
                RowKind::Span(id) => id.clone(),
                RowKind::UnresolvedHeader => SpanId::default(),
            }
        });
        if frame % 18 == 1 {
            let id = id.clone();
            self.toggle_fold(&id);
        }
    }

    fn bench_step(&mut self, built: Instant, cx: &mut Context<Self>) {
        let Some(b) = &mut self.bench else { return };
        let now = Instant::now();
        b.builds.push((now - built).as_secs_f32() * 1e3);
        b.intervals.push((now - b.last).as_secs_f32() * 1e3);
        b.last = now;
        let frame = b.intervals.len();
        if frame > BENCH_FRAMES {
            let stats = |v: &mut Vec<f32>| {
                v.sort_by(f32::total_cmp);
                let at = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
                format!("p50 {:.2}ms p95 {:.2}ms max {:.2}ms", at(0.5), at(0.95), at(1.))
            };
            let mut iv = b.intervals[10..].to_vec();
            let mut bd = b.builds[10..].to_vec();
            let mut rw = b.rows[10.min(b.rows.len())..].to_vec();
            let name = if b.fold.is_some() { "bench-fold" } else { "bench-scroll" };
            println!("{name}: {} frames, rows {}", iv.len(), self.doc.as_ref().map_or(0, |d| d.rows.len()));
            println!("frame interval {}", stats(&mut iv));
            println!("render build   {}", stats(&mut bd));
            println!("rows build     {}", stats(&mut rw));
            cx.quit();
            return;
        }
        if b.fold.is_some() {
            return;
        }
        let rows = self.doc.as_ref().map_or(0, |d| d.rows.len()) as f32;
        let span = (rows * ROW_H - 600.).clamp(1., 1200.);
        self.list.0.borrow().base_handle.set_offset(point(px(0.), px(-((frame as f32 * 37.) % span))));
        self.pan_px(if frame % 60 < 30 { 4. } else { -4. });
    }

    fn render_header(&self, pal: &Palette, inset: f32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            let (color, label, pulse) = match &doc.mode {
                Mode::Live { paused_at: Some(_), .. } => (pal.text, "Paused".to_owned(), false),
                Mode::Live { .. } => (pal.accent, "Live".to_owned(), true),
                Mode::Replay { replay, clock } => (pal.accent, format!("Replay · {}", clock_time(clock.position - replay.start().unwrap_or(0))), false),
                Mode::Static => match s.status {
                    SessionStatus::Failed => (pal.error.ink, "Failed".to_owned(), false),
                    SessionStatus::Interrupted => (pal.slow.fg, "Interrupted".to_owned(), false),
                    SessionStatus::Unknown => (pal.text, "Unknown".to_owned(), false),
                    _ => (pal.ok.fg, "Done".to_owned(), false),
                },
            };
            right = right.child(pill(pal, color, label, pulse, "hdot", 28.));
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
            .pl(px(6. + inset))
            .pr(px(6.))
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
    fn geo(&mut self, vw: f32, card_left: f32) -> Option<Geo> {
        let doc = self.doc.as_mut()?;
        let card_w = vw - card_left - MAIN_PAD - 2.;
        let name_w = (card_w * 0.3).clamp(230., 300.);
        let tl_w = (card_w - name_w - TL_PAD_R).max(40.);
        let overview = match &doc.mode {
            Mode::Replay { replay, .. } => (replay.start().unwrap_or(0), replay.end().unwrap_or(0)),
            _ => (doc.state.axis_start().unwrap_or(doc.now), doc.now),
        };
        if doc.viewport.is_none() {
            if let Some((a, b)) = self.initial_window.take() {
                self.zoomed = true;
                if let Mode::Live { following, .. } = &mut doc.mode {
                    *following = false;
                }
                let at = |s: f64| overview.0 + (s * 1e9) as Ns;
                doc.viewport = Some(Viewport::fit(at(a), at(b), tl_w as u16));
            } else {
                let fit_w = if matches!(doc.mode, Mode::Live { .. }) { tl_w } else { tl_w * 0.86 };
                let fit = doc.fit(fit_w as u16);
                let focus = if matches!(doc.mode, Mode::Replay { .. }) { None } else { focus_window(doc, fit.ns_per_cell, fit_w) };
                doc.viewport = Some(focus.as_ref().map_or(fit, |f| f.1));
                // Row 0 is the default selection; anything else came from `--select`.
                if let Some((id, _)) = focus.filter(|_| doc.selected_row == 0)
                    && let Some(i) = doc.rows.iter().position(|r| r.kind == RowKind::Span(id.clone()))
                {
                    doc.select(i);
                    self.list.scroll_to_item(i, ScrollStrategy::Top);
                }
            }
        }
        let mut vp = doc.layout(tl_w as u16);
        if !self.zoomed && matches!(doc.mode, Mode::Live { following: true, .. }) {
            let fit_w = tl_w * 0.9;
            let fit = Viewport::fit(overview.0, doc.now, fit_w as u16);
            vp = focus_window(doc, fit.ns_per_cell, fit_w).map_or(fit, |f| f.1);
            doc.viewport = Some(vp);
        }
        let origin = overview.0;
        Some(Geo { left: card_left + 1., name_w, tl_w, vp, now: doc.now, origin, overview })
    }

    fn render_card(&mut self, pal: &Palette, vw: f32, card_left: f32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.geo(vw, card_left).expect("render_card runs with a document");
        let folding = self.render_fold(g, window, cx);
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
                if dx.abs() > dy.abs() {
                    this.pan_px(-dx);
                    cx.notify();
                }
            }))
            .child(canvas(|_, _, _| {}, {
                let this = cx.entity().downgrade();
                move |bounds, _, window, _| {
                    window.on_mouse_event(move |ev: &ScrollWheelEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture || !ev.modifiers.platform || !bounds.contains(&ev.position) {
                            return;
                        }
                        let dy = ev.delta.pixel_delta(px(ROW_H)).y.to_f64() as f32;
                        let x = ev.position.x.to_f64() as f32 - g.left - g.name_w;
                        this.update(cx, |this, cx| {
                            this.zoom_at(f64::from((1.0 - dy * 0.01).clamp(0.5, 2.0)), x);
                            cx.notify();
                        })
                        .ok();
                        cx.stop_propagation();
                    });
                }
            })
            .absolute()
            .inset_0())
            .on_pinch(cx.listener(move |this, ev: &PinchEvent, _, cx| {
                let x = ev.position.x.to_f64() as f32 - g.left - g.name_w;
                this.zoom_at(f64::from(1.0 / (1.0 + ev.delta).max(0.2)), x);
                cx.notify();
            }))
            .child(grid)
            .when(show_now && (0.0..=g.tl_w).contains(&now_x), |el| {
                el.child(div().absolute().top_0().bottom_0().left(px(g.name_w + now_x.round())).w(px(1.)).bg(pal.now))
            })
            .child(match folding {
                Some(stack) => stack.into_any_element(),
                None => uniform_list(
                    "rows",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        let t0 = Instant::now();
                        let rows = range
                            .map(|ix| {
                                let row = this.doc.as_ref().expect("rows need a document").rows[ix].clone();
                                this.render_row(ix, &row, ix, g, window, cx)
                            })
                            .collect::<Vec<_>>();
                        if let Some(b) = &mut this.bench {
                            b.rows.push(t0.elapsed().as_secs_f32() * 1e3);
                        }
                        rows
                    }),
                )
                .track_scroll(&self.list)
                .size_full()
                .pb(px(if matches!(doc.mode, Mode::Replay { .. }) { 72. } else { 8. }))
                .into_any_element(),
            })
            .child(gpui_kit::component::scroll::Scrollbar::vertical(&self.list))
            .when(matches!(doc.mode, Mode::Live { .. }) && !(0.0..=g.tl_w).contains(&now_x), |el| {
                el.child(
                    div()
                        .id("jump-now")
                        .absolute()
                        .top(px(8.))
                        .right(px(14.))
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .pl(px(12.))
                        .pr(px(8.))
                        .rounded_full()
                        .bg(pal.card)
                        .border_1()
                        .border_color(pal.line)
                        .shadow(vec![soft_shadow(pal, 0.10, 4., 12.), soft_shadow(pal, 0.05, 1., 2.)])
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(pal.accent)
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.wash))
                        .child("Jump to now")
                        .child(Icon::new(IconName::ChevronRight).size(px(14.)).text_color(pal.accent))
                        .tooltip(|w, cx| Tooltip::new("Follow now again  F").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_to_now();
                            cx.notify();
                        })),
                )
            });
        let stats = self.render_stats(pal);
        let overview = self.render_overview(pal, g, cx);
        let transport = matches!(doc.mode, Mode::Replay { .. }).then(|| self.render_transport(pal, &g, cx).into_any_element());
        let vh = window.viewport_size().height.to_f64() as f32;
        let card = (point(card_left, HEADER_H), size(vw - card_left - MAIN_PAD, vh - HEADER_H - MAIN_PAD));
        let popover = self.render_popover(pal, g, card, window, cx);
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
            .child(overview)
            .child(axis)
            .child(body)
            .when(show_now && (0.0..=g.tl_w).contains(&now_x), |el| {
                let x = g.name_w + now_x.round();
                let top = STATS_H + OVERVIEW_H + AXIS_H;
                el.child(div().absolute().top(px(top - 3.)).left(px(x - 3. + 0.5)).size(px(6.)).rounded_full().bg(pal.accent))
                    .child(div().absolute().top(px(top - 10.)).bottom(px(0.)).left(px(x)).w(px(1.)).bg(pal.now).h(px(10.)))
            })
            .children(transport)
            .children(popover)
    }

    /// While a fold animates, the visible rows as a stack whose hidden or shown rows grow or shrink
    /// with the fold. Only rows in view are built; the uniform list returns when the fold settles.
    fn render_fold(&mut self, g: Geo, window: &mut Window, cx: &mut Context<Self>) -> Option<Div> {
        let f = self.fold_anim.as_ref()?;
        let p = ease(self.progress(f.start, FOLD_ANIM));
        let top = -self.list.0.borrow().base_handle.offset().y.to_f64() as f32;
        let bottom = top + window.viewport_size().height.to_f64() as f32;
        let mut visible = Vec::new();
        let (mut y, mut next) = (0., 0usize);
        for (k, (row, change)) in f.rows.iter().enumerate() {
            if y >= bottom {
                break;
            }
            let h = ROW_H * change.scale(p);
            // A row on its way out answers clicks as the row above it, its parent.
            let ix = if *change == Change::Leave { next.saturating_sub(1) } else { next };
            if y + h > top && h >= 0.5 {
                visible.push((ix, k, row.clone(), *change, y, h));
            }
            next += usize::from(*change != Change::Leave);
            y += h;
        }
        let rows: Vec<_> = visible
            .into_iter()
            .map(|(ix, k, row, change, y, h)| {
                let key = if change == Change::Leave { usize::MAX - k } else { ix };
                div().absolute().left_0().right_0().top(px(y - top)).h(px(h)).overflow_hidden().opacity(change.scale(p)).child(self.render_row(ix, &row, key, g, window, cx))
            })
            .collect();
        Some(div().size_full().relative().overflow_hidden().children(rows))
    }

    /// The whole session as a density strip, with the visible window as a lens: click jumps, drag moves,
    /// dragging an edge zooms.
    fn render_overview(&self, pal: &Palette, g: Geo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const LANE: f32 = 3.;
        const LANE_GAP: f32 = 2.;
        let doc = self.doc.as_ref().expect("overview needs a document");
        let session_end = doc.state.session.ended_at_utc_ns;
        let cols = g.tl_w.ceil().max(1.) as usize;
        // One thin lane per tone (wait in amber, LLM, tool, error).
        let spans = doc.state.spans.values().filter(|s| matches!(s.kind, SpanKind::Llm | SpanKind::Tool | SpanKind::Wait) || s.status == SpanStatus::Failed);
        let alpha = present::density(spans.map(|s| (g.ov_x(s.started_at_utc_ns), g.ov_x(s.effective_end(session_end, g.now)), present::tone(s) as usize)), cols);
        let tones = [Tone::Agent, Tone::Llm, Tone::Tool, Tone::Error];
        let colors = tones.map(|t| if t == Tone::Agent { pal.amber } else { pal.kind(t).mark });
        let marks: Vec<(f32, f32, Hsla)> = alpha
            .iter()
            .enumerate()
            .flat_map(|(i, a)| (0..4).filter(|&k| a[k] > 0.).map(move |k| (i, k, a[k])))
            .map(|(i, k, a)| (i as f32, k as f32 * (LANE + LANE_GAP), colors[k].opacity(a)))
            .collect();
        let (a, b) = g.window();
        let (lx0, lx1) = present::lens_rect(g.ov_x(a), g.ov_x(b), g.tl_w);
        let thin = g.ov_x(b) - g.ov_x(a) < present::LENS_MIN;
        let this = cx.entity().downgrade();
        let strip = canvas(|_, _, _| {}, move |bounds, _, window, _| {
            let top = bounds.origin.y + (bounds.size.height - px(4. * LANE + 3. * LANE_GAP)) / 2.;
            for (x, y, color) in &marks {
                window.paint_quad(fill(Bounds::new(point(bounds.origin.x + px(*x), top + px(*y)), size(px(1.), px(LANE))), *color));
            }
            let local = move |p: Point<Pixels>| (p.x - bounds.origin.x).to_f64() as f32;
            let down = this.clone();
            window.on_mouse_event(move |ev: &MouseDownEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && bounds.contains(&ev.position) {
                    down.update(cx, |this, cx| {
                        this.lens_down(local(ev.position), g);
                        cx.notify();
                    })
                    .ok();
                }
            });
            let drag = this.clone();
            window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble && ev.pressed_button == Some(MouseButton::Left) {
                    drag.update(cx, |this, cx| {
                        if this.lens_drag.is_some() {
                            this.lens_move(local(ev.position), g);
                            cx.notify();
                        }
                    })
                    .ok();
                }
            });
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    this.update(cx, |this, _| this.lens_drag = None).ok();
                }
            });
        });
        let grip = || div().w(px(2.)).h(px(10.)).rounded_full().bg(pal.faint);
        let whole = a <= g.overview.0 && b >= g.overview.1;
        let tl_w = g.tl_w;
        div()
            .h(px(OVERVIEW_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(pal.line)
            .child(
                div()
                    .w(px(g.name_w))
                    .flex_shrink_0()
                    .pl(px(16.))
                    .pr(px(12.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .whitespace_nowrap()
                    .text_size(px(12.))
                    .font_features(tnum())
                    .child(div().flex_1().min_w(px(0.)).truncate().text_color(pal.text).font_weight(FontWeight::MEDIUM).child(present::overview_label((a, b), g.overview)))
                    .when(!whole, |el| {
                        el.child(
                            div()
                                .id("show-all")
                                .flex_shrink_0()
                                .h(px(22.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .rounded(px(6.))
                                .border_1()
                                .border_color(pal.line)
                                .bg(pal.card)
                                .text_size(px(11.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(pal.text)
                                .cursor_pointer()
                                .hover(|s| s.bg(pal.wash))
                                .child("Show all")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let (a, b) = g.overview;
                                    this.set_window(a, b, tl_w);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .id("overview")
                    .w(px(g.tl_w))
                    .h(px(STRIP_H))
                    .relative()
                    .rounded(px(7.))
                    .bg(pal.wash)
                    .cursor_pointer()
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .bottom(px(2.))
                            .left(px(lx0))
                            .w(px(lx1 - lx0))
                            .rounded(px(6.))
                            .bg(pal.card)
                            .when(thin, |el| el.border_2().border_color(pal.accent))
                            .when(!thin, |el| el.border_1().border_color(pal.faint.opacity(if pal.dark { 1. } else { 0.7 })))
                            .shadow(vec![soft_shadow(pal, 0.10, 1., 3.)])
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(2.))
                            .when(!thin, |el| el.child(grip()).child(grip())),
                    )
                    .child(strip.absolute().inset_0()),
            )
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

    /// `ix` is the row's place in the document, for selection and hover; `key` names its elements.
    fn render_row(&mut self, ix: usize, row: &Row, key: usize, g: Geo, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = Palette::new(self.dark(window));
        let doc = self.doc.as_ref().expect("rows need a document");
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
        let hovered = key == ix && self.hovered == Some(ix);
        let live = !matches!(doc.mode, Mode::Static);
        let running = live && s.ended_at_utc_ns.is_none() && s.status == SpanStatus::Running;
        let tone = present::tone(s);
        let kc = pal.kind(tone);
        let (icon, ink) = present::glyph(s);
        let detail = present::detail(s, &doc.state.session.harness);
        let indent = 12. + f32::from(row.depth) * INDENT;
        let end = s.effective_end(doc.state.session.ended_at_utc_ns, g.now);
        // Two pixels in from each edge, so a span that would draw as a sliver reads as off-screen.
        let edge_ns = (2. * g.vp.ns_per_cell) as Ns;
        let (wa, wb) = g.window();
        let hint = present::offscreen_hint(s.started_at_utc_ns, end, (wa + edge_ns, wb - edge_ns));
        let full_title = present::title(s);
        // About 7pt per character at 13pt; the title gets at most 80% of what the name column leaves.
        let room = (g.name_w - indent - 14. - 20. - 12. - 10.) * 0.8;
        let shown_title = present::middle_truncate(&full_title, (room / 7.) as usize);
        let truncated = shown_title != full_title || present::mcp_parts(&s.name).is_some();
        let tip = if present::mcp_parts(&s.name).is_some() { s.name.clone() } else { full_title.clone() };
        let row_bg = if selected {
            pal.card.blend(pal.selected)
        } else if hovered {
            pal.card.blend(pal.hover)
        } else {
            pal.card
        };
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
                .id(("chev", key))
                .size(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(row.has_children, |el| {
                    let progress = match &self.fold_anim {
                        Some(f) if f.id == *id => ease(self.progress(f.start, FOLD_ANIM)),
                        _ => 1.,
                    };
                    let folded = if row.collapsed { progress } else { 1. - progress };
                    el.cursor_pointer()
                        .child(Icon::new(IconName::ChevronDown).size(px(12.)).text_color(pal.muted).rotate(radians(-std::f32::consts::FRAC_PI_2 * folded)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_fold(&chevron_id);
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
                            .id(("title", key))
                            .flex_shrink_0()
                            .max_w(relative(0.8))
                            .truncate()
                            .text_size(px(13.))
                            .when(row.has_children, |el| el.font_weight(FontWeight::MEDIUM))
                            .text_color(if hint.is_some() { pal.text } else { pal.ink })
                            .child(shown_title)
                            .when(truncated, |el| el.tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))),
                    )
                    .when(row.collapsed, |el| {
                        el.child(
                            div()
                                .flex_shrink_0()
                                .h(px(18.))
                                .px(px(7.))
                                .flex()
                                .items_center()
                                .rounded_full()
                                .bg(pal.chip)
                                .text_size(px(11.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(pal.muted)
                                .font_features(tnum())
                                .child(format!("+{}", hidden.len())),
                        )
                    })
                    .when_some(detail, |el, d| el.child(div().flex_shrink(8.).min_w(px(0.)).max_w(px(160.)).truncate().text_size(px(12.5)).text_color(pal.muted).child(d)))
                    .when(running, |el| el.child(Spinner::new().color(pal.accent).with_size(px(12.))))
                    .when(s.status == SpanStatus::Failed, |el| el.child(Icon::new(IconName::TriangleAlert).size(px(13.)).text_color(pal.error.ink))),
            );

        let timeline = match hint {
            Some((edge, text)) => self.render_edge_hint(key, id.clone(), edge, text, pal.kind(tone).mark, &pal, g, cx).into_any_element(),
            None => self.render_bar(key, s, &g, kc, &pal, hovered, slowest, &hidden, row_bg, window).into_any_element(),
        };
        let click_id = id.clone();
        let el = div()
            .id(("row", key))
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
                    this.popover = Pop::Open(Instant::now());
                }
                this.focus.focus(window, cx);
                cx.notify();
            }));
        el.into_any_element()
    }

    /// A span outside the visible window: a chevron at the edge it lies past and how far away it is. Clicking pans to it.
    #[allow(clippy::too_many_arguments)]
    fn render_edge_hint(&self, key: usize, id: SpanId, edge: Edge, text: String, mark: Hsla, pal: &Palette, g: Geo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let left = edge == Edge::Left;
        let chevron = Icon::new(if left { IconName::ChevronLeft } else { IconName::ChevronRight }).size(px(12.)).text_color(mark);
        let label = div().text_size(px(11.)).text_color(pal.muted).font_features(tnum()).whitespace_nowrap().child(text);
        div().w(px(g.tl_w)).h_full().relative().child(
            div()
                .id(("edge", key))
                .absolute()
                .top(px(7.))
                .h(px(18.))
                .when(left, |el| el.left(px(2.)))
                .when(!left, |el| el.right(px(2.)))
                .flex()
                .items_center()
                .gap(px(2.))
                .px(px(4.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(pal.hover.opacity(3.)))
                .map(|el| if left { el.child(chevron).child(label) } else { el.child(label).child(chevron) })
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.pan_to_span(&id, g.tl_w);
                    cx.notify();
                })),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn render_bar(&self, ix: usize, s: &Span, g: &Geo, kc: KindColors, pal: &Palette, hovered: bool, slowest: bool, hidden: &[&Span], row_bg: Hsla, window: &mut Window) -> impl IntoElement + use<> {
        const FADE: f32 = 12.;
        let doc = self.doc.as_ref().expect("bars need a document");
        let session_end = doc.state.session.ended_at_utc_ns;
        let end = s.effective_end(session_end, g.now);
        let open = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        let x0 = g.x(s.started_at_utc_ns);
        let x1 = g.x(end);
        let reveal = self.seen.get(&s.span_id).map_or(1., |t| ease(t.elapsed().as_secs_f32() / REVEAL.as_secs_f32()));
        let x1 = x0 + (x1 - x0) * reveal;
        let mut cell = div().w(px(g.tl_w)).h_full().relative().overflow_hidden();
        // A bar cut by an edge squares off there and fades in from the row, so a clipped span never reads as its own end.
        let (clip_l, clip_r) = (x0 < 0., x1 > g.tl_w);
        let bx = x0.max(0.).round();
        let bw = (x1.min(g.tl_w) - bx).max(3.).round();
        let top = (ROW_H - BAR_H) / 2.;
        let fade = |left: bool| {
            let w = FADE.min(bw / 2.);
            let (from, to) = if left { (row_bg, row_bg.opacity(0.)) } else { (row_bg.opacity(0.), row_bg) };
            div().absolute().top(px(-1.)).bottom(px(-1.)).w(px(w)).when(left, |el| el.left(px(-1.))).when(!left, |el| el.right(px(-1.))).bg(linear_gradient(90., linear_color_stop(from, 0.), linear_color_stop(to, 1.)))
        };
        let mut bar = div()
            .absolute()
            .left(px(bx))
            .top(px(top))
            .w(px(bw))
            .h(px(BAR_H))
            .rounded(px(5.))
            .when(clip_l, |el| el.rounded_l(px(0.)))
            .when(clip_r, |el| el.rounded_r(px(0.)))
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
        bar = bar.when(clip_l, |el| el.child(fade(true))).when(clip_r && !open, |el| el.child(fade(false)));
        cell = cell.child(bar);
        let label = present::bar_label(s, &doc.state.subtree_totals(&s.span_id), session_end, g.now);
        let lw = text_width(window, &label, LABEL_SIZE);
        let tag_w = if slowest { 58. } else { 0. };
        let at = present::label_at(bx, bw, lw + tag_w, g.tl_w);
        let lx = match at {
            LabelAt::After => bx + bw + present::LABEL_GAP,
            LabelAt::Inside => bx + bw - 8. - lw,
            LabelAt::Before => bx - present::LABEL_GAP - lw - tag_w,
        };
        let inside = at == LabelAt::Inside;
        let color = if inside { kc.ink } else { pal.muted };
        // Outside the bar the label sits on a knockout in the row's color, so grid and now lines pass beneath it.
        cell = cell.child(
            div()
                .absolute()
                .left(px(if inside { lx } else { lx - 2. }))
                .when(!inside, |el| el.child(div().absolute().left_0().right_0().top(px(8.)).bottom(px(8.)).rounded(px(3.)).bg(row_bg)))
                .px(px(if inside { 0. } else { 2. }))
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

    /// The selected span's popover, anchored under its bar in window coordinates. `card` is the card's
    /// window origin and size.
    fn render_popover(&self, pal: &Palette, g: Geo, card: (Point<f32>, Size<f32>), window: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if matches!(self.popover, Pop::Closed) {
            return None;
        }
        let doc = self.doc.as_ref()?;
        let s = doc.selected_span()?;
        let end = s.effective_end(doc.state.session.ended_at_utc_ns, g.now);
        let body_top = 1. + STATS_H + OVERVIEW_H + AXIS_H;
        let scroll = self.list.0.borrow().base_handle.offset().y.to_f64() as f32;
        let row_y = (body_top + doc.selected_row as f32 * ROW_H + scroll).clamp(body_top - ROW_H, card.1.height);
        let bar_y = (row_y + (ROW_H - BAR_H) / 2., row_y + (ROW_H + BAR_H) / 2.);
        let bx = |t: Ns| 1. + g.name_w + g.x(t).clamp(0., g.tl_w);
        let full_h = self.pop_h.get();
        if full_h == 0. {
            window.request_animation_frame();
        }
        let natural = if full_h > 0. { full_h + 2. } else { 0. };
        let place = present::place_popover((bx(s.started_at_utc_ns), bx(end)), bar_y, (card.1.width, card.1.height), (POPOVER_W, natural));
        let (icon, ink) = present::glyph(s);
        let sub = [Some(present::kind_label(s.kind).to_owned()), s.model.as_deref().map(present::pretty_model).or_else(|| present::detail(s, &doc.state.session.harness))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let open = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        let (status, label) = match s.status {
            SpanStatus::Failed => (pal.error.ink, "Failed"),
            SpanStatus::Succeeded => (pal.ok.fg, "Done"),
            SpanStatus::Cancelled => (pal.text, "Cancelled"),
            SpanStatus::Running if open => (pal.accent, "Running"),
            _ => (pal.text, "Unknown"),
        };
        let stats = present::popover_stats(&doc.state, s, &doc.state.subtree_totals(&s.span_id), doc.now);
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
                    None => div().text_size(px(12.5)).text_color(pal.muted).child("Not captured"),
                })
        };
        let start = s.started_at_utc_ns;
        let shown = self.pop_shown();
        let scale = 0.96 + 0.04 * shown;
        let inset = POPOVER_W * (1. - scale) / 2.;
        let full_h = self.pop_h.get();
        let measure = self.pop_h.clone();
        let content = div()
            .relative()
            .w(px(POPOVER_W))
            .ml(px(-inset))
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(16.))
            .child(canvas(move |b, _, _| measure.set(b.size.height.to_f64() as f32), |_, _, _, _| {}).absolute().inset_0())
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
                    .child(pill(pal, status, label.to_owned(), open, "pdot", 24.))
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
                                this.close_popover();
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
            });
        // GPUI has no element scale, so the card's bounds shrink toward the bar while the content holds
        // still underneath, clipped.
        let final_h = natural.min(place.max_h);
        let h = final_h * scale;
        let y = if place.above { place.y + final_h - h } else { place.y };
        let arrow_x = place.arrow_x - place.x - present::POP_ARROW;
        let above = place.above;
        let (fill_c, ring) = (pal.pop, pal.line);
        let arrow = canvas(|_, _, _| {}, move |b, _, window, _| {
            let (o, w, hgt) = (b.origin, px(2. * present::POP_ARROW), px(present::POP_ARROW));
            let (tip_y, base_y) = if above { (o.y + hgt, o.y) } else { (o.y, o.y + hgt) };
            let tip = point(o.x + w / 2., tip_y);
            let mut edge = PathBuilder::stroke(px(1.));
            edge.move_to(point(o.x, base_y));
            edge.line_to(tip);
            edge.line_to(point(o.x + w, base_y));
            let mut body = PathBuilder::fill();
            let inset = if above { px(-1.) } else { px(1.) };
            body.move_to(point(o.x, base_y + inset));
            body.line_to(tip);
            body.line_to(point(o.x + w, base_y + inset));
            body.close();
            if let Ok(p) = edge.build() {
                window.paint_path(p, ring);
            }
            if let Ok(p) = body.build() {
                window.paint_path(p, fill_c);
            }
        })
        .absolute()
        .left(px(arrow_x))
        .w(px(2. * present::POP_ARROW))
        .h(px(present::POP_ARROW))
        .when(above, |el| el.bottom(px(-present::POP_ARROW + 1.)))
        .when(!above, |el| el.top(px(-present::POP_ARROW + 1.)));
        let card_el = div()
            .id("popover")
            .occlude()
            .ml(px(inset))
            .w(px(POPOVER_W * scale))
            .map(|el| if full_h > 0. { el.h(px(h)) } else { el.max_h(px(place.max_h)) })
            .overflow_hidden()
            .rounded(px(16.))
            .bg(pal.pop)
            .border_1()
            .border_color(pal.line)
            .shadow(vec![soft_shadow(pal, 0.12, 16., 40.), soft_shadow(pal, 0.06, 2., 6.)])
            .cursor_default()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.close_popover();
                cx.notify();
            }))
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(div().id("pop-scroll").size_full().overflow_y_scroll().child(content));
        Some(
            deferred(
                anchored()
                    .position_mode(AnchoredPositionMode::Window)
                    .position(point(px(card.0.x + place.x), px(card.0.y + y)))
                    .child(div().relative().w(px(POPOVER_W)).opacity(shown).child(card_el).child(arrow)),
            )
            .with_priority(1),
        )
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
        let copied = self.copied_at.is_some_and(|t| t.elapsed() < COPIED_FOR);
        // A faint, inert waterfall behind the copy, so the empty card already looks like what it will hold.
        let sample = [(Tone::Agent, 0.10, 0.62), (Tone::Llm, 0.16, 0.18), (Tone::Tool, 0.36, 0.12), (Tone::Llm, 0.50, 0.22)];
        let ghost = div().absolute().inset_0().flex().flex_col().justify_center().gap(px(16.)).opacity(0.3).children(sample.map(|(t, x, w)| {
            let k = pal.kind(t);
            div().h(px(BAR_H)).relative().child(div().absolute().left(relative(x)).w(relative(w)).h_full().rounded(px(5.)).bg(k.bg).border_1().border_color(k.edge))
        }));
        div().flex_1().relative().overflow_hidden().flex().items_center().justify_center().rounded(px(16.)).bg(pal.card).border_1().border_color(pal.line).child(ghost).child(
            div()
                .w(px(400.))
                .p(px(24.))
                .rounded(px(16.))
                .bg(pal.card.opacity(0.9))
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
                        .h(px(32.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .pl(px(12.))
                        .pr(px(4.))
                        .rounded(px(8.))
                        .bg(pal.wash)
                        .border_1()
                        .border_color(pal.line)
                        .child(div().font_family("Menlo").text_size(px(12.)).text_color(pal.ink).child(COLLECTOR_CMD))
                        .child(
                            div()
                                .id("copy-cmd")
                                .h(px(24.))
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                .px(px(8.))
                                .rounded(px(6.))
                                .text_size(px(11.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(if copied { pal.ok.fg } else { pal.text })
                                .cursor_pointer()
                                .hover(|s| s.bg(pal.card))
                                .child(Icon::new(if copied { IconName::Check } else { IconName::Copy }).size(px(13.)))
                                .child(if copied { "Copied" } else { "Copy" })
                                .tooltip(|w, cx| Tooltip::new("Copy  ⌘C").build(w, cx))
                                .on_click(cx.listener(|this, _, _, cx| this.copy_command(cx))),
                        ),
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

/// The latest turn's window when the whole-trace fit would draw bars as slivers.
fn focus_window(doc: &TraceDoc, ns_per_px: f64, fit_w: f32) -> Option<(SpanId, Viewport)> {
    let t = present::focus_turn(&doc.state, ns_per_px)?;
    let end = t.effective_end(doc.state.session.ended_at_utc_ns, doc.now);
    let pad = (end - t.started_at_utc_ns) / 25;
    Some((t.span_id.clone(), Viewport::fit(t.started_at_utc_ns - pad, end + pad, fit_w as u16)))
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
