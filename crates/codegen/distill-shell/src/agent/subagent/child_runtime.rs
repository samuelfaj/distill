// Modified for Distill by Samuel Fajreldines, 2026.
use distill_message_delivery_core::DeliveryEnvelope;
use distill_tools::implementations::distill::task::coordinator::{
    ActiveMessageAdmission, ChildControl, LocalBoxFuture, SendBoxFuture, SubagentProgress,
};
use distill_tools::implementations::distill::task::types::ActiveAgentMessageDelivery;
use tokio::sync::mpsc;

use super::prompt_turn_receipt::{PromptTurnReceipt, cancel_shell_child_turn};
use crate::session::{SessionCommand, SessionThread};

/// Shell runtime handle retained while a child is active.
pub(crate) struct ShellChildRuntime {
    pub(crate) child_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    pub(crate) message_delivery: crate::session::message_delivery::MessageDeliveryHandle,
    pub(crate) active_message_target_session_id: String,
    pub(crate) active_message_target_agent_id: distill_message_delivery_core::AgentId,
    pub(crate) active_message_target_generation:
        distill_tools::implementations::distill::task::root_control::AgentMessageGeneration,
    pub(crate) child_signals: crate::session::signals::SessionSignalsHandle,
    /// Held by the worker until promotion succeeds.
    /// `None` means the caller still owns the join handle, so a cancel during promotion can wait for the actor to exit.
    pub(crate) _child_thread: Option<SessionThread>,
    pub(crate) receipt_sink: mpsc::Sender<PromptTurnReceipt>,
    #[cfg(test)]
    pub(crate) force_queue_envelope: bool,
    pub(crate) active_message_parent_session_id: String,
    pub(crate) active_message_parent_prompt_index: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ChildControl for ShellChildRuntime {
    type ProgressFuture = LocalBoxFuture<SubagentProgress>;

    fn progress(&self) -> Self::ProgressFuture {
        let signals = self.child_signals.clone();
        Box::pin(async move {
            signals
                .snapshot()
                .await
                .map(|snapshot| SubagentProgress {
                    turn_count: snapshot.turn_count,
                    tool_call_count: snapshot.tool_call_count,
                    tokens_used: snapshot.context_tokens_used,
                    context_window_tokens: snapshot
                        .active_context_window_tokens
                        .unwrap_or(snapshot.context_window_tokens),
                    context_usage_pct: snapshot
                        .active_context_window_tokens
                        .map(|window| {
                            distill_token_estimation::usage_percentage_u8(
                                snapshot.context_tokens_used,
                                window,
                            )
                        })
                        .unwrap_or(snapshot.context_window_usage),
                    tools_used: snapshot.tools_used,
                    error_count: snapshot.error_count,
                })
                .unwrap_or_default()
        })
    }

    fn send_active_message(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let message_delivery = self.message_delivery.clone();
        let receipt_sink = self.receipt_sink.clone();
        let message = delivery.message();
        let envelope = DeliveryEnvelope::from_agent(
            {
                #[cfg(test)]
                if self.force_queue_envelope {
                    distill_message_delivery_core::Operation::Queue
                } else {
                    distill_message_delivery_core::Operation::from(delivery.operation())
                }
                #[cfg(not(test))]
                distill_message_delivery_core::Operation::from(delivery.operation())
            },
            message.text.clone(),
            crate::session::message_delivery::agent_delivery_identity(message.message_id.clone()),
            crate::session::message_delivery::CoordinatorAgentDeliveryGrant::new(
                self.active_message_target_session_id.clone(),
                self.active_message_target_agent_id.clone(),
                self.active_message_target_generation,
                delivery,
            ),
        );
        let parent_prompt_index = self
            .active_message_parent_prompt_index
            .load(std::sync::atomic::Ordering::Acquire);
        let parent_telemetry_ctx = distill_telemetry::TelemetryCtx::new(
            self.active_message_parent_session_id.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(parent_prompt_index)),
        );
        Box::pin(async move {
            message_delivery
                .send(envelope, receipt_sink, parent_telemetry_ctx)
                .await
        })
    }

    fn cancel(&self) {
        cancel_shell_child_turn(&self.child_cmd_tx);
    }
}

const SESSION_THREAD_EXIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// How long to wait for an unpromoted child actor to exit after cancel and shutdown.
pub(crate) const UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Polls `SessionThread::is_finished` until the actor thread exits or `timeout` elapses.
/// A `timeout` of `Duration::ZERO` checks once and never sleeps.
pub(crate) async fn await_session_thread_exit(
    thread: &SessionThread,
    timeout: std::time::Duration,
) -> bool {
    if thread.is_finished() {
        return true;
    }
    if timeout.is_zero() {
        return false;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return thread.is_finished();
        }
        tokio::time::sleep(remaining.min(SESSION_THREAD_EXIT_POLL)).await;
        if thread.is_finished() {
            return true;
        }
    }
}

/// Whether an unpromoted child may drop its worktree and workspace binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnpromotedResourceFate {
    Release,
    Preserve,
}

impl UnpromotedResourceFate {
    pub(crate) fn from_thread_exit(exited: bool) -> Self {
        if exited {
            Self::Release
        } else {
            Self::Preserve
        }
    }

    pub(crate) fn should_release(self) -> bool {
        matches!(self, Self::Release)
    }
}

#[cfg(test)]
#[path = "child_runtime_tests.rs"]
mod tests;
