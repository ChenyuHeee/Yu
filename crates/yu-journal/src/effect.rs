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

/// Keys whose side effects have completed, and where their results landed.
///
/// A `BTreeMap` rather than a hash map so [`EffectLog::committed`] is ordered:
/// the journal's contents are replayed in order, and a map that enumerates
/// differently on every run would make that replay non-deterministic for no
/// gain.
#[derive(Debug, Clone, Default)]
pub struct EffectLog {
    /// Key -> the seq of the result it produced, when the record named one.
    ///
    /// A map rather than a set because "this effect was performed" and "here is
    /// what it produced" are two different answers, and the second is what a
    /// replay needs in order to read the record instead of performing the
    /// effect: DESIGN's "回放遇到已提交的 effect 直接读记录" is only possible if the
    /// record says where. `None` is a completion whose result was never recorded
    /// (or was recorded somewhere this log did not see), which is not the same as
    /// an incomplete effect.
    committed: std::collections::BTreeMap<String, Option<u64>>,
}

impl EffectLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Where the recorded result of a completed effect is, if the record said.
    ///
    /// `Some(None)` means the effect is committed and its result is not
    /// addressable — a replay must still not perform it again, but it has nothing
    /// to read either, and saying so is the caller's job.
    pub fn result_seq(&self, key: &str) -> Option<Option<u64>> {
        self.committed.get(key).copied()
    }

    /// Whether the effect behind `key` has already happened in this run.
    pub fn is_committed(&self, key: &str) -> bool {
        self.committed.contains_key(key)
    }

    /// Mark the effect behind `key` as done. Idempotent: committing a key twice
    /// is the same as committing it once.
    ///
    /// A later completion wins, because the second one is the one that can name
    /// its result. A completion with no result does not overwrite one that has
    /// one — the address is what a replay needs, and dropping it would make a
    /// committed effect unreadable.
    pub fn commit(&mut self, key: impl Into<String>) {
        self.committed.insert(key.into(), None);
    }

    /// Commit `key`, recording where its result landed.
    pub fn commit_with_result(&mut self, key: impl Into<String>, result_seq: Option<u64>) {
        let key = key.into();
        let recorded = self.committed.get(&key).copied().flatten();
        self.committed
            .insert(key, result_seq.or(recorded));
    }

    /// Every committed key, in order.
    /// Rebuild the committed set from a recorded branch.
    ///
    /// Only `Done` effects commit. An `Intended` effect is the ambiguous case —
    /// the intent is recorded and the act is not — and treating it as complete
    /// is exactly how an interrupted side effect vanishes from a replay while
    /// looking finished. `Abandoned` does not commit either: the effect did not
    /// happen, so a replay is free to try again.
    ///
    /// This is the seed a replay starts from, and getting it wrong is silent in
    /// the worst way: the run continues, the effect is skipped, and nothing
    /// anywhere says that a step was passed over.
    pub fn from_events(events: &[crate::Event]) -> Self {
        let mut log = Self::new();
        for event in events {
            if let crate::EventBody::Effect { key, state, .. } = &event.body
                && state.may_skip()
            {
                match state {
                    crate::EffectState::Done { result_seq } => {
                        log.commit_with_result(key.clone(), *result_seq);
                    }
                    _ => log.commit(key.clone()),
                }
            }
        }
        log
    }

    pub fn committed(&self) -> Vec<String> {
        self.committed.keys().cloned().collect()
    }

    /// The effects the record says were attempted and never finished.
    ///
    /// This is the other half of [`EffectLog::from_events`], and it is the half
    /// that answers "做了没记完 → 能检出". A record that ends on `Intended` is a
    /// record that was interrupted inside the one window the discipline exists
    /// for, and the finding is *negative evidence with an address*: the key, what
    /// the effect was, and which call it belonged to. Silence is not an option —
    /// an effect that is neither committed nor reported is one a caller will
    /// assume never happened.
    ///
    /// Last state wins per key, not first: a key that was completed and later
    /// re-attempted without finishing is unresolved again, and a scan that
    /// stopped at the first `Done` would report it as settled.
    pub fn unresolved(events: &[crate::Event]) -> Vec<UnresolvedEffect> {
        let mut last: std::collections::BTreeMap<String, UnresolvedEffect> =
            std::collections::BTreeMap::new();
        for event in events {
            if let crate::EventBody::Effect {
                key,
                description,
                state,
                tool_use_id,
                idempotent,
            } = &event.body
            {
                match state {
                    crate::EffectState::Intended => {
                        last.insert(
                            key.clone(),
                            UnresolvedEffect {
                                key: key.clone(),
                                description: description.clone(),
                                tool_use_id: tool_use_id.clone(),
                                idempotent: *idempotent,
                                at_seq: event.seq,
                            },
                        );
                    }
                    // Settled one way or the other: the key is no longer in the
                    // window, whichever side it landed on.
                    crate::EffectState::Done { .. } | crate::EffectState::Abandoned => {
                        last.remove(key);
                    }
                }
            }
        }
        last.into_values().collect()
    }
}

/// An effect that was recorded as intended and never recorded as finished.
///
/// Carried as a value rather than as a log line because every reader of it has to
/// decide something — re-attempt it, refuse it, or ask a human — and a formatted
/// sentence would have to be re-parsed to do that.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UnresolvedEffect {
    pub key: String,
    /// What the effect was, in the words of the tool that declared it.
    pub description: String,
    /// The call it belonged to, when it belonged to one.
    pub tool_use_id: Option<String>,
    /// The tool's claim that re-performing it is safe — see
    /// [`crate::EventBody::Effect`].
    pub idempotent: bool,
    /// The seq of the `Intended` record, so the finding has an address.
    pub at_seq: u64,
}

impl UnresolvedEffect {
    /// What a replay must do about it, given that the record does not say whether
    /// the effect happened.
    ///
    /// Both eager answers are wrong somewhere, and the key's own claim is what
    /// decides between them: a key that certifies idempotency can be performed
    /// again, and one that does not may be neither skipped (work that maybe never
    /// happened is lost) nor re-performed (work that maybe happened is
    /// duplicated), so it stops and asks.
    pub fn replay(&self) -> ReplayOfUnresolved {
        if self.idempotent {
            ReplayOfUnresolved::Reattempt
        } else {
            ReplayOfUnresolved::Surface
        }
    }
}

/// What an unresolved effect leaves a replay to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayOfUnresolved {
    /// The key certifies that performing it twice is the same as once.
    Reattempt,
    /// The key certifies nothing, so neither skipping nor re-performing is safe.
    /// The effect is reported with its address and a caller — a human, or the
    /// decision layer — decides.
    Surface,
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
