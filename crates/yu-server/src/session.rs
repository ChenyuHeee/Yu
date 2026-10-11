//! Session ownership — one protocol, three hosts.
//!
//! DESIGN: "flower 最大的架构错误是每 step 一个子进程." The replacement is Codex's
//! shape, where the host decides what a session costs and the protocol stays the
//! same across all three:
//!
//! ```text
//! Embedded                     // zero overhead, same process
//!   | LocalDaemon { UnixSocket } // resident daemon
//!   | Remote
//! ```
//!
//! A session belongs to the daemon, not to the CLI that started it, and the CLI
//! reaches it over a Unix socket; [`Host::Embedded`] skips the socket when the
//! caller already is the process that would own the session. Recovery is part of
//! the protocol from the start ([`SessionHost::thread_recovery`]) rather than a
//! repair path bolted on later, and headless mode is the same protocol driven
//! through `exec` with JSONL output.
//!
//! Frames are JSONL: one JSON object per line, terminator included, so a socket
//! loop writes [`encode_line`]'s output directly and reads back one line at a
//! time. That framing is what keeps a crash cheap — a torn write damages at most
//! the final line, and [`decode_line`] rejects it instead of acting on half a
//! request.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use yu_observe::AgentId;

/// Where a session actually runs. The protocol is identical across the three;
/// only the transport, and therefore the per-step cost, differ. That is the
/// whole point of the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    /// Same process, no socket, no serialisation of the fast path.
    Embedded,
    /// A resident daemon reached over a Unix socket. The socket path is the
    /// host's address, so one daemon per state directory is the natural unit.
    LocalDaemon { socket: PathBuf },
    /// A daemon on another machine. Same protocol, higher latency, so it is a
    /// separate variant rather than a URI that pretends to be local.
    Remote { endpoint: String },
}

/// Everything a host needs in order to be reached and to keep state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: Host,
    /// Where sessions keep their journals and indexes (L2/L3). This is the field
    /// that makes recovery possible rather than aspirational: a session that dies
    /// is rebuilt from what is on disk here.
    pub state_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    /// Idle and able to accept work.
    Ready,
    /// Working. A `Send` that arrives while busy must be queued or refused, never
    /// silently dropped.
    Busy,
    /// Died. Wall-clock `at_ms` rather than a monotonic duration, so a crash
    /// stays comparable with journal timestamps across daemon restarts.
    Crashed { at_ms: i64 },
    Stopped,
}

/// What `Status` answers with: enough to decide whether to attach, resume, or
/// start a fresh generation — and no more, since every field here is paid for by
/// whoever reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub status: SessionStatus,
    pub created_ms: i64,
    /// Moved by every request that reaches the session; the cheapest liveness
    /// signal the daemon has.
    pub last_active_ms: i64,
    /// The session's directory under [`ServerConfig::state_dir`]. Recovery reads
    /// this, so it is part of the session's public description, not an internal
    /// detail.
    pub journal_dir: PathBuf,
}

/// Handle to a session a host has launched.
///
// TODO(M3): wire to the yu_core session handle once core lands; this placeholder
// exists so the trait boundary does not have to move when that type appears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchedSession {
    pub id: String,
    /// The agent this session runs. `yu_observe::AgentId` rather than a second
    /// id type, so spans attribute to the session's agent without a translation
    /// table — a session is an agent's live generation, not a separate thing.
    pub agent: AgentId,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("no session `{id}` on this host")]
    UnknownSession { id: String },
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("could not encode a protocol frame: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A host that does not keep the state an operation needs. Named rather than
    /// silently answered, because a daemon that pretends to have done something
    /// is worse than one that says it cannot.
    #[error("this host does not support: {0}")]
    Unsupported(&'static str),
}

/// The socket protocol, request half. One JSON object per line.
///
/// `#[serde(tag = "type")]` gives every frame a discriminator, so a host that
/// meets a request it does not know can name it in an error instead of failing
/// to parse the line at all — the difference between a diagnosable version skew
/// and an unreadable stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Start a new session. The charter is passed in rather than looked up,
    /// because L0 is injected, never remembered.
    Start { charter: String },
    /// Reattach to a session the daemon already owns, including one it is
    /// recovering after a crash.
    Resume { session: String },
    Send { session: String, text: String },
    Stop { session: String },
    /// `None` asks about every session; `Some` narrows to one, which is also how
    /// a caller finds out that its session has crashed.
    Status { session: Option<String> },
}

