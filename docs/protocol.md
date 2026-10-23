# Collector socket protocol

Protocol version 1. Harnesses send canonical events to the collector over a local Unix domain socket. `trace-adapter-sdk::Client` implements the client side.

## Socket path

The client and the collector resolve the path in this order:

1. `$AGENTS_TRACE_WATERFALL_SOCKET`
2. `$XDG_RUNTIME_DIR/agents-trace-waterfall.sock`
3. `~/.local/state/agents-trace-waterfall/collector.sock`

Both defaults are private to your user. Do not point the socket at a shared directory such as `/tmp`, where another local user can create the path first and receive your events.

`trace_adapter_sdk::default_socket_path()` returns the resolved path.

## Framing

Every message is one JSON object followed by `\n` (NDJSON). A line is at most 1 MiB (1048576 bytes), newline included. The `type` field names the message.

## Messages

| Direction | Message | Fields |
| --- | --- | --- |
| client → server | `hello` | `protocol`: `1`. `source_id`: string. |
| server → client | `welcome` | `protocol`: `1`. `cursor`: integer or `null`. |
| client → server | `events` | `events`: array of canonical events. |
| server → client | `ack` | `cursor`: integer or `null`. |
| server → client | `error` | `message`: string. |

Example session:

```
→ {"type":"hello","protocol":1,"source_id":"harness-01J..."}
← {"type":"welcome","protocol":1,"cursor":41}
→ {"type":"events","events":[{"schema_version":1,"source_id":"harness-01J...","source_seq":42,...}]}
← {"type":"ack","cursor":42}
```

A canonical event is the JSON form of `trace_core::CanonicalEvent`: one flat object with the envelope fields, `event_type`, and `body`. `examples/sample-trace.jsonl` holds one event per line.

## Cursor

The cursor is the highest `source_seq` for the source such that every sequence number from the lowest one the collector has persisted for that source up to the cursor is persisted. It is `null` before the collector has persisted any event from the source.

The count starts at the lowest persisted sequence number, not at 1. A source whose first delivered event is 3, because 1 and 2 were dropped, gets cursor 3 once 3 is persisted. If 1 and 2 arrive later, the cursor counts from 1 again.

`source_seq` starts at 1 and increases by 1 for each event from a source. A `source_id` names one sequence stream. A process that restarts its numbering must use a new `source_id`, or the collector treats the new events as already persisted.

## Handshake

1. The client sends `hello`.
2. The server replies `welcome` with the cursor for that source.
3. The client discards buffered events with `source_seq` at or below the cursor, then sends the rest.

## Delivery

- The server sends `ack` only after the frame's events are committed to SQLite with `synchronous=FULL`. An acked event survives a collector crash and a power loss. On macOS, `fsync` does not flush the drive's write cache, so power-loss durability there depends on the drive.
- The server validates every event in a frame before it writes any of them. One invalid event makes the server reply `error` for the whole frame and close the connection.
- On `ack`, the client removes the frame's events from its buffer.
- On `error`, or if the connection closes before `ack`, the client keeps the events and resends them after it reconnects.
- The server deduplicates on `(source_id, event_id)`. A resent event that the server already persisted has no effect.
- The client has at most one message outstanding. It sends the next `events` frame only after the reply to the previous one.

## Errors and limits

- If `hello` names a protocol other than `1`, the server sends `error` and closes the connection.
- If a line exceeds 1 MiB, the server sends `error` and closes the connection.
- An event is valid when `schema_version` is `1`; `source_id`, `event_id` and `session_id` are non-empty; `source_seq` is at least 1; span events (`span.*`, `usage.reported`, `artifact.recorded`) carry a `span_id`; and `occurred_at_utc_ns` and `observed_at_utc_ns` lie from 1970-01-01 up to but not including 2200-01-01. `trace_core::CanonicalEvent::validate` implements these checks for the client, the server and `trace-tui import --adapter canonical`.
- `Client::send` returns an error and buffers none of the batch if any event is invalid, comes from another `source_id`, or would make a frame exceed 1 MiB. Because the client rejects such events before sending, the collector never sees them and the source keeps flowing.

## Reconnect and loss

`Client` reconnects with exponential backoff from 50 ms to 5 s. The backoff resets after a successful `welcome`.

Unacked events wait in a bounded buffer. When the buffer is full, the client drops the oldest event and increments `Client::dropped()`. Delivery is lossy once that count is non-zero. A dropped event leaves a gap in `source_seq`, so the cursor stops below the gap. Later frames are still acked and persisted.

`Client::send` does not wait for acks. It buffers, writes a frame when none is outstanding, and reads any reply that is already there, waiting at most a few milliseconds. It also never waits for a reconnect backoff. A large frame can block on the socket write for up to 5 s if the collector stops reading.

`Client::flush(timeout)` waits for acks. It returns the collector cursor once the buffer is empty. It returns an error that states the unacked and dropped counts if the timeout passes first.
