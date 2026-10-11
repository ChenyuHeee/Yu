//! L3: replay, and fork by reference.
//!
//! DESIGN: "**fork 用引用而非拷贝** —— 成本从 O(记录) 降到 O(1)", and
//! "回放遇到已提交的 effect 直接读记录,绝不重放副作用". Both are properties of the
//! record, so both are testable against it.

mod common;

use std::sync::Arc;

use common::*;
use yu_journal::{EffectState, EventBody, SessionId, ThinkingRecord};

#[tokio::test]
async fn a_fork_reads_its_parents_prefix_without_copying_it() {
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    for i in 0..5 {
        parent
            .append(agent(), EventBody::Note { text: format!("p{i}") })
            .await
            .unwrap();
    }

    let child = journal_fork(dir.path(), "child", parent.clone(), 3).await;
    child
        .append(agent(), EventBody::Note { text: "c0".into() })
        .await
        .unwrap();

    let history = child.history().await.unwrap();
    // Three inherited, one of its own. The fork marker is metadata about the
    // record, not something that happened in it.
    assert_eq!(history.len(), 4, "got {history:?}");
    assert_eq!(history[0].seq, 0);
    assert_eq!(history[2].seq, 2);
    assert_eq!(history[3].body.kind(), "note");

    // The child's own file holds the marker and one note — not a copy of five.
    let own = child.events().await.unwrap();
    assert_eq!(own.len(), 2, "the branch copies nothing: {own:?}");
    assert_eq!(own[0].body.kind(), "forked");

    // While the parent is untouched.
    assert_eq!(parent.events().await.unwrap().len(), 5);
}

#[tokio::test]
async fn forking_is_cheap_regardless_of_how_long_the_parent_is() {
    // O(1), stated as a measurement rather than a claim: the branch's own file
    // is the same size whether the parent holds five events or five hundred.
    let dir = tempdir();
    let small = Arc::new(journal_in(dir.path(), "small").await);
    for i in 0..5 {
        small
            .append(agent(), EventBody::Note { text: format!("s{i}") })
            .await
            .unwrap();
    }

    let long_dir = tempdir();
    let long = Arc::new(journal_in(long_dir.path(), "long").await);
    for i in 0..500 {
        long.append(
            agent(),
            EventBody::Note {
                text: format!("event number {i} with some body to it"),
            },
        )
        .await
        .unwrap();
    }

    let from_small = journal_fork(dir.path(), "b1", small.clone(), 2).await;
    let from_long = journal_fork(long_dir.path(), "b2", long.clone(), 200).await;

    let small_bytes = std::fs::metadata(from_small.path()).unwrap().len();
    let long_bytes = std::fs::metadata(from_long.path()).unwrap().len();
    assert!(
        long_bytes < small_bytes * 3,
        "a fork from a 500-event parent cost {long_bytes} bytes against \
         {small_bytes} from a 5-event one — that is copying, not referencing"
    );

    // The histories are still correct, which is what makes the cheapness usable.
    assert_eq!(from_small.history().await.unwrap().len(), 2);
    assert_eq!(from_long.history().await.unwrap().len(), 200);
}

#[tokio::test]
async fn the_fork_point_is_recorded_because_the_cache_diverges_there() {
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    for i in 0..4 {
        parent
            .append(agent(), EventBody::Note { text: format!("p{i}") })
            .await
            .unwrap();
    }
    let child = journal_fork(dir.path(), "child", parent.clone(), 2).await;

    assert_eq!(
        child.cache_diverges_at(),
        Some((SessionId::new("parent"), 2)),
        "DESIGN: the cost of a fork must be explicit, not discovered on a miss"
    );
    // And the parent, which is not a fork, diverges nowhere.
    assert_eq!(parent.cache_diverges_at(), None);
}

