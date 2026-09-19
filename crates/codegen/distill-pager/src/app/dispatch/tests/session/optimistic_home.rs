// Modified for Distill by Samuel Fajreldines, 2026.
//! GBT-6212: create the session when the TUI opens; the first interaction reveals it.

use super::*;
use crate::app::app_view::{InputOutcome, PasteProvenance};
use crate::app::dispatch::session::lifecycle::{
    handle_session_created, handle_session_failed, handle_worktree_session_failed,
    maybe_create_home_session,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn key_event(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}

/// Feed `ev` to Welcome, require `ActionThenForward(LeaveHome)`, and run it through the event loop's forward path.
fn leave_home_with(app: &mut AppView, ev: &Event) -> Vec<Effect> {
    let outcome = app.handle_input(ev);
    let InputOutcome::ActionThenForward(Action::LeaveHome) = outcome else {
        panic!("expected ActionThenForward(LeaveHome), got {outcome:?}");
    };
    crate::app::event_loop::dispatch_then_forward(
        Action::LeaveHome,
        ev,
        std::time::Instant::now(),
        PasteProvenance::Terminal,
        app,
    )
}

fn creates_session(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, Effect::CreateSession { .. }))
}

#[test]
fn drain_without_session_intent_creates_home_session_and_stays_on_welcome() {
    let mut app = test_app();
    assert!(app.session_startup_allowed());
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(app.agents.is_empty());

    let effects = drain_startup_actions(&mut app);

    assert!(
        matches!(app.active_view, ActiveView::Welcome),
        "home must stay up after the optimistic create, got {:?}",
        app.active_view
    );
    assert_eq!(app.agents.len(), 1);
    assert_eq!(app.home_session_agent, Some(AgentId(0)));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "expected CreateSession, got {effects:?}"
    );
}

#[test]
fn maybe_create_home_session_is_idempotent() {
    let mut app = test_app();
    let first = maybe_create_home_session(&mut app);
    let second = maybe_create_home_session(&mut app);
    assert_eq!(app.agents.len(), 1);
    assert!(!first.is_empty());
    assert!(
        second.is_empty(),
        "a second create must not spawn another agent"
    );
}

#[test]
fn maybe_create_home_session_skips_when_gated() {
    let mut app = test_app();
    app.auth_state = AuthState::Pending { error: None };
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.agents.is_empty());
    assert!(app.home_session_agent.is_none());
}

#[test]
fn maybe_create_home_session_skips_when_startup_will_leave_home() {
    let mut app = test_app();
    app.deferred_startup.prompt = Some("from cli".into());
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.agents.is_empty());
    assert!(app.home_session_agent.is_none());
}

#[test]
fn maybe_create_home_session_skips_when_access_blocked() {
    let mut app = test_app();
    app.gate = Some(distill_login::GateInfo {
        message: "paywall".into(),
        url: None,
        label: None,
    });
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.agents.is_empty());
}

#[test]
fn maybe_create_home_session_does_not_consume_pending_chat() {
    let mut app = test_app();
    app.deferred_startup.pending_chat = true;
    maybe_create_home_session(&mut app);
    assert!(
        app.deferred_startup.pending_chat,
        "optimistic home must not steal leftover /chat"
    );
    assert!(
        app.home_session()
            .is_none_or(|a| !a.chat_kind && !a.conversation_entry)
    );
}

#[test]
fn maybe_create_home_session_skips_when_zdr_blocked() {
    let mut app = test_app();
    app.is_zdr = true;
    app.zdr_access_enabled = false;
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.agents.is_empty());
}

#[test]
fn welcome_keystroke_reveals_home_session_and_types() {
    for focused in [true, false] {
        let mut app = test_app();
        maybe_create_home_session(&mut app);
        let home = app.home_session_agent.expect("home session");
        app.welcome_prompt_focused = focused;
        app.welcome_menu_index = (!focused).then_some(0);

        let effects = leave_home_with(&mut app, &key_event(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(
            !creates_session(&effects),
            "keystroke must reuse the optimistic session, got {effects:?}"
        );
        assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));
        assert!(app.home_session_agent.is_none());
        assert_eq!(app.agents.len(), 1);
        assert_eq!(app.agents.get(&home).map(|a| a.prompt.text()), Some("h"));
        assert!(app.welcome_menu_index.is_none());
    }
}

#[test]
fn welcome_paste_reveals_home_session() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let effects = leave_home_with(&mut app, &Event::Paste("fix the bug".into()));
    assert!(!creates_session(&effects));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));
    assert_eq!(
        app.agents.get(&home).map(|a| a.prompt.text()),
        Some("fix the bug")
    );
}

