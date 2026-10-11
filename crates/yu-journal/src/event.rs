//! Typed events.
//!
//! One event is one JSONL line (constraint 2). The envelope — `seq`, `ts_ms`,
//! `session`, `agent` — is written once per event, however many content blocks
//! that event covers; blocks are *references* inside the line, not further
//! lines. That is the difference between a record with an address and a flat
//! transcript, and it is what makes `render()` able to say what it left out.

use serde::{Deserialize, Serialize};

use yu_observe::{AgentId, TokenUsage};

use crate::JournalError;
use crate::blob::BlobStore;

/// How far a recorded side effect got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    /// Recorded before performing. If the record ends here, the effect was
    /// interrupted in the one window this discipline exists for — and the
    /// correct reading is *unknown*, not *done*.
    Intended,
    /// Completed, with the seq of whatever it produced if that was recorded.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_seq: Option<u64>,
    },
    /// Deliberately not performed. Distinct from `Intended`, which is ambiguous.
    Abandoned,
}

impl EffectState {
    /// Whether a replay may skip this effect's side effect.
    ///
    /// Only `Done` may be skipped. `Intended` must not be: it is the ambiguous
    /// case, and treating it as complete is how an interrupted effect vanishes
    /// from a replay while looking finished.
    pub fn may_skip(&self) -> bool {
        matches!(self, EffectState::Done { .. })
    }
}

/// One thinking block, its text and its signature held apart.
///
/// CONSTRAINT 4, corrected. The first version of this stored the *signature*
/// inline on the JSONL line and the thinking text in a blob — exactly backwards.
/// Measured here the signature is a 36-byte UUID, which is why it looked
/// harmless; on a real Anthropic endpoint signatures are kilobytes, and inlining
/// them bloats the one line that L2 constraint 2 exists to keep small. That the
/// shim's signature is small is a fact about the shim, not about the format this
/// record has to outlive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingRecord {
    /// Digest of the thinking text.
    pub text_blob: String,
    /// Digest of the signature, held separately because it can outweigh the text
    /// it accompanies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_blob: Option<String>,
}

