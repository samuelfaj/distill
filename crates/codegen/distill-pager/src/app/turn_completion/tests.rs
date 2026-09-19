// Modified for Distill by Samuel Fajreldines, 2026.
//! Unit tests for the turn-finalize rails in [`super`] (`turn_completion`), split out via `#[path]` to keep the module itself small.

use super::*;
use crate::app::agent::AgentState;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEventBlock;
use crate::scrollback::state::ScrollbackState;
use std::path::PathBuf;
use std::time::Instant;

fn last_session_event(sb: &ScrollbackState) -> Option<SessionEvent> {
    (0..sb.len())
        .rev()
        .find_map(|i| match sb.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b)) => Some(b.event.clone()),
            _ => None,
        })
}

/// A viewer in TurnRunning with an adopted prompt id, ready to be finalized.
fn running_viewer(prompt_id: &str) -> AgentView {
    let mut agent = super::super::agent_view::test_agent_view(Some("s1"), PathBuf::from("/tmp"));
    agent.attached_as_viewer = true;
    agent.session.start_turn(&mut agent.scrollback);
    agent.session.current_prompt_id = Some(prompt_id.into());
    agent.turn_started_at = Some(Instant::now());
    agent
}

/// A driver in TurnRunning with a local prompt id (default `attached_as_viewer == false`).
fn running_driver(prompt_id: &str) -> AgentView {
    let mut agent = super::super::agent_view::test_agent_view(Some("s1"), PathBuf::from("/tmp"));
    agent.session.start_turn(&mut agent.scrollback);
    agent.session.current_prompt_id = Some(prompt_id.into());
    agent.turn_started_at = Some(Instant::now());
    agent
}

#[test]
fn viewer_finalize_idles_and_pushes_completed_marker() {
    let mut agent = running_viewer("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ViewerFinalized));
    assert!(agent.session.state.is_idle());
    assert!(agent.session.current_prompt_id.is_none());
    assert!(agent.turn_started_at.is_none());
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn viewer_finalize_duplicate_terminal_is_noop() {
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    let len_after_first = agent.scrollback.len();

    // A duplicate/stale terminal for the now-finished turn does nothing.
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::Ignored));
    assert!(agent.session.state.is_idle());
    assert_eq!(
        agent.scrollback.len(),
        len_after_first,
        "a duplicate terminal must not push a second marker"
    );
}

/// A cancelled terminal stamped `cancellationCategory: "HookDenied"` renders the blocked-by-hook marker, never "cancelled by user".
/// A policy block is not a user action, and the audit trail should not say it was.
/// Unknown categories and absent meta stay unnamed, not "by user".
#[test]
fn viewer_finalize_hook_denied_renders_blocked_marker() {
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancellation_category: Some(HOOK_DENIED_CATEGORY),
            ..Default::default()
        },
    );
    match last_session_event(&agent.scrollback) {
        Some(SessionEvent::TurnBlockedByHook { .. }) => {}
        other => panic!("expected TurnBlockedByHook, got {other:?}"),
    }

    // An unknown category stays unnamed (no false hook attribution, no "by user")
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancellation_category: Some("DoomLoop"),
            ..Default::default()
        },
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled {
            cause: crate::scrollback::blocks::CancelledBy::Unspecified,
            ..
        })
    ));
}

#[test]
fn viewer_finalize_names_passive_cancel_from_wire_meta() {
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancel_trigger: Some("session_close"),
            ..Default::default()
        },
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled {
            cause: crate::scrollback::blocks::CancelledBy::SessionClosed,
            ..
        })
    ));
}

#[test]
fn hook_denied_finalize_arms_local_queue_hold() {
    let mut agent = running_viewer("p1");
    agent.session.enqueue_prompt("queued follow-up".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancellation_category: Some(HOOK_DENIED_CATEGORY),
            ..Default::default()
        },
    );
    assert!(
        agent.session.hook_block_hold,
        "a hook-denied end must park the local queue"
    );
    assert!(
        agent.question_view.is_none(),
        "no in-flight stash means nothing to act on, so no card"
    );

    let mut agent = running_viewer("p1");
    agent.session.enqueue_prompt("queued follow-up".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancellation_category: None,
            ..Default::default()
        },
    );
    assert!(
        !agent.session.hook_block_hold,
        "a plain user cancel must not park the queue"
    );
}

fn stash_in_flight(agent: &mut AgentView) {
    let entry = agent
        .scrollback
        .push_block(crate::scrollback::RenderBlock::user_prompt(
            "deploy hihi to prod",
        ));
    agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
        text: "deploy hihi to prod".into(),
        images: vec![],
        scrollback_entry: entry,
        combined_scrollback_entries: vec![],
        chip_elements: vec![],
    });
    agent.note_self_originated_prompt("p1");
}

fn hook_denied_signal<'a>(context: Option<&'a serde_json::Value>) -> TerminalSignal<'a> {
    TerminalSignal {
        prompt_id: Some("p1"),
        stop_reason: Some("cancelled"),
        cancellation_category: Some(HOOK_DENIED_CATEGORY),
        cancellation_context: context,
        ..Default::default()
    }
}

