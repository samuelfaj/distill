// Modified for Distill by Samuel Fajreldines, 2026.
//! API-agnostic conversation representation.
//!
//! The types here capture a superset of what the backends accept, so a caller can switch between them by configuration.
//! Each backend owns its own wire conversion in a sibling module.

mod chat_completions;
mod messages;
mod responses;

pub use chat_completions::{conversation_item_to_chat_message, conversation_to_chat_messages};
pub use messages::build_messages_request;
pub use responses::{
    extra_tool_entries, patch_input_item_ids, patch_reasoning_text_types,
    response_to_conversation_items,
};

use std::sync::Arc;

const STRUCTURED_OUTPUT_SCHEMA_NAME: &str = "structured_output";

/// Truncate to at most `max_bytes`, walking back to a char boundary.
/// Plain `&s[..n]` panics when `n` lands inside a multi-byte character, which tool-call arguments routinely contain.
/// Public because `distill-shell` calls it.
pub fn truncate_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let Some(prefix) = s.get(..end) else {
        return "";
    };
    prefix
}

/// A provider that validates `function.arguments` rejects the whole request, so one malformed call from an earlier turn breaks every turn after it.
/// The matching `tool_result` keeps the original text, so the model can recover.
fn sanitize_tool_arguments(id: &str, name: &str, arguments: Arc<str>) -> Arc<str> {
    // `IgnoredAny` avoids building a DOM on a path that runs for every call.
    if serde_json::from_str::<serde::de::IgnoredAny>(&arguments).is_err() {
        tracing::warn!(
            tool_call_id = id,
            tool_name = name,
            args_preview = truncate_bytes(&arguments, 200),
            "Tool call has invalid JSON arguments; replacing with {{}} to prevent provider 400"
        );
        Arc::<str>::from("{}")
    } else {
        arguments
    }
}

use serde::{Deserialize, Serialize};

use crate::rs;
use crate::tool_overrides::{ToolOverrides, WebSearchOptions, XSearchOptions, drop_empty};
use crate::types::{
    ChatCompletionRequest, ChatContentBlock, ChatRequestMessage, ChatResponseMessage, FinishReason,
    ImageUrl, MessageContent, Role, ToolCallRequest, ToolChoice, ToolDefinition, TraceContext,
    Usage,
};

// ============================================================================
// Core Conversation Types
// ============================================================================

/// A single item in a conversation, the unified internal representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConversationItem {
    /// System instructions/prompt
    System(SystemItem),
    /// User input message
    User(UserItem),
    /// Assistant response
    Assistant(AssistantItem),
    /// Tool/function result
    ToolResult(ToolResultItem),
    /// A tool call executed server-side by the backend agentic sampler.
    /// The client does not execute these; the server already ran them and fed results into the model's context.
    /// Persisted to chat_history.jsonl for session replay/fork; Sent back to the Responses API as input items for context continuity; Rendered by the pager (search queries, sources, etc.).
    BackendToolCall(BackendToolCallItem),
    /// A reasoning item from the Responses API, stored as a sibling of the assistant message so that: N parallel `tco_*` reasoning items (one per backend tool call) round-trip losslessly without last-write-wins clobbering; The interleaved order of `[reasoning, tool_call, reasoning, ..., message]` produced by the model stays byte-stable across turns. That stability is what lets the server-side prefix KV-cache hit.
    /// Wraps `rs::ReasoningItem` directly so no field is dropped on the way through.
    Reasoning(rs::ReasoningItem),
}

/// System message content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemItem {
    pub content: Arc<str>,
    /// `Primary` is omitted on write and filled in when absent, so the leading prompt serializes as it did before this field existed.
    #[serde(
        default = "SyntheticReason::primary",
        skip_serializing_if = "SyntheticReason::is_primary"
    )]
    pub synthetic_reason: SyntheticReason,
}

/// Origin of a `UserItem` or `SystemItem`: typed by the user, the request's system prompt, or synthesized by the runtime for one of the listed reasons.
/// Stored so downstream code (pruning, replay, analytics) can tell synthetic injections from real input without parsing message text.
/// The two non-synthetic origins ([`Self::Human`], [`Self::Primary`]) live here so the field is never optional; each item type defaults to its own when the field is absent.
/// The wire field name stays `synthetic_reason` so old clients keep reading new sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyntheticReason {
    /// Typed by the user.
    #[default]
    Human,
    /// The system prompt that opens the conversation, ahead of every other item.
    Primary,
    /// Metadata injected by the compaction pipeline (e.g., re-read file contents).
    CompactionMeta,
    /// Runtime-injected `<system-reminder>` message.
    SystemReminder,
    /// Continue reminder after a salvaged Length truncation.
    /// Distinct from [`Self::SystemReminder`] so report assembly joins segments around exactly this reminder and no other.
    LengthContinue,
    /// Project-level instruction message (AGENTS.md / CLAUDE.md) injected at session spawn.
    /// Invariant: once placed, never replaced (replacing it would bust the KV-cache prefix).
    ProjectInstructions,
    /// Injected by the auto-continue logic after compaction so the agent keeps working.
    AutoContinue,
    /// Injected by the auto-recovery logic after a transient tool failure to retry the operation.
    AutoRecovery,
    /// User-initiated mid-turn interjection sent via Ctrl+Enter while the model was actively running.
    /// Injected between tool batches so the model sees it as steering context without canceling the turn.
    Interjection,
    /// Model-authored input sent by another agent.
    #[serde(alias = "parent_agent_message")]
    AgentMessage,
    /// Auto-wake synthetic prompt injected when a background bash task completed.
    /// Wakes the agent for a new turn.
    TaskCompleted,
    /// Auto-wake synthetic prompt injected when a background subagent completed.
    /// Wakes the agent for a new turn.
    SubagentCompleted,
    /// Idle-gated notification drain: batched monitor events and/or bash task completions drained when the session is idle.
    /// Wakes the agent.
    NotificationDrain,
    /// Goal orchestrator summary turn.
    /// The goal system triggers a model turn so it can print visible progress.
    /// Wakes the agent.
    GoalSummary,
    /// Goal-achievement classifier nudge injected after the classifier rejects an `update_goal(completed: true)` attempt.
    /// Wakes the agent with a "not yet achieved — keep working" reminder pointing at the persisted details file.
    GoalClassifierNudge,
    /// Scheduled task (`/loop`) prompt fired by the scheduler.
    /// Wakes the agent.
    SchedulerFired,
    /// Feedback from a `Stop`/`SubagentStop` hook that blocked the agent from stopping.
    /// Injected in-turn so the model keeps working within the same turn.
    StopHookFeedback,
    /// Working-directory switch context appended after a session relocation.
    /// Carries a generation marker so recovery can detect an existing append.
    WorkingDirectorySwitch,
    /// Human-authored text relayed from a parent session. Stays a `User` item.
    /// Reserved ahead of its producer so shipped readers classify it before anything writes it; today it shares [`Self::AgentMessage`].
    ParentHumanMessage,
    /// The startup `<user_info>` / rules / VCS-status prefix inserted after the primary prompt.
    /// Reserved ahead of its producer; today the prefix is an untagged `Human` item that the legacy turn walkers skip by position.
    SessionPrefix,
    /// The direct-bash (`!cmd`) command-and-output history message.
    /// Reserved ahead of its producer; today it is an untagged `Human` item.
    DirectBash,
    /// The goal rules and tracking policy that accompany a `/goal` objective.
    /// Reserved ahead of its producer; today rules and objective share one untagged `Human` item.
    GoalSetup,
    /// Catch-all for unknown/future variants.
    #[serde(other)]
    Unknown,
}

impl SyntheticReason {
    /// Serde default for `SystemItem.synthetic_reason`; `#[serde(default = "...")]` takes a function path, not a variant.
    pub fn primary() -> Self {
        Self::Primary
    }

    pub fn is_human(&self) -> bool {
        *self == Self::Human
    }

    pub fn is_primary(&self) -> bool {
        *self == Self::Primary
    }

    /// Whether an item with this reason **starts a prompt turn**, meaning the turn pipeline pushed it while consuming a `prompt_index` slot.
    /// That covers real prompts, auto-wake and other server-initiated turns, as opposed to a mid-turn injection that never incremented the index.
    /// Unknown future reasons fail safe as boundaries so older readers cannot merge a newer conversational origin into a prior turn.
    pub fn starts_prompt_turn(&self) -> bool {
        match self {
            Self::Human
            | Self::AgentMessage
            | Self::ParentHumanMessage
            | Self::DirectBash
            | Self::GoalSetup
            | Self::Unknown
            | Self::TaskCompleted
            | Self::SubagentCompleted
            | Self::NotificationDrain
            | Self::GoalClassifierNudge
            | Self::SchedulerFired => true,
            Self::Primary
            | Self::SessionPrefix
            | Self::CompactionMeta
            | Self::SystemReminder
            | Self::LengthContinue
            | Self::ProjectInstructions
            | Self::AutoContinue
            | Self::AutoRecovery
            | Self::Interjection
            | Self::GoalSummary
            | Self::StopHookFeedback
            | Self::WorkingDirectorySwitch => false,
        }
    }
}

/// How the user *fatally* interrupted (cancelled) the turn immediately preceding this *real* user message.
/// Set only on genuine user messages (`synthetic_reason == Human`) that directly follow a cancelled turn.
/// Automatic terminations (hook-denied, max-turns) are not user interrupts and never set this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorTurnInterrupt {
    /// Previous turn was aborted mid-flight (ESC / Ctrl+C while streaming or running tools).
    MidTurnAbort,
    /// User clicked "No" on a permission prompt, ending the previous turn.
    PermissionRejected,
    /// User cancelled a permission prompt (Cmd+C), ending the previous turn.
    PermissionCancelled,
    /// Forward-compat catch-all for variants written by newer clients.
    #[serde(other)]
    Unknown,
}

/// User message with text and optional images
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserItem {
    pub content: Vec<ContentPart>,
    /// `Human` is omitted on write and filled in when absent, so real user turns serialize as they did when this field was optional.
    #[serde(default, skip_serializing_if = "SyntheticReason::is_human")]
    pub synthetic_reason: SyntheticReason,
    /// Relocation generation for a working-directory switch reminder.
    /// Structural metadata keeps recovery dedup independent of reminder text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_generation: Option<u64>,
    /// Set on a genuine user message that directly follows a user-interrupted turn (see [`PriorTurnInterrupt`]).
    /// `None` for synthetic messages and for real messages that did not follow an interrupt.
    /// `skip_serializing_if` keeps old sessions/round-trips byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior_turn_interrupt: Option<PriorTurnInterrupt>,
    /// Prompt-turn index this user item started, recorded at push time.
    /// `None` for items that do not start a turn and for items persisted before this field existed.
    /// The recount can drift from this coordinate (interjection echoes, image-only prompts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_index: Option<usize>,
}

/// Assistant response with tool calls.
/// Reasoning items, when present, sit beside this item as `ConversationItem::Reasoning(_)` siblings preceding the assistant turn, not bundled here.
/// Old sessions on disk may still carry a `reasoning` field; serde silently ignores it on read (no `deny_unknown_fields`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantItem {
    /// Text content of the response
    pub content: Arc<str>,
    /// Tool calls made by the assistant (client must execute these locally)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// The model that generated this response
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "system_fingerprint",
        deserialize_with = "crate::serde_helpers::empty_string_as_none"
    )]
    pub model_fingerprint: Option<String>,
    /// The reasoning effort the server applied for this response, echoed on `response.reasoning.effort` (Responses API).
    /// Stored beside `model_id`/`model_fingerprint` so per-response effort survives mid-session model/effort switches.
    /// `None` for synthetic items and backends that don't echo it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<crate::ReasoningEffort>,
}

/// Tool result message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultItem {
    /// ID of the tool call this is responding to
    pub tool_call_id: String,
    /// The result content
    pub content: Arc<str>,
    /// Inline images associated with this tool result (e.g. from `read_file` on an image/PDF).
    /// When non-empty, the API conversion layers embed these directly in the tool result message rather than in a separate follow-up user message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ContentPart>,
}

/// A server-side tool call from the backend agentic sampler.
///
/// Wraps the typed Responses API output items so they can be round-tripped back to the server and rendered by the pager.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendToolCallItem {
    /// The specific backend tool that was called.
    pub kind: BackendToolKind,
}

impl BackendToolCallItem {
    /// The backend-tool-call id (the `id` field on the underlying `rs::WebSearchToolCall` / `rs::CustomToolCall` / `rs::CodeInterpreterToolCall`).
    /// Used by the legacy-session upgrader to dedupe against the same call when it also appears inside a sibling assistant's `raw_output` array.
    pub fn id(&self) -> &str {
        match &self.kind {
            BackendToolKind::WebSearch(ws) => ws.id.as_str(),
            BackendToolKind::XSearch(ct) => ct.id.as_str(),
            BackendToolKind::CodeInterpreter(ci) => ci.id.as_str(),
        }
    }

    /// Human-readable summary for token estimation and text extraction.
    pub fn text_summary(&self) -> String {
        match &self.kind {
            BackendToolKind::WebSearch(ws) => {
                let action_desc = match &ws.action {
                    rs::WebSearchToolCallAction::Search(s) => format!("search: {}", s.query),
                    rs::WebSearchToolCallAction::OpenPage(o) => {
                        format!("open: {}", o.url.as_deref().unwrap_or("?"))
                    }
                    rs::WebSearchToolCallAction::Find(f)
                    | rs::WebSearchToolCallAction::FindInPage(f) => {
                        format!("find \"{}\" in {}", f.pattern, f.url)
                    }
                };
                format!("[backend web_search] {action_desc}")
            }
            BackendToolKind::XSearch(ct) => {
                format!("[backend x_search] {}({})", ct.name, ct.input)
            }
            BackendToolKind::CodeInterpreter(ci) => {
                let code_preview = ci
                    .code
                    .as_deref()
                    .map(|c| {
                        if c.len() > 100 {
                            format!("{}...", truncate_bytes(c, 100))
                        } else {
                            c.to_string()
                        }
                    })
                    .unwrap_or_default();
                format!("[backend code_interpreter] {code_preview}")
            }
        }
    }
}

/// Discriminated union of backend-executed tool call types.
///
/// Each variant wraps the native Responses API struct, enabling zero-copy round-tripping when building subsequent API requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "tool_type", rename_all = "snake_case")]
pub enum BackendToolKind {
    /// Server-side web search (query + sources).
    WebSearch(rs::WebSearchToolCall),
    /// Server-side X/Twitter search (keyword, semantic, user, thread).
    XSearch(rs::CustomToolCall),
    /// Server-side code interpreter execution.
    CodeInterpreter(rs::CodeInterpreterToolCall),
}

// ============================================================================
// Content Parts
// ============================================================================

/// A part of message content: text, image, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Plain text content
    Text { text: Arc<str> },
    /// Image content (URL or base64 data URI)
    Image { url: Arc<str> },
}

// ============================================================================
// Reasoning Content
// ============================================================================

/// Reasoning/thinking content from the model.
/// Structured to support both plain text (chat completions) and encrypted reasoning (responses API).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningContent {
    /// Plain text reasoning (always available for display)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<Arc<str>>,
    /// Encrypted reasoning content (Responses API only)
    /// This can be passed back to the API for context continuity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<Arc<str>>,
    /// Original reasoning item ID from the Responses API.
    /// Required when replaying reasoning items in subsequent turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Arc<str>>,
}

impl ReasoningContent {
    /// Create from plain text reasoning (chat completions style)
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(Arc::<str>::from(text.into())),
            encrypted: None,
            id: None,
        }
    }

    /// Create from encrypted content (responses API style)
    pub fn from_encrypted(encrypted: impl Into<String>) -> Self {
        Self {
            text: None,
            encrypted: Some(Arc::<str>::from(encrypted.into())),
            id: None,
        }
    }

    /// Build from a Responses API `ReasoningItem`.
    /// Prefers `content` (full raw reasoning) over `summary` for the text field; in practice the API populates one or the other, never both.
    pub fn from_reasoning_item(r: &rs::ReasoningItem) -> Option<Self> {
        let text = Self::join_content(&r.content).or_else(|| Self::join_summary(&r.summary));
        if text.is_none() && r.encrypted_content.is_none() {
            return None;
        }
        Some(Self {
            text,
            encrypted: r.encrypted_content.as_deref().map(Arc::<str>::from),
            id: Some(Arc::<str>::from(r.id.as_str())),
        })
    }

    /// Check if there's any reasoning content
    pub fn is_empty(&self) -> bool {
        self.text.is_none() && self.encrypted.is_none()
    }

    fn join_content(content: &Option<Vec<rs::ReasoningTextContent>>) -> Option<Arc<str>> {
        let parts = content.as_ref()?;
        let joined: String = parts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (!joined.is_empty()).then_some(Arc::<str>::from(joined))
    }

    fn join_summary(summary: &[rs::SummaryPart]) -> Option<Arc<str>> {
        let joined: String = summary
            .iter()
            .map(|p| {
                let rs::SummaryPart::SummaryText(st) = p;
                st.text.as_str()
            })
            .collect::<Vec<_>>()
            .join("\n");
        (!joined.is_empty()).then_some(Arc::<str>::from(joined))
    }
}

// ============================================================================
// Tool Definitions and Calls
// ============================================================================

/// A tool call made by the assistant that the client must execute locally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique identifier for this tool call
    pub id: Arc<str>,
    /// Name of the function to call
    pub name: String,
    /// JSON-encoded arguments
    pub arguments: Arc<str>,
}

/// Tool/function definition for the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Name of the tool
    pub name: String,
    /// Description of what the tool does
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the parameters
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostedTool {
    WebSearch { options: Option<WebSearchOptions> },
    XSearch { options: Option<XSearchOptions> },
}

impl HostedTool {
    pub fn wire_name(&self) -> &'static str {
        match self {
            HostedTool::WebSearch { .. } => "web_search",
            HostedTool::XSearch { .. } => "x_search",
        }
    }
}

/// Resolve `overrides` onto the hosted tools in place so the serialized request matches the returned echo.
/// Empty options normalize to absent (via `drop_empty`), so a stray `{}` never clears a seeded bound.
/// Returns the applied overrides.
pub fn apply_tool_overrides(
    tools: &mut [HostedTool],
    overrides: Option<&ToolOverrides>,
) -> ToolOverrides {
    let mut applied = ToolOverrides::default();
    for tool in tools.iter_mut() {
        match tool {
            HostedTool::XSearch { options } => {
                if let Some(x) = drop_empty(
                    overrides.and_then(|o| o.x_search.clone()),
                    XSearchOptions::is_empty,
                ) {
                    *options = Some(x);
                }
                applied.x_search = drop_empty(options.clone(), XSearchOptions::is_empty);
            }
            HostedTool::WebSearch { options } => {
                if let Some(w) = drop_empty(
                    overrides.and_then(|o| o.web_search.clone()),
                    WebSearchOptions::is_empty,
                ) {
                    *options = Some(w);
                }
                applied.web_search = drop_empty(options.clone(), WebSearchOptions::is_empty);
            }
        }
    }
    applied
}

impl From<ToolDefinition> for ToolSpec {
    fn from(td: ToolDefinition) -> Self {
        Self {
            name: td.function.name,
            description: td.function.description,
            parameters: td.function.parameters,
        }
    }
}

// ============================================================================
// Conversation Request
// ============================================================================

/// What the sampler does with a completed response whose stop reason is `Length` (max_tokens truncation).
/// `Length` can arrive far below any client budget.
/// Callers that can use partial text opt into `CompletePartial`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LengthPolicy {
    /// Fail the attempt with `MaxTokensTruncation` (legacy behavior).
    Fail,
    /// Complete a response whose tool calls all carry complete arguments; text-only and empty `Length` still fail.
    /// `Length` is usually context exhaustion (the output budget is the window minus the prompt), so retrying cannot succeed.
    /// The default: a caller that does not choose gets its tool calls run and its text-only truncation failed.
    #[default]
    CompleteToolCalls,
    /// Additionally complete with partial text; empty `Length` still fails.
    CompletePartial,
}

