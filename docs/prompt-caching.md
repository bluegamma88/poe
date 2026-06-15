# Prompt Caching

This document explains how the agent enables Anthropic prompt caching through
OpenRouter to cut token cost and latency on multi-turn sessions.

## Problem

Every model turn resends the full conversation transcript: the system prompt,
the tool definitions, and every prior user / assistant / tool message. In an
agent loop that runs many tool rounds per task, the stable prefix (system prompt
+ tools + earlier turns) is re-tokenized and re-billed on every request even
though it never changes.

Anthropic supports **prompt caching**: a cached prefix is billed at a large
discount on subsequent requests and is faster to process. The cache is
prefix-based and lives for 5 minutes (refreshed on each hit), which fits an
agent loop where requests fire seconds apart.

We talk to Anthropic models through OpenRouter's OpenAI-compatible
chat-completions API, so we use OpenRouter's pass-through of Anthropic's
explicit-caching syntax rather than the native Anthropic SDK.

## How Anthropic caching works

Anthropic builds the cacheable prefix in a fixed hierarchy:

```
tools  →  system  →  messages
```

A **cache breakpoint** (`cache_control: { "type": "ephemeral" }`) caches
everything up to and including the marked block, plus everything earlier in the
hierarchy. Key consequences:

- A breakpoint on the **system** message caches the `[tools + system]` segment —
  the tools array is included for free because it precedes `system`.
- Up to **four** breakpoints are allowed.
- A segment must meet a per-model minimum to be cached (Claude Sonnet: 1,024
  tokens; Claude Haiku: 4,096). The tool schemas are what push our
  `[tools + system]` prefix over that threshold — the system prompt alone is far
  too small.

## Wire format

OpenRouter only accepts `cache_control` when a message's `content` is an
**array of content parts**, not a bare string. A plain message:

```json
{ "role": "system", "content": "You are a coding agent." }
```

becomes, once a breakpoint is attached:

```json
{
  "role": "system",
  "content": [
    {
      "type": "text",
      "text": "You are a coding agent.",
      "cache_control": { "type": "ephemeral" }
    }
  ]
}
```

Non-Anthropic providers ignore the marker, so the format stays compatible.

## Implementation (`crates/agent-core/src/lib.rs`)

### Content model

`ChatMessage::content` is a `MessageContent` enum that serializes either way:

| Variant | Serializes as | Used for |
| --- | --- | --- |
| `Text(String)` | bare JSON string | normal messages (unchanged wire format) |
| `Parts(Vec<TextPart>)` | array of content parts | any message carrying a cache breakpoint |

`MessageContent::mark_cache_breakpoint` promotes a `Text` to `Parts` and tags
the final block with `EPHEMERAL_CACHE` (`cache_control: { type: "ephemeral" }`).
`MessageContent::as_text` flattens parts back to a plain string so
`transcript()` output is unaffected.

### Where breakpoints are placed

In `OpenRouterClient::stream_current_messages`, after cloning the transcript for
the request, two breakpoints are applied — gated to the Anthropic family
(`model.starts_with("anthropic/")`) so other providers' wire format is left
untouched:

1. **System prompt** — the first message. Stable for the whole session, and
   reusable across conversations since `[tools + system]` is the canonical
   prefix. This is also what makes tools cacheable.
2. **Latest message** — the last message. Advances every turn, so the growing
   transcript prefix keeps getting cached as the conversation extends.

That is two of the four allowed breakpoints.

### Why tools need no separate breakpoint

Because the prefix order is `tools → system → messages`, the system-prompt
breakpoint already caches the tools array as part of the `[tools + system]`
segment. A dedicated breakpoint on the last tool definition would only help if
the tools stayed stable while the system prompt changed independently — which
never happens here: `SYSTEM_PROMPT` is a `const`, and the per-session cwd /
file-tree context lives in a separate user message, not the system message.

## Verifying it works

- **Unit tests** in `crates/agent-core/src/lib.rs` assert the wire format: plain
  strings stay bare, a breakpoint produces the exact `ephemeral` parts JSON,
  empty content (an assistant tool-call turn) is a no-op, and transcript
  flattening is lossless.
- **At runtime**, the response's `usage.prompt_tokens_details` reports
  `cached_tokens` and `cache_write_tokens`. A non-zero `cached_tokens` on the
  second and later requests of a session confirms cache hits.
- **In the TUI footer**, session usage is shown live as
  `$<cost> · ↑<input> (<cached> cached, <write> write) ↓<output>`. A growing
  `cached` (read) figure across turns is the at-a-glance signal that caching is
  working; `write` rising while `cached` stays at 0 means the cache is being
  populated but never reused (e.g. first request, or upstream provider routing
  changing between requests). See
  [token-usage-tracking.md](token-usage-tracking.md) for how that is plumbed.

## Caveats

- **Stable JSON key ordering is required.** Anthropic invalidates the cache if
  serialized key order drifts between requests. We use `serde` (deterministic
  field order) and static `*_tool_schema()` definitions, so ordering is stable —
  but don't switch to a serializer that randomizes key order.
- **5-minute TTL.** Long idle gaps between turns let the cache expire; the next
  request pays a normal (uncached) write. A 1-hour TTL (`"ttl": "1h"`) is
  available at a higher write cost if sessions have long pauses — not currently
  enabled.
- **Minimum segment size.** If the tool set ever shrinks dramatically, the
  `[tools + system]` prefix could fall below the model's caching minimum and
  silently stop caching.

## File map

| File | Role |
| --- | --- |
| `crates/agent-core/src/lib.rs` | `MessageContent` / `TextPart` / `CacheControl` types, `mark_cache_breakpoint`, and breakpoint placement in `stream_current_messages` |
