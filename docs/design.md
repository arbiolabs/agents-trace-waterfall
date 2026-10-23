# Desktop app design

`trace-waterfall` is the macOS desktop app, built with GPUI through gpui-kit. It answers the same questions as the TUI: what happened, in what order, why it was slow, what it cost, and what failed. It shares the session document (`trace-view`) with the TUI, so live, pause, replay, folding, and search behave identically.

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

**Header.** It shows "✦ Trace" in strong ink, then the session subtitle in muted ink. On the right are a status pill (Live, Done, Failed, Interrupted, or `Replay · 0:06`), a Follow toggle when live, and a Replay button. Every header control is 28px tall with 8px gaps. The status pill has a card fill, a 1px **line** ring, and a soft shadow; the status color appears only on its dot and text.

**Trace card.** It holds the stats strip, the time axis, and the rows.

- **Stats strip.** Six cells separated by 1px lines: Duration, Cost, Tokens, LLM calls, Tool calls, Errors. Each cell has a 12px muted label above a 16px semibold tabular value. Unknown cost reads `—`. Errors render in error ink when greater than zero.
- **Overview.** The whole session as one thin lane per kind. Each pixel column's alpha is its span count over the lane's busiest column, so dense traces read as density, not solid stripes. The visible window is a lens at least 24px wide, with 6px drag handles; a lens narrower than that gets a 2px accent ring. The caption reads `All · 16.2s`, or `7h18m – 7h31m of 8h12m` with a **Show all** button when zoomed.
- **Columns.** A "Span" header over a 300px name column, then the shared timeline. The timeline shows tick lines in **grid** color, with 11px muted tick labels at their tops.
- **Rows.** Each row is 32px tall. It holds tree guide lines (1px **guide** at each 14px indent), a chevron (rotated -90° when folded), a 20px icon tile (card fill, line ring, 6px radius), the title (13px ink, 500 weight on parents), and a muted detail (model or tool). A "+N" chip (18px capsule, 11px muted tabular) follows the title of a folded parent and counts hidden descendants. An MCP tool `mcp__server__tool` reads as `tool` with `server` as its detail. Long titles truncate in the middle and show the full name in a tooltip. A spinner marks running spans and a warning glyph marks failed ones.
- **Bars.** 16px tall, 5px radius, filled with the kind's bg and stroked with its edge. A label after the bar reads `duration · cost` in 11.5px text. When there is no room after the bar, the label goes inside it, right-aligned, in the kind's ink. Outside the bar the label sits on a 2px knockout in the row color, so grid and now lines pass beneath it. A bar cut by a timeline edge squares off there and fades in over 12px. The slowest completed LLM or tool span gets a "Slowest" tag (slow bg and fg). A folded parent's bar shows its children as 6px marks in their kind's mark color.
- **Off-screen spans.** A span wholly outside the visible window shows a chevron in its mark color at that edge with an 11px muted hint, `2.7s · 1h5m ago` or `3m later`. Clicking it pans to the span. Its title dims to **text**.
- **Big traces.** Above 500 spans, a trace opens with every finished turn folded except the latest.
- **Running bars.** They extend to now, a soft highlight sweeps across them every 1.5s, and their right edge glows.
- **Now line.** A 1px **now** color line with a 6px accent dot where it meets the axis. Any manual pan, zoom, or lens drag stops following. While the now line is off-screen, a "Jump to now" capsule floats at the timeline's top right.
- **Hover and selection.** Hovering a row gives it the **hover** wash and gives its bar an inner ring in the mark color. The selected row gets the **selected** wash.

**Detail popover.** It opens with Enter or a click. It sits 6px below the selected bar, aligned to the bar's start and kept 12px inside the card, with an 8px arrow pointing at the bar. It flips above only when it would overflow below and there is more room above. It never grows past the card; its content scrolls instead. It is 330px wide, with a 16px radius and the **pop** shadow.

- **Head.** A 28px icon tile, then the name (13.5px semibold) over "Kind · model", then a status pill and a close button.
- **Stats.** A wash panel with Duration, Started (+offset), Cost, Tokens in, Tokens out, and Spans (descendants).
- **Error.** An error box shows when the span failed.
- **Input and Output.** Each reads `Not captured` in muted ink when absent.
- **More.** Events and Files sections follow when present.

**Replay transport.** In replay, a floating capsule sits centered at the bottom with play/pause, step back, step forward, a speed segmented control (0.25× to 10×), a scrubber, and the time readout.

**Empty state.** A centered card explains `trace-tui collector start`, with a copy button inside the command chip (⌘C also copies; it shows "Copied" for 1.2s), and offers **Import JSONL…**. A faint sample waterfall sits behind it. Dropping a `.jsonl` file onto the window also imports it.

## Color tokens

Values come from the reference. Kinds map as follows: agent, turn, subagent, wait, and custom use **agent**; llm uses **llm**; tool uses **tool**. A failed span uses **error** whatever its kind.

| Token | Light | Dark |
|---|---|---|
| tray | `#f0f0f0` | `#19191c` |
| card | `#ffffff` | `#232326` |
| strong / ink | `#171717` | `#ededed` |
| text | `#6b6b6b` | `#a1a1a6` |
| muted | `#8a8a8a` | `#7c7c82` |
| faint | `#b8b8b8` | `#4a4a50` |
| line | `#ececec` | `#2e2e33` |
| grid | `#f1f1f1` | `#2a2a2e` |
| guide | `#e6e6e6` | `#34343a` |
| hover | `rgba(0,0,0,.022)` | `rgba(255,255,255,.025)` |
| selected | `rgba(47,107,255,.05)` | `rgba(91,154,255,.08)` |
| chip | `#f5f5f5` | `#2c2c30` |
| wash | `#f7f7f7` | `#1d1d20` |
| accent | `#2f6bff` | `#5b9aff` |
| now | `rgba(47,107,255,.55)` | `rgba(91,154,255,.6)` |
| agent bg / edge / ink / mark | `#eef0f3` `#d3d8e0` `#4b5363` `#9aa3b2` | `#33363d` `#4d535d` `#b8c0cc` `#7a8291` |
| llm bg / edge / ink / mark | `#efecff` `#cbc2fb` `#5a45d4` `#8d7cf0` | `#332c5c` `#5a4cc0` `#b6a9ff` `#8d7cf0` |
| tool bg / edge / ink / mark | `#e4f5f1` `#a9ddd1` `#0e7a67` `#3db39a` | `#1c3b35` `#2f7465` `#62d5bb` `#3db39a` |
| error bg / edge / ink / mark | `#fde8e5` `#f1a397` `#c2331f` `#e5604c` | `#46221e` `#8c3c33` `#ff977d` `#e5604c` |
| brand bg / fg (Live pill) | `#eaf1ff` `#2456d6` | `#16243f` `#8ab4ff` |
| ok bg / fg (Done pill) | `#e6f6ec` `#1a7f45` | `#12301f` `#3dd68c` |
| slow bg / fg | `#fff3e0` `#b85c00` | `#3a2410` `#ffa057` |

Wait spans use the agent palette with an amber mark (`#d99a2b`) so idle time reads as waiting, not work.

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
