//! **M6: "改一条 `[reduction]` 规则、重启,行为改变 —— 不重新编译".**
//!
//! The rule that is changed lives in a TOML file; the binary that reads it is the
//! one `cargo test` built, and the test does not rebuild anything between the two
//! runs. What changes between them is one value in one line of one file — which
//! is DESIGN's constraint 0 stated as an observation rather than as a promise:
//!
//! > **prompt / 工具描述 / 角色 / 拓扑 / 归约规则 / hook / 预算 —— 一律外部资源.**
//!
//! and the acceptance criterion is literal: **changing one policy rule must not
//! require recompiling Rust.**
//!
//! The behaviour is observed where it is visible: the request the model is sent.
//! A payload that took `pass_through` is in that request verbatim; the same
//! payload under `drop_with_manifest` is not, and a manifest says what is missing
//! and how to get it back. Anything less — asserting the table parsed, or that a
//! `Reducer` built by hand chose a path — would test the library rather than the
//! product, and the product is the thing that had never read this table.

mod common;

use std::path::Path;

use common::{FakeEndpoint, Reply, run_yu, text_reply, tool_use_reply};

/// A file big enough to be "large" by any budget the harness ships with: well
/// over the 8 KiB inline limit, and marked so its presence in a request is
/// unmistakable.
const MARKER: &str = "THE-RAW-PAYLOAD-MARKER";

fn big_file(dir: &Path) -> u64 {
    // One marker per line, so the payload is both findable and 60 kB long.
    let line = format!("{MARKER} — a line of the payload that only a pass-through would send\n");
    let mut text = String::new();
    while text.len() < 60_000 {
        text.push_str(&line);
    }
    std::fs::write(dir.join("big.txt"), &text).unwrap();
    text.len() as u64
}

/// A policy whose only rule is the one under test, so that what changes between
/// the two runs is a single value.
fn policy(path: &Path, reduction_path: &str) {
    std::fs::write(
        path,
        format!(
            "[[reduction]]\npath = \"{reduction_path}\"\n[reduction.when]\nkind = \"file\"\nsize = \"large\"\n"
        ),
    )
    .unwrap();
}

/// One run of the product: `yu run` with `--tools full`, so a tool result really
/// goes through the reducer before it reaches the model.
fn run_with_reduction(dir: &Path, cwd: &Path, policy_path: &Path) -> FakeEndpoint {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-1",
            "call-1",
            "read_file",
            serde_json::json!({"path": "big.txt"}),
        )),
        Reply::Json(text_reply("msg-2", "REPORT: read it")),
    ]);
    let ran = run_yu(
        &endpoint,
        &[
            "run",
            "--task",
            "read the file and report",
            "--dir",
            dir.to_str().unwrap(),
            "--cwd",
            cwd.to_str().unwrap(),
            "--tools",
            "full",
            "--policy",
            policy_path.to_str().unwrap(),
        ],
        cwd,
        dir,
    );
    ran.ok();
    endpoint
}

#[test]
fn editing_one_reduction_rule_changes_what_the_model_is_sent_without_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let bytes = big_file(cwd.path());
    let policy_path = cwd.path().join("policy.toml");

    let binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_yu"));
    let stamp = |_| {
        let meta = std::fs::metadata(&binary).unwrap();
        (
            meta.len(),
            meta.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap(),
        )
    };
    let before = stamp(());

    // Run one: the rule says a large file result passes through untouched.
    policy(&policy_path, "pass_through");
    let passed = run_with_reduction(dir.path(), cwd.path(), &policy_path);
    let requests = passed.requests();
    assert!(
        requests.len() >= 2,
        "the run must have taken a tool and reported back: {} request(s)",
        requests.len()
    );
    let sent = requests
        .iter()
        .map(common::message_texts)
        .map(|texts| texts.join("\n"))
        .collect::<Vec<_>>();
    let last = sent.last().expect("a second request");
    assert!(
        last.contains(MARKER),
        "the rule said pass_through and the payload is not in the request: {}",
        &last[..last.len().min(400)]
    );
    assert_eq!(
        passed.beyond_the_script(),
        0,
        "the run asked for more replies than the script has"
    );

    // Run two: one value in the file changed. Nothing is rebuilt.
    let after = stamp(());
    policy(&policy_path, "drop_with_manifest");
    let dropped = run_with_reduction(dir.path(), cwd.path(), &policy_path);
    let sent: Vec<String> = dropped
        .requests()
        .iter()
        .map(common::message_texts)
        .map(|texts| texts.join("\n"))
        .collect();
    let last = sent.last().expect("a second request");
    assert!(
        !last.contains(MARKER),
        "the rule said drop_with_manifest and the payload is still being sent: {}",
        &last[..last.len().min(400)]
    );
    assert!(
        last.contains("was dropped"),
        "the manifest must say what happened to the payload, not merely omit it: {last}"
    );
    assert!(
        last.contains(&bytes.to_string()),
        "and it must say how much was omitted ({bytes} bytes): {last}"
    );
    assert!(
        last.contains("read_file"),
        "and carry the re-run key that gets it back: {last}"
    );

    // The two runs are the same binary, and the only difference between them is
    // the file. "It changed" is also what a rebuild would look like, so the
    // artifact is checked: same file, same length, same modification time across
    // both runs.
    assert_eq!(
        (after.0, after.1),
        (before.0, before.1),
        "the binary changed between the runs, so this proves nothing about whether \
         a rebuild was needed"
    );
}

/// A row that can never fire is reported rather than left looking configured.
///
/// `unknown_reduction_axes` existed and had no caller; a dead row is
/// indistinguishable from a row whose condition simply never came up, so the run
/// is the only place the mistake can be seen.
#[test]
fn a_reduction_row_that_can_never_fire_is_reported_by_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    big_file(cwd.path());
    let policy_path = cwd.path().join("typo.toml");
    std::fs::write(
        &policy_path,
        "[[reduction]]\npath = \"dispath_reader\"\n[reduction.when]\nknd = \"file\"\n",
    )
    .unwrap();

    let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", "ok"))]);
    let ran = run_yu(
        &endpoint,
        &[
            "run",
            "--task",
            "say something",
            "--dir",
            dir.path().to_str().unwrap(),
            "--cwd",
            cwd.path().to_str().unwrap(),
            "--policy",
            policy_path.to_str().unwrap(),
        ],
        cwd.path(),
        dir.path(),
    );
    ran.ok();
    assert!(
        ran.stderr.contains("knd"),
        "the run must name the axis it cannot understand: {}",
        ran.stderr
    );
    assert!(
        ran.stderr.contains("dispath_reader"),
        "and the path that is not one of DESIGN's five: {}",
        ran.stderr
    );
    // And the run still finishes: a dead row is reported, not fatal.
    assert_eq!(endpoint.beyond_the_script(), 0);
}
