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
