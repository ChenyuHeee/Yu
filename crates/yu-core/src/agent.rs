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

use yu_journal::{BlobStore, EventBody, Journal};
use yu_observe::{
    AgentId, ShapeSignal, ShapeTracker, SpanKind, StepObservation, Timeline, TokenUsage,
};
use yu_tools::{Payload, Pressure, Reduction, ResultKind, ToolContext, ToolRegistry, ToolResult};
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::sse::StreamedMessage;
use yu_wire::types::{
    ContentBlock, Effort, Message, MessagesRequest, StopDisposition, StopReason, SystemBlock,
    Thinking, ToolUseBlock,
};

use crate::gate::{AdmissionGate, spawn_abortable};

/// DESIGN's default for the decision layer is `xhigh`; the execution layer runs
/// degraded, since it is doing bounded work under supervision.
pub const EXECUTOR_EFFORT: Effort = Effort::Medium;

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
            system: Vec::new(),
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

#[derive(Debug, Clone)]
pub struct LoopOutcome {
    pub steps: u32,
    pub text: String,
    pub stop: StopDisposition,
    pub usage: TokenUsage,
    pub tool_calls: u32,
    /// What the shape metrics noticed. DESIGN: the decision layer watches these
    /// instead of reading the tool stream.
    pub signals: Vec<ShapeSignal>,
    /// Which reduction each tool result took, for M1's accounting.
    pub reductions: Vec<Reduction>,
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
                    // `pause_turn`: a server tool paused mid-turn. Re-send to let
                    // it continue; the assistant turn is already recorded.
                    tracing::debug!(step, "pause_turn — resuming");
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
                    });
                }
                StopDisposition::Truncated => return Err(LoopError::Truncated { steps: step }),
                StopDisposition::Refused => return Err(LoopError::Refused),
                StopDisposition::ContextExhausted => return Err(LoopError::ContextExhausted),
                StopDisposition::Unrecognized(raw) => {
                    return Err(LoopError::UnrecognizedStop(raw));
                }
            }
        }

        Err(LoopError::StepCeiling(self.config.max_steps))
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
        let thinking: String = msg
            .content
            .iter()
            .filter_map(|b| b.as_thinking())
            .map(|t| t.thinking.as_str())
            .collect::<Vec<_>>()
            .join("");
        let signature = msg
            .content
            .iter()
            .filter_map(|b| b.as_thinking())
            .find_map(|t| t.signature.clone());

        // Text to a blob, signature as its own field — never joined. Measured: a
        // signature is 36 bytes and can outweigh the text beside it.
        let thinking_blob = if thinking.is_empty() {
            None
        } else {
            Some(self.journal.blobs().put_str(&thinking)?)
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
                    thinking_blob,
                    thinking_signature: signature,
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
            handles.push((
                index,
                spawn_abortable(async move {
                    let _admission = gate.admit(true).await;
                    run_one(registry, ctx, journal, tu, pressure).await
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
) -> Result<(ContentBlock, Reduction), yu_wire::WireError> {
    let name = tu.name.clone();
    let input = tu.input.clone();

    // `ctx` is needed both inside the blocking task and afterwards, for
    // reduction, so the task gets its own clone.
    let ctx_for_tool = ctx.clone();
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

    // Spilling goes through the journal's content-addressed store, so a re-read
    // of a spilled payload is the same digest and the reducer can refuse to
    // spill it twice.
    let blobs: &BlobStore = journal.blobs();
    let mut spill = |bytes: &[u8]| -> String {
        blobs.put(bytes).unwrap_or_else(|e| {
            tracing::error!(error = %e, "spill failed; the pointer will be dangling");
            String::new()
        })
    };
    let (reduction, reduced) = ctx.reduce(raw, pressure, &mut spill);

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
                reduction: Some(reduction_name(&reduction).to_owned()),
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

fn reduction_name(r: &Reduction) -> &'static str {
    match r {
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

fn request_digest(req: &MessagesRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(req.model.as_bytes());
    hasher.update(req.max_tokens.to_le_bytes());
    for m in &req.messages {
        hasher.update(m.content.concat_text().as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn executor_effort_is_below_the_decision_layer_default() {
        // DESIGN: the decision layer runs `xhigh`; the execution layer is
        // deliberately degraded.
        assert_eq!(EXECUTOR_EFFORT, Effort::Medium);
    }
}
