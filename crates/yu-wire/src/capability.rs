//! What the endpoint actually supports — measured, not assumed.
//!
//! DESIGN's "API 事实" section pins an Anthropic surface. Pointed at any other
//! endpoint that list becomes a hypothesis, and the failure mode of a wrong
//! hypothesis is the worst kind: the server accepts the field, does nothing, and
//! the harness believes it has a feature it does not have.
//!
//! So this module measures. It is deliberately cheap — every probe is a tiny
//! request — and it distinguishes **working** from **accepted but unverified**,
//! because conflating those is how silent no-ops ship.
//!
//! A 422 body enumerates the variants an endpoint accepts, which makes a
//! deliberately-wrong request the cheapest possible capability probe.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::client::WireClient;
use crate::error::WireError;
use crate::quirks::{Quirks, Support};

/// One measured feature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub feature: String,
    pub support: Support,
    pub status: Option<u16>,
    pub detail: String,
    /// For rejections: the variants the endpoint said it does accept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_variants: Vec<String>,
}

impl Capability {
    fn ok(feature: &str, support: Support, status: u16, detail: impl Into<String>) -> Self {
        Self {
            feature: feature.to_owned(),
            support,
            status: Some(status),
            detail: detail.into(),
            expected_variants: Vec::new(),
        }
    }
}

/// A probe specification.
struct Probe {
    feature: &'static str,
    /// Whether a 200 is conclusive.
    ///
    /// `true` for things whose effect is directly visible in the response
    /// (streaming, tool acceptance, token counting). `false` for things whose
    /// acceptance cannot be confirmed from one call (compaction, effort,
    /// structured outputs) — those report `AcceptedUnverified`, which is a
    /// different state from `Yes` on purpose.
    observable: bool,
    body: fn(&str) -> Value,
    betas: &'static [&'static str],
}

const OK_USER: &str = "Reply with the single word: ok";

fn messages_body(model: &str, extra: Value) -> Value {
    let mut base = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": OK_USER}],
    });
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            b.insert(k.clone(), v.clone());
        }
    }
    base
}

