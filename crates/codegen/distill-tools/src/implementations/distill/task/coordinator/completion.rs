//! Between-turn completion buffering: which session is told a child finished.

use super::super::coordinator_state::{
    BufferedCompletion, cap_completion_output, completion_summary,
};
use super::super::types::{SubagentRequest, SubagentResult, SubagentSnapshot};
use super::{ChildRunner, SubagentCoordinator};

/// Sessions unloaded without a `TeardownSession` must not grow the buffer unboundedly.
const MAX_PENDING_COMPLETIONS: usize = 256;

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn buffer_completion(
        &mut self,
        request: &SubagentRequest,
        result: &SubagentResult,
        snapshot: &SubagentSnapshot,
    ) {
        if !self.config.buffer_completions {
            return;
        }
        let target = if request.surface_completion {
            // Only direct workflow children skip: nested ones inherit workflow
            // ownership at reparent yet still owe their spawner a reminder.
            if request.owner.is_workflow() {
                return;
            }
            Some(request.parent_session_id.as_str())
        } else {
            self.graph
                .surface_target(&request.id)
                .filter(|spawner| self.is_spawner_live(spawner))
        };
        let Some(parent_session_id) = target else {
            return;
        };
        let mut summary = completion_summary(request, result, snapshot);
        if let Some(cap) = self.config.buffered_completion_output_cap {
            summary.output = cap_completion_output(&summary.output, cap);
        }
        self.pending_completions.push(BufferedCompletion {
            parent_session_id: parent_session_id.to_owned(),
            summary,
        });
        if self.pending_completions.len() > MAX_PENDING_COMPLETIONS {
            let excess = self.pending_completions.len() - MAX_PENDING_COMPLETIONS;
            self.pending_completions.drain(..excess);
        }
    }

    /// A finished spawner has no reader left.
    pub(super) fn purge_completions_for_spawner(&mut self, finished_session: &str) {
        self.pending_completions
            .retain(|completion| completion.parent_session_id != finished_session);
    }

    /// Cancelled counts as gone: nothing will read the buffer.
    fn is_spawner_live(&self, spawner_session_id: &str) -> bool {
        self.active_child_for_session(spawner_session_id)
            .is_some_and(|child| !child.cancellation.is_cancelled())
    }
}
