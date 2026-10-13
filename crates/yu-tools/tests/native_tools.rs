//! The native tools, and the path from a real tool call to a wire block.

use serde_json::json;

use yu_tools::native::{Exec, ListDir, ReadFile, Tool, ToolContext, ToolRegistry, WriteFile};
use yu_tools::reduce::{Budget, Pressure, Reducer, Reduction};
use yu_tools::result::{Payload, ResultKind, ToolResult};

use yu_wire::types::ContentBlock;

fn context(dir: &std::path::Path, inline_limit: u64) -> ToolContext {
    ToolContext::new(
        dir,
        std::sync::Arc::new(Reducer::new(Budget::new(inline_limit, 400))),
    )
}

#[test]
fn exec_runs_a_command_and_captures_stdout_stderr_and_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let result = Exec.execute(
        &json!({"command": "printf 'hello'; printf 'oops' >&2; exit 7"}),
        &ctx,
    );
    let text = result.payload.text().expect("exec is inline until reduced");
    assert!(text.contains("exit: 7"), "{text}");
    assert!(text.contains("stdout (5 bytes):"), "{text}");
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("stderr (4 bytes):"), "{text}");
    assert!(text.contains("oops"), "{text}");
    assert_eq!(result.kind, ResultKind::Log);
    assert!(
        result.reproducible,
        "a pure command re-runs to the same output"
    );
}

#[test]
fn an_execs_provenance_is_enough_to_re_run_it() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let input = json!({"command": "cd . && printf 'stable output'"});
    let first = Exec.execute(&input, &ctx);
    let again = Exec.execute(&first.provenance.input, &ctx);
    assert_eq!(
        first.payload.text(),
        again.payload.text(),
        "the provenance is the re-run key and must round-trip"
    );
}

#[test]
fn read_file_actually_reads_a_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested/data.txt"), "line one\nline two\n").unwrap();

    let ctx = context(dir.path(), 8 * 1024);
    let result = ReadFile.execute(&json!({"path": "nested/data.txt"}), &ctx);
    assert_eq!(result.payload.text(), Some("line one\nline two\n"));
    assert_eq!(result.kind, ResultKind::File);
    assert!(result.reproducible, "the file is still there");
    assert!(result.addressable, "the path is the address");
    assert_eq!(result.size, "line one\nline two\n".len() as u64);
}

#[test]
fn write_then_list_then_read_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);

    let written = WriteFile.execute(
        &json!({"path": "out/note.txt", "content": "remember this"}),
        &ctx,
    );
    assert!(!written.reproducible, "a write is a side effect");
    assert!(written.payload.text().unwrap().contains("13 bytes"));

    let listed = ListDir.execute(&json!({"path": "out"}), &ctx);
    assert_eq!(listed.payload.text(), Some("note.txt"));
    assert!(listed.reproducible);

    let read = ReadFile.execute(&json!({"path": "out/note.txt"}), &ctx);
    assert_eq!(read.payload.text(), Some("remember this"));
}

#[test]
fn a_tool_failure_is_a_result_the_model_can_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let result = ReadFile.execute(&json!({"path": "absent.txt"}), &ctx);
    let text = result.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("absent.txt"), "{text}");
    assert!(
        result.is_complete(),
        "the error message is the whole result"
    );
}

#[test]
fn the_registry_exposes_the_four_native_tools() {
    let dir = tempfile::tempdir().unwrap();
    let registry = ToolRegistry::native(dir.path());
    assert_eq!(
        registry.names(),
        vec!["read_file", "write_file", "list_dir", "exec"]
    );
    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 4);
    assert_eq!(definitions[0].name(), "read_file");
    assert!(registry.get("exec").is_some());
    assert!(registry.get("nope").is_none());

    // Every definition is a custom tool with a real input schema — there is no
    // schema-less fallback on the measured endpoint (see docs/WIRE-FACTS.md).
    for definition in &definitions {
        assert!(!definition.is_server_defined());
        match definition {
            yu_wire::types::ToolDefinition::Custom(custom) => {
                assert_eq!(custom.input_schema["type"], json!("object"));
                assert!(custom.description.is_some());
            }
            other => panic!("expected a custom tool, got {other:?}"),
        }
    }
}

