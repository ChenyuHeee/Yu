//! `render()` — build the view, and the not-loaded manifest alongside it.
//!
//! DESIGN's first principle is that context is a *view*, not a store:
//!
//! > 上下文 = L0 + L1 + 本轮报告 + 证据块 + 形状指标 + 未装载清单
//! > 渲染出来的,不是累积出来的 → 大小有界、前缀字节稳定(缓存命中高)、永不退化。
//!
//! Two consequences are designed for here rather than hoped for:
//!
//! * **Bounded size.** The view is a function of L0 + L1 + one turn's output, so
//!   it does not grow with turn count. [`View::digest`] makes prefix stability
//!   checkable rather than assumed — which matters more than usual against an
//!   endpoint with automatic prefix caching and inert `cache_control`
//!   (`docs/WIRE-FACTS.md` Quirk 3).
//! * **The omission is stated.** The harness built the view, so it knows exactly
//!   what it left out. DESIGN names the hole this closes: a model cannot know
//!   what it does not know, and a summary lies by claiming to be the content.
//!   [`NotLoaded::render`] is the manifest that does not lie.
//!
//! ## Which layer this is for
//!
//! This renders the **decision layer's** context, and the two layers get their
//! context by different mechanisms — conflating them would undo the design:
//!
//! * The **decision layer** gets a *rendered view*: L0 + L1 + this turn's report,
//!   rebuilt every turn. Its context is the scarce resource, so it must not grow
//!   with turn count. That is what this function produces, and why the result is
//!   one system block plus at most one user message regardless of how long the
//!   run has been going.
//! * The **execution layer** *accumulates* within a task, because it genuinely
//!   needs its own tool calls and results to do the work — but it is "一次性、可
//!   丢弃、fresh", so the accumulation is discarded when the task ends and never
//!   becomes the next task's starting point. That accumulation is the loop's
//!   business (`agent`), not this module's.
//!
//! The trap this avoids: giving the decision layer the executor's transcript.
//! That is the industry default, and it is why tool machinery dominates an
//! agent's context. Here the tool stream never reaches the decision layer at
//! all — it reaches it as shape signals and a manifest.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use yu_observe::{ContextComposition, ShapeSignal};
use yu_wire::types::{Message, SystemBlock, TextBlock};

// ---------------------------------------------------------------------------
// L0 — intent
// ---------------------------------------------------------------------------

/// A human utterance, kept verbatim.
///
/// DESIGN, converged on independently by three sources: Codex's
/// `collect_user_messages` (user messages are collected separately so they
/// survive compaction), Kimi's instruction that "even if the kept messages get
/// truncated, the real ask must be preserved", and the expert protocol's rule
/// that the top of the funnel is yours and you must never hand over a task whose
/// success criterion an agent invented for itself.
///
/// So this is stored as the human wrote it, never paraphrased.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanUtterance {
    pub at_ms: i64,
    pub text: String,
    /// Where it came from, so a later reader can judge how much weight it carries.
    pub source: String,
}

/// L0. Injected every turn, never compacted; retirement is an explicit decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Charter {
    pub text: String,
    pub constraints: Vec<String>,
    /// What counts as done. DESIGN: defining this is never delegated to the
    /// execution layer.
    pub acceptance: Vec<String>,
    pub human_words: Vec<HumanUtterance>,
}

impl Charter {
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.constraints.is_empty()
            && self.acceptance.is_empty()
            && self.human_words.is_empty()
    }
}

// ---------------------------------------------------------------------------
// L1 — state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub what: String,
    pub why: String,
    pub at_ms: i64,
}

/// A proposal that was ruled out.
///
/// DESIGN lists this as part of L1 explicitly. Without it the model re-proposes
/// what has already been rejected, and the human pays to reject it twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejected {
    pub what: String,
    pub why: String,
}

/// Something finished and removed from state.
///
/// DESIGN: L1 contracts by **completion**, not compression — "完成即移出,带理由".
/// Keeping the reason is what makes the contraction auditable and reversible,
/// which is what separates retirement from deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retired {
    pub item: String,
    pub reason: String,
    pub at_ms: i64,
}

/// L1: complete, never summarised. State is summarised only by lying.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub decisions: Vec<Decision>,
    pub open_questions: Vec<String>,
    /// Who is doing what. A label, not a fixed hierarchy — topology is declarative.
    pub roster: Vec<String>,
    pub rejected: Vec<Rejected>,
    pub retired: Vec<Retired>,
}

