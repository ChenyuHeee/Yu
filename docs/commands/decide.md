# `yu decide`

Run the decision layer on a charter.

## Synopsis

```
yu decide [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `-c, --charter <CHARTER>` | required for a new session | What is to be achieved, in the human's own words |
| `--dir <DIR>` | `.yu` | Where the journal and blobs live |
| `--cwd <CWD>` | process working directory | The directory the tools operate in |
| `-m, --model <MODEL>` | `$ANTHROPIC_MODEL`, else `claude-opus-5` | Model string |
| `--max-rounds <N>` | `4` for a new session; otherwise what the session has left | Round ceiling |
| `--tools <SET>` | `read-only` | Tool surface offered to executors |
| `--policy <PATH>` | none | Policy TOML supplying prediction sets and reduction rows |
| `--constraint <TEXT>` | none | A standing constraint. Repeat for several |
| `--topology <KIND>` | none | The declared shape of the work |
| `--session <NAME>` | `decide-<UTC timestamp>` | The session this run belongs to. Conflicts with `--handover` |
| `--handover <PATH>` | none | Start from a handover an earlier session wrote |
| `--handover-out <PATH>` | none | Write a handover for the next session here, when this run ends |
| `--handover-at-growth <N>` | none | Also hand over once the view has grown this many characters. Requires `--handover-out` |

## What it does

Runs a layer whose job is to decide rather than to do. It is given the charter,
your constraints, and the state it has accumulated; it cannot read files, run
commands, or see raw tool output. Everything that touches the world is handed to
a fresh executor as a delegation.

The layer's own tools are `delegate`, `decide`, `retire`, and `spot_check`. It
has no `read_file`, no `write_file`, and no `exec`. See
[Tools](../reference/tools.md#decision-layer-tools).

`ask_human` is specified and **not built**. Calling it returns an unknown-tool
error naming the four that exist.

## The charter

The charter is the human's intent, word for word. It is not a task description
and not a summary.

```sh
yu decide --charter "Explain why check.py fails. Report the cause; change nothing."
```

For a new session, `--charter` is required. When continuing a session with
`--session`, it is optional: the recorded intent is restored from the record.
When it *is* given on a continuation, it is appended to the recorded intent as
a further sentence — it does not replace what was there.

## Standing constraints

`--constraint` takes one constraint. Repeat the flag for several.

```sh
yu decide \
  --charter "Add a --dry-run flag to the CLI." \
  --constraint "Never modify files outside crates/yu-cli." \
  --constraint "Do not change the exit statuses." \
  --tools full
```

Constraints are **injected into every executor's system block**, not restated by
the decision layer each round. This is the difference between a constraint that
holds and one that is dropped on the round nobody remembered to repeat it.
Removing a constraint from the charter changes what the executors are told, and
the executor is not given any constraint it was not told.

## Topology

`--topology` declares the shape of the work.

| Value | Shape |
|---|---|
| `single` | One executor |
| `chain` | Sequential hand-off, each step narrower |
| `fan_out` | One parent, N parallel children. Alias: `fanout` |
| `tree` | Recursive decomposition |
| `reflection` | Author → critic → author |
| `peer` | Coordination through a message board |
| `verification_partition` | Every work item verified exactly once, by a non-author |

!!! warning "`--topology` takes a kind, not a policy entry name"

    The value is parsed as a topology kind. If the policy file declares
    `[topology.review_loop]`, passing `--topology review_loop` fails — the
    policy entry is not looked up by name. Pass `--topology chain`.

An unrecognised value fails with the list of shapes.

## Policy

`--policy` supplies prediction sets and reduction rows. The prediction sets are
what a delegation's report is classified against.

```sh
yu decide --charter "…" --policy policy.toml
```

At start the run prints what it loaded:

```
--- policy policy.toml — 2 outcome set(s): build_result, delivery
```

With no sets declared, delegations are not classified and no landing can be
checked:

```
--- no prediction sets declared: delegations will not be classified, and no landing can be checked
```

A continuation whose policy differs from the one the session started under says
so, and runs with the newly loaded sets. See [Policy file](../reference/policy.md).

## Sessions and continuation

`--session` names the run. A name with a record already under it is continued,
and the run states what it rebuilt:

```
--- continuing a session whose record already existed: seq continues at 36; L0 = 1 constraint(s); L1 = 1 decision(s), 0 rejected, 0 open question(s), rebuilt from 3 recorded call(s)
--- rounds already spent: 1; this continuation runs rounds 2..=4
```

A name with no record starts one, and says so differently, so a name that was
not the one you meant shows up as a new session rather than as a continuation of
nothing:

```
--- session delivery-1: no events recorded yet — this run starts it, and a later run with the same name can continue it
```

Whatever the record cannot supply is named rather than guessed:

```
--- NOT restored from the record: <problem>
```

The round budget belongs to the session. A continuation runs what is **left** of
it, not a second helping. If the record holds no round marker, the run says the
budget is unknown and runs the whole of what it was given.

!!! warning "Name a session before you need to resume it"

    A run can only be continued under a name it was given when it started. A
    session killed before it wrote anything cannot be resumed, and a handover
    does not help — a handover is written when a session **ends**, and a killed
    session wrote none.

`--session` conflicts with `--handover`. Starting from a handover branches from
the record the handover indexes; continuing a session reopens its own record.
They are different operations and cannot both be requested.

## Handover

A handover carries a session's intent, constraints, decisions, and loose ends to
a new generation, with an index of where each item came from.

```sh
# Generation 1: run, and write a handover when it ends.
yu decide --charter "…" --session gen1 --handover-out handover.json

