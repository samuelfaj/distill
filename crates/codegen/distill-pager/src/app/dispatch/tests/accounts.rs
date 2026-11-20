use super::*;
use crate::views::{
    accounts_modal::{AccountsData, AccountsRequest},
    modal::ActiveModal,
};

#[test]
fn accounts_entry_points_open_modal_and_request_loading() {
    let registry =
        crate::slash::registry::CommandRegistry::new(crate::slash::commands::builtin_commands());
    assert_eq!(
        registry.get_for_dispatch("accounts").unwrap().name(),
        "accounts"
    );
    assert_eq!(
        registry.get_for_dispatch("config").unwrap().name(),
        "settings"
    );
    let mut app = test_app_with_agent();
    dispatch(Action::OpenSettings, &mut app);
    let Some(ActiveModal::Settings { state }) = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .active_modal
        .as_mut()
    else {
        panic!("settings")
    };
    let outcome = crate::views::settings_modal::handle_settings_key(
        state,
        &crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    assert!(matches!(
        outcome,
        crate::views::settings_modal::SettingsKeyOutcome::Action(Action::OpenAccounts)
    ));
    let effects = dispatch(Action::OpenAccounts, &mut app);
    let [Effect::Accounts(task)] = effects.as_slice() else {
        panic!("load effect: {effects:?}")
    };
    assert!(matches!(task.request, AccountsRequest::Load));
    let (instance, seq) = (task.instance, task.seq);
    dispatch(
        Action::TaskComplete(TaskResult::AccountsFinished {
            agent_id: AgentId(0),
            instance,
            seq,
            result: Ok(AccountsData {
                accounts: vec![],
                routing: distill_shell::accounts::AutoRouting::default(),
                catalog: None,
            }),
        }),
        &mut app,
    );
    let Some(ActiveModal::Accounts { state }) =
        app.agents.get(&AgentId(0)).unwrap().active_modal.as_ref()
    else {
        panic!("accounts")
    };
    assert!(state.loaded);
    assert!(state.busy.is_none());
}

#[test]
fn reopened_modal_rejects_prior_results_and_browser_feedback() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::OpenAccounts, &mut app);
    let Effect::Accounts(old) = &effects[0] else {
        panic!("accounts effect")
    };
    let old_cancel = old.cancel.clone();
    let old_instance = old.instance;
    let old_seq = old.seq;
    dispatch(Action::OpenAccounts, &mut app);
    assert!(old_cancel.is_cancelled());
    dispatch(
        Action::TaskComplete(TaskResult::AccountsFinished {
            agent_id: AgentId(0),
            instance: old_instance,
            seq: old_seq,
            result: Err("stale result".into()),
        }),
        &mut app,
    );
    dispatch(
        Action::TaskComplete(TaskResult::AccountsLoginFeedback {
            agent_id: AgentId(0),
            instance: old_instance,
            seq: old_seq,
            feedback: Some(distill_shell::accounts::AccountLoginFeedback {
                authorization_url: "https://example.test/sign-in".into(),
                device_code: None,
            }),
        }),
        &mut app,
    );
    let Some(ActiveModal::Accounts { state }) =
        app.agents.get(&AgentId(0)).unwrap().active_modal.as_ref()
    else {
        panic!("accounts")
    };
    assert_ne!(state.instance, old_instance);
    assert!(state.login_feedback.is_none());
    assert_eq!(state.message.as_deref(), Some("Loading accounts…"));
}

