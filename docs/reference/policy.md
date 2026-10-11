# Policy file

A policy is a TOML file that changes behaviour without a rebuild. It is passed
with `--policy` on `yu run` and `yu decide`.

```sh
yu run --task "…" --policy policy.toml
yu decide --charter "…" --policy policy.toml
```

There is no default path and no environment variable. Without `--policy`, the
built-in rules apply and no prediction sets are declared.

The TOML document **is** the policy. There is no `[policy]` wrapper table, and
an unknown top-level key is rejected.

## Schema

All keys are optional.

| Key | Type | Meaning |
|---|---|---|
| `prompts` | table, name → text | Named prompt texts |
| `tool_descriptions` | table, tool name → text | Override a tool's description |
| `roles` | table of tables | Declarative roles |
| `topology` | table of tables | Declarative topologies |
| `reduction` | array of tables | Reduction rules, in priority order |
| `hooks` | array of tables | Event → script bindings |
| `outcome_sets` | array of tables | Prediction sets for delegations |
| `scripts` | table, name → source | Inline script source |
| `budgets` | table | Run ceilings |

!!! warning "Only two of these are consumed today"

    `[[reduction]]` and `[[outcome_sets]]` are wired into the loop. The other
    seven parse, load, and are reachable through the library's accessors, but
    nothing in the run loop reads them yet. **Writing them changes nothing.**

    They are documented here so the format is not a surprise when they are
    wired, not because they currently do anything. A file that sets `[budgets]`
    and expects the run to be bounded will not be bounded.

## `[budgets]`

| Key | Type |
|---|---|
| `max_steps` | integer |
| `max_input_tokens` | integer |
| `max_output_tokens` | integer |
| `max_tool_calls` | integer |

Absent means **no ceiling configured**, never zero. Unknown keys here are
rejected — a typo like `max_input_token` is a parse error, not a silently
ignored line.

## `[roles.<name>]`

| Key | Type | Meaning |
|---|---|---|
| `prompt` | string | The **name** of an entry in `[prompts]`, not the text |
| `model` | string | Model string for this role |
| `effort` | string | `low`, `medium`, `high`, `xhigh`, or `max` |

## `[topology.<name>]`

| Key | Type | Meaning |
|---|---|---|
| `kind` | string | One of the topology kinds |
| `child_role` | string | The role children run as |
| `params` | table, string → string | Kind-specific parameters |

Topology kinds: `single`, `chain`, `fan_out` (alias `fanout`), `tree`,
`reflection`, `peer`, `verification_partition`.

`params` values must be quoted strings — `width = "4"`, not `width = 4`. Only
`width`, consumed by `fan_out`, is read today. Its default is 3.

