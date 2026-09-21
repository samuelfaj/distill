// Modified for Distill by Samuel Fajreldines, 2026.
//! Tests for prompt and bash submission, queueing, and interject shims.

use super::*;

fn agent_ref(app: &AppView, id: AgentId) -> &AgentView {
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    agent
}

/// Sending a prompt is a submit: it retires the active ephemeral tip.
#[test]
fn send_prompt_clears_active_ephemeral_tip() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let agent = app.agents.get_mut(&id).unwrap();
    let _ = agent.ephemeral_tip.show(
        crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint")),
        &mut std::collections::HashMap::new(),
    );
    assert!(agent.ephemeral_tip.is_active());

    let _ = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(
        !app.agents.get(&id).unwrap().ephemeral_tip.is_active(),
        "prompt submit must clear the tip"
    );
}

#[test]
fn doctor_fix_list_and_plan_dispatch_as_background_effects() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let list = dispatch_doctor(crate::slash::command::DoctorRequest::ListFixes, &mut app);
    assert!(matches!(
        list.as_slice(),
        [Effect::PlanDoctorFix {
            target,
            request: crate::slash::command::DoctorRequest::ListFixes,
            ..
        }] if target.agent_id == id
            && target.session_id == agent_ref(&app, id).session.session_id
            && target.cwd == agent_ref(&app, id).session.cwd
    ));

    let fix = dispatch_doctor(
        crate::slash::command::DoctorRequest::Fix(crate::diagnostics::SSH_WRAP_ID),
        &mut app,
    );
    assert!(matches!(
        fix.as_slice(),
        [Effect::PlanDoctorFix {
            request: crate::slash::command::DoctorRequest::Fix(id),
            ..
        }] if *id == crate::diagnostics::SSH_WRAP_ID
    ));
}

fn target_for(app: &AppView, id: AgentId) -> crate::app::actions::DoctorFixTarget {
    crate::app::actions::DoctorFixTarget {
        agent_id: id,
        session_id: agent_ref(app, id).session.session_id.clone(),
        session_binding_epoch: agent_ref(app, id).session_binding_epoch,
        cwd: agent_ref(app, id).session.cwd.clone(),
    }
}

fn doctor_question_app(temp: &std::path::Path) -> AppView {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().prompt.set_text("draft");
    let target = target_for(&app, id);
    super::super::prompt::open_doctor_fix_question(
        &mut app,
        target,
        Box::new(crate::diagnostics::test_fix_plan(temp)),
    );
    app
}

#[test]
fn doctor_fix_modal_stashes_prompt_and_confirms_exactly_one_apply() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = doctor_question_app(temp.path());
    let id = AgentId(0);
    assert_eq!(agent_ref(&app, id).prompt.text(), "");
    let outcome = app
        .agents
        .get_mut(&id)
        .unwrap()
        .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
    let crate::app::app_view::InputOutcome::Action(action) = outcome else {
        panic!("confirm must produce an action: {outcome:?}");
    };
    let effects = dispatch(action, &mut app);
    assert_eq!(agent_ref(&app, id).prompt.text(), "draft");
    assert!(matches!(
        effects.as_slice(),
        [Effect::ApplyDoctorFix { .. }]
    ));
}

#[test]
fn doctor_fix_confirm_rejects_changed_session_or_cwd() {
    let temp = tempfile::tempdir().unwrap();
    for mutate in ["session", "cwd"] {
        let mut app = doctor_question_app(temp.path());
        let id = AgentId(0);
        let outcome = app
            .agents
            .get_mut(&id)
            .unwrap()
            .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            ));
        let crate::app::app_view::InputOutcome::Action(action) = outcome else {
            panic!("confirm must produce an action: {outcome:?}");
        };
        if mutate == "session" {
            app.agents
                .get_mut(&id)
                .unwrap()
                .bind_session_id("changed".into());
        } else {
            app.agents.get_mut(&id).unwrap().session.cwd = std::path::PathBuf::from("/changed");
        }
        let effects = dispatch(action, &mut app);
        assert!(effects.is_empty(), "{mutate}");
        assert_eq!(agent_ref(&app, id).prompt.text(), "draft", "{mutate}");
        assert!(
            last_system_text(&app, id).contains("session changed"),
            "{mutate}"
        );
    }
}

#[test]
fn doctor_fix_promoted_target_allows_confirm_and_apply() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = doctor_question_app(temp.path());
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().unbind_session_id();
    let epoch = agent_ref(&app, id).session_binding_epoch;
    let question = app
        .agents
        .get_mut(&id)
        .unwrap()
        .question_view
        .as_mut()
        .unwrap();
    let Some(crate::views::question_view::LocalQuestionKind::DoctorFix { target, .. }) =
        question.local_kind.as_mut()
    else {
        panic!("doctor modal expected");
    };
    target.session_id = None;
    target.session_binding_epoch = epoch;
    app.agents
        .get_mut(&id)
        .unwrap()
        .bind_session_id("bound".into());
    let outcome = app
        .agents
        .get_mut(&id)
        .unwrap()
        .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
    let crate::app::app_view::InputOutcome::Action(action) = outcome else {
        panic!("confirm must produce an action: {outcome:?}");
    };
    let effects = dispatch(action, &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ApplyDoctorFix { target, .. }]
            if target.session_id == agent_ref(&app, id).session.session_id
    ));
}

#[test]
fn doctor_fix_none_target_rejects_cwd_change() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = doctor_question_app(temp.path());
    let id = AgentId(0);
    let epoch = agent_ref(&app, id).session_binding_epoch;
    let question = app
        .agents
        .get_mut(&id)
        .unwrap()
        .question_view
        .as_mut()
        .unwrap();
    let Some(crate::views::question_view::LocalQuestionKind::DoctorFix { target, .. }) =
        question.local_kind.as_mut()
    else {
        panic!("doctor modal expected");
    };
    target.session_id = None;
    target.session_binding_epoch = epoch;
    let outcome = app
        .agents
        .get_mut(&id)
        .unwrap()
        .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
    let crate::app::app_view::InputOutcome::Action(action) = outcome else {
        panic!("confirm must produce an action: {outcome:?}");
    };
    app.agents.get_mut(&id).unwrap().session.cwd = std::path::PathBuf::from("/changed");
    assert!(dispatch(action, &mut app).is_empty());
    assert!(last_system_text(&app, id).contains("session changed"));
}

#[test]
fn doctor_fix_background_confirm_keeps_original_target() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = doctor_question_app(temp.path());
    let initiator = AgentId(0);
    let original = target_for(&app, initiator);
    let outcome = app
        .agents
        .get_mut(&initiator)
        .unwrap()
        .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
    let crate::app::app_view::InputOutcome::Action(action) = outcome else {
        panic!("confirm must produce an action: {outcome:?}");
    };
    insert_placeholder_agent(&mut app, AgentId(1));
    app.active_view = ActiveView::Agent(AgentId(1));
    assert!(matches!(
        &action,
        Action::DoctorFixConfirmed { target, .. } if target == &original
    ));
    let effects = dispatch(action, &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ApplyDoctorFix { target, .. }] if target == &original
    ));
}

#[test]
fn doctor_fix_cancel_routes_to_initiator_then_fallbacks() {
    let temp = tempfile::tempdir().unwrap();
    let mut app = doctor_question_app(temp.path());
    let initiator = AgentId(0);
    let outcome = app
        .agents
        .get_mut(&initiator)
        .unwrap()
        .handle_question_key_for_test(&crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
    let crate::app::app_view::InputOutcome::Action(action) = outcome else {
        panic!("cancel must produce an action: {outcome:?}");
    };
    insert_placeholder_agent(&mut app, AgentId(1));
    app.active_view = ActiveView::Agent(AgentId(1));
    assert!(dispatch(action, &mut app).is_empty());
    assert_eq!(last_system_text(&app, initiator), "Fix cancelled.");

    let target = target_for(&app, initiator);
    app.agents.shift_remove(&initiator);
    assert!(dispatch(Action::DoctorFixCancelled(target.clone()), &mut app).is_empty());
    assert_eq!(last_system_text(&app, AgentId(1)), "Fix cancelled.");

    app.agents.clear();
    app.active_view = ActiveView::Welcome;
    assert!(dispatch(Action::DoctorFixCancelled(target), &mut app).is_empty());
    assert_eq!(
        app.startup_warnings.last().unwrap().message,
        "Fix cancelled."
    );
}

#[test]
fn doctor_fix_all_cancel_keys_restore_prompt_without_effect() {
    let temp = tempfile::tempdir().unwrap();
    for key in [
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        ),
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('X'),
            crossterm::event::KeyModifiers::SHIFT,
        ),
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('y'),
            crossterm::event::KeyModifiers::CONTROL,
        ),
    ] {
        let mut app = doctor_question_app(temp.path());
        let id = AgentId(0);
        let outcome = app
            .agents
            .get_mut(&id)
            .unwrap()
            .handle_question_key_for_test(&key);
        let crate::app::app_view::InputOutcome::Action(action) = outcome else {
            panic!("cancel must produce an action: {outcome:?}");
        };
        let effects = dispatch(action, &mut app);
        assert!(effects.is_empty());
        assert_eq!(agent_ref(&app, id).prompt.text(), "draft");
        assert_eq!(last_system_text(&app, id), "Fix cancelled.");
    }
}

/// `/history` dispatches `OpenHistorySearch`, which opens the search panel on the active agent with the session's prompt history.
#[test]
fn open_history_search_activates_overlay_on_active_agent() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.prompt_history = vec!["earlier prompt".into()];

    let effects = dispatch(Action::OpenHistorySearch, &mut app);
    assert!(effects.is_empty());
    assert!(
        agent_ref(&app, id).prompt.history_search.is_active(),
        "OpenHistorySearch must activate the history search overlay"
    );
}

/// Sending a bash command is a submit: it retires the active ephemeral tip.
#[test]
fn send_bash_command_clears_active_ephemeral_tip() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let agent = app.agents.get_mut(&id).unwrap();
    let _ = agent.ephemeral_tip.show(
        crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint")),
        &mut std::collections::HashMap::new(),
    );
    assert!(agent.ephemeral_tip.is_active());

    let _ = dispatch(Action::SendBashCommand("ls".into()), &mut app);
    assert!(
        !app.agents.get(&id).unwrap().ephemeral_tip.is_active(),
        "bash submit must clear the tip"
    );
}

/// `ShowUndoTip` on a never-drawn agent is refused by the renderability gate: no tip shown, no count burned, no effects.
/// (Tip on so the renderability gate, not the per-tip gate, is what refuses.)
#[test]
fn show_undo_tip_refused_on_undrawn_agent() {
    let mut app = test_app_with_agent();
    app.contextual_hints.undo = true;
    let id = AgentId(0);

    let effects = dispatch(Action::ShowUndoTip, &mut app);
    assert!(effects.is_empty());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// `ShowUndoTip` on a drawable agent shows the tip and increments the per-session seen count in memory.
/// It emits no effects (nothing is persisted to disk).
#[test]
fn show_undo_tip_shows_and_counts_in_memory() {
    use crate::tips::clear_detector::UNDO_TIP_SEEN_KEY;
    let mut app = test_app_with_agent();
    app.contextual_hints.undo = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);

    let effects = dispatch(Action::ShowUndoTip, &mut app);
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());
    assert_eq!(app.tip_seen_counts.get(UNDO_TIP_SEEN_KEY), Some(&1));
    assert!(
        effects.is_empty(),
        "seen count is in-memory; nothing persisted"
    );
}

/// `ShowUndoTip` is a no-op when its per-tip gate is off: no tip shown, no count burned, even on a drawable agent.
#[test]
fn show_undo_tip_no_op_when_flag_off() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (80, 30);
    app.contextual_hints.undo = false;

    let effects = dispatch(Action::ShowUndoTip, &mut app);
    assert!(effects.is_empty());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// `show_small_screen_tip` on a drawable agent shows the tip and increments the per-session seen count in memory.
/// Nothing is persisted; the fn returns nothing, so it cannot raise effects.
#[test]
fn show_small_screen_tip_shows_and_counts_in_memory() {
    use crate::tips::small_screen::SMALL_SCREEN_TIP_SEEN_KEY;
    let mut app = test_app_with_agent();
    app.contextual_hints.small_screen = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);

    crate::app::dispatch::show_small_screen_tip(&mut app);
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());
    assert_eq!(app.tip_seen_counts.get(SMALL_SCREEN_TIP_SEEN_KEY), Some(&1));
}

/// `show_small_screen_tip` is a no-op when its per-tip gate is off: no tip shown, no count burned, even on a drawable agent.
#[test]
fn show_small_screen_tip_no_op_when_flag_off() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);
    app.contextual_hints.small_screen = false;

    crate::app::dispatch::show_small_screen_tip(&mut app);
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// The trigger defers, WITHOUT consuming the one-shot, until the active view is an agent with a stable, draw-measured size.
/// The first stable in-band measure then shows the tip exactly once.
#[test]
fn small_screen_trigger_waits_for_stable_agent_measure_then_fires_once() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Welcome view: no evaluation, one-shot not consumed.
    app.active_view = ActiveView::Welcome;
    app.maybe_trigger_small_screen_tip();
    assert!(!app.small_screen_tip_evaluated);

    // Agent view, but never drawn (size (0,0)): still deferred.
    app.active_view = ActiveView::Agent(id);
    app.maybe_trigger_small_screen_tip();
    assert!(!app.small_screen_tip_evaluated);

    // Pending post-resize re-measure: still deferred.
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.last_terminal_size = (100, 24);
        agent.terminal_size_stale = true;
    }
    app.maybe_trigger_small_screen_tip();
    assert!(!app.small_screen_tip_evaluated);

    // Stable in-band measure: evaluates once and shows.
    app.agents.get_mut(&id).unwrap().terminal_size_stale = false;
    app.maybe_trigger_small_screen_tip();
    assert!(app.small_screen_tip_evaluated);
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());

    // One-shot: a later call (e.g. after a resize back into the band) is inert.
    app.agents.get_mut(&id).unwrap().ephemeral_tip.clear_all();
    app.maybe_trigger_small_screen_tip();
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// An in-band first measure whose banner row is occluded (session banner, permission ask, modal, open dropdown) defers WITHOUT consuming.
/// The show gate would refuse it, and spending the one-shot invisibly would kill the hint for the run.
/// Once the occluder clears, the next draw shows it.
#[test]
fn small_screen_trigger_defers_while_banner_row_occluded() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.last_terminal_size = (100, 24);
        agent.session_banner_active = true;
    }

    app.maybe_trigger_small_screen_tip();
    assert!(!app.small_screen_tip_evaluated, "occlusion must defer");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");

    // Occluder gone: the next draw evaluates and shows.
    app.agents.get_mut(&id).unwrap().session_banner_active = false;
    app.maybe_trigger_small_screen_tip();
    assert!(app.small_screen_tip_evaluated);
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());
}

/// An out-of-band first measure consumes the one-shot without showing, so a later resize INTO the band can never bring the tip back.
#[test]
fn small_screen_trigger_out_of_band_consumes_without_showing() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 40);

    app.maybe_trigger_small_screen_tip();
    assert!(app.small_screen_tip_evaluated, "evaluation is consumed");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");

    // Later in-band measure: still nothing (one-shot already spent).
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);
    app.maybe_trigger_small_screen_tip();
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// The small-screen tip is ambient: submitting the promote prompt right after it shows must NOT retire it.
/// A real turn takes seconds, so the submit-clear reduced the tip to a sub-second blink.
/// The edit-contextual tips keep their retire-on-submit behavior (`send_prompt_clears_active_ephemeral_tip` above pins that side).
#[test]
fn send_prompt_keeps_ambient_small_screen_tip() {
    use crate::tips::small_screen::SMALL_SCREEN_TIP_SEEN_KEY;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);

    app.maybe_trigger_small_screen_tip();
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());

    let _ = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(
        agent_ref(&app, id).ephemeral_tip.is_active(),
        "ambient tip must survive the prompt submit"
    );
    assert_eq!(
        app.tip_seen_counts.get(SMALL_SCREEN_TIP_SEEN_KEY),
        Some(&1),
        "surviving the submit is the same show — no second count"
    );
}

/// Ambient TTL burns only while the tip row can paint.
/// While the row is not renderable the tick is a frozen no-op (and reports no animation demand).
/// The TTL resumes with the remaining budget once the row can paint again.
#[test]
fn ambient_tip_ttl_freezes_while_row_cannot_paint() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);
    app.maybe_trigger_small_screen_tip();

    let agent = app.agents.get_mut(&id).unwrap();
    let budget = agent.ephemeral_tip.ticks_remaining().unwrap();

    // Row cannot paint (pending post-resize re-measure): TTL frozen.
    agent.note_terminal_resize();
    assert!(
        !agent.ephemeral_tip_needs_tick(),
        "frozen tip demands no ticks"
    );
    for _ in 0..5 {
        assert!(!agent.tick_ephemeral_tip());
    }
    assert_eq!(
        agent.ephemeral_tip.ticks_remaining(),
        Some(budget),
        "occlusion must pause the ambient TTL, not burn it"
    );

    // Row paints again: TTL resumes from the same budget.
    agent.note_terminal_size((100, 24));
    assert!(agent.ephemeral_tip_needs_tick());
    assert!(!agent.tick_ephemeral_tip());
    assert_eq!(agent.ephemeral_tip.ticks_remaining(), Some(budget - 1));

    // Contrast: an edit-contextual tip keeps burning while unrenderable.
    agent.ephemeral_tip.clear_all();
    let _ = agent.ephemeral_tip.show(
        crate::tips::EphemeralTip::new("t", ratatui::text::Line::from("hint")),
        &mut std::collections::HashMap::new(),
    );
    let budget = agent.ephemeral_tip.ticks_remaining().unwrap();
    agent.note_terminal_resize();
    let _ = agent.tick_ephemeral_tip();
    assert_eq!(
        agent.ephemeral_tip.ticks_remaining(),
        Some(budget - 1),
        "edit-contextual tips keep burning while unrenderable"
    );
}

/// Pins the whole lifecycle: show at promote, survive the submit, pause under occlusion, resume, expire on the visible-time budget.
/// Exactly one seen-count for the run, and the spent one-shot never re-triggers.
#[test]
fn small_screen_tip_lifecycle_shows_once_across_submit_and_occlusion() {
    use crate::tips::small_screen::SMALL_SCREEN_TIP_SEEN_KEY;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);

    app.maybe_trigger_small_screen_tip();
    let _ = dispatch(Action::SendPrompt("hello".into()), &mut app);

    let agent = app.agents.get_mut(&id).unwrap();
    assert!(agent.ephemeral_tip.is_active());
    // Occlusion window mid-turn: frozen, then resumes.
    agent.note_terminal_resize();
    for _ in 0..200 {
        let _ = agent.tick_ephemeral_tip();
    }
    assert!(
        agent.ephemeral_tip.is_active(),
        "must not expire off-screen"
    );
    agent.note_terminal_size((100, 24));
    // Lives out the remaining visible-time budget, then expires.
    for _ in 0..300 {
        let _ = agent.tick_ephemeral_tip();
    }
    assert!(
        !agent.ephemeral_tip.is_active(),
        "expires after visible TTL"
    );

    // Expiry does not resurrect anything: one-shot spent, count capped at 1.
    app.maybe_trigger_small_screen_tip();
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
    assert_eq!(app.tip_seen_counts.get(SMALL_SCREEN_TIP_SEEN_KEY), Some(&1));
    assert!(app.small_screen_tip_evaluated);
}

/// The user's compact setting being ON suppresses the tip (the hint would advertise a mode they already use); the one-shot is still consumed.
#[test]
fn small_screen_trigger_suppressed_when_user_compact_on() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);
    app.current_ui.compact_mode = true;

    app.maybe_trigger_small_screen_tip();
    assert!(app.small_screen_tip_evaluated);
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
}

