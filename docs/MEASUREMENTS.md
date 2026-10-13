# Measurements

Numbers this project produced by running, kept because they are the reason to
believe or disbelieve any of it. Where a claim and a measurement disagree, the
measurement is the finding.

Every block below is copied from a real run, not composed. Where a number has not
been taken, it says so rather than showing a plausible one.

---

## The decision layer's context composition

DESIGN's acceptance criterion:

> 决策层跑完一个真实任务后,它上下文里**未经人或模型判断、直接倒进来的原始材料**所占的比例是多少?
> 接近零 → **设计成立**;超过一两成 → **前提有问题**。

`check.py` fails (`math_util.mean` returns `total / len(xs) + 1`). Charter:
*"Explain why check.py fails. Report the cause; change nothing."* Read-only tool
surface, one round, one delegation:

```
--- 1 round(s), 1 delegation(s) in 39.4s
--- tokens in=4087 out=3976 cache_read=10368
--- decision layer context: raw material 0.0% of 26070 chars (raw 0 + reduced 7149 + calls 6825 + definitions 808; tool machinery 56.7%)
--- decision-layer context per request: 7941 20317 26070
--- peak context: 26070 chars, raw material 0.0%
--- executor #0 context: raw material 8.3% of 18929 chars (raw 1580 + reduced 0 + calls 878 + definitions 502; tool machinery 15.6%)
--- spans: assemble=5.9ms first_token=5133.3ms init=4.3ms journal=8.7ms reduce=0.1ms spawn=18465.6ms stream=37503.3ms tool_exec=4.4ms wire_wait=1777.2ms
```

Reproduce with `yu decide --charter "…" --dir <dir> --cwd <dir> --tools read-only`.

**The decision layer holds no raw material.** 0 of 26 070 characters arrived as
they came off a tool. What is in there is its own prompt, the charter, the
ledger, its own `delegate` arguments, and one report the executor reduced to a
budget before sending.

**And it is 56.7% tool machinery.** Nearly all of that is the delegation's
arguments (6 825) and that one report (7 149). The two numbers disagreeing is the
finding rather than a contradiction: a context can be almost entirely machinery
and contain nothing that was poured in unjudged. The old metric could not say
this — it counted a reduced report exactly as it counted `grep` output, so both
landed in the same bucket and the bucket read ~30–66%.

The executor's line is the contrast the design rests on: 8.3% raw material in a
run that only read files. Under `--tools full` this is where the number goes up,
because `exec` output is what `Evidence.content` carries — see the gap below.

### The path this does not cover

`--tools read-only` offers `list_dir` and `read_file`, and **neither produces
evidence** (`delegate::outcome_from_run` pairs only `exec` calls with their
results). So this run exercised no evidence blocks, and the evidence section is
the one channel by which raw material reaches this layer. The claim "the decision
layer holds no raw material" is therefore measured *for a run with no evidence*,
and the other case is guarded by test rather than by run:
`a_view_charges_evidence_as_raw_material_and_its_report_as_a_reduction` and
`the_measurement_moves_when_evidence_accumulates` both fail if the evidence
section stops being charged as raw.

Taking that number end-to-end needs `yu decide --tools full`, which runs
model-chosen `sh -c` commands with no sandbox. Not run here.

### History: the numbers this replaced

Measured 2026-10-10 against the **old** metric, whose numerator was "tool
machinery" (tool results + tool calls + tool definitions) and which summed its
meter across rounds:

| run | tool machinery | what changed |
|---|---|---|
| 1 | **65.9%** | initial — `delegate` returned the executor's full essay (≈34 000 chars) |
| 2 | 48.4% | report put under `REPORT_BUDGET_CHARS` (2 500) with a pointer to the rest |
| 3 | **28.8%** | one delegation per round |
| 4 | 36.8% | run-to-run variance |

Those figures are real measurements of that metric, and they are kept because two
of the defects they were taken through are worth not rediscovering:

- **The numerator could not tell a report from tool output.** Every figure above
  is dominated by the delegated report, which is a product of judgment, charged
  in the same units as the raw material the criterion is about.
