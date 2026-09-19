// Modified for Distill by Samuel Fajreldines, 2026.
//! Admission and safe-point delivery of messages from an owning parent agent.

use super::parent_interject::ParentInterjectSignal;
use super::*;
use crate::session::telemetry::ActiveAgentMessageSafePointTrigger;
use distill_message_delivery_core::{
    DeliveryMessage, MessageDeliveryLifecycle, OwnedDelivery, TerminalCause, TerminalTarget,
    TurnBinding,
};
use distill_tools::implementations::distill::task::coordinator::ActiveMessageAdmission;
use distill_tools::implementations::distill::task::types::{
    ActiveAgentMessage, ActiveAgentMessageDelivery, ActiveAgentMessageOperation,
    ActiveAgentMessageSource,
};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct ParentMessageOrigin {
    sender_session_id: String,
    source: ActiveAgentMessageSource,
}

#[derive(Clone)]
pub(super) struct PendingParentAgentMessage {
    prompt_id: String,
    message_id: String,
    text: Arc<str>,
    /// Effective operation; a safe-point slot holds `Steer` or `Interject`, which sets drain order.
    operation: ActiveAgentMessageOperation,
    telemetry: crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry,
}

type ParentMessageCompletion = oneshot::Sender<crate::session::commands::PromptTurnResult>;
pub(super) type ParentDeliveryMessage =
    DeliveryMessage<String, ParentMessageOrigin, PendingParentAgentMessage>;
type ParentTurnBinding = TurnBinding<String, TurnEpoch>;
type ParentMessageLifecycle = MessageDeliveryLifecycle<
    String,
    ParentMessageOrigin,
    PendingParentAgentMessage,
    ParentMessageCompletion,
    String,
    TurnEpoch,
>;
pub(super) type ParentOwnedDelivery = OwnedDelivery<
    String,
    ParentMessageOrigin,
    PendingParentAgentMessage,
    ParentMessageCompletion,
    String,
    TurnEpoch,
>;

/// Named bound on live Steer and Interject slots waiting for the next safe point.
/// Parent text becomes model-visible in one batch; this keeps that batch finite.
const MAX_PARENT_SAFE_POINT_SLOTS: usize = 32;

/// A safe-point delivery commit could not reach a downstream actor;
/// the slots stay projecting for terminal settlement.
enum DrainCommitError {
    PersistenceUnavailable,
    ChatStateUnavailable,
}

/// Every slot mutation goes through this impl so the interject signal is recounted from the
/// lifecycle rather than kept in step by callers.
#[derive(Default)]
pub(crate) struct MessageDeliveryState {
    lifecycle: ParentMessageLifecycle,
    interject_signal: Arc<ParentInterjectSignal>,
}

impl MessageDeliveryState {
    pub(super) fn interject_signal(&self) -> Arc<ParentInterjectSignal> {
        Arc::clone(&self.interject_signal)
    }

    fn has_slot_for(&self, message: &ActiveAgentMessage) -> bool {
        self.lifecycle.contains_identity(&message.message_id)
    }

    fn is_at_slot_cap(&self) -> bool {
        self.lifecycle.len() >= MAX_PARENT_SAFE_POINT_SLOTS
    }

    fn sync_interject_signal(&self, running: Option<&ParentTurnBinding>) {
        let has_pending = running.is_some_and(|binding| {
            self.lifecycle
                .pending_messages(binding)
                .iter()
                .any(|message| message.content().is_interject())
        });
        self.interject_signal.set_pending(has_pending);
    }

    fn admit_pending(
        &mut self,
        binding: &ParentTurnBinding,
        message: ParentDeliveryMessage,
        completion: ParentMessageCompletion,
    ) -> Result<(), ParentDeliveryMessage> {
        let admitted = self
            .lifecycle
            .admit_pending(binding.clone(), message, completion);
        self.sync_interject_signal(Some(binding));
        admitted
    }

    /// Consumes the wait-abort mark in the same step, so an abort noted after this call belongs
    /// to a later drain and a failed barrier cannot leave it set; only a mark noted for `running`
    /// counts.
    fn begin_delivery(
        &mut self,
        running: &AgentTask,
    ) -> (
        Vec<ParentDeliveryMessage>,
        ActiveAgentMessageSafePointTrigger,
    ) {
        let binding = turn_binding(running);
        let messages = self.lifecycle.begin_delivery(&binding);
        self.sync_interject_signal(Some(&binding));
        let trigger = if self.interject_signal.take_wait_aborted(running.epoch) {
            ActiveAgentMessageSafePointTrigger::WaitAbort
        } else {
            ActiveAgentMessageSafePointTrigger::Natural
        };
        (messages, trigger)
    }

