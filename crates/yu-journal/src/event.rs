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
    Effect { key: String, description: String },
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

// `yu_observe::TokenUsage` is an accounting type and carries no `PartialEq`, so
// equality is written out here instead of derived: two events are equal exactly
// when every recorded fact is equal. Deriving on the rest would be wrong anyway
// — a new field must force a decision here, not silently change what "same
// event" means.
impl PartialEq for EventBody {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                EventBody::SessionStarted {
                    charter: c1,
                    model: m1,
                    quirks: b1,
                },
                EventBody::SessionStarted {
                    charter: c2,
                    model: m2,
                    quirks: b2,
                },
            ) => c1 == c2 && m1 == m2 && b1 == b2,
            (EventBody::UserMessage { text: a }, EventBody::UserMessage { text: b }) => a == b,
            (
                EventBody::ModelRequest {
                    model: m1,
                    messages: n1,
                    digest: d1,
                    est_input_tokens: t1,
                },
                EventBody::ModelRequest {
                    model: m2,
                    messages: n2,
                    digest: d2,
                    est_input_tokens: t2,
                },
            ) => m1 == m2 && n1 == n2 && d1 == d2 && t1 == t2,
            (
                EventBody::ModelResponse {
                    id: i1,
                    stop_reason: s1,
                    usage: u1,
                    blocks: b1,
                    thinking: t1,
                    text_blob: x1,
                },
                EventBody::ModelResponse {
                    id: i2,
                    stop_reason: s2,
                    usage: u2,
                    blocks: b2,
                    thinking: t2,
                    text_blob: x2,
                },
            ) => {
                i1 == i2
                    && s1 == s2
                    && same_usage(u1, u2)
                    && b1 == b2
                    && t1 == t2
                    && x1 == x2
            }
            (
                EventBody::ToolCall {
                    tool_use_id: i1,
                    name: n1,
                    input: v1,
                },
                EventBody::ToolCall {
                    tool_use_id: i2,
                    name: n2,
                    input: v2,
                },
            ) => i1 == i2 && n1 == n2 && v1 == v2,
            (
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
                },
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
                },
            ) => {
                i1 == i2
                    && n1 == n2
                    && k1 == k2
                    && y1 == y2
                    && r1 == r2
                    && a1 == a2
                    && l1 == l2
                    && b1 == b2
                    && d1 == d2
            }
            (
                EventBody::Injected {
                    label: l1,
                    blob: b1,
                },
                EventBody::Injected {
                    label: l2,
                    blob: b2,
                },
            ) => l1 == l2 && b1 == b2,
            (EventBody::Note { text: a }, EventBody::Note { text: b }) => a == b,
            (
                EventBody::Effect {
                    key: k1,
                    description: d1,
                },
                EventBody::Effect {
                    key: k2,
                    description: d2,
                },
            ) => k1 == k2 && d1 == d2,
            _ => false,
        }
    }
}

fn same_usage(a: &TokenUsage, b: &TokenUsage) -> bool {
    a.input_tokens == b.input_tokens
        && a.output_tokens == b.output_tokens
        && a.cache_read_input_tokens == b.cache_read_input_tokens
        && a.cache_creation_input_tokens == b.cache_creation_input_tokens
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

    #[test]
    fn kind_matches_the_serialized_tag() {
        // The tag is what shows up in the file; `kind()` is what observation
        // counts. If they ever drift, the metrics lie.
        let bodies = vec![
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
            EventBody::Effect {
                key: "k".into(),
                description: "d".into(),
            },
        ];
        for body in bodies {
            let value = serde_json::to_value(&body).unwrap();
            assert_eq!(value["event"], json!(body.kind()));
        }
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