/// `show_ssh_wrap_tip` on a drawable agent shows the tip and increments the per-session seen count in memory.
/// Nothing is persisted; the fn returns nothing, so it cannot raise effects.
#[test]
fn show_ssh_wrap_tip_shows_and_counts_in_memory() {
    use crate::tips::ssh_wrap::{SSH_WRAP_TIP_KEY, SSH_WRAP_TIP_SEEN_KEY};
    let mut app = test_app_with_agent();
    app.contextual_hints.ssh_wrap = true;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 40);

    crate::app::dispatch::show_ssh_wrap_tip(&mut app);
    assert_eq!(
        agent_ref(&app, id).ephemeral_tip.current_key(),
        Some(SSH_WRAP_TIP_KEY)
    );
    assert_eq!(app.tip_seen_counts.get(SSH_WRAP_TIP_SEEN_KEY), Some(&1));
}

/// `show_ssh_wrap_tip` is a no-op when `contextual_hints.ssh_wrap` is off: no tip shown, no count burned, even on a drawable agent.
#[test]
fn show_ssh_wrap_tip_no_op_when_flag_off() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 40);
    app.contextual_hints.ssh_wrap = false;

    crate::app::dispatch::show_ssh_wrap_tip(&mut app);
    assert!(app.tip_seen_counts.is_empty(), "no count burned");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// The seen cap holds at one show per session even if the show fn re-runs after the first tip expired or was cleared.
#[test]
fn show_ssh_wrap_tip_respects_once_per_session_cap() {
    use crate::tips::ssh_wrap::SSH_WRAP_TIP_SEEN_KEY;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 40);

    crate::app::dispatch::show_ssh_wrap_tip(&mut app);
    app.agents.get_mut(&id).unwrap().ephemeral_tip.clear_all();
    crate::app::dispatch::show_ssh_wrap_tip(&mut app);
    assert!(
        !agent_ref(&app, id).ephemeral_tip.is_active(),
        "second show must be seen-gated"
    );
    assert_eq!(app.tip_seen_counts.get(SSH_WRAP_TIP_SEEN_KEY), Some(&1));
}

/// The trigger defers, WITHOUT consuming the one-shot, until the active view is an agent with a stable, draw-measured size.
/// The first stable measure with the environment recommending wrap then shows it exactly once.
#[test]
fn ssh_wrap_trigger_waits_for_stable_agent_measure_then_fires_once() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Welcome view: no evaluation, one-shot not consumed.
    app.active_view = ActiveView::Welcome;
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!app.ssh_wrap_tip_evaluated);

    // Agent view, but never drawn (size (0,0)): still deferred.
    app.active_view = ActiveView::Agent(id);
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!app.ssh_wrap_tip_evaluated);

    // Pending post-resize re-measure: still deferred.
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.last_terminal_size = (100, 40);
        agent.terminal_size_stale = true;
    }
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!app.ssh_wrap_tip_evaluated);

    // Stable measure and recommending environment: evaluates once and shows
    app.agents.get_mut(&id).unwrap().terminal_size_stale = false;
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(app.ssh_wrap_tip_evaluated);
    assert_eq!(
        agent_ref(&app, id).ephemeral_tip.current_key(),
        Some(crate::tips::ssh_wrap::SSH_WRAP_TIP_KEY)
    );

    // One-shot: later calls are inert.
    app.agents.get_mut(&id).unwrap().ephemeral_tip.clear_all();
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// A not-recommending environment (local session, wrap sink already active, or a VS Code remote) consumes the one-shot without showing.
/// Whether the environment recommends wrap cannot change while the process runs, so there is nothing to re-evaluate later.
#[test]
fn ssh_wrap_trigger_env_not_recommending_consumes_without_showing() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 40);

    app.maybe_trigger_ssh_wrap_tip_inner(false);
    assert!(app.ssh_wrap_tip_evaluated, "evaluation is consumed");
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
    assert!(app.tip_seen_counts.is_empty(), "no count burned");

    // The one-shot is spent: even a recommending call stays inert.
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!agent_ref(&app, id).ephemeral_tip.is_active());
}

/// A busy tip slot defers WITHOUT consuming: replacing would burn the other session-load tip's once-per-session show.
/// Once the slot frees, the next draw shows the wrap tip.
#[test]
fn ssh_wrap_trigger_defers_while_tip_slot_busy() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    // In the small-screen band so the other session-load tip takes the slot first (mirrors the real draw order: the small-screen trigger runs first)
    app.agents.get_mut(&id).unwrap().last_terminal_size = (100, 24);
    app.maybe_trigger_small_screen_tip();
    assert!(agent_ref(&app, id).ephemeral_tip.is_active());

    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(!app.ssh_wrap_tip_evaluated, "busy slot must defer");
    assert_eq!(
        agent_ref(&app, id).ephemeral_tip.current_key(),
        Some(crate::tips::small_screen::SMALL_SCREEN_TIP_KEY),
        "the earlier tip keeps the slot"
    );

    // Slot free (the first tip expired or cleared): the next draw shows it.
    app.agents.get_mut(&id).unwrap().ephemeral_tip.clear_all();
    app.maybe_trigger_ssh_wrap_tip_inner(true);
    assert!(app.ssh_wrap_tip_evaluated);
    assert_eq!(
        agent_ref(&app, id).ephemeral_tip.current_key(),
        Some(crate::tips::ssh_wrap::SSH_WRAP_TIP_KEY)
    );
}

#[test]
fn focus_prompt_switches_pane() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::FocusPrompt, &mut app);
    assert!(effects.is_empty());
    assert_eq!(agent_ref(&app, id).active_pane, ActivePane::Prompt);
}

/// `FocusPrompt` resolves to the child under a takeover, whose hidden composer refuses the pane; the root keeps its focus too.
#[test]
fn focus_prompt_under_takeover_is_refused_on_the_child() {
    let mut app = test_app_with_agent();
    let parent_id = AgentId(0);
    let child_sid = "child-overlay-focus";
    let child = AgentView::new(
        make_test_agent_session(&app, AgentId(1), child_sid),
        ScrollbackState::new(),
    );
    {
        let parent = app.agents.get_mut(&parent_id).unwrap();
        parent.set_active_pane(ActivePane::Scrollback, true);
        parent.insert_test_child(child_sid.to_string(), Box::new(child));
        parent.active_subagent = Some(child_sid.to_string());
    }

    let effects = dispatch(Action::FocusPrompt, &mut app);

    assert!(effects.is_empty());
    let parent = app.agents.get(&parent_id).unwrap();
    assert_eq!(ActivePane::Scrollback, parent.active_pane);
    let child = parent.subagent_view(child_sid).unwrap();
    assert_eq!(ActivePane::Scrollback, child.active_pane);
}

#[test]
fn send_prompt_produces_effect_and_clears_input() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .textarea
        .insert_str("hello");

    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);

    // Prompt is enqueued and immediately drained (agent was idle).
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "hello"));
    assert!(agent_ref(&app, id).prompt.text().is_empty());
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    assert_eq!(agent_ref(&app, id).scrollback.len(), 1);
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
}

/// Register `pr-workflow` as an ACP-advertised skill on the agent's slash registry, mirroring the shell's available-commands sync.
/// Shared with the modes tests (`/plan <desc>` range forwarding).
pub(super) fn register_pr_workflow_skill(app: &mut AppView, id: AgentId) {
    let agent = app.agents.get_mut(&id).unwrap();
    let models = agent.session.models.clone();
    agent.prompt.sync_acp_commands(
        &[
            acp::AvailableCommand::new("pr-workflow", "PR workflow skill").meta(
                serde_json::json!({
                    "path": "/tmp/skills/pr-workflow/SKILL.md",
                    "scope": "local",
                })
                .as_object()
                .cloned(),
            ),
        ],
        None,
        &models,
    );
}

/// A plain prompt with a mid-text recognized `/skill` token drains into a token-styled user block.
/// It carries the ranges on `SendPrompt` (stamped into wire meta for replay).
#[test]
fn send_prompt_mid_text_skill_token_carries_ranges() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_pr_workflow_skill(&mut app, id);

    let effects = dispatch(
        Action::SendPrompt("great /pr-workflow all good now".into()),
        &mut app,
    );

    assert_eq!(effects.len(), 1);
    match effects.first() {
        Some(Effect::SendPrompt {
            text,
            skill_token_ranges,
            ..
        }) => {
            assert_eq!(text, "great /pr-workflow all good now");
            assert_eq!(skill_token_ranges, &vec![6..18]);
        }
        other => panic!("expected SendPrompt, got {other:?}"),
    }
    // The drained echo block styles exactly the composer-recognized token.
    match &agent_ref(&app, id).scrollback.get(0).unwrap().block {
        RenderBlock::UserPrompt(b) => {
            assert_eq!(b.skill_token_ranges, vec![6..18]);
        }
        other => panic!("expected UserPrompt, got {other:?}"),
    }
}

/// An unrecognized `/word` mid-text stays a plain prompt: no ranges on the block or the effect.
#[test]
fn send_prompt_unknown_token_has_empty_ranges() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt("great /frobnicate now".into()), &mut app);

    assert_eq!(effects.len(), 1);
    match effects.first() {
        Some(Effect::SendPrompt {
            skill_token_ranges, ..
        }) => assert!(skill_token_ranges.is_empty()),
        other => panic!("expected SendPrompt, got {other:?}"),
    }
    match &agent_ref(&app, id).scrollback.get(0).unwrap().block {
        RenderBlock::UserPrompt(b) => assert!(b.skill_token_ranges.is_empty()),
        other => panic!("expected UserPrompt, got {other:?}"),
    }
}

/// An image-bearing prompt with token ranges: the LOCAL echo styles the token, but the wire blocks stay unstamped.
/// The image builder rewrites the text (placeholder stripping), which would shift byte offsets, so replay renders these plain (known limitation).
#[test]
fn image_prompt_with_ranges_styles_echo_but_wire_meta_absent() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        // Real constructor path, with the image attached to the queued row.
        let agent = app.agents.get_mut(&id).unwrap();
        agent
            .session
            .enqueue_prompt_with_skill_tokens("great /pr-workflow go".into(), vec![6..18]);
        agent.session.pending_prompts.back_mut().unwrap().images =
            vec![crate::app::agent_view::test_fixtures::test_pasted_image()];
    }

    let effects = dispatch(Action::DrainQueue, &mut app);

    match &agent_ref(&app, id).scrollback.get(0).unwrap().block {
        RenderBlock::UserPrompt(b) => assert_eq!(b.skill_token_ranges, vec![6..18]),
        other => panic!("expected UserPrompt, got {other:?}"),
    }
    match effects.first() {
        Some(Effect::SendPromptBlocks { blocks, .. }) => {
            let Some(acp::ContentBlock::Text(tb)) = blocks.first() else {
                panic!("first block must be text");
            };
            assert!(
                tb.meta
                    .as_ref()
                    .and_then(|m| m.get("skillTokenRanges"))
                    .is_none(),
                "images path is known-plain on replay: no ranges meta"
            );
        }
        other => panic!("expected SendPromptBlocks, got {other:?}"),
    }
}

/// A leading skill invocation still takes the InjectSkill path: the drained block is a skill prompt (`display_as_skill`), not the mid-text styling.
#[test]
fn send_prompt_leading_skill_keeps_inject_skill_path() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    register_pr_workflow_skill(&mut app, id);

    let effects = dispatch(Action::SendPrompt("/pr-workflow ship it".into()), &mut app);

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SendPromptBlocks { .. })),
        "leading skill must send structured blocks, got {effects:?}"
    );
    match &agent_ref(&app, id).scrollback.get(0).unwrap().block {
        RenderBlock::UserPrompt(b) => {
            assert_eq!(
                b.skill_token_ranges,
                vec![0..12],
                "InjectSkill path styles the leading /pr-workflow token"
            );
            assert_eq!(b.text, "/pr-workflow ship it");
        }
        other => panic!("expected UserPrompt, got {other:?}"),
    }
}

#[test]
fn follow_up_chip_preserves_prompt_draft() {
    // A chip click submits the suggestion but must not wipe a typed draft.
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .textarea
        .insert_str("my draft");
    dispatch(Action::SubmitFollowUp("Summarize".into()), &mut app);
    assert_eq!(agent_ref(&app, id).prompt.text(), "my draft");
}

#[test]
fn send_prompt_clears_follow_up_chips() {
    // Production turn-start path: the local queue drain clears the previous response's chips
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents
        .get_mut(&id)
        .unwrap()
        .apply_follow_ups("resp-1".into(), vec!["a".into()]);
    dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(
        agent_ref(&app, id).follow_ups.is_none(),
        "starting a turn must clear chips"
    );
}

#[test]
fn chip_submit_while_enqueued_clears_follow_up_chips() {
    // A chip click submitted while a turn is RUNNING *and* the local queue is non-empty takes the ENQUEUE path, not immediate-server-send
    // `immediate_server_send_eligible` is false whenever `pending_prompts` is non-empty
    // Clearing only on the immediate-send branch would leave this path with chips on screen after the user had already acted on one
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.apply_follow_ups("resp-1".into(), vec!["Summarize".into()]);
        assert!(agent.follow_ups.is_some(), "precondition: chips shown");
        // A running turn plus a non-empty local queue is NOT immediate-send eligible, so the submit is held in the local queue instead
        agent.session.state = AgentState::TurnRunning;
        agent.session.enqueue_prompt("earlier".into());
        assert!(
            !agent.session.pending_prompts.is_empty(),
            "precondition: non-empty local queue forces the enqueue path"
        );
    }

    let effects = dispatch(Action::SubmitFollowUp("Summarize".into()), &mut app);

    // Not immediate-sent: the chip text is enqueued locally (it does not produce a `SendPrompt` effect for the chip while a turn is running)
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { text, .. } if text == "Summarize")),
        "chip must be enqueued, not immediate-sent, got {effects:?}"
    );
    // The chips are cleared on the enqueue path too
    assert!(
        agent_ref(&app, id).follow_ups.is_none(),
        "enqueue chip path must clear chips"
    );
}

#[test]
fn send_prompt_while_running_queues_without_drain() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(Action::SendPrompt("queued".into()), &mut app);

    // A plain prompt typed while a turn is running is sent IMMEDIATELY (server-authoritative) rather than held in the local drip-feed queue
    // It does NOT start a concurrent turn
    assert_eq!(effects.len(), 1);
    let pid = match effects.first() {
        Some(Effect::SendPrompt {
            text, prompt_id, ..
        }) => {
            assert_eq!(text, "queued");
            prompt_id.clone()
        }
        other => panic!("expected immediate SendPrompt, got {other:?}"),
    };
    // Not in the local queue; turn state unchanged (no new turn started).
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    // Optimistic echo present in the shared queue, keyed by prompt_id.
    let q = app
        .shared_prompt_queue("test-session")
        .expect("optimistic echo present");
    let [front] = q.as_slice() else {
        panic!("expected one optimistic echo, got {q:?}");
    };
    assert_eq!(front.id, pid);
    assert_eq!(front.text, "queued");
}

#[test]
fn send_prompt_while_running_queues_on_server_when_follow_up_steer() {
    struct ResetFollowUp(crate::appearance::FollowUpBehavior);
    impl Drop for ResetFollowUp {
        fn drop(&mut self) {
            crate::appearance::cache::set_follow_up_behavior(self.0);
        }
    }
    let _reset = ResetFollowUp(crate::appearance::cache::load_follow_up_behavior());
    crate::appearance::cache::set_follow_up_behavior(crate::appearance::FollowUpBehavior::Steer);

    let mut app = test_app_with_agent();
    app.leader_mode = false;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(Action::SendPrompt("steer me".into()), &mut app);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::SendPrompt { text, .. }] if text == "steer me"
        ),
        "Steer should server-queue, not interject yet, got {effects:?}"
    );
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
}

#[test]
fn send_prompt_while_running_with_follow_up_queue_stays_local_when_not_leader() {
    struct ResetFollowUp(crate::appearance::FollowUpBehavior);
    impl Drop for ResetFollowUp {
        fn drop(&mut self) {
            crate::appearance::cache::set_follow_up_behavior(self.0);
        }
    }
    let _reset = ResetFollowUp(crate::appearance::cache::load_follow_up_behavior());
    crate::appearance::cache::set_follow_up_behavior(crate::appearance::FollowUpBehavior::Queue);

    let mut app = test_app_with_agent();
    app.leader_mode = false;
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(Action::SendPrompt("later".into()), &mut app);
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendInterject { .. })),
        "Queue must not interject mid-turn, got {effects:?}"
    );
    // Non-leader and Queue mode: local drip-feed path (not server-immediate)
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);
}

/// With Steer, an older local drip-feed row still blocks server-immediate send.
/// Newer Enter must not interject or jump the server queue ahead of the older local prompt.
#[test]
fn send_while_running_with_pending_local_and_steer_preserves_fifo() {
    struct ResetFollowUp(crate::appearance::FollowUpBehavior);
    impl Drop for ResetFollowUp {
        fn drop(&mut self) {
            crate::appearance::cache::set_follow_up_behavior(self.0);
        }
    }
    let _reset = ResetFollowUp(crate::appearance::cache::load_follow_up_behavior());
    crate::appearance::cache::set_follow_up_behavior(crate::appearance::FollowUpBehavior::Steer);

    let mut app = test_app_with_agent();
    app.leader_mode = false;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.enqueue_prompt("two".into());
        assert_eq!(agent.session.pending_prompts.len(), 1);
    }

    let effects = dispatch(Action::SendPrompt("three".into()), &mut app);
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendInterject { .. } | Effect::SendPrompt { .. })),
        "Steer must not bypass empty-local-queue gate, got {effects:?}"
    );
    let order: Vec<&str> = agent_ref(&app, id)
        .session
        .pending_prompts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(order, vec!["two", "three"]);
}

/// Images never ride server-immediate; with Steer they still join the local queue (no interject-on-Enter with attachments).
#[test]
fn send_prompt_with_images_while_running_and_steer_stays_local() {
    struct ResetFollowUp(crate::appearance::FollowUpBehavior);
    impl Drop for ResetFollowUp {
        fn drop(&mut self) {
            crate::appearance::cache::set_follow_up_behavior(self.0);
        }
    }
    let _reset = ResetFollowUp(crate::appearance::cache::load_follow_up_behavior());
    crate::appearance::cache::set_follow_up_behavior(crate::appearance::FollowUpBehavior::Steer);

    let mut app = test_app_with_agent();
    app.leader_mode = false;
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent
            .prompt
            .insert_image(crate::prompt_images::PastedImage {
                element_id: distill_ratatui_textarea::ElementId::from_raw(0),
                display_number: 0,
                mime_type: "image/png".to_owned(),
                dimensions: Some((8, 8)),
                byte_len: 1,
                encoded_bytes: Some(vec![0].into()),
                source_path: None,
                staged_temp_path: None,
                session_image_path: None,
                preview: crate::prompt_images::PromptImagePreview::default(),
            })
            .unwrap();
    }

    let effects = dispatch(Action::SendPrompt("with image".into()), &mut app);
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendInterject { .. } | Effect::SendPrompt { .. })),
        "images + Steer must not interject or server-send, got {effects:?}"
    );
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);
}

/// Regression (queue reorder race): a mid-turn plain prompt must NOT jump the server queue past an older local drip-feed prompt.
/// An immediate-sent prompt would render/run AHEAD of the older local prompt (the merge is server-rows-first), so `[2, 3]` showed up as `[3, 2]`.
/// The new prompt must instead join the local queue behind the older one, preserving FIFO.
#[test]
fn send_while_running_with_pending_local_prompt_preserves_fifo() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        // A turn is running (prompt "1" already drained from the startup queue)
        // An older prompt ("2") is still stranded in the local drip-feed queue because it was enqueued before the turn began
        agent.session.state = AgentState::TurnRunning;
        agent.session.enqueue_prompt("two".into());
        assert_eq!(agent.session.pending_prompts.len(), 1);
    }

    // Send "3" while the queue still holds "2": it must route LOCALLY.
    let effects = dispatch(Action::SendPrompt("three".into()), &mut app);

    // No immediate server-authoritative send (no SendPrompt effect, and no drain since the turn is running)
    assert!(
        effects.is_empty(),
        "must not immediate-send while a local prompt is pending, got {effects:?}"
    );
    // "3" joined the LOCAL queue behind "2" (FIFO preserved).
    let agent = agent_ref(&app, id);
    let order: Vec<&str> = agent
        .session
        .pending_prompts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["two", "three"],
        "new prompt must queue behind the older local prompt"
    );
    // No optimistic shared-queue echo was created (nothing went server-side).
    assert!(
        app.shared_prompt_queue("test-session")
            .is_none_or(|q| q.is_empty()),
        "no server-queue echo while routing locally"
    );
}

