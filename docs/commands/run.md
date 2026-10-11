# `yu run`

Run one task through the execution loop: journal, tools, reduction, shape
metrics.

## Synopsis

```
yu run --task <TASK> [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `-t, --task <TASK>` | required | The task |
| `--dir <DIR>` | `.yu` | Where the journal and blobs live |
| `--cwd <CWD>` | process working directory | The directory the tools operate in |
| `-m, --model <MODEL>` | `$ANTHROPIC_MODEL`, else `claude-opus-5` | Model string |
| `--max-steps <N>` | `12` | Step ceiling for the run |
| `--tools <SET>` | `read-only` | `read-only` or `full` |
| `--session <SESSION>` | `run-<UTC timestamp>` | The session this run belongs to |
| `--policy <PATH>` | none | Policy TOML supplying `[[reduction]]` rows |

## What it does

Stands up a session, offers the model a tool surface, executes what it asks
for, applies the reduction rules to each tool result, and writes every step to
the journal. Prints the model's final text to stdout.

The run ends when the model stops asking for tools, or when `--max-steps` is
spent. A run that spends its step ceiling is not an error; the outcome is
reported and the exit status is `0`.

!!! warning "`--tools full` removes the guard rails"

    With `full`, the model can call `exec`, which runs `sh -c` with no sandbox,
    no timeout, and no path restriction, inheriting the environment of the
    process. It can do anything the process can. There is no confirmation step.

## Sessions

`--session` names the run. Reopening a name that already has a record
**continues** it rather than starting a new one:

```
--- resuming session delivery-1: 14 event(s) already recorded, continuing at seq 14
```

This is what a run after a crash is. Effects that the record marks as completed
are read back from the record rather than performed again, so a resumed run does
not repeat a write that already happened.

If the record says an effect was started but never completed, the run says so
before it continues:

```
--- unresolved effect: write_file:notes.md@call_00_… (write 42 bytes to `notes.md`) recorded at seq 9 — the record does not say whether it happened
```

See [Record format](../reference/record.md#effect-discipline).

## Policy

`--policy` supplies a TOML file whose `[[reduction]]` rows replace the built-in
reduction choices. Rows are evaluated in declaration order and the first match
wins.

```sh
yu run --task "summarise the logs" --policy policy.toml
```

A row that names an axis or a path this build does not understand is **not** a
load error. It is reported on stderr and the row is dead — it cannot match
anything:

```
--- policy policy.toml: [[reduction]] row(s) name axis/axes this build does not understand: shape — such a row cannot match anything, so it is dead rather than merely unsatisfied
```

A malformed file, or an unknown top-level key, does fail the run with exit `1`.
See [Policy file](../reference/policy.md).

## Reporting

stdout carries the model's final reply and nothing else. Everything below goes
to stderr.

| Line | Meaning |
|---|---|
| `session <id> — journal in <dir>` | The session's address |
| `--- tool surface: <names>` | The tools this run was offered |
| `--- N step(s), N tool call(s), N reduction(s) in Ns` | Run totals |
| `--- tokens in=… out=… cache_read=… \| harness=…ms/step model_time=…ms` | Token accounting and overhead |
| `--- spans: <kind>=<ms> …` | Per-span timings |
| `--- reductions: <names>` | Which reduction paths fired |
| `--- shape: <signal>` | Shape signals, one line each |
| `--- executor context: raw material N% of M chars (…)` | Context composition |

**`harness=…ms/step`** is the harness's own cost with model time removed: the
sum of every span that is not waiting on the model, divided by the number of
steps. It is `NaN` when the run took zero steps. This is the number that
separates the cost of the harness from the cost of the model.

If no span was recorded at all, the line reads:

```
--- spans: NONE RECORDED (observation is not wired up)
```

**`--- shape: <signal>`** lines report deterministic signals — repeated
identical calls, files touched outside the expected set, tokens growing without
a diff. They are reported, not judged; deciding what they mean is the decision
layer's job. See [`yu decide`](decide.md).

## Examples

One task with no side effects:

```sh
yu run --task "count the Rust source files under crates/" --cwd .
```

A named session, so a crash is recoverable:

```sh
yu run --session migrate-1 --task "rename the config key" --tools full --cwd .
```

Read the record back afterwards:

```sh
yu metrics --dir .yu
```