- **The denominator counted four contexts at once.** The meter was summed across
  rounds, so the system prompt, L0 and L1 were each charged once per round. The
  union of four contexts is not a context, and the inflation moved the ratio in
  the direction that flattered the design. `DecisionOutcome` now carries one
  `ContextComposition` per request and `peak()` is the worst point rather than
  the last one.

The old reading — "the structure holds; the quantity does not" — was a statement
about the ruler as much as about the thing measured. Under the corrected
numerator the criterion is met outright.

### The growth curve

`--- decision-layer context per request: 7941 20317 26070`

One round, three requests: the view, then the view plus the assistant turn, then
the view plus the assistant turn plus the delegation's return. This is the series
DESIGN's boundedness claim has to be checked against, because the last point of an
unbounded run looks exactly like the last point of a bounded one.

The part of it that is not bounded is the evidence section. `build_view` carries
every delegation's evidence for the whole run and each `Evidence.content` is a
tool's output rendered verbatim, so round *N* re-renders round 1's evidence for
the *N*th time — the view grows with the square of the number of rounds. A run
that produces no evidence (as above) hides this completely, which is why the
previous measurement, taken the same way, could not see it.

## The same measurement after the loop was closed

Same fixture, same charter plus one clause — *"record what you conclude before
you answer"* — read-only surface, four rounds allowed:

```
--- 4 round(s), 1 delegation(s) in 127.9s
--- tokens in=25705 out=17047 cache_read=17536
--- L1: 2 decision(s), 2 rejected, 0 open question(s)
--- decision layer context: raw material 0.0% of 29557 chars (raw 0 + reduced 10670 + calls 0 + definitions 2450; tool machinery 44.4%)
--- decision-layer context per request: 10127 23433 31342 29034 29859 29557
--- peak context: 31342 chars, raw material 0.0%
--- executor #0 context: raw material 29.9% of 28170 chars (raw 8427 + reduced 0 + calls 1579 + definitions 502; tool machinery 37.3%)
```

Three things moved, and each is the mechanism rather than the number.

**L1 is no longer empty.** The layer recorded two decisions with their reasons
and ruled two things out. The second rejection is the one worth reading:

> ruled out: A third read-only pass re-deriving why check.py fails. — because Two
> independent passes over identical file contents agree on the same root cause.
> Further reading cannot produce the one thing still missing — an observed run —
> and the charter forbids the change that would make one meaningful. The last pass
> spent 44 tool calls and 20 file reads to reach the conclusion already on the
> ledger; another would spend the same for nothing.

That is the layer doing the thing the design says only it can do: noticing that
its own executor is looping and stopping it, with the evidence for the judgement
written down. Before this milestone it had nowhere to write it.

**The curve is no longer monotonic.** `10127 23433 31342 29034 29859 29557` — it
rises inside a round as the conversation grows, then *falls* when the next round
re-renders, from 31342 to 29034. The peak is round one's, not the last round's.
DESIGN's "大小有界、永不退化" is a claim about this shape, and the shape is now
testable rather than asserted.

What still grows is L1, which is supposed to: the later rounds' fresh views are
larger than the first (10127 → 29034) because the ledger gained four entries. That
is state, not history, and it is the difference the metric exists to keep visible.

**Raw material is still zero.** 0 of 29 557 characters in the decision layer, with
tool machinery at 44.4% — nearly all of it the one reduced report (10 670) and the
tool definitions (2 450). The executor's 29.9% is the contrast the design rests
on, and it is higher than the first run's 8.3% because this charter's executor
read more.

**Harness cost versus model cost**

From the run above — the number DESIGN says flower never managed to take:

```
spawn=18465.6ms   stream=37503.3ms   wire_wait=1777.2ms   first_token=5133.3ms
assemble=5.9ms    journal=8.7ms      reduce=0.1ms         tool_exec=4.4ms   init=4.3ms
```

