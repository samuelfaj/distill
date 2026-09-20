// Modified for Distill by Samuel Fajreldines, 2026.
//! Tool-call execution for `SessionActor`: a `#[path]` child of `acp_session`.
use super::hooks::RewriteProblem;
use super::*;
use distill_hooks::result::HookDecision;
use futures::StreamExt;
use tracing::Instrument;
#[path = "wait_interrupt.rs"]
mod wait_interrupt;
use wait_interrupt::{
    InterruptedWaitFilter, WaitInterruptCause, apply_interrupted_wait_filter, finished_wait_ids,
    interrupted_wait_tool_result, record_interruptible_wait_outcome, wait_for_wait_interrupt,
    wait_task_ids_from_args,
};
#[derive(Default)]
struct PreToolUseGate {
    hook_ask: Option<HookAsk>,
    rewrite: Option<GateRewrite>,
    additional_context: Vec<distill_hooks::dispatcher::AdditionalContext>,
}
struct GateRewrite {
    tool_input: ToolInput,
    raw_arguments: String,
    raw_input: serde_json::Value,
    hook_name: String,
}
struct ValidatedRewrite {
    rewritten: ToolInput,
    updated_json: serde_json::Value,
    hook_name: String,
}
/// Whether a tool name is an MCP `create_pull_request` (qualified `server__create_pull_request` or bare).
fn is_mcp_create_pull_request(tool_name: &str) -> bool {
    match crate::session::mcp_servers::parse_mcp_tool_name(tool_name) {
        Some((_, tool)) => tool == "create_pull_request",
        None => tool_name == "create_pull_request",
    }
}
/// An MCP tool that returned an error result routes to `PostToolUseFailure`
/// instead of `PostToolUse`. Built-in logical errors (a non-zero
/// `run_terminal_command` exit, a file-not-found) stay on `PostToolUse`.
fn is_mcp_error_result(output: &ToolsToolOutput) -> bool {
    matches!(output, ToolsToolOutput::MCP(_)) && output.is_error()
}
/// One `tool.execution` span, wrapping a single dispatch attempt.
/// Outcome fields are declared `Empty` here because `record` on a field the span never declared is silently dropped.
/// [`record_tool_span_outcome`] fills them in once the result is known.
fn tool_execution_span(
    parent: &tracing::Span,
    session_id: &str,
    prepared: &PreparedToolCall,
    tool_call_id: &str,
    retry: bool,
) -> tracing::Span {
    let mcp_server = crate::session::mcp_servers::parse_mcp_tool_name(prepared.hook_tool_name())
        .map(|(server, _)| server);
    let span = tracing::info_span!(
        parent: parent,
        "tool.execution",
        session_id = %session_id,
        tool_name = %prepared.tool_name,
        model_id = %prepared.model_id,
        // Same value under both names: `tool_call_id` is the join key, `tool_use_id` is kept for existing queries
        tool_use_id = %tool_call_id,
        tool_call_id = %tool_call_id,
        retry,
        server_name = tracing::field::Empty,
        success = tracing::field::Empty,
        outcome = tracing::field::Empty,
        tool_input_size_bytes = prepared.raw_arguments.len() as i64,
        tool_result_size_bytes = tracing::field::Empty,
    );
    if let Some(id) = mcp_server.as_deref() {
        span.record("server_name", id);
    }
    span
}
/// Stamp the dispatch outcome on `span` and close it.
/// Takes the span by value: these fields are recorded exactly once.
fn record_tool_span_outcome(
    span: tracing::Span,
    result: &Result<ToolRunResult, distill_tool_runtime::ToolError>,
) -> bool {
    let (success, result_size) = match result {
        Ok(tool_result) => (
            !tool_result.output.is_error(),
            tool_result.prompt_text.len() as i64,
        ),
        Err(_) => (false, 0),
    };
    span.record("success", success);
    span.record("outcome", tool_output_span_outcome(result));
    span.record("tool_result_size_bytes", result_size);
    success
}
/// Maps a typed tool result onto the fixed span/log outcome set (`success` / `error` / `unconfirmed`).
/// A delivery the tool could not confirm still dispatched successfully, so it reports `unconfirmed`, never `error`.
pub(super) fn tool_output_span_outcome(
    result: &Result<ToolRunResult, distill_tool_runtime::ToolError>,
) -> &'static str {
    use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageDisposition;
    match result {
        Ok(tool_result) => match &tool_result.output {
            ToolsToolOutput::SendSubagentMessage(output) => match output.disposition() {
                SendSubagentMessageDisposition::Accepted => "success",
                SendSubagentMessageDisposition::Rejected => "error",
                SendSubagentMessageDisposition::Unconfirmed => "unconfirmed",
            },
            output if output.is_error() => "error",
            _ => "success",
        },
        Err(_) => "error",
    }
}
/// Blocking wait tools that should abort when a mid-turn interjection is pending.
fn is_interruptible_wait_tool(tool_name: &str, args: &serde_json::Value) -> bool {
    match tool_name {
        "get_task_output"
        | "get_command_or_subagent_output"
        | "get_task_or_subagent_output"
        | "get_terminal_command_output" => distill_tool_types::task_output_waits_from_json(args),
        "wait_tasks" | "wait_commands_or_subagents" | "wait_tasks_or_subagents" => true,
        "Await" | "AwaitShell" => true,
        _ => false,
    }
}
fn should_flush_held_queue_before_wait(
    has_interruptible_wait: bool,
    steer: bool,
    goal_loop_active: bool,
) -> bool {
    has_interruptible_wait && steer && !goal_loop_active
}
use crate::tools::tool_context::BlockingWaitGuard;
/// Clears `awaiting_plan_approval` (and re-persists) when the [`SessionActor::request_plan_approval`] await resolves or is dropped.
/// Resolve means a decision came back; drop means the model turn was cancelled, so a cancelled in-session approval can never strand the bit `true`.
/// `PlanModeState` writes are immediate (no debounce), so writing `false` here would race the quit and lose the gate.
struct AwaitingApprovalGuard<'a>(&'a SessionActor);
impl AwaitingApprovalGuard<'_> {
    /// Keep `awaiting_plan_approval` set (skip the clear-on-drop).
    /// Used when the client disconnected without answering, so resume re-parks the approval.
    fn disarm(self) {
        std::mem::forget(self);
    }
}
impl Drop for AwaitingApprovalGuard<'_> {
    fn drop(&mut self) {
        self.0.plan_mode.lock().set_awaiting_plan_approval(false);
        self.0.persist_plan_mode_state();
    }
}
/// Outcome of reading the on-disk plan file for the exit-plan intercept path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PlanFileRead {
    Present(String),
    Absent,
    Unreadable,
}
pub(super) fn classify_plan_file_read(result: Result<String, std::io::Error>) -> PlanFileRead {
    match result {
        Ok(text) if !text.trim().is_empty() => PlanFileRead::Present(text),
        Ok(_) => PlanFileRead::Absent,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => PlanFileRead::Absent,
        Err(_) => PlanFileRead::Unreadable,
    }
}
/// Whether to intercept exit-plan tools for client-side plan approval.
/// A mode-switch back to agent with `PlanFileRead::Absent` skips intercept (leaving without approving is allowed).
/// `Present` / `Unreadable` still intercept (an unreadable plan fails closed to an empty approval UI rather than a silent exit).
pub(super) fn should_intercept_exit_plan_approval(
    is_exit_plan_mode: bool,
    is_cursor_switch_to_agent: bool,
    is_cursor_create_plan: bool,
    plan_read: &PlanFileRead,
) -> bool {
    if !is_exit_plan_mode && !is_cursor_switch_to_agent && !is_cursor_create_plan {
        return false;
    }
    if is_cursor_switch_to_agent && matches!(plan_read, PlanFileRead::Absent) {
        return false;
    }
    true
}
/// Whether this tool call exits file-backed plan mode (not inline plan creation).
pub(super) fn is_file_backed_exit_plan_input(tool_input: &ToolInput) -> bool {
    if matches!(tool_input, ToolInput::ExitPlanMode(_)) {
        return true;
    }
    false
}
pub(super) fn is_file_backed_exit_plan_kind(
    kind: Option<distill_tools::types::tool::ToolKind>,
) -> bool {
    matches!(kind, Some(distill_tools::types::tool::ToolKind::ExitPlan))
}
/// Split ExitPlan-kind calls into the tail so they run after the rest of the batch.
fn split_exit_plan_tail(
    calls: Vec<crate::sampling::types::ToolCallResponse>,
    kind_of: impl Fn(&str) -> Option<distill_tools::types::tool::ToolKind>,
) -> (
    Vec<crate::sampling::types::ToolCallResponse>,
    Vec<crate::sampling::types::ToolCallResponse>,
) {
    calls
        .into_iter()
        .partition(|call| !is_file_backed_exit_plan_kind(kind_of(&call.function.name)))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanEditGate {
    /// Execute normally (plan mode inactive, not an edit, or allowed target).
    Allow,
    /// Grok-toolset edit outside the plan file (plan-file-only rule).
    RejectNonPlanFile,
}
/// Compat-toolset `Delete` is not on the markdown carve-out: it maps to `AccessKind::Edit` and is plan-file-only (same as grok edits).
/// `apply_patch` is `AccessKind::Tool` (its files are named inside the patch text) and is always rejected: it could touch anything.
/// `enter_plan_mode` / `exit_plan_mode` map to `AccessKind::Read` and are likewise never gated.
fn access_kind_for_resolved_tool(tool_name: &str, tool_input: &ToolInput) -> AccessKind {
    if tool_name == distill_tools::implementations::distill::SEND_FEEDBACK_TOOL_NAME {
        return match tool_input {
            ToolInput::SendFeedback(_) | ToolInput::Dynamic(_) => {
                AccessKind::Tool("send_feedback".to_owned())
            }
            other => AccessKind::from(other),
        };
    }
    AccessKind::from(tool_input)
}
pub(super) fn plan_mode_edit_gate(
    tracker: &crate::session::plan_mode::PlanModeTracker,
    tool_input: &ToolInput,
    access_kind: &AccessKind,
) -> PlanEditGate {
    if !tracker.is_active() {
        return PlanEditGate::Allow;
    }
    if matches!(tool_input, ToolInput::Task(_)) {
        return PlanEditGate::Allow;
    }
    if matches!(tool_input, ToolInput::ApplyPatch(_)) {
        return PlanEditGate::RejectNonPlanFile;
    }
    match access_kind {
        AccessKind::Edit(path) if !tracker.should_auto_approve_edit(Path::new(path)) => {
            PlanEditGate::RejectNonPlanFile
        }
        _ => PlanEditGate::Allow,
    }
}
/// Typed view of an `exit_plan_mode` approval decision.
/// The wire type (`ExitPlanModeExtResponse`) carries `outcome` as a string.
/// Unknown / unrecognized outcomes map to [`Cancelled`](Self::Cancelled) so the session fails CLOSED (stays in plan mode) rather than auto-approving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanApprovalOutcome {
    Approved,
    Cancelled,
    Abandoned,
}
impl PlanApprovalOutcome {
    fn from_response(
        resp: &distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeExtResponse,
    ) -> Self {
        match resp.outcome.as_str() {
            "approved" => Self::Approved,
            "abandoned" => Self::Abandoned,
            _ => Self::Cancelled,
        }
    }
}
/// Classify an `ext_method` failure.
/// Returns `false` when it was delivered but the client went away before answering (quit / disconnect / leader restart).
/// Any other error (including a non-`acp_send` error) defaults to `false` so the approval is kept pending and never auto-approved.
fn ext_method_no_client(err: &acp::Error) -> bool {
    matches!(
        distill_acp_lib::acp_channel_failure(err),
        Some(distill_acp_lib::AcpChannelFailure::SendFailed)
    )
}
/// CONTENT-gated tool bodies for the external stream. Capture-time cap so
/// multi-MB bodies are not retained; emit still drops them when CONTENT is off.
fn external_tool_bodies(
    result: &Result<ToolRunResult, distill_tool_runtime::ToolError>,
) -> (Option<String>, Option<String>) {
    match result {
        Ok(tool_result) if tool_result.output.is_error() => {
            let body = tool_result.output.to_prompt_format();
            (
                Some(distill_telemetry::external::truncate::cap_bytes(
                    &body,
                    distill_telemetry::external::truncate::MAX_CONTENT_BYTES,
                )),
                Some(distill_telemetry::external::truncate::cap_bytes(
                    &body,
                    distill_telemetry::external::truncate::MAX_TOOL_INPUT_JSON_BYTES,
                )),
            )
        }
        Ok(tool_result) => (
            Some(distill_telemetry::external::truncate::cap_bytes(
                &tool_result.output.to_prompt_format(),
                distill_telemetry::external::truncate::MAX_CONTENT_BYTES,
            )),
            None,
        ),
        Err(e) => (
            None,
            Some(distill_telemetry::external::truncate::cap_bytes(
                &e.to_string(),
                distill_telemetry::external::truncate::MAX_TOOL_INPUT_JSON_BYTES,
            )),
        ),
    }
}
/// Model-facing turn injected after a resumed plan is approved.
const PLAN_APPROVED_IMPLEMENT_MESSAGE: &str =
    "The user approved the plan. Implement the plan in plan.md.";
