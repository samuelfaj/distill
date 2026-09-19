// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use tokio::sync::mpsc;

use super::*;
use crate::implementations::distill::task::active_message::{
    ActiveAgentMessageOperation, ActiveAgentMessageOutcome, ActiveAgentMessageQuotaKind,
    ActiveAgentMessageRequest, ActiveMessageTarget,
};

fn sender_fixture() -> (
    AgentMessageSender,
    mpsc::UnboundedReceiver<ActiveMessageIngress>,
    Arc<tokio::sync::Semaphore>,
) {
    let (active_message_tx, active_message_rx) = mpsc::unbounded_channel();
    let permits = Arc::new(tokio::sync::Semaphore::new(4));
    let factory =
        AgentMessageSenderFactory::new(active_message_tx.downgrade(), Arc::clone(&permits), 4);
    let child_id = uuid::Uuid::now_v7().to_string();
    let identity = AgentMessageSender::mint_for_child(Some(&factory), &child_id, true);
    let sender = identity.sender.expect("valid child receives sender");
    assert_eq!(&identity.attempt_id, sender.holder().attempt_id());
    assert_eq!(identity.generation, sender.holder().generation());
    (sender, active_message_rx, permits)
}

#[test]
fn cloned_sender_shares_local_budget() {
    let (sender, _ingress, _permits) = sender_fixture();
    let cloned = sender.clone();
    sender
        .local_outbound_budget
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        1,
        cloned
            .local_outbound_budget
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[tokio::test]
async fn local_outbound_budget_accepts_32_and_rejects_33() {
    let (sender, mut ingress, permits) = sender_fixture();
    let held = permits.clone().acquire_many_owned(4).await.unwrap();
    for _ in 0..32 {
        assert!(matches!(
            sender
                .send(ActiveAgentMessageRequest::try_new("child", "saturated").unwrap())
                .await,
            ActiveAgentMessageOutcome::Saturated { .. }
        ));
    }
    assert_eq!(
        0,
        sender
            .local_outbound_budget
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    drop(held);
    for index in 0..32 {
        let send = tokio::spawn({
            let sender = sender.clone();
            async move {
                sender
                    .send(
                        ActiveAgentMessageRequest::try_new("child", format!("message {index}"))
                            .unwrap(),
                    )
                    .await
            }
        });
        let queued = ingress.recv().await.unwrap();
        queued
            .request
            .respond_to
            .send(ActiveAgentMessageOutcome::Unsupported)
            .unwrap();
        assert_eq!(ActiveAgentMessageOutcome::Unsupported, send.await.unwrap());
    }
    assert_eq!(
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: ActiveAgentMessageQuotaKind::AttemptOutbound,
            limit: 32,
        },
        sender
            .send(ActiveAgentMessageRequest::try_new("child", "overflow").unwrap())
            .await
    );
    assert!(ingress.try_recv().is_err());
}

#[tokio::test]
async fn typed_targets_enter_granted_ingress() {
    let (sender, mut ingress, permits) = sender_fixture();
    for target in [
        ActiveMessageTarget::Parent,
        ActiveMessageTarget::Agent {
            agent_id: distill_message_delivery_core::AgentId::mint(7),
        },
    ] {
        let request = ActiveAgentMessageRequest::try_from_parts(
            target,
            "follow up",
            ActiveAgentMessageOperation::Queue,
        )
        .unwrap();
        let send = tokio::spawn({
            let sender = sender.clone();
            async move { sender.send(request).await }
        });
        let queued = ingress.recv().await.unwrap();
        assert_eq!(3, permits.available_permits());
        let ActiveMessageIngress { request, permit } = queued;
        request
            .respond_to
            .send(ActiveAgentMessageOutcome::Unsupported)
            .unwrap();
        drop(permit);
        assert_eq!(ActiveAgentMessageOutcome::Unsupported, send.await.unwrap());
        assert_eq!(4, permits.available_permits());
    }
}
