//! Pre-registered landing slots: the shape of a report, fixed before the call.
//!
//! DESIGN: *"**问题不是报告太长,是报告的形状没有被限定。** 执行者不知道决策层需要
//! 什么,所以它把一切倒出来。"* — and the mechanism is that the caller **pre-registers
//! the finite set of places the call could land**, and the executor **classifies**
//! what actually happened into one of them. The report collapses from prose into
//! a choice.
//!
//! Three reasons, and the third is the one that makes the rest worth having:
//!
//! 1. A choice is the cheapest honest transmission there is — a few bytes, and
//!    hard to misreport, because it is a classification rather than a narrative.
//! 2. The set travels in the **request**, so it is in the cacheable prefix and
//!    the answer is tiny. The residue `MEASUREMENTS.md` kept finding was the
//!    delegation's arguments plus one report.
//! 3. **A landing is an assertion and the tool results are the proof**, so the
//!    two can be checked against each other — deterministically, by the harness,
//!    without the decision layer having to notice anything. That is what turns
//!    "the executor misreported" from a thing a reader might catch into a thing
//!    a machine catches.
//!
//! `other` is not optional and not a courtesy. DESIGN: *"意外 —— 没人预测到的那个
//! 结果 —— 是**最有决策价值**的信号;一个固定集合会把它挤进最接近的桶."* A set with
//! no escape hatch does not eliminate surprises, it relabels them as the nearest
//! thing somebody did predict, and the relabelling is invisible.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use yu_wire::types::{CustomTool, ToolDefinition};

/// The tool the executor classifies its own result with.
pub const LAND: &str = "land";

/// The slot every set has and no policy may declare.
///
/// Reserved rather than conventional: a policy that could declare its own
/// `other` would be able to make the escape hatch mean something narrower than
/// "we did not predict this", and the one signal DESIGN calls the most valuable
/// would be quietly redefined.
pub const OTHER: &str = "other";

/// A set, resolved and ready to hand to one delegation.
///
/// The id is what the decision layer chose; the slots are what the policy said
/// that id means. Resolved by the caller rather than looked up here, because
/// the vocabulary of a policy is not something a delegation should have to
/// reach for — and because a delegation that resolved its own set could quietly
/// use one nobody declared.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOutcomes {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub slots: Vec<String>,
}

/// What the executor said about its own result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Landing {
    /// Which set this landing was read against.
    ///
    /// Carried because a landing without its set is unreadable: `pass` means
    /// nothing until you know what the alternatives were, and a tally keyed by
    /// slot alone would merge two different sets' vocabularies.
    pub set: String,
    /// The slot it chose. Always a member of the set, or [`OTHER`].
    pub slot: String,
    /// Its one-line reason, in its own words.
    pub why: String,
    /// What it said when what it said was not in the set.
    ///
    /// Kept because "landed in `other`" and "landed in `other` after calling it
    /// `compilation_failed` when the set says `compile_error`" are different
    /// findings: the first is a surprise, the second is a set that is named
    /// wrong, and only the raw word tells them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlisted: Option<String>,
}

impl Landing {
    /// Whether this result was one the caller did not predict.
    pub fn is_other(&self) -> bool {
        self.slot == OTHER
    }
}

/// The `land` tool for one delegation, constrained to that delegation's set.
///
/// Built per call rather than registered once, because the slot names are the
/// whole content of the tool and they differ per delegation. The definition is
/// part of the request's tool list, which is the front of the cacheable prefix —
/// which is where DESIGN wants it: *"预测集在**请求**里(前缀缓存自动命中)"*.
pub fn land_tool(slots: &[String], description: Option<&str>) -> ToolDefinition {
    let mut names: Vec<String> = slots.to_vec();
    names.push(OTHER.to_owned());

    let mut about = String::from(
        "Classify what actually happened, and say why in one line. This is the whole report: \
         the decision layer reads your choice, not a narrative, so pick the slot that matches \
         your evidence rather than the one that sounds best. ",
    );
    if let Some(description) = description {
        about.push_str(description);
        about.push(' ');
    }
    about.push_str(
        "If nothing in the list fits, choose `other` and explain in `why` what happened — a \
         result nobody predicted is the most useful thing you can report, and forcing it into \
         the nearest slot destroys it. Call this once, before you finish.",
    );

    ToolDefinition::Custom(CustomTool::new(
        LAND,
        about,
        json!({
            "type": "object",
            "properties": {
                "slot": {
                    "type": "string",
                    "enum": names,
                    "description": "Where this result landed."
                },
                "why": {
                    "type": "string",
                    "description": "One line: what in your evidence puts it there."
                }
            },
            "required": ["slot", "why"],
            "additionalProperties": false
        }),
    ))
}

