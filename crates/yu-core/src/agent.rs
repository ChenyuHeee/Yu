//! The loop.
//!
//! ## Which layer this is
//!
//! This is the **execution layer**: "一次性、可丢弃、fresh". It accumulates its
//! own tool calls and results *within* a task, because it genuinely needs them
//! to do the work — and then the accumulation is discarded, so there is no path
//! dependency and no cumulative degradation. The decision layer does not use
//! this loop's transcript; it gets a rendered view (`crate::render`).
//!
//! ## What the loop enforces rather than assumes
//!
//! * Every tool result goes back in **one** user message. DESIGN states this as
//!   an iron law; splitting them silently teaches a model to stop calling tools
//!   in parallel.
//! * The history is validated **before** it is sent. The endpoint's own error for
//!   a turn missing its thinking block blames thinking and names no location.
//! * `max_tokens` is not completion. Every `stop_reason` maps to an explicit
//!   disposition and there is no wildcard arm.
//! * Tools that declare themselves parallel run concurrently behind the
//!   admission gate; anything unrecognised is treated as serial, because the
//!   safe default for an unknown tool is that it mutates something.

use std::path::PathBuf;
use std::sync::Arc;

use sha2::{Digest, Sha256};

use yu_journal::{BlobStore, EventBody, Journal, ThinkingRecord};
use yu_observe::{
    AgentId, ContextComposition, ShapeSignal, ShapeTracker, SpanKind, StepObservation, Timeline,
    TokenUsage,
};
use yu_tools::{Payload, Pressure, Reduction, ResultKind, ToolContext, ToolRegistry, ToolResult};
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::sse::StreamedMessage;
use yu_wire::types::{
    ContentBlock, Effort, Message, MessagesRequest, StopDisposition, StopReason, SystemBlock,
    Thinking, ToolDefinition, ToolUseBlock,
};

use crate::gate::{AdmissionGate, spawn_abortable};
use crate::render::CompositionMeter;

/// DESIGN's default for the decision layer is `xhigh`; the execution layer runs
/// degraded, since it is doing bounded work under supervision.
pub const EXECUTOR_EFFORT: Effort = Effort::Medium;

/// The executor's own prompt.
///
/// DESIGN: the execution layer is "被给予约束,不记得约束" — given its constraints,
/// never remembering them — so its prompt is short on purpose. It carries the
/// reporting discipline and nothing else, because everything task-specific
/// belongs in the task.
///
/// It existed as a file for a while before anything loaded it, which meant the
/// executor ran with **no system prompt at all**: the discipline below was
/// written down and never given to anyone. A resource nothing reads is a
/// comment.
pub const EXECUTOR_PROMPT: &str = include_str!("../prompts/executor.md");

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: AgentId,
    pub model: String,
    pub max_tokens: u32,
    pub effort: Effort,
    pub thinking: Thinking,
    pub system: Vec<SystemBlock>,
    /// The step ceiling. DESIGN's triggers are functions of *pressure*, not of
    /// turn count — so this is a backstop against a wedged loop, not the
    /// mechanism that ends a run.
    pub max_steps: u32,
}

impl AgentConfig {
    pub fn new(id: AgentId, model: impl Into<String>) -> Self {
        Self {
            id,
            model: model.into(),
            max_tokens: 8192,
            effort: EXECUTOR_EFFORT,
            thinking: Thinking::adaptive(),
            // The reporting discipline is not optional and is not something a
            // caller should have to remember to attach, so it is the default.
            system: vec![SystemBlock::text(EXECUTOR_PROMPT)],
            max_steps: 40,
        }
    }

    pub fn system(mut self, blocks: Vec<SystemBlock>) -> Self {
        self.system = blocks;
        self
    }