#[test]
fn hook_denied_finalize_displaces_feedback_before_opening_the_card() {
    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    agent.feedback_modal = Some(crate::views::feedback_modal::FeedbackModalState::new(
        crate::views::feedback_modal::OpenFeedbackModal {
            text: Some("draft feedback".to_string()),
            ..Default::default()
        },
    ));

    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));

    assert!(agent.feedback_modal.is_none());
    assert!(agent.question_view.is_some());
    assert!(
        agent.scrollback.len() > 1,
        "displacement notice must be visible"
    );
}

#[test]
fn hook_denied_finalize_requeues_blocked_prompt_and_opens_card() {
    use crate::views::question_view::LocalQuestionKind;

    // The context is built through the shared wire type, so the test breaks if the shell's serialization and the pager's parse ever drift apart
    let context = serde_json::to_value(distill_shell::session::commands::CancellationContext {
        hook_name: Some("global/block-hihi:user_prompt_submit[0].hooks[0]".into()),
        reason: Some("it contains 'hihi'".into()),
        ..Default::default()
    })
    .unwrap();
    let mut agent = running_viewer("p1");
    agent.session.enqueue_prompt("queued follower".into());
    stash_in_flight(&mut agent);
    // dispatch_send_prompt sets this at send; the test mirrors that
    agent.session.tracker.expect_user_echo();
    let bubbles_before = agent.scrollback.len();
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(Some(&context)));
    assert!(
        !agent.session.tracker.expects_user_echo(),
        "a hook-denied finalize must clear the armed echo skip"
    );

    let texts: Vec<&str> = agent
        .session
        .pending_prompts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["deploy hihi to prod", "queued follower"],
        "the blocked prompt must requeue at the FRONT, keeping submit order"
    );
    assert!(agent.session.in_flight_prompt.is_none());

    let front_id = agent.session.pending_prompts.front().map(|p| p.id).unwrap();
    let qv = agent.question_view.as_ref().expect("card must open");
    assert!(
        matches!(qv.local_kind, Some(LocalQuestionKind::PromptBlocked { row_id }) if row_id == front_id),
        "the card must own the requeued front row, got {:?}",
        qv.local_kind
    );
    let q = qv.questions.first().expect("one question");
    let paragraphs: Vec<&str> = q.question.split("\n\n").collect();
    assert_eq!(
        paragraphs,
        vec![
            "Prompt blocked by global/block-hihi",
            "it contains 'hihi'",
            "1 more prompt waiting (queue paused)."
        ],
        "the card header is: framing with the short hook name, the reason \
         verbatim, the queue context — one paragraph each"
    );
    assert!(qv.no_freeform, "the card has no freeform 'Other' row");
    assert!(
        agent.session.blocked_prompt.as_ref().is_some_and(|b| {
            b.row_id == front_id
                && b.hook_name.as_deref()
                    == Some("global/block-hihi:user_prompt_submit[0].hooks[0]")
                && b.reason.as_deref() == Some("it contains 'hihi'")
        }),
        "the block context is stored for card reopens"
    );
    let labels: Vec<&str> = q.options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, vec!["Edit", "Resend", "Discard"]);
    let descriptions: Vec<&str> = q.options.iter().map(|o| o.description.as_str()).collect();
    assert_eq!(
        descriptions,
        vec![
            "Fix your prompt",
            "Send it unchanged. The hook may block it again.",
            "Remove it from the queue"
        ]
    );
    assert!(
        q.options
            .iter()
            .all(|o| o.preview.as_deref() == Some("deploy hihi to prod")),
        "every option previews the full blocked prompt"
    );
    assert!(
        agent.prompt.text().is_empty(),
        "the composer is stashed behind the card"
    );
    assert!(
        agent.scrollback.len() > bubbles_before,
        "the prompt bubble stays (plus the terminal marker); requeue is not a rewind"
    );

    // A composer draft does not stop the hard modal: the card still opens (the decision is mandatory) and the draft survives in the card's stash
    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    agent.prompt.set_text("a newer draft");
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    assert!(agent.question_view.is_some(), "card opens over a draft too");
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("deploy hihi to prod")
    );

    // No context (older shell): the card still opens with generic copy.
    let qv = agent.question_view.as_ref().unwrap();
    assert!(
        qv.questions
            .first()
            .is_some_and(|q| q.question.contains("Prompt blocked by a hook")),
        "missing context falls back to generic copy"
    );
}

#[test]
fn hook_denied_finalize_ignores_foreign_stash() {
    let mut agent = running_viewer("p1");
    // Adopted turn: the rewind stash exists but "p1" was never noted self-originated
    let entry = agent
        .scrollback
        .push_block(crate::scrollback::RenderBlock::user_prompt(
            "another client's prompt",
        ));
    agent.session.in_flight_prompt = Some(crate::app::agent::InFlightPrompt {
        text: "another client's prompt".into(),
        images: vec![],
        scrollback_entry: entry,
        combined_scrollback_entries: vec![],
        chip_elements: vec![],
    });
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    assert!(
        agent.session.hook_block_hold,
        "the hold still arms on a foreign block"
    );
    assert!(
        agent.session.pending_prompts.is_empty(),
        "another client's prompt must not requeue into the local queue"
    );
    assert!(
        agent.question_view.is_none(),
        "no card for another client's prompt"
    );
    assert!(
        agent
            .toast
            .as_ref()
            .is_some_and(|(m, _)| m.contains("queue is paused")),
        "a hold without a card must never be silent"
    );
}

