//! HTTP client: messages, streaming, retry.
//!
//! DESIGN's process model section is about avoiding a subprocess per step; this
//! module is the other half of that — a resident client that reuses one
//! connection pool and one process for the whole run.
//!
//! Timing is emitted as spans as the request proceeds, so `wire_wait`,
//! `first_token` and `stream` are measured at the only place they can be
//! measured: where the bytes arrive.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;

use yu_observe::{SpanKind, Timeline};

use crate::error::{ErrorKind, WireError};
use crate::quirks::Quirks;
use crate::sse::{SseEvent, SseParser, StreamAccumulator, StreamedMessage};
use crate::types::{ContentBlock, MessagesRequest, MessagesResponse};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const MESSAGES_PATH: &str = "/v1/messages";
pub const COUNT_TOKENS_PATH: &str = "/v1/messages/count_tokens";

/// Cap on how much of a response is retained before it proves to be SSE. Only
/// ever holds a body that turned out not to be a stream, so this bounds a
/// pathological case rather than a normal one.
const MAX_BUFFERED_BODY: usize = 32 * 1024 * 1024;

/// How credentials go on the wire.
///
/// Measured: the probe endpoint accepts the token in `x-api-key`. Anthropic
/// itself uses `x-api-key` for API keys and `Authorization: Bearer` for OAuth
/// tokens, so both are modelled rather than assumed.
#[derive(Debug, Clone)]
pub enum Auth {
    ApiKey(String),
    Bearer(String),
}

impl Auth {
    fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::ApiKey(k) => rb.header("x-api-key", k),
            Auth::Bearer(t) => rb.header("authorization", format!("Bearer {t}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff, capped. `attempt` is 1-based.
    fn delay(&self, attempt: u32, retry_after: Option<u64>) -> Duration {
        if let Some(ms) = retry_after {
            return Duration::from_millis(ms.min(self.max_delay_ms));
        }
        let exp = self.base_delay_ms.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(6));
        Duration::from_millis(exp.min(self.max_delay_ms))
    }
}

/// Timing observed while streaming one response.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StreamTiming {
    /// Time to response headers.
    pub first_byte_ns: Option<u64>,
    /// Time to the first content token — text *or* thinking.
    ///
    /// DESIGN wants the distinction between "the model is thinking" and "the
    /// harness is slow"; that requires counting thinking deltas, because on the
    /// measured endpoint every response opens with a thinking block.
    pub first_token_ns: Option<u64>,
    pub total_ns: u64,
    pub events: u64,
    pub text_bytes: usize,
    /// DESIGN's L2 constraint 4 exists because this can rival the text it
    /// accompanies. Now it is measured rather than asserted.
    pub signature_bytes: usize,
}

/// Receives incremental output as it arrives.
///
/// A trait rather than a closure so a sink can hold state without the caller
/// fighting higher-ranked lifetimes.
pub trait StreamSink {
    fn on_text(&mut self, _text: &str) {}
    fn on_thinking(&mut self, _text: &str) {}
    fn on_tool_use(&mut self, _id: &str, _name: &str) {}
    fn on_block(&mut self, _block: &ContentBlock) {}
}

/// Discards everything. For when only the final message matters.
pub struct NullSink;

impl StreamSink for NullSink {}

#[derive(Debug, Clone)]
pub struct StreamOutcome {
    pub message: StreamedMessage,
    pub timing: StreamTiming,
    pub attempts: u32,
}

pub struct WireClient {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    quirks: Quirks,
    retry: RetryPolicy,
    betas: Vec<String>,
    timeline: Option<Arc<Timeline>>,
    default_model: String,
}

impl std::fmt::Debug for WireClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireClient")
            .field("base_url", &self.base_url)
            .field("auth", &match self.auth {
                Auth::ApiKey(_) => "ApiKey(<redacted>)",
                Auth::Bearer(_) => "Bearer(<redacted>)",
            })
            .field("quirks", &self.quirks.name)
            .field("betas", &self.betas)
            .finish_non_exhaustive()
    }
}

impl WireClient {
    pub fn builder() -> WireClientBuilder {
        WireClientBuilder::default()
    }

