use serde_json::json;
use std::collections::HashSet;
use trace_core::*;
use trace_view::*;

const MS: Ns = 1_000_000;

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

/// root 0..1000ms, llm1 100..400ms (billed), tool1 400..500ms failed, tool2 500ms.. running, orphan under a missing parent.
fn sample() -> TraceState {
    let events = vec![
        ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
        start(2, 0, "root", None, "agent"),
        start(3, 100 * MS, "llm1", Some("root"), "llm"),
        ev(4, 400 * MS, Some("llm1"), json!({"event_type": "usage.reported", "body": {"tokens_input": 10, "cost_usd_micros": 12345, "cost_provenance": "actual", "is_final": true}})),
        end(5, 400 * MS, "llm1", "succeeded"),
        start(6, 400 * MS, "tool1", Some("root"), "tool"),
        end(7, 500 * MS, "tool1", "failed"),
        start(8, 500 * MS, "tool2", Some("root"), "tool"),
        start(9, 600 * MS, "orphan", Some("ghost"), "wait"),
        end(10, 700 * MS, "orphan", "succeeded"),
        end(11, 1000 * MS, "root", "succeeded"),
    ];
    TraceState::from_events("s1".into(), &events)
}

fn ids(state: &TraceState, rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r.kind {
            RowKind::Span(id) => format!("{}{}", " ".repeat(r.depth as usize), state.spans[id].name),
            RowKind::UnresolvedHeader => "<unresolved>".into(),
        })
        .collect()
}

#[test]
fn bar_span_clips_and_marks() {
    let vp = Viewport { start_ns: 100, ns_per_cell: 10.0, ..Default::default() };
    assert_eq!(bar_span(120, 155, &vp, 10), Some((2, 6)));
    assert_eq!(bar_span(50, 130, &vp, 10), Some((0, 3)), "clipped at left edge");
    assert_eq!(bar_span(180, 400, &vp, 10), Some((8, 10)), "clipped at right edge");
    assert_eq!(bar_span(0, 500, &vp, 10), Some((0, 10)), "spans the whole viewport");
    assert_eq!(bar_span(142, 142, &vp, 10), Some((4, 5)), "zero duration");
    assert_eq!(bar_span(141, 143, &vp, 10), Some((4, 5)), "sub-cell");
    assert_eq!(bar_span(100, 100, &vp, 10), Some((0, 1)), "zero duration at left edge");
    assert_eq!(bar_span(200, 200, &vp, 10), Some((9, 10)), "zero duration at right edge");
    assert_eq!(bar_span(10, 100, &vp, 10), None, "ends exactly at left edge");
    assert_eq!(bar_span(200, 300, &vp, 10), None, "starts exactly at right edge");
    assert_eq!(bar_span(10, 20, &vp, 10), None);
    assert_eq!(bar_span(150, 160, &vp, 0), None);
}

#[test]
fn viewport_zoom_keeps_anchor_and_pans() {
    let mut vp = Viewport::fit(0, 1000, 10);
    assert_eq!(vp, Viewport { start_ns: 0, ns_per_cell: 100.0, ..Default::default() });
    vp.zoom(0.5, 500);
    assert_eq!(vp, Viewport { start_ns: 250, ns_per_cell: 50.0, ..Default::default() });
    assert_eq!(vp.col_of(500), 5.0);
    vp.pan(-2.0);
    assert_eq!((vp.start_ns, vp.end_ns(10)), (150, 650));
}

