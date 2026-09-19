// Modified for Distill by Samuel Fajreldines, 2026.
//! Wire test for the **no-double-send invariant** (the credential-leak guard).
//!
//! The internal trace firehose can resolve its endpoint and headers from the deprecated `OTEL_EXPORTER_OTLP_*` fallback.
//! When it does, the shell sets `internal_pipeline_consumed_otel_vars = true` and `external::init` MUST refuse to activate.
//! Otherwise the standard vars could point both the internally-authed firehose and the customer server at one endpoint, leaking Grok credentials.
//! Here we prove the refusal end-to-end: even with a fully valid double opt-in pointed at a live server, nothing is exported.

use std::time::Duration;

use distill_telemetry::external;
use distill_test_support::{MockOtelServer, OtelExport};

#[tokio::test]
async fn refuses_to_activate_when_internal_consumed_standard_vars() {
    let server = MockOtelServer::start().await.unwrap();

    let env = server.exporter_env();
    let mut cfg = external::ExternalOtelConfig::resolve_with(|name| env.get(name).cloned(), None)
        .expect("config resolves (the refusal happens at init, not resolution)");
    cfg.client = external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };
    cfg.internal_pipeline_consumed_otel_vars = true;

    external::init(Some(cfg));
    assert!(
        !external::is_active(),
        "external stream MUST refuse to activate to prevent credential leakage"
    );

    distill_telemetry::log_event(distill_telemetry::events::SessionNew {
        session_id: "sess-guard".into(),
        client_identifier: None,
        client_version: None,
        is_git_repo: true,
        permission_mode: distill_telemetry::enums::PermissionMode::Ask,
    });
    tokio::task::spawn_blocking(external::flush).await.unwrap();

    server
        .recorder()
        .wait_for_silence(Duration::from_millis(600), |events| !events.is_empty())
        .await
        .expect("nothing may be exported when refused");
    assert_eq!(Vec::<OtelExport>::new(), server.recorder().exports());

    tokio::task::spawn_blocking(external::shutdown)
        .await
        .unwrap();
}
