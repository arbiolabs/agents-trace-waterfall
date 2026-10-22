//! Pure presentation rules: what each span looks like and says. No GPUI types except icon names.

use gpui_kit::assets::IconName;
use trace_core::{Ns, Span, SpanKind, SpanStatus, Totals, TraceState};
use trace_view::{format_cost, format_duration, format_tokens};

/// Which palette column a span paints with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Agent,
    Llm,
    Tool,
    Error,
}

pub fn tone(s: &Span) -> Tone {
    match (s.status, s.kind) {
        (SpanStatus::Failed, _) => Tone::Error,
        (_, SpanKind::Llm) => Tone::Llm,
        (_, SpanKind::Tool) => Tone::Tool,
        _ => Tone::Agent,
    }
}

/// Glyph color: the kind's ink, Claude's orange, or the wait amber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Kind(Tone),
    Claude,
    Amber,
}

pub fn glyph(s: &Span) -> (IconName, Ink) {
    let ink = Ink::Kind(match s.kind {
        SpanKind::Llm => Tone::Llm,
        SpanKind::Tool => Tone::Tool,
        _ => Tone::Agent,
    });
    match s.kind {
        SpanKind::Llm if is_claude(s) => (IconName::Asterisk, Ink::Claude),
        SpanKind::Llm => (IconName::Sparkles, ink),
        SpanKind::Tool => (tool_icon(s.tool_name.as_deref().unwrap_or(&s.name)), ink),
        SpanKind::Agent => (IconName::Bot, ink),
        SpanKind::Turn => (IconName::MessageSquare, ink),
        SpanKind::Subagent => (IconName::Users, ink),
        SpanKind::Wait => (IconName::Hourglass, Ink::Amber),
        SpanKind::Custom => (IconName::Workflow, ink),
    }
}

fn is_claude(s: &Span) -> bool {
    s.model.as_deref().is_some_and(|m| m.to_lowercase().contains("claude"))
        || s.provider.as_deref().is_some_and(|p| p.eq_ignore_ascii_case("anthropic"))
}

pub fn tool_icon(tool: &str) -> IconName {
    if tool.starts_with("mcp__") {
        return IconName::Plug;
    }
    let words = words(tool);
    let has = |set: &[&str]| words.iter().any(|w| set.contains(&w.as_str()));
    if has(&["web", "fetch", "http", "url", "browse"]) {
        IconName::Globe
    } else if has(&["bash", "shell", "exec", "terminal", "command", "run"]) {
        IconName::SquareTerminal
    } else if has(&["edit", "write", "patch", "multiedit", "notebookedit", "create"]) {
        IconName::Pencil
    } else if has(&["grep", "glob", "search", "find", "ls"]) {
        IconName::Search
    } else if has(&["read", "view", "cat", "open"]) {
        IconName::FileText
    } else if has(&["task", "agent", "subagent"]) {
        IconName::Users
    } else {
        IconName::Wrench
    }
}

/// Lowercase words of an identifier: `read_file`, `MultiEdit`, `web-fetch` → `[read, file]`, `[multi, edit]`, `[web, fetch]`.
fn words(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            out.push(String::new());
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower {
            out.push(String::new());
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        out.last_mut().expect("starts non-empty").extend(c.to_lowercase());
    }
    out.retain(|w| !w.is_empty());
    out
}

/// `claude-opus-5-5` → `Opus 5.5`, `claude-3-5-sonnet-20241022` → `Sonnet 3.5`. Other models pass through.
pub fn pretty_model(model: &str) -> String {
    let base = model.split('[').next().unwrap_or(model);
    let Some(rest) = base.strip_prefix("claude-") else { return model.to_owned() };
    let parts: Vec<&str> = rest.split('-').collect();
    let Some(family) = parts.iter().find(|p| matches!(**p, "opus" | "sonnet" | "haiku")) else { return model.to_owned() };
    let version: Vec<&str> = parts.iter().copied().filter(|p| p.len() <= 2 && p.chars().all(|c| c.is_ascii_digit())).collect();
    let mut name = family[..1].to_uppercase() + &family[1..];
    if !version.is_empty() {
        name = format!("{name} {}", version.join("."));
    }
    name
}

/// The muted text after a span's title.
pub fn detail(s: &Span, harness: &str) -> Option<String> {
    let nonempty = |v: Option<&str>| v.filter(|v| !v.is_empty()).map(str::to_owned);
    match s.kind {
        SpanKind::Llm => s.model.as_deref().map(pretty_model).or_else(|| nonempty(s.provider.as_deref())),
        SpanKind::Tool => nonempty(s.tool_name.as_deref())
            .filter(|t| !t.eq_ignore_ascii_case(&s.name))
            .or_else(|| nonempty(s.provider.as_deref())),
        SpanKind::Agent => nonempty(Some(harness)).or_else(|| nonempty(s.agent_name.as_deref())),
        SpanKind::Subagent => s.model.as_deref().map(pretty_model).or_else(|| nonempty(s.agent_name.as_deref()).map(short_id)),
        _ => None,
    }
}

