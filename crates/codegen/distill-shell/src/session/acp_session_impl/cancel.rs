// Modified for Distill by Samuel Fajreldines, 2026.
//! Cancellation, send-now, rewind, reporting, and terminal cleanup for `SessionActor`.

use super::*;

/// Whether the post-cancel notification drain stays suppressed: rewind and non-stop cancels clear it, a stop gesture arms it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WakeBarrier {
    Armed,
    Clear,
}

/// Outcome of `cancel_running_task`.
#[must_use = "the notification drain and the idle ping both depend on this"]
pub(super) struct CancelOutcome {
    pub(super) barrier: WakeBarrier,
    /// A user cancel with a classified reason tore down a turn, a rewind included.
    /// Whether that leaves the session idle is the loop's call.
    pub(super) turn_stopped: bool,
    pub(super) settled: bool,
}

impl CancelOutcome {
    fn noop() -> Self {
        Self {
            barrier: WakeBarrier::Clear,
            turn_stopped: false,
            settled: false,
        }
    }
}

enum CancelFinalization {
    Rewind,
    Keep(FinalizationLease),
}

/// What a cancel needs to report the turn it tore down.
/// `epoch` is captured with the task under the state lock, so the report names that turn rather than whichever is current when it files.
pub(super) struct CancelledTurn<'a> {
    pub(super) prompt_id: &'a str,
    pub(super) epoch: TurnEpoch,
    pub(super) reason: distill_hooks::event::StopCancelledReason,
    pub(super) trigger: Option<String>,
    pub(super) last_assistant_message: Option<String>,
}

/// Only a client-visible user row can pop for rewind; an interjection fallback takes a plain cancel.
fn front_is_rewind_poppable(front: Option<&InputItem>) -> bool {
    front.is_some_and(|f| {
        matches!(
            f.input_origin.as_prompt_origin(),
            crate::session::PromptOrigin::User
        ) && !super::interjection::is_interject_fallback(&f.prompt_id)
    })
}

impl SessionActor {
    /// Turn-scoped: soft cancel / max-turns only (not user Stop).
    /// `parent_prompt_id` is the authoritative turn id from the turn runner.
    pub(super) fn cancel_running_turn_subagents(&self, parent_prompt_id: &str) {
        self.cancel_subagents_for_prompt_id(parent_prompt_id);
    }

