//! The decision layer.
//!
//! DESIGN: "**直接与人类交互的一级 agent 扮演人类现在的角色** —— 不懂代码、不知道
//! 怎样调工具、甚至看不懂工具结果和返回代码,但承担最关键的决策任务。因此它的上下文
//! 极其宝贵,不能被浪费。"
//!
//! Everything here exists to protect that one resource:
//!
//! * Each round **re-renders** the view (`crate::render`) rather than appending to
//!   a transcript, so the context does not grow with the number of rounds. What
//!   survives between rounds is L0 and L1 — the charter and the ledger — and
//!   nothing else.
//! * The tool surface is `delegate`, not file or shell access. There is no way
//!   for the decision layer to pull a tool stream into its context even if it
//!   wanted to, because no tool returns one.
//! * The acceptance metric DESIGN specifies is computed here, over this layer's
//!   real context: what fraction of it is tool machinery. Industry shape is that
//!   it dominates; the target is near zero. The point of computing it is to make
//!   that claim falsifiable.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use yu_journal::{EventBody, Journal};
use yu_observe::{AgentId, ContextComposition, SpanKind, Timeline, TokenUsage};
use yu_tools::ToolRegistry;
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::types::{
    ContentBlock, Effort, Message, MessagesRequest, StopDisposition, StopReason, Thinking,
    ToolResultBlock,
};

use crate::agent::LoopError;
use crate::delegate::{self, DelegateArgs, DelegateOutcome};
use crate::render::{render, Charter, CompositionMeter, Ledger, RenderInput, View};

/// DESIGN: "`output_config.effort`: … 决策层 `xhigh`,执行层降档".
pub const DECISION_EFFORT: Effort = Effort::Xhigh;

/// The decision layer's own prompt.
///
/// DESIGN: "扒来的 `claude_code` preset 属于**执行层**…决策层没有现成 prompt,但**有
/// 规格了**". This is that spec, written for this layer.
pub const DECISION_PROMPT: &str = include_str!("../prompts/decision_layer.md");

#[derive(Debug, Clone)]
pub struct DecisionConfig {
    pub id: AgentId,
    pub model: String,
    pub max_tokens: u32,
    pub effort: Effort,
    /// How many times the layer may re-render and reconsider.
    pub max_rounds: u32,
    /// Ceiling on delegations within one round, so a layer that keeps asking for
    /// more reading cannot spend without bound.
    pub max_delegations_per_round: u32,
}

impl DecisionConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            id: AgentId::decision(),
            model: model.into(),
            max_tokens: 8192,
            effort: DECISION_EFFORT,
            max_rounds: 8,
            // One delegation per round, so that each round's context is
            // DESIGN's "L0 + L1 + 本轮报告" and nothing more.
            //
            // Measured: allowing six made the ratio scale with how many
            // delegations the layer chose to make, because every return stayed
            // in the message history for the rest of the round — it has to, since
            // a `tool_use` must be answered. Tool machinery reached 49.6%.
            // Capping at one means each report is dropped when the round ends
            // and the next view is rebuilt, which is what the design describes
            // and what keeps the context from growing with the work.
            max_delegations_per_round: 1,
        }
    }
}

/// What the decision layer reports back to whoever invoked it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionOutcome {
    pub rounds: u32,
    pub delegations: u32,
    pub final_text: String,
    pub usage: TokenUsage,
    /// **The falsifiable number**: the context the run *ended* on.
    pub composition: ContextComposition,
    /// Every request's context, in the order they were sent.
    ///
    /// A series rather than one number, because the criterion names *a context*
    /// and a run has many. The earlier version summed them into one meter, which
    /// measured their union — not a context — and charged the system prompt and
    /// the tool definitions to the denominator once per round. That inflation
    /// moved the ratio in the direction that flattered the design, which is the
    /// one direction a measurement here is not allowed to be wrong in.
    ///
    /// The series is also what makes growth visible: DESIGN says the view is
    /// bounded, and a claim of boundedness is only checkable against the shape
    /// of this curve rather than against its last point.
    pub contexts: Vec<ContextComposition>,
    /// Every delegation's reduced outcome, for the caller's own audit.
    pub outcomes: Vec<DelegateOutcome>,
}