/// Outcome of applying a [`LengthPolicy`] to a completed response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LengthVerdict {
    /// The stop reason is not `Length`; the policy does not apply.
    Pass,
    /// A `Length` stop delivered with its partial text content.
    Salvage,
    /// A `Length` stop delivered with its completed tool calls.
    SalvageToolCalls,
    /// A `Length` stop the policy rejects (`MaxTokensTruncation`).
    Fail,
}

impl LengthPolicy {
    /// The fail-vs-salvage decision for a completed response, including the `Length` stop-reason check.
    /// The Grok server never emits a completed `tool_calls` entry for a truncated call.
    /// ChatCompletions and Responses rewrite Length-with-tools to `ToolCalls` at the stream layer, so those never reach it.
    pub fn verdict(self, response: &ConversationResponse) -> LengthVerdict {
        if response.stop_reason != Some(StopReason::Length) {
            return LengthVerdict::Pass;
        }
        let tool_calls = response.tool_calls();
        if !tool_calls.is_empty() {
            // Empty arguments are the zero-arg call convention, not proof of truncation
            // A call cut before its first argument delta also collects as empty
            // Executing it as `{}` just bounces off tool-argument validation, which is cheaper than failing the turn
            let all_arguments_complete = tool_calls.iter().all(|tc| {
                tc.arguments.trim().is_empty()
                    || serde_json::from_str::<serde::de::IgnoredAny>(&tc.arguments).is_ok()
            });
            return match (self, all_arguments_complete) {
                (LengthPolicy::Fail, _)
                | (LengthPolicy::CompleteToolCalls | LengthPolicy::CompletePartial, false) => {
                    LengthVerdict::Fail
                }
                (LengthPolicy::CompleteToolCalls | LengthPolicy::CompletePartial, true) => {
                    LengthVerdict::SalvageToolCalls
                }
            };
        }
        if self == LengthPolicy::CompletePartial && response.empty_reason().is_none() {
            LengthVerdict::Salvage
        } else {
            LengthVerdict::Fail
        }
    }
}

/// A complete conversation request that can be sent to either API.
#[derive(Debug, Clone, Default)]
pub struct ConversationRequest {
    /// The conversation items (messages)
    pub items: Vec<ConversationItem>,
    /// Available tools (client-side, sent as Function definitions)
    pub tools: Vec<ToolSpec>,
    /// Backend-hosted tools (sent as native Responses API tool types).
    /// These are executed server-side by the agentic sampler during inference.
    pub hosted_tools: Vec<HostedTool>,
    /// Tool choice behavior
    pub tool_choice: Option<ConversationToolChoice>,
    /// Model to use (if not using client default)
    pub model: Option<String>,
    /// Sampling temperature
    pub temperature: Option<f32>,
    /// Maximum output tokens
    pub max_output_tokens: Option<u32>,
    /// Top-p sampling
    pub top_p: Option<f32>,
    /// Custom headers for Grok tracking
    pub x_grok_conv_id: Option<String>,
    pub x_grok_req_id: Option<String>,
    pub x_grok_session_id: Option<String>,
    pub x_grok_turn_idx: Option<String>,
    /// Turn-level resubmit attempt (absent on first submissions); sent as `x-grok-transient-retry` so the proxy can count retry traffic.
    pub x_grok_transient_retry: Option<String>,
    pub x_grok_agent_id: Option<String>,
    pub x_grok_deployment_id: Option<String>,
    pub x_grok_user_id: Option<String>,
    /// Optional opaque tracing context (e.g., where to persist the finalized request payload).
    /// Consumers downcast via `trace.as_ref().unwrap().as_any().downcast_ref::<T>()`.
    pub trace: Option<Box<dyn TraceContext>>,
    /// Caller span's W3C `traceparent`; the sampler parents its streaming HTTP span under it.
    /// Non-streaming calls ignore it.
    pub traceparent: Option<String>,
    /// Reasoning effort level for reasoning models.
    pub reasoning_effort: Option<crate::ReasoningEffort>,
    /// JSON Schema for structured output (strict mode).
    pub json_schema: Option<serde_json::Value>,
    /// Sticky routing key for prompt-cache reuse; overrides `x_grok_conv_id` for routing.
    pub prompt_cache_key: Option<String>,
    /// What the sampler does when the response stops with `Length`.
    pub length_policy: LengthPolicy,
}

impl ConversationRequest {
    /// Strip every image; returns the stripped URLs.
    pub fn strip_images(&mut self) -> Vec<Arc<str>> {
        strip_images_where(&mut self.items, |_| true)
    }
}

/// Strip only `urls`.
/// Invariant: replaces parts in place, never adds or removes a `ConversationItem` (the `&mut [_]` signature cannot resize).
pub fn strip_images_by_url(items: &mut [ConversationItem], urls: &[Arc<str>]) -> usize {
    strip_images_where(items, |url| urls.iter().any(|u| u.as_ref() == url)).len()
}

/// Replaces a stripped user image.
/// Deliberately verbose, like the eviction placeholder: a silently-stripped image otherwise induces confident hallucination of its contents.
pub const IMAGE_STRIP_PLACEHOLDER: &str = "[image removed — the server could not process it; \
     its contents are unavailable. Ask the user to re-attach the image if it is still needed.]";

/// User images become [`IMAGE_STRIP_PLACEHOLDER`]; tool-result images are dropped (a placeholder there is invisible to the conversion layers).
fn strip_images_where(
    items: &mut [ConversationItem],
    mut should_strip: impl FnMut(&str) -> bool,
) -> Vec<Arc<str>> {
    let mut stripped = Vec::new();
    for item in items {
        match item {
            ConversationItem::User(user) => {
                for part in &mut user.content {
                    match part {
                        ContentPart::Image { url } if should_strip(url) => {
                            stripped.push(Arc::clone(url));
                            *part = ContentPart::Text {
                                text: Arc::<str>::from(IMAGE_STRIP_PLACEHOLDER),
                            };
                        }
                        ContentPart::Image { .. } | ContentPart::Text { .. } => {}
                    }
                }
            }
            ConversationItem::ToolResult(t) => {
                t.images.retain(|part| match part {
                    ContentPart::Image { url } if should_strip(url) => {
                        stripped.push(Arc::clone(url));
                        false
                    }
                    ContentPart::Image { .. } | ContentPart::Text { .. } => true,
                });
            }
            // Exhaustive on purpose, the items here and the content parts above
            // A future image-bearing variant of either must choose its strip behavior here, not silently keep images
            ConversationItem::System(_)
            | ConversationItem::Assistant(_)
            | ConversationItem::BackendToolCall(_)
            | ConversationItem::Reasoning(_) => {}
        }
    }
    stripped
}

/// Tool choice options
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationToolChoice {
    /// Model decides whether to use tools
    Auto,
    /// Model must not use tools
    None,
    /// Model must use a tool
    Required,
    /// Model must use a specific tool
    Function(String),
}

// ============================================================================
// Conversation Response
// ============================================================================

/// Why the model stopped generating.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::AsRefStr, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StopReason {
    /// Model finished naturally
    Stop,
    /// Hit token limit
    Length,
    /// Model wants to call tools
    ToolCalls,
    /// Content was filtered
    ContentFilter,
}
impl From<FinishReason> for StopReason {
    fn from(fr: FinishReason) -> Self {
        match fr {
            FinishReason::Stop => StopReason::Stop,
            FinishReason::Length => StopReason::Length,
            FinishReason::ToolCalls | FinishReason::FunctionCall => StopReason::ToolCalls,
            FinishReason::ContentFilter => StopReason::ContentFilter,
        }
    }
}

/// Token usage statistics, normalized across OpenAI Chat Completions, OpenAI Responses, and
/// Anthropic Messages backends. `prompt_tokens` is always the FULL prompt size (uncached + cache
/// reads + cache writes) and `cached_prompt_tokens` is only the cache-hit subset; do not subtract.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub reasoning_tokens: u32,
    /// Prompt tokens served from cache (the cache-hit subset of `prompt_tokens`).
    /// OpenAI: `prompt_tokens_details.cached_tokens`. Messages: `cache_read_input_tokens`.
    #[serde(default)]
    pub cached_prompt_tokens: u32,
    /// Prompt tokens written to cache this call (Messages `cache_creation_input_tokens`, billed at ~1.25x).
    /// Part of `prompt_tokens` but distinct from cache reads; 0 on backends without a cache-write signal.
    #[serde(default)]
    pub cache_creation_prompt_tokens: u32,
}

impl TokenUsage {
    pub fn record_on_span(&self, span: &tracing::Span) {
        span.record("prompt_tokens", self.prompt_tokens);
        span.record("completion_tokens", self.completion_tokens);
        span.record("reasoning_tokens", self.reasoning_tokens);
        span.record("cached_prompt_tokens", self.cached_prompt_tokens);
    }
}

/// Convert a finite, non-negative USD amount to the exact integer tick scale
/// used by the billing ledger (1 USD = 1e10 ticks).
pub fn usd_cost_to_ticks(cost_usd: f64) -> Option<i64> {
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return None;
    }
    let ticks = cost_usd * 1e10;
    if !ticks.is_finite() || ticks < 0.0 || ticks >= 2_f64.powi(63) {
        return None;
    }
    Some(ticks.round() as i64)
}

impl From<Usage> for TokenUsage {
    fn from(u: Usage) -> Self {
        let cached_prompt_tokens = u
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cached_tokens);
        let cache_creation_prompt_tokens = u
            .prompt_tokens_details
            .as_ref()
            .map_or(0, |d| d.cache_write_tokens);
        Self {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u
                .completion_tokens_details
                .as_ref()
                .map_or(0, |d| d.reasoning_tokens),
            cached_prompt_tokens,
            cache_creation_prompt_tokens,
        }
    }
}

/// Response from a conversation turn.
/// Helpers (`assistant()`, `empty_reason()`, etc.) treat the trailing Assistant as "the response message" for backwards-compatible call sites.
#[derive(Debug, Clone)]
pub struct ConversationResponse {
    /// The flat ordered list of items produced by this turn.
    /// The trailing item is always an `Assistant` item (possibly with empty content if the model only emitted reasoning or tool calls).
    pub items: Vec<ConversationItem>,
    /// Why the model stopped generating
    pub stop_reason: Option<StopReason>,
    /// Token usage statistics
    pub usage: Option<TokenUsage>,
    /// Server cost in USD ticks (1 USD = 1e10).
    /// `Some(0)` is an authoritative reported free call; `None` means
    /// missing or invalid cost.
    pub cost_usd_ticks: Option<i64>,
    /// Number of `AgentMessageChunk` (text-only) streaming events emitted during this response.
    /// Reasoning/thought chunks are **not** counted.
    /// The caller should then emit a fallback `AgentMessageChunk` so downstream consumers see the turn as complete.
    pub message_chunks_emitted: u64,
    /// Server-reported doom-loop triggers for this response (Responses API only, opt-in via the `x-grok-doom-loop-check` header).
    /// Empty when the check is disabled or nothing was reported; deduplicated by raw label.
    /// See [`crate::doom_loop`].
    pub doom_loop_signals: Vec<crate::doom_loop::DoomLoopSignal>,
    /// Provider-supplied human-readable stop detail, when reported (e.g. a content-filter refusal explanation).
    /// Backend-neutral: normalized from the wire (Messages `message_delta.stop_details.explanation`).
    /// `None` otherwise and on backends that don't report one.
    pub stop_message: Option<String>,
    /// Provider message id (`message.id` on Messages, `id` on Chat Completions);
    /// `None` on backends that do not carry one (such as Responses).
    pub message_id: Option<String>,
    /// Wire stop reason before it collapses into [`StopReason`]: verbatim on the Messages backend.
    /// On the Responses backend only length cuts on tool-less turns are carried.
    /// `None` when unreported.
    pub raw_stop_reason: Option<String>,
    /// The provider's matched stop sequence (Messages API `message_delta.stop_sequence`).
    /// Present only when the model stopped on a configured stop sequence.
    /// `None` otherwise and on backends that do not report one (OAI Chat Completions / Responses).
    pub stop_sequence: Option<String>,
}

/// Normalize a legacy wire cost-ticks value at capture.
/// The REST layer backfills `0` for unreported cost, and negative ticks are never valid, so both become `None` ("unreported", never "free").
/// Authoritative USD costs use [`usd_cost_to_ticks`] so an explicit zero stays
/// distinguishable from this legacy backfill.
pub fn reported_cost_ticks(raw: Option<i64>) -> Option<i64> {
    raw.filter(|&t| t > 0)
}

impl ConversationResponse {
    /// The trailing `Assistant` item, if any.
    /// The producer (`response_to_conversation_items` and the streaming consumers) always appends exactly one Assistant item.
    /// This returns `None` defensively for ad-hoc constructions in tests.
    pub fn assistant(&self) -> Option<&AssistantItem> {
        self.items.iter().rev().find_map(|item| match item {
            ConversationItem::Assistant(a) => Some(a),
            _ => None,
        })
    }

    /// Mutable view of the trailing `Assistant` item.
    pub fn assistant_mut(&mut self) -> Option<&mut AssistantItem> {
        self.items.iter_mut().rev().find_map(|item| match item {
            ConversationItem::Assistant(a) => Some(a),
            _ => None,
        })
    }

    /// Trailing assistant text content, or empty string when the response has no assistant item (or the assistant carries no text).
    /// Used by classifier / dream / summarization call sites that only care about the visible model output.
    pub fn assistant_text(&self) -> String {
        self.assistant()
            .map(|a| a.content.as_ref().to_owned())
            .unwrap_or_default()
    }

    /// Reasoning siblings that precede the trailing `Assistant`, in order.
    /// Used by streaming consumers and the empty-response retry logic that previously inspected `AssistantItem.reasoning`.
    pub fn reasoning_items(&self) -> impl Iterator<Item = &rs::ReasoningItem> {
        self.items.iter().filter_map(|item| match item {
            ConversationItem::Reasoning(r) => Some(r),
            _ => None,
        })
    }

    /// Backend-executed tool calls (web search, X search, code interpreter) produced by this turn, in emission order.
    /// These are sibling items in `items` and must also be persisted to the conversation alongside the trailing `Assistant`.
    pub fn backend_tool_items(&self) -> impl Iterator<Item = &ConversationItem> {
        self.items
            .iter()
            .filter(|item| matches!(item, ConversationItem::BackendToolCall(_)))
    }

    /// Classify why the response is empty, if it is.
    ///
    /// Returns `Some(reason)` when the response has no visible content and no tool calls (the conditions that trigger resampling).
    pub fn empty_reason(&self) -> Option<crate::error::EmptyReason> {
        use crate::error::EmptyReason;
        let Some(a) = self.assistant() else {
            return Some(EmptyReason::NoVisibleContent);
        };
        if !a.content.is_empty() || !a.tool_calls.is_empty() {
            return None;
        }
        let has_reasoning = self
            .reasoning_items()
            .any(|r| !r.summary.is_empty() || r.content.is_some() || r.encrypted_content.is_some());
        if has_reasoning {
            Some(EmptyReason::ReasoningOnly)
        } else {
            Some(EmptyReason::NoVisibleContent)
        }
    }

    /// Check if the response is effectively empty (no content, no tool calls).
    ///
    /// Reasoning-only responses are considered empty so the retry logic resamples.
    pub fn is_empty(&self) -> bool {
        self.empty_reason().is_some()
    }

    /// Get tool calls from the assistant message, if any
    pub fn tool_calls(&self) -> &[ToolCall] {
        self.assistant()
            .map(|a| a.tool_calls.as_slice())
            .unwrap_or(&[])
    }

    /// Returns the assistant text when `AgentMessageChunk` events were lost during streaming (e.g. after an empty-response retry).
    /// The caller then emits it as a fallback.
    /// Returns `None` when streaming already delivered the text or when the response has no text content.
    pub fn fallback_text(&self) -> Option<String> {
        if self.message_chunks_emitted > 0 {
            return None;
        }
        let text = self
            .assistant()
            .map(|a| a.content.as_ref().to_owned())
            .unwrap_or_default();
        if text.is_empty() { None } else { Some(text) }
    }
}

// ============================================================================
// ConversationItem Constructors
// ============================================================================

impl ConversationItem {
    /// Create the request's system prompt, tagged [`SyntheticReason::Primary`].
    pub fn system(content: impl Into<String>) -> Self {
        Self::System(SystemItem {
            content: Arc::<str>::from(content.into()),
            synthetic_reason: SyntheticReason::Primary,
        })
    }

