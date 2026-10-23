# trace-waterfall

A local-first terminal trace waterfall for coding agents. It shows runs, turns, LLM calls, tool calls, subagents, and waits on one shared time axis. You can watch a run live, browse it later, or replay it on a virtual clock.

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

# Record Claude Code sessions as they happen (metadata only).
trace-tui collector start --tail claude-code=$HOME/.claude/projects
```

## Desktop app

`trace-waterfall` is a native macOS app built with GPUI and gpui-kit. It reads the same database as the TUI and follows the system light or dark appearance. See [docs/design.md](docs/design.md) for its design.

```sh
scripts/bundle.sh                                # builds target/release/Trace Waterfall.app with its icon
open "target/release/Trace Waterfall.app"
./target/release/trace-waterfall                 # or run the binary directly
./target/release/trace-waterfall --open 7ad34f22 # open a session by id prefix
```

Click a span for its details. Drag the overview strip to move through long sessions. Drop a `.jsonl` file on the window to import it. The keyboard shortcuts match the TUI.

## Commands

| Command | Purpose |
|---|---|
| `trace-tui` | Sessions overview. |
| `trace-tui live` | Sessions overview filtered to live runs. |
| `trace-tui open <id>` | Open a session's waterfall. Accepts a unique id prefix. |
| `trace-tui replay <id> --speed 2` | Replay a session on a virtual clock. |
| `trace-tui import <file> [--adapter claude-code]` | Import JSONL. |
| `trace-tui export <id> [-o file]` | Write the normalized journal as JSONL. |
| `trace-tui collector start [--tail adapter=path]` | Run the ingestion daemon. |
| `trace-tui doctor` | Check storage, socket, collector, and adapters. |
| `trace-tui purge` | Apply retention. |

Press `?` in the TUI for the keymap.

## Configuration

`~/.config/trace-waterfall/config.toml`, or the file named by `TRACE_WATERFALL_CONFIG`:

```toml
db = "~/.local/share/trace-waterfall/traces.db"   # or TRACE_WATERFALL_DB
# socket = "..."                                  # or TRACE_WATERFALL_SOCKET; default is per-user, see docs/protocol.md
theme = "default"                                 # ascii, monochrome, ascii-monochrome

[retention]
metadata_days = 90
payload_days = 30

[capture]
payloads = false
projects = []

[[tail]]
adapter = "claude-code"
path = "~/.claude/projects"
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
| `trace-app` | GPUI desktop app (`trace-waterfall`). |
| `trace-adapter-sdk` | Harness integration: recorder, socket client, JSONL helpers. Depends only on `trace-core`. |
| `examples/simulated-live` | Deterministic agent run generator for demos, fixtures, and perf tests. |

Run `cargo test --workspace` to check everything.
