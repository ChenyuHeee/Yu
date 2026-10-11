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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use yu_journal::{EventBody, Journal};
use yu_observe::{AgentId, ContextComposition, ShapeSignal, SpanKind, Timeline, TokenUsage};
use yu_tools::ToolRegistry;
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::types::{
    ContentBlock, Effort, Message, MessagesRequest, StopDisposition, StopReason, Thinking,
    ToolResultBlock,
};

use crate::agent::LoopError;
use crate::decide;
use crate::delegate::{self, DelegateArgs, DelegateOutcome, Standing};
use crate::render::{
    render, Charter, CompositionMeter, Ledger, NotLoadedEntry, RenderInput, View,
};
use crate::topology::ExecutorRunner;

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

/// How many turns in a row may run nothing before the round ends.
///
/// Three, because the decision layer's own prompt sets the rule: "The same
/// failure twice. Stop. Do not ask for a third correction — a third attempt at a
/// failure that has already happened twice is not a correction, it is a loop."
/// Two barren turns are a mistake and its correction; the third is the loop, and
/// the round ends there rather than paying for it.
const MAX_BARREN_TURNS_PER_ROUND: u32 = 3;

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
    /// **L1 as the run left it.**
    ///
    /// Carried rather than left on the layer because it is not a by-product of
    /// the run — it is what the run produced. DESIGN's L1 is *state*: it is what
    /// survives across rounds, across generations, and across a restart, and a
    /// state the caller cannot obtain is a state that only exists until the
    /// process exits.
    #[serde(default)]
    pub ledger: Ledger,
    /// Why the run ended without the layer deciding, when it did.
    ///
    /// `None` means the layer reached a decision. `Some` means it did not — and
    /// the caller has to be able to tell, because `final_text` for a stopped run
    /// is the layer's last narration rather than its answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_early: Option<String>,
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

    /// How the run's landings fell across each pre-registered set.
    ///
    /// DESIGN keeps this and **only** this about predictions that did not come
    /// true: *"未命中的预测不留 … 唯一要留的是**落点分布的计数**:某个分支永远被选中,
    /// 这组就是装饰;`other` 常见,这组就是错的."* A guess that did not happen is not
    /// evidence of anything — negative evidence is "we tried X and it failed",
    /// which is an action with a result, not an expectation with none.
    ///
    /// The counts are what makes a set checkable after the fact. A slot nobody
    /// ever lands in is decoration; a set whose `other` fills up is a set that
    /// did not describe the work. Neither is visible from a single run's prose,
    /// and both are visible from the tally.
    pub fn landing_distribution(&self) -> BTreeMap<String, BTreeMap<String, u32>> {
        let mut by_set: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
        for outcome in &self.outcomes {
            let Some(landing) = &outcome.landing else {
                continue;
            };
            // Keyed by the *slot*, which is what the set is judging; the set id
            // groups them for a reader who wants to know which set to fix.
            let set = by_set.entry(landing.set.clone()).or_default();
            *set.entry(landing.slot.clone()).or_default() += 1;
        }
        by_set
    }

    /// The two findings DESIGN says the distribution is for, stated rather than
    /// left for a reader to notice in a table.
    ///
    /// Empty when nothing was pre-registered, and empty when the tally shows
    /// neither problem — which are different things, so callers that care should
    /// check [`Self::landing_distribution`] rather than inferring from silence.
    pub fn landing_problems(&self) -> Vec<String> {
        let mut found = Vec::new();
        for (set, counts) in self.landing_distribution() {
            let total: u32 = counts.values().sum();
            if total == 0 {
                continue;
            }
            let others = counts.get(crate::landing::OTHER).copied().unwrap_or(0);
            if others * 2 > total {
                found.push(format!(
                    "set `{set}`: {others} of {total} landings fell in `other`, so the set \
                     does not describe the work it was declared for"
                ));
            }
        }
        found
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
    /// The prediction sets a policy declared, if one was loaded.
    ///
    /// Held by the layer because the layer is what *chooses* — the executor is
    /// told a set, it does not pick one, and the model names an id rather than
    /// writing slots. Empty means none were declared, and then `delegate` is
    /// offered without the field at all rather than with a blank one.
    outcome_sets: crate::outcomes::OutcomeSets,
    /// How many agents this layer has derived, across every round.
    ///
    /// Carried so that each derived agent has its own address in the record.
    /// Two delegations that both numbered from one would put two runs under one
    /// name, and the record is the only thing that says which of them ran.
    derived: u32,
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
            outcome_sets: crate::outcomes::OutcomeSets::default(),
            derived: 0,
        }
    }

    /// Give the layer the prediction sets a policy declared.
    ///
    /// DESIGN: *"预测集是策略,不是每次现编"* — so this is the only way a set reaches
    /// a delegation, and a layer without one offers `delegate` with no `outcomes`
    /// field rather than a free-text one.
    pub fn with_outcome_sets(mut self, sets: crate::outcomes::OutcomeSets) -> Self {
        self.outcome_sets = sets;
        self
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
        // What the most recent delegation derived, and what it rendered to.
        //
        // Kept apart from `all_outcomes` because a fan-out is *one* delegation
        // that derived several agents, and the view's "this round" section and
        // its shape signals are about the delegation in front of the layer —
        // not about one of its children.
        let mut latest: Vec<DelegateOutcome> = Vec::new();
        let mut last_report: Option<String> = None;
        let mut delegations = 0u32;
        // The last thing the layer said. A run that exhausts its rounds still
        // produced one, and discarding it would leave the caller an error with
        // nothing in it — the shape this replaced, one layer down.
        let mut last_text = String::new();
        // Taken from the tool the view actually advertises rather than written
        // out again here: a copy of the name is a second source of truth, and
        // the failure it produces — advertising one tool and accepting another —
        // is silent.
        let only_tool = delegate::delegate_tool(&self.outcome_sets).name().to_owned();
        // Every name the layer was offered, for the message that tells it a name
        // it used is not one of them.
        let surface = delegate::decision_surface(&self.outcome_sets)
            .iter()
            .map(|tool| format!("`{}`", tool.name()))
            .collect::<Vec<_>>()
            .join(", ");

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
            let view = self.build_view(last_report.as_deref(), &all_outcomes, &latest);
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
            let mut barren_turns = 0u32;
            // Whether the layer wrote to L1 this round. Read by the boundary
            // below, and reset with the round because that is the scope it has
            // meaning in.
            let mut ledger_changed = false;
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
                if !sink.text.trim().is_empty() {
                    last_text = sink.text.clone();
                }

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
                            ledger: self.ledger.clone(),
                            stopped_early: None,
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
                // How many calls this turn actually *ran*, as opposed to were
                // answered. A turn whose every request was refused has nothing
                // to react to, and continuing to converse with it is how a run
                // spends without moving.
                let mut executed = 0u32;
                for tu in &uses {
                    // `spot_check`: the one door raw material may come through,
                    // and it is billed to this layer's own context — DESIGN:
                    // *"`spot_check` 的代价记在决策层自己的上下文账上,所以是稀缺动作"*.
                    // The cost is not a rule anyone enforces; it is that the
                    // bytes arrive here, in the context the layer has least of,
                    // and the meter counts them.
                    if tu.name == crate::spot::SPOT_CHECK {
                        let address = tu.input
                            .get("address")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let block = match crate::spot::resolve(&address, &self.journal).await {
                            Ok(spotted) => {
                                self.journal
                                    .append(
                                        self.config.id.clone(),
                                        EventBody::Note {
                                            text: format!(
                                                "spot_check {}: {} of {} byte(s) loaded into \
                                                 this layer",
                                                spotted.address,
                                                spotted.bytes,
                                                spotted.total_bytes.unwrap_or(spotted.bytes)
                                            ),
                                        },
                                    )
                                    .await?;
                                ToolResultBlock::text(&tu.id, spotted.text)
                            }
                            Err(problem) => {
                                ToolResultBlock::error(&tu.id, problem.to_string())
                            }
                        };
                        result_blocks.push(ContentBlock::ToolResult(block));
                        executed += 1;
                        continue;
                    }

                    // The ledger's own tools next: they are what the layer
                    // writes with, and a round in which it wrote something is a
                    // round that has to end so the next one can show it.
                    if decide::is_decision_tool(&tu.name) {
                        let applied = decide::apply(&tu.name, &tu.input, &mut self.ledger);
                        if applied.changed {
                            ledger_changed = true;
                            // L1 is state and lives in memory; a state that
                            // exists only in memory is one a resumed run cannot
                            // rebuild. The record gets the decision and its
                            // reason verbatim, which is also what makes
                            // `retire`'s "可审、可回滚" true rather than
                            // aspirational.
                            self.journal
                                .append(
                                    self.config.id.clone(),
                                    EventBody::Note {
                                        text: format!(
                                            "{} — {}",
                                            tu.name,
                                            // The *record*, not the model-facing
                                            // text: L2 has to be able to rebuild
                                            // the state, and the model-facing text
                                            // does not repeat the reason back.
                                            if applied.record.is_empty() {
                                                &applied.text
                                            } else {
                                                &applied.record
                                            }
                                        ),
                                    },
                                )
                                .await?;
                        }
                        result_blocks.push(ContentBlock::ToolResult(if applied.malformed {
                            ToolResultBlock::error(&tu.id, applied.text)
                        } else {
                            ToolResultBlock::text(&tu.id, applied.text)
                        }));
                        executed += 1;
                        continue;
                    }

                    // Then check the name before the arguments. Every `tool_use`
                    // used to be deserialised straight into `DelegateArgs`, so a
                    // call to any other tool whose JSON happened to carry a
                    // `task` field was executed as a delegation.
                    if tu.name != only_tool {
                        result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::error(
                            &tu.id,
                            format!(
                                "there is no tool named `{}` in this layer; the tools \
                                 available are {}",
                                tu.name, surface
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
                    // The budget is charged only once a delegation actually
                    // runs, at the bottom of this block. Measured: charging it
                    // here meant an ill-shaped call could not be corrected
                    // inside the round — the layer's own words, from a real run,
                    // were "an ill-shaped call cannot be corrected inside the
                    // same round — there is no in-round retry, and a wasted call
                    // costs the entire round's investigative capacity". A call
                    // that ran nothing has cost nothing.
                    let mut args: DelegateArgs = match serde_json::from_value(tu.input.clone()) {
                        Ok(a) => a,
                        Err(e) => {
                            result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::error(
                                &tu.id,
                                format!("could not read the delegation arguments: {e}"),
                            )));
                            continue;
                        }
                    };
                    // The model named an id; the policy says what it means. An id
                    // that is not in the policy is answered rather than defaulted:
                    // running the delegation without a set would silently drop
                    // the contract, and inventing one would be the layer writing
                    // a prediction set — the one thing DESIGN says it must not do.
                    match set_id_in(&tu.input) {
                        Some(id) => match self.outcome_sets.get(id) {
                            Some(set) => {
                                args.outcomes = Some(crate::landing::PlannedOutcomes {
                                    id: set.id.clone(),
                                    description: set.description.clone(),
                                    slots: set.slots.clone(),
                                });
                            }
                            None => {
                                result_blocks.push(ContentBlock::ToolResult(
                                    ToolResultBlock::error(
                                        &tu.id,
                                        format!(
                                            "there is no outcome set named `{id}`. The policy \
                                             declares: {}. Pick one of those; you cannot \
                                             invent a set, and a delegation with no set \
                                             cannot be checked against anything.",
                                            self.outcome_sets
                                                .ids()
                                                .iter()
                                                .map(|id| format!("`{id}`"))
                                                .collect::<Vec<_>>()
                                                .join(", ")
                                        ),
                                    ),
                                ));
                                continue;
                            }
                        },
                        // A policy declares sets, and this call named none.
                        //
                        // Refused rather than run unclassified, because an
                        // unchecked delegation is the one outcome that makes the
                        // whole mechanism optional: DESIGN's rule is that the
                        // layer *chooses* a set, not that it may decline to. The
                        // schema marks the field required; a required field
                        // nothing enforces is a suggestion.
                        None if !self.outcome_sets.ids().is_empty() => {
                            result_blocks.push(ContentBlock::ToolResult(
                                ToolResultBlock::error(
                                    &tu.id,
                                    format!(
                                        "this delegation named no outcome set, so nothing \
                                         about it could be checked. The policy declares: \
                                         {}. Pass one as `outcomes`.",
                                        self.outcome_sets
                                            .ids()
                                            .iter()
                                            .map(|id| format!("`{id}`"))
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ),
                                ),
                            ));
                            continue;
                        }
                        None => {}
                    }

                    round_delegations += 1;
                    delegations += 1;

                    // **The conditions, injected here.** The layer was given them
                    // in L0 this round; the executor is given them because they
                    // are put on the delegation, not because the layer
                    // remembered to restate them. Removing them from the charter
                    // therefore removes them from every request the run makes,
                    // which is the difference between a constraint and a
                    // convention.
                    args.standing = Standing {
                        constraints: self.charter.constraints.clone(),
                        topology: self.charter.topology.clone(),
                    };
                    args.depth = 1;

                    // **The declared shape governs the derivation.** DESIGN:
                    // "拓扑是可声明的策略,不是硬编码结构" — and a declaration that
                    // only changes what a view says is not a strategy. The runner
                    // carries the conditions down; the plan decides how many
                    // executors there are and what each is asked.
                    let runner = ExecutorRunner::new(
                        self.client.clone(),
                        self.journal.clone(),
                        self.tools.clone(),
                        self.timeline.clone(),
                        self.cwd.clone(),
                        self.config.id.clone(),
                        args.standing.clone(),
                    )
                    // The layer's own delegations are the first level: what it
                    // derives runs at depth 1, and may derive down to the ceiling.
                    .at(1)
                    .landing_in(args.outcomes.clone())
                    .starting_at(self.derived);
                    let outcomes = delegate::run_derivation(&args, &runner).await?;
                    self.derived += outcomes.len() as u32;

                    let rendered = delegate::render_derivation(&outcomes);
                    self.journal
                        .append(
                            self.config.id.clone(),
                            EventBody::Note {
                                text: format!(
                                    "delegation {delegations}: {} agent(s), {} step(s) each, \
                                     {} bytes returned to the decision layer",
                                    outcomes.len(),
                                    outcomes
                                        .iter()
                                        .map(|outcome| outcome.steps)
                                        .sum::<u32>(),
                                    rendered.len()
                                ),
                            },
                        )
                        .await?;
                    result_blocks.push(ContentBlock::ToolResult(ToolResultBlock::text(
                        &tu.id,
                        rendered.clone(),
                    )));
                    // Every derived agent's outcome is kept: the tally counts
                    // landings, and the manifest has to be able to address what
                    // the view does not carry.
                    latest = outcomes.clone();
                    all_outcomes.extend(outcomes);
                    last_report = Some(rendered);
                    executed += 1;
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

                // ---- the round boundary.
                //
                // A round is `render() -> converse -> write back`, and it ends
                // when the view it was rendered from can no longer serve the
                // next request.
                //
                // **A turn that ran nothing is not by itself a reason to end
                // it.** A call the layer got wrong — a name that does not exist,
                // arguments that do not parse — is answered with an error, and
                // the answer is only worth anything if the layer is given a
                // request to read it in. Breaking on the first barren turn threw
                // that answer away: the results message was pushed and then
                // discarded unread when the next round re-rendered. Measured, by
                // the test that asserts a misnamed call is *answered*.
                //
                // What does end a round is a turn that could not move three
                // times running. DESIGN's own intervention trigger is "the same
                // failure twice. Stop. Do not ask for a third correction" — so a
                // third barren turn is a loop, and a fresh round is the one
                // thing that breaks it: the per-round delegation ceiling resets,
                // and the layer sees state it wrote rather than state it
                // remembers writing.
                //
                // **The ledger changing ends it immediately**, before the
                // looping check and regardless of how productive the turn was.
                // The layer wrote a decision or retired something, and the whole
                // point of writing is that the next request sees it. Converse
                // further and it decides several more things without ever seeing
                // the state it is building — which is the failure that made L1
                // worth having.
                //
                // It is never "the model stopped talking": that is `end_turn`,
                // which ends the run, not the round.
                if ledger_changed {
                    tracing::debug!(round, "round ends: the ledger changed");
                    break;
                }
                barren_turns = if executed == 0 { barren_turns + 1 } else { 0 };
                if barren_turns >= MAX_BARREN_TURNS_PER_ROUND {
                    tracing::debug!(round, barren_turns, "round ends: the layer is looping");
                    break;
                }
            }
        }

        // Running out of rounds is an ending, not a failure. The layer worked
        // every round; what it stopped doing is moving. Returning `Err` here
        // meant the caller got no final text, no composition and no acceptance
        // number — and this is one of the two ways a long task ends, so the
        // headline metric was missing from exactly the runs it exists to judge.
        //
        // `stopped_early` is a description rather than a flag because the reason
        // is what makes the outcome usable, and it mirrors
        // `DelegateOutcome::stopped_early` so a caller reads both layers the same
        // way.
        Ok(DecisionOutcome {
            rounds: self.config.max_rounds,
            delegations,
            final_text: last_text,
            usage,
            composition: contexts.last().copied().unwrap_or_default(),
            contexts,
            ledger: self.ledger.clone(),
            stopped_early: Some(format!(
                "the round ceiling of {} was reached: the layer stopped moving, it did \
                 not stop working",
                self.config.max_rounds
            )),
            outcomes: all_outcomes,
        })
    }

    fn build_view(
        &self,
        report: Option<&str>,
        outcomes: &[DelegateOutcome],
        latest: &[DelegateOutcome],
    ) -> View {
        let (shape, omitted) = carry_for_this_round(outcomes, latest);
        render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &self.charter,
            ledger: &self.ledger,
            tools: &delegate::decision_surface(&self.outcome_sets),
            report,
            shape: &shape,
            omitted: &omitted,
        })
    }
}

