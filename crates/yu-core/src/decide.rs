//! The two tools that write L1: `decide` and `retire`.
//!
//! ROADMAP M2: "`decide` 与 `retire` 成为真工具;L1 被真正写入". Until they existed,
//! `Ledger` "只在测试里被写过" — the decision layer had one tool, every round
//! started from an empty L1, and the model re-proposed what it had already ruled
//! out. This module is the definitions the request advertises and the handlers
//! that turn a call into a ledger transition.
//!
//! Deliberately pure: no client, no journal, no async. What happens *around* a
//! call — recording it, re-rendering the view — belongs to the loop; what the
//! call *means* belongs here, where it can be tested without an endpoint.
//!
//! [`apply`] is total. The model can send anything, so a malformed call returns
//! a report that says so; it never panics and never silently does nothing.

use serde_json::Value;

use yu_wire::types::{CustomTool, ToolDefinition};

use crate::render::{Decision, Ledger, Rejected};

/// The tool names, written once. The request and the gate read the same
/// constants; a second copy is how "advertised" and "accepted" drift apart.
pub const DECIDE: &str = "decide";
/// See [`DECIDE`].
pub const RETIRE: &str = "retire";

/// What one tool call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The text for the `tool_result`.
    pub text: String,
    /// Whether the ledger differs from before the call — the audit trail
    /// counts, so a retirement that removed nothing still reports `true` here.
    /// The loop re-renders when this is set.
    pub changed: bool,
    /// Whether the call was rejected: unreadable arguments, or a name this
    /// layer does not have. The loop should also mark the `tool_result` as an
    /// error. A well-formed call that wrote nothing — a repeat — is not
    /// malformed; it is reported, not refused.
    pub malformed: bool,
    /// What went on the ledger, **in the words it went on with**, for the record.
    ///
    /// Not the same as `text`. `text` is written for the model, which does not
    /// need its own reasoning read back to it and is being answered in a turn it
    /// paid for; `record` is written for L2, which has to be able to rebuild the
    /// state. Journaling `text` loses the reason — the first version of the
    /// wiring did exactly that, and the loss is invisible from the outcome,
    /// because L1 in memory still had it.
    ///
    /// Empty when nothing was written.
    pub record: String,
}

/// The two definitions, in a fixed order.
///
/// The order is part of the cache prefix (DESIGN fixes the render order as
/// `tools` → `system` → `messages`), so it does not depend on anything.
pub fn decision_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::Custom(CustomTool::new(
            DECIDE,
            "Record a decision and the reason for it, so it does not have to be \
             re-litigated. Both verdicts go on the ledger and stay in front of you on \
             every following turn: `accepted` as a decision that stands, `rejected` as a \
             proposal that is ruled out. Deciding the same thing twice is reported as a \
             repeat rather than recorded twice.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "what": {
                        "type": "string",
                        "description": "The decision in one line. This text is the entry's \
                                        identity: `retire` later matches it exactly, and \
                                        deciding the same `what` again is a repeat."
                    },
                    "why": {
                        "type": "string",
                        "description": "Why it holds. Recorded verbatim — a decision \
                                        without its reason gets re-litigated."
                    },
                    "verdict": {
                        "type": "string",
                        "enum": ["accepted", "rejected"],
                        "description": "`accepted`: the decision stands. `rejected`: the \
                                        proposal is ruled out and must not come back \
                                        without new information."
                    }
                },
                "required": ["what", "why", "verdict"],
                "additionalProperties": false
            }),
        )),
        ToolDefinition::Custom(CustomTool::new(
            RETIRE,
            "Remove an entry from the ledger, with the reason it is going. Retirement is \
             how state contracts — it is the one way your context shrinks, so use it when \
             an item is done rather than leaving it to accumulate. The item must match an \
             entry's text exactly; a retirement that matches nothing removes nothing and \
             says so.\n\
             It removes the entry whichever list holds it, and one of those lists is the \
             rejected one: retiring a rejection lifts the do-not-re-propose protection you \
             put there. That is the explicit route back — it is recorded, with your reason \
             — but it is not what this tool is for, and retiring a rejection by accident \
             is how a settled question gets re-opened.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "item": {
                        "type": "string",
                        "description": "The exact text of the entry to remove: a decision's \
                                        or rejected proposal's `what`, an open question, or \
                                        a roster entry."
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why it is finished. This is what makes the removal \
                                        auditable, so it is required."
                    }
                },
                "required": ["item", "reason"],
                "additionalProperties": false
            }),
        )),
    ]
}