#[test]
fn turn_end_drains_next_queued_prompt() {
    // A plain prompt typed while running is sent server-authoritatively and drained by the leader, not by the local queue
    // The leader's `running_prompt_id` broadcast is modeled here by a stashed adoption that arrived before the previous turn's PromptResponse
    // The PromptResponse handler adopts it after `finish_turn`, rendering its user block
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Submit first prompt (drains immediately and starts the turn)
    let effects = dispatch(Action::SendPrompt("first".into()), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(agent_ref(&app, id).session.state.is_turn_running());

    // Submit second prompt while running: immediate server-authoritative send (no local queue entry)
    let effects = dispatch(Action::SendPrompt("second".into()), &mut app);
    let pid_second = match effects.first() {
        Some(Effect::SendPrompt {
            text, prompt_id, ..
        }) => {
            assert_eq!(text, "second");
            prompt_id.clone()
        }
        other => panic!("expected immediate SendPrompt, got {other:?}"),
    };
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);

    // Model the leader's running=second broadcast arriving before first's PromptResponse: stash the adoption (FIFO handoff race)
    app.pending_running_adoptions.insert(
        id,
        crate::app::acp_handler::PendingRunningAdoption {
            prompt_id: pid_second.clone(),
            text: Some("second".to_string()),
            combined_texts: None,
            kind: "prompt".to_string(),
            turn_ended: false,
        },
    );

    // The turn ends: PromptResponse runs `finish_turn`, clearing current_prompt_id, then the stashed adoption is applied (turn-start shim)
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    // No re-send (the prompt was already sent at enqueue time): only the billing refresh effect
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    // current_prompt_id was handed off to the second prompt for correlation.
    assert_eq!(
        agent_ref(&app, id).session.current_prompt_id.as_deref(),
        Some(pid_second.as_str())
    );
    assert!(app.pending_running_adoptions.is_empty());
    // Scrollback: user "first", then "Worked for", then user "second"
    assert_eq!(agent_ref(&app, id).scrollback.len(), 3);
}

/// PromptResponse FIFO handoff must forward `combined_texts` (one bubble each).
#[test]
fn prompt_response_fifo_handoff_paints_multi_bubble_combined() {
    use crate::scrollback::block::RenderBlock;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    assert!(agent_ref(&app, id).session.state.is_turn_running());

    app.pending_running_adoptions.insert(
        id,
        crate::app::acp_handler::PendingRunningAdoption {
            prompt_id: "p-combo".into(),
            text: Some("alpha\n\nbeta".into()),
            combined_texts: Some(vec!["alpha".into(), "beta".into()]),
            kind: "prompt".into(),
            turn_ended: false,
        },
    );
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    let agent = app.agents.get(&id).unwrap();
    assert_eq!(agent.session.current_prompt_id.as_deref(), Some("p-combo"));
    assert!(app.pending_running_adoptions.is_empty());
    let user_texts: Vec<_> = (0..agent.scrollback.len())
        .filter_map(|i| agent.scrollback.entry(i))
        .filter_map(|e| match &e.block {
            RenderBlock::UserPrompt(ub) => Some(ub.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(user_texts.contains(&"alpha"));
    assert!(user_texts.contains(&"beta"));
    assert!(user_texts.iter().all(|t| !t.contains("\n\n")));
}

#[test]
fn turn_end_with_empty_queue_stays_idle() {
    // Pin prompt suggestions OFF so the effect list below is deterministic regardless of the dev machine's `[ui].prompt_suggestions` config
    // Thread-local cache; see `turn_end_fetches_prompt_suggestion_when_enabled`
    crate::appearance::cache::set_prompt_suggestions(false);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    // Silent billing refresh after turn completion.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
    // Session event "Worked for" added.
    assert_eq!(agent_ref(&app, id).scrollback.len(), 1);
}

#[test]
fn multiple_queued_prompts_drain_one_per_turn() {
    // Deterministic effect lists; see `turn_end_with_empty_queue_stays_idle`
    crate::appearance::cache::set_prompt_suggestions(false);
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Send first (immediate drain).
    dispatch(Action::SendPrompt("a".into()), &mut app);
    // Queue two more while running (local queue path).
    enqueue_local(&mut app, id, "b");
    enqueue_local(&mut app, id, "c");
    assert_eq!(agent_ref(&app, id).session.queue_len(), 2);

    let end_turn = || {
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: AgentId(0),
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        })
    };

    // Turn end: drain "b" and FetchBilling
    let effects = dispatch(end_turn(), &mut app);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "b"));
    assert!(matches!(
        effects.get(1),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);

    // Turn end: drain "c" and FetchBilling
    let effects = dispatch(end_turn(), &mut app);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "c"));
    assert!(matches!(
        effects.get(1),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);

    // Turn end: FetchBilling only
    let effects = dispatch(end_turn(), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
}

#[test]
fn prompt_response_resets_turn_state() {
    // Deterministic effect lists; see `turn_end_with_empty_queue_stays_idle`
    crate::appearance::cache::set_prompt_suggestions(false);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    // Silent billing refresh after turn completion.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
    assert!(agent_ref(&app, id).turn_started_at.is_none());
    // mark_turn_finished must stamp the activity anchor used by the dashboard relative-time label
    // If this regresses, rows will show "now" forever instead of advancing
    assert!(
        agent_ref(&app, id).last_active_at.is_some(),
        "mark_turn_finished must update last_active_at"
    );
    // Session event message should be in scrollback.
    assert_eq!(agent_ref(&app, id).scrollback.len(), 1);
}

/// Turn end with prompt suggestions enabled fires the `x.ai/suggestPrompt` fetch (before the billing refresh).
/// The loaded suggestion routes back into the agent's controller by id and generation.
#[test]
fn turn_end_fetches_prompt_suggestion_when_enabled() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert_eq!(effects.len(), 2, "suggestion fetch + billing: {effects:?}");
    let Some(Effect::FetchPromptSuggestion {
        agent_id,
        generation,
        model,
        session_id,
    }) = effects.first()
    else {
        panic!("expected FetchPromptSuggestion first, got {effects:?}");
    };
    assert_eq!(*agent_id, id);
    assert!(session_id.is_some());
    // No `grok-4.6` in the test catalog and no env override, so `None` on the wire; the shell then uses its own `grok-4.6` default
    // Suggestion calls never use the session model
    assert_eq!(*model, None);

    // The loaded suggestion lands in the right agent's controller.
    let generation = *generation;
    dispatch(
        Action::TaskComplete(TaskResult::PromptSuggestionLoaded {
            agent_id: id,
            suggestion: Some("run the tests".into()),
            generation,
        }),
        &mut app,
    );
    assert_eq!(
        agent_ref(&app, id).prompt.prompt_suggestion.ghost_for(""),
        Some("run the tests")
    );
}

/// A cancelled turn never suggests (the user interrupted; silence), and any previous suggestion is wiped at the turn boundary.
#[test]
fn cancelled_turn_does_not_fetch_prompt_suggestion() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent
            .prompt
            .prompt_suggestion
            .set_suggestion_for_test("stale suggestion");
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptSuggestion { .. })),
        "cancelled turn must not fetch a suggestion: {effects:?}"
    );
    assert!(
        !agent_ref(&app, id)
            .prompt
            .prompt_suggestion
            .has_suggestion(),
        "turn boundary wipes any stale suggestion"
    );
}

/// A draft in the prompt means the user is already mid-thought: no fetch.
#[test]
fn turn_end_with_draft_does_not_fetch_prompt_suggestion() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.prompt.textarea.insert_str("half-typed follow-up");
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptSuggestion { .. })),
        "a draft suppresses the suggestion fetch: {effects:?}"
    );
}

/// The reconnect-pending early return skips the fetch gate, but the turn boundary must still wipe a stale ghost.
/// The wipe runs before the early returns.
#[test]
fn reconnect_pending_turn_end_still_wipes_prompt_suggestion() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.reconnect_pending = true;
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent
            .prompt
            .prompt_suggestion
            .set_suggestion_for_test("stale suggestion");
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptSuggestion { .. })),
        "reconnect-pending turn end must not fetch a suggestion: {effects:?}"
    );
    assert!(
        !agent_ref(&app, id)
            .prompt
            .prompt_suggestion
            .has_suggestion(),
        "turn boundary wipes the stale suggestion even on the reconnect path"
    );
}

/// A non-empty server-authoritative shared queue means more queued turns are coming: no suggestion fetch.
/// Mirrors the local `pending_prompts` guard.
#[test]
fn turn_end_with_shared_queue_does_not_fetch_prompt_suggestion() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent
            .shared_queue
            .push(crate::app::prompt_queue::QueueEntryWire {
                id: "q1".into(),
                version: 1,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "queued server-side".into(),
                position: 0,
                combined_texts: None,
            });
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPromptSuggestion { .. })),
        "a non-empty server shared queue suppresses the suggestion fetch: {effects:?}"
    );
}

/// A stale generation (a newer turn ended meanwhile) is discarded on load.
#[test]
fn stale_prompt_suggestion_generation_is_discarded() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let stale = app
        .agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .prompt_suggestion
        .begin_fetch();
    // A newer fetch supersedes the stale one.
    let _newer = app
        .agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .prompt_suggestion
        .begin_fetch();

    dispatch(
        Action::TaskComplete(TaskResult::PromptSuggestionLoaded {
            agent_id: id,
            suggestion: Some("stale".into()),
            generation: stale,
        }),
        &mut app,
    );
    assert!(
        !agent_ref(&app, id)
            .prompt
            .prompt_suggestion
            .has_suggestion()
    );
}

/// A suggestion that loads onto an idle, empty prompt is immediately visible: its `shown` impression is latched right at load.
#[test]
fn prompt_suggestion_loaded_visible_latches_shown() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = app
        .agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .prompt_suggestion
        .begin_fetch();

    dispatch(
        Action::TaskComplete(TaskResult::PromptSuggestionLoaded {
            agent_id: id,
            suggestion: Some("run the tests".into()),
            generation,
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(agent.prompt.prompt_suggestion_visible());
    assert!(
        agent.prompt.prompt_suggestion.shown_logged(),
        "visible-at-load suggestion latches its impression immediately"
    );
}

/// A suggestion that arrives while the user has a divergent draft typed is NOT visible at load: no impression is latched.
/// The latch stays armed so the `shown` fires at first actual visibility (via the prompt key path).
/// That keeps the funnel's `shown >= accepted + dismissed` invariant.
#[test]
fn prompt_suggestion_loaded_behind_divergent_draft_defers_shown() {
    crate::appearance::cache::set_prompt_suggestions(true);
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        // The user started a divergent draft while the fetch was in flight.
        agent.prompt.textarea.insert_str("x");
        agent.prompt.prompt_suggestion.begin_fetch()
    };

    dispatch(
        Action::TaskComplete(TaskResult::PromptSuggestionLoaded {
            agent_id: id,
            suggestion: Some("run the tests".into()),
            generation,
        }),
        &mut app,
    );

    let agent = app.agents.get_mut(&id).unwrap();
    assert!(
        agent.prompt.prompt_suggestion.has_suggestion(),
        "suggestion installs even when hidden by the draft"
    );
    assert!(!agent.prompt.prompt_suggestion_visible());
    assert!(
        !agent.prompt.prompt_suggestion.shown_logged(),
        "hidden-at-load suggestion must not count an impression"
    );

    // Clearing the draft makes the ghost visible; the next pass through the shared helper (key path / load path) latches the impression once
    agent.prompt.set_text("");
    agent.log_prompt_suggestion_shown_if_visible();
    assert!(agent.prompt.prompt_suggestion.shown_logged());
}

/// A context overflow (the `ContextTooLarge` block from the RetryState handler) suppresses both the redundant `TurnFailed` and the error toast.
/// Derived from the scrollback, not a session flag; the control case proves it drives suppression.
#[test]
fn prompt_response_context_overflow_suppresses_turn_failed_and_toast() {
    fn run_failed_turn(context_overflow: bool) -> (bool, bool) {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.turn_started_at = Some(std::time::Instant::now());
            if context_overflow {
                // Mirror the RetryState handler pushing the actionable block.
                agent
                    .scrollback
                    .push_block(RenderBlock::session_event(SessionEvent::ContextTooLarge {
                        model: None,
                        stated_limit: None,
                    }));
            }
        }
        dispatch(
            Action::TaskComplete(TaskResult::PromptResponse {
                agent_id: id,
                result: Err("API error (status 500): the prompt is too long for this \
                                 model's context window"
                    .to_string()),
                http_status: None,
                prompt_id: None,
            }),
            &mut app,
        );
        let has_turn_failed = (0..agent_ref(&app, id).scrollback.len()).any(|idx| {
            matches!(
                agent_ref(&app, id).scrollback.entry(idx).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::TurnFailed { .. })
            )
        });
        (has_turn_failed, app.deferred_notification.is_some())
    }

    // Control: with no ContextTooLarge block, PromptResponse still ends the turn with TurnFailed and a toast
    // Overflow copy in the error string must not change that Only a prior ContextTooLarge banner (from RetryState `error_type=context_length`) suppresses the marker
    // Only a prior ContextTooLarge banner (from RetryState `error_type=context_length`) suppresses the marker
    let (failed_block, toast) = run_failed_turn(false);
    assert!(failed_block, "baseline: a failed turn pushes TurnFailed");
    assert!(toast, "baseline: a failed turn emits an error toast");

    // With the block present, both are suppressed in favour of the actionable prompt.
    let (failed_block, toast) = run_failed_turn(true);
    assert!(
        !failed_block,
        "context overflow must suppress the redundant TurnFailed block"
    );
    assert!(!toast, "context overflow must suppress the error toast");
}

#[test]
fn prompt_response_request_failed_banner_suppresses_turn_failed_and_toast() {
    fn run_failed_turn(banner_shown: bool) -> (bool, bool) {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.turn_started_at = Some(std::time::Instant::now());
            if banner_shown {
                // Mirror the RetryState handler pushing the formatted banner.
                agent.scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::RequestFailed {
                        status: Some(500),
                        headline: "Server error (500)".into(),
                        detail: "Something went wrong on our side.".into(),
                    },
                ));
            }
        }
        dispatch(
            Action::TaskComplete(TaskResult::PromptResponse {
                agent_id: id,
                result: Err("Server error (500): Something went wrong on our side.".to_string()),
                http_status: Some(500),
                prompt_id: None,
            }),
            &mut app,
        );
        let has_turn_failed = (0..agent_ref(&app, id).scrollback.len()).any(|idx| {
            matches!(
                agent_ref(&app, id).scrollback.entry(idx).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::TurnFailed { .. })
            )
        });
        (has_turn_failed, app.deferred_notification.is_some())
    }

    let (failed_block, toast) = run_failed_turn(false);
    assert!(failed_block, "baseline: a failed turn pushes TurnFailed");
    assert!(toast, "baseline: a failed turn emits an error toast");

    let (failed_block, toast) = run_failed_turn(true);
    assert!(
        !failed_block,
        "a RequestFailed banner must suppress the redundant TurnFailed"
    );
    assert!(
        !toast,
        "a RequestFailed banner must suppress the error toast"
    );
}

/// The 401/402 race fallbacks must fire on the banner-formatted error text PromptResponse carries.
/// The RetryState notification may lose the race, so no ReAuthRequired block exists yet.
#[test]
fn prompt_response_formatted_401_suppresses_turn_failed_and_stashes_prompt() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
            text: "resend me".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::entry::EntryId::new(1),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
    }
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Request failed (401): Invalid or expired credentials".to_string()),
            http_status: Some(401),
            prompt_id: None,
        }),
        &mut app,
    );
    let agent = agent_ref(&app, id);
    let has_turn_failed = (0..agent.scrollback.len()).any(|idx| {
        matches!(
            agent.scrollback.entry(idx).map(|e| &e.block),
            Some(RenderBlock::SessionEvent(ev))
                if matches!(ev.event, SessionEvent::TurnFailed { .. })
        )
    });
    assert!(
        !has_turn_failed,
        "401 must suppress the redundant TurnFailed"
    );
    assert_eq!(
        agent
            .reauth_stashed_prompt
            .as_ref()
            .map(|p| p.text.as_str()),
        Some("resend me"),
        "401 must stash the prompt for auto-resubmit after /login"
    );
}

#[test]
fn prompt_response_formatted_402_takes_credit_limit_path() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
    }
    // http_status field absent (older shell): the status must be recovered from the formatted text
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Request failed (402): Distill usage balance exhausted".to_string()),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    let agent = agent_ref(&app, id);
    let has_turn_failed = (0..agent.scrollback.len()).any(|idx| {
        matches!(
            agent.scrollback.entry(idx).map(|e| &e.block),
            Some(RenderBlock::SessionEvent(ev))
                if matches!(ev.event, SessionEvent::TurnFailed { .. })
        )
    });
    assert!(
        !has_turn_failed,
        "a credit-limit 402 shows the upsell, not TurnFailed"
    );
}

#[test]
fn credit_limit_402_does_not_overwrite_stash_when_in_flight_cleared() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.in_flight_prompt = None;
        agent.credit_limit_stashed_prompt = Some(crate::app::agent::InFlightPrompt {
            text: "kept".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::EntryId::new(0),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
    }
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Request failed (402): Distill usage balance exhausted".to_string()),
            http_status: Some(402),
            prompt_id: None,
        }),
        &mut app,
    );
    assert_eq!(
        agent_ref(&app, id)
            .credit_limit_stashed_prompt
            .as_ref()
            .map(|p| p.text.as_str()),
        Some("kept")
    );
}

#[test]
fn prompt_response_disk_full_suppresses_turn_failed_and_toast() {
    fn run(pre_seed_disk_full: bool, with_user_echo: bool) -> (bool, bool, usize) {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.turn_started_at = Some(std::time::Instant::now());
            if pre_seed_disk_full {
                agent
                    .scrollback
                    .push_block(RenderBlock::session_event(SessionEvent::DiskFull));
            }
            if with_user_echo {
                agent
                    .scrollback
                    .push_block(RenderBlock::user_prompt("try again"));
            }
        }
        dispatch(
            Action::TaskComplete(TaskResult::PromptResponse {
                agent_id: id,
                result: Err(distill_fast_worktree::ENOSPC_OS_MESSAGE.to_string()),
                http_status: None,
                prompt_id: None,
            }),
            &mut app,
        );
        let disk_fulls = (0..agent_ref(&app, id).scrollback.len())
            .filter(|idx| {
                matches!(
                    agent_ref(&app, id).scrollback.entry(*idx).map(|e| &e.block),
                    Some(RenderBlock::SessionEvent(ev))
                        if matches!(ev.event, SessionEvent::DiskFull)
                )
            })
            .count();
        let failed = (0..agent_ref(&app, id).scrollback.len()).any(|idx| {
            matches!(
                agent_ref(&app, id).scrollback.entry(idx).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::TurnFailed { .. })
            )
        });
        (failed, app.deferred_notification.is_some(), disk_fulls)
    }

    let (failed, toast, disk_fulls) = run(true, false);
    assert!(!failed && !toast && disk_fulls == 1);

    let (failed, toast, disk_fulls) = run(false, false);
    assert!(!failed && !toast && disk_fulls == 1);

    let (failed, toast, disk_fulls) = run(true, true);
    assert!(!failed && !toast && disk_fulls == 2);
}

