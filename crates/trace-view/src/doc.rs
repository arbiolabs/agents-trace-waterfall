use crate::{Row, RowKind, Viewport, flatten, format_duration};
use anyhow::Context;
use std::collections::HashSet;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use trace_core::{CanonicalEvent, Ns, Replay, ReplayClock, SessionId, SessionStatus, Span, SpanId, TraceState, wire_name};
use trace_storage::{SessionFilter, Store};

pub const EXPAND_ALL_LIMIT: usize = 5_000;

/// What drives the view clock. All three project the trace through `TraceState::apply`.
pub enum Mode {
    /// Wall clock. While paused, polled events wait in `buffered` and the display clock stays at `paused_at`.
    Live { following: bool, paused_at: Option<Ns>, buffered: Vec<CanonicalEvent> },
    Replay { replay: Replay, clock: ReplayClock },
    /// A finished session: the clock sits at its end.
    Static,
}

/// One opened session as the user sees it: projected state, visible rows, selection, viewport and clock.
pub struct TraceDoc {
    pub state: TraceState,
    pub mode: Mode,
    pub last_journal_seq: i64,
    pub collapsed: HashSet<SpanId>,
    pub search: String,
    pub rows: Vec<Row>,
    /// Index into `rows`, always in range (0 when empty). Re-derived from `selected` whenever rows change.
    pub selected_row: usize,
    /// Selection identity.
    pub selected: Option<SpanId>,
    /// None until the first `layout` knows the timeline width.
    pub viewport: Option<Viewport>,
    /// Timeline width in the frontend's cell unit.
    pub timeline_width: u16,
    pub now: Ns,
}

impl TraceDoc {
    pub fn new(state: TraceState, mode: Mode, last_journal_seq: i64) -> Self {
        let mut d = Self {
            state,
            mode,
            last_journal_seq,
            collapsed: HashSet::new(),
            search: String::new(),
            rows: Vec::new(),
            selected_row: 0,
            selected: None,
            viewport: None,
            timeline_width: 0,
            now: 0,
        };
        d.refresh();
        d
    }

    /// Every journaled event of `id`; live if the session is still running, else static.
    pub fn open(store: &Store, id: SessionId) -> anyhow::Result<Self> {
        let rows = store.events_since(0, Some(&id))?;
        anyhow::ensure!(!rows.is_empty(), "session {id} has no events");
        let last_journal_seq = rows.last().map_or(0, |r| r.0);
        let mut state = TraceState::new(id);
        for (_, e) in &rows {
            state.apply(e);
        }
        let mode = if state.session.status == SessionStatus::Live {
            Mode::Live { following: true, paused_at: None, buffered: Vec::new() }
        } else {
            Mode::Static
        };
        Ok(Self::new(state, mode, last_journal_seq))
    }

    /// Replay from the start (playing) or parked at the end (paused).
    pub fn open_replay(store: &Store, id: SessionId, speed: f64, at_end: bool) -> anyhow::Result<Self> {
        let replay = Replay::new(id.clone(), store.session_events(&id)?);
        let (Some(start), Some(end)) = (replay.start(), replay.end()) else {
            anyhow::bail!("session {id} has no events")
        };
        let mut clock = ReplayClock::new(if at_end { end } else { start });
        clock.set_speed(speed);
        clock.playing = !at_end;
        let state = replay.state_at(clock.position);
        Ok(Self::new(state, Mode::Replay { replay, clock }, 0))
    }

    pub fn session_id(&self) -> &SessionId {
        &self.state.session.session_id
    }

    pub fn selected_span(&self) -> Option<&Span> {
        self.selected.as_ref().and_then(|id| self.state.spans.get(id))
    }

    /// Rebuilds rows and the clock after any change to state, collapse set or search.
    pub fn refresh(&mut self) {
        let m = (!self.search.trim().is_empty()).then(|| matcher(&self.search));
        self.rows = flatten(&self.state, &self.collapsed, m.as_ref().map(|f| f as &dyn Fn(&Span) -> bool));
        let found = self
            .selected
            .as_ref()
            .and_then(|id| self.rows.iter().position(|r| matches!(&r.kind, RowKind::Span(s) if s == id)));
        self.select(found.unwrap_or(self.selected_row));
        self.tick_clock();
    }

    /// The view clock. Live advances with the wall clock unless paused.
    pub fn tick_clock(&mut self) {
        self.now = match &self.mode {
            Mode::Live { paused_at, .. } => paused_at.unwrap_or_else(wall_ns),
            Mode::Replay { clock, .. } => clock.position,
            Mode::Static => self.state.session.ended_at_utc_ns.or(self.state.last_occurred_at_utc_ns).unwrap_or(0),
        };
    }