/// The socket protocol, response half.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Started { session: String },
    /// A piece of streamed output. Streaming is on the protocol because DESIGN's
    /// observation surface needs `first_token` as a real event, and a response
    /// that only arrives complete cannot report it.
    Chunk { session: String, text: String },
    /// `stop_reason` is carried verbatim: DESIGN requires every stop reason to be
    /// exhausted, so the daemon must not flatten them into "finished".
    Done { session: String, stop_reason: String },
    Error { message: String },
    Status { sessions: Vec<SessionInfo> },
}

/// The three-host contract.
///
/// DESIGN points at Codex's `app-server-daemon`, whose operations are `launch` /
/// `managed_install` / `thread_recovery` / `update_loop`. Only the first and
/// third are named here; `managed_install` and `update_loop` are daemon
/// lifecycle work that M3 adds once there is a binary to install and update, and
/// naming them now would be a signature with no caller.
///
/// The methods are deliberately synchronous. An `async fn` in a trait is not
/// dyn-compatible without boxing, and `async-trait` is not a dependency of this
/// crate — a skeleton should not add one to express a boundary it has not
/// implemented. M3 can make these `async fn` (stable since Rust 1.75) once there
/// is a runtime that needs driving.
///
// TODO(M3): implement over `tokio::net::UnixListener` for `LocalDaemon`, and
// in-process for `Embedded`. Every implementation must make `Stop` idempotent and
// make a `Send` that races a crash land in the journal rather than vanish.
pub trait SessionHost {
    /// Start a session for `charter` and return a handle to it. DESIGN: cheap
    /// enough that topology can be chosen at run time.
    fn launch(&self, config: &ServerConfig, charter: &str) -> Result<LaunchedSession, ServerError>;

    /// DESIGN's `thread_recovery`: reattach to a session that outlived the
    /// process which was talking to it, and report what state it came back in.
    /// Crash recovery is first-class, so this is a protocol operation and not an
    /// operator procedure.
    fn thread_recovery(&self, config: &ServerConfig, session: &str) -> Result<SessionInfo, ServerError>;

    /// Every session this host holds. The daemon's `Status` with no session.
    fn sessions(&self) -> Result<Vec<SessionInfo>, ServerError> {
        Err(ServerError::Unsupported("this host keeps no session list"))
    }

    /// Look one session up, for the operations that only need to know it exists.
    fn status_lookup(&self, config: &ServerConfig, session: &str) -> Result<SessionInfo, ServerError> {
        self.thread_recovery(config, session)
    }

    /// Record a session's new status. A stop that only lived in memory would
    /// come back to life on the next restart, so it has to be written down.
    fn set_status(
        &self,
        _config: &ServerConfig,
        _session: &str,
        _status: SessionStatus,
    ) -> Result<SessionInfo, ServerError> {
        Err(ServerError::Unsupported("this host does not record session status"))
    }
}

/// Encode one protocol frame as a JSONL line, terminator included.
///
/// The newline is part of the return value because the caller is a socket loop,
/// and a framing helper that leaves framing to its caller is how half-written
/// frames get produced.
pub fn encode_line<T: Serialize>(v: &T) -> Result<String, ServerError> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    Ok(line)
}

