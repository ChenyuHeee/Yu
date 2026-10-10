//! Request, response, and content types.
//!
//! Two rules from DESIGN are enforced by the shapes here rather than by
//! convention:
//!
//! * **`stop_reason` is exhaustive.** DESIGN: "必须穷尽 `stop_reason`". A variant
//!   this client does not recognise becomes [`StopReason::Unknown`] carrying the
//!   raw string, and [`StopReason::disposition`] maps *every* variant to what the
//!   loop must do. An unrecognised stop reason can never be mistaken for success.
//! * **Unknown content blocks are preserved verbatim.** An assistant turn is
//!   replayed byte-for-byte (see `docs/WIRE-FACTS.md` Quirk 1), so a block this
//!   client does not model must still round-trip. Nothing is dropped silently.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// DESIGN: "`cache_control` 每请求最多 4 个断点".
pub const MAX_CACHE_BREAKPOINTS: usize = 4;

// ---------------------------------------------------------------------------
// Cache control
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: CacheControlKind,
    /// `"1h"` for the long TTL. Absent means the default 5 minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControlKind {
    Ephemeral,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: None,
        }
    }

    pub fn ephemeral_1h() -> Self {
        Self {
            kind: CacheControlKind::Ephemeral,
            ttl: Some("1h".to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// Thinking / effort / output config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    /// Reasoning is returned. On current Anthropic models the default flipped to
    /// `Omitted`; on the measured endpoint `display` appears inert.
    Summarized,
    Omitted,
    /// Between-tool-call progress notes, hidden reasoning.
    Updates,
}

/// DESIGN: `{type:"adaptive"}` is the default on Opus 5; `budget_tokens` was
/// removed and 400s. The measured endpoint is *more permissive* and accepts the
/// legacy form — see `docs/WIRE-FACTS.md`. Both are modelled so the wire layer
/// can target either without a code change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Thinking {
    Adaptive {
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Disabled,
    /// Legacy. Removed on current Anthropic models; accepted on the measured one.
    Enabled { budget_tokens: u32 },
    /// Sonnet 5.5's way of turning thinking off.
    BetweenTools,
}

impl Thinking {
    pub fn adaptive() -> Self {
        Thinking::Adaptive { display: None }
    }

    pub fn adaptive_with(display: ThinkingDisplay) -> Self {
        Thinking::Adaptive {
            display: Some(display),
        }
    }
}

/// DESIGN: `low`…`max`, default `high` (Opus 5) / `medium` (Opus 5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Structured outputs. `output_config.format`, not the deprecated top-level
    /// `output_format`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_budget: Option<Value>,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// A tool the client defines and executes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    /// Guarantees `tool_use.input` validates against the schema exactly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Stream large tool inputs as they are generated. The client then owns
    /// validation, because a tolerant parser can return truncated input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl CustomTool {
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            input_schema,
            strict: None,
            eager_input_streaming: None,
            defer_loading: None,
            cache_control: None,
        }
    }

    pub fn strict(mut self) -> Self {
        self.strict = Some(true);
        self
    }
}

