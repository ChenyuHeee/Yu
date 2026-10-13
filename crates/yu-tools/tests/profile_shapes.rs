//! TODO step 3's criterion: the wire shape is data.
//!
//! Three things are asserted here, and they are the three TODO asks for:
//!
//! 1. loaded, the surface *is* the profile's — name, description and schema for
//!    every tool this harness can back, restricted to the entries it can back;
//! 2. every entry the harness cannot back is absent rather than advertised
//!    (DESIGN 约束 4, 「不提供的,就不要放进去」);
//! 3. a parameter or a sentence the snapshot carries and the implementation
//!    cannot honour stops the load, rather than going on the wire as a promise.
//!
//! The third one is why the shape assertions are written out as fragments
//! instead of re-running the loader's own code: a test that computed its
//! expectation the way the implementation computes its output would pass on any
//! output at all. What is quoted below is quoted from the capture.
//!
//! The un-profiled half of the criterion lives in `native.rs`'s
//! `wire_shape_baseline`, which is not to be edited.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use yu_tools::profile::ProfileEntry;
use yu_tools::{ToolProfile, ToolRegistry, ToolResult};

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn capture_dir() -> PathBuf {
    PathBuf::from(MANIFEST_DIR).join("../../captures/claude-code")
}

fn committed() -> Arc<ToolProfile> {
    Arc::new(
        ToolProfile::from_path(capture_dir().join("profile/claude-code-2.1.248.json"))
            .expect("the committed capture loads"),
    )
}

fn registry() -> ToolRegistry {
    registry_in("/tmp")
}

fn registry_in(dir: impl Into<PathBuf>) -> ToolRegistry {
    ToolRegistry::profiled(dir, committed()).expect("every face binds to the snapshot")
}

fn wire() -> Vec<Value> {
    registry()
        .definitions()
        .iter()
        .map(|definition| serde_json::to_value(definition).expect("a definition serializes"))
        .collect()
}

fn face<'a>(wire: &'a [Value], name: &str) -> &'a Value {
    wire.iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("`{name}` is not on the wire: {wire:#?}"))
}

fn description(wire: &[Value], name: &str) -> String {
    face(wire, name)["description"]
        .as_str()
        .unwrap_or_else(|| panic!("`{name}` has no description"))
        .to_owned()
}

// ---------------------------------------------------------------------------
// The surface
// ---------------------------------------------------------------------------

/// The four faces this harness can answer, in the order the capture listed them.
///
/// The order is the snapshot's rather than ours because the tool array renders
/// in it, and DESIGN's cache rule is that the request's byte prefix is stable.
#[test]
fn the_surface_is_the_faces_the_profile_names_and_this_crate_backs() {
    let registry = registry();
    assert_eq!(
        registry.wire_names(),
        vec!["Bash", "Edit", "Read", "Write"],
        "the profile lists Bash before Edit before Read before Write; the subset keeps that order"
    );
    // The implementations' own names are untouched: they are the identity the
    // record carries.
    assert_eq!(
        registry.names(),
        vec!["exec", "edit", "read_file", "write_file"]
    );
}

/// 22 of the capture's 26 tools are not advertised, and their absence is the
/// design rather than an oversight.
#[test]
fn a_face_this_harness_cannot_back_is_absent_rather_than_advertised() {
    let registry = registry();
    assert_eq!(committed().entries().len(), 26, "the capture still has 26 tools");
    assert_eq!(registry.definitions().len(), 4, "four of them are backed here");
    for absent in ["Agent", "Grep", "Glob", "TaskCreate", "NotebookEdit", "WebFetch", "TodoWrite"] {
        assert!(
            registry.get(absent).is_none(),
            "`{absent}` has no implementation here, so it must not be offered — a tool the \
             model can call and that answers with an error is 另一种偏离分布"
        );
    }
}

// ---------------------------------------------------------------------------
// Name, description, schema — tool by tool
// ---------------------------------------------------------------------------

