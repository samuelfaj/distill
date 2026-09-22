// Modified for Distill by Samuel Fajreldines, 2026.
use distill_sampling_types::{SearchDateBound, ToolOverrides, WebSearchOptions, XSearchOptions};

use super::{
    CLASSIFIER_REQUEST_TOKEN_RESERVE, LengthSalvageAction, LengthSalvageStreak,
    MAX_OUTPUT_TOKEN_LIMIT_RETRIES, classifier_request_fits_context,
    compact_skill_tool_definition, resolve_configured_cutoff,
};

#[test]
fn chatgpt_requests_do_not_use_or_refresh_the_grok_session() {
    use crate::agent::auth_method::ModelByok;
    let method = agent_client_protocol::AuthMethodId::new("cached_token");
    let gate = super::SessionTokenAuthGate::new(
        Some(&method),
        ModelByok::NotByok,
        &crate::codex_auth::inference_base_url(),
    );
    assert!(!gate.active());
    let grok =
        super::SessionTokenAuthGate::new(Some(&method), ModelByok::NotByok, "https://api.x.ai/v1");
    assert!(grok.active());
}

fn x_cut(to: &str) -> XSearchOptions {
    XSearchOptions {
        date_bound: Some(SearchDateBound::new(None, Some(to.into())).unwrap()),
    }
}

/// When every sample ends with finish reason Length while tools are active, the turn salvages up to the cap and then fails.
/// The reminder is injected only on the first salvage.
#[test]
fn length_salvage_streak_proceeds_to_the_cap_then_exhausts() {
    let mut streak = LengthSalvageStreak::default();
    for n in 1..=MAX_OUTPUT_TOKEN_LIMIT_RETRIES {
        match streak.on_sample(true) {
            LengthSalvageAction::Proceed { inject_reminder } => {
                assert_eq!(
                    inject_reminder,
                    n == 1,
                    "reminder only on the first salvage"
                );
            }
            other => panic!("salvage {n} within the cap must proceed, got {other:?}"),
        }
    }
    assert!(matches!(
        streak.on_sample(true),
        LengthSalvageAction::Exhausted
    ));
}

/// A non-Length (or tool-less) sample resets the streak: a later salvage starts a fresh streak and re-injects the reminder.
#[test]
fn length_salvage_streak_resets_on_non_length_sample() {
    let mut streak = LengthSalvageStreak::default();
    for _ in 0..MAX_OUTPUT_TOKEN_LIMIT_RETRIES {
        let _ = streak.on_sample(true);
    }
    assert!(matches!(
        streak.on_sample(false),
        LengthSalvageAction::NotSalvage
    ));
    assert!(matches!(
        streak.on_sample(true),
        LengthSalvageAction::Proceed {
            inject_reminder: true
        }
    ));
}

