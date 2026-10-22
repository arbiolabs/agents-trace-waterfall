use serde_json::json;
use trace_core::*;

fn ev(seq: u64, t: Ns, span: Option<&str>, body: serde_json::Value) -> CanonicalEvent {
    let mut v = json!({
        "source_id": "src", "source_seq": seq, "event_id": format!("e{seq}"),
        "observed_at_utc_ns": t, "occurred_at_utc_ns": t, "session_id": "s1",
    });
    if let Some(s) = span {
        v["span_id"] = json!(s);
    }
    v.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

fn start(seq: u64, t: Ns, id: &str, parent: Option<&str>, kind: &str) -> CanonicalEvent {
    ev(seq, t, Some(id), json!({"event_type": "span.started", "body": {"name": id, "kind": kind, "parent_span_id": parent}}))
}

fn end(seq: u64, t: Ns, id: &str, status: &str) -> CanonicalEvent {
    ev(seq, t, Some(id), json!({"event_type": "span.ended", "body": {"status": status}}))
}

fn usage(seq: u64, t: Ns, id: &str, input: u64, cost: u64, is_final: bool) -> CanonicalEvent {
    ev(seq, t, Some(id), json!({"event_type": "usage.reported", "body": {
        "tokens_input": input, "cost_usd_micros": cost, "cost_provenance": "actual", "is_final": is_final}}))
}

fn sample() -> Vec<CanonicalEvent> {
    vec![
        ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
        start(2, 10, "root", None, "agent"),
        start(3, 20, "llm1", Some("root"), "llm"),
        start(4, 25, "tool1", Some("root"), "tool"),
        usage(5, 30, "llm1", 100, 7, false),
        end(6, 40, "tool1", "failed"),
        start(7, 41, "tool2", Some("root"), "tool"),
        usage(8, 50, "llm1", 120, 9, true),
        end(9, 60, "llm1", "succeeded"),
        end(10, 70, "tool2", "succeeded"),
        end(11, 80, "root", "succeeded"),
        ev(12, 90, None, json!({"event_type": "session.ended", "body": {"status": "completed"}})),
    ]
}

#[test]
fn wire_format_round_trips() {
    let e = &sample()[7];
    let text = serde_json::to_string(e).unwrap();
    assert!(text.contains(r#""event_type":"usage.reported""#), "{text}");
    assert_eq!(&serde_json::from_str::<CanonicalEvent>(&text).unwrap(), e);
}

#[test]
fn builds_hierarchy_with_concurrent_children() {
    let s = TraceState::from_events("s1".into(), &sample());
    assert_eq!(s.roots(), vec![&SpanId::from("root")]);
    let kids: Vec<_> = s.children(&"root".into()).iter().map(|c| c.0.as_str()).collect();
    assert_eq!(kids, ["llm1", "tool1", "tool2"]);
    assert_eq!(s.session.status, SessionStatus::Completed);
    assert_eq!(s.spans[&SpanId::from("tool1")].status, SpanStatus::Failed);
    assert_eq!(s.wall_clock(1000), 90);
}

#[test]
fn duplicates_are_ignored() {
    let mut events = sample();
    events.extend(sample());
    let s = TraceState::from_events("s1".into(), &events);
    assert_eq!(s, TraceState::from_events("s1".into(), &sample()));
    assert_eq!(s.event_count, 12);
}

#[test]
fn out_of_order_and_missing_parents_reconcile() {
    let mut events = sample();
    events.reverse();
    let reversed = TraceState::from_events("s1".into(), &events);
    let expected = TraceState::from_events("s1".into(), &sample());
    assert_eq!(reversed.spans, expected.spans);
    assert_eq!(reversed.roots(), expected.roots());
    assert_eq!(reversed.pending_count(), 0);

    let orphan_only = TraceState::from_events("s1".into(), &[start(3, 20, "llm1", Some("root"), "llm"), end(9, 60, "root", "succeeded")]);
    assert_eq!(orphan_only.unresolved(), vec![&SpanId::from("llm1")]);
    assert_eq!(orphan_only.pending_count(), 1);
}

#[test]
fn final_usage_supersedes_provisional_and_is_not_double_counted() {
    let s = TraceState::from_events("s1".into(), &sample());
    let u = s.spans[&SpanId::from("llm1")].usage.clone().unwrap();
    assert_eq!((u.tokens_input, u.cost_usd_micros), (Some(120), Some(9)));
    let t = s.totals();
    assert_eq!((t.tokens_input, t.cost_usd_micros(), t.billable_records), (120, Some(9), 1));
    assert!(s.spans[&SpanId::from("root")].usage.is_none());

    let late_delta = TraceState::from_events("s1".into(), &[start(1, 0, "x", None, "llm"), usage(3, 2, "x", 50, 1, true), usage(2, 1, "x", 9, 9, false)]);
    assert_eq!(late_delta.spans[&SpanId::from("x")].usage.as_ref().unwrap().tokens_input, Some(50));
}

#[test]
fn subtree_totals_count_each_span_once() {
    let s = TraceState::from_events("s1".into(), &[
        start(1, 0, "root", None, "agent"),
        usage(2, 1, "root", 1_000, 100, true),
        start(3, 2, "sub", Some("root"), "subagent"),
        usage(4, 3, "sub", 10, 1, true),
        start(5, 4, "llm", Some("sub"), "llm"),
        usage(6, 5, "llm", 20, 2, false),
        usage(7, 6, "llm", 30, 3, true),
        start(8, 7, "tool", Some("sub"), "tool"),
        start(9, 8, "other", None, "agent"),
        usage(10, 9, "other", 5_000, 500, true),
    ]);
    let sum = |id: &str| {
        let t = s.subtree_totals(&id.into());
        (t.tokens_input, t.cost_usd_micros(), t.billable_records)
    };
    assert_eq!(sum("root"), (1_040, Some(104), 3), "own usage plus the final usage of each descendant");
    assert_eq!(sum("sub"), (40, Some(4), 2));
    assert_eq!(sum("tool"), (0, None, 0), "nothing billed means unknown cost");
    assert_eq!(s.subtree_totals(&"root".into()).tokens_input + sum("other").0, s.totals().tokens_input);
}

#[test]
fn unknown_cost_is_not_a_number() {
    let s = TraceState::from_events("s1".into(), &[
        start(1, 0, "x", None, "llm"),
        ev(2, 1, Some("x"), json!({"event_type": "usage.reported", "body": {"tokens_input": 5, "is_final": true}})),
    ]);
    assert_eq!(s.totals().cost_usd_micros(), None);
}

#[test]
fn retry_keeps_failed_attempt() {
    let s = TraceState::from_events("s1".into(), &[
        start(1, 0, "a1", None, "tool"),
        end(2, 5, "a1", "failed"),
        ev(3, 6, Some("a2"), json!({"event_type": "span.started", "body": {"name": "a", "kind": "tool", "retry_of": "a1"}})),
    ]);
    assert_eq!(s.spans[&SpanId::from("a1")].status, SpanStatus::Failed);
    assert_eq!(s.spans[&SpanId::from("a2")].retry_of, Some(SpanId::from("a1")));
}

#[test]
fn durations_clamp_skew_and_stop_open_spans_at_session_end() {
    let s = TraceState::from_events("s1".into(), &[start(1, 100, "x", None, "tool"), end(2, 90, "x", "succeeded")]);
    assert_eq!(s.spans[&SpanId::from("x")].elapsed(None, 1000), Elapsed { ns: 0, skewed: true });
    let open = TraceState::from_events("s1".into(), &[start(1, 100, "x", None, "tool")]);
    assert_eq!(open.spans[&SpanId::from("x")].elapsed(Some(150), 1000).ns, 50);
    assert_eq!(open.spans[&SpanId::from("x")].elapsed(None, 130).ns, 30);
}

#[test]
fn slowest_is_greatest_completed_llm_or_tool() {
    let s = TraceState::from_events("s1".into(), &sample());
    assert_eq!(s.slowest(SLOWEST_DEFAULT_KINDS).unwrap().span_id, SpanId::from("llm1"));
}

#[test]
fn replay_is_deterministic_and_seek_matches_full_fold() {
    let mut events = Vec::new();
    let mut seq = 0;
    for i in 0..1200 {
        seq += 1;
        events.push(start(seq, i * 10, &format!("s{i}"), (i > 0).then_some("s0"), "tool"));
        seq += 1;
        events.push(end(seq, i * 10 + 15, &format!("s{i}"), "succeeded"));
    }
    let shuffled: Vec<_> = events.iter().rev().cloned().collect();
    let a = Replay::new("s1".into(), events.clone());
    let b = Replay::new("s1".into(), shuffled);
    for t in [0, 4999, 5000, 7777, 12_000, 99_999] {
        let at = a.state_at(t);
        assert_eq!(at, b.state_at(t), "t={t}");
        let mut sorted = events.clone();
        sort_events(&mut sorted);
        let manual = TraceState::from_events("s1".into(), sorted.iter().filter(|e| e.occurred_at_utc_ns <= t));
        assert_eq!(at, manual, "t={t}");
    }
    assert_eq!(a.next_event_time(5000), Some(5005));
    assert_eq!(a.prev_event_time(5000), Some(4995));
}

#[test]
fn replay_clock_scales_and_stops_at_end() {
    let mut c = ReplayClock::new(0);
    c.set_speed(2.0);
    c.playing = true;
    c.tick(10, 100);
    assert_eq!(c.position, 20);
    c.tick(1000, 100);
    assert_eq!((c.position, c.playing), (100, false));
    c.faster();
    c.faster();
    c.faster();
    assert_eq!(c.speed(), 10.0);
}

fn session_ended(source: &str, seq: u64, t: Ns, status: &str) -> CanonicalEvent {
    let mut e = ev(seq, t, None, json!({"event_type": "session.ended", "body": {"status": status}}));
    e.source_id = source.into();
    e.event_id = EventId(format!("{source}-end-{seq}"));
    e
}

fn outcome(s: &TraceState) -> (SessionStatus, Option<Ns>, Option<&str>) {
    (s.session.status, s.session.ended_at_utc_ns, s.session.ended_by.as_ref().map(|b| b.0.as_str()))
}

#[test]
fn activity_after_collector_interruption_revives_session() {
    let interrupted = session_ended(COLLECTOR_SOURCE, 0, 50, "interrupted");
    let s = TraceState::from_events("s1".into(), &[start(1, 10, "x", None, "tool"), interrupted.clone(), end(2, 60, "x", "succeeded")]);
    assert_eq!(outcome(&s), (SessionStatus::Live, None, None));
    let still = TraceState::from_events("s1".into(), &[start(1, 10, "x", None, "tool"), interrupted]);
    assert_eq!(outcome(&still), (SessionStatus::Interrupted, Some(50), Some("collector")));
}

#[test]
fn harness_reported_interruption_is_final() {
    let s = TraceState::from_events("s1".into(), &[start(1, 10, "x", None, "tool"), session_ended("src", 2, 50, "interrupted"), end(3, 60, "x", "cancelled")]);
    assert_eq!(outcome(&s), (SessionStatus::Interrupted, Some(50), Some("src")));
}

#[test]
fn session_outcome_does_not_depend_on_apply_order() {
    let events = [
        start(1, 10, "x", None, "tool"),
        session_ended("src", 2, 90, "completed"),
        session_ended(COLLECTOR_SOURCE, 0, 50, "interrupted"),
        session_ended("other", 1, 90, "failed"),
    ];
    let orders: [[usize; 4]; 4] = [[0, 1, 2, 3], [0, 2, 1, 3], [3, 2, 1, 0], [1, 3, 0, 2]];
    for order in orders {
        let s = TraceState::from_events("s1".into(), order.iter().map(|&i| &events[i]));
        assert_eq!(outcome(&s), (SessionStatus::Completed, Some(90), Some("src")), "order {order:?}");
    }
}

#[test]
fn replay_checkpoints_stay_bounded_and_seek_matches_full_fold() {
    let events: Vec<_> = (0..25_000)
        .flat_map(|i| [start(2 * i + 1, i as Ns * 10, &format!("s{i}"), None, "tool"), end(2 * i + 2, i as Ns * 10 + 5, &format!("s{i}"), "succeeded")])
        .collect();
    let r = Replay::new("s1".into(), events.clone());
    assert_eq!(r.events().len(), 50_000);
    assert!(r.checkpoint_count() <= 65, "{} checkpoints", r.checkpoint_count());
    for t in [0, 123_457, 249_995] {
        let manual = TraceState::from_events("s1".into(), events.iter().filter(|e| e.occurred_at_utc_ns <= t));
        assert_eq!(r.state_at(t), manual, "t={t}");
    }
}

#[test]
fn validate_rejects_bad_envelopes() {
    let ok = start(1, 10, "x", None, "tool");
    assert_eq!(ok.validate(), Ok(()));
    let mut no_span = ok.clone();
    no_span.span_id = None;
    assert_eq!(no_span.validate(), Err("event e1: span.started needs a span_id".into()));
    let mut early = ok.clone();
    early.observed_at_utc_ns = -1;
    assert_eq!(early.validate(), Err("event e1: observed_at_utc_ns -1 is outside 1970-01-01..2200-01-01".into()));
    let mut late = ok.clone();
    late.occurred_at_utc_ns = 7_258_118_400_000_000_000;
    assert_eq!(late.validate(), Err("event e1: occurred_at_utc_ns 7258118400000000000 is outside 1970-01-01..2200-01-01".into()));
    let mut schema = ok.clone();
    schema.schema_version = 2;
    assert_eq!(schema.validate(), Err("event e1: schema_version 2 is not 1".into()));
    let mut empty = ok;
    empty.session_id = SessionId::default();
    assert_eq!(empty.validate(), Err("source_id, event_id and session_id must be non-empty".into()));
}
