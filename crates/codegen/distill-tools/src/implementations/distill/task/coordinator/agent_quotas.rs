// Modified for Distill by Samuel Fajreldines, 2026.
//! Coordinator-authoritative quotas for sender-scoped agent messages.
use super::SubagentCoordinator;
use crate::implementations::distill::task::coordinator_state::{ChildRecord, ChildRunner};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOutcome, ActiveAgentMessageQuotaKind, ActiveMessageSenderContext,
};
use std::collections::HashMap;
use std::sync::Arc;
use distill_message_delivery_core::{AgentId, AttemptId};
pub(super) const MAX_SENDER_TARGET_IN_FLIGHT: usize = 4;
pub(super) const MAX_ATTEMPT_OUTBOUND: usize = 32;
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SenderAttemptKey(AgentId, AttemptId);
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TargetIncarnationKey {
    target_id: String,
    generation: uuid::Uuid,
}
impl TargetIncarnationKey {
    fn mint(target_id: &str) -> Self {
        TargetIncarnationKey {
            target_id: target_id.to_owned(),
            generation: uuid::Uuid::now_v7(),
        }
    }
}
#[derive(Debug)]
pub(crate) struct ActiveMessageQuotaAdmission {
    _permit: ActiveMessagePairPermit,
}
#[derive(Debug)]
struct ActiveMessagePairPermit {
    count: Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for ActiveMessagePairPermit {
    fn drop(&mut self) {
        self.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
/// Per-sender in-flight counts owned by one target incarnation; abandoning it releases them all.
type IncarnationPermits = HashMap<SenderAttemptKey, Arc<std::sync::atomic::AtomicUsize>>;
#[derive(Default)]
pub(super) struct AgentMessageQuotas {
    outbound: HashMap<SenderAttemptKey, usize>,
    pairs: HashMap<TargetIncarnationKey, IncarnationPermits>,
    target_incarnations: HashMap<String, TargetIncarnationKey>,
    displaced_incarnations: HashMap<String, TargetIncarnationKey>,
}
impl AgentMessageQuotas {
    pub(super) fn admit_outbound(
        &mut self,
        context: &ActiveMessageSenderContext,
    ) -> Result<(), ActiveAgentMessageOutcome> {
        let ActiveMessageSenderContext::GrantedChild { holder } = context else {
            return Ok(());
        };
        let key = SenderAttemptKey(holder.agent_id().clone(), holder.attempt_id().clone());
        let count = self.outbound.entry(key).or_default();
        if *count >= MAX_ATTEMPT_OUTBOUND {
            return Err(ActiveAgentMessageOutcome::QuotaExceeded {
                kind: ActiveAgentMessageQuotaKind::AttemptOutbound,
                limit: MAX_ATTEMPT_OUTBOUND,
            });
        }
        *count += 1;
        Ok(())
    }
    pub(super) fn begin_pair(
        &mut self,
        context: &ActiveMessageSenderContext,
        target: TargetIncarnationKey,
    ) -> Result<Option<ActiveMessageQuotaAdmission>, ActiveAgentMessageOutcome> {
        let ActiveMessageSenderContext::GrantedChild { holder } = context else {
            return Ok(None);
        };
        let sender = SenderAttemptKey(holder.agent_id().clone(), holder.attempt_id().clone());
        self.pairs.retain(|_, permits| {
            permits.retain(|_, count| count.load(std::sync::atomic::Ordering::Acquire) != 0);
            !permits.is_empty()
        });
        let count = self
            .pairs
            .entry(target)
            .or_default()
            .entry(sender)
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        if count.load(std::sync::atomic::Ordering::Acquire) >= MAX_SENDER_TARGET_IN_FLIGHT {
            return Err(ActiveAgentMessageOutcome::QuotaExceeded {
                kind: ActiveAgentMessageQuotaKind::SenderTargetInFlight,
                limit: MAX_SENDER_TARGET_IN_FLIGHT,
            });
        }
        count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Some(ActiveMessageQuotaAdmission {
            _permit: ActiveMessagePairPermit {
                count: Arc::clone(count),
            },
        }))
    }
    fn current_target(&mut self, target_id: &str) -> TargetIncarnationKey {
        self.target_incarnations
            .entry(target_id.to_owned())
            .or_insert_with(|| TargetIncarnationKey::mint(target_id))
            .clone()
    }
    #[cfg(test)]
    pub(super) fn pair_in_flight_counts(&self) -> Vec<usize> {
        self.pairs
            .values()
            .flat_map(|permits| permits.values())
            .map(|count| count.load(std::sync::atomic::Ordering::Acquire))
            .collect()
    }

    fn clear_attempt(&mut self, agent_id: &AgentId, attempt_id: &AttemptId) {
        let sender = SenderAttemptKey(agent_id.clone(), attempt_id.clone());
        self.outbound.remove(&sender);
        for permits in self.pairs.values_mut() {
            permits.remove(&sender);
        }
    }
    /// The incarnation is over: whatever its futures still hold no longer counts against the target.
    fn abandon(&mut self, target: Option<&TargetIncarnationKey>) {
        if let Some(target) = target {
            self.pairs.remove(target);
        }
    }
}
impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn current_target_incarnation(&mut self, id: &str) -> TargetIncarnationKey {
        self.agent_message_quotas.current_target(id)
    }
    pub(super) fn begin_wake_incarnation(&mut self, id: &str) -> TargetIncarnationKey {
        if self
            .agent_message_quotas
            .displaced_incarnations
            .contains_key(id)
        {
            return self.current_target_incarnation(id);
        }
        let target = TargetIncarnationKey::mint(id);
        if let Some(previous) = self
            .agent_message_quotas
            .target_incarnations
            .insert(id.to_owned(), target.clone())
        {
            self.agent_message_quotas
                .displaced_incarnations
                .insert(id.to_owned(), previous);
        }
        target
    }
    /// The wake committed: the incarnation it displaced is gone for good.
    pub(super) fn activate_wake_incarnation(&mut self, id: &str) {
        let displaced = self.agent_message_quotas.displaced_incarnations.remove(id);
        self.agent_message_quotas.abandon(displaced.as_ref());
    }
    /// The wake never committed: its incarnation is abandoned and the displaced one, if any, is current again.
    pub(super) fn restore_completed_incarnation(&mut self, id: &str) {
        let abandoned = match self.agent_message_quotas.displaced_incarnations.remove(id) {
            Some(previous) => self
                .agent_message_quotas
                .target_incarnations
                .insert(id.to_owned(), previous),
            None => self.agent_message_quotas.target_incarnations.remove(id),
        };
        self.agent_message_quotas.abandon(abandoned.as_ref());
    }
    /// The child completed: its incarnation stays the record's identity but owns no permits.
    pub(super) fn complete_target_incarnation(&mut self, id: &str) {
        let quotas = &mut self.agent_message_quotas;
        let current = quotas.target_incarnations.get(id).cloned();
        quotas.abandon(current.as_ref());
    }
    pub(super) fn clear_target_incarnation(&mut self, id: &str) {
        let current = self.agent_message_quotas.target_incarnations.remove(id);
        let displaced = self.agent_message_quotas.displaced_incarnations.remove(id);
        self.agent_message_quotas.abandon(current.as_ref());
        self.agent_message_quotas.abandon(displaced.as_ref());
    }
    pub(super) fn clear_message_sender_attempt(
        &mut self,
        id: &str,
        record: &ChildRecord<R::Control>,
    ) {
        if let Some(agent_id) = AgentId::from_uuid_v7(id) {
            self.agent_message_quotas
                .clear_attempt(&agent_id, record.attempt_id());
        }
    }
}