#[test]
fn welcome_shift_tab_reveals_home_session_in_plan_mode() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let effects = leave_home_with(&mut app, &key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(!creates_session(&effects));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));
    let Some(agent) = app.agents.get(&home) else {
        panic!("expected agent {home:?}");
    };
    assert!(
        agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
        "the forwarded Shift+Tab must enter Plan on the revealed session"
    );

    // Bind after a queued prompt: the mode must go out before that prompt.
    let _ = dispatch(Action::SendPrompt("go".into()), &mut app);
    let effects =
        handle_session_created(&mut app, home, acp::SessionId::new("plan-home"), None, None);
    let mode_at = effects
        .iter()
        .position(|e| matches!(e, Effect::SetSessionMode { .. }));
    let send_at = effects
        .iter()
        .position(|e| matches!(e, Effect::SendPrompt { .. }));
    assert!(
        matches!((mode_at, send_at), (Some(m), Some(s)) if m < s),
        "SetSessionMode must precede the drained SendPrompt, got {effects:?}"
    );
}

/// The common ordering: SessionCreated arrived before the first keystroke, so the
/// forwarded cycle takes the bound-session path and pushes the mode over ACP.
#[test]
fn welcome_shift_tab_on_bound_home_session_sets_mode_over_acp() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let effects = leave_home_with(&mut app, &key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::SetSessionMode { session_id, .. } if session_id.0.as_ref() == "home-sid"
        )),
        "bound cycle must push SetSessionMode, got {effects:?}"
    );
}

#[test]
fn welcome_shift_tab_honors_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    assert!(maybe_create_home_session(&mut app).is_empty());

    let effects = leave_home_with(&mut app, &key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "got {effects:?}"
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("Shift+Tab must leave home, got {:?}", app.active_view);
    };
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    assert!(agent.plan_mode_pending.unwrap_or(agent.plan_mode_active));
}

/// Skills that reached the hidden home session must be in the revealed composer's slash catalog.
#[test]
fn skills_on_the_hidden_home_session_reach_the_revealed_composer() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    {
        let agent = app.agents.get_mut(&home).unwrap();
        agent.session.available_commands = vec![acp::AvailableCommand::new(
            "my-skill".to_string(),
            "A skill".to_string(),
        )];
        agent.session.available_commands_generation += 1;
    }

    let _ = leave_home_with(&mut app, &key_event(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(
        app.agents.get(&home).is_some_and(|a| a
            .prompt
            .slash_controller
            .registry()
            .get("my-skill")
            .is_some()),
        "reveal must sync the husk's ACP commands into the composer"
    );
}

#[test]
fn worktree_from_welcome_abandons_home_and_keeps_draft() {
    let mut app = test_app_git();
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("draft from home");
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(
        Action::NewWorktreeSession {
            load_session_id: None,
            label: None,
            git_ref: None,
        },
        &mut app,
    );
    assert!(
        !app.agents.contains_key(&home),
        "Ctrl+W must drop the unused in-cwd husk"
    );
    assert!(app.home_session_agent.is_none());
    let ActiveView::Agent(id) = app.active_view else {
        panic!("Ctrl+W must leave home, got {:?}", app.active_view);
    };
    assert_eq!(
        app.agents.get(&id).map(|a| a.prompt.text()),
        Some("draft from home")
    );
}

#[test]
fn dashboard_from_welcome_exits_back_to_welcome() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = crate::app::dispatch::dashboard::dispatch_open_dashboard(&mut app);
    assert!(matches!(app.active_view, ActiveView::AgentDashboard));
    assert_eq!(app.home_session_agent, Some(home));

    let _ = crate::app::dispatch::dashboard::dispatch_exit_dashboard(&mut app);
    assert!(
        matches!(app.active_view, ActiveView::Welcome),
        "closing the dashboard must not reveal the unused home session, got {:?}",
        app.active_view
    );
    assert_eq!(app.home_session_agent, Some(home));
    assert!(app.agents.contains_key(&home));
}

#[test]
fn dashboard_new_agent_abandons_unused_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    let _ = crate::app::dispatch::dashboard::dispatch_open_dashboard(&mut app);

    let _ =
        crate::app::dispatch::dashboard::dispatch_dashboard_create_new_agent_with_detail(&mut app);
    assert!(
        !app.agents.contains_key(&home),
        "New Agent from the dashboard must drop the unused home husk"
    );
    assert!(app.home_session_agent.is_none());
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
}

#[test]
fn foreign_resume_still_detects_after_home_session_create() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    assert!(
        app.only_unused_home_or_empty(),
        "unused home must still count as a cold Welcome launch"
    );
    app.foreign_session_compat = distill_foreign_sessions::EnabledForeignSessionSources {
        claude: true,
        ..Default::default()
    };
    let effect = app.begin_foreign_resume_detection();
    assert!(
        effect.is_some(),
        "Claude/Codex/Cursor continue hint must still run after optimistic home create"
    );
}

#[test]
fn attaching_home_husk_promotes_it_so_new_session_does_not_delete_it() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    crate::app::dispatch::ctx::switch_to_agent(
        &mut app,
        home,
        crate::app::dispatch::ctx::SwitchCause::Picker,
    );
    assert!(
        app.home_session_agent.is_none(),
        "promoting the husk must clear the unused-home pointer"
    );
    assert!(app.agents.contains_key(&home));
    let _ = dispatch(Action::NewSession, &mut app);
    assert!(
        app.agents.contains_key(&home),
        "Ctrl+N after attach must not delete the live conversation"
    );
}

