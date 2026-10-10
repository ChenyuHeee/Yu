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
use yu_wire::types::{CacheControl, ContentBlock, Message, SystemBlock, TextBlock, ToolDefinition};

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
    /// The shape the work is to take, when one was declared.
    ///
    /// L0 rather than a field on the loop, because DESIGN's constraint 0 puts
    /// topology among the things that come from an external resource, and its
    /// derived principle 3 puts constraints *injected* rather than remembered.
    /// A topology the layer is told each round is a constraint; one compiled
    /// into the loop is a structure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topology: Option<crate::topology::TopologySpec>,
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
    /// How many active entries this actually removed.
    ///
    /// Recorded because "retired" and "removed something" are different claims.
    /// A retirement that matched nothing leaves the item in state, and a reader
    /// who assumes otherwise would believe the ledger had contracted when it had
    /// not. Zero here says the item was not found — or had already gone.
    #[serde(default)]
    pub removed: usize,
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
    /// Retire an item: **remove it from active state** and record why.
    ///
    /// DESIGN's `retire(item, reason)` — explicit, auditable, reversible, and
    /// isomorphic to Codex's `new_context` in that the agent may ask for it
    /// rather than only waiting for a threshold.
    ///
    /// The removal is the point. An earlier version only appended to `retired`,
    /// which meant a finished item stayed in `decisions` *and* appeared under
    /// `retired` — counted twice, and the view grew monotonically. That is the
    /// exact failure the whole design exists to prevent: L1 contracts by
    /// completion, not by compression, and completion means the item leaves.
    ///
    /// Returns how many active entries were removed, so a caller can tell
    /// "retired" from "retired something".
    pub fn retire(&mut self, item: impl Into<String>, reason: impl Into<String>) -> usize {
        let item = item.into();
        let before = self.active_len();

        self.decisions.retain(|d| d.what != item);
        self.open_questions.retain(|q| q != &item);
        self.rejected.retain(|r| r.what != item);
        self.roster.retain(|r| r != &item);

        let removed = before - self.active_len();
        self.retired.push(Retired {
            item,
            reason: reason.into(),
            at_ms: yu_observe::now_ms(),
            removed,
        });
        removed
    }

    /// Entries currently in state — the part that is supposed to stay bounded.
    ///
    /// `retired` is deliberately excluded: it is an audit trail, not state, and
    /// counting it would hide the very growth this measures.
    pub fn active_len(&self) -> usize {
        self.decisions.len()
            + self.open_questions.len()
            + self.rejected.len()
            + self.roster.len()
    }

    /// Whether the ledger has entries whose text this retirement would match.
    pub fn holds(&self, item: &str) -> bool {
        self.decisions.iter().any(|d| d.what == item)
            || self.open_questions.iter().any(|q| q == item)
            || self.rejected.iter().any(|r| r.what == item)
            || self.roster.iter().any(|r| r == item)
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
        // One line per entry, with the standing instruction stated once.
        //
        // Measured: written out in full prose per entry, this manifest was the
        // largest single consumer of the decision layer's context — larger than
        // the tool results it was describing. Every entry repeated the same two
        // sentences, and a reader does not need "delegate a reader rather than
        // loading it here" twelve times to have read it once.
        //
        // What is *not* compressed: the list is still complete, each entry still
        // names its size and its address, and it still says outright that
        // nothing was dropped. DESIGN's rule is that a manifest must not lie
        // about what it omitted — not that it must be verbose about it.
        const DEFAULT_LOAD: &str = "delegate a reader for it";
        let mut out = format!(
            "NOT LOADED — {} entr{}, {} bytes, complete: nothing was silently dropped.\n\
             Delegate a reader for any of these rather than loading it into this view.\n",
            self.entries.len(),
            if self.entries.len() == 1 { "y" } else { "ies" },
            self.total_bytes()
        );
        for e in &self.entries {
            out.push_str(&format!(
                "  - {} — {}B — {} — {}",
                e.what, e.bytes, e.address, e.why
            ));
            // Only when it differs from the standing instruction, so the
            // exception is visible rather than buried in repetition.
            if e.how_to_load != DEFAULT_LOAD {
                out.push_str(&format!(" [load via: {}]", e.how_to_load));
            }
            out.push('\n');
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub struct RenderInput<'a> {
    /// The layer's own prompt.
    ///
    /// Constant for a given layer, so it sits at the very front of the request
    /// where it is the most cacheable bytes there are. Passed in rather than
    /// baked in because it is a resource, not code — DESIGN's constraint 0.
    pub prompt: &'a str,
    /// L0.
    pub charter: &'a Charter,
    /// L1.
    pub ledger: &'a Ledger,
    /// The tools the layer may call.
    ///
    /// Part of the rendered prefix, and rendered *here*, because DESIGN fixes
    /// the render order as `tools` → `system` → `messages`. Assembling tools
    /// somewhere else splits the prefix across two authors, and a prefix with
    /// two authors drifts.
    pub tools: &'a [ToolDefinition],
    /// This round's report from the executor.
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
    /// The tools, rendered into the prefix and cache-marked alongside it.
    pub tools: Vec<ToolDefinition>,
    pub messages: Vec<Message>,
    pub not_loaded: NotLoaded,
    /// DESIGN's acceptance metric, computed at render time so it is always
    /// available rather than reconstructed afterwards.
    pub composition: ContextComposition,
    /// SHA-256 over the whole rendered prefix and messages.
    ///
    /// DESIGN makes byte stability an iron law for cache reuse. This turns it
    /// into something that can be asserted in a test instead of assumed.
    pub digest: String,
}

impl View {
    /// Recompute the digest from the current contents.
    ///
    /// Covers tools as well as system and messages: the render order puts tools
    /// first, so a tool change invalidates everything after it, and a digest
    /// that ignored tools would report a stable prefix across a change that
    /// breaks the cache.
    pub fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        for t in &self.tools {
            hasher.update(t.name().as_bytes());
            if let ToolDefinition::Custom(c) = t {
                hasher.update(c.description.as_deref().unwrap_or("").as_bytes());
                hasher.update(c.input_schema.to_string().as_bytes());
            } else if let ToolDefinition::Server(s) = t {
                hasher.update(s.kind.as_bytes());
            }
            hasher.update([0]);
        }
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

    /// Cache breakpoints in this view. Must never exceed the API's limit.
    pub fn cache_breakpoints(&self) -> usize {
        self.tools
            .iter()
            .filter(|t| matches!(t, ToolDefinition::Custom(c) if c.cache_control.is_some()))
            .count()
            + self
                .system
                .iter()
                .filter(|b| b.cache_control.is_some())
                .count()
    }
}

/// Mark a tool as the end of a cacheable prefix.
fn mark_tool_breakpoint(tool: &mut ToolDefinition) {
    if let ToolDefinition::Custom(c) = tool {
        c.cache_control = Some(CacheControl::ephemeral());
    }
}

/// Bytes a tool definition occupies in the context.
///
/// Shared rather than reimplemented, so the prefix measurement here and the
/// conversation measurement in the loops cannot disagree about what a tool
/// costs.
pub fn tool_definition_chars(tools: &[ToolDefinition]) -> u64 {
    tools
        .iter()
        .map(|t| match t {
            ToolDefinition::Custom(c) => (c.name.len()
                + c.description.as_deref().unwrap_or("").len()
                + c.input_schema.to_string().len()) as u64,
            ToolDefinition::Server(s) => (s.kind.len() + s.name.len()) as u64,
        })
        .sum()
}

/// Accumulates what a context is actually made of, as it is built.
///
/// DESIGN's acceptance criterion names two numbers: the tool-machinery ratio in
/// the **decision** layer's context and in the **executor's**. Both are measured
/// with this, deliberately. Two implementations would drift, and the drift would
/// be invisible — each number would look plausible on its own, and the comparison
/// the whole design rests on would be between two different quantities.
///
/// The earlier version of this measurement lived in `render`, hardcoded
/// `tool_call_chars` to zero and took `tool_result_chars` from the manifest's own
/// prose. Applied to a decision-layer view — which by construction holds no tool
/// output — it could only ever report approximately zero. A metric that cannot
/// fail detects nothing, which is why the ratio is now computed where the
/// conversation actually exists.
#[derive(Debug, Default, Clone)]
pub struct CompositionMeter {
    tool_call_chars: u64,
    tool_result_chars: u64,
    tool_definition_chars: u64,
    total_chars: u64,
}

impl CompositionMeter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Charge the tool definitions, which occupy context on every request.
    pub fn add_tools(&mut self, tools: &[ToolDefinition]) {
        let chars = tool_definition_chars(tools);
        self.tool_definition_chars += chars;
        self.total_chars += chars;
    }

    pub fn add_system(&mut self, blocks: &[SystemBlock]) {
        self.total_chars += blocks.iter().map(|b| b.text.len() as u64).sum::<u64>();
    }

    pub fn add_message(&mut self, message: &Message) {
        for block in message.blocks() {
            let chars = match &block {
                ContentBlock::Text(t) => t.text.len() as u64,
                ContentBlock::ToolUse(t) => serde_json::to_string(&t.input)
                    .map(|s| s.len())
                    .unwrap_or(0) as u64,
                ContentBlock::ToolResult(t) => match &t.content {
                    Some(yu_wire::types::ToolResultContent::Text(s)) => s.len() as u64,
                    Some(yu_wire::types::ToolResultContent::Blocks(b)) => b
                        .iter()
                        .filter_map(|x| x.as_text())
                        .map(|s| s.len() as u64)
                        .sum(),
                    None => 0,
                },
                ContentBlock::Thinking(t) => t.thinking.len() as u64,
                ContentBlock::Other(v) => v.to_string().len() as u64,
                _ => 0,
            };
            match &block {
                ContentBlock::ToolUse(_) => self.tool_call_chars += chars,
                ContentBlock::ToolResult(_) => self.tool_result_chars += chars,
                _ => {}
            }
            self.total_chars += chars;
        }
    }

    pub fn snapshot(&self) -> ContextComposition {
        ContextComposition {
            tool_call_chars: self.tool_call_chars,
            tool_result_chars: self.tool_result_chars,
            tool_definition_chars: self.tool_definition_chars,
            total_chars: self.total_chars,
        }
    }
}

