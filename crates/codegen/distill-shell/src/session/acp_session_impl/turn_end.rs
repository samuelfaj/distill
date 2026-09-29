// Modified for Distill by Samuel Fajreldines, 2026.
//! `SessionActor` methods that run when a turn ends: completion handling and turn-error classification.

use super::turn_end_hooks::{cancel_details, cancel_reason_for_completion};
use super::*;
use prod_mc_cli_chat_proxy_types::feedback_types;

fn completion_cancel_trigger(result: &PromptTurnResult) -> Option<&str> {
    match result.as_ref().ok()?.completion_kind {
        PromptCompletionKind::Cancelled {
            context: Some(ref ctx),
            ..
        } => ctx.trigger.as_deref(),
        _ => None,
    }
}

impl SessionActor {
    /// Emit a cosmetic `Plan` update at turn end to clear stale spinners.
    /// When the model ends the turn without a cleanup `todo_write` call, remaining `in_progress` items keep spinning in the UI.
    /// No-op if no `in_progress` items exist.
    pub(super) async fn emit_turn_end_plan_cleanup(&self) {
        use crate::tools::todo::{TodoState, TodoStatus, plan_entry_from_todo_item};
        use distill_tools::types::resources::State;

        // Read the current TodoState (no mutation).
        let (entries, stale_count) = {
            let res = self
                .agent
                .borrow()
                .tool_bridge()
                .read_resource::<State<TodoState>>()
                .await;
            let Some(state) = res else {
                return; // No todo state.
            };

            let stale_count = state
                .0
                .todo_items()
                .filter(|t| t.status == TodoStatus::InProgress)
                .count();
            if stale_count == 0 {
                return;
            }

            // Build plan entries, showing in_progress items as completed
            // `plan_entry_from_todo_item` keeps cancelled metadata and priority
            let entries: Vec<_> = state
                .0
                .todo_items()
                .map(|item| {
                    let mut entry = plan_entry_from_todo_item(item.clone());
                    if item.status == TodoStatus::InProgress {
                        entry.status = acp::PlanEntryStatus::Completed;
                    }
                    entry
                })
                .collect();

            (entries, stale_count)
        };

        tracing::info!(
            stale_count,
            "emitting transient turn-end Plan cleanup — in_progress shown as completed"
        );

        // Transient: this cosmetic UI update must not be persisted or replayed on session reload
        // The real TodoState in Resources is the source of truth
        let notification = acp::SessionNotification::new(
            self.session_info.id.clone(),
            acp::SessionUpdate::Plan(acp::Plan::new(entries)),
        );
        self.emit_transient_notification(notification);
    }

    /// Emit `x.ai/git_head_changed` after an edit or shell command that may have moved HEAD (e.g. `git checkout`, `git commit`).
    /// Clients then update their status bar and changes panel immediately instead of waiting for the debounced fs-watch refresh.
    pub(super) async fn maybe_notify_git_branch(&self) {
        if !self.git_head_enabled {
            return;
        }
        let cwd = self.tool_context.cwd.as_path();

        // `get_worktree_info` doubles as the "in a git repo?" probe (None when not).
        let (worktree_info, branch, commit) = tokio::join!(
            distill_workspace::session::git::get_worktree_info(cwd),
            distill_workspace::session::git::get_branch(cwd),
            distill_workspace::session::git::get_current_commit(cwd),
        );
        let Some((is_worktree, main_repo)) = worktree_info else {
            return;
        };

        let dedup_key = git_head_dedup_key(
            branch.as_deref(),
            is_worktree,
            main_repo.as_deref(),
            commit.as_deref(),
        );
        {
            let mut last = self.last_reported_branch.lock();
            if last.as_deref() == Some(&dedup_key) {
                return;
            }
            *last = Some(dedup_key);
        }

        let params = distill_workspace::session::git::GitHeadChanged {
            session_id: self.session_info.id.0.to_string(),
            branch,
            is_worktree,
            main_repo,
        };
        if let Ok(raw) = serde_json::value::to_raw_value(&params) {
            let notification = acp::ExtNotification::new("x.ai/git_head_changed", raw.into());
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }

        // The status row carries `workspace.branch`, and HEAD has just moved
        // This sits inside the `git_head_enabled` gate above
        // A client that did not ask for HEAD notifications refreshes its row at turn end instead
        self.emit_status_snapshot_detached();
    }

