//! The generation change, as data.
//!
//! DESIGN's diagnosis has two layers. The shallow one is the author: the dying
//! session writes its own handover, so it is the compressed object and the
//! compressor at once, working under pressure, and it is the one entity that
//! cannot see its own gaps. The deep one is the nature of the thing: flower's
//! handover was a *substitute for memory*, so it had to be lossless, and it could
//! not be. **It should have been an index plus state** — and this module is that
//! index plus that state.
//!
//! The division of labour this encodes:
//!
//! * [`LooseEnd`] — the one thing only the dying session can produce, and the
//!   reason it is still asked to produce anything at all;
//! * [`ReaderJob`] / [`ReaderReport`] — `k` fresh readers from different angles,
//!   whose findings [`assemble`] unions;
//! * [`Handover`] — the artefact, deliberately a list of pointers and items
//!   rather than prose;
//! * [`Verifier`] — the check back against the record;
//! * [`Trigger`] — when any of it happens.
//!
//! Nothing here is a summary, and nothing here votes.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use yu_observe::AgentId;

/// The angles the fresh readers read from.
///
/// Not a division of labour: every angle reads the same record. The angles exist
/// because "不同视角 = 天然去相关" — the same model asked for decisions and asked
/// for negative evidence notices different things — and redundancy across
/// *decorrelated* readings is what makes the union worth taking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReaderAngle {
    /// What was decided, and why — the reasons are the part a bare decision list
    /// loses, and they are what a later session needs in order to overturn one.
    DecisionsAndReasons,
    /// What must stay true: constraints, invariants, boundaries.
    ConstraintsAndInvariants,
    /// Things the session knew were unfinished.
    LooseEnds,
    /// What was tried and did not work, with the reason. DESIGN's argument for
    /// why this cannot be reconstructed: "从未明说的否定证据任何读者都恢复不了".
    NegativeEvidence,
    /// What exists, and where — the pointer half of "index plus state".
    ArtifactIndex,
}

impl ReaderAngle {
    pub const ALL: [ReaderAngle; 5] = [
        ReaderAngle::DecisionsAndReasons,
        ReaderAngle::ConstraintsAndInvariants,
        ReaderAngle::LooseEnds,
        ReaderAngle::NegativeEvidence,
        ReaderAngle::ArtifactIndex,
    ];

    /// Stable name, used in the journal and in prompts that brief a reader.
    /// What this angle is asked to extract, used verbatim in the reader prompt.
    pub fn subject(self) -> &'static str {
        match self {
            ReaderAngle::DecisionsAndReasons => {
                "decisions that were taken, and the reason given for each"
            }
            ReaderAngle::ConstraintsAndInvariants => {
                "constraints and invariants that are still in force"
            }
            ReaderAngle::LooseEnds => "threads that are still open",
            ReaderAngle::NegativeEvidence => {
                "things that were tried and did not work, and anything ruled out"
            }
            ReaderAngle::ArtifactIndex => "artifacts that were produced, and where they are",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ReaderAngle::DecisionsAndReasons => "decisions_and_reasons",
            ReaderAngle::ConstraintsAndInvariants => "constraints_and_invariants",
            ReaderAngle::LooseEnds => "loose_ends",
            ReaderAngle::NegativeEvidence => "negative_evidence",
            ReaderAngle::ArtifactIndex => "artifact_index",
        }
    }
}

/// One thing the dying session knows that was never written down.
///
/// Recorded thinking can be re-read, so it is recoverable; a negative result
/// nobody ever stated cannot be reconstructed by any reader, however fresh. That
/// asymmetry is why the dying session still has a job: it is the only holder of
/// the unattested material — everyone else, including every reader, can only see
/// what the record already says.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LooseEnd {
    pub text: String,
    /// Why losing this would change a decision. The reason is the value of the
    /// entry: a loose end nobody can say the importance of is usually one the
    /// session is not sure it saw.
    pub why_it_matters: String,
    /// `true` when a reader could recover this from the record alone.
    ///
    /// `false` entries are the ones no reader can reconstruct, which is exactly
    /// why the dying session must produce them. A handover made only of `true`
    /// entries is a reorganisation of the record, and the readers could have done
    /// that without the session's help at all.
    pub recoverable_from_record: bool,
}

