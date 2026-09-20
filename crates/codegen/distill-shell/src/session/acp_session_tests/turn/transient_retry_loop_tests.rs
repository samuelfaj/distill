// Modified for Distill by Samuel Fajreldines, 2026.
//! Coverage of the turn loop's transient-retry arm: resubmit until success, exhaustion, the kill switch.
//! Elapsed time on the paused clock pins the backoff.

use super::rate_limit_backoff_tests::{
    CapturedRetries, SessionKind, actor_under_test, pump_local_tasks,
};
use super::*;
use distill_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse};
use std::time::Duration;

/// The turn future needs a session-sized stack (spawn.rs: 8 MiB); default test stacks overflow.
pub(super) fn on_session_stack(test: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(test)
        .expect("spawn test thread")
        .join()
        .expect("test thread panicked");
}

pub(super) fn run_paused<F: std::future::Future>(fut: impl FnOnce() -> F) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("test runtime");
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(async move {
        fut().await;
    }));
}

/// No sampler-internal retries: request counts map 1:1 to submissions.
pub(super) fn sampler_surfaces_5xx() -> distill_sampler::RetryPolicy {
    distill_sampler::RetryPolicy {
        max_retries: 0,
        ..Default::default()
    }
}

fn overloaded_503() -> ScriptedResponse {
    ScriptedResponse::text(503, "upstream overloaded")
}

fn retrying_events(retries: &CapturedRetries) -> Vec<(u32, u32, String)> {
    use crate::extensions::notification::RetryState;
    retries
        .lock()
        .unwrap()
        .retries
        .iter()
        .filter_map(|rs| match rs {
            RetryState::Retrying {
                attempt,
                max_retries,
                reason,
                error_type: _,
            } => Some((*attempt, *max_retries, reason.clone())),
            _ => None,
        })
        .collect()
}

async fn run_turn(
    server: &MockInferenceServer,
    enabled: bool,
) -> (
    Result<TurnOutcome, agent_client_protocol::Error>,
    CapturedRetries,
    Duration,
    usize,
) {
    let (actor, retries) =
        actor_under_test(server, SessionKind::Main, sampler_surfaces_5xx(), enabled).await;
    // Drive the real turn loop; the request is built inside it.
    let requests_before = server.request_count();
    let started = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(300),
        actor.process_conversation_turn_with_recovery(
            "req-transient-loop-test",
            None,
            None,
            None,
            &mut length_salvage::LengthSalvage::new(None),
            &mut Default::default(),
        ),
    )
    .await
    .expect("turn must finish within timeout");
    pump_local_tasks().await;
    let elapsed = started.elapsed();
    let submissions = usize::try_from(server.request_count() - requests_before)
        .expect("request delta fits usize");
    (outcome, retries, elapsed, submissions)
}

#[test]
fn transient_5xx_resubmits_until_success() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response("/v1/responses", overloaded_503());
            server.enqueue_response("/v1/responses", overloaded_503());
            // Once the queue drains, the mock serves its default success response

            let (outcome, retries, elapsed, submissions) = run_turn(&server, true).await;

            assert!(
                outcome.is_ok(),
                "two 503s then success must complete the turn: {:?}",
                outcome.as_ref().map(|_| "TurnOutcome").err()
            );
            assert_eq!(submissions, 3, "original + two resubmits");
            assert!(
                // Jitter floor: (2+10)*0.8 = 9.6s.
                elapsed >= Duration::from_millis(9_600),
                "the 2s + 10s backoff must actually be slept (virtual): {elapsed:?}"
            );
            let retrying = retrying_events(&retries);
            assert_eq!(
                retrying,
                vec![
                    (1, 3, "Server error; retrying request".to_string()),
                    (2, 3, "Server error; retrying request".to_string()),
                ],
                "attempt numbering is post-increment and the cause maps 5xx -> Server error"
            );
        })
    });
}

#[test]
fn transient_5xx_exhausts_to_the_original_terminal() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            for _ in 0..4 {
                server.enqueue_response("/v1/responses", overloaded_503());
            }

            let (outcome, retries, elapsed, submissions) = run_turn(&server, true).await;

            assert!(
                outcome.is_err(),
                "a 5xx past the retry budget must surface the original terminal"
            );
            assert_eq!(submissions, 4, "original + full 3-resubmit budget");
            // Jitter floor: each rung sleeps at least 80% of its base, (2+10+30)*0.8 = 33.6s
            assert!(
                elapsed >= Duration::from_millis(33_600),
                "the whole jittered 2s/10s/30s ladder must be slept before terminal: {elapsed:?}"
            );
            assert_eq!(
                retrying_events(&retries).len(),
                3,
                "exactly one Retrying per resubmit, none for the terminal attempt"
            );
            // The terminal must be the legacy internal error, not a new error shape
            if let Err(err) = outcome {
                assert_eq!(
                    err.code,
                    agent_client_protocol::Error::internal_error().code,
                    "exhaustion surfaces the original internal-error terminal"
                );
            }
        })
    });
}

