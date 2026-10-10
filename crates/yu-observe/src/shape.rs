//! Shape metrics.
//!
//! DESIGN: "看形状,不看内容" — the decision layer does not read the tool stream,
//! it watches cheap proxies. The judgements are deterministic, not vibes:
//!
//! * tool calls per turn
//! * files actually touched vs files declared
//! * tokens growing while no diff appears
//! * time since the last verifiable artifact
//! * identical calls repeated
//!
//! This module only *measures*. Acting on a signal is the decision layer's job,
//! and which signal triggers which intervention is policy — see `yu-policy`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::span::AgentId;

/// DESIGN, borrowed from opencode's `DOOM_LOOP_THRESHOLD` and the expert
/// protocol "同一失败出现两次就停,不要纠正第三次".
///
/// The point is that repetition is *counted*, not judged. A model asked to
/// assess whether it is looping will not reliably say so.
pub const REPEAT_THRESHOLD: u32 = 2;

/// One turn's observable shape.
///
/// Deliberately not `Default`: a step with no agent is not a meaningful
/// observation, and inventing one would attribute work to somebody who did not
/// do it. The allocation attribution code makes the same choice for the same
/// reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepObservation {
    pub agent: AgentId,
    pub tool_calls: u32,
    /// Files the tool calls actually touched.
    pub files_touched: Vec<String>,
    /// Files the agent said it would touch. `None` when nothing was declared,
    /// which is itself a finding rather than a zero.
    pub files_declared: Option<Vec<String>>,
    pub tokens_delta: u64,
    /// Whether this step produced something checkable (a diff, a test result, an
    /// artifact). DESIGN: "token 在涨但没有 diff" is a shape, not a complaint.
    pub produced_artifact: bool,
    /// ms since the last step that produced an artifact. `None` if none ever has.
    pub ms_since_last_artifact: Option<u64>,
    /// Canonical signature of each tool call, for repeat detection.
    pub call_signatures: Vec<String>,
}

impl StepObservation {
    pub fn new(agent: AgentId) -> Self {
        Self {
            agent,
            tool_calls: 0,
            files_touched: Vec::new(),
            files_declared: None,
            tokens_delta: 0,
            produced_artifact: false,
            ms_since_last_artifact: None,
            call_signatures: Vec::new(),
        }
    }
}

/// A deterministic observation about shape. No judgement, no prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShapeSignal {
    /// The same call, byte-identical, seen `count` times. DESIGN's intervention
    /// trigger 1 fires at `count >= REPEAT_THRESHOLD`.
    RepeatedCall { signature: String, count: u32 },
    /// Files touched that were never declared.
    ScopeDrift {
        touched: Vec<String>,
        declared: Vec<String>,
    },
    /// Tokens went up and nothing checkable came out.
    GrowthWithoutArtifact { tokens_delta: u64 },
    /// No verifiable artifact for this long.
    NoArtifactFor { ms: u64 },
}

impl ShapeSignal {
    /// Whether this signal is one DESIGN names as an intervention trigger.
    ///
    /// Trigger 1 is "同一失败出现两次 → 停止,不纠正第三次". Only the repeat signal
    /// is a hard stop; the others are re-anchoring triggers, which is a different
    /// (and gentler) intervention.
    pub fn is_hard_stop(&self) -> bool {
        matches!(
            self,
            ShapeSignal::RepeatedCall { count, .. } if *count >= REPEAT_THRESHOLD
        )
    }
}

/// Accumulates `StepObservation`s and emits signals.
#[derive(Debug, Default)]
pub struct ShapeTracker {
    call_counts: HashMap<String, u32>,
    last_artifact_ms: Option<u64>,
    /// Threshold for `NoArtifactFor`, in ms. `None` disables that signal.
    pub artifact_stall_ms: Option<u64>,
}

impl ShapeTracker {
    pub fn new() -> Self {
        Self {
            artifact_stall_ms: Some(15 * 60 * 1000),
            ..Default::default()
        }
    }

