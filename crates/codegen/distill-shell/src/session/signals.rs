// Modified for Distill by Samuel Fajreldines, 2026.
//! Session signals tracking for feedback heuristics.
//!
//! Signals are collected locally in the agent and periodically synced to the backend for analytics / telemetry persistence.
//!
//! Uses a channel-based actor pattern to avoid locks.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tdigests::TDigest;
use tokio::sync::{mpsc, oneshot};

use super::doom_loop_telemetry::merge_tightest_trigger;
use super::inference_metrics::{InferenceLatencyStats, compute_percentiles};

/// Sample the process resident-set high-water mark in bytes.
/// Uses `getrusage(RUSAGE_SELF)` on Unix; returns 0 if sampling fails or on non-Unix targets.
/// Cheap enough to call once per turn.
pub(crate) fn sample_rss_bytes() -> u64 {
    #[cfg(unix)]
    {
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
                let rss = (usage.ru_maxrss).max(0) as u64;
                #[cfg(target_os = "linux")]
                {
                    rss * 1024 // Linux reports in kB
                }
                #[cfg(not(target_os = "linux"))]
                {
                    // macOS (our only non-Linux Unix target) reports bytes
                    // Other BSDs report kB like Linux; revisit the unit if we ever port
                    rss
                }
            } else {
                0
            }
        }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Per-tool success/failure breakdown for a single turn.
///
/// Serialized as part of `SessionSignalsDelta` and synced to backend analytics for per-tool-name stats (e.g. "bash fails 10% of the time").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutcome {
    /// Tool name (e.g. "bash", "read_file", "search_replace")
    pub tool_name: String,
    pub successes: u32,
    pub failures: u32,
}

/// Per-tool execution duration for a single invocation.
/// Written into `turn_result.json` via `SessionSignalsDelta.tool_durations_this_turn`.
/// Downstream analytics join wall time to a specific tool call through it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDuration {
    /// Tool name (e.g. "bash", "read_file", "search_replace")
    pub tool_name: String,
    /// Join key (`missing-call-id-{batch_idx}` if the model omitted one). Empty on legacy packages.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_call_id: String,
    /// Dispatch wall-clock ms (start to result ready), including lock wait and in-dispatch auth retries.
    /// A managed-MCP reauth retry adds its second attempt here; the reauth handshake wait itself is excluded.
    pub duration_ms: u64,
}

/// How a PR creation was performed (shared with the `pr_created` telemetry event so signal and event values can never diverge).
pub use distill_telemetry::enums::PrCreationSource;

/// A PR created during a turn, recorded for PR metrics.
///
/// Serialized as part of `SessionSignalsDelta` into `turn_result.json` so backend analytics can attribute PR creation to the turn's model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PrCreatedSignal {
    /// Full PR URL when parsed from the create output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// PR number when parsed from the create output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    pub source: PrCreationSource,
    /// Whether the session recorded a `git commit` by the end of the turn that created the PR.
    /// Reconciled at `TakeTurnEndSnapshot`, so parallel tool-result ordering cannot mis-attribute.
    /// Distinguishes end-to-end PRs from ones whose work started elsewhere.
    pub had_commit_in_session: bool,
}

/// Snapshot produced at turn end, containing the delta from the previous turn and the current cumulative signals.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDeltaSnapshot {
    /// Current signal values at turn end (cumulative)
    pub current: SessionSignals,
    /// Per-turn deltas (difference from last turn-end snapshot)
    pub delta: SessionSignalsDelta,
    /// Prompt mode captured at the start of this turn.
    /// Populated by the session actor after taking the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_prompt_mode: Option<String>,
    /// Effective prompt mode when the turn ended.
    /// Populated by the session actor after taking the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_prompt_mode: Option<String>,
    /// Per-turn summed input tokens (all model calls).
    /// Stamped from `TurnSpanTotals` post-snapshot; internal transport, not serialized.
    #[serde(skip)]
    pub turn_input_tokens: u64,
    /// Per-turn summed output tokens (incl. reasoning). See `turn_input_tokens`.
    #[serde(skip)]
    pub turn_output_tokens: u64,
    /// Per-turn summed cached input tokens. See `turn_input_tokens`.
    #[serde(skip)]
    pub turn_cached_input_tokens: u64,
    /// Last served model fingerprint (upstream `system_fingerprint`). See `turn_input_tokens`.
    #[serde(skip)]
    pub model_fingerprint: Option<String>,
}

/// Per-turn delta of counter fields.
///
/// All values represent the change since the previous call to `TakeTurnEndSnapshot` (or since session start for the first turn).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSignalsDelta {
    /// Turn number at the time of this snapshot (1-based)
    pub turn_number: u32,
    pub delta_tool_calls: i64,
    pub delta_tool_failures: i64,
    pub delta_errors: i64,
    pub delta_cancellations: i64,
    pub delta_regenerations: i64,
    pub delta_compactions: i64,
    pub delta_edit_and_retries: i64,
    pub delta_positive_ratings: i64,
    pub delta_negative_ratings: i64,
    pub delta_assistant_messages: i64,
    pub delta_long_pauses: i64,
    pub delta_successful_tool_uses: i64,
    /// Consecutive cancellations at turn end (snapshot, not delta; resets on turn complete)
    pub consecutive_cancellations: u32,
    /// Error type strings that occurred during this turn (e.g. "timeout", "rate_limit", "tool_error")
    pub error_types_this_turn: Vec<String>,
    /// Tools called during this turn (deduplicated, sorted, capped at 100)
    pub tools_this_turn: Vec<String>,
    /// `true` when `tools_this_turn` was truncated (> 100 unique entries)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tools_this_turn_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_outcomes_this_turn: Vec<ToolOutcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_durations_this_turn: Vec<ToolDuration>,
    /// Latency for the most recent response in this turn (if recorded)
    pub last_time_to_first_token_ms: Option<u64>,
    /// Total response time for the most recent response in this turn
    pub last_total_response_time_ms: Option<u64>,
    /// ITL p50 for the most recent response in this turn
    pub last_itl_p50_ms: Option<u64>,
    /// ITL p99 for the most recent response in this turn
    pub last_itl_p99_ms: Option<u64>,
    /// ITL max for the most recent response in this turn
    pub last_itl_max_ms: Option<u64>,
    /// ITL mean for the most recent response in this turn
    pub last_itl_mean_ms: Option<u64>,
    /// Number of response (completion - reasoning) tokens generated this turn.
    /// `None` when no token usage was reported (e.g. old client or no inference).
    pub response_tokens: Option<u32>,
    /// Number of thinking (reasoning) tokens generated this turn.
    /// `None` when no token usage was reported (e.g. old client or no inference).
    pub thinking_tokens: Option<u32>,

    // === LOC Attribution Deltas ===
    pub delta_agent_lines_added: i64,
    pub delta_agent_lines_removed: i64,
    pub delta_agent_lines_added_reverted: i64,
    pub delta_agent_lines_removed_reverted: i64,
    pub delta_human_lines_added: i64,
    pub delta_human_lines_removed: i64,
    pub delta_human_lines_added_reverted: i64,
    pub delta_human_lines_removed_reverted: i64,
    pub delta_agent_files_touched: i64,
    pub delta_human_files_touched: i64,
    pub delta_total_files_touched: i64,

    // === Git/PR Metric Deltas ===
    pub delta_git_commits: i64,
    pub delta_prs_created: i64,
    pub delta_prs_merged: i64,
    /// PRs created during this turn (url/number/source/attribution).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prs_created_this_turn: Vec<PrCreatedSignal>,
}

