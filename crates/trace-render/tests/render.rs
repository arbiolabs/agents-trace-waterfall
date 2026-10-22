use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget;
use serde_json::json;
use std::collections::HashSet;
use std::time::Instant;
use trace_core::*;
use trace_render::*;
use trace_view::{Row, Viewport, flatten};

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

fn lines(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (a.y..a.bottom()).map(|y| (a.x..a.right()).map(|x| buf[(x, y)].symbol()).collect::<String>().trim_end().to_owned()).collect()
}

fn render(state: &TraceState, rows: &[Row], theme: &Theme, w: u16, h: u16, sel: &mut WaterfallState) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    let tl_w = w.saturating_sub(30 + 1).max(1);
    WaterfallWidget { state, rows, now: 800 * MS, viewport: Viewport::fit(0, 1000 * MS, tl_w), origin_ns: 0, theme, tree_width: 30, focused: true }
        .render(area, &mut buf, sel);
    buf
}

#[test]
fn renders_unicode_rows() {
    let s = sample();
    let rows = flatten(&s, &HashSet::new(), None);
    let buf = render(&s, &rows, &Theme::default(), 81, 8, &mut WaterfallState { offset: 0, selected: 2 });
    assert_eq!(
        lines(&buf),
        [
            "                              │0s        200ms     400ms     600ms     800ms",
            "▾◆✓ root                 800ms│████████████████████████████████████████┊",
            "   ●✓ llm1         $0.01 300ms│     ███████████████                    ┊",
            "   ■✗ tool1              100ms│                    ░░░░x               ┊",
            "   ■▶ tool2              300ms│                         ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓┊",
            "  Unresolved                  │                                        ┊",
            "   ◌✓ orphan             100ms│                              █████     ┊",
            "                              │                                        ┊",
        ]
    );
    assert!(buf[(0, 3)].modifier.contains(ratatui::style::Modifier::REVERSED));
}

#[test]
fn renders_ascii_mono_rows() {
    let s = sample();
    let rows = flatten(&s, &HashSet::from([SpanId::from("root")]), None);
    let theme = Theme { glyphs: Glyphs::Ascii, palette: Palette::Mono };
    let buf = render(&s, &rows, &theme, 81, 5, &mut WaterfallState::default());
    assert_eq!(
        lines(&buf),
        [
            "                              |0s        200ms     400ms     600ms     800ms",
            "+Ao root                 800ms|########################################:",
            "  Unresolved                  |                                        :",
            "   Wo orphan             100ms|                              #####     :",
            "                              |                                        :",
        ]
    );
    assert_eq!(buf[(1, 1)].fg, ratatui::style::Color::Reset);
}

#[test]
fn tiny_areas_do_not_panic_and_selection_stays_visible() {
    let s = sample();
    let rows = flatten(&s, &HashSet::new(), None);
    let mut sel = WaterfallState { offset: 0, selected: 99 };
    for (w, h) in [(60, 18), (1, 1), (0, 0), (2, 2), (31, 3), (5, 40)] {
        render(&s, &rows, &Theme::default(), w, h, &mut sel);
    }
    let mut sel = WaterfallState { offset: 0, selected: 5 };
    render(&s, &rows, &Theme::default(), 60, 3, &mut sel);
    assert_eq!(sel, WaterfallState { offset: 4, selected: 5 });
    render(&s, &[], &Theme::default(), 60, 3, &mut sel);
    assert_eq!(sel, WaterfallState { offset: 0, selected: 0 });
}

#[test]
fn ten_thousand_spans_render_fast() {
    let mut events = vec![start(0, 0, "root", None, "agent")];
    for i in 0..10_000u64 {
        let parent = if i % 10 == 0 { "root".to_string() } else { format!("s{}", i - i % 10) };
        let t = i as Ns * MS;
        events.push(start(2 * i + 1, t, &format!("s{i}"), Some(&parent), "tool"));
        events.push(end(2 * i + 2, t + 5 * MS, &format!("s{i}"), "succeeded"));
    }
    let s = TraceState::from_events("s1".into(), &events);
    assert_eq!(s.spans.len(), 10_001);
    let area = Rect::new(0, 0, 200, 60);
    let mut buf = Buffer::empty(area);
    let t0 = Instant::now();
    let rows = flatten(&s, &HashSet::new(), None);
    let mut sel = WaterfallState { offset: 0, selected: 5_000 };
    WaterfallWidget { state: &s, rows: &rows, now: 10_000 * MS, viewport: Viewport::fit(0, 10_000 * MS, 160), origin_ns: 0, theme: &Theme::default(), tree_width: 40, focused: true }
        .render(area, &mut buf, &mut sel);
    let took = t0.elapsed();
    println!("flatten+render 10k spans: {took:?}");
    assert_eq!(rows.len(), 10_001);
    assert!(took.as_millis() < 100, "{took:?}");
}

#[test]
fn tree_width_equal_to_area_draws_tree_only() {
    let s = sample();
    let rows = flatten(&s, &HashSet::from([SpanId::from("root")]), None);
    let area = Rect::new(0, 0, 30, 4);
    let mut buf = Buffer::empty(area);
    let theme = Theme { glyphs: Glyphs::Ascii, palette: Palette::Mono };
    WaterfallWidget { state: &s, rows: &rows, now: 800 * MS, viewport: Viewport::fit(0, 1000 * MS, 1), origin_ns: 0, theme: &theme, tree_width: 30, focused: true }
        .render(area, &mut buf, &mut WaterfallState::default());
    assert_eq!(lines(&buf), ["", "+Ao root                 800ms", "  Unresolved", "   Wo orphan             100ms"]);
}
