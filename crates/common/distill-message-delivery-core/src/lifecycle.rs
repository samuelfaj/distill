//! Turn-bound ownership for messages that may be delivered at a safe point.

use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnBinding<PromptId, Epoch> {
    prompt_id: PromptId,
    epoch: Epoch,
}

impl<PromptId, Epoch> TurnBinding<PromptId, Epoch> {
    pub fn new(prompt_id: PromptId, epoch: Epoch) -> Self {
        Self { prompt_id, epoch }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryMessage<Identity, Source, Content> {
    identity: Identity,
    source: Source,
    content: Content,
}

impl<Identity, Source, Content> DeliveryMessage<Identity, Source, Content> {
    pub fn new(identity: Identity, source: Source, content: Content) -> Self {
        Self {
            identity,
            source,
            content,
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn source(&self) -> &Source {
        &self.source
    }

    pub fn content(&self) -> &Content {
        &self.content
    }

    pub fn into_parts(self) -> (Identity, Source, Content) {
        (self.identity, self.source, self.content)
    }
}

pub struct OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch> {
    binding: TurnBinding<PromptId, Epoch>,
    message: DeliveryMessage<Identity, Source, Content>,
    completion: Completion,
}

impl<Identity, Source, Content, Completion, PromptId, Epoch>
    OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>
{
    pub fn message(&self) -> &DeliveryMessage<Identity, Source, Content> {
        &self.message
    }

    pub fn into_parts(
        self,
    ) -> (
        TurnBinding<PromptId, Epoch>,
        DeliveryMessage<Identity, Source, Content>,
        Completion,
    ) {
        (self.binding, self.message, self.completion)
    }
}

enum DeliverySlot<Identity, Source, Content, Completion, PromptId, Epoch> {
    Pending(OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>),
    Projecting(OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>),
    Delivered(OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>),
}

impl<Identity, Source, Content, Completion, PromptId, Epoch>
    DeliverySlot<Identity, Source, Content, Completion, PromptId, Epoch>
{
    fn owned(&self) -> &OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch> {
        match self {
            Self::Pending(owned) | Self::Projecting(owned) | Self::Delivered(owned) => owned,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalCause {
    Completion,
    SoftCancel,
    Rewind,
    HardTeardown,
    ActorDrop,
}

impl TerminalCause {
    fn falls_back(self) -> bool {
        matches!(self, Self::Completion | Self::SoftCancel | Self::Rewind)
    }
}

pub enum TerminalTarget<'a, PromptId, Epoch> {
    Turn(&'a TurnBinding<PromptId, Epoch>),
    All,
}

pub struct TerminalTransition<Identity, Source, Content, Completion, PromptId, Epoch> {
    pub fallbacks: Vec<OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>>,
    pub completions: Vec<OwnedDelivery<Identity, Source, Content, Completion, PromptId, Epoch>>,
}

pub struct MessageDeliveryLifecycle<Identity, Source, Content, Completion, PromptId, Epoch> {
    slots: VecDeque<DeliverySlot<Identity, Source, Content, Completion, PromptId, Epoch>>,
}

impl<Identity, Source, Content, Completion, PromptId, Epoch> Default
    for MessageDeliveryLifecycle<Identity, Source, Content, Completion, PromptId, Epoch>
{
    fn default() -> Self {
        Self {
            slots: VecDeque::new(),
        }
    }
}

impl<Identity, Source, Content, Completion, PromptId, Epoch>
    MessageDeliveryLifecycle<Identity, Source, Content, Completion, PromptId, Epoch>
{
    pub fn len(&self) -> usize {
        self.slots.len()
    }
}

impl<Identity, Source, Content, Completion, PromptId, Epoch>
    MessageDeliveryLifecycle<Identity, Source, Content, Completion, PromptId, Epoch>
where
    Identity: PartialEq,
    PromptId: PartialEq,
    Epoch: PartialEq,
{
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn contains_identity(&self, identity: &Identity) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.owned().message.identity == *identity)
    }

    pub fn admit_pending(
        &mut self,
        binding: TurnBinding<PromptId, Epoch>,
        message: DeliveryMessage<Identity, Source, Content>,
        completion: Completion,
    ) -> Result<(), DeliveryMessage<Identity, Source, Content>> {
        if self
            .slots
            .iter()
            .any(|slot| slot.owned().message.identity == message.identity)
        {
            return Err(message);
        }
        self.slots.push_back(DeliverySlot::Pending(OwnedDelivery {
            binding,
            message,
            completion,
        }));
        Ok(())
    }

    pub fn pending_messages(
        &self,
        binding: &TurnBinding<PromptId, Epoch>,
    ) -> Vec<DeliveryMessage<Identity, Source, Content>>
    where
        Identity: Clone,
        Source: Clone,
        Content: Clone,
    {
        self.slots
            .iter()
            .filter_map(|slot| match slot {
                DeliverySlot::Pending(owned) if owned.binding == *binding => {
                    Some(owned.message.clone())
                }
                _ => None,
            })
            .collect()
    }

    pub fn begin_delivery(
        &mut self,
        binding: &TurnBinding<PromptId, Epoch>,
    ) -> Vec<DeliveryMessage<Identity, Source, Content>>
    where
        Identity: Clone,
        Source: Clone,
        Content: Clone,
    {
        let messages = self.pending_messages(binding);
        if messages.is_empty() {
            return messages;
        }
        self.slots = std::mem::take(&mut self.slots)
            .into_iter()
            .map(|slot| match slot {
                DeliverySlot::Pending(owned) if owned.binding == *binding => {
                    DeliverySlot::Projecting(owned)
                }
                slot => slot,
            })
            .collect();
        messages
    }

    pub fn finish_delivery<Error>(
        &mut self,
        binding: &TurnBinding<PromptId, Epoch>,
        commit: impl FnOnce(&[DeliveryMessage<Identity, Source, Content>]) -> Result<(), Error>,
    ) -> Result<Vec<DeliveryMessage<Identity, Source, Content>>, Error>
    where
        Identity: Clone,
        Source: Clone,
        Content: Clone,
    {
        let messages = self
            .slots
            .iter()
            .filter_map(|slot| match slot {
                DeliverySlot::Projecting(owned) if owned.binding == *binding => {
                    Some(owned.message.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(messages);
        }
        commit(&messages)?;
        self.slots = std::mem::take(&mut self.slots)
            .into_iter()
            .map(|slot| match slot {
                DeliverySlot::Projecting(owned) if owned.binding == *binding => {
                    DeliverySlot::Delivered(owned)
                }
                slot => slot,
            })
            .collect();
        Ok(messages)
    }

    pub fn transition(
        &mut self,
        target: TerminalTarget<'_, PromptId, Epoch>,
        cause: TerminalCause,
    ) -> TerminalTransition<Identity, Source, Content, Completion, PromptId, Epoch> {
        let mut transition = TerminalTransition {
            fallbacks: Vec::new(),
            completions: Vec::new(),
        };
        self.slots = std::mem::take(&mut self.slots)
            .into_iter()
            .filter_map(|slot| {
                let matches = match target {
                    TerminalTarget::Turn(binding) => slot.owned().binding == *binding,
                    TerminalTarget::All => true,
                };
                if !matches {
                    return Some(slot);
                }
                match slot {
                    DeliverySlot::Pending(owned) if cause.falls_back() => {
                        transition.fallbacks.push(owned)
                    }
                    DeliverySlot::Pending(owned)
                    | DeliverySlot::Projecting(owned)
                    | DeliverySlot::Delivered(owned) => transition.completions.push(owned),
                }
                None
            })
            .collect();
        transition
    }
}
