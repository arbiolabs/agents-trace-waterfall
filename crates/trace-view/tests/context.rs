use serde_json::{Value, json};
use trace_core::*;
use trace_view::*;

#[derive(Default)]
struct Trace(Vec<CanonicalEvent>);

impl Trace {
    fn push(&mut self, t: Ns, span: Option<&str>, body: Value) -> &mut Self {
        let seq = self.0.len() as u64 + 1;
        let mut v = json!({
            "source_id": "src", "source_seq": seq, "event_id": format!("e{seq}"),
            "observed_at_utc_ns": t, "occurred_at_utc_ns": t, "session_id": "s1",
        });
        if let Some(s) = span {
            v["span_id"] = json!(s);
        }
        v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        self.0.push(serde_json::from_value(v).unwrap());
        self
    }

    fn span(&mut self, t: Ns, id: &str, parent: Option<&str>, kind: &str) -> &mut Self {
        self.push(t, Some(id), json!({"event_type": "span.started", "body": {"name": id, "kind": kind, "parent_span_id": parent, "tool_name": id}}))
    }

    /// An LLM call reading `input + cache` tokens of context and writing `output`.
    fn llm(&mut self, t: Ns, id: &str, parent: &str, model: &str, cache: u64, output: u64) -> &mut Self {
        self.push(t, Some(id), json!({"event_type": "span.started", "body": {"name": id, "kind": "llm", "parent_span_id": parent, "model": model}}));
        self.push(t, Some(id), json!({"event_type": "usage.reported", "body": {"tokens_input": 10, "cache_read": cache - 10, "tokens_output": output, "is_final": true}}))
    }

    fn state(&self) -> TraceState {
        TraceState::from_events("s1".into(), &self.0)
    }
}

const OPUS: &str = "claude-opus-5-5";

/// Turn 1: a1 (1000), Bash and Read, a2 (1650). Turn 2: a3 (1900), a4 (1880, noise), an Agent tool whose
/// sub-agent reads 50k of its own, a5 (2000), then a compaction to a6 (1200) and a7 (1300).
fn session() -> Trace {
    let mut t = Trace::default();
    t.push(0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}}));
    t.span(0, "root", None, "agent").span(0, "turn1", Some("root"), "turn");
    t.llm(10, "a1", "turn1", OPUS, 1000, 50);
    t.span(20, "Bash", Some("turn1"), "tool").span(25, "Read", Some("turn1"), "tool");
    t.llm(30, "a2", "turn1", OPUS, 1650, 20);
    t.span(40, "turn2", Some("root"), "turn");
    t.llm(50, "a3", "turn2", OPUS, 1900, 0);
    t.llm(60, "a4", "turn2", OPUS, 1880, 0);
    t.span(62, "Agent", Some("turn2"), "tool").span(62, "sub", Some("Agent"), "subagent");
    t.llm(63, "s1", "sub", OPUS, 50_000, 900);
    t.span(64, "Grep", Some("sub"), "tool");
    t.llm(70, "a5", "turn2", OPUS, 2000, 0);
    t.llm(80, "a6", "turn2", OPUS, 1200, 0);
    t.llm(90, "a7", "turn2", OPUS, 1300, 0);
    t
}

fn parts(m: &ContextMap) -> Vec<(PartKind, &str, u64)> {
    m.parts.iter().map(|p| (p.kind, p.label.as_str(), p.tokens)).collect()
}

const BASE: &str = "System, tools & memory";

#[test]
fn tool_growth_splits_across_the_tools_that_ran() {
    let m = context_map(&session().state(), 35).unwrap();
    assert_eq!(
        parts(&m),
        [(PartKind::Baseline, BASE, 1000), (PartKind::Tool, "Bash", 300), (PartKind::Tool, "Read", 300), (PartKind::Output, "Responses", 50)]
    );
    assert_eq!((m.used, m.limit, m.at.0.as_str(), m.compactions, m.model.as_deref()), (1650, Some(200_000), "a2", 0, Some(OPUS)));
}

#[test]
fn the_first_call_is_all_baseline_and_nothing_before_it_maps() {
    let s = session().state();
    assert_eq!(context_map(&s, 5), None);
    assert_eq!(parts(&context_map(&s, 10).unwrap()), [(PartKind::Baseline, BASE, 1000)]);
}

#[test]
fn a_new_turn_is_a_prompt_and_sub_agents_keep_their_own_context() {
    let m = context_map(&session().state(), 75).unwrap();
    // Raw growth is 1020 (650 + 250 + 120), but a5 reads 2000: the 20-token dip at a4 is noise, so the parts
    // scale to the 1000 above baseline, the rounding remainder going to the largest.
    assert_eq!(
        parts(&m),
        [
            (PartKind::Baseline, BASE, 1000),
            (PartKind::Tool, "Read", 296),
            (PartKind::Tool, "Bash", 294),
            (PartKind::Prompts, "Prompts", 225),
            (PartKind::Tool, "Agent", 117),
            (PartKind::Output, "Responses", 68),
        ]
    );
    assert_eq!((m.used, m.at.0.as_str()), (2000, "a5"));
    assert_eq!(m.parts.iter().map(|p| p.tokens).sum::<u64>(), m.used);
}

#[test]
fn a_compaction_resets_to_baseline_and_summary() {
    let m = context_map(&session().state(), 95).unwrap();
    assert_eq!(
        parts(&m),
        [(PartKind::Baseline, BASE, 1000), (PartKind::Summary, "Compaction summary", 200), (PartKind::Other, "Other", 100)]
    );
    assert_eq!((m.used, m.compactions), (1300, 1));
}

#[test]
fn the_window_comes_from_the_model_and_grows_past_a_full_one() {
    let window = |model: &str, ctx: u64| {
        let mut t = Trace::default();
        t.span(0, "root", None, "agent").llm(1, "a", "root", model, ctx, 0);
        context_map(&t.state(), 1).unwrap().limit
    };
    assert_eq!(window(OPUS, 150_000), Some(200_000));
    assert_eq!(window(OPUS, 250_000), Some(1_000_000));
    assert_eq!(window("claude-opus-5-5[1m]", 250_000), Some(1_000_000));
    assert_eq!(window("gpt-5.1-codex", 1000), Some(400_000));
    assert_eq!(window("gemini-2.5-pro", 1000), Some(1_048_576));
    assert_eq!(window("qwen3-coder", 1000), None);
}

#[test]
fn treemap_tiles_the_box_without_overlap() {
    let weights = [6, 6, 4, 0, 3, 2, 2, 1];
    let rects = treemap(&weights, 6.0, 4.0);
    assert_eq!(rects.len(), weights.len());
    assert_eq!(rects[3], (0.0, 0.0, 0.0, 0.0));
    let area = |r: &(f32, f32, f32, f32)| r.2 * r.3;
    for (r, w) in rects.iter().zip(weights) {
        assert!((area(r) - w as f32).abs() < 1e-3, "{r:?} has area {w}");
        assert!(r.0 >= -1e-4 && r.1 >= -1e-4 && r.0 + r.2 <= 6.0 + 1e-4 && r.1 + r.3 <= 4.0 + 1e-4, "{r:?} inside the box");
    }
    for (i, a) in rects.iter().enumerate() {
        for b in &rects[i + 1..] {
            let ix = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
            let iy = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
            assert!(ix <= 1e-4 || iy <= 1e-4, "{a:?} overlaps {b:?}");
        }
    }
    assert_eq!(treemap(&[0, 0], 6.0, 4.0), [(0.0, 0.0, 0.0, 0.0); 2]);
}
