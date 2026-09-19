// Modified for Distill by Samuel Fajreldines, 2026.
//! HTTPS (TLS) gRPC transport coverage for the external OTEL stream.
//! Regression test: `https://` collector endpoints were once rejected at exporter build time and the stream silently disabled itself.
//!
//! The collector presents a certificate signed by a freshly generated CA.
//! The client trusts it via the standard `OTEL_EXPORTER_OTLP_CERTIFICATE` variable, so the full TLS handshake and OTLP export path is exercised.
//! It lives in its own integration-test binary because the external telemetry registry is a process-global `OnceLock`.

mod otlp_collector;

use std::time::Duration;

use otlp_collector as col;
use distill_test_support::{OtelRecorder, OtelSignal};

#[test]
fn external_stream_grpc_over_tls_end_to_end() {
    let tls = col::generate_tls_material();
    let ca_file = tempfile::NamedTempFile::new().expect("CA temp file");
    std::fs::write(ca_file.path(), &tls.ca_cert_pem).expect("write CA pem");
    let ca_path = ca_file.path().to_str().expect("utf-8 CA path").to_string();

    let recorder = OtelRecorder::new();
    let endpoint = col::start_grpc_tls_collector(
        recorder.clone(),
        tls.server_cert_pem.clone(),
        tls.server_key_pem.clone(),
    );
    assert!(endpoint.starts_with("https://"), "{endpoint}");

    let mut cfg = distill_telemetry::external::ExternalOtelConfig::resolve_with(
        |name| match name {
            "GROK_EXTERNAL_OTEL" => Some("1".into()),
            "OTEL_LOGS_EXPORTER" | "OTEL_METRICS_EXPORTER" => Some("otlp".into()),
            "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(endpoint.clone()),
            "OTEL_EXPORTER_OTLP_PROTOCOL" => Some("grpc".into()),
            "OTEL_EXPORTER_OTLP_CERTIFICATE" => Some(ca_path.clone()),
            "OTEL_METRIC_EXPORT_INTERVAL" => Some("200".into()),
            "OTEL_BLRP_SCHEDULE_DELAY" => Some("100".into()),
            _ => None,
        },
        None,
    )
    .expect("double opt-in must resolve");
    assert_eq!(cfg.logs_ca_certificate.as_deref(), Some(ca_path.as_str()));
    cfg.client = distill_telemetry::external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };

    distill_telemetry::external::init(Some(cfg));
    assert!(
        distill_telemetry::external::is_active(),
        "https gRPC exporters must build and activate the stream (GB-4580)"
    );

    // `SessionNew` maps to the `session.count` metric; `SessionHarness` maps to the `session_start` log record
    // Emitting both exercises each signal's TLS export path
    distill_telemetry::log_event(distill_telemetry::events::SessionNew {
        session_id: "sess-grpc-tls-1".into(),
        client_identifier: None,
        client_version: None,
        is_git_repo: true,
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
    });
    distill_telemetry::log_event(distill_telemetry::events::SessionHarness {
        session_id: "sess-grpc-tls-1".into(),
        client_identifier: Some("grok-pager".into()),
        model_id: "grok-4".into(),
        agent_name: "distill-plan".into(),
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
        mcp_server_names: vec![],
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

    distill_telemetry::external::flush();
    col::block_on(recorder.wait_for_signals(Duration::from_secs(10), &[OtelSignal::Logs]))
        .expect("log records must arrive over TLS");
    let names = recorder.event_names();
    assert!(
        names.iter().any(|n| n == "grok_code.session_start"),
        "expected grok_code.session_start in {names:?}"
    );

    // Metrics ride the same TLS channel config; make sure at least one periodic export lands too
    col::block_on(recorder.wait_for_signals(Duration::from_secs(10), &[OtelSignal::Metrics]))
        .expect("metric exports must arrive over TLS");

    distill_telemetry::external::shutdown();
}