#[test]
fn prompt_response_routes_idle_title_through_frame_pipeline() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    // Simulate stale title/progress escapes left over from a previous tick.
    app.pending_notification_escapes = Some("stale-busy-title".into());

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    // The stale busy-title escapes must be replaced with idle-title escapes, not left holding the old busy title
    // The replacement goes through the frame pipeline (writer thread) in the correct order
    assert!(
        app.pending_notification_escapes
            .as_ref()
            .is_none_or(|s| !s.contains("stale-busy-title")),
        "stale notification escapes must be replaced on turn completion",
    );

    // The notification must be deferred (not fired immediately) so the terminal has at least one render frame to apply the idle title
    // The notification then reads the tab title for its subtitle
    assert!(
        app.deferred_notification.is_some(),
        "turn-complete notification must be deferred",
    );
    assert_eq!(
        app.deferred_notification.as_ref().unwrap().1,
        3,
        "deferred notification must wait >75 ms (Ghostty title debounce)",
    );
}

#[test]
fn turn_complete_notification_suppressed_when_queue_non_empty() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Start first turn.
    let effects = dispatch(Action::SendPrompt("first".into()), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(agent_ref(&app, id).session.state.is_turn_running());

    // Send a second prompt while the first is running: immediate server-authoritative send
    let effects = dispatch(Action::SendPrompt("second".into()), &mut app);
    let pid_second = match effects.first() {
        Some(Effect::SendPrompt { prompt_id, .. }) => prompt_id.clone(),
        other => panic!("expected immediate SendPrompt, got {other:?}"),
    };
    // Model the leader's running=second broadcast arriving before first's PromptResponse (a stashed adoption: the next turn is about to start)
    app.pending_running_adoptions.insert(
        id,
        crate::app::acp_handler::PendingRunningAdoption {
            prompt_id: pid_second,
            text: Some("second".to_string()),
            combined_texts: None,
            kind: "prompt".to_string(),
            turn_ended: false,
        },
    );

    // First turn completes; a server prompt is about to start (pending adoption), so the TurnComplete notification must be suppressed
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    // No re-send; only billing refresh. The second prompt is adopted.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    assert!(
        app.deferred_notification.is_none(),
        "notification must be suppressed while another turn is about to start",
    );
    assert!(
        app.pending_notification_escapes.is_none(),
        "idle title escapes must also be suppressed while prompt queue is non-empty",
    );

    // Second turn completes: queue is now empty, notification must fire
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    assert!(
        app.deferred_notification.is_some(),
        "notification must fire when prompt queue is empty",
    );
}

/// Regression: cancelling while prompts are queued must hand the queue to the agent untouched.
/// The FRONT queued prompt runs next (promoted server-side) and the rest stay queued in order.
/// The authoritative `x.ai/queue/changed` rebroadcast (not client-side prediction) updates the mirror.
#[test]
fn cancel_hands_queue_to_agent_without_reordering() {
    use crate::app::prompt_queue::{QueueChanged, QueueEntryWire};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let sid = "test-session".to_string();

    // Running turn (`p-run`) plus three prompts typed while running
    // They exist as optimistic echoes (no confirming broadcast yet) and are mirrored into `agent.shared_queue`
    // That is exactly how the immediate-send path leaves things
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p-run".into());
    }
    app.push_optimistic_prompt_echo(&sid, "q1", "one", "prompt");
    app.push_optimistic_prompt_echo(&sid, "q2", "two", "prompt");
    app.push_optimistic_prompt_echo(&sid, "q3", "three", "prompt");
    {
        let snapshot = app.shared_prompt_queue(&sid).cloned().unwrap();
        app.agents.get_mut(&id).unwrap().shared_queue = snapshot;
    }

    // Cancel: the input stays empty and no queued prompt is dropped client-side; the agent promotes the front (q1) and rebroadcasts
    let effects = dispatch(Action::CancelTurn, &mut app);
    assert!(
        agent_ref(&app, id).prompt.text().is_empty(),
        "cancel must not restore a queued prompt into the input"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CancelTurn { .. })),
        "must emit CancelTurn, got {effects:?}"
    );
    // q1's optimistic echo is left in place: it runs next, and the broadcast (not a client-side retire) reconciles it
    assert!(
        app.optimistic_prompt_echoes
            .get(&sid)
            .is_some_and(|v| v.iter().any(|e| e.id == "q1")),
        "front prompt's optimistic echo must survive the cancel (it runs next)"
    );

    // The agent tears down p-run, promotes q1 (the front) as the running turn, and rebroadcasts the authoritative remaining queue
    // That queue is [q2, q3] with q1 running (dropped from the pending list)
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![
            QueueEntryWire {
                id: "q2".into(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "two".into(),
                position: 0,
                combined_texts: None,
            },
            QueueEntryWire {
                id: "q3".into(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: "three".into(),
                position: 1,
                combined_texts: None,
            },
        ],
        running_prompt_id: Some("q1".into()),

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });

    // Post-broadcast the queue is exactly [q2, q3] in order; q1 is now the running turn, nothing resurrects or reorders
    let texts: Vec<String> = app
        .shared_prompt_queue(&sid)
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| e.text.clone())
        .collect();
    assert_eq!(
        texts,
        vec!["two".to_string(), "three".to_string()],
        "front runs next; remaining queue stays in order"
    );
}

/// Regression for the "queued message renders 2×" dup: a shell/proxy that re-keys the prompt must still reconcile the echo by kind+text.
/// (Re-keyed: the broadcast row and later `running_prompt_id` carry a DIFFERENT id than the pager's optimistic echo.)
/// Without the fallback the echo is pinned forever.
#[test]
fn rekeyed_broadcast_reconciles_optimistic_echo_by_text() {
    use crate::app::prompt_queue::{QueueChanged, QueueEntryWire};

    let mut app = test_app_with_agent();
    let sid = "test-session".to_string();

    app.push_optimistic_prompt_echo(&sid, "pager-id", "run the tests", "prompt");

    // The shell accepted the message under its own id.
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![QueueEntryWire {
            id: "shell-id".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "run the tests".into(),
            position: 0,
            combined_texts: None,
        }],
        running_prompt_id: None,

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });

    let rows = app.shared_prompt_queue(&sid).cloned().unwrap_or_default();
    assert_eq!(
        rows.iter().filter(|e| e.text == "run the tests").count(),
        1,
        "the re-keyed authoritative row must supersede the echo (no 2x row)"
    );
    assert!(
        !app.optimistic_prompt_echoes.contains_key(&sid),
        "the echo must be retired by the text-level reconcile"
    );

    // Same for the promote broadcast: echo pending, then the re-keyed row starts running (dropped from entries, carried as running_prompt_id)
    app.push_optimistic_prompt_echo(&sid, "pager-id-2", "second message", "prompt");
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![QueueEntryWire {
            id: "shell-id-2".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "second message".into(),
            position: 0,
            combined_texts: None,
        }],
        running_prompt_id: None,

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![],
        running_prompt_id: Some("shell-id-2".into()),

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });
    assert!(
        app.shared_prompt_queue(&sid).is_none(),
        "no stale echo row may survive the promote (the turn renders the block)"
    );

    // Running-row fallback: the echo lands AFTER the broadcast that listed the re-keyed row (reconnect-replay shape)
    // The promote must retire it via the prior snapshot's kind+text, not leave it pinned forever
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![QueueEntryWire {
            id: "shell-id-3".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "third message".into(),
            position: 0,
            combined_texts: None,
        }],
        running_prompt_id: None,

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });
    app.push_optimistic_prompt_echo(&sid, "pager-id-3", "third message", "prompt");
    app.apply_queue_changed(QueueChanged {
        session_id: sid.clone(),
        entries: vec![],
        running_prompt_id: Some("shell-id-3".into()),

        running_text: None,
        running_kind: None,
        running_combined_texts: None,
    });
    assert!(
        app.shared_prompt_queue(&sid).is_none(),
        "the promote's running row (kind+text) must retire the late echo"
    );
    assert!(
        !app.optimistic_prompt_echoes.contains_key(&sid),
        "no echo may be pinned after its message started running"
    );
}

#[test]
fn prompt_response_disarms_pending_reconcile() {
    // Healthy path: the RPC response arrives within the grace window; the marker must be disarmed so the reconcile can never double-fire
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnCancelling;
        agent.session.current_prompt_id = Some("pid-stuck".into());
    }
    arm_reconcile(
        &mut app,
        id,
        "pid-stuck",
        "cancelled",
        std::time::Duration::ZERO,
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled).meta(
                serde_json::json!({ "promptId": "pid-stuck" })
                    .as_object()
                    .cloned(),
            )),
            http_status: None,
            prompt_id: Some("pid-stuck".into()),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(
        agent.pending_turn_end_reconcile.is_none(),
        "PromptResponse for the armed prompt must disarm the reconcile"
    );
    assert!(
        agent.session.state.is_idle(),
        "the normal PromptResponse teardown still runs"
    );
}

#[test]
fn prompt_response_resets_cancelling_to_idle() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnCancelling;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    // Silent billing refresh after turn completion.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
    // Cancellation produces a "Turn cancelled" session event.
    assert_eq!(agent_ref(&app, id).scrollback.len(), 1);
}

#[test]
fn cancel_with_queued_prompt_drains_on_completion() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Send "first": drains immediately (agent was idle)
    dispatch(Action::SendPrompt("first".into()), &mut app);
    assert!(agent_ref(&app, id).session.state.is_turn_running());

    // Enqueue follow-up prompt while first is running (local queue path).
    enqueue_local(&mut app, id, "queued");
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);

    // User cancels the running turn.
    let effects = dispatch(Action::CancelTurn, &mut app);
    assert_eq!(effects.len(), 1);
    assert!(agent_ref(&app, id).session.state.is_cancelling());
    // The queued prompt remains queued until the cancelled turn finishes.
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);

    // PromptResponse for cancelled first turn arrives.
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert_eq!(effects.len(), 2);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "queued"));
    assert!(matches!(
        effects.get(1),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
}

#[test]
fn cancel_with_empty_queue_stays_idle() {
    // When cancelled turn ends and queue is empty, behavior is unchanged.
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnCancelling;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );
    // Silent billing refresh after turn completion.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
}

#[test]
fn send_prompt_stashes_in_flight_for_restore() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("hello world".into()), &mut app);
    let stash = agent_ref(&app, id)
        .session
        .in_flight_prompt
        .as_ref()
        .expect("in-flight prompt should be stashed");
    assert_eq!(stash.text, "hello world");
    assert!(stash.images.is_empty());
}

#[test]
fn cancel_with_multiple_queued_prompts_drains_only_front_prompt() {
    // Cancel completion should only resume the next queued prompt, not every queued prompt at once
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Simulate: a turn was cancelled with multiple queued follow-ups.
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnCancelling;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .enqueue_prompt("queued-1".into());
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .enqueue_prompt("queued-2".into());

    // PromptResponse arrives; it should send only the front queued prompt
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert_eq!(effects.len(), 2);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "queued-1"));
    assert!(matches!(
        effects.get(1),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);
    assert_eq!(
        agent_ref(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued-2")
    );
}

#[test]
fn cancel_drain_is_blocked_when_editing_front_prompt() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnCancelling;
    app.agents.get_mut(&id).unwrap().turn_started_at = Some(std::time::Instant::now());
    let queued_id = app
        .agents
        .get_mut(&id)
        .unwrap()
        .session
        .enqueue_prompt("queued-1".into());
    app.agents
        .get_mut(&id)
        .unwrap()
        .session
        .enqueue_prompt("queued-2".into());
    app.agents.get_mut(&id).unwrap().prompt_mode = PromptMode::EditingQueued {
        id: queued_id,
        original: "queued-1".into(),
        server_id: None,
        kind: crate::app::agent::QueueEntryKind::Prompt,
    };

    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    // Drain blocked but billing refresh still happens.
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::FetchBilling { silent: true, .. })
    ));
    assert!(agent_ref(&app, id).session.state.is_idle());
    assert_eq!(agent_ref(&app, id).session.queue_len(), 2);
    assert_eq!(
        agent_ref(&app, id)
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued-1")
    );
    assert_eq!(
        agent_ref(&app, id)
            .session
            .pending_prompts
            .get(1)
            .map(|p| p.text.as_str()),
        Some("queued-2")
    );
}

/// An IDLE bash submit is UNCHANGED: local enqueue and drain (no optimistic shared-queue echo).
#[test]
fn bash_while_idle_stays_on_local_path() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendBashCommand("ls -la".into()), &mut app);
    // An idle submit locally enqueues and immediately drains, emitting SendBashCommand
    assert!(matches!(
        effects.as_slice(),
        [Effect::SendBashCommand { .. }]
    ));
    // No optimistic shared-queue echo on the local path.
    assert!(app.shared_prompt_queue("test-session").is_none());
    // Drain started the turn and set the bash-focus flag locally.
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    assert!(agent_ref(&app, id).bash_turn);
}

/// A bash command submitted before the session binds is queued, clears the composer, and still lands in up-arrow history with its `! ` prefix.
/// It waits for the drain rather than emitting an effect.
#[test]
fn bash_before_the_session_binds_is_queued_and_recorded() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;

    let effects = dispatch(Action::SendBashCommand("ls -la".into()), &mut app);

    assert!(effects.is_empty(), "nothing may send yet, got {effects:?}");
    let agent = agent_ref(&app, id);
    assert_eq!(agent.session.queue_len(), 1, "the command must be queued");
    assert!(
        agent.prompt.text().is_empty(),
        "the composer must be cleared"
    );
    assert_eq!(
        agent.session.prompt_history.first().map(String::as_str),
        Some("! ls -la"),
        "up-arrow history must record the command"
    );
}

#[test]
fn send_prompt_blocked_during_reconnect() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.reconnect_pending = true;

    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(effects.is_empty());
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
    assert!(agent_ref(&app, id).toast.is_some());
}

fn install_post_turn_review(app: &mut AppView, id: AgentId) {
    let agent = app.agents.get_mut(&id).expect("agent");
    agent.plan_mode_active = true;
    agent.kept_plan = crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
    agent.post_turn_plan_review = true;
    let stashed = agent.prompt.stash();
    agent.plan_approval_view = Some(
        crate::views::plan_approval_view::PlanApprovalViewState::after_turn(
            "CreatePlan".to_owned(),
            "# Build it\n".to_owned(),
            stashed,
        ),
    );
}

fn plan_review_closed(
    agent: &AgentView,
) -> Vec<crate::views::plan_approval_view::PlanReviewOutcome> {
    agent
        .scrollback
        .session_events()
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::PlanReviewClosed { outcome, .. } => Some(outcome),
            _ => None,
        })
        .collect()
}

#[test]
fn execute_plan_default_confirm_commits_approved() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".into(),
            plan_file_uri: None,
        },
        &mut app,
    );
    assert!(matches!(effects.as_slice(), [Effect::ExecutePlan { .. }]));

    let agent = app.agents.get_mut(&id).expect("agent");
    // Accept is PlanExecuting; Default only forgets last_plan.
    agent.commit_post_turn_plan_approved();
    crate::app::acp_handler::detect_plan_mode_change_replayed(
        &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
        agent,
        false,
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert_eq!(
        plan_review_closed(agent),
        vec![crate::views::plan_approval_view::PlanReviewOutcome::Approved]
    );
    assert!(
        !agent.kept_plan.is_kept(),
        "Default confirm must forget the keep so re-entering Plan cannot reopen"
    );
    assert!(
        agent.execute_plan.is_some(),
        "PromptResponse still needs the build id"
    );
    assert!(
        agent.plan_mode_pending.is_none(),
        "Default already cleared pending; commit must not restage Some(false)"
    );
    assert!(!agent.plan_mode_active);
}

#[test]
fn revise_send_success_commits_review_after_gates() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let effects = dispatch(Action::RevisePlan("add a rollback".into()), &mut app);
    assert!(
        matches!(effects.as_slice(), [Effect::SendPrompt { .. }]),
        "revise notes must send, got {effects:?}"
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert_eq!(
        agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
        Some("Plan revision sent.")
    );
}

#[test]
fn published_cycle_leave_abandons_and_blocks_approve() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);
    app.agents
        .get_mut(&id)
        .expect("agent")
        .apply_session_modes(Some(acp::SessionModeState::new(
            "plan",
            vec![
                acp::SessionMode::new("default", "Agent"),
                acp::SessionMode::new("ask", "Ask"),
                acp::SessionMode::new("plan", "Plan"),
            ],
        )));

    let cycle = dispatch(Action::CycleMode, &mut app);
    assert!(
        matches!(cycle.as_slice(), [Effect::SetSessionMode { .. }]),
        "published Shift+Tab must still emit set_mode, got {cycle:?}"
    );
    let agent = agent_ref(&app, id);
    assert!(
        agent.plan_approval_view.is_some(),
        "review stays until Default confirms leave-Plan"
    );
    assert!(
        plan_review_closed(agent).is_empty(),
        "Abandoned must wait for Default"
    );
    assert!(agent.kept_plan.is_kept());
    assert_eq!(agent.plan_mode_pending, Some(false));

    let build = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".into(),
            plan_file_uri: None,
        },
        &mut app,
    );
    assert!(
        build.is_empty(),
        "approve must not start after Shift+Tab leave-Plan"
    );
    assert!(agent_ref(&app, id).execute_plan.is_none());

    let agent = app.agents.get_mut(&id).expect("agent");
    crate::app::acp_handler::detect_plan_mode_change_replayed(
        &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
        agent,
        false,
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert_eq!(
        plan_review_closed(agent),
        vec![crate::views::plan_approval_view::PlanReviewOutcome::Abandoned]
    );
    assert!(!agent.kept_plan.is_kept());
    assert!(
        agent.plan_mode_pending.is_none(),
        "Default confirm must not restage pending as Some(false)"
    );
}

#[test]
fn execute_plan_refuse_without_session_keeps_review() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);
    app.agents.get_mut(&id).expect("agent").session.session_id = None;

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".into(),
            plan_file_uri: None,
        },
        &mut app,
    );
    assert!(effects.is_empty(), "no session must not fire ExecutePlan");
    let agent = agent_ref(&app, id);
    assert!(
        agent.kept_plan.is_kept(),
        "a toast must not drop the waiting plan"
    );
    assert!(
        agent.plan_approval_view.is_some(),
        "review stays mounted; refuse only toasts"
    );
    assert!(plan_review_closed(agent).is_empty());
    assert_eq!(
        agent.toast.as_ref().map(|(msg, _)| msg.as_str()),
        Some(crate::app::dispatch::ctx::NO_SESSION_NOTICE)
    );
}

#[test]
fn prompt_response_keeps_a_post_turn_plan_review() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("later turn".into()), &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.plan_mode_active = true;
        agent.post_turn_plan_review = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.open_post_turn_plan_review();
        agent
            .plan_approval_view
            .as_mut()
            .expect("review")
            .comments
            .push(crate::views::plan_approval_view::PlanComment {
                id: 1,
                line_range: 1..2,
                text: "keep this".to_owned(),
            });
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    let pav = agent_ref(&app, id)
        .plan_approval_view
        .as_ref()
        .expect("post-turn review must survive a later PromptResponse");
    assert_eq!(
        pav.comments.first().map(|comment| comment.text.as_str()),
        Some("keep this")
    );
}

fn plan_mode_agent_with_stashed_plan(app: &mut AppView) -> AgentId {
    let id = AgentId(0);
    dispatch(Action::SendPrompt("plan turn".into()), app);
    let agent = app.agents.get_mut(&id).unwrap();
    agent.plan_mode_active = true;
    agent.post_turn_plan_review = true;
    agent.kept_plan = crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
    id
}

#[test]
fn prompt_response_opens_post_turn_review_after_a_successful_plan_turn() {
    let mut app = test_app_with_agent();
    let id = plan_mode_agent_with_stashed_plan(&mut app);

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    assert!(
        agent_ref(&app, id).plan_approval_view.is_some(),
        "a successful CreatePlan turn must open approve/build"
    );
}

