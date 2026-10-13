//! The reduction table.
//!
//! DESIGN: "归约路径由 **(kind × size × reproducible × 任务形状 × 当前压力)** 选",
//! and "五条在同一张表上,由策略选 —— 这就是差异所在。外界每家只实现了其中一条。"
//!
//! | case | path |
//! |---|---|
//! | small | pass through |
//! | large, reproducible | drop + manifest: what is missing, and the command to re-run |
//! | large, not reproducible, addressable | spill + instruction-carrying pointer, read-back closed against re-spilling |
//! | unbounded shape (search / log / long thread) | dispatch a shape-specialised reader |
//! | large structured | sandbox, return only the reduced result (PTC) |
//!
//! Four properties of this module are testable claims rather than prose:
//!
//! 1. [`Reducer::reader_prompt`] is *generated* from the shape and the question
//!    and stays tiny — a reader is a reader, it does not need full tool
//!    discipline. Cheap delegation is what makes the reader path usable at all.
//! 2. [`Reducer::choose`] returns a [`Reduction`] value. The reduction is
//!    therefore an explicit event the caller can journal, not a truncation that
//!    happened inside a formatting function; [`Reducer::apply`] never emits a
//!    payload whose manifest disagrees with the reduction it was given.
//! 3. A payload that came from a spill is never spilled again — neither by
//!    `choose` (which refuses the path) nor by `apply` (which refuses to write
//!    bytes it does not hold). flower's self-loop was reading a spilled file and
//!    spilling the read back out; a payload that is already on disk is never
//!    written to disk again.
//! 4. A spill the store refuses is never rendered as a pointer. `apply` reads an
//!    empty answer from the sink as "not stored" and passes the payload through
//!    instead; the provisional `fnv1a64:` digest [`Reducer::choose`] computes is
//!    not an address any store resolves, so building a pointer from it would
//!    promise the model bytes nothing can hand it and record a blob that is not
//!    there. The pass-through is the one downgrade that is both truthful
//!    (nothing is omitted, so nothing is claimed) and lossless — and this path
//!    exists precisely for payloads that cannot be obtained again, so a silent
//!    drop would destroy the only copy.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::result::{Manifest, Payload, Provenance, ResultKind, ToolResult};

/// Thresholds, in bytes of payload text.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Payloads at or below this go straight through.
    pub inline_limit: u64,
    /// Ceiling on the text a specialised reader may return.
    ///
    /// It also prices the reader path for the payload side: delegating an
    /// unbounded result costs a subagent plus at most this much text, so under
    /// low pressure a payload costing no more than `inline_limit +
    /// reader_limit` is passed through instead. See [`Reducer::choose`].
    pub reader_limit: u64,
}

impl Budget {
    pub fn new(inline_limit: u64, reader_limit: u64) -> Self {
        Self {
            inline_limit,
            reader_limit,
        }
    }
}

impl Default for Budget {
    /// The relation between the two numbers is the point: a reader's answer must
    /// be cheaper than the payload it replaces, or delegation is a loss.
    fn default() -> Self {
        Self {
            inline_limit: 8 * 1024,
            reader_limit: 2 * 1024,
        }
    }
}

/// How full the context window is.
///
/// `fill` is a fraction of the window's edge. It is the dimension DESIGN insists
/// long-range machinery be a function of: "每一个长程机制都应该是『压力』的函数,
/// 不是『轮数』或『时钟』的函数。"
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pressure {
    /// 0.0 = nothing in context, 1.0 = at the window's edge.
    pub fill: f64,
}

// Written by hand rather than derived: `f64` is not `Eq`, so a derive would not
// compile. The contract holds because `fill` is always a real number in [0, 1] —
// every constructor clamps, and NaN is mapped to 0.0.
impl Eq for Pressure {}

impl Pressure {
    /// At or above this, the window is treated as full and the reduction rules
    /// are applied without the low-pressure allowance.
    pub const HIGH_WATERMARK: f64 = 0.75;

    pub fn low() -> Self {
        Self { fill: 0.0 }
    }

    pub fn high() -> Self {
        Self { fill: 1.0 }
    }

    /// A pressure measured elsewhere, clamped into the domain.
    pub fn with_fill(fill: f64) -> Self {
        Self {
            fill: if fill.is_nan() {
                0.0
            } else {
                fill.clamp(0.0, 1.0)
            },
        }
    }

    pub fn is_high(&self) -> bool {
        self.fill >= Self::HIGH_WATERMARK
    }
}

/// The chosen path. A value, so that it can be journalled — see claim 2 above.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "path", rename_all = "snake_case")]
pub enum Reduction {
    PassThrough,
    DropWithManifest {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rerun: Option<String>,
    },
    SpillWithPointer {
        digest: String,
        pointer: String,
    },
    DelegateReader {
        prompt: String,
        subagent: String,
    },
    SandboxReduce,
    /// No reduction was chosen, because no payload was produced: the call's
    /// effect was already recorded as completed, so the loop read the record
    /// instead of running the tool a second time.
    ///
    /// A reduction of its own rather than a pass-through, because the two say
    /// opposite things about where the bytes came from: a pass-through is a
    /// payload the tool just produced, this is a payload that was produced once
    /// and is being re-read. Reporting it as a pass-through would put a call that
    /// never ran into a run's reduction count.
    Replayed { seq: u64 },
}