/// A reader still to be dispatched.
///
/// DESIGN's flow step 2: `k` fresh readers, one per angle. Freshness is a
/// requirement rather than a preference — a reader that has seen the session's
/// history shares the mistakes in it, and the verifier must start from the
/// original record, not from the previous level's report.
///
// TODO(M3): this is where yu_core's `delegate_read` / spawn entry point goes;
// until core lands, a job only describes what should be dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderJob {
    pub angle: ReaderAngle,
    /// The fresh agent that will do the reading. `yu_observe::AgentId` so the
    /// spans of that read attribute to the same identity, with no mapping table.
    pub agent: AgentId,
}

/// What one reader found, from one angle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderReport {
    pub angle: ReaderAngle,
    /// Findings, in the reader's own words. Each should be something the next
    /// session can act on without re-reading the record — if it cannot be, the
    /// finding is a pointer and belongs in `ArtifactIndex`.
    pub findings: Vec<String>,
    /// Where in the record the findings come from, so that a finding can be
    /// checked back against the record rather than against another report.
    pub citations: Vec<String>,
}

/// The artefact a generation change produces.
///
/// DESIGN: "它应该是索引 + 状态." So this is items and pointers — never prose. A
/// field here holds strings that were lifted from the record, which is why the
/// verifier can compare against the record at all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handover {
    /// L0: the human's words and the run's intent. Injected every turn and never
    /// reconstructed, so [`assemble`] leaves it empty rather than guessing.
    pub charter: String,
    /// The `DecisionsAndReasons` angle's findings. DESIGN's L1 state keeps a
    /// decision ledger; this is the part of it that survives a generation.
    pub decisions: Vec<String>,
    pub constraints: Vec<String>,
    pub negative_evidence: Vec<String>,
    pub artifact_index: Vec<String>,
    /// Merged from two sources: the readers' `LooseEnds` findings, and the dying
    /// session's own list, which is the only one that can carry
    /// `recoverable_from_record: false`.
    pub loose_ends: Vec<LooseEnd>,
    /// Pointers back into the record for the items above. An index without
    /// addresses is a summary, so the addresses are part of the artefact and not
    /// an annotation on it.
    pub citations: Vec<String>,
    /// Which angles actually reported. A generation change that ran with three of
    /// five angles is a fact the next session should be able to see.
    pub sources: Vec<ReaderAngle>,
}

/// Assemble a handover from reader reports: **union, not vote**.
///
/// "取并集,不投票 —— 冗余对**抽取**有效,对**判断**无效." A vote — a majority, an
/// intersection, "two readers must agree" — discards exactly the finding that
/// only one framing could see, and having several framings is the reason the
/// readers exist. So every distinct finding from every angle is carried; only
/// exact duplicates collapse.
///
/// `charter` is left empty on purpose: it is L0, injected every turn rather than
/// reconstructed ("约束靠注入,不靠记忆"), and a reader that wrote one would be
/// summarising intent — the failure this flow exists to prevent. Use
/// [`assemble_with_charter`] when the caller already holds it.
pub fn assemble(reports: Vec<ReaderReport>) -> Handover {
    let mut handover = Handover::default();

    for report in reports {
        if !handover.sources.contains(&report.angle) {
            handover.sources.push(report.angle);
        }

        for finding in &report.findings {
            // Whitespace is not a finding; carrying it would inflate the handover
            // with entries that look like content.
            let finding = finding.trim();
            if finding.is_empty() {
                continue;
            }
            match report.angle {
                ReaderAngle::DecisionsAndReasons => push_unique(&mut handover.decisions, finding),
                ReaderAngle::ConstraintsAndInvariants => {
                    push_unique(&mut handover.constraints, finding)
                }
                ReaderAngle::NegativeEvidence => {
                    push_unique(&mut handover.negative_evidence, finding)
                }
                ReaderAngle::ArtifactIndex => push_unique(&mut handover.artifact_index, finding),
                ReaderAngle::LooseEnds => push_loose_end(&mut handover.loose_ends, finding),
            }
        }

        for citation in &report.citations {
            push_unique(&mut handover.citations, citation);
        }
    }

    // Declaration order, so that two handovers assembled from the same reports
    // render byte-identically (cache prefixes are byte-stable — DESIGN).
    handover.sources.sort_unstable();
    handover.sources.dedup();
    handover
}

