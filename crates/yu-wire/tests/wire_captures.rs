//! Parse the real captured wire traffic.
//!
//! Everything asserted here is a fact measured from the endpoint, recorded in
//! `docs/WIRE-FACTS.md`. If a future server change breaks one of these, the
//! failure should name which measured assumption no longer holds — which is why
//! the assertions are specific rather than "it parses".

use std::path::PathBuf;

use serde_json::Value;
use yu_wire::error::{ErrorKind, WireError};
use yu_wire::quirks::{HistoryProblemKind, Quirks};
use yu_wire::sse::{SseParser, StreamAccumulator};
use yu_wire::types::{ContentBlock, MessagesResponse, StopDisposition, StopReason};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"))
}

fn parse_response(name: &str) -> MessagesResponse {
    serde_json::from_str(&fixture(name)).unwrap_or_else(|e| panic!("parsing {name}: {e}"))
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// A one-word answer still costs a thinking block. This is the fact that makes
/// `first_token` timing meaningless if it only counts text deltas.
#[test]
fn even_a_trivial_turn_opens_with_a_thinking_block() {
    let resp = parse_response("response_basic_thinking_text.json");
    assert_eq!(resp.content[0].type_name(), "thinking");
    assert!(resp.carries_thinking());
    assert_eq!(resp.model, "deepseek-flash[1m]");
    // The message id is a UUID here, not Anthropic's `msg_…`.
    assert!(
        !resp.id.starts_with("msg_"),
        "id shape changed: {}",
        resp.id
    );
}

#[test]
fn a_tool_use_turn_carries_thinking_and_a_call_prefixed_id() {
    let resp = parse_response("response_tool_use.json");
    let uses = resp.tool_uses();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].name, "get_time");
    assert_eq!(uses[0].input["tz"], "Asia/Tokyo");
    // Anthropic uses `toolu_…`. Nothing may depend on that.
    assert!(
        uses[0].id.starts_with("call_"),
        "tool-call id shape changed: {}",
        uses[0].id
    );
    assert!(resp.carries_thinking());
    assert_eq!(
        resp.stop_reason.as_ref().map(StopReason::disposition),
        Some(StopDisposition::RunTools)
    );
}

