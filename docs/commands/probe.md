# `yu probe`

Measure what the endpoint actually supports.

## Synopsis

```
yu probe [OPTIONS]
```

## Options

| Option | Default | Meaning |
|---|---|---|
| `--json` | off | Emit the report as JSON instead of a table |

## What it does

Sends a series of requests to the configured endpoint and records which fields
it honours, which it accepts and ignores, and which it rejects. Writes nothing
to disk and opens no session.

This matters because an endpoint that *accepts* a field and ignores it is
indistinguishable, from the client's side, from one that implements it. Without
a probe, `yu` would believe it has a feature the endpoint does not have.

Run it against any endpoint that is not Anthropic's before trusting anything
else in this manual.

## Output

A table on stdout. Each row is one capability:

```
================================================================================================
  CAPABILITIES  https://api.deepseek.com/anthropic  model=deepseek-flash[1m]
================================================================================================
  <support>  <feature>                                      <status>  <detail>
```

Followed by a section naming the design assumptions the endpoint does not
support. The `accepts:` line under a rejected feature enumerates the variants
the endpoint does model — the cheapest possible capability probe, since the
endpoint reports its own vocabulary in the error.

## Saving the result

The JSON report is the input to the quirks profile. Save it and point `yu` at
it so that every later run is constrained by what was measured rather than by a
guess:

```sh
yu probe --json > caps.json
export YU_QUIRKS_JSON=$PWD/caps.json
```

See [Endpoint profile](../configuration.md#endpoint-profile-quirks).

!!! note "The table and the JSON are the same report"

    `--json` changes the encoding, not the measurement. Both cost the same
    requests.

## Exit status

| Status | Condition |
|---|---|
| `0` | The probe completed |
| `1` | No credential, unreachable endpoint, or a transport error |
