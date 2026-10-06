// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-scoped worker model: capability, `_meta` on session/new, `x.ai/session/worker_model/set`.

use agent_client_protocol as acp;
use distill_sampling_types::{ReasoningEffort, ReasoningEffortOption};

use super::{build_minimal_agent_for_tests, make_test_handle, run_local_for_bridge_test};
use crate::agent::config::{EndpointsConfig, ModelEntry};
use crate::agent::mvp_agent::MvpAgent;
use crate::extensions::session_worker_model::{SET_METHOD, handle, worker_from_meta};
use crate::session::handle::SessionWorker;

const SESSION: &str = "worker-model-sess";

fn effort_option(value: ReasoningEffort) -> ReasoningEffortOption {
    ReasoningEffortOption {
        id: value.to_string(),
        value,
        label: value.to_string(),
        description: None,
        default: false,
    }
}

/// `effort-worker` offers only low/high; `plain-worker` has no effort menu.
fn agent_with_session() -> MvpAgent {
    let agent = build_minimal_agent_for_tests();
    let mut effort_worker = ModelEntry::fallback("effort-worker", &EndpointsConfig::default());
    effort_worker.info.supports_reasoning_effort = true;
    effort_worker.info.reasoning_efforts = vec![
        effort_option(ReasoningEffort::Low),
        effort_option(ReasoningEffort::High),
    ];
    agent.models_manager.insert_test_entry("effort-worker", effort_worker);
    agent.models_manager.insert_test_entry(
        "plain-worker",
        ModelEntry::fallback("plain-worker", &EndpointsConfig::default()),
    );
    let sid = acp::SessionId::new(SESSION);
    let mut session = make_test_handle("test-model", false, None);
    session.info.id = sid.clone();
    agent.insert_resident(&sid, session);
    agent
}

fn set_request(params: serde_json::Value) -> acp::ExtRequest {
    acp::ExtRequest::new(SET_METHOD, serde_json::value::to_raw_value(&params).unwrap().into())
}

async fn set_worker(
    agent: &MvpAgent,
    model_id: serde_json::Value,
    effort: serde_json::Value,
) -> Result<serde_json::Value, acp::Error> {
    let request = set_request(
        serde_json::json!({ "sessionId": SESSION, "modelId": model_id, "effort": effort }),
    );
    let response = handle(agent, &request).await?;
    Ok(serde_json::from_str(response.0.get()).unwrap())
}

fn stored(agent: &MvpAgent) -> Option<SessionWorker> {
    let sid = acp::SessionId::new(SESSION);
    agent.resident_handle(&sid).unwrap().worker_override.read().clone()
}

fn pin_config_worker(model: Option<&str>, effort: Option<ReasoningEffort>) {
    crate::jev::set_test_worker_model(model.map(str::to_owned));
    crate::jev::set_test_worker_effort(effort);
}

fn clear_config_worker() {
    crate::jev::clear_test_worker_model();
    crate::jev::clear_test_worker_effort();
}

fn invalid_params_message(err: &acp::Error) -> String {
    assert_eq!(err.code, acp::Error::invalid_params().code, "{err:?}");
    err.data.as_ref().map(ToString::to_string).unwrap_or_default()
}

/// Clients gate the picker on this flag, so it must ride in `initialize`.
#[test]
fn initialize_advertises_session_worker_model() {
    let capabilities = crate::agent::mvp_agent::acp_agent::x_ai_capabilities();
    assert_eq!(capabilities["sessionWorkerModel"], serde_json::json!(true));
    assert!(capabilities.get("toolOverrides").is_some(), "existing capability stays");
}

