//! Native tools.
//!
//! Four tools, each of which fills in the typed result honestly rather than
//! returning a string:
//!
//! | tool | kind | reproducible | addressable | provenance |
//! |---|---|---|---|---|
//! | `read_file` | file | yes — the file is still there | yes — the path is the address | path |
//! | `write_file` | file | **no** — writing is a side effect | yes — the artifact is on disk | path + content length |
//! | `list_dir` | search | yes | yes | path |
//! | `exec` | log | yes — a pure command re-runs identically | yes | the command itself |
//!
//! Those five fields are what the reducer reads. Command output is a log — an
//! unbounded shape DESIGN names explicitly — so a large one goes to a
//! `log_reader` rather than being dropped, while a large file read (a bounded
//! shape that is reproducible) is dropped with its re-run call. `exec`'s
//! provenance carries the command verbatim either way, so re-obtaining it is
//! never a guess.
//!
//! A tool failure (missing file, bad input) comes back as an [`Inline`] payload
//! whose text begins with `error: `. It is still a result — the caller decides
//! whether to send it as `is_error` via [`ToolResult::to_error_block`] — because
//! a tool that panics or returns nothing tells the model less than one that says
//! what went wrong.
//!
//! [`Inline`]: crate::result::Payload::Inline
//! [`ToolResult::to_error_block`]: crate::result::ToolResult::to_error_block

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;

use serde_json::{Value, json};

use yu_wire::types::{CustomTool, ToolDefinition};

use crate::reduce::{Budget, Pressure, Reducer, Reduction};
use crate::result::{Payload, Provenance, ResultKind, ToolResult};

/// What a tool is handed when it runs.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub reducer: Arc<Reducer>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>, reducer: Arc<Reducer>) -> Self {
        Self {
            cwd: cwd.into(),
            reducer,
        }
    }

    /// A context with the default budget, for callers that have no policy yet.
    pub fn with_default_budget(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd, Arc::new(Reducer::new(Budget::default())))
    }

    /// Resolve a tool-supplied path against the working directory. Absolute
    /// paths are honoured as-is.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }

    /// Choose and apply a reduction in one call, returning the decision as well
    /// as what the model will see — the decision is the event.
    pub fn reduce(
        &self,
        result: ToolResult,
        pressure: Pressure,
        spill: &mut dyn FnMut(&[u8]) -> String,
    ) -> (Reduction, ToolResult) {
        let reduction = self.reducer.choose(&result, pressure);
        let applied = self.reducer.apply(result, &reduction, spill);
        (reduction, applied)
    }
}

/// A tool the harness executes itself.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult;
}

/// `read_file` — one file, in full.
pub struct ReadFile;

/// `write_file` — a side effect, and therefore not reproducible.
pub struct WriteFile;

/// `list_dir` — a query over an unbounded set of entries.
pub struct ListDir;

/// `exec` — `sh -c`, output and exit code captured.
pub struct Exec;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "Read a file as UTF-8 text, relative to the working directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to read, absolute or relative to the working directory."
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ))
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let path = match required_str(self.name(), input, "path") {
            Ok(path) => path,
            Err(why) => return failed(self.name(), input, ResultKind::File, why),
        };
        let resolved = ctx.resolve(path);
        match std::fs::read(&resolved) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text_result(self.name(), input, ResultKind::File, text, true, true),
                Err(err) => failed(
                    self.name(),
                    input,
                    ResultKind::File,
                    format!(
                        "`{}` is not valid UTF-8 ({} bytes); read it with a tool that handles bytes",
                        resolved.display(),
                        err.as_bytes().len()
                    ),
                ),
            },
            Err(err) => failed(
                self.name(),
                input,
                ResultKind::File,
                format!("cannot read `{}`: {err}", resolved.display()),
            ),
        }
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "Write UTF-8 text to a file, creating parent directories as needed.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to write, absolute or relative to the working directory."
                    },
                    "content": {
                        "type": "string",
                        "description": "Exact contents of the file after the call."
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ))
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let path = match required_str(self.name(), input, "path") {
            Ok(path) => path,
            Err(why) => return failed(self.name(), input, ResultKind::File, why),
        };
        let content = match required_str(self.name(), input, "content") {
            Ok(content) => content,
            Err(why) => return failed(self.name(), input, ResultKind::File, why),
        };
        let resolved = ctx.resolve(path);
        if let Some(parent) = resolved.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            return failed(
                self.name(),
                input,
                ResultKind::File,
                format!("cannot create `{}`: {err}", parent.display()),
            );
        }
        match std::fs::write(&resolved, content) {
            // Not reproducible: re-running the call is not a way to re-obtain
            // this result, it is a way to do the side effect twice.
            Ok(()) => text_result(
                self.name(),
                input,
                ResultKind::File,
                format!("wrote {} bytes to `{}`", content.len(), path),
                false,
                // The artifact the call produced is on disk and addressable,
                // even though re-running the call is not the way back to it.
                true,
            ),
            Err(err) => failed(
                self.name(),
                input,
                ResultKind::File,
                format!("cannot write `{}`: {err}", resolved.display()),
            ),
        }
    }
}

impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "List one directory's entries, sorted, directories suffixed with `/`.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list, relative to the working directory. Defaults to `.`."
                    }
                },
                "additionalProperties": false
            }),
        ))
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let resolved = ctx.resolve(path);
        let entries = match std::fs::read_dir(&resolved) {
            Ok(entries) => entries,
            Err(err) => {
                return failed(
                    self.name(),
                    input,
                    ResultKind::Search,
                    format!("cannot list `{}`: {err}", resolved.display()),
                );
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let name = entry.file_name().to_string_lossy().into_owned();
                    names.push(if is_dir { format!("{name}/") } else { name });
                }
                Err(err) => {
                    return failed(
                        self.name(),
                        input,
                        ResultKind::Search,
                        format!("cannot read an entry of `{}`: {err}", resolved.display()),
                    );
                }
            }
        }
        // Sorted, so that re-running produces the same bytes: reproducible.
        names.sort();
        text_result(
            self.name(),
            input,
            ResultKind::Search,
            names.join("\n"),
            true,
            true,
        )
    }
}

impl Tool for Exec {
    fn name(&self) -> &str {
        "exec"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "Run a shell command with `sh -c` in the working directory; stdout, stderr and the exit code are captured.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command line, executed with `sh -c`."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ))
    }

    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let command = match required_str(self.name(), input, "command") {
            Ok(command) => command,
            Err(why) => return failed(self.name(), input, ResultKind::Log, why),
        };
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match output {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let text = render_exec_output(command, &output.status, &stdout, &stderr);
                // A pure command re-runs to the same bytes; the provenance
                // carries the command, so `rerun` is exactly this string.
                text_result(self.name(), input, ResultKind::Log, text, true, true)
            }
            Err(err) => failed(
                self.name(),
                input,
                ResultKind::Log,
                format!(
                    "could not run `sh -c {command}` in `{}`: {err}",
                    ctx.cwd.display()
                ),
            ),
        }
    }
}

/// The evidence block: what was run, what it exited with, and both streams.
fn render_exec_output(command: &str, status: &ExitStatus, stdout: &str, stderr: &str) -> String {
    let mut out = format!("$ {command}\n");
    match status.code() {
        Some(code) => out.push_str(&format!("exit: {code}\n")),
        // Killed by a signal has no exit code, and inventing one would be a lie.
        None => out.push_str("exit: terminated by signal\n"),
    }
    out.push_str(&format!("stdout ({} bytes):\n{stdout}\n", stdout.len()));
    out.push_str(&format!("stderr ({} bytes):\n{stderr}\n", stderr.len()));
    out
}

fn text_result(
    tool: &str,
    input: &Value,
    kind: ResultKind,
    text: String,
    reproducible: bool,
    addressable: bool,
) -> ToolResult {
    ToolResult {
        size: text.len() as u64,
        payload: Payload::Inline(text),
        kind,
        reproducible,
        addressable,
        provenance: Provenance::new(tool, input.clone()),
    }
}

fn failed(tool: &str, input: &Value, kind: ResultKind, message: impl Into<String>) -> ToolResult {
    text_result(
        tool,
        input,
        kind,
        format!("error: {}", message.into()),
        false,
        false,
    )
}

/// The failure is returned as a message rather than a finished result so that
/// each call site can report it in its own shape.
fn required_str<'a>(tool: &str, input: &'a Value, key: &str) -> Result<&'a str, String> {
    match input.get(key).and_then(Value::as_str) {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(format!("`{tool}` requires a non-empty string `{key}`")),
    }
}