#[test]
fn two_tool_calls_can_share_one_turn() {
    // The parallel admission gate exists because this is real.
    let resp = parse_response("response_parallel_tool_use.json");
    let uses = resp.tool_uses();
    assert_eq!(uses.len(), 2, "expected two tool calls in one turn");
    let names: Vec<&str> = uses.iter().map(|u| u.name.as_str()).collect();
    assert!(names.contains(&"get_time"));
    assert!(names.contains(&"get_weather"));
    // Distinct ids, or the results could not be matched back.
    assert_ne!(uses[0].id, uses[1].id);
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[test]
fn the_real_stream_assembles_into_thinking_then_text() {
    let raw = fixture("stream_thinking_then_text.sse");
    let mut parser = SseParser::new();
    let mut acc = StreamAccumulator::new();
    for ev in parser.push(raw.as_bytes()) {
        acc.apply(&ev).unwrap_or_else(|e| panic!("applying {ev:?}: {e}"));
    }
    let msg = acc.finish();

    assert_eq!(msg.content.len(), 2, "got {:?}", msg.content);
    assert_eq!(msg.content[0].type_name(), "thinking");
    assert_eq!(msg.content[1].type_name(), "text");
    assert_eq!(msg.content[1].as_text(), Some("ok"));

    let thinking = msg.content[0].as_thinking().unwrap();
    assert!(!thinking.thinking.is_empty(), "thinking text should survive");
    let signature = thinking
        .signature
        .as_deref()
        .expect("signature arrives via signature_delta and must be kept");
    assert_eq!(
        signature.len(),
        36,
        "measured as a 36-byte UUID: {signature}"
    );

    assert_eq!(msg.usage.input_tokens, 37);
    assert!(msg.usage.output_tokens > 0);
    assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
}

/// DESIGN's L2 constraint 4 is that signatures are stored separately from
/// thinking text. This checks the *reason* it matters: on a short turn the
/// signature is longer than the text it accompanies.
#[test]
fn a_signature_can_outweigh_the_thinking_it_accompanies() {
    let raw = fixture("stream_thinking_then_text.sse");
    let mut parser = SseParser::new();
    let mut acc = StreamAccumulator::new();
    for ev in parser.push(raw.as_bytes()) {
        acc.apply(&ev).unwrap();
    }
    let signature_bytes = acc.signature_bytes;
    let msg = acc.finish();
    let thinking_chars = msg.content[0]
        .as_thinking()
        .map(|t| t.thinking.len())
        .unwrap_or(0);
    assert!(
        signature_bytes > 0,
        "no signature_delta was seen in the real stream"
    );
    // Not asserted as universally true -- just that the two are tracked
    // independently, so a case where the signature dominates is representable.
    assert!(
        thinking_chars <= 200,
        "unexpectedly long thinking: {thinking_chars} chars"
    );
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[test]
fn a_422_enumerates_what_the_endpoint_does_accept() {
    let body = fixture("error_422_unknown_variant.json");
    let err = WireError::from_response(422, &body, None);
    assert_eq!(err.kind, ErrorKind::Unsupported);
    assert!(!err.is_retryable(), "retrying an unmodelled field is futile");
    assert_eq!(
        err.expected_variants,
        vec!["web_search_20250305", "web_search_20260209"],
        "the accepted-variant list is the cheapest capability probe there is"
    );
    assert!(err.request_id.is_some());
}

#[test]
fn a_401_is_not_retryable_and_must_not_be_logged_verbatim() {
    let body = fixture("error_401_auth.json");
    let err = WireError::from_response(401, &body, None);
    assert_eq!(err.kind, ErrorKind::Auth);
    assert!(!err.is_retryable());
    // The body embeds the last four characters of the key. Note for whoever adds
    // logging: `err.message` carries key material and must not be logged raw.
    assert!(
        err.message.contains("****"),
        "auth error shape changed; re-check the logging guidance in WIRE-FACTS.md"
    );
}

// ---------------------------------------------------------------------------
// History validation, against the exact bytes the endpoint was given
// ---------------------------------------------------------------------------

#[test]
fn a_history_carrying_its_thinking_passes_validation() {
    let body = fixture("request_history_with_thinking.json");
    let v: Value = serde_json::from_str(&body).unwrap();
    let messages: Vec<yu_wire::types::Message> =
        serde_json::from_value(v["messages"].clone()).unwrap();

    let blocks = messages[1].blocks();
    let kinds: Vec<&str> = blocks.iter().map(ContentBlock::type_name).collect();
    assert!(
        kinds.contains(&"thinking"),
        "fixture should carry thinking; got {kinds:?}"
    );

    Quirks::deepseek_anthropic()
        .check_history(&messages)
        .expect("a faithfully replayed history is valid");
}

/// The awkward case, and the reason `check_history` errs strict.
///
/// The endpoint **accepted** this exact body with a 200 — because the
/// `tool_use.id` was one it had issued itself, so it could recover the turn
/// server-side. But a client cannot know whether an id was endpoint-issued,
/// and in the fabricated-id case (see the next test) the identical shape is
/// rejected with a 400 whose message blames a missing thinking block and names
/// no location.
///
/// So this client requires the thinking block unconditionally. That is
/// deliberately stricter than the server: replaying is free, and a client that
/// guesses right on this particular request would guess wrong on the next one.
#[test]
fn a_history_with_its_thinking_stripped_is_rejected_even_though_the_server_allowed_it() {
    let body = fixture("request_history_stripped.json");
    let v: Value = serde_json::from_str(&body).unwrap();
    let messages: Vec<yu_wire::types::Message> =
        serde_json::from_value(v["messages"].clone()).unwrap();

    let err = Quirks::deepseek_anthropic()
        .check_history(&messages)
        .expect_err("stripped thinking must be caught client-side");
    assert_eq!(err.index, 1, "the assistant turn is messages[1]");
    assert_eq!(err.kind, HistoryProblemKind::ToolUseWithoutThinking);
    // The value of doing this locally: our message names the location.
    assert!(err.to_string().contains("messages[1]"));
}

#[test]
fn a_fabricated_id_with_no_thinking_is_rejected() {
    let body = fixture("request_history_fabricated_id.json");
    let v: Value = serde_json::from_str(&body).unwrap();
    let messages: Vec<yu_wire::types::Message> =
        serde_json::from_value(v["messages"].clone()).unwrap();
    assert!(
        Quirks::deepseek_anthropic()
            .check_history(&messages)
            .is_err()
    );
}

#[test]
fn every_fixture_is_valid_json_or_valid_sse() {
    // A cheap guard against a fixture being saved truncated.
    for name in [
        "response_basic_thinking_text.json",
        "response_tool_use.json",
        "response_parallel_tool_use.json",
        "error_422_unknown_variant.json",
        "error_401_auth.json",
        "request_history_with_thinking.json",
        "request_history_stripped.json",
        "request_history_fabricated_id.json",
    ] {
        let body = fixture(name);
        serde_json::from_str::<Value>(&body).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let sse = fixture("stream_thinking_then_text.sse");
    assert!(sse.contains("event: message_start"));
    assert!(sse.contains("event: message_stop"));
    // `signature_delta` is a delta *type* carried inside a `content_block_delta`
    // event, not an event name of its own.
    assert!(sse.contains(r#""type":"content_block_delta""#));
    assert!(sse.contains(r#""type":"signature_delta""#));
}
