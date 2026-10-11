# `yu metrics`

Summarise a recorded session.

## Synopsis

```
yu metrics [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--dir <DIR>` | `.yu` | The directory holding the journals |

## What it does

Reads every `.jsonl` file directly under `--dir` and prints a per-file summary to
stdout. It calls no model, opens no journal through the library, and never reads
the blob store — it parses each line as JSON and aggregates.

```
delivery-1
  events            102
  responses         12
  tool calls        31
  tool results      31  (inline 20481B, spilled 118204B)
  tokens            in=84021 out=9887 cache_read=40213
  cache hit rate    32.4%
  thinking blocks   12 (signatures kept separately: 12)
  assistant texts   12 recorded (the turn's own words, as blobs)
  reductions        {"SpillWithPointer": 4, "PassThrough": 27}
```

| Field | Meaning |
|---|---|
| `events` | Lines that parsed as events |
| `responses` | Model responses recorded |
| `tool calls` / `tool results` | Calls issued, and results returned |
| `inline` / `spilled` | Bytes kept inside the record, and bytes written to blobs |
| `tokens` | Summed over the session's responses |
| `cache hit rate` | `cache_read / (input + cache_read)`. Printed only when the denominator is non-zero |
| `thinking blocks` | Thinking blocks, and how many carried a separately stored signature |
| `assistant texts` | Turns whose own words were stored as blobs |
| `reductions` | How many results took each reduction path. Printed only when non-empty |

Files are listed in sorted order. Lines that fail to parse are skipped rather
than reported, so a record with a torn final line still summarises.

## Exit status

| Status | Condition |
|---|---|
| `0` | At least one journal was summarised |
| `1` | `no journals in <dir>` |

## Limitations

!!! warning "Metrics reads top-level files only"

    `yu metrics --dir .yu` reads `*.jsonl` directly under `.yu`. It does not
    descend into subdirectories.

    This matters for `yu daemon`, whose sessions live one directory deeper, at
    `.yu/<session>/<session>.jsonl`. Those sessions are **not** summarised by
    this command. See [Record format](../reference/record.md#two-layouts).

!!! note "Blobs are counted, not read"

    `spilled` totals come from the `bytes` field recorded on each tool result.
    The blobs themselves are not opened, so a missing blob does not affect the
    summary.

## Examples

```sh
yu metrics --dir .yu
```

```sh
# After a daemon has been serving sessions.
yu metrics --dir .yu/s1-1759000000000-1
```
