//! How spans and sessions read in either frontend: proper harness names, pretty model names, row titles
//! and the muted detail beside them.

use std::collections::HashMap;
use trace_core::{Session, Span, SpanKind, TraceState};

/// `claude-opus-5-5` → `Opus 5.5`, `claude-3-5-sonnet-20241022` → `Sonnet 3.5`. Other models pass through.
pub fn pretty_model(model: &str) -> String {
    let base = model.split('[').next().unwrap_or(model);
    let base = base.rsplit('/').next().unwrap_or(base);
    if let Some(v) = base.strip_prefix("gpt-") {
        return format!("GPT-{v}");
    }
    if let Some(rest) = base.strip_prefix("gemini-") {
        let words: Vec<String> = rest.split('-').map(|w| w[..1].to_uppercase() + &w[1..]).collect();
        return format!("Gemini {}", words.join(" "));
    }
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

/// The muted text after a span's title. The agent's is the model it ran on.
pub fn detail(s: &Span, state: &TraceState) -> Option<String> {
    let nonempty = |v: Option<&str>| v.filter(|v| !v.is_empty()).map(str::to_owned);
    match s.kind {
        SpanKind::Llm => s.model.as_deref().map(pretty_model).or_else(|| nonempty(s.provider.as_deref())),
        SpanKind::Tool if mcp_parts(&s.name).is_some() => mcp_parts(&s.name).map(|(_, server)| server.to_owned()),
        SpanKind::Tool => nonempty(s.tool_name.as_deref())
            .filter(|t| !t.eq_ignore_ascii_case(&s.name))
            .or_else(|| nonempty(s.provider.as_deref())),
        SpanKind::Agent => top_model(state),
        SpanKind::Subagent => s.model.as_deref().map(pretty_model).or_else(|| nonempty(s.agent_name.as_deref()).map(short_id)),
        _ => None,
    }
}

/// Opaque hex ids (harness agent ids) shorten to 7 characters, like git hashes; names are capitalized.
fn short_id(v: String) -> String {
    if v.len() > 12 && v.chars().all(|c| c.is_ascii_hexdigit()) { v[..7].to_owned() } else { capitalize(&v) }
}

/// The row title. An LLM span named after its model reads `LLM call`, since the detail already names the model.
/// An MCP tool name, `mcp__server__tool`, reads as the tool with the server as its detail. The agent reads
/// as its harness, `Claude Code`, else its own name.
pub fn title(s: &Span, session: &Session) -> String {
    let generic = matches!(s.name.to_lowercase().replace(['_', '-'], " ").as_str(), "llm" | "llm call");
    match s.kind {
        SpanKind::Llm if generic || s.model.as_deref() == Some(&s.name) => kind_label(SpanKind::Llm).to_owned(),
        SpanKind::Agent => harness_name(&session.harness, &session.source_adapter)
            .map(str::to_owned)
            .unwrap_or_else(|| capitalize(s.agent_name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&s.name))),
        SpanKind::Turn => capitalize(&s.name),
        _ => mcp_parts(&s.name).map_or_else(|| s.name.clone(), |(tool, _)| tool.to_owned()),
    }
}

/// `mcp__github__create_issue` → `(create_issue, github)`.
pub fn mcp_parts(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((tool, server))
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

/// A known harness's proper name: `Claude-Code` and `claudecode` both read as Claude Code.
fn known_harness(h: &str) -> Option<&'static str> {
    let key: String = h.chars().filter(|c| !matches!(c, '-' | '_' | ' ')).flat_map(char::to_lowercase).collect();
    Some(match key.as_str() {
        "claudecode" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "geminicli" => "Gemini CLI",
        "copilot" | "githubcopilot" => "GitHub Copilot",
        "windsurf" => "Windsurf",
        "cline" => "Cline",
        "roo" | "roocode" => "Roo Code",
        "kilo" | "kilocode" => "Kilo Code",
        "opencode" => "OpenCode",
        "amp" => "Amp",
        "goose" => "Goose",
        "trae" => "Trae",
        "kiro" => "Kiro",
        "openhands" => "OpenHands",
        "junie" => "Junie",
        "devin" => "Devin",
        "pi" => "Pi",
        "omp" | "ohmypi" => "Oh My Pi",
        "qwen" | "qwencode" => "Qwen Code",
        "kimi" | "kimicli" => "Kimi CLI",
        "kimicode" => "Kimi Code",
        "grok" | "grokcli" => "Grok CLI",
        "openclaw" => "OpenClaw",
        "crush" => "Crush",
        "hermes" | "hermesagent" => "Hermes Agent",
        _ => return None,
    })
}

/// A known harness's proper name, from the session's harness or else its source adapter.
pub fn harness_name(harness: &str, source_adapter: &str) -> Option<&'static str> {
    known_harness(harness).or_else(|| known_harness(source_adapter))
}

/// How a session's harness reads: its proper name when known, `Claude Code`, else as recorded.
pub fn harness_label(harness: &str, source_adapter: &str) -> Option<String> {
    harness_name(harness, source_adapter).map(str::to_owned).or_else(|| Some(capitalize(harness)).filter(|h| !h.is_empty()))
}

/// `turn 1` → `Turn 1`.
fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// The trace's most used LLM model, prettified; ties go to the alphabetically first.
pub fn top_model(state: &TraceState) -> Option<String> {
    let mut models: HashMap<&str, usize> = HashMap::new();
    for m in state.spans.values().filter(|x| x.kind == SpanKind::Llm).filter_map(|x| x.model.as_deref()) {
        *models.entry(m).or_default() += 1;
    }
    models.into_iter().max_by_key(|&(m, n)| (n, std::cmp::Reverse(m))).map(|(m, _)| pretty_model(m))
}

#[cfg(test)]
mod tests {
    #[test]
    fn opaque_agent_ids_shorten_but_names_capitalize() {
        assert_eq!(super::short_id("a000974c519f6187a".into()), "a000974");
        assert_eq!(super::short_id("explorer".into()), "Explorer");
    }
}
