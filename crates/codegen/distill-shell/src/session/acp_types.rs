// Modified for Distill by Samuel Fajreldines, 2026.
//! Public wire types (DTOs) for the ACP session actor.
//!
//! These are the request/response structs exchanged between the agent layer and the session actor.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::session::persistence::Summary;
use crate::util::config::DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT;

// ── Session list ───────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionListRequest {
    pub workspace_directory: PathBuf,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct AllSessionOverviewRequest {}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct SessionListResponse {
    pub session_summaries: Vec<Summary>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct AllSessionOverviewResponse {
    pub all_sessions: BTreeMap<PathBuf, Vec<Summary>>,
}

// ── Compaction ──────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactConversationRequest {
    #[serde(alias = "sessionId")]
    pub session_id: String,
    #[serde(default, alias = "userContext")]
    pub user_context: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactConversationResponse {}

// ── Feedback ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedbackRequest {
    pub session_id: String,
    #[serde(default)]
    pub turn_number: Option<u64>,
    pub feedback_text: String,
}

/// Request to dismiss a feedback request (sent to the feedback backend).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct FeedbackRequestDismiss {
    pub session_id: String,
    pub request_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FeedbackOutcome {
    Submitted,
    SubmittedCleanupFailed,
    LocalOnly,
    OutcomeUnknown,
    /// Unknown wire variant from a newer shell. Treat like [`Self::OutcomeUnknown`]:
    /// do not claim a definite failure or invite a resend.
    #[serde(other)]
    Other,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackResponse {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<FeedbackOutcome>,
    /// Single-use capability returned only for a successful, explicitly consented modal report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_upload_token: Option<String>,
}

/// `turn_number` is optional from the client side.
/// Per-turn UIs (e.g. the thumbs button on a specific assistant message in the desktop chat history) may attach it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ClientFeedbackInput {
    pub session_id: String,

    pub client_type: prod_mc_cli_chat_proxy_types::feedback_types::ClientType,

    #[serde(default)]
    pub rating_type: Option<prod_mc_cli_chat_proxy_types::feedback_types::RatingType>,

    /// Rating value (interpretation depends on rating_type).
    /// thumbs: -1 (down), 0 (neutral), 1 (up).
    /// Values are clamped to valid ranges on the agent side.
    #[serde(default)]
    pub rating_value: Option<i32>,

    #[serde(default)]
    pub feedback_text: Option<String>,

    #[serde(default)]
    pub images: Vec<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackImage>,

    /// Feedback categories (e.g., ["accuracy", "speed", "helpfulness"])
    #[serde(default)]
    pub feedback_categories: Vec<String>,

    #[serde(default)]
    pub context_type: Option<prod_mc_cli_chat_proxy_types::feedback_types::ContextType>,

    /// 0-based turn number this feedback is about.
    #[serde(default, alias = "turnNumber")]
    pub turn_number: Option<i64>,

    /// Feedback request ID: if present, this is a response to a FeedbackRequestNotification (i.e., solicited feedback).
    /// If absent, this is spontaneous user feedback.
    #[serde(default)]
    pub request_id: Option<String>,

    #[serde(default)]
    pub client_version: Option<String>,

    #[serde(default)]
    pub metadata: Option<serde_json::Value>,

    #[serde(default)]
    pub terminal_info: Option<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackTerminalInfo>,

    /// Requests a shell-issued one-shot trace capability after this feedback is accepted.
    #[serde(default, alias = "requestTraceUploadToken")]
    pub request_trace_upload_token: bool,
}

impl ClientFeedbackInput {
    fn clamp_rating_value(
        rating_type: Option<prod_mc_cli_chat_proxy_types::feedback_types::RatingType>,
        rating_value: Option<i32>,
    ) -> Option<i32> {
        use prod_mc_cli_chat_proxy_types::feedback_types::RatingType;

        match (rating_type, rating_value) {
            (Some(RatingType::Thumbs), Some(v)) => Some(v.clamp(-1, 1)),
            (Some(RatingType::Stars), Some(v)) => Some(v.clamp(1, 5)),
            (Some(RatingType::Nps), Some(v)) => Some(v.clamp(0, 10)),
            // No rating type specified, pass through (will be validated by server)
            (None, Some(v)) => Some(v),
            (_, None) => None,
        }
    }

