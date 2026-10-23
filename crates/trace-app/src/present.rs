//! Pure presentation rules: what each span looks like and says. No GPUI types except icon names.

use gpui_kit::assets::IconName;
use gpui_kit::{Hsla, rgb};
use trace_core::{Ns, SessionStatus, Span, SpanKind, SpanStatus, Totals, TraceState};
use trace_storage::SessionSummary;
pub use trace_view::{MAX_GAPS, detail, harness_label, harness_name, idle_gaps, kind_label, mcp_parts, pretty_model, skip_idle, title, top_model};
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

/// Glyph color: the kind's ink or the wait amber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Kind(Tone),
    Amber,
}

pub fn glyph(s: &Span) -> (IconName, Ink) {
    let ink = Ink::Kind(match s.kind {
        SpanKind::Llm => Tone::Llm,
        SpanKind::Tool => Tone::Tool,
        _ => Tone::Agent,
    });
    match s.kind {
        SpanKind::Llm => (IconName::Sparkles, ink),
        SpanKind::Tool => (tool_category(s.tool_name.as_deref().unwrap_or(&s.name)).icon(), ink),
        SpanKind::Agent => (IconName::Bot, ink),
        SpanKind::Turn => (IconName::MessageSquare, ink),
        SpanKind::Subagent => (IconName::Users, ink),
        SpanKind::Wait => (IconName::Hourglass, Ink::Amber),
        SpanKind::Custom => (IconName::Workflow, ink),
    }
}

macro_rules! logos {
    ($($id:ident = $file:literal),* $(,)?) => {
        /// A brand mark embedded from `assets/brands/`; see its NOTICE.md.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Logo { $($id),* }

        impl Logo {
            pub const ALL: &[Logo] = &[$(Logo::$id),*];

            pub fn file(self) -> &'static str {
                match self { $(Logo::$id => $file),* }
            }

            pub fn svg(self) -> &'static [u8] {
                match self { $(Logo::$id => include_bytes!(concat!("../assets/brands/", $file, ".svg"))),* }
            }
        }
    };
}

logos! {
    Amp = "amp-color", Claude = "claude-color", ClaudeCode = "claudecode-color", Cline = "cline", Codex = "codex-color",
    Cursor = "cursor", DeepSeek = "deepseek-color", Devin = "devin-color", Docker = "docker", Figma = "figma-color",
    Gemini = "gemini-color", GeminiCli = "geminicli-color", Git = "git", GitHub = "github", Copilot = "githubcopilot",
    GoogleDrive = "googledrive", Goose = "goose", Grok = "grok", Junie = "junie-color", KiloCode = "kilocode",
    Kimi = "kimi-color", Kiro = "kiro-color", Linear = "linear", Meta = "meta-color", Mistral = "mistral-color",
    Notion = "notion", Ollama = "ollama", OpenAi = "openai", OpenCode = "opencode", OpenHands = "openhands-color",
    Perplexity = "perplexity-color", Postgres = "postgresql", Qwen = "qwen-color", RooCode = "roocode", Stripe = "stripe",
    Trae = "trae-color", Vercel = "vercel", Windsurf = "windsurf", Jira = "jira", Confluence = "confluence",
    Sentry = "sentry", Supabase = "supabase", Asana = "asana", Gmail = "gmail", GoogleCalendar = "googlecalendar",
    GoogleDocs = "googledocs", GoogleSheets = "googlesheets", Cloudflare = "cloudflare", Npm = "npm", Pnpm = "pnpm",
    Rust = "rust", Python = "python", Datadog = "datadog", Grafana = "grafana", MongoDb = "mongodb", Redis = "redis",
    Airtable = "airtable", Trello = "trello", Discord = "discord", Telegram = "telegram", GitLab = "gitlab",
    Bitbucket = "bitbucket",
}

impl Logo {
    /// The asset path the app's source serves, `brands/claude-color.svg`.
    pub fn asset(self) -> String {
        format!("brands/{}.svg", self.file())
    }

    pub fn by_file(file: &str) -> Option<Logo> {
        Logo::ALL.iter().copied().find(|l| l.file() == file)
    }

    /// Full-color marks draw as images; the rest are masks tinted like text.
    pub fn is_color(self) -> bool {
        self.file().ends_with("-color")
    }

    /// Outline and thin-stroke marks cover less of their box than solid ones, so they draw larger to
    /// carry the same optical weight: in a 20pt tile a solid mark is 12pt and an outline mark 14pt.
    pub fn outline(self) -> bool {
        matches!(
            self,
            Logo::OpenAi | Logo::Cursor | Logo::Claude | Logo::Cline | Logo::OpenCode | Logo::KiloCode | Logo::Notion
                | Logo::Linear | Logo::Windsurf | Logo::Perplexity | Logo::Ollama | Logo::Copilot | Logo::Gemini
                | Logo::Goose | Logo::RooCode | Logo::Grok
        )
    }

    /// The mark's size inside a `tile` point tile.
    pub fn size_in(self, tile: f32) -> f32 {
        (tile * if self.outline() { 0.7 } else { 0.6 }).round()
    }

    /// The mark to draw at sidebar and header sizes: the Claude Code mascot smudges at 16pt, so it reads as
    /// the Claude spark; Codex reads as the OpenAI knot.
    pub fn small(self) -> Logo {
        match self {
            Logo::ClaudeCode => Logo::Claude,
            Logo::Codex => Logo::OpenAi,
            l => l,
        }
    }
}

/// A span's brand mark. `tint` colors a single-color mark; `None` tints it with the ink color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Brand {
    pub logo: Logo,
    pub tint: Option<Hsla>,
}

impl Brand {
    fn plain(logo: Logo) -> Self {
        Self { logo, tint: None }
    }
}


/// The harness logo for a session, from its harness or else its source adapter.
pub fn harness_brand(harness: &str, source_adapter: &str) -> Option<Brand> {
    let logo = match harness_name(harness, source_adapter)? {
        "Claude Code" => Logo::ClaudeCode,
        "Codex" => Logo::Codex,
        "Cursor" => Logo::Cursor,
        "Gemini CLI" => Logo::GeminiCli,
        "GitHub Copilot" => Logo::Copilot,
        "Windsurf" => Logo::Windsurf,
        "Cline" => Logo::Cline,
        "Roo Code" => Logo::RooCode,
        "Kilo Code" => Logo::KiloCode,
        "OpenCode" => Logo::OpenCode,
        "Amp" => Logo::Amp,
        "Goose" => Logo::Goose,
        "Trae" => Logo::Trae,
        "Kiro" => Logo::Kiro,
        "OpenHands" => Logo::OpenHands,
        "Junie" => Logo::Junie,
        "Devin" => Logo::Devin,
        _ => return None,
    };
    Some(Brand::plain(logo))
}





/// The model vendor's logo, by model prefix, else by provider.
pub fn vendor_brand(model: Option<&str>, provider: Option<&str>) -> Option<Brand> {
    let model = model.unwrap_or("").to_lowercase();
    let model = model.rsplit('/').next().unwrap_or("");
    let provider = provider.unwrap_or("").to_lowercase();
    let pre = |ps: &[&str]| ps.iter().any(|p| model.starts_with(p));
    let logo = if pre(&["claude"]) || provider == "anthropic" {
        Logo::Claude
    } else if pre(&["gpt", "o1", "o3", "o4", "codex"]) {
        Logo::OpenAi
    } else if pre(&["gemini"]) {
        Logo::Gemini
    } else if pre(&["mistral", "codestral"]) {
        Logo::Mistral
    } else if pre(&["llama"]) {
        Logo::Meta
    } else if pre(&["deepseek"]) {
        Logo::DeepSeek
    } else if pre(&["qwen"]) {
        Logo::Qwen
    } else if pre(&["grok"]) {
        Logo::Grok
    } else if pre(&["kimi", "moonshot"]) {
        Logo::Kimi
    } else if pre(&["perplexity", "sonar"]) {
        Logo::Perplexity
    } else if provider == "ollama" {
        Logo::Ollama
    } else {
        return None;
    };
    Some(Brand::plain(logo))
}

