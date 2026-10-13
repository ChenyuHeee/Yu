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
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use yu_journal::digest_of;
use yu_wire::types::{CustomTool, ToolDefinition};

use crate::profile::{ProfileError, ToolProfile};
use crate::reduce::{Budget, Pressure, Reducer, Reduction};
use crate::result::{Payload, Provenance, ResultKind, ToolResult};

/// What a tool is handed when it runs.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub reducer: Arc<Reducer>,
    /// The face this call arrived under, when it arrived through a profile.
    ///
    /// `None` is the unmeasured case: our own invented face, and our own result
    /// form. `Some("Read")` means the call came in wearing a face the model was
    /// trained on, and the implementation answers in the shape that face implies
    /// — see [`ReadFile::execute`] for the one place it is consulted.
    face: Option<String>,
}

impl ToolContext {
    pub fn new(cwd: impl Into<PathBuf>, reducer: Arc<Reducer>) -> Self {
        Self {
            cwd: cwd.into(),
            reducer,
            face: None,
        }
    }

    /// The name this call wore on the wire, when a profile gave it one.
    pub fn face(&self) -> Option<&str> {
        self.face.as_deref()
    }

    /// Whether this call arrived under a profiled face at all.
    pub fn is_aligned(&self) -> bool {
        self.face.is_some()
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

/// `edit` — exact string replacement, the heaviest convention of the four.
///
/// Not registered by [`ToolRegistry::native`] or [`ToolRegistry::read_only`]:
/// those two surfaces are pinned byte for byte by `wire_shape_baseline`, and a
/// fifth tool is a surface change those surfaces are not meant to absorb. It
/// comes in with a profile, which is where its face is the measured one.
pub struct Edit;

// ---------------------------------------------------------------------------
// The face table
// ---------------------------------------------------------------------------

/// A sentence of a captured description this harness will not repeat, and why.
///
/// Note what this is *not*: it is not a rewrite. A sentence that cannot be
/// honoured is removed whole — the surrounding text is left exactly as the
/// snapshot wrote it, because borrowed-sounding wording that nobody was trained
/// on is the failure DESIGN names when it says 借来的措辞 … 看起来熟悉、所以更难被发现.
struct Refusal {
    /// The text as the snapshot carries it, including the separator that goes
    /// with it, so that removing it leaves the rest of the description joined
    /// the way its author wrote it.
    text: &'static str,
    /// Why this harness cannot stand behind the sentence. Not decoration: the
    /// load fails if `text` is missing from the snapshot, so a re-captured
    /// profile that reworded a sentence cannot quietly resurrect the claim.
    why: &'static str,
}

/// A name on the aligned surface, the implementation that answers it, and what
/// this harness does about the parts of the captured face it cannot honour.
///
/// This table is the whole of `wire_name → impl`, and the only place the
/// correspondence is written down. Everything else about a profiled tool — its
/// name, its description, its schema — comes from the snapshot, because
/// 形状是数据,不是编译进去的. What a snapshot cannot say is which of *our*
/// implementations stands behind each face, and which of the face's promises
/// that implementation can keep; that is this table, and every entry in it is a
/// claim this crate has to be able to defend.
struct Face {
    /// The name the profile carries for this tool.
    wire: &'static str,
    /// The implementation that wears it.
    make: fn() -> Box<dyn Tool>,
    /// wire parameter → the key the implementation reads.
    ///
    /// A parameter of the captured schema that is not listed here is one this
    /// harness does not back, and it does not go on the wire: DESIGN 约束 2,
    /// 「抄了 `Grep` 的 `output_mode` 就得实现它」 — a parameter is a promise.
    params: &'static [(&'static str, &'static str)],
    /// Whether this implementation can change the world — the same split
    /// `native` and `read_only` make, and the reason `profiled_read_only`
    /// exists.
    mutates: bool,
    /// Sentences of the captured description this harness refuses to adopt.
    refuses: &'static [Refusal],
}

/// The four faces this crate can answer, out of the 26 a 2.1.248 profile names.
///
/// DESIGN 约束 4: 「不提供的,就不要放进去」 — an entry with no implementation is not
/// advertised at all, so the other 22 simply do not exist for this run. What is
/// *not* here is as deliberate as what is: `Grep`/`Glob` are absent from this
/// client's surface entirely (measured — zero occurrences in the capture), and
/// the `Task*`, `Cron*`, `Agent`, `Skill`, `NotebookEdit`, `WebFetch` and
/// `WebSearch` faces all describe machinery this harness does not have.
static FACES: [Face; 4] = [
    Face {
        wire: "Read",
        make: || Box::new(ReadFile),
        params: &[("file_path", "path"), ("offset", "offset"), ("limit", "limit")],
        mutates: false,
        refuses: READ_REFUSES,
    },
    Face {
        wire: "Write",
        make: || Box::new(WriteFile),
        params: &[("file_path", "path"), ("content", "content")],
        mutates: true,
        refuses: WRITE_REFUSES,
    },
    Face {
        wire: "Edit",
        make: || Box::new(Edit),
        params: &[
            ("file_path", "file_path"),
            ("old_string", "old_string"),
            ("new_string", "new_string"),
            ("replace_all", "replace_all"),
        ],
        mutates: true,
        refuses: EDIT_REFUSES,
    },
    Face {
        wire: "Bash",
        make: || Box::new(Exec),
        params: &[
            ("command", "command"),
            ("timeout", "timeout"),
            ("description", "description"),
        ],
        mutates: true,
        refuses: BASH_REFUSES,
    },
];

/// `Read`'s captured sentences this harness will not repeat.
///
/// Kept: "Reads a file from the local filesystem."; the offset/limit invitation
/// (both parameters are implemented, and line-based as it says); and the
/// `cat -n` sentence — [`ReadFile::execute`] numbers lines under this face,
/// starting at 1, exactly as the capture's own `tool_result` shows (see
/// `captures/claude-code/raw/requests/0003.body.bin`: `"1\tThe blue crumb…\n2\t"`).
/// Note that the measurement, not the tool's name, is the authority there: a
/// real `cat -n` does not number a file's trailing empty piece, and the captured
/// output does.
const READ_REFUSES: &[Refusal] = &[
    Refusal {
        text: "- `file_path` must be an absolute path.\n",
        why: "TODO step 4 names this one: this harness resolves a relative path against the \
              working directory (ToolContext::resolve), so a relative path does not fail. The \
              schema's own property text still asks for an absolute path — asking is not \
              claiming.",
    },
    Refusal {
        text: "- Reads up to 2000 lines by default.\n",
        why: "there is no default line cap. Truncation is the reduction machinery's job and it \
              does it with an explicit manifest; a second, silent cap inside the reader would \
              hide bytes that no manifest accounts for.",
    },
    Refusal {
        text: "- Reads images (PNG, JPG, …) and presents them visually. Reads PDFs via the `pages` \
               parameter (e.g. \"1-5\", max 20 pages/request; required for PDFs over 10 pages). \
               Reads Jupyter notebooks (.ipynb) as cells with outputs.\n",
        why: "this crate reads UTF-8 text and nothing else; there is no image, PDF or notebook \
              reader behind this face. The `pages` parameter goes with the sentence — it is not \
              in the face table's parameter list, so it is not advertised either.",
    },
    Refusal {
        text: "- Reading a directory, a missing file, or an empty file returns an error or system \
               reminder rather than content.\n",
        why: "two of the three hold here (a directory and a missing file are errors) and the \
              third does not: an empty file is an empty result, not an error, because nothing \
              failed. There is also no 'system reminder' channel in this harness. A sentence \
              that is two-thirds true is a sentence a model cannot act on.",
    },
    Refusal {
        text: "- Do NOT re-read a file you just edited to verify — Edit/Write would have errored \
               if the change failed, and the harness tracks file state for you.",
        why: "this harness keeps no record of which files a conversation has read, so 'the \
              harness tracks file state for you' is false. (TODO step 4 names the `Edit` twin of \
              this sentence; they are the same claim.)",
    },
];

/// `Write`'s captured sentences this harness will not repeat.
const WRITE_REFUSES: &[Refusal] = &[
    Refusal {
        text: " Overwriting an existing file you haven't Read will fail.",
        why: "same missing state as `Read`'s refusal above: nothing here refuses a write because \
              a file was not read first. 'When to use: … one you've already Read' stays, because \
              it recommends an order of work rather than claiming an enforcement that does not \
              exist.",
    },
];

/// `Edit`'s captured sentences this harness will not repeat.
const EDIT_REFUSES: &[Refusal] = &[
    Refusal {
        text: "- You must Read the file in this conversation before editing, or the call will \
               fail.\n",
        why: "TODO step 4's example, and the one that decided this whole exercise: the harness \
              does not track which files a conversation has read, so the sentence is false as \
              written. The other two bullets are kept *because they are true* — exact match and \
              uniqueness are enforced in `Edit::execute`, and the strip-the-prefix instruction is \
              exactly right for the numbered output `Read` produces under this face.",
    },
];

/// `Bash`'s captured sentences this harness will not repeat.
///
/// Kept: the avoid-list bullet (this harness has dedicated tools for `cat`,
/// `head`, `tail`, `sed`, `awk` and `echo` — that is what it is telling the model
/// to use) and the `timeout` bullet, which `Exec` implements: default 120000 ms,
/// clamped at 600000. The Git section keeps its two operating rules and loses
/// Claude Code's own bylines, which are not this harness's to sign.
const BASH_REFUSES: &[Refusal] = &[
    Refusal {
        text: "Executes a bash command and returns its output.\n\n",
        why: "`Exec` runs `sh -c` (see `Exec::execute`). On a machine whose /bin/sh is dash that \
              sentence is false, and rather than swap the shell the harness actually runs — which \
              would change the un-profiled path too — the sentence is removed. What is left \
              starts with the avoid-list, which is the part of this description a model acts on.",
    },
    Refusal {
        text: "- Working directory persists between calls, but prefer absolute paths — `cd` in a \
               compound command can trigger a permission prompt. Shell state (env vars, \
               functions) does not persist; the shell is initialized from the user's profile.\n",
        why: "nothing in it holds: each call is a fresh `sh -c` started in the context's working \
              directory (no persistence), there is no permission prompt to trigger, and a \
              non-interactive shell reads no profile.",
    },
    Refusal {
        text: "- Command output is displayed to you, not reliably to the user.\n",
        why: "this harness has no user-facing display; the sentence describes a UI that does not \
              exist here.",
    },
    Refusal {
        text: "- `run_in_background` runs the command detached: it keeps running across turns and \
               re-invokes you when it exits. No `&` needed.\n",
        why: "there is no background-task registry: a call here either finishes inside the step \
              or is killed by its deadline. The parameter goes with the sentence — it is not in \
              the face table's parameter list, so it is not advertised either.",
    },
    Refusal {
        text: "- Interactive flags (`-i`, e.g. `git rebase -i`, `git add -i`) are not supported in \
               this environment.\n",
        why: "a claim about an environment that does not exist here: `exec` runs whatever it is \
              given, and an interactive command simply reads from a closed stdin.",
    },
    Refusal {
        text: "- End git commit messages with:\nCo-Authored-By: Claude <noreply@anthropic.com>\n",
        why: "Claude Code's own attribution. It is not this harness's, and copying it would have \
              the model sign Yu's commits with another product's byline.",
    },
    Refusal {
        text: "- End PR bodies with:\n🤖 Generated with [Claude Code](https://claude.com/claude-code)",
        why: "same: another product's mark, in text this harness would be putting in its model's \
              mouth.",
    },
];

fn face_named(name: &str) -> Option<&'static Face> {
    FACES.iter().find(|face| face.wire == name)
}

/// The captured description, minus every sentence this harness refuses.
///
/// A refusal whose text is not in the snapshot is an error rather than a no-op.
/// That is the whole point of keeping the refusals as text: a refreshed profile
/// that reworded one of these sentences would otherwise load cleanly, with the
/// false claim still in it and nothing anywhere to say so.
///
/// Nothing else is touched: each refusal carries the separator that belongs to
/// the line it removes, so the sentences that stay are joined exactly as their
/// author joined them. The one repair is at the very end, where the separator
/// belongs to the *preceding* line and would otherwise be left dangling.
fn adopt(profile: &str, face: &Face, captured: &str) -> Result<String, ProfileError> {
    let mut text = captured.to_owned();
    for refusal in face.refuses {
        if !text.contains(refusal.text) {
            return Err(ProfileError::Unrefused {
                profile: profile.to_owned(),
                wire: face.wire,
                text: refusal.text.to_owned(),
                why: refusal.why.to_owned(),
            });
        }
        text = text.replace(refusal.text, "");
    }
    Ok(text.trim_end().to_owned())
}

/// The captured schema, minus every parameter this harness does not read.
///
/// Derived from the face's parameter list rather than hand-listed, so an
/// implementation that learns to read a new parameter starts advertising it the
/// moment the table says so — and a parameter the table does not know about
/// disappears from the wire instead of becoming a promise nobody keeps.
///
/// The other direction is checked here too: every parameter the implementation
/// reads must exist in the snapshot. A missing one means the face is being
/// described from memory, which is the failure the whole capture exists to
/// prevent.
fn backed_schema(
    profile: &str,
    face: &Face,
    implementation: &str,
    schema: &Value,
) -> Result<Value, ProfileError> {
    let mut shaped = schema.clone();
    let Some(object) = shaped.as_object_mut() else {
        return Err(ProfileError::Shape {
            path: profile.into(),
            problem: format!("`{}`'s input_schema is not a JSON object", face.wire),
        });
    };
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        return Err(ProfileError::Shape {
            path: profile.into(),
            problem: format!("`{}`'s input_schema declares no properties", face.wire),
        });
    };
    for (wire, _) in face.params {
        if !properties.contains_key(*wire) {
            return Err(ProfileError::NoSuchParameter {
                profile: profile.to_owned(),
                wire: face.wire,
                parameter: wire,
                implementation: implementation.to_owned(),
            });
        }
    }
    let backed: Vec<&str> = face.params.iter().map(|(wire, _)| *wire).collect();
    let kept: Map<String, Value> = properties
        .iter()
        .filter(|(name, _)| backed.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    object.insert("properties".to_owned(), Value::Object(kept));
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        // A required name whose property is gone would make the schema
        // unsatisfiable — and `additionalProperties: false` means the model
        // could not satisfy it by guessing either.
        required.retain(|name| {
            name.as_str().map(|name| backed.contains(&name)).unwrap_or(false)
        });
    }
    Ok(shaped)
}

