use crate::event::*;
use crate::state::TraceState;

pub const SPEEDS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 5.0, 10.0];
/// Checkpoints are at least this many events apart.
pub const MIN_CHECKPOINT_INTERVAL: usize = 500;
/// Upper bound on checkpoints after the initial empty state, so memory stays O(n) instead of O(n²).
pub const MAX_CHECKPOINTS: usize = 64;

/// A recorded journal plus checkpoints, so seeking never refolds the whole history.
/// Replay only reads recorded events; it never invokes tools or models.
pub struct Replay {
    session_id: SessionId,
    events: Vec<CanonicalEvent>,
    /// `checkpoints[i]` is the state after applying `events[..i * interval]`.
    checkpoints: Vec<TraceState>,
    interval: usize,
}

impl Replay {
    pub fn new(session_id: SessionId, mut events: Vec<CanonicalEvent>) -> Self {
        events.retain(|e| e.session_id == session_id);
        sort_events(&mut events);
        // ponytail: a seek refolds up to n/64 events; add a delta-based checkpoint if long sessions seek slowly.
        let interval = MIN_CHECKPOINT_INTERVAL.max(events.len().div_ceil(MAX_CHECKPOINTS));
        let mut checkpoints = vec![TraceState::new(session_id.clone())];
        let mut state = checkpoints[0].clone();
        for chunk in events.chunks(interval) {
            for e in chunk {
                state.apply(e);
            }
            checkpoints.push(state.clone());
        }
        Self { session_id, events, checkpoints, interval }
    }

    pub fn events(&self) -> &[CanonicalEvent] {
        &self.events
    }

    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn start(&self) -> Option<Ns> {
        self.events.first().map(|e| e.occurred_at_utc_ns)
    }

    pub fn end(&self) -> Option<Ns> {
        self.events.last().map(|e| e.occurred_at_utc_ns)
    }

    /// Number of events with occurrence time <= `t`.
    pub fn index_at(&self, t: Ns) -> usize {
        self.events.partition_point(|e| e.occurred_at_utc_ns <= t)
    }

    /// State after the first `n` events in replay order.
    pub fn state_after(&self, n: usize) -> TraceState {
        let n = n.min(self.events.len());
        let cp = n / self.interval;
        let mut s = self.checkpoints[cp].clone();
        for e in &self.events[cp * self.interval..n] {
            s.apply(e);
        }
        s
    }

    pub fn state_at(&self, t: Ns) -> TraceState {
        self.state_after(self.index_at(t))
    }

    /// Occurrence time of the next/previous distinct event relative to `t`.
    pub fn next_event_time(&self, t: Ns) -> Option<Ns> {
        self.events.get(self.index_at(t)).map(|e| e.occurred_at_utc_ns)
    }

    pub fn prev_event_time(&self, t: Ns) -> Option<Ns> {
        let i = self.events.partition_point(|e| e.occurred_at_utc_ns < t);
        i.checked_sub(1).map(|i| self.events[i].occurred_at_utc_ns)
    }
}

/// Virtual clock for replay; the live clock is wall time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayClock {
    pub position: Ns,
    pub speed_index: usize,
    pub playing: bool,
}

impl ReplayClock {
    pub fn new(position: Ns) -> Self {
        Self { position, speed_index: 2, playing: false }
    }

    pub fn speed(&self) -> f64 {
        SPEEDS[self.speed_index]
    }

    pub fn faster(&mut self) {
        self.speed_index = (self.speed_index + 1).min(SPEEDS.len() - 1);
    }

    pub fn slower(&mut self) {
        self.speed_index = self.speed_index.saturating_sub(1);
    }

    pub fn set_speed(&mut self, speed: f64) {
        self.speed_index = SPEEDS.iter().position(|s| (*s - speed).abs() < 1e-9).unwrap_or(2);
    }

    /// Advances by `real_dt_ns` of wall time, clamped to `end`. Stops playing at the end.
    pub fn tick(&mut self, real_dt_ns: Ns, end: Ns) {
        if !self.playing {
            return;
        }
        self.position = (self.position + (real_dt_ns as f64 * self.speed()) as Ns).min(end);
        if self.position >= end {
            self.playing = false;
        }
    }
}
