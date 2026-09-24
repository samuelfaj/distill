// Modified for Distill by Samuel Fajreldines, 2026.
//! Coverage of 429 handling in the turn loop against a mock server, plus the harness that bursts more subagent turns than the concurrency cap.

use super::support::*;
use super::*;
use distill_test_support::sse::responses_api_script_exact;
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

pub(super) struct CapturedGateway {
    pub retries: Vec<crate::extensions::notification::RetryState>,
    pub usage: Vec<(u64, u64)>,
}
pub(super) type CapturedRetries = Arc<std::sync::Mutex<CapturedGateway>>;

pub(super) fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> CapturedRetries {
    use crate::extensions::notification::{SessionNotification, SessionUpdate};
    let captured: CapturedRetries = Arc::new(std::sync::Mutex::new(CapturedGateway {
        retries: Vec::new(),
        usage: Vec::new(),
    }));
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    if let acp::SessionUpdate::UsageUpdate(usage) = &args.request.update {
                        sink.lock().unwrap().usage.push((usage.used, usage.size));
                    }
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
                        sink.lock().unwrap().retries.push(rs);
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
    actor_under_test_with_startup_policy(
        server,
        session,
        retry_policy,
        transient_retry_enabled,
        false,
    )
    .await
}

async fn actor_under_test_with_startup_policy(
    server: &MockInferenceServer,
    session: SessionKind,
    retry_policy: distill_sampler::RetryPolicy,
    transient_retry_enabled: bool,
    explicit_model_override: bool,
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
    actor.startup_hints.explicit_model_override = explicit_model_override;
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

fn controlled_effort_answer(effort: &str) -> distill_workspace::jev::JevAnswerSet {
    let mut answers = std::collections::BTreeMap::new();
    answers.insert(
        distill_workspace::jev::catalog::routing::MICRO_EFFORT_QUESTION.to_owned(),
        distill_workspace::jev::Answer::Choice {
            choice: effort.to_owned(),
            probabilities: std::collections::BTreeMap::new(),
            confidence: Some(1.0),
        },
    );
    distill_workspace::jev::JevAnswerSet {
        model: "test-decision-model".to_owned(),
        answers,
        usage: Default::default(),
        request_id: Some("test-decision".to_owned()),
        latency_ms: 0,
    }
}

fn controlled_local_capable_answer() -> distill_workspace::jev::JevAnswerSet {
    let answers = [
        (
            distill_workspace::jev::catalog::routing::LOCAL_CAPABLE_QUESTION,
            0.99,
        ),
        (
            distill_workspace::jev::catalog::routing::LOCAL_CONTEXT_QUESTION,
            0.01,
        ),
        (
            distill_workspace::jev::catalog::routing::LOCAL_FRONTIER_QUESTION,
            0.01,
        ),
    ]
    .into_iter()
    .map(|(question, noul)| {
        (
            question.to_owned(),
            distill_workspace::jev::Answer::Noul { noul },
        )
    })
    .collect();
    distill_workspace::jev::JevAnswerSet {
        model: "test-decision-model".to_owned(),
        answers,
        usage: Default::default(),
        request_id: Some("test-local-decision".to_owned()),
        latency_ms: 0,
    }
}

fn routing_entry(
    model: &str,
    base_url: &str,
    efforts: Vec<distill_sampling_types::ReasoningEffortOption>,
) -> crate::agent::config::ModelEntry {
    routing_entry_with_window(model, base_url, efforts, 128_000)
}

fn routing_entry_with_window(
    model: &str,
    base_url: &str,
    efforts: Vec<distill_sampling_types::ReasoningEffortOption>,
    context_window: u64,
) -> crate::agent::config::ModelEntry {
    let mut entry = crate::agent::config::ModelEntry::fallback(
        model,
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.base_url = base_url.to_owned();
    entry.info.context_window = std::num::NonZeroU64::new(context_window)
        .expect("routing test context window must be non-zero");
    entry.info.api_backend = distill_sampling_types::ApiBackend::Responses;
    entry.info.reasoning_effort = efforts
        .iter()
        .find(|effort| effort.default)
        .or_else(|| efforts.first())
        .map(|effort| effort.value);
    entry.info.supports_reasoning_effort = !efforts.is_empty();
    entry.info.reasoning_efforts = efforts;
    entry.api_key = Some("test-routing-key".to_owned());
    entry
}

fn install_wire_routing_catalog(actor: &SessionActor, base_url: &str) {
    install_wire_routing_catalog_with_window(actor, base_url, 128_000);
}

/// The main model with a two-level menu, so auto effort has a real choice.
fn install_wire_routing_catalog_with_window(
    actor: &SessionActor,
    base_url: &str,
    main_context_window: u64,
) {
    let main = routing_entry_with_window(
        "main-model",
        base_url,
        vec![
            distill_sampling_types::ReasoningEffortOption {
                id: "low".to_owned(),
                value: distill_sampling_types::ReasoningEffort::Low,
                label: "Low".to_owned(),
                description: Some("short routine call".to_owned()),
                default: false,
            },
            distill_sampling_types::ReasoningEffortOption {
                id: "high".to_owned(),
                value: distill_sampling_types::ReasoningEffort::High,
                label: "High".to_owned(),
                description: Some("deep reasoning call".to_owned()),
                default: true,
            },
        ],
        main_context_window,
    );
    actor.models_manager.insert_test_entry("main-model", main);
}

fn install_single_effort_catalog(actor: &SessionActor) {
    let mut entry = crate::agent::config::ModelEntry::fallback(
        "single-model",
        &crate::agent::config::EndpointsConfig::default(),
    );
    entry.info.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::Low);
    entry.info.supports_reasoning_effort = true;
    entry.info.reasoning_efforts = vec![distill_sampling_types::ReasoningEffortOption {
        id: "low".to_owned(),
        value: distill_sampling_types::ReasoningEffort::Low,
        label: "Low".to_owned(),
        description: Some("only available level".to_owned()),
        default: true,
    }];
    actor
        .models_manager
        .insert_test_entry("single-model", entry);
}

/// Every round runs on the main model; auto effort and a pinned effort decide
/// only its intensity. The request on the wire must carry the chooser's final
/// model and effort, not only the ledger's view of them.
#[tokio::test(flavor = "current_thread")]
async fn controlled_routes_are_captured_on_the_wire() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("main-model"),
            ])
            .await
            .expect("mock inference server");
            for _ in 0..3 {
                server.enqueue_response(
                    "/v1/responses",
                    ScriptedResponse::sse(responses_api_script_exact("done", "test")),
                );
            }

            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_reasoning_model(None);
            install_wire_routing_catalog_with_window(&actor, &server.url(), 272_000);
            let mut initial = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            initial.model = "main-model".to_owned();
            initial.context_window = std::num::NonZeroU64::new(272_000).unwrap();
            initial.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            actor.chat_state_handle.update_sampling_config(initial);
            crate::jev::set_test_decision_answers([
                Some(controlled_effort_answer("high")),
                Some(controlled_effort_answer("low")),
            ]);

            for (expected_effort, auto) in [(Some("high"), true), (Some("low"), true), (Some("high"), false)] {
                actor.jev_effort_auto.store(auto, std::sync::atomic::Ordering::Relaxed);
                if !auto {
                    let mut pinned = actor
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .expect("test actor has sampling config");
                    pinned.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
                    actor.chat_state_handle.update_sampling_config(pinned);
                }
                // This is the production per-round preparation path. The
                // request is parked only after preparation so no ledger state
                // can substitute for the chooser's final sampler config.
                actor.prepare_sampler_for_turn().await;
                let signals = actor
                    .signals_handle()
                    .snapshot()
                    .await
                    .expect("signals actor should be alive");
                assert_eq!(signals.active_model_id.as_deref(), Some("main-model"));
                assert_eq!(signals.active_reasoning_effort.as_deref(), expected_effort);
                assert_eq!(
                    signals.active_context_window_tokens,
                    Some(272_000),
                    "progress source must publish the main model's window"
                );
                let status = actor.build_status_context().await;
                assert_eq!(
                    status.context_window.context_window_size,
                    Some(272_000),
                    "main status context must consume the main model's window"
                );
                let request = conversation_request(&actor).await;
                let mut budget = actor.rate_limit_wait_budget(None);
                let outcome = actor
                    .run_turn_via_sampler(
                        request,
                        &mut budget,
                        transient_state(0, false),
                        false,
                        TurnParkState::Parked,
                    )
                    .await;
                if let Err(error) = outcome {
                    panic!("controlled route must complete: {error}");
                }
                actor.signals_handle().clear_active_dispatch();
            }
            assert_eq!(
                crate::jev::test_decision_answers_remaining(),
                0,
                "a pinned effort must not pay for a chooser request"
            );
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();

            let responses: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(
                responses.len(),
                3,
                "one captured request per controlled round"
            );
            let actual: Vec<_> = responses
                .iter()
                .map(|body| {
                    (
                        body["model"].as_str().unwrap().to_string(),
                        body.pointer("/reasoning/effort")
                            .and_then(|value| value.as_str())
                            .map(str::to_owned),
                    )
                })
                .collect();
            assert_eq!(
                actual,
                vec![
                    ("main-model".to_string(), Some("high".to_string())),
                    ("main-model".to_string(), Some("low".to_string())),
                    ("main-model".to_string(), Some("high".to_string())),
                ],
                "the sampler must send the chooser's final model and effort, not only ledger state"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn zero_or_single_effort_menus_do_not_invoke_auto_routing() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![MockModelEntry::new("test")])
                .await
                .expect("mock inference server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            actor
                .jev_effort_auto
                .store(true, std::sync::atomic::Ordering::Relaxed);
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_reasoning_model(None);
            crate::jev::set_test_decision_answers([Some(controlled_effort_answer("low"))]);

            // The test actor's default catalog has no effort menu: preparation
            // must keep the configured request and leave the injected answer
            // untouched. This covers the zero-choice guard. A one-choice menu
            // is covered by the same branch in the chooser's unit boundary;
            // the explicit condition is kept here as the regression contract.
            actor.prepare_sampler_for_turn().await;
            let config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            assert_eq!(config.model, "test");
            assert_eq!(config.reasoning_effort, None);
            assert_eq!(crate::jev::test_decision_answers_remaining(), 1);
            crate::jev::clear_test_decision_answers();

            let (single_actor, _retries) =
                actor_under_test(&server, SessionKind::Main, sampler_surfaces_429(), false).await;
            single_actor
                .jev_effort_auto
                .store(true, std::sync::atomic::Ordering::Relaxed);
            install_single_effort_catalog(&single_actor);
            let mut single_config = single_actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            single_config.model = "single-model".to_owned();
            single_config.reasoning_effort = None;
            single_actor
                .chat_state_handle
                .update_sampling_config(single_config);
            crate::jev::set_test_decision_answers([Some(controlled_effort_answer("low"))]);
            single_actor.prepare_sampler_for_turn().await;
            assert_eq!(
                crate::jev::test_decision_answers_remaining(),
                1,
                "a one-level menu must not pay for a chooser request"
            );
            let signals = single_actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.active_model_id.as_deref(), Some("single-model"));
            assert_eq!(signals.active_reasoning_effort, None);
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_local_route_resubmits_base_model_with_fresh_attribution() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let base_server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("main-model"),
            ])
            .await
            .expect("base mock inference server");
            let local_server =
                MockInferenceServer::start_with_models(vec![MockModelEntry::new("local-model")])
                    .await
                    .expect("local mock inference server");
            local_server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::text(400, "local endpoint rejected reasoning payload"),
            );
            base_server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "main-model")),
            );
            let (actor, _retries) = actor_under_test(
                &base_server,
                SessionKind::Main,
                sampler_surfaces_429(),
                false,
            )
            .await;
            actor
                .jev_effort_auto
                .store(true, std::sync::atomic::Ordering::Relaxed);
            actor.models_manager.insert_test_entry(
                "local-model",
                routing_entry_with_window("local-model", &local_server.url(), Vec::new(), 64_000),
            );
            crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
                model: Some("local-model".to_owned()),
                max_context_tokens: Some(256_000),
                ..Default::default()
            });
            crate::jev::set_test_reasoning_model(None);
            crate::jev::set_test_decision_answers([Some(controlled_local_capable_answer())]);
            let mut config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            config.model = "main-model".to_owned();
            config.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            config.context_window = std::num::NonZeroU64::new(128_000).unwrap();
            actor.chat_state_handle.update_sampling_config(config);

            actor.prepare_sampler_for_turn().await;
            let signals = actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.active_model_id.as_deref(), Some("local-model"));
            assert_eq!(signals.active_context_window_tokens, Some(64_000));
            assert_eq!(crate::jev::test_decision_answers_remaining(), 0);
            let request = conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);
            let outcome = actor
                .run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, false),
                    false,
                    TurnParkState::Parked,
                )
                .await;
            if let Err(error) = outcome {
                panic!("local rejection must fall back to the base model: {error}");
            }
            let local_requests: Vec<_> = local_server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            let base_requests: Vec<_> = base_server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(local_requests.len(), 1);
            assert_eq!(local_requests[0]["model"], "local-model");
            assert_eq!(base_requests.len(), 1);
            assert_eq!(base_requests[0]["model"], "main-model");
            let signals = actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.active_model_id.as_deref(), Some("main-model"));
            assert_eq!(signals.active_reasoning_effort.as_deref(), Some("high"));
            assert_eq!(signals.active_context_window_tokens, Some(128_000));
            let status = actor.build_status_context().await;
            assert_eq!(
                status.context_window.context_window_size,
                Some(128_000),
                "fallback status must restore the base model window"
            );
            let rows = actor.jev_ledger.borrow_mut().take_rows();
            assert!(rows.iter().any(|row| row.model == "local-model"));
            assert!(rows.iter().any(|row| {
                row.model == "main-model" && row.effort.as_deref() == Some("high")
            }));
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();
        })
        .await;
}