/// Services by the word that names them, with their Simple Icons color. Short or generic words
/// (`gh`, `git`, `docs`, `sheets`) count only as a whole name, so `mcp__context7_docs__…` stays unbranded.
const SERVICES: &[(&[&str], Logo, u32, bool)] = &[
    (&["github"], Logo::GitHub, 0x181717, false),
    (&["gh"], Logo::GitHub, 0x181717, true),
    (&["git"], Logo::Git, 0xF03C2E, true),
    (&["gitlab"], Logo::GitLab, 0xFC6D26, false),
    (&["bitbucket"], Logo::Bitbucket, 0x0052CC, false),
    (&["linear"], Logo::Linear, 0x5E6AD2, false),
    (&["jira", "atlassian"], Logo::Jira, 0x0052CC, false),
    (&["confluence"], Logo::Confluence, 0x172B4D, false),
    (&["notion"], Logo::Notion, 0x000000, false),
    (&["figma"], Logo::Figma, 0xF24E1E, false),
    (&["stripe"], Logo::Stripe, 0x635BFF, false),
    (&["sentry"], Logo::Sentry, 0x362D59, false),
    (&["supabase"], Logo::Supabase, 0x3FCF8E, false),
    (&["vercel"], Logo::Vercel, 0x000000, false),
    (&["cloudflare"], Logo::Cloudflare, 0xF38020, false),
    (&["googledrive", "gdrive"], Logo::GoogleDrive, 0x4285F4, false),
    (&["gmail"], Logo::Gmail, 0xEA4335, false),
    (&["googlecalendar", "gcal"], Logo::GoogleCalendar, 0x4285F4, false),
    (&["googledocs", "gdocs"], Logo::GoogleDocs, 0x4285F4, false),
    (&["docs"], Logo::GoogleDocs, 0x4285F4, true),
    (&["googlesheets", "gsheets"], Logo::GoogleSheets, 0x34A853, false),
    (&["sheets"], Logo::GoogleSheets, 0x34A853, true),
    (&["postgres", "postgresql", "psql"], Logo::Postgres, 0x4169E1, false),
    (&["mongodb", "mongo"], Logo::MongoDb, 0x47A248, false),
    (&["redis"], Logo::Redis, 0xFF4438, false),
    (&["docker"], Logo::Docker, 0x2496ED, false),
    (&["datadog"], Logo::Datadog, 0x632CA6, false),
    (&["grafana"], Logo::Grafana, 0xF46800, false),
    (&["airtable"], Logo::Airtable, 0x18BFFF, false),
    (&["trello"], Logo::Trello, 0x0052CC, false),
    (&["asana"], Logo::Asana, 0xF06A6A, false),
    (&["discord"], Logo::Discord, 0x5865F2, false),
    (&["telegram"], Logo::Telegram, 0x26A5E4, false),
    (&["npm"], Logo::Npm, 0xCB3837, true),
    (&["pnpm"], Logo::Pnpm, 0xF69220, true),
    (&["python", "python3"], Logo::Python, 0x3776AB, true),
    (&["rustc", "rustup"], Logo::Rust, 0x000000, true),
];

/// The service a tool talks to: the MCP server of `mcp__<server>__tool` (whole, or any word of it, so
/// `claude_ai_Linear` reads as Linear), else the tool name's first word: `github_*`, `gh`, `git`, `notion-*`.
pub fn tool_brand(tool: &str) -> Option<Brand> {
    let service = |w: &str, whole: bool| SERVICES.iter().find(|(names, _, _, only_whole)| names.contains(&w) && (whole || !only_whole));
    let found = match mcp_parts(tool) {
        Some((_, server)) => {
            let ws = words(server);
            service(&ws.concat(), true).or_else(|| ws.iter().find_map(|w| service(w, false)))
        }
        None => words(tool).first().and_then(|w| service(w, true)),
    };
    found.map(|&(_, logo, hex, _)| Brand { logo, tint: Some(rgb(hex).into()) })
}

/// The brand mark a span's icon tile shows, or `None` to keep its lucide glyph. The agent shows the
/// harness, an LLM call its vendor, a tool its service, and a subagent its model's vendor.
pub fn brand_for(s: &Span, harness: Option<Brand>) -> Option<Brand> {
    match s.kind {
        SpanKind::Agent => harness,
        SpanKind::Llm | SpanKind::Subagent => vendor_brand(s.model.as_deref(), s.provider.as_deref()),
        SpanKind::Tool => tool_brand(s.tool_name.as_deref().unwrap_or(&s.name)).or_else(|| tool_brand(&s.name)),
        _ => None,
    }
}

/// What a tool does, for tools without a brand. Each category draws one lucide glyph in the tool ink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Shell,
    Read,
    Write,
    Edit,
    Search,
    Web,
    Browser,
    Agent,
    Plan,
    Memory,
    Database,
    Git,
    Build,
    Test,
    Ask,
    Mcp,
    Other,
}

impl ToolCategory {
    pub fn icon(self) -> IconName {
        match self {
            Self::Shell => IconName::SquareTerminal,
            Self::Read => IconName::FileText,
            Self::Write => IconName::FilePlus,
            Self::Edit => IconName::FilePen,
            Self::Search => IconName::Search,
            Self::Web => IconName::Globe,
            Self::Browser => IconName::AppWindow,
            Self::Agent => IconName::Users,
            Self::Plan => IconName::ListChecks,
            Self::Memory => IconName::Brain,
            Self::Database => IconName::Database,
            Self::Git => IconName::GitBranch,
            Self::Build => IconName::Package,
            Self::Test => IconName::FlaskConical,
            Self::Ask => IconName::MessageCircleQuestionMark,
            Self::Mcp => IconName::Plug,
            Self::Other => IconName::Wrench,
        }
    }
}

/// Rules in priority order: the first category with a word of the tool name wins, so `WebSearch`
/// is Web before Search and `TodoWrite` is Plan before Edit.
const CATEGORY_WORDS: &[(ToolCategory, &[&str])] = &[
    (ToolCategory::Ask, &["ask", "approval"]),
    (ToolCategory::Plan, &["todo", "plan", "skill"]),
    (ToolCategory::Agent, &["task", "agent", "subagent"]),
    (ToolCategory::Memory, &["mem", "memory", "remember"]),
    (ToolCategory::Browser, &["playwright", "puppeteer", "browser", "computer", "screenshot", "click"]),
    (ToolCategory::Web, &["web", "fetch", "http", "url", "curl", "browse"]),
    (ToolCategory::Test, &["test", "tests", "pytest", "jest", "vitest"]),
    (ToolCategory::Build, &["npm", "cargo", "make", "build", "yarn", "pnpm"]),
    (ToolCategory::Git, &["git", "commit", "branch", "checkout", "rebase", "merge"]),
    (ToolCategory::Database, &["sql", "psql", "sqlite", "query", "database", "db"]),
    (ToolCategory::Shell, &["bash", "shell", "exec", "terminal", "command", "run"]),
    (ToolCategory::Edit, &["edit", "patch", "multiedit", "notebookedit", "replace"]),
    (ToolCategory::Write, &["write", "create", "new"]),
    (ToolCategory::Search, &["grep", "glob", "search", "find", "ls"]),
    (ToolCategory::Read, &["read", "view", "cat", "open"]),
];

