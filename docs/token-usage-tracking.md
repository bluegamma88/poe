# Token & Cost Tracking

This document explains how per-request token and cost accounting flows from the
model provider into the TUI footer, where running session totals are displayed.

## Problem

A session makes many model requests — one per tool round — and the user has no
visibility into how many tokens are being spent or what it costs. We want a
live, cumulative readout in the footer: total cost, input tokens, cached tokens,
and output tokens.

## Source of the data

OpenRouter includes a usage block in the **final SSE chunk** of every streaming
response (the legacy `usage: { include: true }` opt-in is deprecated and now a
no-op — usage is always present). The block looks like:

```json
{
  "prompt_tokens": 194,
  "prompt_tokens_details": { "cached_tokens": 100, "cache_write_tokens": 50 },
  "completion_tokens": 2,
  "cost": 0.95,
  "total_tokens": 196
}
```

- `prompt_tokens` — total input, **including** the cached portion.
- `prompt_tokens_details.cached_tokens` — the cached subset of input.
- `completion_tokens` — output.
- `cost` — request cost in OpenRouter credits, where 1 credit = $1 USD.

Because usage arrives once per request and the agent loop issues one request per
tool round, totals are accumulated by the frontend rather than in core.

## The pipeline

```
OpenRouter final chunk
  → Usage (deserialize)                         agent-core
  → ChatChunkEvent::Usage(TokenUsage)           agent-core
  → ModelEvent::Usage(TokenUsage)               agent-core (stream)
  → Event::Usage { usage }                      agent-core (session loop)
  → AppState.session_usage.add(&usage)          agent-tui (accumulate)
  → render_footer / format_usage                agent-tui (display)
```

### Shared type (`crates/agent-protocol/src/lib.rs`)

`TokenUsage` is the wire/IPC representation carried by `Event::Usage`:

| Field | Meaning |
| --- | --- |
| `input_tokens` | total prompt tokens (incl. cached) |
| `cached_tokens` | cached subset of input — cache *reads* |
| `cache_write_tokens` | input tokens written to the cache — cache *writes* |
| `output_tokens` | completion tokens |
| `cost_usd` | request cost in USD |

Reads and writes are distinct: a prefix is *written* the first time it is seen
and *read* on later requests that reuse it. A session showing writes but no
reads is populating the cache without reusing it (first request, or the upstream
provider changing between requests).

`TokenUsage::add` folds one report into a running total — token counts use
saturating addition, cost uses `+=`. Because `cost_usd` is an `f64`, `Event`,
`ModelEvent`, and `TokenUsage` are `PartialEq` but **not** `Eq`.

### Capture (`crates/agent-core/src/lib.rs`)

`ChatCompletionChunk` gains an optional `usage` field. `parse_chat_chunk` checks
it **before** iterating `choices` (the usage chunk normally has none) and returns
`ChatChunkEvent::Usage`. The SSE loop forwards that as `ModelEvent::Usage`, and
the `AgentSession` run loop maps it to the protocol `Event::Usage`. Usage is not
part of the persisted transcript, so the transcript builder ignores it.

### Accumulate & display (`crates/agent-tui/src/lib.rs`)

`AppState` holds a `session_usage: TokenUsage`. The `Event::Usage` arm in
`apply_event` calls `session_usage.add(&usage)`. `render_footer` appends
`format_usage`, which renders:

```
$0.0142 · ↑12.4k (8.1k cached, 2.0k write) ↓1.3k
```

`humanize_tokens` formats counts compactly (`940`, `12.4k`, `3.0M`).

## Behavior notes

- Totals are **cumulative for the session** and persist across turns; they reset
  only when the process restarts. They are not saved/restored with a transcript.
- Other frontends are unaffected: `agent-exec`'s human and JSON renderers fall
  through their `_ => {}` arms, and the `--json` event stream serializes
  `Event::Usage` for free.
- Providers that omit fields are tolerated — every `usage` field defaults, so a
  partial block yields zeros rather than a parse error.

## File map

| File | Role |
| --- | --- |
| `crates/agent-protocol/src/lib.rs` | `TokenUsage` type, `TokenUsage::add`, `Event::Usage` variant |
| `crates/agent-core/src/lib.rs` | `Usage` deserialize, `ChatChunkEvent::Usage`, `ModelEvent::Usage`, mapping to `Event::Usage` |
| `crates/agent-tui/src/lib.rs` | `AppState.session_usage` accumulation, `render_footer` / `format_usage` / `humanize_tokens` |
