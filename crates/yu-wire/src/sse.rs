//! SSE parsing and content-block accumulation.
//!
//! The measured endpoint emits Anthropic-shaped events verbatim — same event
//! names, same delta shapes, including `ping` and `signature_delta`. So this is
//! a straightforward implementation; the interesting part is what it *keeps*.
//!
//! DESIGN's first principle is that context is a view and the record is the
//! source of truth. The accumulator therefore has two outputs: the assembled
//! message the model's turn became, and the raw event bytes, so the journal can
//! hold the stream as it arrived rather than only its summary.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ErrorKind, WireError};
use crate::types::{
    ContentBlock, RedactedThinkingBlock, ServerToolUseBlock, StopReason, TextBlock, ThinkingBlock,
    ToolUseBlock, Usage, WebSearchToolResultBlock,
};

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    /// Byte offset of this frame within the stream, for journal addressing.
    #[serde(default)]
    pub offset: u64,
}

impl SseEvent {
    pub fn json(&self) -> Result<Value, WireError> {
        serde_json::from_str(&self.data).map_err(|e| {
            WireError::new(
                ErrorKind::MalformedResponse,
                format!("SSE data was not JSON: {e}"),
            )
        })
    }

    pub fn kind(&self) -> &str {
        self.event.as_deref().unwrap_or("message")
    }
}

/// Incremental SSE frame parser.
///
/// Tolerates `\n\n`, `\r\n\r\n`, and `\r\n\n` frame separators, ignores comment
/// lines (`:`), and buffers partial frames across chunk boundaries.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    consumed: u64,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk; get back whatever complete frames it completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((sep_at, sep_len)) = find_frame_end(&self.buf) {
            let frame: Vec<u8> = self.buf.drain(..sep_at).collect();
            // Drop the separator itself.
            self.buf.drain(..sep_len);
            let offset = self.consumed;
            self.consumed += (sep_at + sep_len) as u64;
            if let Some(ev) = parse_frame(&frame, offset) {
                out.push(ev);
            }
        }
        out
    }

    /// Flush any trailing frame that arrived without a terminating blank line.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if self.buf.is_empty() {
            return None;
        }
        let frame: Vec<u8> = std::mem::take(&mut self.buf);
        let offset = self.consumed;
        self.consumed += frame.len() as u64;
        parse_frame(&frame, offset)
    }
}

/// Find the first frame boundary: a `\n` followed by `\n` or `\r\n`.
fn find_frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i] != b'\n' {
            continue;
        }
        match (buf.get(i + 1), buf.get(i + 2)) {
            (Some(b'\n'), _) => return Some((i, 2)),
            (Some(b'\r'), Some(b'\n')) => return Some((i, 3)),
            _ => {}
        }
    }
    None
}

fn parse_frame(frame: &[u8], offset: u64) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(frame);
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue; // comment / keep-alive
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data_lines.push(value),
            // `id` and `retry` are unused by this API.
            _ => {}
        }
    }

    let data = data_lines.join("\n");
    if data.is_empty() && event.is_none() {
        return None;
    }
    Some(SseEvent {
        event,
        data,
        offset,
    })
}

