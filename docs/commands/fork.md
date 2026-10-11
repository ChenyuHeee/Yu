# `yu fork`

Fork a session's record at a historical point, showing what it costs.

## Synopsis

```
yu fork --session <SESSION> --at <SEQ> --into <NEW> [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--session <SESSION>` | required | The session whose record is branched from |
| `--at <SEQ>` | required | The sequence number to branch at |
| `--into <NEW>` | required | The new branch's session id |
| `--dir <DIR>` | `.yu` | Where the journals live |

## What it does

Creates a new session whose history is the parent's, up to and including `--at`.
Events below the fork point are **inherited by reference** — they are not copied,
so the new branch costs one marker line regardless of how long the parent is.
The parent file is not written to.

The estimate is printed **before** anything is written, because two of its
numbers are the decision.

## Output

```
--- fork estimate
      forking delivery-1 at seq 20: 41 event(s) / 16817 bytes inherited by
      reference; the cache prefix diverges there, so a replay of this branch
      re-sends the prefix the last recorded request measured at 3324 input
      tokens; the fork itself costs 156 byte(s)
      the parent is not written to: the branch reads its history through it
```

| Number | Meaning |
|---|---|
| `event(s)` | Events the branch inherits |
| `bytes` | Their size, inherited by reference and not rewritten |
| `input tokens` | What a replay must re-send, because the prefix stops matching |
| `marker byte(s)` | What the fork actually writes |

When no request was recorded at or before the fork point, the re-send cost is
not knowable from the record and the estimate says so rather than showing a
plausible number:

```
      … the cache prefix diverges there, so no request was recorded at or before
      the point, so the re-send cost of the prefix is not known from the record;
      the fork itself costs 156 byte(s)
```

After the fork:

```
--- forked gen2 at seq 20: 41 event(s) inherited by reference, 156 byte(s) written
--- cache prefix diverges at delivery-1:20: requests on this branch from that point onward match nothing cached
--- parent delivery-1 still holds 47 event(s), whole
```

!!! warning "A fork is not free"

    Everything below the fork point is shared, but from the fork point onward the
    cache prefix no longer matches: each request on the branch re-sends what the
    parent had cached. The estimate exists so that this is a decision rather than
    a surprise. Forking is a deliberate operation, not a debug button.

## Exit status

| Status | Condition |
|---|---|
| `0` | The branch was created |
| `1` | `--at` is past the end of the parent's own events, the target name has a record already, or the parent cannot be opened |

!!! note "`--at` counts the branch's own events"

    On a record that is itself a fork, `--at` is bounded by the events in that
    branch's own file, not by the whole lineage.

## Examples

Branch a session at sequence 20 into a new name:

```sh
yu fork --dir .yu --session delivery-1 --at 20 --into gen2
```

Then continue the branch:

```sh
yu decide --dir .yu --session gen2
```