/// Decode one JSONL line.
///
/// Trailing whitespace and the terminator are tolerated; everything else is an
/// error. A truncated frame — a write that landed half-way before a crash — and
/// a line carrying an interior newline — a lost frame boundary — must both fail,
/// because the alternative is a host acting on half a request.
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, ServerError> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(ServerError::Protocol("an empty line is not a frame".to_owned()));
    }
    if trimmed.contains('\n') {
        return Err(ServerError::Protocol(
            "a frame contains an interior newline".to_owned(),
        ));
    }
    serde_json::from_str(trimmed).map_err(|e| ServerError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_host() -> Host {
        Host::LocalDaemon {
            socket: PathBuf::from("/run/yu/daemon.sock"),
        }
    }

    fn remote_host() -> Host {
        Host::Remote {
            endpoint: "unix://yu.internal:7777".to_owned(),
        }
    }

    #[test]
    fn the_three_hosts_serialize_distinctly_and_round_trip() {
        let hosts = [Host::Embedded, daemon_host(), remote_host()];

        let encoded: Vec<String> = hosts
            .iter()
            .map(|h| serde_json::to_string(h).expect("a host must encode"))
            .collect();
        assert_ne!(encoded[0], encoded[1]);
        assert_ne!(encoded[1], encoded[2]);
        assert_ne!(encoded[0], encoded[2]);

        // `Embedded` is a unit variant and the other two carry their address, so
        // "which host is this" survives a round trip and a reader can tell that
        // the socket path is the address.
        assert_eq!(encoded[0], "\"embedded\"");
        assert!(encoded[1].contains("/run/yu/daemon.sock"));

        for host in hosts {
            let json = serde_json::to_string(&host).expect("a host must encode");
            let back: Host = serde_json::from_str(&json).expect("a host must decode");
            assert_eq!(host, back);
        }
    }

    #[test]
    fn requests_round_trip_through_jsonl() {
        let requests = [
            Request::Start {
                charter: "Ship the smallest thing that runs.".to_owned(),
            },
            Request::Resume {
                session: "s-1".to_owned(),
            },
            Request::Send {
                session: "s-1".to_owned(),
                text: "check the evidence block".to_owned(),
            },
            Request::Stop {
                session: "s-1".to_owned(),
            },
            Request::Status {
                session: Some("s-1".to_owned()),
            },
            Request::Status { session: None },
        ];

        for request in requests {
            let line = encode_line(&request).expect("a request must encode");
            assert!(line.ends_with('\n'), "a frame is a whole line");
            assert_eq!(line.matches('\n').count(), 1, "one frame, one line");

            let back: Request = decode_line(&line).expect("a request must decode");
            assert_eq!(request, back);
        }
    }

    #[test]
    fn every_request_frame_carries_its_type_discriminator() {
        let line = encode_line(&Request::Stop {
            session: "s-1".to_owned(),
        })
        .expect("a request must encode");
        let value: serde_json::Value = serde_json::from_str(&line).expect("a frame is JSON");
        assert_eq!(value["type"], "stop");
        assert_eq!(value["session"], "s-1");
    }

    #[test]
    fn responses_round_trip_through_jsonl() {
        let info = SessionInfo {
            id: "s-1".to_owned(),
            status: SessionStatus::Crashed { at_ms: 1_726_000_000_000 },
            created_ms: 1_725_000_000_000,
            last_active_ms: 1_725_999_000_000,
            journal_dir: PathBuf::from("/var/yu/s-1"),
        };

        let responses = [
            Response::Started {
                session: "s-1".to_owned(),
            },
            Response::Chunk {
                session: "s-1".to_owned(),
                text: "partial output".to_owned(),
            },
            Response::Done {
                session: "s-1".to_owned(),
                // Verbatim, per DESIGN: a stop reason is not flattened to "done".
                stop_reason: "pause_turn".to_owned(),
            },
            Response::Error {
                message: "no session `s-9`".to_owned(),
            },
            Response::Status {
                sessions: vec![info],
            },
        ];

        for response in responses {
            let line = encode_line(&response).expect("a response must encode");
            let back: Response = decode_line(&line).expect("a response must decode");
            assert_eq!(response, back);
        }
    }

    #[test]
    fn garbage_lines_error_rather_than_panic() {
        let bad = [
            "",
            "   \n",
            "not json",
            "{\"type\":\"start\"",
            "{\"type\":\"no_such_request\"}",
            "{}",
            "{\"type\":\"send\",\"session\":3,\"text\":\"x\"}",
        ];

        for line in bad {
            let err = decode_line::<Request>(line).expect_err("garbage must not decode");
            assert!(
                matches!(err, ServerError::Protocol(_)),
                "{line:?} decoded into something"
            );
        }
    }

    #[test]
    fn a_torn_frame_never_reaches_a_handler() {
        // Half of `{"type":"send","session":"s-1","text":"go"}` — what a crash
        // mid-write leaves behind.
        let torn = "{\"type\":\"send\",\"session\":\"s-1\",\"te";
        assert!(decode_line::<Request>(torn).is_err());

        // Two frames glued together: the boundary was lost, so neither half may
        // be executed.
        let glued = format!(
            "{}{}",
            encode_line(&Request::Stop {
                session: "s-1".to_owned()
            })
            .expect("encode"),
            encode_line(&Request::Status { session: None }).expect("encode"),
        );
        assert!(decode_line::<Request>(&glued).is_err());
    }

    #[test]
    fn a_frame_is_a_whole_line_when_written_to_a_stream() {
        // The property the daemon relies on: concatenating encoded frames and
        // splitting on newlines returns exactly the values that went in.
        let sent = Request::Send {
            session: "s-1".to_owned(),
            text: "line one\nline two".to_owned(),
        };
        let stream = encode_line(&sent).expect("encode");
        assert_eq!(stream.matches('\n').count(), 1, "the payload's newline is escaped");

        let lines: Vec<&str> = stream.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(decode_line::<Request>(lines[0]).expect("decode"), sent);
    }
}

