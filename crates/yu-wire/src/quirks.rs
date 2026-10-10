//! Per-endpoint behavioural divergences.
//!
//! DESIGN: "**不建 provider 抽象层,但要为 quirks 留一层**", and, quoting a
//! practitioner who packages several provider SDKs, "most of my code is other
//! people's quirks". Even a hand-written six-endpoint client needed a dozen
//! quirk files (reasoning keys, tool-call ids, merging user messages).
//!
//! Measured evidence for every flag here is in `docs/WIRE-FACTS.md`. Nothing in
//! this file is inferred from documentation.
//!
//! The most valuable thing this module does is [`Quirks::check_history`]: it
//! converts the endpoint's misleading 400 into a precise, actionable error
//! naming the offending message index.

use serde::{Deserialize, Serialize};

use crate::types::{ContentBlock, MAX_CACHE_BREAKPOINTS, Message, Role};

/// Whether an assistant turn carrying `tool_use` must also carry `thinking`.
///
/// Measured: required unless the endpoint itself issued the tool-call id. Since
/// we cannot know that for a replayed or forked history, the safe encoding is
/// [`ThinkingRequirement::WithToolUse`] — always replay thinking blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingRequirement {
    Never,
    WithToolUse,
}

/// How a client should treat a capability it has not verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Measured working.
    Yes,
    /// Measured rejected.
    No,
    /// The endpoint accepted the field but the effect was not confirmed. Treating
    /// this as "yes" is how silent no-ops get shipped; it is a separate state on
    /// purpose.
    AcceptedUnverified,
    /// Not measured.
    Unknown,
}

impl Support {
    pub fn is_yes(self) -> bool {
        matches!(self, Support::Yes)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Support::Yes => "yes",
            Support::No => "no",
            Support::AcceptedUnverified => "accepted?",
            Support::Unknown => "unknown",
        }
    }
}

/// What this client knows about one endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quirks {
    pub name: String,

    // ---- capabilities -----------------------------------------------------
    /// Anthropic-defined schema-less tools (`bash_20250124`, `text_editor_20250728`).
    pub anthropic_defined_tools: Support,
    /// `tool_search` + `defer_loading`.
    pub tool_search: Support,
    /// Server-side MCP (`mcp_servers` + `mcp_toolset`).
    pub mcp_servers: Support,
    /// `GET /v1/models`.
    pub models_api: Support,
    /// `context_management.edits` is honoured (as opposed to merely accepted).
    pub context_management: Support,
    /// Server-side compaction actually compacts.
    pub server_compaction: Support,
    pub task_budget: Support,
    pub server_side_fallback: Support,
    pub structured_outputs: Support,
    pub effort: Support,
    /// Legacy `thinking: {type:"enabled", budget_tokens:N}`.
    pub legacy_thinking_budget: Support,
    /// `tool_choice: {type:"any"}`.
    pub tool_choice_any: Support,
    pub strict_tools: Support,

    // ---- caching ----------------------------------------------------------
    /// Explicit `cache_control` breakpoints affect caching.
    pub cache_breakpoints: Support,
    /// The server caches prefixes automatically, independent of `cache_control`.
    pub automatic_prefix_cache: Support,

    // ---- history rules ----------------------------------------------------
    /// When a `thinking` block must accompany `tool_use`.
    pub tool_use_requires_thinking: ThinkingRequirement,
    /// Whether `signature` bytes are checked. Measured `false` on the probe
    /// endpoint, which is what makes M3 fork/replay feasible.
    pub thinking_signature_validated: bool,
    /// Whether an unrecognised field yields 422 rather than 400.
    pub unsupported_is_422: bool,
    /// Whether the `anthropic-beta` header gates anything at all.
    pub beta_header_matters: bool,
}

impl Default for Quirks {
    fn default() -> Self {
        Self::unknown()
    }
}

impl Quirks {
    /// Nothing measured. Every capability `Unknown`, every rule conservative.
    pub fn unknown() -> Self {
        Self {
            name: "unknown".to_owned(),
            anthropic_defined_tools: Support::Unknown,
            tool_search: Support::Unknown,
            mcp_servers: Support::Unknown,
            models_api: Support::Unknown,
            context_management: Support::Unknown,
            server_compaction: Support::Unknown,
            task_budget: Support::Unknown,
            server_side_fallback: Support::Unknown,
            structured_outputs: Support::Unknown,
            effort: Support::Unknown,
            legacy_thinking_budget: Support::Unknown,
            tool_choice_any: Support::Unknown,
            strict_tools: Support::Unknown,
            cache_breakpoints: Support::Unknown,
            automatic_prefix_cache: Support::Unknown,
            tool_use_requires_thinking: ThinkingRequirement::WithToolUse,
            thinking_signature_validated: true,
            unsupported_is_422: false,
            beta_header_matters: true,
        }
    }

