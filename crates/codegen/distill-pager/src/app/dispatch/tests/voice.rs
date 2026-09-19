// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for voice mode enable, toggle, and stop dispatchers.

use super::*;

/// Plan mode must not gate voice.
/// Typing `/voice` and Enter through the real input path (prompt keys, then the slash registry, then dispatch) starts recording.
/// With `plan_mode_active` set it behaves exactly like normal mode.
#[test]
fn voice_slash_submit_starts_recording_in_plan_mode() {
    use crate::app::app_view::InputOutcome;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    // The production reveal path flips the flag AND the `/voice` visibility in each slash registry together
    app.apply_voice_mode_enabled(true);
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.plan_mode_active = true;
    agent.set_active_pane(crate::views::agent::ActivePane::Prompt, false);

    for ch in "/voice".chars() {
        app.handle_input(&Event::Key(KeyEvent::new(
            KeyCode::Char(ch),
            KeyModifiers::NONE,
        )));
    }
    let out = app.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let InputOutcome::Action(action) = out else {
        panic!("Enter on /voice must produce a submit action, got {out:?}");
    };
    dispatch(action, &mut app);
    assert!(
        app.voice_listening(),
        "typed /voice + Enter must start recording in plan mode"
    );
}

#[test]
fn voice_on_welcome_noop_when_startup_gated() {
    // Auth or folder trust unresolved: voice must not create a session (that would bypass the startup gate)
    // It stays a silent no-op on welcome
    let mut app = test_app();
    app.auth_state = AuthState::Pending { error: None };
    app.voice_mode_enabled = true;
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);

    dispatch(Action::EnableVoiceMode, &mut app);

    assert!(app.agents.is_empty(), "no session created while gated");
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(!app.voice_listening());
}

#[test]
fn voice_final_appends_to_prompt_with_single_space() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Stopping {
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello");
    p.set_cursor(5);
    let redraw = crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "world".into(),
        },
    );
    assert!(redraw);
    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(p.text(), "hello world");
    assert_eq!(p.cursor(), "hello world".len());
}

#[test]
fn voice_final_inserts_at_mid_text_cursor() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("partial".into()),
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.set_cursor(5); // Cursor after "hello"

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "again".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    // Text should be inserted at cursor position with smart spacing
    assert_eq!(p.text(), "hello again world");
    // Cursor should be after the inserted text (" again")
    assert_eq!(p.cursor(), 5 + " again".len()); // 11
    assert!(app.voice_listening());
    assert!(app.voice_interim().is_none());
}

#[test]
fn voice_final_inserts_at_start_of_text() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("world");
    p.set_cursor(0); // Cursor at start

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "hello".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(p.text(), "hello world");
    assert_eq!(p.cursor(), "hello ".len());
}

#[test]
fn voice_final_inserts_between_words_with_spacing() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("helloworld");
    p.set_cursor(5); // Cursor between "hello" and "world"

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "there".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    // Should add spaces on both sides since neither side has whitespace
    assert_eq!(p.text(), "hello there world");
    assert_eq!(p.cursor(), "hello there ".len());
}

#[test]
fn voice_final_inserts_after_existing_space() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.set_cursor(6); // Cursor after "hello "

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "there".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    // Should not add leading space since cursor is after existing space
    assert_eq!(p.text(), "hello there world");
    assert_eq!(p.cursor(), "hello there ".len());
}

#[test]
fn voice_final_replaces_select_all_without_leading_space() {
    // Select-all then dictate should replace without adding a leading space
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("old text");
    p.set_cursor(0);
    // Simulate select-all via textarea
    p.textarea.set_selection(0, "old text".len());

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "new text".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(
        p.text(),
        "new text",
        "select-all replace must not add leading space"
    );
}

#[test]
fn voice_final_replaces_mid_word_selection_with_spacing() {
    // Replace a mid-word selection, spacing from neighbors outside the selection
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    // Select "worl" (positions 6-10)
    p.set_cursor(6);
    p.textarea.set_selection(6, 10);

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "there".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    // "hello " + "there" + "d" - leading space from 'o', trailing space from 'd'
    assert_eq!(p.text(), "hello there d");
}

#[test]
fn voice_final_no_selection_mid_cursor_insert_unchanged() {
    // Regression: no-selection mid-cursor insert still works
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.set_cursor(5); // After "hello"

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "there".into(),
        },
    );

    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(p.text(), "hello there world");
}

