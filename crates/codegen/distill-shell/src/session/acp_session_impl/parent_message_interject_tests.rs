// Modified for Distill by Samuel Fajreldines, 2026.
use super::tests::{admit, await_with_timeout, message_with_text, set_running, start_turn};
use super::*;
use crate::session::telemetry::{
    ActiveAgentMessageSafePointTrigger, ActiveAgentMessageSettlementStatus, project_settlement,
};
use distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

const STEER_TEXT: &str = "steer text";
const INTERJECT_TEXT: &str = "interject text";

async fn interject_signal(actor: &SessionActor) -> Arc<ParentInterjectSignal> {
    await_with_timeout(actor.state.lock())
        .await
        .message_delivery
        .interject_signal()
}

async fn admit_interject(
    actor: &Arc<SessionActor>,
    id: &str,
) -> crate::agent::subagent::PromptTurnReceipt {
    admit(
        actor,
        message_with_text(id, INTERJECT_TEXT),
        ActiveAgentMessageOperation::Interject,
    )
    .await
}

fn settled_trigger(
    receipt: crate::agent::subagent::PromptTurnReceipt,
) -> Option<ActiveAgentMessageSafePointTrigger> {
    let (_, settled) = project_settlement(
        Some(receipt.telemetry),
        ActiveAgentMessageSettlementStatus::Completed,
        std::time::Instant::now(),
    )
    .expect("admitted settlement projects");
    settled.safe_point_trigger
}

