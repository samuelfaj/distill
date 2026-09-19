use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    Human,
    Agent,
    Runtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Queue,
    Steer,
    Interject,
    InterruptAndSend,
}

pub struct HumanSource(());

pub struct AgentSource(());

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryIdentity<MessageId, PromptId>(MessageId, PromptId);

impl<MessageId, PromptId> DeliveryIdentity<MessageId, PromptId> {
    pub fn new(message_id: MessageId, prompt_id: PromptId) -> Self {
        Self(message_id, prompt_id)
    }

    pub fn into_parts(self) -> (MessageId, PromptId) {
        (self.0, self.1)
    }
}

pub struct DeliveryEnvelope<Source, Grant, Content, Identity> {
    principal: Principal,
    operation: Operation,
    content: Content,
    identity: Identity,
    grant: Grant,
    source: PhantomData<Source>,
}

impl<G, C, I> DeliveryEnvelope<HumanSource, G, C, I> {
    #[must_use]
    pub fn from_human(operation: Operation, content: C, identity: I, grant: G) -> Self {
        Self {
            principal: Principal::Human,
            operation,
            content,
            identity,
            grant,
            source: PhantomData,
        }
    }

    pub fn into_parts(self) -> (Operation, C, I, G) {
        (self.operation, self.content, self.identity, self.grant)
    }
}

impl<G, C, I> DeliveryEnvelope<AgentSource, G, C, I> {
    #[must_use]
    pub fn from_agent(operation: Operation, content: C, identity: I, grant: G) -> Self {
        Self {
            principal: Principal::Agent,
            operation,
            content,
            identity,
            grant,
            source: PhantomData,
        }
    }

    pub fn into_parts(self) -> (Operation, C, I, G) {
        (self.operation, self.content, self.identity, self.grant)
    }
}

impl<S, G, C, I> DeliveryEnvelope<S, G, C, I> {
    pub fn principal(&self) -> Principal {
        self.principal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationSet(u8);

impl OperationSet {
    pub const QUEUE: Self = Self(1 << 0);
    pub const QUEUE_AND_STEER: Self = Self((1 << 0) | (1 << 1));
    pub const QUEUE_STEER_AND_INTERJECT: Self = Self((1 << 0) | (1 << 1) | (1 << 2));

    pub fn contains(self, operation: Operation) -> bool {
        let flag = match operation {
            Operation::Queue => 1 << 0,
            Operation::Steer => 1 << 1,
            Operation::Interject => 1 << 2,
            Operation::InterruptAndSend => 1 << 3,
        };
        self.0 & flag != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizedOperation(Operation);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnsupportedOperation(Operation);

pub fn authorize_operation(
    allowed: OperationSet,
    requested: Operation,
) -> Result<AuthorizedOperation, UnsupportedOperation> {
    if allowed.contains(requested) {
        Ok(AuthorizedOperation(requested))
    } else {
        Err(UnsupportedOperation(requested))
    }
}

#[cfg(test)]
#[path = "envelope_tests.rs"]
mod tests;