#[test]
fn voice_final_into_empty_prompt_has_no_leading_space() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Stopping {
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "hi there".into(),
        },
    );
    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(p.text(), "hi there");
    assert_eq!(p.cursor(), "hi there".len());
}

#[test]
fn voice_final_replaces_whitespace_only_draft() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Stopping {
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("  \n");
    p.set_cursor(0);
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal { text: "hi".into() },
    );
    let p = &app.agents.get(&id).unwrap().prompt;
    assert_eq!(p.text(), "hi");
    assert_eq!(p.cursor(), 2);
}

#[test]
fn voice_final_preserves_trailing_newline() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Stopping {
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("line one\n");
    p.set_cursor("line one\n".len()); // Cursor at end after newline
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "line two".into(),
        },
    );
    // Newline preserved: dictation lands on the new line.
    assert_eq!(
        app.agents.get(&id).unwrap().prompt.text(),
        "line one\nline two"
    );
}

/// A Ctrl+Space release ends only a session a Ctrl+Space hold started.
/// A recording from `/voice` or a Ctrl+Space toggle is left running; its release isn't ours.
#[test]
fn voice_ctrl_space_release_leaves_toggle_recording_running() {
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_cmd_tx = Some(tx);

    // Ctrl+Space starts a toggle recording (no hold-ownership).
    dispatch(Action::VoiceToggle, &mut app);
    assert!(app.voice_listening());
    assert!(!app.voice_state.hold());
    let _ = rx.try_recv(); // drain the PttPress

    // A stray Ctrl+Space release must not stop it.
    dispatch(Action::VoiceStop, &mut app);
    assert!(
        app.voice_listening(),
        "Ctrl+Space release must not stop a toggle session"
    );
    assert!(
        rx.try_recv().is_err(),
        "no PttRelease for a non-hold session"
    );
}

/// A free-tier user hitting the voice keybinding gets the SuperGrok upsell instead of a doomed voice session.
/// The keybinding bypasses the slash registry, so this dispatcher is the enforcement point.
#[test]
fn voice_keybinding_on_restricted_tier_opens_upsell() {
    if !distill_voice::AUDIO_SUPPORTED {
        return; // The tier check runs after the AUDIO_SUPPORTED gate.
    }
    let mut app = test_app_with_agent();
    app.voice_mode_enabled = true;
    // A personal login without a subscription tier is free tier, so voice is restricted
    app.apply_auth_meta(&distill_login::AuthMeta::default());
    assert!(app.is_voice_tier_restricted());

    dispatch(Action::EnableVoiceMode, &mut app);

    assert!(
        app.agents.get(&AgentId(0)).unwrap().question_view.is_some(),
        "restricted-tier voice keybinding must open the SuperGrok upsell"
    );
    assert!(
        !app.voice_listening(),
        "voice must not start on a restricted tier"
    );
}

/// A paid-tier user's voice keybinding is not intercepted by the tier gate.
#[test]
fn voice_keybinding_on_paid_tier_not_gated() {
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    app.voice_mode_enabled = true;
    let meta = distill_login::AuthMeta {
        subscription_tier: Some("SuperGrok".into()),
        ..Default::default()
    };
    app.apply_auth_meta(&meta);
    assert!(!app.is_voice_tier_restricted());

    dispatch(Action::EnableVoiceMode, &mut app);

    // No upsell modal; the paid user proceeds down the normal voice path
    assert!(
        app.agents.get(&AgentId(0)).unwrap().question_view.is_none(),
        "paid-tier voice must not be intercepted by the tier gate"
    );
}

#[test]
fn voice_interim_sets_then_error_clears_state() {
    let mut app = test_app_with_agent();
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(AgentId(0)),
        interim: None,
    };
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::InterimTranscript {
            text: "partial".into(),
        },
    );
    assert_eq!(app.voice_interim(), Some("partial"));

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::Error {
            message: "boom".into(),
            hint: None,
        },
    );
    assert!(!app.voice_listening());
    assert!(app.voice_interim().is_none());
}