#[test]
fn maybe_create_home_session_skips_chat_mode() {
    let mut app = test_app();
    app.chat_mode = true;
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.home_session_agent.is_none());
}

#[test]
fn maybe_create_home_session_skips_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    let effects = maybe_create_home_session(&mut app);
    assert!(effects.is_empty());
    assert!(app.home_session_agent.is_none());
}

#[test]
fn drain_skips_home_create_for_worktree_and_dashboard_intents() {
    let mut app = test_app();
    app.deferred_startup.worktree = true;
    let effects = drain_startup_actions(&mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "worktree startup must not create an in-cwd husk, got {effects:?}"
    );

    let mut app = test_app();
    app.deferred_startup.open_dashboard = true;
    let effects = drain_startup_actions(&mut app);
    assert!(app.home_session_agent.is_none());
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "dashboard startup must not create an in-cwd husk, got {effects:?}"
    );

    let mut app = test_app();
    app.deferred_startup.new_session = true;
    let _ = drain_startup_actions(&mut app);
    // NewSession from drain creates a visible session, not a hidden husk.
    assert!(app.home_session_agent.is_none());
}

#[test]
fn load_from_welcome_abandons_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        !app.agents.contains_key(&home),
        "resume must drop the unused home husk"
    );
    assert!(app.home_session_agent.is_none());
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
}

/// `/resume` reveals the home husk before the picker. LoadSession must still delete it.
#[test]
fn resume_after_reveal_then_load_abandons_empty() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(Action::SendPrompt("/resume".into()), &mut app);
    assert!(app.home_session_agent.is_none());
    assert!(app.agents.contains_key(&home));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));

    let effects = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        !app.agents.contains_key(&home),
        "LoadSession after /resume reveal must drop the unused empty, got agents {:?}",
        app.agents.keys().collect::<Vec<_>>()
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::DeleteSession {
                session_id,
                after: crate::app::actions::AfterSessionDelete::UnusedHusk,
                ..
            } if session_id == "home-sid"
        )),
        "revealed unused empty must be deleted, got {effects:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id != home));
}

#[test]
fn resume_after_reveal_unbound_then_load_drops_empty() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(Action::SendPrompt("/resume".into()), &mut app);
    assert!(app.home_session_agent.is_none());
    assert!(app.agents.contains_key(&home));

    let _ = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        !app.agents.contains_key(&home),
        "unbound revealed husk must still be dropped on LoadSession"
    );
}

#[test]
fn load_session_does_not_abandon_empty_new_session() {
    let mut app = test_app();
    let _ = dispatch(Action::NewSession, &mut app);
    let ActiveView::Agent(new_id) = app.active_view else {
        panic!(
            "/new must activate the new session, got {:?}",
            app.active_view
        );
    };
    let _ = handle_session_created(&mut app, new_id, acp::SessionId::new("new-sid"), None, None);

    let _ = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        app.agents.contains_key(&new_id),
        "LoadSession must not delete an empty /new that was never the home husk"
    );
}

#[test]
fn load_session_keeps_revealed_husk_with_composer_draft() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(Action::SendPrompt("/resume".into()), &mut app);
    assert!(app.home_session_agent.is_none());
    app.agents
        .get_mut(&home)
        .expect("revealed husk")
        .prompt
        .set_text("unsaved notes");

    let _ = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        app.agents.contains_key(&home),
        "LoadSession must not delete the revealed husk while it has an unsaved composer draft"
    );
    assert_eq!(test_agent(&app, home).prompt.text(), "unsaved notes");
}

#[test]
fn load_session_keeps_revealed_husk_with_image_undo_stash() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");

    let _ = dispatch(Action::SendPrompt("/resume".into()), &mut app);
    assert!(app.home_session_agent.is_none());
    app.agents
        .get_mut(&home)
        .expect("revealed husk")
        .prompt
        .push_image_undo_stash_for_test(crate::app::agent_view::test_fixtures::test_pasted_image());

    let _ = dispatch(
        Action::LoadSession("resume-me".into(), None, false),
        &mut app,
    );
    assert!(
        app.agents.contains_key(&home),
        "LoadSession must not delete the husk while Ctrl+U image undo stash is recoverable"
    );
}

#[test]
fn welcome_menu_enter_activates_item_when_prompt_unfocused() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt_focused = false;
    app.welcome_menu_index = Some(1);

    let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(outcome, InputOutcome::Action(Action::FetchSessionList)),
        "Enter on a selected menu item must not be swallowed as send, got {outcome:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Welcome));
}