/// Opaque hex ids (harness agent ids) shorten to 7 characters, like git hashes; names pass through.
fn short_id(v: String) -> String {
    if v.len() > 12 && v.chars().all(|c| c.is_ascii_hexdigit()) { v[..7].to_owned() } else { v }
}

/// The row title. An LLM span named after its model reads `LLM call`, since the detail already names the model.
pub fn title(s: &Span) -> String {
    match (&s.kind, s.model.as_deref()) {
        (SpanKind::Llm, Some(m)) if m == s.name => kind_label(SpanKind::Llm).to_owned(),
        _ => s.name.clone(),
    }
}

pub fn kind_label(k: SpanKind) -> &'static str {
    match k {
        SpanKind::Agent => "Agent",
        SpanKind::Turn => "Turn",
        SpanKind::Llm => "LLM call",
        SpanKind::Tool => "Tool call",
        SpanKind::Subagent => "Sub-agent",
        SpanKind::Wait => "Wait",
        SpanKind::Custom => "Span",
    }
}

/// `6.9s · $0.12`. `rollup` is the span's subtree totals, so a parent reads what its whole branch billed;
/// the cost part shows only when some of it is known.
pub fn bar_label(s: &Span, rollup: &Totals, session_end: Option<Ns>, now: Ns) -> String {
    let d = format_duration(s.elapsed(session_end, now).ns);
    match rollup.cost_usd_micros() {
        Some(c) => format!("{d} · {}", format_cost(Some(c))),
        None => d,
    }
}

/// When the median completed span would draw narrower than 2px at the fitted scale, bars are slivers:
/// the latest turn is the readable place to open on.
pub fn focus_turn(state: &TraceState, ns_per_px: f64) -> Option<&Span> {
    let mut d: Vec<Ns> = state.spans.values().filter_map(|s| Some(s.ended_at_utc_ns? - s.started_at_utc_ns)).collect();
    if d.is_empty() {
        return None;
    }
    let mid = d.len() / 2;
    let median = *d.select_nth_unstable(mid).1;
    if median as f64 / ns_per_px >= 2.0 {
        return None;
    }
    state.spans.values().filter(|s| s.kind == SpanKind::Turn).max_by(|a, b| (a.started_at_utc_ns, &a.span_id).cmp(&(b.started_at_utc_ns, &b.span_id)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LabelAt {
    After,
    Inside,
    Before,
}

pub const LABEL_GAP: f32 = 8.0;
const LABEL_PAD: f32 = 8.0;

/// After the bar when it fits in `width`, else inside when the bar is wide enough, else before it.
pub fn label_at(bar_x: f32, bar_w: f32, label_w: f32, width: f32) -> LabelAt {
    if bar_x + bar_w + LABEL_GAP + label_w <= width {
        LabelAt::After
    } else if label_w + 2.0 * LABEL_PAD <= bar_w {
        LabelAt::Inside
    } else if bar_x - LABEL_GAP - label_w >= 0.0 {
        LabelAt::Before
    } else {
        LabelAt::After
    }
}

/// Left edge of the detail popover: beside the bar so it never hides the span's own subtree,
/// right of it when there is room, else left of it, else pinned to the timeline's right edge.
pub fn popover_x(bar_x0: f32, bar_x1: f32, width: f32, pop_w: f32) -> f32 {
    const GAP: f32 = 16.0;
    if bar_x1 + GAP + pop_w <= width {
        bar_x1.max(0.0) + GAP
    } else if bar_x0 - GAP - pop_w >= 0.0 {
        bar_x0 - GAP - pop_w
    } else {
        (width - pop_w).max(0.0)
    }
}

/// Every descendant of `id`, stopping at `cap`.
pub fn descendants<'a>(state: &'a TraceState, id: &trace_core::SpanId, cap: usize) -> Vec<&'a Span> {
    let mut out: Vec<&'a Span> = Vec::new();
    let mut stack: Vec<&'a trace_core::SpanId> = state.children(id).iter().rev().collect();
    while let Some(k) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        out.push(&state.spans[k]);
        stack.extend(state.children(k).iter().rev());
    }
    out
}