#[test]
fn ticks_and_formatting() {
    let vp = Viewport::fit(0, 1000 * MS, 50);
    assert_eq!(ticks(&vp, 50, 0), [(0, "0s".to_string()), (10, "200ms".into()), (20, "400ms".into()), (30, "600ms".into()), (40, "800ms".into())]);
    let vp = Viewport { start_ns: 30_000 * MS, ns_per_cell: 3e9, ..Default::default() };
    assert_eq!(ticks(&vp, 40, 0), [(0, "30s".to_string()), (10, "1m".into()), (20, "1m30s".into()), (30, "2m".into())]);
    assert_eq!(
        [0, 850, 12_500, 250 * MS, 1500 * MS, 120_000 * MS, 3_900_000 * MS, -250 * MS].map(format_duration),
        ["0s", "850ns", "12.5µs", "250ms", "1.5s", "2m", "1h5m", "-250ms"]
    );
    assert_eq!([None, Some(1234), Some(1_234_567)].map(format_cost), ["—", "$0.0012", "$1.23"]);
    assert_eq!(
        [Some(0), Some(5_400), Some(30_000), Some(120), Some(500), Some(9_999), Some(10_000)].map(format_cost),
        ["$0.00", "$0.0054", "$0.03", "$0.00012", "$0.00050", "$0.01", "$0.01"],
        "cents at a cent and up, two significant figures below"
    );
    assert_eq!([999, 1000, 12_345, 1_500_000].map(format_tokens), ["999", "1k", "12.3k", "1.5M"]);
}

#[test]
fn flatten_orders_collapses_and_groups_unresolved() {
    let s = sample();
    let rows = flatten(&s, &HashSet::new(), None);
    assert_eq!(ids(&s, &rows), ["root", " llm1", " tool1", " tool2", "<unresolved>", " orphan"]);
    assert!(rows[0].has_children && !rows[0].collapsed && !rows[1].has_children);

    let collapsed = HashSet::from([SpanId::from("root")]);
    let rows = flatten(&s, &collapsed, None);
    assert_eq!(ids(&s, &rows), ["root", "<unresolved>", " orphan"]);
    assert!(rows[0].collapsed);
}

#[test]
fn filter_keeps_ancestors_even_when_collapsed() {
    let s = sample();
    let failed = |sp: &Span| sp.status == SpanStatus::Failed;
    let rows = flatten(&s, &HashSet::new(), Some(&failed));
    assert_eq!(ids(&s, &rows), ["root", " tool1"]);
    let collapsed = HashSet::from([SpanId::from("root")]);
    assert_eq!(ids(&s, &flatten(&s, &collapsed, Some(&failed))), ["root"]);
    let waits = |sp: &Span| sp.kind == SpanKind::Wait;
    assert_eq!(ids(&s, &flatten(&s, &HashSet::new(), Some(&waits))), ["<unresolved>", " orphan"]);
}

#[test]
fn extreme_timestamps_saturate_instead_of_overflowing() {
    assert_eq!([i64::MIN, i64::MAX, i64::MIN + 1].map(format_duration), ["-2562047h47m", "2562047h47m", "-2562047h47m"]);
    assert_eq!(Viewport::fit(i64::MIN, i64::MAX, 10), Viewport { start_ns: i64::MIN, ns_per_cell: i64::MAX as f64 / 10.0, ..Default::default() });
}

#[test]
fn flattens_a_hundred_thousand_deep_chain() {
    const N: usize = 100_000;
    let mut events = vec![start(0, 0, "s0", None, "tool")];
    for i in 1..N {
        events.push(start(i as u64, i as Ns, &format!("s{i}"), Some(&format!("s{}", i - 1)), "tool"));
    }
    events.push(end(N as u64, N as Ns, &format!("s{}", N - 1), "failed"));
    let s = TraceState::from_events("s1".into(), &events);
    let rows = flatten(&s, &HashSet::new(), None);
    assert_eq!((rows.len(), rows[1].depth, rows[65_535].depth, rows[N - 1].depth), (N, 1, u16::MAX, u16::MAX));
    let failed = |sp: &Span| sp.status == SpanStatus::Failed;
    assert_eq!(flatten(&s, &HashSet::new(), Some(&failed)).len(), N);
    let collapsed = HashSet::from([SpanId::from("s1")]);
    assert_eq!(ids(&s, &flatten(&s, &collapsed, Some(&failed))), ["s0", " s1"]);
}

#[test]
fn fine_ticks_deep_into_a_session_stay_distinct() {
    let seven_hours: Ns = 7 * 3600 * 1_000_000_000 + 4 * 60 * 1_000_000_000;
    let vp = Viewport { start_ns: seven_hours, ns_per_cell: 100_000_000.0, ..Default::default() };
    let labels: Vec<String> = ticks(&vp, 50, 0).into_iter().map(|(_, l)| l).collect();
    assert_eq!(labels, ["7h4m", "+1s", "+2s", "+3s", "+4s"]);
}