// ---------------------------------------------------------------------------
// A host whose sessions live on disk
// ---------------------------------------------------------------------------

/// The file that makes a directory a session.
///
/// `launch` writes it and the scan reads it, so there is exactly one definition
/// of what a session is. The alternative — a filter on the entry's *name* — is a
/// second definition that no writer is obliged to agree with, and it would rot
/// the moment ids change shape.
const SESSION_RECORD: &str = "info.json";

/// Sessions backed by a directory: one subdirectory each, holding the session's
/// own record.
///
/// DESIGN's `thread_recovery` is the reason this is files rather than a table in
/// memory. A daemon that dies must be able to come back and say what its
/// sessions were doing, and the only thing that survives a process is what was
/// written down — so the state a session is recovered from is the same state it
/// ran under, not a copy made for recovery.
pub struct DirectoryHost {
    root: PathBuf,
    next: std::sync::atomic::AtomicU64,
}

impl DirectoryHost {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            next: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// One directory per session, named by the session id.
    pub fn dir_for(&self, session: &str) -> PathBuf {
        self.root.join(session)
    }

    fn info_path(&self, session: &str) -> PathBuf {
        self.dir_for(session).join(SESSION_RECORD)
    }

    fn write_info(&self, info: &SessionInfo) -> Result<(), ServerError> {
        let path = self.info_path(&info.id);
        let body = serde_json::to_string_pretty(info)?;
        std::fs::write(&path, body)?;
        Ok(())
    }

    fn mark(&self, session: &str, status: SessionStatus) -> Result<SessionInfo, ServerError> {
        let mut info = self
            .sessions()?
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
}

impl DirectoryHost {
    /// One session directory, read back. Split out so both the trait's
    /// `sessions` and `thread_recovery` read the same way.
    ///
    /// The state directory holds more than sessions. DESIGN puts L2 beside the
    /// state it describes, so the CLI lays its journal (`<session>.jsonl`) and
    /// its blob store (`blobs/`) down in this same directory; neither is a
    /// session, and reporting them as crashed ones would grow the session list
    /// by whatever else happens to live there — a daemon that invents a dead
    /// session every time the CLI runs cannot be used to ask what died.
    fn read_sessions(&self) -> Result<Vec<SessionInfo>, ServerError> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            // No directory yet means no sessions, which is not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ServerError::Io(e)),
        };
        for entry in entries.flatten() {
            if let Some(info) = Self::session_at(&entry.path()) {
                out.push(info);
            }
        }
        out.sort_by_key(|s| s.created_ms);
        Ok(out)
    }

    /// One entry of the state directory, read as a session — or `None` when the
    /// entry is something else that lives here (a journal file, the blob store,
    /// anything a human left behind).
    ///
    /// What decides it is the record `launch` writes, not the entry's name: a
    /// directory holding [`SESSION_RECORD`] is a session, whether or not the
    /// record still decodes. A record that is *present and broken* is the
    /// damaged case recovery exists for, so it is reported as [`SessionStatus::
    /// Crashed`] rather than dropped; a record that was never there says nothing
    /// was ever a session, so there is nothing to report.
    fn session_at(path: &Path) -> Option<SessionInfo> {
        if !path.is_dir() {
            return None;
        }
        let id = path.file_name()?.to_string_lossy().into_owned();
        match std::fs::read_to_string(path.join(SESSION_RECORD)) {
            Ok(text) => Some(
                serde_json::from_str::<SessionInfo>(&text)
                    .unwrap_or_else(|_| unreadable_session(id, path)),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(unreadable_session(id, path)),
        }
    }
}

