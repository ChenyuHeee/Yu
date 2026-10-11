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
use crate::error::{WireError, redact_key_material};
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

/// What a 2xx means for a probe.
///
/// `Yes` only where the response itself is the evidence; everything else is
/// `AcceptedUnverified`, because "the endpoint accepted the field" and "the
/// field works" are different states, and reporting the first as the second is
/// how silent no-ops ship.
fn acceptance_support(probe: &Probe) -> Support {
    if probe.observable {
        Support::Yes
    } else {
        Support::AcceptedUnverified
    }
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
            // NOT observable, and it was marked observable before — which is
            // exactly the failure this module's own doc warns about. Whether a
            // cache breakpoint *works* cannot be read off one response: its
            // effect is that a later request hits. The probe table used to print
            // `yes cache_control 200 … cr=0`, presenting a cache miss as a
            // capability. The real answer comes from comparing two requests,
            // which `probe_caching` below does.
            observable: false,
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
    #[allow(clippy::too_many_arguments)]
    fn infer_quirks(
        base_url: &str,
        entries: &[Capability],
        caching: CachingProbe,
        thinking_signature_validated: bool,
        unsupported_is_422: bool,
        beta_header_matters: bool,
    ) -> Quirks {
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

        // The five that used to be left at their defaults, four of them opposite
        // to what had actually been measured. A capability report that disagrees
        // with reality is worse than none, because it is believed.
        q.cache_breakpoints = caching.breakpoints;
        q.automatic_prefix_cache = caching.automatic;
        q.thinking_signature_validated = thinking_signature_validated;
        q.unsupported_is_422 = unsupported_is_422;
        q.beta_header_matters = beta_header_matters;
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
                    Capability::ok(
                        probe.feature,
                        acceptance_support(&probe),
                        status,
                        summarize_body(&text),
                    )
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

        // ---- Quirk 3: is caching automatic, and do breakpoints do anything?
        //
        // One request cannot answer this, so these are not in the `probes` table:
        // they compare two requests and read the *difference*. The earlier table
        // reported `cache_control` as supported on the strength of a 200, which
        // said nothing about whether a breakpoint has any effect at all.
        let (breakpoints, automatic) = probe_caching(client, model).await;

        // ---- Quirk 1 & 5: is the signature checked, and is a schema the
        // endpoint cannot model reported as 422 rather than 400?
        let signature_validated = probe_signature(client, model).await;

        // ---- Quirk 5: `unsupported_is_422`.
        let unsupported_is_422 = entries
            .iter()
            .any(|e| e.support == Support::No && e.status == Some(422));

        // ---- The `anthropic-beta` header. Measured inert here: every field the
        // endpoint accepts, it accepts with or without the header. A probe that
        // needs the header to be present to mean anything would be the honest
        // test; until then this is inferred from the schema rejections, which do
        // not mention it.
        let beta_header_matters = entries
            .iter()
            .any(|e| e.support == Support::No && e.status == Some(400) && !e.expected_variants.is_empty());

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
                detail: error_detail(status, &text),
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

        let quirks = Self::infer_quirks(
            client.base_url(),
            &entries,
            CachingProbe { breakpoints, automatic },
            signature_validated,
            unsupported_is_422,
            beta_header_matters,
        );
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

/// The one-line detail for a rejected probe, bounded and redacted.
///
/// This is the only error body copied into a [`Capability`] without passing
/// through [`WireError`], and a profile is printed and serialized, so the
/// Quirk 6 rules apply to it directly.
fn error_detail(status: u16, body: &str) -> String {
    let redacted = redact_key_material(body);
    format!(
        "HTTP {status}: {}",
        redacted.chars().take(80).collect::<String>()
    )
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
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::time::Duration;

    use super::*;
    use crate::client::Auth;

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
    fn a_rejected_probe_detail_carries_no_key_material() {
        // The `models_api` branch copies the body without `WireError`, so it
        // has to be redacted on its own account — a profile is persisted.
        let body =
            r#"{"error":{"message":"Authentication Fails, Your api key: ****0000 is invalid"}}"#;
        let detail = error_detail(401, body);
        assert!(!detail.contains("0000"), "{detail}");
        assert!(detail.contains("****"), "{detail}");
        assert!(detail.contains("Authentication Fails"), "{detail}");
    }

    #[test]
    fn a_rejected_probe_detail_is_still_bounded() {
        let body = "x".repeat(10_000);
        assert!(error_detail(500, &body).chars().count() < 100);
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

    /// The distinction that stops silent no-ops from shipping.
    ///
    /// It has to be asserted through the table itself. The first version of
    /// this test hand-built one `Capability` and asserted it equalled itself,
    /// so nothing about it was connected to the probe table: flipping
    /// `observable` back to `true` — the exact bug the flag was introduced for
    /// — made no difference to it, and the real regression it claimed to guard
    /// would have passed straight through.
    #[test]
    fn unobservable_features_are_not_reported_as_plain_yes() {
        // Which features are in that class is a judgement about the endpoint,
        // not a property the code can derive, so it is written out here: each
        // of these is accepted with a 200 while the effect it asks about
        // cannot be read off that response.
        const UNOBSERVABLE: [&str; 7] = [
            "output_config.effort",
            "output_config.format (structured outputs)",
            "cache_control",
            "context_management.edits",
            "server-side compaction",
            "task_budget",
            "server-side fallback",
        ];

        let table = probes();
        for feature in UNOBSERVABLE {
            let probe = table
                .iter()
                .find(|p| p.feature == feature)
                .unwrap_or_else(|| panic!("`{feature}` is no longer in the probe table"));
            assert!(
                !probe.observable,
                "`{feature}` is marked observable, but one response cannot show it working; \
                 a 200 would be reported as a plain Yes"
            );
            let support = acceptance_support(probe);
            assert_eq!(
                support,
                Support::AcceptedUnverified,
                "`{feature}` must report accepted-but-unverified"
            );
            assert!(!support.is_yes(), "`{feature}` was reported as a capability");
        }

        // The mapping must still say Yes where the response *is* the evidence,
        // or the rule above would be satisfied by a constant.
        for probe in table.iter().filter(|p| p.observable) {
            assert_eq!(
                acceptance_support(probe),
                Support::Yes,
                "`{}` is observable; its 200 is the evidence",
                probe.feature
            );
        }
    }

    #[test]
    fn quirks_are_inferred_from_measurements_not_from_the_hostname() {
        let entries = vec![
            Capability::ok("tools.anthropic_defined.bash_20250124", Support::No, 422, "nope"),
            Capability::ok("tool_choice.any", Support::Yes, 200, "ok"),
        ];
        let q = Capabilities::infer_quirks(
            "https://some-proxy.test",
            &entries,
            CachingProbe { breakpoints: Support::No, automatic: Support::Yes },
            false,
            true,
            false,
        );
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
        let q = Capabilities::infer_quirks(
            "https://api.anthropic.com",
            &entries,
            CachingProbe { breakpoints: Support::Yes, automatic: Support::No },
            true,
            false,
            true,
        );
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

    // -----------------------------------------------------------------------
    // The caching probe
    //
    // Its answer exists only as the *difference* between two requests, so it
    // cannot be tested against a hand-built value — that would assert nothing
    // about what was sent. These tests use a loopback endpoint that answers in
    // the shape the probe reads, and records what it was asked.
    // -----------------------------------------------------------------------

    fn loopback_listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").expect("a loopback port")
    }

    /// Read one complete HTTP/1.1 request: the head, then `content-length` bytes.
    fn read_request(sock: &mut TcpStream) -> String {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            if let Some(head_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
                let body_len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() >= head_end + 4 + body_len {
                    break;
                }
            }
            match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
            }
        }
        String::from_utf8_lossy(&raw).into_owned()
    }

    /// Answers like an Anthropic-shaped endpoint: a prefix is cached **only
    /// when the request carries a `cache_control` breakpoint**, and requests
    /// without one report no cache activity at all.
    ///
    /// That is the case the probe has to get right, and the one it could not
    /// see while it never sent a breakpoint.
    fn breakpoint_honouring_endpoint() -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
        let listener = loopback_listener();
        let addr = listener.local_addr().expect("listener address");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut written = false;
            // Four requests: two per arm of `probe_caching`.
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let raw = read_request(&mut sock);
                let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let usage = if body.contains("cache_control") {
                    if written {
                        json!({"cache_read_input_tokens": 5000})
                    } else {
                        written = true;
                        json!({"cache_creation_input_tokens": 5000})
                    }
                } else {
                    json!({})
                };
                let reply = format!(
                    r#"{{"id":"msg-loopback","model":"m","content":[],"stop_reason":"end_turn","usage":{usage}}}"#
                );
                let _ = sock.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
                let _ = tx.send(raw);
            }
        });
        (addr, rx)
    }

    /// `body(with_breakpoint)` was defined but only `body(false)` was ever
    /// called, so the probe answered "are breakpoints effective?" from requests
    /// that carried none: on this endpoint that is a `No` about a question
    /// never asked, and `infer_quirks` writes it over the correct default.
    #[tokio::test]
    async fn the_caching_probe_tests_breakpoints_against_a_breakpoint_honouring_endpoint() {
        let (addr, seen) = breakpoint_honouring_endpoint();
        let client = WireClient::builder()
            .base_url(format!("http://{addr}"))
            .auth(Auth::ApiKey("sk-test".to_owned()))
            .build()
            .expect("building the client");

        let (breakpoints, automatic) = probe_caching(&client, "test-model").await;

        // What went out on the wire, because the point is that the breakpoint
        // arm exists at all: a "yes" below would otherwise be an answer to a
        // question the probe never asked.
        let mut bodies = Vec::new();
        while let Ok(raw) = seen.recv_timeout(Duration::from_secs(5)) {
            bodies.push(raw);
        }
        assert!(
            bodies.iter().any(|b| b.contains("cache_control")),
            "no request carried a breakpoint; the probe never exercised the path \
             ({} requests seen, none of them with one)",
            bodies.len()
        );

        // And the answer is the endpoint's, not a default: this endpoint caches
        // only when asked, so breakpoints are effective and nothing is
        // automatic.
        assert_eq!(
            breakpoints,
            Support::Yes,
            "a breakpoint-honouring endpoint must not be reported as inert"
        );
        assert_eq!(automatic, Support::No);
    }
}

