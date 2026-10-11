//! **M6: "一次副作用工具落在崩溃窗口里".**
//!
//! DESIGN's effect discipline is "**先记后做**" — the intent goes into the record
//! before the act — and the window it covers is the one between the two:
//!
//! * **(a)** the intent is recorded and the act never happens. A replay must not
//!   read that record as "done", or the work is silently never done.
//! * **(b)** the act happens and the completion is never recorded. That has to be
//!   detectable, because the record now says nothing about whether it happened,
//!   and both ways of guessing are destructive: skipping loses work that may
//!   never have been done, re-performing duplicates work that may have been.
//!
//! Both halves here are produced by **real runs of the product**, killed with a
//! real `SIGKILL` inside the window, and read back by **another real run** of the
//! same session. Nothing is simulated by calling a helper that no production path
//! calls: the first run is `yu run --tools full` with a scripted model asking for
//! a real tool, the kill is `kill -9` on that process, and the second run is the
//! same binary continuing the same record.
//!
//! How each half is made deterministic, since a crash window is by definition
//! narrow:
//!
//! * **(a)** the tool is `write_file`, and its target is a **FIFO with no
//!   reader**. `std::fs::write` blocks in `open(2)` until a reader appears, so
//!   the act cannot have happened while the process is blocked there — the test
//!   kills it at leisure, having first watched the intent land in the record.
//! * **(b)** the tool is `exec`, and the command writes its artifact and then
//!   `kill -9`s its own parent — the `yu` process. The act has happened, by
//!   construction, at the instant the process dies; the completion record was
//!   never written and could not have been.
//!
//! Neither test asserts how long anything takes; both wait for a fact in the
//! record (or in the filesystem) before acting.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use common::{FakeEndpoint, Reply, message_texts, run_yu, spawn_yu, text_reply, tool_use_reply};
use serde_json::{Value, json};

/// A FIFO, made by asking the system for one rather than by any library call
/// that would have to be trusted to mean the same thing.
fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo must run");
    assert!(status.success(), "mkfifo {}", path.display());
}

