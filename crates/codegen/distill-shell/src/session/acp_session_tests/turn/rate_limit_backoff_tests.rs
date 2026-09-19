// Modified for Distill by Samuel Fajreldines, 2026.
//! Coverage of 429 handling in the turn loop against a mock server, plus the harness that bursts more subagent turns than the concurrency cap.

use super::support::*;
use super::*;
use distill_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy)]
pub(super) enum SessionKind {
    Main,
    Subagent,
}

fn rate_limited_reply(retry_after_secs: u64) -> ScriptedResponse {
    let mut reply = ScriptedResponse::text(429, "concurrent sampling cap exceeded");
    reply
        .headers
        .push(("retry-after".to_string(), retry_after_secs.to_string()));
    reply
}

pub(super) type CapturedRetries =
    Arc<std::sync::Mutex<Vec<crate::extensions::notification::RetryState>>>;

pub(super) fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> CapturedRetries {
    use crate::extensions::notification::{SessionNotification, SessionUpdate};
    let captured: CapturedRetries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                distill_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.method.as_ref() == "x.ai/session_notification" =>
                {
                    if let Ok(SessionNotification {
                        update: SessionUpdate::RetryState(rs),
                        ..
                    }) = serde_json::from_str::<SessionNotification>(args.request.params.get())
                    {
                        sink.lock().unwrap().push(rs);
                    }
                }
                _ => {}
            }
        }
    });
    captured
}

pub(super) fn drain_persistence(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
}

pub(super) fn sampler_surfaces_429() -> distill_sampler::RetryPolicy {
    distill_sampler::RetryPolicy {
        max_retries: 5,
        rate_limit_retry_threshold: distill_sampler::RATE_LIMIT_RETRY_DISABLED,
        ..Default::default()
    }
}

fn sampler_retries_429() -> distill_sampler::RetryPolicy {
    distill_sampler::RetryPolicy {
        max_retries: 5,
        rate_limit_retry_threshold: distill_sampler::RATE_LIMIT_RETRY_THRESHOLD,
        ..Default::default()
    }
}