impl Ledger {
    /// Retire an item with a reason. DESIGN's `retire(item, reason)`: explicit,
    /// auditable, reversible — and isomorphic to Codex's `new_context` in that
    /// the agent may ask for it rather than only waiting for a threshold.
    pub fn retire(&mut self, item: impl Into<String>, reason: impl Into<String>) {
        self.retired.push(Retired {
            item: item.into(),
            reason: reason.into(),
            at_ms: yu_observe::now_ms(),
        });
    }
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// A runnable proof, or the absence of one.
///
/// DESIGN: "证据块不是可选项,是安全带" — and, of the decision layer, "没有可运行
/// 证据的断言一律视为未验证". Modelling the exit code as optional makes the
/// difference between "ran and passed" and "was asserted" visible in the type,
/// rather than in a reader's judgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub label: String,
    pub content: String,
    /// `None` means nothing was actually executed.
    pub exit_code: Option<i32>,
}

impl Evidence {
    pub fn command(label: impl Into<String>, output: impl Into<String>, exit_code: i32) -> Self {
        Self {
            label: label.into(),
            content: output.into(),
            exit_code: Some(exit_code),
        }
    }

    /// An assertion with no runnable evidence behind it.
    pub fn unverified(label: impl Into<String>, claim: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            content: claim.into(),
            exit_code: None,
        }
    }

    pub fn passed(&self) -> Option<bool> {
        self.exit_code.map(|c| c == 0)
    }
}

// ---------------------------------------------------------------------------
// Not-loaded manifest
// ---------------------------------------------------------------------------

/// Something that exists in the record and was deliberately not loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotLoadedEntry {
    pub what: String,
    /// How to reach it: a record seq range, a blob digest, or a path.
    pub address: String,
    pub bytes: u64,
    /// Why it was left out. A manifest that does not say why is a shrug.
    pub why: String,
    /// How to get it, in a form the model can act on.
    pub how_to_load: String,
}

/// The manifest of what the view omits.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotLoaded {
    pub entries: Vec<NotLoadedEntry>,
}

impl NotLoaded {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// The manifest as the model sees it.
    ///
    /// DESIGN's risk 3 is laziness — "可读" does not mean it will be read — and
    /// the stated mitigation is this manifest plus the boundary line. So it says
    /// what exists, what it costs, and how to get it, without claiming to
    /// summarise any of it. An empty manifest renders as nothing at all rather
    /// than as "nothing omitted", which would be noise on every turn.
    pub fn render(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "NOT LOADED — these exist in the record and are deliberately not in this view.\n\
             This list is complete; nothing was silently dropped. To use any of it, delegate \
             a fresh reader rather than loading it here.\n",
        );
        for e in &self.entries {
            out.push_str(&format!(
                "  - {} ({} bytes) at {}\n    left out because: {}\n    to load: {}\n",
                e.what,
                e.bytes,
                e.address,
                e.why,
                e.how_to_load
            ));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub struct RenderInput<'a> {
    pub charter: &'a Charter,
    pub ledger: &'a Ledger,
    /// This turn's report from the executor.
    pub report: Option<&'a str>,
    pub evidence: &'a [Evidence],
    /// Cheap proxies the decision layer watches instead of reading the tool stream.
    pub shape: &'a [ShapeSignal],
    pub omitted: &'a [NotLoadedEntry],
}

/// The rendered context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub system: Vec<SystemBlock>,
    pub messages: Vec<Message>,
    pub not_loaded: NotLoaded,
    /// DESIGN's acceptance metric, computed at render time so it is always
    /// available rather than reconstructed afterwards.
    pub composition: ContextComposition,
    /// SHA-256 over the rendered bytes.
    ///
    /// DESIGN makes byte stability an iron law for cache reuse. This turns it
    /// into something that can be asserted in a test instead of assumed.
    pub digest: String,
}

impl View {
    /// Recompute the digest from the current contents.
    pub fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        for b in &self.system {
            hasher.update(b.text.as_bytes());
            hasher.update([0]);
        }
        for m in &self.messages {
            hasher.update(m.content.concat_text().as_bytes());
            hasher.update([0]);
        }
        hex::encode(hasher.finalize())
    }

    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        self.composition.tool_machinery_ratio()
    }
}

