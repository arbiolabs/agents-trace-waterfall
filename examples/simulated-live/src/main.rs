//! Deterministic agent-session generator.
//!
//! simulated-live --jsonl [--spans N] [--seed S]       JSONL on stdout, synthetic timestamps
//! simulated-live --socket [PATH] [--speed X] [...]    stream to the collector in real time
//! simulated-live --fixtures ROOT                      regenerate the committed sample and fixtures
//!
//! --harness NAME names the session's harness (default `simulated`). --brands mixes real model names and
//! branded and categorized tools into each turn, for checking the app's icons. --cwd PATH sets the working
//! directory (default `/work/demo`) and names the project after its last component.

use anyhow::{Context, bail};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use trace_adapter_sdk::*;
use trace_core::*;
use ulid::Ulid;

/// 2026-01-01T00:00:00Z, the start of every `--jsonl` session.
const SYNTHETIC_T0: Ns = 1_767_225_600_000_000_000;
const SAMPLE_SEED: u64 = 1;
const SAMPLE_SPANS: usize = 20;

const BRAND_MODELS: [(&str, &str); 3] = [("claude-opus-5-5", "anthropic"), ("gpt-5", "openai"), ("gemini-3-pro", "google")];
const BRAND_TOOLS: [&str; 20] = [
    "mcp__github__create_branch", "mcp__notion__search", "mcp__figma__get_frame", "mcp__stripe__create_price",
    "mcp__linear__save_issue", "mcp__sentry__get_issue", "mcp__supabase__execute_sql", "Bash", "Read", "Edit", "Grep",
    "WebFetch", "mcp__playwright__browser_click", "Task", "TodoWrite", "mem_save", "run_sql", "AskUserQuestion",
    "mcp__acme__thing", "frobnicate",
];

/// What a session looks like beyond its size and seed.
#[derive(Clone, Copy)]
struct Flavor<'a> {
    harness: &'a str,
    brands: bool,
    cwd: Option<&'a str>,
}

const PLAIN: Flavor = Flavor { harness: "simulated", brands: false, cwd: None };

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo)
    }
}

struct Sim {
    rec: Recorder,
    now: Arc<AtomicI64>,
    rng: Lcg,
    speed: f64,
    spans: usize,
    brands: bool,
    llms: usize,
}

impl Sim {
    fn pause(&mut self, lo_ms: u64, hi_ms: u64) {
        let ms = self.rng.range(lo_ms, hi_ms);
        self.now.fetch_add((ms as f64 * 1e6 / self.speed) as Ns, Ordering::SeqCst);
    }

    fn start(&mut self, parent: &SpanId, s: SpanStart) -> SpanId {
        self.spans += 1;
        self.rec.span_started(SpanStart { parent: Some(parent.clone()), ..s })
    }

    fn end(&mut self, span: &SpanId) {
        self.rec.span_ended(span, SpanStatus::Succeeded, None);
    }

    fn tool(&mut self, parent: &SpanId, name: &str) -> SpanId {
        self.pause(5, 40);
        self.start(parent, SpanStart { tool_name: Some(name.into()), ..SpanStart::new(SpanKind::Tool, name) })
    }

    /// Starts an LLM call and streams until the first provisional usage delta.
    fn llm_start(&mut self, parent: &SpanId) -> (SpanId, u64, u64) {
        let (model, provider) = if self.brands { BRAND_MODELS[self.llms % BRAND_MODELS.len()] } else { ("sim-model-1", "simulated") };
        self.llms += 1;
        let llm = self.start(parent, SpanStart { model: Some(model.into()), provider: Some(provider.into()), ..SpanStart::new(SpanKind::Llm, "llm call") });
        let (input, output) = (self.rng.range(500, 6000), self.rng.range(60, 900));
        self.pause(200, 900);
        self.rec.span_event(&llm, "first_token", Default::default());
        self.pause(100, 600);
        self.rec.usage(&llm, provisional(output / 3), false);
        (llm, input, output)
    }

    fn llm_finish(&mut self, (llm, input, output): (SpanId, u64, u64)) {
        self.pause(100, 600);
        self.rec.usage(&llm, provisional(output / 3), false);
        self.pause(100, 600);
        let usage = Usage {
            tokens_input: Some(input),
            tokens_output: Some(output),
            cache_read: Some(input / 2),
            cost_usd_micros: Some(input * 3 + output * 15),
            cost_provenance: CostProvenance::Actual,
            ..Usage::default()
        };
        self.rec.usage(&llm, usage, true);
        self.end(&llm);
    }

