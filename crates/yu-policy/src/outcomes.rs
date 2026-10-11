//! Loading `[[outcome_sets]]` from a policy document.
//!
//! DESIGN's constraint 0 and 「预注册落位」 together put this shape where it is:
//! the *sets* are policy data — editing one must not require recompiling Rust —
//! while the claims they are built from are the fixed vocabulary in
//! [`yu_core::outcomes`], because the harness checks landings against the record
//! and only a claim the harness can compute is checkable.
//!
//! The shape:
//!
//! ```toml
//! [[outcome_sets]]
//! id = "build_result"
//! description = "What a build-and-test delegation can come back with."
//!
//! [[outcome_sets.slots]]
//! name = "pass"
//! expects = "succeeded"
//!
//! [[outcome_sets.slots]]
//! name = "compile_error"
//! expects = "failed"
//!
//! [[outcome_sets.slots]]
//! name = "test_failure"
//! expects = "failed"
//!
//! [[outcome_sets.slots]]
//! name = "no_changes_needed"
//! expects = "nothing_run"
//! ```
//!
//! `other` never appears in a file: it is always available, appended by whoever
//! renders the set, and a policy that declares it is rejected.
//!
//! The rules, all enforced here:
//!
//! 1. set ids are unique — a set is selected by id, so a duplicate is an error
//!    rather than a last-one-wins;
//! 2. no slot may be named `other` — the reserved name would collide with the
//!    escape hatch, and the collision would silently cost the one signal DESIGN
//!    says is the most valuable;
//! 3. a set with no slots is an error — with nothing to land in, every result
//!    lands in `other` and the set is decoration;
//! 4. slot names are unique within a set — a landing names its slot;
//! 5. a missing or malformed field names the set and the slot it is in, because
//!    the text is read by whoever edits the policy and has to say *where*.
//!
//! Sets and slots are arrays of tables rather than name-keyed maps for the same
//! reason: a map would turn a duplicate into a *format* error reported by the
//! TOML parser with no idea what it duplicated, and rule 1 or 4 would be
//! unenforceable. Uniqueness is a rule this loader checks so it can name the set
//! and the slot it is about.
//!
//! The rules are checked against the parsed *document* (a [`toml::Value`])
//! rather than the typed struct, because by the time `serde` has rejected a
//! malformed value the context rule 5 needs is gone: a nested "unknown variant"
//! names a line and a column, not the set and the slot. The typed parse runs
//! after validation, so what reaches [`yu_core::outcomes`] is a set the rules
//! have already passed.

use yu_core::outcomes::{Expects, OutcomeSets};

/// The keys a set may carry. Anything else is a typo, and the typed structs'
/// `deny_unknown_fields` would agree — but only this list can say *which* set
/// the typo is in.
const SET_KEYS: [&str; 3] = ["id", "description", "slots"];

/// The keys a slot may carry, for the same reason.
const SLOT_KEYS: [&str; 2] = ["name", "expects"];

/// What a policy document got wrong about its outcome sets.
///
/// Every message names the set, and the slot where the complaint is about one.
/// A missing `id` is named by position (`#2`) because there is no name to use —
/// and a set the author cannot locate is only half-reported.
#[derive(Debug, thiserror::Error)]
pub enum OutcomeSetsError {
    #[error("`outcome_sets` must be an array of tables (`[[outcome_sets]]`), found {found}")]
    NotAnArrayOfTables { found: &'static str },

    #[error("outcome set `{set}`: {problem}")]
    MalformedSet { set: String, problem: String },

    #[error("outcome set `{set}`, slot `{slot}`: {problem}")]
    MalformedSlot {
        set: String,
        slot: String,
        problem: String,
    },

    #[error(
        "outcome set `{id}` is declared more than once; a set is selected by id, so two \
         sets with one id make the choice ambiguous"
    )]
    DuplicateSetId { id: String },

    #[error(
        "outcome set `{set}` declares a slot named `{slot}`, but `other` is reserved: it is \
         always available as the escape hatch and is appended by whoever renders the set, \
         so a declared slot would collide with the one landing that has to ring"
    )]
    ReservedSlotName { set: String, slot: String },

    #[error(
        "outcome set `{set}` declares no slots; every result would have to land in `other`, \
         which makes the set decoration"
    )]
    NoSlots { set: String },

    #[error(
        "outcome set `{set}` declares the slot `{slot}` more than once; a landing names \
         the slot it chose, so two slots with one name make the landing ambiguous"
    )]
    DuplicateSlotName { set: String, slot: String },
}

