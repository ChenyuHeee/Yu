//! Effect discipline in the loop: 先记后做, and what happens when a record ends
//! inside the window between the two.
//!
//! DESIGN: "effect 纪律:**先记后做**。回放遇到已提交的 effect 直接读记录,绝不重放副作用;
//! 有副作用的工具带 idempotency key". The loop writes the intent before it lets a
//! tool act, so a record that was interrupted has a mark where the act would have
//! been. This module is the reader of that mark — the only place that decides what
//! a replay does about it.
//!
//! ## The default, and why it points the way it does
//!
//! A key that is *not* in the record's committed set is never skipped: an effect
//! whose completion is unrecorded must not be replayed as though it had been
//! performed, which is [`EffectState::may_skip`] and is the whole of what that
//! method is for.
//!
//! That is the first half, and on its own it is not safe. The record's `Intended`
//! state is genuinely *ambiguous*: the act may have happened and the completion
//! may have been lost, or the act may never have started. Both eager readings
//! destroy something —
//!
//! * **assume it happened** (skip): work that never happened is silently never
//!   done, and the run proceeds as though it were;
//! * **assume it did not** (re-perform): work that did happen is done a second
//!   time — the duplicate charge, the second email, the double `rm`.
//!
//! — so neither is the default. What decides is evidence, in this order:
//!
//! 1. **the world**, when the tool can be asked: [`EffectVerdict::InPlace`] means
//!    the act happened and only its record is missing, which settles the question
//!    in the direction that never re-performs anything;
//! 2. **the key's own claim**, for keys that certify that performing them twice is
//!    the same as performing them once — a content-addressed write, say;
//! 3. **otherwise, stop**: [`Decision::Surface`] hands the finding — key, what the
//!    effect was, the seq that recorded it — to whoever can decide, rather than
//!    choosing a way to be wrong.
//!
//! The one thing never done is guessing. A wrong guess here is silent by
//! construction: the run continues, and nothing anywhere says a step was
//! passed over or a charge was made twice.
//!
//! [`EffectState::may_skip`]: yu_journal::EffectState::may_skip

use std::collections::BTreeMap;

use yu_journal::{EffectLog, UnresolvedEffect};
use yu_tools::EffectVerdict;

/// What a run already knows about the effects in the record it is continuing.
///
/// Built once when a run starts, from the whole lineage — a fork inherits its
/// parent's completed effects because it inherited the work those effects did
/// (DESIGN: "fork 用引用而非拷贝", and the reference carries the history).
#[derive(Debug, Clone, Default)]
pub struct Replay {
    committed: EffectLog,
    unfinished: BTreeMap<String, UnresolvedEffect>,
}

/// What the record says about one effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing in the record mentions it. Perform it.
    Fresh,
    /// The record says it was performed, and where its result is. Read that;
    /// never perform the effect again.
    Committed { result_seq: Option<u64> },
    /// The record says it was attempted and never finished — the crash window.
    Unfinished { idempotent: bool, at_seq: u64 },
}

/// What a replay does about an unfinished effect, once the world has been asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The act is already in the world. Record that it happened, perform
    /// nothing: the record was incomplete, not the work.
    AlreadyInPlace,
    /// Perform it. The key certifies that doing so twice is the same as once, so
    /// the one way this can be wrong is the harmless one.
    Reattempt,
    /// Perform nothing and say so. Neither skipping nor re-performing is safe on
    /// the evidence available, so the finding goes to a caller instead.
    Surface,
}

impl Replay {
    /// Seed a run from the record it is continuing.
    pub async fn from_journal(journal: &yu_journal::Journal) -> Result<Self, yu_journal::JournalError> {
        let events = journal.history().await?;
        Ok(Self::from_events(&events))
    }

    /// The same seed, from events already in hand.
    ///
    /// The events, not the journal: a caller that is replaying a *range* of a
    /// record — every historical point is a range — must be able to seed from the
    /// range it is replaying rather than from everything the branch ever did.
    pub fn from_events(events: &[yu_journal::Event]) -> Self {
        Self {
            committed: EffectLog::from_events(events),
            unfinished: EffectLog::unresolved(events)
                .into_iter()
                .map(|effect| (effect.key.clone(), effect))
                .collect(),
        }
    }

    pub fn plan(&self, key: &str) -> Plan {
        if let Some(result_seq) = self.committed.result_seq(key) {
            // Committed wins over unfinished, deliberately: the same key can
            // appear both ways when an effect was completed once and then
            // re-attempted without finishing, and the completion is the fact that
            // makes the work already done.
            return Plan::Committed { result_seq };
        }
        match self.unfinished.get(key) {
            Some(effect) => Plan::Unfinished {
                idempotent: effect.idempotent,
                at_seq: effect.at_seq,
            },
            None => Plan::Fresh,
        }
    }

    /// Everything the record left in the crash window, in key order.
    pub fn unfinished(&self) -> Vec<UnresolvedEffect> {
        self.unfinished.values().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.committed.committed().is_empty() && self.unfinished.is_empty()
    }
}

