//! The generation change, as data — and as a run.
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
//!
//! # The flow, wired
//!
//! [`change_generation`] is DESIGN's four steps as one call a run makes:
//!
//! 1. the dying session lists its [`LooseEnd`]s ([`loose_end_prompt`]);
//! 2. fresh readers, one per [`ReaderAngle`], read [`Record`] plus those loose
//!    ends ([`dispatch`]);
//! 3. [`assemble_with_charter`] takes the **union**, never a vote;
//! 4. [`Verifier`] reads the record again for what is important and missing.
//!
//! The record the readers read is not text of this module's invention: it is the
//! lineage's own events, each with the address `yu_core::spot::resolve` follows
//! ([`Record::read`]). That is what makes the outcome an index rather than a
//! summary — every carried item can be anchored to the line it was lifted from
//! (an [`Anchor`]), and the new session can follow an address back to the bytes.
//!
//! Where it fires is [`trigger_for`]: completion first, because completion is the
//! legitimate contraction; the round ceiling, because a session that ran out of
//! rounds is over and its unfinished work is what the next session is for; the
//! growth threshold as the backstop for a stop that is neither; and the agent's
//! own request as the case `Trigger` names last (nothing produces it yet — see
//! [`Trigger::AgentRequested`]).

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yu_observe::AgentId;

use yu_core::decision::{DecisionOutcome, StoppedEarly};
use yu_core::render::{Charter, Decision, HumanUtterance, Ledger, NotLoadedEntry, Rejected};
use yu_journal::{Event, EventBody, Journal, JournalError, SessionId};
use yu_wire::client::{StreamSink, WireClient};
use yu_wire::types::{Message, MessagesRequest};

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// The most of one record item a reader is handed.
///
/// The hard cut is DESIGN's "恒常地板": a reader that has to ingest a spilled
/// hundred-kilobyte tool result in full would make the cheap path expensive, and
/// the address is right there for whoever needs the rest. What is cut is
/// **stated in the text**, so a reader never mistakes a prefix for the whole.
const ITEM_TEXT_BUDGET: usize = 2_000;

/// One thing the record holds, with the address that reaches it.
///
/// The address is one of the two forms `yu_core::spot::resolve` follows —
/// `blob:<digest>` for a payload in the store, `journal:seq=<n>` for text the
/// log holds at that position. A record whose items had no addresses would be
/// the thing DESIGN refuses: an index nobody can follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordItem {
    pub address: String,
    /// The record's own words at that address: lifted, never paraphrased, and
    /// cut only where the cut says so.
    pub text: String,
}

/// A lineage's record, as the readers and the verifier see it.
///
/// Read from the journal rather than passed in from a session's memory, because
/// the verifier's whole claim is that it starts from the record — a record built
/// from what a run remembers would inherit exactly the loss it exists to catch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Which session's log positions these addresses name. Together with
    /// `at_seq` this is the [`Origin`] a new session forks from.
    pub session: SessionId,
    /// One past the last event read: the point a continuation branches from.
    pub at_seq: u64,
    pub items: Vec<RecordItem>,
}

impl Record {
    /// Read the lineage's events and keep the ones that carry text.
    ///
    /// Every kind of text-carrying event is an item — the charter, a note (which
    /// is where decisions and retirements live), a tool call, a tool result, a
    /// recorded reply. Events with no text of their own (`model_request`, fork
    /// markers) are not items, and neither is a tool result a reduction dropped
    /// the payload of: an item nothing can be shown for would enter the
    /// verifier's record as a gap no one can fill.
    pub async fn read(journal: &Journal) -> Result<Self, JournalError> {
        let events = journal.history().await?;
        let at_seq = events.last().map(|event| event.seq + 1).unwrap_or(0);
        let items = events.iter().filter_map(|e| item_of(e, journal)).collect();
        Ok(Self {
            session: journal.session().clone(),
            at_seq,
            items,
        })
    }

    /// A record assembled from somewhere else — another store, a test fixture.
    pub fn of(session: SessionId, at_seq: u64, items: Vec<RecordItem>) -> Self {
        Self {
            session,
            at_seq,
            items,
        }
    }

    /// The record this came from, as a point a new session can branch at.
    pub fn origin(&self) -> Origin {
        Origin {
            session: self.session.clone(),
            at_seq: self.at_seq,
        }
    }

    /// The record as the readers read it: one line per item, addressed.
    ///
    /// The address is not decoration. The reader prompt asks a reader to begin
    /// each finding with the address of the line it came from, and a reader
    /// cannot copy an address it was never shown.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            out.push_str(&format!("[{}] {}\n", item.address, item.text));
        }
        out
    }

    /// What is handed to readers: the record, plus the dying session's loose
    /// ends — DESIGN's flow step 2 reads "记录 + 线头", and a reader given only
    /// the record cannot see what was never written into it.
    pub fn reading_text(&self, loose_ends: &[LooseEnd]) -> String {
        let mut out = self.render();
        if !loose_ends.is_empty() {
            out.push_str(
                "\n--- LOOSE ENDS, from the session that is ending; these are NOT in the \
                 record above ---\n",
            );
            for end in loose_ends {
                out.push_str(&format!("- {}\n", end.text));
            }
        }
        out
    }

    /// Every item's text, in record order — the universe the verifier checks
    /// against.
    pub fn texts(&self) -> Vec<String> {
        self.items.iter().map(|item| item.text.clone()).collect()
    }

    /// Where the item with exactly this text sits, when it is the record's own
    /// text and not a paraphrase of it.
    pub fn address_of(&self, text: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|item| item.text == text)
            .map(|item| item.address.as_str())
    }

    /// Whether this address names something the record holds.
    pub fn knows_address(&self, address: &str) -> bool {
        self.items.iter().any(|item| item.address == address)
    }

    /// The address of the item a finding quotes, when it quotes one.
    ///
    /// A reader that did not copy an address but did quote a line verbatim can
    /// still be anchored by the quote. The floor on the quoted length is what
    /// keeps a finding from being anchored to every short item that happens to
    /// be a substring of it; the longest match wins when several do.
    pub fn address_quoting(&self, finding: &str) -> Option<String> {
        self.items
            .iter()
            .filter(|item| {
                item.text.chars().count() >= MIN_QUOTED_CHARS && finding.contains(&item.text)
            })
            .max_by_key(|item| item.text.len())
            .map(|item| item.address.clone())
    }
}

/// The shortest record item a quote may be anchored to.
///
/// Twelve, the same floor [`Verifier::MIN_IMPORTANT_CHARS`] uses and for the
/// same reason: below it, a "quote" is as likely to be a coincidence of short
/// words as a citation.
const MIN_QUOTED_CHARS: usize = 12;

/// One event's text, with the address that reaches it.
fn item_of(event: &Event, journal: &Journal) -> Option<RecordItem> {
    let (address, text) = match &event.body {
        EventBody::SessionStarted { charter, .. } if !charter.trim().is_empty() => {
            (format!("journal:seq={}", event.seq), charter.clone())
        }
        EventBody::UserMessage { text } if !text.trim().is_empty() => {
            (format!("journal:seq={}", event.seq), text.clone())
        }
        EventBody::Note { text } if !text.trim().is_empty() => {
            (format!("journal:seq={}", event.seq), text.clone())
        }
        // The decision layer's ledger tools are not items: the call and the
        // note it leaves (`decide — decided: …`) are *one* fact in two
        // spellings, and listing both would make the verifier report whichever
        // spelling the handover did not happen to carry as a gap — noise on
        // every real run. The note is the readable spelling, so it is the item;
        // the call's exact JSON is in the record for whoever needs it.
        EventBody::ToolCall { name, .. } if yu_core::decide::is_decision_tool(name) => {
            return None;
        }
        EventBody::ToolCall { name, input, .. } => (
            format!("journal:seq={}", event.seq),
            format!("{name} {input}"),
        ),
        // The payload lives in exactly one of these two places (L2 constraint
        // 1), so the address follows wherever it went.
        EventBody::ToolResult { inline, blob, .. } => match (inline, blob) {
            (Some(text), _) if !text.trim().is_empty() => {
                (format!("journal:seq={}", event.seq), text.clone())
            }
            (None, Some(digest)) => {
                let text = journal.blobs().get_str(digest).ok()?;
                (format!("blob:{digest}"), text)
            }
            _ => return None,
        },
        EventBody::ModelResponse { text_blob, .. } => {
            let digest = text_blob.as_ref()?;
            let text = journal.blobs().get_str(digest).ok()?;
            (format!("blob:{digest}"), text)
        }
        EventBody::Injected { blob, .. } => {
            let text = journal.blobs().get_str(blob).ok()?;
            (format!("blob:{blob}"), text)
        }
        _ => return None,
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(RecordItem {
        address,
        text: cut_to_budget(&text),
    })
}

/// Cut an item to [`ITEM_TEXT_BUDGET`] characters, saying where the rest is.
fn cut_to_budget(text: &str) -> String {
    let total = text.chars().count();
    if total <= ITEM_TEXT_BUDGET {
        return text.to_owned();
    }
    // The budget counts characters — a CJK record is as long as it reads — so
    // the cut has to land on a character boundary rather than at a byte index.
    let end = text
        .char_indices()
        .nth(ITEM_TEXT_BUDGET)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    format!(
        "{}\n[cut: {ITEM_TEXT_BUDGET} of {total} characters — the rest is behind this item's address]",
        &text[..end]
    )
}

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
/// [`dispatch`] is what runs one, and it does not need this type: a reader's
/// identity costs nothing to leave to whoever owns a lineage, and the model call
/// is the whole dispatch. What survives here is the *description* — an angle and
/// the fresh agent that takes it — for a caller that does hold identities and
/// wants the jobs as data before running them (a plan, a fan-out, a test
/// fixture). Nothing in production reads it today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderJob {
    pub angle: ReaderAngle,
    /// The fresh agent that will do the reading. `yu_observe::AgentId` so the
    /// spans of that read attribute to the same identity, with no mapping table.
    pub agent: AgentId,
}