/// An Anthropic-defined tool: a declared `type`, no `input_schema`.
///
/// DESIGN's claim that these "白拿训练过的工具纪律" is **not available on the
/// measured endpoint** — it accepts only the two `web_search` variants and 422s
/// everything else. The type is modelled anyway so the layer can target a real
/// Anthropic endpoint unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ServerTool {
    /// `{"type":"bash_20250124","name":"bash"}` — schema-less.
    pub fn bash() -> Self {
        Self {
            kind: "bash_20250124".to_owned(),
            name: "bash".to_owned(),
            extra: BTreeMap::new(),
        }
    }

    /// `{"type":"text_editor_20250728","name":"str_replace_based_edit_tool"}`.
    ///
    /// The name is fixed by the API, not free-form — getting it wrong silently
    /// produces a different tool.
    pub fn text_editor() -> Self {
        Self {
            kind: "text_editor_20250728".to_owned(),
            name: "str_replace_based_edit_tool".to_owned(),
            extra: BTreeMap::new(),
        }
    }

    /// The latest web-search variant (server-executed, dynamic filtering).
    pub fn web_search() -> Self {
        Self {
            kind: "web_search_20260209".to_owned(),
            name: "web_search".to_owned(),
            extra: BTreeMap::new(),
        }
    }

    /// The tool-search tool. Note: never defer *every* tool, and this tool itself
    /// must not be deferred, or the API returns 400 `All tools have defer_loading set`.
    pub fn tool_search_regex() -> Self {
        Self {
            kind: "tool_search_tool_regex_20251119".to_owned(),
            name: "tool_search_tool_regex".to_owned(),
            extra: BTreeMap::new(),
        }
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.extra.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolDefinition {
    Custom(CustomTool),
    Server(ServerTool),
}

impl ToolDefinition {
    pub fn name(&self) -> &str {
        match self {
            ToolDefinition::Custom(t) => &t.name,
            ToolDefinition::Server(t) => &t.name,
        }
    }

    /// Whether this is an Anthropic-defined, schema-less tool.
    pub fn is_server_defined(&self) -> bool {
        matches!(self, ToolDefinition::Server(_))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any,
    Tool { name: String },
    None,
}

impl ToolChoice {
    pub fn auto() -> Self {
        ToolChoice::Auto {
            disable_parallel_tool_use: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextBlock {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citations: Option<Value>,
}

/// Thinking text and its signature are separate fields, and stay separate
/// through the journal too — DESIGN's L2 constraint 4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingBlock {
    pub thinking: String,
    /// Measured as a 36-byte UUID on the endpoint probed, and *not validated*
    /// there. DESIGN's rule still holds: it can be larger than the text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RedactedThinkingBlock {
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUseBlock {
    /// Opaque. Never parse, validate, or synthesise — see `docs/WIRE-FACTS.md`
    /// Quirk 2. Measured shapes differ from Anthropic's.
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl ToolResultContent {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ToolResultContent::Text(s) => Some(s),
            ToolResultContent::Blocks(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultBlock {
    pub tool_use_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl ToolResultBlock {
    pub fn text(tool_use_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: Some(ToolResultContent::Text(text.into())),
            is_error: None,
            cache_control: None,
        }
    }

    pub fn error(tool_use_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            content: Some(ToolResultContent::Text(text.into())),
            is_error: Some(true),
            cache_control: None,
        }
    }
}

/// Server-side tool invocation (e.g. web search). The server ran it, not us.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerToolUseBlock {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebSearchToolResultBlock {
    pub tool_use_id: String,
    /// Either a list of results or a single error object. Branch on the shape
    /// before indexing — a server-tool error is HTTP 200 with an error payload,
    /// it does not raise.
    pub content: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text(TextBlock),
    Thinking(ThinkingBlock),
    RedactedThinking(RedactedThinkingBlock),
    ToolUse(ToolUseBlock),
    ToolResult(ToolResultBlock),
    ServerToolUse(ServerToolUseBlock),
    WebSearchToolResult(WebSearchToolResultBlock),
    /// A block this client does not model, kept verbatim so it round-trips.
    Other(Value),
}

impl ContentBlock {
    pub fn type_name(&self) -> &str {
        match self {
            ContentBlock::Text(_) => "text",
            ContentBlock::Thinking(_) => "thinking",
            ContentBlock::RedactedThinking(_) => "redacted_thinking",
            ContentBlock::ToolUse(_) => "tool_use",
            ContentBlock::ToolResult(_) => "tool_result",
            ContentBlock::ServerToolUse(_) => "server_tool_use",
            ContentBlock::WebSearchToolResult(_) => "web_search_tool_result",
            ContentBlock::Other(v) => v.get("type").and_then(Value::as_str).unwrap_or("<none>"),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(t) => Some(&t.text),
            _ => None,
        }
    }

    pub fn as_thinking(&self) -> Option<&ThinkingBlock> {
        match self {
            ContentBlock::Thinking(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_tool_use(&self) -> Option<&ToolUseBlock> {
        match self {
            ContentBlock::ToolUse(t) => Some(t),
            _ => None,
        }
    }

    pub fn cache_control_mut(&mut self) -> Option<&mut Option<CacheControl>> {
        match self {
            ContentBlock::Text(_) => None,
            ContentBlock::ToolResult(t) => Some(&mut t.cache_control),
            _ => None,
        }
    }
}

// Manual serde: an unrecognised `type` must land in `Other` with its payload
// intact, rather than failing the parse or being discarded.
//
// Every arm supplies the discriminator through `tagged`. The block structs
// deliberately do *not* carry a `type` field of their own: if they did, a
// variant and its tag could drift apart, and the failure would be a request the
// server rejects rather than a compile error. Keeping the tag at the single
// point where the variant is known makes that drift impossible.
impl Serialize for ContentBlock {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            ContentBlock::Text(b) => tagged("text", b, s),
            ContentBlock::Thinking(b) => tagged("thinking", b, s),
            ContentBlock::RedactedThinking(b) => tagged("redacted_thinking", b, s),
            ContentBlock::ToolUse(b) => tagged("tool_use", b, s),
            ContentBlock::ToolResult(b) => tagged("tool_result", b, s),
            ContentBlock::ServerToolUse(b) => tagged("server_tool_use", b, s),
            ContentBlock::WebSearchToolResult(b) => tagged("web_search_tool_result", b, s),
            // Already carries its own `type`, because it came off the wire that way.
            ContentBlock::Other(v) => serde::Serialize::serialize(v, s),
        }
    }
}

/// Serialize a block struct with its `type` discriminator, tag first.
fn tagged<T, S>(kind: &str, inner: &T, s: S) -> Result<S::Ok, S::Error>
where
    T: serde::Serialize,
    S: serde::Serializer,
{
    let fields = match serde_json::to_value(inner).map_err(serde::ser::Error::custom)? {
        Value::Object(map) => map,
        _ => {
            return Err(serde::ser::Error::custom(
                "a content block must serialize to a JSON object",
            ));
        }
    };
    let mut out = serde_json::Map::new();
    out.insert("type".to_owned(), Value::String(kind.to_owned()));
    out.extend(fields);
    out.serialize(s)
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        // Owned, so the match arms below can move `value` into the converters.
        let ty = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let block = match ty.as_str() {
            "text" => ContentBlock::Text(parse_block(&ty, value)?),
            "thinking" => ContentBlock::Thinking(parse_block(&ty, value)?),
            "redacted_thinking" => ContentBlock::RedactedThinking(parse_block(&ty, value)?),
            "tool_use" => ContentBlock::ToolUse(parse_block(&ty, value)?),
            "tool_result" => ContentBlock::ToolResult(parse_block(&ty, value)?),
            "server_tool_use" => ContentBlock::ServerToolUse(parse_block(&ty, value)?),
            "web_search_tool_result" => {
                ContentBlock::WebSearchToolResult(parse_block(&ty, value)?)
            }
            _ => ContentBlock::Other(value),
        };
        Ok(block)
    }
}

/// Parse one concrete block type.
///
/// A free generic function rather than a captured closure, and deliberately so:
/// a closure would have to be annotated with the enclosing return type
/// (`ContentBlock`), which sends `from_value` straight back into
/// `ContentBlock::deserialize` and recurses until the stack dies. Naming the
/// concrete type at each call site is what breaks the cycle.
fn parse_block<T, E>(ty: &str, value: Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    serde_json::from_value(value).map_err(|e| E::custom(format!("content block `{ty}`: {e}")))
}

impl From<TextBlock> for ContentBlock {
    fn from(b: TextBlock) -> Self {
        ContentBlock::Text(b)
    }
}

impl From<ThinkingBlock> for ContentBlock {
    fn from(b: ThinkingBlock) -> Self {
        ContentBlock::Thinking(b)
    }
}

impl From<ToolUseBlock> for ContentBlock {
    fn from(b: ToolUseBlock) -> Self {
        ContentBlock::ToolUse(b)
    }
}

impl From<ToolResultBlock> for ContentBlock {
    fn from(b: ToolResultBlock) -> Self {
        ContentBlock::ToolResult(b)
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Mid-conversation operator instruction, appended to `messages[]` rather
    /// than editing top-level `system`, so the cached prefix survives.
    System,
}

/// Content is either a bare string or a list of blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    pub fn blocks(&self) -> Vec<ContentBlock> {
        match self {
            MessageContent::Text(t) => vec![ContentBlock::Text(TextBlock {
                text: t.clone(),
                citations: None,
            })],
            MessageContent::Blocks(b) => b.clone(),
        }
    }

    pub fn concat_text(&self) -> String {
        match self {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Blocks(b) => b
                .iter()
                .filter_map(|x| x.as_text())
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        MessageContent::Text(s.to_owned())
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        MessageContent::Text(s)
    }
}

impl From<Vec<ContentBlock>> for MessageContent {
    fn from(b: Vec<ContentBlock>) -> Self {
        MessageContent::Blocks(b)
    }
}

impl Serialize for MessageContent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            MessageContent::Text(t) => serde::Serialize::serialize(t, s),
            MessageContent::Blocks(b) => serde::Serialize::serialize(b, s),
        }
    }
}

impl<'de> Deserialize<'de> for MessageContent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        Ok(match value {
            Value::String(s) => MessageContent::Text(s),
            Value::Array(_) => {
                let blocks: Vec<ContentBlock> =
                    serde_json::from_value(value).map_err(serde::de::Error::custom)?;
                MessageContent::Blocks(blocks)
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "message content must be a string or an array, got {}",
                    match other {
                        Value::Null => "null",
                        Value::Bool(_) => "bool",
                        Value::Number(_) => "number",
                        Value::Object(_) => "object",
                        _ => "?",
                    }
                )));
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

impl Message {
    pub fn user(content: impl Into<MessageContent>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<MessageContent>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    /// A mid-conversation operator instruction.
    ///
    /// Placement rules: it must follow a `user` message (or an assistant message
    /// ending in server-tool use), cannot be `messages[0]`, and must be either
    /// last or followed by an assistant turn.
    pub fn system(content: impl Into<MessageContent>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn blocks(&self) -> Vec<ContentBlock> {
        self.content.blocks()
    }
}

/// A system-prompt block. Cache breakpoints go here when the prompt is stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: SystemBlockKind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemBlockKind {
    Text,
}

impl SystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: SystemBlockKind::Text,
            text: text.into(),
            cache_control: None,
        }
    }

    pub fn with_cache(mut self) -> Self {
        self.cache_control = Some(CacheControl::ephemeral());
        self
    }
}

// ---------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub max_tokens: u32,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Beta features: `context_management.edits`, server-side compaction hooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Value>,
}