# Generation 2: start from it.
yu decide --handover handover.json --session gen2
```

On start from a handover:

```
--- resuming from a handover: 1 item(s) carried, 5 angle(s) reported, index at blob:6f97f7…
```

On writing one:

```
--- handover (<trigger>): 3 item(s) from 5 angle(s), index at blob:… → handover.json
```

| Trigger | When it fires |
|---|---|
| completion | The session reached a final answer |
| round ceiling | The session spent its round budget without one |
| growth | `--handover-at-growth <N>` and the view grew N characters past where it started |

A session that reached its round ceiling hands over without `--handover-at-growth`.
The growth rule covers a session that stopped for any other reason.

!!! note "Growth, not window size"

    `--handover-at-growth` measures growth past the starting point, not the size
    of the view. A freshly seeded view is large by construction, so a rule
    written against total size would fire immediately and keep firing.

When a handover was requested but did not fire:

```
--- no handover: <reason>, and its view did not grow past the declared growth threshold either — this boundary did not warrant the change
```

A handover does not carry everything. What it left out is printed:

```
--- the handover does not carry: <text> [<address>]
```

The address resolves back to the record through the same path `spot_check`
uses.

## Reporting

stdout carries the layer's final answer. Everything below goes to stderr.

### The context report

```
--- decision layer context: raw material 0.0% of 24531 chars (raw 0 + reduced 6490 + calls 0 + definitions 4278; tool machinery 44.4%)
```

`raw material` is the fraction of the context that arrived as unjudged tool
output. It is the number the decision layer's premise is judged by: near zero
means the layer is holding decisions, not evidence.

### Per-request growth

```
--- decision-layer context per request: 10127 23433 31342 29034 29859 29557
--- peak context: 31342 chars, raw material 0.0%
```

Sizes in characters, one per request, in order. It rises within a round as the
conversation grows and falls when the next round re-renders the view. `peak` is
the worst point of the run, not the last one.

### Landings

```
--- landings
      #0 delivery/delivered_and_verified — the executor ran report.sh and it printed every file
      #1 build_result/compile_error — the compiler rejected the trait bound (the executor called it `build_broke`)
      #2 not classified (no set was pre-registered)
      !! the executor landed on `delivered_and_verified`, but a command ran and failed: `echo …` exited 1
```

Each line is one delegation: the slot it landed in, and why. `!!` marks a
**contradiction** — a landing the harness checked against the record and found
inconsistent. This check is deterministic and runs on every delegation; the
decision layer is shown the contradiction rather than the raw evidence.

An honest landing produces no `!!` line. A check that fired on correct runs
would be worth nothing.

### Agent tree

```
--- agents: 2 derived
      decision
        └ executor#1 — RUNTIME: reproduce the failure of check.py and identify the …
          └ executor#1.1 — Read the file `.yu/decide-….jsonl` in the curr…
```

Read from the record, not from memory. Derivation stops at 3 levels below the
decision layer; an executor at the ceiling is not offered `delegate`, and a call
to it anyway is answered with the ceiling it hit. See
[Delegation depth](../reference/tools.md#delegation-depth).

### Ledger (L1)

```
--- L1: 2 decision(s), 2 rejected, 0 open question(s)
      decided: record what you conclude before you answer — because …
      ruled out: a third read-only pass re-deriving the cause — because …
```

When the layer recorded nothing:

```
      (the run wrote nothing to state: it reached no conclusion it was willing to record)
```

### Stopping early

```
--- stopped: <reason>
```

A run that spends its round ceiling is a successful run that stopped early, not
a failure. The exit status is `0`.

## Examples

Read-only investigation, four rounds:

```sh
yu decide \
  --charter "Explain why check.py fails. Report the cause; change nothing." \
  --cwd . --tools read-only
```

A named, resumable session with constraints and a topology:

```sh
yu decide \
  --session delivery-1 \
  --charter "Deliver report.sh in this directory." \
  --constraint "Work only inside the current directory; never modify anything outside it." \
  --topology chain \
  --policy policy.toml \
  --tools full
```

Continue it after a crash:

```sh
yu decide --session delivery-1
```
