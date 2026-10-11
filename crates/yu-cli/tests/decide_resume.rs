//! **端到端验收条件 3, on the path a real run takes: "中途 `kill -9`,恢复后继续,且没有重复副作用".**
//!
//! The acceptance could not exercise this at all: `yu decide` had no
//! `--session`, so a killed decision run had no way to continue, and `--handover`
//! does not help because a handover is written when a session *ends*, and a
//! killed session wrote none. This test drives the whole thing with the product
//! itself: a real `yu decide` process, a real `SIGKILL` in the middle of a tool
//! call, and a second real `yu decide` continuing the same record.
//!
//! What it establishes, in the order the run does:
//!
//! 1. run one's executor completes one side effect (the effect is recorded
//!    `done`) and then runs a command that kills the harness mid-call — so the
//!    record ends with one completed effect and one `intended`;
//! 2. run two reopens the record and **replays the same calls** (same
//!    `tool_use_id`, which is what a replay of a recorded turn is), and the
//!    completed one is answered out of the record — the artifact is byte-for-byte
//!    what run one left, because the command that wrote it did not run again;
//! 3. the unfinished one is neither skipped nor re-run: run two refuses it and
//!    survives (the old command is `kill -9 $PPID`; a second run of it would be
//!    a second death, which is exactly what the assertion rules out);
//! 4. the state comes back: the continuation's first request carries the
//!    decision run one recorded, its round numbering continues at the round the
//!    session had reached, and no second `session_started` is written.
//!
//! There are no live-model credentials in this sandbox: both runs talk to a
//! loopback endpoint answering a scripted sequence of replies, and the tool
//! calls are executed by the real tools (§`--tools full`).

mod common;

use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::Duration;

use common::{FakeEndpoint, Reply, run_yu, spawn_yu, text_reply, tool_use_reply};
use serde_json::{Value, json};

fn journal_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.jsonl"))
}

/// The effects in a record, as `(state, key)` pairs, in order.
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

/// The events of one kind in a record, in order.
fn events_of(record: &str, kind: &str) -> Vec<Value> {
    record
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["body"]["event"] == kind)
        .collect()
}