/// Everything a round's view carries from the delegations so far.
///
/// **A round's view is a function of the latest round, not of the run.** DESIGN
/// states the rule as "渲染出来的,不是累积出来的 → 大小有界、前缀字节稳定、永不退化",
/// and the version before this one broke it: every delegation's evidence was
/// carried, every round, so round *N* re-rendered round 1's tool output for the
/// *N*th time and the view grew with the square of the round count.
///
/// **And evidence is no longer carried at all.** DESIGN puts the context at
/// "L0 + L1 + 本轮报告 + 落位 + 形状指标 + 未装载清单" — raw tool output is not on
/// that list, and the earlier answer to "the executor misreports" was to put it
/// in front of the decision layer and hope it noticed, which would pull exactly
/// the material this architecture exists to keep out back into the scarcest
/// context in the system. The landing is the assertion and the tool results are
/// the proof; `crate::contradiction` checks one against the other, so what
/// reaches this layer is the conclusion and any contradiction found.
///
/// Nothing is dropped. Every piece of evidence is **addressed** in the manifest,
/// which is the one structure whose job is to say what is not in front of you
/// and how to reach it — and `spot_check` is the door that follows an address
/// when the layer decides the bytes are worth their cost.
///
/// **`latest` is a delegation, not an agent.** A fan-out is one call that
/// derived several agents, so the shape signals and the omissions of *all* of
/// them are this round's — taking only the last one's would drop the other three
/// children's records out of the manifest silently, which is the one thing a
/// manifest may not do.
fn carry_for_this_round(
    outcomes: &[DelegateOutcome],
    latest: &[DelegateOutcome],
) -> (Vec<ShapeSignal>, Vec<NotLoadedEntry>) {
    let mut shape = Vec::new();
    let mut omitted = Vec::new();
    for outcome in latest {
        shape.extend(outcome.shape.clone());
        omitted.extend(outcome.omitted.clone());
    }

    for (index, outcome) in outcomes.iter().enumerate() {
        for e in &outcome.evidence {
            let (address, how_to_load) = match &e.address {
                Some(address) => (address.clone(), "spot_check it, or delegate a reader".to_owned()),
                // Saying "not addressable" is the honest entry. The manifest's
                // contract is that an omitted thing is either reachable or
                // explained, and inventing a plausible-looking address for
                // something with nowhere to point would break it silently.
                None => (
                    "not addressable".to_owned(),
                    "it cannot be loaded: this block has no record behind it".to_owned(),
                ),
            };
            let verdict = match e.exit_code {
                Some(0) => "exit 0".to_owned(),
                Some(code) => format!("exit {code}"),
                None => "status not recovered".to_owned(),
            };
            omitted.push(NotLoadedEntry {
                what: format!(
                    "evidence from delegation {}: `{}` ({verdict})",
                    index + 1,
                    e.label
                ),
                address,
                bytes: e.content.len() as u64,
                why: "the decision layer does not read raw evidence; the harness checks \
                      landings against it instead"
                    .to_owned(),
                how_to_load,
            });
        }
    }
    (shape, omitted)
}

