// Modified for Distill by Samuel Fajreldines, 2026.
use super::rate_limit_backoff_tests::{SessionKind, actor_under_test};
use super::transient_retry_loop_tests::{on_session_stack, run_paused, sampler_surfaces_5xx};
use super::*;
use distill_test_support::{MockInferenceServer, MockModelEntry};
use std::time::Duration;
use tracing::Instrument;

fn trace_id(traceparent: &str) -> &str {
    traceparent
        .split('-')
        .nth(1)
        .expect("traceparent has a trace id")
}

#[test]
fn sampling_request_header_carries_the_turn_trace_id() {
    on_session_stack(|| {
        run_paused(|| async {
            // Thread-local, so it must be installed on the session-stack thread the turn runs on.
            let _trace = distill_otel::set_local_trace_subscriber();
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_5xx(), false).await;

            let turn_root = tracing::info_span!("test.turn_root");
            let root_traceparent =
                distill_otel::span_traceparent(&turn_root).expect("root span has a trace id");
            let outcome = tokio::time::timeout(
                Duration::from_secs(300),
                actor
                    .process_conversation_turn_with_recovery(
                        "req-sampling-trace-test",
                        None,
                        None,
                        None,
                        &mut length_salvage::LengthSalvage::new(None),
                        &mut Default::default(),
                    )
                    .instrument(turn_root),
            )
            .await
            .expect("turn must finish within timeout");
            assert!(
                outcome.is_ok(),
                "the turn must complete: {:?}",
                outcome.as_ref().map(|_| "TurnOutcome").err()
            );

            let requests = server.requests();
            let sampling = requests
                .iter()
                .find(|request| request.path == "/v1/responses")
                .expect("one sampling request reached the mock server");
            let header = sampling
                .header("traceparent")
                .expect("the sampler injects traceparent");
            assert_eq!(
                trace_id(&root_traceparent),
                trace_id(header),
                "the sampling request must share the turn's trace id"
            );
        })
    })
}