#[test]
fn prompt_response_keeps_the_plan_file_after_a_failed_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("kept.plan.md");
    std::fs::write(&path, "# Kept plan\n").expect("write");
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("plan turn".into()), &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.plan_mode_active = true;
        agent.post_turn_plan_review = true;
        agent.kept_plan = crate::app::agent_view::KeptPlan::kept(None, Some(path.clone()));
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("plan turn failed".to_string()),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    {
        let agent = agent_ref(&app, id);
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .and_then(|pav| pav.plan_content.as_deref()),
            Some("# Kept plan\n"),
            "a failed keep must still open approve/build from the file"
        );
        assert_eq!(
            agent.kept_plan.path(),
            Some(path.as_path()),
            "a failed turn must leave the kept file"
        );
    }

    dispatch(Action::SendPrompt("retry".into()), &mut app);
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    let pav = agent_ref(&app, id)
        .plan_approval_view
        .as_ref()
        .expect("the next successful turn must reopen from the kept file");
    assert_eq!(pav.plan_content.as_deref(), Some("# Kept plan\n"));
}

#[test]
fn execute_plan_leaves_the_daemon_kickoff_visible() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.plan_mode_active = true;
        agent.post_turn_plan_review = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
    }

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".to_owned(),
            plan_file_uri: None,
        },
        &mut app,
    );
    assert!(matches!(effects.as_slice(), [Effect::ExecutePlan { .. }]));
    assert!(
        !agent_ref(&app, id).session.tracker.expects_user_echo(),
        "no local user row: the daemon kickoff must not be dropped as an echo"
    );
}

fn confirm_default_mode(agent: &mut AgentView) {
    crate::app::acp_handler::detect_plan_mode_change_replayed(
        &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
        agent,
        false,
    );
}

#[test]
fn failed_execute_plan_reopens_review() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".to_owned(),
            plan_file_uri: None,
        },
        &mut app,
    );
    let prompt_id = match effects.as_slice() {
        [Effect::ExecutePlan { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("expected ExecutePlan, got {other:?}"),
    };

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("execute plan failed".to_owned()),
            http_status: None,
            prompt_id: Some(prompt_id),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(
        agent.kept_plan.is_kept(),
        "a failed build start (still in Plan) must leave the waiting plan"
    );
    assert!(
        agent.plan_approval_view.is_some(),
        "a failed build start must reopen approve/build"
    );
    assert!(
        agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
        "a failed start still in Plan must not leave Plan"
    );
}

#[test]
fn failed_execute_plan_after_default_confirm_forgets_the_keep() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".to_owned(),
            plan_file_uri: None,
        },
        &mut app,
    );
    let prompt_id = match effects.as_slice() {
        [Effect::ExecutePlan { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("expected ExecutePlan, got {other:?}"),
    };

    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.commit_post_turn_plan_approved();
        confirm_default_mode(agent);
        assert!(
            !agent.kept_plan.is_kept(),
            "Default confirm always forgets the waiting plan; ExecutePlan only keeps the prompt id"
        );
        assert!(
            agent.plan_approval_view.is_none(),
            "Default confirm is ExecutePlan accept: close approve/build"
        );
        assert_eq!(
            plan_review_closed(agent),
            vec![crate::views::plan_approval_view::PlanReviewOutcome::Approved]
        );
        assert_eq!(
            agent.execute_plan_prompt_id(),
            Some(prompt_id.as_str()),
            "Default confirm must still match the build PromptResponse"
        );
        assert!(
            agent.plan_mode_pending.is_none(),
            "confirmed Default must not restage pending as Some(false)"
        );
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("execute plan failed".to_owned()),
            http_status: None,
            prompt_id: Some(prompt_id),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(
        !agent.kept_plan.is_kept(),
        "a started build must not leave a keep after the turn fails"
    );
    assert!(
        agent.plan_approval_view.is_none(),
        "a started build must not reopen approve/build"
    );
    assert!(agent.execute_plan.is_none());
    assert!(
        !agent.plan_mode_pending.unwrap_or(agent.plan_mode_active),
        "Default confirm already left Plan; PromptResponse must not re-enter"
    );
    assert!(
        agent.plan_mode_pending.is_none(),
        "PromptResponse after Default confirm must not restage pending"
    );
}

#[test]
fn leave_plan_before_end_turn_forgets_keep_without_a_verdict() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).expect("agent");
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.post_turn_plan_review = true;
    }

    let effects = dispatch(
        Action::SetPlanMode(crate::app::actions::PlanModeKind::Off),
        &mut app,
    );
    assert!(
        matches!(
            effects.first(),
            Some(Effect::SetSessionMode { mode_id, .. }) if &*mode_id.0 != "plan"
        ),
        "leave-Plan must still emit session/set_mode, got {effects:?}"
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert!(
        plan_review_closed(agent).is_empty(),
        "leave before EndTurn never showed a review; no abandoned row"
    );
    assert!(
        !agent.kept_plan.is_kept(),
        "EndTurn must not have a keep to open"
    );
    assert_eq!(agent.plan_mode_pending, Some(false));

    app.agents
        .get_mut(&id)
        .expect("agent")
        .open_post_turn_plan_review();
    assert!(
        agent_ref(&app, id).plan_approval_view.is_none(),
        "pending-off must block approve/build after CreatePlan"
    );
}

#[test]
fn abandon_success_commits_review_and_sets_mode() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let effects = dispatch(
        Action::SetPlanMode(crate::app::actions::PlanModeKind::Off),
        &mut app,
    );
    assert!(
        matches!(
            effects.first(),
            Some(Effect::SetSessionMode { mode_id, .. }) if &*mode_id.0 != "plan"
        ),
        "expected SetSessionMode away from plan, got {effects:?}"
    );
    let agent = agent_ref(&app, id);
    assert!(
        agent.plan_approval_view.is_some(),
        "review stays until Default confirms abandon"
    );
    assert!(plan_review_closed(agent).is_empty());
    assert_eq!(agent.plan_mode_pending, Some(false));
    assert!(agent.kept_plan.is_kept());

    let agent = app.agents.get_mut(&id).expect("agent");
    crate::app::acp_handler::detect_plan_mode_change_replayed(
        &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("default")),
        agent,
        false,
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert_eq!(
        plan_review_closed(agent),
        vec![crate::views::plan_approval_view::PlanReviewOutcome::Abandoned]
    );
    assert!(!agent.kept_plan.is_kept());
}

#[test]
fn execute_plan_refuses_while_cancelling() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    app.agents.get_mut(&id).unwrap().session.in_flight_prompt = None;
    let _ = dispatch(Action::CancelTurn, &mut app);
    assert!(agent_ref(&app, id).session.state.is_cancelling());

    let effects = dispatch(
        Action::ExecutePlan {
            plan_file_content: "# Build it\n".into(),
            plan_file_uri: None,
        },
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "cancelling is not idle, got {effects:?}"
    );
    assert!(agent_ref(&app, id).execute_plan.is_none());
    assert!(agent_ref(&app, id).plan_approval_view.is_some());
}

#[test]
fn cancelled_create_plan_turn_opens_post_turn_review() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).expect("agent");
        agent.plan_mode_active = true;
        agent.kept_plan =
            crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.post_turn_plan_review = true;
        agent.begin_local_turn("p-plan");
    }
    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);
    let agent = agent_ref(&app, id);
    assert!(
        agent
            .plan_approval_view
            .as_ref()
            .is_some_and(|pav| pav.is_after_turn()),
        "a cancelled keep must still open approve/build"
    );
}

#[test]
fn ask_mode_update_forgets_the_keep() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);

    let agent = app.agents.get_mut(&id).expect("agent");
    crate::app::acp_handler::detect_plan_mode_change_replayed(
        &acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new("ask")),
        agent,
        false,
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_approval_view.is_none());
    assert!(
        !agent.kept_plan.is_kept(),
        "Ask matches the worker: last_plan is gone"
    );
    assert!(plan_review_closed(agent).is_empty());
}

#[test]
fn set_session_mode_failed_clears_abandoned_intent() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    install_post_turn_review(&mut app, id);
    let _ = dispatch(
        Action::SetPlanMode(crate::app::actions::PlanModeKind::Off),
        &mut app,
    );
    assert_eq!(
        agent_ref(&app, id).pending_post_turn_commit,
        Some(crate::app::agent_view::PostTurnPlanCommit::Abandoned)
    );
    let session_id = agent_ref(&app, id)
        .session
        .session_id
        .clone()
        .expect("session");
    let _ = dispatch(
        Action::TaskComplete(TaskResult::SetSessionModeFailed { session_id }),
        &mut app,
    );
    let agent = agent_ref(&app, id);
    assert!(agent.plan_mode_pending.is_none());
    assert!(
        agent.pending_post_turn_commit.is_none(),
        "failed set_mode must drop Abandoned so a later Default cannot commit it"
    );
    assert!(agent.plan_approval_view.is_some());
}

#[test]
fn send_bash_command_blocked_during_reconnect() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.reconnect_pending = true;

    let effects = dispatch(Action::SendBashCommand("ls".into()), &mut app);
    assert!(effects.is_empty());
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
    assert!(agent_ref(&app, id).toast.is_some());
}

#[test]
fn prompt_response_does_not_drain_during_reconnect() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    // Start a turn and enqueue a second prompt.
    dispatch(Action::SendPrompt("first".into()), &mut app);
    assert!(agent_ref(&app, id).session.state.is_turn_running());
    enqueue_local(&mut app, id, "second");
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);

    // Simulate reconnect_pending before PromptResponse arrives.
    app.reconnect_pending = true;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn)),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    // "second" must NOT be drained.
    assert!(
        effects
            .iter()
            .all(|e| !matches!(e, Effect::SendPrompt { .. })),
        "should not drain queue during reconnect, got: {effects:?}"
    );
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);
}

#[test]
fn send_prompt_works_after_reconnect_clears() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    app.reconnect_pending = true;
    let effects = dispatch(Action::SendPrompt("blocked".into()), &mut app);
    assert!(effects.is_empty());

    // Clear reconnect_pending; prompt should work now
    app.reconnect_pending = false;
    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects.first(), Some(Effect::SendPrompt { .. })));
    assert!(agent_ref(&app, id).session.state.is_turn_running());
}

#[test]
fn switch_model_holds_prompt_until_complete() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let model_id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));

    dispatch(
        Action::SwitchModel {
            model_id: model_id.clone(),
            effort: None,
        },
        &mut app,
    );
    assert!(agent_ref(&app, id).session.model_switch_pending);

    let effects = dispatch(Action::SendPrompt("hello".into()), &mut app);
    assert!(
        effects.is_empty(),
        "prompt must be queued while model switch is pending"
    );
    assert_eq!(agent_ref(&app, id).session.queue_len(), 1);

    let effects = dispatch(
        Action::TaskComplete(TaskResult::SwitchModelComplete {
            agent_id: id,
            model_id,
            effort: None,
            result: Ok(None),
            prev_model_id: None,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { .. }))
    );
    assert_eq!(agent_ref(&app, id).session.queue_len(), 0);
}

#[test]
fn slash_compact_enqueues_command() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt("/compact".into()), &mut app);
    // /compact enqueues as Command and drains immediately (agent was idle).
    assert_eq!(effects.len(), 1);
    assert!(matches!(
        effects.first(),
        Some(Effect::Compact {
            user_context: None,
            ..
        })
    ));
    assert!(agent_ref(&app, id).prompt.text().is_empty());
}

#[test]
fn edit_prompt_direct_route_preserves_nonempty_draft_and_elements() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("existing draft");

    let effects = dispatch(Action::EditPromptExternal, &mut app);
    assert!(effects.is_empty());
    assert_eq!(agent_ref(&app, id).prompt.text(), "existing draft");
    assert!(matches!(
        app.pending_editor,
        Some(crate::app::external_editor::PendingEditorRequest::PromptDraft {
            ref original_text,
            ..
        }) if original_text == "existing draft"
    ));
    assert!(agent_ref(&app, id).session.pending_prompts.is_empty());

    app.pending_editor = None;
    let agent = app.agents.get_mut(&id).unwrap();
    agent.prompt.set_text("");
    agent.prompt.textarea.insert_element(
        "@src/lib.rs",
        crate::views::prompt_widget::KIND_FILE_REF,
        None,
    );
    let chip_text = agent.prompt.text().to_owned();
    let _ = dispatch(Action::EditPromptExternal, &mut app);
    assert!(app.pending_editor.is_none());
    assert_eq!(agent_ref(&app, id).prompt.text(), chip_text);
    assert!(!agent_ref(&app, id).prompt.textarea.elements().is_empty());
}

#[test]
fn typed_edit_prompt_command_opens_only_an_empty_draft() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.screen_mode = crate::app::ScreenMode::Minimal;
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("/edit-prompt");

    let effects = dispatch(Action::SendPrompt("/edit-prompt".into()), &mut app);
    assert!(effects.is_empty());
    assert!(agent_ref(&app, id).prompt.text().is_empty());
    assert!(matches!(
        app.pending_editor,
        Some(crate::app::external_editor::PendingEditorRequest::PromptDraft {
            ref original_text,
            ..
        }) if original_text.is_empty()
    ));
}

#[test]
fn palette_dispatch_preserves_prompt_draft() {
    // Regression for the bug where picking a SlashCommand entry from the Ctrl-P palette wiped whatever the user had typed
    // The palette routes through Action::SendSlashCommandPreservingDraft instead of Action::SendPrompt
    // That arm calls dispatch_send_prompt_inner with clear_prompt=false
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    // User has a draft typed in the prompt.
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .textarea
        .insert_str("hello");
    let initial_history_len = agent_ref(&app, id).session.prompt_history.len();

    let effects = dispatch(
        Action::SendSlashCommandPreservingDraft("/compact".into()),
        &mut app,
    );

    // The slash command still runs end-to-end: it produces the same Compact effect that Action::SendPrompt would
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects.first(), Some(Effect::Compact { .. })));
    // But the user's draft text is intact.
    assert_eq!(agent_ref(&app, id).prompt.text(), "hello");
    // And the slash command was not inserted into prompt history, because the user didn't type it
    assert_eq!(
        agent_ref(&app, id).session.prompt_history.len(),
        initial_history_len,
    );
}

#[test]
fn slash_compact_with_context_enqueues_command() {
    let mut app = test_app_with_agent();
    let effects = dispatch(
        Action::SendPrompt("/compact focus on auth".into()),
        &mut app,
    );
    assert_eq!(effects.len(), 1);
    assert!(matches!(effects.first(),Some(
        Effect::Compact {
            user_context: Some(ctx),
            ..
        }) if ctx == "focus on auth"
    ));
}

#[test]
fn slash_unknown_command_passthrough_enqueues_prompt() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt("/unknown-cmd arg1".into()), &mut app);
    // An unknown slash command passes through and is enqueued as a prompt
    assert_eq!(effects.len(), 1);
    assert!(
        matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "/unknown-cmd arg1")
    );
    assert!(agent_ref(&app, id).prompt.text().is_empty());
}

#[test]
fn non_slash_prompt_still_works() {
    // Verify normal prompts are unaffected by the slash dispatch rewrite.
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    let effects = dispatch(Action::SendPrompt("hello world".into()), &mut app);
    assert_eq!(effects.len(), 1);
    assert!(
        matches!(effects.first(), Some(Effect::SendPrompt { text, .. }) if text == "hello world")
    );
    assert!(agent_ref(&app, id).prompt.text().is_empty());
}

#[test]
fn submit_question_answers_cancel_clears_local_modal_and_restores_prompt() {
    // Full-stack contract test: cancel through the public `submit_question_answers` entry point must
    // (b) restore the stashed prompt text and cursor
    // (c) return InputOutcome::Changed (no Action) and silently drop the directive carried by LocalQuestionKind::Fork.
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};

    let mut app = fork_test_app();
    let id = AgentId(0);

    // Plant text in the prompt that should be restored on cancel.
    app.agents
        .get_mut(&id)
        .unwrap()
        .prompt
        .set_text("user typed text");
    let stashed = app.agents.get_mut(&id).unwrap().prompt.stash();

    let q = Question {
        question: "fork worktree?".into(),
        options: (0..2)
            .map(|_| QuestionOption {
                label: "opt".into(),
                description: String::new(),
                preview: None,
                id: None,
            })
            .collect(),
        multi_select: Some(false),
        id: None,
    };
    app.agents.get_mut(&id).unwrap().question_view = Some(
        QuestionViewState::new("local-fork".into(), vec![q], stashed).with_local_kind(
            LocalQuestionKind::Fork {
                directive: Some("dropped-on-cancel".into()),
            },
        ),
    );

    // Cancel through the production submit path (skipped == true).
    let outcome = app
        .agents
        .get_mut(&id)
        .unwrap()
        .submit_question_answers_for_test(true);

    assert!(
        matches!(outcome, crate::app::app_view::InputOutcome::Changed),
        "cancel must return Changed, got {outcome:?}"
    );
    assert!(
        agent_ref(&app, id).question_view.is_none(),
        "question_view must be cleared after cancel"
    );
    assert_eq!(
        agent_ref(&app, id).prompt.text(),
        "user typed text",
        "prompt text must be restored from stash"
    );
}

#[test]
fn entry_title_prefers_generated_summary_over_first_prompt() {
    use crate::scrollback::block::RenderBlock;
    use crate::views::session_title::entry_title;
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.generated_session_title = Some("LLM short title".into());
    agent
        .scrollback
        .push_block(RenderBlock::user_prompt("longer first user prompt text"));
    let title = entry_title(agent_ref(&app, AgentId(0)));
    assert_eq!(title, "LLM short title");
}

#[test]
fn entry_title_falls_back_to_first_user_prompt() {
    use crate::scrollback::block::RenderBlock;
    use crate::views::session_title::entry_title;
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent
        .scrollback
        .push_block(RenderBlock::user_prompt("first message in this agent"));
    let title = entry_title(agent_ref(&app, AgentId(0)));
    assert_eq!(title, "first message in this agent");
}

#[test]
fn prompt_history_loaded_refreshes_open_history_search_with_current_query() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);

    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.prompt_history = vec!["first prompt".into(), "second prompt".into()];
        let history = agent.combined_prompt_history();
        assert!(agent.prompt.history_search.activate(&history, ""));
        agent.prompt.set_text("third");
        agent.prompt.history_search.update_query("third");
        for _ in 0..100 {
            agent.prompt.history_search.poll();
            if agent.prompt.history_search.result_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(agent.prompt.history_search.result_count(), 0);
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptHistoryLoaded {
            agent_id: id,
            prompts: vec![
                "first prompt".into(),
                "second prompt".into(),
                "third prompt".into(),
            ],
        }),
        &mut app,
    );

    let agent = app.agents.get_mut(&id).unwrap();
    let mut delivered = false;
    for _ in 0..100 {
        if agent.prompt.history_search.poll()
            && agent.prompt.history_search.result_count() == 1
            && agent.prompt.history_search.selected_text() == Some("third prompt")
        {
            delivered = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(delivered, "refresh should preserve the active query");
    assert_eq!(agent.prompt.history_search.selected, 0);
    assert!(!agent.session.prompt_history_loading);

    agent.prompt.history_search.deactivate();
}

/// Paste-then-immediate-send race (agent prompt): Enter fires before the deferred probe completes and the Cmd+V'd image must not be dropped.
/// The send is stashed while the probe is in flight and re-issued on completion, so the sent content carries the image.
#[test]
fn agent_send_before_paste_probe_keeps_image() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.prompt.set_text("look at this");
    }
    // Cmd+V an image: the probe defers (raster snapshot present)
    crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook::with_raster(
        None,
    ));
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let _ = agent
            .handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    }
    let ctx = agent_ref(&app, id)
        .pending_effects
        .iter()
        .find_map(|e| match e {
            Effect::ProbeClipboardAttachment { ctx, .. } => Some(ctx.clone()),
            _ => None,
        })
        .expect("Cmd+V of an image must defer a probe");
    crate::clipboard::clear_clipboard_probe_hook();
    assert_eq!(agent_ref(&app, id).paste_probe_in_flight, 1);

    // Enter before the probe completes: the send is stashed (no enqueue)
    let effects = dispatch(Action::SendPrompt("look at this".into()), &mut app);
    assert!(
        effects.is_empty(),
        "the send must be stashed while the probe is in flight"
    );
    assert!(agent_ref(&app, id).deferred_send.is_some());
    assert!(
        agent_ref(&app, id).session.pending_prompts.is_empty(),
        "nothing enqueued before the image attaches"
    );

    // The probe completes with the image: attach it AND re-issue the stashed send, which now carries the image content block
    let pasted = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    let effects = dispatch(
        Action::TaskComplete(TaskResult::ClipboardAttachmentProbed {
            ctx,
            image: crate::app::actions::ProbedAttachment::Image(pasted),
            file_urls: None,
        }),
        &mut app,
    );
    assert_eq!(agent_ref(&app, id).paste_probe_in_flight, 0);
    assert!(
        agent_ref(&app, id).deferred_send.is_none(),
        "the stashed send was consumed"
    );
    let sent_image = effects.iter().any(|e| {
        matches!(
            e,
            Effect::SendPromptBlocks { blocks, .. }
                if blocks.iter().any(|b| matches!(b, acp::ContentBlock::Image(_)))
        )
    });
    assert!(
        sent_image,
        "the re-issued send must carry the pasted image; effects = {effects:?}"
    );
}

