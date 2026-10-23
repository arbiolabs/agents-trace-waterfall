# Agents Trace Waterfall

A local-first terminal trace waterfall for coding agents. It shows runs, turns, LLM calls, tool calls, subagents, and waits on one shared time axis. You can watch a run live, browse it later, or replay it on a virtual clock.

![The desktop app following a live Claude Code session](docs/media/desktop.gif)

![The TUI following the same kind of live session, then inspecting a span](docs/media/tui.gif)

See [spec.md](spec.md) for the design, [docs/protocol.md](docs/protocol.md) for the collector protocol, and [docs/privacy.md](docs/privacy.md) for what is recorded.

## Quick start

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"

# Browse a recorded trace.
trace-tui import examples/sample-trace.jsonl
trace-tui

# Watch a simulated agent live.
trace-tui collector start &
simulated-live --socket
trace-tui live

# Record your agents' sessions as they happen (metadata only).
trace-tui collector start \
  --tail claude-code=$HOME/.claude/projects \
  --tail codex=$HOME/.codex/sessions \
  --tail pi=$HOME/.pi/agent/sessions \
  --tail opencode=$HOME/.local/share/opencode/opencode.db
```

## Supported harnesses

| Harness | Adapter | Reads | Notes |
|---|---|---|---|
| Claude Code | `claude-code` | `~/.claude/projects/**/*.jsonl` | Sub-agents nest under the tool call that launched them. |
| Codex CLI | `codex` | `~/.codex/sessions/**/rollout-*.jsonl` | Commands, file changes, MCP calls and web searches keep Codex's own timings. Sub-agent threads join the root session under their `spawn_agent` call. |
| pi | `pi` | `~/.pi/agent/sessions/**/*.jsonl` | Model calls carry pi's request timing and cost estimate. Branches are marked. Sub-agent sessions started during a tool call nest under it, live; runs of pi's sub-agent extension nest once the call returns. Forks and clones stay their own sessions. |
| OpenCode | `opencode` | `~/.local/share/opencode/opencode.db`, or `~/.local/share/opencode/storage` for 1.x | Read-only; sessions are re-read as they change. Every SQLite version, and 1.x's JSON files. Sub-agent sessions nest under the task call that started them. |

Every harness nests its sub-agents inside the parent's trace, so one session shows the whole run.

Costs a harness doesn't report are estimated from the model's API list price (Claude and GPT-5 / GPT-4 models, cache reads and writes included) and shown as `~$1.20`. On a subscription plan this is what the same usage would cost through the API, not what you are billed. Unknown models stay `—`.

Every adapter records metadata only: turns, model calls, tool names, timings, tokens and cost, never prompt, response or tool text. Any other harness can send events directly through `trace-adapter-sdk` or the socket protocol in [docs/protocol.md](docs/protocol.md). `trace-tui doctor` lists what each adapter cannot observe.

## Desktop app

`agents-trace-waterfall` is a native macOS app built with GPUI and gpui-kit. It reads the same database as the TUI and follows the system light or dark appearance. See [docs/design.md](docs/design.md) for its design.

```sh
scripts/bundle.sh                                       # builds target/release/Agents Trace Waterfall.app with its icon
open "target/release/Agents Trace Waterfall.app"
./target/release/agents-trace-waterfall                 # or run the binary directly
./target/release/agents-trace-waterfall --open 7ad34f22 # open a session by id prefix
```

Click a span for its details. Drag the overview strip to move through long sessions. Drop a `.jsonl` file on the window to import it. The keyboard shortcuts match the TUI.

## Commands

| Command | Purpose |
|---|---|
| `trace-tui` | Sessions overview. |
| `trace-tui live` | Sessions overview filtered to live runs. |
| `trace-tui open <id>` | Open a session's waterfall. Accepts a unique id prefix. |
| `trace-tui replay <id> --speed 2` | Replay a session on a virtual clock. |
| `trace-tui import <file> [--adapter claude-code\|codex\|pi\|opencode]` | Import a transcript, or an OpenCode database. |
| `trace-tui export <id> [-o file]` | Write the normalized journal as JSONL. |
| `trace-tui collector start [--tail adapter=path]` | Run the ingestion daemon. |
| `trace-tui doctor` | Check storage, socket, collector, and adapters. |
| `trace-tui purge` | Apply retention. |

Press `?` in the TUI for the keymap.

## Configuration

`~/.config/agents-trace-waterfall/config.toml`, or the file named by `AGENTS_TRACE_WATERFALL_CONFIG`:

```toml
db = "~/.local/share/agents-trace-waterfall/traces.db"  # or AGENTS_TRACE_WATERFALL_DB
# socket = "..."                                         # or AGENTS_TRACE_WATERFALL_SOCKET; default is per-user, see docs/protocol.md
theme = "default"                                        # ascii, monochrome, ascii-monochrome

[retention]
metadata_days = 90
payload_days = 30

[capture]
payloads = false
projects = []

[[tail]]
adapter = "claude-code"
path = "~/.claude/projects"

[[tail]]
adapter = "codex"
path = "~/.codex/sessions"
```

## Layout

| Crate | Owns |
|---|---|
| `trace-core` | Domain types, the reducer shared by live and replay, ordering, cost rules. |
| `trace-storage` | SQLite journal, projections, retention, redaction. |
| `trace-ingest` | Collector daemon, socket server, adapters (`canonical`, `claude-code`). |
| `trace-render` | Waterfall widget, time axis, tree flattening, clipping. |
| `trace-view` | The session document shared by the TUI and the desktop app: live polling, replay, folding, viewport. |
| `trace-tui` | CLI and Ratatui app. |
| `trace-app` | GPUI desktop app (`agents-trace-waterfall`). |
| `trace-adapter-sdk` | Harness integration: recorder, socket client, JSONL helpers. Depends only on `trace-core`. |
| `examples/simulated-live` | Deterministic agent run generator for demos, fixtures, and perf tests. |

Run `cargo test --workspace` to check everything.