/// What comparing two requests revealed about caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachingProbe {
    /// Do explicit `cache_control` breakpoints change anything?
    pub breakpoints: Support,
    /// Does the server cache prefixes on its own?
    pub automatic: Support,
}

/// Compare two identical cacheable requests to find out what caching actually
/// does here.
///
/// DESIGN calls the byte prefix an iron law, and on this endpoint it is the
/// *entire* mechanism rather than a hint to a breakpoint placer. That is only
/// knowable by sending the same prefix twice and reading `cache_read_input_tokens`
/// on the second — which is why one 200 could never have answered it.
///
/// Two arms, because "does the server cache prefixes by itself" and "does a
/// breakpoint change anything" are different questions: a server may cache
/// automatically (a breakpoint then adds nothing) or only on request (the
/// breakpoint is the whole mechanism). Only the *difference* between the arms
/// answers the second question.
async fn probe_caching(client: &WireClient, model: &str) -> (Support, Support) {
    // Long enough to clear any minimum cacheable prefix, and byte-identical
    // between the two requests in an arm.
    let prefix = "You are a meticulous assistant. ".repeat(200);

    let plain = caching_arm(client, model, &prefix, false).await;
    let explicit = caching_arm(client, model, &prefix, true).await;

    // Automatic: with no breakpoint anywhere in the request, a cache read on
    // the second identical request can only be the server deciding to cache on
    // its own.
    let automatic = if !plain.second_ok {
        Support::Unknown
    } else if plain.read_second > 0 {
        Support::Yes
    } else {
        Support::No
    };

    // Breakpoints: effective only if asking for one produced cache activity the
    // unadorned requests did not. On an endpoint that caches by itself both
    // arms behave identically and the breakpoint is inert — accepted, and doing
    // nothing (this endpoint's measured Quirk 3). Reading this answer off the
    // no-breakpoint arm — which is all this probe did while `body(true)` was
    // dead code — can only ever say `No`, and on a breakpoint-honouring
    // endpoint that `No` overwrites the correct default.
    let breakpoints = if !plain.second_ok || !explicit.second_ok {
        // One arm is not evidence about the other. An unmeasured answer must
        // not be written into the profile as a measurement.
        Support::Unknown
    } else if explicit.cached() && !plain.cached() {
        Support::Yes
    } else {
        Support::No
    };

    (breakpoints, automatic)
}