    /// A user Stop with `cancel_subagents` cancels all of this session's non-workflow children.
    /// Uses the session-bound backend API so the cancel cannot touch other sessions.
    pub(super) fn cancel_all_session_subagents(&self) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use distill_tools::implementations::distill::task::backend::ChannelBackend;
            let backend = ChannelBackend::for_session(event_tx, self.session_id_string());
            let _ = backend.request_cancel_parent_session(tokio::sync::oneshot::channel().0);
        }
    }

    /// Re-open Task spawns for this session after a prior user Stop.
    pub(super) fn open_subagent_spawn_admission(&self) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use distill_tools::implementations::distill::task::backend::ChannelBackend;
            let backend = ChannelBackend::for_session(event_tx, self.session_id_string());
            let _ = backend.open_spawn_admission();
        }
    }

    fn cancel_subagents_for_prompt_id(&self, parent_prompt_id: &str) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use distill_tools::implementations::distill::task::types::{
                SubagentCancelRequest, SubagentCancelTarget, SubagentEvent,
            };
            let _ = event_tx.send(SubagentEvent::Cancel(SubagentCancelRequest {
                parent_session_id: Some(self.session_id_string()),
                target: SubagentCancelTarget::ParentPromptId(parent_prompt_id.to_string()),
                respond_to: tokio::sync::oneshot::channel().0,
            }));
        }
    }

    /// Takes the state lock with `try_lock`, so it must run before its caller's first await: a task suspended across one still holds the guard.
    fn arm_wake_barrier(&self, trigger: Option<&crate::session::CancelTrigger>) {
        if let Some(gate) = &self.tool_context.task_wake_suppressed {
            gate.set(true);
        }
        let mut state = self.state.try_lock().expect("session state is actor-owned");
        state.notifications_suppressed = true;
        distill_telemetry::unified_log::info(
            "shell.task_wake.cancel_barrier",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "trigger": trigger.map(crate::session::CancelTrigger::as_str),
                "gate": self
                    .tool_context
                    .task_wake_suppressed
                    .as_ref()
                    .is_some_and(|gate| gate.get()),
                "state": state.notifications_suppressed,
            })),
        );
        drop(state);
        if let Some(is_turn_active) = &self.tool_context.is_turn_active {
            is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn abort_turn_task(&self, task: &AgentTask, epoch: super::turn_report_slot::TurnEpoch) {
        self.turn_phases.emit_pending_latency();
        task.abort();
        self.turn_report.release_aborted(epoch);
    }

    /// The Ctrl+C teardown, except the running command moves to the background instead of being killed.
    /// Background tasks, subagents, and the queue are untouched.
    pub(super) async fn cancel_turn_for_send_now(
        &self,
        replay_buffer: &mut ReplayBuffer,
    ) -> CancelOutcome {
        if let Some(notification) = replay_buffer.flush() {
            self.emit_buffered(notification).await;
        }
        let flushed = self.flush_stranded_interjections().await;
        if flushed > 0 {
            distill_telemetry::unified_log::info(
                "shell.prompt.send_now_flushed_interjections",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "count": flushed })),
            );
        }
        let outcome = self
            .cancel_running_task(crate::session::CancelOptions {
                trigger: Some(crate::session::CancelTrigger::SendNow),
                ..Default::default()
            })
            .await;
        if !outcome.settled {
            return outcome;
        }
        if let Some(gate) = &self.tool_context.task_wake_suppressed {
            gate.set(false);
        }
        {
            let mut state = self.state.lock().await;
            state.notifications_suppressed = false;
            if state.take_hook_block_hold() {
                distill_telemetry::unified_log::info(
                    "shell.prompt.hook_block_hold_released",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "send_now" })),
                );
            }
        }
        distill_telemetry::unified_log::info(
            "shell.task_wake.gate_cleared",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({ "reason": "send_now" })),
        );
        outcome
    }

    /// Names the turn this cancel tore down, so a turn promoted in between is refused rather than reported over.
    fn report_cancelled_turn(&self, cancelled: CancelledTurn<'_>) {
        self.claim_and_queue(
            cancelled.prompt_id,
            cancelled.epoch,
            TurnEnd::Cancelled {
                reason: cancelled.reason,
                trigger: cancelled.trigger,
                reason_details: None,
                last_assistant_message: cancelled.last_assistant_message,
            },
        );
    }

    /// One line per rewind-requested cancel: `rewound | legacy_rewound | queued_row_removed | stale_prompt_id | window_closed | non_user_front`.
    fn log_rewind_decision(
        &self,
        requested_prompt_id: Option<&str>,
        front_prompt_id: Option<&str>,
        rewind_disposition: &str,
    ) {
        distill_telemetry::unified_log::info(
            "shell.cancel.rewind_decision",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "requested_prompt_id": requested_prompt_id,
                "front_prompt_id": front_prompt_id,
                "rewind_disposition": rewind_disposition,
            })),
        );
    }

    /// Trim the rewound turn from in-memory history and resolve it as `Rewound`.
    /// `target_prompt_index` is the cut captured when the rewind was claimed; `None` uses a fresh snapshot (the legacy path with no prompt id).
    async fn finish_rewound_cancel(
        &self,
        input: InputItem,
        target_prompt_index: Option<usize>,
        total_tokens: u64,
        turn_stopped: bool,
    ) -> CancelOutcome {
        if let Some(mut snapshot) = self.chat_state_handle.snapshot().await {
            let target_prompt_index =
                target_prompt_index.unwrap_or_else(|| snapshot.prompt_index.saturating_sub(1));
            snapshot.prompt_index = target_prompt_index;
            snapshot.prompt_texts.truncate(target_prompt_index);
            // The cut counts turns by prompt markers; this path never replays, so conversation after a compact relies on the marker
            let keep_count =
                conversation_truncate_for_prompt(&snapshot.conversation, target_prompt_index);
            snapshot.conversation.truncate(keep_count);
            self.chat_state_handle.restore_snapshot(snapshot);
            self.file_state_tracker
                .truncate_from(target_prompt_index)
                .await;
        }
        let _ = input.respond_to.send(Ok(PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens,
            turn_snapshot: None,
            completion_kind: PromptCompletionKind::Rewound,
            structured_output: None,
            usage: None,
            tool_overrides: self.effective_tool_overrides(),
        }));
        // The rewind cleared the barrier when it claimed/popped; wakes flow.
        CancelOutcome {
            barrier: WakeBarrier::Clear,
            turn_stopped,
            settled: true,
        }
    }

    #[tracing::instrument(name = "session.cancel", skip_all, fields(trigger = ?options.trigger.as_ref().map(crate::session::CancelTrigger::kind)))]
    pub(super) async fn cancel_running_task(
        &self,
        options: crate::session::CancelOptions,
    ) -> CancelOutcome {
        let kind = options
            .trigger
            .as_ref()
            .map(crate::session::CancelTrigger::kind);
        let cancel_reason = super::turn_end_hooks::cancel_reason_for_options(&options);
        let crate::session::CancelOptions {
            cancel_subagents,
            kill_background_tasks,
            history,
            trigger,
            user_initiated,
        } = options;
        let rewind_requested = matches!(
            history,
            crate::session::CancelHistoryDisposition::RewindIfNoOutput { .. }
        );
        let requested_prompt_id = match &history {
            crate::session::CancelHistoryDisposition::RewindIfNoOutput { prompt_id } => {
                prompt_id.clone()
            }
            crate::session::CancelHistoryDisposition::Keep => None,
        };
        // Claim the named front under one lock before teardown; a stale id (a new turn already promoted) is a no-op
        // Legacy (no id) uses the path below
        let mut claimed_rewound: Option<(
            InputItem,
            TurnEpoch,
            usize,
            Vec<super::parent_message::ParentOwnedDelivery>,
        )> = None;
        if let Some(requested) = requested_prompt_id.as_deref() {
            let mut state = self.state.lock().await;
            let front_prompt_id = state.pending_inputs.front().map(|f| f.prompt_id.clone());
            if front_prompt_id.as_deref() != Some(requested) {
                // The client gave up on a prompt that landed behind the running turn: drop the row now,
                // regardless of the rewind window, or it runs later against a client that shows Idle
                let queued_behind_front = if Self::is_running_prompt(&state, requested) {
                    None
                } else {
                    state
                        .pending_inputs
                        .iter()
                        .position(|item| item.prompt_id == requested)
                };
                let rewind_disposition = match queued_behind_front {
                    Some(pos) => {
                        Self::remove_pending_row(&mut state, pos);
                        self.broadcast_queue_changed(&state);
                        "queued_row_removed"
                    }
                    None => "stale_prompt_id",
                };
                drop(state);
                self.log_rewind_decision(
                    Some(requested),
                    front_prompt_id.as_deref(),
                    rewind_disposition,
                );
                return CancelOutcome::noop();
            }
            let poppable = front_is_rewind_poppable(state.pending_inputs.front());
            let rewind_disposition = if state.rewindable && poppable {
                // Drain here; a claimed rewind never reaches the generic sweep
                self.sweep_monitor_buffer_into_pending(&mut state, "monitor-cancel-drain");
                let turn_epoch = self.turn_report.epoch();
                if let Some(task) = state.running_task.take_if(|t| t.prompt_id == requested) {
                    self.abort_turn_task(&task, turn_epoch);
                    self.cancel_active_sampling_requests();
                }
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                state.notifications_suppressed = false;
                distill_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "rewind" })),
                );
                state.rewindable = false;
                // The cut is captured under the lock so later commits cannot move it
                let target_prompt_index = self
                    .chat_state_handle
                    .get_prompt_index()
                    .await
                    .saturating_sub(1);
                let binding = distill_message_delivery_core::TurnBinding::new(
                    requested.to_owned(),
                    turn_epoch,
                );
                let (message_completions, had_fallbacks) = self.transition_parent_messages(
                    &mut state,
                    distill_message_delivery_core::TerminalTarget::Turn(&binding),
                    distill_message_delivery_core::TerminalCause::Rewind,
                );
                claimed_rewound = state
                    .pending_inputs
                    .pop_front()
                    .map(|input| (input, turn_epoch, target_prompt_index, message_completions));
                // Broadcast after pop_front so clients do not see the cut prompt
                // still queued alongside the new parent-message fallback rows.
                if had_fallbacks {
                    self.broadcast_queue_changed(&state);
                }
                "rewound"
            } else if !state.rewindable {
                // Window closed: fall through to a normal cancel of this front.
                "window_closed"
            } else {
                "non_user_front"
            };
            drop(state);
            self.log_rewind_decision(
                Some(requested),
                front_prompt_id.as_deref(),
                rewind_disposition,
            );
        }
        // A claimed rewind owns only the old turn
        // The generic teardown targets whatever is running now and must not run; a new turn may already be promoted
        if let Some((input, epoch, target_prompt_index, message_completions)) = claimed_rewound {
            let _strip_guard = self.prepare_image_strips_for_rewind().await;
            self.cancel_active_sampling_requests();
            self.cancel_pending_image_strips_for_rewind();
            self.notify_turn_abort(epoch, distill_agent_lifecycle::TurnAbortReason::Interrupted)
                .await;
            let total_tokens = self.chat_state_handle.get_total_tokens().await;
            let result = Ok(PromptTurnOk {
                stop_reason: acp::StopReason::Cancelled,
                total_tokens,
                turn_snapshot: None,
                completion_kind: PromptCompletionKind::Rewound,
                structured_output: None,
                usage: None,
                tool_overrides: self.effective_tool_overrides(),
            });
            Self::settle_parent_message_completions(message_completions, &result);
            return self
                .finish_rewound_cancel(
                    input,
                    Some(target_prompt_index),
                    total_tokens,
                    cancel_reason.is_some(),
                )
                .await;
        }
        let mut finalization = if rewind_requested {
            CancelFinalization::Rewind
        } else {
            let Some(lease) = self.state.lock().await.claim_cancel_finalization() else {
                return CancelOutcome::noop();
            };
            CancelFinalization::Keep(lease)
        };
        let claimed_turn_epoch = match &finalization {
            CancelFinalization::Rewind => None,
            CancelFinalization::Keep(lease) => lease.binding.epoch(),
        };
        let suppress_task_wakes = kind == Some(crate::session::CancelKind::StopGesture);
        // Abort in-flight `/compact` or auto-compact generation (via the stream select and the pre-replace guard)
        // Safe when no compact is running
        self.compaction.cancel.request_cancel();
        if suppress_task_wakes {
            self.arm_wake_barrier(trigger.as_ref());
        }

        // This unified-log marker is the counterpart of `shell.cancel.received` in `MvpAgent::cancel`.
        // The pin is only a snapshot: the authoritative cancel identity is `running_task.prompt_id`, captured under the state lock below `current_prompt_id` is cleared early (turn scope guard drop / `handle_completion`) while.
        // Keying the durable `TurnCompleted` on the pin alone would lose the terminal (and its `cancelTrigger`) in that window.
        let pinned_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        {
            distill_telemetry::unified_log::info(
                "shell.cancel.processing",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": &pinned_prompt_id,
                    "cancel_subagents": cancel_subagents,
                    "kill_background_tasks": kill_background_tasks,
                    "rewind_if_no_output": rewind_requested,
                    "requested_prompt_id": &requested_prompt_id,
                    "trigger": trigger.as_ref().map(crate::session::CancelTrigger::as_str),
                })),
            );
        }

        // Sample before the abort and the teardown awaits, so the persisted cancel elapsed is turn runtime, not post-abort kill/token work
        // `None` when no running task supplies a start time (do not persist `Some(0)`)
        let cancel_elapsed_ms = {
            let state = self.state.lock().await;
            let cancel_elapsed_ms = state
                .running_task
                .as_ref()
                .map(|t| elapsed_ms_saturating(t.started_at, std::time::Instant::now()));
            if cancel_subagents
                && let Some(epoch) = claimed_turn_epoch
                && let Some(task) = state.running_task.as_ref()
            {
                self.abort_turn_task(task, epoch);
                self.cancel_active_sampling_requests();
            }
            cancel_elapsed_ms
        };

        if cancel_subagents {
            // Then cancel every non-workflow session child (including prior turns) and close spawn admission until the next turn opens it
            self.cancel_all_session_subagents();
        }

        // After the abort, so this chat-state round trip cannot keep the turn task alive, and before the teardown edits the text
        let last_assistant_message = if cancel_reason.is_some() {
            self.last_assistant_message_for_cancel().await
        } else {
            None
        };

        // A send-now redirect is the user continuing, not stopping, so it never kills an in-flight command.
        let send_now = matches!(trigger, Some(crate::session::CancelTrigger::SendNow));

        // Kill all running foreground terminal processes before aborting the task.
        // Send-now skips this and backgrounds them after the abort.
        // A narrow race exists: the running task could spawn a new terminal between this call and the abort() below In practice this is negligible; abort() drops the future and any child handle it owns.
        if !send_now {
            self.kill_foreground_commands_for_cancel().await;
        }

        if kill_background_tasks {
            if self.startup_hints.is_subagent {
                // Subagent teardown: only kill tasks owned by this session, not the parent's or sibling's tasks on the shared backend
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks_by_owner(&self.session_info.id.0)
                    .await;
            } else {
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks()
                    .await;
            }
        }

        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        let (
            cancelled_prompt_id,
            pending_inputs,
            rewound_input,
            had_queued_user_prompt,
            turn_epoch,
            message_completions,
        ) = {
            let mut state = self.state.lock().await;
            debug_assert!(
                pinned_prompt_id.is_none()
                    || state.running_prompt_id().is_none()
                    || state.running_prompt_id() == pinned_prompt_id.as_deref(),
                "current_prompt_id pin disagrees with running_task identity"
            );

            if let CancelFinalization::Keep(lease) = &finalization
                && !state.finalization_binding_is_current(lease)
            {
                tracing::error!(
                    binding = ?lease.binding,
                    "claimed cancel lost its exact finalization binding"
                );
                // Fail-stop, but do not strand StopGesture suppression (same
                // cleanup as the identity-mismatch return below).
                if suppress_task_wakes {
                    if let Some(gate) = &self.tool_context.task_wake_suppressed {
                        gate.set(false);
                    }
                    state.notifications_suppressed = false;
                }
                return CancelOutcome::noop();
            }

            // Closes the race between abort() and the TurnActiveGuard drop: is_turn_active may still be true
            // InjectNotification would then route Next-priority events to the buffer instead of pending_notifications
            // Moving them here keeps them in the queue; an interactive stop defers their drain, other cancels do not
            self.sweep_monitor_buffer_into_pending(&mut state, "monitor-cancel-drain");

            // When killing all background tasks, also clear their pending notifications; the monitors that produced them are now dead
            if kill_background_tasks {
                state.clear_pending_notifications();
            }

            let front_is_user_row = front_is_rewind_poppable(state.pending_inputs.front());
            let front_prompt_id = state.pending_inputs.front().map(|f| f.prompt_id.clone());
            // Window-closed / non-user-front: the front can change across the teardown awaits; a now-stale id must not cancel the promoted turn
            let identity_matches = requested_prompt_id
                .as_deref()
                .is_none_or(|id| front_prompt_id.as_deref() == Some(id));
            let was_rewindable = state.rewindable;
            // Rewind holds no lease, so it reads the live epoch; a no-task cancel has none.
            let turn_epoch =
                claimed_turn_epoch.or_else(|| rewind_requested.then(|| self.turn_report.epoch()));
            let mut rewound_task_prompt_id = None;
            let rewound_input = if rewind_requested
                && requested_prompt_id.is_none()
                && state.rewindable
                && front_is_user_row
            {
                if let Some(task) = state.running_task.take()
                    && let Some(epoch) = turn_epoch
                {
                    self.abort_turn_task(&task, epoch);
                    rewound_task_prompt_id = Some(task.prompt_id);
                }
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                state.notifications_suppressed = false;
                distill_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "rewind" })),
                );
                state.rewindable = false;
                state.pending_inputs.pop_front()
            } else {
                None
            };
            if rewind_requested && requested_prompt_id.is_none() {
                self.log_rewind_decision(
                    None,
                    front_prompt_id.as_deref(),
                    if rewound_input.is_some() {
                        "legacy_rewound"
                    } else if !was_rewindable {
                        "window_closed"
                    } else {
                        "non_user_front"
                    },
                );
            }
            // Binding of the task actually torn down here; rewinds keep it `None`
            // so their terminal transition targets `All` (matching the cause).
            let mut binding = None;
            let cancelled_prompt_id = if !identity_matches {
                None
            } else if rewound_input.is_some() {
                // The legacy rewind aborted the live task above; name it so the
                // tore-down-task teardown below runs for it.
                rewound_task_prompt_id
            } else if let CancelFinalization::Keep(_) = &finalization {
                state.running_task.as_ref().map(|task| {
                    if let Some(epoch) = turn_epoch {
                        self.abort_turn_task(task, epoch);
                    }
                    binding = Some(distill_message_delivery_core::TurnBinding::new(
                        task.prompt_id.clone(),
                        task.epoch,
                    ));
                    task.prompt_id.clone()
                })
            } else {
                state.running_task.take().map(|task| {
                    if let Some(epoch) = turn_epoch {
                        self.abort_turn_task(&task, epoch);
                    }
                    binding = Some(distill_message_delivery_core::TurnBinding::new(
                        task.prompt_id.clone(),
                        task.epoch,
                    ));
                    task.prompt_id
                })
            };
            // The front changed across our awaits; leave the promoted turn alone
            if !identity_matches {
                self.log_rewind_decision(
                    requested_prompt_id.as_deref(),
                    front_prompt_id.as_deref(),
                    "stale_prompt_id",
                );
                if suppress_task_wakes {
                    if let Some(gate) = &self.tool_context.task_wake_suppressed {
                        gate.set(false);
                    }
                    state.notifications_suppressed = false;
                }
                return CancelOutcome::noop();
            }

            // Only an interactive stop (Ctrl+C / Esc / [stop]) also removes queued task/workflow completion wakes.
            // Preserve real user prompts and unrelated synthetic entries so `maybe_start_running_task` can promote the next genuine user turn.
            // The cancelling client does not pull any prompt back into its input; the server queue is the single source of truth for what runs next.
            let message_cause = if kill_background_tasks {
                distill_message_delivery_core::TerminalCause::HardTeardown
            } else if rewound_input.is_some() {
                distill_message_delivery_core::TerminalCause::Rewind
            } else {
                distill_message_delivery_core::TerminalCause::SoftCancel
            };
            let message_target =
                if message_cause == distill_message_delivery_core::TerminalCause::HardTeardown {
                    distill_message_delivery_core::TerminalTarget::All
                } else {
                    binding.as_ref().map_or(
                        distill_message_delivery_core::TerminalTarget::All,
                        distill_message_delivery_core::TerminalTarget::Turn,
                    )
                };
            let (message_completions, had_message_fallbacks) =
                self.transition_parent_messages(&mut state, message_target, message_cause);
            let pending_inputs = if rewound_input.is_some() {
                VecDeque::new()
            } else if kill_background_tasks {
                std::mem::take(&mut state.pending_inputs)
            } else {
                let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
                let mut cancelled = VecDeque::new();
                for (idx, item) in std::mem::take(&mut state.pending_inputs)
                    .into_iter()
                    .enumerate()
                {
                    let is_running_turn = idx == 0;
                    if is_running_turn {
                        cancelled.push_back(item);
                    } else if suppress_task_wakes
                        && matches!(
                            item.input_origin.as_prompt_origin(),
                            super::PromptOrigin::TaskCompleted { .. }
                                | super::PromptOrigin::WorkflowCompleted { .. }
                        )
                    {
                        if let Some(fallback) = item.task_wake_fallback {
                            Self::push_task_wake_fallback(&mut state, fallback);
                        }
                        Self::respond_removed_prompt(item.respond_to);
                    } else {
                        kept.push_back(item);
                    }
                }
                state.pending_inputs = kept;
                cancelled
            };
            if had_message_fallbacks {
                self.broadcast_queue_changed(&state);
            }
            // Whether a user prompt remains queued behind the just-cancelled turn.
            // It distinguishes the next turn's redirect kind for telemetry `queued_after_cancel` means a queued prompt is promoted; `cancel_then_send` means the user types a fresh prompt.
            // Synthetic inputs (auto-wake / nudges) are not user redirects.
            let had_queued_user_prompt = state
                .pending_inputs
                .iter()
                .any(|i| !i.input_origin.is_synthetic());
            // `current_prompt_id` is deliberately NOT cleared here: cancel usage attribution must snapshot the ledger against the live pin first
            // It is cleared below, right after the `finalize_usage_from_outcome` / `snapshot_prompt_usage` call
            (
                cancelled_prompt_id,
                pending_inputs,
                rewound_input,
                had_queued_user_prompt,
                turn_epoch,
                message_completions,
            )
        };
        // True iff this cancel aborted a live task: the Keep-with-task rail and
        // every rewind rail that tore one down. A Keep lease that bound no task
        // sheds all task authority, so this stays false there.
        let tore_down_task = cancelled_prompt_id.is_some();
        if tore_down_task {
            self.cancel_active_sampling_requests();
            if rewound_input.is_none() {
                // The aborted turn can strand a continue reminder awaiting its continuation, plus dangling tool calls
                // Repair now so the on-disk tail is clean even if the session ends here (the next push would otherwise repair lazily)
                // Rewinds skip this: they replace the turn's history wholesale
                self.chat_state_handle
                    .repair_dangling_after_harness_halt("user_cancel");
            }
        }

        if rewound_input.is_none()
            && let Some(prompt_id) = cancelled_prompt_id.as_deref()
            && let Some(epoch) = turn_epoch
            && let Some(reason) = cancel_reason
        {
            self.report_cancelled_turn(CancelledTurn {
                prompt_id,
                epoch,
                reason,
                trigger: trigger.as_ref().map(|t| t.as_str().to_string()),
                last_assistant_message,
            });
        }

        if tore_down_task {
            self.events.cancel_active_tool();
        }
        // No prompt id means no turn ran, and closing a fresh session would otherwise write a `TurnEnded` with no `turn_started`
        if rewound_input.is_none() && cancelled_prompt_id.is_some() {
            // The trigger (esc / ctrl_c / …) goes into the events.jsonl `cancellation_context`
            // The category stays `MidTurnAbort` so the existing dashboards/dataset keep working
            let cancellation_context = trigger
                .as_ref()
                .map(|t| serde_json::json!({ "trigger": t.as_str() }));
            self.emit_turn_ended(
                crate::session::events::TurnOutcomeLabel::Cancelled,
                Some(crate::session::events::CancellationCategory::MidTurnAbort),
                cancellation_context,
            );
            // Mark the next real user prompt as following a mid-turn abort so replay/analytics/the model can see the user stopped this turn
            // Send-now is a silent cancel-and-send: the user is continuing, not aborting
            // It must not set the interrupt category or the interrupt envelope for its own continuation turn (mirrors the cancel-rate skip above)
            if !send_now {
                self.events.set_prior_interrupt_category(
                    crate::session::events::CancellationCategory::MidTurnAbort,
                );
            }
            // Set the interjection frame only if this abort cut assistant text and left no other signal
            // A dangling tool already emits a cancelled result
            if !send_now
                && !self.chat_state_handle.has_dangling_tool_calls().await
                && self
                    .chat_state_handle
                    .get_last_assistant_text_in_turn()
                    .await
                    .is_some()
            {
                self.events.set_pending_interrupt_reminder();
            }
            // Shared `redirect_kind` for the data pipeline: the next user turn's `turn_started` records HOW the user redirected after this abort
            self.events
                .set_prior_redirect_kind(if had_queued_user_prompt {
                    crate::session::events::RedirectKind::QueuedAfterCancel
                } else {
                    crate::session::events::RedirectKind::CancelThenSend
                });
        }

        // After the abort, so the turn no longer owns the commands it started.
        if send_now {
            self.background_foreground_commands_for_send_now().await;
        }

        if tore_down_task {
            if let Some(is_turn_active) = &self.tool_context.is_turn_active {
                is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            // The aborted turn's `BlockingWaitGuard`s drop asynchronously (they live in tool futures owned by the drainer task / subagent spawn task).
            // Until they do, `queue_input` would read a stale depth > 0 and auto-send-now the next prompt against a turn already gone.
            self.tool_context.blocking_wait_depth.reset();
            self.flush_pending_skill_reminders().await;
        }

        // No multi-second drain here (the actor loop would block RecordSubagentUsage)
        // Uses the same `UsageDrainOutcome` policy as freeze, via `finalize_usage_from_outcome`
        let cancelled_usage = if rewound_input.is_none()
            && let Some(ref prompt_id) = cancelled_prompt_id
        {
            let reply = self.outstanding_reply_for_prompt(prompt_id).await;
            let outcome = super::turn::UsageDrainOutcome::from_outstanding_reply(reply.as_ref());
            self.finalize_usage_from_outcome(prompt_id, outcome).await
        } else {
            None
        };
        if tore_down_task {
            self.cancel_active_sampling_requests();
            *self.doom_loop_turn_tally.lock() = Default::default();
        }
        let turn_stopped = cancelled_prompt_id.is_some() && cancel_reason.is_some();
        // Announced for every cancel that stopped a turn, including a rewind and a cancel-and-send
        // A no-op if the turn task announced first
        if cancelled_prompt_id.is_some()
            && let Some(epoch) = turn_epoch
        {
            self.notify_turn_abort(epoch, distill_agent_lifecycle::TurnAbortReason::Interrupted)
                .await;
        }
        // Rewind rails hold no lease, so clear the aborted turn's exact
        // resources here (the Keep rail does it in `finish_finalization_lease`).
        if matches!(finalization, CancelFinalization::Rewind)
            && let Some(prompt_id) = cancelled_prompt_id.as_deref()
        {
            self.clear_exact_turn_resources(prompt_id).await;
        }
        // A no-task cancel of a pinned front (turn started / pin set, task not yet or no longer in the slot) still needs a durable terminal so resume can tell unknown duration from a 0ms turn.
        // A queued front that never started has no pin and must not emit.
        let no_task_pinned_prompt_id = match &finalization {
            CancelFinalization::Keep(lease) => match &lease.binding {
                FinalizationBinding::NoTask(Some(id)) if pinned_prompt_id.as_ref() == Some(id) => {
                    Some(id.clone())
                }
                _ => None,
            },
            CancelFinalization::Rewind => None,
        };
        if rewound_input.is_none()
            && let Some(prompt_id) = cancelled_prompt_id.or(no_task_pinned_prompt_id)
        {
            // Only a cancel that ends a turn counts, and before the terminal's snapshot so it lands
            // on this turn's row. An idle Esc has no turn; a rewind replaces the turn, not stops it.
            if user_initiated && !rewind_requested {
                self.signals_handle().record_cancellation();
            }
            // `cancelTrigger` lets clients tell a send-now cancel from a Ctrl+C/Esc one
            // `MidTurnAbort` matches what the prompt's RPC resolves with below, so the event and the RPC agree
            self.emit_turn_completed(
                prompt_id,
                &Ok(acp::StopReason::Cancelled),
                cancelled_usage.clone(),
                trigger.as_ref().map(crate::session::CancelTrigger::as_str),
                // A no-task pinned cancel reaches here too; only a torn-down task is a mid-turn abort.
                tore_down_task.then(|| {
                    crate::session::commands::meta_category_str(
                        crate::session::events::CancellationCategory::MidTurnAbort,
                    )
                }),
                None,
                cancel_elapsed_ms,
                None,
            )
            .await;
        }

        if let Some(input) = rewound_input {
            let _strip_guard = self.prepare_image_strips_for_rewind().await;
            self.cancel_active_sampling_requests();
            self.cancel_pending_image_strips_for_rewind();
            let result = Ok(PromptTurnOk {
                stop_reason: acp::StopReason::Cancelled,
                total_tokens,
                turn_snapshot: None,
                completion_kind: PromptCompletionKind::Rewound,
                structured_output: None,
                usage: None,
                tool_overrides: self.effective_tool_overrides(),
            });
            Self::settle_parent_message_completions(message_completions, &result);
            return self
                .finish_rewound_cancel(input, None, total_tokens, turn_stopped)
                .await;
        }

        let message_result = Ok(PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens,
            turn_snapshot: None,
            completion_kind: PromptCompletionKind::Cancelled {
                category: Some(crate::session::events::CancellationCategory::MidTurnAbort),
                context: None,
            },
            structured_output: None,
            usage: cancelled_usage.clone(),
            tool_overrides: self.effective_tool_overrides(),
        });
        Self::settle_parent_message_completions(message_completions, &message_result);
        for (idx, input) in pending_inputs.into_iter().enumerate() {
            // idx 0 gets running-turn attribution only when this cancel tore
            // down a live task; a no-task cancel attests nothing about it.
            let is_running_turn = tore_down_task && idx == 0;
            if let Some(task_id) = input.input_origin.completion_id()
                && let Some(reservations) = &self.tool_context.task_completion_reservations
            {
                reservations.release(task_id);
            }
            let _ = input
                .respond_to
                .send(Ok(PromptTurnOk {
                    stop_reason: acp::StopReason::Cancelled,
                    total_tokens,
                    turn_snapshot: None,
                    completion_kind: PromptCompletionKind::Cancelled {
                        // Running turn only, matching events.jsonl's category; queued prompts were removed, not mid-turn aborted
                        category: is_running_turn
                            .then_some(crate::session::events::CancellationCategory::MidTurnAbort),
                        // Attach the trigger to the running turn only (idx 0); MvpAgent stamps it on the `PromptResponse` `_meta`
                        context: if is_running_turn {
                            trigger.as_ref().map(|t| {
                                crate::session::commands::CancellationContext {
                                    trigger: Some(t.as_str().to_string()),
                                    ..Default::default()
                                }
                            })
                        } else {
                            None
                        },
                    },
                    structured_output: None,
                    usage: if is_running_turn {
                        cancelled_usage.clone()
                    } else {
                        None
                    },
                    // Only the running turn (idx 0) ran, so only it reports the tool overrides it ran under
                    // A queued prompt that never promoted has nothing to report (like respond_removed_prompt)
                    tool_overrides: if is_running_turn {
                        self.effective_tool_overrides()
                    } else {
                        None
                    },
                }))
                .ok();
        }
        let settled = match &mut finalization {
            CancelFinalization::Keep(lease) => self.finish_finalization_lease(lease).await,
            CancelFinalization::Rewind => false,
        };
        CancelOutcome {
            barrier: if suppress_task_wakes {
                WakeBarrier::Armed
            } else {
                WakeBarrier::Clear
            },
            turn_stopped,
            settled,
        }
    }

    /// The user sent a message mid-turn: move any running command to the background instead of killing it.
    /// Each one's tool call gets an honest answer saying the command is still running.
    async fn background_foreground_commands_for_send_now(&self) {
        // This session and its subagents share one terminal, so move only this session's own commands.
        // Replying to another session's command would put an answer in this session's history for a question it never asked.
        let owner = Some(self.session_info.id.0.as_ref());
        let backgrounded = {
            self.agent
                .borrow()
                .tool_bridge()
                .background_foreground_commands(owner)
                .await
        };

        // Empty can mean nothing was running, or that this backend cannot background at all. Kill, so the command does not outlive its turn.
        // A kill does not report which commands it stopped, so `repair_dangling_tool_calls` answers the tool call before the next request.
        if backgrounded.is_empty() {
            self.kill_foreground_commands_for_cancel().await;
        }

        for bg in backgrounded {
            // No command text: the model already has it in the tool call this answers.
            let message = format!(
                "Command was moved to the background because the user sent a new message. \
                 The process is still running, not cancelled. Retrieve its output later \
                 with {} (task_id: {}).",
                self.tool_context.task_output_tool_name, bg.tool_call_id
            );
            self.chat_state_handle
                .push_tool_result(ConversationItem::tool_result(bg.tool_call_id, message));
        }
    }

    /// Kill the foreground commands this cancel owns. A subagent kills only its own, never the parent's or a sibling's on the shared backend.
    #[tracing::instrument(name = "cancel.kill_foreground", skip_all)]
    async fn kill_foreground_commands_for_cancel(&self) {
        if self.startup_hints.is_subagent {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands_by_owner(&self.session_info.id.0)
                .await;
        } else {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands()
                .await;
        }
    }
}
