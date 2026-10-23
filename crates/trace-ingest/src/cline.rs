//! Cline and Roo Code (its fork) tasks to canonical events. Both keep one folder per task under the VS Code
//! extension's global storage, `globalStorage/saoudrizwan.claude-dev/tasks/<task>/` for Cline and
//! `globalStorage/rooveterinaryinc.roo-cline/tasks/<task>/` for Roo (the Cline 3.x CLI uses
//! `~/.cline/data/tasks/`, the Roo CLI `~/.vscode-mock/global-storage/tasks/`). [`read`] takes that
//! `tasks` folder, its parent, or one task folder. Metadata only: no prompt, message, reasoning, command
//! or tool text is copied.
//!
//! A task's `ui_messages.json` is the chat as the extension shows it, a JSON array rewritten in place
//! after every change, so a task is re-read whole whenever that file's mtime moves. Messages carry only
//! the epoch-millisecond time they were first shown, and none has an id, so span ids derive from the task
//! id and that time.
//!
//! Spans: `agent:<task>` root; `turn:` from the task's first message, and from a user reply once the
//! previous turn completed (`completion_result`) or the task was resumed; `llm:` per `api_req_started`,
//! whose JSON text holds the request's tokens and the extension's own cost estimate once it finishes;
//! `tool:` per tool, command, MCP, browser or sub-agent ask/say. A model call ends when its first tool is
//! shown (tools run as the reply streams), or with its last message; a tool ends when the next tool or
//! model call starts. Cline stamps each message with its model (`modelInfo`, or `task_metadata.json`'s
//! `model_usage` before that); Roo records no model at all.
//!
//! A command's exit code is only in the tool result sent back to the model, in
//! `api_conversation_history.json`: its status line ("Command failed with exit code 1." in Cline,
//! "Exit code: 1" in Roo) marks the command failed; nothing else of that text is kept.
//!
//! Sub-agents: Roo's `new_task` delegates to a child task whose `history_item.json` names its
//! `parentTaskId`. Its events join the root task's session inside `subagent:<child>`, under the parent's
//! `newTask` tool call that was shown last before the child began. Cline's `use_subagents` runs sub-agents
//! in process and records only each one's totals: a `subagent:` span per sub-agent under the call, holding
//! one aggregate `llm:` span with those totals. Cline's `new_task` starts an unlinked task, so it stays a
//! separate session.
//!
//! Cline 4.x's SDK (its CLI, and the extension's SDK bundle) keeps sessions instead, read by [`sdk`]:
//! `~/.cline/data/sessions/<session>/` (or `data/db/<session>/` behind its SQLite store) holds the
//! `<session>.json` manifest and `<session>.messages.json`, the provider-format conversation rewritten
//! whole after each step. [`read`] also takes the `data` folder, `sessions`, or one session's folder.

use serde_json::{Value, json};
use std::path::Path;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, span};

pub const SNAPSHOT: bool = true;

/// Only [`read`] produces events; the adapter exists for what it discloses.
pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Info(harness))
}

