//! Content-free product telemetry for active messages between agents.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageOutcome {
    /// The child host admitted the message; it says nothing about whether the message later completed.
    Accepted,
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated,
    QuotaExceeded,
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Unsupported,
    /// The request was invalid without exceeding the configured byte cap.
    Invalid,
    /// The request exceeded the configured byte cap.
    Limit,
    ChannelClosed,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageOperation {
    Queue,
    Steer,
    Interject,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageCompleted {
    pub outcome: ActiveAgentMessageOutcome,
    pub requested_operation: ActiveAgentMessageOperation,
    pub duration_ms: u64,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageLimitHit {
    pub max_bytes: u64,
    pub observed_bytes: u64,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageQuotaKind {
    SenderTargetInFlight,
    AttemptOutbound,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageQuotaHit {
    pub kind: ActiveAgentMessageQuotaKind,
    pub limit: u64,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageSettlementDisposition {
    Completed,
    Failed,
    Cancelled,
    ReceiptClosed,
    TimedOut,
    AdmissionUncertain,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageFallbackDisposition {
    NotApplicable,
    Queued,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageFallbackReason {
    Idle,
    Completion,
    SoftCancel,
    Rewind,
}

/// How the safe point that delivered the message came about.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActiveAgentMessageSafePointTrigger {
    /// The turn reached a safe point on its own.
    Natural,
    /// A pending interject aborted a blocking wait tool to reach the safe point early.
    WaitAbort,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct ActiveAgentMessageSettled {
    pub disposition: ActiveAgentMessageSettlementDisposition,
    pub requested_operation: ActiveAgentMessageOperation,
    pub effective_operation: ActiveAgentMessageOperation,
    pub fallback_disposition: ActiveAgentMessageFallbackDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<ActiveAgentMessageFallbackReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_point_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_point_trigger: Option<ActiveAgentMessageSafePointTrigger>,
    pub duration_ms: u64,
}

#[cfg(test)]
#[path = "active_agent_message_tests.rs"]
mod tests;