/// Whether this module handles the call.
///
/// The loop checks this before parsing anything, so a `tool_use` for a tool
/// this layer does not have is answered with a report rather than attempted.
pub fn is_decision_tool(name: &str) -> bool {
    name == DECIDE || name == RETIRE
}

/// Apply one call to the ledger.
///
/// Total by construction: every path either writes a well-formed entry or
/// returns a report of why it did not.
pub fn apply(name: &str, input: &Value, ledger: &mut Ledger) -> Applied {
    match name {
        DECIDE => decide(input, ledger),
        RETIRE => retire(input, ledger),
        other => refuse(
            other,
            format!(
                "there is no tool named `{other}` in this layer; the tools that write \
                 the ledger are `{DECIDE}` and `{RETIRE}`"
            ),
        ),
    }
}

/// A refusal: a report the model can act on, and no write.
fn refuse(tool: &str, problem: impl std::fmt::Display) -> Applied {
    Applied {
        text: format!(
            "The `{tool}` call was rejected: {problem}. Nothing on the ledger was changed."
        ),
        changed: false,
        malformed: true,
        record: String::new(),
    }
}

/// A decision that ran into an entry already on the ledger.
///
/// DESIGN's L1 exists so the layer does not have to speak for itself: the ledger
/// is its memory of what it already ruled. A second identical entry would make
/// L1 grow with every loop of the layer's own thinking, which is the
/// accumulation the whole design exists to prevent. But the repeat is still an
/// event — the layer is looping — so it is reported rather than swallowed, and
/// the call is not refused: the layer may have new reasons.
fn repeat(what: &str, verdict: &str, same_reason: bool) -> Applied {
    let text = if same_reason {
        format!(
            "Nothing recorded: `{what}` is already on the ledger as {verdict} with the \
             same reason — this is a repeat of a decision already taken, so no second \
             entry was added."
        )
    } else {
        format!(
            "Nothing recorded: `{what}` is already on the ledger as {verdict}, and the \
             reason offered now differs from the recorded one — the recorded reason was \
             not written over. If the decision itself has changed, retire the entry \
             first, then decide again."
        )
    };
    Applied {
        text,
        changed: false,
        malformed: false,
        record: String::new(),
    }
}

/// A decision that flips an entry already on the other list.
///
/// The ledger must not say "accepted" and "must not come back" about the same
/// thing; both would render, and the view would contradict itself. The old
/// entry leaves, and the result quotes its reason, because a removal whose
/// reason vanishes is a silent forgetting — DESIGN: "任何遗忘都是一个显式事件".
fn reversal(what: &str, old_why: &str, why: &str, from: &str, to: &str) -> Applied {
    Applied {
        text: format!(
            "Reversal recorded: `{what}` was on the ledger as {from} — because {old_why} \
             — and is now {to}. The new reason is recorded with it; the earlier entry, \
             and the reason just quoted, were removed."
        ),
        changed: true,
        malformed: false,
        record: format!(
            "reversed: `{what}` was {from} (because {old_why}) and is now {to} — because {why}"
        ),
    }
}

fn recorded(what: &str, why: &str, verdict: Verdict) -> Applied {
    let text = match verdict {
        Verdict::Accepted => format!(
            "Recorded as accepted: `{what}`. It is on the ledger from now on, so it does \
             not have to be decided again."
        ),
        Verdict::Rejected => format!(
            "Recorded as rejected: `{what}`. It is on the rejected list from now on, so it \
             must not come back without new information."
        ),
    };
    let word = match verdict {
        Verdict::Accepted => "decided",
        Verdict::Rejected => "ruled out",
    };
    Applied {
        text,
        changed: true,
        malformed: false,
        record: format!("{word}: `{what}` — because {why}"),
    }
}

/// The arguments must be an object before any field can be read by name.
fn object<'a>(tool: &str, input: &'a Value) -> Result<&'a serde_json::Map<String, Value>, Applied> {
    input.as_object().ok_or_else(|| {
        refuse(
            tool,
            format!(
                "the arguments must be a JSON object, not {}",
                json_kind(input)
            ),
        )
    })
}