fn probes() -> Vec<Probe> {
    vec![
        Probe {
            feature: "messages",
            observable: true,
            betas: &[],
            body: |m| messages_body(m, json!({})),
        },
        Probe {
            feature: "streaming",
            observable: true,
            betas: &[],
            body: |m| messages_body(m, json!({"stream": true})),
        },
        Probe {
            feature: "thinking.adaptive",
            observable: true,
            betas: &[],
            body: |m| messages_body(m, json!({"thinking": {"type": "adaptive"}})),
        },
        Probe {
            feature: "thinking.legacy_budget_tokens",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 2048}}),
                )
            },
        },
        Probe {
            feature: "output_config.effort",
            // The field is accepted; whether it changes behaviour needs a
            // quality comparison, not one call.
            observable: false,
            betas: &[],
            body: |m| messages_body(m, json!({"output_config": {"effort": "low"}})),
        },
        Probe {
            feature: "output_config.format (structured outputs)",
            observable: false,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"output_config": {"format": {"type": "json_schema", "schema": {
                        "type": "object",
                        "properties": {"answer": {"type": "string"}},
                        "required": ["answer"],
                        "additionalProperties": false
                    }}}}),
                )
            },
        },
        Probe {
            feature: "cache_control",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"system": [{"type": "text", "text": "You are terse.", "cache_control": {"type": "ephemeral"}}]}),
                )
            },
        },
        Probe {
            feature: "tools.custom",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"tools": [{
                        "name": "get_time",
                        "description": "Return the current time in a named timezone.",
                        "input_schema": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]}
                    }]}),
                )
            },
        },
        Probe {
            feature: "tools.strict",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"tools": [{
                        "name": "get_time",
                        "description": "Return the current time in a named timezone.",
                        "strict": true,
                        "input_schema": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"], "additionalProperties": false}
                    }]}),
                )
            },
        },
        Probe {
            feature: "tool_choice.any",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({
                        "tools": [{"name": "get_time", "description": "Time.", "input_schema": {"type": "object", "properties": {}}}],
                        "tool_choice": {"type": "any"}
                    }),
                )
            },
        },
        Probe {
            feature: "tools.anthropic_defined.bash_20250124",
            observable: true,
            betas: &[],
            body: |m| messages_body(m, json!({"tools": [{"type": "bash_20250124", "name": "bash"}]})),
        },
        Probe {
            feature: "tools.anthropic_defined.text_editor_20250728",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"tools": [{"type": "text_editor_20250728", "name": "str_replace_based_edit_tool"}]}),
                )
            },
        },
        Probe {
            feature: "tools.tool_search + defer_loading",
            observable: true,
            betas: &[],
            body: |m| {
                messages_body(
                    m,
                    json!({"tools": [
                        {"type": "tool_search_tool_regex_20251119", "name": "tool_search_tool_regex"},
                        {"name": "deferred", "description": "Deferred.", "defer_loading": true, "input_schema": {"type": "object", "properties": {}}},
                        {"name": "always_on", "description": "Always loaded.", "input_schema": {"type": "object", "properties": {}}}
                    ]}),
                )
            },
        },
        Probe {
            feature: "mcp_servers + mcp_toolset",
            observable: true,
            betas: &["mcp-client-2025-11-20"],
            body: |m| {
                messages_body(
                    m,
                    json!({
                        "mcp_servers": [{"type": "url", "url": "https://example.com/mcp", "name": "probe"}],
                        "tools": [{"type": "mcp_toolset", "mcp_server_name": "probe"}]
                    }),
                )
            },
        },
        Probe {
            feature: "context_management.edits",
            observable: false,
            betas: &["context-management-2025-06-27"],
            body: |m| {
                messages_body(
                    m,
                    json!({"context_management": {"edits": [{"type": "clear_tool_uses_20250919"}]}}),
                )
            },
        },
        Probe {
            feature: "server-side compaction",
            observable: false,
            betas: &["compact-2026-01-12"],
            body: |m| {
                messages_body(
                    m,
                    json!({"context_management": {"edits": [{"type": "compact_20260112"}]}}),
                )
            },
        },
        Probe {
            feature: "task_budget",
            observable: false,
            betas: &["task-budgets-2026-03-13"],
            body: |m| {
                messages_body(
                    m,
                    json!({"output_config": {"effort": "low", "task_budget": {"type": "tokens", "total": 20000}}}),
                )
            },
        },
        Probe {
            feature: "server-side fallback",
            observable: false,
            betas: &["server-side-fallback-2026-07-01"],
            body: |m| messages_body(m, json!({"fallbacks": "default"})),
        },
    ]
}

/// A full capability report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub base_url: String,
    pub model: String,
    pub probed_at_ms: i64,
    pub entries: Vec<Capability>,
    /// Inferred from the measurements.
    pub quirks: Quirks,
}

impl Capabilities {
    pub fn get(&self, feature: &str) -> Option<&Capability> {
        self.entries.iter().find(|e| e.feature == feature)
    }

    /// Infer a [`Quirks`] profile from what was measured.
    ///
    /// This is DESIGN's "约束靠注入,不靠记忆" applied to the wire layer: the
    /// constraint set is produced by measurement and then injected, rather than
    /// being something the code is expected to remember.
    fn infer_quirks(base_url: &str, entries: &[Capability]) -> Quirks {
        let find = |f: &str| entries.iter().find(|e| e.feature == f).map(|e| e.support);
        let mut q = if base_url.contains("api.anthropic.com") {
            Quirks::anthropic()
        } else {
            Quirks::unknown()
        };
        q.name = if base_url.contains("api.anthropic.com") {
            "anthropic".to_owned()
        } else {
            format!("probed:{base_url}")
        };

        let set = |field: &mut Support, feature: &str| {
            if let Some(s) = find(feature) {
                *field = s;
            }
        };
        set(&mut q.anthropic_defined_tools, "tools.anthropic_defined.bash_20250124");
        set(&mut q.tool_search, "tools.tool_search + defer_loading");
        set(&mut q.mcp_servers, "mcp_servers + mcp_toolset");
        set(&mut q.context_management, "context_management.edits");
        set(&mut q.server_compaction, "server-side compaction");
        set(&mut q.task_budget, "task_budget");
        set(&mut q.server_side_fallback, "server-side fallback");
        set(&mut q.structured_outputs, "output_config.format (structured outputs)");
        set(&mut q.effort, "output_config.effort");
        set(&mut q.legacy_thinking_budget, "thinking.legacy_budget_tokens");
        set(&mut q.tool_choice_any, "tool_choice.any");
        set(&mut q.strict_tools, "tools.strict");
        q
    }