#[test]
fn welcome_overlay_paste_does_not_reach_composer() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("");
    app.trust_state = TrustState::Pending {
        workspace: app.cwd.clone(),
    };

    let outcome = app.handle_input(&Event::Paste("should not land".into()));
    assert!(
        !matches!(
            outcome,
            InputOutcome::Action(_) | InputOutcome::ActionThenForward(_)
        ),
        "trust overlay must own paste, got {outcome:?}"
    );
    assert!(
        app.welcome_prompt.text().is_empty(),
        "overlay paste must not leak into the home composer, got {:?}",
        app.welcome_prompt.text()
    );
}

#[test]
fn welcome_empty_enter_reveals_home_session() {
    // Focused, or unfocused with no menu selection (Esc from the prompt).
    for focused in [true, false] {
        let mut app = test_app();
        maybe_create_home_session(&mut app);
        let home = app.home_session_agent.expect("home session");
        app.welcome_prompt_focused = focused;
        app.welcome_menu_index = None;

        let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(outcome, InputOutcome::Action(Action::LeaveHome)),
            "focused={focused}: empty Enter leaves home without sending, got {outcome:?}"
        );
        let effects = dispatch(Action::LeaveHome, &mut app);
        assert!(!creates_session(&effects));
        assert!(matches!(app.active_view, ActiveView::Agent(id) if id == home));
        assert!(
            app.agents
                .get(&home)
                .is_some_and(|a| a.session.pending_prompts.is_empty())
        );
    }
}

/// When the action leaves Welcome up, the forwarded key / paste becomes a welcome
/// draft (carried across by the eventual leave-home) instead of being re-routed
/// through Welcome's key handling.
#[test]
fn forward_lands_in_the_welcome_composer_when_the_action_leaves_welcome_up() {
    for (ev, expected) in [
        (key_event(KeyCode::Char('y'), KeyModifiers::NONE), "y"),
        (Event::Paste("fix the bug".into()), "fix the bug"),
    ] {
        let mut app = test_app();
        maybe_create_home_session(&mut app);
        app.welcome_prompt_focused = false;
        let _ = crate::app::event_loop::dispatch_then_forward(
            Action::CycleMode,
            &ev,
            std::time::Instant::now(),
            PasteProvenance::Terminal,
            &mut app,
        );
        assert!(matches!(app.active_view, ActiveView::Welcome));
        assert!(app.welcome_menu_index.is_none());
        assert_eq!(app.welcome_prompt.text(), expected);
    }
}

