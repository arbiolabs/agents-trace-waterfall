//! Pure presentation rules: what each span looks like and says. No GPUI types except icon names.

use gpui_kit::assets::IconName;
use trace_core::{Ns, Span, SpanKind, SpanStatus, Totals, TraceState};
use trace_view::{Row, format_cost, format_duration, format_tokens};

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
        SpanKind::Tool if mcp_parts(&s.name).is_some() => mcp_parts(&s.name).map(|(_, server)| server.to_owned()),
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
/// An MCP tool name, `mcp__server__tool`, reads as the tool with the server as its detail.
pub fn title(s: &Span) -> String {
    match (&s.kind, s.model.as_deref()) {
        (SpanKind::Llm, Some(m)) if m == s.name => kind_label(SpanKind::Llm).to_owned(),
        _ => mcp_parts(&s.name).map_or_else(|| s.name.clone(), |(tool, _)| tool.to_owned()),
    }
}

/// `mcp__github__create_issue` → `(create_issue, github)`.
pub fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((tool, server))
}

/// Keeps both ends of a long name, which is where paths and tool names differ: `read_project_fi…_config.json`.
pub fn middle_truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 3 {
        return s.to_owned();
    }
    let tail = (max - 1) / 2;
    let head = max - 1 - tail;
    let chars: Vec<char> = s.chars().collect();
    format!("{}…{}", chars[..head].iter().collect::<String>(), chars[n - tail..].iter().collect::<String>())
}

/// Which timeline edge a span lies beyond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
}

/// For a span wholly outside the visible `window`, the edge it lies past and what the edge hint reads:
/// `2.7s · 1h5m ago` (its length, then how long before the window it ended) or `3m later`.
pub fn offscreen_hint(start: Ns, end: Ns, window: (Ns, Ns)) -> Option<(Edge, String)> {
    if end < window.0 {
        Some((Edge::Left, format!("{} · {} ago", format_duration(end - start), format_duration(window.0 - end))))
    } else if start > window.1 {
        Some((Edge::Right, format!("{} later", format_duration(start - window.1))))
    } else {
        None
    }
}

/// Above this many spans a trace opens with its finished turns folded.
pub const FOLD_ABOVE: usize = 500;

/// Turns to fold when a big trace opens: every turn with children except the latest and any still running.
pub fn default_folds(state: &TraceState) -> Vec<trace_core::SpanId> {
    if state.spans.len() <= FOLD_ABOVE {
        return Vec::new();
    }
    let turns: Vec<&Span> = state.spans.values().filter(|s| s.kind == SpanKind::Turn && !state.children(&s.span_id).is_empty()).collect();
    let latest = turns.iter().max_by(|a, b| (a.started_at_utc_ns, &a.span_id).cmp(&(b.started_at_utc_ns, &b.span_id))).map(|s| s.span_id.clone());
    let mut out: Vec<_> = turns.into_iter().filter(|s| s.ended_at_utc_ns.is_some() && Some(&s.span_id) != latest.as_ref()).map(|s| s.span_id.clone()).collect();
    out.sort();
    out
}

/// The overview's caption: `All · 16.2s` for the whole trace, else `7h18m – 7h31m of 8h12m`.
pub fn overview_label(window: (Ns, Ns), whole: (Ns, Ns)) -> String {
    let total = whole.1 - whole.0;
    if window.0 <= whole.0 && window.1 >= whole.1 {
        return format!("All · {}", format_duration(total));
    }
    let a = (window.0 - whole.0).clamp(0, total);
    let b = (window.1 - whole.0).clamp(a, total);
    format!("{} – {} of {}", format_duration(a), format_duration(b), format_duration(total))
}

