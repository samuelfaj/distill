use super::*;
use crate::events::TelemetryEvent;

#[test]
fn event_names_are_stable() {
    assert_eq!(
        ActiveAgentMessageCompleted::NAME,
        "active_agent_message_completed"
    );
    assert_eq!(
        ActiveAgentMessageLimitHit::NAME,
        "active_agent_message_limit_hit"
    );
    assert_eq!(
        ActiveAgentMessageQuotaHit::NAME,
        "active_agent_message_quota_hit"
    );
    assert_eq!(
        ActiveAgentMessageSettled::NAME,
        "active_agent_message_settled"
    );
}

#[test]
fn immediate_outcomes_are_a_closed_content_free_contract() {
    let outcomes = [
        ActiveAgentMessageOutcome::Accepted,
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        ActiveAgentMessageOutcome::Saturated,
        ActiveAgentMessageOutcome::QuotaExceeded,
        ActiveAgentMessageOutcome::AdmissionUncertain,
        ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline,
        ActiveAgentMessageOutcome::Unsupported,
        ActiveAgentMessageOutcome::Invalid,
        ActiveAgentMessageOutcome::Limit,
        ActiveAgentMessageOutcome::ChannelClosed,
    ];
    assert_eq!(
        outcomes.map(|value| serde_json::to_value(value).expect("serialize outcome")),
        [
            "accepted",
            "not_found_or_not_owned",
            "not_active_or_finalizing",
            "saturated",
            "quota_exceeded",
            "admission_uncertain",
            "not_accepted_before_deadline",
            "unsupported",
            "invalid",
            "limit",
            "channel_closed",
        ],
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageCompleted {
            outcome: ActiveAgentMessageOutcome::Accepted,
            requested_operation: ActiveAgentMessageOperation::Steer,
            duration_ms: 7,
        })
        .expect("serialize completion"),
        serde_json::json!({
            "outcome": "accepted",
            "requested_operation": "steer",
            "duration_ms": 7,
        }),
    );
}

#[test]
fn operation_values_are_a_closed_snake_case_set() {
    let operations = [
        ActiveAgentMessageOperation::Queue,
        ActiveAgentMessageOperation::Steer,
        ActiveAgentMessageOperation::Interject,
    ];
    assert_eq!(
        operations.map(|value| serde_json::to_value(value).expect("serialize operation")),
        ["queue", "steer", "interject"],
    );
}

#[test]
fn limit_and_settlement_payloads_use_fixed_content_free_fields() {
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageQuotaHit {
            kind: ActiveAgentMessageQuotaKind::SenderTargetInFlight,
            limit: 4,
        })
        .expect("serialize quota"),
        serde_json::json!({"kind": "sender_target_in_flight", "limit": 4}),
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageLimitHit {
            max_bytes: 8_u64,
            observed_bytes: 9_u64,
        })
        .expect("serialize limit"),
        serde_json::json!({"max_bytes": 8, "observed_bytes": 9}),
    );

    let dispositions = [
        ActiveAgentMessageSettlementDisposition::Completed,
        ActiveAgentMessageSettlementDisposition::Failed,
        ActiveAgentMessageSettlementDisposition::Cancelled,
        ActiveAgentMessageSettlementDisposition::ReceiptClosed,
        ActiveAgentMessageSettlementDisposition::TimedOut,
        ActiveAgentMessageSettlementDisposition::AdmissionUncertain,
    ];
    assert_eq!(
        dispositions.map(|value| serde_json::to_value(value).expect("serialize disposition")),
        [
            "completed",
            "failed",
            "cancelled",
            "receipt_closed",
            "timed_out",
            "admission_uncertain",
        ],
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageSettled {
            disposition: ActiveAgentMessageSettlementDisposition::Completed,
            requested_operation: ActiveAgentMessageOperation::Steer,
            effective_operation: ActiveAgentMessageOperation::Steer,
            fallback_disposition: ActiveAgentMessageFallbackDisposition::NotApplicable,
            fallback_reason: None,
            safe_point_latency_ms: Some(3),
            safe_point_trigger: Some(ActiveAgentMessageSafePointTrigger::Natural),
            duration_ms: 11,
        })
        .expect("serialize settlement"),
        serde_json::json!({
            "disposition": "completed",
            "requested_operation": "steer",
            "effective_operation": "steer",
            "fallback_disposition": "not_applicable",
            "safe_point_latency_ms": 3,
            "safe_point_trigger": "natural",
            "duration_ms": 11,
        }),
    );
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageSettled {
            disposition: ActiveAgentMessageSettlementDisposition::Completed,
            requested_operation: ActiveAgentMessageOperation::Steer,
            effective_operation: ActiveAgentMessageOperation::Queue,
            fallback_disposition: ActiveAgentMessageFallbackDisposition::Queued,
            fallback_reason: Some(ActiveAgentMessageFallbackReason::Completion),
            safe_point_latency_ms: None,
            safe_point_trigger: None,
            duration_ms: 11,
        })
        .expect("serialize fallback settlement"),
        serde_json::json!({
            "disposition": "completed",
            "requested_operation": "steer",
            "effective_operation": "queue",
            "fallback_disposition": "queued",
            "fallback_reason": "completion",
            "duration_ms": 11,
        }),
    );
}

#[test]
fn safe_point_trigger_serializes_snake_case_and_omits_none() {
    let triggers = [
        ActiveAgentMessageSafePointTrigger::Natural,
        ActiveAgentMessageSafePointTrigger::WaitAbort,
    ];
    assert_eq!(
        triggers.map(|value| serde_json::to_value(value).expect("serialize trigger")),
        ["natural", "wait_abort"],
    );
    let settled = serde_json::to_value(ActiveAgentMessageSettled {
        disposition: ActiveAgentMessageSettlementDisposition::Completed,
        requested_operation: ActiveAgentMessageOperation::Interject,
        effective_operation: ActiveAgentMessageOperation::Interject,
        fallback_disposition: ActiveAgentMessageFallbackDisposition::NotApplicable,
        fallback_reason: None,
        safe_point_latency_ms: None,
        safe_point_trigger: None,
        duration_ms: 11,
    })
    .expect("serialize settlement");
    assert_eq!(None, settled.get("safe_point_trigger"));
}
