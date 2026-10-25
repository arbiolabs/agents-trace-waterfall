//! What fills the main agent's context window at a point in the session. Adapters record no prompt or tool
//! text, so the composition is inferred from token usage: each LLM call's context is input + cache read +
//! cache write, and the growth between consecutive calls is attributed to what happened in between.

use crate::{clock_time, format_tokens, mcp_parts};
use trace_core::{Ns, Span, SpanId, SpanKind, TraceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartKind {
    /// The first call's context: system prompt, tool definitions, memory.
    Baseline,
    /// User prompts that opened a new turn.
    Prompts,
    /// The model's own earlier replies.
    Output,
    /// Tool results, one part per tool.
    Tool,
    /// Growth with no tool or new turn to explain it.
    Other,
    /// What a compaction left behind.
    Summary,
}

impl PartKind {
    pub fn label(self) -> &'static str {
        match self {
            PartKind::Baseline => "System, tools & memory",
            PartKind::Prompts => "Prompts",
            PartKind::Output => "Responses",
            PartKind::Tool => "Tool results",
            PartKind::Other => "Other",
            PartKind::Summary => "Compaction summary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub label: String,
    pub kind: PartKind,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMap {
    /// Baseline first, the rest by tokens, largest first. Sums to `used`.
    pub parts: Vec<Part>,
    pub used: u64,
    /// The model's context window, when known.
    pub limit: Option<u64>,
    /// The LLM call shown.
    pub at: SpanId,
    pub compactions: usize,
    pub model: Option<String>,
}

/// A drop of more than this fraction of the context is a compaction; smaller drops are noise.
const COMPACTION_DROP: u64 = 4;
const ONE_MILLION: u64 = 1_000_000;

/// The main agent's context as of its last LLM call starting at or before `at`. Sub-agents keep their own
/// context, so their calls and tools are left out.
pub fn context_map(state: &TraceState, at: Ns) -> Option<ContextMap> {
    let main = |s: &&Span| !under_subagent(state, s);
    let mut calls: Vec<(&Span, u64)> = state
        .spans
        .values()
        .filter(|s| s.kind == SpanKind::Llm && s.started_at_utc_ns <= at)
        .filter(main)
        .filter_map(|s| Some((s, context_of(s)?)))
        .collect();
    calls.sort_by(|a, b| (a.0.started_at_utc_ns, &a.0.span_id).cmp(&(b.0.started_at_utc_ns, &b.0.span_id)));
    let mut tools: Vec<&Span> = state.spans.values().filter(|s| s.kind == SpanKind::Tool).filter(main).collect();
    tools.sort_by_key(|s| s.started_at_utc_ns);

    let (first, last) = (calls.first()?, calls.last()?);
    let baseline = first.1;
    let mut parts: Vec<Part> = Vec::new();
    let mut compactions = 0;
    for pair in calls.windows(2) {
        let ((prev, before), (next, after)) = (pair[0], pair[1]);
        if after < before {
            if before - after > before / COMPACTION_DROP {
                compactions += 1;
                parts = vec![part(PartKind::Summary, PartKind::Summary.label(), after.saturating_sub(baseline))];
            }
            continue;
        }
        let delta = after - before;
        let output = delta.min(prev.usage.as_ref().and_then(|u| u.tokens_output).unwrap_or(0));
        add(&mut parts, PartKind::Output, PartKind::Output.label(), output);
        let rest = delta - output;
        let (lo, hi) = (prev.started_at_utc_ns, next.started_at_utc_ns);
        let between = &tools[tools.partition_point(|t| t.started_at_utc_ns < lo)..tools.partition_point(|t| t.started_at_utc_ns < hi)];
        if !between.is_empty() {
            let n = between.len() as u64;
            for (i, t) in between.iter().enumerate() {
                let share = rest / n + u64::from(i == 0) * (rest % n);
                add(&mut parts, PartKind::Tool, &tool_label(t), share);
            }
        } else if turn_of(state, prev) != turn_of(state, next) {
            add(&mut parts, PartKind::Prompts, PartKind::Prompts.label(), rest);
        } else {
            add(&mut parts, PartKind::Other, PartKind::Other.label(), rest);
        }
    }

    let used = last.1;
    let baseline = baseline.min(used);
    rescale(&mut parts, used - baseline);
    parts.retain(|p| p.tokens > 0);
    parts.sort_by_key(|p| std::cmp::Reverse(p.tokens));
    parts.insert(0, part(PartKind::Baseline, PartKind::Baseline.label(), baseline));

    let model = last.0.model.clone();
    let peak = calls.iter().map(|c| c.1).max().unwrap_or(0);
    let limit = model.as_deref().and_then(window_of).map(|l| if peak > l { l.max(ONE_MILLION) } else { l }).filter(|l| *l >= peak);
    Some(ContextMap { parts, used, limit, at: last.0.span_id.clone(), compactions, model })
}

/// The context window of a model family, by name.
fn window_of(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    match () {
        _ if m.contains("[1m]") => Some(ONE_MILLION),
        _ if m.starts_with("claude") => Some(200_000),
        _ if m.starts_with("gpt-5") => Some(400_000),
        _ if m.starts_with("gemini") => Some(1_048_576),
        _ => None,
    }
}

/// Tokens the call read: fresh input plus cache. None when it reported no usage.
fn context_of(s: &Span) -> Option<u64> {
    let u = s.usage.as_ref()?;
    let n = u.tokens_input.unwrap_or(0) + u.cache_read.unwrap_or(0) + u.cache_write.unwrap_or(0);
    (n > 0).then_some(n)
}

fn ancestors<'a>(state: &'a TraceState, s: &'a Span) -> impl Iterator<Item = &'a Span> {
    std::iter::successors(Some(s), |s| s.parent_span_id.as_ref().and_then(|p| state.spans.get(p))).skip(1)
}

fn under_subagent(state: &TraceState, s: &Span) -> bool {
    ancestors(state, s).any(|a| a.kind == SpanKind::Subagent)
}

fn turn_of<'a>(state: &'a TraceState, s: &'a Span) -> Option<&'a SpanId> {
    ancestors(state, s).find(|a| a.kind == SpanKind::Turn).map(|t| &t.span_id)
}

/// `Bash`, or `create_issue (github)` for an MCP tool.
fn tool_label(t: &Span) -> String {
    let name = t.tool_name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&t.name);
    mcp_parts(name).map_or_else(|| name.to_owned(), |(tool, server)| format!("{tool} ({server})"))
}

fn part(kind: PartKind, label: &str, tokens: u64) -> Part {
    Part { label: label.to_owned(), kind, tokens }
}

fn add(parts: &mut Vec<Part>, kind: PartKind, label: &str, tokens: u64) {
    match parts.iter_mut().find(|p| p.kind == kind && p.label == label) {
        Some(p) => p.tokens += tokens,
        None => parts.push(part(kind, label, tokens)),
    }
}

/// Scales `parts` to sum to `target` exactly; the rounding remainder goes to the largest part. Growth that
/// nothing explains (all deltas noise) becomes one Other part.
fn rescale(parts: &mut Vec<Part>, target: u64) {
    let sum: u64 = parts.iter().map(|p| p.tokens).sum();
    if sum == 0 {
        parts.clear();
        if target > 0 {
            parts.push(part(PartKind::Other, PartKind::Other.label(), target));
        }
        return;
    }
    for p in parts.iter_mut() {
        p.tokens = (u128::from(p.tokens) * u128::from(target) / u128::from(sum)) as u64;
    }
    let short = target - parts.iter().map(|p| p.tokens).sum::<u64>();
    if let Some(p) = parts.iter_mut().max_by_key(|p| p.tokens) {
        p.tokens += short;
    }
}

/// Squarified treemap (Bruls et al.) of `weights` over a `w` × `h` box, as `(x, y, w, h)` per weight in input
/// order. Weights are laid in the order given, so a caller that wants big tiles first sorts them first.
/// Zero weights get empty rectangles. The rectangles tile the box exactly.
pub fn treemap(weights: &[u64], w: f32, h: f32) -> Vec<(f32, f32, f32, f32)> {
    let mut out = vec![(0.0f64, 0.0, 0.0, 0.0); weights.len()];
    let total: u64 = weights.iter().sum();
    if total == 0 || w <= 0.0 || h <= 0.0 {
        return vec![(0.0, 0.0, 0.0, 0.0); weights.len()];
    }
    let scale = f64::from(w) * f64::from(h) / total as f64;
    let areas: Vec<f64> = weights.iter().map(|&v| v as f64 * scale).collect();
    let live: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0).collect();
    let (mut x, mut y, mut w, mut h) = (0.0f64, 0.0f64, f64::from(w), f64::from(h));
    let mut i = 0;
    while i < live.len() {
        let side = w.min(h);
        let mut j = i + 1;
        while j < live.len() && worst(&live[i..=j], &areas, side) <= worst(&live[i..j], &areas, side) {
            j += 1;
        }
        let row = &live[i..j];
        let last_row = j == live.len();
        let sum: f64 = row.iter().map(|&k| areas[k]).sum();
        let horizontal = w >= h;
        let along = if horizontal { h } else { w };
        let thick = if last_row { if horizontal { w } else { h } } else { sum / along };
        let mut at = 0.0;
        for (n, &k) in row.iter().enumerate() {
            let len = if n + 1 == row.len() { along - at } else { areas[k] / thick };
            out[k] = if horizontal { (x, y + at, thick, len) } else { (x + at, y, len, thick) };
            at += len;
        }
        if horizontal {
            x += thick;
            w -= thick;
        } else {
            y += thick;
            h -= thick;
        }
        i = j;
    }
    out.into_iter().map(|(a, b, c, d)| (a as f32, b as f32, c as f32, d as f32)).collect()
}

/// The worst aspect ratio in a row laid along `side`.
fn worst(row: &[usize], areas: &[f64], side: f64) -> f64 {
    let sum: f64 = row.iter().map(|&k| areas[k]).sum();
    let (max, min) = row.iter().fold((0.0f64, f64::MAX), |(mx, mn), &k| (mx.max(areas[k]), mn.min(areas[k])));
    let (s2, side2) = (sum * sum, side * side);
    (side2 * max / s2).max(s2 / (side2 * min))
}

impl ContextMap {
    /// `64.8k / 200k · 32%`, or `64.8k` when the window is unknown.
    pub fn fill(&self) -> String {
        match self.limit {
            Some(l) => format!("{} / {} · {}%", format_tokens(self.used), format_tokens(l), (self.used * 100 + l / 2) / l),
            None => format_tokens(self.used),
        }
    }

    /// The window left over; zero when unknown.
    pub fn free(&self) -> u64 {
        self.limit.map_or(0, |l| l.saturating_sub(self.used))
    }

    /// `Compacted 2× · showing call at 41:07`, timed from the start of the trace.
    pub fn caption(&self, state: &TraceState) -> String {
        let compacted = match self.compactions {
            0 => "Not compacted".to_owned(),
            n => format!("Compacted {n}×"),
        };
        let at = state.spans.get(&self.at).map_or(0, |s| s.started_at_utc_ns - state.axis_start().unwrap_or(s.started_at_utc_ns));
        format!("{compacted} · showing call at {}", clock_time(at))
    }
}
