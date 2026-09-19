// Modified for Distill by Samuel Fajreldines, 2026.
//! Sender verification and child agent-target resolution.

use crate::implementations::distill::task::coordinator_state::ChildRunner;
use crate::implementations::distill::task::root_control::RootControl;
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOutcome, ActiveAgentMessageSource, ActiveMessageRoute,
    ActiveMessageSenderContext, ActiveMessageTarget,
};

use super::SubagentCoordinator;
use super::wake::{ResolvedSend, WakeAuthorization};

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn verify_message_sender(
        &self,
        context: &ActiveMessageSenderContext,
    ) -> Result<(), ActiveAgentMessageOutcome> {
        let ActiveMessageSenderContext::GrantedChild { holder } = context else {
            return Ok(());
        };
        let Some(child) = self.active.get(holder.agent_id().as_str()) else {
            return Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        };
        if child.request.id != child.child_session_id
            || child.request.owner.is_workflow()
            || child.child_session_id != holder.session_id()
            || child.attempt_id != *holder.attempt_id()
            || child.generation != holder.generation()
            || child.cancellation.is_cancelled()
            || child.explicitly_killed
            || child.active_messages.is_finalizing()
        {
            return Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        Ok(())
    }

    pub(super) fn resolve_agent_target(
        &mut self,
        context: &ActiveMessageSenderContext,
        target: &ActiveMessageTarget,
        resolved_target: Option<&super::agent_quotas::TargetIncarnationKey>,
    ) -> (ResolvedSend, ActiveMessageRoute) {
        if matches!(context, ActiveMessageSenderContext::RootSession { .. }) {
            let ActiveMessageTarget::Agent { agent_id } = target else {
                return failed(ActiveAgentMessageOutcome::Unsupported);
            };
            let ActiveMessageSenderContext::RootSession { session_id } = context else {
                unreachable!("root sender checked above")
            };
            let Some(sender) = self.runner.resolve_root_session(session_id) else {
                return failed(ActiveAgentMessageOutcome::Unsupported);
            };
            if sender.agent_id() == agent_id {
                return failed(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
            }
            if self.graph.contains(agent_id.as_str()) {
                let id = agent_id.as_str();
                let route = if self.graph.root_session(id) == Some(session_id) {
                    ActiveMessageRoute::ParentToOwnedDescendant
                } else {
                    ActiveMessageRoute::Peer
                };
                return (self.resolve_typed_child_id(id, resolved_target), route);
            }
            let Some(control) = self.runner.resolve_root(agent_id) else {
                return failed(ActiveAgentMessageOutcome::Unsupported);
            };
            return (self.resolve_root_control(control), ActiveMessageRoute::Peer);
        }
        let ActiveMessageSenderContext::GrantedChild { holder } = context else {
            return failed(ActiveAgentMessageOutcome::Unsupported);
        };
        let completed_target = match target {
            ActiveMessageTarget::ChildId(id)
                if self.graph.is_reachable_from(id, holder.session_id()) =>
            {
                self.completed.get(id)
            }
            ActiveMessageTarget::Address(address) => self.completed.values().find(|child| {
                child.agent_address.as_ref() == Some(address)
                    && self
                        .graph
                        .is_reachable_from(&child.request.id, holder.session_id())
            }),
            ActiveMessageTarget::Agent { agent_id } if self.graph.contains(agent_id.as_str()) => {
                self.completed.get(agent_id.as_str())
            }
            ActiveMessageTarget::ChildId(_)
            | ActiveMessageTarget::Agent { .. }
            | ActiveMessageTarget::Parent => None,
        };
        if completed_target
            .is_some_and(|child| !child.request.owner.is_workflow() && !child.wake_eligible)
        {
            return failed(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        match target {
            ActiveMessageTarget::Parent => {
                let Some(spawner) = self.graph.direct_spawner(holder.agent_id().as_str()) else {
                    let Some(root_session) = self.graph.root_session(holder.agent_id().as_str())
                    else {
                        return failed(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
                    };
                    let Some(control) = self.runner.resolve_root_session(root_session) else {
                        return failed(ActiveAgentMessageOutcome::Unsupported);
                    };
                    if control.agent_id() == holder.agent_id() {
                        return failed(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
                    }
                    return (
                        self.resolve_root_control(control),
                        ActiveMessageRoute::DescendantToParent,
                    );
                };
                let child_id = self
                    .active_child_for_session(spawner)
                    .map(|child| child.request.id.clone());
                match child_id.as_deref().and_then(|id| self.active.get(id)) {
                    Some(child) if child.request.owner.is_workflow() => {
                        failed(ActiveAgentMessageOutcome::NotFoundOrNotOwned)
                    }
                    Some(child)
                        if child.cancellation.is_cancelled()
                            || child.explicitly_killed
                            || child.active_messages.is_finalizing() =>
                    {
                        failed(ActiveAgentMessageOutcome::NotActiveOrFinalizing)
                    }
                    Some(child) => {
                        let id = child.request.id.clone();
                        (
                            ResolvedSend::Admit {
                                subagent_id: id.clone(),
                                target: self.current_target_incarnation(&id),
                            },
                            ActiveMessageRoute::DescendantToParent,
                        )
                    }
                    None => failed(ActiveAgentMessageOutcome::NotActiveOrFinalizing),
                }
            }
            ActiveMessageTarget::Agent { agent_id } => {
                if agent_id == holder.agent_id() {
                    return failed(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
                }
                let id = agent_id.as_str();
                if !self.graph.contains(id) {
                    let Some(control) = self.runner.resolve_root(agent_id) else {
                        return failed(ActiveAgentMessageOutcome::Unsupported);
                    };
                    let route = if self.graph.root_session(holder.agent_id().as_str())
                        == Some(control.session_id())
                    {
                        ActiveMessageRoute::DescendantToParent
                    } else {
                        ActiveMessageRoute::Peer
                    };
                    return (self.resolve_root_control(control), route);
                }
                let route = if self.graph.is_ancestor(holder.agent_id().as_str(), id) {
                    ActiveMessageRoute::DescendantToParent
                } else if self.graph.is_reachable_from(id, holder.session_id()) {
                    ActiveMessageRoute::ParentToOwnedDescendant
                } else {
                    ActiveMessageRoute::Peer
                };
                (self.resolve_typed_child_id(id, resolved_target), route)
            }
            ActiveMessageTarget::Address(address) => (
                self.resolve_address_send(address, holder.session_id(), resolved_target),
                ActiveMessageRoute::ParentToOwnedDescendant,
            ),
            ActiveMessageTarget::ChildId(id) => (
                self.resolve_owned_child_id(id, holder.session_id(), resolved_target),
                ActiveMessageRoute::ParentToOwnedDescendant,
            ),
        }
    }

    fn resolve_typed_child_id(
        &mut self,
        id: &str,
        resolved_target: Option<&super::agent_quotas::TargetIncarnationKey>,
    ) -> ResolvedSend {
        let workflow = self
            .active
            .get(id)
            .is_some_and(|child| child.request.owner.is_workflow())
            || self
                .pending
                .get(id)
                .is_some_and(|child| child.request.owner.is_workflow())
            || self
                .completed
                .get(id)
                .is_some_and(|child| child.request.owner.is_workflow());
        if workflow {
            return ResolvedSend::Fail(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
        }
        if self
            .active
            .get(id)
            .is_some_and(|child| child.explicitly_killed)
            || self
                .pending
                .get(id)
                .is_some_and(|child| child.explicitly_killed)
            || self
                .completed
                .get(id)
                .is_some_and(|child| !child.wake_eligible)
        {
            return ResolvedSend::Fail(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        if self.active.contains_key(id) {
            ResolvedSend::Admit {
                subagent_id: id.to_owned(),
                target: self.current_target_incarnation(id),
            }
        } else {
            // A peer root owns none of this lineage; the wake path must trust this resolution.
            self.resolve_inactive_child_id(id, resolved_target, WakeAuthorization::ResolvedTarget)
        }
    }

    pub(super) fn message_source(context: &ActiveMessageSenderContext) -> ActiveAgentMessageSource {
        match context {
            ActiveMessageSenderContext::HumanRoot { .. } => ActiveAgentMessageSource::Human,
            ActiveMessageSenderContext::RootSession { .. }
            | ActiveMessageSenderContext::GrantedChild { .. } => ActiveAgentMessageSource::Agent,
        }
    }

    pub(super) fn message_sender_session(context: &ActiveMessageSenderContext) -> &str {
        match context {
            ActiveMessageSenderContext::RootSession { session_id }
            | ActiveMessageSenderContext::HumanRoot { session_id } => session_id,
            ActiveMessageSenderContext::GrantedChild { holder } => holder.session_id(),
        }
    }
}

fn failed(outcome: ActiveAgentMessageOutcome) -> (ResolvedSend, ActiveMessageRoute) {
    (
        ResolvedSend::Fail(outcome),
        ActiveMessageRoute::ParentToOwnedDescendant,
    )
}

#[cfg(test)]
#[path = "agent_targets_tests.rs"]
pub(super) mod tests;
