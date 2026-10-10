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
    /// **The falsifiable number.** Measured over this layer's real context.
    pub composition: ContextComposition,
    /// Every delegation's reduced outcome, for the caller's own audit.
    pub outcomes: Vec<DelegateOutcome>,
}

impl DecisionOutcome {
    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        self.composition.tool_machinery_ratio()
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
        let mut meter = CompositionMeter::new();
        let mut usage = TokenUsage::default();
        let mut all_outcomes: Vec<DelegateOutcome> = Vec::new();
        let mut delegations = 0u32;

        self.journal
            .append(
                self.config.id.clone(),
                EventBody::SessionStarted {
                    charter: self.charter.text.clone(),
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

            meter.add_system(&system);
            meter.add_tools(&tools);
            for m in &messages {
                meter.add_message(m);
            }

            if let Err(problem) = self.client.quirks().check_history(&messages) {
                return Err(LoopError::History(problem));
            }

            // ---- one round: delegate until the layer stops asking
            let mut round_delegations = 0u32;
            loop {
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
                        let msg = Message::assistant(assistant);
                        meter.add_message(&msg);
                        let final_text = sink.text.clone();
                        self.journal
                            .append(
                                self.config.id.clone(),
                                EventBody::Note {
                                    text: format!("decision reached after {round} round(s)"),
                                },
                            )
                            .await?;
                        return Ok(DecisionOutcome {
                            rounds: round,
                            delegations,
                            final_text,
                            usage,
                            composition: meter.snapshot(),
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
                meter.add_message(&results);
                messages.push(results);

                if let Err(problem) = self.client.quirks().check_history(&messages) {
                    return Err(LoopError::History(problem));
                }
            }
        }

        Err(LoopError::StepCeiling(self.config.max_rounds))
    }

    fn build_view(&self, report: Option<&str>, outcomes: &[DelegateOutcome]) -> View {
        // Evidence from every delegation so far is carried, because DESIGN makes
        // an assertion without runnable evidence unverified, and the decision
        // layer is the one that has to notice.
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
        // this whole module exists to avoid. Evidence and shape are carried in
        // full because they are what the layer judges, and both are bounded by
        // construction.
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
    fn composition_counts_delegate_returns_as_tool_machinery() {
        // The metric has to see the delegate round trip, or it would report a
        // perfect zero for a layer that does nothing but delegate.
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
        meter.add_message(&Message::user(vec![ContentBlock::ToolResult(
            ToolResultBlock::text("call_00_x", "a report of some length"),
        )]));

        let composition = meter.snapshot();
        assert!(composition.tool_call_chars > 0, "tool calls must be counted");
        assert!(
            composition.tool_result_chars > 0,
            "delegate returns are tool results and must be counted"
        );
        let ratio = composition.tool_machinery_ratio().unwrap();
        assert!(
            (0.0..1.0).contains(&ratio),
            "ratio must be a real fraction, got {ratio}"
        );
    }

    #[test]
    fn the_ratio_is_undefined_for_an_empty_context_rather_than_zero() {
        assert_eq!(CompositionMeter::new().snapshot().tool_machinery_ratio(), None);
    }

    #[test]
    fn composition_counts_every_message_the_layer_actually_saw() {
        // A meter that only counted the system prompt would report near-zero
        // tool machinery for any layer, which is the vacuous-metric failure.
        let mut meter = CompositionMeter::new();
        for _ in 0..5 {
            meter.add_message(&Message::user(vec![ContentBlock::ToolResult(
                ToolResultBlock::text("call_00_x", &"x".repeat(1000)),
            )]));
        }
        let c = meter.snapshot();
        assert!(c.tool_result_chars >= 5000);
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
