use super::*;
use crate::theme::Theme;
use distill_shell::accounts::{Account, AccountStore, QuotaSnapshot, QuotaStatus};
use ratatui::buffer::Buffer;

fn key(state: &mut AccountsModalState, code: KeyCode) -> AccountsOutcome {
    state.handle_key(&KeyEvent::new(code, KeyModifiers::NONE))
}
fn account(id: &str, label: &str) -> AccountStatus {
    AccountStatus {
        account: Account {
            id: id.into(),
            label: label.into(),
            provider: Provider::Chatgpt,
            legacy: false,
        },
        connected: false,
        email: None,
        connection_error: None,
        quota: None,
    }
}
fn model(id: &str, name: &str, provider: Option<&str>) -> agent_client_protocol::ModelInfo {
    agent_client_protocol::ModelInfo::new(agent_client_protocol::ModelId::new(id), name).meta(
        provider.map(|provider| {
            serde_json::json!({"accountProvider": provider})
                .as_object()
                .unwrap()
                .clone()
        }),
    )
}
fn ready() -> AccountsModalState {
    let mut state = AccountsModalState::new(vec![
        model("discovered-main", "Discovered Main", Some("chatgpt")),
        model("discovered-worker", "Discovered Worker", Some("chatgpt")),
        model("foreign-model", "Foreign model", Some("claude")),
        model("chatgpt-unverified", "ChatGPT name is not authority", None),
        model("auto", "Auto", Some("chatgpt")),
    ]);
    let (seq, _) = state.begin(AccountsRequest::Load).unwrap();
    state.complete(
        seq,
        Ok(AccountsData {
            accounts: vec![account("work", "Work"), account("personal", "Personal")],
            routing: AutoRouting::default(),
            catalog: None,
        }),
    );
    state
}
fn request(outcome: AccountsOutcome) -> AccountsRequest {
    match outcome {
        AccountsOutcome::Request(request) => request,
        other => panic!("expected account request, got {other:?}"),
    }
}
fn add_route(state: &mut AccountsModalState, worker: bool) {
    key(state, KeyCode::Char('a'));
    // Existing account is preselected. Tab moves to Main; Enter opens picker.
    key(state, KeyCode::Tab);
    key(state, KeyCode::Enter);
    key(state, KeyCode::Enter);
    if worker {
        key(state, KeyCode::Tab);
        key(state, KeyCode::Enter);
        key(state, KeyCode::Down);
        key(state, KeyCode::Down);
        key(state, KeyCode::Enter);
        key(state, KeyCode::Tab);
    } else {
        key(state, KeyCode::Tab);
        key(state, KeyCode::Tab);
    }
    key(state, KeyCode::Tab); // Skip optional fallback.
    key(state, KeyCode::Enter);
    assert!(matches!(state.mode, Mode::Browse));
}

#[test]
fn account_crud_and_auth_capture_the_exact_account() {
    let mut state = ready();
    key(&mut state, KeyCode::Down);
    assert!(
        matches!(request(key(&mut state, KeyCode::Char('l'))), AccountsRequest::Login(id) if id == "personal")
    );
    key(&mut state, KeyCode::Char('n'));
    if let Mode::Rename { label, .. } = &mut state.mode {
        label.set_text("Renamed personal");
    }
    assert!(
        matches!(request(key(&mut state, KeyCode::Enter)), AccountsRequest::Rename { id, label } if id == "personal" && label == "Renamed personal")
    );
    key(&mut state, KeyCode::Char('d'));
    // Confirmation owns the captured target even when selection changes later.
    state.account_selected = 0;
    assert!(
        matches!(request(key(&mut state, KeyCode::Enter)), AccountsRequest::Logout(id) if id == "personal")
    );
    key(&mut state, KeyCode::Char('a'));
    key(&mut state, KeyCode::Right);
    key(&mut state, KeyCode::Tab);
    state.paste("Second Claude");
    let AccountsRequest::Add {
        id,
        provider,
        label,
    } = request(key(&mut state, KeyCode::Enter))
    else {
        panic!("add")
    };
    assert_eq!(provider, Provider::Claude);
    assert_eq!(label, "Second Claude");
    assert!(id.starts_with("account-"));
    let (seq, _) = state
        .begin(AccountsRequest::Add {
            id: id.clone(),
            provider,
            label,
        })
        .unwrap();
    let mut data = vec![account("work", "Work"), account(&id, "Second Claude")];
    data[1].account.provider = provider;
    assert_eq!(
        state.complete(
            seq,
            Ok(AccountsData {
                accounts: data,
                routing: AutoRouting::default(),
                catalog: None
            })
        ),
        Some(id.clone())
    );
    key(&mut state, KeyCode::Char('x'));
    assert!(
        matches!(request(key(&mut state, KeyCode::Enter)), AccountsRequest::Remove(target) if target == id)
    );
}