#[test]
fn kill_switch_off_fails_on_first_transient() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response("/v1/responses", overloaded_503());

            let (outcome, retries, elapsed, submissions) = run_turn(&server, false).await;

            assert!(outcome.is_err(), "switch off: first 503 is terminal");
            assert_eq!(submissions, 1, "no resubmits");
            assert!(
                retrying_events(&retries).is_empty(),
                "no Retrying notifications from the disabled arm"
            );
            // No elapsed upper bound: auto-advance jumps virtual time on any unrelated timer, so only lower bounds are meaningful here
            let _ = elapsed;
        })
    });
}

// Not covered here: IdleTimeout through the loop
// The sampler's stall detection is I/O-time based, so it cannot fire under the paused clock
// Eligibility for the kind is pinned at the handler level instead

/// The cumulative budget is prompt-scoped: turn-loop re-entries must share one 10-resubmit budget, not get a fresh 3 per entry.
/// Auto-recovery re-enters this way, calling `process_conversation_turn_with_recovery` repeatedly without a new prompt.
/// Entries submit 4+4+4+2+1 = 15 times; a counter that reset per loop entry would make it 20.
#[test]
fn prompt_budget_spans_turn_loop_reentries() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            for _ in 0..20 {
                server.enqueue_response("/v1/responses", overloaded_503());
            }

            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_5xx(), true).await;
            let requests_before = server.request_count();
            for entry in 0..5 {
                let outcome = tokio::time::timeout(
                    Duration::from_secs(600),
                    actor.process_conversation_turn_with_recovery(
                        &format!("req-reentry-{entry}"),
                        None,
                        None,
                        None,
                        &mut length_salvage::LengthSalvage::new(None),
                        &mut Default::default(),
                    ),
                )
                .await
                .expect("entry must finish within timeout");
                assert!(outcome.is_err(), "every 503-fed entry must stay terminal");
            }
            pump_local_tasks().await;

            let submissions = server.request_count() - requests_before;
            assert_eq!(
                submissions, 15,
                "5 re-entries share one 10-resubmit prompt budget \
                 (4+4+4+2+1); 20 means the counter regressed to a loop-local"
            );
        })
    });
}

#[test]
fn turn_phase_prompt_latency_invariants() {
    on_session_stack(|| {
        run_paused(|| async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response("/v1/responses", overloaded_503());
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(distill_test_support::sse::responses_api_script_exact(
                    "final answer",
                    "test",
                )),
            );

            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_5xx(), true).await;

            let outcome = tokio::time::timeout(
                Duration::from_secs(300),
                actor.process_conversation_turn_with_recovery(
                    "req-turn-phase-invariant",
                    None,
                    None,
                    None,
                    &mut length_salvage::LengthSalvage::new(None),
                    &mut Default::default(),
                ),
            )
            .await
            .expect("turn must finish within timeout");
            assert!(
                outcome.is_ok(),
                "one 503 then a streamed answer completes the turn"
            );
            pump_local_tasks().await;

            let phases = actor.turn_phases.complete();

            assert_eq!(phases.sampling_request_count, 2);
            assert_eq!(phases.sampling_retry_count, 1);
            assert_eq!(phases.tool_blocking_ms, 0);
            assert_eq!(phases.compaction_ms, 0);

            let accounted_ms = phases.before_first_model_ms
                + phases.sampling_ms
                + phases.tool_blocking_ms
                + phases.compaction_ms
                + phases.between_sampling_overhead_ms
                + phases.after_last_sampling_ms;
            let turn_total_ms = phases.turn_total_ms;
            assert_eq!(
                accounted_ms, turn_total_ms,
                "six phase buckets must partition turn_total_ms"
            );

            let ttfm_ms = phases.ttfm_ms.expect("a streamed answer stamps ttfm");
            assert!(
                ttfm_ms <= turn_total_ms,
                "ttfm {ttfm_ms}ms exceeds turn_total_ms {turn_total_ms}ms"
            );
        })
    });
}