/// Identifies one session's record.
///
/// It is part of every event's envelope rather than of the file it happens to
/// sit in: DESIGN's L3 wants any historical point reconstructible, and a fork
/// copies references across session boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One recorded event.
///
/// `seq` comes from the journal, never from the caller — it is the address that
/// `read_from` and the seekable index turn into a byte offset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    /// Wall clock, milliseconds since the epoch, written by the journal.
    ///
    /// DESIGN: "每个 `Event` 带 `ts`" — flower's events had none, which is why
    /// every downstream timing there had to be bolted on from outside.
    pub ts_ms: i64,
    pub session: SessionId,
    pub agent: AgentId,
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventBody {
    /// L0 intent. Constraint 3 applies to how it is *re-injected* each turn, not
    /// to this first recording: the charter is stated once, here.
    SessionStarted {
        charter: String,
        model: String,
        quirks: String,
    },
    /// The human's own words, kept verbatim. DESIGN: 人类原话原样存活.
    UserMessage { text: String },
    /// `digest` is of the rendered request, so a re-sent identical view is
    /// detectable — which is also the byte-level fact prefix caching turns on
    /// (`docs/WIRE-FACTS.md` Quirk 3).
    ModelRequest {
        model: String,
        messages: usize,
        digest: String,
        est_input_tokens: Option<u64>,
    },
    /// Constraint 4: the thinking text and its signature are held separately,
    /// each as its own blob, and are never folded together. The signature is
    /// replayed verbatim (Quirk 1), so it must survive; and it can be kilobytes
    /// on a real endpoint, so it must not sit inline on the line.
    ///
    /// `blocks` holds one reference per content block, in wire order — a blob
    /// digest or a short inline string. Keeping them as references is what keeps
    /// this event at one line no matter how many blocks a turn contains.
    ModelResponse {
        id: String,
        stop_reason: String,
        usage: TokenUsage,
        /// Ordered content-block type names — the shape of the turn.
        blocks: Vec<String>,
        /// Ordered thinking, each block with its own text and signature.
        ///
        /// A list rather than a concatenation, for two reasons that both bite.
        /// The endpoint interleaves `thinking → text → server_tool_use → …
        /// → thinking` (WIRE-FACTS), so collapsing them loses the order; and
        /// Quirk 1 requires thinking to be replayed byte-exact, so welding two
        /// blocks together attaches one block's signature to another's text —
        /// which replays as a corrupted turn.
        #[serde(default)]
        thinking: Vec<ThinkingRecord>,
        /// The assistant's own words, as a blob digest.
        ///
        /// This is the most important product of a turn, and an earlier version
        /// recorded only the block *type names* — `["thinking","text"]` — so the
        /// text never reached the record at all. Three promises rest on it: L2's
        /// completeness, the decision layer's "每轮最终答复", and the next
        /// generation's verifier reading the primary record rather than a report.
        #[serde(default)]
        text_blob: Option<String>,
    },
    ToolCall {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Constraint 1: the payload appears exactly once, via `inline` XOR `blob`.
    ///
    /// Neither field is set when a reduction dropped the payload on purpose —
    /// DESIGN's "丢 + 清单" path — in which case `reduction` carries the note
    /// that says so. A dropped payload is an explicit event; a duplicated one is
    /// a bug.
    ToolResult {
        tool_use_id: String,
        name: String,
        /// `file` / `search` / `log` / `diff` / `structured` — what the payload
        /// is shaped like, which is what a reduction decision is made on.
        kind: String,
        bytes: u64,
        /// Whether re-running the tool would produce the same payload.
        reproducible: bool,
        /// Whether the payload can be addressed and read back later.
        addressable: bool,
        inline: Option<String>,
        blob: Option<String>,
        reduction: Option<String>,
    },
    /// Constraint 3: re-sent material goes in `blob`, referenced by digest.
    ///
    /// The injection point recurs every turn; the bytes must not. Use
    /// [`EventBody::injected`] so the store answers the dedup question.
    Injected { label: String, blob: String },
    Note { text: String },
    /// L3 "先记后做" (record before acting): the intent is in the record before
    /// the side effect happens, so a replay of this branch reads the recorded
    /// result instead of performing the effect again.
    /// This journal branches from another one.
    ///
    /// Recorded once, as the new branch's first event. DESIGN: "fork 用引用而非
    /// 拷贝 —— 成本从 O(记录) 降到 O(1)", so this is the *whole* storage cost of a
    /// fork: a pointer, not a copy. Everything before `parent_at_seq` belongs to
    /// the parent and is read through it.
    ///
    /// The cache consequence is part of the record rather than something a caller
    /// discovers later: DESIGN is explicit that "fork 的代价必须显式:从分叉点起
    /// 缓存前缀不再匹配". A fork is not a free debugging button.
    Forked {
        parent: SessionId,
        parent_at_seq: u64,
    },

    /// One agent derived another, and this is the edge.
    ///
    /// The **parent is the recording agent** — the event's own envelope `agent`
    /// — so a reader draws the tree from edges rather than inferring it from
    /// timestamps and task text, which is what an untyped `Note` would have
    /// forced. DESIGN's lineage requirement ("血缘完整") is about work: who asked
    /// whom to do what.
    ///
    /// Not a [`EventBody::Forked`], deliberately. A fork branches a *record* at
    /// a point in its own history so a replay can diverge, and DESIGN makes its
    /// cost explicit ("从分叉点起缓存前缀不再匹配"). A delegation branches *work*:
    /// the child's events continue this record, and nothing about the cache is
    /// owed to it. Keeping the two apart is what stops every spawn from
    /// implying a cache invalidation it does not cause.
    Delegated {
        /// The agent that will run the derived task.
        child: AgentId,
        /// Which delegation level the child runs at. 1 is the decision layer's
        /// own delegation; the ceiling is `MAX_DELEGATION_DEPTH` in `yu-core`.
        depth: u32,
        /// The task the child was given, as it was given.
        task: String,
    },

    /// A side effect, recorded before it happens ("先记后做").
    ///
    /// Carries its **state**, because an event's existence alone cannot say
    /// whether the effect happened. The window this discipline exists to cover
    /// is precisely the one between recording the intent and performing the act,
    /// so a record that cannot distinguish "about to" from "did" makes the one
    /// failure it was built to catch indistinguishable from success — and a
    /// replay seed built from it would treat an interrupted effect as finished
    /// and skip it silently.
    Effect {
        key: String,
        description: String,
        state: EffectState,
        /// The tool call this effect was performed for, when there was one, so
        /// the intent and its result can be tied together.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
    },
}

impl EventBody {
    /// The type as it is spelled in the file. Observation and shape metrics
    /// count by this, so it must match the serde tag exactly.
    pub fn kind(&self) -> &'static str {
        match self {
            EventBody::SessionStarted { .. } => "session_started",
            EventBody::UserMessage { .. } => "user_message",
            EventBody::ModelRequest { .. } => "model_request",
            EventBody::ModelResponse { .. } => "model_response",
            EventBody::ToolCall { .. } => "tool_call",
            EventBody::ToolResult { .. } => "tool_result",
            EventBody::Injected { .. } => "injected",
            EventBody::Note { .. } => "note",
            EventBody::Effect { .. } => "effect",
            EventBody::Forked { .. } => "forked",
            EventBody::Delegated { .. } => "delegated",
        }
    }

    /// Record material that is being injected into a context — a system prompt,
    /// a task reminder. Stored once, referenced by digest (constraint 3).
    ///
    /// This exists so that re-injection cannot be spelled as an inline string:
    /// inlining would rewrite the whole body into the log every turn it is sent.
    pub fn injected(
        label: impl Into<String>,
        text: &str,
        blobs: &BlobStore,
    ) -> Result<Self, JournalError> {
        Ok(EventBody::Injected {
            label: label.into(),
            blob: blobs.put_str(text)?,
        })
    }

    /// Constraint 1, enforced where events enter the record.
    ///
    /// Both fields set is the shape Claude Code's transcript has — the payload
    /// once inside the message, once in a top-level field. It is rejected rather
    /// than tolerated, because tolerating it is how the duplication became
    /// normal in the first place.
    pub(crate) fn check_stored_once(&self) -> Result<(), JournalError> {
        match self {
            EventBody::ToolResult {
                inline: Some(_),
                blob: Some(_),
                ..
            } => Err(JournalError::PayloadStoredTwice),
            _ => Ok(()),
        }
    }
}

