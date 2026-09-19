// Modified for Distill by Samuel Fajreldines, 2026.
//! Replay committed, handwritten bot-relay wire fixtures.
//!
//! These JSON files are the language-neutral artifact TS / Swift / Kotlin
//! clients will later replay. They are not produced by serializing the Rust
//! types. Harness locations: `fixtures/bot_relay/README.md`.

use serde_json::{Value, json};
use distill_tool_protocol::{
    BotBindConversationParams, BotCommandParams, BotEmptyResult, BotEventChannel, BotEventEnvelope,
    BotRelayError, BotRelayErrorCode, BotRelaySiblingAccount, BotRelaySignIn, BotRosterResult,
    BotStatusResult, BotSubscribeParams, BotTranscriptOffboxParams, BotTranscriptOffboxResult,
    BotVncDescriptorParams, BotVncDescriptorResult, COMMAND_REJECTED_AGENT_ID_MISMATCH,
    COMMAND_REJECTED_ARGS_INVALID, COMMAND_REJECTED_ARGS_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE, COMMAND_REJECTED_ATTACHMENT_NOT_FOUND,
    COMMAND_REJECTED_ATTACHMENT_NOT_READY, COMMAND_REJECTED_ATTACHMENT_TOO_LARGE,
    COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE, COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE,
    COMMAND_REJECTED_AUDIENCE_UNSUPPORTED, COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD, HubChannel,
    HubResyncRequiredEvent, HubTurnFinishedEvent,
};

const ERROR_IDENTITY_UNAVAILABLE: &str =
    include_str!("../fixtures/bot_relay/error_identity_unavailable.json");
const ERROR_LINK_REQUIRED: &str = include_str!("../fixtures/bot_relay/error_link_required.json");
const ERROR_LINK_REMOVED: &str = include_str!("../fixtures/bot_relay/error_link_removed.json");
const ERROR_CONSENT_REQUIRED: &str =
    include_str!("../fixtures/bot_relay/error_consent_required.json");
const ERROR_ENTERPRISE_UNSUPPORTED: &str =
    include_str!("../fixtures/bot_relay/error_enterprise_unsupported.json");
const ERROR_LEGACY_PRICING_UNSUPPORTED: &str =
    include_str!("../fixtures/bot_relay/error_legacy_pricing_unsupported.json");
const ERROR_EMAIL_UNVERIFIED: &str =
    include_str!("../fixtures/bot_relay/error_email_unverified.json");
const ERROR_LINK_CONFLICT: &str = include_str!("../fixtures/bot_relay/error_link_conflict.json");
const ERROR_LINK_CONFLICT_SIBLINGS: &str =
    include_str!("../fixtures/bot_relay/error_link_conflict_siblings.json");
const ERROR_CURSOR_ACCOUNT_UNAVAILABLE: &str =
    include_str!("../fixtures/bot_relay/error_cursor_account_unavailable.json");
const ERROR_LINK_UNSUPPORTED: &str =
    include_str!("../fixtures/bot_relay/error_link_unsupported.json");
const ERROR_LEGACY_PRIVACY_UNSUPPORTED: &str =
    include_str!("../fixtures/bot_relay/error_legacy_privacy_unsupported.json");
const ERROR_NO_PLAN: &str = include_str!("../fixtures/bot_relay/error_no_plan.json");
const ERROR_USAGE_EXHAUSTED: &str =
    include_str!("../fixtures/bot_relay/error_usage_exhausted.json");
const ERROR_BOX_MIGRATING: &str = include_str!("../fixtures/bot_relay/error_box_migrating.json");
const ERROR_BOX_RECREATING: &str = include_str!("../fixtures/bot_relay/error_box_recreating.json");
const ERROR_BOX_UNAVAILABLE: &str =
    include_str!("../fixtures/bot_relay/error_box_unavailable.json");
