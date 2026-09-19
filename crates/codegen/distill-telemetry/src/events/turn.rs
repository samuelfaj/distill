//! Turn and cancellation product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Outcome {
    Completed,
    Cancelled,
    Error,
}

#[derive(Serialize)]
pub struct TurnCompleted {
    pub outcome: Outcome,
    pub duration_ms: u64,
    pub tool_call_count: u32,
    pub model_id: String,
    /// External-stream-only `session.id` (`#[serde(skip)]`); lets an emit outside the ambient
    /// `TelemetryCtx` carry it. `None` falls back to the task-local ctx.
    #[serde(skip)]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_tokens: Option<u64>,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CancellationScope {
    Turn,
    Compaction,
}

#[derive(Serialize, Clone, Copy)]
pub struct CancellationCompleted {
    pub latency_ms: u64,
    pub scope: CancellationScope,
}

/// Model issued a shell tool call whose command is `true` (keepalive thrash signal).
#[derive(Serialize)]
pub struct ShellTrueNoop {
    pub tool_name: String,
}

/// Harness nudged the model to break a run of identical tool calls. Pairs with [`ActionStationarityStop`]: the nudge
/// fires first and once per run, the stop only if the run continues to the hard limit. `problematically_repeating` splits
/// the two threshold tiers (tools whose identical repeats are never productive versus everything else).
#[derive(Serialize)]
pub struct ActionStationarityNudge {
    pub problematically_repeating: bool,
    pub run_len: u32,
    pub tool_name: String,
}

/// Harness hard-stopped a turn after identical tool thrash (silent EndTurn).
#[derive(Serialize)]
pub struct ActionStationarityStop {
    pub true_noop: bool,
    pub problematically_repeating: bool,
    pub run_len: u32,
    pub tool_name: String,
}