/// `session/new|load|resume` share this parser: absent keys inherit the config,
/// `""` disables the worker, and `workerEffort` absent or `auto` is auto.
#[test]
fn meta_worker_choice_parses_inherit_disable_and_effort() {
    let meta = |value: serde_json::Value| value.as_object().cloned();
    assert_eq!(worker_from_meta(None).unwrap(), None);
    assert_eq!(worker_from_meta(meta(serde_json::json!({})).as_ref()).unwrap(), None);
    assert_eq!(
        worker_from_meta(meta(serde_json::json!({ "workerEffort": "high" })).as_ref()).unwrap(),
        None,
        "effort without a model id inherits the config"
    );
    assert_eq!(
        worker_from_meta(meta(serde_json::json!({ "workerModelId": "" })).as_ref()).unwrap(),
        Some(SessionWorker { model_id: None, effort: None }),
    );
    let auto = |meta_value| {
        worker_from_meta(meta(meta_value).as_ref()).unwrap().expect("override").effort
    };
    assert_eq!(auto(serde_json::json!({ "workerModelId": "m" })), None);
    assert_eq!(auto(serde_json::json!({ "workerModelId": "m", "workerEffort": "auto" })), None);
    assert_eq!(
        auto(serde_json::json!({ "workerModelId": " m ", "workerEffort": "High" })),
        Some(ReasoningEffort::High)
    );
    for bad in [
        serde_json::json!({ "workerModelId": 3 }),
        serde_json::json!({ "workerModelId": "m", "workerEffort": 3 }),
        serde_json::json!({ "workerModelId": "m", "workerEffort": "warp9" }),
    ] {
        let err = worker_from_meta(meta(bad).as_ref()).expect_err("malformed _meta");
        assert_eq!(err.code, acp::Error::invalid_params().code);
    }
}

/// A client can pick the worker at `session/new` and the session keeps it in
/// memory: without `_meta` the session inherits the config.
#[test]
fn new_session_meta_sets_the_session_worker_override() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        pin_config_worker(Some("config-worker"), None);
        agent.set_auth_method(acp::AuthMethodId::new("cached_token"));
        let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        );
        agent.initialize_request.set(init).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let create = |meta: Option<serde_json::Value>| {
            let request = acp::NewSessionRequest::new(cwd.path().to_path_buf())
                .meta(meta.and_then(|value| value.as_object().cloned()));
            let agent = &agent;
            async move { Box::pin(agent.new_session_inner(request)).await }
        };

        let inherited = create(None).await.expect("session/new").session_id;
        let handle = agent.resident_handle(&inherited).unwrap();
        assert_eq!(
            *handle.worker_override.read(),
            Some(SessionWorker {
                model_id: Some("config-worker".to_owned()),
                effort: None,
            }),
            "no _meta captures the worker configured when the session starts"
        );

        let chosen = create(Some(serde_json::json!({
            "workerModelId": "session-worker", "workerEffort": "low",
        })))
        .await
        .expect("session/new with worker _meta")
        .session_id;
        let handle = agent.resident_handle(&chosen).unwrap();
        assert_eq!(
            *handle.worker_override.read(),
            Some(SessionWorker {
                model_id: Some("session-worker".to_owned()),
                effort: Some(ReasoningEffort::Low),
            })
        );

        let disabled = create(Some(serde_json::json!({ "workerModelId": "" })))
            .await
            .expect("session/new disabling the worker")
            .session_id;
        let handle = agent.resident_handle(&disabled).unwrap();
        assert_eq!(
            *handle.worker_override.read(),
            Some(SessionWorker { model_id: None, effort: None })
        );

        let err = create(Some(serde_json::json!({ "workerModelId": 7 })))
            .await
            .expect_err("malformed _meta is rejected before a session is created");
        assert_eq!(err.code, acp::Error::invalid_params().code);

        for sid in [inherited, chosen, disabled] {
            agent.remove_session(&sid);
        }
        clear_config_worker();
    });
}

fn system_head(item: &distill_sampling_types::ConversationItem) -> String {
    match item {
        distill_sampling_types::ConversationItem::System(sys) => sys.content.to_string(),
        other => panic!("conversation must start with a System item, got {other:?}"),
    }
}

