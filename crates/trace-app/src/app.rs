use crate::Cli;
use crate::present::{self, Change, Edge, Grab, Group, LabelAt, Shape, SidebarMode, SidebarNode, Status, Tone, ease};
use crate::theme::{KindColors, Palette};
use chrono::{Local, TimeZone};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::{Icon, Sizable, Theme, ThemeMode};
use gpui_kit::prelude::*;
use gpui_kit::*;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use trace_core::{Ns, SLOWEST_DEFAULT_KINDS, SPEEDS, SessionId, SessionStatus, Span, SpanId, SpanKind, SpanStatus};
use trace_storage::{SessionFilter, SessionSummary, Store};
use trace_view::{Mode, Row, RowKind, TraceDoc, Viewport, format_cost, format_duration, format_tokens, format_total_cost, tick_times};

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
const STRIP_H: f32 = 30.;
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
/// Room at the overview's right end for the idle gap marker.
const IDLE_MARK_W: f32 = 104.;
/// The sidebar tray's opacity over the window's blurred backdrop.
const SIDEBAR_ALPHA: f32 = 0.7;
/// Key context of the focused row list, where Enter inspects and ⌘C copies the span name.
pub const ROWS_CONTEXT: &str = "TraceRows";

gpui_kit::actions!(
    agents_trace_waterfall,
    [
        Quit, About, Import, ExportSession, ToggleSidebar, ZoomIn, ZoomOut, ZoomToFit, Follow, Replay, NextEvent, PrevEvent,
        Inspect, AddNote, ZoomToSpan, ToggleBranch, CollapseAll, ExpandAll, NextActivity, PrevActivity, CopySpanName, CopySpanJson, ExportSpanJson, ShowInReplay
    ]
);

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
    /// Where open spans stop: the session end, or the last event while idle.
    end: Option<Ns>,
    /// Following with now in view, so off-screen spans read from real now.
    following: bool,
    /// The last event of an idle live session; the overview ends there and marks the gap to now.
    idle: Option<Ns>,
    live: bool,
    /// The overview's scale, idle stretches squeezed.
    squeeze: present::Squeeze,
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

    /// Width of the overview's time scale: the strip less the idle marker's room.
    fn ov_w(&self) -> f32 {
        if self.idle.is_some() { self.tl_w - IDLE_MARK_W } else { self.tl_w }
    }

    /// Overview strip x of `t`.
    fn ov_x(&self, t: Ns) -> f32 {
        self.squeeze.x(t)
    }

    fn ov_t(&self, x: f32) -> Ns {
        self.squeeze.t(x)
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

/// What a right-click in the sidebar targets, for its context menu.
#[derive(Clone)]
enum MenuTarget {
    Session(SessionId),
    /// A workspace or project header: its state key and directory.
    Folder { key: String, path: String },
}

/// The sidebar's remembered layout, saved next to the store so it survives a relaunch.
struct SidebarState {
    path: PathBuf,
    mode: SidebarMode,
    /// Keys of collapsed workspaces and projects; see `present::Workspace::key`.
    collapsed: HashSet<String>,
    /// The header last toggled and when, so its chevron can turn.
    turned: Option<(String, Instant)>,
}

impl SidebarState {
    fn load(path: PathBuf) -> Self {
        let v: serde_json::Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let mode = if v["mode"] == "workspaces" { SidebarMode::Workspaces } else { SidebarMode::Recent };
        let collapsed = v["collapsed"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(str::to_owned)).collect();
        Self { path, mode, collapsed, turned: None }
    }

    /// Best effort: a read-only directory only costs the layout on the next launch.
    fn save(&self) {
        let mut collapsed: Vec<&String> = self.collapsed.iter().collect();
        collapsed.sort();
        let mode = if self.mode == SidebarMode::Workspaces { "workspaces" } else { "recent" };
        let body = serde_json::json!({ "mode": mode, "collapsed": collapsed });
        trace_view::write_private(&self.path, body.to_string().as_bytes()).ok();
    }

    fn toggle(&mut self, key: &str) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_owned());
        }
        self.turned = Some((key.to_owned(), Instant::now()));
        self.save();
    }
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
    pop_scroll: ScrollHandle,
    /// `--popover-scroll-end`: hold the popover's body scrolled to its end.
    pop_scroll_end: bool,
    hovered: Option<usize>,
    list: UniformListScrollHandle,
    focus: FocusHandle,
    session_search: Entity<InputState>,
    span_search: Entity<InputState>,
    span_search_open: bool,
    /// The open session's span notes, kept in step with the store.
    notes: BTreeMap<SpanId, String>,
    note: Entity<TextareaState>,
    /// The span whose note the editor holds.
    note_span: Option<SpanId>,
    /// Idle stretches of the open trace, keyed by span count and overview span so they recompute only on change.
    idle_gaps: ((usize, Ns, Ns), Vec<(Ns, Ns)>),
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
    /// What the last sidebar right-click targeted, for its context menu.
    menu_target: Option<MenuTarget>,
    sidebar: SidebarState,
    /// macOS Increase Contrast.
    contrast: bool,
    /// macOS Reduce Transparency: the sidebar drops its translucency.
    solid_sidebar: bool,
    _subs: Vec<Subscription>,
}

