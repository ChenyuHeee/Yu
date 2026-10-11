//! L3 effect discipline, against a real journal.
//!
//! DESIGN: "先记后做。回放遇到已提交的 effect 直接读记录,绝不重放副作用". The unit
//! tests in `effect.rs` cover the guard's own rules; these tests cover the reason
//! the rules exist — a replay pass over a recorded branch must not run the side
//! effect a second time, and the journal is where "it already happened" is
//! visible.

mod common;

use common::{agent, journal_in, tempdir};
use yu_journal::{
    EffectGuard, EffectLog, EffectState, EventBody, Journal, ReplayOfUnresolved,
};
use yu_observe::AgentId;

/// One attempt at an effect-bearing tool call, the way a live loop makes it:
/// the key is claimed, the intent is recorded *before* the side effect, the side
/// effect runs, and only then is the result recorded.
///
/// Returns `false` when the effect was already committed, in which case the
/// caller must read the recorded result instead of doing the work again.
async fn attempt(
    journal: &Journal,
    log: &mut EffectLog,
    runs: &mut usize,
    key: &str,
    result: &str,
) -> bool {
    let Some(guard) = EffectGuard::begin(log, key) else {
        return false;
    };
    journal
        .append(
            AgentId::new("exec"),
            EventBody::Effect {
                key: guard.key().to_owned(),
                description: "charge the customer".into(),
                state: EffectState::Done { result_seq: None },
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();
    *runs += 1; // the side effect itself
    guard.mark_done();
    journal
        .append(
            AgentId::new("exec"),
            EventBody::ToolResult {
                tool_use_id: "call_00_charge".into(),
                name: "charge".into(),
                kind: "structured".into(),
                bytes: result.len() as u64,
                reproducible: false,
                addressable: true,
                inline: Some(result.to_owned()),
                blob: None,
                reduction: None,
            },
        )
        .await
        .unwrap();
    true
}

#[tokio::test]
async fn a_replay_of_a_recorded_branch_does_not_repeat_the_side_effect() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "run").await;
    let key = "charge:invoice-7";

    // Live pass.
    let mut log = EffectLog::new();
    let mut runs = 0;
    assert!(attempt(&journal, &mut log, &mut runs, key, r#"{"ok":true}"#).await);
    assert_eq!(runs, 1);
    assert!(log.is_committed(key));

    // A second live attempt in the same process is refused by the guard.
    assert!(!attempt(&journal, &mut log, &mut runs, key, r#"{"ok":true}"#).await);
    assert_eq!(runs, 1);

    // Replay pass: a fresh log, seeded from what the branch recorded. DESIGN's
    // rule is that the record answers "did this already happen" — the effect must
    // not run again, and the previously recorded result is still there to read.
    let recorded = journal.events().await.unwrap();
    let mut replayed = EffectLog::new();
    for event in &recorded {
        if let EventBody::Effect { key, .. } = &event.body {
            replayed.commit(key);
        }
    }
    let mut replay_runs = 0;
    assert!(!attempt(&journal, &mut replayed, &mut replay_runs, key, "unused").await);
    assert_eq!(replay_runs, 0, "a recorded effect is never repeated");

    let recorded_result = recorded.iter().find_map(|event| match &event.body {
        EventBody::ToolResult { inline, .. } => inline.clone(),
        _ => None,
    });
    assert_eq!(recorded_result.as_deref(), Some(r#"{"ok":true}"#));

    // Intent before effect: the Effect event precedes the result it produced.
    assert_eq!(recorded[0].body.kind(), "effect");
    assert_eq!(recorded[1].body.kind(), "tool_result");
}

#[tokio::test]
async fn a_second_begin_on_a_committed_key_returns_none() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "run").await;
    let mut log = EffectLog::new();

    let first = EffectGuard::begin(&mut log, "send:msg-1").unwrap();
    assert_eq!(first.key(), "send:msg-1");
    first.mark_done();
    // The journal now carries the intent, and the log carries the completion.
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "send:msg-1".into(),
                description: "send the message".into(),
                state: EffectState::Done { result_seq: None },
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();

    assert!(EffectGuard::begin(&mut log, "send:msg-1").is_none());
    assert_eq!(log.committed(), vec!["send:msg-1".to_owned()]);
}

#[tokio::test]
async fn an_abandoned_effect_stays_uncommitted_and_is_retried() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "run").await;
    let mut log = EffectLog::new();
    let key = "charge:invoice-8";

    // The attempt dies before the effect completes (error path, cancellation).
    {
        let _guard = EffectGuard::begin(&mut log, key).unwrap();
    }
    assert!(!log.is_committed(key));
    let recorded_keys: Vec<String> = journal
        .events()
        .await
        .unwrap()
        .iter()
        .filter_map(|event| match &event.body {
            EventBody::Effect { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect();
    assert!(recorded_keys.is_empty(), "nothing was recorded either");

    // Retry is allowed, and now the guard closes the key.
    let mut runs = 0;
    assert!(attempt(&journal, &mut log, &mut runs, key, "ok").await);
    assert_eq!(runs, 1);
    assert!(!attempt(&journal, &mut log, &mut runs, key, "ok").await);
}

/// A replay seed built from the record must not mistake an interrupted effect
/// for a finished one.
///
/// The window this discipline exists to cover is between "recorded the intent"
/// and "did the thing". A seed that commits everything it sees treats an effect
/// caught in that window as complete, skips it on replay, and says nothing —
/// so the one failure the mechanism was built to catch is the one it hides.
#[tokio::test]
async fn an_interrupted_effect_is_not_treated_as_done() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "replay").await;

    // Recorded the intent, performed the effect, recorded that it finished.
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "charge-the-card".into(),
                description: "take payment".into(),
                state: EffectState::Intended,
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "charge-the-card".into(),
                description: "take payment".into(),
                state: EffectState::Done { result_seq: None },
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();

    // Recorded the intent and was interrupted before the act.
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "send-the-email".into(),
                description: "notify the customer".into(),
                state: EffectState::Intended,
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();

    let events = journal.events().await.unwrap();
    let seed = EffectLog::from_events(&events);

    assert!(
        seed.is_committed("charge-the-card"),
        "a completed effect is skipped on replay"
    );
    assert!(
        !seed.is_committed("send-the-email"),
        "an effect that was only intended must not be skipped — nobody knows \
         whether it happened"
    );
}

#[tokio::test]
async fn an_abandoned_effect_is_retryable() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "replay").await;
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "rotate-the-key".into(),
                description: "deliberately not performed".into(),
                state: EffectState::Abandoned,
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();
    let seed = EffectLog::from_events(&journal.events().await.unwrap());
    assert!(
        !seed.is_committed("rotate-the-key"),
        "an effect that never happened may be attempted again"
    );
}

#[test]
fn only_done_may_be_skipped() {
    assert!(EffectState::Done { result_seq: None }.may_skip());
    assert!(!EffectState::Intended.may_skip());
    assert!(!EffectState::Abandoned.may_skip());
}

/// An intent the record never finishes is *named*, with an address.
///
/// **M6 (b): "做了没记完 → 能检出".** The failure this guards against is not a
/// crash — the crash happened. It is the reader: a scan that only asks "is there a
/// `Done`" answers "no", and a caller that hears only that concludes the effect
/// never happened and performs it again. The record holds everything needed to say
/// otherwise: what the effect was, which call it belonged to, and whether its key
/// certifies that performing it twice is the same as once.
#[tokio::test]
async fn an_intent_the_record_never_finishes_is_reported_with_its_address() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "window").await;

    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "write:/tmp/out.txt#sha256:abc".into(),
                description: "write 12 bytes to /tmp/out.txt".into(),
                state: EffectState::Intended,
                tool_use_id: Some("call-1".into()),
                idempotent: true,
            },
        )
        .await
        .unwrap();
    journal
        .append(
            agent(),
            EventBody::Effect {
                key: "exec:call-2".into(),
                description: "run `sh -c rm -rf build`".into(),
                state: EffectState::Intended,
                tool_use_id: Some("call-2".into()),
                idempotent: false,
            },
        )
        .await
        .unwrap();

    let seed = journal.replay_seed().await.unwrap();
    let unresolved = journal.unresolved_effects().await.unwrap();

    assert_eq!(
        unresolved
            .iter()
            .map(|u| (u.key.as_str(), u.at_seq))
            .collect::<Vec<_>>(),
        vec![("exec:call-2", 1), ("write:/tmp/out.txt#sha256:abc", 0)],
        "every unfinished effect must be named, with the seq that recorded it"
    );
    assert_eq!(
        unresolved[0].replay(),
        ReplayOfUnresolved::Surface,
        "a command that cannot certify itself must not be re-run on a record \
         that does not say whether it ran"
    );
    assert_eq!(
        unresolved[1].replay(),
        ReplayOfUnresolved::Reattempt,
        "a key that certifies idempotency may be performed again"
    );
    assert_eq!(
        unresolved[1].tool_use_id.as_deref(),
        Some("call-1"),
        "the finding is tied to the call it belonged to, or it cannot be checked"
    );

    // And the seed agrees with it: neither key may be skipped.
    assert!(!seed.is_committed("write:/tmp/out.txt#sha256:abc"));
    assert!(!seed.is_committed("exec:call-2"));
}

