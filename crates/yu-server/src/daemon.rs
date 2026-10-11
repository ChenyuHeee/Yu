//! The resident daemon: sessions it holds, and the record each one is.
//!
//! DESIGN's process model:
//!
//! ```text
//! AppServerTarget::Embedded                    // zero overhead, same process
//!                 | LocalDaemon { UnixSocket } // resident daemon
//!                 | Remote
//! ```
//!
//! and its verdict on the alternative — "flower 最大的架构错误是每 step 一个子进程".
//! The thing that makes a resident daemon worth its socket is that it *owns* the
//! session: the record is the daemon's file, the human's message is in it before
//! any turn is attempted, and a daemon that is killed is a daemon whose sessions
//! are recovered from what is on disk rather than from what a client remembers.
//!
//! So this is the half [`crate::session::DirectoryHost`] deliberately is not: a
//! host that keeps the journals. `DirectoryHost` writes `info.json` and nothing
//! else, which is the right shape for a metadata view and would be a lie as a
//! session owner — a `Send` there can only answer that it cannot record anything.
//!
//! ## What recovery is, exactly
//!
//! `thread_recovery` reports the last record that was *whole*. A process killed
//! mid-append leaves a fragment after the last newline, and the fragment is not a
//! record: it never decoded, it is not what the run did, and treating it as one
//! would put a half-written fact into the history a later reader draws from.
//! Attaching (`deliver`, and the first write of a new daemon) opens the journal,
//! which truncates the fragment and resumes seq numbering from the last whole
//! record — the record is the only source of truth about where it ends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use yu_journal::{Event, EventBody, Journal, SessionId, UnresolvedEffect};
use yu_observe::AgentId;

use crate::session::{
    LaunchedSession, SESSION_RECORD, ServerConfig, ServerError, SessionHost, SessionInfo,
    SessionStatus,
};

/// A session the daemon holds: its description, and its record.
pub struct Live {
    info: std::sync::Mutex<SessionInfo>,
    journal: Journal,
}

impl Live {
    pub fn id(&self) -> String {
        self.info.lock().expect("no panic in the daemon").id.clone()
    }

    /// The record itself. Exposed because the daemon is not the only thing that
    /// reads it: a test asks what the session's journal holds, and so does a
    /// front end reporting what a run left.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// The last seq in the record, or `None` when it is empty.
    pub fn last_seq(&self) -> Option<u64> {
        // The highest recorded seq is the address. It is not the count: seqs
        // continue across a lineage, and a branch that forked at 200 has few
        // events and a high address.
        self.info.lock().expect("no panic in the daemon").last_seq
    }

    /// Append one message from the human, verbatim. DESIGN: "人类原话原样存活".
    pub async fn record(&self, text: &str) -> Result<u64, ServerError> {
        let event = self
            .journal
            .append(
                AgentId::new("human"),
                EventBody::UserMessage {
                    text: text.to_owned(),
                },
            )
            .await?;
        self.touch(event.seq);
        Ok(event.seq)
    }

    /// The effects the record left in the crash window — see
    /// [`yu_journal::UnresolvedEffect`]. Reported by whoever attaches, because an
    /// effect whose completion was never written is exactly the thing a resumed
    /// session must not walk past in silence.
    pub async fn unresolved(&self) -> Result<Vec<UnresolvedEffect>, ServerError> {
        Ok(self.journal.unresolved_effects().await?)
    }

    /// Everything the record holds, for a caller that has to read it rather than
    /// be told about it.
    pub async fn history(&self) -> Result<Vec<Event>, ServerError> {
        Ok(self.journal.history().await?)
    }

    fn touch(&self, seq: u64) {
        let mut info = self.info.lock().expect("no panic in the daemon");
        info.last_active_ms = yu_observe::now_ms();
        info.last_seq = Some(seq);
    }
}

/// A daemon that owns its sessions.
pub struct Daemon {
    root: PathBuf,
    next: std::sync::atomic::AtomicU64,
    /// Sessions that are attached, by id. Attached means "the daemon holds the
    /// journal", and it is what makes two clients talking about one session talk
    /// about the same record.
    live: Mutex<BTreeMap<String, Arc<Live>>>,
}

