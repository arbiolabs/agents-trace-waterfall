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

**Sidebar** (260px, tray background). It has a search field, then sessions grouped into **Live**, **Today**, and **Earlier**. Each row shows a status dot (pulsing while live), the project name, and a muted line with start time, duration, and cost. The selected row gets a card-colored pill.

**Header.** It shows "✦ Trace" in strong ink, then the session subtitle in muted ink. On the right are a status pill (Live, Done, Failed, Interrupted), a Replay button, and a Follow toggle when live.

**Trace card.** It holds the stats strip, the time axis, and the rows.

- **Stats strip.** Six cells separated by 1px lines: Duration, Cost, Tokens, LLM calls, Tool calls, Errors. Each cell has a 12px muted label above a 16px semibold tabular value. Unknown cost reads `—`. Errors render in error ink when greater than zero.
- **Columns.** A "Span" header over a 300px name column, then the shared timeline. The timeline shows tick lines in **grid** color, with 11px muted tick labels at their tops.
- **Rows.** Each row is 32px tall. It holds tree guide lines (1px **guide** at each 14px indent), a chevron (rotated -90° when folded), a 20px icon tile (card fill, line ring, 6px radius), the title (13px ink, 500 weight on parents), and a muted detail (model or tool). A "+N" chip on folded parents counts hidden descendants. A spinner marks running spans and a warning glyph marks failed ones.
- **Bars.** 16px tall, 5px radius, filled with the kind's bg and stroked with its edge. A label after the bar reads `duration · cost` in 11.5px text. When there is no room after the bar, the label goes inside it, right-aligned, in the kind's ink. The slowest completed LLM or tool span gets a "Slowest" tag (slow bg and fg). A folded parent's bar shows its children as 6px marks in their kind's mark color.
- **Running bars.** They extend to now, a soft highlight sweeps across them every 1.5s, and their right edge glows.
- **Now line.** A 1px **now** color line with a 6px accent dot where it meets the axis.
- **Hover and selection.** Hovering a row gives it the **hover** wash and gives its bar an inner ring in the mark color. The selected row gets the **selected** wash.

**Detail popover.** It opens with Enter or a click. It anchors under the selected row, 330px wide, with a 16px radius and the **pop** shadow.

- **Head.** A 28px icon tile, then the name (13.5px semibold) over "Kind · model", then a status pill and a close button.
- **Stats.** A wash panel with Duration, Started (+offset), Cost, Tokens in, Tokens out, and Spans (descendants).
- **Error.** An error box shows when the span failed.
- **Input and Output.** Each reads `Not captured` in muted ink when absent.
- **More.** Events and Files sections follow when present.

**Replay transport.** In replay, a floating capsule sits centered at the bottom with play/pause, step back, step forward, a speed segmented control (0.25× to 10×), a scrubber, and the time readout.

**Empty state.** A centered card explains `trace-tui collector start` and offers **Import JSONL…**. Dropping a `.jsonl` file onto the window also imports it.

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
| agent bg / edge / ink / mark | `#eef0f3` `#d3d8e0` `#4b5363` `#9aa3b2` | `#2c2f35` `#41464f` `#b8c0cc` `#7a8291` |
| llm bg / edge / ink / mark | `#efecff` `#cbc2fb` `#5a45d4` `#8d7cf0` | `#2a2545` `#4c3fa0` `#b6a9ff` `#8d7cf0` |
| tool bg / edge / ink / mark | `#e4f5f1` `#a9ddd1` `#0e7a67` `#3db39a` | `#16302b` `#285f54` `#62d5bb` `#3db39a` |
| error bg / edge / ink / mark | `#fde8e5` `#f1a397` `#c2331f` `#e5604c` | `#3b1714` `#74302a` `#ff977d` `#e5604c` |
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