/// Which of DESIGN's five paths applies.
///
/// A vocabulary rather than a `Reduction`, because a `Reduction` carries
/// payload-specific data — the re-run command, the digest, the reader prompt —
/// that a policy cannot know and should not have to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReductionPath {
    PassThrough,
    DropWithManifest,
    SpillWithPointer,
    DispatchReader,
    SandboxReduce,
}

impl ReductionPath {
    pub fn as_str(self) -> &'static str {
        match self {
            ReductionPath::PassThrough => "pass_through",
            ReductionPath::DropWithManifest => "drop_with_manifest",
            ReductionPath::SpillWithPointer => "spill_with_pointer",
            ReductionPath::DispatchReader => "dispatch_reader",
            ReductionPath::SandboxReduce => "sandbox_reduce",
        }
    }

    /// The path a policy named, or `None` if it named something else.
    ///
    /// `None` rather than a default: an unrecognised name in a policy is a
    /// mistake, and silently reducing it to `PassThrough` would turn a typo into
    /// a decision nobody made.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "pass_through" => ReductionPath::PassThrough,
            "drop_with_manifest" => ReductionPath::DropWithManifest,
            "spill_with_pointer" => ReductionPath::SpillWithPointer,
            "dispatch_reader" | "delegate_reader" => ReductionPath::DispatchReader,
            "sandbox_reduce" => ReductionPath::SandboxReduce,
            _ => return None,
        })
    }
}

/// Chooses and applies reductions.
///
/// Shared behind an `Arc` by every tool context in a process. `Clone` is derived
/// and `Debug` is written out, because the policy's table is a trait object and a
/// derived `Debug` cannot see through it.
#[derive(Clone)]
pub struct Reducer {
    budget: Budget,
    /// A policy's table, consulted before the built-in one.
    table: Option<Arc<dyn ReductionTable>>,
}

impl std::fmt::Debug for Reducer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reducer")
            .field("budget", &self.budget)
            .field("policy_table", &self.table.is_some())
            .finish()
    }
}

/// The axes DESIGN's table is indexed by.
///
/// Passed to a policy so it can select a row without reaching into the reducer —
/// the policy sees the same facts the built-in table does, and no others.
#[derive(Debug)]
pub struct Axes<'a> {
    pub kind: ResultKind,
    pub size: u64,
    pub reproducible: bool,
    pub addressable: bool,
    pub pressure: Pressure,
    pub already_on_disk: bool,
    /// The threshold a policy needs in order to interpret "large".
    pub inline_limit: u64,
    pub provenance: &'a Provenance,
}

/// A reduction policy supplied from outside.
///
/// DESIGN lists "归约规则" among the things that must be external resources
/// (constraint 0), and the acceptance criterion is that changing one must not
/// need a rebuild. Until this trait existed, the reduction table was a constant
/// in this crate — so constraint 0 held for every rule except the one the whole
/// project is built around.
///
/// Returning `None` falls through to the built-in table, so a policy expresses
/// only the rows it wants to change.
pub trait ReductionTable: Send + Sync {
    fn path_for(&self, axes: &Axes<'_>) -> Option<ReductionPath>;
}

impl Reducer {
    /// Attach a policy's table. Consulted before the built-in one.
    pub fn with_table(mut self, table: Arc<dyn ReductionTable>) -> Self {
        self.table = Some(table);
        self
    }

