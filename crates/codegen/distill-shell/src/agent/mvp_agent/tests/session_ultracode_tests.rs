// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/session/ultracode/set`: capability, toggle/explicit set, effort untouched.
use agent_client_protocol as acp;
use distill_tool_types::ExecutionMode;

use super::{build_minimal_agent_for_tests, make_test_handle, run_local_for_bridge_test};
use crate::agent::mvp_agent::MvpAgent;
use crate::extensions::session_ultracode::{SET_METHOD, handle};

const SESSION: &str = "ultracode-sess";

// Re-exec beats the cached Distill home and keeps synthetic state out of the
// user's store, using the existing isolated-process test pattern.
fn run_in_isolated_home(test_name: &str) -> bool {
    const CHILD: &str = "DISTILL_ULTRACODE_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test_name) {
        return false;
    }
    let home = tempfile::tempdir().unwrap();
    let filter = module_path!().split_once("::").unwrap().1;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{filter}::{test_name}"),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, test_name)
        .env("DISTILL_HOME", home.path())
        .env("GROK_HOME", home.path())
        .output()
        .expect("isolated UltraCode test process");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child filter must run one test"
    );
    true
}

fn agent_with_session() -> MvpAgent {
    let agent = build_minimal_agent_for_tests();
    let sid = acp::SessionId::new(SESSION);
    let mut session = make_test_handle("test-model", false, None);
    session.info.id = sid.clone();
    session.reasoning_effort = Some(distill_sampling_types::ReasoningEffort::Low);
    agent.insert_resident(&sid, session);
    agent
}

async fn set(agent: &MvpAgent, params: serde_json::Value) -> Result<serde_json::Value, acp::Error> {
    let request =
        acp::ExtRequest::new(SET_METHOD, serde_json::value::to_raw_value(&params).unwrap().into());
    let response = handle(agent, &request).await?;
    Ok(serde_json::from_str(response.0.get()).unwrap())
}

fn flag(agent: &MvpAgent) -> bool {
    let sid = acp::SessionId::new(SESSION);
    agent.resident_handle(&sid).unwrap().execution_mode.load() == ExecutionMode::Ultracode
}

#[test]
fn initialize_advertises_session_ultracode() {
    let capabilities = crate::agent::mvp_agent::acp_agent::x_ai_capabilities();
    assert_eq!(capabilities["sessionUltracode"], serde_json::json!(true));
}

/// Omitted `enabled` toggles; explicit values set. The flag is the only thing that changes, never effort.
#[test]
fn ultracode_set_toggles_and_sets_without_touching_effort() {
    if run_in_isolated_home("ultracode_set_toggles_and_sets_without_touching_effort") {
        return;
    }
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        let sid = acp::SessionId::new(SESSION);
        assert!(!flag(&agent));
        let info = agent.resident_handle(&sid).unwrap().info;
        assert!(crate::extensions::session_ultracode::load_mode(&info).unwrap().1 == ExecutionMode::Normal);

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION })).await.unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": true }));
        assert!(flag(&agent));
        assert!(crate::extensions::session_ultracode::load_mode(&info).unwrap().1 == ExecutionMode::Ultracode);

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION, "enabled": true }))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": true }), "explicit on is idempotent");

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION })).await.unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": false }));
        assert!(!flag(&agent));

        let out = set(&agent, serde_json::json!({ "sessionId": SESSION, "enabled": false }))
            .await
            .unwrap();
        assert_eq!(out, serde_json::json!({ "enabled": false }));

        assert!(
            crate::extensions::session_ultracode::load_mode(&info).unwrap().1 == ExecutionMode::Normal,
            "explicit off must survive reload"
        );
        let handle = agent.resident_handle(&sid).unwrap();
        assert_eq!(
            handle.reasoning_effort,
            Some(distill_sampling_types::ReasoningEffort::Low),
            "toggling Ultracode must not change effort"
        );
    });
}

#[test]
fn ultracode_set_unknown_session_is_not_found() {
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        let err = set(&agent, serde_json::json!({ "sessionId": "nope", "enabled": true }))
            .await
            .unwrap_err();
        assert_eq!(err.code, acp::Error::resource_not_found(None).code);
    });
}

