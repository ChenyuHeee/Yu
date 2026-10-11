//! The harness's check on a landing, because nobody else can make it.
//!
//! DESIGN: *"**落位是断言,工具结果是证明** —— 而证明由 **harness** 核对:确定性、免费、
//! 不依赖决策层注意到。矛盾发生时作为**事件**上报(执行者误报本身是决策相关的事实)。"*
//!
//! The design's earlier answer to "the executor misreports" was to put the
//! evidence in front of the decision layer and hope it noticed. That is the one
//! thing this architecture forbids — it would pull the raw material back into
//! the scarcest context in the system — and it is also the weaker check, because
//! a reader who is skimming notices nothing. DESIGN's revised rule is stronger
//! and cheaper: *"这比'读证据'更强:机器判,不靠人看;而且它第一次让误报**可被抓**,而不只是
//! 可见."*
//!
//! So the check is here, it is deterministic, and its answer is one of three
//! things rather than two. **Undecided is not a courtesy**: a command whose exit
//! status was never recovered leaves the claim neither proven nor disproven, and
//! collapsing that into "fine" is exactly how a harness reports success it did
//! not observe. Only a **refutation** is a contradiction, because only a
//! refutation is a fact the harness has that the executor's account contradicts.

use crate::outcomes::Expects;

/// What the record shows about one run, in the only terms a claim can be
/// checked against.
///
/// Deliberately not the report and not the transcript: a check that read prose
/// would be making a judgement, and the whole point is that this one is free and
/// reproducible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Whether the run reached completion rather than being stopped.
    pub completed: bool,
    /// One entry per command that ran: its label, and its exit status where the
    /// record captured one.
    ///
    /// A `None` status means the command ran and the status was not recovered —
    /// *not* that it succeeded or failed. A reduction that replaced the output
    /// with a pointer is the common cause, and guessing there would be inventing
    /// the evidence the check exists to use.
    pub commands: Vec<(String, Option<i32>)>,
}

impl Facts {
    /// The first command that reported a non-zero status.
    fn first_failure(&self) -> Option<(&str, i32)> {
        self.commands
            .iter()
            .find_map(|(label, code)| match code {
                Some(code) if *code != 0 => Some((label.as_str(), *code)),
                _ => None,
            })
    }

    /// A command whose status was never recovered.
    fn unknown_status(&self) -> Option<&str> {
        self.commands
            .iter()
            .find_map(|(label, code)| code.is_none().then_some(label.as_str()))
    }

    fn successes(&self) -> usize {
        self.commands
            .iter()
            .filter(|(_, code)| *code == Some(0))
            .count()
    }
}

/// What the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The record shows what the landing claimed.
    Confirmed,
    /// The record shows the landing is false. This is the contradiction, and it
    /// is reported upward as an event rather than absorbed.
    Refuted(String),
    /// The record does not settle it.
    ///
    /// Kept apart from both of the others. "We could not check" and "we checked
    /// and it held" are different findings, and a harness that reported the
    /// first as the second would be manufacturing the assurance it was built to
    /// provide.
    Undecided(String),
}

impl Verdict {
    pub fn is_refuted(&self) -> bool {
        matches!(self, Verdict::Refuted(_))
    }

    /// The reason, for either of the two non-confirming answers.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Confirmed => None,
            Verdict::Refuted(why) | Verdict::Undecided(why) => Some(why),
        }
    }
}