/// Every sentence of every face, classified.
///
/// `kept` fragments are quoted from the capture; `dropped` fragments are the
/// ones this harness refuses to repeat, and they are listed again in
/// `native.rs`'s refusal tables with the reason for each. A fragment that moves
/// from one list to the other without a comment saying why is the failure this
/// test exists to catch.
const CLASSIFIED: &[(&str, &[&str], &[&str])] = &[
    (
        "Read",
        &[
            "Reads a file from the local filesystem.",
            "You can optionally specify a line offset and limit (especially handy for long files), \
             but it's recommended to read the whole file by not providing these parameters",
            "Results are returned using cat -n format, with line numbers starting at 1",
        ],
        &[
            "must be an absolute path",
            "Reads up to 2000 lines by default",
            "Reads images (PNG, JPG, …)",
            "system reminder",
            "the harness tracks file state for you",
            "pages",
        ],
    ),
    (
        "Write",
        &[
            "Writes a file to the local filesystem, overwriting if one exists.",
            "When to use: creating a new file, or fully replacing one you've already Read.",
            "For partial changes, use Edit instead.",
        ],
        &["will fail"],
    ),
    (
        "Edit",
        &[
            "Performs exact string replacement in a file.",
            "`old_string` must match the file exactly, including indentation, and be unique — the \
             edit fails otherwise.",
            "Strip the Read line prefix (line number + tab) before matching.",
            "`replace_all: true` replaces every occurrence instead.",
        ],
        &[
            "You must Read the file in this conversation before editing",
            "or the call will fail",
        ],
    ),
    (
        "Bash",
        &[
            "- IMPORTANT: Avoid using this tool to run `cat`, `head`, `tail`, `sed`, `awk`, or \
             `echo` commands,",
            "- `timeout` is in milliseconds: default 120000, max 600000.",
            "# Git",
            "- Use the `gh` CLI for GitHub operations (PRs, issues, API).",
            "- Commit or push only when the user asks. If on the default branch, branch first.",
        ],
        &[
            "Executes a bash command",
            "Working directory persists between calls",
            "permission prompt",
            "Command output is displayed to you",
            "run_in_background",
            "Interactive flags",
            "Co-Authored-By: Claude",
            "🤖 Generated with [Claude Code]",
        ],
    ),
];

#[test]
fn every_face_says_what_was_kept_and_nothing_that_was_refused() {
    let wire = wire();
    for (tool, kept, dropped) in CLASSIFIED {
        let said = description(&wire, tool);
        for sentence in *kept {
            assert!(
                said.contains(sentence),
                "`{tool}`'s description lost a sentence this harness backs:\n  missing: {sentence}\
                 \n  description:\n{said}"
            );
        }
        for sentence in *dropped {
            assert!(
                !said.contains(sentence),
                "`{tool}`'s description still says `{sentence}`, which this harness does not do — \
                 a description that lies is worse than a shape nobody recognises:\n{said}"
            );
        }
        // And no refusal left a seam behind.
        assert!(!said.contains("\n\n\n"), "`{tool}` has a blank seam: {said:?}");
        assert_eq!(said, said.trim(), "`{tool}`'s description has stray whitespace");
    }
}

#[test]
fn the_name_on_the_wire_is_the_profiles_own_spelling() {
    let profile = committed();
    let wire = wire();
    for entry in profile.entries() {
        if let Some(tool) = wire.iter().find(|tool| tool["name"] == entry.name) {
            assert_eq!(
                tool["name"].as_str(),
                Some(entry.name.as_str()),
                "the name is copied, not translated: it is the name the model was trained to call"
            );
        }
    }
    assert_eq!(wire.len(), 4);
}

