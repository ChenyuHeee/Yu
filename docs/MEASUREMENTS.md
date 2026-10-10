# Measurements

Numbers this project produced by running, kept because they are the reason to
believe or disbelieve any of it. Where a claim and a measurement disagree, the
measurement is the finding.

Re-measure any of these with `yu decide` on a fresh directory.

---

## The decision layer's context composition

DESIGN's acceptance criterion for the whole architecture:

> 决策层跑完一个真实任务后,它上下文里**工具机制的占比**是多少?
> 接近零 → **设计成立**;超过一两成 → **前提有问题**。

Same charter, same task (`check.py` fails; explain why, change nothing),
read-only tool surface, four runs:

| run | tool machinery | what changed |
|---|---|---|
| 1 | **65.9%** | initial — `delegate` returned the executor's full essay (≈34 000 chars) |
| 2 | 48.4% | report put under `REPORT_BUDGET_CHARS` (2 500) with a pointer to the rest |
| 3 | **28.8%** | one delegation per round, so each round is truly "L0 + L1 + 本轮报告" |
| 4 | 36.8% | run-to-run variance; the ratio tracks how many delegations the layer chose |

**Verdict: the structure holds; the quantity does not.**

DESIGN's isolation claim is sound and was verified: no tool stream and no
transcript ever reaches the decision layer — only a report, a manifest of what
was left out, and deterministic shape counts. But "趋近于零" is not reached at
~30–50%, and DESIGN's own threshold of "一两成" is exceeded.

The residue is the layer's own `delegate` arguments (~5 500 chars) and the one
report it asked for (~4 800), not a flood of tool output. That distinction is
measured rather than argued: if the layer were drowning, halving the number of
delegations would not have moved the ratio twenty points.

Reaching DESIGN's number would need the layer to carry *decisions* forward in L1
rather than reports, which needs `decide` and `retire` as tools rather than the
one tool the thinnest slice builds.

## Harness cost versus model cost

From `yu run`, and the number DESIGN says flower never managed to take:

```
spans: assemble=0.2ms stream=2089.8ms wire_wait=592.3ms
harness=0.1ms/step   model_time=2682ms
```

The harness is ~0.007% of the run. This is only meaningful now that
`ToolExec`/`Reduce`/`Journal` are emitted: `harness_time_ns` is defined as the sum
of the non-model spans, so before those existed it omitted the harness's three
commonest costs while presenting the result as a measurement.

## What a refusal costs

`yu decide` reports both layers from one run, which is what DESIGN's criterion
compares:

```
decision layer context: tool machinery 28.8% of 33766 chars
executor #0 context:    tool machinery 13.7% of 11432 chars
```

The executor's ratio is *lower* than the decision layer's in the read-only case,
because a reader that only opens files produces little machinery. It is not lower
when the executor runs commands — run 1's executors reached 57.0%.

## Quirk measurements

Endpoint capability and the six behavioural quirks are in
[WIRE-FACTS.md](WIRE-FACTS.md), each backed by a probe in `.probe/` and by real
wire captures in `crates/yu-wire/tests/fixtures/`. `yu probe` re-measures the
capability matrix; `yu probe --json > caps.json` writes a profile that
`YU_QUIRKS_JSON=caps.json` reads back, which is what makes "约束靠注入" true rather
than aspirational.

Two facts worth keeping in view, because both make the design *more* dependent on
things it already says:

- **Anthropic-defined schema-less tools are rejected** (422 `unknown variant`).
  Every tool needs an explicit `input_schema`; DESIGN's "白拿训练过的工具纪律"
  does not apply on this endpoint.
- **`cache_control` is inert and prefix caching is automatic.** There is no
  breakpoint to place, so byte-stability of the rendered prefix is the entire
  caching mechanism rather than a hint to it.