    /// Polls the journal for live sessions (buffering while paused) or advances a playing replay by `dt`.
    pub fn advance(&mut self, store: &Store, dt: Duration) -> anyhow::Result<()> {
        match &mut self.mode {
            Mode::Live { paused_at, buffered, .. } => {
                let new = store.events_since(self.last_journal_seq, Some(&self.state.session.session_id))?;
                let Some((seq, _)) = new.last() else {
                    return Ok(());
                };
                self.last_journal_seq = *seq;
                let new = new.into_iter().map(|(_, e)| e);
                if paused_at.is_some() {
                    buffered.extend(new);
                } else {
                    self.apply_all(new);
                }
            }
            Mode::Replay { replay, clock } if clock.playing => {
                let before = clock.position;
                clock.tick(dt.as_nanos() as Ns, replay.end().unwrap_or(before));
                if replay.index_at(before) != replay.index_at(clock.position) {
                    self.state = replay.state_at(clock.position);
                    self.refresh();
                } else {
                    self.tick_clock();
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Records the timeline width, fits the viewport on first use, and keeps `now` in view while following.
    /// A zero width leaves the timeline state untouched.
    pub fn layout(&mut self, width: u16) -> Viewport {
        if width > 0 {
            self.timeline_width = width;
            let mut vp = self.viewport.unwrap_or_else(|| self.fit(width));
            if let Mode::Live { following: true, .. } = self.mode {
                vp.start_ns = self.now.saturating_sub((f64::from(width) * vp.ns_per_cell * 0.95) as Ns);
            }
            self.viewport = Some(vp);
        }
        self.viewport.unwrap_or_else(|| self.fit(1))
    }

    /// Fits the viewport to the whole trace, or the whole recording in replay.
    pub fn fit(&self, width: u16) -> Viewport {
        let (start, end) = match &self.mode {
            Mode::Replay { replay, .. } => (replay.start().unwrap_or(0), replay.end().unwrap_or(0)),
            _ => (self.state.axis_start().unwrap_or(self.now), self.now),
        };
        Viewport::fit(start, end, width)
    }

    pub fn apply_all(&mut self, events: impl IntoIterator<Item = CanonicalEvent>) {
        for e in events {
            self.state.apply(&e);
        }
        if matches!(self.mode, Mode::Live { .. }) && self.state.session.status != SessionStatus::Live {
            self.mode = Mode::Static;
        }
        self.refresh();
    }

    pub fn select(&mut self, i: usize) {
        self.selected_row = i.min(self.rows.len().saturating_sub(1));
        self.selected = match self.rows.get(self.selected_row).map(|r| &r.kind) {
            Some(RowKind::Span(id)) => Some(id.clone()),
            _ => None,
        };
    }

    pub fn move_by(&mut self, d: isize) {
        self.select(self.selected_row.saturating_add_signed(d));
    }

    /// Collapses an expanded parent, else selects the parent.
    pub fn left(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let row = &self.rows[self.selected_row];
        if row.has_children && !row.collapsed {
            self.collapsed.insert(id);
            self.refresh();
        } else if let Some(p) = self.state.spans[&id].parent_span_id.clone()
            && let Some(i) = self.rows.iter().position(|r| r.kind == RowKind::Span(p.clone()))
        {
            self.select(i);
        }
    }

    /// Expands a collapsed parent, else moves to its first child.
    pub fn right(&mut self) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        let row = &self.rows[self.selected_row];
        if row.collapsed {
            self.collapsed.remove(&id);
            self.refresh();
        } else if row.has_children {
            self.move_by(1);
        }
    }

    /// Expands every descendant if any is collapsed, else collapses them all. Stops after EXPAND_ALL_LIMIT spans.
    pub fn toggle_descendants(&mut self) -> Option<String> {
        let id = self.selected.clone()?;
        let mut subtree = vec![id];
        let mut i = 0;
        while i < subtree.len() && subtree.len() < EXPAND_ALL_LIMIT {
            subtree.extend(self.state.children(&subtree[i]).iter().cloned());
            i += 1;
        }
        subtree.truncate(EXPAND_ALL_LIMIT);
        let parents: Vec<_> = subtree.into_iter().filter(|s| !self.state.children(s).is_empty()).collect();
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

    /// `factor` < 1 zooms in, anchored on the selected span's start, or on `now` while following.
    pub fn zoom(&mut self, factor: f64) {
        let following = matches!(self.mode, Mode::Live { following: true, .. });
        let anchor = match self.selected_span() {
            Some(s) if !following => s.started_at_utc_ns,
            _ => self.now,
        };
        if let Some(vp) = &mut self.viewport {
            vp.zoom(factor, anchor);
        }
    }

    /// Pans a quarter of the timeline width per unit of `dir`; stops following.
    pub fn pan(&mut self, dir: f64) {
        if let Mode::Live { following, .. } = &mut self.mode {
            *following = false;
        }
        let cells = f64::from(self.timeline_width.max(4)) / 4.0 * dir;
        if let Some(vp) = &mut self.viewport {
            vp.pan(cells);
        }
    }

    /// Replay only: moves the clock to `to`, clamped to the recording, and re-projects the state.
    pub fn seek(&mut self, to: impl FnOnce(&Replay, &ReplayClock) -> Option<Ns>) {
        let Mode::Replay { replay, clock } = &mut self.mode else {
            return;
        };
        if let Some(t) = to(replay, clock) {
            clock.position = t.clamp(replay.start().unwrap_or(t), replay.end().unwrap_or(t));
            self.state = replay.state_at(clock.position);
        }
        self.refresh();
    }

    /// Replay only: seeks by `steps` tenths of the visible timeline.
    pub fn seek_by(&mut self, steps: Ns) {
        let step = (self.timeline_width.max(10) as f64 * self.viewport.map_or(1e6, |vp| vp.ns_per_cell) / 10.0) as Ns;
        self.seek(|_, c| Some(c.position + steps * step));
    }

    /// Live: pause, or resume and apply every buffered event. Replay: play/pause, restarting from the top at the end.
    pub fn toggle_pause(&mut self) {
        match &mut self.mode {
            Mode::Live { paused_at, buffered, .. } => {
                if paused_at.take().is_some() {
                    let queued = std::mem::take(buffered);
                    self.apply_all(queued);
                } else {
                    *paused_at = Some(self.now);
                }
            }
            Mode::Replay { replay, clock } => {
                if !clock.playing && Some(clock.position) >= replay.end() {
                    clock.position = replay.start().unwrap_or(0);
                }
                clock.playing = !clock.playing;
                self.seek(|_, c| Some(c.position));
            }
            Mode::Static => {}
        }
    }

    /// Lag and queue depth for live, position and speed for replay.
    pub fn status(&self) -> String {
        match &self.mode {
            Mode::Live { following, paused_at, buffered } => {
                let lag = self.state.last_occurred_at_utc_ns.map_or("—".into(), |t| format_duration((wall_ns() - t).max(0)));
                let state = if paused_at.is_some() { "PAUSED" } else { "LIVE" };
                let follow = if *following { " follow" } else { "" };
                format!("{state}{follow} lag {lag} queued {}", buffered.len())
            }
            Mode::Replay { replay, clock } => {
                let start = replay.start().unwrap_or(0);
                format!(
                    "REPLAY {} {}x {}/{} ev {}/{}",
                    if clock.playing { "playing" } else { "paused" },
                    clock.speed(),
                    format_duration(clock.position - start),
                    format_duration(replay.end().unwrap_or(0) - start),
                    replay.index_at(clock.position),
                    replay.events().len()
                )
            }
            Mode::Static => "STATIC".into(),
        }
    }
}

/// Writes `dir/span-<id>.json` readable only by the owner. The untrusted id is reduced to `[A-Za-z0-9._-]`.
pub fn export_span(dir: &Path, s: &Span) -> anyhow::Result<PathBuf> {
    let id: String = s
        .span_id
        .to_string()
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    let path = dir.join(format!("span-{id}.json"));
    write_private(&path, serde_json::to_string_pretty(s)?.as_bytes())?;
    Ok(path)
}

/// Writes `bytes` to `path`, readable only by the owner: traces can hold tool inputs and outputs.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| {
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.write_all(bytes)
        })
        .with_context(|| format!("writing {}", path.display()))
}

pub fn wall_ns() -> Ns {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as Ns)
}

