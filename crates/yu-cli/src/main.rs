//! Yu — front end and short-path bypass.
//!
//! DESIGN: "每一个长程机制都应该是『压力』的函数,不是『轮数』或『时钟』的函数."
//!
//! ## The bypass is the point
//!
//! > **需要一条真正的旁路,不是退化模式** —— Kimi `-p` 故意绕开整个 harness;
//! > opencode 的 minimal 模式仍走 server + SQLite,仍在付钱
//!
//! So `yu exec` shares no machinery with `yu run`: no journal, no session, no
//! reduction, no topology, no policy. It opens one connection, sends one
//! request, and exits. Everything it *does* share — the wire types, the SSE
//! parser, the quirk checks — is a library call, not a facility it stands up.
//! If `exec` ever needs to create a directory, read a policy, or write a record,
//! this has stopped being a bypass.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use yu_core::agent::{Agent, AgentConfig};
use yu_journal::{EventBody, Journal, SessionId};
use yu_observe::{AgentId, Timeline};
use yu_tools::ToolRegistry;
use yu_wire::capability::Capabilities;
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::types::{Message, MessagesRequest, Thinking, ThinkingDisplay};

#[global_allocator]
static ALLOC: yu_observe::Allocator = yu_observe::Allocator;

#[derive(Parser, Debug)]
#[command(name = "yu", version, about = "A decision-layer agent harness.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Measure what the endpoint actually supports.
    ///
    /// DESIGN's "API 事实" is a specification, not a description. Against any
    /// endpoint but Anthropic's it is a hypothesis, and a server that accepts a
    /// field and ignores it will happily let the harness believe it has a
    /// feature it does not have. This asks.
    Probe {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },

    /// One request, straight to the wire. The short-path bypass.
    ///
    /// Deliberately does not touch sessions, journals, tools, or policy.
    Exec {
        /// The prompt.
        #[arg(short, long)]
        prompt: String,
        /// Model to use. Defaults to $ANTHROPIC_MODEL.
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long, default_value_t = 4096)]
        max_tokens: u32,
        /// Show the model's reasoning summary as it streams.
        #[arg(long)]
        show_thinking: bool,
        /// Report timings to stderr.
        #[arg(long)]
        timing: bool,
        /// Print the assembled content blocks as JSON instead of streaming text.
        #[arg(long)]
        json: bool,
    },
    /// Run a task through the loop: journal, tools, reduction, shape metrics.
    ///
    /// This is the long path. Unlike `exec`, it stands up a session and writes a
    /// record — which is exactly the cost `exec` exists to avoid.
    Run {
        /// The task.
        #[arg(short, long)]
        task: String,
        /// Directory for journals and blobs.
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
        /// The directory the tools operate in.
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long, default_value_t = 12)]
        max_steps: u32,
    },

    /// Summarise a recorded session.
    Metrics {
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Probe { json } => cmd_probe(json).await,
        Command::Exec {
            prompt,
            model,
            max_tokens,
            show_thinking,
            timing,
            json,
        } => cmd_exec(&prompt, model, max_tokens, show_thinking, timing, json).await,
        Command::Run {
            task,
            dir,
            cwd,
            model,
            max_steps,
        } => cmd_run(&task, &dir, cwd, model, max_steps).await,
        Command::Metrics { dir } => cmd_metrics(&dir).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("yu: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// probe
// ---------------------------------------------------------------------------

async fn cmd_probe(json: bool) -> anyhow::Result<()> {
    let caps = Capabilities::probe_from_env().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&caps)?);
    } else {
        print!("{caps}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// exec — the bypass
// ---------------------------------------------------------------------------

/// Streams to stdout as tokens arrive.
struct StdoutSink {
    show_thinking: bool,
    thinking_open: bool,
    tool_calls: Vec<String>,
}

impl StreamSink for StdoutSink {
    fn on_thinking(&mut self, text: &str) {
        if !self.show_thinking {
            return;
        }
        if !self.thinking_open {
            eprint!("\x1b[2m[thinking] ");
            self.thinking_open = true;
        }
        eprint!("{text}");
        let _ = std::io::stderr().flush();
    }

    fn on_text(&mut self, text: &str) {
        if self.thinking_open {
            eprintln!("\x1b[0m");
            self.thinking_open = false;
        }
        print!("{text}");
        let _ = std::io::stdout().flush();
    }

    fn on_tool_use(&mut self, id: &str, name: &str) {
        self.tool_calls.push(format!("{name}({id})"));
    }
}

async fn cmd_exec(
    prompt: &str,
    model: Option<String>,
    max_tokens: u32,
    show_thinking: bool,
    timing: bool,
    as_json: bool,
) -> anyhow::Result<()> {
    let client = WireClient::from_env()?;
    let model = model.unwrap_or_else(|| client.default_model().to_owned());

    let req = MessagesRequest::new(model, max_tokens, vec![Message::user(prompt)])
        // Adaptive is the only on-mode on current models, and the measured
        // endpoint runs adaptive even when the field is omitted.
        .thinking(Thinking::adaptive_with(ThinkingDisplay::Summarized))
        .streaming();

    let mut sink = StdoutSink {
        show_thinking,
        thinking_open: false,
        tool_calls: Vec::new(),
    };

    let outcome = client.stream(&req, &mut sink).await?;
    if sink.thinking_open {
        eprintln!("\x1b[0m");
    }

    if as_json {
        println!("{}", serde_json::to_string_pretty(&outcome.message)?);
    }

    if timing {
        let t = &outcome.timing;
        let ms = |ns: Option<u64>| ns.map(|n| n as f64 / 1e6);
        eprintln!(
            "\n--- stop={:?} in={} out={} cache_read={} attempts={}\n\
             --- wire_wait={:.1}ms first_token={:.1}ms stream={:.1}ms events={} text={}B signature={}B",
            outcome.message.stop_reason,
            outcome.message.usage.input_tokens,
            outcome.message.usage.output_tokens,
            outcome.message.usage.cache_read_input_tokens,
            outcome.attempts,
            ms(t.first_byte_ns).unwrap_or(f64::NAN),
            ms(t.first_token_ns).unwrap_or(f64::NAN),
            t.total_ns as f64 / 1e6,
            t.events,
            t.text_bytes,
            t.signature_bytes,
        );
        if !sink.tool_calls.is_empty() {
            eprintln!("--- tool calls: {}", sink.tool_calls.join(", "));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run — the long path
// ---------------------------------------------------------------------------

async fn cmd_run(
    task: &str,
    dir: &PathBuf,
    cwd: Option<PathBuf>,
    model: Option<String>,
    max_steps: u32,
) -> anyhow::Result<()> {
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let timeline = Timeline::new();
    timeline.enter(AgentId::new("exec"));
    // The client must share the timeline, or the wire spans are never recorded
    // and the harness-overhead metric reports zero model time.
    let client = Arc::new(WireClient::from_env()?.with_timeline(timeline.clone()));
    let model = model.unwrap_or_else(|| client.default_model().to_owned());

    // Session id from the wall clock: a record has to be findable later, and
    // this is the only thing about it that is knowable at open time.
    let session = SessionId::new(format!(
        "run-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S")
    ));
    let journal = Arc::new(Journal::open(dir, session.clone()).await?);

    journal
        .append(
            AgentId::new("exec"),
            EventBody::SessionStarted {
                charter: task.to_owned(),
                model: model.clone(),
                quirks: client.quirks().name.clone(),
            },
        )
        .await?;

    let tools = Arc::new(ToolRegistry::native(cwd.clone()));
    let config = AgentConfig::new(AgentId::new("exec"), model).max_steps(max_steps);
    let mut agent = Agent::new(
        client.clone(),
        journal.clone(),
        tools,
        timeline.clone(),
        config,
        cwd,
    );

    eprintln!(
        "session {} — journal in {}",
        session.as_str(),
        dir.display()
    );

    let started = std::time::Instant::now();
    let outcome = agent.run(task).await?;
    let elapsed = started.elapsed();

    println!("{}", outcome.text);

    // DESIGN's headline number: harness cost per step with model time removed.
    let mut metrics = yu_observe::Metrics::default();
    metrics.observe_spans(&timeline.spans());
    metrics.steps = outcome.steps as u64;
    metrics.tool_calls = outcome.tool_calls as u64;
    metrics.usage = outcome.usage;

    eprintln!(
        "\n--- {} step(s), {} tool call(s), {} reduction(s) in {:.1}s",
        outcome.steps,
        outcome.tool_calls,
        outcome.reductions.len(),
        elapsed.as_secs_f64()
    );
    eprintln!(
        "--- tokens in={} out={} cache_read={} | harness={:.1}ms/step model_time={:.0}ms",
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
        outcome.usage.cache_read_input_tokens,
        metrics.harness_time_per_step_ms().unwrap_or(f64::NAN),
        metrics.model_time_ns() as f64 / 1e6,
    );
    // DESIGN wants this breakdown visible from day one: it is the only way to
    // tell harness cost from model cost.
    if !metrics.elapsed_ns_by_kind.is_empty() {
        let mut parts: Vec<String> = metrics
            .elapsed_ns_by_kind
            .iter()
            .map(|(k, v)| format!("{k}={:.1}ms", *v as f64 / 1e6))
            .collect();
        parts.sort();
        eprintln!("--- spans: {}", parts.join(" "));
    } else {
        eprintln!("--- spans: NONE RECORDED (observation is not wired up)");
    }
    if !outcome.reductions.is_empty() {
        let names: Vec<String> = outcome
            .reductions
            .iter()
            .map(|r| format!("{r:?}").split_whitespace().next().unwrap_or("?").to_owned())
            .collect();
        eprintln!("--- reductions: {}", names.join(", "));
    }
    for signal in &outcome.signals {
        // DESIGN's intervention triggers are deterministic, so they are
        // *reported* here rather than judged.
        eprintln!("--- shape: {signal:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

async fn cmd_metrics(dir: &PathBuf) -> anyhow::Result<()> {
    let mut sessions: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    sessions.sort();
    if sessions.is_empty() {
        anyhow::bail!("no journals in {}", dir.display());
    }

    for path in sessions {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let body = std::fs::read_to_string(&path)?;
        let mut events = 0usize;
        let mut tool_calls = 0usize;
        let mut tool_results = 0usize;
        let mut responses = 0usize;
        let mut thinking_blobs = 0usize;
        let mut signatures = 0usize;
        let mut inline_bytes = 0u64;
        let mut blob_bytes = 0u64;
        let mut reductions: std::collections::BTreeMap<String, usize> = Default::default();
        let mut input = 0u64;
        let mut output = 0u64;
        let mut cache_read = 0u64;

        for line in body.lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            events += 1;
            let kind = event
                .get("body")
                .and_then(|b| b.get("event"))
                .and_then(|k| k.as_str())
                .unwrap_or("");
            match kind {
                "tool_call" => tool_calls += 1,
                "tool_result" => {
                    tool_results += 1;
                    let b = &event["body"];
                    if b.get("inline").is_some_and(|v| !v.is_null()) {
                        inline_bytes += b.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                    if b.get("blob").is_some_and(|v| !v.is_null()) {
                        blob_bytes += b.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0);
                    }
                    if let Some(r) = b.get("reduction").and_then(|v| v.as_str()) {
                        *reductions.entry(r.to_owned()).or_default() += 1;
                    }
                }
                "model_response" => {
                    responses += 1;
                    let b = &event["body"];
                    if b.get("thinking_blob").is_some_and(|v| !v.is_null()) {
                        thinking_blobs += 1;
                    }
                    if b.get("thinking_signature").is_some_and(|v| !v.is_null()) {
                        signatures += 1;
                    }
                    if let Some(u) = b.get("usage") {
                        input += u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        output += u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                        cache_read += u
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                }
                _ => {}
            }
        }

        println!("\n{name}");
        println!("  events            {events}");
        println!("  responses         {responses}");
        println!("  tool calls        {tool_calls}");
        println!("  tool results      {tool_results}  (inline {inline_bytes}B, spilled {blob_bytes}B)");
        println!("  tokens            in={input} out={output} cache_read={cache_read}");
        let total_cacheable = input + cache_read;
        if total_cacheable > 0 {
            println!(
                "  cache hit rate    {:.1}%",
                cache_read as f64 / total_cacheable as f64 * 100.0
            );
        }
        println!("  thinking blobs    {thinking_blobs} (signatures kept separately: {signatures})");
        if !reductions.is_empty() {
            println!("  reductions        {reductions:?}");
        }
    }
    Ok(())
}