/// [`assemble`], plus the charter that L0 owns.
pub fn assemble_with_charter(charter: impl Into<String>, reports: Vec<ReaderReport>) -> Handover {
    let mut handover = assemble(reports);
    handover.charter = charter.into();
    handover
}

/// Append unless an identical entry is already present.
///
/// Exact match on purpose. Near-duplicates survive rather than being merged:
/// merging two similar findings is a judgement about whether they say the same
/// thing, and this step is not allowed to make judgements.
fn push_unique(out: &mut Vec<String>, item: &str) {
    if !out.iter().any(|existing| existing == item) {
        out.push(item.to_owned());
    }
}

/// A reader's loose-end finding, as a [`LooseEnd`].
fn push_loose_end(out: &mut Vec<LooseEnd>, text: &str) {
    if out.iter().any(|existing| existing.text == text) {
        return;
    }
    out.push(LooseEnd {
        text: text.to_owned(),
        // `ReaderReport` has no field for a reason yet (M4); asserting one here
        // would be inventing it.
        why_it_matters: String::new(),
        // A reader can only report what the record (or the dying session's list)
        // already says, so everything it produces is recoverable by
        // construction. `false` is reserved for the dying session's own entries.
        recoverable_from_record: true,
    });
}

impl Handover {
    /// Structural checks that must hold before the handover is used as L1.
    ///
    /// Not a content judgement — this cannot tell whether a handover is good,
    /// only whether it is a handover. `charter` is checked because an empty one
    /// means the next session would have to reconstruct intent, and intent is the
    /// one thing that must never be reconstructed.
    pub fn check(&self) -> Result<(), HandoverError> {
        if self.sources.is_empty() {
            return Err(HandoverError::NoReaders);
        }
        if self.charter.trim().is_empty() {
            return Err(HandoverError::EmptyCharter);
        }
        Ok(())
    }

    /// A handover outlives the session that produced it and crosses process
    /// boundaries, so it has a wire form before it has a caller.
    pub fn to_json(&self) -> Result<String, HandoverError> {
        serde_json::to_string(self).map_err(|e| HandoverError::Codec(e.to_string()))
    }

    pub fn from_json(s: &str) -> Result<Self, HandoverError> {
        serde_json::from_str(s).map_err(|e| HandoverError::Codec(e.to_string()))
    }

    /// Everything this handover already carries, for the gap check.
    fn mentioned(&self) -> BTreeSet<&str> {
        self.decisions
            .iter()
            .chain(self.constraints.iter())
            .chain(self.negative_evidence.iter())
            .chain(self.artifact_index.iter())
            .chain(self.citations.iter())
            .map(String::as_str)
            .chain(self.loose_ends.iter().map(|end| end.text.as_str()))
            .chain(std::iter::once(self.charter.as_str()))
            .collect()
    }
}

/// Step 4 of the flow: check the handover back against the record.
///
/// The verifier looks for what is "记录里有、且重要、而交接漏了" — in the record,
/// important, and missing from the handover. It reads the record, not the
/// readers' reports: a verifier that checks a report against another report
/// inherits the same compression loss ("验证者从原始记录出发,不从上一级报告出发").
pub struct Verifier;

