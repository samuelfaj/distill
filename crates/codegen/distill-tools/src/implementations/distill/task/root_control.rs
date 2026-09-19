// Modified for Distill by Samuel Fajreldines, 2026.
//! Host-specific delivery and identity for a resident root agent.
use crate::implementations::distill::task::coordinator::{
    ActiveMessageAdmission, SendBoxFuture,
};
use crate::implementations::distill::task::types::ActiveAgentMessageDelivery;
use distill_message_delivery_core::{AgentId, AttemptId};
/// Human-input epoch used to bound one autonomous root turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserInputGeneration(u64);
impl UserInputGeneration {
    #[must_use]
    pub fn new(value: u64) -> Self {
        UserInputGeneration(value)
    }
}
/// Host receipt capability retained for every resident root delivery.
pub trait RootReceiptSink: Clone + 'static {
    fn is_closed(&self) -> bool;
}
/// Receipt sink for an uninhabited root control.
#[derive(Clone)]
pub enum NoRootReceiptSink {}
impl RootReceiptSink for NoRootReceiptSink {
    fn is_closed(&self) -> bool {
        ::core::unreachable!()
    }
}
/// One host-minted agent incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentMessageGeneration(uuid::Uuid);
impl AgentMessageGeneration {
    #[must_use]
    pub fn mint(entropy: u128) -> Self {
        AgentMessageGeneration(uuid::Uuid::from_u128(entropy))
    }
    pub(crate) fn new() -> Self {
        AgentMessageGeneration(uuid::Uuid::now_v7())
    }
}
/// A resident root control must keep its actor and receipt drain live through admission.
pub trait RootControl: 'static {
    type ReceiptSink: RootReceiptSink;
    fn agent_id(&self) -> &AgentId;
    fn session_id(&self) -> &str;
    fn attempt_id(&self) -> &AttemptId;
    fn generation(&self) -> AgentMessageGeneration;
    fn user_input_generation(&self) -> UserInputGeneration;
    fn label(&self) -> &str;
    fn receipt_sink(&self) -> Self::ReceiptSink;
    fn deliver(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission>;
}
/// Default for hosts that cannot resolve resident roots.
pub enum NoRootControl {}
impl RootControl for NoRootControl {
    type ReceiptSink = NoRootReceiptSink;
    fn agent_id(&self) -> &AgentId {
        ::core::unreachable!()
    }
    fn session_id(&self) -> &str {
        ::core::unreachable!()
    }
    fn attempt_id(&self) -> &AttemptId {
        ::core::unreachable!()
    }
    fn generation(&self) -> AgentMessageGeneration {
        ::core::unreachable!()
    }
    fn user_input_generation(&self) -> UserInputGeneration {
        ::core::unreachable!()
    }
    fn label(&self) -> &str {
        ::core::unreachable!()
    }
    fn receipt_sink(&self) -> Self::ReceiptSink {
        ::core::unreachable!()
    }
    fn deliver(
        &self,
        _delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        ::core::unreachable!()
    }
}