#[test]
fn voice_interim_preview_at_mid_prompt_cursor() {
    use crate::views::prompt_widget::{PromptStyle, VoicePromptOverlay};
    use ratatui::style::{Color, Modifier};
    use ratatui::{buffer::Buffer, layout::Rect};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.set_cursor(5); // Caret after "hello", on the space.

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::InterimTranscript {
            text: "there".into(),
        },
    );

    // The interim is overlay-only: it must not mutate the draft text or move the caret.
    assert_eq!(app.voice_interim(), Some("there"));
    {
        let p = &app.agents.get(&id).unwrap().prompt;
        assert_eq!(p.text(), "hello world", "interim must not mutate the draft");
        assert_eq!(p.cursor(), 5, "interim must not move the caret");
    }

    // Render the bound prompt and assert the interim previews AT the caret (in the italic overlay
    // style) while the text after the caret stays visible — shifted right, not overwritten.
    let interim_text = app.voice_interim().unwrap().to_string();
    let style = PromptStyle {
        focused: true,
        show_prefix: false,
        vpad_top: 0,
        chrome: false,
        ..Default::default()
    };
    let area = Rect::new(0, 0, 40, 3);
    let mut buf = Buffer::empty(area);
    let overlay = VoicePromptOverlay {
        interim: Some(interim_text.as_str()),
        color: Color::Cyan,
    };
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .draw(&mut buf, area, None, &style, None, Some(overlay));

    let row: String = (0..area.width)
        .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
        .collect();
    assert_eq!(
        row.trim_end(),
        "hello there world",
        "interim must insert at the caret and keep the trailing text visible"
    );
    // The ghost fragment carries the italic interim style; 't' of "there" sits at column 6.
    let ghost = buf.cell((6, 0)).unwrap();
    assert_eq!(ghost.symbol(), "t");
    assert!(
        ghost.style().add_modifier.contains(Modifier::ITALIC),
        "interim preview must use the italic overlay style"
    );
    // The real draft after the caret keeps its normal (non-italic) style.
    let tail = buf.cell((12, 0)).unwrap();
    assert_eq!(tail.symbol(), "w");
    assert!(!tail.style().add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn voice_interim_preview_replaces_active_selection() {
    // Dictating over a highlight must preview a replace of that span (same as the live insert),
    // not an insert at the caret into the selected text.
    use crate::views::prompt_widget::{PromptStyle, VoicePromptOverlay};
    use ratatui::style::{Color, Modifier};
    use ratatui::{buffer::Buffer, layout::Rect};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.textarea.set_selection(0, 5); // "hello" selected

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::InterimTranscript { text: "hi".into() },
    );

    assert_eq!(app.voice_interim(), Some("hi"));
    {
        let p = &app.agents.get(&id).unwrap().prompt;
        assert_eq!(p.text(), "hello world", "interim must not mutate the draft");
        assert_eq!(
            p.selection_range(),
            Some(0..5),
            "interim must not clear the selection"
        );
    }

    let interim_text = app.voice_interim().unwrap().to_string();
    let style = PromptStyle {
        focused: true,
        show_prefix: false,
        vpad_top: 0,
        chrome: false,
        ..Default::default()
    };
    let area = Rect::new(0, 0, 40, 3);
    let mut buf = Buffer::empty(area);
    let overlay = VoicePromptOverlay {
        interim: Some(interim_text.as_str()),
        color: Color::Cyan,
    };
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .draw(&mut buf, area, None, &style, None, Some(overlay));

    let row: String = (0..area.width)
        .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
        .collect();
    assert_eq!(
        row.trim_end(),
        "hi world",
        "interim must preview replacing the selection, not inserting into it"
    );
    let ghost = buf.cell((0, 0)).unwrap();
    assert_eq!(ghost.symbol(), "h");
    assert!(
        ghost.style().add_modifier.contains(Modifier::ITALIC),
        "replacement preview must use the italic overlay style"
    );
    let tail = buf.cell((3, 0)).unwrap();
    assert_eq!(tail.symbol(), "w");
    assert!(!tail.style().add_modifier.contains(Modifier::ITALIC));
}

#[test]
fn commit_interim_replaces_active_selection() {
    // Leftover-interim submit records the selection span so merge_voice_fragment
    // drops the highlighted text the same way the live insert does.
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("hi".into()),
    };
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("hello world");
    p.textarea.set_selection(0, 5);

    let commit = crate::voice::commit_interim_into_prompt(&mut app).expect("commit");
    assert_eq!(commit.fragment, "hi");
    assert_eq!(commit.replace, Some(0..5));
    assert_eq!(
        app.agents.get(&id).unwrap().prompt.text(),
        "hi world",
        "commit must replace the selection, not insert into it"
    );
}