/// Space-separated tokens, all of which must match. `kind:` `status:` `model:` `tool:` `name:` scope a token;
/// a bare token matches any of them. Case-insensitive substring.
pub fn matcher(query: &str) -> impl Fn(&Span) -> bool + use<> {
    let terms: Vec<(Option<String>, String)> = query
        .split_whitespace()
        .map(|t| match t.split_once(':') {
            Some((k @ ("kind" | "status" | "model" | "tool" | "name"), v)) => (Some(k.to_owned()), v.to_lowercase()),
            _ => (None, t.to_lowercase()),
        })
        .collect();
    move |s: &Span| {
        let fields = [
            ("name", s.name.to_lowercase()),
            ("kind", wire_name(&s.kind)),
            ("status", wire_name(&s.status)),
            ("model", s.model.as_deref().unwrap_or("").to_lowercase()),
            ("tool", s.tool_name.as_deref().unwrap_or("").to_lowercase()),
        ];
        terms
            .iter()
            .all(|(k, v)| fields.iter().any(|(f, val)| k.as_deref().is_none_or(|k| k == *f) && val.contains(v.as_str())))
    }
}

/// A full id or a unique prefix of one.
pub fn resolve_session(store: &Store, prefix: &str) -> anyhow::Result<SessionId> {
    let hits: Vec<SessionId> =
        store.list_sessions(&SessionFilter::default())?.into_iter().map(|s| s.session_id).filter(|id| id.0.starts_with(prefix)).collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no session matches {prefix:?}"),
        many if many.iter().any(|id| id.0 == prefix) => Ok(SessionId(prefix.to_owned())),
        many => anyhow::bail!("{prefix:?} is ambiguous: {}", many.iter().map(|i| i.0.as_str()).collect::<Vec<_>>().join(", ")),
    }
}
