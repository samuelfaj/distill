// Modified for Distill by Samuel Fajreldines, 2026.
//! Tool-call and model-response product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct ToolCallCompleted {
    pub tool_name: String,
    pub outcome: distill_session_events::types::ToolOutcome,
    /// Content-free: the hook name is kept out of OTLP and product events and rides only the session-event row.
    pub hook_rewrote: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result_size_bytes: Option<u64>,
    /// Model at call time, for the external stream only (`#[serde(skip)]`).
    #[serde(skip)]
    pub model_id: String,
    /// Primary file path of the call, for the external stream only (`#[serde(skip)]`: never serialized to product events/analytics).
    /// Always reduced to `file_extension`; the full path rides the `OTEL_LOG_TOOL_DETAILS` gate.
    #[serde(skip)]
    pub file_path: Option<String>,
    /// Tool parameters for the external stream's `OTEL_LOG_TOOL_DETAILS`
    /// 4 KB preview **and** `OTEL_LOG_TOOL_CONTENT` full `tool_input`
    /// (`#[serde(skip)]`; reduced / capped at emit time).
    #[serde(skip)]
    pub parameters: Option<serde_json::Value>,
    /// Tool-call id for the external stream (`#[serde(skip)]`; always-on
    /// join key, not content).
    #[serde(skip)]
    pub tool_use_id: Option<String>,
    /// Tool result body for `OTEL_LOG_TOOL_CONTENT` (`#[serde(skip)]`).
    #[serde(skip)]
    pub tool_output: Option<String>,
    /// Failure text for `OTEL_LOG_TOOL_CONTENT` (`#[serde(skip)]`).
    #[serde(skip)]
    pub error_message: Option<String>,
}

#[derive(Serialize)]
pub struct ModelResponseReceived {
    pub model_id: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// USD ticks (1e10 ticks = $1); `None` when unpriced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
}

/// Emitted once per turn via [`crate::external::emit`] (not [`crate::session_ctx::log_event`]), so Mixpanel never
/// receives `assistant_response`. Thinking/tool-use blocks are excluded at the source. `response_length` always exports
/// on the external event; `response_text` is `#[serde(skip)]` and gated by `OTEL_LOG_ASSISTANT_RESPONSES`.
#[derive(Serialize)]
pub struct AssistantResponse {
    /// Char/byte count of the assembled text blocks (always-on, like
    /// `prompt_length`). Zero on tool-only turns.
    pub response_length: usize,
    /// Gated by `OTEL_LOG_ASSISTANT_RESPONSES`; omitted when length is 0.
    #[serde(skip)]
    pub response_text: Option<String>,
}
