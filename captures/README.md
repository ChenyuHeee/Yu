# Claude Code's tool surface, captured

Raw material for TODO step 2 (抓真形状) and DESIGN's 「工具面:与 Claude Code 对齐」.
One real `claude -p` run was pointed at a local recording reverse proxy, and the
`tools` array and `system` field it sent are committed here, unedited.

The point of the capture is that the shapes are **measured, not remembered**.
Everything below distinguishes what was on the wire from what this document
thinks it means.

```
captures/
  proxy.py                       recording reverse proxy (forwards + records, redacts at ingest)
  to_profile.py                  capture -> profile (mechanical projection)
  verify.py                      the criterion: does the data still say what it said
  claude-code/
    run.sh                       the capture run, reproducible
    drive/                       the working directory the run was pointed at
    raw/                         the capture itself
    profile/claude-code-2.1.248.json
    MANIFEST.json                which request the profile came from, and every sha256
```

## The run

| | |
|---|---|
| client | Claude Code 2.1.248 (`cc_version=2.1.248.600`, `cc_entrypoint=sdk-cli`) |
| invocation | `claude -p "$(cat drive/prompt.txt)" --output-format stream-json --verbose` |
| model | `ANTHROPIC_MODEL` from the environment; the client sent it as `deepseek-flash` |
| endpoint | `ANTHROPIC_BASE_URL`, via `http://127.0.0.1:<port>` (the proxy) |
| environment | rebuilt from scratch by `run.sh` — only endpoint config crosses over |
| session | one session, `bf3ca0bf-…`, three requests |
| turns | 3; the model called `Read` and `Bash` (in one turn) and answered |

`run.sh` rebuilds the environment deliberately. Run from inside another Claude
Code session, inherited variables (`CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_EFFORT`,
the `VSCODE_*` set) would make the child a VS Code-embedded session rather than
a CLI one — the first attempt here did exactly that, and the surface differed
(see "The surface is configuration-dependent" below).

## Files in `raw/`

| file | what it is |
|---|---|
| `requests/0001.body.bin` + `.meta.json` | the session-title request: 0 tools, the small title system prompt |
| `requests/0002.body.bin` + `.meta.json` | **the capture**: the first tool-carrying request — 26 tools, 3 system blocks |
| `requests/0003.body.bin` + `.meta.json` | the same session after the tool results: tools and system byte-identical |
| `proxy.log.jsonl` | one line per request, written by the proxy: path, size, sha256, what was redacted |
| `cc-run.stdout.jsonl` | Claude Code's own stream: its `system:init` event lists the tools it advertised |
| `cc-run.stderr.txt`, `proxy.stdout.txt`, `proxy.stderr.txt` | run noise, kept so nothing about the run is hidden |
| `run-identity.txt` | client version, model, arguments |

The bodies are committed whole (≈165 KB) rather than trimmed: the tools array
and the system prompt *are* the deliverable, and the meta/log/hash files make a
future trim mechanical if size ever matters.

## What makes this trustworthy — and what does not

**Trustworthy:**

* **Recorded before forwarding.** `proxy.py` writes the request to disk before
  it talks to the endpoint, so the capture cannot have been shaped by the
  response. Its `body_sha256_as_received` is computed at that moment.
* **Nothing was rewritten.** Every meta file's `body_sha256_as_received` equals
  its `body_sha256_recorded`; `body_redacted` is false everywhere. The only
  edits are the credential headers, replaced by `****` at ingest and listed in
  `redacted_headers` — `authorization` on all three requests. There is no code
  path in the proxy that writes a header value.
* **Two independent accounts of the tool list.** The wire array in
  `0002.body.bin` and Claude Code's own `system:init` event in
  `cc-run.stdout.jsonl` agree, 26 for 26 (`verify.py` checks this).
* **One session.** All three bodies carry the same `metadata.user_id` session
  id, so they are one run, and all tool-carrying requests agree on tools and
  system (`verify.py` checks both).
* **The check can fail.** `verify.py` recomputes everything from disk.
  Mutating one character of the profile, one byte of a body, or planting a
  key-shaped string makes it exit non-zero — demonstrated, not asserted.

