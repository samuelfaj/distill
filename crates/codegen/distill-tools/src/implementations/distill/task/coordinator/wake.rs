// Modified for Distill by Samuel Fajreldines, 2026.
//! Completed-child wake admission and displacement.

use super::super::admission::{AdmissionDecision, AdmissionError};
use super::active_message::ParkedSpawnReadyMessage;
use super::agent_quotas::{ActiveMessageQuotaAdmission, TargetIncarnationKey};
use super::queue::{QueuedCaller, QueuedSpawn, StartOrigin};
use super::{SubagentCoordinator, SubagentLimitDecision};
use crate::implementations::distill::task::active_message::ActiveMessageIngress;
use crate::implementations::distill::task::coordinator_state::{
    ChildRunner, DisplacedCompletedChild, MAX_COMPLETED_ENTRIES, WakeOrigin,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOutcome, ActiveAgentMessageSource, ActiveMessageRoute,
    ActiveMessageSenderContext, SubagentActiveMessageRequest,
};

pub(super) struct PendingWake {
    pub(super) route: ActiveMessageRoute,
    pub(super) incarnation: TargetIncarnationKey,
    pub(super) quota_admission: Option<ActiveMessageQuotaAdmission>,
    pub(super) ingress: ActiveMessageIngress,
}

pub(super) enum ResolvedSend {
    Admit {
        subagent_id: String,
        target: TargetIncarnationKey,
    },
    Park {
        subagent_id: String,
        target: TargetIncarnationKey,
    },
    Wake {
        subagent_id: String,
        target: TargetIncarnationKey,
        authorization: WakeAuthorization,
    },
    Root {
        key: super::root_targets::RootTargetKey,
    },
    Fail(ActiveAgentMessageOutcome),
}