fn journal_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// The effects in a record, as `(state, key)` pairs, in order.
///
/// The state is reported as the name of the variant, not as its serialised body:
/// what these tests are about is whether a completion was recorded, and the seq it
/// names is a detail of the run rather than a fact worth asserting on.
fn effects_in(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["body"]["event"] == "effect")
        .map(|event| {
            let state = &event["body"]["state"];
            let state = if state.get("done").is_some() {
                "done".to_owned()
            } else {
                state.as_str().unwrap_or("<unreadable>").to_owned()
            };
            (
                state,
                event["body"]["key"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn kill_and_reap(child: &mut Child, what: &str) {
    common::sigkill(child);
    let status = child.wait().expect("the process must be reapable");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "{what} was not killed by SIGKILL, so this test is not about a crash: {status}"
    );
}

/// **(a) 记了意图没动手 → 重放时不会被当成已完成.**
///
/// Run one records the intent to write, and then blocks inside the write — so the
/// act provably has not happened — and is killed. Run two continues the same
/// session and asks for the same call again (same `tool_use_id`, which is what a
/// replay of a recorded turn is). The record's `Intended` must not be read as
/// "done": the write has to happen, and the test observes it happening.
///
/// The revert that has to fail this: `EffectState::may_skip` returning true for
/// `Intended` — the one place where the wrong default silently turns an
/// interrupted effect into a completed one.
#[test]
fn an_effect_that_was_intended_and_never_performed_is_not_replayed_as_done() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let session = "crash-a";
    // One path, named by both runs: the key is built from where the bytes go and
    // which bytes they are, so a second call to a *different* path would be a
    // different effect and would prove nothing about the first one.
    let target = cwd.path().join("blocking-target");

    const CONTENT: &str = "the bytes that must end up on disk";
    // The call the model makes, identically, in both runs: the same tool_use_id is
    // what makes run two a replay of the recorded turn rather than a fresh call.
    let call = || json!({"path": target.to_str().unwrap(), "content": CONTENT});

    // ---- run one: the intent lands, the act blocks, the process is killed
    let first = FakeEndpoint::start(vec![Reply::Json(tool_use_reply(
        "msg-1",
        "call-w1",
        "write_file",
        call(),
    ))]);
    mkfifo(&target);
    let mut child = spawn_yu(
        &first,
        &[
            "run",
            "--task",
            "write the file",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--tools",
            "full",
            "--session",
            session,
        ],
        cwd.path(),
    );

    let record = journal_path(dir.path(), session);
    assert!(
        common::wait_for_text(&record, "\"state\":\"intended\"", Duration::from_secs(20)),
        "the intent never reached the record, so this test would be about nothing: {}",
        std::fs::read_to_string(&record).unwrap_or_default()
    );
    use std::os::unix::fs::FileTypeExt;
    assert!(
        std::fs::metadata(&target).unwrap().file_type().is_fifo(),
        "the target must still be the FIFO the write is blocked on; if it is a \
         file, the act happened and this test is not about the window"
    );
    kill_and_reap(&mut child, "the first run");

    let effects = effects_in(&record);
    assert_eq!(effects.len(), 1, "one effect was declared: {effects:?}");
    assert_eq!(
        effects[0].0, "intended",
        "the record must end on the intent, with no completion: {effects:?}"
    );
    let key = effects[0].1.clone();
    assert!(
        key.starts_with("write_file:") && key.ends_with("@call-w1"),
        "the key must name the effect and the call: {key}"
    );

    // ---- run two: same session, same call, and now the target can be written
    std::fs::remove_file(&target).unwrap();
    let second = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply("msg-2", "call-w1", "write_file", call())),
        Reply::Json(text_reply("msg-3", "REPORT: written")),
    ]);
    let ran = run_yu(
        &second,
        &[
            "run",
            "--task",
            "write the file",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--tools",
            "full",
            "--session",
            session,
        ],
        cwd.path(),
        dir.path(),
    );
    ran.ok();

    // The finding the resumed run must have made, out loud.
    assert!(
        ran.stderr.contains("unresolved effect"),
        "a resumed run has to name the effect the record left in the window: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains(&key),
        "and name it by its key: {}",
        ran.stderr
    );

    // The observation that makes this a test of the mechanism rather than of the
    // bookkeeping: the write happened. A replay that read `Intended` as done
    // would have answered from the record and touched nothing.
    assert!(
        target.exists(),
        "the effect was re-attempted but the file is not there. If the record's \
         `Intended` was read as complete, this is how it shows: the work is \
         silently skipped and the run reports success."
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), CONTENT);

    // And the record says so: a second attempt, this one finished.
    let effects = effects_in(&record);
    assert_eq!(
        effects
            .iter()
            .map(|(state, _)| state.as_str())
            .collect::<Vec<_>>(),
        vec!["intended", "intended", "done"],
        "the record must show the interrupted attempt, the one that ran, and its \
         completion: {effects:?}"
    );
}