    /// The facts a policy row is selected by.
    fn axes<'a>(&self, result: &'a ToolResult, pressure: Pressure) -> Axes<'a> {
        Axes {
            kind: result.kind,
            size: result.size,
            reproducible: result.reproducible,
            addressable: result.addressable,
            pressure,
            already_on_disk: result.provenance.from_spill.is_some()
                || result.payload.is_on_disk(),
            inline_limit: self.budget.inline_limit,
            provenance: &result.provenance,
        }
    }

    /// Build the reduction for a policy-chosen path.
    ///
    /// Mirrors the branches of [`Reducer::choose`]. Kept in step by
    /// `a_policy_path_matches_what_the_builtin_table_would_have_chosen`, which
    /// asserts the two agree for the same axes rather than trusting that they do.
    fn materialise(&self, path: ReductionPath, result: &ToolResult) -> Reduction {
        let already_on_disk =
            result.provenance.from_spill.is_some() || result.payload.is_on_disk();
        match path {
            ReductionPath::PassThrough => Reduction::PassThrough,
            ReductionPath::SandboxReduce => Reduction::SandboxReduce,
            ReductionPath::DropWithManifest => Reduction::DropWithManifest {
                rerun: rerun_command(&result.provenance),
            },
            ReductionPath::SpillWithPointer if already_on_disk => {
                Reduction::DropWithManifest { rerun: None }
            }
            ReductionPath::SpillWithPointer => {
                let digest = content_digest(payload_bytes(result).unwrap_or_default());
                Reduction::SpillWithPointer {
                    digest: digest.clone(),
                    pointer: spill_pointer(&digest, result.size),
                }
            }
            ReductionPath::DispatchReader => self.delegate(result),
        }
    }

    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            table: None,
        }
    }

    pub fn budget(&self) -> Budget {
        self.budget
    }

    /// DESIGN's table, literally. Must consult kind, size, reproducible,
    /// addressable, and pressure — and must NOT spill a payload whose
    /// `provenance.from_spill` is set (claim 3).
    ///
    /// Precedence, and why:
    ///
    /// 1. **Size floor.** A payload that fits goes through, whatever else is
    ///    true of it. This is the constant floor DESIGN keeps under every other
    ///    rule.
    /// 2. **Unbounded shape.** Checked before the storage rows because the shape
    ///    is what makes the payload dangerous; its size is only a symptom. A
    ///    search can return any number of hits, and no amount of re-running
    ///    changes that. Under low pressure a payload no larger than what a
    ///    reader would cost all in (`inline_limit + reader_limit`) is passed
    ///    through instead, because delegating it would spend that much and add a
    ///    subagent on top.
    /// 3. **Large structured.** Sandboxed, and only the reduced result returns.
    /// 4. **Large, reproducible.** Dropped, with the re-run key.
    /// 5. **Large, not reproducible, addressable.** Spilled, with an
    ///    instruction-carrying pointer — unless the payload is already on disk,
    ///    in which case it is dropped with a pointer to the blob it came from.
    /// 6. **Anything else.** Nothing can be re-run and nowhere can it be written,
    ///    so the payload is handed to a reader: the one path that reduces without
    ///    claiming something false.
    pub fn choose(&self, result: &ToolResult, pressure: Pressure) -> Reduction {
        // The policy's table first, so a declared rule wins over the built-in
        // one. `None` means the policy said nothing about these axes, which
        // falls through rather than stopping — a policy expresses the rows it
        // wants to change, not every row.
        if let Some(table) = &self.table {
            let axes = self.axes(result, pressure);
            if let Some(path) = table.path_for(&axes) {
                return self.materialise(path, result);
            }
        }

        if result.size <= self.budget.inline_limit {
            return Reduction::PassThrough;
        }

        // The whole of claim 3, in one predicate: a payload that came from a
        // spill, or that is already sitting on disk, is never spilled again.
        let already_on_disk = result.provenance.from_spill.is_some() || result.payload.is_on_disk();

        if result.kind.is_unbounded() {
            // What delegating would cost anyway: the floor plus the largest
            // answer the reader is allowed to return. While the window is empty,
            // paying that for the payload itself is the cheaper trade — no
            // subagent, no round trip, and the model sees the real hits.
            let affordable = self
                .budget
                .inline_limit
                .saturating_add(self.budget.reader_limit);
            if !pressure.is_high() && result.size <= affordable {
                return Reduction::PassThrough;
            }
            return self.delegate(result);
        }

        if result.kind == ResultKind::Structured {
            return Reduction::SandboxReduce;
        }

        if result.reproducible {
            return Reduction::DropWithManifest {
                rerun: rerun_command(&result.provenance),
            };
        }

        if result.addressable && !already_on_disk {
            let digest = content_digest(payload_bytes(result).unwrap_or_default());
            return Reduction::SpillWithPointer {
                digest: digest.clone(),
                pointer: spill_pointer(&digest, result.size),
            };
        }

        if already_on_disk {
            // Nowhere new to put it and no honest re-run key: point at the blob
            // that already exists rather than writing a second copy of it.
            return Reduction::DropWithManifest { rerun: None };
        }

        self.delegate(result)
    }

    /// Apply the chosen reduction, producing the result the model will see.
    ///
    /// `spill` is called to persist bytes when the path requires it and returns
    /// the digest the store filed them under; an empty return means the store
    /// refused them, and it is unambiguous because no store's digest is empty.
    /// A digest the store assigned is authoritative and replaces the provisional
    /// one computed by [`Reducer::choose`]; a refusal replaces nothing, because
    /// that provisional digest addresses nothing — the payload is passed through
    /// in full instead (see claim 4 in the module docs). `apply` is idempotent
    /// with respect to spilling: a payload that carries no bytes (already
    /// dropped, already spilled) is returned untouched rather than written out
    /// again.
    ///
    /// `DelegateReader` and `SandboxReduce` cannot be completed here — running a
    /// subagent and running a sandbox are the caller's, not the reducer's. What
    /// comes back is the honest dispatch: a [`Payload::Reduced`] that names the
    /// reader and the question and states that the raw payload never entered the
    /// context. When the reader reports, the caller completes the path with
    /// [`Reducer::apply_reader_answer`].
    pub fn apply(
        &self,
        mut result: ToolResult,
        reduction: &Reduction,
        spill: &mut dyn FnMut(&[u8]) -> String,
    ) -> ToolResult {
        match reduction {
            Reduction::PassThrough => result,

            // `apply` is never the road to a replayed call — the loop answers it
            // from the record, without a payload to reduce — but an unreachable
            // arm here would be a panic waiting for the first caller that
            // disagrees. Returning the result untouched is the only answer that
            // claims nothing.
            Reduction::Replayed { .. } => result,

            Reduction::DropWithManifest { rerun } => {
                let note = drop_note(&result, rerun.as_deref());
                let blob = result.provenance.from_spill.clone();
                result.payload = Payload::Dropped(Manifest {
                    omitted_bytes: result.size,
                    rerun: rerun.clone(),
                    note,
                    blob,
                });
                result
            }

            Reduction::SpillWithPointer { .. } => {
                // No bytes to write, or the bytes are already on disk: refuse
                // rather than spill a manifest or a second copy.
                let Some(bytes) = payload_bytes(&result) else {
                    return result;
                };
                let stored = spill(bytes);

                if stored.is_empty() {
                    // The store refused the bytes. The provisional digest the
                    // reduction carries is not an address it can resolve, so a
                    // pointer built from it would send the model to bytes that
                    // are not there while the manifest and the journal both
                    // said "on disk". The bytes are still in hand, and this
                    // path exists only for payloads nothing can re-obtain, so
                    // they are passed through: the message then omits nothing,
                    // claims nothing, and loses nothing.
                    if let Some(text) = result.payload.text().map(str::to_owned) {
                        result.payload = Payload::Inline(text);
                    }
                    return result;
                }

                let pointer = spill_pointer(&stored, result.size);
                result.payload = Payload::Spilled {
                    digest: stored.clone(),
                    pointer,
                    manifest: Manifest {
                        omitted_bytes: result.size,
                        // Nothing needs re-running: the bytes are on disk and
                        // the provenance above still names the call that made
                        // them.
                        rerun: None,
                        note: format!(
                            "The full {} result is on disk, and the provenance records the call that produced it",
                            result.kind.as_str()
                        ),
                        blob: Some(stored),
                    },
                };
                result
            }

            Reduction::DelegateReader { subagent, .. } => {
                let kind = result.kind;
                // This text is OURS, not Claude Code's: Claude Code has no form
                // for a harness-side dispatch notice — its subagent's answer
                // arrives as the tool result itself, with nothing announcing it.
                // It is written as plain prose rather than a bracketed tag like
                // `[delegated]`, because a tag nothing was trained on is the
                // shape this module is replacing.
                let text = format!(
                    "The raw {kind} result ({size} bytes) was handed to the `{subagent}` reader and never entered this context.\nQuestion: {question}\nThe reader's answer follows as its own message. Do not read the raw output yourself — avoiding that cost is the whole point of this path.",
                    kind = kind.as_str(),
                    size = result.size,
                    subagent = subagent,
                    question = default_question(&result),
                );
                let blob = result.provenance.from_spill.clone();
                result.payload = Payload::Reduced {
                    text,
                    manifest: Manifest {
                        omitted_bytes: result.size,
                        rerun: None,
                        note: format!(
                            "delegated to the `{}` shape reader; the raw result is not in this message and the reader's answer is not written yet",
                            subagent
                        ),
                        blob,
                    },
                };
                result
            }

            Reduction::SandboxReduce => {
                let kind = result.kind;
                // Ours, for the same reason the reader dispatch above is: no
                // model was trained on a harness reporting that it sandboxed a
                // payload, and a bracketed tag would pretend otherwise.
                let text = format!(
                    "The {kind} payload ({size} bytes) was routed to the sandbox; only the reduced result comes back, as its own message.\nRead that result, not the raw data — the raw data is not in this context.",
                    kind = kind.as_str(),
                    size = result.size,
                );
                let blob = result.provenance.from_spill.clone();
                result.payload = Payload::Reduced {
                    text,
                    manifest: Manifest {
                        omitted_bytes: result.size,
                        rerun: None,
                        note: format!(
                            "routed to the sandbox for reduction (PTC); the raw {} data is not in this message",
                            kind.as_str()
                        ),
                        blob,
                    },
                };
                result
            }
        }
    }

    /// Complete the reader path: replace the dispatch pointer with what the
    /// reader actually answered.
    ///
    /// The answer is capped at [`Budget::reader_limit`] — that ceiling is what
    /// keeps delegation cheap, and a reader that returns a wall of text has
    /// defeated the path. A cap that fires is recorded in the manifest rather
    /// than applied silently, and the manifest is rebuilt against the answer's
    /// real size, so the accounting stays honest either way.
    pub fn apply_reader_answer(
        &self,
        mut result: ToolResult,
        answer: impl Into<String>,
    ) -> ToolResult {
        let mut text = answer.into();
        let mut note =
            "read by a shape-specialised reader; the unshown bytes are not in this message"
                .to_owned();
        let limit = self.budget.reader_limit;
        if text.len() as u64 > limit {
            text = truncate_at_char_boundary(&text, limit);
            note = format!(
                "read by a shape-specialised reader; the answer exceeded the {limit}-byte reader ceiling and was cut there, so this is a reduction of a reduction"
            );
        }
        let omitted = result.size.saturating_sub(text.len() as u64);
        let blob = result.provenance.from_spill.clone();
        result.payload = Payload::Reduced {
            text,
            manifest: Manifest {
                omitted_bytes: omitted,
                rerun: None,
                note,
                blob,
            },
        };
        result
    }

    /// DESIGN's claim 1: the reader prompt must be SMALL — it is only a reader
    /// and does not need full tool discipline.
    ///
    /// It is generated from the shape and the question, so a new shape costs a
    /// sentence, not a template. Tests assert it stays under 1200 characters for
    /// every kind.
    pub fn reader_prompt(kind: ResultKind, question: &str) -> String {
        let question = truncate_at_char_boundary(question, READER_QUESTION_MAX_BYTES);
        format!(
            "You are a {kind} reader, not a general agent: one question, one answer.\n\
             {note}\n\
             Question: {question}\n\
             Answer only that question, in a few hundred words at most. Quote the exact paths, \
             identifiers or lines you relied on. If the material does not answer it, say \
             `not in this result` instead of guessing, and do not summarise everything you saw.",
            kind = kind.as_str(),
            note = shape_note(kind),
            question = question,
        )
    }

    fn delegate(&self, result: &ToolResult) -> Reduction {
        Reduction::DelegateReader {
            prompt: Self::reader_prompt(result.kind, &default_question(result)),
            subagent: reader_name(result.kind),
        }
    }
}

