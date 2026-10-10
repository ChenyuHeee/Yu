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
    /// A tier-2 script failed to evaluate. Carried as text because the engine's
    /// diagnostic is the useful part, and re-typing it would lose the position.
    #[error("script: {0}")]
    Script(String),
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

/// The Rhai host: tier 2, for policy that contains judgement.
///
/// DESIGN recommends Rhai because it is pure Rust, sandboxed by default, and
/// adds no native dependency — so a policy script can express a decision without
/// the binary growing a C toolchain or a JIT.
///
/// "Sandboxed by default" is a claim worth being precise about, since it is the
/// reason this engine was chosen. Rhai's `Engine` has no file, network, process
/// or time access unless a host function is registered, and none are registered
/// here; it starts with no modules at all. What remains is the ability to loop or
/// recurse, which is why an operation ceiling is set as well — an unbounded
/// script is a hang, and a hang in a policy rule looks exactly like a slow model.
pub struct RhaiHost {
    /// Ceiling on the work one evaluation may do.
    ///
    /// DESIGN's rule for long-path mechanisms is that they should be a function
    /// of pressure rather than of ambition; a script that never returns is the
    /// degenerate case, so the budget is structural rather than advisory.
    pub max_operations: u64,
}

impl Default for RhaiHost {
    fn default() -> Self {
        Self {
            // Generous for a rule that decides something, small enough that a
            // runaway loop is measured in milliseconds rather than in a hung run.
            max_operations: 100_000,
        }
    }
}

impl RhaiHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_operations(mut self, max: u64) -> Self {
        self.max_operations = max;
        self
    }
}

/// The top-level fields a script reads off `ctx`.
///
/// A scan rather than a parse: it only has to be conservative in the direction
/// that matters, which is finding every field that is read. A name it fails to
/// see means a missed check, never a false alarm.
fn referenced_context_fields(script: &str) -> Vec<String> {
    let chars: Vec<char> = script.chars().collect();
    let needle: Vec<char> = "ctx.".chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            let mut j = i + needle.len();
            let mut name = String::new();
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                name.push(chars[j]);
                j += 1;
            }
            if !name.is_empty() {
                found.push(name);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Refuse a rule that reads a field this context does not have.
fn check_context_fields(script: &str, ctx: &serde_json::Value) -> Result<(), PolicyError> {
    let Some(map) = ctx.as_object() else {
        return Ok(());
    };
    for field in referenced_context_fields(script) {
        if !map.contains_key(&field) {
            return Err(PolicyError::Script(format!(
                "the rule reads `ctx.{field}`, which this policy context does not have. \
                 Without this check Rhai would yield `()` for the missing field and the \
                 comparison would be false, so a misspelling would read as a condition \
                 that simply did not hold."
            )));
        }
    }
    Ok(())
}

/// Convert a JSON context into Rhai values.
///
/// Written out rather than pulled in behind a feature, because the conversion is
/// where a policy script and the run state meet: an object that silently became
/// an opaque string would make every rule that inspects run state fail in a way
/// that looks like the rule being wrong.
fn json_to_dynamic(value: &serde_json::Value) -> rhai::Dynamic {
    use rhai::Dynamic;
    match value {
        serde_json::Value::Null => Dynamic::UNIT,
        serde_json::Value::Bool(b) => Dynamic::from_bool(*b),
        // `no_float` is on, so a JSON float that is not integral has to be
        // reported rather than truncated — a rule that compares 0.9 against 1
        // would otherwise be silently wrong.
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Dynamic::from_int(i),
            None => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < i64::MAX as f64 => {
                    Dynamic::from_int(f as i64)
                }
                _ => Dynamic::from(format!("{n}")),
            },
        },
        serde_json::Value::String(s) => Dynamic::from(s.clone()),
        serde_json::Value::Array(items) => {
            let array: rhai::Array = items.iter().map(json_to_dynamic).collect();
            Dynamic::from_array(array)
        }
        serde_json::Value::Object(map) => {
            let mut out = rhai::Map::new();
            for (key, item) in map {
                out.insert(key.clone().into(), json_to_dynamic(item));
            }
            Dynamic::from_map(out)
        }
    }
}