/// One implementation wearing one face: the shape that will go on the wire,
/// resolved once at construction so that rendering it cannot fail later.
struct Shaped {
    face: &'static Face,
    definition: CustomTool,
}

/// Bind one face to the snapshot and to the implementation that will wear it.
///
/// Done once, at construction, and it can fail — which is the point. Every
/// failure here is "the snapshot does not say what this implementation needs it
/// to say", and the alternative to failing is a request that describes a tool
/// nobody has.
fn shaped(
    profile: &ToolProfile,
    face: &'static Face,
    implementation: &str,
) -> Result<Shaped, ProfileError> {
    let entry = profile
        .entry(face.wire)
        .ok_or_else(|| ProfileError::NoSuchTool {
            profile: profile.name().to_owned(),
            wire: face.wire,
            implementation: implementation.to_owned(),
        })?;
    let input_schema = backed_schema(profile.name(), face, implementation, &entry.input_schema)?;
    let mut definition = CustomTool::new(entry.name.clone(), String::new(), input_schema);
    definition.description = match entry.description.as_deref() {
        Some(captured) => Some(adopt(profile.name(), face, captured)?),
        None => None,
    };
    Ok(Shaped { face, definition })
}

/// What a profile-bound registry is allowed to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Everything the profile names and this crate can back.
    Everything,
    /// The readers only, as in [`ToolRegistry::read_only`].
    Readers,
}

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

    /// One file, or a window of lines out of it.
    ///
    /// Two things happen here that the capture decided, and they are one
    /// decision rather than two:
    ///
    /// * **`offset` / `limit` are line numbers and line counts**, as the captured
    ///   schema's own text says. `offset` is 1-based — the number a caller can
    ///   read off the output — and `0` is accepted as "from the beginning"
    ///   because the captured schema allows 0 and a schema that allows a value
    ///   cannot make it an error.
    /// * **Under a profiled face the lines come back numbered**, `{n}\t{line}`,
    ///   which is what the capture's own `tool_result` shows (`0003.body.bin`:
    ///   `"1\tThe blue crumb is under the third stone.\n2\t"`). Numbering is what
    ///   makes an offset usable: an unnumbered slice of a file tells the model
    ///   nothing about where in the file it is looking, and `Edit`'s captured
    ///   instruction — strip the line prefix before matching — only parses if the
    ///   prefix is there.
    ///
    /// Un-profiled, the whole file comes back exactly as it is on disk, byte for
    /// byte, which is what every request this harness has ever sent carried. The
    /// window joins lines with `\n`; a trailing newline is a line boundary, not a
    /// line, so a window that ends at end-of-file loses nothing.
    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let path = match required_str(self.name(), input, "path") {
            Ok(path) => path,
            Err(why) => return failed(self.name(), input, ResultKind::File, why),
        };
        let window = match Window::read(self.name(), input) {
            Ok(window) => window,
            Err(why) => return failed(self.name(), input, ResultKind::File, why),
        };
        let resolved = ctx.resolve(path);
        match std::fs::read(&resolved) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => {
                    let rendered = window.render(&text, ctx.is_aligned());
                    text_result(self.name(), input, ResultKind::File, rendered, true, true)
                }
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

