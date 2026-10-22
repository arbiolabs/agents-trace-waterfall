use serde::{Deserialize, Serialize};
use serde_json::Map;
use std::cmp::Ordering;

pub type Ns = i64;
pub type Attributes = Map<String, serde_json::Value>;
pub const SCHEMA_VERSION: u32 = 1;

macro_rules! id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
id!(SessionId);
id!(SpanId);
id!(SourceId);
id!(EventId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Agent,
    Turn,
    Llm,
    Tool,
    Subagent,
    Wait,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    #[default]
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Live,
    Completed,
    Failed,
    Interrupted,
    Unknown,
}

/// Where a cost figure came from. Unknown renders as `—`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CostProvenance {
    Actual,
    Estimate,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_micros: Option<u64>,
    #[serde(default)]
    pub cost_provenance: CostProvenance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "body")]
pub enum EventBody {
    #[serde(rename = "session.started")]
    SessionStarted {
        #[serde(default)]
        project_id: String,
        #[serde(default)]
        working_directory: String,
        #[serde(default)]
        source_adapter: String,
        #[serde(default)]
        harness: String,
        #[serde(default)]
        host: String,
        #[serde(default)]
        labels: Attributes,
    },
    #[serde(rename = "session.ended")]
    SessionEnded { status: SessionStatus },
    #[serde(rename = "span.started")]
    SpanStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_span_id: Option<SpanId>,
        name: String,
        kind: SpanKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_of: Option<SpanId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_preview: Option<String>,
        #[serde(default)]
        attributes: Attributes,
    },
    /// Merges attributes; replaces any field that is present.
    #[serde(rename = "span.updated")]
    SpanUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        attributes: Attributes,
    },
    #[serde(rename = "span.ended")]
    SpanEnded {
        status: SpanStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_preview: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "span.event")]
    SpanEvent {
        name: String,
        #[serde(default)]
        attributes: Attributes,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload_ref: Option<String>,
    },
    /// `is_final: false` is a provisional delta; a final report replaces everything before it.
    #[serde(rename = "usage.reported")]
    UsageReported {
        #[serde(flatten)]
        usage: Usage,
        #[serde(default)]
        is_final: bool,
    },
    #[serde(rename = "artifact.recorded")]
    ArtifactRecorded {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change_summary: Option<String>,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat {},
}

impl EventBody {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session.started",
            Self::SessionEnded { .. } => "session.ended",
            Self::SpanStarted { .. } => "span.started",
            Self::SpanUpdated { .. } => "span.updated",
            Self::SpanEnded { .. } => "span.ended",
            Self::SpanEvent { .. } => "span.event",
            Self::UsageReported { .. } => "usage.reported",
            Self::ArtifactRecorded { .. } => "artifact.recorded",
            Self::Heartbeat {} => "heartbeat",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanonicalEvent {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    pub source_id: SourceId,
    pub source_seq: u64,
    pub event_id: EventId,
    pub observed_at_utc_ns: Ns,
    pub occurred_at_utc_ns: Ns,
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<SpanId>,
    #[serde(flatten)]
    pub body: EventBody,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

impl CanonicalEvent {
    /// Deterministic replay order. Ties broken by source position, which does not imply causality.
    pub fn order_cmp(&self, other: &Self) -> Ordering {
        (self.occurred_at_utc_ns, &self.source_id, self.source_seq, &self.event_id).cmp(&(
            other.occurred_at_utc_ns,
            &other.source_id,
            other.source_seq,
            &other.event_id,
        ))
    }

    pub fn dedup_key(&self) -> (SourceId, EventId) {
        (self.source_id.clone(), self.event_id.clone())
    }
}

pub fn sort_events(events: &mut [CanonicalEvent]) {
    events.sort_by(CanonicalEvent::order_cmp);
}

/// The serde wire name of a unit enum value, e.g. `SpanStatus::Failed` -> "failed".
pub fn wire_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}
