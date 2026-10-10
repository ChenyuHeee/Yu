//! Trait boundaries.
//!
//! DESIGN's layering rule: "`core` 只定义 trait,`policy` 注入实现." `core` must not
//! depend on `yu-policy`, so that policy can change without touching the loop —
//! which is constraint 0's acceptance criterion ("改一条策略规则,不重新编译 Rust").

use serde_json::Value;

/// Constraints, injected rather than remembered.
///
/// DESIGN's derived principle 3: "约束靠注入,不靠记忆" — every layer, every turn,
/// is *given* its constraints. Nothing here has a default, deliberately: a
/// default would let a missing constraint look like an absent requirement, and
/// the failure mode of an unstated constraint is that the work is done wrong
/// competently.
pub trait PolicySource: Send + Sync {
    /// A named prompt, role description, or tool description.
    fn prompt(&self, name: &str) -> Option<String>;

    /// A named numeric budget.
    fn budget(&self, name: &str) -> Option<u64>;

    /// A named structured rule, for policy expressed as data.
    fn rule(&self, name: &str) -> Option<Value>;
}

/// A policy source with nothing in it.
///
/// Every lookup returns `None`, so an unloaded policy is *visible* as an absence
/// rather than silently substituted with built-in defaults. That distinction is
/// the difference between "no budget configured" and "budget unlimited".
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyPolicy;

impl PolicySource for EmptyPolicy {
    fn prompt(&self, _name: &str) -> Option<String> {
        None
    }

    fn budget(&self, _name: &str) -> Option<u64> {
        None
    }

    fn rule(&self, _name: &str) -> Option<Value> {
        None
    }
}

/// The trigger for compaction or a generation change.
///
/// DESIGN names this as one of two traps already stepped in by others:
///
/// > **压缩后立刻重新触发**:压缩后窗口被前缀重播种,按全量计费会立刻再次触发。
/// > **触发器必须计"自上次压缩以来的增长",不是总上下文**(Codex 的 `body_after_prefix`)
///
/// So the state that matters is not the context size but the growth since the
/// last contraction. [`GrowthBudget::rearm`] is what makes the difference, and
/// the test that a large-but-unchanged context does *not* fire is the point of
/// the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthBudget {
    pub threshold: u64,
    /// Context size at the last contraction.
    baseline: u64,
    /// Largest size seen since the baseline, for reporting.
    peak: u64,
}

impl GrowthBudget {
    pub fn new(threshold: u64) -> Self {
        Self {
            threshold,
            baseline: 0,
            peak: 0,
        }
    }

    /// Report the current context size; true when growth since the last
    /// contraction has passed the threshold.
    pub fn observe(&mut self, current: u64) -> bool {
        self.peak = self.peak.max(current);
        self.growth(current) >= self.threshold
    }

    pub fn growth(&self, current: u64) -> u64 {
        current.saturating_sub(self.baseline)
    }

    pub fn peak_growth(&self) -> u64 {
        self.peak.saturating_sub(self.baseline)
    }

    /// Called after a contraction: the baseline becomes the size now.
    pub fn rearm(&mut self, current: u64) {
        self.baseline = current;
        self.peak = current;
    }

    pub fn baseline(&self) -> u64 {
        self.baseline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_policy_reports_absence_rather_than_defaults() {
        let p = EmptyPolicy;
        assert_eq!(p.prompt("charter"), None);
        assert_eq!(p.budget("max_steps"), None);
        assert_eq!(p.rule("topology"), None);
    }

    #[test]
    fn a_large_but_unchanged_context_does_not_fire() {
        // The trap DESIGN names: after a contraction the window is re-seeded by
        // the prefix, so a total-context trigger fires again immediately.
        let mut budget = GrowthBudget::new(10_000);
        budget.rearm(900_000);
        assert!(
            !budget.observe(900_000),
            "900k of unchanged context is not growth"
        );
        assert!(!budget.observe(905_000));
    }

    #[test]
    fn growth_since_the_last_contraction_fires() {
        let mut budget = GrowthBudget::new(10_000);
        budget.rearm(900_000);
        assert!(!budget.observe(905_000));
        assert!(budget.observe(911_000), "11k of growth must fire");
    }

    #[test]
    fn rearming_resets_the_count_and_can_fire_again() {
        let mut budget = GrowthBudget::new(10_000);
        budget.rearm(0);
        assert!(budget.observe(12_000));
        budget.rearm(12_000);
        assert!(!budget.observe(12_000), "must not fire immediately after rearming");
        assert!(budget.observe(25_000));
        assert_eq!(budget.baseline(), 12_000);
    }

    #[test]
    fn a_context_that_shrinks_does_not_report_negative_growth() {
        let mut budget = GrowthBudget::new(1000);
        budget.rearm(5000);
        assert_eq!(budget.growth(100), 0, "growth saturates rather than wrapping");
        assert!(!budget.observe(100));
    }

    #[test]
    fn peak_growth_remembers_the_high_water_mark() {
        let mut budget = GrowthBudget::new(100_000);
        budget.rearm(1000);
        budget.observe(40_000);
        budget.observe(20_000);
        assert_eq!(
            budget.peak_growth(),
            39_000,
            "the peak is what a budget report should show, not the last sample"
        );
        assert!(!budget.observe(20_000));
    }
}
