//! L3 effect discipline, against a real journal.
//!
//! DESIGN: "先记后做。回放遇到已提交的 effect 直接读记录,绝不重放副作用". The unit
//! tests in `effect.rs` cover the guard's own rules; these tests cover the reason
//! the rules exist — a replay pass over a recorded branch must not run the side
//! effect a second time, and the journal is where "it already happened" is
//! visible.

mod common;

use common::{agent, journal_in, tempdir};
use yu_journal::{EffectGuard, EffectLog, EventBody, Journal};
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