#[test]
fn resolves_unique_session_prefixes() {
    let started = |session: &str, seq| {
        let mut e = ev(seq, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}}));
        e.session_id = session.into();
        e
    };
    let mut store = trace_storage::Store::open_in_memory().unwrap();
    store.append(&[started("abc1", 1), started("abc2", 2), started("abc", 3)]).unwrap();
    assert_eq!(resolve_session(&store, "abc1").unwrap(), SessionId::from("abc1"));
    assert_eq!(resolve_session(&store, "abc").unwrap(), SessionId::from("abc"), "an exact id wins over its extensions");
    assert!(resolve_session(&store, "ab").unwrap_err().to_string().contains("ambiguous"));
    assert!(resolve_session(&store, "zz").unwrap_err().to_string().contains("no session"));
}

#[test]
fn tick_times_respect_a_pixel_gap() {
    let vp = Viewport { start_ns: 0, ns_per_cell: 100.0 * MS as f64, ..Default::default() };
    let t = tick_times(&vp, 450.0, 0, 80.0);
    assert_eq!(t, vec![(0, "0s".to_string()), (10_000 * MS, "10s".into()), (20_000 * MS, "20s".into()), (30_000 * MS, "30s".into()), (40_000 * MS, "40s".into())]);
}

#[test]
fn fold_all_hides_every_branch_and_keeps_the_selection_in_view() {
    let mut doc = TraceDoc::new(sample(), Mode::Static, 0);
    doc.refresh();
    let i = doc.rows.iter().position(|r| r.kind == RowKind::Span("tool1".into())).unwrap();
    doc.select(i);
    doc.fold_all(true);
    assert!(doc.all_folded());
    assert_eq!(ids(&doc.state, &doc.rows), ["root", "<unresolved>", " orphan"]);
    assert_eq!(doc.selected, Some("root".into()), "the selection climbs to what stays visible");
    doc.fold_all(false);
    assert!(!doc.all_folded());
    assert_eq!(doc.rows.len(), 6);
    doc.fold_all(true);
    doc.state.apply(&start(20, 800 * MS, "late", Some("tool2"), "tool"));
    doc.refresh();
    assert!(doc.all_folded(), "a branch arriving under a folded parent stays out of sight, so the next toggle expands");
}

#[test]
fn total_cost_marks_estimates_and_lower_bounds() {
    let t = |actual, estimate, unknown, billable| Totals { cost_actual_usd_micros: actual, cost_estimate_usd_micros: estimate, cost_unknown_records: unknown, billable_records: billable, ..Default::default() };
    assert_eq!(format_total_cost(&t(1_200_000, 0, 0, 1)), "$1.20");
    assert_eq!(format_total_cost(&t(1_000_000, 200_000, 0, 2)), "~$1.20", "part estimated from list prices");
    assert_eq!(format_total_cost(&t(0, 1_200_000, 1, 2)), "≥ $1.20", "a call of unknown cost makes it a lower bound");
    assert_eq!(format_total_cost(&t(0, 0, 1, 1)), "—");
}

#[test]
fn folds_take_idle_time_out_of_the_timeline() {
    const H: Ns = 3_600_000 * MS;
    let f = Folds::new(&[(H, 10 * H), (12 * H, 20 * H)]);
    assert_eq!((f.fold(H / 2), f.fold(5 * H), f.fold(11 * H), f.fold(21 * H)), (H / 2, H, 2 * H, 4 * H), "inside a stretch folds onto its start");
    for t in [0, H / 2, 10 * H + 1, 11 * H, 21 * H] {
        assert_eq!(f.unfold(f.fold(t)), t, "unfold inverts fold outside stretches");
    }
    assert_eq!(f.unfold(H), H, "a fold point unfolds to where the waiting began");
    assert_eq!(f.inside(5 * H), Some((H, 10 * H)));

    // An hour of work, nine idle hours, an hour of work: fits as two hours.
    let vp = Viewport::fit_folded(0, 11 * H, 100, Folds::new(&[(H, 10 * H)]));
    assert_eq!((vp.col_of(H), vp.col_of(10 * H), vp.col_of(11 * H)), (50.0, 50.0, 100.0), "the idle stretch takes no width");
    assert_eq!(vp.end_ns(100), 11 * H);
    let mut zoomed = vp;
    zoomed.zoom(0.5, 10 * H);
    assert_eq!(zoomed.col_of(10 * H), 50.0, "zoom keeps its anchor across a fold");
    let mut panned = vp;
    panned.pan(50.0);
    assert_eq!(panned.start_ns, H, "panning lands on the fold, where the waiting began");
    let labels: Vec<String> = tick_times(&vp, 100.0, 0, 20.0).into_iter().map(|(_, l)| l).collect();
    assert!(labels.contains(&"10h".to_owned()) || labels.contains(&"10h30m".to_owned()), "labels read real time after the fold: {labels:?}");
}

