//! Tests for sending a prompt through a parked wait, including after `/btw`.

use super::*;

use crate::app::agent::AgentState;
use crate::app::agent_view::test_fixtures::simulate_task_output_wait;
use crate::scrollback::block::RenderBlock;
use crate::views::btw_overlay::BtwOverlayState;
use agent_client_protocol as acp;

fn agent_ref(app: &AppView, id: AgentId) -> &AgentView {
    let Some(agent) = app.agents.get(&id) else {
        panic!("expected agent {id:?}");
    };
    agent
}

fn running_turn_app() -> AppView {
    let mut app = test_app_with_agent();
    let agent = app.agents.get_mut(&AgentId(0)).unwrap();
    agent.session.state = AgentState::TurnRunning;
    agent.session.current_prompt_id = Some("p1".into());
    agent.front_message_committed = true;
    app
}

fn sent_texts(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::SendInterject { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn sent_interjects(effects: &[Effect]) -> Vec<(String, String)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::SendInterject {
                text,
                interjection_id,
                ..
            } => Some((text.clone(), interjection_id.clone())),
            _ => None,
        })
        .collect()
}

fn interjection_texts(app: &AppView) -> Vec<String> {
    let agent = agent_ref(app, AgentId(0));
    (0..agent.scrollback.len())
        .filter_map(|idx| {
            let entry = agent.scrollback.entry(idx)?;
            match &entry.block {
                RenderBlock::UserPrompt(p) if p.is_interjection => Some(p.text.clone()),
                _ => None,
            }
        })
        .collect()
}

/// Forces the local drip-feed send path so a mid-turn Enter enqueues locally instead of handing the send to the server.
struct LocalFollowUp {
    previous: crate::appearance::FollowUpBehavior,
}
impl LocalFollowUp {
    fn enter(app: &mut AppView, behavior: crate::appearance::FollowUpBehavior) -> Self {
        app.leader_mode = false;
        let previous = crate::appearance::cache::load_follow_up_behavior();
        crate::appearance::cache::set_follow_up_behavior(behavior);
        Self { previous }
    }
    fn queue(app: &mut AppView) -> Self {
        Self::enter(app, crate::appearance::FollowUpBehavior::Queue)
    }
    fn steer(app: &mut AppView) -> Self {
        Self::enter(app, crate::appearance::FollowUpBehavior::Steer)
    }
}
impl Drop for LocalFollowUp {
    fn drop(&mut self) {
        crate::appearance::cache::set_follow_up_behavior(self.previous);
    }
}

/// Sending while parked must go through even when the last thing the user did was `/btw` (the overlay is still open).
#[test]
fn send_while_waiting_goes_through_when_btw_overlay_is_open() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "held");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        assert!(agent.is_parked_on_sendable_wait());
        agent.btw_state = Some(BtwOverlayState::done(
            "what were we doing".into(),
            "waiting on the task".into(),
        ));
        agent.btw_focused = true;
    }

    let effects = dispatch_send_prompt(&mut app, "keep going".into());

    assert_eq!(
        sent_texts(&effects),
        vec!["held".to_string(), "keep going".to_string()]
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .is_empty()
    );
    assert!(
        agent_ref(&app, AgentId(0)).btw_state.is_some(),
        "releasing the send must not dismiss the /btw overlay",
    );
}

/// A prompt queued *before* `/btw` (a follow-up typed while the model was thinking) must stay queued when the answer lands.
/// The send path is what releases a message typed while parked; `/btw` completion must not flush the queue.
#[test]
fn btw_response_does_not_flush_an_unrelated_queued_prompt() {
    let mut app = running_turn_app();
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        agent.btw_state = Some(BtwOverlayState::Loading {
            question: "status?".into(),
        });
    }
    enqueue_local(&mut app, AgentId(0), "queued before btw");

    let effects = dispatch_task_result(
        TaskResult::BtwResponse {
            image_notice: None,
            agent_id: AgentId(0),
            result: Ok("still waiting".into()),
            minimal_request_id: None,
        },
        &mut app,
    );

    assert!(
        sent_texts(&effects).is_empty(),
        "btw completion must not interject a pre-queued follow-up, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued before btw")
    );
    assert!(
        matches!(
            agent_ref(&app, AgentId(0)).btw_state,
            Some(BtwOverlayState::Done { .. })
        ),
        "the overlay must still show the answer",
    );
}

