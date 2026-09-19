// Modified for Distill by Samuel Fajreldines, 2026.
//! Narrow message capability minted for one child activation.

use std::sync::Arc;

use tokio::sync::mpsc;
use distill_message_delivery_core::{AgentId, AttemptId};

use crate::implementations::distill::task::active_message::{
    ActiveAgentMessageQuotaKind, ActiveMessageIngress,
};
use crate::implementations::distill::task::coordinator::ActiveChildGeneration;
use crate::register_resource;

#[derive(Clone, PartialEq, Eq)]
pub struct AgentMessageHolder {
    agent_id: AgentId,
    session_id: Arc<str>,
    attempt_id: AttemptId,
    generation: ActiveChildGeneration,
}

impl AgentMessageHolder {
    pub(super) fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(super) fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub(super) fn generation(&self) -> ActiveChildGeneration {
        self.generation
    }
}

impl std::fmt::Debug for AgentMessageHolder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentMessageHolder")
            .field("agent_id", &self.agent_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct AgentMessageSender {
    active_message_tx: mpsc::UnboundedSender<ActiveMessageIngress>,
    active_message_permits: Arc<tokio::sync::Semaphore>,
    active_message_capacity: usize,
    holder: AgentMessageHolder,
    local_outbound_budget: Arc<std::sync::atomic::AtomicUsize>,
}

impl AgentMessageSender {
    pub(crate) fn mint_for_child(
        factory: Option<&AgentMessageSenderFactory>,
        child_id: &str,
        enabled: bool,
    ) -> MintedChildIdentity {
        let attempt_id = AttemptId::mint(uuid::Uuid::new_v4().as_u128());
        let generation = ActiveChildGeneration::new();
        let sender = if enabled {
            AgentId::from_uuid_v7(child_id)
                .zip(factory)
                .and_then(|(agent_id, factory)| {
                    Some(AgentMessageSender {
                        active_message_tx: factory.active_message_tx.upgrade()?,
                        active_message_permits: Arc::clone(&factory.active_message_permits),
                        active_message_capacity: factory.active_message_capacity,
                        holder: AgentMessageHolder {
                            agent_id,
                            session_id: Arc::from(child_id),
                            attempt_id: attempt_id.clone(),
                            generation,
                        },
                        local_outbound_budget: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    })
                })
        } else {
            None
        };
        MintedChildIdentity {
            attempt_id,
            generation,
            sender,
        }
    }

    #[cfg(test)]
    pub(crate) fn holder(&self) -> &AgentMessageHolder {
        &self.holder
    }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.active_message_permits.available_permits()
    }

    pub async fn send(
        &self,
        request: super::active_message::ActiveAgentMessageRequest,
    ) -> super::active_message::ActiveAgentMessageOutcome {
        const MAX_LOCAL_OUTBOUND: usize = 32;
        let permit = match Arc::clone(&self.active_message_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                return super::active_message::ActiveAgentMessageOutcome::Saturated {
                    max_in_flight: self.active_message_capacity,
                };
            }
        };
        if self
            .local_outbound_budget
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            >= MAX_LOCAL_OUTBOUND
        {
            return super::active_message::ActiveAgentMessageOutcome::QuotaExceeded {
                kind: ActiveAgentMessageQuotaKind::AttemptOutbound,
                limit: MAX_LOCAL_OUTBOUND,
            };
        }
        let (respond_to, response_rx) = tokio::sync::oneshot::channel();
        let ingress = ActiveMessageIngress {
            request: super::active_message::SubagentActiveMessageRequest {
                request,
                sender_context: super::active_message::ActiveMessageSenderContext::GrantedChild {
                    holder: self.holder.clone(),
                },
                respond_to,
            },
            permit,
        };
        if self.active_message_tx.send(ingress).is_err() {
            return super::active_message::ActiveAgentMessageOutcome::ChannelClosed;
        }
        response_rx
            .await
            .unwrap_or(super::active_message::ActiveAgentMessageOutcome::ChannelClosed)
    }
}

impl std::fmt::Debug for AgentMessageSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentMessageSender")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct AgentMessageSenderResource(pub AgentMessageSender);

register_resource!(
    "distill",
    "AgentMessageSenderResource",
    AgentMessageSenderResource
);

pub(crate) struct MintedChildIdentity {
    pub(crate) attempt_id: AttemptId,
    pub(crate) generation: ActiveChildGeneration,
    pub(crate) sender: Option<AgentMessageSender>,
}

pub(crate) struct AgentMessageSenderFactory {
    active_message_tx: mpsc::WeakUnboundedSender<ActiveMessageIngress>,
    active_message_permits: Arc<tokio::sync::Semaphore>,
    active_message_capacity: usize,
}

impl AgentMessageSenderFactory {
    pub(crate) fn new(
        active_message_tx: mpsc::WeakUnboundedSender<ActiveMessageIngress>,
        active_message_permits: Arc<tokio::sync::Semaphore>,
        active_message_capacity: usize,
    ) -> Self {
        AgentMessageSenderFactory {
            active_message_tx,
            active_message_permits,
            active_message_capacity,
        }
    }
}

#[cfg(test)]
#[path = "agent_message_sender_tests.rs"]
mod tests;
