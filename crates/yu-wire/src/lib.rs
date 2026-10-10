//! Messages API wire layer.
//!
//! DESIGN: Rust has no official Anthropic SDK, so this must be hand-written —
//! and "quirk 层躲不掉". Measured facts about the endpoint this is built
//! against live in `docs/WIRE-FACTS.md`; `quirks` is the module that encodes them.
//!
//! Layout:
//!
//! * [`types`] — request/response/content types.
//! * [`sse`] — SSE parse and content-block accumulation.
//! * [`client`] — HTTP, streaming, retry.
//! * [`error`] — error classification, including the classes DESIGN insists be
//!   exhausted (`stop_reason`, refusal, `max_tokens`, `pause_turn`).
//! * [`quirks`] — per-endpoint behavioural divergences.
//! * [`capability`] — what the endpoint actually supports, measured not assumed.
//! * [`count_tokens`] — local counting via `POST /v1/messages/count_tokens`.

pub mod capability;
pub mod client;
pub mod count_tokens;
pub mod error;
pub mod quirks;
pub mod sse;
pub mod types;

pub use client::WireClient;
pub use error::{ErrorKind, WireError};
pub use quirks::Quirks;
pub use types::{Message, StopReason, Usage};
