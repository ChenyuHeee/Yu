//! **M6: "进程被 `kill -9` 后能从最后一条完好记录续跑".**
//!
//! Nothing here is simulated. The daemon is `yu daemon`, run as a process; the
//! kill is `kill -9` against its pid; the restart is a second process over the
//! same state directory; the "did it resume?" question is answered by what the
//! second process writes into the record and by the record itself.
//!
//! What this establishes: a process that was killed loses nothing it had
//! committed, a later process continues from the last record that was written
//! whole, and a torn fragment left by a kill during an append is repaired rather
//! than welded onto the next record.
//!
//! What it does not establish, said plainly: the torn fragment in the second half
//! of `a_killed_daemon_resumes_from_the_last_record_it_wrote` is written by the
//! test, not raced. A crash that lands exactly between two writes cannot be
//! produced on demand from outside the process, so the state is constructed — the
//! fragment is byte-for-byte what such a crash leaves, a partial line after the
//! last newline — and everything done to it afterwards is the production path:
//! the daemon's own reopen, its own tail repair, and its own seq numbering.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use common::{sigkill, wait_for_file, wait_for_text};
use serde_json::Value;
use yu_server::session::{Request, Response, decode_line, encode_line};

/// A daemon child, killed when the test is done with it even if the test panics.
struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Quietly: a daemon this test already killed is gone, and `kill` shelling
        // out to say so would be noise in an otherwise silent run.
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Start `yu daemon` and wait until it *answers*, not merely until its socket
/// file exists.
///
/// The distinction is the whole reason this is a loop over connections: a socket
/// file outlives the process that bound it, so after a kill the path is already
/// there while nothing is listening on it. Waiting for the file would hand the
/// test a connect to a dead address, which is the failure mode this test is
/// about.
fn start_daemon(dir: &Path, socket: &Path) -> Daemon {
    let log = dir.join("daemon.log");
    let stderr = std::fs::File::create(&log).expect("a log for the daemon");
    let child = Command::new(common::yu())
        .args([
            "daemon",
            "--dir",
            dir.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("the daemon must be spawnable");
    let daemon = Daemon { child };
    // `wait_for_file` first, so that a daemon that cannot bind at all fails with
    // the message about the socket rather than a connect timeout.
    assert!(
        wait_for_file(socket, Duration::from_secs(10)),
        "the daemon never bound its socket at {}: {}",
        socket.display(),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return daemon;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "the daemon never accepted a connection at {}: {}",
        socket.display(),
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// One request over the real socket, and the frame that ends its answer.
///
/// A request may be answered by several frames — `Send` reports what it recorded
/// before it says it is done — so this reads until a frame that is not a chunk.
async fn ask(socket: &Path, request: &Request) -> Response {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .expect("the daemon must be accepting connections");
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write
        .write_all(encode_line(request).unwrap().as_bytes())
        .await
        .unwrap();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the daemon must answer")
            .unwrap()
            .expect("a frame");
        match decode_line::<Response>(&line).expect("a decodable frame") {
            Response::Chunk { .. } => continue,
            terminal => return terminal,
        }
    }
}

/// The seqs in a record, read straight off disk. A test that read them through
/// the daemon would be asking the thing under test whether it did its job.
fn seqs_in(path: &Path) -> Vec<u64> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| event["seq"].as_u64())
        .collect()
}

fn journal_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(session).join(format!("{session}.jsonl"))
}