#[test]
fn voice_error_hint_lands_in_bound_agent_scrollback() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let before = app.agents.get(&id).unwrap().scrollback.len();

    // Hint follows the bound target (like finals), not the active view.
    app.active_view = ActiveView::AgentDashboard;
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::Error {
            message: "no speech detected".into(),
            hint: Some("allow terminal mic access in system settings".into()),
        },
    );
    let agent = app.agents.get(&id).unwrap();
    assert_eq!(agent.scrollback.len(), before + 1);
    let text = match agent
        .scrollback
        .get(agent.scrollback.len() - 1)
        .map(|e| &e.block)
    {
        Some(crate::scrollback::block::RenderBlock::System(b)) => b.text.as_str(),
        other => panic!("expected system hint block, got {other:?}"),
    };
    assert!(
        text.contains("no speech detected")
            && text.contains("allow terminal mic access in system settings"),
        "scrollback should carry short message + long hint, got {text:?}"
    );

    // No hint while still bound: toast only, no scrollback growth
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: None,
    };
    let before = app.agents.get(&id).unwrap().scrollback.len();
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::Error {
            message: "boom".into(),
            hint: None,
        },
    );
    assert_eq!(app.agents.get(&id).unwrap().scrollback.len(), before);
}

#[test]
fn voice_error_hint_dropped_for_dashboard_dispatch() {
    // The dispatch box has no scrollback; only the dashboard toast survives.
    let mut app = test_app_with_agent();
    app.active_view = ActiveView::AgentDashboard;
    ensure_dashboard_state(&mut app);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::DashboardDispatch,
        interim: None,
    };
    let before = app.agents.get(&AgentId(0)).unwrap().scrollback.len();
    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::Error {
            message: "no speech detected".into(),
            hint: Some("allow terminal mic access in system settings".into()),
        },
    );
    assert_eq!(
        app.agents.get(&AgentId(0)).unwrap().scrollback.len(),
        before
    );
    assert!(
        app.dashboard
            .as_ref()
            .is_some_and(|d| d.error_toast.is_some())
    );
}

#[test]
fn voice_interim_ignored_after_stop() {
    // Late interim events that arrive after recording stopped must not repopulate the overlay
    let mut app = test_app_with_agent();
    app.voice_state = VoiceState::Idle; // Not recording, so interim is None
    let redraw = crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::InterimTranscript {
            text: "late".into(),
        },
    );
    assert!(!redraw, "stale interim event must not request a redraw");
    assert!(
        app.voice_interim().is_none(),
        "stale interim event must not set voice_interim"
    );
}

#[test]
fn voice_interim_kept_on_stop_then_cleared_by_final() {
    // An explicit stop keeps the last interim on screen (no flicker) until the trailing final commits it; the final then clears the interim
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("partial".into()),
    };

    app.voice_stop_keeping_final();
    assert!(!app.voice_listening());
    assert_eq!(
        app.voice_interim(),
        Some("partial"),
        "stop keeps the interim"
    );

    crate::voice::handle_voice_event(
        &mut app,
        distill_voice::VoiceEvent::UtteranceFinal {
            text: "partial".into(),
        },
    );
    assert_eq!(app.voice_interim(), None, "final clears the interim");
    assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "partial");
}

#[test]
fn voice_toggle_starts_and_stops() {
    // Starting routes through the `/voice` gate, which requires compiled-in audio capture; skip on builds without a `cpal` backend
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_ui_active = true;
    app.voice_cmd_tx = Some(tx);

    dispatch(Action::VoiceToggle, &mut app);
    assert!(app.voice_listening());
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttPress)
    ));

    dispatch(Action::VoiceToggle, &mut app);
    assert!(!app.voice_listening());
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttRelease)
    ));
}

#[test]
fn voice_toggle_silent_no_op_when_flag_disabled() {
    // With the voice gate off (kill switch or env force-off), the voice key is a silent no-op: no recording, and no toast
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = false;
    app.voice_cmd_tx = Some(tx);
    dispatch(Action::VoiceToggle, &mut app);
    assert!(
        !app.voice_listening(),
        "must not start recording when the flag is off"
    );
    assert!(
        !app.voice_ui_active,
        "voice mode must not arm with flag off"
    );
    assert!(rx.try_recv().is_err(), "no PttPress with flag off");
}