    /// Configure from the environment.
    ///
    /// Reads `ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`,
    /// and `ANTHROPIC_MODEL`. Credentials are never logged — see the `Debug`
    /// impl, which redacts them.
    pub fn from_env() -> Result<Self, WireError> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| "https://api.anthropic.com".to_owned());
        let auth = if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(key)
        } else if let Ok(token) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
            // Measured: the probe endpoint accepts the auth token via x-api-key.
            Auth::ApiKey(token)
        } else {
            return Err(WireError::new(
                ErrorKind::Auth,
                "no credential: set ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN",
            ));
        };
        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-opus-5".to_owned());
        // A measured profile, if one was saved. This is DESIGN's "约束靠注入":
        // `yu probe --json` writes the report, this reads it back, and the
        // constraints in force are the ones that were actually observed rather
        // than the ones the code assumed.
        let quirks = match std::env::var("YU_QUIRKS_JSON") {
            Ok(path) => match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| Quirks::from_capabilities_json(&text))
            {
                Ok(quirks) => quirks,
                Err(problem) => {
                    // Loud, and fatal to the profile rather than to the run: a
                    // profile that failed to load must not silently become the
                    // built-in default, because then a run would proceed under
                    // constraints nobody chose.
                    tracing::error!(path = %path, %problem, "could not load the quirks profile; \
                        falling back to an unmeasured profile, which assumes the strict rules");
                    Quirks::unknown()
                }
            },
            Err(_) => Quirks::from_name(&std::env::var("YU_QUIRKS").unwrap_or_default())
                .unwrap_or_else(Quirks::unknown),
        };
        Self::builder()
            .base_url(base_url)
            .auth(auth)
            .quirks(quirks)
            .default_model(model)
            .build()
    }

    pub fn quirks(&self) -> &Quirks {
        &self.quirks
    }

    /// Attach a timeline, so the wire layer records its own spans.
    ///
    /// Without this `wire_wait` / `first_token` / `stream` are never emitted and
    /// the harness-overhead metric silently reads zero model time — a
    /// measurement that looks like a result.
    pub fn with_timeline(mut self, timeline: Arc<Timeline>) -> Self {
        self.timeline = Some(timeline);
        self
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        betas: &[String],
    ) -> reqwest::RequestBuilder {
        let rb = self
            .http
            .request(method, self.url(path))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json");
        let rb = self.auth.apply(rb);
        if betas.is_empty() {
            rb
        } else {
            rb.header("anthropic-beta", betas.join(","))
        }
    }

    /// Send without retry or classification, returning the raw status and body.
    ///
    /// Used by the capability prober, which must observe status codes rather
    /// than have them interpreted, and by `count_tokens`, which needs to attach
    /// betas per request rather than from client configuration.
    pub(crate) async fn raw_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        betas: &[String],
    ) -> Result<(u16, String, Option<u64>), WireError> {
        let mut rb = self.request(method, path, betas);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.map_err(|e| WireError::transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| secs * 1000);
        let text = resp
            .text()
            .await
            .map_err(|e| WireError::transport(e.to_string()))?;
        Ok((status, text, retry_after))
    }

    /// Non-streaming request, with retry on retryable failures.
    pub async fn send(&self, req: &MessagesRequest) -> Result<MessagesResponse, WireError> {
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let timer = self.timeline.as_ref().map(|t| t.start(SpanKind::WireWait));
            let outcome = self.send_once(req).await;
            if let Some(t) = timer {
                let span = t.finish();
                tracing::debug!(elapsed_ms = span.elapsed_ms(), "wire_wait");
            }
            match outcome {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if !e.is_retryable() || attempts >= self.retry.max_attempts {
                        return Err(e);
                    }
                    let delay = self.retry.delay(attempts, retry_after_of(&e));
                    tracing::warn!(
                        attempt = attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        "retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn send_once(&self, req: &MessagesRequest) -> Result<MessagesResponse, WireError> {
        let resp = self
            .request(reqwest::Method::POST, MESSAGES_PATH, &self.betas)
            .json(req)
            .send()
            .await
            .map_err(|e| WireError::transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|secs| secs * 1000);
        let body = resp
            .text()
            .await
            .map_err(|e| WireError::transport(e.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(WireError::from_response(status, &body, retry_after));
        }
        serde_json::from_str(&body).map_err(|e| {
            WireError::new(
                ErrorKind::MalformedResponse,
                format!("response did not parse: {e}; body began: {}", preview(&body)),
            )
        })
    }

    /// Streaming request.
    ///
    /// Retry is only attempted when nothing has been emitted yet. A stream that
    /// has already delivered content cannot be safely restarted without
    /// duplicating that content downstream, so a mid-stream failure is returned
    /// rather than retried.
    pub async fn stream<S: StreamSink>(
        &self,
        req: &MessagesRequest,
        sink: &mut S,
    ) -> Result<StreamOutcome, WireError> {
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            match self.stream_once(req, sink).await {
                Ok(mut outcome) => {
                    outcome.attempts = attempts;
                    return Ok(outcome);
                }
                Err(e) => {
                    let emitted = e.emitted_content;
                    if !e.error.is_retryable() || emitted || attempts >= self.retry.max_attempts {
                        return Err(e.error);
                    }
                    let delay = self.retry.delay(attempts, retry_after_of(&e.error));
                    tracing::warn!(
                        attempt = attempts,
                        delay_ms = delay.as_millis() as u64,
                        "retrying stream before any content was emitted"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn stream_once<S: StreamSink>(
        &self,
        req: &MessagesRequest,
        sink: &mut S,
    ) -> Result<StreamOutcome, StreamFailure> {
        let started = Instant::now();
        let mut timing = StreamTiming::default();

        // Force `stream: true` rather than trusting the caller to have set it.
        // A method named `stream` that quietly returns a non-streaming body is
        // a footgun, and this endpoint answers a non-streaming request with a
        // plain JSON message under a 200 — which parses to zero SSE frames and
        // would otherwise surface as a turn with no `stop_reason`.
        let mut req = req.clone();
        req.stream = Some(true);

        let wire_timer = self
            .timeline
            .as_ref()
            .map(|t| t.start(SpanKind::WireWait));

        let resp = self
            .request(reqwest::Method::POST, MESSAGES_PATH, &self.betas)
            .json(&req)
            .send()
            .await
            .map_err(|e| StreamFailure::new(WireError::transport(e.to_string())))?;

        let status = resp.status().as_u16();
        if let Some(t) = wire_timer {
            t.finish();
        }
        if !(200..300).contains(&status) {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|secs| secs * 1000);
            let body = resp.text().await.unwrap_or_default();
            return Err(StreamFailure::new(WireError::from_response(
                status, &body, retry_after,
            )));
        }

        timing.first_byte_ns = Some(started.elapsed().as_nanos() as u64);

        // Started here and finished after the loop. Creating it after the loop
        // and finishing immediately measures nothing — which is what this did
        // until an end-to-end run showed `stream=0.0ms` on a 3.5 s request.
        let stream_timer = self
            .timeline
            .as_ref()
            .map(|t| t.start(SpanKind::Stream));

        let mut parser = SseParser::new();
        let mut acc = StreamAccumulator::new();
        let mut stream = resp.bytes_stream();
        let mut emitted = false;
        // Buffered only until the first SSE frame proves this really is a
        // stream, so the normal path pays nothing — but a response that turns
        // out to be plain JSON is still recoverable rather than lost.
        let mut buffered: Vec<u8> = Vec::new();
        let mut saw_sse = false;
        // Track which blocks have been announced so `on_tool_use` fires once.
        let mut announced: std::collections::HashSet<usize> = std::collections::HashSet::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    return Err(StreamFailure {
                        error: WireError::transport(e.to_string()),
                        emitted_content: emitted,
                    });
                }
            };
            if !saw_sse && buffered.len() < MAX_BUFFERED_BODY {
                buffered.extend_from_slice(&chunk);
            }
            let frames = parser.push(&chunk);
            if !frames.is_empty() {
                saw_sse = true;
                buffered.clear();
            }
            for ev in frames {
                timing.events += 1;
                self.dispatch(&ev, sink, &mut timing, &mut announced, &mut emitted, started);
                if let Err(e) = acc.apply(&ev) {
                    return Err(StreamFailure {
                        error: e,
                        emitted_content: emitted,
                    });
                }
            }
        }
        if let Some(ev) = parser.finish() {
            timing.events += 1;
            self.dispatch(&ev, sink, &mut timing, &mut announced, &mut emitted, started);
            if let Err(e) = acc.apply(&ev) {
                return Err(StreamFailure {
                    error: e,
                    emitted_content: emitted,
                });
            }
        }

        // No SSE frames at all. Either the server ignored `stream: true`, or
        // this is a response we do not understand. A complete message body is
        // accepted rather than discarded — a proxy that drops the flag is a real
        // thing, and the alternative is reporting a finished turn as a
        // malformed one.
        if timing.events == 0 {
            let body = String::from_utf8_lossy(&buffered);
            match serde_json::from_str::<MessagesResponse>(&body) {
                Ok(resp) => {
                    for block in &resp.content {
                        sink.on_block(block);
                    }
                    if let Some(text) = resp.content.iter().find_map(|b| b.as_text()) {
                        sink.on_text(text);
                    }
                    timing.total_ns = started.elapsed().as_nanos() as u64;
                    return Ok(StreamOutcome {
                        message: StreamedMessage {
                            id: resp.id,
                            model: resp.model,
                            content: resp.content,
                            stop_reason: resp.stop_reason,
                            stop_sequence: resp.stop_sequence,
                            usage: resp.usage,
                        },
                        timing,
                        attempts: 1,
                    });
                }
                Err(e) => {
                    return Err(StreamFailure {
                        error: WireError::new(
                            ErrorKind::MalformedResponse,
                            format!(
                                "HTTP 200 but the body was neither SSE nor a message: {e}; \
                                 body began: {}",
                                body.chars().take(300).collect::<String>()
                            ),
                        ),
                        emitted_content: false,
                    });
                }
            }
        }

        timing.total_ns = started.elapsed().as_nanos() as u64;
        timing.text_bytes = acc.text_len;
        timing.signature_bytes = acc.signature_bytes;

        if let Some(timer) = stream_timer {
            let span = timer
                .attr("events", timing.events as i64)
                .attr("text_bytes", timing.text_bytes as i64)
                .attr("signature_bytes", timing.signature_bytes as i64)
                .finish();
            tracing::debug!(elapsed_ms = span.elapsed_ms(), "stream");
        }

        // `FirstToken` as a span, not merely a field. DESIGN lists it among the
        // spans, and the distinction it draws — "the model is thinking" versus
        // "the harness is slow" — is only legible if this number sits beside the
        // harness spans in the same report.
        if let Some(first_token_ns) = timing.first_token_ns
            && let Some(timeline) = &self.timeline
        {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("events".to_owned(), serde_json::json!(timing.events));
            timeline.record_measured(
                SpanKind::FirstToken,
                timeline.current_agent(),
                first_token_ns,
                attrs,
            );
        }

        Ok(StreamOutcome {
            message: acc.finish(),
            timing,
            attempts: 1,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch<S: StreamSink>(
        &self,
        ev: &SseEvent,
        sink: &mut S,
        timing: &mut StreamTiming,
        announced: &mut std::collections::HashSet<usize>,
        emitted: &mut bool,
        started: Instant,
    ) {
        let Ok(payload) = ev.json() else { return };
        match ev.kind() {
            "content_block_start" => {
                let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(block) = payload.get("content_block")
                    && let (Some(id), Some(name)) = (
                        block.get("id").and_then(Value::as_str),
                        block.get("name").and_then(Value::as_str),
                    )
                    && announced.insert(index)
                {
                    sink.on_tool_use(id, name);
                }
            }
            "content_block_delta" => {
                let Some(delta) = payload.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(Value::as_str) {
                            marking_first_token(timing, emitted, started);
                            sink.on_text(t);
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                            marking_first_token(timing, emitted, started);
                            sink.on_thinking(t);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn marking_first_token(timing: &mut StreamTiming, emitted: &mut bool, started: Instant) {
    *emitted = true;
    if timing.first_token_ns.is_none() {
        // Measured from the moment the request went out, so this number includes
        // `wire_wait` — which is correct: what it has to answer is "how long
        // until the user saw something", not "how long after the headers".
        timing.first_token_ns = Some(started.elapsed().as_nanos() as u64);
    }
}

fn retry_after_of(e: &WireError) -> Option<u64> {
    match e.kind {
        ErrorKind::RateLimited { retry_after_ms } => retry_after_ms,
        _ => None,
    }
}

fn preview(body: &str) -> String {
    body.chars().take(200).collect()
}

/// A stream failure, tagged with whether output had already been delivered.
#[derive(Debug)]
struct StreamFailure {
    error: WireError,
    emitted_content: bool,
}

impl StreamFailure {
    fn new(error: WireError) -> Self {
        Self {
            error,
            emitted_content: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct WireClientBuilder {
    base_url: Option<String>,
    auth: Option<Auth>,
    quirks: Option<Quirks>,
    retry: Option<RetryPolicy>,
    timeline: Option<Arc<Timeline>>,
    betas: Vec<String>,
    default_model: Option<String>,
    timeout_secs: Option<u64>,
}

impl WireClientBuilder {
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn quirks(mut self, quirks: Quirks) -> Self {
        self.quirks = Some(quirks);
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = Some(retry);
        self
    }

    pub fn timeline(mut self, timeline: Arc<Timeline>) -> Self {
        self.timeline = Some(timeline);
        self
    }

    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    pub fn beta(mut self, beta: impl Into<String>) -> Self {
        self.betas.push(beta.into());
        self
    }

    pub fn timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = Some(secs);
        self
    }

    pub fn build(self) -> Result<WireClient, WireError> {
        let auth = self
            .auth
            .ok_or_else(|| WireError::new(ErrorKind::Auth, "no credential configured"))?;
        let timeout = self.timeout_secs.unwrap_or(600);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout))
            // Deliberately not the default (up to 10 hops). This client talks to
            // one configured endpoint, so a redirect cannot express anything it
            // wants — it can only move the request to a host the operator did not
            // choose, which here means exfiltrating the credential: reqwest
            // strips only `Authorization`/`Cookie` on a cross-host hop, and ours
            // is the custom header `x-api-key`. `none()` makes the 3xx an ordinary
            // non-2xx error pointing at the real fix (`base_url`), instead of a
            // silent 200 from somewhere else. A 307/308 replays the body, so
            // following one would also hand over the prompt.
            .redirect(reqwest::redirect::Policy::none())
            // A pooled, resident connection is the point: no per-step handshake.
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| WireError::transport(format!("could not build HTTP client: {e}")))?;
        Ok(WireClient {
            http,
            base_url: self
                .base_url
                .unwrap_or_else(|| "https://api.anthropic.com".to_owned()),
            auth,
            quirks: self.quirks.unwrap_or_else(Quirks::unknown),
            retry: self.retry.unwrap_or_default(),
            betas: self.betas,
            timeline: self.timeline,
            default_model: self.default_model.unwrap_or_else(|| "claude-opus-5".to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};

    use super::*;
    use crate::types::Message;

    #[test]
    fn debug_never_prints_the_credential() {
        let c = WireClient::builder()
            .base_url("https://example.test")
            .auth(Auth::ApiKey("sk-super-secret-value".into()))
            .build()
            .unwrap();
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("super-secret"), "credential leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn builder_requires_a_credential() {
        let err = WireClient::builder()
            .base_url("https://example.test")
            .build()
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Auth);
    }

    #[test]
    fn url_join_tolerates_a_trailing_slash() {
        let c = WireClient::builder()
            .base_url("https://example.test/anthropic/")
            .auth(Auth::ApiKey("k".into()))
            .build()
            .unwrap();
        assert_eq!(c.url(MESSAGES_PATH), "https://example.test/anthropic/v1/messages");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let p = RetryPolicy {
            max_attempts: 6,
            base_delay_ms: 100,
            max_delay_ms: 1000,
        };
        assert_eq!(p.delay(1, None).as_millis(), 100);
        assert_eq!(p.delay(2, None).as_millis(), 200);
        assert_eq!(p.delay(3, None).as_millis(), 400);
        assert_eq!(p.delay(9, None).as_millis(), 1000, "must be capped");
    }

    #[test]
    fn retry_after_wins_over_backoff() {
        let p = RetryPolicy::default();
        assert_eq!(p.delay(1, Some(1234)).as_millis(), 1234);
        // ...but is still capped.
        assert_eq!(
            p.delay(1, Some(u64::MAX)).as_millis(),
            p.max_delay_ms as u128
        );
    }

    // -----------------------------------------------------------------------
    // Redirects
    //
    // The endpoint is explicit configuration, so a redirect is never something
    // this client wants: following one hands the request to a host the operator
    // did not choose. These tests use real loopback servers rather than a mocked
    // transport, because what has to be observed is which host the bytes
    // physically reach.
    // -----------------------------------------------------------------------

    fn loopback_listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").expect("a loopback port")
    }

    /// Read one complete HTTP/1.1 request: the head, then `content-length` bytes.
    ///
    /// The body is drained before the caller answers. A server that replies and
    /// closes while the client is still writing turns a redirect into a
    /// connection reset, and a test that means to catch a followed redirect
    /// would then be unable to tell the two apart.
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

    /// Answers one request with a 307 to `location` — what a gateway in front of
    /// the configured endpoint sends when it wants the request elsewhere.
    fn start_redirector(location: String) -> SocketAddr {
        let listener = loopback_listener();
        let addr = listener.local_addr().expect("listener address");
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            read_request(&mut sock);
            let reply = format!(
                "HTTP/1.1 307 Temporary Redirect\r\n\
                 Location: {location}\r\n\
                 Content-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            );
            let _ = sock.write_all(reply.as_bytes());
        });
        addr
    }

    /// Records whatever request arrives, and answers `200` with a message shaped
    /// like the endpoint's — so a client that followed the redirect would see a
    /// perfectly ordinary success and could not tell where the answer came from.
    struct Spy {
        addr: SocketAddr,
        received: std::sync::mpsc::Receiver<String>,
    }

    impl Spy {
        fn start() -> Self {
            let listener = loopback_listener();
            let addr = listener.local_addr().expect("listener address");
            listener.set_nonblocking(true).expect("non-blocking listener");
            let (tx, received) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match listener.accept() {
                        Ok((mut sock, _)) => {
                            let request = read_request(&mut sock);
                            let _ = sock.write_all(&spy_reply());
                            let _ = tx.send(request);
                            return;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if Instant::now() >= deadline {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                }
            });
            Self { addr, received }
        }

        /// `None` means nothing arrived within `wait`.
        fn captured(&self, wait: Duration) -> Option<String> {
            self.received.recv_timeout(wait).ok()
        }
    }

    fn spy_reply() -> Vec<u8> {
        let body = r#"{"id":"msg-spy","model":"spy","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A 3xx must not be followed: reqwest's default policy would forward the
    /// request to the `Location` host, and `x-api-key` is not a header it strips
    /// (it strips only `Authorization`/`Cookie`, and only when host or port
    /// changes), so the API key — and, because 307/308 replay the body, the whole
    /// prompt — would be delivered to a host this client was never pointed at.
    #[tokio::test]
    async fn a_redirect_never_carries_the_credential_to_another_endpoint() {
        let canary = "sk-canary-must-not-leave-0001";
        let spy = Spy::start();
        let redirector = start_redirector(format!("http://{}{}", spy.addr, MESSAGES_PATH));

        let client = WireClient::builder()
            .base_url(format!("http://{redirector}"))
            .auth(Auth::ApiKey(canary.to_owned()))
            .build()
            .expect("building the client");
        let req = MessagesRequest::new("claude-opus-5", 16, vec![Message::user("hello")]);

        let outcome = client.send(&req).await;

        // The spy is the only server on that address, so a captured request is
        // one this client sent there — credential (and body) included.
        let captured = spy.captured(Duration::from_secs(2));
        assert!(
            captured.is_none(),
            "the redirect target received the forwarded request:\n{}",
            captured.as_deref().unwrap_or("")
        );

        let err = outcome.expect_err("a redirect must fail rather than be followed");
        assert_eq!(
            err.status,
            Some(307),
            "the caller must learn the endpoint answered with a redirect: {err}"
        );
    }
}