#[tokio::test]
async fn a_killed_daemon_resumes_from_the_last_record_it_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    // ---- the first daemon: a session, and two messages in its record
    let mut daemon = start_daemon(&state, &socket);
    let pid = daemon.child.id();
    let Response::Started { session } = ask(
        &socket,
        &Request::Start {
            charter: "carry the change through".to_owned(),
        },
    )
    .await
    else {
        panic!("Start must answer with a session id");
    };
    for text in ["the parser drops the closing brace", "now fix the parser"] {
        let responses = ask(
            &socket,
            &Request::Send {
                session: session.clone(),
                text: text.to_owned(),
            },
        )
        .await;
        assert!(
            matches!(responses, Response::Done { .. }),
            "Send must be answered: {responses:?}"
        );
    }
    let path = journal_path(&state, &session);
    assert_eq!(
        seqs_in(&path),
        vec![0, 1, 2],
        "the charter and two messages are the record"
    );
    let before_kill = std::fs::read(&path).unwrap();

    // ---- kill -9. Not a shutdown, not a signal the process can handle.
    sigkill(&daemon.child);
    let status = daemon.child.wait().expect("the daemon must be reapable");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "the daemon was not killed by SIGKILL, so this test is not about a kill: {status}"
    );
    assert!(
        Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            // The expected answer here is "No such process", and `kill` says so
            // on stderr; the failure is the *success*.
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true),
        "the process is still alive after kill -9"
    );
    // The socket file outlives the process that bound it. Saying so is the point:
    // a daemon that refused to start because the path existed would be a daemon
    // that could never be restarted after a crash.
    assert!(socket.exists(), "the socket file must outlive the process");

    // ---- a second daemon, over the same state directory
    let restarted = start_daemon(&state, &socket);
    assert_ne!(restarted.child.id(), pid, "the daemon is a new process");

    let recovered = ask(
        &socket,
        &Request::Resume {
            session: session.clone(),
        },
    )
    .await;
    let Response::Status { sessions } = recovered else {
        panic!("Resume must answer with the session's state: {recovered:?}");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
    assert_eq!(
        sessions[0].last_seq,
        Some(2),
        "recovery must report the last record the killed process committed"
    );

    // ---- and it continues from there
    ask(
        &socket,
        &Request::Send {
            session: session.clone(),
            text: "and it is done".to_owned(),
        },
    )
    .await;
    assert_eq!(
        seqs_in(&path),
        vec![0, 1, 2, 3],
        "the resumed daemon must continue after the last committed record, not \
         restart the numbering or repeat an address"
    );
    let after = std::fs::read(&path).unwrap();
    assert!(
        after.starts_with(&before_kill),
        "the records that were committed before the kill must still be there, \
         unchanged, at the front of the file"
    );

    // ---- a kill *during* an append leaves a fragment, and the next daemon
    //      repairs it rather than welding onto it
    let mut daemon = restarted;
    sigkill(&daemon.child);
    daemon.child.wait().expect("reap");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"seq\":4,\"ts_ms\":1,\"session\":\"")
            .unwrap();
    }
    drop(daemon);

    let third = start_daemon(&state, &socket);
    let resp = ask(
        &socket,
        &Request::Status {
            session: Some(session.clone()),
        },
    )
    .await;
    let Response::Status { sessions } = resp else {
        panic!("Status must answer: {resp:?}");
    };
    assert_eq!(
        sessions[0].last_seq,
        Some(3),
        "a fragment is not a record, and recovery must say so rather than count it"
    );
    ask(
        &socket,
        &Request::Send {
            session: session.clone(),
            text: "after the torn append".to_owned(),
        },
    )
    .await;
    assert_eq!(
        seqs_in(&path),
        vec![0, 1, 2, 3, 4],
        "the fragment must be dropped, not welded onto the next record"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("\"ts_ms\":1,\"session\":\"\n"),
        "the fragment is still in the file: {text}"
    );
    drop(third);

    // Every one of those records is decodable, which is what a repaired tail
    // means: a file that opens and yields whole events.
    let whole = std::fs::read_to_string(&path).unwrap();
    for line in whole.lines() {
        serde_json::from_str::<Value>(line).expect("every remaining line is a record");
    }
    assert!(
        wait_for_text(&path, "after the torn append", Duration::from_secs(1)),
        "the last message must be in the record"
    );
}