#[test]
fn voice_toggle_starts_without_voice_mode_prereq() {
    // Ctrl+Space is a direct start; it no longer requires `/voice` first
    // Skip when audio capture isn't compiled in (see sibling test).
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_ui_active = false;
    app.voice_cmd_tx = Some(tx);
    dispatch(Action::VoiceToggle, &mut app);
    assert!(app.voice_ui_active, "Ctrl+Space enables voice mode");
    assert!(
        app.voice_listening(),
        "Ctrl+Space starts recording without a /voice prerequisite"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttPress)
    ));
}

#[test]
fn voice_mode_enable_starts_recording_and_stays_on() {
    // `/voice` gates on compiled-in audio capture; skip when the build has no `cpal` backend (e.g. Bazel or headless), where enabling is a no-op.
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_mode_enabled = true;
    app.voice_cmd_tx = Some(tx);

    // `EnableVoiceMode` (the Ctrl+Space hold-press start) begins recording when the pipeline is already up
    dispatch(Action::EnableVoiceMode, &mut app);
    assert!(app.voice_ui_active);
    assert!(app.voice_listening(), "start begins recording");
    assert!(
        !app.voice_state.pending_cold_start(),
        "pipeline already up — no re-request"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttPress)
    ));

    // `EnableVoiceMode` is start-only (not a toggle): running it again while already recording is idempotent, no stop, no second PttPress
    dispatch(Action::EnableVoiceMode, &mut app);
    assert!(app.voice_ui_active, "start never turns voice mode off");
    assert!(app.voice_listening());
    assert!(
        rx.try_recv().is_err(),
        "no second PttPress while already recording"
    );
}

#[test]
fn voice_mode_on_requests_lazy_pipeline_when_missing() {
    // Skip when audio capture isn't compiled in (see sibling test).
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    app.voice_mode_enabled = true;
    // No voice_cmd_tx: the first /voice asks the event loop to spawn
    dispatch(Action::EnableVoiceMode, &mut app);
    assert!(app.voice_ui_active);
    assert!(
        app.voice_state.pending_cold_start(),
        "event loop should spawn the pipeline and auto-start capture"
    );
}

#[test]
fn voice_toggle_while_spawn_pending_keeps_start_armed() {
    // A second Ctrl+Space while the pipeline is still spawning re-affirms the queued start rather than cancelling it
    // There's no visible recording yet to toggle off
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    app.voice_mode_enabled = true;
    app.voice_state = VoiceState::ColdStart {
        hold: false,
        target: VoiceTarget::Agent(AgentId(0)),
    };

    dispatch(Action::VoiceToggle, &mut app);
    assert!(
        app.voice_state.pending_cold_start(),
        "Ctrl+Space re-affirms the queued auto-start (does not cancel it)"
    );
    assert!(!app.voice_listening());
}

#[test]
fn voice_toggle_preserves_pending_ctrl_space_hold_cancel() {
    // A Ctrl+Space quick-tap queues a hold-owned cold-start
    // A Ctrl+Space toggle arriving before the pipeline spawns must re-affirm it without clearing hold-ownership
    // The matching Ctrl+Space release then still cancels the tap
    if !distill_voice::AUDIO_SUPPORTED {
        return;
    }
    let mut app = test_app_with_agent();
    app.voice_mode_enabled = true;
    app.voice_state = VoiceState::ColdStart {
        hold: true,
        target: VoiceTarget::Agent(AgentId(0)),
    };

    dispatch(Action::VoiceToggle, &mut app);
    assert!(
        app.voice_state.hold(),
        "toggle must not clear the Ctrl+Space hold-ownership"
    );

    dispatch(Action::VoiceStop, &mut app);
    assert!(
        !app.voice_state.pending_cold_start(),
        "the matching Ctrl+Space release still cancels the queued tap"
    );
}

#[test]
fn voice_toggle_can_always_stop_even_with_flag_disabled() {
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    // Recording was started while the flag was on, then it flipped off.
    app.voice_cmd_tx = Some(tx);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(AgentId(0)),
        interim: None,
    };
    app.voice_mode_enabled = false;
    app.voice_ui_active = false;

    dispatch(Action::VoiceToggle, &mut app);
    assert!(
        !app.voice_listening(),
        "must stop an active recording even with the flag off"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttRelease)
    ));
}