/// A rejected local round must resubmit the session model's thinking payload.
/// DeepSeek 400s with "reasoning_content must be passed back" if fallback
/// drops the Reasoning sibling that folds onto the tool-call assistant.
#[tokio::test(flavor = "current_thread")]
async fn rejected_local_route_resubmits_reasoning_content_to_the_session_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            const REASONING_TEXT: &str = "thinking-must-be-passed-back";
            let base_server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("main-model"),
            ])
            .await
            .expect("base mock inference server");
            let local_server =
                MockInferenceServer::start_with_models(vec![MockModelEntry::new("local-model")])
                    .await
                    .expect("local mock inference server");
            local_server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::text(
                    400,
                    "The `reasoning_content` in the thinking mode must be passed back to the API.",
                ),
            );
            base_server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "main-model")),
            );
            let (actor, _retries) = actor_under_test(
                &base_server,
                SessionKind::Main,
                sampler_surfaces_429(),
                false,
            )
            .await;
            actor
                .jev_effort_auto
                .store(true, std::sync::atomic::Ordering::Relaxed);
            actor.models_manager.insert_test_entry(
                "local-model",
                routing_entry_with_window("local-model", &local_server.url(), Vec::new(), 64_000),
            );
            crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
                model: Some("local-model".to_owned()),
                max_context_tokens: Some(256_000),
                ..Default::default()
            });
            crate::jev::set_test_reasoning_model(None);
            crate::jev::set_test_decision_answers([Some(controlled_local_capable_answer())]);
            let mut config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            config.model = "main-model".to_owned();
            config.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            config.context_window = std::num::NonZeroU64::new(128_000).unwrap();
            actor.chat_state_handle.update_sampling_config(config);

            actor
                .chat_state_handle
                .push_user_message(ConversationItem::user("continue"));
            actor
                .chat_state_handle
                .push_model_output(ConversationItem::Reasoning(
                    distill_sampling_types::synthesized_reasoning_item(REASONING_TEXT),
                ));
            actor
                .chat_state_handle
                .push_model_output(ConversationItem::assistant_tool_calls(vec![
                    distill_sampling_types::ToolCall {
                        id: "call_run".into(),
                        name: "bash".to_string(),
                        arguments: r#"{"command":"rustc -vV"}"#.into(),
                    },
                ]));
            actor
                .chat_state_handle
                .push_tool_result(ConversationItem::tool_result("call_run", "rustc 1.80.0"));

            actor.prepare_sampler_for_turn().await;
            let request = conversation_request(&actor).await;
            assert!(
                request
                    .items
                    .iter()
                    .any(|item| matches!(item, ConversationItem::Reasoning(_))),
                "the turn request must still carry the thinking sibling"
            );
            let mut budget = actor.rate_limit_wait_budget(None);
            let outcome = actor
                .run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, false),
                    false,
                    TurnParkState::Parked,
                )
                .await;
            if let Err(error) = outcome {
                panic!("local rejection must fall back to the session model: {error}");
            }
            let local_requests: Vec<_> = local_server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            let base_requests: Vec<_> = base_server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(local_requests.len(), 1);
            assert_eq!(base_requests.len(), 1);
            assert!(
                request_body_carries_text(&base_requests[0], REASONING_TEXT),
                "fallback to the session model must pass reasoning_content back: {}",
                base_requests[0]
            );
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();
        })
        .await;
}