#[test]
fn classifier_request_bound_enforces_its_reserve_with_saturating_arithmetic() {
    let window = 12_000 + CLASSIFIER_REQUEST_TOKEN_RESERVE;
    for (input, context_window, expected) in [
        (12_000, window, true),
        (12_001, window, false),
        (u64::MAX, u64::MAX, false),
    ] {
        assert_eq!(
            classifier_request_fits_context(input, context_window),
            expected
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn route_capacity_parser_and_admission_preserve_parent_window() {
    use super::super::support::create_test_actor;
    use super::super::compaction::{
        configured_output_token_budget, error_context_window,
        explicit_provider_context_window,
    };
    use super::super::compaction_config::{
        SUPPRESS_AUTH, SUPPRESS_NONE, SUPPRESS_STICKY, SUPPRESS_TURN,
    };
    use distill_sampling_types::ConversationRequest;
    use distill_sampler::{SamplingErrorInfo, SamplingErrorKind, SamplerConfig};
    use std::num::NonZeroU64;
    use std::sync::Arc;
    use tokio::task::LocalSet;

    LocalSet::new()
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = Arc::new(
                create_test_actor(99_741, 262_144, 85, gateway_tx, persistence_tx).await,
            );
            let route = SamplerConfig {
                base_url: "https://api.deepinfra.com/v1".to_owned(),
                model: "deepinfra/unknown-model".to_owned(),
                context_window: 262_144,
                max_completion_tokens: Some(32_768),
                ..Default::default()
            };
            let request = ConversationRequest::default();

            assert!(actor
                .route_request_overflow_trigger(&request, Some(&route))
                .await
                .is_none(),
                "the selected route's 262K envelope must fit before provider feedback"
            );
            assert_eq!(configured_output_token_budget(&request, &route), Some(32_768));

            // The raw DeepInfra error supplies the actual endpoint limit even
            // though the selected route and parent window are both 262K.
            let raw = "Upstream error from DeepInfra: Requested token count exceeds the model's maximum context length of 131072 tokens. You requested a total of 132509 tokens: 99741 tokens from the input messages and 32768 tokens for the completion. Please reduce the number of tokens in the input messages or the completion to fit within the limit.";
            let error = SamplingErrorInfo {
                kind: SamplingErrorKind::Api,
                status_code: Some(400),
                message: raw.to_owned(),
                is_retryable: false,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
                model_metadata: None,
                empty_response_context: None,
                doom_loop_triggers: None,
                doom_loop_aborted_at_chunk: None,
                credential: distill_sampling_types::SentCredential::Unknown,
            };
            assert_eq!(explicit_provider_context_window(raw), Some(131_072));
            assert_eq!(error_context_window(&error, Some(&route)), Some(131_072));
            let mut auth_error = error.clone();
            auth_error.kind = SamplingErrorKind::Auth;
            auth_error.status_code = Some(401);
            assert!(
                !actor
                    .should_compact_on_error_for_route(&auth_error, Some(&route), Some(32_768))
                    .await,
                "route capacity evidence must not steal the auth recovery path"
            );
            let mut rate_error = error.clone();
            rate_error.kind = SamplingErrorKind::RateLimited;
            rate_error.status_code = Some(429);
            assert!(
                !actor
                    .should_compact_on_error_for_route(&rate_error, Some(&route), Some(32_768))
                    .await,
                "route capacity evidence must not steal the rate-limit recovery path"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("parent config")
                    .context_window,
                NonZeroU64::new(262_144).expect("non-zero parent cap")
            );

            let mut near_boundary_route = route.clone();
            near_boundary_route.context_window = 134_000;
            let trigger = actor
                .route_request_overflow_trigger(&request, Some(&near_boundary_route))
                .await
                .expect("input plus effective output plus margin must reject near the cap");
            assert_eq!(trigger.tokens_used, 99_741);

            let mut input_only_route = route.clone();
            input_only_route.context_window = 100_000;
            input_only_route.max_completion_tokens = None;
            assert!(actor
                .route_request_overflow_trigger(&request, Some(&input_only_route))
                .await
                .is_some(), "unknown output cannot bypass a known input overflow");

            for suppression in [SUPPRESS_AUTH, SUPPRESS_STICKY, SUPPRESS_TURN] {
                actor
                    .compaction
                    .auto_compact_suppressed
                    .store(suppression, std::sync::atomic::Ordering::Relaxed);
                let error = actor
                    .preflight_route_context(&request, Some(&input_only_route), false)
                    .await
                    .expect_err("known overflow under suppression must be local");
                assert_eq!(
                    crate::sampling::error::error_code_from_data(&error),
                    Some(crate::extensions::notification::CONTEXT_LENGTH_ERROR_TYPE),
                    "suppression {suppression} must not dispatch a known-impossible request"
                );
            }
            actor
                .compaction
                .auto_compact_suppressed
                .store(SUPPRESS_NONE, std::sync::atomic::Ordering::Relaxed);

            for form in [
                "maximum context length is 131072 tokens",
                "maximum context length of 131072 tokens",
                "context window: 131072",
            ] {
                assert_eq!(explicit_provider_context_window(form), Some(131_072), "{form}");
            }
            assert_eq!(
                explicit_provider_context_window(
                    "requested total 132509: 99741 input and 32768 completion"
                ),
                None,
                "request counters are not endpoint capacity"
            );
            assert_eq!(
                explicit_provider_context_window("deepinfra/model-131072"),
                None,
                "model digits are not endpoint capacity"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn route_preflight_keeps_mid_salvage_terminal_without_rewrite_or_usage() {
    use super::super::support::create_test_actor;
    use super::super::TurnParkState;
    use distill_sampling_types::{ConversationItem, ConversationRequest};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::task::LocalSet;

    LocalSet::new()
        .run_until(async {
            crate::jev::set_test_decision_answers([]);
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_tier_config(Default::default());

            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut actor = create_test_actor(95_000, 100_000, 85, gateway_tx, persistence_tx).await;
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("source context"),
                ConversationItem::user("continue the unfinished work"),
                ConversationItem::assistant("joined report"),
            ]);
            actor.chat_state_handle.record_token_usage(95_000);
            let actor = Arc::new(actor);
            let before_conversation = serde_json::to_value(
                actor.chat_state_handle.get_conversation().await,
            )
            .expect("serializable conversation");
            let before_usage = actor
                .chat_state_handle
                .try_get_session_usage()
                .await
                .expect("session usage");
            let mut request = ConversationRequest::default();
            request.max_output_tokens = Some(10_000);
            let mut budget = actor.rate_limit_wait_budget(None);

            let result = actor
                .run_turn_via_sampler(
                    request,
                    &mut budget,
                    super::super::support::transient_state(0, true),
                    true,
                    TurnParkState::Fresh,
                )
                .await;
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("oversize salvage must terminate before dispatch"),
            };
            assert!(crate::sampling::error::is_max_tokens_turn_error(&error));
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|data| data.get(crate::sampling::error::SALVAGE_CAUSE_KEY))
                    .and_then(|value| value.as_str()),
                Some(crate::sampling::error::SALVAGE_CAUSE_OVERFLOW)
            );
            assert!(!actor.route_overflow_recovery_armed());
            assert_eq!(
                actor.compaction.count.load(Ordering::Relaxed),
                0,
                "salvage overflow must not rewrite the conversation"
            );
            assert_eq!(
                serde_json::to_value(actor.chat_state_handle.get_conversation().await)
                    .expect("serializable conversation after preflight"),
                before_conversation,
                "source/report must survive the quiet salvage termination"
            );
            assert_eq!(
                actor
                    .chat_state_handle
                    .try_get_session_usage()
                    .await
                    .expect("session usage after preflight"),
                before_usage,
                "a pre-dispatch refusal must not invent usage"
            );
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_tier_config();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn route_preflight_keeps_budgeted_children_local_without_compaction_or_usage() {
    use super::super::support::{create_test_actor, transient_state};
    use super::super::TurnParkState;
    use distill_sampling_types::ConversationRequest;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use tokio::task::LocalSet;

    LocalSet::new()
        .run_until(async {
            crate::jev::set_test_decision_answers([]);
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_tier_config(Default::default());

            for (task_output_budget, retry_only_before_output) in [(true, false), (false, true)] {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut actor =
                    create_test_actor(95_000, 100_000, 85, gateway_tx, persistence_tx).await;
                let output_budget = task_output_budget.then(|| {
                    crate::tools::tool_context::TaskOutputTokenBudget::limited(1_000)
                });
                actor.tool_context.task_output_token_budget = output_budget.clone();
                actor.tool_context.sampler_retry_only_before_output = retry_only_before_output;
                let actor = Arc::new(actor);
                let mut request = ConversationRequest::default();
                request.max_output_tokens = Some(10_000);
                let mut budget = actor.rate_limit_wait_budget(None);

                let result = actor
                    .run_turn_via_sampler(
                        request,
                        &mut budget,
                        transient_state(0, true),
                        false,
                        TurnParkState::Fresh,
                    )
                    .await;
                let error = match result {
                    Err(error) => error,
                    Ok(_) => panic!("workflow child must fail locally before dispatch"),
                };
                assert_eq!(
                    crate::sampling::error::error_code_from_data(&error),
                    Some(crate::extensions::notification::CONTEXT_LENGTH_ERROR_TYPE)
                );
                assert!(!actor.route_overflow_recovery_armed());
                assert_eq!(actor.compaction.count.load(Ordering::Relaxed), 0);
                assert_eq!(
                    actor
                        .chat_state_handle
                        .try_get_session_usage()
                        .await
                        .expect("session usage after local failure")
                        .attributions
                        .len(),
                    0,
                    "a pre-dispatch refusal must not create a paid-attempt row"
                );
                if let Some(output_budget) = output_budget {
                    assert_eq!(
                        output_budget.usage(),
                        (0, false),
                        "a pre-dispatch refusal must not exhaust the output grant"
                    );
                }
            }

            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_tier_config();
        })
        .await;
}

#[test]
fn deepinfra_overflow_compacts_rebuilds_once_and_allows_later_growth() {
    use super::super::support::{
        create_test_actor, drain_gateway, drain_persistence, transient_state,
    };
    use super::super::{SamplerTurnOutcome, TurnParkState};
    use distill_sampling_types::{ApiBackend, ConversationItem};
    use distill_sampler::{RetryPolicy, SamplerActor, SamplerConfig};
    use distill_test_support::sse::responses_api_script_exact;
    use distill_test_support::{MockInferenceServer, MockModelEntry, ScriptedResponse};
    use std::sync::Arc;
    use tokio::task::LocalSet;

    const RAW_OVERFLOW: &str = "Upstream error from DeepInfra: Requested token count exceeds the model's maximum context length of 131072 tokens. You requested a total of 132509 tokens: 99741 tokens from the input messages and 32768 tokens for the completion. Please reduce the number of tokens in the input messages or the completion to fit within the limit.";

    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            LocalSet::new().block_on(&runtime, async {
            // The real Jev chooser must not run from this sampler seam test:
            // an exhausted test queue returns None without a provider call.
            crate::jev::set_test_decision_answers([]);
            crate::jev::set_test_local_config(Default::default());
            crate::jev::set_test_tier_config(Default::default());

            let server = MockInferenceServer::start_with_models(vec![
                MockModelEntry::new("test").with_api_backend("responses"),
            ])
            .await
            .expect("mock server");
            let summary = "Summary of prior work. ".repeat(40);
            let overflow = || {
                ScriptedResponse::json(
                    400,
                    serde_json::json!({
                        "error": {
                            "type": "invalid_request_error",
                            "message": RAW_OVERFLOW,
                        }
                    }),
                )
            };
            server.enqueue_response("/v1/responses", overflow());
            server.enqueue_response(
                "/v1/responses",
                ScriptedResponse::sse(responses_api_script_exact(&summary, "test")),
            );
            server.enqueue_response("/v1/responses", overflow());

            let sampling_cfg = SamplerConfig {
                api_key: Some("test-key".to_owned()),
                base_url: server.url(),
                model: "test".to_owned(),
                api_backend: ApiBackend::Responses,
                context_window: 262_144,
                max_completion_tokens: Some(32_768),
                max_retries: Some(0),
                idle_timeout_secs: Some(30),
                ..Default::default()
            };
            let (sampler_event_tx, sampler_event_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_sampler::SamplingEvent>();
            let sampler_handle = SamplerActor::spawn(
                sampling_cfg,
                RetryPolicy {
                    max_retries: 0,
                    ..Default::default()
                },
                sampler_event_tx,
            );
            let (gateway_tx, gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            drain_persistence(persistence_rx);
            let mut actor =
                create_test_actor(99_741, 262_144, 85, gateway_tx, persistence_tx).await;
            actor.sampler_handle = sampler_handle;
            actor.max_retries = 0;
            actor.transient_retry_enabled = true;
            let mut parent_config = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("parent config");
            parent_config.base_url = server.url();
            parent_config.api_backend = ApiBackend::Responses;
            parent_config.model = "test".to_owned();
            parent_config.max_completion_tokens = Some(32_768);
            actor.chat_state_handle.update_sampling_config(parent_config);
            assert_eq!(
                actor
                    .chat_state_handle
                    .get_sampling_config()
                    .await
                    .expect("updated parent config")
                    .base_url,
                server.url(),
                "the fake actor must use the scripted server before entering the turn"
            );
            let filler = "x".repeat(8_000);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("system"),
                ConversationItem::user(format!("prior work {filler}")),
                ConversationItem::assistant(format!("prior result {filler}")),
                ConversationItem::user("continue the long-running goal"),
            ]);
            actor.chat_state_handle.record_token_usage(99_741);

            let actor = Arc::new(actor);
            {
                let drainer = actor.clone();
                let mut sampler_event_rx = sampler_event_rx;
                tokio::task::spawn_local(async move {
                    while let Some(event) = sampler_event_rx.recv().await {
                        drainer.handle_sampling_event(event).await;
                    }
                });
            }

            // Exercise the production sampler/recovery seam directly. The
            // outer turn loop rebuilds this same request after receiving the
            // CompactAndResubmit outcome.
            let mut budget = actor.rate_limit_wait_budget(None);
            let first_result = actor
                .run_turn_via_sampler(
                    actor
                        .chat_state_handle
                        .build_request(
                            Vec::new(),
                            None,
                            false,
                            None,
                            actor.session_id_string(),
                            "overflow-recovery".to_owned(),
                        )
                        .await
                        .expect("initial request"),
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            match first_result {
                Ok(SamplerTurnOutcome::CompactAndResubmit) => {}
                Ok(SamplerTurnOutcome::Response(..)) => {
                    panic!(
                        "outcome=Response; the scripted overflow unexpectedly returned a response; {}",
                        server.request_log_summary()
                    )
                }
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { .. }) => {
                    panic!(
                        "outcome=RefreshAuthAndResubmit; the scripted overflow unexpectedly entered auth recovery; {}",
                        server.request_log_summary()
                    )
                }
                Ok(SamplerTurnOutcome::RetryTransient { .. }) => {
                    panic!(
                        "outcome=RetryTransient; the scripted overflow unexpectedly entered transient retry; {}",
                        server.request_log_summary()
                    )
                }
                Err(error) => panic!(
                    "outcome=Err; the first provider overflow did not compact: {error}; {}",
                    server.request_log_summary()
                ),
            }
            let response_requests: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(response_requests.len(), 2, "main overflow plus real compaction");
            assert_ne!(
                response_requests[0], response_requests[1],
                "the compaction request must differ from the failed main request"
            );
            assert_eq!(
                response_requests[1]
                    .get("max_output_tokens")
                    .and_then(|value| value.as_u64()),
                Some(32_768),
                "the real compaction dispatch must carry the effective output budget"
            );
            assert!(
                response_requests
                    .iter()
                    .all(|body| body.get("model").and_then(|model| model.as_str()) == Some("test")),
                "every selected provider request must stay pinned to test; {}",
                server.request_log_summary()
            );
            assert!(
                server
                    .requests()
                    .iter()
                    .filter(|entry| entry.path.contains("/v1/responses"))
                    .all(|entry| entry.path == "/v1/responses"),
                "the recovery seam must use the scripted Responses endpoint; {}",
                server.request_log_summary()
            );
            let parent_after_compaction = actor
                .chat_state_handle
                .get_sampling_config()
                .await
                .expect("parent config after compaction");
            assert_eq!(parent_after_compaction.model, "test");
            assert_eq!(parent_after_compaction.api_backend, ApiBackend::Responses);
            assert_eq!(parent_after_compaction.context_window.get(), 262_144);
            assert_eq!(parent_after_compaction.max_completion_tokens, Some(32_768));

            let rebuilt_request = actor
                .chat_state_handle
                .build_request(
                    Vec::new(),
                    None,
                    false,
                    None,
                    actor.session_id_string(),
                    "overflow-recovery-rebuilt".to_owned(),
                )
                .await
                .expect("rebuilt request");
            let second_result = actor
                .run_turn_via_sampler(
                    rebuilt_request,
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            assert_eq!(
                server
                    .request_bodies()
                    .into_iter()
                    .filter(|body| body.get("model").is_some())
                    .count(),
                3,
                "original overflow, one compaction request, and one rebuilt retry"
            );
            let response_requests: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            match second_result {
                Err(_) => {}
                Ok(SamplerTurnOutcome::Response(..)) => panic!(
                    "outcome=Response; same impossible episode was dispatched as success; {}",
                    server.request_log_summary()
                ),
                Ok(SamplerTurnOutcome::CompactAndResubmit) => panic!(
                    "outcome=CompactAndResubmit; same impossible episode compacted twice; {}",
                    server.request_log_summary()
                ),
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { .. }) => panic!(
                    "outcome=RefreshAuthAndResubmit; same impossible episode entered auth recovery; {}",
                    server.request_log_summary()
                ),
                Ok(SamplerTurnOutcome::RetryTransient { .. }) => panic!(
                    "outcome=RetryTransient; same impossible episode entered transient retry; {}",
                    server.request_log_summary()
                ),
            }
            assert_ne!(
                response_requests[0].get("input"),
                response_requests[2].get("input"),
                "the impossible request input must not be resent unchanged after compaction"
            );
            assert!(actor.route_overflow_recovery_armed());

            server.set_response("later successful goal progress");
            let later_result = actor
                .run_turn_via_sampler(
                    actor
                        .chat_state_handle
                        .build_request(
                            Vec::new(),
                            None,
                            false,
                            None,
                            actor.session_id_string(),
                            "later-progress".to_owned(),
                        )
                        .await
                        .expect("later request"),
                    &mut budget,
                    transient_state(0, true),
                    false,
                    TurnParkState::Fresh,
                )
                .await;
            match later_result {
                Ok(SamplerTurnOutcome::Response(..)) => {}
                Ok(SamplerTurnOutcome::CompactAndResubmit) => panic!(
                    "outcome=CompactAndResubmit; later progress did not reach the provider; {}",
                    server.request_log_summary()
                ),
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { .. }) => panic!(
                    "outcome=RefreshAuthAndResubmit; later progress entered auth recovery; {}",
                    server.request_log_summary()
                ),
                Ok(SamplerTurnOutcome::RetryTransient { .. }) => panic!(
                    "outcome=RetryTransient; later progress entered transient retry; {}",
                    server.request_log_summary()
                ),
                Err(error) => panic!(
                    "outcome=Err; later progress failed: {error}; {}",
                    server.request_log_summary()
                ),
            }
            assert!(!actor.route_overflow_recovery_armed());
            actor.chat_state_handle.record_token_usage(230_000);
            assert!(
                actor.check_auto_compact_needed().await.is_some(),
                "later goal growth must be allowed to compact again"
            );
            let final_bodies: Vec<_> = server
                .request_bodies()
                .into_iter()
                .filter(|body| body.get("model").is_some())
                .collect();
            assert_eq!(final_bodies.len(), 4, "one later successful provider request");
            assert!(
                final_bodies
                    .iter()
                    .all(|body| body.get("model").and_then(|model| model.as_str()) == Some("test")),
                "all recovery requests must retain the selected model pin; {}",
                server.request_log_summary()
            );
            assert_eq!(
                server
                    .requests()
                    .into_iter()
                    .filter(|entry| entry.path.contains("/v1/responses"))
                    .count(),
                4,
                "all four scripted provider calls must use /v1/responses"
            );
            crate::jev::clear_test_decision_answers();
            crate::jev::clear_test_local_config();
            crate::jev::clear_test_tier_config();
            });
        })
        .expect("spawn large-stack test thread")
        .join()
        .expect("test thread");
}

