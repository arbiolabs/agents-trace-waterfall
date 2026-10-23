# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Replaying a session that is still running follows the recording as it grows: playback waits at the
  live edge and plays on as the agent works. Both frontends mark it: a pulsing Recording pill, a LIVE
  tag and "live edge" in the player, and Back to live in place of Exit replay (TUI: "still recording").

- Oh My Pi adapter (`omp`): tails `~/.omp/agent/sessions`, pi's format with Oh My Pi's own `duration`
  and `completedAt` timing; sessions read as Oh My Pi.
- A conformance test: one scenario recorded in each harness's own format must yield the same trace shape.
- Codex rollouts compressed to `.jsonl.zst` are read, and archived sessions can be tailed.

### Fixed

- OpenCode: a shell command that exits non-zero is a failed tool (OpenCode marks it completed with the
  exit code in its metadata). A fork no longer repeats its parent's copied history, and context
  compaction counts as a model call with its tokens and cost.
- pi: tools that ran in parallel each end by their own measured duration, not when the whole batch's
  results were written.
- Codex: review and compaction threads stand as their own sessions instead of dangling as sub-agents
  with no parent; item times of 0 count as unrecorded.

### Changed

- Durations put a space between units: `1h 7m`, `2m 30s`.

- Idle time is folded out of the timeline in both frontends: an idle stretch takes no width and shows as
  a break with its length (`17h`, `2d`), so a long session's work fills the view. Replay jumps across
  idle time instead of playing through it.
- Every idle stretch of 10 minutes or more folds, however long the session (up to 64 of them).
- The replay player counts active time (`1:12:05 / 6:22:04`) and its scrubber moves through active time
  only; the session's wall-clock length reads separately, rounded (`14 days overall`).
- Desktop header: the open session is centred, the project name in full ink with harness and model
  muted beside it, and the app's mark is a waterfall of bars instead of a sparkle.

### Removed

- The idle bands with their Skip button, and Go → Next / Previous Activity (`{` `}` in the TUI): folding
  makes them unnecessary. The overview's idle bands still jump past an idle stretch when clicked.

## [1.1.0] - 2026-10-23

Codex, pi and OpenCode join Claude Code, every harness's sub-agents nest inside the parent's trace, and
costs no harness reports are estimated from model list prices.

### Added

- Codex CLI adapter (`codex`): tails `~/.codex/sessions` rollouts. Turns, one span per model
  response with cached-token split, tool calls, and the commands, file changes, MCP calls and web
  searches inside them with Codex's own start and end times.
- pi adapter (`pi`): tails `~/.pi/agent/sessions`. Model calls use pi's recorded request start and
  duration and carry its cost estimate; tool calls end at their results.
- OpenCode adapter (`opencode`): reads OpenCode 2.x's `opencode.db` read-only, for import and live
  tailing, re-reading a session whenever it changes. Turns end with OpenCode's idle outcome.
- Pi is a recognized harness name.
- Sub-agents for every harness: Codex sub-agent threads, OpenCode child sessions and pi sub-agent
  extension runs join the root session, nested under the tool call that started them.
- OpenCode before 2.0: sessions in the `session`, `message` and `part` tables, one model call per step,
  and 1.x's JSON-file `storage` folder (`--tail opencode=~/.local/share/opencode/storage`).
- pi branches: a turn that continues from an earlier message is named `turn N (branch)`.
- pi sub-agents that keep their own session: a session started from a parent during one of its tool
  calls nests under that call, live. Forks and clones stay separate, without their copied history.
- Cost estimates: a model call that reports tokens but no cost (Claude Code, Codex) is priced from the
  model's API list price, cache reads and writes included, and marked as an estimate. Totals that
  include estimates read `~$1.20`; existing stores recompute their session totals once on open.

### Fixed

- A Codex sub-agent closes when its own turn ends, even if the parent never reports it done.
- The sidebar counts the main agent's turns only, not its sub-agents'.

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

[Unreleased]: https://github.com/diego-arbio-dev/agents-trace-waterfall/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/diego-arbio-dev/agents-trace-waterfall/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/diego-arbio-dev/agents-trace-waterfall/releases/tag/v1.0.0
