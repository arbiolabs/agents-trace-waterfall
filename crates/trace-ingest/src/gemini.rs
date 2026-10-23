//! Gemini CLI sessions (`~/.gemini/tmp/<project>/chats/session-<time>-<id>.jsonl`, or under
//! `$GEMINI_CLI_HOME/.gemini` when that is set) to canonical events. Metadata only: no prompt, response,
//! thought or tool payload text is copied.
//!
//! A session file is a log of records: a header (`sessionId`, `projectHash`, `startTime`, `kind`), then
//! messages, `$set` metadata updates, and `$patch` / `$rewindTo` edits of the history. A message is
//! appended again, whole and under the same id, each time it changes: a model reply first without its
//! tool calls, then with them once they finish. Earlier versions wrote the same record as one JSON object
//! rewritten in place (`session-*.json`). Either way the file is read whole whenever it changes
//! ([`read`]), folded to the latest version of each message, and turned into one line for the adapter.
//! History edits are ignored: rewound or compressed messages still cost their tokens and time.
//!
//! Spans: `agent:<session>` root, `turn:<message id>` from a user prompt (the session context Gemini CLI
//! records as a user message before the first prompt joins that prompt's turn) to the last reply or
//! tool call before the next prompt, `llm:<message id>` per model reply with its token counts, from the
//! message before it to the reply's timestamp (written when the stream finished), and `tool:<call id>`
//! from the reply that asked for it to the call's own timestamp (written when it finished). A shell
//! command that exits non-zero is recorded as a success; the `Exit Code:` line in its returned output
//! marks it failed.
//!
//! Sub-agents: each writes its own file, `chats/<parent session>/<agent id>.jsonl` with `kind:
//! "subagent"`. Its model calls and tools join the parent session inside `subagent:<agent id>`, which the
//! parent's tool call starts and ends once it records the agent's id (`agentId`), when the call returns.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;
use trace_adapter_sdk::{RawEvent, SourceInfo, TraceAdapter, system_now_ns};
use trace_core::*;

use crate::emit::{Emit, parse_utc, span};

pub const SNAPSHOT: bool = true;

pub fn adapter(harness: &'static str) -> Box<dyn TraceAdapter> {
    Box::new(Gemini(harness))
}

/// Every session file under `path` (or `path` itself) modified after `since` (epoch milliseconds), and
/// the newest modification seen.
// ponytail: each changed file is re-read whole; fine for sessions of a few MB, tail by offset if not.
pub fn read(path: &Path, since: i64, harness: &'static str) -> anyhow::Result<(Vec<CanonicalEvent>, i64)> {
    anyhow::ensure!(path.exists(), "{} does not exist; point it at ~/.gemini/tmp or a session file", path.display());
    let mut files = Vec::new();
    if path.is_dir() {
        session_files(path, &mut files);
    } else {
        files.push((path.to_owned(), modified_ms(path).unwrap_or(i64::MAX)));
    }
    let newest = files.iter().map(|(_, t)| *t).max().unwrap_or(since).max(since);
    let adapter = Gemini(harness);
    let mut events = Vec::new();
    for (f, _) in files.into_iter().filter(|(_, t)| *t > since) {
        let session = std::fs::read_to_string(&f).ok().and_then(|text| fold(&text, &f));
        let Some(line) = session else {
            anyhow::ensure!(path.is_dir(), "{} holds no Gemini CLI session", f.display());
            continue;
        };
        events.extend(adapter.normalize(RawEvent { line: line.to_string(), observed_at_utc_ns: system_now_ns() })?);
    }
    Ok((events, newest))
}

