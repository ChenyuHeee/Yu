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

use yu_journal::digest_of;
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

/// A side effect a tool is about to perform, declared before it performs it.
///
/// DESIGN: "effect 纪律:**先记后做**。回放遇到已提交的 effect 直接读记录,绝不重放副作用;
/// 有副作用的工具带 idempotency key". The declaration exists so the loop can put the
/// intent in the record *before* the act: a record that ends on an intent is a
/// record that was interrupted inside the one window this discipline exists for,
/// and a record with no intent before the act cannot say the act was coming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectIntent {
    /// The effect's identity as the tool understands it: what is being changed,
    /// and to what. The loop appends the call's id to make it unique per call —
    /// the same tool with the same input twice in one run is two effects, not
    /// one, so a key naming only the inputs would let the second be skipped as
    /// though it were the first.
    pub key: String,
    /// What the effect is, in words, for whoever reads the record afterwards.
    pub description: String,
    /// Whether performing this effect twice is the same as performing it once.
    ///
    /// The tool's claim, and it is recorded because a replay is answered by the
    /// record: an interrupted effect is otherwise undecidable, and a key that
    /// certifies nothing may be neither skipped nor re-performed without asking
    /// someone.
    pub idempotent: bool,
}

/// Whether an effect that was recorded as intended is already in place.
///
/// The world is evidence about the crash window, and for its second half it is
/// the only evidence there is: the record says the act was about to happen and
/// says nothing about whether it did, so a tool that can look at what it would
/// have changed answers the question the record cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectVerdict {
    /// The effect's outcome is present: the bytes are written, the artifact
    /// matches. The act happened; only the record of it is missing.
    InPlace,
    /// The effect's outcome is absent, as far as the tool can tell. The act did
    /// not happen, or was undone.
    NotInPlace,
    /// The tool cannot tell — a command's effects are not inspectable, and
    /// answering either way would be a claim nothing supports.
    Unknown,
}