#[test]
fn hook_denied_finalize_during_queue_edit_requeues_without_card() {
    let mut agent = running_viewer("p1");
    agent.session.enqueue_prompt("row being edited".into());
    let edited_id = agent.session.pending_prompts.front().unwrap().id;
    stash_in_flight(&mut agent);
    agent.prompt_mode = crate::app::queue_edit::PromptMode::EditingQueued {
        id: edited_id,
        original: "row being edited".into(),
        server_id: None,
        kind: crate::app::agent::QueueEntryKind::Prompt,
    };
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    let texts: Vec<&str> = agent
        .session
        .pending_prompts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    assert_eq!(
        texts,
        vec!["deploy hihi to prod", "row being edited"],
        "the blocked prompt requeues at the front even mid-edit"
    );
    assert!(
        agent.question_view.is_none(),
        "no card over a composer busy with a queue edit"
    );
    assert!(agent.session.hook_block_hold);
    assert!(
        matches!(
            agent.prompt_mode,
            crate::app::queue_edit::PromptMode::EditingQueued { id, .. } if id == edited_id
        ),
        "the in-progress edit survives the requeue"
    );
}

#[test]
fn blocked_prompt_card_refuses_skip() {
    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    assert!(agent.question_view.is_some());

    let ctrl_c = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('c'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let _ = agent.handle_question_key_for_test(&ctrl_c);
    assert!(
        agent.question_view.is_some(),
        "Ctrl+C must not dismiss the blocked-prompt card"
    );
    assert!(
        agent.session.hook_block_hold,
        "the hold survives the refused skip"
    );

    // The focused card's footer must not advertise the refused dismissal.
    let labels: Vec<String> = agent
        .current_shortcut_hints(&crate::actions::ActionRegistry::defaults())
        .iter()
        .map(|hint| hint.label.to_string())
        .collect();
    assert!(
        !labels.contains(&"dismiss".to_string()),
        "the hard-modal card must not hint a dismiss it refuses, got {labels:?}"
    );
    assert!(
        labels.contains(&"next answer".to_string()),
        "this probe must be looking at the focused card's own footer, got {labels:?}"
    );

    let esc = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    let _ = agent.handle_question_key_for_test(&esc);
    assert!(
        agent.question_view.is_some(),
        "Esc must not dismiss the blocked-prompt card"
    );

    // `y` (option copy) is disabled on this card: copying "Edit / Fix your prompt" is noise
    // A real copy always toasts, so no toast means no copy happened
    agent.toast = None;
    let yank = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('y'),
        crossterm::event::KeyModifiers::NONE,
    );
    let _ = agent.handle_question_key_for_test(&yank);
    assert!(
        agent.toast.is_none(),
        "y must not copy option text on the blocked-prompt card"
    );
    assert!(agent.question_view.is_some(), "and the card stays up");
}

/// Choosing Edit on the card and then pressing Esc must not resend the unfixed row: the hold stays and the card reopens.
#[test]
fn esc_from_blocked_row_edit_reopens_card() {
    use crate::views::question_view::LocalQuestionKind;

    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    let row_id = agent.session.pending_prompts.front().map(|p| p.id).unwrap();
    assert!(agent.question_view.is_some());

    // The router's Edit arm: close the card, open the row in queue-edit.
    agent.question_view = None;
    agent.enter_queue_edit(row_id, false, None);
    assert!(matches!(
        agent.prompt_mode,
        crate::app::queue_edit::PromptMode::EditingQueued { .. }
    ));

    let esc = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    let outcome = agent.handle_editing_queued_key(&esc);
    assert!(
        !matches!(
            outcome,
            Some(crate::app::app_view::InputOutcome::Action(
                crate::app::actions::Action::DrainQueue
            ))
        ),
        "Esc out of a blocked-row edit must not drain (silent resend), got {outcome:?}"
    );
    assert!(
        agent.session.hook_block_hold,
        "the hold survives a cancelled edit"
    );
    let qv = agent.question_view.as_ref().expect("the card reopens");
    assert!(
        matches!(qv.local_kind, Some(LocalQuestionKind::PromptBlocked { row_id: r }) if r == row_id)
    );
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("deploy hihi to prod"),
        "the unfixed row stays parked at the front"
    );
}

/// Saving the blocked row is the card's Edit resolution: the hold releases and the fixed row drains in place.
#[test]
fn saving_blocked_row_releases_hold() {
    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    let row_id = agent.session.pending_prompts.front().map(|p| p.id).unwrap();

    agent.question_view = None;
    agent.enter_queue_edit(row_id, false, None);
    agent.prompt.set_text("deploy to prod (fixed)");
    let outcome = agent.save_edited_queued_row(row_id, None, true);
    assert!(
        matches!(
            outcome,
            crate::app::app_view::InputOutcome::Action(crate::app::actions::Action::DrainQueue)
        ),
        "saving the blocked row resends it in place, got {outcome:?}"
    );
    assert!(!agent.session.hook_block_hold);
    assert!(agent.session.blocked_prompt.is_none());
    assert!(
        agent.question_view.is_none(),
        "no card after the resolution"
    );
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("deploy to prod (fixed)")
    );
}