/// name → tool, with registration order preserved.
///
/// Order matters beyond tidiness: DESIGN's cache rule is that the byte prefix of
/// a request is stable, and tool definitions render first. A registry that
/// reordered them would invalidate the prefix on every call.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    index: HashMap<String, usize>,
    cwd: PathBuf,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            cwd: PathBuf::from("."),
        }
    }

    /// Register a tool. A name that is already present is replaced in place, so
    /// the render order never depends on how many times a name was registered.
    pub fn register(&mut self, tool: Box<dyn Tool>) -> &mut Self {
        let name = tool.name().to_owned();
        match self.index.get(&name).copied() {
            Some(existing) => self.tools[existing] = tool,
            None => {
                self.index.insert(name, self.tools.len());
                self.tools.push(tool);
            }
        }
        self
    }

    /// A registry with `read_file`, `write_file`, `list_dir`, `exec`.
    /// Everything: reading, writing, and arbitrary command execution.
    ///
    /// This is the set an executor needs to do real work, and it is also the set
    /// that can do real damage. It is named `native` rather than `default`
    /// deliberately — DESIGN holds that an agent's tool surface is a decision,
    /// and a caller should have to reach for `exec` rather than receive it by
    /// omission.
    pub fn native(ctx_cwd: impl Into<PathBuf>) -> Self {
        let mut registry = Self::new();
        registry.cwd = ctx_cwd.into();
        registry
            .register(Box::new(ReadFile))
            .register(Box::new(WriteFile))
            .register(Box::new(ListDir))
            .register(Box::new(Exec));
        registry
    }

    /// Reading only: no `write_file`, no `exec`.
    ///
    /// For work that reasons about a tree without changing it, and for any run
    /// that has not been explicitly authorised to execute commands. The
    /// difference between this and [`Self::native`] is the whole of what an agent
    /// can affect, so it is worth being able to state which one is in force.
    pub fn read_only(ctx_cwd: impl Into<PathBuf>) -> Self {
        let mut registry = Self::new();
        registry.cwd = ctx_cwd.into();
        registry
            .register(Box::new(ReadFile))
            .register(Box::new(ListDir));
        registry
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.index.get(name).map(|index| &*self.tools[*index])
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|tool| tool.definition()).collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }

    /// The working directory this registry's tools were built for.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// A ready context for this registry's tools.
    pub fn context(&self) -> ToolContext {
        ToolContext::with_default_budget(self.cwd.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext::with_default_budget(dir)
    }

    #[test]
    fn contexts_and_registries_cross_thread_boundaries() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ToolContext>();
        assert_send_sync::<ToolRegistry>();
        assert_send_sync::<Reducer>();
        assert_send_sync::<ToolResult>();
    }

    #[test]
    fn native_registry_has_the_four_tools_in_a_stable_order() {
        let registry = ToolRegistry::native("/tmp");
        assert_eq!(
            registry.names(),
            vec!["read_file", "write_file", "list_dir", "exec"]
        );
        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 4);
        for (definition, name) in definitions.iter().zip(registry.names()) {
            assert_eq!(definition.name(), name);
            assert!(!definition.is_server_defined());
        }
    }

    #[test]
    fn definitions_are_byte_stable_across_calls() {
        let registry = ToolRegistry::native("/tmp");
        let a = serde_json::to_string(&registry.definitions()).unwrap();
        let b = serde_json::to_string(&registry.definitions()).unwrap();
        assert_eq!(a, b, "tool definitions render first; they must not move");
    }

    #[test]
    fn registering_a_duplicate_name_replaces_in_place() {
        let mut registry = ToolRegistry::native("/tmp");
        registry.register(Box::new(ReadFile));
        assert_eq!(
            registry.names(),
            vec!["read_file", "write_file", "list_dir", "exec"]
        );
        assert_eq!(registry.definitions().len(), 4);
    }

    #[test]
    fn unknown_tools_are_absent_rather_than_substituted() {
        let registry = ToolRegistry::native("/tmp");
        assert!(registry.get("rm_rf").is_none());
        assert!(registry.get("read_file").is_some());
    }

    #[test]
    fn read_file_returns_the_file_and_marks_it_addressable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let result = ReadFile.execute(&json!({"path": "a.txt"}), &ctx(dir.path()));
        assert_eq!(result.payload.text(), Some("hello\n"));
        assert_eq!(result.kind, ResultKind::File);
        assert!(result.reproducible);
        assert!(result.addressable);
        assert!(result.is_complete());
        assert_eq!(result.provenance.input, json!({"path": "a.txt"}));
    }

    #[test]
    fn read_file_reports_a_missing_file_as_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let result = ReadFile.execute(&json!({"path": "nope.txt"}), &ctx(dir.path()));
        let text = result.payload.text().unwrap();
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains("nope.txt"), "{text}");
    }

    #[test]
    fn write_file_writes_and_is_not_reproducible() {
        let dir = tempfile::tempdir().unwrap();
        let result = WriteFile.execute(
            &json!({"path": "sub/b.txt", "content": "body"}),
            &ctx(dir.path()),
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/b.txt")).unwrap(),
            "body"
        );
        assert!(!result.reproducible, "a write is a side effect");
        assert!(result.addressable);
        assert!(result.payload.text().unwrap().contains("4 bytes"));
    }

    #[test]
    fn list_dir_sorts_and_marks_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let result = ListDir.execute(&json!({"path": "."}), &ctx(dir.path()));
        assert_eq!(result.payload.text().unwrap(), "a.txt\nb.txt\nzdir/");
        assert_eq!(result.kind, ResultKind::Search);
        assert!(result.reproducible);
    }

    #[test]
    fn list_dir_defaults_to_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only.txt"), "").unwrap();
        let result = ListDir.execute(&json!({}), &ctx(dir.path()));
        assert_eq!(result.payload.text().unwrap(), "only.txt");
    }

    #[test]
    fn exec_captures_stdout_and_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let result = Exec.execute(
            &json!({"command": "printf 'out'; exit 3"}),
            &ctx(dir.path()),
        );
        let text = result.payload.text().unwrap();
        assert!(text.contains("exit: 3"), "{text}");
        assert!(text.contains("out"), "{text}");
        assert_eq!(result.kind, ResultKind::Log);
        assert!(result.reproducible);
        assert_eq!(
            result
                .provenance
                .input
                .get("command")
                .and_then(Value::as_str),
            Some("printf 'out'; exit 3"),
            "the provenance must be enough to re-run the command"
        );
    }

    #[test]
    fn exec_captures_stderr_separately_from_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let result = Exec.execute(
            &json!({"command": "printf 'to-out'; printf 'to-err' >&2"}),
            &ctx(dir.path()),
        );
        let text = result.payload.text().unwrap();
        // Split after the echoed command line, so the assertion is about the
        // captured streams and not about the command that produced them.
        let (_, streams) = text.split_once("stdout (").unwrap();
        let (stdout, stderr) = streams.split_once("stderr (").unwrap();
        assert!(stdout.contains("to-out"));
        assert!(
            !stdout.contains("to-err"),
            "streams must not be merged: {text}"
        );
        assert!(stderr.contains("to-err"));
    }

    #[test]
    fn exec_runs_in_the_context_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = Exec.execute(&json!({"command": "pwd"}), &ctx(dir.path()));
        let text = result.payload.text().unwrap();
        // macOS reports /private/var for a temp dir; compare the tail.
        let tail = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(text.contains(&tail), "{text}");
    }

    #[test]
    fn exec_reports_a_shell_that_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = ctx(dir.path());
        bad.cwd = dir.path().join("does-not-exist");
        let result = Exec.execute(&json!({"command": "true"}), &bad);
        assert!(result.payload.text().unwrap().starts_with("error: "));
    }

    #[test]
    fn missing_input_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let result = Exec.execute(&json!({}), &ctx(dir.path()));
        let text = result.payload.text().unwrap();
        assert!(
            text.contains("requires a non-empty string `command`"),
            "{text}"
        );
    }

    #[test]
    fn the_context_reduce_helper_returns_the_event_and_the_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut big = "x".repeat(40_000);
        big.push('\n');
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let context = ctx(dir.path());
        let raw = ReadFile.execute(&json!({"path": "big.txt"}), &context);
        let mut spill = |bytes: &[u8]| format!("digest:{}", bytes.len());
        let (reduction, shown) = context.reduce(raw, Pressure::high(), &mut spill);
        // A large, reproducible file read is dropped, with its re-run call.
        assert!(matches!(reduction, Reduction::DropWithManifest { .. }));
        assert!(!shown.is_complete());
        assert_eq!(shown.size, big.len() as u64);
        assert!(shown.render().contains("read_file"));
    }
}