#[cfg(test)]
mod round_material_tests {
    use super::*;
    use crate::render::{Evidence, NotLoadedEntry};

    fn outcome_with(index: usize, evidence_bytes: usize) -> DelegateOutcome {
        let mut outcome = crate::delegate::tests_support::outcome_for(&format!("task {index}"));
        outcome.evidence = vec![
            Evidence::command(format!("cmd {index}"), "x".repeat(evidence_bytes), 0)
                .addressed_to(format!("blob:digest{index}")),
        ];
        outcome.omitted = vec![NotLoadedEntry {
            what: format!("`read_file` result {index}"),
            address: format!("journal:seq={index}"),
            bytes: 10,
            why: "raw tool output".to_owned(),
            how_to_load: "delegate a reader for it".to_owned(),
        }];
        outcome
    }

    /// **M3: "决策层的视野里不出现原始证据."**
    ///
    /// The function no longer has a way to return evidence, so the strongest
    /// form of this guard is that the manifest accounts for all of it. A view
    /// that carried a tool's output would be a view whose raw-material ratio is
    /// not zero, and the count below is what zero means.
    #[test]
    fn no_evidence_reaches_the_view_and_all_of_it_is_addressed() {
        let outcomes: Vec<_> = (0..10).map(|i| outcome_with(i, 500)).collect();
        let (shape, omitted) = carry_for_this_round(&outcomes, &outcomes);
        let _ = shape;

        for index in 0..10 {
            let needle = format!("cmd {index}");
            let entry = omitted
                .iter()
                .find(|e| e.what.contains(&needle))
                .unwrap_or_else(|| {
                    panic!(
                        "`{needle}` was not carried and is not in the manifest either, so it \
                         was dropped silently — which is the one thing a manifest may not do: \
                         {:#?}",
                        omitted.iter().map(|e| &e.what).collect::<Vec<_>>()
                    )
                });
            assert_eq!(
                entry.address,
                format!("blob:digest{index}"),
                "an omitted thing has to be reachable, not merely mentioned"
            );
            assert_eq!(entry.bytes, 500);
        }
    }

