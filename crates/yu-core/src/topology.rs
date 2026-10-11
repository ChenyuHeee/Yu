//! Topology: a declarative strategy, not a hard-coded shape.
//!
//! DESIGN: "**层级不是固定的两层。** 每一个 agent 都可以再派生 agent,拓扑是
//! **可声明的策略**,不是硬编码结构."
//!
//! And the enabling condition, which is the part that actually decides whether any
//! of this is real:
//!
//! > **使能条件就是你说的那件事:派生足够便宜,拓扑才能由 agent 在运行时自己选。**
//! > 派生贵,拓扑就只能固定 —— Claude Agent SDK 的固定下界正是这么把拓扑钉死的。
//!
//! So this module is deliberately thin. It holds the shapes as data, decides
//! which one a task calls for, and derives through `delegate` — the same
//! instrument the decision layer uses. Nothing here would be worth its weight if
//! derivation were expensive, which is why the cost of a derivation is measured
//! rather than assumed (`SpanKind::Spawn`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::LoopError;
use crate::delegate::{self, DelegateArgs, DelegateOutcome};

/// The shapes DESIGN names, plus the degenerate one.
///
/// `Single` is not in DESIGN's table and is here because most tasks are it. A
/// topology interpreter that cannot say "no children" forces every caller to
/// special-case the common answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyKind {
    /// No children. Do the work here.
    Single,
    /// Sequential handoff, each step narrowing. For steps that are clear and want
    /// verifying one at a time.
    Chain,
    /// One parent, N parallel children, parent reduces. For reading many places
    /// that do not depend on each other.
    FanOut,
    /// Recursive decomposition, for when the task's own boundary is unclear.
    Tree,
    /// Author → critic → author.
    Reflection,
    /// Peers coordinating through a message board, for long runs where each needs
    /// to know what the others are doing.
    Peer,
    /// Each work item verified exactly once, by someone who did not write it.
    VerificationPartition,
}

impl TopologyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TopologyKind::Single => "single",
            TopologyKind::Chain => "chain",
            TopologyKind::FanOut => "fan_out",
            TopologyKind::Tree => "tree",
            TopologyKind::Reflection => "reflection",
            TopologyKind::Peer => "peer",
            TopologyKind::VerificationPartition => "verification_partition",
        }
    }

    pub fn from_str(name: &str) -> Option<Self> {
        Some(match name {
            "single" => TopologyKind::Single,
            "chain" => TopologyKind::Chain,
            "fan_out" | "fanout" => TopologyKind::FanOut,
            "tree" => TopologyKind::Tree,
            "reflection" => TopologyKind::Reflection,
            "peer" => TopologyKind::Peer,
            "verification_partition" => TopologyKind::VerificationPartition,
            _ => return None,
        })
    }
}

/// A topology as data, which is what makes it a policy rather than a structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySpec {
    pub kind: TopologyKind,
    /// The role derived agents take, when the topology derives any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_role: Option<String>,
    /// Kind-specific knobs, free-form so a new topology does not have to touch
    /// this file — the same reason `TopologyRule` keeps them free-form.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl Default for TopologySpec {
    fn default() -> Self {
        Self {
            kind: TopologyKind::Single,
            child_role: None,
            params: BTreeMap::new(),
        }
    }
}

impl TopologySpec {
    pub fn new(kind: TopologyKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    pub fn param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }

    pub fn with_child_role(mut self, role: impl Into<String>) -> Self {
        self.child_role = Some(role.into());
        self
    }

    fn fan_out_width(&self) -> usize {
        self.params
            .get("width")
            .and_then(|w| w.parse().ok())
            .unwrap_or(3)
    }