/// Overview density: each span counts once in every column it touches, per lane, and a column's alpha
/// is its count over the lane's busiest column, floored so a lone span stays visible.
pub fn density(spans: impl IntoIterator<Item = (f32, f32, usize)>, cols: usize) -> Vec<[f32; 4]> {
    const FLOOR: f32 = 0.18;
    let mut counts = vec![[0u32; 4]; cols];
    if cols == 0 {
        return Vec::new();
    }
    for (x0, x1, lane) in spans {
        let col = |x: f32| (x.floor().max(0.) as usize).min(cols - 1);
        let (a, b) = (col(x0), col(x1));
        counts[a..=b.max(a)].iter_mut().for_each(|c| c[lane] += 1);
    }
    let mut peak = [1u32; 4];
    for c in &counts {
        for k in 0..4 {
            peak[k] = peak[k].max(c[k]);
        }
    }
    counts.iter().map(|c| std::array::from_fn(|k| if c[k] == 0 { 0. } else { (c[k] as f32 / peak[k] as f32).max(FLOOR) })).collect()
}

/// The overview lens never draws narrower than this, so it stays grabbable.
pub const LENS_MIN: f32 = 24.;
const LENS_HANDLE: f32 = 6.;

/// The lens as drawn: the window's true extent widened about its center to `LENS_MIN`, kept inside the strip.
pub fn lens_rect(x0: f32, x1: f32, strip_w: f32) -> (f32, f32) {
    let w = (x1 - x0).max(LENS_MIN).min(strip_w);
    let a = ((x0 + x1 - w) / 2.).clamp(0., (strip_w - w).max(0.));
    (a, a + w)
}

/// What a press at `x` grabs on the drawn lens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    Start,
    End,
    Inside,
    Outside,
}

pub fn lens_grab(x: f32, lens: (f32, f32)) -> Grab {
    if (lens.0 - 2.0..lens.0 + LENS_HANDLE).contains(&x) {
        Grab::Start
    } else if (lens.1 - LENS_HANDLE..=lens.1 + 2.0).contains(&x) {
        Grab::End
    } else if (lens.0..lens.1).contains(&x) {
        Grab::Inside
    } else {
        Grab::Outside
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

pub const POP_GAP: f32 = 6.;
pub const POP_ARROW: f32 = 8.;
const POP_MARGIN: f32 = 12.;

/// Where the detail popover sits, in card coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f32,
    /// Top of the popover body; the arrow sits in the gap between it and the bar.
    pub y: f32,
    pub max_h: f32,
    pub above: bool,
    /// Center of the arrow's base.
    pub arrow_x: f32,
}

/// Below the bar, aligned to its start and kept 12pt inside the card. It flips above only when it would
/// overflow below and there is more room above; either way it stays between `top` (the rows' top edge, so it
/// never covers the stats or axis) and the card bottom, and scrolls instead of growing past them.
/// `bar` is the bar's drawn x extent, `bar_y` its top and bottom, `pop` the popover's width and natural height.
pub fn place_popover(bar: (f32, f32), bar_y: (f32, f32), card: (f32, f32), top: f32, pop: (f32, f32)) -> Placement {
    let (cw, ch) = card;
    let (pw, ph) = pop;
    let x = bar.0.min(cw - POP_MARGIN - pw).max(POP_MARGIN);
    let below = bar_y.1 + POP_GAP + POP_ARROW;
    let room_below = ch - POP_MARGIN - below;
    let above_bottom = bar_y.0 - POP_GAP - POP_ARROW;
    let room_above = above_bottom - top - POP_GAP;
    let above = ph > room_below && room_above > room_below;
    let (y, max_h) = if above { (above_bottom - ph.min(room_above), room_above) } else { (below, room_below) };
    let arrow_x = (bar.0 + ((bar.1 - bar.0) / 2.).min(14.)).clamp(x + 18., x + pw - 18.);
    Placement { x, y, max_h, above, arrow_x }
}

/// Ease-out quint, within 0.01 of the design's cubic-bezier(0.22, 1, 0.36, 1).
pub fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0., 1.)).powi(5)
}

/// What a fold does to one row while it animates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Stay,
    Enter,
    Leave,
}

impl Change {
    /// Height and opacity fraction at eased progress `p`.
    pub fn scale(self, p: f32) -> f32 {
        match self {
            Change::Stay => 1.0,
            Change::Enter => p,
            Change::Leave => 1.0 - p,
        }
    }
}

