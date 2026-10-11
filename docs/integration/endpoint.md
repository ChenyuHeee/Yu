# Endpoint compatibility

The Messages API is a specification. Against any endpoint but Anthropic's it is
a hypothesis, and a server that accepts a field and ignores it will let a client
believe it has a feature it does not have.

Everything on this page was **measured**, not assumed. Re-measure against your
own endpoint before relying on any of it:

```sh
yu probe
```

!!! note "Reference measurement"

    Endpoint `https://api.deepseek.com/anthropic`, model `deepseek-flash[1m]`,
    measured 2026-10-10. Treat the values below as one endpoint's answers, not
    as properties of the API.

## Capability matrix

| Feature | Status |
|---|---|
| `POST /v1/messages` | works |
| SSE streaming | works — Anthropic event names verbatim, including `ping` |
| `thinking` | **always emitted**, even for `"reply ok"` at `max_tokens: 16` |
| `thinking: {type:"adaptive"}` | accepted |
| `thinking: {type:"adaptive", display:"summarized"}` | accepted |
| `thinking: {type:"enabled", budget_tokens:N}` | accepted — Anthropic rejects this |
| `thinking.display` effect | **unverified** — thinking text came back non-empty either way |
| `output_config.effort` | accepted, effect unverified |
| `output_config.format` | accepted, effect unverified |
| `cache_control` breakpoints | **inert** — caching is automatic |
| Mid-conversation `{"role":"system"}` | accepted |
| `stop_sequences` | accepted |
| `POST /v1/messages/count_tokens` | works |
| Custom tools with `input_schema` | works |
| `strict: true` on a tool | accepted |
| `tool_choice: {type:"any"}` | accepted — Anthropic rejects this on current models |
| Parallel `tool_use` (2 in one turn) | works |
| `tool_result.is_error` | works |
| `web_search_20260209` (server tool) | works — cost 67 372 input tokens in one call |
| `web_search_20250305` | accepted |
| `bash_20250124` | **rejected, 422 `unknown variant`** |
| `text_editor_20250728` | **rejected, 422 `unknown variant`** |
| `tool_search_tool_*` + `defer_loading` | **rejected, 422** |
| `mcp_servers` / `mcp_toolset` | **rejected, 422** |
| `GET /v1/models` | **404** |
| `context_management.edits` | accepted, **assumed inert** |
| `task_budget` | accepted, **assumed inert** |
| `fallbacks` | accepted, **assumed inert** |
| `anthropic-beta` header | **ignored** — accepted fields are accepted with or without it |
| Authentication | enforced. `x-api-key` works; a bad key returns 401 |

### What this costs

- **Schema-less server tools are unavailable.** `bash_20250124` and
  `text_editor_20250728` are both rejected, so every tool must declare an
  `input_schema`.
- **`tool_search` and `defer_loading` are unavailable**, so the startup cost of
  a large tool surface cannot be reduced this way.
- **Server-side compaction is accepted and does nothing.** Treat
  `context_management`, server compaction, and `task_budget` as inert. Anything
  that manages context has to be done client-side.
- **`web_search_20260209` is the one working server tool**, and it produces
  exactly the unbounded result shape the reduction table exists to handle.

## Quirks

Behavioural divergences from Anthropic. Each one is something a client must
special-case.

### Quirk 1 — thinking blocks must be replayed {: #quirk-1 }

Reconstructing an assistant turn without its `thinking` block fails:

```
400 invalid_request_error
The `content[].thinking` in the thinking mode must be passed back to the API.
```

The message does not name the offending message. The precise rule, established
over 16 probe cases:

| Case | Result |
|---|---|
| Fabricated `tool_use` id, no `thinking` | **400** — for every id format tried |
| Fabricated id, **any** `thinking` block | **200** — the `signature` is **not validated** |
| An id the endpoint itself issued, no `thinking` | **200** |

The endpoint keeps its issued tool calls server-side and only demands the
`thinking` block when it cannot recognise the id.

**Consequences.**