/// Read a `land` call.
///
/// Total, and generous on purpose. A slot the set does not contain is not a
/// malformed call — it is a result the caller did not predict, which is exactly
/// what [`OTHER`] is for, and refusing it would push the executor into picking a
/// slot it does not believe in. The raw word is kept so the difference between
/// "a surprise" and "a badly named set" stays visible downstream.
pub fn read_landing(input: &Value, slots: &[String], set: &str) -> Landing {
    let named = input
        .get("slot")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let why = input
        .get("why")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned();

    match named {
        Some(name) if name == OTHER || slots.iter().any(|s| s == &name) => Landing {
            set: set.to_owned(),
            slot: name,
            why,
            unlisted: None,
        },
        Some(name) => Landing {
            set: set.to_owned(),
            slot: OTHER.to_owned(),
            why,
            unlisted: Some(name),
        },
        // No slot at all. The schema requires one, so this is a model that did
        // not follow it; the result is still unpredicted, which is what `other`
        // says, and `why` carries whatever it did say.
        None => Landing {
            set: set.to_owned(),
            slot: OTHER.to_owned(),
            why,
            unlisted: None,
        },
    }
}

/// What the executor's own words look like when the harness answers the call.
///
/// It says what happens next rather than "ok", because the landing is an
/// assertion the harness is about to check — DESIGN: *"自洽检查由 harness 做,不由
/// 决策层做"* — and an executor that knows it is about to be checked reports
/// differently from one that thinks it is filling in a form.
pub fn acknowledgement(slot: &str) -> String {
    format!(
        "Recorded: this delegation lands on `{slot}`. The harness checks that against what \
         your commands actually did, and a landing that contradicts them is reported upward \
         as an event rather than absorbed."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots() -> Vec<String> {
        vec![
            "pass".to_owned(),
            "compile_error".to_owned(),
            "test_failure".to_owned(),
        ]
    }

    fn tool_names(tool: &ToolDefinition) -> Vec<String> {
        let ToolDefinition::Custom(c) = tool else {
            panic!("`land` is a custom tool");
        };
        c.input_schema["properties"]["slot"]["enum"]
            .as_array()
            .expect("the slot enum")
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_tool_offers_the_sets_slots_and_always_offers_other() {
        let tool = land_tool(&slots(), Some("A build was attempted."));
        let offered = tool_names(&tool);
        assert_eq!(
            offered,
            vec!["pass", "compile_error", "test_failure", "other"],
            "`other` is last and always present: without it a fixed set does not \
             remove surprises, it relabels them as the nearest prediction"
        );
        let ToolDefinition::Custom(c) = &tool else {
            unreachable!()
        };
        assert!(
            c.description.as_deref().unwrap().contains("A build was attempted."),
            "the set's own description has to reach the executor, or it is a \
             policy nobody reads"
        );
        assert_eq!(tool.name(), LAND);
    }

    #[test]
    fn a_slot_in_the_set_is_taken_at_its_word() {
        let landing = read_landing(
            &json!({"slot": "compile_error", "why": "error[E0308] on line 4"}),
            &slots(),
            "build",
        );
        assert_eq!(landing.slot, "compile_error");
        assert_eq!(landing.why, "error[E0308] on line 4");
        assert_eq!(landing.unlisted, None);
        assert!(!landing.is_other());
    }

    /// **DESIGN: "`other` 必须存在,落进去要响."**
    ///
    /// A slot nobody predicted is not an error to be refused — refusing it
    /// teaches the executor to pick a slot it does not believe in, which is the
    /// failure the escape hatch exists to prevent. It lands in `other`, and the
    /// word it used is kept so the *reason* it is in `other` survives.
    #[test]
    fn an_unpredicted_slot_lands_in_other_and_keeps_the_word_it_used() {
        let landing = read_landing(
            &json!({"slot": "disk_full", "why": "no space left on device"}),
            &slots(),
            "build",
        );
        assert_eq!(landing.slot, OTHER);
        assert!(landing.is_other());
        assert_eq!(
            landing.unlisted.as_deref(),
            Some("disk_full"),
            "`landed in other` and `landed in other after calling it disk_full` are \
             different findings — a surprise, versus a set that is named wrong"
        );
        assert_eq!(landing.why, "no space left on device");
    }

    #[test]
    fn a_call_with_no_slot_is_other_rather_than_a_refusal() {
        let landing = read_landing(&json!({"why": "it went sideways"}), &slots(), "build");
        assert_eq!(landing.slot, OTHER);
        assert_eq!(landing.unlisted, None);
        assert_eq!(landing.why, "it went sideways");
    }

    #[test]
    fn a_slot_that_is_not_a_string_is_other_rather_than_a_panic() {
        for bad in [json!({"slot": 7}), json!({"slot": null}), json!({"slot": ""}), json!({})] {
            let landing = read_landing(&bad, &slots(), "build");
            assert_eq!(
                landing.slot, OTHER,
                "a malformed slot is still an unpredicted result: {bad}"
            );
        }
    }

    #[test]
    fn the_acknowledgement_says_the_landing_will_be_checked() {
        let text = acknowledgement("pass");
        assert!(text.contains("lands on `pass`"));
        assert!(
            text.contains("checks"),
            "an executor that thinks it is filling in a form reports differently \
             from one that knows an assertion is about to be tested: {text}"
        );
    }
}
