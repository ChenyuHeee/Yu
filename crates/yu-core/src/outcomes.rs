//! Pre-registered outcome sets: the shape of a delegation's report is fixed
//! **before** the call is made.
//!
//! DESIGN, 「预注册落位:报告的形状在调用之前就定了」:
//!
//! > **机制**:调用时先**预注册这个调用可能的落点**(一个有限集合),执行者把实际结果**归类**到其中一格。报告从散文收缩成一次**选择**。
//! >
//! > **预测集是策略,不是每次现编。** 决策层按定义不懂这个领域,列不出"编译失败 / 测试失败 / 超时"。它只**选**一个集合,**不发明**(约束 0)。
//! >
//! > **`other` 必须存在,落进去要响。**
//! >
//! > **自洽检查由 harness 做,不由决策层做。**
//!
//! This module holds the *vocabulary*: the types a landing is expressed in.
//! Reading sets from a policy file is `yu-policy`'s job (and so is enforcing
//! the rules a policy must satisfy); the types sit in `core` because the
//! consistency check lives here and has to be able to name what it checks
//! against.
//!
//! Why the claims are a closed enum while the sets are policy data: the harness
//! checks a landing against the record, and only a claim the harness can
//! *compute* is checkable. A policy composes checkable claims; it cannot invent
//! a new way of being checked. So a new [`Expects`] is a Rust change and a new
//! *set* is not — which is exactly the line DESIGN's constraint 0 draws.
//!
//! The record a claim is read against is the one `delegate::outcome_from_run`
//! builds: the commands that ran, each with an exit status when one was
//! captured, and whether the run completed. Every variant of [`Expects`] is a
//! statement about those facts, which is what keeps the check deterministic and
//! free.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One pre-registered set: an id the decision layer selects by, and the slots a
/// result may land in.
///
/// A set is a *menu*, not a classifier. The executor lands in the slot whose
/// claim its result satisfies, and the harness checks that claim against the
/// record; nothing asks a model whether the landing was right. Slot names are
/// the policy author's — this domain's words, which the decision layer could
/// not have supplied — while [`Slot::expects`] is drawn from the fixed
/// vocabulary the harness can compute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeSet {
    /// What the decision layer asks for at call time. Unique across a policy,
    /// because a set is *selected* by id: two sets with one id would make the
    /// selection ambiguous, and the loader rejects the duplicate rather than
    /// letting the last one win.
    pub id: String,
    /// What the set covers, for the one making the choice. The decision layer
    /// is not expected to know the domain; a description is how a set explains
    /// itself without the layer having to invent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The landing places, in the order the policy declares them.
    ///
    /// Never contains [`OutcomeSets::OTHER`]: `other` is always available and
    /// is appended by whoever renders the set, so declaring it is a policy
    /// error rather than a slot.
    pub slots: Vec<Slot>,
}

/// One landing place: the name the executor reports, and the claim the harness
/// checks it against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    /// What the landing is called in the report — the whole of what the
    /// decision layer sees of the classification.
    pub name: String,
    /// What that name asserts about the record. See [`Expects`]: each claim is
    /// a statement the harness confirms or refutes from the tool results, which
    /// is what makes a landing an assertion instead of prose.
    pub expects: Expects,
}