/// Session signals that inform feedback request heuristics.
/// These signals are tracked locally in the agent and periodically synced to the backend for analytics / telemetry persistence.
/// Field names are aligned with the backend analytics schema for session signals.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionSignals {
    // === Turn/Message Counts ===
    /// Number of user prompts/turns in this session
    pub turn_count: u32,
    pub user_message_count: u32,
    pub assistant_message_count: u32,

    // === Error/Failure Counts ===
    /// Number of errors encountered (general errors including sampling)
    pub error_count: u32,
    /// Number of tool failures (subset of errors, specific to tools)
    pub tool_failure_count: u32,

    // === User Behavior Signals ===
    /// Number of user cancellations (Ctrl+C during agent work)
    pub cancellation_count: u32,
    /// Number of consecutive cancellations (resets when a turn completes)
    pub consecutive_cancellations: u32,
    /// Number of regenerations (user asked to redo last response)
    pub regeneration_count: u32,
    /// Whether the user has reverted any changes
    #[serde(default)]
    pub has_reverted: bool,

    // === Context/Compaction ===
    pub compaction_count: u32,
    /// Cumulative total tokens across all compactions (sum of tokens_before each compaction)
    pub total_tokens_before_compaction: u64,
    /// Current context window usage as percentage (0-100)
    pub context_window_usage: u8,
    /// Raw tokens currently used in the active context window
    pub context_tokens_used: u64,
    /// Raw model context window token limit
    pub context_window_tokens: u64,

    // === Tool Usage ===
    pub tool_call_count: u32,
    /// Distinct tools that have been used in this session
    #[serde(default)]
    pub tools_used: Vec<String>,

    // === Model Usage ===
    /// Distinct models that have been used in this session
    #[serde(default)]
    pub models_used: Vec<String>,
    /// Primary model ID (the most recently used or initially set model)
    #[serde(default)]
    pub primary_model_id: Option<String>,
    /// Model selected for the currently dispatched round. This is a live UI
    /// hint, not durable session metadata; it is intentionally omitted from
    /// persisted signal snapshots.
    #[serde(skip)]
    pub active_model_id: Option<String>,
    /// Effort selected for the currently dispatched round. See
    /// `active_model_id` for the lifetime and persistence contract.
    #[serde(skip)]
    pub active_reasoning_effort: Option<String>,

    // === Edit & Retry ===
    /// Number of edit-and-retry actions (user rewinds and submits a different prompt)
    pub edit_and_retry_count: u32,

    // === Bash tool patterns (distill) ===
    /// Number of times the bash tool was used for a bare `echo "<msg>"` (or close variant).
    #[serde(default)]
    pub bash_bare_echo_count: u32,

    // === Git/PR Metrics ===
    /// Number of successful `git commit` statements observed in bash tool calls.
    #[serde(default)]
    pub git_commit_count: u32,
    /// Number of PRs created via the session (bash `gh pr create` or MCP).
    #[serde(default)]
    pub pr_created_count: u32,
    /// Number of successful `gh pr merge` statements observed in bash tool calls.
    #[serde(default)]
    pub pr_merged_count: u32,

    // === Inference Idle Timeout ===
    #[serde(default)]
    pub inference_idle_timeouts: u32,
    /// Number of doom-loop recovery resamples (server-detected reasoning loops discarded and re-sampled by the sampler's retry loop).
    #[serde(default)]
    pub doom_loop_recovery_attempts: u32,
    /// Completed responses accepted still carrying confident doom-loop signals (the resample budget was spent).
    #[serde(default)]
    pub doom_loop_recovery_accepted_after_budget: u32,
    /// Tightest (lowest-threshold) raw trigger label recovery observed this session, e.g. `tail_repetition:4@thinking`.
    /// Labels only.
    #[serde(default)]
    pub doom_loop_recovery_top_trigger: Option<String>,
    /// Stream chunks consumed by doomed attempts at their mid-stream abort points, summed across resamples (terminal detections add nothing).
    #[serde(default)]
    pub doom_loop_recovery_aborted_chunks: u64,
    /// Configured idle timeout threshold (seconds); set once at session start.
    #[serde(default)]
    pub inference_idle_timeout_configured_secs: Option<u64>,

    // === GCS Upload Queue ===
    /// Total items enqueued for background upload.
    #[serde(default)]
    pub gcs_queue_enqueued: u64,
    /// Successful background uploads.
    #[serde(default)]
    pub gcs_queue_uploaded: u64,
    /// Items that exhausted retry budget (superset of expired).
    #[serde(default)]
    pub gcs_queue_failed: u64,
    /// Enqueue failures that fell back to inline upload.
    #[serde(default)]
    pub gcs_queue_fallbacks: u64,
    #[serde(default)]
    pub gcs_queue_circuit_breaker_trips: u64,
    /// Current queue depth (snapshot gauge).
    #[serde(default)]
    pub gcs_queue_pending: u64,
    /// Current disk usage of queue temp dir in bytes (snapshot gauge).
    #[serde(default)]
    pub gcs_queue_pending_bytes: u64,
    /// Orphaned temp files cleaned up at startup.
    #[serde(default)]
    pub gcs_queue_orphans_cleaned: u64,

    // === Ratings ===
    /// Number of positive ratings (thumbs-up / stars >= 4)
    pub positive_ratings: u32,
    /// Number of negative ratings (thumbs-down / stars <= 2)
    pub negative_ratings: u32,

    // === Engagement ===
    /// Number of long pauses between turns (idle > 60 s)
    pub long_pauses_count: u32,

    // === Session Metadata ===
    /// Session duration in seconds (updated on each sync)
    pub session_duration_seconds: u64,

    // === Latency Metrics ===
    /// Average time to first token in milliseconds (across all turns)
    pub avg_time_to_first_token_ms: u64,
    /// Average total response time in milliseconds (across all turns)
    pub avg_response_time_ms: u64,
    pub min_time_to_first_token_ms: u64,
    pub max_time_to_first_token_ms: u64,
    /// Total number of responses measured for latency
    pub latency_sample_count: u32,

    // === Inter-Token Latency (ITL) Metrics ===
    /// Session-level ITL p50 in milliseconds (computed from TDigest)
    pub itl_p50_ms: Option<u64>,
    /// Session-level ITL p99 in milliseconds (computed from TDigest)
    pub itl_p99_ms: Option<u64>,
    /// Session-level ITL max across all responses (monotonic max)
    pub itl_max_ms: Option<u64>,
    /// Session-level ITL mean in milliseconds (exact: sum / count)
    pub itl_mean_ms: Option<u64>,
    /// Total content chunks received across all responses
    pub total_chunk_count: u64,
    /// Number of responses measured for ITL
    pub itl_sample_count: u32,

    // === LOC Attribution ===
    /// Gross lines added by agent (monotonic, only increases)
    #[serde(default)]
    pub agent_lines_added: i64,
    /// Gross baseline lines removed by agent (monotonic)
    #[serde(default)]
    pub agent_lines_removed: i64,
    /// Agent-added lines that were later rejected/superseded (monotonic)
    #[serde(default)]
    pub agent_lines_added_reverted: i64,
    /// Agent-removed lines that were later rejected/superseded (monotonic)
    #[serde(default)]
    pub agent_lines_removed_reverted: i64,
    /// Gross lines added by human (monotonic)
    #[serde(default)]
    pub human_lines_added: i64,
    /// Gross baseline lines removed by human (monotonic)
    #[serde(default)]
    pub human_lines_removed: i64,
    /// Human-added lines that were later rejected/superseded (monotonic)
    #[serde(default)]
    pub human_lines_added_reverted: i64,
    /// Human-removed lines that were later rejected/superseded (monotonic)
    #[serde(default)]
    pub human_lines_removed_reverted: i64,
    /// Distinct files touched by agent
    #[serde(default)]
    pub agent_files_touched: u32,
    /// Distinct files touched by human
    #[serde(default)]
    pub human_files_touched: u32,
    /// Total distinct files touched (union)
    #[serde(default)]
    pub total_files_touched: u32,

    // === Internal ITL state (not serialized over the wire) ===
    /// TDigest for session-level percentile computation
    #[serde(skip)]
    pub itl_digest: Option<TDigest>,
    /// Running sum of all ITL intervals (for exact mean computation)
    #[serde(skip)]
    pub itl_sum_ms: u64,
    /// Running count of all ITL intervals (for exact mean computation)
    #[serde(skip)]
    pub itl_interval_count: u64,

    // === Observability ===
    /// Peak resident set size in bytes (monotonically increasing)
    #[serde(default)]
    pub peak_rss_bytes: u64,
}