    fn finish_delivery<Error>(
        &mut self,
        binding: &ParentTurnBinding,
        commit: impl FnOnce(&[ParentDeliveryMessage]) -> Result<(), Error>,
    ) -> Result<Vec<ParentDeliveryMessage>, Error> {
        self.lifecycle.finish_delivery(binding, commit)
    }

    /// `running` is the turn still live after the transition, if any; a stale completion for an
    /// earlier turn must not silence its pending slots or drop its wait-abort mark.
    fn transition(
        &mut self,
        target: TerminalTarget<'_, String, TurnEpoch>,
        cause: TerminalCause,
        running: Option<&AgentTask>,
    ) -> distill_message_delivery_core::TerminalTransition<
        String,
        ParentMessageOrigin,
        PendingParentAgentMessage,
        ParentMessageCompletion,
        String,
        TurnEpoch,
    > {
        let ends_running_turn = match &target {
            TerminalTarget::All => true,
            TerminalTarget::Turn(binding) => {
                running.is_none_or(|task| **binding == turn_binding(task))
            }
        };
        let transition = self.lifecycle.transition(target, cause);
        // The ended turn's mark must not be attributed to the next turn's first drain.
        if ends_running_turn {
            self.interject_signal.clear_wait_aborted();
        }
        self.sync_interject_signal(running.map(turn_binding).as_ref());
        transition
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.lifecycle.is_empty()
    }
}

impl Drop for MessageDeliveryState {
    fn drop(&mut self) {
        let transition = self.transition(TerminalTarget::All, TerminalCause::ActorDrop, None);
        let result = Err(acp::Error::internal_error()
            .data("active-message owner dropped before terminal settlement"));
        transition
            .completions
            .into_iter()
            .chain(transition.fallbacks)
            .for_each(|owned| {
                let (_, _, completion) = owned.into_parts();
                let _ = completion.send(result.clone());
            });
    }
}

impl PendingParentAgentMessage {
    pub(super) fn is_interject(&self) -> bool {
        self.operation == ActiveAgentMessageOperation::Interject
    }

    fn into_input(
        self,
        origin: ParentMessageOrigin,
        respond_to: ParentMessageCompletion,
    ) -> InputItem {
        let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            self.text.to_string(),
        ))];
        let queue_meta = crate::session::prompt_queue::QueueEntryMeta {
            id: self.prompt_id.clone(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "parent_agent_message".to_owned(),
            text: SessionActor::queue_text_from_blocks(&prompt_blocks),
            combined_texts: None,
        };
        InputItem {
            prompt_id: self.prompt_id,
            prompt_blocks,
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: matches!(origin.source, ActiveAgentMessageSource::Human),
            json_schema: None,
            input_origin: InputOrigin::new(
                if matches!(origin.source, ActiveAgentMessageSource::Human) {
                    super::PromptOrigin::ParentHumanMessage {
                        message_id: self.message_id,
                        sender_session_id: origin.sender_session_id,
                    }
                } else {
                    super::PromptOrigin::ParentAgentMessage {
                        message_id: self.message_id,
                        sender_session_id: origin.sender_session_id,
                    }
                },
            ),
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: Some(queue_meta),
            queue_mutation_policy: QueueMutationPolicy::new(true, false),
            send_now: false,
            traceparent: None,
        }
    }
}

fn turn_binding(task: &AgentTask) -> TurnBinding<String, TurnEpoch> {
    TurnBinding::new(task.prompt_id.clone(), task.epoch)
}

fn contains_queued_identity(state: &State, identity: &str) -> bool {
    state.pending_inputs.iter().any(|item| {
        matches!(
            item.input_origin.as_prompt_origin(),
            PromptOrigin::ParentAgentMessage { message_id, .. }
            | PromptOrigin::ParentHumanMessage { message_id, .. }
                if message_id == identity
        )
    })
}

impl SessionActor {
    pub(super) async fn admit_parent_agent_message(
        self: &Arc<Self>,
        delivery: ActiveAgentMessageDelivery,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: distill_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        let message = delivery.message().clone();
        let requested = delivery.operation();
        let source = delivery.source();
        self.admit_parent_agent_message_inner(
            Some(delivery),
            source,
            message,
            requested,
            receipt_sink,
            parent_telemetry_ctx,
            respond_to,
            completion_tx,
        )
        .await;
    }