/// One carried item and the record line it was lifted from.
///
/// The pairing is the index. A flat list of addresses says that *something* came
/// from somewhere — which is what makes a summary indistinguishable from an
/// index — while an anchor says which item to go and check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// The carried item's text, exactly as it appears in its section of the
    /// handover.
    pub item: String,
    /// An address the record answers — `blob:<digest>` or `journal:seq=<n>`.
    pub address: String,
}

/// What one reader found, from one angle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReaderReport {
    pub angle: ReaderAngle,
    /// Findings, in the reader's own words. Each should be something the next
    /// session can act on without re-reading the record — if it cannot be, the
    /// finding is a pointer and belongs in `ArtifactIndex`.
    pub findings: Vec<String>,
    /// Where in the record the findings came from, so that a finding can be
    /// checked back against the record rather than against another report.
    pub anchors: Vec<Anchor>,
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
    /// Pointers back into the record, each paired with the item it belongs to.
    /// An index without addresses is a summary, so the addresses are part of the
    /// artefact and not an annotation on it — and each address has to resolve,
    /// which is what a consumer checks before trusting one.
    pub anchors: Vec<Anchor>,
    /// Which angles actually reported. A generation change that ran with three of
    /// five angles is a fact the next session should be able to see.
    pub sources: Vec<ReaderAngle>,
    /// The record this indexes. `None` for a handover assembled from reports
    /// alone, which is honest: without an origin there is no record to point at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
}

/// The record a handover indexes: which session, and how far into it the change
/// happened.
///
/// Carried because `journal:seq=<n>` names a position in *one* lineage's log.
/// Handed to a new session, the address is only resolvable if that session's
/// journal stands on this one — which is what [`Handover::resume_into`] makes
/// true, by branching rather than by copying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub session: SessionId,
    pub at_seq: u64,
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

        for anchor in &report.anchors {
            if !handover
                .anchors
                .iter()
                .any(|existing| existing.item == anchor.item && existing.address == anchor.address)
            {
                handover.anchors.push(anchor.clone());
            }
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
    /// boundaries: this is the form that goes into the record's store
    /// ([`Handover::store`]) and the form a later session reads back.
    ///
    /// Serialised exactly once per change, on purpose. The address the record
    /// keeps is the digest of these bytes, so re-serialising a handover to write
    /// it somewhere else would name an address the store does not have.
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
            .map(String::as_str)
            .chain(self.loose_ends.iter().map(|end| end.text.as_str()))
            .chain(self.anchors.iter().map(|anchor| anchor.item.as_str()))
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
            .filter(|item| important(item) && !mentioned.contains(item.as_str()))
            .cloned()
            .collect()
    }

    /// [`Verifier::find_gaps`], against an addressed record, with each gap's
    /// address attached.
    ///
    /// A gap without an address is a finding the caller cannot act on, and
    /// DESIGN's rule for a manifest applies to a gap list too: if something is
    /// named, how to reach it is part of naming it.
    ///
    /// **What "carried" means here, and why it is two rules.** An item is
    /// covered when the handover holds it in the record's own words — the exact
    /// match [`Verifier::find_gaps`] uses — **or** when one of the handover's
    /// anchors points at that record line, because an anchor is the handover
    /// saying *"this line was read, and here is what it says"*. The second rule
    /// matters more than it looks: a reader asked to extract something in its
    /// own words almost never writes the record line back verbatim, so the
    /// exact-match rule alone calls every extracted item a gap — measured on a
    /// scripted run, the gap list was the whole record, which is a check that
    /// fires on every run and is therefore read by nobody.
    ///
    /// What the anchor does **not** buy is a judgement about the finding. A
    /// reader that anchors a line and gets it wrong is not caught here; judging
    /// whether a finding is a fair account of the line it points at is what a
    /// verifier with a model is for (the interface is deliberately the one it
    /// needs). Two failure modes are therefore open, and both are stated rather
    /// than hidden: an unanchored paraphrase still reads as a gap, and a wrong
    /// anchored finding reads as coverage.
    ///
    /// **What this reads.** The record and the handover, and nothing else. The
    /// readers' reports are deliberately not a parameter: a verifier that checks
    /// a report against another report inherits the same compression loss, which
    /// is why M5's test corrupts those reports and requires the conclusion to
    /// survive unchanged.
    pub fn find_gaps_in(record: &Record, handover: &Handover) -> Vec<Gap> {
        let mentioned = handover.mentioned();
        let anchored: BTreeSet<&str> = handover
            .anchors
            .iter()
            .map(|anchor| anchor.address.as_str())
            .collect();
        record
            .items
            .iter()
            .filter(|item| {
                important(&item.text)
                    && !mentioned.contains(item.text.as_str())
                    && !anchored.contains(item.address.as_str())
            })
            .map(|item| Gap {
                text: item.text.clone(),
                address: item.address.clone(),
            })
            .collect()
    }
}

/// One thing the record holds, is important by the verifier's test, and the
/// handover does not account for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub text: String,
    /// The record address of the missing item. Empty only when the gap was found
    /// against a record that cannot address it, which the flow never builds.
    pub address: String,
}

/// Whether a record item is long enough for the verifier to call it important.
///
/// One function rather than two copies of the comparison, because the text check
/// and the addressed check have to draw the line in the same place, and two
/// numbers that drift would each look right on their own.
fn important(text: &str) -> bool {
    text.trim().chars().count() >= Verifier::MIN_IMPORTANT_CHARS
}

#[derive(Debug, thiserror::Error)]
pub enum HandoverError {
    #[error("no reader reports: a handover assembled from nothing is not a handover")]
    NoReaders,
    #[error("charter is empty — L0 is injected, never reconstructed, so a handover must be given one")]
    EmptyCharter,
    #[error("handover codec: {0}")]
    Codec(String),
    /// A reader's call failed. Distinct from an empty answer, which is a
    /// finding: this one means there is no answer to read.
    #[error("reader call: {0}")]
    Model(String),
    #[error("record: {0}")]
    Record(#[from] JournalError),
    #[error(
        "this handover does not say which record it indexes, so no session can follow it back"
    )]
    NoOrigin,
    #[error(
        "the record this handover indexes is not here: session `{session}` has no event at seq \
         {at_seq} (its last is {last})"
    )]
    MissingRecord {
        session: String,
        at_seq: u64,
        last: String,
    },
    #[error(
        "a session cannot branch from itself: `{session}` is both the record this handover \
         indexes and the session being started"
    )]
    SameSession { session: String },
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
    /// The session ran out of rounds without reaching a final answer.
    ///
    /// The other way a long task ends, and the one the round ceiling produces:
    /// the session is **over** — its loop has no round left to run — and the
    /// work it left either stops there or is carried by a new session. This is
    /// not a threshold dressed up: nothing about the window decides it, because
    /// a window belongs to a session and this session has ended. (Measured, in
    /// the end-to-end acceptance: the ceiling was reached, the growth backstop
    /// was not, and no handover was written — on the exact run a generation
    /// change was most wanted.)
    CeilingReached { ceiling: u32 },
    /// Backstop, for a stop that is not the ceiling. `growth_since_last` is
    /// measured from the last compaction, never from the start of the run: a
    /// freshly compacted window is large by construction, since the prefix
    /// re-seeds it, so a total-context trigger fires again the moment it
    /// finishes, forever.
    Threshold { growth_since_last: u64, threshold: u64 },
    /// The agent asked for a new window (DESIGN's `new_context`, isomorphic to
    /// `retire`): a request, not a threshold.
    ///
    /// **Nothing produces this yet.** The other two cases are decided from what
    /// a finished run reports about itself ([`trigger_for`]); this one needs a
    /// tool on the decision layer's surface — a `retire`-shaped call that ends
    /// the session — and M5 built the flow the call would fire, not the call.
    /// It is named here rather than omitted because the three triggers are one
    /// decision, and a caller that sees only two would read the third as
    /// unnecessary.
    AgentRequested,
}