// ---------------------------------------------------------------------------
// Accumulation
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Partial {
    Text(String),
    Thinking { text: String, signature: String },
    Redacted(String),
    ToolUse { id: String, name: String, json: String },
    ServerToolUse { id: String, name: String, json: String },
    Opaque(Value),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamedMessage {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

/// Turns a stream of SSE events back into a message.
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    id: String,
    model: String,
    blocks: Vec<(usize, Partial)>,
    stop_reason: Option<StopReason>,
    stop_sequence: Option<String>,
    usage: Usage,
    /// Cumulative streamed text, for the caller's first-token timing.
    pub text_len: usize,
    /// Whether any `signature_delta` was seen. DESIGN's L2 constraint 4 exists
    /// precisely because signatures can outweigh the text they accompany.
    pub signature_bytes: usize,
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, ev: &SseEvent) -> Result<(), WireError> {
        let payload = ev.json()?;
        let ty = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_else(|| ev.kind());

        match ty {
            "message_start" => {
                if let Some(m) = payload.get("message") {
                    self.id = str_at(m, "id");
                    self.model = str_at(m, "model");
                    if let Some(u) = m.get("usage") {
                        self.usage = serde_json::from_value(u.clone()).unwrap_or_default();
                    }
                }
            }
            "content_block_start" => {
                let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = payload.get("content_block").cloned().unwrap_or(Value::Null);
                self.blocks.push((index, partial_from(&block)));
            }
            "content_block_delta" => {
                let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = payload.get("delta").cloned().unwrap_or(Value::Null);
                self.apply_delta(index, &delta);
            }
            "content_block_stop" => {}
            "message_delta" => {
                if let Some(d) = payload.get("delta") {
                    if let Some(sr) = d.get("stop_reason").and_then(Value::as_str) {
                        self.stop_reason = Some(StopReason::from_wire(sr));
                    }
                    if let Some(ss) = d.get("stop_sequence").and_then(Value::as_str) {
                        self.stop_sequence = Some(ss.to_owned());
                    }
                }
                if let Some(u) = payload.get("usage") {
                    // message_delta usage carries final output_tokens.
                    let merged: Usage = serde_json::from_value(u.clone()).unwrap_or_default();
                    if merged.input_tokens > 0 {
                        self.usage.input_tokens = merged.input_tokens;
                    }
                    if merged.output_tokens > 0 {
                        self.usage.output_tokens = merged.output_tokens;
                    }
                    if merged.cache_read_input_tokens > 0 {
                        self.usage.cache_read_input_tokens = merged.cache_read_input_tokens;
                    }
                    if merged.cache_creation_input_tokens > 0 {
                        self.usage.cache_creation_input_tokens =
                            merged.cache_creation_input_tokens;
                    }
                }
            }
            "message_stop" => {}
            "ping" => {}
            "error" => {
                let err = payload.get("error").cloned().unwrap_or(Value::Null);
                let message = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("stream error")
                    .to_owned();
                let etype = err.get("type").and_then(Value::as_str).unwrap_or("");
                let kind = match etype {
                    "overloaded_error" => ErrorKind::Overloaded,
                    "rate_limit_error" => ErrorKind::RateLimited {
                        retry_after_ms: None,
                    },
                    "authentication_error" => ErrorKind::Auth,
                    _ => ErrorKind::StreamError,
                };
                return Err(WireError::new(kind, message));
            }
            // Unknown event kinds are ignored rather than fatal: a newer server
            // may add events, and refusing to continue would be a regression.
            _ => {}
        }
        Ok(())
    }

    fn apply_delta(&mut self, index: usize, delta: &Value) {
        let ty = delta.get("type").and_then(Value::as_str).unwrap_or("");
        let Some((_, partial)) = self.blocks.iter_mut().find(|(i, _)| *i == index) else {
            return;
        };
        match (ty, partial) {
            ("text_delta", Partial::Text(s)) => {
                if let Some(t) = delta.get("text").and_then(Value::as_str) {
                    s.push_str(t);
                    self.text_len += t.len();
                }
            }
            ("thinking_delta", Partial::Thinking { text, .. }) => {
                if let Some(t) = delta.get("thinking").and_then(Value::as_str) {
                    text.push_str(t);
                    self.text_len += t.len();
                }
            }
            ("signature_delta", Partial::Thinking { signature, .. }) => {
                if let Some(s) = delta.get("signature").and_then(Value::as_str) {
                    signature.push_str(s);
                    self.signature_bytes += s.len();
                }
            }
            ("input_json_delta", Partial::ToolUse { json, .. })
            | ("input_json_delta", Partial::ServerToolUse { json, .. }) => {
                if let Some(p) = delta.get("partial_json").and_then(Value::as_str) {
                    json.push_str(p);
                }
            }
            _ => {}
        }
    }

    /// Assemble the final message. Consumes the accumulator.
    pub fn finish(self) -> StreamedMessage {
        let mut blocks: Vec<(usize, ContentBlock)> = self
            .blocks
            .into_iter()
            .map(|(i, p)| (i, finalize(p)))
            .collect();
        blocks.sort_by_key(|(i, _)| *i);
        StreamedMessage {
            id: self.id,
            model: self.model,
            content: blocks.into_iter().map(|(_, b)| b).collect(),
            stop_reason: self.stop_reason,
            stop_sequence: self.stop_sequence,
            usage: self.usage,
        }
    }
}

fn partial_from(block: &Value) -> Partial {
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => Partial::Text(str_at(block, "text")),
        "thinking" => Partial::Thinking {
            text: str_at(block, "thinking"),
            signature: str_at(block, "signature"),
        },
        "redacted_thinking" => Partial::Redacted(str_at(block, "data")),
        "tool_use" => Partial::ToolUse {
            id: str_at(block, "id"),
            name: str_at(block, "name"),
            json: String::new(),
        },
        "server_tool_use" => Partial::ServerToolUse {
            id: str_at(block, "id"),
            name: str_at(block, "name"),
            json: String::new(),
        },
        _ => Partial::Opaque(block.clone()),
    }
}

