//! Bounded reads.
//!
//! `read_from` reads the whole tail, which makes addressing a range pointless:
//! the cost of addressing part of the record is the cost of the whole record.
//! `read_range` is the fix, and these tests pin the properties that make it one.

mod common;

use std::sync::Arc;

use common::*;
use yu_journal::EventBody;

#[tokio::test]
async fn a_range_returns_exactly_that_range() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..6 {
        journal
            .append(agent(), EventBody::Note { text: format!("n{i}") })
            .await
            .unwrap();
    }

    let ranged = journal.read_range(2, 4).await.unwrap();
    assert_eq!(ranged.len(), 2, "half-open [2,4) is two events");
    assert_eq!(ranged[0].seq, 2);
    assert_eq!(ranged[1].seq, 3);

    // Same events the unbounded read would have produced for that span.
    let all = journal.events().await.unwrap();
    assert_eq!(ranged, all[2..4].to_vec());
}

#[tokio::test]
async fn the_upper_bound_is_exclusive_and_an_empty_range_is_empty() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..4 {
        journal
            .append(agent(), EventBody::Note { text: format!("n{i}") })
            .await
            .unwrap();
    }
    assert!(journal.read_range(2, 2).await.unwrap().is_empty());
    assert!(journal.read_range(3, 1).await.unwrap().is_empty());
    assert_eq!(journal.read_range(0, 1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_range_running_past_the_end_is_bounded_rather_than_unbounded() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..3 {
        journal
            .append(agent(), EventBody::Note { text: format!("n{i}") })
            .await
            .unwrap();
    }
    // `until` beyond the last event falls back to the committed end, not to
    // "read to the end of whatever is there".
    let ranged = journal.read_range(1, 9_999).await.unwrap();
    assert_eq!(ranged.len(), 2);
    assert_eq!(ranged[0].seq, 1);
}

/// The bound is real: a range read must not look at the tail at all.
///
/// Proven by planting a *readable* extra record past the requested range. An
/// unbounded read finds it; a bounded one cannot, because it never reads that
/// far. (The first version of this test damaged the tail instead, which proved
/// nothing: a torn tail is dropped rather than reported, so both reads agreed.)
#[tokio::test]
async fn a_range_read_never_reads_past_its_upper_bound() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..5 {
        journal
            .append(agent(), EventBody::Note { text: format!("n{i}") })
            .await
            .unwrap();
    }

    // A well-formed record with a seq far outside the requested range, written
    // straight to the file so the index does not know about it.
    let planted = format!(
        r#"{{"seq":999,"ts_ms":0,"session":"s","agent":"planted","body":{{"event":"note","text":"tail"}}}}"#
    );
    let path = journal.path().to_path_buf();
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    use std::io::Write;
    file.write_all(planted.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);

    let ranged = journal.read_range(0, 3).await.unwrap();
    assert_eq!(ranged.len(), 3);
    assert!(
        ranged.iter().all(|e| e.seq < 3),
        "a bounded read must not return anything past its bound"
    );

    // Sanity: the planted record is genuinely readable, so the range read's
    // silence about it is about where it stopped, not about it being unparseable.
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"seq\":999"), "the planted record is on disk");
}

/// The bound is a bound on a branch too, where `until` names a seq this branch
/// never wrote.
///
/// A branch's index holds only the seqs *it* recorded — everything at or below
/// the fork point is read through the parent — so `until` is absent from it for
/// any range ending before the branch's first own event. Looking the bound up
/// with `offset_of` answered `None` there and fell back to the committed end of
/// the branch's file, so the read came back with the branch's whole file: events
/// at and past the exclusive bound, which is the read an addressed range exists
/// not to be.
#[tokio::test]
async fn a_branchs_range_stops_at_a_bound_the_branch_does_not_hold() {
    let dir = tempdir();
    let parent = Arc::new(journal_in(dir.path(), "parent").await);
    for i in 0..6 {
        parent
            .append(agent(), EventBody::Note { text: format!("p{i}") })
            .await
            .unwrap();
    }

    // The branch's own addresses start at its fork point: the marker at 4, then
    // the three notes it records for itself.
    let child = journal_fork(dir.path(), "child", parent.clone(), 4).await;
    for i in 0..3 {
        child
            .append(agent(), EventBody::Note { text: format!("c{i}") })
            .await
            .unwrap();
    }
    assert_eq!(
        child
            .events()
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![4, 5, 6, 7],
        "the branch holds none of the seqs below its fork point"
    );

    let ranged = child.read_range(0, 3).await.unwrap();
    assert!(
        ranged.iter().all(|e| e.seq < 3),
        "a range must not return events at or past its exclusive bound: {ranged:?}"
    );
    assert!(
        ranged.is_empty(),
        "nothing in this branch sits below seq 3: {ranged:?}"
    );
}