#[test]
fn ultracode_child_toggle_cannot_change_or_persist_the_root_switch() {
    if run_in_isolated_home("ultracode_child_toggle_cannot_change_or_persist_the_root_switch") {
        return;
    }
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        set(
            &agent,
            serde_json::json!({ "sessionId": SESSION, "enabled": true }),
        )
        .await
        .unwrap();
        let root = agent
            .resident_handle(&acp::SessionId::new(SESSION))
            .unwrap();
        let mut child = make_test_handle("test-model", false, None);
        child.info.id = acp::SessionId::new("ultracode-child");
        child.tool_context.subagent_depth = 1;
        child.execution_mode = root.execution_mode.clone();
        let child_info = child.info.clone();
        agent.insert_resident(&child.info.id.clone(), child);
        let error = set(
            &agent,
            serde_json::json!({ "sessionId": "ultracode-child", "enabled": false }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, acp::Error::invalid_params().code);
        assert!(flag(&agent));
        assert!(crate::extensions::session_ultracode::load_mode(&root.info).unwrap().1 == ExecutionMode::Ultracode);
        assert!(
            !crate::session::persistence::session_dir(&child_info)
                .join("execution_mode.json")
                .exists()
        );
        set(
            &agent,
            serde_json::json!({ "sessionId": SESSION, "enabled": false }),
        )
        .await
        .unwrap();
        assert!(root.execution_mode.load() == ExecutionMode::Normal);
        assert!(crate::extensions::session_ultracode::load_mode(&root.info).unwrap().1 == ExecutionMode::Normal);
    });
}

#[test]
fn execution_modes_replace_each_other_and_persist_without_changing_pins() {
    if run_in_isolated_home("execution_modes_replace_each_other_and_persist_without_changing_pins") {
        return;
    }
    run_local_for_bridge_test(|| async {
        let agent = agent_with_session();
        let root = agent.resident_handle(&acp::SessionId::new(SESSION)).unwrap();
        // Existing saved UltraCode sessions still restore correctly.
        let dir = crate::session::persistence::ensure_owner_only_session_dir(&root.info).unwrap();
        std::fs::write(dir.join("ultracode.json"), r#"{"enabled":true}"#).unwrap();
        assert_eq!(crate::extensions::session_ultracode::load_mode(&root.info).unwrap().1, ExecutionMode::Ultracode);
        for mode in [ExecutionMode::Ultrafast, ExecutionMode::Ultracode, ExecutionMode::Ultrasave, ExecutionMode::Normal, ExecutionMode::Ultrafast] {
            let params = serde_json::json!({"sessionId": SESSION, "mode": mode});
            let request = acp::ExtRequest::new(
                crate::extensions::session_ultracode::MODE_SET_METHOD,
                serde_json::value::to_raw_value(&params).unwrap().into(),
            );
            let response = handle(&agent, &request).await.unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(response.0.get()).unwrap(), serde_json::json!({"mode": mode, "choice": mode}));
            assert_eq!(root.execution_mode.load(), mode);
            assert_eq!(crate::extensions::session_ultracode::load_mode(&root.info).unwrap().1, mode);
        }
        // Disabling an inactive legacy mode must not disable UltraFast.
        set(&agent, serde_json::json!({"sessionId": SESSION, "enabled": false})).await.unwrap();
        assert_eq!(root.execution_mode.load(), ExecutionMode::Ultrafast);
        // Relative commands use server state even when a client has not yet
        // received the preceding acknowledgement.
        // Turning a fixed mode off returns to Auto, which keeps running the
        // last effective mode until its next decision.
        use distill_tool_types::ExecutionModeChoice as Choice;
        for (target, operation, expected) in [
            (Choice::Ultrafast, "toggle", Choice::Auto),
            (Choice::Ultrafast, "toggle", Choice::Ultrafast),
            (Choice::Ultracode, "disable", Choice::Ultrafast),
            (Choice::Ultrafast, "disable", Choice::Auto),
            (Choice::Ultrasave, "toggle", Choice::Ultrasave),
            (Choice::Ultrafast, "disable", Choice::Ultrasave),
            (Choice::Ultrasave, "toggle", Choice::Auto),
            (Choice::Normal, "set", Choice::Normal),
            (Choice::Auto, "set", Choice::Auto),
        ] {
            let params = serde_json::json!({"sessionId": SESSION, "mode": target, "operation": operation});
            let request = acp::ExtRequest::new(
                crate::extensions::session_ultracode::MODE_SET_METHOD,
                serde_json::value::to_raw_value(&params).unwrap().into(),
            );
            let response = handle(&agent, &request).await.unwrap();
            let response: serde_json::Value = serde_json::from_str(response.0.get()).unwrap();
            assert_eq!(response["choice"], serde_json::json!(expected));
            assert_eq!(root.execution_mode.choice(), expected);
            assert_eq!(crate::extensions::session_ultracode::load_mode(&root.info).unwrap().0, expected);
            if let Some(fixed) = expected.fixed() {
                assert_eq!(root.execution_mode.load(), fixed);
            }
        }
        assert_eq!(root.reasoning_effort, Some(distill_sampling_types::ReasoningEffort::Low));
    });
}

