//! Kimi CLI (MoonshotAI/kimi-cli, the Python agent; its last releases call themselves Kimi Code CLI)
//! sessions to canonical events, from its share folder (`~/.kimi`, or `$KIMI_SHARE_DIR`). Metadata only:
//! no prompt, message, thinking or tool payload text is copied.
//!
//! Layout: `sessions/<md5 of work dir>/<session>/wire.jsonl`, the session's wire messages as JSON lines
//! (`{"timestamp": <epoch seconds>, "message": {"type", "payload"}}` after a `metadata` line), and
//! `subagents/<agent id>/wire.jsonl` beside it for each sub-agent. `kimi.json` lists the work dirs whose
//! md5 names the session folders. A session is re-read whole whenever one of its wire files changes;
//! event ids derive from line numbers and tool call ids, so the store drops what it already has.
//!
//! Spans: `agent:<session>` root, `turn:<session>:<line>` from `TurnBegin` to `TurnEnd`, `llm:` per step
//! from `StepBegin` to the `StatusUpdate` that reports the step's token usage (failed on
//! `StepInterrupted`), and `tool:<session>:<call id>` from `ToolCall` to its `ToolResult` (failed when
//! `return_value.is_error`, which a shell command's non-zero exit sets).
//!
//! Sub-agents: the `Agent` tool's sub-agent joins the root session as `subagent:<session>:<agent id>`
//! under the tool call that started it: the parent wire's `SubagentEvent`s name the call for a
//! foreground agent, `tasks/<task>/spec.json` for a background one. Its span ends with its first run.
//!
//! The wire never names the model: the model is `config.toml`'s default model, or the one a sub-agent's
//! `meta.json` launched it with. The Kimi Code CLI (TypeScript) that replaced kimi-cli keeps a different
//! format in `~/.kimi-code`, read by [`crate::kimi_code`].

use anyhow::Context;
use md5::{Digest, Md5};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Kimi(harness))
}

