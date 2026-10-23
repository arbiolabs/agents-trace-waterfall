//! Kimi Code CLI (MoonshotAI/kimi-code, the TypeScript successor of kimi-cli) sessions to canonical
//! events, from its data folder (`~/.kimi-code`, or `$KIMI_CODE_HOME`). Metadata only: no prompt,
//! message, thinking or tool payload text is copied.
//!
//! Layout: `sessions/<work dir key>/<session>/state.json` (with the work dir as `cwd`) and one
//! `agents/<agent id>/wire.jsonl` per agent: `main`, and `agent-0`, `agent-1`… for sub-agents. A wire
//! file is JSON lines of `{"type", ...payload, "time": <epoch ms>}` after a `metadata` line (wire
//! protocol 1.5). A session is re-read whole whenever one of its files changes; event ids derive from
//! Kimi Code's step uuids, tool call ids and line numbers, so the store drops what it already has.
//!
//! Spans: `agent:<session>` root, `turn:<session>:<agent>:<line>` from `turn.prompt` to `turn.ended`,
//! `llm:<step uuid>` per model call from its `llm.request` (which names model and provider) until its
//! stream ended (the request time plus the `step.end` loop event's first-token latency and stream
//! duration), with the step's token usage, and `tool:<session>:<call id>` from the `tool.call` loop event
//! to its `tool.result` (failed when `isError`, which a non-zero shell exit sets).
//!
//! Sub-agents: `subagent.spawned` in the parent's wire names the agent and the tool call that started
//! it; the agent joins the root session as `subagent:<session>:<agent id>` under that call, until the
//! parent records `subagent.completed`, `subagent.failed` or `subagent.cancelled`.

use anyhow::Context;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};
use crate::kimi::{dirs, mtime_ms, name, read_json};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(KimiCode(harness))
}

struct KimiCode(&'static str);

impl TraceAdapter for KimiCode {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.0.into(),
            name: "Kimi Code".into(),
            harness: "Kimi Code".into(),
            unobservable: [
                "cost (estimated from list prices)",
                "context compaction calls",
                "prompt, message, thinking and tool input/output text (excluded by design)",
                "host",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, _raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        anyhow::bail!("{} sessions are read from the data folder (~/.kimi-code), not line by line", self.0)
    }
}

/// Events for every session whose files changed after `since` (epoch milliseconds of the newest
/// modification time), and the newest change seen. `path` is the data folder or its `sessions` folder.
pub fn read(path: &Path, since: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    let sessions = if path.ends_with("sessions") { path.to_owned() } else { path.join("sessions") };
    anyhow::ensure!(sessions.is_dir(), "{} does not exist; point it at ~/.kimi-code", sessions.display());
    let mut newest = since;
    let mut events = Vec::new();
    for group in dirs(&sessions) {
        for dir in dirs(&group) {
            let agents: Vec<_> = dirs(&dir.join("agents")).into_iter().filter(|a| a.join("wire.jsonl").is_file()).collect();
            let changed = agents.iter().map(|a| mtime_ms(&a.join("wire.jsonl"))).chain([mtime_ms(&dir.join("state.json"))]).max().unwrap_or(0);
            if agents.is_empty() || changed <= since {
                continue;
            }
            newest = newest.max(changed);
            session(&mut events, harness, &dir, &agents).with_context(|| format!("reading {}", dir.display()))?;
        }
    }
    Ok((events, newest))
}

/// A sub-agent as its parent recorded it: the call that started it, its name, and how it ended.
#[derive(Default)]
struct Spawn {
    call: String,
    name: String,
    at: Ns,
    end: Option<(Ns, SpanStatus)>,
}

fn session(events: &mut Vec<CanonicalEvent>, harness: &str, dir: &Path, agents: &[std::path::PathBuf]) -> anyhow::Result<()> {
    let state = read_json(&dir.join("state.json"));
    let sid = state["id"].as_str().map_or_else(|| name(dir), str::to_owned);
    let wires: Vec<(String, Vec<Record>)> = agents.iter().map(|a| Ok((name(a), wire(&a.join("wire.jsonl"))?))).collect::<anyhow::Result<_>>()?;
    let mut spawns: HashMap<String, Spawn> = HashMap::new();
    for (_, records) in &wires {
        for (at, r, _) in records {
            let Some(id) = r["subagentId"].as_str() else { continue };
            let status = match r["type"].as_str() {
                Some("subagent.spawned") => {
                    let s = spawns.entry(id.to_owned()).or_default();
                    s.call = r["parentToolCallId"].as_str().unwrap_or_default().to_owned();
                    s.name = r["subagentName"].as_str().unwrap_or("subagent").to_owned();
                    s.at = *at;
                    continue;
                }
                Some("subagent.completed") => SpanStatus::Succeeded,
                Some("subagent.failed") => SpanStatus::Failed,
                Some("subagent.cancelled") => SpanStatus::Cancelled,
                _ => continue,
            };
            spawns.entry(id.to_owned()).or_default().end.get_or_insert((*at, status));
        }
    }

    for (agent, records) in &wires {
        let Some(&(first, ..)) = records.first() else { continue };
        let main = agent == "main";
        let source = if main { format!("{harness}:{sid}") } else { format!("{harness}:{sid}/{agent}") };
        let mut e = Emit { source: SourceId(source), session: SessionId(sid.clone()), observed: system_now_ns(), seq: 0, out: Vec::new() };
        let top = if main { span("agent", &sid) } else { span("subagent", &format!("{sid}:{agent}")) };
        if main {
            let cwd = state["cwd"].as_str().unwrap_or_default();
            let body = EventBody::SessionStarted {
                project_id: Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd).to_owned(),
                working_directory: cwd.to_owned(),
                source_adapter: harness.to_owned(),
                harness: "Kimi Code".into(),
                host: String::new(),
                labels: Attributes::new(),
            };
            e.push(format!("session:{sid}"), first, None, body);
            e.start(&top, first, None, SpanKind::Agent, "Kimi Code", |s| s.agent_name = Some("main".into()));
        } else {
            let spawn = spawns.get(agent);
            let under = spawn.filter(|s| !s.call.is_empty()).map_or_else(|| span("agent", &sid), |s| span("tool", &format!("{sid}:{}", s.call)));
            let kind = spawn.map_or("subagent", |s| s.name.as_str()).to_owned();
            e.start(&top, spawn.map_or(first, |s| s.at), Some(&under), SpanKind::Subagent, &kind, |s| s.agent_name = Some(kind.clone()));
        }
        feed(&mut e, &sid, agent, &top, records);
        if let Some(&(at, status)) = spawns.get(agent).and_then(|s| s.end.as_ref()) {
            e.end(format!("end:{top}"), &top, at, status, None);
        }
        events.append(&mut e.out);
    }
    Ok(())
}