/// Events that can be sent to the signals actor.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SignalEvent {
    // === Turn/Message Events ===
    /// Increment turn count (user submitted a prompt)
    IncrementTurn,
    /// Record an assistant message (response completed)
    RecordAssistantMessage,

    // === Tool Events ===
    RecordToolCall(String),
    RecordToolSuccess(String),
    /// Record a tool failure (tool returned error) with the tool name
    RecordToolFailure(String),
    /// Record a tool execution duration (dispatch wall clock).
    RecordToolDuration {
        tool_name: String,
        tool_call_id: String,
        duration_ms: u64,
    },

    // === Error Events ===
    /// Record a general error (sampling, network, etc.)
    /// Optionally carries an error type string (e.g. "timeout", "rate_limit", "tool_error").
    RecordError {
        error_type: Option<String>,
    },

    // === User Behavior Events ===
    /// Record a cancellation (user pressed Ctrl+C)
    RecordCancellation,
    RecordRegeneration,
    /// Record an edit-and-retry (user rewinds and submits a different prompt)
    RecordEditAndRetry,
    MarkReverted,
    /// Record successful turn completion (resets consecutive cancellations)
    RecordTurnComplete,

    RecordIdleTimeout,

    /// Record a doom-loop recovery resample (poisoned attempt discarded).
    RecordDoomLoopRecoveryAttempt {
        /// Raw trigger labels of the aborted attempt (never content).
        triggers: Vec<String>,
        /// Chunk index the mid-stream abort fired at; `None` for terminal-response detections.
        aborted_at_chunk: Option<u64>,
    },

    /// Record a completed response accepted with confident doom-loop signals after the resample budget was spent.
    RecordDoomLoopAcceptedAfterBudget {
        triggers: Vec<String>,
    },

    /// Set-once tracing config fields (idle timeout threshold).
    /// Called once at session construction; preserved on the backend when unset.
    SetTracingConfig {
        inference_idle_timeout_configured_secs: u64,
    },

    /// Snapshot GCS upload queue stats into signals.
    /// The actor reads the atomics once and stores plain u64 values; the Arc is not retained in actor state.
    RecordGcsQueueSnapshot {
        enqueued: u64,
        uploaded: u64,
        failed: u64,
        fallbacks: u64,
        circuit_breaker_trips: u64,
        pending: u64,
        pending_bytes: u64,
        orphans_cleaned: u64,
    },

    // === Rating Events ===
    /// Record a positive rating (thumbs-up / stars >= 4)
    RecordPositiveRating,
    /// Record a negative rating (thumbs-down / stars <= 2)
    RecordNegativeRating,

    // === Context Events ===
    RecordCompaction {
        tokens_before: u64,
    },
    UpdateContextUsage {
        tokens_used: u64,
        context_window: u64,
    },

    // === Model Events ===
    RecordModelUsage(String),
    SetPrimaryModel(String),
    /// Served checkpoint of the current turn's latest model response.
    RecordModelFingerprint(String),
    /// Set or clear the live dispatch attribution used by subagent progress.
    SetActiveDispatch {
        model_id: Option<String>,
        reasoning_effort: Option<String>,
    },

    // === Latency Events ===
    RecordLatency {
        time_to_first_token_ms: u64,
        total_response_time_ms: u64,
    },
    RecordInferenceMetrics(InferenceLatencyStats),
    /// Record token usage from a model response (completion and reasoning tokens).
    /// Accumulated per turn and reset at each `TakeTurnEndSnapshot`.
    RecordTokenUsage {
        completion_tokens: u32,
        reasoning_tokens: u32,
    },

    // === LOC Attribution Events ===
    /// Record a LOC change (lines added/removed by agent or human).
    RecordLocChange {
        is_agent: bool,
        lines_added: i64,
        lines_removed: i64,
        file_path: Box<std::path::PathBuf>,
    },
    /// Record lines reverted (rejected/superseded hunk).
    RecordLocRevert {
        lines_added_reverted: i64,
        lines_removed_reverted: i64,
    },

    // === Turn Delta Events === Take a turn-end snapshot and compute delta from previous turn end.
    // Returns the delta snapshot for sending to the backend.
    // `completed`: the turn finished, so its tool outcomes become the ones feedback attaches (`GetLastTurnToolOutcomes`); a cancelled or failed turn's partial outcomes must not replace them.
    TakeTurnEndSnapshot {
        respond_to: oneshot::Sender<TurnDeltaSnapshot>,
        completed: bool,
    },
    /// Get tool outcomes from the last completed turn (for feedback notifications).
    GetLastTurnToolOutcomes(oneshot::Sender<Vec<ToolOutcome>>),

    /// Bare echo/printf command executed via the bash tool.
    RecordBareEcho,

    // === Git/PR Metric Events ===
    /// Successful `git commit` statement in a bash tool call.
    RecordGitCommit,
    /// PR created via bash `gh pr create` or an MCP create_pull_request tool.
    RecordPrCreated(PrCreatedSignal),
    /// Successful `gh pr merge` statement in a bash tool call.
    RecordPrMerged,

    // === Control Events ===
    /// Seed initial counts from persisted data (for session resume)
    SeedCounts {
        user_message_count: u32,
        assistant_message_count: u32,
        tool_call_count: u32,
        tools_used: Vec<String>,
        models_used: Vec<String>,
    },
    /// Restore full signals state from a persisted snapshot.
    /// Preferred over SeedCounts when a signals.json file exists.
    RestoreSignals(SessionSignals),
    GetSnapshot(oneshot::Sender<SessionSignals>),
    CheckAndMarkSync(oneshot::Sender<bool>),
    Shutdown,
}

/// Handle for sending signals to the tracker actor.
/// This is cheap to clone and can be passed around freely.
/// All operations are non-blocking sends to a channel.
#[derive(Clone)]
pub struct SessionSignalsHandle {
    tx: mpsc::UnboundedSender<SignalEvent>,
}

impl SessionSignalsHandle {
    /// Create a new standalone signals handle with its own background actor.
    pub fn new() -> Self {
        let (handle, actor) = SessionSignalsActor::new();
        tokio::spawn(actor.run());
        handle
    }
}

impl Default for SessionSignalsHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionSignalsHandle {
    // === Turn/Message Methods ===

    /// Increment the turn count (called when user submits a prompt).
    /// This also increments user_message_count.
    pub fn increment_turn(&self) {
        let _ = self.tx.send(SignalEvent::IncrementTurn);
    }

    /// Record an assistant message (response completed).
    pub fn record_assistant_message(&self) {
        let _ = self.tx.send(SignalEvent::RecordAssistantMessage);
    }

    /// Record successful turn completion (resets consecutive cancellations).
    pub fn record_turn_complete(&self) {
        let _ = self.tx.send(SignalEvent::RecordTurnComplete);
    }

    // === Tool Methods ===

    pub fn record_tool_call(&self, tool_name: impl Into<String>) {
        let _ = self.tx.send(SignalEvent::RecordToolCall(tool_name.into()));
    }

    pub(crate) fn record_tool_success(&self, tool_name: impl Into<String>) {
        let _ = self
            .tx
            .send(SignalEvent::RecordToolSuccess(tool_name.into()));
    }

    pub fn record_tool_failure(&self, tool_name: impl Into<String>) {
        let _ = self
            .tx
            .send(SignalEvent::RecordToolFailure(tool_name.into()));
    }

    /// Record a tool execution duration in milliseconds (dispatch wall clock).
    pub(crate) fn record_tool_duration(
        &self,
        tool_name: impl Into<String>,
        tool_call_id: impl Into<String>,
        duration_ms: u64,
    ) {
        let _ = self.tx.send(SignalEvent::RecordToolDuration {
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            duration_ms,
        });
    }

    /// Record a bare echo/printf command for telemetry.
    /// Called from `execute_tool_calls` when `BashOutput.was_bare_echo` is true.
    /// Runs independently of doom loop detector config.
    pub(crate) fn record_bare_echo(&self) {
        let _ = self.tx.send(SignalEvent::RecordBareEcho);
    }

    // === Git/PR Metric Methods ===

    /// Record a successful `git commit` statement from a bash tool call.
    pub(crate) fn record_git_commit(&self) {
        let _ = self.tx.send(SignalEvent::RecordGitCommit);
    }

    /// Record a PR created during this turn.
    pub(crate) fn record_pr_created(&self, pr: PrCreatedSignal) {
        let _ = self.tx.send(SignalEvent::RecordPrCreated(pr));
    }

    /// Record a successful `gh pr merge` statement from a bash tool call.
    pub(crate) fn record_pr_merged(&self) {
        let _ = self.tx.send(SignalEvent::RecordPrMerged);
    }

    // === Error Methods ===

    pub fn record_error(&self) {
        let _ = self.tx.send(SignalEvent::RecordError { error_type: None });
    }

    /// Record an error with a type string for analytics (e.g. "timeout", "rate_limit").
    pub(crate) fn record_error_typed(&self, error_type: impl Into<String>) {
        let _ = self.tx.send(SignalEvent::RecordError {
            error_type: Some(error_type.into()),
        });
    }

    // === User Behavior Methods ===

    /// Record a cancellation (user pressed Ctrl+C).
    pub fn record_cancellation(&self) {
        let _ = self.tx.send(SignalEvent::RecordCancellation);
    }

    pub fn record_regeneration(&self) {
        let _ = self.tx.send(SignalEvent::RecordRegeneration);
    }