/// The fixed part of a reader prompt is a sentence per shape; everything else is
/// the caller's question. A caller that hands us a whole file as a question does
/// not get to make the reader expensive: the question is cut to this many bytes
/// on a character boundary, which is what makes claim 1 true rather than
/// aspirational.
const READER_QUESTION_MAX_BYTES: u64 = 400;

fn shape_note(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::File => {
            "You are reading one file. Answer about this file only, and say which lines you used."
        }
        ResultKind::Search => {
            "You are reading a hit list. Hits are unbounded; your answer is not. Report the hits \
             that matter and why the rest do not."
        }
        ResultKind::Log => {
            "You are reading a log. Find the first failure and its cause; do not recite the tail."
        }
        ResultKind::Diff => {
            "You are reading a diff. Report what changed and what is at risk, not the hunks."
        }
        ResultKind::Structured => {
            "You are reading structured data. Answer from its shape and the rows that matter, not \
             by dumping it."
        }
    }
}

/// Shape-specialised, one per kind: DESIGN's "专用读者按形状分工,不是一个通用摘要器".
fn reader_name(kind: ResultKind) -> String {
    format!("{}_reader", kind.as_str())
}

/// A question to hand the reader when the caller named none. It is derived from
/// the provenance, because that is the only thing the result itself carries
/// about why it exists.
fn default_question(result: &ToolResult) -> String {
    let subject = subject_of(&result.provenance);
    match result.kind {
        ResultKind::File => format!("What is in {subject}, and which parts answer the task?"),
        ResultKind::Search => format!("What did {subject} find, and which hits matter?"),
        ResultKind::Log => format!("What failed in {subject}, and what is the first cause?"),
        ResultKind::Diff => format!("What changed in {subject}, and what is at risk?"),
        ResultKind::Structured => format!("What does {subject} contain, and what does it imply?"),
    }
}

