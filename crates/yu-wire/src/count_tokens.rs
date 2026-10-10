//! Token counting.
//!
//! DESIGN: "本地计数用 `POST /v1/messages/count_tokens`". Nothing here estimates
//! locally — a local estimate is exactly the kind of number that looks precise
//! and quietly misleads, which is the failure mode this project exists to avoid.

use serde::{Deserialize, Serialize};

use crate::client::{COUNT_TOKENS_PATH, WireClient};
use crate::error::{ErrorKind, WireError};
use crate::types::{Message, SystemBlock, Thinking, ToolDefinition};

#[derive(Debug, Clone, Serialize)]
pub struct CountTokensRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<&'a [SystemBlock]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<&'a [ToolDefinition]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<&'a Thinking>,
}

impl<'a> CountTokensRequest<'a> {
    pub fn new(model: &'a str, messages: &'a [Message]) -> Self {
        Self {
            model,
            messages,
            system: None,
            tools: None,
            thinking: None,
        }
    }

    pub fn system(mut self, system: &'a [SystemBlock]) -> Self {
        self.system = Some(system);
        self
    }

    pub fn tools(mut self, tools: &'a [ToolDefinition]) -> Self {
        self.tools = Some(tools);
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CountTokensResponse {
    pub input_tokens: u64,
}

impl WireClient {
    /// Count the input tokens a request would consume.
    ///
    /// Note the measured caveat in `docs/WIRE-FACTS.md` Quirk 4: on the probe
    /// endpoint `input_tokens` **excludes thinking blocks**, so the same turn
    /// with and without 86 characters of thinking reports the same count. Do not
    /// use this to detect whether thinking survived into a request.
    pub async fn count_tokens(&self, req: &CountTokensRequest<'_>) -> Result<u64, WireError> {
        let body = serde_json::to_value(req).map_err(|e| {
            WireError::new(ErrorKind::CountTokens, format!("could not encode: {e}"))
        })?;
        let (status, text, _) = self.raw_request(reqwest::Method::POST, COUNT_TOKENS_PATH, Some(&body), &[]).await?;
        if !(200..300).contains(&status) {
            return Err(WireError::from_response(status, &text, None));
        }
        let parsed: CountTokensResponse = serde_json::from_str(&text).map_err(|e| {
            WireError::new(
                ErrorKind::CountTokens,
                format!("count_tokens response did not parse: {e}"),
            )
        })?;
        Ok(parsed.input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_omits_absent_fields() {
        let messages = vec![Message::user("hi")];
        let req = CountTokensRequest::new("m", &messages);
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("system").is_none());
        assert!(v.get("tools").is_none());
        assert_eq!(v["model"], "m");
    }
}