#[test]
fn enabled_save_switches_to_auto_failure_keeps_model_and_disabled_save_does_not_switch() {
    use distill_shell::accounts::{AutoOption, AutoRouting, AutoStrategy};
    let mut app = test_app_with_agent();
    let manual = acp::ModelId::new("manual");
    let manual_info = acp::ModelInfo::new(manual.clone(), "Manual").meta(Some(
        serde_json::json!({"accountProvider":"chatgpt"})
            .as_object()
            .unwrap()
            .clone(),
    ));
    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .models
        .available
        .insert(manual.clone(), manual_info.clone());
    let fixture = distill_shell::accounts::AccountStatus {
        account: distill_shell::accounts::Account {
            id: "fixture".into(),
            provider: distill_shell::accounts::Provider::Chatgpt,
            label: "Fixture".into(),
            legacy: false,
        },
        connected: false,
        email: None,
        connection_error: None,
        quota: None,
    };

    app.agents
        .get_mut(&AgentId(0))
        .unwrap()
        .session
        .models
        .set_current(manual.clone(), None);
    let effects = dispatch(Action::OpenAccounts, &mut app);
    let Effect::Accounts(task) = &effects[0] else {
        panic!("load")
    };
    dispatch(
        Action::TaskComplete(TaskResult::AccountsFinished {
            agent_id: AgentId(0),
            instance: task.instance,
            seq: task.seq,
            result: Ok(AccountsData {
                accounts: vec![fixture.clone()],
                routing: AutoRouting::default(),
                catalog: None,
            }),
        }),
        &mut app,
    );
    let routing = AutoRouting {
        enabled: true,
        strategy: AutoStrategy::OrderedFirst,
        options: vec![AutoOption {
            id: "route".into(),
            account_id: "fixture".into(),
            main_model: "manual".into(),
            worker_fallback_model: None,
            worker_model: None,
        }],
    };
    let effects = dispatch(
        Action::AccountsRequest(AccountsRequest::Save(routing.clone())),
        &mut app,
    );
    let Effect::Accounts(task) = &effects[0] else {
        panic!("save")
    };
    let catalog = acp::SessionModelState::new(
        manual.clone(),
        vec![
            manual_info.clone(),
            acp::ModelInfo::new(acp::ModelId::new("auto"), "Auto"),
        ],
    );
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AccountsFinished {
            agent_id: AgentId(0),
            instance: task.instance,
            seq: task.seq,
            result: Ok(AccountsData {
                accounts: vec![fixture.clone()],
                routing: routing.clone(),
                catalog: Some(catalog),
            }),
        }),
        &mut app,
    );
    assert!(
        matches!(effects.as_slice(), [Effect::SwitchModel { model_id, session_id, .. }] if model_id.0.as_ref() == "auto" && session_id.0.as_ref() == "test-session")
    );
    let Some(ActiveModal::Accounts { state }) =
        app.agents.get(&AgentId(0)).unwrap().active_modal.as_ref()
    else {
        panic!("accounts")
    };
    assert_eq!(state.saved, routing);
    assert!(state.auto_activation_pending);
    assert!(!state.message.as_deref().unwrap().contains("Auto active"));
    assert_eq!(
        app.agents.get(&AgentId(0)).unwrap().session.models.current,
        Some(manual.clone())
    );
    assert!(
        app.agents
            .get(&AgentId(0))
            .unwrap()
            .session
            .models
            .available
            .contains_key(&acp::ModelId::new("auto"))
    );
    assert!(!state.models.iter().any(|model| model.id == "auto")); // Logical Auto is not a provider model in a route.
    dispatch(
        Action::TaskComplete(TaskResult::SwitchModelComplete {
            agent_id: AgentId(0),
            model_id: acp::ModelId::new("auto"),
            effort: None,
            result: Err(SwitchModelError::Other(
                "No eligible fixture accounts".into(),
            )),
            prev_model_id: None,
        }),
        &mut app,
    );
    let Some(ActiveModal::Accounts { state }) =
        app.agents.get(&AgentId(0)).unwrap().active_modal.as_ref()
    else {
        panic!("accounts")
    };
    assert!(!state.auto_activation_pending);
    assert!(state.saved.enabled); // Activation failure does not pretend persistence failed.
    assert!(
        state
            .message
            .as_deref()
            .unwrap()
            .contains("Auto activation failed")
    );
    assert!(!state.message.as_deref().unwrap().contains("Auto active"));
    assert_eq!(
        app.agents.get(&AgentId(0)).unwrap().session.models.current,
        Some(manual.clone())
    );

    let disabled = AutoRouting {
        enabled: false,
        ..routing
    };
    let effects = dispatch(
        Action::AccountsRequest(AccountsRequest::Save(disabled.clone())),
        &mut app,
    );
    let Effect::Accounts(task) = &effects[0] else {
        panic!("disabled save")
    };
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AccountsFinished {
            agent_id: AgentId(0),
            instance: task.instance,
            seq: task.seq,
            result: Ok(AccountsData {
                accounts: vec![fixture.clone()],
                routing: disabled,
                catalog: Some(acp::SessionModelState::new(
                    manual.clone(),
                    vec![manual_info.clone()],
                )),
            }),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(
        app.agents.get(&AgentId(0)).unwrap().session.models.current,
        Some(manual)
    );
    assert!(
        !app.agents
            .get(&AgentId(0))
            .unwrap()
            .session
            .models
            .available
            .contains_key(&acp::ModelId::new("auto"))
    );
}
