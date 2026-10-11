# Installation

## Requirements

| Item | Requirement |
|---|---|
| Rust | 1.85 or later — the workspace uses edition 2024 |
| Network | Reachable Messages API endpoint |
| Credential | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` |

There is no published binary and no package on crates.io. Build from source.

## Build

```sh
git clone https://github.com/ChenyuHeee/Yu.git
cd Yu
cargo build --release
```

The binary is `target/release/yu`.

```sh
./target/release/yu --version
```

To install it on `PATH`:

```sh
cargo install --path crates/yu-cli
```

!!! note "Release profile"

    The release profile sets `lto = "thin"`, `codegen-units = 1`, and
    `debug = 1`. Building in release mode matters if per-step overhead is what
    you are measuring; a debug build reports the debug build's overhead.

## Verify the installation

Confirm the binary runs and the endpoint answers. This step costs one model
request.

```sh
export ANTHROPIC_API_KEY=sk-...
yu exec -p "reply ok"
```

Expected: the model's reply on stdout, exit status `0`.

If no credential is set, the client fails with:

```
no credential: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN
```

## Measure the endpoint before trusting it

`yu probe` asks the endpoint which fields it actually honours. Run it once
against any endpoint that is not Anthropic's; the result decides which features
the rest of the manual applies to.

```sh
yu probe
```

See [`yu probe`](commands/probe.md) and
[Endpoint compatibility](integration/endpoint.md).

## Upgrading

`yu` writes records to disk. Records are read back by `yu metrics`, `yu fork`,
and session continuation. A record written by an older build decodes under a
newer one as long as the event envelope is unchanged; `yu metrics` skips any
line it cannot parse rather than failing. See [Record format](reference/record.md).
