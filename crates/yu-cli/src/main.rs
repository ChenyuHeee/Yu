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
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use yu_core::agent::{Agent, AgentConfig};
use yu_core::decision::{DecisionConfig, DecisionLayer, Resumed};
use yu_core::render::{Charter, HumanUtterance};
use yu_policy::resource::Policy;
use yu_journal::{EventBody, Journal, SessionId};
use yu_workflow::handover::{
    Handover, ReaderAngle, WireReader, change_generation, trigger_for,
};
use yu_observe::{AgentId, ContextComposition, SpanKind, Timeline};
use yu_tools::ToolRegistry;
use yu_wire::capability::Capabilities;
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::types::{Message, MessagesRequest, Thinking, ThinkingDisplay};

#[global_allocator]
static ALLOC: yu_observe::Allocator = yu_observe::Allocator;

/// Which tools an executor is given.
///
/// DESIGN's premise is that an agent's tool surface is a decision rather than a
/// default. `exec` runs arbitrary model-chosen shell commands, so it is opted
/// into explicitly and is never what an unqualified run gets.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ToolSet {
    /// `read_file` and `list_dir`. Cannot change anything.
    ReadOnly,
    /// Adds `write_file` and `exec`. Can change anything the process can.
    Full,
}

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
        #[arg(long, value_enum, default_value_t = ToolSet::ReadOnly)]
        tools: ToolSet,
        /// The session this run belongs to. Reopening an existing one continues
        /// its record instead of starting a new one — which is what a run after a
        /// crash is, and what makes the effect discipline on the way in worth
        /// anything.
        #[arg(long)]
        session: Option<String>,
        /// The policy resource. `[[reduction]]` rows come from here, so which of
        /// DESIGN's five paths a tool result takes is a file edit rather than a
        /// rebuild.
        #[arg(long)]
        policy: Option<PathBuf>,
    },

    /// Run the decision layer on a charter. The thin vertical slice.
    ///
    /// DESIGN's acceptance question is answered by the number this prints: after
    /// a real task, what fraction of the decision layer's context is tool
    /// machinery? Near zero means the design holds. More than a tenth or two
    /// means the premise is wrong, and it is much better to learn that here than
    /// six months from now.
    Decide {
        /// The charter: what is to be achieved, in the human's own words.
        ///
        /// Required for a new session. When continuing one (`--session`), the
        /// recorded intent is restored and this is optional — and when it is
        /// given it is what it says it is: a further sentence of the human's,
        /// appended to L0 rather than replacing what was there.
        #[arg(short, long)]
        charter: Option<String>,
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
        #[arg(long)]
        cwd: Option<PathBuf>,
        #[arg(short, long)]
        model: Option<String>,
        /// The round ceiling.
        ///
        /// A new session gets 4 when this is not given. A continuation defaults
        /// to the ceiling the session was run under, recorded in its round
        /// markers; the budget belongs to the session, so what the continuation
        /// runs is what is left of it, not a second helping.
        #[arg(long)]
        max_rounds: Option<u32>,
        #[arg(long, value_enum, default_value_t = ToolSet::ReadOnly)]
        tools: ToolSet,
        /// The policy resource. A delegation's prediction sets come from here,
        /// which is what lets one be changed without rebuilding anything.
        #[arg(long)]
        policy: Option<PathBuf>,
        /// A standing constraint. Repeat for several. These are *injected* into
        /// every executor rather than restated by the decision layer each round,
        /// which is the only way one cannot be dropped on the round nobody
        /// remembered to repeat it.
        #[arg(long = "constraint")]
        constraints: Vec<String>,
        /// The declared topology: how the work is shaped. The declaration
        /// governs how many executors run, not merely what the view says.
        #[arg(long)]
        topology: Option<String>,
        /// The session this run belongs to, by name.
        ///
        /// A name with a record already under it is **continued**, and the run
        /// says what it restored: L0 is rebuilt from the state the session was
        /// handed (its seed event), L1 by replaying the `decide`/`retire` calls
        /// the record holds, and the rounds already spent are read from the
        /// round markers — so the continuation runs what is left of the
        /// session's budget rather than a second full one. Effects the record
        /// has recorded as completed are read back from it rather than
        /// performed again. Whatever the record cannot supply is named, out
        /// loud, rather than guessed at.
        ///
        /// A name with no record starts one, which is what makes a killed run
        /// resumable: the run that is killed has to have been named when it
        /// started. The two outcomes are printed differently, so a name that was
        /// not the one meant shows up as "new session" rather than as a
        /// continuation of nothing.
        #[arg(long, conflicts_with = "handover")]
        session: Option<String>,
        /// Start from a handover an earlier session wrote: its intent, its
        /// constraints, its decisions, its loose ends, and an index of where
        /// each came from. The new session's journal branches from the record
        /// the handover indexes, so its addresses keep meaning what they meant.
        #[arg(long)]
        handover: Option<PathBuf>,
        /// Write a handover for the next session here, when this run ends. The
        /// session boundary is where a generation change is decided: completion
        /// decides it first, and so does a session that ran out of rounds
        /// without reaching a final answer — the work it left is exactly what
        /// the next session continues.
        #[arg(long)]
        handover_out: Option<PathBuf>,
        /// A second way to hand over a session that stopped without a final
        /// answer: once its view has grown this many characters past where it
        /// started. A session that reached its round ceiling hands over without
        /// this; the growth rule covers a session that stopped for anything
        /// else. Growth, never window size: a freshly seeded window is large by
        /// construction, so billing that would fire forever.
        #[arg(long, requires = "handover_out")]
        handover_at_growth: Option<u64>,
    },

    /// Summarise a recorded session.
    Metrics {
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
    },

    /// Fork a session's record at a historical point, showing what it costs.
    ///
    /// DESIGN: "fork 是带成本提示的操作,不是免费调试按钮" — the estimate is printed
    /// before the fork is made, because the two numbers it carries are the
    /// decision: the storage is one marker line however long the parent is, and
    /// the prefix from the fork point onward is what a replay has to re-establish
    /// because the cache stops matching there. The parent is not written to.
    Fork {
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
        /// The session whose record is being branched from.
        #[arg(long)]
        session: String,
        /// The seq to branch at: the new branch continues from it and inherits
        /// everything below it.
        #[arg(long)]
        at: u64,
        /// The new branch's session id.
        #[arg(long)]
        into: String,
    },

    /// Serve sessions over a Unix socket, owning the records they live in.
    ///
    /// DESIGN's process model: the session belongs to the daemon, not to the CLI
    /// that started it, so a client that dies does not take the session with it —
    /// and a daemon that is killed is recovered from what is on disk.
    Daemon {
        /// Where sessions keep their journals and their descriptions.
        #[arg(long, default_value = ".yu")]
        dir: PathBuf,
        /// The socket the protocol is served on.
        #[arg(long, default_value = ".yu/daemon.sock")]
        socket: PathBuf,
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
            tools,
            session,
            policy,
        } => {
            cmd_run(
                &task,
                &dir,
                cwd,
                model,
                max_steps,
                tools,
                session.as_deref(),
                policy.as_deref(),
            )
            .await
        }
        Command::Decide {
            charter,
            dir,
            cwd,
            model,
            max_rounds,
            tools,
            policy,
            constraints,
            topology,
            session,
            handover,
            handover_out,
            handover_at_growth,
        } => {
            cmd_decide(
                charter.as_deref(),
                &dir,
                cwd,
                model,
                max_rounds,
                tools,
                policy.as_deref(),
                &constraints,
                topology.as_deref(),
                session.as_deref(),
                handover.as_deref(),
                handover_out.as_deref(),
                handover_at_growth,
            )
            .await
        }
        Command::Metrics { dir } => cmd_metrics(&dir).await,
        Command::Fork {
            dir,
            session,
            at,
            into,
        } => cmd_fork(&dir, &session, at, &into).await,
        Command::Daemon { dir, socket } => cmd_daemon(&dir, &socket).await,
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