/// The schema goes on the wire as the capture wrote it, minus the parameters
/// this harness does not read.
///
/// `additionalProperties: false` and `$schema` are asserted separately because
/// they are the two things a typed round trip through Rust drops first — the
/// capture's README says to expect exactly that.
#[test]
fn the_schema_is_the_captures_minus_what_this_harness_cannot_back() {
    let profile = committed();
    let wire = wire();
    let expected: &[(&str, &[&str], &[&str])] = &[
        // tool, kept properties, required
        ("Read", &["file_path", "offset", "limit"], &["file_path"]),
        ("Write", &["file_path", "content"], &["file_path", "content"]),
        (
            "Edit",
            &["file_path", "old_string", "new_string", "replace_all"],
            &["file_path", "old_string", "new_string"],
        ),
        ("Bash", &["command", "description", "timeout"], &["command"]),
    ];
    for (tool, properties, required) in expected {
        let schema = &face(&wire, tool)["input_schema"];
        let captured = &profile.entry(tool).expect("the capture carries it").input_schema;

        assert_eq!(
            schema["$schema"], captured["$schema"],
            "`{tool}`: the captured $schema key is part of the shape"
        );
        assert_eq!(
            schema["additionalProperties"], json!(false),
            "`{tool}`: additionalProperties: false is what makes the schema a contract"
        );
        let mut names: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .map(String::as_str)
            .collect();
        names.sort();
        let mut wanted = properties.to_vec();
        wanted.sort();
        assert_eq!(names, wanted, "`{tool}` advertises the wrong parameters");
        let mut sent: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        sent.sort();
        let mut wanted = required.to_vec();
        wanted.sort();
        assert_eq!(sent, wanted, "`{tool}` requires the wrong parameters");

        for (name, value) in schema["properties"].as_object().expect("properties") {
            assert_eq!(
                Some(value),
                captured["properties"].get(name),
                "`{tool}`.{name} is not the captured parameter, description and bounds included"
            );
        }
    }
    // The parameters that are gone, named one by one: each is a promise this
    // harness cannot keep, so each is a tool that would lie.
    assert!(face(&wire, "Read")["input_schema"]["properties"].get("pages").is_none());
    assert!(
        face(&wire, "Bash")["input_schema"]["properties"]
            .get("run_in_background")
            .is_none()
    );
    assert!(
        face(&wire, "Bash")["input_schema"]["properties"]
            .get("dangerouslyDisableSandbox")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Binding failures are failures
// ---------------------------------------------------------------------------

fn synthetic(name: &str, entries: Vec<ProfileEntry>) -> Arc<ToolProfile> {
    Arc::new(ToolProfile::new(name, entries).expect("a non-empty profile"))
}

/// A snapshot that lost a parameter an implementation reads does not load.
#[test]
fn a_parameter_the_implementation_needs_must_be_in_the_snapshot() {
    let profile = synthetic(
        "hand-written",
        vec![ProfileEntry::new(
            "Read",
            "Reads a file from the local filesystem.",
            json!({
                "type": "object",
                "properties": {"file_path": {"type": "string"}},
                "required": ["file_path"],
                "additionalProperties": false
            }),
        )],
    );
    let problem = ToolRegistry::profiled("/tmp", profile)
        .err()
        .expect("a face whose parameters are missing must not load");
    let said = problem.to_string();
    assert!(said.contains("`offset`"), "{said}");
    assert!(said.contains("read_file"), "{said}");
}

/// A snapshot that reworded a sentence we refuse to copy does not load either.
///
/// The point is the direction: re-wording must not be able to *resurrect* a
/// claim. If the refusal's text is gone, the harness says so rather than
/// assuming the claim went with it.
#[test]
fn a_reworded_refused_sentence_stops_the_load() {
    let profile = synthetic(
        "hand-written",
        vec![ProfileEntry::new(
            "Edit",
            "Performs exact string replacement in a file.\n\n- Snappy new wording for the first \
             bullet.\n- `replace_all: true` replaces every occurrence instead.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean"}
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        )],
    );
    let problem = ToolRegistry::profiled("/tmp", profile)
        .err()
        .expect("a description that cannot be refused must not load");
    let said = problem.to_string();
    assert!(said.contains("does not contain"), "{said}");
    assert!(said.contains("must Read the file in this conversation"), "{said}");
}

// ---------------------------------------------------------------------------
// Dispatch and translation
// ---------------------------------------------------------------------------

#[test]
fn the_wire_name_dispatches_to_the_implementation_behind_the_face() {
    let registry = registry();
    assert_eq!(registry.get("Read").expect("Read is on the surface").name(), "read_file");
    assert_eq!(registry.get("Bash").expect("Bash is on the surface").name(), "exec");
    assert_eq!(registry.get("Edit").expect("Edit is on the surface").name(), "edit");
    // The implementation's own name still resolves: it is the identity the
    // record speaks, and a recorded re-run instruction has to keep working.
    assert_eq!(registry.get("read_file").expect("identity still resolves").name(), "read_file");
    assert!(registry.get("Grep").is_none());
    assert!(registry.get("nope").is_none());
}

#[test]
fn a_wire_parameter_becomes_the_implementations_own_name() {
    let registry = registry();
    assert_eq!(
        registry.input_for("Read", &json!({"file_path": "a.txt", "offset": 2})),
        json!({"path": "a.txt", "offset": 2})
    );
    assert_eq!(
        registry.input_for("Write", &json!({"file_path": "a.txt", "content": "x"})),
        json!({"path": "a.txt", "content": "x"})
    );
    // Identity mappings stay identity, and a key the face does not map is passed
    // through rather than swallowed: the tool is the thing that can say it does
    // not know it.
    assert_eq!(
        registry.input_for("Bash", &json!({"command": "ls", "turbo": true})),
        json!({"command": "ls", "turbo": true})
    );
    // Un-profiled, nothing is translated — there is nothing to translate.
    let plain = ToolRegistry::native("/tmp");
    assert_eq!(
        plain.input_for("read_file", &json!({"path": "a.txt"})),
        json!({"path": "a.txt"})
    );
}

// ---------------------------------------------------------------------------
// What the model sees back
// ---------------------------------------------------------------------------

/// The harness's `Read`, under the face, answers with the bytes the capture
/// recorded Claude Code answering with.
///
/// This is the strongest thing in this file: not "numbered output" but *that*
/// output, for the file the capture's own run read (`drive/hello.txt`), against
/// the string in `0003.body.bin`'s `tool_result`. If the numbering, the tab, the
/// trailing empty piece or the byte it is rendered from moves, this fails.
#[test]
fn read_under_its_face_answers_with_the_captured_tool_result() {
    // Read where the capture's own run read: its working directory is committed,
    // and `context_for` is the run's own way of building a call's context.
    let registry = registry_in(capture_dir().join("drive"));
    let context = registry.context_for("Read");
    assert_eq!(context.face(), Some("Read"), "the call wears its face");
    let tool = registry.get("Read").expect("Read is on the surface");
    let input = registry.input_for("Read", &json!({"file_path": "hello.txt"}));
    let result = tool.execute(&input, &context);
    assert_eq!(
        result.payload.text(),
        Some("1\tThe blue crumb is under the third stone.\n2\t"),
        "the answer must be the one Claude Code gave for this file, in its shape"
    );
    assert_eq!(result.kind, yu_tools::ResultKind::File);
}

#[test]
fn offset_and_limit_window_the_lines_in_the_numbers_the_face_shows() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\n").unwrap();
    let registry = registry_in(dir.path());
    let context = registry.context_for("Read");
    let tool = registry.get("Read").expect("Read is on the surface");

    let window = registry.input_for("Read", &json!({"file_path": "f.txt", "offset": 3, "limit": 2}));
    let result = tool.execute(&window, &context);
    assert_eq!(
        result.payload.text(),
        Some("3\tc\n4\td"),
        "an offset is the line number the caller can read off the output"
    );

    // Zero is allowed by the captured schema, and a value the schema allows
    // cannot be an error: it means the first line.
    let from_zero = registry.input_for("Read", &json!({"file_path": "f.txt", "offset": 0, "limit": 1}));
    assert_eq!(tool.execute(&from_zero, &context).payload.text(), Some("1\ta"));

    // Past the end: no such lines, so no lines. Nothing failed, so no error.
    let past = registry.input_for("Read", &json!({"file_path": "f.txt", "offset": 9}));
    assert_eq!(tool.execute(&past, &context).payload.text(), Some(""));
}

