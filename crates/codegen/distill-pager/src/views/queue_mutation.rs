//! Row-mutation policy for the queue pane: how a row's origin and wire kind project to edit/send
//! capabilities. A `ReadOnly` pane protects every row, including unknown kinds and local rows, so a
//! mirrored queue the view cannot address never offers a control that would route to another session.

/// How a [`crate::views::queue_pane::QueuePane`] derives its rows' capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueMutation {
    /// Each row's origin and wire kind decide (the session's own queue).
    PerRowKind,
    /// Every row is protected (a subagent's queue mirrored into its fullscreen view).
    ReadOnly,
}

/// Capabilities projected from a queue row under the pane's [`QueueMutation`].
/// Under `PerRowKind`, local rows and unknown server kinds stay editable/sendable for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServerRowCapabilities {
    can_mutate: bool,
}

impl ServerRowCapabilities {
    const EDITABLE: Self = Self { can_mutate: true };
    const PROTECTED: Self = Self { can_mutate: false };

    /// A server row's capabilities from its wire `kind`.
    pub(crate) fn for_pane(kind: &str, mutation: QueueMutation) -> Self {
        match mutation {
            QueueMutation::ReadOnly => Self::PROTECTED,
            QueueMutation::PerRowKind if kind == "parent_agent_message" => Self::PROTECTED,
            QueueMutation::PerRowKind => Self::EDITABLE,
        }
    }

    /// A client-local `pending_prompts` row's capabilities.
    pub(crate) fn for_local(mutation: QueueMutation) -> Self {
        match mutation {
            QueueMutation::ReadOnly => Self::PROTECTED,
            QueueMutation::PerRowKind => Self::EDITABLE,
        }
    }

    pub(crate) fn is_protected(self) -> bool {
        !self.can_mutate
    }

    pub(crate) fn can_edit(self) -> bool {
        self.can_mutate
    }

    pub(crate) fn can_delete(self) -> bool {
        self.can_mutate
    }

    pub(crate) fn can_reorder(self) -> bool {
        self.can_mutate
    }

    pub(crate) fn can_send_now(self) -> bool {
        self.can_mutate
    }
}

#[cfg(test)]
#[path = "queue_mutation_tests.rs"]
mod tests;
