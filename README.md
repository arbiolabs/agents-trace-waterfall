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
socket = "/tmp/trace-waterfall.sock"              # or TRACE_WATERFALL_SOCKET
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
| `trace-tui` | CLI and Ratatui app. |
| `trace-adapter-sdk` | Harness integration: recorder, socket client, JSONL helpers. Depends only on `trace-core`. |
| `examples/simulated-live` | Deterministic agent run generator for demos, fixtures, and perf tests. |

Run `cargo test --workspace` to check everything.