/// Paste-then-immediate-interject race: Ctrl+Enter while the pasted image's probe is still off-thread must stash the interject (draft untouched).
/// It is re-issued on completion carrying the freshly attached image, mirroring the Enter/SendPrompt stash.
#[test]
fn interject_before_paste_probe_keeps_image() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning; // interject needs a live turn
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.prompt.set_text("look at this");
    }
    // Cmd+V an image: the probe defers
    crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook::with_raster(
        None,
    ));
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let _ = agent
            .handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    }
    let ctx = agent_ref(&app, id)
        .pending_effects
        .iter()
        .find_map(|e| match e {
            Effect::ProbeClipboardAttachment { ctx, .. } => Some(ctx.clone()),
            _ => None,
        })
        .expect("Cmd+V of an image must defer a probe");
    crate::clipboard::clear_clipboard_probe_hook();

    // Ctrl+Enter before the probe completes: stashed, draft untouched
    {
        let agent = app.agents.get_mut(&id).unwrap();
        let outcome =
            agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(
            matches!(outcome, crate::app::app_view::InputOutcome::Changed),
            "the interject must be stashed, not emitted, got {outcome:?}"
        );
    }
    assert_eq!(
        agent_ref(&app, id).deferred_send,
        Some(crate::app::agent_view::AgentDeferredSend::Interject)
    );
    assert_eq!(
        agent_ref(&app, id).prompt.text(),
        "look at this",
        "the stash must not consume the draft before the image attaches"
    );

    // The probe completes with the image: attach it AND re-issue the interject, which now carries the image content block
    let pasted = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    let effects = dispatch(
        Action::TaskComplete(TaskResult::ClipboardAttachmentProbed {
            ctx,
            image: crate::app::actions::ProbedAttachment::Image(pasted),
            file_urls: None,
        }),
        &mut app,
    );
    assert_eq!(agent_ref(&app, id).paste_probe_in_flight, 0);
    assert!(agent_ref(&app, id).deferred_send.is_none());
    let sent_image = effects.iter().any(|e| {
        matches!(
            e,
            Effect::SendPromptNow { blocks, .. }
                if blocks.iter().any(|b| matches!(b, acp::ContentBlock::Image(_)))
        )
    });
    assert!(
        sent_image,
        "the re-issued send-now must carry the pasted image; effects = {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, id).prompt.text(),
        "",
        "the reissue consumes the draft exactly like a direct interject"
    );
    assert!(agent_ref(&app, id).prompt.images.is_empty());
}

/// Guard: a stashed send must NOT be re-issued to the wrong session when the user switched agents during the probe window.
/// The image still attaches to the original target; the stale send is dropped, not sent to the new agent.
#[test]
fn agent_paste_completion_after_switch_does_not_send_to_other_agent() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = test_app_with_agent(); // agent A = AgentId(0), active view
    let a = AgentId(0);
    let b = AgentId(1);
    // A second agent B for the user to switch to mid-probe.
    let session_b = make_test_agent_session(&app, b, "session-b");
    app.agents
        .insert(b, AgentView::new(session_b, ScrollbackState::new()));

    // Cmd+V an image into A (active view A): the probe defers
    {
        let agent = app.agents.get_mut(&a).unwrap();
        agent.set_active_pane(ActivePane::Prompt, true);
        agent.prompt.set_text("for agent A");
    }
    crate::clipboard::set_clipboard_probe_hook(crate::clipboard::ClipboardProbeHook::with_raster(
        None,
    ));
    {
        let agent = app.agents.get_mut(&a).unwrap();
        let _ = agent
            .handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    }
    let ctx = agent_ref(&app, a)
        .pending_effects
        .iter()
        .find_map(|e| match e {
            Effect::ProbeClipboardAttachment { ctx, .. } => Some(ctx.clone()),
            _ => None,
        })
        .expect("Cmd+V of an image must defer a probe");
    crate::clipboard::clear_clipboard_probe_hook();
    // Model the event loop: `AppView::handle_input` drains the view's pending effects after the key event, before the completion arrives
    // The completion arm hands back anything still queued
    app.agents.get_mut(&a).unwrap().pending_effects.clear();

    // Enter (still on A): the send is stashed
    let effects = dispatch(Action::SendPrompt("for agent A".into()), &mut app);
    assert!(effects.is_empty());
    assert!(agent_ref(&app, a).deferred_send.is_some());

    // User switches to agent B during the probe window.
    app.active_view = ActiveView::Agent(b);
    let b_queue_before = agent_ref(&app, b).session.pending_prompts.len();

    // A's probe completes.
    let pasted = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    let effects = dispatch(
        Action::TaskComplete(TaskResult::ClipboardAttachmentProbed {
            ctx,
            image: crate::app::actions::ProbedAttachment::Image(pasted),
            file_urls: None,
        }),
        &mut app,
    );

    // (1) The image attaches to the ORIGINAL target A.
    assert_eq!(agent_ref(&app, a).prompt.images.len(), 1);
    assert!(agent_ref(&app, a).prompt.text().contains("[Image #1]"));
    let Some(image) = agent_ref(&app, a).prompt.images.first() else {
        panic!("expected the image attached to agent A");
    };
    let preview_identity = image.preview.identity();
    // (2) A's stash is cleared so it can't leak.
    assert!(agent_ref(&app, a).deferred_send.is_none());
    assert_eq!(agent_ref(&app, a).paste_probe_in_flight, 0);
    // (3) No send is re-issued to the now-active B
    // The only returned effect prepares the image that was attached to A
    match effects.as_slice() {
        [Effect::PreparePromptImagePreview { preparation }] => assert_eq!(
            preparation.preview().identity(),
            preview_identity,
            "preview preparation must belong to agent A's attached image",
        ),
        other => panic!("expected only agent A preview preparation, got {other:?}"),
    }
    assert_eq!(
        agent_ref(&app, b).session.pending_prompts.len(),
        b_queue_before,
        "agent B's queue must be untouched"
    );
}

/// A prompt submitted before the session binds is held in the agent's queue rather than dropped.
/// `maybe_drain_queue` emits nothing until `SessionCreated` arrives.
#[test]
fn prompt_before_the_session_binds_is_queued() {
    let mut app = test_app();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);
    assert!(
        agent_ref(&app, id).session.session_id.is_none(),
        "precondition: session not bound yet"
    );

    let effects = dispatch_send_prompt_inner(&mut app, "fix the bug".into(), true, false, false);

    assert_eq!(
        agent_ref(&app, id).session.queue_len(),
        1,
        "the prompt must be queued, not dropped"
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::SendPrompt { .. })),
        "nothing may go to the wire before the session binds, got {effects:?}"
    );
}

/// Returns true if any system block in agent 0's scrollback contains `needle`.
/// Avoids `last_system_text`'s "last block must be System" panic for the allowed-command control (which may leave no system block).
fn scrollback_has_system_text(app: &AppView, id: AgentId, needle: &str) -> bool {
    let sb = &agent_ref(app, id).scrollback;
    (0..sb.len()).any(
        |i| matches!(&sb.get(i).unwrap().block, RenderBlock::System(s) if s.text.contains(needle)),
    )
}

#[test]
fn minimal_mode_blocks_fullscreen_pane_slash_command() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let before = agent_scrollback_len(&app);
    // `/find` drives the deleted interactive scrollback pane.
    let effects = dispatch_send_prompt(&mut app, "/find foo".to_string());
    assert!(
        effects.is_empty(),
        "a gated command must not produce effects, got: {effects:?}"
    );
    assert_eq!(
        agent_scrollback_len(&app),
        before + 1,
        "the gate should commit exactly one system block"
    );
    let refusal = last_system_text(&app, AgentId(0));
    assert!(
        refusal.starts_with("/find isn't available in minimal mode"),
        "got: {refusal:?}"
    );
    assert!(
        refusal.contains("Run /fullscreen"),
        "the refusal must name the way out, got: {refusal:?}"
    );
}

#[test]
fn fullscreen_mode_blocks_minimal_only_slash_command() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Fullscreen;
    let effects = dispatch_send_prompt(&mut app, "/expand".to_string());
    assert!(effects.is_empty(), "got: {effects:?}");
    let refusal = last_system_text(&app, AgentId(0));
    assert_eq!(
        refusal,
        "/expand isn't available in fullscreen mode: press Tab to focus the scrollback, \
         then → on the block."
    );
}

#[test]
fn mode_switcher_in_its_own_mode_says_you_are_already_there() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    let effects = dispatch_send_prompt(&mut app, "/minimal".to_string());
    assert!(effects.is_empty(), "got: {effects:?}");
    assert_eq!(
        last_system_text(&app, AgentId(0)),
        "You're already in minimal mode."
    );
}

/// Inline (`--no-alt-screen`) is a full TUI, so fullscreen-only commands run there.
/// The gate keys off "is minimal", not "is `ScreenMode::Fullscreen`".
#[test]
fn non_minimal_mode_allows_fullscreen_pane_slash_command() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Inline;
    let _ = dispatch_send_prompt(&mut app, "/find foo".to_string());
    assert!(
        !scrollback_has_system_text(&app, AgentId(0), "isn't available"),
        "the gate must not fire outside minimal mode"
    );
}

#[test]
fn minimal_mode_allows_mode_agnostic_slash_command() {
    let mut app = test_app_with_agent();
    app.screen_mode = crate::app::ScreenMode::Minimal;
    // `/help` is a minimal-native command (opens the command palette).
    let _ = dispatch_send_prompt(&mut app, "/help".to_string());
    assert!(
        !scrollback_has_system_text(&app, AgentId(0), "isn't available"),
        "denylist default must keep mode-agnostic commands available"
    );
}

#[test]
fn show_queue_empty_commits_empty_message() {
    let mut app = test_app_with_agent();
    let before = agent_scrollback_len(&app);
    let effects = dispatch(Action::ShowQueue, &mut app);
    assert!(effects.is_empty(), "got: {effects:?}");
    assert_eq!(agent_scrollback_len(&app), before + 1);
    assert_eq!(last_system_text(&app, AgentId(0)), "Queue is empty.");
}

#[test]
fn show_queue_lists_local_prompts_in_order() {
    let mut app = test_app_with_agent();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.enqueue_prompt("first prompt".to_string());
        agent
            .session
            .enqueue_prompt("second\nwith two lines".to_string());
    }
    let effects = dispatch(Action::ShowQueue, &mut app);
    assert!(effects.is_empty(), "got: {effects:?}");
    let text = last_system_text(&app, AgentId(0));
    assert!(text.contains("Queued prompts (2):"), "got: {text:?}");
    assert!(text.contains("#1  first prompt"), "got: {text:?}");
    // Multi-line prompts collapse to the first line plus a count suffix
    assert!(text.contains("#2  second  (+1 more line)"), "got: {text:?}");
}

#[test]
fn show_queue_no_active_agent_is_noop() {
    let mut app = test_app();
    let effects = dispatch(Action::ShowQueue, &mut app);
    assert!(effects.is_empty(), "ShowQueue without an agent is a no-op");
}

/// Count of "Turn cancelled by user …" marker blocks in the agent's scrollback.
fn count_cancelled_markers(app: &AppView, id: AgentId) -> usize {
    let agent = agent_ref(app, id);
    (0..agent.scrollback.len())
        .filter(|i| {
            matches!(
                agent.scrollback.entry(*i).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::TurnCancelled { .. })
            )
        })
        .count()
}

/// Count of "Worked for …" marker blocks (parked or terminal).
fn count_completed_markers(app: &AppView, id: AgentId) -> usize {
    let agent = agent_ref(app, id);
    (0..agent.scrollback.len())
        .filter(|i| {
            matches!(
                agent.scrollback.entry(*i).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::TurnCompleted { .. })
            )
        })
        .count()
}

/// A cancelled `PromptResponse` for the running turn, optionally stamped `_meta.cancelTrigger`.
fn cancelled_prompt_response(id: AgentId, cancel_trigger: Option<&str>) -> Action {
    let mut meta = serde_json::Map::new();
    if let Some(t) = cancel_trigger {
        meta.insert("cancelTrigger".into(), serde_json::Value::String(t.into()));
    }
    let pr = if meta.is_empty() {
        acp::PromptResponse::new(acp::StopReason::Cancelled)
    } else {
        acp::PromptResponse::new(acp::StopReason::Cancelled).meta(Some(meta))
    };
    Action::TaskComplete(TaskResult::PromptResponse {
        agent_id: id,
        result: Ok(pr),
        http_status: None,
        prompt_id: None,
    })
}

/// A `_meta.cancelTrigger: "send_now"` turn end renders no cancel/completed marker.
#[test]
fn send_now_cancel_via_wire_meta_pushes_no_cancelled_marker() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    assert!(agent_ref(&app, id).session.state.is_turn_running());

    let _ = dispatch(cancelled_prompt_response(id, Some("send_now")), &mut app);

    assert!(agent_ref(&app, id).session.state.is_idle());
    assert_eq!(
        count_cancelled_markers(&app, id),
        0,
        "a send-now cancel must not render the cancelled marker"
    );
    assert_eq!(
        count_completed_markers(&app, id),
        0,
        "no substitute 'Worked for' for the cancelled turn either"
    );
}

/// Control: a plain cancel (no meta, no expectation) still renders its marker.
#[test]
fn plain_cancel_still_pushes_cancelled_marker() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    // The turn already produced output, so Ctrl+C takes the standard cancel path, not the rewind
    app.agents.get_mut(&id).unwrap().session.in_flight_prompt = None;
    let _ = dispatch(Action::CancelTurn, &mut app);
    assert!(agent_ref(&app, id).session.state.is_cancelling());

    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);

    assert!(agent_ref(&app, id).session.state.is_idle());
    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "a real user cancel keeps its marker"
    );
}

/// A non-"send_now" wire trigger wins over the client-side expectation (marker renders).
#[test]
fn non_send_now_wire_trigger_wins_over_client_expectation() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    app.agents.get_mut(&id).unwrap().expect_send_now_cancel = Some("p-send-now".into());

    let _ = dispatch(cancelled_prompt_response(id, Some("ctrl_c")), &mut app);

    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "an explicit non-send-now wire trigger must render the marker"
    );
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "the expectation is consumed at every driver turn end"
    );
}

/// Older-shell fallback: `SendPromptNow` arms the expectation; a meta-less cancel is suppressed.
#[test]
fn send_prompt_now_dispatch_arms_expectation_and_suppresses_marker() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    app.agents.get_mut(&id).unwrap().front_message_committed = true;

    let effects = dispatch(
        Action::SendPromptNow {
            text: "run this instead".into(),
            images: vec![],
        },
        &mut app,
    );
    assert!(matches!(effects.as_slice(), [Effect::SendPromptNow { .. }]));
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_some(),
        "send-now dispatch must arm the cancel expectation"
    );

    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);

    assert_eq!(
        count_cancelled_markers(&app, id),
        0,
        "the expected send-now cancel must not render the cancelled marker"
    );
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "the expectation is consumed by the turn end"
    );
}

/// Older-shell control: a plain `SendPrompt` during a held wait stays unarmed, so a meta-less cancel still renders its marker.
#[test]
fn plain_send_during_blocking_wait_does_not_arm_and_meta_less_cancel_is_visible() {
    use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "an idle-drain send must NOT arm the expectation"
    );
    simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

    let effects = dispatch(Action::SendPrompt("wake up and do this".into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [Effect::SendPrompt { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("mid-turn plain prompt takes the immediate server send, got {other:?}"),
    };
    let agent = agent_ref(&app, id);
    assert!(
        agent.expect_send_now_cancel.is_none(),
        "a plain send into a held wait must not arm send-now"
    );
    assert!(
        agent.follow_without_jump_prompt_id.is_none(),
        "a plain send must not pin follow-without-jump"
    );
    assert!(
        !agent.send_now_painted_blocks.contains_key(&prompt_id),
        "a plain send must not paint a speculative send-now block"
    );

    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);
    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "older-shell meta-less cancel after an unarmed plain send stays visible"
    );
}

/// Modern shell: wire `cancelTrigger=send_now` suppresses the marker without a pager arm.
#[test]
fn plain_send_during_blocking_wait_trusts_wire_send_now_trigger() {
    use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

    let effects = dispatch(Action::SendPrompt("wake up and do this".into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [Effect::SendPrompt { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("mid-turn plain prompt takes the immediate server send, got {other:?}"),
    };
    let agent = agent_ref(&app, id);
    assert!(agent.expect_send_now_cancel.is_none());
    assert!(agent.follow_without_jump_prompt_id.is_none());
    assert!(!agent.send_now_painted_blocks.contains_key(&prompt_id));

    let _ = dispatch(cancelled_prompt_response(id, Some("send_now")), &mut app);
    assert_eq!(count_cancelled_markers(&app, id), 0);
    assert_eq!(
        count_completed_markers(&app, id),
        0,
        "no substitute completed marker for a wire send-now cancel"
    );
}

fn count_user_prompts(app: &AppView, id: AgentId) -> usize {
    let agent = agent_ref(app, id);
    (0..agent.scrollback.len())
        .filter(|i| {
            matches!(
                agent.scrollback.entry(*i).map(|e| &e.block),
                Some(RenderBlock::UserPrompt(_))
            )
        })
        .count()
}

fn cancelled_removed_from_queue(id: AgentId, prompt_id: &str) -> Action {
    let mut meta = serde_json::Map::new();
    meta.insert("promptId".into(), prompt_id.into());
    meta.insert(
        crate::app::turn_completion::COMPLETION_KIND_KEY.into(),
        crate::app::turn_completion::REMOVED_FROM_QUEUE_KIND.into(),
    );
    Action::TaskComplete(TaskResult::PromptResponse {
        agent_id: id,
        result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled).meta(Some(meta))),
        http_status: None,
        prompt_id: Some(prompt_id.into()),
    })
}

#[test]
fn send_during_wake_queues_without_starting_a_turn() {
    use crate::app::agent_view::RunningWakeTurn;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.running_wake_turn = Some(RunningWakeTurn {
            prompt_id: "task-completed-wake".into(),
            cancel_sent: false,
        });
    }

    const TEXT: &str = "try again";
    let effects = dispatch(Action::SendPrompt(TEXT.into()), &mut app);
    match effects.as_slice() {
        [Effect::SendPrompt { text, .. }] => assert_eq!(text, TEXT),
        other => panic!("wake send must go to the server queue, got {other:?}"),
    }
    let agent = agent_ref(&app, id);
    assert_eq!(
        count_user_prompts(&app, id),
        0,
        "queued follow-up must not paint a scrollback bubble mid-wake"
    );
    assert!(
        agent.session.state.is_idle(),
        "must not start_turn over a non-adopted wake"
    );
    assert!(agent.session.current_prompt_id.is_none());
    assert!(agent.running_wake_turn.is_some());
}

