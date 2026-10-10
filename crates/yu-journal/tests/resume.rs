//! Reopening, addressable reads, and the seekable index.
//!
//! DESIGN's L2 is "可索引、可寻址" and L3 needs "任意历史点可被精确重建". Both rest on
//! the same property: a seq is an address that still means the same thing after
//! the process that wrote it is gone.

mod common;

use common::{agent, journal_in, tempdir};
use yu_journal::{EventBody, Journal, JournalIndex, SessionId};

#[tokio::test]
async fn reopening_resumes_seq_numbering() {
    let dir = tempdir();

    for round in 0..3u64 {
        let journal = journal_in(dir.path(), "s").await;
        assert_eq!(journal.len(), round as usize, "events survive the restart");
        let event = journal
            .append(
                agent(),
                EventBody::Note {
                    text: format!("round {round}"),
                },
            )
            .await
            .unwrap();
        assert_eq!(event.seq, round, "numbering resumes, it does not restart");
    }

    let journal = journal_in(dir.path(), "s").await;
    assert_eq!(journal.len(), 3);
    let seqs: Vec<u64> = journal
        .events()
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2]);
}

#[tokio::test]
async fn read_from_returns_only_later_events() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..5u64 {
        journal
            .append(agent(), EventBody::Note { text: format!("{i}") })
            .await
            .unwrap();
    }

    let tail = journal.read_from(3).await.unwrap();
    assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4]);
    assert!(journal.read_from(5).await.unwrap().is_empty());
    assert_eq!(journal.read_from(0).await.unwrap(), journal.events().await.unwrap());

    // A seq that was never written still addresses the tail after it, so a
    // reader holding a stale seq is not stranded.
    assert_eq!(journal.read_from(4).await.unwrap()[0].seq, 4);
}

#[tokio::test]
async fn the_index_offsets_are_the_line_starts() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    for i in 0..8u64 {
        journal
            .append(
                agent(),
                EventBody::Note {
                    text: format!("record {i}"),
                },
            )
            .await
            .unwrap();
    }

    // Rebuilt from the file on disk, the way `Journal::open` does it.
    let index = JournalIndex::build(journal.path()).unwrap();
    assert_eq!(index.len(), 8);
    let bytes = std::fs::read(journal.path()).unwrap();
    for event in journal.events().await.unwrap() {
        let offset = index.offset_of(event.seq).unwrap() as usize;
        assert!(
            bytes[offset..].starts_with(format!("{{\"seq\":{}", event.seq).as_bytes()),
            "seq {} should be addressable at its own line start",
            event.seq
        );
        assert!(offset == 0 || bytes[offset - 1] == b'\n');
    }
    assert_eq!(index.offset_of(8), None);
}

#[tokio::test]
async fn an_addressable_read_survives_a_restart() {
    let dir = tempdir();
    {
        let journal = journal_in(dir.path(), "s").await;
        for i in 0..4u64 {
            journal
                .append(agent(), EventBody::Note { text: format!("{i}") })
                .await
                .unwrap();
        }
    }

    // DESIGN's换代 flow reads the record from a fresh process: the address has to
    // mean the same thing there.
    let journal = journal_in(dir.path(), "s").await;
    let tail = journal.read_from(2).await.unwrap();
    assert_eq!(tail.len(), 2);
    assert!(tail.iter().all(|e| e.ts_ms > 0));
    assert_eq!(tail[0].session, SessionId::new("s"));
}

#[tokio::test]
async fn a_large_result_does_not_hide_later_events() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;

    // A megabyte inline in one line: the index must still find what follows it,
    // which is why it is built from byte offsets rather than by line number.
    journal
        .append(
            agent(),
            EventBody::ToolResult {
                tool_use_id: "call_1".into(),
                name: "read".into(),
                kind: "file".into(),
                bytes: 1 << 20,
                reproducible: true,
                addressable: false,
                inline: Some("z".repeat(1 << 20)),
                blob: None,
                reduction: None,
            },
        )
        .await
        .unwrap();
    journal
        .append(agent(), EventBody::Note { text: "after".into() })
        .await
        .unwrap();

    let index = JournalIndex::build(journal.path()).unwrap();
    let second = index.offset_of(1).unwrap() as usize;
    let bytes = std::fs::read(journal.path()).unwrap();
    assert!(bytes[second..].starts_with(b"{\"seq\":1"));
    assert_eq!(journal.read_from(1).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_corrupt_record_is_visible_rather_than_skipped() {
    let dir = tempdir();
    let journal = journal_in(dir.path(), "s").await;
    journal
        .append(agent(), EventBody::Note { text: "a".into() })
        .await
        .unwrap();
    let path = journal.path().to_path_buf();
    drop(journal);

    // Damage the file the way a stray write would, leaving the terminator intact.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"seq\":1,\"ts_ms\":");
    text.push('\n');
    std::fs::write(&path, text).unwrap();

    assert!(matches!(
        JournalIndex::build(&path),
        Err(yu_journal::JournalError::Corrupt { .. })
    ));
    // A record that is present but unreadable is not silently absent.
    assert!(matches!(
        Journal::open(dir.path(), SessionId::new("s")).await,
        Err(yu_journal::JournalError::Corrupt { .. })
    ));
}