const ERROR_COMMAND_REJECTED: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected.json");
const ERROR_COMMAND_REJECTED_AGENT_ID_MISMATCH: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_agent_id_mismatch.json");
const ERROR_COMMAND_REJECTED_ARGS_TOO_LARGE: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_args_too_large.json");
const ERROR_COMMAND_REJECTED_ARGS_INVALID: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_args_invalid.json");
const ERROR_COMMAND_REJECTED_AUDIENCE_UNSUPPORTED: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_audience_unsupported.json");
const ERROR_COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE: &str = include_str!(
    "../fixtures/bot_relay/error_command_rejected_attachments_not_supported_in_live.json"
);
const ERROR_COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE: &str = include_str!(
    "../fixtures/bot_relay/error_command_rejected_attachment_credential_unavailable.json"
);
const ERROR_COMMAND_REJECTED_ATTACHMENT_NOT_FOUND: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_attachment_not_found.json");
const ERROR_COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_attachment_wrong_source.json");
const ERROR_COMMAND_REJECTED_ATTACHMENT_TOO_LARGE: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_attachment_too_large.json");
const ERROR_COMMAND_REJECTED_ATTACHMENT_NOT_READY: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_attachment_not_ready.json");
const ERROR_COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD: &str =
    include_str!("../fixtures/bot_relay/error_command_rejected_gateway_unknown_method.json");
const ERROR_COMPUTER_UNAVAILABLE: &str =
    include_str!("../fixtures/bot_relay/error_computer_unavailable.json");
const ERROR_UPSTREAM_ERROR: &str = include_str!("../fixtures/bot_relay/error_upstream_error.json");
const ERROR_UNKNOWN_CODE: &str = include_str!("../fixtures/bot_relay/error_unknown_code.json");
const EVENT_HUB_TURN_FINISHED: &str =
    include_str!("../fixtures/bot_relay/event_hub_turn_finished.json");
const EVENT_HUB_RESYNC_REQUIRED: &str =
    include_str!("../fixtures/bot_relay/event_hub_resync_required.json");
const EVENT_UPSTREAM_TRANSCRIPT: &str =
    include_str!("../fixtures/bot_relay/event_upstream_transcript.json");
const EVENT_WITH_EVENT_ID: &str = include_str!("../fixtures/bot_relay/event_with_event_id.json");
const EVENT_WITHOUT_EVENT_ID: &str =
    include_str!("../fixtures/bot_relay/event_without_event_id.json");
const EVENT_UNKNOWN_EXTRA_FIELD: &str =
    include_str!("../fixtures/bot_relay/event_unknown_extra_field.json");
const SEQUENCE_SEQ_GAP_NO_RESYNC: &str =
    include_str!("../fixtures/bot_relay/sequence_seq_gap_no_resync.json");
const SEQUENCE_REDELIVER_FRESH_SEQ: &str =
    include_str!("../fixtures/bot_relay/sequence_redeliver_fresh_seq.json");
const SEQUENCE_EXPLICIT_RESYNC: &str =
    include_str!("../fixtures/bot_relay/sequence_explicit_resync.json");
const SEQUENCE_SAME_SEQ_TWO_PAYLOADS: &str =
    include_str!("../fixtures/bot_relay/sequence_same_seq_two_payloads.json");
const METHOD_COMMAND_PARAMS: &str =
    include_str!("../fixtures/bot_relay/method_command_params.json");
const METHOD_COMMAND_RESULT: &str =
    include_str!("../fixtures/bot_relay/method_command_result.json");
const METHOD_VNC_DESCRIPTOR_PARAMS: &str =
    include_str!("../fixtures/bot_relay/method_vnc_descriptor_params.json");
const METHOD_VNC_DESCRIPTOR_RESULT: &str =
    include_str!("../fixtures/bot_relay/method_vnc_descriptor_result.json");
const METHOD_ROSTER_RESULT: &str = include_str!("../fixtures/bot_relay/method_roster_result.json");
const METHOD_STATUS_RESULT: &str = include_str!("../fixtures/bot_relay/method_status_result.json");
const METHOD_SUBSCRIBE_PARAMS: &str =
    include_str!("../fixtures/bot_relay/method_subscribe_params.json");
const METHOD_BIND_CONVERSATION_PARAMS: &str =
    include_str!("../fixtures/bot_relay/method_bind_conversation_params.json");
const METHOD_TRANSCRIPT_OFFBOX_PARAMS: &str =
    include_str!("../fixtures/bot_relay/method_transcript_offbox_params.json");
const METHOD_TRANSCRIPT_OFFBOX_RESULT: &str =
    include_str!("../fixtures/bot_relay/method_transcript_offbox_result.json");