/// A session whose record cannot be read: known by the directory it lives in,
/// and reported as the interruption recovery is for.
///
/// The timestamps are zero because the record that would have carried them is
/// exactly what is missing; inventing `now` for `created_ms` would put the
/// session at the end of a `created_ms`-ordered list, which is a claim about
/// when it was made that nothing supports.
fn unreadable_session(id: String, dir: &Path) -> SessionInfo {
    SessionInfo {
        id,
        status: SessionStatus::Crashed {
            at_ms: yu_observe::now_ms(),
        },
        created_ms: 0,
        last_active_ms: 0,
        journal_dir: dir.to_path_buf(),
    }
}

impl SessionHost for DirectoryHost {
    fn launch(&self, _config: &ServerConfig, _charter: &str) -> Result<LaunchedSession, ServerError> {
        // A counter plus the wall clock: unique within a process, and ordered
        // across processes for anyone reading the directory back.
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("s{}-{n}", yu_observe::now_ms());
        let dir = self.dir_for(&id);
        std::fs::create_dir_all(&dir)?;
        let info = SessionInfo {
            id: id.clone(),
            status: SessionStatus::Ready,
            created_ms: yu_observe::now_ms(),
            last_active_ms: yu_observe::now_ms(),
            journal_dir: dir,
        };
        self.write_info(&info)?;
        Ok(LaunchedSession {
            id,
            agent: AgentId::new("session"),
        })
    }

    fn thread_recovery(&self, _config: &ServerConfig, session: &str) -> Result<SessionInfo, ServerError> {
        // The recorded status is the truth about what it was doing; a session
        // recorded as `Busy` was interrupted mid-turn, which is the one case a
        // caller must not treat as ready. Reporting the record unchanged is the
        // point — recovery that "helpfully" reset a busy session to ready would
        // hide the interruption it exists to surface.
        self.read_sessions()?
            .into_iter()
            .find(|s| s.id == session)
            .ok_or_else(|| ServerError::UnknownSession {
                id: session.to_owned(),
            })
    }

    fn sessions(&self) -> Result<Vec<SessionInfo>, ServerError> {
        self.read_sessions()
    }

    fn set_status(
        &self,
        _config: &ServerConfig,
        session: &str,
        status: SessionStatus,
    ) -> Result<SessionInfo, ServerError> {
        self.mark(session, status)
    }
}

// ---------------------------------------------------------------------------
// The socket loop
// ---------------------------------------------------------------------------

/// Serve the JSONL protocol on a Unix socket.
///
/// DESIGN: "会话由 daemon 持有,经 Unix socket 通信;要零开销就进程内嵌." This is the
/// daemon half; `Embedded` is the same protocol with the socket removed, which is
/// why nothing here holds session state that a caller could not hold itself.
///
/// One connection is one client, and clients do not take turns: every accepted
/// connection is driven *alongside* this loop rather than inside it, so a client
/// that is waiting — mid-frame, or simply holding its connection open between
/// requests, which is what a live client looks like — cannot keep the next one
/// out. DESIGN's daemon is the process several front ends attach to; attaching
/// is only independent if it does not queue behind whoever attached first.
///
/// The accept loop's own failure is still fatal (the listener is the daemon's,
/// not a client's), but nothing a *connection* does reaches it: a client whose
/// socket fails takes its own connection down and nobody else's. See
/// [`serve_frames`], where that boundary is drawn.
///
/// Frames are decoded one line at a time and a malformed line produces an error
/// frame rather than closing the connection — a client that sent one bad frame
/// should not lose the session it was talking about.
///
/// `S: Sync` is what lets several connections hold the host at once. The
/// connections are borrowed from this frame instead of spawned, which is why the
/// loop drives them itself: a task per connection would need an owned host and
/// therefore a different signature. That trade has a floor worth naming, since
/// `SessionHost`'s methods are synchronous: a host call that blocks is felt by
/// every connection this loop is driving, where a task would confine it. Nothing
/// in the host blocks today — revisit here, not at the call sites, when
/// something does.
pub async fn serve<S: SessionHost + Sync>(
    listener: tokio::net::UnixListener,
    host: &S,
    config: ServerConfig,
) -> Result<(), ServerError> {
    let mut clients = FuturesUnordered::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                clients.push(serve_client(stream, host, &config));
            }
            // The guard is load-bearing: an empty `FuturesUnordered` is ready at
            // once with `None`, and a select branch that is always ready spins.
            _ = clients.next(), if !clients.is_empty() => {}
        }
    }
}