impl ScriptHost for RhaiHost {
    fn eval_bool(&self, script: &str, ctx: &serde_json::Value) -> Result<bool, PolicyError> {
        let mut engine = rhai::Engine::new();
        // The sandbox, stated rather than assumed: no host functions are
        // registered, so the only capability a script has is arithmetic over the
        // context it was handed.
        engine.set_max_operations(self.max_operations);
        // Bounded output too: a script that builds a huge string is a denial of
        // service that an operation count alone would not catch.
        engine.set_max_string_size(64 * 1024);
        engine.set_max_array_size(4096);
        engine.set_max_map_size(4096);
        engine.set_max_call_levels(32);

        // A misspelled field must not read as `false`.
        //
        // Rhai yields `()` for an absent map key and `() > 1` is false, so a rule
        // naming a field the context does not have would evaluate false and look
        // like a condition that simply did not hold — the silent-omission failure
        // this project exists to avoid, arriving inside its own policy engine.
        //
        // Rhai's hook for this is behind its unstable `internals` feature, so the
        // check is done here instead: every top-level field the script reads off
        // `ctx` is confirmed to exist before evaluation. That covers the common
        // misspelling at the point a rule is loaded rather than the point it
        // quietly stops firing. It does *not* cover a misspelling one level down
        // (`ctx.usage.token` where the field is `tokens`); that remains a silent
        // false, and is the reason a policy should keep its contexts shallow.
        check_context_fields(script, ctx)?;

        let mut scope = rhai::Scope::new();
        scope.push_constant("ctx", json_to_dynamic(ctx));

        engine
            .eval_with_scope::<bool>(&mut scope, script)
            .map_err(|e| PolicyError::Script(e.to_string()))
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
    fn the_script_hosts_are_honest_about_what_they_are() {
        let ctx = serde_json::json!({ "loose_ends": 1 });

        // Rhai is implemented, so a tier-2 rule actually runs.
        assert!(RhaiHost::new().eval_bool("ctx.loose_ends > 0", &ctx).is_ok());

        // Wasmtime is not, and says so rather than pretending. DESIGN keeps it
        // for stronger isolation than Rhai gives; half a wasmtime host would be
        // worse than none, because a policy that chose it for isolation would be
        // running without it.
        assert!(matches!(
            WasmtimeHost.eval_bool("true", &ctx),
            Err(PolicyError::Unsupported(_))
        ));

        assert_eq!(RhaiHost::new().engine_name(), "rhai");
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

// ---------------------------------------------------------------------------
// The reduction table, injected
// ---------------------------------------------------------------------------

/// The axes a policy row can name. Anything else is a typo, and is reported as
/// one rather than matching nothing in silence.
pub const REDUCTION_AXES: [&str; 7] = [
    "kind",
    "size",
    "reproducible",
    "addressable",
    "pressure",
    "already_on_disk",
    "tool",
];

impl Policy {
    /// The axes named by reduction rows that this policy does not understand.
    ///
    /// An unknown axis cannot match anything, so the row is dead — and a dead
    /// row is indistinguishable from a row whose condition simply never came up.
    /// Reporting it at load time is the difference between a policy that does
    /// nothing and a policy that says it does nothing.
    pub fn unknown_reduction_axes(&self) -> Vec<String> {
        let mut unknown = std::collections::BTreeSet::new();
        for row in &self.reduction {
            for axis in row.when.keys() {
                if !REDUCTION_AXES.contains(&axis.as_str()) {
                    unknown.insert(axis.clone());
                }
            }
        }
        unknown.into_iter().collect()
    }
}

fn axis_bool(want: &str) -> Option<bool> {
    match want {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// Whether a row's conditions all hold.
///
/// Every named axis must match, and all of them are conjunctive: a row states
/// the situation it applies to, and a row that applied whenever *any* of its
/// axes matched would be a rule nobody could predict.
fn row_matches(row: &ReductionRule, axes: &yu_tools::reduce::Axes<'_>) -> bool {
    row.when.iter().all(|(axis, want)| match axis.as_str() {
        "kind" => axes.kind.as_str() == want,
        "size" => match want.as_str() {
            "small" => axes.size <= axes.inline_limit,
            "large" => axes.size > axes.inline_limit,
            _ => false,
        },
        "reproducible" => axis_bool(want) == Some(axes.reproducible),
        "addressable" => axis_bool(want) == Some(axes.addressable),
        "pressure" => match want.as_str() {
            "low" => !axes.pressure.is_high(),
            "high" => axes.pressure.is_high(),
            _ => false,
        },
        "already_on_disk" => axis_bool(want) == Some(axes.already_on_disk),
        "tool" => axes.provenance.tool == *want,
        // Unreachable for a policy that passed `unknown_reduction_axes`, and
        // false rather than true so an unvalidated one fails closed.
        _ => false,
    })
}

/// Constraint 0 for the reduction table.
///
/// DESIGN lists "归约规则" among the resources that must be external, and the
/// acceptance criterion is that changing one must not need a rebuild. Until this
/// impl existed the table was a constant in `yu-tools` — so the criterion held
/// for every rule except the one the project is built around.
///
/// Rows are tried in declaration order, so a policy expresses precedence by
/// position rather than by a numeric priority nobody maintains.
impl yu_tools::reduce::ReductionTable for Policy {
    fn path_for(
        &self,
        axes: &yu_tools::reduce::Axes<'_>,
    ) -> Option<yu_tools::reduce::ReductionPath> {
        for row in &self.reduction {
            if row_matches(row, axes) {
                // `parse` returning `None` means the policy named a path that
                // does not exist. Falling through to the next row would let a
                // typo quietly select a different strategy, so the row is
                // skipped *and* reported by `unknown_reduction_paths`.
                return yu_tools::reduce::ReductionPath::parse(&row.path);
            }
        }
        None
    }
}

impl Policy {
    /// Paths named by rows that are not one of DESIGN's five.
    pub fn unknown_reduction_paths(&self) -> Vec<String> {
        let mut unknown = Vec::new();
        for row in &self.reduction {
            if yu_tools::reduce::ReductionPath::parse(&row.path).is_none()
                && !unknown.contains(&row.path)
            {
                unknown.push(row.path.clone());
            }
        }
        unknown
    }
}

#[cfg(test)]
mod reduction_table_tests {
    use super::*;
    use yu_tools::reduce::{Axes, Budget, Pressure, Reduction, ReductionPath, ReductionTable, Reducer};
    use yu_tools::result::{Provenance, ResultKind, ToolResult};

    fn axes_for<'a>(
        kind: ResultKind,
        size: u64,
        reproducible: bool,
        addressable: bool,
        pressure: Pressure,
        provenance: &'a Provenance,
    ) -> Axes<'a> {
        Axes {
            kind,
            size,
            reproducible,
            addressable,
            pressure,
            already_on_disk: false,
            inline_limit: 8192,
            provenance,
        }
    }

    fn big_log() -> ToolResult {
        ToolResult::inline(
            "exec",
            serde_json::json!({"command": "cargo test"}),
            ResultKind::Log,
            "x".repeat(40_000),
        )
    }

    /// The acceptance criterion for constraint 0, on the rule it was least true
    /// of: a policy changes what the reducer does, with no rebuild.
    #[test]
    fn a_policy_row_changes_the_reduction() {
        let p = Policy::from_toml_str(
            r#"
            [[reduction]]
            path = "pass_through"
            [reduction.when]
            kind = "log"
            size = "large"
            "#,
        )
        .unwrap();

        let result = big_log();
        let axes = axes_for(
            result.kind,
            result.size,
            result.reproducible,
            result.addressable,
            Pressure::high(),
            &result.provenance,
        );
        assert_eq!(
            p.path_for(&axes),
            Some(ReductionPath::PassThrough),
            "the declared row must win"
        );

        // And an undeclared situation falls through rather than being answered
        // by the policy — a policy expresses the rows it changes, not every row.
        let small = ToolResult::inline(
            "read_file",
            serde_json::json!({"path": "a.rs"}),
            ResultKind::File,
            "short",
        );
        let small_axes = axes_for(
            small.kind,
            small.size,
            small.reproducible,
            small.addressable,
            Pressure::low(),
            &small.provenance,
        );
        assert_eq!(p.path_for(&small_axes), None);
    }

    #[test]
    fn rows_are_tried_in_declaration_order_so_position_expresses_precedence() {
        let p = Policy::from_toml_str(
            r#"
            [[reduction]]
            path = "sandbox_reduce"
            [reduction.when]
            kind = "log"

            [[reduction]]
            path = "pass_through"
            [reduction.when]
            kind = "log"
            "#,
        )
        .unwrap();
        let result = big_log();
        let axes = axes_for(
            result.kind,
            result.size,
            result.reproducible,
            result.addressable,
            Pressure::high(),
            &result.provenance,
        );
        assert_eq!(p.path_for(&axes), Some(ReductionPath::SandboxReduce));
    }

    /// Every axis a row may name must actually be consulted by the matcher.
    ///
    /// A name in `REDUCTION_AXES` that the matcher does not handle would be a row
    /// that validates and then never matches anything — indistinguishable from a
    /// condition that simply never came up. So this checks each axis on its own,
    /// with a value the axes really satisfy.
    #[test]
    fn every_axis_a_row_can_name_is_consulted_by_the_matcher() {
        let result = big_log();
        let axes = axes_for(
            result.kind,
            result.size,
            true,  // reproducible
            true,  // addressable
            Pressure::high(),
            &result.provenance,
        );

        // The value each axis must be given to be satisfied by these axes.
        let satisfied: [(&str, &str); 7] = [
            ("kind", "log"),
            ("size", "large"),
            ("reproducible", "true"),
            ("addressable", "true"),
            ("pressure", "high"),
            ("already_on_disk", "false"),
            ("tool", "exec"),
        ];
        assert_eq!(
            satisfied.len(),
            REDUCTION_AXES.len(),
            "every declared axis needs a satisfied value here, or this test stops \
             covering the list"
        );

        for (axis, value) in satisfied {
            assert!(
                REDUCTION_AXES.contains(&axis),
                "`{axis}` is tested here but is not a declared axis"
            );
            let policy = Policy::from_toml_str(&format!(
                "[[reduction]]\npath = \"pass_through\"\n[reduction.when]\n{axis} = \"{value}\"\n"
            ))
            .unwrap();
            assert!(
                policy.unknown_reduction_axes().is_empty(),
                "`{axis}` must be a known axis"
            );
            assert_eq!(
                policy.path_for(&axes),
                Some(ReductionPath::PassThrough),
                "a row naming only `{axis} = \"{value}\"` must match axes that satisfy it — \
                 the matcher is not consulting this axis"
            );
        }
    }

    #[test]
    fn a_typo_in_an_axis_or_a_path_is_reported_rather_than_silently_dead() {
        let p = Policy::from_toml_str(
            r#"
            [[reduction]]
            path = "dispath_reader"
            [reduction.when]
            knd = "log"
            "#,
        )
        .unwrap();
        assert_eq!(p.unknown_reduction_axes(), vec!["knd".to_owned()]);
        assert_eq!(p.unknown_reduction_paths(), vec!["dispath_reader".to_owned()]);
    }

    #[test]
    fn every_path_design_names_is_parseable() {
        for path in [
            "pass_through",
            "drop_with_manifest",
            "spill_with_pointer",
            "dispatch_reader",
            "sandbox_reduce",
        ] {
            assert!(
                ReductionPath::parse(path).is_some(),
                "DESIGN names `{path}` and the parser does not know it"
            );
        }
        assert!(ReductionPath::parse("nonsense").is_none());
    }

    /// The policy path and the built-in table must agree where they overlap.
    ///
    /// `materialise` mirrors the branches of `Reducer::choose`; two
    /// implementations of one rule drift, and the drift would be silent because
    /// each would look right alone. This asserts they do not.
    #[test]
    fn a_policy_path_matches_what_the_builtin_table_would_have_chosen() {
        fn path_of(r: &Reduction) -> ReductionPath {
            match r {
                Reduction::PassThrough => ReductionPath::PassThrough,
                Reduction::DropWithManifest { .. } => ReductionPath::DropWithManifest,
                Reduction::SpillWithPointer { .. } => ReductionPath::SpillWithPointer,
                Reduction::DelegateReader { .. } => ReductionPath::DispatchReader,
                Reduction::SandboxReduce => ReductionPath::SandboxReduce,
            }
        }

        /// A table that always names the path the built-in table chose.
        struct Echo(ReductionPath);
        impl ReductionTable for Echo {
            fn path_for(&self, _axes: &Axes<'_>) -> Option<ReductionPath> {
                Some(self.0)
            }
        }

        let budget = Budget::new(8192, 2048);
        let builtin = Reducer::new(budget);
        let cases = vec![
            ToolResult::inline(
                "read_file",
                serde_json::json!({"path": "a.rs"}),
                ResultKind::File,
                "small",
            ),
            ToolResult::inline(
                "exec",
                serde_json::json!({"command": "cargo test"}),
                ResultKind::Log,
                "x".repeat(40_000),
            ),
            ToolResult::inline(
                "exec",
                serde_json::json!({"command": "cargo build"}),
                ResultKind::Diff,
                "y".repeat(40_000),
            ),
        ];

        for result in cases {
            for pressure in [Pressure::low(), Pressure::high()] {
                let chosen = builtin.choose(&result, pressure);
                let echoed = builtin
                    .clone()
                    .with_table(std::sync::Arc::new(Echo(path_of(&chosen))));
                assert_eq!(
                    echoed.choose(&result, pressure),
                    chosen,
                    "the policy path and the built-in table must produce the same \
                     reduction for the same axes"
                );
            }
        }
    }
}

#[cfg(test)]
mod script_host_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_rule_reads_run_state_and_decides() {
        let host = RhaiHost::new();
        let ctx = json!({"steps": 12, "budget": 10, "tool": "exec"});
        assert!(
            host.eval_bool("ctx.steps > ctx.budget", &ctx).unwrap(),
            "a rule must be able to compare run state"
        );
        assert!(!host.eval_bool("ctx.steps < ctx.budget", &ctx).unwrap());
    }

    #[test]
    fn nested_state_is_reachable_by_name() {
        // The reason the context is converted structurally rather than as an
        // opaque string: a rule that inspects run state has to be able to reach
        // into it, or every such rule fails in a way that looks like the rule
        // being wrong.
        let host = RhaiHost::new();
        let ctx = json!({
            "shape": {"repeats": 2},
            "files": ["a.rs", "b.rs"],
            "usage": {"input_tokens": 5000}
        });
        assert!(host.eval_bool("ctx.shape.repeats >= 2", &ctx).unwrap());
        assert!(host.eval_bool("ctx.files.len() == 2", &ctx).unwrap());
        assert!(
            host.eval_bool("ctx.usage.input_tokens > 4000", &ctx).unwrap()
        );
    }

    #[test]
    fn a_missing_field_is_an_error_not_a_false() {
        // Rhai yields `()` for an absent map key and `() > 1` is false, so a
        // rule that misspells a field would evaluate false and look like a
        // condition that did not hold. A typo in a policy rule has to be a
        // loud failure, not a decision.
        let host = RhaiHost::new();
        let err = host
            .eval_bool("ctx.nonexistent > 1", &json!({"steps": 1}))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("nonexistent"), "the error must name the field: {text}");
    }

    #[test]
    fn a_runaway_script_is_stopped_rather_than_hanging_the_run() {
        // "Sandboxed by default" covers capability, not time. A loop is the
        // capability that remains, and an unbounded one looks exactly like a slow
        // model — so the ceiling is structural.
        let host = RhaiHost::new().with_max_operations(10_000);
        let err = host
            .eval_bool("let i = 0; while true { i += 1; } true", &json!({}))
            .unwrap_err();
        assert!(matches!(err, PolicyError::Script(_)), "{err}");
    }

    #[test]
    fn a_script_that_does_not_return_a_bool_is_an_error() {
        let host = RhaiHost::new();
        let err = host.eval_bool("42", &json!({})).unwrap_err();
        assert!(matches!(err, PolicyError::Script(_)), "{err}");
    }

    #[test]
    fn a_syntax_error_is_reported_with_the_engine_diagnostic() {
        let host = RhaiHost::new();
        let err = host.eval_bool("ctx. > >", &json!({})).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("script"), "{text}");
        assert!(text.len() > "script: ".len() + 8, "the diagnostic is the useful part: {text}");
    }

    #[test]
    fn the_sandbox_has_no_host_functions_registered() {
        // The claim that justifies choosing Rhai. If this ever starts passing,
        // the engine has gained a capability and the reason for the choice has
        // changed with it.
        let host = RhaiHost::new();
        for attempt in [
            r#"file_exists("/etc/passwd")"#,
            r#"system("echo hi")"#,
            r#"eval("1 + 1")"#,
        ] {
            assert!(
                host.eval_bool(attempt, &json!({})).is_err(),
                "`{attempt}` should not be available to a policy script"
            );
        }
    }

    #[test]
    fn the_host_names_its_engine_so_a_policy_can_record_which_one_ran() {
        assert_eq!(RhaiHost::new().engine_name(), "rhai");
    }

    #[test]
    fn the_wasmtime_host_says_it_is_not_available_rather_than_pretending() {
        // DESIGN keeps wasmtime for stronger isolation than Rhai gives. Half a
        // wasmtime host would be worse than none, because a policy that chose it
        // for isolation would be running without it.
        let host = WasmtimeHost;
        assert_eq!(host.engine_name(), "wasmtime");
        assert!(matches!(
            host.eval_bool("true", &json!({})),
            Err(PolicyError::Unsupported(_))
        ));
    }
}
