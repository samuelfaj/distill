//! Memory subsystem telemetry routes through `log_event` (product tier, `Enabled` mode only).
//! Events carry no PII or user content, only counts, scores, durations, and config values.

use serde::Serialize;

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMode {
    #[default]
    Legacy,
    V2,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Rollout {
    Off,
    RecordOnly,
    Shadow,
    #[default]
    Active,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2CaptureStage {
    #[default]
    Queued,
    Claimed,
    Completed,
    Noop,
    Retry,
    Failed,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2FailureClass {
    #[default]
    Disabled,
    Storage,
    Lease,
    Model,
    MalformedOutput,
    EmptyOutput,
    Timeout,
    Convergence,
    AccessPolicy,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2DreamDisposition {
    Ineligible,
    Ready,
    Coalesced,
    Busy,
    Noop,
    Shadow,
    Committed,
    Reconciled,
    Retry,
    #[default]
    Failed,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2FlushOutcome {
    #[default]
    Success,
    RetryableFailure,
    TerminalFailure,
    Timeout,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2TargetKind {
    #[default]
    Observation,
    Topic,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Component {
    #[default]
    Capture,
    Flush,
    Dream,
    GarbageCollection,
    Forget,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2ControlsPinned {
    pub rollout: MemoryV2Rollout,
    pub capture_enabled: bool,
    pub automatic_dream_enabled: bool,
    pub manual_dream_enabled: bool,
    pub file_writes_enabled: bool,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct MemoryV2ModelUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// USD ticks (1e10 ticks = $1); `None` when unpriced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2CaptureLifecycle {
    pub stage: MemoryV2CaptureStage,
    pub from_turn: u32,
    pub through_turn: u32,
    pub attempt: u32,
    pub observation_count: usize,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
    #[serde(flatten)]
    pub usage: MemoryV2ModelUsage,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2FlushResult {
    pub outcome: MemoryV2FlushOutcome,
    pub target_cursor: u32,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2DreamLifecycle {
    pub disposition: MemoryV2DreamDisposition,
    pub observation_count: usize,
    pub topic_change_count: usize,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
    #[serde(flatten)]
    pub usage: MemoryV2ModelUsage,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2CarryoverOutcome {
    #[default]
    Imported,
    Failed,
}

/// One legacy `MEMORY.md` carried into a v2 scope at session start. Counts
/// only; no-op starts (no source, unchanged source) emit nothing.
#[derive(Debug, Default, Serialize)]
pub struct MemoryV2CarryoverCompleted {
    pub scope: MemoryV2Scope,
    pub outcome: MemoryV2CarryoverOutcome,
    pub topics_created: u32,
    pub topics_appended: u32,
    pub sections_skipped: u32,
    pub bytes_written: u64,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Scope {
    #[default]
    Global,
    Workspace,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2GcCompleted {
    pub archived_observations_removed: u64,
    pub terminal_jobs_removed: u64,
}

#[derive(Debug, Default, Serialize)]
/// Content-free telemetry boundary for the deferred memory-v2 forget UX.
///
/// A future shell caller can map `ForgetResult::was_already_forgotten` and the
/// post-operation tombstone count into this event without paths, hashes,
/// reasons, or forgotten content.
pub struct MemoryV2Forgotten {
    pub target_kind: MemoryV2TargetKind,
    pub was_already_forgotten: bool,
    pub tombstone_count: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2FailClosed {
    pub component: MemoryV2Component,
    pub reason: MemoryV2FailureClass,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchSource {
    #[default]
    Tool,
    Injection,
    CompactionRecovery,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchMode {
    #[default]
    FtsOnly,
    Hybrid,
    EmbeddingFallback,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchOutcome {
    #[default]
    Results,
    Empty,
    Error,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchErrorClass {
    IndexOpen,
    Fts,
    Vector,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryInjectionOutcome {
    #[default]
    Results,
    Empty,
    Error,
    Skipped,
}

#[derive(Default, Serialize)]
pub struct MemorySessionInit {
    pub session_id: String,
    pub memory_enabled: bool,
    pub memory_mode: MemoryMode,
    pub watcher_config_enabled: bool,
    pub watcher_started: bool,
    pub temporal_decay_enabled: bool,
    pub mmr_enabled: bool,
    pub mmr_lambda: f64,
    pub half_life_days: f64,
    pub embedding_dimensions: usize,
    pub total_chunks: usize,
    pub total_files: usize,
    pub has_global_memory_md: bool,
    pub has_workspace_memory_md: bool,
}

#[derive(Default, Serialize)]
pub struct MemorySearch {
    pub session_id: String,
    pub source: MemorySearchSource,
    #[serde(rename = "search_mode")]
    pub mode: MemorySearchMode,
    pub outcome: MemorySearchOutcome,
    pub query_length: usize,
    pub keyword_count: usize,
    pub result_count: usize,
    pub top_score: f64,
    pub min_score_threshold: f64,
    pub duration_ms: u64,
    pub vec_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<MemorySearchErrorClass>,
}

#[derive(Default, Serialize)]
pub struct MemoryFlushStart {
    pub session_id: String,
    pub trigger: String,
    pub conversation_len: usize,
    pub user_message_count: usize,
}

#[derive(Default, Serialize)]
pub struct MemoryFlushComplete {
    pub session_id: String,
    pub trigger: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub response_length: usize,
    pub accepted_length: usize,
    pub was_truncated: bool,
}

#[derive(Default, Serialize)]
pub struct MemoryInjection {
    pub session_id: String,
    pub outcome: MemoryInjectionOutcome,
    pub was_greeting_fallback: bool,
    pub result_count: usize,
    pub total_snippet_chars: usize,
    pub top_score: f64,
    pub configured_min_score: f64,
    pub injection_duration_ms: u64,
    pub injected_bytes: u64,
    pub estimated_tokens: u64,
    pub global_entry_count: usize,
    pub workspace_entry_count: usize,
    pub was_reused: bool,
}

#[derive(Default, Serialize)]
pub struct MemoryReindex {
    pub session_id: String,
    pub source: String,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub embedded: usize,
    pub duration_ms: u64,
    pub trigger: String,
}

#[derive(Default, Serialize)]
pub struct MemoryWatcherSync {
    pub session_id: String,
    pub dirty_file_count: usize,
    pub claimed: bool,
    pub reindexed_count: usize,
    pub embedded_count: usize,
    pub duration_ms: u64,
}

#[derive(Default, Serialize)]
pub struct MemorySessionSummary {
    pub session_id: String,
    pub memory_enabled: bool,
    pub memory_mode: MemoryMode,
    pub session_duration_secs: u64,
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub recovery_search_count: u64,
    pub total_chunks_at_end: usize,
    pub chunks_added_this_session: usize,
    pub session_end_result: String,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
    pub capture_prompt_tokens: u64,
    pub capture_completion_tokens: u64,
    pub capture_cost_usd_ticks: u64,
    pub dream_prompt_tokens: u64,
    pub dream_completion_tokens: u64,
    pub dream_cost_usd_ticks: u64,
    pub injected_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_events_have_no_content_bearing_fields() {
        let search_event = MemorySearch {
            source: MemorySearchSource::CompactionRecovery,
            mode: MemorySearchMode::EmbeddingFallback,
            outcome: MemorySearchOutcome::Error,
            error_class: Some(MemorySearchErrorClass::Vector),
            ..Default::default()
        };
        let search = serde_json::to_value(search_event).unwrap();
        assert_eq!(
            search.get("source").and_then(|v| v.as_str()),
            Some("compaction_recovery")
        );
        assert_eq!(
            search.get("search_mode").and_then(|v| v.as_str()),
            Some("embedding_fallback")
        );
        assert_eq!(
            search.get("outcome").and_then(|v| v.as_str()),
            Some("error")
        );
        assert_eq!(
            serde_json::to_value(MemoryInjectionOutcome::Skipped).unwrap(),
            "skipped"
        );
        assert_eq!(serde_json::to_value(MemoryMode::V2).unwrap(), "v2");
        let events = [
            serde_json::to_value(MemorySessionInit::default()).unwrap(),
            search,
            serde_json::to_value(MemoryFlushStart::default()).unwrap(),
            serde_json::to_value(MemoryFlushComplete::default()).unwrap(),
            serde_json::to_value(MemoryInjection {
                injected_bytes: 2048,
                estimated_tokens: 512,
                global_entry_count: 3,
                workspace_entry_count: 8,
                was_reused: true,
                ..Default::default()
            })
            .unwrap(),
            serde_json::to_value(MemoryReindex::default()).unwrap(),
            serde_json::to_value(MemoryWatcherSync::default()).unwrap(),
            serde_json::to_value(MemorySessionSummary::default()).unwrap(),
            serde_json::to_value(MemoryV2ControlsPinned::default()).unwrap(),
            serde_json::to_value(MemoryV2CaptureLifecycle {
                usage: MemoryV2ModelUsage {
                    model_id: Some("grok-4".to_owned()),
                    prompt_tokens: Some(100),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap(),
            serde_json::to_value(MemoryV2FlushResult::default()).unwrap(),
            serde_json::to_value(MemoryV2DreamLifecycle::default()).unwrap(),
            serde_json::to_value(MemoryV2GcCompleted::default()).unwrap(),
            serde_json::to_value(MemoryV2Forgotten::default()).unwrap(),
            serde_json::to_value(MemoryV2FailClosed::default()).unwrap(),
        ];
        const STRING_KEYS: &[&str] = &[
            "session_id",
            "memory_mode",
            "source",
            "search_mode",
            "outcome",
            "error_class",
            "trigger",
            "session_end_result",
            "rollout",
            "stage",
            "failure_class",
            "disposition",
            "target_kind",
            "component",
            "reason",
            "model_id",
        ];
        for event in events {
            for (key, value) in event.as_object().unwrap() {
                let is_aggregate_field = key.ends_with("_length")
                    || key.ends_with("_count")
                    || key.ends_with("_chars")
                    || key.ends_with("_tokens")
                    || key.ends_with("_ticks")
                    || key.ends_with("_bytes");
                let content_key = !is_aggregate_field
                    && ["query", "snippet", "path", "content", "message"]
                        .iter()
                        .any(|word| key.contains(word));
                assert!(
                    !value.is_object()
                        && !value.is_array()
                        && (!value.is_string() || STRING_KEYS.contains(&key.as_str()))
                        && !content_key,
                    "unsafe memory field {key}"
                );
            }
        }
    }
}