/// The real Welcome-stays route: chat mode (no husk) + Local workspace without
/// an ACK opens the y/N prompt. The forwarded `y` must not confirm it.
#[cfg(feature = "local-workspace")]
#[test]
#[serial_test::serial(GROK_CHAT_LOCAL_WORKSPACE_ACK)]
fn leave_home_into_local_workspace_ack_keeps_the_keystroke_as_a_draft() {
    let _ack = distill_test_support::EnvGuard::unset(
        crate::app::session_startup::GROK_CHAT_LOCAL_WORKSPACE_ACK_ENV,
    );
    let home = tempfile::tempdir().unwrap();
    let _home = distill_test_support::EnvGuard::set("GROK_HOME", home.path().to_str().unwrap());
    crate::app::session_startup::set_active_local_workspace(None).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut app = test_app();
    app.chat_mode = true;
    app.cwd = tmp.path().to_path_buf();
    app.welcome_workspace_mode = crate::views::welcome::WelcomeWorkspaceMode::LocalWorkspace;
    assert!(maybe_create_home_session(&mut app).is_empty());

    let effects = leave_home_with(&mut app, &key_event(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(app.welcome_local_workspace_ack_pending);
    assert!(!creates_session(&effects), "got {effects:?}");
    assert!(app.agents.is_empty());
    assert_eq!(app.welcome_prompt.text(), "y");
}

#[test]
fn leave_home_is_a_no_op_behind_the_startup_gate() {
    let mut app = test_app();
    app.trust_state = TrustState::Pending {
        workspace: app.cwd.clone(),
    };
    assert!(dispatch(Action::LeaveHome, &mut app).is_empty());
    assert!(app.agents.is_empty());

    let mut app = test_app();
    app.consent_state = crate::app::consent::ConsentState::Pending {
        notice: crate::app::consent::ConsentNotice {
            id: "terms".into(),
            version: 1,
            title: "Updated terms".into(),
            segments: vec![crate::app::consent::ConsentSegment::Text("Review.".into())],
            links: Vec::new(),
            accept_label: "Got it".into(),
        },
        legibility: crate::app::consent::ConsentLegibility::Painted,
        painted_at: Some(std::time::Instant::now()),
    };
    assert!(dispatch(Action::LeaveHome, &mut app).is_empty());
    assert!(app.agents.is_empty());
}

/// The paywall / ZDR menus own every key on Welcome, so typing there never
/// reaches the leave-home branch (the shared helper is deliberately not access-gated).
#[test]
fn typing_on_a_gated_welcome_does_not_leave_home() {
    let mut app = test_app();
    app.gate = Some(distill_shell::auth::GateInfo {
        message: "SuperGrok subscription required".into(),
        url: Some("https://grok.com/supergrok".into()),
        label: Some("Subscribe".into()),
    });
    app.welcome_prompt_focused = true;
    let outcome = app.handle_input(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(
        !matches!(
            outcome,
            InputOutcome::ActionThenForward(_) | InputOutcome::Action(_)
        ),
        "paywalled welcome must swallow text keys, got {outcome:?}"
    );

    let mut app = test_app();
    app.is_zdr = true;
    app.zdr_access_enabled = false;
    app.welcome_prompt_focused = true;
    let outcome = app.handle_input(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(
        !matches!(
            outcome,
            InputOutcome::ActionThenForward(_) | InputOutcome::Action(_)
        ),
        "ZDR-blocked welcome must swallow text keys, got {outcome:?}"
    );
}

#[test]
fn welcome_enter_with_text_sends_prompt_and_leaves_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt_focused = true;
    app.welcome_prompt.set_text("hello");

    let outcome = app.handle_input(&key_event(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(Action::SendPrompt(text)) = outcome else {
        panic!("expected SendPrompt, got {outcome:?}");
    };
    assert_eq!(text, "hello");

    let _effects = dispatch(Action::SendPrompt(text), &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "first sent prompt must leave home, got {:?}",
        app.active_view
    );
    assert!(app.home_session_agent.is_none());
    let Some(agent) = app.agents.get(&AgentId(0)) else {
        panic!("expected agent 0");
    };
    assert!(
        agent
            .session
            .pending_prompts
            .iter()
            .any(|p| p.text == "hello")
            || agent.prompt.text() == "hello",
        "the first prompt must be queued or still in the composer after reveal"
    );
}

#[test]
fn hidden_home_session_is_not_the_active_agent() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(
        get_active_agent(&app).is_none(),
        "dispatch must not treat the unused home session as the session on screen"
    );
    assert!(app.active_agent().is_none());
    assert_eq!(
        app.home_session().map(|a| a.session.id),
        Some(AgentId(0)),
        "home_session is the explicit accessor"
    );
}

#[test]
fn explicit_new_session_from_welcome_leaves_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "Ctrl+N / explicit new must leave home, got {:?}",
        app.active_view
    );
    assert!(app.home_session_agent.is_none());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "explicit new creates a session, got {effects:?}"
    );
}

#[test]
fn drain_with_initial_prompt_does_not_create_home_session() {
    let mut app = test_app();
    app.deferred_startup.prompt = Some("from cli".into());

    let effects = drain_startup_actions(&mut app);
    let creates = effects
        .iter()
        .filter(|e| matches!(e, Effect::CreateSession { .. }))
        .count();
    assert_eq!(
        creates, 1,
        "CLI prompt must create one session, not home then another, got {effects:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
    assert!(app.home_session_agent.is_none());
}

#[test]
fn drain_with_always_prompt_does_not_orphan_home_create() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    app.deferred_startup.prompt = Some("from cli".into());

    let effects = drain_startup_actions(&mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "Always + grok \"prompt\" must not emit an in-cwd home CreateSession, got {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "Always + grok \"prompt\" must isolate, got {effects:?}"
    );
    assert_eq!(app.agents.len(), 1);
    assert!(app.home_session_agent.is_none());
}

#[test]
fn session_created_after_abandon_unregisters() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    let _ = dispatch(Action::NewSession, &mut app);
    assert!(!app.agents.contains_key(&home));

    let effects =
        handle_session_created(&mut app, home, acp::SessionId::new("late-home"), None, None);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::UnregisterActiveSession { session_id } if session_id.0.as_ref() == "late-home"
        )),
        "late SessionCreated for an abandoned home must unregister, got {effects:?}"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::DeleteSession { session_id, after: crate::app::actions::AfterSessionDelete::UnusedHusk, .. }
                if session_id == "late-home"
        )),
        "late SessionCreated for an abandoned home must delete the unused dir, got {effects:?}"
    );
}

#[test]
fn drain_does_not_create_home_session_when_resuming() {
    let mut app = test_app();
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "resume-me".into(),
            session_cwd: None,
            chat_kind: false,
        });

    let effects = drain_startup_actions(&mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. })),
        "resume must win, got {effects:?}"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "optimistic create must not race a resume"
    );
}

#[test]
fn home_session_create_failure_clears_placeholder_keeps_draft_and_warns() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt_focused = true;
    app.welcome_prompt.set_text("keep me");
    let home = app.home_session_agent.expect("home session");

    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: home,
            error: "No space left on device".into(),
            timed_out: false,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(app.home_session_agent.is_none());
    assert!(app.agents.is_empty());
    assert_eq!(app.welcome_prompt.text(), "keep me");
    assert!(
        app.startup_warnings
            .iter()
            .any(|w| w.message.contains("No space left on device")),
        "create failure must stay visible on home, got {:?}",
        app.startup_warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect::<Vec<_>>(),
    );

    let effects = leave_home_with(&mut app, &key_event(KeyCode::Char('!'), KeyModifiers::NONE));
    assert!(
        creates_session(&effects),
        "after a failed create, the next keystroke must retry, got {effects:?}"
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("retry must leave home, got {:?}", app.active_view);
    };
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    let text = agent.prompt.text();
    assert!(
        text.contains("keep me") && text.contains('!'),
        "got {text:?}"
    );
}