impl TraceApp {
    pub fn new(
        store: Store,
        capture: trace_ingest::Capture,
        forced_dark: Option<bool>,
        cli: &Cli,
        state_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let session_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search sessions"));
        let span_search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter spans"));
        let note = cx.new(|cx| TextareaState::new(window, cx).placeholder("Add a note").auto_grow(2, 8));
        let subs = vec![
            cx.subscribe(&note, |this: &mut Self, state, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.save_note(state.read(cx).value().to_string());
                    cx.notify();
                }
            }),
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
            pop_scroll: ScrollHandle::new(),
            pop_scroll_end: cli.popover_scroll_end,
            hovered: None,
            list: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            session_search,
            span_search,
            span_search_open: false,
            notes: BTreeMap::new(),
            note,
            note_span: None,
            idle_gaps: ((0, 0, 0), Vec::new()),
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
            menu_target: None,
            sidebar: SidebarState::load(state_path),
            contrast: display_accessibility().0,
            solid_sidebar: display_accessibility().1,
            _subs: subs,
        };
        let w = this.sidebar_target(window.viewport_size().width.to_f64() as f32);
        this.sidebar_anim.0 = w;
        this.sidebar_anim.1 = w;
        apply_theme(this.dark(window), cx);
        if let Some(m) = &cli.sidebar_mode {
            this.sidebar.mode = if m == "workspaces" { SidebarMode::Workspaces } else { SidebarMode::Recent };
        }
        if let Some(q) = &cli.search {
            this.session_search.update(cx, |s, cx| s.set_value(q.clone(), window, cx));
        }
        this.refresh_sessions(cx);
        if !cli.collapse.is_empty() {
            let keys = folder_keys(&this.sessions, |name| cli.collapse.iter().any(|c| c == name));
            this.sidebar.collapsed.extend(keys);
        }
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
        if let Some((x, y)) = cli.context_menu {
            cx.spawn_in(window, async move |_, cx| {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                cx.update(|window, cx| {
                    let position = point(px(x as f32), px(y as f32));
                    let modifiers = Modifiers::default();
                    window.dispatch_event(PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Right, position, modifiers, click_count: 1, first_mouse: false }), cx);
                    window.dispatch_event(PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Right, position, modifiers, click_count: 1 }), cx);
                    window.refresh();
                })
                .ok();
            })
            .detach();
        }
        if let Some(n) = cli.tab {
            cx.spawn_in(window, async move |_, cx| {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                for _ in 0..n {
                    cx.update(|window, cx| {
                        window.dispatch_keystroke(Keystroke::parse("tab").expect("a valid keystroke"), cx);
                        window.refresh();
                    })
                    .ok();
                    cx.background_executor().timer(Duration::from_millis(200)).await;
                }
            })
            .detach();
        }
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

    fn palette(&self, window: &Window) -> Palette {
        Palette::new(self.dark(window), self.contrast)
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
    /// An idle live document draws no frames of its own, so this also wakes it to poll.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self.doc.as_ref().is_some_and(|d| matches!(d.mode, Mode::Live { .. })) {
            cx.notify();
        }
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
                self.notes = self.store.annotations(doc.session_id()).unwrap_or_default();
                self.note_span = None;
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
        let turning = self.sidebar.turned.as_ref().is_some_and(|(_, t)| self.progress(*t, FOLD_ANIM) < 1.);
        let Some(doc) = &mut self.doc else { return turning };
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
            Mode::Live { paused_at, .. } => paused_at.is_none() && idle(doc).is_none(),
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
        running || turning || now < self.reveal_until || self.fold_anim.is_some() || popping
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

    /// Opens the popover on the selected span with its note focused.
    fn edit_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.as_ref().is_none_or(|d| d.selected.is_none()) {
            return;
        }
        if !matches!(self.popover, Pop::Open(_)) {
            self.popover = Pop::Open(Instant::now());
        }
        self.note.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// Points the note editor at the popover's span, so it never shows another span's note.
    fn sync_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let want = match self.popover {
            Pop::Open(_) => self.doc.as_ref().and_then(|d| d.selected.clone()),
            _ => self.note_span.clone(),
        };
        if want != self.note_span {
            let text = want.as_ref().and_then(|id| self.notes.get(id)).cloned().unwrap_or_default();
            self.note_span = want;
            self.note.update(cx, |s, cx| s.set_value(text, window, cx));
        }
    }

    // ponytail: one write per keystroke; debounce if typing ever lags on a slow disk.
    fn save_note(&mut self, text: String) {
        let (Some(doc), Some(span)) = (&self.doc, &self.note_span) else { return };
        if let Err(e) = self.store.set_annotation(doc.session_id(), span, &text) {
            self.error = Some(format!("{e:#}"));
        } else if text.trim().is_empty() {
            self.notes.remove(span);
        } else {
            self.notes.insert(span.clone(), text);
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
        let grab = present::lens_grab(x, present::lens_rect(g.ov_x(a), g.ov_x(b), g.ov_w()));
        if let (Grab::Outside, Some((_, end))) = (grab, g.squeeze.band_at(x)) {
            self.skip_to(end, b - a, g.tl_w);
            return;
        }
        self.lens_drag = Some(match grab {
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

    /// Follows now again at the current scale, so now lands at the timeline's right edge.
    fn jump_to_now(&mut self) {
        if let Some(TraceDoc { mode: Mode::Live { following, .. }, .. }) = &mut self.doc {
            *following = true;
        }
    }

    /// Brings an off-screen span into view at the current scale, or fits it when it is longer than the view.
    fn pan_to_span(&mut self, id: &SpanId, tl_w: f32) {
        let Some(doc) = &self.doc else { return };
        let (Some(s), Some(vp)) = (doc.state.spans.get(id), doc.viewport) else { return };
        let end = s.effective_end(shown_end(doc), doc.now);
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

    /// Follow reads as on only while now is in view, so pressing it while now is off-screen jumps there.
    fn toggle_follow(&mut self) {
        let Some(doc) = &mut self.doc else { return };
        let visible = now_visible(doc);
        match &mut doc.mode {
            Mode::Live { following, .. } if *following && visible => *following = false,
            Mode::Live { .. } => self.jump_to_now(),
            _ => {}
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
        let end = s.effective_end(shown_end(doc), doc.now);
        let pad = ((end - s.started_at_utc_ns) / 10).max(1_000_000);
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        doc.viewport = Some(Viewport::fit(s.started_at_utc_ns - pad, end + pad, doc.timeline_width.max(1)));
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let typing = [self.span_search.read(cx).focus_handle(cx), self.session_search.read(cx).focus_handle(cx), self.note.read(cx).focus_handle(cx)]
            .iter()
            .any(|f| f.is_focused(window));
        if typing {
            if ks.key == "escape" {
                self.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }
        // Space activates a focused button; only the row list takes it as play or pause.
        if ks.key == "space" && self.doc.is_some() && !self.focus.is_focused(window) {
            return;
        }
        let named = ["up", "down", "left", "right", "enter", "escape", "space", "home", "end"];
        let key = if named.contains(&ks.key.as_str()) { ks.key.clone() } else { ks.key_char.clone().unwrap_or_else(|| ks.key.clone()) };
        if ks.modifiers.platform && ks.key == "f" {
            self.open_span_search(window, cx);
            return;
        }
        if ks.modifiers.platform && ks.key == "\\" {
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
            "escape" if !doc.search.is_empty() => {
                self.span_search.update(cx, |s, cx| s.set_value("", window, cx));
                self.span_search_open = false;
            }
            "e" => self.error = doc.toggle_descendants(),
            "+" | "=" | "-" => self.zoom_by(if key == "-" { 2.0 } else { 0.5 }),
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

    fn zoom_by(&mut self, factor: f64) {
        let Some(doc) = &mut self.doc else { return };
        doc.zoom(factor);
        if let Mode::Live { following, .. } = &mut doc.mode {
            *following = false;
        }
        self.zoomed = true;
    }

    /// Whether the selected span has a branch, and whether it is folded, for the row context menu.
    fn selected_branch(&self) -> (bool, bool) {
        let Some(doc) = &self.doc else { return (false, false) };
        doc.rows.get(doc.selected_row).map_or((false, false), |r| (r.has_children, r.collapsed))
    }

    /// Asks where to save `bytes`, starting in Downloads with `name`, and writes them owner-only.
    fn save_as(&mut self, name: String, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let dir = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Downloads");
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await
                && let Err(e) = trace_view::write_private(&path, &bytes)
            {
                this.update(cx, |this, cx| {
                    this.error = Some(format!("{e:#}"));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Saves a session's events as canonical JSONL, which Import reads back.
    fn export_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let lines: anyhow::Result<Vec<String>> = self.store.session_events(&id).and_then(|evs| evs.iter().map(|e| Ok(serde_json::to_string(e)?)).collect());
        match lines {
            Ok(lines) => self.save_as(format!("{id}.jsonl"), (lines.join("\n") + "\n").into_bytes(), cx),
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Opens the session in replay, paused where the selected span starts, with that span selected.
    fn show_in_replay(&mut self) {
        let Some((sid, span, at)) = self.doc.as_ref().and_then(|d| d.selected_span().map(|s| (d.session_id().clone(), s.span_id.clone(), s.started_at_utc_ns))) else { return };
        self.open(sid, true);
        let Some(doc) = &mut self.doc else { return };
        doc.seek(|_, _| Some(at));
        if let Mode::Replay { clock, .. } = &mut doc.mode {
            clock.playing = false;
        }
        doc.refresh();
        if let Some(i) = doc.rows.iter().position(|r| r.kind == RowKind::Span(span.clone())) {
            doc.select(i);
            self.list.scroll_to_item(i, ScrollStrategy::Center);
        }
    }

    /// The whole session as the overview shows it, for Zoom to Fit.
    fn fit_all(&mut self) {
        let Some(doc) = &self.doc else { return };
        let (a, b) = match &doc.mode {
            Mode::Replay { replay, .. } => (replay.start().unwrap_or(0), replay.end().unwrap_or(0)),
            _ => (doc.state.axis_start().unwrap_or(doc.now), idle(doc).unwrap_or(doc.now)),
        };
        let w = f32::from(doc.timeline_width.max(1));
        self.set_window(a, b, w);
    }

    /// The menu bar's and the row context menu's commands. Span commands act on the selected span; a
    /// right-click selects its row first.
    fn on_actions(&self, root: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        fn span(this: &TraceApp) -> Option<Span> {
            this.doc.as_ref()?.selected_span().cloned()
        }
        root.on_action(cx.listener(|_, _: &About, window, cx| {
            let detail = format!("Version {}\nA waterfall of what your coding agents did, as it happens.", env!("CARGO_PKG_VERSION"));
            drop(window.prompt(PromptLevel::Info, "Agents Trace Waterfall", Some(&detail), &["OK"], cx));
        }))
        .on_action(cx.listener(|this, _: &Import, _, cx| this.prompt_import(cx)))
        .on_action(cx.listener(|this, _: &ExportSession, _, cx| {
            if let Some(id) = this.doc.as_ref().map(|d| d.session_id().clone()) {
                this.export_session(id, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
            this.toggle_sidebar();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
            this.zoom_by(0.5);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
            this.zoom_by(2.0);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| {
            this.fit_all();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &Follow, _, cx| {
            this.toggle_follow();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &Replay, _, cx| {
            this.toggle_replay();
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &NextEvent, _, cx| {
            if let Some(doc) = &mut this.doc {
                doc.seek(|r, c| r.next_event_time(c.position));
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PrevEvent, _, cx| {
            if let Some(doc) = &mut this.doc {
                doc.seek(|r, c| r.prev_event_time(c.position));
            }
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &Inspect, _, cx| {
            if this.doc.as_ref().is_some_and(|d| d.selected.is_some()) {
                this.popover = Pop::Open(Instant::now());
                cx.notify();
            }
        }))
        .on_action(cx.listener(|this, _: &AddNote, window, cx| this.edit_note(window, cx)))
        .on_action(cx.listener(|this, _: &NextActivity, _, cx| {
            this.skip_idle(true);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &PrevActivity, _, cx| {
            this.skip_idle(false);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &CollapseAll, _, cx| {
            this.fold_all_spans(true);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ExpandAll, _, cx| {
            this.fold_all_spans(false);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ZoomToSpan, _, cx| {
            if let Some(s) = span(this) {
                this.zoom_to_span(&s.span_id);
                cx.notify();
            }
        }))
        .on_action(cx.listener(|this, _: &ToggleBranch, _, cx| {
            if let Some(s) = span(this).filter(|_| this.selected_branch().0) {
                this.toggle_fold(&s.span_id);
                cx.notify();
            }
        }))
        .on_action(cx.listener(|this, _: &CopySpanName, _, cx| {
            if let Some(s) = span(this) {
                cx.write_to_clipboard(ClipboardItem::new_string(s.name));
            }
        }))
        .on_action(cx.listener(|this, _: &CopySpanJson, _, cx| {
            if let Some(json) = span(this).and_then(|s| serde_json::to_string_pretty(&s).ok()) {
                cx.write_to_clipboard(ClipboardItem::new_string(json));
            }
        }))
        .on_action(cx.listener(|this, _: &ExportSpanJson, _, cx| {
            if let Some((s, json)) = span(this).and_then(|s| Some((s.span_id.clone(), serde_json::to_string_pretty(&s).ok()?))) {
                this.save_as(format!("span-{s}.json"), json.into_bytes(), cx);
            }
        }))
        .on_action(cx.listener(|this, _: &ShowInReplay, _, cx| {
            this.show_in_replay();
            cx.notify();
        }))
    }
}

/// macOS Increase Contrast and Reduce Transparency, read once at launch. `TW_CONTRAST=1` forces Increase
/// Contrast on, for screenshots.
fn display_accessibility() -> (bool, bool) {
    let forced = std::env::var_os("TW_CONTRAST").is_some();
    #[cfg(target_os = "macos")]
    {
        let w = objc2_app_kit::NSWorkspace::sharedWorkspace();
        (forced || w.accessibilityDisplayShouldIncreaseContrast(), w.accessibilityDisplayShouldReduceTransparency())
    }
    #[cfg(not(target_os = "macos"))]
    (forced, false)
}

fn apply_theme(dark: bool, cx: &mut App) {
    let p = Palette::new(dark, false);
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
        self.sync_note(window, cx);
        let vw = window.viewport_size().width.to_f64() as f32;
        let (sw, sliding) = self.sidebar_w(vw);
        if self.tick() || sliding || self.bench.is_some() {
            window.request_animation_frame();
        }
        let pal = self.palette(window);
        let main_pl = MAIN_PAD * (1. - sw / SIDEBAR_W);
        let card_left = sw + main_pl;
        let root = div()
            .id("root")
            .when(self.doc.is_none(), |el| el.track_focus(&self.focus))
            .on_key_down(cx.listener(Self::on_key))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.import(paths.paths().to_vec(), cx)))
            .size_full()
            .flex()
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .text_color(pal.ink)
            // The sidebar is the translucent tray over the window's blur, like a macOS source list; the
            // content side stays opaque.
            .child(
                div()
                    .w(px(sw))
                    .flex_shrink_0()
                    .h_full()
                    .overflow_hidden()
                    .bg(if self.solid_sidebar { pal.tray } else { pal.tray.opacity(SIDEBAR_ALPHA) })
                    .child(div().ml(px(sw - SIDEBAR_W)).h_full().child(self.render_sidebar(&pal, cx))),
            )
            .child(
                div()
                    .bg(pal.tray)
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
        let root = self.on_actions(root, cx);
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
        let today = today_start();
        let now = trace_view::wall_ns();
        let open_id = self.doc.as_ref().map(|d| d.session_id().clone());
        let searching = !self.session_search_text(cx).is_empty();
        let mut list = div().id("sessions").flex_1().min_h(px(0.)).overflow_y_scroll().px(px(10.)).pb(px(10.));
        for node in present::group_sessions(&self.sessions, self.sidebar.mode, now, today) {
            match node {
                SidebarNode::Section { group, sessions } => {
                    list = list.child(div().pt(px(14.)).pb(px(6.)).px(px(10.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.muted).child(group.title()));
                    for s in sessions {
                        list = list.child(self.render_session_row(pal, s, group, open_id.as_ref() == Some(&s.session_id), today, false, cx));
                    }
                }
                SidebarNode::Workspace(w) => {
                    // A search shows every match, so it opens whatever it found inside.
                    let open = searching || !self.sidebar.collapsed.contains(&w.key());
                    let home = std::env::var("HOME").unwrap_or_default();
                    let tip = if w.path.is_empty() { "Sessions that reported no working directory".to_owned() } else { present::tilde(&w.path, &home) };
                    let count = div().text_size(px(11.5)).text_color(pal.muted).font_features(tnum()).child(w.sessions().to_string());
                    list = list.child(self.render_folder(pal, FolderRow { key: w.key(), path: w.path.clone(), name: w.name.clone(), spoken: w.spoken(), tip, depth: 0, open, live: w.live() > 0, trailing: count.into_any_element() }, cx));
                    if !open {
                        continue;
                    }
                    for p in &w.projects {
                        let open = searching || !self.sidebar.collapsed.contains(&p.key());
                        let trailing = div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(11.5))
                            .text_color(pal.muted)
                            .font_features(tnum())
                            .children(p.cost().map(|c| format_cost(Some(c))))
                            .child(div().h(px(16.)).min_w(px(16.)).px(px(5.)).flex().items_center().justify_center().rounded_full().bg(pal.hover.opacity(4.)).child(p.sessions.len().to_string()));
                        let path = p.path.strip_prefix('#').map_or(p.path.clone(), |_| String::new());
                        list = list.child(self.render_folder(pal, FolderRow { key: p.key(), tip: present::tilde(&path, &home), path, name: p.name.clone(), spoken: p.spoken(), depth: 1, open, live: p.live > 0, trailing: trailing.into_any_element() }, cx));
                        if open {
                            for s in &p.sessions {
                                let g = present::group(s.status == SessionStatus::Live, s.started_at_utc_ns.or(s.last_event_at_utc_ns), today);
                                list = list.child(self.render_session_row(pal, s, g, open_id.as_ref() == Some(&s.session_id), today, true, cx));
                            }
                        }
                    }
                }
            }
        }
        if self.sessions.is_empty() && searching {
            list = list.child(div().px(px(10.)).pt(px(14.)).text_size(px(12.5)).text_color(pal.muted).child("No matching sessions"));
        }
        let app = cx.entity().downgrade();
        let list = list.role(Role::List).aria_label("Sessions").context_menu(move |menu, _, cx| {
            let Some(target) = app.upgrade().and_then(|a| a.read(cx).menu_target.clone()) else { return menu };
            let act = |label: &'static str, f: Box<MenuAction>| {
                let app = app.clone();
                let f = Rc::new(f);
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let f = f.clone();
                    app.update(cx, |this, cx| {
                        f(this, window, cx);
                        cx.notify();
                    })
                    .ok();
                })
            };
            match target {
                MenuTarget::Session(id) => {
                    let (a, b, c, d) = (id.clone(), id.clone(), id.clone(), id);
                    menu.item(act("Open", Box::new(move |this, _, _| this.open(a.clone(), false))))
                        .item(act("Replay", Box::new(move |this, _, _| this.open(b.clone(), true))))
                        .separator()
                        .item(act("Copy Session ID", Box::new(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(c.0.clone())))))
                        .item(act("Export JSONL…", Box::new(move |this, _, cx| this.export_session(d.clone(), cx))))
                }
                MenuTarget::Folder { key, path } => {
                    let (reveal, copy) = (path.clone(), path.clone());
                    let menu = menu
                        .item(act("Collapse All Others", Box::new(move |this, _, _| this.collapse_others(&key))))
                        .item(act("Collapse All", Box::new(|this, _, _| this.fold_all_folders(true))))
                        .item(act("Expand All", Box::new(|this, _, _| this.fold_all_folders(false))));
                    if path.is_empty() {
                        return menu;
                    }
                    menu.separator()
                        .item(act("Reveal in Finder", Box::new(move |_, _, cx| cx.open_with_system(std::path::Path::new(&reveal)))))
                        .item(act("Copy Path", Box::new(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())))))
                }
            }
        });
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
                        .child(Input::new(&self.session_search).tab_index(TAB_SIDEBAR).appearance(false).small().prefix(Icon::new(IconName::Search).size(px(13.)).text_color(pal.muted))),
                ),
            )
            .child(
                div()
                    .px(px(14.))
                    .pt(px(6.))
                    .pb(px(2.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(div().flex_1().child(self.render_mode_switch(pal, cx)))
                    .when(self.sidebar.mode == SidebarMode::Workspaces, |el| {
                        let folded = self.folders_all_folded();
                        el.child(
                            fold_all_button("fold-folders", folded, if folded { "Expand all workspaces" } else { "Collapse all workspaces" }, pal, pal.tray, TAB_SIDEBAR)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.fold_all_folders(!folded);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(list)
            .child(
                div().px(px(14.)).pb(px(14.)).pt(px(6.)).child(
                    focusable(div().id("import"), pal, pal.tray, TAB_SIDEBAR)
                        .role(Role::Button)
                        .aria_label("Import JSONL")
                        .tooltip(|w, cx| Tooltip::new("Import a JSONL trace  ⌘O").build(w, cx))
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

    /// Recent | Workspaces, a compact segmented control under the search field.
    fn render_mode_switch(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let track = if pal.dark { pal.chip } else { pal.hover.opacity(5.) };
        div().id("mode-switch").role(Role::RadioGroup).aria_label("Group sessions").h(px(26.)).flex().p(px(2.)).rounded(px(8.)).bg(track).children([SidebarMode::Recent, SidebarMode::Workspaces].map(|m| {
            let on = self.sidebar.mode == m;
            focusable(div().id(m.label()), pal, track, TAB_SIDEBAR)
                .role(Role::RadioButton)
                .aria_label(m.label())
                .aria_selected(on)
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { pal.ink } else { pal.muted })
                .cursor_pointer()
                .when(on, |el| el.bg(if pal.dark { pal.pop } else { pal.card }).shadow(vec![soft_shadow(pal, 0.08, 1., 2.)]))
                .when(!on, |el| el.hover(|s| s.text_color(pal.text)))
                .child(m.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sidebar.mode = m;
                    this.sidebar.save();
                    cx.notify();
                }))
        }))
    }

    /// A workspace or project header: chevron, folder or name, live mark, trailing counts. Click, Enter, or
    /// Space toggles it; Left collapses and Right expands.
    fn render_folder(&self, pal: &Palette, f: FolderRow, cx: &mut Context<Self>) -> impl IntoElement {
        let turn = match &self.sidebar.turned {
            Some((k, t)) if *k == f.key => ease(self.progress(*t, FOLD_ANIM)),
            _ => 1.,
        };
        let folded = if f.open { 1. - turn } else { turn };
        let workspace = f.depth == 0;
        let (key, menu_key, menu_path) = (f.key.clone(), f.key.clone(), f.path.clone());
        let (open, tip) = (f.open, f.tip);
        focusable(div().id(SharedString::from(format!("folder-{}", f.key))), pal, pal.tray, TAB_SIDEBAR)
            .role(Role::Button)
            .aria_label(f.spoken)
            .aria_expanded(f.open)
            .h(px(if workspace { 30. } else { 28. }))
            .mt(px(if workspace { 8. } else { 0. }))
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(4. + 12. * f32::from(f.depth)))
            .pr(px(10.))
            .rounded(px(7.))
            .cursor_pointer()
            .hover(|s| s.bg(pal.hover.opacity(3.)))
            .child(Icon::new(IconName::ChevronDown).size(px(12.)).text_color(pal.muted).rotate(radians(-std::f32::consts::FRAC_PI_2 * folded)))
            .when(workspace, |el| el.child(Icon::new(if f.open { IconName::FolderOpen } else { IconName::Folder }).size(px(14.)).text_color(pal.muted)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(12.5))
                    .font_weight(if workspace { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                    .text_color(if workspace { pal.ink } else { pal.text })
                    .child(f.name),
            )
            .when(f.live, |el| el.child(status_mark(pal, Status::Live, SharedString::from(format!("live-{key}")))))
            .child(f.trailing)
            .when(!tip.is_empty(), |el| el.tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, _| this.menu_target = Some(MenuTarget::Folder { key: menu_key.clone(), path: menu_path.clone() })))
            .on_key_down(cx.listener({
                let key = key.clone();
                move |this, ev: &KeyDownEvent, _, cx| {
                    let k = ev.keystroke.key.as_str();
                    let toggle = matches!(k, "enter" | "space") || (k == "left" && open) || (k == "right" && !open);
                    if toggle {
                        this.sidebar.toggle(&key);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.sidebar.toggle(&key);
                cx.notify();
            }))
    }

    /// Collapses every other header at the same level as `key`, and opens `key`.
    /// Moves a window of `len` so the activity after `end`, the end of an idle stretch, starts near its left edge.
    fn skip_to(&mut self, end: Ns, len: Ns, tl_w: f32) {
        let a = end - len / 20;
        self.set_window(a, a + len, tl_w);
    }

    /// Next or Previous Activity: jumps the timeline across the nearest idle stretch at the same zoom.
    fn skip_idle(&mut self, forward: bool) {
        let Some(doc) = &self.doc else { return };
        let (Some(vp), tl_w) = (doc.viewport, doc.timeline_width) else { return };
        let len = vp.end_ns(tl_w) - vp.start_ns;
        if let Some(a) = present::skip_idle(&self.idle_gaps.1, vp.start_ns, len, forward) {
            self.set_window(a, a + len, f32::from(tl_w));
        }
    }

    /// Folds or unfolds every span in the open trace, without the per-branch animation.
    fn fold_all_spans(&mut self, fold: bool) {
        let Some(doc) = &mut self.doc else { return };
        self.fold_anim = None;
        doc.fold_all(fold);
        self.list.scroll_to_item(doc.selected_row, ScrollStrategy::Nearest);
    }

    fn folders_all_folded(&self) -> bool {
        folder_keys(&self.sessions, |_| true).iter().all(|k| self.sidebar.collapsed.contains(k))
    }

    /// Collapses or expands every workspace and project in the sidebar.
    fn fold_all_folders(&mut self, fold: bool) {
        self.sidebar.collapsed = if fold { folder_keys(&self.sessions, |_| true).into_iter().collect() } else { HashSet::new() };
        self.sidebar.turned = None;
        self.sidebar.save();
    }

    fn collapse_others(&mut self, key: &str) {
        let level = &key[..2];
        let all = folder_keys(&self.sessions, |_| true);
        self.sidebar.collapsed.retain(|k| !k.starts_with(level));
        self.sidebar.collapsed.extend(all.into_iter().filter(|k| k.starts_with(level) && k != key));
        self.sidebar.save();
    }

    /// A session in the sidebar. `nested` rows sit inside a project, so they drop the project name and lead
    /// with how the session ran.
    #[allow(clippy::too_many_arguments)]
    fn render_session_row(&self, pal: &Palette, s: &SessionSummary, group: Group, selected: bool, today: Ns, nested: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let name = if !s.project_id.is_empty() {
            s.project_id.clone()
        } else {
            s.working_directory.rsplit('/').find(|p| !p.is_empty()).unwrap_or("Untitled session").to_owned()
        };
        let start = s.started_at_utc_ns.or(s.last_event_at_utc_ns);
        let when = start.and_then(|t| Local.timestamp_opt(t / 1_000_000_000, 0).single()).map(|t| {
            if start.unwrap_or(0) >= today { t.format("%H:%M").to_string() } else { t.format("%b %-d").to_string() }
        });
        let dur = present::session_duration(s.started_at_utc_ns, s.ended_at_utc_ns, s.last_event_at_utc_ns, s.status == SessionStatus::Live, trace_view::wall_ns())
            .map(format_duration);
        let join = |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
        // We never store prompts, so what tells sessions apart is how they ran.
        let turns = match s.turn_count {
            0 => None,
            1 => Some("1 turn".to_owned()),
            n => Some(format!("{n} turns")),
        };
        let how = join(vec![present::harness_label(&s.harness, &s.source_adapter), s.model.as_deref().map(present::pretty_model), turns.clone()]);
        let meta = join(vec![when, dur, s.totals.cost_usd_micros().map(|_| format_total_cost(&s.totals))]);
        let status = present::session_status(s.status, s.last_event_at_utc_ns, trace_view::wall_ns());
        let id = s.session_id.clone();
        let menu_id = s.session_id.clone();
        let spoken = [Some(name.clone()), Some(how.clone()).filter(|h| !h.is_empty()), Some(status.label().to_owned()), Some(meta.clone())].into_iter().flatten().collect::<Vec<_>>().join(", ");
        let ring = if selected { pal.card } else { pal.tray };
        // Inside a project the name is the header above, and the tile already names a known harness.
        let (title, how) = if nested && !how.is_empty() {
            let harness = present::harness_label(&s.harness, &s.source_adapter).filter(|_| present::harness_brand(&s.harness, &s.source_adapter).is_none());
            (join(vec![harness, s.model.as_deref().map(present::pretty_model), turns.clone()]), String::new())
        } else {
            (name, how)
        };
        focusable(div().id(SharedString::from(format!("session-{}", s.session_id))), pal, pal.tray, TAB_SIDEBAR)
            .flex()
            .gap(px(10.))
            .pl(px(if nested { 28. } else { 10. }))
            .pr(px(10.))
            .py(px(7.))
            .rounded(px(9.))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(spoken)
            .aria_selected(selected)
            .when(selected, |el| el.bg(pal.card).shadow(vec![soft_shadow(pal, 0.06, 1., 3.)]).border_1().border_color(pal.line))
            .when(!selected, |el| el.border_1().border_color(transparent_black()).hover(|st| st.bg(pal.hover.opacity(3.))))
            .child(session_lead(pal, present::harness_brand(&s.harness, &s.source_adapter), status, present::shows_badge(status, group), ring, ("sdot", s.session_id.0.len() + s.span_count)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::MEDIUM).text_color(pal.ink).child(title))
                    .when(!how.is_empty(), |el| el.child(div().truncate().text_size(px(11.5)).text_color(pal.text).child(how)))
                    .child(div().truncate().text_size(px(11.5)).text_color(pal.muted).font_features(tnum()).child(meta)),
            )
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, _| this.menu_target = Some(MenuTarget::Session(menu_id.clone()))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open(id.clone(), false);
                cx.notify();
            }))
    }
}

type MenuAction = dyn Fn(&mut TraceApp, &mut Window, &mut Context<TraceApp>);

/// A workspace or project header's content, for `render_folder`.
struct FolderRow {
    key: String,
    /// The directory, empty for the No workspace group.
    path: String,
    name: String,
    spoken: String,
    tip: String,
    depth: u8,
    open: bool,
    live: bool,
    trailing: AnyElement,
}

/// State keys of every workspace and project whose name passes `keep`.
fn folder_keys(sessions: &[SessionSummary], keep: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for node in present::group_sessions(sessions, SidebarMode::Workspaces, trace_view::wall_ns(), 0) {
        if let SidebarNode::Workspace(w) = node {
            if keep(&w.name) {
                out.push(w.key());
            }
            out.extend(w.projects.iter().filter(|p| keep(&p.name)).map(|p| p.key()));
        }
    }
    out
}

/// Local midnight today, for the Today group.
fn today_start() -> Ns {
    Local::now().date_naive().and_hms_opt(0, 0, 0).and_then(|d| d.and_local_timezone(Local).earliest()).map_or(0, |d| d.timestamp_nanos_opt().unwrap_or(0))
}

/// Tab order groups: sidebar, header, the trace card, then the popover. All positive, since index 0 sorts
/// apart from them.
const TAB_SIDEBAR: isize = 1;
const TAB_HEADER: isize = 2;
const TAB_CARD: isize = 3;
const TAB_POPOVER: isize = 4;

/// A keyboard-reachable control: a tab stop in group `order` that draws the focus ring, 2pt of accent at
/// 50% set 2pt off its edge over `surface`, only when focus arrived from the keyboard.
fn focusable(el: Stateful<Div>, pal: &Palette, surface: Hsla, order: isize) -> Stateful<Div> {
    let ring = pal.accent.opacity(0.5);
    el.tab_index(order).focus_visible(move |s| s.shadow(focus_ring(ring, surface, false)))
}

/// The focus ring as two spread shadows: the 2pt gap in the surface color, then 2pt of ring beyond it.
/// `inset` draws it inside the element instead, for a region such as the row list that fills its card.
fn focus_ring(ring: Hsla, surface: Hsla, inset: bool) -> Vec<BoxShadow> {
    let spread = |c: Hsla, w: f32| BoxShadow { color: c, offset: point(px(0.), px(0.)), blur_radius: px(0.), spread_radius: px(w), inset };
    if inset { vec![spread(ring, 2.)] } else { vec![spread(ring, 4.), spread(surface, 2.)] }
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

/// A session row's leading mark: the harness logo on a 16pt tile with the status glyph as a badge on its
/// corner, ringed 2pt in the row's fill so it reads as cut out of the tile; or the bare glyph when the
/// harness has no logo. `badge` is false for rows that go without one.
fn session_lead(pal: &Palette, harness: Option<present::Brand>, status: Status, badge: bool, ring: Hsla, id: impl Into<ElementId>) -> AnyElement {
    let slot = div().w(px(16.)).flex_shrink_0().flex().justify_center();
    // Without a tile, a row that goes without a badge keeps its glyph, faded, so the column stays aligned.
    let Some(b) = harness else { return slot.pt(px(4.)).when(!badge, |el| el.opacity(0.4)).child(status_mark(pal, status, id)).into_any_element() };
    slot.pt(px(1.))
        .child(
            div()
                .relative()
                .size(px(16.))
                .child(harness_tile(b, 16., pal))
                .when(badge, |el| el.child(div().absolute().right(px(-5.)).bottom(px(-5.)).p(px(2.)).rounded_full().bg(ring).child(status_mark(pal, status, id)))),
        )
        .into_any_element()
}

/// The harness mark on a tinted tile, at sidebar and header sizes.
fn harness_tile(b: present::Brand, size: f32, pal: &Palette) -> impl IntoElement {
    let b = present::Brand { logo: b.logo.small(), ..b };
    div()
        .size(px(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px((size * 0.28).round()))
        .bg(if pal.dark { pal.chip } else { pal.card })
        .border_1()
        .border_color(pal.line)
        .child(logo(b, b.logo.size_in(size), pal))
}

/// A status in its color and its own shape: a filled dot (pulsing while live), a ring, a check, an x, or a pause.
fn status_mark(pal: &Palette, status: Status, id: impl Into<ElementId>) -> AnyElement {
    status_mark_pulse(pal, status, status == Status::Live, id)
}

fn status_mark_pulse(pal: &Palette, status: Status, pulse: bool, id: impl Into<ElementId>) -> AnyElement {
    let (hue, shape) = status.look();
    let color = pal.hue(hue);
    let glyph = |icon: IconName| div().size(px(8.)).flex().items_center().justify_center().child(Icon::new(icon).size(px(10.)).text_color(color)).into_any_element();
    match shape {
        Shape::Dot if pulse => div()
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
            .into_any_element(),
        Shape::Dot => div().size(px(7.)).rounded_full().bg(color).into_any_element(),
        Shape::Ring => div().size(px(7.)).rounded_full().border(px(1.5)).border_color(color).into_any_element(),
        Shape::Check => glyph(IconName::Check),
        Shape::Cross => glyph(IconName::X),
        Shape::Pause => glyph(IconName::Pause),
    }
}

/// A status capsule: card fill, line ring, soft shadow, with the status color only on its glyph and text.
fn pill(pal: &Palette, status: Status, label: String, pulse: bool, id: impl Into<ElementId>, h: f32) -> impl IntoElement {
    let color = pal.hue(status.look().0);
    let id = id.into();
    div()
        .id(SharedString::from(format!("pill-{id:?}")))
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
        .role(Role::Status)
        .aria_label(format!("Status: {label}"))
        .child(status_mark_pulse(pal, status, pulse, id))
        .child(label)
}

// ── Header ─────────────────────────────────────────────────────────────────

impl TraceApp {
    fn render_sidebar_toggle(&self, pal: &Palette, cx: &mut Context<Self>) -> impl IntoElement {
        focusable(div().id("sidebar-toggle"), pal, pal.tray, TAB_HEADER)
            .role(Role::Button)
            .aria_label("Toggle sidebar")
            .aria_expanded(self.sidebar_anim.1 > 0.)
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
            .tooltip(|w, cx| Tooltip::new("Toggle sidebar  ⌥⌘S").build(w, cx))
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
                if v.is_empty() {
                    return "n/a".to_owned();
                }
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
        let mut harness = None;
        if let Some(doc) = &self.doc {
            let s = &doc.state.session;
            harness = present::harness_brand(&s.harness, &s.source_adapter);
            // From the trace itself, since a sidebar search may have filtered this session out of the list.
            let model = present::top_model(&doc.state);
            subtitle = [Some(s.project_id.clone()).filter(|p| !p.is_empty()), present::harness_label(&s.harness, &s.source_adapter), model]
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
                        .child(Input::new(&self.span_search).tab_index(TAB_HEADER).appearance(false).small().cleanable(true).prefix(Icon::new(IconName::ListFilter).size(px(13.)).text_color(pal.muted))),
                );
            }
            let (status, label, pulse) = match &doc.mode {
                Mode::Live { paused_at: Some(_), .. } => (Status::Paused, "Paused".to_owned(), false),
                Mode::Live { .. } => match idle(doc) {
                    Some(last) => (Status::Idle, format!("Idle · {}", format_duration(doc.now - last)), false),
                    None => (Status::Live, "Live".to_owned(), true),
                },
                Mode::Replay { replay, clock } => (Status::Live, format!("Replay · {}", clock_time(clock.position - replay.start().unwrap_or(0))), false),
                Mode::Static => {
                    let st = present::session_status(s.status, None, 0);
                    (st, st.label().to_owned(), false)
                }
            };
            right = right.child(pill(pal, status, label, pulse, "hdot", 28.));
            if let Mode::Live { following, .. } = &doc.mode {
                let on = *following && now_visible(doc);
                right = right.child(
                    tool_button("follow", IconName::Crosshair, Some("Follow"), on, pal)
                        .aria_toggled(if on { Toggled::True } else { Toggled::False })
                        .tooltip(|w, cx| Tooltip::new("Keep the latest activity in view  F or ⇧⌘F").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_follow();
                            cx.notify();
                        })),
                );
            }
            let replaying = matches!(doc.mode, Mode::Replay { .. });
            right = right.child(
                tool_button("replay", if replaying { IconName::X } else { IconName::RotateCcw }, Some(if replaying { "Exit replay" } else { "Replay" }), false, pal)
                    .tooltip(move |w, cx| Tooltip::new(if replaying { "Leave replay  R or ⌘R" } else { "Replay from the start  R or ⌘R" }).build(w, cx))
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
            .child(div().text_size(px(15.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child("Agents Trace Waterfall"))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .children(harness.map(|b| harness_tile(b, 16., pal)))
                    .child(div().min_w(px(0.)).truncate().text_size(px(13.)).text_color(pal.muted).child(subtitle)),
            )
            .child(right)
    }
}

/// A 24pt icon button that collapses everything, or expands everything once it is all collapsed.
fn fold_all_button(id: &'static str, folded: bool, tip: &'static str, pal: &Palette, surface: Hsla, order: isize) -> Stateful<Div> {
    focusable(div().id(id), pal, surface, order)
        .role(Role::Button)
        .aria_label(tip.split("  ").next().unwrap_or(tip).to_owned())
        .tooltip(move |w, cx| Tooltip::new(tip).build(w, cx))
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .cursor_pointer()
        .hover(|st| st.bg(pal.hover.opacity(4.)))
        .child(Icon::new(if folded { IconName::ChevronsUpDown } else { IconName::ChevronsDownUp }).size(px(14.)).text_color(pal.muted))
}

fn tool_button(id: &'static str, icon: IconName, label: Option<&str>, on: bool, pal: &Palette) -> Stateful<Div> {
    let fg = if on { pal.brand.fg } else { pal.text };
    focusable(div().id(id), pal, pal.tray, TAB_HEADER)
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
        .role(Role::Button)
        .when_some(label, |el, l| el.aria_label(l.to_owned()))
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
        let idle_at = idle(doc);
        // An idle session's overview ends at its last activity; the gap to now is a marker, not hours of empty strip.
        let overview = match &doc.mode {
            Mode::Replay { replay, .. } => (replay.start().unwrap_or(0), replay.end().unwrap_or(0)),
            _ => (doc.state.axis_start().unwrap_or(doc.now), idle_at.unwrap_or(doc.now)),
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
                let focus = if matches!(doc.mode, Mode::Replay { .. }) { None } else { focus_window(doc, fit.ns_per_cell, tl_w * 0.86) };
                doc.viewport = Some(focus.as_ref().map_or(fit, |f| f.1));
                // Opening on the latest turn of a long session leaves now off-screen, so it does not follow.
                if let (Some((_, f)), Mode::Live { following, .. }) = (&focus, &mut doc.mode)
                    && f.end_ns(tl_w as u16) < doc.now
                {
                    *following = false;
                    self.zoomed = true;
                }
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
            vp = focus_window(doc, fit.ns_per_cell, fit_w).map(|f| f.1).filter(|f| f.end_ns(tl_w as u16) >= doc.now).unwrap_or(fit);
            doc.viewport = Some(vp);
        }
        let origin = overview.0;
        let live = matches!(doc.mode, Mode::Live { .. });
        let following = matches!(doc.mode, Mode::Live { following: true, .. }) && now_visible(doc);
        // Replay projects only the past, so everything after the clock would read as idle.
        let key = (doc.state.spans.len(), overview.0, overview.1 / 1_000_000_000);
        if matches!(doc.mode, Mode::Replay { .. }) {
            self.idle_gaps = (key, Vec::new());
        } else if self.idle_gaps.0 != key {
            self.idle_gaps = (key, present::idle_gaps(&doc.state, overview, shown_end(doc), doc.now));
        }
        let ov_w = if idle_at.is_some() { tl_w - IDLE_MARK_W } else { tl_w };
        let squeeze = present::Squeeze::new(overview, ov_w, &self.idle_gaps.1);
        Some(Geo { left: card_left + 1., name_w, tl_w, vp, now: doc.now, origin, overview, end: shown_end(doc), following, idle: idle_at, live, squeeze })
    }

    fn render_card(&mut self, pal: &Palette, vw: f32, card_left: f32, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let g = self.geo(vw, card_left).expect("render_card runs with a document");
        let folding = self.render_fold(g, window, cx);
        let doc = self.doc.as_ref().expect("checked by caller");
        let ticks = tick_times(&g.vp, f64::from(g.tl_w), g.origin, 90.);
        let show_now = !matches!(doc.mode, Mode::Static);
        let now_x = g.x(g.now);
        let count = doc.rows.len();
        let grid = div()
            .absolute()
            .inset_0()
            .children(self.idle_in_view(&g).into_iter().map(|(_, _, x0, x1)| {
                div().absolute().top_0().bottom_0().left(px(g.name_w + x0)).w(px(x1 - x0)).bg(pal.wash).border_l_1().border_r_1().border_color(pal.line)
            }))
            .children(ticks.iter().map(|(t, _)| div().absolute().top_0().bottom_0().left(px(g.name_w + g.x(*t).round())).w(px(1.)).bg(pal.grid)));
        let axis_right = g.name_w + g.tl_w + TL_PAD_R;
        let tick_x: Vec<f32> = ticks.iter().map(|(t, l)| present::tick_label_x(g.name_w + g.x(*t).round(), text_width(window, l, 11.), axis_right)).collect();
        let axis = div()
            .h(px(AXIS_H))
            .flex_shrink_0()
            .relative()
            .border_b_1()
            .border_color(pal.line)
            .child(div().absolute().left(px(16.)).top_0().h_full().flex().items_center().text_size(px(12.)).font_weight(FontWeight::MEDIUM).text_color(pal.muted).child("Span"))
            .child({
                let folded = doc.all_folded();
                let tip = if folded { "Expand all  ⌥⌘→" } else { "Collapse all  ⌥⌘←" };
                div().absolute().left(px(g.name_w - 34.)).top(px((AXIS_H - 24.) / 2.)).child(
                    fold_all_button("fold-spans", folded, tip, pal, pal.card, TAB_CARD).on_click(cx.listener(move |this, _, _, cx| {
                        this.fold_all_spans(!folded);
                        cx.notify();
                    })),
                )
            })
            .children(ticks.iter().zip(tick_x).map(|((_, l), x)| {
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .left(px(x))
                    .text_size(px(11.))
                    .text_color(pal.muted)
                    .font_features(tnum())
                    .child(l.clone())
            }));
        let ring = pal.accent.opacity(0.5);
        let body = div()
            .id("body")
            .track_focus(&self.focus)
            .key_context(ROWS_CONTEXT)
            .tab_index(TAB_CARD)
            .focus_visible(move |s| s.shadow(focus_ring(ring, ring, true)))
            .role(Role::Tree)
            .aria_label("Spans")
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
            .when_some(g.live.then(|| present::now_capsule(g.now, g.window(), g.idle.is_some())).flatten(), |el, (edge, text)| {
                let left = edge == Edge::Left;
                let chevron = Icon::new(if left { IconName::ChevronLeft } else { IconName::ChevronRight }).size(px(14.)).text_color(pal.accent);
                el.child(
                    focusable(div().id("jump-now"), pal, pal.card, TAB_CARD)
                        .role(Role::Button)
                        .aria_label(format!("Jump to now, {text}"))
                        .absolute()
                        .top(px(8.))
                        .when(left, |el| el.left(px(g.name_w + 8.)).pl(px(8.)).pr(px(12.)))
                        .when(!left, |el| el.right(px(14.)).pl(px(12.)).pr(px(8.)))
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .rounded_full()
                        .bg(pal.card)
                        .border_1()
                        .border_color(pal.line)
                        .shadow(vec![soft_shadow(pal, 0.10, 4., 12.), soft_shadow(pal, 0.05, 1., 2.)])
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .font_features(tnum())
                        .text_color(pal.accent)
                        .cursor_pointer()
                        .hover(|s| s.bg(pal.wash))
                        .map(|el| if left { el.child(chevron).child(text) } else { el.child(text).child(chevron) })
                        .tooltip(|w, cx| Tooltip::new("Jump to now and follow  F or ⇧⌘F").build(w, cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.jump_to_now();
                            cx.notify();
                        })),
                )
            });
        let app = cx.entity().downgrade();
        let focus = self.focus.clone();
        let body = body.context_menu(move |menu, _, cx| {
            let Some(app) = app.upgrade() else { return menu };
            let (branch, collapsed) = app.read(cx).selected_branch();
            let noted = app.read(cx).doc.as_ref().and_then(|d| d.selected.as_ref()).is_some_and(|id| app.read(cx).notes.contains_key(id));
            menu.action_context(focus.clone())
                .menu("Inspect", Box::new(Inspect))
                .menu(if noted { "Edit Note" } else { "Add Note" }, Box::new(AddNote))
                .menu("Zoom to Fit", Box::new(ZoomToSpan))
                .menu_with_disabled(if collapsed { "Expand Branch" } else { "Collapse Branch" }, Box::new(ToggleBranch), !branch)
                .menu("Collapse All", Box::new(CollapseAll))
                .menu("Expand All", Box::new(ExpandAll))
                .separator()
                .menu("Copy Span Name", Box::new(CopySpanName))
                .menu("Copy Span JSON", Box::new(CopySpanJson))
                .menu("Export Span JSON…", Box::new(ExportSpanJson))
                .separator()
                .menu("Show in Replay", Box::new(ShowInReplay))
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
            .children(self.render_idle_bands(pal, &g, cx))
            .children(transport)
            .children(popover)
    }

    /// Idle stretches wide enough in view to mark: index, the stretch, and its x range on the timeline.
    fn idle_in_view(&self, g: &Geo) -> Vec<(usize, (Ns, Ns), f32, f32)> {
        let (a, b) = g.window();
        self.idle_gaps
            .1
            .iter()
            .enumerate()
            .filter(|(_, (g0, g1))| *g1 > a && *g0 < b)
            .map(|(i, &(g0, g1))| (i, (g0, g1), g.x(g0).max(0.), g.x(g1).min(g.tl_w)))
            .filter(|(.., x0, x1)| x1 - x0 >= 120.)
            .collect()
    }

    /// Over each idle band, which the grid draws beneath the rows: how long nothing ran and a button that
    /// skips past it.
    fn render_idle_bands(&self, pal: &Palette, g: &Geo, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let (a, b) = g.window();
        let (len, tl_w) = (b - a, g.tl_w);
        let top = STATS_H + OVERVIEW_H + AXIS_H;
        self.idle_in_view(g)
            .into_iter()
            .map(|(i, (g0, g1), x0, x1)| {
                let idle = format_duration(g1 - g0);
                div()
                    .absolute()
                    .top(px(top + 10.))
                    .left(px(1. + g.name_w + x0))
                    .w(px(x1 - x0))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .id(("skip-idle", i))
                            .occlude()
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .rounded_full()
                            .bg(pal.card)
                            .border_1()
                            .border_color(pal.line)
                            .shadow(vec![soft_shadow(pal, 0.06, 1., 3.)])
                            .text_size(px(12.))
                            .font_features(tnum())
                            .cursor_pointer()
                            .hover(|st| st.bg(pal.wash))
                            .role(Role::Button)
                            .aria_label(format!("{idle} idle, skip"))
                            .child(div().text_color(pal.muted).child(format!("{idle} idle")))
                            .child(div().font_weight(FontWeight::MEDIUM).text_color(pal.text).child("Skip"))
                            .child(Icon::new(IconName::ChevronsRight).size(px(13.)).text_color(pal.text))
                            .tooltip(|w, cx| Tooltip::new("Jump to the next activity  ⌥⌘]").build(w, cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.skip_to(g1, len, tl_w);
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            })
            .collect()
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

    /// The whole session as a density sparkline per lane, with the visible window as a lens: click jumps,
    /// drag moves, dragging an edge zooms.
    fn render_overview(&self, pal: &Palette, g: Geo, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        const LANE: f32 = 5.;
        const LANE_GAP: f32 = 2.;
        const RUG: f32 = 3.;
        let doc = self.doc.as_ref().expect("overview needs a document");
        const LANES_H: f32 = present::LANES as f32 * LANE + (present::LANES - 1) as f32 * LANE_GAP;
        let session_end = g.end;
        let ov_w = g.ov_w();
        let cols = ov_w.ceil().max(1.) as usize;
        // One lane per kind (wait in amber, LLM, tool), each column's height its density; failures tick above in red.
        let lane = |k: SpanKind| match k {
            SpanKind::Wait => Some(0),
            SpanKind::Llm => Some(1),
            SpanKind::Tool => Some(2),
            _ => None,
        };
        let spans = doc.state.spans.values().filter_map(|s| Some((g.ov_x(s.started_at_utc_ns), g.ov_x(s.effective_end(session_end, g.now)), lane(s.kind)?)));
        let alpha = present::density(spans, cols);
        let colors = [pal.amber, pal.llm.mark, pal.tool.mark];
        let mut marks: Vec<(f32, f32, f32, Hsla)> = alpha
            .iter()
            .enumerate()
            .flat_map(|(i, a)| (0..present::LANES).filter(|&k| a[k] > 0.).map(move |k| (i, k, a[k])))
            .map(|(i, k, a)| {
                let h = (a * LANE).max(1.);
                (i as f32, k as f32 * (LANE + LANE_GAP) + LANE - h, h, colors[k])
            })
            .collect();
        let mut errors: Vec<usize> = doc.state.spans.values().filter(|s| s.status == SpanStatus::Failed).map(|s| (g.ov_x(s.started_at_utc_ns).max(0.) as usize).min(cols - 1)).collect();
        errors.sort_unstable();
        errors.dedup();
        // A rug above the lanes, so even hundreds of failures never hide the density beneath.
        marks.extend(errors.into_iter().map(|i| (i as f32, -RUG - 1.5, RUG, pal.error.mark)));
        let (a, b) = g.window();
        let (lx0, lx1) = present::lens_rect(g.ov_x(a), g.ov_x(b), ov_w);
        let thin = g.ov_x(b) - g.ov_x(a) < present::LENS_MIN;
        let this = cx.entity().downgrade();
        let sq = g.squeeze;
        let band_fill = pal.line.opacity(if pal.dark { 0.5 } else { 0.35 });
        let strip = canvas(|_, _, _| {}, move |bounds, _, window, _| {
            for &(g0, _) in sq.gaps() {
                window.paint_quad(fill(Bounds::new(point(bounds.origin.x + px(sq.x(g0)), bounds.origin.y + px(3.)), size(px(sq.gap_w), bounds.size.height - px(6.))), band_fill));
            }
            let top = bounds.origin.y + (bounds.size.height - px(LANES_H)) / 2. + px(RUG / 2. + 1.);
            for (x, y, h, color) in &marks {
                window.paint_quad(fill(Bounds::new(point(bounds.origin.x + px(*x), top + px(*y)), size(px(1.), px(*h))), *color));
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
        let grip = |c: Hsla| div().w(px(2.)).h(px(10.)).rounded_full().bg(c);
        let whole = a <= g.overview.0 && b >= g.overview.1;
        let tl_w = g.tl_w;
        let idle_mark = g.idle.map(|last| {
            let gap = present::idle_gap(last, g.now);
            let tip = format!("Last activity {} ago. The overview ends there; now lies past its right edge.", format_duration(g.now - last));
            div()
                .id("idle-gap")
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w(px(IDLE_MARK_W))
                .flex()
                .items_center()
                .justify_end()
                .gap(px(4.))
                .pr(px(8.))
                .border_l_1()
                .border_color(pal.slow.fg.opacity(0.5))
                .text_size(px(11.))
                .font_weight(FontWeight::MEDIUM)
                .font_features(tnum())
                .text_color(pal.slow.fg)
                .role(Role::Status)
                .aria_label(format!("{gap}, now"))
                .child(gap)
                .child(Icon::new(IconName::ArrowRight).size(px(11.)).text_color(pal.slow.fg))
                .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
        });
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
                            focusable(div().id("show-all"), pal, pal.card, 2)
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
                                .role(Role::Button)
                                .aria_label("Show all")
                                .child("Show all")
                                .tooltip(|w, cx| Tooltip::new("Fit the whole session  ⌘0").build(w, cx))
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
                            .px(px(2.))
                            .when(thin, |el| el.justify_center().child(grip(pal.accent)))
                            .when(!thin, |el| el.justify_between().child(grip(pal.faint)).child(grip(pal.faint))),
                    )
                    .child(strip.absolute().top_0().bottom_0().left_0().w(px(ov_w)))
                    .children(g.squeeze.gaps().iter().enumerate().map(|(i, &(g0, g1))| {
                        let tip = format!("{} idle. Click to skip it.", format_duration(g1 - g0));
                        div()
                            .id(("ov-gap", i))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(g.squeeze.x(g0)))
                            .w(px(g.squeeze.gap_w))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(9.5))
                            .font_features(tnum())
                            .text_color(pal.muted)
                            .when(g.squeeze.gap_w >= 20., |el| el.child(present::compact_duration(g1 - g0)))
                            .tooltip(move |w, cx| Tooltip::new(tip.clone()).build(w, cx))
                    }))
                    .children(idle_mark),
            )
    }

    fn render_stats(&self, pal: &Palette) -> impl IntoElement {
        let doc = self.doc.as_ref().expect("stats need a document");
        let t = doc.state.totals();
        let count = |k: SpanKind| doc.state.spans.values().filter(|s| s.kind == k).count();
        let cost = format_total_cost(&t);
        let idle = idle(doc);
        let until_now = idle.map(|_| format!("{} until now", format_duration(doc.state.wall_clock(doc.now))));
        let cells = [
            ("Duration", format_duration(doc.state.wall_clock(idle.unwrap_or(doc.now))), pal.ink),
            ("Cost", cost, pal.ink),
            ("Tokens", format_tokens(t.tokens_input + t.tokens_output), pal.ink),
            ("LLM calls", count(SpanKind::Llm).to_string(), pal.ink),
            ("Tool calls", count(SpanKind::Tool).to_string(), pal.ink),
            ("Errors", t.failed_spans.to_string(), if t.failed_spans > 0 { pal.error.ink } else { pal.ink }),
        ];
        div().h(px(STATS_H)).flex_shrink_0().flex().border_b_1().border_color(pal.line).children(cells.into_iter().enumerate().map(|(i, (label, value, color))| {
            let tip = until_now.clone().filter(|_| i == 0);
            div()
                .id(("stat", i))
                .role(Role::Label)
                .aria_label(label)
                .aria_value(value.clone())
                .when_some(tip, |el, t| el.tooltip(move |w, cx| Tooltip::new(t.clone()).build(w, cx)))
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
        let pal = self.palette(window);
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
        let running = live && s.ended_at_utc_ns.is_none() && s.status == SpanStatus::Running && idle(doc).is_none();
        let tone = present::tone(s);
        let kc = pal.kind(tone);
        let mark = span_mark(s, &doc.state.session, 20., &pal);
        let detail = present::detail(s, &doc.state);
        let indent = 12. + f32::from(row.depth) * INDENT;
        let end = s.effective_end(g.end, g.now);
        // Two pixels in from each edge, so a span that would draw as a sliver reads as off-screen.
        let edge_ns = (2. * g.vp.ns_per_cell) as Ns;
        let (wa, wb) = g.window();
        let hint = present::offscreen_hint(s.started_at_utc_ns, end, (wa + edge_ns, wb - edge_ns), g.following.then_some(g.now));
        let full_title = present::title(s, &doc.state.session);
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
            .child(icon_tile(mark, 20., &pal))
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
                    .when(s.status == SpanStatus::Failed, |el| el.child(Icon::new(IconName::TriangleAlert).size(px(13.)).text_color(pal.error.ink)))
                    .when_some(self.notes.get(id).cloned(), |el, note| {
                        el.child(
                            div()
                                .id(("note", key))
                                .flex_shrink_0()
                                .child(Icon::new(IconName::StickyNote).size(px(13.)).text_color(pal.muted))
                                .tooltip(move |w, cx| Tooltip::new(note.clone()).build(w, cx)),
                        )
                    }),
            );

        let timeline = match hint {
            Some((edge, text)) => self.render_edge_hint(key, id.clone(), edge, text, pal.kind(tone).mark, &pal, g, cx).into_any_element(),
            None => self.render_bar(key, s, &g, kc, &pal, hovered, slowest, &hidden, row_bg, window).into_any_element(),
        };
        let click_id = id.clone();
        let spoken = present::row_label(s, &doc.state, &doc.state.subtree_totals(id), g.end, g.now);
        let el = div()
            .id(("row", key))
            .role(Role::TreeItem)
            .aria_label(spoken)
            .aria_level(usize::from(row.depth) + 1)
            .aria_selected(selected)
            .when(row.has_children, |el| el.aria_expanded(!row.collapsed))
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
            .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, window, cx| {
                this.select(ix);
                this.focus.focus(window, cx);
                cx.notify();
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
        let session_end = g.end;
        let end = s.effective_end(session_end, g.now);
        let unended = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        // An open span in an idle session stops at the last event and trails off instead of growing.
        let stale = unended && g.live && session_end.is_some() && doc.state.session.ended_at_utc_ns.is_none();
        let open = unended && !stale;
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
            .border(px(if pal.contrast { 1.5 } else { 1. }))
            .overflow_hidden()
            .border_color(if hovered { kc.mark } else { kc.edge });
        bar = if open {
            bar.bg(linear_gradient(90., linear_color_stop(kc.bg, 0.55), linear_color_stop(kc.bg.opacity(0.0), 1.))).border_color(if pal.contrast { kc.edge } else { kc.edge.opacity(0.7) }).child(
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
        const DASH: f32 = 4.;
        const DASH_GAP: f32 = 3.;
        let tail_w = if stale && !clip_r { 3. * (DASH + DASH_GAP) } else { 0. };
        for (k, a) in [0.7, 0.45, 0.2].into_iter().enumerate().take(if tail_w > 0. { 3 } else { 0 }) {
            let x = bx + bw + DASH_GAP + k as f32 * (DASH + DASH_GAP);
            cell = cell.child(div().absolute().left(px(x)).top(px((ROW_H - 2.) / 2.)).w(px(DASH)).h(px(2.)).rounded_full().bg(kc.mark.opacity(a)));
        }
        let bw_label = bw + tail_w;
        let label = present::bar_label(s, &doc.state.subtree_totals(&s.span_id), session_end, g.now);
        let lw = text_width(window, &label, LABEL_SIZE);
        let tag_w = if slowest { 58. } else { 0. };
        let at = present::label_at(bx, bw_label, lw + tag_w, g.tl_w);
        let lx = match at {
            LabelAt::After => bx + bw_label + present::LABEL_GAP,
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
        let end = s.effective_end(g.end, g.now);
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
        let place = present::place_popover((bx(s.started_at_utc_ns), bx(end)), bar_y, (card.1.width, card.1.height), body_top, (POPOVER_W, natural));
        let mark = span_mark(s, &doc.state.session, 28., pal);
        let sub = [Some(present::kind_label(s.kind).to_owned()), s.model.as_deref().map(present::pretty_model).or_else(|| present::detail(s, &doc.state))]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let open = s.ended_at_utc_ns.is_none() && !matches!(doc.mode, Mode::Static);
        let quiet = open && idle(doc).is_some();
        let (status, label) = present::span_status(s.status, open, quiet);
        let stats = present::popover_stats(&doc.state, s, &doc.state.subtree_totals(&s.span_id), g.end, doc.now);
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
                    .child(icon_tile(mark, 28., pal))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(div().truncate().text_size(px(13.5)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child(present::title(s, &doc.state.session)))
                            .child(div().truncate().text_size(px(12.)).text_color(pal.muted).child(sub)),
                    )
                    .child(pill(pal, status, label.to_owned(), open && !quiet, "pdot", 24.))
                    .child(
                        focusable(div().id("close"), pal, pal.pop, TAB_POPOVER)
                            .role(Role::Button)
                            .aria_label("Close")
                            .tooltip(|w, cx| Tooltip::new("Close  Esc").build(w, cx))
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
                    .children(stats.chunks(3).map(|row| {
                        // A short last row keeps the column widths of the rows above.
                        div().flex().gap(px(10.)).children(row.iter().map(cell)).children((row.len()..3).map(|_| div().flex_1()))
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(div().text_size(px(11.5)).font_weight(FontWeight::MEDIUM).text_color(pal.muted).child("Note"))
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(6.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(pal.line)
                            .text_size(px(12.5))
                            .child(Textarea::new(&self.note).appearance(false).tab_index(TAB_POPOVER).aria_label("Note")),
                    ),
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
            .map(|el| {
                let present = |v: &Option<String>| v.as_ref().is_some_and(|b| !b.trim().is_empty());
                if present(&s.input_preview) || present(&s.output_preview) {
                    el.child(section("Input", s.input_preview.clone())).child(section("Output", s.output_preview.clone()))
                } else {
                    el.child(div().text_size(px(12.5)).text_color(pal.muted).child("Input and output not captured"))
                }
            })
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
        if self.pop_scroll_end {
            self.pop_scroll.scroll_to_bottom();
        }
        // The body scrolls when it outgrows the room; a fade at the bottom says there is more until its end.
        let max_scroll = self.pop_scroll.max_offset().y.to_f64() as f32;
        let more_below = natural > final_h + 1. && -(self.pop_scroll.offset().y.to_f64() as f32) < max_scroll - 1.;
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
            .child(div().id("pop-scroll").size_full().overflow_y_scroll().track_scroll(&self.pop_scroll).on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify())).child(content))
            .when(more_below, |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom_0()
                        .left_0()
                        .right_0()
                        .h(px(24.))
                        .rounded_b(px(16.))
                        .bg(linear_gradient(180., linear_color_stop(pal.pop.opacity(0.), 0.), linear_color_stop(pal.pop, 1.))),
                )
            });
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
        let icon_btn = |id: &'static str, icon: IconName, label: &'static str, tip: &'static str| {
            focusable(div().id(id), pal, pal.pop, TAB_CARD)
                .role(Role::Button)
                .aria_label(label)
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
                .child(icon_btn("back", IconName::SkipBack, "Previous event", "Previous event  [ or ⌘[").on_click(cx.listener(|this, _, _, cx| {
                    if let Some(doc) = &mut this.doc {
                        doc.seek(|r, c| r.prev_event_time(c.position));
                    }
                    cx.notify();
                })))
                .child(
                    focusable(div().id("play"), pal, pal.pop, TAB_CARD)
                        .role(Role::Button)
                        .aria_label(if playing { "Pause" } else { "Play" })
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
                .child(icon_btn("fwd", IconName::SkipForward, "Next event", "Next event  ] or ⌘]").on_click(cx.listener(|this, _, _, cx| {
                    if let Some(doc) = &mut this.doc {
                        doc.seek(|r, c| r.next_event_time(c.position));
                    }
                    cx.notify();
                })))
                .child(div().w(px(1.)).h(px(20.)).mx(px(8.)).bg(pal.line))
                .child(div().flex().items_center().p(px(2.)).rounded(px(8.)).bg(pal.chip).children(SPEEDS.iter().enumerate().map(|(i, sp)| {
                    let on = i == speed_ix;
                    focusable(div().id(("speed", i)), pal, pal.chip, TAB_CARD)
                        .role(Role::RadioButton)
                        .aria_label(format!("{sp}× speed"))
                        .aria_selected(on)
                        .tooltip(move |w, cx| Tooltip::new(format!("Play at {sp}×  < slower, > faster")).build(w, cx))
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
        // A faint, inert waterfall behind the copy that fades out radially around it, so the empty card
        // already looks like what it will hold. Painted per column: GPUI has no radial gradient.
        // A waterfall: an agent bar over the whole run, then calls stepping down and to the right.
        let sample = [(Tone::Agent, 0.06, 0.86), (Tone::Llm, 0.1, 0.22), (Tone::Tool, 0.3, 0.14), (Tone::Llm, 0.42, 0.24), (Tone::Tool, 0.64, 0.2)];
        let colors = sample.map(|(t, x, w)| (pal.kind(t).edge, x, w));
        let ghost = canvas(|_, _, _| {}, move |b, _, window, _| {
            let (w, h) = (b.size.width.to_f64() as f32, b.size.height.to_f64() as f32);
            let (cx, cy) = (w / 2., h / 2.);
            let rows = colors.len() as f32;
            for (i, (color, x0, bw)) in colors.iter().enumerate() {
                let y = cy + (i as f32 - (rows - 1.) / 2.) * 4. * BAR_H;
                let (a, z) = ((x0 * w).round() as i32, ((x0 + bw) * w).round() as i32);
                for x in a..z {
                    let d = (((x as f32 - cx) / 300.).powi(2) + ((y - cy) / 150.).powi(2)).sqrt();
                    let taper = ((x - a).min(z - 1 - x) as f32 / 5.).clamp(0.2, 1.);
                    // Never fully gone behind the copy, so all five rows still read as one waterfall.
                    let alpha = (0.15 + 0.85 * ((d - 0.7) / 0.8).clamp(0., 1.)) * 0.4 * taper;
                    if alpha > 0. {
                        window.paint_quad(fill(Bounds::new(b.origin + point(px(x as f32), px(y - BAR_H / 2.)), size(px(1.), px(BAR_H))), color.opacity(alpha)));
                    }
                }
            }
        })
        .absolute()
        .inset_0();
        div().flex_1().relative().overflow_hidden().flex().items_center().justify_center().rounded(px(16.)).bg(pal.card).border_1().border_color(pal.line).child(ghost).child(
            div()
                .w(px(400.))
                .p(px(24.))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(14.))
                .child(icon_tile(Icon::new(IconName::Activity).size(px(26.)).text_color(pal.accent).into_any_element(), 44., pal))
                .child(div().text_size(px(17.)).font_weight(FontWeight::SEMIBOLD).text_color(pal.ink).child("No traces yet"))
                .child(
                    div()
                        .text_size(px(13.))
                        .line_height(px(20.))
                        .text_color(pal.text)
                        .text_center()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child("Start the collector and run an agent,")
                        .child("or import a JSONL trace.")
                        .child(div().pt(px(6.)).text_color(pal.muted).child("You can also drop a .jsonl file onto this window.")),
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
                            focusable(div().id("copy-cmd"), pal, pal.wash, TAB_CARD)
                                .role(Role::Button)
                                .aria_label("Copy command")
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

/// A span's mark at the size it draws inside a `tile` point tile: its brand logo, else its lucide glyph.
fn span_mark(s: &Span, session: &trace_core::Session, tile: f32, pal: &Palette) -> AnyElement {
    match present::brand_for(s, present::harness_brand(&session.harness, &session.source_adapter)) {
        // Below 24pt the harness mascots smudge, so they draw as their small marks, as in the sidebar.
        Some(b) => {
            let b = if tile < 24. { present::Brand { logo: b.logo.small(), ..b } } else { b };
            logo(b, b.logo.size_in(tile), pal)
        }
        None => {
            let size = (tile * 0.6).round();
            let (icon, ink) = present::glyph(s);
            Icon::new(icon).size(px(size)).text_color(pal.ink_of(ink)).into_any_element()
        }
    }
}

/// A brand mark `size` points square. Full-color marks draw as images, sized at the source so they
/// rasterize 1:1 (see `Assets`); single-color marks draw as masks in the brand tint.
fn logo(b: present::Brand, size: f32, pal: &Palette) -> AnyElement {
    if b.logo.is_color() {
        img(SharedString::from(format!("{}?pt={size}", b.logo.asset()))).size(px(size)).flex_shrink_0().into_any_element()
    } else {
        svg().path(b.logo.asset()).size(px(size)).flex_shrink_0().text_color(pal.brand_tint(b.tint)).into_any_element()
    }
}

fn icon_tile(mark: AnyElement, size: f32, pal: &Palette) -> impl IntoElement {
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
        .child(mark)
}

/// When a live session went quiet, for the Idle pill and stopped spinners.
fn idle(doc: &TraceDoc) -> Option<Ns> {
    present::idle_since(&doc.state, matches!(doc.mode, Mode::Live { .. }), doc.now)
}

/// Where open spans stop drawing; see `present::shown_end`.
fn shown_end(doc: &TraceDoc) -> Option<Ns> {
    present::shown_end(&doc.state, idle(doc))
}

fn now_visible(doc: &TraceDoc) -> bool {
    doc.viewport.is_some_and(|vp| (vp.start_ns..=vp.end_ns(doc.timeline_width)).contains(&doc.now))
}

/// The latest turn's window when the whole-trace fit would draw bars as slivers.
fn focus_window(doc: &TraceDoc, ns_per_px: f64, fit_w: f32) -> Option<(SpanId, Viewport)> {
    let t = present::focus_turn(&doc.state, ns_per_px)?;
    let end = t.effective_end(shown_end(doc), doc.now);
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