/// Read one required string field.
///
/// Hand-written rather than derived: serde's message for a wrong type does not
/// name the offending field, and this text is read by the model.
fn text_field<'a>(
    tool: &str,
    args: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a str, Applied> {
    match args.get(name) {
        None => Err(refuse(tool, format!("`{name}` is missing"))),
        Some(Value::String(text)) if text.trim().is_empty() => {
            Err(refuse(tool, format!("`{name}` is empty")))
        }
        Some(Value::String(text)) => Ok(text),
        Some(other) => Err(refuse(
            tool,
            format!("`{name}` must be a string, not {}", json_kind(other)),
        )),
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Accepted,
    Rejected,
}

/// The verdict selects which list the entry lands on, and the two lists mean
/// opposite things. There is no default: a missing verdict is refused rather
/// than guessed at, because a guess puts the decision on the wrong list and
/// nothing downstream can tell it was a guess.
fn verdict_field(args: &serde_json::Map<String, Value>) -> Result<Verdict, Applied> {
    match args.get("verdict") {
        None => Err(refuse(
            DECIDE,
            "`verdict` is missing; it must be \"accepted\" or \"rejected\"",
        )),
        Some(Value::String(verdict)) => match verdict.as_str() {
            "accepted" => Ok(Verdict::Accepted),
            "rejected" => Ok(Verdict::Rejected),
            _ => Err(refuse(
                DECIDE,
                format!("`verdict` is `{verdict}`, which is neither \"accepted\" nor \"rejected\""),
            )),
        },
        Some(other) => Err(refuse(
            DECIDE,
            format!(
                "`verdict` must be a string, not {} — it must be \"accepted\" or \"rejected\"",
                json_kind(other)
            ),
        )),
    }
}

fn decide(input: &Value, ledger: &mut Ledger) -> Applied {
    let args = match object(DECIDE, input) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let what = match text_field(DECIDE, args, "what") {
        Ok(value) => value,
        Err(refused) => return refused,
    };
    let why = match text_field(DECIDE, args, "why") {
        Ok(value) => value,
        Err(refused) => return refused,
    };
    let verdict = match verdict_field(args) {
        Ok(value) => value,
        Err(refused) => return refused,
    };
    let what = what.to_owned();
    let why = why.to_owned();

    // One `what`, one entry — the text is the identity, whichever list it is on.
    match verdict {
        Verdict::Accepted => {
            if let Some(existing) = ledger.decisions.iter().find(|d| d.what == what) {
                return repeat(&what, "accepted", existing.why == why);
            }
            if let Some(index) = ledger.rejected.iter().position(|r| r.what == what) {
                let old = ledger.rejected.remove(index);
                let record = reversal(&old.what, &old.why, &why, "rejected", "accepted");
                ledger.decisions.push(Decision {
                    what,
                    why,
                    at_ms: yu_observe::now_ms(),
                });
                return record;
            }
            let record = recorded(&what, &why, Verdict::Accepted);
            ledger.decisions.push(Decision {
                what,
                why,
                at_ms: yu_observe::now_ms(),
            });
            record
        }
        Verdict::Rejected => {
            if let Some(existing) = ledger.rejected.iter().find(|r| r.what == what) {
                return repeat(&what, "rejected", existing.why == why);
            }
            if let Some(index) = ledger.decisions.iter().position(|d| d.what == what) {
                let old = ledger.decisions.remove(index);
                let record = reversal(&old.what, &old.why, &why, "accepted", "rejected");
                ledger.rejected.push(Rejected { what, why });
                return record;
            }
            let record = recorded(&what, &why, Verdict::Rejected);
            ledger.rejected.push(Rejected {
                what,
                why,
            });
            record
        }
    }
}

fn retire(input: &Value, ledger: &mut Ledger) -> Applied {
    let args = match object(RETIRE, input) {
        Ok(args) => args,
        Err(refused) => return refused,
    };
    let item = match text_field(RETIRE, args, "item") {
        Ok(value) => value,
        Err(refused) => return refused,
    };
    let reason = match text_field(RETIRE, args, "reason") {
        Ok(value) => value,
        Err(refused) => return refused,
    };

    // Read the audit trail *before* retiring: `Ledger::retire` reports only how
    // many active entries it removed, and zero is two different events — the
    // item was never in state, or it had already gone. The result must say
    // which, because "retired" and "removed something" are different claims,
    // and the `removed` field is where the difference is recorded.
    let already_retired = ledger.retired.iter().filter(|r| r.item == item).count();
    let removed = ledger.retire(item, reason);
    let entries = if removed == 1 { "entry" } else { "entries" };

    let text = match (removed, already_retired) {
        (0, 0) => format!(
            "Nothing removed: no entry on the ledger matches `{item}` exactly. The \
             retirement is recorded with removed: 0 — if you meant to retire something, \
             use the entry's exact text."
        ),
        (0, _) => format!(
            "Nothing removed: `{item}` was not in state; it had already been retired. \
             This retirement is recorded with removed: 0, so the ledger does not claim \
             it shrank."
        ),
        (n, _) => format!(
            "Retired: `{item}` left active state ({n} {entries} removed). The reason is on \
             the audit trail."
        ),
    };

    Applied {
        text,
        changed: true,
        malformed: false,
        record: format!("retired: `{item}` — {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn accepted(what: &str, why: &str) -> Value {
        json!({ "what": what, "why": why, "verdict": "accepted" })
    }

    fn rejected(what: &str, why: &str) -> Value {
        json!({ "what": what, "why": why, "verdict": "rejected" })
    }

    #[test]
    fn the_two_tools_that_write_the_ledger_are_the_ones_this_module_accepts() {
        let tools = decision_tools();
        assert_eq!(
            tools.len(),
            2,
            "the ledger tools are exactly decide and retire"
        );
        assert_eq!(
            tools.iter().map(ToolDefinition::name).collect::<Vec<_>>(),
            [DECIDE, RETIRE]
        );
        for tool in &tools {
            assert!(!tool.is_server_defined(), "{}", tool.name());
            assert!(matches!(tool, ToolDefinition::Custom(_)), "{}", tool.name());
            assert!(
                is_decision_tool(tool.name()),
                "the request advertises `{}` but `apply` would not take it",
                tool.name()
            );
        }
        // The gate is the same set — no more, no less. The prompt names five
        // tools; only the two that write L1 are here.
        assert!(!is_decision_tool("delegate"));
        assert!(!is_decision_tool("ask_human"));
        assert!(!is_decision_tool("spot_check"));
        assert!(!is_decision_tool("Decide"), "names are exact");
        assert!(!is_decision_tool(""));
    }

    #[test]
    fn both_schemas_are_object_schemas_with_exactly_the_fields_the_handler_reads() {
        // docs/WIRE-FACTS.md: "There is no schema-less fallback here; every tool
        // is `input_schema`-defined." A definition without its schema is a 422,
        // and a schema whose fields drift from the handler's is a call that is
        // advertised and then refused.
        let tools = decision_tools();
        let fields = |name: &str| -> (ToolDefinition, Vec<String>) {
            let tool = tools
                .iter()
                .find(|t| t.name() == name)
                .unwrap_or_else(|| panic!("`{name}` must be defined"))
                .clone();
            let ToolDefinition::Custom(custom) = &tool else {
                panic!("`{name}` must be client-defined")
            };
            assert_eq!(custom.input_schema["type"], "object", "{name}");
            assert!(
                custom.description.as_deref().is_some_and(|d| !d.is_empty()),
                "{name} must carry a description; it is prompt surface"
            );
            let properties = custom.input_schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("`{name}` must have properties"));
            let mut names: Vec<String> = properties.keys().cloned().collect();
            names.sort();
            let required = custom.input_schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("`{name}` must require its fields"));
            for property in properties.keys() {
                assert!(
                    required.iter().any(|r| r.as_str() == Some(property)),
                    "`{name}.{property}` is readable but not required"
                );
            }
            assert_eq!(required.len(), properties.len(), "{name}");
            assert_eq!(custom.input_schema["additionalProperties"], false, "{name}");
            (tool, names)
        };

        let (decide, decide_fields) = fields(DECIDE);
        assert_eq!(decide_fields, ["verdict", "what", "why"]);
        let ToolDefinition::Custom(decide) = decide else {
            unreachable!()
        };
        assert_eq!(
            decide.input_schema["properties"]["verdict"]["enum"],
            json!(["accepted", "rejected"])
        );
        assert_eq!(decide.input_schema["properties"]["what"]["type"], "string");
        assert_eq!(decide.input_schema["properties"]["why"]["type"], "string");

        let (_, retire_fields) = fields(RETIRE);
        assert_eq!(retire_fields, ["item", "reason"]);
    }

    #[test]
    fn an_accepted_decision_lands_on_the_decisions_list_and_nowhere_else() {
        let mut ledger = Ledger::default();
        let applied = apply(
            DECIDE,
            &accepted(
                "keep the ledger transitions pure",
                "a transition that needs an endpoint to test is one nobody tests",
            ),
            &mut ledger,
        );

        assert!(!applied.malformed, "{}", applied.text);
        assert!(applied.changed);
        assert!(applied.text.contains("accepted"), "{}", applied.text);
        assert_eq!(ledger.decisions.len(), 1);
        assert_eq!(ledger.decisions[0].what, "keep the ledger transitions pure");
        assert_eq!(
            ledger.decisions[0].why,
            "a transition that needs an endpoint to test is one nobody tests"
        );
        assert!(
            ledger.decisions[0].at_ms > 0,
            "a decision carries when it was taken"
        );
        assert!(ledger.rejected.is_empty(), "accepted is not rejected");
    }

    #[test]
    fn a_rejected_proposal_lands_on_the_rejected_list_so_it_cannot_come_back() {
        // DESIGN: "Without it the model re-proposes what has already been
        // rejected, and the human pays to reject it twice."
        let mut ledger = Ledger::default();
        let applied = apply(
            DECIDE,
            &rejected(
                "inline the executor's raw output into the view",
                "it is exactly the raw material the decision layer exists to not read",
            ),
            &mut ledger,
        );

        assert!(!applied.malformed, "{}", applied.text);
        assert!(applied.changed);
        assert_eq!(ledger.rejected.len(), 1);
        assert_eq!(
            ledger.rejected[0].what,
            "inline the executor's raw output into the view"
        );
        assert_eq!(
            ledger.rejected[0].why,
            "it is exactly the raw material the decision layer exists to not read"
        );
        assert!(ledger.decisions.is_empty(), "rejected is not accepted");
    }

    #[test]
    fn deciding_the_same_thing_twice_reports_the_repeat_rather_than_duplicating_it() {
        let mut ledger = Ledger::default();
        let call = accepted("ship the fix", "it is the only thing between us and green");
        let first = apply(DECIDE, &call, &mut ledger);
        let again = apply(DECIDE, &call, &mut ledger);

        assert!(first.changed);
        assert_eq!(
            ledger.decisions.len(),
            1,
            "a repeated decision is not a second decision"
        );
        assert!(
            !again.malformed,
            "a repeat is an event, not an error: {}",
            again.text
        );
        assert!(!again.changed, "nothing was written");
        assert!(again.text.contains("already"), "{}", again.text);
        assert!(again.text.contains("repeat"), "{}", again.text);
    }

    #[test]
    fn a_repeat_with_a_new_reason_neither_duplicates_nor_overwrites_the_record() {
        let mut ledger = Ledger::default();
        apply(
            DECIDE,
            &accepted("ship the fix", "it unblocks the metric"),
            &mut ledger,
        );
        let again = apply(
            DECIDE,
            &accepted("ship the fix", "the customer is waiting on this today"),
            &mut ledger,
        );

        assert_eq!(ledger.decisions.len(), 1);
        assert_eq!(
            ledger.decisions[0].why, "it unblocks the metric",
            "a recorded reason is not silently replaced"
        );
        assert!(!again.malformed);
        assert!(!again.changed);
        assert!(again.text.contains("differs"), "{}", again.text);
        assert!(
            again.text.contains("retire"),
            "the report must name the explicit way to change a decision: {}",
            again.text
        );
    }

    #[test]
    fn reversing_a_decision_moves_it_instead_of_leaving_both_claims_on_the_ledger() {
        // A `what` that is accepted and rejected at once renders as two
        // contradictory statements in the same view, which is worse than either
        // one alone.
        let mut ledger = Ledger::default();
        apply(
            DECIDE,
            &accepted("use a shared cache", "it removes the second read"),
            &mut ledger,
        );
        let flipped = apply(
            DECIDE,
            &rejected(
                "use a shared cache",
                "it makes two runs see each other's state",
            ),
            &mut ledger,
        );

        assert!(!flipped.malformed, "{}", flipped.text);
        assert!(flipped.changed);
        assert!(ledger.decisions.is_empty(), "the accepted entry must leave");
        assert_eq!(ledger.rejected.len(), 1);
        assert_eq!(ledger.rejected[0].what, "use a shared cache");
        assert!(flipped.text.contains("Reversal"), "{}", flipped.text);
        assert!(
            flipped.text.contains("it removes the second read"),
            "the reason that is leaving must be stated, not dropped: {}",
            flipped.text
        );

        // And the other direction, so the pair is pinned.
        let mut ledger = Ledger::default();
        apply(
            DECIDE,
            &rejected("use a shared cache", "it makes two runs see each other"),
            &mut ledger,
        );
        let flipped = apply(
            DECIDE,
            &accepted("use a shared cache", "the runs are now isolated"),
            &mut ledger,
        );
        assert!(ledger.rejected.is_empty(), "the rejected entry must leave");
        assert_eq!(ledger.decisions.len(), 1);
        assert!(flipped.text.contains("Reversal"), "{}", flipped.text);
    }

    #[test]
    fn retiring_something_present_removes_it_from_state_and_says_how_much_left() {
        let mut ledger = Ledger::default();
        ledger.decisions.push(Decision {
            what: "ship the small fix".into(),
            why: "it unblocks the metric".into(),
            at_ms: 1,
        });

        let applied = apply(
            RETIRE,
            &json!({ "item": "ship the small fix", "reason": "merged; cargo test is green" }),
            &mut ledger,
        );

        assert!(!applied.malformed, "{}", applied.text);
        assert!(applied.changed);
        assert!(ledger.decisions.is_empty(), "the item leaves active state");
        assert!(!ledger.holds("ship the small fix"));
        assert_eq!(ledger.retired.len(), 1);
        assert_eq!(ledger.retired[0].removed, 1);
        assert_eq!(ledger.retired[0].reason, "merged; cargo test is green");
        assert!(applied.text.contains("Retired"), "{}", applied.text);
        assert!(applied.text.contains('1'), "{}", applied.text);
    }

    #[test]
    fn retiring_something_that_was_never_there_says_so_rather_than_claiming_it_was_removed() {
        // "retired" and "removed something" are different claims. A result that
        // says the first when the second did not happen is the dishonesty this
        // project is built to avoid.
        let mut ledger = Ledger::default();
        let applied = apply(
            RETIRE,
            &json!({ "item": "an item that was never on the ledger", "reason": "done" }),
            &mut ledger,
        );

        assert!(
            !applied.malformed,
            "a well-formed retirement is not an error: {}",
            applied.text
        );
        assert_eq!(ledger.active_len(), 0);
        assert_eq!(
            ledger.retired[0].removed, 0,
            "the attempt is recorded honestly"
        );
        assert!(
            applied.text.starts_with("Nothing removed"),
            "the result must not read as done: {}",
            applied.text
        );
        assert!(applied.text.contains("removed: 0"), "{}", applied.text);
        assert!(
            applied.text.contains("exactly"),
            "a mismatch must point at the exact text: {}",
            applied.text
        );
    }

    #[test]
    fn retiring_something_already_retired_says_so_rather_than_repeating_the_never_found_wording() {
        let mut ledger = Ledger::default();
        ledger.decisions.push(Decision {
            what: "ship the small fix".into(),
            why: "it unblocks the metric".into(),
            at_ms: 1,
        });
        let first = apply(
            RETIRE,
            &json!({ "item": "ship the small fix", "reason": "merged" }),
            &mut ledger,
        );
        let second = apply(
            RETIRE,
            &json!({ "item": "ship the small fix", "reason": "just to be sure" }),
            &mut ledger,
        );

        assert!(first.text.starts_with("Retired"), "{}", first.text);
        assert!(
            second.text.contains("already"),
            "a second retirement of the same item must say it was already gone: {}",
            second.text
        );
        assert_ne!(
            second.text, first.text,
            "the two absences are different events and must not share wording"
        );
        assert!(!second.text.contains("exactly"), "{}", second.text);
        assert_eq!(
            ledger.retired.len(),
            2,
            "the second attempt is on the audit trail"
        );
        assert_eq!(ledger.retired[0].removed, 1);
        assert_eq!(ledger.retired[1].removed, 0);
    }

    #[test]
    fn retirement_matches_the_entry_text_exactly_and_a_near_miss_removes_nothing() {
        let mut ledger = Ledger::default();
        ledger.decisions.push(Decision {
            what: "ship the small fix".into(),
            why: "it unblocks the metric".into(),
            at_ms: 1,
        });

        let applied = apply(
            RETIRE,
            &json!({ "item": "Ship the small fix", "reason": "done" }),
            &mut ledger,
        );

        assert_eq!(
            ledger.decisions.len(),
            1,
            "a near miss must not remove the entry"
        );
        assert_eq!(ledger.retired[0].removed, 0);
        assert!(applied.text.contains("exactly"), "{}", applied.text);
    }

    #[test]
    fn a_decision_missing_a_field_is_rejected_by_name_and_writes_nothing() {
        for missing in ["what", "why", "verdict"] {
            let mut ledger = Ledger::default();
            let mut call = accepted("W", "R");
            call.as_object_mut().unwrap().remove(missing);
            let applied = apply(DECIDE, &call, &mut ledger);

            assert!(applied.malformed, "missing `{missing}`: {}", applied.text);
            assert!(!applied.changed);
            assert!(
                applied.text.contains(missing),
                "the refusal must name the field: {}",
                applied.text
            );
            assert_eq!(ledger, Ledger::default(), "a rejected call leaves no trace");
        }
    }

    #[test]
    fn an_unknown_verdict_is_rejected_rather_than_guessed() {
        // The verdict picks the list, and the lists mean opposite things, so a
        // default would be a guess about which one the layer meant — and the
        // schema does not mark the field optional, so nothing here defaults it.
        for verdict in [json!("maybe"), json!("Accepted"), json!(true), json!(null)] {
            let mut ledger = Ledger::default();
            let applied = apply(
                DECIDE,
                &json!({ "what": "W", "why": "R", "verdict": verdict }),
                &mut ledger,
            );

            assert!(applied.malformed, "verdict {verdict}: {}", applied.text);
            assert!(!applied.changed);
            assert!(
                applied.text.contains("accepted") && applied.text.contains("rejected"),
                "the refusal must state the two valid values: {}",
                applied.text
            );
            assert_eq!(ledger, Ledger::default());
        }
    }

    #[test]
    fn a_wrong_type_or_a_non_object_argument_is_rejected_and_writes_nothing() {
        let cases = [
            json!({ "what": 5, "why": "R", "verdict": "accepted" }),
            json!({ "what": "W", "why": ["R"], "verdict": "accepted" }),
            json!(["not", "an", "object"]),
            json!(null),
            json!("decide"),
        ];
        for call in cases {
            let mut ledger = Ledger::default();
            let applied = apply(DECIDE, &call, &mut ledger);
            assert!(applied.malformed, "{call}: {}", applied.text);
            assert!(!applied.changed);
            assert!(applied.text.contains("rejected:"), "{}", applied.text);
            assert_eq!(ledger, Ledger::default());
        }
    }

    #[test]
    fn an_empty_or_blank_field_is_rejected_rather_than_recorded_as_state() {
        // A decision with an empty `what` is an entry `retire` can never
        // usefully match; one with an empty `why` renders as "X — because ",
        // which is the kind of hollow state the ledger exists to prevent.
        let mut ledger = Ledger::default();
        let applied = apply(DECIDE, &accepted("   ", "R"), &mut ledger);
        assert!(applied.malformed, "{}", applied.text);
        assert!(applied.text.contains("empty"), "{}", applied.text);
        assert_eq!(ledger, Ledger::default());

        let mut ledger = Ledger::default();
        let applied = apply(RETIRE, &json!({ "item": "X", "reason": "\t" }), &mut ledger);
        assert!(applied.malformed, "{}", applied.text);
        assert!(applied.text.contains("empty"), "{}", applied.text);
        assert_eq!(ledger, Ledger::default());
    }

    #[test]
    fn a_retirement_missing_a_field_or_with_a_wrong_type_is_rejected_and_writes_nothing() {
        let cases = [
            json!({ "reason": "done" }),
            json!({ "item": "X" }),
            json!({ "item": 7, "reason": "done" }),
            json!({ "item": "X", "reason": ["done"] }),
            json!(null),
        ];
        for call in cases {
            let mut ledger = Ledger::default();
            let applied = apply(RETIRE, &call, &mut ledger);
            assert!(applied.malformed, "{call}: {}", applied.text);
            assert!(!applied.changed);
            assert!(
                applied.text.contains("`retire` call was rejected"),
                "{}",
                applied.text
            );
            assert_eq!(ledger, Ledger::default());
        }
    }

    #[test]
    fn a_call_to_a_tool_this_module_does_not_have_is_reported_rather_than_ignored() {
        // The prompt advertises five tools; the ledger tools are two of them. A
        // call to any other must come back as a report — silence would read as
        // success, and the layer would believe it had decided something.
        let mut ledger = Ledger::default();
        let applied = apply("ask_human", &json!({ "question": "ship it?" }), &mut ledger);

        assert!(applied.malformed, "{}", applied.text);
        assert!(!applied.changed);
        assert!(applied.text.contains("ask_human"), "{}", applied.text);
        assert!(
            applied.text.contains(DECIDE) && applied.text.contains(RETIRE),
            "the report must name what this layer can do: {}",
            applied.text
        );
        assert_eq!(ledger, Ledger::default());
    }

    #[test]
    fn every_call_gets_a_report_and_nothing_that_is_rejected_leaves_a_trace() {
        // `apply` is total: there is no input for which it returns nothing, and
        // none for which it panics — a panic here would take the decision layer
        // down mid-run. The model can send anything.
        let garbage = [
            json!(null),
            json!(false),
            json!(0),
            json!(1.5),
            json!("decide"),
            json!([]),
            json!({}),
            json!({ "what": null, "why": null, "verdict": null }),
            json!({ "what": ["W"], "why": { "r": 1 }, "verdict": [] }),
            json!({ "item": {}, "reason": 3 }),
            json!({ "item": "X", "reason": "done", "extra": { "deep": [1, 2] } }),
        ];
        for name in [DECIDE, RETIRE, "ask_human", ""] {
            for call in &garbage {
                let mut ledger = Ledger::default();
                let applied = apply(name, call, &mut ledger);
                assert!(
                    !applied.text.trim().is_empty(),
                    "`{name}` {call} produced no report"
                );
                if applied.malformed {
                    assert_eq!(ledger, Ledger::default(), "`{name}` {call} wrote state");
                }
            }
        }
        // And the well-formed calls are still reachable afterwards, so the
        // refusals cannot be achieving totality by refusing everything.
        let mut ledger = Ledger::default();
        assert!(!apply(DECIDE, &accepted("W", "R"), &mut ledger).malformed);
        assert!(
            !apply(
                RETIRE,
                &json!({ "item": "W", "reason": "done" }),
                &mut ledger
            )
            .malformed
        );
        assert_eq!(ledger.retired.len(), 1);
    }

    #[test]
    fn a_rejection_written_by_the_tool_is_in_the_next_view_where_it_blocks_re_proposal() {
        // The point of writing L1 is that the *next* render carries it. This is
        // an end-to-end assertion across the two modules in this crate: a
        // rejection that never reaches the view cannot stop anything from
        // coming back.
        use crate::render::{Charter, RenderInput, render};

        let mut ledger = Ledger::default();
        apply(
            DECIDE,
            &rejected(
                "rewrite the renderer as a cache",
                "it would be a second source of truth for the view",
            ),
            &mut ledger,
        );
        apply(
            DECIDE,
            &accepted("keep L1 complete", "state is summarised only by lying"),
            &mut ledger,
        );

        let view = render(RenderInput {
            prompt: "test prompt",
            charter: &Charter::default(),
            ledger: &ledger,
            tools: &decision_tools(),
            report: None,
            evidence: &[],
            shape: &[],
            omitted: &[],
        });
        let rendered: String = view
            .system
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("keep L1 complete"),
            "the decision must reach the view"
        );
        assert!(
            rendered.contains("state is summarised only by lying"),
            "the reason must reach the view"
        );
        assert!(rendered.contains("rewrite the renderer as a cache"));
        assert!(
            rendered.contains("do not re-propose"),
            "the rejection must arrive under the heading that rules it out"
        );
    }
}