#[test]
fn voice_stop_stops_and_drops_pending_cold_start() {
    // The Ctrl+Space hold release stops capture and cancels a queued cold-start
    // A release that arrives while the pipeline is still spawning can't leave a hot mic running after the key is up
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    // A live recording started by a Ctrl+Space hold-press
    app.voice_state = VoiceState::Recording {
        hold: true,
        target: VoiceTarget::Agent(AgentId(0)),
        interim: None,
    };

    dispatch(Action::VoiceStop, &mut app);
    assert!(!app.voice_listening());
    assert!(!app.voice_state.pending_cold_start());
    assert!(!app.voice_state.hold());
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttRelease)
    ));
}

/// A stray Ctrl+Space release must NOT cancel a cold-start queued by `/voice` or a Ctrl+Space toggle (`hold` is false for those).
#[test]
fn voice_stop_leaves_non_hold_cold_start_armed() {
    let mut app = test_app_with_agent();
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    // Queued by /voice, not Ctrl+Space (hold inactive).
    app.voice_state = VoiceState::ColdStart {
        hold: false,
        target: VoiceTarget::Agent(AgentId(0)),
    };

    dispatch(Action::VoiceStop, &mut app);
    assert!(
        app.voice_state.pending_cold_start(),
        "a /voice cold-start must survive an unrelated Ctrl+Space release"
    );
}

/// Changing the STT language shuts down a running pipeline (it holds the VoiceConfig it was spawned with) and persists the preference.
/// A mid-recording change also ends the in-flight session so the mic indicator clears at once.
/// Otherwise it would linger until the dead pipeline's channel-close is misreported.
#[test]
fn voice_stt_language_change_recycles_pipeline() {
    let mut app = test_app_with_agent();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(AgentId(0)),
        interim: Some("hola".to_string()),
    };

    let effects = dispatch(Action::SetVoiceSttLanguage("es".to_string()), &mut app);

    assert_eq!(app.voice_config.language, "es");
    assert_eq!(app.current_ui.voice_stt_language.as_deref(), Some("es"));
    assert!(
        app.voice_cmd_tx.is_none(),
        "handle must drop so the event loop respawns with the new config"
    );
    assert!(
        !app.voice_listening(),
        "recycling the pipeline must end the in-flight session (no lingering hot mic)"
    );
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::Shutdown)
    ));
    assert!(matches!(
        effects.as_slice(),
        [Effect::PersistSetting {
            key: "voice_stt_language",
            ..
        }]
    ));
}

/// With the UI key unset and a `[voice].language` in effect, selecting the UI default (English) must still commit.
/// The rollback value is the language actually in effect, not the unset-UI default.
#[test]
fn voice_stt_language_english_commits_over_voice_config_language() {
    let mut app = test_app_with_agent();
    app.voice_config.language = "es".into(); // [voice].language; UI key unset

    let effects = dispatch(Action::SetVoiceSttLanguage("en".to_string()), &mut app);

    assert_eq!(app.voice_config.language, "en");
    assert_eq!(app.current_ui.voice_stt_language.as_deref(), Some("en"));
    assert!(matches!(
        effects.as_slice(),
        [Effect::PersistSetting {
            key: "voice_stt_language",
            value: crate::settings::SettingValue::Enum("en"),
            rollback_value: crate::settings::SettingValue::Enum("es"),
        }]
    ));
}

