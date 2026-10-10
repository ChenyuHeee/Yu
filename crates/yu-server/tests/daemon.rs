//! The daemon, against real files and a real socket.
//!
//! DESIGN: "会话由 daemon 持有,经 Unix socket 通信". Both halves are exercised
//! here rather than described, because a protocol that has never carried a frame
//! is a schema, not a server.

use std::path::PathBuf;

use yu_server::session::{
    DirectoryHost, Host, Request, Response, ServerConfig, SessionHost, SessionStatus, decode_line,
    encode_line, handle_for_test, serve,
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