struct Kimi(&'static str);

impl TraceAdapter for Kimi {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.0.into(),
            name: self.0.into(),
            harness: self.0.into(),
            unobservable: [
                "model per call (the default model in config.toml; a session started with --model shows the default)",
                "cost (estimated from list prices)",
                "context compaction as a call of its own (folded into the step's model call)",
                "background shell commands (the call that starts one succeeds; its outcome arrives later as a notification)",
                "prompt, message, thinking and tool input/output text (excluded by design)",
                "host",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, _raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        anyhow::bail!("{} sessions are read from the share folder (~/.kimi), not line by line", self.0)
    }
}

/// Events for every session whose wire files changed after `since` (epoch milliseconds of the newest
/// modification time), and the newest change seen. `path` is the share folder or its `sessions` folder.
pub fn read(path: &Path, since: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    let share = if path.ends_with("sessions") { path.parent().unwrap_or(path) } else { path };
    anyhow::ensure!(share.join("sessions").is_dir(), "{} has no sessions folder; point it at ~/.kimi", share.display());
    let work_dirs = work_dirs(&share.join("kimi.json"));
    let models = Models::load(&share.join("config.toml"));
    let mut newest = since;
    let mut events = Vec::new();
    for group in dirs(&share.join("sessions")) {
        for dir in dirs(&group) {
            let main = dir.join("wire.jsonl");
            if !main.is_file() {
                continue;
            }
            let subs: Vec<PathBuf> = dirs(&dir.join("subagents")).into_iter().filter(|d| d.join("wire.jsonl").is_file()).collect();
            let changed = std::iter::once(main.clone()).chain(subs.iter().map(|d| d.join("wire.jsonl"))).map(|p| mtime_ms(&p)).max().unwrap_or(0);
            if changed <= since {
                continue;
            }
            newest = newest.max(changed);
            let sid = name(&dir);
            let cwd = work_dirs.get(&name(&group)).cloned().unwrap_or_default();
            session(&mut events, harness, &sid, &cwd, &dir, &subs, &models).with_context(|| format!("reading {}", dir.display()))?;
        }
    }
    Ok((events, newest))
}

fn session(events: &mut Vec<CanonicalEvent>, harness: &str, sid: &str, cwd: &str, dir: &Path, subs: &[PathBuf], models: &Models) -> anyhow::Result<()> {
    let records = wire(&dir.join("wire.jsonl"))?;
    let Some(&(start, ..)) = records.first() else { return Ok(()) };
    // The call that started each sub-agent: a foreground agent's events reach the parent wire wrapped in
    // `SubagentEvent`, a background agent is a task whose spec names the call.
    let mut spawned_by: HashMap<String, String> = HashMap::new();
    for (_, kind, p, _) in &records {
        if kind == "SubagentEvent"
            && let (Some(agent), Some(call)) = (p["agent_id"].as_str(), p["parent_tool_call_id"].as_str())
        {
            spawned_by.entry(agent.to_owned()).or_insert_with(|| call.to_owned());
        }
    }
    for task in dirs(&dir.join("tasks")) {
        let spec = read_json(&task.join("spec.json"));
        if let (Some(agent), Some(call)) = (spec["kind_payload"]["agent_id"].as_str(), spec["tool_call_id"].as_str()) {
            spawned_by.entry(agent.to_owned()).or_insert_with(|| call.to_owned());
        }
    }

    let mut e = Emit { source: SourceId(format!("{harness}:{sid}")), session: SessionId(sid.to_owned()), observed: system_now_ns(), seq: 0, out: Vec::new() };
    let project = Path::new(cwd).file_name().and_then(|n| n.to_str()).unwrap_or(cwd);
    let body = EventBody::SessionStarted {
        project_id: project.to_owned(),
        working_directory: cwd.to_owned(),
        source_adapter: harness.to_owned(),
        harness: harness.to_owned(),
        host: String::new(),
        labels: Attributes::new(),
    };
    e.push(format!("session:{sid}"), start, None, body);
    let top = span("agent", sid);
    e.start(&top, start, None, SpanKind::Agent, harness, |s| s.agent_name = Some("main".into()));
    let mut run = Run { sid, own: sid.to_owned(), top, subagent: false, model: models.get(None), turn: None, llm: None, interrupted: false, turns: 0 };
    run.feed(&mut e, &records);
    events.append(&mut e.out);

    for sub in subs {
        let agent = name(sub);
        let records = wire(&sub.join("wire.jsonl"))?;
        let Some(&(start, ..)) = records.first() else { continue };
        let meta = read_json(&sub.join("meta.json"));
        let own = format!("{sid}:{agent}");
        let mut e = Emit { source: SourceId(format!("{harness}:{sid}/{agent}")), session: SessionId(sid.to_owned()), observed: system_now_ns(), seq: 0, out: Vec::new() };
        let top = span("subagent", &own);
        let under = spawned_by.get(&agent).map_or_else(|| span("agent", sid), |call| span("tool", &format!("{sid}:{call}")));
        let kind = meta["subagent_type"].as_str().unwrap_or("subagent").to_owned();
        e.start(&top, start, Some(&under), SpanKind::Subagent, &kind, |s| s.agent_name = Some(kind.clone()));
        let mut run = Run { sid, own, top, subagent: true, model: models.get(meta["launch_spec"]["effective_model"].as_str()), turn: None, llm: None, interrupted: false, turns: 0 };
        run.feed(&mut e, &records);
        events.append(&mut e.out);
    }
    Ok(())
}

/// One wire file's turns, steps and tool calls.
struct Run<'a> {
    /// The root session, which tool span ids are scoped to.
    sid: &'a str,
    /// The session, or `<session>:<agent id>` for a sub-agent, which turn and step span ids are scoped to.
    own: String,
    top: SpanId,
    subagent: bool,
    model: Option<(String, String)>,
    turn: Option<SpanId>,
    llm: Option<SpanId>,
    interrupted: bool,
    turns: u64,
}