    /// Record an edit-and-retry (user rewinds and submits a different prompt).
    pub(crate) fn record_edit_and_retry(&self) {
        let _ = self.tx.send(SignalEvent::RecordEditAndRetry);
    }

    pub(crate) fn record_idle_timeout(&self) {
        let _ = self.tx.send(SignalEvent::RecordIdleTimeout);
    }

    /// Record a doom-loop recovery resample (poisoned attempt discarded).
    pub(crate) fn record_doom_loop_recovery_attempt(
        &self,
        triggers: Vec<String>,
        aborted_at_chunk: Option<u64>,
    ) {
        let _ = self.tx.send(SignalEvent::RecordDoomLoopRecoveryAttempt {
            triggers,
            aborted_at_chunk,
        });
    }

    /// Record a budget-spent accept (final response kept confident signals).
    pub(crate) fn record_doom_loop_accepted_after_budget(&self, triggers: Vec<String>) {
        let _ = self
            .tx
            .send(SignalEvent::RecordDoomLoopAcceptedAfterBudget { triggers });
    }

    /// Set-once tracing config fields at session construction.
    /// Records the configured thresholds so dashboards can filter/group by config.
    /// Preserved on the backend when unset; subsequent syncs with None don't overwrite.
    pub(crate) fn set_tracing_config(&self, inference_idle_timeout_secs: u64) {
        let _ = self.tx.send(SignalEvent::SetTracingConfig {
            inference_idle_timeout_configured_secs: inference_idle_timeout_secs,
        });
    }

    /// Snapshot GCS upload queue stats into signals.
    ///
    /// Reads the atomics from `UploadQueueStats` once and sends plain u64 values to the actor; the Arc is NOT retained in the signal event.
    pub(crate) fn snapshot_gcs_queue(&self, stats: &distill_file_utils::queue::UploadQueueStats) {
        use std::sync::atomic::Ordering;
        let _ = self.tx.send(SignalEvent::RecordGcsQueueSnapshot {
            enqueued: stats.enqueued.load(Ordering::Relaxed),
            uploaded: stats.uploaded.load(Ordering::Relaxed),
            failed: stats.failed.load(Ordering::Relaxed),
            fallbacks: stats.enqueue_fallbacks.load(Ordering::Relaxed),
            circuit_breaker_trips: stats.circuit_breaker_trips.load(Ordering::Relaxed),
            pending: stats.pending.load(Ordering::Relaxed),
            pending_bytes: stats.pending_bytes.load(Ordering::Relaxed),
            orphans_cleaned: distill_file_utils::queue::last_orphans_cleaned(),
        });
    }

    pub fn mark_reverted(&self) {
        let _ = self.tx.send(SignalEvent::MarkReverted);
    }

    // === Rating Methods ===

    /// Record a positive rating (thumbs-up / stars >= 4).
    pub(crate) fn record_positive_rating(&self) {
        let _ = self.tx.send(SignalEvent::RecordPositiveRating);
    }

    /// Record a negative rating (thumbs-down / stars <= 2).
    pub(crate) fn record_negative_rating(&self) {
        let _ = self.tx.send(SignalEvent::RecordNegativeRating);
    }

    // === Context Methods ===

    pub fn record_compaction(&self, tokens_before: u64) {
        let _ = self
            .tx
            .send(SignalEvent::RecordCompaction { tokens_before });
    }

    pub fn update_context_usage(&self, tokens_used: u64, context_window: u64) {
        let _ = self.tx.send(SignalEvent::UpdateContextUsage {
            tokens_used,
            context_window,
        });
    }

    // === Model Methods ===

    /// Record model usage (adds to models_used list if not already present).
    pub fn record_model_usage(&self, model_id: impl Into<String>) {
        let _ = self.tx.send(SignalEvent::RecordModelUsage(model_id.into()));
    }

    /// Record the served fingerprint of the current turn's latest model response. Held here,
    /// not in the turn task, so a cancelled or failed turn's snapshot still carries it.
    pub(crate) fn record_model_fingerprint(&self, fingerprint: impl Into<String>) {
        let _ = self
            .tx
            .send(SignalEvent::RecordModelFingerprint(fingerprint.into()));
    }

    pub fn set_primary_model(&self, model_id: impl Into<String>) {
        let _ = self.tx.send(SignalEvent::SetPrimaryModel(model_id.into()));
    }

    /// Publish the final per-round route after Jev chooser/tier/floor
    /// adjustments. This travels through the existing progress snapshot path.
    pub fn set_active_dispatch(
        &self,
        model_id: impl Into<String>,
        reasoning_effort: Option<impl Into<String>>,
    ) {
        let _ = self.tx.send(SignalEvent::SetActiveDispatch {
            model_id: Some(model_id.into()),
            reasoning_effort: reasoning_effort.map(Into::into),
        });
    }

    /// Clear live route attribution after fallback, cancellation, or
    /// completion so a later progress tick cannot show a stale round.
    pub fn clear_active_dispatch(&self) {
        let _ = self.tx.send(SignalEvent::SetActiveDispatch {
            model_id: None,
            reasoning_effort: None,
        });
    }

    // === Seeding Methods ===

    /// Seed initial counts from persisted conversation data.
    /// Call this when resuming a session to restore accurate counters (message counts, tool call count, distinct tools/models used).
    /// Prefer `restore_signals` when a full persisted snapshot is available.
    pub fn seed_counts(
        &self,
        user_message_count: u32,
        assistant_message_count: u32,
        tool_call_count: u32,
        tools_used: Vec<String>,
        models_used: Vec<String>,
    ) {
        if self
            .tx
            .send(SignalEvent::SeedCounts {
                user_message_count,
                assistant_message_count,
                tool_call_count,
                tools_used,
                models_used,
            })
            .is_err()
        {
            tracing::warn!("Failed to seed signal counts: actor shut down");
        }
    }

    /// Restore full signals state from a persisted snapshot.
    /// Preferred over `seed_counts` when a `signals.json` file exists.
    /// Restores all counters, including ones that survive compaction (turn_count, error_count, tool_failure_count, etc.).
    pub(crate) fn restore_signals(&self, signals: SessionSignals) {
        if self.tx.send(SignalEvent::RestoreSignals(signals)).is_err() {
            tracing::warn!("Failed to restore signals: actor shut down");
        }
    }

    // === Latency Methods ===

    /// Record latency metrics for a response.
    /// `time_to_first_token_ms`: Time from request start to first token received.
    /// `total_response_time_ms`: Time from request start to response complete.
    pub fn record_latency(&self, time_to_first_token_ms: u64, total_response_time_ms: u64) {
        let _ = self.tx.send(SignalEvent::RecordLatency {
            time_to_first_token_ms,
            total_response_time_ms,
        });
    }

    /// Called after each streaming response completes with ITL stats computed from per-chunk timestamps.
    pub(crate) fn record_inference_metrics(&self, stats: InferenceLatencyStats) {
        let _ = self.tx.send(SignalEvent::RecordInferenceMetrics(stats));
    }

    /// Record token usage from a model response.
    /// `completion_tokens`: total output tokens (includes reasoning tokens).
    /// `reasoning_tokens`: thinking/reasoning tokens (subset of completion_tokens).
    #[tracing::instrument(skip_all, fields(completion_tokens, reasoning_tokens))]
    pub(crate) fn record_token_usage(&self, completion_tokens: u32, reasoning_tokens: u32) {
        let span = tracing::Span::current();
        span.record("completion_tokens", i64::from(completion_tokens));
        span.record("reasoning_tokens", i64::from(reasoning_tokens));
        let _ = self.tx.send(SignalEvent::RecordTokenUsage {
            completion_tokens,
            reasoning_tokens,
        });
    }

    // === Turn Delta Methods ===

    /// Take a turn-end snapshot and compute delta from previous turn end.
    /// The actor atomically stores the current state as the new baseline for the next delta computation.
    /// Returns `None` if the actor is shut down.
    pub(crate) async fn take_turn_end_snapshot(&self) -> Option<TurnDeltaSnapshot> {
        self.take_turn_end_snapshot_inner(true).await
    }

    /// [`Self::take_turn_end_snapshot`] for a turn that was cancelled or failed, taken by its
    /// terminal. Same delta and baseline advance; the partial tool outcomes stay out of feedback.
    pub(crate) async fn take_unfinished_turn_snapshot(&self) -> Option<TurnDeltaSnapshot> {
        self.take_turn_end_snapshot_inner(false).await
    }

    async fn take_turn_end_snapshot_inner(&self, completed: bool) -> Option<TurnDeltaSnapshot> {
        let (respond_to, rx) = oneshot::channel();
        self.tx
            .send(SignalEvent::TakeTurnEndSnapshot {
                respond_to,
                completed,
            })
            .ok()?;
        rx.await.ok()
    }