mod pinned_mode_model {
    //! `[modes.ultrasave]` / `[modes.ultrafast]`: the model and serving path of a session pinned to a mode.
    use agent_client_protocol as acp;
    use distill_sampler::ProviderTier;
    use distill_tool_types::ExecutionMode;

    use super::super::{build_minimal_agent_for_tests, run_local_for_bridge_test};
    use crate::agent::account_routing::{self, tests::NamedAccounts};
    use crate::agent::config::pinned_mode_settings;
    use crate::agent::mvp_agent::MvpAgent;
    use crate::extensions::session_ultracode::{
        MODE_SET_METHOD, handle_with_config, pinned_provider_tier,
    };

    const THIRD: &str = "placeholder-model-c";

    fn isolated(test: &str, body: impl FnOnce()) {
        account_routing::tests::isolated(
            &format!("agent::mvp_agent::tests::session_ultracode_tests::pinned_mode_model::{test}"),
            body,
        );
    }

    /// Opens one session on `model` (`auto` for account Auto) over the fixture's accounts. The
    /// agent stays where the caller made it: its session supervisor keeps the agent's address.
    async fn session_on(
        agent: &MvpAgent,
        fixture: &NamedAccounts,
        model: &str,
    ) -> (acp::SessionId, tempfile::TempDir) {
        for (id, entry) in &fixture.models {
            agent.models_manager.insert_test_entry(id, entry.clone());
        }
        let mut third = fixture.models["gpt-6-luna"].clone();
        third.info.model = THIRD.into();
        agent.models_manager.insert_test_entry(THIRD, third);
        agent
            .initialize_request
            .set(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
                    acp::ClientCapabilities::new()
                        .fs(acp::FileSystemCapabilities::new())
                        .terminal(false),
                ),
            )
            .unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let sid = agent
            .new_session_inner(
                acp::NewSessionRequest::new(cwd.path().to_path_buf())
                    .meta(serde_json::json!({ "modelId": model }).as_object().cloned()),
            )
            .await
            .expect("session admission")
            .session_id;
        (sid, cwd)
    }

    async fn set_mode(agent: &MvpAgent, sid: &acp::SessionId, mode: &str, config: &str) {
        let root: toml::Value = toml::from_str(config).unwrap();
        let params = serde_json::json!({ "sessionId": sid.0.as_ref(), "mode": mode });
        let request = acp::ExtRequest::new(
            MODE_SET_METHOD,
            serde_json::value::to_raw_value(&params).unwrap().into(),
        );
        Box::pin(handle_with_config(agent, &request, Some(&root)))
            .await
            .expect("mode set");
    }

    /// The model every main request and every compaction of the session reads.
    async fn session_model(agent: &MvpAgent, sid: &acp::SessionId) -> String {
        let handle = agent.resident_handle(sid).unwrap();
        let stored = handle
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap();
        assert_eq!(
            handle.model_id.0.as_ref(),
            stored.model,
            "ACP and request models agree"
        );
        stored.model
    }

    fn state_file(agent: &MvpAgent, sid: &acp::SessionId) -> std::path::PathBuf {
        crate::session::persistence::session_dir(&agent.resident_handle(sid).unwrap().info)
            .join("pinned_mode.json")
    }

    /// The saving is a cheaper model doing the work, so UltraSave leads on the model its own
    /// key names. A configured worker alone must not move a pinned session (the key is the
    /// opt-in), and the utility model is a decision helper, never the lead.
    #[test]
    fn ultrasave_leads_only_on_the_model_its_key_names_never_the_utility_model() {
        let settings =
            |config: &str, mode| pinned_mode_settings(Some(&toml::from_str(config).unwrap()), mode);
        let no_cheaper = |config: &str| {
            crate::agent::config::ultrasave_has_no_cheaper_model(Some(
                &toml::from_str(config).unwrap(),
            ))
        };
        let both =
            "[models]\nworker = \"worker-model\"\n[modes.ultrasave]\nmodel = \"save-model\"\n";
        assert_eq!(
            settings(both, ExecutionMode::Ultrasave).model.as_deref(),
            Some("save-model")
        );
        let worker = "[models]\nworker = \"worker-model\"\n";
        assert_eq!(settings(worker, ExecutionMode::Ultrasave).model, None);
        assert!(no_cheaper(worker));
        let named_worker =
            "[models]\nworker = \"worker-model\"\n[modes.ultrasave]\nmodel = \"worker\"\n";
        assert_eq!(
            settings(named_worker, ExecutionMode::Ultrasave)
                .model
                .as_deref(),
            Some("worker-model")
        );
        assert!(!no_cheaper(named_worker));
        let worker_missing = "[modes.ultrasave]\nmodel = \"worker\"\n";
        assert_eq!(settings(worker_missing, ExecutionMode::Ultrasave).model, None);
        assert!(no_cheaper(worker_missing));
        let utility_only = "[jev.local]\nmodel = \"utility-model\"\n[models]\nworker = \" \"\n";
        assert_eq!(settings(utility_only, ExecutionMode::Ultrasave).model, None);
        assert!(no_cheaper(utility_only));
        assert!(crate::agent::config::ultrasave_has_no_cheaper_model(None));
        // UltraFast is the same model served faster: the worker never leads it.
        assert_eq!(settings(worker, ExecutionMode::Ultrafast).model, None);
        let fast = "[modes.ultrafast]\nmodel = \"fast-model\"\nprovider_tier = \"fast\"\n";
        assert_eq!(
            settings(fast, ExecutionMode::Ultrafast),
            crate::agent::config::PinnedModeSettings {
                model: Some("fast-model".into()),
                provider_tier: Some(ProviderTier::Fast),
            }
        );
        let tiers = "[modes.ultrasave]\nprovider_tier = \"Economy\"\n[modes.ultrafast]\nprovider_tier = \"turbo\"\n";
        assert_eq!(
            settings(tiers, ExecutionMode::Ultrasave).provider_tier,
            Some(ProviderTier::Economy)
        );
        assert_eq!(
            settings(tiers, ExecutionMode::Ultrafast).provider_tier,
            None,
            "unknown tier"
        );
        for mode in [ExecutionMode::Normal, ExecutionMode::Ultracode] {
            assert_eq!(settings(both, mode), Default::default());
        }
    }

    /// The owner's profile: account Auto routing on. A pinned UltraSave session runs its main
    /// requests on the mode's model as a session-only pick, and the shared Auto profile on disk
    /// stays on; leaving the mode hands the session back to Auto.
    #[test]
    #[serial_test::serial]
    fn pinned_ultrasave_runs_on_its_model_and_leaves_auto_routing_on_disk() {
        isolated(
            "pinned_ultrasave_runs_on_its_model_and_leaves_auto_routing_on_disk",
            || {
                run_local_for_bridge_test(|| async {
                    let fixture = NamedAccounts::new("http://127.0.0.1:1/v1");
                    fixture.routing(crate::accounts::AutoStrategy::HighestRemaining, true);
                    fixture.quota("three", Some(80.0));
                    let agent = build_minimal_agent_for_tests();
                    let (sid, _cwd) = session_on(&agent, &fixture, "auto").await;
                    let models = agent.models_manager.models();
                    let next_turn = async || {
                        let handle = agent.resident_handle(&sid).unwrap();
                        let stored = handle
                            .chat_state_handle
                            .get_sampling_config()
                            .await
                            .unwrap();
                        let turn = distill_sampler::SamplerConfig {
                            api_key: handle.chat_state_handle.get_credentials().await.api_key,
                            base_url: stored.base_url.clone(),
                            model: stored.model.clone(),
                            api_backend: stored.api_backend.clone(),
                            extra_headers: stored.extra_headers.clone(),
                            ..Default::default()
                        };
                        let routes =
                            account_routing::routes_for_turn(&fixture.store, &models, &turn, true)
                                .await
                                .unwrap();
                        (stored, routes)
                    };
                    let auto_model = session_model(&agent, &sid).await;
                    assert_ne!(
                        auto_model, "gpt-6.1-sol",
                        "Auto's pick differs from the mode's model"
                    );

                    let config = "[modes.ultrasave]\nmodel = \"gpt-6.1-sol\"\n";
                    set_mode(&agent, &sid, "ultrasave", config).await;
                    let (pinned, routes) = next_turn().await;
                    assert_eq!(pinned.model, "gpt-6.1-sol");
                    assert!(
                        pinned
                            .extra_headers
                            .contains_key(account_routing::PIN_HEADER)
                    );
                    assert!(
                        !routes.is_empty()
                            && routes.iter().all(|route| {
                                route.option_id.is_none() && route.main_model == "gpt-6.1-sol"
                            }),
                        "every main request runs on the mode's model, not an Auto option: {routes:?}"
                    );
                    assert!(
                        fixture.store.load_routing().unwrap().enabled,
                        "routing.enabled unchanged on disk"
                    );

                    set_mode(&agent, &sid, "auto", config).await;
                    let (left, routes) = next_turn().await;
                    assert!(!left.extra_headers.contains_key(account_routing::PIN_HEADER));
                    assert!(
                        routes.iter().any(|route| route.option_id.is_some()),
                        "back on Auto: {routes:?}"
                    );
                    assert_eq!(left.model, auto_model);
                    assert!(fixture.store.load_routing().unwrap().enabled);
                    assert!(!state_file(&agent, &sid).exists());
                    agent.remove_session(&sid);
                });
            },
        );
    }

    /// Under Auto the effective mode may be UltraSave, but model and serving path are the
    /// user's pinned choices only: an Auto pick never moves the session off its model.
    #[test]
    #[serial_test::serial]
    fn auto_picking_ultrasave_changes_neither_model_nor_serving_path() {
        isolated(
            "auto_picking_ultrasave_changes_neither_model_nor_serving_path",
            || {
                run_local_for_bridge_test(|| async {
                    let fixture = NamedAccounts::new("http://127.0.0.1:1/v1");
                    fixture.routing(crate::accounts::AutoStrategy::OrderedFirst, false);
                    let agent = build_minimal_agent_for_tests();
                    let (sid, _cwd) = session_on(&agent, &fixture, "gpt-6.1-sol").await;
                    let config = "[models]\nworker = \"gpt-6-luna\"\n[modes.ultrasave]\nmodel = \"gpt-6-luna\"\nprovider_tier = \"economy\"\n";
                    let handle = agent.resident_handle(&sid).unwrap();
                    assert!(
                        handle.execution_mode.is_auto(),
                        "a new session starts in Auto"
                    );
                    assert!(handle.execution_mode.pick_auto(ExecutionMode::Ultrasave));
                    set_mode(&agent, &sid, "auto", config).await;
                    assert_eq!(handle.execution_mode.load(), ExecutionMode::Ultrasave);
                    assert_eq!(session_model(&agent, &sid).await, "gpt-6.1-sol");
                    assert_eq!(pinned_provider_tier(&handle.info), None);
                    assert!(!state_file(&agent, &sid).exists());
                    agent.remove_session(&sid);
                });
            },
        );
    }

    /// Leaving the mode gives back the model it replaced, but a model the user picked while
    /// pinned is their choice and stays. `model = "worker"` leads on the configured worker.
    #[test]
    #[serial_test::serial]
    fn leaving_the_mode_restores_the_model_unless_the_user_picked_another() {
        isolated(
            "leaving_the_mode_restores_the_model_unless_the_user_picked_another",
            || {
                run_local_for_bridge_test(|| async {
                    let fixture = NamedAccounts::new("http://127.0.0.1:1/v1");
                    fixture.routing(crate::accounts::AutoStrategy::OrderedFirst, false);
                    let agent = build_minimal_agent_for_tests();
                    let (sid, _cwd) = session_on(&agent, &fixture, "gpt-6.1-sol").await;
                    let config = "[modes.ultrasave]\nmodel = \"gpt-6-luna\"\n";
                    set_mode(&agent, &sid, "ultrasave", config).await;
                    assert_eq!(session_model(&agent, &sid).await, "gpt-6-luna");
                    set_mode(&agent, &sid, "normal", config).await;
                    assert_eq!(
                        session_model(&agent, &sid).await,
                        "gpt-6.1-sol",
                        "restored on leave"
                    );

                    set_mode(&agent, &sid, "ultrasave", config).await;
                    assert_eq!(session_model(&agent, &sid).await, "gpt-6-luna");
                    Box::pin(agent.set_model_gated(acp::SetSessionModelRequest::new(
                        sid.clone(),
                        acp::ModelId::new(THIRD),
                    )))
                    .await
                    .expect("the user's own pick while pinned");
                    set_mode(&agent, &sid, "auto", config).await;
                    assert_eq!(
                        session_model(&agent, &sid).await,
                        THIRD,
                        "the user's pick stays"
                    );

                    let worker =
                        "[models]\nworker = \"gpt-6-luna\"\n[modes.ultrasave]\nmodel = \"worker\"\n";
                    set_mode(&agent, &sid, "ultrasave", worker).await;
                    assert_eq!(
                        session_model(&agent, &sid).await,
                        "gpt-6-luna",
                        "the named worker"
                    );
                    // Switching to another pinned mode leaves this one first.
                    set_mode(&agent, &sid, "ultrafast", worker).await;
                    assert_eq!(session_model(&agent, &sid).await, THIRD);
                    agent.remove_session(&sid);
                });
            },
        );
    }

    /// With none of the keys set, pinning any mode changes neither the model, the request
    /// config nor the serving path: today's behaviour.
    #[test]
    #[serial_test::serial]
    fn without_mode_keys_every_model_choice_and_request_is_unchanged() {
        isolated(
            "without_mode_keys_every_model_choice_and_request_is_unchanged",
            || {
                run_local_for_bridge_test(|| async {
                    let fixture = NamedAccounts::new("http://127.0.0.1:1/v1");
                    fixture.routing(crate::accounts::AutoStrategy::OrderedFirst, false);
                    let agent = build_minimal_agent_for_tests();
                    let (sid, _cwd) = session_on(&agent, &fixture, "gpt-6.1-sol").await;
                    let handle = agent.resident_handle(&sid).unwrap();
                    let before = handle
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .unwrap();
                    let config = "[jev.local]\nmodel = \"utility-model\"\n";
                    for mode in [
                        "ultrasave",
                        "ultrafast",
                        "ultracode",
                        "normal",
                        "ultrasave",
                        "auto",
                    ] {
                        set_mode(&agent, &sid, mode, config).await;
                        let after = handle
                            .chat_state_handle
                            .get_sampling_config()
                            .await
                            .unwrap();
                        assert_eq!(after.model, before.model, "{mode}");
                        assert_eq!(after.extra_headers, before.extra_headers, "{mode}");
                        assert_eq!(after.reasoning_effort, before.reasoning_effort, "{mode}");
                        assert_eq!(pinned_provider_tier(&handle.info), None, "{mode}");
                        assert!(!state_file(&agent, &sid).exists(), "{mode}");
                    }
                    agent.remove_session(&sid);
                });
            },
        );
    }

    /// A tier change mid-session would invalidate the provider prompt cache, so the tier is
    /// decided when the session is pinned and only leaving the mode clears it.
    #[test]
    #[serial_test::serial]
    fn the_serving_path_is_decided_once_per_pinned_session() {
        isolated(
            "the_serving_path_is_decided_once_per_pinned_session",
            || {
                run_local_for_bridge_test(|| async {
                    let fixture = NamedAccounts::new("http://127.0.0.1:1/v1");
                    fixture.routing(crate::accounts::AutoStrategy::OrderedFirst, false);
                    let agent = build_minimal_agent_for_tests();
                    let (sid, _cwd) = session_on(&agent, &fixture, "gpt-6.1-sol").await;
                    let info = agent.resident_handle(&sid).unwrap().info;
                    let fast = "[modes.ultrafast]\nprovider_tier = \"fast\"\n";
                    set_mode(&agent, &sid, "ultrafast", fast).await;
                    assert_eq!(pinned_provider_tier(&info), Some(ProviderTier::Fast));
                    assert_eq!(
                        session_model(&agent, &sid).await,
                        "gpt-6.1-sol",
                        "same model"
                    );
                    let economy = "[modes.ultrafast]\nprovider_tier = \"economy\"\n";
                    set_mode(&agent, &sid, "ultrafast", economy).await;
                    assert_eq!(pinned_provider_tier(&info), Some(ProviderTier::Fast));
                    set_mode(&agent, &sid, "normal", fast).await;
                    assert_eq!(pinned_provider_tier(&info), None);
                    agent.remove_session(&sid);
                });
            },
        );
    }
}
