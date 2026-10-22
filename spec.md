# Trace Waterfall TUI — Specification v0.1

**Status:** Proposed  
**Stack:** Rust, Ratatui, Crossterm, Tokio, SQLite  
**Principle:** One event stream, two modes: live and history.

## 1. Goal

Build a local-first, terminal-native trace waterfall for coding agents. Show a hierarchical timeline of agent runs, turns, LLM calls, tool calls, subagents and waits. Support live inspection, historical browsing, deterministic replay, and cost/performance diagnostics.

**User questions:** What happened? In what order? Why was it slow? What cost money? What failed? What files changed?

**Reference:** https://www.affectui.com/components/trace-waterfall

## 2. Scope

### MVP
- Live session list with status, origin, project, started time, duration, model, cost, token counts and errors.
- Attach to a live trace without interrupting its agent.
- Hierarchical waterfall of nested spans sharing a time axis.
- Pause/resume view, follow-now, zoom, horizontal pan, vertical navigation, fold/unfold.
- Select a span and inspect metadata, input/output previews, errors and events.
- Persist events and inspect completed or interrupted sessions offline.
- Replay historical sessions at selectable speeds, seek, step between events.
- Search/filter by name, kind, status, model and tool.
- JSONL import/export; native ingestion interface for adapters.

### Later
- Cross-session comparison, cost budgets, critical-path analysis, diff viewer, OTLP receiver, multi-machine sync, alternate agent lanes and causality graph.

### Non-goals for MVP
- Running or controlling agents; cloud telemetry platform; editing traces; reconstructing unrecorded reasoning; collecting secrets by default.

## 3. Information architecture

1. **Sessions:** searchable list; live and historical mixed, filter by status/project/source.
2. **Waterfall:** selected session, hierarchy and timeline.
3. **Inspector:** selected span details; tabs Overview / Input / Output / Events / Error / Files.
4. **Replay:** same Waterfall view driven by a virtual clock rather than real time.
5. **Help:** keymap and capabilities.

Layout >= 120 columns: left 30–42 columns span tree, right timeline, optionally inspector at bottom or right depending on height. On 80–119 columns: compact labels, inspector toggles overlay. Below 80 columns: switch between tree, timeline and inspector, never silently hide data. Minimum supported size 60x18.

## 4. Interaction model

| Key | Action |
|---|---|
| `j/k`, `↑/↓` | Select preceding/following visible span |
| `h/l`, `←/→` | Collapse/expand branch or select parent/first child |
| `Enter` | Open inspector |
| `Esc` | Return/back/close overlay |
| `Tab` | Cycle focus: sessions/tree/timeline/inspector |
| `Space` | Pause/resume live view or playback |
| `f` | Follow now toggle |
| `+/-` | Zoom into/out of time axis |
| `H/L` | Pan backward/forward in time |
| `g/G` | First/last visible span |
| `[` / `]` | Previous/next event in replay |
| `<` / `>` | Decrease/increase replay speed |
| `/` | Search spans |
| `F` | Filters |
| `e` | Toggle expansion of all descendants (within limits) |
| `s` | Sessions screen |
| `?` | Help |
| `q` | Quit |

Make keymap configurable later. Always show relevant shortcut hints in footer.

## 5. Domain model

### Session
- `session_id`: UUID/ULID
- `project_id`, `working_directory`, `source_adapter`, `harness`, `host`
- `started_at_utc_ns`, `ended_at_utc_ns?`
- `status`: live / completed / failed / interrupted / unknown
- `root_span_id?`
- optional labels and summary; privacy policy metadata

### Span
- `span_id`, `session_id`, `trace_id`, `parent_span_id?`
- `name`, `kind`: agent / turn / llm / tool / subagent / wait / custom
- `status`: running / succeeded / failed / cancelled / unknown
- `started_at_utc_ns`, `ended_at_utc_ns?`
- `model?`, `provider?`, `tool_name?`, `agent_name?`
- `tokens_input?`, `tokens_output?`, `cache_read?`, `cache_write?`, `cost_usd_micros?`
- attributes map and linked payload references