impl Trigger {
    pub fn should_fire(&self) -> bool {
        match *self {
            Trigger::NaturalCompletion | Trigger::AgentRequested | Trigger::CeilingReached { .. } => {
                true
            }
            Trigger::Threshold {
                growth_since_last,
                threshold,
            } => growth_since_last >= threshold,
        }
    }

    /// One line a record can carry: why the change happened.
    ///
    /// Written out rather than derived from the debug form, because this text
    /// goes into L2 and is read by whoever audits the change later, and
    /// `Threshold { growth_since_last: 24000, threshold: 20000 }` is not a
    /// sentence.
    pub fn describe(&self) -> String {
        match self {
            Trigger::NaturalCompletion => "the session reached a decision".to_owned(),
            Trigger::CeilingReached { ceiling } => format!(
                "the session reached its round ceiling of {ceiling} without a final answer: the \
                 work is unfinished and this boundary is where it continues"
            ),
            Trigger::Threshold {
                growth_since_last,
                threshold,
            } => format!(
                "the window grew {growth_since_last} characters past where the session started, \
                 over a threshold of {threshold}"
            ),
            Trigger::AgentRequested => "the session asked for a new window".to_owned(),
        }
    }
}

/// Whether a finished session should hand over, and why.
///
/// This is where the trigger meets a run: the session boundary is the only place
/// a contestable decision about a context can be made — mid-run there is no unit
/// of work to have finished, and the flow needs the whole record, which the
/// session is still writing. `growth_threshold` is the declared backstop, in
/// characters, for a stop that is not the ceiling; `None` means no threshold was
/// declared.
///
/// The cases that fire, and the one that cannot:
///
/// * a session that reached a decision finished a unit of work, and DESIGN puts
///   completion first: *"工作自然完成时优先(完成才是合法的收缩)"* — the session
///   knows what no longer matters, while a threshold can only guess;
/// * a session that **ran out its rounds** is over without having finished
///   anything: there is no round left to run, so the work either stops here or a
///   new session carries it. Nothing else conditions this — the round ceiling is
///   not a pressure reading, it is the session's own ending — and the end-to-end
///   acceptance is what showed the difference: the ceiling was reached, no
///   threshold was crossed, and no handover was written on a run whose work was
///   unfinished and worth continuing;
/// * a session that stopped for **any other reason** (a stop this harness does
///   not produce yet — see [`yu_core::decision::StoppedEarly`]) hands over only
///   when its window has grown past the declared threshold, **counted from where
///   this session started** — never from the size of the window itself, which
///   would fire again the moment a contraction re-seeded it (DESIGN's 压缩后立刻重新
///   触发 trap, and the reason [`yu_core::traits::GrowthBudget`] has no
///   total-context field);
/// * [`Trigger::AgentRequested`] has no producer yet — see its own doc.
pub fn trigger_for(outcome: &DecisionOutcome, growth_threshold: Option<u64>) -> Option<Trigger> {
    if outcome.stopped_early.is_none() {
        return Some(Trigger::NaturalCompletion);
    }
    if let Some(StoppedEarly::RoundCeiling { ceiling }) = &outcome.stopped_early {
        return Some(Trigger::CeilingReached { ceiling: *ceiling });
    }
    let threshold = growth_threshold?;
    // Armed at the session's own first view, because that is the last
    // contraction: the window was empty just before it. Every later view is
    // measured against it, so a session that starts large and stays large — the
    // shape of a post-compaction window — does not fire.
    let mut budget = yu_core::traits::GrowthBudget::new(threshold);
    let mut contexts = outcome.contexts.iter();
    if let Some(first) = contexts.next() {
        budget.rearm(first.total_chars);
    }
    for context in contexts {
        budget.observe(context.total_chars);
    }
    let trigger = Trigger::from_growth(&budget);
    trigger.should_fire().then_some(trigger)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(angle: ReaderAngle, findings: &[&str]) -> ReaderReport {
        ReaderReport {
            angle,
            findings: findings.iter().map(|f| (*f).to_owned()).collect(),
            anchors: Vec::new(),
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
                    anchors: vec![Anchor {
                        item: "Chose a Unix socket over HTTP".to_owned(),
                        address: "journal:seq=7".to_owned(),
                    }],
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
        handover.origin = Some(Origin {
            session: SessionId::new("run-7"),
            at_seq: 41,
        });

        let encoded = handover.to_json().expect("a handover must encode");
        let decoded = Handover::from_json(&encoded).expect("a handover must decode");
        assert_eq!(decoded, handover);
        assert!(decoded.check().is_ok());
        assert_eq!(
            decoded.anchors[0].address, "journal:seq=7",
            "an anchor is item and address together: a handover whose addresses lost \
             their items is the flat list the index exists to replace"
        );
        assert_eq!(
            decoded.origin.as_ref().map(|o| o.session.as_str()),
            Some("run-7"),
            "the record a handover indexes has to survive the wire, or no session can \
             follow it back"
        );

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
/// network and without spending anything. The same seam carries the dying
/// session's loose-end call, because that is one more fresh call of the same
/// shape: a prompt in, a list out, no tools.
pub trait ReaderModel: Send + Sync {
    fn read(
        &self,
        prompt: &str,
    ) -> impl std::future::Future<Output = Result<String, HandoverError>> + Send;
}

/// How much a reader or the dying session may answer with.
const READER_MAX_TOKENS: u32 = 2_048;

/// The production reader: one model call, no tools, no history.
pub struct WireReader {
    client: Arc<WireClient>,
}

impl WireReader {
    pub fn new(client: Arc<WireClient>) -> Self {
        Self { client }
    }
}

impl ReaderModel for WireReader {
    async fn read(&self, prompt: &str) -> Result<String, HandoverError> {
        let request = MessagesRequest::new(
            self.client.default_model().to_owned(),
            READER_MAX_TOKENS,
            vec![Message::user(prompt)],
        );
        let mut sink = ReaderSink::default();
        // The reply's prose, not its blocks: a reader reports findings, and its
        // thinking is not its answer. A call that fails is reported as itself
        // rather than as an empty reply — those are different facts about the
        // world, and the second would enter the handover as an angle that found
        // nothing.
        self.client
            .stream(&request, &mut sink)
            .await
            .map_err(|e| HandoverError::Model(e.to_string()))?;
        Ok(sink.text)
    }
}

#[derive(Default)]
struct ReaderSink {
    text: String,
}

impl StreamSink for ReaderSink {
    fn on_text(&mut self, text: &str) {
        self.text.push_str(text);
    }
}

/// The prompt one reader is given.
///
/// DESIGN's claim 1 about the specialised reader is that its cost *is* the size of
/// this prompt — "它只是个读取器,不需要完整的工具纪律". So it is deliberately
/// small: one angle, the record, and an instruction to report findings rather
/// than prose. It carries no tool discipline because it has no tools.
///
/// It asks for the address and for the reason because both are what makes the
/// outcome an index rather than a summary: an address is what the next session
/// follows back to the record, and a decision that arrives without its reason is
/// re-litigated. Neither is enforced here — [`parse_reply`] takes what it can
/// prove from the record and lets the rest through as it came.
pub fn reader_prompt(angle: ReaderAngle, record: &str) -> String {
    format!(
        "You are reading a record to extract one specific kind of thing from it. You have no \
         tools; the record below is everything there is.\n\n\
         Extract, in your own words, the {subject}. Report only what the record actually shows. \
         If it shows nothing under this heading, say so — an absence is a finding, and inventing \
         a plausible entry is worse than returning none.\n\n\
         Begin every finding with the address of the record line it came from, copied exactly, \
         in square brackets. Where the record gives a reason for the thing you are reporting, \
         keep the reason with it, after ` :: ` — a decision that arrives without its reason is \
         re-litigated.\n\n\
         Answer as a list, one finding per line, each beginning with \"- \". Nothing else: no \
         preamble, no summary, no offers to help.\n\n\
         --- RECORD ---\n{record}\n--- END RECORD ---",
        subject = angle.subject(),
    )
}

/// The prompt the dying session is asked its one irreplaceable question with.
///
/// DESIGN: "濒死 session 只做一件不可替代的事:交'未了线头'清单。它知道那些**没被写下来**的
/// 东西". Everything else about the handover can be reconstructed by a reader from
/// the record; this cannot, which is why the session is still asked for it and
/// why the list it returns is marked `recoverable_from_record: false`.
pub fn loose_end_prompt(state: &str) -> String {
    format!(
        "You are the session that is ending. Everything you wrote into the record can be read \
         back by others; nothing else survives you.\n\n\
         Write down only what is NOT in the record: threads you left open, things you decided \
         against and never recorded, hunches you never stated, results you did not write down. \
         Do not organise, and do not restate what the record already holds — a reader can \
         recover that already, and a list of it looks like work while adding none.\n\n\
         One per line, beginning with \"- \", as: what it is :: why it matters. If there is \
         truly nothing, answer exactly: none\n\n\
         --- WHAT YOU HOLD ---\n{state}\n--- END ---",
    )
}

/// The dying session's own state, as the loose-end prompt shows it.
///
/// The parts that make "what did I never write down" answerable: the intent, the
/// constraints, and the state the session built. Deliberately not the record —
/// the record is what the readers get, and showing it here would invite the
/// session to restate it.
fn session_state(charter: &Charter, ledger: &Ledger) -> String {
    let mut out = format!("intent: {}\n", charter.intent());
    let mut section = |heading: &str, lines: Vec<String>| {
        if lines.is_empty() {
            return;
        }
        out.push_str(&format!("{heading}:\n"));
        for line in lines {
            out.push_str(&format!("- {line}\n"));
        }
    };
    section("constraints", charter.constraints.clone());
    section("acceptance", charter.acceptance.clone());
    section(
        "decisions",
        ledger
            .decisions
            .iter()
            .map(|d| format!("{} — because {}", d.what, d.why))
            .collect(),
    );
    section("open questions", ledger.open_questions.clone());
    section(
        "rejected",
        ledger
            .rejected
            .iter()
            .map(|r| format!("{} — because {}", r.what, r.why))
            .collect(),
    );
    out
}

/// The loose ends a dying session's reply carries.
///
/// A line without the ` :: ` separator is still a loose end — the text is what
/// matters — with the reason left empty rather than invented. A reply that
/// ignores the list format is kept whole, the same rule [`parse_reply`] follows:
/// this is the one material nothing else can reconstruct, and dropping it because
/// of punctuation would be the silent omission the whole design is built against.
fn parse_loose_ends(raw: &str) -> Vec<LooseEnd> {
    let mut ends = Vec::new();
    for line in raw.lines() {
        let Some(rest) = line.trim().strip_prefix("- ") else {
            continue;
        };
        match rest.trim().split_once(" :: ") {
            Some((text, why)) => ends.extend(loose_end_from(text, why)),
            None => ends.extend(loose_end_from(rest, "")),
        }
    }
    if ends.is_empty() {
        ends.extend(loose_end_from(raw, ""));
    }
    ends
}

/// One loose end, unless the session said there is nothing to report.
fn loose_end_from(text: &str, why: &str) -> Option<LooseEnd> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("none") {
        return None;
    }
    Some(LooseEnd {
        text: text.to_owned(),
        why_it_matters: why.trim().to_owned(),
        // The prompt asked for what is not in the record, so this is true by
        // construction of the question rather than by inspection of the answer.
        // It is the one claim only this session can make.
        recoverable_from_record: false,
    })
}

/// Run the readers over the record and return what each of them found.
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
///
/// The loose ends are part of the reading on purpose: DESIGN's step 2 is "读记录
/// + 线头", and they are the one kind of material the record cannot contain.
pub async fn dispatch<M: ReaderModel>(
    model: &M,
    record: &Record,
    loose_ends: &[LooseEnd],
    angles: &[ReaderAngle],
) -> Result<Vec<ReaderReport>, HandoverError> {
    if angles.is_empty() {
        return Err(HandoverError::NoReaders);
    }
    let reading = record.reading_text(loose_ends);
    let mut reports = Vec::with_capacity(angles.len());
    for angle in angles {
        let raw = model.read(&reader_prompt(*angle, &reading)).await?;
        let reply = parse_reply(&raw, record);
        reports.push(ReaderReport {
            angle: *angle,
            findings: reply.findings,
            anchors: reply.anchors,
        });
    }
    Ok(reports)
}

/// One reader's reply, split into findings and the anchors that reach them.
#[derive(Debug, Default)]
struct ParsedReply {
    findings: Vec<String>,
    anchors: Vec<Anchor>,
}

/// Turn a reader's reply into findings and anchors.
///
/// Lines beginning `- ` are findings, because that is what the prompt asks for.
/// A reply that ignores the format is kept whole rather than discarded: a reader
/// that answered in prose still read the record, and throwing its work away
/// would be the silent omission this whole design is built against.
///
/// **An address a reader writes is checked, not trusted.** The prompt asks for
/// the record line's address in square brackets, and a model asked to copy a
/// 64-character digest will sometimes copy it wrong. So a claimed address is
/// kept only when the record has it; otherwise the finding is anchored by
/// quoting — the longest record item the finding contains, when that item is
/// long enough for the match to mean anything. A finding that can be anchored
/// neither way is carried without one: the finding is still what a reader saw,
/// and saying "I do not know where this came from" is honest.
fn parse_reply(raw: &str, record: &Record) -> ParsedReply {
    let mut parsed = ParsedReply::default();
    for line in raw.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            continue;
        }
        let (claimed, finding) = match rest.strip_prefix('[') {
            Some(tail) => match tail.split_once(']') {
                Some((claimed, finding)) => (Some(claimed.trim()), finding.trim()),
                // An unclosed bracket is not a prefix this format knows; the
                // line is kept as it came rather than half-eaten.
                None => (None, rest),
            },
            None => (None, rest),
        };
        if finding.is_empty() {
            continue;
        }
        let address = claimed
            .filter(|claimed| record.knows_address(claimed))
            .map(str::to_owned)
            .or_else(|| record.address_quoting(finding));
        if let Some(address) = address {
            parsed.anchors.push(Anchor {
                item: finding.to_owned(),
                address,
            });
        }
        parsed.findings.push(finding.to_owned());
    }
    if parsed.findings.is_empty() {
        let whole = raw.trim();
        if !whole.is_empty() {
            parsed.findings.push(whole.to_owned());
        }
    }
    parsed
}

