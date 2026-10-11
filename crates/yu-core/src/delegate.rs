//! `delegate` — reduction by decomposition.
//!
//! DESIGN's fourth derived principle: "**溢出靠分解,不靠压缩**" — overflow is
//! handled by decomposition, not compression, because compressing is *guessing*
//! what does not matter while completing is *knowing*.
//!
//! This module is where that becomes a mechanism rather than a slogan. The
//! decision layer never sees a tool stream, because `delegate` never returns
//! one. What it returns is:
//!
//! * the executor's own final report,
//! * **evidence** — the commands that ran and what they returned, because
//!   DESIGN makes an assertion without runnable evidence count as unverified,
//! * **shape** — the deterministic counts, so the decision layer can judge
//!   progress without reading content,
//! * and a **manifest** of everything the run recorded that is deliberately not
//!   in this view, with its size and how to reach it.
//!
//! The alternative — handing back the transcript and letting the decision layer
//! skim it — is the industry default, and it is exactly why tool machinery
//! dominates an agent's context. Here it structurally cannot.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use yu_journal::{EventBody, Journal};
use yu_observe::{ContextComposition, ShapeSignal, SpanKind, Timeline, TokenUsage};
use yu_tools::native::parse_exit_code;
use yu_wire::client::WireClient;
use yu_wire::types::{CustomTool, ToolDefinition};

use crate::agent::{Agent, AgentConfig, LoopOutcome};
use crate::render::{Evidence, NotLoadedEntry};

/// How much of a delegation's report the decision layer will accept.
///
/// Derived from measurement, not taste. Running the vertical slice with no bound
/// put a single executor's essay of ~34 000 characters into the decision layer's
/// context, making tool results **65.9%** of it — far past DESIGN's stated
/// threshold of "超过一两成" for "the premise has a problem".
///
/// The diagnosis matters more than the number. The structural reduction was
/// working: no tool stream ever reached the decision layer, only a report. What
/// was missing was the same discipline applied one level up. DESIGN handles an
/// oversized tool result with a manifest and a pointer (`溢出靠分解`); a
/// delegation's report is a tool result like any other, and was the one place
/// that rule was not applied.
pub const REPORT_BUDGET_CHARS: usize = 2500;

/// The budget when the executor landed in `other`.
///
/// Wider because a predicted result is mostly carried by its slot and an
/// unpredicted one is carried entirely by its prose: there is no slot to read,
/// so the words are the whole account. Cutting them to the width of a predicted
/// outcome would discard exactly the part that made the result worth reporting.
pub const WIDE_REPORT_BUDGET_CHARS: usize = 8000;

/// What the decision layer asks for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateArgs {
    /// What to accomplish. The executor starts fresh and knows only this.
    pub task: String,
    /// What the decision layer actually needs to know, when that differs from
    /// the task. A report that answers the task but not the question is a common
    /// and expensive failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The set of landing slots this call was launched with, when one was
    /// pre-registered. `None` means the delegation was not asked to classify
    /// itself, and nothing downstream may pretend it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<crate::landing::PlannedOutcomes>,
    /// Files the work is expected to touch.
    ///
    /// DESIGN lists "实际触碰文件 vs 预期" as a shape signal, and this is where
    /// the "expected" half comes from — nothing else produces it. Declaring it
    /// is what makes scope drift detectable rather than merely regrettable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_files: Option<Vec<String>>,
}

impl DelegateArgs {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            outcomes: None,
            task: task.into(),
            question: None,
            expected_files: None,
        }
    }
}

/// What comes back: a view, not a transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateOutcome {
    /// The executor's final reply.
    pub report: String,
    /// Commands that ran, with their exit codes where the run captured one.
    pub evidence: Vec<Evidence>,
    /// Where the executor classified this result, when it was asked to.
    ///
    /// `None` means no set was pre-registered, which is different from a landing
    /// of `other` and must not be rendered as one: "we did not ask" and "it did
    /// not fit what we asked" are different findings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<crate::landing::Landing>,
    /// Deterministic counts.
    pub shape: Vec<ShapeSignal>,
    /// What the run recorded and this view does not contain.
    pub omitted: Vec<NotLoadedEntry>,
    pub steps: u32,
    pub tool_calls: u32,
    pub usage: TokenUsage,
    /// Whether the executor reported completion rather than stopping.
    ///
    /// Derived from [`Self::stopped_early`], which is the run's own statement
    /// about itself. Read as a pair: this says whether, that says why.
    pub completed: bool,
    /// Where the executor stopped, when it stopped before finishing.
    ///
    /// Carried because a bare "did not complete" is a state, not an account:
    /// the decision layer has to be able to tell a run that ran out of steps
    /// from one that was cut off, and the run's own reason is the only truthful
    /// source. `None` for a run that completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_early: Option<String>,
    /// Where the executor's full reply is recorded, if it was.
    ///
    /// Carried so that a truncated report can point at the rest instead of
    /// merely announcing that something was cut — a truncation with no address
    /// is a loss, and DESIGN's rule is that a manifest names how to reach what
    /// it left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_address: Option<String>,
    /// What the *executor's* context was made of.
    ///
    /// Carried back so the two ratios DESIGN's acceptance criterion names can be
    /// reported side by side from one run. The decision layer's is meant to
    /// approach zero; this one is meant not to, because this is where the tool
    /// stream actually lives.
    pub executor_composition: ContextComposition,
}