/// What a slot asserts about the delegation's record.
///
/// Read every variant as a tri-state: the record *confirms* the claim,
/// *refutes* it, or cannot decide (a fact the claim needs was not captured —
/// an exit status the reduction dropped, say). Only the refuted state is the
/// misreport DESIGN wants as an event; the undecided state is reported as a
/// landing that was not cross-checked, never as one that was.
///
/// The claims deliberately do not multiply: several slots of one set may claim
/// `failed` (`compile_error` and `test_failure` are the canonical pair), and
/// the harness does not disambiguate them. Its job is consistency — a landing
/// must not say something the record contradicts — not classification, which is
/// the executor's, made where the domain knowledge is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expects {
    /// The work passed: the run completed, at least one command ran, and every
    /// command that reported an exit status reported success.
    ///
    /// Refuted by a non-zero status, by a run that did not complete, and by a
    /// run that ran nothing. DESIGN's fourth intervention trigger — "自称成功但
    /// 无可运行证据" — is that last case, and it is why "at least one command"
    /// is part of the claim rather than implied by it: success with no runnable
    /// evidence must be refutable without anyone reading the report.
    Succeeded,

    /// Something failed: at least one command reported a non-zero exit status.
    ///
    /// Completion is not part of this claim. A run that hit a failing command
    /// and was then cut off still has evidence showing a failure, and forcing
    /// it to instead claim `stopped_early` would hide the failure the decision
    /// layer needs to act on.
    Failed,

    /// Nothing needed to run: the run completed without running a command.
    ///
    /// Completion *is* part of this claim. A run that stopped before it ran
    /// anything has not shown that nothing needed to run — absorbing it here
    /// would turn an unexpected stop into a clean-looking "nothing to do", the
    /// squeeze-into-the-nearest-bucket failure the escape hatch exists to
    /// prevent. It belongs in `stopped_early`, or in `other`, where the
    /// decision layer will see it.
    NothingRun,

    /// The run did not finish: it was stopped before it reported completion.
    ///
    /// This is the claim a cut-off executor cannot make about itself — it is
    /// the harness that lands a stopped run here, from `stopped_early`, when
    /// the set declared this claim. A set that predicts timeouts needs this
    /// slot, or every timeout shows up as `other` and a set that was right
    /// reads as a set that was wrong (DESIGN: "`other` 常见,这组就是错的").
    StoppedEarly,

    /// No claim about the record.
    ///
    /// Some landings are statements about the report rather than about commands
    /// (`needs_human`, `blocked_on_a_question`), and a slot that cannot be
    /// contradicted by the record must say so rather than borrow a claim it
    /// does not mean. Landing here is counted like any other landing and
    /// cross-checked like none — the honest form of "the harness cannot check
    /// this", rather than a check that quietly passes everything.
    Unchecked,
}

impl Expects {
    /// Every claim, in the order diagnostics list them.
    ///
    /// A list rather than an external derive so the vocabulary stays visible in
    /// one place. [`Expects::name`] matches exhaustively without a wildcard arm
    /// — adding a variant will not compile until it is named — and the tests
    /// below fail until every list here agrees, so a new claim cannot join
    /// without someone deciding what it is called and what it asserts.
    pub const ALL: [Expects; 5] = [
        Expects::Succeeded,
        Expects::Failed,
        Expects::NothingRun,
        Expects::StoppedEarly,
        Expects::Unchecked,
    ];

    /// Every name a slot may declare, in the order errors list them.
    pub const NAMES: [&str; 5] = [
        Expects::Succeeded.name(),
        Expects::Failed.name(),
        Expects::NothingRun.name(),
        Expects::StoppedEarly.name(),
        Expects::Unchecked.name(),
    ];

    /// The name this claim is declared and reported under.
    ///
    /// The match is exhaustive and has no wildcard: a new variant stops the
    /// build here, at the one place where its name — the string a policy
    /// writes and a report carries — has to be decided.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::NothingRun => "nothing_run",
            Self::StoppedEarly => "stopped_early",
            Self::Unchecked => "unchecked",
        }
    }

    /// The claim a slot declares with this name, or `None`.
    ///
    /// `None` and not a default. A misspelled `expects` must be an error where
    /// the policy is loaded, never a slot that silently asserts something no
    /// one chose — the silent-omission failure the whole project is arranged
    /// against, arriving inside the mechanism that exists to catch misreports.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|claim| claim.name() == name)
    }
}

/// Written out rather than derived so there is exactly one mapping between a
/// claim and its name: the string a policy declares, the string an error lists
/// and the string a report carries all come from [`Expects::name`]. A
/// `#[serde(rename_all = ...)]` would be a second list, and two lists drift.
impl Serialize for Expects {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Expects {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name).ok_or_else(|| D::Error::unknown_variant(&name, &Self::NAMES))
    }
}

/// The sets a policy declares, in the order it declares them.
///
/// Declaration order rather than a keyed map, so a policy file reads top to
/// bottom as it will be shown, and so that uniqueness is a rule the loader
/// checks — and can name — rather than a format error reported with no idea
/// what was duplicated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutcomeSets {
    sets: Vec<OutcomeSet>,
}

impl OutcomeSets {
    /// The escape hatch. DESIGN: "**`other` 必须存在,落进去要响。**"
    ///
    /// It is never in [`OutcomeSet::slots`]; whoever renders a set appends it.
    /// A declared slot with this name would collide with the one place an
    /// unpredicted result can land, and losing that silently is the failure the
    /// reservation exists to prevent — so the loader rejects the declaration
    /// instead.
    pub const OTHER: &'static str = "other";