// Equality is written out here instead of derived: two events are equal exactly
// when every recorded fact is equal, and that claim deserves one place to be
// made — a new field must force a decision here, not silently change what "same
// event" means. (`usage` is compared by `TokenUsage`'s own `PartialEq`; it is an
// accounting type whose fields are all recorded facts, so a field added there is
// counted rather than missed.)
//
// The match is on `self` alone, and that is load-bearing rather than stylistic.
// It used to be on `(self, other)` and to end in `_ => false`, and `Forked` —
// added later — was given no arm, so it fell into that catch-all and
// `Forked == Forked` was *false*. A wildcard cannot tell "these are two different
// variants" from "nobody wrote an arm for this one": the first is the answer
// "unequal", the second is a comparison that silently lost a case, and the arm
// was there for the first. Matching one side exhaustively hands the second case
// to the compiler — a new variant is a non-exhaustive match, and a new field is a
// missing binding in the arm *and* in the `other` pattern inside it. Cross-variant
// pairs are not an arm at all: they are the `other` pattern failing to match,
// which is the only place "not equal" can be said without also swallowing the
// same-variant case.
impl PartialEq for EventBody {
    fn eq(&self, other: &Self) -> bool {
        match self {
            EventBody::SessionStarted {
                charter: c1,
                model: m1,
                quirks: b1,
            } => matches!(
                other,
                EventBody::SessionStarted {
                    charter: c2,
                    model: m2,
                    quirks: b2,
                } if c1 == c2 && m1 == m2 && b1 == b2
            ),
            EventBody::UserMessage { text: a } => {
                matches!(other, EventBody::UserMessage { text: b } if a == b)
            }
            EventBody::ModelRequest {
                model: m1,
                messages: n1,
                digest: d1,
                est_input_tokens: t1,
            } => matches!(
                other,
                EventBody::ModelRequest {
                    model: m2,
                    messages: n2,
                    digest: d2,
                    est_input_tokens: t2,
                } if m1 == m2 && n1 == n2 && d1 == d2 && t1 == t2
            ),
            EventBody::ModelResponse {
                id: i1,
                stop_reason: s1,
                usage: u1,
                blocks: b1,
                thinking: t1,
                text_blob: x1,
            } => matches!(
                other,
                EventBody::ModelResponse {
                    id: i2,
                    stop_reason: s2,
                    usage: u2,
                    blocks: b2,
                    thinking: t2,
                    text_blob: x2,
                } if i1 == i2
                    && s1 == s2
                    && u1 == u2
                    && b1 == b2
                    && t1 == t2
                    && x1 == x2
            ),
            EventBody::ToolCall {
                tool_use_id: i1,
                name: n1,
                input: v1,
            } => matches!(
                other,
                EventBody::ToolCall {
                    tool_use_id: i2,
                    name: n2,
                    input: v2,
                } if i1 == i2 && n1 == n2 && v1 == v2
            ),
            EventBody::ToolResult {
                tool_use_id: i1,
                name: n1,
                kind: k1,
                bytes: y1,
                reproducible: r1,
                addressable: a1,
                inline: l1,
                blob: b1,
                reduction: d1,
            } => matches!(
                other,
                EventBody::ToolResult {
                    tool_use_id: i2,
                    name: n2,
                    kind: k2,
                    bytes: y2,
                    reproducible: r2,
                    addressable: a2,
                    inline: l2,
                    blob: b2,
                    reduction: d2,
                } if i1 == i2
                    && n1 == n2
                    && k1 == k2
                    && y1 == y2
                    && r1 == r2
                    && a1 == a2
                    && l1 == l2
                    && b1 == b2
                    && d1 == d2
            ),
            EventBody::Injected {
                label: l1,
                blob: b1,
            } => matches!(
                other,
                EventBody::Injected {
                    label: l2,
                    blob: b2,
                } if l1 == l2 && b1 == b2
            ),
            EventBody::Note { text: a } => {
                matches!(other, EventBody::Note { text: b } if a == b)
            }
            EventBody::Forked {
                parent: p1,
                parent_at_seq: q1,
            } => matches!(
                other,
                EventBody::Forked {
                    parent: p2,
                    parent_at_seq: q2,
                } if p1 == p2 && q1 == q2
            ),
            EventBody::Effect {
                key: k1,
                description: d1,
                state: s1,
                tool_use_id: t1,
            } => matches!(
                other,
                EventBody::Effect {
                    key: k2,
                    description: d2,
                    state: s2,
                    tool_use_id: t2,
                } if k1 == k2 && d1 == d2 && s1 == s2 && t1 == t2
            ),
            EventBody::Delegated {
                child: c1,
                depth: p1,
                task: t1,
            } => matches!(
                other,
                EventBody::Delegated {
                    child: c2,
                    depth: p2,
                    task: t2,
                } if c1 == c2 && p1 == p2 && t1 == t2
            ),
        }
    }
}

