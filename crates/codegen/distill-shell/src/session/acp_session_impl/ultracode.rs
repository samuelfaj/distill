// Modified for Distill by Samuel Fajreldines, 2026.
//! Ultracode concern for `SessionActor`: per-turn orchestration reminder while the session flag is on.
use super::*;

pub(super) const ULTRACODE_DIRECTIVE: &str = "Ultracode mode is ON. Choose the smallest useful action: investigate concrete uncertainty or decompose only into independently verifiable smaller tasks. Preserve a suitable configured execution owner, considering available capability, relative cost and handoff overhead; if that information is unavailable, follow applicable defaults without assuming savings. Execute directly when assigned the slice or delegation is unavailable or adds no value; avoid re-delegating owned work. Keep inspection and implementation with one execution owner; parent may predeclare checks. Separate exploration or planning only for concrete uncertainty. No agent quota. Preserve useful independent parallel work and quality checks. Before delegating, state the objective, restrictions, owned paths, acceptance criteria and existing evidence references. Preserve the exact relevant acceptance contract in execution and verification handoffs. Write ownership does not bound verification: trace the affected user-visible path through actual entrypoints, including unchanged callers and adapters, within granted read permissions. Before inspecting new code, tests or results, predeclare discriminating inputs and expected outcomes from the original acceptance contract. After integration, exercise actual entrypoints at affected validation/data-preservation boundaries with those checks; report uncovered requirements and test limits. Preserve explicit model/effort pins and capability and permission ceilings. Delegate only tasks smaller than the parent objective, within the shared depth, concurrency and budget limits. If admission is refused, execute locally or wait for independent work to finish; never recursively retry or multiply budgets. Workflow-owned, harness-internal, output-budgeted and isolated-worktree agents are leaves. Integrate child results by inspecting their evidence and actual changes, then verify the parent objective; a child report alone is not completion. Report unresolved uncertainty and concrete runtime limits.";

impl SessionActor {
    /// Called once per user turn from `handle_prompt()`; pushes nothing while Ultracode is off.
    pub(super) fn inject_ultracode_reminder(&self) {
        if self.ultracode.load(std::sync::atomic::Ordering::Relaxed) {
            self.push_system_reminder_with_tag(ULTRACODE_DIRECTIVE, self.reminder_wrapper_tag());
        }
    }
}