/// A block landing mid-edit of an unrelated row must not lose the blocked prompt when that edit resolves.
/// Saving the other row keeps the hold and brings the card up.
#[test]
fn saving_unrelated_row_keeps_hold_and_reopens_card() {
    use crate::views::question_view::LocalQuestionKind;

    let mut agent = running_viewer("p1");
    agent.session.enqueue_prompt("row being edited".into());
    let edited_id = agent.session.pending_prompts.front().unwrap().id;
    stash_in_flight(&mut agent);
    agent.prompt_mode = crate::app::queue_edit::PromptMode::EditingQueued {
        id: edited_id,
        original: "row being edited".into(),
        server_id: None,
        kind: crate::app::agent::QueueEntryKind::Prompt,
    };
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    let blocked_id = agent.session.pending_prompts.front().map(|p| p.id).unwrap();
    assert_ne!(blocked_id, edited_id);

    agent.prompt.set_text("row being edited, now changed");
    let _ = agent.save_edited_queued_row(edited_id, None, true);
    assert!(
        agent.session.hook_block_hold,
        "saving another row is not the blocked prompt's resolution"
    );
    let qv = agent.question_view.as_ref().expect("the card opens now");
    assert!(
        matches!(qv.local_kind, Some(LocalQuestionKind::PromptBlocked { row_id }) if row_id == blocked_id)
    );
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("deploy hihi to prod"),
        "the blocked row stays parked at the front"
    );
}

/// A blocked-prompt card deferred behind an already-open question reopens when that question closes; the hold and context survive the collision.
#[test]
fn deferred_card_reopens_when_other_question_closes() {
    use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
    use distill_tools::implementations::distill::ask_user_question::{Question, QuestionOption};

    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    // Another local card is up when the block finalizes.
    let stashed = agent.prompt.stash();
    agent.question_view = Some(
        QuestionViewState::new(
            "dummy-fork".into(),
            vec![Question {
                question: "fork?".into(),
                id: None,
                options: vec![QuestionOption {
                    label: "No".into(),
                    description: "stay".into(),
                    preview: None,
                    id: None,
                }],
                multi_select: Some(false),
            }],
            stashed,
        )
        .with_local_kind(LocalQuestionKind::Fork { directive: None }),
    );

    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    let row_id = agent.session.pending_prompts.front().map(|p| p.id).unwrap();
    assert!(
        agent.session.hook_block_hold,
        "the hold arms behind the card"
    );
    assert!(
        matches!(
            agent
                .question_view
                .as_ref()
                .and_then(|q| q.local_kind.as_ref()),
            Some(LocalQuestionKind::Fork { .. })
        ),
        "the block card defers while the other question is open"
    );

    // Skipping the fork card (Ctrl+C) must bring the blocked-prompt card back.
    let ctrl_c = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('c'),
        crossterm::event::KeyModifiers::CONTROL,
    );
    let _ = agent.handle_question_key_for_test(&ctrl_c);
    let qv = agent
        .question_view
        .as_ref()
        .expect("the blocked-prompt card reopens");
    assert!(
        matches!(qv.local_kind, Some(LocalQuestionKind::PromptBlocked { row_id: r }) if r == row_id)
    );
}

/// An out-of-range option index must never reach the destructive Discard arm: the answer is ignored and the queue row survives.
#[test]
fn unknown_card_index_never_discards() {
    use crate::views::question_view::QuestionSelection;

    let mut agent = running_viewer("p1");
    stash_in_flight(&mut agent);
    let _ = finalize_turn_from_terminal(&mut agent, "s1", hook_denied_signal(None));
    agent.question_view.as_mut().unwrap().selections = vec![QuestionSelection::Single(Some(7))];

    let enter = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    let _ = agent.handle_question_key_for_test(&enter);
    assert_eq!(
        agent
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("deploy hihi to prod"),
        "a bogus index must not discard the blocked prompt"
    );
    assert!(
        agent.session.hook_block_hold,
        "the hold survives an ignored answer"
    );
}

/// One chooser for every rail: only the hook-denied category flips the copy.
#[test]
fn cancelled_turn_event_picks_marker_by_category() {
    let d = std::time::Duration::from_millis(700);
    assert!(matches!(
        cancelled_turn_event(None, Some(HOOK_DENIED_CATEGORY), d),
        SessionEvent::TurnBlockedByHook { .. }
    ));
    assert!(matches!(
        cancelled_turn_event(None, None, d),
        SessionEvent::TurnCancelled { .. }
    ));
    assert_eq!(
        cancelled_turn_event(None, Some(HOOK_DENIED_CATEGORY), d).message(),
        "Turn blocked by a hook in 0.7s."
    );
}

