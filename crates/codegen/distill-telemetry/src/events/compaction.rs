// Modified for Distill by Samuel Fajreldines, 2026.
//! Compaction product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto,
}

/// Mixpanel mode label. Detail is omitted so `segments` never includes it.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionModeLabel {
    Summary,
    Transcript,
    Segments,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TwoPassOutcome {
    /// Policy or product-exception off (cursor, subagents).
    Disabled,
    /// Enabled, fell back to single-pass.
    SinglePass,
    /// Pass-2 summary applied.
    TwoPass,
}

#[derive(Serialize)]
pub struct AutoCompactFired {
    pub tokens_before: u64,
    pub percentage: u8,
}

#[derive(Serialize)]
pub struct CompactionTriggered {
    pub trigger: CompactionTrigger,
    pub tokens_used: u64,
    pub context_window: u64,
    pub percentage: u8,
    pub model_id: String,
    pub user_context_provided: bool,
    pub compaction_id: String,
    pub compaction_mode: CompactionModeLabel,
    pub two_pass_enabled: bool,
    pub is_subagent: bool,
}

#[derive(Serialize)]
pub struct CompactionCompleted {
    pub duration_ms: u64,
    pub tokens_before: u64,
    pub tokens_after: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub compaction_id: String,
    pub compaction_mode: CompactionModeLabel,
    pub two_pass: TwoPassOutcome,
    pub segments_queued: u32,
    pub degenerate_retries: u32,
    pub input_overflow_retries: u32,
    pub is_subagent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_compaction_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_compaction_ms: Option<u64>,
}

pub struct CompactionBeginParams {
    pub trigger: CompactionTrigger,
    pub tokens_used: u64,
    pub context_window: u64,
    pub model_id: String,
    pub user_context_provided: bool,
    pub compaction_mode: CompactionModeLabel,
    pub two_pass_enabled: bool,
    pub is_subagent: bool,
}

pub struct CompactionCompleteStats {
    pub tokens_after: u64,
    pub two_pass_used: bool,
    pub segments_queued: u32,
    pub degenerate_retries: u32,
    pub input_overflow_retries: u32,
}

#[derive(Clone, Copy)]
pub struct CompactionTiming {
    pub model_wait_ms: Option<u64>,
    pub pre_compaction_ms: Option<u64>,
    pub post_compaction_ms: Option<u64>,
}

/// Emits `compaction_triggered` on `begin` and `compaction_completed` on `complete`, correlated by a shared `compaction_id`.
/// A scope dropped without `complete` (error or cancel) emits no completion.
pub struct CompactionScope {
    pub compaction_id: String,
    pub tokens_before: u64,
    pub model_id: String,
    start: std::time::Instant,
    _active: crate::activity::ActivityGaugeGuard,
    compaction_mode: CompactionModeLabel,
    two_pass_enabled: bool,
    is_subagent: bool,
}

impl CompactionScope {
    pub fn begin(params: CompactionBeginParams) -> Self {
        let CompactionBeginParams {
            trigger,
            tokens_used,
            context_window,
            model_id,
            user_context_provided,
            compaction_mode,
            two_pass_enabled,
            is_subagent,
        } = params;
        let compaction_id = uuid::Uuid::new_v4().to_string();
        let percentage = distill_token_estimation::usage_percentage_u8(tokens_used, context_window);
        let active = crate::activity::COMPACTIONS_ACTIVE.enter();
        debug_assert!(
            crate::activity::COMPACTIONS_ACTIVE.get() >= 1,
            "CompactionTriggered must stamp a self-inclusive count"
        );
        crate::session_ctx::log_event(CompactionTriggered {
            trigger,
            tokens_used,
            context_window,
            percentage,
            model_id: model_id.clone(),
            user_context_provided,
            compaction_id: compaction_id.clone(),
            compaction_mode,
            two_pass_enabled,
            is_subagent,
        });
        Self {
            compaction_id,
            tokens_before: tokens_used,
            model_id,
            start: std::time::Instant::now(),
            _active: active,
            compaction_mode,
            two_pass_enabled,
            is_subagent,
        }
    }

    pub fn complete(self, stats: CompactionCompleteStats, timing: CompactionTiming) {
        let two_pass = match (self.two_pass_enabled, stats.two_pass_used) {
            (false, _) => TwoPassOutcome::Disabled,
            (true, true) => TwoPassOutcome::TwoPass,
            (true, false) => TwoPassOutcome::SinglePass,
        };
        crate::session_ctx::log_event(CompactionCompleted {
            duration_ms: self.start.elapsed().as_millis() as u64,
            tokens_before: self.tokens_before,
            tokens_after: stats.tokens_after,
            model_id: Some(self.model_id),
            compaction_id: self.compaction_id,
            compaction_mode: self.compaction_mode,
            two_pass,
            segments_queued: stats.segments_queued,
            degenerate_retries: stats.degenerate_retries,
            input_overflow_retries: stats.input_overflow_retries,
            is_subagent: self.is_subagent,
            model_wait_ms: timing.model_wait_ms,
            pre_compaction_ms: timing.pre_compaction_ms,
            post_compaction_ms: timing.post_compaction_ms,
        });
    }
}

/// Auto-compaction suppressed after a deterministic failure so the turn loop stops re-firing a doomed compaction.
/// Fires once per transition into the suppressed state; `reason` is a fixed classification: `credit_block | size | auth | schema | other`.
#[derive(Serialize)]
pub struct AutoCompactSuppressed {
    pub reason: &'static str,
    pub estimated_tokens: u64,
    pub context_window: u64,
}

#[derive(Serialize)]
pub struct CompactionRetryDegraded {
    pub trigger: CompactionTrigger,
    pub reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_stage: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_chars: Option<u64>,
    pub attempt: u32,
    pub context_window: u64,
    pub compaction_id: String,
}
