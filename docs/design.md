# Desktop app design

`agents-trace-waterfall` is the macOS desktop app, built with GPUI through gpui-kit. It answers the same questions as the TUI: what happened, in what order, why it was slow, what it cost, and what failed. It shares the session document (`trace-view`) with the TUI, so live, pause, replay, folding, and search behave identically.

The visual reference is the affectUI Trace Waterfall. The app should feel like a native Mac utility: quiet chrome, dense but calm data, motion that explains state changes.

## Window

- Default size 1440×900. Minimum 960×600.
- Transparent title bar. The content runs under it, and the traffic lights sit inside the sidebar.
- The window background is the **tray** color. Content sits on **card** surfaces with a 1px **line** ring and a soft shadow.
- Follows the system appearance: light or dark. `--appearance light|dark` overrides it for screenshots.

## Layout

```
┌──────────────┬───────────────────────────────────────────────────────────┐
│ ● ● ●        │ ✦ Trace  project · harness                    ● Live  ⏵ ⊕ │
│ ⌕ Search     │ ┌───────────────────────────────────────────────────────┐ │
│              │ │ Duration │ Cost │ Tokens │ LLM calls │ Tool calls │ Err│ │
│ LIVE         │ ├──────────────┬────────────────────────────────────────┤ │
│ ● project  … │ │ Span         │ 0s      15s      30s   ┊   45s         │ │
│ TODAY        │ │ ⌄ ◆ Agent    │ ▕▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▏ 43s · $0.42  │ │
│ ✓ project  … │ │   ✳ LLM call │  ▕▔▔▏ 6.9s · $0.12                     │ │
│ EARLIER      │ │   …          │                                        │ │
│              │ └──────────────┴────────────────────────────────────────┘ │
└──────────────┴───────────────────────────────────────────────────────────┘
```

**Sidebar** (260px, tray background). It has a search field, then sessions grouped into **Live**, **Today**, and **Earlier**. Each row shows a status dot (pulsing while live), the project name, a line with harness, model, and turn count, and a muted line with start time, duration, and cost. Prompts are never stored, so rows tell sessions apart by how they ran. The selected row gets a card-colored pill.

A compact **Recent | Workspaces** segmented control sits under the search field. Workspaces mode is a three-level source list. A session's project is its working directory, labelled with the directory's name (else its project id). Its workspace is that directory's parent, labelled with its name, with the `~`-abbreviated path as a tooltip. Sessions without a working directory gather under **No workspace**, last. At every level live work floats to the top, then the most recent activity (`present::group_sessions`).

- **Workspace header.** Chevron, folder icon, name (12.5px semibold), a live dot when any session in it is live, and its session count.
- **Project row.** Indented one level: chevron, name, live dot, total cost, and a count capsule.
- **Session rows.** Indented under their project. The project name is already above, so the title reads `model · turns`, with the harness named only when it has no logo.

Clicking a header, or Enter, Space, Left, and Right on a focused one, folds it; its chevron turns over 240ms. A search shows every match, so it opens whatever group holds one. The mode and the folded groups are saved to `<store>.sidebar.json` beside the store. Right-clicking a header offers Collapse All Others, Reveal in Finder, and Copy Path. VoiceOver reads a header as `Workspace tools, 6 sessions, 1 live`.

**Header.** It shows "✦ Trace" in strong ink, then the session subtitle in muted ink. On the right are a status pill (Live, Idle, Done, Failed, Interrupted, or `Replay · 0:06`), a Follow toggle when live, and a Replay button. Follow reads as pressed only while now is in the visible window. Every header control is 28px tall with 8px gaps. The status pill has a card fill, a 1px **line** ring, and a soft shadow; the status color appears only on its dot and text.

**Trace card.** It holds the stats strip, the time axis, and the rows.