#[test]
fn routing_selectors_edit_reorder_and_save_persist_the_draft() {
    let home = tempfile::tempdir().unwrap();
    let store = AccountStore::new(home.path().to_path_buf());
    store.add("work", Provider::Chatgpt, "Work").unwrap();
    let mut state = ready();
    key(&mut state, KeyCode::Tab);
    add_route(&mut state, false);
    assert_eq!(state.routing.options[0].main_model, "discovered-main");
    assert_eq!(state.routing.options[0].worker_model, None);
    assert_eq!(state.routing.options[0].worker_model(), "discovered-main");
    add_route(&mut state, true);
    let second = state.routing.options[1].id.clone();
    assert_eq!(
        state.routing.options[1].worker_model.as_deref(),
        Some("discovered-worker")
    );
    key(&mut state, KeyCode::Char('K'));
    assert_eq!(state.routing.options[0].id, second);
    key(&mut state, KeyCode::Char('t'));
    key(&mut state, KeyCode::Char(' '));
    let AccountsRequest::Save(draft) = request(key(&mut state, KeyCode::Char('s'))) else {
        panic!("save")
    };
    assert!(state.dirty());
    assert!(store.load_routing().unwrap().options.is_empty());
    store.save_routing(&draft).unwrap();
    let saved = store.load_routing().unwrap();
    assert_eq!(saved, draft);
    let (seq, _) = state.begin(AccountsRequest::Save(draft)).unwrap();
    state.complete(
        seq,
        Ok(AccountsData {
            accounts: state.accounts.clone(),
            routing: saved,
            catalog: None,
        }),
    );
    assert!(!state.dirty());
    key(&mut state, KeyCode::Enter);
    key(&mut state, KeyCode::Tab);
    key(&mut state, KeyCode::Enter);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);
    key(&mut state, KeyCode::Esc); // Cancel the edited option.
    assert_eq!(state.routing, state.saved);
    key(&mut state, KeyCode::Char('J'));
    key(&mut state, KeyCode::Esc);
    assert!(matches!(state.mode, Mode::Discard));
    assert!(matches!(
        key(&mut state, KeyCode::Char('y')),
        AccountsOutcome::Close
    ));
    assert_eq!(store.load_routing().unwrap(), state.saved);

    let mut filtered = ready();
    filtered.accounts[1].account.provider = Provider::Claude;
    filtered.pane = Pane::Routing;
    key(&mut filtered, KeyCode::Char('a'));
    filtered.activate(Control::Field(1));
    let Mode::Pick { choices, .. } = &filtered.mode else {
        panic!("Main picker")
    };
    assert_eq!(
        choices
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>(),
        ["discovered-main", "discovered-worker"]
    );
    filtered.activate(Control::Choose(0));
    filtered.activate(Control::Field(2));
    let Mode::Pick { choices, .. } = &filtered.mode else {
        panic!("Worker picker")
    };
    assert_eq!(
        choices
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>(),
        [
            "",
            "discovered-main",
            "discovered-worker",
            "foreign-model",
            "chatgpt-unverified"
        ]
    );
    filtered.activate(Control::Choose(2));
    filtered.activate(Control::Field(0));
    filtered.activate(Control::Choose(1));
    let Mode::Route { option, .. } = &filtered.mode else {
        panic!("route")
    };
    assert!(option.main_model.is_empty());
    assert_eq!(option.worker_model.as_deref(), Some("discovered-worker"));
    assert!(filtered.message.as_deref().unwrap().contains("Claude Main"));
    filtered.activate(Control::Field(1));
    let Mode::Pick { choices, .. } = &filtered.mode else {
        panic!("Claude Main")
    };
    assert_eq!(
        choices
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>(),
        ["foreign-model"]
    );
    filtered.activate(Control::Choose(0));
    filtered.activate(Control::Field(2));
    let Mode::Pick { choices, .. } = &filtered.mode else {
        panic!("Claude Worker")
    };
    assert_eq!(
        choices
            .iter()
            .map(|(_, id)| id.as_str())
            .collect::<Vec<_>>(),
        [
            "",
            "discovered-main",
            "discovered-worker",
            "foreign-model",
            "chatgpt-unverified"
        ]
    );
    filtered.activate(Control::Choose(4)); // A custom model has no account-provider tag.
    filtered.activate(Control::Field(3));
    filtered.activate(Control::Choose(2));
    filtered.activate(Control::Field(4));
    assert_eq!(
        filtered.routing.options[0].worker_model.as_deref(),
        Some("chatgpt-unverified")
    );
    assert_eq!(
        filtered.routing.options[0].worker_fallback_model.as_deref(),
        Some("discovered-worker")
    );
    assert!(filtered.validate_routing(&filtered.routing).is_ok());
    filtered.routing.options[0].main_model = "chatgpt-unverified".into();
    assert!(matches!(
        key(&mut filtered, KeyCode::Char('s')),
        AccountsOutcome::Changed
    ));
    assert!(
        filtered
            .message
            .as_deref()
            .unwrap()
            .contains("Main provider is unverified")
    );

    let mut unavailable = ready();
    unavailable.saved = AutoRouting {
        enabled: true,
        strategy: AutoStrategy::OrderedFirst,
        options: vec![AutoOption {
            id: "saved-route".into(),
            account_id: "work".into(),
            main_model: "discovered-main".into(),
            worker_fallback_model: None,
            worker_model: None,
        }],
    };
    unavailable.routing = unavailable.saved.clone();
    unavailable.routing.enabled = false;
    unavailable.update_models(vec![]);
    assert!(
        matches!(key(&mut unavailable, KeyCode::Char('s')), AccountsOutcome::Request(AccountsRequest::Save(ref routing)) if !routing.enabled && routing.options == unavailable.saved.options)
    );
    unavailable.routing.strategy = AutoStrategy::HighestRemaining;
    assert!(matches!(
        key(&mut unavailable, KeyCode::Char('s')),
        AccountsOutcome::Changed
    ));
    unavailable.routing = unavailable.saved.clone();
    assert!(matches!(
        key(&mut unavailable, KeyCode::Char('s')),
        AccountsOutcome::Changed
    ));
}

