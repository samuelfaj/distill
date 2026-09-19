// Modified for Distill by Samuel Fajreldines, 2026.
//! Wire test for the external OTEL stream with **both content gates ON**, the higher-risk privacy path.
//! Here prompt text and tool parameters actually leave the process.
//! Asserts against `distill_test_support::MockOtelServer` that:
//!
//! - gated content (`prompt`, `tool_parameters`, `file_path`, verbatim `tool_name`/`mcp_server.name`) IS present when the gate is on,
//! - planted secret shapes are STILL scrubbed inside that gated content (gates loosen *which fields* export, never the secret scrub),
//! - identity attributes ride every record and metric once set,
//! - `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE=cumulative` and `OTEL_METRICS_INCLUDE_VERSION=1` take effect on the wire,
//! - the remote fleet kill switch stops emission in-process.
//!
//! Everything runs in one sequential test: the `EXTERNAL` registry is a process-global `OnceLock`.
//! Each init-config scenario is its own test binary.

use std::time::Duration;

use serde_json::Value;
use distill_telemetry::external::{self, ExternalOtelRemotePolicy, IdentityAttrs};
use distill_test_support::{MockOtelServer, OtelMetricData, OtelSignal, OtelTemporality};

const SECRET_KEY: &str = "sk-LEAKaaaaaaaaaaaaaaaa1234567890";
const SECRET_MODEL: &str = "grok-4-sk-LEAKmodel1234567890abcd";
const PROMPT_MARK: &str = "promptbodymarker";
const PARAM_MARK: &str = "parammarker";
const LONG_CMD_MARK: &str = "longcmdmarker";
const DENY_CMD_MARK: &str = "denycmdmarker";
const RESPONSE_MARK: &str = "assistantresponsemarker";
const OAUTH_EMAIL: &str = "otel.parity.on@example.com";
const CLIENT_VERSION: &str = "9.9.9-cv";