fn subject_of(provenance: &Provenance) -> String {
    const KEYS: [&str; 6] = ["command", "pattern", "query", "path", "url", "name"];
    for key in KEYS {
        if let Some(value) = provenance.input.get(key).and_then(|v| v.as_str()) {
            return format!("`{}`", truncate_at_char_boundary(value, 120));
        }
    }
    format!("the `{}` call", provenance.tool)
}

/// The command that would re-obtain a result.
///
/// A tool that was handed a shell command can be re-run by that command
/// verbatim — that is DESIGN's "重跑的命令是什么" at its most literal. Anything
/// else is re-run by re-issuing the call with the same input, which the
/// provenance records in full.
pub fn rerun_command(provenance: &Provenance) -> Option<String> {
    if let Some(command) = provenance.input.get("command").and_then(|v| v.as_str()) {
        return Some(command.to_owned());
    }
    let input = serde_json::to_string(&provenance.input).ok()?;
    Some(format!("{}({input})", provenance.tool))
}

/// The instruction-carrying pointer: where the bytes are, and what to do with
/// them instead of reading them.
///
/// **Ours.** DESIGN's requirement is content, not shape — "隔离必须带指令":
/// the sentence must carry the address, say to delegate a reader, and say not to
/// read the whole file. The address line above it is Claude Code's
/// ("Output too large (…). Full output saved to: …", rendered by
/// [`ToolResult::render`]), but in Claude Code that address is a path the model
/// can open with Read, so the block stands alone; a blob digest is not readable
/// by the executor's tools, and the instruction is the way back in.
///
/// [`ToolResult::render`]: crate::result::ToolResult::render
fn spill_pointer(digest: &str, size: u64) -> String {
    format!(
        "Do not read the whole blob yourself. Delegate: hand `{digest}` to a reader agent and \
         ask it your question — the reader returns the answer, not the file. Reading the blob \
         whole costs {size} bytes of the context this path exists to protect, and the bytes are \
         already on disk, so they will not be spilled again."
    )
}