    /// A policy's sets, as loaded. Public so that a test — the harness check's
    /// tests above all — can build a set without a policy file in sight.
    ///
    /// The uniqueness and reservation rules are the *loader's* (`yu-policy`);
    /// this constructs the vocabulary, so it takes the sets as given.
    pub fn from_sets(sets: Vec<OutcomeSet>) -> Self {
        Self { sets }
    }

    /// The set under `id`, or `None`.
    ///
    /// `None` is what a caller gets for an id no policy declared, and for
    /// [`OutcomeSets::OTHER`] when it is asked for as a *set* — `other` is a
    /// landing inside a set, never a set.
    pub fn get(&self, id: &str) -> Option<&OutcomeSet> {
        self.sets.iter().find(|set| set.id == id)
    }

    /// Every declared id, in declaration order.
    pub fn ids(&self) -> Vec<&str> {
        self.sets.iter().map(|set| set.id.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(id: &str, slots: &[(&str, Expects)]) -> OutcomeSet {
        OutcomeSet {
            id: id.to_owned(),
            description: None,
            slots: slots
                .iter()
                .map(|(name, expects)| Slot {
                    name: (*name).to_owned(),
                    expects: *expects,
                })
                .collect(),
        }
    }

    /// The reason the vocabulary is in `core`: a set can be built and checked
    /// with no policy file — and no TOML — anywhere near it.
    #[test]
    fn a_core_test_can_build_sets_without_a_policy_file() {
        let sets = OutcomeSets::from_sets(vec![
            set(
                "build_result",
                &[("pass", Expects::Succeeded), ("compile_error", Expects::Failed)],
            ),
            set("review", &[("approved", Expects::Unchecked)]),
        ]);

        let build = sets.get("build_result").expect("declared above");
        assert_eq!(build.slots.len(), 2);
        assert_eq!(build.slots[0].name, "pass");
        assert_eq!(build.slots[0].expects, Expects::Succeeded);
        assert_eq!(sets.ids(), vec!["build_result", "review"], "declaration order");
        assert_eq!(sets.get("nothing_declared"), None);
        assert_eq!(OutcomeSets::OTHER, "other", "the escape hatch's name is an interface");
    }

    /// The name mapping, in both directions and across the wire: the string a
    /// policy declares is the string a report carries is the string an error
    /// lists.
    #[test]
    fn a_claim_crosses_the_wire_as_one_name_in_both_directions() {
        for claim in Expects::ALL {
            let text = serde_json::to_string(&claim).expect("a claim serializes");
            assert_eq!(text, format!("\"{}\"", claim.name()));
            let back: Expects = serde_json::from_str(&text).expect("and deserializes");
            assert_eq!(back, claim);
        }

        // An unknown name is refused rather than defaulted to the claim that
        // checks nothing.
        assert_eq!(Expects::from_name("maybe"), None);
        let refused = serde_json::from_str::<Expects>("\"maybe\"").expect_err("must not default");
        assert!(refused.to_string().contains("maybe"), "{refused}");
    }

    /// The tripwire for the vocabulary's growth.
    ///
    /// `name`'s match already fails to compile on a new variant; this is the
    /// second gate, and it is written as a match over a *value* with no
    /// wildcard arm on purpose. Adding a claim cannot be silent: someone has to
    /// come here, give it a name, and add it to `ALL` and `NAMES` — and a
    /// catch-all arm, on this test or in any consumer, would be the way to make
    /// it silent.
    #[test]
    fn every_claim_is_named_and_mapped_exactly_once() {
        assert_eq!(
            Expects::ALL.len(),
            Expects::NAMES.len(),
            "a claim was added to one list and not the other"
        );

        let claim = Expects::Unchecked;
        match claim {
            Expects::Succeeded
            | Expects::Failed
            | Expects::NothingRun
            | Expects::StoppedEarly
            | Expects::Unchecked => {}
        }

        for claim in Expects::ALL {
            assert_eq!(
                Expects::from_name(claim.name()),
                Some(claim),
                "`{}` does not parse back to its own claim",
                claim.name()
            );
            assert!(
                Expects::NAMES.contains(&claim.name()),
                "`{}` is a claim but not a name",
                claim.name()
            );
        }
    }
}
