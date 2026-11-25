// Modified for Distill by Samuel Fajreldines, 2026.
//! File ownership between live children of one root session: overlap
//! rejection at spawn and the sibling list injected into a starting child.

use distill_tool_types::SubagentIsolationMode;

use super::super::file_ownership::{SiblingFiles, conflict_message, first_overlap};
use super::super::types::SubagentRequest;
use super::{ChildRunner, SubagentCoordinator};

fn requests_worktree(request: &SubagentRequest) -> bool {
    request.runtime_overrides.isolation == Some(SubagentIsolationMode::Worktree)
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    /// Live (pending, active or queued) non-workflow children of `request`'s
    /// root session that share the workspace, oldest first. Excludes the
    /// request itself, its spawner chain and worktree-isolated children.
    fn live_workspace_requests(&self, request: &SubagentRequest) -> Vec<&SubagentRequest> {
        let relevant = |other: &SubagentRequest| {
            other.id != request.id
                && other.parent_session_id == request.parent_session_id
                && !other.owner.is_workflow()
                && !other.cancel_token.is_cancelled()
                && !requests_worktree(other)
                && !self.graph.is_ancestor(&request.id, &other.id)
        };
        let mut live: Vec<&SubagentRequest> = self
            .pending
            .values()
            .filter(|child| child.launched)
            .map(|child| &child.request)
            .chain(
                self.active
                    .values()
                    .filter(|child| child.worktree_path.is_none())
                    .map(|child| &child.request),
            )
            .chain(self.queued.iter().map(|queued| &*queued.request))
            .filter(|other| relevant(other))
            .collect();
        // Child ids are UUIDv7, so id order is start order.
        live.sort_by(|a, b| a.id.cmp(&b.id));
        live
    }

    /// Rejection text when `request` declares files a live sibling already
    /// declared. Worktree-isolated and workflow spawns never conflict.
    pub(super) fn declared_file_conflict(&self, request: &SubagentRequest) -> Option<String> {
        if request.owner.is_workflow()
            || requests_worktree(request)
            || request.runtime_overrides.declared_files.is_empty()
        {
            return None;
        }
        self.live_workspace_requests(request)
            .into_iter()
            .find_map(|other| {
                first_overlap(
                    &request.runtime_overrides.declared_files,
                    &other.runtime_overrides.declared_files,
                )
                .map(|overlap| conflict_message(&other.description, overlap))
            })
    }

    /// Siblings to list in `request`'s prompt when it starts.
    pub(super) fn live_siblings_for(&self, request: &SubagentRequest) -> Vec<SiblingFiles> {
        if request.owner.is_workflow() || requests_worktree(request) {
            return Vec::new();
        }
        self.live_workspace_requests(request)
            .into_iter()
            .map(|other| SiblingFiles {
                description: other.description.clone(),
                files: other.runtime_overrides.declared_files.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
