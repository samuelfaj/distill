// Modified for Distill by Samuel Fajreldines, 2026.
//! Resident-root resolution and bounded admission lifecycle.

use crate::implementations::distill::task::active_message::{
    ActiveMessageAdmissionLease, ActiveMessageIngress,
};
use crate::implementations::distill::task::coordinator::SubagentCoordinator;
use crate::implementations::distill::task::coordinator::active_message::{
    ActiveMessageCompletionTarget, ActiveMessageFuture, ActiveMessageLifecycle,
};
use crate::implementations::distill::task::coordinator::wake::ResolvedSend;
use crate::implementations::distill::task::coordinator_state::{
    ACTIVE_MESSAGE_ADMISSION_TIMEOUT, ChildRunner, MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
};
use crate::implementations::distill::task::root_control::{
    AgentMessageGeneration, RootControl, RootReceiptSink,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessage, ActiveAgentMessageDelivery, ActiveAgentMessageOutcome,
    ActiveAgentMessageSource, ActiveMessageRoute, ActiveMessageSenderContext,
};
use distill_message_delivery_core::{AgentId, AttemptId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RootTargetKey {
    pub(super) agent_id: AgentId,
    pub(super) attempt_id: AttemptId,
    pub(super) generation: AgentMessageGeneration,
}

impl RootTargetKey {
    pub(super) fn from_control(control: &impl RootControl) -> Self {
        RootTargetKey {
            agent_id: control.agent_id().clone(),
            attempt_id: control.attempt_id().clone(),
            generation: control.generation(),
        }
    }
}

pub(super) struct RootTargetLifecycle<C> {
    control: C,
    active_messages: ActiveMessageLifecycle,
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn resolve_root_control(&mut self, control: R::RootControl) -> ResolvedSend {
        let key = RootTargetKey::from_control(&control);
        match self.root_active_messages.get(&key) {
            Some(_) => ResolvedSend::Root { key },
            None => {
                self.root_active_messages.insert(
                    key.clone(),
                    RootTargetLifecycle {
                        control,
                        active_messages: ActiveMessageLifecycle::default(),
                    },
                );
                ResolvedSend::Root { key }
            }
        }
    }

    pub(super) fn admit_root_message(
        &mut self,
        key: RootTargetKey,
        route: ActiveMessageRoute,
        ingress: ActiveMessageIngress,
    ) {
        let ActiveMessageIngress { request, permit } = ingress;
        let Some(root) = self.root_active_messages.get_mut(&key) else {
            let _ = request
                .respond_to
                .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
            return;
        };
        if root.control.receipt_sink().is_closed() {
            let _ = request
                .respond_to
                .send(ActiveAgentMessageOutcome::ChannelClosed);
            if matches!(
                root.active_messages,
                ActiveMessageLifecycle::Open { in_flight: 0, .. }
            ) {
                self.root_active_messages.remove(&key);
            }
            return;
        }
        match root.active_messages.begin_admission() {
            super::active_message::BeginAdmission::Started => {}
            super::active_message::BeginAdmission::Finalizing => {
                let _ = request
                    .respond_to
                    .send(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
                return;
            }
            super::active_message::BeginAdmission::Saturated => {
                let _ = request
                    .respond_to
                    .send(ActiveAgentMessageOutcome::Saturated {
                        max_in_flight: MAX_ACTIVE_MESSAGE_ADMISSIONS_PER_CHILD,
                    });
                return;
            }
        }
        let source = match request.sender_context {
            ActiveMessageSenderContext::HumanRoot { .. } => ActiveAgentMessageSource::Human,
            ActiveMessageSenderContext::RootSession { .. }
            | ActiveMessageSenderContext::GrantedChild { .. } => ActiveAgentMessageSource::Agent,
        };
        let sender_session_id = match &request.sender_context {
            ActiveMessageSenderContext::RootSession { session_id }
            | ActiveMessageSenderContext::HumanRoot { session_id } => session_id.as_ref(),
            ActiveMessageSenderContext::GrantedChild { holder } => holder.session_id(),
        };
        let message_id = uuid::Uuid::now_v7().to_string();
        let lease = ActiveMessageAdmissionLease::new();
        let delivery = ActiveAgentMessageDelivery::new(
            ActiveAgentMessage {
                message_id: message_id.clone(),
                sender_session_id: sender_session_id.to_owned(),
                text: request.request.text().clone(),
            },
            request.request.operation(),
            source,
            route,
            std::sync::Arc::clone(&lease),
        );
        self.active_messages.push(ActiveMessageFuture {
            target: ActiveMessageCompletionTarget::Root(key),
            message_id,
            future: root.control.deliver(delivery),
            cancellation: Box::pin(tokio_util::sync::CancellationToken::new().cancelled_owned()),
            deadline: Box::pin(tokio::time::sleep(ACTIVE_MESSAGE_ADMISSION_TIMEOUT)),
            lease,
            ingress_permit: Some(permit),
            quota_admission: None,
            respond_to: Some(request.respond_to),
        });
    }

    pub(super) fn finish_root_admission(&mut self, key: &RootTargetKey, is_settled: bool) {
        let Some(root) = self.root_active_messages.get_mut(key) else {
            return;
        };
        let _ = root.active_messages.finish_admission(is_settled);
        let is_idle = matches!(
            root.active_messages,
            ActiveMessageLifecycle::Open { in_flight: 0, .. }
        );
        if is_idle {
            self.root_active_messages.remove(key);
        }
    }
}

#[cfg(test)]
#[path = "root_targets_tests.rs"]
pub(super) mod tests;
