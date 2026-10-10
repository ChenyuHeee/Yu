//! The append-only journal: one event, one JSONL line.
//!
//! DESIGN gives L2 and L3 one mechanism: "L2 日志就是 journal —— 一个机制两个用途:
//! 回放的基础 = 记忆的载体". The file is therefore both what `render()` reads and
//! what replay walks, which is why it is addressed by `seq` rather than by
//! position in a transcript, and why reopening it resumes numbering instead of
//! restarting.
//!
//! Writing is exclusive to one holder per file: one writer per journal. DESIGN's
//! process model keeps the session in the daemon, so this is the daemon's file;
//! a second writer appending would duplicate seqs, which the index reports as
//! corruption rather than papering over.

use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Mutex;

use yu_observe::AgentId;

use crate::JournalError;
use crate::blob::{BlobStore, digest_of};
use crate::event::{Event, EventBody, SessionId};
use crate::reader::{JournalIndex, parse_events};

/// Directory holding the content-addressed store, shared by every session in the
/// same directory: a fork then references its parent's blobs instead of copying
/// them, which is what takes fork's cost from O(record) to O(1) (DESIGN, L3).
const BLOBS_DIR: &str = "blobs";

/// An append-only event log plus the blob store its references point into.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    path: PathBuf,
    session: SessionId,
    blobs: BlobStore,
    writer: Mutex<Writer>,
    index: RwLock<JournalIndex>,
    /// Records the cost of journal IO.
    ///
    /// Held here rather than timed at each call site because DESIGN's headline
    /// measurement — per-step overhead with model time removed — is *defined* as
    /// the sum of the non-model spans. Journal IO is one of them, and a call
    /// site that forgot to time itself would understate the harness's own cost
    /// while reporting a smaller number as though it were better.
    timeline: Option<std::sync::Arc<yu_observe::Timeline>>,
}

#[derive(Debug)]
struct Writer {
    file: tokio::fs::File,
    /// Byte offset just past the last committed record: where the next line starts.
    offset: u64,
    next_seq: u64,
    /// Why this writer no longer appends: a write failed and cutting its
    /// fragment back out of the file failed too. The file may end inside a
    /// record and this process can no longer tell where the boundary is, so
    /// nothing more may be appended; reopening repairs the tail.
    poisoned: Option<String>,
    /// Test-only: the next append's staged failure. See [`Fault`].
    #[cfg(test)]
    fault: Fault,
}

/// A staged write failure for the tests.
///
/// The failure that matters here — a disk that takes a prefix of the line and
/// then refuses the rest — cannot be produced on demand with a healthy file, so
/// the test arms it at the seam.
#[cfg(test)]
#[derive(Debug, Default)]
struct Fault {
    /// Accept this many bytes of the next write, then fail it.
    write_after: Option<usize>,
    /// Also fail the cut-back that follows: the only way to reach a poisoned
    /// writer.
    cut_back: bool,
}

impl Writer {
    /// Write the line and hand it to the OS.
    ///
    /// The error may leave a prefix of the line in the file — that is what a
    /// full disk does — which is why the caller rolls back rather than only
    /// reporting.
    async fn write_line(&mut self, line: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        if let Some(bytes) = self.fault.write_after.take() {
            self.file.write_all(&line[..bytes.min(line.len())]).await?;
            return Err(std::io::Error::other("staged write failure"));
        }
        self.file.write_all(line).await?;
        // Reaches the OS, so a process that dies now still has this record.
        // Machine-level durability (fsync) is a checkpoint's job, not the job of
        // every append.
        self.file.flush().await
    }