    pub fn max_steps(mut self, n: u32) -> Self {
        self.max_steps = n;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error(transparent)]
    Wire(#[from] yu_wire::WireError),

    #[error(transparent)]
    Journal(#[from] yu_journal::JournalError),

    /// Caught locally so the failure names a message index. The server's own
    /// error for this does not.
    #[error("history rejected before sending — {0}")]
    History(#[from] yu_wire::quirks::HistoryProblem),

    #[error("hit max_tokens after {steps} step(s); output is truncated, not complete")]
    Truncated { steps: u32 },

    #[error("the model declined this request")]
    Refused,

    #[error("the context window was exhausted")]
    ContextExhausted,

    /// DESIGN: never treat an unknown stop reason as success.
    #[error("unrecognised stop_reason `{0}`; refusing to assume it means completion")]
    UnrecognizedStop(String),

    #[error("step ceiling of {0} reached without completing")]
    StepCeiling(u32),
}

/// A stop the loop's own machinery caused, rather than the model ending its
/// turn.
///
/// Kept out of [`StopDisposition`] because that vocabulary belongs to the
/// endpoint: the dispositions are readings of a `stop_reason`, and "the budget
/// ran out" is not one. It is the harness's own stop, and it is the most common
/// way a run ends — so it is reported as a fact about the run, not raised as an
/// error that erases the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStop {
    /// `max_steps` was reached while the model was still working: the last step
    /// ended in a tool call whose result it never got to see.
    StepCeiling(u32),
    /// The endpoint answered `pause_turn`, which no layer of this harness can
    /// serve: it means a **server-side** tool paused mid-turn, and re-sending
    /// continues it — but every tool this harness registers is a custom tool
    /// with an explicit schema, so there is nothing here to resume.
    PauseTurn,
}

impl LoopStop {
    /// The account to give whoever reads the outcome.
    ///
    /// The decision layer gets an account of what happened instead of a hole
    /// where the delegation was, so a stop is something to decide on rather than
    /// a failure it has to survive.
    pub fn describe(self) -> String {
        match self {
            LoopStop::StepCeiling(limit) => format!(
                "The executor reached its step ceiling of {limit} while still working and \
                 produced no final answer; what follows is where it got to, and the evidence \
                 is what it ran."
            ),
            LoopStop::PauseTurn => "The endpoint answered `pause_turn`, which this layer cannot \
                 serve: it means a server-side tool paused mid-turn, and no server-side tool \
                 exists here. Re-sending would continue nothing and spend the step anyway, so \
                 the run was stopped and is reported."
                .to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopOutcome {
    pub steps: u32,
    pub text: String,
    /// The model's last `stop_reason`, as a disposition. For a run that ends on
    /// the step ceiling this is what the model was asking for when the budget
    /// ran out — not a completion.
    pub stop: StopDisposition,
    pub usage: TokenUsage,
    pub tool_calls: u32,
    /// What the shape metrics noticed. DESIGN: the decision layer watches these
    /// instead of reading the tool stream.
    pub signals: Vec<ShapeSignal>,
    /// Which reduction each tool result took, for M1's accounting.
    pub reductions: Vec<Reduction>,
    /// Set when the loop stopped for its own reason rather than the model
    /// ending its turn. `None` means the run reported completion.
    ///
    /// This is the field the decision layer's "did it finish" question is
    /// answered from. Before it existed, `run` returned `Ok` only on
    /// [`StopDisposition::Complete`] and the ceiling was an `Err`, so every
    /// outcome that could actually occur looked complete — the caller's
    /// `completed` was true by construction, and its "stopped without
    /// completing" branch described a state production could not reach.
    pub stopped_early: Option<LoopStop>,
    /// What this executor's context was made of.
    ///
    /// The other half of DESIGN's acceptance criterion. The decision layer's
    /// ratio is meant to approach zero; this one is meant to be large, because
    /// this is where the tool stream actually lives. Reported together they make
    /// the contrast a measurement rather than a claim.
    pub composition: ContextComposition,
}

/// Accumulates the streamed response.
#[derive(Default)]
struct CollectSink {
    text: String,
    thinking: String,
    tool_calls: Vec<(String, String)>,
}

impl StreamSink for CollectSink {
    fn on_text(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn on_thinking(&mut self, text: &str) {
        self.thinking.push_str(text);
    }

    fn on_tool_use(&mut self, id: &str, name: &str) {
        self.tool_calls.push((id.to_owned(), name.to_owned()));
    }
}

pub struct Agent {
    client: Arc<WireClient>,
    journal: Arc<Journal>,
    tools: Arc<ToolRegistry>,
    timeline: Arc<Timeline>,
    gate: AdmissionGate,
    ctx: ToolContext,
    config: AgentConfig,
    messages: Vec<Message>,
    shape: ShapeTracker,
}

impl Agent {
    pub fn new(
        client: Arc<WireClient>,
        journal: Arc<Journal>,
        tools: Arc<ToolRegistry>,
        timeline: Arc<Timeline>,
        config: AgentConfig,
        cwd: PathBuf,
    ) -> Self {
        Self {
            client,
            journal,
            tools,
            timeline,
            gate: AdmissionGate::new(),
            ctx: ToolContext::with_default_budget(cwd),
            config,
            messages: Vec::new(),
            shape: ShapeTracker::new(),
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub async fn run(&mut self, task: &str) -> Result<LoopOutcome, LoopError> {
        // Zero is not a run: no request can be sent, so no outcome — not even
        // the ceiling's — could describe what happened. The error remains for
        // the configuration mistake; every run with a budget stops as an
        // outcome (see the loop's tail).
        if self.config.max_steps == 0 {
            return Err(LoopError::StepCeiling(0));
        }

        self.journal
            .append(
                self.config.id.clone(),
                EventBody::UserMessage {
                    text: task.to_owned(),
                },
            )
            .await?;
        self.messages.push(Message::user(task));

        let mut usage = TokenUsage::default();
        let mut signals = Vec::new();
        let mut reductions = Vec::new();
        let mut tool_calls = 0u32;

        // The last step's answer and stop reason, kept because the ceiling is a
        // real ending and the account of where the run got to is made of them.
        // Only a step that asked for tools reaches the assignment below, so
        // neither a completion nor a stop the loop reports itself passes here.
        let mut last: Option<(u32, String, StopDisposition)> = None;

        for step in 1..=self.config.max_steps {
            self.timeline.enter(self.config.id.clone());

            // ---- assemble, and validate before spending anything
            let assemble = self.timeline.start(SpanKind::Assemble);
            let req = self.build_request();
            let quirks = self.client.quirks();
            if let Err(problem) = quirks.check_history(&req.messages) {
                assemble.finish();
                return Err(LoopError::History(problem));
            }
            if let Err(problem) = quirks.check_cache_breakpoints(req.cache_breakpoints()) {
                assemble.finish();
                return Err(LoopError::History(problem));
            }
            let digest = request_digest(&req);
            let message_count = req.messages.len();
            assemble.finish();

            self.journal
                .append(
                    self.config.id.clone(),
                    EventBody::ModelRequest {
                        model: req.model.clone(),
                        messages: message_count,
                        digest,
                        est_input_tokens: None,
                    },
                )
                .await?;

            // ---- call
            let mut sink = CollectSink::default();
            let outcome = self.client.stream(&req, &mut sink).await?;
            usage.add(&outcome.message.usage.to_observed());

            self.journal_response(&outcome.message).await?;

            let uses: Vec<ToolUseBlock> = outcome
                .message
                .content
                .iter()
                .filter_map(|b| b.as_tool_use().cloned())
                .collect();

            for tu in &uses {
                self.journal
                    .append(
                        self.config.id.clone(),
                        EventBody::ToolCall {
                            tool_use_id: tu.id.clone(),
                            name: tu.name.clone(),
                            input: tu.input.clone(),
                        },
                    )
                    .await?;
            }

            // The assistant turn is replayed verbatim, thinking blocks included.
            // Measured: dropping them is a 400 (WIRE-FACTS Quirk 1).
            self.messages
                .push(Message::assistant(outcome.message.content.clone()));

            // ---- disposition, exhaustively
            let stop = outcome
                .message
                .stop_reason
                .clone()
                .unwrap_or_else(|| StopReason::Unknown("<absent>".to_owned()));
            let disposition = stop.disposition();

            // ---- shape
            //
            // Observed *after* the disposition is known, because whether a step
            // produced something is partly a question of whether it finished. A
            // final step that answers in prose and calls no tool has produced
            // the artifact; counting it as "tokens growing with no artifact"
            // would make the signal fire on every successful run, which is worse
            // than useless — it trains whoever reads it to ignore it.
            let observation = StepObservation {
                agent: self.config.id.clone(),
                tool_calls: uses.len() as u32,
                files_touched: touched_paths(&uses),
                // DESIGN's "触碰文件 vs 预期" needs a declared set, which nothing
                // produces yet. Left absent rather than filled with a guess,
                // since the drift check treats `None` as "not checkable".
                files_declared: None,
                tokens_delta: outcome.message.usage.output_tokens,
                // Crude for M0: a tool call, or a step that reached a terminal
                // state, stands in for "produced something checkable". The real
                // signal wants diff detection (M1).
                produced_artifact: !uses.is_empty()
                    || matches!(disposition, StopDisposition::Complete),
                ms_since_last_artifact: None,
                call_signatures: uses.iter().map(call_signature).collect(),
            };
            signals.extend(
                self.shape
                    .observe(&observation, yu_observe::now_ms().max(0) as u64),
            );

            match disposition {
                StopDisposition::RunTools => {
                    tool_calls += uses.len() as u32;
                    let pressure = pressure_from(&outcome.message.usage);
                    let (blocks, applied) = self.dispatch(&uses, pressure).await?;
                    reductions.extend(applied);
                    // DESIGN: every `tool_result` in ONE user message.
                    self.messages.push(Message::user(blocks));
                }
                StopDisposition::Resume => {
                    // `pause_turn` says a **server-side** tool paused mid-turn and
                    // re-sending lets it continue. This layer serves no server
                    // tools, so it cannot be in that state, and re-sending is
                    // what turned this into a spin: one step per iteration until
                    // the budget was gone, after which the run reported a *step
                    // ceiling* — an account that named the budget instead of the
                    // cause. `decision.rs` refuses `pause_turn` for the same
                    // reason; this arm is that policy applied to a loop that can
                    // still hand back an account instead of an error. The two
                    // arms live in different files, so they change together: a
                    // layer that ever serves server tools is the one case that
                    // makes `Resume` legitimate again, and it makes it for both.
                    return Ok(LoopOutcome {
                        steps: step,
                        text: sink.text.clone(),
                        stop: disposition,
                        usage,
                        tool_calls,
                        signals,
                        reductions,
                        stopped_early: Some(LoopStop::PauseTurn),
                        composition: self.measure_context(),
                    });
                }
                StopDisposition::Complete => {
                    return Ok(LoopOutcome {
                        steps: step,
                        text: sink.text.clone(),
                        stop: disposition,
                        usage,
                        tool_calls,
                        signals,
                        reductions,
                        stopped_early: None,
                        composition: self.measure_context(),
                    });
                }
                StopDisposition::Truncated => return Err(LoopError::Truncated { steps: step }),
                StopDisposition::Refused => return Err(LoopError::Refused),
                StopDisposition::ContextExhausted => return Err(LoopError::ContextExhausted),
                StopDisposition::Unrecognized(raw) => {
                    return Err(LoopError::UnrecognizedStop(raw));
                }
            }

            // Only a step that asked for tools to run reaches this point, and
            // it is the account a ceiling would report.
            last = Some((step, sink.text.clone(), disposition));
        }

        // The step ceiling, reported as an outcome rather than raised as an
        // error.
        //
        // DESIGN's rule is that the decision layer receives an account of what
        // happened. The ceiling is the most common way an executor's run ends —
        // a loop that keeps calling tools reaches it — and as an `Err` it
        // travelled up `run_delegation`'s `?` into `DecisionLayer::run`, which
        // then returned `Err` with no report, no evidence and no acceptance
        // numbers. Everything the account needs is still in hand here: the
        // record holds what ran, and the counters and signals below are the
        // run's own shape.
        let (steps, text, stop) = last.unwrap_or_else(|| {
            // Unreachable: a positive budget sends at least one request, and
            // every request lands in `last`. Stated rather than asserted, since
            // a panic here would be the same lost run this branch exists to
            // prevent.
            (
                0,
                String::new(),
                StopDisposition::Unrecognized("<no step was taken>".to_owned()),
            )
        });
        Ok(LoopOutcome {
            steps,
            text,
            stop,
            usage,
            tool_calls,
            signals,
            reductions,
            stopped_early: Some(LoopStop::StepCeiling(self.config.max_steps)),
            composition: self.measure_context(),
        })
    }

    /// What this agent's context is made of, measured over the whole of it.
    ///
    /// Everything the request carries counts: the tool definitions and system
    /// prompt are as much a part of the context as the messages are, and leaving
    /// them out would understate the machinery in exactly the direction that
    /// flatters the design.
    fn measure_context(&self) -> ContextComposition {
        let mut meter = CompositionMeter::new();
        meter.add_tools(&self.tools.definitions());
        if !self.config.system.is_empty() {
            meter.add_system(&self.config.system);
        }
        for message in &self.messages {
            meter.add_message(message);
        }
        meter.snapshot()
    }

    fn build_request(&self) -> MessagesRequest {
        let mut req = MessagesRequest::new(
            self.config.model.clone(),
            self.config.max_tokens,
            self.messages.clone(),
        )
        .thinking(self.config.thinking.clone())
        .effort(self.config.effort);
        if !self.config.system.is_empty() {
            req = req.system(self.config.system.clone());
        }
        let definitions = self.tools.definitions();
        if !definitions.is_empty() {
            req = req.tools(definitions);
        }
        req
    }

    /// Record the response, honouring DESIGN's L2 constraint 4.
    async fn journal_response(&self, msg: &StreamedMessage) -> Result<(), LoopError> {
        let blobs = self.journal.blobs();
        let mut thinking: Vec<ThinkingRecord> = Vec::new();
        let mut text = String::new();

        for block in &msg.content {
            match block {
                ContentBlock::Thinking(t) => {
                    // One record per block, in order. The endpoint interleaves
                    // `thinking → text → server_tool_use → … → thinking`, and
                    // Quirk 1 requires each to replay byte-exact — so welding two
                    // blocks into one string would attach the signature of one to
                    // the text of another, and that replays as a corrupted turn.
                    let text_blob = blobs.put_str(&t.thinking)?;
                    let signature_blob = match &t.signature {
                        Some(signature) => Some(blobs.put_str(signature)?),
                        None => None,
                    };
                    thinking.push(ThinkingRecord {
                        text_blob,
                        signature_blob,
                    });
                }
                ContentBlock::Text(t) => text.push_str(&t.text),
                _ => {}
            }
        }

        // The turn's actual product. An earlier version recorded only the block
        // *type names* here, so the words themselves never reached the record —
        // which broke L2's completeness, the decision layer's "每轮最终答复", and
        // the next generation's ability to verify against the primary record.
        let text_blob = if text.is_empty() {
            None
        } else {
            Some(blobs.put_str(&text)?)
        };

        self.journal
            .append(
                self.config.id.clone(),
                EventBody::ModelResponse {
                    id: msg.id.clone(),
                    stop_reason: msg
                        .stop_reason
                        .as_ref()
                        .map(|s| s.as_str().to_owned())
                        .unwrap_or_else(|| "<absent>".to_owned()),
                    usage: msg.usage.to_observed(),
                    blocks: msg.content.iter().map(|b| b.type_name().to_owned()).collect(),
                    thinking,
                    text_blob,
                },
            )
            .await?;
        Ok(())
    }

    /// Run the requested tools and return their result blocks.
    ///
    /// Parallel-capable tools run concurrently behind a shared admission;
    /// everything else runs one at a time behind an exclusive one. The
    /// distinction is a property of the tool, decided once, not re-derived.
    async fn dispatch(
        &self,
        uses: &[ToolUseBlock],
        pressure: Pressure,
    ) -> Result<(Vec<ContentBlock>, Vec<Reduction>), LoopError> {
        let mut slots: Vec<Option<(ContentBlock, Reduction)>> = Vec::new();
        slots.resize_with(uses.len(), || None);

        let mut concurrent = Vec::new();
        let mut exclusive = Vec::new();
        for (index, tu) in uses.iter().enumerate() {
            if supports_parallel(&tu.name) {
                concurrent.push(index);
            } else {
                exclusive.push(index);
            }
        }

        let mut handles = Vec::new();
        for index in concurrent {
            let tu = uses[index].clone();
            let gate = self.gate.clone();
            let registry = self.tools.clone();
            let ctx = self.ctx.clone();
            let journal = self.journal.clone();
            let timeline = self.timeline.clone();
            handles.push((
                index,
                spawn_abortable(async move {
                    let _admission = gate.admit(true).await;
                    run_one(registry, ctx, journal, tu, pressure, timeline).await
                }),
            ));
        }
        for (index, handle) in handles {
            // Outer `Result` is the join; inner is the tool.
            let outcome = handle
                .await
                .map_err(|e| yu_wire::WireError::transport(format!("tool task failed: {e}")))??;
            slots[index] = Some(outcome);
        }

        for index in exclusive {
            let tu = uses[index].clone();
            // Admission is taken and released around each serial tool, so a
            // long write cannot be interleaved with a read.
            let _admission = self.gate.admit(false).await;
            let outcome = run_one(
                self.tools.clone(),
                self.ctx.clone(),
                self.journal.clone(),
                tu,
                pressure,
                self.timeline.clone(),
            )
            .await?;
            slots[index] = Some(outcome);
        }

        let mut blocks = Vec::with_capacity(uses.len());
        let mut reductions = Vec::with_capacity(uses.len());
        for (index, slot) in slots.into_iter().enumerate() {
            let (block, reduction) = slot.ok_or_else(|| {
                yu_wire::WireError::transport(format!("tool {index} produced no result"))
            })?;
            blocks.push(block);
            reductions.push(reduction);
        }
        Ok((blocks, reductions))
    }
}

/// Execute one tool, reduce its result, and record both.
async fn run_one(
    registry: Arc<ToolRegistry>,
    ctx: ToolContext,
    journal: Arc<Journal>,
    tu: ToolUseBlock,
    pressure: Pressure,
    timeline: Arc<Timeline>,
) -> Result<(ContentBlock, Reduction), yu_wire::WireError> {
    let name = tu.name.clone();
    let input = tu.input.clone();

    // `ctx` is needed both inside the blocking task and afterwards, for
    // reduction, so the task gets its own clone.
    let ctx_for_tool = ctx.clone();
    let tool_timer = timeline.start(SpanKind::ToolExec).label(name.clone());
    let raw = tokio::task::spawn_blocking(move || match registry.get(&name) {
        Some(tool) => tool.execute(&input, &ctx_for_tool),
        None => ToolResult::inline(
            name.clone(),
            input.clone(),
            ResultKind::Structured,
            format!("error: no tool named `{name}` is registered"),
        ),
    })
    .await
    .map_err(|e| yu_wire::WireError::transport(format!("tool panicked: {e}")))?;
    let executed = tool_timer
        .attr("bytes", raw.size as i64)
        .attr("kind", raw.kind.as_str())
        .finish();
    tracing::debug!(
        elapsed_us = executed.elapsed_ns / 1000,
        tool = %tu.name,
        bytes = raw.size,
        "tool_exec"
    );

    // Spilling goes through the journal's content-addressed store, so a re-read
    // of a spilled payload is the same digest and the reducer can refuse to
    // spill it twice. A store that fails answers with the empty string, which is
    // the reducer's "not stored": it passes the payload through rather than
    // handing the model an address that would not resolve, and the refusal is
    // recorded rather than smoothed over.
    let blobs: &BlobStore = journal.blobs();
    let mut refused = false;
    let mut spill = |bytes: &[u8]| -> String {
        match blobs.put(bytes) {
            Ok(digest) => digest,
            Err(error) => {
                refused = true;
                tracing::error!(
                    error = %error,
                    bytes = bytes.len(),
                    "spill refused; the payload will be passed through"
                );
                String::new()
            }
        }
    };
    let raw_bytes = raw.size;
    let reduce_timer = timeline.start(SpanKind::Reduce).label(tu.name.clone());
    let (reduction, reduced) = ctx.reduce(raw, pressure, &mut spill);
    let reduced_span = reduce_timer
        .attr("path", reduction_name(&reduction, refused))
        .attr("bytes_in", raw_bytes as i64)
        .attr("bytes_out", reduced.render().len() as i64)
        .finish();
    tracing::debug!(elapsed_us = reduced_span.elapsed_ns / 1000, "reduce");

    // L2 constraint 1: the payload is recorded exactly once, and which of the
    // two fields carries it is decided here rather than left to chance.
    let (inline, blob) = match &reduced.payload {
        Payload::Inline(text) => (Some(text.clone()), None),
        Payload::Reduced { text, .. } => (Some(text.clone()), None),
        Payload::Spilled { digest, .. } => (None, Some(digest.clone())),
        // Dropped: the manifest explains the absence, so neither field is set.
        Payload::Dropped(_) => (None, None),
    };

    journal
        .append(
            AgentId::new("exec"),
            EventBody::ToolResult {
                tool_use_id: tu.id.clone(),
                name: tu.name.clone(),
                kind: reduced.kind.as_str().to_owned(),
                bytes: reduced.size,
                reproducible: reduced.reproducible,
                addressable: reduced.addressable,
                inline,
                blob,
                reduction: Some(reduction_name(&reduction, refused).to_owned()),
            },
        )
        .await
        .map_err(|e| yu_wire::WireError::transport(format!("journaling tool result: {e}")))?;

    Ok((reduced.to_block(&tu.id), reduction))
}

/// Whether a tool may run alongside others.
///
/// Read-only tools may; anything that can write must not. An unrecognised tool
/// is treated as serial, because the safe default for an unknown tool is that it
/// mutates something.
///
/// This table lives here because `yu_tools::Tool` does not yet carry the
/// declaration. TODO(M1): move it onto the trait, so a new tool cannot be added
/// without answering the question.
fn supports_parallel(name: &str) -> bool {
    matches!(name, "read_file" | "list_dir")
}

/// The name recorded for a reduction is the path the result actually took.
///
/// Not always the one `choose` proposed: a spill the store refused comes back as
/// a pass-through (see `Reducer::apply`), and a journal that kept the proposed
/// name would record a spill that never happened.
fn reduction_name(r: &Reduction, spill_refused: bool) -> &'static str {
    match r {
        Reduction::SpillWithPointer { .. } if spill_refused => "spill_failed_pass_through",
        Reduction::PassThrough => "pass_through",
        Reduction::DropWithManifest { .. } => "drop_with_manifest",
        Reduction::SpillWithPointer { .. } => "spill_with_pointer",
        Reduction::DelegateReader { .. } => "delegate_reader",
        Reduction::SandboxReduce => "sandbox_reduce",
    }
}

fn call_signature(tu: &ToolUseBlock) -> String {
    // Canonical so that two identical calls compare equal even if the model
    // ordered the keys differently.
    format!(
        "{}:{}",
        tu.name,
        serde_json::to_string(&tu.input).unwrap_or_else(|_| "<unserializable>".to_owned())
    )
}

/// Paths a tool call appears to touch, for the drift signal.
fn touched_paths(uses: &[ToolUseBlock]) -> Vec<String> {
    let mut out = Vec::new();
    for tu in uses {
        for key in ["path", "file", "file_path", "dir"] {
            if let Some(p) = tu.input.get(key).and_then(|v| v.as_str()) {
                out.push(p.to_owned());
            }
        }
    }
    out
}

/// How full the context is.
///
/// DESIGN wants every long-path mechanism to be a function of *pressure* rather
/// than of turn count or the clock. Cache reads count toward the window, because
/// they occupy it just the same.
fn pressure_from(usage: &yu_wire::types::Usage) -> Pressure {
    const WINDOW: u64 = 1_000_000;
    let occupied = usage.input_tokens + usage.cache_read_input_tokens;
    Pressure::with_fill(occupied as f64 / WINDOW as f64)
}

pub(crate) fn request_digest(req: &MessagesRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(req.model.as_bytes());
    hasher.update(req.max_tokens.to_le_bytes());

    // Everything that shapes the request has to be in here. On an endpoint whose
    // caching is purely prefix-based and whose `cache_control` is inert
    // (WIRE-FACTS Quirk 3), an invisible prefix is an invisible cache: two
    // requests with entirely different system prompts would otherwise share a
    // digest and be recorded as "the same view re-sent", which is the opposite
    // of what the journal is for.
    if let Some(system) = &req.system {
        for block in system {
            hasher.update(block.text.as_bytes());
            hasher.update([0]);
        }
    }
    if let Some(tools) = &req.tools {
        for tool in tools {
            hasher.update(tool.name().as_bytes());
            match tool {
                ToolDefinition::Custom(c) => {
                    hasher.update(c.description.as_deref().unwrap_or("").as_bytes());
                    hasher.update(c.input_schema.to_string().as_bytes());
                }
                ToolDefinition::Server(server) => hasher.update(server.kind.as_bytes()),
            }
            hasher.update([0]);
        }
    }
    if let Some(thinking) = &req.thinking {
        hasher.update(
            serde_json::to_string(thinking)
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    if let Some(output) = &req.output_config {
        hasher.update(
            serde_json::to_string(output)
                .unwrap_or_default()
                .as_bytes(),
        );
    }

    for message in &req.messages {
        // Walk the blocks, not just the concatenated text: a turn's thinking and
        // tool calls are part of what is sent, and on this endpoint replaying
        // them is mandatory rather than optional.
        for block in message.blocks() {
            hasher.update(block.type_name().as_bytes());
            match &block {
                ContentBlock::ToolUse(t) => {
                    hasher.update(t.id.as_bytes());
                    hasher.update(t.input.to_string().as_bytes());
                }
                ContentBlock::ToolResult(t) => hasher.update(t.tool_use_id.as_bytes()),
                _ => {}
            }
        }
        hasher.update(message.content.concat_text().as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..16])
}

/// Fixtures for tests that need a run to *repeat* rather than finish.
///
/// The loop's budget can only be reached by a model that keeps asking for
/// tools, which no real endpoint will do on request — and `WireClient` is a
/// concrete type, so there is nothing to substitute inside the loop. What this
/// provides is the double at the system's edge instead: a real HTTP server on a
/// real loopback socket, the same shape `yu-wire`'s redirect tests use, with
/// everything between the socket and the assertion left as production code.
///
/// Gated on `cfg(test)` so it cannot become a production shortcut.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use yu_wire::client::{Auth, WireClient};

    /// A scratch directory this test owns. `tempfile` is not a dependency of
    /// this crate, and the directory is re-created empty, so a leftover from a
    /// failed run can never be read as part of a passing one.
    pub fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yu-core-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A turn that asks for one `exec` call and is nowhere near done.
    ///
    /// The thinking block is not decoration: the strict history rule requires
    /// one with every `tool_use`, and the loop validates before it sends.
    pub fn endless_tool_use(command: &str) -> String {
        serde_json::json!({
            "id": "msg-fake",
            "model": "fake-model",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "keep going", "signature": "sig"},
                {"type": "tool_use", "id": "call_00_fake", "name": "exec",
                 "input": {"command": command}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    /// A turn the endpoint says it wants to resume: a server-side tool paused
    /// mid-turn. A layer with no server tools cannot be in that state, which is
    /// what makes it the interesting reply here.
    pub fn paused_turn_reply() -> String {
        serde_json::json!({
            "id": "msg-fake",
            "model": "fake-model",
            "role": "assistant",
            "content": [{"type": "text", "text": "pausing"}],
            "stop_reason": "pause_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    /// A turn that answers and ends.
    pub fn final_reply(text: &str) -> String {
        serde_json::json!({
            "id": "msg-fake",
            "model": "fake-model",
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    /// A model endpoint that answers from a fixed script.
    pub struct FakeEndpoint {
        addr: SocketAddr,
        requests: Arc<AtomicUsize>,
    }

    impl FakeEndpoint {
        /// Answer every request with the same reply.
        pub fn always(reply: impl Into<String>) -> Self {
            Self::scripted(vec![reply.into()])
        }

        /// Answer the nth request with the nth reply, repeating the last.
        pub fn scripted(replies: Vec<String>) -> Self {
            assert!(!replies.is_empty(), "a script needs at least one reply");
            let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
            let addr = listener.local_addr().expect("the listener's address");
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = requests.clone();
            std::thread::spawn(move || {
                while let Ok((mut sock, _)) = listener.accept() {
                    if read_request(&mut sock).is_none() {
                        continue;
                    }
                    let index = counter.fetch_add(1, Ordering::SeqCst);
                    let reply = replies
                        .get(index)
                        .unwrap_or_else(|| replies.last().expect("checked at construction"));
                    // An answer that arrives before the client has finished
                    // writing would be read as a connection reset, so the
                    // request is fully drained above before this one.
                    let _ = sock.write_all(&http_json(reply));
                }
            });
            Self { addr, requests }
        }

        /// A client pointed at this endpoint, with the strict history rules a
        /// measured profile would impose.
        pub fn client(&self) -> Arc<WireClient> {
            Arc::new(
                WireClient::builder()
                    .base_url(format!("http://{}", self.addr))
                    .auth(Auth::ApiKey("test-credential".to_owned()))
                    .default_model("fake-model")
                    .build()
                    .expect("a loopback base url is a valid endpoint"),
            )
        }

        /// How many requests actually arrived, for asserting what stopped a run.
        pub fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    /// Read one complete HTTP request: the head, then `content-length` bytes.
    fn read_request(sock: &mut TcpStream) -> Option<()> {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(head_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
                let body_len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= head_end + 4 + body_len {
                    return Some(());
                }
            }
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => return None,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// A plain JSON message under a 200. The client accepts one — a proxy that
    /// drops `stream: true` is a real thing, and this keeps the double simple
    /// without pretending the endpoint is something it is not.
    fn http_json(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use yu_journal::{SessionId, digest_of};
    use yu_tools::{Provenance, Tool};
    use yu_wire::types::{CustomTool, ToolDefinition};

    #[test]
    fn unknown_tools_are_serial_by_default() {
        // The safe default: an unrecognised tool might write.
        assert!(supports_parallel("read_file"));
        assert!(supports_parallel("list_dir"));
        assert!(!supports_parallel("write_file"));
        assert!(!supports_parallel("exec"));
        assert!(!supports_parallel("some_new_tool_nobody_declared"));
    }

    #[test]
    fn identical_calls_share_a_signature_regardless_of_key_order() {
        let a = ToolUseBlock {
            id: "call_00_a".into(),
            name: "read_file".into(),
            input: json!({"path": "src/a.rs", "start": 1}),
        };
        let b = ToolUseBlock {
            id: "call_00_b".into(),
            name: "read_file".into(),
            input: json!({"start": 1, "path": "src/a.rs"}),
        };
        assert_eq!(call_signature(&a), call_signature(&b));
        // ...but the id is not part of it: the same call twice is the repetition
        // DESIGN's counter is meant to catch.
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn a_different_call_is_a_different_signature() {
        let a = ToolUseBlock {
            id: "i".into(),
            name: "read_file".into(),
            input: json!({"path": "src/a.rs"}),
        };
        let b = ToolUseBlock {
            id: "i".into(),
            name: "read_file".into(),
            input: json!({"path": "src/b.rs"}),
        };
        assert_ne!(call_signature(&a), call_signature(&b));
    }

    #[test]
    fn touched_paths_reads_the_common_parameter_names() {
        let uses = vec![
            ToolUseBlock {
                id: "1".into(),
                name: "read_file".into(),
                input: json!({"path": "src/a.rs"}),
            },
            ToolUseBlock {
                id: "2".into(),
                name: "list_dir".into(),
                input: json!({"dir": "src"}),
            },
            ToolUseBlock {
                id: "3".into(),
                name: "exec".into(),
                input: json!({"command": "ls"}),
            },
        ];
        assert_eq!(touched_paths(&uses), vec!["src/a.rs", "src"]);
    }

    #[test]
    fn pressure_counts_cache_reads_against_the_window() {
        let usage = yu_wire::types::Usage {
            input_tokens: 1000,
            cache_read_input_tokens: 999_000,
            ..Default::default()
        };
        // A cache hit still occupies the window; ignoring it would report an
        // empty context while the window is full.
        assert!(pressure_from(&usage).is_high());
    }

    #[test]
    fn pressure_is_low_for_a_small_context() {
        let usage = yu_wire::types::Usage {
            input_tokens: 500,
            ..Default::default()
        };
        assert!(!pressure_from(&usage).is_high());
    }

    #[test]
    fn the_digest_is_stable_and_input_sensitive() {
        let a = MessagesRequest::new("m", 100, vec![Message::user("hello")]);
        let b = MessagesRequest::new("m", 100, vec![Message::user("hello")]);
        let c = MessagesRequest::new("m", 100, vec![Message::user("goodbye")]);
        assert_eq!(request_digest(&a), request_digest(&b));
        assert_ne!(request_digest(&a), request_digest(&c));
    }

    /// The digest has to cover the whole *prefix*, not just the messages.
    ///
    /// The previous version hashed model, max_tokens and message text only, so
    /// two requests whose system prompts were completely different shared a
    /// digest — and the journal recorded them as the same view re-sent. On an
    /// endpoint where caching is purely prefix-based (WIRE-FACTS Quirk 3), that
    /// is a cache you cannot see.
    #[test]
    fn the_digest_covers_the_prefix_and_not_only_the_messages() {
        let base = MessagesRequest::new("m", 100, vec![Message::user("hello")]);

        let terse = base
            .clone()
            .system(vec![SystemBlock::text("Answer in one word.")]);
        let verbose = base
            .clone()
            .system(vec![SystemBlock::text("Explain your reasoning at length.")]);
        assert_ne!(
            request_digest(&terse),
            request_digest(&verbose),
            "two different system prompts must not share a digest"
        );
        assert_ne!(
            request_digest(&base),
            request_digest(&terse),
            "adding a system prompt must change the digest"
        );

        let with_tools = base.clone().tools(vec![crate::delegate::delegate_tool()]);
        assert_ne!(
            request_digest(&base),
            request_digest(&with_tools),
            "the tool set is part of the prefix"
        );

        let mut with_thinking = base.clone();
        with_thinking.thinking = Some(Thinking::adaptive());
        assert_ne!(
            request_digest(&base),
            request_digest(&with_thinking),
            "the thinking configuration changes what is sent"
        );

        let with_effort = base.clone().effort(Effort::Xhigh);
        assert_ne!(
            request_digest(&base),
            request_digest(&with_effort),
            "effort changes what is sent"
        );
    }

    /// Replaying a turn is part of the request, so it is part of the digest.
    ///
    /// A tool result carries the id it answers, and on this endpoint those ids
    /// must be replayed verbatim (Quirk 2) — so two histories that differ only in
    /// which call a result answers are genuinely different requests.
    #[test]
    fn the_digest_sees_tool_results_and_their_ids() {
        let blocks = |id: &str| {
            vec![ContentBlock::ToolResult(yu_wire::types::ToolResultBlock::text(
                id, "14:32 JST",
            ))]
        };
        let a = MessagesRequest::new("m", 100, vec![Message::user(blocks("call_00_a"))]);
        let b = MessagesRequest::new("m", 100, vec![Message::user(blocks("call_00_b"))]);
        assert_ne!(request_digest(&a), request_digest(&b));
    }

    #[test]
    fn executor_effort_is_below_the_decision_layer_default() {
        // DESIGN: the decision layer runs `xhigh`; the execution layer is
        // deliberately degraded.
        assert_eq!(EXECUTOR_EFFORT, Effort::Medium);
    }

    // -----------------------------------------------------------------------
    // The loop's own stop is an outcome, not an error
    // -----------------------------------------------------------------------

    /// A run that exhausts its step budget comes back as an outcome.
    ///
    /// The budget is the harness's own stop and the most common way an
    /// executor's run ends. As an `Err` it erased the run: `run_delegation`'s
    /// `?` carried it up into `DecisionLayer::run`, which returned `Err` with no
    /// report, no evidence and no numbers — and because `Ok` was returned only
    /// on completion, a caller asking "did it finish?" was answered yes for
    /// every outcome the loop could actually produce.
    #[tokio::test]
    async fn a_run_that_exhausts_its_steps_is_an_outcome_rather_than_an_error() {
        let fake = test_support::FakeEndpoint::always(test_support::endless_tool_use("true"));
        let dir = scratch_dir("step-ceiling");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let mut agent = Agent::new(
            fake.client(),
            journal,
            Arc::new(ToolRegistry::native(&dir)),
            Timeline::new(),
            AgentConfig::new(AgentId::new("executor"), "fake-model").max_steps(2),
            dir.clone(),
        );

        let outcome = agent
            .run("keep going")
            .await
            .expect("a run that runs out of steps is an outcome, not an error");

        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.tool_calls, 2);
        assert_eq!(outcome.stopped_early, Some(LoopStop::StepCeiling(2)));
        assert_ne!(
            outcome.stop,
            StopDisposition::Complete,
            "the last turn asked for a tool, so it cannot read as completion"
        );
        assert_eq!(fake.requests(), 2, "the budget is what stopped the run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `pause_turn` stops the run and says so, instead of being re-sent.
    ///
    /// The disposition presupposes a server-side tool that paused mid-turn, and
    /// nothing in this harness registers one, so "not servable here" is the
    /// honest reading. Re-sending — which this loop did — spends a step per
    /// iteration to learn the same thing, and the run then reports a step
    /// ceiling: an account of a spin that names the budget rather than the
    /// cause. The request count is what separates the two behaviours: one turn
    /// asked, one turn answered.
    ///
    /// `decision.rs` refuses the same disposition, so the two loops now apply
    /// one policy — see the `Resume` arm in `run` for why it is stated in both.
    #[tokio::test]
    async fn a_pause_turn_is_stopped_with_an_account_rather_than_re_sent() {
        let fake = test_support::FakeEndpoint::always(test_support::paused_turn_reply());
        let dir = scratch_dir("pause-turn");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let mut agent = Agent::new(
            fake.client(),
            journal,
            Arc::new(ToolRegistry::read_only(&dir)),
            Timeline::new(),
            AgentConfig::new(AgentId::new("executor"), "fake-model").max_steps(3),
            dir.clone(),
        );

        let outcome = agent
            .run("keep going")
            .await
            .expect("an unservable stop reason is an outcome, not an error");

        assert_eq!(fake.requests(), 1, "re-sending is the spin this replaced");
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.stopped_early, Some(LoopStop::PauseTurn));
        assert_eq!(outcome.stop, StopDisposition::Resume);
        assert_eq!(outcome.text, "pausing");
        assert!(
            outcome
                .stopped_early
                .expect("set above")
                .describe()
                .contains("pause_turn"),
            "the account has to name what happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // A spill the store refused must not become a pointer
    // -----------------------------------------------------------------------

    /// A tool whose payload is large enough, and irreproducible enough, to take
    /// the spill path.
    struct BigResult;

    const BIG_BYTES: usize = 20_000;

    fn big_payload() -> String {
        "x".repeat(BIG_BYTES)
    }

    impl Tool for BigResult {
        fn name(&self) -> &str {
            "big_result"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition::Custom(CustomTool::new(
                self.name(),
                "Return a large, irreproducible payload.",
                json!({"type": "object", "properties": {}}),
            ))
        }

        fn execute(&self, _input: &Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                payload: Payload::Inline(big_payload()),
                kind: ResultKind::File,
                size: BIG_BYTES as u64,
                reproducible: false,
                addressable: true,
                provenance: Provenance::new(self.name(), json!({})),
            }
        }
    }

    fn big_call() -> ToolUseBlock {
        ToolUseBlock {
            id: "call_00_big".to_owned(),
            name: "big_result".to_owned(),
            input: json!({}),
        }
    }

    fn registry() -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(BigResult));
        Arc::new(registry)
    }

    use super::test_support::scratch_dir;

    /// The text the model receives — the place a pointer would lie in.
    fn block_text(block: &ContentBlock) -> String {
        let ContentBlock::ToolResult(block) = block else {
            panic!("expected a tool_result block, got {block:?}");
        };
        block
            .content
            .as_ref()
            .and_then(|content| content.as_text())
            .expect("a tool result carries text")
            .to_owned()
    }

    /// A short prefix of `text` for a failure message; payloads are tens of KB.
    fn snippet(text: &str) -> &str {
        let mut end = text.len().min(200);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    #[tokio::test]
    async fn a_refused_spill_is_passed_through_and_never_named_as_a_blob() {
        let dir = scratch_dir("spill-refused");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let payload = big_payload();

        // A file where the payload's shard directory would have to be created:
        // the store cannot file these bytes, which is the failure under test.
        let shard = dir.join("blobs").join(&digest_of(payload.as_bytes())[..2]);
        std::fs::write(&shard, b"in the way").unwrap();

        let (block, _) = run_one(
            registry(),
            ToolContext::with_default_budget(&dir),
            journal.clone(),
            big_call(),
            Pressure::high(),
            Timeline::new(),
        )
        .await
        .unwrap();

        // The model gets the bytes themselves and no address at all: the
        // provisional `fnv1a64:` digest resolves in no store.
        let text = block_text(&block);
        assert!(
            text.contains(&payload),
            "a refused spill must pass the payload through; the model got: {}",
            snippet(&text)
        );
        assert!(
            !text.contains("fnv1a64:")
                && !text.contains("saved as blob")
                && !text.contains("on disk"),
            "a refused spill must not claim an address that does not resolve: {}",
            snippet(&text)
        );

        // And the journal records where the bytes actually are.
        let events = journal.events().await.unwrap();
        let mut recorded = 0;
        for event in &events {
            if let EventBody::ToolResult {
                inline,
                blob,
                reduction,
                ..
            } = &event.body
            {
                recorded += 1;
                assert_eq!(
                    blob.as_deref(),
                    None,
                    "the journal must not name a blob the store does not hold"
                );
                assert_eq!(inline.as_deref(), Some(payload.as_str()));
                assert_eq!(reduction.as_deref(), Some("spill_failed_pass_through"));
            }
        }
        assert_eq!(recorded, 1, "exactly one tool result is recorded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stored_spill_names_the_blob_the_store_actually_holds() {
        // The pair to the test above: when the store accepts, the pointer names
        // a digest that resolves — so a refusal test cannot pass by the spill
        // path simply never running.
        let dir = scratch_dir("spill-stored");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let payload = big_payload();
        let digest = digest_of(payload.as_bytes());

        let (block, _) = run_one(
            registry(),
            ToolContext::with_default_budget(&dir),
            journal.clone(),
            big_call(),
            Pressure::high(),
            Timeline::new(),
        )
        .await
        .unwrap();

        assert!(
            journal.blobs().has(&digest),
            "the named blob must be in the store"
        );
        assert!(block_text(&block).contains(&digest));

        let events = journal.events().await.unwrap();
        let mut recorded = 0;
        for event in &events {
            if let EventBody::ToolResult {
                inline,
                blob,
                reduction,
                ..
            } = &event.body
            {
                recorded += 1;
                assert_eq!(blob.as_deref(), Some(digest.as_str()));
                assert_eq!(inline.as_deref(), None);
                assert_eq!(reduction.as_deref(), Some("spill_with_pointer"));
            }
        }
        assert_eq!(recorded, 1, "exactly one tool result is recorded");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
