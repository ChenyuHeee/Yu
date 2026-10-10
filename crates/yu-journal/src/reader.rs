//! The seekable index: `seq` → byte offset.
//!
//! DESIGN's L2 is "append-only、类型化、可索引、可寻址" and its survey of prior art
//! names Codex's `seekable_reader.rs` + `session_index.rs` as the shape to copy —
//! a record you jump into, not a transcript you read from the start. This is that
//! index.
//!
//! It is rebuilt by scanning the log at open rather than kept in a sidecar file.
//! A sidecar is a second thing that can disagree with the log; a scan cannot. The
//! cost is one pass over the file per open, which is the price of the index never
//! being able to lie about what is on disk.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use crate::JournalError;
use crate::event::{Event, EventHead};

/// Byte offset of every recorded event, by seq.
#[derive(Debug, Clone, Default)]
pub struct JournalIndex {
    offsets: BTreeMap<u64, u64>,
    /// Bytes up to and including the last newline — the end of the last record
    /// that was fully written. A file longer than this ends in a torn append.
    committed: u64,
    /// The session named by the first record, so reopening the same file under a
    /// different session id is a visible error rather than silent mixing.
    session: Option<String>,
}

impl JournalIndex {
    /// Scan `path` and index every complete record in it.
    ///
    /// A trailing record without its newline is not indexed: a record is
    /// terminated by construction, so an unterminated remnant is a torn append
    /// (or a read that raced a writer), never a record that was silently dropped.
    pub fn build(path: &Path) -> Result<Self, JournalError> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut index = JournalIndex::default();
        let mut buf = Vec::new();
        let mut offset: u64 = 0;

        loop {
            buf.clear();
            let read = reader.read_until(b'\n', &mut buf)?;
            if read == 0 {
                break;
            }
            if buf.last() != Some(&b'\n') {
                tracing::warn!(
                    offset,
                    bytes = read,
                    "journal ends in a partial record; it is not indexed"
                );
                break;
            }
            let start = offset;
            offset += read as u64;

            let line = &buf[..buf.len() - 1];
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let head: EventHead = serde_json::from_slice(line).map_err(|err| {
                JournalError::Corrupt {
                    offset: start,
                    reason: err.to_string(),
                }
            })?;
            if index.offsets.insert(head.seq, start).is_some() {
                // Two records claiming one address means two writers, or a file
                // that was edited. Either way the address is no longer a fact.
                return Err(JournalError::Corrupt {
                    offset: start,
                    reason: format!("seq {} appears more than once", head.seq),
                });
            }
            if index.session.is_none() {
                index.session = head.session;
            }
        }

        index.committed = offset;
        Ok(index)
    }

    /// Byte offset where `seq` begins, if it was recorded.
    pub fn offset_of(&self, seq: u64) -> Option<u64> {
        self.offsets.get(&seq).copied()
    }

    /// How many records are indexed.
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// Offset of the first record at or after `seq`.
    ///
    /// Addressable reads start here. Taking the *next* record rather than
    /// insisting on an exact match means a caller holding a seq that was never
    /// written still gets the tail it asked for instead of an error.
    pub(crate) fn offset_at_or_after(&self, seq: u64) -> Option<u64> {
        self.offsets.range(seq..).next().map(|(_, offset)| *offset)
    }

    /// End of the last complete record.
    pub(crate) fn committed_len(&self) -> u64 {
        self.committed
    }

    /// The session named by the first record.
    pub(crate) fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// The seq the next append must use.
    pub(crate) fn next_seq(&self) -> u64 {
        self.offsets
            .keys()
            .next_back()
            .map(|seq| seq + 1)
            .unwrap_or(0)
    }

    /// Record an append of `end - offset` bytes. The index is derived state, so
    /// it is written *after* the line is in the file.
    pub(crate) fn insert(&mut self, seq: u64, offset: u64, end: u64) {
        self.offsets.insert(seq, offset);
        self.committed = self.committed.max(end);
    }
}