/// The manifest's prose for a dropped payload: what happened to it and what is
/// not here. The way back — `rerun` or `blob` — is printed by
/// [`ToolResult::render`] from the manifest's own fields, so this note does not
/// repeat it; the two together must still name every field the manifest set.
///
/// [`ToolResult::render`]: crate::result::ToolResult::render
fn drop_note(result: &ToolResult, rerun: Option<&str>) -> String {
    let kind = result.kind.as_str();
    match (result.provenance.from_spill.as_deref(), rerun) {
        (Some(blob), _) => format!(
            "This {kind} result was read back from blob {blob} and was not written out again: the \
             bytes are already on disk at that digest. Delegate a reader to the blob rather than \
             reading it whole."
        ),
        (None, Some(_)) => format!(
            "The {kind} result was dropped, not summarised; nothing of it is in this message."
        ),
        (None, None) => format!(
            "The {kind} result was dropped and no re-run command is known for `{}`; nothing of it \
             is in this message.",
            result.provenance.tool
        ),
    }
}

/// The payload's bytes, when it has any. `Dropped` and `Spilled` carry none —
/// which is the same statement as "there is nothing here to spill again".
fn payload_bytes(result: &ToolResult) -> Option<&[u8]> {
    result.payload.text().map(str::as_bytes)
}