#[test]
fn cancelled_turn_event_names_passive_causes() {
    let d = std::time::Duration::from_secs(10);
    let banner = |trigger: Option<&str>, category: Option<&str>| {
        cancelled_turn_event(trigger, category, d).message()
    };
    assert_eq!(
        banner(Some("ctrl_c"), None),
        "Turn cancelled by user in 10s."
    );
    assert_eq!(
        banner(Some("mouse"), None),
        "Turn cancelled by user in 10s."
    );
    assert_eq!(
        banner(Some("dashboard_stop"), None),
        "Turn cancelled by user in 10s."
    );
    assert_eq!(
        banner(Some("session_close"), None),
        "Turn cancelled because the session closed in 10s."
    );
    assert_eq!(
        banner(Some("session_delete"), None),
        "Turn cancelled because the session closed in 10s."
    );
    assert_eq!(
        banner(Some("shutdown"), None),
        "Turn cancelled because the session shut down in 10s."
    );
    assert_eq!(
        banner(
            None,
            Some(distill_shell::session::commands::MAX_TURNS_REACHED_CATEGORY)
        ),
        "Turn cancelled after reaching the turn limit in 10s."
    );
    assert_eq!(
        banner(
            None,
            Some(distill_shell::session::commands::PERMISSION_REJECTED_CATEGORY)
        ),
        "Turn cancelled because a permission was denied in 10s."
    );
    assert_eq!(
        banner(
            None,
            Some(distill_shell::session::commands::PERMISSION_CANCELLED_CATEGORY)
        ),
        "Turn cancelled because a permission prompt was dismissed in 10s."
    );
    assert_eq!(
        banner(Some("host_interrupt"), None),
        "Turn cancelled by the agent host in 10s."
    );
    assert_eq!(banner(None, None), "Turn cancelled in 10s.");
    assert_eq!(banner(None, Some("MidTurnAbort")), "Turn cancelled in 10s.");
    assert_eq!(
        banner(Some("gateway_cancel"), None),
        "Turn cancelled in 10s."
    );
}

#[test]
fn viewer_finalize_stop_reason_to_marker_mapping() {
    // A "cancelled" stop renders the Turn cancelled marker
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            ..Default::default()
        },
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled { .. })
    ));

    // An "error" stop with agentResult renders TurnFailed with formatted text
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("error"),
            agent_result: Some(r#"API error (status 500): {"error":"boom"}"#),
            ..Default::default()
        },
    );
    match last_session_event(&agent.scrollback) {
        Some(SessionEvent::TurnFailed { error, .. }) => {
            assert_eq!(
                error,
                "Server error (500): Something went wrong on our side. Wait a minute and send again."
            );
        }
        other => panic!("expected TurnFailed, got {other:?}"),
    }

    // An "error" stop with a dedicated banner already in the trailing run: the banner explains the failure, so no redundant TurnFailed marker
    let mut agent = running_viewer("p1");
    agent
        .scrollback
        .push_block(RenderBlock::session_event(SessionEvent::RequestFailed {
            status: Some(500),
            headline: "Server error (500)".into(),
            detail: String::new(),
        }));
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("error"),
            ..Default::default()
        },
    );
    assert!(
        !matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ),
        "a trailing RequestFailed banner must suppress the viewer's TurnFailed"
    );

    // But a banner buried behind a substantive block is a previous turn's: the trailing-run scan stops and the marker is pushed
    let mut agent = running_viewer("p1");
    agent
        .scrollback
        .push_block(RenderBlock::session_event(SessionEvent::RequestFailed {
            status: Some(500),
            headline: "Server error (500)".into(),
            detail: String::new(),
        }));
    agent.scrollback.push_block(RenderBlock::user_prompt("hi"));
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("error"),
            ..Default::default()
        },
    );
    assert!(
        matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ),
        "a banner behind a substantive block must not suppress the marker"
    );

    // A "rate_limit" stop finishes the turn but pushes no marker (not actionable from a viewer)
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("rate_limit"),
            ..Default::default()
        },
    );
    assert!(agent.session.state.is_idle());
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "rate_limit must not push a marker on a viewer"
    );

    // An unknown or other reason renders Turn completed (the catch-all)
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("max_tokens"),
            ..Default::default()
        },
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCompleted { .. })
    ));
}

#[test]
fn driver_arms_reconcile_and_does_not_finish() {
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(
        matches!(agent.session.state, AgentState::TurnRunning),
        "the driver's turn must NOT be finished — the PromptResponse RPC owns it"
    );
    let pending = agent
        .pending_turn_end_reconcile
        .as_ref()
        .expect("the driver's awaited turn must arm a reconcile");
    assert_eq!(pending.prompt_id, "p1");
    assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
}

#[test]
fn driver_mismatched_prompt_id_does_not_arm() {
    // Stale/peer terminal must not arm reconcile on a different live turn.
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p-other"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::Ignored));
    assert!(agent.pending_turn_end_reconcile.is_none());
    assert!(matches!(agent.session.state, AgentState::TurnRunning));
}

#[test]
fn driver_missing_prompt_id_arms_against_current_when_idle_in_turn() {
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert_eq!(
        agent.pending_turn_end_reconcile.as_ref().unwrap().prompt_id,
        "p1"
    );
}

fn stream_agent_text(agent: &mut AgentView, text: &str) {
    use crate::acp::meta::NotificationMeta;
    use agent_client_protocol as acp;
    let meta = NotificationMeta::default();
    let _ = agent.session.tracker.handle_update(
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
        &meta,
        &mut agent.scrollback,
    );
}