    fn turn(&mut self, root: &SpanId, i: usize) {
        self.pause(10, 100);
        let turn = self.start(root, SpanStart::new(SpanKind::Turn, format!("turn {}", i + 1)));
        let llm = self.llm_start(&turn);
        if i.is_multiple_of(5) && !self.brands {
            // Tool call overlapping the streaming LLM response.
            let t = self.tool(&turn, "read_file");
            self.pause(100, 500);
            self.end(&t);
        }
        self.llm_finish(llm);
        // The third turn calls every tool at once, so one screen shows every brand and category.
        let tools: Vec<&str> = match i {
            _ if !self.brands => Vec::new(),
            2 => BRAND_TOOLS.to_vec(),
            _ => (0..2).map(|k| BRAND_TOOLS[(2 * i + k) % BRAND_TOOLS.len()]).collect(),
        };
        for name in tools {
            let t = self.tool(&turn, name);
            self.pause(50, 400);
            self.end(&t);
        }
        match i % 5 {
            _ if self.brands => {}
            1 => {
                let t = self.tool(&turn, "edit_file");
                self.pause(50, 300);
                self.rec.artifact(&t, &format!("src/module_{i}.rs"), Some("+12 -3"));
                self.end(&t);
            }
            2 => {
                let t = self.tool(&turn, "bash");
                self.pause(300, 1500);
                self.rec.span_event(&t, "stderr", attrs(json!({"bytes": self.rng.range(100, 4000)})));
                self.rec.span_ended(&t, SpanStatus::Failed, Some("exit status 1: 2 tests failed"));
                self.pause(50, 200);
                let retry = self.start(
                    &turn,
                    SpanStart { tool_name: Some("bash".into()), retry_of: Some(t), ..SpanStart::new(SpanKind::Tool, "bash") },
                );
                self.pause(300, 1500);
                self.end(&retry);
            }
            3 => {
                self.pause(10, 50);
                let sub = self.start(
                    &turn,
                    SpanStart { agent_name: Some("explorer".into()), ..SpanStart::new(SpanKind::Subagent, "explore codebase") },
                );
                let llm = self.llm_start(&sub);
                self.llm_finish(llm);
                let grep = self.tool(&sub, "grep");
                let read = self.tool(&sub, "read_file");
                self.pause(100, 400);
                self.end(&grep);
                self.pause(100, 400);
                self.end(&read);
                self.pause(10, 50);
                self.end(&sub);
            }
            4 => {
                self.pause(10, 50);
                let w = self.start(&turn, SpanStart::new(SpanKind::Wait, "await approval"));
                self.pause(500, 2500);
                self.end(&w);
                let t = self.tool(&turn, "write_file");
                self.pause(50, 300);
                self.end(&t);
            }
            _ => {}
        }
        self.pause(10, 100);
        self.end(&turn);
    }
}

fn provisional(output: u64) -> Usage {
    Usage { tokens_output: Some(output), cost_usd_micros: Some(output * 15), cost_provenance: CostProvenance::Estimate, ..Usage::default() }
}

fn attrs(v: serde_json::Value) -> Attributes {
    v.as_object().cloned().unwrap_or_default()
}

/// One session with at least `spans` spans. Same arguments give identical events.
fn generate(seed: u64, spans: usize, t0: Ns, speed: f64, flavor: Flavor) -> Vec<CanonicalEvent> {
    let session_id = Ulid::from_parts((t0 / 1_000_000) as u64, seed as u128).to_string();
    let now = Arc::new(AtomicI64::new(t0));
    let clock = now.clone();
    let rec = Recorder::with_clock(format!("simulated-live-{session_id}").as_str().into(), session_id.as_str().into(), move || clock.load(Ordering::SeqCst));
    let mut sim = Sim { rec, now, rng: Lcg(seed), speed, spans: 0, brands: flavor.brands, llms: 0 };
    match flavor.cwd {
        Some(dir) => sim.rec.session_started(dir.trim_end_matches('/').rsplit('/').next().unwrap_or(dir), dir, flavor.harness),
        None => sim.rec.session_started("demo-project", "/work/demo", flavor.harness),
    }
    sim.spans += 1;
    let root = sim.rec.span_started(SpanStart { agent_name: Some("main".into()), ..SpanStart::new(SpanKind::Agent, "agent") });
    let mut i = 0;
    while sim.spans < spans {
        sim.turn(&root, i);
        i += 1;
    }
    sim.pause(10, 100);
    sim.end(&root);
    sim.rec.session_ended(SessionStatus::Completed);
    sim.rec.take()
}