/// Merge the dying session's own loose ends into what the readers reported.
///
/// Exact text matches, the same rule [`assemble`] uses: merging near-duplicates
/// would be a judgement about whether two lines say the same thing, and this step
/// does not make judgements. Where both carry the same text the session's entry
/// wins, because it has a reason and because it is the only source that can say
/// the item was never written down — and because a reader echoing it may simply
/// have read it from the loose-end list it was handed.
fn merge_loose_ends(into: &mut Vec<LooseEnd>, session: Vec<LooseEnd>) {
    for end in session {
        match into.iter_mut().find(|existing| existing.text == end.text) {
            Some(existing) => {
                if !end.why_it_matters.is_empty() {
                    existing.why_it_matters = end.why_it_matters;
                }
                existing.recoverable_from_record = false;
            }
            None => into.push(end),
        }
    }
}

// ---------------------------------------------------------------------------
// The change, as a run makes it
// ---------------------------------------------------------------------------

/// What one generation change produced.
#[derive(Debug, Clone)]
pub struct Change {
    /// Why it happened — the same value [`trigger_for`] decided, carried so the
    /// outcome can be read without reconstructing the session's ending.
    pub trigger: Trigger,
    pub handover: Handover,
    /// What the verifier found in the record and not in the handover. Recorded
    /// as an event and handed to the caller, because a gap that is noticed and
    /// then dropped is worse than one that was never noticed.
    pub gaps: Vec<Gap>,
    /// The address of the handover itself in the record's store — the index of
    /// the index, for whatever reads the record next.
    pub handover_at: String,
}

/// DESIGN's four steps, as one call: loose ends, readers, union, verification.
///
/// The run that calls this is the session that is ending; the artefact it returns
/// is what the session that starts next is given. Everything it produces is
/// recorded: the handover goes into the record's store at an address
/// ([`Change::handover_at`]), and the change and each gap it found go in as
/// events, because a generation change nobody can find in the record is a
/// generation change nobody can audit.
///
/// The record is read **once**, at the top, and every step works from that one
/// reading — the readers, the anchors and the verifier all see the same bytes.
/// A second read could see events the session appended while the readers were
/// thinking, and the verifier would then be checking a handover against a record
/// its readers never had.
pub async fn change_generation<M: ReaderModel>(
    model: &M,
    journal: &Journal,
    trigger: Trigger,
    charter: &Charter,
    ledger: &Ledger,
    angles: &[ReaderAngle],
) -> Result<Change, HandoverError> {
    let record = Record::read(journal).await?;

    // Step 1 — the one thing only the dying session can produce.
    let loose = parse_loose_ends(
        &model
            .read(&loose_end_prompt(&session_state(charter, ledger)))
            .await?,
    );

    // Step 2 — k fresh readers, each with the record plus those loose ends.
    let reports = dispatch(model, &record, &loose, angles).await?;

    // Step 3 — the union, plus the two things assembly cannot know: where the
    // record is, and which loose ends the session itself was sure about.
    let mut handover = assemble_with_charter(charter.intent(), reports);
    handover.origin = Some(record.origin());
    merge_loose_ends(&mut handover.loose_ends, loose);

    // Step 4 — back to the record, not to the reports.
    let gaps = Verifier::find_gaps_in(&record, &handover);

    let handover_at = handover.store(journal)?;
    journal
        .append(
            AgentId::new("harness"),
            EventBody::Note {
                text: format!(
                    "generation change ({}): {} angle(s), {} item(s) carried, {} gap(s) found \
                     against the record; handover at {handover_at}",
                    trigger.describe(),
                    handover.sources.len(),
                    handover.carried_len(),
                    gaps.len(),
                ),
            },
        )
        .await?;
    if !gaps.is_empty() {
        // Each gap with its address, so the event is actionable rather than
        // merely alarming: whoever reads the record next can look.
        let listed = gaps
            .iter()
            .map(|gap| format!("{} [{}]", gap.text, gap.address))
            .collect::<Vec<_>>()
            .join(" | ");
        journal
            .append(
                AgentId::new("harness"),
                EventBody::Note {
                    text: format!(
                        "generation change: {} item(s) the record holds, that the verifier judged \
                         important, and that the handover does not carry: {listed}",
                        gaps.len()
                    ),
                },
            )
            .await?;
    }

    Ok(Change {
        trigger,
        handover,
        gaps,
        handover_at,
    })
}

