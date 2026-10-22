use trace_core::*;
use ulid::Ulid;

/// Fields of a `span.started` event. Build with `SpanStart { model: .., ..SpanStart::new(kind, name) }`.
#[derive(Debug, Clone)]
pub struct SpanStart {
    pub parent: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tool_name: Option<String>,
    pub agent_name: Option<String>,
    pub retry_of: Option<SpanId>,
    pub input_preview: Option<String>,
    pub attributes: Attributes,
}

impl SpanStart {
    pub fn new(kind: SpanKind, name: impl Into<String>) -> Self {
        Self {
            parent: None,
            name: name.into(),
            kind,
            model: None,
            provider: None,
            tool_name: None,
            agent_name: None,
            retry_of: None,
            input_preview: None,
            attributes: Attributes::new(),
        }
    }
}

pub fn system_now_ns() -> Ns {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as Ns)
}

/// Builds canonical events for one source and session. Events accumulate until [`Recorder::take`].
pub struct Recorder {
    source_id: SourceId,
    session_id: SessionId,
    clock: Box<dyn FnMut() -> Ns + Send>,
    seq: u64,
    ids: u32,
    salt: u128,
    last_ms: u64,
    events: Vec<CanonicalEvent>,
}

impl Recorder {
    pub fn new(source_id: SourceId, session_id: SessionId) -> Self {
        Self::with_clock(source_id, session_id, system_now_ns)
    }

    pub fn with_clock(source_id: SourceId, session_id: SessionId, clock: impl FnMut() -> Ns + Send + 'static) -> Self {
        // ULID random part: 48 bits from (source, session) so ids differ across sessions, 32-bit counter.
        let salt = (fnv1a(format!("{source_id}\0{session_id}").as_bytes()) as u128 & 0xFFFF_FFFF_FFFF) << 32;
        Self { source_id, session_id, clock: Box::new(clock), seq: 0, ids: 0, salt, last_ms: 0, events: Vec::new() }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns and clears the events recorded so far.
    pub fn take(&mut self) -> Vec<CanonicalEvent> {
        std::mem::take(&mut self.events)
    }

    /// Records any event body. The typed helpers below cover the common cases.
    pub fn emit(&mut self, span_id: Option<SpanId>, body: EventBody) {
        let now = (self.clock)();
        let event_id = EventId(self.next_ulid(now));
        self.seq += 1;
        self.events.push(CanonicalEvent {
            schema_version: SCHEMA_VERSION,
            source_id: self.source_id.clone(),
            source_seq: self.seq,
            event_id,
            observed_at_utc_ns: now,
            occurred_at_utc_ns: now,
            session_id: self.session_id.clone(),
            span_id,
            body,
        });
    }

    pub fn session_started(&mut self, project_id: &str, working_directory: &str, harness: &str) {
        self.emit(
            None,
            EventBody::SessionStarted {
                project_id: project_id.into(),
                working_directory: working_directory.into(),
                source_adapter: self.source_id.0.clone(),
                harness: harness.into(),
                host: String::new(),
                labels: Attributes::new(),
            },
        );
    }

    pub fn session_ended(&mut self, status: SessionStatus) {
        self.emit(None, EventBody::SessionEnded { status });
    }

    pub fn span_started(&mut self, s: SpanStart) -> SpanId {
        let id = SpanId(self.next_ulid(0));
        let body = EventBody::SpanStarted {
            parent_span_id: s.parent,
            name: s.name,
            kind: s.kind,
            model: s.model,
            provider: s.provider,
            tool_name: s.tool_name,
            agent_name: s.agent_name,
            retry_of: s.retry_of,
            input_preview: s.input_preview,
            attributes: s.attributes,
        };
        self.emit(Some(id.clone()), body);
        id
    }

    pub fn span_ended(&mut self, span: &SpanId, status: SpanStatus, error: Option<&str>) {
        self.emit(Some(span.clone()), EventBody::SpanEnded { status, output_preview: None, error: error.map(Into::into) });
    }

    pub fn usage(&mut self, span: &SpanId, usage: Usage, is_final: bool) {
        self.emit(Some(span.clone()), EventBody::UsageReported { usage, is_final });
    }

    pub fn span_event(&mut self, span: &SpanId, name: &str, attributes: Attributes) {
        self.emit(Some(span.clone()), EventBody::SpanEvent { name: name.into(), attributes, payload_ref: None });
    }

    pub fn artifact(&mut self, span: &SpanId, path: &str, change_summary: Option<&str>) {
        self.emit(
            Some(span.clone()),
            EventBody::ArtifactRecorded { path: path.into(), hash: None, change_summary: change_summary.map(Into::into) },
        );
    }

    pub fn heartbeat(&mut self) {
        self.emit(None, EventBody::Heartbeat {});
    }

    /// Strictly increasing per recorder, even if the clock steps backwards.
    fn next_ulid(&mut self, now: Ns) -> String {
        self.last_ms = self.last_ms.max((now.max(0) / 1_000_000) as u64);
        self.ids += 1;
        Ulid::from_parts(self.last_ms, self.salt | self.ids as u128).to_string()
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}