/// Re-selecting the persisted language is a no-op: no persist, and the running pipeline is left alone.
/// An unset UI key never no-ops; the first explicit selection always commits (pins the choice to `[ui]`).
#[test]
fn voice_stt_language_noop_keeps_pipeline() {
    let mut app = test_app_with_agent();
    app.current_ui.voice_stt_language = Some("es".to_string());
    app.voice_config.language = "es".into();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);

    let effects = dispatch(Action::SetVoiceSttLanguage("es".to_string()), &mut app);

    assert!(effects.is_empty());
    assert!(app.voice_cmd_tx.is_some(), "pipeline must survive a no-op");
    assert!(rx.try_recv().is_err());

    // An unset UI key with a matching live value still commits (pins the choice)
    // The language is unchanged, so the pipeline must NOT be recycled
    app.current_ui.voice_stt_language = None;
    let effects = dispatch(Action::SetVoiceSttLanguage("es".to_string()), &mut app);
    assert!(!effects.is_empty(), "explicit pick must persist when unset");
    assert_eq!(app.current_ui.voice_stt_language.as_deref(), Some("es"));
    assert!(
        app.voice_cmd_tx.is_some(),
        "re-pinning the same language must not cut off dictation"
    );
    assert!(
        rx.try_recv().is_err(),
        "no Shutdown when language is unchanged"
    );

    // A non-canonical stored value (hand-edited or invalid on disk) re-commits so the clean canonical is rewritten
    // Still no language change, so the pipeline survives
    app.current_ui.voice_stt_language = Some("ES!".to_string());
    let effects = dispatch(Action::SetVoiceSttLanguage("es".to_string()), &mut app);
    assert!(
        !effects.is_empty(),
        "invalid stored value must be rewritten"
    );
    assert_eq!(app.current_ui.voice_stt_language.as_deref(), Some("es"));
    assert!(
        app.voice_cmd_tx.is_some(),
        "rewriting a non-canonical mirror must not recycle the pipeline"
    );
    assert!(
        rx.try_recv().is_err(),
        "no Shutdown when language is unchanged"
    );
}

/// `auto` is stored as the preference (not a resolved locale code) so the voice crate re-resolves it from the locale on each STT connect.
#[test]
fn voice_stt_language_auto_stored_unresolved() {
    let mut app = test_app_with_agent();

    let _ = dispatch(Action::SetVoiceSttLanguage("auto".to_string()), &mut app);

    assert_eq!(app.voice_config.language, "auto");
    assert_eq!(app.current_ui.voice_stt_language.as_deref(), Some("auto"));
}

#[test]
fn voice_submit_includes_interim() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    app.voice_cmd_tx = Some(tx);
    {
        let p = &mut app.agents.get_mut(&id).unwrap().prompt;
        p.set_text("hello");
        p.set_cursor("hello".len()); // Caret at end: the leftover interim appends.
    }
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("world".into()),
    };

    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    let Some(Effect::SendPrompt { text, .. }) = effects.first() else {
        panic!("expected SendPrompt, got {effects:?}");
    };
    assert_eq!(text, "hello world");
    assert!(!app.voice_listening());
    assert!(matches!(
        rx.try_recv(),
        Ok(distill_voice::VoiceCommand::PttRelease)
    ));
}

#[test]
fn voice_merge_replaces_selection_range() {
    // Submit-time merge with a selection replaces the selected span (matching the live insert),
    // rather than keeping the selected text and inserting at the caret.
    assert_eq!(
        crate::voice::merge_voice_fragment("hello world", Some(0..5), "hi"),
        "hi world"
    );
    // An empty range at the caret inserts with smart spacing on both sides.
    assert_eq!(
        crate::voice::merge_voice_fragment("foobar", Some(3..3), "baz"),
        "foo baz bar"
    );
    // No range appends with a single space.
    assert_eq!(
        crate::voice::merge_voice_fragment("hello", None, "world"),
        "hello world"
    );
    // A blank draft is replaced outright regardless of the range.
    assert_eq!(
        crate::voice::merge_voice_fragment("   ", Some(0..3), "hi"),
        "hi"
    );
}

#[test]
fn voice_submit_interim_only() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("ghost only".into()),
    };

    let effects = dispatch(Action::SendPrompt(String::new()), &mut app);
    let Some(Effect::SendPrompt { text, .. }) = effects.first() else {
        panic!("expected SendPrompt, got {effects:?}");
    };
    assert_eq!(text, "ghost only");
    assert!(!app.voice_listening());
}

#[test]
fn voice_submit_follow_up_keeps_chip_literal() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let p = &mut app.agents.get_mut(&id).unwrap().prompt;
    p.set_text("draft");
    p.set_cursor("draft".len()); // Cursor at end for append behavior
    app.voice_state = VoiceState::Recording {
        hold: false,
        target: VoiceTarget::Agent(id),
        interim: Some("dictated".into()),
    };

    let effects = dispatch(Action::SubmitFollowUp("chip text".into()), &mut app);
    let Some(Effect::SendPrompt { text, .. }) = effects.first() else {
        panic!("expected SendPrompt, got {effects:?}");
    };
    assert_eq!(text, "chip text");
    assert_eq!(app.agents.get(&id).unwrap().prompt.text(), "draft dictated");
    assert!(!app.voice_listening());
}