/// Which lines of a file a read asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    /// 1-based, and never 0: `0` is the first line, which is line 1.
    from: usize,
    /// How many lines, when the caller said.
    lines: Option<usize>,
}

impl Window {
    fn read(tool: &str, input: &Value) -> Result<Self, String> {
        let offset = whole_number(tool, input, "offset")?.unwrap_or(1);
        let lines = whole_number(tool, input, "limit")?;
        if let Some(0) = lines {
            return Err(format!(
                "`{tool}` requires `limit` to be at least 1 line; 0 lines is not a read, and \
                 returning nothing for it would look like an empty file"
            ));
        }
        Ok(Self {
            from: offset.max(1),
            lines,
        })
    }

    /// The requested lines, numbered when the call wore a profiled face.
    ///
    /// A window past the end of the file is empty, and an empty answer is a true
    /// answer — there are no such lines — so it is not an error. Nothing failed.
    fn render(&self, text: &str, numbered: bool) -> String {
        if !numbered && self.lines.is_none() && self.from <= 1 {
            // The un-profiled whole-file read, unchanged: the file's own bytes.
            return text.to_owned();
        }
        let all: Vec<&str> = text.split('\n').collect();
        let last = match self.lines {
            Some(lines) => self.from.saturating_sub(1).saturating_add(lines),
            None => all.len(),
        };
        let start = (self.from - 1).min(all.len());
        let end = last.min(all.len());
        let selected = &all[start..end.max(start)];
        if !numbered {
            return selected.join("\n");
        }
        selected
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}\t{line}", start + index + 1))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A whole number from the input, when the caller sent one.
///
/// A fractional or negative line count is a request this harness cannot honour,
/// and saying so is better than rounding it: the captured schema declares
/// integers, so a non-integer means the caller and the schema disagree about
/// what the parameter is.
fn whole_number(tool: &str, input: &Value, key: &str) -> Result<Option<usize>, String> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) => Ok(Some(number as usize)),
            None => Err(format!(
                "`{tool}` requires `{key}` to be a whole number of lines, and got {value}"
            )),
        },
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

impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    /// The built-in shape, for a registry that carries `edit` without a profile
    /// — which nothing in this crate does today (see [`ToolRegistry::native`]).
    /// It is written truthfully anyway, because a definition that only exists to
    /// be overridden is how a fallback starts lying.
    ///
    /// The names here are the implementation's own, and they are the same names
    /// Claude Code's `Edit` uses, which is the one of the four faces where the
    /// un-profiled and profiled vocabularies coincide.
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::Custom(CustomTool::new(
            self.name(),
            "Replace an exact string in a file, in place.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "File to edit, absolute or relative to the working directory."
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace; it must occur exactly once unless `replace_all` is set."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Text to replace it with. May be empty, to delete `old_string`."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence of `old_string` (default false)."
                    }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        ))
    }

    fn effect(&self, input: &Value) -> Option<EffectIntent> {
        let path = input.get("file_path").and_then(Value::as_str)?;
        let old = input.get("old_string").and_then(Value::as_str)?;
        let new = input.get("new_string").and_then(Value::as_str)?;
        let replace_all = input.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
        Some(edit_intent(path, old, new, replace_all))
    }

    /// Whether the edit's target text is in the file and what it replaced is
    /// gone.
    ///
    /// Deliberately not "is the file what the edit would have produced": the
    /// only states this tool can *see* are its two strings, and the three
    /// answers below are the three cases those two can distinguish. See
    /// [`EffectVerdict`] for why "cannot tell" is one of them rather than a
    /// guess.
    fn effect_in_place(&self, input: &Value, ctx: &ToolContext) -> EffectVerdict {
        let Some(path) = input.get("file_path").and_then(Value::as_str) else {
            return EffectVerdict::Unknown;
        };
        let Some(old) = input.get("old_string").and_then(Value::as_str) else {
            return EffectVerdict::Unknown;
        };
        let Some(new) = input.get("new_string").and_then(Value::as_str) else {
            return EffectVerdict::Unknown;
        };
        match std::fs::read(ctx.resolve(path)) {
            Ok(bytes) => {
                let Ok(text) = String::from_utf8(bytes) else {
                    return EffectVerdict::Unknown;
                };
                match (text.contains(old), text.contains(new)) {
                    // What it replaced is gone and what it wrote is there.
                    (false, true) => EffectVerdict::InPlace,
                    // The text it was to replace is still there untouched.
                    (true, false) => EffectVerdict::NotInPlace,
                    // Both, or neither: the file is in neither the state before
                    // the edit nor the state the edit describes. A substring
                    // relationship between the two strings is enough to produce
                    // this (`old = "ab"`, `new = "xab"`), so answering either way
                    // would be a claim nothing supports.
                    _ => EffectVerdict::Unknown,
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => EffectVerdict::NotInPlace,
            Err(_) => EffectVerdict::Unknown,
        }
    }

    /// Exact string replacement, with the two refusals the captured description
    /// promises: the text must match exactly, and it must be unique unless
    /// `replace_all` is set.
    ///
    /// Both refusals happen *before* anything is written, and the file is
    /// written in one call rather than truncated first — an edit that fails its
    /// own contract must leave the file exactly as it found it.
    fn execute(&self, input: &Value, ctx: &ToolContext) -> ToolResult {
        let tool = self.name();
        let path = match required_str(tool, input, "file_path") {
            Ok(path) => path,
            Err(why) => return failed(tool, input, ResultKind::Diff, why),
        };
        let old = match required_str(tool, input, "old_string") {
            Ok(old) => old,
            Err(why) => return failed(tool, input, ResultKind::Diff, why),
        };
        // `new_string` may be empty — deleting the matched text is an edit —
        // so presence and non-emptiness are asked separately here.
        let new = match input.get("new_string").and_then(Value::as_str) {
            Some(new) => new,
            None => {
                return failed(
                    tool,
                    input,
                    ResultKind::Diff,
                    format!("`{tool}` requires a string `new_string`; it may be empty, but it must be there"),
                );
            }
        };
        let replace_all = match input.get("replace_all") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(flag)) => *flag,
            Some(other) => {
                return failed(
                    tool,
                    input,
                    ResultKind::Diff,
                    format!("`{tool}` requires `replace_all` to be true or false, and got {other}"),
                );
            }
        };
        if old == new {
            return failed(
                tool,
                input,
                ResultKind::Diff,
                "`old_string` and `new_string` are the same, so this call would change nothing. \
                 The schema says they must differ.",
            );
        }
        let resolved = ctx.resolve(path);
        let text = match std::fs::read(&resolved) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    return failed(
                        tool,
                        input,
                        ResultKind::Diff,
                        format!("`{}` is not valid UTF-8, so exact string replacement cannot be done on it", resolved.display()),
                    );
                }
            },
            Err(err) => {
                return failed(
                    tool,
                    input,
                    ResultKind::Diff,
                    format!("cannot read `{}`: {err}. `edit` changes a file that exists; use `write_file` to create one", resolved.display()),
                );
            }
        };
        let replacements = text.matches(old).count();
        if replacements == 0 {
            return failed(
                tool,
                input,
                ResultKind::Diff,
                format!(
                    "the text to replace was not found in `{}`. The match is exact — whitespace \
                     and indentation included — so nothing was changed.",
                    resolved.display()
                ),
            );
        }
        if replacements > 1 && !replace_all {
            return failed(
                tool,
                input,
                ResultKind::Diff,
                format!(
                    "`{}` contains {replacements} occurrences of that text of {} bytes, and \
                     `replace_all` is false, so which one to replace is undecidable. Nothing was \
                     changed: give more surrounding text to make the match unique, or set \
                     `replace_all`.",
                    resolved.display(),
                    old.len()
                ),
            );
        }
        let replaced = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        match std::fs::write(&resolved, &replaced) {
            Ok(()) => text_result(
                tool,
                input,
                ResultKind::Diff,
                // A write, so not reproducible: re-running the call is not a way
                // to re-obtain this result, it is a way to edit twice.
                format!(
                    "edited `{}`: replaced {}{} occurrence{} of {} bytes with {} bytes",
                    resolved.display(),
                    if replace_all { "all " } else { "" },
                    if replace_all { replacements.to_string() } else { "1".to_owned() },
                    if replacements == 1 { "" } else { "s" },
                    old.len(),
                    new.len()
                ),
                false,
                true,
            ),
            Err(err) => failed(
                tool,
                input,
                ResultKind::Diff,
                format!("cannot write `{}`: {err}; nothing was changed", resolved.display()),
            ),
        }
    }
}