### Span event
- `event_id`, `span_id`, `timestamp_utc_ns`, `event_type`, `attributes`, `payload_ref?`

### Canonical source event
- `schema_version`, `source_id`, `source_seq`, `event_id`, `observed_at_utc_ns`, `occurred_at_utc_ns`, `session_id`, `span_id?`, `event_type`, `body`
- Types: `session.started`, `session.ended`, `span.started`, `span.updated`, `span.ended`, `span.event`, `usage.reported`, `artifact.recorded`, `heartbeat`.
- A retry creates a new span with `retry_of` linkage; never overwrite the failed attempt.

Events are append-only and idempotent on `(source_id, event_id)`. Use sequence numbers per source for ordering and to detect gaps. Reconcile events arriving out of order by stable IDs and render missing parents in an `Unresolved` group pending arrival. On reconnect, request cursor-based catchup, then resume subscription. Event-time is authoritative for historical chronology; ingestion-time remains available to diagnose delivery delay. Clocks across hosts may be skewed: capture clock offsets where possible and display uncertainty rather than claiming exact concurrency.

### Cost accounting
Usage and cost belong to the billing operation (usually an LLM span) and must not be summed again at ancestor level. Report aggregate costs from unique billable leaf records; distinguish observed actual charge vs model-price estimate vs unknown. Partial token deltas are provisional and must be replaced, not double counted, when final usage arrives. Display unknown as `—`.

### Duration and critical path
- Span elapsed = end-or-view-clock minus start (clamped to >=0 with clock-skew warning).
- Axis start = session start; bars share the same time-to-column mapping.
- Parent time is inclusive; avoid summing child durations to infer total wall clock.
- Concurrent child spans may overlap. Sum of operation durations != total session duration.
- Slowest call means greatest completed eligible LLM/tool duration (toggleable).
- Critical path is deferred pending explicit dependency/cause edges; don't infer it from nesting alone.

## 6. Live versus replay: one reducer

```
Ingestion adapters -> Normalizer -> Append-only journal -> Materializer -> UI state
                                  |                        |
                                  +-> SQLite               +-> Live clock
                                                           +-> Replay clock
```

The same pure reducer applies events in both modes:

`apply(state, event) -> state`

Live mode reads the subscription and uses `now` to render open spans; historical mode loads a snapshot plus journal events and uses a controllable virtual clock. Pausing live mode freezes its display cursor, NOT source collection. Resume by catching up buffered events without losing events. Track visible lag, queue depth and dropped events explicitly.

### Replay semantics
- Enter replay at run start, run end, or a bookmarked event.
- Speeds: 0.25x, 0.5x, 1x, 2x, 5x, 10x.
- Seek with timeline cursor; step to previous/next canonical event.
- Rendering at time T includes events with occurrence time <= T; an unfinished span ends visually at T.
- Snapshots checkpoint periodically (for example, each 500 events), so seeking does not replay entire history every time.
- Reconstruct event-order deterministically using `(occurred_at, source_id, source_seq, event_id)`; tie-breaking does not imply causality.
- Keep a clear separation between the **recorded execution** and virtual replay; replay never invokes tools or LLMs.

## 7. Timeline rendering

Custom `StatefulWidget` using Ratatui `Buffer`, not one Gauge widget per span. Vertical virtualized rows only. Each row has fold indicator, kind/status glyph, indentation, clipped name and duration/cost metadata; the remaining columns are a shared timeline. For each visible span:

```
start_x = floor((span.start_ms - viewport.start_ms) / viewport.ms_per_cell)
end_x   = ceil((effective_end_ms - viewport.start_ms) / viewport.ms_per_cell)
```

Clip to viewport; guarantee a one-cell marker for visible sub-cell operations. Unicode `█`, `▓`, `░`, `│`; ASCII fallback `#`, `=`, `.`, `|`. Draw readable tick labels and optional vertical grid lines. Status-color legend: agent slate, LLM violet, tool teal, failure red, wait amber. Color-blind and monochrome modes must distinguish statuses with glyphs, not color alone.

