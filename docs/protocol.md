# Collector socket protocol

Protocol version 1. Harnesses send canonical events to the collector over a local Unix domain socket. `trace-adapter-sdk::Client` implements the client side.

## Socket path

The client and the collector resolve the path in this order:

1. `$TRACE_WATERFALL_SOCKET`
2. `$XDG_RUNTIME_DIR/trace-waterfall.sock`
3. `~/.local/state/trace-waterfall/collector.sock`

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

The cursor is the highest `source_seq` for the source such that every sequence number from 1 to the cursor is persisted. It is `null` when sequence 1 is not persisted.

`source_seq` starts at 1 and increases by 1 for each event from a source. A `source_id` names one sequence stream. A process that restarts its numbering must use a new `source_id`, or the collector treats the new events as already persisted.

## Handshake

1. The client sends `hello`.
2. The server replies `welcome` with the cursor for that source.
3. The client discards buffered events with `source_seq` at or below the cursor, then sends the rest.

## Delivery

- The server sends `ack` only after it writes every event in the frame to durable storage.
- On `ack`, the client removes the frame's events from its buffer.
- On `error`, or if the connection closes before `ack`, the client keeps the events and resends them after it reconnects.
- The server deduplicates on `(source_id, event_id)`. A resent event that the server already persisted has no effect.
- The client sends one `events` frame at a time and waits for its reply.

## Errors and limits

- If `hello` names a protocol other than `1`, the server sends `error` and closes the connection.
- If a line exceeds 1 MiB, the server sends `error` and closes the connection.
- `Client::send` rejects an event whose serialized frame would exceed 1 MiB, and an event whose `source_id` differs from the client's.

## Reconnect and loss

`Client` reconnects with exponential backoff from 50 ms to 5 s. The backoff resets after a successful `welcome`.

Unacked events wait in a bounded buffer. When the buffer is full, the client drops the oldest event and increments `Client::dropped()`. Delivery is lossy once that count is non-zero. A dropped event leaves a gap in `source_seq`, so the cursor stops below the gap. Later frames are still acked and persisted.

`Client::flush(timeout)` returns the collector cursor once the buffer is empty. It returns an error that states the unacked and dropped counts if the timeout passes first.
