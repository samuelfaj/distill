//! Anthropic Messages API (`/v1/messages`) wire types.

use serde::{Deserialize, Serialize};

// ============================================================================
// Request Types
// ============================================================================

/// POST /v1/messages request body
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolParam>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoiceParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputFormat {
    JsonSchema { schema: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_config: Option<OutputConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemParam {
    Text(String),
    Blocks(Vec<TextBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBlock {
    #[serde(rename = "type")]
    pub r#type: String, // always "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub r#type: String, // "ephemeral"
    /// Entry lifetime; absent is the API default of five minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

/// The one-hour cache lifetime (billed at 2x the input price to write, against 1.25x for the default).
pub const EXTENDED_CACHE_TTL: &str = "1h";

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            r#type: "ephemeral".to_owned(),
            ttl: None,
        }
    }
}

impl MessagesRequest {
    /// Sets `ttl` on every breakpoint the request carries and returns how many it touched.
    /// One lifetime for all of them keeps the API's rule that a longer-lived entry never follows a shorter one.
    pub fn set_cache_ttl(&mut self, ttl: Option<&str>) -> usize {
        let mut touched = 0;
        let mut set = |cache_control: &mut Option<CacheControl>| {
            if let Some(cache_control) = cache_control {
                cache_control.ttl = ttl.map(str::to_owned);
                touched += 1;
            }
        };
        if let Some(SystemParam::Blocks(blocks)) = &mut self.system {
            for block in blocks {
                set(&mut block.cache_control);
            }
        }
        for message in &mut self.messages {
            let MessageContent::Blocks(blocks) = &mut message.content else {
                continue;
            };
            for block in blocks {
                match block {
                    ContentBlock::Text { cache_control, .. }
                    | ContentBlock::Image { cache_control, .. }
                    | ContentBlock::ToolUse { cache_control, .. }
                    | ContentBlock::ToolResult { cache_control, .. } => set(cache_control),
                    ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ToolAddition { .. } => {}
                }
            }
        }
        touched
    }
}

/// Content blocks used in both requests and responses
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Image {
        source: ImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolResultContent,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Encrypted reasoning the model chose to redact: an opaque `data` blob, never plaintext.
    /// Parsed so a stream carrying one deserializes instead of failing the whole event parse; request-building and the sampler never construct one.
    RedactedThinking {
        data: String,
    },
    /// Offers a tool declared with `defer_loading` from this point of the conversation on. Sent only in a system-role
    /// message, under the mid-conversation tool changes beta; never part of a response.
    ToolAddition {
        tool: ToolChangeTarget,
    },
}

/// The tool a [`ContentBlock::ToolAddition`] names: one declared in `tools`, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChangeTarget {
    ToolReference { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// Tool definition (Anthropic Messages API format)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    /// Withheld from the model until a [`ContentBlock::ToolAddition`] offers it. A deferred tool is not part of the
    /// rendered prompt, so declaring it from the first request keeps the cached prefix when it joins later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
}

/// Tool choice (Anthropic Messages API format)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoiceParam {
    Auto,
    Any,
    Tool { name: String },
}

/// Three modes per the Anthropic Messages API: Adaptive: 4.6+ models, API decides budget; Enabled: 4.0-4.5 models,
/// explicit budget_tokens; Disabled: pre-thinking models or thinking_budget=0.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Omitted,
    Summarized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ThinkingConfig {
    Enabled {
        budget_tokens: u32,
    },
    Adaptive {
        // Newer thinking-capable models omit thinking content unless display = "summarized".
        // Older models ignore this field; skipping `None` keeps the old wire shape
        #[serde(skip_serializing_if = "Option::is_none")]
        display: Option<ThinkingDisplay>,
    },
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

// ============================================================================
// Response Types
// ============================================================================

/// Non-streaming response from POST /v1/messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub r#type: String, // "message"
    pub role: String, // "assistant"
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<StopReason>,
    pub usage: MessagesUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    StopSequence,
    Refusal,
    PauseTurn,
    ModelContextWindowExceeded,
    /// Catch-all so a new server-side stop reason never fails the terminal `message_delta` parse and discards an already-streamed response.
    /// Preserves the wire string for logging and faithful re-serialization.
    /// Must stay the LAST variant: serde tries the tagged variants above first.
    #[serde(untagged)]
    Unknown(String),
}

impl StopReason {
    /// The verbatim wire string, derived from the serde `snake_case` renames so it cannot drift from the wire contract.
    /// `Unknown` yields its inner string unchanged.
    pub fn wire_str(&self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(s)) => s,
            other => {
                debug_assert!(
                    false,
                    "StopReason must serialize to a string, got {other:?}"
                );
                "end_turn".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagesUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    /// How `cache_creation_input_tokens` splits by lifetime. Absent on endpoints
    /// that do not report it; the whole write then counts as five-minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreationUsage>,
}

/// The `usage.cache_creation` breakdown of a Messages response: tokens written
/// with each cache lifetime. The two sum to `cache_creation_input_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheCreationUsage {
    #[serde(default)]
    pub ephemeral_5m_input_tokens: u32,
    #[serde(default)]
    pub ephemeral_1h_input_tokens: u32,
}

// ============================================================================
// Streaming Event Types
// ============================================================================

/// Top-level streaming event (SSE `type` field determines variant)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageStreamEvent {
    MessageStart {
        message: MessagesResponse,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        usage: MessageDeltaUsage,
    },
    MessageStop,
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: StreamDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    Ping,
    Error {
        error: StreamError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    pub stop_reason: Option<StopReason>,
    /// The stop sequence that was matched, present only when `stop_reason == "stop_sequence"`; `None` otherwise.
    /// Consumers echo it on the Messages API `message.stop_sequence`.
    /// Optional so its absence never fails the terminal parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// Provider detail for the stop; on `refusal`, `explanation` carries the
    /// reason the request was blocked (e.g. an Anthropic ToS auto-refusal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_details: Option<StopDetails>,
}

/// Detail for a terminal `message_delta`, e.g. `{"type":"refusal","category":"frontier_llm","explanation":"..."}`.
/// All fields optional so an unknown shape never fails the terminal parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StopDetails {
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageDeltaUsage {
    pub output_tokens: u32,
    #[serde(default)]
    pub input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreationUsage>,
}

/// Content delta within a content_block_delta event
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamError {
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_deserializes_all_known_values_and_catches_unknown() {
        let parse = |raw: &str| -> StopReason {
            serde_json::from_str(&format!("\"{raw}\""))
                .unwrap_or_else(|e| panic!("stop_reason {raw:?} must parse: {e}"))
        };
        assert!(matches!(parse("end_turn"), StopReason::EndTurn));
        assert!(matches!(parse("max_tokens"), StopReason::MaxTokens));
        assert!(matches!(parse("tool_use"), StopReason::ToolUse));
        assert!(matches!(parse("stop_sequence"), StopReason::StopSequence));
        assert!(matches!(parse("refusal"), StopReason::Refusal));
        assert!(matches!(parse("pause_turn"), StopReason::PauseTurn));
        assert!(matches!(
            parse("model_context_window_exceeded"),
            StopReason::ModelContextWindowExceeded
        ));
        match parse("some_future_stop_reason") {
            StopReason::Unknown(s) => assert_eq!(s, "some_future_stop_reason"),
            other => panic!("unknown value must preserve the wire string, got {other:?}"),
        }

        // wire_str is the inverse: known variants round-trip through the serde renames, Unknown yields its inner string unchanged
        assert_eq!(StopReason::MaxTokens.wire_str(), "max_tokens");
        assert_eq!(
            StopReason::ModelContextWindowExceeded.wire_str(),
            "model_context_window_exceeded"
        );
        assert_eq!(
            StopReason::Unknown("some_future_stop_reason".to_string()).wire_str(),
            "some_future_stop_reason"
        );
        assert_eq!(
            serde_json::to_string(&StopReason::Unknown("some_future_stop_reason".into())).unwrap(),
            "\"some_future_stop_reason\"",
            "catch-all must re-serialize the wire string faithfully"
        );
        // The catch-all must also work through the Option<StopReason> field it is parsed from in production
        let delta: MessageDeltaBody =
            serde_json::from_str(r#"{"stop_reason":"mystery_reason"}"#).unwrap();
        match delta.stop_reason {
            Some(StopReason::Unknown(s)) => assert_eq!(s, "mystery_reason"),
            other => panic!("expected Unknown through Option, got {other:?}"),
        }
    }

    /// The terminal `message_delta` of a refusal-terminated stream must parse.
    /// The fixture is a full event because the internally-tagged `MessageStreamEvent` wrapper is the production parse site.
    #[test]
    fn message_delta_with_refusal_stop_reason_parses() {
        let event: MessageStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":5,"input_tokens":10}}"#,
        )
        .expect("refusal message_delta must deserialize");
        match event {
            MessageStreamEvent::MessageDelta { delta, usage } => {
                assert!(matches!(delta.stop_reason, Some(StopReason::Refusal)));
                assert!(delta.stop_details.is_none(), "no stop_details on the wire");
                assert_eq!(usage.output_tokens, 5);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    /// A refusal `message_delta` carrying `stop_details` (as emitted by
    /// Anthropic ToS auto-refusals) must parse and preserve the explanation,
    /// and unknown keys inside `stop_details` must not fail the parse.
    #[test]
    fn message_delta_with_refusal_stop_details_parses() {
        let event: MessageStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"frontier_llm","explanation":"This request was blocked.","future_key":42}},"usage":{"output_tokens":0}}"#,
        )
        .expect("refusal message_delta with stop_details must deserialize");
        match event {
            MessageStreamEvent::MessageDelta { delta, .. } => {
                assert!(matches!(delta.stop_reason, Some(StopReason::Refusal)));
                let details = delta.stop_details.expect("stop_details must be captured");
                assert_eq!(details.r#type.as_deref(), Some("refusal"));
                assert_eq!(details.category.as_deref(), Some("frontier_llm"));
                assert_eq!(
                    details.explanation.as_deref(),
                    Some("This request was blocked.")
                );
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    /// A `stop_sequence`-terminated `message_delta` must parse and preserve the matched string.
    /// Consumers echo it on the Messages API `message.stop_sequence`.
    #[test]
    fn message_delta_captures_matched_stop_sequence() {
        let event: MessageStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"stop_sequence","stop_sequence":"END"},"usage":{"output_tokens":7}}"#,
        )
        .expect("stop_sequence message_delta must deserialize");
        match event {
            MessageStreamEvent::MessageDelta { delta, .. } => {
                assert!(matches!(delta.stop_reason, Some(StopReason::StopSequence)));
                assert_eq!(delta.stop_sequence.as_deref(), Some("END"));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }

        // Absent `stop_sequence` stays `None` and never fails the parse.
        let event: MessageStreamEvent = serde_json::from_str(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#,
        )
        .expect("end_turn message_delta must deserialize");
        match event {
            MessageStreamEvent::MessageDelta { delta, .. } => {
                assert_eq!(delta.stop_sequence, None);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    /// A `redacted_thinking` content block must deserialize into the dedicated variant, preserving the opaque `data`.
    /// Failing the whole `content_block_start` parse would discard an already-streamed response.
    #[test]
    fn redacted_thinking_content_block_parses() {
        let event: MessageStreamEvent = serde_json::from_str(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"EvwBCkgY...opaque"}}"#,
        )
        .expect("redacted_thinking content_block_start must deserialize");
        match event {
            MessageStreamEvent::ContentBlockStart { content_block, .. } => match content_block {
                ContentBlock::RedactedThinking { data } => {
                    assert_eq!(data, "EvwBCkgY...opaque");
                }
                other => panic!("expected RedactedThinking, got {other:?}"),
            },
            other => panic!("expected ContentBlockStart, got {other:?}"),
        }

        // Round-trips to Claude's wire shape.
        let json =
            serde_json::to_value(ContentBlock::RedactedThinking { data: "abc".into() }).unwrap();
        assert_eq!(
            json.get("type"),
            Some(&serde_json::json!("redacted_thinking"))
        );
        assert_eq!(json.get("data"), Some(&serde_json::json!("abc")));
    }

    /// The mid-conversation tool change and the deferred declaration must match the documented wire shape exactly:
    /// the API answers a malformed one with a 400, which turns the feature off for the rest of the process.
    #[test]
    fn tool_addition_and_defer_loading_serialize_to_the_documented_shape() {
        let block = ContentBlock::ToolAddition {
            tool: ToolChangeTarget::ToolReference {
                name: "generate_image".to_owned(),
            },
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            serde_json::json!({
                "type": "tool_addition",
                "tool": {"type": "tool_reference", "name": "generate_image"}
            })
        );
        let tool = ToolParam {
            name: "generate_image".to_owned(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            defer_loading: Some(true),
        };
        assert_eq!(serde_json::to_value(&tool).unwrap()["defer_loading"], true);
        let plain = ToolParam {
            defer_loading: None,
            ..tool
        };
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("defer_loading")
                .is_none()
        );
    }

    #[test]
    fn output_format_json_schema_wire_shape() {
        let fmt = OutputFormat::JsonSchema {
            schema: serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        };
        let json = serde_json::to_value(&fmt).unwrap();
        assert_eq!(json.get("type"), Some(&serde_json::json!("json_schema")));
        assert_eq!(
            json.get("schema").and_then(|s| s.get("type")),
            Some(&serde_json::json!("object"))
        );
        assert!(json.get("name").is_none());

        let config = OutputConfig {
            effort: None,
            format: Some(fmt),
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("effort").is_none(), "effort omitted when None");
        assert_eq!(
            json.get("format").and_then(|f| f.get("type")),
            Some(&serde_json::json!("json_schema"))
        );
    }
}
