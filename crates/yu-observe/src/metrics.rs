//! Aggregate metrics, including the one flower never managed to measure:
//! per-step harness overhead with model time removed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::span::{AgentId, Span, SpanKind};

/// Token accounting for one request or a whole run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }

    /// Cache hit rate over the cacheable prefix.
    ///
    /// Returns `None` rather than 0.0 when nothing was cacheable — a run that
    /// never had a cacheable prefix has an undefined hit rate, not a zero one.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let total = self.input_tokens + self.cache_read_input_tokens;
        if total == 0 {
            return None;
        }
        Some(self.cache_read_input_tokens as f64 / total as f64)
    }
}

/// DESIGN's acceptance metric: the share of an agent's context that is **raw
/// material** — bytes poured in as they came off a tool, with neither a human
/// nor a model having decided what in them mattered.
///
/// DESIGN first stated this as "工具机制" (tool machinery) and that measured the
/// wrong thing. Under it, a context holding one reduced report and a context
/// holding fifty thousand lines of grep both read about 30%: both arrive as tool
/// results, so both are counted the same. They are not the same finding, and the
/// claim — *this layer is not where the material piles up* — is about the second
/// one. A report is a product of judgment. It was decided what to leave out.
///
/// The breakdown survives the change because the parts answer different
/// questions, and because collapsing them is what made the old number unable to
/// tell two very different contexts apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextComposition {
    /// Tool output as it came off the tool: stdout, a file's contents, a log,
    /// an evidence block. **DESIGN's numerator.**
    pub raw_material_chars: u64,
    /// Tool output that a reduction produced before it arrived — a report, a
    /// summary, an address rather than the thing.
    pub reduced_result_chars: u64,
    /// Characters of tool-call *arguments* in the context.
    ///
    /// The model wrote these, so they are judgment rather than material — but
    /// they are also pure overhead from the context's point of view, which is
    /// why they stay counted separately rather than folded into either side.
    pub tool_call_chars: u64,
    /// Characters of tool *definitions* in the context.
    ///
    /// Occupies context on every single request, so a ratio that omitted it
    /// would under-report the prefix's cost. It is neither raw nor judged: it is
    /// the declared surface.
    pub tool_definition_chars: u64,
    /// All characters in the context, including everything above.
    pub total_chars: u64,
}

impl ContextComposition {
    /// Raw material as a fraction of the context.
    ///
    /// `None` when the context is empty. Returning `Some(0.0)` there would let a
    /// renderer that produced nothing at all look like a perfectly lean one —
    /// and a metric that cannot report "unmeasured" is a metric that cannot fail.
    pub fn raw_material_ratio(&self) -> Option<f64> {
        if self.total_chars == 0 {
            return None;
        }
        Some(self.raw_material_chars as f64 / self.total_chars as f64)
    }

    /// Every byte of the context that is tool machinery rather than intent,
    /// state, or a result's prose.
    pub fn tool_machinery_chars(&self) -> u64 {
        self.tool_call_chars
            + self.reduced_result_chars
            + self.raw_material_chars
            + self.tool_definition_chars
    }

    /// Tool machinery as a fraction of the context.
    ///
    /// **Not the acceptance criterion.** It is kept because it is a real
    /// quantity, because it is what the numbers recorded before this metric
    /// changed were measured in, and because the two ratios disagreeing is
    /// itself informative: a context can be nearly all machinery and nearly no
    /// raw material, which is exactly what a context holding one report looks
    /// like.
    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        if self.total_chars == 0 {
            return None;
        }
        Some(self.tool_machinery_chars() as f64 / self.total_chars as f64)
    }
}

/// Rolled-up numbers over a set of spans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub steps: u64,
    pub requests: u64,
    pub tool_calls: u64,
    pub usage: TokenUsage,
    /// Wall-clock spent, per span kind.
    pub elapsed_ns_by_kind: BTreeMap<String, u64>,
    /// Wall-clock spent per agent.
    pub elapsed_ns_by_agent: BTreeMap<String, u64>,
    /// Allocation attributed per agent, in bytes.
    pub allocated_bytes_by_agent: BTreeMap<String, u64>,
    pub context: ContextComposition,
}

impl Metrics {
    pub fn observe_spans(&mut self, spans: &[Span]) {
        self.elapsed_ns_by_kind.clear();
        self.elapsed_ns_by_agent.clear();
        for span in spans {
            *self
                .elapsed_ns_by_kind
                .entry(span.kind.as_str().to_owned())
                .or_default() += span.elapsed_ns;
            *self
                .elapsed_ns_by_agent
                .entry(span.agent.as_str().to_owned())
                .or_default() += span.elapsed_ns;
        }
    }

    /// Total wall-clock attributable to the model, across all agents.
    pub fn model_time_ns(&self) -> u64 {
        SpanKind::ALL
            .iter()
            .filter(|k| k.is_model_time())
            .filter_map(|k| self.elapsed_ns_by_kind.get(k.as_str()).copied())
            .sum()
    }

    /// Total wall-clock attributable to the harness itself.
    pub fn harness_time_ns(&self) -> u64 {
        SpanKind::ALL
            .iter()
            .filter(|k| !k.is_model_time())
            .filter_map(|k| self.elapsed_ns_by_kind.get(k.as_str()).copied())
            .sum()
    }

    /// **The headline number.** DESIGN: "flower 至今测不到的那一项" — harness
    /// overhead per step, with model time subtracted.
    ///
    /// `None` when no steps have been observed: zero steps has no per-step cost.
    pub fn harness_time_per_step_ms(&self) -> Option<f64> {
        if self.steps == 0 {
            return None;
        }
        Some(self.harness_time_ns() as f64 / self.steps as f64 / 1_000_000.0)
    }

    pub fn record_agent_allocation(&mut self, agent: &AgentId, bytes: u64) {
        *self
            .allocated_bytes_by_agent
            .entry(agent.as_str().to_owned())
            .or_default() += bytes;
    }
}

/// Per-agent metric slice, for the `yu metrics --agent` view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMetrics {
    pub agent: AgentId,
    pub context: ContextComposition,
    pub usage: TokenUsage,
}

impl AgentMetrics {
    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        self.context.tool_machinery_ratio()
    }
}