/// `(path relative to the repo root, events)` for the committed sample and test fixtures.
fn fixtures() -> Vec<(&'static str, Vec<CanonicalEvent>)> {
    let sample = generate(SAMPLE_SEED, SAMPLE_SPANS, SYNTHETIC_T0, 1.0, PLAIN);

    let mut shuffled = sample.clone();
    let mut rng = Lcg(7);
    for i in (1..shuffled.len()).rev() {
        shuffled.swap(i, rng.range(0, i as u64 + 1) as usize);
    }

    let duplicated = sample.iter().enumerate().flat_map(|(i, e)| std::iter::repeat_n(e.clone(), if i % 5 == 0 { 2 } else { 1 })).collect();

    let subagent = sample
        .iter()
        .position(|e| matches!(e.body, EventBody::SpanStarted { kind: SpanKind::Subagent, .. }))
        .expect("sample has a subagent");
    let mut missing_parent = sample.clone();
    missing_parent.remove(subagent);

    let interrupted = sample[..sample.len() * 3 / 5].to_vec();

    vec![
        ("examples/sample-trace.jsonl", sample),
        ("tests/fixtures/out-of-order.jsonl", shuffled),
        ("tests/fixtures/duplicates.jsonl", duplicated),
        ("tests/fixtures/missing-parent.jsonl", missing_parent),
        ("tests/fixtures/interrupted.jsonl", interrupted),
    ]
}

fn jsonl(events: &[CanonicalEvent]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_jsonl(&mut buf, events).expect("writing to a Vec cannot fail");
    buf
}

