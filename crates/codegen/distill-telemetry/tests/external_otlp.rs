// Modified for Distill by Samuel Fajreldines, 2026.
//! Integration test for the external OTEL stream against `distill_test_support::MockOtelServer`.
//! Covers wire payloads, delta temporality, canary absence at the wire layer with gates off, flush on shutdown within 2 s, and post-shutdown silence.

use std::time::Duration;

use serde_json::Value;
use distill_telemetry::external::IdentityAttrs;
use distill_test_support::{
    MockOtelServer, OtelMetricData, OtelNumber, OtelSignal, OtelTemporality,
};

const CANARY_MODEL: &str = "sk-CANARYabcdefghij1234567890";
const CANARY_PROMPT: &str = "CANARY_PROMPT_TEXT do not export";
const CANARY_MCP: &str = "canary-internal-mcp-server";
const CANARY_CMD: &str = "CANARY_BASH_COMMAND_ls_la";
const CANARY_RESPONSE: &str = "CANARY_ASSISTANT_PROSE do not export";
const OAUTH_EMAIL: &str = "otel.parity@example.com";

fn deny_decision(
    command: &str,
    tool_use_id: &str,
) -> distill_telemetry::events::PermissionDecisionRecord {
    distill_telemetry::events::PermissionDecisionRecord {
        payload: distill_telemetry::events::PermissionDecisionPayload {
            tool_name: "run_terminal_cmd".into(),
            access_kind: distill_telemetry::events::AccessKind::Bash,
            decision: distill_telemetry::events::PermissionOutcome::Deny,
            wait_ms: 10,
            permission_mode: distill_telemetry::enums::PermissionMode::Ask,
            source: Some("user_reject".into()),
            subagent_session_id: None,
            subagent_type: None,
            manager_prompt_attempted: None,
            prompt_outcome: None,
            prompt_outcome_detail: None,
            remember_tool_approvals: None,
            decision_reason: None,
            classifier_source: None,
            classifier_verdict: None,
            security_findings: None,
            classifier_latency_ms: None,
            auto_denials_consecutive: None,
            auto_denials_total: None,
        },
        tool_input: distill_telemetry::events::ExternalToolInput {
            parameters: Some(serde_json::json!({ "command": command })),
            tool_use_id: Some(tool_use_id.into()),
        },
    }
}