const METHOD_EMPTY_RESULT: &str = include_str!("../fixtures/bot_relay/method_empty_result.json");

fn parse_object(raw: &str) -> Value {
    let value: Value = serde_json::from_str(raw).unwrap_or_else(|e| {
        panic!("fixture is not JSON: {e}\n{raw}");
    });
    assert!(value.is_object(), "fixture must be a JSON object: {raw}");
    value
}

fn replay_error(raw: &str) -> (Value, BotRelayError) {
    let value = parse_object(raw);
    let parsed: BotRelayError = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("BotRelayError rejected fixture: {e}\n{value}"));
    (value, parsed)
}

fn replay_envelope(raw: &str) -> (Value, BotEventEnvelope) {
    let value = parse_object(raw);
    let parsed: BotEventEnvelope = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("BotEventEnvelope rejected fixture: {e}\n{value}"));
    (value, parsed)
}

fn assert_error(
    raw: &str,
    wire_code: &str,
    retryable: bool,
    detail: Value,
    reason: Option<&str>,
    expected: BotRelayErrorCode,
) -> BotRelayError {
    let (wire, err) = replay_error(raw);
    assert_eq!(wire["code"], wire_code);
    assert_eq!(wire["retryable"], retryable);
    assert_eq!(wire["detail"], detail);
    match reason {
        Some(r) => assert_eq!(wire["reason"], r),
        None => assert!(
            wire.get("reason").is_none(),
            "reason must be absent on {wire_code}, got {wire}"
        ),
    }
    assert_eq!(err.code, expected);
    assert_eq!(err.retryable, retryable);
    assert_eq!(err.reason.as_deref(), reason);
    err
}

