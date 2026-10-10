//! Error classification.
//!
//! DESIGN: "不建 provider 抽象层,但要为 quirks 留一层." Errors are part of that
//! layer, because endpoints disagree about *which status code* means what.
//!
//! The measured endpoint distinguishes two failure classes that Anthropic folds
//! into one — see [`ErrorKind::Unsupported`].

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ErrorKind {
    /// Connection, DNS, TLS, timeout. Retryable.
    Transport,
    /// 401 / 403. Not retryable — the credential is wrong.
    Auth,
    /// 429. Retryable after the indicated delay.
    RateLimited { retry_after_ms: Option<u64> },
    /// 529 or an explicit overload signal. Retryable.
    Overloaded,
    /// 5xx. Retryable.
    ServerError,
    /// 400 `invalid_request_error`. Not retryable, and it *is* about our request.
    InvalidRequest,
    /// 422: the endpoint does not model a field we sent.
    ///
    /// **Measured as distinct from 400** (see `docs/WIRE-FACTS.md` Quirk 5). It
    /// is not retryable, and it is not a prompt bug — it means this endpoint
    /// cannot express what we asked for. Conflating it with `InvalidRequest`
    /// would send whoever debugs it looking in the wrong place.
    Unsupported,
    /// 404 — endpoint or resource absent.
    NotFound,
    /// The response arrived but could not be understood.
    MalformedResponse,
    /// SSE `error` event midway through a stream.
    StreamError,
    /// Token counting failed.
    CountTokens,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub message: String,
    pub request_id: Option<String>,
    /// For [`ErrorKind::Unsupported`]: the variants the endpoint said it accepts.
    ///
    /// A 422 body enumerates them ("unknown variant `x`, expected `a` or `b`"),
    /// which makes this the cheapest possible capability probe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_variants: Vec<String>,
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.kind)?;
        if let Some(s) = self.status {
            write!(f, " (HTTP {s})")?;
        }
        write!(f, ": {}", self.message)?;
        if !self.expected_variants.is_empty() {
            write!(f, " [accepts: {}]", self.expected_variants.join(", "))?;
        }
        if let Some(id) = &self.request_id {
            write!(f, " (request_id: {id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for WireError {}

impl WireError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            message: message.into(),
            request_id: None,
            expected_variants: Vec::new(),
        }
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transport, message)
    }

    /// Whether retrying the identical request could plausibly succeed.
    ///
    /// `Unsupported` and `InvalidRequest` are deliberately absent: retrying a
    /// schema the endpoint cannot deserialize will fail identically forever.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Transport
                | ErrorKind::RateLimited { .. }
                | ErrorKind::Overloaded
                | ErrorKind::ServerError
        )
    }

    /// Build from an HTTP status and a parsed error body.
    pub fn from_response(status: u16, body: &str, retry_after_ms: Option<u64>) -> Self {
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        let err = parsed.get("error");
        let message = err
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if body.trim().is_empty() {
                    format!("HTTP {status} with an empty body")
                } else {
                    body.chars().take(400).collect()
                }
            });

        // `request_id` appears either inside the message or alongside it.
        let request_id = parsed
            .get("request_id")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| extract_request_id(&message));

        let kind = match status {
            400 => ErrorKind::InvalidRequest,
            401 | 403 => ErrorKind::Auth,
            404 => ErrorKind::NotFound,
            422 => ErrorKind::Unsupported,
            429 => ErrorKind::RateLimited { retry_after_ms },
            529 => ErrorKind::Overloaded,
            s if (500..600).contains(&s) => ErrorKind::ServerError,
            _ => ErrorKind::InvalidRequest,
        };

        let expected_variants = if kind == ErrorKind::Unsupported {
            parse_expected_variants(&message)
        } else {
            Vec::new()
        };

        Self {
            kind,
            status: Some(status),
            message,
            request_id,
            expected_variants,
        }
    }
}

/// Pull `(request_id: ...)` out of a message that embeds it.
fn extract_request_id(message: &str) -> Option<String> {
    let start = message.find("request_id: ")? + "request_id: ".len();
    let rest = &message[start..];
    let end = rest
        .find(|c: char| c == ')' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_owned())
}

/// Parse the variant list out of a serde deserialization error.
///
/// Measured shape:
/// ```text
/// unknown variant `bash_20250124`, expected `web_search_20250305` or `web_search_20260209`
/// ```
pub fn parse_expected_variants(message: &str) -> Vec<String> {
    let Some(pos) = message.find("expected ") else {
        return Vec::new();
    };
    let tail = &message[pos + "expected ".len()..];
    // Stop at the serde position suffix if present.
    let tail = tail.split(" at line").next().unwrap_or(tail);

    let mut out = Vec::new();
    let mut rest = tail;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else { break };
        out.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unparseable_tool_type_is_unsupported_not_invalid_request() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Failed to deserialize the JSON body into the target type: tools[0]: unknown variant `bash_20250124`, expected `web_search_20250305` or `web_search_20260209` at line 1 column 94 (request_id: 28c13699-22d5-4d57-b958-b84294704c72)"}}"#;
        let e = WireError::from_response(422, body, None);
        assert_eq!(e.kind, ErrorKind::Unsupported);
        assert!(!e.is_retryable(), "a 422 will fail identically forever");
        assert_eq!(
            e.expected_variants,
            vec!["web_search_20250305", "web_search_20260209"]
        );
        assert_eq!(
            e.request_id.as_deref(),
            Some("28c13699-22d5-4d57-b958-b84294704c72")
        );
    }

    #[test]
    fn rate_limit_is_retryable_and_carries_its_delay() {
        let body = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let e = WireError::from_response(429, body, Some(1500));
        assert!(e.is_retryable());
        assert_eq!(e.kind, ErrorKind::RateLimited { retry_after_ms: Some(1500) });
    }

    #[test]
    fn auth_failure_is_not_retryable() {
        // Measured: this endpoint's 401 embeds the last 4 chars of the key.
        let body = r#"{"error":{"type":"authentication_error","message":"Authentication Fails, Your api key: ****0000 is invalid"}}"#;
        let e = WireError::from_response(401, body, None);
        assert_eq!(e.kind, ErrorKind::Auth);
        assert!(!e.is_retryable());
    }

    #[test]
    fn server_errors_are_retryable() {
        for status in [500, 502, 503, 529] {
            let e = WireError::from_response(status, "{}", None);
            assert!(e.is_retryable(), "{status} should be retryable");
        }
    }

    #[test]
    fn empty_body_still_produces_a_message() {
        let e = WireError::from_response(500, "", None);
        assert!(e.message.contains("500"));
    }

    #[test]
    fn variant_parsing_handles_a_single_variant() {
        let v = parse_expected_variants("unknown variant `x`, expected `only_one` at line 1 column 2");
        assert_eq!(v, vec!["only_one"]);
    }
}