#[tokio::test]
async fn lineage_survives_more_than_one_generation() {
    let dir = tempdir();
    let first = Arc::new(journal_in(dir.path(), "gen1").await);
    for i in 0..3 {
        first
            .append(agent(), EventBody::Note { text: format!("g1-{i}") })
            .await
            .unwrap();
    }
    let second = Arc::new(journal_fork(dir.path(), "gen2", first.clone(), 2).await);
    second
        .append(agent(), EventBody::Note { text: "g2".into() })
        .await
        .unwrap();
    // Seqs continue across the lineage, so gen2's own events are 2 and 3.
    let third = journal_fork(dir.path(), "gen3", second.clone(), 4).await;

    let lineage = third.lineage();
    assert_eq!(lineage.len(), 2, "both ancestors are named: {lineage:?}");
    assert_eq!(lineage[0], (SessionId::new("gen2"), 4));
    assert_eq!(lineage[1], (SessionId::new("gen1"), 2));

    // A three-generation history is the whole chain, not just the last hop:
    // gen1's two notes and gen2's one, with the two fork markers excluded.
    let history = third.history().await.unwrap();
    assert_eq!(
        history.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 3],
        "the chain resolves through every generation: {history:?}"
    );
}

/// A replay must not perform a side effect the branch already performed.
#[tokio::test]
async fn a_replay_seed_inherits_completed_effects_but_not_interrupted_ones() {
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    parent
        .append(
            agent(),
            EventBody::Effect {
                key: "already-charged".into(),
                description: "took the payment".into(),
                state: EffectState::Done { result_seq: None },
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();
    parent
        .append(
            agent(),
            EventBody::Effect {
                key: "was-interrupted".into(),
                description: "recorded the intent, never acted".into(),
                state: EffectState::Intended,
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();

    let child = journal_fork(dir.path(), "child", parent.clone(), 2).await;
    let seed = child.replay_seed().await.unwrap();

    assert!(
        seed.is_committed("already-charged"),
        "a branch inherits the effects its parent completed"
    );
    assert!(
        !seed.is_committed("was-interrupted"),
        "but not one that was only intended — nobody knows whether it happened"
    );
}

#[tokio::test]
async fn a_branch_that_forks_before_an_effect_does_not_inherit_it() {
    // The reason the fork point is a seq and not "everything the parent has".
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    parent
        .append(agent(), EventBody::Note { text: "before".into() })
        .await
        .unwrap();
    parent
        .append(
            agent(),
            EventBody::Effect {
                key: "charged-later".into(),
                description: "after the fork point".into(),
                state: EffectState::Done { result_seq: None },
                tool_use_id: None,
                idempotent: false,
            },
        )
        .await
        .unwrap();

    let child = journal_fork(dir.path(), "child", parent.clone(), 1).await;
    let seed = child.replay_seed().await.unwrap();
    assert!(
        !seed.is_committed("charged-later"),
        "branching before an effect means not having done it"
    );
}

/// **M6: "fork 一个历史点 → 显示预估成本".**
///
/// DESIGN: "fork 是带成本提示的操作,不是免费调试按钮" — the cost has to be visible
/// *before* the fork, not discovered afterwards. Two costs, pulling in opposite
/// directions, and the estimate has to state them separately or it says nothing:
/// the storage, which is one marker line however long the parent is, and the
/// prefix, which is everything the branch will have to re-establish because the
/// cache stops matching at the fork point. The token figure is the size the
/// record already measured for the last request at or before the point.
#[tokio::test]
async fn the_fork_estimate_separates_the_free_half_from_the_paid_one() {
    let small_dir = tempdir();
    let small = Arc::new(journal_in(small_dir.path(), "p").await);
    for i in 0..5 {
        small
            .append(agent(), EventBody::Note { text: format!("s{i}") })
            .await
            .unwrap();
    }
    small
        .append(
            agent(),
            EventBody::ModelRequest {
                model: "m".into(),
                messages: 3,
                digest: "d".into(),
                est_input_tokens: Some(1234),
            },
        )
        .await
        .unwrap();

    // A request *after* the fork point must not be counted: the branch does not
    // take that turn, so charging for it would overstate the re-send.
    small
        .append(
            agent(),
            EventBody::ModelRequest {
                model: "m".into(),
                messages: 9,
                digest: "later".into(),
                est_input_tokens: Some(99_999),
            },
        )
        .await
        .unwrap();

    // Forking at 6: the branch continues from that seq and inherits everything
    // below it, which is the five notes and the request that measured the prefix.
    let estimate = small.estimate_fork(6).await.unwrap();
    assert_eq!(estimate.parent, SessionId::new("p"));
    assert_eq!(estimate.at_seq, 6);
    assert_eq!(estimate.events, 6, "six events precede the point");
    assert!(estimate.bytes > 0, "the prefix has a size: {estimate}");
    assert_eq!(
        estimate.est_input_tokens,
        Some(1234),
        "the re-send cost is the last measurement at or before the point"
    );
    assert!(
        estimate.marker_bytes < 512,
        "the fork itself is one line: {estimate}"
    );

    // The free half, stated as the thing that does not grow: a parent with a
    // hundred times the history pays the same storage for the branch.
    let long_dir = tempdir();
    let long = Arc::new(journal_in(long_dir.path(), "p").await);
    for i in 0..500 {
        long.append(
            agent(),
            EventBody::Note {
                text: format!("event {i} with a body of its own"),
            },
        )
        .await
        .unwrap();
    }
    let long_estimate = long.estimate_fork(400).await.unwrap();
    assert!(
        long_estimate.marker_bytes <= estimate.marker_bytes + 8,
        "the marker grows with the *digits* of the seq it names, and with nothing \
         else: {long_estimate} against {estimate}"
    );
    assert!(
        long_estimate.bytes > estimate.bytes * 50,
        "and the prefix the branch re-establishes is the part that does grow: \
         {} against {}",
        long_estimate.bytes,
        estimate.bytes
    );
}

/// "真的分叉,原分支不动" — the fork happens, and the parent is untouched.
///
/// The estimate is only worth printing if the operation behind it is real, and
/// the parent is what the branch reads its whole history through: a fork that
/// rewrote, truncated or renumbered it would corrupt the record it was supposed
/// to branch from.
#[tokio::test]
async fn forking_a_point_leaves_the_parent_byte_for_byte_alone() {
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    for i in 0..6 {
        parent
            .append(agent(), EventBody::Note { text: format!("p{i}") })
            .await
            .unwrap();
    }

    let estimate = parent.estimate_fork(4).await.unwrap();
    assert_eq!(estimate.events, 4);

    let before = std::fs::read(parent.path()).unwrap();
    let child = journal_fork(dir.path(), "child", parent.clone(), 4).await;

    assert_eq!(
        std::fs::read(parent.path()).unwrap(),
        before,
        "the parent's bytes changed when a branch was taken from it"
    );
    assert_eq!(parent.len(), 6, "and its record is still its own");
    assert_eq!(
        parent.events().await.unwrap().len(),
        6,
        "a fork must not append to the record it branches from"
    );

    // The branch really is at the point that was estimated: the four events the
    // estimate counted are the four it inherits.
    assert_eq!(child.history().await.unwrap().len(), estimate.events);
    assert_eq!(
        child.cache_diverges_at(),
        Some((SessionId::new("parent"), 4)),
        "the estimate said the cache diverges at the point, and it does"
    );
}

#[tokio::test]
async fn a_forks_thinking_is_readable_through_the_chain() {
    // Whatever the branch inherited must be usable, not merely counted.
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    let digest = parent.blobs().put_str("the earlier reasoning").unwrap();
    parent
        .append(
            agent(),
            EventBody::ModelResponse {
                id: "m1".into(),
                stop_reason: "end_turn".into(),
                usage: yu_observe::TokenUsage::default(),
                blocks: vec!["thinking".into()],
                thinking: vec![ThinkingRecord {
                    text_blob: digest.clone(),
                    signature_blob: None,
                }],
                text_blob: None,
            },
        )
        .await
        .unwrap();

    let child = journal_fork(dir.path(), "child", parent.clone(), 1).await;
    let history = child.history().await.unwrap();
    let EventBody::ModelResponse { thinking, .. } = &history[0].body else {
        panic!("expected the inherited model response");
    };
    assert_eq!(
        child.blobs().get_str(&thinking[0].text_blob).unwrap(),
        "the earlier reasoning",
        "the branch reads the same content-addressed blobs, which is why sharing them is free"
    );
    assert_eq!(digest, thinking[0].text_blob);
}