#[test]
fn async_cancellation_and_failed_save_preserve_identity_and_draft() {
    let mut state = ready();
    let (old_seq, token) = state
        .begin(AccountsRequest::Login("personal".into()))
        .unwrap();
    key(&mut state, KeyCode::Esc);
    assert!(token.is_cancelled());
    let (new_seq, _) = state.begin(AccountsRequest::Login("work".into())).unwrap();
    state.complete(
        old_seq,
        Ok(AccountsData {
            accounts: vec![],
            routing: AutoRouting::default(),
            catalog: None,
        }),
    );
    assert!(matches!(state.busy, Some(AccountsRequest::Login(ref id)) if id == "work"));
    state.complete(new_seq, Err("Sign-in failed. Retry Login.".into()));
    assert_eq!(state.accounts.len(), 2);
    state.routing.enabled = true;
    state.routing.options.push(AutoOption {
        id: "route".into(),
        account_id: "work".into(),
        main_model: "discovered-main".into(),
        worker_fallback_model: None,
        worker_model: None,
    });
    let draft = state.routing.clone();
    let (seq, _) = state.begin(AccountsRequest::Save(draft.clone())).unwrap();
    state.complete(seq, Err("Could not save routing.".into()));
    assert_eq!(state.routing, draft);
    assert!(state.dirty());
    assert!(!state.saved.enabled);
    state.begin(AccountsRequest::Login("work".into())).unwrap();
    assert!(matches!(
        key(&mut state, KeyCode::Esc),
        AccountsOutcome::Request(AccountsRequest::Load)
    ));
    let (seq, _) = state.begin(AccountsRequest::Load).unwrap();
    state.complete(
        seq,
        Ok(AccountsData {
            accounts: state.accounts.clone(),
            routing: AutoRouting::default(),
            catalog: None,
        }),
    );
    assert_eq!(state.routing, draft); // Status reload after cancellation preserves unsaved routing.
}