    /// Asks the subagent tracker which subagents are still live for this prompt and whether any subagent usage went unapplied.
    /// Returns `None` if the query failed.
    pub(super) async fn outstanding_reply_for_prompt(
        &self,
        prompt_id: &str,
    ) -> Option<distill_tools::implementations::distill::task::types::SubagentOutstandingReply>
    {
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return Some(Default::default());
        };
        use distill_tools::implementations::distill::task::types::{
            SubagentEvent, SubagentOutstandingRequest,
        };
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::Outstanding(SubagentOutstandingRequest {
                parent_session_id: self.session_id_string(),
                prompt_id: prompt_id.to_string(),
                respond_to,
            }))
            .is_err()
        {
            return None;
        }
        rx.await.ok()
    }

    /// Whether usage should be reported incomplete; the error-path attach and tests call this.
    /// Delegates to [`super::turn::UsageDrainOutcome::report_incomplete`].
    pub(super) fn usage_incomplete_from_reply(
        reply: Option<
            &distill_tools::implementations::distill::task::types::SubagentOutstandingReply,
        >,
    ) -> bool {
        super::turn::UsageDrainOutcome::from_outstanding_reply(reply).report_incomplete()
    }

    pub(super) fn clear_subagent_usage_not_applied(&self, prompt_id: &str) {
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return;
        };
        use distill_tools::implementations::distill::task::types::{
            SubagentClearUsageNotAppliedRequest, SubagentEvent,
        };
        let _ = tx.send(SubagentEvent::ClearUsageNotApplied(
            SubagentClearUsageNotAppliedRequest {
                parent_session_id: self.session_id_string(),
                prompt_id: prompt_id.to_string(),
            },
        ));
    }

    /// Returns whether this handler OWNED the completion (it matched and dequeued the front prompt).
    /// `false` means a stale completion for a turn another path (e.g. Cancel) already finalized; callers must not treat it as a turn ending now.
    pub(super) async fn handle_completion(
        &self,
        prompt_id: String,
        epoch: TurnEpoch,
        task_identity: &TaskIdentity,
        result: PromptTurnResult,
        elapsed_ms: Option<u64>,
    ) -> bool {
        let Some(mut lease) =
            self.state
                .lock()
                .await
                .claim_task_finalization(&prompt_id, epoch, task_identity)
        else {
            let state = self.state.lock().await;
            tracing::warn!("Received stale completion for prompt: {prompt_id}");
            distill_telemetry::unified_log::warn(
                "shell.turn.stale_completion_dropped",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": prompt_id,
                    "running_prompt_id": state.running_prompt_id(),
                })),
            );
            return false;
        };

        let result = result.map(|mut ok| {
            ok.tool_overrides = self.effective_tool_overrides();
            ok
        });
        let should_flush_reminders = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .as_deref()
            .is_none_or(|current| current == prompt_id);
        if should_flush_reminders {
            self.flush_pending_skill_reminders().await;
        }

        // Read before the lock: the report below runs under it and cannot await.
        let last_assistant_message = match result.as_ref().ok() {
            Some(ok) if cancel_reason_for_completion(&ok.completion_kind).is_some() => {
                self.last_assistant_message_for_cancel().await
            }
            _ => None,
        };
        let mut state = self.state.lock().await;
        // True only when this completion matched the front prompt and dequeued it
        // The unknown-prompt branch below must NOT emit a terminal: a turn the Cancel path already finalized can leave a stale completion here
        let mut owned_completion = false;
        let mut broadcast_queue = false;
        if state
            .pending_inputs
            .front()
            .is_some_and(|input| input.prompt_id == prompt_id)
        {
            let Some(input) = state.pending_inputs.pop_front() else {
                return false;
            };
            owned_completion = true;
            let _ = input.respond_to.send(result.clone()).ok();
            // The completed prompt left the queue, so re-broadcast the authoritative queue
            // That happens below, after `running_task` clears, so the wire's `running_prompt_id` does not advertise the finished turn
            broadcast_queue = input.queue_meta.is_some();
        } else {
            tracing::warn!("Received completion for unknown prompt: {prompt_id}");
            distill_telemetry::unified_log::warn(
                "shell.turn.stale_completion_dropped",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": prompt_id,
                    "running_prompt_id": state.running_prompt_id(),
                })),
            );
        }
        let binding = distill_message_delivery_core::TurnBinding::new(prompt_id.clone(), epoch);
        let (message_completions, had_message_fallbacks) = self.transition_parent_messages(
            &mut state,
            distill_message_delivery_core::TerminalTarget::Turn(&binding),
            distill_message_delivery_core::TerminalCause::Completion,
        );
        broadcast_queue |= had_message_fallbacks;
        // Owned (dequeued at the front) completions only
        // The unknown-prompt branch above is a stale completion the Cancel path already finalized, and `RemovedFromQueue` never ran
        let finalizes_turn = owned_completion
            && !matches!(
                result,
                Ok(PromptTurnOk {
                    completion_kind: PromptCompletionKind::RemovedFromQueue,
                    ..
                })
            );
        // This runs under the lock that promotion needs and before the task is cleared, so this completion's turn is still the current one
        if finalizes_turn
            && let Ok(ok) = &result
            && let Some(reason) = cancel_reason_for_completion(&ok.completion_kind)
        {
            self.report_turn_end(
                &prompt_id,
                TurnEnd::Cancelled {
                    reason,
                    trigger: completion_cancel_trigger(&result).map(str::to_string),
                    reason_details: cancel_details(&ok.completion_kind),
                    last_assistant_message,
                },
            );
        }
        // The turn sets the queue hold at the block verdict (single writer)
        // Completion handling only announces the hold, and only while it is still set
        // A prompt that cleared the hold between the verdict and this completion must not be overridden here
        let mut held_rows_notice: Option<usize> = None;
        if finalizes_turn
            && state.hook_block_held()
            && !state.pending_inputs.is_empty()
            && matches!(
                result,
                Ok(PromptTurnOk {
                    completion_kind: PromptCompletionKind::Cancelled {
                        category: Some(crate::session::events::CancellationCategory::HookDenied),
                        ..
                    },
                    ..
                })
            )
        {
            held_rows_notice = Some(state.pending_inputs.len());
        }
        if broadcast_queue {
            self.broadcast_queue_changed(&state);
        }
        // Auto-compact runs inline in process_conversation_turn, so nothing queues it here after the turn

        // If the user toggled plan mode off while this turn ran (state == ExitPending), complete the deferred exit now that the turn is finished
        // The next handle_prompt() will inject the exit reminder via has_pending_exit_reminder()
        {
            let mut tracker = self.plan_mode.lock();
            let transitioned =
                tracker.state() == crate::session::plan_mode::PlanModeState::ExitPending;
            tracker.complete_deferred_exit();
            drop(tracker);
            if transitioned {
                self.persist_plan_mode_state();
            }
        }
        // Drop the state guard before sends and async emits.
        drop(state);
        Self::settle_parent_message_completions(message_completions, &result);

        if let Some(held) = held_rows_notice {
            self.send_hook_annotation(&format!(
                "\u{26a0} {held} queued prompt(s) on hold after the block. Edit or remove them, or send a prompt to resume."
            ))
            .await;
        }

        // Durable counterpart of the fire-and-forget `prompt_complete` emitted from `MvpAgent::prompt`.
        // The turn's terminal goes on the persisted and replayed `_x.ai/session/update` rail A viewer that re-attaches mid-turn then finalizes from replay instead of stranding on "Waiting…" The caller flushed the replay buffer.
        if finalizes_turn {
            let mapped = result
                .as_ref()
                .map(|ok| ok.stop_reason)
                .map_err(Clone::clone);
            let usage = match &result {
                Ok(ok) => ok.usage.clone(),
                Err(err) => {
                    if let Some(u) = crate::sampling::error::prompt_usage_from_error(err) {
                        Some(u)
                    } else {
                        self.error_path_usage_fallback(&prompt_id).await
                    }
                }
            };
            // `cancellationCategory` on the terminal `_meta` lets re-attaching viewers finalize with the same copy as the driver
            // A hook-denied turn must not render as "cancelled by user"
            let cancellation_category = result
                .as_ref()
                .ok()
                .and_then(|ok| ok.completion_kind.cancellation_category_meta());
            let cancellation_context = result
                .as_ref()
                .ok()
                .and_then(|ok| ok.completion_kind.cancellation_context_meta());
            self.emit_turn_completed(
                prompt_id.clone(),
                &mapped,
                usage,
                completion_cancel_trigger(&result),
                cancellation_category.as_deref(),
                cancellation_context,
                elapsed_ms,
                result
                    .as_ref()
                    .ok()
                    .and_then(|ok| ok.turn_snapshot.as_ref()),
            )
            .await;
        }
        if !self.finish_finalization_lease(&mut lease).await {
            tracing::error!(
                prompt_id,
                "claimed completion failed exact finalization release"
            );
            return false;
        }
        owned_completion
    }

    /// Emit the durable, replayable `TurnCompleted` terminal, the single path shared by `handle_completion` and `cancel_running_task`.
    /// `(stop_reason, agent_result)` come from `prompt_complete_fields`, the same source as `prompt_complete`, so the two signals never disagree.
    /// `cancel_trigger` (when `Some`) rides the `_meta` as `cancelTrigger`; `"send_now"` marks a cancel-and-send end (marker suppressed).
    pub(super) async fn emit_turn_completed(
        &self,
        prompt_id: String,
        mapped: &std::result::Result<acp::StopReason, acp::Error>,
        usage: Option<crate::extensions::notification::PromptUsage>,
        cancel_trigger: Option<&str>,
        cancellation_category: Option<&str>,
        cancellation_context: Option<serde_json::Value>,
        elapsed_ms: Option<u64>,
        snapshot: Option<&TurnDeltaSnapshot>,
    ) {
        let (stop_reason, agent_result, error_kind) =
            crate::sampling::error::prompt_complete_fields(mapped);
        let mut extra = serde_json::Map::new();
        if let Some(t) = cancel_trigger {
            extra.insert("cancelTrigger".to_string(), serde_json::json!(t));
        }
        if let Some(c) = cancellation_category {
            extra.insert("cancellationCategory".to_string(), serde_json::json!(c));
        }
        if let Some(ctx) = cancellation_context {
            extra.insert("cancellationContext".to_string(), ctx);
        }
        let extra_meta = (!extra.is_empty()).then_some(extra);
        self.send_xai_notification_with_extra_meta(
            crate::session::turn_completion::build_turn_completed(
                prompt_id.clone(),
                stop_reason,
                agent_result,
                error_kind,
                usage,
                elapsed_ms,
            ),
            extra_meta,
            crate::session::storage::jsonl::AppendDurability::Durable,
        )
        .await;

        // Behind the finalization lease, so a turn posts exactly one delta whichever path settles it.
        let turn_outcome = match mapped {
            Ok(acp::StopReason::Cancelled) => feedback_types::TurnOutcome::Cancelled,
            Ok(_) => feedback_types::TurnOutcome::Completed,
            Err(_) => feedback_types::TurnOutcome::Error,
        };
        let taken;
        let snapshot = match snapshot {
            Some(snapshot) => Some(snapshot),
            None => {
                taken = self
                    .signals_handle()
                    .take_unfinished_turn_snapshot()
                    .await
                    .map(|mut snapshot| {
                        self.apply_prompt_modes_to_snapshot(&mut snapshot);
                        snapshot
                    });
                taken.as_ref()
            }
        };
        self.report_turn_delta(&prompt_id, snapshot, elapsed_ms, turn_outcome)
            .await;

        // Cost, context occupancy and the turn timer all moved during the turn.
        self.emit_status_snapshot_detached();
    }

    /// Telemetry error category; delegates to `stop_failure_error_type` so the two classifications cannot drift.
    pub(super) fn classify_turn_error(err: &acp::Error) -> String {
        use distill_hooks::event::StopFailureKind as K;
        match Self::stop_failure_error_type(err) {
            K::RateLimit => "rate_limit",
            K::AuthenticationFailed => "auth",
            K::InvalidRequest => "invalid_request",
            K::ServerError => "internal",
            K::MaxOutputTokens => "max_tokens",
            K::Unknown => "unknown",
        }
        .to_string()
    }

    pub(super) fn turn_error_fields(err: &acp::Error) -> (String, String, Option<String>) {
        let category = Self::classify_turn_error(err);
        let code = crate::sampling::error::error_kind_str_from_error(err)
            .or_else(|| crate::sampling::error::error_code_from_data(err))
            .map(str::to_owned)
            .unwrap_or_else(|| category.clone());
        // Only HTTP 400 is the backend's bad-request reason; 403 content-safety and 404 model/auth enrichment also fold into invalid_request but must not ship their bodies.
        let detail =
            (crate::sampling::error::http_status_from_error(err) == Some(400)).then(|| {
                let named = crate::sampling::error::rewrite_service_names(
                    &crate::sampling::error::acp_error_message(err),
                );
                distill_telemetry::redact_error_detail(&named)
            });
        (category, code, detail)
    }

    /// The `StopFailure` hook input's classified `error`.
    /// Structured markers win over the JSON-RPC code because they are more specific; anything the runtime cannot distinguish stays `Unknown`.
    pub(super) fn stop_failure_error_type(
        err: &acp::Error,
    ) -> distill_hooks::event::StopFailureKind {
        use distill_hooks::event::StopFailureKind as K;
        if crate::sampling::error::is_max_tokens_turn_error(err) {
            return K::MaxOutputTokens;
        }
        // The HTTP status carried in `err.data` is more specific than the JSON-RPC code, so it is checked first 403 is content-safety, not auth.
        // On the turn path it carries `http_status: 403` and folds into `invalid_request`.
        // On the setup path it has no status, so `-32603` below makes it `server_error`.
        match crate::sampling::error::http_status_from_error(err) {
            Some(401) => return K::AuthenticationFailed,
            Some(429) | Some(503) | Some(529) => return K::RateLimit,
            Some(s) if (400..500).contains(&s) => return K::InvalidRequest,
            Some(s) if s >= 500 => return K::ServerError,
            _ => {}
        }
        match i32::from(err.code) {
            crate::sampling::error::RATE_LIMITED_ERROR_CODE => K::RateLimit,
            -32000 => K::AuthenticationFailed,
            -32002 | -32600 | -32602 => K::InvalidRequest,
            -32603 => K::ServerError,
            _ => K::Unknown,
        }
    }

    /// Whether a turn error is transient infra worth a goal retry.
    /// Keys on the JSON-RPC code only (unlike `stop_failure_error_type`), so `-32603` counts as infra.
    pub(super) fn is_infra_turn_error(err: &acp::Error) -> bool {
        matches!(
            i32::from(err.code),
            crate::sampling::error::RATE_LIMITED_ERROR_CODE | -32000 | -32603
        )
    }

    /// `(turn_succeeded, suppress_goal_continuation, infra_pause_message)`.
    /// StationarityEnded counts as success for the goal streak but skips the GoalSummary re-queue.
    /// `infra_pause_message` is extracted before `handle_completion` consumes `result`.
    pub(super) fn post_turn_goal_degradation_plan(
        result: &PromptTurnResult,
    ) -> (bool, bool, Option<String>) {
        let suppress_goal_continuation = result.as_ref().ok().is_some_and(|ok| {
            matches!(
                ok.completion_kind,
                crate::session::commands::PromptCompletionKind::StationarityEnded
            )
        });
        let turn_cancelled = result.as_ref().ok().is_some_and(|ok| {
            matches!(
                ok.completion_kind,
                crate::session::commands::PromptCompletionKind::Cancelled { .. }
                    | crate::session::commands::PromptCompletionKind::MaxTurnsReached { .. }
            )
        });
        let turn_succeeded = result
            .as_ref()
            .ok()
            .is_some_and(|ok| !turn_cancelled && ok.stop_reason != acp::StopReason::Refusal);
        let infra_pause_message = result
            .as_ref()
            .err()
            .filter(|err| Self::is_infra_turn_error(err))
            .map(Self::format_turn_error_message);
        (
            turn_succeeded,
            suppress_goal_continuation,
            infra_pause_message,
        )
    }

    pub(super) async fn apply_infra_pause_after_turn_err(&self, message: String) -> bool {
        let slash_detail = match message.strip_prefix("Turn failed: ") {
            Some(rest) => rest.to_owned(),
            None => message.clone(),
        };
        let paused = self
            .auto_pause_goal_if_active_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                message,
            )
            .await;
        if paused {
            self.send_slash_command_output(&format!(
                "Goal paused due to turn error: {slash_detail}. Use /goal resume to retry."
            ))
            .await;
        }
        paused
    }

    /// Extract the best human-readable detail from an infra turn error.
    pub(super) fn turn_error_detail(err: &acp::Error) -> Option<String> {
        err.data
            .as_ref()
            .and_then(crate::sampling::error::error_detail_from_data)
            .or_else(|| {
                if !err.message.is_empty() {
                    Some(err.message.clone())
                } else {
                    None
                }
            })
    }

    pub(super) fn format_turn_error_message(err: &acp::Error) -> String {
        if let Some(detail) = Self::turn_error_detail(err) {
            format!("Turn failed: {detail}")
        } else {
            format!("Turn failed: {}", Self::classify_turn_error(err))
        }
    }
}