pub(super) async fn actor_under_test(
    server: &MockInferenceServer,
    session: SessionKind,
    retry_policy: distill_sampler::RetryPolicy,
    transient_retry_enabled: bool,
) -> (Arc<SessionActor>, CapturedRetries) {
    let sampler_max_retries = retry_policy.max_retries;
    let sampling_cfg = distill_sampler::SamplerConfig {
        base_url: server.url(),
        model: "test".to_string(),
        api_backend: distill_sampler::ApiBackend::Responses,
        context_window: 256_000,
        max_retries: Some(sampler_max_retries),
        idle_timeout_secs: Some(30),
        ..Default::default()
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_sampler::SamplingEvent>();
    let sampler_handle =
        distill_sampler::SamplerActor::spawn(sampling_cfg, retry_policy, sampler_event_tx);

    let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
    let captured_retries = drain_gateway(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
    drain_persistence(persistence_rx);

    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    actor.startup_hints.is_subagent = matches!(session, SessionKind::Subagent);
    actor.transient_retry_enabled = transient_retry_enabled;
    // The per-turn config push carries the shell's max_retries; mirror the policy.
    actor.max_retries = sampler_max_retries;

    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = distill_sampling_types::ApiBackend::Responses;
    cfg.model = "test".to_string();
    actor.chat_state_handle.update_sampling_config(cfg);

    let actor = Arc::new(actor);
    {
        // Sampler-event drainer, matching the production run loop.
        let drainer = actor.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
    }
    (actor, captured_retries)
}

pub(super) async fn conversation_request(actor: &Arc<SessionActor>) -> ConversationRequest {
    actor
        .chat_state_handle
        .build_request(
            Vec::new(),
            None,
            false,
            None,
            actor.session_id_string(),
            "req-rate-limit-test".to_string(),
        )
        .await
        .expect("chat state actor should be alive")
}

pub(super) async fn pump_local_tasks() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn explicit_sampler_threshold_disables_subagent_wait_budget() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true)
                    .await;
            let mut config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            config.max_retries = Some(3);
            config.rate_limit_retry_threshold = Some(4);
            actor.chat_state_handle.update_sampling_config(config);

            let reconstructed = actor.reconstruct_full_config().await;
            assert_eq!(
                reconstructed.max_retries,
                Some(3),
                "session request reconstruction must preserve the model retry budget"
            );
            assert_eq!(
                reconstructed.rate_limit_retry_threshold,
                Some(4),
                "session request reconstruction must preserve the model threshold"
            );
            let config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            let budget = actor.rate_limit_wait_budget(config.rate_limit_retry_threshold);

            assert!(
                !budget.can_wait(),
                "an explicit sampler threshold must disable the separate subagent 429 wait loop"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn subagent_429_wait_is_owned_and_capped_by_the_pacer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response("/v1/responses", rate_limited_reply(90));

            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true)
                    .await;
            let request = conversation_request(&actor).await;
            let requests_before = server.request_count();
            let mut budget = actor.rate_limit_wait_budget(None);

            let started = tokio::time::Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(300),
                actor.run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                ),
            )
            .await
            .expect("turn must finish within timeout");
            let waited = started.elapsed();

            match outcome {
                Ok(SamplerTurnOutcome::Response(..)) => {}
                Ok(_) => panic!("expected a Response outcome after the second submission"),
                Err(err) => panic!("subagent turn must survive the 429: {err:?}"),
            }
            assert_eq!(
                server.request_count(),
                requests_before + 2,
                "the surfaced 429 plus the pacer's one resubmit"
            );
            assert_eq!(
                budget.attempts_used(),
                1,
                "the pacer must see and pace the 429 itself; the sampler did not absorb it"
            );
            assert!(
                waited >= Duration::from_secs(20) && waited <= Duration::from_secs(40),
                "one pacer wait capped near 30s, not the raw 90s hint or a stacked wait: {waited:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn paced_wait_notifies_the_client_with_a_retrying_state() {
    use crate::extensions::notification::RetryState;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response("/v1/responses", rate_limited_reply(1));

            let (actor, retries) =
                actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true)
                    .await;
            let request = conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);

            let outcome = tokio::time::timeout(
                Duration::from_secs(30),
                actor.run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                ),
            )
            .await
            .expect("turn must finish within timeout");
            assert!(matches!(outcome, Ok(SamplerTurnOutcome::Response(..))));

            pump_local_tasks().await;

            let retrying: Vec<_> = retries
                .lock()
                .unwrap()
                .iter()
                .filter_map(|rs| match rs {
                    RetryState::Retrying {
                        attempt,
                        max_retries,
                        reason,
                        ..
                    } => Some((*attempt, *max_retries, reason.clone())),
                    _ => None,
                })
                .collect();

            assert_eq!(retrying.len(), 1, "one paced wait must notify exactly once");
            let Some((attempt, max_retries, reason)) = retrying.first() else {
                panic!("expected one retrying notification: {retrying:?}");
            };
            assert_eq!(*attempt, 1);
            assert_eq!(*max_retries, 8, "default subagent attempt budget");
            assert!(
                reason.contains("waiting"),
                "reason should be legible: {reason}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn exhausted_subagent_budget_notifies_exhausted_with_the_attempts_taken() {
    use crate::extensions::notification::RetryState;
    use crate::session::acp_session::RateLimitWaitConfig;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            for _ in 0..=RateLimitWaitConfig::DEFAULT_MAX_ATTEMPTS {
                server.enqueue_response("/v1/responses", rate_limited_reply(1));
            }

            let (actor, retries) =
                actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true)
                    .await;
            let request = conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);

            let outcome = tokio::time::timeout(
                Duration::from_secs(60),
                actor.run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                ),
            )
            .await
            .expect("turn must finish within timeout");
            match outcome {
                Err(err) => assert_eq!(
                    i32::from(err.code),
                    crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                    "an exhausted budget must surface the rate-limited terminal: {err:?}"
                ),
                Ok(_) => panic!("a budget spent on 429s must fail the turn"),
            }

            pump_local_tasks().await;

            let exhausted: Vec<_> = retries
                .lock()
                .unwrap()
                .iter()
                .filter_map(|rs| match rs {
                    RetryState::Exhausted {
                        attempts,
                        is_rate_limited,
                        ..
                    } => Some((*attempts, *is_rate_limited)),
                    _ => None,
                })
                .collect();

            assert_eq!(exhausted.len(), 1, "one terminal exhaustion notification");
            let Some(&(attempts, is_rate_limited)) = exhausted.first() else {
                panic!("expected one exhausted notification: {exhausted:?}");
            };
            assert_eq!(
                attempts,
                RateLimitWaitConfig::DEFAULT_MAX_ATTEMPTS,
                "the client must see the paced attempts, not a first-try zero"
            );
            assert!(is_rate_limited, "the terminal must be flagged rate-limited");
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn main_session_429_is_owned_by_the_sampler_never_the_pacer() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for (enqueued, expect_ok) in [(1usize, true), (2usize, false)] {
                let server =
                    MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                        .await
                        .expect("mock inference server");
                for _ in 0..enqueued {
                    server.enqueue_response("/v1/responses", rate_limited_reply(1));
                }

                let (actor, _retries) =
                    actor_under_test(&server, SessionKind::Main, sampler_retries_429(), true).await;
                let request = conversation_request(&actor).await;
                let requests_before = server.request_count();
                let mut budget = actor.rate_limit_wait_budget(None);

                let outcome = tokio::time::timeout(
                    Duration::from_secs(30),
                    actor.run_turn_via_sampler(
                        request,
                        &mut budget,
                        transient_state(0, true),
                        false,
                        TurnParkState::Fresh,
                    ),
                )
                .await
                .expect("turn must finish within timeout");

                if expect_ok {
                    match outcome {
                        Ok(SamplerTurnOutcome::Response(..)) => {}
                        Ok(_) => panic!("expected a Response after the sampler's own retry"),
                        Err(err) => panic!("the sampler's own retry must recover: {err:?}"),
                    }
                } else {
                    match outcome {
                        Err(err) => assert_eq!(
                            i32::from(err.code),
                            crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                            "terminal must carry the rate-limited code: {err:?}"
                        ),
                        Ok(_) => panic!("persistent 429 past the sampler budget must fail"),
                    }
                }
                assert_eq!(
                    server.request_count(),
                    requests_before + 2,
                    "the sampler's own attempt plus one retry (enqueued={enqueued})"
                );
                assert_eq!(
                    budget.attempts_used(),
                    0,
                    "a main session never paces the 429 (enqueued={enqueued})"
                );
            }
        })
        .await;
}