#[tokio::test]
async fn external_stream_gates_on_end_to_end() {
    let server = MockOtelServer::start().await.unwrap();

    let mut env = server.exporter_env();
    env.extend([
        ("OTEL_LOG_USER_PROMPTS", "1".to_owned()),
        ("OTEL_LOG_TOOL_DETAILS", "1".to_owned()),
        ("OTEL_LOG_ASSISTANT_RESPONSES", "1".to_owned()),
        ("OTEL_LOG_TOOL_CONTENT", "1".to_owned()),
        (
            "OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE",
            "cumulative".to_owned(),
        ),
        ("OTEL_METRICS_INCLUDE_VERSION", "1".to_owned()),
    ]);
    let mut cfg = external::ExternalOtelConfig::resolve_with(|name| env.get(name).cloned(), None)
        .expect("double opt-in must resolve");
    assert!(cfg.gates.log_user_prompts && cfg.gates.log_tool_details);
    assert!(cfg.gates.log_tool_content, "content gate must be on");
    assert!(
        cfg.gates.log_assistant_responses,
        "assistant gate must be on in this binary"
    );
    cfg.client = external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: CLIENT_VERSION.into(),
        app_entrypoint: "cli".into(),
    };

    external::init(Some(cfg));
    assert!(external::is_active(), "gates-on config must activate");

    external::set_identity(IdentityAttrs {
        user_id: Some("user-x".into()),
        email: Some(OAUTH_EMAIL.into()),
        organization_id: Some("org-acme".into()),
        team_id: Some("team-7".into()),
        deployment_id: Some("deploy-eu".into()),
    });

    assert!(!distill_telemetry::is_enabled());

    distill_telemetry::log_event(distill_telemetry::events::SessionHarness {
        session_id: "sess-gates-on".into(),
        client_identifier: Some("grok-pager".into()),
        model_id: "grok-4".into(),
        agent_name: "distill-plan".into(),
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
        mcp_server_names: vec!["internal-mcp".into()],
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
        prompt_length: 100,
        model_id: "grok-4".into(),
        client_identifier: None,
        screen_mode: None,
        prompt_text: Some(format!("refactor {PROMPT_MARK} with key {SECRET_KEY} now")),
        command_name: Some("compact".into()),
    });
    distill_telemetry::log_event(distill_telemetry::events::ModelResponseReceived {
        model_id: SECRET_MODEL.into(),
        duration_ms: 5,
        stop_reason: Some("stop".into()),
        prompt_tokens: Some(11),
        completion_tokens: Some(7),
        reasoning_tokens: Some(3),
        cached_prompt_tokens: Some(9),
        cache_creation_tokens: None,
        context_tokens: None,
        cost_usd_ticks: None,
    });
    distill_telemetry::log_event(distill_telemetry::events::ToolCallCompleted {
        tool_name: "github__create_issue".into(),
        outcome: distill_session_events::types::ToolOutcome::Success,
        hook_rewrote: false,
        duration_ms: 12,
        tool_result_size_bytes: None,
        model_id: "grok".into(),
        file_path: Some("/tmp/projectdir/config.toml".into()),
        parameters: Some(serde_json::json!({
            "marker": PARAM_MARK,
            "token": SECRET_KEY,
            "deep": {"a": {"b": "c"}},
        })),
        tool_use_id: Some("call-github".into()),
        tool_output: Some(format!("ok {PARAM_MARK}")),
        error_message: None,
    });
    let long_command = format!("{LONG_CMD_MARK}{}", "x".repeat(600));
    distill_telemetry::log_event(distill_telemetry::events::ToolCallCompleted {
        tool_name: "run_terminal_cmd".into(),
        outcome: distill_session_events::types::ToolOutcome::Success,
        hook_rewrote: false,
        duration_ms: 8,
        tool_result_size_bytes: None,
        model_id: "grok".into(),
        file_path: None,
        parameters: Some(serde_json::json!({ "command": long_command })),
        tool_use_id: Some("call-bash-long".into()),
        tool_output: None,
        error_message: None,
    });
    distill_telemetry::log_event(distill_telemetry::events::PermissionDecisionRecord {
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
            parameters: Some(serde_json::json!({ "command": DENY_CMD_MARK })),
            tool_use_id: Some("call-deny-1".into()),
        },
    });
    distill_telemetry::external::emit(&distill_telemetry::events::AssistantResponse {
        response_length: RESPONSE_MARK.len(),
        response_text: Some(RESPONSE_MARK.into()),
    });

    tokio::task::spawn_blocking(external::flush).await.unwrap();
    server
        .recorder()
        .wait_for_signals(
            Duration::from_secs(10),
            &[OtelSignal::Logs, OtelSignal::Metrics],
        )
        .await
        .expect("server must receive both signals");

    let records = server.recorder().log_records();
    let harness = server
        .recorder()
        .log_record("grok_code.session_start")
        .expect("grok_code.session_start record");
    assert_eq!("ai.distill", harness.scope);
    assert_eq!(
        harness.resource.get("service.name").and_then(Value::as_str),
        Some("grok-cli"),
        "service.name=grok-cli is a wire commitment"
    );
    assert_eq!(
        harness
            .resource
            .get("grok_code.schema.version")
            .and_then(Value::as_str),
        Some("v1")
    );
    assert!(
        records.iter().all(|record| record.body.is_none()),
        "no record may carry a body"
    );

    assert_eq!(
        harness.attributes.get("user.id").and_then(Value::as_str),
        Some("user-x")
    );
    assert_eq!(
        harness.attributes.get("user.email").and_then(Value::as_str),
        Some(OAUTH_EMAIL)
    );
    assert_eq!(
        harness
            .attributes
            .get("organization.id")
            .and_then(Value::as_str),
        Some("org-acme")
    );
    assert_eq!(
        harness.attributes.get("team.id").and_then(Value::as_str),
        Some("team-7")
    );
    assert_eq!(
        harness
            .attributes
            .get("deployment.id")
            .and_then(Value::as_str),
        Some("deploy-eu")
    );

    let prompt = server
        .recorder()
        .log_record("grok_code.user_prompt")
        .expect("grok_code.user_prompt record");
    let prompt_text = prompt
        .attributes
        .get("prompt")
        .and_then(Value::as_str)
        .expect("prompt attr present when OTEL_LOG_USER_PROMPTS=1");
    assert!(
        prompt_text.contains(PROMPT_MARK),
        "gated prompt body must export: {prompt_text:?}"
    );
    assert!(
        !prompt_text.contains(SECRET_KEY),
        "secret survived in prompt: {prompt_text:?}"
    );
    assert_eq!(
        prompt
            .attributes
            .get("command_name")
            .and_then(Value::as_str),
        Some("compact")
    );

    let tool = server
        .recorder()
        .log_record("grok_code.tool_result")
        .expect("grok_code.tool_result record");
    assert_eq!(
        tool.attributes.get("tool_name").and_then(Value::as_str),
        Some("github__create_issue"),
        "details gate exposes the verbatim tool name"
    );
    assert_eq!(
        tool.attributes.get("mcp_tool.name").and_then(Value::as_str),
        Some("create_issue")
    );
    assert_eq!(
        tool.attributes
            .get("mcp_server.name")
            .and_then(Value::as_str),
        Some("github")
    );
    assert_eq!(
        tool.attributes
            .get("file_extension")
            .and_then(Value::as_str),
        Some("toml"),
        "file_extension always exported"
    );
    assert!(
        tool.attributes.contains_key("file_path"),
        "full path exported under details gate"
    );
    let params = tool
        .attributes
        .get("tool_parameters")
        .and_then(Value::as_str)
        .expect("tool_parameters present under details gate");
    assert!(
        params.contains(PARAM_MARK),
        "gated params must export: {params:?}"
    );
    assert!(
        !params.contains(SECRET_KEY),
        "secret survived in params: {params:?}"
    );
    let tool_input = tool
        .attributes
        .get("tool_input")
        .and_then(Value::as_str)
        .expect("tool_input present under content gate");
    assert!(
        tool_input.contains(PARAM_MARK),
        "full tool_input must export: {tool_input:?}"
    );
    let expected_output = format!("ok {PARAM_MARK}");
    assert_eq!(
        tool.attributes.get("tool_output").and_then(Value::as_str),
        Some(expected_output.as_str())
    );

    let bash = records
        .iter()
        .find(|r| {
            r.event_name == "grok_code.tool_result"
                && r.attributes.get("tool_use_id").and_then(Value::as_str) == Some("call-bash-long")
        })
        .expect("long-command tool_result");
    let full_command = bash
        .attributes
        .get("full_command")
        .and_then(Value::as_str)
        .expect("full_command under content gate");
    assert!(
        full_command.starts_with(LONG_CMD_MARK),
        "full_command must keep the marker: {full_command:?}"
    );
    assert_eq!(
        full_command.len(),
        LONG_CMD_MARK.len() + 600,
        "full_command must not 512→128 collapse"
    );
    assert!(
        !full_command.contains("…[truncated]"),
        "full_command must not collapse: {full_command:?}"
    );

    let decision = server
        .recorder()
        .log_record("grok_code.tool_decision")
        .expect("grok_code.tool_decision record");
    assert_eq!(
        decision.attributes.get("decision").and_then(Value::as_str),
        Some("deny")
    );
    assert_eq!(
        decision
            .attributes
            .get("tool_use_id")
            .and_then(Value::as_str),
        Some("call-deny-1")
    );
    let deny_params = decision
        .attributes
        .get("tool_parameters")
        .and_then(Value::as_str)
        .expect("deny tool_decision exports params under details gate");
    assert!(
        deny_params.contains(DENY_CMD_MARK),
        "deny params must export: {deny_params:?}"
    );
    assert_eq!(
        decision
            .attributes
            .get("full_command")
            .and_then(Value::as_str),
        Some(DENY_CMD_MARK)
    );

    let assistant = server
        .recorder()
        .log_record("grok_code.assistant_response")
        .expect("grok_code.assistant_response record");
    assert_eq!(None, assistant.body, "no record may carry a body");
    let response = assistant
        .attributes
        .get("response")
        .and_then(Value::as_str)
        .expect("gated response present");
    assert!(
        response.contains(RESPONSE_MARK),
        "gated response must export: {response:?}"
    );
    let response_length = assistant
        .attributes
        .get("response_length")
        .and_then(Value::as_i64)
        .or_else(|| {
            assistant
                .attributes
                .get("response_length")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        })
        .expect("response_length always-on");
    assert_eq!(response_length as usize, RESPONSE_MARK.len());

    let tokens = server
        .recorder()
        .metric_points_named("grok_code.token.usage");
    assert!(!tokens.is_empty(), "token.usage must export");
    for p in &tokens {
        let OtelMetricData::Sum { temporality, .. } = p.data else {
            panic!("token.usage is a sum, got {p:?}");
        };
        assert_eq!(
            OtelTemporality::Cumulative,
            temporality,
            "cumulative requested"
        );
        assert_eq!(
            p.attributes.get("app.version").and_then(Value::as_str),
            Some(CLIENT_VERSION),
            "OTEL_METRICS_INCLUDE_VERSION=1 attaches app.version"
        );
        assert_eq!(
            p.attributes.get("user.id").and_then(Value::as_str),
            Some("user-x")
        );
        assert_eq!(
            p.attributes.get("user.email").and_then(Value::as_str),
            Some(OAUTH_EMAIL)
        );
        let model = p
            .attributes
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !model.contains("sk-LEAKmodel"),
            "metric model must be scrubbed: {model:?}"
        );
    }

    let wire = server.recorder().body_text().unwrap();
    assert!(!wire.contains(SECRET_KEY), "secret key reached the wire");
    assert!(
        !wire.contains("sk-LEAKmodel"),
        "secret model shape reached the wire"
    );

    tokio::task::spawn_blocking(external::flush).await.unwrap();
    tokio::task::spawn_blocking(|| {
        external::apply_remote_policy(ExternalOtelRemotePolicy {
            force_disable: true,
            lock_content_gates: false,
        })
    })
    .await
    .unwrap();
    assert!(
        !external::is_active(),
        "kill switch must clear the emission gate"
    );
    let (silence, ()) = tokio::join!(
        server
            .recorder()
            .wait_for_silence(Duration::from_millis(400), |events| {
                events
                    .iter()
                    .any(|event| event.signal() == OtelSignal::Logs)
            }),
        async {
            distill_telemetry::log_event(distill_telemetry::events::PromptSubmitted {
                prompt_length: 1,
                model_id: "grok-4".into(),
                client_identifier: None,
                screen_mode: None,
                prompt_text: Some("post-kill".into()),
                command_name: None,
            });
        },
    );
    silence.expect("no log exports after the remote kill switch");

    tokio::task::spawn_blocking(external::shutdown)
        .await
        .unwrap();
}