Running bars extend to the current clock; provide a fixed 'now' cursor where in viewport. Follow mode pans or expands gracefully when current time passes the right edge; manual horizontal pan disables follow. Maintain stable row selection by span ID when new children arrive. Collapsing a parent hides descendants but does not delete them. Optional compact child marks on collapsed parent.

Don't rerender for every raw token. Batch UI invalidations to ~10–20 fps maximum while live; idle event-driven redraw.

## 8. Inspector

Tabs: Overview, Input, Output, Events, Error, Files. Display timestamps, duration, self/inclusive time if safely computable, model/provider, usage, cost provenance, source, parent, attributes and child counts. Input/output payloads are optional, truncated previews by default, with an explicit expand-to-full action when permitted. Show `not captured` rather than synthesizing missing data. Support vertical scroll, text wrapping, copy selected details (OSC52 where supported, fallback), and `e` export selected span JSON.

**Privacy:** Default collection captures metadata, timings, sanitized tool names and usage, **not** raw prompts, outputs, env vars or tool stdout. Full payload capture is opt-in per adapter/project. Redact known secrets before writing to disk; file permissions 0600; configurable retention and purge. Avoid storing credentials in trace attributes.

## 9. Ingestion and persistence

### MVP transport
- Local Unix domain socket for JSONL event frames (Windows named pipes deferred).
- CLI import from JSONL for historical datasets.
- Adapter interface supports file tailing and SDK-based event forwarding.
- Socket messages versioned, bounded in size, acknowledged with source cursor; reconnect/backfill required.
- Agent execution is independent of TUI: collector daemon persists traces while TUI is closed.

### Adapter contract

```
trait TraceAdapter {
    fn source_info(&self) -> SourceInfo;
    fn normalize(&self, raw: RawEvent) -> Result<Vec<CanonicalEvent>>;
}
```

Start with one first-party adapter for the agent harness being instrumented; any integration must disclose which timings, usage and payload fields it cannot observe. Later: OpenTelemetry/OTLP intake and file-based bridges for third-party harnesses.

### Storage
SQLite in WAL mode, schema migrations, indexed tables for `sessions`, `spans`, `events`, `usage`, `payloads`, `snapshots`, `source_cursors`. Journal contains canonical events, materialized tables accelerate list queries and are rebuildable. Compress large opt-in payload blobs; retention policy separated for metadata vs payload data. Use one writer task to avoid write contention and bounded subscriber broadcast channels.

## 10. Proposed crates / modules

```
trace-waterfall/
  Cargo.toml
  crates/
    trace-core/        # Domain types, reducer, clock, ordering, cost rules
    trace-storage/     # SQLite journal, projections, snapshots, queries
    trace-ingest/      # Event server, normalization, adapter protocol
    trace-render/      # Waterfall widget, axis, tree flattening, clipping
    trace-tui/         # CLI, Ratatui app, navigation, inspectors, replay
    trace-adapter-sdk/ # Harness integration library and JSONL helpers
  examples/
    simulated-live/    # Deterministic live feed with errors and concurrency
    sample-trace.jsonl
  tests/
    fixtures/
  docs/
    protocol.md
    privacy.md
```

Dependencies: Rust stable; Ratatui + Crossterm; Tokio for async runtime; serde/serde_json; rusqlite (bundled SQLite) or sqlx (choose one); uuid/ulid; tracing for internal diagnostics; clap for CLI. Pin compatible versions at implementation time.

Internal agent adapter integration should not force a dependency on the TUI or storage crate.

## 11. CLI surface

```
trace-tui                       # Sessions overview
trace-tui live                  # Filter to live runs
trace-tui open <session-id>     # Open waterfall
trace-tui replay <session-id> --speed 2
trace-tui import ./run.jsonl
trace-tui export <session-id> --format jsonl
trace-tui collector start       # Background ingestion process
trace-tui doctor                # Verify socket, storage, adapter health
```