/// The intent behind an `edit` call, in the one place the tool and its tests
/// read it from.
///
/// The key names the place, both strings and the flag, because the effect is the
/// whole of that: the same replacement in a different file, or with
/// `replace_all` flipped, is a different effect. It is *not* certified
/// idempotent — see [`edit_intent`]'s caller in `Execute` and the doc below.
///
/// **Not idempotent**, unlike a write, and for a reason that is one character
/// deep: `new_string` may contain `old_string` (`old = "ab"`, `new = "xab"`),
/// and then performing the edit twice is not performing it once. A key that
/// certifies otherwise would let a replay duplicate work under cover of a claim.
fn edit_intent(path: &str, old: &str, new: &str, replace_all: bool) -> EffectIntent {
    let description = format!(
        "edit `{path}`: replace `{old}` with `{new}`{}",
        if replace_all { " (all occurrences)" } else { "" }
    );
    EffectIntent {
        key: format!(
            "edit:{path}#sha256:{}{}#{}",
            digest_of(old.as_bytes()),
            digest_of(new.as_bytes()),
            if replace_all { "all" } else { "one" }
        ),
        description,
        idempotent: false,
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
    ///
    /// `description`, when the model sends one, is carried into the record: it is
    /// the model's own one-line account of what the command does, which is the
    /// thing a reader of the journal after the fact most wants and the thing
    /// nothing else in the record supplies. It changes no behaviour, and that is
    /// deliberate — the captured schema requires only `command`, so a call
    /// without a description must not fail.
    fn effect(&self, input: &Value) -> Option<EffectIntent> {
        let command = input.get("command").and_then(Value::as_str)?;
        let said = input
            .get("description")
            .and_then(Value::as_str)
            .filter(|said| !said.is_empty());
        Some(EffectIntent {
            key: format!("exec:sha256:{}", digest_of(command.as_bytes())),
            description: match said {
                Some(said) => format!("run `sh -c {command}` — the model says: {said}"),
                None => format!("run `sh -c {command}`"),
            },
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
        let deadline = match deadline_for(input, ctx.is_aligned()) {
            Ok(deadline) => deadline,
            Err(why) => return failed(self.name(), input, ResultKind::Log, why),
        };
        match run_command(command, &ctx.cwd, deadline) {
            Ok(Run { status, stdout, stderr, timed_out }) => {
                let stdout = String::from_utf8_lossy(&stdout);
                let stderr = String::from_utf8_lossy(&stderr);
                let mut text = render_exec_output(command, &status, &stdout, &stderr);
                if timed_out {
                    // After the streams, never inside the header: the exit
                    // marker's position is a function of the command, and
                    // `parse_exit_code` reads that position rather than the text.
                    text.push_str(&format!(
                        "\nthe command was still running after {} ms and was killed; what it \
                         printed before that is above\n",
                        deadline.map(|d| d.as_millis()).unwrap_or(0)
                    ));
                }
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

/// The one place a command is run: `sh -c`, in `cwd`, with a deadline.
///
/// Not `Command::output()` any more, and the reason is the deadline rather than
/// the pipes: `output()` reads both streams while the child runs (so a command
/// cannot deadlock on a full pipe) but it waits forever. This keeps the reading
/// on its own threads and watches the clock on this one, so the two properties
/// hold together.
struct Run {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

fn run_command(command: &str, cwd: &Path, deadline: Option<Duration>) -> std::io::Result<Run> {
    use std::os::unix::process::CommandExt;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so that a deadline can end the *command*
        // rather than the shell it was started in. Without this, `sh -c "sleep
        // 5"` leaves `sleep` running and holding our pipes open after the shell
        // has been killed — a timeout that stops nothing, which is what the
        // first version of this did, and what its test caught.
        .process_group(0)
        .spawn()?;
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    // Both pipes are drained on their own threads, so a child that fills one
    // while this thread is polling for its exit still makes progress.
    let out_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let started = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        match deadline {
            Some(deadline) if started.elapsed() >= deadline => {
                // The group, by its id, through the shell's own `kill` builtin —
                // no dependency and no signal bindings, and POSIX guarantees the
                // builtin. Then the child is waited for: a `Child` dropped
                // without waiting leaves a zombie, and the readers would never
                // see end-of-file.
                let _ = Command::new("sh")
                    .arg("-c")
                    .arg(format!("kill -KILL -{pid}"))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                break (child.wait()?, true);
            }
            // 5 ms is the polling granularity and therefore the floor on how
            // precisely a deadline is honoured; a command that finishes inside a
            // poll interval is not killed for it.
            _ => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    Ok(Run {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        timed_out,
    })
}

/// How long a command may run, from the call and the face it arrived under.
///
/// The captured `Bash` description says two things about `timeout` — "default
/// 120000, max 600000" — and both are implemented, but only under a profiled
/// face, because they are *that face's* claims. Un-profiled there is no deadline
/// at all, which is what this harness has always done: a `cargo build` that
/// takes ten minutes is not a hanging command, and a harness that started
/// killing it would be changing the un-measured path in the name of a sentence
/// nobody had sent it. An explicitly supplied `timeout` is honoured either way,
/// and clamped to the captured maximum under a profiled face so that "max
/// 600000" stays true.
fn deadline_for(input: &Value, aligned: bool) -> Result<Option<Duration>, String> {
    const DEFAULT_MS: u64 = 120_000;
    const MAX_MS: u64 = 600_000;
    let asked = match input.get("timeout") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_f64() {
            Some(ms) if ms > 0.0 => Some(ms),
            Some(ms) => {
                return Err(format!(
                    "`exec` requires `timeout` to be a positive number of milliseconds, and got {ms}"
                ));
            }
            None => {
                return Err(format!(
                    "`exec` requires `timeout` to be a number of milliseconds, and got {value}"
                ));
            }
        },
    };
    let ms = match (asked, aligned) {
        (Some(ms), true) => ms.min(MAX_MS as f64),
        (Some(ms), false) => ms,
        (None, true) => DEFAULT_MS as f64,
        (None, false) => return Ok(None),
    };
    Ok(Some(Duration::from_millis(ms as u64)))
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
    /// wire name → tool index, built at construction.
    ///
    /// Empty when no profile is loaded, which is what keeps the un-profiled
    /// path exactly as it was: `get` falls back to the implementation's own
    /// name, and that is the only lookup that ever happens.
    wire: HashMap<String, usize>,
    /// The face each tool wears, aligned with `tools` by index. `None` is a tool
    /// with no profile behind it — its own definition is what goes on the wire.
    faces: Vec<Option<Shaped>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            cwd: PathBuf::from("."),
            reducer: None,
            wire: HashMap::new(),
            faces: Vec::new(),
        }
    }

    /// The surface a profile names, restricted to what this harness can back.
    ///
    /// This is the whole of step 3: the registry asks the snapshot what each
    /// tool is called and what it says, and asks [`FACES`] which of our
    /// implementations stands behind it. An entry with no implementation is not
    /// advertised at all (DESIGN 约束 4, 「不提供的,就不要放进去」), so a 26-tool
    /// profile becomes the four faces this crate can answer and the other 22 do
    /// not exist for this run — the model cannot call what it was never shown,
    /// and it is shown nothing that would answer with an error.
    ///
    /// The order is the snapshot's, not ours: the tool array renders in the
    /// order the profile listed, and DESIGN's cache rule wants that byte prefix
    /// stable.
    pub fn profiled(
        ctx_cwd: impl Into<PathBuf>,
        profile: Arc<ToolProfile>,
    ) -> Result<Self, ProfileError> {
        Self::from_profile(ctx_cwd, profile, Reach::Everything)
    }

    /// The same surface with everything that can change the world left out —
    /// the profiled analogue of [`Self::read_only`], and the reason [`Reach`]
    /// exists rather than a second table.
    pub fn profiled_read_only(
        ctx_cwd: impl Into<PathBuf>,
        profile: Arc<ToolProfile>,
    ) -> Result<Self, ProfileError> {
        Self::from_profile(ctx_cwd, profile, Reach::Readers)
    }

    fn from_profile(
        ctx_cwd: impl Into<PathBuf>,
        profile: Arc<ToolProfile>,
        reach: Reach,
    ) -> Result<Self, ProfileError> {
        let mut registry = Self::new();
        registry.cwd = ctx_cwd.into();
        for entry in profile.entries() {
            let Some(face) = face_named(&entry.name) else {
                continue;
            };
            if reach == Reach::Readers && face.mutates {
                continue;
            }
            let tool = (face.make)();
            let implementation = tool.name().to_owned();
            let worn = shaped(profile.as_ref(), face, &implementation)?;
            let index = registry.tools.len();
            registry.index.insert(implementation, index);
            registry.tools.push(tool);
            registry.wire.insert(face.wire.to_owned(), index);
            registry.faces.push(Some(worn));
        }
        Ok(registry)
    }

    fn shaped(&self, name: &str) -> Option<&Shaped> {
        let index = self
            .wire
            .get(name)
            .or_else(|| self.index.get(name))
            .copied()?;
        self.faces.get(index).and_then(Option::as_ref)
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
            Some(existing) => {
                self.tools[existing] = tool;
                // A face belongs to the implementation a profile bound it to.
                // Replacing that implementation replaces the thing the face was
                // measured against, so the face and its wire name go with it
                // rather than going on describing the new one.
                self.faces[existing] = None;
                self.wire.retain(|_, index| *index != existing);
            }
            None => {
                self.index.insert(name, self.tools.len());
                self.tools.push(tool);
                self.faces.push(None);
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

    /// The implementation that answers `name`.
    ///
    /// Two names resolve, and both resolve on purpose. The wire name is what a
    /// model calls (`Read`); the implementation's own name is its identity
    /// (satisfied by `read_file`) and stays dispatchable, which is what keeps a
    /// recorded re-run instruction — `read_file({"path": …})` — working in a run
    /// whose surface is aligned.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.wire
            .get(name)
            .or_else(|| self.index.get(name))
            .map(|index| &*self.tools[*index])
    }

    /// The shapes as they go on the wire: the profile's where a profile is
    /// loaded and the snapshot carries this tool, the built-in one otherwise.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .iter()
            .enumerate()
            .map(|(index, tool)| match self.faces.get(index).and_then(Option::as_ref) {
                Some(shaped) => ToolDefinition::Custom(shaped.definition.clone()),
                None => tool.definition(),
            })
            .collect()
    }

    /// The implementations' own names: the identity a result's provenance
    /// records, and the same vocabulary the record speaks no matter which face
    /// the call arrived under.
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect()
    }

    /// The names the model sees, which are the ones it will call.
    ///
    /// Anything that *describes* this surface to anybody — the run's own
    /// account of itself, the decision layer's view of its executors — has to
    /// use these rather than [`Self::names`], or it describes a surface no model
    /// was offered.
    pub fn wire_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .enumerate()
            .map(|(index, tool)| match self.faces.get(index).and_then(Option::as_ref) {
                Some(shaped) => shaped.face.wire.to_owned(),
                None => tool.name().to_owned(),
            })
            .collect()
    }

    /// One wire call's input, in the implementation's own vocabulary.
    ///
    /// The wire speaks the profile's parameter names (`file_path`) and the
    /// implementation reads its own (`path`). The translation happens once, in
    /// the loop, before anything looks at the input — effect, effect check and
    /// execution all see the same object — and it is why the record is in one
    /// vocabulary whichever face a call arrived under.
    ///
    /// Keys the face does not map are passed through untouched: `additionalProperties: false`
    /// means a conforming model sends none, and dropping what a non-conforming
    /// one sent would hide it from the tool that could say so.
    pub fn input_for(&self, wire_name: &str, input: &Value) -> Value {
        let Some(shaped) = self.shaped(wire_name) else {
            return input.clone();
        };
        let Some(object) = input.as_object() else {
            return input.clone();
        };
        let mut translated = object.clone();
        for (wire, implementation) in shaped.face.params {
            if let Some(value) = object.get(*wire) {
                translated.remove(*wire);
                translated.insert((*implementation).to_owned(), value.clone());
            }
        }
        Value::Object(translated)
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

    /// A context for one wire call, carrying the face that call arrived under.
    ///
    /// Per call rather than per run because the face is per call: two calls in
    /// one batch can arrive under different names, and an implementation that
    /// answers in the shape a face implies has to know which one it is answering.
    pub fn context_for(&self, wire_name: &str) -> ToolContext {
        let mut context = self.context();
        if let Some(shaped) = self.shaped(wire_name) {
            context.face = Some(shaped.face.wire.to_owned());
        }
        context
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

    /// The two surfaces that were pinned before any of this existed still carry
    /// exactly what they carried. `edit` is not one of their tools: it arrives
    /// with a profile, and `the_built_in_tool_shapes_are_unchanged` is the test
    /// that says so in bytes.
    #[test]
    fn edit_is_not_on_an_unprofiled_surface() {
        for registry in [ToolRegistry::native("/tmp"), ToolRegistry::read_only("/tmp")] {
            assert!(
                registry.get("edit").is_none(),
                "an un-profiled surface advertises {:?}, and `edit` has no measured face of its \
                 own to wear there",
                registry.names()
            );
        }
    }

    /// A context carries the face of the call it was built for, and an
    /// un-profiled context carries none.
    #[test]
    fn only_a_profiled_call_wears_a_face() {
        let plain = ToolRegistry::native("/tmp");
        assert!(plain.context_for("read_file").face().is_none());
        assert!(!plain.context_for("read_file").is_aligned());

        let profile = crate::profile::ToolProfile::from_path(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../captures/claude-code/profile/claude-code-2.1.248.json"
        ))
        .expect("the committed capture loads");
        let registry = ToolRegistry::profiled("/tmp", Arc::new(profile)).expect("every face binds");
        assert_eq!(registry.context_for("Read").face(), Some("Read"));
        assert_eq!(
            registry.context_for("read_file").face(),
            Some("Read"),
            "the implementation's own name is the same call, so it wears the same face"
        );
        assert_eq!(registry.context_for("Bash").face(), Some("Bash"));
        assert_eq!(registry.wire_names(), vec!["Bash", "Edit", "Read", "Write"]);
        assert_eq!(registry.names(), vec!["exec", "edit", "read_file", "write_file"]);
    }

    /// The captured `Bash` says "default 120000, max 600000", and both numbers
    /// are enforced — but only under a face that says them.
    #[test]
    fn the_deadline_follows_the_face_that_declares_one() {
        let none = json!({"command": "true"});
        let asked = json!({"command": "true", "timeout": 200});
        let greedy = json!({"command": "true", "timeout": 9_000_000});

        assert_eq!(deadline_for(&none, false), Ok(None));
        assert_eq!(
            deadline_for(&asked, false),
            Ok(Some(Duration::from_millis(200))),
            "an explicit timeout is honoured with or without a face"
        );
        assert_eq!(
            deadline_for(&none, true),
            Ok(Some(Duration::from_millis(120_000))),
            "the profiled face's default"
        );
        assert_eq!(
            deadline_for(&greedy, true),
            Ok(Some(Duration::from_millis(600_000))),
            "the profiled face's maximum: the schema property says (max 600000)"
        );
        assert_eq!(
            deadline_for(&greedy, false),
            Ok(Some(Duration::from_millis(9_000_000))),
            "nothing in our own face claims a maximum, so nothing clamps one"
        );
        assert!(deadline_for(&json!({"command": "true", "timeout": 0}), true).is_err());
        assert!(deadline_for(&json!({"command": "true", "timeout": "soon"}), true).is_err());
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