impl Run<'_> {
    fn feed(&mut self, e: &mut Emit, records: &[(Ns, String, Value, usize)]) {
        for (at, kind, p, line) in records {
            let at = *at;
            match kind.as_str() {
                "TurnBegin" => {
                    // A turn the user interrupted has no `TurnEnd`.
                    self.end_turn(e, at, SpanStatus::Cancelled);
                    self.turns += 1;
                    let turn = span("turn", &format!("{}:{line}", self.own));
                    e.start(&turn, at, Some(&self.top), SpanKind::Turn, &format!("turn {}", self.turns), |_| {});
                    self.turn = Some(turn);
                    self.interrupted = false;
                }
                "StepBegin" => {
                    self.end_llm(e, at, SpanStatus::Succeeded, None);
                    let Some(turn) = self.turn.clone() else { continue };
                    let llm = span("llm", &format!("{}:{line}", self.own));
                    let (model, provider) = self.model.clone().unzip();
                    e.start(&llm, at, Some(&turn), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                        s.model = model.clone();
                        s.provider = provider;
                    });
                    self.llm = Some(llm);
                }
                "StatusUpdate" if p["token_usage"].is_object() => {
                    let Some(llm) = &self.llm else { continue };
                    let u = &p["token_usage"];
                    let usage = Usage {
                        tokens_input: u["input_other"].as_u64(),
                        tokens_output: u["output"].as_u64(),
                        cache_read: u["input_cache_read"].as_u64(),
                        cache_write: u["input_cache_creation"].as_u64(),
                        cost_usd_micros: None,
                        cost_provenance: CostProvenance::Unknown,
                    };
                    e.push(format!("{llm}:usage"), at, Some(llm), EventBody::UsageReported { usage, is_final: true });
                    self.end_llm(e, at, SpanStatus::Succeeded, None);
                }
                "StepInterrupted" => {
                    self.interrupted = true;
                    self.end_llm(e, at, SpanStatus::Failed, Some("step interrupted by the user or an error"));
                }
                "ToolCall" => {
                    let (Some(turn), Some(call), Some(name)) = (&self.turn, p["id"].as_str(), p["function"]["name"].as_str()) else { continue };
                    let tool = span("tool", &format!("{}:{call}", self.sid));
                    e.start(&tool, at, Some(turn), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                }
                "ToolResult" => {
                    let Some(call) = p["tool_call_id"].as_str() else { continue };
                    let tool = span("tool", &format!("{}:{call}", self.sid));
                    let (status, error) = match p["return_value"]["is_error"].as_bool() {
                        Some(true) => (SpanStatus::Failed, Some("tool reported an error (output not captured)")),
                        _ => (SpanStatus::Succeeded, None),
                    };
                    e.end(format!("end:{tool}"), &tool, at, status, error);
                }
                "TurnEnd" => {
                    self.end_llm(e, at, SpanStatus::Succeeded, None);
                    let status = if self.interrupted { SpanStatus::Failed } else { SpanStatus::Succeeded };
                    self.end_turn(e, at, status);
                    if self.subagent {
                        // Ends with its first run; a resumed sub-agent's later turns nest under it all the same.
                        e.end(format!("end:{}", self.top), &self.top, at, status, None);
                    }
                }
                _ => {}
            }
        }
    }

    fn end_llm(&mut self, e: &mut Emit, at: Ns, status: SpanStatus, error: Option<&str>) {
        if let Some(llm) = self.llm.take() {
            e.end(format!("end:{llm}"), &llm, at, status, error);
        }
    }

    fn end_turn(&mut self, e: &mut Emit, at: Ns, status: SpanStatus) {
        self.end_llm(e, at, status, None);
        if let Some(turn) = self.turn.take() {
            e.end(format!("end:{turn}"), &turn, at, status, None);
        }
    }
}

/// A wire file's messages: when, type, payload, line number. The `metadata` header and unreadable lines are skipped.
fn wire(path: &Path) -> anyhow::Result<Vec<(Ns, String, Value, usize)>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(text
        .lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let v: Value = serde_json::from_str(line).ok()?;
            let at = v["timestamp"].as_f64().map(|s| (s * 1e9).round() as Ns).filter(|t| VALID_TIME_NS.contains(t))?;
            let m = &v["message"];
            Some((at, m["type"].as_str()?.to_owned(), m["payload"].clone(), i + 1))
        })
        .collect())
}

/// Session folder name to work dir: kimi-cli names each folder by the md5 of the work dir's path,
/// prefixed `<kaos>_` for a work dir outside the local machine.
fn work_dirs(kimi_json: &Path) -> HashMap<String, String> {
    let meta = read_json(kimi_json);
    meta["work_dirs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let path = w["path"].as_str()?;
            let hash = format!("{:x}", Md5::digest(path.as_bytes()));
            let folder = match w["kaos"].as_str() {
                Some("local") | None => hash,
                Some(kaos) => format!("{kaos}_{hash}"),
            };
            Some((folder, path.to_owned()))
        })
        .collect()
}

/// `config.toml`'s model aliases: `[models.<alias>]` with `model` and `provider`, and `default_model`.
struct Models(toml::Table);

impl Models {
    fn load(path: &Path) -> Self {
        Self(std::fs::read_to_string(path).ok().and_then(|t| t.parse().ok()).unwrap_or_default())
    }

    /// The model and provider of `alias`, or of the default model.
    fn get(&self, alias: Option<&str>) -> Option<(String, String)> {
        let alias = alias.or_else(|| self.0.get("default_model")?.as_str())?;
        let m = self.0.get("models")?.get(alias)?;
        let s = |k: &str| m.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        Some((s("model"), s("provider"))).filter(|(model, _)| !model.is_empty())
    }
}

pub(crate) fn read_json(p: &Path) -> Value {
    std::fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

pub(crate) fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

pub(crate) fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// The subfolders of `dir`, sorted; nothing if it is missing.
pub(crate) fn dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    out.sort();
    out
}