#[test]
fn removed_from_queue_completion_kind_is_silent_without_wake_marker() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch(Action::SendPrompt("try again".into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [Effect::SendPrompt { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("idle drain starts a turn, got {other:?}"),
    };

    let _ = dispatch(cancelled_removed_from_queue(id, &prompt_id), &mut app);
    assert_eq!(count_cancelled_markers(&app, id), 0);
    assert_eq!(count_completed_markers(&app, id), 0);
}

#[test]
fn cancelled_response_during_newer_wake_still_shows_cancel_marker() {
    use crate::app::agent_view::RunningWakeTurn;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch(Action::SendPrompt("user turn".into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [Effect::SendPrompt { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("idle drain starts a turn, got {other:?}"),
    };
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.running_wake_turn = Some(RunningWakeTurn {
            prompt_id: "task-completed-newer".into(),
            cancel_sent: false,
        });
    }

    let mut meta = serde_json::Map::new();
    meta.insert("promptId".into(), prompt_id.clone().into());
    let _ = dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::Cancelled).meta(Some(meta))),
            http_status: None,
            prompt_id: Some(prompt_id),
        }),
        &mut app,
    );
    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "ambient wake state must not hide a real cancel"
    );
}

/// Pending foreground-subagent UI: a confirmed held row stays reachable and actionable.
#[test]
fn plain_send_during_pending_subagent_wait_keeps_confirmed_queue_row_reachable() {
    use crate::app::agent_view::{ActivePane, test_fixtures::simulate_subagent_wait};
    use crate::app::app_view::InputOutcome;
    use crate::app::prompt_queue::QueueEntryWire;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use distill_acp_lib::AcpClientMessage;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    let running_prompt_id = agent_ref(&app, id)
        .session
        .current_prompt_id
        .clone()
        .expect("idle drain starts a running turn");
    simulate_subagent_wait(app.agents.get_mut(&id).unwrap());

    const HELD: &str = "interrupt the subagent";
    let effects = dispatch(Action::SendPrompt(HELD.into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [Effect::SendPrompt { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("plain prompt takes the immediate server send, got {other:?}"),
    };
    {
        let agent = agent_ref(&app, id);
        assert!(agent.expect_send_now_cancel.is_none());
        assert!(agent.follow_without_jump_prompt_id.is_none());
        assert!(!agent.send_now_painted_blocks.contains_key(&prompt_id));
    }

    const AUTH_VERSION: u64 = 3;
    let params = serde_json::json!({
        "sessionId": "test-session",
        "entries": [{
            "id": prompt_id,
            "version": AUTH_VERSION,
            "kind": "prompt",
            "text": HELD,
            "position": 0,
        }],
        "runningPromptId": running_prompt_id,
    });
    let (response_tx, _rx) = tokio::sync::oneshot::channel();
    crate::app::acp_handler::handle(
        AcpClientMessage::ExtNotification(distill_acp_lib::AcpArgs {
            request: acp::ExtNotification::new(
                "x.ai/queue/changed",
                std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
            ),
            response_tx,
        }),
        &mut app,
    );

    {
        let agent = agent_ref(&app, id);
        assert_eq!(
            agent.shared_queue.as_slice(),
            [QueueEntryWire {
                id: prompt_id.clone(),
                version: AUTH_VERSION,
                owner: None,
                last_editor: None,
                kind: "prompt".into(),
                text: HELD.into(),
                position: 0,
                combined_texts: None,
            }]
        );
    }

    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.sync_queue_pane();
        assert!(!agent.visible_queue_is_empty());
        let ids = agent.queue.entry_ids();
        assert_eq!(ids.len(), 1, "confirmed held row must be the only pane row");
        let Some(&row_id) = ids.first() else {
            panic!("expected pane row id: {ids:?}");
        };
        let row = agent
            .queue
            .row_ref(row_id)
            .expect("synced pane row is resolvable");
        assert_eq!(row.server_id.as_deref(), Some(prompt_id.as_str()));
        assert_eq!(row.version, AUTH_VERSION);
    }

    let _ = dispatch(Action::ShowQueue, &mut app);
    let queue_text = last_system_text(&app, id);
    assert!(
        queue_text.contains(HELD),
        "/queue must list the confirmed prompt, got {queue_text:?}"
    );

    app.registry = crate::actions::ActionRegistry::non_vscode_for_test();
    app.agents.get_mut(&id).unwrap().hide_queue_pane();
    let outcome = app.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Char(';'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        matches!(outcome, InputOutcome::Changed),
        "Ctrl+; must toggle the queue overlay, got {outcome:?}"
    );
    {
        let agent = app.agents.get_mut(&id).unwrap();
        assert!(agent.queue.overlay.visible, "Ctrl+; must show the overlay");
        assert!(agent.queue.overlay.focused, "Ctrl+; must focus the overlay");
        assert_eq!(agent.active_pane, ActivePane::Queue);
        let ids = agent.queue.entry_ids();
        assert_eq!(ids.len(), 1);
        let Some(&row_id) = ids.first() else {
            panic!("expected overlay row id: {ids:?}");
        };
        let row = agent
            .queue
            .row_ref(row_id)
            .expect("overlay still has the row");
        assert_eq!(row.server_id.as_deref(), Some(prompt_id.as_str()));
        assert_eq!(row.version, AUTH_VERSION);
        agent.queue.list_state.select_by_id(row_id);
    }

    let outcome = app.handle_input(&Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )));
    assert!(
        matches!(
            outcome,
            InputOutcome::Action(Action::QueueInterjectShared {
                ref id,
                expected_version,
                new_text: None,
            }) if id == &prompt_id && expected_version == AUTH_VERSION
        ),
        "queue send-now must target the confirmed row, got {outcome:?}"
    );
}

/// A plain mid-turn prompt (no held wait) must not arm the expectation.
#[test]
fn plain_send_while_streaming_does_not_arm_expectation() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);

    let _ = dispatch(Action::SendPrompt("follow-up".into()), &mut app);
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "a queued follow-up outside a held wait must not arm the expectation"
    );

    let _ = dispatch(Action::CancelTurn, &mut app);
    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);
    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "the later real cancel keeps its marker"
    );
}

/// Queue-pane "Send now" of a server row arms the expectation (older-shell fallback).
#[test]
fn queue_interject_shared_arms_expectation_while_running() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    app.agents.get_mut(&id).unwrap().front_message_committed = true;

    let effects = dispatch(
        Action::QueueInterjectShared {
            id: "srv-row-1".into(),
            expected_version: 1,
            new_text: None,
        },
        &mut app,
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::QueueInterject { .. }]
    ));
    assert_eq!(
        agent_ref(&app, id).expect_send_now_cancel.as_deref(),
        Some("srv-row-1"),
        "server-row send-now must arm the cancel expectation"
    );
    assert!(agent_ref(&app, id).is_self_originated_prompt("srv-row-1"));
}

/// During an active goal the shell promotes a send-now WITHOUT cancelling.
/// Neither `SendPromptNow` nor a server-row send-now may arm the expectation; a stale arm would mute a later real cancel's marker.
#[test]
fn send_now_during_active_goal_does_not_arm_expectation() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.front_message_committed = true;
        agent.goal_state = Some(crate::app::agent::GoalDisplayState::test_stub());
    }

    let effects = dispatch(
        Action::SendPromptNow {
            text: "goal steer".into(),
            images: vec![],
        },
        &mut app,
    );
    assert!(matches!(effects.as_slice(), [Effect::SendPromptNow { .. }]));
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "goal turns promote without cancelling; the expectation must stay unarmed"
    );
    assert_eq!(agent_ref(&app, id).send_now_painted_blocks.len(), 1);

    let effects = dispatch(
        Action::QueueInterjectShared {
            id: "srv-row-goal".into(),
            expected_version: 1,
            new_text: None,
        },
        &mut app,
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::QueueInterject { .. }]
    ));
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "server-row send-now during a goal must stay unarmed too"
    );
}

/// The prompt's RPC resolves as removed-without-running (the expected outcome of routing the Send Now as an interjection).
/// That takes the non-running `PromptResponse` path, but it must NOT retire the painted block before its interjection claim arrives.
/// Otherwise the message is dropped and re-pushed at the scrollback end (flicker / reorder).
#[test]
fn goal_send_now_painted_block_survives_removed_from_queue_response() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.front_message_committed = true;
        agent.goal_state = Some(crate::app::agent::GoalDisplayState::test_stub());
    }

    let effects = dispatch(
        Action::SendPromptNow {
            text: "goal steer".into(),
            images: vec![],
        },
        &mut app,
    );
    let painted_pid = match effects.as_slice() {
        [Effect::SendPromptNow { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("goal send-now takes the immediate server send, got {other:?}"),
    };
    let block_entry = {
        let agent = agent_ref(&app, id);
        assert!(agent.is_self_originated_prompt(&painted_pid));
        assert!(
            agent.is_send_now_awaiting_interjection_claim(&painted_pid),
            "the goal Send Now block must be awaiting its interjection claim"
        );
        let Some(&(block_entry, _)) = agent.send_now_painted_blocks.get(&painted_pid) else {
            panic!("expected painted send-now block {painted_pid}");
        };
        block_entry
    };

    // The queued prompt's RPC resolves without becoming the running turn.
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Ok(acp::PromptResponse::new(acp::StopReason::EndTurn).meta(
                serde_json::json!({ "promptId": painted_pid })
                    .as_object()
                    .cloned(),
            )),
            http_status: None,
            prompt_id: None,
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(
        agent.send_now_painted_blocks.contains_key(&painted_pid),
        "the painted block must survive the removed-from-queue response",
    );
    assert!(
        agent.scrollback.index_of_id(block_entry).is_some(),
        "the block must stay in place (not dropped / re-pushed at the end)",
    );
}

/// A `queue/changed` broadcast that no longer lists a CONFIRMED active-goal Send Now row must NOT retire its painted block.
/// The shell converted it into an interjection and dropped the queue row; the block waits for the interjection notification's claim.
#[test]
fn goal_send_now_painted_block_survives_queue_changed_removal() {
    use distill_acp_lib::AcpClientMessage;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    let running_prompt_id = agent_ref(&app, id).session.current_prompt_id.clone();
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.front_message_committed = true;
        agent.goal_state = Some(crate::app::agent::GoalDisplayState::test_stub());
    }

    let effects = dispatch(
        Action::SendPromptNow {
            text: "goal steer".into(),
            images: vec![],
        },
        &mut app,
    );
    let painted_pid = match effects.as_slice() {
        [Effect::SendPromptNow { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("goal send-now takes the immediate server send, got {other:?}"),
    };
    let Some(&(block_entry, _)) = agent_ref(&app, id)
        .send_now_painted_blocks
        .get(&painted_pid)
    else {
        panic!("expected painted send-now block {painted_pid}");
    };
    // Model the row as CONFIRMED shell-side (optimistic echo already cleared)
    // The race: a confirmed row disappears from a later broadcast when the Send Now is merged as an interjection
    app.agents
        .get_mut(&id)
        .unwrap()
        .optimistic_queue_ids
        .remove(&painted_pid);
    assert!(agent_ref(&app, id).is_send_now_awaiting_interjection_claim(&painted_pid));

    // A broadcast that no longer lists the row, still naming the running goal turn (so no adoption side-effects fire)
    let params = serde_json::json!({
        "sessionId": "test-session",
        "entries": [],
        "runningPromptId": running_prompt_id,
    });
    let (response_tx, _rx) = tokio::sync::oneshot::channel();
    crate::app::acp_handler::handle(
        AcpClientMessage::ExtNotification(distill_acp_lib::AcpArgs {
            request: acp::ExtNotification::new(
                "x.ai/queue/changed",
                std::sync::Arc::from(serde_json::value::to_raw_value(&params).unwrap()),
            ),
            response_tx,
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert!(
        agent.send_now_painted_blocks.contains_key(&painted_pid),
        "the confirmed goal Send Now block must survive the row's removal",
    );
    assert!(
        agent.scrollback.index_of_id(block_entry).is_some(),
        "the block must stay in place for handle_interjection to claim",
    );
}

/// Send-now during a reconnect outage must not fire into the dead channel; the payload is requeued locally (the producer already consumed it).
#[test]
fn send_prompt_now_during_reconnect_requeues_locally() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    app.reconnect_pending = true;

    let effects = dispatch(
        Action::SendPromptNow {
            text: "typed mid-outage".into(),
            images: vec![],
        },
        &mut app,
    );
    assert!(
        effects.is_empty(),
        "no effect may fire while reconnecting, got {effects:?}"
    );
    let agent = agent_ref(&app, id);
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("typed mid-outage"),
        "the consumed payload must be requeued at the front"
    );
    assert!(
        agent.expect_send_now_cancel.is_none(),
        "no expectation may be armed for a send that never left"
    );
    assert!(agent.toast.is_some(), "the outage must explain itself");
}

/// A failed send-now RPC requeues its payload (front) instead of silently dropping the message, and retires the optimistic queue echo.
#[test]
fn failed_send_now_requeues_payload_and_retires_echo() {
    use agent_client_protocol as acp;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);

    let effects = dispatch(
        Action::SendPromptNow {
            text: "gets lost on the wire".into(),
            images: vec![],
        },
        &mut app,
    );
    let prompt_id = match effects.as_slice() {
        [Effect::SendPromptNow { prompt_id, .. }] => prompt_id.clone(),
        other => panic!("expected SendPromptNow effect, got {other:?}"),
    };
    assert!(
        agent_ref(&app, id)
            .shared_queue
            .iter()
            .any(|e| e.id == prompt_id),
        "the optimistic echo must be visible before the failure"
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::SendPromptNowFailed {
            agent_id: id,
            session_id: acp::SessionId::new("test-session"),
            prompt_id: prompt_id.clone(),
            error: "transport closed".into(),
            blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                "gets lost on the wire".to_string(),
            ))],
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("gets lost on the wire"),
        "the failed payload must be requeued at the front"
    );
    assert!(
        !agent.shared_queue.iter().any(|e| e.id == prompt_id),
        "the optimistic echo must be retired"
    );
    assert!(agent.toast.is_some(), "the failure must explain itself");
}

/// An image-bearing prompt submitted during a parked sendable wait routes through send-now (images ride as content blocks).
/// It must not silently hold in the local queue behind the wait.
#[test]
fn image_prompt_during_sendable_wait_routes_to_send_now() {
    use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");

    let img = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    {
        let agent = app.agents.get_mut(&id).unwrap();
        // A real chip insert: `drain_images` reconciles against live textarea elements, so a bare `images.push` would be dropped as stale
        agent.prompt.insert_image(img).unwrap();
    }

    let effects = dispatch(Action::SendPrompt("look at [Image #1]".into()), &mut app);
    match effects.as_slice() {
        [Effect::SendPromptNow { blocks, .. }] => {
            assert!(
                blocks
                    .iter()
                    .any(|b| matches!(b, acp::ContentBlock::Image(_))),
                "the pasted image must ride the send-now blocks"
            );
        }
        other => {
            panic!("expected SendPromptNow for an image prompt in a sendable wait, got {other:?}")
        }
    }
    let agent = agent_ref(&app, id);
    assert!(
        agent.session.pending_prompts.is_empty(),
        "the message must not ALSO be held in the local queue"
    );
    assert!(
        agent.prompt.images.is_empty(),
        "the composer images must be consumed by the send"
    );
}

/// The local drip-feed drain must hold while a non-running server row exists.
/// The shell owns the next turn (its `running_prompt_id` broadcast starts it).
/// Draining locally would promote a bogus local turn that swallows the real turn's deltas.
#[test]
fn local_drain_holds_while_server_row_queued() {
    use crate::app::dispatch::queue::maybe_drain_queue;
    use crate::app::dispatch::tests::enqueue_local;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    enqueue_local(&mut app, id, "local follow-up");
    app.agents.get_mut(&id).unwrap().shared_queue =
        vec![crate::app::prompt_queue::QueueEntryWire {
            id: "srv-1".into(),
            version: 0,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "server-owned next".into(),
            position: 0,
            combined_texts: None,
        }];

    let agent = app.agents.get_mut(&id).unwrap();
    assert!(agent.session.state.is_idle());
    let effects = maybe_drain_queue(agent).effects;
    assert!(
        effects.is_empty(),
        "local drain must hold while the server owns the next turn, got {effects:?}"
    );
    assert_eq!(
        agent.session.pending_prompts.len(),
        1,
        "the local row must stay queued"
    );

    // The running server row does NOT hold the drain (it is the in-flight turn, not a queued one)
    // Once it's marked running and the turn ends, the local row drains normally
    agent.session.current_prompt_id = Some("srv-1".into());
    let effects = maybe_drain_queue(agent).effects;
    assert!(
        matches!(effects.as_slice(), [Effect::SendPrompt { .. }]),
        "a running-only shared queue must not hold the local drain, got {effects:?}"
    );
}

/// A real turn start clears a stale expectation (so a later Ctrl+C keeps its marker).
#[test]
fn stale_expectation_cleared_on_next_turn_start() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().expect_send_now_cancel = Some("p-stale".into());

    dispatch(Action::SendPrompt("fresh turn".into()), &mut app);
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "turn start must clear a stale expectation"
    );

    // Real Ctrl+C on the fresh turn keeps its marker (stash dropped, so the standard path runs)
    app.agents.get_mut(&id).unwrap().session.in_flight_prompt = None;
    let _ = dispatch(Action::CancelTurn, &mut app);
    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);
    assert_eq!(count_cancelled_markers(&app, id), 1);
}

/// An explicit user cancel supersedes a pending send-now expectation (marker renders).
#[test]
fn interactive_cancel_supersedes_send_now_expectation() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.expect_send_now_cancel = Some("p-send-now".into());
        agent.session.in_flight_prompt = None;
    }

    let _ = dispatch(Action::CancelTurn, &mut app);
    assert!(
        agent_ref(&app, id).expect_send_now_cancel.is_none(),
        "an interactive cancel must clear the send-now expectation"
    );

    let _ = dispatch(cancelled_prompt_response(id, None), &mut app);
    assert_eq!(
        count_cancelled_markers(&app, id),
        1,
        "the interactive cancel keeps its marker"
    );
}

/// A modern send-now cancel out of a park leaves no markers: the park is markerless and wire `cancelTrigger=send_now` suppresses the cancel marker.
#[test]
fn send_now_cancel_after_park_leaves_no_markers() {
    use crate::app::agent_view::test_fixtures::{count_turn_markers, simulate_task_output_wait};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    dispatch(Action::SendPrompt("first".into()), &mut app);
    simulate_task_output_wait(app.agents.get_mut(&id).unwrap(), "bg-1");
    assert_eq!(
        count_turn_markers(agent_ref(&app, id)),
        0,
        "a park writes no marker"
    );

    let _ = dispatch(Action::SendPrompt("next thing".into()), &mut app);
    let _ = dispatch(cancelled_prompt_response(id, Some("send_now")), &mut app);

    assert_eq!(count_cancelled_markers(&app, id), 0);
    assert_eq!(
        count_completed_markers(&app, id),
        0,
        "no completed marker renders for the cancelled parked turn"
    );
}

/// Shell-suggestion async gate 1: a debounce expiring after the user left bash mode fetches nothing.
/// A stale timer must not fire a suggest request for chat text.
/// Positive control: still in bash mode, the fetch fires.
#[test]
fn suggestion_debounce_after_bash_exit_fetches_nothing() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = true;
        agent.prompt.textarea.insert_str("git st");
        agent
            .prompt
            .suggestions
            .text_changed("git st", false, false);
        agent.prompt.suggestions.generation()
    };

    // Positive control: in bash mode, the expired debounce fetches.
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SuggestionDebounceExpired {
            agent_id: id,
            generation,
        }),
        &mut app,
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchShellSuggestions { .. })),
        "in-mode debounce must fetch: {effects:?}"
    );

    // Mode exited between debounce arm and expiry: no fetch.
    app.agents.get_mut(&id).unwrap().prompt_input_mode =
        crate::app::agent_view::PromptInputMode::Normal;
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SuggestionDebounceExpired {
            agent_id: id,
            generation,
        }),
        &mut app,
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchShellSuggestions { .. })),
        "post-exit debounce must fetch nothing: {effects:?}"
    );
}

