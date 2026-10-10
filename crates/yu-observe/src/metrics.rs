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

/// DESIGN's acceptance metric: the share of an agent's context occupied by tool
/// machinery (tool results + tool calls).
///
/// Industry shape is that this dominates. The decision layer's target is
/// "趋近于零". The value of this number is that it makes the claim falsifiable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextComposition {
    /// Characters of tool-call arguments in the rendered view.
    pub tool_call_chars: u64,
    /// Characters of tool results in the rendered view.
    pub tool_result_chars: u64,
    /// All characters in the rendered view.
    pub total_chars: u64,
}

impl ContextComposition {
    /// Tool machinery as a fraction of the view. `None` when the view is empty.
    pub fn tool_machinery_ratio(&self) -> Option<f64> {
        if self.total_chars == 0 {
            return None;
        }
        Some((self.tool_call_chars + self.tool_result_chars) as f64 / self.total_chars as f64)
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