    /// Create a user message with text content, tagged [`SyntheticReason::Human`] for real user input.
    /// For synthetic injections, use a dedicated constructor such as [`ConversationItem::user_meta`] or [`ConversationItem::system_reminder`].
    pub fn user(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::Human,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a user message with multiple content parts, tagged [`SyntheticReason::Human`] for real user input.
    pub fn user_with_parts(parts: Vec<ContentPart>) -> Self {
        Self::User(UserItem {
            content: parts,
            synthetic_reason: SyntheticReason::Human,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a synthetic user message for metadata injection.
    /// Used by the compaction pipeline to inject file contents as plain-text user messages.
    /// Tagged with [`SyntheticReason::CompactionMeta`] so downstream code (pruning, compaction helpers) skips it.
    pub fn user_meta(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::CompactionMeta,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a synthetic user message for a runtime system reminder.
    /// Used for injected `<system-reminder>` content such as skill discovery updates and plan-mode reminders.
    /// Tagged with [`SyntheticReason::SystemReminder`] so downstream code can skip it when counting real user prompts.
    pub fn system_reminder(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::SystemReminder,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// The synthetic continue reminder, tagged [`SyntheticReason::LengthContinue`].
    pub fn length_continue_reminder(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::LengthContinue,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Return the working-directory generation carried by this switch reminder.
    pub fn working_directory_switch_generation(&self) -> Option<u64> {
        match self {
            Self::User(user)
                if user.synthetic_reason == SyntheticReason::WorkingDirectorySwitch =>
            {
                user.cwd_generation
            }
            _ => None,
        }
    }

    /// User message containing project instructions (AGENTS.md / CLAUDE.md).
    /// Tagged [`SyntheticReason::ProjectInstructions`] for spawn-time idempotence.
    /// Once in the conversation, it MUST NOT be replaced or re-inserted; see the variant docstring.
    pub fn project_instructions(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::ProjectInstructions,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Working-directory switch reminder with a structural generation marker.
    pub fn working_directory_switch(content: impl Into<String>, cwd_generation: u64) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::WorkingDirectorySwitch,
            cwd_generation: Some(cwd_generation),
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a model-authored message received from another agent.
    pub fn agent_message(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::AgentMessage,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a synthetic user message for an auto-continue prompt.
    /// Used after compaction to tell the agent to keep working.
    /// Tagged with [`SyntheticReason::AutoContinue`] so it is not counted as a real user prompt by truncation / rewind logic.
    pub fn auto_continue(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::AutoContinue,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a synthetic user message for an auto-recovery retry prompt.
    /// Used by the auto-recovery loop after a transient tool failure.
    /// Tagged with [`SyntheticReason::AutoRecovery`] so it is not counted as a real user prompt by truncation / rewind logic.
    pub fn auto_recovery(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::AutoRecovery,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create a synthetic user message for a mid-turn interjection.
    /// Compaction, replay, and analytics can then distinguish it from real prompts and other synthetic injections.
    pub fn interjection(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::Interjection,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Auto-wake synthetic prompt for a completed background bash task.
    pub fn task_completed(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::TaskCompleted,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Auto-wake synthetic prompt for a completed background subagent.
    pub fn subagent_completed(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::SubagentCompleted,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Idle-gated notification drain (batched completions / monitor events).
    pub fn notification_drain(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::NotificationDrain,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Goal orchestrator summary turn.
    pub fn goal_summary(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::GoalSummary,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Goal-achievement classifier nudge injected after the classifier rejects an `update_goal(completed: true)` attempt.
    /// Tagged distinctly from `goal_summary` so trace tooling can tell the two synthetic user turns apart.
    /// The wire role/tag is the same `<system-reminder>` shape for both.
    pub fn goal_classifier_nudge(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::GoalClassifierNudge,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Scheduled task (`/loop`) prompt fired by the scheduler.
    pub fn scheduler_fired(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::SchedulerFired,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// See [`SyntheticReason::StopHookFeedback`].
    pub fn stop_hook_feedback(content: impl Into<String>) -> Self {
        Self::User(UserItem {
            content: vec![ContentPart::Text {
                text: Arc::<str>::from(content.into()),
            }],
            synthetic_reason: SyntheticReason::StopHookFeedback,
            cwd_generation: None,
            prior_turn_interrupt: None,
            prompt_index: None,
        })
    }

    /// Create an assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant(AssistantItem {
            content: Arc::<str>::from(content.into()),
            tool_calls: Vec::new(),
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    /// Create an assistant message with a model ID.
    /// Reasoning, when present, lives as a separate sibling `ConversationItem::Reasoning(_)` placed before this item.
    /// Callers should push that item to the conversation list separately.
    pub fn assistant_with_model(content: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self::Assistant(AssistantItem {
            content: Arc::<str>::from(content.into()),
            tool_calls: Vec::new(),
            model_id: Some(model_id.into()),
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    /// Create an assistant message that makes tool calls
    pub fn assistant_tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self::Assistant(AssistantItem {
            content: Arc::<str>::from(""),
            tool_calls,
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })
    }

    /// Create a tool result message
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::ToolResult(ToolResultItem {
            tool_call_id: tool_call_id.into(),
            content: Arc::<str>::from(content.into()),
            images: Vec::new(),
        })
    }

    /// Create a tool result message with inline images.
    ///
    /// The images are embedded directly in the tool result message sent to the API, rather than being deferred to a follow-up user message.
    pub fn tool_result_with_images(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        images: Vec<ContentPart>,
    ) -> Self {
        Self::ToolResult(ToolResultItem {
            tool_call_id: tool_call_id.into(),
            content: Arc::<str>::from(content.into()),
            images,
        })
    }

    /// Get the role of this item
    pub fn role(&self) -> Role {
        match self {
            Self::System(_) => Role::System,
            Self::User(_) => Role::User,
            Self::Assistant(_) => Role::Assistant,
            Self::ToolResult(_) => Role::Tool,
            Self::BackendToolCall(_) => Role::Assistant,
            // Reasoning is part of the assistant's turn
            Self::Reasoning(_) => Role::Assistant,
        }
    }

    /// Origin of this item. `None` for Assistant, ToolResult, Reasoning, and BackendToolCall items, which have no origin metadata.
    pub fn synthetic_reason(&self) -> Option<&SyntheticReason> {
        match self {
            Self::User(u) => Some(&u.synthetic_reason),
            Self::System(s) => Some(&s.synthetic_reason),
            Self::Assistant(_)
            | Self::ToolResult(_)
            | Self::Reasoning(_)
            | Self::BackendToolCall(_) => None,
        }
    }

    /// Whether this is a `User` item typed by the user rather than synthesized by the runtime.
    pub fn is_human_user_turn(&self) -> bool {
        matches!(self, Self::User(u) if u.synthetic_reason.is_human())
    }

    /// Whether this is the system prompt that opens the conversation rather than a runtime-injected `System` item.
    pub fn is_primary_system_turn(&self) -> bool {
        matches!(self, Self::System(s) if s.synthetic_reason.is_primary())
    }

    /// Add an image to a user message. No-op for other message types.
    pub fn add_image(&mut self, url: impl Into<String>) {
        if let Self::User(user) = self {
            user.content.push(ContentPart::Image {
                url: Arc::<str>::from(url.into()),
            });
        }
    }

    /// Extract text content from this item
    pub fn text_content(&self) -> String {
        match self {
            Self::System(s) => s.content.as_ref().to_owned(),
            Self::User(u) => u
                .content
                .iter()
                .filter_map(|p| match p {
                    ContentPart::Text { text } => Some(text.as_ref()),
                    ContentPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Assistant(a) => a.content.as_ref().to_owned(),
            Self::ToolResult(t) => t.content.as_ref().to_owned(),
            Self::BackendToolCall(b) => b.text_summary(),
            Self::Reasoning(r) => reasoning_item_text(r),
        }
    }
}

// Shared-compaction L1 bridge: `CompactionItem` / `CompactionItemFactory`
// Lets the shared engine in `crates/common/distill-compaction` operate over distill's `ConversationItem` without depending on this crate
// That preserves the `SyntheticReason` tags the replay / spawn-time idempotence guards rely on
impl distill_compaction::CompactionItem for ConversationItem {
    fn role(&self) -> distill_compaction::CompactionRole {
        use distill_compaction::CompactionRole;
        // distill has no distinct `Developer` role; everything maps onto the four `Role` variants `ConversationItem::role()` already returns
        match self.role() {
            Role::System => CompactionRole::System,
            Role::User => CompactionRole::User,
            Role::Assistant => CompactionRole::Assistant,
            Role::Tool => CompactionRole::Tool,
        }
    }

    fn text(&self) -> Option<String> {
        // Tool results and tool-only assistant turns can be textless; the shared algorithms expect `None` rather than an empty string there
        let text = self.text_content();
        if text.is_empty() { None } else { Some(text) }
    }

    fn has_tool_requests(&self) -> bool {
        matches!(self, Self::Assistant(a) if !a.tool_calls.is_empty())
    }

    fn is_compaction_summary(&self) -> bool {
        // distill has no structural marker that uniquely identifies a prior compaction summary
        // Returning `false` is safe for the full-replace path, which does not consult this (it summarizes the whole conversation)
        // Revisit (add a dedicated marker) before routing distill history through the shared `history`/`inter` filter
        false
    }

    fn attachment_refs(&self) -> Vec<distill_compaction::CompactionFileRef> {
        // distill `UserItem`s carry only `Text`/`Image { url }` content parts
        // There is no id-and-name attachment-ref concept like the chat harness's `GrokTurn` has
        // The full-replace path does not read this; revisit if image attachments need to survive into the `<grok_user_queries>` preamble
        Vec::new()
    }
}

impl distill_compaction::CompactionItemFactory for ConversationItem {
    fn new_user(text: String) -> Self {
        Self::user(text)
    }

    fn new_user_meta(text: String) -> Self {
        Self::user_meta(text)
    }

    fn new_project_instructions(text: String) -> Self {
        Self::project_instructions(text)
    }

    fn new_system_reminder(text: String) -> Self {
        Self::system_reminder(text)
    }
}

/// Extract human-readable text from a Responses-API reasoning item by joining its `summary` parts (in order) followed by its `content` blocks.
/// Encrypted-only reasoning items return an empty string since their text is not user-visible.
pub fn reasoning_item_text(r: &rs::ReasoningItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    for sp in &r.summary {
        match sp {
            rs::SummaryPart::SummaryText(t) => parts.push(t.text.clone()),
        }
    }
    if let Some(ref content) = r.content {
        for c in content {
            parts.push(c.text.clone());
        }
    }
    parts.join("\n")
}

/// Construct an `rs::ReasoningItem` carrying a single `SummaryText` part.
/// `id` is left empty because none of the synthesizing paths carry a stable upstream id.
/// `encrypted_content` is `None` because its only source is the Responses API itself.
pub fn synthesized_reasoning_item(text: impl Into<String>) -> rs::ReasoningItem {
    rs::ReasoningItem {
        id: String::new(),
        summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
            text: text.into(),
        })],
        content: None,
        encrypted_content: None,
        status: None,
    }
}

/// Splice a streaming-fallback reasoning text into a `Vec<ConversationItem>` produced by `response_to_conversation_items`.
/// If any existing `Reasoning` sibling already carries text, leave `items` untouched (the deltas are redundant); Otherwise, if there is a `Reasoning` sibling with no text, append a `SummaryText` part to it (avoids introducing a phantom sibling); Otherwise, insert a new `Reasoning(synthesized_reasoning_item(text))` immediately before the trailing `Assistant`.
pub fn inject_streaming_reasoning_fallback(items: &mut Vec<ConversationItem>, text: String) {
    if text.is_empty() {
        return;
    }
    let any_with_text = items.iter().any(|i| match i {
        ConversationItem::Reasoning(r) => r.summary.iter().any(|sp| match sp {
            rs::SummaryPart::SummaryText(t) => !t.text.is_empty(),
        }),
        _ => false,
    });
    if any_with_text {
        return;
    }
    if let Some(ConversationItem::Reasoning(r)) = items
        .iter_mut()
        .find(|i| matches!(i, ConversationItem::Reasoning(_)))
    {
        r.summary
            .push(rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                text,
            }));
        return;
    }
    let pos = items
        .iter()
        .rposition(|i| matches!(i, ConversationItem::Assistant(_)))
        .unwrap_or(items.len());
    items.insert(
        pos,
        ConversationItem::Reasoning(synthesized_reasoning_item(text)),
    );
}

/// Reconstruct sibling `Reasoning` and `BackendToolCall` items from a legacy chat-history row's raw JSON.
/// We also write any newly-emitted ids back into the set so subsequent assistants don't re-emit.
/// Singular `reasoning` path: builds a synthetic `rs::ReasoningItem` from the legacy `ReasoningContent { text, encrypted, id }`. `id` is preserved when present; some thinking blocks never carried one
pub fn upgrade_legacy_reasoning(
    raw: &serde_json::Value,
    sibling_btc_ids_seen: &mut std::collections::HashSet<String>,
) -> Vec<ConversationItem> {
    let mut siblings: Vec<ConversationItem> = Vec::new();
    let Some(obj) = raw.as_object() else {
        return siblings;
    };

    // v1 assistant: type == "assistant"
    let is_v1_assistant = obj.get("type").and_then(|t| t.as_str()) == Some("assistant");
    // v0 assistant: role == "assistant" with top-level reasoning_content
    let is_v0_assistant = obj.get("role").and_then(|r| r.as_str()) == Some("assistant");

    if !is_v1_assistant && !is_v0_assistant {
        return siblings;
    }

    // Path A: v1 with `raw_output` (backend-search era)
    // Expand each entry as `rs::OutputItem` and lift Reasoning / backend-tool items to siblings
    // `Message` / `FunctionCall` are already on the assistant row (content / tool_calls), so skip them
    if let Some(raw_output) = obj.get("raw_output").and_then(|v| v.as_array()) {
        for entry in raw_output {
            let Ok(item) = serde_json::from_value::<rs::OutputItem>(entry.clone()) else {
                continue;
            };
            match item {
                rs::OutputItem::Reasoning(r) => {
                    siblings.push(ConversationItem::Reasoning(r));
                }
                rs::OutputItem::WebSearchCall(ws) if sibling_btc_ids_seen.insert(ws.id.clone()) => {
                    siblings.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                        kind: BackendToolKind::WebSearch(ws),
                    }));
                }
                rs::OutputItem::CustomToolCall(ct)
                    if sibling_btc_ids_seen.insert(ct.id.clone()) =>
                {
                    siblings.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                        kind: BackendToolKind::XSearch(ct),
                    }));
                }
                rs::OutputItem::CodeInterpreterCall(ci)
                    if sibling_btc_ids_seen.insert(ci.id.clone()) =>
                {
                    siblings.push(ConversationItem::BackendToolCall(BackendToolCallItem {
                        kind: BackendToolKind::CodeInterpreter(ci),
                    }));
                }
                _ => {}
            }
        }
        // raw_output is the highest-fidelity source; if it was present we ignore singular `reasoning`
        // The two are mutually exclusive in practice and raw_output is the superset
        return siblings;
    }

    // Path B: v1 with singular `reasoning: ReasoningContent` (earlier clients or chat-completions written as v1)
    if is_v1_assistant && let Some(reasoning) = obj.get("reasoning").and_then(|r| r.as_object()) {
        let text = reasoning.get("text").and_then(|t| t.as_str());
        let encrypted = reasoning.get("encrypted").and_then(|t| t.as_str());
        let id = reasoning
            .get("id")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if let Some(item) = build_synthetic_reasoning(id, text, encrypted) {
            siblings.push(ConversationItem::Reasoning(item));
        }
        return siblings;
    }

    // Path C: v0 `ChatRequestMessage` with top-level `reasoning_content`
    if is_v0_assistant
        && let Some(rc) = obj.get("reasoning_content").and_then(|t| t.as_str())
        && !rc.is_empty()
        && let Some(item) = build_synthetic_reasoning(String::new(), Some(rc), None)
    {
        siblings.push(ConversationItem::Reasoning(item));
    }

    siblings
}

/// Build a `rs::ReasoningItem` from legacy text / encrypted strings.
/// Returns `None` when there's nothing to preserve.
fn build_synthetic_reasoning(
    id: String,
    text: Option<&str>,
    encrypted: Option<&str>,
) -> Option<rs::ReasoningItem> {
    let text = text.filter(|t| !t.is_empty());
    let encrypted = encrypted.filter(|e| !e.is_empty());
    if text.is_none() && encrypted.is_none() {
        return None;
    }
    let summary = match text {
        Some(t) => vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
            text: t.to_string(),
        })],
        None => Vec::new(),
    };
    Some(rs::ReasoningItem {
        id,
        summary,
        content: None,
        encrypted_content: encrypted.map(String::from),
        status: None,
    })
}

impl UserItem {
    /// Add an image to this user message
    pub fn add_image(&mut self, url: impl Into<String>) {
        self.content.push(ContentPart::Image {
            url: Arc::<str>::from(url.into()),
        });
    }
}

impl AssistantItem {
    /// Add a tool call to this assistant message
    pub fn add_tool_call(&mut self, call: ToolCall) {
        self.tool_calls.push(call);
    }

    /// Set the model ID for this assistant message
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        self.model_id = Some(model_id.into());
        self
    }
}

impl ConversationItem {
    /// Set the model ID if this is an assistant message. No-op for other types.
    pub fn with_model_id(mut self, model_id: impl Into<String>) -> Self {
        if let Self::Assistant(ref mut a) = self {
            a.model_id = Some(model_id.into());
        }
        self
    }

    /// Mark this message as the genuine user turn that directly followed a user-interrupted turn (see [`PriorTurnInterrupt`]).
    /// No-op for any non-`User` variant, so callers can apply it unconditionally.
    pub fn set_prior_turn_interrupt(&mut self, interrupt: PriorTurnInterrupt) {
        if let Self::User(u) = self {
            u.prior_turn_interrupt = Some(interrupt);
        }
    }

    /// Record the prompt-turn index this user item starts (see [`UserItem::prompt_index`]).
    /// No-op for any non-`User` variant, so callers can apply it unconditionally.
    pub fn set_prompt_index(&mut self, prompt_index: usize) {
        if let Self::User(u) = self {
            u.prompt_index = Some(prompt_index);
        }
    }
}

// ============================================================================
// ConversationRequest Builder
// ============================================================================

impl ConversationRequest {
    /// Create a new empty conversation request
    pub fn new() -> Self {
        Self::default()
    }

    /// Create from a list of conversation items
    pub fn from_items(items: Vec<ConversationItem>) -> Self {
        Self {
            items,
            ..Default::default()
        }
    }

    /// Add an item to the conversation
    pub fn push(&mut self, item: ConversationItem) {
        self.items.push(item);
    }

    /// Set the model
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set tools
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Set tool choice
    pub fn with_tool_choice(mut self, choice: ConversationToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Set temperature
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Set max output tokens
    pub fn with_max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_tokens);
        self
    }

    /// Set conversation ID header
    pub fn with_conv_id(mut self, conv_id: impl Into<String>) -> Self {
        self.x_grok_conv_id = Some(conv_id.into());
        self
    }

    /// Set request ID header
    pub fn with_req_id(mut self, req_id: impl Into<String>) -> Self {
        self.x_grok_req_id = Some(req_id.into());
        self
    }

    /// Set trace context for request logging.
    /// Accepts any type that implements `TraceContext` (i.e., `Clone + Send + Sync + Debug + 'static`).
    pub fn with_trace(mut self, trace: impl TraceContext + 'static) -> Self {
        self.trace = Some(Box::new(trace));
        self
    }

    pub fn with_json_schema(mut self, schema: serde_json::Value) -> Self {
        self.json_schema = Some(schema);
        self
    }
}

/// Calculate how many conversation items to keep so that everything from prompt-turn `target_prompt_index` onward is dropped.
/// **Before the first** [`UserItem::prompt_index`]: legacy rules apply. The first marker-less non-synthetic is the `<user_info>` preamble. Later non-synthetics and [`SyntheticReason::starts_prompt_turn`] synthetics are turns; **From the first marker onward**: only marked rows open turns; unmarked mid-turn phantoms (bash / permission followup) never open a cut.
pub fn conversation_truncate_for_prompt(
    conversation: &[ConversationItem],
    target_prompt_index: usize,
) -> usize {
    let first_marker = conversation.iter().find_map(|item| match item {
        ConversationItem::User(u) => u.prompt_index,
        _ => None,
    });

    let Some(first_marker_idx) = first_marker else {
        return conversation_truncate_legacy(conversation, target_prompt_index);
    };

    let unmarked_prefix_turns = count_legacy_turns_until_marker(conversation);
    if unmarked_prefix_turns != first_marker_idx {
        return conversation_truncate_markers_only(conversation, target_prompt_index);
    }

    conversation_truncate_progressive(conversation, target_prompt_index)
}

fn conversation_truncate_markers_only(
    conversation: &[ConversationItem],
    target_prompt_index: usize,
) -> usize {
    for (i, item) in conversation.iter().enumerate() {
        let ConversationItem::User(user) = item else {
            continue;
        };
        if let Some(idx) = user.prompt_index
            && idx >= target_prompt_index
        {
            return i;
        }
    }
    conversation.len()
}

fn conversation_truncate_legacy(
    conversation: &[ConversationItem],
    target_prompt_index: usize,
) -> usize {
    let mut next_unmarked_index = 0usize;
    let mut seen_unmarked_preamble = false;

    for (i, item) in conversation.iter().enumerate() {
        let ConversationItem::User(user) = item else {
            continue;
        };

        let effective_index = match &user.synthetic_reason {
            SyntheticReason::Human if !seen_unmarked_preamble => {
                seen_unmarked_preamble = true;
                None
            }
            SyntheticReason::Human => Some(next_unmarked_index),
            reason if reason.starts_prompt_turn() => Some(next_unmarked_index),
            _ => None,
        };

        if let Some(idx) = effective_index {
            if idx >= target_prompt_index {
                return i;
            }
            next_unmarked_index = idx + 1;
        }
    }

    conversation.len()
}

/// Count legacy turns in the unmarked prefix (stops at the first marker).
fn count_legacy_turns_until_marker(conversation: &[ConversationItem]) -> usize {
    let mut turns = 0usize;
    let mut seen_unmarked_preamble = false;

    for item in conversation {
        let ConversationItem::User(user) = item else {
            continue;
        };
        if user.prompt_index.is_some() {
            break;
        }
        match &user.synthetic_reason {
            SyntheticReason::Human if !seen_unmarked_preamble => {
                seen_unmarked_preamble = true;
            }
            SyntheticReason::Human => turns += 1,
            reason if reason.starts_prompt_turn() => turns += 1,
            _ => {}
        }
    }
    turns
}

fn conversation_truncate_progressive(
    conversation: &[ConversationItem],
    target_prompt_index: usize,
) -> usize {
    let mut next_unmarked_index = 0usize;
    let mut seen_unmarked_preamble = false;
    let mut seen_marker = false;

    for (i, item) in conversation.iter().enumerate() {
        let ConversationItem::User(user) = item else {
            continue;
        };

        let effective_index = if let Some(idx) = user.prompt_index {
            seen_marker = true;
            Some(idx)
        } else if seen_marker {
            None
        } else {
            match &user.synthetic_reason {
                SyntheticReason::Human if !seen_unmarked_preamble => {
                    seen_unmarked_preamble = true;
                    None
                }
                SyntheticReason::Human => Some(next_unmarked_index),
                reason if reason.starts_prompt_turn() => Some(next_unmarked_index),
                _ => None,
            }
        };

        if let Some(idx) = effective_index {
            if idx >= target_prompt_index {
                return i;
            }
            if user.prompt_index.is_none() {
                next_unmarked_index = idx + 1;
            }
        }
    }

    conversation.len()
}

/// Transform CWD paths in conversation items (used for forked sessions).
/// This replaces source_cwd with target_cwd in text content.
pub fn transform_conversation_cwd(
    items: &mut [ConversationItem],
    source_cwd: &str,
    target_cwd: &str,
) {
    for item in items.iter_mut() {
        match item {
            ConversationItem::System(s) => {
                if s.content.contains(source_cwd) {
                    s.content = Arc::<str>::from(s.content.replace(source_cwd, target_cwd));
                }
            }
            ConversationItem::User(u) => {
                for part in u.content.iter_mut() {
                    if let ContentPart::Text { text } = part
                        && text.contains(source_cwd)
                    {
                        *text = Arc::<str>::from(text.replace(source_cwd, target_cwd));
                    }
                }
            }
            ConversationItem::Assistant(a) => {
                if a.content.contains(source_cwd) {
                    a.content = Arc::<str>::from(a.content.replace(source_cwd, target_cwd));
                }
                // Tool call arguments contain file paths that must also be rewritten.
                // The arguments field is a JSON-encoded string source_cwd appears as a literal substring (serde_json does not escape `/`), so str::replace is safe
                // Reverse (worktree to root): so the synced-back session doesn't reference a deleted worktree directory on the next turn
                for tc in &mut a.tool_calls {
                    if tc.arguments.contains(source_cwd) {
                        tc.arguments =
                            Arc::<str>::from(tc.arguments.replace(source_cwd, target_cwd));
                    }
                }
            }
            ConversationItem::ToolResult(t) => {
                if t.content.contains(source_cwd) {
                    t.content = Arc::<str>::from(t.content.replace(source_cwd, target_cwd));
                }
            }
            // Backend tool calls don't contain workspace paths, so there is nothing to rewrite
            ConversationItem::BackendToolCall(_) => {}
            // Reasoning items rarely reference CWD paths, but they can; patch both summary parts and content blocks defensively
            ConversationItem::Reasoning(r) => {
                for sp in r.summary.iter_mut() {
                    match sp {
                        rs::SummaryPart::SummaryText(t) => {
                            if t.text.contains(source_cwd) {
                                t.text = t.text.replace(source_cwd, target_cwd);
                            }
                        }
                    }
                }
                if let Some(ref mut content) = r.content {
                    for c in content.iter_mut() {
                        if c.text.contains(source_cwd) {
                            c.text = c.text.replace(source_cwd, target_cwd);
                        }
                    }
                }
            }
        }
    }
}

// ============================================================================
// Conversation Repair
// ============================================================================

/// Each variant maps to a distinct synthetic `ToolResult` body produced by [`repair_dangling_tool_calls`].
/// An earlier revision had a `PostProcessingFailed` variant; it is intentionally absent now.
#[derive(Debug, Clone, Copy)]
pub enum DanglingToolCallReason {
    /// User pressed Ctrl+C / aborted, or the cause cannot be determined.
    ///
    /// Default fallback when no more specific reason is plumbed through.
    UserCancelled,
    /// Harness halted the turn (internal error, policy guard, etc.).
    /// `class` is a stable taxonomy tag used by metrics and the synthetic message.
    /// It is `&'static str` because every call site is known at compile time.
    HarnessHalted { class: &'static str },
}

/// Insert synthetic `ToolResult` items for any tool calls that lack a result.
/// The API rejects this with "No tool output found for function call …".
pub fn repair_dangling_tool_calls(
    conversation: &mut Vec<ConversationItem>,
    reason: DanglingToolCallReason,
) -> usize {
    // Phase 1: forward scan to find every assistant with unanswered tool calls.
    // We record (insert_position, synthetic_items) for each repair site.
    let mut repairs: Vec<(usize, Vec<ConversationItem>)> = Vec::new();
    let mut i = 0;

    while i < conversation.len() {
        if let Some(ConversationItem::Assistant(a)) = conversation.get(i)
            && !a.tool_calls.is_empty()
        {
            // Snapshot the call metadata we need (avoids borrowing `conversation`).
            let tool_calls: Vec<(Arc<str>, String)> = a
                .tool_calls
                .iter()
                .map(|tc| (tc.id.clone(), tc.name.clone()))
                .collect();

            // Collect answered IDs from the immediately following ToolResult items.
            let mut answered = std::collections::HashSet::new();
            let mut j = i + 1;
            while j < conversation.len() {
                if let Some(ConversationItem::ToolResult(tr)) = conversation.get(j) {
                    answered.insert(tr.tool_call_id.clone());
                    j += 1;
                } else {
                    break;
                }
            }

            // Build synthetic results for unanswered calls, preserving call order.
            let synthetic: Vec<ConversationItem> = tool_calls
                .iter()
                .filter(|(id, _)| !answered.contains(id.as_ref()))
                .map(|(id, name)| {
                    ConversationItem::tool_result(
                        id.as_ref(),
                        synthetic_dangling_result_text(name, reason),
                    )
                })
                .collect();

            if !synthetic.is_empty() {
                repairs.push((j, synthetic));
            }

            i = j;
            continue;
        }
        i += 1;
    }

    // Phase 2: apply repairs in reverse index order so earlier indices stay valid.
    let total: usize = repairs.iter().map(|(_, s)| s.len()).sum();
    for (insert_at, synthetic) in repairs.into_iter().rev() {
        conversation.splice(insert_at..insert_at, synthetic);
    }
    total
}

/// Read-only counterpart to [`repair_dangling_tool_calls`].
/// Lets callers decide whether the repair *would* fire (and therefore already signal a cancellation to the model) without mutating the conversation.
pub fn has_dangling_tool_calls(conversation: &[ConversationItem]) -> bool {
    let mut i = 0;
    while i < conversation.len() {
        if let Some(ConversationItem::Assistant(a)) = conversation.get(i)
            && !a.tool_calls.is_empty()
        {
            // Collect answered IDs from the immediately following ToolResults.
            let mut answered = std::collections::HashSet::new();
            let mut j = i + 1;
            while j < conversation.len() {
                if let Some(ConversationItem::ToolResult(tr)) = conversation.get(j) {
                    answered.insert(tr.tool_call_id.clone());
                    j += 1;
                } else {
                    break;
                }
            }
            if a.tool_calls
                .iter()
                .any(|tc| !answered.contains(tc.id.as_ref()))
            {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

fn synthetic_dangling_result_text(name: &str, reason: DanglingToolCallReason) -> String {
    // Exhaustive match (no `_ =>` guard) so adding a new variant is a compile-time error at every call site that renders these messages
    match reason {
        DanglingToolCallReason::UserCancelled => {
            format!("Tool execution was cancelled by the user (tool `{name}` was not executed).")
        }
        DanglingToolCallReason::HarnessHalted { class } => format!(
            "Tool execution was halted by the harness ({class}); the tool `{name}` was not executed.",
        ),
    }
}

/// Remove duplicate `ToolResult` entries for the same `tool_call_id`.
/// The LLM API rejects this with "each tool_use must have a single result".
/// If a `tool_call_id` appears more than once, only the **last** occurrence is kept (the real result), and earlier duplicates are removed.
pub fn dedup_duplicate_tool_results(conversation: &mut Vec<ConversationItem>) -> usize {
    let mut total_removed = 0;
    let mut i = 0;

    while i < conversation.len() {
        // Look for assistant messages with tool calls.
        if let Some(ConversationItem::Assistant(a)) = conversation.get(i)
            && !a.tool_calls.is_empty()
        {
            // Scan the run of ToolResult items immediately after.
            let start = i + 1;
            let mut end = start;
            while end < conversation.len() {
                if conversation
                    .get(end)
                    .is_some_and(|item| matches!(item, ConversationItem::ToolResult(_)))
                {
                    end += 1;
                } else {
                    break;
                }
            }

            // Within [start..end), find duplicates by tool_call_id.
            // Keep the *last* occurrence of each id (the real result).
            if end > start {
                let mut seen = std::collections::HashMap::<String, usize>::new();
                let mut to_remove = Vec::<usize>::new();

                for (idx, item) in conversation.iter().enumerate().take(end).skip(start) {
                    if let ConversationItem::ToolResult(tr) = item
                        && let Some(prev) = seen.insert(tr.tool_call_id.clone(), idx)
                    {
                        // We've seen this id before, so mark the *previous* for removal
                        to_remove.push(prev);
                    }
                }

                if !to_remove.is_empty() {
                    // Remove in reverse order so indices stay valid.
                    to_remove.sort_unstable();
                    to_remove.reverse();
                    for idx in &to_remove {
                        conversation.remove(*idx);
                    }
                    total_removed += to_remove.len();
                    // Don't advance i: the window shifted, so re-scan from the same spot
                    continue;
                }
            }

            i = end;
            continue;
        }
        i += 1;
    }

    total_removed
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod compaction_item_bridge_tests {
    use super::*;
    use distill_compaction::{CompactionItem, CompactionItemFactory, CompactionRole};

    #[test]
    fn role_maps_every_variant() {
        assert_eq!(
            CompactionItem::role(&ConversationItem::system("s")),
            CompactionRole::System
        );
        assert_eq!(
            CompactionItem::role(&ConversationItem::user("u")),
            CompactionRole::User
        );
        assert_eq!(
            CompactionItem::role(&ConversationItem::assistant("a")),
            CompactionRole::Assistant
        );
        assert_eq!(
            CompactionItem::role(&ConversationItem::tool_result("tc1", "r")),
            CompactionRole::Tool
        );
        // BackendToolCall / Reasoning are part of the assistant turn; they must map to Assistant, never Tool
        assert_eq!(
            CompactionItem::role(&ConversationItem::Reasoning(synthesized_reasoning_item(
                "t"
            ))),
            CompactionRole::Assistant
        );
    }

    #[test]
    fn text_is_none_when_empty_some_otherwise() {
        assert_eq!(
            CompactionItem::text(&ConversationItem::user("hello")),
            Some("hello".to_string())
        );
        // An assistant tool-only turn has empty text content, so it maps to `None`
        let tool_only = ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "tc1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]);
        assert_eq!(CompactionItem::text(&tool_only), None);
    }

    #[test]
    fn has_tool_requests_only_for_assistant_with_tool_calls() {
        let with_tools = ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "tc1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]);
        assert!(CompactionItem::has_tool_requests(&with_tools));
        assert!(!CompactionItem::has_tool_requests(
            &ConversationItem::assistant("no tools")
        ));
        assert!(!CompactionItem::has_tool_requests(&ConversationItem::user(
            "u"
        )));
        assert!(!CompactionItem::has_tool_requests(
            &ConversationItem::tool_result("tc1", "r")
        ));
    }

    #[test]
    fn is_tool_result_default_tracks_role() {
        assert!(CompactionItem::is_tool_result(
            &ConversationItem::tool_result("tc1", "r")
        ));
        assert!(!CompactionItem::is_tool_result(&ConversationItem::user(
            "u"
        )));
    }

    #[test]
    fn metadata_accessors_are_conservative() {
        // distill has no structural compaction-summary marker and no id-and-name attachment refs, so both return empty/false
        assert!(!CompactionItem::is_compaction_summary(
            &ConversationItem::user("u")
        ));
        assert!(!CompactionItem::is_compaction_summary(
            &ConversationItem::user_meta("summary")
        ));
        assert!(CompactionItem::attachment_refs(&ConversationItem::user("u")).is_empty());
    }

    /// Each factory constructor must keep the `SyntheticReason` tag that the replay / spawn-time idempotence guards rely on.
    /// A compacted history rebuilt through the shared assembler is then indistinguishable from the in-shell one.
    #[test]
    fn factory_constructors_preserve_synthetic_reason_tags() {
        let plain = <ConversationItem as CompactionItemFactory>::new_user("q".into());
        assert_matches_user_reason(&plain, SyntheticReason::Human);

        let meta = <ConversationItem as CompactionItemFactory>::new_user_meta("m".into());
        assert_matches_user_reason(&meta, SyntheticReason::CompactionMeta);

        let proj =
            <ConversationItem as CompactionItemFactory>::new_project_instructions("p".into());
        assert_matches_user_reason(&proj, SyntheticReason::ProjectInstructions);

        let reminder = <ConversationItem as CompactionItemFactory>::new_system_reminder("r".into());
        assert_matches_user_reason(&reminder, SyntheticReason::SystemReminder);
    }

    fn assert_matches_user_reason(item: &ConversationItem, expected: SyntheticReason) {
        let ConversationItem::User(parts) = item else {
            panic!("factory must produce a User item, got {item:?}");
        };
        assert_eq!(expected, parts.synthetic_reason);
    }
}

#[cfg(test)]
#[path = "conversation/test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "conversation/chat_completions_tests.rs"]
mod chat_completions_tests;

#[cfg(test)]
#[path = "conversation/responses_tests.rs"]
mod responses_tests;

#[cfg(test)]
#[path = "conversation/messages_tests.rs"]
mod messages_tests;

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::tool_overrides::*;
    use assert_matches::assert_matches;

    /// Keeps `forwards_prompt_cache_key()` honest against each mapping: a key that never reaches the wire looks like a 0% cache hit, not a bug.
    #[test]
    fn prompt_cache_key_reaches_the_wire_only_where_the_backend_claims() {
        let request = || ConversationRequest {
            items: vec![ConversationItem::user("hi")],
            model: Some("test-model".to_string()),
            prompt_cache_key: Some("cache-key-1".to_string()),
            ..Default::default()
        };

        for backend in [
            crate::ApiBackend::ChatCompletions,
            crate::ApiBackend::Responses,
            crate::ApiBackend::Messages,
        ] {
            let on_wire = match backend {
                crate::ApiBackend::Responses => {
                    rs::CreateResponse::from(&request())
                        .prompt_cache_key
                        .as_deref()
                        == Some("cache-key-1")
                }
                crate::ApiBackend::ChatCompletions => {
                    let mapped = ChatCompletionRequest::from(request());
                    serde_json::to_value(&mapped)
                        .expect("chat request serializes")
                        .get("prompt_cache_key")
                        .is_some()
                }
                crate::ApiBackend::Messages => {
                    let mapped = super::messages::build_messages_request(&request());
                    serde_json::to_value(&mapped)
                        .expect("messages request serializes")
                        .get("prompt_cache_key")
                        .is_some()
                }
            };
            assert_eq!(
                on_wire,
                backend.forwards_prompt_cache_key(),
                "{backend:?}: forwards_prompt_cache_key() disagrees with the mapping"
            );
        }
    }

    #[test]
    fn prior_turn_interrupt_serde_round_trip_and_unknown_fallback() {
        for (variant, wire) in [
            (PriorTurnInterrupt::MidTurnAbort, "\"mid_turn_abort\""),
            (
                PriorTurnInterrupt::PermissionRejected,
                "\"permission_rejected\"",
            ),
            (
                PriorTurnInterrupt::PermissionCancelled,
                "\"permission_cancelled\"",
            ),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
            let back: PriorTurnInterrupt = serde_json::from_str(wire).unwrap();
            assert_eq!(back, variant);
        }
        // Forward-compat: an unknown wire string decodes to `Unknown`.
        let unknown: PriorTurnInterrupt = serde_json::from_str("\"some_future_cause\"").unwrap();
        assert_eq!(unknown, PriorTurnInterrupt::Unknown);
    }

    #[test]
    fn user_item_prior_turn_interrupt_omitted_when_none_present_when_set() {
        // A real prompt with no marker omits the field entirely (byte-stable with sessions written before this field existed)
        let plain = ConversationItem::user("hello");
        let json = serde_json::to_string(&plain).unwrap();
        assert!(
            !json.contains("prior_turn_interrupt"),
            "None marker must be omitted, got {json}"
        );

        // The setter stamps it; it serializes and round-trips.
        let mut marked = ConversationItem::user("hello");
        marked.set_prior_turn_interrupt(PriorTurnInterrupt::MidTurnAbort);
        let json = serde_json::to_string(&marked).unwrap();
        assert!(
            json.contains("\"prior_turn_interrupt\":\"mid_turn_abort\""),
            "got {json}"
        );
        let back: ConversationItem = serde_json::from_str(&json).unwrap();
        assert_matches!(
            back,
            ConversationItem::User(UserItem {
                prior_turn_interrupt: Some(PriorTurnInterrupt::MidTurnAbort),
                ..
            })
        );
    }

    #[test]
    fn set_prior_turn_interrupt_is_noop_on_non_user_items() {
        let mut sys = ConversationItem::system("sp");
        sys.set_prior_turn_interrupt(PriorTurnInterrupt::MidTurnAbort);
        assert_matches!(sys, ConversationItem::System(_));
    }

    #[test]
    fn tool_overrides_update_apply_merges_tristate() {
        let x = XSearchOptions {
            date_bound: Some(SearchDateBound::new(None, Some("2024-03-15".into())).unwrap()),
        };
        let w = WebSearchOptions {
            allowed_domains: Some(vec!["x.com".into()]),
            excluded_domains: None,
        };

        // set: an object sets that tool's options.
        let base = ToolOverridesUpdate {
            x_search: Some(Some(x.clone())),
            web_search: None,
        }
        .apply(None);
        assert_eq!(
            base.as_ref().and_then(|o| o.x_search.clone()),
            Some(x.clone())
        );

        // leave: an absent field keeps the base's entry; a set field updates only itself.
        let merged = ToolOverridesUpdate {
            x_search: None,
            web_search: Some(Some(w.clone())),
        }
        .apply(base.clone());
        assert_eq!(merged.as_ref().and_then(|o| o.x_search.clone()), Some(x));
        assert_eq!(merged.and_then(|o| o.web_search), Some(w));

        // clear: `null` clears just that tool; clearing the last remaining tool empties the override to `None`
        let cleared = ToolOverridesUpdate {
            x_search: Some(None),
            web_search: None,
        }
        .apply(base);
        assert!(cleared.is_none());
    }

    #[test]
    fn empty_per_turn_override_never_clears_a_seeded_cutoff() {
        use serde_json::json;
        // A stray empty `{}` carries no instruction, so a definition-seeded cutoff must survive it
        // Only an explicit bound changes the window; `null` reverts to the seed
        let update = ToolOverridesUpdate::parse(&json!({"xSearch": {}}))
            .unwrap()
            .apply(None);
        let mut tools = vec![HostedTool::XSearch {
            options: Some(XSearchOptions {
                date_bound: Some(SearchDateBound::new(None, Some("2024-01-01".into())).unwrap()),
            }),
        }];
        let applied = apply_tool_overrides(&mut tools, update.as_ref());
        assert_eq!(
            applied
                .x_search
                .and_then(|x| x.date_bound)
                .and_then(|b| b.to_date().map(str::to_owned)),
            Some("2024-01-01".to_string()),
            "an empty override must not widen a seeded cutoff"
        );

        let mut tools = vec![HostedTool::XSearch {
            options: Some(XSearchOptions {
                date_bound: Some(SearchDateBound::new(None, Some("2024-01-01".into())).unwrap()),
            }),
        }];
        let direct = ToolOverrides::parse(&json!({"xSearch": {}})).unwrap();
        let applied = apply_tool_overrides(&mut tools, Some(&direct));
        assert_eq!(
            applied
                .x_search
                .and_then(|x| x.date_bound)
                .and_then(|b| b.to_date().map(str::to_owned)),
            Some("2024-01-01".to_string()),
            "an empty override leaves the seeded bound, which stays attested"
        );
    }

    #[test]
    fn search_date_bound_validation() {
        // Non-canonical dates: unpadded is NotZeroPadded
        // A five-digit year and year 0 (below the minimum year 1) are InvalidDate; a valid padded window is accepted
        assert!(matches!(
            SearchDateBound::new(Some("2024-3-5".into()), None),
            Err(SearchDateBoundError::NotZeroPadded { .. })
        ));
        assert!(matches!(
            SearchDateBound::new(Some("10000-01-01".into()), None),
            Err(SearchDateBoundError::InvalidDate { .. })
        ));
        assert!(matches!(
            SearchDateBound::new(Some("0000-01-01".into()), None),
            Err(SearchDateBoundError::InvalidDate { .. })
        ));
        assert!(SearchDateBound::new(Some("0001-01-01".into()), Some("0099-12-31".into())).is_ok());

        // Inverted window is rejected with the typed error; equal and ordered windows are accepted.
        assert!(matches!(
            SearchDateBound::new(Some("2024-03-15".into()), Some("2024-01-01".into())),
            Err(SearchDateBoundError::InvertedWindow { .. })
        ));
        assert!(SearchDateBound::new(Some("2024-01-01".into()), Some("2024-01-01".into())).is_ok());
        assert!(SearchDateBound::new(Some("2024-01-01".into()), Some("2024-01-02".into())).is_ok());

        // The rejection also holds through parse and the composed aggregate wire type
        // A client cannot smuggle an inverted window past the outer types
        let inverted = serde_json::json!({"fromDate": "2024-03-15", "toDate": "2024-01-01"});
        let err = SearchDateBound::parse(&inverted)
            .expect_err("inverted window must fail parse")
            .to_string();
        assert!(err.contains("on or before"), "unhelpful error: {err}");
        assert!(
            ToolOverridesUpdate::parse(&serde_json::json!({"xSearch": {"dateBound": &inverted}}))
                .is_err(),
            "inverted window must fail through the aggregate wire type"
        );
    }

    #[test]
    fn json_schema_converts_to_all_three_api_wire_formats() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "summary": { "type": "string" } },
            "required": ["summary"]
        });

        let req = ConversationRequest::from_items(vec![ConversationItem::user("Summarize")])
            .with_json_schema(schema.clone());

        // Chat Completions: json_schema becomes response_format
        let chat_req: ChatCompletionRequest = req.clone().into();
        let fmt = serde_json::to_value(chat_req.response_format.unwrap()).unwrap();
        assert_eq!(fmt.get("type"), Some(&serde_json::json!("json_schema")));
        assert_eq!(
            fmt.get("json_schema").and_then(|v| v.get("name")),
            Some(&serde_json::json!(STRUCTURED_OUTPUT_SCHEMA_NAME))
        );
        assert_eq!(
            fmt.get("json_schema").and_then(|v| v.get("strict")),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            fmt.get("json_schema").and_then(|v| v.get("schema")),
            Some(&schema)
        );

        // Responses API: json_schema becomes text.format
        let resp: rs::CreateResponse = (&req).into();
        let rs::TextResponseFormatConfiguration::JsonSchema(f) = resp.text.unwrap().format else {
            panic!("Expected json_schema format");
        };
        assert_eq!(f.name, STRUCTURED_OUTPUT_SCHEMA_NAME);
        assert_eq!(f.strict, Some(true));
        assert_eq!(f.schema, Some(schema.clone()));

        // Messages API: json_schema becomes output_config.format
        let msgs_req = build_messages_request(&req);
        let output_config = msgs_req.output_config.expect("output_config should be set");
        let fmt = output_config.format.expect("format should be set");
        let crate::messages::OutputFormat::JsonSchema { schema: s } = fmt;
        assert_eq!(s, schema);
        assert!(msgs_req.thinking.is_none());
        assert!(output_config.effort.is_none());
    }

    #[test]
    fn test_messages_request_cache_breakpoint_placement() {
        let json = agent_request(2);
        let Some(messages) = json.get("messages").and_then(|v| v.as_array()) else {
            panic!("expected messages array: {json:#}");
        };

        assert_eq!(
            json.pointer("/system/0/cache_control/type")
                .and_then(|v| v.as_str()),
            Some("ephemeral"),
            "{json:#}",
        );
        let Some(last_msg) = messages.last() else {
            panic!("expected last message: {json:#}");
        };
        assert_eq!(
            marker_on_last_block(last_msg),
            Some("ephemeral"),
            "tip: {json:#}"
        );
        assert_eq!(
            last_msg
                .get("content")
                .and_then(|c| c.as_array())
                .and_then(|b| b.last())
                .and_then(|b| b.get("type"))
                .and_then(|t| t.as_str()),
            Some("tool_result"),
        );

        let Some(previous_user) = messages
            .len()
            .checked_sub(1)
            .and_then(|n| messages.get(..n))
            .and_then(|prefix| {
                prefix
                    .iter()
                    .rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            })
        else {
            panic!("previous user: {json:#}");
        };
        let Some(prev) = messages.get(previous_user) else {
            panic!("previous user index: {json:#}");
        };
        assert_eq!(
            marker_on_last_block(prev),
            Some("ephemeral"),
            "previous turn's tip: {json:#}",
        );
        assert_eq!(count_cache_control(&json), 3, "{json:#}");
    }

    /// truncate_bytes must not panic on a multi-byte char boundary.
    #[test]
    fn test_truncate_bytes_non_ascii() {
        // "路径" is 6 bytes (2 CJK chars × 3 bytes each).
        // Truncating at 4 would land inside the second char, so it must walk back to 3
        let s = "路径";
        assert_eq!(s.len(), 6);
        assert_eq!(truncate_bytes(s, 4), "路"); // only 3 bytes fit
        assert_eq!(truncate_bytes(s, 3), "路"); // exact boundary
        assert_eq!(truncate_bytes(s, 6), s); // full string
        assert_eq!(truncate_bytes(s, 100), s); // larger than string
        assert_eq!(truncate_bytes(s, 0), ""); // zero

        // Emoji (4-byte): truncating at 5 must back up to 4.
        let e = "🎉!";
        assert_eq!(e.len(), 5); // 4-byte emoji plus the 1-byte '!'
        assert_eq!(truncate_bytes(e, 5), "🎉!");
        assert_eq!(truncate_bytes(e, 4), "🎉");
        assert_eq!(truncate_bytes(e, 3), ""); // 3 lands inside the emoji, so it walks back to 0
    }

    // ============================================================================
    // conversation_truncate_for_prompt Tests
    // ============================================================================

    #[test]
    fn test_truncate_for_prompt_basic() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"), // prompt 0
            ConversationItem::assistant("Asst 1"),
            ConversationItem::user("User 2"), // prompt 1
            ConversationItem::assistant("Asst 2"),
            ConversationItem::user("User 3"), // prompt 2
        ];

        // Keep up to and including prompt 0 (first user message)
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 3);

        // Keep up to and including prompt 1
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 5);

        // Keep up to and including prompt 2 (all messages)
        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 6);
    }

    #[test]
    fn test_truncate_for_prompt_with_tool_calls() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"), // prompt 0
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call_1".into(),
                name: "bash".to_string(),
                arguments: "{}".into(),
            }]),
            ConversationItem::tool_result("call_1", "result"),
            ConversationItem::assistant("Done"),
            ConversationItem::user("User 2"), // prompt 1
        ];

        // Keep up to prompt 0
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 5);

        // Keep up to prompt 1 (all)
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 6);
    }

    #[test]
    fn test_truncate_for_prompt_empty() {
        let conversation: Vec<ConversationItem> = vec![];
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 0);
    }

    #[test]
    fn test_truncate_for_prompt_no_user_messages() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::assistant("Asst"),
        ];

        // No user messages, so target_prompt_index 0 keeps everything
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 2);
    }

    #[test]
    fn test_truncate_for_prompt_skips_interspersed_system_reminders() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"),
            ConversationItem::assistant("Asst 1"),
            ConversationItem::system_reminder("<system-reminder>skill update</system-reminder>"),
            ConversationItem::user("User 2"),
            ConversationItem::assistant("Asst 2"),
            ConversationItem::system_reminder("<system-reminder>plan reminder</system-reminder>"),
            ConversationItem::user("User 3"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 4);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 7);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 8);
    }

    #[test]
    fn test_truncate_for_prompt_skips_auto_continue() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"),
            ConversationItem::assistant("Asst 1"),
            ConversationItem::auto_continue("Continue working on the task"),
            ConversationItem::assistant("Continuing..."),
            ConversationItem::user("User 2"),
            ConversationItem::assistant("Asst 2"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 5);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 7);
    }

    #[test]
    fn test_truncate_for_prompt_skips_auto_recovery() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"),
            ConversationItem::assistant("Asst 1"),
            ConversationItem::auto_recovery("Try the tool again"),
            ConversationItem::assistant("Retrying..."),
            ConversationItem::user("User 2"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 5);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 6);
    }

    #[test]
    fn test_truncate_for_prompt_skips_synthetic_user() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("User 1"),
            ConversationItem::assistant("Asst 1"),
            ConversationItem::system_reminder("Stop repeating"),
            ConversationItem::assistant("Retry guidance"),
            ConversationItem::user("User 2"),
            ConversationItem::assistant("Asst 2"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 5);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 7);
    }

    /// Regression (rewind kept the rewound turn).
    /// Synthetic-origin turns (auto-wake task/subagent completion, notification drain, scheduler) consume a prompt_index slot.
    /// The counting fallback must therefore treat their marker-less user items as turn starts.
    #[test]
    fn test_truncate_for_prompt_counts_marker_less_synthetic_turn_starts() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("<user_info>preamble</user_info>"),
            ConversationItem::user("P0"),
            ConversationItem::assistant("A0"),
            ConversationItem::task_completed("Background task abc completed"), // turn 1
            ConversationItem::assistant("A1"),
            ConversationItem::user("P2"),
            ConversationItem::assistant("A2"),
        ];

        // Rewind to turn 2 cuts at P2.
        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 6);
        // Rewind to the auto-wake turn itself cuts at the wake item.
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 4);
        // Rewind to turn 0 keeps only the preamble prefix.
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 2);
    }

    /// Mid-turn synthetics must NOT count as turn starts even in a session that also contains turn-start synthetics.
    #[test]
    fn test_truncate_for_prompt_mid_turn_synthetics_do_not_count() {
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("<user_info>preamble</user_info>"),
            ConversationItem::user("P0"),
            ConversationItem::interjection("also do this"), // mid-turn
            ConversationItem::assistant("A0"),
            ConversationItem::scheduler_fired("loop fired"), // turn 1
            ConversationItem::system_reminder("reminder"),   // mid-turn
            ConversationItem::assistant("A1"),
            ConversationItem::user("P2"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 8);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 5);
    }

    /// Explicit `UserItem::prompt_index` markers are authoritative.
    /// They resync the running index regardless of synthetic reasons.
    /// They also locate the cut in compaction-rebuilt prefixes where counting is structurally wrong.
    #[test]
    fn test_truncate_for_prompt_prefers_prompt_index_markers() {
        let marked = |text: &str, idx: usize| {
            let mut item = ConversationItem::user(text);
            item.set_prompt_index(idx);
            item
        };
        let marked_wake = |text: &str, idx: usize| {
            let mut item = ConversationItem::task_completed(text);
            item.set_prompt_index(idx);
            item
        };

        // Post-compaction shape: rebuilt preamble, carried last user query, summary, then a marker-carrying live turn
        // Counting would assign the carried query index 0 and never find turn 12; the marker does
        let conversation = vec![
            ConversationItem::system("SP"),
            ConversationItem::user("<user_info>rebuilt</user_info>"),
            ConversationItem::user("carried last user query"),
            ConversationItem::user_meta("summary"),
            marked_wake("Background task xyz completed", 11),
            ConversationItem::assistant("A11"),
            marked("P12", 12),
            ConversationItem::assistant("A12"),
        ];

        assert_eq!(conversation_truncate_for_prompt(&conversation, 12), 6);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 11), 4);
        // Unnumbered user rows do not open turns once markers exist (mid-turn phantoms omit prompt_index)
        // Rewind to 12 finds no marker at or above 12
        let mut mixed = conversation.clone();
        let Some(slot) = mixed.get_mut(6) else {
            panic!("expected mixed[6]: {mixed:?}");
        };
        *slot = ConversationItem::user("mid-turn phantom");
        assert_eq!(conversation_truncate_for_prompt(&mixed, 12), mixed.len());
        assert_eq!(conversation_truncate_for_prompt(&mixed, 11), 4);
    }

    /// Mid-turn plain users without `prompt_index` must not shift the cut when real turns carry markers (bash-mode / permission followup shape).
    #[test]
    fn test_truncate_for_prompt_unmarked_phantoms_ignored_when_markers_present() {
        let marked = |text: &str, idx: usize| {
            let mut item = ConversationItem::user(text);
            item.set_prompt_index(idx);
            item
        };
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("<user_info>preamble</user_info>"),
            marked("P0", 0),
            ConversationItem::assistant("A0"),
            ConversationItem::user("!pwd phantom"),
            ConversationItem::assistant("bash out"),
            marked("P1", 1),
            ConversationItem::assistant("A1"),
            ConversationItem::user("permission followup phantom"),
            marked("P2", 2),
            ConversationItem::assistant("A2"),
        ];
        // Cut at P2 (index 2) keeps through the followup phantom and drops P2 onward
        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 9);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 6);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 2);
    }

    /// Mixed upgrade: unmarked historic turns before the first marker still count (contiguous with the first absolute index).
    /// Phantoms after the first marker do not.
    /// Whole-buffer marker mode would under-cut here.
    #[test]
    fn test_truncate_for_prompt_mixed_unmarked_prefix_then_markers() {
        let marked = |text: &str, idx: usize| {
            let mut item = ConversationItem::user(text);
            item.set_prompt_index(idx);
            item
        };
        // Preamble, unmarked P0/P1, then marked P2/P3 with a phantom after the markers
        let conversation = vec![
            ConversationItem::system("System"),
            ConversationItem::user("<user_info>preamble</user_info>"),
            ConversationItem::user("old P0"),
            ConversationItem::assistant("A0"),
            ConversationItem::user("old P1"),
            ConversationItem::assistant("A1"),
            marked("new P2", 2),
            ConversationItem::assistant("A2"),
            ConversationItem::user("!pwd phantom"),
            marked("new P3", 3),
            ConversationItem::assistant("A3"),
        ];
        // Cut at 2 keeps through A1 (drops marked P2 onward)
        assert_eq!(conversation_truncate_for_prompt(&conversation, 2), 6);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 1), 4);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 0), 2);
        assert_eq!(conversation_truncate_for_prompt(&conversation, 3), 9);
    }

    /// `prompt_index` round-trips through JSON and is omitted when `None` (byte-stable with sessions written before the field existed).
    #[test]
    fn test_user_prompt_index_serde_roundtrip() {
        let plain = ConversationItem::user("hello");
        let json = serde_json::to_string(&plain).unwrap();
        assert!(
            !json.contains("prompt_index"),
            "None marker must be omitted, got {json}"
        );

        let mut marked = ConversationItem::user("hello");
        marked.set_prompt_index(7);
        let json = serde_json::to_string(&marked).unwrap();
        assert!(json.contains("\"prompt_index\":7"), "got {json}");
        let back: ConversationItem = serde_json::from_str(&json).unwrap();
        match back {
            ConversationItem::User(u) => assert_eq!(u.prompt_index, Some(7)),
            other => panic!("expected user, got {other:?}"),
        }

        // Old sessions (no field) deserialize as None.
        let legacy: ConversationItem =
            serde_json::from_str(r#"{"type":"user","content":[{"type":"text","text":"hi"}]}"#)
                .unwrap();
        match legacy {
            ConversationItem::User(u) => assert_eq!(u.prompt_index, None),
            other => panic!("expected user, got {other:?}"),
        }
    }

    // ============================================================================
    // transform_conversation_cwd Tests
    // ============================================================================

    #[test]
    fn test_transform_cwd_with_image_content() {
        let mut items = vec![ConversationItem::user_with_parts(vec![
            ContentPart::Text {
                text: "Look at /old/path/image.png".into(),
            },
            ContentPart::Image {
                url: "https://example.com/img.png".into(),
            },
        ])];

        transform_conversation_cwd(&mut items, "/old/path", "/new/path");

        let Some(ConversationItem::User(u)) = items.first() else {
            panic!("expected user: {items:?}");
        };
        let [ContentPart::Text { text }, ContentPart::Image { url }, ..] = u.content.as_slice()
        else {
            panic!("expected text+image: {:?}", u.content);
        };
        assert_eq!(text.as_ref(), "Look at /new/path/image.png");
        // Image URLs are left untouched
        assert_eq!(url.as_ref(), "https://example.com/img.png");
    }

    // ============================================================================
    // transform_conversation_cwd Tool Call & Edge Case Tests
    // ============================================================================

    #[test]
    fn test_transform_cwd_transforms_tool_call_arguments() {
        // Tool call arguments containing paths are transformed alongside text content.
        // The model then sees consistent paths on the next turn
        let worktree = "/home/user/.grok/worktrees/project/ab-uuid-a";
        let root = "/home/user/project";

        let mut items = vec![ConversationItem::Assistant(AssistantItem {
            content: format!("I'll read the file at {worktree}/src/main.rs").into(),
            tool_calls: vec![
                ToolCall {
                    id: "call_1".into(),
                    name: "read_file".to_string(),
                    arguments: format!(r#"{{"target_file":"{worktree}/src/main.rs"}}"#).into(),
                },
                ToolCall {
                    id: "call_2".into(),
                    name: "search_replace".to_string(),
                    arguments: format!(
                        r#"{{"file_path":"{worktree}/src/lib.rs","old_string":"foo","new_string":"bar"}}"#
                    ).into(),
                },
                ToolCall {
                    id: "call_3".into(),
                    name: "run_terminal_cmd".to_string(),
                    arguments: format!(
                        r#"{{"command":"cargo test --manifest-path {worktree}/Cargo.toml"}}"#
                    ).into(),
                },
            ],
            model_id: None,
            model_fingerprint: None,
            reasoning_effort: None,
        })];

        transform_conversation_cwd(&mut items, worktree, root);

        // Content is transformed
        assert_eq!(
            items.first().map(|i| i.text_content()),
            Some(format!("I'll read the file at {root}/src/main.rs"))
        );

        // Tool call arguments are also transformed
        let Some(ConversationItem::Assistant(a)) = items.first() else {
            panic!("Expected Assistant item: {items:?}");
        };
        let [tc0, tc1, tc2, ..] = a.tool_calls.as_slice() else {
            panic!("expected three tool calls: {:?}", a.tool_calls);
        };
        assert!(
            tc0.arguments.contains(root),
            "read_file arguments should contain root path"
        );
        assert!(
            !tc0.arguments.contains(worktree),
            "read_file arguments should not contain worktree path"
        );
        assert!(
            tc1.arguments.contains(root),
            "search_replace arguments should contain root path"
        );
        assert!(
            !tc1.arguments.contains(worktree),
            "search_replace arguments should not contain worktree path"
        );
        assert!(
            tc2.arguments.contains(root),
            "run_terminal_cmd arguments should contain root path"
        );
        assert!(
            !tc2.arguments.contains(worktree),
            "run_terminal_cmd arguments should not contain worktree path"
        );
    }

    #[test]
    fn test_transform_cwd_worktree_to_root_syncback() {
        // End-to-end sync-back scenario: worktree paths become root paths
        // This simulates what happens when a forked session's worktree contents are synced back to the original root path
        let worktree = "/home/user/.grok/worktrees/myproject/fork-a";
        let root = "/home/user/myproject";

        let mut items = vec![
            // System prompt with worktree cwd
            ConversationItem::system(format!(
                "You are an AI assistant. The user's workspace is at {worktree}."
            )),
            // User message (typically doesn't have worktree paths, but could)
            ConversationItem::user("Fix the bug in main.rs"),
            // Assistant saying what it found
            ConversationItem::assistant(format!(
                "I found the issue in {worktree}/src/main.rs at line 42."
            )),
            // Tool result from read_file
            ConversationItem::tool_result(
                "call_1",
                format!("Contents of {worktree}/src/main.rs:\nfn main() {{}}\n"),
            ),
            // Assistant with tool calls for the fix
            ConversationItem::Assistant(AssistantItem {
                content: format!("I'll fix the bug in {worktree}/src/main.rs").into(),
                tool_calls: vec![ToolCall {
                    id: "call_2".into(),
                    name: "search_replace".to_string(),
                    arguments: format!(
                        r#"{{"file_path":"{worktree}/src/main.rs","old_string":"fn main() {{}}","new_string":"fn main() {{\n    println!(\"Hello\");\n}}"}}"#
                    ).into(),
                }],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ];

        transform_conversation_cwd(&mut items, worktree, root);

        // All text content should be transformed
        assert!(
            items
                .first()
                .is_some_and(|i| i.text_content().contains(root))
        );
        assert!(
            items
                .first()
                .is_some_and(|i| !i.text_content().contains(worktree))
        );

        assert!(
            items
                .get(2)
                .is_some_and(|i| i.text_content().contains(root))
        );
        assert!(
            items
                .get(2)
                .is_some_and(|i| !i.text_content().contains(worktree))
        );

        assert!(
            items
                .get(3)
                .is_some_and(|i| i.text_content().contains(root))
        );
        assert!(
            items
                .get(3)
                .is_some_and(|i| !i.text_content().contains(worktree))
        );

        assert!(
            items
                .get(4)
                .is_some_and(|i| i.text_content().contains(root))
        );
        assert!(
            items
                .get(4)
                .is_some_and(|i| !i.text_content().contains(worktree))
        );

        // Tool call arguments are also transformed (worktree to root)
        let Some(ConversationItem::Assistant(a)) = items.get(4) else {
            panic!("expected assistant at 4: {items:?}");
        };
        let Some(tc) = a.tool_calls.first() else {
            panic!("expected tool call: {:?}", a.tool_calls);
        };
        assert!(
            tc.arguments.contains(root),
            "tool_call arguments should contain root path after sync-back"
        );
        assert!(
            !tc.arguments.contains(worktree),
            "tool_call arguments should not contain worktree path after sync-back"
        );
    }

    #[test]
    fn test_transform_cwd_forward_fork_root_to_worktree() {
        // Forward direction: root to worktree (forking)
        // Tool call arguments are transformed so the fork session's history has consistent worktree paths everywhere
        let root = "/home/user/myproject";
        let worktree = "/home/user/.grok/worktrees/myproject/fork-a";

        let mut items = vec![
            ConversationItem::system(format!("Working in {root}.")),
            ConversationItem::Assistant(AssistantItem {
                content: format!("I previously edited {root}/src/main.rs").into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".to_string(),
                    arguments: format!(r#"{{"target_file":"{root}/src/main.rs"}}"#).into(),
                }],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ];

        transform_conversation_cwd(&mut items, root, worktree);

        // Text content transformed to worktree
        assert!(
            items
                .first()
                .is_some_and(|i| i.text_content().contains(worktree))
        );

        // Tool calls also transformed to worktree
        let Some(ConversationItem::Assistant(a)) = items.get(1) else {
            panic!("expected assistant at 1: {items:?}");
        };
        let Some(tc) = a.tool_calls.first() else {
            panic!("expected tool call: {:?}", a.tool_calls);
        };
        assert!(
            tc.arguments.contains(worktree),
            "tool_call arguments should contain worktree path after forward fork"
        );
        assert!(
            !tc.arguments.contains(root),
            "tool_call arguments should not contain root path after forward fork"
        );
    }

    #[test]
    fn test_transform_cwd_no_false_positives_on_partial_match() {
        // Ensure we don't accidentally match a prefix that's a substring
        let mut items = vec![ConversationItem::user(
            "/home/user/myproject-extra/src/main.rs and /home/user/myproject/src/lib.rs",
        )];

        transform_conversation_cwd(&mut items, "/home/user/myproject", "/new/path");

        // Both paths get transformed because str::replace does substring matching.
        // "/home/user/myproject-extra" contains "/home/user/myproject" as a prefix, so it becomes "/new/path-extra" (a known false positive)
        assert_eq!(
            items.first().map(|i| i.text_content()).as_deref(),
            Some("/new/path-extra/src/main.rs and /new/path/src/lib.rs")
        );
    }

    #[test]
    fn test_transform_cwd_mixed_conversation_full() {
        // Full conversation with all item types and multiple path occurrences
        let src = "/worktree/abc";
        let dst = "/root/project";

        let mut items = vec![
            ConversationItem::system(format!("Workspace: {src}")),
            ConversationItem::user_with_parts(vec![
                ContentPart::Text {
                    text: format!("Edit {src}/a.rs").into(),
                },
                ContentPart::Text {
                    text: format!("And {src}/b.rs").into(),
                },
            ]),
            ConversationItem::assistant(format!("Done with {src}/a.rs and {src}/b.rs")),
            ConversationItem::tool_result("call_1", format!("File {src}/a.rs saved")),
        ];

        transform_conversation_cwd(&mut items, src, dst);

        // System
        assert_eq!(
            items.first().map(|i| i.text_content()),
            Some(format!("Workspace: {dst}"))
        );

        // User - both text parts
        let Some(ConversationItem::User(u)) = items.get(1) else {
            panic!("expected user at 1: {items:?}");
        };
        let [
            ContentPart::Text { text: t0 },
            ContentPart::Text { text: t1 },
            ..,
        ] = u.content.as_slice()
        else {
            panic!("expected two text parts: {:?}", u.content);
        };
        assert_eq!(t0.as_ref(), format!("Edit {dst}/a.rs").as_str());
        assert_eq!(t1.as_ref(), format!("And {dst}/b.rs").as_str());

        // Assistant text
        assert_eq!(
            items.get(2).map(|i| i.text_content()),
            Some(format!("Done with {dst}/a.rs and {dst}/b.rs"))
        );

        // Tool result
        assert_eq!(
            items.get(3).map(|i| i.text_content()),
            Some(format!("File {dst}/a.rs saved"))
        );
    }

    #[test]
    fn test_transform_cwd_assistant_only_tool_calls_no_content() {
        // Assistant message with empty content but tool calls containing paths
        let worktree = "/home/user/.grok/worktrees/proj/fork-a";
        let root = "/home/user/proj";

        let mut items = vec![ConversationItem::assistant_tool_calls(vec![
            ToolCall {
                id: "call_1".into(),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"target_file":"{worktree}/src/main.rs"}}"#).into(),
            },
            ToolCall {
                id: "call_2".into(),
                name: "grep".to_string(),
                arguments: format!(r#"{{"pattern":"TODO","path":"{worktree}/src"}}"#).into(),
            },
        ])];

        transform_conversation_cwd(&mut items, worktree, root);

        // Content is empty, so no transform there
        assert_eq!(items.first().map(|i| i.text_content()).as_deref(), Some(""));

        // Tool call arguments are transformed
        let Some(ConversationItem::Assistant(a)) = items.first() else {
            panic!("expected assistant: {items:?}");
        };
        let [tc0, tc1, ..] = a.tool_calls.as_slice() else {
            panic!("expected two tool calls: {:?}", a.tool_calls);
        };
        assert!(
            tc0.arguments.contains(root),
            "read_file arguments should contain root path"
        );
        assert!(
            !tc0.arguments.contains(worktree),
            "read_file arguments should not contain worktree path"
        );
        assert!(
            tc1.arguments.contains(root),
            "grep arguments should contain root path"
        );
        assert!(
            !tc1.arguments.contains(worktree),
            "grep arguments should not contain worktree path"
        );
    }

    // ============================================================================
    // ConversationResponse Tests
    // ============================================================================

    #[test]
    fn test_fallback_text_after_empty_response_retry() {
        // Scenario: empty-response retry; text is present but no AgentMessageChunk events were streamed
        let response = ConversationResponse {
            items: vec![ConversationItem::assistant("All features implemented.")],
            stop_reason: Some(StopReason::Stop),
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 0,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        assert_eq!(
            response.fallback_text().as_deref(),
            Some("All features implemented.")
        );
    }

    #[test]
    fn test_fallback_text_none_when_chunks_streamed() {
        // Normal streaming: text was already delivered via AgentMessageChunk events, so no fallback is needed
        let response = ConversationResponse {
            items: vec![ConversationItem::assistant("Hello")],
            stop_reason: Some(StopReason::Stop),
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 42,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        assert!(response.fallback_text().is_none());
    }

    #[test]
    fn test_fallback_text_none_for_empty_response() {
        // Truly empty response (no content, no chunks): no fallback.
        let response = ConversationResponse {
            items: vec![ConversationItem::assistant("")],
            stop_reason: None,
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 0,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        assert!(response.fallback_text().is_none());
    }

    #[test]
    fn test_fallback_text_fires_for_reasoning_only_stream() {
        // Reasoning-only scenario: the model produced only thought chunks (which increment chunk_index but NOT message_chunks_emitted)
        // The final text arrived at completion time, so message_chunks_emitted is 0 even though the model did produce content
        // The fallback MUST fire in this case
        let response = ConversationResponse {
            items: vec![ConversationItem::assistant("Summary after reasoning.")],
            stop_reason: Some(StopReason::Stop),
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 0, // only reasoning chunks were streamed
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        assert_eq!(
            response.fallback_text().as_deref(),
            Some("Summary after reasoning.")
        );
    }

    #[test]
    fn test_fallback_text_none_for_tool_call_only_response() {
        // Tool-call-only response: no text content, no fallback needed.
        let response = ConversationResponse {
            items: vec![ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "1".into(),
                name: "read_file".to_string(),
                arguments: "{}".into(),
            }])],
            stop_reason: Some(StopReason::ToolCalls),
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: 0,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        assert!(response.fallback_text().is_none());
    }

    // ============================================================================
    // StopReason Conversion Tests
    // ============================================================================

    #[test]
    fn test_stop_reason_from_finish_reason() {
        assert_eq!(StopReason::from(FinishReason::Stop), StopReason::Stop);
        assert_eq!(StopReason::from(FinishReason::Length), StopReason::Length);
        assert_eq!(
            StopReason::from(FinishReason::ToolCalls),
            StopReason::ToolCalls
        );
        assert_eq!(
            StopReason::from(FinishReason::FunctionCall),
            StopReason::ToolCalls
        );
        assert_eq!(
            StopReason::from(FinishReason::ContentFilter),
            StopReason::ContentFilter
        );
    }

    #[test]
    fn test_conversation_item_with_model_id() {
        let item = ConversationItem::assistant("Hello").with_model_id("grok-3");

        let ConversationItem::Assistant(a) = item else {
            panic!("Expected Assistant");
        };
        assert_eq!(a.model_id, Some("grok-3".to_string()));

        // Non-assistant should be unchanged
        let user = ConversationItem::user("Hi").with_model_id("grok-3");
        assert_matches!(user, ConversationItem::User(_));
    }

    #[test]
    fn test_repair_no_tool_calls() {
        let mut conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
            ConversationItem::assistant("hi"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            0
        );
        assert_eq!(conv.len(), 3);
    }

    #[test]
    fn test_repair_all_answered() {
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::tool_result("c1", "ok"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            0
        );
    }

    #[test]
    fn test_repair_single_dangling() {
        let mut conv = vec![
            ConversationItem::user("hello"),
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 3);
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert!(tr.content.contains("cancelled"));
            assert!(tr.content.contains("run_terminal_cmd"));
        });
    }

    #[test]
    fn test_has_dangling_tool_calls() {
        // No tool calls: not dangling
        assert!(!has_dangling_tool_calls(&[
            ConversationItem::user("hello"),
            ConversationItem::assistant("hi"),
        ]));
        // Fully answered tool call: not dangling
        assert!(!has_dangling_tool_calls(&[
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::tool_result("c1", "ok"),
        ]));
        // Unanswered tool call (mid-tool / parked-on-permission case): dangling
        assert!(has_dangling_tool_calls(&[
            ConversationItem::user("hello"),
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
        ]));
        // Partially answered parallel calls: dangling
        assert!(has_dangling_tool_calls(&[
            assistant_with_calls(&[("c1", "a"), ("c2", "b")]),
            ConversationItem::tool_result("c1", "ok"),
        ]));
    }

    #[test]
    fn test_repair_single_dangling_harness_halted() {
        let mut conv = vec![
            ConversationItem::user("hello"),
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
        ];
        assert_eq!(
            repair_dangling_tool_calls(
                &mut conv,
                DanglingToolCallReason::HarnessHalted {
                    class: "policy_guard",
                },
            ),
            1
        );
        assert_eq!(conv.len(), 3);
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert_eq!(
                tr.content.as_ref(),
                "Tool execution was halted by the harness (policy_guard); \
                 the tool `run_terminal_cmd` was not executed.",
            );
            assert!(!tr.content.contains("user"));
        });
    }

    #[test]
    fn test_repair_multiple_dangling_with_harness_halted_preserves_order() {
        // Two parallel dangling tool calls, both rendered with the harness-halted wording, must be appended in original call order
        // Each carries its own tool name
        let mut conv = vec![assistant_with_calls(&[
            ("read_call_1", "read_file"),
            ("grep_call_2", "grep"),
        ])];
        assert_eq!(
            repair_dangling_tool_calls(
                &mut conv,
                DanglingToolCallReason::HarnessHalted {
                    class: "policy_guard",
                },
            ),
            2
        );
        assert_eq!(conv.len(), 3);
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "read_call_1");
            assert!(tr.content.contains("`read_file`"));
            assert!(tr.content.contains("policy_guard"));
        });
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "grep_call_2");
            assert!(tr.content.contains("`grep`"));
        });
    }

    #[test]
    fn test_repair_multiple_dangling() {
        let mut conv = vec![assistant_with_calls(&[("c1", "read_file"), ("c2", "grep")])];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            2
        );
        assert_eq!(conv.len(), 3);
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
        });
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c2");
        });
    }

    #[test]
    fn test_repair_partial() {
        // call_1 answered, call_2 not
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file"), ("c2", "grep")]),
            ConversationItem::tool_result("c1", "file contents"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 3);
        // Existing result stays at index 1, synthetic inserted after it
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert_eq!(tr.content.as_ref(), "file contents");
        });
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c2");
            assert!(tr.content.contains("cancelled"));
        });
    }

    #[test]
    fn test_repair_idempotent() {
        let mut conv = vec![
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
            ConversationItem::user("hi"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 3);
        // Synthetic result inserted right after the assistant, before the user message
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
        });
        assert_matches!(conv.get(2), Some(ConversationItem::User(_)));
        // Second call: nothing to repair
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            0
        );
        assert_eq!(conv.len(), 3);
    }

    #[test]
    fn test_repair_first_turn_resolved_second_dangling() {
        // First turn fully resolved, second turn dangling
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::tool_result("c1", "ok"),
            ConversationItem::assistant("done"),
            ConversationItem::user("second"),
            assistant_with_calls(&[("c2", "run_terminal_cmd")]),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 6);
        assert_matches!(conv.get(5), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c2");
        });
    }

    #[test]
    fn test_repair_dangling_before_text_only_assistant() {
        // First assistant has dangling tool call, second is text-only.
        // The full scan must still repair the first assistant's dangling call.
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::assistant("text-only response"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 3);
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert!(tr.content.contains("cancelled"));
        });
        assert_matches!(conv.get(2), Some(ConversationItem::Assistant(a)) => {
            assert!(a.tool_calls.is_empty());
        });
    }

    #[test]
    fn test_repair_inserts_after_last_tool_result() {
        // Assistant made 3 calls, first two answered, third dangling.
        // There's a user message after the tool results
        // The synthetic result goes after the last tool result, before the user message
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file"), ("c2", "grep"), ("c3", "bash")]),
            ConversationItem::tool_result("c1", "file contents"),
            ConversationItem::tool_result("c2", "grep results"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            1
        );
        assert_eq!(conv.len(), 4);
        // c1 result at index 1, c2 result at index 2, synthetic c3 at index 3
        assert_matches!(conv.get(3), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c3");
            assert!(tr.content.contains("cancelled"));
        });
    }

    #[test]
    fn test_repair_inserts_before_trailing_user_message() {
        // Assistant made calls, no answers, then user sent a message.
        // Synthetic results should go right after the assistant, before the user message.
        let mut conv = vec![
            ConversationItem::user("do stuff"),
            assistant_with_calls(&[("c1", "read_file"), ("c2", "grep")]),
            ConversationItem::user("never mind"),
        ];
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            2
        );
        assert_eq!(conv.len(), 5);
        // Original: [user, assistant, user]
        // After:    [user, assistant, tool(c1), tool(c2), user]
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
        });
        assert_matches!(conv.get(3), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c2");
        });
        assert_matches!(conv.get(4), Some(ConversationItem::User(_)));
    }

    #[test]
    fn test_repair_multiple_assistants_dangling_throughout() {
        // Simulates an old session where the user interrupted multiple tool calls across the conversation
        // All dangling calls must be repaired, not just the last one
        let mut conv = vec![
            ConversationItem::user("hello"),
            // Turn 1: assistant makes a call, user interrupts
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
            ConversationItem::user("no, the repo is already cloned"),
            // Turn 2: assistant works normally
            assistant_with_calls(&[("c2", "read_file")]),
            ConversationItem::tool_result("c2", "file contents"),
            ConversationItem::assistant("here's what I found"),
            // Turn 3: assistant makes a call, user interrupts again
            ConversationItem::user("now do something else"),
            assistant_with_calls(&[("c3", "run_terminal_cmd")]),
            ConversationItem::user("actually never mind"),
            // Turn 4: assistant makes a call, user interrupts yet again
            assistant_with_calls(&[("c4", "grep")]),
        ];
        // c1, c3, c4 are dangling; c2 is answered, so 3 repairs
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            3
        );
        // The 10 original items plus the 3 synthetic results
        assert_eq!(conv.len(), 13);
        // After repair, each unanswered tool call has a synthetic result in place.
        assert_matches!(conv.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert!(tr.content.contains("cancelled"));
        });
        assert_matches!(conv.get(9), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c3");
            assert!(tr.content.contains("cancelled"));
        });
        assert_matches!(conv.get(12), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c4");
            assert!(tr.content.contains("cancelled"));
        });
        // Idempotent: second run should find nothing
        assert_eq!(
            repair_dangling_tool_calls(&mut conv, DanglingToolCallReason::UserCancelled),
            0
        );
    }

    // ====================================================================
    // dedup_duplicate_tool_results tests
    // ====================================================================

    #[test]
    fn test_dedup_no_duplicates() {
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file"), ("c2", "grep")]),
            ConversationItem::tool_result("c1", "ok"),
            ConversationItem::tool_result("c2", "found it"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 0);
        assert_eq!(conv.len(), 3);
    }

    #[test]
    fn test_dedup_single_duplicate_keeps_last() {
        // This is the exact bug scenario: cancelled result followed by real result.
        let mut conv = vec![
            assistant_with_calls(&[("c1", "run_terminal_cmd")]),
            ConversationItem::tool_result(
                "c1",
                "Tool execution was cancelled by the user (tool `run_terminal_cmd` was not executed).",
            ),
            ConversationItem::tool_result("c1", "exit: 0\nreal output here"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 1);
        assert_eq!(conv.len(), 2); // the assistant and one tool_result
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert!(tr.content.contains("real output here"));
        });
    }

    #[test]
    fn test_dedup_multiple_calls_one_has_dup() {
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file"), ("c2", "grep")]),
            ConversationItem::tool_result("c1", "cancelled"),
            ConversationItem::tool_result("c2", "found it"),
            ConversationItem::tool_result("c1", "real content"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 1);
        assert_eq!(conv.len(), 3); // the assistant and two tool_results
        // c1 keeps the real content (last occurrence)
        let c1_results: Vec<_> = conv
            .iter()
            .filter_map(|item| {
                if let ConversationItem::ToolResult(tr) = item
                    && tr.tool_call_id == "c1"
                {
                    return Some(tr.content.as_ref());
                }
                None
            })
            .collect();
        assert_eq!(c1_results, vec!["real content"]);
    }

    #[test]
    fn test_dedup_no_tool_calls() {
        let mut conv = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("hello"),
            ConversationItem::assistant("hi"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 0);
        assert_eq!(conv.len(), 3);
    }

    #[test]
    fn test_dedup_empty_conversation() {
        let mut conv = vec![];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 0);
    }

    #[test]
    fn test_dedup_multiple_assistant_messages() {
        // Two assistant messages, each with a duplicate.
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::tool_result("c1", "old"),
            ConversationItem::tool_result("c1", "new"),
            ConversationItem::user("ok"),
            assistant_with_calls(&[("c2", "grep")]),
            ConversationItem::tool_result("c2", "stale"),
            ConversationItem::tool_result("c2", "fresh"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 2);
        assert_eq!(conv.len(), 5); // Two assistants, two kept tool_results, and one user remain
        assert_matches!(conv.get(1), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c1");
            assert_eq!(tr.content.as_ref(), "new");
        });
        assert_matches!(conv.get(4), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.tool_call_id, "c2");
            assert_eq!(tr.content.as_ref(), "fresh");
        });
    }

    #[test]
    fn test_dedup_idempotent() {
        let mut conv = vec![
            assistant_with_calls(&[("c1", "read_file")]),
            ConversationItem::tool_result("c1", "cancelled"),
            ConversationItem::tool_result("c1", "real"),
        ];
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 1);
        assert_eq!(conv.len(), 2);
        // Second run should be a no-op.
        assert_eq!(dedup_duplicate_tool_results(&mut conv), 0);
        assert_eq!(conv.len(), 2);
    }

    // ========== strip_images tests ==========

    #[test]
    fn test_strip_images_removes_user_images() {
        let mut req = ConversationRequest::default();
        let mut user = ConversationItem::user("describe this");
        user.add_image("data:image/png;base64,abc123".to_string());
        req.items.push(user);

        let stripped = req.strip_images();
        assert_eq!(stripped.len(), 1);

        // Verify image was replaced with placeholder text
        let Some(ConversationItem::User(user)) = req.items.first() else {
            panic!("Expected User item: {:?}", req.items);
        };
        assert_eq!(user.content.len(), 2); // the original text and the replaced image
        assert_matches!(user.content.get(1), Some(ContentPart::Text { text }) => {
            assert!(text.contains("image removed"));
        });
    }

    #[test]
    fn test_strip_images_returns_zero_when_no_images() {
        let mut req = ConversationRequest::default();
        req.items.push(ConversationItem::user("just text"));
        req.items.push(ConversationItem::system("system prompt"));
        req.items.push(ConversationItem::assistant("response"));

        let stripped = req.strip_images();
        assert_eq!(stripped.len(), 0);
    }

    #[test]
    fn test_strip_images_leaves_text_unchanged() {
        let mut req = ConversationRequest::default();
        req.items.push(ConversationItem::user("hello world"));

        req.strip_images();

        let Some(ConversationItem::User(user)) = req.items.first() else {
            panic!("Expected User item: {:?}", req.items);
        };
        assert_eq!(user.content.len(), 1);
        assert_matches!(user.content.first(), Some(ContentPart::Text { text }) => {
            assert_eq!(text.as_ref(), "hello world");
        });
    }

    #[test]
    fn test_strip_images_ignores_system_assistant_tool_items() {
        let mut req = ConversationRequest::default();
        req.items.push(ConversationItem::system("system prompt"));
        req.items.push(ConversationItem::assistant("response"));
        req.items
            .push(ConversationItem::tool_result("call-1", "result text"));

        let stripped = req.strip_images();
        assert_eq!(stripped.len(), 0);

        // Verify nothing was modified
        assert_matches!(req.items.first(), Some(ConversationItem::System(s)) => {
            assert_eq!(s.content.as_ref(), "system prompt");
        });
        assert_matches!(req.items.get(1), Some(ConversationItem::Assistant(a)) => {
            assert_eq!(a.content.as_ref(), "response");
        });
        assert_matches!(req.items.get(2), Some(ConversationItem::ToolResult(tr)) => {
            assert_eq!(tr.content.as_ref(), "result text");
        });
    }

    #[test]
    fn test_strip_images_mixed_content_only_replaces_images() {
        let mut req = ConversationRequest::default();
        let mut user = ConversationItem::user("look at these");
        user.add_image("data:image/png;base64,img1".to_string());
        req.items.push(user);

        req.strip_images();

        let Some(ConversationItem::User(user)) = req.items.first() else {
            panic!("Expected User item: {:?}", req.items);
        };
        assert_eq!(user.content.len(), 2);
        // Text part preserved
        assert_matches!(user.content.first(), Some(ContentPart::Text { text }) => {
            assert_eq!(text.as_ref(), "look at these");
        });
        // Image part replaced
        assert_matches!(user.content.get(1), Some(ContentPart::Text { text }) => {
            assert!(text.contains("image removed"));
        });
    }

    #[test]
    fn test_strip_images_multiple_user_items_with_images() {
        let mut req = ConversationRequest::default();

        let mut user1 = ConversationItem::user("first");
        user1.add_image("data:image/png;base64,aaa".to_string());
        user1.add_image("data:image/png;base64,bbb".to_string());
        req.items.push(user1);

        req.items.push(ConversationItem::assistant("ok"));

        let mut user2 = ConversationItem::user("second");
        user2.add_image("data:image/png;base64,ccc".to_string());
        req.items.push(user2);

        let stripped = req.strip_images();
        assert_eq!(stripped.len(), 3);
    }

    #[test]
    fn test_strip_images_clears_tool_result_images() {
        let mut req = ConversationRequest::default();
        req.items.push(ConversationItem::tool_result_with_images(
            "call_1",
            "Read image file: photo.png",
            vec![
                ContentPart::Image {
                    url: "data:image/png;base64,aaa".into(),
                },
                ContentPart::Image {
                    url: "data:image/png;base64,bbb".into(),
                },
            ],
        ));

        let stripped = req.strip_images();
        assert_eq!(stripped.len(), 2);

        let Some(ConversationItem::ToolResult(t)) = req.items.first() else {
            panic!("Expected ToolResult: {:?}", req.items);
        };
        assert!(t.images.is_empty(), "images should be cleared after strip");
        assert_eq!(
            t.content.as_ref(),
            "Read image file: photo.png",
            "text content preserved"
        );
    }

    /// The URL-scoped strip: listed URLs are stripped from both part kinds (User images become the placeholder, ToolResult images are removed).
    /// Unlisted images survive, and the count reflects parts stripped.
    #[test]
    fn test_strip_images_by_url_strips_only_listed_urls() {
        let listed: Arc<str> = "data:image/png;base64,aaa".into();
        let mut user = ConversationItem::user("look");
        user.add_image(listed.to_string());
        user.add_image("data:image/png;base64,unlisted".to_string());
        let mut items = vec![
            user,
            ConversationItem::tool_result_with_images(
                "call_1",
                "read photo.png",
                vec![
                    ContentPart::Image {
                        url: listed.clone(),
                    },
                    ContentPart::Image {
                        url: "data:image/png;base64,unlisted".into(),
                    },
                ],
            ),
        ];

        assert_eq!(strip_images_by_url(&mut items, &[listed]), 2);

        // The whole safety case for persisting via `replace_history`: an in-place strip never changes item count or ordering
        assert_eq!(items.len(), 2, "strip must never add or remove items");

        let Some(ConversationItem::User(u)) = items.first() else {
            panic!("expected User: {items:?}");
        };
        assert!(
            u.content.iter().any(|p| matches!(
                p,
                ContentPart::Text { text } if text.as_ref() == IMAGE_STRIP_PLACEHOLDER
            )),
            "listed user image must be replaced by the strip placeholder: {:?}",
            u.content
        );
        let user_image_urls: Vec<_> = u
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Image { url } => Some(url.as_ref()),
                _ => None,
            })
            .collect();
        assert_eq!(
            user_image_urls,
            ["data:image/png;base64,unlisted"],
            "listed user image replaced, unlisted survives"
        );
        let Some(ConversationItem::ToolResult(t)) = items.get(1) else {
            panic!("expected ToolResult: {items:?}");
        };
        assert!(
            matches!(t.images.as_slice(), [ContentPart::Image { url }] if url.contains("unlisted")),
            "listed tool image removed, unlisted survives: {:?}",
            t.images
        );
    }

    #[test]
    fn test_tool_result_with_images_serde_round_trip() {
        let item = ConversationItem::tool_result_with_images(
            "call_1",
            "Read image file: photo.png",
            vec![ContentPart::Image {
                url: "data:image/png;base64,iVBOR".into(),
            }],
        );
        let json = serde_json::to_string(&item).expect("serialize");
        let back: ConversationItem = serde_json::from_str(&json).expect("deserialize");

        if let ConversationItem::ToolResult(t) = &back {
            assert_eq!(t.images.len(), 1);
            assert!(
                matches!(t.images.first(), Some(ContentPart::Image { url }) if url.contains("iVBOR"))
            );
        } else {
            panic!("Expected ToolResult");
        }
    }

    #[test]
    fn test_tool_result_without_images_serde_omits_field() {
        let item = ConversationItem::tool_result("call_1", "just text");
        let json = serde_json::to_string(&item).expect("serialize");
        assert!(
            !json.contains("images"),
            "empty images should be omitted: {json}"
        );

        let back: ConversationItem = serde_json::from_str(&json).expect("deserialize");
        if let ConversationItem::ToolResult(t) = &back {
            assert!(t.images.is_empty());
        } else {
            panic!("Expected ToolResult");
        }
    }

    // ── SyntheticReason tests ─────────────────────────────────────────────────

    /// Historical `doom_loop_warning` tags deserialize as Unknown after removal.
    #[test]
    fn historical_doom_loop_warning_deserializes_as_unknown() {
        let json = serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": "legacy"}],
            "synthetic_reason": "doom_loop_warning"
        });
        let item: ConversationItem = serde_json::from_value(json).expect("deserialize");
        if let ConversationItem::User(u) = item {
            assert_eq!(SyntheticReason::Unknown, u.synthetic_reason);
        } else {
            panic!("expected User variant");
        }
    }

    #[test]
    fn working_directory_switch_round_trips_generation() {
        let item = ConversationItem::working_directory_switch("moved", 7);
        let json = serde_json::to_value(&item).expect("serialize");
        assert_eq!(
            json.get("synthetic_reason").and_then(|v| v.as_str()),
            Some("working_directory_switch")
        );
        assert_eq!(json.get("cwd_generation"), Some(&serde_json::json!(7)));
        let back: ConversationItem = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.working_directory_switch_generation(), Some(7));
    }

    #[test]
    fn legacy_user_defaults_cwd_generation_to_none() {
        let item: ConversationItem = serde_json::from_value(serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": "hello"}],
            "synthetic_reason": "system_reminder"
        }))
        .expect("deserialize legacy user");
        let ConversationItem::User(user) = item else {
            panic!("expected user");
        };
        assert!(user.cwd_generation.is_none());
    }

    /// `synthetic_reason` round-trips through JSON.
    /// Old sessions that omit the field entirely deserialize as `Human` (via the serde default); sessions with the field preserve the value.
    #[test]
    fn synthetic_reason_json_roundtrip() {
        // New: has synthetic_reason.
        let item = ConversationItem::system_reminder("test");
        let json = serde_json::to_value(&item).expect("serialize");
        assert_eq!(
            json.get("synthetic_reason"),
            Some(&serde_json::json!("system_reminder")),
            "synthetic_reason must serialize as snake_case string"
        );
        let back: ConversationItem = serde_json::from_value(json).expect("deserialize");
        if let ConversationItem::User(u) = back {
            assert_eq!(SyntheticReason::SystemReminder, u.synthetic_reason);
        } else {
            panic!("expected User variant after round-trip");
        }

        // Old JSONL without the field must deserialize as Human.
        let old_json = serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": "hello"}]
        });
        let old: ConversationItem = serde_json::from_value(old_json).expect("deserialize old");
        assert!(
            old.is_human_user_turn(),
            "old sessions without synthetic_reason must deserialize as Human"
        );
    }

    /// Real user messages must NOT serialize the `synthetic_reason` key at all (ensured by `skip_serializing_if = "SyntheticReason::is_human"`).
    #[test]
    fn real_user_message_omits_synthetic_reason_key() {
        let item = ConversationItem::user("hello");
        let json = serde_json::to_value(&item).expect("serialize");
        assert!(
            json.get("synthetic_reason").is_none(),
            "real user messages must not include synthetic_reason in JSON"
        );
    }

    /// `Primary` is the absent state on the wire: the leading prompt writes no key and reads back from none, while a tagged System item round-trips its tag.
    #[test]
    fn system_item_synthetic_reason_serde() {
        let primary = serde_json::json!({"type": "system", "content": "prompt"});
        assert_eq!(
            primary,
            serde_json::to_value(ConversationItem::system("prompt")).unwrap()
        );
        let item: ConversationItem = serde_json::from_value(primary).unwrap();
        assert!(item.is_primary_system_turn());

        let tagged = serde_json::json!({
            "type": "system",
            "content": "reminder",
            "synthetic_reason": "system_reminder"
        });
        let item: ConversationItem = serde_json::from_value(tagged.clone()).unwrap();
        assert!(!item.is_primary_system_turn());
        assert_eq!(tagged, serde_json::to_value(&item).unwrap());
    }

    #[test]
    fn parent_agent_message_alias_deserializes() {
        let payload = serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": "agent context"}],
            "synthetic_reason": "parent_agent_message"
        });
        let item: ConversationItem =
            serde_json::from_value(payload).expect("deserialize staged spelling");
        assert!(matches!(
            item,
            ConversationItem::User(UserItem {
                synthetic_reason: SyntheticReason::AgentMessage,
                ..
            })
        ));
    }

    /// Forward-compat regression guard for the `#[serde(other)]` arm.
    /// Payloads from newer clients with an unknown `synthetic_reason` value must deserialize as `SyntheticReason::Unknown` rather than failing.
    #[test]
    fn unknown_synthetic_reason_deserializes_for_forward_compat() {
        let payload = serde_json::json!({
            "type": "user",
            "content": [{"type": "text", "text": "hello"}],
            "synthetic_reason": "some_future_variant"
        });
        let item: ConversationItem =
            serde_json::from_value(payload).expect("deserialize forward-compat payload");
        if let ConversationItem::User(u) = item {
            assert_eq!(SyntheticReason::Unknown, u.synthetic_reason);
            assert!(u.synthetic_reason.starts_prompt_turn());
        } else {
            panic!("expected User variant");
        }
    }

    #[test]
    fn empty_reason_none_when_has_content() {
        let resp = make_response(ConversationItem::assistant("hello"));
        assert!(resp.empty_reason().is_none());
        assert!(!resp.is_empty());
    }

    #[test]
    fn empty_reason_none_when_has_tool_calls() {
        let resp = make_response(ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "tc1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        }]));
        assert!(resp.empty_reason().is_none());
        assert!(!resp.is_empty());
    }

    #[test]
    fn empty_reason_no_visible_content() {
        let resp = make_response(ConversationItem::assistant(""));
        assert_eq!(
            resp.empty_reason(),
            Some(crate::error::EmptyReason::NoVisibleContent)
        );
        assert!(resp.is_empty());
    }

    #[test]
    fn empty_reason_non_assistant_returns_no_visible_content() {
        let resp = make_response(ConversationItem::tool_result("tc1", "result"));
        assert_eq!(
            resp.empty_reason(),
            Some(crate::error::EmptyReason::NoVisibleContent)
        );
    }

    /// `LengthPolicy::verdict` is the single fail-vs-salvage gate shared by the actor and direct-collect paths; pin every cell.
    #[test]
    fn length_policy_verdict_gate() {
        let length = |item: ConversationItem| {
            let mut r = make_response(item);
            r.stop_reason = Some(StopReason::Length);
            r
        };
        let call = |arguments: &str| ToolCall {
            id: "tc1".into(),
            name: "do_thing".into(),
            arguments: arguments.into(),
        };
        let text = length(ConversationItem::assistant("partial"));
        let empty = length(ConversationItem::assistant(""));
        let complete_tools = length(ConversationItem::assistant_tool_calls(vec![call(
            "{\"x\": 1}",
        )]));
        // Zero-arg calls stream empty arguments; downstream normalizes to `{}`.
        let empty_args_tools = length(ConversationItem::assistant_tool_calls(vec![call("")]));
        let truncated_tools = length(ConversationItem::assistant_tool_calls(vec![call(
            "{\"x\": \"trunc",
        )]));
        // One truncated call poisons the batch even when siblings are complete.
        let mixed_tools = length(ConversationItem::assistant_tool_calls(vec![
            call("{\"x\": 1}"),
            call("{\"x\": \"trunc"),
        ]));
        let stop = make_response(ConversationItem::assistant("done"));

        assert_eq!(LengthPolicy::default(), LengthPolicy::CompleteToolCalls);

        assert_eq!(LengthPolicy::Fail.verdict(&text), LengthVerdict::Fail);
        assert_eq!(
            LengthPolicy::Fail.verdict(&complete_tools),
            LengthVerdict::Fail
        );
        assert_eq!(LengthPolicy::Fail.verdict(&stop), LengthVerdict::Pass);

        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&complete_tools),
            LengthVerdict::SalvageToolCalls
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&empty_args_tools),
            LengthVerdict::SalvageToolCalls
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&truncated_tools),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&mixed_tools),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&text),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&empty),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompleteToolCalls.verdict(&stop),
            LengthVerdict::Pass
        );

        assert_eq!(
            LengthPolicy::CompletePartial.verdict(&text),
            LengthVerdict::Salvage
        );
        assert_eq!(
            LengthPolicy::CompletePartial.verdict(&empty),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompletePartial.verdict(&complete_tools),
            LengthVerdict::SalvageToolCalls
        );
        assert_eq!(
            LengthPolicy::CompletePartial.verdict(&truncated_tools),
            LengthVerdict::Fail
        );
        assert_eq!(
            LengthPolicy::CompletePartial.verdict(&stop),
            LengthVerdict::Pass
        );
    }

    #[test]
    fn stop_reason_as_str_matches_serde() {
        assert_eq!(StopReason::Stop.as_ref(), "stop");
        assert_eq!(StopReason::Length.as_ref(), "length");
        assert_eq!(StopReason::ToolCalls.as_ref(), "tool_calls");
        assert_eq!(StopReason::ContentFilter.as_ref(), "content_filter");
    }

    // `tco_*` reasoning items from parallel backend tool calls round-trip losslessly as N sibling `Reasoning` items. (A prior data-loss bug: `AssistantItem.reasoning` was last-write-wins.); Multi-turn conversations preserve emission order `[Sys, U1, R, BTC*, A1, U2, R, BTC*, A2, ...]` rather than `[Sys, U1, ..., UN, R*, A*]`. (A prior ordering bug defeated the server-side prefix cache.); `conversation_to_chat_messages` folds preceding Reasoning siblings into the next assistant's `reasoning_content`. This is the chat-completions wire path; `patch_reasoning_text_types` injects the `type: "reasoning_text"` discriminator on nested `content[]` items. async-openai's derived Serialize omits it.

    #[test]
    fn multi_tco_reasoning_items_round_trip_as_siblings() {
        let make_reasoning = |suffix: &str, summary: &str, encrypted: Option<&str>| {
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: format!("rs_resp123_{suffix}"),
                summary: if summary.is_empty() {
                    vec![]
                } else {
                    vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                        text: summary.to_string(),
                    })]
                },
                content: None,
                encrypted_content: encrypted.map(str::to_owned),
                status: Some(rs::OutputStatus::Completed),
            })
        };
        let make_tco = |suffix: &str| {
            rs::OutputItem::Reasoning(rs::ReasoningItem {
                id: format!("tco_resp123_call-{suffix}"),
                summary: vec![],
                content: None,
                encrypted_content: Some(format!("enc_blob_{suffix}")),
                status: Some(rs::OutputStatus::Completed),
            })
        };
        let make_ws = |suffix: &str, query: &str| {
            rs::OutputItem::WebSearchCall(rs::WebSearchToolCall {
                id: format!("ws_resp123_{suffix}"),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: query.to_string(),
                    sources: Some(vec![]),
                }),
            })
        };

        let response = rs::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp123".to_string(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "distill".to_string(),
            object: "response".to_string(),
            output: vec![
                make_reasoning("a", "thinking pre-search", None),
                make_ws("5", "capybara facts"),
                make_ws("6", "wombat habitat"),
                make_tco("5"),
                make_tco("6"),
                make_reasoning("b", "follow-up thinking", None),
                make_ws("7", "platypus venom"),
                make_tco("7"),
            ],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status: rs::Status::Completed,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        };

        let items = response_to_conversation_items(response);

        // Five reasoning siblings: 2 real `rs_*` and 3 encrypted `tco_*`
        let reasoning_ids: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                ConversationItem::Reasoning(r) => Some(r.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning_ids,
            vec![
                "rs_resp123_a",
                "tco_resp123_call-5",
                "tco_resp123_call-6",
                "rs_resp123_b",
                "tco_resp123_call-7",
            ],
            "every reasoning item — including all 3 tco_* — must round-trip in emission order"
        );

        // Exactly one trailing Assistant.
        assert!(matches!(items.last(), Some(ConversationItem::Assistant(_))));

        // Backend tool calls preserved in order.
        let bt_count = items
            .iter()
            .filter(|i| matches!(i, ConversationItem::BackendToolCall(_)))
            .count();
        assert_eq!(bt_count, 3);
    }

    #[test]
    fn conversation_to_chat_messages_folds_reasoning_into_following_assistant() {
        let items = vec![
            ConversationItem::user("hi"),
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "thinking step 1".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r2".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "thinking step 2".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
            ConversationItem::assistant("answer"),
        ];

        let msgs = conversation_to_chat_messages(items);
        let [user, assistant] = msgs.as_slice() else {
            panic!("expected user + assistant: {msgs:?}");
        };
        assert_eq!(user.role, Role::User);
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.text_content(), "answer");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("thinking step 1\nthinking step 2"),
            "reasoning text joined and attached to the assistant"
        );
    }

    #[test]
    fn conversation_to_chat_messages_drops_trailing_reasoning() {
        let items = vec![
            ConversationItem::user("hi"),
            ConversationItem::Reasoning(rs::ReasoningItem {
                id: "r1".to_string(),
                summary: vec![rs::SummaryPart::SummaryText(rs::SummaryTextContent {
                    text: "abandoned thinking".to_string(),
                })],
                content: None,
                encrypted_content: None,
                status: None,
            }),
        ];
        let msgs = conversation_to_chat_messages(items);
        assert_eq!(
            msgs.len(),
            1,
            "trailing reasoning has no assistant to attach to"
        );
        assert_eq!(msgs.first().map(|m| m.role), Some(Role::User));
    }

    #[test]
    fn conversation_to_chat_messages_folds_reasoning_across_backend_tool_call() {
        // The canonical post-tool-call ordering is `[..., Reasoning, BackendToolCall, Assistant]`.
        // The BackendToolCall is emitted as its own synthetic assistant message, but it must NOT drop the pending reasoning
        let items = vec![
            ConversationItem::user("hi"),
            reasoning_sibling("r1", "thinking before search", None),
            ConversationItem::BackendToolCall(BackendToolCallItem {
                kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                    id: "ws_1".to_string(),
                    status: rs::WebSearchToolCallStatus::Completed,
                    action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                        query: "capybaras".to_string(),
                        sources: Some(vec![]),
                    }),
                }),
            }),
            ConversationItem::assistant("answer"),
        ];

        let msgs = conversation_to_chat_messages(items);

        let [user, btc, assistant] = msgs.as_slice() else {
            panic!("expected user + synthetic BTC assistant + assistant: {msgs:?}");
        };
        assert_eq!(user.role, Role::User);
        // BackendToolCall becomes a synthetic assistant carrying its summary; it does not itself carry the reasoning
        assert_eq!(btc.role, Role::Assistant);
        assert_eq!(btc.text_content(), "[backend web_search] search: capybaras");
        assert_eq!(
            btc.reasoning_content.as_deref(),
            None,
            "reasoning lands on the real assistant, not the synthetic BTC message"
        );
        // The real assistant turn keeps the reasoning that preceded the backend tool call
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.text_content(), "answer");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("thinking before search"),
            "reasoning preceding a BackendToolCall folds onto the following \
             assistant rather than being dropped"
        );
    }

    #[test]
    fn conversation_to_chat_messages_passes_reasoning_content_on_tool_call_turns() {
        // DeepSeek thinking mode with `tools`: the assistant that issued
        // tool_calls must carry `reasoning_content` on the next request, or
        // the API returns 400 "must be passed back".
        let items = vec![
            ConversationItem::user("continue"),
            ConversationItem::Reasoning(crate::synthesized_reasoning_item(
                "plan the windows check",
            )),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call_1".into(),
                name: "bash".to_string(),
                arguments: r#"{"command":"rustc -vV"}"#.into(),
            }]),
            ConversationItem::tool_result("call_1", "rustc 1.80"),
        ];
        let msgs = conversation_to_chat_messages(items);
        let assistant = msgs
            .iter()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
            .expect("tool-call assistant");
        assert_eq!(
            assistant.reasoning_content.as_deref(),
            Some("plan the windows check")
        );
        let json = serde_json::to_value(assistant).expect("assistant serializes");
        assert_eq!(
            json.get("reasoning_content").and_then(|v| v.as_str()),
            Some("plan the windows check"),
            "the Chat Completions wire body must include reasoning_content"
        );
    }

    #[test]
    fn tools_request_sends_reasoning_content_on_every_assistant() {
        // DeepSeek: a tools request must echo reasoning_content on *all*
        // previous assistant turns, even those that never thought and never
        // called a tool. A missing field is a 400.
        let req = ConversationRequest::from_items(vec![
            ConversationItem::user("hi"),
            ConversationItem::assistant("plain answer"),
            ConversationItem::user("now use a tool"),
            ConversationItem::Reasoning(crate::synthesized_reasoning_item("plan the call")),
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "call_1".into(),
                name: "bash".to_string(),
                arguments: r#"{"command":"true"}"#.into(),
            }]),
            ConversationItem::tool_result("call_1", "ok"),
        ])
        .with_model("deepseek-v4.1-flash")
        .with_tools(vec![ToolSpec {
            name: "bash".to_string(),
            description: Some("run".to_string()),
            parameters: serde_json::json!({}),
        }]);
        let chat: ChatCompletionRequest = req.into();
        let json = serde_json::to_value(&chat).expect("request serializes");
        let messages = json
            .get("messages")
            .and_then(|v| v.as_array())
            .expect("messages");
        let assistants: Vec<&serde_json::Value> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .collect();
        assert_eq!(assistants.len(), 2);
        assert_eq!(
            assistants[0]
                .get("reasoning_content")
                .and_then(|v| v.as_str()),
            Some(""),
            "an earlier assistant with no thinking still sends the field"
        );
        assert_eq!(
            assistants[1]
                .get("reasoning_content")
                .and_then(|v| v.as_str()),
            Some("plan the call")
        );
    }

    #[test]
    fn conversation_item_to_chat_message_backend_tool_call_is_synthetic_assistant() {
        // A BackendToolCall has no Chat Completions equivalent
        // It is emitted as a synthetic assistant message carrying its human-readable `text_summary()`
        let item = ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::WebSearch(rs::WebSearchToolCall {
                id: "ws_1".to_string(),
                status: rs::WebSearchToolCallStatus::Completed,
                action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                    query: "capybaras".to_string(),
                    sources: Some(vec![]),
                }),
            }),
        });

        let msg = conversation_item_to_chat_message(item);

        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.text_content(), "[backend web_search] search: capybaras");
        assert!(
            msg.tool_calls.is_empty(),
            "synthetic assistant carries no tool calls"
        );
        assert_eq!(msg.reasoning_content.as_deref(), None);
    }

    // upgrade_legacy_reasoning: legacy in-memory reconstruction
    // Three legacy on-disk shapes that the on-read upgrader must lift to sibling Reasoning / BackendToolCall items: v1 assistant with `raw_output: Vec<OutputItem>` (backend-search era); v1 assistant with singular `reasoning: ReasoningContent` (earlier distill / chat-completions written as v1); v0 `ChatRequestMessage` with top-level `reasoning_content`.
    // Idempotent (current-format rows produce zero siblings); verified by `upgrade_is_idempotent_on_post_pr_rows`

    #[test]
    fn upgrade_legacy_reasoning_singular_distill_shape() {
        // Synthetic fixture with truncated text and an encrypted stub for readability
        // The assistant row carries `reasoning: { text, encrypted, id }` inline, the earlier shape
        let raw = serde_json::json!({
            "type": "assistant",
            "content": "Web search results for cats and dogs...",
            "reasoning": {
                "text": "The web search results for cats and dogs are mostly about",
                "encrypted": "bIfXFNBiP8EI8F7pkKC1tgbYjvVuIctMAlCUGMii",
                "id": "rs_00000000-0000-4000-8000-000000000001"
            },
            "model_id": "distill",
            "model_fingerprint": "fp_test000000000001"
        });
        let mut seen = std::collections::HashSet::new();
        let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
        assert_eq!(siblings.len(), 1, "exactly one sibling Reasoning emitted");
        let Some(ConversationItem::Reasoning(r)) = siblings.first() else {
            panic!("expected Reasoning sibling, got {siblings:?}");
        };
        assert_eq!(r.id, "rs_00000000-0000-4000-8000-000000000001");
        let [rs::SummaryPart::SummaryText(s)] = r.summary.as_slice() else {
            panic!("expected one summary text: {:?}", r.summary);
        };
        assert_eq!(
            s.text,
            "The web search results for cats and dogs are mostly about"
        );
        assert_eq!(
            r.encrypted_content.as_deref(),
            Some("bIfXFNBiP8EI8F7pkKC1tgbYjvVuIctMAlCUGMii")
        );
    }

    #[test]
    fn upgrade_legacy_reasoning_raw_output_expands_parallel_tco_blobs() {
        // Backend-search-era shape: raw_output preserves the full ordered Vec<OutputItem>
        // The N parallel `tco_*` reasoning items round-trip as N sibling Reasoning items
        let raw = serde_json::json!({
            "type": "assistant",
            "content": "I searched two things in parallel.",
            "tool_calls": [],
            "raw_output": [
                {"type":"reasoning","id":"tco_1","summary":[],"encrypted_content":"enc1"},
                {"type":"web_search_call","id":"ws_1","status":"completed",
                 "action":{"type":"search","query":"q1","sources":[]}},
                {"type":"reasoning","id":"tco_2","summary":[],"encrypted_content":"enc2"},
                {"type":"web_search_call","id":"ws_2","status":"completed",
                 "action":{"type":"search","query":"q2","sources":[]}},
                {"type":"reasoning","id":"rs_main",
                 "summary":[{"type":"summary_text","text":"final synthesis"}]},
                {"type":"message","id":"msg_1","status":"completed","role":"assistant",
                 "content":[{"type":"output_text","text":"I searched two things in parallel.",
                             "annotations":[]}]}
            ]
        });
        let mut seen = std::collections::HashSet::new();
        let siblings = upgrade_legacy_reasoning(&raw, &mut seen);

        // Three Reasoning plus two BackendToolCall items: five siblings
        // Message and FunctionCall (none here) are NOT emitted as siblings.
        assert_eq!(siblings.len(), 5);

        let reasoning_ids: Vec<&str> = siblings
            .iter()
            .filter_map(|s| match s {
                ConversationItem::Reasoning(r) => Some(r.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning_ids,
            vec!["tco_1", "tco_2", "rs_main"],
            "all three reasoning items recovered in emission order — duplicate recovery is \
             structurally impossible here"
        );

        let btc_ids: Vec<&str> = siblings
            .iter()
            .filter_map(|s| match s {
                ConversationItem::BackendToolCall(b) => Some(b.id()),
                _ => None,
            })
            .collect();
        assert_eq!(btc_ids, vec!["ws_1", "ws_2"]);

        // Both web-search ids registered for downstream dedup.
        assert!(seen.contains("ws_1"));
        assert!(seen.contains("ws_2"));
    }

    #[test]
    fn upgrade_legacy_reasoning_dedupes_backend_tool_calls_seen_as_siblings() {
        // BackendToolCall was already a sibling in legacy rows
        // The same call can appear *both* as its own JSONL row *and* inside the following assistant's raw_output
        // The upgrader must not emit a duplicate
        let mut seen = std::collections::HashSet::new();
        seen.insert("ws_already_a_sibling".to_string());

        let raw = serde_json::json!({
            "type": "assistant",
            "content": "",
            "raw_output": [
                {"type":"web_search_call","id":"ws_already_a_sibling","status":"completed",
                 "action":{"type":"search","query":"x","sources":[]}},
                {"type":"web_search_call","id":"ws_new","status":"completed",
                 "action":{"type":"search","query":"y","sources":[]}}
            ]
        });
        let siblings = upgrade_legacy_reasoning(&raw, &mut seen);
        let btc_ids: Vec<&str> = siblings
            .iter()
            .filter_map(|s| match s {
                ConversationItem::BackendToolCall(b) => Some(b.id()),
                _ => None,
            })
            .collect();
        assert_eq!(
            btc_ids,
            vec!["ws_new"],
            "the already-sibling call is skipped; only the new one is emitted"
        );
    }

    #[test]
    fn upgrade_is_idempotent_on_post_pr_rows() {
        // Current-format assistant has neither `reasoning` nor `raw_output`; the upgrader must produce zero siblings
        // Re-running the load path then doesn't accumulate duplicates
        let raw = serde_json::json!({
            "type": "assistant",
            "content": "answer",
            "model_id": "distill"
        });
        let mut seen = std::collections::HashSet::new();
        assert!(upgrade_legacy_reasoning(&raw, &mut seen).is_empty());

        // Standalone sibling rows are also no-ops.
        let r = serde_json::json!({"type":"reasoning","id":"rs_1","summary":[]});
        assert!(upgrade_legacy_reasoning(&r, &mut seen).is_empty());

        let u = serde_json::json!({"type":"user","content":[{"type":"text","text":"hi"}]});
        assert!(upgrade_legacy_reasoning(&u, &mut seen).is_empty());

        let s = serde_json::json!({"type":"system","content":"prompt"});
        assert!(upgrade_legacy_reasoning(&s, &mut seen).is_empty());
    }

    #[test]
    fn upgrade_skips_assistant_with_empty_reasoning() {
        // Assistant with `reasoning: {}` (no text / encrypted / id): no sibling is emitted; there's nothing to preserve
        let raw = serde_json::json!({
            "type": "assistant",
            "content": "answer",
            "reasoning": {}
        });
        let mut seen = std::collections::HashSet::new();
        assert!(upgrade_legacy_reasoning(&raw, &mut seen).is_empty());
    }

    /// INVARIANT: prefix stability across turns without reasoning.
    /// Context and instructions must be prefixed once and consistently across requests.
    #[test]
    fn prefix_stable_across_turns_no_reasoning() {
        let turn1_items = vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Hello"),
        ];
        let req1 = ConversationRequest::from_items(turn1_items.clone());

        let mut turn2_items = turn1_items.clone();
        turn2_items.push(ConversationItem::assistant("Hi there!"));
        turn2_items.push(ConversationItem::user("How are you?"));
        let req2 = ConversationRequest::from_items(turn2_items.clone());

        assert_prefix_stable(&req1, &req2);

        let mut turn3_items = turn2_items.clone();
        turn3_items.push(ConversationItem::assistant("I'm well!"));
        turn3_items.push(ConversationItem::tool_result("tc1", "x"));
        turn3_items.push(ConversationItem::user("Great"));
        let req3 = ConversationRequest::from_items(turn3_items);

        assert_prefix_stable(&req2, &req3);
    }

    /// Prefix stability when turns carry Reasoning siblings with encrypted_content.
    /// Encrypted reasoning is the cache-sensitive payload.
    /// Its serialized position must be byte-identical across turns or the server-side prefix cache misses.
    #[test]
    fn prefix_stable_with_reasoning_siblings() {
        let turn1 = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("u1"),
        ];
        let req1 = ConversationRequest::from_items(turn1.clone());

        let mut turn2 = turn1.clone();
        turn2.push(reasoning_sibling("r1", "thinking 1", Some("enc1")));
        turn2.push(ConversationItem::assistant("response 1"));
        turn2.push(ConversationItem::user("u2"));
        let req2 = ConversationRequest::from_items(turn2.clone());

        assert_prefix_stable(&req1, &req2);

        let mut turn3 = turn2.clone();
        turn3.push(reasoning_sibling("r2", "thinking 2", Some("enc2")));
        turn3.push(ConversationItem::assistant("response 2"));
        turn3.push(ConversationItem::user("u3"));
        let req3 = ConversationRequest::from_items(turn3);

        assert_prefix_stable(&req2, &req3);
    }

    /// Canary for `serde_json`'s `preserve_order` feature.
    /// Without it, `BTreeMap` is used which alphabetizes keys.
    /// This test serializes once and verifies that a known field ordering matches the struct declaration order (not alphabetical).
    #[test]
    fn serialization_determinism() {
        let req = ConversationRequest::from_items(vec![
            ConversationItem::system("You are a helpful assistant."),
            ConversationItem::user("Hello, how are you?"),
            reasoning_sibling("r1", "think", Some("enc")),
            ConversationItem::assistant("I'm well!"),
            ConversationItem::user("Tell me more."),
        ]);

        // Deterministic: the same input gives the same bytes
        let body1 = serde_json::to_string(&input_items_json(&req)).unwrap();
        let body2 = serde_json::to_string(&input_items_json(&req)).unwrap();
        assert_eq!(body1, body2, "repeated serialization must be identical");

        // Insertion-order preservation for an EasyInputMessage with serde tag = "type" (renamed to snake_case)
        // The wire JSON must emit `type` before `role` before `content`
        // With BTreeMap (no preserve_order) these would be alphabetized to content, role, type
        let input = input_items_json(&req);
        let Some(first_item) = input.first() else {
            panic!("expected input item: {input:?}");
        };
        let first_item_str = serde_json::to_string(first_item).unwrap();
        let type_pos = first_item_str
            .find("\"type\"")
            .expect("type field must exist");
        let role_pos = first_item_str
            .find("\"role\"")
            .expect("role field must exist");
        let content_pos = first_item_str
            .find("\"content\"")
            .expect("content field must exist");
        assert!(
            type_pos < role_pos && role_pos < content_pos,
            "preserve_order must maintain struct declaration order \
             (type < role < content), got type@{type_pos} role@{role_pos} \
             content@{content_pos}. Is `preserve_order` enabled in Cargo.toml?"
        );
    }

    /// Reasoning sibling WITHOUT `encrypted_content` (e.g. synthesized from Chat Completions plaintext `reasoning_content`) still round-trips inline.
    /// There is no "fast path" or "slow path", just typed serialization.
    #[test]
    fn reasoning_without_encrypted_content_round_trips_inline() {
        let req = ConversationRequest::from_items(vec![
            ConversationItem::system("sys"),
            ConversationItem::user("u1"),
            reasoning_sibling("r1", "I think about this...", None),
            ConversationItem::assistant("response text"),
        ]);

        let input = input_items_json(&req);
        let summary = summarise_input(&input);

        assert_eq!(summary.len(), 4);
        assert_eq!(summary.get(2).map(String::as_str), Some("reasoning:r1"));

        // Encrypted content absent on the wire.
        let enc = input.get(2).and_then(|v| v.get("encrypted_content"));
        assert!(enc.is_none() || enc.and_then(|v| v.as_str()).is_none());

        // The legacy placeholder sentinel must not appear
        let body_str = serde_json::to_string(&input).unwrap();
        assert!(!body_str.contains("__RAW_OUTPUT_PLACEHOLDER_"));
    }
}