#[test]
fn a_large_file_read_reaches_the_wire_as_a_drop_with_its_rerun_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), "x".repeat(4_000)).unwrap();
    // A tiny ceiling, so even a modest read is reduced.
    let ctx = context(dir.path(), 16);
    let raw = ReadFile.execute(&json!({"path": "big.txt"}), &ctx);
    assert!(raw.size > 16);

    let reduction = ctx.reducer.choose(&raw, Pressure::high());
    match &reduction {
        Reduction::DropWithManifest { rerun } => assert_eq!(
            rerun.as_deref(),
            Some("read_file({\"path\":\"big.txt\"})"),
            "a reproducible read is dropped with the call that re-obtains it"
        ),
        other => panic!("expected a drop, got {other:?}"),
    }
    let mut spill = |_: &[u8]| "unused".to_owned();
    let shown = ctx.reducer.apply(raw, &reduction, &mut spill);

    let block = shown.to_block("call_00_4Ykgo3t3AxTqrrfx12mk4590");
    match block {
        ContentBlock::ToolResult(ref inner) => {
            assert_eq!(inner.tool_use_id, "call_00_4Ykgo3t3AxTqrrfx12mk4590");
            assert_eq!(inner.is_error, None);
            let text = inner.content.as_ref().and_then(|c| c.as_text()).unwrap();
            assert!(text.contains("dropped"), "{text}");
            assert!(text.contains("4000"), "{text}");
            assert!(text.contains("read_file({\"path\":\"big.txt\"})"), "{text}");
        }
        other => panic!("expected a tool_result, got {other:?}"),
    }
}

#[test]
fn a_large_command_output_goes_to_a_log_reader() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 16);
    let raw = Exec.execute(&json!({"command": "printf 'a lot of output here'"}), &ctx);
    assert!(raw.size > 16);
    assert_eq!(raw.kind, ResultKind::Log);

    match ctx.reducer.choose(&raw, Pressure::high()) {
        Reduction::DelegateReader { subagent, prompt } => {
            assert_eq!(subagent, "log_reader");
            assert!(prompt.contains("a lot of output here"), "{prompt}");
        }
        other => panic!("command output is a log, and logs are unbounded: {other:?}"),
    }
}

#[test]
fn an_error_block_keeps_the_tool_use_id_it_was_given() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let result = Exec.execute(&json!({"command": "true"}), &ctx);
    let block = result.to_error_block("toolu_01ABC", "the tool was denied by policy");
    match block {
        ContentBlock::ToolResult(inner) => {
            assert_eq!(inner.tool_use_id, "toolu_01ABC");
            assert_eq!(inner.is_error, Some(true));
            assert_eq!(
                inner.content.and_then(|c| c.as_text().map(str::to_owned)),
                Some("the tool was denied by policy".to_owned())
            );
        }
        other => panic!("expected a tool_result, got {other:?}"),
    }
}

#[test]
fn the_context_helper_returns_the_event_and_the_view_together() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let raw: ToolResult = Exec.execute(&json!({"command": "printf 'small'"}), &ctx);
    let mut store = Vec::new();
    let mut spill = |bytes: &[u8]| {
        store.extend_from_slice(bytes);
        format!("store:{}", store.len())
    };
    let (reduction, shown) = ctx.reduce(raw, Pressure::low(), &mut spill);
    assert_eq!(reduction, Reduction::PassThrough);
    assert!(shown.is_complete());
    assert!(store.is_empty(), "pass-through writes nothing");
    assert!(matches!(shown.payload, Payload::Inline(_)));
}

// ---------------------------------------------------------------------------
// `edit` — the convention is the whole of its value, so it is tested as one
// ---------------------------------------------------------------------------

use yu_tools::native::Edit;

fn file(dir: &std::path::Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn read_back(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

#[test]
fn edit_replaces_a_unique_exact_match_in_place() {
    let dir = tempfile::tempdir().unwrap();
    file(dir.path(), "f.txt", "one\ntwo\nthree\n");
    let result = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "two", "new_string": "TWO"}),
        &context(dir.path(), 8 * 1024),
    );
    assert_eq!(read_back(dir.path(), "f.txt"), "one\nTWO\nthree\n");
    assert_eq!(result.kind, ResultKind::Diff);
    assert!(!result.reproducible, "an edit is a side effect");
    assert!(result.addressable, "the file is on disk");
    assert_eq!(result.provenance.tool, "edit");
    assert_eq!(result.provenance.input["file_path"], json!("f.txt"));
}

#[test]
fn edit_can_delete_text_by_replacing_it_with_nothing() {
    let dir = tempfile::tempdir().unwrap();
    file(dir.path(), "f.txt", "keep\ndrop me\nkeep\n");
    let result = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "drop me\n", "new_string": ""}),
        &context(dir.path(), 8 * 1024),
    );
    assert_eq!(read_back(dir.path(), "f.txt"), "keep\nkeep\n");
    assert!(result.payload.text().unwrap().contains("with 0 bytes"), "{:?}", result.payload.text());
}

