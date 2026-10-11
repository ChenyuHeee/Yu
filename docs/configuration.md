# Configuration

`yu` reads no configuration file. All configuration is environment variables and
command-line flags.

## Environment variables

| Variable | Read by | Default when unset |
|---|---|---|
| `ANTHROPIC_API_KEY` | credential | — |
| `ANTHROPIC_AUTH_TOKEN` | credential, used only if `ANTHROPIC_API_KEY` is unset | — |
| `ANTHROPIC_BASE_URL` | endpoint | `https://api.anthropic.com` |
| `ANTHROPIC_MODEL` | default model | `claude-opus-5` |
| `YU_QUIRKS_JSON` | path to a saved endpoint profile | — |
| `YU_QUIRKS` | named endpoint profile, used only if `YU_QUIRKS_JSON` is unset | `unknown` |

## Credentials

Set exactly one. Both are sent as the `x-api-key` header; the two names exist
because different deployments issue different shapes of secret, not because they
are sent differently.

```sh
export ANTHROPIC_API_KEY=sk-...
```

!!! danger "No credential is a hard failure"

    With neither variable set, the client fails immediately:

    ```
    no credential: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN
    ```

    Credentials are redacted from the client's `Debug` output and are never
    written to a record. The endpoint itself is less careful — a `401` response
    body echoes the last four characters of the key it received. Treat captured
    error bodies as sensitive. See
    [Quirk 6](integration/endpoint.md#quirk-6).

## Endpoint

```sh
export ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic
```

The endpoint must speak the Anthropic Messages API. It need not be Anthropic's,
and it need not honour everything Anthropic honours — that is what the quirks
profile is for.

Before pointing `yu` at an endpoint, measure it:

```sh
yu probe
```

## Model

Resolution order, highest first:

1. `--model` on the command line
2. `ANTHROPIC_MODEL`
3. `claude-opus-5`

```sh
export ANTHROPIC_MODEL=claude-sonnet-5
```

The model string is sent verbatim. `yu` does not validate it against a list and
does not map between names.

## Endpoint profile (quirks)

The endpoint profile records which of the Messages API's behaviours the endpoint
actually has. It is not cosmetic: it decides whether history must carry
`thinking` blocks, whether identifiers may be synthesised, and whether
`cache_control` does anything.

There are two ways to set it, and `YU_QUIRKS_JSON` wins.

**Measured — preferred.** Write the report from a probe and read it back.

```sh
yu probe --json > caps.json
export YU_QUIRKS_JSON=$PWD/caps.json
```

**Named — a shortcut, unverified against your endpoint.**

```sh
export YU_QUIRKS=anthropic
```

| Name | Meaning |
|---|---|
| `anthropic` | Anthropic's own behaviour |
| `deepseek-anthropic`, `deepseek` | The measured behaviour of `https://api.deepseek.com/anthropic` |
| `unknown` | Assumes the strict rules. The default when nothing is set |

!!! warning "A profile that fails to load does not stop the run"

    If `YU_QUIRKS_JSON` names a file that cannot be read or parsed, the client
    logs an error and falls back to the `unknown` profile — it does not abort.
    The run then proceeds under constraints nobody chose. Check the log if a run
    behaves as though the endpoint were stricter than it is.

## State directory

Commands that read or write records take `--dir`, default `.yu`. It holds the
journals and the blob store.

```
.yu/
  <session>.jsonl
  blobs/<2 hex>/<62 hex>
```

!!! warning "`--dir` is not the working directory of the tools"

    `--dir` is where records live. `--cwd` is the directory the tools operate
    in, and it defaults to the process working directory. `--cwd` defaults to
    the directory you launched `yu` from, **not** to `--dir`. A run with
    `--dir /var/yu` and no `--cwd` reads and writes files under the current
    directory and keeps its journal in `/var/yu`.

    `--cwd` is available on `yu run` and `yu decide`. `yu probe`, `yu exec`,
    `yu metrics`, `yu fork`, and `yu daemon` do not take it.

Add `.yu/` to `.gitignore`. Journals contain model conversations, tool output,
and file contents read during the run.

## Span output

Span timings are printed to **stderr** by the commands that produce them, and a
run that recorded none prints `spans: NONE RECORDED (observation is not wired
up)` rather than an empty line. The observation surface has no environment
variable: spans are always emitted, and per-step harness overhead is always
reported. See [`yu run`](commands/run.md#reporting).