fn stream(path: &Path, events: &[CanonicalEvent]) -> anyhow::Result<()> {
    let mut client = Client::new(path, events[0].source_id.clone(), 100_000);
    let mut i = 0;
    while i < events.len() {
        let now = system_now_ns();
        let due = events[i..].iter().take_while(|e| e.occurred_at_utc_ns <= now).count();
        if due == 0 {
            std::thread::sleep(Duration::from_nanos((events[i].occurred_at_utc_ns - now) as u64));
            continue;
        }
        client.send(&events[i..i + due])?;
        i += due;
    }
    let cursor = client.flush(Duration::from_secs(10))?;
    eprintln!("sent {} events, collector cursor {cursor:?}, dropped {}", events.len(), client.dropped());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1).peekable();
    let (mut mode, mut spans, mut seed, mut speed) = (None, SAMPLE_SPANS, SAMPLE_SEED, 1.0);
    let mut path: Option<PathBuf> = None;
    let mut flavor = PLAIN;
    let mut harness = String::new();
    let mut cwd = None;
    while let Some(a) = args.next() {
        let mut value = || args.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--jsonl" => mode = Some("jsonl"),
            "--socket" => {
                mode = Some("socket");
                if args.peek().is_some_and(|p| !p.starts_with("--")) {
                    path = args.next().map(PathBuf::from);
                }
            }
            "--fixtures" => {
                mode = Some("fixtures");
                path = Some(value()?.into());
            }
            "--spans" => spans = value()?.parse()?,
            "--seed" => seed = value()?.parse()?,
            "--speed" => speed = value()?.parse()?,
            "--harness" => harness = value()?,
            "--brands" => flavor.brands = true,
            "--cwd" => cwd = Some(value()?),
            _ => bail!("unknown argument {a}\nusage: simulated-live (--jsonl | --socket [PATH] | --fixtures ROOT) [--spans N] [--seed S] [--speed X] [--harness NAME] [--brands] [--cwd PATH]"),
        }
    }
    if !harness.is_empty() {
        flavor.harness = &harness;
    }
    flavor.cwd = cwd.as_deref();
    match mode {
        Some("jsonl") => {
            write_jsonl(std::io::stdout().lock(), &generate(seed, spans, SYNTHETIC_T0, 1.0, flavor))?;
        }
        Some("socket") => {
            anyhow::ensure!(speed > 0.0, "--speed must be positive");
            stream(&path.unwrap_or_else(default_socket_path), &generate(seed, spans, system_now_ns(), speed, flavor))?;
        }
        Some("fixtures") => {
            let root = path.expect("set with --fixtures");
            for (rel, events) in fixtures() {
                std::fs::write(root.join(rel), jsonl(&events)).with_context(|| rel)?;
            }
        }
        _ => bail!("pass --jsonl, --socket [PATH] or --fixtures ROOT"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn state(events: &[CanonicalEvent]) -> TraceState {
        TraceState::from_events(events[0].session_id.clone(), events)
    }

    #[test]
    fn same_seed_gives_identical_jsonl() {
        let a = jsonl(&generate(42, 200, SYNTHETIC_T0, 1.0, PLAIN));
        assert_eq!(a, jsonl(&generate(42, 200, SYNTHETIC_T0, 1.0, PLAIN)));
        assert_ne!(a, jsonl(&generate(43, 200, SYNTHETIC_T0, 1.0, PLAIN)));
    }

    #[test]
    fn cwd_names_the_project_after_its_directory() {
        let events = generate(1, 20, SYNTHETIC_T0, 1.0, Flavor { cwd: Some("/u/code/tools/tw-r5"), ..PLAIN });
        let s = state(&events);
        assert_eq!((s.session.project_id.as_str(), s.session.working_directory.as_str()), ("tw-r5", "/u/code/tools/tw-r5"));
    }

    #[test]
    fn committed_fixtures_match_generator() {
        for (rel, events) in fixtures() {
            let committed = std::fs::read(repo_root().join(rel)).unwrap();
            assert!(committed == jsonl(&events), "{rel} is stale; run `cargo run -p simulated-live -- --fixtures .`");
            assert_eq!(read_jsonl(&committed[..]).unwrap(), events, "{rel} parses");
        }
    }

    #[test]
    fn sample_trace_has_retry_failure_and_final_usage() {
        let events = read_jsonl(&std::fs::read(repo_root().join("examples/sample-trace.jsonl")).unwrap()[..]).unwrap();
        assert!((60..=150).contains(&events.len()), "{} events", events.len());
        let s = state(&events);
        assert_eq!(s.session.status, SessionStatus::Completed);
        let failed: Vec<_> = s.spans.values().filter(|sp| sp.status == SpanStatus::Failed).collect();
        assert_eq!(failed.len(), 1);
        let retry = s.spans.values().find(|sp| sp.retry_of.is_some()).expect("a retry span");
        assert_eq!(retry.retry_of.as_ref(), Some(&failed[0].span_id));
        assert_eq!(retry.status, SpanStatus::Succeeded);
        assert!(s.spans.values().any(|sp| sp.kind == SpanKind::Llm && sp.usage_final));
        assert!(s.spans.values().any(|sp| !sp.artifacts.is_empty()));
        assert!(s.spans.values().any(|sp| sp.kind == SpanKind::Subagent && !s.children(&sp.span_id).is_empty()));
        assert!(s.spans.values().any(|sp| sp.kind == SpanKind::Wait));
    }

    #[test]
    fn fixtures_exercise_reducer_edge_cases() {
        let f = fixtures();
        let sample = state(&f[0].1);
        assert_eq!(state(&f[1].1), sample, "out-of-order converges to the in-order state");
        assert_eq!(state(&f[2].1), sample, "duplicates are idempotent");
        assert!(f[2].1.len() > f[0].1.len());
        let missing = state(&f[3].1);
        assert_eq!(missing.unresolved().len(), 3, "subagent's llm, grep and read_file wait for their parent");
        let interrupted = state(&f[4].1);
        assert_eq!(interrupted.session.status, SessionStatus::Live);
        assert!(interrupted.spans.values().any(|sp| sp.ended_at_utc_ns.is_none()));
    }

    #[test]
    fn large_session_for_perf() {
        let events = generate(SAMPLE_SEED, 10_000, SYNTHETIC_T0, 1.0, PLAIN);
        assert!(state(&events).spans.len() >= 10_000);
    }
}