**Not trustworthy — say these out loud:**

* **There is no endpoint-side correlator.** This endpoint returns no
  `request-id` header (`upstream_request_id` is null in every log line —
  measured). The evidence that the bytes went out is the proxy's own log and
  hashes, plus the client's transcript. A reader who trusts neither this proxy
  nor this client has to re-run the capture, which `run.sh` is for.
* **The proxy is part of the evidence chain**, and it is a file in this
  directory, not a third party. It has no reason to lie and no test that could
  have caught it lying; what it does have is the above hashes on both sides of
  the forward.
* **This is one machine, one model, one entrypoint.** Claude Code builds its
  tool list per session and per environment; see the next section. Nothing here
  says what Claude Code sends on a different machine, and the profile is a
  snapshot by construction (DESIGN 约束 3).

## The profile, and what the converter does

`to_profile.py RAW_DIR OUT_JSON MANIFEST_JSON` does three things and nothing
else:

1. picks the first `/v1/messages` request that carries a non-empty `tools`
   array (here: request 2);
2. writes `{"tools": …, "system": …}` — those two fields, copied verbatim.
   Key order, whitespace inside strings, `$schema` keys, required lists: all as
   sent. The only difference from the wire bytes is JSON indentation;
3. writes `MANIFEST.json`, naming the request, the tool names, and the sha256
   of every file involved, so the projection can be re-derived and compared
   instead of believed.

Re-derive it any time:

```sh
python3 captures/to_profile.py captures/claude-code/raw \
    captures/claude-code/profile/claude-code-2.1.248.json \
    captures/claude-code/MANIFEST.json
python3 captures/verify.py captures/claude-code/raw \
    captures/claude-code/profile/claude-code-2.1.248.json \
    captures/claude-code/MANIFEST.json --scan captures
```

## The 26 tools

`Agent Bash CronCreate CronDelete CronList DesignSync Edit EnterWorktree
ExitWorktree ListAgents NotebookEdit Read ReportFindings ScheduleWakeup
SendMessage Skill TaskCreate TaskGet TaskList TaskOutput TaskStop TaskUpdate
WebFetch WebSearch Workflow Write`

Each is exactly `{name, description, input_schema}` — no `strict`, no `type`,
no `cache_control`. The schemas are JSON Schema draft 2020-12 with
`additionalProperties: false`.

Which of them this harness can back with a real implementation
(`crates/yu-tools` today has `read_file`, `write_file`, `list_dir`, `exec`):

