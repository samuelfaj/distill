// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode concern for `SessionActor`: per-turn orchestration reminder while the session flag is on.
use super::*;

pub(super) const ULTRACODE_DIRECTIVE: &str = "Ultracode mode is ON. For this objective choose the smallest useful next action: execute directly when the work is understood; investigate a specific uncertainty; or decompose only when bounded smaller tasks can be verified independently. Delegation is optional in this mode, overriding default delegation preferences, with no agent quota. Keep targeted inspection, implementation and checks for a bounded known change with one execution owner; separate exploration or planning only to resolve a concrete uncertainty. Preserve useful independent parallel work and quality checks. Before delegating, state the objective, restrictions, owned paths, acceptance criteria and existing evidence references. Preserve explicit model/effort pins and capability and permission ceilings. Delegate only tasks smaller than the parent objective, within the shared depth, concurrency and budget limits. If admission is refused, execute locally or wait for independent work to finish; never recursively retry or multiply budgets. Workflow-owned, harness-internal, output-budgeted and isolated-worktree agents are leaves. Integrate child results by inspecting their evidence and actual changes, then verify the parent objective; a child report alone is not completion. Report unresolved uncertainty and concrete runtime limits.";

impl SessionActor {
    /// Called once per user turn from `handle_prompt()`; pushes nothing while Ultracode is off.
    pub(super) fn inject_ultracode_reminder(&self) {
        if self.ultracode.load(std::sync::atomic::Ordering::Relaxed) {
            self.push_system_reminder_with_tag(ULTRACODE_DIRECTIVE, self.reminder_wrapper_tag());
        }
    }
}
