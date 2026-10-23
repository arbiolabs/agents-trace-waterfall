# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.0.0] - 2026-10-23

The first release: a local-first trace waterfall for coding agents, as a native macOS app and a
terminal UI over the same recorded traces.

### Added

**Recording**
- Collector daemon (`trace-tui collector start`) that journals events from a local socket and
  keeps recording while no viewer is open. It reconnects, backfills by cursor and marks gaps.
- Claude Code adapter that tails `~/.claude/projects` transcripts, plus a canonical JSONL adapter.
- JSONL import and export of a session's normalized journal.
- SQLite store in WAL mode. Events are append-only and idempotent. Sessions that stop reporting
  are reconciled as interrupted.
- Privacy by default: metadata only, payload capture opt-in per project, known secrets redacted
  before they reach disk, owner-only database files, retention and purge. See `docs/privacy.md`.
- `trace-adapter-sdk` for instrumenting other harnesses, and `simulated-live` for demos and tests.

**Desktop app (Agents Trace Waterfall)**
- A waterfall of agent runs, turns, LLM calls, tool calls, sub-agents and waits on one shared
  time axis, live or historical.
- A sidebar of sessions by recency or grouped by workspace and project, with search.
- Live follow, pause, zoom, pan, an overview strip with a draggable lens, and replay on a
  virtual clock.
- A span popover with timing, cost, tokens, errors, events and files.
- Brand marks for harnesses, model vendors and tools. Agent rows read as the harness
  ("Claude Code") with the session's main model.
- Span notes: write a note on any span from its popover, with `n`, or from the row menu. Notes
  are stored beside the trace, never in it, and are left out of exports.
- Collapse or expand everything, in the timeline (⌥⌘← / ⌥⌘→) and in the sidebar's workspaces.
- Long sessions: idle stretches squeeze to labelled bands in the overview and show on the
  timeline as bands with a Skip button. Go → Next / Previous Activity (⌥⌘] / ⌥⌘[) jumps across them.
- Native macOS behaviour: light and dark appearance, vibrancy, menus, context menus, focus
  rings, VoiceOver labels, Reduce Motion, Increase Contrast and Reduce Transparency.
- `scripts/bundle.sh` builds `Agents Trace Waterfall.app` with its icon.

**Terminal UI (`trace-tui`)**
- Sessions list with filters, a waterfall with tree and timeline, an inspector with Overview,
  Input, Output, Events, Error and Files tabs, search, replay, and copy or export of span JSON.
- The same naming as the desktop app. Plus span notes (`n`), fold all (`E`), idle marks on the
  time axis and previous / next activity (`{` / `}`).
- Unicode and ASCII glyphs, colour and monochrome themes. Usable down to 60×18.

[Unreleased]: https://github.com/diego-arbio-dev/agents-trace-waterfall/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/diego-arbio-dev/agents-trace-waterfall/releases/tag/v1.0.0