impl Daemon {
    /// Open a daemon over a state directory, finding whatever sessions are
    /// already there.
    ///
    /// Nothing is attached here: a session directory is read for its description,
    /// and its journal is opened when something actually talks to it. A daemon
    /// that opened every journal at start-up would repair every torn tail before
    /// anyone asked, which is work for sessions nobody resumed.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ServerError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            next: std::sync::atomic::AtomicU64::new(0),
            live: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_for(&self, session: &str) -> PathBuf {
        self.root.join(session)
    }

    /// The description of one session, read from disk.
    fn read_sessions(&self) -> Result<Vec<SessionInfo>, ServerError> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ServerError::Io(e)),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            match std::fs::read_to_string(path.join(SESSION_RECORD)) {
                Ok(text) => {
                    let Ok(mut info) = serde_json::from_str::<SessionInfo>(&text) else {
                        // A record that is present and broken is the damaged case
                        // recovery is for, not a session to drop.
                        out.push(crashed(id, &path));
                        continue;
                    };
                    // The record on disk is the authority on where the session
                    // got to, not the description written next to it: a daemon
                    // killed between an append and a status write leaves the two
                    // disagreeing, and the append is the one that happened.
                    info.last_seq = last_whole_record(&path.join(format!("{id}.jsonl")))?
                        .map(|seq| seq)
                        .or(info.last_seq);
                    out.push(info);
                }
                // No description: not a session, whatever else lives here.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => out.push(crashed(id, &path)),
            }
        }
        out.sort_by_key(|s| s.created_ms);
        Ok(out)
    }

    /// Attach a session: open its journal, repairing a torn tail, and hold it.
    pub async fn attach(&self, session: &str) -> Result<Arc<Live>, ServerError> {
        let mut live = self.live.lock().await;
        if let Some(existing) = live.get(session) {
            return Ok(existing.clone());
        }
        let info = self
            .read_sessions()?
            .into_iter()
            .find(|s| s.id == session)
            .ok_or_else(|| ServerError::UnknownSession {
                id: session.to_owned(),
            })?;
        // In the session's own directory, beside its description, so that a
        // session is one directory: its record, the blobs the record references,
        // and the file that says what it is. `journal_dir` on the description
        // means exactly this path.
        let journal = Journal::open(self.dir_for(session), SessionId::new(session)).await?;
        let attached = Arc::new(Live {
            info: std::sync::Mutex::new(info),
            journal,
        });
        live.insert(session.to_owned(), attached.clone());
        Ok(attached)
    }

    fn write_info(&self, info: &SessionInfo) -> Result<(), ServerError> {
        let body = serde_json::to_string_pretty(info)?;
        std::fs::write(self.dir_for(&info.id).join(SESSION_RECORD), body)?;
        Ok(())
    }

    /// Start a session: a directory, a description, and a record that says what
    /// it was started for.
    pub async fn start(&self, charter: &str) -> Result<Arc<Live>, ServerError> {
        let n = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("s{}-{n}", yu_observe::now_ms());
        let dir = self.dir_for(&id);
        std::fs::create_dir_all(&dir)?;

        let info = SessionInfo {
            id: id.clone(),
            status: SessionStatus::Ready,
            created_ms: yu_observe::now_ms(),
            last_active_ms: yu_observe::now_ms(),
            journal_dir: dir.clone(),
            last_seq: None,
        };
        self.write_info(&info)?;

        // The charter goes into the record as the session's first event, which is
        // what makes a recovered session self-describing: DESIGN's L0 is
        // *injected*, never remembered, so the thing that injects it needs a copy
        // that outlives the client that typed it.
        let journal = Journal::open(&dir, SessionId::new(&id)).await?;
        journal
            .append(
                AgentId::new("daemon"),
                EventBody::SessionStarted {
                    charter: charter.to_owned(),
                    model: std::env::var("ANTHROPIC_MODEL")
                        .unwrap_or_else(|_| "claude-opus-5".to_owned()),
                    // The daemon constructs no client, so it cannot name a
                    // measured profile. It records the profile that *will* be in
                    // force for a turn, which is the one the process was told
                    // about, and says `unknown` when it was told nothing — a
                    // claim about constraints not in evidence would be worse than
                    // an honest blank.
                    quirks: std::env::var("YU_QUIRKS").unwrap_or_else(|_| "unknown".to_owned()),
                },
            )
            .await?;
        let attached = Arc::new(Live {
            info: std::sync::Mutex::new(info),
            journal,
        });
        attached.touch(0);
        self.live
            .lock()
            .await
            .insert(id, attached.clone());
        Ok(attached)
    }
}