/// What to do about an unfinished effect, given what the world says.
///
/// Written out rather than inlined at the one call site because this is the
/// decision the whole discipline turns on, and it deserves to be readable — and
/// testable — on its own. Every arm here is a claim about which way of being
/// wrong is survivable.
pub fn decide(idempotent: bool, verdict: EffectVerdict) -> Decision {
    match verdict {
        // The world settles it without any need to trust the key: the act is
        // there. Performing it again would be the duplicate; skipping it loses
        // nothing.
        EffectVerdict::InPlace => Decision::AlreadyInPlace,
        // The world says the act is not there. Re-performing is the only way the
        // work gets done — but only a key that certifies idempotency may do it,
        // because "the world says so" is the tool's reading of a path it can see,
        // not a proof about everything the effect touched.
        EffectVerdict::NotInPlace | EffectVerdict::Unknown if idempotent => Decision::Reattempt,
        // A command, a network call, a payment: the record does not say whether
        // it happened, the tool cannot look, and re-performing may duplicate it.
        EffectVerdict::NotInPlace | EffectVerdict::Unknown => Decision::Surface,
    }
}

/// The key a record holds for one call: the tool's identity for the effect, and
/// the call it belongs to.
///
/// The call id is part of it because the same tool with the same input twice in
/// one run is two effects. Without it, the second call would find the first one's
/// completion in the seed and be skipped — in an ordinary, uncrashed run.
pub fn effect_key(intent_key: &str, tool_use_id: &str) -> String {
    format!("{intent_key}@{tool_use_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use yu_journal::{EffectState, EventBody, SessionId};
    use yu_observe::AgentId;

    fn event(seq: u64, body: EventBody) -> yu_journal::Event {
        yu_journal::Event {
            seq,
            ts_ms: 1,
            session: SessionId::new("s"),
            agent: AgentId::new("exec"),
            body,
        }
    }

    fn effect(key: &str, state: EffectState, idempotent: bool) -> EventBody {
        EventBody::Effect {
            key: key.to_owned(),
            description: "something happened".to_owned(),
            state,
            tool_use_id: Some("call-1".to_owned()),
            idempotent,
        }
    }

    /// The seed's three answers, from one record.
    #[test]
    fn the_plan_reads_the_record_in_three_ways() {
        let events = vec![
            event(0, effect("done", EffectState::Done { result_seq: Some(7) }, true)),
            event(1, effect("started", EffectState::Intended, true)),
            event(2, effect("started", EffectState::Intended, false)),
        ];
        let replay = Replay::from_events(&events);

        assert_eq!(
            replay.plan("done"),
            Plan::Committed { result_seq: Some(7) },
            "a completed effect names where its result is"
        );
        assert_eq!(
            replay.plan("started"),
            Plan::Unfinished {
                idempotent: false,
                at_seq: 2
            },
            "the last word about a key is the one that counts"
        );
        assert_eq!(replay.plan("never-seen"), Plan::Fresh);
    }

    /// **The sharp one.** An effect whose completion was never recorded must not
    /// replay as though it had been — and must not confidently do it again
    /// either.
    #[test]
    fn the_crash_window_is_resolved_by_evidence_and_otherwise_surfaced() {
        // Nothing was recorded about whether the act happened, and the key
        // certifies nothing: stop and ask. Both eager readings lose something
        // real here, which is why neither is the default.
        assert_eq!(decide(false, EffectVerdict::Unknown), Decision::Surface);
        assert_eq!(decide(false, EffectVerdict::NotInPlace), Decision::Surface);

        // A key that certifies idempotency may be performed again when the world
        // has no record of it — the second attempt is the same as the first.
        assert_eq!(decide(true, EffectVerdict::NotInPlace), Decision::Reattempt);
        assert_eq!(decide(true, EffectVerdict::Unknown), Decision::Reattempt);

        // And when the world says the act is there, nothing is performed, whoever
        // the key belongs to: this is the half of the window the record could not
        // answer, answered by the only thing that can.
        for idempotent in [true, false] {
            assert_eq!(
                decide(idempotent, EffectVerdict::InPlace),
                Decision::AlreadyInPlace
            );
        }
    }

    #[test]
    fn the_call_id_is_part_of_the_key_so_two_calls_are_two_effects() {
        assert_ne!(
            effect_key("write_file:p#sha256:abc", "call-1"),
            effect_key("write_file:p#sha256:abc", "call-2"),
            "the same write asked twice is two effects; a key without the call \
             would let the second be skipped as the first"
        );
        assert!(effect_key("write_file:p", "call-1").contains("call-1"));
    }

    #[test]
    fn an_empty_record_seeds_an_empty_replay() {
        let replay = Replay::from_events(&[]);
        assert!(replay.is_empty());
        assert_eq!(replay.plan("anything"), Plan::Fresh);
        assert!(replay.unfinished().is_empty());
    }
}
