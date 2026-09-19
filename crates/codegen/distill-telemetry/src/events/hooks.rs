//! Hook lifecycle product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    Success,
    Error,
    Blocked,
}

/// Outcome of one `PreToolUse` gate callback.
/// Only `Denied` blocks the tool; the rest (including the `TimedOut`/`TransportError`/`Malformed`/`UnknownDecision` fail-open paths) let it run.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ClientHookGateOutcome {
    Denied,
    Proceeded,
    TimedOut,
    TransportError,
    Malformed,
    UnknownDecision,
}

#[derive(Serialize)]
pub struct HookAdded {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookRemoved {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookTrusted {
    pub success: bool,
}

#[derive(Serialize)]
pub struct HookExecuted {
    pub hook_name: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub duration_ms: u64,
    pub outcome: HookOutcome,
}

#[derive(Serialize)]
pub struct HookBlocked {
    pub hook_name: String,
    pub cause: HookBlockCause,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum HookBlockCause {
    Denied,
    /// A `PreToolUse` `updatedInput` that could not be applied.
    UnusableRewrite,
    /// A `Stop`/`SubagentStop` hook blocked the agent from stopping.
    StopBlocked,
    /// A `UserPromptSubmit` hook blocked the prompt before the turn started.
    PromptBlocked,
}

/// Per-callback outcome of a `PreToolUse` gate.
/// A deny returns early, so callbacks still pending at that point are not logged.
#[derive(Serialize)]
pub struct ClientHookGate {
    pub callback_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub outcome: ClientHookGateOutcome,
    pub duration_ms: u64,
}