#[test]
fn queue_mode_send_while_waiting_stays_queued() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::queue(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        assert!(agent.is_parked_on_sendable_wait());
    }

    let effects = dispatch_send_prompt(&mut app, "just typed".into());

    assert!(
        sent_texts(&effects).is_empty(),
        "Queue must not interject during a wait, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["queued while thinking", "just typed"]
    );
}

/// Enter during a wait flushes earlier held follow-ups, then the message just typed.
#[test]
fn send_while_waiting_flushes_held_follow_ups_then_the_new_prompt() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        assert!(agent.is_parked_on_sendable_wait());
    }

    let effects = dispatch_send_prompt(&mut app, "just typed".into());

    assert_eq!(
        sent_texts(&effects),
        vec![
            "queued while thinking".to_string(),
            "just typed".to_string()
        ]
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn queue_mode_send_while_waiting_with_empty_queue_stays_queued() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::queue(&mut app);
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        assert!(agent.is_parked_on_sendable_wait());
    }

    let effects = dispatch_send_prompt(&mut app, "just typed".into());

    assert!(
        sent_texts(&effects).is_empty(),
        "Queue must not wait-interject even with an empty pile, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["just typed"]
    );
}

#[test]
fn parked_wait_does_not_flush_held_follow_ups_in_queue_mode() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::queue(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert!(
        sent_texts(&effects).is_empty(),
        "Queue mode must keep follow-ups held across a wait, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["queued while thinking"]
    );
}

#[test]
fn parked_wait_flushes_held_follow_ups_in_steer_mode() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    enqueue_local(&mut app, AgentId(0), "also queued");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        assert!(agent.is_parked_on_sendable_wait());
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert_eq!(
        sent_texts(&effects),
        vec![
            "queued while thinking".to_string(),
            "also queued".to_string()
        ]
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .is_empty(),
        "wait-start flush must empty the local held queue"
    );
}

#[test]
fn send_after_wait_flush_does_not_resend_earlier_follow_up() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }
    let flushed = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));
    assert_eq!(
        sent_texts(&flushed),
        vec!["queued while thinking".to_string()]
    );

    let effects = dispatch_send_prompt(&mut app, "just typed".into());

    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::SendPrompt { text, .. } if text == "just typed"
        )),
        "the new message must send, got {effects:?}"
    );
    assert!(
        !effects.iter().any(|e| match e {
            Effect::SendInterject { text, .. } | Effect::SendPrompt { text, .. } => {
                text == "queued while thinking"
            }
            _ => false,
        }),
        "wait-start flush already sent the earlier follow-up, got {effects:?}"
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .is_empty(),
        "the earlier follow-up must not remain to drain a second time, leftover {:?}",
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
    );
}

#[test]
fn flush_skips_unflushable_front_and_releases_later_prompts() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.enqueue_bash_command("ls".into());
        simulate_task_output_wait(agent, "task-1");
    }
    enqueue_local(&mut app, AgentId(0), "queued while thinking");

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert_eq!(
        sent_texts(&effects),
        vec!["queued while thinking".to_string()]
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["ls"]
    );
}

#[test]
fn flush_does_not_run_while_send_now_cancel_is_armed() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        agent.expect_send_now_cancel = Some("send-now-echo".into());
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert!(sent_texts(&effects).is_empty(), "got {effects:?}");
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued while thinking")
    );
}

#[test]
fn flush_paints_identical_follow_ups_separately() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "ok");
    enqueue_local(&mut app, AgentId(0), "ok");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert_eq!(
        sent_texts(&effects),
        vec!["ok".to_string(), "ok".to_string()]
    );
    assert_eq!(
        interjection_texts(&app),
        vec!["ok".to_string(), "ok".to_string()]
    );
}

