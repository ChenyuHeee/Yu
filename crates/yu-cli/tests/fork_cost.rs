//! **M6: "fork 一个历史点 → 显示预估成本 → 真的分叉,原分支不动".**
//!
//! DESIGN is explicit that a fork is not free and must not look free:
//!
//! > **fork 的代价必须显式**:从分叉点起缓存前缀不再匹配。**fork 是带成本提示的
//! > 操作,不是免费调试按钮**.
//!
//! So the three things this asserts are the three in the criterion, in order: the
//! price is shown, the branch happens, and the parent is exactly what it was.
//! Observed on the product — `yu fork` is run as a process — and read off the
//! files rather than off the command's own claims where it matters: the parent's
//! bytes are compared before and after, and the branch's first line is read out of
//! its own journal.

mod common;

use std::path::Path;

use common::{Ran, run_yu};
use yu_journal::{EventBody, Journal, SessionId};
use yu_observe::AgentId;

/// A record with a history worth branching from: a charter, some turns, and a
/// request that measured how big its context was.
///
/// Built through the library rather than through a run because it is the fixture,
/// not the mechanism: what is under test is `yu fork`, and it is run as a process
/// against this record. The `ModelRequest` is here because the estimate's token
/// figure comes from it — a record this fixture could not produce would make the
/// second half of the estimate untestable.
async fn record(dir: &Path, session: &str, events: u64) {
    let journal = Journal::open(dir, SessionId::new(session)).await.unwrap();
    journal
        .append(
            AgentId::new("exec"),
            EventBody::SessionStarted {
                charter: "make the tests pass".to_owned(),
                model: "claude-opus-5".to_owned(),
                quirks: "unknown".to_owned(),
            },
        )
        .await
        .unwrap();
    // The measured request goes in early, so that every point this test forks at
    // has one behind it — the estimate reads the last request at or before the
    // point, and a fixture that put it at the end would make the token figure
    // untestable at every point before the end.
    journal
        .append(
            AgentId::new("exec"),
            EventBody::ModelRequest {
                model: "claude-opus-5".to_owned(),
                messages: 4,
                digest: "d".to_owned(),
                est_input_tokens: Some(4321),
            },
        )
        .await
        .unwrap();
    for i in 2..events {
        journal
            .append(
                AgentId::new("exec"),
                EventBody::Note {
                    text: format!("event {i}"),
                },
            )
            .await
            .unwrap();
    }
}

fn fork(dir: &Path, session: &str, at: u64, into: &str) -> Ran {
    run_yu(
        &common::FakeEndpoint::start(Vec::new()),
        &[
            "fork",
            "--dir",
            dir.to_str().unwrap(),
            "--session",
            session,
            "--at",
            &at.to_string(),
            "--into",
            into,
        ],
        dir,
        dir,
    )
}

#[tokio::test]
async fn a_fork_shows_its_price_takes_the_branch_and_leaves_the_parent_alone() {
    let dir = tempfile::tempdir().unwrap();
    record(dir.path(), "session", 8).await;
    let parent_path = dir.path().join("session.jsonl");
    let before = std::fs::read(&parent_path).unwrap();
    let parent_len = before.len();

    let ran = fork(dir.path(), "session", 5, "branch");

    // ---- 1. the price, before the branch exists
    let estimate_at = ran
        .stderr
        .find("--- fork estimate")
        .expect("the estimate must be printed before the branch is made");
    let forked_at = ran
        .stderr
        .find("--- forked branch at seq 5")
        .expect("the branch must be reported");
    assert!(
        estimate_at < forked_at,
        "the price has to come before the fork, or it is a receipt rather than a \
         warning: {}",
        ran.stderr
    );
    ran.ok();

    // Both halves of the cost, and they are the two different things: what the
    // branch inherits by reference (which does not grow with the parent), and what
    // a replay would have to re-send because the cache stops matching here.
    assert!(
        ran.stderr.contains("5 event(s)"),
        "the estimate must say how much history is inherited: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains("4321 input tokens"),
        "the estimate must carry the re-send cost the record measured: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains("the parent is not written to"),
        "{}",
        ran.stderr
    );

    // ---- 2. the branch really exists, at the point that was priced
    let branch_path = dir.path().join("branch.jsonl");
    assert!(branch_path.is_file(), "no branch was written");
    let branch = Journal::open(dir.path(), SessionId::new("branch"))
        .await
        .unwrap();
    let own = branch.events().await.unwrap();
    assert_eq!(
        own.len(),
        1,
        "a fork copies nothing: the branch's own file is its marker and no more: \
         {:?}",
        own.iter().map(|e| e.body.kind()).collect::<Vec<_>>()
    );
    let EventBody::Forked {
        parent,
        parent_at_seq,
    } = &own[0].body
    else {
        panic!("a branch's first record must say where it came from: {:?}", own[0]);
    };
    assert_eq!(parent, &SessionId::new("session"));
    assert_eq!(*parent_at_seq, 5);
    // The history it reads is the parent's, through the parent — the branch's own
    // file holds one line and its history holds five.
    assert_eq!(branch.history().await.unwrap().len(), 5);
    // Where the cache stops matching is a fact the *forking* process reports: a
    // journal reopened from disk later does not hold a handle to its parent, which
    // is why the marker in the record is the durable half of this and the printed
    // line is the operation's own answer.
    assert!(
        ran.stderr.contains("diverges at session:5"),
        "the fork must say where the prefix stops matching, not leave it to be \
         discovered on a cache miss: {}",
        ran.stderr
    );

    // ---- 3. the original branch did not move
    assert_eq!(
        std::fs::read(&parent_path).unwrap(),
        before,
        "the parent's bytes changed when a branch was taken from it"
    );
    assert_eq!(
        std::fs::metadata(&parent_path).unwrap().len(),
        parent_len as u64
    );
    let parent = Journal::open(dir.path(), SessionId::new("session"))
        .await
        .unwrap();
    assert_eq!(
        parent.len(),
        8,
        "and nothing was appended to the record the branch came from"
    );

    // A branch onto a name that already has a record is refused rather than
    // appended to: the marker would otherwise land in somebody else's branch.
    let second = fork(dir.path(), "session", 3, "branch");
    assert!(!second.status.success());
    assert!(
        second.stderr.contains("already has a record"),
        "{}",
        second.stderr
    );
}

/// The estimate is a measurement of the point, not of the session: two points in
/// one record are priced differently, and the one further along costs more of the
/// prefix.
#[tokio::test]
async fn the_price_is_a_function_of_the_point() {
    let dir = tempfile::tempdir().unwrap();
    record(dir.path(), "session", 20).await;

    let early = fork(dir.path(), "session", 3, "early");
    let late = fork(dir.path(), "session", 15, "late");
    early.ok();
    late.ok();

    let tokens = |text: &str| -> u64 {
        text.split("measured at ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("no measured re-send cost in: {text}"))
    };
    assert!(
        tokens(&late.stderr) == tokens(&early.stderr),
        "both points are measured by the same recorded request, so the token \
         figure is the same: {}",
        late.stderr
    );
    // What differs is how much of the record the branch inherits — and that is
    // the part a caller is choosing between.
    assert!(
        late.stderr.contains("15 event(s)") && early.stderr.contains("3 event(s)"),
        "each point must report its own history: {} / {}",
        early.stderr,
        late.stderr
    );
}