#[test]
fn replay_jumps_across_idle_time() {
    const H: Ns = 3_600_000 * MS;
    let events = vec![
        ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
        start(2, 0, "root", None, "agent"),
        start(3, 0, "a", Some("root"), "tool"),
        end(4, H / 2, "a", "succeeded"),
        start(5, 10 * H, "b", Some("root"), "tool"),
        end(6, 10 * H + H / 2, "b", "succeeded"),
        end(7, 10 * H + H / 2, "root", "succeeded"),
    ];
    let state = TraceState::from_events("s1".into(), &events);
    let mut doc = TraceDoc::new(state.clone(), Mode::Static, 0);
    assert_eq!(doc.folds.gaps(), [(H / 2, 10 * H)]);
    let mut replay = TraceDoc::new(TraceState::from_events("s1".into(), &events[..2]), Mode::Replay { replay: Replay::new("s1".into(), events.clone()), clock: ReplayClock::new(H / 2 - MS) }, 0);
    assert_eq!(replay.folds.gaps(), [(H / 2, 10 * H)], "replay folds the whole recording's idle time from the start");
    let Mode::Replay { clock, .. } = &mut replay.mode else { unreachable!() };
    clock.playing = true;
    let store = trace_storage::Store::open_in_memory().unwrap();
    replay.advance(&store, std::time::Duration::from_millis(10)).unwrap();
    let Mode::Replay { clock, .. } = &replay.mode else { unreachable!() };
    assert_eq!(clock.position, 10 * H, "playback skips nine idle hours instead of waiting through them");
    doc.layout(100);
    assert_eq!(doc.viewport.unwrap().folds, doc.folds);
}

#[test]
fn player_reads_active_time_and_names_the_overall_length() {
    assert_eq!((clock_time(65 * 1_000_000_000), clock_time(3_725 * 1_000_000_000)), ("1:05".into(), "1:02:05".into()));
    let d = 86_400 * 1_000_000_000i64;
    assert_eq!((spoken_span(14 * d), spoken_span(d), spoken_span(5 * 3_600_000_000_000), spoken_span(12 * 60_000_000_000), spoken_span(40_000_000_000)), ("14 days".into(), "24 hours".into(), "5 hours".into(), "12 minutes".into(), "40 seconds".into()));
    assert_eq!(spoken_span(335 * 3_600_000_000_000 + 1_800_000_000_000), "14 days", "335h30m rounds to 14 days, not down to 13");

    const H: Ns = 3_600_000 * MS;
    let events = vec![
        ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
        start(2, 0, "root", None, "agent"),
        start(3, 0, "a", Some("root"), "tool"),
        end(4, H, "a", "succeeded"),
        start(5, 10 * H, "b", Some("root"), "tool"),
        end(6, 11 * H, "b", "succeeded"),
        end(7, 11 * H, "root", "succeeded"),
    ];
    let mut doc = TraceDoc::new(TraceState::from_events("s1".into(), &events[..2]), Mode::Replay { replay: Replay::new("s1".into(), events.clone()), clock: ReplayClock::new(10 * H + H / 2) }, 0);
    assert_eq!(doc.replay_times(), Some((H + H / 2, 2 * H, 11 * H)), "an hour and a half into two active hours, eleven hours overall");
    doc.seek_active(0.75);
    let Mode::Replay { clock, .. } = &doc.mode else { unreachable!() };
    assert_eq!(clock.position, 10 * H + H / 2, "three quarters of the active time is past the idle stretch");
}