#[tokio::test]
async fn external_stream_end_to_end() {
    let server = MockOtelServer::start().await.unwrap();

    let env = server.exporter_env();
    let mut cfg = distill_telemetry::external::ExternalOtelConfig::resolve_with(
        |name| env.get(name).cloned(),
        None,
    )
    .expect("double opt-in must resolve");
    cfg.client = distill_telemetry::external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };

    distill_telemetry::external::init(Some(cfg));
    assert!(distill_telemetry::external::is_active());

    distill_telemetry::external::set_identity(IdentityAttrs {
        user_id: Some("user-gates-off".into()),
        email: Some(OAUTH_EMAIL.into()),
        organization_id: None,
        team_id: None,
        deployment_id: None,
    });

    assert!(!distill_telemetry::is_enabled());
    distill_telemetry::log_event(distill_telemetry::events::SessionNew {
        session_id: "sess-int-1".into(),
        client_identifier: None,
        client_version: None,
        is_git_repo: true,
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
    });
    distill_telemetry::log_event(distill_telemetry::events::SessionHarness {
        session_id: "sess-int-1".into(),
        client_identifier: Some("grok-pager".into()),
        model_id: "grok-4".into(),
        agent_name: "distill-plan".into(),
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
        mcp_server_names: vec![CANARY_MCP.into()],
        plugin_names: vec![],
        skill_names: vec![],
        lsp_server_names: vec![],
        hook_names: vec![],
        agents_md_dir_names: vec![],
        memory_enabled: false,
        memory_retrieval_mode: distill_telemetry::events::MemoryRetrievalMode::Disabled,
        is_git_repo: true,
        auto_update: None,
    });
    distill_telemetry::log_event(distill_telemetry::events::PromptSubmitted {
        prompt_length: CANARY_PROMPT.len(),
        model_id: "grok-4".into(),
        client_identifier: None,
        screen_mode: None,
        prompt_text: Some(CANARY_PROMPT.into()),
        command_name: None,
    });
    distill_telemetry::log_event(distill_telemetry::events::ModelResponseReceived {
        model_id: CANARY_MODEL.into(),
        duration_ms: 5,
        stop_reason: Some("stop".into()),
        prompt_tokens: Some(11),
        completion_tokens: Some(7),
        reasoning_tokens: None,
        cached_prompt_tokens: None,
        cache_creation_tokens: None,
        context_tokens: None,
        cost_usd_ticks: None,
    });
    distill_telemetry::log_event(distill_telemetry::events::ToolCallCompleted {
        tool_name: "run_terminal_cmd".into(),
        outcome: distill_session_events::types::ToolOutcome::Success,
        hook_rewrote: false,
        duration_ms: 3,
        tool_result_size_bytes: None,
        model_id: "grok".into(),
        file_path: None,
        parameters: Some(serde_json::json!({ "command": CANARY_CMD })),
        tool_use_id: Some("call-gates-off".into()),
        tool_output: None,
        error_message: None,
    });
    distill_telemetry::log_event(deny_decision(CANARY_CMD, "call-deny-off"));
    distill_telemetry::external::emit(&distill_telemetry::events::AssistantResponse {
        response_length: CANARY_RESPONSE.len(),
        response_text: Some(CANARY_RESPONSE.into()),
    });

    tokio::task::spawn_blocking(distill_telemetry::external::flush)
        .await
        .unwrap();
    server
        .recorder()
        .wait_for_signals(
            Duration::from_secs(10),
            &[OtelSignal::Logs, OtelSignal::Metrics],
        )
        .await
        .expect("server must receive both signals");

    let records = server.recorder().log_records();
    for record in &records {
        assert_eq!("ai.distill", record.scope);
        assert_eq!(
            Some("grok-cli"),
            record.resource.get("service.name").and_then(Value::as_str),
            "service.name=grok-cli is a wire commitment: {record:?}"
        );
    }
    let event_names: Vec<&str> = records
        .iter()
        .map(|record| record.event_name.as_str())
        .collect();
    for expected in [
        "grok_code.session_start",
        "grok_code.user_prompt",
        "grok_code.api_request",
    ] {
        assert!(
            event_names.contains(&expected),
            "missing {expected} in {event_names:?}"
        );
    }
    assert_eq!(
        1,
        event_names
            .iter()
            .filter(|name| **name == "grok_code.session_start")
            .count()
    );

    let metrics = server.recorder().metric_points();
    let mut session_count_total = 0i64;
    for point in &metrics {
        let OtelMetricData::Sum {
            temporality, value, ..
        } = point.data
        else {
            continue;
        };
        assert_eq!(
            OtelTemporality::Delta,
            temporality,
            "default temporality must be Delta (CC parity)"
        );
        if point.name == "grok_code.session.count"
            && let Some(OtelNumber::Int(count)) = value
        {
            session_count_total += count;
        }
    }
    let metric_names: Vec<&str> = metrics.iter().map(|point| point.name.as_str()).collect();
    assert!(
        metric_names.contains(&"grok_code.session.count"),
        "missing session.count in {metric_names:?}"
    );
    assert!(metric_names.contains(&"grok_code.token.usage"));
    assert_eq!(
        1, session_count_total,
        "session.count must increment exactly once per SessionNew"
    );

    let wire = server.recorder().body_text().unwrap();
    assert!(
        !wire.contains("CANARY"),
        "canary reached the wire: gates are off / scrub failed"
    );
    assert!(
        !wire.contains(CANARY_MCP),
        "MCP server name reached the wire"
    );
    let prompt = server
        .recorder()
        .log_record("grok_code.user_prompt")
        .expect("grok_code.user_prompt record");
    assert_eq!(
        Some(OAUTH_EMAIL),
        prompt.attributes.get("user.email").and_then(Value::as_str)
    );
    assert!(!prompt.attributes.contains_key("prompt"));
    let assistant = server
        .recorder()
        .log_record("grok_code.assistant_response")
        .expect("grok_code.assistant_response record");
    assert!(
        assistant.attributes.contains_key("response_length"),
        "response_length is always-on"
    );
    assert!(
        !assistant.attributes.contains_key("response"),
        "gated response must be absent with gates off"
    );
    assert_eq!(None, assistant.body, "no record may carry a body");
    let decision = server
        .recorder()
        .log_record("grok_code.tool_decision")
        .expect("grok_code.tool_decision record");
    assert!(!decision.attributes.contains_key("tool_parameters"));
    assert!(!decision.attributes.contains_key("full_command"));
    let tool = server
        .recorder()
        .log_record("grok_code.tool_result")
        .expect("grok_code.tool_result record");
    assert!(!tool.attributes.contains_key("full_command"));
    assert!(!tool.attributes.contains_key("tool_parameters"));
    assert!(!tool.attributes.contains_key("tool_input"));
    assert!(!tool.attributes.contains_key("tool_output"));
    assert!(
        metrics.iter().any(|point| {
            point.name == "grok_code.session.count"
                && point.attributes.get("user.email").and_then(Value::as_str) == Some(OAUTH_EMAIL)
        }),
        "user.email must ride metrics when OAuth identity is set"
    );

    let start = std::time::Instant::now();
    tokio::task::spawn_blocking(distill_telemetry::external::shutdown)
        .await
        .unwrap();
    assert!(
        start.elapsed() <= Duration::from_millis(2500),
        "shutdown watchdog must bound exit at ~2s (took {:?})",
        start.elapsed()
    );
    assert!(!distill_telemetry::external::is_active());

    let (silence, ()) = tokio::join!(
        server
            .recorder()
            .wait_for_silence(Duration::from_millis(400), |events| !events.is_empty()),
        async {
            distill_telemetry::log_event(distill_telemetry::events::PromptSubmitted {
                prompt_length: 1,
                model_id: "grok-4".into(),
                client_identifier: None,
                screen_mode: None,
                prompt_text: None,
                command_name: None,
            });
        },
    );
    silence.expect("no exports after shutdown");

    tokio::task::spawn_blocking(distill_telemetry::external::shutdown)
        .await
        .unwrap();
}
