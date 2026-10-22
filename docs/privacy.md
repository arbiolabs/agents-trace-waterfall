# Privacy

Agent traces often contain secrets and proprietary source code. trace-waterfall therefore records metadata by default and never collects raw content unless you opt in.

## What is collected by default

- Session metadata: project, working directory, adapter, harness, host.
- Span metadata: name, kind, status, start and end times, model, provider, tool name.
- Usage: token counts and cost, with provenance (actual, estimate, or unknown).
- Artifact records: file path, hash, and change summary.

## What is not collected by default

- Raw prompts and model outputs.
- Environment variables.
- Tool stdout and stderr.
- Credentials of any kind.

The inspector shows `not captured` for any field that was not recorded. It never synthesizes missing data.

## Opting in to payload capture

Adapters may send short `input_preview` and `output_preview` strings and full payloads by reference (`payload_ref`). Enable full capture per adapter or per project in the config file:

```toml
[capture]
payloads = false          # default
projects = ["my-project"] # opt in for these projects only
```

## Safeguards on disk

- Before any write, the store redacts known secret shapes: `sk-…` and `sk-ant-…` API keys, AWS `AKIA…` keys, GitHub tokens, Slack `xox…` tokens, `Bearer` tokens, and PEM private key blocks. Each is replaced with `[REDACTED]`.
- The database file and its WAL files are created with mode `0600`.
- Retention is configured separately for metadata and payloads. `trace-tui purge` applies it.

## Export

`trace-tui export` writes the normalized journal. Payload blobs are excluded unless you pass `--include-payloads`.
