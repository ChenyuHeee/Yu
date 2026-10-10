//! Declarative policy resources.
//!
//! DESIGN's constraint 0:
//!
//! > **prompt / 工具描述 / 角色 / 拓扑 / 归约规则 / hook / 预算 —— 一律外部资源.**
//!
//! and its acceptance criterion is literal: **changing one policy rule must not
//! require recompiling Rust.** So every kind below is *data* — a `String` or a
//! `BTreeMap` — and never a Rust `enum`. An enum would turn "add a topology kind"
//! into a recompile, which is exactly the tax constraint 0 exists to cancel.
//!
//! Two tiers, per DESIGN:
//!
//! * tier 1 — declarative data in TOML, loaded by [`Policy`];
//! * tier 2 — embedded scripts for policy that contains judgement, evaluated by a
//!   [`ScriptHost`] (Rhai by preference, wasmtime when isolation must be
//!   stronger).
//!
//! Maps are `BTreeMap`, not `HashMap`: policy files are diffed by humans and
//! hashed into byte-stable cache prefixes (DESIGN's API facts), so iteration
//! order must not depend on a random seed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Ceilings a run may spend. `None` means "no ceiling configured" rather than
/// zero — an absent budget must be visibly absent, because a budget that reads as
/// configured but is not is worse than none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Budgets {
    pub max_steps: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_tool_calls: Option<u64>,
}

/// A declarative role. Topology is a policy (DESIGN), so a role is a *view* —
/// which prompt it is given, which model tier it runs at — and never behaviour.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Role {
    /// Name of an entry in [`Policy::prompts`], not the text itself, so that one
    /// prompt can serve several roles and a prompt edit has exactly one home.
    pub prompt: Option<String>,
    /// Model downgrade for execution-layer roles. `None` keeps the run default,
    /// which is the decision layer's model.
    pub model: Option<String>,
    /// `output_config.effort` for this role: `low` … `max`.
    pub effort: Option<String>,
}

/// One declarative topology: DESIGN's chain / fan-out / tree / reflection loop /
/// peer / verification partition. `kind` is a string because that list is
/// today's set, not a closed one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TopologyRule {
    pub kind: String,
    /// Role name for derived agents, when the kind derives any.
    pub child_role: Option<String>,
    /// Kind-specific knobs, free-form so that a new topology does not have to
    /// touch this file.
    pub params: BTreeMap<String, String>,
}

/// One row of DESIGN's reduction table. The path is chosen by
/// (kind × size × reproducible × task shape × current pressure); each row states
/// the axes it constrains and the path taken when they match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReductionRule {
    /// `pass_through` / `drop_with_manifest` / `spill_with_pointer` /
    /// `dispatch_reader` / `sandbox_reduce`. A string for the same reason as
    /// [`TopologyRule::kind`]: DESIGN names five paths today, not a closed set.
    pub path: String,
    /// Axis name -> the value that selects this row, e.g. `size = "large"`.
    pub when: BTreeMap<String, String>,
}

/// A hook: an event, plus the name of the script to run on it. The script is a
/// name rather than inline source so one script can be shared by several hooks
/// and reviewed in one place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hook {
    /// Event that fires it, e.g. `before_tool_call` / `on_handover`.
    pub on: String,
    /// Entry in [`Policy::scripts`]. `None` declares the hook's position without
    /// arming it, which is how a policy records intent it has not written yet.
    pub script: Option<String>,
}

/// A whole policy, as read from one TOML file.
///
/// Unknown keys are rejected rather than ignored: a typo (`[budget]`,
/// `max_input_token`) would otherwise leave a rule looking configured while it
/// does nothing, which is a silent omission — the one thing DESIGN does not
/// permit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    /// Prompt name -> text. The decision layer's specification lives here (DESIGN:
    /// "决策层没有现成 prompt,但有规格了"), and it is the bundle whose edit must
    /// never require Rust.
    pub prompts: BTreeMap<String, String>,
    /// Tool name -> the description shown to the model.
    pub tool_descriptions: BTreeMap<String, String>,
    pub roles: BTreeMap<String, Role>,
    /// Topology name -> rule. A name is what a parent agent asks for at run time.
    pub topology: BTreeMap<String, TopologyRule>,
    /// Reduction table rows. Order is priority: rows are evaluated in declaration
    /// order, and TOML preserves it, so a policy expresses precedence by position
    /// rather than by a numeric field nobody maintains.
    pub reduction: Vec<ReductionRule>,
    pub hooks: Vec<Hook>,
    /// Tier 2 script name -> source.
    pub scripts: BTreeMap<String, String>,
    pub budgets: Budgets,
}