fn caching_body(model: &str, prefix: &str, with_breakpoint: bool) -> Value {
    let system = if with_breakpoint {
        json!([{"type": "text", "text": prefix, "cache_control": {"type": "ephemeral"}}])
    } else {
        json!([{"type": "text", "text": prefix}])
    };
    json!({
        "model": model,
        "max_tokens": 16,
        "system": system,
        "messages": [{"role": "user", "content": "Say ok"}],
    })
}

/// One arm of [`probe_caching`]: the same prefix sent twice, with or without a
/// `cache_control` breakpoint.
#[derive(Debug, Default)]
struct CacheArm {
    /// The second request came back 2xx, so its cache counters mean something.
    second_ok: bool,
    /// The server wrote cache on a request in this arm.
    wrote: bool,
    /// `cache_read_input_tokens` on the second request.
    read_second: u64,
}

impl CacheArm {
    /// The server cached this prefix: it wrote something, or read something
    /// back. The two arms' answers are only meaningful as a difference.
    fn cached(&self) -> bool {
        self.wrote || self.read_second > 0
    }
}

async fn caching_arm(
    client: &WireClient,
    model: &str,
    prefix: &str,
    with_breakpoint: bool,
) -> CacheArm {
    let body = caching_body(model, prefix, with_breakpoint);
    let mut arm = CacheArm::default();
    for attempt in 0..2 {
        let Ok((status, text, _)) = client
            .raw_request(reqwest::Method::POST, "/v1/messages", Some(&body), &[])
            .await
        else {
            continue;
        };
        if !(200..300).contains(&status) {
            continue;
        }
        if cache_created(&text).unwrap_or(0) > 0 {
            arm.wrote = true;
        }
        if attempt == 1 {
            arm.second_ok = true;
            arm.read_second = cache_read(&text).unwrap_or(0);
        }
    }
    arm
}