/// The text of a request's system blocks, joined.
fn system_text(request: &Value) -> String {
    request["system"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn kill_and_reap(child: &mut Child, what: &str) {
    use std::os::unix::process::ExitStatusExt;
    let status = child.wait().expect("the process must be reapable");
    assert_eq!(
        status.signal(),
        Some(9),
        "{what} was not killed by SIGKILL, so this test is not about a crash: {status}"
    );
}

/// A turn with two ledger writes, so the continuation has state worth carrying.
fn two_decisions() -> Reply {
    Reply::Json(json!({
        "id": "msg-a1",
        "type": "message",
        "role": "assistant",
        "model": common::TEST_MODEL,
        "content": [
            {"type": "thinking", "thinking": "record what happened",
             "signature": "sig-msg-a1"},
            {"type": "tool_use", "id": "call-a1", "name": "decide",
             "input": {"what": "the artifact is written by the first command",
                       "why": "the command returned exit 0",
                       "verdict": "accepted"}}
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {"input_tokens": 120, "output_tokens": 30,
                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
    }))
}

#[test]
fn a_killed_decide_continues_without_performing_its_effects_twice() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let session = "resume-e2e";
    let artifact = cwd.path().join("artifact.txt");

    // The command run one completes: it appends to the artifact — appending
    // rather than writing, so a second run of it would be visible as a second
    // copy of the bytes, which is what "the same effect, done twice" means.
    let write_call = || {
        json!({"command": format!("printf 'work\\n' >> {}", artifact.display())})
    };
    // The command run one dies on: it kills its own parent, the `yu` process, at
    // the instant it runs. A completion record is impossible by construction.
    let kill_call = || json!({"command": "kill -9 $PPID"});

    // ---- run one: one completed effect, one decision, then the crash
    let first = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-d1",
            "call-d1",
            "delegate",
            json!({"task": "write the artifact, then measure it"}),
        )),
        Reply::Json(tool_use_reply("msg-e1", "call-e1", "exec", write_call())),
        Reply::Json(text_reply("msg-e2", "REPORT: wrote it")),
        two_decisions(),
        Reply::Json(tool_use_reply(
            "msg-d2",
            "call-d2",
            "delegate",
            json!({"task": "look at the artifact once more"}),
        )),
        Reply::Json(tool_use_reply("msg-e3", "call-e2", "exec", kill_call())),
    ]);
    let mut child = spawn_yu(
        &first,
        &[
            "decide",
            "--charter",
            "deliver the artifact",
            "--session",
            session,
            // A standing constraint, passed once and never again: run two is
            // given no `--constraint`, so the only way it can still be in force
            // is out of the record.
            "--constraint",
            "the artifact is the deliverable; do not overwrite it",
            "--tools",
            "full",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--max-rounds",
            "4",
        ],
        cwd.path(),
    );

    let record = journal_path(dir.path(), session);
    // The completed effect, before the crash: waiting for it rather than for the
    // death makes the test independent of how fast the scripted endpoint is.
    assert!(
        common::wait_for_text(&record, "\"state\":{\"done\"", Duration::from_secs(20)),
        "the first effect never completed, so this test would be about nothing: {}",
        std::fs::read_to_string(&record).unwrap_or_default()
    );
    kill_and_reap(&mut child, "the first run");

    // 做了: one effect completed, its record naming the result it produced.
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "work\n",
        "the first run's command was supposed to have written the artifact"
    );
    let effects = effects_in(&record);
    assert_eq!(
        effects
            .iter()
            .map(|(state, _)| state.as_str())
            .collect::<Vec<_>>(),
        vec!["intended", "done", "intended"],
        "the record must end with one completed effect and one interrupted one: {effects:?}"
    );
    let killed_key = effects
        .iter()
        .rev()
        .find(|(state, _)| state == "intended")
        .map(|(_, key)| key.clone())
        .expect("the interrupted effect is on the record");
    assert!(
        killed_key.starts_with("exec:") && killed_key.ends_with("@call-e2"),
        "the interrupted effect must name the call that was in flight: {killed_key}"
    );

    // ---- run two: the same record, continued, replaying the same calls
    let second = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-r1",
            "call-d3",
            "delegate",
            json!({"task": "finish the job"}),
        )),
        // The same call run one completed, to the character: same call id, same
        // command, so the same effect key — which is what a replay of a recorded
        // turn is, and what makes this a test of the discipline rather than of a
        // different call.
        Reply::Json(tool_use_reply("msg-r2", "call-e1", "exec", write_call())),
        // And the same call that killed run one.
        Reply::Json(tool_use_reply("msg-r3", "call-e2", "exec", kill_call())),
        Reply::Json(text_reply("msg-r4", "REPORT: nothing was performed again")),
        Reply::Json(text_reply("msg-r5", "decided: the artifact stands")),
    ]);
    let ran = run_yu(
        &second,
        &[
            "decide",
            // No `--charter`: the intent comes from the record. No
            // `--max-rounds`: the session's own ceiling comes from the record.
            "--session",
            session,
            "--tools",
            "full",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
        ],
        cwd.path(),
        dir.path(),
    );

    // The refusal, out loud, naming the effect it refuses: the record does not
    // say whether it happened, and neither skipping nor re-running is safe.
    assert!(
        ran.stderr.contains("unresolved effect") && ran.stderr.contains(&killed_key),
        "the continuation has to surface the unfinished effect and name it: {}",
        ran.stderr
    );
    // The command did not run a second time: it ends by killing its parent, so a
    // second run of it would be a second death of the harness.
    assert!(
        !ran.killed_by(9),
        "the continuation was killed by the same command it was supposed to refuse: {}",
        ran.stderr
    );
    ran.ok();

    // **没有重复副作用.** The artifact is byte-for-byte what run one left: the
    // completed effect was answered out of the record, and the command that
    // wrote it did not run again.
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "work\n",
        "the completed effect was performed a second time"
    );
    let effects = effects_in(&record);
    assert_eq!(
        effects.iter().filter(|(state, _)| state == "done").count(),
        1,
        "exactly one completion may ever be recorded for this effect: {effects:?}"
    );
    assert!(
        effects
            .iter()
            .find(|(state, _)| state == "done")
            .is_some_and(|(_, key)| key.ends_with("@call-e1")),
        "and it is the first call's: {effects:?}"
    );

    // ---- the state that had to come back
    let record_text = std::fs::read_to_string(&record).unwrap();

    // L1, in front of the continuation: the decision run one wrote is in the
    // first request's system blocks, which is where the rendered view lives.
    let request = second
        .requests()
        .into_iter()
        .find(|request| system_text(request).contains("You are the decision layer"))
        .expect("the continuation asked the layer something");
    assert!(
        system_text(&request).contains("the artifact is written by the first command"),
        "the recorded decision has to be rebuilt into L1: {}",
        system_text(&request)
    );
    // And L0: the standing constraint run one was given, which run two was
    // *not* given again — so it can only be in the request out of the record.
    assert!(
        system_text(&request).contains("the artifact is the deliverable; do not overwrite it"),
        "the constraint the session ran under has to come back with it: {}",
        system_text(&request)
    );

    // The round budget: spent 2, continued at 3 — not restarted.
    assert!(
        ran.stderr.contains("rounds already spent: 2")
            && ran.stderr.contains("this continuation runs rounds 3..=4"),
        "the continuation has to continue the session's budget: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains("3 round(s)"),
        "and report the session's round count: {}",
        ran.stderr
    );
    // And the continuation's own work is numbered after the work that already
    // happened, in the record and in the count.
    assert!(
        record_text.contains("delegation 3:"),
        "the delegation numbering has to continue: {record_text}"
    );
    assert!(
        ran.stderr.contains("3 delegation(s) (2 before this continuation)"),
        "the delegation count has to be the session's: {}",
        ran.stderr
    );

    // The session was continued, not started again: one beginning, one seed, one
    // marker per round the session ran.
    assert_eq!(
        events_of(&record_text, "session_started").len(),
        1,
        "a continuation must not start the session a second time"
    );
    assert_eq!(events_of(&record_text, "session_seeded").len(), 1);
    let rounds: Vec<u64> = events_of(&record_text, "round_started")
        .iter()
        .map(|event| event["body"]["round"].as_u64().unwrap())
        .collect();
    assert_eq!(
        rounds,
        vec![1, 2, 3],
        "the rounds the session ran, counted across both runs: {record_text}"
    );

    // The two runs' executors have distinct addresses in the record.
    let children: Vec<String> = events_of(&record_text, "delegated")
        .iter()
        .map(|event| event["body"]["child"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        children,
        vec![
            "executor#1".to_owned(),
            "executor#2".to_owned(),
            "executor#3".to_owned()
        ],
        "three runs in one record must not share an address: {children:?}"
    );
}