#[tokio::test(flavor = "current_thread")]
async fn route_overflow_latch_resets_after_real_progress_for_later_goal_growth() {
    use super::super::support::create_test_actor;
    use std::sync::atomic::Ordering;
    use tokio::task::LocalSet;

    LocalSet::new()
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await;
            assert!(actor.check_auto_compact_needed().await.is_some());

            actor.arm_route_overflow_recovery();
            assert!(actor.route_overflow_recovery_armed());
            assert!(actor.check_auto_compact_needed().await.is_none());

            // The dispatch success path clears the same latch; model-visible
            // progress must permit the next long-goal growth to compact again.
            actor.clear_route_overflow_recovery();
            assert!(!actor.route_overflow_recovery_armed());
            assert_eq!(
                actor
                    .compaction
                    .auto_compact_suppressed
                    .load(Ordering::Relaxed),
                crate::session::compaction_config::SUPPRESS_NONE
            );
            assert!(actor.check_auto_compact_needed().await.is_some());

            for preserved in [
                crate::session::compaction_config::SUPPRESS_TURN,
                crate::session::compaction_config::SUPPRESS_AUTH,
                crate::session::compaction_config::SUPPRESS_STICKY,
            ] {
                actor
                    .compaction
                    .auto_compact_suppressed
                    .store(preserved, Ordering::Relaxed);
                actor.clear_route_overflow_recovery();
                assert_eq!(
                    actor
                        .compaction
                        .auto_compact_suppressed
                        .load(Ordering::Relaxed),
                    preserved,
                    "clearing a route episode must preserve suppression state {preserved}"
                );
            }
        })
        .await;
}