#[test]
fn send_after_home_session_create_failure_creates_and_sends() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("hello after fail");
    let home = app.home_session_agent.expect("home session");
    dispatch(
        Action::TaskComplete(TaskResult::SessionFailed {
            agent_id: home,
            error: "create failed".into(),
            timed_out: false,
        }),
        &mut app,
    );

    let effects = dispatch(Action::SendPrompt("hello after fail".into()), &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "send after a failed create must still start a session, got {:?}",
        app.active_view
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "expected a retry CreateSession, got {effects:?}"
    );
}

fn bind_home_session(app: &mut AppView) {
    let home = app.home_session_agent.expect("home session");
    let _ = handle_session_created(app, home, acp::SessionId::new("home-sid"), None, None);
}

#[test]
fn session_scoped_slash_from_home_needs_bound_session() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let _ = dispatch(Action::SendPrompt("/context".into()), &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("slash submit reveals home, got {:?}", app.active_view);
    };
    assert!(
        last_system_text(&app, id).contains("No active session"),
        "unbound home must not pretend /context works, got {:?}",
        last_system_text(&app, id)
    );
}

#[test]
fn session_scoped_slash_from_home_works_once_bound() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);

    let effects = dispatch(Action::SendPrompt("/context".into()), &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "slash submit leaves home, got {:?}",
        app.active_view
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ShowContextInfo { .. }))
            || app.agents.values().any(|a| matches!(
                a.active_modal,
                Some(crate::views::modal::ActiveModal::UsageInfo { .. })
            )),
        "/context after SessionCreated must open context, got {effects:?}"
    );
}

#[test]
fn session_free_slash_from_home_works_before_session_created() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let _ = dispatch(Action::SendPrompt("/help".into()), &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("/help must reveal home, got {:?}", app.active_view);
    };
    assert!(
        app.agents.get(&id).is_some_and(|a| matches!(
            a.active_modal,
            Some(crate::views::modal::ActiveModal::CommandPalette { .. })
        )),
        "/help must open the palette without a bound session_id"
    );
}

#[test]
fn settings_slash_from_home_opens_settings() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let _ = dispatch(Action::SendPrompt("/settings".into()), &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("/settings must reveal home, got {:?}", app.active_view);
    };
    assert!(
        app.agents.get(&id).is_some_and(|a| matches!(
            a.active_modal,
            Some(crate::views::modal::ActiveModal::Settings { .. })
        )),
        "/settings must open the modal"
    );
}

#[test]
fn session_info_and_rename_from_home_after_bind() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);

    let info = dispatch(Action::SendPrompt("/session-info".into()), &mut app);
    assert!(
        info.iter()
            .any(|e| matches!(e, Effect::ShowSessionInfo { .. }))
            || app.agents.values().any(|a| matches!(
                a.active_modal,
                Some(crate::views::modal::ActiveModal::UsageInfo { .. })
            )),
        "/session-info must run once the home session is bound, got {info:?}"
    );

    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let rename = dispatch(Action::SendPrompt("/rename my title".into()), &mut app);
    assert!(
        rename
            .iter()
            .any(|e| matches!(e, Effect::RenameSession { title, .. } if title == "my title")),
        "/rename must apply on the revealed home session, got {rename:?}"
    );
}

#[test]
fn welcome_slash_offers_dashboard_when_enabled() {
    let app = test_app();
    assert!(
        crate::views::dashboard::dashboard_enabled(),
        "test fixture assumes the dashboard flag is on"
    );
    assert!(
        app.welcome_prompt
            .slash_controller
            .registry()
            .get("dashboard")
            .is_some(),
        "/dashboard must appear on home the same way /help does"
    );
}

#[test]
fn welcome_slash_then_tab_completes_on_revealed_session() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    app.welcome_prompt_focused = true;

    let _ = leave_home_with(&mut app, &key_event(KeyCode::Char('/'), KeyModifiers::NONE));
    assert!(app.agents.get(&home).is_some_and(|a| a.prompt.slash_open()));

    let _ = app.handle_input(&key_event(KeyCode::Char('h'), KeyModifiers::NONE));
    let _ = app.handle_input(&key_event(KeyCode::Char('e'), KeyModifiers::NONE));
    let _ = app.handle_input(&key_event(KeyCode::Tab, KeyModifiers::NONE));
    let Some(agent) = app.agents.get(&home) else {
        panic!("expected agent {home:?}");
    };
    let text = agent.prompt.text();
    assert!(
        text.starts_with("/he"),
        "Tab must accept a completion, got {text:?}"
    );
}

#[test]
fn get_active_agent_ignores_home_session_on_dashboard() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.active_view = ActiveView::AgentDashboard;
    assert!(
        get_active_agent(&app).is_none(),
        "dashboard must not treat the hidden home session as active"
    );
}