/// The category of a tool name. An MCP tool classifies by its tool part, then its server, then reads as Mcp.
pub fn tool_category(tool: &str) -> ToolCategory {
    let classify = |name: &str| {
        let words = words(name);
        CATEGORY_WORDS.iter().find(|(_, set)| words.iter().any(|w| set.contains(&w.as_str()))).map(|(c, _)| *c)
    };
    match mcp_parts(tool) {
        Some((t, server)) => classify(t).or_else(|| classify(server)).unwrap_or(ToolCategory::Mcp),
        None if tool.starts_with("mcp__") => ToolCategory::Mcp,
        None => classify(tool).unwrap_or(ToolCategory::Other),
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
/// its length, then how far past the window it lies: `2.7s · 1h5m before`, `3m after`. Only while
/// following, when the window ends at now, does a span behind it read from real now: `2.7s · 7h13m ago`.
pub fn offscreen_hint(start: Ns, end: Ns, window: (Ns, Ns), following_now: Option<Ns>) -> Option<(Edge, String)> {
    if end < window.0 {
        let gap = match following_now {
            Some(now) => format!("{} ago", format_duration(now - end)),
            None => format!("{} before", format_duration(window.0 - end)),
        };
        Some((Edge::Left, format!("{} · {gap}", format_duration((end - start).max(0)))))
    } else if start > window.1 {
        Some((Edge::Right, format!("{} after", format_duration(start - window.1))))
    } else {
        None
    }
}

/// A live session with no event for this long reads as idle: its open spans stop at the last event.
pub const IDLE_AFTER: Ns = 10 * 60 * 1_000_000_000;

/// When a live session went quiet: its last event, if that is more than `IDLE_AFTER` before `now`.
pub fn idle_since(state: &TraceState, live: bool, now: Ns) -> Option<Ns> {
    let last = state.last_occurred_at_utc_ns?;
    (live && state.session.ended_at_utc_ns.is_none() && now - last > IDLE_AFTER).then_some(last)
}

/// Where open spans stop drawing: the session's end, or the last event while idle. Feed it to
/// `Span::effective_end` as the session end, so open spans neither grow forever nor outlive the evidence.
pub fn shown_end(state: &TraceState, idle: Option<Ns>) -> Option<Ns> {
    state.session.ended_at_utc_ns.or(idle)
}

/// The capsule pinned at the timeline edge while now is off-screen: `Now +6h49m`, the distance from
/// the view's right edge to now, or `Now −3m` when the view lies past it. While idle the overview's
/// idle marker already states the gap, so the capsule reads just `Now` rather than a second, nearly equal figure.
pub fn now_capsule(now: Ns, window: (Ns, Ns), idle: bool) -> Option<(Edge, String)> {
    if now > window.1 {
        Some((Edge::Right, if idle { "Now".into() } else { format!("Now +{}", format_duration(now - window.1)) }))
    } else if now < window.0 {
        Some((Edge::Left, format!("Now \u{2212}{}", format_duration(window.0 - now))))
    } else {
        None
    }
}

/// A session's duration, by one rule for the sidebar and the header: start to end once ended; start to now
/// while live and active; start to the last event while idle or when the end was never recorded.
pub fn session_duration(started: Option<Ns>, ended: Option<Ns>, last_event: Option<Ns>, live: bool, now: Ns) -> Option<Ns> {
    let start = started?;
    let end = match (ended, live && last_event.is_none_or(|t| now - t <= IDLE_AFTER)) {
        (Some(e), _) => e,
        (None, true) => now,
        (None, false) => last_event?,
    };
    Some((end - start).max(0))
}



/// The overview's time scale with each idle stretch squeezed to a narrow band, so a two-week session's
/// bursts of work get the width instead of the nights between them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Squeeze {
    a: Ns,
    b: Ns,
    pub gap_w: f32,
    /// Strip pixels per nanosecond of activity.
    k: f64,
    gaps: [(Ns, Ns); MAX_GAPS],
    n: usize,
}

impl Squeeze {
    pub const GAP_W: f32 = 28.;

    pub fn new((a, b): (Ns, Ns), w: f32, gaps: &[(Ns, Ns)]) -> Self {
        let n = gaps.len().min(MAX_GAPS);
        let mut arr = [(0, 0); MAX_GAPS];
        arr[..n].copy_from_slice(&gaps[..n]);
        // Bands take at most 40% of the strip.
        let gap_w = if n == 0 { 0. } else { Self::GAP_W.min(w * 0.4 / n as f32) };
        let active = ((b - a) - arr[..n].iter().map(|(x, y)| y - x).sum::<Ns>()).max(1);
        Self { a, b, gap_w, k: f64::from(w - n as f32 * gap_w) / active as f64, gaps: arr, n }
    }

    pub fn gaps(&self) -> &[(Ns, Ns)] {
        &self.gaps[..self.n]
    }

    /// Strip x of `t`, pinned to the strip: a window running past the session's ends stops at its edges.
    pub fn x(&self, t: Ns) -> f32 {
        let t = t.clamp(self.a, self.b);
        let (mut x, mut from) = (0f64, self.a);
        for &(g0, g1) in self.gaps() {
            if t < g0 {
                break;
            }
            x += (g0 - from) as f64 * self.k;
            if t < g1 {
                return (x + (t - g0) as f64 / (g1 - g0) as f64 * f64::from(self.gap_w)) as f32;
            }
            x += f64::from(self.gap_w);
            from = g1;
        }
        (x + (t - from) as f64 * self.k) as f32
    }

    pub fn t(&self, x: f32) -> Ns {
        let (x, mut x0, mut from) = (f64::from(x), 0f64, self.a);
        for &(g0, g1) in self.gaps() {
            let band = x0 + (g0 - from) as f64 * self.k;
            if x < band {
                break;
            }
            if x < band + f64::from(self.gap_w) {
                return g0 + ((x - band) / f64::from(self.gap_w) * (g1 - g0) as f64) as Ns;
            }
            x0 = band + f64::from(self.gap_w);
            from = g1;
        }
        from + ((x - x0) / self.k) as Ns
    }

    /// The idle stretch whose band holds strip x.
    pub fn band_at(&self, x: f32) -> Option<(Ns, Ns)> {
        self.gaps().iter().copied().find(|&(g0, _)| (0. ..self.gap_w).contains(&(x - self.x(g0))))
    }
}


/// `47h`, `3d`, `35m`: an idle band's label in the few points it has.
pub fn compact_duration(ns: Ns) -> String {
    let m = ns / 60_000_000_000;
    match m {
        m if m >= 48 * 60 => format!("{}d", m / (24 * 60)),
        m if m >= 60 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

pub fn idle_gap(last: Ns, now: Ns) -> String {
    format!("+{} idle", format_duration(now - last))
}

/// One status vocabulary, so the sidebar badge, the header pill, and the popover pill always agree on
/// color and shape. Status is never color alone: each has its own glyph shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Live,
    Idle,
    Paused,
    Done,
    Failed,
    Interrupted,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Accent,
    Amber,
    Ok,
    Error,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Dot,
    Ring,
    Check,
    Cross,
    Pause,
}

impl Status {
    pub fn look(self) -> (Hue, Shape) {
        match self {
            Status::Live => (Hue::Accent, Shape::Dot),
            Status::Idle => (Hue::Amber, Shape::Ring),
            Status::Paused => (Hue::Neutral, Shape::Pause),
            Status::Done => (Hue::Ok, Shape::Check),
            Status::Failed => (Hue::Error, Shape::Cross),
            Status::Interrupted => (Hue::Amber, Shape::Pause),
            Status::Unknown => (Hue::Neutral, Shape::Ring),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Live => "Live",
            Status::Idle => "Idle",
            Status::Paused => "Paused",
            Status::Done => "Done",
            Status::Failed => "Failed",
            Status::Interrupted => "Interrupted",
            Status::Unknown => "Unknown",
        }
    }
}

/// A stored session's status as of `now`: a live session quiet for `IDLE_AFTER` reads as idle.
pub fn session_status(status: SessionStatus, last_event: Option<Ns>, now: Ns) -> Status {
    match status {
        SessionStatus::Live if last_event.is_some_and(|t| now - t > IDLE_AFTER) => Status::Idle,
        SessionStatus::Live => Status::Live,
        SessionStatus::Completed => Status::Done,
        SessionStatus::Failed => Status::Failed,
        SessionStatus::Interrupted => Status::Interrupted,
        SessionStatus::Unknown => Status::Unknown,
    }
}

/// A span's status for its popover pill. `quiet` is an open span in an idle session.
pub fn span_status(s: SpanStatus, open: bool, quiet: bool) -> (Status, &'static str) {
    match s {
        SpanStatus::Failed => (Status::Failed, "Failed"),
        SpanStatus::Succeeded => (Status::Done, "Done"),
        SpanStatus::Cancelled => (Status::Interrupted, "Cancelled"),
        SpanStatus::Running if quiet => (Status::Idle, "Idle"),
        SpanStatus::Running if open => (Status::Live, "Running"),
        _ => (Status::Unknown, "Unknown"),
    }
}

/// Whether a sidebar row shows its status badge. A finished session in Earlier is the common case, so it
/// goes without one and the rows that need attention stand out.
pub fn shows_badge(status: Status, group: Group) -> bool {
    !(status == Status::Done && group == Group::Earlier)
}

/// Where an axis tick label starts: 5pt right of its tick, but never closer than 8pt to the card's edge
/// at `right`, so the last label never touches it.
pub fn tick_label_x(tick_x: f32, label_w: f32, right: f32) -> f32 {
    (tick_x + 5.).min(right - 8. - label_w)
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

/// The overview's caption: `All · 16.2s` for the whole trace, else `7h18m – 7h31m of 8h12m`. When both
/// ends would read the same, it names the window's length and place instead: `4.4s window at 2h4m · 2h4m total`.
pub fn overview_label(window: (Ns, Ns), whole: (Ns, Ns)) -> String {
    let total = whole.1 - whole.0;
    if window.0 <= whole.0 && window.1 >= whole.1 {
        return format!("All · {}", format_duration(total));
    }
    let a = (window.0 - whole.0).clamp(0, total);
    let b = (window.1 - whole.0).clamp(a, total);
    let (fa, fb, ft) = (format_duration(a), format_duration(b), format_duration(total));
    if fa == fb {
        return format!("{} window at {fa} · {ft} total", format_duration(window.1 - window.0));
    }
    format!("{fa} – {fb} of {ft}")
}

/// Overview lanes: wait, LLM, tool. Errors draw as ticks on top instead of a lane.
pub const LANES: usize = 3;

/// Overview density: per lane, each pixel column sums how much of it spans cover, so a column with
/// three overlapping calls reads three times as busy as one with a single call. A column's alpha is the
/// square root of its coverage over the lane's busiest column, floored so a lone instant stays visible.
/// Spans are `(x0, x1, lane)` in columns.
pub fn density(spans: impl IntoIterator<Item = (f32, f32, usize)>, cols: usize) -> Vec<[f32; LANES]> {
    const FLOOR: f32 = 0.18;
    // An instant still occupies a sliver, so a burst of zero-length events adds up.
    const MIN_W: f32 = 0.05;
    if cols == 0 {
        return Vec::new();
    }
    let mut cover = vec![[0f32; LANES]; cols];
    for (x0, x1, lane) in spans {
        let (x0, x1) = (x0.clamp(0., cols as f32 - MIN_W), x1.clamp(0., cols as f32));
        let x1 = x1.max(x0 + MIN_W);
        for (c, cell) in cover.iter_mut().enumerate().take(x1.ceil() as usize).skip(x0.floor() as usize) {
            cell[lane] += x1.min(c as f32 + 1.) - x0.max(c as f32);
        }
    }
    let mut peak = [f32::EPSILON; LANES];
    for c in &cover {
        for k in 0..LANES {
            peak[k] = peak[k].max(c[k]);
        }
    }
    cover.iter().map(|c| std::array::from_fn(|k| if c[k] <= 0. { 0. } else { (c[k] / peak[k]).sqrt().max(FLOOR) })).collect()
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


/// `6.9s · $0.12`. `rollup` is the span's subtree totals, so a parent reads what its whole branch billed;
/// the cost part shows only when some of it is known.
pub fn bar_label(s: &Span, rollup: &Totals, session_end: Option<Ns>, now: Ns) -> String {
    let d = format_duration(s.elapsed(session_end, now).ns);
    match rollup.cost_usd_micros() {
        Some(c) => format!("{d} · {}", format_cost(Some(c))),
        None => d,
    }
}

/// What VoiceOver reads for a row: `LLM call, Opus 5.5, 1.6 seconds, 2 cents, succeeded`.
pub fn row_label(s: &Span, state: &TraceState, rollup: &Totals, session_end: Option<Ns>, now: Ns) -> String {
    let status = match s.status {
        SpanStatus::Succeeded => "succeeded",
        SpanStatus::Failed => "failed",
        SpanStatus::Cancelled => "cancelled",
        SpanStatus::Running => "running",
        _ => "status unknown",
    };
    [Some(title(s, &state.session)), detail(s, state), Some(spoken_duration(s.elapsed(session_end, now).ns)), rollup.cost_usd_micros().map(spoken_cost), Some(status.to_owned())]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// `1.6 seconds`, `250 milliseconds`, `2 minutes 30 seconds`, `1 hour 5 minutes`.
pub fn spoken_duration(ns: Ns) -> String {
    let unit = |n: u64, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    let ms = ns.max(0) / 1_000_000;
    if ms < 1_000 {
        return unit(ms as u64, "millisecond");
    }
    if ms < 60_000 {
        let secs = format!("{:.1}", ms as f64 / 1e3);
        let secs = secs.strip_suffix(".0").unwrap_or(&secs);
        return if secs == "1" { "1 second".into() } else { format!("{secs} seconds") };
    }
    let s = (ms / 1_000) as u64;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    let parts = [(h, "hour"), (m, "minute"), (s, "second")];
    let first = parts.iter().position(|(n, _)| *n > 0).unwrap_or(2);
    parts[first..(first + 2).min(3)].iter().filter(|(n, _)| *n > 0).map(|(n, w)| unit(*n, w)).collect::<Vec<_>>().join(" ")
}

/// `2 cents`, `less than a cent`, `$1.23`.
pub fn spoken_cost(micros: u64) -> String {
    match (micros as f64 / 10_000.).round() as u64 {
        _ if micros < 5_000 => "less than a cent".into(),
        1 => "1 cent".into(),
        c if c < 100 => format!("{c} cents"),
        _ => format_cost(Some(micros)),
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
    // A turn that ended the instant it began, as a session's last one often does, would focus a 1ns window.
    let end = state.session.ended_at_utc_ns;
    state.spans.values().filter(|s| s.kind == SpanKind::Turn && s.effective_end(end, Ns::MAX) > s.started_at_utc_ns).max_by(|a, b| (a.started_at_utc_ns, &a.span_id).cmp(&(b.started_at_utc_ns, &b.span_id)))
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

/// The popover's stats cells, in reading order. Cost and tokens cover the span's subtree (`rollup`).
/// Tokens and Spans drop out when there is nothing to count, as for a tool call, so the grid holds only facts.
/// `end` is where open spans stop, from `shown_end`.
pub fn popover_stats(state: &TraceState, s: &Span, rollup: &Totals, end: Option<Ns>, now: Ns) -> Vec<(&'static str, String)> {
    let origin = state.axis_start().unwrap_or(s.started_at_utc_ns);
    let mut out = vec![
        ("Duration", format_duration(s.elapsed(end, now).ns)),
        ("Started", format!("+{}", format_duration(s.started_at_utc_ns - origin))),
        ("Cost", trace_view::format_total_cost(rollup)),
    ];
    if rollup.billable_records > 0 && rollup.tokens_input + rollup.tokens_output > 0 {
        out.push(("Tokens in", format_tokens(rollup.tokens_input)));
        out.push(("Tokens out", format_tokens(rollup.tokens_output)));
    }
    let spans = descendants(state, &s.span_id, usize::MAX).len();
    if spans > 0 {
        out.push(("Spans", spans.to_string()));
    }
    out
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

/// How the sidebar lists sessions: by recency (Live, Today, Earlier) or by where they ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarMode {
    #[default]
    Recent,
    Workspaces,
}

impl SidebarMode {
    pub fn label(self) -> &'static str {
        match self {
            SidebarMode::Recent => "Recent",
            SidebarMode::Workspaces => "Workspaces",
        }
    }
}

/// The sidebar's tree. Recent mode yields only `Section`s; Workspaces mode only `Workspace`s.
#[derive(Debug, PartialEq)]
pub enum SidebarNode<'a> {
    Section { group: Group, sessions: Vec<&'a SessionSummary> },
    Workspace(Workspace<'a>),
}

/// The parent directory of a set of projects: `~/code/tools` holds `trace-waterfall` and `tw-r5`.
/// `path` is empty for sessions that never reported a working directory.
#[derive(Debug, PartialEq)]
pub struct Workspace<'a> {
    pub path: String,
    pub name: String,
    pub projects: Vec<Project<'a>>,
}

/// One working directory and the sessions that ran in it, live first, then latest first.
#[derive(Debug, PartialEq)]
pub struct Project<'a> {
    pub path: String,
    pub name: String,
    pub sessions: Vec<&'a SessionSummary>,
    /// How many of `sessions` are live as of the grouping's `now`; idle ones are not.
    pub live: usize,
}

pub const NO_WORKSPACE: &str = "No workspace";

impl Workspace<'_> {
    pub fn sessions(&self) -> usize {
        self.projects.iter().map(|p| p.sessions.len()).sum()
    }

    pub fn live(&self) -> usize {
        self.projects.iter().map(|p| p.live).sum::<usize>()
    }

    /// The state key under which the sidebar remembers this workspace collapsed.
    pub fn key(&self) -> String {
        format!("w:{}", self.path)
    }

    /// `Workspace tools, 6 sessions, 1 live`, for VoiceOver.
    pub fn spoken(&self) -> String {
        spoken_counts(&format!("Workspace {}", self.name), self.sessions(), self.live())
    }
}

impl Project<'_> {
    pub fn cost(&self) -> Option<u64> {
        self.sessions.iter().filter_map(|s| s.totals.cost_usd_micros()).reduce(|a, b| a + b)
    }

    pub fn key(&self) -> String {
        format!("p:{}", self.path)
    }

    pub fn spoken(&self) -> String {
        let base = spoken_counts(&format!("Project {}", self.name), self.sessions.len(), self.live);
        match self.cost() {
            Some(c) => format!("{base}, {}", spoken_cost(c)),
            None => base,
        }
    }
}

fn spoken_counts(head: &str, sessions: usize, live: usize) -> String {
    let n = if sessions == 1 { "1 session".to_owned() } else { format!("{sessions} sessions") };
    if live > 0 { format!("{head}, {n}, {live} live") } else { format!("{head}, {n}") }
}

/// When a session last did anything.
fn activity(s: &SessionSummary) -> Ns {
    s.last_event_at_utc_ns.or(s.ended_at_utc_ns).or(s.started_at_utc_ns).unwrap_or(0)
}

/// Live work first, then the most recent: a sort key, smallest first.
fn rank(live: bool, at: Ns) -> (bool, std::cmp::Reverse<Ns>) {
    (!live, std::cmp::Reverse(at))
}

fn basename(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

fn parent(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) => "/",
        Some(i) => &p[..i],
        None => "",
    }
}

/// Groups sessions for the sidebar. Recent mode buckets them into Live, Today, and Earlier by `today_start`.
/// Workspaces mode nests them as workspace (the project directory's parent), project (the working
/// directory), session. At every level, anything live as of `now` floats to the top, then the most
/// recent activity; sessions without a working directory gather last under `No workspace`.
pub fn group_sessions<'a>(sessions: &'a [SessionSummary], mode: SidebarMode, now: Ns, today_start: Ns) -> Vec<SidebarNode<'a>> {
    let live = |s: &SessionSummary| session_status(s.status, s.last_event_at_utc_ns, now) == Status::Live;
    let mut sorted: Vec<&SessionSummary> = sessions.iter().collect();
    sorted.sort_by_key(|s| rank(live(s), activity(s)));
    if mode == SidebarMode::Recent {
        let mut out: Vec<(Group, Vec<&SessionSummary>)> = Vec::new();
        for s in sessions {
            let g = group(s.status == SessionStatus::Live, s.started_at_utc_ns.or(s.last_event_at_utc_ns), today_start);
            match out.iter_mut().find(|(k, _)| *k == g) {
                Some((_, v)) => v.push(s),
                None => out.push((g, vec![s])),
            }
        }
        out.sort_by_key(|(g, _)| *g);
        return out.into_iter().map(|(group, sessions)| SidebarNode::Section { group, sessions }).collect();
    }
    let mut spaces: Vec<Workspace> = Vec::new();
    for s in sorted {
        let dir = s.working_directory.trim();
        let (ws_path, project_path) = if dir.is_empty() { (String::new(), format!("#{}", s.project_id)) } else { (parent(dir).to_owned(), dir.to_owned()) };
        let ws = match spaces.iter().position(|w| w.path == ws_path) {
            Some(i) => &mut spaces[i],
            None => {
                let name = if ws_path.is_empty() { NO_WORKSPACE.to_owned() } else { Some(basename(&ws_path)).filter(|n| !n.is_empty()).unwrap_or("/").to_owned() };
                spaces.push(Workspace { path: ws_path, name, projects: Vec::new() });
                spaces.last_mut().expect("just pushed")
            }
        };
        match ws.projects.iter_mut().find(|p| p.path == project_path) {
            Some(p) => {
                p.live += usize::from(live(s));
                p.sessions.push(s);
            }
            None => {
                let name = Some(basename(dir)).filter(|n| !n.is_empty()).map(str::to_owned).or_else(|| Some(s.project_id.clone()).filter(|p| !p.is_empty())).unwrap_or_else(|| "Untitled".to_owned());
                ws.projects.push(Project { path: project_path, name, live: usize::from(live(s)), sessions: vec![s] });
            }
        }
    }
    // Sessions arrived ranked, so each project's first session and each workspace's first project carry
    // its rank, and a stable sort by that keeps the order the sessions gave.
    let head = |p: &Project| rank(live(p.sessions[0]), activity(p.sessions[0]));
    for w in &mut spaces {
        w.projects.sort_by_key(|p| head(p));
    }
    spaces.sort_by_key(|w| (w.path.is_empty(), head(&w.projects[0])));
    spaces.into_iter().map(SidebarNode::Workspace).collect()
}

