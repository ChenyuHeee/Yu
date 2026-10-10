//! Effect discipline for L3.
//!
//! DESIGN: "effect 纪律:先记后做。回放遇到已提交的 effect 直接读记录,绝不重放副作用;
//! 有副作用的工具带 idempotency key". A replay of a recorded branch reads the result
//! it recorded instead of performing the side effect again — which only works if the
//! side effect's key is committed exactly once and never assumed.
//!
//! The guard is the cheap half of that rule, inside one process: it makes "was this
//! key already committed" a question the caller has to ask *before* acting, and it
//! leaves the key uncommitted unless the caller says the effect completed.

use std::collections::BTreeSet;

/// Keys whose side effects have completed.
///
/// `BTreeSet` rather than a hash set so [`EffectLog::committed`] is ordered:
/// the journal's contents are replayed in order, and a set that enumerates
/// differently on every run would make that replay non-deterministic for no
/// gain.
#[derive(Debug, Clone, Default)]
pub struct EffectLog {
    committed: BTreeSet<String>,
}

impl EffectLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the effect behind `key` has already happened in this run.
    pub fn is_committed(&self, key: &str) -> bool {
        self.committed.contains(key)
    }

    /// Mark the effect behind `key` as done. Idempotent: committing a key twice
    /// is the same as committing it once.
    pub fn commit(&mut self, key: impl Into<String>) {
        self.committed.insert(key.into());
    }

    /// Every committed key, in order.
    pub fn committed(&self) -> Vec<String> {
        self.committed.iter().cloned().collect()
    }
}

/// Permission to perform one side effect.
///
/// [`EffectGuard::begin`] returning `None` means "this key is committed — do not
/// perform the side effect; read the recorded result instead". Holding the guard
/// borrows the log, so the two halves of the rule cannot be interleaved by
/// accident: while an effect is in flight, nobody else can begin another key on
/// the same log.
pub struct EffectGuard<'a> {
    log: &'a mut EffectLog,
    key: String,
    /// Set by [`EffectGuard::mark_done`]. Dropping an unmarked guard is the
    /// aborted case and must leave the key uncommitted.
    done: bool,
}

impl<'a> EffectGuard<'a> {
    /// Claim `key`, or report that it is already committed.
    ///
    /// This is the "先记" half: the caller is expected to have the intent in the
    /// journal (an `EventBody::Effect`) before the effect happens, so a replay
    /// of the recorded branch sees the key and skips the work.
    pub fn begin(log: &'a mut EffectLog, key: impl Into<String>) -> Option<Self> {
        let key = key.into();
        if log.is_committed(&key) {
            return None;
        }
        Some(EffectGuard {
            log,
            key,
            done: false,
        })
    }

    /// Record that the side effect completed.
    pub fn mark_done(mut self) {
        let key = std::mem::take(&mut self.key);
        self.log.commit(key);
        self.done = true;
    }

    /// The idempotency key this guard holds.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for EffectGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            // Committing here would be the one state replay must never see: an
            // effect that never happened, recorded as done. An abandoned attempt
            // (error path, cancellation, crash) stays uncommitted so it is
            // retried — which is safe precisely because the key is an
            // idempotency key.
            tracing::debug!(
                key = %self.key,
                "effect guard dropped without mark_done; the effect stays uncommitted"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_done_then_begin_returns_none() {
        let mut log = EffectLog::new();
        assert!(!log.is_committed("charge-card:inv-7"));
        let guard = EffectGuard::begin(&mut log, "charge-card:inv-7").unwrap();
        assert_eq!(guard.key(), "charge-card:inv-7");
        guard.mark_done();

        assert!(log.is_committed("charge-card:inv-7"));
        // The second begin is refused: the side effect must not run again.
        assert!(EffectGuard::begin(&mut log, "charge-card:inv-7").is_none());
    }

    #[test]
    fn beginning_twice_on_the_same_key_yields_none_the_second_time() {
        let mut log = EffectLog::new();
        EffectGuard::begin(&mut log, "k").unwrap().mark_done();
        assert!(EffectGuard::begin(&mut log, "k").is_none());
        // Still None however many times the question is asked.
        assert!(EffectGuard::begin(&mut log, "k").is_none());
    }

    #[test]
    fn an_abandoned_guard_does_not_commit() {
        let mut log = EffectLog::new();
        drop(EffectGuard::begin(&mut log, "k").unwrap());
        assert!(!log.is_committed("k"));
        assert!(log.committed().is_empty());
        // An aborted effect stays uncommitted, so a retry is allowed.
        let retry = EffectGuard::begin(&mut log, "k").unwrap();
        retry.mark_done();
        assert!(log.is_committed("k"));
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let mut log = EffectLog::new();
        EffectGuard::begin(&mut log, "a").unwrap().mark_done();
        assert!(EffectGuard::begin(&mut log, "b").is_some());
    }

    #[test]
    fn committed_is_ordered_and_deduplicated() {
        let mut log = EffectLog::new();
        log.commit("b");
        log.commit("a");
        log.commit("b");
        assert_eq!(log.committed(), vec!["a".to_owned(), "b".to_owned()]);
    }
}