struct Info(&'static str);

impl TraceAdapter for Info {
    fn source_info(&self) -> SourceInfo {
        let mut unobservable = vec![
            "LLM call end in tasks (taken from its first tool, or its last message)",
            "tool end in tasks (taken from the next tool or model call)",
            "LLM call start in SDK sessions (taken from the message before the reply)",
            "tool start in SDK sessions (taken from the reply that called it)",
            "cost provenance (the extension's own estimate from model prices)",
            "failure of task tools other than commands, unless the extension shows an error",
            "per-call timing, model calls and tool calls of Cline 3.x's in-process sub-agents (only their totals)",
            "which spawn call started an SDK sub-agent when several start at once (paired by order)",
            "project of tasks whose history entry lives only in VS Code's state database",
            "prompt, message, reasoning, command and tool input/output text (excluded by design)",
            "host",
        ];
        if self.0 == "roo" {
            unobservable.push("model (Roo records none per task or message)");
        }
        SourceInfo { source_id: self.0.into(), name: self.0.into(), harness: self.0.into(), unobservable: unobservable.into_iter().map(str::to_owned).collect() }
    }

    fn normalize(&self, _raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        Ok(Vec::new())
    }
}

/// Events for every task or SDK session whose messages file changed after `since_ms` (its mtime, epoch
/// milliseconds), and the newest change read.
// ponytail: a write in the same millisecond as the previous poll's newest waits for the task's next change.
pub fn read(path: &Path, since_ms: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.is_dir(), "{} is not a {harness} tasks or sessions folder, nor one task's or session's folder", path.display());
    let (tasks, only) = if path.join("ui_messages.json").is_file() {
        (path.parent().unwrap_or(path).to_owned(), path.file_name().and_then(|n| n.to_str()))
    } else if path.join("tasks").is_dir() {
        (path.join("tasks"), None)
    } else {
        (path.to_owned(), None)
    };
    let mut ids: Vec<String> = std::fs::read_dir(&tasks)?.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|id| only.is_none_or(|o| o == id)).collect();
    ids.sort();
    let mut newest = since_ms;
    let mut events = Vec::new();
    for id in ids {
        let changed = mtime_ms(&tasks.join(&id).join("ui_messages.json"));
        if changed.is_none_or(|m| m <= since_ms) {
            continue;
        }
        // A file caught mid-rewrite fails to parse; the cursor stays put so the next poll reads it whole.
        let Some(msgs) = read_json(&tasks.join(&id).join("ui_messages.json")).and_then(|v| v.as_array().cloned()) else { continue };
        newest = newest.max(changed.unwrap_or(since_ms));
        events.extend(Task::load(&tasks, &id, msgs, harness).events());
    }
    for dir in sdk::sessions(path) {
        if let Some((more, changed)) = sdk::read_session(&dir, since_ms, harness) {
            newest = newest.max(changed);
            events.extend(more);
        }
    }
    Ok((events, newest))
}

struct Task<'a> {
    tasks: &'a Path,
    id: &'a str,
    harness: &'static str,
    msgs: Vec<Value>,
    /// Roo's per-task history entry: `workspace`, `parentTaskId`.
    history: Value,
    /// Cline's `task_metadata.json`: `model_usage` for messages that predate `modelInfo`.
    metadata: Value,
    /// Each command's exit status from the model-facing tool results, by the time its message was shown.
    command_failed: std::collections::HashMap<i64, bool>,
}

impl<'a> Task<'a> {
    fn load(tasks: &'a Path, id: &'a str, msgs: Vec<Value>, harness: &'static str) -> Self {
        let dir = tasks.join(id);
        let api = read_json(&dir.join("api_conversation_history.json")).unwrap_or_default();
        let command_failed = command_statuses(&msgs, &api);
        Task { tasks, id, harness, msgs, history: history_item(tasks, id), metadata: read_json(&dir.join("task_metadata.json")).unwrap_or_default(), command_failed }
    }

    fn events(self) -> Vec<CanonicalEvent> {
        let Some(first) = self.msgs.first().and_then(ts) else { return Vec::new() };
        let mut root = self.id.to_owned();
        let mut parent = None;
        // Bounded, so a corrupt cycle cannot hang the collector.
        for _ in 0..64 {
            let Some(up) = history_item(self.tasks, &root)["parentTaskId"].as_str().map(str::to_owned) else { break };
            parent.get_or_insert_with(|| up.clone());
            root = up;
        }
        let top = if parent.is_some() { span("subagent", self.id) } else { span("agent", self.id) };
        let mut w = Walk {
            e: Emit { source: SourceId(format!("{}:{}", self.harness, self.id)), session: SessionId(root.clone()), observed: system_now_ns(), seq: 0, out: Vec::new() },
            task: &self,
            top: top.clone(),
            subagent: parent.is_some(),
            turn: None,
            turns: 0,
            llm: None,
            tool: None,
            llm_model: None,
            last: first,
        };
        if let Some(parent) = parent {
            let under = spawning_call(self.tasks, &parent, first).unwrap_or_else(|| if parent == root { span("agent", &root) } else { span("subagent", &parent) });
            w.e.start(&top, first, Some(&under), SpanKind::Subagent, "subagent", |s| s.agent_name = Some("subagent".into()));
        } else {
            let cwd = self.workspace().unwrap_or_default();
            let project = Path::new(&cwd).file_name().and_then(|n| n.to_str()).unwrap_or(&cwd).to_owned();
            let body = EventBody::SessionStarted { project_id: project, working_directory: cwd, source_adapter: self.harness.into(), harness: self.harness.into(), host: String::new(), labels: Attributes::new() };
            w.e.push(format!("session:{}", self.id), first, None, body);
            w.e.start(&top, first, None, SpanKind::Agent, self.harness, |s| s.agent_name = Some("main".into()));
        }
        for (i, m) in self.msgs.iter().enumerate() {
            if let Some(t) = ts(m) {
                w.message(i, m, t);
            }
        }
        // A finished request at the end of the chat ends with its last message; an unfinished one stays open.
        if w.llm.as_ref().is_some_and(|l| l.info.get("tokensIn").is_some() || l.info.get("cost").is_some()) {
            w.end_llm(w.last, None);
        }
        w.e.out
    }