#[test]
fn reveal_home_session_keeps_agent_composer_setup() {
    let mut app = test_app();
    app.appearance.prompt.compact = true;
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("keep draft");
    assert!(
        app.agents
            .get(&AgentId(0))
            .is_some_and(|a| a.prompt.compact()),
        "new-session setup stamps compact on the hidden agent"
    );
    assert!(!app.welcome_prompt.compact());

    dispatch(Action::SendPrompt("keep draft".into()), &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("send must reveal the home session");
    };
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    assert!(
        agent.prompt.compact(),
        "reveal must not drop compact onto the welcome widget"
    );
    assert!(
        agent
            .session
            .pending_prompts
            .iter()
            .any(|p| p.text == "keep draft")
            || agent.prompt.text() == "keep draft",
        "typed draft must survive reveal"
    );
    // Only the draft moves; the agent's configured widget is not parked on Welcome.
    assert!(!app.welcome_prompt.compact());
    assert!(app.welcome_prompt.text().is_empty());
}

#[test]
fn explicit_new_session_from_welcome_drops_unused_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    let home = app.home_session_agent.expect("home session");
    if let Some(agent) = app.agents.get_mut(&home) {
        agent.session.session_id = Some(acp::SessionId::new("home-sid"));
    }

    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "Ctrl+N must leave home, got {:?}",
        app.active_view
    );
    assert!(app.home_session_agent.is_none());
    assert_eq!(
        app.agents.len(),
        1,
        "unused empty home session must not linger beside the new one"
    );
    assert!(
        !app.agents.contains_key(&home),
        "hidden home agent must be dropped"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "explicit new creates a session, got {effects:?}"
    );
}

#[test]
fn explicit_new_session_from_welcome_takes_home_draft() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("keep me");
    dispatch(Action::NewSession, &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("Ctrl+N must leave home");
    };
    assert_eq!(
        app.agents.get(&id).map(|a| a.prompt.text()),
        Some("keep me")
    );
    assert!(app.welcome_prompt.text().is_empty());
}

#[test]
fn new_session_from_welcome_honors_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    assert!(
        maybe_create_home_session(&mut app).is_empty(),
        "Always must not create an in-cwd husk"
    );

    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "Always worktree must not be skipped because the hidden home session has no branch, got {effects:?}"
    );
}

#[test]
fn send_from_welcome_honors_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("hello");

    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "first send from home must isolate when Always, got {effects:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
}

/// Vim input mode parks an empty prompt on Scrollback; the worktree path must
/// still land the forwarded first keystroke in the composer.
#[test]
fn keystroke_from_welcome_honors_always_worktree_under_vim() {
    crate::appearance::cache::set_simple_mode(false);
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    assert!(maybe_create_home_session(&mut app).is_empty());

    let effects = leave_home_with(&mut app, &key_event(KeyCode::Char('h'), KeyModifiers::NONE));
    crate::appearance::cache::set_simple_mode(true);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "first keystroke from home must isolate when Always, got {effects:?}"
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("keystroke must leave home, got {:?}", app.active_view);
    };
    assert_eq!(app.agents.get(&id).map(|a| a.prompt.text()), Some("h"));
}

#[test]
fn keystroke_from_welcome_in_chat_mode_creates_chat_session() {
    let mut app = test_app();
    app.chat_mode = true;
    assert!(maybe_create_home_session(&mut app).is_empty());

    let effects = leave_home_with(&mut app, &key_event(KeyCode::Char('h'), KeyModifiers::NONE));
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::CreateSession {
                chat_kind: true,
                ..
            }
        )),
        "got {effects:?}"
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("keystroke must leave home, got {:?}", app.active_view);
    };
    assert_eq!(app.agents.get(&id).map(|a| a.prompt.text()), Some("h"));
}

#[test]
fn initial_prompt_from_welcome_honors_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    maybe_create_home_session(&mut app);

    let effects =
        crate::app::dispatch::prompt::dispatch_initial_prompt(&mut app, "from cli".into());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "grok \"prompt\" with Always must isolate, got {effects:?}"
    );
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
}

#[test]
fn voice_disabled_on_welcome_does_not_leave_home() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.voice_mode_enabled = false;
    let effects = dispatch(Action::EnableVoiceMode, &mut app);
    assert!(effects.is_empty());
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(app.home_session_agent.is_some());
}

#[test]
fn voice_on_welcome_honors_always_worktree() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    assert!(maybe_create_home_session(&mut app).is_empty());
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_cmd_tx = Some(tx);

    let effects = dispatch(Action::EnableVoiceMode, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "voice on home with Always must isolate, got {effects:?}"
    );
    assert!(app.home_session_agent.is_none());
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
}

