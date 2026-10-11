# Measured wire facts

Everything here was **measured**, not assumed. Probes live in `.probe/`; raw captures in
`crates/yu-wire/tests/fixtures/`. Re-run with `yu probe`.

Endpoint: `https://api.deepseek.com/anthropic` — model `deepseek-flash[1m]`.
Measured 2026-10-10.

DESIGN.md's "API 事实" section pins the *Anthropic* surface. Against this endpoint that
surface is a hypothesis. This file records which parts survive.

---

## Capability matrix

| Feature | Status | Evidence |
|---|---|---|
| `POST /v1/messages` | works | probe 1 `basic` |
| SSE streaming | works | probe 1 `stream` — Anthropic event names verbatim, incl. `ping`; `signature_delta` arrives as a delta type |
| `thinking` | **always emitted** | probe 1 `basic` — even `"reply ok"` at `max_tokens:16` returns a `thinking` block |
| `thinking: {type:"adaptive"}` | accepted | probe 1 `thinking_adaptive` |
| `thinking: {type:"adaptive", display:"summarized"}` | accepted | probe 1 `thinking_summarized` |
| `thinking: {type:"enabled", budget_tokens:N}` | **accepted** | probe 1 `thinking_budget` — Anthropic 400s on this; the shim allows it |
| `thinking.display` effect | **unverified** | thinking text came back non-empty in both cases |
| `output_config.effort` | accepted, effect unverified | probe 1 `effort`, `effort_xhigh` |
| `output_config.format` (structured outputs) | accepted, effect unverified | probe 1 `beta_output_format` |
| `cache_control` breakpoints | **inert** — caching is automatic | probe 2 `cache_probe_1/2`; see Quirk 3 |
| mid-conversation `{"role":"system"}` | accepted | probe 1 `sys_midconv` |
| `stop_sequences` | accepted | probe 1 `stop_sequences` |
| `POST /v1/messages/count_tokens` | works | probe 1 `count_tokens` |
| custom tools (`input_schema`) | works | probe 1 `tool_native` |
| `strict: true` on a tool | accepted | probe 1 `tool_strict` |
| `tool_choice: {type:"any"}` | **accepted** | probe 1 `tool_choice_any` — Anthropic 400s this on Opus 5.5 / Sonnet 5.5 |
| parallel `tool_use` (2 in one turn) | works | probe 2 `parallel_tools` |
| `tool_result.is_error` | works | probe 3 `F_real_iserror` |
| `web_search_20260209` (server tool) | works | probe 2 `websearch` — cost 67 372 input tokens |
| `web_search_20250305` | accepted (listed as a valid variant) | probe 1 `tool_bash` 422 message |
| `bash_20250124` | **REJECTED 422** | probe 1 `tool_bash` — `unknown variant` |
| `text_editor_20250728` | **REJECTED 422** | probe 1 `tool_editor` — `unknown variant` |
| `tool_search_tool_*` + `defer_loading` | **REJECTED 422** | probe 1 `tool_search` — `unknown variant` |
| `mcp_servers` / `mcp_toolset` | **REJECTED 422** | probe 1 `beta_mcp` — `unknown variant` |
| `GET /v1/models` | **404** | probe 1 `models` |
| `context_management.edits` | accepted, **assumed inert** | probe 1 `beta_ctxmgmt` / `beta_compact` |
| `task_budget` | accepted, **assumed inert** | probe 1 `beta_task_budget` |
| `fallbacks` | accepted, **assumed inert** | probe 1 `beta_fallbacks` |
| `anthropic-beta` header | **ignored** | the header is never required; accepted fields are accepted with or without it |
| auth | **enforced**, `x-api-key` works | probe 2 `auth_garbage` → 401 |

### Consequences for DESIGN

- **"白拿训练过的工具纪律" is unavailable.** `bash_20250124` and `text_editor_20250728` are
  both rejected. There is no schema-less fallback here; every tool is `input_schema`-defined.
- **`tool_search` / `defer_loading` is unavailable** — DESIGN risk 5 ("能否砍掉协调者启动底价")
  cannot be measured on this endpoint.
- **Server-side compaction / context_management / task_budget are accepted but do nothing.**
  Treat them as inert. Compaction and context management must be built client-side.
  This is not a downgrade: DESIGN's whole thesis is that client-side *isolation* beats
  server-side *compression* anyway.
- **`web_search_20260209` is the one server tool that works**, and it is the ideal
  worked example for the M1 reduction table — it produces exactly the unbounded
  shape (`web_search_tool_result`) the design describes, and it cost 67 k input tokens
  in a single call.

---

## Quirks

The quirk layer is load-bearing, exactly as DESIGN predicts. These are behavioural
divergences from Anthropic that any client must special-case.

### Quirk 1 — an assistant turn with `tool_use` must also carry a `thinking` block

The hard one. Reconstructing history without a `thinking` block returns:

```
400 invalid_request_error
The `content[].thinking` in the thinking mode must be passed back to the API.
```