fn request_body_carries_text(body: &serde_json::Value, text: &str) -> bool {
    body.to_string().contains(text)
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_child_model_and_effort_survive_all_routing_passes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("main-model"),
                MockModelEntry::new("other-model"),
            ])
            .await
            .expect("mock inference server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (actor, _retries) = actor_under_test_with_startup_policy(
                &server,
                SessionKind::Subagent,
                sampler_surfaces_429(),
                false,
                true,
            )
            .await;
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_reasoning_model(None);
            install_wire_routing_catalog(&actor, &server.url());
            actor
                .jev_effort_auto
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let mut config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            config.model = "main-model".to_owned();
            config.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            actor.chat_state_handle.update_sampling_config(config);
            crate::jev::set_test_decision_answers([Some(controlled_effort_answer("low"))]);

            actor.prepare_sampler_for_turn().await;
            let signals = actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.active_model_id.as_deref(), Some("main-model"));
            assert_eq!(signals.active_reasoning_effort.as_deref(), Some("high"));
            assert_eq!(
                crate::jev::test_decision_answers_remaining(),
                1,
                "manual child policy must not invoke the chooser"
            );
            assert_eq!(
                actor.jev_ledger.borrow().pending_route_model().as_deref(),
                Some("main-model")
            );
            let request = conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);
            let outcome = actor
                .run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Parked,
                )
                .await;
            if let Err(error) = outcome {
                panic!("explicit child route must complete: {error}");
            }
            let requests: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["model"], "main-model");
            assert_eq!(
                requests[0]
                    .pointer("/reasoning/effort")
                    .and_then(|value| value.as_str()),
                Some("high")
            );

            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_child_model_with_auto_effort_stays_pinned() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("main-model"),
                MockModelEntry::new("other-model"),
            ])
            .await
            .expect("mock inference server");
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );
            let (actor, _retries) = actor_under_test_with_startup_policy(
                &server,
                SessionKind::Subagent,
                sampler_surfaces_429(),
                false,
                true,
            )
            .await;
            actor
                .jev_effort_auto
                .store(true, std::sync::atomic::Ordering::Relaxed);
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_reasoning_model(None);
            install_wire_routing_catalog(&actor, &server.url());
            let mut config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            config.model = "main-model".to_owned();
            config.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            actor.chat_state_handle.update_sampling_config(config);
            crate::jev::set_test_decision_answers([Some(controlled_effort_answer("low"))]);

            actor.prepare_sampler_for_turn().await;
            let signals = actor
                .signals_handle()
                .snapshot()
                .await
                .expect("signals actor should be alive");
            assert_eq!(signals.active_model_id.as_deref(), Some("main-model"));
            assert_eq!(signals.active_reasoning_effort.as_deref(), Some("low"));
            assert_eq!(crate::jev::test_decision_answers_remaining(), 0);
            assert_eq!(
                actor.jev_ledger.borrow().pending_route_model().as_deref(),
                Some("main-model")
            );

            let request = conversation_request(&actor).await;
            let mut budget = actor.rate_limit_wait_budget(None);
            let outcome = actor
                .run_turn_via_sampler(
                    request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Parked,
                )
                .await;
            if let Err(error) = outcome {
                panic!("explicit model plus auto effort must complete: {error}");
            }
            let requests: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["model"], "main-model");
            assert_eq!(
                requests[0]
                    .pointer("/reasoning/effort")
                    .and_then(|value| value.as_str()),
                Some("low")
            );
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_reasoning_model();
        })
        .await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_sends_updated_final_model_and_effort() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test"),
                MockModelEntry::new("other-model"),
                MockModelEntry::new("main-model"),
            ])
            .await
            .expect("mock inference server");
            server.enqueue_response("/v1/responses", rate_limited_reply(90));
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact("done", "test")),
            );

            let (actor, _retries) =
                actor_under_test(&server, SessionKind::Subagent, sampler_surfaces_429(), true)
                    .await;
            actor
                .jev_effort_auto
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let mut initial = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            initial.model = "other-model".to_string();
            initial.reasoning_effort = None;
            actor.chat_state_handle.update_sampling_config(initial);

            let request = conversation_request(&actor).await;
            let runner = actor.clone();
            let task = tokio::task::spawn_local(async move {
                let mut budget = runner.rate_limit_wait_budget(None);
                runner
                    .run_turn_via_sampler(
                        request,
                        &mut budget,
                        transient_state(0, true),
                        false,
                        TurnParkState::Fresh,
                    )
                    .await
            });
            pump_local_tasks().await;

            let mut retry_config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("test actor has sampling config");
            retry_config.model = "main-model".to_string();
            retry_config.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::High);
            actor.chat_state_handle.update_sampling_config(retry_config);
            tokio::time::advance(Duration::from_secs(31)).await;
            let outcome = task.await.expect("retry task must join");
            if let Err(error) = outcome {
                panic!("retry must complete: {error}");
            }

            let responses: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(responses.len(), 2, "initial request plus one retry");
            assert_eq!(responses[0]["model"], "other-model");
            assert!(responses[0].pointer("/reasoning/effort").is_none());
            assert_eq!(responses[1]["model"], "main-model");
            assert_eq!(
                responses[1]
                    .pointer("/reasoning/effort")
                    .and_then(|v| v.as_str()),
                Some("high")
            );
        })
        .await;
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
                .retries
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
                .retries
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
