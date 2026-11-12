// Modified for Distill by Samuel Fajreldines, 2026.
//! Authoritative per-turn execution mode for `SessionActor`.
use super::*;

pub(super) const ULTRACODE_DIRECTIVE: &str = "Ultracode mode is ON. Prioritize care and correctness for refactoring, substantial logic, subtle bugs and risky changes, even in one file. Understand invariants and the root cause before editing; verify the changed behavior and relevant edge cases. For this objective choose the smallest useful next action: execute directly when the work is understood; investigate a specific uncertainty; or decompose only when bounded smaller tasks can be verified independently. Delegation is optional in this mode, overriding default delegation preferences, with no agent quota. Keep targeted inspection, implementation and checks for a bounded known change with one execution owner; separate exploration or planning only to resolve a concrete uncertainty. Preserve useful independent parallel work and quality checks. Before delegating, state the objective, restrictions, owned paths, acceptance criteria and existing evidence references. Preserve explicit model/effort pins and capability and permission ceilings. Delegate only tasks smaller than the parent objective, within the shared depth, concurrency and budget limits. If admission is refused, execute locally or wait for independent work to finish; never recursively retry or multiply budgets. Workflow-owned, harness-internal, output-budgeted and isolated-worktree agents are leaves. Integrate child results by inspecting their evidence and actual changes, then verify the parent objective; a child report alone is not completion. Report unresolved uncertainty and concrete runtime limits.";

pub(super) const ULTRAFAST_DIRECTIVE: &str = "UltraFast mode is active. Minimize elapsed time to a verified result. For a simple interactive action, such as capturing and showing a browser screenshot, act directly and return the result promptly. Delegate only genuinely independent parts when parallel work saves time after startup and integration costs; never split a single quick action. Batch independent tool calls and do the critical path directly yourself. Inspect the smallest relevant surface, implement, run focused checks first and the full check once at the end, and fix only actual failures. Do not write plans or progress lists, or rerun checks that later edits did not invalidate. Preserve explicit model and effort choices, permissions, repository instructions, required project checks and final integrated verification. Inspect real command output; a summary alone is not proof. After a failure diagnose its cause and change the approach. Stop when acceptance criteria are verified and report concisely, including anything unverified. Before finishing, check every requirement and edge case the task states with a quick test, and fix any that fail.";

pub(super) const ULTRASAVE_DIRECTIVE: &str = "UltraSave mode is active. Minimize money cost first, then total input and output tokens, to a verified result. Execute directly. Delegate to a subagent (runs on the cheaper worker model) only large, mechanical, fully specified work where the worker's re-reads cost less than the main model doing it; do small or judgment work directly. Search narrowly before reading, read only relevant ranges, reuse evidence already in context, and batch independent reads or edits in one round. Do not write plans or progress lists, or repeat checks that later edits did not invalidate. Do not restate source text in a report when a path and check result suffice. Preserve explicit model and effort choices, repository instructions, permissions, exact code and error evidence and required project checks. Never hide failures, truncate required work or omit verification to save tokens. Stop when acceptance criteria are verified and report the result and remaining uncertainty concisely. Before finishing, check every requirement and edge case the task states with a quick test, and fix any that fail.";

// The texts for a mode on its v2 row: the objective, then only what the
// harness enforces on that row or what the model must do because the harness
// cannot. No delegation advice: a mode that may delegate says nothing about it.
pub(super) const ULTRAFAST_V2_DIRECTIVE: &str = "UltraFast mode is active: reach a verified result in the least elapsed time. Automatic reasoning effort is set once per turn, at medium or lower. Batch independent tool calls and write no plans or progress lists. Run focused checks while working and the full check once at the end; after a failure, find its cause and change the approach. Before finishing, confirm each stated requirement with a quick test and name anything left unverified.";

pub(super) const ULTRASAVE_V2_DIRECTIVE: &str = "UltraSave mode is active: reach a verified result at the lowest money cost. Automatic reasoning effort is chosen per round, at high or lower. Reuse evidence already in context instead of reading or running it again, batch independent reads and edits into one round, and report paths and check results rather than quoted source. Never drop required work or verification to save tokens. Before finishing, test every requirement the task states and fix what fails.";

pub(super) const ULTRACODE_V2_DIRECTIVE: &str = "UltraCode mode is active: make the change correct first, ahead of speed and cost. Understand the invariants and the root cause before editing, and verify the changed behavior and its edge cases with checks you run. Automatic reasoning effort is never lowered for a routine round and is at least high after a failed check or tool. At the end of a turn that changed code, the harness reviews the changes independently, returns refuted gaps for you to fix, and appends the verification status to your final message.";

/// The turn-end review runs in root sessions only, so a subagent's v2
/// directive leaves out the sentence that promises it.
pub(super) const ULTRACODE_V2_REVIEW_SENTENCE: &str = "At the end of a turn that changed code, the harness reviews the changes independently, returns refuted gaps for you to fix, and appends the verification status to your final message.";

/// The directive `mode` states on its v2 row; Normal has one text on both rows.
pub(super) fn v2_directive(mode: distill_tool_types::ExecutionMode, is_subagent: bool) -> String {
    use distill_tool_types::ExecutionMode;
    match mode {
        ExecutionMode::Ultracode if is_subagent => {
            ULTRACODE_V2_DIRECTIVE.replace(&format!(" {ULTRACODE_V2_REVIEW_SENTENCE}"), "")
        }
        ExecutionMode::Ultracode => ULTRACODE_V2_DIRECTIVE.to_owned(),
        ExecutionMode::Ultrafast => ULTRAFAST_V2_DIRECTIVE.to_owned(),
        ExecutionMode::Ultrasave => ULTRASAVE_V2_DIRECTIVE.to_owned(),
        ExecutionMode::Normal => NORMAL_DIRECTIVE.to_owned(),
    }
}