    /// Cut a failed append back to the last committed record, and report why the
    /// append failed.
    ///
    /// The file is O_APPEND, so the kernel — not `offset` — chose where the
    /// fragment landed; cutting to `offset` is the only way back to the
    /// boundary. A fragment left in place welds the next line onto itself into
    /// one line that decodes as nothing — the corruption `repair_torn_tail`
    /// drops at open time, prevented here at write time.
    ///
    /// If even the cut fails, the writer poisons itself: it cannot know where
    /// the boundary is any more, so it must not append again. The fragment is
    /// then an ordinary torn record, which the next open repairs.
    async fn rollback(&mut self, offset: u64, cause: std::io::Error) -> JournalError {
        match self.cut_back_to(offset).await {
            Ok(()) => JournalError::Io(cause),
            Err(cut) => {
                let reason = format!(
                    "append failed ({cause}), and cutting the fragment back to byte {offset} \
                     failed too ({cut}); no further append is possible until the journal is reopened"
                );
                tracing::warn!("{reason}");
                self.poisoned = Some(reason.clone());
                JournalError::Io(std::io::Error::new(cause.kind(), reason))
            }
        }
    }

    /// Truncate the file to `len`: the end of the last committed record.
    async fn cut_back_to(&mut self, len: u64) -> std::io::Result<()> {
        #[cfg(test)]
        if std::mem::take(&mut self.fault.cut_back) {
            return Err(std::io::Error::other("staged cut-back failure"));
        }
        self.file.set_len(len).await
    }
}

impl Journal {
    /// Open (or continue) the journal for `session` inside `dir`.
    ///
    /// Reopening an existing file rebuilds the index from it and resumes seq
    /// numbering — the record is the only source of truth about where it ends.
    pub async fn open(dir: impl Into<PathBuf>, session: SessionId) -> Result<Self, JournalError> {
        let dir = dir.into();
        if session.as_str().is_empty() {
            return Err(JournalError::EmptySessionId);
        }
        tokio::fs::create_dir_all(&dir).await?;

        let path = dir.join(event_log_file_name(&session));
        let blobs = BlobStore::open(dir.join(BLOBS_DIR))?;

        let mut index = JournalIndex::default();
        if tokio::fs::try_exists(&path).await? {
            index = JournalIndex::build(&path)?;
            if let Some(found) = index.session()
                && found != session.as_str()
            {
                return Err(JournalError::SessionMismatch {
                    found: found.to_owned(),
                    expected: session.as_str().to_owned(),
                });
            }
            Self::repair_torn_tail(&path, index.committed_len()).await?;
        }

        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let offset = index.committed_len();
        let next_seq = index.next_seq();

        Ok(Self {
            dir,
            path,
            session,
            blobs,
            writer: Mutex::new(Writer {
                file,
                offset,
                next_seq,
                poisoned: None,
                #[cfg(test)]
                fault: Fault::default(),
            }),
            index: RwLock::new(index),
            timeline: None,
        })
    }

    /// Append one event. Returns it with its assigned `seq` and timestamp.
    ///
    /// An append commits the whole line or leaves the file as it was: a write
    /// that fails partway is cut back to the last committed record, so a failure
    /// never strands a fragment for the next line to weld onto.
    ///
    /// Blob references are not resolved here. Append is a write, and the store
    /// answers "does this reference point at anything" on read, where the answer
    /// is reported as [`JournalError::BlobMissing`] rather than swallowed.
    pub async fn append(&self, agent: AgentId, body: EventBody) -> Result<Event, JournalError> {
        body.check_stored_once()?;

        let timer = self
            .timeline
            .as_ref()
            .map(|t| t.start_for(yu_observe::SpanKind::Journal, agent.clone()));
        let kind = body.kind();
        let result = self.append_inner(agent, body).await;
        if let Some(t) = timer {
            let span = t
                .attr("event", kind)
                .attr("ok", result.is_ok())
                .finish();
            tracing::trace!(elapsed_us = span.elapsed_ns / 1000, kind, "journal append");
        }
        result
    }

    /// Attach a timeline, so journal IO is charged to the harness's own cost.
    pub fn with_timeline(mut self, timeline: std::sync::Arc<yu_observe::Timeline>) -> Self {
        self.timeline = Some(timeline);
        self
    }