impl DecisionOutcome {
    pub fn raw_material_ratio(&self) -> Option<f64> {
        self.composition.raw_material_ratio()
    }

    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        self.composition.tool_machinery_ratio()
    }

    /// The largest context of the run, by total size.
    ///
    /// The worst point rather than the last one, because a run that peaked at
    /// 40 000 characters and ended at 4 000 did not stay small — and "跑完一个
    /// 真实任务后" is a question about the run, not about its final frame.
    pub fn peak(&self) -> Option<&ContextComposition> {
        self.contexts.iter().max_by_key(|c| c.total_chars)
    }

    /// The highest raw-material ratio any single request reached.
    ///
    /// `None` when nothing was measured, which is reported as such rather than
    /// as zero.
    pub fn peak_raw_material_ratio(&self) -> Option<f64> {
        self.contexts
            .iter()
            .filter_map(|c| c.raw_material_ratio())
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
    }
}

#[derive(Default)]
struct SilentSink {
    text: String,
}

impl StreamSink for SilentSink {
    fn on_text(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

pub struct DecisionLayer {
    client: Arc<WireClient>,
    journal: Arc<Journal>,
    tools: Arc<ToolRegistry>,
    timeline: Arc<Timeline>,
    config: DecisionConfig,
    charter: Charter,
    ledger: Ledger,
    cwd: PathBuf,
}

impl DecisionLayer {
    pub fn new(
        client: Arc<WireClient>,
        journal: Arc<Journal>,
        tools: Arc<ToolRegistry>,
        timeline: Arc<Timeline>,
        config: DecisionConfig,
        charter: Charter,
        cwd: PathBuf,
    ) -> Self {
        Self {
            client,
            journal,
            tools,
            timeline,
            config,
            charter,
            ledger: Ledger::default(),
            cwd,
        }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Run the layer to a decision.
    pub async fn run(&mut self) -> Result<DecisionOutcome, LoopError> {
        let mut usage = TokenUsage::default();
        let mut contexts: Vec<ContextComposition> = Vec::new();
        let mut all_outcomes: Vec<DelegateOutcome> = Vec::new();
        let mut delegations = 0u32;
        // Taken from the tool the view actually advertises rather than written
        // out again here: a copy of the name is a second source of truth, and
        // the failure it produces — advertising one tool and accepting another —
        // is silent.
        let only_tool = delegate::delegate_tool().name().to_owned();

        self.journal
            .append(
                self.config.id.clone(),
                EventBody::SessionStarted {
                    // The intent, not the `text` field: a charter that arrived
                    // as the human's own sentence has an empty `text`, and
                    // recording that verbatim field would write an empty charter
                    // into L2.
                    charter: self.charter.intent().to_owned(),
                    model: self.config.model.clone(),
                    quirks: self.client.quirks().name.clone(),
                },
            )
            .await?;

        for round in 1..=self.config.max_rounds {
            self.timeline.enter(self.config.id.clone());

            // ---- render, fresh. This is the whole point: the view is built from
            // L0 + L1 + this round's inputs, never appended to a running history.
            let assemble = self.timeline.start(SpanKind::Assemble);
            let report = all_outcomes.last().map(DelegateOutcome::render);
            let view = self.build_view(report.as_deref(), &all_outcomes);
            let (system, tools, mut messages) =
                (view.system.clone(), view.tools.clone(), view.messages.clone());
            assemble.finish();

            // Measured once per *request*, and this meter is rebuilt for each
            // round rather than carried across them. A run has many contexts;
            // summing them measures their union, which is not a context and which
            // charges the system prompt and the tool definitions to the
            // denominator once per round.
            let mut meter = CompositionMeter::new();
            // The view's own parts arrive already classified: `render` knew which
            // section was a reduction and which was a tool's output, and
            // recomputing that here would be a second answer to the same question.
            meter.add_composition(&view.composition);

            if let Err(problem) = self.client.quirks().check_history(&messages) {
                return Err(LoopError::History(problem));
            }

            // ---- one round: delegate until the layer stops asking
            let mut round_delegations = 0u32;
            loop {
                // The context this request will actually carry. Recorded before
                // it is sent, because this is the thing the criterion asks about.
                contexts.push(meter.snapshot());

                let mut req = MessagesRequest::new(
                    self.config.model.clone(),
                    self.config.max_tokens,
                    messages.clone(),
                )
                .system(system.clone())
                .tools(tools.clone())
                .thinking(Thinking::adaptive())
                .effort(self.config.effort);

                // Decision-layer turns are short and the loop is driven by tool
                // results, not by a long generation.
                req.max_tokens = self.config.max_tokens;

                let digest = request_digest_of(&req);
                self.journal
                    .append(
                        self.config.id.clone(),
                        EventBody::ModelRequest {
                            model: req.model.clone(),
                            messages: req.messages.len(),
                            digest,
                            est_input_tokens: None,
                        },
                    )
                    .await?;

                let mut sink = SilentSink::default();
                let outcome = self.client.stream(&req, &mut sink).await?;
                usage.add(&outcome.message.usage.to_observed());

                let assistant = outcome.message.content.clone();
                let stop = outcome
                    .message
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| StopReason::Unknown("<absent>".to_owned()));

                match stop.disposition() {
                    StopDisposition::RunTools => {}
                    StopDisposition::Complete => {
                        let final_text = sink.text.clone();
                        self.journal
                            .append(
                                self.config.id.clone(),
                                EventBody::Note {
                                    text: format!("decision reached after {round} round(s)"),
                                },
                            )
                            .await?;
                        // The context the run ended on is the last one it *sent*.
                        // The assistant turn that just arrived is the answer, not
                        // part of any context this layer carried, so charging it
                        // would be charging the layer for its own output.
                        let composition = contexts.last().copied().unwrap_or_default();
                        return Ok(DecisionOutcome {
                            rounds: round,
                            delegations,
                            final_text,
                            usage,
                            composition,
                            contexts,
                            outcomes: all_outcomes,
                        });
                    }
                    StopDisposition::Truncated => {
                        return Err(LoopError::Truncated { steps: round });
                    }
                    StopDisposition::Refused => return Err(LoopError::Refused),
                    StopDisposition::ContextExhausted => {
                        return Err(LoopError::ContextExhausted)
                    }
                    StopDisposition::Unrecognized(raw) => {
                        return Err(LoopError::UnrecognizedStop(raw))
                    }
                    // A server tool pausing is meaningless here: this layer has
                    // no server tools. Treating it as "continue" would spin.
                    StopDisposition::Resume => {
                        return Err(LoopError::UnrecognizedStop("pause_turn".to_owned()));
                    }
                }

                // ---- run the delegations, all results in ONE user message
                let uses: Vec<_> = assistant
                    .iter()
                    .filter_map(|b| b.as_tool_use().cloned())
                    .collect();

                let assistant_msg = Message::assistant(assistant);
                meter.add_message(&assistant_msg);
                messages.push(assistant_msg);

                let mut result_blocks = Vec::new();
                for tu in &uses {
                    // Check the name before the arguments. Every `tool_use` used
                    // to be deserialised straight into `DelegateArgs`, so a call
                    // to any other tool whose JSON happened to carry a `task`
                    // field was executed as a delegation — and the prompt names
                    // the tools this layer is meant to grow (`decide`, `retire`),
                    // which makes "the model called one of them" a state the
                    // harness has to answer rather than one it can assume away.
                    if tu.name != only_tool {
                        result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::error(
                            &tu.id,
                            format!(
                                "there is no tool named `{}` in this layer; the only tool \
                                 available is `{}`",
                                tu.name, only_tool
                            ),
                        )));
                        continue;
                    }
                    if round_delegations >= self.config.max_delegations_per_round {
                        result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::error(
                            &tu.id,
                            format!(
                                "delegation ceiling of {} reached for this round. Decide with \
                                 what you have.",
                                self.config.max_delegations_per_round
                            ),
                        )));
                        continue;
                    }
                    round_delegations += 1;
                    delegations += 1;

                    let args: DelegateArgs = match serde_json::from_value(tu.input.clone()) {
                        Ok(a) => a,
                        Err(e) => {
                            result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::error(
                                &tu.id,
                                format!("could not read the delegation arguments: {e}"),
                            )));
                            continue;
                        }
                    };

