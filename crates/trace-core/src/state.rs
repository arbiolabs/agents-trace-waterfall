use crate::event::*;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Session {
    pub session_id: SessionId,
    pub project_id: String,
    pub working_directory: String,
    pub source_adapter: String,
    pub harness: String,
    pub host: String,
    pub labels: Attributes,
    pub started_at_utc_ns: Option<Ns>,
    pub ended_at_utc_ns: Option<Ns>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SpanEventRecord {
    pub event_id: EventId,
    pub timestamp_utc_ns: Ns,
    pub name: String,
    pub attributes: Attributes,
    pub payload_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Artifact {
    pub recorded_at_utc_ns: Ns,
    pub path: String,
    pub hash: Option<String>,
    pub change_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span {
    pub span_id: SpanId,
    pub session_id: SessionId,
    pub trace_id: SessionId,
    pub source_id: SourceId,
    pub parent_span_id: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub status: SpanStatus,
    pub started_at_utc_ns: Ns,
    pub ended_at_utc_ns: Option<Ns>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tool_name: Option<String>,
    pub agent_name: Option<String>,
    pub retry_of: Option<SpanId>,
    pub input_preview: Option<String>,
    pub output_preview: Option<String>,
    pub error: Option<String>,
    pub attributes: Attributes,
    /// Usage billed to this span only. Never rolled up into ancestors.
    pub usage: Option<Usage>,
    pub usage_final: bool,
    pub events: Vec<SpanEventRecord>,
    pub artifacts: Vec<Artifact>,
}

/// Projection of one session's journal. Built only through [`TraceState::apply`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TraceState {
    pub session: Session,
    pub spans: BTreeMap<SpanId, Span>,
    /// Children ordered by (start, id); keyed by parent even when the parent has not arrived.
    children: BTreeMap<SpanId, Vec<SpanId>>,
    /// Events that reference a span whose `span.started` has not arrived yet.
    pending: BTreeMap<SpanId, Vec<CanonicalEvent>>,
    seen: BTreeSet<(SourceId, EventId)>,
    pub event_count: usize,
    pub last_occurred_at_utc_ns: Option<Ns>,
}

impl TraceState {
    pub fn new(session_id: SessionId) -> Self {
        Self { session: Session { session_id, ..Default::default() }, ..Default::default() }
    }

    pub fn from_events<'a>(session_id: SessionId, events: impl IntoIterator<Item = &'a CanonicalEvent>) -> Self {
        let mut s = Self::new(session_id);
        for e in events {
            s.apply(e);
        }
        s
    }

    /// The single reducer for live and replay. Idempotent on `(source_id, event_id)`;
    /// returns false when the event was a duplicate or for another session.
    pub fn apply(&mut self, e: &CanonicalEvent) -> bool {
        if e.session_id != self.session.session_id || !self.seen.insert(e.dedup_key()) {
            return false;
        }
        self.event_count += 1;
        // An interruption is a guess from silence; later activity proves the session was still live.
        if self.session.status == SessionStatus::Interrupted
            && !matches!(e.body, EventBody::SessionEnded { .. })
            && self.session.ended_at_utc_ns.is_some_and(|t| e.occurred_at_utc_ns > t)
        {
            self.session.status = SessionStatus::Live;
            self.session.ended_at_utc_ns = None;
        }
        self.last_occurred_at_utc_ns = Some(self.last_occurred_at_utc_ns.map_or(e.occurred_at_utc_ns, |t| t.max(e.occurred_at_utc_ns)));
        self.reduce(e);
        true
    }

    fn reduce(&mut self, e: &CanonicalEvent) {
        let t = e.occurred_at_utc_ns;
        match &e.body {
            EventBody::SessionStarted { project_id, working_directory, source_adapter, harness, host, labels } => {
                let s = &mut self.session;
                s.project_id.clone_from(project_id);
                s.working_directory.clone_from(working_directory);
                s.source_adapter.clone_from(source_adapter);
                s.harness.clone_from(harness);
                s.host.clone_from(host);
                s.labels.extend(labels.clone());
                s.started_at_utc_ns = Some(t);
            }
            EventBody::SessionEnded { status } => {
                self.session.status = *status;
                self.session.ended_at_utc_ns = Some(t);
            }
            EventBody::Heartbeat {} => {}
            EventBody::SpanStarted { parent_span_id, name, kind, model, provider, tool_name, agent_name, retry_of, input_preview, attributes } => {
                let Some(id) = e.span_id.clone() else { return };
                if self.spans.contains_key(&id) {
                    return;
                }
                let span = Span {
                    span_id: id.clone(),
                    session_id: e.session_id.clone(),
                    trace_id: e.session_id.clone(),
                    source_id: e.source_id.clone(),
                    parent_span_id: parent_span_id.clone(),
                    name: name.clone(),
                    kind: *kind,
                    status: SpanStatus::Running,
                    started_at_utc_ns: t,
                    ended_at_utc_ns: None,
                    model: model.clone(),
                    provider: provider.clone(),
                    tool_name: tool_name.clone(),
                    agent_name: agent_name.clone(),
                    retry_of: retry_of.clone(),
                    input_preview: input_preview.clone(),
                    output_preview: None,
                    error: None,
                    attributes: attributes.clone(),
                    usage: None,
                    usage_final: false,
                    events: Vec::new(),
                    artifacts: Vec::new(),
                };
                if let Some(p) = parent_span_id {
                    let siblings = self.children.entry(p.clone()).or_default();
                    let key = (t, &id);
                    let pos = siblings.partition_point(|s| {
                        let sp = &self.spans[s];
                        (sp.started_at_utc_ns, &sp.span_id) < key
                    });
                    siblings.insert(pos, id.clone());
                }
                self.spans.insert(id.clone(), span);
                for pending in self.pending.remove(&id).unwrap_or_default() {
                    self.reduce(&pending);
                }
            }
            body => {
                let Some(id) = &e.span_id else { return };
                let Some(span) = self.spans.get_mut(id) else {
                    self.pending.entry(id.clone()).or_default().push(e.clone());
                    return;
                };
                match body {
                    EventBody::SpanUpdated { name, model, attributes } => {
                        if let Some(n) = name {
                            span.name.clone_from(n);
                        }
                        if let Some(m) = model {
                            span.model = Some(m.clone());
                        }
                        span.attributes.extend(attributes.clone());
                    }
                    EventBody::SpanEnded { status, output_preview, error } => {
                        span.status = *status;
                        span.ended_at_utc_ns = Some(t);
                        if output_preview.is_some() {
                            span.output_preview.clone_from(output_preview);
                        }
                        if error.is_some() {
                            span.error.clone_from(error);
                        }
                    }
                    EventBody::SpanEvent { name, attributes, payload_ref } => span.events.push(SpanEventRecord {
                        event_id: e.event_id.clone(),
                        timestamp_utc_ns: t,
                        name: name.clone(),
                        attributes: attributes.clone(),
                        payload_ref: payload_ref.clone(),
                    }),
                    EventBody::UsageReported { usage, is_final } => apply_usage(span, usage, *is_final),
                    EventBody::ArtifactRecorded { path, hash, change_summary } => span.artifacts.push(Artifact {
                        recorded_at_utc_ns: t,
                        path: path.clone(),
                        hash: hash.clone(),
                        change_summary: change_summary.clone(),
                    }),
                    _ => unreachable!("session and span.started handled above"),
                }
            }
        }
    }

    /// Spans with no parent, ordered by (start, id).
    pub fn roots(&self) -> Vec<&SpanId> {
        self.sorted(self.spans.values().filter(|s| s.parent_span_id.is_none()))
    }

    /// Spans whose parent has not arrived. Rendered in an `Unresolved` group.
    pub fn unresolved(&self) -> Vec<&SpanId> {
        self.sorted(self.spans.values().filter(|s| s.parent_span_id.as_ref().is_some_and(|p| !self.spans.contains_key(p))))
    }

    pub fn children(&self, id: &SpanId) -> &[SpanId] {
        self.children.get(id).map_or(&[], Vec::as_slice)
    }

    pub fn pending_count(&self) -> usize {
        self.pending.values().map(Vec::len).sum()
    }

    fn sorted<'a>(&'a self, it: impl Iterator<Item = &'a Span>) -> Vec<&'a SpanId> {
        let mut v: Vec<_> = it.collect();
        v.sort_by(|a, b| (a.started_at_utc_ns, &a.span_id).cmp(&(b.started_at_utc_ns, &b.span_id)));
        v.into_iter().map(|s| &s.span_id).collect()
    }
}

fn apply_usage(span: &mut Span, u: &Usage, is_final: bool) {
    if is_final {
        span.usage = Some(u.clone());
        span.usage_final = true;
        return;
    }
    if span.usage_final {
        return;
    }
    let cur = span.usage.get_or_insert_with(Usage::default);
    let add = |a: &mut Option<u64>, b: Option<u64>| {
        if let Some(b) = b {
            *a = Some(a.unwrap_or(0) + b);
        }
    };
    add(&mut cur.tokens_input, u.tokens_input);
    add(&mut cur.tokens_output, u.tokens_output);
    add(&mut cur.cache_read, u.cache_read);
    add(&mut cur.cache_write, u.cache_write);
    add(&mut cur.cost_usd_micros, u.cost_usd_micros);
    if u.cost_provenance != CostProvenance::Unknown {
        cur.cost_provenance = u.cost_provenance;
    }
}
