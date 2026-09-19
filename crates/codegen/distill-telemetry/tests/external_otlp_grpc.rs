// Modified for Distill by Samuel Fajreldines, 2026.
//! gRPC transport coverage for the external OTEL stream, mirroring the primary HTTP/protobuf wire test in `external_otlp.rs`.
//! It lives in its own integration-test binary because the external telemetry registry is a process-global `OnceLock`.

mod otlp_collector;

use std::time::Duration;

use otlp_collector as col;
use distill_test_support::{OtelMetricData, OtelRecorder, OtelSignal, OtelTemporality};

const CANARY_MODEL: &str = "sk-CANARYgrpcabcdefghij1234567890";
const CANARY_PROMPT: &str = "CANARY_GRPC_PROMPT_TEXT do not export";
const CANARY_MCP: &str = "canary-grpc-internal-mcp-server";

#[test]
fn external_stream_grpc_end_to_end() {
    let recorder = OtelRecorder::new();
    let endpoint = col::start_grpc_collector(recorder.clone());

    let mut cfg = distill_telemetry::external::ExternalOtelConfig::resolve_with(
        |name| match name {
            "GROK_EXTERNAL_OTEL" => Some("1".into()),
            "OTEL_LOGS_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".into()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(endpoint.clone()),
            "OTEL_EXPORTER_OTLP_PROTOCOL" => Some("grpc".into()),
            "OTEL_METRIC_EXPORT_INTERVAL" => Some("200".into()),
            "OTEL_BLRP_SCHEDULE_DELAY" => Some("100".into()),
            _ => None,
        },
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

    distill_telemetry::log_event(distill_telemetry::events::SessionNew {
        session_id: "sess-grpc-1".into(),
        client_identifier: None,
        client_version: None,
        is_git_repo: true,
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
    });
    distill_telemetry::log_event(distill_telemetry::events::SessionHarness {
        session_id: "sess-grpc-1".into(),
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

    distill_telemetry::external::flush();
    col::block_on(recorder.wait_for_signals(
        Duration::from_secs(10),
        &[OtelSignal::Logs, OtelSignal::Metrics],
    ))
    .expect("gRPC collector must receive both signals");

    let event_names = recorder.event_names();
    for expected in [
        "grok_code.session_start",
        "grok_code.user_prompt",
        "grok_code.api_request",
    ] {
        assert!(
            event_names.iter().any(|n| n == expected),
            "missing {expected} in {event_names:?}"
        );
    }

    let metrics = recorder.metric_points();
    assert!(
        metrics.iter().any(|p| p.name == "grok_code.session.count"),
        "missing session.count in {metrics:?}"
    );
    assert!(
        metrics.iter().any(|p| p.name == "grok_code.token.usage"),
        "missing token.usage in {metrics:?}"
    );
    for point in metrics {
        let OtelMetricData::Sum { temporality, .. } = point.data else {
            continue;
        };
        assert_eq!(
            OtelTemporality::Delta,
            temporality,
            "default temporality must be Delta over gRPC"
        );
    }

    let wire = recorder.body_text().unwrap();
    assert!(!wire.contains("CANARY"), "canary reached the gRPC wire");
    assert!(
        !wire.contains(CANARY_MCP),
        "MCP server name reached the gRPC wire"
    );

    distill_telemetry::external::shutdown();
}
