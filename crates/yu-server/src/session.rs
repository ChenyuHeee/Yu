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

use std::path::PathBuf;

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