#[test]
fn quota_unknown_unavailable_stale_and_loading_are_distinct() {
    let mut account = account("work", "Work");
    assert!(render::quota_label(&account, None).contains("unknown"));
    assert!(!render::quota_label(&account, None).contains("0%"));
    account.connected = true;
    account.quota = Some(QuotaSnapshot {
        account_id: "work".into(),
        checked_at: chrono::Utc::now(),
        fetched_at: Some(chrono::Utc::now() - chrono::Duration::minutes(10)),
        status: QuotaStatus::Available,
        remaining_percent: Some(37.0),
        detail: None,
    });
    assert!(render::quota_label(&account, None).contains("37% remaining (stale)"));
    account.quota.as_mut().unwrap().fetched_at = Some(chrono::Utc::now());
    assert!(render::quota_label(&account, None).contains("37% remaining ["));
    account.connected = false;
    assert!(render::quota_label(&account, None).contains("37% remaining (stale)"));
    account.quota.as_mut().unwrap().status = QuotaStatus::Unavailable;
    assert!(render::quota_label(&account, None).contains("unavailable"));
    account.quota.as_mut().unwrap().status = QuotaStatus::Unsupported;
    assert!(render::quota_label(&account, None).contains("unsupported"));
    assert!(
        render::quota_label(&account, Some(&AccountsRequest::Refresh("work".into())))
            .contains("refreshing")
    );
}

#[test]
fn responsive_render_and_mouse_controls_remain_usable_with_long_values() {
    let mut state = ready();
    state.accounts[0].account.label = "Very long account label — 日本語 👩‍💻 ".repeat(8);
    state.accounts[0].email = Some("unbreakable.address".repeat(12));
    for i in 0..12 {
        state.accounts.push(account(&format!("extra-{i}"), "Extra"));
    }
    for (width, height) in [(120, 40), (80, 24), (34, 14)] {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render_accounts_modal(&mut buf, area, &mut state, false, &Theme::current());
        assert!(
            state
                .hits
                .iter()
                .all(|(r, _)| r.right() <= width && r.bottom() <= height)
        );
        assert!(
            state
                .hits
                .iter()
                .any(|(_, c)| matches!(c, Control::Account(0))),
            "selected account must stay visible at {width}x{height}"
        );
        let rect = state
            .hits
            .iter()
            .find(|(_, c)| matches!(c, Control::Add))
            .unwrap()
            .0;
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert!(matches!(state.mode, Mode::Add { .. }));
        key(&mut state, KeyCode::Esc);
        state.pane = Pane::Routing;
        render_accounts_modal(&mut buf, area, &mut state, false, &Theme::current());
        let rect = state
            .hits
            .iter()
            .rfind(|(_, c)| matches!(c, Control::Add))
            .unwrap()
            .0;
        state.handle_mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y);
        assert!(matches!(state.mode, Mode::Route { .. }));
        render_accounts_modal(&mut buf, area, &mut state, false, &Theme::current());
        assert!(
            state
                .hits
                .iter()
                .any(|(_, c)| matches!(c, Control::Field(2)))
        );
        key(&mut state, KeyCode::Esc);
        state.pane = Pane::Accounts;
    }
    state.accounts.clear();
    let area = Rect::new(0, 0, 34, 14);
    let mut buf = Buffer::empty(area);
    render_accounts_modal(&mut buf, area, &mut state, false, &Theme::current());
    let text: String = buf.content.iter().map(|c| c.symbol()).collect();
    assert!(text.contains("No accounts"));
}