    /// Roo's history entry names the workspace; Cline keeps its history beside the tasks folder.
    fn workspace(&self) -> Option<String> {
        if let Some(w) = self.history["workspace"].as_str() {
            return Some(w.to_owned());
        }
        let history = read_json(&self.tasks.parent()?.join("state").join("taskHistory.json"))?;
        history.as_array()?.iter().find(|h| h["id"] == self.id)?["cwdOnTaskInitialization"].as_str().map(str::to_owned)
    }

    fn model(&self, m: &Value, at: i64) -> (Option<String>, Option<String>) {
        let info = &m["modelInfo"];
        if let Some(id) = info["modelId"].as_str() {
            return (Some(id.to_owned()), info["providerId"].as_str().map(str::to_owned));
        }
        let used = self.metadata["model_usage"].as_array().into_iter().flatten().rev().find(|u| u["ts"].as_i64().is_some_and(|t| t <= at));
        used.map_or((None, None), |u| (u["model_id"].as_str().map(str::to_owned), u["model_provider_id"].as_str().map(str::to_owned)))
    }
}

struct Llm {
    span: SpanId,
    start: Ns,
    /// The `api_req_started` JSON: tokens, cost and cancel reason once the request finished.
    info: Value,
    model: Option<String>,
}

struct Tool {
    span: SpanId,
    start: Ns,
    error: Option<&'static str>,
    /// Cline's latest `subagent` status for a `use_subagents` call.
    subagents: Value,
}

struct Walk<'a, 'b> {
    e: Emit,
    task: &'b Task<'a>,
    top: SpanId,
    subagent: bool,
    turn: Option<SpanId>,
    turns: u64,
    llm: Option<Llm>,
    tool: Option<Tool>,
    /// The model of the last finished call: Cline's sub-agents run on it.
    llm_model: Option<String>,
    last: Ns,
}