The message is misleading and does not name the offending message.

The **precise** rule, established by probes 3 and 4 over 16 cases:

- Fabricated `tool_use` id + no `thinking` → **400**, for *every* id format tried
  (`call_00_*`, `call_99_*`, `call_0_*`, `call00*`, `toolu_*`, short ids). Format is irrelevant.
- Fabricated id + **any** `thinking` block → **200**. The `signature` is **not validated**;
  an all-zeros UUID was accepted, as was the string `"not-a-real-signature"`.
- A `tool_use` id **the endpoint itself issued** + no `thinking` → **200**.

So the endpoint keeps its issued tool calls server-side, and only demands the `thinking`
block when it cannot recognise the id. Two consequences:

1. **Always replay `thinking` blocks byte-exact.** It is free, it satisfies the strict
   case, and it is also what Anthropic's preserved thinking requires on Opus 5.x.
   `yu-wire` enforces this when assembling history rather than letting a doomed request
   go out and fail with a confusing error.
2. **M3 fork/replay is feasible.** Because `signature` is unvalidated, a forked branch may
   carry synthesised thinking blocks. Rewriting history does not 400 the way Anthropic's
   preserved-thinking check would.

### Quirk 2 — identifier shapes differ from Anthropic

| | Anthropic | This endpoint |
|---|---|---|
| message id | `msg_01...` | UUID, e.g. `4aeac4b3-5f44-4b94-bef3-cb721ea7bef0` |
| `tool_use` id | `toolu_01...` | `call_00_<base62>`, e.g. `call_00_4Ykgo3t3AxTqrrfx12mk4590` |

Nothing in `yu-wire` may parse, validate, or synthesise an id. Ids are opaque bytes and are
replayed verbatim. (Note these are *also* the ids the endpoint indexes its issued-call
record by — see Quirk 1 — so rewriting one silently changes behaviour.)

### Quirk 3 — prefix caching is automatic; `cache_control` is inert

`cache_creation_input_tokens` is **always 0**. Only `cache_read_input_tokens` is populated.
Two identical requests with the same long system prefix: first `cr=0`, second `cr=2299`.
An explicit `cache_control` breakpoint changes nothing.

The probe sends both: the same prefix twice with a breakpoint, and twice without, and
compares. It used to send only the unadorned pair and read the answer off that — which
could not distinguish "breakpoints do nothing here" from "this probe never placed one",
and on a breakpoint-honouring endpoint would have reported `No` and overwritten the
correct default in `Quirks::anthropic()`. The `inert` above is now an observation rather
than an inference; `capability::tests` guards both the verdict and the fact that a
breakpoint actually went out on the wire.

Consequence: DESIGN's "字节稳定是铁律" is **more** important here, not less. There is no
breakpoint to place and no way to steer the cache — stability of the byte prefix is the
entire mechanism. `cache_control` is still emitted (it is correct for Anthropic and costs
nothing here), but no strategy may depend on it.

### Quirk 4 — `input_tokens` excludes `thinking` blocks

An assistant turn carrying 86 characters of thinking and the same turn with thinking
stripped both reported `input_tokens: 256`. Token accounting must not be used to detect
whether thinking survived into a request.

### Quirk 5 — 422 vs 400 are different failure classes

Malformed *schema* returns `422` and the message **enumerates the valid variants**:

```
422 Failed to deserialize the JSON body into the target type:
    tools[0]: unknown variant `bash_20250124`,
    expected `web_search_20250305` or `web_search_20260209` at line 1 column 94
```

This is not retryable and not a normal `invalid_request_error` — it means the client sent
a field the endpoint does not model. `yu-wire` classifies it separately so it is never
retried and never mistaken for a prompt bug. It is also the cheapest possible capability
probe: send a deliberately unknown variant and read the `expected` list.

### Quirk 6 — 401 leaks the last 4 characters of the key

```
401 authentication_error: Authentication Fails, Your api key: ****0000 is invalid
```

Error bodies must be logged carefully; this one embeds key material.

---

## Endpoint behaviour worth knowing

- `stop_reason` observed: `end_turn`, `tool_use`, `max_tokens`. `refusal` and `pause_turn`
  never appeared in ~40 requests. They are still handled — DESIGN requires exhausting
  `stop_reason`, and an unhandled variant must not be silently treated as success.
- Streaming emits `ping` events between blocks; a parser must tolerate and skip them.
- `signature` arrives as a **`signature_delta` delta type**, carried inside an ordinary
  `content_block_delta` event — it is not an event name of its own. It comes *after* the
  `thinking_delta`s and before `content_block_stop`. `content_block_start` for a thinking
  block carries `"signature": ""`, so an accumulator must treat empty-signature-then-delta
  as the normal path rather than as a malformed block.
- `web_search` results interleave: `thinking` → `text` → `server_tool_use` →
  `web_search_tool_result` → `thinking` → … Many blocks per turn. This is the unbounded
  shape the reduction table has to handle.
