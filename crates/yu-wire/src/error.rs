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

        // Every copy of `message` — logged, journaled, serialized — is made
        // from here, so this is the only place key material has to be stopped.
        let message = redact_key_material(&message);

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

/// Remove key material an error body echoed back, before it is stored anywhere.
///
/// WIRE-FACTS Quirk 6 measured the shape this exists for:
///
/// ```text
/// Authentication Fails, Your api key: ****0000 is invalid
/// ```
///
/// The endpoint masks the key but prints the characters the mask preserves —
/// the key's real last four. `message` is public and is copied into `Display`,
/// `Debug`, serde output and the capability profile, so the credentials are
/// stripped at ingestion, where every copy is made from, rather than in each
/// consumer: a consumer that forgets cannot leak what was never stored, and a
/// consumer that does not exist yet cannot either.
pub fn redact_key_material(message: &str) -> String {
    redact_named_values(&redact_masked_suffix(message))
}

/// Longest run of characters that can be the preserved tail of a masked key.
///
/// Measured as four (`****0000`). Asterisks followed by something longer are a
/// word that happens to follow asterisks, not a key's tail, and are left alone
/// — redaction that eats prose trades one failure for another.
const MAX_MASKED_SUFFIX: usize = 8;

/// The characters a key, or what is left of one, can be made of.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Drop what a `****` mask preserves from the key behind it.
///
/// `Your api key: ****0000` loses `0000`; the mask itself stays, because the
/// mask is what says a credential was involved, and it is not key material.
fn redact_masked_suffix(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '*' || chars.get(i + 1) != Some(&'*') {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let mut stars = i;
        while chars.get(stars) == Some(&'*') {
            stars += 1;
        }
        out.extend(chars[i..stars].iter());

        let mut end = stars;
        while chars.get(end).is_some_and(|c| is_key_char(*c)) {
            end += 1;
        }
        if end - stars > MAX_MASKED_SUFFIX {
            out.extend(chars[stars..end].iter());
        }
        i = end;
    }
    out
}

/// Words this endpoint uses to name the credential it is reporting on.
const KEY_PHRASES: [&str; 3] = ["api key", "api_key", "apikey"];

/// Replace a credential written in the clear after a key-naming phrase.
///
/// The measured body masks the key, but masking is the endpoint's choice and
/// not something a client may rely on; keyed on the name rather than the mask,
/// a body that prints the value plainly is redacted as well.
fn redact_named_values(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        let Some((value_start, value_end)) = named_value(&lower, &chars, i) else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        out.extend(chars[i..value_start].iter());
        // The replacement is the endpoint's own marker: it keeps the sentence
        // readable and does not pretend the key is still inspectable.
        out.push_str("****");
        i = value_end;
    }
    out
}

/// If a key-naming phrase starts at `i`, return the half-open range of the
/// credential following it — the phrase, then `:` or `=`, then the value.
///
/// A value that is already a mask is [`redact_masked_suffix`]'s business, and
/// reporting the range here would strip the mask too.
fn named_value(lower: &[char], chars: &[char], i: usize) -> Option<(usize, usize)> {
    let phrase_len = KEY_PHRASES
        .iter()
        .find(|p| starts_with_ascii(lower, i, p))?
        .chars()
        .count();
    let mut j = i + phrase_len;
    while chars.get(j) == Some(&' ') {
        j += 1;
    }
    if !matches!(chars.get(j), Some(':') | Some('=')) {
        return None;
    }
    j += 1;
    while chars.get(j) == Some(&' ') {
        j += 1;
    }
    let value_start = j;
    while chars.get(j).is_some_and(|c| is_key_char(*c)) {
        j += 1;
    }
    (j > value_start && chars.get(value_start) != Some(&'*')).then_some((value_start, j))
}

fn starts_with_ascii(lower: &[char], i: usize, needle: &str) -> bool {
    let mut j = i;
    for c in needle.chars() {
        if lower.get(j) != Some(&c) {
            return false;
        }
        j += 1;
    }
    true
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
    fn an_auth_failure_does_not_keep_the_key_characters_the_mask_preserved() {
        // The captured body, not a paraphrase of it: the mask is real and so is
        // the suffix behind it (WIRE-FACTS Quirk 6).
        let body = include_str!("../tests/fixtures/error_401_auth.json");
        let e = WireError::from_response(401, body, None);

        // What the message is for survives: the failure, and where to look it
        // up. Redaction that costs diagnosis is not paid for by dropping the
        // whole message.
        assert!(e.message.contains("Authentication Fails"), "{}", e.message);
        assert!(e.message.contains("****"), "{}", e.message);
        assert_eq!(
            e.request_id.as_deref(),
            Some("da1437c0-3fdc-4777-a104-e06d9c918f1d")
        );

        // Every form a consumer would copy or persist is free of the suffix.
        let copies = [
            ("message", e.message.clone()),
            ("Display", format!("{e}")),
            ("Debug", format!("{e:?}")),
            ("serde", serde_json::to_string(&e).unwrap()),
        ];
        for (shape, text) in copies {
            assert!(
                !text.contains("0000"),
                "key characters reached {shape}: {text}"
            );
        }
    }

    #[test]
    fn a_key_written_in_the_clear_is_replaced_by_the_mask() {
        // Masking is the endpoint's choice, not a client guarantee; the
        // redaction is keyed on the name so an unmasked echo is covered too.
        let body = r#"{"error":{"type":"authentication_error","message":"Invalid API key: sk-live-abc123 for this endpoint (request_id: r1)"}}"#;
        let e = WireError::from_response(401, body, None);
        assert!(!e.message.contains("sk-live-abc123"), "{}", e.message);
        assert!(e.message.contains("request_id: r1"), "{}", e.message);
    }

    #[test]
    fn redaction_leaves_a_message_that_names_no_key_alone() {
        // The 422 message is the diagnostically dense one — variant list and
        // all — and must come through untouched.
        let raw = "Failed to deserialize the JSON body into the target type: tools[0]: \
                   unknown variant `bash_20250124`, expected `web_search_20250305` at line 1 column 94";
        let body = format!(r#"{{"error":{{"message":"{raw}"}}}}"#);
        let e = WireError::from_response(422, &body, None);
        assert_eq!(e.message, raw);
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