#[test]
fn voice_on_welcome_after_exit_starts_a_session() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    crate::app::dispatch::session::lifecycle::reveal_home_session(&mut app);
    assert!(matches!(app.active_view, ActiveView::Agent(_)));
    let _ = dispatch(Action::ExitSession, &mut app);
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(!app.agents.is_empty(), "exit leaves the prior agent");
    assert!(app.home_session_agent.is_none());

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_cmd_tx = Some(tx);
    dispatch(Action::EnableVoiceMode, &mut app);
    assert!(
        matches!(app.active_view, ActiveView::Agent(_)),
        "voice after /exit must start a session, got {:?}",
        app.active_view
    );
    if distill_voice::AUDIO_SUPPORTED {
        assert!(app.voice_listening(), "capture must start");
    }
}

#[test]
fn worktree_create_failure_restores_queued_prompt_to_welcome() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    app.welcome_prompt.set_text("keep the worktree prompt");

    let _ = dispatch(
        Action::SendPrompt("keep the worktree prompt".into()),
        &mut app,
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("Always send must leave home");
    };

    let _ = handle_worktree_session_failed(&mut app, id, "worktree add failed".into(), None, false);
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert_eq!(app.welcome_prompt.text(), "keep the worktree prompt");
    assert!(
        app.startup_warnings
            .iter()
            .any(|w| w.message.contains("worktree add failed")),
        "worktree create failure must stay visible, got {:?}",
        app.startup_warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn welcome_shift_tab_off_yolo_notifies_after_session_created() {
    let mut app = test_app();
    app.default_yolo = true;
    maybe_create_home_session(&mut app);
    let home_id = app.home_session_agent.expect("home session");
    assert!(
        app.agents
            .get(&home_id)
            .is_some_and(|a| a.session.is_yolo())
    );

    let persist = leave_home_with(&mut app, &key_event(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(
        persist.iter().any(|e| matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "ask",
                session_id: None,
                ..
            }
        )),
        "pre-bind cycle persists ask without a session id, got {persist:?}"
    );
    let Some(home) = app.agents.get(&home_id) else {
        panic!("expected agent {home_id:?}");
    };
    assert!(!home.session.is_yolo());
    assert_eq!(home.deferred_permission_mode, Some("ask"));

    // A destructive prompt queued before the bind must not reach the shell ahead of the mode change.
    let _ = dispatch(Action::SendPrompt("rm -rf build".into()), &mut app);
    let effects = handle_session_created(
        &mut app,
        home_id,
        acp::SessionId::new("yolo-home"),
        None,
        None,
    );
    let notify_at = effects.iter().position(|e| {
        matches!(
            e,
            Effect::PersistPermissionMode {
                canonical: "ask",
                session_id: Some(sid),
                ..
            } if sid.0.as_ref() == "yolo-home"
        )
    });
    let send_at = effects
        .iter()
        .position(|e| matches!(e, Effect::SendPrompt { .. }));
    assert!(
        matches!((notify_at, send_at), (Some(n), Some(s)) if n < s),
        "yolo_mode_changed must precede the drained SendPrompt, got {effects:?}"
    );
    assert!(
        app.agents
            .get(&home_id)
            .is_some_and(|a| a.deferred_permission_mode.is_none())
    );
}

#[test]
fn session_free_slash_from_always_welcome_keeps_create_effect() {
    let mut app = test_app_git();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Always;
    assert!(maybe_create_home_session(&mut app).is_empty());

    let effects = dispatch(Action::SendPrompt("/help".into()), &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateWorktreeSession { .. })),
        "/help on Always Welcome must not drop CreateWorktreeSession, got {effects:?}"
    );
    let ActiveView::Agent(id) = app.active_view else {
        panic!("/help must leave home, got {:?}", app.active_view);
    };
    assert!(
        app.agents.get(&id).is_some_and(|a| matches!(
            a.active_modal,
            Some(crate::views::modal::ActiveModal::CommandPalette { .. })
        )),
        "/help must open the palette"
    );
}

#[test]
fn bound_home_abandon_deletes_unused_husk() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    bind_home_session(&mut app);
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::DeleteSession {
                session_id,
                after: crate::app::actions::AfterSessionDelete::UnusedHusk,
                ..
            } if session_id == "home-sid"
        )),
        "abandoning a bound unused husk must delete it, got {effects:?}"
    );
}

#[test]
fn create_fail_restores_all_queued_prompts_and_draft() {
    let mut app = test_app();
    maybe_create_home_session(&mut app);
    app.welcome_prompt.set_text("first");
    let _ = dispatch(Action::SendPrompt("first".into()), &mut app);
    let ActiveView::Agent(id) = app.active_view else {
        panic!("send must leave home");
    };
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .session
            .enqueue_prompt_with_skill_tokens("second".into(), Vec::new());
        agent.prompt.set_text("third");
    }

    let _ = handle_session_failed(&mut app, id, "disk full".into(), false);
    assert!(matches!(app.active_view, ActiveView::Welcome));
    let restored = app.welcome_prompt.text();
    assert!(
        restored.contains("first") && restored.contains("second") && restored.contains("third"),
        "every queued prompt and the leftover draft must return, got {restored:?}"
    );
}