#[cfg(test)]
mod turn_error_fields_tests {
    use super::*;

    fn err_with_status(message: &str, status: u16) -> acp::Error {
        acp::Error::internal_error().data(crate::sampling::error::error_data_with_status(
            message.to_string(),
            Some(status),
        ))
    }

    #[test]
    fn http_400_detail_is_redacted_and_a_closed_code_is_emitted() {
        let err = err_with_status(
            "invalid request https://api.example.com/v1/chat?token=CANARYSECRET",
            400,
        );
        let (_category, code, detail) = SessionActor::turn_error_fields(&err);
        assert!(!code.is_empty(), "a closed error_code is always emitted");
        let detail = detail.expect("an HTTP 400 turn error ships a redacted detail");
        assert!(
            !detail.contains("CANARYSECRET"),
            "url token survived: {detail}"
        );
        assert!(!detail.contains("/v1/chat"), "url path survived: {detail}");
    }

    #[test]
    fn detail_is_capped() {
        let err = err_with_status(&"a".repeat(1000), 400);
        let (_c, _code, detail) = SessionActor::turn_error_fields(&err);
        assert_eq!(
            detail.expect("400 ships a detail").chars().count(),
            256,
            "detail must be capped"
        );
    }

    #[test]
    fn non_400_errors_omit_detail() {
        // 401 auth, 403 content-safety, 429 rate-limit, 500 server all fold into
        // invalid_request/other but must never ship their (PII-bearing) bodies.
        for status in [401u16, 403, 429, 500] {
            let err = err_with_status("secret path /Users/someone/keys", status);
            let (_c, _code, detail) = SessionActor::turn_error_fields(&err);
            assert!(detail.is_none(), "status {status} must not ship a detail");
        }
    }
}