/// Check a parsed policy document's `[[outcome_sets]]` against the five rules.
///
/// A document with no `outcome_sets` key is fine and declares none — only a
/// *declared* set is held to the rules. An empty declaration is not the same
/// thing: `outcome_sets = []` declares no sets either, and is equally fine;
/// what rule 3 refuses is a set that exists but has nothing to land in.
pub fn validate_outcome_sets(document: &toml::Value) -> Result<(), OutcomeSetsError> {
    let Some(value) = document
        .as_table()
        .and_then(|root| root.get("outcome_sets"))
    else {
        return Ok(());
    };
    let sets = value.as_array().ok_or(OutcomeSetsError::NotAnArrayOfTables {
        found: kind_of(value),
    })?;

    // Rule 1's memory: the ids already seen, in declaration order.
    let mut ids: Vec<&str> = Vec::new();

    for (index, value) in sets.iter().enumerate() {
        // The set's designator until its id is known, and the text it is
        // reported by when the id itself is the problem.
        let position = format!("#{}", index + 1);
        let table = value
            .as_table()
            .ok_or_else(|| OutcomeSetsError::MalformedSet {
                set: position.clone(),
                problem: format!("a set is a table, found {}", kind_of(value)),
            })?;

        let id = match table.get("id") {
            Some(toml::Value::String(id)) if !id.is_empty() => id.as_str(),
            Some(toml::Value::String(_)) => {
                return Err(OutcomeSetsError::MalformedSet {
                    set: position,
                    problem: "`id` is empty; a set is selected by its id".to_owned(),
                });
            }
            Some(found) => {
                return Err(OutcomeSetsError::MalformedSet {
                    set: position,
                    problem: format!("`id` is a string, found {}", kind_of(found)),
                });
            }
            None => {
                return Err(OutcomeSetsError::MalformedSet {
                    set: position,
                    problem: "no `id`; a set is selected by its id".to_owned(),
                });
            }
        };

        // Rule 1.
        if ids.contains(&id) {
            return Err(OutcomeSetsError::DuplicateSetId { id: id.to_owned() });
        }
        ids.push(id);

        reject_unknown_keys(table, &SET_KEYS, id, None)?;

        if let Some(description) = table.get("description")
            && !description.is_str()
        {
            return Err(OutcomeSetsError::MalformedSet {
                set: id.to_owned(),
                problem: format!("`description` is a string, found {}", kind_of(description)),
            });
        }

        // Rule 3. A `slots` key that is present but not an array of tables is
        // reported as what it is, not as "no slots" — a wrong type and an
        // absence call for different fixes.
        let slots = match table.get("slots") {
            Some(toml::Value::Array(slots)) if !slots.is_empty() => slots,
            Some(toml::Value::Array(_)) | None => {
                return Err(OutcomeSetsError::NoSlots { set: id.to_owned() });
            }
            Some(found) => {
                return Err(OutcomeSetsError::MalformedSet {
                    set: id.to_owned(),
                    problem: format!(
                        "`slots` are `[[outcome_sets.slots]]` tables, found {}",
                        kind_of(found)
                    ),
                });
            }
        };

        // Rule 4's memory: the names already seen within this set.
        let mut names: Vec<&str> = Vec::new();

        for (index, value) in slots.iter().enumerate() {
            let position = format!("#{}", index + 1);
            let table = value
                .as_table()
                .ok_or_else(|| OutcomeSetsError::MalformedSlot {
                    set: id.to_owned(),
                    slot: position.clone(),
                    problem: format!("a slot is a table, found {}", kind_of(value)),
                })?;

            let name = match table.get("name") {
                Some(toml::Value::String(name)) if !name.is_empty() => name.as_str(),
                Some(toml::Value::String(_)) => {
                    return Err(OutcomeSetsError::MalformedSlot {
                        set: id.to_owned(),
                        slot: position,
                        problem: "`name` is empty; a landing names the slot it chose".to_owned(),
                    });
                }
                Some(found) => {
                    return Err(OutcomeSetsError::MalformedSlot {
                        set: id.to_owned(),
                        slot: position,
                        problem: format!("`name` is a string, found {}", kind_of(found)),
                    });
                }
                None => {
                    return Err(OutcomeSetsError::MalformedSlot {
                        set: id.to_owned(),
                        slot: position,
                        problem: "no `name`; a landing names the slot it chose".to_owned(),
                    });
                }
            };

            // Checked before `expects` so that `expecs = "failed"` is reported
            // as the typo it is, rather than as the missing key it also is.
            reject_unknown_keys(table, &SLOT_KEYS, id, Some(name))?;

            match table.get("expects") {
                Some(toml::Value::String(expects)) => {
                    // Rule 5 in its most common form: a claim nobody can check
                    // is refused here, where the set and slot can be named,
                    // rather than defaulted to the claim that checks nothing.
                    if Expects::from_name(expects).is_none() {
                        return Err(OutcomeSetsError::MalformedSlot {
                            set: id.to_owned(),
                            slot: name.to_owned(),
                            problem: format!(
                                "`expects = \"{expects}\"` is not a claim; expected one of {}",
                                Expects::NAMES
                                    .map(|name| format!("`{name}`"))
                                    .join(", ")
                            ),
                        });
                    }
                }
                Some(found) => {
                    return Err(OutcomeSetsError::MalformedSlot {
                        set: id.to_owned(),
                        slot: name.to_owned(),
                        problem: format!("`expects` is a name, found {}", kind_of(found)),
                    });
                }
                None => {
                    return Err(OutcomeSetsError::MalformedSlot {
                        set: id.to_owned(),
                        slot: name.to_owned(),
                        problem: "no `expects`; a slot states the claim the harness checks it \
                                  against, or `unchecked` if it states none"
                            .to_owned(),
                    });
                }
            }

            // Rule 2.
            if name == OutcomeSets::OTHER {
                return Err(OutcomeSetsError::ReservedSlotName {
                    set: id.to_owned(),
                    slot: name.to_owned(),
                });
            }

            // Rule 4.
            if names.contains(&name) {
                return Err(OutcomeSetsError::DuplicateSlotName {
                    set: id.to_owned(),
                    slot: name.to_owned(),
                });
            }
            names.push(name);
        }
    }

    Ok(())
}