impl DelegateOutcome {
    /// The text the decision layer actually sees for this delegation.
    ///
    /// Kept deliberately thin. Everything here is billed to the scarcest context
    /// in the system, so it states the outcome and the evidence and stops.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(if self.completed {
            "DELEGATION COMPLETE\n"
        } else {
            "DELEGATION STOPPED WITHOUT COMPLETING\n"
        });
        // The reason directly under the header, because "it stopped" without
        // "where" leaves the decision layer to guess between states that call
        // for different responses — re-deriving the answer from the tail of the
        // report is the work this field exists to remove.
        if let Some(stopped) = &self.stopped_early {
            out.push_str(stopped);
            out.push('\n');
        }
        out.push_str(&format!(
            "({} step(s), {} tool call(s))\n\n",
            self.steps, self.tool_calls
        ));
        // A landing of `other` widens the budget. DESIGN: "落进 `other` 时**放宽**
        // 报告:那正是要细节的时候." When the result fits a slot the caller named,
        // the slot is most of the report and the prose is a footnote; when it does
        // not, the prose is the only account of what happened, and cutting it to
        // the width of a predicted outcome throws away the thing that made it
        // worth reporting.
        let budget = match &self.landing {
            Some(landing) if landing.is_other() => WIDE_REPORT_BUDGET_CHARS,
            _ => REPORT_BUDGET_CHARS,
        };
        out.push_str(&truncate_report(
            &self.report,
            self.report_address.as_deref(),
            budget,
        ));
        out.push('\n');

        if self.evidence.is_empty() {
            // DESIGN: "没有可运行证据的断言一律视为未验证". Said plainly, because a
            // reader who has to infer this will usually infer the opposite.
            out.push_str(
                "\nEVIDENCE: none. Nothing was run, so every claim above is unverified.\n",
            );
        } else {
            out.push_str("\nEVIDENCE\n");
            for e in &self.evidence {
                match e.exit_code {
                    Some(code) => {
                        out.push_str(&format!("- [{}] exit {}\n", e.label, code));
                    }
                    None => out.push_str(&format!("- [{}] exit unknown\n", e.label)),
                }
                for line in e.content.lines().take(20) {
                    out.push_str(&format!("    {line}\n"));
                }
                let extra = e.content.lines().count().saturating_sub(20);
                if extra > 0 {
                    out.push_str(&format!("    … {extra} more line(s), see the manifest\n"));
                }
            }
        }

        if !self.shape.is_empty() {
            out.push_str("\nSHAPE\n");
            for s in &self.shape {
                out.push_str(&format!("- {}\n", describe_signal(s)));
            }
        }

        let manifest = crate::render::NotLoaded {
            entries: self.omitted.clone(),
        }
        .render();
        if !manifest.is_empty() {
            out.push('\n');
            out.push_str(&manifest);
        }
        out
    }

    /// Byte cost of this delegation in the decision layer's context.
    pub fn context_chars(&self) -> usize {
        self.render().len()
    }
}