#[test]
fn compact_skill_schema_keeps_invocation_and_recovery_without_catalog_rows() {
    use crate::sampling::types::ToolDefinition;

    let definition = ToolDefinition::function(
        "skill",
        Some("<available_skills><skill name=\"old\" /></available_skills>"),
        serde_json::json!({"type": "object"}),
    );
    let compact = compact_skill_tool_definition(definition, Some("skill"));
    let description = compact
        .function
        .description
        .expect("compact skill description");
    assert!(description.contains("AvailableSkills"));
    assert!(description.contains("skill_content"));
    assert!(description.contains("recovery/resource"));
    assert!(!description.contains("<available_skills>"));
}

#[test]
fn seed_cutoff_is_inherited_without_a_per_turn_update() {
    let seed = ToolOverrides {
        x_search: Some(x_cut("2020-01-01")),
        web_search: None,
    };
    assert_eq!(resolve_configured_cutoff(Some(seed.clone()), None), seed);
}

#[test]
fn non_empty_base_cutoff_wins_per_tool_and_an_empty_one_reverts_to_the_seed() {
    let seed = ToolOverrides {
        x_search: Some(x_cut("2020-01-01")),
        web_search: Some(WebSearchOptions {
            allowed_domains: Some(vec!["x.com".into()]),
            excluded_domains: None,
        }),
    };
    let base = ToolOverrides {
        x_search: Some(x_cut("2019-06-01")),
        web_search: Some(WebSearchOptions {
            allowed_domains: Some(vec![]),
            excluded_domains: None,
        }),
    };
    let got = resolve_configured_cutoff(Some(seed.clone()), Some(&base));
    assert_eq!(got.x_search, Some(x_cut("2019-06-01")));
    assert_eq!(got.web_search, seed.web_search);
}

