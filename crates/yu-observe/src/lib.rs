//! Observation surface.
//!
//! DESIGN: "观测(第一天就有)". Two things flower could never measure live here:
//!
//! * every event carries a `ts`, so no downstream timing needs to be bolted on
//!   from outside;
//! * **per-step overhead with model time subtracted** — the cost of the harness
//!   itself, as distinct from the cost of waiting for a model.
//!
//! This crate is a leaf. It must not depend on any other `yu-*` crate, so that
//! the wire layer can time itself without creating a cycle.

pub mod alloc;
pub mod metrics;
pub mod shape;
pub mod span;

pub use alloc::{AgentScope, Allocator};
pub use metrics::{AgentMetrics, ContextComposition, Metrics, TokenUsage};
pub use shape::{REPEAT_THRESHOLD, ShapeSignal, ShapeTracker, StepObservation};
pub use span::{AgentId, Span, SpanKind, SpanTimer, Timeline};

/// Milliseconds since the Unix epoch.
///
/// Wall-clock rather than monotonic, because journals outlive processes and a
/// `ts` has to stay comparable across runs. Durations come from `Instant`.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// RFC 3339 timestamp, microsecond precision, for human-readable journal fields.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