/// One client's conversation, with the error boundary around it.
///
/// Everything a connection can fail at — a read that goes wrong, a write to a
/// client that has already gone away (EPIPE: the client asked and left before
/// the answer could land) — stops here. Returning it to the accept loop would
/// end the daemon for every *other* client, which is the one outcome a socket
/// error must never have: a client's problem is not allowed to become the
/// server's. It is logged because by this point there is nobody else to tell —
/// the client that would have heard it is the one that is gone.
async fn serve_client<S: SessionHost + Sync>(
    stream: tokio::net::UnixStream,
    host: &S,
    config: &ServerConfig,
) {
    if let Err(e) = serve_frames(stream, host, config).await {
        tracing::warn!("closing a client connection that failed: {e}");
    }
}

/// The framing loop for one connection.
async fn serve_frames<S: SessionHost>(
    stream: tokio::net::UnixStream,
    host: &S,
    config: &ServerConfig,
) -> Result<(), ServerError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let responses = match decode_line::<Request>(&line) {
            Ok(request) => handle(host, config, request),
            Err(e) => vec![Response::Error {
                message: format!("could not read the request: {e}"),
            }],
        };
        for response in responses {
            // A frame that cannot be written ends the connection: the client
            // is gone, and pretending otherwise leaves a half-written frame
            // on a socket nobody is reading.
            write.write_all(encode_line(&response)?.as_bytes()).await?;
        }
    }
    Ok(())
}

/// `handle`, exposed for tests. The socket loop is the only production caller.
pub fn handle_for_test<S: SessionHost>(
    host: &S,
    config: &ServerConfig,
    request: Request,
) -> Vec<Response> {
    handle(host, config, request)
}

/// Turn one request into the frames it produces.
fn handle<S: SessionHost>(host: &S, config: &ServerConfig, request: Request) -> Vec<Response> {
    match request {
        Request::Status { session } => match session {
            Some(session) => match host.thread_recovery(config, &session) {
                Ok(info) => vec![Response::Status {
                    sessions: vec![info],
                }],
                Err(e) => vec![Response::Error {
                    message: e.to_string(),
                }],
            },
            None => match host.sessions() {
                Ok(sessions) => vec![Response::Status { sessions }],
                Err(e) => vec![Response::Error {
                    message: e.to_string(),
                }],
            },
        },
        Request::Start { charter } => match host.launch(config, &charter) {
            Ok(launched) => vec![Response::Started {
                session: launched.id,
            }],
            Err(e) => vec![Response::Error {
                message: e.to_string(),
            }],
        },
        Request::Resume { session } => match host.thread_recovery(config, &session) {
            Ok(info) => vec![Response::Status {
                sessions: vec![info],
            }],
            Err(e) => vec![Response::Error {
                message: e.to_string(),
            }],
        },
        Request::Stop { session } => {
            // Sessions are owned by the daemon, so stopping one is a state
            // change it must record — a stop that only lived in memory would
            // come back to life on the next restart.
            match host.set_status(config, &session, SessionStatus::Stopped) {
                Ok(_) => vec![Response::Done {
                    session,
                    stop_reason: "stopped".to_owned(),
                }],
                Err(e) => vec![Response::Error {
                    message: e.to_string(),
                }],
            }
        }
        Request::Send { session, text: _ } => vec![Response::Error {
            message: format!(
                "session `{session}` cannot be driven from this host: running a turn needs a \
                 model client, which `yu-server` deliberately does not construct. Wire a \
                 `SessionRunner` in and this becomes the frame that streams its output."
            ),
        }],
    }
}