    /// Convert to a FeedbackSubmission for sending to the feedback backend.
    /// `user_id` is absent here; the backend extracts it from the auth token.
    /// `&mut self`: drains `images` into the submission instead of cloning megabytes of base64; the input is not read for images afterwards.
    pub(crate) fn take_submission(
        &mut self,
        model_id: Option<String>,
        resolved_model_id: Option<String>,
        model_fingerprint: Option<String>,
        turn_number: Option<i64>,
    ) -> prod_mc_cli_chat_proxy_types::feedback_types::FeedbackSubmission {
        use prod_mc_cli_chat_proxy_types::feedback_types::FeedbackContent;

        let clamped_rating_value = Self::clamp_rating_value(self.rating_type, self.rating_value);
        let content = match (
            self.rating_type,
            clamped_rating_value,
            self.feedback_text.clone(),
        ) {
            (Some(rating_type), Some(rating_value), Some(text)) => {
                FeedbackContent::RatingWithText {
                    rating_type,
                    rating_value,
                    text,
                }
            }
            (Some(rating_type), Some(rating_value), None) => FeedbackContent::Rating {
                rating_type,
                rating_value,
            },
            // Fallback: any other shape becomes Text (empty string preserved).
            (_, _, text) => FeedbackContent::Text(text.unwrap_or_default()),
        };

        let mut s = crate::session::feedback_manager::new_submission(
            self.session_id.clone(),
            self.client_type,
            content,
        );
        s.turn_number = turn_number;
        s.images = std::mem::take(&mut self.images);
        s.feedback_categories = self.feedback_categories.clone();
        s.model_id = model_id;
        s.resolved_model_id = resolved_model_id;
        s.model_fingerprint = model_fingerprint;
        s.context_type = self.context_type;
        s.request_id = self.request_id.clone();
        s.client_version = self.client_version.clone();
        s.metadata = self.metadata.clone();
        s.terminal_info = self.terminal_info.clone();
        s
    }

    pub(crate) fn is_solicited(&self) -> bool {
        self.request_id.is_some()
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

/// `x.ai/feedback/drafts/update` params, built by the pager and parsed by the shell. The full body
/// is required so a partial update fails the parse instead of half-updating the draft.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FeedbackDraftUpdateRequest {
    pub session_id: String,
    pub draft_id: distill_feedback::FeedbackDraftId,
    #[serde(flatten)]
    pub input: distill_feedback::FeedbackDraftInput,
}

/// The `draft_id` variant of `x.ai/feedback` params, built by the pager and parsed by the shell.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedbackDraftSendRequest {
    pub session_id: String,
    pub draft_id: distill_feedback::FeedbackDraftId,
    #[serde(default)]
    pub request_trace_upload_token: bool,
    pub edited_body: FeedbackDraftEditedBody,
}

/// `edited_body` of [`FeedbackDraftSendRequest`]: the edited draft plus the pager's client context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FeedbackDraftEditedBody {
    #[serde(flatten)]
    pub input: distill_feedback::FeedbackDraftInput,
    #[serde(default)]
    pub images: Vec<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_info: Option<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackTerminalInfo>,
}

/// Pager attestation carried on the one-shot `x.ai/feedback/upload-trace` request. Deliberately no
/// catch-all variant: an unknown intent fails the request instead of changing its gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTraceUploadIntent {
    SendThisSession,
}

// ── Rollout survey ──────────────────────────────────────────────────────

/// Request to submit rollout survey responses about worktree improvements
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RolloutSurveyRequest {
    pub session_id: String,
    pub preferences: Vec<String>,
    pub feedback: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct RolloutSurveyResponse {
    pub success: bool,
}

// ── Citations / comments ────────────────────────────────────────────────

/// A reference to a range of lines in a file.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
}