/// A resumed session keeps the head it was saved with. Loading it must make the section name the
/// worker its children run on now and change nothing else.
#[test]
fn cold_load_updates_a_saved_head_that_names_another_worker() {
    // Attaching a session overflows the default test thread stack.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            run_local_for_bridge_test(|| async {
                let agent = build_minimal_agent_for_tests();
                for worker in ["worker-a", "worker-b"] {
                    agent.models_manager.insert_test_entry(
                        worker,
                        ModelEntry::fallback(worker, &EndpointsConfig::default()),
                    );
                }
                pin_config_worker(None, None);
                agent.set_auth_method(acp::AuthMethodId::new("cached_token"));
                let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                    .client_capabilities(
                        acp::ClientCapabilities::new()
                            .fs(acp::FileSystemCapabilities::new())
                            .terminal(false),
                    );
                agent.initialize_request.set(init).unwrap();
                let cwd = tempfile::tempdir().unwrap();
                let meta = |worker: &str| {
                    serde_json::json!({ "workerModelId": worker })
                        .as_object()
                        .cloned()
                };

                let sid = Box::pin(agent.new_session_inner(
                    acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(meta("worker-a")),
                ))
                .await
                .expect("session/new")
                .session_id;
                let info = agent.resident_handle(&sid).expect("resident session").info;
                let history = std::fs::read_to_string(
                    crate::session::persistence::session_dir(&info).join("chat_history.jsonl"),
                )
                .expect("the new session saved its history");
                let saved = system_head(
                    &serde_json::from_str(history.lines().next().expect("saved head"))
                        .expect("saved head parses"),
                );
                assert!(
                    saved.contains("Worker model: `worker-a`"),
                    "the saved head names worker-a: {saved:.300}"
                );
                agent.remove_session(&sid);

                Box::pin(
                    agent.load_session_inner(
                        acp::LoadSessionRequest::new(sid.clone(), cwd.path().to_path_buf())
                            .meta(meta("worker-b")),
                    ),
                )
                .await
                .expect("cold load");
                let conversation = agent
                    .resident_handle(&sid)
                    .expect("loaded session")
                    .chat_state_handle
                    .get_conversation()
                    .await;
                assert_eq!(
                    system_head(conversation.first().expect("loaded head")),
                    saved.replace("`worker-a`", "`worker-b`"),
                    "the head names the loaded session's worker and nothing else changes"
                );
                agent.remove_session(&sid);
                clear_config_worker();
            })
        })
        .expect("spawn large-stack test thread")
        .join()
        .expect("test thread");
}

#[tokio::test(flavor = "current_thread")]
async fn set_worker_model_unknown_session_is_resource_not_found() {
    let agent = build_minimal_agent_for_tests();
    let request = set_request(
        serde_json::json!({ "sessionId": "nope", "modelId": null, "effort": null }),
    );
    let err = handle(&agent, &request).await.expect_err("unknown session");
    assert_eq!(err.code, acp::Error::resource_not_found(None::<String>).code);
}

/// A bad choice must say why and leave the previous choice intact.
#[tokio::test(flavor = "current_thread")]
async fn set_worker_model_rejects_unknown_models_and_unsupported_efforts() {
    let agent = agent_with_session();
    pin_config_worker(None, None);
    set_worker(&agent, "effort-worker".into(), "high".into()).await.expect("valid choice");
    let before = stored(&agent);

    let err = set_worker(&agent, "no-such-model".into(), serde_json::Value::Null)
        .await
        .expect_err("unknown model");
    assert!(invalid_params_message(&err).contains("no-such-model"));

    let err = set_worker(&agent, "effort-worker".into(), "medium".into())
        .await
        .expect_err("effort outside the model's menu");
    assert!(invalid_params_message(&err).contains("medium"));

    let err = set_worker(&agent, "plain-worker".into(), "high".into())
        .await
        .expect_err("model without an effort menu");
    assert!(invalid_params_message(&err).contains("plain-worker"));

    let err = set_worker(&agent, "effort-worker".into(), "warp9".into())
        .await
        .expect_err("unparseable effort");
    invalid_params_message(&err);

    assert_eq!(stored(&agent), before, "a rejected request changes nothing");
    clear_config_worker();
}