#[allow(clippy::too_many_arguments)]
async fn cmd_run(
    task: &str,
    dir: &PathBuf,
    cwd: Option<PathBuf>,
    model: Option<String>,
    max_steps: u32,
    tools: ToolSet,
    session: Option<&str>,
    policy_path: Option<&Path>,
) -> anyhow::Result<()> {
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let timeline = Timeline::new();
    timeline.enter(AgentId::new("exec"));
    // The client must share the timeline, or the wire spans are never recorded
    // and the harness-overhead metric reports zero model time.
    let client = Arc::new(WireClient::from_env()?.with_timeline(timeline.clone()));
    let model = model.unwrap_or_else(|| client.default_model().to_owned());

    // Session id from the wall clock unless the caller named one: a record has to
    // be findable later, and a named session is how a run continues one that a
    // crash ended.
    let session = SessionId::new(match session {
        Some(id) => id.to_owned(),
        None => format!("run-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S")),
    });
    let journal = Arc::new(
        Journal::open(dir, session.clone())
            .await?
            .with_timeline(timeline.clone()),
    );

    if journal.is_empty() {
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
    } else {
        // Continuing a record that already exists. Reported rather than passed
        // over: a run that silently resumed would look exactly like a run that
        // started fresh, and the difference is the whole reason the id was named.
        let next = journal.len() as u64;
        eprintln!(
            "--- resuming session {}: {} event(s) already recorded, continuing at seq {next}",
            session.as_str(),
            journal.len()
        );
        report_unresolved(&journal).await;
        journal
            .append(
                AgentId::new("exec"),
                EventBody::Note {
                    text: format!(
                        "resumed at seq {next}: this run continues a record that already                          existed. Effects recorded as completed are read from the record                          rather than performed again."
                    ),
                },
            )
            .await?;
    }

    let policy = match policy_path {
        Some(path) => Some(load_policy(path)?),
        None => None,
    };
    let tools = Arc::new(build_tools(tools, cwd.clone(), policy.as_ref()));
    eprintln!("--- tool surface: {}", describe_tools(&tools));
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
    report_ratio("executor", &outcome.composition);
    Ok(())
}

fn build_tools(set: ToolSet, cwd: PathBuf, policy: Option<&Policy>) -> ToolRegistry {
    let registry = match set {
        ToolSet::ReadOnly => ToolRegistry::read_only(cwd),
        ToolSet::Full => ToolRegistry::native(cwd),
    };
    // The policy's `[[reduction]]` rows become the table the tools reduce under.
    // Until this line existed, a policy could declare reduction rules, load them,
    // validate them — and nothing that reduces had ever read one.
    match policy {
        Some(policy) => registry.with_reducer(Arc::new(
            yu_tools::Reducer::new(yu_tools::Budget::default())
                .with_table(Arc::new(policy.clone())),
        )),
        None => registry,
    }
}