#[test]
fn handwritten_error_fixtures() {
    struct Case {
        code: BotRelayErrorCode,
        raw: &'static str,
        retryable: bool,
        detail: Value,
        reason: Option<&'static str>,
        detail_upstream: Option<&'static str>,
    }
    let cases = [
        Case {
            code: BotRelayErrorCode::IdentityUnavailable,
            raw: ERROR_IDENTITY_UNAVAILABLE,
            retryable: true,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LinkRequired,
            raw: ERROR_LINK_REQUIRED,
            retryable: false,
            detail: json!({}),
            reason: Some("no_link"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LinkRemoved,
            raw: ERROR_LINK_REMOVED,
            retryable: false,
            detail: json!({}),
            reason: Some("unlinked"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::ConsentRequired,
            raw: ERROR_CONSENT_REQUIRED,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_consent_required"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::EnterpriseUnsupported,
            raw: ERROR_ENTERPRISE_UNSUPPORTED,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_enterprise_member"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LegacyPricingUnsupported,
            raw: ERROR_LEGACY_PRICING_UNSUPPORTED,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_legacy_pricing"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::EmailUnverified,
            raw: ERROR_EMAIL_UNVERIFIED,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_email_unverified"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LinkConflict,
            raw: ERROR_LINK_CONFLICT,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_link_declined"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::CursorAccountUnavailable,
            raw: ERROR_CURSOR_ACCOUNT_UNAVAILABLE,
            retryable: false,
            detail: json!({}),
            reason: Some("user_missing"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LinkUnsupported,
            raw: ERROR_LINK_UNSUPPORTED,
            retryable: false,
            detail: json!({}),
            reason: Some("jit_some_future_rule"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::LegacyPrivacyUnsupported,
            raw: ERROR_LEGACY_PRIVACY_UNSUPPORTED,
            retryable: false,
            detail: json!({}),
            reason: Some("personal"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::NoPlan,
            raw: ERROR_NO_PLAN,
            retryable: false,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::UsageExhausted,
            raw: ERROR_USAGE_EXHAUSTED,
            retryable: false,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::BoxMigrating,
            raw: ERROR_BOX_MIGRATING,
            retryable: true,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::BoxRecreating,
            raw: ERROR_BOX_RECREATING,
            retryable: true,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::BoxUnavailable,
            raw: ERROR_BOX_UNAVAILABLE,
            retryable: true,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::CommandRejected,
            raw: ERROR_COMMAND_REJECTED,
            retryable: false,
            detail: json!({}),
            reason: Some("not_yet_enabled"),
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::ComputerUnavailable,
            raw: ERROR_COMPUTER_UNAVAILABLE,
            retryable: true,
            detail: json!({}),
            reason: None,
            detail_upstream: None,
        },
        Case {
            code: BotRelayErrorCode::UpstreamError,
            raw: ERROR_UPSTREAM_ERROR,
            retryable: false,
            detail: json!({"upstream": "upstream rejected the request"}),
            reason: None,
            detail_upstream: Some("upstream rejected the request"),
        },
    ];
    assert_eq!(cases.len(), BotRelayErrorCode::ALL.len());
    for case in &cases {
        assert!(
            BotRelayErrorCode::ALL.contains(&case.code),
            "case {} is not in ALL",
            case.code
        );
        let err = assert_error(
            case.raw,
            case.code.as_str(),
            case.retryable,
            case.detail.clone(),
            case.reason,
            case.code,
        );
        assert_eq!(err.detail.upstream.as_deref(), case.detail_upstream);
    }
    for code in BotRelayErrorCode::ALL {
        assert!(
            cases.iter().any(|c| c.code == *code),
            "no handwritten fixture for {code}"
        );
    }
}

#[test]
fn handwritten_agent_id_mismatch_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_AGENT_ID_MISMATCH,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_AGENT_ID_MISMATCH),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_args_too_large_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_ARGS_TOO_LARGE,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_ARGS_TOO_LARGE),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_args_invalid_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_ARGS_INVALID,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_ARGS_INVALID),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_audience_unsupported_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_AUDIENCE_UNSUPPORTED,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_AUDIENCE_UNSUPPORTED),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_attachments_not_supported_in_live_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_ATTACHMENTS_NOT_SUPPORTED_IN_LIVE),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_attach_upload_reject_reasons() {
    for (raw, reason) in [
        (
            ERROR_COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE,
            COMMAND_REJECTED_ATTACHMENT_CREDENTIAL_UNAVAILABLE,
        ),
        (
            ERROR_COMMAND_REJECTED_ATTACHMENT_NOT_FOUND,
            COMMAND_REJECTED_ATTACHMENT_NOT_FOUND,
        ),
        (
            ERROR_COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE,
            COMMAND_REJECTED_ATTACHMENT_WRONG_SOURCE,
        ),
        (
            ERROR_COMMAND_REJECTED_ATTACHMENT_TOO_LARGE,
            COMMAND_REJECTED_ATTACHMENT_TOO_LARGE,
        ),
        (
            ERROR_COMMAND_REJECTED_ATTACHMENT_NOT_READY,
            COMMAND_REJECTED_ATTACHMENT_NOT_READY,
        ),
    ] {
        let err = assert_error(
            raw,
            "command_rejected",
            false,
            json!({}),
            Some(reason),
            BotRelayErrorCode::CommandRejected,
        );
        assert_eq!(err.detail.upstream, None);
    }
}

#[test]
fn handwritten_gateway_unknown_method_reason() {
    let err = assert_error(
        ERROR_COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD,
        "command_rejected",
        false,
        json!({}),
        Some(COMMAND_REJECTED_GATEWAY_UNKNOWN_METHOD),
        BotRelayErrorCode::CommandRejected,
    );
    assert_eq!(err.detail.upstream, None);
}