/// Request to record an inline comment on a prompt turn.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommentRequest {
    pub session_id: String,
    /// 0-indexed prompt turn this comment is associated with
    pub prompt_index: u32,
    pub comment: String,
    pub citation: Citation,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommentResponse {
    pub comment_id: String,
    pub recorded: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommentDeleteRequest {
    pub session_id: String,
    pub comment_id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommentDeleteResponse {
    pub comment_id: String,
    pub deleted: bool,
}

// ── Rewind ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewindMode {
    /// Roll back both conversation and files (full time-travel).
    All,
    /// Roll back conversation only; leave files untouched.
    /// Use when the agent went in the wrong direction but the code is fine.
    ConversationOnly,
    /// Roll back files only; leave conversation untouched.
    /// Use when the files went wrong but the conversation context is valuable.
    #[serde(alias = "code_only")]
    FilesOnly,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindRequest {
    /// Target prompt index to rewind to (0-based).
    /// Rewinding to N restores the state from before prompt N ran; prompts 0..N-1 are kept.
    pub target_prompt_index: usize,
    /// Whether to force rewind even with conflicts
    pub force: bool,
    /// Clients must specify this explicitly.
    /// Defaults to `All` for backwards compatibility with older clients.
    #[serde(default = "default_rewind_mode")]
    pub mode: RewindMode,
}

pub(crate) fn default_rewind_mode() -> RewindMode {
    RewindMode::All
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindResponse {
    pub success: bool,
    pub target_prompt_index: usize,
    pub mode: RewindMode,
    /// List of file paths that were reverted (only populated on success with All or FilesOnly)
    pub reverted_files: Vec<String>,
    /// List of file paths that can be cleanly reverted (no conflicts)
    #[serde(default)]
    pub clean_files: Vec<String>,
    /// List of conflicts that were encountered (when `force` is false and conflicts exist, `success` is false)
    pub conflicts: Vec<RewindConflictInfo>,
    /// The original prompt text at target_prompt_index, for pre-filling the input field.
    /// Populated on successful conversation rewind (All or ConversationOnly).
    #[serde(default)]
    pub prompt_text: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindConflictInfo {
    pub path: String,
    pub conflict_type: String, // "missing_file", "extra_file", "content_mismatch"
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindPointsRequest {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindPointsResponse {
    pub rewind_points: Vec<RewindPointInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RewindPointInfo {
    pub prompt_index: usize,
    pub created_at: String,
    pub num_file_snapshots: usize,
    /// Whether this prompt has file snapshots that can be reverted.
    /// When false, only conversation rewind is available for this checkpoint.
    #[serde(default)]
    pub has_file_changes: bool,
    /// Preview of the user prompt text (truncated)
    #[serde(default)]
    pub prompt_preview: Option<String>,
}

// ── Session info ────────────────────────────────────────────────────────

/// Itemized token usage for one context category, shown as an informational row in `/context`, e.g. the skills listing or the MCP server listing.
/// Token counts come from rendering the current state (the skill set, the connected servers), never from parsing conversation text.
/// Once injected, these rows overlap [`ContextInfo::message_tokens`]; a fresh session can show rows before the reminders are injected.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TokenUsageCategory {
    /// Display label, e.g. `"Skills"` or `"MCP servers"`.
    pub label: String,
    /// Estimated tokens this category costs in context.
    pub tokens: u64,
    /// By convention a count followed by a noun, e.g. `"21 skills"`; the pager right-aligns the leading count across rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl TokenUsageCategory {
    /// `text` is the canonical render from `SkillManager::listing_snapshot`.
    pub fn skills_listing(text: &str, skill_count: usize) -> Self {
        Self {
            label: "Skills".to_string(),
            tokens: distill_token_estimation::estimate_tokens(text),
            detail: Some(count_detail(skill_count as u64, "skill")),
        }
    }

    /// `text` is the canonical model-facing catalog render.
    pub fn workflows_listing(text: &str, workflow_count: usize) -> Self {
        Self {
            label: "Workflows".to_string(),
            tokens: distill_token_estimation::estimate_tokens(text),
            detail: Some(count_detail(workflow_count as u64, "workflow")),
        }
    }

    /// `text` is the full reminder body for the current server set.
    pub fn mcp_servers(text: &str, server_count: usize) -> Self {
        Self {
            label: "MCP servers".to_string(),
            tokens: distill_token_estimation::estimate_tokens(text),
            detail: Some(count_detail(server_count as u64, "server")),
        }
    }

    /// `text` is the rendered section from `Agent::agents_md_section`.
    pub fn agents_md(text: &str, file_count: usize) -> Self {
        Self {
            label: "AGENTS.md".to_string(),
            tokens: distill_token_estimation::estimate_tokens(text),
            detail: Some(count_detail(file_count as u64, "file")),
        }
    }
}

/// Formats a count with a naively pluralized noun: `"1 skill"`, `"21 skills"`.
pub fn count_detail(count: u64, noun: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{count} {noun}{suffix}")
}

/// Context usage breakdown for session info.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ContextInfo {
    pub used: u64,
    pub total: u64,
    pub system_prompt_tokens: u64,
    pub tool_definitions_count: u64,
    pub tool_definitions_tokens: u64,
    pub compaction_count: u64,
    pub turn_count: u64,
    pub tool_call_count: u64,
    /// Total conversation items (system + user + assistant + tool responses).
    pub message_count: u64,
    /// Bytes/4 estimate of all non-system conversation items.
    pub message_tokens: u64,
    pub free_tokens: u64,
    pub usage_pct: u8,
    /// The resolved auto-compact threshold percent (0-100) for the active model at the time this snapshot was captured.
    /// Comes from the 6-tier resolution (env > user per-model > user global > GB per-model > GB global > 85).
    /// Used by the TUI `/context` view so the displayed “Auto-compact at X%” matches the actual trigger (e.g. 65 for Distill in remote settings).
    #[serde(default = "default_auto_compact_threshold")]
    pub auto_compact_threshold_percent: u8,
    /// Itemized usage rows (skills, workflows, MCP servers, AGENTS.md).
    /// Empty on partial snapshots.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub usage_categories: Vec<TokenUsageCategory>,
}

impl ContextInfo {
    /// Partial snapshot from a notification carrying only used and total.
    /// Breakdown fields default to zero until the next full ContextInfo update.
    pub fn from_notification(used: u64, total: u64) -> Self {
        Self {
            used,
            total,
            usage_pct: distill_token_estimation::usage_percentage_u8(used, total),
            free_tokens: distill_token_estimation::free_tokens(total, used),
            auto_compact_threshold_percent: DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT,
            ..Self::default()
        }
    }
}

/// Serde default for the threshold field (keeps old snapshots and partials deserializing without error and gives the historical default of 85).
fn default_auto_compact_threshold() -> u8 {
    DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT
}

/// Unified session info data returned by GetSessionInfo.
/// One query, all the fields needed for /session-info and /context.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoData {
    /// Agent definition name for this session (e.g. `Distill`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_display_name: Option<String>,
    pub resolved_model_id: Option<String>,
    pub model_fingerprint: Option<String>,
    /// Catalog opt-in to display checkpoint identity (the served fingerprint and the resolved model ID) for this model.
    /// Sole control: the client keeps no built-in per-slug default, so turning this off in the catalog hides both.
    #[serde(default)]
    pub show_model_fingerprint: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    /// Gateway chat conversation id when this session is gateway-proxied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub turns: u64,
    /// Current turn (0-based).
    /// Matches the `turn_number` used in TurnStarted events, traces, and rewinds.
    #[serde(default)]
    pub turn_index: u64,
    pub context: ContextInfo,
}

pub fn model_display_name(
    name: Option<&str>,
    model: &str,
    resolved: Option<&str>,
    show_resolved: bool,
) -> String {
    // If the catalogue entry has a name, that's the displayed model.
    if let Some(n) = name {
        return n.to_string();
    }

    // For displaying the resolved model slug from the API response.
    if show_resolved {
        return match resolved.filter(|r| *r != model) {
            Some(r) => format!("{model} ({r})"),
            None => model.to_string(),
        };
    }

    model.to_string()
}

/// Full wire response for `x.ai/session/info`.
///
/// Wraps `SessionInfoData` with session-level fields (`session_id`, `cwd`) that come from the agent layer rather than the session actor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfoResponse {
    pub session_id: String,
    pub cwd: String,
    #[serde(flatten)]
    pub data: SessionInfoData,
}

// ── Feedback context ────────────────────────────────────────────────────

/// Context gathered from a session to enrich feedback notifications.
///
/// Uses the shared feedback wire types directly so consumers can assign fields to `FeedbackSubmission` without mapping.
#[derive(Debug, Clone, Default)]
pub struct FeedbackContext {
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
    pub tool_outcomes: Vec<prod_mc_cli_chat_proxy_types::feedback_types::FeedbackToolOutcome>,
    pub compaction_count: i64,
    pub context_window_usage: u8,
    pub context_tokens_used: u64,
    pub context_window_tokens: u64,
    pub session_cwd: String,
    pub reasoning_effort: Option<crate::sampling::ReasoningEffort>,
    pub model_id: Option<String>,
    pub model_fingerprint: Option<String>,
}

// ── Startup hints ───────────────────────────────────────────────────────

// `pub` (not `pub(crate)`): carried by the public `SessionCommand` enum (`UpdateAttachPolicy`), whose fields are reachable at `pub`
// A `pub(crate)` field type there trips the `private_interfaces` lint
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupHints {
    #[serde(default)]
    pub non_interactive: bool,
    #[serde(default)]
    pub skip_git_status: bool,
    /// Leading conversation items to preserve verbatim across compaction (the immutable head).
    /// A fresh subagent's head is its spawn-injected items; a `resume_from` subagent's is just the System head so the resumed body stays compactable.
    #[serde(default)]
    pub inherited_prefix_len: Option<usize>,
    /// When true, this session is a subagent child and its prompts should not be appended to the per-CWD prompt_history.jsonl file.
    #[serde(default)]
    pub is_subagent: bool,
    /// Parent session id when this session is a subagent child.
    /// Emitted as `parent_agent_id` on the turn span for trace attribution.
    #[serde(default)]
    pub parent_session_id: Option<String>,
    /// The task's `subagent_type` when this session is a subagent child, put on hook payloads for attribution.
    /// It matches the `SubagentStart`/`SubagentStop` events the parent emits, which also key off the task type, not the resolved agent name.
    #[serde(default)]
    pub subagent_type: Option<String>,
    /// Set on a fork spawn so `install_system_prompt` does NOT overwrite the inherited System at `conversation[0]`.
    /// The verbatim parent copy already holds the parent's System, and overwriting it would bust the cache prefix.
    #[serde(default)]
    pub preserve_inherited_system: bool,
    /// Tool names the session delivers its reply through (e.g. a messaging MCP tool); listing any keeps the full MCP waits at the prefix and tool-definition gates instead of the short startup grace.
    #[serde(default)]
    pub delivery_tools: Vec<String>,
    /// Only `"alwaysAllow"` is honored: would-be prompts resolve as allow at the manager's dispatch gate.
    /// Clamped off by the managed always-approve pin; a configured `defaultMode` wins.
    /// Unlike `yoloMode` / `autoMode`, a warm re-attach to an already-resident actor does NOT re-apply it.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// A subagent model supplied by the caller is authoritative for the
    /// lifetime of the child. This is transient spawn policy, not client
    /// input, so it is never read from an attach payload or persisted here.
    #[serde(skip)]
    pub explicit_model_override: bool,
    /// A subagent effort supplied by the caller is authoritative for the
    /// lifetime of the child. `auto` deliberately leaves this false so the
    /// child may still use the per-round effort chooser on its pinned model.
    #[serde(skip)]
    pub explicit_effort_override: bool,
    #[serde(skip)]
    pub startup_traceparent: std::cell::RefCell<Option<String>>,
}

impl StartupHints {
    /// Shared by the spawn path and the resident re-attach path so both resolve identically.
    pub(crate) fn resolve_mcp_strategy(&self) -> distill_telemetry::enums::McpInitStrategy {
        use distill_telemetry::enums::McpInitStrategy;
        match std::env::var("MCP_INIT_STRATEGY") {
            Ok(v) if !v.trim().is_empty() => McpInitStrategy::from(v),
            _ if self.non_interactive => McpInitStrategy::Blocking,
            _ => McpInitStrategy::Progressive,
        }
    }

    pub(crate) fn take_mcp_reroot_traceparent(&self) -> Option<String> {
        if self.is_subagent {
            return None;
        }
        self.startup_traceparent.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_mcp_reroot_traceparent_one_shot_and_skips_subagent() {
        let hints = StartupHints {
            startup_traceparent: std::cell::RefCell::new(Some("tp".to_owned())),
            ..Default::default()
        };
        assert_eq!(hints.take_mcp_reroot_traceparent().as_deref(), Some("tp"));
        assert_eq!(hints.take_mcp_reroot_traceparent(), None);

        let subagent = StartupHints {
            is_subagent: true,
            startup_traceparent: std::cell::RefCell::new(Some("tp".to_owned())),
            ..Default::default()
        };
        assert_eq!(subagent.take_mcp_reroot_traceparent(), None);
    }

    #[test]
    fn unknown_feedback_outcome_deserializes_as_unknown() {
        let response: FeedbackResponse = serde_json::from_value(serde_json::json!({
            "success": false,
            "outcome": "submitted_after_retry",
        }))
        .expect("newer outcome should remain backward compatible");

        assert_eq!(response.outcome, Some(FeedbackOutcome::Other));
    }

    #[test]
    fn desktop_client_type_deserializes_and_round_trips() {
        let json = r#"{
            "session_id": "sess-1",
            "client_type": "desktop",
            "rating_type": "thumbs",
            "rating_value": 1,
            "feedback_text": "great session",
            "feedback_categories": ["accuracy"]
        }"#;

        let mut input: ClientFeedbackInput = serde_json::from_str(json).unwrap();
        assert_eq!(
            input.client_type,
            prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Desktop
        );
        assert_eq!(input.session_id, "sess-1");

        let submission = input.take_submission(Some("grok-3".into()), None, None, Some(5));
        assert_eq!(
            submission.client_type,
            prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Desktop
        );
        assert_eq!(submission.client_type.to_string(), "desktop");
    }

    /// The agent uses `turn_number` to attach that turn's user/assistant text instead of the latest.
    #[test]
    fn turn_number_deserializes_from_snake_and_camel_case() {
        let snake = r#"{
            "session_id": "sess-1",
            "client_type": "desktop",
            "turn_number": 3
        }"#;
        let snake_input: ClientFeedbackInput = serde_json::from_str(snake).unwrap();
        assert_eq!(snake_input.turn_number, Some(3));

        let camel = r#"{
            "session_id": "sess-1",
            "client_type": "desktop",
            "turnNumber": 7
        }"#;
        let camel_input: ClientFeedbackInput = serde_json::from_str(camel).unwrap();
        assert_eq!(camel_input.turn_number, Some(7));

        let absent = r#"{
            "session_id": "sess-1",
            "client_type": "desktop"
        }"#;
        let absent_input: ClientFeedbackInput = serde_json::from_str(absent).unwrap();
        assert_eq!(absent_input.turn_number, None);
    }

    use serde_json::json;

    // ── RewindMode serialization ──────────────────────────────────────

    #[test]
    fn rewind_mode_serializes_to_snake_case() {
        assert_eq!(serde_json::to_value(RewindMode::All).unwrap(), json!("all"));
        assert_eq!(
            serde_json::to_value(RewindMode::ConversationOnly).unwrap(),
            json!("conversation_only")
        );
        assert_eq!(
            serde_json::to_value(RewindMode::FilesOnly).unwrap(),
            json!("files_only")
        );
    }

    #[test]
    fn rewind_mode_deserializes_from_snake_case() {
        assert_eq!(
            serde_json::from_value::<RewindMode>(json!("all")).unwrap(),
            RewindMode::All
        );
        assert_eq!(
            serde_json::from_value::<RewindMode>(json!("conversation_only")).unwrap(),
            RewindMode::ConversationOnly
        );
        assert_eq!(
            serde_json::from_value::<RewindMode>(json!("files_only")).unwrap(),
            RewindMode::FilesOnly
        );
        // Backwards-compat alias: "code_only" still deserializes to FilesOnly
        assert_eq!(
            serde_json::from_value::<RewindMode>(json!("code_only")).unwrap(),
            RewindMode::FilesOnly
        );
    }

    #[test]
    fn rewind_mode_default_is_all() {
        assert_eq!(default_rewind_mode(), RewindMode::All);
    }

    #[test]
    fn rewind_mode_rejects_unknown_variant() {
        assert!(serde_json::from_value::<RewindMode>(json!("code_only_v2")).is_err());
    }

    // ── RewindRequest backwards compatibility ─────────────────────────

    #[test]
    fn rewind_request_missing_mode_defaults_to_all() {
        let req: RewindRequest =
            serde_json::from_value(json!({"target_prompt_index": 2, "force": false})).unwrap();
        assert_eq!(req.mode, RewindMode::All);
        assert_eq!(req.target_prompt_index, 2);
        assert!(!req.force);
    }

    #[test]
    fn rewind_request_explicit_mode_is_respected() {
        let req: RewindRequest = serde_json::from_value(
            json!({"target_prompt_index": 5, "force": true, "mode": "code_only"}),
        )
        .unwrap();
        assert_eq!(req.mode, RewindMode::FilesOnly);
        assert!(req.force);
    }

    #[test]
    fn rewind_request_roundtrip() {
        let original = RewindRequest {
            target_prompt_index: 3,
            force: false,
            mode: RewindMode::ConversationOnly,
        };
        let json = serde_json::to_value(&original).unwrap();
        let decoded: RewindRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.target_prompt_index, 3);
        assert_eq!(decoded.mode, RewindMode::ConversationOnly);
    }

    // ── RewindResponse fields ─────────────────────────────────────────

    #[test]
    fn rewind_response_includes_mode_and_prompt_text() {
        let resp = RewindResponse {
            success: true,
            target_prompt_index: 1,
            mode: RewindMode::ConversationOnly,
            reverted_files: vec![],
            clean_files: vec![],
            conflicts: vec![],
            prompt_text: Some("fix the bug".into()),
            error: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v.get("mode"), Some(&json!("conversation_only")));
        assert_eq!(v.get("prompt_text"), Some(&json!("fix the bug")));
        assert_eq!(v.get("success"), Some(&json!(true)));
    }

    #[test]
    fn rewind_response_prompt_text_null_when_none() {
        let resp = RewindResponse {
            success: true,
            target_prompt_index: 0,
            mode: RewindMode::FilesOnly,
            reverted_files: vec!["src/main.rs".into()],
            clean_files: vec![],
            conflicts: vec![],
            prompt_text: None,
            error: None,
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v.get("prompt_text"), Some(&json!(null)));
        assert_eq!(v.get("reverted_files"), Some(&json!(["src/main.rs"])));
    }

    #[test]
    fn rewind_response_deserialize_with_defaults() {
        let v = json!({
            "success": false,
            "target_prompt_index": 4,
            "mode": "all",
            "reverted_files": [],
            "conflicts": [{"path": "a.rs", "conflict_type": "content_mismatch"}],
            "error": "dirty working tree"
        });
        let resp: RewindResponse = serde_json::from_value(v).unwrap();
        assert!(!resp.success);
        assert_eq!(resp.mode, RewindMode::All);
        assert!(resp.prompt_text.is_none());
        assert!(resp.clean_files.is_empty());
        assert_eq!(resp.conflicts.len(), 1);
        assert_eq!(
            resp.conflicts.first().map(|c| c.path.as_str()),
            Some("a.rs")
        );
    }

    // ── RewindPointInfo.has_file_changes ──────────────────────────────

    #[test]
    fn rewind_point_info_has_file_changes_true() {
        let point = RewindPointInfo {
            prompt_index: 2,
            created_at: "2025-01-01T00:00:00Z".into(),
            num_file_snapshots: 3,
            has_file_changes: true,
            prompt_preview: Some("refactor auth".into()),
        };
        let v = serde_json::to_value(&point).unwrap();
        assert_eq!(v.get("has_file_changes"), Some(&json!(true)));
        assert_eq!(v.get("num_file_snapshots"), Some(&json!(3)));
    }

    #[test]
    fn rewind_point_info_has_file_changes_false_when_no_snapshots() {
        let point = RewindPointInfo {
            prompt_index: 0,
            created_at: "2025-01-01T00:00:00Z".into(),
            num_file_snapshots: 0,
            has_file_changes: false,
            prompt_preview: None,
        };
        let v = serde_json::to_value(&point).unwrap();
        assert_eq!(v.get("has_file_changes"), Some(&json!(false)));
        assert_eq!(v.get("num_file_snapshots"), Some(&json!(0)));
    }

    #[test]
    fn rewind_point_info_has_file_changes_defaults_to_false() {
        let v = json!({
            "prompt_index": 1,
            "created_at": "2025-01-01T00:00:00Z",
            "num_file_snapshots": 5
        });
        let point: RewindPointInfo = serde_json::from_value(v).unwrap();
        assert!(!point.has_file_changes);
        assert_eq!(point.num_file_snapshots, 5);
        assert!(point.prompt_preview.is_none());
    }

    #[test]
    fn context_info_from_notification_computes_derived_fields() {
        let c = ContextInfo::from_notification(50_000, 200_000);
        assert_eq!(c.used, 50_000);
        assert_eq!(c.total, 200_000);
        assert_eq!(c.usage_pct, 25);
        assert_eq!(c.free_tokens, 150_000);
        assert_eq!(c.system_prompt_tokens, 0);
        assert_eq!(c.message_count, 0);
        assert_eq!(c.compaction_count, 0);
    }

    #[test]
    fn context_info_from_notification_zero_total() {
        let c = ContextInfo::from_notification(100, 0);
        assert_eq!(c.usage_pct, 0);
        assert_eq!(c.free_tokens, 0);
    }

    #[test]
    fn usage_categories_tolerate_serde_skew_in_both_directions() {
        // Old agents omit the field entirely: deserialize to empty.
        let from_old_agent: ContextInfo = serde_json::from_str(r#"{"used":1,"total":2}"#).unwrap();
        assert!(from_old_agent.usage_categories.is_empty());

        // Empty vec is skipped on serialize (old clients see no new field).
        let json = serde_json::to_string(&ContextInfo::default()).unwrap();
        assert!(!json.contains("usageCategories"), "{json}");

        // Extra fields from newer agents are ignored, keeping the label renderable
        let row: TokenUsageCategory =
            serde_json::from_str(r#"{"kind":"agents_md","label":"AGENTS.md","tokens":42}"#)
                .unwrap();
        assert_eq!(row.label, "AGENTS.md");

        // Rows round-trip.
        let original = TokenUsageCategory::skills_listing("t", 2);
        let json = serde_json::to_string(&original).unwrap();
        let roundtripped: TokenUsageCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped, original);

        let agents = TokenUsageCategory::agents_md("rules", 1);
        assert_eq!(agents.label, "AGENTS.md");
        assert_eq!(agents.detail.as_deref(), Some("1 file"));
        assert!(agents.tokens > 0);
    }
}