// ---------------------------------------------------------------------------
// What a fresh session starts from
// ---------------------------------------------------------------------------

/// A new session's starting state: L0 and L1, built from a handover.
///
/// The third thing a session needs — the index itself, as a manifest entry — is
/// [`Handover::carried_entry`], because it needs the address the handover was
/// *stored* at, which only the caller that stored it has.
#[derive(Debug, Clone)]
pub struct NewSession {
    pub charter: Charter,
    pub ledger: Ledger,
}

impl Handover {
    /// How much state this carries, counted the way the note in L2 counts it.
    pub fn carried_len(&self) -> usize {
        self.decisions.len()
            + self.constraints.len()
            + self.negative_evidence.len()
            + self.artifact_index.len()
            + self.loose_ends.len()
    }

    /// Store the handover in the record's blob store, and say where.
    ///
    /// Content-addressed, so this is a reference rather than a second copy: the
    /// same handover stored twice costs one payload, and an address outlives the
    /// process that wrote it.
    pub fn store(&self, journal: &Journal) -> Result<String, HandoverError> {
        let digest = journal.blobs().put_str(&self.to_json()?)?;
        Ok(format!("blob:{digest}"))
    }

    /// The manifest entry a new session carries for the handover.
    ///
    /// `at` is the address [`Handover::store`] returned, and `bytes` its size in
    /// the store: a manifest entry without a size makes the reader guess what
    /// following it costs, and this is the one entry whose cost is knowable.
    pub fn carried_entry(&self, at: &str, bytes: u64) -> NotLoadedEntry {
        NotLoadedEntry {
            what: format!(
                "the previous generation's handover ({} item(s), from {} angle(s))",
                self.carried_len(),
                self.sources.len()
            ),
            address: at.to_owned(),
            bytes,
            why: "state carried across a generation change: the ledger and the charter are \
                  its state half, this is the index half"
                .to_owned(),
            how_to_load: "spot_check it, then follow the address it gives for the item you need"
                .to_owned(),
        }
    }

    /// Build the fresh session's L0 and L1 from this handover.
    ///
    /// `base` is the new session's own charter — the human's words for *this*
    /// session, which a handover never replaces. What it adds to is the rest:
    ///
    /// * the previous session's intent, verbatim, as a human utterance. DESIGN:
    ///   "人类原话原样存活"; carried is not reconstructed, and the intent is the
    ///   one thing that must never be guessed;
    /// * the constraints, into L0 where DESIGN puts them — which is also what
    ///   makes them *injected*: `DecisionLayer` passes `charter.constraints` into
    ///   every delegation's [`yu_core::delegate::Standing`], so a constraint that
    ///   arrives this way reaches the new session's executors' system blocks.
    ///   That is the difference between carrying a constraint and remembering one;
    /// * decisions and negative evidence, into L1 — the two lists whose whole
    ///   purpose is to stop a settled question from coming back;
    /// * loose ends, as open questions: a thread the dying session knew was open
    ///   is exactly what "待决问题" is for.
    ///
    /// Every carried L1 entry that has an anchor names it, so the state points at
    /// the material rather than standing in for it.
    pub fn new_session(&self, base: &Charter) -> NewSession {
        let mut charter = base.clone();
        if !self.charter.trim().is_empty()
            && !charter
                .human_words
                .iter()
                .any(|already| already.text == self.charter)
        {
            charter.human_words.push(HumanUtterance {
                at_ms: yu_observe::now_ms(),
                text: self.charter.clone(),
                source: "carried by the handover from the previous session".to_owned(),
            });
        }
        for constraint in &self.constraints {
            if !charter.constraints.iter().any(|held| held == constraint) {
                charter.constraints.push(constraint.clone());
            }
        }

        let mut ledger = Ledger::default();
        for item in &self.decisions {
            let (what, why) = self.reason_of(item);
            ledger.decisions.push(Decision {
                what,
                why,
                at_ms: yu_observe::now_ms(),
            });
        }
        for item in &self.negative_evidence {
            let (what, why) = self.reason_of(item);
            ledger.rejected.push(Rejected { what, why });
        }
        for end in &self.loose_ends {
            ledger.open_questions.push(end.text.clone());
        }

        NewSession { charter, ledger }
    }

    /// Split a carried finding into its decision and its reason.
    ///
    /// The reader was asked for `what :: why`; when it gave one, that is the
    /// entry. When it did not, the reason says where the decision stands instead
    /// of being blank — `render_l1` writes "- {what} — because {why}", and a
    /// blank there reads as a decision with no reason rather than as a reason
    /// that did not travel.
    fn reason_of(&self, item: &str) -> (String, String) {
        let (what, mut why) = match item.split_once(" :: ") {
            Some((what, why)) => (what.trim().to_owned(), why.trim().to_owned()),
            None => (item.trim().to_owned(), String::new()),
        };
        if why.is_empty() {
            why.push_str("carried from the previous generation's record");
        }
        if let Some(anchor) = self.anchors.iter().find(|anchor| anchor.item == item) {
            why.push_str(&format!("; the item is at {}", anchor.address));
        }
        (what, why)
    }

    /// Open the record this handover indexes as a new session's own journal.
    ///
    /// A branch, not a copy. `journal:seq=<n>` names a position in *one*
    /// lineage's log, so a new session that started its own log from zero would
    /// make every one of those addresses mean something else — silently, which
    /// is the worst way for an index to be wrong. Continuing the lineage gives
    /// every point in it exactly one address, at O(1) storage: the fork marker is
    /// the whole cost.
    ///
    /// The parent's record has to be here. A handover whose record is missing is
    /// refused as itself rather than silently resumed against a log that would
    /// answer its addresses with the wrong events.
    pub async fn resume_into(
        &self,
        dir: &Path,
        session: SessionId,
    ) -> Result<Journal, HandoverError> {
        let Some(origin) = &self.origin else {
            return Err(HandoverError::NoOrigin);
        };
        // Branching onto the same session is not a branch: the marker would go
        // into the very log it claims to come from, and the record would then
        // say the session was its own ancestor. Measured — the CLI's session ids
        // have second resolution, so two runs inside one second collide here,
        // and without this check the failure is silent.
        if origin.session == session {
            return Err(HandoverError::SameSession {
                session: session.to_string(),
            });
        }
        let parent = Arc::new(Journal::open(dir, origin.session.clone()).await?);
        let prefix = parent.history_prefix(origin.at_seq).await?;
        if prefix.last().map(|event| event.seq + 1) != Some(origin.at_seq) {
            return Err(HandoverError::MissingRecord {
                session: origin.session.to_string(),
                at_seq: origin.at_seq,
                last: prefix
                    .last()
                    .map(|event| event.seq.to_string())
                    .unwrap_or_else(|| "nothing".to_owned()),
            });
        }
        Ok(Journal::open_fork(dir, session, parent, origin.at_seq).await?)
    }
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

    /// A record fixture: address and text, in that order.
    fn record(items: &[(&str, &str)]) -> Record {
        Record::of(
            SessionId::new("s"),
            items.len() as u64,
            items
                .iter()
                .map(|(address, text)| RecordItem {
                    address: (*address).to_owned(),
                    text: (*text).to_owned(),
                })
                .collect(),
        )
    }

    #[tokio::test]
    async fn every_angle_is_dispatched_and_its_findings_survive() {
        const LINE: &str = "the parser was rewritten because the old one was slow";
        let record = record(&[("journal:seq=7", LINE)]);
        let model = Scripted::new(&format!("- [journal:seq=7] {LINE}"));
        let reports = dispatch(
            &model,
            &record,
            &[],
            &[ReaderAngle::DecisionsAndReasons, ReaderAngle::LooseEnds],
        )
        .await
        .unwrap();

        assert_eq!(model.seen.lock().unwrap().len(), 2, "one reader per angle");
        let handover = assemble_with_charter("ship it", reports);
        assert_eq!(
            handover.sources,
            vec![ReaderAngle::DecisionsAndReasons, ReaderAngle::LooseEnds]
        );
        assert!(
            handover.decisions.iter().any(|found| found == LINE),
            "the decisions angle's finding must reach the handover: {handover:?}"
        );
        assert_eq!(
            handover.anchors,
            vec![Anchor {
                item: LINE.to_owned(),
                address: "journal:seq=7".to_owned(),
            }],
            "and it must arrive anchored to the line it was lifted from: {handover:?}"
        );
    }