1. **Replay `thinking` blocks byte-exact.** It is free, it satisfies the strict
   case, and it is what Anthropic's preserved thinking requires anyway.
   `yu-wire` enforces this when assembling history rather than letting a doomed
   request go out and fail with a confusing error.
2. **Fork and replay are feasible.** Because `signature` is unvalidated, a
   forked branch may carry synthesised thinking blocks. Rewriting history does
   not fail the way Anthropic's preserved-thinking check would.

### Quirk 2 — identifier shapes differ {: #quirk-2 }

| | Anthropic | This endpoint |
|---|---|---|
| Message id | `msg_01…` | UUID, e.g. `4aeac4b3-5f44-4b94-bef3-cb721ea7bef0` |
| `tool_use` id | `toolu_01…` | `call_00_<base62>`, e.g. `call_00_4Ykgo3t3AxTqrrfx12mk4590` |

Nothing may parse, validate, or synthesise an id. Ids are opaque bytes and are
replayed verbatim. They are also the key the endpoint indexes its issued-call
record by — see Quirk 1 — so rewriting one silently changes behaviour.

### Quirk 3 — prefix caching is automatic, `cache_control` is inert {: #quirk-3 }

`cache_creation_input_tokens` is **always 0**. Only `cache_read_input_tokens` is
populated. Two identical requests with the same long system prefix: the first
reports `cache_read=0`, the second `cache_read=2299`. Adding an explicit
`cache_control` breakpoint changes nothing.

**Consequence: byte-stability of the rendered prefix is the entire caching
mechanism.** There is no breakpoint to place and no way to steer the cache.
`cache_control` is still emitted — it is correct for Anthropic and costs nothing
here — but no strategy may depend on it.

### Quirk 4 — `input_tokens` excludes thinking blocks {: #quirk-4 }

An assistant turn carrying 86 characters of thinking and the same turn with the
thinking stripped both reported `input_tokens: 256`. Token accounting cannot be
used to detect whether thinking survived into a request.

### Quirk 5 — 422 and 400 are different failure classes {: #quirk-5 }

A malformed *schema* returns `422`, and the message enumerates the valid
variants:

```
422 Failed to deserialize the JSON body into the target type:
    tools[0]: unknown variant `bash_20250124`,
    expected `web_search_20250305` or `web_search_20260209` at line 1 column 94
```

This is not retryable and not a normal `invalid_request_error` — it means the
client sent a field the endpoint does not model. `yu-wire` classifies it
separately so it is never retried and never mistaken for a prompt bug.

It is also the cheapest possible capability probe: send a deliberately unknown
variant and read the `expected` list back.

### Quirk 6 — 401 leaks the last 4 characters of the key {: #quirk-6 }

```
401 authentication_error: Authentication Fails, Your api key: ****0000 is invalid
```

Error bodies must be handled carefully; this one embeds key material. See
[Credentials](../configuration.md#credentials).

## Streaming notes

- **`ping` events** are emitted between blocks and must be tolerated and
  skipped.
- **`signature` arrives as a `signature_delta` delta type**, carried inside an
  ordinary `content_block_delta` event — it is not an event name of its own. It
  arrives after the `thinking_delta`s and before `content_block_stop`.
- **`content_block_start` for a thinking block carries `"signature": ""`**, so
  an accumulator must treat empty-signature-then-delta as the normal path rather
  than as a malformed block.
- **`web_search` results interleave**: `thinking` → `text` → `server_tool_use`
  → `web_search_tool_result` → `thinking` → … Many blocks per turn. This is the
  unbounded shape the reduction table handles.

## `stop_reason`

Observed: `end_turn`, `tool_use`, `max_tokens`. `refusal` and `pause_turn` never
appeared across roughly 40 requests, and are still handled — an unhandled
variant must not be silently treated as success.

## Running against Anthropic

Set the profile explicitly if you do not probe:

```sh
export YU_QUIRKS=anthropic
```

On Anthropic's endpoint the schema-less server tools are available,
`tool_search` and `defer_loading` work, and `cache_control` breakpoints are
honoured rather than inert. The probe is still the authority — measure rather
than assume.