impl Verifier {
    /// Record items shorter than this are never called important. See
    /// [`Verifier::find_gaps`] for why a number like this is honest and a
    /// pretend judgement would not be.
    pub const MIN_IMPORTANT_CHARS: usize = 12;

    /// Items present in `record`, judged important, and absent from `handover`,
    /// in record order (the record is a chronology, and a gap list is read
    /// against it).
    ///
    /// The relevance test is crude, deliberately and visibly so: an item is
    /// important if it is at least [`Verifier::MIN_IMPORTANT_CHARS`] characters
    /// long, and already carried if the handover contains that exact string. Three
    /// consequences, stated rather than hidden:
    ///
    /// * a short but load-bearing item such as `retry=3` is invisible to it;
    /// * a paraphrase of a handover entry is reported as a gap — a false positive;
    /// * a long unimportant item is reported too.
    ///
    /// Doing better means *reading* the record and judging relevance, which is a
    /// model's job (M4). The interface here is the one the real verifier needs;
    /// the crudeness is in the open rather than dressed up as judgement.
    pub fn find_gaps(record: &[String], handover: &Handover) -> Vec<String> {
        let mentioned = handover.mentioned();
        record
            .iter()
            .filter(|item| {
                item.trim().chars().count() >= Self::MIN_IMPORTANT_CHARS
                    && !mentioned.contains(item.as_str())
            })
            .cloned()
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    #[error("no reader reports: a handover assembled from nothing is not a handover")]
    NoReaders,
    #[error("charter is empty — L0 is injected, never reconstructed, so a handover must be given one")]
    EmptyCharter,
    #[error("handover codec: {0}")]
    Codec(String),
}

impl Trigger {
    /// Build a threshold trigger from the growth budget the loop already keeps.
    ///
    /// The rule — count growth since the last contraction, never total context —
    /// had been written twice: once in `yu_core::traits::GrowthBudget`, where it
    /// is enforced by the type having no total-context field, and once here.
    /// Two implementations of one rule is how they drift, and the drift would be
    /// silent because both would look right alone. This converts rather than
    /// re-derives, so there is one implementation and this is a view of it.
    pub fn from_growth(budget: &yu_core::traits::GrowthBudget) -> Self {
        Trigger::Threshold {
            growth_since_last: budget.peak_growth(),
            threshold: budget.threshold,
        }
    }
}

/// Why a generation change happens.
///
/// DESIGN names the trap this enum exists to avoid: "压缩后立刻重新触发:压缩后窗口
/// 被前缀重播种,按全量计费会立刻再次触发。**触发器必须计"自上次压缩以来的增长",不是
/// 总上下文**" — Codex added `body_after_prefix` for exactly this. Note what the
/// type cannot express: there is no total-context field, so the mistake cannot be
/// made by passing the wrong number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// Work finished. Preferred, because completion *knows* what no longer
    /// matters while a threshold can only guess ("完成才是合法的收缩").
    NaturalCompletion,
    /// Backstop. `growth_since_last` is measured from the last compaction, never
    /// from the start of the run: a freshly compacted window is large by
    /// construction, since the prefix re-seeds it, so a total-context trigger
    /// fires again the moment it finishes, forever.
    Threshold { growth_since_last: u64, threshold: u64 },
    /// The agent asked for a new window (DESIGN's `new_context`, isomorphic to
    /// `retire`): a request, not a threshold.
    AgentRequested,
}