/// A session whose description is unreadable, reported as the interruption
/// recovery exists for. The timestamps are zero because the record that would
/// have carried them is what is missing.
fn crashed(id: String, dir: &Path) -> SessionInfo {
    SessionInfo {
        id,
        status: SessionStatus::Crashed {
            at_ms: yu_observe::now_ms(),
        },
        created_ms: 0,
        last_active_ms: 0,
        journal_dir: dir.to_path_buf(),
        last_seq: None,
    }
}

/// The seq of the last record in `path` that was written whole, if any.
///
/// Read line by line and stopped at the first line that does not decode: a
/// fragment is not a record, and a record after a fragment cannot be trusted to
/// be one either. Returning the last *whole* seq is what "the last good record"
/// means for a caller asking where a killed session got to.
fn last_whole_record(path: &Path) -> Result<Option<u64>, ServerError> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut last = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Event>(line) else {
            break;
        };
        last = Some(event.seq);
    }
    Ok(last)
}

impl SessionHost for Daemon {
    async fn launch(&self, _config: &ServerConfig, charter: &str) -> Result<LaunchedSession, ServerError> {
        let live = self.start(charter).await?;
        Ok(LaunchedSession {
            id: live.id(),
            agent: AgentId::new("session"),
        })
    }

    async fn thread_recovery(
        &self,
        _config: &ServerConfig,
        session: &str,
    ) -> Result<SessionInfo, ServerError> {
        let mut info = self
            .read_sessions()?
            .into_iter()
            .find(|s| s.id == session)
            .ok_or_else(|| ServerError::UnknownSession {
                id: session.to_owned(),
            })?;
        // The recorded status is the truth about what it was doing; a session
        // recorded as `Busy` was interrupted mid-turn, and reporting the record
        // unchanged is the point — recovery that "helpfully" reset a busy session
        // to ready would hide the interruption it exists to surface.
        info.last_seq = last_whole_record(&self.dir_for(session).join(format!("{session}.jsonl")))?;
        Ok(info)
    }

    async fn sessions(&self) -> Result<Vec<SessionInfo>, ServerError> {
        self.read_sessions()
    }

    async fn set_status(
        &self,
        _config: &ServerConfig,
        session: &str,
        status: SessionStatus,
    ) -> Result<SessionInfo, ServerError> {
        let mut info = self
            .read_sessions()?
            .into_iter()
            .find(|s| s.id == session)
            .ok_or_else(|| ServerError::UnknownSession {
                id: session.to_owned(),
            })?;
        info.status = status;
        info.last_active_ms = yu_observe::now_ms();
        self.write_info(&info)?;
        Ok(info)
    }

    async fn deliver(
        &self,
        _config: &ServerConfig,
        session: &str,
        text: &str,
    ) -> Result<u64, ServerError> {
        let live = self.attach(session).await?;
        let seq = live.record(text).await?;
        // The description is written after the record, deliberately: a daemon
        // killed between the two leaves a record that is one event longer than
        // its description says, and `read_sessions` takes the record's answer.
        // The other order would leave a description claiming an event that never
        // made it to disk.
        let info = {
            let info = live.info.lock().expect("no panic in the daemon");
            info.clone()
        };
        self.write_info(&info)?;
        Ok(seq)
    }
}