    async fn append_inner(&self, agent: AgentId, body: EventBody) -> Result<Event, JournalError> {
        let mut writer = self.writer.lock().await;
        if let Some(reason) = &writer.poisoned {
            return Err(JournalError::Io(std::io::Error::other(reason.clone())));
        }
        let seq = writer.next_seq;
        let event = Event {
            seq,
            ts_ms: yu_observe::now_ms(),
            session: self.session.clone(),
            agent,
            body,
        };

        // Constraint 2: one line, written once. The envelope appears here and
        // nowhere else, however many content blocks this event covers — the cost
        // of recording an event is one envelope, not one per block.
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');

        let offset = writer.offset;
        if let Err(err) = writer.write_line(&line).await {
            return Err(writer.rollback(offset, err).await);
        }
        writer.offset = offset + line.len() as u64;
        writer.next_seq = seq + 1;
        drop(writer);

        let bytes = line.len();
        self.index_write().insert(seq, offset, offset + bytes as u64);
        tracing::debug!(seq, kind = event.body.kind(), bytes, "appended");
        Ok(event)
    }

    /// Every event in the journal, in order.
    pub async fn events(&self) -> Result<Vec<Event>, JournalError> {
        self.read_from(0).await
    }

    /// Addressable read: only events with `seq >= from`.
    pub async fn read_from(&self, from: u64) -> Result<Vec<Event>, JournalError> {
        // The offset is taken while the index is locked, then the lock is dropped
        // before the read: a reader must never hold up an append.
        let Some(start) = self.index_read().offset_at_or_after(from) else {
            return Ok(Vec::new());
        };

        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        parse_events(&bytes, start)
    }

