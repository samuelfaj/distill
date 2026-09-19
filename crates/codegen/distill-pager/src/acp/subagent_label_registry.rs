//! Display labels for the subagents a session has spawned, recorded on `SubagentSpawned` (a wake re-spawn
//! overwrites) and resolved when a `send_subagent_message` row is built, since render time has no registry.

use std::collections::HashMap;
use std::sync::Arc;

use crate::scrollback::blocks::tool::SentMessageTarget;

/// Both halves come from the same spawn, so a row that names the label can also open the child.
#[derive(Debug, Clone)]
struct SubagentLabel {
    label: Arc<str>,
    child_session_id: Arc<str>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SubagentLabelRegistry {
    /// Keyed by the tool's `subagent_id` (equal to `child_session_id` today), never by the child session directly.
    labels: HashMap<String, SubagentLabel>,
}

impl SubagentLabelRegistry {
    pub(crate) fn record(
        &mut self,
        subagent_id: &str,
        label: impl Into<Arc<str>>,
        child_session_id: impl Into<Arc<str>>,
    ) {
        self.labels.insert(
            subagent_id.to_owned(),
            SubagentLabel {
                label: label.into(),
                child_session_id: child_session_id.into(),
            },
        );
    }

    /// The row's target: the recorded label, or the raw id when no spawn was seen for it. The `parent` alias is the
    /// caller's to recognize before asking here.
    pub(crate) fn resolve(&self, subagent_id: String) -> SentMessageTarget {
        match self.labels.get(&subagent_id) {
            Some(SubagentLabel {
                label,
                child_session_id,
            }) => SentMessageTarget::Named {
                label: Arc::clone(label),
                child_session_id: Arc::clone(child_session_id),
            },
            None => SentMessageTarget::Unresolved { subagent_id },
        }
    }
}