/// **(b) 做了没记完 → 能检出.**
///
/// Run one runs a command that writes its artifact and then kills the harness —
/// `kill -9 $PPID`, aimed at the `yu` process itself. The act has happened at the
/// instant of death; the completion could not have been written.
///
/// Run two continues the session and makes the same call again. What must happen:
/// the record's unfinished effect is *detected*, and it is neither skipped nor
/// re-run — a command cannot certify that running it twice is the same as once,
/// so the honest move is to stop and say so. The artifact the first run produced
/// is the evidence that this is not hypothetical: it is on disk, and the record
/// does not mention it.
///
/// The revert that has to fail this: a `decide` that re-attempts an unfinished
/// effect whatever its key claims. The command appends, so a second run shows up
/// as a second copy of the bytes — which is exactly the duplicate the discipline
/// exists to prevent.
#[test]
fn an_effect_that_happened_without_being_completed_is_detected_and_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let session = "crash-b";
    let artifact = cwd.path().join("artifact.txt");

    // Append the artifact, then kill the process that is waiting for this shell.
    // Append rather than write: a second run of this command would be visible as a
    // second copy of the bytes, which is what "the same effect, done twice" means
    // for a command that touches a file.
    let command = format!(
        "printf 'unrecorded' >> {}; kill -9 $PPID",
        artifact.display()
    );
    let call = || json!({"command": command});

    let first = FakeEndpoint::start(vec![Reply::Json(tool_use_reply(
        "msg-e1",
        "call-e1",
        "exec",
        call(),
    ))]);
    let mut child = spawn_yu(
        &first,
        &[
            "run",
            "--task",
            "run the command",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--tools",
            "full",
            "--session",
            session,
        ],
        cwd.path(),
    );
    let record = journal_path(dir.path(), session);
    // The process kills itself while the tool call is in flight, so `wait` is the
    // thing that observes the crash.
    let status = child.wait().expect("the run must be reapable");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "the command was supposed to kill the harness; it did not: {status}"
    );

    // 做了: the artifact is there. The record, meanwhile, says only that the
    // effect was intended — which is the whole of (b).
    assert!(
        artifact.exists(),
        "the command's artifact is missing, so this test is not about an act that \
         happened"
    );
    assert_eq!(std::fs::read_to_string(&artifact).unwrap(), "unrecorded");
    let effects = effects_in(&record);
    assert_eq!(
        effects.len(),
        1,
        "the record must hold the intent and nothing more: {effects:?}"
    );
    assert_eq!(effects[0].0, "intended");
    let key = effects[0].1.clone();
    assert!(key.starts_with("exec:") && key.ends_with("@call-e1"), "{key}");

    // ---- run two: the same call again, on the same record
    let second = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply("msg-e2", "call-e1", "exec", call())),
        Reply::Json(text_reply("msg-e3", "REPORT: I did not run it")),
    ]);
    let ran = run_yu(
        &second,
        &[
            "run",
            "--task",
            "run the command",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--tools",
            "full",
            "--session",
            session,
        ],
        cwd.path(),
        dir.path(),
    );
    // 检出: the resumed run reports the effect, by key, and says what is unknown.
    assert!(
        ran.stderr.contains("unresolved effect") && ran.stderr.contains(&key),
        "the resumed run must surface the unfinished effect and name it: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains("the record does not say whether it happened"),
        "and say what the record does not establish: {}",
        ran.stderr
    );

    // The command did not run again. The first evidence is the process itself:
    // the command ends by killing its own parent, so a second run of it is also a
    // second death of the harness. The exit status is evidence here rather than a
    // precondition — a run that was killed is the failure being watched for.
    assert!(
        !ran.killed_by(9),
        "the resumed run was killed by the same command it was supposed to \
         refuse to run, so the command ran a second time"
    );
    assert!(
        ran.status.success(),
        "the resumed run failed for some other reason: {}",
        ran.stderr
    );

    // And the artifact is exactly what the first run left. A re-attempt would
    // have appended to it: work that may already have happened, done twice, which
    // is the failure the ambiguity exists to prevent.
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "unrecorded",
        "the command was run a second time and its effect was duplicated"
    );

    // What the model was told, rather than silence about a call it made.
    let refusal = second
        .requests()
        .iter()
        .map(|request| message_texts(request).join("\n"))
        .find(|text| text.contains("refused"))
        .unwrap_or_else(|| {
            let all: Vec<String> = second
                .requests()
                .iter()
                .map(|r| message_texts(r).join("\n"))
                .collect();
            panic!(
                "the call must be answered with a refusal, not an empty success; sent: {all:#?}\nrecord: {}",
                std::fs::read_to_string(&record).unwrap_or_default()
            )
        });
    assert!(
        refusal.contains(&key),
        "the refusal must name the effect it is refusing: {refusal}"
    );

    // The record: a second intent (the attempt was made), a note explaining the
    // refusal, and still no completion — because there is still no fact to record.
    let effects = effects_in(&record);
    assert!(
        effects.iter().all(|(state, _)| state == "intended"),
        "nothing may be recorded as done: the record does not say it happened, \
         and the harness did not perform it: {effects:?}"
    );
    let record_text = std::fs::read_to_string(&record).unwrap();
    assert!(
        record_text.contains("neither re-run nor skipped"),
        "the refusal is an event in the record, not only a message to the model: \
         {record_text}"
    );
}