| Tool | Verdict |
|---|---|
| `Read`, `Write`, `Edit`, `Bash` | **implementable** — the shapes match what `yu-tools` already has; `Edit` is TODO step 4; `Bash` needs `run_in_background` and `dangerouslyDisableSandbox` backed by real behaviour before its description may be copied |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate` | **implementable** — a session-scoped task store; this is the modern `TodoWrite` |
| `TaskOutput`, `TaskStop` | not yet — a background-task registry; `TaskOutput`'s own description marks it DEPRECATED |
| `EnterWorktree`, `ExitWorktree` | not yet — needs a session-scoped working directory and git worktree lifecycle |
| `Agent`, `SendMessage`, `ListAgents` | not yet — a subagent/messaging runtime; this harness puts delegation at the decision layer, and the description's contract (agent types from `.claude/agents/*.md`, teammates, cloud sessions) is Claude Code's own |
| `Skill` | not yet — a skills registry and resolution |
| `CronCreate`, `CronDelete`, `CronList`, `ScheduleWakeup` | not yet — durable scheduling (`/loop`, `.claude/scheduled_tasks.json`) |
| `NotebookEdit` | not yet — needs `.ipynb`-aware `Read` and cell semantics |
| `WebFetch`, `WebSearch` | not yet — need a fetch/search backend; the endpoint's server-side `web_search` is a different wire shape (no `input_schema`) and cannot stand in for this face |
| `DesignSync`, `ReportFindings`, `Workflow` | **cannot** — they address Claude Code's own product surfaces (claude.ai design projects, the host review UI, cloud workflow runs) |

TODO's rule applies to every "not yet": 不提供的,就不要放进去. Copying the
description without the implementation is the failure mode to avoid — and the
descriptions are load-bearing, e.g. `Edit`'s "You must Read the file in this
conversation before editing, or the call will fail" and `Write`'s "Overwriting
an existing file you haven't Read will fail" are claims about client-side
state this harness does not currently keep.

## What the real shapes contradict

1. **`Grep` and `Glob` do not exist in this surface.** 2.1.248 advertises
   neither, so TODO step 4's 「搜索(Grep / Glob)」 has no shape to copy from
   this capture, and the premise that 「被训练的 Bash 描述里明确劝阻」 grep in
   Bash is not in these bytes: the Bash description's avoid-list is
   `cat`, `head`, `tail`, `sed`, `awk`, `echo` — grep is not on it. In the run,
   the model reached for a `Grep` tool, found none, and used `Bash` with `grep`,
   which is the sanctioned path here.
2. **`TodoWrite` is not in the default surface either.** With defaults,
   `TaskCreate/TaskGet/TaskList/TaskUpdate` replace it. `TodoWrite` appears
   only when `CLAUDE_CODE_ENABLE_TASKS=0`: measured in both directions — the
   variant swaps the four `Task*` tools for `TodoWrite` and changes nothing
   else. TODO's 「抓包里会有 TodoWrite」 holds only under that flag.
3. **`Task` is the client's internal name for the wire tool `Agent`.** Claude
   Code's own `system:init` event says `Task`; the wire array says `Agent`.
   DESIGN and TODO both say 「Task 工具」 — the name on the wire is `Agent`.
4. **The request envelope** (worth knowing before step 3's test): model name
   rewritten by the client (`deepseek-flash[1m]` → `deepseek-flash`),
   `max_tokens: 32000`, `thinking: {type: "adaptive"}`,
   `context_management.edits: [{type: "clear_thinking_20251015", keep: "all"}]`,
   `output_config.effort: "high"`, `stream: true`. `system` is three blocks:
   `[0]` a billing header text block (`x-anthropic-billing-header: cc_version=…
   ; cc_entrypoint=sdk-cli;`), `[1]` the 62-char SDK preamble, `[2]` the ~7.5 KB
   system prompt — blocks 1 and 2 carry `cache_control`. This is the "扒 prompt"
   material, in the same package as the tools.
5. **`redact_key_material` eats prose.** Applying it to captured text turned
   Claude Code's ExitWorktree description "the tool is a `**no-op**`" into
   "the tool is a `****`" — its mask-tail rule strips runs of up to
   `MAX_MASKED_SUFFIX` (8) key characters after `**`, and `no-op` is five. That
   is why `proxy.py` redacts by exact credential value at ingest and applies no
   mask-tail pass to anything a client wrote. The same hazard applies anywhere
   else in this project that runs that function over prose rather than over
   endpoint error text.
6. **`AdditionalProperties: false` + `$schema`** on every tool schema: if
   step 3's profile loader re-serializes schemas through a Rust type that drops
   `$schema` or reorders keys, "逐字段相同" will fail on this capture — which is
   the point, but it should be expected rather than discovered.

## The surface is configuration-dependent

Measured, same client version, same endpoint, same model:

| environment | surface |
|---|---|
| plain CLI (`run.sh`) | 26 tools, including `TaskCreate/TaskGet/TaskList/TaskUpdate`, no `TodoWrite` |
| `CLAUDE_CODE_ENABLE_TASKS=0` | 23 tools: the four `Task*` tools are replaced by `TodoWrite`; nothing else changes |
| inherited from a VS Code session | same as the line above, plus `effort: "max"` from `CLAUDE_EFFORT` |

Only the first is committed. The variant is reproducible without touching the
committed capture:

```sh
OUT=$PWD/.yu/raw-tasks-disabled CLAUDE_CODE_ENABLE_TASKS=0 \
  bash captures/claude-code/run.sh
```

Which of these is "the" surface to align with is a design decision, not a
measurement — the capture's job is to keep it from being made by memory.