    /// DESIGN's assumed target: the full Anthropic surface.
    pub fn anthropic() -> Self {
        Self {
            name: "anthropic".to_owned(),
            anthropic_defined_tools: Support::Yes,
            tool_search: Support::Yes,
            mcp_servers: Support::Yes,
            models_api: Support::Yes,
            context_management: Support::Yes,
            server_compaction: Support::Yes,
            task_budget: Support::Yes,
            server_side_fallback: Support::Yes,
            structured_outputs: Support::Yes,
            effort: Support::Yes,
            legacy_thinking_budget: Support::No,
            tool_choice_any: Support::No,
            strict_tools: Support::Yes,
            cache_breakpoints: Support::Yes,
            automatic_prefix_cache: Support::No,
            tool_use_requires_thinking: ThinkingRequirement::WithToolUse,
            thinking_signature_validated: true,
            unsupported_is_422: false,
            beta_header_matters: true,
        }
    }

    /// Measured on `https://api.deepseek.com/anthropic`. Every value here is
    /// backed by a probe recorded in `docs/WIRE-FACTS.md`.
    pub fn deepseek_anthropic() -> Self {
        Self {
            name: "deepseek-anthropic".to_owned(),
            // 422 `unknown variant` for bash_20250124 and text_editor_20250728.
            anthropic_defined_tools: Support::No,
            tool_search: Support::No,
            mcp_servers: Support::No,
            models_api: Support::No,
            // Accepted with a 200, but nothing observable changed. Calling these
            // "yes" would be exactly the silent no-op DESIGN warns about.
            context_management: Support::AcceptedUnverified,
            server_compaction: Support::AcceptedUnverified,
            task_budget: Support::AcceptedUnverified,
            server_side_fallback: Support::AcceptedUnverified,
            structured_outputs: Support::AcceptedUnverified,
            effort: Support::AcceptedUnverified,
            // Measured accepted, though Anthropic 400s it on current models.
            legacy_thinking_budget: Support::Yes,
            // Measured 200, where Anthropic 400s on Opus 5.5 / Sonnet 5.5.
            tool_choice_any: Support::Yes,
            strict_tools: Support::Yes,
            // `cache_creation_input_tokens` is always 0 and only `cache_read` is
            // populated; two identical requests hit cache without a breakpoint.
            cache_breakpoints: Support::No,
            automatic_prefix_cache: Support::Yes,
            tool_use_requires_thinking: ThinkingRequirement::WithToolUse,
            // Measured: an all-zeros UUID and the string "not-a-real-signature"
            // were both accepted.
            thinking_signature_validated: false,
            unsupported_is_422: true,
            beta_header_matters: false,
        }
    }