/// `/Users/d/code/tools` → `~/code/tools` when `home` is `/Users/d`.
pub fn tilde(path: &str, home: &str) -> String {
    match path.strip_prefix(home) {
        Some(rest) if !home.is_empty() && (rest.is_empty() || rest.starts_with('/')) => format!("~{rest}"),
        _ => path.to_owned(),
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
    fn brands_follow_harness_vendor_and_service() {
        let logo = |b: Option<Brand>| b.map(|b| b.logo);
        assert_eq!(logo(harness_brand("claude-code", "")), Some(Logo::ClaudeCode));
        assert_eq!(logo(harness_brand("Gemini-CLI", "")), Some(Logo::GeminiCli));
        assert_eq!(logo(harness_brand("simulated", "roo")), Some(Logo::RooCode), "falls back to the source adapter");
        assert_eq!(logo(harness_brand("copilot", "")), Some(Logo::Copilot));
        assert_eq!(logo(harness_brand("simulated", "simulated-live")), None);
        assert_eq!(harness_label("claude-code", "").as_deref(), Some("Claude Code"));
        assert_eq!(harness_label("simulated", "roo").as_deref(), Some("Roo Code"));
        assert_eq!(harness_label("simulated", "simulated-live").as_deref(), Some("Simulated"));
        assert_eq!(harness_label("", ""), None);

        let vendor = |m: &str| logo(vendor_brand(Some(m), None));
        assert_eq!(vendor("claude-opus-5-5"), Some(Logo::Claude));
        assert_eq!(vendor("gpt-5"), Some(Logo::OpenAi));
        assert_eq!(vendor("o3-mini"), Some(Logo::OpenAi));
        assert_eq!(vendor("openai/gpt-5-codex"), Some(Logo::OpenAi));
        assert_eq!(vendor("gemini-3-pro"), Some(Logo::Gemini));
        assert_eq!(vendor("codestral-latest"), Some(Logo::Mistral));
        assert_eq!(vendor("llama-4-scout"), Some(Logo::Meta));
        assert_eq!(vendor("DeepSeek-V3"), Some(Logo::DeepSeek));
        assert_eq!(vendor("kimi-k2"), Some(Logo::Kimi));
        assert_eq!(vendor("sonar-pro"), Some(Logo::Perplexity));
        assert_eq!(vendor("sim-model-1"), None);
        assert_eq!(logo(vendor_brand(Some("mystery"), Some("Anthropic"))), Some(Logo::Claude));
        assert_eq!(logo(vendor_brand(Some("phi4"), Some("ollama"))), Some(Logo::Ollama));

        let tool = |t: &str| tool_brand(t).map(|b| (b.logo, b.tint));
        let hex = |h: u32| Some(Hsla::from(rgb(h)));
        assert_eq!(tool("mcp__github__create_branch"), Some((Logo::GitHub, hex(0x181717))));
        assert_eq!(tool("mcp__claude_ai_Linear__list_issues"), Some((Logo::Linear, hex(0x5E6AD2))));
        assert_eq!(tool("mcp__google_drive__search"), Some((Logo::GoogleDrive, hex(0x4285F4))));
        assert_eq!(tool("mcp__postgres__query"), Some((Logo::Postgres, hex(0x4169E1))));
        assert_eq!(tool("github_create_pr"), Some((Logo::GitHub, hex(0x181717))));
        assert_eq!(tool("gh"), Some((Logo::GitHub, hex(0x181717))));
        assert_eq!(tool("git"), Some((Logo::Git, hex(0xF03C2E))));
        assert_eq!(tool("notion-search"), Some((Logo::Notion, hex(0x000000))));
        assert_eq!(tool("Bash"), None);
        assert_eq!(tool("mcp__context7__resolve"), None);
        assert_eq!(tool("digit"), None, "a service name must be a whole word");
        assert_eq!(tool("mcp__sentry__get_issue"), Some((Logo::Sentry, hex(0x362D59))));
        assert_eq!(tool("mcp__supabase__execute_sql"), Some((Logo::Supabase, hex(0x3FCF8E))));
        assert_eq!(tool("mcp__atlassian__get_issue"), Some((Logo::Jira, hex(0x0052CC))));
        assert_eq!(tool("mcp__google_docs__read"), Some((Logo::GoogleDocs, hex(0x4285F4))));
        assert_eq!(tool("mcp__context7_docs__resolve"), None, "generic words count only as a whole server name");
        assert_eq!(tool("mcp__playwright__browser_click"), None, "Simple Icons has no Playwright mark");
    }

    #[test]
    fn brand_for_picks_by_kind_and_keeps_lucide_otherwise() {
        let s = state();
        let cc = harness_brand(&s.session.harness, &s.session.source_adapter);
        let span = |id: &str| &s.spans[&id.into()];
        assert_eq!(brand_for(span("root"), cc).map(|b| b.logo), Some(Logo::ClaudeCode));
        assert_eq!(brand_for(span("llm"), cc).map(|b| b.logo), Some(Logo::Claude));
        assert_eq!(brand_for(span("sub"), cc).map(|b| b.logo), Some(Logo::Claude), "a subagent shows its model's vendor");
        assert_eq!(brand_for(span("Bash"), cc), None);
        let mut plain_sub = span("sub").clone();
        plain_sub.model = None;
        assert_eq!(brand_for(&plain_sub, cc), None, "a subagent without a known model keeps its glyph");
    }

    #[test]
    fn every_logo_embeds_an_svg() {
        for l in Logo::ALL {
            assert!(l.svg().starts_with(b"<svg"), "{l:?}");
            assert_eq!(Logo::by_file(l.file()), Some(*l));
        }
    }

    #[test]
    fn prettifies_claude_model_names() {
        assert_eq!(pretty_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(pretty_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(pretty_model("claude-3-5-sonnet-20241022"), "Sonnet 3.5");
        assert_eq!(pretty_model("claude-haiku-4-5[1m]"), "Haiku 4.5");
        assert_eq!(pretty_model("claude-opus-4-1-20250805"), "Opus 4.1");
        assert_eq!(pretty_model("gpt-4o"), "GPT-4o");
        assert_eq!(pretty_model("openai/gpt-5"), "GPT-5");
        assert_eq!(pretty_model("gemini-3-pro"), "Gemini 3 Pro");
        assert_eq!(pretty_model("mistral-large"), "mistral-large");
        assert_eq!(pretty_model("claude-instant"), "claude-instant");
    }

    #[test]
    fn classifies_tools_into_categories() {
        use ToolCategory::*;
        let cases = [
            ("Bash", Shell),
            ("local_shell", Shell),
            ("Read", Read),
            ("NotebookRead", Read),
            ("read_file", Read),
            ("Edit", Edit),
            ("MultiEdit", Edit),
            ("apply_patch", Edit),
            ("str_replace", Edit),
            ("create_file", Write),
            ("Write", Write),
            ("Grep", Search),
            ("Glob", Search),
            ("codebase_search", Search),
            ("ToolSearch", Search),
            ("WebFetch", Web),
            ("WebSearch", Web),
            ("curl", Web),
            ("mcp__playwright__browser_click", Browser),
            ("computer", Browser),
            ("Task", Agent),
            ("spawn_agent", Agent),
            ("TodoWrite", Plan),
            ("update_plan", Plan),
            ("Skill", Plan),
            ("mcp__engram__mem_save", Memory),
            ("remember", Memory),
            ("run_sql", Database),
            ("sqlite_query", Database),
            ("git_commit", Git),
            ("cargo_build", Build),
            ("make", Build),
            ("run_tests", Test),
            ("pytest", Test),
            ("AskUserQuestion", Ask),
            ("request_approval", Ask),
            ("mcp__acme__thing", Mcp),
            ("mcp__slack__post_message", Mcp),
            ("frobnicate", Other),
        ];
        for (tool, cat) in cases {
            assert_eq!(tool_category(tool), cat, "{tool}");
        }
        assert_eq!(Shell.icon(), IconName::SquareTerminal);
        assert_eq!(Other.icon(), IconName::Wrench);
    }

    #[test]
    fn glyphs_and_details_follow_kind() {
        let s = state();
        let sub = &s.spans[&"sub".into()];
        let llm = &s.spans[&"llm".into()];
        let bash = &s.spans[&"Bash".into()];
        assert_eq!(glyph(llm), (IconName::Sparkles, Ink::Kind(Tone::Llm)));
        assert_eq!(glyph(sub), (IconName::Users, Ink::Kind(Tone::Agent)));
        assert_eq!(glyph(bash), (IconName::SquareTerminal, Ink::Kind(Tone::Tool)));
        assert_eq!(tone(bash), Tone::Error, "a failed span paints with the error palette");
        assert_eq!(detail(llm, &s).as_deref(), Some("Sonnet 5.5"));
        assert_eq!(title(llm, &s.session), "LLM call", "a span named for its kind reads as the kind");
        let mut named_after_model = llm.clone();
        named_after_model.name = "claude-sonnet-5-5".into();
        assert_eq!(title(&named_after_model, &s.session), "LLM call");
        assert_eq!(detail(bash, &s), None, "a tool named after itself gets no detail");
        let root = &s.spans[&"root".into()];
        assert_eq!((title(root, &s.session), detail(root, &s)), ("Claude Code".to_owned(), Some("Sonnet 5.5".to_owned())), "the agent reads as its harness and model");
        let mut unknown = s.session.clone();
        unknown.harness = "simulated".into();
        let mut named = root.clone();
        named.agent_name = Some("main".into());
        assert_eq!(title(&named, &unknown), "Main", "an unknown harness falls back to the agent's name");
        let mut turn = root.clone();
        (turn.kind, turn.name) = (SpanKind::Turn, "turn 1".into());
        assert_eq!(title(&turn, &s.session), "Turn 1");
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
        let stats = popover_stats(&s, sub, &s.subtree_totals(&"sub".into()), None, 40_000 * MS).into_iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>();
        assert_eq!(stats, ["Duration 30s", "Started +8.1s", "Cost $0.05", "Tokens in 41.2k", "Tokens out 1.4k", "Spans 2"]);
        let llm = popover_stats(&s, &s.spans[&"llm".into()], &s.subtree_totals(&"llm".into()), None, 40_000 * MS).into_iter().map(|(k, _)| k).collect::<Vec<_>>();
        assert_eq!(llm, ["Duration", "Started", "Cost", "Tokens in", "Tokens out"], "a leaf has no Spans cell");
        let bash = popover_stats(&s, &s.spans[&"Bash".into()], &s.subtree_totals(&"Bash".into()), None, 40_000 * MS).into_iter().map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>();
        assert_eq!(bash, ["Duration 1s", "Started +12s", "Cost —"], "an unbilled tool call keeps only what it has");
    }

    #[test]
    fn idle_stretches_squeeze_and_skip() {
        const H: Ns = 3_600_000 * MS;
        let mut events = vec![ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})), start(2, 0, "root", None, json!({"kind": "agent"}))];
        // Two bursts of work ten hours apart; the agent span stays open across the night.
        for (i, t) in [0, 10 * H].into_iter().enumerate() {
            let id = format!("tool{i}");
            events.push(start(10 + 2 * i as u64, t, &id, Some("root"), json!({"kind": "tool"})));
            events.push(ev(11 + 2 * i as u64, t + H / 2, Some(&id), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})));
        }
        let s = TraceState::from_events("s1".into(), &events);
        let span = (0, 10 * H + H / 2);
        let gaps = idle_gaps(&s, span, Some(span.1), span.1);
        assert_eq!(gaps, [(H / 2, 10 * H)], "the open agent span does not hide the idle night");
        let sq = Squeeze::new(span, 400., &gaps);
        assert_eq!((sq.x(0), sq.x(span.1)), (0., 400.));
        assert!((sq.x(H / 2) - 186.).abs() < 0.01, "each half hour of work gets half the room the band leaves: {}", sq.x(H / 2));
        assert_eq!(sq.band_at(sq.x(H / 2) + 5.), Some(gaps[0]));
        assert_eq!(sq.band_at(10.), None);
        for t in [H / 4, 3 * H, 10 * H + H / 4] {
            assert!((sq.t(sq.x(t)) - t).abs() < 1_000 * MS, "t(x(t)) round-trips within f32 pixels");
        }
        assert_eq!(Squeeze::new(span, 400., &[]).x(span.1 / 2), 200., "no idle stretches: linear");
        assert_eq!((sq.x(-H), sq.x(50 * H)), (0., 400.), "times past the ends pin to the strip's edges");
        let len = H;
        assert_eq!(skip_idle(&gaps, 0, len, true), Some(10 * H - len / 20), "forward lands just before the next burst");
        assert_eq!(skip_idle(&gaps, 10 * H - len / 20, len, true), None, "nothing idle after the last burst");
        assert_eq!(skip_idle(&gaps, 10 * H - len / 20, len, false), Some(H / 2 + len / 20 - len), "back puts the previous burst's end near the right edge");
        assert_eq!((compact_duration(9 * H + 30 * 60_000 * MS), compact_duration(72 * H), compact_duration(35 * 60_000 * MS)), ("9h".into(), "3d".into(), "35m".into()));
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
        let t = 3_700_000 * MS;
        events.push(start(40, t, "empty", Some("root"), json!({"kind": "turn"})));
        events.push(ev(41, t, Some("empty"), json!({"event_type": "span.ended", "body": {"status": "succeeded"}})));
        let s = TraceState::from_events("s1".into(), &events);
        assert_eq!(focus_turn(&s, hour_over_1000px).map(|t| t.name.as_str()), Some("turn1"), "a zero-length last turn is skipped");
        events.pop();
        events.push(ev(41, t, None, json!({"event_type": "session.ended", "body": {"status": "interrupted"}})));
        let s = TraceState::from_events("s1".into(), &events);
        assert_eq!(focus_turn(&s, hour_over_1000px).map(|t| t.name.as_str()), Some("turn1"), "so is one left open when the session ended as it began");
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
        assert_eq!((title(&t, &s.session), detail(&t, &s)), ("create_issue".to_owned(), Some("github".to_owned())));
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
        let now = 30_000_000 * MS;
        assert_eq!(offscreen_hint(0, 2_700 * MS, w, None), Some((Edge::Left, "2.7s · 1h4m before".into())));
        assert_eq!(offscreen_hint(4_180_000 * MS, 4_190_000 * MS, w, None), Some((Edge::Right, "3m after".into())));
        assert_eq!(offscreen_hint(3_950_000 * MS, 4_190_000 * MS, w, None), None);
        assert_eq!(offscreen_hint(0, 2_700 * MS, w, Some(now)), Some((Edge::Left, "2.7s · 8h19m ago".into())), "following reads from real now");
        assert_eq!(offscreen_hint(4_180_000 * MS, 4_190_000 * MS, w, Some(now)), Some((Edge::Right, "3m after".into())));
        assert_eq!(offscreen_hint(2_700 * MS, 2_699 * MS, w, None), Some((Edge::Left, "0s · 1h4m before".into())), "an end logged before its start reads as an instant");
    }

    #[test]
    fn a_quiet_live_session_goes_idle_and_its_open_spans_stop_at_the_last_event() {
        let mut events = vec![
            ev(1, 0, None, json!({"event_type": "session.started", "body": {"project_id": "p"}})),
            start(2, 0, "root", None, json!({"kind": "agent"})),
            start(3, 5_000 * MS, "sub", Some("root"), json!({"kind": "subagent"})),
        ];
        let s = TraceState::from_events("s1".into(), &events);
        let last = 5_000 * MS;
        assert_eq!(idle_since(&s, true, last + IDLE_AFTER), None, "exactly at the threshold is still active");
        let later = last + 6 * 3_600_000 * MS;
        assert_eq!(idle_since(&s, true, later), Some(last));
        assert_eq!(idle_since(&s, false, later), None, "replay and static traces are never idle");
        let sub = &s.spans[&"sub".into()];
        assert_eq!(sub.effective_end(shown_end(&s, idle_since(&s, true, later)), later), last);
        assert_eq!(sub.effective_end(shown_end(&s, None), later), later, "an active session's open spans run to now");
        events.push(ev(4, 6_000 * MS, None, json!({"event_type": "session.ended", "body": {"status": "completed"}})));
        let ended = TraceState::from_events("s1".into(), &events);
        assert_eq!(idle_since(&ended, true, later), None, "an ended session is done, not idle");
    }

    #[test]
    fn now_capsule_reads_the_distance_past_the_view() {
        let h = 3_600_000 * MS;
        let m = 60_000 * MS;
        let w = (7 * h, 7 * h + 31 * m);
        assert_eq!(now_capsule(14 * h + 20 * m, w, false), Some((Edge::Right, "Now +6h49m".into())));
        assert_eq!(now_capsule(14 * h + 20 * m, w, true), Some((Edge::Right, "Now".into())), "idle: the gap is told once, by the idle marker");
        assert_eq!(now_capsule(7 * h + 10 * m, w, false), None);
        const SEC: Ns = 1_000_000_000;
        let s = Some(0);
        assert_eq!(session_duration(s, None, Some(14 * SEC), true, 60 * SEC), Some(60 * SEC), "live and active: to now, as the header says");
        assert_eq!(session_duration(s, None, Some(14 * SEC), true, 14 * SEC + IDLE_AFTER + 1), Some(14 * SEC), "idle: to the last event");
        assert_eq!(session_duration(s, Some(30 * SEC), Some(31 * SEC), false, 99 * SEC), Some(30 * SEC), "ended: to the end");
        assert_eq!(session_duration(None, None, Some(1), true, 9), None);
        assert_eq!(now_capsule(6 * h, w, false), Some((Edge::Left, "Now \u{2212}1h".into())));
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
        let whole = (0, 2 * h + 4 * m + 30_000 * MS);
        assert_eq!(overview_label((2 * h + 4 * m, 2 * h + 4 * m + 4_400 * MS), whole), "4.4s window at 2h4m · 2h4m total", "identical ends name length and place");
    }

    #[test]
    fn density_scales_each_lane_by_its_busiest_column() {
        // Lane 1: one span over columns 0..4, three more stacked on column 1.
        let d = density([(0., 4., 1), (1., 2., 1), (1., 2., 1), (1., 2., 1), (2.5, 3., 2)], 5);
        let lane = |k: usize| d.iter().map(|c| (c[k] * 100.).round() / 100.).collect::<Vec<_>>();
        assert_eq!(lane(1), [0.5, 1., 0.5, 0.5, 0.]);
        assert_eq!(lane(2), [0., 0., 1., 0., 0.]);
        let sparse = density((0..1).map(|_| (0., 0., 0)).chain((0..10).map(|_| (1., 2., 0))), 2);
        assert_eq!(sparse.iter().map(|c| c[0]).collect::<Vec<_>>(), [0.18, 1.], "a lone instant stays visible");
        let burst = density((0..200).map(|i| (i as f32 / 100., i as f32 / 100., 0)), 2);
        assert!(burst.iter().all(|c| c[0] > 0.95), "a burst of instants reads as busy: {burst:?}");
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
    fn one_status_map_gives_every_status_its_own_shape() {
        let all = [Status::Live, Status::Idle, Status::Paused, Status::Done, Status::Failed, Status::Interrupted];
        assert_eq!(all.map(|s| s.look().1), [Shape::Dot, Shape::Ring, Shape::Pause, Shape::Check, Shape::Cross, Shape::Pause]);
        assert_eq!(Status::Idle.look().0, Hue::Amber, "idle is amber everywhere");
        let h = 3_600_000 * MS;
        assert_eq!(session_status(SessionStatus::Live, Some(0), 7 * h), Status::Idle);
        assert_eq!(session_status(SessionStatus::Live, Some(0), 60_000 * MS), Status::Live);
        assert_eq!(session_status(SessionStatus::Completed, Some(0), 7 * h), Status::Done);
        assert_eq!(span_status(SpanStatus::Running, true, true), (Status::Idle, "Idle"));
        assert!(!shows_badge(Status::Done, Group::Earlier));
        assert!(shows_badge(Status::Done, Group::Today));
        assert!(shows_badge(Status::Failed, Group::Earlier));
        assert_eq!(idle_gap(0, 7 * h + 2 * 60_000 * MS), "+7h2m idle");
    }

    #[test]
    fn rows_read_aloud_as_kind_model_time_cost_and_outcome() {
        let s = state();
        let label = |id: &str| row_label(&s.spans[&id.into()], &s, &s.subtree_totals(&id.into()), None, 40_000 * MS);
        let mut llm = s.spans[&"llm".into()].clone();
        llm.name = "llm call".into();
        assert_eq!(row_label(&llm, &s, &s.subtree_totals(&"llm".into()), None, 40_000 * MS), "LLM call, Sonnet 5.5, 3 seconds, 5 cents, succeeded");
        assert_eq!(label("Bash"), "Bash, 1 second, failed");
        assert_eq!(spoken_duration(1_600 * MS), "1.6 seconds");
        assert_eq!(spoken_duration(250 * MS), "250 milliseconds");
        assert_eq!(spoken_duration(150_000 * MS), "2 minutes 30 seconds");
        assert_eq!(spoken_duration(3_900_000 * MS), "1 hour 5 minutes");
        assert_eq!([1_200, 15_000, 20_000, 1_234_567].map(spoken_cost), ["less than a cent", "2 cents", "2 cents", "$1.23"]);
    }

    #[test]
    fn the_last_tick_label_keeps_off_the_card_edge() {
        assert_eq!(tick_label_x(100., 40., 1000.), 105.);
        assert_eq!(tick_label_x(980., 40., 1000.), 952.);
    }

    #[test]
    fn llm_spans_with_generic_names_read_as_llm_call() {
        let s = state();
        let mut llm = s.spans[&"llm".into()].clone();
        for name in ["llm call", "LLM_call", "llm"] {
            llm.name = name.into();
            assert_eq!(title(&llm, &s.session), "LLM call", "{name}");
        }
    }

    #[test]
    fn groups_sessions_by_liveness_then_day() {
        assert_eq!(group(true, Some(0), 100), Group::Live);
        assert_eq!(group(false, Some(150), 100), Group::Today);
        assert_eq!(group(false, Some(50), 100), Group::Earlier);
        assert_eq!(group(false, None, 100), Group::Earlier);
    }

    fn summary(id: &str, dir: &str, status: SessionStatus, last: Ns) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            project_id: format!("{id}-project"),
            working_directory: dir.into(),
            source_adapter: String::new(),
            harness: String::new(),
            host: String::new(),
            status,
            started_at_utc_ns: Some(last - 10),
            ended_at_utc_ns: None,
            last_event_at_utc_ns: Some(last),
            model: None,
            totals: Totals::default(),
            span_count: 0,
            turn_count: 0,
        }
    }

    #[test]
    fn workspaces_nest_projects_and_float_live_work_to_the_top() {
        let h = 3_600_000 * MS;
        let now = 100 * h;
        let mut sessions = vec![
            summary("a", "/u/code/tools/trace-waterfall", SessionStatus::Completed, now - 9 * h),
            summary("b", "/u/code/tools/tw-r5/", SessionStatus::Live, now - MS),
            summary("c", "/u/code/tools/trace-waterfall", SessionStatus::Completed, now - 7 * h),
            summary("d", "/u/work/api", SessionStatus::Completed, now - 5 * h),
            summary("e", "", SessionStatus::Failed, now - h),
            summary("f", "/u/work/api", SessionStatus::Live, now - 20 * h),
        ];
        sessions[0].totals.cost_actual_usd_micros = 20_000;
        sessions[0].totals.billable_records = 1;
        sessions[2].totals.cost_actual_usd_micros = 5_000;
        sessions[2].totals.billable_records = 1;
        let tree = group_sessions(&sessions, SidebarMode::Workspaces, now, 0);
        type Shape = Vec<(String, Vec<(String, Vec<String>)>)>;
        let shape: Shape = tree
            .iter()
            .map(|n| match n {
                SidebarNode::Workspace(w) => (w.name.clone(), w.projects.iter().map(|p| (p.name.clone(), p.sessions.iter().map(|s| s.session_id.0.clone()).collect())).collect()),
                SidebarNode::Section { .. } => panic!("workspaces mode has no sections"),
            })
            .collect();
        let own = |w: &str, ps: &[(&str, &[&str])]| (w.to_owned(), ps.iter().map(|(p, ss)| (p.to_string(), ss.iter().map(|s| s.to_string()).collect())).collect::<Vec<_>>());
        assert_eq!(
            shape,
            [
                own("tools", &[("tw-r5", &["b"]), ("trace-waterfall", &["c", "a"])]),
                own("work", &[("api", &["d", "f"])]),
                own(NO_WORKSPACE, &[("e-project", &["e"])]),
            ],
            "live first, then latest; an idle live session sorts by its last activity; no workspace goes last"
        );
        let SidebarNode::Workspace(tools) = &tree[0] else { unreachable!() };
        assert_eq!(tools.path, "/u/code/tools");
        assert_eq!(tools.spoken(), "Workspace tools, 3 sessions, 1 live");
        assert_eq!(tools.projects[1].spoken(), "Project trace-waterfall, 2 sessions, 3 cents");
        assert_eq!(tools.projects[1].cost(), Some(25_000));
        assert_eq!(tools.projects[0].cost(), None);
        let SidebarNode::Workspace(work) = &tree[1] else { unreachable!() };
        assert_eq!((work.live(), work.spoken()), (0, "Workspace work, 2 sessions".to_owned()), "idle is not live");
        assert_eq!((tools.key(), tools.projects[0].key()), ("w:/u/code/tools".to_owned(), "p:/u/code/tools/tw-r5/".to_owned()));
    }

    #[test]
    fn recent_mode_buckets_by_liveness_then_day() {
        let now = 1_000;
        let sessions = [summary("a", "/x", SessionStatus::Completed, 300), summary("b", "/x", SessionStatus::Live, 900), summary("c", "/x", SessionStatus::Completed, 600)];
        let tree = group_sessions(&sessions, SidebarMode::Recent, now, 500);
        let shape: Vec<(Group, Vec<&str>)> = tree
            .iter()
            .map(|n| match n {
                SidebarNode::Section { group, sessions } => (*group, sessions.iter().map(|s| s.session_id.0.as_str()).collect()),
                SidebarNode::Workspace(_) => panic!("recent mode has no workspaces"),
            })
            .collect();
        assert_eq!(shape, [(Group::Live, vec!["b"]), (Group::Today, vec!["c"]), (Group::Earlier, vec!["a"])]);
    }

    #[test]
    fn home_abbreviates_to_a_tilde() {
        assert_eq!(tilde("/Users/d/code/tools", "/Users/d"), "~/code/tools");
        assert_eq!(tilde("/Users/d", "/Users/d"), "~");
        assert_eq!(tilde("/Users/dan/x", "/Users/d"), "/Users/dan/x");
        assert_eq!(tilde("/opt/x", ""), "/opt/x");
    }
}
