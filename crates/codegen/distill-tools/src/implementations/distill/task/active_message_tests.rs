// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::implementations::distill::task::coordinator::ActiveMessageAdmission;

#[test]
fn admission_lease_settlement_table_is_exact() {
    use ActiveMessageAdmission::{Admitted, ChannelClosed, Rejected, Unsupported};
    use ActiveMessageLeaseState::{Claimed, Committed, Open, Revoked};

    for (state, admission, is_settled, expected_state) in [
        (Committed, Admitted, true, Committed),
        (Revoked, Admitted, false, Revoked),
        (Open, Admitted, false, Open),
        (Claimed, Admitted, false, Claimed),
        (Open, Unsupported, true, Revoked),
        (Revoked, Unsupported, true, Revoked),
        (Claimed, Unsupported, false, Claimed),
        (Committed, Unsupported, false, Committed),
        (Open, Rejected, true, Revoked),
        (Revoked, Rejected, true, Revoked),
        (Claimed, Rejected, false, Claimed),
        (Committed, Rejected, false, Committed),
        (Open, ChannelClosed, true, Revoked),
        (Revoked, ChannelClosed, true, Revoked),
        (Claimed, ChannelClosed, false, Claimed),
        (Committed, ChannelClosed, false, Committed),
    ] {
        let lease = ActiveMessageAdmissionLease::from_state(state);
        assert_eq!(is_settled, lease.settle(admission));
        assert_eq!(expected_state, lease.state());
    }
}

#[test]
fn request_enforces_utf8_byte_cap() {
    let exact = "é".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES / 2);
    assert!(ActiveAgentMessageRequest::try_new("sub-1", exact).is_ok());

    let oversized = "é".repeat(MAX_ACTIVE_AGENT_MESSAGE_BYTES / 2 + 1);
    assert_eq!(
        ActiveAgentMessageRequest::try_new("sub-1", oversized).unwrap_err(),
        ActiveAgentMessageOutcome::Limit {
            max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
            observed_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES + 2,
        }
    );
    assert_eq!(
        ActiveAgentMessageRequest::try_new("sub-1", "").unwrap_err(),
        ActiveAgentMessageOutcome::Limit {
            max_bytes: MAX_ACTIVE_AGENT_MESSAGE_BYTES,
            observed_bytes: 0,
        }
    );
}

#[test]
fn quota_kind_serialization_and_schema_are_stable() {
    assert_eq!(
        serde_json::to_value(ActiveAgentMessageQuotaKind::SenderTargetInFlight).unwrap(),
        "sender_target_in_flight",
    );
    let schema = serde_json::to_value(schemars::schema_for!(ActiveAgentMessageQuotaKind)).unwrap();
    assert!(schema.to_string().contains("attempt_outbound"));
}

#[test]
fn try_new_defaults_to_queue_and_preserves_explicit_operation() {
    let queued = ActiveAgentMessageRequest::try_new("sub-1", "follow up").unwrap();
    assert_eq!(queued.operation(), ActiveAgentMessageOperation::Queue);
    for explicit in [
        ActiveAgentMessageOperation::Steer,
        ActiveAgentMessageOperation::Interject,
    ] {
        let request =
            ActiveAgentMessageRequest::try_new_with_operation("sub-1", "follow up", explicit)
                .unwrap();
        assert_eq!(request.operation(), explicit);
    }
}