/// The counterexample for *exact*: a match that differs only in whitespace is
/// not a match, and the refusal leaves the file alone.
#[test]
fn an_edit_that_differs_only_in_whitespace_is_not_a_match() {
    let dir = tempfile::tempdir().unwrap();
    let before = "fn main() {\n    let x = 1;\n}\n";
    file(dir.path(), "f.rs", before);
    let result = Edit.execute(
        &json!({"file_path": "f.rs", "old_string": "\tlet x = 1;", "new_string": "\tlet x = 2;"}),
        &context(dir.path(), 8 * 1024),
    );
    let text = result.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("not found"), "{text}");
    assert_eq!(
        read_back(dir.path(), "f.rs"),
        before,
        "a refused edit must not touch the file"
    );
}

/// The counterexample for *unique*: two matches and no `replace_all` is
/// undecidable, so nothing happens.
#[test]
fn an_edit_that_matches_twice_is_refused_unless_replace_all_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let before = "alpha\nbeta\nalpha\n";
    file(dir.path(), "f.txt", before);

    let refused = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "alpha", "new_string": "gamma"}),
        &context(dir.path(), 8 * 1024),
    );
    let text = refused.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("2 occurrences"), "{text}");
    assert_eq!(read_back(dir.path(), "f.txt"), before, "nothing was changed");

    let replaced = Edit.execute(
        &json!({
            "file_path": "f.txt",
            "old_string": "alpha",
            "new_string": "gamma",
            "replace_all": true
        }),
        &context(dir.path(), 8 * 1024),
    );
    assert_eq!(read_back(dir.path(), "f.txt"), "gamma\nbeta\ngamma\n");
    assert!(
        replaced.payload.text().unwrap().contains("replaced all 2 occurrences"),
        "{:?}",
        replaced.payload.text()
    );
}

#[test]
fn an_edit_that_would_change_nothing_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    file(dir.path(), "f.txt", "same\n");
    let result = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "same", "new_string": "same"}),
        &context(dir.path(), 8 * 1024),
    );
    let text = result.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("must differ"), "{text}");
}

#[test]
fn an_edit_requires_a_non_empty_old_string_and_a_new_string_that_may_be_empty() {
    let dir = tempfile::tempdir().unwrap();
    file(dir.path(), "f.txt", "text\n");
    let empty_old = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "", "new_string": "x"}),
        &context(dir.path(), 8 * 1024),
    );
    assert!(empty_old.payload.text().unwrap().starts_with("error: "));

    let missing_new = Edit.execute(
        &json!({"file_path": "f.txt", "old_string": "text"}),
        &context(dir.path(), 8 * 1024),
    );
    let text = missing_new.payload.text().unwrap();
    assert!(text.contains("`new_string`"), "{text}");
    assert_eq!(read_back(dir.path(), "f.txt"), "text\n");
}

#[test]
fn an_edit_of_a_file_that_is_not_there_says_what_to_use_instead() {
    let dir = tempfile::tempdir().unwrap();
    let result = Edit.execute(
        &json!({"file_path": "absent.txt", "old_string": "a", "new_string": "b"}),
        &context(dir.path(), 8 * 1024),
    );
    let text = result.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("write_file"), "{text}");
}

/// The effect an edit declares, and the world's answer to it.
///
/// Both halves matter: the key names the place, both strings and the flag, so a
/// replay can tell two edits apart; the verdicts are what the crash window is
/// read from.
#[test]
fn an_edit_declares_what_it_will_change_and_where_the_world_stands() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let input = json!({"file_path": "f.txt", "old_string": "old", "new_string": "new"});
    let intent = Edit.effect(&input).expect("an edit is an effect");
    assert!(intent.key.starts_with("edit:f.txt#sha256:"), "{}", intent.key);
    assert!(intent.description.contains("`old`"), "{}", intent.description);
    assert!(intent.description.contains("`new`"), "{}", intent.description);
    assert_ne!(
        intent.key,
        Edit
            .effect(&json!({
                "file_path": "f.txt",
                "old_string": "old",
                "new_string": "new",
                "replace_all": true
            }))
            .expect("also an effect")
            .key,
        "flipping replace_all changes what the effect is"
    );
    assert!(
        !intent.idempotent,
        "`new_string` may contain `old_string`, so performing the edit twice is not \
         performing it once — the key must not certify what it cannot see"
    );

    // The world, in the three states it can be in.
    file(dir.path(), "f.txt", "old text\n");
    assert_eq!(Edit.effect_in_place(&input, &ctx), yu_tools::EffectVerdict::NotInPlace);
    Edit.execute(&input, &ctx);
    assert_eq!(Edit.effect_in_place(&input, &ctx), yu_tools::EffectVerdict::InPlace);
    file(dir.path(), "f.txt", "neither of them\n");
    assert_eq!(
        Edit.effect_in_place(&input, &ctx),
        yu_tools::EffectVerdict::Unknown,
        "neither the text it was to replace nor the text it was to write is in the file"
    );
}