/// Who vouched for a completed-child wake when its target resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WakeAuthorization {
    /// Typed `Agent` target: the resolver authorized the sender (granted child or resident root).
    ResolvedTarget,
    /// Legacy `ChildId`/`Address` target: wake admission re-checks the sender's lineage.
    SenderLineage,
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn wake_completed_child(
        &mut self,
        subagent_id: String,
        route: ActiveMessageRoute,
        target: TargetIncarnationKey,
        authorization: WakeAuthorization,
        quota_admission: Option<ActiveMessageQuotaAdmission>,
        ingress: ActiveMessageIngress,
    ) {
        if let Err(outcome) = self.verify_message_sender(&ingress.request.sender_context) {
            let _ = ingress.request.respond_to.send(outcome);
            return;
        }
        if self.completed.get(&subagent_id).is_some_and(|completed| {
            !completed
                .terminal_published
                .load(std::sync::atomic::Ordering::Acquire)
        }) {
            self.pending_wakes
                .entry(subagent_id)
                .or_default()
                .push(PendingWake {
                    route,
                    incarnation: target,
                    quota_admission,
                    ingress,
                });
            return;
        }
        if !self.runner.supports_wake() {
            let _ = ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            return;
        }
        if self.completed.get(&subagent_id).is_some_and(|child| {
            self.spawn_blocked_sessions
                .contains(&child.request.parent_session_id)
        }) {
            let _ = ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            return;
        }
        let can_wake = self.completed.get(&subagent_id).is_some_and(|child| {
            !child.request.owner.is_workflow()
                && (authorization == WakeAuthorization::ResolvedTarget
                    || matches!(
                        &ingress.request.sender_context,
                        ActiveMessageSenderContext::GrantedChild { .. }
                    )
                    || self.graph.is_reachable_from(
                        &subagent_id,
                        Self::message_sender_session(&ingress.request.sender_context),
                    ))
        });
        if !can_wake {
            let _ = ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
            return;
        }

        let Some(completed) = self.completed.get(&subagent_id) else {
            let _ = ingress
                .request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
            return;
        };
        let mut wake_request = completed.request.clone();
        let agent_address = completed.agent_address.clone();
        let spawner_session_id = completed.spawner_session_id.clone();
        let ActiveMessageIngress {
            request:
                SubagentActiveMessageRequest {
                    request,
                    sender_context,
                    respond_to,
                },
            permit,
        } = ingress;
        let wake_message_source = Self::message_source(&sender_context);
        wake_request.prompt = request.text().to_string();
        wake_request.fork_context = false;
        wake_request.parent_prompt_id = None;
        wake_request.run_in_background = true;
        wake_request.surface_completion = false;
        wake_request.await_to_completion = false;
        wake_request.cancel_token = tokio_util::sync::CancellationToken::new();
        let message_id = match wake_message_source {
            ActiveAgentMessageSource::Agent => {
                format!("parent-agent-message-{}", uuid::Uuid::now_v7())
            }
            ActiveAgentMessageSource::Human => {
                format!("parent-message-{}", uuid::Uuid::now_v7())
            }
        };
        let wake_origin = WakeOrigin {
            agent_id: subagent_id.clone(),
            source: wake_message_source,
            message_id: message_id.clone(),
        };
        let parked = ParkedSpawnReadyMessage {
            subagent_id: subagent_id.clone(),
            sender_context,
            route,
            request,
            respond_to: Some(respond_to),
            deadline: None,
            initial_message_id: Some(message_id.clone()),
            quota_admission,
            permit: Some(permit),
        };

        let running = self.session_running_count(&wake_request.parent_session_id);
        match self.admission.admit(&wake_request, running) {
            AdmissionDecision::Start => {
                let completed =
                    self.completed
                        .remove(&subagent_id)
                        .map(|completed| DisplacedCompletedChild {
                            completed: Box::new(completed),
                        });
                self.completed_order.retain(|id| id != &subagent_id);
                self.spawn_ready.push(parked);
                self.start_child(
                    wake_request,
                    None,
                    None,
                    StartOrigin::Direct,
                    agent_address,
                    spawner_session_id,
                    Some(wake_origin),
                    completed,
                );
            }
            AdmissionDecision::Enqueue => {
                self.notify_limit(
                    &wake_request,
                    SubagentLimitDecision::QueuedAtConcurrentLimit {
                        limit: self.admission.max_concurrent(),
                    },
                );
                #[expect(
                    clippy::expect_used,
                    reason = "wake target was resolved from completed state and is removed exactly once"
                )]
                let completed = self.completed.remove(&subagent_id).expect(
                    "wake target was resolved from completed state and is removed exactly once",
                );
                self.completed_order.retain(|id| id != &subagent_id);
                self.spawn_ready.push(parked);
                self.queued.push_back(QueuedSpawn {
                    request: Box::new(wake_request),
                    queued_at: tokio::time::Instant::now(),
                    caller: QueuedCaller::Backgrounded,
                    agent_address,
                    spawner_session_id,
                    wake_origin: Some(wake_origin),
                    wake: Some(DisplacedCompletedChild {
                        completed: Box::new(completed),
                    }),
                });
            }
            AdmissionDecision::Reject(error) => {
                self.notify_limit(
                    &wake_request,
                    match error {
                        AdmissionError::ConcurrentLimitReached { limit } => {
                            SubagentLimitDecision::RejectedAtConcurrentLimit { limit }
                        }
                    },
                );
                parked.reply(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            }
        }
    }
    pub(super) fn restore_displaced_completion(&mut self, displaced: DisplacedCompletedChild) {
        let id = displaced.completed.request.id.clone();
        let age = displaced.completed.completion_age;
        let retained_age_floor = self
            .next_completion_age
            .saturating_sub(MAX_COMPLETED_ENTRIES as u64);
        if age < retained_age_floor {
            self.graph.remove(&id);
            self.clear_target_incarnation(&id);
            for waiter in self.waiters.remove(&id).unwrap_or_default() {
                let _ = waiter.respond_to.send(None);
            }
            return;
        }
        self.restore_completed_incarnation(&id);
        let snapshot = self.completed_snapshot_for_query(&displaced.completed);
        for waiter in self.waiters.remove(&id).unwrap_or_default() {
            let _ = waiter.respond_to.send(Some(snapshot.clone()));
        }
        let index = self
            .completed_order
            .iter()
            .position(|entry| {
                self.completed
                    .get(entry)
                    .is_some_and(|completed| completed.completion_age > age)
            })
            .unwrap_or(self.completed_order.len());
        self.completed.insert(id.clone(), *displaced.completed);
        self.completed_order.insert(index, id);
    }
}