    /// The other thing a round's view must not do: grow with the run.
    ///
    /// The manifest grows — one line per delegation — and that is the trade
    /// DESIGN names: addresses rather than payloads. Ten delegations of the same
    /// size must produce ten address lines and not ten payloads.
    #[test]
    fn the_manifest_grows_by_addresses_not_by_payloads() {
        let one = carry_for_this_round(&[outcome_with(0, 100_000)], &[outcome_with(0, 100_000)]);
        let ten: Vec<_> = (0..10).map(|i| outcome_with(i, 100_000)).collect();
        let many = carry_for_this_round(&ten, &ten);

        let lines = |(_, omitted): &(Vec<ShapeSignal>, Vec<NotLoadedEntry>)| omitted.len();
        assert!(lines(&one) < lines(&many), "each delegation is accounted for once");
        // 100 000 bytes of output per delegation, described in well under a
        // kilobyte each — the ratio DESIGN's "大小有界" is about.
        let described: usize = many.1.iter().map(|e| e.what.len() + e.address.len()).sum();
        assert!(
            described < 10_000,
            "a megabyte of payload was described in {described} bytes of address — if this \
             grew with the payload, the manifest would be a second copy of the record"
        );
    }

    #[test]
    fn evidence_with_nowhere_to_point_says_so_rather_than_inventing_an_address() {
        let mut old = outcome_with(0, 10);
        old.evidence[0].address = None;
        let (_, omitted) = carry_for_this_round(&[old.clone()], &[old]);
        let entry = omitted
            .iter()
            .find(|e| e.what.contains("cmd 0"))
            .expect("the unaddressable block is still named");
        assert_eq!(entry.address, "not addressable");
        assert!(
            entry.how_to_load.contains("cannot be loaded"),
            "and the reader is told it cannot be fetched: {}",
            entry.how_to_load
        );
    }