    /// Read a bounded range of events, `from` inclusive and `until` exclusive.
    ///
    /// `read_from` seeks to the offset and then reads to the end of the file, so
    /// reading event 3 of a million materialises the other 999 997 — which makes
    /// addressing a range pointless, because the cost of addressing it is the
    /// cost of reading everything. Both offsets are already in the index, so the
    /// only missing piece was stopping.
    ///
    /// This is the precondition for `delegate_read`: a reader is sent at a
    /// *range* of the record, and a range that costs the whole record is not a
    /// reduction.
    pub async fn read_range(&self, from: u64, until: u64) -> Result<Vec<Event>, JournalError> {
        if until <= from {
            return Ok(Vec::new());
        }
        let (start, end) = {
            let index = self.index_read();
            let Some(start) = index.offset_at_or_after(from) else {
                return Ok(Vec::new());
            };
            // Exclusive upper bound: where `until` begins, or the end of what has
            // been committed. Falling back to the file end rather than to
            // "unbounded" keeps the read bounded even for a range that runs past
            // the last event.
            let end = index.offset_of(until).unwrap_or_else(|| index.committed_len());
            (start, end)
        };
        if end <= start {
            return Ok(Vec::new());
        }

        let mut file = tokio::fs::File::open(&self.path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        // Exactly the span, so the tail is never touched.
        let mut bytes = vec![0u8; (end - start) as usize];
        file.read_exact(&mut bytes).await?;
        parse_events(&bytes, start)
    }

    /// The blob store events in this journal reference.
    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Number of events recorded.
    pub fn len(&self) -> usize {
        self.index_read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The directory this journal lives in (log and blobs).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The JSONL file itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Drop a trailing record that was never terminated.
    ///
    /// A torn append was never committed, and leaving it would weld the next
    /// append onto the end of it — destroying two records to preserve a fragment.
    /// The log is append-only for *records*; an unterminated remnant is not one.
    async fn repair_torn_tail(path: &Path, committed: u64) -> Result<(), JournalError> {
        let on_disk = tokio::fs::metadata(path).await?.len();
        if on_disk <= committed {
            return Ok(());
        }
        tracing::warn!(
            path = %path.display(),
            dropped_bytes = on_disk - committed,
            "journal ends in a partial record; truncating it"
        );
        let file = tokio::fs::OpenOptions::new().write(true).open(path).await?;
        file.set_len(committed).await?;
        file.sync_all().await?;
        Ok(())
    }

    /// The file is the source of truth and the index is a view derived from it,
    /// so a panic elsewhere must not make reads fail: take the lock back rather
    /// than propagating the poison.
    fn index_read(&self) -> RwLockReadGuard<'_, JournalIndex> {
        self.index.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn index_write(&self) -> RwLockWriteGuard<'_, JournalIndex> {
        self.index.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// File name for a session's log.
///
/// Session ids come from the caller and are not path components: `a/b` would
/// write into another directory. Unsafe characters become `_`, and an id that had
/// to be rewritten carries a digest suffix — two distinct ids must never be able
/// to land on the same file, because that is silent mixing of two records.
fn event_log_file_name(session: &SessionId) -> String {
    let name = session.as_str();
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();

    if !safe.is_empty() && safe == name && safe != "." && safe != ".." {
        return format!("{safe}.jsonl");
    }
    let stem = if safe.is_empty() { "session" } else { &safe };
    format!("{stem}-{}.jsonl", &digest_of(name.as_bytes())[..12])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> AgentId {
        AgentId::new("exec")
    }

    async fn open(dir: &Path, session: &str) -> Journal {
        Journal::open(dir, SessionId::new(session)).await.unwrap()
    }

    /// Arm the write path's failure for the next append: `bytes` of the line are
    /// accepted and the rest refused, and the rollback that follows fails too if
    /// `cut_back`.
    async fn fail_next_write(journal: &Journal, bytes: usize, cut_back: bool) {
        journal.writer.lock().await.fault = Fault {
            write_after: Some(bytes),
            cut_back,
        };
    }

    /// Make the log readable through a fresh descriptor.
    ///
    /// macOS updates a file's size lazily for descriptors other than the one
    /// that wrote it, so a read through a new descriptor can miss bytes that are
    /// already in the file. fsync settles the size; a test that reads the log
    /// behind the writer's back has to ask for that.
    async fn sync(journal: &Journal) {
        journal.writer.lock().await.file.sync_all().await.unwrap();
    }

    #[tokio::test]
    async fn a_write_that_fails_partway_is_cut_back_so_the_log_still_opens() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path(), "s").await;
        journal
            .append(agent(), EventBody::Note { text: "a".into() })
            .await
            .unwrap();
        sync(&journal).await;
        let before = std::fs::read(journal.path()).unwrap();

        // ENOSPC's shape: the disk accepts a prefix of the line, then refuses.
        fail_next_write(&journal, 5, false).await;
        let err = journal
            .append(agent(), EventBody::Note { text: "b".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Io(_)));

        // The fragment was cut back, so the file is byte-for-byte what it was
        // before the failed append, and the index did not move either.
        assert_eq!(std::fs::read(journal.path()).unwrap(), before);
        assert_eq!(journal.len(), 1);

        // The failed seq was never consumed, and the retry lands as a clean line.
        let event = journal
            .append(agent(), EventBody::Note { text: "b".into() })
            .await
            .unwrap();
        assert_eq!(event.seq, 1);
        sync(&journal).await;
        assert_eq!(
            std::fs::read_to_string(journal.path())
                .unwrap()
                .lines()
                .count(),
            2
        );

        // The property a left-behind fragment would destroy: a reopen that finds
        // only whole records.
        let reopened = open(dir.path(), "s").await;
        assert_eq!(reopened.events().await.unwrap().len(), 2);
        assert_eq!(reopened.read_from(1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_append_that_cannot_be_cut_back_refuses_the_next_one() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path(), "s").await;
        journal
            .append(agent(), EventBody::Note { text: "a".into() })
            .await
            .unwrap();

        fail_next_write(&journal, 5, true).await;
        let err = journal
            .append(agent(), EventBody::Note { text: "b".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Io(_)));

        // The fragment is still in the file, so the writer must refuse instead of
        // welding the next line onto it: the file is untouched by the attempt.
        sync(&journal).await;
        let fragment = std::fs::read_to_string(journal.path()).unwrap();
        assert!(!fragment.ends_with('\n'));
        let err = journal
            .append(agent(), EventBody::Note { text: "c".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Io(_)));
        sync(&journal).await;
        assert_eq!(std::fs::read_to_string(journal.path()).unwrap(), fragment);

        // Reopening repairs the torn tail — the fragment is dropped, not welded —
        // and appends resume where the last committed record ended.
        let reopened = open(dir.path(), "s").await;
        assert_eq!(reopened.len(), 1);
        let event = reopened
            .append(agent(), EventBody::Note { text: "c".into() })
            .await
            .unwrap();
        assert_eq!(event.seq, 1);
        sync(&reopened).await;
        assert_eq!(
            std::fs::read_to_string(journal.path())
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn appends_are_numbered_in_order_and_written_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path(), "s").await;
        assert!(journal.is_empty());

        for i in 0..3 {
            let event = journal
                .append(
                    agent(),
                    EventBody::Note {
                        text: format!("note {i}"),
                    },
                )
                .await
                .unwrap();
            assert_eq!(event.seq, i);
            assert_eq!(event.session, SessionId::new("s"));
            assert_eq!(event.agent, agent());
            assert!(event.ts_ms > 0);
        }

        assert_eq!(journal.len(), 3);
        let events = journal.events().await.unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].body.kind(), "note");

        // One line per event: the file's line count is the record's length.
        let text = std::fs::read_to_string(journal.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
    }

    #[tokio::test]
    async fn reopening_resumes_seq_numbering() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = open(dir.path(), "s").await;
            journal
                .append(agent(), EventBody::Note { text: "a".into() })
                .await
                .unwrap();
            journal
                .append(agent(), EventBody::Note { text: "b".into() })
                .await
                .unwrap();
        }
        let journal = open(dir.path(), "s").await;
        assert_eq!(journal.len(), 2);
        let event = journal
            .append(agent(), EventBody::Note { text: "c".into() })
            .await
            .unwrap();
        assert_eq!(event.seq, 2);
        assert_eq!(journal.events().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn read_from_returns_only_later_events() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path(), "s").await;
        for i in 0..5 {
            journal
                .append(
                    agent(),
                    EventBody::Note {
                        text: format!("{i}"),
                    },
                )
                .await
                .unwrap();
        }

