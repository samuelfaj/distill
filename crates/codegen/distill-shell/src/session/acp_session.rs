// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(clippy::await_holding_refcell_ref)]
#![allow(clippy::arc_with_non_send_sync)]
//! Session actor implementation for the MVP ACP agent.
//!
//! Each session runs as an actor with its own chat history and tool context.
//! The agent owns the client connection and routes commands and events via channels:
//! - Agent to Session: `SessionCommand` (prompt, cancel, shutdown)
//! - Session to Client: `session_notification` via a shared gateway handle
//!
use super::commands::{
    AdvertiseTrigger, ParsedPromptInfo, PromptCompletionKind, PromptTurnOk, PromptTurnResult,
    SessionCommand, TaskWakeAdmission, TaskWakeFallback, ok_end_turn,
};
use super::handle::SessionHandle;
use super::notifications::NotificationSender;
use crate::agent::update_chunk_merge::{BufferingSettings, ReplayBuffer};
use crate::extensions::memory::{MemoryDreamDisposition, MemoryDreamResponse};
use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
use crate::extensions::notification::{
    HookAnnotationKind, RetryState, SessionNotification as XaiSessionNotification,
    is_reauthable_failure,
};
use crate::sampling::error::map_sampling_err_to_acp;
use crate::sampling::types::{ToolCallResponse, ToolDefinition};
use crate::sampling::{
    ContentPart, ConversationItem, ConversationRequest, ConversationResponse, SamplingError,
    SyntheticReason, ToolSpec, conversation_truncate_for_prompt,
};
use crate::session::ClientFsConfig;
use crate::session::feedback_manager::{FeedbackManager, FeedbackManagerConfig};
use crate::session::fs_watch::{self, git_head_dedup_key};
use crate::session::info::Info as SessionInfo;
use crate::session::mcp_servers::McpInitStrategy;
use crate::session::mcp_servers::McpMetaConfigMap;
use crate::session::mcp_servers::McpState;
use crate::session::mcp_servers::OauthInteractivity;
use crate::session::mcp_servers::build_pending_clients;
use crate::session::mcp_servers::mcp_server_name;
use crate::session::mcp_servers::mcp_target_str;
use crate::session::mcp_servers::mcp_transport_str;
use crate::session::mcp_servers::parse_mcp_tool_name;
use crate::session::persistence::{
    PersistenceContentChunk, PersistenceHandle, PersistenceMsg, get_prompt_file_path,
};
use crate::session::plan_mode::PromptMode;
use crate::session::prompt_parser::parse_prompt_with_skills;
use crate::session::replay_events::{SessionEvent, SessionNotification};
use crate::session::result::ExtMethodResult;
use crate::session::signals::{SessionSignalsHandle, TurnDeltaSnapshot};
use crate::session::slash_commands::{self, BuiltinAction, SlashCommandOutcome};
use crate::session::storage::SessionUpdate;
use crate::session::user_message::construct_user_message_minimal;
use crate::session::user_message::extract_user_query;
use crate::terminal::TerminalRunRequest;
use crate::tools::ToolContext;
use agent_client_protocol as acp;
use agent_client_protocol::ContentBlock;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_agent::AgentDefinition;
use distill_agent::prompt::agents_md::LEGACY_AGENTS_MD_REMINDER_PREFIX;
use distill_agent::prompt::skills::SkillsConfig;
use distill_sampler::SamplerConfig as SamplingConfig;
use distill_sampling_types::truncate_bytes;
use distill_tools::computer::local::LocalTerminalBackend;
use distill_tools::implementations::BashToolInput;
use distill_tools::implementations::distill::web_fetch::WebFetchConfig;
use distill_tools::types::ToolInput;
use distill_tools::types::compat::CompatConfig;
use distill_tools::types::output::{
    BashOutput, ReadFileOutput, ToolOutput as ToolsToolOutput, ToolRunResult,
};
use distill_workspace::file_system::CodebaseIndexManager;
use distill_workspace::permission::{
    AccessKind, ClientType, Decision, HookAsk, PermissionEvent, PermissionHandle, PermissionRequest,
};
use distill_workspace::session::file_state::{FileStateHandle, FileStateTracker};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
#[cfg(test)]
use std::sync::OnceLock;
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};
use tokio::time::{Duration, sleep};
const SESSION_LOG: &str = "xai_session";
#[path = "compaction.rs"]
mod compaction;
#[path = "compaction_segments.rs"]
mod compaction_segments;
#[path = "acp_session_impl/types.rs"]
mod types;
pub(crate) use types::*;
pub use types::{TodoGateDecision, TodoGateReason};
#[path = "acp_session_impl/goal.rs"]
mod goal;
#[path = "acp_session_impl/named_workflow_args.rs"]
mod named_workflow_args;
#[path = "acp_session_impl/turn.rs"]
mod turn;
#[path = "acp_session_impl/workflow.rs"]
mod workflow_run;
pub(crate) use turn::SUBAGENT_USAGE_DRAIN;
#[path = "acp_session_impl/tool_layer_images.rs"]
mod tool_layer_images;
use tool_layer_images::*;
#[path = "acp_session_impl/auth_retry.rs"]
mod auth_retry;
pub(crate) use auth_retry::{
    AuthRetryDecision, AuthRetrySchedule, human_duration, pace_uncharged_resubmit,
};
#[path = "acp_session_impl/rate_limit_waits.rs"]
mod rate_limit_waits;
pub(crate) use rate_limit_waits::{
    RateLimitWaitBudget, RateLimitWaitConfig, RateLimitWaitDecision,
};
#[path = "acp_session_impl/active_agent_message_presentation.rs"]
mod active_agent_message_presentation;
use active_agent_message_presentation::*;
#[path = "acp_session_impl/post_tool_use_delivery.rs"]
mod post_tool_use_delivery;
#[path = "acp_session_impl/tool_calls.rs"]
mod tool_calls;
use post_tool_use_delivery::*;
#[path = "acp_session_impl/image_strip.rs"]
mod image_strip;
#[path = "acp_session_impl/interjection.rs"]
mod interjection;
#[path = "acp_session_impl/sampling_events.rs"]
mod sampling_events;
#[path = "acp_session_impl/workflow_write_smoke_check.rs"]
mod workflow_write_smoke_check;
pub(crate) use interjection::*;
#[path = "acp_session_impl/laziness.rs"]
mod laziness;
#[cfg(test)]
pub(crate) use laziness::*;
#[path = "acp_session_impl/queue_mutation.rs"]
mod queue_mutation;
use queue_mutation::{InputOrigin, QueueMutationPolicy};
#[path = "acp_session_impl/prompt_queue.rs"]
mod prompt_queue;
pub(super) use prompt_queue::QueueInputRequest;
#[cfg(test)]
use tool_calls::BridgeToolSuccess;
#[path = "acp_session_impl/mcp.rs"]
mod mcp;
#[path = "acp_session_impl/mcp_init.rs"]
mod mcp_init;
#[path = "acp_session_impl/parent_interject.rs"]
mod parent_interject;
#[path = "acp_session_impl/parent_message.rs"]
mod parent_message;
use mcp_init::*;
#[path = "acp_session_impl/hooks_plugins.rs"]
mod hooks_plugins;
#[path = "acp_session_impl/mcp_failed_reminder.rs"]
mod mcp_failed_reminder;
#[path = "acp_session_impl/model_switch.rs"]
mod model_switch;
#[path = "acp_session_impl/slash_exec.rs"]
mod slash_exec;
use super::PromptOrigin;
use super::chat_persistence;
use super::compaction_config;
use super::memory_state;
use super::telemetry;
#[path = "acp_session_impl/prompt_build.rs"]
mod prompt_build;
use prompt_build::*;
#[path = "acp_session_impl/prompt_offload.rs"]
mod prompt_offload;
#[cfg(test)]
pub(crate) use prompt_offload::LARGE_PROMPT_THRESHOLD;
#[path = "acp_session_impl/session_mode.rs"]
mod session_mode;
use session_mode::*;
#[path = "acp_session_impl/child_tool_projection.rs"]
mod child_tool_projection;
#[path = "acp_session_impl/jev_laziness.rs"]
mod jev_laziness;
#[path = "acp_session_impl/jev_ledger.rs"]
pub(crate) mod jev_ledger;
#[path = "acp_session_impl/jev_memory.rs"]
mod jev_memory;
#[path = "acp_session_impl/jev_routing.rs"]
mod jev_routing;
#[path = "acp_session_impl/jev_tool_result.rs"]
mod jev_tool_result;
#[path = "acp_session_impl/jev_tool_subset.rs"]
mod jev_tool_subset;
#[path = "acp_session_impl/length_salvage.rs"]
mod length_salvage;
#[path = "acp_session_impl/sampler_turn.rs"]
mod sampler_turn;
pub(crate) use jev_routing::*;
#[path = "acp_session_impl/jev_compaction.rs"]
mod jev_compaction;
pub(crate) use jev_compaction::*;
use sampler_turn::*;
#[path = "acp_session_impl/tool_dispatch.rs"]
mod tool_dispatch;
use tool_dispatch::*;
#[path = "acp_session_impl/mcp_snapshot.rs"]
mod mcp_snapshot;
use mcp_snapshot::*;
#[path = "acp_session_impl/turn_task.rs"]
mod turn_task;
use turn_task::*;
#[path = "acp_session_impl/cancel.rs"]
mod cancel;
#[path = "acp_session_impl/reminders.rs"]
mod reminders;
use reminders::*;
pub use reminders::{CollectedTodoGateInput, TodoGateInput, evaluate_todo_gate};
#[path = "acp_session_impl/laziness_classifier.rs"]
mod laziness_classifier;
pub(crate) use laziness_classifier::*;
#[path = "acp_session_impl/notification_drain.rs"]
mod notification_drain;
use notification_drain::*;
#[path = "acp_session_impl/extensions.rs"]
mod extensions;
use extensions::*;
#[path = "acp_session_impl/memory_dream.rs"]
mod memory_dream;
use memory_dream::*;
#[path = "acp_session_impl/goal_support.rs"]
mod goal_support;
#[path = "acp_session_impl/memory_capture.rs"]
mod memory_capture;
#[path = "acp_session_impl/memory_carryover.rs"]
mod memory_carryover;
#[path = "acp_session_impl/memory_control.rs"]
mod memory_control;
#[path = "acp_session_impl/memory_forget.rs"]
mod memory_forget;
#[path = "acp_session_impl/v2_memory_dream.rs"]
mod v2_memory_dream;
pub(crate) use goal_support::*;
#[path = "acp_session_impl/hook_dispatch.rs"]
mod hook_dispatch;
use hook_dispatch::*;
#[path = "acp_session_impl/turn_report_slot.rs"]
mod turn_report_slot;
pub(crate) use turn_report_slot::TurnEpoch;
use turn_report_slot::{CommitOutcome, TurnReportClaim};
#[path = "acp_session_impl/turn_end_hooks.rs"]
mod turn_end_hooks;
use turn_end_hooks::TurnEnd;
#[path = "acp_session_impl/stop_gate.rs"]
mod stop_gate;
pub use stop_gate::MAX_STOP_HOOK_CONTINUATIONS_PER_TURN;
#[path = "acp_session_impl/context_snapshot.rs"]
mod context_snapshot;
#[path = "acp_session_impl/recap.rs"]
mod recap;
#[path = "acp_session_impl/rewind.rs"]
mod rewind;
#[path = "acp_session_impl/session_setup.rs"]
mod session_setup;
#[path = "acp_session_impl/side_call.rs"]
mod side_call;
#[path = "acp_session_impl/status_line.rs"]
pub(crate) mod status_line;
#[path = "acp_session_impl/title_refresh.rs"]
mod title_refresh;
#[path = "acp_session_impl/turn_end.rs"]
mod turn_end;
#[path = "acp_session_impl/turn_summary.rs"]
mod turn_summary;
#[path = "acp_session_impl/updates.rs"]
mod updates;
#[cfg(test)]
#[path = "acp_session_impl/updates_tests.rs"]
mod updates_tests;
pub use recap::SIDE_QUESTION_INSTRUCTION;
#[path = "acp_session_impl/background_tasks.rs"]
mod background_tasks;
#[path = "acp_session_impl/run_loop.rs"]
mod run_loop;
use run_loop::*;
#[path = "acp_session_impl/spawn.rs"]
mod spawn;
use super::acp_types::*;
pub use spawn::SessionThread;
pub(crate) use spawn::*;
/// Client-registered hook gates (the `x.ai/hooks/run` reverse request).
mod hooks;
pub(crate) struct InputItem {
    pub(crate) prompt_id: String,
    pub(crate) prompt_blocks: Vec<ContentBlock>,
    pub(crate) prompt_mode: PromptMode,
    pub(crate) trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
    pub(crate) artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
    /// Optional client identifier from the prompt request meta (overrides session-level one)
    pub(crate) client_identifier: Option<String>,
    /// See [`SessionCommand::Prompt::screen_mode`]. Telemetry-only.
    pub(crate) screen_mode: Option<String>,
    /// See [`SessionCommand::Prompt::verbatim`].
    pub(crate) verbatim: bool,
    pub(crate) json_schema: Option<serde_json::Value>,
    /// Authoritative typed provenance for this input.
    pub(crate) input_origin: InputOrigin,
    /// Typed deferred completion retained while an admitted task wake is queued.
    /// Consumed by an interactive stop if it removes the wake before the turn starts.
    pub(crate) task_wake_fallback: Option<TaskWakeFallback>,
    pub(crate) tool_overrides_update: Option<distill_sampling_types::ToolOverridesUpdate>,
    pub(crate) respond_to: oneshot::Sender<PromptTurnResult>,
    /// Fired after the user message is in chat history and a persistence flush barrier has completed (see `SessionCommand::Prompt::persist_ack`).
    pub(crate) persist_ack: Option<oneshot::Sender<()>>,
    /// Pre-parsed prompt channel. See `SessionCommand::Prompt::parsed_prompt_tx`.
    pub(crate) parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    /// Fires when this exact row is promoted to the running turn.
    /// Dropped on removal so a queued-but-never-started initial child prompt cannot ack.
    pub(crate) initial_child_prompt_ready: Option<oneshot::Sender<oneshot::Sender<()>>>,
    /// Server-authoritative prompt-queue metadata.
    /// `Some` for user-originated prompts (they appear in the shared queue).
    /// `None` for synthetic / system inputs (auto-wake, nudges, notification drains).
    pub(crate) queue_meta: Option<crate::session::prompt_queue::QueueEntryMeta>,
    pub(crate) queue_mutation_policy: QueueMutationPolicy,
    /// Whether this prompt entered via the send-now path (explicit, derived during a blocking wait, or an interjection fallback).
    /// Send-now inserts land behind earlier still-queued send-now prompts, so stacked sends run FIFO.
    /// Sends stack e.g. during a goal turn, which promotes but never cancels.
    pub(crate) send_now: bool,
    /// See [`SessionCommand::Prompt::traceparent`].
    pub(crate) traceparent: Option<String>,
}
use crate::session::commands::{NotificationPriority, NotificationSource};
/// Built by [`SessionActor::resolve_goal_tool_names()`] to avoid duplicating `tool_for_kind()` calls across goal functions.
struct GoalToolNames {
    goal: String,
    task: String,
    todo: String,
}
/// Shared body of the goal-mode system reminder.
/// Used by both `setup_goal` (initial `/goal <objective>`) and `resume_goal` (`/goal resume`).
/// Placeholders are uppercase to avoid collision with the literal `{...}` content in the prompt.
pub(super) const GOAL_TASK_DISCIPLINE_TEMPLATE: &str =
    include_str!("templates/goal_task_discipline.md");