    // === Control Methods ===

    /// Returns None if the actor has been shut down.
    pub async fn snapshot(&self) -> Option<SessionSignals> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(SignalEvent::GetSnapshot(tx)).ok()?;
        rx.await.ok()
    }

    /// Tool outcomes from the last completed turn (preserved across turn resets).
    pub(crate) async fn last_turn_tool_outcomes(&self) -> Vec<ToolOutcome> {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(SignalEvent::GetLastTurnToolOutcomes(tx))
            .is_err()
        {
            return Vec::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Returns true if sync should be performed (enough time has passed).
    /// Automatically marks the sync time if returning true.
    pub async fn check_and_mark_sync(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(SignalEvent::CheckAndMarkSync(tx)).is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    // === LOC Attribution Methods ===

    /// Record a LOC change from the LOC sink bridge.
    pub(crate) fn record_loc_change(
        &self,
        is_agent: bool,
        lines_added: i64,
        lines_removed: i64,
        file_path: std::path::PathBuf,
    ) {
        let _ = self.tx.send(SignalEvent::RecordLocChange {
            is_agent,
            lines_added,
            lines_removed,
            file_path: Box::new(file_path),
        });
    }

    /// Record lines reverted (rejected/superseded hunk).
    pub(crate) fn record_loc_revert(&self, lines_added_reverted: i64, lines_removed_reverted: i64) {
        let _ = self.tx.send(SignalEvent::RecordLocRevert {
            lines_added_reverted,
            lines_removed_reverted,
        });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(SignalEvent::Shutdown);
    }
}

/// Actor that processes signal events.
///
/// Runs as a background task and maintains the signal state.
pub struct SessionSignalsActor {
    rx: mpsc::UnboundedReceiver<SignalEvent>,
    signals: SessionSignals,
    /// Set of distinct tools used (for deduplication)
    tools_set: HashSet<String>,
    /// Set of distinct models used (for deduplication)
    models_set: HashSet<String>,
    /// Session start time (for calculating duration)
    session_start: Instant,
    last_sync: Option<Instant>,
    /// Minimum interval between syncs
    sync_interval: Duration,
    /// Sum of all time-to-first-token values (for computing average)
    ttft_sum_ms: u64,
    /// Sum of all response time values (for computing average)
    response_time_sum_ms: u64,
    /// Time of the last turn start (for detecting long pauses)
    last_turn_time: Option<Instant>,
    /// Threshold for considering a pause as "long" (default: 60 seconds)
    long_pause_threshold: Duration,

    // === Turn Delta State ===
    /// Snapshot of signals at the previous turn end (for delta computation).
    /// `None` before the first turn-end snapshot is taken.
    previous_turn_snapshot: Option<SessionSignals>,
    /// Served fingerprint of this turn's latest model response. Reset on `IncrementTurn` and
    /// taken by `TakeTurnEndSnapshot`.
    turn_model_fingerprint: Option<String>,
    /// Reset after each `TakeTurnEndSnapshot`.
    tools_this_turn: Vec<String>,
    /// Key: tool name, Value: (successes, failures).
    /// Reset after each `TakeTurnEndSnapshot`.
    tool_outcomes_this_turn: HashMap<String, (u32, u32)>,
    /// Reset after each `TakeTurnEndSnapshot`.
    error_types_this_turn: Vec<String>,
    /// Reset after each `TakeTurnEndSnapshot`.
    tool_durations_this_turn: Vec<ToolDuration>,
    /// Latency of the most recent response in the current turn.
    /// Reset after each `TakeTurnEndSnapshot`.
    last_turn_ttft_ms: Option<u64>,
    /// Total response time of the most recent response in the current turn.
    last_turn_response_time_ms: Option<u64>,
    /// Accumulated ITL intervals for the current turn (cleared at turn end).
    turn_itl_intervals: Vec<u64>,
    /// Accumulated response (completion - reasoning) tokens for the current turn.
    /// `None` until the first `RecordTokenUsage` event in this turn.
    /// Reset to `None` after each `TakeTurnEndSnapshot`.
    turn_response_tokens: Option<u32>,
    /// Accumulated thinking (reasoning) tokens for the current turn.
    /// `None` until the first `RecordTokenUsage` event in this turn.
    /// Reset to `None` after each `TakeTurnEndSnapshot`.
    turn_thinking_tokens: Option<u32>,
    /// Reset after each `TakeTurnEndSnapshot`.
    prs_created_this_turn: Vec<PrCreatedSignal>,
    /// Preserved across turn resets for feedback notifications.
    last_completed_turn_tool_outcomes: Vec<ToolOutcome>,

    // === LOC Attribution state ===
    /// Distinct files touched by agent (for dedup)
    agent_files_set: HashSet<std::path::PathBuf>,
    /// Distinct files touched by human (for dedup)
    human_files_set: HashSet<std::path::PathBuf>,
}

impl SessionSignalsActor {
    /// Create a new actor and its handle.
    ///
    /// The actor must be spawned as a background task using `actor.run()`.
    pub fn new() -> (SessionSignalsHandle, Self) {
        Self::with_sync_interval(Duration::from_secs(60))
    }

    /// Fold `triggers` into the session's tightest-observed trigger label.
    fn merge_doom_loop_top_trigger(&mut self, triggers: &[String]) {
        let current = self.signals.doom_loop_recovery_top_trigger.take();
        self.signals.doom_loop_recovery_top_trigger = merge_tightest_trigger(current, triggers);
    }

    pub(crate) fn with_sync_interval(sync_interval: Duration) -> (SessionSignalsHandle, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = SessionSignalsHandle { tx };
        let actor = Self {
            rx,
            signals: SessionSignals::default(),
            tools_set: HashSet::new(),
            models_set: HashSet::new(),
            session_start: Instant::now(),
            last_sync: None,
            sync_interval,
            ttft_sum_ms: 0,
            response_time_sum_ms: 0,
            last_turn_time: None,
            long_pause_threshold: Duration::from_secs(60),
            // Turn delta state
            previous_turn_snapshot: None,
            turn_model_fingerprint: None,
            tools_this_turn: Vec::new(),
            tool_outcomes_this_turn: HashMap::new(),
            error_types_this_turn: Vec::new(),
            tool_durations_this_turn: Vec::new(),
            last_turn_ttft_ms: None,
            last_turn_response_time_ms: None,
            turn_itl_intervals: Vec::new(),
            turn_response_tokens: None,
            turn_thinking_tokens: None,
            prs_created_this_turn: Vec::new(),
            last_completed_turn_tool_outcomes: Vec::new(),
            agent_files_set: HashSet::new(),
            human_files_set: HashSet::new(),
        };
        (handle, actor)
    }

    /// Run the actor, processing events until shutdown.
    ///
    /// This should be spawned as a background task.
    pub async fn run(mut self) {
        while let Some(event) = self.rx.recv().await {
            match event {
                // === Turn/Message Events ===
                SignalEvent::IncrementTurn => {
                    // Detect long pauses (longer than threshold since last turn)
                    if let Some(last) = self.last_turn_time
                        && last.elapsed() >= self.long_pause_threshold
                    {
                        self.signals.long_pauses_count += 1;
                    }
                    self.last_turn_time = Some(Instant::now());

                    self.signals.turn_count += 1;
                    self.signals.user_message_count += 1;
                    self.turn_model_fingerprint = None;
                }
                SignalEvent::RecordAssistantMessage => {
                    self.signals.assistant_message_count += 1;
                }
                SignalEvent::RecordTurnComplete => {
                    self.signals.consecutive_cancellations = 0;
                }
                SignalEvent::RecordIdleTimeout => {
                    self.signals.inference_idle_timeouts += 1;
                }
                SignalEvent::RecordDoomLoopRecoveryAttempt {
                    triggers,
                    aborted_at_chunk,
                } => {
                    self.signals.doom_loop_recovery_attempts += 1;
                    self.signals.doom_loop_recovery_aborted_chunks += aborted_at_chunk.unwrap_or(0);
                    self.merge_doom_loop_top_trigger(&triggers);
                }
                SignalEvent::RecordDoomLoopAcceptedAfterBudget { triggers } => {
                    self.signals.doom_loop_recovery_accepted_after_budget += 1;
                    self.merge_doom_loop_top_trigger(&triggers);
                }
                SignalEvent::SetTracingConfig {
                    inference_idle_timeout_configured_secs,
                } => {
                    self.signals.inference_idle_timeout_configured_secs =
                        Some(inference_idle_timeout_configured_secs);
                }
                SignalEvent::RecordGcsQueueSnapshot {
                    enqueued,
                    uploaded,
                    failed,
                    fallbacks,
                    circuit_breaker_trips,
                    pending,
                    pending_bytes,
                    orphans_cleaned,
                } => {
                    self.signals.gcs_queue_enqueued = enqueued;
                    self.signals.gcs_queue_uploaded = uploaded;
                    self.signals.gcs_queue_failed = failed;
                    self.signals.gcs_queue_fallbacks = fallbacks;
                    self.signals.gcs_queue_circuit_breaker_trips = circuit_breaker_trips;
                    self.signals.gcs_queue_pending = pending;
                    self.signals.gcs_queue_pending_bytes = pending_bytes;
                    self.signals.gcs_queue_orphans_cleaned = orphans_cleaned;
                }

                // === Tool Events ===
                SignalEvent::RecordToolCall(tool_name) => {
                    self.signals.tool_call_count += 1;
                    // Track per-turn tool usage (includes repeats)
                    self.tools_this_turn.push(tool_name.clone());
                    if self.tools_set.insert(tool_name.clone()) {
                        self.signals.tools_used.push(tool_name);
                    }
                }
                SignalEvent::RecordToolSuccess(tool_name) => {
                    // Track per-tool success for this turn
                    let entry = self
                        .tool_outcomes_this_turn
                        .entry(tool_name)
                        .or_insert((0, 0));
                    entry.0 += 1;
                }
                SignalEvent::RecordToolFailure(tool_name) => {
                    self.signals.tool_failure_count += 1;
                    self.signals.error_count += 1; // Tool failures also count as errors
                    // Track per-tool failure for this turn
                    let entry = self
                        .tool_outcomes_this_turn
                        .entry(tool_name)
                        .or_insert((0, 0));
                    entry.1 += 1;
                }
                SignalEvent::RecordToolDuration {
                    tool_name,
                    tool_call_id,
                    duration_ms,
                } => {
                    self.tool_durations_this_turn.push(ToolDuration {
                        tool_name,
                        tool_call_id,
                        duration_ms,
                    });
                }
                SignalEvent::RecordBareEcho => {
                    self.signals.bash_bare_echo_count += 1;
                }

                // === Git/PR Metric Events ===
                SignalEvent::RecordGitCommit => {
                    self.signals.git_commit_count += 1;
                }
                SignalEvent::RecordPrCreated(pr) => {
                    self.signals.pr_created_count += 1;
                    self.prs_created_this_turn.push(pr);
                }
                SignalEvent::RecordPrMerged => {
                    self.signals.pr_merged_count += 1;
                }
                // === Error Events ===
                SignalEvent::RecordError { error_type } => {
                    self.signals.error_count += 1;
                    if let Some(et) = error_type {
                        self.error_types_this_turn.push(et);
                    }
                }

                // === User Behavior Events ===
                SignalEvent::RecordCancellation => {
                    self.signals.cancellation_count += 1;
                    self.signals.consecutive_cancellations += 1;
                }
                SignalEvent::RecordRegeneration => {
                    self.signals.regeneration_count += 1;
                }
                SignalEvent::RecordEditAndRetry => {
                    self.signals.edit_and_retry_count += 1;
                }
                SignalEvent::MarkReverted => {
                    self.signals.has_reverted = true;
                }

                // === Rating Events ===
                SignalEvent::RecordPositiveRating => {
                    self.signals.positive_ratings += 1;
                }
                SignalEvent::RecordNegativeRating => {
                    self.signals.negative_ratings += 1;
                }

                // === Context Events ===
                SignalEvent::RecordCompaction { tokens_before } => {
                    self.signals.compaction_count += 1;
                    self.signals.total_tokens_before_compaction += tokens_before;
                }
                SignalEvent::UpdateContextUsage {
                    tokens_used,
                    context_window,
                } => {
                    self.signals.context_window_usage =
                        ((tokens_used * 100) / context_window).min(100) as u8;
                    self.signals.context_tokens_used = tokens_used;
                    self.signals.context_window_tokens = context_window;
                }

                // === Model Events ===
                SignalEvent::RecordModelUsage(model_id) => {
                    if self.models_set.insert(model_id.clone()) {
                        self.signals.models_used.push(model_id);
                    }
                }
                SignalEvent::RecordModelFingerprint(fingerprint) => {
                    self.turn_model_fingerprint = Some(fingerprint);
                }
                SignalEvent::SetPrimaryModel(model_id) => {
                    self.signals.primary_model_id = Some(model_id.clone());
                    // Also record as used
                    if self.models_set.insert(model_id.clone()) {
                        self.signals.models_used.push(model_id);
                    }
                }
                SignalEvent::SetActiveDispatch {
                    model_id,
                    reasoning_effort,
                } => {
                    self.signals.active_model_id = model_id;
                    self.signals.active_reasoning_effort = reasoning_effort;
                }

                // === Latency Events ===
                SignalEvent::RecordLatency {
                    time_to_first_token_ms,
                    total_response_time_ms,
                } => {
                    // Track per-turn latency (overwritten each time within a turn; the last value before TakeTurnEndSnapshot is used)
                    self.last_turn_ttft_ms = Some(time_to_first_token_ms);
                    self.last_turn_response_time_ms = Some(total_response_time_ms);

                    self.update_latency_stats(time_to_first_token_ms, total_response_time_ms);
                }
                SignalEvent::RecordInferenceMetrics(stats) => {
                    // Track per-turn latency for turn-delta snapshots
                    if let Some(ttfb) = stats.time_to_first_token_ms {
                        self.last_turn_ttft_ms = Some(ttfb);
                        self.last_turn_response_time_ms = Some(stats.time_to_last_byte_ms);
                    }

                    // Accumulate all ITL intervals from this response into turn buffer
                    if !stats.itl_intervals_ms.is_empty() {
                        self.turn_itl_intervals
                            .extend_from_slice(&stats.itl_intervals_ms);
                        self.signals.total_chunk_count += stats.chunk_count as u64;
                        self.signals.itl_sample_count += 1;
                    }

                    // Also feed the existing TTFB/TTLB tracking for backward compat
                    if let Some(ttfb) = stats.time_to_first_token_ms {
                        self.update_latency_stats(ttfb, stats.time_to_last_byte_ms);
                    }
                }
                SignalEvent::RecordTokenUsage {
                    completion_tokens,
                    reasoning_tokens,
                } => {
                    let response = completion_tokens.saturating_sub(reasoning_tokens);
                    *self.turn_response_tokens.get_or_insert(0) += response;
                    *self.turn_thinking_tokens.get_or_insert(0) += reasoning_tokens;
                }

                // === LOC Attribution Events ===
                SignalEvent::RecordLocChange {
                    is_agent,
                    lines_added,
                    lines_removed,
                    file_path,
                } => {
                    let file_path = *file_path;
                    // Update total_files_touched before per-author sets
                    let is_new_file = !self.agent_files_set.contains(&file_path)
                        && !self.human_files_set.contains(&file_path);
                    if is_new_file {
                        self.signals.total_files_touched += 1;
                    }

                    if is_agent {
                        self.signals.agent_lines_added += lines_added;
                        self.signals.agent_lines_removed += lines_removed;
                        if self.agent_files_set.insert(file_path) {
                            self.signals.agent_files_touched += 1;
                        }
                    } else {
                        self.signals.human_lines_added += lines_added;
                        self.signals.human_lines_removed += lines_removed;
                        if self.human_files_set.insert(file_path) {
                            self.signals.human_files_touched += 1;
                        }
                    }
                }
                SignalEvent::RecordLocRevert {
                    lines_added_reverted: _,
                    lines_removed_reverted: _,
                } => {
                    // TODO: Attribute reverts per-author once HunkRemoved events carry author information
                    // For now all 4 revert counters stay at 0 to avoid publishing misleading partial data
                }

                // === Turn Delta Events ===
                SignalEvent::TakeTurnEndSnapshot {
                    respond_to,
                    completed,
                } => {
                    // Reconcile PR-create attribution now that every event of the turn has been processed (same channel, FIFO)
                    // Parallel tool results can record a create before a sibling commit lands
                    if self.signals.git_commit_count > 0 {
                        for pr in &mut self.prs_created_this_turn {
                            pr.had_commit_in_session = true;
                        }
                    }

                    // Update duration before computing delta
                    self.signals.session_duration_seconds = self.session_start.elapsed().as_secs();

                    // Sample peak RSS for memory monitoring
                    let current_rss = sample_rss_bytes();
                    if current_rss > self.signals.peak_rss_bytes {
                        self.signals.peak_rss_bytes = current_rss;
                    }

                    let (turn_itl_p50, turn_itl_p99, turn_itl_max, turn_itl_mean) =
                        self.compute_and_merge_turn_itl();

                    self.update_session_itl_percentiles();

                    let prev = self.previous_turn_snapshot.as_ref();
                    let delta_tool_calls = self.signals.tool_call_count as i64
                        - prev.map_or(0, |p| p.tool_call_count as i64);
                    let delta_tool_failures = self.signals.tool_failure_count as i64
                        - prev.map_or(0, |p| p.tool_failure_count as i64);

                    let mut delta = SessionSignalsDelta {
                        turn_number: self.signals.turn_count,
                        delta_tool_calls,
                        delta_tool_failures,
                        delta_errors: self.signals.error_count as i64
                            - prev.map_or(0, |p| p.error_count as i64),
                        delta_cancellations: self.signals.cancellation_count as i64
                            - prev.map_or(0, |p| p.cancellation_count as i64),
                        delta_regenerations: self.signals.regeneration_count as i64
                            - prev.map_or(0, |p| p.regeneration_count as i64),
                        delta_compactions: self.signals.compaction_count as i64
                            - prev.map_or(0, |p| p.compaction_count as i64),
                        delta_edit_and_retries: self.signals.edit_and_retry_count as i64
                            - prev.map_or(0, |p| p.edit_and_retry_count as i64),
                        delta_positive_ratings: self.signals.positive_ratings as i64
                            - prev.map_or(0, |p| p.positive_ratings as i64),
                        delta_negative_ratings: self.signals.negative_ratings as i64
                            - prev.map_or(0, |p| p.negative_ratings as i64),
                        delta_assistant_messages: self.signals.assistant_message_count as i64
                            - prev.map_or(0, |p| p.assistant_message_count as i64),
                        delta_long_pauses: self.signals.long_pauses_count as i64
                            - prev.map_or(0, |p| p.long_pauses_count as i64),
                        delta_successful_tool_uses: delta_tool_calls - delta_tool_failures,
                        consecutive_cancellations: self.signals.consecutive_cancellations,
                        error_types_this_turn: std::mem::take(&mut self.error_types_this_turn),
                        tools_this_turn: Vec::new(),      // filled below
                        tools_this_turn_truncated: false, // filled below
                        tool_outcomes_this_turn: Vec::new(), // filled below
                        tool_durations_this_turn: std::mem::take(
                            &mut self.tool_durations_this_turn,
                        ),
                        last_time_to_first_token_ms: self.last_turn_ttft_ms.take(),
                        last_total_response_time_ms: self.last_turn_response_time_ms.take(),
                        last_itl_p50_ms: turn_itl_p50,
                        last_itl_p99_ms: turn_itl_p99,
                        last_itl_max_ms: turn_itl_max,
                        last_itl_mean_ms: turn_itl_mean,
                        response_tokens: self.turn_response_tokens.take(),
                        thinking_tokens: self.turn_thinking_tokens.take(),
                        // LOC deltas
                        delta_agent_lines_added: self.signals.agent_lines_added
                            - prev.map_or(0, |p| p.agent_lines_added),
                        delta_agent_lines_removed: self.signals.agent_lines_removed
                            - prev.map_or(0, |p| p.agent_lines_removed),
                        delta_agent_lines_added_reverted: self.signals.agent_lines_added_reverted
                            - prev.map_or(0, |p| p.agent_lines_added_reverted),
                        delta_agent_lines_removed_reverted: self
                            .signals
                            .agent_lines_removed_reverted
                            - prev.map_or(0, |p| p.agent_lines_removed_reverted),
                        delta_human_lines_added: self.signals.human_lines_added
                            - prev.map_or(0, |p| p.human_lines_added),
                        delta_human_lines_removed: self.signals.human_lines_removed
                            - prev.map_or(0, |p| p.human_lines_removed),
                        delta_human_lines_added_reverted: self.signals.human_lines_added_reverted
                            - prev.map_or(0, |p| p.human_lines_added_reverted),
                        delta_human_lines_removed_reverted: self
                            .signals
                            .human_lines_removed_reverted
                            - prev.map_or(0, |p| p.human_lines_removed_reverted),
                        delta_agent_files_touched: self.signals.agent_files_touched as i64
                            - prev.map_or(0, |p| p.agent_files_touched as i64),
                        delta_human_files_touched: self.signals.human_files_touched as i64
                            - prev.map_or(0, |p| p.human_files_touched as i64),
                        delta_total_files_touched: self.signals.total_files_touched as i64
                            - prev.map_or(0, |p| p.total_files_touched as i64),
                        delta_git_commits: self.signals.git_commit_count as i64
                            - prev.map_or(0, |p| p.git_commit_count as i64),
                        delta_prs_created: self.signals.pr_created_count as i64
                            - prev.map_or(0, |p| p.pr_created_count as i64),
                        delta_prs_merged: self.signals.pr_merged_count as i64
                            - prev.map_or(0, |p| p.pr_merged_count as i64),
                        prs_created_this_turn: std::mem::take(&mut self.prs_created_this_turn),
                    };

                    // Deduplicate and cap tools_this_turn at 100 entries.
                    // With tool_outcomes_this_turn providing per-tool counts, duplicates in tools_this_turn are redundant
                    let mut tools = std::mem::take(&mut self.tools_this_turn);
                    tools.sort();
                    tools.dedup();
                    const MAX_TOOLS_THIS_TURN: usize = 100;
                    let truncated = tools.len() > MAX_TOOLS_THIS_TURN;
                    if truncated {
                        tools.truncate(MAX_TOOLS_THIS_TURN);
                    }
                    delta.tools_this_turn = tools;
                    delta.tools_this_turn_truncated = truncated;

                    // Build sorted per-tool outcome list from the accumulated map.
                    let mut outcomes: Vec<ToolOutcome> =
                        std::mem::take(&mut self.tool_outcomes_this_turn)
                            .into_iter()
                            .map(|(tool_name, (successes, failures))| ToolOutcome {
                                tool_name,
                                successes,
                                failures,
                            })
                            .collect();
                    outcomes.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
                    // Preserve a copy for feedback notifications (survives turn reset)
                    // Feedback lands on a completed message, so a cancelled turn's partial outcomes must not replace them
                    if completed {
                        self.last_completed_turn_tool_outcomes = outcomes.clone();
                    }
                    delta.tool_outcomes_this_turn = outcomes;

                    let snapshot = TurnDeltaSnapshot {
                        current: self.signals.clone(),
                        delta,
                        start_prompt_mode: None,
                        end_prompt_mode: None,
                        // Stamped from `TurnSampling` post-snapshot (see turn.rs).
                        turn_input_tokens: 0,
                        turn_output_tokens: 0,
                        turn_cached_input_tokens: 0,
                        model_fingerprint: self.turn_model_fingerprint.take(),
                    };

                    // Store current as baseline for next delta
                    self.previous_turn_snapshot = Some(self.signals.clone());

                    let _ = respond_to.send(snapshot);
                }
                SignalEvent::GetLastTurnToolOutcomes(respond_to) => {
                    let _ = respond_to.send(self.last_completed_turn_tool_outcomes.clone());
                }

                // === Control Events ===
                SignalEvent::SeedCounts {
                    user_message_count,
                    assistant_message_count,
                    tool_call_count,
                    tools_used,
                    models_used,
                } => {
                    // Turn count equals user messages (each user prompt is a turn)
                    self.signals.turn_count = user_message_count;
                    self.signals.user_message_count = user_message_count;
                    self.signals.assistant_message_count = assistant_message_count;
                    self.signals.tool_call_count = tool_call_count;

                    // Restore distinct tools used
                    for tool in tools_used {
                        if self.tools_set.insert(tool.clone()) {
                            self.signals.tools_used.push(tool);
                        }
                    }

                    // Restore distinct models used
                    for model in models_used {
                        if self.models_set.insert(model.clone()) {
                            self.signals.models_used.push(model);
                        }
                    }

                    // Set previous_turn_snapshot so the first turn-end delta after seed_counts is computed correctly (not against baseline 0)
                    self.previous_turn_snapshot = Some(self.signals.clone());
                }
                SignalEvent::RestoreSignals(mut restored) => {
                    // Rebuild dedup sets from the restored signals
                    self.tools_set = restored.tools_used.iter().cloned().collect();
                    self.models_set = restored.models_used.iter().cloned().collect();

                    // TDigest is not serializable, so it will be.
                    // None after deserialization We keep it.
                    // None here; persisted itl_p50_ms/itl_p99_ms values are preserved update_session_itl_percentiles() won't overwrite them while digest is None.
                    restored.itl_digest = None;

                    // Back-compute ITL running sums from persisted mean and count compute_and_merge_turn_itl() then produces correct cumulative means when new intervals arrive itl_interval_count (number of individual inter-token.
                    // Each of the itl_sample_count responses contributes (chunks - 1) intervals NOTE: slightly lossy due to integer truncation.
                    let itl_n = restored
                        .total_chunk_count
                        .saturating_sub(restored.itl_sample_count as u64);
                    restored.itl_sum_ms = restored.itl_mean_ms.unwrap_or(0) * itl_n;
                    restored.itl_interval_count = itl_n;

                    tracing::info!(
                        turn_count = restored.turn_count,
                        tool_call_count = restored.tool_call_count,
                        error_count = restored.error_count,
                        "Restored session signals from persisted snapshot"
                    );

                    // Back-compute running latency sums from the persisted averages
                    // Subsequent update_latency_stats() calls then produce correct averages (the actor accumulates sums, not averages)
                    // NOTE: Integer division makes this slightly lossy (truncation error at most n per restore)
                    let n = restored.latency_sample_count as u64;
                    self.ttft_sum_ms = restored.avg_time_to_first_token_ms * n;
                    self.response_time_sum_ms = restored.avg_response_time_ms * n;

                    // Adjust session_start so that elapsed() continues from the persisted duration rather than restarting from 0
                    // Use checked_sub to avoid panic on Windows where Instant is based on QueryPerformanceCounter (time since boot)
                    // If the restored duration exceeds uptime the subtraction would underflow; fall back to now (resets elapsed to 0)
                    let duration = Duration::from_secs(restored.session_duration_seconds);
                    self.session_start = match Instant::now().checked_sub(duration) {
                        Some(t) => t,
                        None => {
                            tracing::warn!(
                                restored_duration_secs = restored.session_duration_seconds,
                                "Restored session duration exceeds system uptime; \
                                 resetting session_start to now"
                            );
                            Instant::now()
                        }
                    };

                    // Set previous_turn_snapshot so the first turn-end delta after restore is computed correctly (not against baseline 0)
                    self.previous_turn_snapshot = Some(restored.clone());
                    self.signals = restored;

                    // consecutive_cancellations tracks an in-progress frustration streak
                    // Restoring a non-zero value from a prior session would inflate frustration heuristics before the user has cancelled anything
                    self.signals.consecutive_cancellations = 0;
                }
                SignalEvent::GetSnapshot(respond_to) => {
                    // Update duration before sending snapshot
                    self.signals.session_duration_seconds = self.session_start.elapsed().as_secs();

                    // If the current turn has buffered intervals, merge them temporarily for accurate percentiles (without clearing the buffer)
                    if !self.turn_itl_intervals.is_empty() {
                        let temp_digest = TDigest::from_values(
                            self.turn_itl_intervals.iter().map(|&v| v as f64).collect(),
                        );
                        let combined_digest = match &self.signals.itl_digest {
                            None => temp_digest,
                            Some(existing) => existing.merge(&temp_digest),
                        };

                        self.signals.itl_p50_ms =
                            Some(combined_digest.estimate_quantile(0.50) as u64);
                        self.signals.itl_p99_ms =
                            Some(combined_digest.estimate_quantile(0.99) as u64);

                        // Also update max and mean with buffered data
                        let turn_max = *self.turn_itl_intervals.iter().max().unwrap();
                        self.signals.itl_max_ms = Some(
                            self.signals
                                .itl_max_ms
                                .map_or(turn_max, |old| old.max(turn_max)),
                        );

                        let turn_sum: u64 = self.turn_itl_intervals.iter().sum();
                        let temp_sum = self.signals.itl_sum_ms + turn_sum;
                        let total_count =
                            self.signals.itl_interval_count + self.turn_itl_intervals.len() as u64;
                        self.signals.itl_mean_ms = Some(temp_sum / total_count);
                    } else {
                        self.update_session_itl_percentiles();
                    }

                    let _ = respond_to.send(self.signals.clone());
                }
                SignalEvent::CheckAndMarkSync(respond_to) => {
                    let should_sync = match self.last_sync {
                        None => true,
                        Some(last) => last.elapsed() >= self.sync_interval,
                    };
                    if should_sync {
                        self.last_sync = Some(Instant::now());
                        self.signals.session_duration_seconds =
                            self.session_start.elapsed().as_secs();
                    }
                    let _ = respond_to.send(should_sync);
                }
                SignalEvent::Shutdown => {
                    break;
                }
            }
        }
    }

    /// Update TTFB/TTLB latency statistics (min/max/avg tracking).
    ///
    /// Shared by `RecordLatency` and `RecordInferenceMetrics` handlers.
    fn update_latency_stats(&mut self, time_to_first_token_ms: u64, total_response_time_ms: u64) {
        // Track min/max
        if self.signals.latency_sample_count == 0 {
            // First sample: initialize min/max
            self.signals.min_time_to_first_token_ms = time_to_first_token_ms;
            self.signals.max_time_to_first_token_ms = time_to_first_token_ms;
        } else {
            self.signals.min_time_to_first_token_ms = self
                .signals
                .min_time_to_first_token_ms
                .min(time_to_first_token_ms);
            self.signals.max_time_to_first_token_ms = self
                .signals
                .max_time_to_first_token_ms
                .max(time_to_first_token_ms);
        }

        // Accumulate for average calculation
        self.ttft_sum_ms += time_to_first_token_ms;
        self.response_time_sum_ms += total_response_time_ms;
        self.signals.latency_sample_count += 1;

        // Update averages
        let count = self.signals.latency_sample_count as u64;
        self.signals.avg_time_to_first_token_ms = self.ttft_sum_ms / count;
        self.signals.avg_response_time_ms = self.response_time_sum_ms / count;
    }

    /// Compute and merge inter-token latency stats for the current turn.
    ///
    /// Returns (p50, p99, max, mean) for this turn only.
    fn compute_and_merge_turn_itl(
        &mut self,
    ) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
        if self.turn_itl_intervals.is_empty() {
            return (None, None, None, None);
        }

        // Compute exact stats for this turn
        let mut sorted = self.turn_itl_intervals.clone();
        sorted.sort_unstable();

        let len = sorted.len();
        let (turn_p50, turn_p99, turn_max, turn_mean, turn_sum) = compute_percentiles(&sorted);

        // Update session-level tracking
        self.signals.itl_max_ms = Some(
            self.signals
                .itl_max_ms
                .map_or(turn_max, |old| old.max(turn_max)),
        );
        self.signals.itl_sum_ms += turn_sum;
        self.signals.itl_interval_count += len as u64;

        self.signals.itl_mean_ms = Some(self.signals.itl_sum_ms / self.signals.itl_interval_count);

        // Merge turn intervals into session TDigest
        let turn_digest =
            TDigest::from_values(self.turn_itl_intervals.iter().map(|&v| v as f64).collect());

        self.signals.itl_digest = Some(match &self.signals.itl_digest {
            None => turn_digest,
            Some(existing) => existing.merge(&turn_digest),
        });

        self.turn_itl_intervals.clear();

        (
            Some(turn_p50),
            Some(turn_p99),
            Some(turn_max),
            Some(turn_mean),
        )
    }

    /// Compute session-level ITL percentiles from TDigest.
    /// Called when syncing session signals or taking a snapshot.
    /// When `itl_digest` is `None`, the existing `itl_p50_ms`/`itl_p99_ms` values are kept.
    fn update_session_itl_percentiles(&mut self) {
        if let Some(digest) = &self.signals.itl_digest {
            self.signals.itl_p50_ms = Some(digest.estimate_quantile(0.50) as u64);
            self.signals.itl_p99_ms = Some(digest.estimate_quantile(0.99) as u64);
        }
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> SessionSignals {
        self.signals.clone()
    }
}

/// Spawn a new signals actor and return its handle.
pub fn spawn_signals_actor() -> SessionSignalsHandle {
    spawn_signals_actor_with_interval(Duration::from_secs(30))
}

pub fn spawn_signals_actor_with_interval(sync_interval: Duration) -> SessionSignalsHandle {
    let (handle, actor) = SessionSignalsActor::with_sync_interval(sync_interval);
    tokio::spawn(actor.run());
    handle
}

#[cfg(test)]
#[path = "signals_tests.rs"]
mod tests;
