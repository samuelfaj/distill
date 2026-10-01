// Modified for Distill by Samuel Fajreldines, 2026.
//! Main model, main effort and auto-effort are per session: one ACP session's choice never becomes
//! another session's (or a future session's) default.

use agent_client_protocol as acp;
use distill_sampling_types::{
    REASONING_EFFORT_AUTO_META_KEY, REASONING_EFFORT_META_KEY, ReasoningEffort,
};

use super::{build_minimal_agent_for_tests, make_live_session_handle, run_local_for_bridge_test};
use crate::agent::config::{EndpointsConfig, ModelEntry};
use crate::agent::mvp_agent::MvpAgent;
use crate::agent::mvp_agent::reasoning_effort::SessionMainMeta;

fn with_effort_models(agent: &MvpAgent, ids: &[&str]) {
    for id in ids {
        let mut entry = ModelEntry::fallback(id, &EndpointsConfig::default());
        entry.info.supports_reasoning_effort = true;
        entry.info.reasoning_effort = Some(ReasoningEffort::Medium);
        agent.models_manager.insert_test_entry(*id, entry);
    }
}

/// A resident session whose fake actor accepts every model switch.
fn add_session(agent: &MvpAgent, sid: &str, model: &str) -> acp::SessionId {
    let sid = acp::SessionId::new(sid);
    let (mut handle, _cmd_tx, mut cmd_rx) = make_live_session_handle(&sid, None);
    handle.model_id = acp::ModelId::new(model);
    handle
        .jev_effort_auto
        .store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let crate::session::SessionCommand::SetSessionModel {
                switch,
                responds_to,
            } = cmd
            {
                let _ = responds_to.send(Ok(acp::ModelId::new(switch.sampling_config.model)));
            }
        }
    });
    agent.insert_resident(&sid, handle);
    sid
}

fn meta(pairs: &[(&str, serde_json::Value)]) -> Option<acp::Meta> {
    Some(
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    )
}

async fn set_model(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    model: &str,
    meta: Option<acp::Meta>,
) -> serde_json::Map<String, serde_json::Value> {
    let request =
        acp::SetSessionModelRequest::new(sid.clone(), acp::ModelId::new(model)).meta(meta);
    agent
        .set_model_gated(request)
        .await
        .expect("set_model")
        .meta
        .expect("response _meta")
}