/// Decode complete JSONL records, starting at byte `base` (used for error
/// offsets only).
pub(crate) fn parse_events(bytes: &[u8], base: u64) -> Result<Vec<Event>, JournalError> {
    let mut events = Vec::new();
    let mut offset = base;
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let start = offset;
        offset += line.len() as u64;
        let Some(body) = line.strip_suffix(b"\n") else {
            // See `JournalIndex::build`: unterminated means "not a record".
            break;
        };
        if body.is_empty() {
            continue;
        }
        let event = serde_json::from_slice(body).map_err(|err| JournalError::Corrupt {
            offset: start,
            reason: err.to_string(),
        })?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBody, SessionId};
    use std::io::Write;

    fn write_log(dir: &Path, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.join("s.jsonl");
        let mut file = File::create(&path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        path
    }

    fn line(seq: u64, kind: &str) -> String {
        let event = Event {
            seq,
            ts_ms: 1,
            session: SessionId::new("s"),
            agent: yu_observe::AgentId::new("a"),
            body: EventBody::Note {
                text: kind.to_owned(),
            },
        };
        serde_json::to_string(&event).unwrap()
    }

    #[test]
    fn offsets_point_at_the_start_of_each_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[&line(0, "a"), &line(1, "b"), &line(2, "c")]);
        let bytes = std::fs::read(&path).unwrap();
        let index = JournalIndex::build(&path).unwrap();

        assert_eq!(index.len(), 3);
        for seq in 0..3u64 {
            let offset = index.offset_of(seq).unwrap() as usize;
            assert_eq!(bytes[offset], b'{', "seq {seq} should start a line");
            assert!(
                offset == 0 || bytes[offset - 1] == b'\n',
                "seq {seq} should start right after a newline"
            );
        }
        assert_eq!(index.committed_len(), bytes.len() as u64);
        assert_eq!(index.session(), Some("s"));
    }

    #[test]
    fn unknown_seqs_have_no_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[&line(0, "a"), &line(1, "b")]);
        let index = JournalIndex::build(&path).unwrap();
        assert_eq!(index.offset_of(2), None);
        assert_eq!(index.offset_at_or_after(2), None);
        assert_eq!(index.offset_at_or_after(1), index.offset_of(1));
        assert_eq!(index.next_seq(), 2);
    }

    #[test]
    fn an_empty_file_indexes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[]);
        let index = JournalIndex::build(&path).unwrap();
        assert!(index.is_empty());
        assert_eq!(index.next_seq(), 0);
        assert_eq!(index.committed_len(), 0);
        assert!(index.session().is_none());
    }

    #[test]
    fn a_missing_file_is_an_error_not_an_empty_index() {
        let dir = tempfile::tempdir().unwrap();
        let err = JournalIndex::build(&dir.path().join("nope.jsonl")).unwrap_err();
        assert!(matches!(err, JournalError::Io(_)));
    }

    #[test]
    fn a_torn_tail_is_stopped_at_not_read_as_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[&line(0, "a")]);
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"seq\":1,\"ts_ms\":1,\"sess").unwrap();
        drop(file);

        let index = JournalIndex::build(&path).unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index.next_seq(), 1);
        // The partial write is not part of any record, so it is not committed.
        assert_eq!(index.committed_len(), good_len);
    }

    #[test]
    fn a_corrupt_record_is_reported_with_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[&line(0, "a"), "not json at all"]);
        match JournalIndex::build(&path) {
            Err(JournalError::Corrupt { offset, reason }) => {
                assert_eq!(offset, line(0, "a").len() as u64 + 1);
                assert!(!reason.is_empty());
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn a_repeated_seq_is_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_log(dir.path(), &[&line(0, "a"), &line(0, "b")]);
        assert!(matches!(
            JournalIndex::build(&path),
            Err(JournalError::Corrupt { .. })
        ));
    }

    #[test]
    fn parse_events_stops_at_an_unterminated_remnant() {
        let good = line(0, "a");
        let mut bytes = good.clone().into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"seq\":1,\"ts_ms\":1");
        let events = parse_events(&bytes, 0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 0);
    }
}
