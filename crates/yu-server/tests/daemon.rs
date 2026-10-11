//! The daemon, against real files and a real socket.
//!
//! DESIGN: "会话由 daemon 持有,经 Unix socket 通信". Both halves are exercised
//! here rather than described, because a protocol that has never carried a frame
//! is a schema, not a server.

use std::path::{Path, PathBuf};

use yu_server::session::{
    DirectoryHost, Host, Request, Response, ServerConfig, ServerError, SessionHost, SessionStatus,
    decode_line, encode_line, handle_for_test, serve,
};

fn config(root: PathBuf) -> ServerConfig {
    ServerConfig {
        host: Host::LocalDaemon {
            socket: root.join("daemon.sock"),
        },
        state_dir: root,
    }
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Bind a daemon in `dir` and start serving it in the background.
///
/// The returned handle is the accept loop: `serve` returns when — and only when
/// — the loop itself ends, so a test can ask whether it is still running.
/// `serve` borrows its host rather than owning it, which is why the host is
/// moved into the task and borrowed from there, the same way a caller that holds
/// a host would.
fn serving_daemon(dir: &Path) -> (PathBuf, tokio::task::JoinHandle<Result<(), ServerError>>) {
    let socket = dir.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let host = std::sync::Arc::new(DirectoryHost::new(dir));
    let cfg = config(dir.to_path_buf());
    let serving = tokio::spawn(async move { serve(listener, host.as_ref(), cfg).await });
    (socket, serving)
}

/// The sessions the daemon has written down, read straight from the state
/// directory instead of through `sessions()` — so a test that uses this to wait
/// for something to happen is not waiting on the code it is testing.
fn session_dirs(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().join("info.json").is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn launching_a_session_writes_it_down_and_recovery_reads_it_back() {
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let host = DirectoryHost::new(dir.path());

    let launched = host.launch(&cfg, "make the tests pass").unwrap();
    assert!(host.dir_for(&launched.id).is_dir(), "a session owns a directory");
    assert!(host.dir_for(&launched.id).join("info.json").is_file());

    let recovered = host.thread_recovery(&cfg, &launched.id).unwrap();
    assert_eq!(recovered.id, launched.id);
    assert_eq!(recovered.status, SessionStatus::Ready);
    assert_eq!(host.sessions().unwrap().len(), 1);
}

/// Recovery must report what the session was doing, not what would be convenient.
///
/// A session recorded as `Busy` was interrupted mid-turn. Resetting it to ready
/// would hide exactly the interruption recovery exists to surface.
#[test]
fn a_busy_session_is_not_quietly_reset_to_ready() {
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let host = DirectoryHost::new(dir.path());
    let launched = host.launch(&cfg, "charter").unwrap();

    host.set_status(&cfg, &launched.id, SessionStatus::Busy)
        .unwrap();

    let recovered = host.thread_recovery(&cfg, &launched.id).unwrap();
    assert_eq!(
        recovered.status,
        SessionStatus::Busy,
        "an interrupted session must come back looking interrupted"
    );
}

#[test]
fn an_unreadable_record_is_a_crash_rather_than_a_session_that_never_existed() {
    let dir = tempdir();
    let host = DirectoryHost::new(dir.path());
    std::fs::create_dir_all(dir.path().join("torn")).unwrap();
    std::fs::write(dir.path().join("torn/info.json"), "{ this is not json").unwrap();

    let sessions = host.sessions().unwrap();
    assert_eq!(sessions.len(), 1, "a damaged session must not be dropped");
    assert!(
        matches!(sessions[0].status, SessionStatus::Crashed { .. }),
        "a record that will not decode is the case recovery is for: {:?}",
        sessions[0].status
    );
}

/// The state directory holds more than sessions, and only sessions may be
/// reported as sessions.
///
/// DESIGN puts L2 where the state is, so the CLI writes its journal
/// (`<session>.jsonl`) and its blob store (`blobs/`) into the same directory the
/// daemon keeps its session directories in. Scanning every entry as a session
/// reported the blob store as one that had crashed — a phantom crash for every
/// CLI run, in the one list a caller reads to find out what actually died.
#[test]
fn the_journals_own_files_are_not_reported_as_crashed_sessions() {
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let host = DirectoryHost::new(dir.path());
    let launched = host.launch(&cfg, "charter").unwrap();

    // What a CLI run leaves behind in the same directory.
    std::fs::write(dir.path().join("run-20260101T000000.jsonl"), "{}\n").unwrap();
    std::fs::create_dir_all(dir.path().join("blobs/ab")).unwrap();
    std::fs::write(dir.path().join("blobs/ab/cdef"), "a payload").unwrap();

    let sessions = host.sessions().unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec![launched.id.as_str()],
        "the journal file and the blob store are not sessions"
    );
    assert_eq!(
        sessions[0].status,
        SessionStatus::Ready,
        "and the session that is one must not be dragged down with them"
    );

    // Not a session is not recoverable as one either: `blobs` is a name in the
    // state directory, not a session that needs reattaching.
    assert!(matches!(
        host.thread_recovery(&cfg, "blobs"),
        Err(ServerError::UnknownSession { .. })
    ));
}

#[test]
fn an_unknown_session_is_named_in_the_error() {
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let host = DirectoryHost::new(dir.path());
    let err = host.thread_recovery(&cfg, "nope").unwrap_err();
    assert!(err.to_string().contains("nope"), "{err}");
}

#[test]
fn stopping_a_session_is_recorded_not_just_remembered() {
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let host = DirectoryHost::new(dir.path());
    let launched = host.launch(&cfg, "charter").unwrap();

    // Through the protocol, as a client would.
    let responses = handle_for_test(
        &host,
        &cfg,
        Request::Stop {
            session: launched.id.clone(),
        },
    );
    assert!(matches!(responses.as_slice(), [Response::Done { .. }]));

    // A fresh read of the directory — i.e. what a restarted daemon would see.
    let recovered = host.thread_recovery(&cfg, &launched.id).unwrap();
    assert_eq!(recovered.status, SessionStatus::Stopped);
}

#[test]
fn a_session_survives_the_daemon_that_started_it() {
    // The point of writing sessions down: a daemon that dies and comes back must
    // find the same sessions, not new ones.
    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let id = {
        let host = DirectoryHost::new(dir.path());
        host.launch(&cfg, "charter").unwrap().id
    };
    let restarted = DirectoryHost::new(dir.path());
    let found = restarted.thread_recovery(&cfg, &id).unwrap();
    assert_eq!(found.id, id);
    assert!(found.created_ms > 0, "the record kept when it was made");
}

#[tokio::test]
async fn the_socket_carries_the_protocol() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempdir();
    let cfg = config(dir.path().to_path_buf());
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let host = std::sync::Arc::new(DirectoryHost::new(dir.path()));

    let serving = {
        let host = host.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { serve(listener, host.as_ref(), cfg).await })
    };

    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    // Start a session over the wire.
    write
        .write_all(
            encode_line(&Request::Start {
                charter: "do the thing".into(),
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    let Response::Started { session } = decode_line(&line).unwrap() else {
        panic!("expected Started, got {line}");
    };

    // Ask about it.
    write
        .write_all(
            encode_line(&Request::Status {
                session: Some(session.clone()),
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    let Response::Status { sessions } = decode_line(&line).unwrap() else {
        panic!("expected Status, got {line}");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);

    // A malformed frame is answered, not fatal: a client that sent one bad line
    // must not lose the session it was asking about.
    write.write_all(b"this is not a request\n").await.unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_line::<Response>(&line).unwrap(),
        Response::Error { .. }
    ));

    // And the connection is still usable afterwards.
    write
        .write_all(encode_line(&Request::Status { session: None }).unwrap().as_bytes())
        .await
        .unwrap();
    let line = lines.next_line().await.unwrap().unwrap();
    assert!(matches!(
        decode_line::<Response>(&line).unwrap(),
        Response::Status { .. }
    ));

    serving.abort();
}

/// A client that stays connected must not hold up the next client.
///
/// `serve` used to handle one connection to the end before accepting the next,
/// so a client that stayed connected without sending anything — which is what a
/// live client between requests looks like — made the daemon unavailable to
/// everyone else for as long as it stayed. DESIGN's daemon is the process
/// several front ends attach to: attaching and *staying* attached cannot be the
/// same thing as owning the server.
#[tokio::test]
async fn a_client_that_stays_connected_does_not_hold_up_the_next_one() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempdir();
    let (socket, serving) = serving_daemon(dir.path());

    // The first client connects, says nothing, and stays.
    let quiet = tokio::net::UnixStream::connect(&socket).await.unwrap();

    // A second client must be answered while the first is still connected.
    let second = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = second.into_split();
    let mut lines = BufReader::new(read).lines();
    write
        .write_all(encode_line(&Request::Status { session: None }).unwrap().as_bytes())
        .await
        .unwrap();

    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("a second client was never served: the daemon serves one connection at a time")
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_line::<Response>(&line).unwrap(),
        Response::Status { .. }
    ));

    drop(quiet);
    serving.abort();
}