#[test]
fn handwritten_link_conflict_siblings() {
    let (wire, err) = replay_error(ERROR_LINK_CONFLICT_SIBLINGS);
    assert_eq!(wire["code"], "link_conflict");
    assert_eq!(wire["reason"], "jit_link_declined");
    assert_eq!(
        wire["detail"]["siblingAccounts"].as_array().map(Vec::len),
        Some(3)
    );
    assert_eq!(err.code, BotRelayErrorCode::LinkConflict);
    assert_eq!(err.reason.as_deref(), Some("jit_link_declined"));
    assert_eq!(err.detail.upstream, None);
    let siblings = err
        .detail
        .sibling_accounts
        .as_deref()
        .expect("siblingAccounts");
    assert_eq!(
        siblings,
        [
            BotRelaySiblingAccount {
                sign_in: BotRelaySignIn::X,
                handle: Some("grokfan".to_owned()),
                created_at_ms: 1_699_920_000_000,
                linked: true,
            },
            BotRelaySiblingAccount {
                sign_in: BotRelaySignIn::Apple,
                handle: None,
                created_at_ms: 1_717_200_000_000,
                linked: false,
            },
            BotRelaySiblingAccount {
                sign_in: BotRelaySignIn::Other,
                handle: None,
                created_at_ms: 0,
                linked: false,
            },
        ]
    );
    assert_eq!(siblings.iter().filter(|s| s.linked).count(), 1);
    let reserialized = serde_json::to_value(&err).unwrap();
    assert_eq!(
        reserialized["detail"]["siblingAccounts"][0],
        wire["detail"]["siblingAccounts"][0]
    );
    assert_eq!(
        reserialized["detail"]["siblingAccounts"][1],
        wire["detail"]["siblingAccounts"][1]
    );
    assert_eq!(
        reserialized["detail"]["siblingAccounts"][2]["signIn"],
        "other"
    );
    for sign_in in BotRelaySignIn::ALL {
        assert_eq!(BotRelaySignIn::from_wire(sign_in.as_str()), *sign_in);
    }
}

#[test]
fn unknown_error_code_degrades_to_upstream_error() {
    let (wire, err) = replay_error(ERROR_UNKNOWN_CODE);
    assert_eq!(wire["code"], "some_future_code");
    assert_ne!(wire["code"], "upstream_error");
    assert_eq!(wire["retryable"], true);
    assert_eq!(wire["detail"], json!({"upstream": "preserve me"}));
    assert!(wire.get("reason").is_none());
    assert_eq!(err.code, BotRelayErrorCode::UpstreamError);
    assert!(err.retryable);
    assert_eq!(err.detail.upstream.as_deref(), Some("preserve me"));
    assert_eq!(err.reason, None);
}

// Wire schema: `preview` is a required string and may be non-empty.
// The hub emitter always sends ""; this fixture proves the field is not
// constrained to empty.
#[test]
fn hub_turn_finished_envelope() {
    let (wire, env) = replay_envelope(EVENT_HUB_TURN_FINISHED);
    assert_eq!(wire["v"], 1);
    assert_eq!(wire["agentId"], "agt_1");
    assert_eq!(wire["seq"], 2);
    assert_eq!(wire["channel"], "hub:turn_finished");
    assert_eq!(wire["event"]["agentId"], "agt_1");
    assert_eq!(wire["event"]["conversationIds"], json!(["conv_1"]));
    assert_eq!(wire["event"]["preview"], "done");
    assert!(wire.get("eventId").is_none());

    assert_eq!(env.v, 1);
    assert_eq!(env.agent_id, "agt_1");
    assert_eq!(env.seq, 2);
    assert_eq!(env.channel, HubChannel::TurnFinished.into());
    assert_eq!(env.event_id, None);
    let body: HubTurnFinishedEvent = serde_json::from_value(env.event).unwrap();
    assert_eq!(body.agent_id, "agt_1");
    assert_eq!(body.conversation_ids, vec!["conv_1".to_owned()]);
    assert_eq!(body.preview, "done");
}

#[test]
fn hub_resync_required_envelope() {
    let (wire, env) = replay_envelope(EVENT_HUB_RESYNC_REQUIRED);
    assert_eq!(wire["v"], 1);
    assert_eq!(wire["channel"], "hub:resync_required");
    assert_eq!(wire["event"]["agentId"], "agt_1");
    assert!(wire.get("eventId").is_none());

    assert_eq!(env.channel, HubChannel::ResyncRequired.into());
    assert_eq!(env.seq, 3);
    assert_eq!(env.event_id, None);
    let body: HubResyncRequiredEvent = serde_json::from_value(env.event).unwrap();
    assert_eq!(body.agent_id, "agt_1");
}

#[test]
fn upstream_verbatim_channel_envelope() {
    let (wire, env) = replay_envelope(EVENT_UPSTREAM_TRANSCRIPT);
    assert_eq!(wire["channel"], "transcript");
    assert_eq!(wire["event"]["kind"], "entry");
    assert_eq!(wire["event"]["id"], "e1");
    assert!(matches!(env.channel, BotEventChannel::Upstream(_)));
    assert_eq!(env.channel.as_str(), "transcript");
    assert_eq!(env.seq, 4);
}