/// Build the view.
///
/// Deterministic by construction: same inputs produce byte-identical output, so
/// the cache prefix is stable across turns. Nothing here reads a clock or a
/// counter — a timestamp in the view would invalidate the cache on every turn,
/// which is the classic silent invalidator.
pub fn render(input: RenderInput<'_>) -> View {
    let system = vec![SystemBlock::text(render_intent(input.charter, input.ledger))];

    let mut volatile = String::new();
    if let Some(report) = input.report {
        volatile.push_str("\n## This turn\n");
        volatile.push_str(report);
        volatile.push('\n');
    }
    if !input.evidence.is_empty() {
        volatile.push_str("\n## Evidence\n");
        for e in input.evidence {
            match e.exit_code {
                Some(code) => volatile.push_str(&format!(
                    "- [{}] exit {}\n{}\n",
                    e.label, code, e.content
                )),
                // DESIGN: an assertion with no runnable evidence is unverified.
                // Saying so in the view is cheaper than hoping the reader notices.
                None => volatile.push_str(&format!(
                    "- [{}] UNVERIFIED — no command was run\n{}\n",
                    e.label, e.content
                )),
            }
        }
    }
    if !input.shape.is_empty() {
        volatile.push_str("\n## Shape\n");
        for s in input.shape {
            volatile.push_str(&format!("- {}\n", describe_signal(s)));
        }
    }

    let not_loaded = NotLoaded {
        entries: input.omitted.to_vec(),
    };
    let manifest = not_loaded.render();
    if !manifest.is_empty() {
        volatile.push('\n');
        volatile.push_str(&manifest);
    }

    let messages = if volatile.trim().is_empty() {
        Vec::new()
    } else {
        vec![Message::user(vec![TextBlock {
            text: volatile.trim().to_owned(),
            citations: None,
        }
        .into()])]
    };

    let composition = measure_composition(&system, &messages, &input);

    let mut view = View {
        system,
        messages,
        not_loaded,
        composition,
        digest: String::new(),
    };
    view.digest = view.compute_digest();
    view
}

fn render_intent(charter: &Charter, ledger: &Ledger) -> String {
    let mut out = String::new();
    if !charter.text.is_empty() {
        out.push_str("# Charter\n");
        out.push_str(&charter.text);
        out.push('\n');
    }
    if !charter.constraints.is_empty() {
        out.push_str("\n## Constraints\n");
        for c in &charter.constraints {
            out.push_str(&format!("- {c}\n"));
        }
    }
    if !charter.acceptance.is_empty() {
        out.push_str("\n## Acceptance\n");
        for a in &charter.acceptance {
            out.push_str(&format!("- {a}\n"));
        }
    }
    if !charter.human_words.is_empty() {
        out.push_str("\n## In the human's own words\n");
        for u in &charter.human_words {
            out.push_str(&format!("- ({}) {}\n", u.source, u.text));
        }
    }
    if !ledger.decisions.is_empty() {
        out.push_str("\n## Decisions\n");
        for d in &ledger.decisions {
            out.push_str(&format!("- {} — because {}\n", d.what, d.why));
        }
    }
    if !ledger.open_questions.is_empty() {
        out.push_str("\n## Open questions\n");
        for q in &ledger.open_questions {
            out.push_str(&format!("- {q}\n"));
        }
    }
    if !ledger.rejected.is_empty() {
        out.push_str("\n## Already rejected — do not re-propose\n");
        for r in &ledger.rejected {
            out.push_str(&format!("- {} — because {}\n", r.what, r.why));
        }
    }
    if !ledger.roster.is_empty() {
        out.push_str("\n## Roster\n");
        for r in &ledger.roster {
            out.push_str(&format!("- {r}\n"));
        }
    }
    if !ledger.retired.is_empty() {
        out.push_str("\n## Retired (done, removed from state)\n");
        for r in &ledger.retired {
            out.push_str(&format!("- {} — {}\n", r.item, r.reason));
        }
    }
    out
}

fn describe_signal(s: &ShapeSignal) -> String {
    match s {
        ShapeSignal::RepeatedCall { signature, count } => {
            format!("identical call `{signature}` seen {count} times")
        }
        ShapeSignal::ScopeDrift { touched, declared } => format!(
            "touched {} file(s) outside the declared set of {}",
            touched.len(),
            declared.len()
        ),
        ShapeSignal::GrowthWithoutArtifact { tokens_delta } => {
            format!("{tokens_delta} tokens added with no artifact produced")
        }
        ShapeSignal::NoArtifactFor { ms } => {
            format!("no verifiable artifact for {}s", ms / 1000)
        }
    }
}