/// The two fields the index needs from a line.
///
/// Decoding a whole event to index it would mean re-parsing every giant payload
/// in the file, so the index reads only the header — and the session comes along
/// with `seq` because it is free here and lets `Journal::open` notice a file that
/// is being reopened under the wrong session id.
#[derive(Deserialize)]
pub(crate) struct EventHead {
    pub(crate) seq: u64,
    pub(crate) session: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input_tokens: u64) -> TokenUsage {
        TokenUsage {
            input_tokens,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 0,
        }
    }

    /// One body of every variant.
    ///
    /// The failure mode this list exists for — an arm missing from the
    /// hand-written `PartialEq` — is invisible from any single case, so a test
    /// that has to hold "for every variant" takes the list from here instead of
    /// carrying its own, where the missing one would be missing too.
    fn one_of_each() -> Vec<EventBody> {
        vec![
            EventBody::SessionStarted {
                charter: "c".into(),
                model: "m".into(),
                quirks: "q".into(),
            },
            EventBody::UserMessage { text: "hi".into() },
            EventBody::ModelRequest {
                model: "m".into(),
                messages: 1,
                digest: "d".into(),
                est_input_tokens: None,
            },
            EventBody::ModelResponse {
                id: "id".into(),
                stop_reason: "end_turn".into(),
                usage: usage(1),
                blocks: vec![],
                thinking: Vec::new(),
                text_blob: None,
            },
            EventBody::ToolCall {
                tool_use_id: "t".into(),
                name: "read".into(),
                input: json!({}),
            },
            EventBody::ToolResult {
                tool_use_id: "t".into(),
                name: "read".into(),
                kind: "file".into(),
                bytes: 1,
                reproducible: true,
                addressable: true,
                inline: Some("x".into()),
                blob: None,
                reduction: None,
            },
            EventBody::Injected {
                label: "l".into(),
                blob: "b".into(),
            },
            EventBody::Note { text: "n".into() },
            EventBody::Forked {
                parent: SessionId::new("parent"),
                parent_at_seq: 3,
            },
            EventBody::Effect {
                key: "k".into(),
                description: "d".into(),
                state: EffectState::Intended,
                tool_use_id: None,
            },
        ]
    }

    #[test]
    fn kind_matches_the_serialized_tag() {
        // The tag is what shows up in the file; `kind()` is what observation
        // counts. If they ever drift, the metrics lie.
        for body in one_of_each() {
            let value = serde_json::to_value(&body).unwrap();
            assert_eq!(value["event"], json!(body.kind()), "{body:?}");
        }
    }

    /// No variant compares unequal to itself.
    ///
    /// This is the property a missing arm breaks, and the one the wildcard arm
    /// hid: `Forked` had no arm, so it fell through to `_ => false` and a branch
    /// did not equal itself. Checked over every variant rather than the one that
    /// happened to be missing, so the next variant added is covered by the same
    /// guard instead of needing its own.
    #[test]
    fn no_variant_compares_unequal_to_itself() {
        for body in one_of_each() {
            assert_eq!(body, body.clone(), "{body:?} is not equal to itself");
        }
    }

    /// `Forked` compares the fields it records, and nothing else.
    ///
    /// Both recorded fields matter: the parent names the record being extended
    /// and the seq is the point it was extended from, so a fork read back from
    /// disk must compare equal to the one that was written, and two different
    /// forks must not.
    #[test]
    fn a_fork_is_equal_to_itself_and_to_no_other_fork() {
        let fork = EventBody::Forked {
            parent: SessionId::new("parent"),
            parent_at_seq: 3,
        };
        assert_eq!(fork, fork.clone());
        assert_eq!(
            fork,
            EventBody::Forked {
                parent: SessionId::new("parent"),
                parent_at_seq: 3,
            }
        );
        assert_ne!(
            fork,
            EventBody::Forked {
                parent: SessionId::new("other"),
                parent_at_seq: 3,
            }
        );
        assert_ne!(
            fork,
            EventBody::Forked {
                parent: SessionId::new("parent"),
                parent_at_seq: 4,
            }
        );
        // A different variant is still unequal — the answer the wildcard arm was
        // supposed to give, kept while the same-variant cases stopped reaching it.
        assert_ne!(fork, EventBody::Note { text: "n".into() });
    }

    #[test]
    fn bodies_round_trip_through_json() {
        let body = EventBody::ModelResponse {
            id: "m1".into(),
            stop_reason: "tool_use".into(),
            usage: usage(17),
            blocks: vec!["a".into(), "b".into()],
            thinking: vec![ThinkingRecord {
                text_blob: "f".repeat(64),
                signature_blob: Some("a".repeat(64)),
            }],
            text_blob: Some("e".repeat(64)),
        };
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(serde_json::from_str::<EventBody>(&text).unwrap(), body);
    }

    #[test]
    fn equality_is_field_wise_including_usage() {
        let a = EventBody::ModelResponse {
            id: "m".into(),
            stop_reason: "end_turn".into(),
            usage: usage(5),
            blocks: vec![],
            thinking: Vec::new(),
            text_blob: None,
        };
        let mut b = a.clone();
        assert_eq!(a, b);
        if let EventBody::ModelResponse { usage, .. } = &mut b {
            usage.input_tokens += 1;
        }
        assert_ne!(a, b);
        assert_ne!(a, EventBody::Note { text: "m".into() });
    }

    #[test]
    fn injected_puts_the_body_in_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(dir.path()).unwrap();
        let body = EventBody::injected("task_reminder", "remember the budget", &blobs).unwrap();
        let EventBody::Injected { label, blob } = &body else {
            panic!("expected Injected, got {body:?}");
        };
        assert_eq!(label, "task_reminder");
        assert_eq!(blobs.get_str(blob).unwrap(), "remember the budget");
    }

    #[test]
    fn a_payload_in_two_places_is_rejected() {
        let both = EventBody::ToolResult {
            tool_use_id: "t".into(),
            name: "read".into(),
            kind: "file".into(),
            bytes: 1,
            reproducible: true,
            addressable: true,
            inline: Some("small".into()),
            blob: Some("f".repeat(64)),
            reduction: None,
        };
        assert!(matches!(
            both.check_stored_once(),
            Err(JournalError::PayloadStoredTwice)
        ));
        // Inline only, blob only, or neither (a payload dropped on purpose with a
        // reduction note) are all legitimate.
        for (inline, blob) in [
            (Some("s".to_owned()), None),
            (None, Some("f".repeat(64))),
            (None, None),
        ] {
            let body = EventBody::ToolResult {
                tool_use_id: "t".into(),
                name: "read".into(),
                kind: "file".into(),
                bytes: 1,
                reproducible: true,
                addressable: true,
                inline,
                blob,
                reduction: Some("dropped: reproducible, re-run `cargo test`".into()),
            };
            assert!(body.check_stored_once().is_ok());
        }
    }

    #[test]
    fn a_line_is_decoded_into_a_head_without_its_payload() {
        let event = Event {
            seq: 7,
            ts_ms: 1,
            session: SessionId::new("s"),
            agent: yu_observe::AgentId::new("a"),
            body: EventBody::Note {
                text: "x".repeat(4096),
            },
        };
        let line = serde_json::to_string(&event).unwrap();
        let head: EventHead = serde_json::from_str(&line).unwrap();
        assert_eq!(head.seq, 7);
        assert_eq!(head.session.as_deref(), Some("s"));
    }
}