    #[tokio::test]
    async fn each_reader_is_asked_for_its_own_angle() {
        let record = record(&[("journal:seq=1", "the migration was reverted")]);
        let model = Scripted::new("- x");
        dispatch(&model, &record, &[], &[ReaderAngle::NegativeEvidence])
            .await
            .unwrap();
        let prompts = model.seen.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(
            prompts[0].contains(ReaderAngle::NegativeEvidence.subject()),
            "a reader must be asked for its angle and not a generic summary: {}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("the migration was reverted")
                && prompts[0].contains("journal:seq=1"),
            "a reader is given the record, with its addresses: {}",
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
        assert!(
            loose_end_prompt("").len() < 1200,
            "the dying session's one question is a reader-sized call too: {}",
            loose_end_prompt("").len()
        );
    }

    #[tokio::test]
    async fn a_reader_that_ignores_the_format_is_not_discarded() {
        // Malformed output still represents work done on the record. Dropping it
        // would be exactly the silent omission the design exists to prevent.
        let model = Scripted::new("Nothing under this heading, as far as I can see.");
        let reports = dispatch(&model, &record(&[]), &[], &[ReaderAngle::ArtifactIndex])
            .await
            .unwrap();
        let handover = assemble_with_charter("ship it", reports);
        assert!(
            handover
                .artifact_index
                .iter()
                .any(|found| found.contains("Nothing under this heading")),
            "the reader's account must be carried even when it does not match the format: \
             {handover:?}"
        );
    }

    /// A reader that did not copy an address is still anchored by what it quoted.
    #[tokio::test]
    async fn an_address_a_reader_did_not_copy_is_recovered_from_the_quote() {
        const LINE: &str = "the socket path is read from a stale config on restart";
        let record = record(&[("journal:seq=9", LINE)]);
        let model = Scripted::new(&format!("- {LINE}, so restarts pick up the old path"));
        let reports = dispatch(&model, &record, &[], &[ReaderAngle::ConstraintsAndInvariants])
            .await
            .unwrap();
        assert_eq!(
            reports[0].anchors,
            vec![Anchor {
                item: format!("{LINE}, so restarts pick up the old path"),
                address: "journal:seq=9".to_owned(),
            }],
            "a verbatim quote is a way back to the record even without the address"
        );
    }

    /// An address the record does not have is not believed.
    #[tokio::test]
    async fn an_address_the_record_does_not_have_is_not_believed() {
        let record = record(&[("journal:seq=9", "the build failed on the release branch")]);
        let model = Scripted::new("- [journal:seq=900] the build failed on the release branch");
        let reports = dispatch(&model, &record, &[], &[ReaderAngle::NegativeEvidence])
            .await
            .unwrap();
        assert_eq!(
            reports[0].anchors,
            vec![Anchor {
                item: "the build failed on the release branch".to_owned(),
                address: "journal:seq=9".to_owned(),
            }],
            "a claimed address the record cannot answer is dropped, and the quote decides: \
             {:#?}",
            reports[0].anchors
        );
    }

    #[test]
    fn the_record_renders_each_item_with_the_address_that_reaches_it() {
        let record = record(&[
            ("journal:seq=0", "ship the daemon"),
            ("blob:abc", "the reply the executor gave"),
        ]);
        let rendered = record.render();
        assert!(rendered.contains("[journal:seq=0] ship the daemon"), "{rendered}");
        assert!(rendered.contains("[blob:abc] the reply the executor gave"), "{rendered}");

        // The loose ends are part of the reading, and are labelled as not being
        // in the record — a reader that took them for record lines would anchor
        // findings to addresses that do not exist.
        let with_loose = record.reading_text(&[LooseEnd {
            text: "the retry budget was raised mid-run".to_owned(),
            why_it_matters: "the passing test proves less than it looks like".to_owned(),
            recoverable_from_record: false,
        }]);
        assert!(with_loose.contains("the retry budget was raised mid-run"));
        assert!(
            with_loose.contains("NOT in the record"),
            "the loose ends have to be marked as outside the record: {with_loose}"
        );
        assert!(
            !with_loose.contains("the retry budget was raised mid-run [")
                && !with_loose.contains("] the retry budget was raised mid-run"),
            "a loose end is never given an address, because there is none: {with_loose}"
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
        let err = dispatch(&model, &record(&[]), &[], &[]).await.unwrap_err();
        assert!(matches!(err, HandoverError::NoReaders));
    }

    // ---- the dying session's own contribution ------------------------------

    #[test]
    fn the_sessions_loose_ends_keep_their_reason_and_are_never_recoverable() {
        let ends = parse_loose_ends(
            "Here is what I did not write down:\n\
             - the daemon was tested against a stale socket path :: the passing test proves \
             less than it looks like\n\
             - the retry budget was raised mid-run\n\
             none",
        );
        assert_eq!(ends.len(), 2, "{ends:#?}");
        assert_eq!(ends[0].text, "the daemon was tested against a stale socket path");
        assert_eq!(ends[0].why_it_matters, "the passing test proves less than it looks like");
        assert!(
            !ends[0].recoverable_from_record && !ends[1].recoverable_from_record,
            "the prompt asked for what is not in the record, which is the one claim only \
             this session can make"
        );
        assert_eq!(ends[1].why_it_matters, "", "a reason nobody gave is not invented");

        assert!(
            parse_loose_ends("none").is_empty(),
            "a session with nothing to say must be able to say so"
        );
        // A reply that ignores the list format is kept whole rather than
        // dropped: this is the one material nothing else can reconstruct.
        let prose = parse_loose_ends("The socket path was stale in the test I ran at 14:20.");
        assert_eq!(prose.len(), 1, "{prose:#?}");
        assert!(prose[0].text.contains("14:20"), "{prose:#?}");
    }

    #[test]
    fn a_loose_end_the_readers_also_reported_keeps_the_sessions_reason() {
        let mut assembled = vec![LooseEnd {
            text: "the retry budget was raised mid-run".to_owned(),
            why_it_matters: String::new(),
            recoverable_from_record: true,
        }];
        merge_loose_ends(
            &mut assembled,
            vec![LooseEnd {
                text: "the retry budget was raised mid-run".to_owned(),
                why_it_matters: "the next session will hit the same rate limit".to_owned(),
                recoverable_from_record: false,
            }],
        );
        assert_eq!(assembled.len(), 1, "one entry, not two: {assembled:#?}");
        assert_eq!(assembled[0].why_it_matters, "the next session will hit the same rate limit");
        assert!(!assembled[0].recoverable_from_record);
    }

    // ---- when a change happens ---------------------------------------------

    /// A finished session, as the layer reports it.
    fn outcome(stopped_early: Option<StoppedEarly>, contexts: &[u64]) -> DecisionOutcome {
        DecisionOutcome {
            rounds: 1,
            delegations: 0,
            final_text: String::new(),
            usage: yu_observe::TokenUsage::default(),
            composition: yu_observe::ContextComposition::default(),
            contexts: contexts
                .iter()
                .map(|chars| yu_observe::ContextComposition {
                    total_chars: *chars,
                    ..Default::default()
                })
                .collect(),
            ledger: Ledger::default(),
            stopped_early,
            outcomes: Vec::new(),
        }
    }

    /// **The end-to-end acceptance, as a test.**
    ///
    /// A run that reached its round ceiling hands over — whether or not its
    /// window grew and whether or not any threshold was declared. This is the
    /// case the acceptance failed on: the session stopped at the ceiling, the
    /// growth backstop was not crossed, and nothing was written at a boundary
    /// where the work was unfinished and a continuation was wanted. The test
    /// fails if the ceiling arm is reverted to the threshold-only rule, and the
    /// `None` threshold case fails if the ceiling is made conditional on a
    /// declared threshold.
    ///
    /// The case this replaced asserted the opposite for a stopped session — a
    /// large window that had not grown was "not pressure", so no handover. That
    /// was true of the threshold rule and is not true of the session being over:
    /// the window belongs to a session that has ended. The growth rule itself is
    /// not gone — it is the backstop for a stop this harness does not produce
    /// yet, and its subtraction is still guarded by
    /// [`the_growth_rule_has_one_implementation`].
    #[test]
    fn a_session_that_decided_hands_over_and_so_does_one_that_ran_out_of_rounds() {
        let decided = trigger_for(&outcome(None, &[10_000, 12_000]), Some(5_000));
        assert_eq!(
            decided,
            Some(Trigger::NaturalCompletion),
            "completion is the legitimate contraction, whatever the window did"
        );

        // The ceiling, with a window that never grew and a threshold that was
        // never approached: still fires, because the session is over.
        let ceiling = StoppedEarly::RoundCeiling { ceiling: 5 };
        let quiet = trigger_for(
            &outcome(Some(ceiling.clone()), &[9_000_000, 9_001_000]),
            Some(5_000),
        );
        assert_eq!(
            quiet,
            Some(Trigger::CeilingReached { ceiling: 5 }),
            "a session that ran out of rounds is over; its work either stops here or is \
             carried, and nothing about the window changes that"
        );

        // And with no threshold declared at all — the shape the acceptance ran
        // in — the ceiling still hands over.
        assert_eq!(
            trigger_for(
                &outcome(Some(ceiling.clone()), &[10_000, 90_000]),
                None
            ),
            Some(Trigger::CeilingReached { ceiling: 5 })
        );

        // Growing past a declared threshold does not change which trigger it is:
        // the stop is what fired, not the growth.
        assert_eq!(
            trigger_for(&outcome(Some(ceiling), &[10_000, 16_000]), Some(5_000)),
            Some(Trigger::CeilingReached { ceiling: 5 })
        );
    }
}

/// The flow over a real record.
///
/// These drive [`change_generation`] against a journal the way a run would: the
/// record is L2's own events, the readers answer from a script (their judgement
/// is not testable, their plumbing is), and the assertions are about what the
/// flow does with what comes back.
#[cfg(test)]
mod change_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use yu_core::spot::resolve;
    use yu_observe::TokenUsage;

    const CHARTER: &str = "ship the daemon";

    /// An item the readers can carry, and one they were scripted to walk past.
    const CARRIED: &str = "the daemon owns sessions; the CLI only attaches";
    const PLANTED: &str = "resume was never wired to the socket, so a crash loses the session";

    /// A scratch directory this test owns, emptied first.
    async fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yu-workflow-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = tokio::fs::create_dir_all(&dir).await;
        dir
    }