!!! note "`--topology` does not look these up"

    A policy entry named `[topology.review_loop]` is not reachable by
    `--topology review_loop`. That flag parses a topology **kind**. See
    [`yu decide`](../commands/decide.md#topology).

## `[[reduction]]`

Each row states a path and the axes it constrains. Rows are evaluated in
declaration order and the first match wins.

```toml
[[reduction]]
path = "drop_with_manifest"
[reduction.when]
size = "large"
reproducible = "true"
```

| Key | Type |
|---|---|
| `path` | string |
| `when` | table, axis → value |

Paths: `pass_through`, `drop_with_manifest`, `spill_with_pointer`,
`dispatch_reader` (alias `delegate_reader`), `sandbox_reduce`.

Axes: `kind`, `size`, `reproducible`, `addressable`, `pressure`,
`already_on_disk`, `tool`.

| Axis | Accepted values |
|---|---|
| `kind` | `file`, `search`, `log`, `diff`, `structured` |
| `size` | `small`, `large` |
| `reproducible` | `true` / `yes`, `false` / `no` |
| `addressable` | `true` / `yes`, `false` / `no` |
| `already_on_disk` | `true` / `yes`, `false` / `no` |
| `pressure` | `low`, `high` |
| `tool` | a tool name, e.g. `exec`, `read_file` |

Axes within a row are **conjunctive**: every named axis must match.

!!! warning "An unknown axis makes a row dead, not fatal"

    A row naming an axis or path this build does not recognise loads without
    error and can never match. The CLI reports it:

    ```
    --- policy policy.toml: [[reduction]] row(s) name axis/axes this build does not understand: shape — such a row cannot match anything, so it is dead rather than merely unsatisfied
    ```

    Check this line. A dead row is otherwise indistinguishable from a row whose
    condition never came up.

## `[[outcome_sets]]`

A prediction set is the finite list of things a delegation can come back with.
It is declared when the delegation is made, and the executor classifies its
actual result into one of the slots. The report shrinks from prose to a choice.

```toml
[[outcome_sets]]
id = "build_result"
description = "What a build-and-test delegation can come back with."

[[outcome_sets.slots]]
name = "pass"
expects = "succeeded"

[[outcome_sets.slots]]
name = "compile_error"
expects = "failed"

[[outcome_sets.slots]]
name = "test_failure"
expects = "failed"

[[outcome_sets.slots]]
name = "no_changes_needed"
expects = "nothing_run"
```

| Set key | Type | Required |
|---|---|---|
| `id` | string | yes |
| `description` | string | no |
| `slots` | array of tables | yes |

| Slot key | Type | Required |
|---|---|---|
| `name` | string | yes |
| `expects` | string | yes |

### `expects`

Each slot states a claim, and the claim is checked against the record. Only
claims the harness can compute are allowed.

| Value | Claim |
|---|---|
| `succeeded` | The run completed and every command it ran exited zero |
| `failed` | At least one command reported a non-zero exit status |
| `nothing_run` | The run completed without running a command |
| `stopped_early` | The run was stopped before it reported completion |
| `unchecked` | No claim. Not cross-checked |

`unchecked` is how a slot opts out of the check. The others are computed, and a
landing that contradicts its slot's claim is reported as a contradiction:

```
!! the executor landed on `delivered_and_verified`, but a command ran and failed: `echo …` exited 1
```

!!! note "The check is bidirectional"

    A landing that matches its claim produces no output. A check that fired on
    correct runs would be worth nothing.

### Rules

The loader enforces five rules and names the set and slot it is complaining
about.

1. Set ids are unique.
2. No slot may be named `other`. The name is reserved — `other` is always
   available, appended when the set is rendered, and never declared in a file.
3. A set with no slots is an error. With nothing to land in, every result lands
   in `other` and the set is decoration.
4. Slot names are unique within a set.
5. A missing or malformed field names the set and the slot it is in.

Declaring no `outcome_sets` key is fine, and so is `outcome_sets = []`. Only a
*declared* set is held to these rules.

!!! note "`other` is where the interesting results go"

    A result nobody predicted is the most informative one, so the escape hatch
    is always present. Landing in `other` also widens the report budget, because
    that is the moment detail is worth having.

    Watch the distribution. A slot that is never chosen means the set is
    decoration; `other` being common means the set is wrong.

## `[[hooks]]` and `[scripts]`

```toml
[[hooks]]
on = "on_handover"
script = "handover_is_worth_it"

[scripts]
handover_is_worth_it = "ctx.loose_ends > 0 || ctx.steps_since_compaction > ctx.threshold"
```

| Hook key | Type | Meaning |
|---|---|---|
| `on` | string | The event name |
| `script` | string | The **name** of an entry in `[scripts]` |

`[scripts]` values are **inline source text**, not file paths. The semantics are
Rhai. A path given as a value would be evaluated as a broken Rhai expression.

Naming a script that does not exist is an error. Omitting `script` records the
intent without arming it.

Neither is consumed today. See the warning at the top of this page.

## Load errors

| Condition | Behaviour |
|---|---|
| File missing | Exit `1`: ``could not read policy file `<path>`: …`` |
| Malformed TOML | Exit `1`: `policy TOML is malformed: …` |
| Unknown top-level key | Exit `1`, same as malformed |
| Unknown key in `[budgets]` | Exit `1` |
| Outcome-set rule violation | Exit `1`, naming the set and slot |
| Unknown reduction axis or path | **Loads.** Warning on stderr, row is dead |
| Unknown key elsewhere | Ignored |

## A complete example

```toml
[prompts]
decision_charter = """
You are the decision layer. You do not read code and you do not call tools.
You own completion criteria, architecture, and anything irreversible.
"""

[roles.decision]
prompt = "decision_charter"
effort = "xhigh"

[topology.review]
kind = "reflection"
child_role = "worker"

[[reduction]]
path = "drop_with_manifest"
[reduction.when]
size = "large"
reproducible = "true"

[[reduction]]
path = "spill_with_pointer"
[reduction.when]
size = "large"
reproducible = "false"
addressable = "true"

[[outcome_sets]]
id = "delivery"
description = "What a deliverable-producing delegation can come back with."

[[outcome_sets.slots]]
name = "delivered_and_verified"
expects = "succeeded"

[[outcome_sets.slots]]
name = "delivered_unverified"
expects = "nothing_run"

[[outcome_sets.slots]]
name = "blocked"
expects = "failed"
```
