// Modified for Distill by Samuel Fajreldines, 2026.
//! Wire test for ambient-context injection.
//! Events emitted inside a `with_session_ctx` scope must carry `session.id`, `turn_number`, `prompt.id`, and a monotonic `event.sequence`.
//! `prompt.id` must appear on events ONLY, never on metrics (unbounded cardinality).
//! This complements the other wire tests, which emit outside any ctx.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use distill_telemetry::external;
use distill_test_support::{MockOtelServer, OtelSignal};

#[tokio::test]
async fn ambient_ctx_injects_session_turn_and_prompt_id() {
    let server = MockOtelServer::start().await.unwrap();

    let env = server.exporter_env();
    let mut cfg = external::ExternalOtelConfig::resolve_with(|name| env.get(name).cloned(), None)
        .expect("double opt-in must resolve");
    cfg.client = external::config::ExternalClientInfo {
        service_version: "0.0.0-test".into(),
        client_version: "0.0.0-test".into(),
        app_entrypoint: "cli".into(),
    };
    external::init(Some(cfg));
    assert!(external::is_active());

    let ctx = distill_telemetry::TelemetryCtx::new(
        "sess-ctx".to_owned(),
        Arc::new(tokio::sync::Mutex::new(3usize)),
    );
    distill_telemetry::with_session_ctx(ctx, async {
        distill_telemetry::session_ctx::begin_prompt_id();
        distill_telemetry::log_event(distill_telemetry::events::PromptSubmitted {
            prompt_length: 42,
            model_id: "grok-4".into(),
            client_identifier: None,
            screen_mode: None,
            prompt_text: None,
            command_name: None,
        });
        distill_telemetry::log_event(distill_telemetry::events::ModelResponseReceived {
            model_id: "grok-4".into(),
            duration_ms: 5,
            stop_reason: Some("stop".into()),
            prompt_tokens: Some(11),
            completion_tokens: None,
            reasoning_tokens: None,
            cached_prompt_tokens: None,
            cache_creation_tokens: None,
            context_tokens: None,
            cost_usd_ticks: None,
        });
    })
    .await;

    tokio::task::spawn_blocking(external::flush).await.unwrap();
    server
        .recorder()
        .wait_for_signals(
            Duration::from_secs(10),
            &[OtelSignal::Logs, OtelSignal::Metrics],
        )
        .await
        .expect("server must receive both signals");

    let prompt = server
        .recorder()
        .log_record("grok_code.user_prompt")
        .expect("user_prompt present");
    assert_eq!(
        prompt.attributes.get("session.id").and_then(Value::as_str),
        Some("sess-ctx"),
        "ambient session.id injected onto events"
    );
    assert_eq!(
        prompt.attributes.get("turn_number").and_then(Value::as_i64),
        Some(3),
        "ambient turn_number injected onto events"
    );
    let prompt_id = prompt
        .attributes
        .get("prompt.id")
        .and_then(Value::as_str)
        .expect("prompt.id injected onto events");
    assert!(!prompt_id.is_empty(), "prompt.id must be a real uuid");
    assert!(
        prompt.attributes.contains_key("event.sequence"),
        "event.sequence injected onto every event"
    );

    let tokens: Vec<_> = server
        .recorder()
        .metric_points_named("grok_code.token.usage");
    assert!(!tokens.is_empty(), "token.usage must export");
    for p in &tokens {
        assert!(
            !p.attributes.contains_key("prompt.id"),
            "prompt.id must never reach metrics"
        );
        assert!(
            !p.attributes.contains_key("turn_number"),
            "turn_number must never reach metrics"
        );
        assert_eq!(
            p.attributes.get("session.id").and_then(Value::as_str),
            Some("sess-ctx")
        );
    }

    tokio::task::spawn_blocking(external::shutdown)
        .await
        .unwrap();
}
