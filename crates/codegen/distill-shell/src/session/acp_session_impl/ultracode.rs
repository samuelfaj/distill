// Modified for Distill by Samuel Fajreldines, 2026.
//! Authoritative per-turn execution mode for `SessionActor`.
use super::*;

pub(super) const ULTRACODE_DIRECTIVE: &str = "Ultracode mode is ON. For this objective choose the smallest useful next action: execute directly when the work is understood; investigate a specific uncertainty; or decompose only when bounded smaller tasks can be verified independently. Delegation is optional in this mode, overriding default delegation preferences, with no agent quota. Keep targeted inspection, implementation and checks for a bounded known change with one execution owner; separate exploration or planning only to resolve a concrete uncertainty. Preserve useful independent parallel work and quality checks. Before delegating, state the objective, restrictions, owned paths, acceptance criteria and existing evidence references. Preserve explicit model/effort pins and capability and permission ceilings. Delegate only tasks smaller than the parent objective, within the shared depth, concurrency and budget limits. If admission is refused, execute locally or wait for independent work to finish; never recursively retry or multiply budgets. Workflow-owned, harness-internal, output-budgeted and isolated-worktree agents are leaves. Integrate child results by inspecting their evidence and actual changes, then verify the parent objective; a child report alone is not completion. Report unresolved uncertainty and concrete runtime limits.";

pub(super) const ULTRAFAST_DIRECTIVE: &str = "UltraFast mode is active. Minimize elapsed time to a verified result. Split the work once: find the independent parts and launch all of them in parallel in a single round (subagents run on the worker model), batching independent tool calls in that same round. Do the critical path directly yourself. Inspect the smallest relevant surface, implement, run focused checks first and the full check once at the end, and fix only actual failures. Do not write plans or progress lists, or rerun checks that later edits did not invalidate. Preserve explicit model and effort choices, permissions, repository instructions, required project checks and final integrated verification. Inspect real command output; a summary alone is not proof. After a failure diagnose its cause and change the approach. Stop when acceptance criteria are verified and report concisely, including anything unverified. Before finishing, check every requirement and edge case the task states with a quick test, and fix any that fail.";

pub(super) const ULTRASAVE_DIRECTIVE: &str = "UltraSave mode is active. Minimize money cost first, then total input and output tokens, to a verified result. Execute directly. Delegate to a subagent (runs on the cheaper worker model) only large, mechanical, fully specified work where the worker's re-reads cost less than the main model doing it; do small or judgment work directly. Search narrowly before reading, read only relevant ranges, reuse evidence already in context, and batch independent reads or edits in one round. Do not write plans or progress lists, or repeat checks that later edits did not invalidate. Do not restate source text in a report when a path and check result suffice. Preserve explicit model and effort choices, repository instructions, permissions, exact code and error evidence and required project checks. Never hide failures, truncate required work or omit verification to save tokens. Stop when acceptance criteria are verified and report the result and remaining uncertainty concisely. Before finishing, check every requirement and edge case the task states with a quick test, and fix any that fail.";

impl SessionActor {
    /// Every turn states the authority, including Normal after switching off.
    pub(super) fn inject_execution_mode_reminder(&self) {
        use distill_tool_types::ExecutionMode;
        let directive = match self.execution_mode.load() {
            ExecutionMode::Normal => "Normal execution mode is active. Follow the configured default execution policy.",
            ExecutionMode::Ultrafast => ULTRAFAST_DIRECTIVE,
            ExecutionMode::Ultracode => ULTRACODE_DIRECTIVE,
            ExecutionMode::Ultrasave => ULTRASAVE_DIRECTIVE,
        };
        self.push_system_reminder_with_tag(
            &format!("Current execution mode supersedes all earlier execution mode reminders. {directive}"),
            self.reminder_wrapper_tag(),
        );
    }
}
