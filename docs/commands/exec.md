# `yu exec`

One request, straight to the wire. The short-path bypass.

## Synopsis

```
yu exec --prompt <PROMPT> [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `-p, --prompt <PROMPT>` | required | The prompt |
| `-m, --model <MODEL>` | `$ANTHROPIC_MODEL`, else `claude-opus-5` | Model string |
| `--max-tokens <N>` | `4096` | Output token ceiling for the response |
| `--show-thinking` | off | Stream the model's reasoning summary as it arrives |
| `--timing` | off | Print timings to stderr |
| `--json` | off | Print the assembled content blocks as JSON |

## What it does

Sends one request and streams the reply to stdout. It deliberately does not
touch sessions, journals, tools, or policy:

- No session is created and no record is written.
- No tools are offered. A `tool_use` block in the response is returned, never
  executed.
- No policy file is read.
- No reduction is applied. The reply is emitted as it arrives.

This is what a short task should cost. `yu run` exists for everything that needs
a record, and it pays for that record on every step.

## Thinking

Thinking is always requested, with its display set to summarized. `--show-thinking`
controls whether the summary is *shown*, not whether it is requested. Without
the flag the summary is streamed and discarded.

!!! note "Thinking blocks are part of the response, not decoration"

    On endpoints that require it, an assistant turn containing `tool_use` must
    be sent back with its `thinking` block intact. `yu exec` does not replay
    anything, so this does not arise here — but it is why the field is
    requested at all. See [Quirk 1](../integration/endpoint.md#quirk-1).

## `--timing`

Prints to stderr:

```
--- stop=<STOP_REASON> in=<N> out=<N> cache_read=<N> attempts=<N>
--- wire_wait=<ms> first_token=<ms> stream=<ms> events=<N> text=<N>B signature=<N>B
```

| Field | Meaning |
|---|---|
| `stop` | The response's `stop_reason`, verbatim |
| `wire_wait` | Time to first byte from the endpoint |
| `first_token` | Time to the first content token |
| `stream` | Total time spent streaming |
| `attempts` | Requests sent, including retries |
| `events` | SSE events received |
| `signature` | Bytes of thinking signature — measured separately because it can exceed the thinking text itself |

`--timing` also prints `--- tool calls: <name>(<id>), …` when the response
contained any.

!!! note "Retries"

    A request is retried only while nothing has been emitted. Once a token has
    reached stdout, a transport failure is reported rather than retried, because
    retrying would duplicate output.

## Examples

Ask one question and print only the answer:

```sh
yu exec -p "reply ok"
```

Measure the wire against the configured endpoint:

```sh
yu exec -p "reply ok" --timing >/dev/null
```

Inspect the raw blocks, which is how you see a `thinking` or `tool_use` block
that the text stream would not show:

```sh
yu exec -p "what time is it in Tokyo?" --json
```