/// Build the outcome from a finished executor run, reading the record to recover
/// the evidence and to say honestly what is not being shown.
pub async fn outcome_from_run(
    run: &LoopOutcome,
    journal: &Journal,
    declared_files: Option<&[String]>,
    planned: Option<&crate::landing::PlannedOutcomes>,
) -> Result<DelegateOutcome, yu_journal::JournalError> {
    let events = journal.events().await?;
    // The executor's own classification of its result, recovered from the record
    // rather than from the loop's memory — the same way `exec` evidence is, and
    // for the same reason: the record is the thing that survives the process.
    let mut landing_input: Option<serde_json::Value> = None;

    // Pair each `exec` call with its result to recover runnable evidence. Only
    // commands count: a file read is not evidence that something works.
    let mut evidence = Vec::new();
    let mut pending: Option<(String, String)> = None; // (tool name, command)
    let mut omitted = Vec::new();
    let mut unpaired_bytes: u64 = 0;
    // The most recent recorded reply is the one this delegation is reporting.
    let mut report_address: Option<String> = None;

    for event in &events {
        match &event.body {
            EventBody::ToolCall {
                tool_use_id,
                name,
                input,
            } => {
                if name == crate::landing::LAND {
                    // Last one wins: a second `land` is a correction, and the
                    // record keeps both so the correction is visible.
                    landing_input = Some(input.clone());
                }
                if name == "exec" {
                    let command = input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("<unparsed command>")
                        .to_owned();
                    pending = Some((tool_use_id.clone(), command));
                }
            }
            EventBody::ToolResult {
                tool_use_id,
                name,
                bytes,
                inline,
                blob,
                ..
            } => {
                if name == "exec"
                    && let Some((id, command)) = &pending
                    && id == tool_use_id
                {
                    // The producer owns the block's format, so the reader lives
                    // with it (`yu_tools::native`). The command is passed in
                    // because the block echoes it: the exit marker's position is
                    // a function of the command, which is what keeps output that
                    // merely resembles a marker from being read as one.
                    //
                    // A reduction that replaced the output with a pointer is not
                    // a render, so nothing parses there and the code is reported
                    // as genuinely unknown — never defaulted to success.
                    let text = inline.clone().unwrap_or_default();
                    let exit_code = parse_exit_code(command, &text);
                    // Where the full output is, so a block this view does not
                    // carry can still be pulled back. The same address the
                    // omitted entries below use, computed the same way: a
                    // spilled result is reachable by digest, an inline one by
                    // its position in the record.
                    let address = match (blob, inline.is_some()) {
                        (Some(digest), _) => format!("blob:{digest}"),
                        (None, true) => format!("journal:seq={}", event.seq),
                        (None, false) => format!("journal:seq={} (dropped by reduction)", event.seq),
                    };
                    evidence.push(Evidence {
                        label: command.clone(),
                        content: text,
                        exit_code,
                        address: Some(address),
                    });
                    pending = None;
                    continue;
                }
                // Everything else that was recorded is not in this view. Name it,
                // size it, and say how to reach it.
                let address = match (blob, inline.is_some()) {
                    (Some(digest), _) => format!("blob:{digest}"),
                    (None, true) => format!("journal:seq={}", event.seq),
                    (None, false) => format!("journal:seq={} (dropped by reduction)", event.seq),
                };
                unpaired_bytes += bytes;
                omitted.push(NotLoadedEntry {
                    what: format!("`{name}` result"),
                    address,
                    bytes: *bytes,
                    why: "raw tool output; not needed to judge this delegation".to_owned(),
                    how_to_load: "delegate a reader for it".to_owned(),
                });
            }
            EventBody::ModelResponse {
                thinking, text_blob, ..
            } => {
                if let Some(blob) = text_blob {
                    report_address = Some(format!("blob:{blob}"));
                }
                for record in thinking {
                    omitted.push(NotLoadedEntry {
                        what: "reasoning from an executor turn".to_owned(),
                        address: format!("blob:{}", record.text_blob),
                        // The blob's real size, not zero. A manifest that says
                        // an entry is free while the bytes sit in the store is
                        // understating what the run cost — and the entry above
                        // this one exists precisely so a reader can decide what
                        // is worth fetching, which they cannot do from a size
                        // that is always wrong in the same direction.
                        bytes: journal.blobs().size(&record.text_blob).unwrap_or(0),
                        why: "the executor's reasoning is not evidence of anything".to_owned(),
                        how_to_load: "delegate a reader for it".to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    // Scope drift is judged against what the decision layer declared, which is
    // the only source of "expected" in the system.
    let mut shape = run.signals.clone();
    if let Some(declared) = declared_files {
        let touched: Vec<String> = events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ToolCall { name, input, .. }
                    if matches!(name.as_str(), "write_file" | "read_file") =>
                {
                    input
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                }
                _ => None,
            })
            .collect();
        let undeclared: Vec<String> = touched
            .iter()
            .filter(|p| !declared.iter().any(|d| p.ends_with(d.as_str()) || d == *p))
            .cloned()
            .collect();
        if !undeclared.is_empty() {
            shape.push(ShapeSignal::ScopeDrift {
                touched: undeclared,
                declared: declared.to_vec(),
            });
        }
    }

    let _ = unpaired_bytes;

    // Whether the run finished is the run's own statement, taken from the field
    // that says so. Reading it off `stop` instead is what made it vacuously
    // true: the loop returned only on completion, so every outcome that reached
    // here was complete by construction and the "stopped" branch below was
    // unreachable prose (see `LoopOutcome::stopped_early`).
    let stopped_early = run.stopped_early;
    let landing = match (planned, landing_input) {
        (Some(planned), Some(input)) => Some(crate::landing::read_landing(
            &input,
            &planned.slots,
            &planned.id,
        )),
        // Asked, and did not answer. That is itself a finding, and it is
        // reported as an unpredicted result rather than as an absent one: the
        // report may still be prose, but nothing in it was classified, so
        // nothing in it has been checked.
        (Some(planned), None) => Some(crate::landing::Landing {
            set: planned.id.clone(),
            slot: crate::landing::OTHER.to_owned(),
            why: "the executor did not classify its result".to_owned(),
            unlisted: None,
        }),
        (None, _) => None,
    };

    Ok(DelegateOutcome {
        report: run.text.clone(),
        landing,
        evidence,
        shape,
        omitted,
        steps: run.steps,
        tool_calls: run.tool_calls,
        usage: run.usage,
        completed: stopped_early.is_none(),
        stopped_early: stopped_early.map(|stop| stop.describe()),
        executor_composition: run.composition,
        report_address,
    })
}

/// Cut a report to the budget, naming where the rest lives.
///
/// The truncation is stated rather than silent, and it carries an address when
/// one exists. DESIGN: "清单不谎称" — a manifest that says "truncated" without
/// saying where is only half honest, and the half it omits is the useful one.
fn truncate_report(report: &str, address: Option<&str>, budget: usize) -> String {
    if report.chars().count() <= budget {
        return report.to_owned();
    }
    let mut end = budget;
    while end > 0 && !report.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = report[..end].to_owned();
    out.push_str(&format!(
        "\n\n[report truncated: {end} of {} characters",
        report.chars().count()
    ));
    match address {
        Some(address) => out.push_str(&format!(
            "; the full reply is recorded at {address} — delegate a reader for it rather \
             than inlining it here]\n"
        )),
        None => out.push_str("; the full reply was not recorded, so it is gone]\n"),
    }
    out
}

fn describe_signal(s: &ShapeSignal) -> String {
    match s {
        ShapeSignal::RepeatedCall { signature, count } => {
            format!("the identical call `{signature}` was made {count} times")
        }
        ShapeSignal::ScopeDrift { touched, declared } => format!(
            "{} file(s) touched outside the {} declared — {}",
            touched.len(),
            declared.len(),
            touched.join(", ")
        ),
        ShapeSignal::GrowthWithoutArtifact { tokens_delta } => {
            format!("{tokens_delta} tokens spent with no artifact produced")
        }
        ShapeSignal::NoArtifactFor { ms } => {
            format!("no verifiable artifact for {}s", ms / 1000)
        }
    }
}

/// The `delegate` tool as the decision layer sees it.
///
/// The description is part of the prompt surface: it is where the decision layer
/// learns that a delegation returns a report and not a transcript, so it should
/// ask a question rather than expect to inspect the work.
/// The whole surface the decision layer is offered, in one place.
///
/// A function rather than a list assembled at the call site, because the prompt
/// advertises a surface and the request must carry the same one. Two places that
/// name the tools drift, and the drift is invisible in both directions: the
/// model is told about a tool it cannot call, or handed one it was never told
/// about. `decision_layer_advertises_exactly_the_tools_it_is_offered` compares
/// this list against the prompt's own.
pub fn decision_surface() -> Vec<ToolDefinition> {
    let mut surface = vec![delegate_tool()];
    // `decide` and `retire` after `delegate`, in that fixed order: the order is
    // part of the cache prefix, so it must not depend on anything.
    surface.extend(crate::decide::decision_tools());
    surface
}

pub fn delegate_tool() -> ToolDefinition {
    ToolDefinition::Custom(CustomTool::new(
        "delegate",
        "Send a fresh executor to do work, or to answer a question about work already \
         done. The executor starts with no memory of previous delegations, so state \
         everything it needs in `task`. It reports back a summary, its evidence, and a \
         list of what was recorded but not shown to you — never its raw output, so ask \
         a question rather than expecting to inspect the work.",
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "What to accomplish. The executor knows only this."
                },
                "question": {
                    "type": "string",
                    "description": "What you need to know, if that differs from the task."
                },
                "expected_files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files you expect this work to touch. Declaring them is what makes scope drift detectable."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    ))
}

/// Run one delegation.
pub async fn run_delegation(
    args: &DelegateArgs,
    client: Arc<WireClient>,
    journal: Arc<Journal>,
    tools: Arc<yu_tools::ToolRegistry>,
    timeline: Arc<Timeline>,
    cwd: PathBuf,
) -> Result<DelegateOutcome, crate::agent::LoopError> {
    // DESIGN: "验证用 fresh" — the executor starts clean. It is given the task
    // and its constraints; it does not remember the last one.
    let task = match &args.question {
        Some(q) if q != &args.task => format!("{}\n\nWhen you are done, answer this directly: {q}", args.task),
        _ => args.task.clone(),
    };

    // The executor is told the budget and the reason for it. A length limit
    // handed over as an arbitrary rule gets ignored; one that says where the
    // text is going explains itself.
    let task = format!(
        "{task}\n\n---\nYour final reply goes straight into the decision layer's context, which \
         is the scarcest resource in this system. Keep it under {REPORT_BUDGET_CHARS} characters. \
         State the outcome and the evidence for it; leave out the narrative of how you got there."
    );

    // Last, so it is the final thing read before the executor starts: the
    // classification is the report, and an instruction to fill in a form at the
    // end of a long task gets followed less often than one that arrives at the
    // end of the instructions.
    let task = match &args.outcomes {
        Some(planned) => format!(
            "{task}\n\n---\nBefore you finish, call `land` exactly once and say where this \
             result landed. You were launched with these slots: {}. Choose the one your evidence \
             supports rather than the one that sounds best, and choose `other` if none of them \
             fits — an unpredicted result is more useful to the decision layer than a wrong \
             prediction, and forcing it into the nearest slot destroys it.",
            planned
                .slots
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => task,
    };

    // The pre-registered slots reach the executor two ways, and both are needed.
    // The *tool* carries them into the request's tool list, which is the front of
    // the cacheable prefix — DESIGN: "预测集在**请求**里(前缀缓存自动命中)". The
    // *task* names them in prose because a schema `enum` says what is allowed and
    // not what each slot means, and an executor choosing between `compile_error`
    // and `test_failure` needs the difference stated rather than inferred.
    let mut config =
        AgentConfig::new(yu_observe::AgentId::new("executor"), client.default_model())
            .max_steps(16);
    if let Some(planned) = &args.outcomes {
        config = config.extra_tool(crate::landing::land_tool(
            &planned.slots,
            planned.description.as_deref(),
        ));
    }
    // DESIGN lists `spawn` among the spans, and topology is meant to be chosen at
    // run time on the strength of derivation being cheap. Neither claim is
    // checkable while the span is never emitted: there was no way to see that a
    // derivation had happened at all, let alone what it cost.
    let spawn = timeline
        .start(SpanKind::Spawn)
        .label(format!("delegate:{}", args.question.is_some()));
    let mut agent = Agent::new(client, journal.clone(), tools, timeline, config, cwd);
    let run = agent.run(&task).await;
    let spawned = spawn
        .attr("task_chars", task.len() as i64)
        .attr("ok", run.is_ok())
        .finish();
    tracing::debug!(elapsed_ms = spawned.elapsed_ms(), "spawn");
    let run = run?;
    outcome_from_run(
        &run,
        &journal,
        args.expected_files.as_deref(),
        args.outcomes.as_ref(),
    )
    .await
    .map_err(crate::agent::LoopError::Journal)
}

/// Fixtures for tests that exercise what gets derived rather than what comes
/// back. Gated on `cfg(test)` so it cannot become a production shortcut.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;

    pub fn outcome_for(task: &str) -> DelegateOutcome {
        DelegateOutcome {
            landing: None,
            report: format!("did: {task}"),
            evidence: Vec::new(),
            shape: Vec::new(),
            omitted: Vec::new(),
            steps: 1,
            tool_calls: 0,
            usage: TokenUsage::default(),
            completed: true,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::test_support::scratch_dir;

    #[test]
    fn the_delegate_tool_description_says_it_returns_a_summary_not_a_transcript() {
        // The description is prompt surface. If it stops promising a summary,
        // the decision layer will start expecting raw output and will be
        // surprised every time — which shows up as it asking for spot checks.
        let ToolDefinition::Custom(tool) = delegate_tool() else {
            panic!("delegate must be a client-defined tool");
        };
        assert_eq!(tool.name, "delegate");
        let desc = tool.description.unwrap_or_default();
        assert!(desc.contains("never its raw output"), "{desc}");
        assert!(desc.contains("no memory of previous delegations"), "{desc}");
        assert_eq!(tool.input_schema["required"], json!(["task"]));
    }

    #[test]
    fn expected_files_is_optional_but_documented_as_the_source_of_drift_detection() {
        let ToolDefinition::Custom(tool) = delegate_tool() else {
            panic!()
        };
        let props = &tool.input_schema["properties"];
        assert!(props.get("expected_files").is_some());
        assert!(
            props["expected_files"]["description"]
                .as_str()
                .unwrap()
                .contains("drift")
        );
    }

    /// The exit code comes from what the `exec` tool actually printed.
    ///
    /// The test this replaces fed the reader a hand-written
    /// `"some output\nexit 0\n"` — a shape no tool in this repository produces —
    /// so it could not fail when the producer's format changed, which is
    /// precisely what had happened: the producer wrote `exit: 3` and the reader
    /// searched for `exit `, with a space, and found nothing on any real run.
    /// Running real commands through the real tool is what makes a change to
    /// either end a failure here.
    #[test]
    fn the_exit_code_is_read_from_what_the_exec_tool_actually_printed() {
        use yu_tools::native::{Exec, Tool, ToolContext};

        let ctx = ToolContext::with_default_budget(std::env::temp_dir());
        let run = |command: &str| -> Option<i32> {
            let result = Exec.execute(&json!({"command": command}), &ctx);
            let text = result.payload.text().unwrap_or_default();
            parse_exit_code(command, text)
        };

        // A real exit status round-trips from the tool's own output.
        assert_eq!(run("printf 'six bytes'; exit 3"), Some(3));
        assert_eq!(run("true"), Some(0));

        // Output that merely resembles the marker is not the status. This
        // command exits 1 while its output ends with a line that reads exactly
        // like a success, and the tail scan this replaced reported 0 for it — a
        // passing verdict drawn from the failed command's own noise.
        assert_eq!(run("printf 'done\\nexit 0'; exit 1"), Some(1));

        // The important case: no exit status must NOT become 0.
        assert_eq!(parse_exit_code("true", ""), None);
        assert_eq!(parse_exit_code("true", "some output with no status"), None);
    }

    #[test]
    fn a_report_with_no_evidence_says_so_explicitly() {
        let outcome = DelegateOutcome {
            landing: None,
            report: "I refactored the parser and it is much cleaner now.".into(),
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 0,
            usage: TokenUsage::default(),
            completed: true,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(
            rendered.contains("unverified"),
            "an evidence-free claim must be labelled: {rendered}"
        );
        assert!(rendered.contains("DELEGATION COMPLETE"));
    }

    #[test]
    fn an_incomplete_run_is_not_reported_as_complete() {
        let outcome = DelegateOutcome {
            landing: None,
            report: "gave up".into(),
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 16,
            tool_calls: 30,
            usage: TokenUsage::default(),
            completed: false,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(rendered.contains("STOPPED WITHOUT COMPLETING"));
        assert!(!rendered.contains("DELEGATION COMPLETE"));
    }

    #[test]
    fn evidence_shows_the_command_and_its_exit_code() {
        let outcome = DelegateOutcome {
            landing: None,
            report: "tests pass".into(),
            evidence: vec![Evidence::command("cargo test", "running 12 tests\nok", 0)],
            shape: vec![],
            omitted: vec![],
            steps: 2,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(rendered.contains("[cargo test] exit 0"));
        assert!(!rendered.contains("unverified"));
    }

    fn outcome_with_report(report: String, address: Option<&str>) -> DelegateOutcome {
        DelegateOutcome {
            landing: None,
            report,
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: address.map(str::to_owned),
        }
    }

    /// The measurement that produced this bound: one unbounded delegation made
    /// tool results 65.9% of the decision layer's context.
    #[test]
    fn an_oversized_report_is_cut_and_points_at_the_rest() {
        let long = "word ".repeat(REPORT_BUDGET_CHARS);
        let outcome = outcome_with_report(long, Some("blob:abc123"));
        let rendered = outcome.render();

        assert!(rendered.contains("report truncated"), "{rendered}");
        assert!(
            rendered.contains("blob:abc123"),
            "a truncation must say where the rest is: {rendered}"
        );
        assert!(
            outcome.context_chars() < REPORT_BUDGET_CHARS * 2,
            "the bound must actually bound: {} chars",
            outcome.context_chars()
        );
    }

    #[test]
    fn a_truncation_with_no_address_says_the_text_is_gone() {
        // It must not imply the rest is retrievable when it is not. "Truncated"
        // and "truncated, and you can read the rest" are different facts, and a
        // reader who assumes the second when the first holds will ask for
        // something that does not exist.
        let long = "word ".repeat(REPORT_BUDGET_CHARS);
        let rendered = outcome_with_report(long, None).render();
        assert!(rendered.contains("report truncated"));
        assert!(rendered.contains("not recorded"), "{rendered}");
        assert!(!rendered.contains("delegate a reader"), "{rendered}");
    }

    #[test]
    fn a_report_within_budget_is_passed_through_untouched() {
        let exact = "y".repeat(REPORT_BUDGET_CHARS);
        let rendered = outcome_with_report(exact.clone(), None).render();
        assert!(!rendered.contains("truncated"));
        assert!(rendered.contains(&exact));
    }

    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Multi-byte text must not be split, or the report reaching the model is
        // corrupt rather than merely short.
        let cjk = "测".repeat(REPORT_BUDGET_CHARS * 2);
        let rendered = outcome_with_report(cjk, None).render();
        assert!(rendered.contains("report truncated"));
        // The bound is on characters, not bytes, so a multi-byte report is cut
        // at the same visible length — and the cut lands on a boundary rather
        // than panicking or producing a replacement character.
        assert!(rendered.contains("测测"));
        assert!(
            !rendered.contains('\u{FFFD}'),
            "a split character would show as U+FFFD"
        );
        let body = rendered
            .split("DELEGATION COMPLETE")
            .nth(1)
            .expect("the header is present");
        assert!(
            body.chars().count() < REPORT_BUDGET_CHARS * 2,
            "the bound counts characters, not bytes"
        );
    }

    #[test]
    fn the_executor_is_told_the_budget_and_the_reason_for_it() {
        // The reason is the part that gets followed; a bare limit reads as an
        // arbitrary rule.
        let source = include_str!("delegate.rs");
        assert!(source.contains("scarcest resource"));
        assert!(source.contains("Keep it under"));
    }

    #[test]
    fn long_evidence_is_truncated_in_the_view_but_pointed_at() {
        let long = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let outcome = DelegateOutcome {
            landing: None,
            report: "done".into(),
            evidence: vec![Evidence::command("cargo test", long, 0)],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
            stopped_early: None,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(rendered.contains("more line(s)"), "{rendered}");
        // The decision layer's context is the scarce resource: the view must not
        // expand with the executor's output.
        assert!(outcome.context_chars() < 1200, "{}", outcome.context_chars());
    }

    // -----------------------------------------------------------------------
    // Through a real run, against a real socket
    //
    // The tests above build outcomes by hand; these drive delegations, so the
    // mechanism under test is the one that runs in production rather than a
    // restatement of it.
    // -----------------------------------------------------------------------

    /// The code a command exited with survives the whole path: the tool, the
    /// record, and the evidence block the decision layer reads.
    ///
    /// The round trip in `yu-tools` proves the two ends agree on the format;
    /// this proves the number reaches the view. A reader that only worked on the
    /// tool's immediate return value would pass that one and still leave every
    /// real run reporting `exit unknown`.
    #[tokio::test]
    async fn the_real_exit_code_reaches_the_decision_layers_evidence() {
        use crate::agent::test_support::{FakeEndpoint, endless_tool_use, final_reply};
        use yu_journal::SessionId;

        let command = "printf 'six bytes'; exit 3";
        let fake = FakeEndpoint::scripted(vec![endless_tool_use(command), final_reply("done")]);
        let dir = scratch_dir("delegate-exit-code");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let tools = Arc::new(yu_tools::ToolRegistry::native(&dir));

        let outcome = run_delegation(
            &DelegateArgs::new("run the command"),
            fake.client(),
            journal,
            tools,
            Timeline::new(),
            dir.clone(),
        )
        .await
        .expect("the executor finishes on its second turn");

        assert!(outcome.completed);
        assert_eq!(outcome.steps, 2);
        assert_eq!(outcome.evidence.len(), 1, "one command ran");
        assert_eq!(outcome.evidence[0].exit_code, Some(3));

        let rendered = outcome.render();
        assert!(rendered.contains("] exit 3"), "{rendered}");
        assert!(
            !rendered.contains("exit unknown"),
            "the evidence block must conduct: {rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An executor that runs out of steps returns a report that says where it
    /// stopped, and the decision layer survives it.
    ///
    /// This is the harness's most common failure: a loop that keeps calling
    /// tools reaches its budget. It used to travel up `run_delegation`'s `?` as
    /// an `Err`, and `DecisionLayer::run` returned `Err` with it — no report, no
    /// evidence, no acceptance numbers. A stopped delegation is an outcome like
    /// any other now, and the header the decision layer reads is the one
    /// `render` writes for a run that did not finish.
    #[tokio::test]
    async fn an_executor_that_exhausts_its_steps_returns_a_report_not_an_error() {
        use crate::agent::test_support::{FakeEndpoint, endless_tool_use};
        use yu_journal::SessionId;

        let fake = FakeEndpoint::always(endless_tool_use("printf 'still working'; exit 3"));
        let dir = scratch_dir("delegate-step-ceiling");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let tools = Arc::new(yu_tools::ToolRegistry::native(&dir));

        let outcome = run_delegation(
            &DelegateArgs::new("keep going"),
            fake.client(),
            journal,
            tools,
            Timeline::new(),
            dir.clone(),
        )
        .await
        .expect("an executor that runs out of steps must not kill the decision layer");

        assert!(!outcome.completed, "a run that stopped did not complete");
        assert_eq!(outcome.steps, 16, "the delegation's own budget");
        assert!(
            !outcome.evidence.is_empty() && outcome.evidence.iter().all(|e| e.exit_code == Some(3)),
            "what ran before the budget ran out is still evidence"
        );

        let rendered = outcome.render();
        assert!(
            rendered.contains("DELEGATION STOPPED WITHOUT COMPLETING"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("DELEGATION COMPLETE\n"),
            "a stopped run must not also read as complete: {rendered}"
        );
        assert!(
            rendered.contains("step ceiling of 16"),
            "the report must say where it stopped: {rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `pause_turn` reaches the decision layer as an account, not as a death.
    ///
    /// Nothing here can serve the disposition — it presupposes a server-side
    /// tool — so it is refused, and the refusal has to say *which* stop this
    /// was. The decision layer's next move differs between "the executor ran out
    /// of budget" and "the endpoint asked for something this harness cannot
    /// serve", and a stop that reads as the wrong one buys the wrong response.
    #[tokio::test]
    async fn a_pause_turn_is_reported_to_the_decision_layer_rather_than_killing_it() {
        use crate::agent::test_support::{FakeEndpoint, paused_turn_reply};
        use yu_journal::SessionId;

        let fake = FakeEndpoint::always(paused_turn_reply());
        let dir = scratch_dir("delegate-pause-turn");
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let tools = Arc::new(yu_tools::ToolRegistry::native(&dir));

        let outcome = run_delegation(
            &DelegateArgs::new("keep going"),
            fake.client(),
            journal,
            tools,
            Timeline::new(),
            dir.clone(),
        )
        .await
        .expect("an unservable stop reason must not kill the decision layer");

        assert!(!outcome.completed);
        assert_eq!(fake.requests(), 1, "re-sending is the spin this replaced");
        let rendered = outcome.render();
        assert!(
            rendered.contains("DELEGATION STOPPED WITHOUT COMPLETING"),
            "{rendered}"
        );
        assert!(rendered.contains("pause_turn"), "{rendered}");
        assert!(
            !rendered.contains("step ceiling"),
            "the cause must not be confused with the budget: {rendered}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }    // ---- pre-registered landings, end to end -------------------------------

    /// A turn that asks to classify, with the slot the executor chose.
    fn land_reply(slot: &str, why: &str) -> String {
        serde_json::json!({
            "id": "msg-land",
            "model": "fake-model",
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "classifying", "signature": "sig"},
                {"type": "tool_use", "id": "call_00_land", "name": crate::landing::LAND,
                 "input": {"slot": slot, "why": why}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })
        .to_string()
    }

    fn planned(slots: &[&str]) -> crate::landing::PlannedOutcomes {
        crate::landing::PlannedOutcomes {
            id: "build".to_owned(),
            description: Some("A build was attempted.".to_owned()),
            slots: slots.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    async fn run_with_landing(replies: Vec<String>, outcomes: Option<crate::landing::PlannedOutcomes>, name: &str) -> DelegateOutcome {
        use crate::agent::test_support::FakeEndpoint;
        use yu_journal::SessionId;

        let fake = FakeEndpoint::scripted(replies);
        let dir = scratch_dir(name);
        let journal = Arc::new(Journal::open(&dir, SessionId::new("s")).await.unwrap());
        let tools = Arc::new(yu_tools::ToolRegistry::native(&dir));
        let mut args = DelegateArgs::new("build the thing");
        args.outcomes = outcomes;
        let outcome = run_delegation(&args, fake.client(), journal, tools, Timeline::new(), dir.clone())
            .await
            .expect("the scripted executor finishes");
        let _ = std::fs::remove_dir_all(&dir);
        outcome
    }

    /// **M3's round trip: the executor classifies, and the classification comes back.**
    ///
    /// The landing is recovered from the record rather than from the loop's
    /// return value, so this also proves the `land` call reaches L2 — a landing
    /// that existed only in memory would be invisible to everything downstream,
    /// including the harness check that reads it.
    #[tokio::test]
    async fn a_pre_registered_set_comes_back_as_the_slot_the_executor_chose() {
        use crate::agent::test_support::final_reply;

        let outcome = run_with_landing(
            vec![land_reply("compile_error", "error[E0308]"), final_reply("done")],
            Some(planned(&["pass", "compile_error"])),
            "delegate-landing",
        )
        .await;

        let landing = outcome.landing.as_ref().expect("a set was pre-registered");
        assert_eq!(landing.slot, "compile_error");
        assert_eq!(landing.why, "error[E0308]");
        assert!(!landing.is_other());
        assert_eq!(outcome.steps, 2, "land, then finish");
    }

    /// **Design: "`other` 必须存在,落进去要响."**
    #[tokio::test]
    async fn a_slot_the_set_does_not_contain_lands_in_other_with_the_word_used() {
        use crate::agent::test_support::final_reply;

        let outcome = run_with_landing(
            vec![land_reply("disk_full", "no space left"), final_reply("done")],
            Some(planned(&["pass", "compile_error"])),
            "delegate-other",
        )
        .await;

        let landing = outcome.landing.as_ref().expect("a set was pre-registered");
        assert!(landing.is_other(), "an unpredicted slot lands in `other`");
        assert_eq!(
            landing.unlisted.as_deref(),
            Some("disk_full"),
            "and the word it used survives, so a surprise can be told from a \
             set that is named wrong"
        );
    }

    /// An executor that was asked and did not answer is not the same as one that
    /// was never asked.
    #[tokio::test]
    async fn an_executor_that_was_asked_and_did_not_answer_lands_in_other_with_that_reason() {
        use crate::agent::test_support::final_reply;

        let outcome = run_with_landing(
            vec![final_reply("I finished, I think")],
            Some(planned(&["pass", "compile_error"])),
            "delegate-silent",
        )
        .await;

        let landing = outcome.landing.as_ref().expect("a set was pre-registered");
        assert!(landing.is_other());
        assert!(
            landing.why.contains("did not classify"),
            "the reason has to be the absence itself, not an empty string: {}",
            landing.why
        );
        // And the other direction: a delegation that was never given a set must
        // not report a landing at all, or "we did not ask" would render as
        // "it did not fit".
        let unasked = run_with_landing(
            vec![final_reply("done")],
            None,
            "delegate-unasked",
        )
        .await;
        assert_eq!(unasked.landing, None);
    }

    /// **"落进 `other` 时放宽报告."**
    #[tokio::test]
    async fn landing_in_other_keeps_more_of_the_report() {
        use crate::agent::test_support::final_reply;

        let long = "word ".repeat(REPORT_BUDGET_CHARS);
        let predicted = run_with_landing(
            vec![land_reply("pass", "it worked"), final_reply(&long)],
            Some(planned(&["pass"])),
            "delegate-narrow",
        )
        .await;
        let unpredicted = run_with_landing(
            vec![land_reply("something_else", "it did not fit"), final_reply(&long)],
            Some(planned(&["pass"])),
            "delegate-wide",
        )
        .await;

        assert!(!predicted.landing.as_ref().unwrap().is_other());
        assert!(unpredicted.landing.as_ref().unwrap().is_other());
        assert!(
            unpredicted.render().chars().count() > predicted.render().chars().count() * 2,
            "an unpredicted result is carried entirely by its prose, so the budget \
             has to widen: {} vs {}",
            unpredicted.render().chars().count(),
            predicted.render().chars().count()
        );
    }

}