/// Refuse a key the shape does not have, naming where it is.
///
/// The typed structs deny unknown fields too, but their diagnostic names the
/// line and the key only; this one names the set and the slot, which is what
/// the person editing the policy is looking at.
fn reject_unknown_keys(
    table: &toml::Table,
    allowed: &[&str],
    set: &str,
    slot: Option<&str>,
) -> Result<(), OutcomeSetsError> {
    for key in table.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        let shape = if slot.is_some() { "slot" } else { "set" };
        let problem = format!(
            "unknown key `{key}`; a {shape} has {}",
            allowed
                .iter()
                .map(|key| format!("`{key}`"))
                .collect::<Vec<_>>()
                .join(" and ")
        );
        return Err(match slot {
            Some(slot) => OutcomeSetsError::MalformedSlot {
                set: set.to_owned(),
                slot: slot.to_owned(),
                problem,
            },
            None => OutcomeSetsError::MalformedSet {
                set: set.to_owned(),
                problem,
            },
        });
    }
    Ok(())
}

/// How to say what a malformed value is, for an error read by whoever wrote it.
fn kind_of(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "a string",
        toml::Value::Integer(_) => "an integer",
        toml::Value::Float(_) => "a float",
        toml::Value::Boolean(_) => "a boolean",
        toml::Value::Datetime(_) => "a datetime",
        toml::Value::Array(_) => "an array",
        toml::Value::Table(_) => "a table",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Policy, PolicyError};

    const EXAMPLE: &str = r#"
        [[outcome_sets]]
        id = "build_result"
        description = "What a build-and-test delegation can come back with."

        [[outcome_sets.slots]]
        name = "pass"
        expects = "succeeded"

        [[outcome_sets.slots]]
        name = "compile_error"
        expects = "failed"
    "#;

    #[test]
    fn a_declared_set_loads_with_its_slots_in_declaration_order() {
        let policy = Policy::from_toml_str(EXAMPLE).expect("the example must parse");
        let set = policy
            .outcome_sets
            .get("build_result")
            .expect("declared in the example");

        assert_eq!(
            set.description.as_deref(),
            Some("What a build-and-test delegation can come back with.")
        );
        assert_eq!(
            set.slots
                .iter()
                .map(|slot| slot.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pass", "compile_error"],
            "slots keep the order the policy declares them in"
        );
        assert_eq!(set.slots[0].expects, Expects::Succeeded);
        assert_eq!(set.slots[1].expects, Expects::Failed);
        assert_eq!(policy.outcome_sets.ids(), vec!["build_result"]);
        assert!(
            set.slots.iter().all(|slot| slot.name != OutcomeSets::OTHER),
            "`other` is appended by whoever renders the set, never declared"
        );
        assert_eq!(policy.outcome_sets.get("never_declared"), None);
    }

    /// Absent and refused are different things. A policy with no `outcome_sets`
    /// declares none and loads; a set declared with no slots is the error
    /// (rule 3), because a declared-but-empty set reads as coverage and is not.
    #[test]
    fn a_policy_with_no_outcome_sets_declares_none() {
        let policy = Policy::from_toml_str("[budgets]\nmax_steps = 5\n").expect("must parse");
        assert!(policy.outcome_sets.ids().is_empty());
        assert_eq!(policy.outcome_sets.get("build_result"), None);

        let empty =
            Policy::from_toml_str("outcome_sets = []\n").expect("an empty declaration parses");
        assert!(empty.outcome_sets.ids().is_empty());
    }

    /// A declaration of the wrong shape is refused before anything reads it as
    /// a set: `outcome_sets` is an array of tables, and a value that is not one
    /// has no set to name — only the shape itself to point at.
    #[test]
    fn a_malformed_outcome_sets_value_is_refused() {
        let err = Policy::from_toml_str("outcome_sets = 3\n").expect_err("must not load");
        assert!(
            matches!(
                err,
                PolicyError::OutcomeSets(OutcomeSetsError::NotAnArrayOfTables { .. })
            ),
            "{err}"
        );
        assert!(err.to_string().contains("array of tables"), "{err}");
    }

    /// Rule 1. A set is *selected* by id; two sets with one id would make the
    /// selection ambiguous, so the loader refuses rather than letting the last
    /// declaration win.
    #[test]
    fn two_sets_with_one_id_are_an_error_not_last_one_wins() {
        let err = Policy::from_toml_str(
            r#"
            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "pass"
            expects = "succeeded"

            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "fail"
            expects = "failed"
            "#,
        )
        .expect_err("a duplicate id must not load");
        assert!(
            matches!(
                err,
                PolicyError::OutcomeSets(OutcomeSetsError::DuplicateSetId { ref id })
                    if id == "build_result"
            ),
            "{err}"
        );
    }

    /// Rule 2. DESIGN: "**`other` 必须存在,落进去要响。**" — declaring the escape
    /// hatch as a slot would collide with the one landing that has to ring, and
    /// the collision would silently cost the most valuable signal there is.
    #[test]
    fn declaring_a_slot_named_other_is_an_error() {
        let err = Policy::from_toml_str(
            r#"
            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "pass"
            expects = "succeeded"

            [[outcome_sets.slots]]
            name = "other"
            expects = "unchecked"
            "#,
        )
        .expect_err("`other` is reserved and cannot be declared");
        assert!(
            matches!(
                err,
                PolicyError::OutcomeSets(OutcomeSetsError::ReservedSlotName {
                    ref set, ref slot,
                }) if set == "build_result" && slot == "other"
            ),
            "{err}"
        );
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    /// Rule 3. A set with nothing to land in forces every result into `other`,
    /// which is a set that is decoration — both when the slots key is absent
    /// and when it is an empty array.
    #[test]
    fn a_set_with_no_slots_is_rejected_as_decoration() {
        for (name, document) in [
            (
                "no `slots` key at all",
                "[[outcome_sets]]\nid = \"build_result\"\n",
            ),
            (
                "`slots` declared empty",
                "[[outcome_sets]]\nid = \"build_result\"\nslots = []\n",
            ),
        ] {
            let err = Policy::from_toml_str(document)
                .expect_err("a set with no slots must not load");
            assert!(
                matches!(
                    err,
                    PolicyError::OutcomeSets(OutcomeSetsError::NoSlots { ref set })
                        if set == "build_result"
                ),
                "{name}: {err}"
            );
        }
    }

    /// Rule 4. A landing names the slot it chose, so two slots with one name
    /// make the landing ambiguous — and the check would have two claims to
    /// choose between for one name.
    #[test]
    fn a_slot_name_declared_twice_is_rejected() {
        let err = Policy::from_toml_str(
            r#"
            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "pass"
            expects = "succeeded"

            [[outcome_sets.slots]]
            name = "pass"
            expects = "unchecked"
            "#,
        )
        .expect_err("a duplicate slot name must not load");
        assert!(
            matches!(
                err,
                PolicyError::OutcomeSets(OutcomeSetsError::DuplicateSlotName {
                    ref set, ref slot,
                }) if set == "build_result" && slot == "pass"
            ),
            "{err}"
        );
    }

    /// Rule 5, and the reason this loader reads the document instead of the
    /// typed struct: every one of these mistakes has to name the set — and the
    /// slot, when the complaint is about one — because the person reading the
    /// error is editing the policy, not debugging the parser. A set with no id
    /// has no name to give, and is named by its position instead.
    #[test]
    fn a_missing_or_malformed_field_names_the_set_and_the_slot_it_is_in() {
        let cases: [(&str, &[&str]); 8] = [
            (
                // no set id
                r#"
                [[outcome_sets]]
                [[outcome_sets.slots]]
                name = "pass"
                expects = "succeeded"
                "#,
                &["outcome set `#1`", "no `id`"],
            ),
            (
                // id is not a string
                r#"
                [[outcome_sets]]
                id = 7
                [[outcome_sets.slots]]
                name = "pass"
                expects = "succeeded"
                "#,
                &["outcome set `#1`", "`id` is a string, found an integer"],
            ),
            (
                // slot with no name
                r#"
                [[outcome_sets]]
                id = "build_result"
                [[outcome_sets.slots]]
                expects = "succeeded"
                "#,
                &["outcome set `build_result`", "slot `#1`", "no `name`"],
            ),
            (
                // slot with no expects
                r#"
                [[outcome_sets]]
                id = "build_result"
                [[outcome_sets.slots]]
                name = "compile_error"
                "#,
                &[
                    "outcome set `build_result`",
                    "slot `compile_error`",
                    "no `expects`",
                ],
            ),
            (
                // a claim that is not in the vocabulary
                r#"
                [[outcome_sets]]
                id = "build_result"
                [[outcome_sets.slots]]
                name = "compile_error"
                expects = "suceeded"
                "#,
                &[
                    "outcome set `build_result`",
                    "slot `compile_error`",
                    "`expects = \"suceeded\"` is not a claim",
                    "`succeeded`",
                ],
            ),
            (
                // expects is not a name
                r#"
                [[outcome_sets]]
                id = "build_result"
                [[outcome_sets.slots]]
                name = "compile_error"
                expects = 3
                "#,
                &[
                    "outcome set `build_result`",
                    "slot `compile_error`",
                    "found an integer",
                ],
            ),
            (
                // a typoed key in a slot
                r#"
                [[outcome_sets]]
                id = "build_result"
                [[outcome_sets.slots]]
                name = "compile_error"
                expecs = "failed"
                "#,
                &[
                    "outcome set `build_result`",
                    "slot `compile_error`",
                    "`expecs`",
                ],
            ),
            (
                // a typoed key in a set
                r#"
                [[outcome_sets]]
                id = "build_result"
                descripton = "a typo"
                [[outcome_sets.slots]]
                name = "pass"
                expects = "succeeded"
                "#,
                &["outcome set `build_result`", "`descripton`"],
            ),
        ];

        for (document, fragments) in cases {
            let err = Policy::from_toml_str(document).expect_err("must not load");
            let text = err.to_string();
            for fragment in fragments {
                assert!(
                    text.contains(fragment),
                    "the error must contain {fragment:?}, since whoever edits the policy \
                     has to find the spot: {text}"
                );
            }
        }
    }

    /// The M3 acceptance criterion — "改一条策略里的预测集,行为改变,不重新编译" —
    /// as a test. The same compiled code loads two different sets because the
    /// *file* changed; nothing was rebuilt between the two loads, and what the
    /// harness will check landings against is different.
    #[test]
    fn editing_the_policy_changes_the_pre_registered_outcomes_without_recompiling() {
        let path = std::env::temp_dir().join(format!("yu-outcomes-{}.toml", std::process::id()));

        std::fs::write(
            &path,
            r#"
            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "pass"
            expects = "succeeded"
            [[outcome_sets.slots]]
            name = "compile_error"
            expects = "failed"
            "#,
        )
        .expect("temp policy must be writable");
        let before = Policy::from_path(&path).expect("the first version loads");

        // The edit: one slot becomes two, and the set now predicts a stop.
        std::fs::write(
            &path,
            r#"
            [[outcome_sets]]
            id = "build_result"
            [[outcome_sets.slots]]
            name = "pass"
            expects = "succeeded"
            [[outcome_sets.slots]]
            name = "test_failure"
            expects = "failed"
            [[outcome_sets.slots]]
            name = "timeout"
            expects = "stopped_early"
            "#,
        )
        .expect("temp policy must be editable");
        let after = Policy::from_path(&path).expect("the edited version loads");

        std::fs::remove_file(&path).expect("temp policy must be removable");

        let before_set = before.outcome_sets.get("build_result").expect("declared");
        let after_set = after.outcome_sets.get("build_result").expect("still declared");

        assert_ne!(
            before_set.slots, after_set.slots,
            "the loaded sets must differ, because only the TOML differed"
        );
        assert_eq!(before_set.slots.len(), 2);
        assert_eq!(after_set.slots.len(), 3);
        assert_eq!(after_set.slots[1].name, "test_failure");
        assert_eq!(after_set.slots[2].expects, Expects::StoppedEarly);
        assert!(
            before_set
                .slots
                .iter()
                .all(|slot| slot.expects != Expects::StoppedEarly),
            "the first policy did not predict a stop; the second does"
        );
    }

    /// The loaded sets survive being written back out as TOML, which is the
    /// only form a policy has — a set that could not be re-encoded would be a
    /// set whose declaration cannot be stored, exported, or hashed into a
    /// byte-stable cache prefix.
    #[test]
    fn outcome_sets_round_trip_through_toml() {
        let policy = Policy::from_toml_str(EXAMPLE).expect("the example must parse");
        let text = toml::to_string(&policy).expect("a policy must re-encode as TOML");
        let again = Policy::from_toml_str(&text).expect("the re-encoded policy must parse");
        assert_eq!(policy.outcome_sets, again.outcome_sets);
    }
}