/// A stable, dependency-free content digest (`FNV-1a`, 64 bit).
///
/// The store's digest is authoritative — `apply` adopts whatever the spill
/// closure returns. This one exists so that `choose` can return a self-contained
/// [`Reduction`] that can be journalled *before* anything is written, which is
/// what keeps the reduction an event rather than a side effect. It is never
/// rendered as an address: when the store refuses, the payload is passed through
/// rather than pointed at with this (see claim 4 in the module docs).
fn content_digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn truncate_at_char_boundary(text: &str, limit: u64) -> String {
    let mut end = (limit as usize).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn budget() -> Budget {
        Budget::new(100, 50)
    }

    fn result(kind: ResultKind, size: u64) -> ToolResult {
        ToolResult {
            payload: Payload::Inline("x".repeat(size as usize)),
            kind,
            size,
            reproducible: false,
            addressable: false,
            provenance: Provenance::new("t", json!({})),
        }
    }

    fn no_spill() -> impl FnMut(&[u8]) -> String {
        |_| "store:digest".to_owned()
    }

    #[test]
    fn small_payloads_pass_through_at_any_pressure() {
        let r = result(ResultKind::File, 100);
        let reducer = Reducer::new(budget());
        assert_eq!(reducer.choose(&r, Pressure::low()), Reduction::PassThrough);
        assert_eq!(reducer.choose(&r, Pressure::high()), Reduction::PassThrough);
    }

    #[test]
    fn large_reproducible_file_drops_with_the_rerun_call() {
        let mut r = result(ResultKind::File, 101);
        r.reproducible = true;
        r.addressable = true;
        r.provenance = Provenance::new("read_file", json!({"path": "big.rs"}));
        match Reducer::new(budget()).choose(&r, Pressure::low()) {
            Reduction::DropWithManifest { rerun } => {
                assert_eq!(rerun.as_deref(), Some("read_file({\"path\":\"big.rs\"})"));
            }
            other => panic!("expected a drop, got {other:?}"),
        }
    }

    #[test]
    fn exec_reruns_as_its_own_command() {
        let provenance = Provenance::new("exec", json!({"command": "cargo test --all"}));
        assert_eq!(
            rerun_command(&provenance).as_deref(),
            Some("cargo test --all")
        );
    }

    #[test]
    fn large_irreproducible_addressable_payload_spills() {
        let mut r = result(ResultKind::Structured, 200);
        r.addressable = true;
        match Reducer::new(budget()).choose(&r, Pressure::low()) {
            // Structured is handled before the storage rows by design.
            Reduction::SandboxReduce => {}
            other => panic!("large structured sandboxes, got {other:?}"),
        }

        let mut log = result(ResultKind::File, 200);
        log.addressable = true;
        match Reducer::new(budget()).choose(&log, Pressure::low()) {
            Reduction::SpillWithPointer { digest, pointer } => {
                assert!(digest.starts_with("fnv1a64:"));
                assert!(pointer.contains(&digest));
            }
            other => panic!("expected a spill, got {other:?}"),
        }
    }

    #[test]
    fn unbounded_shapes_delegate_a_reader() {
        let r = result(ResultKind::Search, 200);
        match Reducer::new(budget()).choose(&r, Pressure::high()) {
            Reduction::DelegateReader { prompt, subagent } => {
                assert_eq!(subagent, "search_reader");
                assert!(prompt.contains("which hits matter"), "{prompt}");
            }
            other => panic!("expected a reader, got {other:?}"),
        }
    }

    #[test]
    fn low_pressure_spares_an_unbounded_payload_a_reader_would_have_cost_as_much_as() {
        // Above the floor (100), inside what delegating would have cost
        // (100 + 50), and therefore cheaper to keep than to delegate.
        let r = result(ResultKind::Search, 120);
        let reducer = Reducer::new(budget());
        assert_eq!(reducer.choose(&r, Pressure::low()), Reduction::PassThrough);

        // The same payload once the window is full: delegate, and let the
        // reader pay for the size.
        assert!(matches!(
            reducer.choose(&r, Pressure::high()),
            Reduction::DelegateReader { .. }
        ));

        // Beyond that allowance even an empty window delegates.
        let big = result(ResultKind::Search, 151);
        assert!(matches!(
            reducer.choose(&big, Pressure::low()),
            Reduction::DelegateReader { .. }
        ));
    }

    #[test]
    fn large_structured_goes_to_the_sandbox() {
        let r = result(ResultKind::Structured, 500);
        assert_eq!(
            Reducer::new(budget()).choose(&r, Pressure::low()),
            Reduction::SandboxReduce
        );
    }

    #[test]
    fn claim3_a_payload_read_back_from_a_spill_is_never_spilled() {
        let mut r = result(ResultKind::File, 500);
        r.addressable = true;
        r.reproducible = false;
        r.provenance.from_spill = Some("fnv1a64:abc".to_owned());

        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        assert!(
            !matches!(reduction, Reduction::SpillWithPointer { .. }),
            "read-back must be closed against re-spilling, got {reduction:?}"
        );
        assert!(matches!(
            reduction,
            Reduction::DropWithManifest { rerun: None }
        ));

        // And the rendered payload still points at the blob it came from.
        let mut spill = no_spill();
        let applied = Reducer::new(budget()).apply(r, &reduction, &mut spill);
        assert_eq!(
            applied.payload.manifest().unwrap().blob.as_deref(),
            Some("fnv1a64:abc")
        );
        assert!(applied.render().contains("fnv1a64:abc"));
    }

    #[test]
    fn claim3_an_already_spilled_payload_is_never_spilled_again() {
        let mut r = result(ResultKind::File, 500);
        r.addressable = true;
        r.payload = Payload::Spilled {
            digest: "fnv1a64:dead".to_owned(),
            pointer: "pointer".to_owned(),
            manifest: Manifest {
                omitted_bytes: 500,
                rerun: None,
                note: "n".to_owned(),
                blob: Some("fnv1a64:dead".to_owned()),
            },
        };
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        assert!(!matches!(reduction, Reduction::SpillWithPointer { .. }));

        // `apply` refuses independently of `choose`.
        let mut spilled_bytes = 0usize;
        let mut spill = |b: &[u8]| {
            spilled_bytes += b.len();
            "nope".to_owned()
        };
        let applied = Reducer::new(budget()).apply(
            r,
            &Reduction::SpillWithPointer {
                digest: "fnv1a64:other".to_owned(),
                pointer: "p".to_owned(),
            },
            &mut spill,
        );
        assert_eq!(spilled_bytes, 0, "apply must not write a second copy");
        assert!(matches!(applied.payload, Payload::Spilled { .. }));
        assert_eq!(
            applied.payload.manifest().unwrap().blob.as_deref(),
            Some("fnv1a64:dead")
        );
    }

    #[test]
    fn apply_spills_the_bytes_and_adopts_the_stores_digest() {
        let mut r = result(ResultKind::File, 200);
        r.addressable = true;
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        let mut seen = Vec::new();
        let mut spill = |bytes: &[u8]| {
            seen.extend_from_slice(bytes);
            "sha256:from-the-store".to_owned()
        };
        let applied = Reducer::new(budget()).apply(r, &reduction, &mut spill);
        assert_eq!(seen.len(), 200);
        match &applied.payload {
            Payload::Spilled {
                digest,
                pointer,
                manifest,
            } => {
                assert_eq!(digest, "sha256:from-the-store");
                assert!(pointer.contains("sha256:from-the-store"));
                assert_eq!(manifest.blob.as_deref(), Some("sha256:from-the-store"));
                assert_eq!(manifest.omitted_bytes, 200);
            }
            other => panic!("expected a spill, got {other:?}"),
        }
        assert!(!applied.is_complete());
        assert_eq!(applied.omitted_bytes(), 200);
    }

    #[test]
    fn a_refused_spill_passes_the_payload_through_instead_of_naming_an_address() {
        let mut r = result(ResultKind::File, 200);
        r.addressable = true;
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        let Reduction::SpillWithPointer { digest, .. } = &reduction else {
            panic!("a large, irreproducible, addressable file spills; got {reduction:?}");
        };
        let provisional = digest.clone();

        // The store refused the write: the sink answers with the empty string,
        // which is what the journal-backed sink returns when `BlobStore::put`
        // fails. Nothing is in the store under any digest.
        let mut refused = |_: &[u8]| String::new();
        let applied = Reducer::new(budget()).apply(r, &reduction, &mut refused);

        let text = applied.render();
        assert!(
            !text.contains(&provisional),
            "the model must not be handed the provisional `{provisional}` digest: {text}"
        );
        assert!(!text.contains("fnv1a64:"), "{text}");
        assert!(!text.contains("on disk"), "nothing was stored: {text}");

        // The lossless downgrade: the model has the real bytes, and a payload
        // with every byte present claims nothing was omitted.
        let bytes = "x".repeat(200);
        assert_eq!(applied.payload.text(), Some(bytes.as_str()));
        assert!(applied.is_complete());
        assert_eq!(applied.omitted_bytes(), 0);
        assert_eq!(applied.payload.manifest(), None);
    }

    #[test]
    fn apply_drops_and_the_render_says_so() {
        let mut r = result(ResultKind::Log, 300);
        r.reproducible = true;
        r.provenance = Provenance::new("exec", json!({"command": "cargo test"}));
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        let mut spill = no_spill();
        let applied = Reducer::new(budget()).apply(r, &reduction, &mut spill);
        let text = applied.render();
        assert!(text.contains("300"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(!applied.is_complete());
        assert!(
            !text.contains(&"x".repeat(300)),
            "the payload must not be present"
        );
    }

    #[test]
    fn apply_delegates_the_reader_and_the_pointer_says_so() {
        let r = result(ResultKind::Search, 400);
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        let mut spill = no_spill();
        let applied = Reducer::new(budget()).apply(r, &reduction, &mut spill);
        let text = applied.render();
        assert!(text.contains("search_reader"), "{text}");
        assert!(text.contains("400"), "{text}");
        assert!(text.contains("Do not read"), "{text}");
        assert!(!applied.is_complete());
    }

    #[test]
    fn apply_reader_answer_rebuilds_the_accounting() {
        let r = result(ResultKind::Log, 400);
        let reduction = Reducer::new(budget()).choose(&r, Pressure::high());
        let mut spill = no_spill();
        let shown = Reducer::new(budget()).apply(r, &reduction, &mut spill);
        let answered = Reducer::new(budget()).apply_reader_answer(shown, "one failure: bad port");
        match &answered.payload {
            Payload::Reduced { text, manifest } => {
                assert_eq!(text, "one failure: bad port");
                assert_eq!(manifest.omitted_bytes, 400 - text.len() as u64);
            }
            other => panic!("expected a reduction, got {other:?}"),
        }
        assert!(answered.render().contains("one failure: bad port"));
    }

    #[test]
    fn apply_reader_answer_enforces_the_ceiling_and_records_it() {
        let r = result(ResultKind::Log, 400);
        let answered = Reducer::new(budget()).apply_reader_answer(r, "y".repeat(200));
        let text = answered.render();
        assert!(text.contains("reader ceiling"), "{text}");
        assert_eq!(answered.payload.text().unwrap().len(), 50);
    }

    #[test]
    fn reader_prompt_is_small_for_every_kind() {
        for kind in ResultKind::ALL {
            let prompt = Reducer::reader_prompt(kind, "what is the first failure?");
            assert!(
                prompt.len() < 1200,
                "{} reader prompt is {} chars",
                kind.as_str(),
                prompt.len()
            );
            assert!(prompt.contains(kind.as_str()));
        }
    }

    #[test]
    fn a_giant_question_cannot_inflate_the_reader_prompt() {
        let question = "q".repeat(50_000);
        for kind in ResultKind::ALL {
            let prompt = Reducer::reader_prompt(kind, &question);
            assert!(
                prompt.len() < 1200,
                "{}: {} chars",
                kind.as_str(),
                prompt.len()
            );
        }
    }

    #[test]
    fn the_reader_prompt_is_generated_from_shape_and_question() {
        let a = Reducer::reader_prompt(ResultKind::Search, "which files match?");
        let b = Reducer::reader_prompt(ResultKind::Log, "which files match?");
        assert_ne!(a, b, "each shape gets its own reader instructions");
        assert!(a.contains("which files match?"));
    }

    #[test]
    fn rerun_key_is_derived_from_provenance_only_when_it_exists() {
        assert_eq!(
            rerun_command(&Provenance::new("exec", json!({"command": "ls"}))).as_deref(),
            Some("ls")
        );
        assert_eq!(
            rerun_command(&Provenance::new("list_dir", json!({"path": "."}))).as_deref(),
            Some("list_dir({\"path\":\".\"})")
        );
    }

    #[test]
    fn pressure_is_clamped_and_ordered() {
        assert!(!Pressure::low().is_high());
        assert!(Pressure::high().is_high());
        assert!(Pressure::with_fill(0.9).is_high());
        assert!(!Pressure::with_fill(0.5).is_high());
        assert_eq!(Pressure::with_fill(7.0).fill, 1.0);
        assert_eq!(Pressure::with_fill(-7.0).fill, 0.0);
        assert_eq!(Pressure::with_fill(f64::NAN), Pressure::low());
    }

    #[test]
    fn content_digest_is_stable_and_size_sensitive() {
        assert_eq!(content_digest(b"abc"), content_digest(b"abc"));
        assert_ne!(content_digest(b"abc"), content_digest(b"abd"));
        assert!(content_digest(b"abc").starts_with("fnv1a64:"));
    }
}
