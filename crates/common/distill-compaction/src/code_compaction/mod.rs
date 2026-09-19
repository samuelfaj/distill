// Modified for Distill by Samuel Fajreldines, 2026.
//! distill's "code agent" compaction subsystem.
//!
//! distill does not select a tail to keep; it summarizes the whole
//! conversation and rebuilds a fresh history from scratch (the *full-replace*
//! strategy). This module groups that subsystem — generic over the engine's
//! [`CompactionItem`](crate::item::CompactionItem) /
//! [`CompactionItemFactory`](crate::item::CompactionItemFactory) seams — so it
//! can be reused as a unit by distill, separate from Grok chat's
//! [`intra_compaction`](crate::intra_compaction) (tail-keep, per-step) and
//! [`inter_compaction`](crate::inter_compaction) (chunked, between-turn).
//!
//! Layout (mirroring
//! [`intra_compaction`](crate::intra_compaction) /
//! [`inter_compaction`](crate::inter_compaction)):
//!
//! - **Policy & content**: [`prompt`] (summarization prompt), [`summary`]
//!   (summary cleaning + carrier), [`failure`] (deterministic-vs-transient
//!   classification), [`config`] (tunables + trigger/seed defaults).
//! - **Algorithm**: [`assemble`] (full-replace history rebuild).
//! - **Orchestration**: [`compact`]
//!   (`build prompt → sample → clean → assemble`).
//!
//! Host-specific concerns (triggers, transport, persistence/replay, state
//! commit, metrics observer) stay in the product host (for example
//! `distill-shell`).

pub mod assemble;
pub mod compact;
pub mod config;
pub mod failure;
pub mod observer;
pub mod prompt;
pub mod sample;
pub mod summary;

pub use assemble::{CompactedHistoryParts, assemble_compacted_history};
pub use compact::{
    FullReplaceContext, FullReplaceError, FullReplaceOutput, FullReplaceSummary,
    apply_full_replace_compaction, sample_full_replace_summary,
};
pub use config::{
    DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT, FullReplaceConfig, MIN_SUMMARY_SEED_CHARS,
};
pub use failure::is_context_length_error;
pub use observer::{FullReplaceAttemptOutcome, FullReplaceObserver};
pub use prompt::{
    SELF_SUMMARIZATION_PROMPT, SummaryPromptKind, build_summary_prompt, build_summary_prompt_kind,
};
pub use sample::{SampleRetryError, SampledSummary, sample_summary_with_retries};
pub use summary::{
    format_compact_summary, format_compact_summary_content, is_degenerate_summary, wrap_user_query,
};
