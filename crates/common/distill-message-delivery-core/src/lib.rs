//! Source-typed message delivery values and operation authorization.

mod address;
mod envelope;
pub mod identity;
mod lifecycle;

pub use address::{
    AddressCandidate, AddressDecision, AddressPresence, AgentAddress, advertised_address,
    mint_child_address, resolve_address,
};
pub use envelope::{
    AgentSource, AuthorizedOperation, DeliveryEnvelope, DeliveryIdentity, HumanSource, Operation,
    OperationSet, Principal, UnsupportedOperation, authorize_operation,
};
pub use identity::{AgentId, AttemptId, IdentityAction, next_identity};
pub use lifecycle::{
    DeliveryMessage, MessageDeliveryLifecycle, OwnedDelivery, TerminalCause, TerminalTarget,
    TerminalTransition, TurnBinding,
};
