# Embedding the crates

The workspace is nine crates. This page covers what each one exposes, which
direction dependencies run, and where the extension points are.

## Crate graph

```
yu-cli  ──►  yu-workflow  ──►  yu-core  ──►  yu-wire
                                  │           yu-journal
                                  │           yu-tools
                                  │           yu-observe
yu-server ──►  yu-core
yu-policy ──►  injects into yu-core's traits
```

| Crate | Responsibility |
|---|---|
| `yu-wire` | Messages API: types, SSE, retries, error classification, quirks, token counting |
| `yu-journal` | The record: typed events, index, addressable reads, effects, replay, fork |
| `yu-core` | Loop, agent lifecycle, rendering, topology, scheduling, cancellation, the parallel gate |
| `yu-tools` | Native tools, result typing, reduction |
| `yu-policy` | Declarative policy resources and the script host |
| `yu-observe` | Spans, metrics, allocation attribution, shape signals |
| `yu-server` | Daemon and embedded dual mode |
| `yu-workflow` | Generation change, handover, human interaction |
| `yu-cli` | Front end and short-path bypass |

**Dependency direction is one-way and enforced by review, not by the compiler.**
`yu-observe` is a leaf so the wire layer can time itself without a cycle.
`yu-core` defines traits; `yu-policy` injects implementations; `yu-core` does not
depend on `yu-policy`.

## Extension points

Two traits are the intended seams.

### `yu_core::traits::PolicySource`

```rust
pub trait PolicySource {
    fn prompt(&self, name: &str) -> Option<String>;
    fn budget(&self, name: &str) -> Option<u64>;
    fn rule(&self, name: &str) -> Option<serde_json::Value>;
}
```

Implemented by `yu_policy::resource::Policy`. Implement it yourself to supply
policy from somewhere other than a TOML file.

### `yu_tools::reduce::ReductionTable`

Implemented by `Policy` for `[[reduction]]` rows. Implement it to choose
reduction paths in code.

### `yu_tools::Tool`

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult;
    fn effect(&self, _input: &Value) -> Option<EffectIntent> { None }
    fn effect_in_place(&self, _input: &Value, _ctx: &ToolContext) -> EffectVerdict {
        EffectVerdict::Unknown
    }
}
```

Register with `ToolRegistry::register`. Declaring an effect is what puts the
tool under the record's intent/completion discipline; a tool that declares none
is not replay-protected.

## Key entry points

### `yu-wire`

```rust
let client = WireClient::from_env()?;                  // env-driven
let client = WireClient::builder()
    .base_url("https://api.anthropic.com")
    .auth(Auth::ApiKey(key))
    .quirks(Quirks::anthropic())
    .build()?;

let outcome = client.stream(&request, &mut sink).await?;
let response = client.send(&request).await?;
let tokens = client.count_tokens(&count_request).await?;
```

`stream` forces `stream: true` and retries only while nothing has been emitted.
The sink is `StreamSink`, whose methods all have defaults. `Capabilities::probe`
backs `yu probe`.

### `yu-journal`

```rust
let journal = Journal::open(dir, SessionId::new("delivery-1")).await?;
let event = journal.append(agent_id, EventBody::Note { text })?;

journal.read_from(10)?;        // seq >= 10, to the end
journal.read_range(10, 20)?;   // [10, 20)

journal.replay_seed()?;        // committed effects
journal.unresolved_effects()?; // intended, never completed
journal.estimate_fork(20)?;
Journal::open_fork(dir, session, Arc::new(parent), 20).await?;
```

`open` rebuilds the index, repairs a torn tail, and re-attaches a parent when
the first event is `forked`.

### `yu-core`

```rust
let mut layer = DecisionLayer::new(client, journal, tools, timeline, config, charter, cwd);
let outcome = layer.run().await?;

let resumed = Resumed::read(&journal, &agent_id)?;
let mut layer = DecisionLayer::resume(resumed);
```

`DecisionConfig::new(model)` defaults to `max_tokens: 8192`, `effort: Xhigh`,
`max_rounds: 8`, and one delegation per round. The CLI overrides the round
count.

`DecisionOutcome` carries `rounds`, `delegations`, `final_text`, `usage`,
`composition`, `contexts`, `ledger`, `stopped_early`, and `outcomes`, with
helpers `raw_material_ratio()`, `tool_machinery_ratio()`,
`landing_distribution()`, and `landing_problems()`.

The execution-layer loop is `yu_core::agent::{Agent, AgentConfig}`; it is not
re-exported at the crate root.

### `yu-policy`

```rust
let policy = Policy::from_path(Path::new("policy.toml"))?;
policy.unknown_reduction_axes();   // dead rows
policy.unknown_reduction_paths();
```

The tier-2 scripting surface is `ScriptHost`, with `RhaiHost` implemented.
`WasmtimeHost` is declared and returns an unsupported error.

### `yu-observe`

```rust
let timeline = Timeline::new();
let timer = timeline.start(SpanKind::ToolExec);
// …
timer.finish();

let mut metrics = Metrics::default();
metrics.observe_spans(&timeline.spans());
metrics.harness_time_per_step_ms();
```

`Allocator` is the global allocator type; the CLI installs it.

### `yu-server`

```rust
pub enum Host {
    Embedded,
    LocalDaemon { socket: PathBuf },
    Remote { endpoint: String },
}

pub struct ServerConfig { pub host: Host, pub state_dir: PathBuf }

serve(listener, &host, config).await?;
```

!!! warning "`Embedded` and `Remote` are configuration values, not transports"

    The crate implements `SessionHost` for two types: `Daemon`, which owns
    journals, and `DirectoryHost`, which is metadata only. **Nothing implements
    an in-process transport for `Host::Embedded`, and nothing implements
    `Host::Remote`.**

    To embed, drive `Daemon` directly in-process and skip the socket, or
    implement `SessionHost` yourself and call `serve`. Setting `Host::Embedded`
    in a config does not by itself avoid the socket.

## The daemon as a library

```rust
let daemon = Daemon::open(root)?;
let live = daemon.start("charter text")?;
live.record("human words")?;      // appends a user_message
live.unresolved()?;
live.history()?;
```

`Live::record` returns the sequence number it wrote. It does not run a turn.

For a client of the socket protocol, use `yu_server::session::{Request,
Response, encode_line, decode_line}` over a `tokio::net::UnixStream`. The
protocol is documented in [`yu daemon`](../commands/daemon.md#protocol).

## Generation change

`yu_workflow::handover` holds the handover machinery: `Handover::assemble`,
`Handover::check`, `Verifier::find_gaps`, `Trigger::should_fire`, the
`ReaderModel` trait, and `NewSession::{new_session, resume_into}`. `WireReader`
is the `ReaderModel` implementation backed by a `WireClient`.

## Stability

The workspace version is `0.0.1`. Nothing here is published to crates.io, and no
API is frozen. The dependency direction and the two trait seams are the parts
that are treated as load-bearing.