/// The six cells of the popover's stats panel, in reading order. Cost and tokens cover the span's subtree (`rollup`).
pub fn popover_stats(state: &TraceState, s: &Span, rollup: &Totals, now: Ns) -> [(&'static str, String); 6] {
    let origin = state.axis_start().unwrap_or(s.started_at_utc_ns);
    let billed = rollup.billable_records > 0;
    let tokens = |n: u64| if billed { format_tokens(n) } else { "—".into() };
    [
        ("Duration", format_duration(s.elapsed(state.session.ended_at_utc_ns, now).ns)),
        ("Started", format!("+{}", format_duration(s.started_at_utc_ns - origin))),
        ("Cost", format_cost(rollup.cost_usd_micros())),
        ("Tokens in", tokens(rollup.tokens_input)),
        ("Tokens out", tokens(rollup.tokens_output)),
        ("Spans", descendants(state, &s.span_id, usize::MAX).len().to_string()),
    ]
}

/// Sidebar group of a session: live ones first, then by local calendar day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Live,
    Today,
    Earlier,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Live => "Live",
            Group::Today => "Today",
            Group::Earlier => "Earlier",
        }
    }
}

pub fn group(live: bool, started: Option<Ns>, today_start: Ns) -> Group {
    match (live, started) {
        (true, _) => Group::Live,
        (_, Some(t)) if t >= today_start => Group::Today,
        _ => Group::Earlier,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn opaque_agent_ids_shorten_but_names_do_not() {
        assert_eq!(super::short_id("a000974c519f6187a".into()), "a000974");
        assert_eq!(super::short_id("explorer".into()), "explorer");
    }

    use super::*;
    use serde_json::json;
    use trace_core::CanonicalEvent;

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

    fn start(seq: u64, t: Ns, id: &str, parent: Option<&str>, body: serde_json::Value) -> CanonicalEvent {
        let mut b = json!({"name": id, "parent_span_id": parent});
        b.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        ev(seq, t, Some(id), json!({"event_type": "span.started", "body": b}))
    }

    fn state() -> TraceState {
        let events = vec![
            ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p", "harness": "claude-code"}})),
            start(2, 0, "root", None, json!({"kind": "agent"})),
            start(3, 8_100 * MS, "sub", Some("root"), json!({"kind": "subagent", "model": "claude-sonnet-5-5"})),
            start(4, 9_000 * MS, "llm", Some("sub"), json!({"kind": "llm", "model": "claude-sonnet-5-5"})),
            ev(5, 12_000 * MS, Some("llm"), json!({"event_type": "usage.reported", "body": {"tokens_input": 41_200, "tokens_output": 1_400, "cost_usd_micros": 50_000, "cost_provenance": "actual", "is_final": true}})),
            ev(6, 12_000 * MS, Some("llm"), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})),
            start(7, 12_000 * MS, "Bash", Some("sub"), json!({"kind": "tool", "tool_name": "Bash"})),
            ev(8, 13_000 * MS, Some("Bash"), json!({"event_type": "span.ended", "body": {"status": "failed"}})),
            ev(9, 38_100 * MS, Some("sub"), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})),
        ];
        TraceState::from_events("s1".into(), &events)
    }

    #[test]
    fn prettifies_claude_model_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(pretty_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(pretty_model("claude-3-5-sonnet-20241022"), "Sonnet 3.5");
        assert_eq!(pretty_model("claude-haiku-4-5[1m]"), "Haiku 4.5");
        assert_eq!(pretty_model("claude-opus-4-1-20250805"), "Opus 4.1");
        assert_eq!(pretty_model("gpt-4o"), "gpt-4o");
        assert_eq!(pretty_model("claude-instant"), "claude-instant");
    }

    #[test]
    fn maps_tools_to_icons() {
        let cases = [
            ("Bash", IconName::SquareTerminal),
            ("Read", IconName::FileText),
            ("read_file", IconName::FileText),
            ("Edit", IconName::Pencil),
            ("MultiEdit", IconName::Pencil),
            ("write_file", IconName::Pencil),
            ("Grep", IconName::Search),
            ("Glob", IconName::Search),
            ("WebFetch", IconName::Globe),
            ("WebSearch", IconName::Globe),
            ("Task", IconName::Users),
            ("mcp__github__create_issue", IconName::Plug),
            ("TodoWrite", IconName::Pencil),
            ("frobnicate", IconName::Wrench),
        ];
        for (tool, icon) in cases {
            assert_eq!(tool_icon(tool), icon, "{tool}");
        }
    }

    #[test]
    fn glyphs_and_details_follow_kind() {
        let s = state();
        let sub = &s.spans[&"sub".into()];
        let llm = &s.spans[&"llm".into()];
        let bash = &s.spans[&"Bash".into()];
        assert_eq!(glyph(llm), (IconName::Asterisk, Ink::Claude));
        assert_eq!(glyph(sub), (IconName::Users, Ink::Kind(Tone::Agent)));
        assert_eq!(glyph(bash), (IconName::SquareTerminal, Ink::Kind(Tone::Tool)));
        assert_eq!(tone(bash), Tone::Error, "a failed span paints with the error palette");
        assert_eq!(detail(llm, "").as_deref(), Some("Sonnet 5.5"));
        assert_eq!(title(llm), "llm");
        let mut named_after_model = llm.clone();
        named_after_model.name = "claude-sonnet-5-5".into();
        assert_eq!(title(&named_after_model), "LLM call");
        assert_eq!(detail(bash, ""), None, "a tool named after itself gets no detail");
        assert_eq!(detail(&s.spans[&"root".into()], "claude-code").as_deref(), Some("claude-code"));
        let label = |id: &str| bar_label(&s.spans[&id.into()], &s.subtree_totals(&id.into()), None, 40_000 * MS);
        assert_eq!(label("llm"), "3s · $0.05");
        assert_eq!(label("Bash"), "1s");
        assert_eq!(label("sub"), "30s · $0.05", "a parent reads its subtree's cost");
    }

    #[test]
    fn labels_go_after_then_inside_then_before() {
        assert_eq!(label_at(10.0, 100.0, 60.0, 400.0), LabelAt::After);
        assert_eq!(label_at(300.0, 90.0, 60.0, 400.0), LabelAt::Inside);
        assert_eq!(label_at(300.0, 40.0, 60.0, 400.0), LabelAt::Before);
        assert_eq!(label_at(5.0, 390.0, 60.0, 400.0), LabelAt::Inside);
        assert_eq!(label_at(20.0, 30.0, 60.0, 100.0), LabelAt::After, "nowhere fits: after, clipped");
    }

    #[test]
    fn popover_stats_read_from_the_span_and_its_subtree() {
        let s = state();
        let sub = &s.spans[&"sub".into()];
        let stats = popover_stats(&s, sub, &s.subtree_totals(&"sub".into()), 40_000 * MS).map(|(k, v)| format!("{k} {v}"));
        assert_eq!(stats, ["Duration 30s", "Started +8.1s", "Cost $0.05", "Tokens in 41.2k", "Tokens out 1.4k", "Spans 2"]);
        let llm = popover_stats(&s, &s.spans[&"llm".into()], &s.subtree_totals(&"llm".into()), 40_000 * MS).map(|(_, v)| v);
        assert_eq!(llm, ["3s", "+9s", "$0.05", "41.2k", "1.4k", "0"]);
        let bash = popover_stats(&s, &s.spans[&"Bash".into()], &s.subtree_totals(&"Bash".into()), 40_000 * MS).map(|(_, v)| v);
        assert_eq!(bash[2..5], ["—", "—", "—"], "nothing billed reads unknown, not zero");
    }

    #[test]
    fn opens_on_the_latest_turn_only_when_bars_would_be_slivers() {
        let mut events = vec![
            ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
            start(2, 0, "root", None, json!({"kind": "agent"})),
        ];
        for (i, t) in [0, 3_600_000 * MS].into_iter().enumerate() {
            let seq = 10 * (i as u64 + 1);
            let turn = format!("turn{i}");
            events.push(start(seq, t, &turn, Some("root"), json!({"kind": "turn"})));
            events.push(ev(seq + 1, t + 5_000 * MS, Some(&turn), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})));
        }
        let s = TraceState::from_events("s1".into(), &events);
        let hour_over_1000px = 3_605_000.0 * MS as f64 / 1000.0;
        assert_eq!(focus_turn(&s, hour_over_1000px).map(|t| t.name.as_str()), Some("turn1"), "5s turns are 1.4px wide in an hour");
        assert!(focus_turn(&s, 10.0 * MS as f64).is_none(), "at 10ms/px a 5s turn is 500px: the fit is readable");
    }

    #[test]
    fn popover_sits_beside_the_bar() {
        assert_eq!(popover_x(10.0, 100.0, 1000.0, 330.0), 116.0);
        assert_eq!(popover_x(600.0, 900.0, 1000.0, 330.0), 254.0);
        assert_eq!(popover_x(100.0, 900.0, 1000.0, 330.0), 670.0);
        assert_eq!(popover_x(-50.0, -20.0, 1000.0, 330.0), 16.0, "an off-screen bar puts it at the left edge");
    }

    #[test]
    fn groups_sessions_by_liveness_then_day() {
        assert_eq!(group(true, Some(0), 100), Group::Live);
        assert_eq!(group(false, Some(150), 100), Group::Today);
        assert_eq!(group(false, Some(50), 100), Group::Earlier);
        assert_eq!(group(false, None, 100), Group::Earlier);
    }
}
