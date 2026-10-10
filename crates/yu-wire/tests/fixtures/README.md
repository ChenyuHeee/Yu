# Wire fixtures

Real captures from `https://api.deepseek.com/anthropic` (model `deepseek-flash[1m]`),
taken 2026-10-10 by the probe scripts in `/.probe/`. These are recordings, not
hand-written examples — hand-written fixtures only test what the author already
believed, which is exactly the belief under test here.

Committed deliberately small (36 KB). The `websearch` capture was 317 KB and is
not included; `docs/WIRE-FACTS.md` records what it showed.

| file | what it is |
|---|---|
| `response_basic_thinking_text.json` | Simplest possible turn, and it still opens with a `thinking` block |
| `response_tool_use.json` | `thinking` + `tool_use`; the id is `call_00_…`, not Anthropic's `toolu_…` |
| `response_parallel_tool_use.json` | Two `tool_use` blocks in one turn |
| `stream_thinking_then_text.sse` | Raw SSE: `message_start` → thinking deltas → `signature_delta` → text → `message_stop` |
| `error_422_unknown_variant.json` | A 422 whose message enumerates the accepted tool variants |
| `error_401_auth.json` | A 401 that embeds the last four characters of the API key |
| `request_history_with_thinking.json` | A history carrying its thinking block |
| `request_history_stripped.json` | The same history with thinking removed |
| `request_history_fabricated_id.json` | A fabricated tool-call id with no thinking |

The last three are request bodies, kept because they are the evidence behind
Quirk 1 and because [`Quirks::check_history`] is validated against the same bytes
the endpoint was given.

Note on `request_history_stripped.json`: the endpoint **accepted** this one with a
200, because its `tool_use.id` was one the endpoint itself had issued and it could
recover the turn server-side. It is kept precisely because it is the awkward case —
see the test that documents why this client rejects it anyway.