/// A missing wire pid still arms the lost-PR reconcile (full teardown lives in `reconcile_overdue_turn_ends` / PromptResponse).
#[test]
fn repro_terminal_without_prompt_id_arms_reconcile_for_lost_pr() {
    let mut agent = running_driver("p1");
    stream_agent_text(&mut agent, "done");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(agent.pending_turn_end_reconcile.is_some());
    assert!(
        matches!(agent.session.state, AgentState::TurnRunning),
        "driver stays TurnRunning until PR or overdue reconcile"
    );
    assert_eq!(
        agent.session.tracker.activity(),
        Some(crate::acp::tracker::TurnActivity::Responding)
    );
}

/// If it armed here, the overdue reconcile would force-finish the live turn mid-write.
#[test]
fn driver_missing_prompt_id_ignored_during_tool_call_write() {
    let mut agent = running_driver("p1");
    assert!(
        agent
            .session
            .tracker
            .note_tool_call_arguments_delta(Some("spawn_subagent"), 0)
    );
    assert!(matches!(
        agent.session.tracker.activity(),
        Some(crate::acp::tracker::TurnActivity::WritingToolCall(_))
    ));

    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::Ignored));
    assert!(
        agent.pending_turn_end_reconcile.is_none(),
        "mid-write terminal must not arm the lost-PR reconcile"
    );
    assert!(matches!(agent.session.state, AgentState::TurnRunning));
}

/// A dead stream mid-write must not block lost-response recovery.
#[test]
fn driver_missing_prompt_id_arms_when_tool_call_write_is_stale() {
    let mut agent = running_driver("p1");
    agent
        .session
        .tracker
        .note_tool_call_arguments_delta(Some("spawn_subagent"), 0);
    agent.session.tracker.backdate_last_tool_call_delta(
        crate::acp::tracker::WRITING_DELTA_STALE_AFTER + std::time::Duration::from_secs(1),
    );

    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert_eq!(
        agent.pending_turn_end_reconcile.as_ref().unwrap().prompt_id,
        "p1"
    );
}

/// An exact pid still arms the reconcile (the control case).
#[test]
fn recovery_mode_matching_turn_completed_arms_reconcile_for_lost_pr() {
    let mut agent = running_driver("p1");
    stream_agent_text(&mut agent, "done");

    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    assert!(matches!(agent.session.state, AgentState::TurnRunning));
    assert!(agent.pending_turn_end_reconcile.is_some());
}

/// Re-arming the same pid keeps the earliest received_at (the grace does not extend forever).
#[test]
fn driver_rearm_same_pid_preserves_received_at() {
    let mut agent = running_driver("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    let first = agent
        .pending_turn_end_reconcile
        .as_ref()
        .unwrap()
        .received_at;
    std::thread::sleep(std::time::Duration::from_millis(5));
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("end_turn"),
            ..Default::default()
        },
    );
    let second = agent
        .pending_turn_end_reconcile
        .as_ref()
        .unwrap()
        .received_at;
    assert_eq!(first, second);
}

// ── End markers: always the plain event text (work lives in the status row) ──

fn insert_bg_task(agent: &mut AgentView, task_id: &str, is_monitor: bool) {
    agent.session.bg_tasks.insert(
        task_id.into(),
        crate::app::agent::BgTaskState {
            task_id: task_id.into(),
            tool_call_id: format!("call-{task_id}"),
            command: "sleep 5".into(),
            description: None,
            cwd: "/tmp".into(),
            output_file: "/tmp/out".into(),
            status: crate::app::agent::BgTaskStatus::Running,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            exit_code: None,
            signal: None,
            stdout: String::new(),
            stdout_line_count: 0,
            truncated: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            is_monitor,
            restored_from_replay: false,
        },
    );
}

/// The newest session-event marker block.
fn last_marker_block(agent: &AgentView) -> &SessionEventBlock {
    (0..agent.scrollback.len())
        .rev()
        .find_map(|i| match agent.scrollback.get(i).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(b)) => Some(b),
            _ => None,
        })
        .expect("a session-event marker must exist")
}

#[test]
fn real_end_marker_stays_plain_with_running_work() {
    let mut agent = running_driver("p1");
    insert_bg_task(&mut agent, "bg-1", false);

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
    );

    let block = last_marker_block(&agent);
    assert_eq!(block.event.message(), "Worked for 2.0s");
    assert_eq!(
        agent.watchers().commands,
        1,
        "the running command feeds the status-row watchers cue instead"
    );
}

#[test]
fn workless_marker_renders_legacy_text() {
    let mut agent = running_driver("p1");

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(2)),
        }),
    );

    let block = last_marker_block(&agent);
    assert_eq!(block.event.message(), "Worked for 2.0s");
}

// ── Send-now cancel marker suppression (viewer finalize rail) ────────

