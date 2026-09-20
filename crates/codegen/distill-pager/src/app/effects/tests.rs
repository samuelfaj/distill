// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
fn nth<T>(xs: &[T], i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}
/// Test-only key lookup over either a JSON object or a bare `Map`.
trait JsonKeyed {
    fn key(&self, key: &str) -> Option<&serde_json::Value>;
    fn describe(&self) -> String;
}
impl JsonKeyed for serde_json::Value {
    fn key(&self, key: &str) -> Option<&serde_json::Value> {
        self.get(key)
    }
    fn describe(&self) -> String {
        self.to_string()
    }
}
impl JsonKeyed for serde_json::Map<String, serde_json::Value> {
    fn key(&self, key: &str) -> Option<&serde_json::Value> {
        self.get(key)
    }
    fn describe(&self) -> String {
        serde_json::Value::Object(self.clone()).to_string()
    }
}
impl<T: JsonKeyed + ?Sized> JsonKeyed for &T {
    fn key(&self, key: &str) -> Option<&serde_json::Value> {
        (**self).key(key)
    }
    fn describe(&self) -> String {
        (**self).describe()
    }
}
fn j<'a, V: JsonKeyed + ?Sized>(v: &'a V, key: &str) -> &'a serde_json::Value {
    let Some(got) = v.key(key) else {
        panic!("missing json key {key}: {}", v.describe());
    };
    got
}
use std::path::PathBuf;
use actions::ProbedAttachment;
use distill_shell::extensions::billing::{BillingConfig, Cent, UsagePeriod};
/// The invalid-params server detail survives `attach_prompt_usage` wrapping `error.data` as `{message, promptUsage}`.
#[test]
fn format_acp_error_reads_detail_from_wrapped_data() {
    let bare = acp::Error::invalid_params().data("model does not support tools");
    assert_eq!(format_acp_error(&bare, false), "model does not support tools");
    let wrapped = acp::Error::invalid_params()
        .data(
            serde_json::json!({
            "message": "model does not support tools",
            "promptUsage": { "inputTokens": 12, "outputTokens": 0, "numTurns": 1 }
        }),
        );
    assert_eq!(format_acp_error(&wrapped, false), "model does not support tools");
}
#[test]
fn format_acp_error_formats_http_500_dump() {
    let err = acp::Error::internal_error()
        .data(
            serde_json::json!({
            "message": "API error (status 500 Internal Server Error): {\"error\":\"upstream exploded\"}",
            "http_status": 500
        }),
        );
    assert_eq!(
            format_acp_error(&err, false),
            "Server error (500): Something went wrong on our side. Wait a minute and send again."
        );
}
/// The typed kind in `error.data` (the shell's `terminal_error_data` shape) picks the truncation copy.
/// The placeholder message proves the typed extraction, not the text fallback, chose it.
#[test]
fn format_acp_error_typed_truncation_kind_renders_truncation_copy() {
    let err = acp::Error::internal_error()
        .data(
            serde_json::json!({
            "message": "turn ended early",
            "error_kind": "max_tokens_truncation"
        }),
        );
    assert_eq!(
            format_acp_error(&err, false),
            "Response truncated: turn ended early"
        );
    let unknown = acp::Error::internal_error()
        .data(
            serde_json::json!({
            "message": "a future failure quoting: response truncated by max_tokens",
            "error_kind": "a_future_kind"
        }),
        );
    assert_eq!(
            format_acp_error(&unknown, false),
            "Request failed: a future failure quoting: response truncated by max_tokens. Try sending again."
        );
}
#[test]
fn format_acp_error_rate_limit_surfaces_detail_or_fallback() {
    use distill_shell::sampling::error::{
        FREE_USAGE_USER_MESSAGE, RATE_LIMITED_ERROR_CODE,
        RATE_LIMITED_USER_MESSAGE_API_KEY, RATE_LIMITED_USER_MESSAGE_OAUTH,
    };
    let cap_body = "The service is temporarily at capacity. Please retry your request shortly.";
    let capacity = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited")
        .data(format!(
            "API error (status 429 Too Many Requests): {cap_body}"
        ));
    assert_eq!(format_acp_error(&capacity, false), cap_body);
    assert_eq!(format_acp_error(&capacity, true), cap_body);
    let rpm_body = "You are sending requests too quickly. Please slow down, or upgrade to a Grok subscription for higher limits: https://grok.com/supergrok";
    let rpm = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited")
        .data(format!("API error (status 429 Too Many Requests): {rpm_body}"));
    assert!(format_acp_error(&rpm, false).contains("grok.com/supergrok"));
    assert_eq!(format_acp_error(&rpm, true), RATE_LIMITED_USER_MESSAGE_API_KEY);
    let empty = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited");
    assert_eq!(format_acp_error(&empty, false), RATE_LIMITED_USER_MESSAGE_OAUTH);
    assert_eq!(format_acp_error(&empty, true), RATE_LIMITED_USER_MESSAGE_API_KEY);
    let free = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited")
        .data(
            "API error (status 429 Too Many Requests): \
             subscription:free-usage-exhausted: You have used all your free usage.",
        );
    assert_eq!(format_acp_error(&free, false), FREE_USAGE_USER_MESSAGE);
    assert_eq!(format_acp_error(&free, true), FREE_USAGE_USER_MESSAGE);
    let free_wrapped = acp::Error::new(RATE_LIMITED_ERROR_CODE, "Rate limited")
        .data(
            serde_json::json!({
                "message": "API error (status 429 Too Many Requests): \
                    subscription:free-usage-exhausted: You have used all your free usage.",
                "promptUsage": { "inputTokens": 12, "outputTokens": 0, "numTurns": 1 }
            }),
        );
    assert_eq!(
            format_acp_error(&free_wrapped, false),
            FREE_USAGE_USER_MESSAGE
        );
}
/// Non-empty token ranges ride the wire block meta as `skillTokenRanges` byte pairs; the text itself is untouched.
#[test]
fn plain_prompt_block_stamps_skill_token_ranges_meta() {
    let block = plain_prompt_content_block("great /pr-workflow go".into(), &[6..18]);
    let acp::ContentBlock::Text(tb) = block else {
        panic!("expected text block");
    };
    assert_eq!(tb.text, "great /pr-workflow go");
    let meta = tb.meta.expect("meta stamped when ranges non-empty");
    assert_eq!(j(&meta, "skillTokenRanges"), &serde_json::json!([[6, 18]]));
}
/// Empty ranges keep `meta: None`; the legacy wire shape is unchanged.
#[test]
fn plain_prompt_block_no_meta_when_ranges_empty() {
    let block = plain_prompt_content_block("hello".into(), &[]);
    let acp::ContentBlock::Text(tb) = block else {
        panic!("expected text block");
    };
    assert_eq!(tb.text, "hello");
    assert!(tb.meta.is_none());
}
/// With a screen mode, `_meta` carries both `promptId` and `screenMode` (the shell threads the latter into `prompt_submitted.screen_mode`).
#[test]
fn prompt_request_meta_stamps_screen_mode() {
    let meta = prompt_request_meta("p-1", Some("minimal"));
    assert_eq!(
            meta,
            serde_json::json!({ "promptId": "p-1", "screenMode": "minimal" })
        );
}
/// Without a screen mode (`SessionFlags::default()` in tests), the key is omitted; the legacy `{"promptId": …}` wire shape stays byte-identical.
#[test]
fn prompt_request_meta_omits_screen_mode_when_unset() {
    let meta = prompt_request_meta("p-2", None);
    assert_eq!(meta, serde_json::json!({ "promptId": "p-2" }));
}
/// Text-only interjections must omit the `content` key entirely; the legacy `x.ai/interject` wire shape stays byte-identical.
#[test]
fn interject_params_omit_content_when_no_blocks() {
    let sid = acp::SessionId::new("s1");
    let params = build_interject_params(&sid, "steer", "i1", None);
    let obj = params.as_object().unwrap();
    assert!(!obj.contains_key("content"), "content key must be absent");
    assert_eq!(j(&obj, "sessionId"), "s1");
    assert_eq!(j(&obj, "text"), "steer");
    assert_eq!(j(&obj, "interjectionId"), "i1");
    assert_eq!(obj.len(), 3, "no extra keys on the legacy shape");
}
/// The agent serializes `ExtMethodResult<KillTaskResponse>`: the outcome lives at `result.outcome`.
/// Probing the top level instead was why the tasks-pane ✗ never removed stale (`not_found`) rows after a session resume.
#[test]
fn parse_kill_outcome_reads_result_envelope() {
    use distill_tools::types::KillOutcome;
    let resp = r#"{"result":{"taskId":"t-1","outcome":"not_found"}}"#;
    assert_eq!(parse_kill_outcome(resp), Some(KillOutcome::NotFound));
    let resp = r#"{"result":{"taskId":"t-1","outcome":"killed"}}"#;
    assert_eq!(parse_kill_outcome(resp), Some(KillOutcome::Killed));
    let resp = r#"{"result":{"taskId":"t-1","outcome":"already_exited"}}"#;
    assert_eq!(parse_kill_outcome(resp), Some(KillOutcome::AlreadyExited));
}
/// Round-trip through the agent's own serializer: what `extensions::task::respond()` produces must parse back to the same typed outcome.
/// This guards against the two sides drifting apart.
#[test]
fn parse_kill_outcome_round_trips_agent_serialization() {
    use distill_shell::extensions::task::KillTaskResponse;
    use distill_shell::session::result::ExtMethodResult;
    use distill_tools::types::KillOutcome;
    let wire = serde_json::to_string(
            &ExtMethodResult::success(KillTaskResponse {
                task_id: "t-1".into(),
                outcome: KillOutcome::NotFound,
            }),
        )
        .unwrap();
    assert_eq!(parse_kill_outcome(&wire), Some(KillOutcome::NotFound));
}
/// Error envelopes and malformed payloads yield `None` (clear pending state, keep the row).
#[test]
fn parse_kill_outcome_none_for_error_or_malformed() {
    assert_eq!(
            parse_kill_outcome(r#"{"result":null,"error":"session not found"}"#),
            None
        );
    assert_eq!(parse_kill_outcome("not json"), None);
    assert_eq!(parse_kill_outcome("{}"), None);
    assert_eq!(
            parse_kill_outcome(r#"{"result":{"taskId":"t-1","outcome":"exploded"}}"#),
            None
        );
}
/// Typed `outcome`: `Cancelled` maps to `StoppedLive`; `AlreadyFinished` and `NotFound` map to `NothingLive` (carrying the real status when known).
#[test]
fn parse_subagent_kill_outcome_reads_typed_outcome() {
    assert!(matches!(
            parse_subagent_kill_outcome(
                r#"{"result":{"subagentId":"sa-1","cancelled":true,"outcome":{"kind":"cancelled"}}}"#
            ),
            SubagentKillOutcome::StoppedLive
        ));
    assert!(matches!(
            parse_subagent_kill_outcome(
                r#"{"result":{"subagentId":"sa-1","cancelled":false,"outcome":{"kind":"already_finished","status":"completed"}}}"#
            ),
            SubagentKillOutcome::NothingLive { status: Some(s) } if s == "completed"
        ));
    assert!(matches!(
            parse_subagent_kill_outcome(
                r#"{"result":{"subagentId":"sa-1","cancelled":false,"outcome":{"kind":"not_found"}}}"#
            ),
            SubagentKillOutcome::NothingLive { status: None }
        ));
}
/// An older shell sends no `outcome`; the parser falls back to the legacy `cancelled` bool: true means `StoppedLive`, false means `NothingLive`.
#[test]
fn parse_subagent_kill_outcome_falls_back_to_legacy_bool() {
    assert!(matches!(
            parse_subagent_kill_outcome(r#"{"result":{"subagentId":"sa-1","cancelled":true}}"#),
            SubagentKillOutcome::StoppedLive
        ));
    assert!(matches!(
            parse_subagent_kill_outcome(r#"{"result":{"subagentId":"sa-1","cancelled":false}}"#),
            SubagentKillOutcome::NothingLive { status: None }
        ));
}
/// An unknown future `kind` deserializes to `Unknown` (via `#[serde(other)]`) and falls back to the always-present `cancelled` bool.
/// It must not become `RpcFailed`, which would leave the row stuck.
#[test]
fn parse_subagent_kill_outcome_unknown_kind_falls_back_to_legacy_bool() {
    assert!(matches!(
            parse_subagent_kill_outcome(
                r#"{"result":{"subagentId":"sa-1","cancelled":true,"outcome":{"kind":"some_future_kind"}}}"#
            ),
            SubagentKillOutcome::StoppedLive
        ));
    assert!(matches!(
            parse_subagent_kill_outcome(
                r#"{"result":{"subagentId":"sa-1","cancelled":false,"outcome":{"kind":"some_future_kind"}}}"#
            ),
            SubagentKillOutcome::NothingLive { status: None }
        ));
}
/// Round-trip through the agent's own serializer guards the two sides against drifting apart.
#[test]
fn parse_subagent_kill_outcome_round_trips_agent_serialization() {
    use distill_shell::extensions::task::{
        CancelSubagentResponse, SubagentCancelOutcomeDto,
    };
    let wire = serde_json::to_string(
            &ExtMethodResult::success(CancelSubagentResponse {
                subagent_id: "sa-1".into(),
                cancelled: false,
                outcome: Some(SubagentCancelOutcomeDto::AlreadyFinished {
                    status: "failed".into(),
                }),
            }),
        )
        .unwrap();
    assert!(matches!(
            parse_subagent_kill_outcome(&wire),
            SubagentKillOutcome::NothingLive { status: Some(s) } if s == "failed"
        ));
}
/// A top-level payload (no `result` envelope), error envelopes, and malformed payloads are a failed RPC (`RpcFailed`).
/// The caller must not finalize a possibly-live row.
#[test]
fn parse_subagent_kill_outcome_rpc_failed_for_error_or_malformed() {
    assert!(matches!(
            parse_subagent_kill_outcome(r#"{"cancelled":true}"#),
            SubagentKillOutcome::RpcFailed
        ));
    assert!(matches!(
            parse_subagent_kill_outcome(r#"{"result":null,"error":"session not found"}"#),
            SubagentKillOutcome::RpcFailed
        ));
    assert!(matches!(
            parse_subagent_kill_outcome("not json"),
            SubagentKillOutcome::RpcFailed
        ));
    assert!(matches!(
            parse_subagent_kill_outcome("{}"),
            SubagentKillOutcome::RpcFailed
        ));
}
/// Image-bearing interjections carry the blocks as a `content` array.
#[test]
fn interject_params_carry_content_when_blocks_present() {
    let sid = acp::SessionId::new("s1");
    let blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "look at [Image #1]",
        ))];
    let params = build_interject_params(
        &sid,
        "look at [Image #1]",
        "i1",
        Some(blocks.as_slice()),
    );
    let content = j(&params, "content").as_array().expect("content array");
    assert_eq!(content.len(), 1);
    assert_eq!(j(nth(content, 0), "text"), "look at [Image #1]");
}
/// A billing config with every field unset, for use as a base in `credit_balance_from_config` tests via struct-update syntax.
fn empty_billing_config() -> BillingConfig {
    BillingConfig {
        credit_usage_percent: None,
        current_period: None,
        monthly_limit: None,
        used: None,
        on_demand_cap: None,
        on_demand_used: None,
        prepaid_balance: None,
        is_unified_billing_user: None,
        billing_period_start: None,
        billing_period_end: None,
        history: vec![],
    }
}
#[test]
fn credit_balance_prefers_credit_usage_percent_over_limit_used() {
    let c = BillingConfig {
        credit_usage_percent: Some(42.0),
        monthly_limit: Some(Cent { val: 10_000 }),
        used: Some(Cent { val: 9_000 }),
        ..empty_billing_config()
    };
    assert_eq!(credit_balance_from_config(c).usage_pct, 42.0);
}

#[test]
fn billing_usage_provenance_requires_a_reported_usage_field() {
    assert!(!billing_config_has_usage_data(&empty_billing_config()));
    assert!(billing_config_has_usage_data(&BillingConfig {
        credit_usage_percent: Some(0.0),
        ..empty_billing_config()
    }));
    assert!(billing_config_has_usage_data(&BillingConfig {
        monthly_limit: Some(Cent { val: 0 }),
        ..empty_billing_config()
    }));
}
#[test]
fn credit_balance_forwards_is_unified_billing_user() {
    let c = BillingConfig {
        is_unified_billing_user: Some(true),
        ..empty_billing_config()
    };
    assert_eq!(
            credit_balance_from_config(c).is_unified_billing_user,
            Some(true)
        );
    assert_eq!(
            credit_balance_from_config(empty_billing_config()).is_unified_billing_user,
            None
        );
}
#[test]
fn credit_balance_falls_back_to_limit_used_when_percent_absent() {
    let c = BillingConfig {
        monthly_limit: Some(Cent { val: 10_000 }),
        used: Some(Cent { val: 2_500 }),
        ..empty_billing_config()
    };
    assert_eq!(credit_balance_from_config(c).usage_pct, 25.0);
}
/// Match production: RFC 3339 renders as the user's local wall-clock (no zone label).
fn expected_period_end_display(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .expect("test fixture is valid RFC 3339")
        .with_timezone(&chrono::Local)
        .format("%B %-d, %H:%M")
        .to_string()
}
#[test]
fn credit_balance_prefers_current_period_end_over_billing_period_end() {
    let end = "2026-06-08T20:00:00Z";
    let c = BillingConfig {
        credit_usage_percent: Some(10.0),
        current_period: Some(UsagePeriod {
            period_type: Some("USAGE_PERIOD_TYPE_WEEKLY".into()),
            start: Some("2026-06-01T00:00:00Z".into()),
            end: Some(end.into()),
        }),
        billing_period_end: Some("2026-07-01T20:00:00Z".into()),
        ..empty_billing_config()
    };
    assert_eq!(
            credit_balance_from_config(c).period_end_display.as_deref(),
            Some(expected_period_end_display(end).as_str())
        );
}
#[test]
fn credit_balance_period_end_uses_local_timezone() {
    let winter = "2026-01-15T20:00:00Z";
    let summer = "2026-07-15T20:00:00Z";
    let winter_cfg = BillingConfig {
        billing_period_end: Some(winter.into()),
        ..empty_billing_config()
    };
    let summer_cfg = BillingConfig {
        billing_period_end: Some(summer.into()),
        ..empty_billing_config()
    };
    assert_eq!(
            credit_balance_from_config(winter_cfg)
                .period_end_display
                .as_deref(),
            Some(expected_period_end_display(winter).as_str())
        );
    assert_eq!(
            credit_balance_from_config(summer_cfg)
                .period_end_display
                .as_deref(),
            Some(expected_period_end_display(summer).as_str())
        );
    assert_ne!(
            expected_period_end_display(winter),
            expected_period_end_display(summer)
        );
}
#[test]
fn credit_balance_falls_back_to_billing_period_end() {
    let end = "2026-07-01T20:00:00Z";
    let c = BillingConfig {
        billing_period_end: Some(end.into()),
        ..empty_billing_config()
    };
    assert_eq!(
            credit_balance_from_config(c).period_end_display.as_deref(),
            Some(expected_period_end_display(end).as_str())
        );
}
#[test]
fn credit_balance_period_end_falls_back_when_current_period_has_no_end() {
    let end = "2026-07-01T20:00:00Z";
    let c = BillingConfig {
        current_period: Some(UsagePeriod {
            period_type: None,
            start: Some("2026-06-01T00:00:00Z".into()),
            end: None,
        }),
        billing_period_end: Some(end.into()),
        ..empty_billing_config()
    };
    assert_eq!(
            credit_balance_from_config(c).period_end_display.as_deref(),
            Some(expected_period_end_display(end).as_str())
        );
}
#[test]
fn credit_balance_period_end_none_when_unavailable() {
    assert!(
            credit_balance_from_config(empty_billing_config())
                .period_end_display
                .is_none()
        );
}
#[test]
fn credit_balance_clamps_new_percent_above_100() {
    let c = BillingConfig {
        credit_usage_percent: Some(150.0),
        ..empty_billing_config()
    };
    assert_eq!(credit_balance_from_config(c).usage_pct, 100.0);
}
#[test]
fn credit_balance_clamps_legacy_used_above_limit() {
    let c = BillingConfig {
        monthly_limit: Some(Cent { val: 1_000 }),
        used: Some(Cent { val: 2_500 }),
        ..empty_billing_config()
    };
    assert_eq!(credit_balance_from_config(c).usage_pct, 100.0);
}
#[test]
fn credit_balance_effective_equals_usage_when_no_on_demand() {
    let c = BillingConfig {
        credit_usage_percent: Some(40.0),
        ..empty_billing_config()
    };
    let bal = credit_balance_from_config(c);
    assert!(!bal.pay_as_you_go);
    assert_eq!(bal.on_demand_cap_cents, None);
    assert_eq!(bal.effective_usage_pct, 40.0);
}
#[test]
fn credit_balance_effective_uses_on_demand_ratio_when_included_exhausted() {
    let c = BillingConfig {
        credit_usage_percent: Some(100.0),
        on_demand_cap: Some(Cent { val: 5_000 }),
        on_demand_used: Some(Cent { val: 1_000 }),
        ..empty_billing_config()
    };
    let bal = credit_balance_from_config(c);
    assert!(bal.pay_as_you_go);
    assert_eq!(bal.usage_pct, 100.0);
    assert_eq!(bal.effective_usage_pct, 20.0);
    assert_eq!(bal.on_demand_cap_cents, Some(5_000));
    assert_eq!(bal.on_demand_used_cents, Some(1_000));
}
#[test]
fn parse_auto_topup_present_rule_resolves() {
    let v = serde_json::json!({
            "rule": {"enabled": true, "topupAmount": {"val": 2000}, "maxAmountPerMonth": {"val": 10000}}
        });
    match parse_auto_topup_response(&v) {
        crate::views::credit_bar::AutoTopupFetch::Resolved(at) => {
            assert!(at.enabled);
            assert_eq!(at.topup_amount_cents, Some(2000));
            assert_eq!(at.max_amount_cents, Some(10000));
        }
        other => panic!("expected Resolved, got {other:?}"),
    }
}
#[test]
fn parse_auto_topup_empty_body_resolves_to_disabled() {
    for v in [serde_json::json!({}), serde_json::json!({ "rule": null })] {
        match parse_auto_topup_response(&v) {
            crate::views::credit_bar::AutoTopupFetch::Resolved(at) => {
                assert!(!at.enabled);
            }
            other => panic!("expected Resolved(disabled), got {other:?}"),
        }
    }
}
#[test]
fn parse_auto_topup_rule_without_enabled_is_disabled() {
    let v = serde_json::json!({ "rule": {"topupAmount": {"val": 500}} });
    match parse_auto_topup_response(&v) {
        crate::views::credit_bar::AutoTopupFetch::Resolved(at) => {
            assert!(!at.enabled);
            assert_eq!(at.topup_amount_cents, Some(500));
        }
        other => panic!("expected Resolved(disabled), got {other:?}"),
    }
}
#[test]
fn parse_auto_topup_malformed_body_is_unchanged() {
    for v in [serde_json::json!(null), serde_json::json!(42)] {
        match parse_auto_topup_response(&v) {
            crate::views::credit_bar::AutoTopupFetch::Unchanged => {}
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }
}
#[test]
fn credit_balance_effective_tracks_included_for_new_shape_under_100() {
    let c = BillingConfig {
        credit_usage_percent: Some(95.0),
        on_demand_cap: Some(Cent { val: 5_000 }),
        on_demand_used: Some(Cent { val: 0 }),
        ..empty_billing_config()
    };
    let bal = credit_balance_from_config(c);
    assert!(bal.pay_as_you_go);
    assert_eq!(bal.effective_usage_pct, 95.0);
}
#[test]
fn credit_balance_effective_blends_budget_for_legacy_shape_under_100() {
    let c = BillingConfig {
        monthly_limit: Some(Cent { val: 10_000 }),
        used: Some(Cent { val: 5_000 }),
        on_demand_cap: Some(Cent { val: 10_000 }),
        on_demand_used: Some(Cent { val: 0 }),
        ..empty_billing_config()
    };
    let bal = credit_balance_from_config(c);
    assert!(bal.pay_as_you_go);
    assert_eq!(bal.usage_pct, 50.0);
    assert_eq!(bal.effective_usage_pct, 25.0);
}
#[test]
fn parse_worktree_restore_payload_full() {
    use distill_workspace::session::git::RestoreDegree;
    let value = serde_json::json!({
            "codeRestored": true,
            "restoreSummary": "checked out abc12345, staged: true, unstaged: false, untracked: 3",
            "restoreDegree": "full",
        });
    let (restored, summary, degree) = parse_worktree_restore_payload(&value);
    assert!(restored);
    assert_eq!(degree, Some(RestoreDegree::Full));
    assert!(summary.unwrap().contains("staged: true"));
}
#[test]
fn parse_worktree_restore_payload_head_only() {
    use distill_workspace::session::git::RestoreDegree;
    let value = serde_json::json!({
            "codeRestored": true,
            "restoreSummary": "checked out abc (session registry disabled — staged/unstaged/untracked not restored)",
            "restoreDegree": "head_only",
        });
    let (_, _, degree) = parse_worktree_restore_payload(&value);
    assert_eq!(degree, Some(RestoreDegree::HeadOnly));
}
#[test]
fn parse_worktree_restore_payload_missing_fields() {
    let value = serde_json::json!({ "codeRestored": false });
    let (restored, summary, degree) = parse_worktree_restore_payload(&value);
    assert!(!restored);
    assert!(summary.is_none());
    assert!(degree.is_none());
}
/// A typo or unknown variant must parse as `None` rather than silently round-tripping a bogus value.
#[test]
fn parse_worktree_restore_payload_rejects_unknown_degree() {
    let value = serde_json::json!({
            "codeRestored": true,
            "restoreSummary": "x",
            "restoreDegree": "full_",
        });
    let (_, _, degree) = parse_worktree_restore_payload(&value);
    assert!(degree.is_none(), "typo must produce None");
}
#[test]
fn parse_worktree_strategy_summary_grove_success_and_empty() {
    let value = serde_json::json!({
            "strategy": {
                "requestedStrategy": "grove",
                "resolvedStrategy": "grove-fuse",
                "transport": "fuse",
                "sourceMode": "local",
                "daemonCapabilityClass": "current"
            }
        });
    assert_eq!(
            parse_worktree_strategy_summary(&value).as_deref(),
            Some("Requested Grove; using `grove-fuse` (local objects).")
        );
    assert!(parse_worktree_strategy_summary(&serde_json::json!({})).is_none());
    assert!(
            parse_worktree_strategy_summary(&serde_json::json!({ "strategy": {} })).is_none()
        );
}
#[test]
fn parse_session_load_restore_meta_full_shape() {
    use distill_workspace::session::git::RestoreDegree;
    let meta = serde_json::json!({
            "codeRestore": {
                "restored": true,
                "summary": "checked out abc12345",
                "degree": "head_only",
            }
        });
    let (restored, summary, degree) = parse_session_load_restore_meta(meta.as_object());
    assert!(restored);
    assert_eq!(summary.as_deref(), Some("checked out abc12345"));
    assert_eq!(degree, Some(RestoreDegree::HeadOnly));
}
#[test]
fn parse_session_load_restore_meta_absent_returns_false() {
    let (restored, summary, degree) = parse_session_load_restore_meta(None);
    assert!(!restored);
    assert!(summary.is_none());
    assert!(degree.is_none());
}
#[test]
fn parse_session_load_restore_meta_no_coderestore_key() {
    let meta = serde_json::json!({ "other": 1 });
    let (restored, summary, degree) = parse_session_load_restore_meta(meta.as_object());
    assert!(!restored);
    assert!(summary.is_none());
    assert!(degree.is_none());
}
/// Parser must reject unknown degree strings in the meta path.
#[test]
fn parse_session_load_restore_meta_rejects_unknown_degree() {
    let meta = serde_json::json!({
            "codeRestore": { "restored": true, "summary": "x", "degree": "weird" }
        });
    let (_, _, degree) = parse_session_load_restore_meta(meta.as_object());
    assert!(degree.is_none());
}
/// A deadline is `Timeout` and a decode/persist panic is `Panicked`, never `ReadFailed`; the deadline arm returns without waiting on the stalled thread.
#[tokio::test]
async fn clipboard_probe_stage_names_a_deadline_and_a_panic_apart_from_a_read_failure() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    let started = std::time::Instant::now();
    let late = clipboard_probe_stage(
            std::time::Duration::from_millis(20),
            || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok((ProbedAttachment::NoRaster, None))
            },
        )
        .await;
    assert!(started.elapsed() < std::time::Duration::from_millis(250), "the deadline must not wait on the stall");
    assert_eq!(late.expect_err("the stage outlives the deadline").reason, Reason::Timeout);
    let panicked = clipboard_probe_stage(
            std::time::Duration::from_secs(5),
            || panic!("decoder bug"),
        )
        .await;
    assert_eq!(panicked.expect_err("a panicking stage drops").reason, Reason::Panicked);
}
fn probe_raster() -> crate::clipboard::ImageData {
    crate::clipboard::ImageData {
        data: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3],
        mime_type: "image/png".to_owned(),
    }
}
/// Each drop reason maps to exactly one completion; a failed persist keeps its error text for the toast.
#[tokio::test]
async fn bounded_clipboard_probe_maps_each_drop_reason_to_its_completion() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    for (reason, image, expected) in [
        (Reason::PasteboardChangedBeforeRead, None, "ProbeDropped"),
        (Reason::PasteboardChangedAfterRead, Some(probe_raster()), "ProbeDropped"),
        (Reason::BracketedPayloadMismatch, None, "ProbeDropped"),
        (Reason::PersistFailed, Some(probe_raster()), "PersistFailed(disk full)"),
        (Reason::ReadFailed, None, "ProbeFailed"),
        (Reason::Timeout, None, "ProbeFailed"),
        (Reason::Panicked, None, "ProbeFailed"),
    ] {
        let message = (reason == Reason::PersistFailed).then(|| "disk full".to_owned());
        let (attachment, file_urls) = bounded_clipboard_probe(
                std::time::Duration::from_secs(5),
                move || Err(crate::clipboard::ProbeDrop {
                    reason,
                    image,
                    message,
                }),
            )
            .await;
        let got = match attachment {
            ProbedAttachment::ProbeDropped => "ProbeDropped".to_owned(),
            ProbedAttachment::PersistFailed(message) => {
                format!("PersistFailed({message})")
            }
            ProbedAttachment::ProbeFailed => "ProbeFailed".to_owned(),
            other => panic!("{reason:?} completed as {other:?}"),
        };
        assert_eq!(got, expected, "{reason:?}");
        assert!(file_urls.is_none(), "{reason:?} must attach nothing");
    }
    let (attachment, file_urls) = bounded_clipboard_probe(
            std::time::Duration::from_secs(5),
            || Ok((ProbedAttachment::NoRaster, Some("/tmp/a".to_owned()))),
        )
        .await;
    assert!(matches!(attachment, ProbedAttachment::NoRaster));
    assert_eq!(file_urls.as_deref(), Some("/tmp/a"));
}
/// Runs the blocking stage against a canned pasteboard (baseline changeCount 1) with no session images dir.
fn probe_stage(
    hook: crate::clipboard::ClipboardProbeHook,
) -> (ClipboardProbeStage, u32) {
    crate::clipboard::set_clipboard_probe_hook(hook);
    let outcome = probe_clipboard_attachment_blocking(
        Some(1),
        Some("caption".to_owned()),
        false,
        None,
    );
    let probe_calls = crate::clipboard::clipboard_probe_call_count();
    crate::clipboard::clear_clipboard_probe_hook();
    (outcome, probe_calls)
}
/// A board that moved since enqueue drops before any pasteboard read.
#[test]
fn probe_stage_drops_before_reading_when_the_board_moved_since_the_keypress() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    let (outcome, probe_calls) = probe_stage(crate::clipboard::ClipboardProbeHook {
        snapshot: Some((Some(2), true)),
        ..crate::clipboard::ClipboardProbeHook::with_raster(Some(probe_raster()))
    });
    let drop = outcome.expect_err("the board moved since the keypress");
    assert_eq!(drop.reason, Reason::PasteboardChangedBeforeRead);
    assert!(drop.image.is_none());
    assert_eq!(probe_calls, 0, "a stale board must not be read at all");
}
/// A copy landing during the read (GB-5461) discards the raster that read returned and names the move.
#[test]
fn probe_stage_drops_the_raster_when_the_board_moved_during_the_read() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    let (outcome, probe_calls) = probe_stage(crate::clipboard::ClipboardProbeHook {
        snapshot_after_read: Some((Some(2), true)),
        ..crate::clipboard::ClipboardProbeHook::with_raster(Some(probe_raster()))
    });
    let drop = outcome.expect_err("the board moved under the read");
    assert_eq!(drop.reason, Reason::PasteboardChangedAfterRead);
    assert_eq!(drop.image.map(|img| img.data), Some(probe_raster().data));
    assert_eq!(probe_calls, 1);
}
/// A read error keeps its own reason: the deadline kill stays `Timeout`, anything else `ReadFailed`.
#[test]
fn probe_stage_keeps_the_read_error_reason() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    for reason in [Reason::Timeout, Reason::ReadFailed] {
        let (outcome, _) = probe_stage(crate::clipboard::ClipboardProbeHook {
            attachment_probe_error: Some(reason),
            ..crate::clipboard::ClipboardProbeHook::with_raster(None)
        });
        let drop = outcome.expect_err("the read failed");
        assert_eq!(drop.reason, reason);
        assert!(drop.image.is_none(), "{reason:?}");
    }
}
/// An unchanged board passes its raster through decoded, or reports that none was there.
#[test]
fn probe_stage_attaches_the_raster_or_reports_none() {
    let (outcome, _) = probe_stage(
        crate::clipboard::ClipboardProbeHook::with_raster(Some(probe_raster())),
    );
    let (attachment, file_urls) = outcome.expect("the read stands");
    assert!(matches!(attachment, ProbedAttachment::Image(_)), "got {attachment:?}");
    assert!(file_urls.is_none());
    let (outcome, _) = probe_stage(
        crate::clipboard::ClipboardProbeHook::with_raster(None),
    );
    let (attachment, _) = outcome.expect("the read stands");
    assert!(matches!(attachment, ProbedAttachment::NoRaster), "got {attachment:?}");
}
/// A failed session persist keeps the raster (for the drop's hash) and the error text (for the toast).
#[test]
fn probe_stage_reports_a_failed_persist_with_its_error() {
    use distill_telemetry::events::ClipboardProbeDropReason as Reason;
    let not_a_dir = tempfile::NamedTempFile::new().expect("temp file");
    crate::clipboard::set_clipboard_probe_hook(
        crate::clipboard::ClipboardProbeHook::with_raster(Some(probe_raster())),
    );
    let outcome = probe_clipboard_attachment_blocking(
        Some(1),
        None,
        false,
        Some(not_a_dir.path().to_path_buf()),
    );
    crate::clipboard::clear_clipboard_probe_hook();
    let drop = outcome.expect_err("persisting under a plain file must fail");
    assert_eq!(drop.reason, Reason::PersistFailed);
    assert_eq!(drop.image.map(|img| img.data), Some(probe_raster().data));
    assert!(drop.message.is_some_and(|message| !message.is_empty()));
}
/// Unknown keys return a descriptive error.
#[tokio::test]
async fn persist_setting_unknown_key_returns_err() {
    use crate::settings::SettingValue;
    let result = persist_setting("not-a-real-setting", SettingValue::Bool(true)).await;
    match result {
        Err(msg) => {
            assert!(
                msg.contains("unknown setting key"),
                "expected error to mention unknown setting key, got: {msg}",
            )
        }
        Ok(()) => panic!("expected Err for unknown key"),
    }
}
/// Type-mismatch returns Err, not panic: the parser runs inside spawned tasks.
#[tokio::test]
async fn persist_setting_type_mismatch_errors_compact_mode() {
    use crate::settings::SettingValue;
    let r = persist_setting("compact_mode", SettingValue::String("nope".into())).await;
    let err = r.expect_err("compact_mode with String payload must return Err");
    assert!(
            err.contains("persist_setting(compact_mode) expected Bool"),
            "error message must mention key + expected kind, got: {err}",
        );
}
/// Type-mismatch for `show_timestamps`.
#[tokio::test]
async fn persist_setting_type_mismatch_errors_show_timestamps() {
    use crate::settings::SettingValue;
    let r = persist_setting("show_timestamps", SettingValue::String("nope".into()))
        .await;
    let err = r.expect_err("show_timestamps with String payload must return Err");
    assert!(
            err.contains("persist_setting(show_timestamps) expected Bool"),
            "error message must mention key + expected kind, got: {err}",
        );
}
/// Type-mismatch for `show_timeline`.
#[tokio::test]
async fn persist_setting_type_mismatch_errors_show_timeline() {
    use crate::settings::SettingValue;
    let r = persist_setting("show_timeline", SettingValue::String("nope".into())).await;
    let err = r.expect_err("show_timeline with String payload must return Err");
    assert!(
            err.contains("persist_setting(show_timeline) expected Bool"),
            "error message must mention key + expected kind, got: {err}",
        );
}
#[tokio::test]
async fn persist_setting_type_mismatch_errors_page_flip_on_send() {
    use crate::settings::SettingValue;
    let r = persist_setting("page_flip_on_send", SettingValue::String("nope".into()))
        .await;
    let err = r.expect_err("page_flip_on_send with String payload must return Err");
    assert!(
            err.contains("persist_setting(page_flip_on_send) expected Bool"),
            "got: {err}",
        );
}
#[tokio::test]
async fn persist_setting_type_mismatch_errors_confirm_before_rewind() {
    use crate::settings::SettingValue;
    let r = persist_setting("confirm_before_rewind", SettingValue::String("nope".into()))
        .await;
    let err = r.expect_err("confirm_before_rewind with String payload must return Err");
    assert!(
            err.contains("persist_setting(confirm_before_rewind) expected Bool"),
            "got: {err}",
        );
}
#[tokio::test]
async fn persist_setting_type_mismatch_errors_combine_queued_prompts() {
    use crate::settings::SettingValue;
    let r = persist_setting(
            "combine_queued_prompts",
            SettingValue::String("nope".into()),
        )
        .await;
    let err = r.expect_err("combine_queued_prompts with String payload must return Err");
    assert!(
            err.contains("persist_setting(combine_queued_prompts) expected Bool"),
            "got: {err}",
        );
}
/// Type-mismatch for `simple_mode`.
#[tokio::test]
async fn persist_setting_type_mismatch_errors_simple_mode() {
    use crate::settings::SettingValue;
    let r = persist_setting("simple_mode", SettingValue::Int(42)).await;
    let err = r.expect_err("simple_mode with Int payload must return Err");
    assert!(
            err.contains("persist_setting(simple_mode) expected Bool"),
            "error message must mention key + expected kind, got: {err}",
        );
}
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
/// Spawn a fake ACP agent that counts `x.ai/yolo_mode_changed` notifications.
/// Exits when the channel closes.
fn spawn_fake_acp_agent(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpAgentMessage>,
) -> Arc<AtomicUsize> {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let distill_acp_lib::AcpAgentMessage::ExtNotification(args) = msg {
                if args.request.method.as_ref() == "x.ai/yolo_mode_changed" {
                    counter_clone.fetch_add(1, Ordering::SeqCst);
                }
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    counter
}
/// Redirect `GROK_HOME` to a tempdir for test isolation.
fn setup_distill_home_in_tempdir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir creation");
    unsafe {
        std::env::set_var("GROK_HOME", tmp.path());
    }
    tmp
}
fn register_session_in(root: &std::path::Path, id: &str) -> acp::SessionId {
    use distill_active_sessions::{ActiveSession, register_in};
    let session_id = acp::SessionId::new(id);
    register_in(
            root,
            ActiveSession {
                session_id: session_id.clone(),
                pid: std::process::id(),
                cwd: "/tmp/test".into(),
                opened_at: chrono::Utc::now(),
            },
        )
        .expect("register");
    session_id
}
/// Lock-free: the helper removes the registry entry (the normal path).
#[test]
fn unregister_best_effort_removes_entry_when_lock_free() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sid = register_session_in(dir.path(), "s1");
    unregister_active_session_best_effort_in(dir.path(), &sid);
    assert!(
            distill_active_sessions::list_in(dir.path())
                .expect("list")
                .is_empty(),
            "lock-free unregister must remove the entry",
        );
}
/// Contended: the quit path must skip the shared flock rather than block.
/// The unregister runs on a worker joined against a deadline so a blocking regression fails fast instead of deadlocking the test binary.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn unregister_best_effort_is_nonblocking_under_lock_contention() {
    use std::os::unix::io::AsRawFd;
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = tempfile::tempdir().expect("tempdir");
    let sid = register_session_in(dir.path(), "s1");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join("active_sessions.lock"))
        .expect("open lock");
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let (tx, rx) = mpsc::channel();
    let root = dir.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        unregister_active_session_best_effort_in(&root, &sid);
        let _ = tx.send(());
    });
    let returned = rx.recv_timeout(Duration::from_secs(2)).is_ok();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    worker.join().expect("worker thread");
    assert!(
            returned,
            "contended unregister blocked on the shared flock instead of skipping",
        );
    assert_eq!(
            distill_active_sessions::list_in(dir.path())
                .expect("list")
                .len(),
            1,
            "contended unregister must leave the entry for collect_crashed",
        );
}
/// A real I/O error (uncreatable registry root) is swallowed: the best-effort helper logs and returns instead of panicking.
#[test]
fn unregister_best_effort_swallows_io_error() {
    let file = tempfile::NamedTempFile::new().expect("tempfile");
    let bad_root = file.path().join("not-a-dir");
    unregister_active_session_best_effort_in(&bad_root, &acp::SessionId::new("s1"));
}
/// BestEffort path fires exactly one ACP notification regardless of disk outcome.
#[tokio::test]
async fn persist_permission_mode_acp_notification_fires_once_on_best_effort() {
    use agent_client_protocol as acp;
    let _guard = setup_distill_home_in_tempdir();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let counter = spawn_fake_acp_agent(rx);
    let session_id = Some(acp::SessionId::new(Arc::from("test-session")));
    let result = persist_permission_mode_and_notify(
            "always-approve",
            session_id,
            PermissionModePersist::BestEffort,
            tx,
        )
        .await;
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "ACP `x.ai/yolo_mode_changed` notification must fire exactly once \
             on BestEffort path (regardless of disk outcome)",
        );
    assert!(
            matches!(
                result,
                TaskResult::SettingPersisted { .. }
                    | TaskResult::SettingPersistFailedBestEffort { .. },
            ),
            "BestEffort path must return SettingPersisted (Ok) or \
             SettingPersistFailedBestEffort (Err), got {result:?}",
        );
}
/// WithRollback: notification count matches disk outcome (1 on Ok, 0 on Err).
#[tokio::test]
async fn persist_permission_mode_acp_notification_gated_on_disk_for_with_rollback() {
    use agent_client_protocol as acp;
    let _guard = setup_distill_home_in_tempdir();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let counter = spawn_fake_acp_agent(rx);
    let session_id = Some(acp::SessionId::new(Arc::from("test-session")));
    let result = persist_permission_mode_and_notify(
            "always-approve",
            session_id,
            PermissionModePersist::WithRollback("ask"),
            tx,
        )
        .await;
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let count = counter.load(Ordering::SeqCst);
    match result {
        TaskResult::SettingPersisted { .. } => {
            assert_eq!(
                    count, 1,
                    "WithRollback + disk Ok must fire ACP notification exactly once",
                );
        }
        TaskResult::SettingPersistFailed { .. } => {
            assert_eq!(
                    count, 0,
                    "WithRollback + disk Err must SUPPRESS the ACP notification \
                     (Issue 3 — keeps agent and pager state consistent on rollback)",
                );
        }
        other => {
            panic!("expected SettingPersisted or SettingPersistFailed, got {other:?}")
        }
    }
}
/// `session_id: None` suppresses ACP notification unconditionally.
#[tokio::test]
async fn persist_permission_mode_no_session_id_suppresses_acp() {
    let _guard = setup_distill_home_in_tempdir();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let counter = spawn_fake_acp_agent(rx);
    let _result = persist_permission_mode_and_notify(
            "always-approve",
            None,
            PermissionModePersist::BestEffort,
            tx,
        )
        .await;
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "session_id=None must suppress the ACP notification — sessionless \
             agents have no ACP channel to notify",
        );
}
/// BestEffort with a disk failure must not return `SettingPersisted`.
#[tokio::test]
async fn persist_permission_mode_best_effort_failure_returns_dedicated_variant() {
    let _guard = setup_distill_home_in_tempdir();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _counter = spawn_fake_acp_agent(rx);
    let result = persist_permission_mode_and_notify(
            "always-approve",
            None,
            PermissionModePersist::BestEffort,
            tx,
        )
        .await;
    match result {
        TaskResult::SettingPersisted { key, value } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(value, crate::settings::SettingValue::Enum("always-approve"));
        }
        TaskResult::SettingPersistFailedBestEffort { key, error: _ } => {
            assert_eq!(
                    key, "permission_mode",
                    "BestEffort failure MUST report key=permission_mode and \
                     NOT lie about success via SettingPersisted",
                );
        }
        other => {
            panic!(
                "BestEffort must return SettingPersisted (Ok) or \
                 SettingPersistFailedBestEffort (Err), got {other:?} — \
                 a regression to `SettingPersisted` on failure is Round-2 Issue 2",
            )
        }
    }
}
/// (Err, WithRollback) suppresses for all canonicals.
#[test]
fn should_send_yolo_acp_with_rollback_suppresses_on_err() {
    let result: Result<(), String> = Err("simulated disk failure".to_string());
    assert!(
            !should_send_yolo_acp_notification(&result, PermissionModePersist::WithRollback("ask")),
            "WithRollback + Err MUST suppress the ACP notification",
        );
    assert!(
            !should_send_yolo_acp_notification(
                &result,
                PermissionModePersist::WithRollback("always-approve")
            ),
            "WithRollback + Err MUST suppress regardless of the prior canonical",
        );
    assert!(
            !should_send_yolo_acp_notification(
                &result,
                PermissionModePersist::WithRollback("default")
            ),
            "WithRollback + Err MUST suppress for the 'default' prior canonical too",
        );
}
/// (Ok, WithRollback) fires for all canonicals.
#[test]
fn should_send_yolo_acp_with_rollback_fires_on_ok() {
    let ok: Result<(), String> = Ok(());
    assert!(
            should_send_yolo_acp_notification(&ok, PermissionModePersist::WithRollback("ask")),
            "WithRollback + Ok must fire the ACP notification (happy path)",
        );
    assert!(
            should_send_yolo_acp_notification(
                &ok,
                PermissionModePersist::WithRollback("always-approve")
            ),
            "WithRollback + Ok fires regardless of the prior canonical",
        );
    assert!(
            should_send_yolo_acp_notification(&ok, PermissionModePersist::WithRollback("default")),
            "WithRollback + Ok fires for 'default' prior canonical too",
        );
}
#[test]
fn should_send_yolo_acp_best_effort_fires_on_both_outcomes() {
    let ok: Result<(), String> = Ok(());
    let err: Result<(), String> = Err("simulated".to_string());
    assert!(
            should_send_yolo_acp_notification(&ok, PermissionModePersist::BestEffort),
            "BestEffort + Ok must notify",
        );
    assert!(
            should_send_yolo_acp_notification(&err, PermissionModePersist::BestEffort),
            "BestEffort + Err must STILL notify (cycle_mode contract \
             — the cycle_mode state machine doesn't have a clean \
             single-field rollback)",
        );
}
#[test]
fn route_permission_mode_result_ok_returns_persisted() {
    let result = route_permission_mode_result(
        Ok(()),
        PermissionModePersist::WithRollback("ask"),
        "always-approve",
    );
    match result {
        TaskResult::SettingPersisted { key, value } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(value, crate::settings::SettingValue::Enum("always-approve"));
        }
        other => panic!("Ok must return SettingPersisted, got {other:?}"),
    }
}
#[test]
fn route_permission_mode_result_err_with_rollback_off_routes_to_failed() {
    let result = route_permission_mode_result(
        Err("simulated".to_string()),
        PermissionModePersist::WithRollback("ask"),
        "always-approve",
    );
    match result {
        TaskResult::SettingPersistFailed { key, rollback_value, error } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(rollback_value, crate::settings::SettingValue::Enum("ask"));
            assert_eq!(error, "simulated");
        }
        other => {
            panic!("WithRollback + Err must return SettingPersistFailed, got {other:?}")
        }
    }
}
#[test]
fn route_permission_mode_result_err_with_rollback_on_routes_to_failed() {
    let result = route_permission_mode_result(
        Err("simulated".to_string()),
        PermissionModePersist::WithRollback("always-approve"),
        "ask",
    );
    match result {
        TaskResult::SettingPersistFailed { key, rollback_value, error } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(
                    rollback_value,
                    crate::settings::SettingValue::Enum("always-approve"),
                    "prev_canonical='always-approve' must route to canonical \
                     'always-approve' for rollback",
                );
            assert_eq!(error, "simulated");
        }
        other => {
            panic!("WithRollback + Err must return SettingPersistFailed, got {other:?}")
        }
    }
}
/// Rollback preserves "default" canonical (not collapsed to "ask").
#[test]
fn route_permission_mode_result_err_with_rollback_default_routes_to_failed() {
    let result = route_permission_mode_result(
        Err("simulated".to_string()),
        PermissionModePersist::WithRollback("default"),
        "always-approve",
    );
    match result {
        TaskResult::SettingPersistFailed { key, rollback_value, error } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(
                    rollback_value,
                    crate::settings::SettingValue::Enum("default"),
                    "PR 11: prev_canonical='default' must roll back to canonical 'default', \
                     NOT collapse onto 'ask' through a bool projection",
                );
            assert_eq!(error, "simulated");
        }
        other => {
            panic!("WithRollback + Err must return SettingPersistFailed, got {other:?}")
        }
    }
}
/// Ok path preserves "default" canonical verbatim.
#[test]
fn route_permission_mode_result_ok_preserves_default_canonical() {
    let result = route_permission_mode_result(
        Ok(()),
        PermissionModePersist::WithRollback("ask"),
        "default",
    );
    match result {
        TaskResult::SettingPersisted { key, value } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(
                    value,
                    crate::settings::SettingValue::Enum("default"),
                    "PR 11: 'default' canonical must survive the route fn intact",
                );
        }
        other => panic!("Ok must return SettingPersisted, got {other:?}"),
    }
}
/// `(Err, BestEffort)` must not return `SettingPersisted`.
#[test]
fn route_permission_mode_result_err_best_effort_routes_to_dedicated_variant() {
    let result = route_permission_mode_result(
        Err("simulated".to_string()),
        PermissionModePersist::BestEffort,
        "always-approve",
    );
    match result {
        TaskResult::SettingPersistFailedBestEffort { key, error } => {
            assert_eq!(key, "permission_mode");
            assert_eq!(error, "simulated");
        }
        TaskResult::SettingPersisted { .. } => {
            panic!(
                "BestEffort + Err MUST NOT return SettingPersisted — that would lie about \
                 success on disk failure (Round-2 Issue 2 regression)",
            )
        }
        other => {
            panic!("BestEffort + Err must return SettingPersistFailedBestEffort, got {other:?}",)
        }
    }
}
#[test]
fn marketplace_outcome_succeeded_only_accepts_success_status() {
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};
    let success = ActionOutcome {
        status: OutcomeStatus::Success,
        message: "updated".into(),
        requires_reload: true,
        requires_restart: false,
    };
    let failed = ActionOutcome {
        status: OutcomeStatus::InternalError,
        message: "failed".into(),
        requires_reload: false,
        requires_restart: false,
    };
    assert!(marketplace_outcome_succeeded(&success));
    assert!(!marketplace_outcome_succeeded(&failed));
}
#[tokio::test]
async fn check_marketplace_updates_dispatches_update_and_skips_failed_notifications() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use distill_acp_lib::AcpAgentMessage;
    use distill_hooks_plugins_types::{ActionOutcome, MarketplaceAction, OutcomeStatus};
    let action_calls = Arc::new(AtomicUsize::new(0));
    let saw_update = Arc::new(AtomicBool::new(false));
    let saw_wrong_action = Arc::new(AtomicBool::new(false));
    let saw_success_notification = Arc::new(AtomicBool::new(false));
    let action_calls_for_task = action_calls.clone();
    let saw_update_for_task = saw_update.clone();
    let saw_wrong_action_for_task = saw_wrong_action.clone();
    let saw_success_notification_for_task = saw_success_notification.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                match args.request.method.as_ref() {
                    "x.ai/marketplace/list" => {
                        let response = serde_json::json!({
                                "result": {
                                    "sources": [{
                                        "sourceName": "test-source",
                                        "sourceKind": "git",
                                        "sourceUrlOrPath": "https://example.com/plugins.git",
                                        "plugins": [{
                                            "name": "test-plugin",
                                            "version": "2.0.0",
                                            "description": null,
                                            "category": null,
                                            "author": null,
                                            "tags": [],
                                            "relativePath": "plugins/test-plugin",
                                            "skillCount": 0,
                                            "hasHooks": false,
                                            "hasAgents": false,
                                            "hasMcp": false,
                                            "installStatus": "update_available",
                                            "installedVersion": "1.0.0"
                                        }],
                                        "error": null
                                    }]
                                }
                            });
                        let raw = serde_json::value::RawValue::from_string(
                                response.to_string(),
                            )
                            .expect("serialize marketplace list response");
                        let _ = args
                            .response_tx
                            .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                    }
                    "x.ai/marketplace/action" => {
                        action_calls_for_task.fetch_add(1, Ordering::SeqCst);
                        let req: distill_hooks_plugins_types::MarketplaceActionRequest = serde_json::from_str(
                                args.request.params.get(),
                            )
                            .expect("parse marketplace action request");
                        match req.action {
                            MarketplaceAction::Update {
                                source_url_or_path,
                                plugin_relative_path,
                            } if source_url_or_path == "https://example.com/plugins.git"
                                && plugin_relative_path == "plugins/test-plugin" => {
                                saw_update_for_task.store(true, Ordering::SeqCst);
                            }
                            _ => {
                                saw_wrong_action_for_task.store(true, Ordering::SeqCst);
                            }
                        }
                        let outcome = ActionOutcome {
                            status: OutcomeStatus::InternalError,
                            message: "update failed".into(),
                            requires_reload: false,
                            requires_restart: false,
                        };
                        let response = serde_json::json!({ "result": outcome });
                        let raw = serde_json::value::RawValue::from_string(
                                response.to_string(),
                            )
                            .expect("serialize marketplace action response");
                        let _ = args
                            .response_tx
                            .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                    }
                    "x.ai/plugins/notify-updates" => {
                        saw_success_notification_for_task.store(true, Ordering::SeqCst);
                        let raw = serde_json::value::RawValue::from_string("{}".into())
                            .expect("serialize notify response");
                        let _ = args
                            .response_tx
                            .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                    }
                    _ => {
                        let raw = serde_json::value::RawValue::from_string("{}".into())
                            .expect("serialize fallback response");
                        let _ = args
                            .response_tx
                            .send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                    }
                }
            }
        }
    });
    let mut tasks = JoinSet::new();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    execute(
        Effect::CheckMarketplaceUpdates {
            agent_id: AgentId(7),
            session_id: acp::SessionId::new(Arc::from("test-session")),
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    let result = tasks
        .join_next()
        .await
        .expect("task should complete")
        .expect("task should not panic");
    match result {
        TaskResult::MarketplaceUpdatesAvailable { agent_id, updates } => {
            assert_eq!(agent_id, AgentId(7));
            assert!(updates.is_empty());
        }
        other => panic!("expected MarketplaceUpdatesAvailable, got {other:?}"),
    }
    assert_eq!(action_calls.load(Ordering::SeqCst), 1);
    assert!(saw_update.load(Ordering::SeqCst));
    assert!(!saw_wrong_action.load(Ordering::SeqCst));
    assert!(!saw_success_notification.load(Ordering::SeqCst));
}
#[tokio::test]
async fn foreign_scan_task_echoes_sequence_without_enabled_sources() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    let app_coordinator = crate::app::ForeignScanCoordinator::default();
    app_coordinator.begin_request(41);
    execute(
        Effect::ScanForeignSessions {
            cwd: PathBuf::from("/path/that/must/not/be-read"),
            compat: distill_foreign_sessions::EnabledForeignSessionSources::default(),
            distill_home: PathBuf::from("/path/that/must/not/be-read"),
            coordinator: app_coordinator.clone(),
            seq: 41,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::ForeignSessionsScanned { entries, seq } => {
            assert!(entries.is_empty());
            assert_eq!(seq, 41);
        }
        other => panic!("expected ForeignSessionsScanned, got {other:?}"),
    }
    drop(app_coordinator);
}
#[tokio::test]
async fn foreign_resume_detection_runs_as_task_result() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    let (quit, _) = execute(
        Effect::CanonicalizeForeignResumeCwd {
            requested_cwd: PathBuf::from("/path/that/does-not-exist"),
            launch_token: 7,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    assert!(!quit);
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::ForeignResumeCwdCanonicalized {
            canonical_cwd,
            launch_token,
            ..
        } => {
            assert!(canonical_cwd.is_none());
            assert_eq!(launch_token, 7);
        }
        other => panic!("expected ForeignResumeCwdCanonicalized, got {other:?}"),
    }
    let canonical_cwd = dunce::canonicalize(tempfile::tempdir().unwrap().path())
        .unwrap();
    let (quit, _) = execute(
        Effect::DetectForeignResumeHint {
            canonical_cwd: canonical_cwd.clone(),
            compat: distill_foreign_sessions::EnabledForeignSessionSources::default(),
            distill_home: PathBuf::from("/path/that/must/not-be-read"),
            launch_token: 8,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    assert!(!quit);
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::ForeignResumeHintDetected {
            canonical_cwd: result_cwd,
            launch_token,
            hint,
        } => {
            assert_eq!(result_cwd, canonical_cwd);
            assert_eq!(launch_token, 8);
            assert!(hint.is_none());
        }
        other => panic!("expected ForeignResumeHintDetected, got {other:?}"),
    }
}
/// `FetchSessionList` wire shape: search sends `query` (no `allowRelax`); browse opts into `allowRelax` and parses `x.ai/listScope`.
/// All outcomes echo `seq` and `query`.
#[tokio::test]
async fn fetch_session_list_pushes_query_and_echoes_seq() {
    use std::sync::{Arc, Mutex};
    use distill_acp_lib::AcpAgentMessage;
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let captured_for_task = captured.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                assert_eq!(args.request.method.as_ref(), "x.ai/session/list");
                let params: serde_json::Value = serde_json::from_str(
                        args.request.params.get(),
                    )
                    .expect("params JSON");
                let fail = params.get("query").and_then(|q| q.as_str())
                    == Some("fail-me");
                let browse = params.get("query").is_none();
                captured_for_task.lock().unwrap().push(params);
                let body = if fail {
                    serde_json::json!({ "error": "boom" })
                } else if browse {
                    serde_json::json!({
                            "result": {
                                "sessions": [],
                                "_meta": { "x.ai/listScope": "repo" },
                            }
                        })
                } else {
                    serde_json::json!({ "result": { "sessions": [] } })
                };
                let raw = serde_json::value::RawValue::from_string(body.to_string())
                    .expect("serialize list response");
                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(Arc::from(raw))));
            }
        }
    });
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let run = |effect: Effect| {
        let mut tasks = JoinSet::new();
        execute(
            effect,
            &mut tasks,
            &tx,
            Path::new("."),
            &SessionFlags::default(),
            &progress_tx,
        );
        tasks
    };
    use crate::views::session_picker_surface::SessionPickerHost;
    let mut tasks = run(Effect::FetchSessionList {
        host: SessionPickerHost::AgentModal,
        cwd_override: None,
        generation: 41,
        query: Some("hit".into()),
        seq: 7,
        kind_filter: None,
        headless_policy: Default::default(),
    });
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::SessionListLoaded {
            host,
            generation,
            sessions,
            scope,
            seq,
            query,
            ..
        } => {
            assert!(sessions.is_empty());
            assert_eq!(host, SessionPickerHost::AgentModal, "host must be echoed");
            assert_eq!(generation, 41, "generation must be echoed");
            assert_eq!(seq, 7, "seq must be echoed, not reconstructed");
            assert_eq!(query.as_deref(), Some("hit"), "query must be echoed");
            assert!(
                    !scope.is_relaxed(),
                    "search responses carry no relaxed scope"
                );
        }
        other => panic!("expected SessionListLoaded, got {other:?}"),
    }
    let mut tasks = run(Effect::FetchSessionList {
        host: SessionPickerHost::Welcome,
        cwd_override: None,
        generation: 42,
        query: None,
        seq: 8,
        kind_filter: None,
        headless_policy: Default::default(),
    });
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::SessionListLoaded { scope, seq, query, .. } => {
            assert_eq!(seq, 8);
            assert_eq!(query, None);
            assert!(
                    scope.is_relaxed(),
                    "_meta[\"x.ai/listScope\"] must parse into the task result"
                );
        }
        other => panic!("expected SessionListLoaded, got {other:?}"),
    }
    let mut tasks = run(Effect::FetchSessionList {
        host: SessionPickerHost::Dashboard,
        cwd_override: Some("/dashboard-cwd".into()),
        generation: 44,
        query: None,
        seq: 10,
        kind_filter: Some(vec!["build".into()]),
        headless_policy: Default::default(),
    });
    assert!(matches!(
            tasks.join_next().await.expect("task").expect("no panic"),
            TaskResult::SessionListLoaded {
                host: SessionPickerHost::Dashboard,
                ..
            }
        ));
    let mut tasks = run(Effect::FetchSessionList {
        host: SessionPickerHost::Welcome,
        cwd_override: None,
        generation: 43,
        query: Some("fail-me".into()),
        seq: 9,
        kind_filter: None,
        headless_policy: Default::default(),
    });
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::SessionListFailed { host, generation, error, seq, query } => {
            assert_eq!(error, "boom");
            assert_eq!(host, SessionPickerHost::Welcome, "failure must echo the host");
            assert_eq!(generation, 43, "failure must echo the generation");
            assert_eq!(seq, 9);
            assert_eq!(
                    query.as_deref(),
                    Some("fail-me"),
                    "failure must echo the query (gates the indicator clear)"
                );
        }
        other => panic!("expected SessionListFailed, got {other:?}"),
    }
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 4);
    assert_eq!(j(nth(&captured, 0), "query"), "hit");
    assert_eq!(j(nth(&captured, 0), "limit"), 30);
    assert_eq!(j(nth(&captured, 0), "headless"), "exclude");
    assert_eq!(j(nth(&captured, 1), "headless"), "exclude");
    assert_eq!(j(nth(&captured, 2), "headless"), "exclude");
    assert_eq!(j(nth(&captured, 2), "limit"), 100);
    assert_eq!(j(nth(&captured, 2), "cwd"), "/dashboard-cwd");
    assert_eq!(j(nth(&captured, 3), "headless"), "exclude");
    assert_eq!(j(nth(&captured, 0), "cwd"), ".");
    assert!(
            nth(&captured, 0).get("allowRelax").is_none(),
            "search fetches must not opt into relaxing: {:?}",
            nth(&captured, 0)
        );
    assert!(
            nth(&captured, 1).get("query").is_none(),
            "plain fetch must not send a query key: {:?}",
            nth(&captured, 1)
        );
    assert_eq!(
            j(nth(&captured, 1), "allowRelax"), true,
            "browse fetches opt into relaxing"
        );
    assert_eq!(j(nth(&captured, 3), "query"), "fail-me");
}
#[tokio::test]
async fn fetch_dashboard_sessions_explicitly_excludes_headless() {
    use std::sync::{Arc, Mutex};
    use distill_acp_lib::AcpAgentMessage;
    let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
    let captured_for_task = captured.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                assert_eq!(args.request.method.as_ref(), "x.ai/session/list");
                let params = serde_json::from_str(args.request.params.get())
                    .expect("params JSON");
                *captured_for_task.lock().unwrap() = Some(params);
                let body = serde_json::json!({ "result": { "sessions": [] } });
                let raw = serde_json::value::RawValue::from_string(body.to_string())
                    .expect("serialize list response");
                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(Arc::from(raw))));
                break;
            }
        }
    });
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    execute(
        Effect::FetchDashboardSessions,
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::DashboardSessionsLoaded { sessions } => assert!(sessions.is_empty()),
        other => panic!("expected DashboardSessionsLoaded, got {other:?}"),
    }
    let captured = captured.lock().unwrap();
    let Some(params) = captured.as_ref() else {
        panic!("captured params");
    };
    assert_eq!(j(params, "headless"), "exclude");
}
#[tokio::test]
async fn fetch_session_list_sends_kind_facet_filter() {
    use std::sync::{Arc, Mutex};
    use distill_acp_lib::AcpAgentMessage;
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let captured_for_task = captured.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                let params: serde_json::Value = serde_json::from_str(
                        args.request.params.get(),
                    )
                    .expect("params JSON");
                captured_for_task.lock().unwrap().push(params);
                let body = serde_json::json!({ "result": { "sessions": [] } });
                let raw = serde_json::value::RawValue::from_string(body.to_string())
                    .expect("ser");
                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(Arc::from(raw))));
            }
        }
    });
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    execute(
        Effect::FetchSessionList {
            host: crate::views::session_picker_surface::SessionPickerHost::Welcome,
            cwd_override: None,
            generation: 1,
            query: None,
            seq: 1,
            kind_filter: Some(vec!["build".into()]),
            headless_policy: Default::default(),
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    let _ = tasks.join_next().await;
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
            j(j(j(nth(&captured, 0), "_meta"), "x.ai/facetFilters"), "kind"),
            &serde_json::json!(["build"])
        );
}
#[tokio::test]
async fn fetch_workflows_list_sends_session_id() {
    use std::sync::{Arc, Mutex};
    use distill_acp_lib::AcpAgentMessage;
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let captured_for_task = captured.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                assert_eq!(args.request.method.as_ref(), "x.ai/workflows/list");
                let params: serde_json::Value = serde_json::from_str(
                        args.request.params.get(),
                    )
                    .expect("params JSON");
                captured_for_task.lock().unwrap().push(params);
                let body = serde_json::json!({ "result": { "workflows": [] } });
                let raw = serde_json::value::RawValue::from_string(body.to_string())
                    .expect("serialize workflows response");
                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(Arc::from(raw))));
            }
        }
    });
    let session_id = acp::SessionId::new(Arc::from("test-session"));
    let mut tasks = JoinSet::new();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    execute(
        Effect::FetchWorkflowsList {
            agent_id: AgentId(3),
            session_id: session_id.clone(),
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::WorkflowsListLoaded {
            agent_id,
            session_id: result_session_id,
            result,
        } => {
            assert_eq!(agent_id, AgentId(3));
            assert_eq!(result_session_id, session_id);
            assert!(result.expect("workflows load").is_empty());
        }
        other => panic!("expected WorkflowsListLoaded, got {other:?}"),
    }
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(j(nth(&captured, 0), "sessionId"), "test-session");
    assert!(nth(&captured, 0).get("cwd").is_none());
}
/// The debounce arm must echo `query` and `seq` exactly.
/// Awaits the real 250 ms debounce (tokio's paused clock needs `test-util`, not enabled in this crate).
#[tokio::test]
async fn debounce_session_search_echoes_query_and_seq() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    use crate::views::session_picker_surface::SessionPickerHost;
    execute(
        Effect::DebounceSessionSearch {
            host: SessionPickerHost::Welcome,
            generation: 5,
            query: "abc".into(),
            seq: 9,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::SessionSearchDebounceExpired { host, generation, query, seq } => {
            assert_eq!(host, SessionPickerHost::Welcome, "host must be echoed");
            assert_eq!(generation, 5, "generation must be echoed");
            assert_eq!(query, "abc");
            assert_eq!(seq, 9);
        }
        other => panic!("expected SessionSearchDebounceExpired, got {other:?}"),
    }
}
/// The deep-search executor must echo host, generation, and seq verbatim and carry the selected Headless policy on the wire.
#[tokio::test]
async fn deep_search_sessions_echoes_routing_and_policy() {
    use std::sync::{Arc, Mutex};
    use crate::views::session_picker_surface::SessionPickerHost;
    use distill_acp_lib::AcpAgentMessage;
    let captured: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::default();
    let captured_for_task = captured.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let AcpAgentMessage::ExtMethod(args) = msg {
                let params = serde_json::from_str(args.request.params.get())
                    .expect("deep-search params JSON");
                captured_for_task
                    .lock()
                    .unwrap()
                    .push((args.request.method.as_ref().to_string(), params));
                let body = serde_json::json!({
                        "result": { "results": [], "bootstrapping": false }
                    });
                let raw = serde_json::value::RawValue::from_string(body.to_string())
                    .expect("serialize search response");
                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(Arc::from(raw))));
            }
        }
    });
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    execute(
        Effect::DeepSearchSessions {
            host: SessionPickerHost::AgentModal,
            generation: 11,
            query: "abc".into(),
            seq: 4,
            headless_policy: distill_shell::session::unified_list::HeadlessPolicy::Only,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::DeepSearchResults { host, generation, results, seq } => {
            assert_eq!(host, SessionPickerHost::AgentModal, "host must be echoed");
            assert_eq!(generation, 11, "generation must be echoed");
            assert_eq!(seq, 4, "seq must be echoed");
            assert!(results.is_empty());
        }
        other => panic!("expected DeepSearchResults, got {other:?}"),
    }
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(nth(&captured, 0).0, "x.ai/session/search");
    assert_eq!(j(&nth(&captured, 0).1, "headless"), "only");
}
/// The card-detail executor must echo host, generation, seq, and the row identity verbatim; a session missing on disk zeroes the stats.
#[tokio::test]
async fn load_card_detail_echoes_identity_and_zeroes_missing_session() {
    use crate::views::session_picker_surface::SessionPickerHost;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = JoinSet::new();
    execute(
        Effect::LoadCardDetail {
            host: SessionPickerHost::Welcome,
            generation: 6,
            source: "local".into(),
            session_id: "wire-echo-missing-session-e5b1".into(),
            cwd: "/nonexistent/effects-test".into(),
            seq: 3,
        },
        &mut tasks,
        &tx,
        Path::new("."),
        &SessionFlags::default(),
        &progress_tx,
    );
    match tasks.join_next().await.expect("task").expect("no panic") {
        TaskResult::CardDetailLoaded {
            host,
            generation,
            source,
            session_id,
            seq,
            detail,
        } => {
            assert_eq!(host, SessionPickerHost::Welcome, "host must be echoed");
            assert_eq!(generation, 6, "generation must be echoed");
            assert_eq!(seq, 3, "seq must be echoed");
            assert_eq!(source, "local");
            assert_eq!(session_id, "wire-echo-missing-session-e5b1");
            assert_eq!(detail.turn_count, 0);
            assert_eq!(detail.tool_call_count, 0);
            assert!(detail.first_prompt_preview.is_empty());
        }
        other => panic!("expected CardDetailLoaded, got {other:?}"),
    }
}
/// Verify that every profile name produced by `SessionFlags::agent_profile()` is a valid `BuiltinAgentName` that the shell can resolve.
#[test]
fn agent_profile_names_are_valid_builtins() {
    use std::str::FromStr;
    use distill_agent::config::BuiltinAgentName;
    let test_cases: &[(SessionFlags, &str)] = &[
        (
            SessionFlags {
                plan_mode: true,
                subagents: true,
                ask_user: false,
                ..Default::default()
            },
            "distill-plan",
        ),
        (
            SessionFlags {
                plan_mode: true,
                subagents: false,
                ask_user: false,
                ..Default::default()
            },
            "distill-plan-no-subagents",
        ),
        (
            SessionFlags {
                plan_mode: true,
                subagents: true,
                ask_user: true,
                ..Default::default()
            },
            "distill-plan",
        ),
        (
            SessionFlags {
                plan_mode: true,
                subagents: false,
                ask_user: true,
                ..Default::default()
            },
            "distill-plan-no-subagents",
        ),
        (
            SessionFlags {
                plan_mode: false,
                subagents: false,
                ask_user: true,
                ..Default::default()
            },
            "distill-ask-user",
        ),
        (
            SessionFlags {
                plan_mode: false,
                subagents: true,
                ask_user: true,
                ..Default::default()
            },
            "distill-ask-user",
        ),
    ];
    for (flags, expected_name) in test_cases {
        let profile = flags.agent_profile();
        assert_eq!(
                profile,
                Some(*expected_name),
                "flags {flags:?} should produce profile {expected_name:?}"
            );
        let builtin = BuiltinAgentName::from_str(expected_name);
        assert!(
                builtin.is_ok(),
                "profile name {expected_name:?} is not a valid BuiltinAgentName: {:?}",
                builtin.err()
            );
    }
}
/// Default flags produce no agent profile (uses Distill default).
#[test]
fn default_flags_produce_no_profile() {
    let flags = SessionFlags::default();
    assert_eq!(flags.agent_profile(), None);
}
/// --subagents alone produces no profile (Distill already has TaskTool).
#[test]
fn subagents_without_plan_produces_no_profile() {
    let flags = SessionFlags {
        plan_mode: false,
        subagents: true,
        ask_user: false,
        ..Default::default()
    };
    assert_eq!(flags.agent_profile(), None);
}
/// Neutralize `GROK_AGENT` for the profile-matrix tests below.
/// The tests would then assert the wrong branch.
/// Callers must be `#[serial_test::serial(GROK_AGENT)]` (process-global env).
fn without_grok_agent() -> crate::test_util::EnvVarGuard {
    crate::test_util::EnvVarGuard::set("GROK_AGENT", "")
}
/// At the runtime defaults every `--no-*` flag is false, so every `SessionFlags` bool is true via `!args.no_*`.
/// `to_meta()` then reflects the full plan profile and no separate `askUserQuestion` toggle.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn runtime_default_flags_produce_plan_meta() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: true,
        subagents: true,
        ask_user: true,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-plan");
    assert!(meta.get("askUserQuestion").is_none());
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// --plan alone produces meta with `agentProfile` only and a `askUserQuestion: false` since `ask_user` is off here.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn plan_only_meta() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: true,
        subagents: false,
        ask_user: false,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-plan-no-subagents");
    assert_eq!(j(&meta, "askUserQuestion"), false);
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// --plan --subagents selects the full plan profile.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn plan_with_subagents_meta() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: true,
        subagents: true,
        ask_user: false,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-plan");
    assert_eq!(j(&meta, "askUserQuestion"), false);
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// --ask-user alone selects the Distill-ask-user profile.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn ask_user_alone_meta() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: false,
        subagents: false,
        ask_user: true,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-ask-user");
    assert!(meta.get("askUserQuestion").is_none());
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// --plan --ask-user: plan already includes ask-user; profile is plan.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn plan_with_ask_user_uses_plan_profile() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: true,
        subagents: false,
        ask_user: true,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-plan-no-subagents");
    assert!(meta.get("askUserQuestion").is_none());
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// --no-plan --no-subagents --no-ask-user picks the default profile.
/// It must still emit `askUserQuestion: false` so the shell can strip the tool at the builder.
/// Mirrors the runtime: the `subagents` toggle alone does not need an `agentProfile` (default `Distill` already has it).
#[test]
fn subagents_alone_emits_only_ask_user_question_disable() {
    let flags = SessionFlags {
        plan_mode: false,
        subagents: true,
        ask_user: false,
        ..Default::default()
    };
    let meta = flags.to_meta().expect("askUserQuestion=false must produce meta");
    assert!(meta.get("agentProfile").is_none());
    assert_eq!(j(&meta, "askUserQuestion"), false);
}
/// All three flags on at the runtime default produce Distill-plan and no `askUserQuestion` field.
#[serial_test::serial(GROK_AGENT)]
#[test]
fn all_flags_meta() {
    let _env = without_grok_agent();
    let flags = SessionFlags {
        plan_mode: true,
        subagents: true,
        ask_user: true,
        ..Default::default()
    };
    let meta = flags.to_meta().unwrap();
    assert_eq!(j(&meta, "agentProfile"), "distill-plan");
    assert!(meta.get("askUserQuestion").is_none());
    assert_eq!(j(&meta, "yoloMode"), false);
}
/// `--no-ask-user` must land as `_meta.askUserQuestion = false` regardless of which profile (if any) the other flags select.
#[test]
fn to_meta_emits_ask_user_question_false_when_disabled() {
    for plan in [false, true] {
        for subagents in [false, true] {
            let flags = SessionFlags {
                plan_mode: plan,
                subagents,
                ask_user: false,
                ..Default::default()
            };
            let meta = flags
                .to_meta()
                .unwrap_or_else(|| {
                    panic!(
                        "ask_user=false must always emit meta (plan={plan}, subagents={subagents})"
                    )
                });
            assert_eq!(
                    j(&meta, "askUserQuestion"), false,
                    "askUserQuestion must be false (plan={plan}, subagents={subagents}); meta={meta:?}"
                );
        }
    }
}
/// Symmetric positive control: when `ask_user` is enabled the field is omitted entirely.
/// The shell defaults to enabled when the key is absent (see `parse_ask_user_question_from_meta`).
#[test]
fn to_meta_omits_ask_user_question_when_enabled() {
    for plan in [false, true] {
        for subagents in [false, true] {
            let flags = SessionFlags {
                plan_mode: plan,
                subagents,
                ask_user: true,
                ..Default::default()
            };
            if let Some(meta) = flags.to_meta() {
                assert!(
                        meta.get("askUserQuestion").is_none(),
                        "askUserQuestion must be absent when enabled (plan={plan}, subagents={subagents}); meta={meta:?}"
                    );
            }
        }
    }
}
#[test]
fn to_meta_emits_auto_mode_when_enabled() {
    let flags = SessionFlags {
        auto_mode: true,
        yolo_mode: false,
        ..Default::default()
    };
    let meta = flags.to_meta().expect("auto_mode must emit meta");
    assert_eq!(j(&meta, "autoMode"), true);
    assert_eq!(
            j(&meta, "yoloMode"), false,
            "yoloMode must be explicitly false, not omitted (absent key falls \
             back to the shell's connect-time default / leader injection)"
        );
}
#[test]
fn create_permission_override_replaces_global_permission_seeds() {
    let flags = SessionFlags {
        yolo_mode: true,
        auto_mode: false,
        ..Default::default()
    };
    let mut meta = flags.to_meta();
    apply_permission_mode_override(&mut meta, Some(PermissionModeKind::Auto));
    let meta = meta.expect("permission metadata");
    assert_eq!(j(&meta, "yoloMode"), false);
    assert_eq!(j(&meta, "autoMode"), true);
}
/// yoloMode must ride the meta explicitly for both polarities; an absent key does not mean off (see the emit-site comment in `to_meta`).
/// Cycling Always-Approve back to Normal before the session starts must not create a yolo session.
#[test]
fn to_meta_always_emits_yolo_mode_explicitly() {
    for yolo in [false, true] {
        let flags = SessionFlags {
            yolo_mode: yolo,
            ..Default::default()
        };
        let meta = flags.to_meta().expect("permission seeds must always emit meta");
        assert_eq!(
                j(&meta, "yoloMode"),
                &serde_json::json!(yolo),
                "yoloMode must be explicit (yolo={yolo}); meta={meta:?}"
            );
    }
}
#[test]
fn to_meta_chat_mode_stamps_kind_and_omits_agent_profile() {
    let flags = SessionFlags {
        chat_mode: true,
        plan_mode: true,
        subagents: true,
        ask_user: true,
        ..Default::default()
    };
    let meta = flags.to_meta().expect("chat_mode must emit meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert!(
            meta.get("agentProfile").is_none(),
            "K12: chat mode must omit Build agentProfile"
        );
    assert_chat_meta_has_no_workspace_bind_keys(
        &serde_json::Value::Object(meta.clone()),
    );
}
/// Load meta merge: explicit `chat_kind` alone (no process-wide chat_mode) stamps kind and strips agentProfile, the conversation-resume case.
#[test]
fn load_meta_chat_kind_alone_stamps_kind_and_strips_profile() {
    let flags = SessionFlags {
        chat_mode: false,
        plan_mode: true,
        subagents: true,
        ask_user: true,
        ..Default::default()
    };
    let mut meta = flags.to_meta();
    let chat_kind = true;
    if chat_kind || flags.chat_mode {
        apply_chat_kind_meta(&mut meta);
        scrub_chat_workspace_bind_meta(&mut meta);
    }
    let meta = meta.expect("chat_kind must produce meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert!(
            meta.get("agentProfile").is_none(),
            "entry chat_kind must strip Build agentProfile"
        );
    assert_chat_meta_has_no_workspace_bind_keys(
        &serde_json::Value::Object(meta.clone()),
    );
}
/// Chat create and load meta must never include client workspace-bind keys (`envId`, Direct hub id, gateway attach).
/// That holds even when cloud fields are present on the effect; the backend owns the workspace for `kind=chat`.
fn assert_chat_meta_has_no_workspace_bind_keys(meta: &serde_json::Value) {
    for key in CHAT_FORBIDDEN_WORKSPACE_BIND_KEYS {
        assert!(
                meta.get(*key).is_none(),
                "chat meta must not include workspace-bind key {key:?}: {meta}"
            );
    }
    assert!(
            meta.get("x.ai/cloud_existing_workspace").is_none(),
            "chat meta without attach must not include existing workspace: {meta}"
        );
}
#[test]
fn chat_create_meta_never_includes_workspace_bind_keys_when_cloud_fields_set() {
    let flags = SessionFlags {
        chat_mode: true,
        ..Default::default()
    };
    let mut meta = flags.to_meta();
    apply_chat_kind_meta(&mut meta);
    scrub_chat_workspace_bind_meta(&mut meta);
    let meta = meta.expect("chat create must emit meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert_chat_meta_has_no_workspace_bind_keys(
        &serde_json::Value::Object(meta.clone()),
    );
}
#[test]
fn chat_load_meta_never_includes_workspace_bind_keys() {
    let flags = SessionFlags::default();
    let mut meta = flags.to_meta();
    apply_chat_kind_meta(&mut meta);
    {
        let obj = meta.get_or_insert_with(acp::Meta::new);
        obj.insert("envId".into(), serde_json::json!("env-poison"));
        obj.insert("x.ai/cloud_server_id".into(), serde_json::json!("srv-poison"));
        obj.insert(
            "x.ai/cloud_existing_workspace".into(),
            serde_json::json!({
                    "server_id": "srv-poison",
                    "cwd": "/ws",
                }),
        );
    }
    scrub_chat_workspace_bind_meta(&mut meta);
    let meta = meta.expect("chat load must emit meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert_chat_meta_has_no_workspace_bind_keys(
        &serde_json::Value::Object(meta.clone()),
    );
}
/// The attach stamp keeps the existing workspace and local intent; envId and Direct hub stay stripped.
#[cfg(feature = "local-workspace")]
#[test]
fn scrub_chat_workspace_matrix_attach_exception() {
    use crate::app::session_startup::{LocalWorkspaceConfig, LocalWorkspaceMode};
    let mut meta = Some(acp::Meta::new());
    {
        let obj = meta.as_mut().unwrap();
        obj.insert("envId".into(), serde_json::json!("env-x"));
        obj.insert("x.ai/cloud_server_id".into(), serde_json::json!("hub-x"));
        obj.insert(
            "x.ai/cloud_existing_workspace".into(),
            serde_json::json!({"server_id": "srv-x", "cwd": "/ws"}),
        );
    }
    scrub_chat_workspace_bind_meta(&mut meta);
    let scrubbed = meta.as_ref().unwrap();
    assert!(scrubbed.get("envId").is_none());
    assert!(scrubbed.get("x.ai/cloud_server_id").is_none());
    assert!(scrubbed.get("x.ai/cloud_existing_workspace").is_none());
    let mut meta = Some(acp::Meta::new());
    apply_local_workspace_meta(
        &mut meta,
        &LocalWorkspaceConfig {
            mode: LocalWorkspaceMode::Attach,
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
            server_id: Some("srv-dogfood".into()),
        },
    );
    {
        let obj = meta.as_mut().unwrap();
        obj.insert("envId".into(), serde_json::json!("env-must-go"));
        obj.insert("x.ai/cloud_server_id".into(), serde_json::json!("hub-must-go"));
    }
    scrub_chat_workspace_bind_meta(&mut meta);
    let scrubbed = meta.as_ref().unwrap();
    assert!(scrubbed.get("envId").is_none(), "envId must stay scrubbed");
    assert!(
            scrubbed.get("x.ai/cloud_server_id").is_none(),
            "Direct hub must stay scrubbed"
        );
    assert_eq!(
            j(j(&scrubbed, "x.ai/cloud_existing_workspace"), "server_id"),
            "srv-dogfood"
        );
    assert_eq!(j(j(&scrubbed, "x.ai/local_workspace"), "mode"), "attach");
    assert_eq!(j(j(&scrubbed, "x.ai/local_workspace"), "server_id"), "srv-dogfood");
    assert_eq!(j(j(&scrubbed, "x.ai/local_workspace"), "cwd"), "/tmp/repo");
}
#[cfg(feature = "local-workspace")]
#[test]
fn to_meta_chat_attach_stamps_local_and_existing() {
    use crate::app::session_startup::{LocalWorkspaceConfig, LocalWorkspaceMode};
    let flags = SessionFlags {
        chat_mode: true,
        local_workspace: Some(LocalWorkspaceConfig {
            mode: LocalWorkspaceMode::Attach,
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
            server_id: Some("srv-1".into()),
        }),
        ..Default::default()
    };
    let meta = flags.to_meta().expect("meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert_eq!(j(j(&meta, "x.ai/local_workspace"), "mode"), "attach");
    assert_eq!(j(j(&meta, "x.ai/cloud_existing_workspace"), "server_id"), "srv-1");
    assert!(meta.get("envId").is_none());
    assert!(meta.get("x.ai/cloud_server_id").is_none());
}
#[cfg(feature = "local-workspace")]
#[test]
fn to_meta_chat_own_stamps_intent_without_existing() {
    use crate::app::session_startup::{LocalWorkspaceConfig, LocalWorkspaceMode};
    let flags = SessionFlags {
        chat_mode: true,
        local_workspace: Some(LocalWorkspaceConfig {
            mode: LocalWorkspaceMode::Own,
            cwd: Some(std::path::PathBuf::from("/tmp/repo-own")),
            server_id: None,
        }),
        ..Default::default()
    };
    let meta = flags.to_meta().expect("meta");
    assert_eq!(j(j(&meta, "x.ai/local_workspace"), "mode"), "own");
    assert_eq!(j(j(&meta, "x.ai/local_workspace"), "cwd"), "/tmp/repo-own");
    assert!(j(&meta, "x.ai/local_workspace").get("server_id").is_none());
    assert!(
            meta.get("x.ai/cloud_existing_workspace").is_none(),
            "own must not stamp existing; shell mints server_id"
        );
    assert!(meta.get("envId").is_none());
}
#[cfg(feature = "local-workspace")]
#[test]
fn reject_non_fs_only_advertised_tools_matrix() {
    let fs_only = ["workspace.fs_list", "workspace.fs_read_file", "workspace.put_files"];
    assert!(reject_non_fs_only_advertised_tools(Some(fs_only.as_slice())).is_ok());
    assert!(
            reject_non_fs_only_advertised_tools(None)
                .unwrap_err()
                .contains("uncheckable")
        );
    assert!(
            reject_non_fs_only_advertised_tools(Some(&[] as &[_]))
                .unwrap_err()
                .contains("empty")
        );
    let with_exec = ["workspace.fs_list", "workspace.bash", "terminal.exec"];
    let err = reject_non_fs_only_advertised_tools(Some(with_exec.as_slice()))
        .unwrap_err();
    assert!(err.contains("FS-only"), "{err}");
    assert!(err.contains("workspace.bash"), "{err}");
    assert!(err.contains("terminal.exec"), "{err}");
}
#[cfg(feature = "local-workspace")]
#[test]
fn finalize_chat_session_meta_stamps_attach_on_worktree_path() {
    use crate::app::session_startup::{LocalWorkspaceConfig, LocalWorkspaceMode};
    let flags = SessionFlags {
        chat_mode: false,
        local_workspace: Some(LocalWorkspaceConfig {
            mode: LocalWorkspaceMode::Attach,
            cwd: Some(std::path::PathBuf::from("/tmp/repo")),
            server_id: Some("srv-wt".into()),
        }),
        ..Default::default()
    };
    let mut meta = flags.to_meta();
    finalize_chat_session_meta(&mut meta, true, &flags);
    let meta = meta.expect("meta");
    assert_eq!(j(j(&meta, "x.ai/session"), "kind"), "chat");
    assert_eq!(j(j(&meta, "x.ai/local_workspace"), "mode"), "attach");
    assert_eq!(j(j(&meta, "x.ai/cloud_existing_workspace"), "server_id"), "srv-wt");
    assert!(meta.get("envId").is_none());
}
#[test]
fn to_meta_yolo_suppresses_auto_mode() {
    let flags = SessionFlags {
        auto_mode: true,
        yolo_mode: true,
        ..Default::default()
    };
    let meta = flags.to_meta().expect("yolo must emit meta");
    assert_eq!(j(&meta, "yoloMode"), true);
    assert_eq!(
            j(&meta, "autoMode"), false,
            "yolo wins; autoMode must be explicitly false (not omitted)"
        );
}
/// Verify that each resolved profile name produces a valid `AgentDefinition` whose name matches the expected kebab-case string.
#[test]
fn agent_profile_definitions_have_correct_names() {
    use std::str::FromStr;
    use distill_agent::config::BuiltinAgentName;
    for name in [
        "distill-plan",
        "distill-plan-no-subagents",
        "distill-ask-user",
    ] {
        let builtin = BuiltinAgentName::from_str(name).unwrap();
        let def = builtin.definition();
        assert_eq!(
                def.name, name,
                "definition name should match the kebab-case profile name"
            );
    }
}
fn make_session_info(
    model: &str,
    resolved: Option<&str>,
    used: u64,
    total: u64,
) -> distill_shell::session::SessionInfoResponse {
    use distill_shell::session::acp_types::{ContextInfo, SessionInfoData};
    distill_shell::session::SessionInfoResponse {
        session_id: "test-session-id".into(),
        cwd: "/tmp/test".into(),
        data: SessionInfoData {
            agent_name: None,
            model: Some(model.into()),
            model_display_name: None,
            resolved_model_id: resolved.map(Into::into),
            model_fingerprint: None,
            show_model_fingerprint: false,
            api_backend: None,
            conversation_id: None,
            turns: 0,
            turn_index: 0,
            context: ContextInfo {
                used,
                total,
                auto_compact_threshold_percent: 85,
                ..Default::default()
            },
        },
    }
}
#[test]
fn format_session_info_session_auth_ignores_api_key_env() {
    let info = make_session_info("auto", None, 1000, 10000);
    let text = format_session_info(&info, None, false, false, true);
    assert!(text.contains("Auth method: OAuth"), "{text}");
    assert!(!text.contains("Manage account and credits"), "{text}");
    assert!(!text.contains("Also present: XAI_API_KEY"), "{text}");
    assert!(!text.contains("console.x.ai"), "{text}");
    assert!(!text.contains("grok login"), "{text}");
}
#[test]
fn format_session_info_api_key_without_env() {
    let info = make_session_info("auto", None, 1000, 10000);
    let text = format_session_info(&info, None, false, true, false);
    assert!(text.contains("Auth method: API key\n"), "{text}");
    assert!(!text.contains("XAI_API_KEY"), "{text}");
    assert!(!text.contains("Manage account and credits"), "{text}");
    assert!(
            text.contains("Run `grok login` to use your SuperGrok subscription instead."),
            "{text}"
        );
    assert!(!text.contains("grok.com"), "{text}");
}
#[test]
fn format_session_info_api_key_auth_suggests_grok_login() {
    let info = make_session_info("auto", None, 1000, 10000);
    let text = format_session_info(&info, None, false, true, true);
    assert!(text.contains("Auth method: API key (XAI_API_KEY)"), "{text}");
    assert!(!text.contains("Manage account and credits"), "{text}");
    assert!(
            text.contains("Run `grok login` to use your SuperGrok subscription instead."),
            "{text}"
        );
    assert!(!text.contains("Also present: XAI_API_KEY"), "{text}");
    assert!(!text.contains("console.x.ai"), "{text}");
    assert!(!text.contains("grok.com"), "{text}");
}
#[test]
fn format_session_info_session_only_shows_oauth() {
    let info = make_session_info("auto", None, 1000, 10000);
    let text = format_session_info(&info, None, false, false, false);
    assert!(text.contains("Auth method: OAuth"), "{text}");
    assert!(!text.contains("Manage account and credits"), "{text}");
    assert!(!text.contains("Also present: XAI_API_KEY"), "{text}");
    assert!(!text.contains("console.x.ai"), "{text}");
    assert!(!text.contains("grok login"), "{text}");
}
#[test]
fn format_session_info_shows_conversation_id_when_present() {
    let mut info = make_session_info("auto", None, 1000, 10000);
    info.data.conversation_id = Some("conv_abc123".into());
    let text = format_session_info(&info, None, false, false, false);
    assert!(text.contains("Conversation ID: conv_abc123"));
    assert!(text.contains("Session ID: test-session-id"));
}
#[test]
fn format_session_info_shows_resolved_when_enabled_and_different() {
    let info = make_session_info("grok-4.5", Some("grok-4.3"), 1000, 10000);
    let text = format_session_info(&info, None, true, false, false);
    assert!(text.contains("Model: grok-4.5 (grok-4.3)"));
}
#[test]
fn format_session_info_hides_resolved_when_disabled() {
    let info = make_session_info("grok-4.5", Some("grok-4.3"), 1000, 10000);
    let text = format_session_info(&info, None, false, false, false);
    assert!(text.contains("Model: grok-4.5"));
    assert!(!text.contains("grok-4.3"));
}
/// The (cwd, id)-derived summary path resolves and `generated_title` wins.
#[tokio::test]
async fn lookup_session_title_loads_single_summary_by_cwd() {
    let root = tempfile::tempdir().unwrap();
    let cwd = "/workspace";
    let dir = root
        .path()
        .join("sessions")
        .join(distill_shell::util::distill_home::encode_cwd_dirname(cwd))
        .join("sess-1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
            dir.join("summary.json"),
            serde_json::json!({
                "info": { "id": "sess-1", "cwd": cwd },
                "session_summary": "raw summary",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "num_messages": 1,
                "current_model_id": "m",
                "generated_title": "Renamed title"
            })
                .to_string(),
        )
        .unwrap();
    let id = acp::SessionId::new("sess-1");
    let title = lookup_session_title_in(root.path().to_path_buf(), &id, cwd).await;
    assert_eq!(title.as_deref(), Some("Renamed title"));
}
#[test]
fn format_session_info_no_parens_when_resolved_matches_requested() {
    let info = make_session_info("grok-4.5", Some("grok-4.5"), 1000, 10000);
    let text = format_session_info(&info, None, true, false, false);
    assert!(text.contains("Model: grok-4.5"));
    assert!(!text.contains("(grok-4.5)"));
}
#[test]
fn format_session_info_shows_model_hash_when_catalog_flag_set() {
    let mut info = make_session_info("v9", None, 1000, 10000);
    info.data.model_fingerprint = Some("abc123".into());
    info.data.show_model_fingerprint = true;
    let text = format_session_info(&info, None, false, false, false);
    assert!(text.contains("Model Hash: abc123"));
}
#[test]
fn format_session_info_hides_model_hash_for_noncoding_without_flag() {
    let mut info = make_session_info("v9", None, 1000, 10000);
    info.data.model_fingerprint = Some("abc123".into());
    info.data.show_model_fingerprint = false;
    let text = format_session_info(&info, None, false, false, false);
    assert!(!text.contains("Model Hash"));
}
#[test]
fn format_session_info_hides_model_hash_for_coding_slug_without_flag() {
    let mut info = make_session_info("grok-4.6", None, 1000, 10000);
    info.data.model_fingerprint = Some("abc123".into());
    info.data.show_model_fingerprint = false;
    let text = format_session_info(&info, None, false, false, false);
    assert!(!text.contains("Model Hash"));
}
#[test]
fn session_picker_summary_strips_skill_xml() {
    use distill_tools::implementations::skills::skill::extract_skill_display_text;
    let summary = "<command-name>pr-babysit</command-name>\n\
                        <command-message>/pr-babysit</command-message>\n\
                        <command-args>check</command-args>"
        .to_string();
    let display = extract_skill_display_text(&summary).unwrap_or(summary);
    assert_eq!(display, "/pr-babysit check");
}
#[test]
fn session_picker_summary_preserves_normal_text() {
    use distill_tools::implementations::skills::skill::extract_skill_display_text;
    let summary = "Fix authentication bug in login flow".to_string();
    let display = extract_skill_display_text(&summary).unwrap_or(summary);
    assert_eq!(display, "Fix authentication bug in login flow");
}
#[test]
fn sanitize_user_error_rewrites_shared_service_names() {
    for (pattern, replacement) in distill_shell::sampling::error::SERVICE_NAME_REWRITES {
        for variant in [pattern.to_string(), pattern.to_ascii_uppercase()] {
            assert_eq!(
                    sanitize_user_error(&format!("ACP error: {variant} unreachable")),
                    format!("error: {replacement} unreachable")
                );
        }
    }
}
#[test]
fn compact_error_message_empty_data_renders_terse_and_no_data_uses_display() {
    let empty = compact_error_message(&acp::Error::internal_error().data(""));
    assert_eq!(empty, "");
    assert_eq!(
            crate::scrollback::blocks::SessionEvent::CompactionFailed { error: empty }.message(),
            "Compaction failed."
        );
    assert_eq!(
            compact_error_message(&acp::Error::internal_error()),
            "Internal error"
        );
}
#[test]
fn sanitize_user_error_strips_auth_prefixes() {
    assert_eq!(
            sanitize_user_error(
                "Authentication required: Login timed out after 10 minutes. Please try again."
            ),
            "Login timed out after 10 minutes. Please try again."
        );
    assert_eq!(
            sanitize_user_error("Authentication failed: something went wrong"),
            "something went wrong"
        );
    assert_eq!(
            sanitize_user_error("Login timed out after 10 minutes. Please try again."),
            "Login timed out after 10 minutes. Please try again."
        );
}
#[test]
fn sanitize_user_error_collapses_disk_full() {
    assert_eq!(
            sanitize_user_error(
                "couldn't create worktree: Internal error: \"hub error: Worktree creation failed: not enough free disk space\""
            ),
            "No space left on device"
        );
    assert_eq!(
            sanitize_user_error(
                "couldn't create worktree: failed to copy index: No space left on device (os error 28)"
            ),
            "No space left on device"
        );
    assert_eq!(
            sanitize_user_error("Internal error: \"Disk quota exceeded or out of space.\""),
            "No space left on device"
        );
    assert_eq!(
            sanitize_user_error("couldn't create worktree: failed to get HEAD commit from source"),
            "couldn't create worktree: failed to get HEAD commit from source"
        );
}
/// Production ordering of the deferred worktree resume failure: the detail is sanitized first, then composed.
/// Sanitizing the composed message would collapse a disk-full chain whole and erase the title hint for a deferred local-miss target.
#[test]
fn worktree_resume_failure_sanitizes_detail_before_hint() {
    use crate::app::session_title_resolve::worktree_resume_failure_message;
    let raw = "failed to copy index: No space left on device (os error 28)";
    let msg = worktree_resume_failure_message(
        Some("typo title"),
        &sanitize_user_error(raw),
    );
    assert_eq!(
            msg,
            format!(
                "couldn't resume worktree session: No space left on device; {}",
                crate::app::session_title_resolve::title_miss_hint("typo title")
            )
        );
    let id_msg = worktree_resume_failure_message(None, &sanitize_user_error(raw));
    assert_eq!(id_msg, "couldn't resume worktree session: No space left on device");
}
#[test]
fn rewind_execute_params_sends_conversation_only_with_force() {
    let params = rewind_execute_params("sess-1", 3);
    assert_eq!(j(&params, "sessionId"), "sess-1");
    assert_eq!(j(&params, "targetPromptIndex"), 3);
    assert_eq!(j(&params, "force"), true);
    assert_eq!(j(&params, "mode"), REWIND_MODE_WIRE);
    assert_eq!(j(&params, "mode"), "conversation_only");
}
/// Exact wire bytes of the one-shot request: the shell's `upload_trace_offer_gate_allows`
/// relaxation keys off this exact snake_case value, so the shape is a cross-crate contract.
#[test]
fn upload_trace_request_with_intent_exact_wire_shape() {
    let request = UploadTraceRequest {
        session_id: "sess-1".to_string(),
        intent: Some(
            crate::views::feedback_modal::FeedbackTraceUploadIntent::SendThisSession,
        ),
        trace_upload_token: Some("grant-1".to_string()),
    };
    assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"sessionId":"sess-1","intent":"send_this_session","traceUploadToken":"grant-1"}"#
        );
}
/// A legacy trace-card upload must stay byte-identical to the pre-intent request
/// (no `"intent":null`), so an older shell's strict parsing cannot regress.
#[test]
fn upload_trace_request_without_intent_keeps_legacy_wire_shape() {
    let request = UploadTraceRequest {
        session_id: "sess-1".to_string(),
        intent: None,
        trace_upload_token: None,
    };
    assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"sessionId":"sess-1"}"#
        );
}
