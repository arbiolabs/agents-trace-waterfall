use base64::Engine;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::widgets::TableState;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use trace_core::{SessionId, SessionStatus, SpanId};
use trace_render::Theme;
use trace_storage::{SessionFilter, SessionSummary, Store};
use trace_view::{Mode, TraceDoc, export_span};
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

/// A `TraceDoc` plus the terminal-only state around it: tree scroll, focus and the inspector.
pub struct View {
    pub doc: TraceDoc,
    /// First visible row; `WaterfallWidget` scrolls it to keep the selection visible.
    pub scroll: usize,
    pub focus: Focus,
    pub inspector: Option<Inspector>,
    /// The session's span notes, kept in step with the store.
    pub notes: BTreeMap<SpanId, String>,
}

impl View {
    fn new(doc: TraceDoc, store: &Store) -> Self {
        let notes = store.annotations(doc.session_id()).unwrap_or_default();
        Self {
            doc,
            scroll: 0,
            focus: Focus::Tree,
            inspector: None,
            notes,
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
    /// Text being typed after `/`. Sessions: list filter. Waterfall: span search, or the note with `note_prompt`.
    pub prompt: Option<String>,
    /// The prompt edits the selected span's note, saved on Enter, instead of searching as you type.
    pub note_prompt: bool,
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
            note_prompt: false,
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
        self.view = Some(View::new(TraceDoc::open(&self.store, id)?, &self.store));
        self.screen = Screen::Waterfall;
        Ok(())
    }

    pub fn open_replay(&mut self, id: SessionId, speed: f64, at_end: bool) -> anyhow::Result<()> {
        let doc = TraceDoc::open_replay(&self.store, id, speed, at_end)?;
        self.view = Some(View::new(doc, &self.store));
        self.screen = Screen::Waterfall;
        self.last_tick = Instant::now();
        Ok(())
    }

    /// How long the terminal loop may block before calling `tick`. None = wait for input.
    pub fn tick_interval(&self) -> Option<Duration> {
        match (self.screen, &self.view) {
            (Screen::Sessions, _) => Some(SESSIONS_REFRESH),
            (Screen::Waterfall, Some(v)) => match &v.doc.mode {
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
        match &mut self.view {
            Some(v) => v.doc.advance(&self.store, dt),
            None => Ok(()),
        }
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
        if self.note_prompt {
            match code {
                KeyCode::Esc => {}
                KeyCode::Enter => {
                    let text = buf.clone();
                    self.report(|a| a.save_note(&text));
                }
                KeyCode::Backspace => {
                    buf.pop();
                    return;
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    return;
                }
                _ => return,
            }
            self.prompt = None;
            self.note_prompt = false;
            return;
        }
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

    /// Stores the selected span's note; blank text removes it.
    fn save_note(&mut self, text: &str) -> anyhow::Result<()> {
        let Some(v) = &mut self.view else { return Ok(()) };
        let Some(id) = v.doc.selected.clone() else { return Ok(()) };
        self.store.set_annotation(v.doc.session_id(), &id, text)?;
        if text.trim().is_empty() {
            v.notes.remove(&id);
        } else {
            v.notes.insert(id, text.to_owned());
        }
        Ok(())
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
                    v.doc.search = text;
                    v.doc.refresh();
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
            KeyCode::Char('j') | KeyCode::Down => v.doc.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => v.doc.move_by(-1),
            KeyCode::Char('g') | KeyCode::Home => v.doc.select(0),
            KeyCode::Char('G') | KeyCode::End => v.doc.select(usize::MAX),
            KeyCode::Char('h') | KeyCode::Left if v.focus == Focus::Timeline => v.doc.pan(-1.0),
            KeyCode::Char('l') | KeyCode::Right if v.focus == Focus::Timeline => v.doc.pan(1.0),
            KeyCode::Char('h') | KeyCode::Left => v.doc.left(),
            KeyCode::Char('l') | KeyCode::Right => v.doc.right(),
            KeyCode::Char('e') => self.message = v.doc.toggle_descendants(),
            KeyCode::Char('E') => {
                let fold = !v.doc.all_folded();
                v.doc.fold_all(fold);
            }
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
            KeyCode::Esc if !v.doc.search.is_empty() => {
                v.doc.search.clear();
                v.doc.refresh();
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
        let Some(view) = &mut self.view else { return };
        if code == KeyCode::Char('n') {
            if let Some(id) = &view.doc.selected {
                self.prompt = Some(view.notes.get(id).cloned().unwrap_or_default());
                self.note_prompt = true;
            }
            return;
        }
        let v = &mut view.doc;
        match (code, &mut v.mode) {
            (KeyCode::Char('/'), _) => self.prompt = Some(v.search.clone()),
            (KeyCode::Char('+' | '='), _) => v.zoom(0.5),
            (KeyCode::Char('-'), _) => v.zoom(2.0),
            (KeyCode::Char('H'), Mode::Replay { .. }) => v.seek_by(-1),
            (KeyCode::Char('L'), Mode::Replay { .. }) => v.seek_by(1),
            (KeyCode::Char('H'), _) => v.pan(-1.0),
            (KeyCode::Char('L'), _) => v.pan(1.0),
            (KeyCode::Char(' '), Mode::Live { .. }) => v.toggle_pause(),
            (KeyCode::Char(' '), Mode::Replay { .. }) => {
                self.last_tick = Instant::now();
                v.toggle_pause();
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
                    self.message = Some(match export_span(Path::new("."), s) {
                        Ok(p) => format!("exported {}", p.display()),
                        Err(e) => format!("error: {e:#}"),
                    });
                }
            }
            _ => {}
        }
    }
}