#[test]
fn envelope_with_event_id() {
    let (wire, env) = replay_envelope(EVENT_WITH_EVENT_ID);
    assert_eq!(wire["eventId"], "evt_9");
    assert_eq!(env.event_id.as_deref(), Some("evt_9"));
    assert_eq!(env.seq, 5);
}

#[test]
fn envelope_without_event_id() {
    let (wire, env) = replay_envelope(EVENT_WITHOUT_EVENT_ID);
    assert!(
        !wire.as_object().unwrap().contains_key("eventId"),
        "without-eventId fixture must omit the key, not send null"
    );
    assert_eq!(env.event_id, None);
    assert_eq!(env.seq, 6);
}

#[test]
fn unknown_extra_field_is_ignored() {
    let (wire, env) = replay_envelope(EVENT_UNKNOWN_EXTRA_FIELD);
    assert_eq!(
        wire["futureField"], "must-ignore",
        "fixture must carry the extra field so deny_unknown_fields would fail"
    );
    assert_eq!(env.agent_id, "agt_1");
    assert_eq!(env.seq, 7);
    assert_eq!(env.channel.as_str(), "transcript");
    assert_eq!(env.event_id, None);
}

struct SequenceFixture {
    expected_resync_count: u64,
    expected_distinct_events: u64,
    frames: Vec<BotEventEnvelope>,
}

fn replay_sequence(raw: &str) -> SequenceFixture {
    let value: Value = serde_json::from_str(raw).expect("sequence fixture is JSON");
    let expected_resync_count = value["expectedResyncCount"]
        .as_u64()
        .expect("sequence fixture must set expectedResyncCount");
    let expected_distinct_events = value["expectedDistinctEvents"]
        .as_u64()
        .expect("sequence fixture must set expectedDistinctEvents");
    let frames = value["frames"]
        .as_array()
        .expect("sequence fixture must have frames");
    let parsed: Vec<BotEventEnvelope> = frames
        .iter()
        .map(|f| {
            serde_json::from_value(f.clone())
                .unwrap_or_else(|e| panic!("sequence frame rejected: {e}\n{f}"))
        })
        .collect();
    SequenceFixture {
        expected_resync_count,
        expected_distinct_events,
        frames: parsed,
    }
}

/// Test-side model of a conforming receive path. Language harnesses must
/// reproduce these observations rather than re-count tags / `frames.len()`.
///
/// - Every ingested frame is one observed event (`seq` is not a dedupe key).
/// - A resync is observed only on an explicit `hub:resync_required`.
struct ReferenceConsumer {
    observed_events: u64,
    observed_resyncs: u64,
}

impl ReferenceConsumer {
    fn new() -> Self {
        Self {
            observed_events: 0,
            observed_resyncs: 0,
        }
    }

    fn ingest(&mut self, frame: &BotEventEnvelope) {
        if frame.channel == HubChannel::ResyncRequired.into() {
            self.observed_resyncs += 1;
        }
        self.observed_events += 1;
    }
}

fn assert_sequence_outcomes(seq: &SequenceFixture) {
    let mut consumer = ReferenceConsumer::new();
    for frame in &seq.frames {
        consumer.ingest(frame);
    }
    assert_eq!(consumer.observed_resyncs, seq.expected_resync_count);
    assert_eq!(consumer.observed_events, seq.expected_distinct_events);
}

#[test]
fn seq_gap_without_resync_is_not_a_resync() {
    let seq = replay_sequence(SEQUENCE_SEQ_GAP_NO_RESYNC);
    assert_eq!(seq.frames[0].seq, 1);
    assert_eq!(seq.frames[1].seq, 4);
    assert_eq!(seq.expected_resync_count, 0);
    assert_sequence_outcomes(&seq);
}

#[test]
fn redelivered_payload_with_fresh_seq_is_two_events() {
    let seq = replay_sequence(SEQUENCE_REDELIVER_FRESH_SEQ);
    assert_eq!(seq.frames[0].event, seq.frames[1].event);
    assert_ne!(seq.frames[0].seq, seq.frames[1].seq);
    assert_eq!(seq.expected_distinct_events, 2);
    assert_sequence_outcomes(&seq);
}