const NORMAL_DIRECTIVE: &str =
    "Normal execution mode is active. Follow the configured default execution policy.";

/// The UltraCode sentences that only matter to a session that can delegate.
pub(super) const ULTRACODE_DELEGATION_SENTENCES: [&str; 7] = [
    "Delegation is optional in this mode, overriding default delegation preferences, with no agent quota.",
    "Keep targeted inspection, implementation and checks for a bounded known change with one execution owner; separate exploration or planning only to resolve a concrete uncertainty.",
    "Before delegating, state the objective, restrictions, owned paths, acceptance criteria and existing evidence references.",
    "Delegate only tasks smaller than the parent objective, within the shared depth, concurrency and budget limits.",
    "If admission is refused, execute locally or wait for independent work to finish; never recursively retry or multiply budgets.",
    "Workflow-owned, harness-internal, output-budgeted and isolated-worktree agents are leaves.",
    "Integrate child results by inspecting their evidence and actual changes, then verify the parent objective; a child report alone is not completion.",
];

/// [`ULTRACODE_DIRECTIVE`] for a session that cannot spawn: delegation advice
/// there is noise the child pays for on every turn.
pub(super) fn ultracode_leaf_directive() -> String {
    ULTRACODE_DELEGATION_SENTENCES
        .iter()
        .fold(ULTRACODE_DIRECTIVE.to_owned(), |text, sentence| {
            text.replace(&format!(" {sentence}"), "")
        })
}

impl SessionActor {
    /// Whether this session can spawn a child now: it has the spawn tool and is
    /// below the mode's depth ceiling. A child without the delegation policy
    /// (workflow and other leaf agents) cannot.
    fn can_delegate(&self) -> bool {
        let has_spawn_tool = self
            .agent
            .borrow()
            .tool_bridge()
            .toolset()
            .tool_name_for_kind(distill_tools::types::tool::ToolKind::Task)
            .is_some();
        let below_ceiling = match &self.tool_context.execution_policy {
            Some(policy) => self.tool_context.subagent_depth < policy.effective_max_depth(),
            None => !self.startup_hints.is_subagent,
        };
        has_spawn_tool && below_ceiling
    }

    /// States the authority for the current mode, including Normal after
    /// switching off. A mid-turn mode change always calls this.
    pub(super) fn inject_execution_mode_reminder(&self) {
        self.chat_state_handle
            .push_user_message(self.execution_mode_reminder());
    }

    /// Turn start: states the mode unless the live history already does, so
    /// an unchanged mode is not repeated every turn. Decided from the history
    /// itself, never a remembered flag, so rewind, fork and resume stay right.
    pub(super) async fn inject_execution_mode_reminder_at_turn_start(&self) {
        let reminder = self.execution_mode_reminder();
        let conversation = self.chat_state_handle.get_conversation().await;
        if !execution_mode_already_stated(&conversation, &reminder) {
            self.chat_state_handle.push_user_message(reminder);
        }
    }

    fn execution_mode_reminder(&self) -> ConversationItem {
        use distill_tool_types::ExecutionMode;
        let owned;
        let directive = match self.execution_mode.load() {
            ExecutionMode::Normal => NORMAL_DIRECTIVE,
            // A mode on its v2 row states what that row enforces.
            mode if super::mode_policy::v2_modes().contains(mode) => {
                owned = v2_directive(mode, self.startup_hints.is_subagent);
                owned.as_str()
            }
            ExecutionMode::Ultrafast => ULTRAFAST_DIRECTIVE,
            ExecutionMode::Ultracode if self.can_delegate() => ULTRACODE_DIRECTIVE,
            ExecutionMode::Ultracode => {
                owned = ultracode_leaf_directive();
                owned.as_str()
            }
            ExecutionMode::Ultrasave => ULTRASAVE_DIRECTIVE,
        };
        super::reminders::wrap_in_reminder_tag(
            &format!("{DIRECTIVE_LEAD} {directive}"),
            self.reminder_wrapper_tag(),
        )
    }
}

const DIRECTIVE_LEAD: &str = "Current execution mode supersedes all earlier execution mode reminders.";

/// Whether the latest execution-mode directive after the last compaction
/// output is `reminder`. Compaction drops earlier directives, and a later
/// directive for another mode supersedes an older copy of this one.
fn execution_mode_already_stated(conversation: &[ConversationItem], reminder: &ConversationItem) -> bool {
    use distill_sampling_types::SyntheticReason;
    let is_user = |item: &ConversationItem, reason: SyntheticReason| {
        matches!(item, ConversationItem::User(user) if user.synthetic_reason == reason)
    };
    let after_compaction = conversation
        .iter()
        .rposition(|item| is_user(item, SyntheticReason::CompactionMeta))
        .map_or(0, |index| index + 1);
    conversation
        .get(after_compaction..)
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|item| {
            is_user(item, SyntheticReason::SystemReminder) && item.text_content().contains(DIRECTIVE_LEAD)
        })
        .is_some_and(|latest| latest.text_content() == reminder.text_content())
}