    /// Run the probe suite against a client.
    ///
    /// Every request is small. The whole suite costs on the order of a few
    /// hundred tokens.
    pub async fn probe(client: &WireClient, model: &str) -> Result<Self, WireError> {
        let mut entries = Vec::new();

        for probe in probes() {
            let body = (probe.body)(model);
            let betas: Vec<String> = probe.betas.iter().map(|s| (*s).to_owned()).collect();
            let result = client
                .raw_request(reqwest::Method::POST, "/v1/messages", Some(&body), &betas)
                .await;

            entries.push(match result {
                Ok((status, text, _)) if (200..300).contains(&status) => {
                    let support = if probe.observable {
                        Support::Yes
                    } else {
                        Support::AcceptedUnverified
                    };
                    Capability::ok(probe.feature, support, status, summarize_body(&text))
                }
                Ok((status, text, _)) => {
                    let err = WireError::from_response(status, &text, None);
                    Capability {
                        feature: probe.feature.to_owned(),
                        support: Support::No,
                        status: Some(status),
                        detail: err.message.chars().take(200).collect(),
                        expected_variants: err.expected_variants,
                    }
                }
                Err(e) => Capability {
                    feature: probe.feature.to_owned(),
                    support: Support::Unknown,
                    status: None,
                    detail: format!("probe failed: {e}"),
                    expected_variants: Vec::new(),
                },
            });
        }

        // Models API: a GET, not a POST.
        let models = client
            .raw_request(reqwest::Method::GET, "/v1/models", None, &[])
            .await;
        entries.push(match models {
            Ok((status, text, _)) if (200..300).contains(&status) => {
                Capability::ok("models_api", Support::Yes, status, first_model_ids(&text))
            }
            Ok((status, text, _)) => Capability {
                feature: "models_api".to_owned(),
                support: Support::No,
                status: Some(status),
                detail: format!("HTTP {status}: {}", text.chars().take(80).collect::<String>()),
                expected_variants: Vec::new(),
            },
            Err(e) => Capability {
                feature: "models_api".to_owned(),
                support: Support::Unknown,
                status: None,
                detail: format!("probe failed: {e}"),
                expected_variants: Vec::new(),
            },
        });

        // count_tokens needs a real body shape.
        let count_body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "Count the tokens in this sentence, please."}]
        });
        let counted = client
            .raw_request(
                reqwest::Method::POST,
                "/v1/messages/count_tokens",
                Some(&count_body),
                &[],
            )
            .await;
        entries.push(match counted {
            Ok((status, text, _)) if (200..300).contains(&status) => {
                Capability::ok("count_tokens", Support::Yes, status, summarize_body(&text))
            }
            Ok((status, text, _)) => Capability {
                feature: "count_tokens".to_owned(),
                support: Support::No,
                status: Some(status),
                detail: WireError::from_response(status, &text, None).message,
                expected_variants: Vec::new(),
            },
            Err(e) => Capability {
                feature: "count_tokens".to_owned(),
                support: Support::Unknown,
                status: None,
                detail: format!("probe failed: {e}"),
                expected_variants: Vec::new(),
            },
        });

        let quirks = Self::infer_quirks(client.base_url(), &entries);
        Ok(Self {
            base_url: client.base_url().to_owned(),
            model: model.to_owned(),
            probed_at_ms: yu_observe::now_ms(),
            entries,
            quirks,
        })
    }

    /// Probe using the environment's configuration.
    pub async fn probe_from_env() -> Result<Self, WireError> {
        let client = WireClient::from_env()?;
        let model = client.default_model().to_owned();
        Self::probe(&client, &model).await
    }
}

/// Convenience wrapper for `yu probe`.
pub async fn probe_from_env() -> Result<Capabilities, WireError> {
    Capabilities::probe_from_env().await
}