/// The prompt must name the worker this session's children run on: the session's
/// choice beats `[models] worker`, and the session's own main model is no worker,
/// whether it is named by catalog key or routing slug.
#[test]
fn orchestration_worker_follows_the_session_not_the_config() {
    use crate::extensions::session_worker_model::orchestration_worker;
    use distill_agent::prompt::context::PromptAudience;
    let models = crate::agent::remote_config::ModelsManager::default();
    for (key, routing) in [
        ("session-worker", "session-worker"),
        ("config-worker", "config-worker"),
        ("main-key", "main/routing-slug"),
    ] {
        models.insert_test_entry(
            key,
            ModelEntry::fallback(routing, &EndpointsConfig::default()),
        );
    }
    pin_config_worker(Some("config-worker"), None);
    let decide = |worker: Option<SessionWorker>, session_model: &str| {
        let state = crate::session::handle::new_session_worker_state();
        *state.write() = worker;
        orchestration_worker(
            &state,
            &models,
            session_model,
            PromptAudience::Primary,
            true,
        )
    };
    let session = |model_id: Option<&str>| {
        Some(SessionWorker {
            model_id: model_id.map(str::to_owned),
            effort: None,
        })
    };

    assert_eq!(
        decide(session(Some("session-worker")), "main-key").as_deref(),
        Some("session-worker"),
        "the session's choice beats the configured worker"
    );
    assert_eq!(
        decide(session(None), "main-key"),
        None,
        "a disabled worker names none"
    );
    assert_eq!(
        decide(None, "main-key").as_deref(),
        Some("config-worker"),
        "a session that never chose falls back to the config"
    );
    assert_eq!(
        decide(session(Some("main-key")), "main/routing-slug"),
        None,
        "the session's own main model, by slug, is not a worker"
    );
    let state = crate::session::handle::new_session_worker_state();
    *state.write() = session(Some("session-worker"));
    assert_eq!(
        orchestration_worker(&state, &models, "main-key", PromptAudience::Subagent, true),
        None,
        "a child does the delegated work"
    );
    clear_config_worker();
}

/// Set, disable and clear in turn: the response reports the effective worker and
/// where it came from, and the config is only consulted once the override is gone.
#[tokio::test(flavor = "current_thread")]
async fn set_worker_model_sets_disables_and_restores_the_config() {
    let agent = agent_with_session();
    pin_config_worker(Some("config-worker"), Some(ReasoningEffort::Medium));

    let set = set_worker(&agent, "effort-worker".into(), "low".into()).await.unwrap();
    assert_eq!(
        set,
        serde_json::json!({ "modelId": "effort-worker", "effort": "low", "source": "session" }),
        "the session choice beats the configured worker and effort"
    );
    let auto = set_worker(&agent, "effort-worker".into(), "auto".into()).await.unwrap();
    assert_eq!(auto["effort"], "auto", "auto beats the configured level");
    let auto = set_worker(&agent, "plain-worker".into(), serde_json::Value::Null).await.unwrap();
    assert_eq!(
        auto,
        serde_json::json!({ "modelId": "plain-worker", "effort": "auto", "source": "session" })
    );

    let disabled = set_worker(&agent, "".into(), "high".into()).await.unwrap();
    assert_eq!(
        disabled,
        serde_json::json!({ "modelId": null, "effort": "auto", "source": "session" }),
        "an empty model id means the main model does all the work"
    );
    assert_eq!(stored(&agent), Some(SessionWorker { model_id: None, effort: None }));

    let restored = set_worker(&agent, serde_json::Value::Null, "high".into()).await.unwrap();
    assert_eq!(
        restored,
        serde_json::json!({ "modelId": "config-worker", "effort": "medium", "source": "config" }),
    );
    assert_eq!(stored(&agent), None);
    clear_config_worker();
}