/// Build the view.
///
/// Deterministic by construction: same inputs produce byte-identical output, so
/// the cache prefix is stable across turns. Nothing here reads a clock or a
/// counter — a timestamp in the view would invalidate the cache on every turn,
/// which is the classic silent invalidator.
pub fn render(input: RenderInput<'_>) -> View {
    // The prefix, in the order the API renders it: tools, then system, then
    // messages. A byte change anywhere in it invalidates everything after, so
    // the stable parts go first and each gets a breakpoint. Four breakpoints is
    // the API's ceiling and this uses at most four: last tool, prompt, L0, L1.
    // Everything before the last breakpoint that still matches is reused when
    // L1 changes — which is most turns.
    let mut tools: Vec<ToolDefinition> = input.tools.to_vec();
    if let Some(last) = tools.last_mut() {
        mark_tool_breakpoint(last);
    }

    let system = vec![
        // Constant for the life of the layer.
        SystemBlock::text(input.prompt).with_cache(),
        // L0: the charter. Changes only when the human changes their mind.
        SystemBlock::text(render_l0(input.charter)).with_cache(),
        // L1: the ledger. Changes whenever state changes, so it is the boundary
        // that actually gets exercised.
        SystemBlock::text(render_l1(input.ledger)).with_cache(),
    ];

    // The view always carries exactly one user turn, and it always says
    // something. Two reasons, and the second is the one that bit:
    //
    // 1. The API requires at least one message; a request with only a system
    //    prompt is a 400.
    // 2. More importantly, a layer handed a view with no report has to be told
    //    *that* — "nothing has been done yet" is information, and its absence
    //    reads as "there is nothing to respond to".
    let mut volatile = String::from("## This round\n");
    match input.report {
        Some(report) => {
            volatile.push_str(report);
            volatile.push('\n');
        }
        None => volatile.push_str(
            "Nothing has been delegated or reported yet. Decide what to do, or \
             delegate the work.\n",
        ),
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

    let messages = vec![Message::user(vec![TextBlock {
        text: volatile.trim().to_owned(),
        citations: None,
    }
    .into()])];

    let composition = measure_composition(&tools, &system, &messages, &input);

    let mut view = View {
        system,
        tools,
        messages,
        not_loaded,
        composition,
        digest: String::new(),
    };
    view.digest = view.compute_digest();
    view
}

/// L0 — intent. Rendered as its own block so the boundary between it and L1 can
/// carry a cache breakpoint: adding a decision must not reshuffle the charter.
fn render_l0(charter: &Charter) -> String {
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
    if let Some(topology) = &charter.topology {
        out.push_str(&format!(
            "\n## Declared topology: {}\n",
            topology.kind.as_str()
        ));
        if let Some(role) = &topology.child_role {
            out.push_str(&format!("Derived agents take the role: {role}\n"));
        }
        for (key, value) in &topology.params {
            out.push_str(&format!("- {key} = {value}\n"));
        }
    }
    out
}

/// L1 — state. Complete, never summarised; contracted by retirement, not by
/// compression.
fn render_l1(ledger: &Ledger) -> String {
    let mut out = String::new();
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

/// What this *rendered prefix* is made of.
///
/// Scope matters here, and an earlier version got it wrong in a way worth
/// recording. It hardcoded `tool_call_chars: 0` and took `tool_result_chars`
/// from the manifest's own prose, then reported a ratio. For a decision-layer
/// view — which by construction contains no tool output — that could only ever
/// produce a number near zero, so the metric could not fail, and a metric that
/// cannot fail detects nothing.
///
/// The rendered prefix genuinely has no tool calls or results: those arrive in
/// the *conversation* afterwards. So this reports what the prefix does contain —
/// the tool definitions, and the manifest, which is the one place tool output is
/// even referred to. The ratio over a whole conversation, which is the number
/// DESIGN's acceptance criterion actually names, is measured where the
/// conversation exists: `crate::decision` for the decision layer and
/// `crate::agent` for the executor.
fn measure_composition(
    tools: &[ToolDefinition],
    system: &[SystemBlock],
    messages: &[Message],
    input: &RenderInput<'_>,
) -> ContextComposition {
    let definition_chars = tool_definition_chars(tools);

    // Addresses, not payloads: the manifest says what exists and how to reach
    // it, and the payload stays where it is.
    let manifest_chars: u64 = input
        .omitted
        .iter()
        .map(|e| (e.what.len() + e.address.len() + e.why.len() + e.how_to_load.len()) as u64)
        .sum();

    let total: u64 = definition_chars
        + system.iter().map(|b| b.text.len() as u64).sum::<u64>()
        + messages
            .iter()
            .map(|m| m.content.concat_text().len() as u64)
            .sum::<u64>();

    ContextComposition {
        tool_call_chars: 0,
        tool_result_chars: manifest_chars,
        tool_definition_chars: definition_chars,
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
            topology: None,
        }
    }

    fn empty_ledger() -> Ledger {
        Ledger::default()
    }

    const TEST_PROMPT: &str = "You are the decision layer.";

    /// The intent half of the prefix — L0 and L1, i.e. everything after the
    /// layer's own prompt. Tests address the intent through this rather than
    /// through `system[0]`, which is now the prompt and would silently make
    /// every assertion here vacuous if the prompt block were ever dropped.
    fn intent(view: &View) -> String {
        view.system
            .iter()
            .skip(1)
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn delegate_tool() -> ToolDefinition {
        crate::delegate::delegate_tool()
    }

    fn render_with<'a>(
        charter: &'a Charter,
        ledger: &'a Ledger,
        omitted: &'a [NotLoadedEntry],
        evidence: &'a [Evidence],
    ) -> View {
        render(RenderInput {
            prompt: TEST_PROMPT,
            charter,
            ledger,
            tools: &[],
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
        assert!(intent(&view).contains("do not re-propose"));
        assert!(intent(&view).contains("rewrite it in Go"));
    }

    #[test]
    fn the_humans_own_words_survive_verbatim() {
        let c = charter();
        let l = empty_ledger();
        let view = render_with(&c, &l, &[], &[]);
        assert!(
            intent(&view).contains("just make it work, but don't touch release"),
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
        assert!(intent(&view).contains("done in commit 4f2a"));
    }

    #[test]
    fn a_large_record_costs_addresses_not_payloads() {
        // What the *record* varies is the manifest's length, never its content:
        // 50 entries of a megabyte each are described in a few kB of addresses.
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
        assert!(large_len < 50 * 400);
        assert!(large_len > small_len);
        assert_eq!(large.messages.len(), 1, "the view stays one message");
    }

    /// The boundedness claim that actually matters, and that an earlier version
    /// of this test suite did not check.
    ///
    /// The previous test here was named `view_size_does_not_grow_with_the_record`
    /// and rendered with an **empty ledger** on both sides — so it varied the
    /// manifest and never touched L1, which is the collection that grows
    /// monotonically in production. It asserted a property it could not fail.
    /// This one grows L1 for real, so it fails if the ledger stops contracting.
    #[test]
    fn l1_growth_is_visible_and_retirement_is_what_reverses_it() {
        let c = charter();
        let mut l = empty_ledger();

        let size_with = |l: &Ledger| intent(&render_with(&c, l, &[], &[])).len();
        let empty_size = size_with(&l);

        // 40 decisions accumulate; the view must reflect that, or the metric
        // below is measuring nothing.
        for i in 0..40 {
            l.decisions.push(Decision {
                what: format!("decision number {i} with a reasonably long description"),
                why: "because the alternative was worse in a way worth writing down".into(),
                at_ms: 0,
            });
        }
        let grown_size = size_with(&l);
        assert!(
            grown_size > empty_size,
            "L1 growth must show up in the view, or nothing here is being measured"
        );
        assert_eq!(l.active_len(), 40);

        // Retiring is the mechanism that reverses it — DESIGN: "靠完成收缩".
        for i in 0..40 {
            let removed = l.retire(
                format!("decision number {i} with a reasonably long description"),
                "superseded",
            );
            assert_eq!(removed, 1, "retire must remove the entry it names");
        }
        assert_eq!(l.active_len(), 0);
        let shrunk_size = size_with(&l);
        assert!(
            shrunk_size < grown_size,
            "retiring everything must shrink the view: {shrunk_size} vs {grown_size}"
        );
        // The audit trail survives even though the state is gone.
        assert_eq!(l.retired.len(), 40);
    }

    #[test]
    fn a_retired_item_leaves_state_and_is_not_counted_twice() {
        let mut l = Ledger {
            decisions: vec![Decision {
                what: "rewrite the parser".into(),
                why: "faster".into(),
                at_ms: 0,
            }],
            open_questions: vec!["should we cache?".into()],
            rejected: vec![Rejected {
                what: "use a regex".into(),
                why: "unreadable".into(),
            }],
            roster: vec!["executor-a".into()],
            ..Ledger::default()
        };
        assert!(l.holds("rewrite the parser"));

        assert_eq!(l.retire("rewrite the parser", "done"), 1);
        assert!(!l.holds("rewrite the parser"));
        // It appears once, under retired — not under both.
        let c = charter();
        let view = render_with(&c, &l, &[], &[]);
        assert_eq!(
            intent(&view).matches("rewrite the parser").count(),
            1,
            "a retired item must be stated once, not counted twice"
        );

        assert_eq!(l.retire("should we cache?", "answered by the load test"), 1);
        assert_eq!(l.retire("use a regex", "reconsidering"), 1);
        assert_eq!(l.retire("executor-a", "task finished"), 1);
        assert_eq!(l.active_len(), 0);
    }

    #[test]
    fn retiring_something_absent_removes_nothing_and_says_so() {
        // "retired" and "removed something" are different claims. A retirement
        // that matched nothing must not look like a contraction.
        let mut l = empty_ledger();
        let removed = l.retire("something that was never there", "tidying up");
        assert_eq!(removed, 0);
        assert_eq!(l.retired.len(), 1);
        assert_eq!(l.retired[0].removed, 0);
        assert_eq!(l.active_len(), 0);
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
            prompt: TEST_PROMPT,
            tools: &[delegate_tool()],
            charter: &c,
            ledger: &l,
            report: Some("first"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let b = render(RenderInput {
            prompt: TEST_PROMPT,
            tools: &[delegate_tool()],
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
            prompt: TEST_PROMPT,
            tools: &[delegate_tool()],
            charter: &c,
            ledger: &l,
            report: Some("one"),
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let b = render(RenderInput {
            prompt: TEST_PROMPT,
            tools: &[delegate_tool()],
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
            prompt: TEST_PROMPT,
            tools: &[delegate_tool()],
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