#[test]
fn inherited_cutoff_agrees_with_the_wire_echo_so_the_two_implementations_cannot_drift() {
    use distill_sampling_types::{HostedTool, apply_tool_overrides};
    let web = WebSearchOptions {
        allowed_domains: Some(vec!["x.com".into()]),
        excluded_domains: None,
    };
    let cases = [
        (
            Some(ToolOverrides {
                x_search: Some(x_cut("2020-01-01")),
                web_search: None,
            }),
            None,
        ),
        (
            Some(ToolOverrides {
                x_search: Some(x_cut("2020-01-01")),
                web_search: Some(web.clone()),
            }),
            Some(ToolOverrides {
                x_search: Some(x_cut("2019-06-01")),
                web_search: None,
            }),
        ),
        (
            None,
            Some(ToolOverrides {
                x_search: Some(x_cut("2018-01-01")),
                web_search: Some(web.clone()),
            }),
        ),
    ];
    for (seed, base) in cases {
        let mut tools = vec![
            HostedTool::WebSearch { options: None },
            HostedTool::XSearch { options: None },
        ];
        apply_tool_overrides(&mut tools, seed.as_ref());
        let wire_echo = apply_tool_overrides(&mut tools, base.as_ref());
        let inherited = resolve_configured_cutoff(seed.clone(), base.as_ref());
        assert_eq!(wire_echo, inherited, "seed={seed:?} base={base:?}");
    }
}