- **Stats strip.** Six cells separated by 1px lines: Duration, Cost, Tokens, LLM calls, Tool calls, Errors. Each cell has a 12px muted label above a 16px semibold tabular value. Unknown cost reads `—`. Errors render in error ink when greater than zero.
- **Overview.** The whole session as one thin lane each for wait, LLM, and tool spans. Each pixel column sums how much of it spans cover, and its alpha is the square root of that over the lane's busiest column, so dense traces read as density, not dashes. Failures draw as a red rug of 1px ticks above the lanes. The visible window is a lens at least 24px wide, with 6px drag handles; a lens narrower than that gets a 2px accent ring. The caption reads `All · 16.2s`, or `7h18m – 7h31m of 8h12m` with a **Show all** button when zoomed. When both ends would read the same it names the window's length and place instead: `4.4s at 2h4m of 2h4m`.
- **Columns.** A "Span" header over a 300px name column, then the shared timeline. The timeline shows tick lines in **grid** color, with 11px muted tick labels at their tops.
- **Rows.** Each row is 32px tall. It holds tree guide lines (1px **guide** at each 14px indent), a chevron (rotated -90° when folded), a 20px icon tile (card fill, line ring, 6px radius), the title (13px ink, 500 weight on parents), and a muted detail (model or tool). A "+N" chip (18px capsule, 11px muted tabular) follows the title of a folded parent and counts hidden descendants. An MCP tool `mcp__server__tool` reads as `tool` with `server` as its detail. Long titles truncate in the middle and show the full name in a tooltip. A spinner marks running spans and a warning glyph marks failed ones.
- **Bars.** 16px tall, 5px radius, filled with the kind's bg and stroked with its edge. A label after the bar reads `duration · cost` in 11.5px text. When there is no room after the bar, the label goes inside it, right-aligned, in the kind's ink. Outside the bar the label sits on a 2px knockout in the row color, so grid and now lines pass beneath it. A bar cut by a timeline edge squares off there and fades in over 12px. The slowest completed LLM or tool span gets a "Slowest" tag (slow bg and fg). A folded parent's bar shows its children as 6px marks in their kind's mark color.
- **Off-screen spans.** A span wholly outside the visible window shows a chevron in its mark color at that edge with an 11px muted hint. While live it reads `2.7s · 1h5m ago` or `3m later`; a finished trace reads `2.7s · 1h5m before` or `3m after`. Clicking it pans to the span. Its title dims to **text**.
- **Big traces.** Above 500 spans, a trace opens with every finished turn folded except the latest.
- **Running bars.** They extend to now, a soft highlight sweeps across them every 1.5s, and their right edge glows.
- **Idle sessions.** A live session with no event for 10 minutes is idle. The status pill reads `Idle · 6h49m` (time since the last event) in slow ink, spinners stop, and open spans end at the last event with a three-dash tail that fades out. Duration runs from the first event to the last; its tooltip gives the length until now. An idle session draws no animation frames and polls every 2s.
- **Now line.** A 1px **now** color line with a 6px accent dot where it meets the axis. Any manual pan, zoom, or lens drag stops following. Opening a long session on its latest turn leaves now off-screen, so it opens with Follow off. While live and now is off-screen, a `Now +6h49m ›` capsule (the distance from the view's right edge to now) is pinned at the timeline's top right; clicking it or pressing `f` brings now in at the current scale and turns Follow on.
- **Hover and selection.** Hovering a row gives it the **hover** wash and gives its bar an inner ring in the mark color. The selected row gets the **selected** wash.

**Context panel.** The header's Context toggle (`c`, ⌘K) opens a 300px card to the right of the trace card. Its head reads `Context` and the model over the fill, `425k / 1M · 43%`, in the stats strip's type. Below, a squarified treemap of the main agent's context at the selected span (the clock when the root is selected): tiles with a 6px radius and 3px gaps, labelled when at least 44×24px, tokens under the label when 42px tall, a tooltip on each. Colors follow hue-means-work: the baseline (system, tools, memory) agent gray, responses llm, tool results tool, prompts brand, a compaction summary ok, other chip. Free space up to the model's window is a dashed **faint** outline. The footer reads `Compacted 1× · showing call at 90:38:07` or `Not compacted · …`.

**Detail popover.** It opens with Enter or a click. It sits 6px below the selected bar, aligned to the bar's start and kept 12px inside the card, with an 8px arrow pointing at the bar. It flips above only when it would overflow below and there is more room above. It never extends past the card bottom less 12px; its content scrolls instead, with a 24px fade at the bottom while more lies below. When the natural height fits neither side, it takes the side with more room. It is 330px wide, with a 16px radius and the **pop** shadow.

- **Head.** A 28px icon tile, then the name (13.5px semibold) over "Kind · model", then a status pill and a close button.
- **Stats.** A wash panel with Duration, Started (+offset), Cost, Tokens in, Tokens out, and Spans (descendants).
- **Error.** An error box shows when the span failed.
- **Input and Output.** Each reads `Not captured` in muted ink when absent.
- **More.** Events and Files sections follow when present.

**Replay transport.** In replay, a floating capsule sits centered at the bottom with play/pause, step back, step forward, a speed segmented control (0.25× to 10×), a scrubber, and the time readout.

**Empty state.** A centered card explains `trace-tui collector start`, with a copy button inside the command chip (⌘C also copies; it shows "Copied" for 1.2s), and offers **Import JSONL…**. A faint sample waterfall sits behind it, fading out radially around the copy. Dropping a `.jsonl` file onto the window also imports it.

## Icon system

Every span row and the popover head show one mark in a tile. `present::brand_for` and `present::tool_category` pick it, in this order.

1. **Brand.** The agent row shows the session's harness logo, matched on its harness or source adapter with case, hyphens, and underscores ignored (Claude Code, Codex, Cursor, Gemini CLI, Copilot, Windsurf, Cline, Roo Code, Kilo Code, OpenCode, Amp, Goose, Trae, Kiro, OpenHands, Junie, Devin). An LLM row shows its model vendor by model prefix (Claude, OpenAI, Gemini, Mistral, Meta, DeepSeek, Qwen, Grok, Kimi, Perplexity) or the Ollama provider. A subagent shows its model's vendor when it has one. A tool row shows the service it talks to: the MCP server of `mcp__<server>__tool`, whole or any word of it, or the first word of the tool name (`github_*`, `gh`, `git`, `notion-*`). Generic words (`gh`, `git`, `docs`, `sheets`, `npm`, `python`) count only as a whole name.
2. **Category.** Any other tool draws its category's lucide glyph in the tool ink. The first row whose words appear in the tool name wins. An MCP tool classifies by its tool part, then its server, then reads as MCP.
3. **Kind.** Everything else keeps its kind glyph: turn, subagent without a known model, wait, custom.

| Category | Glyph | Words |
| --- | --- | --- |
| Ask | message-circle-question-mark | ask, approval |
| Plan | list-checks | todo, plan, skill |
| Agent | users | task, agent, subagent |
| Memory | brain | mem, memory, remember |
| Browser | app-window | playwright, puppeteer, browser, computer, screenshot, click |
| Web | globe | web, fetch, http, url, curl, browse |
| Test | flask-conical | test, tests, pytest, jest, vitest |
| Build | package | npm, cargo, make, build, yarn, pnpm |
| Git | git-branch | git, commit, branch, checkout, rebase, merge |
| Database | database | sql, psql, sqlite, query, database, db |
| Shell | square-terminal | bash, shell, exec, terminal, command, run |
| Edit | file-pen | edit, patch, multiedit, notebookedit, replace |
| Write | file-plus | write, create, new |
| Search | search | grep, glob, search, find, ls |
| Read | file-text | read, view, cat, open |
| MCP | plug | any other `mcp__*` |
| Other | wrench | anything else |

Color is for brands only, so the category glyphs stay calm. Full-color marks (`-color` files) draw as images, rasterized at the size they draw so they stay sharp on a 2x display. Single-color marks draw as masks in their Simple Icons color. In dark mode a brand color darker than 5% luminance (GitHub, Notion, Vercel, Confluence, Sentry) takes the ink color instead. The marks and their licenses are listed in `crates/trace-app/assets/brands/NOTICE.md`; Simple Icons has no Playwright or Slack mark, so those tools use their category.

The sidebar shows the harness logo at 16px in place of the status dot, with the dot as a badge on its lower-right corner, ringed in the row fill. The header subtitle starts with the harness logo at 14px.

## Color tokens

Values come from the reference. Kinds map as follows: agent, turn, subagent, wait, and custom use **agent**; llm uses **llm**; tool uses **tool**. A failed span uses **error** whatever its kind.

| Token | Light | Dark |
|---|---|---|
| tray | `#f0f0f0` | `#19191c` |
| card | `#ffffff` | `#232326` |
| strong / ink | `#171717` | `#ededed` |
| text | `#515154` | `#c7c7cc` |
| muted | `#6e6e73` | `#a1a1a6` |
| faint (decorative only) | `#b8b8b8` | `#4a4a50` |
| line | `#ececec` | `#2e2e33` |
| grid | `#f1f1f1` | `#2a2a2e` |
| guide | `#e6e6e6` | `#34343a` |
| hover | `rgba(0,0,0,.022)` | `rgba(255,255,255,.025)` |
| selected | `rgba(47,107,255,.05)` | `rgba(91,154,255,.08)` |
| chip | `#f5f5f5` | `#2c2c30` |
| wash | `#f7f7f7` | `#1d1d20` |
| accent | `#2f6bff` | `#5b9aff` |
| now | `rgba(47,107,255,.55)` | `rgba(91,154,255,.6)` |
| agent bg / edge / ink / mark | `#eef0f3` `#d3d8e0` `#4b5363` `#9aa3b2` | `#3a3d44` `#50555f` `#b8c0cc` `#7a8291` |
| llm bg / edge / ink / mark | `#efecff` `#cbc2fb` `#5a45d4` `#8d7cf0` | `rgba(141,124,240,.28)` `rgba(167,151,255,.65)` `#c9bfff` `#8d7cf0` |
| tool bg / edge / ink / mark | `#e4f5f1` `#a9ddd1` `#0e7a67` `#3db39a` | `#1c3b35` `#2f7465` `#62d5bb` `#3db39a` |
| error bg / edge / ink / mark | `#fde8e5` `#f1a397` `#c2331f` `#e5604c` | `#46221e` `#8c3c33` `#ff977d` `#e5604c` |
| brand bg / fg (Live pill) | `#eaf1ff` `#2456d6` | `#16243f` `#8ab4ff` |
| ok bg / fg (Done pill) | `#e6f6ec` `#1a7f45` | `#12301f` `#3dd68c` |
| slow bg / fg | `#fff3e0` `#b85c00` | `#3a2410` `#ffa057` |

Muted text meets 4.5:1 on the card in both appearances. Only grid, guide, and faint are lighter, and they never carry text. In dark mode the LLM fill is the lavender mark at 28% over the card with a lighter 1px edge, and agent bars sit one surface step above the card, so neither sinks into it.

Wait spans use the agent palette with an amber mark (`#d99a2b`) so idle time reads as waiting, not work.

Hue means work, and nothing else. Agent, turn, subagent, and custom spans are structure, so they share the neutral agent gray rather than taking a third hue; only LLM (lavender) and tool (teal) spans are colored, and amber always means waiting or idle. The overview's three lanes use the same rule: amber for wait, lavender for LLM, teal for tool, with failures as a red rug.

Increase Contrast (`TW_CONTRAST=1` forces it for screenshots) draws hairlines in **faint**, and bar edges solid in the kind's mark color at 1.5pt.

## Type and spacing

- System UI font (SF Pro), 14px base, -0.006em tracking. Numbers are tabular.
- Row 32px, bar 16px, axis 30px, name column 300px, indent 14px, card radius 16px, tray padding 4px. Easing is `cubic-bezier(0.22, 1, 0.36, 1)`.

## Motion

- Bars reveal left to right in 360ms when a span first appears.
- A folded branch's rows collapse in 360ms with the same easing.
- The popover opens with a fade and a scale from 0.96 over 180ms, and closes in 160ms.
- The live dot pulses on a 1.4s cycle. The running spinner turns every 900ms.
- Redraws batch to the display refresh. While nothing runs, the app is idle with zero animation frames.

## Interaction

Keys match the TUI: `j`/`k` or the arrow keys move, `h`/`l` fold or unfold, Enter opens the popover, Esc closes it, Space pauses or plays, `f` toggles follow, `+`/`-` zoom, `H`/`L` pan, `[`/`]` step through events, `<`/`>` change speed, `g`/`G` jump to the first or last row, `/` or ⌘F searches, `r` toggles replay, `e` expands all. Trackpad scrolling moves rows vertically. A horizontal swipe or shift-scroll pans time. Pinching, or ⌘ with scroll, zooms around the pointer. Clicking a bar selects it, and double-clicking zooms to fit it.
