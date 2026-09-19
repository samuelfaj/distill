// Modified for Distill by Samuel Fajreldines, 2026.
//! Resume admission for a copied child transcript.
//!
//! Spawn injects the system prompt, AGENTS.md, and the resume task before the
//! first compact can run, so a copy that fills the window is not safe.
//! Budgeted children skip `check_auto_compact_needed`, so a fat transcript
//! that needs compact must abort instead of arming a no-op flag.

use std::sync::atomic::{AtomicBool, Ordering};

/// Spawn injects prelude tokens before the first compact can shrink history.
const RESUME_WINDOW_FIT_PERCENT: u8 = 95;

/// Child context window and auto-compact threshold for resume admission.
pub(super) struct ResumeWindowPolicy {
    pub context_window: u64,
    pub auto_compact_threshold_percent: u8,
}

impl ResumeWindowPolicy {
    pub(super) fn token_limit(&self) -> u64 {
        // Transcript estimate omits spawn prelude; never admit above the auto-compact threshold.
        let fit_percent =
            u64::from(RESUME_WINDOW_FIT_PERCENT.min(self.auto_compact_threshold_percent));
        self.context_window * fit_percent / 100
    }

    pub(super) fn fits(&self, estimated_tokens: u64) -> bool {
        self.context_window != 0 && estimated_tokens <= self.token_limit()
    }

    fn over_auto_compact_threshold(&self, estimated_tokens: u64) -> bool {
        distill_token_estimation::exceeds_threshold(
            estimated_tokens,
            self.context_window,
            self.auto_compact_threshold_percent,
        )
    }

    /// First-turn compact plan for a resumed transcript that already fits the window.
    pub(super) fn plan_force_compact(
        &self,
        estimated_tokens: u64,
        has_task_output_budget: bool,
    ) -> ResumeForceCompact {
        if !self.over_auto_compact_threshold(estimated_tokens) {
            return ResumeForceCompact::NotNeeded;
        }
        if has_task_output_budget {
            return ResumeForceCompact::AbortBudgeted;
        }
        ResumeForceCompact::Arm
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResumeForceCompact {
    NotNeeded,
    Arm,
    /// Over the auto-compact threshold, but a budgeted child never runs
    /// `check_auto_compact_needed`, so arming `force_compact` would no-op.
    AbortBudgeted,
}

pub(super) fn arm_force_compact(force_compact: &AtomicBool, should_force: bool) {
    if should_force {
        force_compact.store(true, Ordering::Relaxed);
    }
}

/// Leading items to preserve across compaction on resume: the System head only, so the resumed body (the child's own work) stays compactable.
/// Returns 0 when there's no leading System; the spawn path then inserts one and bumps the prefix to 1.
pub(super) fn resume_inherited_prefix_len(
    conversation: &[distill_sampling_types::conversation::ConversationItem],
) -> usize {
    use distill_sampling_types::conversation::ConversationItem;

    conversation
        .iter()
        .take_while(|item| matches!(item, ConversationItem::System(_)))
        .count()
}

#[cfg(test)]
#[path = "resume_window_tests.rs"]
mod tests;