/// Shell-suggestion async gate 2: a response landing after the user left bash mode is dropped wholesale.
/// No ghost, no dropdown items over the normal-mode chat draft.
#[test]
fn suggestions_landing_after_bash_exit_are_dropped() {
    use crate::views::suggestion_controller::{
        CompletionItemParsed, GhostSuggestionParsed, SuggestResponseParsed,
        SuggestionSource as ShellSuggestionSource,
    };

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = true;
        agent.prompt.textarea.insert_str("git st");
        agent
            .prompt
            .suggestions
            .text_changed("git st", false, false);
        // Leave bash mode with the fetch in flight.
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Normal;
        agent.prompt.suggestions.generation()
    };

    let response = SuggestResponseParsed {
        ghost: Some(GhostSuggestionParsed {
            suffix: "atus --porcelain".into(),
            source: ShellSuggestionSource::History,
        }),
        completions: vec![CompletionItemParsed {
            display: "git status --porcelain".into(),
            description: String::new(),
            insert_text: "git status --porcelain".into(),
            source: ShellSuggestionSource::History,
            priority: 10,
            replace_range: Some(0..6),
            token_text: None,
            truncated: false,
        }],
        generation,
    };
    let effects = dispatch(
        Action::TaskComplete(TaskResult::ShellSuggestionsLoaded {
            agent_id: id,
            response,
            request_text: "git st".into(),
            request_cursor: "git st".len(),
        }),
        &mut app,
    );
    assert!(effects.is_empty(), "landing is a no-op: {effects:?}");
    let agent = agent_ref(&app, id);
    assert!(
        !agent.prompt.has_ghost_text(),
        "no ghost may appear over the normal-mode draft"
    );
    assert!(agent.prompt.suggestions.dropdown.items.is_empty());
}

/// The always-on pipeline end to end, with the `GROK_SUGGESTIONS` env flag OFF.
/// Tab fires a deterministic fetch, and the response that lands behaves like Tab in a terminal.
/// A single file candidate splices in place immediately and the drill-down refetch rides out with the dispatch.
#[test]
fn tab_fetch_landing_insta_accepts_single_candidate_always_on() {
    use crate::views::suggestion_controller::{
        CompletionItemParsed, SuggestResponseParsed, SuggestionSource as ShellSuggestionSource,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = false;
        agent.prompt.textarea.insert_str("cat no");

        let _ = agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let generation = agent
            .pending_effects
            .iter()
            .find_map(|e| match e {
                Effect::FetchShellSuggestions {
                    generation,
                    include_ai: false,
                    ..
                } => Some(*generation),
                _ => None,
            })
            .expect("Tab must fire a deterministic fetch");
        // The event loop would drain these into execution; clear for the landing assertion below
        agent.pending_effects.clear();
        generation
    };

    let response = SuggestResponseParsed {
        ghost: None,
        completions: vec![CompletionItemParsed {
            display: "notes.md".into(),
            description: String::new(),
            insert_text: "cat notes.md".into(),
            source: ShellSuggestionSource::FilePath,
            priority: 2,
            replace_range: Some(4..6),
            token_text: Some("notes.md".into()),
            truncated: false,
        }],
        generation,
    };
    let effects = dispatch(
        Action::TaskComplete(TaskResult::ShellSuggestionsLoaded {
            agent_id: id,
            response,
            request_text: "cat no".into(),
            request_cursor: "cat no".len(),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert_eq!(agent.prompt.text(), "cat notes.md", "insta-accepted");
    assert!(!agent.prompt.completion_dropdown_open());
    assert!(
        !agent.prompt.has_ghost_text(),
        "no ghost without the env flag"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::FetchShellSuggestions {
                include_ai: false,
                ..
            }
        )),
        "the accept's drill-down refetch must ride out with the dispatch: {effects:?}"
    );
}

/// Same pipeline with an ambiguous candidate set: the response that lands opens the dropdown (and installs no ghost); the user picks with arrows and Tab.
/// History rows model an OLD shell (new shells honor `tokenOnly` and send none on Tab fetches); a whole-line set must still just open the dropdown.
#[test]
fn tab_fetch_landing_opens_dropdown_for_ambiguous_set_always_on() {
    use crate::views::suggestion_controller::{
        CompletionItemParsed, GhostSuggestionParsed, SuggestResponseParsed,
        SuggestionSource as ShellSuggestionSource,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = false;
        agent.prompt.textarea.insert_str("git st");
        let _ = agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        agent.pending_effects.clear();
        agent.prompt.suggestions.generation()
    };

    let history_item = |line: &str| CompletionItemParsed {
        display: line.to_owned(),
        description: String::new(),
        insert_text: line.to_owned(),
        source: ShellSuggestionSource::History,
        priority: 10,
        replace_range: Some(0..6),
        token_text: None,
        truncated: false,
    };
    let response = SuggestResponseParsed {
        // The shell still sends a ghost for history matches; without the env flag it must not render
        ghost: Some(GhostSuggestionParsed {
            suffix: "atus --porcelain-A".into(),
            source: ShellSuggestionSource::History,
        }),
        completions: vec![
            history_item("git status --porcelain-A"),
            history_item("git status --porcelain-B"),
        ],
        generation,
    };
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ShellSuggestionsLoaded {
            agent_id: id,
            response,
            request_text: "git st".into(),
            request_cursor: "git st".len(),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert_eq!(
        agent.prompt.text(),
        "git st",
        "no accept for two candidates"
    );
    assert!(
        agent.prompt.completion_dropdown_open(),
        "the armed Tab opens the dropdown when its candidates land"
    );
    assert_eq!(agent.prompt.suggestions.dropdown.items.len(), 2);
    assert!(
        !agent.prompt.has_ghost_text(),
        "ghost rendering stays env-gated"
    );
}

/// A response lands on the agent that fetched it, not on the active view.
/// A view switch mid-flight must still deliver the candidates to the agent that armed the Tab (and never to whatever is on screen).
#[test]
fn suggestions_landing_routes_by_agent_id_not_active_view() {
    use crate::views::suggestion_controller::{
        CompletionItemParsed, SuggestResponseParsed, SuggestionSource as ShellSuggestionSource,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = false;
        agent.prompt.textarea.insert_str("git st");
        let _ = agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        agent.pending_effects.clear();
        agent.prompt.suggestions.generation()
    };
    // The user switched away while the fetch was in flight.
    app.active_view = ActiveView::Welcome;

    let response = SuggestResponseParsed {
        ghost: None,
        completions: vec![CompletionItemParsed {
            display: "git status".into(),
            description: String::new(),
            insert_text: "git status".into(),
            source: ShellSuggestionSource::History,
            priority: 10,
            replace_range: Some(0..6),
            token_text: None,
            truncated: false,
        }],
        generation,
    };
    let _ = dispatch(
        Action::TaskComplete(TaskResult::ShellSuggestionsLoaded {
            agent_id: id,
            response,
            request_text: "git st".into(),
            request_cursor: "git st".len(),
        }),
        &mut app,
    );

    let agent = agent_ref(&app, id);
    assert_eq!(
        agent.prompt.suggestions.dropdown.items.len(),
        1,
        "candidates must land on the fetching agent even off-screen"
    );
    assert_eq!(agent.prompt.suggestions.dropdown.request_text, "git st");
}

/// The debounce expiry also routes by the arming agent (the timer carries `agent_id`, just as `ShellSuggestionsLoaded` does).
/// An expiry firing after a view switch still fetches for the arming agent instead of no-oping, or worse, reading another agent's state.
#[test]
fn suggestion_debounce_routes_by_agent_id_not_active_view() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let generation = {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
        agent.prompt.suggestions.enabled = true;
        agent.prompt.textarea.insert_str("git st");
        agent
            .prompt
            .suggestions
            .text_changed("git st", false, false);
        agent.prompt.suggestions.generation()
    };
    // The user switched away while the timer was running.
    app.active_view = ActiveView::Welcome;

    let effects = dispatch(
        Action::TaskComplete(TaskResult::SuggestionDebounceExpired {
            agent_id: id,
            generation,
        }),
        &mut app,
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::FetchShellSuggestions {
                agent_id: AgentId(0),
                ..
            }
        )),
        "expiry must fetch for the arming agent even off-screen: {effects:?}"
    );
}

/// Casual commenting parks its draft and keeps the composer live, the opposite of a permission.
/// Closing a card over it therefore restores into the composer and leaves the parked draft alone.
#[test]
fn casual_commenting_keeps_its_parked_draft_when_a_card_closes() {
    use crate::views::question_view::QuestionViewState;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use distill_tools::implementations::distill::ask_user_question::Question;

    let id = AgentId(0);
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&id).unwrap();
    agent.active_pane = crate::app::agent_view::AgentPane::Prompt;
    agent.prompt.set_text("pre-comment draft");
    agent.casual_stashed_prompt = Some(agent.prompt.stash());
    agent.prompt.set_text("the casual comment");

    // A card open stashes the live comment and blanks the composer.
    let stashed = agent.prompt.stash();
    agent.question_view = Some(QuestionViewState::new(
        "card-over-comment".into(),
        vec![Question {
            question: "busy?".into(),
            options: vec![],
            multi_select: Some(false),
            id: None,
        }],
        stashed,
    ));
    agent.prompt.set_text("");
    agent.handle_question_key_for_test(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));

    assert_eq!(
        agent.prompt.text(),
        "the casual comment",
        "the live comment comes back to the composer"
    );
    assert_eq!(
        agent
            .casual_stashed_prompt
            .as_ref()
            .map(|s| s.text.as_str()),
        Some("pre-comment draft"),
        "the parked pre-comment draft must survive"
    );
}

mod prompt_history_recording_tests {
    use super::{test_agent, test_app_with_agent};
    use crate::app::actions::Action;
    use crate::app::agent::AgentId;
    use crate::app::dispatch::router::dispatch;

    fn history(app: &crate::app::app_view::AppView) -> Vec<String> {
        test_agent(app, AgentId(0)).session.prompt_history.clone()
    }

    #[test]
    fn a_typed_slash_command_lands_in_the_history() {
        let mut app = test_app_with_agent();

        dispatch(
            Action::SendPrompt("/rename payment retries".into()),
            &mut app,
        );

        assert_eq!(history(&app), ["/rename payment retries"]);
    }

    /// The shell returns prompts it was sent, including ones the local list already holds.
    #[test]
    fn the_fetch_does_not_reorder_what_the_shell_also_knows() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        dispatch(Action::SendPrompt("hello".into()), &mut app);
        dispatch(Action::SendBashCommand("ls".into()), &mut app);
        dispatch(Action::SendPrompt("/asim-skill".into()), &mut app);
        dispatch(Action::SendPrompt("/session-info".into()), &mut app);

        dispatch(
            Action::TaskComplete(crate::app::actions::TaskResult::PromptHistoryLoaded {
                agent_id: id,
                prompts: vec!["/asim-skill".to_owned(), "hello".to_owned()],
            }),
            &mut app,
        );

        assert_eq!(
            history(&app),
            ["/session-info", "/asim-skill", "! ls", "hello"]
        );
    }

    #[test]
    fn an_immediate_repeat_collapses_but_an_interleaved_one_does_not() {
        let mut app = test_app_with_agent();

        dispatch(Action::SendPrompt("build it".into()), &mut app);
        dispatch(Action::SendPrompt("build it".into()), &mut app);
        dispatch(Action::SendPrompt("check it".into()), &mut app);
        dispatch(Action::SendPrompt("build it".into()), &mut app);

        assert_eq!(history(&app), ["build it", "check it", "build it"]);
    }

    /// `/notacommand` reaches the command recorder and then the prompt recorder.
    #[test]
    fn an_unknown_command_is_recorded_once() {
        let mut app = test_app_with_agent();

        dispatch(Action::SendPrompt("/notacommand".into()), &mut app);

        assert_eq!(history(&app), ["/notacommand"]);
    }

    /// Bash, slash and note entries never reach the shell. The local list is their only copy.
    #[test]
    fn a_late_history_fetch_keeps_what_only_the_client_knows() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        dispatch(Action::SendBashCommand("git status".into()), &mut app);
        dispatch(
            Action::SendPrompt("/rename payment retries".into()),
            &mut app,
        );

        dispatch(
            Action::TaskComplete(crate::app::actions::TaskResult::PromptHistoryLoaded {
                agent_id: id,
                prompts: vec!["git status".to_owned(), "fetched from the shell".to_owned()],
            }),
            &mut app,
        );

        let after = history(&app);
        assert!(
            after.contains(&"! git status".to_owned()),
            "the bash command survived the fetch: {after:?}"
        );
        assert!(
            !after.contains(&"git status".to_owned()),
            "the shell stores bash bare, so its copy must not double the prefixed one: {after:?}"
        );
        assert!(
            after.contains(&"/rename payment retries".to_owned()),
            "the slash command survived the fetch: {after:?}"
        );
        assert!(
            after.contains(&"fetched from the shell".to_owned()),
            "the fetched history is still merged in: {after:?}"
        );
    }

    /// Storing the `#` would recall a prompt opening `# Context` as a note.
    #[test]
    fn a_remember_note_is_recorded_as_plain_text() {
        let mut app = test_app_with_agent();

        dispatch(
            Action::SendRememberNote("deploys need the staging flag".into()),
            &mut app,
        );

        assert_eq!(history(&app), ["deploys need the staging flag"]);
    }

    #[test]
    fn a_refused_remember_note_records_nothing() {
        let mut app = test_app_with_agent();

        dispatch(Action::SendRememberNote("   ".into()), &mut app);

        assert!(history(&app).is_empty());
    }

    /// A stashed draft is not history. Only the send that follows puts a row in the recall list.
    #[test]
    fn stashing_a_draft_adds_no_history_row() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("first draft");
            agent.stash_prompt_draft(crate::app::agent_view::StashCause::ClearedDraft);
            agent.prompt.set_text("second draft");
            agent.stash_prompt_draft(crate::app::agent_view::StashCause::ClearedDraft);
        }
        assert!(history(&app).is_empty());

        dispatch(Action::SendPrompt("a real send".into()), &mut app);

        assert_eq!(history(&app), ["a real send"]);
    }

    /// `/remember foo` runs the command recorder and then `SendRememberNote(foo)`.
    #[test]
    fn a_slash_remember_records_only_the_typed_command() {
        let mut app = test_app_with_agent();

        dispatch(
            Action::SendPrompt("/remember deploys need the staging flag".into()),
            &mut app,
        );

        assert_eq!(history(&app), ["/remember deploys need the staging flag"]);
    }
}

mod prompt_stash_dispatch_tests {
    use super::test_app_with_agent;
    use crate::app::actions::Action;
    use crate::app::agent::AgentId;
    use crate::app::agent_view::StashCause;
    use crate::app::dispatch::router::dispatch;

    /// Esc-Esc clear hands the draft to the stash rather than dropping it. Ranking and restore behavior lives in `agent_view::prompt_stash`.
    #[test]
    fn clear_prompt_stashes_the_cleared_draft() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .set_text("cleared draft");

        let effects = dispatch(Action::ClearPrompt, &mut app);

        assert!(effects.is_empty());
        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.prompt.text(), "", "composer must be cleared");
        let stash = agent.prompt_stash.as_ref().expect("draft was stashed");
        assert_eq!(stash.prompt.text, "cleared draft");
        assert_eq!(stash.cause, StashCause::ClearedDraft);
    }

    /// A cleared `!` draft lands in the slot with its shell mode and nowhere else: it was never sent.
    #[test]
    fn clearing_a_shell_draft_stashes_it_and_records_no_history() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;
            agent.prompt.set_text("git status");
        }

        dispatch(Action::ClearPrompt, &mut app);

        let agent = app.agents.get(&id).unwrap();
        assert!(agent.combined_prompt_history().is_empty());
        let stash = agent.prompt_stash.as_ref().expect("draft was stashed");
        assert_eq!(stash.prompt.text, "git status");
        assert_eq!(
            stash.input_mode,
            crate::app::agent_view::PromptInputMode::Bash
        );
    }

    #[test]
    fn clear_prompt_on_empty_composer_stashes_nothing() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);

        dispatch(Action::ClearPrompt, &mut app);

        assert!(app.agents.get(&id).unwrap().prompt_stash.is_none());
    }

    #[test]
    fn chord_stashed_draft_auto_restores_after_next_send() {
        // Both sends consume the composer, so both hand the draft back.
        let sends = [
            Action::SendPrompt("quick side question".into()),
            Action::SendBashCommand("git status".into()),
        ];

        for send in sends {
            let label = format!("{send:?}");
            let mut app = test_app_with_agent();
            let id = AgentId(0);
            {
                let agent = app.agents.get_mut(&id).unwrap();
                agent.prompt.set_text("stashed thought");
                agent.stash_prompt_draft(StashCause::Chord);
            }

            dispatch(send, &mut app);

            let agent = app.agents.get(&id).unwrap();
            assert_eq!(
                agent.prompt.text(),
                "stashed thought",
                "{label} must restore the stash"
            );
            assert!(agent.prompt_stash.is_none(), "{label} left the slot full");
        }
    }

    /// `SendPromptNow` also force-sends a queued row, which never touched the composer.
    /// Only the caller that consumed the draft reports it, so the queued-row case must leave the stash alone.
    #[test]
    fn a_queued_row_send_now_leaves_the_stash_alone() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("stashed thought");
            agent.stash_prompt_draft(StashCause::Chord);
        }

        dispatch(
            Action::SendPromptNow {
                text: "a queued row".into(),
                images: vec![],
            },
            &mut app,
        );

        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.prompt.text(), "", "the composer was never the source");
        assert!(agent.prompt_stash.is_some(), "the stash must stay put");
    }

    /// A `#` note with no text is refused, so it never consumed a draft and must not hand the stash back.
    /// The marker has to sit after the guard, not before it.
    #[test]
    fn a_rejected_remember_note_does_not_restore_the_stash() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("stashed thought");
            agent.stash_prompt_draft(StashCause::Chord);
        }

        dispatch(Action::SendRememberNote("   ".into()), &mut app);

        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.prompt.text(), "", "the refused note restores nothing");
        assert!(agent.prompt_stash.is_some(), "the stash must stay put");
    }

    /// A handler that rejects the action never consumed the draft, so the stash must stay put.
    #[test]
    fn a_rejected_send_does_not_restore_the_stash() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.prompt.set_text("stashed thought");
            agent.stash_prompt_draft(StashCause::Chord);
            agent.prompt.set_text("never sent");
        }
        app.reconnect_pending = true;

        dispatch(Action::SendPrompt("never sent".into()), &mut app);

        let agent = app.agents.get(&id).unwrap();
        assert_eq!(
            agent.prompt.text(),
            "never sent",
            "the draft is still unsent"
        );
        assert!(
            agent.prompt_stash.is_some(),
            "a refused send must not hand the stash back"
        );
    }

    /// An Esc-Esc-cleared draft is a discard: it must NOT bounce back after the next send. Chord pop and history browse remain its recovery paths.
    #[test]
    fn esc_cleared_draft_does_not_auto_restore_after_send() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        app.agents
            .get_mut(&id)
            .unwrap()
            .prompt
            .set_text("discarded draft");

        dispatch(Action::ClearPrompt, &mut app);
        dispatch(Action::SendPrompt("next prompt".into()), &mut app);

        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.prompt.text(), "", "discarded draft must stay stashed");
        assert!(agent.prompt_stash.is_some());
    }

    /// Auto-restore never overwrites composer state the user already touched.
    #[test]
    fn auto_restore_declines_on_a_non_empty_or_moded_composer() {
        let mut app = test_app_with_agent();
        let id = AgentId(0);
        let agent = app.agents.get_mut(&id).unwrap();
        agent.prompt.set_text("stashed thought");
        agent.stash_prompt_draft(StashCause::Chord);

        agent.prompt.set_text("already typing");

        agent.auto_restore_stash_after_send();

        assert_eq!(agent.prompt.text(), "already typing");
        assert!(agent.prompt_stash.is_some());

        agent.prompt.set_text("");
        agent.prompt_input_mode = crate::app::agent_view::PromptInputMode::Bash;

        agent.auto_restore_stash_after_send();

        assert_eq!(
            agent.prompt.text(),
            "",
            "moded composer must not be overwritten"
        );
        assert!(agent.prompt_stash.is_some());
    }
}
