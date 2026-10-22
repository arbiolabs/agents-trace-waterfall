use anyhow::Context;
use base64::Engine;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use trace_core::{
    CanonicalEvent, Ns, Replay, ReplayClock, SessionId, SessionStatus, Span, SpanId, TraceState,
};
use trace_render::{Row, RowKind, Theme, Viewport, WaterfallState, flatten};
use trace_storage::{SessionFilter, SessionSummary, Store};

pub const EXPAND_ALL_LIMIT: usize = 5_000;
const LIVE_TICK: Duration = Duration::from_millis(66);
const SESSIONS_REFRESH: Duration = Duration::from_secs(1);
const STATUS_FILTERS: [Option<SessionStatus>; 5] = [
    None,
    Some(SessionStatus::Live),
    Some(SessionStatus::Completed),
    Some(SessionStatus::Failed),
    Some(SessionStatus::Interrupted),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Sessions,
    Waterfall,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Timeline,
    Inspector,
}

/// What drives the view clock. All three project the trace through `TraceState::apply`.
pub enum Mode {
    /// Wall clock. While paused, polled events wait in `buffered` and the display clock stays at `paused_at`.
    Live {
        following: bool,
        paused_at: Option<Ns>,
        buffered: Vec<CanonicalEvent>,
    },
    Replay {
        replay: Replay,
        clock: ReplayClock,
    },
    /// A finished session: the clock sits at its end.
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Input,
    Output,
    Events,
    Error,
    Files,
}

impl Tab {
    pub const ALL: [Tab; 6] = [
        Tab::Overview,
        Tab::Input,
        Tab::Output,
        Tab::Events,
        Tab::Error,
        Tab::Files,
    ];