/// A tool the harness executes itself.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult;

    /// The side effect this call will perform, or `None` for a tool that changes
    /// nothing.
    ///
    /// The default is `None` because most tools are readers, and because
    /// declaring an effect is a claim only the tool's author can make — a wrong
    /// `Some` would put a needless stop in a replay's path. The other direction
    /// is guarded rather than defaulted: every tool in [`ToolRegistry::native`]
    /// that can change something declares one, and
    /// `every_mutating_tool_declares_its_effect` fails if one stops.
    fn effect(&self, _input: &Value) -> Option<EffectIntent> {
        None
    }

    /// Whether an effect recorded as intended is already in place in the world.
    ///
    /// Consulted when a replay meets an intent the record never finished — the
    /// half of the crash window the record cannot answer. `Unknown` is the
    /// default because a tool that has not said how to look cannot be assumed to
    /// have looked.
    fn effect_in_place(&self, _input: &Value, _ctx: &ToolContext) -> EffectVerdict {
        EffectVerdict::Unknown
    }
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

    fn effect(&self, input: &Value) -> Option<EffectIntent> {
        let path = input.get("path").and_then(Value::as_str)?;
        let content = input.get("content").and_then(Value::as_str)?;
        Some(write_intent(path, content))
    }

    /// The one native tool whose effect is *inspectable*, and therefore the one
    /// that can answer the crash window's open question.
    ///
    /// The key names the bytes and the place, so "did this happen?" is a question
    /// about the world with a checkable answer: read the path and hash it. A
    /// record that ends on this intent, next to a file that already holds those
    /// exact bytes, is a record of an act that happened and was not written down.
    fn effect_in_place(&self, input: &Value, ctx: &ToolContext) -> EffectVerdict {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return EffectVerdict::Unknown;
        };
        let Some(content) = input.get("content").and_then(Value::as_str) else {
            return EffectVerdict::Unknown;
        };
        match std::fs::read(ctx.resolve(path)) {
            Ok(bytes) if bytes == content.as_bytes() => EffectVerdict::InPlace,
            Ok(_) => EffectVerdict::NotInPlace,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => EffectVerdict::NotInPlace,
            Err(_) => EffectVerdict::Unknown,
        }
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

    /// A command is an effect because nothing here can know that it is not.
    ///
    /// `cargo test` and `rm -rf build` arrive through the same door, and the tool
    /// has no way to tell them apart — so the honest declaration is that a shell
    /// command may change anything.
    fn effect(&self, input: &Value) -> Option<EffectIntent> {
        let command = input.get("command").and_then(Value::as_str)?;
        Some(EffectIntent {
            key: format!("exec:sha256:{}", digest_of(command.as_bytes())),
            description: format!("run `sh -c {command}`"),
            // A command is not its own key: running the same one twice is two
            // effects, which is why this is `false` and not a claim that the
            // second run is free.
            idempotent: false,
        })
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

/// The header's two halves, defined once because the writer and the reader must
/// not drift: the bug this replaces was a writer emitting `exit: 3` and a reader
/// searching for `exit ` — with a space — which returned `None` on every real
/// command, so the decision layer's evidence block read `exit unknown` always.
const COMMAND_ECHO: &str = "$ ";
const EXIT_MARKER: &str = "exit: ";

/// The evidence block: what was run, what it exited with, and both streams.
///
/// Producer for [`parse_exit_code`], and the pair is the point. The block opens
/// with the echoed command and the exit marker, and only after both come the
/// streams, so the marker's position is a function of the command rather than a
/// property of the text. A reader therefore never has to search for it — and
/// searching is exactly how a command that merely *printed* something shaped
/// like a marker would get away with reporting another command's status.
fn render_exec_output(command: &str, status: &ExitStatus, stdout: &str, stderr: &str) -> String {
    let mut out = format!("{COMMAND_ECHO}{command}\n");
    match status.code() {
        Some(code) => out.push_str(&format!("{EXIT_MARKER}{code}\n")),
        // Killed by a signal has no exit code, and inventing one would be a lie.
        None => out.push_str(&format!("{EXIT_MARKER}terminated by signal\n")),
    }
    out.push_str(&format!("stdout ({} bytes):\n{stdout}\n", stdout.len()));
    out.push_str(&format!("stderr ({} bytes):\n{stderr}\n", stderr.len()));
    out
}

/// Read the exit status back out of `output`, which must be what
/// [`render_exec_output`] wrote for `command`.
///
/// This lives beside the producer because the producer owns the format; the
/// consumer (`yu-core`'s evidence) calls it rather than re-deriving it, so there
/// is one definition of the contract instead of two that have to agree.
///
/// The parse is structural, not a search. The block begins with `$ ` and the
/// command verbatim, so the marker sits at a byte offset computed from the
/// command the caller already has — it is the command it ran. Nothing that
/// follows the marker is read. That matters: `printf 'exit 0\n'; exit 1` exits
/// nonzero while printing a line that reads exactly like a success, and a
/// parser that scanned the tail for something marker-shaped would hand the
/// decision layer `0` — an assertion reported as verified when it is false.
///
/// `None` means "no exit status was recovered" and never "it passed": a command
/// killed by a signal has no code to recover, and text that is not a render at
/// all (a reduction's pointer, a reader's answer, an older hand-written format)
/// is not guessed at.
pub fn parse_exit_code(command: &str, output: &str) -> Option<i32> {
    let rest = output.strip_prefix(COMMAND_ECHO)?;
    let rest = rest.strip_prefix(command)?;
    let rest = rest.strip_prefix('\n')?;
    let marker = rest.lines().next()?;
    marker.strip_prefix(EXIT_MARKER)?.trim().parse().ok()
}

/// The intent behind a `write_file` call, in the one place both the tool and its
/// tests read it from.
///
/// The key names the place and the bytes, so that the effect it identifies is the
/// *whole* effect: writing different content to the same path is a different
/// effect, and so is writing the same content elsewhere. That is what makes the
/// claim below — that performing it twice is the same as once — a fact about the
/// bytes rather than about the call.
fn write_intent(path: &str, content: &str) -> EffectIntent {
    EffectIntent {
        key: format!(
            "write_file:{path}#sha256:{}",
            digest_of(content.as_bytes())
        ),
        description: format!("write {} bytes to `{path}`", content.len()),
        idempotent: true,
    }
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
    /// The reducer every context built from this registry runs under.
    ///
    /// Held here rather than passed at each call site because the registry is
    /// where a run's tool surface is assembled, and the reduction table is part
    /// of that surface: which of DESIGN's five paths a result takes is policy,
    /// and a policy that reached only some of the tools would be a second,
    /// contradicting account of what the run does.
    reducer: Option<Arc<Reducer>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            cwd: PathBuf::from("."),
            reducer: None,
        }
    }

    /// Build every context from `reducer` instead of the default budget.
    ///
    /// This is where a policy's `[[reduction]]` rows reach the code that reduces
    /// — `yu-policy` implements `yu_tools::reduce::ReductionTable`, the front end
    /// hands the table in here, and nothing below this line knows what a TOML
    /// file is. DESIGN's constraint 0: "改一条策略规则,不重新编译 Rust".
    pub fn with_reducer(mut self, reducer: Arc<Reducer>) -> Self {
        self.reducer = Some(reducer);
        self
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
    ///
    /// The context carries the registry's reducer, so a table installed with
    /// [`ToolRegistry::with_reducer`] is the one every call actually reduces
    /// under. Building the context with the default budget here — which is what
    /// this did — is how a policy could be loaded, parsed and consulted by nobody.
    pub fn context(&self) -> ToolContext {
        match &self.reducer {
            Some(reducer) => ToolContext::new(self.cwd.clone(), reducer.clone()),
            None => ToolContext::with_default_budget(self.cwd.clone()),
        }
    }

    /// The reducer this registry's contexts run under.
    pub fn reducer(&self) -> Arc<Reducer> {
        self.reducer
            .clone()
            .unwrap_or_else(|| Arc::new(Reducer::new(Budget::default())))
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

    /// The round trip that makes the marker a contract rather than a string two
    /// modules happen to share: a real command in, the code it exited with out.
    ///
    /// Changing what `render_exec_output` writes without teaching
    /// `parse_exit_code` the same format fails here, which is the point — the
    /// two ends were written once, ten lines apart, disagreeing about a colon.
    #[test]
    fn the_exit_marker_round_trips_from_a_real_command_to_its_code() {
        let dir = tempfile::tempdir().unwrap();
        let command = "printf 'six bytes'; exit 3";
        let result = Exec.execute(&json!({"command": command}), &ctx(dir.path()));
        let text = result.payload.text().unwrap();
        assert_eq!(parse_exit_code(command, text), Some(3));
        assert_eq!(parse_exit_code(command, "some output\nexit 3\n"), None);
    }

    /// A command that prints a line shaped like the marker is not the marker.
    ///
    /// This command fails while its output ends with a line reading exactly like
    /// a success. The tail scan that this replaced found that line and reported
    /// `0` — a passing verdict drawn from the failed command's own noise. The
    /// structural read never looks into the streams, so the status it returns is
    /// the one the shell exited with.
    #[test]
    fn stdout_that_resembles_the_marker_does_not_become_the_status() {
        let dir = tempfile::tempdir().unwrap();
        let command = "printf 'done\\nexit 0'; exit 1";
        let result = Exec.execute(&json!({"command": command}), &ctx(dir.path()));
        let text = result.payload.text().unwrap();
        assert_eq!(parse_exit_code(command, text), Some(1));
    }

    #[test]
    fn a_marker_that_is_not_an_evidence_block_is_not_guessed_at() {
        // A reduction's pointer or a reader's answer is not a render, and the
        // honest answer for it is "no status", never a number.
        assert_eq!(
            parse_exit_code("true", "the full log result is on disk"),
            None
        );
        // A signal has no exit code to recover; `None`, not an invented one.
        let killed =
            "$ sleep 5\nexit: terminated by signal\nstdout (0 bytes):\n\nstderr (0 bytes):\n\n";
        assert_eq!(parse_exit_code("sleep 5", killed), None);
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

    /// Every tool that can change something says so, and every tool that cannot
    /// says nothing.
    ///
    /// The default is `None`, which is the right default for a crate of readers
    /// and the wrong answer for `write_file` and `exec`. A tool that performs a
    /// side effect without declaring it is the one hole the crash discipline
    /// cannot see: its writes never enter the record, so a replay re-does them
    /// with nothing anywhere to say that it did.
    #[test]
    fn every_mutating_tool_declares_its_effect() {
        let registry = ToolRegistry::native("/tmp");
        let mutating = ["write_file", "exec"];
        for name in registry.names() {
            let tool = registry.get(&name).expect("registered");
            let declared = tool.effect(&json!({"path": "p", "content": "c", "command": "true"}));
            if mutating.contains(&name.as_str()) {
                assert!(
                    declared.is_some(),
                    "`{name}` can change the world and declares no effect, so nothing \
                     about it ever reaches the record"
                );
            } else {
                assert!(
                    declared.is_none(),
                    "`{name}` declares an effect it does not have: {declared:?}"
                );
            }
        }
        // The read-only surface changes nothing and therefore declares nothing.
        let read_only = ToolRegistry::read_only("/tmp");
        for name in read_only.names() {
            assert!(
                read_only
                    .get(&name)
                    .expect("registered")
                    .effect(&json!({"path": "p"}))
                    .is_none(),
                "`{name}` is offered as read-only and declares an effect"
            );
        }
    }

    /// The write's key names the bytes and the place, and its verdict is checked
    /// against the world.
    ///
    /// This is the pair a record needs in order to answer the crash window's
    /// second half: the key is what "did it happen?" can be asked *about*, and
    /// the verdict is the asking.
    #[test]
    fn a_write_declares_what_it_will_change_and_reports_whether_it_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let context = ctx(dir.path());
        let tool = WriteFile;
        let input = json!({"path": "out.txt", "content": "hello"});
        let intent = tool.effect(&input).expect("writing is an effect");
        assert!(
            intent.key.starts_with("write_file:out.txt#sha256:"),
            "the key must name the place and the bytes: {}",
            intent.key
        );
        assert!(intent.idempotent, "writing the same bytes twice is one write");

        assert_eq!(
            tool.effect_in_place(&input, &context),
            EffectVerdict::NotInPlace,
            "nothing is written yet"
        );
        WriteFile.execute(&input, &context);
        assert_eq!(
            tool.effect_in_place(&input, &context),
            EffectVerdict::InPlace,
            "the bytes are on disk, so the act happened"
        );
        // Same path, different bytes: a different effect, and not in place.
        assert_eq!(
            tool.effect_in_place(&json!({"path": "out.txt", "content": "other"}), &context),
            EffectVerdict::NotInPlace
        );
        assert_ne!(
            write_intent("out.txt", "hello").key,
            write_intent("out.txt", "other").key,
            "writing different bytes is a different effect"
        );
    }

    /// A command cannot certify anything about itself.
    ///
    /// This is the safe direction stated as a value: `unknown` means a replay of
    /// an unfinished command stops and asks, rather than choosing between losing
    /// the work and doing it twice.
    #[test]
    fn a_command_declares_an_effect_it_cannot_certify() {
        let dir = tempfile::tempdir().unwrap();
        let intent = Exec
            .effect(&json!({"command": "rm -rf build"}))
            .expect("a command may change anything, so it is an effect");
        assert!(!intent.idempotent, "a command is not its own key");
        assert!(intent.description.contains("rm -rf build"), "{intent:?}");
        assert_eq!(
            Exec.effect_in_place(&json!({"command": "rm -rf build"}), &ctx(dir.path())),
            EffectVerdict::Unknown,
            "a command's effects are not inspectable, and saying otherwise would be \
             a claim nothing supports"
        );
    }

    /// The reduction table reaches the tools through the registry, or a policy
    /// can be loaded and consulted by nobody.
    #[test]
    fn a_registry_reducer_reaches_every_context_it_builds() {
        use crate::reduce::{Axes, ReductionPath, ReductionTable};
        struct AlwaysDrop;
        impl ReductionTable for AlwaysDrop {
            fn path_for(&self, _: &Axes<'_>) -> Option<ReductionPath> {
                Some(ReductionPath::DropWithManifest)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let payload = "x".repeat(64);
        std::fs::write(dir.path().join("f.txt"), &payload).unwrap();
        let input = json!({"path": "f.txt"});

        let plain = ToolRegistry::read_only(dir.path());
        let raw = ReadFile.execute(&input, &plain.context());
        let mut spill = |_: &[u8]| String::new();
        let (path, _) =
            plain
                .context()
                .reduce(raw.clone(), Pressure::low(), &mut spill);
        assert_eq!(path, Reduction::PassThrough, "small payloads pass through");

        let tabled = ToolRegistry::read_only(dir.path()).with_reducer(Arc::new(
            Reducer::new(Budget::default()).with_table(Arc::new(AlwaysDrop)),
        ));
        let mut spill = |_: &[u8]| String::new();
        let (path, shown) = tabled.context().reduce(raw, Pressure::low(), &mut spill);
        assert!(
            matches!(path, Reduction::DropWithManifest { .. }),
            "the registry's table did not reach the reducer: {path:?}"
        );
        assert!(!shown.is_complete());
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

#[cfg(test)]
mod wire_shape_baseline {
    use super::*;

    /// The four built-in tools' wire shape, as of the commit that introduced
    /// this constant. One entry per tool, in registry order.
    const WIRE_SHAPE: [&str; 4] = [
        r#"{"name":"read_file","description":"Read a file as UTF-8 text, relative to the working directory.","input_schema":{"additionalProperties":false,"properties":{"path":{"description":"File to read, absolute or relative to the working directory.","type":"string"}},"required":["path"],"type":"object"}}"#,
        r#"{"name":"write_file","description":"Write UTF-8 text to a file, creating parent directories as needed.","input_schema":{"additionalProperties":false,"properties":{"content":{"description":"Exact contents of the file after the call.","type":"string"},"path":{"description":"File to write, absolute or relative to the working directory.","type":"string"}},"required":["path","content"],"type":"object"}}"#,
        r#"{"name":"list_dir","description":"List one directory's entries, sorted, directories suffixed with `/`.","input_schema":{"additionalProperties":false,"properties":{"path":{"description":"Directory to list, relative to the working directory. Defaults to `.`.","type":"string"}},"type":"object"}}"#,
        r#"{"name":"exec","description":"Run a shell command with `sh -c` in the working directory; stdout, stderr and the exit code are captured.","input_schema":{"additionalProperties":false,"properties":{"command":{"description":"Command line, executed with `sh -c`.","type":"string"}},"required":["command"],"type":"object"}}"#,
    ];


    /// **TODO 第 3 步的判据:「不给 profile 时,发出去的请求与今天逐字节相同」.**
    ///
    /// This pins the wire shape of the four built-in tools as it is *today*, so
    /// that adding a profile cannot quietly move it. The profile work is only
    /// safe if the un-profiled path is provably unchanged — and "provably" means
    /// a test that fails when a byte moves, not a review that reads two diffs and
    /// believes them.
    ///
    /// Compared as the serialized wire form rather than as Rust values, because
    /// the wire form is what the model sees: a `serde(rename)` or a reordered
    /// field moves the request without moving any Rust-level equality.
    #[test]
    fn the_built_in_tool_shapes_are_unchanged() {
        let rendered: Vec<String> = ToolRegistry::native(".")
            .definitions()
            .iter()
            .map(|d| serde_json::to_string(d).expect("a definition serializes"))
            .collect();
        assert_eq!(
            rendered.as_slice(),
            WIRE_SHAPE,
            "the built-in tools' wire shape moved. If that was deliberate, update \
             this constant — and say in the commit why, because every request sent \
             before the change described a shape that no longer exists, and the \
             models trained on it have not been told."
        );
    }

    /// The four names, pinned separately: the JSON above says *something* moved,
    /// this says *what*, which is the difference between a guard and a riddle.
    #[test]
    fn the_built_in_tool_names_are_unchanged() {
        assert_eq!(
            ToolRegistry::native(".").names(),
            vec!["read_file", "write_file", "list_dir", "exec"]
        );
    }
}