    async fn admit_parent_agent_message_inner(
        self: &Arc<Self>,
        delivery: Option<ActiveAgentMessageDelivery>,
        message_source: ActiveAgentMessageSource,
        message: ActiveAgentMessage,
        requested: ActiveAgentMessageOperation,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: distill_telemetry::TelemetryCtx,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        let receipt_permit = match receipt_sink.reserve_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                let _ = respond_to.send(ActiveMessageAdmission::ChannelClosed);
                return;
            }
        };
        self.ensure_prefix_ready().await;

        let prompt_id = format!("parent-message-{}", message.message_id);
        let (turn_result_tx, turn_result_rx) = oneshot::channel();
        let admitted_at = std::time::Instant::now();
        let mut state = self.state.lock().await;
        if state.message_delivery.has_slot_for(&message)
            || contains_queued_identity(&state, &message.message_id)
        {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        let effective = match (requested, state.running_task.as_ref()) {
            (ActiveAgentMessageOperation::Steer, Some(_)) => ActiveAgentMessageOperation::Steer,
            (ActiveAgentMessageOperation::Interject, Some(_)) => {
                ActiveAgentMessageOperation::Interject
            }
            (
                ActiveAgentMessageOperation::Queue
                | ActiveAgentMessageOperation::Steer
                | ActiveAgentMessageOperation::Interject,
                None,
            )
            | (ActiveAgentMessageOperation::Queue, Some(_)) => ActiveAgentMessageOperation::Queue,
        };
        if effective != ActiveAgentMessageOperation::Queue
            && state.message_delivery.is_at_slot_cap()
        {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        let telemetry = crate::session::telemetry::ActiveAgentMessageAdmissionTelemetry::new(
            admitted_at,
            parent_telemetry_ctx,
            requested,
            effective,
            (requested != ActiveAgentMessageOperation::Queue
                && effective == ActiveAgentMessageOperation::Queue)
                .then_some(crate::session::telemetry::ActiveAgentMessageFallbackReason::Idle),
        );
        let content = PendingParentAgentMessage {
            prompt_id: prompt_id.clone(),
            message_id: message.message_id.clone(),
            text: message.text,
            operation: effective,
            telemetry: telemetry.clone(),
        };
        let origin = ParentMessageOrigin {
            sender_session_id: message.sender_session_id,
            source: message_source,
        };
        let commit = || match effective {
            ActiveAgentMessageOperation::Queue => {
                let item = content.into_input(origin, turn_result_tx);
                self.commit_queued_delivery(
                    &mut state,
                    super::prompt_queue::PreparedDelivery(item),
                );
            }
            ActiveAgentMessageOperation::Steer | ActiveAgentMessageOperation::Interject => {
                let binding = turn_binding(
                    state
                        .running_task
                        .as_ref()
                        .unwrap_or_else(|| unreachable!("slot effective only while running")),
                );
                state
                    .message_delivery
                    .admit_pending(
                        &binding,
                        DeliveryMessage::new(message.message_id, origin, content),
                        turn_result_tx,
                    )
                    .unwrap_or_else(|_| unreachable!("live identity checked under state lock"));
            }
        };
        let committed = if let Some(delivery) = delivery {
            delivery.commit_admission(commit).is_some()
        } else {
            commit();
            true
        };
        if !committed {
            let _ = respond_to.send(ActiveMessageAdmission::Rejected);
            return;
        }
        drop(state);

        receipt_permit.send(crate::agent::subagent::PromptTurnReceipt {
            prompt_id,
            result: turn_result_rx,
            telemetry,
        });
        let _ = respond_to.send(ActiveMessageAdmission::Admitted);
        Self::maybe_start_running_task(self.clone(), completion_tx).await;
    }

    pub(super) async fn drain_parent_messages_at_safe_point(&self) -> bool {
        let model_id = self.current_model_id().await;
        let user_chunk_meta = serde_json::json!({ "modelId": model_id })
            .as_object()
            .cloned();
        let notification_meta = self.build_notification_meta();
        let (binding, trigger) = {
            let mut state = self.state.lock().await;
            // Reborrow once so `running_task` and `message_delivery` can be borrowed disjointly.
            let state = &mut *state;
            let Some(task) = state.running_task.as_ref() else {
                return false;
            };
            let (messages, trigger) = state.message_delivery.begin_delivery(task);
            if messages.is_empty() {
                return false;
            }
            (turn_binding(task), trigger)
        };
        // The barrier precedes every visible side effect: a dead or cancelled
        // persistence actor skips delivery, and a teardown settlement landing
        // while this await is suspended leaves nothing to roll back.
        let (persisted_tx, persisted_rx) = oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck {
                respond_to: persisted_tx,
            })
            .is_err()
            || !matches!(persisted_rx.await, Ok(Ok(())))
        {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "parent-message drain skipped: persistence barrier failed"
            );
            return false;
        }
        // Persist and push only inside the commit under the state lock: teardown settlement transitions slots under the same lock, so a slot settled during the barrier yields no projecting messages here and its text never.
        let mut state = self.state.lock().await;
        let committed = state
            .message_delivery
            .finish_delivery(&binding, |messages| {
                let ordered: Vec<&_> =
                    super::parent_interject::order_for_delivery(messages).collect();
                for message in &ordered {
                    let update = acp::SessionUpdate::UserMessageChunk(
                        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                            message.content().text.to_string(),
                        )))
                        .meta(user_chunk_meta.clone()),
                    );
                    self.notifications
                        .persistence_tx
                        .send(PersistenceMsg::Update(SessionUpdate::Acp(Box::new(
                            acp::SessionNotification::new(self.session_info.id.clone(), update)
                                .meta(notification_meta.clone().as_object().cloned()),
                        ))))
                        .map_err(|_| DrainCommitError::PersistenceUnavailable)?;
                }
                self.chat_state_handle
                    .try_push_user_messages_batch(
                        ordered
                            .iter()
                            .map(|message| {
                                ConversationItem::agent_message(message.content().text.to_string())
                            })
                            .collect(),
                    )
                    .map_err(|_| DrainCommitError::ChatStateUnavailable)?;
                let delivered_at = std::time::Instant::now();
                for message in messages {
                    message
                        .content()
                        .telemetry
                        .record_safe_point_delivery(delivered_at, trigger);
                }
                Ok(())
            });
        match committed {
            Ok(delivered) => !delivered.is_empty(),
            Err(DrainCommitError::PersistenceUnavailable) => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    "parent-message drain skipped: persistence actor unavailable"
                );
                false
            }
            Err(DrainCommitError::ChatStateUnavailable) => {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    "parent-message drain skipped: chat-state actor unavailable"
                );
                false
            }
        }
    }

    pub(super) fn transition_parent_messages(
        &self,
        state: &mut State,
        target: TerminalTarget<'_, String, TurnEpoch>,
        cause: TerminalCause,
    ) -> (Vec<ParentOwnedDelivery>, bool) {
        let running = state.running_task.as_ref();
        let transition = state.message_delivery.transition(target, cause, running);
        let has_fallbacks = !transition.fallbacks.is_empty();
        for owned in transition.fallbacks {
            let fallback_reason = match cause {
                TerminalCause::Completion => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::Completion
                }
                TerminalCause::SoftCancel => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::SoftCancel
                }
                TerminalCause::Rewind => {
                    crate::session::telemetry::ActiveAgentMessageFallbackReason::Rewind
                }
                TerminalCause::HardTeardown | TerminalCause::ActorDrop => {
                    unreachable!("terminal cause cannot produce a fallback")
                }
            };
            let (_, message, completion) = owned.into_parts();
            let (_, origin, content) = message.into_parts();
            content.telemetry.record_fallback(fallback_reason);
            state
                .pending_inputs
                .push_back(content.into_input(origin, completion));
        }
        (transition.completions, has_fallbacks)
    }

    pub(super) async fn settle_all_parent_messages(&self, cause: TerminalCause) {
        debug_assert!(matches!(
            cause,
            TerminalCause::HardTeardown | TerminalCause::ActorDrop
        ));
        let completions = {
            let mut state = self.state.lock().await;
            let (completions, _) =
                self.transition_parent_messages(&mut state, TerminalTarget::All, cause);
            completions
        };
        let result = Err(acp::Error::internal_error()
            .data("active-message owner ended before delivery completed"));
        Self::settle_parent_message_completions(completions, &result);
    }

    pub(super) fn settle_parent_message_completions(
        completions: Vec<ParentOwnedDelivery>,
        result: &crate::session::commands::PromptTurnResult,
    ) {
        completions.into_iter().for_each(|owned| {
            let (_, _, completion) = owned.into_parts();
            let _ = completion.send(result.clone());
        });
    }

    #[cfg(test)]
    async fn admit_parent_agent_message_for_test(
        self: &Arc<Self>,
        message: ActiveAgentMessage,
        source: ActiveAgentMessageSource,
        operation: ActiveAgentMessageOperation,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        respond_to: oneshot::Sender<ActiveMessageAdmission>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) {
        self.admit_parent_agent_message_inner(
            None,
            source,
            message,
            operation,
            receipt_sink,
            distill_telemetry::TelemetryCtx::new(
                "test-parent".to_owned(),
                std::sync::Arc::new(tokio::sync::Mutex::new(0)),
            ),
            respond_to,
            completion_tx,
        )
        .await;
    }
}

#[cfg(test)]
#[path = "parent_message_tests.rs"]
pub(super) mod tests;

#[cfg(test)]
#[path = "parent_message_interject_tests.rs"]
mod interject_tests;