pub(super) const GOAL_RULES_TEMPLATE: &str = include_str!("templates/goal_rules.md");
pub(super) const GOAL_RULES_TEMPLATE_LEGACY: &str = include_str!("templates/goal_rules_legacy.md");
/// Plan-aware preamble folded into the goal-rules block when the planner is enabled and a plan exists.
/// Empty on the legacy path.
const GOAL_PLAN_BLOCK_TEMPLATE: &str = include_str!("templates/goal_plan_block.md");
/// Placeholders are lowercase because the template carries no literal JSON-ish `{...}` content that would collide.
/// (`goal_rules.md` keeps uppercase placeholders because it embeds verbatim user prose that may contain `{...}`.).
/// A `/goal … --budget N` cap IS enforced at the turn-end continuation gate (terminal `BudgetLimited`).
pub(super) const GOAL_CONTINUATION_DIRECTIVE_TEMPLATE: &str =
    include_str!("templates/goal_continuation_directive.md");
pub(super) const GOAL_CONTINUATION_DIRECTIVE_TEMPLATE_LEGACY: &str =
    include_str!("templates/goal_continuation_directive_legacy.md");
/// Compact can run mid-turn (`run_compact_only` / CompactAndResubmit); those
/// callers must not inherit TurnEnd drain, `rounds_since_verify++`, or budget stop.
enum GoalContinuationPurpose {
    TurnEnd,
    Compaction,
}
/// Built continuation directive plus the optional premature-stop pattern that the caller emits when it actually continues.
/// Produced by [`SessionActor::prepare_goal_continuation`].
struct GoalContinuationPlan {
    directive: String,
    stop_pattern: Option<&'static str>,
    /// Recommendation embedded in `directive`, if any.
    /// Handed to `consume_strategist_note` (compare-and-clear) only once the directive is committed for delivery.
    strategy_rec: Option<String>,
}
/// Maximum age of a queue-edit hold before the promoter discards it.
/// Bounds leaked holds after a client crash or dropped `release_edit`.
/// A repeat `hold_edit` inserts a fresh stamp so re-entering edit after a dropped release does not inherit an aged bound.
pub(crate) const EDIT_HOLD_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
fn expire_older_than(holds: &mut HashMap<String, std::time::Instant>, ttl: std::time::Duration) {
    holds.retain(|_, since| since.elapsed() < ttl);
}
#[cfg(test)]
fn backdate_edit_hold(
    holds: &mut HashMap<String, std::time::Instant>,
    id: &str,
    age: std::time::Duration,
) {
    if let Some(since) = holds.get_mut(id) {
        *since = std::time::Instant::now() - age;
    }
}
/// Task scheduling state: the only fields that remain behind `TokioMutex`.
/// All chat state has been fully migrated to `ChatStateActor` via `chat_state_handle`.
/// Credentials (api_key, optional extra access key, client_version) live in the `credentials` sync mutex on `SessionActor`.
pub(crate) struct State {
    pub(crate) running_task: Option<AgentTask>,
    finalization_gate: FinalizationGate,
    pub(crate) message_delivery: parent_message::MessageDeliveryState,
    pub(crate) pending_inputs: VecDeque<InputItem>,
    pub(crate) pending_notifications: Vec<PendingNotification>,
    /// Prompt ids under composer edit, stamped (and re-stamped) by `hold_edit`.
    /// Held followers are skipped by combine; a live held front blocks promote.
    pub(crate) edit_holds: HashMap<String, std::time::Instant>,
    /// When true, notifications are buffered but not drained until genuine user re-engagement.
    /// Set by an interactive stop, cleared by a user prompt.
    pub(crate) notifications_suppressed: bool,
    /// A `UserPromptSubmit` hook blocked the previous prompt.
    /// The promoter must not auto-start the next queued row, so follow-ups never run as if the blocked prompt had succeeded.
    /// All transitions go through [`State::arm_hook_block_hold`] / [`State::take_hook_block_hold`]; read via [`State::hook_block_held`].
    pub(crate) hook_block_hold: HookBlockHold,
    /// Active prompt is still rewindable until the first outbound prompt-scoped event is emitted.
    /// Set at promote, cleared at first output or by the rewind pop itself.
    pub(crate) rewindable: bool,
    /// Whether the running front's user message is recorded where the model will see it.
    /// Transitions go through `mark_front_message_committed`.
    pub(crate) front_message_committed: bool,
    /// Layer-3 LazinessDetector: number of `<system-reminder>` nudges injected so far in this (session, model) pair.
    /// Reset to 0 by the actor's main `select!` loop when its `model_switch_rx` watch channel fires.
    /// See the `model_switch_rx.changed()` arm in `run_session`.
    pub(crate) nudges_used_this_session: u32,
}
/// Queue hold after a prompt-gate block; see [`State::hook_block_hold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HookBlockHold {
    #[default]
    Ready,
    Held,
}
impl State {
    pub(crate) fn arm_hook_block_hold(&mut self) {
        self.hook_block_hold = HookBlockHold::Held;
    }
    /// Clear the hold; returns whether it was held (callers log the release).
    pub(crate) fn take_hook_block_hold(&mut self) -> bool {
        std::mem::take(&mut self.hook_block_hold) == HookBlockHold::Held
    }
    pub(crate) fn hook_block_held(&self) -> bool {
        self.hook_block_hold == HookBlockHold::Held
    }
    pub(crate) fn clear_pending_notifications(&mut self) {
        self.pending_notifications.clear();
    }
    /// Prompt id of the in-flight turn, if any.
    /// `running_task` lives under the same lock as `pending_inputs`, while `handle_completion` clears `current_prompt_id` before taking this lock.
    pub(crate) fn running_prompt_id(&self) -> Option<&str> {
        self.running_task.as_ref().map(|t| t.prompt_id.as_str())
    }
    /// Sweep `pending_inputs` by `drop_if`, but never the running turn's own slot (matched by prompt id, not index 0).
    /// Returned items keep live `respond_to` senders; dropping them unfulfilled is only safe for synthetic items — user-originated matches must be resolved or `session/prompt` hangs.
    /// Deleting the in-flight slot shifts a queued user prompt to index 0, where `cancel_running_task` destroys it and the message is lost.
    pub(crate) fn sweep_pending_inputs(
        &mut self,
        drop_if: impl Fn(&InputItem) -> bool,
    ) -> Vec<InputItem> {
        let running_pid = self.running_task.as_ref().map(|t| t.prompt_id.clone());
        let mut dropped = Vec::new();
        let mut kept = VecDeque::with_capacity(self.pending_inputs.len());
        for item in std::mem::take(&mut self.pending_inputs) {
            if running_pid.as_deref() != Some(item.prompt_id.as_str()) && drop_if(&item) {
                dropped.push(item);
            } else {
                kept.push_back(item);
            }
        }
        self.pending_inputs = kept;
        dropped
    }
}
/// Shared idle predicate for synthetic-turn injection so notification drain and the laziness check cannot drift.
/// True only when no turn is running, no user prompt is queued, and neither an interactive stop nor a hook-block hold is waiting on the user.
/// An injection under that hold would outrun the user's next prompt; idle reporting uses `state_is_busy` instead because an interrupt really is idle.
pub(crate) fn is_session_idle_for_injection(state: &State) -> bool {
    state.running_task.is_none()
        && !state.finalization_gate.is_active()
        && state.pending_inputs.is_empty()
        && !state.notifications_suppressed
        && !state.hook_block_held()
}
pub(crate) fn state_is_busy(state: &State) -> bool {
    state.running_task.is_some()
        || state.finalization_gate.is_active()
        || !state.pending_inputs.is_empty()
}
use distill_login::AuthManager;
#[derive(Clone)]
struct ShellManagedGatewayToolClient {
    proxy_base_url: String,
    auth_manager: Arc<AuthManager>,
}
#[async_trait::async_trait]
impl distill_tools::types::resources::ManagedGatewayToolCaller for ShellManagedGatewayToolClient {
    async fn call_tool(
        &self,
        call_id: &str,
        arguments: serde_json::Value,
        caller: &str,
    ) -> Result<
        distill_tools::types::resources::ManagedGatewayToolCallResponse,
        distill_tool_runtime::ToolError,
    > {
        let auth_key = self
            .auth_manager
            .get_valid_token()
            .await
            .ok()
            .or_else(|| self.auth_manager.current_or_expired().map(|a| a.key))
            .ok_or_else(|| {
                distill_tool_runtime::ToolError::unauthorized("no auth token available")
            })?;
        let response = crate::session::managed_mcp::call_gateway_tool(
            &self.proxy_base_url,
            &auth_key,
            call_id,
            arguments,
        )
        .await
        .map_err(|error| managed_gateway_error_to_tool_error(error, caller))?;
        Ok(
            distill_tools::types::resources::ManagedGatewayToolCallResponse {
                result: response.result,
                connectors_needing_reauth: response.connectors_needing_reauth,
            },
        )
    }
}
fn managed_gateway_error_to_tool_error(
    error: crate::session::managed_mcp::ManagedMcpFetchError,
    caller: &str,
) -> distill_tool_runtime::ToolError {
    match error {
        crate::session::managed_mcp::ManagedMcpFetchError::Status { status, message } => {
            let detail = format!("Managed MCP gateway tool call failed: {message}");
            let mut err = if status == reqwest::StatusCode::UNAUTHORIZED {
                distill_tool_runtime::ToolError::unauthorized(detail)
            } else if status == reqwest::StatusCode::FORBIDDEN {
                distill_tool_runtime::ToolError::permission_denied(detail)
            } else {
                let tool_id = distill_tool_protocol::ToolId::new(caller).unwrap_or_else(|_| {
                    distill_tool_protocol::ToolId::new("use_tool").expect("valid")
                });
                distill_tool_runtime::ToolError::execution(tool_id, detail)
            };
            match err.details.as_mut() {
                Some(serde_json::Value::Object(map)) => {
                    map.insert(
                        HTTP_STATUS_DETAILS_KEY.to_string(),
                        serde_json::json!(status.as_u16()),
                    );
                }
                _ => {
                    err.details = Some(serde_json::json!({
                        HTTP_STATUS_DETAILS_KEY: status.as_u16(),
                    }));
                }
            }
            err
        }
        crate::session::managed_mcp::ManagedMcpFetchError::Transport(e) => {
            distill_tool_runtime::ToolError::network_error(format!(
                "Managed MCP gateway tool call failed: {}",
                e.without_url()
            ))
        }
        crate::session::managed_mcp::ManagedMcpFetchError::NoAuth => {
            distill_tool_runtime::ToolError::unauthorized("no auth token available")
        }
    }
}
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod managed_gateway_error_tests {
    use super::*;
    fn status_error(code: u16, message: &str) -> crate::session::managed_mcp::ManagedMcpFetchError {
        crate::session::managed_mcp::ManagedMcpFetchError::Status {
            status: reqwest::StatusCode::from_u16(code).unwrap(),
            message: message.to_string(),
        }
    }
    #[test]
    fn unauthorized_status_maps_to_unauthorized_and_carries_status() {
        let err = managed_gateway_error_to_tool_error(status_error(401, "expired"), "use_tool");
        assert_eq!(err.kind, distill_tool_runtime::ToolErrorKind::Unauthorized);
        assert!(err.detail.contains("expired"));
        let details = err.details.as_ref().unwrap();
        assert_eq!(
            details.get(HTTP_STATUS_DETAILS_KEY),
            Some(&serde_json::json!(401))
        );
    }
    #[test]
    fn forbidden_status_maps_to_permission_denied_and_carries_status() {
        let err = managed_gateway_error_to_tool_error(status_error(403, "denied"), "use_tool");
        assert_eq!(
            err.kind,
            distill_tool_runtime::ToolErrorKind::PermissionDenied
        );
        let details = err.details.as_ref().unwrap();
        assert_eq!(
            details.get(HTTP_STATUS_DETAILS_KEY),
            Some(&serde_json::json!(403))
        );
    }
    #[test]
    fn general_status_maps_to_execution_with_caller_tool_id() {
        let err = managed_gateway_error_to_tool_error(status_error(500, "boom"), "CallMcpTool");
        assert_eq!(err.kind, distill_tool_runtime::ToolErrorKind::Execution);
        let details = err.details.as_ref().unwrap();
        assert_eq!(
            details.get(HTTP_STATUS_DETAILS_KEY),
            Some(&serde_json::json!(500))
        );
        assert_eq!(
            details.get("tool_id"),
            Some(&serde_json::json!("CallMcpTool"))
        );
    }
    #[test]
    fn general_status_falls_back_to_use_tool_for_unknown_caller() {
        let err = managed_gateway_error_to_tool_error(status_error(500, "boom"), "not a tool id");
        assert_eq!(err.kind, distill_tool_runtime::ToolErrorKind::Execution);
        let details = err.details.as_ref().unwrap();
        assert_eq!(details.get("tool_id"), Some(&serde_json::json!("use_tool")));
    }
    #[test]
    fn no_auth_maps_to_unauthorized() {
        let err = managed_gateway_error_to_tool_error(
            crate::session::managed_mcp::ManagedMcpFetchError::NoAuth,
            "use_tool",
        );
        assert_eq!(err.kind, distill_tool_runtime::ToolErrorKind::Unauthorized);
    }
    #[tokio::test]
    async fn transport_error_maps_to_network_error_without_url() {
        let transport = reqwest::Client::new()
            .post("http://127.0.0.1:1/mcp/tools/call")
            .send()
            .await
            .expect_err("connection to a dead port should fail");
        let err = managed_gateway_error_to_tool_error(
            crate::session::managed_mcp::ManagedMcpFetchError::Transport(transport),
            "use_tool",
        );
        assert_eq!(err.kind, distill_tool_runtime::ToolErrorKind::NetworkError);
        assert!(err.detail.contains("Managed MCP gateway tool call failed"));
        assert!(
            !err.detail.contains("http://"),
            "transport detail must not leak the proxy URL: {}",
            err.detail
        );
    }
}
/// Data carried from prepare_tool_call through dispatch_tool to finalize.
#[derive(Debug, Clone)]
pub(crate) struct PreparedToolCall {
    /// The model's tool call ID (for tool_result matching).
    call_id: String,
    /// ACP-internal tool call ID.
    tool_call_id: acp::ToolCallId,
    /// The tool name as requested by the model.
    tool_name: String,
    /// The raw arguments string (for post_tool_use hook payload).
    raw_arguments: String,
    /// Parsed JSON arguments ready for bridge.call().
    parsed_args: serde_json::Value,
    /// Model ID at time of call.
    model_id: String,
    /// Whether concatenated JSON recovery was used, and how many objects were found.
    concatenated_json_count: usize,
    /// Resolved target for meta-dispatch tools (`use_tool`, `CallMcpTool`); `None` for ordinary tools.
    /// See [`ToolInput::dispatch_target_name`].
    dispatch_target_name: Option<String>,
    /// Read-only per `ToolKind`; decides whether the call takes the per-file lock.
    is_read_only: bool,
    rewriting_hook: Option<String>,
    additional_context: Vec<distill_hooks::dispatcher::AdditionalContext>,
}
impl PreparedToolCall {
    /// The tool name hooks see: the resolved dispatch target, else the wire name.
    /// The single source for the resolved name across the dispatch-phase hook events (PostToolUse / PostToolUseFailure) and their telemetry labels.
    pub(crate) fn hook_tool_name(&self) -> &str {
        self.dispatch_target_name
            .as_deref()
            .unwrap_or(&self.tool_name)
    }
}
#[cfg(test)]
pub(crate) use crate::session::streaming_capture::STREAMING_CAPTURE_MAX_BYTES;
pub(crate) use crate::session::streaming_capture::StreamingTurnCapture;
/// One memoized model's auth state, keyed by model id; see [`SessionActor::model_auth_memo`] for the invalidation contract.
#[derive(Clone)]
pub(crate) struct ModelAuthMemo {
    pub(crate) model_id: String,
    pub(crate) facts: crate::agent::config::ModelAuthFacts,
    pub(crate) provider: Option<distill_login::AuthProviderRef>,
}
pub(crate) struct PendingImageStrip {
    pub(crate) urls: Vec<std::sync::Arc<str>>,
    pub(crate) timed_out: bool,
    pub(crate) applying: bool,
}
pub(crate) struct ImageStripRewriteBarrier {
    gate: std::sync::Arc<tokio::sync::RwLock<()>>,
    strips: std::sync::Arc<tokio::sync::Mutex<()>>,
}
pub(crate) struct ImageStripWriteGuard {
    _gate_guard: tokio::sync::OwnedRwLockReadGuard<()>,
    _strip_guard: tokio::sync::OwnedMutexGuard<()>,
}
impl ImageStripRewriteBarrier {
    pub(crate) fn new() -> Self {
        Self {
            gate: std::sync::Arc::new(tokio::sync::RwLock::new(())),
            strips: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    pub(crate) async fn lock_strip(&self) -> ImageStripWriteGuard {
        let strip_guard = std::sync::Arc::clone(&self.strips).lock_owned().await;
        let gate_guard = std::sync::Arc::clone(&self.gate).read_owned().await;
        ImageStripWriteGuard {
            _strip_guard: strip_guard,
            _gate_guard: gate_guard,
        }
    }
    pub(crate) async fn lock_rewind(&self) -> tokio::sync::OwnedRwLockWriteGuard<()> {
        std::sync::Arc::clone(&self.gate).write_owned().await
    }
}
pub(crate) struct StreamOwnership {
    pub(crate) generation: u64,
    pub(crate) waiter: Option<tokio::sync::oneshot::Sender<()>>,
}
#[cfg(test)]
impl StreamOwnership {
    pub(crate) fn with_waiter(waiter: Option<tokio::sync::oneshot::Sender<()>>) -> Self {
        Self {
            generation: 0,
            waiter,
        }
    }
}
pub(crate) struct StreamApplySpan {
    request_id: distill_sampler::RequestId,
    region: distill_telemetry::region::Region,
    chunk_count: i64,
    bytes: i64,
}
impl StreamApplySpan {
    fn record_and_close(self) {
        self.region.span().record("chunk_count", self.chunk_count);
        self.region.span().record("bytes", self.bytes);
        self.region.close();
    }
}
pub(crate) struct SessionActor {
    pub(crate) repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState,
    pub(crate) session_info: SessionInfo,
    /// Transient turn-retry kill switch, resolved once at spawn; flips apply to new sessions.
    /// Off for subagents in the first release; headless is enforced per turn via `attach_non_interactive`.
    pub(crate) transient_retry_enabled: bool,
    /// Cumulative transient resubmits this prompt.
    /// Prompt-scoped on the actor: auto-recovery, stop-hook continuations, and the goal loop re-enter the turn loop within one prompt.
    /// A loop-local counter would reset the cap (exhaustion itself triggers auto-recovery).
    pub(crate) transient_retries_prompt_total: std::cell::Cell<u32>,
    /// Start of the current transient-recovery episode (first failed attempt; cleared on a successful sample).
    /// Prompt-scoped with the counter above.
    pub(crate) transient_episode_start: std::cell::Cell<Option<tokio::time::Instant>>,
    /// Normal sessions hold a clone of `MvpAgent::auth_method_id`.
    /// Subagents instead get a fresh, isolated handle seeded once at spawn (frozen for their lifetime).
    /// `None` until the agent has selected a method.
    pub(crate) auth_method_id: crate::agent::auth_method::SharedAuthMethodId,
    /// Memoized per-model auth state, read through [`SessionActor::model_auth_facts`] and [`SessionActor::model_auth_provider`].
    /// Because a config edit can turn the selected model into a per-model BYOK model without changing its id, keying on the id alone is insufficient.
    /// Each model/credential chokepoint must clear this memo (`replace(None)`).
    pub(crate) model_auth_memo: std::cell::RefCell<Option<ModelAuthMemo>>,
    /// This turn's model/effort distribution, reported when the turn ends.
    pub(crate) jev_ledger: std::cell::RefCell<jev_ledger::JevTurnLedger>,
    /// Per-session Jev routing choice. The models manager is shared across
    /// sessions for catalog refreshes, so this must not be read from it while
    /// routing a child.
    pub(crate) jev_effort_auto: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The event fires at each of the six `OaiCompatClient` 401 arms in `distill-sampler`.
    /// Threaded into every `SamplerConfig` reconstructed by `reconstruct_full_config`.
    /// `None` when the session was spawned without an `AuthManager` (BYOK direct mode, test fixtures).
    pub(crate) attribution_callback: Option<distill_sampler::SharedAttributionCallback>,
    /// Owns the token refresher (`configure_refresher()`) and non-sampler 401 attribution. Sampler path uses `attribution_callback`.
    /// Idle-resume refresh calls `record_auth_401` directly with this handle.
    /// `None` for tests / BYOK that don't need refresh or the attribution emit.
    pub(crate) auth_manager: Option<Arc<AuthManager>>,
    /// Set from the `session/new` / `session/load` `_meta` chat kind and sticky for the session.
    /// Chat-kind ACU/list sources product REST skills, never Build disk skills.
    pub(crate) is_chat_kind: bool,
    pub(crate) state: TokioMutex<State>,
    /// Notification transport: gateway, persistence channel, replay buffer.
    pub(crate) notifications: NotificationSender,
    pub(crate) permissions: PermissionHandle,
    pub(crate) tool_context: ToolContext,
    /// Managed Read-deny glob patterns, resolved once at construction.
    /// (Re-)injected into the ToolBridge so the Grep tool excludes policy-forbidden paths.
    pub(crate) deny_read_globs: Vec<String>,
    /// Consolidated MCP state (configs, clients, init status) protected by a single lock.
    /// The single lock keeps config updates and init-status checks atomic.
    pub(crate) mcp_state: Arc<TokioMutex<McpState>>,
    /// MCP initialization strategy. `Cell`: per-attachment policy.
    /// A resident `session/load` carrying explicit `startupHints` re-applies the attaching client's strategy (`UpdateAttachPolicy`).
    /// So a headless client attaching to an actor spawned by an interactive one still gets Blocking MCP init on its turns (and vice versa).
    pub(crate) mcp_strategy: std::cell::Cell<McpInitStrategy>,
    /// Actor-based chat state handle; manages conversation, tokens, timing, and persistence.
    /// Also stores credentials (api_key, optional extra access key, client_version) opaquely.
    pub(crate) chat_state_handle: distill_chat_state::ChatStateHandle,
    /// Current running prompt/turn id, shared with SessionHandle.
    pub(crate) current_prompt_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) active_work: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) unattributed_background_usage: std::sync::atomic::AtomicBool,
    /// Open blocking reverse-requests (permission / question / plan-approval), keyed by `tool_call_id`.
    /// Shared with `SessionHandle` so the roster can read it synchronously to report `NeedsInput`.
    /// Mutated by `PendingInteractionGuard` at each reverse-request site. Never persisted.
    pub(crate) pending_interactions: crate::session::pending_interaction::PendingInteractions,
    /// Gates product analytics, not trace uploads.
    /// Resolved at spawn as `is_telemetry_enabled() && !is_zdr()`; ZDR teams always have this false.
    pub(crate) telemetry_enabled: bool,
    pub(crate) supports_backend_search: std::cell::Cell<bool>,
    /// Per-turn override, set at promotion. Not persisted; a reload reverts to the definition seed.
    pub(crate) tool_overrides: std::cell::RefCell<Option<distill_sampling_types::ToolOverrides>>,
    pub(crate) resolved_tool_overrides:
        std::sync::Arc<arc_swap::ArcSwapOption<distill_sampling_types::ToolOverrides>>,
    pub(crate) compactions_remaining:
        std::cell::Cell<Option<distill_sampling_types::CompactionsRemaining>>,
    pub(crate) compaction_at_tokens:
        std::cell::Cell<Option<distill_sampling_types::CompactionAtTokens>>,
    /// Server-side doom-loop check policy, resolved once at spawn by `Config::resolve_doom_loop_recovery`; `None` means disabled.
    /// `reconstruct_full_config` threads it into the sampler config, and the sampler itself sends the matching `x-grok-doom-loop-check` header.
    pub(crate) doom_loop_recovery: Option<distill_sampling_types::DoomLoopRecoveryPolicy>,
    /// Telemetry-only per-turn detector/recovery tally (deduplicated labels, attempts, budget-spent accept, tightest recovery trigger).
    /// Accumulated by the event drainer and taken at turn end for analytics events.
    pub(crate) doom_loop_turn_tally:
        parking_lot::Mutex<crate::session::doom_loop_telemetry::DoomLoopTurnTally>,
    /// File state tracker for rewind functionality
    pub(crate) file_state_tracker: Arc<FileStateTracker>,
    /// Last prompt text before the most recent rewind.
    /// When set, the next `prompt()` compares its text to distinguish regeneration (same text) from edit-and-retry (different text).
    pub(crate) rewind_pending_prompt: std::sync::Mutex<Option<String>>,
    /// Startup hints for the session: they customize the user message prefix and the git status mode.
    /// (Non-interactive mode gets the fast no-untracked git status.)
    pub(crate) startup_hints: StartupHints,
    /// Wakes the status-line emitter task, and on drop ends it.
    /// See [`status_line::run_status_emitter`] and the `Drop` beside it.
    pub(crate) status_wake: status_line::StatusWake,
    /// Delivery-tool names for the CURRENT attachment, seeded from the spawn `startupHints.deliveryTools`.
    /// Re-applied when a resident `session/load` carries explicit hints (`UpdateAttachPolicy`).
    /// Structural spawn-time hints (subagent identity, inherited prefix) never change on re-attach; per-attachment policy may.
    pub(crate) delivery_tools: std::cell::RefCell<Vec<String>>,
    /// `nonInteractive` for the CURRENT attachment (same lifecycle as `delivery_tools`).
    /// `startup_hints.non_interactive` keeps governing spawn-time structure (system prompt variant, user-message prefix, git-status mode).
    pub(crate) attach_non_interactive: std::rc::Rc<std::cell::Cell<bool>>,
    /// Verbatim mirror-fork override: when `Some`, every turn sends this exact parent tool schema instead of the locally-built toolset.
    /// That keeps the child's request prefix byte-identical to the parent for radix cache reuse.
    /// `None` for all non-fork (and summarized-fork) sessions.
    pub(crate) forked_tool_override: Option<Vec<ToolSpec>>,
    /// Compaction configuration and runtime state.
    pub(crate) compaction: super::compaction_config::CompactionConfig,
    /// Memory subsystem: storage, flush config, injection state, telemetry.
    pub(crate) memory: super::memory_state::SessionMemory,
    /// Telemetry counters for session summary.
    pub(crate) session_start: std::time::Instant,
    /// Per-chunk idle timeout for inference streaming; a stall aborts the stream.
    pub(crate) inference_idle_timeout: Duration,
    /// Remote kill switch (`uncharged_401_park`), resolved once at spawn —
    /// see `resolve_uncharged_401_park`.
    pub(crate) uncharged_401_park_enabled: bool,
    pub(crate) max_retries: u32,
    /// Fixed bounds on a subagent turn's 429 waiting.
    pub(crate) rate_limit_waits: RateLimitWaitConfig,
    /// Maximum tool-use turns before the session stops. `None` means unlimited.
    pub(crate) max_turns: Option<usize>,
    /// Pending mid-turn interjections from the user (Ctrl+Enter).
    /// Pushed by `SessionCommand::Interject` handler, drained at safe points in `process_conversation_turn`.
    /// Internally synchronized.
    pub(crate) pending_interjections: InterjectionBuffer<acp::ImageContent>,
    /// Skill-announcement reminders that arrived while a turn was running.
    /// Flushed at the same safe points as `pending_interjections`, plus on cancel/idle.
    /// The flush also delivers the plan tracker's buffered mid-turn activation reminder (see `activate_plan_mode_mid_turn`).
    pub(crate) pending_skill_reminders: Mutex<Vec<ConversationItem>>,
    /// Idle flush timeout: `None` means disabled, `Some(duration)` flushes after that much inactivity.
    pub(crate) idle_flush_timeout: Option<std::time::Duration>,
    /// Periodic dream check interval: `None` means disabled.
    pub(crate) dream_check_timeout: Option<std::time::Duration>,
    /// Conversation length at last idle flush; skip if unchanged (no new messages).
    pub(crate) last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize,
    /// Internal event queue for actor-owned replay buffering and flush barriers.
    pub(crate) event_tx: mpsc::UnboundedSender<SessionEvent>,
    /// Buffering settings captured at session creation.
    /// The concrete ReplayBuffer is owned by `run_session()`.
    pub(crate) buffering_settings: Option<BufferingSettings>,
    /// Client identifier for telemetry, passed from the MvpAgent (extracted from initialize meta)
    pub(crate) client_identifier: Option<String>,
    /// Origin client for User-Agent on sampling requests.
    pub(crate) origin_client: Option<crate::http::OriginClientInfo>,
    /// Feedback manager for signal tracking and feedback request heuristics
    pub(crate) feedback_manager: Arc<FeedbackManager>,
    pub(crate) upload_queue:
        std::sync::Arc<std::sync::OnceLock<distill_file_utils::queue::UploadQueue>>,
    /// Cancellation token for the feedback sync loop (None if no feedback client)
    pub(crate) sync_loop_cancel: Option<tokio_util::sync::CancellationToken>,
    /// The fully-built Agent: owns the ToolBridge, system prompt, policies, and the AgentDefinition.
    /// Replaces the old `tool_bridge` and `agent_definition` fields.
    /// Wrapped in `RefCell` for mid-session mutation (skill refresh, prompt regen).
    pub(crate) agent: std::cell::RefCell<distill_agent::Agent>,
    /// Dedup slot for `x.ai/git_head_changed`, shared with the fs-watch `GitHead` consumer (see `git_head_dedup_key`).
    pub(crate) last_reported_branch: Arc<parking_lot::Mutex<Option<String>>>,
    /// Client opted into `x.ai/gitHeadChanged`.
    /// When false (headless/SDK), `maybe_notify_git_branch` no-ops; no git subprocess runs.
    git_head_enabled: bool,
    /// A client that will draw a status row has attached (`x.ai/statusLine`).
    /// While false, the emitter wakes and returns without building anything: no git discovery, no chat-state round trips.
    /// Live rather than fixed at spawn, because a resident session outlives the client that created it.
    pub(crate) status_line_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Shared models manager for etag-triggered refresh from response headers.
    pub(crate) models_manager: crate::agent::remote_config::ModelsManager,
    /// The system prompt's `Workspace Path` is set at build time via `AgentBuilder::with_prompt_working_directory()`.
    /// Set once at session spawn from the `prompt_display_cwd` parameter.
    /// Uses `OnceLock` for lock-free reads, a set-once guarantee, and `&self` mutability (SessionActor is behind `Arc`).
    pub(crate) display_cwd: std::sync::OnceLock<String>,
    /// Initialized from the `AgentDefinition` at spawn, updated when the session mode changes via `handle_session_mode()`.
    /// Used by the model-switch guard to determine whether a model's `agent_type` is compatible with the current session.
    pub(crate) active_agent_type: parking_lot::Mutex<Option<String>>,
    /// Live gate shared with the notification bridge (see `NotificationBridgeConfig::queue_exit_reminder_on_approved_exit`).
    /// Seeded at spawn from the agent definition's harness.
    /// Refreshed by `handle_rebuild_agent_for_definition` so the bridge always agrees with the live harness gate.
    pub(crate) queue_exit_reminder_on_approved_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Live gate shared with the notification bridge (see `NotificationBridgeConfig::emit_local_background_tasks`).
    /// Default `true`; flipped off when the session is gateway-backed so local
    /// snapshots cannot last-wins-clear remote Running rows.
    pub(crate) emit_local_background_tasks: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// First skill the current prompt activated via its slash-skill path, recorded as `skill.name` on the turn span.
    /// Reset at the start of each prompt (`handle_prompt`), so it never leaks across turns.
    pub(crate) active_skill: parking_lot::Mutex<Option<String>>,
    /// Canonical session mode last set via ACP `session/set_mode`.
    /// Used as the fallback start prompt mode when prompt request metadata does not explicitly provide one.
    pub(crate) current_prompt_mode: Arc<parking_lot::Mutex<PromptMode>>,
    /// Prompt mode captured at the start of the current turn.
    /// Set once in `handle_prompt` and never modified during the turn.
    /// Used for `start_prompt_mode` telemetry.
    pub(crate) turn_start_prompt_mode: parking_lot::Mutex<PromptMode>,
    /// Set at turn start from the prompt mode parameter, then updated only by agent-initiated tool calls (`EnterPlanMode` / `ExitPlanMode`).
    /// NOT affected by `session/set_mode` (which only changes the next turn's start mode).
    /// Read at turn end for `end_prompt_mode` telemetry.
    pub(crate) turn_prompt_mode: Arc<parking_lot::Mutex<PromptMode>>,
    /// Session-scoped dynamic plan-mode state (not part of `AgentDefinition`).
    /// All plan mode logic lives in `plan_mode.rs`; the session actor just calls into the tracker at the appropriate points.
    /// `Arc`-shared with the notification bridge so `PlanModeEntered` / `PlanModeExited` tool notifications can transition state directly.
    pub(crate) plan_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PlanModeTracker>>,
    /// Whether goal mode (`/goal`) is enabled for this session (feature flag).
    pub(crate) goal_enabled: bool,
    pub(crate) background_workflows_enabled: bool,
    goal_harness_enabled: std::sync::atomic::AtomicBool,
    goal_harness_availability_reconciled: std::sync::atomic::AtomicBool,
    /// Session-scoped state for the `/goal` Design-Execute-Verify loop.
    /// Modeled after `plan_mode` above.
    pub(crate) goal_tracker: Arc<parking_lot::Mutex<crate::session::goal_tracker::GoalTracker>>,
    /// `task_id`s of background tasks (and monitors) that originated during the goal turn.
    /// Either spawned by the goal model itself or reparented from a harness verifier/planner subagent on its exit.
    /// Their late auto-wake completions are dropped by [`Self::maybe_drain_notifications`] regardless of the goal's current status.
    pub(crate) goal_turn_task_ids: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Consecutive non-completing (cancelled/errored) goal-mode turns while the goal is `Active`.
    /// Auto-pauses the goal with `GoalPauseReason::BackOff` once the counter reaches [`GOAL_CONTINUATION_BACKOFF_THRESHOLD`].
    /// In-memory only; session restart is itself a reset.
    pub(crate) goal_continuation_streak: std::sync::atomic::AtomicU32,
    pub(crate) goal_blocked_streak: std::sync::atomic::AtomicU32,
    pub(crate) goal_update_rx: std::cell::RefCell<
        Option<
            tokio::sync::mpsc::UnboundedReceiver<
                distill_tools::implementations::distill::update_goal::UpdateGoalEnvelope,
            >,
        >,
    >,
    pub(crate) goal_update_tx: tokio::sync::mpsc::UnboundedSender<
        distill_tools::implementations::distill::update_goal::UpdateGoalEnvelope,
    >,
    pub(crate) workflow_manager:
        Arc<tokio::sync::Mutex<crate::session::workflow::manager::WorkflowManager>>,
    pub(crate) workflow_launch_tx: tokio::sync::mpsc::UnboundedSender<
        distill_tools::implementations::distill::workflow::WorkflowLaunchEnvelope,
    >,
    pub(crate) goal_classifier_enabled: bool,
    /// Master switch for the goal planner subagent.
    pub(crate) goal_planner_enabled: bool,
    /// Master switch for the one-shot goal summarizer (the closing "what was accomplished" summary on a verified achievement).
    /// Cached at actor construction (mirrors `goal_classifier_enabled`); absent remote setting tracks goal mode, `Some(false)` is a kill-switch.
    pub(crate) goal_summary_enabled: bool,
    /// Remote tier of the Length-salvage budget resolver, snapshot from
    /// `RemoteSettings` at actor construction. `Some(0)` is explicit off.
    pub(crate) length_salvage_remote_budget: Option<u32>,
    /// Resolved skeptic count for the verification stage.
    /// Cached at actor construction (mirrors `goal_classifier_enabled`) and threaded into [`Self::run_verification_stage_for_drain`].
    /// Default `GOAL_VERIFIER_SKEPTIC_COUNT`; clamped to `[GOAL_VERIFIER_SKEPTIC_MIN, GOAL_VERIFIER_SKEPTIC_MAX]` by the resolver.
    pub(crate) goal_verifier_skeptic_count: u32,
    /// Resolved per-role `/goal` model selection (planner / strategist single pairs and the ordered skeptic pool).
    /// The kill-switch is already applied.
    /// Cached at actor construction from remote settings; `Default` (all `InheritCurrent`, empty pool) reproduces today's behavior.
    pub(crate) goal_role_models: GoalRoleModelConfig,
    /// Kill-switch (`GROK_GOAL_USE_CURRENT_MODEL_ONLY` / `[features] goal_use_current_model_only`) resolved at actor build.
    /// When `true`, every `/goal` role inherits the current model.
    /// The skeptic panel also checks this flag directly so a previously-frozen `skeptic_model_assignment` is overridden too.
    pub(crate) goal_use_current_model_only: bool,
    /// Resolved per-goal cap on classifier runs (past the cap the goal auto-pauses via `BackOff`).
    /// Cached at actor construction like `goal_verifier_skeptic_count`.
    /// Default `GOAL_CLASSIFIER_MAX_RUNS_DEFAULT`; floored at `GOAL_CLASSIFIER_MAX_RUNS_MIN` with no upper ceiling by the resolver.
    pub(crate) goal_classifier_max_runs: u32,
    /// Resolved N for the stall-triggered strategist: it fires every N consecutive `NotAchieved` verifications (and again at 2N, 3N, …).
    /// Default `max(1, goal_classifier_max_runs / 2)`; clamped to `>= 1` by the resolver.
    /// Read by the strategist trigger in `apply_classifier_outcome`.
    pub(crate) goal_strategist_every: u32,
    /// Resolved refuted-goal round count before the continuation directive escalates to a forceful "re-verify now" block.
    /// Cached at actor construction.
    /// Read by [`Self::prepare_goal_continuation`].
    pub(crate) goal_reverify_after: u32,
    /// Set on session load once `maybe_reconcile_active_goal_without_plan` has run.
    /// Subsequent prompt-flow ticks then don't repeat the pause-on-load check.
    pub(crate) goal_plan_reconciled: std::sync::atomic::AtomicBool,
    pub(crate) pending_classifier_completions: parking_lot::Mutex<
        VecDeque<distill_tools::implementations::distill::update_goal::UpdateGoalInput>,
    >,
    /// [`Self::account_not_achieved_without_sampler`].
    pub(crate) goal_classifier_in_flight: std::sync::atomic::AtomicBool,
    /// Agent-level managed MCP gateway catalog cache.
    pub(crate) managed_mcp_handle: crate::session::managed_mcp::ManagedMcpStateHandle,
    /// Original client-provided MCP servers from session creation.
    /// Retained for re-merge during plugin reload.
    pub(crate) initial_client_mcp_servers: Vec<acp::McpServer>,
    /// Shared MCP tool metadata for the BM25 search index. Updated after MCP init.
    pub(crate) tool_metadata_snapshot:
        Arc<std::sync::Mutex<crate::session::tool_index::ToolMetadataSnapshot>>,
    /// MCP servers (connected and failed) already announced via system-reminder.
    /// See [`crate::session::announcement_state::McpAnnounced`] for how repeats are deduped.
    pub(crate) mcp_announcements: Mutex<crate::session::announcement_state::McpAnnounced>,
    /// Controls whether MCP server reminders inject only changes (Delta) or the full server list (Full).
    /// Read from `MCP_REMINDER_MODE` env var.
    pub(crate) mcp_reminder_mode: McpReminderMode,
    /// Set when the MCP server set changes and a reminder needs injection.
    /// Cleared by `maybe_inject_mcp_reminder` after injecting.
    pub(crate) mcp_reminder_dirty: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) mcp_connecting_reminder_injected: std::cell::Cell<bool>,
    /// Serializes snapshot refreshes end-to-end so the last write is the latest read, without holding `mcp_state` across refresh awaits.
    pub(crate) mcp_refresh_gate: Arc<tokio::sync::Mutex<()>>,
    /// Background-computed user-message prefix, injected before the first prompt.
    pub(crate) deferred_prefix: DeferredPrefix,
    /// The prompt gates' per-generation startup grace and full-wait timeout.
    pub(crate) mcp_startup_waits: McpStartupWaits,
    /// Background MCP init passes (handshakes and tool registration), aborted with the actor.
    pub(crate) mcp_init_tasks: std::cell::RefCell<tokio::task::JoinSet<()>>,
    pub(crate) weak_self: std::sync::Weak<SessionActor>,
    /// Where `&self` methods hand startup work so it outlives the caller's future but not the session.
    pub(crate) startup_tasks: StartupTaskHandle,
    /// Extensions to notify at turn and session lifecycle edges. Built once by `session_extension_registry` at actor construction and frozen after.
    pub(crate) extension_registry: distill_agent_lifecycle::LocalExtensionRegistry,
    /// Local date last shown to the model.
    /// Shown via the `<user_info>` prefix (session start, compaction, model switch) or a date-rollover `<system-reminder>`.
    /// Drives [`SessionActor::maybe_inject_date_rollover_reminder`].
    pub(crate) last_announced_local_date: std::cell::Cell<chrono::NaiveDate>,
    /// True when the render-failure fallback stamped a date into a date-free template's prefix.
    /// [`SessionActor::maybe_inject_date_rollover_reminder`] then still rolls it over.
    pub(crate) prefix_carries_fallback_date: std::cell::Cell<bool>,
    /// Prompt index when search_tool last ran; -1 means never.
    /// Used for turns_since_last_search.
    pub(crate) last_search_prompt_index: std::sync::atomic::AtomicI64,
    /// Timestamp (millis since epoch) of the last successful API request.
    /// Used to detect session resume after idle and proactively refresh model metadata.
    pub(crate) last_api_request_at: std::sync::atomic::AtomicI64,
    /// Loaded at session startup; can be updated mid-session via `/plugins reload`.
    /// `None` when no plugin registry was supplied at spawn time.
    /// Wrapped in `RefCell` for mid-session reload from `&self` methods.
    pub(crate) hook_registry:
        std::cell::RefCell<Option<Arc<distill_hooks::discovery::HookRegistry>>>,
    /// Disabled-hooks snapshot every dispatch filters on, so the actor never reads the file mid-turn.
    /// Loaded at spawn and refreshed by hook reload and enable/disable; another session's toggle lands here at the next reload.
    pub(crate) hook_disabled: std::cell::RefCell<Arc<distill_hooks::trust::DisabledHooks>>,
    /// The turn's single end-of-turn hook report.
    /// Actor-scoped rather than turn-local because the gate runs on the turn task while a cancel runs on the command loop.
    pub(crate) turn_report: turn_report_slot::TurnReportSlot,
    /// Keyed on the same turn epoch as `turn_report`: a turn announces its abort at most once.
    pub(crate) turn_abort: turn_report_slot::AbortAnnouncement,
    /// Set once by [`turn_end_hooks::TurnEndQueue::spawn`]; `None` before the loop starts.
    pub(crate) turn_end_tx:
        std::cell::RefCell<Option<tokio::sync::mpsc::UnboundedSender<turn_end_hooks::QueueItem>>>,
    /// Client hooks from `session/new` `_meta["x.ai/hooks"]`; gated in [`crate::session::acp_session::hooks`].
    /// `RefCell` so `load_session` reconnect can replace the set on the live actor (see `SessionCommand::SetClientHooks`).
    pub(crate) client_hooks: std::cell::RefCell<crate::extensions::hooks::ClientHooks>,
    /// Resolved workspace root for hooks: git worktree root if in a git repo, otherwise session cwd.
    /// Used for hook child process cwd, envelope fields, and GROK_WORKSPACE_ROOT env var.
    pub(crate) hook_resolved_workspace_root: String,
    /// The detected VCS kind for this session's workspace.
    pub(crate) vcs_kind: distill_workspace::session::git::VcsKind,
    /// Errors from last hook config load (parse failures, etc.).
    pub(crate) hook_load_errors: std::cell::RefCell<Vec<String>>,
    /// Plugin registry snapshot for this session. Updated on `/plugins reload`.
    /// `RefCell` for mid-session reload from `&self` methods.
    pub(crate) plugin_registry:
        std::cell::RefCell<Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>>,
    /// Shared handle to the agent-level plugin registry.
    /// Used by `/plugins reload` to trigger a rebuild that new sessions see.
    pub(crate) plugin_registry_handle: Option<distill_agent::plugins::SharedPluginRegistryHandle>,
    /// Centralized event tracking: event log, turn-end guard, active tool, doom loop terminate flag.
    pub(crate) events: crate::session::events::EventTracker,
    /// Optional hub-side session event emitter (always constructed without a harness client in the agent; methods no-op with `None` transport).
    pub(crate) observability_bridge: distill_computer_hub_sdk::ObservabilityBridge,
    /// Turn number captured at the start of each turn (before prompt index increment).
    /// Used by `ToolCallStarted` bridge emissions so they report the same turn number as `TurnStarted` / `TurnEnded`.
    pub(crate) current_turn_number: std::cell::Cell<u64>,
    pub(crate) turn_phases: std::sync::Arc<distill_telemetry::turn_phases::TurnPhaseProfile>,
    /// Recap rate-limit watermark (`main_turns` of last finished recap; `0` means none).
    pub(crate) last_recap_main_turn: std::cell::Cell<usize>,
    /// True while a recap model call is in flight (auto or manual).
    /// Prevents concurrent `spawn_local` recaps from racing watermark restore.
    pub(crate) recap_in_flight: std::cell::Cell<bool>,
    /// Bumped on each real user prompt (queue accept and turn start); an in-flight recap suppresses emit if this changes before commit.
    pub(crate) recap_epoch: std::cell::Cell<u64>,
    /// A newer completion (or a real prompt / rewind / cancel / shutdown) aborts it; its result would describe an older turn.
    /// A completion respawns it; see `restart_turn_summary`.
    /// Cleared when the task finishes so `Some` means "still running".
    pub(crate) turn_summary_task: std::cell::RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Generation of the currently registered turn-summary task.
    /// Bumped on each spawn so a finishing older task cannot clear a newer slot.
    pub(crate) turn_summary_generation: std::cell::Cell<u64>,
    /// Turn-summary gate, resolved once at spawn (env / config / remote settings, from the `turn_summary` feature).
    pub(crate) turn_summary_enabled: bool,
    /// Early-session title-refresh gate, resolved once at spawn (defaults to `turn_summary_enabled`; see `Config::resolve_title_refresh`).
    pub(crate) title_refresh_enabled: bool,
    /// The in-flight title-refresh side-call, if any.
    /// Only one runs at a time (a newer completion skips rather than aborts); aborted on rename, rewind, and shutdown.
    /// See `maybe_refresh_title`.
    pub(crate) title_refresh_task: std::cell::RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// Generation of the currently registered title-refresh task.
    /// A finishing task whose generation no longer matches must not persist its result.
    pub(crate) title_refresh_generation: std::cell::Cell<u64>,
    /// Index into `TITLE_REFRESH_TURNS` of the next checkpoint to apply.
    /// Advanced when an attempt completes (success *or* failure, with catch-up past skipped checkpoints), and persisted to the watermark.
    /// Once it reaches the end the title is frozen.
    pub(crate) next_title_refresh_idx: std::cell::Cell<usize>,
    /// True while THIS session has a prompt turn in flight (RAII-guarded in `handle_prompt`).
    /// `tool_context.is_turn_active` is the agent-wide coordinator flag shared by all sessions, so it is unusable for per-session decisions.
    /// `Arc` so it can be re-checked inside the chat-state actor's `RepairHistory` handler.
    pub(crate) session_turn_active: Arc<std::sync::atomic::AtomicBool>,
    /// Out-of-band capture of streamed generations for trace upload; never returned by `BuildConversationRequest` or sent back to the model.
    /// `Completed` drops the in-progress generation without wiping earlier uncommitted ones, so a same-turn doomloop retry is preserved.
    /// A queued prompt's `StreamStarted` can reset the live slot between cancel and `TakeStreamingCapture`, so the take sees a prompt-id mismatch and drops `streaming_partial.json`.
    pub(crate) streaming_turn_capture: parking_lot::Mutex<StreamingTurnCapture>,
    pub(crate) stream_apply_span: parking_lot::Mutex<Option<StreamApplySpan>>,
    pub(crate) current_turn_span_id: parking_lot::Mutex<Option<tracing::Id>>,
    /// Orders streamed text against tool-call `send_update`s, which allocate the client-visible `eventId` at call time on separate `LocalSet` tasks.
    /// Without the barrier a tool call can land between still-draining text chunks and split the assistant message on every client.
    /// Map presence owns the stream; a timeout clears the waiter without invalidating FIFO events already queued, and turn/cancel boundaries revoke abandoned ownership.
    pub(crate) turn_stream_drained:
        parking_lot::Mutex<std::collections::HashMap<distill_sampler::RequestId, StreamOwnership>>,
    /// A server-confirmed image strip awaiting proof that the stripped retry helped.
    /// URLs are buffered by request id on `ImagesStripped`.
    /// They persist to stored history only when that request's `Completed` arrives, and drop on `Failed`.
    pub(crate) pending_image_strip: parking_lot::Mutex<
        std::collections::HashMap<distill_sampler::RequestId, PendingImageStrip>,
    >,
    /// Serializes durable image-strip writes with conversation rewinds.
    pub(crate) image_strip_rewrite_barrier: ImageStripRewriteBarrier,
    /// Handle to the per-session `distill-sampler` actor.
    /// Live sessions get a real handle from `spawn_session_actor`; tests and other constructor sites use `SamplerHandle::noop()`.
    /// All inference flows through this handle.
    pub(crate) sampler_handle: distill_sampler::SamplerHandle,
    /// Turn-sampling gate: `None` is the main session (ungated), `Some` is the process tree's shared sampling semaphore.
    /// See `acquire_subagent_sampling_permit`.
    pub(crate) sampling_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Cached recipe for constructing this session's [`distill_agent::Agent`].
    /// Reused by `handle_rebuild_agent_for_definition` to build a fresh `Agent`.
    /// A fresh `Agent` covers the system prompt, the [`distill_tools::bridge::ToolBridge`], and the tool registry.
    pub(crate) rebuild_spec: Arc<crate::session::agent_rebuild::AgentRebuildSpec>,
    /// Resolved vision model ID for auxiliary image processing.
    /// Populated from `Config.image_description_model` at spawn.
    pub(crate) image_description_model: String,
    /// Cache auxiliary image outputs by content and prompt fingerprint.
    pub(crate) image_describe_cache: Arc<crate::session::image_describe::ImageDescribeCache>,
    /// Per-subagent token state keyed by `subagent_id`; sums into goal totals via [`Self::goal_tokens`].
    pub(crate) subagent_token_records: parking_lot::Mutex<HashMap<String, SubagentTokenRecord>>,
    pub(crate) workspace_ops: distill_workspace::WorkspaceOps,
    /// Template for building trace configs on synthetic auto-wake turns.
    /// Captured from the first real user prompt's trace config so synthetic turns can upload artifacts using the same bucket/method.
    pub(crate) trace_config_template: std::cell::RefCell<Option<TraceConfigTemplate>>,
    /// Generation counter bumped on each fresh user prompt so the laziness check can detect input without a stored-permit wake.
    /// A `Notify` fired before the classifier spawns would abort the first idle wait; an `AtomicU64` snapshot has no such hazard.
    /// One consumer only, so a lock-bearing `watch` channel is unnecessary (model-switch still uses `watch` because the main loop must wake).
    pub(crate) user_input_generation: std::sync::atomic::AtomicU64,
    /// Observation-only: no nudges are ever injected when this is `Some`.
    /// `Arc<Path>` because the path is immutable after session spawn.
    /// Concurrent appends rely on `O_APPEND`'s atomic guarantee for writes under `PIPE_BUF` (JSONL lines fit).
    pub(crate) laziness_debug_log: Option<std::sync::Arc<std::path::Path>>,
    /// Last live-orphan disk scan.
    /// SessionActor is `!Send`, so a `Cell` is enough to throttle mid-turn ticks without a lock.
    pub(crate) last_live_orphan_reconcile: std::cell::Cell<Option<std::time::Instant>>,
}
/// Template for building trace configs on synthetic auto-wake turns.
/// Captured from the first real user prompt's `TraceExportConfig`.
/// Synthetic turns can then upload artifacts to the same GCS bucket using the same upload method (direct / proxy).
#[derive(Clone)]
pub(crate) struct TraceConfigTemplate {
    pub(crate) bucket_url: Option<String>,
    pub(crate) upload_method: crate::session::repo_changes::UploadMethod,
}
impl SessionActor {
    fn signals_handle(&self) -> SessionSignalsHandle {
        self.feedback_manager.signals_handle()
    }
    fn emit_event(&self, event: crate::session::events::Event) {
        self.events.emit(event);
    }
    fn emit_turn_ended(
        &self,
        outcome: crate::session::events::TurnOutcomeLabel,
        category: Option<crate::session::events::CancellationCategory>,
        context: Option<serde_json::Value>,
    ) {
        self.events.emit_turn_ended(outcome, category, context);
    }
    /// Current model ID for OTLP span attributes.
    /// Reads from chat_state_handle so it always reflects the latest model override; no stale cached field.
    /// Returns "unknown" if no sampling config is set.
    async fn current_model_id(&self) -> String {
        self.chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unknown".to_string())
    }
    fn session_id_string(&self) -> String {
        self.session_info.id.0.to_string()
    }
    /// Send a before-turn hook via the local workspace channel.
    /// Fire-and-forget; failures are logged but do not interrupt the turn.
    async fn send_before_turn_event(
        &self,
        payload: distill_tool_protocol::turn_hook::BeforeTurnPayload,
    ) {
        self.workspace_ops
            .on_before_turn(&self.session_id_string(), &payload)
            .await;
    }
    /// Send an after-turn hook via the local workspace channel.
    /// Fire-and-forget; failures are logged but do not interrupt the turn.
    #[tracing::instrument(
        name = "session.after_turn",
        skip_all,
        fields(session_id = %self.session_info.id.0, turn_number = payload.turn_number)
    )]
    async fn send_after_turn_event(
        &self,
        payload: distill_tool_protocol::turn_hook::AfterTurnPayload,
    ) {
        self.workspace_ops
            .on_after_turn(&self.session_id_string(), &payload)
            .await;
    }
    /// Compute the live command availability snapshot for this session.
    /// Convenience wrapper that fetches the toolset and delegates to `build_command_availability`.
    /// Use this on the inbound resolve path.
    async fn command_availability(&self) -> slash_commands::CommandAvailability {
        #[cfg(test)]
        crate::session::slash_authority::record_command_availability_call();
        let tool_names = self.registered_tool_names().await;
        let has_workflow_runs = !self.workflow_tracker().await.lock().list().is_empty();
        let availability = self.build_command_availability(&tool_names, has_workflow_runs);
        if !self.goal_runs_on_workflow_engine() {
            self.maybe_reconcile_active_goal_without_harness().await;
        }
        self.maybe_reconcile_active_goal_without_plan().await;
        availability
    }
    /// Compute command availability without workflow-manager reads or goal reconciliation.
    async fn command_availability_for_skill_projection(
        &self,
    ) -> slash_commands::CommandAvailability {
        #[cfg(test)]
        crate::session::slash_authority::record_command_availability_call();
        let tool_names = self.registered_tool_names().await;
        self.build_local_command_availability(&tool_names)
    }
    /// Build the `CommandAvailability` snapshot from a precomputed slice of tool names plus the live session-scoped capability state.
    /// Single source of truth for the seven gate fields.
    /// Both `command_availability` (resolve path) and `send_available_commands_update` (advertise path) call this so the two paths can never drift.
    fn build_command_availability(
        &self,
        tool_names: &[String],
        has_workflow_runs: bool,
    ) -> slash_commands::CommandAvailability {
        let mut availability = self.build_local_command_availability(tool_names);
        availability.goal = if self.goal_runs_on_workflow_engine() {
            self.sync_goal_harness()
        } else {
            self.sync_goal_harness_from_tools(tool_names)
        };
        availability.workflow_management = has_workflow_runs;
        availability
    }
    fn build_local_command_availability(
        &self,
        tool_names: &[String],
    ) -> slash_commands::CommandAvailability {
        use distill_tools::implementations::memory::{
            MEMORY_GET_TOOL_NAME, MEMORY_SEARCH_TOOL_NAME,
        };
        let can_read_memory = self
            .memory
            .mode()
            .is_some_and(crate::config::MemoryMode::is_v2)
            || tool_names
                .iter()
                .any(|n| n == MEMORY_SEARCH_TOOL_NAME || n == MEMORY_GET_TOOL_NAME);
        let goal = if self.goal_runs_on_workflow_engine() {
            self.goal_enabled
        } else {
            goal_slash_and_harness_available(self.goal_enabled, tool_names)
        };
        slash_commands::CommandAvailability {
            feedback: self.feedback_manager.is_enabled(),
            memory: self.memory.is_enabled() && can_read_memory,
            memory_configured: !self.memory.process_disabled
                && (self.memory.backend_params.is_some()
                    || self.memory.configured_storage.is_some()),
            scheduler: tool_names
                .iter()
                .any(|n| n == distill_tools::implementations::distill::SCHEDULER_CREATE_TOOL_NAME),
            hooks: self.hook_registry.borrow().is_some(),
            plugins: self.plugin_registry.borrow().is_some(),
            goal,
            workflows: tool_names.iter().any(|n| {
                n == distill_tools::implementations::distill::workflow::WORKFLOW_TOOL_NAME
            }),
            workflow_management: false,
        }
    }
    /// Names of every tool registered with the session's tool bridge.
    /// Allocates one `Vec<String>` per call.
    /// Callers that need both gating and the wire payload should call once and pass the slice to `build_command_availability`.
    async fn registered_tool_names(&self) -> Vec<String> {
        let bridge = self.agent.borrow().tool_bridge().clone();
        bridge
            .tool_definitions()
            .await
            .into_iter()
            .map(|td| td.function.name)
            .collect()
    }
    pub(crate) async fn workflow_tracker(
        &self,
    ) -> Arc<parking_lot::Mutex<crate::session::workflow::tracker::WorkflowTracker>> {
        self.workflow_manager.lock().await.tracker()
    }
    pub(crate) async fn is_busy(&self) -> bool {
        if self.active_work.load(std::sync::atomic::Ordering::Acquire) > 0 {
            return true;
        }
        {
            let state = self.state.lock().await;
            if state_is_busy(&state) {
                return true;
            }
        }
        crate::session::pending_interaction::has_parked_plan_approval(&self.pending_interactions)
    }
    pub(crate) fn goal_runs_on_workflow_engine(&self) -> bool {
        self.background_workflows_enabled
    }
    /// Uses `AgentMessageChunk` so the text appears in the conversation scrollback.
    /// Then flushes the replay buffer so the text is delivered before the turn ends.
    async fn send_slash_command_output(&self, text: &str) {
        self.send_slash_command_output_with_meta(text, None).await;
    }
    async fn send_host_turn_slash_command_output(&self, text: &str) {
        let mut chunk_meta = serde_json::Map::new();
        chunk_meta.insert(
            crate::session::storage::HOST_TURN_META_KEY.into(),
            serde_json::json!(true),
        );
        self.send_slash_command_output_with_meta(text, Some(chunk_meta))
            .await;
    }
    async fn send_slash_command_output_with_meta(
        &self,
        text: &str,
        meta: Option<serde_json::Map<String, serde_json::Value>>,
    ) {
        self.send_update(
            acp::SessionUpdate::AgentMessageChunk(
                acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_string(),
                )))
                .meta(meta),
            ),
            None,
        )
        .await;
        if let Err(e) = crate::session::replay_events::flush_replay_actor(&self.event_tx).await {
            tracing::warn!(?e, "flush_replay_actor failed");
        }
    }
    async fn send_feedback_notification(&self, request: crate::session::feedback::FeedbackRequest) {
        self.send_xai_notification(XaiSessionUpdate::FeedbackRequest(request.into()))
            .await;
    }
}
impl SessionActor {
    /// Used by async methods that need to drop the `RefCell::Ref<Agent>` borrow before awaiting.
    /// `Arc::clone` is cheap, and an outstanding `Ref` across `.await` would panic if anything on the suspended path did `self.agent.borrow_mut()`.
    fn tool_bridge_handle(&self) -> Arc<distill_tools::bridge::ToolBridge> {
        Arc::clone(self.agent.borrow().tool_bridge())
    }
}
const PROMPT_CONTEXT_FILENAME: &str = "prompt_context.json";
/// Persist the structured prompt context to `{session_dir}/prompt_context.json`.
/// The saved JSON enables deterministic re-rendering and `grok prompt --json` inspection.
fn save_prompt_context(session_info: &SessionInfo, prompt_context: &distill_agent::PromptContext) {
    let dir = match crate::session::persistence::ensure_owner_only_session_dir(session_info) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(?e, "failed to create session dir for prompt_context.json");
            return;
        }
    };
    let path = dir.join(PROMPT_CONTEXT_FILENAME);
    match serde_json::to_string_pretty(prompt_context) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                tracing::warn!(?e, "failed to write prompt_context.json");
            }
        }
        Err(e) => {
            tracing::warn!(?e, "failed to serialize PromptContext");
        }
    }
}
const SYSTEM_PROMPT_FILENAME: &str = "system_prompt.txt";
/// Synchronously and atomically rewrite `{session_dir}/chat_history.jsonl`.
/// `chat_state_handle.replace_conversation` persists the same content, but only after two async actor hops.
/// Whichever atomic `rename` lands last wins and the content is identical, so the two writers can never produce a torn file.
fn persist_chat_history_jsonl_sync(session_info: &SessionInfo, conversation: &[ConversationItem]) {
    let dir = match crate::session::persistence::ensure_owner_only_session_dir(session_info) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(session_id = %session_info.id.0, ?e,
                "persist_chat_history_jsonl_sync: failed to create session dir");
            return;
        }
    };
    let final_path = dir.join("chat_history.jsonl");
    let tmp_path = dir.join("chat_history.jsonl.sync.tmp");
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut buf = Vec::new();
        for item in conversation {
            serde_json::to_writer(&mut buf, item)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            buf.push(b'\n');
        }
        std::fs::File::create(&tmp_path)?.write_all(&buf)?;
        std::fs::rename(&tmp_path, &final_path)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::warn!(session_id = %session_info.id.0, ?e,
            "persist_chat_history_jsonl_sync: failed to persist chat_history.jsonl");
        let _ = std::fs::remove_file(&tmp_path);
    }
}
/// Persist the exact rendered system prompt to `{session_dir}/system_prompt.txt`.
/// Should match the first System entry in `chat_history.jsonl` modulo trailing newlines (`canonical_system_prompt_eq`).
/// A trailing-newline-only difference is benign; this artifact is a convenience mirror, and the conversation head stays the source of truth.
fn save_system_prompt(session_info: &SessionInfo, system_prompt: &str) {
    let dir = match crate::session::persistence::ensure_owner_only_session_dir(session_info) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(?e, "failed to create session dir for system_prompt.txt");
            return;
        }
    };
    let path = dir.join(SYSTEM_PROMPT_FILENAME);
    if let Err(e) = std::fs::write(&path, system_prompt) {
        tracing::warn!(?e, "failed to write system_prompt.txt");
    }
}
/// Load the canonical system prompt from `{session_dir}/system_prompt.txt`.
/// Returns `None` for sessions created before this artifact existed.
/// Callers should fall back to extracting from `chat_history.jsonl` if absent.
#[expect(dead_code, reason = "API for future viewers/debug tools")]
pub(crate) fn load_system_prompt(session_info: &SessionInfo) -> Option<String> {
    let dir = crate::session::persistence::session_dir(session_info);
    load_system_prompt_from_dir(&dir)
}
fn load_system_prompt_from_dir(session_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(session_dir.join(SYSTEM_PROMPT_FILENAME)).ok()
}
/// Load the canonical prompt context from `{session_dir}/prompt_context.json`.
///
/// Returns `None` for sessions without a persisted context.
#[expect(dead_code, reason = "API for future viewers/debug tools")]
pub(crate) fn load_prompt_context(
    session_info: &SessionInfo,
) -> Option<distill_agent::PromptContext> {
    let dir = crate::session::persistence::session_dir(session_info);
    load_prompt_context_from_dir(&dir)
}
fn load_prompt_context_from_dir(
    session_dir: &std::path::Path,
) -> Option<distill_agent::PromptContext> {
    let json = std::fs::read_to_string(session_dir.join(PROMPT_CONTEXT_FILENAME)).ok()?;
    serde_json::from_str(&json)
        .map_err(|e| tracing::warn!(?e, "failed to deserialize prompt_context.json"))
        .ok()
}
#[cfg(test)]
#[path = "acp_session_tests/client_hooks_tests.rs"]
mod client_hooks_tests;
#[cfg(test)]
#[path = "acp_session_tests/managed_hooks_tests.rs"]
mod managed_hooks_tests;
#[cfg(test)]
#[path = "acp_session_tests/model_switch_label_tests.rs"]
mod model_switch_label_tests;
#[cfg(test)]
#[path = "acp_session_tests/replace_system_prompt_tests.rs"]
mod replace_system_prompt_tests;
#[cfg(test)]
#[path = "acp_session_tests/support.rs"]
mod support;
#[cfg(test)]
#[path = "acp_session_tests/usage_categories_tests.rs"]
mod usage_categories_tests;
#[cfg(test)]
mod managed_gateway_descriptor_tests {
    use super::*;
    use distill_tools::types::output::{MCPOutput, ToolOutput};
    use distill_tools::types::tool::{ToolKind, ToolNamespace};
    #[derive(Debug, Default)]
    struct FixtureMcpTool;
    impl distill_tools::types::tool_metadata::ToolMetadata for FixtureMcpTool {
        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }
        fn tool_namespace(&self) -> ToolNamespace {
            ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "fixture"
        }
    }
    impl distill_tool_runtime::Tool for FixtureMcpTool {
        type Args = serde_json::Value;
        type Output = ToolOutput;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("server__tool").expect("valid")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("server__tool", "fixture")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _args: serde_json::Value,
        ) -> Result<ToolOutput, distill_tool_runtime::ToolError> {
            Ok(ToolOutput::MCP(MCPOutput::okay_output(
                "server__tool".to_string(),
                "server".to_string(),
                "ok".to_string(),
            )))
        }
    }
    #[tokio::test]
    async fn refresh_snapshot_indexes_only_admitted_gateway_tools() {
        let bridge = Arc::new(crate::tools::bridge::ToolBridge::for_test());
        bridge
            .register_mcp_tools(
                "server__tool".to_string(),
                FixtureMcpTool,
                Some(serde_json::json!({"type": "object"})),
            )
            .expect("local fixture registration succeeds");
        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let managed = crate::session::managed_mcp::ManagedMcpStateHandle::default();
        {
            let mut state = managed.lock().await;
            state.enable_gateway_tools();
            let epoch = state.start_gateway_tool_fetch().unwrap();
            assert!(state.complete_gateway_tool_fetch(
                epoch,
                crate::session::managed_mcp::GatewayToolCatalog {
                    tools: vec![
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "server".to_string(),
                            connector_name: "Gateway Collision".to_string(),
                            tool_id: "tool".to_string(),
                            tool_name: "Collision".to_string(),
                            call_id: "gateway.collision".to_string(),
                            description: "Gateway collision".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "gateway".to_string(),
                            connector_name: "Gateway".to_string(),
                            tool_id: "search".to_string(),
                            tool_name: "Search".to_string(),
                            call_id: "gateway.search".to_string(),
                            description: "Gateway search".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                    ],
                    total_tools: 2,
                    connectors_needing_reauth: vec![],
                    reauth_connectors: vec![],
                }
            ));
        }
        let snapshot = Arc::new(std::sync::Mutex::new(
            crate::session::tool_index::ToolMetadataSnapshot::default(),
        ));
        refresh_mcp_snapshot_for_test(
            bridge,
            mcp_state,
            managed,
            snapshot.clone(),
            std::collections::HashMap::new(),
        )
        .await;
        let snapshot = snapshot.lock().unwrap();
        let names: std::collections::HashSet<&str> = snapshot
            .tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect();
        assert!(names.contains("gateway__search"));
        let server_tool = snapshot
            .tools
            .iter()
            .find(|tool| tool.qualified_name == "server__tool")
            .expect("local MCP tool remains indexed");
        assert_eq!(server_tool.server_name, "server");
        assert_eq!(server_tool.description, "fixture");
    }
    #[tokio::test]
    async fn refresh_snapshot_excludes_disabled_gateway_tools_and_connectors() {
        let bridge = Arc::new(crate::tools::bridge::ToolBridge::for_test());
        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let managed = crate::session::managed_mcp::ManagedMcpStateHandle::default();
        {
            let mut state = managed.lock().await;
            state.enable_gateway_tools();
            let epoch = state.start_gateway_tool_fetch().unwrap();
            assert!(state.complete_gateway_tool_fetch(
                epoch,
                crate::session::managed_mcp::GatewayToolCatalog {
                    tools: vec![
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "linear".to_string(),
                            connector_name: "Linear".to_string(),
                            tool_id: "list_issues".to_string(),
                            tool_name: "List".to_string(),
                            call_id: "linear.list_issues".to_string(),
                            description: "List issues".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "linear".to_string(),
                            connector_name: "Linear".to_string(),
                            tool_id: "create_issue".to_string(),
                            tool_name: "Create".to_string(),
                            call_id: "linear.create_issue".to_string(),
                            description: "Create issue".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "slack".to_string(),
                            connector_name: "Slack".to_string(),
                            tool_id: "search".to_string(),
                            tool_name: "Search".to_string(),
                            call_id: "slack.search".to_string(),
                            description: "Search Slack".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                    ],
                    total_tools: 3,
                    connectors_needing_reauth: vec![],
                    reauth_connectors: vec![],
                }
            ));
        }
        let snapshot = Arc::new(std::sync::Mutex::new(
            crate::session::tool_index::ToolMetadataSnapshot::default(),
        ));
        let disabled: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::from([
                (
                    "linear".to_string(),
                    std::collections::HashSet::from(["linear__create_issue".to_string()]),
                ),
                (
                    crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string(),
                    std::collections::HashSet::from(["slack".to_string()]),
                ),
            ]);
        refresh_mcp_snapshot_for_test(bridge, mcp_state, managed, snapshot.clone(), disabled).await;
        let snapshot = snapshot.lock().unwrap();
        let names: std::collections::HashSet<&str> = snapshot
            .tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect();
        assert!(names.contains("linear__list_issues"));
        assert!(!names.contains("linear__create_issue"));
        assert!(!names.contains("slack__search"));
    }
}
/// ToolBridge must route file operations through the injected FileSystem, not direct disk I/O.
/// When `.with_fs()` is dropped from the builder, tools fall back to LocalFs and ACP client-side enforcement stops working.
#[cfg(test)]
#[path = "acp_session_tests/fs_injection_regression_tests.rs"]
mod fs_injection_regression_tests;
#[cfg(test)]
#[path = "acp_session_tests/interjection_actor_tests.rs"]
mod interjection_actor_tests;
#[cfg(test)]
#[path = "acp_session_tests/observability_bridge_mapping_tests.rs"]
mod observability_bridge_mapping_tests;
#[cfg(test)]
#[path = "acp_session_tests/permission_auto_mode_tests.rs"]
mod permission_auto_mode_tests;
#[cfg(test)]
#[path = "acp_session_tests/permission_prompt_notification_tests.rs"]
mod permission_prompt_notification_tests;
/// Tests that a resume re-parks the parked `exit_plan_mode` approval.
#[cfg(test)]
#[path = "acp_session_tests/plan_approval_resume_tests.rs"]
mod plan_approval_resume_tests;
/// Mixed-batch plan.md write and exit_plan_mode snapshot.
#[cfg(test)]
#[path = "acp_session_tests/plan_exit_batch_barrier_tests.rs"]
mod plan_exit_batch_barrier_tests;
/// Plan-mode edit gate: read-only except the plan file, even under allow-all.
#[cfg(test)]
#[path = "acp_session_tests/plan_mode_edit_gate_tests.rs"]
mod plan_mode_edit_gate_tests;
/// Mid-turn plan-mode toggle: immediate activation and buffered reminder.
#[cfg(test)]
#[path = "acp_session_tests/plan_mode_midturn_tests.rs"]
mod plan_mode_midturn_tests;
#[cfg(test)]
#[path = "acp_session_tests/pre_tool_use_decision_tests.rs"]
mod pre_tool_use_decision_tests;
/// Tests for [`conversation_has_project_instructions`], the idempotence helper that gates the spawn-time AGENTS.md / CLAUDE.md injector.
#[cfg(test)]
#[path = "acp_session_tests/project_instructions_idempotence_tests.rs"]
mod project_instructions_idempotence_tests;
#[cfg(test)]
#[path = "acp_session_tests/prompt_gate_tests.rs"]
mod prompt_gate_tests;
#[cfg(test)]
#[path = "acp_session_tests/prompt_mode_transition_tests.rs"]
mod prompt_mode_transition_tests;
#[cfg(test)]
#[path = "acp_session_tests/prompt_queue_actor_tests.rs"]
mod prompt_queue_actor_tests;
/// Regression coverage for the per-turn `record_token_usage` path.
#[cfg(test)]
#[path = "acp_session_tests/record_response_token_usage_tests.rs"]
mod record_response_token_usage_tests;
#[cfg(test)]
#[path = "acp_session_tests/replay_buffer_send_update_tests.rs"]
mod replay_buffer_send_update_tests;
#[cfg(test)]
#[path = "acp_session_tests/reverse_request_session_id_tests.rs"]
mod reverse_request_session_id_tests;
#[cfg(test)]
#[path = "acp_session_tests/rewind_cross_compaction_tests.rs"]
mod rewind_cross_compaction_tests;
#[cfg(test)]
#[path = "acp_session_tests/rewind_synthetic_turn_tests.rs"]
mod rewind_synthetic_turn_tests;
#[cfg(test)]
#[path = "acp_session_tests/slash_authority_turn_tests.rs"]
mod slash_authority_turn_tests;
/// Pins the `SubagentFinished` usage-fold attribution gate.
#[cfg(test)]
#[path = "acp_session_tests/subagent_usage_fold_tests.rs"]
mod subagent_usage_fold_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn_completion_emit_tests.rs"]
mod turn_completion_emit_tests;
#[cfg(test)]
mod tool_meta_stamp_tests {
    //! Pin the `x.ai/tool` stamps on the harness emission paths.
    //! Those are the early ToolCall registered by `prepare_tool_call` and the permission-request ToolCallUpdate.
    //! (A dropped `stamp_tool_meta` call would regress silently.)
    use super::replay_buffer_send_update_tests::make_replay_send_update_fixture;
    use super::support::test_agent_with_tools;
    use super::*;
    use distill_tools::registry::types::ToolConfig;
    use distill_tools::tool_taxonomy::TOOL_META_KEY;
    use distill_workspace::permission::PermissionCommand;
    use tokio::sync::mpsc;
    fn read_file_call() -> crate::sampling::types::ToolCallResponse {
        crate::sampling::types::ToolCallResponse {
            id: "call-stamp-1".to_string(),
            kind: "function".to_string(),
            function: crate::sampling::types::ToolCallFunction {
                name: "read_file".to_string(),
                arguments: r#"{"target_file":"/tmp/stamp.txt"}"#.to_string(),
            },
        }
    }
    /// The `x.ai/tool` object from an event's `_meta`, if present.
    fn tool_meta(meta: Option<&acp::Meta>) -> Option<&serde_json::Value> {
        meta.and_then(|m| m.get(TOOL_META_KEY))
    }
    #[tokio::test(flavor = "current_thread")]
    async fn prepare_tool_call_stamps_early_tool_call_and_refinement() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut fixture = make_replay_send_update_fixture().await;
                fixture.actor.agent = std::cell::RefCell::new(
                    test_agent_with_tools(vec![ToolConfig::from_id(
                        "Distill:read_file".to_string(),
                    )])
                    .await,
                );
                let prepared = fixture
                    .actor
                    .prepare_tool_call(read_file_call(), &mut Vec::new())
                    .await
                    .expect("prepare_tool_call should not error");
                assert!(prepared.is_ok(), "read_file should prepare cleanly");
                let mut early = None;
                let mut refined = None;
                while let Ok(event) = fixture.event_rx.try_recv() {
                    let SessionEvent::Notification(SessionNotification::Acp(n)) = event else {
                        continue;
                    };
                    match &n.update {
                        acp::SessionUpdate::ToolCall(tc) => early = Some(tc.meta.clone()),
                        acp::SessionUpdate::ToolCallUpdate(tu) => {
                            refined = Some(tu.meta.clone());
                        }
                        _ => {}
                    }
                }
                let early = early.expect("early ToolCall emitted");
                let t = tool_meta(early.as_ref()).expect("early ToolCall carries x.ai/tool");
                assert_eq!(
                    t.pointer("/name").unwrap_or(&serde_json::Value::Null),
                    "read_file"
                );
                assert_eq!(
                    t.pointer("/kind").unwrap_or(&serde_json::Value::Null),
                    "read"
                );
                assert_eq!(
                    t.pointer("/namespace").unwrap_or(&serde_json::Value::Null),
                    "distill"
                );
                assert!(t.get("input").is_none(), "identity-only before parse");
                let refined = refined.expect("refinement ToolCallUpdate emitted");
                let t = tool_meta(refined.as_ref()).expect("refinement carries x.ai/tool");
                assert_eq!(
                    t.pointer("/input/path").unwrap_or(&serde_json::Value::Null),
                    "/tmp/stamp.txt"
                );
            })
            .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn permission_request_update_carries_tool_meta() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut fixture = make_replay_send_update_fixture().await;
                fixture.actor.agent = std::cell::RefCell::new(
                    test_agent_with_tools(vec![ToolConfig::from_id(
                        "Distill:read_file".to_string(),
                    )])
                    .await,
                );
                let (perm_tx, mut perm_rx) = mpsc::unbounded_channel();
                fixture.actor.permissions = PermissionHandle::Actor {
                    cmd_tx: perm_tx,
                    yolo_state: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    auto_state: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    side_query_wired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    yolo_pin: None,
                    deny_read_globs: Arc::new(vec![]),
                    in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    user_prompt_notify: Arc::new(parking_lot::Mutex::new(None)),
                };
                let captured: Arc<tokio::sync::Mutex<Option<acp::ToolCallUpdate>>> =
                    Arc::new(tokio::sync::Mutex::new(None));
                let captured_in_task = captured.clone();
                tokio::task::spawn_local(async move {
                    while let Some(cmd) = perm_rx.recv().await {
                        if let PermissionCommand::Request {
                            request:
                                PermissionRequest {
                                    tool_call_update, ..
                                },
                            respond_to,
                        } = cmd
                        {
                            *captured_in_task.lock().await = Some(tool_call_update);
                            let _ = respond_to.send(
                                distill_workspace::permission::PermissionResolution {
                                    decision: Decision::Allow,
                                    event: None,
                                },
                            );
                        }
                    }
                });
                let prepared = fixture
                    .actor
                    .prepare_tool_call(read_file_call(), &mut Vec::new())
                    .await
                    .expect("prepare_tool_call should not error");
                assert!(prepared.is_ok(), "allowed read_file should prepare cleanly");
                let update = captured
                    .lock()
                    .await
                    .take()
                    .expect("permission request must have been issued");
                let t = tool_meta(update.meta.as_ref())
                    .expect("permission-request ToolCallUpdate carries x.ai/tool");
                assert_eq!(
                    t.pointer("/name").unwrap_or(&serde_json::Value::Null),
                    "read_file"
                );
                assert_eq!(
                    t.pointer("/kind").unwrap_or(&serde_json::Value::Null),
                    "read"
                );
                assert_eq!(
                    t.pointer("/input/path").unwrap_or(&serde_json::Value::Null),
                    "/tmp/stamp.txt"
                );
            })
            .await;
    }
}
/// Drop guard that records aggregate turn metrics on the current tracing span
struct TurnMetrics {
    turn_tool_count: u64,
    turn_model_calls: u64,
    span: tracing::Span,
}
impl TurnMetrics {
    fn new() -> Self {
        Self {
            turn_tool_count: 0,
            turn_model_calls: 0,
            span: tracing::Span::current(),
        }
    }
    fn record_model_response(&mut self, num_tool_calls: usize) {
        self.turn_model_calls += 1;
        self.turn_tool_count += num_tool_calls as u64;
    }
}
impl Drop for TurnMetrics {
    fn drop(&mut self) {
        self.span.record("turn_tool_count", self.turn_tool_count);
        self.span.record("turn_model_calls", self.turn_model_calls);
    }
}
/// Token rotation on the sampler/inference path is owned by the proactive refresh loop and the per-turn `refresh_token_if_expired`.
/// `handle_sampling_failure` passes auth errors to the caller and never invokes the refresher itself.
#[cfg(test)]
#[path = "acp_session_tests/auth_error_no_retry_tests.rs"]
mod auth_error_no_retry_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/auth_retry_budget_tests.rs"]
mod auth_retry_budget_tests;
/// Regression coverage for the auto-wake suppression sweep and shutdown drain.
/// These exercise the helpers added to fix the trailing `<system-reminder>` chat history bug.
#[cfg(test)]
#[path = "acp_session_tests/auto_wake_suppression_tests.rs"]
mod auto_wake_suppression_tests;
#[cfg(test)]
#[path = "acp_session_tests/between_turn_completion_tests.rs"]
mod between_turn_completion_tests;
#[cfg(test)]
#[path = "acp_session_tests/build_tool_parse_error_message_tests.rs"]
mod build_tool_parse_error_message_tests;
#[cfg(test)]
#[path = "acp_session_tests/cancel_running_task_tests.rs"]
mod cancel_running_task_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/chat_history_integrity_tests.rs"]
mod chat_history_integrity_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/disk_full_tests.rs"]
mod disk_full_tests;
#[cfg(test)]
#[path = "acp_session_tests/feedback_turn_lookup_tests.rs"]
mod feedback_turn_lookup_tests;
#[cfg(test)]
#[path = "acp_session_tests/idle_resume_tests.rs"]
mod idle_resume_tests;
#[cfg(test)]
#[path = "acp_session_tests/image_strip_tests.rs"]
mod image_strip_tests;
#[cfg(test)]
#[path = "acp_session_tests/inline_auto_compact_flow_tests.rs"]
mod inline_auto_compact_flow_tests;
#[cfg(test)]
#[path = "acp_session_tests/laziness/laziness_debug_tests.rs"]
mod laziness_debug_tests;
#[cfg(test)]
#[path = "acp_session_tests/laziness/laziness_detector_tests.rs"]
mod laziness_detector_tests;
#[cfg(test)]
#[path = "acp_session_tests/laziness/laziness_integration_tests.rs"]
mod laziness_integration_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/length_salvage_tests.rs"]
mod length_salvage_tests;
#[cfg(test)]
#[path = "acp_session_tests/load_user_prompts_tests.rs"]
mod load_user_prompts_tests;
#[cfg(test)]
#[path = "acp_session_tests/mcp_connecting_reminder_tests.rs"]
mod mcp_connecting_reminder_tests;
#[cfg(test)]
#[path = "acp_session_tests/mcp_failed_reminder_tests.rs"]
mod mcp_failed_reminder_tests;
#[cfg(test)]
#[path = "acp_session_tests/media_gen_auth_retry_tests.rs"]
mod media_gen_auth_retry_tests;
#[cfg(test)]
#[path = "acp_session_tests/media_gen_batch_limit_tests.rs"]
mod media_gen_batch_limit_tests;
#[cfg(test)]
#[path = "acp_session_tests/memory_config_tests.rs"]
mod memory_config_tests;
#[cfg(test)]
#[path = "acp_session_tests/parallel_dispatch_tests.rs"]
mod parallel_dispatch_tests;
#[cfg(test)]
#[path = "acp_session_tests/prompt_context_persistence_tests.rs"]
mod prompt_context_persistence_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/rate_limit_backoff_tests.rs"]
mod rate_limit_backoff_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/sampling_trace_tests.rs"]
mod sampling_trace_tests;
#[cfg(test)]
#[path = "acp_session_tests/session_thread_tests.rs"]
mod session_thread_tests;
#[cfg(test)]
#[path = "acp_session_tests/status_line_payload_tests.rs"]
mod status_line_payload_tests;
#[cfg(test)]
#[path = "acp_session_tests/tool_definitions_artifact_tests.rs"]
mod tool_definitions_artifact_tests;
#[cfg(test)]
#[path = "acp_session_tests/tool_layer_images_bridge_tests.rs"]
mod tool_layer_images_bridge_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/transient_retry_loop_tests.rs"]
mod transient_retry_loop_tests;
/// Turn-level retry on transient sampler failures.
#[cfg(test)]
#[path = "acp_session_tests/transient_retry_tests.rs"]
mod transient_retry_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn/turn_end_guard_tests.rs"]
mod turn_end_guard_tests;
#[cfg(test)]
#[path = "acp_session_tests/turn_end_reporting_tests.rs"]
mod turn_end_reporting_tests;
#[cfg(test)]
#[path = "acp_session_tests/wait_for_mcp_prefix_tests.rs"]
mod wait_for_mcp_prefix_tests;
#[cfg(test)]
#[path = "acp_session_tests/web_search_e2e_tests.rs"]
mod web_search_e2e_tests;
#[cfg(test)]
mod managed_gateway_tool_tests {
    use super::*;
    use distill_tools::types::output::{MCPOutput, ToolOutput};
    use distill_tools::types::tool::{ToolKind, ToolNamespace};
    use distill_tools::types::tool_metadata::ToolMetadata;
    #[derive(Debug)]
    struct FixtureMcpTool;
    impl ToolMetadata for FixtureMcpTool {
        fn kind(&self) -> ToolKind {
            ToolKind::Other
        }
        fn tool_namespace(&self) -> ToolNamespace {
            ToolNamespace::MCP
        }
        fn description_template(&self) -> &str {
            "fixture"
        }
    }
    impl distill_tool_runtime::Tool for FixtureMcpTool {
        type Args = serde_json::Value;
        type Output = ToolOutput;
        fn id(&self) -> distill_tool_protocol::ToolId {
            distill_tool_protocol::ToolId::new("server__tool").expect("valid")
        }
        fn description(
            &self,
            _ctx: &::distill_tool_runtime::ListToolsContext,
        ) -> distill_tool_types::ToolDescription {
            distill_tool_types::ToolDescription::new("server__tool", "fixture")
        }
        async fn run(
            &self,
            _ctx: distill_tool_runtime::ToolCallContext,
            _args: serde_json::Value,
        ) -> Result<ToolOutput, distill_tool_runtime::ToolError> {
            Ok(ToolOutput::MCP(MCPOutput::okay_output(
                "server__tool".to_string(),
                "server".to_string(),
                "ok".to_string(),
            )))
        }
    }
    #[tokio::test]
    async fn refresh_snapshot_seeds_only_admitted_gateway_catalog_entries() {
        let bridge = Arc::new(crate::tools::bridge::ToolBridge::for_test());
        bridge
            .register_mcp_tools(
                "server__tool".to_string(),
                FixtureMcpTool,
                Some(serde_json::json!({"type": "object"})),
            )
            .expect("local fixture registration succeeds");
        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let managed = crate::session::managed_mcp::ManagedMcpStateHandle::default();
        {
            let mut state = managed.lock().await;
            state.enable_gateway_tools();
            let epoch = state.start_gateway_tool_fetch().unwrap();
            assert!(state.complete_gateway_tool_fetch(
                epoch,
                crate::session::managed_mcp::GatewayToolCatalog {
                    tools: vec![
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "server".to_string(),
                            connector_name: "Gateway Collision".to_string(),
                            tool_id: "tool".to_string(),
                            tool_name: "Collision".to_string(),
                            call_id: "gateway.collision".to_string(),
                            description: "Gateway collision".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "gateway".to_string(),
                            connector_name: "Gateway".to_string(),
                            tool_id: "search".to_string(),
                            tool_name: "Search".to_string(),
                            call_id: "gateway.search".to_string(),
                            description: "Gateway search".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                    ],
                    total_tools: 2,
                    connectors_needing_reauth: vec![],
                    reauth_connectors: vec![],
                }
            ));
        }
        let snapshot = Arc::new(std::sync::Mutex::new(
            crate::session::tool_index::ToolMetadataSnapshot::default(),
        ));
        refresh_mcp_snapshot_for_test(
            bridge.clone(),
            mcp_state,
            managed,
            snapshot.clone(),
            std::collections::HashMap::new(),
        )
        .await;
        let catalog = bridge
            .read_resource::<distill_tools::types::resources::ManagedGatewayToolCatalog>()
            .await
            .expect("catalog resource should be seeded");
        assert!(catalog.get("gateway__search").is_some());
        assert!(
            catalog.get("server__tool").is_none(),
            "gateway catalog resource must match admitted snapshot and skip local collisions"
        );
    }
    #[tokio::test]
    async fn refresh_snapshot_excludes_disabled_gateway_tools_and_connectors() {
        let bridge = Arc::new(crate::tools::bridge::ToolBridge::for_test());
        let mcp_state = Arc::new(TokioMutex::new(McpState::new(vec![])));
        let managed = crate::session::managed_mcp::ManagedMcpStateHandle::default();
        {
            let mut state = managed.lock().await;
            state.enable_gateway_tools();
            let epoch = state.start_gateway_tool_fetch().unwrap();
            assert!(state.complete_gateway_tool_fetch(
                epoch,
                crate::session::managed_mcp::GatewayToolCatalog {
                    tools: vec![
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "linear".to_string(),
                            connector_name: "Linear".to_string(),
                            tool_id: "list_issues".to_string(),
                            tool_name: "List".to_string(),
                            call_id: "linear.list_issues".to_string(),
                            description: "List issues".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "linear".to_string(),
                            connector_name: "Linear".to_string(),
                            tool_id: "create_issue".to_string(),
                            tool_name: "Create".to_string(),
                            call_id: "linear.create_issue".to_string(),
                            description: "Create issue".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                        crate::session::managed_mcp::GatewayTool {
                            connector_id: "slack".to_string(),
                            connector_name: "Slack".to_string(),
                            tool_id: "search".to_string(),
                            tool_name: "Search".to_string(),
                            call_id: "slack.search".to_string(),
                            description: "Search Slack".to_string(),
                            json_schema: serde_json::json!({"type": "object"}),
                        },
                    ],
                    total_tools: 3,
                    connectors_needing_reauth: vec![],
                    reauth_connectors: vec![],
                }
            ));
        }
        let snapshot = Arc::new(std::sync::Mutex::new(
            crate::session::tool_index::ToolMetadataSnapshot::default(),
        ));
        let disabled: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::from([
                (
                    "linear".to_string(),
                    std::collections::HashSet::from(["linear__create_issue".to_string()]),
                ),
                (
                    crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string(),
                    std::collections::HashSet::from(["slack".to_string()]),
                ),
            ]);
        refresh_mcp_snapshot_for_test(
            bridge.clone(),
            mcp_state,
            managed,
            snapshot.clone(),
            disabled,
        )
        .await;
        let catalog = bridge
            .read_resource::<distill_tools::types::resources::ManagedGatewayToolCatalog>()
            .await
            .expect("catalog resource should be seeded");
        assert!(catalog.get("linear__list_issues").is_some());
        assert!(catalog.get("linear__create_issue").is_none());
        assert!(catalog.get("slack__search").is_none());
        let snapshot = snapshot.lock().unwrap();
        let names: std::collections::HashSet<&str> = snapshot
            .tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect();
        assert!(names.contains("linear__list_issues"));
        assert!(!names.contains("linear__create_issue"));
        assert!(!names.contains("slack__search"));
    }
}
#[cfg(test)]
#[path = "acp_session_tests/goal/goal_compaction_reseed_tests.rs"]
mod goal_compaction_reseed_tests;
#[cfg(test)]
#[path = "acp_session_tests/goal/goal_planner_e2e_tests.rs"]
mod goal_planner_e2e_tests;
#[cfg(test)]
#[path = "acp_session_tests/interjection_tests.rs"]
mod interjection_tests;
#[cfg(test)]
#[path = "acp_session_tests/recap_display_only_tests.rs"]
mod recap_display_only_tests;
#[cfg(test)]
#[path = "acp_session_tests/reminder_policy_tests.rs"]
mod reminder_policy_tests;