const BURST_SERVICE_TIME: Duration = Duration::from_millis(300);

struct BurstMetrics {
    completed: usize,
    failed: usize,
}

async fn run_burst(n: usize, cap: usize) -> BurstMetrics {
    let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
        .await
        .expect("mock inference server");
    server.set_inference_concurrency_cap(cap, BURST_SERVICE_TIME, 1);

    let mut turns = Vec::with_capacity(n);
    for _ in 0..n {
        let (actor, _retries) =
            actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true).await;
        let request = conversation_request(&actor).await;
        turns.push((actor, request));
    }

    let handles: Vec<_> = turns
        .into_iter()
        .map(|(actor, request)| {
            tokio::task::spawn_local(async move {
                let mut budget = actor.rate_limit_wait_budget(None);
                tokio::time::timeout(
                    Duration::from_secs(60),
                    actor.run_turn_via_sampler(
                        request,
                        &mut budget,
                        transient_state(0, true),
                        false,
                        TurnParkState::Fresh,
                    ),
                )
                .await
                .expect("burst turn must finish within timeout")
            })
        })
        .collect();

    let mut completed = 0;
    let mut failed = 0;
    for handle in handles {
        match handle.await.expect("burst turn task must not panic") {
            Ok(SamplerTurnOutcome::Response(..)) => completed += 1,
            Ok(_) => panic!("unexpected recovery outcome in burst"),
            Err(err) => {
                assert_eq!(
                    i32::from(err.code),
                    crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                    "burst failures must be rate-limited terminals: {err:?}"
                );
                failed += 1;
            }
        }
    }
    BurstMetrics { completed, failed }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn subagents_over_cap_all_complete_under_paced_time() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let metrics = run_burst(12, 4).await;
            assert_eq!(
                metrics.completed, 12,
                "every subagent turn must pace through the cap"
            );
            assert_eq!(
                metrics.failed, 0,
                "no turn may fail terminally under the cap"
            );
        })
        .await;
}
