// Modified for Distill by Samuel Fajreldines, 2026.
//! Dedicated binary: the external-stream `OnceLock` is process-global, so this
//! construction canary cannot live in the lib test suite.

use distill_telemetry::external::{self, ExternalOtelConfig};

#[test]
fn prompt_submitted_prompt_text_is_some_when_stream_active() {
    let mut cfg = ExternalOtelConfig::resolve_with(
        |name| match name {
            "GROK_EXTERNAL_OTEL" => Some("1".into()),
            "OTEL_LOGS_EXPORTER" => Some("console".into()),
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
    external::init(Some(cfg));
    assert!(
        external::is_active(),
        "console exporter must activate the stream"
    );

    let user_message = "PARITY_PROMPT live construction";
    let ev = distill_telemetry::events::PromptSubmitted {
        prompt_length: user_message.len(),
        model_id: "grok-4".into(),
        client_identifier: None,
        screen_mode: None,
        prompt_text: Some(user_message.to_owned()),
        command_name: None,
    };
    assert_eq!(ev.prompt_text.as_deref(), Some(user_message));
    external::shutdown();
}