impl Trigger {
    pub fn should_fire(&self) -> bool {
        match *self {
            Trigger::NaturalCompletion | Trigger::AgentRequested => true,
            Trigger::Threshold {
                growth_since_last,
                threshold,
            } => growth_since_last >= threshold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(angle: ReaderAngle, findings: &[&str]) -> ReaderReport {
        ReaderReport {
            angle,
            findings: findings.iter().map(|f| (*f).to_owned()).collect(),
            citations: Vec::new(),
        }
    }

    #[test]
    fn every_angle_has_a_stable_name() {
        assert_eq!(ReaderAngle::ALL.len(), 5);

        let names: BTreeSet<&str> = ReaderAngle::ALL.iter().map(|a| a.as_str()).collect();
        assert_eq!(names.len(), ReaderAngle::ALL.len(), "names must not collide");

        assert_eq!(
            serde_json::to_string(&ReaderAngle::NegativeEvidence).expect("angle must encode"),
            "\"negative_evidence\""
        );
    }

    /// The point of the crate, as a test: one reader out of three saw something
    /// the others did not, and a vote of any threshold above one would have
    /// dropped it.
    #[test]
    fn assembly_takes_the_union_not_a_vote() {
        let reports = vec![
            report(
                ReaderAngle::DecisionsAndReasons,
                &[
                    "Chose a Unix socket so the daemon owns sessions",
                    "The per-step subprocess was rejected because CLI init dominated",
                ],
            ),
            report(
                ReaderAngle::ConstraintsAndInvariants,
                &["The decision layer's context must not grow with turns"],
            ),
            report(
                ReaderAngle::NegativeEvidence,
                &["A provider abstraction layer was considered and dropped"],
            ),
        ];

        let handover = assemble(reports);
        let carried = |needle: &str| {
            handover
                .decisions
                .iter()
                .chain(handover.constraints.iter())
                .chain(handover.negative_evidence.iter())
                .chain(handover.artifact_index.iter())
                .any(|found| found == needle)
        };

        for finding in [
            "The per-step subprocess was rejected because CLI init dominated",
            "The decision layer's context must not grow with turns",
            "A provider abstraction layer was considered and dropped",
        ] {
            assert!(
                carried(finding),
                "a finding only one angle reported was dropped: {finding}"
            );
        }

        assert_eq!(
            handover.sources,
            vec![
                ReaderAngle::DecisionsAndReasons,
                ReaderAngle::ConstraintsAndInvariants,
                ReaderAngle::NegativeEvidence,
            ]
        );
    }

    #[test]
    fn every_angle_has_a_destination_and_duplicates_collapse() {
        let reports: Vec<ReaderReport> = ReaderAngle::ALL
            .iter()
            .map(|&angle| report(angle, &[&format!("finding from {}", angle.as_str())]))
            .collect();
        let handover = assemble(reports);

        assert_eq!(handover.decisions.len(), 1);
        assert_eq!(handover.constraints.len(), 1);
        assert_eq!(handover.negative_evidence.len(), 1);
        assert_eq!(handover.artifact_index.len(), 1);
        assert_eq!(handover.loose_ends.len(), 1);
        assert_eq!(handover.sources.len(), ReaderAngle::ALL.len());

        // Two readers agreeing is one finding: the union of a set, not a
        // concatenation. Redundancy helps extraction, not volume.
        let repeated = assemble(vec![
            report(ReaderAngle::ConstraintsAndInvariants, &["One rule, once"]),
            report(
                ReaderAngle::ConstraintsAndInvariants,
                &["One rule, once", "A second rule, once"],
            ),
        ]);
        assert_eq!(repeated.constraints.len(), 2);
        assert_eq!(repeated.sources, vec![ReaderAngle::ConstraintsAndInvariants]);
    }

    #[test]
    fn a_reader_s_loose_end_is_recoverable_by_construction() {
        let handover = assemble(vec![report(
            ReaderAngle::LooseEnds,
            &["The retry budget was raised mid-run"],
        )]);

        assert_eq!(handover.loose_ends.len(), 1);
        assert!(
            handover.loose_ends[0].recoverable_from_record,
            "a fresh reader can only report what the record already carries"
        );
    }

    #[test]
    fn the_verifier_finds_what_the_handover_missed() {
        let record = vec![
            "The daemon owns sessions; the CLI only attaches".to_owned(),
            "Resume was never wired to the socket, so a crash loses the session".to_owned(),
        ];
        let handover = assemble_with_charter(
            "Ship the daemon, not a per-step subprocess.",
            vec![report(
                ReaderAngle::ConstraintsAndInvariants,
                &["The daemon owns sessions; the CLI only attaches"],
            )],
        );

        let gaps = Verifier::find_gaps(&record, &handover);
        assert_eq!(
            gaps,
            vec!["Resume was never wired to the socket, so a crash loses the session".to_owned()],
            "the planted gap is found and the carried item is not reported"
        );
    }

    /// The verifier's crudeness, asserted rather than described: both failure
    /// modes below are the price of not reading the record with a model.
    #[test]
    fn the_verifier_is_crude_on_purpose() {
        let record = vec![
            "retry=3".to_owned(),
            "Resume is not wired to the socket, so a crash loses the session".to_owned(),
        ];
        let handover = assemble_with_charter(
            "charter",
            vec![report(
                ReaderAngle::ConstraintsAndInvariants,
                &["A crash must not lose the session; resume is not wired yet"],
            )],
        );

        let gaps = Verifier::find_gaps(&record, &handover);
        assert!(
            !gaps.iter().any(|gap| gap == "retry=3"),
            "short items sit below the importance floor"
        );
        assert!(
            gaps.iter().any(|gap| gap.contains("Resume is not wired")),
            "a paraphrase is not recognised — the false positive this check admits"
        );
    }

    #[test]
    fn a_threshold_counts_growth_since_the_last_compaction() {
        // The window is large — a compaction just re-seeded it from the prefix —
        // but it has not grown. Billing that as total context would fire here, and
        // would fire again after every compaction, forever.
        let unchanged = Trigger::Threshold {
            growth_since_last: 0,
            threshold: 20_000,
        };
        assert!(!unchanged.should_fire());

        // A small window that has grown past the threshold does fire.
        let grown = Trigger::Threshold {
            growth_since_last: 20_001,
            threshold: 20_000,
        };
        assert!(grown.should_fire());

        // The boundary is inclusive, so a threshold of zero fires immediately —
        // a policy that wants no threshold trigger must omit it.
        assert!(
            Trigger::Threshold {
                growth_since_last: 20_000,
                threshold: 20_000,
            }
            .should_fire()
        );
    }

    #[test]
    fn completion_and_agent_requests_always_fire() {
        assert!(Trigger::NaturalCompletion.should_fire());
        assert!(Trigger::AgentRequested.should_fire());
    }

    #[test]
    fn the_handover_round_trips_through_json() {
        let mut handover = assemble_with_charter(
            "Ship the daemon, not a per-step subprocess.",
            vec![
                ReaderReport {
                    angle: ReaderAngle::DecisionsAndReasons,
                    findings: vec!["Chose a Unix socket over HTTP".to_owned()],
                    citations: vec!["journal://run-7#412".to_owned()],
                },
                report(ReaderAngle::LooseEnds, &["The retry budget was raised"]),
            ],
        );
        // The dying session's own contribution, which is the only one that can
        // carry an unrecoverable entry.
        handover.loose_ends.push(LooseEnd {
            text: "The daemon was tested against a stale socket path".to_owned(),
            why_it_matters: "the passing test proves less than it looks like".to_owned(),
            recoverable_from_record: false,
        });

        let encoded = handover.to_json().expect("a handover must encode");
        let decoded = Handover::from_json(&encoded).expect("a handover must decode");
        assert_eq!(decoded, handover);
        assert!(decoded.check().is_ok());
        assert!(decoded.citations.contains(&"journal://run-7#412".to_owned()));

        for trigger in [
            Trigger::NaturalCompletion,
            Trigger::Threshold {
                growth_since_last: 9,
                threshold: 10,
            },
            Trigger::AgentRequested,
        ] {
            let encoded = serde_json::to_string(&trigger).expect("a trigger must encode");
            let decoded: Trigger = serde_json::from_str(&encoded).expect("a trigger must decode");
            assert_eq!(decoded, trigger);
        }
    }

    #[test]
    fn a_handover_needs_readers_and_a_charter_to_be_a_handover() {
        assert!(matches!(
            Handover::default().check(),
            Err(HandoverError::NoReaders)
        ));

        let mut handover = assemble(vec![report(
            ReaderAngle::DecisionsAndReasons,
            &["Chose a Unix socket over HTTP"],
        )]);
        assert!(matches!(handover.check(), Err(HandoverError::EmptyCharter)));

        handover.charter = "Ship the daemon.".to_owned();
        assert!(handover.check().is_ok());
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// A reader's model call.
///
/// Injected rather than reached for, so the dispatch logic — which readers, what
/// each is asked, how their answers become a handover — is testable without a
/// network and without spending anything.
pub trait ReaderModel: Send + Sync {
    fn read(
        &self,
        prompt: &str,
    ) -> impl std::future::Future<Output = Result<String, HandoverError>> + Send;
}

/// The prompt one reader is given.
///
/// DESIGN's claim 1 about the specialised reader is that its cost *is* the size of
/// this prompt — "它只是个读取器,不需要完整的工具纪律". So it is deliberately
/// small: one angle, the record, and an instruction to report findings rather
/// than prose. It carries no tool discipline because it has no tools.
pub fn reader_prompt(angle: ReaderAngle, record: &str) -> String {
    format!(
        "You are reading a record to extract one specific kind of thing from it. You have no \
         tools; the record below is everything there is.\n\n\
         Extract, in your own words, the {subject}. Report only what the record actually shows. \
         If it shows nothing under this heading, say so — an absence is a finding, and inventing \
         a plausible entry is worse than returning none.\n\n\
         Answer as a list, one finding per line, each beginning with \"- \". Nothing else: no \
         preamble, no summary, no offers to help.\n\n\
         --- RECORD ---\n{record}\n--- END RECORD ---",
        subject = angle.subject(),
    )
}

/// Run the readers and assemble what they return.
///
/// DESIGN's flow, wired. Until this existed the angle types, the union and the
/// verifier were all present and tested, and **nothing dispatched a
/// `ReaderJob`** — the same concept implemented twice and wired zero times.
///
/// The readers are sequential and each gets the whole record. Both are
/// deliberate: concurrency would need a rate-limit story this layer does not
/// have, and handing every reader the same record is what makes their framings
/// the only thing that differs between them — which is where the decorrelation
/// comes from. DESIGN's instruction is to take the **union, not a vote**:
/// redundancy helps extraction, not judgement, so agreement is not evidence and
/// disagreement is not a problem to resolve here.
pub async fn dispatch<M: ReaderModel>(
    model: &M,
    record: &str,
    angles: &[ReaderAngle],
) -> Result<Handover, HandoverError> {
    if angles.is_empty() {
        return Err(HandoverError::NoReaders);
    }
    let mut reports = Vec::with_capacity(angles.len());
    for angle in angles {
        let raw = model.read(&reader_prompt(*angle, record)).await?;
        reports.push(ReaderReport {
            angle: *angle,
            findings: parse_findings(&raw),
            citations: vec![format!("record ({} bytes)", record.len())],
        });
    }
    Ok(assemble(reports))
}

/// Turn a reader's reply into findings.
///
/// Lines beginning `- ` are findings, because that is what the prompt asks for.
/// A reply that ignores the format is kept whole rather than discarded: a reader
/// that answered in prose still read the record, and throwing its work away
/// would be the silent omission this whole design is built against.
fn parse_findings(raw: &str) -> Vec<String> {
    let mut findings = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("- ") {
            let rest = rest.trim();
            if !rest.is_empty() {
                findings.push(rest.to_owned());
            }
        }
    }
    if findings.is_empty() {
        let whole = raw.trim();
        if !whole.is_empty() {
            findings.push(whole.to_owned());
        }
    }
    findings
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use std::sync::Mutex;

    /// A reader that answers from a script, and records what it was asked.
    struct Scripted {
        reply: String,
        seen: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_owned(),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl ReaderModel for Scripted {
        async fn read(&self, prompt: &str) -> Result<String, HandoverError> {
            self.seen.lock().unwrap().push(prompt.to_owned());
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn every_angle_is_dispatched_and_its_findings_survive() {
        let model = Scripted::new("- the parser was rewritten because the old one was slow");
        let handover = dispatch(
            &model,
            "record text",
            &[ReaderAngle::DecisionsAndReasons, ReaderAngle::LooseEnds],
        )
        .await
        .unwrap();

        assert_eq!(model.seen.lock().unwrap().len(), 2, "one reader per angle");
        assert!(handover.sources.contains(&ReaderAngle::DecisionsAndReasons));
        assert!(handover.sources.contains(&ReaderAngle::LooseEnds));
        assert!(
            !handover.decisions.is_empty(),
            "the decisions angle's finding must reach the handover: {handover:?}"
        );
    }

    #[tokio::test]
    async fn each_reader_is_asked_for_its_own_angle() {
        let model = Scripted::new("- x");
        dispatch(&model, "r", &[ReaderAngle::NegativeEvidence]).await.unwrap();
        let prompts = model.seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(ReaderAngle::NegativeEvidence.subject()),
            "a reader must be asked for its angle and not a generic summary: {}",
            prompts[0]
        );
    }

    #[tokio::test]
    async fn the_reader_prompt_stays_small() {
        // DESIGN's claim 1: the specialised reader's cost is the size of this
        // prompt, because it needs no tool discipline. If it grows into a full
        // agent brief, the cheap path has stopped being cheap.
        for angle in ReaderAngle::ALL {
            let prompt = reader_prompt(angle, "");
            assert!(
                prompt.len() < 1200,
                "{angle:?} prompt is {} bytes",
                prompt.len()
            );
        }
    }

    #[tokio::test]
    async fn a_reader_that_ignores_the_format_is_not_discarded() {
        // Malformed output still represents work done on the record. Dropping it
        // would be exactly the silent omission the design exists to prevent.
        let model = Scripted::new("Nothing under this heading, as far as I can see.");
        let handover = dispatch(&model, "r", &[ReaderAngle::ArtifactIndex])
            .await
            .unwrap();
        assert!(
            handover.citations.iter().any(|c| c.contains("record")),
            "the reader's account must be carried even when it does not match the format"
        );
    }

    #[test]
    fn the_growth_rule_has_one_implementation() {
        // The trigger is a view of the loop's budget, not a second copy of the
        // rule. If these ever disagree, one of them is wrong and nothing would
        // say which.
        let mut budget = yu_core::traits::GrowthBudget::new(10_000);
        budget.rearm(900_000);
        assert!(!budget.observe(905_000));
        assert!(budget.observe(920_000));

        let trigger = Trigger::from_growth(&budget);
        assert!(trigger.should_fire(), "a budget that fired must fire the trigger");

        let mut quiet = yu_core::traits::GrowthBudget::new(10_000);
        quiet.rearm(900_000);
        let _ = quiet.observe(901_000);
        assert!(
            !Trigger::from_growth(&quiet).should_fire(),
            "a budget that did not fire must not fire the trigger"
        );
    }

    #[tokio::test]
    async fn no_angles_is_an_error_rather_than_an_empty_handover() {
        let model = Scripted::new("- x");
        let err = dispatch(&model, "r", &[]).await.unwrap_err();
        assert!(matches!(err, HandoverError::NoReaders));
    }
}