/// Read a policy, and say out loud about any rule that can never fire.
///
/// A row naming an axis this build does not understand can never match, which
/// makes it a dead row — and a dead row is indistinguishable from a row whose
/// condition simply never came up. The same goes for a path that is not one of
/// DESIGN's five: the reducer skips it rather than guessing, so the only place the
/// mistake can be seen is here.
fn load_policy(path: &Path) -> anyhow::Result<Policy> {
    let policy = Policy::from_path(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let axes = policy.unknown_reduction_axes();
    if !axes.is_empty() {
        eprintln!(
            "--- policy {}: [[reduction]] row(s) name axis/axes this build does not              understand: {} — such a row cannot match anything, so it is dead rather              than merely unsatisfied",
            path.display(),
            axes.join(", ")
        );
    }
    let paths = policy.unknown_reduction_paths();
    if !paths.is_empty() {
        eprintln!(
            "--- policy {}: [[reduction]] row(s) name path(s) that are not one of the              five: {} — those rows are skipped, not approximated",
            path.display(),
            paths.join(", ")
        );
    }
    Ok(policy)
}

/// Print the effects the record left in the crash window.
///
/// This is the finding a resumed session must not walk past: an effect whose
/// completion was never written, about which the record says neither "done" nor
/// "not done". Printing it is not decoration — a run that heard nothing would
/// conclude the record was clean, and the difference between those two states is
/// the whole of the effect discipline.
async fn report_unresolved(journal: &yu_journal::Journal) {
    let Ok(unfinished) = journal.unresolved_effects().await else {
        return;
    };
    for effect in unfinished {
        eprintln!(
            "--- unresolved effect: {} ({}) recorded at seq {} — the record does not say \
             whether it happened{}",
            effect.key,
            effect.description,
            effect.at_seq,
            match effect.replay() {
                yu_journal::ReplayOfUnresolved::Reattempt => {
                    ", and its key certifies that performing it again is safe"
                }
                yu_journal::ReplayOfUnresolved::Surface => {
                    ", and its key certifies nothing, so it will not be performed again"
                }
            }
        );
    }
}

/// State the tool surface rather than leaving it to be inferred from a flag.
///
/// An executor that can run commands and one that cannot produce the same
/// looking transcript, so the run has to say which it was.
fn describe_tools(registry: &ToolRegistry) -> String {
    let mut names = registry.names();
    names.sort();
    names.join(", ")
}

/// Report DESIGN's acceptance metric for one layer's context.
///
/// Raw material is the headline: bytes that arrived as they came off a tool,
/// with neither a human nor a model having decided what in them mattered. It is
/// the criterion because it is the thing the architecture claims to keep out of
/// the decision layer, and because it is the one number a context full of reports
/// and a context full of `grep` output cannot both satisfy.
///
/// The breakdown is printed with it so the number can be argued with, and the
/// tool-machinery ratio is printed too — a context can be nearly all machinery
/// and nearly no raw material, and that combination is the design working.
///
/// `None` means nothing was measured, which is reported as such rather than as
/// zero — a layer with no measured context and a layer with a perfectly lean one
/// are different findings.
fn report_ratio(label: &str, composition: &ContextComposition) {
    match composition.raw_material_ratio() {
        Some(ratio) => eprintln!(
            "--- {label} context: raw material {:.1}% of {} chars \
             (raw {} + reduced {} + calls {} + definitions {}; tool machinery {:.1}%)",
            ratio * 100.0,
            composition.total_chars,
            composition.raw_material_chars,
            composition.reduced_result_chars,
            composition.tool_call_chars,
            composition.tool_definition_chars,
            composition.tool_machinery_ratio().unwrap_or(0.0) * 100.0,
        ),
        None => eprintln!("--- {label} context: NOT MEASURED (no context was recorded)"),
    }
}

/// Print where each delegation landed, and anything the harness caught.
///
/// The landing is the report, so this is the run's actual result rather than a
/// summary of it: a delegation that landed on `pass` says more in five
/// characters than its prose does, and the check that follows is the only part
/// of this output nobody had to be trusted for.
fn report_landings(outcome: &yu_core::decision::DecisionOutcome) {
    if outcome.outcomes.iter().all(|d| d.landing.is_none()) {
        return;
    }
    eprintln!("\n--- landings");
    for (i, delegation) in outcome.outcomes.iter().enumerate() {
        match &delegation.landing {
            Some(landing) => {
                let note = if landing.is_other() {
                    match &landing.unlisted {
                        Some(word) => format!(" (the executor called it `{word}`)"),
                        None => " (unpredicted)".to_owned(),
                    }
                } else {
                    String::new()
                };
                eprintln!(
                    "      #{i} {}/{} — {}{note}",
                    landing.set, landing.slot, landing.why
                );
            }
            None => eprintln!("      #{i} not classified (no set was pre-registered)"),
        }
        if let Some(caught) = &delegation.contradiction {
            // Marked, not buried. This is the one line in the output that is a
            // machine's finding rather than anyone's account.
            eprintln!("      !! {caught}");
        }
    }
    for problem in outcome.landing_problems() {
        eprintln!("      !! {problem}");
    }
}

/// Print L1 as the run left it.
///
/// L1 is what the run *produced* — the decisions and their reasons, what was
/// ruled out, what is still open. Printing it is not decoration: on a run that
/// decided nothing, seeing an empty ledger is the finding, and a summary line
/// saying "0 decisions" would read as a clean run rather than a layer that could
/// not commit to anything.
fn report_ledger(ledger: &yu_core::render::Ledger) {
    eprintln!(
        "\n--- L1: {} decision(s), {} rejected, {} open question(s)",
        ledger.decisions.len(),
        ledger.rejected.len(),
        ledger.open_questions.len()
    );
    for d in &ledger.decisions {
        eprintln!("      decided: {} — because {}", d.what, d.why);
    }
    for r in &ledger.rejected {
        eprintln!("      ruled out: {} — because {}", r.what, r.why);
    }
    if ledger.decisions.is_empty() && ledger.rejected.is_empty() {
        eprintln!(
            "      (the run wrote nothing to state: it reached no conclusion it was \
             willing to record)"
        );
    }
}

/// Print the delegation tree the run left in the record.
///
/// Read back out of L2 rather than out of anything the run held in memory, for
/// the same reason every other reader of the record is: the tree is a fact about
/// the run, and a fact that lived only in the process that produced it is one no
/// later reader can check. DESIGN's claim — "每一个 agent 都可以再派生 agent" — is
/// a claim about the shape a run actually took, and this is where that shape
/// becomes visible instead of asserted.
async fn report_agents(journal: &yu_journal::Journal) {
    let Ok(events) = journal.events().await else {
        return;
    };
    let edges = yu_core::delegate::spawn_edges(&events);
    if edges.is_empty() {
        return;
    }
    let mut children: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    let mut parents: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut derived: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for edge in &edges {
        let parent = edge.parent.as_str().to_owned();
        children
            .entry(parent.clone())
            .or_default()
            .push((edge.child.as_str().to_owned(), edge.task.clone()));
        parents.insert(parent);
        derived.insert(edge.child.as_str().to_owned());
    }

    eprintln!("\n--- agents: {} derived", edges.len());
    // Whoever derives and is never derived is a root. Naming the root rather
    // than assuming the decision layer means a tree read back from a forked or
    // resumed record still prints.
    for root in parents.difference(&derived) {
        eprintln!("      {root}");
        print_subtree(&children, root, 1);
    }
}

fn print_subtree(
    children: &std::collections::BTreeMap<String, Vec<(String, String)>>,
    node: &str,
    depth: usize,
) {
    let Some(kids) = children.get(node) else {
        return;
    };
    for (child, task) in kids {
        // The task, cut to one line: this is a shape report, and the tasks
        // themselves are in the record at full length.
        let mut label: String = task.chars().take(60).collect();
        if label.chars().count() < task.chars().count() {
            label.push('…');
        }
        eprintln!("      {}└ {child} — {label}", "  ".repeat(depth));
        print_subtree(children, child, depth + 1);
    }
}

/// Print how the decision layer's context moved over the run.
///
/// DESIGN says the view is bounded — "大小有界、前缀字节稳定、永不退化". That is a
/// claim about a curve, so it is checkable only against the series and not
/// against its last point: a run that ends small after peaking at forty thousand
/// characters did not stay bounded, it merely stopped.
fn report_growth(outcome: &yu_core::decision::DecisionOutcome) {
    if outcome.contexts.len() < 2 {
        return;
    }
    let curve: Vec<String> = outcome
        .contexts
        .iter()
        .map(|c| c.total_chars.to_string())
        .collect();
    eprintln!("--- decision-layer context per request: {}", curve.join(" "));
    if let Some(peak) = outcome.peak() {
        eprintln!(
            "--- peak context: {} chars, raw material {:.1}%",
            peak.total_chars,
            peak.raw_material_ratio().unwrap_or(0.0) * 100.0
        );
    }
}

// ---------------------------------------------------------------------------
// decide — the vertical slice
// ---------------------------------------------------------------------------

/// Everything a decision run needs standing up before the layer exists.
struct DecisionBoot {
    client: Arc<WireClient>,
    journal: Arc<Journal>,
    tools: Arc<ToolRegistry>,
    timeline: Arc<Timeline>,
    model: String,
    session: SessionId,
}

/// Stand up the timeline, client, journal and tool surface for a decision run.
///
/// Extracted from `cmd_decide` so that it has a caller other than `main`. DESIGN
/// lists `init` among the spans, and this is the only place it is emitted —
/// process and pool start-up is exactly the per-step cost flower paid — so it
/// needs a guard, and a guard needs a caller a test can reach.
async fn boot_decision(
    dir: &Path,
    cwd: PathBuf,
    model: Option<String>,
    tool_set: ToolSet,
    policy: Option<&Policy>,
    resume: Option<&Handover>,
    session_name: Option<&str>,
) -> anyhow::Result<DecisionBoot> {
    let timeline = Timeline::new();
    timeline.enter(AgentId::decision());
    let init = timeline.start(SpanKind::Init);

    let client = Arc::new(WireClient::from_env()?.with_timeline(timeline.clone()));
    let model = model.unwrap_or_else(|| client.default_model().to_owned());
    // A named session is a session the caller wants to find again — for a
    // continuation it is the record being reopened, and the name is the address.
    let session = SessionId::new(match session_name {
        Some(name) => name.to_owned(),
        None => format!("decide-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S")),
    });
    // A continuation branches from the record its handover indexes rather than
    // starting a log of its own. `journal:seq=<n>` names a position in one
    // lineage, so a session that restarted the numbering would make every
    // address the handover carries mean something else — silently.
    let journal = Arc::new(match resume {
        Some(handover) => handover.resume_into(dir, session.clone()).await?,
        None => Journal::open(dir, session.clone()).await?,
    }
    .with_timeline(timeline.clone()));
    let tools = Arc::new(build_tools(tool_set, cwd, policy));
    init.finish();

    Ok(DecisionBoot {
        client,
        journal,
        tools,
        timeline,
        model,
        session,
    })
}