/// DESIGN's acceptance metric: how much of this view is tool machinery.
///
/// The industry shape is that tool results dominate. The decision layer's target
/// is near zero, and the point of computing it here is to make that claim
/// falsifiable rather than aspirational.
fn measure_composition(
    system: &[SystemBlock],
    messages: &[Message],
    input: &RenderInput<'_>,
) -> ContextComposition {
    let total: u64 = system.iter().map(|b| b.text.len() as u64).sum::<u64>()
        + messages
            .iter()
            .map(|m| m.content.concat_text().len() as u64)
            .sum::<u64>();

    // The manifest is the one place tool machinery is *supposed* to appear in a
    // decision-layer view, and even there only as addresses rather than content.
    let manifest_chars: u64 = input
        .omitted
        .iter()
        .map(|e| (e.what.len() + e.address.len() + e.why.len() + e.how_to_load.len()) as u64)
        .sum();

    ContextComposition {
        tool_call_chars: 0,
        tool_result_chars: manifest_chars,
        total_chars: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn charter() -> Charter {
        Charter {
            text: "Make the build reproducible.".into(),
            constraints: vec!["do not touch the release branch".into()],
            acceptance: vec!["`cargo test` exits 0".into()],
            human_words: vec![HumanUtterance {
                at_ms: 0,
                text: "just make it work, but don't touch release".into(),
                source: "terminal".into(),
            }],
        }
    }

    fn empty_ledger() -> Ledger {
        Ledger::default()
    }

    fn render_with<'a>(
        charter: &'a Charter,
        ledger: &'a Ledger,
        omitted: &'a [NotLoadedEntry],
        evidence: &'a [Evidence],
    ) -> View {
        render(RenderInput {
            charter,
            ledger,
            report: Some("did the thing"),
            evidence,
            shape: &[],
            omitted,
        })
    }

    #[test]
    fn rendering_twice_produces_byte_identical_output() {
        let c = charter();
        let l = empty_ledger();
        let a = render_with(&c, &l, &[], &[]);
        let b = render_with(&c, &l, &[], &[]);
        assert_eq!(a.digest, b.digest, "the view must be deterministic");
        assert_eq!(a.messages, b.messages);
    }

    #[test]
    fn the_view_never_contains_a_record_payload_only_its_address() {
        // The whole point of a manifest: it points, it does not contain.
        const SECRET: &str = "PAYLOAD-THAT-MUST-NOT-BE-INLINED";
        let c = charter();
        let l = empty_ledger();
        let omitted = vec![NotLoadedEntry {
            what: "the full test log".into(),
            address: "journal:seq=41..97".into(),
            bytes: 8_400_000,
            why: "not needed for this decision".into(),
            how_to_load: "delegate_read(seq=41..97, \"did anything fail?\")".into(),
        }];
        let view = render_with(&c, &l, &omitted, &[]);
        let rendered = view.messages[0].content.concat_text();
        assert!(!rendered.contains(SECRET));
        assert!(rendered.contains("journal:seq=41..97"));
        assert!(rendered.contains("8400000"), "the cost must be stated");
    }

    #[test]
    fn an_empty_manifest_renders_as_nothing() {
        // A manifest that says "nothing omitted" on every turn is noise, and
        // noise on a stable prefix is a cache problem.
        assert_eq!(NotLoaded::default().render(), "");
    }

    #[test]
    fn the_manifest_claims_completeness_explicitly() {
        let manifest = NotLoaded {
            entries: vec![NotLoadedEntry {
                what: "old transcript".into(),
                address: "blob:abc123".into(),
                bytes: 100,
                why: "superseded".into(),
                how_to_load: "delegate_read(blob:abc123, ...)".into(),
            }],
        }
        .render();
        assert!(manifest.contains("nothing was silently dropped"));
        assert!(manifest.contains("blob:abc123"));
    }

    #[test]
    fn evidence_with_no_command_is_marked_unverified() {
        // DESIGN: an assertion without runnable evidence counts as unverified.
        let c = charter();
        let l = empty_ledger();
        let evidence = vec![
            Evidence::command("cargo test", "ok", 0),
            Evidence::unverified("perf", "it is definitely faster now"),
        ];
        let view = render_with(&c, &l, &[], &evidence);
        let rendered = view.messages[0].content.concat_text();
        assert!(rendered.contains("exit 0"));
        assert!(rendered.contains("UNVERIFIED"));
    }

    #[test]
    fn rejected_proposals_are_carried_so_they_are_not_re_proposed() {
        let c = charter();
        let l = Ledger {
            rejected: vec![Rejected {
                what: "rewrite it in Go".into(),
                why: "the whole point is to stay in Rust".into(),
            }],
            ..Ledger::default()
        };
        let view = render_with(&c, &l, &[], &[]);
        let system = &view.system[0].text;
        assert!(system.contains("do not re-propose"));
        assert!(system.contains("rewrite it in Go"));
    }

    #[test]
    fn the_humans_own_words_survive_verbatim() {
        let c = charter();
        let l = empty_ledger();
        let view = render_with(&c, &l, &[], &[]);
        assert!(
            view.system[0]
                .text
                .contains("just make it work, but don't touch release"),
            "DESIGN: 人类原话原样存活"
        );
    }

    #[test]
    fn retiring_records_the_reason() {
        let mut l = Ledger::default();
        l.retire("migrate the config loader", "done in commit 4f2a");
        assert_eq!(l.retired.len(), 1);
        assert_eq!(l.retired[0].reason, "done in commit 4f2a");
        // DESIGN: contraction is by completion, and the reason is what makes it
        // auditable and reversible.
        let c = charter();
        let view = render_with(&c, &l, &[], &[]);
        assert!(view.system[0].text.contains("done in commit 4f2a"));
    }

    #[test]
    fn view_size_does_not_grow_with_the_record() {
        // DESIGN: the view is rendered, not accumulated, so it is bounded. What
        // varies with the record is the manifest's *length*, not its content —
        // so a large record produces addresses, never payloads.
        let c = charter();
        let l = empty_ledger();
        let small = render_with(&c, &l, &[], &[]);
        let many: Vec<NotLoadedEntry> = (0..50)
            .map(|i| NotLoadedEntry {
                what: format!("segment {i}"),
                address: format!("journal:seq={}", i * 100),
                bytes: 1_000_000,
                why: "not needed".into(),
                how_to_load: "delegate_read(...)".into(),
            })
            .collect();
        let large = render_with(&c, &l, &many, &[]);
        let small_len = small.messages[0].content.concat_text().len();
        let large_len = large.messages[0].content.concat_text().len();
        // 50 entries at ~1 MB each are described in a few kB of addresses.
        assert!(large_len < 50 * 400);
        assert!(large_len > small_len);
        assert_eq!(large.messages.len(), 1, "the view stays one message");
    }

    #[test]
    fn composition_counts_only_the_manifest_as_tool_machinery() {
        let c = charter();
        let l = empty_ledger();
        let view = render_with(&c, &l, &[], &[]);
        // A decision-layer view with no omissions has zero tool machinery.
        assert_eq!(view.composition.tool_call_chars, 0);
        assert_eq!(view.composition.tool_result_chars, 0);
        assert_eq!(view.tool_machinery_ratio(), Some(0.0));
    }

    #[test]
    fn ratio_is_undefined_for_an_empty_view_rather_than_zero() {
        // Zero and "undefined" are different claims; conflating them would let a
        // broken renderer look like a perfect one.
        let composition = ContextComposition::default();
        assert_eq!(composition.tool_machinery_ratio(), None);
    }

    #[test]
    fn a_changed_report_changes_the_digest() {
        let c = charter();
        let l = empty_ledger();
        let a = render(RenderInput {
            charter: &c,
            ledger: &l,
            report: Some("first"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let b = render(RenderInput {
            charter: &c,
            ledger: &l,
            report: Some("second"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn the_stable_part_is_a_prefix_of_the_view() {
        // Cache reuse depends on L0/L1 sitting before anything volatile, since a
        // byte change anywhere invalidates everything after it.
        let c = charter();
        let l = empty_ledger();
        let a = render(RenderInput {
            charter: &c,
            ledger: &l,
            report: Some("one"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let b = render(RenderInput {
            charter: &c,
            ledger: &l,
            report: Some("a completely different report"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        assert_eq!(
            a.system, b.system,
            "the system prefix must not depend on this turn's output"
        );
    }

    #[test]
    fn shape_signals_are_described_without_the_tool_stream() {
        let c = charter();
        let l = empty_ledger();
        let shape = vec![
            ShapeSignal::RepeatedCall {
                signature: "read:src/a.rs".into(),
                count: 2,
            },
            ShapeSignal::GrowthWithoutArtifact { tokens_delta: 900 },
        ];
        let view = render(RenderInput {
            charter: &c,
            ledger: &l,
            report: None,
            evidence: &[],
            shape: &shape,
            omitted: &[],
        });
        let rendered = view.messages[0].content.concat_text();
        assert!(rendered.contains("seen 2 times"));
        assert!(rendered.contains("900 tokens added with no artifact"));
    }
}