fn cache_read(text: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.get("usage")?.get("cache_read_input_tokens")?.as_u64()
}

fn cache_created(text: &str) -> Option<u64> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.get("usage")?
        .get("cache_creation_input_tokens")?
        .as_u64()
}

/// Does the endpoint check a thinking block's signature?
///
/// Quirk 1 established by hand, over sixteen cases, that a fabricated tool-call
/// id requires a thinking block but that the block's `signature` is not verified.
/// That fact is what makes M3's fork and replay feasible — a forked branch can
/// carry synthesised thinking. It lived only in a hand-run script, so nothing
/// would have noticed if the endpoint started checking.
async fn probe_signature(client: &WireClient, model: &str) -> bool {
    let tools = json!([{
        "name": "get_time",
        "description": "Return the current time.",
        "input_schema": {"type": "object", "properties": {"tz": {"type": "string"}}, "required": ["tz"]}
    }]);
    // A deliberately impossible signature. If the endpoint cares, it says so.
    let body = json!({
        "model": model,
        "max_tokens": 32,
        "tools": tools,
        "messages": [
            {"role": "user", "content": "What time is it in Tokyo?"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I should call the time tool.",
                 "signature": "00000000-0000-0000-0000-000000000000"},
                {"type": "tool_use", "id": "call_00_probe0000000000000000",
                 "name": "get_time", "input": {"tz": "Asia/Tokyo"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_00_probe0000000000000000",
                 "content": "14:32 JST"}
            ]}
        ]
    });
    match client
        .raw_request(reqwest::Method::POST, "/v1/messages", Some(&body), &[])
        .await
    {
        // Accepted a signature that cannot be genuine.
        Ok((status, _, _)) if (200..300).contains(&status) => false,
        // Refused it.
        Ok(_) => true,
        // Could not tell. Conservatively assume it is checked, because assuming
        // otherwise would make a fork silently produce a corrupt turn.
        Err(_) => true,
    }
}