impl Walk<'_, '_> {
    fn message(&mut self, i: usize, m: &Value, at: Ns) {
        let id = format!("{}:{}", self.task.id, m["ts"]);
        let kind = m["say"].as_str().or_else(|| m["ask"].as_str()).unwrap_or_default();
        let ask = m["type"] == "ask";
        match kind {
            _ if i == 0 => self.start_turn(&id, at),
            "user_feedback" if self.turn.is_none() => self.start_turn(&id, at),
            "resume_task" | "resume_completed_task" if ask => {
                let last = self.last;
                self.end_llm(last, None);
                self.close_tool(last);
                self.end_turn(last, SpanStatus::Cancelled);
            }
            "api_req_started" => {
                self.close_tool(at);
                let last = self.last;
                self.end_llm(last, None);
                let parent = self.ensure_turn(&id, at);
                let llm = span("llm", &id);
                let (model, provider) = self.task.model(m, m["ts"].as_i64().unwrap_or_default());
                self.e.start(&llm, at, Some(&parent), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                    s.model = model.clone();
                    s.provider = provider;
                });
                let info = m["text"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
                self.llm = Some(Llm { span: llm, start: at, info, model });
            }
            "tool" | "command" | "use_mcp_server" | "browser_action_launch" | "use_subagents" => {
                self.end_llm(at, None);
                self.close_tool(at);
                let parent = self.ensure_turn(&id, at);
                let detail: Value = m["text"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
                let name = match kind {
                    "tool" => detail["tool"].as_str().unwrap_or("tool"),
                    "command" => "execute_command",
                    "use_mcp_server" => detail["toolName"].as_str().or_else(|| detail["type"].as_str()).unwrap_or("use_mcp_server"),
                    "browser_action_launch" => "browser_action",
                    other => other,
                };
                let tool = span("tool", &id);
                self.e.start(&tool, at, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                let failed = kind == "command" && m["ts"].as_i64().and_then(|t| self.task.command_failed.get(&t)) == Some(&true);
                self.tool = Some(Tool { span: tool, start: at, error: failed.then_some("command exited non-zero (output not captured)"), subagents: Value::Null });
            }
            "completion_result" | "followup" | "plan_mode_respond" | "act_mode_respond" => {
                self.end_llm(at, None);
                self.close_tool(at);
                if kind == "completion_result" && !ask {
                    self.end_turn(at, SpanStatus::Succeeded);
                }
            }
            "error" | "diff_error" => {
                if let Some(t) = self.tool.as_mut() {
                    t.error = Some("tool reported an error (text not captured)");
                }
            }
            "api_req_failed" if ask => self.end_llm(at, Some("request failed (text not captured)")),
            "subagent" => {
                if let Some(t) = self.tool.as_mut() {
                    t.subagents = m["text"].as_str().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
                }
            }
            _ => {}
        }
        self.last = self.last.max(at);
    }

    fn start_turn(&mut self, id: &str, at: Ns) {
        let last = self.last;
        self.end_turn(last, SpanStatus::Succeeded);
        self.turns += 1;
        let turn = span("turn", id);
        self.e.start(&turn, at, Some(&self.top), SpanKind::Turn, &format!("turn {}", self.turns), |_| {});
        self.turn = Some(turn);
    }

    /// The open turn, started here when activity resumes without a user message.
    fn ensure_turn(&mut self, id: &str, at: Ns) -> SpanId {
        if self.turn.is_none() {
            self.start_turn(id, at);
        }
        self.turn.clone().expect("started above")
    }

    /// Ends the open turn; a sub-agent's first completed turn also ends its span.
    fn end_turn(&mut self, at: Ns, status: SpanStatus) {
        let Some(turn) = self.turn.take() else { return };
        self.e.end(format!("end:{turn}"), &turn, at, status, None);
        if self.subagent && status == SpanStatus::Succeeded {
            self.e.end(format!("end:{}", self.top), &self.top, at, status, None);
            self.subagent = false;
        }
    }

    fn end_llm(&mut self, at: Ns, error: Option<&str>) {
        let Some(l) = self.llm.take() else { return };
        let at = at.max(l.start);
        let usage = usage(&l.info, &l.info["cost"]);
        let reported = usage.is_some();
        if let Some(usage) = usage {
            self.e.push(format!("{}:usage", l.span), at, Some(&l.span), EventBody::UsageReported { usage, is_final: true });
        }
        let status = match (error, l.info["cancelReason"].as_str()) {
            (Some(_), _) => SpanStatus::Failed,
            (None, Some("user_cancelled")) => SpanStatus::Cancelled,
            (None, Some(_)) => SpanStatus::Failed,
            // A request that never reported usage was abandoned before it finished.
            (None, None) if !reported => SpanStatus::Cancelled,
            (None, None) => SpanStatus::Succeeded,
        };
        self.e.end(format!("end:{}", l.span), &l.span, at, status, error.or_else(|| l.info["streamingFailedMessage"].as_str().map(|_| "streaming failed (text not captured)")));
        self.llm_model = l.model;
    }

    fn close_tool(&mut self, at: Ns) {
        let Some(t) = self.tool.take() else { return };
        let at = at.max(t.start);
        for (i, item) in t.subagents["items"].as_array().into_iter().flatten().enumerate() {
            let sub = SpanId(format!("{}:{i}", t.span.0.replacen("tool:", "subagent:", 1)));
            self.e.start(&sub, t.start, Some(&t.span), SpanKind::Subagent, "subagent", |s| s.agent_name = Some("subagent".into()));
            let llm = SpanId(format!("{}:{i}", t.span.0.replacen("tool:", "llm:", 1)));
            let model = self.llm_model.clone();
            self.e.start(&llm, t.start, Some(&sub), SpanKind::Llm, model.as_deref().unwrap_or("llm call"), |s| {
                s.model = model.clone();
                s.attributes.insert("aggregate".into(), json!(true));
            });
            let totals = json!({"tokensIn": item["inputTokens"], "tokensOut": item["outputTokens"]});
            if let Some(usage) = usage(&totals, &item["totalCost"]) {
                self.e.push(format!("{llm}:usage"), at, Some(&llm), EventBody::UsageReported { usage, is_final: true });
            }
            let status = if item["status"] == "failed" { SpanStatus::Failed } else { SpanStatus::Succeeded };
            self.e.end(format!("end:{llm}"), &llm, at, status, None);
            self.e.end(format!("end:{sub}"), &sub, at, status, None);
        }
        let status = if t.error.is_some() { SpanStatus::Failed } else { SpanStatus::Succeeded };
        self.e.end(format!("end:{}", t.span), &t.span, at, status, t.error);
    }
}

fn usage(info: &Value, cost: &Value) -> Option<Usage> {
    if info["tokensIn"].is_null() && info["tokensOut"].is_null() {
        return None;
    }
    let cost = cost.as_f64().filter(|c| *c > 0.).map(|c| (c * 1e6).round() as u64);
    Some(Usage {
        tokens_input: info["tokensIn"].as_u64(),
        tokens_output: info["tokensOut"].as_u64(),
        cache_read: info["cacheReads"].as_u64(),
        cache_write: info["cacheWrites"].as_u64(),
        cost_usd_micros: cost,
        cost_provenance: if cost.is_some() { CostProvenance::Estimate } else { CostProvenance::Unknown },
    })
}

/// Each command message's exit status: the status lines of the model-facing tool results, in order, in
/// the first API user message written at or after the command was shown (the next request carries them).
// ponytail: matched by position, so a command that produced no status line (denied, still running) can
// shift a later command's status within the same request; match on tool ids if that shows.
fn command_statuses(msgs: &[Value], api: &Value) -> std::collections::HashMap<i64, bool> {
    let results: Vec<(i64, Vec<Option<bool>>)> = api
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| Some((m["ts"].as_i64()?, m["content"].as_array()?.iter().filter_map(status_line).collect())))
        .collect();
    let mut used = vec![0; results.len()];
    let mut out = std::collections::HashMap::new();
    for t in msgs.iter().filter(|m| m["ask"] == "command" || m["say"] == "command").filter_map(|m| m["ts"].as_i64()) {
        let Some(j) = results.iter().position(|(at, _)| *at >= t) else { continue };
        if let Some(Some(failed)) = results[j].1.get(used[j]) {
            out.insert(t, *failed);
        }
        used[j] += 1;
    }
    out
}

/// The last command status line in a content block, if it has one: Some(failed), or None when the
/// harness could not tell. A `tool_result` block nests its text.
fn status_line(block: &Value) -> Option<Option<bool>> {
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => block["text"].as_str()?.to_owned(),
    };
    text.lines().rev().find_map(|l| {
        let l = l.trim();
        if l.starts_with("Command failed with exit code") || l.starts_with("Command terminated by signal") || l.starts_with("Process terminated by signal") {
            Some(Some(true))
        } else if l.starts_with("Command executed successfully") {
            Some(Some(false))
        } else if let Some(code) = l.strip_prefix("Exit code: ") {
            Some(code.parse::<i64>().ok().map(|c| c != 0))
        } else if l == "Command executed." || l.starts_with("Command is still running") {
            Some(None)
        } else {
            None
        }
    })
}

