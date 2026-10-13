//! `--tool-profile`, end to end through the product.
//!
//! The unit tests in `yu-tools` prove that a loaded profile moves the shapes a
//! registry renders. This proves the thing that actually matters: the bytes that
//! leave the process change, the model's call is answered under the profile's
//! parameter names, and the un-profiled run beside it is untouched.
//!
//! It runs the real binary against a scripted endpoint, so the request asserted
//! on here is the request that would have gone to the model.

mod common;

use std::path::PathBuf;

use serde_json::{Value, json};

use common::{FakeEndpoint, Reply, run_yu, text_reply, tool_use_reply};

/// The committed capture, which is the whole point of a profile: measured, not
/// remembered.
fn capture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../captures/claude-code/profile/claude-code-2.1.248.json")
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn tool_entry<'a>(request: &'a Value, name: &str) -> &'a Value {
    request["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == name))
        .unwrap_or_else(|| panic!("no `{name}` in the request's tools: {}", request["tools"]))
}

/// A run that reads a file through the profile's face gets that file, answered
/// in the face's parameter names and in the face's result shape.
#[test]
fn a_profiled_run_sends_the_profiles_tools_and_answers_under_their_names() {
    let endpoint = FakeEndpoint::start(vec![
        Reply::Json(tool_use_reply(
            "msg-1",
            "call_00_read",
            "Read",
            json!({"file_path": "hello.txt"}),
        )),
        Reply::Json(text_reply("msg-2", "read it")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("hello.txt"), "The blue crumb is under the third stone.\n").unwrap();

    let ran = run_yu(
        &endpoint,
        &[
            "run",
            "--task",
            "read hello.txt",
            "--cwd",
            work.path().to_str().unwrap(),
            "--tools",
            "full",
            "--tool-profile",
            capture().to_str().unwrap(),
            "--max-steps",
            "2",
        ],
        work.path(),
        dir.path(),
    );
    ran.ok();

    let requests = endpoint.requests();
    assert_eq!(requests.len(), 2, "one turn to call, one to answer");

    // 1. The tool array is the profile's, in the capture's order, and nothing
    //    this harness invented is in it.
    assert_eq!(
        tool_names(&requests[0]),
        vec!["Bash", "Edit", "Read", "Write"],
        "the surface is the profile's subset, in the snapshot's order"
    );

    // 2. It is the captured description, minus the sentences this harness
    //    refuses — checked here in the product, not only in the crate.
    let read = tool_entry(&requests[0], "Read");
    let said = read["description"].as_str().expect("a description");
    assert!(said.contains("Results are returned using cat -n format"), "{said}");
    assert!(
        !said.contains("must be an absolute path"),
        "the harness resolves relative paths, so this sentence is false here: {said}"
    );
    assert_eq!(
        read["input_schema"]["properties"]["file_path"]["type"], json!("string"),
        "the schema is the capture's, under the capture's parameter names"
    );
    assert!(
        read["input_schema"]["properties"].get("pages").is_none(),
        "a parameter this harness cannot honour must not be advertised"
    );

    // 3. The model's call, in the profile's vocabulary, was answered — with the
    //    numbered lines the face implies.
    let second = format!("{:?}", common::message_texts(&requests[1]));
    assert!(
        second.contains("1\\tThe blue crumb is under the third stone.\\n2\\t"),
        "the tool_result is the one the captured face produces: {second}"
    );

    // And the run says which surface it built — in the names the model was
    // given, and with the count — so a four-of-26 surface cannot be mistaken for
    // the whole snapshot, and cannot be reported under names no executor has.
    assert!(
        ran.stderr
            .contains("--- tool surface: Bash, Edit, Read, Write (profile `claude-code-2.1.248`"),
        "{}",
        ran.stderr
    );
    assert!(ran.stderr.contains("4 of its 26 tools are backed here"), "{}", ran.stderr);
}

/// The same task without `--tool-profile` sends the built-in shapes — the
/// un-profiled path this harness has always had.
#[test]
fn without_a_profile_the_same_run_sends_the_built_in_tools() {
    let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", "done"))]);
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let ran = run_yu(
        &endpoint,
        &["run", "--task", "say done", "--cwd", work.path().to_str().unwrap()],
        work.path(),
        dir.path(),
    );
    ran.ok();

    let requests = endpoint.requests();
    assert_eq!(
        tool_names(&requests[0]),
        vec!["read_file", "list_dir"],
        "the read-only default, spelled the way it has always been spelled"
    );
    assert!(ran.stderr.contains("--- tool surface: list_dir, read_file"), "{}", ran.stderr);
}

/// A profile that cannot be read stops the run before it sends anything.
///
/// The alternative — starting anyway under the built-in shapes — is the failure
/// the whole flag exists to prevent: a run whose shapes nobody chose, reporting
/// itself as an ordinary run.
#[test]
fn a_profile_that_does_not_load_stops_the_run() {
    let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", "should not arrive"))]);
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let ran = run_yu(
        &endpoint,
        &[
            "run",
            "--task",
            "anything",
            "--cwd",
            work.path().to_str().unwrap(),
            "--tool-profile",
            "/nonexistent/tool-profile.json",
        ],
        work.path(),
        dir.path(),
    );
    assert!(!ran.status.success(), "the run must not start: {}", ran.stdout);
    assert!(ran.stderr.contains("/nonexistent/tool-profile.json"), "{}", ran.stderr);
    assert_eq!(
        endpoint.requests().len(),
        0,
        "nothing may reach the model under shapes nobody chose"
    );
}

/// `--tools` still decides what an executor may do; the profile only decides
/// what its tools are called.
///
/// The default is read-only, so a profiled run with no `--tools` gets the
/// profile's readers — and a harness whose default is "the whole snapshot" would
/// be handing out `Bash` to callers who never asked for it.
#[test]
fn a_read_only_profiled_run_gets_the_profiles_reader() {
    let endpoint = FakeEndpoint::start(vec![Reply::Json(text_reply("msg-1", "done"))]);
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();

    let ran = run_yu(
        &endpoint,
        &[
            "run",
            "--task",
            "say done",
            "--cwd",
            work.path().to_str().unwrap(),
            "--tool-profile",
            capture().to_str().unwrap(),
        ],
        work.path(),
        dir.path(),
    );
    ran.ok();

    assert_eq!(tool_names(&endpoint.requests()[0]), vec!["Read"]);
    assert!(ran.stderr.contains("--- tool surface: Read (profile"), "{}", ran.stderr);
}