/// The un-profiled reader is untouched: the file's own bytes, byte for byte.
#[test]
fn without_a_face_read_file_still_returns_the_file_itself() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "1\tnot numbered by us\n").unwrap();
    let context = yu_tools::ToolContext::with_default_budget(dir.path());
    let tool = ToolRegistry::native(dir.path());
    let input = tool.input_for("read_file", &json!({"path": "f.txt"}));
    let result: ToolResult = tool.get("read_file").expect("registered").execute(&input, &context);
    assert_eq!(
        result.payload.text(),
        Some("1\tnot numbered by us\n"),
        "a line that looks numbered must not be renumbered by an un-profiled read"
    );
}

/// Every parameter the four faces declare, driven through the face.
///
/// The criterion TODO sets for step 4 is that each declared parameter has a test
/// that drives it, and "declared" means declared *on the wire*: a parameter the
/// model sends under the profile's name. The translation is the thing that could
/// silently not happen — the tool would then report a missing parameter and the
/// call would fail in a way that looks like the model's fault.
#[test]
fn every_declared_parameter_is_driven_through_the_face() {
    let dir = tempfile::tempdir().unwrap();
    let registry = registry_in(dir.path());
    let drive = |wire: &str, input: Value| -> ToolResult {
        let context = registry.context_for(wire);
        let translated = registry.input_for(wire, &input);
        registry
            .get(wire)
            .unwrap_or_else(|| panic!("`{wire}` is on the surface"))
            .execute(&translated, &context)
    };

    // `Write`: file_path, content.
    std::fs::write(dir.path().join("src.txt"), "one\ntwo\n").unwrap();
    let written = drive(
        "Write",
        json!({"file_path": "out.txt", "content": "written through the face"}),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written through the face",
        "{}",
        written.payload.text().unwrap_or_default()
    );

    // `Read`: file_path, offset, limit.
    let read = drive(
        "Read",
        json!({"file_path": "src.txt", "offset": 2, "limit": 1}),
    );
    assert_eq!(read.payload.text(), Some("2\ttwo"));

    // `Edit`: file_path, old_string, new_string, replace_all.
    let edited = drive(
        "Edit",
        json!({
            "file_path": "out.txt",
            "old_string": "through",
            "new_string": "under",
            "replace_all": false
        }),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "written under the face",
        "{}",
        edited.payload.text().unwrap_or_default()
    );

    // `Bash`: command, description, timeout.
    let ran = drive(
        "Bash",
        json!({"command": "printf 'from the shell'", "description": "Print a line", "timeout": 30_000}),
    );
    let text = ran.payload.text().unwrap_or_default();
    assert!(text.contains("from the shell"), "{text}");
    assert!(text.contains("exit: 0"), "{text}");

    // The description reaches the record rather than being swallowed by the
    // translation — the one of the three that changes nothing the model sees.
    let intent = registry
        .get("Bash")
        .and_then(|tool| tool.effect(&registry.input_for("Bash", &json!({"command": "ls", "description": "List files"}))))
        .expect("a command is an effect");
    assert!(intent.description.contains("List files"), "{}", intent.description);
}

/// A read-only profiled surface is the profile's readers and nothing else: the
/// split `--tools` makes survives the profile, and it is made by the profile's
/// own reach rather than by a second table.
#[test]
fn a_read_only_profiled_surface_is_the_profiles_readers() {
    let registry = ToolRegistry::profiled_read_only("/tmp", committed()).expect("the profile binds");
    assert_eq!(registry.wire_names(), vec!["Read"]);
    assert_eq!(registry.names(), vec!["read_file"]);
    assert!(registry.get("Bash").is_none(), "a reader-only run cannot run commands");
    assert!(registry.get("Write").is_none());
    assert!(registry.get("Edit").is_none());
}