/// The parent's `newTask` tool call shown last before the child's first message.
fn spawning_call(tasks: &Path, parent: &str, child_start: Ns) -> Option<SpanId> {
    let msgs = read_json(&tasks.join(parent).join("ui_messages.json"))?;
    let call = msgs.as_array()?.iter().rev().filter(|m| m["ask"] == "tool" || m["say"] == "tool").filter(|m| ts(m).is_some_and(|t| t <= child_start)).find(|m| m["text"].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok()).is_some_and(|d| d["tool"] == "newTask"))?;
    Some(span("tool", &format!("{parent}:{}", call["ts"])))
}

fn history_item(tasks: &Path, id: &str) -> Value {
    read_json(&tasks.join(id).join("history_item.json")).unwrap_or_default()
}

fn ts(m: &Value) -> Option<Ns> {
    m["ts"].as_i64().and_then(|t| t.checked_mul(1_000_000)).filter(|t| VALID_TIME_NS.contains(t))
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

fn mtime_ms(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).and_then(|m| m.modified()).ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// Cline 4.x SDK sessions. Each messages file is `{agent, sessionId, origin, messages}`, the messages in the
/// provider's own shape (`role`, `content` blocks with `tool_use` and `tool_result`) plus `id`, `ts` (epoch
/// ms, when the message was written) and, on a reply, `modelInfo {id, provider}` and `metrics {inputTokens,
/// outputTokens, cacheReadTokens, cacheWriteTokens, cost}`.
///
/// Spans: `turn:` per user message that is not only tool results; `llm:` per assistant message, from the
/// message before it (the prompt, or the last tool result in) to its own `ts`; `tool:<session>:<call id>`
/// per `tool_use`, from its reply to the message that carries its `tool_result`, failed when that result is
/// `is_error` or reports `success: false` (a command's non-zero exit). A reply without tool calls ends the turn.
///
/// Sub-agents: a sub-agent's file sits beside its root's as `<agent id>.messages.json`, its `origin`
/// naming its `parentThreadId`. Its events join the root session inside `subagent:<its session id>`, under
/// the parent's earliest unclaimed `spawn_agent` call made before the sub-agent's first message (spawn
/// calls carry no child id, so they pair by order).
pub(crate) mod sdk {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// The session folders at `path`: itself, or those in it, its `sessions` or its `db` folder.
    pub fn sessions(path: &Path) -> Vec<PathBuf> {
        let own = |d: &Path| d.file_name().and_then(|n| n.to_str()).is_some_and(|n| d.join(format!("{n}.messages.json")).is_file());
        if own(path) {
            return vec![path.to_owned()];
        }
        let mut out: Vec<PathBuf> = [path.to_owned(), path.join("sessions"), path.join("db")]
            .iter()
            .flat_map(|base| std::fs::read_dir(base).into_iter().flatten().flatten().map(|e| e.path()))
            .filter(|p| own(p))
            .collect();
        out.sort();
        out
    }

    struct Thread {
        id: String,
        parent: Option<String>,
        msgs: Vec<Value>,
    }

    /// The session's events if any of its messages files changed after `since_ms`, with that change; None
    /// when unchanged, or when a file is caught mid-rewrite (the next poll reads the session whole).
    pub fn read_session(dir: &Path, since_ms: i64, harness: &'static str) -> Option<(Vec<CanonicalEvent>, i64)> {
        let root = dir.file_name()?.to_str()?.to_owned();
        let files: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.to_str().is_some_and(|s| s.ends_with(".messages.json"))).collect();
        let changed = files.iter().filter_map(|f| mtime_ms(f)).max()?;
        if changed <= since_ms {
            return None;
        }
        let mut threads = Vec::new();
        for f in &files {
            let v = read_json(f)?;
            let id = v["sessionId"].as_str().unwrap_or(&root).to_owned();
            let parent = v["origin"]["parentThreadId"].as_str().filter(|_| id != root).map(str::to_owned);
            threads.push(Thread { id, parent, msgs: v["messages"].as_array().cloned().unwrap_or_default() });
        }
        threads.sort_by_key(|t| (t.parent.is_some(), first_ts(t), t.id.clone()));
        let spawned_by = spawn_calls(&threads);
        let manifest = read_json(&dir.join(format!("{root}.json"))).unwrap_or_default();
        let mut events = Vec::new();
        for t in &threads {
            events.extend(thread_events(t, &root, spawned_by.get(&t.id), &manifest, harness));
        }
        Some((events, changed))
    }

    fn first_ts(t: &Thread) -> Option<Ns> {
        t.msgs.iter().find_map(ts)
    }

    /// Pairs each sub-agent with the spawn call that started it.
    fn spawn_calls(threads: &[Thread]) -> HashMap<String, SpanId> {
        let mut calls: HashMap<&str, Vec<(Ns, String)>> = HashMap::new();
        for t in threads {
            for m in t.msgs.iter().filter(|m| m["role"] == "assistant") {
                let Some(at) = ts(m) else { continue };
                for b in blocks(m).filter(|b| b["type"] == "tool_use" && b["name"] == "spawn_agent") {
                    calls.entry(&t.id).or_default().push((at, b["id"].as_str().unwrap_or_default().to_owned()));
                }
            }
        }
        let mut out = HashMap::new();
        // Threads are sorted by first message, so sub-agents claim calls in the order they started.
        for t in threads {
            let (Some(parent), Some(start)) = (t.parent.as_deref(), first_ts(t)) else { continue };
            let Some(free) = calls.get_mut(parent) else { continue };
            if let Some(i) = free.iter().position(|(at, _)| *at <= start) {
                let (_, call) = free.remove(i);
                out.insert(t.id.clone(), span("tool", &format!("{parent}:{call}")));
            }
        }
        out
    }

    fn blocks(m: &Value) -> impl Iterator<Item = &Value> {
        m["content"].as_array().into_iter().flatten()
    }

    fn thread_events(t: &Thread, root: &str, spawned_by: Option<&SpanId>, manifest: &Value, harness: &'static str) -> Vec<CanonicalEvent> {
        let Some(first) = first_ts(t) else { return Vec::new() };
        // Each tool call's result: when it came back, and whether it failed.
        let mut results: HashMap<&str, (Ns, bool)> = HashMap::new();
        for m in t.msgs.iter().filter(|m| m["role"] == "user") {
            let Some(at) = ts(m) else { continue };
            for b in blocks(m).filter(|b| b["type"] == "tool_result") {
                let failed = b["is_error"] == true || b["content"].as_array().into_iter().flatten().any(|c| c["success"] == false);
                results.insert(b["tool_use_id"].as_str().unwrap_or_default(), (at, failed));
            }
        }
        let sub = t.parent.is_some();
        let top = if sub { span("subagent", &t.id) } else { span("agent", &t.id) };
        let mut e = Emit { source: SourceId(format!("{harness}:{}", t.id)), session: SessionId(root.to_owned()), observed: system_now_ns(), seq: 0, out: Vec::new() };
        if let Some(parent) = &t.parent {
            let under = spawned_by.cloned().unwrap_or_else(|| if parent == root { span("agent", root) } else { span("subagent", parent) });
            e.start(&top, first, Some(&under), SpanKind::Subagent, "subagent", |s| s.agent_name = Some("subagent".into()));
        } else {
            let cwd = manifest["cwd"].as_str().unwrap_or_default().to_owned();
            let project = Path::new(&cwd).file_name().and_then(|n| n.to_str()).unwrap_or(&cwd).to_owned();
            let body = EventBody::SessionStarted { project_id: project, working_directory: cwd, source_adapter: harness.into(), harness: harness.into(), host: String::new(), labels: Attributes::new() };
            e.push(format!("session:{}", t.id), first, None, body);
            e.start(&top, first, None, SpanKind::Agent, harness, |s| s.agent_name = Some("main".into()));
        }
        let (mut turn, mut turns, mut prev, mut sub_open) = (None::<SpanId>, 0, first, sub);
        for m in &t.msgs {
            let Some(at) = ts(m) else { continue };
            let id = format!("{}:{}", t.id, m["id"].as_str().map_or_else(|| m["ts"].to_string(), str::to_owned));
            let prompt = m["role"] == "user" && (m["content"].is_string() || blocks(m).any(|b| b["type"] != "tool_result"));
            if prompt || (m["role"] == "assistant" && turn.is_none()) {
                if let Some(old) = turn.take() {
                    e.end(format!("end:{old}"), &old, prev, SpanStatus::Succeeded, None);
                }
                turns += 1;
                let new = span("turn", &id);
                e.start(&new, if prompt { at } else { prev }, Some(&top), SpanKind::Turn, &format!("turn {turns}"), |_| {});
                turn = Some(new);
            }
            if m["role"] == "assistant" {
                let parent = turn.clone().expect("started above");
                let llm = span("llm", &id);
                let (model, provider) = (m["modelInfo"]["id"].as_str(), m["modelInfo"]["provider"].as_str());
                e.start(&llm, prev.min(at), Some(&parent), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
                    s.model = model.map(str::to_owned);
                    s.provider = provider.map(str::to_owned);
                });
                let x = &m["metrics"];
                let totals = json!({"tokensIn": x["inputTokens"], "tokensOut": x["outputTokens"], "cacheReads": x["cacheReadTokens"], "cacheWrites": x["cacheWriteTokens"]});
                if let Some(usage) = usage(&totals, &x["cost"]) {
                    e.push(format!("{llm}:usage"), at, Some(&llm), EventBody::UsageReported { usage, is_final: true });
                }
                e.end(format!("end:{llm}"), &llm, at, SpanStatus::Succeeded, None);
                let mut called = false;
                for b in blocks(m).filter(|b| b["type"] == "tool_use") {
                    let (Some(call), Some(name)) = (b["id"].as_str(), b["name"].as_str()) else { continue };
                    called = true;
                    let tool = span("tool", &format!("{}:{call}", t.id));
                    e.start(&tool, at, Some(&parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
                    if let Some(&(end, failed)) = results.get(call) {
                        let (status, error) = if failed { (SpanStatus::Failed, Some("tool reported failure (output not captured)")) } else { (SpanStatus::Succeeded, None) };
                        e.end(format!("end:{tool}"), &tool, end.max(at), status, error);
                    }
                }
                if !called {
                    let done = turn.take().expect("started above");
                    e.end(format!("end:{done}"), &done, at, SpanStatus::Succeeded, None);
                    if sub_open {
                        e.end(format!("end:{top}"), &top, at, SpanStatus::Succeeded, None);
                        sub_open = false;
                    }
                }
            }
            prev = prev.max(at);
        }
        // A run that stopped mid-turn: the manifest's final status ends it.
        let stopped = match manifest["status"].as_str() {
            Some("failed") => Some(SpanStatus::Failed),
            Some("cancelled") => Some(SpanStatus::Cancelled),
            _ => None,
        };
        if let (Some(open), Some(status), false) = (turn, stopped, sub) {
            e.end(format!("end:{open}"), &open, prev, status, None);
        }
        e.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cursor is the newest `ui_messages.json` mtime read; a file caught mid-rewrite is skipped without
    /// moving it, so the next poll reads that task whole.
    #[test]
    fn polls_read_changed_tasks_and_wait_out_a_half_written_file() {
        let d = std::env::temp_dir().join(format!("tw-cline-cursor-{}", std::process::id()));
        let file = d.join("tasks/t1/ui_messages.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let msgs = json!([{"ts": 1_792_756_800_000_i64, "type": "say", "say": "task"}, {"ts": 1_792_756_801_000_i64, "type": "say", "say": "api_req_started", "text": "{\"tokensIn\":1,\"tokensOut\":2}"}]);
        std::fs::write(&file, msgs.to_string()).unwrap();

        let (all, cursor) = read(&d, i64::MIN, "roo").unwrap();
        assert!(!all.is_empty() && cursor > 0);
        assert!(read(&d, cursor, "roo").unwrap().0.is_empty(), "nothing changed since the cursor");
        let ids = |v: &[CanonicalEvent]| v.iter().map(|e| e.event_id.0.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&all), ids(&read(&d.join("tasks/t1"), i64::MIN, "roo").unwrap().0), "one task's folder reads the same events");

        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(&file, "[{\"ts\": 1").unwrap();
        let (events, after) = read(&d, cursor, "roo").unwrap();
        assert!(events.is_empty() && after == cursor, "a half-written file waits for the next poll");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