/// The last state per key is what counts, in both directions.
///
/// A key that finished and was later re-attempted without finishing is in the
/// window again — a scan that stopped at the first `Done` would call it settled
/// and skip work that may be outstanding.
#[tokio::test]
async fn resolution_is_the_last_state_and_not_the_first() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "settling").await;
    let effect = |state| EventBody::Effect {
        key: "rotate".into(),
        description: "rotate the key".into(),
        state,
        tool_use_id: None,
        idempotent: true,
    };

    journal.append(agent(), effect(EffectState::Intended)).await.unwrap();
    journal
        .append(agent(), effect(EffectState::Done { result_seq: None }))
        .await
        .unwrap();
    assert!(
        journal.unresolved_effects().await.unwrap().is_empty(),
        "a later completion settles an earlier intent"
    );

    journal.append(agent(), effect(EffectState::Intended)).await.unwrap();
    assert_eq!(
        journal.unresolved_effects().await.unwrap().len(),
        1,
        "a re-attempt that never finished is in the window again"
    );
    journal
        .append(agent(), effect(EffectState::Abandoned))
        .await
        .unwrap();
    assert!(
        journal.unresolved_effects().await.unwrap().is_empty(),
        "and a deliberate abandonment settles it too: the effect did not happen"
    );
    // The two readings are separate answers to separate questions, and this is
    // where they can disagree: the key *has* been completed once (so a replay may
    // read the record instead of doing the work), and the last attempt was
    // abandoned (so nothing is left in the window). The window is what a replay
    // must not skip through; `committed` is what it may skip *with*.
    let seed = journal.replay_seed().await.unwrap();
    assert!(seed.is_committed("rotate"));
}