fn modified_ms(p: &Path) -> Option<i64> {
    let t = std::fs::metadata(p).ok()?.modified().ok()?;
    i64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// `*.jsonl` and legacy `session-*.json` files below `dir`.
fn session_files(dir: &Path, out: &mut Vec<(std::path::PathBuf, i64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for p in entries.flatten().map(|e| e.path()) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if p.is_dir() {
            session_files(&p, out);
        } else if (name.ends_with(".jsonl") || (name.starts_with("session-") && name.ends_with(".json")))
            && let Some(t) = modified_ms(&p)
        {
            out.push((p, t));
        }
    }
}

/// One session file as the adapter's line: its metadata, the latest version of each message in order,
/// the session it joins (a sub-agent's parent, named by its folder) and the project folder.
fn fold(text: &str, path: &Path) -> Option<Value> {
    let mut meta = serde_json::Map::new();
    let mut order: Vec<String> = Vec::new();
    let mut messages: HashMap<String, Value> = HashMap::new();
    let mut add = |m: &Value| {
        let Some(id) = m["id"].as_str() else { return };
        if messages.insert(id.to_owned(), m.clone()).is_none() {
            order.push(id.to_owned());
        }
    };
    let whole = serde_json::from_str::<Value>(text).ok().filter(|v| v["messages"].is_array());
    let records: Vec<Value> = match whole {
        Some(v) => vec![v],
        None => text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect(),
    };
    for r in records {
        let Value::Object(mut r) = r else { continue };
        let header = r.contains_key("sessionId");
        if let Some(Value::Object(set)) = r.remove("$set") {
            r = set;
        } else if !header {
            // A message, or a history edit ($patch, $rewindTo).
            add(&Value::Object(r));
            continue;
        }
        for m in r.remove("messages").as_ref().and_then(Value::as_array).into_iter().flatten() {
            add(m);
        }
        meta.extend(r);
    }
    meta.get("sessionId")?.as_str()?;
    let root = (meta.get("kind").and_then(Value::as_str) == Some("subagent"))
        .then(|| path.parent()?.file_name()?.to_str())
        .flatten()
        .filter(|p| *p != "chats");
    // `<project>/.project_root` names the folder Gemini CLI ran in; older versions keep only its hash.
    let project = path.ancestors().skip(1).take(3).find_map(|d| std::fs::read_to_string(d.join(".project_root")).ok());
    let messages: Vec<Value> = order.into_iter().filter_map(|id| messages.remove(&id)).collect();
    Some(json!({"root": root, "project": project.map(|p| p.trim().to_owned()), "meta": meta, "messages": messages}))
}

struct Gemini(&'static str);

impl TraceAdapter for Gemini {
    fn source_info(&self) -> SourceInfo {
        SourceInfo {
            source_id: self.0.into(),
            name: self.0.into(),
            harness: self.0.into(),
            unobservable: [
                "LLM call start (taken from the message before the reply)",
                "tool execution start (taken from the end of the model call that asked for it)",
                "sub-agents until the call that started them returns (only then does the parent record the agent's id)",
                "sub-agent outcome beyond its tool call's status",
                "cost (Gemini CLI records token counts only)",
                "prompt, response, thought and tool input/output text (excluded by design)",
                "session end (no end marker; idle sessions are reconciled as interrupted)",
                "host",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }

    fn normalize(&self, raw: RawEvent) -> anyhow::Result<Vec<CanonicalEvent>> {
        if raw.line.trim().is_empty() {
            return Ok(Vec::new());
        }
        let v: Value = serde_json::from_str(&raw.line)?;
        let (meta, messages) = (&v["meta"], v["messages"].as_array().map(Vec::as_slice).unwrap_or_default());
        let Some(own) = meta["sessionId"].as_str() else { return Ok(Vec::new()) };
        let ts_of = |m: &Value| m.as_str().and_then(parse_utc).filter(|t| VALID_TIME_NS.contains(t));
        let Some(start) = ts_of(&meta["startTime"]).or_else(|| messages.iter().find_map(|m| ts_of(&m["timestamp"]))) else {
            return Ok(Vec::new());
        };
        let root_session = v["root"].as_str().unwrap_or(own);
        let subagent = root_session != own;
        let top = if subagent { span("subagent", own) } else { span("agent", own) };
        let mut e = Emit { source: SourceId(format!("{}:{own}", self.0)), session: SessionId(root_session.to_owned()), observed: raw.observed_at_utc_ns, seq: 0, out: Vec::new() };

        if !subagent {
            let dir = v["project"].as_str().unwrap_or_default();
            let project = Path::new(dir).file_name().and_then(|n| n.to_str()).unwrap_or(dir);
            let mut labels = Attributes::new();
            if let Some(hash) = meta["projectHash"].as_str() {
                labels.insert("gemini_project_hash".into(), json!(hash));
            }
            let body = EventBody::SessionStarted {
                project_id: project.to_owned(),
                working_directory: dir.to_owned(),
                source_adapter: self.0.into(),
                harness: self.0.into(),
                host: String::new(),
                labels,
            };
            e.push(format!("session:{own}"), start, None, body);
            e.start(&top, start, None, SpanKind::Agent, self.0, |s| s.agent_name = Some("main".into()));
        }

        // The open turn, when it last did something, and whether it called the model yet.
        let mut turn: Option<(SpanId, Ns, bool)> = None;
        let (mut turns, mut prev, mut model) = (0, start, None);
        for m in messages {
            let Some(ts) = ts_of(&m["timestamp"]) else { continue };
            let id = m["id"].as_str().unwrap_or_default();
            match m["type"].as_str() {
                Some("user") if !subagent => {
                    let tool_results = m["content"].as_array().is_some_and(|c| c.iter().any(|p| p.get("functionResponse").is_some()));
                    if !tool_results && turn.as_ref().is_none_or(|t| t.2) {
                        end_turn(&mut e, turn.take());
                        turns += 1;
                        let t = span("turn", id);
                        e.start(&t, ts, Some(&top), SpanKind::Turn, &format!("turn {turns}"), |_| {});
                        turn = Some((t, ts, false));
                    }
                }
                // A reply with neither tokens nor model is one Gemini CLI made up (e.g. acknowledging binary content).
                Some("gemini") if m["tokens"].is_object() || m["model"].is_string() => {
                    model = m["model"].as_str().or(model);
                    let parent = turn.as_ref().map_or_else(|| top.clone(), |t| t.0.clone());
                    let last = reply(&mut e, m, id, &parent, prev.min(ts), ts, model);
                    if let Some(t) = &mut turn {
                        (t.1, t.2) = (t.1.max(last), true);
                    }
                }
                _ => {}
            }
            prev = ts;
        }
        // A reply with no text and no finished tool calls is still waiting for them: the turn goes on.
        let last = messages.last();
        let waiting = last.is_none_or(|m| m["type"] != "gemini" || (m["toolCalls"].as_array().is_none_or(Vec::is_empty) && is_empty(&m["content"])));
        if !waiting {
            end_turn(&mut e, turn);
        }
        Ok(e.out)
    }
}

fn is_empty(content: &Value) -> bool {
    match content {
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => true,
    }
}

/// The turn ends with the last thing it did; a later read that finds more ends it again, later.
fn end_turn(e: &mut Emit, turn: Option<(SpanId, Ns, bool)>) {
    if let Some((t, at, _)) = turn {
        e.end(format!("end:{t}@{at}"), &t, at, SpanStatus::Succeeded, None);
    }
}

/// One model reply and the tool calls it made (with any sub-agent they started). Returns when the
/// last of them finished.
fn reply(e: &mut Emit, m: &Value, id: &str, parent: &SpanId, start: Ns, end: Ns, model: Option<&str>) -> Ns {
    let llm = span("llm", id);
    e.start(&llm, start, Some(parent), SpanKind::Llm, model.unwrap_or("llm call"), |s| {
        s.model = model.map(str::to_owned);
        s.provider = Some("google".into());
        s.attributes.insert("start_inferred".into(), json!(true));
    });
    let t = &m["tokens"];
    if t.is_object() {
        let n = |k: &str| t[k].as_u64();
        // Gemini counts cached tokens inside the prompt, and thinking apart from the output.
        let usage = Usage {
            tokens_input: n("input").map(|i| i.saturating_sub(n("cached").unwrap_or(0)) + n("tool").unwrap_or(0)),
            tokens_output: n("output").map(|o| o + n("thoughts").unwrap_or(0)),
            cache_read: n("cached"),
            cache_write: None,
            cost_usd_micros: None,
            cost_provenance: CostProvenance::Unknown,
        };
        e.push(format!("{llm}:usage"), end, Some(&llm), EventBody::UsageReported { usage, is_final: true });
    }
    e.end(format!("end:{llm}"), &llm, end, SpanStatus::Succeeded, None);
    let mut last = end;
    for c in m["toolCalls"].as_array().into_iter().flatten() {
        let (Some(call), Some(name)) = (c["id"].as_str(), c["name"].as_str()) else { continue };
        let tool = span("tool", call);
        e.start(&tool, end, Some(parent), SpanKind::Tool, name, |s| s.tool_name = Some(name.to_owned()));
        let (status, error) = match c["status"].as_str() {
            Some("success") if shell_failed(c) => (SpanStatus::Failed, Some("command exited non-zero (output not captured)")),
            Some("success") => (SpanStatus::Succeeded, None),
            Some("error") => (SpanStatus::Failed, Some("tool reported an error (output not captured)")),
            Some("cancelled") => (SpanStatus::Cancelled, None),
            _ => continue,
        };
        let done = c["timestamp"].as_str().and_then(parse_utc).filter(|t| VALID_TIME_NS.contains(t)).unwrap_or(end).max(end);
        if let Some(agent) = c["agentId"].as_str() {
            let sub = span("subagent", agent);
            // Newer versions run every agent through `invoke_agent`; older ones name the tool after the agent.
            let name = c["args"]["agent_name"].as_str().unwrap_or(name);
            e.start(&sub, end, Some(&tool), SpanKind::Subagent, name, |s| s.agent_name = Some(name.to_owned()));
            e.end(format!("end:{sub}"), &sub, done, status, error);
        }
        e.end(format!("end:{tool}"), &tool, done, status, error);
        last = last.max(done);
    }
    last
}

/// Gemini CLI reports a shell command that ran to a non-zero exit as a success, with `Exit Code: <n>`
/// as the last line it appends to the output it returns to the model. Only that line is read.
// ponytail: a command whose own output ends in such a line and that exits 0 reads as failed; take the
// exit code from elsewhere if Gemini CLI ever records it.
fn shell_failed(call: &Value) -> bool {
    let outputs = call["result"].as_array().into_iter().flatten().filter_map(|p| p["functionResponse"]["response"]["output"].as_str());
    call["name"] == "run_shell_command"
        && outputs.into_iter().any(|o| o.lines().rev().find_map(|l| l.strip_prefix("Exit Code: ")).is_some_and(|c| c.trim() != "0"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_folder_once_per_change() {
        let dir = std::env::temp_dir().join(format!("tw-gemini-{}", std::process::id()));
        let chats = dir.join("conf/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let header = json!({"sessionId": "s1", "projectHash": "h", "startTime": "2026-10-23T12:00:00Z", "kind": "main"});
        let prompt = json!({"id": "u1", "timestamp": "2026-10-23T12:00:00Z", "type": "user", "content": [{"text": "(prompt)"}]});
        std::fs::write(chats.join("session-1.jsonl"), format!("{header}\n{prompt}\n")).unwrap();
        std::fs::write(chats.join("logs.json"), "[]").unwrap();

        let (events, cursor) = read(&dir, i64::MIN, "gemini-cli").unwrap();
        assert_eq!(events.iter().filter(|e| matches!(e.body, EventBody::SpanStarted { kind: SpanKind::Turn, .. })).count(), 1);
        assert!(cursor > 0);
        assert_eq!(read(&dir, cursor, "gemini-cli").unwrap(), (Vec::new(), cursor), "nothing changed since the cursor");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