#[cfg(test)]
mod subagent_sampling_gate_tests {
    use super::super::super::support::create_test_actor;
    use super::super::acquire_subagent_sampling_permit;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    #[derive(Default)]
    struct ConcurrencyProbe {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl ConcurrencyProbe {
        fn enter(&self) {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        }
        fn leave(&self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn subagent_submits_never_exceed_cap_and_excess_queues() {
        const CAP: usize = 3;
        const TURNS: usize = 12;
        let semaphore = Arc::new(Semaphore::new(CAP));
        let probe = Arc::new(ConcurrencyProbe::default());
        let ran = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..TURNS {
            let gate = Some(semaphore.clone());
            let probe = probe.clone();
            let ran = ran.clone();
            handles.push(tokio::spawn(async move {
                let permit = acquire_subagent_sampling_permit(&gate).await;
                assert!(permit.is_some(), "a subagent turn must receive a permit");
                probe.enter();
                tokio::time::sleep(Duration::from_millis(20)).await;
                probe.leave();
                ran.fetch_add(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            ran.load(Ordering::SeqCst),
            TURNS,
            "every queued turn ran (queued, not errored)"
        );
        assert!(
            probe.max_in_flight.load(Ordering::SeqCst) <= CAP,
            "in-flight subagent submits exceeded the cap: {} > {CAP}",
            probe.max_in_flight.load(Ordering::SeqCst),
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_without_deadlock() {
        let semaphore = Arc::new(Semaphore::new(1));
        let gate = Some(semaphore.clone());
        let held = acquire_subagent_sampling_permit(&gate).await;
        assert!(held.is_some());

        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move {
                let _permit = acquire_subagent_sampling_permit(&gate).await;
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            semaphore.available_permits(),
            0,
            "slot stays held while the second turn queues"
        );
        waiter.abort();
        let _ = waiter.await;

        drop(held);
        let next = tokio::time::timeout(
            Duration::from_millis(200),
            acquire_subagent_sampling_permit(&gate),
        )
        .await
        .expect("a permit must be free once the held one is released");
        assert!(next.is_some(), "the cancelled waiter did not leak the slot");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn submit_holds_permit_for_subagent_not_main() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let saturated = Arc::new(Semaphore::new(1));
                let _held = saturated.clone().acquire_owned().await.unwrap();

                let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let (p_tx, _p_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut subagent = create_test_actor(0, 200_000, 80, gw_tx, p_tx).await;
                subagent.sampling_gate = Some(saturated.clone());
                let subagent = Arc::new(subagent);

                let queued = tokio::time::timeout(
                    Duration::from_millis(150),
                    subagent.submit_turn_request(Default::default()),
                )
                .await;
                assert!(
                    queued.is_err(),
                    "a subagent submit must queue behind the drained gate, never reaching the sampler"
                );

                let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let (p_tx, _p_rx) = tokio::sync::mpsc::unbounded_channel();
                let main = create_test_actor(0, 200_000, 80, gw_tx, p_tx).await;
                assert!(main.sampling_gate.is_none());
                let main = Arc::new(main);

                let ran = tokio::time::timeout(
                    Duration::from_millis(150),
                    main.submit_turn_request(Default::default()),
                )
                .await;
                assert!(
                    ran.is_ok(),
                    "the main session must reach the sampler even while the gate is drained"
                );
                assert!(
                    main.turn_stream_drained.lock().is_empty(),
                    "a result with no queued terminal event must release request ownership before returning"
                );
            })
            .await;
    }
}