/// One agent's turns, model calls and tool calls.
fn feed(e: &mut Emit, sid: &str, agent: &str, top: &SpanId, records: &[Record]) {
    let mut turn: Option<SpanId> = None;
    let mut turns = 0;
    // The step under way, and its model call once requested: (span, request time).
    let mut step: Option<String> = None;
    let mut llm: Option<(SpanId, Ns)> = None;
    for (at, r, line) in records {
        let at = *at;
        let ev = &r["event"];
        match (r["type"].as_str(), ev["type"].as_str()) {
            (Some("turn.prompt"), _) => {
                if let Some(t) = turn.take() {
                    e.end(format!("end:{t}"), &t, at, SpanStatus::Cancelled, None);
                }
                turns += 1;
                let t = span("turn", &format!("{sid}:{agent}:{line}"));
                e.start(&t, at, Some(top), SpanKind::Turn, &format!("turn {turns}"), |_| {});
                turn = Some(t);
            }
            (Some("turn.ended"), _) => {
                let (status, error) = match r["reason"].as_str() {
                    Some("completed") => (SpanStatus::Succeeded, None),
                    Some("cancelled") => (SpanStatus::Cancelled, None),
                    _ => (SpanStatus::Failed, r["error"]["code"].as_str().or(r["reason"].as_str())),
                };
                if let Some((l, _)) = llm.take() {
                    e.end(format!("end:{l}"), &l, at, status, error);
                }
                if let Some(t) = turn.take() {
                    e.end(format!("end:{t}"), &t, at, status, error);
                }
            }
            (Some("context.append_loop_event"), Some("step.begin")) => step = ev["uuid"].as_str().map(str::to_owned),
            // Compaction requests are calls of their own, but nothing records when they end or what they used.
            (Some("llm.request"), _) if r["kind"] == "loop" => {
                let (Some(within), Some(uuid)) = (turn.as_ref(), step.as_deref()) else { continue };
                let l = span("llm", uuid);
                let (model, provider) = (r["model"].as_str().map(str::to_owned), r["provider"].as_str().map(str::to_owned));
                e.start(&l, at, Some(within), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                    s.model = model.clone();
                    s.provider = provider;
                });
                llm = Some((l, at));
            }
            (Some("context.append_loop_event"), Some("step.end")) => {
                step = None;
                let Some((l, requested)) = llm.take() else { continue };
                if let Some(u) = ev["usage"].as_object() {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64);
                    let usage = Usage { tokens_input: n("inputOther"), tokens_output: n("output"), cache_read: n("inputCacheRead"), cache_write: n("inputCacheCreation"), cost_usd_micros: None, cost_provenance: CostProvenance::Unknown };
                    e.push(format!("{l}:usage"), at, Some(&l), EventBody::UsageReported { usage, is_final: true });
                }
                let streamed = ev["llmFirstTokenLatencyMs"].as_i64().zip(ev["llmStreamDurationMs"].as_i64()).map(|(a, b)| requested + (a + b) * 1_000_000);
                let (status, error) = match ev["finishReason"].as_str() {
                    Some("error") => (SpanStatus::Failed, Some("model call failed")),
                    Some("interrupted") => (SpanStatus::Cancelled, None),
                    _ => (SpanStatus::Succeeded, None),
                };
                e.end(format!("end:{l}"), &l, streamed.unwrap_or(at).clamp(requested, at), status, error);
            }
            (Some("context.append_loop_event"), Some("tool.call")) => {
                let (Some(within), Some(call), Some(tool_name)) = (turn.as_ref(), ev["toolCallId"].as_str(), ev["name"].as_str()) else { continue };
                let tool = span("tool", &format!("{sid}:{call}"));
                e.start(&tool, at, Some(within), SpanKind::Tool, tool_name, |s| s.tool_name = Some(tool_name.to_owned()));
            }
            (Some("context.append_loop_event"), Some("tool.result")) => {
                let Some(call) = ev["toolCallId"].as_str() else { continue };
                let tool = span("tool", &format!("{sid}:{call}"));
                let (status, error) = if ev["result"]["isError"] == true { (SpanStatus::Failed, Some("tool reported an error (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                e.end(format!("end:{tool}"), &tool, at, status, error);
            }
            _ => {}
        }
    }
}

/// A wire record with its time and line number.
type Record = (Ns, Value, usize);

/// A wire file's records; the `metadata` header and unreadable lines are skipped.
fn wire(path: &Path) -> anyhow::Result<Vec<Record>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let v: Value = serde_json::from_str(line).ok()?;
            let at = v["time"].as_i64()?.checked_mul(1_000_000).filter(|t| VALID_TIME_NS.contains(t))?;
            Some((at, v, i + 1))
        })
        .collect())
}