impl Policy {
    /// Parse a policy from TOML text.
    ///
    /// This function existing at run time *is* the acceptance criterion: the
    /// binary that reads a policy is not rebuilt when the policy changes.
    pub fn from_toml_str(s: &str) -> Result<Self, PolicyError> {
        toml::from_str(s).map_err(|e| PolicyError::Parse(e.to_string()))
    }

    /// Parse a policy from a file. The delivery path for constraint 0: rules live
    /// on disk and are re-read, not compiled in.
    pub fn from_path(p: &Path) -> Result<Self, PolicyError> {
        let text = std::fs::read_to_string(p).map_err(|source| PolicyError::Io {
            path: p.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// The prompt under `name`, or `None`.
    ///
    /// `None` and not `""`: an empty prompt renders as an agent that was given
    /// nothing while looking like it was given something. Callers that cannot
    /// proceed without one use [`Policy::require_prompt`].
    pub fn prompt(&self, name: &str) -> Option<&str> {
        self.prompts.get(name).map(String::as_str)
    }

    /// The description for tool `name`, or `None` for the same reason as
    /// [`Policy::prompt`].
    pub fn tool_description(&self, name: &str) -> Option<&str> {
        self.tool_descriptions.get(name).map(String::as_str)
    }

    pub fn budget(&self) -> &Budgets {
        &self.budgets
    }

    /// As [`Policy::prompt`], but an absent name is an error rather than a
    /// silently promptless agent.
    pub fn require_prompt(&self, name: &str) -> Result<&str, PolicyError> {
        self.prompt(name).ok_or_else(|| PolicyError::Missing {
            key: format!("prompts.{name}"),
        })
    }

    /// The source of a tier-2 script, by name.
    pub fn script(&self, name: &str) -> Result<&str, PolicyError> {
        self.scripts
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| PolicyError::UnknownScript {
                name: name.to_owned(),
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("could not read policy file `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("policy TOML is malformed: {0}")]
    Parse(String),
    #[error("policy is missing `{key}`")]
    Missing { key: String },
    #[error("policy has no script named `{name}`")]
    UnknownScript { name: String },
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Tier 2: the host that evaluates embedded policy scripts.
///
/// DESIGN recommends **Rhai** because it is pure Rust, sandboxed by default, and
/// adds no separate runtime — the script shares the process, so there is no
/// toolchain or cross-compilation story, and the sandbox is on by default rather
/// than something to configure. `wasmtime` is the escalation for policy that must
/// be isolated *from the host process*, which buys a real boundary at the cost of
/// a large dependency and a compiled artifact per script. Neither is implemented
/// yet; the trait exists so that choosing between them — or keeping both, selected
/// per script — never reaches the callers.
pub trait ScriptHost {
    /// Evaluate a script whose contract is "return a bool for this context".
    ///
    /// `ctx` is JSON rather than a Rust type for the same reason the rest of this
    /// module is data: a policy script must be able to inspect run state the host
    /// never anticipated, without a Rust edit.
    fn eval_bool(&self, script: &str, ctx: &serde_json::Value) -> Result<bool, PolicyError>;

    /// Name of the engine, for logs and for a policy file to record which engine
    /// ran a rule.
    fn engine_name(&self) -> &'static str;
}

/// Placeholder Rhai host. See [`ScriptHost`] for why Rhai is the default choice.
///
// TODO(M4): implement against the `rhai` crate, with the engine kept per-policy
// rather than per-call so limits and registered functions are configured once.
pub struct RhaiHost;

impl ScriptHost for RhaiHost {
    fn eval_bool(&self, _script: &str, _ctx: &serde_json::Value) -> Result<bool, PolicyError> {
        Err(PolicyError::Unsupported(
            "not yet implemented: rhai script host",
        ))
    }

    fn engine_name(&self) -> &'static str {
        "rhai"
    }
}

/// Placeholder wasmtime host. See [`ScriptHost`] for when this is worth its cost.
///
// TODO(M4): implement once there is a policy that needs a boundary the Rhai
// sandbox cannot give — until then this would be a dependency with no caller.
pub struct WasmtimeHost;

impl ScriptHost for WasmtimeHost {
    fn eval_bool(&self, _script: &str, _ctx: &serde_json::Value) -> Result<bool, PolicyError> {
        Err(PolicyError::Unsupported(
            "not yet implemented: wasmtime script host",
        ))
    }

    fn engine_name(&self) -> &'static str {
        "wasmtime"
    }
}

/// A small, complete policy: the shape reference for hand-written files and the
/// input to this module's tests.
///
/// Every bundle is optional, so a file may carry only what it overrides. The
/// shape, with the same keys as below:
///
/// ```toml
/// [prompts]               # name -> text, injected every turn (L0)
/// [tool_descriptions]     # tool name -> description shown to the model
/// [roles.<name>]          # prompt / model / effort
/// [topology.<name>]       # kind / child_role / params
/// [[reduction]]           # one table row, in priority order
/// [[hooks]]               # event -> named tier-2 script
/// [scripts.<name>]        # tier-2 script source
/// [budgets]               # step and token ceilings
/// ```
pub const EXAMPLE_POLICY_TOML: &str = r#"
# A policy is data. Editing anything below — including the decision layer's
# prompt — is a file edit, never a recompile.

[prompts]
decision_charter = """
You are the decision layer. You do not read code and you do not call tools.
You own completion criteria, architecture, and anything irreversible.
A claim without runnable evidence is unverified; ask for the evidence.
Delegate everything else, and ask the human when the risk is systemic.
"""
worker_brief = """
You are a disposable execution agent. You are given the constraints you must
respect; do not assume any constraint you were not given.
"""

[tool_descriptions]
delegate = "Hand a self-contained task to a fresh execution agent."
spot_check = "Ask for a runnable artifact that proves a claim. Expensive."

[roles.decision]
prompt = "decision_charter"
effort = "xhigh"

[roles.worker]
prompt = "worker_brief"
model = "claude-sonnet-5"

[topology.review_loop]
kind = "reflection_loop"
child_role = "worker"

[[reduction]]
path = "drop_with_manifest"
[reduction.when]
size = "large"
reproducible = "true"

[[reduction]]
path = "spill_with_pointer"
[reduction.when]
size = "large"
reproducible = "false"
addressable = "true"

[[reduction]]
path = "dispatch_reader"
[reduction.when]
shape = "unbounded"

[[hooks]]
on = "on_handover"
script = "handover_is_worth_it"

[scripts]
handover_is_worth_it = "ctx.loose_ends > 0 || ctx.steps_since_compaction > ctx.threshold"

[budgets]
max_steps = 200
max_input_tokens = 900000
max_output_tokens = 64000
max_tool_calls = 400
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_policy_parses() {
        let policy = Policy::from_toml_str(EXAMPLE_POLICY_TOML).expect("the example must parse");

        let charter = policy
            .prompt("decision_charter")
            .expect("the example declares a decision charter");
        assert!(charter.contains("You are the decision layer"));

        assert_eq!(
            policy.tool_description("spot_check"),
            Some("Ask for a runnable artifact that proves a claim. Expensive.")
        );
        assert_eq!(
            policy.roles["decision"].prompt.as_deref(),
            Some("decision_charter")
        );
        assert_eq!(
            policy.topology["review_loop"].kind.as_str(),
            "reflection_loop"
        );
        assert_eq!(policy.reduction.len(), 3);
        assert_eq!(policy.reduction[0].path.as_str(), "drop_with_manifest");
        assert_eq!(
            policy
                .script("handover_is_worth_it")
                .expect("the hook's script is declared"),
            "ctx.loose_ends > 0 || ctx.steps_since_compaction > ctx.threshold"
        );
    }

    #[test]
    fn a_budget_value_round_trips() {
        let policy = Policy::from_toml_str(EXAMPLE_POLICY_TOML).expect("the example must parse");
        assert_eq!(policy.budget().max_input_tokens, Some(900_000));

        // Round-trip through TOML, not Rust: the value has to survive being
        // written back out as data, since that is the only form it has.
        let text = toml::to_string(&policy).expect("a policy must re-encode as TOML");
        let again = Policy::from_toml_str(&text).expect("re-encoded policy must parse");
        assert_eq!(policy.budgets, again.budgets);
        assert_eq!(again.budget().max_tool_calls, Some(400));
        assert_eq!(again.budget().max_output_tokens, Some(64_000));

        // An absent ceiling must stay absent, not turn into "zero allowed".
        let partial = Policy::from_toml_str("[budgets]\nmax_steps = 5\n").expect("must parse");
        assert_eq!(partial.budget().max_steps, Some(5));
        assert_eq!(partial.budget().max_tool_calls, None);
    }

    #[test]
    fn an_unknown_prompt_is_none_not_an_empty_string() {
        let policy = Policy::from_toml_str(EXAMPLE_POLICY_TOML).expect("the example must parse");
        assert_eq!(policy.prompt("no_such_prompt"), None);
        assert_eq!(policy.tool_description("no_such_tool"), None);
        assert!(matches!(
            policy.require_prompt("no_such_prompt"),
            Err(PolicyError::Missing { key }) if key == "prompts.no_such_prompt"
        ));
    }

    #[test]
    fn a_malformed_policy_is_an_error_not_a_panic() {
        for bad in [
            "[[[",                        // not TOML at all
            "[prompts]\nfoo = ",          // truncated value
            "[budget]\nmax_steps = 3",    // a typo in a bundle name, refused
        ] {
            let err = Policy::from_toml_str(bad).expect_err("malformed input must not parse");
            assert!(matches!(err, PolicyError::Parse(_)), "{bad:?} -> {err}");
        }
    }

    /// The acceptance criterion, asserted as behaviour rather than as a promise:
    /// the same call site answers differently because the *data* differs. If any
    /// part of the decision layer's specification were compiled into Rust, these
    /// two would agree.
    #[test]
    fn a_policy_rule_is_data_the_call_site_cannot_second_guess() {
        let terse = Policy::from_toml_str(
            r#"
            [prompts]
            decision_charter = "Only the human's words count as intent."
            "#,
        )
        .expect("must parse");

        let expansive = Policy::from_toml_str(
            r#"
            [prompts]
            decision_charter = "Delegate everything; read nothing but evidence blocks."
            "#,
        )
        .expect("must parse");

        assert_eq!(
            terse.prompt("decision_charter"),
            Some("Only the human's words count as intent.")
        );
        assert_eq!(
            expansive.prompt("decision_charter"),
            Some("Delegate everything; read nothing but evidence blocks.")
        );
        assert_ne!(
            terse.prompt("decision_charter"),
            expansive.prompt("decision_charter"),
            "the answer must come from the TOML, not from Rust"
        );
    }

    #[test]
    fn a_policy_can_be_edited_on_disk_without_recompiling() {
        let path = std::env::temp_dir().join(format!("yu-policy-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[prompts]\ndecision_charter = \"Read the record, not the report.\"\n",
        )
        .expect("temp policy must be writable");

        let policy = Policy::from_path(&path).expect("a file on disk must parse");
        assert_eq!(
            policy.prompt("decision_charter"),
            Some("Read the record, not the report.")
        );

        std::fs::remove_file(&path).expect("temp policy must be removable");

        let missing = Policy::from_path(&path).expect_err("the file is gone again");
        assert!(matches!(missing, PolicyError::Io { .. }));
    }

    #[test]
    fn an_undeclared_script_names_itself_in_the_error() {
        let policy = Policy::default();
        assert!(matches!(
            policy.script("nope"),
            Err(PolicyError::UnknownScript { name }) if name == "nope"
        ));
    }

    #[test]
    fn tier_two_hosts_are_declared_but_unimplemented() {
        let ctx = serde_json::json!({ "loose_ends": 1 });

        assert!(matches!(
            RhaiHost.eval_bool("true", &ctx),
            Err(PolicyError::Unsupported(_))
        ));
        assert!(matches!(
            WasmtimeHost.eval_bool("true", &ctx),
            Err(PolicyError::Unsupported(_))
        ));

        assert_eq!(RhaiHost.engine_name(), "rhai");
        assert_eq!(WasmtimeHost.engine_name(), "wasmtime");
    }
}

// ---------------------------------------------------------------------------
// Injection
// ---------------------------------------------------------------------------

/// The wire that makes constraint 0 true rather than aspirational.
///
/// DESIGN: "**约束靠注入,不靠记忆**" — every layer, every turn, is *given* its
/// constraints. `yu-core` defines the trait and must not depend on this crate;
/// this is the other half, where a parsed policy becomes something the loop can
/// ask. Until this impl existed, the acceptance criterion ("changing one policy
/// rule must not require recompiling Rust") held only for parsing a file: the
/// rules could be read and nothing could read them.
impl yu_core::traits::PolicySource for Policy {
    fn prompt(&self, name: &str) -> Option<String> {
        self.prompts.get(name).cloned()
    }

    fn budget(&self, name: &str) -> Option<u64> {
        // Budgets are named fields rather than a map, so a typo in a policy is a
        // parse error instead of a silently-unset rule. The cost is this
        // translation, which is exhaustive by construction — a new budget field
        // without an arm here is a compile error, not a lookup that quietly
        // returns `None`.
        let Budgets {
            max_steps,
            max_input_tokens,
            max_output_tokens,
            max_tool_calls,
        } = &self.budgets;
        match name {
            "max_steps" => *max_steps,
            "max_input_tokens" => *max_input_tokens,
            "max_output_tokens" => *max_output_tokens,
            "max_tool_calls" => *max_tool_calls,
            _ => None,
        }
    }

    fn rule(&self, name: &str) -> Option<serde_json::Value> {
        // One namespace across the tables, so a caller asks for a rule by name
        // without having to know which table it was declared in — the thing that
        // lets topology be chosen at run time rather than compiled in.
        if let Some(topology) = self.topology.get(name) {
            return serde_json::to_value(topology).ok();
        }
        if name == "reduction" {
            return serde_json::to_value(&self.reduction).ok();
        }
        self.hooks
            .iter()
            .find(|h| h.on == name)
            .and_then(|h| serde_json::to_value(h).ok())
    }
}

#[cfg(test)]
mod policy_source_tests {
    use super::*;
    use yu_core::traits::{EmptyPolicy, PolicySource};

    /// The acceptance criterion for constraint 0, stated as a test.
    ///
    /// Two policies that differ only in their TOML must produce two different
    /// answers through the *same* compiled code path. If any decision were
    /// compiled into Rust, these would have to agree.
    #[test]
    fn two_policies_answer_differently_through_the_same_code() {
        let a = Policy::from_toml_str(
            r#"
            [prompts]
            decision_charter = "Ship the smallest change that works."

            [budgets]
            max_steps = 10
            "#,
        )
        .unwrap();
        let b = Policy::from_toml_str(
            r#"
            [prompts]
            decision_charter = "Stop at the first sign of trouble."

            [budgets]
            max_steps = 999
            "#,
        )
        .unwrap();

        assert_ne!(
            PolicySource::prompt(&a, "decision_charter"),
            PolicySource::prompt(&b, "decision_charter")
        );
        assert_eq!(PolicySource::budget(&a, "max_steps"), Some(10));
        assert_eq!(PolicySource::budget(&b, "max_steps"), Some(999));
    }

    /// The injection boundary is a trait object, so a loop can hold a policy
    /// without knowing which crate it came from.
    #[test]
    fn a_policy_is_usable_as_a_trait_object() {
        let p = Policy::from_toml_str(
            r#"
            [prompts]
            executor = "Do the work."

            [budgets]
            max_output_tokens = 4096
            "#,
        )
        .unwrap();
        let source: &dyn PolicySource = &p;
        assert_eq!(source.prompt("executor").as_deref(), Some("Do the work."));
        assert_eq!(source.budget("max_output_tokens"), Some(4096));
    }

    #[test]
    fn an_absent_rule_is_none_rather_than_a_default() {
        // A missing constraint must be visible as missing. Returning a default
        // here would let an unconfigured budget look like an unlimited one.
        let p = Policy::default();
        assert_eq!(PolicySource::prompt(&p, "nope"), None);
        assert_eq!(PolicySource::budget(&p, "max_steps"), None);
        assert_eq!(PolicySource::rule(&p, "nope"), None);
        // And the empty source agrees, so "unconfigured" reads the same however
        // it is reached.
        assert_eq!(PolicySource::prompt(&EmptyPolicy, "nope"), None);
    }

    #[test]
    fn rules_are_reachable_by_name_across_tables() {
        let p = Policy::from_toml_str(
            r#"
            [topology.chain]
            kind = "chain"
            child_role = "executor"

            [[hooks]]
            on = "before_tool_call"
            "#,
        )
        .unwrap();
        let topology = PolicySource::rule(&p, "chain").expect("topology reachable by name");
        assert_eq!(topology["kind"], "chain");
        let hook = PolicySource::rule(&p, "before_tool_call").expect("hook reachable by event");
        assert_eq!(hook["on"], "before_tool_call");
    }

    #[test]
    fn every_budget_name_the_loop_asks_for_is_answered() {
        // Guards the translation above: a budget field that exists but is not
        // reachable by name would read as unconfigured.
        let p = Policy::from_toml_str(
            r#"
            [budgets]
            max_steps = 1
            max_input_tokens = 2
            max_output_tokens = 3
            max_tool_calls = 4
            "#,
        )
        .unwrap();
        for (name, expected) in [
            ("max_steps", 1),
            ("max_input_tokens", 2),
            ("max_output_tokens", 3),
            ("max_tool_calls", 4),
        ] {
            assert_eq!(
                PolicySource::budget(&p, name),
                Some(expected),
                "budget `{name}` is not reachable through PolicySource"
            );
        }
    }
}