async fn complete_running_turn(actor: &SessionActor, cause: TerminalCause) -> Vec<String> {
    let mut state = await_with_timeout(actor.state.lock()).await;
    let binding = turn_binding(state.running_task.as_ref().expect("running task"));
    actor.transition_parent_messages(&mut state, TerminalTarget::Turn(&binding), cause);
    state
        .pending_inputs
        .iter()
        .map(|input| input.prompt_id.clone())
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn running_interject_admits_a_slot_with_interject_effective_and_no_queue_row() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        set_running(&actor, "running").await;

        let receipt = admit(
            &actor,
            message_with_text("i1", INTERJECT_TEXT),
            ActiveAgentMessageOperation::Interject,
        )
        .await;

        assert_eq!("parent-message-i1", receipt.prompt_id);
        let state = await_with_timeout(actor.state.lock()).await;
        let binding = turn_binding(state.running_task.as_ref().expect("running task"));
        let pending = state.message_delivery.lifecycle.pending_messages(&binding);
        let [pending_message] = pending.as_slice() else {
            panic!("expected one pending message, got {}", pending.len());
        };
        assert!(pending_message.content().is_interject());
        assert_eq!(1, state.message_delivery.lifecycle.len());
        assert_eq!(
            state
                .pending_inputs
                .iter()
                .map(|input| input.prompt_id.as_str())
                .collect::<Vec<_>>(),
            ["running"]
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn interject_drains_ahead_of_an_earlier_steer_in_one_batch() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
        let (actor, _) = super::super::support::create_test_actor_with_chat_persistence(
            0,
            256_000,
            85,
            gateway_tx,
            persistence_tx.clone(),
            Box::new(crate::session::chat_persistence::ChannelChatPersistence::new(persistence_tx)),
        )
        .await;
        let actor = Arc::new(actor);
        let persistence_log = Rc::new(RefCell::new(Vec::new()));
        tokio::task::spawn_local({
            let persistence_log = Rc::clone(&persistence_log);
            async move {
                while let Some(message) = persistence_rx.recv().await {
                    match message {
                        PersistenceMsg::FlushAndAck { respond_to } => {
                            persistence_log.borrow_mut().push("barrier".to_owned());
                            let _ = respond_to.send(Ok(()));
                        }
                        PersistenceMsg::Update(update) => {
                            let json = serde_json::to_string(&update).expect("serialize update");
                            for text in [STEER_TEXT, INTERJECT_TEXT] {
                                if json.contains(text) {
                                    persistence_log.borrow_mut().push(format!("chunk:{text}"));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        });
        set_running(&actor, "running").await;
        admit(
            &actor,
            message_with_text("s1", STEER_TEXT),
            ActiveAgentMessageOperation::Steer,
        )
        .await;
        admit(
            &actor,
            message_with_text("i1", INTERJECT_TEXT),
            ActiveAgentMessageOperation::Interject,
        )
        .await;

        assert!(actor.drain_parent_messages_at_safe_point().await);
        // A trailing barrier proves the interceptor has consumed everything the drain sent.
        let (settled_tx, settled_rx) = oneshot::channel();
        actor
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck {
                respond_to: settled_tx,
            })
            .expect("interceptor alive");
        await_with_timeout(settled_rx)
            .await
            .expect("interceptor acks")
            .expect("barrier ok");

        assert_eq!(
            [
                "barrier",
                "chunk:interject text",
                "chunk:steer text",
                "barrier",
            ],
            persistence_log.borrow().as_slice()
        );
        let conversation = await_with_timeout(actor.chat_state_handle.get_conversation()).await;
        let texts = conversation.iter().map(ConversationItem::text_content);
        assert_eq!(texts.collect::<Vec<_>>(), [INTERJECT_TEXT, STEER_TEXT]);
        let mut state = await_with_timeout(actor.state.lock()).await;
        let binding = turn_binding(state.running_task.as_ref().expect("running task"));
        let (completions, had_fallbacks) = actor.transition_parent_messages(
            &mut state,
            TerminalTarget::Turn(&binding),
            TerminalCause::Completion,
        );
        assert_eq!((2, false), (completions.len(), had_fallbacks));
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn signal_is_armed_only_by_pending_interject_slots_for_the_running_turn() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let signal = interject_signal(&actor).await;
        set_running(&actor, "running").await;

        let mut observed = vec![signal.is_pending()];
        admit(
            &actor,
            message_with_text("s1", STEER_TEXT),
            ActiveAgentMessageOperation::Steer,
        )
        .await;
        observed.push(signal.is_pending());
        admit_interject(&actor, "i1").await;
        observed.push(signal.is_pending());
        admit_interject(&actor, "i2").await;
        observed.push(signal.is_pending());

        assert_eq!(observed, [false, false, true, true]);
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn begin_delivery_and_transition_clear_the_signal() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let signal = interject_signal(&actor).await;
        set_running(&actor, "running").await;

        admit_interject(&actor, "i1").await;
        assert!(actor.drain_parent_messages_at_safe_point().await);
        let after_drain = signal.is_pending();
        admit_interject(&actor, "i2").await;
        let queue = complete_running_turn(&actor, TerminalCause::Completion).await;

        assert_eq!((false, false), (after_drain, signal.is_pending()));
        assert_eq!(queue, ["running", "parent-message-i2"]);
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_after_a_wait_abort_records_the_wait_abort_trigger_once() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let signal = interject_signal(&actor).await;
        let turn = start_turn(&actor, "running").await;

        let aborted = admit_interject(&actor, "i1").await;
        signal.note_wait_aborted(turn);
        assert!(actor.drain_parent_messages_at_safe_point().await);
        let natural = admit_interject(&actor, "i2").await;
        assert!(actor.drain_parent_messages_at_safe_point().await);

        assert_eq!(
            [
                Some(ActiveAgentMessageSafePointTrigger::WaitAbort),
                Some(ActiveAgentMessageSafePointTrigger::Natural),
            ],
            [settled_trigger(aborted), settled_trigger(natural)]
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_after_a_wait_abort_does_not_leak_the_trigger_into_the_next_turn() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let signal = interject_signal(&actor).await;
        let cancelled = start_turn(&actor, "turn-1").await;

        admit_interject(&actor, "i1").await;
        signal.note_wait_aborted(cancelled);
        complete_running_turn(&actor, TerminalCause::SoftCancel).await;
        let mark_survived = signal.take_wait_aborted(cancelled);
        start_turn(&actor, "turn-2").await;
        let receipt = admit_interject(&actor, "i2").await;
        assert!(actor.drain_parent_messages_at_safe_point().await);

        assert_eq!(
            (false, Some(ActiveAgentMessageSafePointTrigger::Natural)),
            (mark_survived, settled_trigger(receipt))
        );
    }))
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn stale_completion_for_an_earlier_binding_keeps_the_current_turns_wait_abort() {
    let local = tokio::task::LocalSet::new();
    await_with_timeout(local.run_until(async {
        let (actor, _) = await_with_timeout(super::super::support::build_actor()).await;
        let signal = interject_signal(&actor).await;
        let earlier = start_turn(&actor, "turn-1").await;
        let current = start_turn(&actor, "turn-2").await;
        let receipt = admit_interject(&actor, "i1").await;
        signal.note_wait_aborted(current);

        {
            let mut state = await_with_timeout(actor.state.lock()).await;
            let stale = TurnBinding::new("turn-1".to_owned(), earlier);
            actor.transition_parent_messages(
                &mut state,
                TerminalTarget::Turn(&stale),
                TerminalCause::Completion,
            );
        }
        assert!(actor.drain_parent_messages_at_safe_point().await);

        assert_eq!(
            Some(ActiveAgentMessageSafePointTrigger::WaitAbort),
            settled_trigger(receipt)
        );
    }))
    .await;
}
