//! Kilo Code CLI: the conformance scenario as `kilo.db` rows, shaped like a live run of `kilo run` 7.8.17
//! against Ollama. Each model call is an assistant message holding one step (`step-start`, its tool parts,
//! `step-finish` once the tools are done), a sub-agent is a child session named by the task tool's
//! `metadata.sessionId`, and the failing shell command is a `completed` bash part with `metadata.exit` 1.

mod support;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use support::*;
use trace_core::*;
use trace_ingest::{Capture, import_file};
use trace_storage::{SessionFilter, Store};

/// Kilo's tables as of 7.8.17, trimmed to the columns that matter and the ones that cannot be null.
/// `session_message` is OpenCode 2.x's, keyed to `session`; Kilo leaves it empty for these sessions.
const SCHEMA: &str = "
CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, parent_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
  title TEXT NOT NULL, version TEXT NOT NULL, agent TEXT, cost REAL NOT NULL DEFAULT 0, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id), type TEXT NOT NULL, seq INTEGER,
  time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);";

struct Db {
    c: Connection,
    parts: usize,
}

impl Db {
    fn session(&self, id: &str, parent: Option<&str>, agent: &str, start: f64, end: f64) {
        self.c.execute("INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES (?1, 'p', ?2, 's', '/work/conf', '(title)', '7.8.17', ?3, ?4, ?5)", params![id, parent, agent, ms(start), ms(end)]).unwrap();
    }

    fn message(&self, sid: &str, id: &str, at: f64, data: Value) {
        self.c.execute("INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)", params![id, sid, ms(at), data.to_string()]).unwrap();
    }

    /// Part ids sort in creation order, as Kilo's do.
    fn part(&mut self, sid: &str, mid: &str, at: f64, data: Value) {
        self.parts += 1;
        self.c.execute("INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)", params![format!("prt_{:03}", self.parts), mid, sid, ms(at), data.to_string()]).unwrap();
    }

    fn user(&mut self, sid: &str, id: &str, at: f64, agent: &str) {
        self.message(sid, id, at, json!({"role": "user", "time": {"created": ms(at)}, "agent": agent, "model": {"providerID": "ollama", "modelID": "qwen3:8b"}}));
        self.part(sid, id, at, json!({"type": "text", "text": "(prompt)"}));
    }

    /// One model call from `start` to `end`, then its tools; the step finishes once they are done.
    fn step(&mut self, sid: &str, id: &str, start: f64, end: f64, tools: &[(&str, &str, f64, Value)]) {
        let tokens = json!({"total": 170, "input": 100, "output": 20, "reasoning": 0, "cache": {"write": 0, "read": 50}});
        let finished = tools.iter().map(|(_, _, end, _)| *end).fold(end, f64::max);
        let finish = if tools.is_empty() { "stop" } else { "tool-calls" };
        self.message(sid, id, start, json!({"role": "assistant", "agent": "code", "cost": 0, "tokens": tokens, "modelID": "qwen3:8b", "providerID": "ollama", "time": {"created": ms(start), "completed": ms(finished)}, "finish": finish}));
        self.part(sid, id, start, json!({"type": "step-start", "time": {"start": ms(start)}}));
        for (call, tool, done, metadata) in tools {
            let state = json!({"status": "completed", "input": {}, "output": "(output)", "metadata": metadata, "time": {"start": ms(end), "end": ms(*done)}});
            self.part(sid, id, end, json!({"type": "tool", "tool": tool, "callID": call, "state": state}));
        }
        self.part(sid, id, finished, json!({"reason": finish, "type": "step-finish", "time": {"start": ms(start), "end": ms(finished)}, "tokens": tokens, "cost": 0}));
    }
}

fn kilo(d: &Path) -> Vec<(PathBuf, &'static str)> {
    let path = d.join("kilo.db");
    let mut db = Db { c: Connection::open(&path).unwrap(), parts: 0 };
    db.c.execute_batch(SCHEMA).unwrap();
    db.session("ses_main", None, "code", 0., 11.);
    db.session("ses_helper", Some("ses_main"), "general", 6., 9.5);
    db.user("ses_main", "msg_01", 0., "code");
    db.step("ses_main", "msg_02", 0.5, 2., &[("call_read", "read", 3., json!({"truncated": false}))]);
    db.step("ses_main", "msg_03", 3., 4., &[("call_bash", "bash", 5., json!({"exit": 1, "truncated": false}))]);
    db.step("ses_main", "msg_04", 5., 6., &[("call_task", "task", 10., json!({"parentSessionId": "ses_main", "sessionId": "ses_helper"}))]);
    db.user("ses_helper", "msg_05", 6., "general");
    db.step("ses_helper", "msg_06", 6.5, 8., &[("call_grep", "grep", 8.5, json!({"matches": 1}))]);
    db.step("ses_helper", "msg_07", 8.5, 9.5, &[]);
    db.step("ses_main", "msg_08", 10., 11., &[]);
    vec![(path, "kilo")]
}

#[test]
fn kilo_conforms() {
    conforms("kilo", kilo(&dir()));
}

#[test]
fn sessions_are_kilos_and_carry_no_text() {
    let files = kilo(&dir());
    let mut store = Store::open_in_memory().unwrap();
    let first = import_file(&mut store, &files[0].0, "kilo", &Capture { payloads: true, projects: Vec::new() }).unwrap();
    let again = import_file(&mut store, &files[0].0, "kilo", &Capture::default()).unwrap();
    assert_eq!((again.inserted, again.duplicates), (0, first.inserted), "re-reading the database adds nothing");
    let summary = &store.list_sessions(&SessionFilter::default()).unwrap()[0];
    assert_eq!((summary.harness.as_str(), summary.source_adapter.as_str()), ("kilo", "kilo"));
    let s = store.load_session(&summary.session_id).unwrap();
    let root = s.spans.values().find(|x| x.kind == SpanKind::Agent).unwrap();
    assert_eq!((root.name.as_str(), root.source_id.0.as_str()), ("kilo", "kilo:ses_main"));
    for x in s.spans.values() {
        assert_eq!((&x.input_preview, &x.output_preview), (&None, &None), "{} carries no text even with payload capture on", x.span_id);
    }
}