#[test]
fn same_seq_two_payloads_are_two_events() {
    let seq = replay_sequence(SEQUENCE_SAME_SEQ_TWO_PAYLOADS);
    assert_eq!(seq.frames[0].seq, seq.frames[1].seq);
    assert_ne!(seq.frames[0].event, seq.frames[1].event);
    assert_eq!(seq.expected_distinct_events, 2);
    assert_sequence_outcomes(&seq);
}

#[test]
fn explicit_resync_required_is_observed() {
    let seq = replay_sequence(SEQUENCE_EXPLICIT_RESYNC);
    assert_eq!(seq.frames[1].channel, HubChannel::ResyncRequired.into());
    assert_eq!(seq.expected_resync_count, 1);
    assert_eq!(seq.expected_distinct_events, 2);
    assert_sequence_outcomes(&seq);
}

#[test]
fn method_command_params_and_result() {
    let params: BotCommandParams =
        serde_json::from_str(METHOD_COMMAND_PARAMS).expect("command params");
    assert_eq!(params.agent_id, "agt_1");
    assert_eq!(params.name, "sendPrompt");
    assert_eq!(params.args, json!({"prompt": "hello"}));

    let result: Value = serde_json::from_str(METHOD_COMMAND_RESULT).expect("command result");
    assert_eq!(result, json!({"accepted": true}));
}

#[test]
fn method_vnc_descriptor_params_require_agent_id() {
    let params: BotVncDescriptorParams =
        serde_json::from_str(METHOD_VNC_DESCRIPTOR_PARAMS).expect("vnc params");
    assert_eq!(params.agent_id, "agt_1");
}

#[test]
fn method_vnc_descriptor_null_expires_hint() {
    let wire = parse_object(METHOD_VNC_DESCRIPTOR_RESULT);
    assert_eq!(wire["expiresHint"], Value::Null);
    let result: BotVncDescriptorResult =
        serde_json::from_value(wire).expect("vnc descriptor result");
    assert_eq!(result.vnc_url, "https://example.invalid/vnc");
    assert_eq!(result.expires_hint, None);
}

#[test]
fn method_roster_status_subscribe_bind() {
    let roster: BotRosterResult = serde_json::from_str(METHOD_ROSTER_RESULT).expect("roster");
    assert_eq!(roster.agents.len(), 2);
    assert_eq!(roster.agents[0].agent_id, "agt_1");
    assert_eq!(roster.agents[0].last_turn_at, Some(1_700_000_123_000));
    assert!(roster.agents[0].viewer_is_owner);
    assert_eq!(roster.agents[1].agent_id, "agt_2");
    assert!(!roster.agents[1].viewer_is_owner);
    assert_eq!(Some(1_700_000_200_000), roster.remembered_at_ms);

    let status: BotStatusResult = serde_json::from_str(METHOD_STATUS_RESULT).expect("status");
    assert_eq!(status.run_state, distill_tool_protocol::BotRunState::Hibernated);

    let sub: BotSubscribeParams = serde_json::from_str(METHOD_SUBSCRIBE_PARAMS).expect("subscribe");
    assert_eq!(sub.agent_ids, ["agt_a", "agt_b"]);
    assert!(
        !sub.full_fidelity,
        "omitted fullFidelity must default false; a Swift null would fail this fixture"
    );

    let bind: BotBindConversationParams =
        serde_json::from_str(METHOD_BIND_CONVERSATION_PARAMS).expect("bind");
    assert_eq!(bind.conversation_id, "conv_1");
    assert_eq!(bind.primary, "agt_a");

    let empty: BotEmptyResult = serde_json::from_str(METHOD_EMPTY_RESULT).expect("empty");
    let _ = empty;
}

#[test]
fn method_transcript_offbox() {
    let params: BotTranscriptOffboxParams =
        serde_json::from_str(METHOD_TRANSCRIPT_OFFBOX_PARAMS).expect("transcript params");
    assert_eq!(params.agent_id, "agt_1");
    assert_eq!(params.cursor.as_deref(), Some("c_1"));

    let result: BotTranscriptOffboxResult =
        serde_json::from_str(METHOD_TRANSCRIPT_OFFBOX_RESULT).expect("transcript result");
    assert_eq!(result.entries, json!([{"id": "e1"}]));
    assert_eq!(result.next_cursor.as_deref(), Some("c_2"));
}