    /// What this topology says to do with a task.
    ///
    /// Pure: no client, no journal, no waiting. The decision of *what shape* the
    /// work takes is separated from doing it, so the shape can be tested,
    /// logged, and changed without a run.
    pub fn plan(&self, task: &str, items: &[String]) -> Derivation {
        match self.kind {
            TopologyKind::Single => Derivation::Direct,
            TopologyKind::Chain => Derivation::Chain {
                // The first step is the task itself; later steps are taken from
                // `items`, which is how a chain is actually run — each step
                // narrowing on what the previous one returned.
                tasks: std::iter::once(task.to_owned())
                    .chain(items.iter().cloned())
                    .collect(),
            },
            TopologyKind::FanOut => {
                if items.is_empty() {
                    // Nothing to fan out over. Inventing children would be
                    // decomposition by guessing.
                    return Derivation::Direct;
                }
                Derivation::FanOut {
                    tasks: items.iter().take(self.fan_out_width().max(1)).cloned().collect(),
                }
            }
            TopologyKind::Tree => {
                if items.is_empty() {
                    return Derivation::Direct;
                }
                Derivation::FanOut {
                    tasks: items.to_vec(),
                }
            }
            TopologyKind::Reflection => Derivation::Reflect {
                author: task.to_owned(),
                critic: self
                    .params
                    .get("critique")
                    .cloned()
                    .unwrap_or_else(|| format!("Critique this work against its stated goal: {task}")),
            },
            TopologyKind::Peer => Derivation::Direct,
            TopologyKind::VerificationPartition => {
                if items.is_empty() {
                    return Derivation::Direct;
                }
                Derivation::Verify {
                    work_items: items.to_vec(),
                }
            }
        }
    }

    /// Whether this topology derives children at all.
    pub fn derives(&self) -> bool {
        !matches!(self.kind, TopologyKind::Single | TopologyKind::Peer)
    }
}

/// The shape a topology chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
pub enum Derivation {
    /// Work here, no children.
    Direct,
    /// Sequential: each task runs after the previous.
    Chain { tasks: Vec<String> },
    /// Parallel: the parent reduces what comes back.
    FanOut { tasks: Vec<String> },
    /// Author, then critic, then author again.
    Reflect { author: String, critic: String },
    /// One verifier per work item, each a non-author.
    Verify { work_items: Vec<String> },
}

impl Derivation {
    /// How many children this shape creates.
    pub fn child_count(&self) -> usize {
        match self {
            Derivation::Direct => 0,
            Derivation::Chain { tasks } => tasks.len(),
            Derivation::FanOut { tasks } => tasks.len(),
            Derivation::Reflect { .. } => 2,
            Derivation::Verify { work_items } => work_items.len(),
        }
    }
}

/// Runs one delegation. Injected so the derivation logic is testable without a
/// client and without spending anything.
pub trait DelegateRunner {
    fn run(
        &self,
        args: DelegateArgs,
    ) -> impl std::future::Future<Output = Result<DelegateOutcome, LoopError>> + Send;
}

/// Carry out a derivation, returning what each child reported.
///
/// Children are derived through `delegate` — the same instrument the decision
/// layer uses — so a child is fresh, gets only its own task, and reports back a
/// reduced view rather than a transcript. That is what keeps a fan-out from
/// costing N transcripts in the parent's context.
pub async fn derive<R: DelegateRunner>(
    runner: &R,
    derivation: &Derivation,
) -> Result<Vec<DelegateOutcome>, LoopError> {
    let mut outcomes = Vec::new();
    match derivation {
        Derivation::Direct => {}
        Derivation::FanOut { tasks } => {
            // Sequential rather than concurrent. Deriving in parallel needs a
            // rate-limit story and a shared-budget story that this layer does
            // not have yet, and DESIGN's own warning applies: a mechanism should
            // be a function of pressure, not of ambition. The shape is still
            // declared as a fan-out because that is what the parent will reduce.
            for task in tasks {
                let mut args = DelegateArgs::new(task.clone());
                args.expected_files = None;
                outcomes.push(runner.run(args).await?);
            }
        }
        Derivation::Chain { tasks } => {
            for task in tasks {
                outcomes.push(runner.run(DelegateArgs::new(task.clone())).await?);
            }
        }
        Derivation::Reflect { author, critic } => {
            outcomes.push(runner.run(DelegateArgs::new(author.clone())).await?);
            outcomes.push(runner.run(DelegateArgs::new(critic.clone())).await?);
        }
        Derivation::Verify { work_items } => {
            for item in work_items {
                // DESIGN: "验证者从原始记录出发,不从上一级报告出发" and "每个工作项
                // 恰好被一个非作者验证一次". The verifier is therefore a fresh
                // reader, not the author asked to check itself.
                let args = DelegateArgs {
                    outcomes: None,
                    task: format!(
                        "Verify this work item against the record. You did not write it. \
                         Report only what the record actually shows: {item}"
                    ),
                    question: None,
                    expected_files: None,
                };
                outcomes.push(runner.run(args).await?);
            }
        }
    }
    Ok(outcomes)
}