#[test]
fn failed_flush_drops_trailing_echoes_before_requeue() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    enqueue_local(&mut app, AgentId(0), "also queued");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }
    let flushed = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));
    assert_eq!(interjection_texts(&app).len(), 2);
    let remaining = sent_interjects(&flushed)
        .into_iter()
        .map(|(text, id)| {
            (
                text,
                id,
                Some(vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "<skill>body</skill>",
                ))]),
            )
        })
        .collect();

    let _ = dispatch_task_result(
        TaskResult::InterjectFailed {
            agent_id: AgentId(0),
            error: "channel closed".into(),
            remaining,
        },
        &mut app,
    );

    assert!(
        interjection_texts(&app).is_empty(),
        "failed send must not leave optimistic echoes, leftover {:?}",
        interjection_texts(&app)
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["queued while thinking", "also queued"]
    );
    assert!(agent_ref(&app, AgentId(0)).self_interjection_ids.is_empty());
    assert!(
        agent_ref(&app, AgentId(0))
            .interjection_painted_blocks
            .is_empty()
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .all(|p| p.wire_matches_display() && p.wire_blocks.is_none()),
        "failed image/text leftover must stay flushable"
    );
}

#[test]
fn failed_flush_drops_only_the_failed_identical_follow_up() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "ok");
    enqueue_local(&mut app, AgentId(0), "ok");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }
    let flushed = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));
    let sent = sent_interjects(&flushed);
    assert_eq!(sent.len(), 2);
    let Some((first_text, first_id)) = sent.first().cloned() else {
        panic!("expected a flushed interject");
    };

    let _ = dispatch_task_result(
        TaskResult::InterjectFailed {
            agent_id: AgentId(0),
            error: "channel closed".into(),
            remaining: vec![(first_text, first_id, None)],
        },
        &mut app,
    );

    assert_eq!(interjection_texts(&app), vec!["ok".to_string()]);
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["ok"]
    );
}

#[test]
fn flush_does_not_leapfrog_held_server_queue_front() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        agent.shared_queue = vec![crate::app::prompt_queue::QueueEntryWire {
            id: "server-first".into(),
            version: 1,
            owner: None,
            last_editor: None,
            kind: "prompt".into(),
            text: "from another client".into(),
            position: 0,
            combined_texts: None,
        }];
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert!(sent_texts(&effects).is_empty(), "got {effects:?}");
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued while thinking")
    );
}

#[test]
fn flush_does_not_run_during_loading_replay() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
        agent.session.loading_replay = true;
    }

    let effects = super::super::queue::flush_held_local_queue_into_wait(&mut app, Some(AgentId(0)));

    assert!(sent_texts(&effects).is_empty(), "got {effects:?}");
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .front()
            .map(|p| p.text.as_str()),
        Some("queued while thinking")
    );
}

#[test]
fn interject_flushes_held_follow_ups() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }

    let effects = dispatch(
        Action::Interject {
            text: "just typed".into(),
            images: vec![],
        },
        &mut app,
    );

    assert_eq!(
        sent_texts(&effects),
        vec![
            "queued while thinking".to_string(),
            "just typed".to_string()
        ]
    );
    assert!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .is_empty()
    );
}

#[test]
fn send_now_does_not_interject_held_follow_ups_into_cancelled_turn() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::steer(&mut app);
    enqueue_local(&mut app, AgentId(0), "queued while thinking");
    {
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        simulate_task_output_wait(agent, "task-1");
    }

    let effects = dispatch(
        Action::SendPromptNow {
            text: "just typed".into(),
            images: vec![],
        },
        &mut app,
    );

    assert!(
        sent_texts(&effects).is_empty(),
        "send-now must not flush into the dying turn, got {effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::SendPromptNow { .. })),
        "send-now must still fire, got {effects:?}"
    );
    assert_eq!(
        agent_ref(&app, AgentId(0))
            .session
            .pending_prompts
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>(),
        vec!["queued while thinking"]
    );
}

/// A thinking turn (running, but not parked / watching) still queues.
#[test]
fn send_while_thinking_stays_queued() {
    let mut app = running_turn_app();
    let _mode = LocalFollowUp::queue(&mut app);

    let effects = dispatch_send_prompt(&mut app, "later".into());

    assert!(
        sent_texts(&effects).is_empty(),
        "a thinking turn must not interject, got {effects:?}"
    );
    assert_eq!(agent_ref(&app, AgentId(0)).session.pending_prompts.len(), 1);
}