fn session_state(
    agent: &MvpAgent,
    sid: &acp::SessionId,
) -> (String, Option<ReasoningEffort>, bool) {
    let handle = agent.resident_handle(sid).expect("resident");
    (
        handle.model_id.0.to_string(),
        handle.reasoning_effort,
        handle
            .jev_effort_auto
            .load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// What a session created now without `_meta` would start from.
fn defaults_for_a_new_session(agent: &MvpAgent) -> (String, Option<ReasoningEffort>, bool) {
    (
        agent.models_manager.current_model_id().0.to_string(),
        agent.models_manager.current_reasoning_effort(),
        SessionMainMeta::default().effort_auto(agent.models_manager.current_effort_auto()),
    )
}

#[test]
fn initialize_advertises_session_main_model() {
    let capabilities = crate::agent::mvp_agent::acp_agent::x_ai_capabilities();
    assert_eq!(capabilities["sessionMainModel"], serde_json::json!(true));
    assert_eq!(capabilities["sessionWorkerModel"], serde_json::json!(true));
}

/// An explicit level must beat auto (same rule as `session/set_model`); otherwise the client's
/// flag, and only then the configured default.
#[test]
fn explicit_effort_beats_auto_and_absent_meta_inherits_the_default() {
    let parse =
        |pairs: &[(&str, serde_json::Value)]| SessionMainMeta::from_meta(meta(pairs).as_ref());
    let level = serde_json::json!("low");
    let on = serde_json::json!(true);
    assert!(
        !parse(&[
            (REASONING_EFFORT_META_KEY, level.clone()),
            (REASONING_EFFORT_AUTO_META_KEY, on.clone())
        ])
        .effort_auto(true)
    );
    assert!(parse(&[(REASONING_EFFORT_AUTO_META_KEY, on)]).effort_auto(false));
    assert!(
        !parse(&[(REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(false))]).effort_auto(true)
    );
    assert!(
        parse(&[]).effort_auto(true),
        "no meta keeps the configured default"
    );
    assert!(!parse(&[]).effort_auto(false));
    let main = parse(&[
        ("modelId", serde_json::json!(" m ")),
        (REASONING_EFFORT_META_KEY, level),
    ]);
    assert_eq!(main.model_id.as_deref(), Some("m"));
    assert_eq!(main.effort, Some(ReasoningEffort::Low));
    assert_eq!(parse(&[("modelId", serde_json::json!(""))]).model_id, None);
}

/// Two sessions in one agent process: A's model, effort and auto flag change nothing on B, and a
/// session created afterwards without `_meta` starts from the configured defaults, not from A.
#[tokio::test(flavor = "current_thread")]
async fn set_model_on_one_session_never_leaks_to_another_or_to_future_sessions() {
    let agent = build_minimal_agent_for_tests();
    with_effort_models(&agent, &["model-a", "model-b", "model-x"]);
    let a = add_session(&agent, "sess-a", "model-a");
    let b = add_session(&agent, "sess-b", "model-b");
    let defaults = defaults_for_a_new_session(&agent);
    let b_before = session_state(&agent, &b);

    set_model(
        &agent,
        &a,
        "model-x",
        meta(&[(REASONING_EFFORT_META_KEY, serde_json::json!("high"))]),
    )
    .await;
    assert_eq!(
        session_state(&agent, &a),
        ("model-x".into(), Some(ReasoningEffort::High), false)
    );
    set_model(
        &agent,
        &a,
        "model-x",
        meta(&[(REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(true))]),
    )
    .await;
    assert!(session_state(&agent, &a).2, "auto turned on for A");

    assert_eq!(session_state(&agent, &b), b_before, "B is untouched");
    assert_eq!(
        defaults_for_a_new_session(&agent),
        defaults,
        "a future session C inherits config, not A"
    );
    let c = SessionMainMeta::default();
    assert!(c.effort_auto(agent.models_manager.current_effort_auto()));

    // B can change on its own afterwards without touching A either.
    set_model(
        &agent,
        &b,
        "model-a",
        meta(&[(REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(false))]),
    )
    .await;
    assert!(!session_state(&agent, &b).2);
    assert!(session_state(&agent, &a).2, "A keeps auto");
    assert_eq!(defaults_for_a_new_session(&agent), defaults);
}

/// The pager's footer and `/new` default follow the process-wide values, so its requests still update them.
#[tokio::test(flavor = "current_thread")]
async fn the_first_party_pager_keeps_updating_process_defaults() {
    let agent = build_minimal_agent_for_tests();
    with_effort_models(&agent, &["model-a", "model-x"]);
    let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .meta(meta(&[("clientType", serde_json::json!("grok-pager"))]));
    agent.initialize_request.set(init).unwrap();
    let a = add_session(&agent, "sess-a", "model-a");
    set_model(
        &agent,
        &a,
        "model-x",
        meta(&[(REASONING_EFFORT_META_KEY, serde_json::json!("high"))]),
    )
    .await;
    assert_eq!(
        agent.models_manager.current_model_id().0.as_ref(),
        "model-x"
    );
    assert_eq!(
        agent.models_manager.current_reasoning_effort(),
        Some(ReasoningEffort::High)
    );
    assert!(!agent.models_manager.current_effort_auto());
}

/// The response acknowledges what was applied, next to the existing `model` and `contextWindow`.
#[tokio::test(flavor = "current_thread")]
async fn set_model_response_acknowledges_model_effort_and_auto() {
    let agent = build_minimal_agent_for_tests();
    with_effort_models(&agent, &["model-a", "model-x"]);
    let a = add_session(&agent, "sess-a", "model-a");

    let ack = set_model(
        &agent,
        &a,
        "model-x",
        meta(&[(REASONING_EFFORT_META_KEY, serde_json::json!("low"))]),
    )
    .await;
    assert_eq!(ack["canonicalModelId"], "model-x");
    assert_eq!(ack["reasoningEffort"], "low");
    assert_eq!(ack["reasoningEffortAuto"], false);
    assert!(
        ack.contains_key("model") && ack.contains_key("contextWindow"),
        "existing fields stay"
    );

    let ack = set_model(
        &agent,
        &a,
        "model-x",
        meta(&[(REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(true))]),
    )
    .await;
    assert_eq!(ack["reasoningEffortAuto"], true);

    // An explicit level in the same request wins over auto.
    let ack = set_model(
        &agent,
        &a,
        "model-x",
        meta(&[
            (REASONING_EFFORT_META_KEY, serde_json::json!("high")),
            (REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(true)),
        ]),
    )
    .await;
    assert_eq!(ack["reasoningEffort"], "high");
    assert_eq!(ack["reasoningEffortAuto"], false);
}

/// `session/load|resume` with `_meta` applies the model, level and auto flag to that session only.
#[tokio::test(flavor = "current_thread")]
async fn load_meta_applies_model_effort_and_auto_to_that_session_only() {
    let agent = build_minimal_agent_for_tests();
    with_effort_models(&agent, &["persisted-model", "asked-model", "other"]);
    let other = add_session(&agent, "sess-other", "other");
    let loaded = add_session(&agent, "sess-loaded", "persisted-model");
    let defaults = defaults_for_a_new_session(&agent);
    let info = crate::session::info::Info {
        id: loaded.clone(),
        cwd: "/tmp".to_string(),
    };
    let summary =
        crate::session::persistence::Summary::new(&info, acp::ModelId::new("persisted-model"))
            .unwrap();
    let main_meta = SessionMainMeta::from_meta(
        meta(&[
            ("modelId", serde_json::json!("asked-model")),
            (REASONING_EFFORT_META_KEY, serde_json::json!("low")),
            (REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(true)),
        ])
        .as_ref(),
    );
    agent
        .restore_persisted_model(&loaded, &summary, main_meta.effort, &main_meta)
        .await;
    assert_eq!(
        session_state(&agent, &loaded),
        ("asked-model".into(), Some(ReasoningEffort::Low), false)
    );
    assert_eq!(session_state(&agent, &other), ("other".into(), None, true));
    assert_eq!(defaults_for_a_new_session(&agent), defaults);
}

/// `session/new` `_meta` picks the model, level and auto flag atomically, for that session only.
#[test]
fn new_session_meta_sets_main_model_effort_and_auto_per_session() {
    run_local_for_bridge_test(|| async {
        let agent = build_minimal_agent_for_tests();
        with_effort_models(&agent, &["model-x"]);
        agent.set_auth_method(acp::AuthMethodId::new("cached_token"));
        let init = acp::InitializeRequest::new(acp::ProtocolVersion::V1).client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        );
        agent.initialize_request.set(init).unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let defaults = defaults_for_a_new_session(&agent);
        let create = |m: Option<acp::Meta>| {
            let request = acp::NewSessionRequest::new(cwd.path().to_path_buf()).meta(m);
            let agent = &agent;
            async move {
                Box::pin(agent.new_session_inner(request))
                    .await
                    .expect("session/new")
                    .session_id
            }
        };

        let plain = create(None).await;
        let (_, _, auto) = session_state(&agent, &plain);
        assert!(auto, "no meta: the configured default (auto) applies");

        let manual = create(meta(&[(
            REASONING_EFFORT_AUTO_META_KEY,
            serde_json::json!(false),
        )]))
        .await;
        assert!(
            !session_state(&agent, &manual).2,
            "reasoningEffortAuto=false applies"
        );

        let level_wins = create(meta(&[
            ("modelId", serde_json::json!("model-x")),
            (REASONING_EFFORT_META_KEY, serde_json::json!("low")),
            (REASONING_EFFORT_AUTO_META_KEY, serde_json::json!(true)),
        ]))
        .await;
        assert_eq!(
            session_state(&agent, &level_wins),
            ("model-x".into(), Some(ReasoningEffort::Low), false),
            "an explicit level beats auto"
        );

        let later = create(None).await;
        assert_eq!(
            session_state(&agent, &later).0,
            defaults.0,
            "a later session C ignores the earlier meta"
        );
        assert!(session_state(&agent, &later).2);
        assert_eq!(defaults_for_a_new_session(&agent), defaults);
        for sid in [plain, manual, level_wins, later] {
            agent.remove_session(&sid);
        }
    });
}
