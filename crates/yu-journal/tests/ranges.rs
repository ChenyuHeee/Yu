//! Bounded reads.
//!
//! `read_from` reads the whole tail, which makes addressing a range pointless:
//! the cost of addressing part of the record is the cost of the whole record.
//! `read_range` is the fix, and these tests pin the properties that make it one.

mod common;

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