fn finalize(p: Partial) -> ContentBlock {
    match p {
        Partial::Text(text) => ContentBlock::Text(TextBlock {
            text,
            citations: None,
        }),
        Partial::Thinking { text, signature } => ContentBlock::Thinking(ThinkingBlock {
            thinking: text,
            // An empty signature is omitted rather than sent as "". Measured:
            // `content_block_start` for a thinking block carries `"signature": ""`
            // before any `signature_delta` arrives.
            signature: (!signature.is_empty()).then_some(signature),
        }),
        Partial::Redacted(data) => ContentBlock::RedactedThinking(RedactedThinkingBlock { data }),
        Partial::ToolUse { id, name, json } => ContentBlock::ToolUse(ToolUseBlock {
            id,
            name,
            input: parse_partial_json(&json),
        }),
        Partial::ServerToolUse { id, name, json } => ContentBlock::ServerToolUse(ServerToolUseBlock {
            id,
            name,
            input: parse_partial_json(&json),
        }),
        Partial::Opaque(v) => {
            if v.get("type").and_then(Value::as_str) == Some("web_search_tool_result") {
                ContentBlock::WebSearchToolResult(WebSearchToolResultBlock {
                    tool_use_id: str_at(&v, "tool_use_id"),
                    content: v.get("content").cloned().unwrap_or(Value::Null),
                })
            } else {
                ContentBlock::Other(v)
            }
        }
    }
}

/// A partially streamed tool input may be empty or truncated.
///
/// With `eager_input_streaming` the server does not validate, so this can be
/// incomplete. Falling back to `Null` rather than erroring keeps the failure
/// visible downstream (the tool call has no arguments) instead of aborting the
/// stream — but callers must validate before executing.
fn parse_partial_json(s: &str) -> Value {
    if s.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(s).unwrap_or(Value::Null)
}

fn str_at(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: message_start\ndata: {\"a\"").is_empty());
        let evs = p.push(b":1}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"a\":1}");
    }

    #[test]
    fn crlf_framing_is_accepted() {
        let mut p = SseParser::new();
        let evs = p.push(b"event: ping\r\ndata: {}\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("ping"));
    }

    #[test]
    fn comments_and_blank_keepalives_are_ignored() {
        let mut p = SseParser::new();
        let evs = p.push(b": keep-alive\n\nevent: ping\ndata: {}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("ping"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        let mut p = SseParser::new();
        let evs = p.push(b"data: line one\ndata: line two\n\n");
        assert_eq!(evs[0].data, "line one\nline two");
    }

    /// The real stream shape captured from the endpoint: thinking, then a
    /// `signature_delta`, then a text block.
    #[test]
    fn assembles_a_real_thinking_then_text_stream() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"4aeac4b3\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"deepseek-flash[1m]\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":37,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"We need answer\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"69164b36-fbed-4709-adfd-d6871900f038\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":14}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );

        let mut parser = SseParser::new();
        let mut acc = StreamAccumulator::new();
        for ev in parser.push(raw.as_bytes()) {
            acc.apply(&ev).unwrap();
        }
        let msg = acc.finish();

        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.content[0].type_name(), "thinking");
        assert_eq!(msg.content[1].type_name(), "text");
        let thinking = msg.content[0].as_thinking().unwrap();
        assert_eq!(thinking.thinking, "We need answer");
        assert_eq!(
            thinking.signature.as_deref(),
            Some("69164b36-fbed-4709-adfd-d6871900f038")
        );
        assert_eq!(msg.content[1].as_text(), Some("ok"));
        assert_eq!(msg.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(msg.usage.input_tokens, 37);
        assert_eq!(msg.usage.output_tokens, 14);
    }

    #[test]
    fn streamed_tool_input_is_assembled_from_partial_json() {
        let raw = concat!(
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_00_x\",\"name\":\"get_time\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"tz\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Asia/Tokyo\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let mut parser = SseParser::new();
        let mut acc = StreamAccumulator::new();
        for ev in parser.push(raw.as_bytes()) {
            acc.apply(&ev).unwrap();
        }
        let msg = acc.finish();
        let tu = msg.content[0].as_tool_use().unwrap();
        assert_eq!(tu.id, "call_00_x");
        assert_eq!(tu.name, "get_time");
        assert_eq!(tu.input["tz"], "Asia/Tokyo");
    }

    #[test]
    fn mid_stream_error_becomes_a_typed_error() {
        let mut acc = StreamAccumulator::new();
        let ev = SseEvent {
            event: Some("error".into()),
            data: r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#.into(),
            offset: 0,
        };
        let err = acc.apply(&ev).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Overloaded);
        assert!(err.is_retryable());
    }
}
