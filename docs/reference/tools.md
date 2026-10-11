# Tools

Two different tool surfaces exist. An executor is offered file and shell tools; the
decision layer is offered delegation and ledger tools. They do not overlap.

## Executor tools

Enabled by `--tools`. The set is fixed; there is no per-tool switch.

| Tool set | Tools |
|---|---|
| `read-only` | `read_file`, `list_dir` |
| `full` | `read_file`, `write_file`, `list_dir`, `exec` |

The surface is printed at the start of a run:

```
--- tool surface: list_dir, read_file
```

### `read_file`

Read a file as UTF-8 text, relative to the working directory.

| Parameter | Type | Required |
|---|---|---|
| `path` | string | yes |

Resolves absolute paths as given and relative paths against the working
directory. Rejects non-UTF-8 content rather than lossily converting it. The
result is marked reproducible and addressable: the same call returns the same
bytes, and the bytes can be spilled and addressed.

### `write_file`

Write UTF-8 text to a file, creating parent directories as needed.

| Parameter | Type | Required |
|---|---|---|
| `path` | string | yes |
| `content` | string | yes |

Truncating overwrite. There is no append mode and no read-modify-write. Declares
a non-reproducible effect: a write is a side effect and is not replayed.

### `list_dir`

List one directory's entries, sorted, directories suffixed with `/`.

| Parameter | Type | Required |
|---|---|---|
| `path` | string | no — defaults to `.` |

Non-recursive. One level only.

### `exec`

Run a shell command with `sh -c` in the working directory; stdout, stderr and
the exit code are captured.

| Parameter | Type | Required |
|---|---|---|
| `command` | string | yes |

The result is rendered as:

```
$ <command>
exit: <code>
stdout (<n> bytes):
<stdout>
stderr (<n> bytes):
<stderr>
```

When the process was killed by a signal, the marker line is
`exit: terminated by signal` — no exit code is invented.

!!! danger "`exec` is unsandboxed"

    No chroot, no path jail, no allow-list, no timeout. `..` is not blocked and
    absolute paths are honoured. The command inherits the environment of the
    `yu` process and runs with its privileges. stdin is `/dev/null`.

    Output is captured in full and bounded only afterwards, by the reduction
    layer. See [Reduction](#reduction).

### Conditional executor tools

| Tool | Offered when |
|---|---|
| `delegate` | The run is permitted to derive agents — see [Delegation depth](#delegation-depth) |
| `land` | The delegation was launched with a prediction set |

An executor's `delegate` takes `task` (required) and `items` (optional, the
places to cover).

`land` classifies the run's own outcome: `slot` (the name of the slot it landed
in, or `other`) and `why` (the reason, in the executor's words). Both required.

## Decision layer tools

Fixed set, in fixed order. The order is part of the cache prefix, so it does not
vary between runs.

| Tool | Purpose |
|---|---|
| `delegate` | Hand a self-contained task to a fresh executor |
| `decide` | Record a decision or a rejection into the ledger |
| `retire` | Remove an entry from the ledger, with a reason |
| `spot_check` | Pull one thing out of the record, by address |

`ask_human` is specified and **not built**. It has no schema and no dispatch;
calling it returns an error naming the four tools that exist.

### `delegate`

| Parameter | Type | Required |
|---|---|---|
| `task` | string | yes |
| `question` | string | no |
| `items` | array of string | no |
| `expected_files` | array of string | no |
| `outcomes` | string, one of the policy's set ids | yes, **iff** the policy declares at least one set |

When the policy declares no outcome sets, `outcomes` is removed from the schema
entirely and only `task` is required. When it declares sets, `outcomes` is
required and its value is constrained to the declared ids — the layer chooses
from a list, it cannot invent a set.

A delegation that names no set, or an unknown one, is refused with an error
result rather than run.

The delegation returns a report: the executor's final text, its evidence
(commands and exit codes), shape signals, an omitted-item manifest, step and
token counts, its landing, and any contradiction the harness found.

### `decide`

| Parameter | Type | Required |
|---|---|---|
| `what` | string | yes |
| `why` | string | yes |
| `verdict` | `accepted` or `rejected` | yes |

Writes to the ledger (L1). `accepted` records a decision; `rejected` records a
ruled-out proposal, which is what stops the layer re-proposing something it
already dismissed.

`what` is the entry's identity: `retire` later matches it exactly. A repeat of
the same `what` is reported and not recorded twice. Reversing a verdict is
recorded as a reversal, with both reasons.

### `retire`

| Parameter | Type | Required |
|---|---|---|
| `item` | string | yes |
| `reason` | string | yes |

Removes an entry, matched by its exact text — a decision's or rejection's
`what`, an open question, or a roster entry. This is the only mechanism that
shrinks the ledger.

### `spot_check`

| Parameter | Type | Required |
|---|---|---|
| `address` | string | yes |

Resolves one address and loads it into the decision layer's own context. This is
the only channel by which raw material reaches that layer, and its cost is
charged there — it is a scarce action, not a reading habit.

Address forms are in [Record format](record.md#addresses).

## Delegation depth

An executor may derive further executors. The ceiling is **3 levels below the
decision layer**:

```
decision
  └ executor#1
      └ executor#2
          └ executor#3      ← at the ceiling
```

An executor at the ceiling is not offered `delegate` at all. If the model calls
the name anyway, it is answered without an error:

```
there is no `delegate` here: derivation stops at 3 levels, and this run is at the last one. Do the work here, or report what you could not do and why.
```

## Reduction

A tool result is not a string. It carries a kind, a size, whether it can be
reproduced, whether it can be addressed, and which tool produced it. The path
taken is chosen from those axes plus the current context pressure.

| Axis | Values |
|---|---|
| `kind` | `file`, `search`, `log`, `diff`, `structured` |
| `size` | `small` (≤ 8192 bytes), `large` |
| `reproducible` | `true` / `false` |
| `addressable` | `true` / `false` |
| `pressure` | `low` (fill < 0.75), `high` |
| `already_on_disk` | `true` / `false` |
| `tool` | the producing tool's name, e.g. `exec` |

| Path | What happens |
|---|---|
| `pass_through` | The result goes through unchanged |
| `drop_with_manifest` | Dropped, with a manifest saying how much was dropped and the command to re-run |
| `spill_with_pointer` | Written to the blob store, replaced by a pointer carrying instructions |
| `dispatch_reader` | A shape-specialised reader agent is dispatched. Alias: `delegate_reader` |
| `sandbox_reduce` | Reduced inside a sandbox; only the reduced result returns |

Built-in thresholds: `inline_limit` is 8192 bytes and `reader_limit` is 2048
bytes. A policy's `[[reduction]]` rows replace the built-in choice for the rows
they match. See [Policy file](policy.md#reduction).

Every reduction is an explicit event in the record. A manifest states what was
left out rather than approximating it.