// ---------------------------------------------------------------------------
// `exec`'s two new parameters, and `read_file`'s two
// ---------------------------------------------------------------------------

#[test]
fn a_command_that_overruns_its_timeout_is_killed_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let started = std::time::Instant::now();
    let result = Exec.execute(
        &json!({"command": "sleep 5; printf 'never printed'", "timeout": 200}),
        &ctx,
    );
    let elapsed = started.elapsed();
    let text = result.payload.text().unwrap();
    assert!(elapsed < std::time::Duration::from_secs(4), "it waited {elapsed:?}");
    assert!(text.contains("was still running after 200 ms and was killed"), "{text}");
    assert!(
        text.contains("stdout (0 bytes):"),
        "the command was killed before it printed anything, so the capture says so rather than \
         carrying the echo of what it was going to print: {text}"
    );
    // The exit marker stays where the format puts it, so the reader of the
    // result is not fooled about a status the command never produced.
    assert_eq!(yu_tools::native::parse_exit_code("sleep 5; printf 'never printed'", text), None);
}

#[test]
fn a_timeout_that_is_not_a_positive_number_is_refused_rather_than_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    for bad in [json!(0), json!(-5), json!("soon"), json!(null)] {
        let result = Exec.execute(&json!({"command": "true", "timeout": bad}), &ctx);
        let text = result.payload.text().unwrap();
        if bad == json!(null) {
            assert!(text.contains("exit: 0"), "null is absent, not invalid: {text}");
        } else {
            assert!(text.starts_with("error: "), "`{bad}` must be refused: {text}");
        }
    }
}

#[test]
fn a_description_the_model_sent_is_carried_into_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path(), 8 * 1024);
    let intent = Exec
        .effect(&json!({"command": "ls -la", "description": "List files in current directory"}))
        .expect("a command is an effect");
    assert!(intent.description.contains("ls -la"), "{}", intent.description);
    assert!(
        intent.description.contains("List files in current directory"),
        "the model's own account of what it ran is the one thing the record cannot \
         reconstruct afterwards: {}",
        intent.description
    );
    // And a call without one still runs: the captured schema requires `command`
    // only, so requiring more here would refuse calls its own face allows.
    let result = Exec.execute(&json!({"command": "printf ok"}), &ctx);
    assert!(result.payload.text().unwrap().contains("ok"));
}

#[test]
fn a_window_into_a_file_is_the_lines_it_names() {
    let dir = tempfile::tempdir().unwrap();
    file(dir.path(), "f.txt", "l1\nl2\nl3\nl4\n");
    let ctx = context(dir.path(), 8 * 1024);

    let whole = ReadFile.execute(&json!({"path": "f.txt"}), &ctx);
    assert_eq!(
        whole.payload.text(),
        Some("l1\nl2\nl3\nl4\n"),
        "without a window the file comes back exactly as it is on disk"
    );

    let window = ReadFile.execute(&json!({"path": "f.txt", "offset": 2, "limit": 2}), &ctx);
    assert_eq!(window.payload.text(), Some("l2\nl3"));
    assert!(window.reproducible, "the file is still there, and the window is a function of it");

    let past_the_end = ReadFile.execute(&json!({"path": "f.txt", "offset": 40}), &ctx);
    assert_eq!(
        past_the_end.payload.text(),
        Some(""),
        "there are no such lines; nothing failed, so this is not an error"
    );

    let not_a_line_count = ReadFile.execute(&json!({"path": "f.txt", "limit": 1.5}), &ctx);
    let text = not_a_line_count.payload.text().unwrap();
    assert!(text.starts_with("error: "), "{text}");
    let zero = ReadFile.execute(&json!({"path": "f.txt", "limit": 0}), &ctx);
    assert!(zero.payload.text().unwrap().starts_with("error: "));
}
