# Record format

A session's record is an append-only JSONL file: one event per line, typed,
indexed by sequence number, and addressable. It is the basis for replay as well
as the memory of the run — one mechanism, two uses.

## Two layouts

`yu daemon` and the other commands write different layouts under the same `--dir`.

**CLI.** One journal per session, one shared blob store.

```
.yu/
  delivery-1.jsonl
  gen2.jsonl
  blobs/
    3f/
      2b9c…        (62 more hex characters)
```

**Daemon.** Each session gets its own directory.

```
.yu/
  daemon.sock
  s1759000000000-1/
    info.json
    s1759000000000-1.jsonl
    blobs/
      3f/
        2b9c…
```

The consequence to remember: [`yu metrics`](../commands/metrics.md) reads only
top-level `*.jsonl` files, so daemon-owned sessions are not summarised by it.

## File naming

A session id becomes a file name by this rule:

- ASCII letters, digits, `-`, `_`, and `.` are kept. Every other character
  becomes `_`.
- If the result is non-empty, unchanged, and is not exactly `.` or `..`, the
  file is `<name>.jsonl`.
- Otherwise the file is `<stem>-<12 hex chars>.jsonl`, where the stem is the
  sanitised name or the literal `session` when sanitisation produced nothing,
  and the 12 characters are the head of the SHA-256 of the original name.

So `delivery-1` becomes `delivery-1.jsonl`, while `a/b` becomes
`a_b-<12 hex>.jsonl` — which cannot collide with a session genuinely named
`a_b`. A name like `../../etc/passwd` stays a single path component.

## Blobs

Large payloads are stored once, outside the journal, and referenced by digest.

| Property | Value |
|---|---|
| Digest | SHA-256, lowercase hex, 64 characters |
| Path | `blobs/<first 2 hex>/<remaining 62 hex>` |
| Write | Temp file, then rename. A crash never leaves a half-written blob |
| Idempotent | Writing the same bytes twice stores one file |

The fan-out directory is what keeps any one directory from holding every blob in
a long run.

## Event envelope

One event per line. Field names are literal.

```json
{
  "seq": 12,
  "ts_ms": 1759000001234,
  "session": "delivery-1",
  "agent": "executor#1",
  "body": { "event": "note", "text": "…" }
}
```

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | Position in this branch. Starts at 0 and is contiguous |
| `ts_ms` | integer | Wall-clock milliseconds since the epoch |
| `session` | string | The session id |
| `agent` | string | Which agent produced the event |
| `body` | object | Tagged by an `event` key |

`ts_ms` is wall-clock rather than monotonic, because records outlive processes
and must stay comparable across restarts.

## Event bodies

| `event` | Fields |
|---|---|
| `session_started` | `charter`, `model`, `quirks` |
| `user_message` | `text` |
| `model_request` | `model`, `messages`, `digest`, `est_input_tokens` |
| `model_response` | `id`, `stop_reason`, `usage`, `blocks`, `thinking`, `text_blob` |
| `tool_call` | `tool_use_id`, `name`, `input` |
| `tool_result` | `tool_use_id`, `name`, `kind`, `bytes`, `reproducible`, `addressable`, `inline`, `blob`, `reduction` |
| `injected` | `label`, `blob` |
| `note` | `text` |
| `effect` | `key`, `description`, `state`, `tool_use_id`, `idempotent` |
| `forked` | `parent`, `parent_at_seq` |
| `delegated` | `child`, `depth`, `task` |
| `session_seeded` | `blob` |
| `round_started` | `round`, `ceiling` |

Four engineering rules are enforced by this shape:

1. **A result is not stored twice.** A `tool_result` with both `inline` and
   `blob` is rejected at append time. It has one home or the other.
2. **The envelope is not repeated per content block.** One event is one line;
   blocks are references inside it.
3. **Re-sent material is deduplicated by reference.** Anything injected
   repeatedly — a task reminder, a charter — is stored once as a blob and
   referenced by digest. See `injected`, `session_seeded`.
4. **`signature` is stored separately from thinking text.** On some endpoints a
   signature is longer than the text it signs, so there is no field in which the
   two could be joined. See `thinking[].text_blob` and
   `thinking[].signature_blob`.

Optional fields are **omitted** from the line when absent rather than written as
`null`: `model_request.est_input_tokens`, `model_response.text_blob`,
`effect.tool_use_id`, and the `result_seq` inside a completed effect.

## Effect discipline

A side effect is recorded **before** it is performed, so a crash between the two
is detectable rather than silent.

```json
{"seq":9,"body":{"event":"effect","key":"write_file:notes.md@call_00_…","description":"write 42 bytes to `notes.md`","state":"intended","idempotent":true}}
```

`state` is one of:

| Value | Meaning |
|---|---|
| `"intended"` | Recorded, not yet performed |
| `{"done":{"result_seq":31}}` | Performed; `result_seq` points at the tool result |
| `{"done":{}}` | Performed, with no result event recorded |
| `"abandoned"` | Recorded, deliberately not performed |

On resume, only `done` counts as committed. An effect whose last state is
`intended` is **unresolved**: the record does not say whether it happened.

```
--- unresolved effect: write_file:notes.md@call_00_… (write 42 bytes to `notes.md`) recorded at seq 9 — the record does not say whether it happened
```

A resumed run reads committed effects back from the record instead of repeating
them. An unresolved effect is reported and not silently retried, because the key
cannot certify that the work was not already done.

## Addresses

An address names one thing in the record. `spot_check` takes one, and a handover
index is built from them.

| Address | Refers to |
|---|---|
| `blob:<digest>` | A blob, by its SHA-256 |
| `journal:seq=<n>` | The event at sequence `n` |
| `journal:seq=<n> (dropped by reduction)` | An event whose payload was dropped. Resolves to the statement that it was dropped — not an error, and not a success |

An address that cannot be resolved is an error naming the address. A dropped
payload is not a dangling reference; the record says what happened to it.

## Reading a record

```sh
# Summaries.
yu metrics --dir .yu

# One line, pretty-printed.
sed -n '13p' .yu/delivery-1.jsonl | python3 -m json.tool

# Every effect and its outcome.
grep '"event":"effect"' .yu/delivery-1.jsonl
```

`yu fork` branches a record at a sequence number; see [`yu fork`](../commands/fork.md).
