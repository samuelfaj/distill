//! Prompt submission, suggestion, latency, and ack product telemetry events.

use super::McpStrategy;
use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PromptSuggestionAction {
    /// A suggestion loaded and rendered as ghost text in the prompt input.
    Shown,
    /// The user accepted the ghost text (Tab / Right arrow).
    Accepted,
    /// The user explicitly dismissed the ghost text (Esc).
    Dismissed,
    SkippedCatalog,
    Fetched,
    FetchedEmpty,
    Filtered,
    FetchFailed,
}

/// Prompt-suggestion telemetry never includes suggestion text.
#[derive(Serialize)]
pub struct PromptSuggestion {
    pub action: PromptSuggestionAction,
    /// Length of the full suggestion in characters (content-free size signal: are long or short suggestions likelier to be accepted?).
    pub chars: usize,
    /// Number of whitespace-separated words in the suggestion.
    pub words: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Serialize)]
pub struct PromptSubmitted {
    pub prompt_length: usize,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    /// Pager screen mode from the prompt request `_meta.screenMode` (`fullscreen` | `inline` | `minimal` | `headless`).
    /// `None` for non-pager clients and synthetic prompts (goal summaries, drains, interjections).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_mode: Option<String>,
    /// Raw prompt text for the external stream's `OTEL_LOG_USER_PROMPTS` gate **only**.
    /// `#[serde(skip)]`: never serialized to product events/analytics.
    /// Dropped at external emit time unless the gate is on (then capped at 60 KB and secret-scrubbed).
    #[serde(skip)]
    pub prompt_text: Option<String>,
    /// Slash/skill command name for the external `command_name` attr.
    /// Always-on metadata (not user prompt text). `#[serde(skip)]`.
    #[serde(skip)]
    pub command_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PromptLatency {
    pub turn_index: u32,
    pub total_ms: u64,
    pub mcp_wait_ms: u64,
    pub tool_collection_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_status_wait_ms: Option<u64>,
    pub model_call_ms: u64,
    pub pre_model_ms: u64,
    pub mcp_server_count: u32,
    pub mcp_tools_registered: u32,
    pub mcp_strategy: McpStrategy,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    pub ttlb_ms: u64,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u32>,
    pub before_first_model_ms: u64,
    pub sampling_ms: u64,
    pub tool_blocking_ms: u64,
    pub compaction_ms: u64,
    pub between_sampling_overhead_ms: u64,
    pub after_last_sampling_ms: u64,
    pub turn_total_ms: u64,
    pub sampling_request_count: u32,
    pub sampling_retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfm_ms: Option<u64>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptAckSurface {
    Tui,
    Headless,
}

/// What the client did with the unacknowledged prompt's text.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptAckDisposition {
    RestoredToComposer,
    MergedIntoDraft,
    /// Nothing went back to a composer: skill / wire-block / bash prompts, a composer busy editing a
    /// queued row, a draft that already carries images, or the headless runner.
    NotRestorable,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptAckPromptKind {
    Prompt,
    Bash,
    Skill,
}

/// The client sent `session/prompt` and saw no acknowledgment (queue broadcast,
/// update, or response naming the prompt) within its limit, so it aborted the
/// turn locally. `waited_ms` is the observed wait at the abort.
#[derive(Serialize)]
pub struct PromptAckTimeoutFired {
    pub limit_ms: u64,
    pub waited_ms: u64,
    pub surface: PromptAckSurface,
    pub disposition: PromptAckDisposition,
    pub prompt_kind: PromptAckPromptKind,
}
