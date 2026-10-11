# Commands — conventions

Conventions that apply to every subcommand. Individual pages document only what
is specific to them.

## Synopsis

```
yu <COMMAND> [OPTIONS]
```

Run `yu <COMMAND> --help` for the authoritative option list of the build in
front of you.

## Exit status

| Status | Meaning |
|---|---|
| `0` | Success |
| `1` | Any runtime error. The error text is printed to stderr as `yu: <message>` |
| `2` | Argument parsing failed. Emitted by the argument parser before the command runs |

There are no other statuses, and no command defines a command-specific one. A
run that stops early for an operational reason — the round ceiling was reached,
a budget was spent — is a **successful** run: it reports `stopped: <reason>` and
exits `0`. Only a failure to run at all exits `1`.

## Output streams

Every command that produces both an answer and a report separates them.

| Stream | Carries |
|---|---|
| stdout | The result: the model's reply, the assembled blocks, or the metrics summary |
| stderr | Everything else: the `---` report lines, warnings, progress |

```sh
# The answer alone.
yu run --task "..." 2>/dev/null

# The report alone.
yu run --task "..." >/dev/null
```

## Shared options

| Option | Default | Applies to | Meaning |
|---|---|---|---|
| `--dir <DIR>` | `.yu` | `run`, `decide`, `metrics`, `fork`, `daemon` | Where journals and blobs live |
| `--cwd <CWD>` | process working directory | `run`, `decide` | The directory the tools operate in |
| `--model <MODEL>` | `$ANTHROPIC_MODEL`, else `claude-opus-5` | `exec`, `run`, `decide` | Model string, sent verbatim |
| `--tools <SET>` | `read-only` | `run`, `decide` | `read-only` or `full` |
| `--policy <PATH>` | none | `run`, `decide` | A policy TOML file |

!!! warning "`--dir` and `--cwd` are different directories"

    `--dir` holds records. `--cwd` is where tools read and write. Neither
    implies the other. See [Configuration](../configuration.md#state-directory).

## Tool sets

| Value | Tools granted |
|---|---|
| `read-only` | `read_file`, `list_dir` |
| `full` | `read_file`, `write_file`, `list_dir`, `exec` |

`read-only` is the default on both commands that take the flag. The executor's
surface is printed on start:

```
--- tool surface: list_dir, read_file
```

See [Tools](../reference/tools.md) for the schemas.

## Session naming

A session is an address on disk. Sessions are named, and naming one is how a run
is found again after it ends, including when it ends by `kill -9`.

| Command | Name when `--session` is not given |
|---|---|
| `yu run` | `run-YYYYMMDDTHHMMSS` |
| `yu decide` | `decide-YYYYMMDDTHHMMSS` |

The timestamp is UTC and has one-second resolution.

!!! warning "Give long-running work an explicit session name"

    The generated name is minted when the run **starts**. A run that is killed
    cannot be continued unless its name was known in advance. Pass `--session`
    for anything that should survive a crash:

    ```sh
    yu decide --session delivery-1 --charter "..."
    ```

    A second run under the same name at the same second would otherwise collide.
    `yu decide` refuses a same-second collision rather than silently forking a
    session onto itself.

## Reports

Commands that run a model print a report to stderr. Common line shapes:

| Line | Meaning |
|---|---|
| `--- tokens in=… out=… cache_read=…` | Token usage for the run |
| `--- spans: <kind>=<ms> …` | Per-span timings, or `NONE RECORDED` |
| `--- decision layer context: raw material N% of M chars (…)` | Composition of the decision layer's context |
| `--- executor context: raw material N% of M chars (…)` | The same, for one executor |

`raw material` is the fraction of a context that arrived as unjudged tool
output. It is the number the decision layer's design is judged by; see
[`yu decide`](decide.md#the-context-report).