    fn charter() -> Charter {
        Charter::from_the_humans_own_words(CHARTER, 1_700_000_000_000, "test")
    }

    /// The record used by the verifier tests: the charter, one item the readers
    /// will carry, and one planted item they will not. Returns the planted
    /// item's seq so the test can name its address instead of guessing it.
    async fn planted_record(name: &str) -> (PathBuf, Arc<Journal>, u64) {
        let dir = scratch(name).await;
        let journal = Arc::new(Journal::open(&dir, SessionId::new("dying")).await.unwrap());
        journal
            .append(
                AgentId::decision(),
                EventBody::SessionStarted {
                    charter: CHARTER.to_owned(),
                    model: "m".to_owned(),
                    quirks: "q".to_owned(),
                },
            )
            .await
            .unwrap();
        journal
            .append(
                AgentId::decision(),
                EventBody::Note {
                    text: CARRIED.to_owned(),
                },
            )
            .await
            .unwrap();
        let planted = journal
            .append(
                AgentId::decision(),
                EventBody::Note {
                    text: PLANTED.to_owned(),
                },
            )
            .await
            .unwrap();
        (dir, journal, planted.seq)
    }

    /// A model that answers per angle, and answers the dying session's own
    /// question too — the same one call of the same shape.
    struct AngleScript {
        loose: String,
        per_angle: BTreeMap<ReaderAngle, String>,
        seen: Mutex<Vec<String>>,
    }