        let tail = journal.read_from(2).await.unwrap();
        assert_eq!(
            tail.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(journal.read_from(5).await.unwrap().is_empty());
        assert!(journal.read_from(99).await.unwrap().is_empty());
        assert_eq!(journal.read_from(0).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn a_partial_trailing_record_is_repaired_so_appends_stay_clean() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = open(dir.path(), "s").await;
            journal
                .append(agent(), EventBody::Note { text: "a".into() })
                .await
                .unwrap();
        }
        let path = dir.path().join("s.jsonl");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"seq\":1,\"ts_ms\":1,\"sess").unwrap();
        drop(file);

        let journal = open(dir.path(), "s").await;
        assert_eq!(journal.len(), 1);
        let event = journal
            .append(agent(), EventBody::Note { text: "b".into() })
            .await
            .unwrap();
        assert_eq!(event.seq, 1);
        assert_eq!(journal.events().await.unwrap().len(), 2);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"sess\""));
    }

    #[tokio::test]
    async fn a_tool_result_cannot_be_stored_twice() {
        let dir = tempfile::tempdir().unwrap();
        let journal = open(dir.path(), "s").await;
        let err = journal
            .append(
                agent(),
                EventBody::ToolResult {
                    tool_use_id: "call_1".into(),
                    name: "read".into(),
                    kind: "file".into(),
                    bytes: 5,
                    reproducible: true,
                    addressable: true,
                    inline: Some("hello".into()),
                    blob: Some(journal.blobs().put(b"hello").unwrap()),
                    reduction: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::PayloadStoredTwice));
        // Rejected before anything was written: a bad event leaves no trace.
        assert!(journal.is_empty());
        assert_eq!(std::fs::read_to_string(journal.path()).unwrap(), "");
    }

    #[tokio::test]
    async fn sessions_in_one_directory_keep_separate_logs_but_share_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let a = open(dir.path(), "a").await;
        let b = open(dir.path(), "b").await;
        assert_ne!(a.path(), b.path());

        let digest = a.blobs().put_str("shared payload").unwrap();
        a.append(
            agent(),
            EventBody::Injected {
                label: "l".into(),
                blob: digest.clone(),
            },
        )
        .await
        .unwrap();
        b.append(
            agent(),
            EventBody::Injected {
                label: "l".into(),
                blob: digest.clone(),
            },
        )
        .await
        .unwrap();

        assert_eq!(b.blobs().get_str(&digest).unwrap(), "shared payload");
        assert_eq!(a.events().await.unwrap()[0].seq, 0);
        assert_eq!(b.events().await.unwrap()[0].seq, 0);
    }

    #[tokio::test]
    async fn a_file_reopened_under_another_session_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        {
            let journal = open(dir.path(), "s").await;
            journal
                .append(agent(), EventBody::Note { text: "a".into() })
                .await
                .unwrap();
        }
        // Somebody renamed the file (or two ids collided): the record says which
        // session it belongs to, and that wins over the file name.
        std::fs::rename(dir.path().join("s.jsonl"), dir.path().join("other.jsonl")).unwrap();
        match Journal::open(dir.path(), SessionId::new("other")).await {
            Err(JournalError::SessionMismatch { found, expected }) => {
                assert_eq!(found, "s");
                assert_eq!(expected, "other");
            }
            other => panic!("expected SessionMismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_empty_session_id_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Journal::open(dir.path(), SessionId::new("")).await,
            Err(JournalError::EmptySessionId)
        ));
    }

    #[test]
    fn handles_are_shareable_across_tasks() {
        /// DESIGN's process model keeps the session in a daemon (and can embed
        /// it in-process), so every handle another crate shares between tasks
        /// has to move safely. This is a compile-time property; the test exists
        /// so that losing it is a test failure rather than a surprise downstream.
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Journal>();
        assert_send_sync::<Event>();
        assert_send_sync::<EventBody>();
        assert_send_sync::<SessionId>();
        assert_send_sync::<JournalError>();
        assert_send_sync::<BlobStore>();
        assert_send_sync::<JournalIndex>();
        assert_send_sync::<crate::effect::EffectLog>();
    }

    #[test]
    fn session_ids_never_escape_the_directory() {

        let name = event_log_file_name(&SessionId::new("../../etc/passwd"));
        assert!(!name.contains('/'), "{name}");
        assert!(std::path::Path::new(&name).components().count() == 1);
        // Two ids that sanitize to the same stem must not collide.
        assert_ne!(
            event_log_file_name(&SessionId::new("a/b")),
            event_log_file_name(&SessionId::new("a_b"))
        );
        // A clean id is used as-is, so ordinary files are readable at a glance.
        assert_eq!(event_log_file_name(&SessionId::new("s-1")), "s-1.jsonl");
    }

    #[tokio::test]
    async fn blobs_referenced_by_an_event_are_readable_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let digest = {
            let journal = open(dir.path(), "s").await;
            let digest = journal.blobs().put_str("large payload").unwrap();
            journal
                .append(
                    agent(),
                    EventBody::ToolResult {
                        tool_use_id: "call_1".into(),
                        name: "read".into(),
                        kind: "file".into(),
                        bytes: 13,
                        reproducible: true,
                        addressable: true,
                        inline: None,
                        blob: Some(digest.clone()),
                        reduction: None,
                    },
                )
                .await
                .unwrap();
            digest
        };
        let journal = open(dir.path(), "s").await;
        assert_eq!(journal.blobs().get_str(&digest).unwrap(), "large payload");
    }
}