/// Shared "revise the plan" message for the request-changes outcome, used by both the mid-turn intercept and the resume re-park.
fn revise_plan_message(feedback: &str) -> String {
    let feedback = feedback.trim();
    if feedback.is_empty() {
        "The user wants to revise the plan. \
         Ask the user what changes they would like to make."
            .to_string()
    } else {
        format!("The user wants to revise the plan. The user said:\n{feedback}")
    }
}
/// What the resume re-park does with the user's decision.
/// Extracted from `resume_plan_approval` so the branch logic is unit-testable without driving a real turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ResumeAction {
    /// Approved: leave plan mode and start an implement turn (Agent mode).
    LeaveAndImplement,
    /// Request changes: stay in plan mode and start a revise turn (Plan mode).
    StayAndRevise(String),
    /// Abandoned: leave plan mode and wait for the user (no turn).
    LeaveOnly,
}
fn resume_action_for(outcome: PlanApprovalOutcome, feedback: Option<String>) -> ResumeAction {
    match outcome {
        PlanApprovalOutcome::Approved => ResumeAction::LeaveAndImplement,
        PlanApprovalOutcome::Cancelled => {
            ResumeAction::StayAndRevise(revise_plan_message(feedback.as_deref().unwrap_or("")))
        }
        PlanApprovalOutcome::Abandoned => ResumeAction::LeaveOnly,
    }
}
pub(super) struct BridgeToolSuccess<'a> {
    pub tool_call_id: &'a acp::ToolCallId,
    pub call_id: &'a str,
    pub requested_tool_name: &'a str,
    pub effective_tool_name: &'a str,
    pub drained: DrainedToolSuccess,
    pub concatenated_json_count: usize,
    pub model_id: &'a str,
    pub tool_parsed_args: &'a serde_json::Value,
    pub model_output_override: Option<String>,
}
impl SessionActor {
    /// Merge the canonical `x.ai/tool` identity envelope into a tool-call event's `_meta`, resolving the tool from the live toolset by wire name.
    pub(super) fn stamp_tool_meta(
        &self,
        existing: Option<acp::Meta>,
        wire_name: &str,
        parsed: Option<&ToolInput>,
    ) -> Option<acp::Meta> {
        let toolset = self.agent.borrow().tool_bridge().toolset();
        distill_tools::normalization::merge_tool_meta(
            &toolset,
            existing.map(serde_json::Value::Object),
            wire_name,
            parsed,
        )
        .and_then(|v| v.as_object().cloned())
    }
    #[tracing::instrument(
        name = "tools.execute",
        skip_all,
        fields(
            tool_count = tool_calls.len(),
            model_id,
            session_id = %self.session_info.id.0
        )
    )]
    pub(super) async fn execute_tool_calls(
        &self,
        tool_calls: Vec<crate::sampling::types::ToolCallResponse>,
    ) -> Result<ToolLoop, acp::Error> {
        if let Some(cfg) = self.chat_state_handle.get_sampling_config().await {
            tracing::Span::current().record("model_id", cfg.model.as_str());
        }
        let mut final_result: Option<ToolLoop> = None;
        let mut deferred_followups: Vec<ConversationItem> = Vec::new();
        let tool_calls = self.reject_excess_media_gen_calls(tool_calls).await?;
        if !tool_calls.is_empty() {
            if tool_calls.len() > 1 {
                let kind_of = |name: &str| self.agent.borrow().tool_bridge().tool_kind(name);
                let (body, tail) = split_exit_plan_tail(tool_calls, kind_of);
                if !body.is_empty() {
                    self.execute_tool_calls_batch(body, &mut deferred_followups, &mut final_result)
                        .await?;
                }
                if !tail.is_empty() {
                    self.execute_tool_calls_batch(tail, &mut deferred_followups, &mut final_result)
                        .await?;
                }
            } else {
                self.execute_tool_calls_batch(
                    tool_calls,
                    &mut deferred_followups,
                    &mut final_result,
                )
                .await?;
            }
        }
        {
            let _span = if !deferred_followups.is_empty() {
                Some(
                    tracing::info_span!(
                        "tools.deferred_followups",
                        count = deferred_followups.len()
                    )
                    .entered(),
                )
            } else {
                None
            };
            for chat in deferred_followups {
                self.chat_state_handle.push_user_message(chat);
            }
        }
        self.drain_interjections_at_safe_point().await;
        self.flush_pending_skill_reminders().await;
        if let Some(final_result) = final_result {
            return Ok(final_result);
        }
        Ok(ToolLoop::Continue)
    }
    /// Per-name media-gen counts that exceed this session's cap.
    pub(super) fn media_gen_over_cap(
        &self,
        calls: &[distill_sampling_types::ToolCall],
    ) -> Vec<distill_tools::media_gen_limits::MediaGenOverCap> {
        let kind_of = |name: &str| self.agent.borrow().tool_bridge().tool_kind(name);
        distill_tools::media_gen_limits::over_cap_by_name(
            calls.iter().map(|c| (c.name.as_str(), kind_of(&c.name))),
            &self.rebuild_spec.media_gen_batch_limits,
        )
    }
    /// Every rejected tool_use id still gets a tool_result so the provider batch stays paired.
    /// Registers Pending `ToolCall` then Failed `ToolCallUpdate` (via `handle_tool_not_executed`) so clients do not orphan the update.
    async fn reject_excess_media_gen_calls(
        &self,
        tool_calls: Vec<crate::sampling::types::ToolCallResponse>,
    ) -> Result<Vec<crate::sampling::types::ToolCallResponse>, acp::Error> {
        let kind_of = |name: &str| self.agent.borrow().tool_bridge().tool_kind(name);
        let (allowed, rejected) = distill_tools::media_gen_limits::partition_media_gen_batch(
            tool_calls,
            |c| c.function.name.as_str(),
            |c| kind_of(&c.function.name),
            &self.rebuild_spec.media_gen_batch_limits,
        );
        if rejected.is_empty() {
            return Ok(allowed);
        }
        let rejected_count = rejected.len();
        let mut rejected_by_name: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for (call, reason) in rejected {
            let tool_name = call.function.name.clone();
            *rejected_by_name.entry(tool_name.clone()).or_default() += 1;
            let tool_call_id = acp::ToolCallId::new(std::sync::Arc::from(call.id.clone()));
            let early_raw_input =
                serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok();
            let meta = self.stamp_tool_meta(None, &tool_name, None);
            self.send_update(
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), tool_name.clone())
                        .kind(acp::ToolKind::Other)
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(early_raw_input)
                        .meta(meta),
                ),
                None,
            )
            .await;
            self.handle_tool_not_executed(&call.id, &tool_call_id, reason)
                .await?;
        }
        let limits = &self.rebuild_spec.media_gen_batch_limits;
        for (name, rejected_n) in &rejected_by_name {
            let max = self
                .agent
                .borrow()
                .tool_bridge()
                .tool_kind(name)
                .and_then(|k| distill_tools::media_gen_limits::max_calls_per_batch(k, limits))
                .unwrap_or(0);
            let admitted = allowed.iter().filter(|c| c.function.name == *name).count();
            distill_telemetry::unified_log::info(
                "shell.media_gen.batch_rejected",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "tool_name": name,
                    "total": admitted + rejected_n,
                    "rejected": rejected_n,
                    "max": max,
                    "disposition": "first_k_tail",
                })),
            );
        }
        tracing::warn!(
            session_id = %self.session_info.id.0,
            rejected = rejected_count,
            allowed = allowed.len(),
            "media_gen batch limit rejected tool calls"
        );
        Ok(allowed)
    }
    /// Runs prepare, then dispatch, then post-flight.
    /// Caller owns the outer tail flush.
    async fn execute_tool_calls_batch(
        &self,
        tool_calls: Vec<crate::sampling::types::ToolCallResponse>,
        deferred_followups: &mut Vec<ConversationItem>,
        final_result: &mut Option<ToolLoop>,
    ) -> Result<(), acp::Error> {
        if self.permissions.is_auto_mode() {
            let conversation = self.chat_state_handle.get_conversation().await;
            super::refresh_classifier_transcript(&self.permissions, &conversation);
        }
        let mcp_surface_requested = tool_calls.iter().any(|c| {
            c.function.name == "search_tool"
                || c.function.name == "use_tool"
                || c.function
                    .name
                    .contains(crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER)
        });
        let mut approved: Vec<PreparedToolCall> = Vec::new();
        for call in tool_calls.into_iter() {
            if final_result.is_some() {
                let message = match &*final_result {
                    Some(ToolLoop::PermissionReject { .. }) => {
                        format!(
                            "Tool execution cancelled due to earlier permission rejection for tool `{}`",
                            call.function.name
                        )
                    }
                    Some(ToolLoop::Cancelled) => {
                        format!(
                            "Tool execution cancelled due to earlier user cancellation for tool `{}`",
                            call.function.name
                        )
                    }
                    Some(ToolLoop::FollowupMessage(_)) => {
                        format!(
                            "Tool execution cancelled due to earlier user followup message for tool `{}`",
                            call.function.name
                        )
                    }
                    _ => {
                        format!("Tool execution cancelled for tool `{}`", call.function.name)
                    }
                };
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(call.id.clone(), message));
                continue;
            }
            self.emit_event(crate::session::events::Event::ToolStarted {
                tool_name: call.function.name.clone(),
            });
            self.observability_bridge
                .emit(
                    distill_tool_protocol::session_event::SessionEvent::ToolCallStarted {
                        tool_call_id: call.id.clone(),
                        tool_name: call.function.name.clone(),
                        turn_number: self.current_turn_number.get(),
                    },
                )
                .await;
            let call_name = call.function.name.clone();
            match self.prepare_tool_call(call, deferred_followups).await? {
                Ok(prepared) => approved.push(prepared),
                Err(tool_loop) => {
                    self.events.tool_finished();
                    if let Some((server, tool)) =
                        crate::session::mcp_servers::parse_mcp_tool_name(&call_name)
                    {
                        let error_reason = match &tool_loop {
                            ToolLoop::PermissionReject { reason, .. } => reason.clone(),
                            ToolLoop::Cancelled => "cancelled".to_string(),
                            ToolLoop::FollowupMessage(_) => "followup".to_string(),
                            ToolLoop::HookDenied { hook_name, .. } => {
                                format!("hook_denied:{hook_name}")
                            }
                            other => format!("{other:?}"),
                        };
                        self.emit_event(distill_session_events::Event::McpToolCallCompleted {
                            server_name: server.to_string(),
                            tool_name: tool.to_string(),
                            call_id: format!(
                                "{}{}{}",
                                server,
                                crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER,
                                tool
                            ),
                            duration_ms: 0,
                            success: false,
                            is_timeout: false,
                            error: Some(error_reason),
                            reconnect_attempted: false,
                            auth_retry_attempted: false,
                        });
                    }
                    if matches!(
                        tool_loop,
                        ToolLoop::PermissionReject { .. }
                            | ToolLoop::Cancelled
                            | ToolLoop::FollowupMessage(_)
                    ) && final_result.is_none()
                    {
                        *final_result = Some(tool_loop);
                    }
                }
            }
        }
        if approved.iter().any(|p| p.tool_name == "search_tool") {
            self.retry_auth_required_servers().await;
        }
        if mcp_surface_requested {
            self.retry_unreachable_servers().await;
        }
        let dispatch_cwd = self.tool_context.cwd.to_path_buf();
        let lock_args: Vec<_> = {
            let toolset = self.agent.borrow().tool_bridge().toolset();
            approved
                .iter()
                .map(|prepared| {
                    toolset.remap_params(&prepared.tool_name, prepared.parsed_args.clone())
                })
                .collect()
        };
        let lock_cwd = dispatch_cwd.clone();
        let lock_paths = tokio::task::spawn_blocking(move || {
            lock_args
                .iter()
                .map(|args| lock_path_for_args(args, &lock_cwd))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "file-lock path resolution failed");
            vec![None; approved.len()]
        });
        let write_paths: std::collections::HashSet<_> = approved
            .iter()
            .zip(&lock_paths)
            .filter(|(prepared, _)| !prepared.is_read_only)
            .filter_map(|(_, path)| path.clone())
            .collect();
        let file_locks = {
            let mut map: std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>> =
                std::collections::HashMap::new();
            for path in lock_paths.iter().flatten() {
                if write_paths.contains(path) {
                    map.entry(path.clone())
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())));
                }
            }
            map
        };
        let shared_recovery = Arc::new(tokio::sync::OnceCell::<bool>::const_new());
        let workspace_ops = self.workspace_ops.clone();
        let workflow_smoke_check_cwd = self.tool_context.cwd.to_path_buf();
        let workflow_smoke_check_display_cwd = self.display_cwd.get().map(std::path::PathBuf::from);
        let workflow_smoke_check_session_dir =
            crate::session::persistence::session_dir(&self.session_info);
        let workflow_smoke_check_enabled = self.background_workflows_enabled;
        let (
            workflow_template_param_names,
            workflow_tool_name,
            workflow_script_path_param,
            workflow_validate_only_param,
        ) = {
            let toolset = self.agent.borrow().tool_bridge().toolset();
            let param_names = toolset.template_param_names();
            (
                param_names.clone(),
                toolset
                    .tool_name_for_kind(distill_tools::types::tool::ToolKind::Workflow)
                    .unwrap_or_else(|| "workflow".to_owned()),
                workflow_write_smoke_check::workflow_param_name("script_path", &param_names),
                workflow_write_smoke_check::workflow_param_name("validate_only", &param_names),
            )
        };
        let workflow_smoke_check_indices = workflow_write_smoke_check::last_smoke_check_indices(
            approved.iter().enumerate().map(|(idx, prepared)| {
                let kind = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .tool_kind(&prepared.tool_name);
                let path_param_names = kind
                    .map(|kind| {
                        workflow_write_smoke_check::path_param_names_for_kind(
                            kind,
                            &workflow_template_param_names,
                        )
                    })
                    .unwrap_or_default();
                let path = workflow_write_smoke_check::authored_workflow_arg_path(
                    kind,
                    &prepared.parsed_args,
                    &path_param_names,
                )
                .map(|input| {
                    lock_paths
                        .get(idx)
                        .and_then(|path| path.clone())
                        .unwrap_or_else(|| input.to_owned())
                });
                (idx, path)
            }),
        );
        let workflow_smoke_check_permits = Arc::new(tokio::sync::Semaphore::new(
            workflow_write_smoke_check::MAX_CONCURRENT_CHECKS,
        ));
        if should_flush_held_queue_before_wait(
            approved.iter().any(|prepared| {
                is_interruptible_wait_tool(&prepared.tool_name, &prepared.parsed_args)
            }),
            crate::util::config::follow_up_steer_enabled().await,
            self.goal_loop_active(),
        ) {
            self.promote_queued_as_interjections().await;
        }
        let pending_interjections = self.pending_interjections.clone();
        let (parent_interject, running_turn) = {
            let state = self.state.lock().await;
            (
                state.message_delivery.interject_signal(),
                state.running_task.as_ref().map(|task| task.epoch),
            )
        };
        let session_id: Arc<str> = Arc::from(&*self.session_info.id.0);
        let dispatch_futures: Vec<_> = approved
            .iter()
            .enumerate()
            .map(|(idx, prepared)| {
                let am = self.auth_manager.clone();
                let shared_recovery = Arc::clone(&shared_recovery);
                let workspace_ops = workspace_ops.clone();
                let session_id = session_id.clone();
                let workflow_smoke_check_cwd = workflow_smoke_check_cwd.clone();
                let workflow_smoke_check_display_cwd = workflow_smoke_check_display_cwd.clone();
                let workflow_smoke_check_session_dir = workflow_smoke_check_session_dir.clone();
                let workflow_smoke_check_permits = Arc::clone(&workflow_smoke_check_permits);
                let workflow_smoke_check_this_call = workflow_smoke_check_indices.contains(&idx);
                let workflow_tool_name = workflow_tool_name.clone();
                let workflow_script_path_param = workflow_script_path_param.clone();
                let workflow_validate_only_param = workflow_validate_only_param.clone();
                let pending_interjections = pending_interjections.clone();
                let parent_interject = Arc::clone(&parent_interject);
                let blocking_wait_depth = self.tool_context.blocking_wait_depth.clone();
                let interruptible =
                    is_interruptible_wait_tool(&prepared.tool_name, &prepared.parsed_args);
                let prepared = {
                    let mut dispatch_prepared = prepared.clone();
                    if interruptible
                        && let InterruptedWaitFilter::Rewritten { kept, requested } =
                            apply_interrupted_wait_filter(
                                &blocking_wait_depth,
                                &mut dispatch_prepared.parsed_args,
                            )
                    {
                        tracing::info!(
                            tool = %dispatch_prepared.tool_name,
                            kept = kept.len(),
                            requested,
                            "strip post-interrupt side-work from wait_all"
                        );
                    }
                    Arc::new(dispatch_prepared)
                };
                let authored_tool_kind = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .tool_kind(&prepared.tool_name);
                let authored_path_param_names = authored_tool_kind
                    .map(|kind| {
                        workflow_write_smoke_check::path_param_names_for_kind(
                            kind,
                            &self
                                .agent
                                .borrow()
                                .tool_bridge()
                                .toolset()
                                .template_param_names(),
                        )
                    })
                    .unwrap_or_default();
                let lock = lock_paths
                    .get(idx)
                    .and_then(|p| p.as_ref())
                    .and_then(|path| file_locks.get(path).cloned());
                let tools_execute_span = tracing::Span::current();
                async move {
                    let exec_start = std::time::Instant::now();
                    let tool_span = tool_execution_span(
                        &tools_execute_span,
                        session_id.as_ref(),
                        &prepared,
                        &prepared.call_id,
                        false,
                    );
                    let tool_span_for_record = tool_span.clone();
                    let run_tool = || {
                        let prepared = Arc::clone(&prepared);
                        let workspace_ops = workspace_ops.clone();
                        let session_id = session_id.clone();
                        let lock = lock.clone();
                        let workflow_smoke_check_cwd = workflow_smoke_check_cwd.clone();
                        let workflow_smoke_check_display_cwd =
                            workflow_smoke_check_display_cwd.clone();
                        let workflow_smoke_check_session_dir =
                            workflow_smoke_check_session_dir.clone();
                        let workflow_smoke_check_permits =
                            Arc::clone(&workflow_smoke_check_permits);
                        let authored_path_param_names = authored_path_param_names.clone();
                        let workflow_tool_name = workflow_tool_name.clone();
                        let workflow_script_path_param = workflow_script_path_param.clone();
                        let workflow_validate_only_param = workflow_validate_only_param.clone();
                        async move {
                            let result = {
                                let _guard = if let Some(ref l) = lock {
                                    Some(l.lock().await)
                                } else {
                                    None
                                };
                                dispatch_tool(&workspace_ops, &prepared, &session_id).await
                            };
                            let mut result = result;
                            let snapshot =
                                if workflow_smoke_check_enabled && workflow_smoke_check_this_call {
                                    workflow_write_smoke_check::snapshot_authored_workflow(
                                        authored_tool_kind,
                                        &prepared.parsed_args,
                                        &authored_path_param_names,
                                        &workflow_smoke_check_cwd,
                                        workflow_smoke_check_display_cwd.as_deref(),
                                        &workflow_smoke_check_session_dir,
                                    )
                                    .await
                                } else {
                                    None
                                };
                            let failure = match snapshot {
                                Some(Ok(snapshot)) => {
                                    workflow_write_smoke_check::check_snapshot(
                                        snapshot,
                                        &workflow_smoke_check_permits,
                                    )
                                    .await
                                }
                                Some(Err(failure)) => Some(failure),
                                None => None,
                            };
                            if let (Ok(tool_result), Some(failure)) = (&mut result, failure) {
                                workflow_write_smoke_check::append_validation_warning(
                                    &mut tool_result.prompt_text,
                                    &failure,
                                    &workflow_tool_name,
                                    &workflow_script_path_param,
                                    &workflow_validate_only_param,
                                );
                            }
                            result
                        }
                    };
                    let wait_guard = interruptible
                        .then(|| BlockingWaitGuard::enter(blocking_wait_depth.clone()));
                    let (result, wait_aborted) = if interruptible {
                        async {
                            tokio::select! {
                                biased;
                                result = call_with_auth_retry(
                                    am.as_ref(),
                                    Some(&shared_recovery),
                                    &prepared.tool_name,
                                    run_tool,
                                ) => (result, false),
                                cause = wait_for_wait_interrupt(
                                    &pending_interjections,
                                    &parent_interject,
                                ) => {
                                    tracing::info!(
                                        tool = %prepared.tool_name,
                                        ?cause,
                                        "abort wait tool: interrupt pending"
                                    );
                                    match (cause, running_turn) {
                                        (WaitInterruptCause::ParentInterject, Some(turn)) => {
                                            parent_interject.note_wait_aborted(turn);
                                        }
                                        (WaitInterruptCause::ParentInterject, None)
                                        | (WaitInterruptCause::HumanInterjection, _) => {}
                                    }
                                    (
                                        Ok(interrupted_wait_tool_result(
                                            &prepared.parsed_args,
                                            cause,
                                        )),
                                        true,
                                    )
                                }
                            }
                        }
                        .instrument(tool_span)
                        .await
                    } else {
                        (
                            call_with_auth_retry(
                                am.as_ref(),
                                Some(&shared_recovery),
                                &prepared.tool_name,
                                run_tool,
                            )
                            .instrument(tool_span)
                            .await,
                            false,
                        )
                    };
                    if let Some(guard) = wait_guard.as_ref() {
                        let waited = wait_task_ids_from_args(&prepared.parsed_args);
                        let finished = if wait_aborted {
                            Vec::new()
                        } else {
                            result.as_ref().map(finished_wait_ids).unwrap_or_default()
                        };
                        record_interruptible_wait_outcome(
                            &blocking_wait_depth,
                            guard.generation(),
                            waited,
                            wait_aborted,
                            &finished,
                        );
                    }
                    let duration_ms = exec_start.elapsed().as_millis() as u64;
                    let outcome = tool_output_span_outcome(&result);
                    let success = record_tool_span_outcome(tool_span_for_record, &result);
                    distill_telemetry::unified_log::info(
                        "shell.tool.exec_done",
                        Some(session_id.as_ref()),
                        Some(serde_json::json!({
                            "tool_name": prepared.tool_name.as_str(),
                            "tool_call_id": prepared.call_id.as_str(),
                            "elapsed_ms": duration_ms,
                            "success": success,
                            "outcome": outcome,
                        })),
                    );
                    (idx, result, duration_ms)
                }
            })
            .collect();
        tokio::task::yield_now().await;
        let mut dispatch_stream = futures::stream::FuturesUnordered::new();
        for fut in dispatch_futures {
            dispatch_stream.push(fut);
        }
        let mut approved_slots: Vec<Option<PreparedToolCall>> =
            approved.into_iter().map(Some).collect();
        let (dispatch_tx, mut dispatch_rx) = tokio::sync::mpsc::unbounded_channel::<(
            usize,
            Result<ToolRunResult, distill_tool_runtime::ToolError>,
            u64,
        )>();
        let drainer = tokio::spawn(
            async move {
                while let Some(item) = dispatch_stream.next().await {
                    if dispatch_tx.send(item).is_err() {
                        break;
                    }
                }
            }
            .in_current_span(),
        );
        let _drainer_guard = crate::util::AbortOnDrop(drainer);
        while let Some((idx, result, duration_ms)) = dispatch_rx.recv().await {
            let Some(prepared) = approved_slots.get_mut(idx).and_then(Option::take) else {
                tracing::error!(
                    batch_idx = idx,
                    slots = approved_slots.len(),
                    "dispatch result has no pending approved slot; dropping it"
                );
                continue;
            };
            self.signals_handle().record_tool_call(&prepared.tool_name);
            let tool_call_id = if prepared.call_id.is_empty() {
                tracing::warn!(
                    tool = %prepared.tool_name,
                    batch_idx = idx,
                    "tool call id empty; synthesizing join key"
                );
                format!("missing-call-id-{idx}")
            } else {
                prepared.call_id.clone()
            };
            self.events.tool_started(
                prepared.tool_name.clone(),
                tool_call_id.clone(),
                duration_ms,
            );
            let mut post_tool_use_delivery: Option<PostToolUseDelivery> = None;
            let mut post_tool_use_failure_contexts: Vec<
                distill_hooks::dispatcher::AdditionalContext,
            > = Vec::new();
            let tool_result_size_bytes: Option<u64> = match &result {
                Ok(tool_result) => Some(tool_result.prompt_text.len() as u64),
                Err(_) => None,
            };
            let tool_failed = match &result {
                Ok(tool_result) => {
                    crate::session::telemetry::record_completed_tool_output(
                        &tool_result.output,
                        &prepared.parsed_args,
                        duration_ms,
                    );
                    tool_result.output.is_error()
                }
                Err(_) => true,
            };
            let (ext_tool_output, ext_error_message) = if distill_telemetry::external::is_active() {
                external_tool_bodies(&result)
            } else {
                (None, None)
            };
            let tool_loop = match result {
                Ok(tool_result) => {
                    let effective_tool_name = tool_result
                        .effective_tool_name
                        .clone()
                        .or_else(|| prepared.dispatch_target_name.clone())
                        .unwrap_or_else(|| prepared.tool_name.clone());
                    let drained = DrainedToolSuccess::new(tool_result);
                    let mcp_failed = is_mcp_error_result(drained.output());
                    let (model_output_override, deferred_hook_scrollback) = if mcp_failed {
                        post_tool_use_failure_contexts = self
                            .dispatch_tool_failure(
                                &prepared,
                                drained.output().to_prompt_format(),
                                duration_ms,
                            )
                            .await;
                        (None, None)
                    } else {
                        let (mut delivery, deferred_hook_scrollback) = self
                            .dispatch_post_tool_use_hook(
                                &prepared,
                                drained.output(),
                                Some(duration_ms),
                            )
                            .await;
                        let model_output_override = delivery.model_output.take();
                        post_tool_use_delivery = Some(delivery);
                        (model_output_override, deferred_hook_scrollback)
                    };
                    let bridge_result = self
                        .handle_bridge_tool_success(BridgeToolSuccess {
                            tool_call_id: &prepared.tool_call_id,
                            call_id: &prepared.call_id,
                            requested_tool_name: &prepared.tool_name,
                            effective_tool_name: &effective_tool_name,
                            drained,
                            concatenated_json_count: prepared.concatenated_json_count,
                            model_id: &prepared.model_id,
                            tool_parsed_args: &prepared.parsed_args,
                            model_output_override,
                        })
                        .await;
                    if let Some(scrollback) = deferred_hook_scrollback {
                        self.emit_post_tool_use_scrollback(scrollback).await;
                    }
                    let followups = bridge_result?;
                    deferred_followups.extend(followups);
                    if prepared.tool_name == "search_tool" {
                        let pi = self.chat_state_handle.get_prompt_index().await as i64;
                        self.last_search_prompt_index
                            .store(pi, std::sync::atomic::Ordering::Relaxed);
                    }
                    ToolLoop::Continue
                }
                Err(err) => {
                    let err: anyhow::Error = err.into();
                    let err_followups = self
                        .handle_tool_error(
                            &prepared.tool_call_id,
                            &prepared.call_id,
                            &prepared.tool_name,
                            prepared.dispatch_target_name.as_deref(),
                            &err,
                            &prepared.model_id,
                        )
                        .await;
                    deferred_followups.extend(err_followups);
                    post_tool_use_failure_contexts = self
                        .dispatch_tool_failure(&prepared, format!("{err:#}"), duration_ms)
                        .await;
                    ToolLoop::Continue
                }
            };
            {
                let bridge = self.agent.borrow().tool_bridge().clone();
                if let Some(effects) = bridge.apply_pending_skill_update().await {
                    if let Some(item) = self.wrap_skill_reminder(&effects).await {
                        deferred_followups.push(item);
                    }
                    if effects.send_available_commands {
                        self.send_available_commands_update(AdvertiseTrigger::from(effects.kind))
                            .await;
                    }
                }
            }
            for context in &prepared.additional_context {
                deferred_followups.push(self.wrap_hook_note(
                    distill_hooks::event::HookEventName::PreToolUse,
                    HookNoteKind::Context,
                    &context.hook_name,
                    &context.text,
                ));
            }
            if let Some(delivery) = post_tool_use_delivery {
                for context in &delivery.additional_context {
                    deferred_followups.push(self.wrap_hook_note(
                        distill_hooks::event::HookEventName::PostToolUse,
                        HookNoteKind::Context,
                        &context.hook_name,
                        &context.text,
                    ));
                }
                for block in &delivery.blocks {
                    deferred_followups.push(self.wrap_hook_note(
                        distill_hooks::event::HookEventName::PostToolUse,
                        HookNoteKind::Feedback,
                        &block.hook_name,
                        &block.reason,
                    ));
                }
            }
            for context in &post_tool_use_failure_contexts {
                deferred_followups.push(self.wrap_hook_note(
                    distill_hooks::event::HookEventName::PostToolUseFailure,
                    HookNoteKind::Context,
                    &context.hook_name,
                    &context.text,
                ));
            }
            self.events.tool_finished();
            let tool_outcome = match &tool_loop {
                _ if tool_failed => crate::session::events::ToolOutcome::Error,
                ToolLoop::Continue => crate::session::events::ToolOutcome::Success,
                ToolLoop::PermissionReject { .. } => {
                    crate::session::events::ToolOutcome::PermissionRejected
                }
                ToolLoop::Cancelled => crate::session::events::ToolOutcome::PermissionCancelled,
                ToolLoop::FollowupMessage(_) => crate::session::events::ToolOutcome::Followup,
                ToolLoop::HookDenied { .. } => crate::session::events::ToolOutcome::HookDenied,
                ToolLoop::NonExistingTool | ToolLoop::ToolParsingError => {
                    crate::session::events::ToolOutcome::InvalidTool
                }
            };
            let rewriting_hook = prepared.rewriting_hook.clone();
            let hook_rewrote = rewriting_hook.is_some();
            self.signals_handle().record_tool_duration(
                &prepared.tool_name,
                &tool_call_id,
                duration_ms,
            );
            self.emit_event(crate::session::events::Event::ToolCompleted {
                tool_name: prepared.tool_name.clone(),
                duration_ms,
                outcome: tool_outcome,
                tool_call_id: tool_call_id.clone(),
                source: crate::session::events::ToolCompletedSource::Shell,
                rewriting_hook,
            });
            self.observability_bridge
                .emit(
                    distill_tool_protocol::session_event::SessionEvent::ToolCallCompleted {
                        tool_call_id: prepared.call_id.clone(),
                        tool_name: prepared.tool_name.clone(),
                        duration_ms,
                        outcome: map_tool_outcome(tool_outcome),
                    },
                )
                .await;
            let (ext_file_path, ext_parameters) = if distill_telemetry::external::is_active() {
                let parsed: Option<serde_json::Value> =
                    serde_json::from_str(&prepared.raw_arguments).ok();
                let file_path = parsed.as_ref().and_then(|v| {
                    ["file_path", "target_file", "filePath", "path"]
                        .iter()
                        .find_map(|k| v.get(*k).and_then(|p| p.as_str()))
                        .map(str::to_owned)
                });
                (file_path, parsed)
            } else {
                (None, None)
            };
            distill_telemetry::session_ctx::log_event(
                distill_telemetry::events::ToolCallCompleted {
                    tool_name: prepared.tool_name.clone(),
                    outcome: tool_outcome,
                    hook_rewrote,
                    duration_ms,
                    tool_result_size_bytes,
                    model_id: prepared.model_id.clone(),
                    file_path: ext_file_path,
                    parameters: ext_parameters,
                    tool_use_id: distill_telemetry::external::is_active()
                        .then(|| prepared.call_id.clone()),
                    tool_output: ext_tool_output,
                    error_message: ext_error_message,
                },
            );
            if let Some(artifact) = compaction_artifact_read(&prepared.parsed_args) {
                distill_telemetry::event_span!(
                    "compaction.segment_read",
                    session_id = %self.session_info.id.0,
                    tool_name = %prepared.tool_name,
                    artifact = %artifact,
                    // i64: redact drops u64 (serializes as string)
                    // None means the field is omitted
                    segment_index = artifact.segment_index().map(|i| i as i64),
                    success = tool_outcome.ran_successfully(),
                    duration_ms = duration_ms as i64,
                    tool_result_size_bytes = tool_result_size_bytes.map_or(0, |n| n as i64),
                );
            }
            match &tool_loop {
                ToolLoop::PermissionReject { .. }
                | ToolLoop::Cancelled
                | ToolLoop::FollowupMessage(_) => {
                    if final_result.is_none() {
                        *final_result = Some(tool_loop);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    async fn apply_pre_tool_use_gate(
        &self,
        call: &crate::sampling::types::ToolCallResponse,
        tool_call_id: &acp::ToolCallId,
        resolved_tool_name: &str,
        dispatch_target_name: &Option<String>,
        raw_input: &serde_json::Value,
    ) -> Result<Result<PreToolUseGate, ToolLoop>, acp::Error> {
        let mut envelope = self.make_pre_tool_use_envelope(resolved_tool_name, &call.id, raw_input);
        let mut gate = PreToolUseGate::default();
        let hook_registry_snapshot = self.hook_registry.borrow().clone();
        if let Some(registry) = hook_registry_snapshot {
            let ctx = self.hook_run_ctx();
            let batch = self.announce_hook_run(&registry, &envelope, &ctx);
            let pre_result =
                distill_hooks::dispatcher::dispatch_pre_tool_use(&registry, &envelope, &ctx).await;
            self.send_hook_execution(&batch, &pre_result.results).await;
            self.emit_hook_executed_telemetry(
                "pre_tool_use",
                Some(resolved_tool_name),
                &pre_result.results,
            )
            .await;
            match pre_result.decision {
                HookDecision::Deny { reason, hook_name } => {
                    return Ok(Err(self
                        .deny_tool(
                            &call.id,
                            tool_call_id,
                            resolved_tool_name,
                            hook_name,
                            reason,
                        )
                        .await?));
                }
                HookDecision::Ask { reason, hook_name } => {
                    gate.hook_ask = Some(HookAsk { hook_name, reason });
                }
                HookDecision::Defer { hook_name } => {
                    tracing::warn!(
                        %hook_name,
                        tool = %resolved_tool_name,
                        "PreToolUse hook returned 'defer'; continuing through the normal permission flow"
                    );
                }
                HookDecision::Allow => {}
            }
            gate.additional_context = pre_result.additional_context;
            if let Some(rewrite) = pre_result.updated_input {
                let validated = match self
                    .validate_hook_rewrite(
                        call,
                        tool_call_id,
                        resolved_tool_name,
                        dispatch_target_name,
                        rewrite,
                    )
                    .await?
                {
                    Ok(validated) => validated,
                    Err(blocked) => return Ok(Err(blocked)),
                };
                envelope = self.make_pre_tool_use_envelope(
                    resolved_tool_name,
                    &call.id,
                    &validated.updated_json,
                );
                gate.rewrite = Some(GateRewrite {
                    tool_input: validated.rewritten,
                    raw_arguments: validated.updated_json.to_string(),
                    raw_input: validated.updated_json,
                    hook_name: validated.hook_name,
                });
            }
        }
        if let Some(denied) = self
            .run_pre_tool_use_client_hook(call, tool_call_id, &envelope)
            .await?
        {
            return Ok(Err(denied));
        }
        Ok(Ok(gate))
    }
    async fn validate_hook_rewrite(
        &self,
        call: &crate::sampling::types::ToolCallResponse,
        tool_call_id: &acp::ToolCallId,
        resolved_tool_name: &str,
        dispatch_target_name: &Option<String>,
        rewrite: distill_hooks::dispatcher::InputRewrite,
    ) -> Result<Result<ValidatedRewrite, ToolLoop>, acp::Error> {
        let updated_json = serde_json::Value::Object(rewrite.input);
        let rewritten = match self
            .tool_bridge_handle()
            .try_parse(&call.function.name, updated_json.clone())
            .await
        {
            Ok(input) => input,
            Err(err) => {
                let schema_error = distill_hooks::event::clip_reason(&err.to_string());
                return Ok(Err(self
                    .block_unusable_rewrite(
                        &call.id,
                        tool_call_id,
                        resolved_tool_name,
                        rewrite.hook_name,
                        RewriteProblem::FailsSchema(schema_error),
                    )
                    .await?));
            }
        };
        if rewritten.dispatch_target_name() != *dispatch_target_name {
            return Ok(Err(self
                .block_unusable_rewrite(
                    &call.id,
                    tool_call_id,
                    resolved_tool_name,
                    rewrite.hook_name,
                    RewriteProblem::RetargetsCall,
                )
                .await?));
        }
        Ok(Ok(ValidatedRewrite {
            rewritten,
            updated_json,
            hook_name: rewrite.hook_name,
        }))
    }
    pub(crate) async fn prepare_tool_call(
        &self,
        call: crate::sampling::types::ToolCallResponse,
        deferred_followups: &mut Vec<ConversationItem>,
    ) -> Result<Result<PreparedToolCall, ToolLoop>, acp::Error> {
        let tool_call_id = acp::ToolCallId::new(Arc::from(call.id.clone()));
        let model_id_str = self.current_model_id().await;
        tracing::info!(
            "Model requesting tool: name='{}', call_id='{}'",
            call.function.name,
            call.id,
        );
        {
            let _span = tracing::info_span!("tool.register").entered();
            let early_raw_input =
                serde_json::from_str::<serde_json::Value>(&call.function.arguments).ok();
            let subagent_background =
                distill_tools::is_task_tool_id(&call.function.name).then(|| {
                    early_raw_input
                        .as_ref()
                        .and_then(|v| v.get("run_in_background").or_else(|| v.get("background")))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true)
                });
            let mut meta = self.stamp_tool_meta(None, &call.function.name, None);
            if let Some(bg) = subagent_background {
                meta.get_or_insert_with(serde_json::Map::new).insert(
                    "subagentBackground".to_string(),
                    serde_json::Value::Bool(bg),
                );
            }
            self.send_update(
                acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(tool_call_id.clone(), call.function.name.clone())
                        .kind(acp::ToolKind::Other)
                        .status(acp::ToolCallStatus::Pending)
                        .raw_input(early_raw_input)
                        .meta(meta),
                ),
                None,
            )
            .await;
        }
        if let Some((mcp_server, _)) = parse_mcp_tool_name(&call.function.name)
            && !mcp_server_dispatchable(&self.mcp_state, &mcp_server).await
        {
            let err =
                anyhow::anyhow!("Tool not available. Use search_tool to find available tools.");
            let followups = self
                .handle_tool_error(
                    &tool_call_id,
                    &call.id,
                    &call.function.name,
                    None,
                    &err,
                    &model_id_str,
                )
                .await;
            deferred_followups.extend(followups);
            return Ok(Err(ToolLoop::NonExistingTool));
        }
        let args_str = crate::session::helpers::tool_input_parsing::normalize_empty_arguments(
            &call.function.arguments,
        );
        let parse_result = serde_json::from_str::<serde_json::Value>(args_str);
        let mut concatenated_json_count: usize = 0;
        let mut raw_input = match &parse_result {
            Ok(value) => value.clone(),
            Err(e) => {
                if let Some(objects) = crate::session::helpers::tool_input_parsing::try_extract_concatenated_json_objects(
                    &call.function.arguments,
                ) {
                    let total_count = objects.len();
                    if objects.is_empty() {
                        json!({ "raw": call.function.arguments.clone() })
                    } else {
                        let best_match = objects
                            .first()
                            .cloned()
                            .unwrap_or_else(|| {
                                json!({ "raw": call.function.arguments.clone() })
                            });
                        let mut selected_index = 0;
                        let mut matched_tool = false;
                        let bridge = self.agent.borrow().tool_bridge().clone();
                        for (idx, obj) in objects.iter().enumerate() {
                            if bridge
                                .try_parse(&call.function.name, obj.clone())
                                .await
                                .is_ok()
                            {
                                selected_index = idx;
                                matched_tool = true;
                                break;
                            }
                        }
                        tracing::warn!(
                            tool_name = %call.function.name,
                            call_id = %call.id,
                            total_objects = total_count,
                            selected_index,
                            matched_named_tool = matched_tool,
                            "Detected concatenated JSON in tool arguments — \
                            extracting best matching object (index {selected_index}/{total_count}). \
                            The model should use separate tool calls instead of \
                            concatenating JSON objects."
                        );
                        concatenated_json_count = total_count;
                        best_match
                    }
                } else {
                    tracing::warn!(
                        "Failed to parse arguments as JSON ({}), wrapping in 'raw' field",
                        e
                    );
                    json!({ "raw": call.function.arguments.clone() })
                }
            }
        };
        let parsed = self
            .tool_bridge_handle()
            .try_parse(&call.function.name, raw_input.clone())
            .await;
        let mut tool_input = match parsed {
            Ok(input) => input,
            Err(err) => {
                self.handle_tool_parse_error(
                    &tool_call_id,
                    &call.id,
                    &call.function.name,
                    err,
                    &call.function.arguments,
                    &model_id_str,
                )
                .await?;
                return Ok(Err(ToolLoop::ToolParsingError));
            }
        };
        let _recovered_raw_input = if concatenated_json_count > 0 {
            Some(raw_input.clone())
        } else {
            None
        };
        let dispatch_target_name = tool_input.dispatch_target_name();
        let resolved_tool_name = dispatch_target_name
            .clone()
            .unwrap_or_else(|| call.function.name.clone());
        let mut raw_arguments = call.function.arguments.clone();
        let mut rewriting_hook: Option<String> = None;
        let mut hook_ask: Option<HookAsk> = None;
        let mut hook_additional_context = Vec::new();
        if self.may_have_hooks_for(distill_hooks::event::HookEventName::PreToolUse) {
            let gate = match self
                .apply_pre_tool_use_gate(
                    &call,
                    &tool_call_id,
                    &resolved_tool_name,
                    &dispatch_target_name,
                    &raw_input,
                )
                .await?
            {
                Ok(gate) => gate,
                Err(blocked) => return Ok(Err(blocked)),
            };
            hook_ask = gate.hook_ask;
            hook_additional_context = gate.additional_context;
            if let Some(rewrite) = gate.rewrite {
                tool_input = rewrite.tool_input;
                raw_arguments = rewrite.raw_arguments;
                raw_input = rewrite.raw_input;
                concatenated_json_count = 0;
                rewriting_hook = Some(rewrite.hook_name);
            }
        }
        let access_kind = access_kind_for_resolved_tool(&resolved_tool_name, &tool_input);
        let plan_gate = plan_mode_edit_gate(&self.plan_mode.lock(), &tool_input, &access_kind);
        if plan_gate != PlanEditGate::Allow {
            distill_telemetry::event_span!(
                "tool.decision",
                tool_name = %call.function.name,
                tool_use_id = %call.id,
                model_id = %model_id_str,
                decision = "deny",
                source = "plan_mode",
                wait_ms = 0_i64,
            );
            let msg = self.plan_mode_edit_rejected_message().await;
            self.handle_tool_not_executed(&call.id, &tool_call_id, msg)
                .await?;
            return Ok(Err(ToolLoop::Continue));
        }
        let tool_call_display = self
            .send_tool_call_start(&tool_call_id, &call.function.name, tool_input.clone())
            .await;
        let plan_file_auto_approve = match &access_kind {
            AccessKind::Edit(path) if hook_ask.is_none() => self
                .plan_mode
                .lock()
                .should_auto_approve_edit(std::path::Path::new(path)),
            _ => false,
        };
        if plan_file_auto_approve {
            distill_telemetry::event_span!(
                "tool.decision",
                tool_name = %call.function.name,
                tool_use_id = %call.id,
                model_id = %model_id_str,
                decision = "allow",
                source = "config",
                wait_ms = 0_i64,
            );
        }
        if !plan_file_auto_approve {
            let (perm_title, perm_kind, perm_raw_input) = tool_call_display
                .as_ref()
                .map(|(t, k, r)| (Some(t.clone()), Some(*k), Some(r.clone())))
                .unwrap_or((None, None, None));
            let tool_call_update = acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .title(perm_title)
                    .kind(perm_kind)
                    .raw_input(perm_raw_input),
            )
            .meta(self.stamp_tool_meta(None, &call.function.name, Some(&tool_input)));
            let (telemetry_access_kind, _access_detail) = match &access_kind {
                distill_workspace::permission::AccessKind::Read(p) => (
                    distill_telemetry::events::AccessKind::Read,
                    p.clone().unwrap_or_default(),
                ),
                distill_workspace::permission::AccessKind::Edit(p) => {
                    (distill_telemetry::events::AccessKind::Edit, p.clone())
                }
                distill_workspace::permission::AccessKind::Bash(cmd) => {
                    (distill_telemetry::events::AccessKind::Bash, cmd.clone())
                }
                distill_workspace::permission::AccessKind::Grep { path, glob } => (
                    distill_telemetry::events::AccessKind::Grep,
                    path.clone().or_else(|| glob.clone()).unwrap_or_default(),
                ),
                distill_workspace::permission::AccessKind::MCPTool { name, .. } => {
                    (distill_telemetry::events::AccessKind::Mcp, name.clone())
                }
                distill_workspace::permission::AccessKind::WebFetch(u) => {
                    (distill_telemetry::events::AccessKind::Web, u.clone())
                }
                distill_workspace::permission::AccessKind::WebSearch(q) => {
                    (distill_telemetry::events::AccessKind::Web, q.clone())
                }
                distill_workspace::permission::AccessKind::AgentMessage { subagent_id } => (
                    distill_telemetry::events::AccessKind::AgentMessage,
                    subagent_id.clone(),
                ),
                _ => (distill_telemetry::events::AccessKind::Other, String::new()),
            };
            let canonical_permission_tool_name =
                crate::session::telemetry::canonical_permission_tool_name(&access_kind);
            let subagent_session_id = if self.startup_hints.is_subagent {
                Some(self.session_id_string())
            } else {
                None
            };
            let perm_mode = if self.permissions.is_yolo_mode() {
                distill_telemetry::enums::PermissionMode::AlwaysApprove
            } else if self.permissions.is_auto_mode() {
                distill_telemetry::enums::PermissionMode::Auto
            } else {
                distill_telemetry::enums::PermissionMode::Ask
            };
            distill_telemetry::session_ctx::log_event(
                distill_telemetry::events::PermissionPrompted {
                    tool_name: canonical_permission_tool_name.clone(),
                    access_kind: telemetry_access_kind,
                    permission_mode: perm_mode,
                    subagent_session_id: subagent_session_id.clone(),
                    subagent_type: None,
                },
            );
            let perm_start = self.events.permission_requested(&call.function.name);
            debug_assert!(
                !self.session_info.id.0.is_empty(),
                "permission reverse-request must carry a non-empty sessionId (design §5.4)"
            );
            let path_context = Some(distill_workspace::permission::types::RequestPathContext {
                real_cwd: std::path::PathBuf::from(self.session_info.cwd.as_str()),
                display_cwd: self
                    .display_cwd
                    .get()
                    .map(|cwd| std::path::PathBuf::from(cwd.as_str())),
            });
            let perm_wait_start = std::time::Instant::now();
            let perm_wait_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
                "permission.wait",
                wait_ms = tracing::field::Empty
            ));
            let resolution = {
                let _pending_guard =
                    crate::session::pending_interaction::PendingInteractionGuard::new(
                        self.pending_interactions.clone(),
                        self.notifications.gateway.clone(),
                        self.session_info.id.clone(),
                        tool_call_id.to_string(),
                        crate::session::pending_interaction::PendingKind::Permission,
                    );
                self.permissions
                    .request(PermissionRequest {
                        path_context,
                        session_id: Some(self.session_info.id.0.to_string()),
                        hook_ask,
                        ..PermissionRequest::new(access_kind.clone(), tool_call_update)
                    })
                    .await
            };
            perm_wait_span
                .span()
                .record("wait_ms", perm_wait_start.elapsed().as_millis() as i64);
            perm_wait_span.close();
            let manager_event = resolution.event;
            let decision = resolution.decision;
            self.events.permission_resolved(
                &call.function.name,
                match &decision {
                    Decision::Allow | Decision::Ask => {
                        distill_session_events::types::PermissionDecision::Allow
                    }
                    Decision::Reject(_) | Decision::PolicyDeny(_) => {
                        distill_session_events::types::PermissionDecision::Deny
                    }
                    Decision::Cancelled => {
                        distill_session_events::types::PermissionDecision::Cancelled
                    }
                    Decision::FollowupMessage(_) => {
                        distill_session_events::types::PermissionDecision::Followup
                    }
                },
                perm_start,
            );
            let shell_wait_ms = perm_start.elapsed().as_millis() as u64;
            let decision_outcome = crate::session::telemetry::permission_outcome(&decision);
            let resolved = crate::session::telemetry::resolved_decision_telemetry(
                manager_event.as_ref(),
                &decision,
                perm_mode,
                shell_wait_ms,
                self.permissions.is_yolo_mode(),
            );
            distill_telemetry::event_span!(
                "tool.decision",
                tool_name = %call.function.name,
                tool_use_id = %call.id,
                model_id = %model_id_str,
                decision = decision_outcome.as_ref(),
                source = resolved.source.as_deref().unwrap_or(""),
                wait_ms = resolved.wait_ms as i64,
            );
            distill_telemetry::session_ctx::log_event({
                let payload = crate::session::telemetry::permission_decision_payload(
                    canonical_permission_tool_name,
                    telemetry_access_kind,
                    &decision,
                    subagent_session_id.clone(),
                    manager_event.as_ref(),
                    resolved,
                );
                let tool_input = if distill_telemetry::external::is_active() {
                    distill_telemetry::events::ExternalToolInput {
                        parameters: Some(raw_input.clone()),
                        tool_use_id: Some(call.id.clone()),
                    }
                } else {
                    distill_telemetry::events::ExternalToolInput::default()
                };
                distill_telemetry::events::PermissionDecisionRecord {
                    payload,
                    tool_input,
                }
            });
            match decision {
                Decision::PolicyDeny(ref reason) | Decision::Reject(ref reason) => {
                    let is_policy_deny = matches!(&decision, Decision::PolicyDeny(_));
                    let message = if is_policy_deny {
                        format!("Tool `{}` was not executed: {reason}", call.function.name)
                    } else {
                        format!("{reason} for tool `{}`", call.function.name)
                    };
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    let (tool_input_value, tool_input_truncated) =
                        distill_hooks::event::truncate_payload(raw_input.clone());
                    self.dispatch_hook(
                        distill_hooks::event::HookEventName::PermissionDenied,
                        distill_hooks::event::HookPayload::PermissionDenied {
                            tool_name: resolved_tool_name.clone(),
                            tool_use_id: tool_call_id.to_string(),
                            tool_input: tool_input_value,
                            tool_input_truncated,
                        },
                        None,
                    )
                    .await;
                    let loop_action = if is_policy_deny {
                        ToolLoop::Continue
                    } else {
                        ToolLoop::PermissionReject {
                            tool_name: call.function.name.clone(),
                            reason: reason.clone(),
                        }
                    };
                    return Ok(Err(loop_action));
                }
                Decision::Cancelled => {
                    let message = format!(
                        "User cancelled the execution for tool `{}`",
                        call.function.name
                    );
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    return Ok(Err(ToolLoop::Cancelled));
                }
                Decision::FollowupMessage(followup_message) => {
                    let message = format!(
                        "The user elected to avoid running the {} tool. The tool was not executed. \
                         Please refer to the user's message for next steps.",
                        call.function.name
                    );
                    self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                        .await?;
                    return Ok(Err(ToolLoop::FollowupMessage(followup_message)));
                }
                Decision::Allow | Decision::Ask => {}
            }
        }
        let is_exit_plan_mode = matches!(&tool_input, ToolInput::ExitPlanMode(_));
        let is_file_backed_exit = is_file_backed_exit_plan_input(&tool_input);
        let is_cursor_switch_to_agent = false;
        let is_cursor_create_plan = false;
        let plan_file_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        let plan_read = if is_file_backed_exit || is_cursor_create_plan {
            let inline_cursor_plan: Option<PlanFileRead> = None;
            if let Some(plan) = inline_cursor_plan {
                plan
            } else {
                let io_result = tokio::fs::read_to_string(&plan_file_path).await;
                if let Err(ref e) = io_result
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        path = %plan_file_path.display(),
                        error = %e,
                        "[exit_plan_mode] plan file unreadable; intercepting anyway"
                    );
                }
                classify_plan_file_read(io_result)
            }
        } else {
            PlanFileRead::Absent
        };
        let plan_content = match &plan_read {
            PlanFileRead::Present(s) => Some(s.clone()),
            PlanFileRead::Absent | PlanFileRead::Unreadable => None,
        };
        if should_intercept_exit_plan_approval(
            is_exit_plan_mode,
            is_cursor_switch_to_agent,
            is_cursor_create_plan,
            &plan_read,
        ) {
            tracing::info!(
                tool_call_id = %tool_call_id,
                cursor_create_plan = is_cursor_create_plan,
                cursor_switch_to_agent = is_cursor_switch_to_agent,
                has_plan_content = plan_content.is_some(),
                "[exit_plan_mode] intercepted, sending ext_method to client"
            );
            let resp = self
                .request_plan_approval(&tool_call_id, plan_content.clone())
                .await;
            match resp {
                Ok(parsed) => match PlanApprovalOutcome::from_response(&parsed) {
                    PlanApprovalOutcome::Abandoned => {
                        tracing::info!("[exit_plan_mode] user abandoned plan — deactivating");
                        self.leave_plan_mode_to_default();
                        let message = format!(
                            "The user chose to abandon the plan entirely (via the Abandon option in the plan approval dialog). Plan mode has been disabled. Do not call {} again unless the user explicitly asks to re-enter plan mode.",
                            call.function.name
                        );
                        let tool_update = acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            acp::ToolCallUpdateFields::new()
                                .status(Some(acp::ToolCallStatus::Completed))
                                .content(Some(vec![acp::ToolCallContent::from(
                                    acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                                )])),
                        );
                        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                            .await;
                        let tool_chat = ConversationItem::tool_result(call.id.clone(), message);
                        self.chat_state_handle.push_tool_result(tool_chat);
                        return Ok(Err(ToolLoop::Continue));
                    }
                    PlanApprovalOutcome::Cancelled => {
                        let message = if plan_content.is_some() {
                            revise_plan_message(parsed.feedback.as_deref().unwrap_or(""))
                        } else {
                            "The user does not want to exit plan mode. \
                             Continue planning and ask the user what they would like to do."
                                .to_string()
                        };
                        let tool_update = acp::ToolCallUpdate::new(
                            tool_call_id.clone(),
                            acp::ToolCallUpdateFields::new()
                                .status(Some(acp::ToolCallStatus::Completed))
                                .content(Some(vec![acp::ToolCallContent::from(
                                    acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                                )])),
                        );
                        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                            .await;
                        let tool_chat = ConversationItem::tool_result(call.id.clone(), message);
                        self.chat_state_handle.push_tool_result(tool_chat);
                        return Ok(Err(ToolLoop::Continue));
                    }
                    PlanApprovalOutcome::Approved => {
                        tracing::info!("[exit_plan_mode] user approved — executing tool");
                    }
                },
                Err(err) => {
                    if ext_method_no_client(&err) {
                        tracing::debug!(%err, "exit_plan_mode: no client wired; executing tool");
                    } else {
                        tracing::info!(
                            %err,
                            "exit_plan_mode: client disconnected mid-approval; plan mode stays active"
                        );
                        let message = "Plan approval could not be completed because the \
                             client disconnected. Plan mode remains active; the approval \
                             will reappear on reconnect."
                            .to_string();
                        self.handle_tool_not_executed(&call.id, &tool_call_id, message)
                            .await?;
                        return Ok(Err(ToolLoop::Cancelled));
                    }
                }
            }
        } else if is_cursor_switch_to_agent {
            tracing::info!(
                tool_call_id = %tool_call_id,
                "[exit_plan_mode] cursor SwitchMode(agent) with empty plan — skipping intercept"
            );
        }
        let is_read_only = self
            .agent
            .borrow()
            .tool_bridge()
            .tool_kind(&call.function.name)
            .map(|k| {
                use distill_tools::types::tool::ToolKind;
                matches!(
                    k,
                    ToolKind::Read
                        | ToolKind::Search
                        | ToolKind::Lsp
                        | ToolKind::ListDir
                        | ToolKind::List
                        | ToolKind::MemorySearch
                        | ToolKind::MemoryGet
                        | ToolKind::WebSearch
                        | ToolKind::WebFetch
                        | ToolKind::EnterPlan
                        | ToolKind::ExitPlan
                        | ToolKind::AskUser
                )
            })
            .unwrap_or(false);
        let prepared = PreparedToolCall {
            call_id: call.id.clone(),
            tool_call_id,
            tool_name: call.function.name.clone(),
            raw_arguments,
            parsed_args: raw_input.clone(),
            model_id: model_id_str,
            concatenated_json_count,
            dispatch_target_name,
            is_read_only,
            rewriting_hook,
            additional_context: hook_additional_context,
        };
        Ok(Ok(prepared))
    }
    /// Issue the `x.ai/exit_plan_mode` reverse-request and await the user's decision.
    /// Shared by the mid-turn intercept and the resume re-park.
    /// Marks `awaiting_plan_approval` while the request is outstanding and clears it on every exit path via [`AwaitingApprovalGuard`].
    pub(super) async fn request_plan_approval(
        &self,
        tool_call_id: &acp::ToolCallId,
        plan_content: Option<String>,
    ) -> Result<
        distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeExtResponse,
        acp::Error,
    > {
        use agent_client_protocol::Client as _;
        use distill_tools::implementations::distill::exit_plan_mode::{
            ExitPlanModeExtRequest, ExitPlanModeExtResponse,
        };
        let ext_req = ExitPlanModeExtRequest {
            session_id: self.session_id_string(),
            tool_call_id: tool_call_id.to_string(),
            plan_content,
        };
        debug_assert!(
            !ext_req.session_id.is_empty(),
            "exit_plan_mode reverse-request must carry a non-empty sessionId (design §5.4)"
        );
        let ext_request = acp::ExtRequest::new(
            "x.ai/exit_plan_mode",
            serde_json::value::to_raw_value(&ext_req)
                .expect("ExitPlanModeExtRequest serialization should not fail")
                .into(),
        );
        self.dispatch_permission_prompt_notification("Plan approval requested")
            .await;
        self.plan_mode.lock().set_awaiting_plan_approval(true);
        self.persist_plan_mode_state();
        let clear_awaiting = AwaitingApprovalGuard(self);
        let resp = {
            let _pending_guard = crate::session::pending_interaction::PendingInteractionGuard::new(
                self.pending_interactions.clone(),
                self.notifications.gateway.clone(),
                self.session_info.id.clone(),
                tool_call_id.to_string(),
                crate::session::pending_interaction::PendingKind::PlanApproval,
            );
            self.notifications.gateway.ext_method(ext_request).await
        };
        let raw = match resp {
            Ok(raw) => raw,
            Err(err) => {
                clear_awaiting.disarm();
                return Err(err);
            }
        };
        Ok(
            serde_json::from_str::<ExitPlanModeExtResponse>(raw.0.get()).unwrap_or_else(|_| {
                ExitPlanModeExtResponse {
                    outcome: "cancelled".into(),
                    feedback: None,
                }
            }),
        )
    }
    /// Leave plan mode (approved/abandoned) and tell the client to show the Default mode.
    /// Mirrors the mid-turn exit so the resume re-park drives the mode change through the same path.
    fn leave_plan_mode_to_default(&self) {
        let deactivated = self.plan_mode.lock().deactivate_approved();
        if deactivated {
            *self.current_prompt_mode.lock() = PromptMode::Agent;
            *self.turn_prompt_mode.lock() = PromptMode::Agent;
            self.persist_plan_mode_state();
            self.enqueue_current_mode_update(acp::SessionModeId::new(
                distill_tools::types::SessionMode::Default.as_id(),
            ));
        }
    }
    /// Resume hook: re-issue the parked `exit_plan_mode` approval after a session restored with `awaiting_plan_approval == true`.
    /// The client then re-shows approval chrome over a real live waiter.
    /// Handles the decision with no in-flight turn.
    pub(super) async fn resume_plan_approval(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        if !self.plan_mode.lock().is_awaiting_plan_approval() {
            return;
        }
        if crate::session::pending_interaction::has_parked_plan_approval(&self.pending_interactions)
        {
            tracing::debug!("[exit_plan_mode] resume: approval already pending; skip re-park");
            return;
        }
        let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        let plan_content = match tokio::fs::read_to_string(&plan_path).await {
            Ok(s) if !s.trim().is_empty() => s,
            _ => {
                tracing::info!("[exit_plan_mode] resume: no plan.md; clearing awaiting flag");
                self.plan_mode.lock().set_awaiting_plan_approval(false);
                self.persist_plan_mode_state();
                return;
            }
        };
        let tool_call_id = acp::ToolCallId::new(Arc::from(
            format!("exit-plan-mode-resume-{}", self.session_info.id.0).as_str(),
        ));
        tracing::info!(
            tool_call_id = %tool_call_id,
            "[exit_plan_mode] re-parking approval after resume"
        );
        let parsed = match self
            .request_plan_approval(&tool_call_id, Some(plan_content))
            .await
        {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::debug!(%err, "resume exit_plan_mode reverse-request failed");
                return;
            }
        };
        match resume_action_for(PlanApprovalOutcome::from_response(&parsed), parsed.feedback) {
            ResumeAction::LeaveOnly => {
                tracing::info!("[exit_plan_mode] resume: user abandoned plan");
                self.leave_plan_mode_to_default();
            }
            ResumeAction::StayAndRevise(text) => {
                tracing::info!("[exit_plan_mode] resume: user requested changes");
                self.start_resume_turn(text, PromptMode::Plan, completion_tx)
                    .await;
            }
            ResumeAction::LeaveAndImplement => {
                tracing::info!("[exit_plan_mode] resume: user approved plan");
                self.leave_plan_mode_to_default();
                self.start_resume_turn(
                    PLAN_APPROVED_IMPLEMENT_MESSAGE.to_string(),
                    PromptMode::Agent,
                    completion_tx,
                )
                .await;
            }
        }
    }
    /// Inject a synthetic user turn after a resumed plan decision and kick the scheduler (no in-flight turn exists on resume to continue).
    async fn start_resume_turn(
        self: Arc<Self>,
        text: String,
        mode: PromptMode,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        let prompt_id = format!("plan-resume-{}", chrono::Utc::now().timestamp_millis());
        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(text))];
        let (respond_to, _rx) = oneshot::channel();
        let _ = self
            .queue_input(QueueInputRequest::from_legacy_prompt_id(
                prompt_blocks,
                prompt_id,
                mode,
                respond_to,
            ))
            .await;
        SessionActor::maybe_start_running_task(self.clone(), completion_tx).await;
    }
    /// Refine the initial (minimal) ToolCall that was registered during tool preparation.
    /// Returns `(title, kind, raw_input)` so callers can reuse them.
    async fn send_tool_call_start(
        &self,
        tool_call_id: &acp::ToolCallId,
        wire_name: &str,
        tool_call_input: ToolInput,
    ) -> Result<(String, acp::ToolKind, serde_json::Value), acp::Error> {
        #[allow(unused_mut)]
        let mut raw_input = match &tool_call_input {
            ToolInput::SendSubagentMessage(message) => serde_json::to_value(message)?,
            _ => serde_json::to_value(&tool_call_input)?,
        };
        let mut canonical_meta = self.stamp_tool_meta(None, wire_name, Some(&tool_call_input));
        let (title, kind, locations, content) = match tool_call_input {
            ToolInput::ListDir(list_dir) => (
                format!("List `{}`", list_dir.target_directory),
                acp::ToolKind::Other,
                vec![acp::ToolCallLocation::new(
                    list_dir.target_directory.clone(),
                )],
                vec![],
            ),
            ToolInput::SearchReplace(sr) => {
                let display_path = self.tool_context.cwd.join(&sr.file_path).to_path_buf();
                let meta = if !sr.old_string.is_empty() {
                    let _span = tracing::info_span!("tool.sr_line_lookup").entered();
                    self.tool_context
                        .fs
                        .read_to_string(&display_path)
                        .await
                        .ok()
                        .and_then(|file_content| {
                            let pos = file_content.find(&sr.old_string)?;
                            let line =
                                file_content.get(..pos).unwrap_or("").matches('\n').count() + 1;
                            serde_json::json!({ "old_line" : line, "new_line" : line, })
                                .as_object()
                                .cloned()
                        })
                } else {
                    None
                };
                (
                    format!("Edit `{}`", sr.file_path.as_str()),
                    acp::ToolKind::Edit,
                    vec![acp::ToolCallLocation::new(sr.file_path.clone())],
                    vec![acp::ToolCallContent::from(
                        acp::Diff::new(display_path, sr.new_string)
                            .old_text(Some(sr.old_string))
                            .meta(meta),
                    )],
                )
            }
            ToolInput::Bash(bash_tool) => execute_tool_call_parts(
                &bash_tool.command,
                Some(bash_tool.description.as_str()),
                self.tool_context.cwd.as_path(),
            ),
            ToolInput::ReadFile(read_file) => {
                if let Some(skill) = self.skill_for_read_path(&read_file.path).await {
                    self.emit_skill_md_read(skill);
                }
                (
                    format!("Read `{}`", read_file.path),
                    acp::ToolKind::Read,
                    vec![
                        acp::ToolCallLocation::new(read_file.path)
                            // Same normalization as the canonical `_meta` input, so one event can't show two start lines
                            .line(
                                distill_tools::normalization::norm_offset_i64(read_file.offset)
                                    .map(|l| l as u32),
                            ),
                    ],
                    Vec::new(),
                )
            }
            ToolInput::TodoWrite(_) => (
                "Updating plan".to_string(),
                acp::ToolKind::Think,
                Vec::new(),
                Vec::new(),
            ),
            ToolInput::Grep(gs) => (gs.pattern.clone(), acp::ToolKind::Search, vec![], vec![]),
            ToolInput::WebSearch(ws) => (
                format!("Web search: \"{}\"", ws.query),
                acp::ToolKind::Search,
                vec![],
                vec![],
            ),
            ToolInput::ImageGen(ig) => (
                format!("imagine: {}", ig.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ImageEdit(ie) => (
                format!("imagine-edit: {}", ie.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ImageToVideo(i2v) => (
                format!(
                    "image-to-video: {}",
                    i2v.prompt.as_deref().unwrap_or(&i2v.image)
                ),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ReferenceToVideo(r2v) => (
                format!("reference-to-video: {}", r2v.prompt),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::MCPTool(mcp_tool) => (
                mcp_tool.tool_name.to_owned(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::TaskOutput(task_output) => {
                let ids = task_output.resolved_task_ids();
                let label = match ids.as_slice() {
                    [] => "Get task output".to_string(),
                    [one] => format!("Get task output: {one}"),
                    many => format!("Get task output: {} tasks", many.len()),
                };
                (label, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::WaitTasks(wait) => (
                format!(
                    "Wait tasks: {} ids, mode={}",
                    wait.task_ids.len(),
                    match wait.mode {
                        distill_tool_types::WaitMode::WaitAny => "wait_any",
                        distill_tool_types::WaitMode::WaitAll => "wait_all",
                    }
                ),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::KillTask(kill_task) => (
                format!("Kill task: {}", kill_task.task_id),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::Skill(skill) => {
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::SkillDispatched {
                        skill_name: skill.skill.clone(),
                        plugin_source: None,
                        trigger: distill_telemetry::events::SkillTrigger::SkillTool,
                        skill_source: None,
                    },
                );
                distill_telemetry::event_span!(
                    "skill.activated",
                    skill_name = %skill.skill,
                    invocation_trigger = "skill_tool",
                );
                (
                    format!("Skill: {}", skill.skill),
                    acp::ToolKind::Other,
                    vec![],
                    vec![],
                )
            }
            ToolInput::ApplyPatch(_) => (
                "Apply patch".to_string(),
                acp::ToolKind::Edit,
                vec![],
                vec![],
            ),
            ToolInput::SendFeedback(_) => (
                "Feedback drafted".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::Dynamic(_)
                if wire_name
                    == distill_tools::implementations::distill::SEND_FEEDBACK_TOOL_NAME =>
            {
                (
                    "Feedback drafted".to_string(),
                    acp::ToolKind::Other,
                    vec![],
                    vec![],
                )
            }
            ToolInput::Dynamic(_) => (
                "Dynamic tool call".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::MemorySearch(ms) => {
                let end = ms
                    .query
                    .char_indices()
                    .nth(60)
                    .map_or(ms.query.len(), |(i, _)| i);
                let display = match ms.query.get(..end) {
                    Some(s) => s,
                    None => ms.query.as_str(),
                };
                (
                    format!("Memory search: \"{display}\""),
                    acp::ToolKind::Other,
                    vec![],
                    vec![],
                )
            }
            ToolInput::MemoryGet(mg) => (
                format!("Memory read: {}", mg.path),
                acp::ToolKind::Read,
                vec![],
                vec![],
            ),
            ToolInput::HashlineEdit(he) => (
                format!("Edit `{}`", he.file_path),
                acp::ToolKind::Edit,
                vec![acp::ToolCallLocation::new(he.file_path.clone())],
                vec![],
            ),
            ToolInput::Task(task) => (
                task.description.clone(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::EnterPlanMode(_) => (
                "Plan: Enter".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::ExitPlanMode(_) => (
                "Plan: Exit".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::AskUserQuestion(ref ask) => {
                let title = match ask.questions.as_slice() {
                    [q] => format!("Ask: {}", q.question),
                    qs => format!("Ask {} questions", qs.len()),
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::SendSubagentMessage(message) => {
                let (title, kind) = active_agent_message_tool_call_display(&message);
                (title, kind, vec![], vec![])
            }
            ToolInput::WebFetch(wf) => (
                format!("Fetch: {}", wf.url),
                acp::ToolKind::Fetch,
                vec![],
                vec![],
            ),
            ToolInput::SearchTool(st) => (
                format!("Search tools: \"{}\"", st.query),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::UseTool(ut) => (ut.tool_name.clone(), acp::ToolKind::Other, vec![], vec![]),
            ToolInput::Write(ref w) => (
                format!("Write `{}`", w.file_path),
                acp::ToolKind::Edit,
                vec![acp::ToolCallLocation::new(w.file_path.clone())],
                vec![acp::ToolCallContent::from(
                    acp::Diff::new(
                        self.tool_context.cwd.join(&w.file_path).to_path_buf(),
                        w.content.clone(),
                    )
                    .old_text(Some(String::new())),
                )],
            ),
            ToolInput::Workflow(ref w) => {
                let script_name = |script: &str| -> Option<String> {
                    let head = script.get(..600).unwrap_or(script);
                    let rest = head.get(head.find("name:")? + 5..)?;
                    let rest = rest.get(rest.find('"')? + 1..)?;
                    Some(rest.get(..rest.find('"')?)?.to_string())
                };
                use distill_tools::implementations::distill::workflow::WorkflowSource;
                let inline_name = match &w.source {
                    WorkflowSource::Script { script } => script_name(script),
                    _ => None,
                };
                let title = if w.validate_only {
                    let source_name = match &w.source {
                        WorkflowSource::Name { name } => Some(name.clone()),
                        _ => inline_name,
                    };
                    match source_name {
                        Some(name) => format!("Validating workflow '{name}'"),
                        None => "Validating workflow script".to_string(),
                    }
                } else {
                    match &w.source {
                        WorkflowSource::Script { .. } => match inline_name {
                            Some(name) => format!("Creating workflow '{name}'"),
                            None => "Creating workflow".to_string(),
                        },
                        WorkflowSource::Name { name } => format!("Workflow: {name}"),
                        WorkflowSource::Resume { .. } => "Workflow: resume run".to_string(),
                        WorkflowSource::Pause { .. } => "Workflow: pause run".to_string(),
                        WorkflowSource::Stop { .. } => "Workflow: stop run".to_string(),
                        WorkflowSource::ScriptPath { .. } => "Workflow: launch script".to_string(),
                    }
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::UpdateGoal(ref ug) => {
                let title = if ug.completed == Some(true) {
                    "Goal: marking complete".to_string()
                } else if let Some(ref reason) = ug.blocked_reason {
                    format!("Goal: blocked — {reason}")
                } else if let Some(ref msg) = ug.message {
                    format!("Goal: {msg}")
                } else {
                    "Goal: update".to_string()
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::Monitor(ref m) => (
                format!("Start monitor: {}", m.description),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::SchedulerCreate(ref sc) => {
                let title = match (&sc.task_id, &sc.interval) {
                    (Some(id), Some(interval)) => {
                        format!("Update scheduled task {id} (every {interval})")
                    }
                    (Some(id), None) => format!("Update scheduled task {id}"),
                    (None, Some(interval)) => {
                        format!("Create scheduled task (every {interval})")
                    }
                    (None, None) => "Create scheduled task".to_string(),
                };
                (title, acp::ToolKind::Other, vec![], vec![])
            }
            ToolInput::SchedulerDelete(ref sd) => (
                format!("Delete scheduled task: {}", sd.id),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            ToolInput::SchedulerList(_) => (
                "List scheduled tasks".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
            #[allow(unreachable_patterns)]
            _ => (
                "Tool call".to_string(),
                acp::ToolKind::Other,
                vec![],
                vec![],
            ),
        };
        if let Some(access) = self
            .agent
            .borrow()
            .tool_bridge()
            .read_resource::<distill_tools::types::memory_v2::MemoryV2AccessResource>()
            .await
        {
            let roots = access.0.scope_roots();
            let path_is_memory = |path: &Path| {
                let resolved = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.tool_context
                        .cwd
                        .join(path.to_string_lossy())
                        .to_path_buf()
                };
                roots.iter().any(|root| resolved.starts_with(root))
            };
            let is_memory_activity = locations
                .iter()
                .any(|location| path_is_memory(&location.path))
                || ["path", "file_path", "target_directory"]
                    .iter()
                    .filter_map(|key| raw_input.get(key).and_then(|value| value.as_str()))
                    .any(|path| path_is_memory(Path::new(path)));
            if is_memory_activity {
                canonical_meta
                    .get_or_insert_with(Default::default)
                    .insert("memory_v2_activity".to_owned(), true.into());
            }
        }
        let tool_call_update = acp::ToolCallUpdate::new(
            tool_call_id.clone(),
            acp::ToolCallUpdateFields::new()
                .title(Some(title.clone()))
                .kind(Some(kind))
                .locations(Some(locations))
                .content(if content.is_empty() {
                    None
                } else {
                    Some(content)
                })
                .raw_input(Some(raw_input.clone())),
        )
        .meta(canonical_meta);
        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_call_update), None)
            .await;
        Ok((title, kind, raw_input))
    }
    /// Resolves the path the way the read tool does, so `~` paths and forked sessions still match.
    async fn skill_for_read_path(
        &self,
        path: &str,
    ) -> Option<distill_tools::implementations::skills::types::SkillInfo> {
        if self.is_chat_kind {
            return None;
        }
        let read_path = distill_tools::types::resources::resolve_model_path(
            self.tool_context.cwd.as_path(),
            self.display_cwd.get().map(Path::new),
            path,
        );
        if read_path.file_name().is_none_or(|name| name != "SKILL.md") {
            return None;
        }
        let read_path = dunce::canonicalize(&read_path).unwrap_or(read_path);
        self.slash_skills_for_resolve()
            .await
            .into_iter()
            .find(|skill| {
                crate::session::telemetry::is_same_skill_file(Path::new(&skill.path), &read_path)
            })
    }
    fn emit_skill_md_read(&self, skill: distill_tools::implementations::skills::types::SkillInfo) {
        let skill_source =
            crate::session::telemetry::skill_source(skill.scope, skill.plugin_name.as_deref());
        distill_telemetry::event_span!(
            "skill.activated",
            skill_name = %skill.name,
            invocation_trigger = "skill_md_read",
            skill_source = skill_source,
        );
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::SkillDispatched {
            skill_name: skill.name,
            plugin_source: skill.plugin_name,
            trigger: distill_telemetry::events::SkillTrigger::SkillMdRead,
            skill_source: Some(skill_source.to_owned()),
        });
    }
    pub(super) fn make_pre_tool_use_envelope(
        &self,
        resolved_tool_name: &str,
        call_id: &str,
        input: &serde_json::Value,
    ) -> distill_hooks::event::HookEventEnvelope {
        let (tool_input, tool_input_truncated) =
            distill_hooks::event::truncate_payload(input.clone());
        self.make_hook_envelope(
            distill_hooks::event::HookEventName::PreToolUse,
            None,
            distill_hooks::event::HookPayload::PreToolUse {
                tool_name: resolved_tool_name.to_string(),
                tool_use_id: call_id.to_string(),
                tool_input,
                tool_input_truncated,
                subagent_type: self.subagent_type_label(),
            },
        )
    }
    async fn handle_tool_parse_error(
        &self,
        tool_call_id: &acp::ToolCallId,
        call_id: &str,
        function_name: &str,
        err: distill_tool_runtime::ToolError,
        raw_arguments: &str,
        model_id: &str,
    ) -> Result<(), acp::Error> {
        tracing::error!(
            session_id = %self.session_info.id.0,
            tool_name = function_name,
            model_id = model_id,
            error_kind = "parse_failure",
            error_message = %err,
            "tool_error: parse_failure"
        );
        self.signals_handle().record_tool_failure(function_name);
        let message = build_tool_parse_error_message(function_name, &err, raw_arguments);
        let title = (err.kind == distill_tool_runtime::ToolErrorKind::NotFound)
            .then(|| format!("Agent tried calling a tool that doesn't exist: {function_name}"));
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::Failed))
                    .title(title)
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                    )])),
            )),
            None,
        )
        .await;
        let tool_chat = ConversationItem::tool_result(call_id.to_string(), message);
        self.chat_state_handle.push_tool_result(tool_chat);
        Ok(())
    }
    /// Queued auto-wake synthetic prompts for a task/subagent the model already learned about are dropped before they get flushed to chat history.
    /// Flushed, they would appear as a trailing `<system-reminder>` with no assistant reply.
    /// `TaskCompletionReminder` uses the same predicate; they cannot drift because they share the function.
    pub(super) async fn drop_pending_items_for_consumed_completions(&self, consumed_ids: &[&str]) {
        if consumed_ids.is_empty() {
            return;
        }
        let mut state = self.state.lock().await;
        let dropped = state.sweep_pending_inputs(|i| {
            i.input_origin
                .completion_id()
                .is_some_and(|id| consumed_ids.contains(&id))
        });
        let dropped_inputs = dropped.len();
        let before_notifications = state.pending_notifications.len();
        state
            .pending_notifications
            .retain(|n| !consumed_ids.contains(&n.source.task_id()));
        let dropped_notifications = before_notifications - state.pending_notifications.len();
        drop(state);
        if let Some(reservations) = &self.tool_context.task_completion_reservations {
            for task_id in dropped
                .iter()
                .filter_map(|input| input.input_origin.completion_id())
            {
                reservations.release(task_id);
            }
        }
        if dropped_inputs > 0 || dropped_notifications > 0 {
            tracing::info!(
                dropped_inputs,
                dropped_notifications,
                consumed_ids = ?consumed_ids,
                "auto-wake: dropped queued synthetic items for consumed completions"
            );
        }
    }
    /// Drain queued runtime producer wakes from `pending_inputs`, and clear ALL `pending_notifications` unconditionally.
    /// A runtime wake that slipped past the per-tool-result sweep must not be flushed to `chat_history.jsonl` after the actor returns.
    /// Drain-policy rows are preserved.
    pub(super) async fn drop_pending_synthetic_items(&self) {
        let mut state = self.state.lock().await;
        let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
        let mut dropped = Vec::new();
        for input in std::mem::take(&mut state.pending_inputs) {
            if input.input_origin.is_preemptible_runtime_wake() {
                dropped.push(input);
            } else {
                kept.push_back(input);
            }
        }
        state.pending_inputs = kept;
        state.pending_notifications.clear();
        drop(state);
        if let Some(reservations) = &self.tool_context.task_completion_reservations {
            for task_id in dropped
                .iter()
                .filter_map(|input| input.input_origin.completion_id())
            {
                reservations.release(task_id);
            }
        }
    }
    /// Record git/PR ops from a successful tool result into session signals (`turn_result.json`) and telemetry.
    /// Detection runs here at the shell's tool-result chokepoint over the command and prompt output.
    /// Nothing is wired through the tool's output schema.
    fn record_git_pr_signals(&self, effective_tool_name: &str, result: &ToolRunResult) {
        use distill_telemetry::enums::PrCreationSource;
        use distill_tools::util::git_detect;
        match &result.output {
            distill_tools::types::output::ToolOutput::Bash(b) if b.exit_code == 0 => {
                let Some(ops) = git_detect::detect_git_ops(&b.command, &b.output_for_prompt) else {
                    return;
                };
                if ops.committed {
                    self.signals_handle().record_git_commit();
                }
                if let Some(pr) = ops.pr_created {
                    self.record_pr_created(pr, PrCreationSource::Bash);
                }
                if ops.pr_merged {
                    self.signals_handle().record_pr_merged();
                    distill_telemetry::session_ctx::log_event(
                        distill_telemetry::events::PrMerged {},
                    );
                }
            }
            distill_tools::types::output::ToolOutput::MCP(m)
                if !m.is_error && is_mcp_create_pull_request(effective_tool_name) =>
            {
                let pr = git_detect::PrRef::find_in(&result.prompt_text).unwrap_or_default();
                self.record_pr_created(pr, PrCreationSource::Mcp);
            }
            _ => {}
        }
    }
    /// Record a PR creation into session signals.
    /// `had_commit_in_session` is provisional here: the signals actor.
    /// reconciles it at `TakeTurnEndSnapshot`, after every event of the turn.
    fn record_pr_created(
        &self,
        pr: distill_tools::util::git_detect::PrRef,
        source: distill_telemetry::enums::PrCreationSource,
    ) {
        self.signals_handle()
            .record_pr_created(crate::session::signals::PrCreatedSignal {
                url: pr.url,
                number: pr.number,
                source,
                had_commit_in_session: false,
            });
    }
    pub(super) async fn handle_bridge_tool_success(
        &self,
        args: BridgeToolSuccess<'_>,
    ) -> Result<Vec<ConversationItem>, acp::Error> {
        use crate::session::acp_conversion::{acp_plan_update, acp_tool_update, maybe_rewrite};
        let BridgeToolSuccess {
            tool_call_id,
            call_id,
            requested_tool_name,
            effective_tool_name,
            drained,
            concatenated_json_count,
            model_id,
            tool_parsed_args,
            model_output_override,
        } = args;
        let (mut result, mut tool_layer_images) = drained.into_parts();
        let consumed_ids =
            distill_tools::reminders::task_completion::consumed_completion_ids(&result.output);
        if !consumed_ids.is_empty() {
            self.drop_pending_items_for_consumed_completions(&consumed_ids)
                .await;
        }
        if let ToolsToolOutput::BackgroundTaskStarted(ref bg) = result.output {
            self.record_goal_turn_task_ids([bg.task_id.clone()]);
        }
        if matches!(
            &result.output,
            ToolsToolOutput::SearchReplace(
                distill_tools::types::output::SearchReplaceOutput::EditsApplied(_)
            ) | ToolsToolOutput::Bash(_)
        ) {
            self.maybe_notify_git_branch().await;
        }
        if let distill_tools::types::output::ToolOutput::Bash(ref b) = result.output
            && b.was_bare_echo
        {
            self.signals_handle().record_bare_echo();
        }
        self.record_git_pr_signals(effective_tool_name, &result);
        let path_rewriter = self.path_rewriter();
        let tool_meta = {
            let state = self.mcp_state.lock().await;
            state.mcp_tool_meta.get(effective_tool_name).cloned()
        };
        tool_layer_images.extend(drain_tool_layer_extracted_images(&mut result.output));
        if let Some(mut tool_update) =
            acp_tool_update(&result.output, call_id, path_rewriter.as_ref(), tool_meta)
        {
            if tool_update.fields.status == Some(acp::ToolCallStatus::Failed) {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    tool_name = requested_tool_name,
                    effective_tool_name = effective_tool_name,
                    model_id = model_id,
                    error_kind = "tool_output_error",
                    "tool_error: tool_output_error"
                );
                self.signals_handle()
                    .record_tool_failure(requested_tool_name);
            } else {
                self.signals_handle()
                    .record_tool_success(requested_tool_name);
            }
            if matches!(
                &result.output,
                distill_tools::types::output::ToolOutput::EnterPlanMode(_)
                    | distill_tools::types::output::ToolOutput::ExitPlanMode(_)
            ) {
                let plan_path = self.plan_mode.lock().plan_file_path().display().to_string();
                if let Some(ref mut content) = tool_update.fields.content {
                    for item in content.iter_mut() {
                        if let acp::ToolCallContent::Content(acp::Content {
                            content: acp::ContentBlock::Text(t),
                            ..
                        }) = item
                        {
                            t.text = format!("Plan file: {}", plan_path);
                        }
                    }
                }
            }
            tool_update.tool_call_id = tool_call_id.clone();
            self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
                .await;
        } else {
            self.signals_handle()
                .record_tool_success(requested_tool_name);
        }
        if let Some(acp_plan) = acp_plan_update(&result.output) {
            self.send_update(acp::SessionUpdate::Plan(acp_plan), None)
                .await;
        }
        let output_replaced = model_output_override.is_some();
        if let Some(replacement) = model_output_override {
            result.prompt_text =
                substitute_rendered_output(&result.prompt_text, &result.output, replacement);
        }
        let prompt_text = if concatenated_json_count > 0 && !self.is_cursor_harness() {
            let remaining = concatenated_json_count - 1;
            format!(
                "{}\n\n<system-reminder>\nIMPORTANT: Your tool call contained {} concatenated JSON \
                 objects, but only the best-matching one was executed. The remaining {} \
                 were ignored. You MUST use separate tool calls (one per operation) \
                 instead of concatenating multiple JSON objects in a single call's \
                 arguments. Make {} individual tool call{} for the remaining \
                 operations.\n</system-reminder>",
                result.prompt_text,
                concatenated_json_count,
                remaining,
                remaining,
                if remaining == 1 { "" } else { "s" },
            )
        } else {
            result.prompt_text
        };
        let (prompt_text, inline_images, extracted_images) = if output_replaced {
            (
                maybe_rewrite(path_rewriter.as_ref(), prompt_text),
                Vec::new(),
                Vec::new(),
            )
        } else {
            self.render_non_replaced_tool_body(
                &result.output,
                prompt_text,
                path_rewriter.as_ref(),
                tool_parsed_args,
                effective_tool_name,
                requested_tool_name,
                tool_layer_images,
            )
            .await
        };
        // Jev pass over the finished result (todo.md areas A/C/D): it may narrow
        // what the model re-reads or annotate it, never widen or approve.
        // The command behind the result, when the tool was a shell: the lanes need
        // it to tell a document (`cat`, `jq`, `git diff`) from a log, and to tell
        // the reader what to re-run when nothing was archived.
        let tool_command = tool_parsed_args
            .get("command")
            .or_else(|| tool_parsed_args.get("cmd"))
            .or_else(|| tool_parsed_args.get("script"))
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let prompt_text = self
            .jev_post_process_tool_result(requested_tool_name, tool_command, prompt_text)
            .await;
        let tool_chat = if inline_images.is_empty() {
            ConversationItem::tool_result(call_id.to_string(), prompt_text)
        } else {
            ConversationItem::tool_result_with_images(
                call_id.to_string(),
                prompt_text,
                inline_images,
            )
        };
        self.chat_state_handle.push_tool_result(tool_chat);
        let mut deferred_followups = Vec::new();
        if !extracted_images.is_empty() {
            let count = extracted_images.len();
            tracing::info!(
                session_id = %self.session_info.id,
                tool = requested_tool_name,
                count,
                "base64 images extracted from tool result",
            );
            let acp_images: Vec<agent_client_protocol::ImageContent> = extracted_images
                .into_iter()
                .map(|img| agent_client_protocol::ImageContent::new(img.data, img.mime_type))
                .collect();
            let is_cursor_for_tool_result = self.is_cursor_harness();
            let mut norm_result = crate::session::image_normalize::normalize_images(
                acp_images,
                is_cursor_for_tool_result,
            )
            .await;
            if !norm_result.re_encode_fallbacks.is_empty() {
                tracing::warn!(
                    session_id = %self.session_info.id,
                    notes = %norm_result.re_encode_fallbacks.join(" "),
                    "Extracted tool image kept original after re-encode failure",
                );
            }
            if let Some((notice, notes)) = crate::session::image_normalize::dropped_to_envelope(
                std::mem::take(&mut norm_result.dropped),
                is_cursor_for_tool_result,
            ) {
                deferred_followups.push(ConversationItem::system_reminder(notice));
                self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes })
                    .await;
            }
            for norm in norm_result.images {
                let url = format!("data:{};base64,{}", norm.mime_type, norm.data);
                let mut image_msg =
                    ConversationItem::system_reminder("[Image extracted from tool result above]");
                image_msg.add_image(url);
                deferred_followups.push(image_msg);
            }
        }
        Ok(deferred_followups)
    }
    async fn render_non_replaced_tool_body(
        &self,
        output: &ToolsToolOutput,
        prompt_text: String,
        path_rewriter: Option<&crate::session::acp_conversion::PathRewriter>,
        tool_parsed_args: &serde_json::Value,
        effective_tool_name: &str,
        requested_tool_name: &str,
        tool_layer_images: Vec<distill_tools::util::base64_images::ExtractedImage>,
    ) -> (
        String,
        Vec<ContentPart>,
        Vec<distill_tools::util::base64_images::ExtractedImage>,
    ) {
        use crate::session::acp_conversion::maybe_rewrite;
        let mut prompt_text = prompt_text;
        let mut inline_images: Vec<ContentPart> = Vec::new();
        let extraction = if !self.is_cursor_harness()
            && !matches!(
                *output,
                ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(_))
                    | ToolsToolOutput::ReadFile(ReadFileOutput::PdfPageImages(_))
            ) {
            distill_tools::util::base64_images::extract_base64_images(prompt_text)
        } else {
            distill_tools::util::base64_images::ExtractionResult {
                text: prompt_text,
                images: Vec::new(),
            }
        };
        let mut extracted_images = extraction.images;
        split_tool_layer_for_harness(
            self.is_cursor_harness(),
            &mut extracted_images,
            tool_layer_images,
        );
        prompt_text = maybe_rewrite(path_rewriter, extraction.text);
        if !self.is_cursor_harness()
            && let ToolsToolOutput::ReadFile(ReadFileOutput::ImageContent(ref image_content)) =
                *output
        {
            let path = tool_parsed_args
                .get("target_file")
                .or_else(|| tool_parsed_args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            use crate::session::image_normalize::{InlineAttachVerdict, inline_attach_verdict};
            match inline_attach_verdict(&image_content.data) {
                InlineAttachVerdict::TooSmall => {
                    prompt_text = format!(
                        "[Image from {path} was not attached: too small for vision models]"
                    );
                }
                InlineAttachVerdict::Unreadable => {
                    prompt_text = format!(
                        "[Image from {path} was not attached: invalid or unreadable image data]"
                    );
                }
                InlineAttachVerdict::Attach => {
                    let url = format!(
                        "data:{};base64,{}",
                        image_content.mime_type, image_content.data
                    );
                    inline_images.push(ContentPart::Image {
                        url: std::sync::Arc::<str>::from(url),
                    });
                    prompt_text = format!("Read image file: {path}");
                }
            }
        }
        if !self.is_cursor_harness()
            && let ToolsToolOutput::ReadFile(ReadFileOutput::PdfPageImages(ref pdf)) = *output
        {
            for page in &pdf.pages {
                let url = format!("data:{};base64,{}", page.mime_type, page.data);
                inline_images.push(ContentPart::Image {
                    url: std::sync::Arc::<str>::from(url),
                });
            }
            let path = tool_parsed_args
                .get("target_file")
                .or_else(|| tool_parsed_args.get("path"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            prompt_text = format!(
                "Read PDF file: {path} ({} pages rendered, {} total)",
                pdf.pages.len(),
                pdf.total_pages,
            );
        }
        (prompt_text, inline_images, extracted_images)
    }
    /// Handle a hard tool execution error (dispatch/validation failure).
    /// Tool failures are not fed to the doom-loop detector (error-count streaks were removed).
    /// This therefore never warns/terminates and returns no deferred follow-ups today.
    pub(super) async fn handle_tool_error(
        &self,
        tool_call_id: &acp::ToolCallId,
        call_id: &str,
        requested_tool_name: &str,
        effective_tool_name: Option<&str>,
        err: &anyhow::Error,
        model_id: &str,
    ) -> Vec<ConversationItem> {
        tracing::error!(
            session_id = %self.session_info.id.0,
            tool_name = requested_tool_name,
            effective_tool_name = effective_tool_name,
            model_id = model_id,
            error_kind = "execution_failure",
            error_message = %err,
            "tool_error: execution_failure"
        );
        self.signals_handle()
            .record_tool_failure(requested_tool_name);
        let rewriter = self.path_rewriter();
        let err_str = match rewriter.as_ref() {
            Some(rw) => rw.rewrite(&err.to_string()),
            None => err.to_string(),
        };
        let message = match effective_tool_name {
            Some(effective) if effective != requested_tool_name => {
                format!("Tool `{effective}` failed via `{requested_tool_name}`: {err_str}")
            }
            _ => format!("Tool `{requested_tool_name}` failed: {err_str}"),
        };
        self.send_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                tool_call_id.clone(),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::Failed))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(message.clone())),
                    )]))
                    .raw_output(Some(json!({
                        "error": "tool_execution_failed",
                        "message": err_str,
                    }))),
            )),
            None,
        )
        .await;
        let tool_chat = ConversationItem::tool_result(call_id.to_string(), message);
        self.chat_state_handle.push_tool_result(tool_chat);
        vec![]
    }
    /// Model-facing rejection for a non-plan-file edit while plan mode is active.
    /// Rendered via the session's `TemplateRenderer` so `${{ plan_path }}` resolves; falls back if rendering fails.
    pub(super) async fn plan_mode_edit_rejected_message(&self) -> String {
        let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
        self.render_plan_template(
            crate::session::plan_mode::plan_mode_edit_rejected_template(),
            &plan_path,
            false,
        )
        .await
        .unwrap_or_else(|| {
            format!(
                "Rejected: file edits are not allowed in plan mode - the only editable \
                 file is the plan file ({}).",
                plan_path.display()
            )
        })
    }
    pub(super) async fn handle_tool_not_executed(
        &self,
        model_call_id: &str,
        tool_call_id: &acp::ToolCallId,
        reason: String,
    ) -> Result<(), acp::Error> {
        let tool_update = acp::ToolCallUpdate::new(
            tool_call_id.clone(),
            acp::ToolCallUpdateFields::new()
                .status(Some(acp::ToolCallStatus::Failed))
                .content(Some(vec![acp::ToolCallContent::from(
                    acp::ContentBlock::Text(acp::TextContent::new(reason.clone())),
                )])),
        );
        self.send_update(acp::SessionUpdate::ToolCallUpdate(tool_update), None)
            .await;
        let tool_chat = ConversationItem::tool_result(model_call_id.to_owned(), reason);
        self.chat_state_handle.push_tool_result(tool_chat);
        Ok(())
    }
}
/// The title peels a redundant leading `cd <cwd>` for chrome only; `raw_input` is serialized separately and stays full.
fn execute_tool_call_parts(
    command: &str,
    description: Option<&str>,
    cwd: &std::path::Path,
) -> (
    String,
    acp::ToolKind,
    Vec<acp::ToolCallLocation>,
    Vec<acp::ToolCallContent>,
) {
    let display = distill_tools::util::strip_redundant_session_cd(command, cwd);
    (
        format!("Execute `{display}`"),
        acp::ToolKind::Execute,
        Vec::new(),
        vec![acp::ToolCallContent::from(acp::ContentBlock::Text(
            acp::TextContent::new(description.unwrap_or_default().to_string()),
        ))],
    )
}
#[cfg(test)]
mod execute_tool_call_parts_tests {
    use super::execute_tool_call_parts;
    use std::path::Path;
    #[test]
    fn peels_redundant_session_cd_from_title() {
        let (title, ..) =
            execute_tool_call_parts("cd /proj && echo hi", Some("desc"), Path::new("/proj"));
        assert_eq!(title, "Execute `echo hi`");
    }
    #[test]
    fn keeps_command_when_cd_not_redundant() {
        let (title, ..) = execute_tool_call_parts("cd /other && ls", None, Path::new("/proj"));
        assert_eq!(title, "Execute `cd /other && ls`");
    }
}
#[cfg(test)]
mod mcp_error_routing_tests {
    use super::is_mcp_error_result;
    use distill_tools::types::output::{MCPOutput, TodoWriteOutput, ToolOutput};
    #[test]
    fn only_mcp_error_results_route_to_post_tool_use_failure() {
        let mcp_error = ToolOutput::MCP(MCPOutput::errored(
            "search".into(),
            "memory".into(),
            "boom".into(),
        ));
        assert!(
            is_mcp_error_result(&mcp_error),
            "MCP error result → failure"
        );
        let mcp_ok = ToolOutput::MCP(MCPOutput::okay_output(
            "search".into(),
            "memory".into(),
            "ok".into(),
        ));
        assert!(
            !is_mcp_error_result(&mcp_ok),
            "MCP success stays PostToolUse"
        );
        let builtin_error = ToolOutput::Todo(TodoWriteOutput::DuplicateId("dup".into()));
        assert!(
            builtin_error.is_error(),
            "guard: builtin is a logical error"
        );
        assert!(
            !is_mcp_error_result(&builtin_error),
            "built-in logical error stays PostToolUse"
        );
    }
}
#[cfg(test)]
mod exit_plan_tail_predicate_tests {
    use super::{
        is_file_backed_exit_plan_input, is_file_backed_exit_plan_kind, split_exit_plan_tail,
    };
    use distill_tools::types::ToolInput;
    use distill_tools::types::tool::ToolKind;
    fn call(name: &str, args: &str) -> crate::sampling::types::ToolCallResponse {
        crate::sampling::types::ToolCallResponse {
            id: format!("call_{name}"),
            kind: "function".into(),
            function: crate::sampling::types::ToolCallFunction::new(name, args),
        }
    }
    /// Wire name does not matter; only [`ToolKind::ExitPlan`] does.
    fn kind_of(name: &str) -> Option<ToolKind> {
        match name {
            "exit_plan_mode" | "FinishPlan" => Some(ToolKind::ExitPlan),
            _ => None,
        }
    }
    #[test]
    fn exit_plan_kind_is_file_backed_exit() {
        assert!(is_file_backed_exit_plan_kind(Some(ToolKind::ExitPlan)));
        assert!(!is_file_backed_exit_plan_kind(Some(ToolKind::Edit)));
        assert!(!is_file_backed_exit_plan_kind(None));
        assert!(is_file_backed_exit_plan_input(&ToolInput::ExitPlanMode(
            distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeInput {}
        )));
    }
    fn mixed(calls: Vec<crate::sampling::types::ToolCallResponse>) -> bool {
        let (body, tail) = split_exit_plan_tail(calls, kind_of);
        !body.is_empty() && !tail.is_empty()
    }
    #[test]
    fn split_puts_exit_plan_in_tail() {
        let write = call(
            "search_replace",
            r#"{"file_path":"/tmp/plan.md","old_string":"a","new_string":"b"}"#,
        );
        let exit = call("exit_plan_mode", "{}");
        let renamed_exit = call("FinishPlan", "{}");
        let create = call(
            "CreatePlan",
            r#"{"name":"p","overview":"o","plan":"plan body","todos":[]}"#,
        );
        assert!(mixed(vec![write.clone(), exit.clone()]));
        assert!(mixed(vec![exit.clone(), write.clone()]));
        assert!(mixed(vec![write.clone(), renamed_exit.clone()]));
        assert!(!mixed(vec![exit.clone()]));
        assert!(!mixed(vec![write.clone()]));
        assert!(!mixed(vec![write.clone(), create.clone()]));
        assert!(mixed(vec![write, exit, create]));
    }
}
#[cfg(test)]
mod exit_plan_intercept_tests {
    use super::{PlanFileRead, classify_plan_file_read, should_intercept_exit_plan_approval};
    #[test]
    fn exit_plan_mode_empty_plan_still_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            true,
            false,
            false,
            &PlanFileRead::Absent,
        ));
    }
    #[test]
    fn exit_plan_mode_nonempty_plan_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            true,
            false,
            false,
            &PlanFileRead::Present("plan body".into()),
        ));
    }
    #[test]
    fn create_plan_empty_still_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            false,
            false,
            true,
            &PlanFileRead::Absent,
        ));
    }
    #[test]
    fn create_plan_nonempty_intercepts() {
        assert!(should_intercept_exit_plan_approval(
            false,
            false,
            true,
            &PlanFileRead::Present("inline plan".into()),
        ));
    }
    #[test]
    fn unrelated_tool_does_not_intercept() {
        assert!(!should_intercept_exit_plan_approval(
            false,
            false,
            false,
            &PlanFileRead::Absent,
        ));
        assert!(!should_intercept_exit_plan_approval(
            false,
            false,
            false,
            &PlanFileRead::Present("ignored".into()),
        ));
    }
    #[test]
    fn classify_plan_file_read_present() {
        assert_eq!(
            classify_plan_file_read(Ok("# plan".into())),
            PlanFileRead::Present("# plan".into())
        );
    }
    #[test]
    fn classify_plan_file_read_absent_empty() {
        assert_eq!(
            classify_plan_file_read(Ok("  \n".into())),
            PlanFileRead::Absent
        );
    }
    #[test]
    fn classify_plan_file_read_absent_not_found() {
        assert_eq!(
            classify_plan_file_read(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "missing",
            ))),
            PlanFileRead::Absent
        );
    }
    #[test]
    fn classify_plan_file_read_unreadable_permission_denied() {
        assert_eq!(
            classify_plan_file_read(Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            ))),
            PlanFileRead::Unreadable
        );
    }
}
#[cfg(test)]
mod plan_mode_edit_gate_tests {
    use super::{PlanEditGate, plan_mode_edit_gate};
    use crate::session::plan_mode::PlanModeTracker;
    use distill_tools::types::ToolInput;
    use distill_workspace::permission::AccessKind;
    /// Tracker with plan mode Active and plan file at `/tmp/gate-session/plan.md`.
    fn active_tracker() -> PlanModeTracker {
        let mut t = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert!(t.enter_pending());
        assert!(t.activate());
        t
    }
    fn gate(tracker: &PlanModeTracker, input: &ToolInput) -> PlanEditGate {
        plan_mode_edit_gate(tracker, input, &AccessKind::from(input))
    }
    fn search_replace(path: &str) -> ToolInput {
        use distill_tools::implementations::distill::search_replace::SearchReplaceInput;
        ToolInput::SearchReplace(SearchReplaceInput {
            file_path: path.into(),
            old_string: "a".into(),
            new_string: "b".into(),
            replace_all: false,
        })
    }
    fn write(path: &str) -> ToolInput {
        use distill_tools::implementations::opencode::write::WriteInput;
        ToolInput::Write(WriteInput {
            file_path: path.into(),
            content: "x".into(),
        })
    }
    /// Grok edit tools are plan-file-only while plan mode is active: the enforcement that makes plan mode read-only even under always-approve.
    #[test]
    fn grok_edits_outside_plan_file_rejected() {
        let t = active_tracker();
        assert_eq!(
            gate(&t, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::RejectNonPlanFile
        );
        assert_eq!(
            gate(&t, &write("/tmp/README.md")),
            PlanEditGate::RejectNonPlanFile,
            "grok tools get no markdown exception — plan file only"
        );
    }
    /// The carve-out and the permission bypass share `should_auto_approve_edit`, so the plan file itself stays editable.
    #[test]
    fn plan_file_edit_allowed() {
        let t = active_tracker();
        assert_eq!(
            gate(&t, &search_replace("/tmp/gate-session/plan.md")),
            PlanEditGate::Allow
        );
        assert_eq!(
            gate(&t, &write("/tmp/gate-session/plan.md")),
            PlanEditGate::Allow
        );
    }
    /// `apply_patch` names its files inside the patch text, never the plan file alone: always rejected in plan mode.
    #[test]
    fn apply_patch_rejected_in_plan_mode() {
        use distill_tools::implementations::codex::apply_patch::ApplyPatchInput;
        let t = active_tracker();
        assert_eq!(
            gate(
                &t,
                &ToolInput::ApplyPatch(ApplyPatchInput {
                    patch: String::new()
                })
            ),
            PlanEditGate::RejectNonPlanFile
        );
    }
    #[test]
    fn task_not_gated_in_plan_mode() {
        use distill_tool_types::TaskToolInput;
        let t = active_tracker();
        assert_eq!(
            gate(
                &t,
                &ToolInput::Task(TaskToolInput {
                    prompt: "p".into(),
                    description: "d".into(),
                    subagent_type: "general-purpose".into(),
                    run_in_background: false,
                    capability_mode: None,
                    isolation: None,
                    resume_from: None,
                    cwd: None,
                    model: None,
                    task_id: None,
                })
            ),
            PlanEditGate::Allow
        );
    }
    /// Non-edit tools are never gated; they flow to the normal permission path (where yolo may auto-approve them).
    /// Plan mode blocks edits, not bash/reads.
    #[test]
    fn non_edit_tools_not_gated() {
        use distill_tools::implementations::BashToolInput;
        let t = active_tracker();
        assert_eq!(
            gate(
                &t,
                &ToolInput::Bash(BashToolInput {
                    command: "echo hi > /tmp/f".into(),
                    timeout: None,
                    description: "write via bash".into(),
                    is_background: false,
                })
            ),
            PlanEditGate::Allow,
            "bash is deliberately not gated — plan mode blocks edits only"
        );
    }
    /// Inactive (or merely Pending) plan mode gates nothing.
    #[test]
    fn inactive_or_pending_plan_mode_allows_everything() {
        let inactive = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert_eq!(
            gate(&inactive, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::Allow
        );
        let mut pending = PlanModeTracker::new(std::path::PathBuf::from("/tmp/gate-session"));
        assert!(pending.enter_pending());
        assert_eq!(
            gate(&pending, &search_replace("/tmp/src/main.rs")),
            PlanEditGate::Allow,
            "Pending means the model has no plan-mode instructions yet — don't gate"
        );
    }
}
#[cfg(test)]
mod plan_approval_helper_tests {
    use super::{
        PlanApprovalOutcome, ResumeAction, ext_method_no_client, resume_action_for,
        revise_plan_message,
    };
    use distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeExtResponse;
    fn resp(outcome: &str) -> ExitPlanModeExtResponse {
        ExitPlanModeExtResponse {
            outcome: outcome.into(),
            feedback: None,
        }
    }
    #[test]
    fn outcome_from_response_maps_known_and_fails_closed() {
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("approved")),
            PlanApprovalOutcome::Approved
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("abandoned")),
            PlanApprovalOutcome::Abandoned
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("cancelled")),
            PlanApprovalOutcome::Cancelled
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("approve")),
            PlanApprovalOutcome::Cancelled
        );
        assert_eq!(
            PlanApprovalOutcome::from_response(&resp("")),
            PlanApprovalOutcome::Cancelled
        );
    }
    #[test]
    fn ext_method_no_client_defaults_false_for_untagged_error() {
        assert!(!ext_method_no_client(&distill_acp_lib::acp_internal_error(
            "unrelated internal error"
        )));
    }
    #[test]
    fn revise_plan_message_includes_feedback_when_present() {
        let with = revise_plan_message("use async");
        assert!(with.contains("The user said:"));
        assert!(with.contains("use async"));
    }
    #[test]
    fn resume_action_maps_each_outcome() {
        assert_eq!(
            resume_action_for(PlanApprovalOutcome::Approved, None),
            ResumeAction::LeaveAndImplement
        );
        assert_eq!(
            resume_action_for(PlanApprovalOutcome::Abandoned, Some("ignored".into())),
            ResumeAction::LeaveOnly
        );
        match resume_action_for(PlanApprovalOutcome::Cancelled, Some("tweak it".into())) {
            ResumeAction::StayAndRevise(text) => assert!(text.contains("tweak it")),
            other => panic!("expected StayAndRevise, got {other:?}"),
        }
    }
}
#[cfg(test)]
mod wait_interrupt_tests {
    use super::{
        BlockingWaitGuard, WaitInterruptCause, is_interruptible_wait_tool,
        should_flush_held_queue_before_wait,
    };
    #[test]
    fn interrupted_wait_result_does_not_consume_the_waited_task() {
        let result = super::interrupted_wait_tool_result(
            &serde_json::json!({"task_ids": ["bg-running"], "timeout_ms": 120_000}),
            WaitInterruptCause::HumanInterjection,
        );
        assert!(
            distill_tools::reminders::task_completion::consumed_completion_ids(&result.output)
                .is_empty()
        );
    }
    #[test]
    fn wait_start_flush_skips_queue_mode_and_active_goals() {
        assert!(should_flush_held_queue_before_wait(true, true, false));
        assert!(
            !should_flush_held_queue_before_wait(true, false, false),
            "queue mode must keep follow-ups held across a wait"
        );
        assert!(
            !should_flush_held_queue_before_wait(true, true, true),
            "an active goal wait is not sendable; promoting would abort it immediately"
        );
        assert!(!should_flush_held_queue_before_wait(false, true, false));
    }
    #[test]
    fn interruptible_wait_tool_only_when_timeout_positive() {
        assert!(is_interruptible_wait_tool(
            "get_command_or_subagent_output",
            &serde_json::json!({"task_ids": ["t"], "timeout_ms": 120_000})
        ));
        assert!(!is_interruptible_wait_tool(
            "get_task_output",
            &serde_json::json!({"task_ids": ["t"], "timeout_ms": 0})
        ));
        assert!(!is_interruptible_wait_tool(
            "get_task_output",
            &serde_json::json!({"task_ids": ["t"]})
        ));
        assert!(is_interruptible_wait_tool(
            "wait_commands_or_subagents",
            &serde_json::json!({"task_ids": ["t"]})
        ));
        assert!(!is_interruptible_wait_tool(
            "read_file",
            &serde_json::json!({"target_file": "/tmp/x"})
        ));
    }
    /// `BlockingWaitGuard` counts nested waits; drop always decrements.
    #[test]
    fn blocking_wait_guard_counts_and_restores_on_drop() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        {
            let _g1 = BlockingWaitGuard::enter(depth.clone());
            assert_eq!(depth.depth(), 1);
            {
                let _g2 = BlockingWaitGuard::enter(depth.clone());
                assert_eq!(depth.depth(), 2);
            }
            assert_eq!(depth.depth(), 1);
        }
        assert_eq!(depth.depth(), 0, "drop must restore");
    }
    /// An aborted wait future must not leak the depth count.
    #[tokio::test(start_paused = true)]
    async fn blocking_wait_guard_decrements_when_future_aborted() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        let inner = depth.clone();
        let task = tokio::spawn(async move {
            let _g = BlockingWaitGuard::enter(inner);
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        });
        tokio::task::yield_now().await;
        assert_eq!(depth.depth(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(depth.depth(), 0, "abort must not leak");
    }
    #[test]
    fn blocking_wait_guard_reset_is_generation_scoped() {
        use std::sync::Arc;
        let depth = Arc::new(crate::tools::tool_context::BlockingWaitState::new());
        let old = BlockingWaitGuard::enter(depth.clone());
        assert_eq!(depth.depth(), 1);
        depth.reset();
        let new = BlockingWaitGuard::enter(depth.clone());
        assert_eq!(depth.depth(), 1);
        drop(old);
        assert_eq!(
            depth.depth(),
            1,
            "old-generation drop must not consume the new wait"
        );
        drop(new);
        assert_eq!(depth.depth(), 0);
    }
}