/// Interleaves the rows before and after a fold. A fold only hides or shows rows, so the shorter
/// list is a subsequence of the longer. Kept rows take their new state, so a parent shows its new chevron.
pub fn fold_merge(old: &[Row], new: &[Row]) -> Vec<(Row, Change)> {
    let entering = new.len() >= old.len();
    let (long, short, extra) = if entering { (new, old, Change::Enter) } else { (old, new, Change::Leave) };
    let mut j = 0;
    long.iter()
        .map(|r| match short.get(j).filter(|s| s.kind == r.kind) {
            Some(s) => {
                j += 1;
                (if entering { r } else { s }.clone(), Change::Stay)
            }
            None => (r.clone(), extra),
        })
        .collect()
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
    fn ease_tracks_the_design_curve() {
        // Points on cubic-bezier(0.22, 1, 0.36, 1) as (x, y).
        for (x, y) in [(0.0, 0.0), (0.192, 0.657), (0.3425, 0.875), (1.0, 1.0)] {
            assert!((ease(x) - y).abs() < 0.01, "ease({x}) = {} vs {y}", ease(x));
        }
        assert_eq!(ease(2.0), 1.0);
    }

    #[test]
    fn fold_merge_marks_hidden_and_shown_rows() {
        let row = |id: &str, collapsed: bool| Row { kind: trace_view::RowKind::Span(id.into()), depth: 0, has_children: false, collapsed, retry: false };
        let open = [row("p", false), row("a", false), row("b", false), row("q", false)];
        let shut = [row("p", true), row("q", false)];
        let marks = |m: Vec<(Row, Change)>| m.into_iter().map(|(r, c)| (r.kind, r.collapsed, c)).collect::<Vec<_>>();
        let id = |s: &str| trace_view::RowKind::Span(s.into());
        assert_eq!(
            marks(fold_merge(&open, &shut)),
            [(id("p"), true, Change::Stay), (id("a"), false, Change::Leave), (id("b"), false, Change::Leave), (id("q"), false, Change::Stay)]
        );
        assert_eq!(
            marks(fold_merge(&shut, &open)),
            [(id("p"), false, Change::Stay), (id("a"), false, Change::Enter), (id("b"), false, Change::Enter), (id("q"), false, Change::Stay)]
        );
        assert_eq!([Change::Stay, Change::Enter, Change::Leave].map(|c| c.scale(0.25)), [1.0, 0.25, 0.75]);
    }
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
    fn popover_anchors_below_the_bar_and_flips_only_on_overflow() {
        let p = place_popover((400., 600.), (100., 116.), (1200., 800.), 0., (330., 300.));
        assert_eq!(p, Placement { x: 400., y: 130., max_h: 658., above: false, arrow_x: 418. });
        let p = place_popover((1000., 1010.), (100., 116.), (1200., 800.), 0., (330., 300.));
        assert_eq!((p.x, p.arrow_x), (858., 1005.), "clamped inside the card; the arrow still points at the bar");
        let p = place_popover((400., 600.), (700., 716.), (1200., 800.), 0., (330., 300.));
        assert_eq!(p, Placement { x: 400., y: 386., max_h: 680., above: true, arrow_x: 418. });
        let p = place_popover((400., 600.), (400., 416.), (1200., 450.), 200., (330., 300.));
        assert_eq!((p.y, p.max_h, p.above), (206., 180., true), "a flipped popover stops 6pt under the rows' top, never over the stats");
        let p = place_popover((400., 600.), (60., 76.), (1200., 400.), 0., (330., 900.));
        assert_eq!((p.y, p.max_h, p.above), (90., 298., false), "tall content scrolls below rather than flipping into less room");
        assert_eq!(place_popover((0., 4.), (0., 16.), (1200., 800.), 0., (330., 100.)).x, 12.);
    }

    #[test]
    fn mcp_tools_read_as_tool_and_server() {
        assert_eq!(mcp_parts("mcp__github__create_issue"), Some(("create_issue", "github")));
        assert_eq!(mcp_parts("mcp__claude_ai_Linear__list_issues"), Some(("list_issues", "claude_ai_Linear")));
        assert_eq!(mcp_parts("mcp__broken"), None);
        assert_eq!(mcp_parts("Bash"), None);
        let s = state();
        let mut t = s.spans[&"Bash".into()].clone();
        t.name = "mcp__github__create_issue".into();
        t.tool_name = Some(t.name.clone());
        assert_eq!((title(&t), detail(&t, "")), ("create_issue".to_owned(), Some("github".to_owned())));
    }

    #[test]
    fn long_titles_keep_both_ends() {
        assert_eq!(middle_truncate("read_project_file_config.json", 15), "read_pr…ig.json");
        assert_eq!(middle_truncate("Bash", 15), "Bash");
        assert_eq!(middle_truncate("abcdef", 5), "ab…ef");
    }

    #[test]
    fn offscreen_spans_name_their_edge_and_distance() {
        let w = (3_900_000 * MS, 4_000_000 * MS);
        let left = offscreen_hint(0, 2_700 * MS, w);
        assert_eq!(left, Some((Edge::Left, "2.7s · 1h4m ago".into())));
        assert_eq!(offscreen_hint(4_180_000 * MS, 4_190_000 * MS, w), Some((Edge::Right, "3m later".into())));
        assert_eq!(offscreen_hint(3_950_000 * MS, 4_190_000 * MS, w), None);
    }

    #[test]
    fn big_traces_fold_finished_turns_but_the_latest() {
        let mut events = vec![ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}}))];
        let mut seq = 2;
        for t in 0..3 {
            let turn = format!("turn{t}");
            let at = t * 1_000 * MS;
            events.push(start(seq, at, &turn, None, json!({"kind": "turn"})));
            for k in 0..200 {
                events.push(start(seq + 1 + k, at, &format!("{turn}.{k}"), Some(&turn), json!({"kind": "tool"})));
            }
            if t < 2 {
                events.push(ev(seq + 500, at + 900 * MS, Some(&turn), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})));
            }
            seq += 1000;
        }
        let s = TraceState::from_events("s1".into(), &events);
        assert_eq!(default_folds(&s), vec![trace_core::SpanId::from("turn0"), "turn1".into()]);
        assert!(default_folds(&state()).is_empty(), "small traces open unfolded");
    }

    #[test]
    fn overview_label_reads_range_and_total() {
        let h = 3_600_000 * MS;
        let m = 60_000 * MS;
        assert_eq!(overview_label((0, 16_200 * MS), (0, 16_200 * MS)), "All · 16.2s");
        assert_eq!(overview_label((7 * h + 18 * m, 7 * h + 31 * m), (0, 8 * h + 12 * m)), "7h18m – 7h31m of 8h12m");
    }

    #[test]
    fn density_scales_each_lane_by_its_busiest_column() {
        let d = density([(0., 3.5, 1), (1., 1., 1), (1., 2., 1), (2., 2., 2)], 5);
        let lane = |k: usize| d.iter().map(|c| c[k]).collect::<Vec<_>>();
        assert_eq!(lane(1), [1. / 3., 1., 2. / 3., 1. / 3., 0.]);
        assert_eq!(lane(2), [0., 0., 1., 0., 0.]);
        let sparse = density((0..1).map(|_| (0., 0., 0)).chain((0..10).map(|_| (1., 1., 0))), 2);
        assert_eq!(sparse.iter().map(|c| c[0]).collect::<Vec<_>>(), [0.18, 1.], "a lone span stays visible");
    }

    #[test]
    fn lens_has_a_minimum_width_and_edge_handles() {
        assert_eq!(lens_rect(100., 104., 500.), (90., 114.));
        assert_eq!(lens_rect(0., 2., 500.), (0., 24.));
        assert_eq!(lens_rect(100., 300., 500.), (100., 300.));
        let l = (90., 114.);
        assert_eq!([89., 95., 102., 109., 115., 140.].map(|x| lens_grab(x, l)), [Grab::Start, Grab::Start, Grab::Inside, Grab::End, Grab::End, Grab::Outside]);
    }

    #[test]
    fn groups_sessions_by_liveness_then_day() {
        assert_eq!(group(true, Some(0), 100), Group::Live);
        assert_eq!(group(false, Some(150), 100), Group::Today);
        assert_eq!(group(false, Some(50), 100), Group::Earlier);
        assert_eq!(group(false, None, 100), Group::Earlier);
    }
}