    #[test]
    fn a_delegations_evidence_carries_its_verdict_into_the_manifest() {
        // An address the layer can follow is most of the entry; what the command
        // did is the rest, and it is what decides whether the bytes are worth
        // loading at all.
        let mut failing = outcome_with(0, 10);
        failing.evidence[0].exit_code = Some(3);
        let (_, omitted) = carry_for_this_round(&[failing.clone()], &[failing]);
        assert!(
            omitted.iter().any(|e| e.what.contains("exit 3")),
            "a failing command has to be visible as one in the manifest: {:#?}",
            omitted.iter().map(|e| &e.what).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_run_with_no_delegations_carries_nothing() {
        let (shape, omitted) = carry_for_this_round(&[], &[]);
        assert!(shape.is_empty() && omitted.is_empty());
    }
}

/// The set id a delegation named, read however it arrived unambiguously.
///
/// The schema says `string`. Measured: a real run sent `["investigation"]` —
/// the right set, the wrong encoding — and was refused with a message that said
/// to pass one without saying in what shape. The layer then spent the rest of
/// the run reasoning about the interface instead of the task, and finished
/// blocked:
///
/// > The array form `["investigation"]` — the form the tool schema specifies —
/// > was refused with the same "named no outcome set" message, so the encoding
/// > of the value is the one variable not yet tried … the run is handed back as
/// > blocked.
///
/// A one-element list names exactly what a bare string names, so it is read as
/// one. Anything else is not a name, and the refusal says so — strictness about
/// *which* set is the point; strictness about JSON punctuation is a way to lose
/// a run to a comma.
fn set_id_in(input: &serde_json::Value) -> Option<&str> {
    match input.get("outcomes") {
        Some(serde_json::Value::String(id)) => Some(id.as_str()),
        Some(serde_json::Value::Array(items)) if items.len() == 1 => items[0].as_str(),
        _ => None,
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
        let tool = delegate::delegate_tool(&Default::default());
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
            shape: &[],
            omitted: &[],
        });
        let many = render(RenderInput {
            prompt: DECISION_PROMPT,
            charter: &charter,
            ledger: &ledger,
            tools: &[],
            report: Some("tenth"),
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

#[cfg(test)]
mod landing_distribution_tests {
    use super::*;
    use crate::landing::Landing;

    fn outcome_with(landings: Vec<Option<(&str, &str)>>) -> DecisionOutcome {
        DecisionOutcome {
            rounds: 1,
            delegations: landings.len() as u32,
            final_text: String::new(),
            usage: TokenUsage::default(),
            composition: ContextComposition::default(),
            contexts: Vec::new(),
            ledger: Ledger::default(),
            stopped_early: None,
            outcomes: landings
                .into_iter()
                .map(|landing| {
                    let mut outcome =
                        crate::delegate::tests_support::outcome_for("a task");
                    outcome.landing = landing.map(|(set, slot)| Landing {
                        set: set.to_owned(),
                        slot: slot.to_owned(),
                        why: String::new(),
                        unlisted: None,
                    });
                    outcome
                })
                .collect(),
        }
    }

    #[test]
    fn the_tally_counts_each_slot_the_run_landed_in() {
        let outcome = outcome_with(vec![
            Some(("build", "pass")),
            Some(("build", "pass")),
            Some(("build", "compile_error")),
        ]);
        let tally = outcome.landing_distribution();
        assert_eq!(tally["build"]["pass"], 2);
        assert_eq!(tally["build"]["compile_error"], 1);
        assert!(outcome.landing_problems().is_empty(), "nothing is wrong here");
    }

    /// **DESIGN: "`other` 常见,这组就是错的."**
    #[test]
    fn a_set_that_mostly_lands_in_other_is_reported_as_the_wrong_set() {
        let outcome = outcome_with(vec![
            Some(("build", "pass")),
            Some(("build", crate::landing::OTHER)),
            Some(("build", crate::landing::OTHER)),
        ]);
        let problems = outcome.landing_problems();
        assert_eq!(problems.len(), 1, "one finding, not none and not three: {problems:?}");
        assert!(
            problems[0].contains("build") && problems[0].contains("other"),
            "the finding has to name the set and the reason: {}",
            problems[0]
        );

        // And a set that fits is not reported — the check can fail, so a clean
        // run means something.
        let fine = outcome_with(vec![
            Some(("build", "pass")),
            Some(("build", crate::landing::OTHER)),
            Some(("build", "pass")),
            Some(("build", "pass")),
        ]);
        assert!(fine.landing_problems().is_empty());
    }

    #[test]
    fn a_delegation_that_was_never_asked_contributes_nothing_to_the_tally() {
        let outcome = outcome_with(vec![None, Some(("build", "pass"))]);
        let tally = outcome.landing_distribution();
        assert_eq!(tally["build"]["pass"], 1);
        assert_eq!(
            tally.values().map(|c| c.values().sum::<u32>()).sum::<u32>(),
            1,
            "an unasked delegation must not be counted as landing anywhere — \
             `we did not ask` is not a slot"
        );
    }
}
