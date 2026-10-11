# `yu daemon`

Serve sessions over a Unix socket, owning the records they live in.

## Synopsis

```
yu daemon [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--dir <DIR>` | `.yu` | Where sessions keep their journals and descriptions |
| `--socket <SOCKET>` | `.yu/daemon.sock` | The socket the protocol is served on |

## What it does

Binds a Unix socket and serves the session protocol on it, in the foreground.
The session belongs to the daemon, not to the client that started it, so a client
that dies does not take the session with it.

On start:

```
--- daemon up: sessions in .yu, protocol on .yu/daemon.sock
```

If the socket path is already occupied by a live daemon, it refuses to start.
If the path holds a socket that nobody is serving — what `kill -9` leaves behind
— it says so and rebinds:

```
--- .yu/daemon.sock is a socket nobody is serving: the daemon that bound it is gone (a kill -9 leaves exactly this). Rebinding.
```

Nothing is attached at start-up. Sessions are read from disk per request, so a
daemon that opened every journal eagerly would repair every torn tail before
anyone asked.

!!! danger "The daemon does not run model turns"

    `send` records the human's words into the session's journal. It does not
    call a model, and the daemon never constructs a client. The daemon can
    *resume* a session and record into it, but nothing here executes a turn.

    `yu run` and `yu decide` are separate processes that open their own
    journals. They do not go through the daemon.

## Session storage

The daemon uses a different on-disk layout from the CLI. Each session gets its
own directory:

```
.yu/
  daemon.sock
  <session-id>/
    info.json           session description: status, timestamps, last seq
    <session-id>.jsonl  the journal
    blobs/<2 hex>/<62 hex>
```

Compare [Record format](../reference/record.md#two-layouts). A consequence worth
knowing: [`yu metrics`](metrics.md) reads only top-level `*.jsonl` files and will
not summarise daemon-owned sessions.

## Protocol

**Framing.** One JSON object per line, newline-terminated, in both directions. A
blank line is skipped. A malformed line is answered with an `error` frame rather
than closing the connection.

**No client subcommand exists.** `yu` has no `attach`, `send`, or `status`
command. Speak the protocol directly, or write a client against
`yu_server::session::{Request, Response, encode_line, decode_line}`.

### Requests

Every frame carries a `type` discriminator.

| Frame | Fields | Effect |
|---|---|---|
| `{"type":"start","charter":"…"}` | `charter` | Starts a session. Answers `started` |
| `{"type":"resume","session":"s-1"}` | `session` | Reattaches to a session the daemon owns, including one it is recovering. Answers `status` — **not** `started` |
| `{"type":"send","session":"s-1","text":"…"}` | `session`, `text` | Records the text as a user message. Answers `chunk` then `done` |
| `{"type":"stop","session":"s-1"}` | `session` | Marks the session stopped. Answers `done` |
| `{"type":"status","session":"s-1"}` | `session`, or `null` for all | Answers `status` |

### Responses

| Frame | Fields |
|---|---|
| `{"type":"started","session":"s-1"}` | `session` |
| `{"type":"chunk","session":"s-1","text":"…"}` | A piece of streamed output |
| `{"type":"done","session":"s-1","stop_reason":"…"}` | `stop_reason`, carried verbatim |
| `{"type":"error","message":"…"}` | The failure |
| `{"type":"status","sessions":[ … ]}` | A list of `SessionInfo` |

`stop_reason` is never flattened to a generic "finished". The daemon's own
values are:

| Value | Emitted by |
|---|---|
| `recorded` | `send` — the message was recorded |
| `stopped` | `stop` |

A `send` answers with an explicit statement that no turn ran:

```
recorded at seq 12; running the turn needs a model client, which `yu-server` deliberately does not construct
```

### `SessionInfo`

```json
{
  "id": "s1759000000000-1",
  "status": "ready",
  "created_ms": 1759000000000,
  "last_active_ms": 1759000001234,
  "journal_dir": ".yu/s1759000000000-1",
  "last_seq": 12
}
```

`status` is one of `starting`, `ready`, `busy`, `stopped`, or an object for a
crash: `{"crashed":{"at_ms":1759000009999}}`. A crash is reported with a
wall-clock timestamp rather than a duration, so it stays comparable with journal
timestamps across daemon restarts.

`last_seq` is the last sequence number that decodes. It is absent when the host
keeps no record, which is not the same as a record that is empty.

## Talking to it

```sh
yu daemon --dir .yu &
socat - UNIX-CONNECT:.yu/daemon.sock
```

Then, one frame per line:

```json
{"type":"start","charter":"Deliver report.sh in this directory."}
{"type":"status","session":null}
{"type":"send","session":"s1759000000000-1","text":"Begin."}
{"type":"stop","session":"s1759000000000-1"}
```

## Exit status

| Status | Condition |
|---|---|
| `0` | The daemon shut down cleanly |
| `1` | The socket is served by a live daemon, or binding failed |
