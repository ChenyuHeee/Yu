//! L2 — the record.
//!
//! DESIGN: the record is append-only, typed, indexable, and *addressable*. It is
//! never "remembered", only "readable". A summary claims to be the content; an
//! index does not.
//!
//! Four engineering constraints DESIGN names, all of which differ from how
//! existing harnesses store things:
//!
//! 1. Results are not stored twice. (Claude Code's transcript duplicates every
//!    `tool_result` — once inside the message, once in a top-level field.)
//!    Enforced by [`Journal::append`]: a `ToolResult` body with both `inline` and
//!    `blob` is rejected as [`JournalError::PayloadStoredTwice`].
//! 2. The envelope is not repeated per content block. (Today: one JSONL line per
//!    block, each rewriting uuid/timestamp/cwd/gitBranch.) One `Event` is one
//!    line; blocks are references inside it.
//! 3. Re-sent material is deduplicated by reference. (`task_reminder`-style
//!    injections are otherwise re-sent dozens of times.) See [`EventBody::injected`]
//!    and the idempotent [`BlobStore::put`].
//! 4. `signature` is stored separately from thinking text — measured on this
//!    endpoint, a signature is a 36-byte UUID while thinking text can be shorter.
//!    DESIGN's rule holds: the signature can be larger than the body. There is no
//!    field in which the two could be joined.
//!
//! L2 doubles as the L3 journal: one mechanism, two uses — the basis for replay
//! ([`EffectLog`], [`EffectGuard`]) and the carrier of memory.

#![forbid(unsafe_code)]

pub mod blob;
pub mod effect;
pub mod event;
pub mod log;
pub mod reader;

pub use blob::{BlobStore, digest_of};
pub use effect::{EffectGuard, EffectLog};
pub use event::{EffectState, Event, EventBody, SessionId, ThinkingRecord};
pub use log::Journal;
pub use reader::JournalIndex;

/// Everything this crate can fail with.
///
/// The variants stay specific because a record's failure modes are its meaning:
/// "the file is not there", "the file is there and does not decode", and "the
/// reference points at nothing" are different facts about the world, and a caller
/// that cannot tell them apart cannot decide whether to retry, stop, or go ask a
/// human.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Serde(#[from] serde_json::Error),

    /// A reference with no file behind it. Content addressing means this cannot
    /// be a stale name — the blob was removed, or was never written.
    #[error("blob {digest} is not in the store")]
    BlobMissing { digest: String },

    /// Not a sha256 hex string, and therefore not a reference. Rejected before it
    /// is ever joined to a path, so a reference can never address anything outside
    /// the store.
    #[error("`{digest}` is not a sha256 digest of 64 hex characters")]
    InvalidDigest { digest: String },

    /// The byte offset says where to look, so the damage can be inspected rather
    /// than guessed at.
    #[error("record at byte {offset} does not decode: {reason}")]
    Corrupt { offset: u64, reason: String },

    /// A blob that is not text was asked for as text. Reported instead of being
    /// lossily replaced, because the caller's next move differs.
    #[error("blob {digest} is not valid utf-8")]
    NotUtf8 { digest: String },

    /// The file names one session and was opened as another. Two records must
    /// never be mixed silently.
    #[error("journal file holds session `{found}` but was opened as `{expected}`")]
    SessionMismatch { found: String, expected: String },

    #[error("a session id must not be empty")]
    EmptySessionId,

    /// L2 constraint 1. A `ToolResult` carrying both `inline` and `blob` is the
    /// duplication DESIGN names, and it is refused rather than accepted as a
    /// style choice.
    #[error("tool result carries both `inline` and `blob`; a payload is stored exactly once")]
    PayloadStoredTwice,
}