fn summarize_body(text: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return text.chars().take(120).collect();
    };
    let stop = v
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("<none>");
    let blocks: Vec<&str> = v
        .get("content")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|b| b.get("type").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let usage = v.get("usage").cloned().unwrap_or(Value::Null);
    format!(
        "stop={stop} blocks=[{}] in={} out={} cr={}",
        blocks.join(","),
        usage.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
        usage.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
        usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
}

fn first_model_ids(text: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return "unparseable".to_owned();
    };
    let ids: Vec<String> = v
        .get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str))
                .take(8)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        "no models listed".to_owned()
    } else {
        ids.join(", ")
    }
}

impl std::fmt::Display for Capabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f)?;
        writeln!(f, "{}", "=".repeat(96))?;
        writeln!(f, "  CAPABILITIES  {}  model={}", self.base_url, self.model)?;
        writeln!(f, "{}", "=".repeat(96))?;
        for e in &self.entries {
            writeln!(
                f,
                "  {:<9} {:<46} {:>4}  {}",
                e.support.as_str(),
                e.feature,
                e.status.map(|s| s.to_string()).unwrap_or_else(|| "--".into()),
                e.detail
            )?;
            if !e.expected_variants.is_empty() {
                writeln!(f, "            accepts: {}", e.expected_variants.join(", "))?;
            }
        }
        let limits = self.quirks.limitations();
        if !limits.is_empty() {
            writeln!(f)?;
            writeln!(f, "  What this costs against DESIGN:")?;
            for l in limits {
                writeln!(f, "    - {l}")?;
            }
        }
        writeln!(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_bodies_are_all_well_formed_json() {
        for p in probes() {
            let body = (p.body)("test-model");
            assert!(
                body.get("model").is_some(),
                "probe `{}` must name a model",
                p.feature
            );
            assert!(
                body.get("max_tokens").is_some(),
                "probe `{}` must bound its output",
                p.feature
            );
        }
    }

    #[test]
    fn probe_output_is_bounded_so_probing_stays_cheap() {
        for p in probes() {
            let body = (p.body)("m");
            let max = body
                .get("max_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            assert!(max <= 4096, "probe `{}` allows {max} tokens", p.feature);
        }
    }

    #[test]
    fn unobservable_features_are_not_reported_as_plain_yes() {
        // The distinction that stops silent no-ops from shipping.
        let caps = Capabilities {
            base_url: "https://example.test".into(),
            model: "m".into(),
            probed_at_ms: 0,
            entries: vec![Capability::ok(
                "server-side compaction",
                Support::AcceptedUnverified,
                200,
                "ok",
            )],
            quirks: Quirks::unknown(),
        };
        assert_eq!(
            caps.get("server-side compaction").unwrap().support,
            Support::AcceptedUnverified
        );
        assert!(!caps.get("server-side compaction").unwrap().support.is_yes());
    }

    #[test]
    fn quirks_are_inferred_from_measurements_not_from_the_hostname() {
        let entries = vec![
            Capability::ok("tools.anthropic_defined.bash_20250124", Support::No, 422, "nope"),
            Capability::ok("tool_choice.any", Support::Yes, 200, "ok"),
        ];
        let q = Capabilities::infer_quirks("https://some-proxy.test", &entries);
        assert_eq!(q.anthropic_defined_tools, Support::No);
        assert_eq!(q.tool_choice_any, Support::Yes);
        assert!(q.name.starts_with("probed:"));
    }

    #[test]
    fn anthropic_hostname_still_gets_probed_values() {
        let entries = vec![Capability::ok(
            "tools.anthropic_defined.bash_20250124",
            Support::No,
            422,
            "unexpected on the real API",
        )];
        let q = Capabilities::infer_quirks("https://api.anthropic.com", &entries);
        assert_eq!(
            q.anthropic_defined_tools,
            Support::No,
            "measurement must win over the default profile"
        );
        // Untouched fields keep the Anthropic defaults.
        assert_eq!(q.tool_search, Support::Yes);
    }

    #[test]
    fn body_summary_reports_cache_reads() {
        let s = summarize_body(
            r#"{"stop_reason":"end_turn","content":[{"type":"thinking"},{"type":"text"}],"usage":{"input_tokens":37,"output_tokens":14,"cache_read_input_tokens":128}}"#,
        );
        assert!(s.contains("cr=128"), "{s}");
        assert!(s.contains("blocks=[thinking,text]"), "{s}");
    }
}