/// A client that disappears before its answer can be written takes only its own
/// connection down.
///
/// The failed write is the ordinary one: the client asked for something and was
/// gone before the answer could land, so the write fails with EPIPE. `serve`
/// used to hand that error to `?`, which ended the accept loop and with it the
/// daemon — for every other client too. A socket error belongs to the connection
/// it happened on.
#[tokio::test]
async fn a_client_that_disappears_before_its_answer_is_written_does_not_stop_the_daemon() {
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let dir = tempdir();
    let socket = dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();

    // The client asks and leaves, all of it before the daemon has accepted
    // anything: dropping the stream closes it, and the request is already in the
    // socket. That order is what makes the failure deterministic — the daemon
    // reads a request and writes into a socket nobody holds — instead of a race
    // between the client's close and the daemon's write.
    {
        let mut gone = tokio::net::UnixStream::connect(&socket).await.unwrap();
        gone.write_all(
            encode_line(&Request::Start {
                charter: "short-lived".to_owned(),
            })
            .unwrap()
            .as_bytes(),
        )
        .await
        .unwrap();
    }

    let host = std::sync::Arc::new(DirectoryHost::new(dir.path()));
    let cfg = config(dir.path().to_path_buf());
    let serving = {
        let host = host.clone();
        tokio::spawn(async move { serve(listener, host.as_ref(), cfg).await })
    };

    // The request did reach the host: `Start` writes the session down before it
    // answers. Without this, a request that never arrived would leave the test
    // passing for the wrong reason — there would be no failed write to contain.
    let deadline = Instant::now() + Duration::from_secs(5);
    while session_dirs(dir.path()).is_empty() {
        assert!(
            Instant::now() < deadline,
            "the request from the client that left was never handled"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A live client still gets answers.
    let live = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("the daemon stopped accepting clients when one connection failed");
    let (read, mut write) = live.into_split();
    let mut lines = BufReader::new(read).lines();
    write
        .write_all(encode_line(&Request::Status { session: None }).unwrap().as_bytes())
        .await
        .unwrap();
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("the daemon stopped answering after one client's connection failed")
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_line::<Response>(&line).unwrap(),
        Response::Status { .. }
    ));

    assert!(
        !serving.is_finished(),
        "the accept loop ended with the client that failed"
    );
    serving.abort();
}