/// A viewer finalizing a `send_now`-stamped `cancelled` terminal pushes no marker.
#[test]
fn viewer_finalize_suppresses_send_now_cancel_marker() {
    let mut agent = running_viewer("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancel_trigger: Some("send_now"),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ViewerFinalized));
    assert!(agent.session.state.is_idle(), "the turn still finishes");
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "a send-now cancel pushes no marker on a viewer"
    );

    // A non-send-now trigger keeps the marker even with a local expectation set (wire is authoritative)
    let mut agent = running_viewer("p1");
    agent.expect_send_now_cancel = Some("p-mine".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancel_trigger: Some("ctrl_c"),
            ..Default::default()
        },
    );
    assert!(matches!(
        last_session_event(&agent.scrollback),
        Some(SessionEvent::TurnCancelled { .. })
    ));

    // Wire `host_interrupt` is authoritative: a leaked stamp on Send Now paints the host banner.
    let mut agent = running_viewer("p1");
    agent.expect_send_now_cancel = Some("p-mine".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancel_trigger: Some("host_interrupt"),
            ..Default::default()
        },
    );
    match last_session_event(&agent.scrollback) {
        Some(ev @ SessionEvent::TurnCancelled { .. }) => {
            assert!(
                ev.message().contains("agent host"),
                "wire host_interrupt is authoritative: {}",
                ev.message()
            );
        }
        other => panic!("expected a host-interrupt banner, got {other:?}"),
    }

    // Older shell (no meta): the local expectation is the fallback
    let mut agent = running_viewer("p1");
    agent.expect_send_now_cancel = Some("p-mine".into());
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            ..Default::default()
        },
    );
    assert!(
        last_session_event(&agent.scrollback).is_none(),
        "the armed expectation suppresses the marker without wire meta"
    );
    assert!(
        agent.expect_send_now_cancel.is_none(),
        "the expectation is consumed on the viewer finalize"
    );
}

/// The driver arm records the broadcast's `cancelTrigger` for a lost-RPC reconcile.
#[test]
fn driver_arm_records_cancel_trigger_for_reconcile() {
    let mut agent = running_driver("p1");
    let outcome = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("cancelled"),
            cancel_trigger: Some("send_now"),
            cancellation_category: Some(HOOK_DENIED_CATEGORY),
            ..Default::default()
        },
    );
    assert!(matches!(outcome, TerminalApply::ReconcileArmed));
    let pending = agent
        .pending_turn_end_reconcile
        .as_ref()
        .expect("reconcile must be armed");
    assert_eq!(pending.cancel_trigger.as_deref(), Some("send_now"));
    assert_eq!(
        pending.cancellation_category.as_deref(),
        Some(HOOK_DENIED_CATEGORY),
        "the armed pending must park the category for the reconcile sweep"
    );
}

/// The turn-end marker takes no fold path: a park has no row to fold into.
#[test]
fn turn_end_after_park_pushes_single_marker() {
    use crate::app::agent_view::test_fixtures::count_turn_markers;

    let mut agent = running_driver("p1");
    super::super::agent_view::test_fixtures::simulate_task_output_wait(&mut agent, "bg-1");
    assert!(agent.renders_parked());
    assert_eq!(count_turn_markers(&agent), 0, "the park writes no marker");

    push_turn_terminal_marker(
        &mut agent,
        Some(SessionEvent::TurnCompleted {
            elapsed: Some(std::time::Duration::from_secs(5)),
        }),
    );

    assert_eq!(
        count_turn_markers(&agent),
        1,
        "the real turn end pushes exactly one marker"
    );
    assert_eq!(last_marker_block(&agent).event.message(), "Worked for 5.0s");
}

fn base_input<'a>(stop: TurnStopReason) -> TerminalMarkerInput<'a> {
    TerminalMarkerInput {
        stop,
        elapsed_ms: Some(1000),
        agent_result: None,
        send_now_cancel: false,
        cancel_trigger: None,
        cancellation_category: None,
        error_kind: None,
        error_banner_present: false,
    }
}

#[test]
fn turn_stop_reason_parses_exact_tokens() {
    assert_eq!(TurnStopReason::from("end_turn"), TurnStopReason::EndTurn);
    assert_eq!(TurnStopReason::from("cancelled"), TurnStopReason::Cancelled);
    assert_eq!(TurnStopReason::from("refusal"), TurnStopReason::Refusal);
    assert_eq!(
        TurnStopReason::from("rate_limit"),
        TurnStopReason::RateLimit
    );
    assert_eq!(TurnStopReason::from("error"), TurnStopReason::Error);
    assert_eq!(
        TurnStopReason::from("max_tokens"),
        TurnStopReason::MaxTokens
    );
    assert_eq!(
        TurnStopReason::from("max_turn_requests"),
        TurnStopReason::MaxTurnRequests
    );
    assert_eq!(TurnStopReason::from("nope"), TurnStopReason::Unknown);
}

#[test]
fn turn_stop_reason_missing_is_unknown() {
    assert_eq!(TurnStopReason::from(None), TurnStopReason::Unknown);
    assert_eq!(TurnStopReason::from(Some("")), TurnStopReason::Unknown);
}

#[test]
fn classifier_completed_with_elapsed() {
    let ev = terminal_marker(base_input(TurnStopReason::EndTurn)).unwrap();
    assert!(matches!(
        ev,
        SessionEvent::TurnCompleted { elapsed: Some(d) } if d == std::time::Duration::from_millis(1000)
    ));
}

#[test]
fn classifier_completed_without_elapsed_is_none() {
    let mut input = base_input(TurnStopReason::EndTurn);
    input.elapsed_ms = None;
    let ev = terminal_marker(input).unwrap();
    assert!(matches!(ev, SessionEvent::TurnCompleted { elapsed: None }));
}