impl MessagesRequest {
    pub fn new(model: impl Into<String>, max_tokens: u32, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            messages,
            ..Default::default()
        }
    }

    pub fn system(mut self, blocks: Vec<SystemBlock>) -> Self {
        self.system = Some(blocks);
        self
    }

    pub fn tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn thinking(mut self, thinking: Thinking) -> Self {
        self.thinking = Some(thinking);
        self
    }

    pub fn effort(mut self, effort: Effort) -> Self {
        let cfg = self.output_config.get_or_insert_with(OutputConfig::default);
        cfg.effort = Some(effort);
        self
    }

    pub fn streaming(mut self) -> Self {
        self.stream = Some(true);
        self
    }

    /// Count cache breakpoints across the whole request.
    ///
    /// DESIGN calls the 4-breakpoint cap and the `tools`→`system`→`messages`
    /// render order iron rules, because a byte change anywhere in the prefix
    /// invalidates everything after it.
    pub fn cache_breakpoints(&self) -> usize {
        let mut n = 0;
        if let Some(tools) = &self.tools {
            for t in tools {
                if let ToolDefinition::Custom(c) = t
                    && c.cache_control.is_some()
                {
                    n += 1;
                }
            }
        }
        if let Some(system) = &self.system {
            n += system.iter().filter(|b| b.cache_control.is_some()).count();
        }
        for m in &self.messages {
            if let MessageContent::Blocks(blocks) = &m.content {
                for b in blocks {
                    match b {
                        ContentBlock::ToolResult(t) if t.cache_control.is_some() => n += 1,
                        _ => {}
                    }
                }
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

impl Usage {
    pub fn to_observed(&self) -> yu_observe::TokenUsage {
        yu_observe::TokenUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
        }
    }
}

/// Populated only when `stop_reason == Refusal`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopDetails {
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Open set: `cyber`, `bio`, `reasoning_extraction`, `frontier_llm`, `general_harms`, …
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

/// What the loop must do about a stop reason.
///
/// DESIGN: "必须穷尽 `stop_reason`". Mapping to a disposition (rather than
/// matching ad hoc at each call site) is what makes the exhaustion checkable:
/// [`StopReason::disposition`] has no wildcard arm, so a new variant is a
/// compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDisposition {
    /// Normal completion.
    Complete,
    /// Run the requested tools and continue.
    RunTools,
    /// Hit `max_tokens`. Output is truncated — never treat as completion.
    Truncated,
    /// A server tool paused mid-turn; re-send to let it continue.
    Resume,
    /// Safety classifier declined. Terminal.
    Refused,
    /// Context window exhausted.
    ContextExhausted,
    /// A stop reason this client does not know. Must be surfaced, not assumed.
    Unrecognized(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    /// Carries the raw string so nothing is lost and nothing is assumed.
    Unknown(String),
}

// Manual serde rather than a derive: `Unknown` has to swallow *any* string the
// server sends, and a derived externally-tagged enum would instead fail to parse
// a stop reason it does not know — turning a future server addition into a hard
// error rather than a visible `Unrecognized`.
impl Serialize for StopReason {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(StopReason::from_wire(&raw))
    }
}

impl StopReason {
    /// Map a wire string to a variant. Unknown strings are preserved.
    pub fn from_wire(s: &str) -> Self {
        match s {
            "end_turn" => StopReason::EndTurn,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "tool_use" => StopReason::ToolUse,
            "pause_turn" => StopReason::PauseTurn,
            "refusal" => StopReason::Refusal,
            "model_context_window_exceeded" => StopReason::ModelContextWindowExceeded,
            other => StopReason::Unknown(other.to_owned()),
        }
    }
    pub fn disposition(&self) -> StopDisposition {
        match self {
            StopReason::EndTurn => StopDisposition::Complete,
            StopReason::StopSequence => StopDisposition::Complete,
            StopReason::ToolUse => StopDisposition::RunTools,
            StopReason::MaxTokens => StopDisposition::Truncated,
            StopReason::PauseTurn => StopDisposition::Resume,
            StopReason::Refusal => StopDisposition::Refused,
            StopReason::ModelContextWindowExceeded => StopDisposition::ContextExhausted,
            StopReason::Unknown(raw) => StopDisposition::Unrecognized(raw.clone()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::MaxTokens => "max_tokens",
            StopReason::StopSequence => "stop_sequence",
            StopReason::ToolUse => "tool_use",
            StopReason::PauseTurn => "pause_turn",
            StopReason::Refusal => "refusal",
            StopReason::ModelContextWindowExceeded => "model_context_window_exceeded",
            StopReason::Unknown(raw) => raw,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    pub model: String,
    #[serde(default = "default_role")]
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(default)]
    pub stop_details: Option<StopDetails>,
}

fn default_role() -> Role {
    Role::Assistant
}

impl MessagesResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_uses(&self) -> Vec<&ToolUseBlock> {
        self.content.iter().filter_map(|b| b.as_tool_use()).collect()
    }

    /// Whether this response must be replayed with its thinking blocks intact.
    ///
    /// Measured: an assistant turn carrying `tool_use` needs a `thinking` block
    /// unless the endpoint recognises the tool-call id. See Quirk 1.
    pub fn carries_thinking(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking(_) | ContentBlock::RedactedThinking(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_content_block_round_trips_verbatim() {
        let raw = json!({"type": "some_future_block", "payload": {"nested": [1, 2, 3]}});
        let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert!(matches!(block, ContentBlock::Other(_)));
        assert_eq!(block.type_name(), "some_future_block");
        // The whole point: nothing is dropped.
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    /// Every known block must serialize with its discriminator and survive a
    /// round trip.
    ///
    /// This test exists because its absence let a real bug through: the tag was
    /// never emitted, so `ContentBlock::Text` serialized as `{"text":"hi"}` and a
    /// request containing a `tool_result` would have been rejected by the server.
    /// Nothing deserialization-side could catch it, because a struct without a
    /// `type` field simply ignores the tag on the way in.
    #[test]
    fn every_known_content_block_serializes_with_its_type_tag() {
        let blocks = vec![
            ContentBlock::Text(TextBlock {
                text: "hi".into(),
                citations: None,
            }),
            ContentBlock::Thinking(ThinkingBlock {
                thinking: "reasoning".into(),
                signature: Some("69164b36-fbed-4709-adfd-d6871900f038".into()),
            }),
            ContentBlock::RedactedThinking(RedactedThinkingBlock {
                data: "opaque".into(),
            }),
            ContentBlock::ToolUse(ToolUseBlock {
                id: "call_00_abc".into(),
                name: "get_time".into(),
                input: json!({"tz": "Asia/Tokyo"}),
            }),
            ContentBlock::ToolResult(ToolResultBlock::text("call_00_abc", "14:32 JST")),
            ContentBlock::ServerToolUse(ServerToolUseBlock {
                id: "srv_1".into(),
                name: "web_search".into(),
                input: json!({"query": "rust"}),
            }),
            ContentBlock::WebSearchToolResult(WebSearchToolResultBlock {
                tool_use_id: "srv_1".into(),
                content: json!([{"title": "Rust"}]),
            }),
        ];
        for block in blocks {
            let value = serde_json::to_value(&block).unwrap();
            assert_eq!(
                value.get("type").and_then(Value::as_str),
                Some(block.type_name()),
                "block `{}` serialized without its type tag: {value}",
                block.type_name()
            );
            let back: ContentBlock = serde_json::from_value(value.clone())
                .unwrap_or_else(|e| panic!("re-parsing `{}`: {e}", block.type_name()));
            assert_eq!(back, block, "round trip changed `{}`", block.type_name());
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                value,
                "serialization is not stable for `{}`",
                block.type_name()
            );
        }
    }

    #[test]
    fn an_unknown_block_is_not_re_tagged() {
        // `Other` came off the wire already tagged; adding one would duplicate it.
        let raw = json!({"type": "some_future_block", "payload": 1});
        let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    #[test]
    fn tool_result_serializes_in_the_shape_the_captures_show() {
        // The exact case that was broken: replayed history sends this.
        let block = ContentBlock::ToolResult(ToolResultBlock::text("call_00_x", "14:32 JST"));
        let value = serde_json::to_value(&block).unwrap();
        assert_eq!(value["type"], "tool_result");
        assert_eq!(value["tool_use_id"], "call_00_x");
        assert_eq!(value["content"], "14:32 JST");
    }

    #[test]
    fn message_content_accepts_bare_string_and_array() {
        let a: MessageContent = serde_json::from_value(json!("hello")).unwrap();
        assert_eq!(a.concat_text(), "hello");
        let b: MessageContent = serde_json::from_value(json!([{"type":"text","text":"hi"}])).unwrap();
        assert_eq!(b.concat_text(), "hi");
    }

    #[test]
    fn unknown_stop_reason_is_never_success() {
        let s: StopReason = serde_json::from_value(json!("some_new_reason")).unwrap();
        assert_eq!(s, StopReason::Unknown("some_new_reason".to_owned()));
        assert_eq!(
            s.disposition(),
            StopDisposition::Unrecognized("some_new_reason".to_owned())
        );
        assert_ne!(s.disposition(), StopDisposition::Complete);
    }

    #[test]
    fn max_tokens_is_truncation_not_completion() {
        assert_eq!(
            StopReason::MaxTokens.disposition(),
            StopDisposition::Truncated
        );
    }

    #[test]
    fn signature_is_not_folded_into_thinking_text() {
        let b: ContentBlock = serde_json::from_value(
            json!({"type":"thinking","thinking":"short","signature":"69164b36-fbed-4709-adfd-d6871900f038"}),
        )
        .unwrap();
        let t = b.as_thinking().unwrap();
        assert_eq!(t.thinking, "short");
        assert_eq!(t.signature.as_deref().unwrap().len(), 36);
    }

    #[test]
    fn cache_breakpoints_are_counted_across_the_request() {
        let mut req = MessagesRequest::new("m", 16, vec![Message::user("hi")]);
        req.system = Some(vec![SystemBlock::text("sys").with_cache()]);
        assert_eq!(req.cache_breakpoints(), 1);
        req.messages.push(Message::user(vec![ContentBlock::ToolResult(
            ToolResultBlock {
                tool_use_id: "x".into(),
                content: Some(ToolResultContent::Text("r".into())),
                is_error: None,
                cache_control: Some(CacheControl::ephemeral()),
            },
        )]));
        assert_eq!(req.cache_breakpoints(), 2);
    }

    #[test]
    fn text_editor_tool_name_is_the_fixed_one() {
        // A custom tool with an arbitrary name is a *different* tool.
        assert_eq!(ServerTool::text_editor().name, "str_replace_based_edit_tool");
        assert_eq!(ServerTool::bash().name, "bash");
    }
}