    impl AngleScript {
        fn new(loose: &str) -> Self {
            Self {
                loose: loose.to_owned(),
                per_angle: BTreeMap::new(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn answering(mut self, angle: ReaderAngle, reply: &str) -> Self {
            self.per_angle.insert(angle, reply.to_owned());
            self
        }
    }

    impl ReaderModel for AngleScript {
        async fn read(&self, prompt: &str) -> Result<String, HandoverError> {
            self.seen.lock().unwrap().push(prompt.to_owned());
            if prompt.contains("You are the session that is ending") {
                return Ok(self.loose.clone());
            }
            for (angle, reply) in &self.per_angle {
                if prompt.contains(angle.subject()) {
                    return Ok(reply.clone());
                }
            }
            // An angle that found nothing says nothing: the absence is recorded
            // by the angle having reported at all.
            Ok(String::new())
        }
    }

    /// **M5: "验证者能在一个刻意构造的漏项里把漏掉的东西找出来."**
    ///
    /// The record holds two important items. The readers carry one of them — in
    /// their own words, anchored to the line it came from — and never mention
    /// the other. The verifier, reading the same record rather than the reports,
    /// has to name the omission with the address that reaches it, and must *not*
    /// report the carried one: a check that calls everything a gap would pass
    /// the first assertion while being worthless.
    ///
    /// The carried finding is a paraphrase on purpose. The record's own words
    /// are nowhere in the handover, so it can only be covered by the anchor
    /// pointing at that line — which is the rule that makes this check
    /// discriminating rather than a list of the whole record.
    #[tokio::test]
    async fn the_verifier_finds_the_omission_the_readers_missed() {
        let (dir, journal, planted_seq) = planted_record("omission").await;
        let carried_seq = planted_seq - 1;
        const EXTRACTED: &str = "the daemon owns the sessions, and the CLI only attaches to it";
        let model = AngleScript::new("none").answering(
            ReaderAngle::ConstraintsAndInvariants,
            &format!("- [journal:seq={carried_seq}] {EXTRACTED}"),
        );

        let change = change_generation(
            &model,
            &journal,
            Trigger::NaturalCompletion,
            &charter(),
            &Ledger::default(),
            &ReaderAngle::ALL,
        )
        .await
        .expect("the flow runs over a real record");

        let gaps: Vec<&str> = change.gaps.iter().map(|gap| gap.text.as_str()).collect();
        assert!(
            gaps.contains(&PLANTED),
            "the planted omission was not found by the verifier: {gaps:#?}"
        );
        assert!(
            !gaps.contains(&CARRIED),
            "the carried item was reported as a gap, so this check is not distinguishing \
             anything: {gaps:#?}"
        );
        assert_eq!(
            change.gaps[0].address,
            format!("journal:seq={planted_seq}"),
            "a gap without the address that reaches it is a finding nobody can act on"
        );

        // The finding is an event in the record, not just a return value: a gap
        // that is noticed and then dropped is worse than one never noticed.
        let notes: Vec<String> = journal
            .events()
            .await
            .unwrap()
            .iter()
            .filter_map(|event| match &event.body {
                EventBody::Note { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            notes.iter().any(|text| text.contains(PLANTED) && text.contains(&format!("journal:seq={planted_seq}"))),
            "the omission has to reach the record with its address: {notes:#?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **M5 criterion 4: "把上一级的报告改坏,验证结论不变."**
    ///
    /// The same record is verified three times, and the only thing that changes
    /// between the runs is what the *readers* said — which is the previous
    /// level's report, and the material a verifier written the obvious way would
    /// read:
    ///
    /// * honest readers carry one item and miss the other;
    /// * a confident fabrication claims the planted item was verified and fixed,
    ///   and asserts something the record does not contain at all;
    /// * the reports deleted — the readers report nothing.
    ///
    /// The conclusion has to survive all three. A verifier that compared the
    /// handover against the reports would see the fabrication as coverage and
    /// drop the gap; a verifier whose *record* was the reports would find
    /// nothing missing, because a union cannot omit what it was built from. Both
    /// failures are exactly "继承同一份压缩损失", and both are caught below.
    #[tokio::test]
    async fn the_verifiers_conclusion_survives_a_corrupted_report() {
        async fn run(name: &str, model: &AngleScript) -> Vec<Gap> {
            let (dir, journal, _) = planted_record(name).await;
            let change = change_generation(
                model,
                &journal,
                Trigger::NaturalCompletion,
                &charter(),
                &Ledger::default(),
                &ReaderAngle::ALL,
            )
            .await
            .expect("the flow runs");
            let _ = std::fs::remove_dir_all(&dir);
            change.gaps
        }

        let honest = run(
            "verifier-honest",
            &AngleScript::new("none").answering(
                ReaderAngle::ConstraintsAndInvariants,
                &format!("- [journal:seq=1] {CARRIED}"),
            ),
        )
        .await;

        // The parent's report, replaced by a confident fabrication: it says the
        // planted item is handled, and it claims something the record never said.
        let fabricated = run(
            "verifier-fabricated",
            &AngleScript::new("the socket story is handled, nothing to carry")
                .answering(
                    ReaderAngle::ConstraintsAndInvariants,
                    &format!(
                        "- resume was fully wired and tested this session :: verified, no \
                         further action needed\n\
                         - [journal:seq=1] {CARRIED}\n\
                         - the migration to the new config loader was completed and reviewed"
                    ),
                )
                .answering(ReaderAngle::DecisionsAndReasons, "- the migration is done"),
        )
        .await;

        // And with the reports deleted outright.
        let deleted = run("verifier-deleted", &AngleScript::new("none")).await;

        let texts = |gaps: &[Gap]| gaps.iter().map(|g| g.text.clone()).collect::<Vec<_>>();
        assert_eq!(
            texts(&honest),
            texts(&fabricated),
            "corrupting the report changed the verifier's conclusion: a verifier that reads \
             the report it is checking is not verifying anything"
        );
        assert!(
            texts(&fabricated).contains(&PLANTED.to_owned()),
            "the fabrication talked the verifier out of a real omission: {fabricated:#?}"
        );
        assert!(
            texts(&deleted).contains(&PLANTED.to_owned()),
            "with the reports gone the omission has to be found against the record itself, \
             or the record was never what was being checked: {deleted:#?}"
        );
    }

    /// The hole the anchor rule admits, pinned rather than left to be discovered.
    ///
    /// An anchor is the handover's claim that a record line was read, and this
    /// check takes that claim: it is a fact about the handover (the address is
    /// one the record answers), not prose the verifier has to trust. So a reader
    /// that points at a line and lies about what it says suppresses the gap —
    /// which the fabrication without an address above cannot do.
    ///
    /// This is the price of the rule that makes the check discriminating at all
    /// (see [`Verifier::find_gaps_in`]), and it is a *content* judgement, which
    /// is the part the doc says belongs to a verifier with a model. The test
    /// exists so that the limit is a known one.
    #[tokio::test]
    async fn an_anchor_is_a_claim_this_check_takes() {
        let (dir, journal, planted_seq) = planted_record("verifier-anchored-lie").await;
        let model = AngleScript::new("none").answering(
            ReaderAngle::ConstraintsAndInvariants,
            &format!(
                "- [journal:seq={planted_seq}] the resume path was verified end to end and \
                 needs no further work"
            ),
        );
        let change = change_generation(
            &model,
            &journal,
            Trigger::NaturalCompletion,
            &charter(),
            &Ledger::default(),
            &ReaderAngle::ALL,
        )
        .await
        .expect("the flow runs");

        // The address is real — the fabrication points at the line it lies
        // about, which is what makes this a different attack from the one above.
        assert!(
            change
                .handover
                .anchors
                .iter()
                .any(|anchor| anchor.address == format!("journal:seq={planted_seq}")),
            "{:#?}",
            change.handover.anchors
        );
        assert!(
            !change.gaps.iter().any(|gap| gap.text == PLANTED),
            "the anchored lie is caught, so this test's description of the limit is stale: \
             {:#?}",
            change.gaps
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **M5 criterion 2: "新会话能顺着它回到记录."**
    ///
    /// Three things, in the order a new session needs them:
    ///
    /// 1. every anchor in the handover resolves — through `spot_check`'s own
    ///    resolver, on the record the handover came from — to the text it was
    ///    lifted from;
    /// 2. the handover records the point in the record it indexes, and a new
    ///    session resumes *as a branch of that record*: the same addresses
    ///    resolve in its journal, because the lineage carries them;
    /// 3. the new session's L0/L1 carry the state, and its manifest names the
    ///    handover itself, so the index is in front of the session that has to
    ///    use it rather than only in a file.
    #[tokio::test]
    async fn a_new_session_can_follow_the_handover_back_to_the_record() {
        const DECISION: &str = "the socket path is read from a stale config on restart";
        const CONSTRAINT: &str = "the launcher must keep working when the socket is absent";
        const REPORT: &str = "the executor reported: the launcher reads its path once";
        let dir = scratch("follow-the-index").await;
        let journal = Arc::new(Journal::open(&dir, SessionId::new("dying")).await.unwrap());
        journal
            .append(
                AgentId::decision(),
                EventBody::SessionStarted {
                    charter: CHARTER.to_owned(),
                    model: "m".to_owned(),
                    quirks: "q".to_owned(),
                },
            )
            .await
            .unwrap();
        let note = journal
            .append(
                AgentId::decision(),
                EventBody::Note {
                    text: DECISION.to_owned(),
                },
            )
            .await
            .unwrap();
        let rule = journal
            .append(
                AgentId::decision(),
                EventBody::Note {
                    text: CONSTRAINT.to_owned(),
                },
            )
            .await
            .unwrap();
        let digest = journal.blobs().put_str(REPORT).unwrap();
        journal
            .append(
                AgentId::new("executor#1"),
                EventBody::ModelResponse {
                    id: "msg-1".to_owned(),
                    stop_reason: "end_turn".to_owned(),
                    usage: TokenUsage::default(),
                    blocks: vec!["text".to_owned()],
                    thinking: Vec::new(),
                    text_blob: Some(digest.clone()),
                },
            )
            .await
            .unwrap();

        let model = AngleScript::new("the launcher's path is cached at start-up")
            .answering(
                ReaderAngle::DecisionsAndReasons,
                &format!("- [journal:seq={}] {DECISION}", note.seq),
            )
            .answering(
                ReaderAngle::ConstraintsAndInvariants,
                &format!("- [journal:seq={}] {CONSTRAINT}", rule.seq),
            )
            .answering(
                ReaderAngle::ArtifactIndex,
                &format!("- [blob:{digest}] {REPORT}"),
            );

        let change = change_generation(
            &model,
            &journal,
            Trigger::NaturalCompletion,
            &charter(),
            &Ledger::default(),
            &ReaderAngle::ALL,
        )
        .await
        .expect("the flow runs");

        // 1. Every address the handover carries answers with the material it
        //    names — through the same resolver `spot_check` dispatches to, so
        //    this is the door a session would actually knock on.
        assert_eq!(change.handover.anchors.len(), 3, "{:#?}", change.handover.anchors);
        for anchor in &change.handover.anchors {
            let spotted = resolve(&anchor.address, &journal).await.unwrap_or_else(|e| {
                panic!(
                    "`{}` is in the handover's index and the record will not answer it: {e}",
                    anchor.address
                )
            });
            assert!(
                spotted.text.contains(&anchor.item),
                "`{}` answered with something other than the item it is anchored to: {}",
                anchor.address,
                spotted.text
            );
        }

        // 2. The new session's journal is a branch of the old one, so the old
        //    record's positions still mean what they meant.
        let next = change
            .handover
            .resume_into(&dir, SessionId::new("next"))
            .await
            .expect("the record is here, so the branch can be taken");
        for anchor in &change.handover.anchors {
            let spotted = resolve(&anchor.address, &next).await.unwrap_or_else(|e| {
                panic!(
                    "the new session cannot follow `{}` back to the record it inherited: {e}",
                    anchor.address
                )
            });
            assert!(spotted.text.contains(&anchor.item), "{}", spotted.text);
        }

        // 3. The state, and the index. The decision is L1 state and names where
        //    it came from; the constraint is L0, which is the list the layer
        //    injects into every executor; the loose end is an open question; and
        //    the manifest points at the handover itself.
        let seed = change.handover.new_session(&charter());
        let decision = seed
            .ledger
            .decisions
            .iter()
            .find(|d| d.what == DECISION)
            .expect("the carried decision is L1 state");
        assert!(
            decision.why.contains(&format!("journal:seq={}", note.seq)),
            "the state entry has to name where it came from: {}",
            decision.why
        );
        assert!(
            seed.charter.constraints.iter().any(|held| held == CONSTRAINT),
            "the carried constraint has to reach L0, which is what the layer injects: {:#?}",
            seed.charter.constraints
        );
        assert!(
            seed.ledger
                .open_questions
                .iter()
                .any(|q| q.contains("cached at start-up")),
            "a loose end the dying session knew about is an open question: {:#?}",
            seed.ledger.open_questions
        );
        let entry = change.handover.carried_entry(&change.handover_at, 1234);
        let spotted = resolve(&entry.address, &next)
            .await
            .expect("the handover names itself at an address the record keeps");
        assert!(
            spotted.text.contains(DECISION) && spotted.text.contains(&digest),
            "the address has to reach the handover itself, whose own anchors reach the \
             record: {}",
            spotted.text
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A handover whose record is not here is refused as itself.
    ///
    /// `journal:seq=<n>` names a position in one lineage. Resumed against a log
    /// that has nothing to do with it, every address would resolve to the wrong
    /// event — silently, which is the worst way for an index to be wrong — so the
    /// flow refuses rather than pretending.
    #[tokio::test]
    async fn resuming_a_handover_whose_record_is_missing_is_refused() {
        let dir = scratch("missing-record").await;
        let handover = Handover {
            charter: CHARTER.to_owned(),
            origin: Some(Origin {
                session: SessionId::new("a-session-that-never-ran"),
                at_seq: 12,
            }),
            ..Handover::default()
        };
        let err = handover
            .resume_into(&dir, SessionId::new("next"))
            .await
            .expect_err("a record that is not here cannot be branched from");
        assert!(
            matches!(err, HandoverError::MissingRecord { .. }),
            "{err}"
        );
        assert!(err.to_string().contains("a-session-that-never-ran"), "{err}");

        let err = Handover::default()
            .resume_into(&dir, SessionId::new("next"))
            .await
            .expect_err("a handover with no origin cannot be followed back");
        assert!(matches!(err, HandoverError::NoOrigin), "{err}");

        // Branching onto the session the handover came from is not a branch: the
        // marker would land in the log it claims to come from. Measured through
        // the CLI, whose session ids have second resolution.
        let err = Handover {
            charter: CHARTER.to_owned(),
            origin: Some(Origin {
                session: SessionId::new("same"),
                at_seq: 1,
            }),
            ..Handover::default()
        }
        .resume_into(&dir, SessionId::new("same"))
        .await
        .expect_err("a session cannot be its own ancestor");
        assert!(matches!(err, HandoverError::SameSession { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
