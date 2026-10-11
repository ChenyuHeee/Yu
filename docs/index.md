# Yu — CLI and Developer Manual

`yu` is an agent harness with a decision layer. The decision layer holds the
human's intent and the run's state; execution is delegated to disposable agents
that are told what they need and are not expected to remember anything.

This manual documents the command surface, the files it reads and writes, and
the interfaces an integrator builds against. It does not document the design
rationale; that lives in the repository.

## Scope

| Audience | Start at |
|---|---|
| Operating `yu` from a shell | [Installation](install.md) → [Commands](commands/index.md) |
| Writing policy or reading records | [Policy file](reference/policy.md) · [Record format](reference/record.md) |
| Building against the crates or the socket | [Embedding the crates](integration/embedding.md) |
| Pointing `yu` at a non-Anthropic endpoint | [Endpoint compatibility](integration/endpoint.md) |

## Command map

| Command | Purpose | Calls a model | Writes a record |
|---|---|---|---|
| [`yu probe`](commands/probe.md) | Measure what the endpoint supports | yes | no |
| [`yu exec`](commands/exec.md) | One request, straight to the wire | yes | no |
| [`yu run`](commands/run.md) | One task through the execution loop | yes | yes |
| [`yu decide`](commands/decide.md) | The decision layer on a charter | yes | yes |
| [`yu metrics`](commands/metrics.md) | Summarise a recorded session | no | no |
| [`yu fork`](commands/fork.md) | Branch a record at a historical point | no | marker only |
| [`yu daemon`](commands/daemon.md) | Serve sessions over a Unix socket | no | yes |

`yu exec` and `yu run` are the two ends of the same axis. `exec` is the short
path: one request, no session, no journal, no tools. `run` is the long path: it
stands up a session and writes a record, which is exactly the cost `exec` exists
to avoid.

## Minimum viable run

1. Build the binary. See [Installation](install.md).
2. Export a credential.

    ```sh
    export ANTHROPIC_API_KEY=sk-...
    ```

3. Confirm the endpoint answers.

    ```sh
    yu exec -p "reply ok"
    ```

4. Run one task with no side effects. The `read-only` tool surface is the
    default and cannot change anything.

    ```sh
    yu run --task "count the Rust source files under crates/" --cwd .
    ```

5. Read back what was recorded.

    ```sh
    yu metrics --dir .yu
    ```

!!! warning "`--tools full` adds `exec`"

    The `full` tool surface adds `write_file` and `exec`. `exec` runs `sh -c`
    with no sandbox, no timeout, and no path restriction, under the environment
    of the calling process. It can do anything the process can. See
    [Tools](reference/tools.md).

## Conventions used in this manual

- `$VAR` denotes an environment variable. See [Configuration](configuration.md).
- Values shown in `--flag <VALUE>` form are literal, including the case.
- A path like `.yu/` is relative to the process working directory unless stated
  otherwise.
- Exit status is `0` on success and `1` on any runtime error. Argument-parsing
  failures exit `2`. See [Exit status](commands/index.md#exit-status).