/// Check one landing against what the record shows.
///
/// **No catch-all arm, deliberately.** `Expects` is a closed vocabulary the
/// harness owns — DESIGN's constraint 0 draws the line there, not at the sets —
/// so a new claim must be given a way of being checked before anything compiles.
/// A `_ =>` here would let a new claim be silently confirmed, which is the one
/// failure a checker must not have.
pub fn check(expects: Expects, facts: &Facts) -> Verdict {
    match expects {
        Expects::Succeeded => {
            if !facts.completed {
                return Verdict::Refuted(
                    "the run did not complete, so it did not succeed".to_owned(),
                );
            }
            if let Some((label, code)) = facts.first_failure() {
                return Verdict::Refuted(format!("a command ran and failed: `{label}` exited {code}"));
            }
            if facts.commands.is_empty() {
                // DESIGN's fourth trigger, made machine-checkable: "自称成功但
                // **无可运行证据**". A pass with nothing behind it is the exact
                // shape of a claim this harness exists to refuse.
                return Verdict::Refuted(
                    "no command ran, so there is nothing that would have failed had it been \
                     broken"
                        .to_owned(),
                );
            }
            if let Some(label) = facts.unknown_status() {
                return Verdict::Undecided(format!(
                    "`{label}` ran and its exit status was not recovered, so success is \
                     neither shown nor contradicted"
                ));
            }
            Verdict::Confirmed
        }

        Expects::Failed => {
            if facts.first_failure().is_some() {
                return Verdict::Confirmed;
            }
            if let Some(label) = facts.unknown_status() {
                return Verdict::Undecided(format!(
                    "`{label}` ran and its exit status was not recovered, so failure is \
                     neither shown nor contradicted"
                ));
            }
            Verdict::Refuted(format!(
                "every command that ran succeeded ({} of them), so nothing failed",
                facts.successes()
            ))
        }

        Expects::NothingRun => {
            if !facts.completed {
                // Absorbing a stopped run here would turn an unexpected stop into
                // a clean-looking "nothing to do" — the squeeze-into-the-nearest-
                // bucket failure the escape hatch exists to prevent, arriving
                // through the checker instead of the model.
                return Verdict::Refuted(
                    "the run stopped before it finished, so it has not shown that nothing \
                     needed to run"
                        .to_owned(),
                );
            }
            if !facts.commands.is_empty() {
                return Verdict::Refuted(format!(
                    "{} command(s) ran, so something did need to run",
                    facts.commands.len()
                ));
            }
            Verdict::Confirmed
        }

        Expects::StoppedEarly => {
            if facts.completed {
                return Verdict::Refuted(
                    "the run completed, so it did not stop early".to_owned(),
                );
            }
            Verdict::Confirmed
        }

        Expects::Unchecked => Verdict::Undecided(
            "this landing makes no claim the record can check".to_owned(),
        ),
    }
}