                    let outcome = delegate::run_delegation(
                        &args,
                        self.client.clone(),
                        self.journal.clone(),
                        self.tools.clone(),
                        self.timeline.clone(),
                        self.cwd.clone(),
                    )
                    .await?;

                    let rendered = outcome.render();
                    self.journal
                        .append(
                            self.config.id.clone(),
                            EventBody::Note {
                                text: format!(
                                    "delegation {}: {} step(s), {} evidence block(s), {} bytes \
                                     returned to the decision layer",
                                    delegations,
                                    outcome.steps,
                                    outcome.evidence.len(),
                                    rendered.len()
                                ),
                            },
                        )
                        .await?;
                    result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::text(
                        &tu.id, rendered,
                    )));
                    all_outcomes.push(outcome);
                }

                let results = Message::user(result_blocks);
                // A delegation's return is a **reduction**, not raw material: it
                // is `DelegateOutcome::render`, truncated to a budget with an
                // address for the rest. Counting it as raw material is exactly
                // the confusion the metric was changed to end — it made "this
                // layer holds one report" indistinguishable from "this layer
                // holds fifty thousand lines of grep".
                meter.add_reduced_message(&results);
                messages.push(results);

                if let Err(problem) = self.client.quirks().check_history(&messages) {
                    return Err(LoopError::History(problem));
                }
            }
        }

        // Reachable **only** when `max_rounds` is zero. The inner loop above has
        // no `break` — a round converses until the model stops asking — so for
        // any `max_rounds >= 1` a completed run always returned from inside it,
        // and the outer `for` never advanced past its first iteration. Measured:
        // a run with `--max-rounds 1` makes as many requests as it likes, and
        // every run that ends reports `1 round(s)`.
        //
        // That is not a metric defect — the reported round count is true — but
        // `max_rounds` is a user-facing knob that bounds nothing, and this arm
        // reading like a live guard is worse than it being absent.
        //
        // It belongs to the milestone that makes re-rendering mean something:
        // rounds only differ once L1 is written between them, and today the
        // ledger is never written during a run. The guard that pins this is
        // `the_round_ceiling_does_not_bound_a_run_that_keeps_asking` in
        // `tests/decision_layer_loop.rs`, which fails the moment a round can
        // actually end — forcing whoever makes it real to say so deliberately
        // rather than to discover it.
        Err(LoopError::StepCeiling(self.config.max_rounds))
    }

    fn build_view(&self, report: Option<&str>, outcomes: &[DelegateOutcome]) -> View {
        // Evidence from every delegation so far is carried, because DESIGN makes
        // an assertion without runnable evidence unverified, and the decision
        // layer is the one that has to notice.
        //
        // **This is unbounded, and it is the reason the view is not.** Each
        // `Evidence.content` is a tool's output rendered verbatim, and this
        // carries all of them for the whole run, so round N re-renders round 1's
        // evidence for the Nth time — the view grows with the square of the
        // number of rounds. An earlier version of this comment said evidence was
        // "bounded by construction"; it is not, and the measurement now says so
        // (`ContextComposition::raw_material_chars` counts exactly this).
        //
        // It is left as it is here deliberately rather than trimmed, because
        // trimming it in place would be compression — guessing which evidence
        // mattered. DESIGN's answer is that the decision layer should not be
        // reading raw evidence at all: M3 replaces this section with
        // pre-registered landing slots, and `spot_check` becomes the one channel
        // that can pull a tool's output in, charged to this layer's own account.
        let evidence: Vec<_> = outcomes
            .iter()
            .flat_map(|o| o.evidence.iter().cloned())
            .collect();
        let shape: Vec<_> = outcomes
            .iter()
            .flat_map(|o| o.shape.iter().cloned())
            .collect();
        // Only the last delegation's manifest. Earlier rounds' omissions are
        // already addressed by the record; restating them every round would make
        // the manifest grow with the number of rounds, which is the accumulation
        // this whole module exists to avoid. The manifest is addresses rather than
        // payloads, so unlike the evidence above it really is bounded — by the
        // record's size, not by the run's length.
        let omitted: Vec<_> = outcomes
            .last()
            .map(|o| o.omitted.clone())
            .unwrap_or_default();

        render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &self.charter,
            ledger: &self.ledger,
            tools: &[delegate::delegate_tool()],
            report,
            evidence: &evidence,
            shape: &shape,
            omitted: &omitted,
        })
    }
}

