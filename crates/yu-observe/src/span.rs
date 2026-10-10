//! Spans.
//!
//! DESIGN's span list: `assemble / wire_wait / first_token / stream / tool_exec /
//! spawn / init`. `reduce` and `journal` are added because M1's reduction table
//! and DESIGN's L2 cost constraints are both things we will want numbers for.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Identifies one agent within a run. Topology is declarative (DESIGN), so this
/// is a label, not a position in a fixed hierarchy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The decision layer. DESIGN: the first-level agent that stands in for the
    /// human, whose context is the scarce resource.
    pub fn decision() -> Self {
        Self("decision".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    /// Process / agent start-up. The per-step subprocess cost flower paid.
    Init,
    /// Building the request view. DESIGN's `render()`.
    Assemble,
    /// Time to first byte from the wire.
    WireWait,
    /// Time from request start to the first content token. The metric that
    /// distinguishes "the model is thinking" from "the harness is slow".
    FirstToken,
    /// Token streaming.
    Stream,
    /// Tool execution.
    ToolExec,
    /// Deriving a child agent. DESIGN: derivation must be cheap enough that
    /// topology can be chosen at run time.
    Spawn,
    /// Tool-result reduction. DESIGN's reduction policy table.
    Reduce,
    /// Journal append / index.
    Journal,
}

impl SpanKind {
    pub const ALL: [SpanKind; 9] = [
        SpanKind::Init,
        SpanKind::Assemble,
        SpanKind::WireWait,
        SpanKind::FirstToken,
        SpanKind::Stream,
        SpanKind::ToolExec,
        SpanKind::Spawn,
        SpanKind::Reduce,
        SpanKind::Journal,
    ];

    /// Whether a span's wall-clock is dominated by a remote model call.
    ///
    /// DESIGN's headline measurement is "每 step 扣除模型时间后的纯开销". Anything
    /// that is fundamentally waiting on the model must be excluded from it, so
    /// that number means *our* cost rather than the network's.
    pub fn is_model_time(self) -> bool {
        matches!(
            self,
            SpanKind::WireWait | SpanKind::FirstToken | SpanKind::Stream
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SpanKind::Init => "init",
            SpanKind::Assemble => "assemble",
            SpanKind::WireWait => "wire_wait",
            SpanKind::FirstToken => "first_token",
            SpanKind::Stream => "stream",
            SpanKind::ToolExec => "tool_exec",
            SpanKind::Spawn => "spawn",
            SpanKind::Reduce => "reduce",
            SpanKind::Journal => "journal",
        }
    }
}

/// A completed span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub id: u64,
    pub kind: SpanKind,
    pub agent: AgentId,
    /// Wall-clock start. Every observed thing carries a `ts` — flower's `Event`
    /// did not, which is why all of its timing had to be reconstructed outside.
    pub started_at_ms: i64,
    pub elapsed_ns: u64,
    /// Derived from `kind`; stored so the metric can be read without re-deriving
    /// the classification, and so a mis-classification is visible in the record.
    pub model_time: bool,
    pub label: Option<String>,
    pub attrs: BTreeMap<String, serde_json::Value>,
}

impl Span {
    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_ns as f64 / 1_000_000.0
    }
}

/// In-flight span.
///
/// Dropping one without finishing still records it, tagged `unfinished`. DESIGN's
/// stance is that no omission may be silent — that applies to measurement too.
pub struct SpanTimer {
    timeline: Arc<Timeline>,
    kind: SpanKind,
    agent: AgentId,
    started_at_ms: i64,
    start: Instant,
    label: Option<String>,
    attrs: BTreeMap<String, serde_json::Value>,
    finished: bool,
}

impl SpanTimer {
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn attr(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.attrs.insert(key.into(), value.into());
        self
    }

    /// Record the span and hand it back. Consuming `self` means a caller cannot
    /// accidentally record twice.
    pub fn finish(mut self) -> Span {
        self.finished = true;
        let span = self.snapshot();
        self.timeline.record(span.clone());
        span
    }

    fn snapshot(&self) -> Span {
        Span {
            id: self.timeline.next_id(),
            kind: self.kind,
            agent: self.agent.clone(),
            started_at_ms: self.started_at_ms,
            elapsed_ns: self.start.elapsed().as_nanos() as u64,
            model_time: self.kind.is_model_time(),
            label: self.label.clone(),
            attrs: self.attrs.clone(),
        }
    }
}

impl Drop for SpanTimer {
    fn drop(&mut self) {
        if !self.finished {
            let mut span = self.snapshot();
            span.attrs
                .insert("unfinished".to_owned(), serde_json::Value::Bool(true));
            self.timeline.record(span);
        }
    }
}

/// Collector for spans.
///
/// Recording is serialised by a mutex, which is fine: spans are per-request, not
/// per-token. If a future milestone ever records per-token spans this needs to
/// become per-thread buffers.
#[derive(Debug, Default)]
pub struct Timeline {
    spans: Mutex<Vec<Span>>,
    next: AtomicU64,
    current: Mutex<Option<AgentId>>,
}

impl Timeline {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Set the agent that subsequent un-labelled spans are attributed to.
    pub fn enter(&self, agent: AgentId) {
        *self.current.lock().expect("timeline poisoned") = Some(agent);
    }

    pub fn current_agent(&self) -> AgentId {
        self.current
            .lock()
            .expect("timeline poisoned")
            .clone()
            .unwrap_or_else(AgentId::decision)
    }

    /// Begin a span. `self: &Arc<Self>` so the timer can hold a real handle.
    pub fn start(self: &Arc<Self>, kind: SpanKind) -> SpanTimer {
        self.start_for(kind, self.current_agent())
    }

    /// Begin a span attributed to a specific agent, regardless of the current one.
    pub fn start_for(self: &Arc<Self>, kind: SpanKind, agent: AgentId) -> SpanTimer {
        SpanTimer {
            timeline: Arc::clone(self),
            kind,
            agent,
            started_at_ms: crate::now_ms(),
            start: Instant::now(),
            label: None,
            attrs: BTreeMap::new(),
            finished: false,
        }
    }

    pub fn record(&self, span: Span) {
        self.spans.lock().expect("timeline poisoned").push(span);
    }

    /// Record a span whose duration was measured elsewhere.
    ///
    /// Some of DESIGN's spans are *durations*, not windows. `FirstToken` is the
    /// clearest: it is "how long until the first content arrived", measured from
    /// the moment the request went out — so it overlaps `WireWait` rather than
    /// following it, and there is no window to hold a guard across. Handing the
    /// duration in is the only honest way to record it; wrapping a timer around
    /// nothing would produce zero, which is the bug this exists to avoid.
    ///
    /// `started_at_ms` is back-dated so the span sits at the point in the run it
    /// describes rather than at the point it was recorded.
    pub fn record_measured(
        &self,
        kind: SpanKind,
        agent: AgentId,
        elapsed_ns: u64,
        attrs: BTreeMap<String, serde_json::Value>,
    ) -> Span {
        let span = Span {
            id: self.next_id(),
            kind,
            agent,
            started_at_ms: crate::now_ms() - (elapsed_ns / 1_000_000) as i64,
            elapsed_ns,
            model_time: kind.is_model_time(),
            label: None,
            attrs,
        };
        self.record(span.clone());
        span
    }

    pub fn spans(&self) -> Vec<Span> {
        self.spans.lock().expect("timeline poisoned").clone()
    }

    pub fn clear(&self) {
        self.spans.lock().expect("timeline poisoned").clear();
    }
}
