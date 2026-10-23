//! What every transcript adapter shares: an event builder that numbers events from the top of the
//! transcript, so re-reading one yields identical events, and the UTC timestamp parser.

use trace_core::*;

pub(crate) struct Emit {
    pub source: SourceId,
    pub session: SessionId,
    pub observed: Ns,
    pub seq: u64,
    pub out: Vec<CanonicalEvent>,
}

impl Emit {
    pub fn push(&mut self, event_id: String, at: Ns, span: Option<&SpanId>, body: EventBody) {
        self.seq += 1;
        self.out.push(CanonicalEvent {
            schema_version: SCHEMA_VERSION,
            source_id: self.source.clone(),
            source_seq: self.seq,
            event_id: EventId(event_id),
            observed_at_utc_ns: self.observed,
            occurred_at_utc_ns: at,
            session_id: self.session.clone(),
            span_id: span.cloned(),
            body,
        });
    }

    pub fn start(
        &mut self,
        span: &SpanId,
        at: Ns,
        parent: Option<&SpanId>,
        kind: SpanKind,
        name: &str,
        f: impl FnOnce(&mut StartFields),
    ) {
        let mut s = StartFields::default();
        f(&mut s);
        let body = EventBody::SpanStarted {
            parent_span_id: parent.cloned(),
            name: name.to_owned(),
            kind,
            model: s.model,
            provider: s.provider,
            tool_name: s.tool_name,
            agent_name: s.agent_name,
            retry_of: None,
            input_preview: None,
            attributes: s.attributes,
        };
        self.push(format!("start:{span}"), at, Some(span), body);
    }

    pub fn end(
        &mut self,
        event_id: String,
        span: &SpanId,
        at: Ns,
        status: SpanStatus,
        error: Option<&str>,
    ) {
        self.push(
            event_id,
            at,
            Some(span),
            EventBody::SpanEnded {
                status,
                output_preview: None,
                error: error.map(str::to_owned),
            },
        );
    }
}

#[derive(Default)]
pub(crate) struct StartFields {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tool_name: Option<String>,
    pub agent_name: Option<String>,
    pub attributes: Attributes,
}

pub(crate) fn span(kind: &str, id: &str) -> SpanId {
    SpanId(format!("{kind}:{id}"))
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction]Z`, the timestamp form Claude Code, Codex and pi write.
pub(crate) fn parse_utc(s: &str) -> Option<Ns> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let num = |p: &str| p.parse::<i64>().ok();
    let d: Vec<_> = date.split('-').map(num).collect::<Option<_>>()?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let t: Vec<_> = hms.split(':').map(num).collect::<Option<_>>()?;
    let (&[y, m @ 1..=12, day @ 1..=31], &[h @ 0..=23, mi @ 0..=59, sec @ 0..=60]) = (&d[..], &t[..]) else {
        return None;
    };
    if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<9}").parse::<i64>().ok()?
    };
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era.checked_mul(146_097)? + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    days.checked_mul(86_400)?
        .checked_add(h * 3600 + mi * 60 + sec)?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos)
}

