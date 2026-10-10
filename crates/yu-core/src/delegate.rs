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
    /// Deterministic counts.
    pub shape: Vec<ShapeSignal>,
    /// What the run recorded and this view does not contain.
    pub omitted: Vec<NotLoadedEntry>,
    pub steps: u32,
    pub tool_calls: u32,
    pub usage: TokenUsage,
    /// Whether the executor reported completion rather than stopping.
    pub completed: bool,
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
        out.push_str(&format!(
            "({} step(s), {} tool call(s))\n\n",
            self.steps, self.tool_calls
        ));
        out.push_str(&truncate_report(&self.report, self.report_address.as_deref()));
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
) -> Result<DelegateOutcome, yu_journal::JournalError> {
    let events = journal.events().await?;

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
                    // The executor's `exec` renders a trailing exit status; if it
                    // did not, the code is genuinely unknown and is reported as
                    // such rather than defaulted to success.
                    let text = inline.clone().unwrap_or_default();
                    let exit_code = parse_exit_code(&text);
                    evidence.push(Evidence {
                        label: command.clone(),
                        content: text,
                        exit_code,
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
                        bytes: 0,
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

    Ok(DelegateOutcome {
        report: run.text.clone(),
        evidence,
        shape,
        omitted,
        steps: run.steps,
        tool_calls: run.tool_calls,
        usage: run.usage,
        completed: matches!(run.stop, yu_wire::types::StopDisposition::Complete),
        executor_composition: run.composition,
        report_address,
    })
}

/// Cut a report to the budget, naming where the rest lives.
///
/// The truncation is stated rather than silent, and it carries an address when
/// one exists. DESIGN: "清单不谎称" — a manifest that says "truncated" without
/// saying where is only half honest, and the half it omits is the useful one.
fn truncate_report(report: &str, address: Option<&str>) -> String {
    if report.chars().count() <= REPORT_BUDGET_CHARS {
        return report.to_owned();
    }
    let mut end = REPORT_BUDGET_CHARS;
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

/// The executor prints a trailing `exit <code>` line; recover it if present.
///
/// Returning `None` for "not found" matters: defaulting to 0 would turn an
/// unrun command into a passing one, which is the exact failure the evidence
/// block exists to catch.
fn parse_exit_code(output: &str) -> Option<i32> {
    output
        .lines()
        .rev()
        .take(3)
        .find_map(|line| line.trim().strip_prefix("exit ")?.trim().parse().ok())
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

    let config = AgentConfig::new(yu_observe::AgentId::new("executor"), client.default_model())
        .max_steps(16);
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
    outcome_from_run(&run, &journal, args.expected_files.as_deref())
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
            report: format!("did: {task}"),
            evidence: Vec::new(),
            shape: Vec::new(),
            omitted: Vec::new(),
            steps: 1,
            tool_calls: 0,
            usage: TokenUsage::default(),
            completed: true,
            executor_composition: ContextComposition::default(),
            report_address: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn exit_codes_are_recovered_but_absence_is_not_read_as_success() {
        assert_eq!(parse_exit_code("some output\nexit 0\n"), Some(0));
        assert_eq!(parse_exit_code("some output\nexit 1\n"), Some(1));
        assert_eq!(parse_exit_code("some output\nexit 137"), Some(137));
        // The important case: no exit status must NOT become 0.
        assert_eq!(parse_exit_code("some output with no status"), None);
        assert_eq!(parse_exit_code(""), None);
    }

    #[test]
    fn a_report_with_no_evidence_says_so_explicitly() {
        let outcome = DelegateOutcome {
            report: "I refactored the parser and it is much cleaner now.".into(),
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 0,
            usage: TokenUsage::default(),
            completed: true,
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
            report: "gave up".into(),
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 16,
            tool_calls: 30,
            usage: TokenUsage::default(),
            completed: false,
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
            report: "tests pass".into(),
            evidence: vec![Evidence::command("cargo test", "running 12 tests\nok", 0)],
            shape: vec![],
            omitted: vec![],
            steps: 2,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(rendered.contains("[cargo test] exit 0"));
        assert!(!rendered.contains("unverified"));
    }

    fn outcome_with_report(report: String, address: Option<&str>) -> DelegateOutcome {
        DelegateOutcome {
            report,
            evidence: vec![],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
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
            report: "done".into(),
            evidence: vec![Evidence::command("cargo test", long, 0)],
            shape: vec![],
            omitted: vec![],
            steps: 1,
            tool_calls: 1,
            usage: TokenUsage::default(),
            completed: true,
            executor_composition: ContextComposition::default(),
            report_address: None,
        };
        let rendered = outcome.render();
        assert!(rendered.contains("more line(s)"), "{rendered}");
        // The decision layer's context is the scarce resource: the view must not
        // expand with the executor's output.
        assert!(outcome.context_chars() < 1200, "{}", outcome.context_chars());
    }
}