#[test]
fn classifier_refusal_max_tokens_max_requests_unknown_are_completed() {
    for stop in [
        TurnStopReason::Refusal,
        TurnStopReason::MaxTokens,
        TurnStopReason::MaxTurnRequests,
        TurnStopReason::Unknown,
    ] {
        assert!(matches!(
            terminal_marker(base_input(stop)),
            Some(SessionEvent::TurnCompleted { .. })
        ));
    }
}

#[test]
fn classifier_cancelled_and_hook_denied() {
    let ev = terminal_marker(base_input(TurnStopReason::Cancelled)).unwrap();
    assert!(matches!(
        ev,
        SessionEvent::TurnCancelled {
            cause: crate::scrollback::blocks::CancelledBy::Unspecified,
            ..
        }
    ));
    let mut input = base_input(TurnStopReason::Cancelled);
    input.cancellation_category = Some(HOOK_DENIED_CATEGORY);
    let ev = terminal_marker(input).unwrap();
    assert!(matches!(ev, SessionEvent::TurnBlockedByHook { .. }));
}

#[test]
fn classifier_cancelled_uses_cancel_trigger() {
    let mut input = base_input(TurnStopReason::Cancelled);
    input.cancel_trigger = Some("ctrl_c");
    assert!(matches!(
        terminal_marker(input),
        Some(SessionEvent::TurnCancelled {
            cause: crate::scrollback::blocks::CancelledBy::User,
            ..
        })
    ));
    let mut input = base_input(TurnStopReason::Cancelled);
    input.cancel_trigger = Some("session_close");
    assert!(matches!(
        terminal_marker(input),
        Some(SessionEvent::TurnCancelled {
            cause: crate::scrollback::blocks::CancelledBy::SessionClosed,
            ..
        })
    ));
}

#[test]
fn classifier_cancelled_missing_elapsed_is_zero() {
    let mut input = base_input(TurnStopReason::Cancelled);
    input.elapsed_ms = None;
    match terminal_marker(input) {
        Some(SessionEvent::TurnCancelled { elapsed, .. }) => {
            assert_eq!(elapsed, std::time::Duration::ZERO);
        }
        other => panic!("expected cancelled, got {other:?}"),
    }
}

#[test]
fn classifier_send_now_and_rate_limit_are_none() {
    assert!(
        terminal_marker(TerminalMarkerInput {
            send_now_cancel: true,
            ..base_input(TurnStopReason::Cancelled)
        })
        .is_none()
    );
    assert!(
        terminal_marker(TerminalMarkerInput {
            send_now_cancel: true,
            cancellation_category: Some(HOOK_DENIED_CATEGORY),
            ..base_input(TurnStopReason::Cancelled)
        })
        .is_none()
    );
    assert!(terminal_marker(base_input(TurnStopReason::RateLimit)).is_none());
}

#[test]
fn classifier_error_banner_and_failed_elapsed() {
    assert!(matches!(
        terminal_marker(TerminalMarkerInput {
            agent_result: Some("boom"),
            ..base_input(TurnStopReason::Error)
        }),
        Some(SessionEvent::TurnFailed {
            elapsed: Some(_),
            ..
        })
    ));
    assert!(
        terminal_marker(TerminalMarkerInput {
            agent_result: Some("boom"),
            error_banner_present: true,
            ..base_input(TurnStopReason::Error)
        })
        .is_none()
    );
    match terminal_marker(TerminalMarkerInput {
        agent_result: Some("boom"),
        elapsed_ms: None,
        ..base_input(TurnStopReason::Error)
    }) {
        Some(SessionEvent::TurnFailed { elapsed: None, .. }) => {}
        other => panic!("expected failed with None elapsed, got {other:?}"),
    }
}

#[test]
fn classifier_error_typed_truncation_kind_picks_truncation_copy() {
    // The typed kind alone must pick the truncation copy; the flattened message contains nothing text-matching could find
    match terminal_marker(TerminalMarkerInput {
        agent_result: Some("turn ended early"),
        error_kind: Some(WireErrorType::MaxTokensTruncation),
        ..base_input(TurnStopReason::Error)
    }) {
        Some(SessionEvent::TurnFailed { error, .. }) => {
            assert_eq!(error, "Response truncated: turn ended early");
        }
        other => panic!("expected TurnFailed, got {other:?}"),
    }
}

/// The real wire pair (the canonical truncation text as `agent_result` plus the typed kind) renders the exact user-visible copy.
/// This pins the headline-echo dedupe: a regression there would ship "Response truncated: response truncated by max_tokens".
/// Every placeholder-text test would stay green through that regression.
#[test]
fn viewer_finalize_truncation_wire_pair_renders_exact_user_copy() {
    let mut agent = running_viewer("p1");
    let _ = finalize_turn_from_terminal(
        &mut agent,
        "s1",
        TerminalSignal {
            prompt_id: Some("p1"),
            stop_reason: Some("error"),
            agent_result: Some(distill_shell::sampling::error::MAX_TOKENS_TRUNCATION_MESSAGE),
            error_kind: Some(WireErrorType::MaxTokensTruncation),
            ..Default::default()
        },
    );
    match last_session_event(&agent.scrollback) {
        Some(SessionEvent::TurnFailed { error, .. }) => {
            assert_eq!(error, "Response truncated: The model hit its output limit.");
        }
        other => panic!("expected TurnFailed, got {other:?}"),
    }
}