    /// Feed one step, get back whatever the shape says.
    pub fn observe(&mut self, step: &StepObservation, now_ms: u64) -> Vec<ShapeSignal> {
        let mut signals = Vec::new();

        for sig in &step.call_signatures {
            let count = self.call_counts.entry(sig.clone()).or_insert(0);
            *count += 1;
            if *count >= REPEAT_THRESHOLD {
                signals.push(ShapeSignal::RepeatedCall {
                    signature: sig.clone(),
                    count: *count,
                });
            }
        }

        if let Some(declared) = &step.files_declared {
            let touched: Vec<String> = step
                .files_touched
                .iter()
                .filter(|f| !declared.contains(f))
                .cloned()
                .collect();
            if !touched.is_empty() {
                signals.push(ShapeSignal::ScopeDrift {
                    touched,
                    declared: declared.clone(),
                });
            }
        }

        if step.produced_artifact {
            self.last_artifact_ms = Some(now_ms);
        } else if step.tokens_delta > 0 {
            signals.push(ShapeSignal::GrowthWithoutArtifact {
                tokens_delta: step.tokens_delta,
            });
            if let (Some(last), Some(stall)) = (self.last_artifact_ms, self.artifact_stall_ms)
                && now_ms.saturating_sub(last) > stall
            {
                signals.push(ShapeSignal::NoArtifactFor {
                    ms: now_ms - last,
                });
            }
        }

        signals
    }

    /// Forget repeat counts. Called when the loop is re-anchored: DESIGN's
    /// intervention 1 is "重新锚定后重启", which resets the count by definition.
    pub fn reset_counts(&mut self) {
        self.call_counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(sigs: &[&str], tokens: u64, artifact: bool) -> StepObservation {
        StepObservation {
            tool_calls: sigs.len() as u32,
            tokens_delta: tokens,
            produced_artifact: artifact,
            call_signatures: sigs.iter().map(|s| (*s).to_owned()).collect(),
            ..StepObservation::new(AgentId::new("exec"))
        }
    }

    #[test]
    fn second_identical_call_is_a_hard_stop() {
        let mut t = ShapeTracker::new();
        assert!(t.observe(&step(&["read:a.rs"], 10, true), 0).is_empty());
        let sig = t.observe(&step(&["read:a.rs"], 10, true), 1);
        assert_eq!(sig.len(), 1);
        assert!(sig[0].is_hard_stop(), "second identical call must stop the loop");
    }

    #[test]
    fn distinct_calls_do_not_trip_the_threshold() {
        let mut t = ShapeTracker::new();
        t.observe(&step(&["read:a.rs"], 10, true), 0);
        let sig = t.observe(&step(&["read:b.rs"], 10, true), 1);
        assert!(sig.is_empty());
    }

    #[test]
    fn growth_without_artifact_is_reported() {
        let mut t = ShapeTracker::new();
        let sig = t.observe(&step(&[], 500, false), 0);
        assert!(matches!(
            sig.as_slice(),
            [ShapeSignal::GrowthWithoutArtifact { tokens_delta: 500 }]
        ));
    }

    #[test]
    fn scope_drift_lists_only_undeclared_files() {
        let mut t = ShapeTracker::new();
        let mut s = step(&[], 1, true);
        s.files_declared = Some(vec!["src/a.rs".into()]);
        s.files_touched = vec!["src/a.rs".into(), "src/b.rs".into()];
        let sig = t.observe(&s, 0);
        match &sig[..] {
            [ShapeSignal::ScopeDrift { touched, .. }] => {
                assert_eq!(touched, &vec!["src/b.rs".to_owned()])
            }
            other => panic!("expected scope drift, got {other:?}"),
        }
    }

    #[test]
    fn reanchoring_resets_the_repeat_count() {
        let mut t = ShapeTracker::new();
        t.observe(&step(&["read:a.rs"], 0, true), 0);
        t.reset_counts();
        let sig = t.observe(&step(&["read:a.rs"], 0, true), 1);
        assert!(sig.is_empty(), "count must restart after re-anchoring");
    }
}