/// The landing the harness itself can make, for a run that never got to speak.
///
/// A cut-off executor is gone; nothing it said can be trusted about how it
/// ended, and DESIGN judges a set by how often `other` fills up — so a set that
/// correctly predicted a timeout would read as a set that was wrong if the
/// harness could not land it. This is the one landing the harness makes rather
/// than checks, and it is made only from the fact that the run stopped.
///
/// `None` when the set does not declare such a slot: then `other` is the honest
/// answer, and it rings.
pub fn harness_landing_for(stopped: bool, declared: &[crate::outcomes::Slot]) -> Option<String> {
    if !stopped {
        return None;
    }
    declared
        .iter()
        .find(|slot| slot.expects == Expects::StoppedEarly)
        .map(|slot| slot.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(label: &str, code: Option<i32>) -> (String, Option<i32>) {
        (label.to_owned(), code)
    }

    fn facts(completed: bool, commands: Vec<(String, Option<i32>)>) -> Facts {
        Facts { completed, commands }
    }

    /// **M3: "自洽检查真的会触发 —— 构造一次执行者误报(落位说 `pass`,工具结果 exit 非零)."**
    #[test]
    fn a_pass_claimed_over_a_failing_command_is_refuted() {
        let verdict = check(
            Expects::Succeeded,
            &facts(true, vec![ran("cargo test", Some(1))]),
        );
        let why = verdict.reason().expect("a refutation has a reason");
        assert!(verdict.is_refuted(), "{verdict:?}");
        assert!(
            why.contains("cargo test") && why.contains('1'),
            "the refutation has to name the command and its status, or it is an \
             accusation without a fact in it: {why}"
        );
    }

    /// The other half of DESIGN's fourth trigger: success with nothing behind it.
    #[test]
    fn a_pass_with_no_command_behind_it_is_refuted_rather_than_confirmed() {
        let verdict = check(Expects::Succeeded, &facts(true, vec![]));
        assert!(verdict.is_refuted(), "{verdict:?}");
        assert!(verdict.reason().unwrap().contains("no command ran"));
    }

    #[test]
    fn a_refutation_is_not_an_opinion_about_a_run_that_merely_stopped() {
        // One run, three claims, three answers — and each answer is about the
        // claim rather than about the run's mood. A run that stopped after a
        // green command did not succeed (it did not finish), did not fail (the
        // command was green), and did stop early. None of those is inferred from
        // another.
        let stopped = facts(false, vec![ran("cargo build", Some(0))]);
        assert!(
            check(Expects::Succeeded, &stopped).is_refuted(),
            "not finishing is not succeeding"
        );
        assert!(
            check(Expects::Failed, &stopped).is_refuted(),
            "a green command is not evidence of failure just because the run stopped"
        );
        assert_eq!(check(Expects::StoppedEarly, &stopped), Verdict::Confirmed);
    }

    /// A claim the record cannot settle is neither confirmed nor refuted.
    #[test]
    fn an_unrecovered_status_leaves_the_claim_undecided_rather_than_confirming_it() {
        for expects in [Expects::Succeeded, Expects::Failed] {
            let verdict = check(expects, &facts(true, vec![ran("cargo test", None)]));
            assert!(
                matches!(verdict, Verdict::Undecided(_)),
                "{expects:?} was answered {verdict:?} for a command whose status was \
                 never recovered — collapsing that into an answer manufactures the \
                 assurance this check exists to provide"
            );
        }
    }

    #[test]
    fn nothing_run_refuses_a_run_that_stopped_before_showing_it() {
        let stopped = facts(false, vec![]);
        assert!(
            check(Expects::NothingRun, &stopped).is_refuted(),
            "an unexpected stop must not arrive as a clean-looking 'nothing to do'"
        );
        assert_eq!(
            check(Expects::NothingRun, &facts(true, vec![])),
            Verdict::Confirmed
        );
        assert!(check(Expects::NothingRun, &facts(true, vec![ran("ls", Some(0))])).is_refuted());
    }

    #[test]
    fn stopped_early_is_refuted_by_a_run_that_completed() {
        assert!(check(Expects::StoppedEarly, &facts(true, vec![])).is_refuted());
        assert_eq!(
            check(Expects::StoppedEarly, &facts(false, vec![])),
            Verdict::Confirmed
        );
    }

    #[test]
    fn an_unchecked_slot_is_undecided_rather_than_silently_confirmed() {
        let verdict = check(Expects::Unchecked, &facts(true, vec![]));
        assert!(matches!(verdict, Verdict::Undecided(_)), "{verdict:?}");
        assert!(!verdict.is_refuted());
    }

    #[test]
    fn a_green_run_confirms_a_pass_and_refutes_a_failure_claim() {
        let green = facts(true, vec![ran("cargo test", Some(0))]);
        assert_eq!(check(Expects::Succeeded, &green), Verdict::Confirmed);
        assert!(check(Expects::Failed, &green).is_refuted());
    }

    #[test]
    fn the_first_failure_is_named_rather_than_the_last() {
        let verdict = check(
            Expects::Succeeded,
            &facts(
                true,
                vec![ran("build", Some(0)), ran("test", Some(101)), ran("lint", Some(2))],
            ),
        );
        let why = verdict.reason().unwrap();
        assert!(
            why.contains("test") && !why.contains("lint"),
            "the earliest failure is the cause of the rest and the one worth naming: {why}"
        );
    }

    /// The one landing the harness makes for a run that cannot speak for itself.
    #[test]
    fn a_stopped_run_is_landed_by_the_harness_only_when_the_set_declared_the_slot() {
        use crate::outcomes::{OutcomeSet, Slot};
        let declared = |expects: Expects| OutcomeSet {
            id: "build".to_owned(),
            description: None,
            slots: vec![Slot {
                name: "timed_out".to_owned(),
                expects,
            }],
        };
        assert_eq!(
            harness_landing_for(true, &declared(Expects::StoppedEarly).slots).as_deref(),
            Some("timed_out")
        );
        assert_eq!(
            harness_landing_for(true, &declared(Expects::Succeeded).slots),
            None,
            "a set with no slot for it must fall to `other`, where it rings, rather \
             than being squeezed into a slot that claims something else"
        );
        assert_eq!(
            harness_landing_for(false, &declared(Expects::StoppedEarly).slots),
            None,
            "a run that completed is not landed by the harness at all"
        );
    }
}