fn request_digest_of(req: &MessagesRequest) -> String {
    // Reuses the executor's digest so a request is identified the same way
    // wherever it is recorded.
    crate::agent::request_digest(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentConfig;
    use crate::render::{Evidence, NotLoadedEntry};
    use yu_wire::types::SystemBlock;
    use serde_json::json;

    #[test]
    fn the_decision_prompt_is_the_original_one_not_a_borrowed_coding_prompt() {
        // DESIGN: "扒来的 claude_code preset 属于执行层…决策层没有现成 prompt".
        // The failure this guards is a coding-agent prompt leaking in, which
        // would show up as the layer reading files and running commands.
        assert!(DECISION_PROMPT.contains("You are the decision layer"));
        // It must refuse the executor's surface rather than offer it.
        assert!(
            DECISION_PROMPT.contains("You do not do the work"),
            "the prompt must disclaim the executor's role"
        );
        assert!(
            DECISION_PROMPT.contains("do not have file access"),
            "the prompt must refuse file and shell access"
        );
        // And it must name the surface it *does* have.
        assert!(DECISION_PROMPT.contains("delegate"));
    }

    #[test]
    fn the_prompt_states_the_four_things_it_reads_and_the_five_triggers() {
        // These are DESIGN's spec for this layer; losing one silently changes
        // what the layer is.
        for needle in [
            "The plan",
            "final reply",
            "evidence",
            "The diff",
            "The same failure twice",
            "Scope drift",
            "wrong surface",
            "no runnable evidence",
            "attributed elsewhere",
        ] {
            assert!(
                DECISION_PROMPT.contains(needle),
                "the decision prompt no longer covers `{needle}`"
            );
        }
    }

    #[test]
    fn the_prompt_keeps_the_judgement_that_must_not_be_delegated() {
        for needle in [
            "what counts as done",
            "architecture and interface",
            "security-sensitive",
            "shared branch",
            "irreversible",
            "whether the evidence actually proves the claim",
        ] {
            assert!(
                DECISION_PROMPT.contains(needle),
                "the prompt no longer reserves `{needle}` to the decision layer"
            );
        }
    }

    #[test]
    fn the_prompt_carries_the_one_sentence_test_and_the_re_anchoring_order() {
        assert!(DECISION_PROMPT.contains("one sentence"));
        let reanchor = DECISION_PROMPT
            .find("Re-anchor")
            .expect("re-anchoring must be specified");
        let tail = &DECISION_PROMPT[reanchor..];
        let goal = tail.find("the goal").unwrap();
        let constraint = tail.find("the constraint").unwrap();
        let criterion = tail.find("the criterion").unwrap();
        assert!(
            goal < constraint && constraint < criterion,
            "re-anchoring must state goal, then constraint, then criterion"
        );
    }

    #[test]
    fn the_decision_layer_runs_above_the_executor_not_below_it() {
        assert_eq!(DECISION_EFFORT, Effort::Xhigh);
        assert_ne!(DECISION_EFFORT, crate::agent::EXECUTOR_EFFORT);
    }

    #[test]
    fn the_executor_prompt_is_actually_attached() {
        // For a while the prompt existed as a file that nothing loaded, so the
        // executor ran with no system prompt — the reporting discipline written
        // down and never handed to anyone.
        let config = AgentConfig::new(AgentId::new("executor"), "m");
        assert!(
            !config.system.is_empty(),
            "an executor with no system prompt has been given no constraints"
        );
        let text = &config.system[0].text;
        assert!(text.contains("You are an executor"));
        for needle in [
            "no memory of any previous work",
            "Show the evidence",
            "Do not attribute failures elsewhere",
            "Do not go beyond the task",
            "stop and report",
        ] {
            assert!(
                text.contains(needle),
                "the executor prompt no longer covers `{needle}`"
            );
        }
    }

    #[test]
    fn only_the_delegate_tool_is_offered() {
        // DESIGN: "工具面:`delegate` / `ask_human` / `retire` / `decide` /
        // `spot_check` —— **不是** Read/Write/Bash". The thinnest slice builds
        // one of the five; the important half of the claim is what is absent.
        let tool = delegate::delegate_tool();
        assert_eq!(tool.name(), "delegate");
        assert!(!tool.is_server_defined());
        assert!(matches!(tool, yu_wire::types::ToolDefinition::Custom(_)));
    }

    #[test]
    fn a_delegate_return_is_a_reduction_rather_than_raw_material() {
        // The metric has to see the delegate round trip — a layer that does
        // nothing but delegate would otherwise report a perfect zero — and it
        // has to see it as what it is. `DelegateOutcome::render` truncated the
        // report to a budget and gave an address for the rest, so nothing here
        // came straight off a tool.
        let mut meter = CompositionMeter::new();
        meter.add_system(&[SystemBlock::text("system prompt")]);
        meter.add_message(&Message::user("the rendered view"));
        meter.add_message(&Message::assistant(vec![ContentBlock::ToolUse(
            yu_wire::types::ToolUseBlock {
                id: "call_00_x".into(),
                name: "delegate".into(),
                input: json!({"task": "go and read the whole repository"}),
            },
        )]));
        meter.add_reduced_message(&Message::user(vec![ContentBlock::ToolResult(
            ToolResultBlock::text("call_00_x", &"a report of some length".repeat(100)),
        )]));

        let composition = meter.snapshot();
        assert!(composition.tool_call_chars > 0, "tool calls must be counted");
        assert!(
            composition.reduced_result_chars > 1000,
            "a delegate return is a reduction and must be charged as one"
        );
        assert_eq!(
            composition.raw_material_chars, 0,
            "nothing in this context came straight off a tool"
        );
        assert_eq!(composition.raw_material_ratio(), Some(0.0));
        // Still machinery, though — which is why the two ratios are read
        // separately rather than one replacing the other.
        assert!(
            composition.tool_machinery_ratio().unwrap() > 0.5,
            "the delegate round trip is real machinery in this context"
        );
    }

    #[test]
    fn the_ratio_is_undefined_for_an_empty_context_rather_than_zero() {
        assert_eq!(CompositionMeter::new().snapshot().raw_material_ratio(), None);
        assert_eq!(CompositionMeter::new().snapshot().tool_machinery_ratio(), None);
    }

    /// **M1's acceptance criterion, as a test.**
    ///
    /// One number, and it can tell "this layer holds fifty thousand lines of
    /// grep" from "this layer holds one report". The two contexts below are
    /// deliberately the *same size* and carry the *same content block type*, so
    /// they are indistinguishable to anything that classifies by shape — which is
    /// what the tool-machinery ratio did, and why it reported the same figure for
    /// both.
    #[test]
    fn the_metric_separates_a_report_from_the_same_volume_of_raw_output() {
        let volume = 2000usize;
        let report = {
            let mut meter = CompositionMeter::new();
            meter.add_system(&[SystemBlock::text(&"s".repeat(volume))]);
            meter.add_reduced_message(&Message::user(vec![ContentBlock::ToolResult(
                ToolResultBlock::text("call_00_x", &"r".repeat(volume)),
            )]));
            meter.snapshot()
        };
        let grep = {
            let mut meter = CompositionMeter::new();
            meter.add_system(&[SystemBlock::text(&"s".repeat(volume))]);
            // Same shape, same size, and nothing has judged a byte of it.
            meter.add_message(&Message::user(vec![ContentBlock::ToolResult(
                ToolResultBlock::text("call_00_x", &"g".repeat(volume)),
            )]));
            meter.snapshot()
        };

        assert_eq!(
            report.raw_material_ratio(),
            Some(0.0),
            "a reduced report is a product of judgment, not poured-in material"
        );
        assert!(
            grep.raw_material_ratio().unwrap() > 0.4,
            "the same volume of unjudged tool output has to read differently, got {:?}",
            grep.raw_material_ratio()
        );
        // The separation is the whole point, so the old metric's inability to
        // make it is asserted rather than described — that is what stops it from
        // being quietly reintroduced as "the same number".
        assert_eq!(
            report.tool_machinery_ratio(),
            grep.tool_machinery_ratio(),
            "the tool-machinery ratio cannot tell these two contexts apart, which \
             is precisely why it is not the criterion"
        );
    }

    #[test]
    fn composition_counts_every_message_the_layer_actually_saw() {
        // A meter that only counted the system prompt would report near-zero for
        // any layer, which is the vacuous-metric failure.
        let mut meter = CompositionMeter::new();
        for _ in 0..5 {
            meter.add_message(&Message::user(vec![ContentBlock::ToolResult(
                ToolResultBlock::text("call_00_x", &"x".repeat(1000)),
            )]));
        }
        let c = meter.snapshot();
        // Unreduced tool output is raw material by default: the unflattering
        // reading is the one a caller has to opt out of, not into.
        assert!(c.raw_material_chars >= 5000);
        assert!(c.total_chars >= 5000);
    }

    #[test]
    fn the_view_is_rebuilt_each_round_rather_than_appended_to() {
        // Structural: `run` builds `messages` from `view.messages` at the top of
        // every round, so nothing from a previous round survives except L0/L1.
        // Proven here by checking that render's output length does not depend on
        // how many delegations have already happened.
        let charter = Charter {
            text: "ship it".into(),
            ..Charter::default()
        };
        let ledger = Ledger::default();
        let one = render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &charter,
            ledger: &ledger,
            tools: &[],
            report: Some("first"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let many = render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &charter,
            ledger: &ledger,
            tools: &[],
            report: Some("tenth"),
            evidence: &[Evidence::command("cargo test", "ok", 0)],
            shape: &[],
            omitted: &[NotLoadedEntry {
                what: "a".into(),
                address: "blob:x".into(),
                bytes: 10,
                why: "y".into(),
                how_to_load: "z".into(),
            }],
        });
        // The prefix has a fixed shape — prompt, L0, L1 — however much history
        // exists, and the messages are rebuilt rather than appended to.
        assert_eq!(one.system.len(), many.system.len());
        assert_eq!(one.system.len(), 3, "prompt + L0 + L1");
        assert_eq!(one.messages.len(), 1);
        assert_eq!(many.messages.len(), 1, "the view stays one message");

        // The property that makes the cache work: L0 is byte-identical between
        // the two, so a change further down the request cannot invalidate the
        // charter's cached prefix.
        assert_eq!(
            one.system[1], many.system[1],
            "adding evidence and a manifest must not disturb L0"
        );
    }
}