The harness's own spans (`assemble`, `journal`, `reduce`, `tool_exec`, `init`)
total about 23 ms against 37.5 s of streaming and 18.5 s inside the delegation.
The executor's per-step figure is printed separately by `yu run`, where
`harness_time_ns` is defined as the sum of the non-model spans — meaningful only
now that `ToolExec`, `Reduce` and `Journal` are emitted, since before those
existed it omitted the harness's three commonest costs while presenting the
result as a measurement.

## Quirk measurements

Endpoint capability and the six behavioural quirks are in
[WIRE-FACTS.md](WIRE-FACTS.md), each backed by a probe in `.probe/` and by real
wire captures in `crates/yu-wire/tests/fixtures/`. `yu probe` re-measures the
capability matrix; `yu probe --json > caps.json` writes a profile that
`YU_QUIRKS_JSON=caps.json` reads back, which is what makes "约束靠注入" true rather
than aspirational — and both directions of that pipeline are now guarded, so a
renamed field fails a test instead of silently reverting the run to an unmeasured
profile.

Two facts worth keeping in view, because both make the design *more* dependent on
things it already says:

- **Anthropic-defined schema-less tools are rejected** (422 `unknown variant`).
  Every tool needs an explicit `input_schema`; DESIGN's "白拿训练过的工具纪律"
  does not apply on this endpoint.
- **`cache_control` is inert and prefix caching is automatic.** There is no
  breakpoint to place, so byte-stability of the rendered prefix is the entire
  caching mechanism rather than a hint to it. The probe that reports this now
  sends a breakpoint and compares it against not sending one, instead of
  inferring the answer from the requests that carry none.

## Does the aligned tool surface help? First measurement: no, and it costs more

TODO step 5 asked for the assumption *"形状无所谓"* to be turned into a
measurement that can fail. It was run, and on this task it failed in the
direction nobody was arguing for.

One task, two arms, same model, same sandbox, same `--tools full`, the only
difference being `--tool-profile`:

> In this directory there are exactly three .txt files. Each contains the line
> 'STATUS: pending'. Change that line to 'STATUS: done' in all three, changing
> nothing else in any file. Then report which files you changed.

| arm | tool calls | tokens in | out | cache read | steps |
|---|---|---|---|---|---|
| **A — aligned** (`claude-code-2.1.248`) | `Read`×6 `Edit`×3 `Bash`×4 = 13 | **3742** | 1728 | **19840** | 8 |
| **B — original shapes** | `read_file`×4 `list_dir`×2 `exec`×3 = 9 | **1725** | 1464 | 9984 | 7 |

**Both succeeded**, verified outside the harness: 3 of 3 files changed, 104 bytes
each, zero `pending` left, and arm B's output is byte-identical to arm A's.

**The aligned surface cost about twice the input tokens and half again the tool
calls.** The cause is not subtle: the captured descriptions are long — Claude
Code's `Read` and `Bash` descriptions are several hundred words each — and a tool
description is paid for on **every request**, cached or not. An alignment that
buys in-distribution phrasing at twice the prefix cost is a trade that has to be
shown to pay, and this measurement does not show it.

### The reason TODO gave for `Edit` is false at this endpoint

TODO: *"没有它,每次改动都是整文件重写:既贵又严重偏离分布."* Arm B had no `Edit`
and **did not rewrite any file**. It ran `exec` with `sed`. The executor always
has `exec`, so a missing dedicated tool does not degrade into an expensive
rewrite — it degrades into the shell.

That is the same shape as the `Grep` finding from the capture: the model reaches
for `exec` as the universal fallback. Two separate expectations about what a
missing tool costs were both wrong for the same reason.

### What this does not say

One task, one run per arm. The task was easy and both arms had a one-line shell
answer available, so it cannot separate "the aligned surface is better at edit
semantics" from "this task did not need edit semantics". A task where `Edit`'s
exactness matters — a target string that occurs more than once, or where
whitespace is load-bearing — is the measurement that would, and it has not been
run. Until it is, the honest reading is: **alignment has a measured cost and no
measured benefit.**