    /// Look up a built-in profile by name.
    ///
    /// Only the profiles compiled in here are reachable. A name produced by
    /// probing is `probed:<url>` and is deliberately *not* answered from a
    /// table: the whole point of measuring is that the answer depends on the
    /// endpoint, and re-deriving it from a name would substitute a guess for a
    /// measurement. Persisted profiles come back through
    /// [`Quirks::from_capabilities_json`] instead.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" => Some(Self::anthropic()),
            "deepseek-anthropic" | "deepseek" => Some(Self::deepseek_anthropic()),
            "unknown" => Some(Self::unknown()),
            _ => None,
        }
    }

    /// Recover a measured profile from a persisted capability report.
    ///
    /// Closes the round trip that was open: `yu probe --json > caps.json` writes
    /// a profile whose name is `probed:<url>`, and `from_name` could never
    /// answer it — so the constraints could be measured and then not injected.
    /// DESIGN's derived principle 3 is that constraints are *injected* rather
    /// than remembered, and this is the injection point.
    pub fn from_capabilities_json(json: &str) -> Result<Self, String> {
        #[derive(serde::Deserialize)]
        struct Report {
            quirks: Quirks,
        }
        let report: Report =
            serde_json::from_str(json).map_err(|e| format!("not a capability report: {e}"))?;
        Ok(report.quirks)
    }

    /// Whether a name came from probing rather than from the built-in table.
    pub fn is_probed_name(name: &str) -> bool {
        name.starts_with("probed:")
    }

    /// Whether `cache_control` is worth emitting at all.
    ///
    /// On an endpoint with automatic prefix caching the breakpoints are inert,
    /// but emitting them is still correct for Anthropic and costs nothing. The
    /// flag exists so *strategy* can avoid depending on them.
    pub fn breakpoints_are_effective(&self) -> bool {
        self.cache_breakpoints.is_yes()
    }

    /// Validate a message history against the rules this endpoint enforces
    /// before sending it.
    ///
    /// Every check here corresponds to a measured 400 or to a rule DESIGN states
    /// as an iron law. Catching them client-side replaces a remote error that
    /// does not name a location with one that does.
    pub fn check_history(&self, messages: &[Message]) -> Result<(), HistoryProblem> {
        for (index, msg) in messages.iter().enumerate() {
            // `blocks()` returns an owned Vec, so bind it before borrowing into it.
            let blocks = msg.blocks();
            match msg.role {
                Role::Assistant => {
                    if self.tool_use_requires_thinking == ThinkingRequirement::WithToolUse {
                        let has_tool_use = blocks
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolUse(_)));
                        let has_thinking = blocks.iter().any(|b| {
                            matches!(
                                b,
                                ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)
                            )
                        });
                        if has_tool_use && !has_thinking {
                            return Err(HistoryProblem {
                                index,
                                kind: HistoryProblemKind::ToolUseWithoutThinking,
                                detail: "assistant turn carries `tool_use` but no `thinking` \
                                         block. This endpoint rejects it with a 400 that says \
                                         \"The content[].thinking in the thinking mode must be \
                                         passed back to the API\" and does not name the message. \
                                         Replay the recorded thinking block verbatim."
                                    .to_owned(),
                            });
                        }
                    }
                }
                Role::User => {
                    // DESIGN: all tool_result blocks must be returned in the SAME
                    // user message, and must match the preceding assistant turn.
                    let results: Vec<String> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult(r) => Some(r.tool_use_id.clone()),
                            _ => None,
                        })
                        .collect();
                    if results.is_empty() {
                        continue;
                    }
                    let expected: Vec<String> = match index.checked_sub(1) {
                        Some(prev) if messages[prev].role == Role::Assistant => messages[prev]
                            .blocks()
                            .iter()
                            .filter_map(|b| b.as_tool_use().map(|t| t.id.clone()))
                            .collect(),
                        _ => Vec::new(),
                    };
                    for id in &results {
                        if !expected.contains(id) {
                            return Err(HistoryProblem {
                                index,
                                kind: HistoryProblemKind::OrphanToolResult {
                                    tool_use_id: id.clone(),
                                },
                                detail: format!(
                                    "`tool_result` for `{id}` has no matching `tool_use` in the \
                                     preceding assistant message. Ids are opaque and must be \
                                     replayed verbatim; a single edited character produces this."
                                ),
                            });
                        }
                    }
                    let missing: Vec<String> = expected
                        .iter()
                        .filter(|e| !results.contains(e))
                        .cloned()
                        .collect();
                    if !missing.is_empty() {
                        return Err(HistoryProblem {
                            index,
                            kind: HistoryProblemKind::MissingToolResult {
                                tool_use_ids: missing,
                            },
                            detail: "every `tool_use` must be answered in the next user message. \
                                     Splitting them across messages silently teaches the model to \
                                     stop calling tools in parallel."
                                .to_owned(),
                        });
                    }
                }
                Role::System => {
                    // Mid-conversation system messages: cannot be first, must
                    // follow a user message, and must be last or followed by an
                    // assistant turn.
                    if index == 0 {
                        return Err(HistoryProblem {
                            index,
                            kind: HistoryProblemKind::SystemMessageMisplaced,
                            detail: "a mid-conversation system message cannot be messages[0]; \
                                     use the top-level `system` field."
                                .to_owned(),
                        });
                    }
                    if messages[index - 1].role != Role::User {
                        return Err(HistoryProblem {
                            index,
                            kind: HistoryProblemKind::SystemMessageMisplaced,
                            detail: "a mid-conversation system message must follow a user \
                                     message."
                                .to_owned(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Check a request's cache breakpoint count.
    pub fn check_cache_breakpoints(&self, count: usize) -> Result<(), HistoryProblem> {
        if count > MAX_CACHE_BREAKPOINTS {
            return Err(HistoryProblem {
                index: usize::MAX,
                kind: HistoryProblemKind::TooManyBreakpoints { count },
                detail: format!(
                    "{count} cache breakpoints exceeds the limit of {MAX_CACHE_BREAKPOINTS}. \
                     Render order is tools -> system -> messages, and a byte change anywhere in \
                     the prefix invalidates everything after it."
                ),
            });
        }
        Ok(())
    }

    /// Human-readable notes for `yu probe` output: what this endpoint cannot do
    /// that DESIGN assumed, and what that costs.
    pub fn limitations(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.anthropic_defined_tools == Support::No {
            out.push(
                "Anthropic-defined schema-less tools unavailable — every tool needs an \
                 explicit input_schema, and DESIGN's \"白拿训练过的工具纪律\" does not apply."
                    .to_owned(),
            );
        }
        if self.tool_search == Support::No {
            out.push(
                "tool_search / defer_loading unavailable — DESIGN risk 5 (cutting the \
                 coordinator's start-up floor) cannot be measured here."
                    .to_owned(),
            );
        }
        if self.server_compaction == Support::AcceptedUnverified {
            out.push(
                "Server-side compaction is accepted but inert — compaction and context \
                 management must be client-side."
                    .to_owned(),
            );
        }
        if self.cache_breakpoints == Support::No && self.automatic_prefix_cache.is_yes() {
            out.push(
                "Cache breakpoints are inert; prefix caching is automatic. DESIGN's \
                 \"字节稳定是铁律\" is the whole mechanism here — there is no breakpoint to place."
                    .to_owned(),
            );
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryProblemKind {
    ToolUseWithoutThinking,
    OrphanToolResult { tool_use_id: String },
    MissingToolResult { tool_use_ids: Vec<String> },
    SystemMessageMisplaced,
    TooManyBreakpoints { count: usize },
}

/// A history the endpoint would reject, located precisely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryProblem {
    /// Index into `messages`, or `usize::MAX` when not message-specific.
    pub index: usize,
    pub kind: HistoryProblemKind,
    pub detail: String,
}

impl std::fmt::Display for HistoryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.index == usize::MAX {
            write!(f, "{}", self.detail)
        } else {
            write!(f, "messages[{}]: {}", self.index, self.detail)
        }
    }
}

impl std::error::Error for HistoryProblem {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ThinkingBlock, ToolResultBlock, ToolUseBlock, Thinking};

    fn tool_use(id: &str) -> ContentBlock {
        ContentBlock::ToolUse(ToolUseBlock {
            id: id.to_owned(),
            name: "t".to_owned(),
            input: serde_json::json!({}),
        })
    }

    /// The single most important measured rule: reproduce the endpoint's 400
    /// locally, with a location the endpoint does not give.
    #[test]
    fn tool_use_without_thinking_is_caught_with_an_index() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![tool_use("call_00_a")]),
            Message::user(vec![ContentBlock::ToolResult(ToolResultBlock::text(
                "call_00_a", "r",
            ))]),
        ];
        let err = q.check_history(&messages).unwrap_err();
        assert_eq!(err.index, 1);
        assert_eq!(err.kind, HistoryProblemKind::ToolUseWithoutThinking);
        // Unlike the endpoint's message, ours names the location.
        assert!(err.to_string().contains("messages[1]"));
    }

    #[test]
    fn thinking_present_satisfies_the_rule() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "need the tool".into(),
                    signature: Some("69164b36-fbed-4709-adfd-d6871900f038".into()),
                }),
                tool_use("call_00_a"),
            ]),
            Message::user(vec![ContentBlock::ToolResult(ToolResultBlock::text(
                "call_00_a", "r",
            ))]),
        ];
        q.check_history(&messages).unwrap();
    }

    #[test]
    fn an_endpoint_that_does_not_require_thinking_skips_the_check() {
        let mut q = Quirks::deepseek_anthropic();
        q.tool_use_requires_thinking = ThinkingRequirement::Never;
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![tool_use("call_00_a")]),
            Message::user(vec![ContentBlock::ToolResult(ToolResultBlock::text(
                "call_00_a", "r",
            ))]),
        ];
        q.check_history(&messages).unwrap();
    }

    #[test]
    fn orphan_tool_result_is_located() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "x".into(),
                    signature: None,
                }),
                tool_use("call_00_a"),
            ]),
            Message::user(vec![ContentBlock::ToolResult(ToolResultBlock::text(
                "call_00_EDITED", "r",
            ))]),
        ];
        let err = q.check_history(&messages).unwrap_err();
        assert_eq!(err.index, 2);
        assert!(matches!(
            err.kind,
            HistoryProblemKind::OrphanToolResult { .. }
        ));
    }

    #[test]
    fn unanswered_tool_use_is_caught() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![
            Message::user("go"),
            Message::assistant(vec![
                ContentBlock::Thinking(ThinkingBlock {
                    thinking: "x".into(),
                    signature: None,
                }),
                tool_use("call_00_a"),
                tool_use("call_00_b"),
            ]),
            Message::user(vec![ContentBlock::ToolResult(ToolResultBlock::text(
                "call_00_a", "r",
            ))]),
        ];
        let err = q.check_history(&messages).unwrap_err();
        match err.kind {
            HistoryProblemKind::MissingToolResult { tool_use_ids } => {
                assert_eq!(tool_use_ids, vec!["call_00_b".to_owned()])
            }
            other => panic!("expected MissingToolResult, got {other:?}"),
        }
    }

    #[test]
    fn system_message_cannot_open_the_conversation() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![Message::system("be terse"), Message::user("go")];
        let err = q.check_history(&messages).unwrap_err();
        assert_eq!(err.kind, HistoryProblemKind::SystemMessageMisplaced);
    }

    #[test]
    fn system_message_after_a_user_turn_is_fine() {
        let q = Quirks::deepseek_anthropic();
        let messages = vec![
            Message::user("go"),
            Message::assistant("ok"),
            Message::user("again"),
            Message::system("answer with one word"),
        ];
        q.check_history(&messages).unwrap();
    }

    #[test]
    fn five_cache_breakpoints_are_rejected() {
        let q = Quirks::anthropic();
        assert!(q.check_cache_breakpoints(4).is_ok());
        let err = q.check_cache_breakpoints(5).unwrap_err();
        assert_eq!(err.kind, HistoryProblemKind::TooManyBreakpoints { count: 5 });
    }

    #[test]
    fn limitations_name_what_design_assumed_and_this_endpoint_cannot_do() {
        let notes = Quirks::deepseek_anthropic().limitations();
        assert!(notes.iter().any(|n| n.contains("tool_search")));
        assert!(notes.iter().any(|n| n.contains("schema-less")));
        assert!(notes.iter().any(|n| n.contains("inert")));
    }

    /// The injection path, end to end.
    ///
    /// `yu probe --json` writes a report whose profile is named `probed:<url>`,
    /// and nothing could read it back — so the constraints could be measured and
    /// then not injected, which is the one place DESIGN's derived principle 3
    /// actually bears weight.
    #[test]
    fn a_measured_profile_survives_the_round_trip() {
        let mut measured = Quirks::deepseek_anthropic();
        measured.name = "probed:https://example.test".to_owned();
        measured.cache_breakpoints = Support::No;
        measured.automatic_prefix_cache = Support::Yes;
        measured.thinking_signature_validated = false;
        measured.unsupported_is_422 = true;

        let report = serde_json::json!({
            "base_url": "https://example.test",
            "model": "m",
            "probed_at_ms": 0,
            "entries": [],
            "quirks": measured,
        });

        let recovered = Quirks::from_capabilities_json(&report.to_string()).unwrap();
        assert_eq!(recovered, measured, "a measured profile must survive intact");
        // Including the fields the old `infer_quirks` never set at all.
        assert_eq!(recovered.automatic_prefix_cache, Support::Yes);
        assert!(!recovered.thinking_signature_validated);
        assert!(recovered.unsupported_is_422);

        // And the name is one the built-in table cannot answer — which is
        // precisely why this path has to exist rather than a name lookup.
        assert!(Quirks::is_probed_name(&recovered.name));
        assert!(Quirks::from_name(&recovered.name).is_none());
    }

    #[test]
    fn a_profile_that_will_not_load_is_an_error_not_a_default() {
        // Silently substituting the built-in profile would mean running under
        // constraints nobody chose, while believing they were measured.
        assert!(Quirks::from_capabilities_json("not json at all").is_err());
        assert!(Quirks::from_capabilities_json("{}").is_err());
    }

    #[test]
    fn unknown_quirks_are_conservative() {
        let q = Quirks::unknown();
        // Unmeasured must not read as "supported".
        assert!(!q.anthropic_defined_tools.is_yes());
        assert_eq!(q.effort, Support::Unknown);
        // And the history rule defaults to the strict one.
        assert_eq!(q.tool_use_requires_thinking, ThinkingRequirement::WithToolUse);
        assert!(q.thinking_signature_validated);
    }

    #[test]
    fn thinking_variant_is_available_for_the_legacy_path() {
        // Guards the type used by the legacy-budget quirk.
        let t = Thinking::Enabled { budget_tokens: 2048 };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            serde_json::json!({"type":"enabled","budget_tokens":2048})
        );
    }
}