Use environment variables and config TOML for collector path, database location, retention, colors, capture mode and adapter directories.

## 12. Detailed workflow examples

### Watch live
1. Instrumented harness emits session and span events to local collector.
2. Collector journals/acknowledges events and publishes updates.
3. Open `trace-tui live`, select session and press Enter.
4. Waterfall follows the current time and shows running bars; press Space to freeze visual time without stopping recording.
5. Inspect a failed tool span, then press `f` to return to live follow.
6. On completion the session transitions to completed, remains navigable, and is replayable.

### Inspect history
1. Start `trace-tui` and filter by project, error status or date.
2. Open a completed session and inspect summary and slowest span.
3. Collapse branches, zoom into expensive interval, inspect retries.
4. Start replay to see what information was present at each moment.
5. Export the normalized trace for bug reports without leaking opt-in payloads unintentionally.

## 13. Testing and acceptance criteria

### Correctness
- Out-of-order start/end events, missing parents, duplicate event deliveries, interrupted sessions, retries, and concurrent children are covered by tests.
- Final token usage supersedes incremental usage; no double-counting in parent aggregates.
- Identical imported event journals produce identical projected trace state at the same replay timestamp.
- Pausing/resuming the UI cannot drop persisted events.
- Reopening a complete session offline works without collector running.
- Session interruption after collector restart is reconciled and labeled correctly.

### UI
- Correct positioning/clipping at zoom boundaries and zero-duration spans.
- Terminal resize remains navigable at 60x18.
- Stable selection when children arrive; collapsed branches stay collapsed.
- Every interactive action usable from keyboard; monochrome fallback readable.
- Graceful performance target: smooth >=10 fps redraw during a synthetic 10,000-span session on typical developer hardware, without attempting to render offscreen rows.

### Operational
- Lossless ingestion for supported source backfill; gap indicators when upstream cannot replay.
- Collector survives TUI exit and restarts without duplicate spans.
- Backpressure and disk errors are explicit; never claim lossless capture when durability/acks fail.
- Retention and payload redaction covered by security tests.

## 14. Milestones

**M0 — Fixture and renderer:** generate synthetic nested trace; build custom waterfall widget, selection, folding, zoom/pan, inspector.

**M1 — History:** SQLite journal, projections, import/export, session browser, virtual replay clock, seek/snapshot tests.

**M2 — Live:** collector daemon, Unix socket protocol, one harness adapter, reconnect/catchup, pause/follow and graceful interruption.

**M3 — Diagnostics:** filters, slowest operation, cost aggregation, token breakdown, retry highlighting, provenance labels, UI performance and privacy hardening.

## 15. Architectural decisions

1. **Event sourcing rather than snapshots-only:** enables faithful replay, repair, and debugging ingestion races.
2. **Collector separated from TUI:** closing the interface must not stop recording.
3. **Ratatui custom renderer:** shared-axis bars and large trees need exact cell-level drawing and virtualization.
4. **A single reducer with interchangeable clocks:** live and history remain functionally consistent.
5. **Capture metadata by default:** agent traces regularly contain secrets and proprietary source code.
6. **Store observations, not imaginary reasoning:** explain known inputs, tool results and timings; don't pretend private chain-of-thought is available.
7. **Respect concurrency:** a tree is nesting, not automatically a full causal dependency graph.

## 16. Potential extension: causality and file-diff awareness

Add span links (`caused_by`, `retry_of`, `spawned_by`) and structured artifacts (file path, hash, change summary, patch reference, git commit). This can power a future Chronological/Causal mode and deep integration with developer-agent replay tools without changing the event-journal foundation.

## References
- affectUI Trace Waterfall: https://www.affectui.com/components/trace-waterfall
- Ratatui docs: https://docs.rs/ratatui/latest/ratatui/
- OpenTelemetry tracing specification: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/trace/api.md
- TraceCraft TUI (existing prior art): https://www.tracecraft.io/user-guide/tui/