/// The round ceiling a new session gets when none is given.
const DEFAULT_DECIDE_ROUNDS: u32 = 4;

/// Parse a `--topology` argument, naming the shapes when it is not one.
fn topology_named(name: &str) -> anyhow::Result<yu_core::topology::TopologySpec> {
    let kind = yu_core::topology::TopologyKind::from_str(name).ok_or_else(|| {
        anyhow::anyhow!(
            "`{name}` is not a topology this harness knows; the shapes are single, chain, \
             fan_out, tree, reflection, peer, verification_partition"
        )
    })?;
    Ok(yu_core::topology::TopologySpec::new(kind))
}

/// State what a continuation rebuilt from the record, and what it could not.
///
/// Printed rather than left implicit, because the failure mode here is silent in
/// both directions: a run that believed it was round 1 of a session that is
/// really round 4 would spend the budget twice, and a run that lost the
/// constraints the session was under would work without them — neither of which
/// shows up in anything but this report.
fn report_resume(resumed: &Resumed, rounds: u32) {
    eprintln!(
        "--- continuing a session whose record already existed: seq continues at {}; L0 = {} \
         constraint(s){}; L1 = {} decision(s), {} rejected, {} open question(s), rebuilt from \
         {} recorded call(s)",
        resumed.at_seq,
        resumed.charter.constraints.len(),
        match &resumed.charter.topology {
            Some(topology) => format!(", topology {}", topology.kind.as_str()),
            None => String::new(),
        },
        resumed.ledger.decisions.len(),
        resumed.ledger.rejected.len(),
        resumed.ledger.open_questions.len(),
        resumed.ledger_calls,
    );
    match resumed.rounds_spent {
        Some(spent) => eprintln!(
            "--- rounds already spent: {spent}; this continuation runs rounds {}..={rounds}",
            spent + 1
        ),
        None => eprintln!(
            "--- rounds already spent: unknown — the record holds no round marker, so this \
             continuation runs the whole budget it was given"
        ),
    }
    for problem in &resumed.not_recoverable {
        eprintln!("--- NOT restored from the record: {problem}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_decide(
    charter_text: Option<&str>,
    dir: &PathBuf,
    cwd: Option<PathBuf>,
    model: Option<String>,
    max_rounds: Option<u32>,
    tool_set: ToolSet,
    policy_path: Option<&Path>,
    constraints: &[String],
    topology: Option<&str>,
    session_name: Option<&str>,
    handover_path: Option<&Path>,
    handover_out: Option<&Path>,
    handover_at_growth: Option<u64>,
) -> anyhow::Result<()> {
    let cwd = cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // A new session has to be told what to do; a continuation already carries
    // its intent in the record, and asking for it again would invite the
    // operator to rewrite the charter of a session the human already stated.
    if charter_text.is_none() && session_name.is_none() {
        anyhow::bail!("`--charter` is required unless `--session` continues an existing record");
    }
    // Whether this run continues a record or starts one. Asked before anything
    // is written, and about the same file the journal will write — a name with a
    // record is a session to continue, and a name without one is a session to
    // start (which is what makes a killed run resumable: the run that gets
    // killed has to have been named when it started).
    let named_record_exists = session_name.is_some_and(|name| {
        dir.join(yu_journal::event_log_file_name(&SessionId::new(name)))
            .exists()
    });
    if session_name.is_some() && !named_record_exists && charter_text.is_none() {
        anyhow::bail!(
            "there is no record of a session named `{}` in {}, so this would be a new session — \
             and a new session has to be given a `--charter`",
            session_name.expect("just checked"),
            dir.display()
        );
    }

    // A handover from a previous session, if this one is a continuation. Read as
    // text and kept as text: its blob's address is the digest of the bytes that
    // were stored, so re-serialising it here would name an address that is not
    // the one the record holds.
    let resumed = match handover_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            let handover = Handover::from_json(&raw)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
            Some((handover, raw))
        }
        None => None,
    };

    // The policy is read before the tools are built, because it decides how they
    // reduce. Reading it afterwards — which is what this did — left the table
    // loaded and consulted by nobody.
    let policy = match policy_path {
        Some(path) => Some(load_policy(path)?),
        None => None,
    };

    let DecisionBoot {
        client,
        journal,
        tools,
        timeline,
        model,
        session,
    } = boot_decision(
        dir,
        cwd.clone(),
        model,
        tool_set,
        policy.as_ref(),
        resumed.as_ref().map(|(h, _)| h),
        session_name,
    )
    .await?;

    // ---- read the record back, when this run continues one
    //
    // Before a single request is sent, because everything the continuation does
    // is conditioned on it: L0 and L1 come from here, the round budget comes
    // from here, and what the record could *not* supply is said out loud below
    // rather than filled in by a guess.
    let mut continued: Option<Resumed> = None;
    if let Some(name) = session_name {
        if journal.is_empty() {
            // No record, or an empty file: either way there is nothing to
            // continue. Said out loud, because the two outcomes of `--session` —
            // started here, continued here — must not look alike.
            eprintln!(
                "--- session {name}: no events recorded yet — this run starts it, and a later \
                 run with the same name can continue it"
            );
        } else {
            continued = Some(Resumed::read(&journal, &AgentId::decision()).await?);
        }
    }

    // ---- the round budget
    //
    // It belongs to the session, not to the invocation: a continuation spends
    // what is left of the ceiling the session was run under (recorded in its
    // round markers) unless the operator explicitly gives it another. Running
    // the whole budget again would spend it twice, and the round count the run
    // reports would describe neither run.
    let rounds = max_rounds
        .or_else(|| continued.as_ref().and_then(|resumed| resumed.ceiling))
        .unwrap_or(DEFAULT_DECIDE_ROUNDS);
    // Kept for the run's summary, which reports the session's totals: the
    // difference between "5 rounds" and "5 rounds, 3 of them before this
    // continuation" is the difference between two readings of one number.
    let resumed_rounds_before = continued
        .as_ref()
        .and_then(|resumed| resumed.rounds_spent)
        .unwrap_or(0);
    if let Some(resumed) = &continued {
        let name = session_name.expect("a continuation has a session name");
        let spent = resumed.rounds_spent.unwrap_or(0);
        if spent >= rounds {
            anyhow::bail!(
                "session `{name}` has already spent {spent} of its {rounds} round(s): this \
                 continuation would have no round to run. Pass a higher `--max-rounds` to give \
                 it room, or leave the session where it is"
            );
        }
        if resumed.forked_from.is_some() && resumed.tool_surface.is_none() {
            // A branch's L0 and L1 were handed to it from a handover, and this
            // record — written before sessions recorded the state they start
            // from — does not hold them. Continuing it would run a session whose
            // constraints and whose settled questions are both unknown, which is
            // the one thing a resume must not do silently.
            let (parent, at) = resumed.forked_from.as_ref().expect("just checked");
            anyhow::bail!(
                "the record of `{name}` is a branch of `{parent}` at seq {at}, and holds no seed \
                 event: the state it inherited from that branch's handover cannot be rebuilt \
                 from this record, so continuing it would run without the constraints and \
                 decisions the session was under. Start a new session from the handover instead"
            );
        }
    }

    // DESIGN: "人类原话原样存活". The charter arrives as the human typed it and is
    // kept that way; there is no summarising step between them and the decision
    // layer. The constructor exists so the sentence lands in exactly one field —
    // setting both `text` and `human_words` renders L0 with the intent twice on
    // every round, which is the layer least able to afford it.
    let mut charter = match &continued {
        // **L0, rebuilt from the record**, not from these flags. What the
        // session was handed — its constraints, its acceptance, its declared
        // topology, the human's own words — is what it is still under, and none
        // of it is re-invented here. A flag adds to it; a flag never quietly
        // replaces a state the record holds.
        Some(resumed) => {
            if resumed.charter.is_empty() && charter_text.is_none() {
                anyhow::bail!(
                    "the record of `{}` holds no charter, and none was passed: a continuation \
                     with no L0 would be a session with no intent",
                    session_name.expect("a continuation has a session name"),
                );
            }
            let mut charter = resumed.charter.clone();
            if let Some(text) = charter_text {
                charter.human_words.push(HumanUtterance {
                    at_ms: chrono::Utc::now().timestamp_millis(),
                    text: text.to_owned(),
                    // Stated, because a later reader has to be able to tell the
                    // session's original words from one added mid-continuation.
                    source: "cli, on continuing this session".to_owned(),
                });
            }
            for constraint in constraints {
                if !charter.constraints.iter().any(|held| held == constraint) {
                    charter.constraints.push(constraint.clone());
                }
            }
            if let Some(name) = topology {
                let declared = topology_named(name)?;
                if let Some(held) = &charter.topology
                    && held != &declared
                {
                    eprintln!(
                        "--- the declared topology is being replaced: the record holds `{}`, \
                         this invocation declares `{name}`",
                        held.kind.as_str()
                    );
                }
                charter.topology = Some(declared);
            }
            charter
        }
        None => {
            let mut charter = Charter::from_the_humans_own_words(
                charter_text.expect("checked: a new session is given a charter"),
                chrono::Utc::now().timestamp_millis(),
                "cli",
            );
            charter.constraints = constraints.to_vec();
            if let Some(name) = topology {
                charter.topology = Some(topology_named(name)?);
            }
            charter
        }
    };

    if let Some(resumed) = &continued {
        report_resume(resumed, rounds);
    }

    // **The state half of the handover, into L0 and L1.** The human's sentence
    // above is this session's; what the previous one carried is added to it —
    // its intent verbatim, its constraints (which is the list the layer injects
    // into every executor), its decisions and ruled-out proposals, and its loose
    // ends as open questions.
    let mut seed = None;
    if let Some((handover, raw)) = &resumed {
        seed = Some(handover.new_session(&charter));
        charter = seed.as_ref().expect("just set").charter.clone();
        eprintln!(
            "--- resuming from a handover: {} item(s) carried, {} angle(s) reported, index at {}",
            handover.carried_len(),
            handover.sources.len(),
            format_args!("blob:{}", yu_journal::digest_of(raw.as_bytes())),
        );
    }

    let surface = describe_tools(&tools);
    // What the record says the session's executor surface was, against what this
    // invocation built. They can legitimately differ — the operator chooses —
    // but they must not differ *silently*: an executor that lost `exec` or
    // `write_file` because of a default is a continuation that cannot do what
    // the session was doing.
    if let Some(recorded) = continued
        .as_ref()
        .and_then(|resumed| resumed.tool_surface.as_ref())
    {
        let mut now: Vec<String> = tools.names();
        now.sort();
        if recorded != &now {
            eprintln!(
                "--- the executor tool surface has changed since this session started: the \
                 record holds [{}], this invocation built [{}] — the continuation runs with \
                 what was just built",
                recorded.join(", "),
                now.join(", ")
            );
        }
    }
    let mut config = DecisionConfig::new(model);
    config.max_rounds = rounds;
    // The prediction sets come from the policy file, so changing one is an edit
    // to a resource rather than a rebuild — DESIGN's constraint 0, applied to
    // the one thing the decision layer is forbidden to invent.
    let reader_client = client.clone();
    let mut layer = DecisionLayer::new(
        client,
        // Cloned rather than moved: the record outlives the layer that wrote it,
        // and the tree this run left is read back out of it below.
        journal.clone(),
        tools,
        timeline.clone(),
        config,
        charter.clone(),
        cwd,
    );
    // **The index half.** The layer is not handed the handover's material — it
    // is told where the material is, which is what a manifest is for, and it can
    // pay for what it decides to look at.
    if let (Some(seed), Some((handover, raw))) = (&seed, &resumed) {
        layer = layer.with_carried(vec![handover.carried_entry(
            &format!("blob:{}", yu_journal::digest_of(raw.as_bytes())),
            raw.len() as u64,
        )]);
        *layer.ledger_mut() = seed.ledger.clone();
    }
    // The prediction sets, against the ones the session was run under: a
    // continuation whose policy no longer declares a set the record holds would
    // refuse the same delegations the session was making, and that is a
    // difference the operator has to see rather than discover from a refusal.
    if let Some(recorded) = continued
        .as_ref()
        .and_then(|resumed| resumed.outcome_sets.as_ref())
    {
        let now: Vec<String> = policy
            .as_ref()
            .map(|policy| policy.outcome_sets.ids().into_iter().map(str::to_owned).collect())
            .unwrap_or_default();
        if recorded != &now {
            eprintln!(
                "--- the policy's outcome sets have changed since this session started: the \
                 record holds [{}], this invocation loaded [{}]",
                recorded.join(", "),
                now.join(", ")
            );
        }
    }
    // **The record, as this run's L0/L1.** The layer is handed the state the
    // record rebuilt — L0 and L1, its counters, and the index of what the
    // session did before this continuation — so the run does not begin as if
    // nothing had happened.
    if let Some(resumed) = continued.take() {
        layer = layer.resume(resumed);
    }
    if let Some(policy) = &policy {
        let path = policy_path.expect("a policy came from somewhere");
        let ids = policy.outcome_sets.ids();
        eprintln!(
            "--- policy {} — {} outcome set(s){}",
            path.display(),
            ids.len(),
            if ids.is_empty() {
                String::new()
            } else {
                format!(": {}", ids.join(", "))
            }
        );
        if ids.is_empty() {
            // Said out loud rather than passed over: without a set, `delegate`
            // is offered with no `outcomes` field, and every delegation runs
            // unchecked. That is a real difference in what the run can prove,
            // and it should not be discoverable only by reading the tool schema.
            eprintln!(
                "--- no prediction sets declared: delegations will not be classified, and \
                 no landing can be checked"
            );
        }
        layer = layer.with_outcome_sets(policy.outcome_sets.clone());
    }

    eprintln!(
        "session {} — journal in {} — executor tool surface: {}",
        session.as_str(),
        dir.display(),
        surface
    );
    // A layer that inherited a record — through a handover, which branches one —
    // inherits its unfinished effects too. Reported for the same reason a
    // resumed run reports them: the fact that the record does not say whether an
    // effect happened is itself the finding, and silence reads as a clean record.
    report_unresolved(&journal).await;
    let started = std::time::Instant::now();
    let outcome = layer.run().await?;
    let elapsed = started.elapsed();

    println!("{}", outcome.final_text);

    // The counts are the session's, not this invocation's: a continuation
    // inherits the numbers and continues them, so "round 5, delegation 3" means
    // one thing in the record rather than one thing per process.
    eprintln!(
        "\n--- {} round(s), {} delegation(s){} in {:.1}s",
        outcome.rounds,
        outcome.delegations,
        match resumed_rounds_before {
            0 => String::new(),
            before => format!(" ({before} before this continuation)"),
        },
        elapsed.as_secs_f64()
    );
    eprintln!(
        "--- tokens in={} out={} cache_read={}",
        outcome.usage.input_tokens, outcome.usage.output_tokens, outcome.usage.cache_read_input_tokens
    );
    if let Some(stopped) = &outcome.stopped_early {
        // Never inferred from the absence of an answer: a run that stopped has
        // to say why, in words, next to the numbers it would otherwise look
        // complete without.
        eprintln!("--- stopped: {}", stopped.describe());
    }

    report_landings(&outcome);
    report_agents(&journal).await;
    report_ledger(&outcome.ledger);
    // The comparison the whole design rests on, from one run.
    report_ratio("decision layer", &outcome.composition);
    report_growth(&outcome);
    for (i, d) in outcome.outcomes.iter().enumerate() {
        report_ratio(&format!("executor #{i}"), &d.executor_composition);
    }

    let mut metrics = yu_observe::Metrics::default();
    metrics.observe_spans(&timeline.spans());
    let mut parts: Vec<String> = metrics
        .elapsed_ns_by_kind
        .iter()
        .map(|(k, v)| format!("{k}={:.1}ms", *v as f64 / 1e6))
        .collect();
    parts.sort();
    eprintln!("--- spans: {}", parts.join(" "));

    // ---- the generation change, if this run is to hand over
    //
    // Decided here, at the session boundary, because that is where the two
    // things a trigger needs exist: a run that has ended, and a record that has
    // stopped being written. `trigger_for` owns the decision; this owns the
    // consequences.
    if let Some(path) = handover_out {
        match trigger_for(&outcome, handover_at_growth) {
            Some(trigger) => {
                let reader = WireReader::new(reader_client);
                let change = change_generation(
                    &reader,
                    &journal,
                    trigger,
                    &charter,
                    &outcome.ledger,
                    &ReaderAngle::ALL,
                )
                .await?;
                let json = change.handover.to_json()?;
                std::fs::write(path, &json)
                    .map_err(|e| anyhow::anyhow!("writing {}: {e}", path.display()))?;
                eprintln!(
                    "--- handover ({}): {} item(s) from {} angle(s), index at {} → {}",
                    change.trigger.describe(),
                    change.handover.carried_len(),
                    change.handover.sources.len(),
                    change.handover_at,
                    path.display(),
                );
                // Said out loud, because a gap is the one thing this flow found
                // and could not fix by itself.
                for gap in &change.gaps {
                    eprintln!(
                        "--- the handover does not carry: {} [{}]",
                        gap.text, gap.address
                    );
                }
            }
            None => {
                // Worded for what actually happened. The previous line said "the
                // session stopped without deciding", while L1 held a decision,
                // and its first real reader concluded the layer had decided
                // nothing. The stop is about the *final answer*; the ledger is
                // reported above this line, separately.
                let why = outcome
                    .stopped_early
                    .as_ref()
                    .map(|stopped| stopped.describe())
                    .unwrap_or_else(|| "the session reached no final answer".to_owned());
                eprintln!(
                    "--- no handover: {why}, and its view did not grow past the declared growth \
                     threshold either — this boundary did not warrant the change"
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// fork — a historical point, priced before it is taken
// ---------------------------------------------------------------------------

async fn cmd_fork(dir: &PathBuf, session: &str, at: u64, into: &str) -> anyhow::Result<()> {
    let parent = Arc::new(Journal::open(dir, SessionId::new(session)).await?);
    if at > parent.len() as u64 {
        anyhow::bail!(
            "session `{session}` holds {} event(s): there is no seq {at} to fork at",
            parent.len()
        );
    }
    if dir.join(format!("{into}.jsonl")).exists() {
        // A fork onto an existing record would append a marker to somebody
        // else's branch, which is the one thing the marker exists to prevent.
        anyhow::bail!("`{into}` already has a record in {}; pick another name", dir.display());
    }

    // The estimate, before anything is written.
    let estimate = parent.estimate_fork(at).await?;
    eprintln!("--- fork estimate");
    eprintln!("      {estimate}");
    eprintln!(
        "      the parent is not written to: the branch reads its history through it"
    );

    let child = Journal::open_fork(dir, SessionId::new(into), parent.clone(), at).await?;

    eprintln!(
        "--- forked {} at seq {at}: {} event(s) inherited by reference, {} byte(s) written",
        SessionId::new(into),
        estimate.events,
        estimate.marker_bytes
    );
    match child.cache_diverges_at() {
        Some((parent_session, at)) => eprintln!(
            "--- cache prefix diverges at {parent_session}:{at}: requests on this branch from \
             that point onward match nothing cached"
        ),
        None => eprintln!("--- this branch is not a fork and diverges nowhere"),
    }
    eprintln!(
        "--- parent {} still holds {} event(s), whole",
        parent.session(),
        parent.len()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon — sessions the process owns
// ---------------------------------------------------------------------------

async fn cmd_daemon(dir: &Path, socket: &Path) -> anyhow::Result<()> {
    // A socket file outlives the process that bound it, so a killed daemon leaves
    // one behind. Removing it blindly would steal the address from a live daemon;
    // refusing blindly would mean a killed daemon could never be restarted. The
    // question is answered by trying to connect.
    if socket.exists() {
        match tokio::net::UnixStream::connect(socket).await {
            Ok(_) => anyhow::bail!(
                "{} is already served by a live daemon; stop it before starting another",
                socket.display()
            ),
            Err(_) => {
                eprintln!(
                    "--- {} is a socket nobody is serving: the daemon that bound it is gone \
                     (a kill -9 leaves exactly this). Rebinding.",
                    socket.display()
                );
                std::fs::remove_file(socket)
                    .map_err(|e| anyhow::anyhow!("removing {}: {e}", socket.display()))?;
            }
        }
    }
    if let Some(parent) = socket.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(socket)
        .map_err(|e| anyhow::anyhow!("binding {}: {e}", socket.display()))?;
    let host = yu_server::daemon::Daemon::open(dir)?;
    let config = yu_server::session::ServerConfig {
        host: yu_server::session::Host::LocalDaemon {
            socket: socket.to_path_buf(),
        },
        state_dir: dir.to_path_buf(),
    };
    // Printed after the bind, so a client that waits for this line is waiting for
    // something that is true.
    eprintln!(
        "--- daemon up: sessions in {}, protocol on {}",
        dir.display(),
        socket.display()
    );
    yu_server::session::serve(listener, &host, config).await?;
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
        let mut assistant_texts = 0usize;
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
                    // Each thinking block is a record holding two digests: the
                    // text's and the signature's, kept apart because either can
                    // be the larger of the two.
                    if let Some(records) = b.get("thinking").and_then(|v| v.as_array()) {
                        thinking_blobs += records.len();
                        signatures += records
                            .iter()
                            .filter(|r| r.get("signature_blob").is_some_and(|v| !v.is_null()))
                            .count();
                    }
                    if b.get("text_blob").is_some_and(|v| !v.is_null()) {
                        assistant_texts += 1;
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
        println!("  thinking blocks   {thinking_blobs} (signatures kept separately: {signatures})");
        println!("  assistant texts   {assistant_texts} recorded (the turn's own words, as blobs)");
        if !reductions.is_empty() {
            println!("  reductions        {reductions:?}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **M1: "删掉任意一种 span 的 `timeline.start(...)` → 有测试失败."**
    ///
    /// `init` measures process and pool start-up — the cost flower paid per step
    /// — so it is emitted by the boot sequence and nowhere else, and it is the
    /// one span a test that is already running cannot produce. The other eight
    /// are guarded by `yu-core`'s scripted-endpoint run; this is the guard for
    /// the ninth.
    #[tokio::test]
    async fn the_boot_sequence_records_an_init_span() {
        // A credential and nothing else. `WireClient::from_env` builds an HTTP
        // client with a connection pool, which is startup cost rather than a
        // request, so nothing here reaches the network. This is the only test in
        // this binary, which is what makes touching the process environment
        // acceptable.
        if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
            unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-not-a-real-key") };
        }

        let dir = std::env::temp_dir().join(format!("yu-cli-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let boot = boot_decision(&dir, dir.clone(), None, ToolSet::ReadOnly, None, None, None)
            .await
            .expect("the boot sequence");

        let init = boot
            .timeline
            .spans()
            .into_iter()
            .find(|span| span.kind == SpanKind::Init)
            .expect("the boot sequence recorded no `init` span");
        assert_eq!(
            init.agent,
            AgentId::decision(),
            "start-up belongs to the decision layer, whatever runs next"
        );
        // Finished rather than dropped: a span that is only recorded because its
        // guard was dropped measures nothing, and says so in `attrs`.
        assert!(
            !init.attrs.contains_key("unfinished"),
            "the span was dropped rather than finished: {:?}",
            init.attrs
        );

        // Boot produces the layer's whole surface, not just a timeline.
        assert_eq!(boot.session.as_str().split('-').next(), Some("decide"));
        assert!(
            !boot.tools.names().is_empty(),
            "a layer with no tools cannot delegate"
        );
        assert!(dir.join(format!("{}.jsonl", boot.session.as_str())).is_file());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