    fn shift(self, d: isize) -> Tab {
        let i = Self::ALL.iter().position(|t| *t == self).unwrap_or(0) as isize;
        Self::ALL[(i + d).rem_euclid(Self::ALL.len() as isize) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inspector {
    pub tab: Tab,
    pub scroll: u16,
}

pub struct View {
    pub state: TraceState,
    pub mode: Mode,
    /// Last journal_seq read for this session.
    pub cursor: i64,
    pub collapsed: HashSet<SpanId>,
    pub search: String,
    pub rows: Vec<Row>,
    pub wf: WaterfallState,
    /// Selection identity; `wf.selected` is re-derived from it whenever rows change.
    pub selected: Option<SpanId>,
    /// None until the first render knows the timeline width.
    pub viewport: Option<Viewport>,
    pub timeline_width: u16,
    pub focus: Focus,
    pub inspector: Option<Inspector>,
    pub now: Ns,
}

impl View {
    fn new(state: TraceState, mode: Mode, cursor: i64) -> Self {
        let mut v = Self {
            state,
            mode,
            cursor,
            collapsed: HashSet::new(),
            search: String::new(),
            rows: Vec::new(),
            wf: WaterfallState::default(),
            selected: None,
            viewport: None,
            timeline_width: 0,
            focus: Focus::Tree,
            inspector: None,
            now: 0,
        };
        v.refresh();
        v
    }

    pub fn session_id(&self) -> &SessionId {
        &self.state.session.session_id
    }

    pub fn selected_span(&self) -> Option<&Span> {
        self.selected
            .as_ref()
            .and_then(|id| self.state.spans.get(id))
    }

    /// Rebuilds rows and the clock after any change to state, collapse set or search.
    pub fn refresh(&mut self) {
        let m = (!self.search.trim().is_empty()).then(|| matcher(&self.search));
        self.rows = flatten(
            &self.state,
            &self.collapsed,
            m.as_ref().map(|f| f as &dyn Fn(&Span) -> bool),
        );
        let found = self.selected.as_ref().and_then(|id| {
            self.rows
                .iter()
                .position(|r| matches!(&r.kind, RowKind::Span(s) if s == id))
        });
        self.select(found.unwrap_or(self.wf.selected));
        self.tick_clock();
    }

    /// The view clock. Live advances with the wall clock unless paused.
    pub fn tick_clock(&mut self) {
        self.now = match &self.mode {
            Mode::Live { paused_at, .. } => paused_at.unwrap_or_else(wall_ns),
            Mode::Replay { clock, .. } => clock.position,
            Mode::Static => self
                .state
                .session
                .ended_at_utc_ns
                .or(self.state.last_occurred_at_utc_ns)
                .unwrap_or(0),
        };
    }

    fn select(&mut self, i: usize) {
        self.wf.selected = i.min(self.rows.len().saturating_sub(1));
        self.selected = match self.rows.get(self.wf.selected).map(|r| &r.kind) {
            Some(RowKind::Span(id)) => Some(id.clone()),
            _ => None,
        };
    }

    /// Fits the viewport to the whole trace, or the whole recording in replay.
    pub fn fit(&self, width: u16) -> Viewport {
        let (start, end) = match &self.mode {
            Mode::Replay { replay, .. } => (replay.start().unwrap_or(0), replay.end().unwrap_or(0)),
            _ => (self.state.axis_start().unwrap_or(self.now), self.now),
        };
        Viewport::fit(start, end, width)
    }

    fn apply_all(&mut self, events: impl IntoIterator<Item = CanonicalEvent>) {
        for e in events {
            self.state.apply(&e);
        }
        if matches!(self.mode, Mode::Live { .. })
            && self.state.session.status != SessionStatus::Live
        {
            self.mode = Mode::Static;
        }
        self.refresh();
    }

    fn move_by(&mut self, d: isize) {
        self.select(self.wf.selected.saturating_add_signed(d));
    }

    fn left(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let row = &self.rows[self.wf.selected];
        if row.has_children && !row.collapsed {
            self.collapsed.insert(id);
            self.refresh();
        } else if let Some(p) = self.state.spans[&id].parent_span_id.clone()
            && let Some(i) = self
                .rows
                .iter()
                .position(|r| r.kind == RowKind::Span(p.clone()))
        {
            self.select(i);
        }
    }

    fn right(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let row = &self.rows[self.wf.selected];
        if row.collapsed {
            self.collapsed.remove(&id);
            self.refresh();
        } else if row.has_children {
            self.move_by(1);
        }
    }

    /// Expands every descendant if any is collapsed, else collapses them all. Stops after EXPAND_ALL_LIMIT spans.
    fn toggle_descendants(&mut self) -> Option<String> {
        let id = self.selected.clone()?;
        let mut subtree = vec![id];
        let mut i = 0;
        while i < subtree.len() && subtree.len() < EXPAND_ALL_LIMIT {
            subtree.extend(self.state.children(&subtree[i]).iter().cloned());
            i += 1;
        }
        subtree.truncate(EXPAND_ALL_LIMIT);
        let parents: Vec<_> = subtree
            .into_iter()
            .filter(|s| !self.state.children(s).is_empty())
            .collect();
        if parents.iter().any(|p| self.collapsed.contains(p)) {
            parents.iter().for_each(|p| {
                self.collapsed.remove(p);
            });
        } else {
            self.collapsed.extend(parents);
        }
        self.refresh();
        (i >= EXPAND_ALL_LIMIT).then(|| format!("stopped at {EXPAND_ALL_LIMIT} spans"))
    }

    fn zoom(&mut self, factor: f64) {
        let following = matches!(
            self.mode,
            Mode::Live {
                following: true,
                ..
            }
        );
        let anchor = match self.selected_span() {
            Some(s) if !following => s.started_at_utc_ns,
            _ => self.now,
        };
        if let Some(vp) = &mut self.viewport {
            vp.zoom(factor, anchor);
        }
    }

    fn pan(&mut self, dir: f64) {
        if let Mode::Live { following, .. } = &mut self.mode {
            *following = false;
        }
        let cells = f64::from(self.timeline_width.max(4)) / 4.0 * dir;
        if let Some(vp) = &mut self.viewport {
            vp.pan(cells);
        }
    }

    fn seek(&mut self, to: impl FnOnce(&Replay, &ReplayClock) -> Option<Ns>) {
        let Mode::Replay { replay, clock } = &mut self.mode else {
            return;
        };
        if let Some(t) = to(replay, clock) {
            clock.position = t.clamp(replay.start().unwrap_or(t), replay.end().unwrap_or(t));
            self.state = replay.state_at(clock.position);
        }
        self.refresh();
    }

    /// Footer status: lag and queue depth for live, position and speed for replay.
    pub fn status(&self) -> String {
        match &self.mode {
            Mode::Live {
                following,
                paused_at,
                buffered,
            } => {
                let lag = self.state.last_occurred_at_utc_ns.map_or("—".into(), |t| {
                    trace_render::format_duration((wall_ns() - t).max(0))
                });
                let state = if paused_at.is_some() {
                    "PAUSED"
                } else {
                    "LIVE"
                };
                let follow = if *following { " follow" } else { "" };
                format!("{state}{follow} lag {lag} queued {}", buffered.len())
            }
            Mode::Replay { replay, clock } => {
                let start = replay.start().unwrap_or(0);
                format!(
                    "REPLAY {} {}x {}/{} ev {}/{}",
                    if clock.playing { "playing" } else { "paused" },
                    clock.speed(),
                    trace_render::format_duration(clock.position - start),
                    trace_render::format_duration(replay.end().unwrap_or(0) - start),
                    replay.index_at(clock.position),
                    replay.events().len()
                )
            }
            Mode::Static => "STATIC".into(),
        }
    }
}

pub struct App {
    pub store: Store,
    pub theme: Theme,
    pub screen: Screen,
    help_return: Screen,
    pub help_scroll: u16,
    pub filter: SessionFilter,
    pub sessions: Vec<SessionSummary>,
    pub table: TableState,
    pub view: Option<View>,
    /// Text being typed after `/`. Sessions: list filter. Waterfall: span search.
    pub prompt: Option<String>,
    pub message: Option<String>,
    /// OSC52 sequence for the terminal loop to write to stdout.
    pub clipboard: Option<String>,
    pub quit: bool,
    last_tick: Instant,
    last_refresh: Instant,
}

impl App {
    pub fn new(store: Store, theme: Theme, filter: SessionFilter) -> anyhow::Result<Self> {
        let mut app = Self {
            store,
            theme,
            screen: Screen::Sessions,
            help_return: Screen::Sessions,
            help_scroll: 0,
            filter,
            sessions: Vec::new(),
            table: TableState::default().with_selected(0),
            view: None,
            prompt: None,
            message: None,
            clipboard: None,
            quit: false,
            last_tick: Instant::now(),
            last_refresh: Instant::now(),
        };
        app.refresh_sessions()?;
        Ok(app)
    }

    pub fn refresh_sessions(&mut self) -> anyhow::Result<()> {
        self.sessions = self.store.list_sessions(&self.filter)?;
        self.last_refresh = Instant::now();
        Ok(())
    }

    pub fn open(&mut self, id: SessionId) -> anyhow::Result<()> {
        let rows = self.store.events_since(0, Some(&id))?;
        anyhow::ensure!(!rows.is_empty(), "session {id} has no events");
        let cursor = rows.last().map_or(0, |r| r.0);
        let mut state = TraceState::new(id);
        for (_, e) in &rows {
            state.apply(e);
        }
        let mode = if state.session.status == SessionStatus::Live {
            Mode::Live {
                following: true,
                paused_at: None,
                buffered: Vec::new(),
            }
        } else {
            Mode::Static
        };
        self.view = Some(View::new(state, mode, cursor));
        self.screen = Screen::Waterfall;
        Ok(())
    }

    pub fn open_replay(&mut self, id: SessionId, speed: f64, at_end: bool) -> anyhow::Result<()> {
        let replay = Replay::new(id.clone(), self.store.session_events(&id)?);
        let (Some(start), Some(end)) = (replay.start(), replay.end()) else {
            anyhow::bail!("session {id} has no events")
        };
        let mut clock = ReplayClock::new(if at_end { end } else { start });
        clock.set_speed(speed);
        clock.playing = !at_end;
        let state = replay.state_at(clock.position);
        self.view = Some(View::new(state, Mode::Replay { replay, clock }, 0));
        self.screen = Screen::Waterfall;
        self.last_tick = Instant::now();
        Ok(())
    }

    /// How long the terminal loop may block before calling `tick`. None = wait for input.
    pub fn tick_interval(&self) -> Option<Duration> {
        match (self.screen, &self.view) {
            (Screen::Sessions, _) => Some(SESSIONS_REFRESH),
            (Screen::Waterfall, Some(v)) => match &v.mode {
                Mode::Live { .. } => Some(LIVE_TICK),
                Mode::Replay { clock, .. } if clock.playing => Some(LIVE_TICK),
                _ => None,
            },
            _ => None,
        }
    }

    /// Advances clocks by real elapsed time and polls the journal.
    pub fn tick(&mut self) {
        let dt = self.last_tick.elapsed();
        self.last_tick = Instant::now();
        if let Err(e) = self.tick_by(dt) {
            self.message = Some(format!("error: {e:#}"));
        }
    }

    pub fn tick_by(&mut self, dt: Duration) -> anyhow::Result<()> {
        if self.screen == Screen::Sessions && self.last_refresh.elapsed() >= SESSIONS_REFRESH {
            self.refresh_sessions()?;
        }
        let Some(v) = &mut self.view else {
            return Ok(());
        };
        match &mut v.mode {
            Mode::Live {
                paused_at,
                buffered,
                ..
            } => {
                let new = self
                    .store
                    .events_since(v.cursor, Some(&v.state.session.session_id))?;
                let Some((seq, _)) = new.last() else {
                    return Ok(());
                };
                v.cursor = *seq;
                let new = new.into_iter().map(|(_, e)| e);
                if paused_at.is_some() {
                    buffered.extend(new);
                } else {
                    v.apply_all(new);
                }
            }
            Mode::Replay { replay, clock } if clock.playing => {
                let before = clock.position;
                clock.tick(dt.as_nanos() as Ns, replay.end().unwrap_or(before));
                if replay.index_at(before) != replay.index_at(clock.position) {
                    v.state = replay.state_at(clock.position);
                    v.refresh();
                } else {
                    v.tick_clock();
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.message = None;
        if self.prompt.is_some() {
            self.prompt_key(key.code);
            return;
        }
        let code = key.code;
        match (self.screen, code) {
            (_, KeyCode::Char('q')) => self.quit = true,
            (Screen::Help, KeyCode::Esc | KeyCode::Char('?')) => self.screen = self.help_return,
            (Screen::Help, KeyCode::Char('j') | KeyCode::Down) => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            (Screen::Help, KeyCode::Char('k') | KeyCode::Up) => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            (Screen::Help, _) => {}
            (s, KeyCode::Char('?')) => {
                self.help_return = s;
                self.screen = Screen::Help;
            }
            (_, KeyCode::Char('s')) => {
                self.screen = Screen::Sessions;
                self.report(|a| a.refresh_sessions());
            }
            (Screen::Sessions, _) => self.sessions_key(code),
            (Screen::Waterfall, _) => self.waterfall_key(code),
        }
    }

    fn report(&mut self, f: impl FnOnce(&mut Self) -> anyhow::Result<()>) {
        if let Err(e) = f(self) {
            self.message = Some(format!("error: {e:#}"));
        }
    }

    fn prompt_key(&mut self, code: KeyCode) {
        let Some(buf) = &mut self.prompt else { return };
        match code {
            KeyCode::Esc => {
                buf.clear();
                self.apply_prompt();
                self.prompt = None;
                return;
            }
            KeyCode::Enter => {
                self.prompt = None;
                return;
            }
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Char(c) => buf.push(c),
            _ => return,
        }
        self.apply_prompt();
    }

    fn apply_prompt(&mut self) {
        let text = self.prompt.clone().unwrap_or_default();
        match self.screen {
            Screen::Sessions => {
                self.filter.text = (!text.is_empty()).then_some(text);
                self.table.select(Some(0));
                self.report(|a| a.refresh_sessions());
            }
            Screen::Waterfall => {
                if let Some(v) = &mut self.view {
                    v.search = text;
                    v.refresh();
                }
            }
            Screen::Help => {}
        }
    }

    fn sessions_key(&mut self, code: KeyCode) {
        let n = self.sessions.len();
        let i = self.table.selected().unwrap_or(0);
        match code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.table.select(Some((i + 1).min(n.saturating_sub(1))))
            }
            KeyCode::Char('k') | KeyCode::Up => self.table.select(Some(i.saturating_sub(1))),
            KeyCode::Char('g') | KeyCode::Home => self.table.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => self.table.select(Some(n.saturating_sub(1))),
            KeyCode::Char('/') => self.prompt = Some(self.filter.text.clone().unwrap_or_default()),
            KeyCode::Char('F') => {
                let at = STATUS_FILTERS
                    .iter()
                    .position(|s| *s == self.filter.status)
                    .unwrap_or(0);
                self.filter.status = STATUS_FILTERS[(at + 1) % STATUS_FILTERS.len()];
                self.table.select(Some(0));
                self.report(|a| a.refresh_sessions());
            }
            KeyCode::Enter => {
                if let Some(s) = self.sessions.get(i) {
                    let id = s.session_id.clone();
                    self.report(|a| a.open(id));
                }
            }
            KeyCode::Esc => {
                self.filter.text = None;
                self.report(|a| a.refresh_sessions());
            }
            _ => {}
        }
    }

    fn waterfall_key(&mut self, code: KeyCode) {
        let Some(v) = &mut self.view else {
            self.screen = Screen::Sessions;
            return;
        };
        if v.focus == Focus::Inspector
            && let Some(ins) = &mut v.inspector
        {
            match code {
                KeyCode::Char('j') | KeyCode::Down => ins.scroll = ins.scroll.saturating_add(1),
                KeyCode::Char('k') | KeyCode::Up => ins.scroll = ins.scroll.saturating_sub(1),
                KeyCode::Char('h') | KeyCode::Left => {
                    *ins = Inspector {
                        tab: ins.tab.shift(-1),
                        scroll: 0,
                    }
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    *ins = Inspector {
                        tab: ins.tab.shift(1),
                        scroll: 0,
                    }
                }
                KeyCode::Esc => {
                    v.inspector = None;
                    v.focus = Focus::Tree;
                }
                KeyCode::Tab => v.focus = Focus::Tree,
                _ => return self.common_key(code),
            }
            return;
        }
        match code {
            KeyCode::Char('j') | KeyCode::Down => v.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => v.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => v.select(0),
            KeyCode::Char('G') | KeyCode::End => v.select(usize::MAX),
            KeyCode::Char('h') | KeyCode::Left if v.focus == Focus::Timeline => v.pan(-1.0),
            KeyCode::Char('l') | KeyCode::Right if v.focus == Focus::Timeline => v.pan(1.0),
            KeyCode::Char('h') | KeyCode::Left => v.left(),
            KeyCode::Char('l') | KeyCode::Right => v.right(),
            KeyCode::Char('e') => self.message = v.toggle_descendants(),
            KeyCode::Enter => {
                v.inspector = Some(v.inspector.unwrap_or(Inspector {
                    tab: Tab::Overview,
                    scroll: 0,
                }));
                v.focus = Focus::Inspector;
            }
            KeyCode::Tab => {
                v.focus = match v.focus {
                    Focus::Tree => Focus::Timeline,
                    _ => {
                        v.inspector.get_or_insert(Inspector {
                            tab: Tab::Overview,
                            scroll: 0,
                        });
                        Focus::Inspector
                    }
                }
            }
            KeyCode::Esc if !v.search.is_empty() => {
                v.search.clear();
                v.refresh();
            }
            KeyCode::Esc => {
                self.screen = Screen::Sessions;
                self.report(|a| a.refresh_sessions());
            }
            _ => self.common_key(code),
        }
    }

    /// Keys that work in every waterfall focus.
    fn common_key(&mut self, code: KeyCode) {
        let Some(v) = &mut self.view else { return };
        match (code, &mut v.mode) {
            (KeyCode::Char('/'), _) => self.prompt = Some(v.search.clone()),
            (KeyCode::Char('+' | '='), _) => v.zoom(0.5),
            (KeyCode::Char('-'), _) => v.zoom(2.0),
            (KeyCode::Char('H'), Mode::Replay { .. }) => {
                let step = (v.timeline_width.max(10) as f64
                    * v.viewport.map_or(1e6, |vp| vp.ns_per_cell)
                    / 10.0) as Ns;
                v.seek(|_, c| Some(c.position - step));
            }
            (KeyCode::Char('L'), Mode::Replay { .. }) => {
                let step = (v.timeline_width.max(10) as f64
                    * v.viewport.map_or(1e6, |vp| vp.ns_per_cell)
                    / 10.0) as Ns;
                v.seek(|_, c| Some(c.position + step));
            }
            (KeyCode::Char('H'), _) => v.pan(-1.0),
            (KeyCode::Char('L'), _) => v.pan(1.0),
            (
                KeyCode::Char(' '),
                Mode::Live {
                    paused_at,
                    buffered,
                    ..
                },
            ) => {
                if paused_at.take().is_some() {
                    let queued = std::mem::take(buffered);
                    v.apply_all(queued);
                } else {
                    *paused_at = Some(v.now);
                }
            }
            (KeyCode::Char(' '), Mode::Replay { replay, clock }) => {
                if !clock.playing && Some(clock.position) >= replay.end() {
                    clock.position = replay.start().unwrap_or(0);
                }
                clock.playing = !clock.playing;
                self.last_tick = Instant::now();
                v.seek(|_, c| Some(c.position));
            }
            (KeyCode::Char('f'), Mode::Live { following, .. }) => *following = !*following,
            (KeyCode::Char('[' | ']' | '<' | '>' | ' ' | 'f'), Mode::Static) => {
                self.message = Some("not live or replaying; press r to replay".into())
            }
            (KeyCode::Char('f'), Mode::Replay { .. }) => {
                self.message = Some("follow is for live sessions".into())
            }
            (KeyCode::Char('['), Mode::Replay { .. }) => {
                v.seek(|r, c| r.prev_event_time(c.position))
            }
            (KeyCode::Char(']'), Mode::Replay { .. }) => {
                v.seek(|r, c| r.next_event_time(c.position))
            }
            (KeyCode::Char('<'), Mode::Replay { clock, .. }) => clock.slower(),
            (KeyCode::Char('>'), Mode::Replay { clock, .. }) => clock.faster(),
            (KeyCode::Char('r'), m) => {
                let replaying = matches!(m, Mode::Replay { .. });
                let id = v.session_id().clone();
                if replaying {
                    self.report(|a| a.open(id));
                } else {
                    self.report(|a| a.open_replay(id, 1.0, false));
                }
            }
            (KeyCode::Char('y'), _) => {
                if let Some(json) = v
                    .selected_span()
                    .and_then(|s| serde_json::to_string_pretty(s).ok())
                {
                    self.clipboard = Some(format!(
                        "\x1b]52;c;{}\x07",
                        base64::engine::general_purpose::STANDARD.encode(&json)
                    ));
                    self.message =
                        Some(format!("copied span JSON ({} bytes) via OSC52", json.len()));
                }
            }
            (KeyCode::Char('x'), _) => {
                if let Some(s) = v.selected_span() {
                    let path = format!("span-{}.json", s.span_id);
                    let res = serde_json::to_string_pretty(s)
                        .map_err(anyhow::Error::from)
                        .and_then(|j| {
                            std::fs::write(&path, j).with_context(|| format!("writing {path}"))
                        });
                    self.message = Some(match res {
                        Ok(()) => format!("exported {path}"),
                        Err(e) => format!("error: {e:#}"),
                    });
                }
            }
            _ => {}
        }
    }
}

pub fn wall_ns() -> Ns {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as Ns)
}

/// Space-separated tokens, all of which must match. `kind:` `status:` `model:` `tool:` `name:` scope a token;
/// a bare token matches any of them. Case-insensitive substring.
pub fn matcher(query: &str) -> impl Fn(&Span) -> bool + use<> {
    let terms: Vec<(Option<String>, String)> = query
        .split_whitespace()
        .map(|t| match t.split_once(':') {
            Some((k @ ("kind" | "status" | "model" | "tool" | "name"), v)) => {
                (Some(k.to_owned()), v.to_lowercase())
            }
            _ => (None, t.to_lowercase()),
        })
        .collect();
    move |s: &Span| {
        let fields = [
            ("name", s.name.to_lowercase()),
            ("kind", label(&s.kind)),
            ("status", label(&s.status)),
            ("model", s.model.as_deref().unwrap_or("").to_lowercase()),
            ("tool", s.tool_name.as_deref().unwrap_or("").to_lowercase()),
        ];
        terms.iter().all(|(k, v)| {
            fields
                .iter()
                .any(|(f, val)| k.as_deref().is_none_or(|k| k == *f) && val.contains(v.as_str()))
        })
    }
}

/// The serde name of a snake_case enum, e.g. `failed`.
pub fn label<T: serde::Serialize>(t: &T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}