/// The real runner: derives through the executor.
pub struct ExecutorRunner {
    pub client: std::sync::Arc<yu_wire::client::WireClient>,
    pub journal: std::sync::Arc<yu_journal::Journal>,
    pub tools: std::sync::Arc<yu_tools::ToolRegistry>,
    pub timeline: std::sync::Arc<yu_observe::Timeline>,
    pub cwd: std::path::PathBuf,
}

impl DelegateRunner for ExecutorRunner {
    async fn run(&self, args: DelegateArgs) -> Result<DelegateOutcome, LoopError> {
        delegate::run_delegation(
            &args,
            self.client.clone(),
            self.journal.clone(),
            self.tools.clone(),
            self.timeline.clone(),
            self.cwd.clone(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("item {i}")).collect()
    }

    #[test]
    fn single_derives_nothing() {
        let spec = TopologySpec::new(TopologyKind::Single);
        assert_eq!(spec.plan("do it", &items(3)), Derivation::Direct);
        assert!(!spec.derives());
        assert_eq!(spec.plan("do it", &items(3)).child_count(), 0);
    }

    #[test]
    fn fan_out_derives_one_child_per_item_and_the_parent_reduces() {
        let spec = TopologySpec::new(TopologyKind::FanOut);
        let plan = spec.plan("read everything", &items(3));
        let Derivation::FanOut { tasks } = plan else {
            panic!("expected a fan-out, got {plan:?}");
        };
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0], "item 0");
    }

    #[test]
    fn fan_out_respects_its_declared_width() {
        // The width is policy, not a constant in Rust — constraint 0.
        let spec = TopologySpec::new(TopologyKind::FanOut).param("width", "2");
        assert_eq!(spec.plan("t", &items(9)).child_count(), 2);
        let wider = TopologySpec::new(TopologyKind::FanOut).param("width", "5");
        assert_eq!(wider.plan("t", &items(9)).child_count(), 5);
    }

    #[test]
    fn fanning_out_over_nothing_is_direct_rather_than_inventing_children() {
        // Decomposing a task with no items would be decomposition by guessing —
        // the same failure as summarising instead of completing.
        let spec = TopologySpec::new(TopologyKind::FanOut);
        assert_eq!(spec.plan("t", &[]), Derivation::Direct);
        assert_eq!(spec.plan("t", &[]).child_count(), 0);
    }

    #[test]
    fn a_chain_runs_the_task_then_each_narrowing_step() {
        let spec = TopologySpec::new(TopologyKind::Chain);
        let Derivation::Chain { tasks } = spec.plan("start here", &items(2)) else {
            panic!("expected a chain");
        };
        assert_eq!(tasks, vec!["start here", "item 0", "item 1"]);
    }

    #[test]
    fn reflection_pairs_an_author_with_a_critic_of_the_stated_goal() {
        let spec = TopologySpec::new(TopologyKind::Reflection);
        let Derivation::Reflect { author, critic } = spec.plan("write the parser", &[]) else {
            panic!("expected a reflection");
        };
        assert_eq!(author, "write the parser");
        assert!(
            critic.contains("write the parser"),
            "the critic must be given the goal, not just told to be critical: {critic}"
        );
    }

    #[test]
    fn verification_is_a_partition_not_a_sample() {
        // DESIGN: "分片的是工作,不是知识 … 每个工作项恰好被一个非作者验证一次".
        // Every item gets a verifier; none gets two.
        let spec = TopologySpec::new(TopologyKind::VerificationPartition);
        let Derivation::Verify { work_items } = spec.plan("t", &items(4)) else {
            panic!("expected a verification partition");
        };
        assert_eq!(work_items.len(), 4);
        assert_eq!(spec.plan("t", &items(4)).child_count(), 4);
    }

    #[test]
    fn every_kind_the_policy_can_name_is_understood() {
        // A policy names its topology with a string. A name this module does not
        // understand must not silently become `Single` — that would turn a typo
        // into a strategy change nobody chose.
        for kind in [
            TopologyKind::Single,
            TopologyKind::Chain,
            TopologyKind::FanOut,
            TopologyKind::Tree,
            TopologyKind::Reflection,
            TopologyKind::Peer,
            TopologyKind::VerificationPartition,
        ] {
            assert_eq!(TopologyKind::from_str(kind.as_str()), Some(kind));
        }
        assert_eq!(TopologyKind::from_str("fann_out"), None);
    }

    #[test]
    fn a_topology_spec_round_trips_through_json() {
        // It comes from a policy document and goes into a record, so both
        // directions have to work.
        let spec = TopologySpec::new(TopologyKind::FanOut)
            .param("width", "4")
            .param("note", "reads that do not depend on each other");
        let text = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<TopologySpec>(&text).unwrap(), spec);
    }

    #[test]
    fn deriving_and_peer_are_distinguished() {
        // A peer topology coordinates rather than deriving, so a caller that
        // checks `derives()` to decide whether to expect children gets it right.
        assert!(!TopologySpec::new(TopologyKind::Peer).derives());
        assert!(TopologySpec::new(TopologyKind::FanOut).derives());
        assert!(TopologySpec::new(TopologyKind::VerificationPartition).derives());
    }

    /// A declared topology reaches the layer that has to follow it.
    ///
    /// A spec nothing reads is a comment. This is the injection point: the shape
    /// is rendered into L0 every round, so the layer is *given* it rather than
    /// expected to remember it.
    #[test]
    fn a_declared_topology_is_rendered_into_the_view() {
        use crate::render::{Charter, Ledger, RenderInput, render};

        let charter = Charter {
            text: "read every module".into(),
            topology: Some(
                TopologySpec::new(TopologyKind::FanOut)
                    .param("width", "4")
                    .with_child_role("reader"),
            ),
            ..Charter::default()
        };
        let ledger = Ledger::default();
        let view = render(RenderInput {
            prompt: "p",
            charter: &charter,
            ledger: &ledger,
            tools: &[],
            report: None,
            shape: &[],
            omitted: &[],
        });
        let l0 = &view.system[1].text;
        assert!(l0.contains("fan_out"), "the declared shape must be stated: {l0}");
        assert!(l0.contains("width = 4"), "and its parameters: {l0}");
        assert!(l0.contains("reader"), "and the child role: {l0}");
    }

    #[test]
    fn a_default_spec_is_single_rather_than_some_shape_nobody_chose() {
        assert_eq!(TopologySpec::default().kind, TopologyKind::Single);
        assert!(!TopologySpec::default().derives());
    }

    // ---- execution, through an injected runner -----------------------------

    struct Counting(std::sync::Mutex<Vec<String>>);

    impl DelegateRunner for Counting {
        async fn run(&self, args: DelegateArgs) -> Result<DelegateOutcome, LoopError> {
            self.0.lock().unwrap().push(args.task.clone());
            Ok(crate::delegate::tests_support::outcome_for(&args.task))
        }
    }

    #[tokio::test]
    async fn a_fan_out_actually_derives_one_child_per_task() {
        let runner = Counting(std::sync::Mutex::new(Vec::new()));
        let spec = TopologySpec::new(TopologyKind::FanOut);
        let plan = spec.plan("t", &items(3));
        let outcomes = derive(&runner, &plan).await.unwrap();

        assert_eq!(outcomes.len(), 3);
        let seen = runner.0.lock().unwrap();
        assert_eq!(seen.len(), 3, "each child got exactly one task: {seen:?}");
        assert_eq!(seen[0], "item 0");
    }

    #[tokio::test]
    async fn verifying_never_asks_the_author_to_check_its_own_work() {
        let runner = Counting(std::sync::Mutex::new(Vec::new()));
        let plan = Derivation::Verify {
            work_items: vec!["the parser".into()],
        };
        derive(&runner, &plan).await.unwrap();
        let seen = runner.0.lock().unwrap();
        assert!(
            seen[0].contains("You did not write it"),
            "a verifier that inherits the author's framing verifies the framing: {}",
            seen[0]
        );
    }

    #[tokio::test]
    async fn a_direct_derivation_derives_nothing() {
        let runner = Counting(std::sync::Mutex::new(Vec::new()));
        assert!(derive(&runner, &Derivation::Direct).await.unwrap().is_empty());
        assert!(runner.0.lock().unwrap().is_empty(), "no child was derived");
    }
}
