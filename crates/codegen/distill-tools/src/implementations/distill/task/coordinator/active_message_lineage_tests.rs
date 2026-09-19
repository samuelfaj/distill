// Modified for Distill by Samuel Fajreldines, 2026.
use tokio::sync::mpsc;

use super::tests::{
    AdmissionCall, TestCoordinator, begin_human_send, begin_send, finish_child, fixture,
    insert_child_with, recv_with_timeout, release_admission, response_outcome,
};
use super::*;
use crate::implementations::distill::task::coordinator_state::InternalEvent;
use crate::implementations::distill::task::types::SubagentOwner;

fn insert_grandchild(
    coordinator: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
) -> String {
    insert_child_with(
        coordinator,
        admissions,
        "grandchild",
        "root",
        Some("child"),
        SubagentOwner::Task,
    )
}

#[tokio::test]
async fn nested_spawner_address_send_to_grandchild_is_admitted_and_settles() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let address = insert_grandchild(&mut coordinator, admission_tx);
    let response = begin_human_send(&mut coordinator, &command_tx, &address, "child");
    let call = recv_with_timeout(&mut admissions).await;
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert!(matches!(
        response_outcome(response).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));

    // A lost completion would leave `in_flight` at one and park the output.
    finish_child(&mut coordinator, "grandchild");
    assert!(coordinator.completed.contains_key("grandchild"));
    assert!(!coordinator.terminal_outputs.contains_key("grandchild"));
}

#[tokio::test]
async fn nested_spawner_raw_id_send_is_admitted_foreign_is_rejected() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    insert_grandchild(&mut coordinator, admission_tx);
    for sender in ["child", "root"] {
        let response = begin_send(&mut coordinator, &command_tx, "grandchild", sender);
        let call = recv_with_timeout(&mut admissions).await;
        let message_id = call.message_id.clone();
        release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
        assert_eq!(
            ActiveAgentMessageOutcome::Accepted { message_id },
            response_outcome(response).await,
            "sender {sender}"
        );
    }

    let foreign = begin_send(&mut coordinator, &command_tx, "grandchild", "foreign");
    assert_eq!(
        ActiveAgentMessageOutcome::NotFoundOrNotOwned,
        response_outcome(foreign).await
    );
    assert!(admissions.try_recv().is_err());
}

#[tokio::test]
async fn dropped_advertise_does_not_affect_model_send_ownership() {
    let (mut coordinator, command_tx, admission_tx, mut admissions) = fixture();
    let address = insert_grandchild(&mut coordinator, admission_tx);
    assert_eq!(
        coordinator.graph.advertise_target("grandchild"),
        Some("child")
    );
    coordinator.handle_internal(InternalEvent::DropSpawnerClaim {
        subagent_id: "grandchild".to_owned(),
    });
    assert!(coordinator.graph.advertise_target("grandchild").is_none());
    let response = begin_human_send(&mut coordinator, &command_tx, &address, "child");
    let call = recv_with_timeout(&mut admissions).await;
    release_admission(&mut coordinator, call, ActiveMessageAdmission::Admitted).await;
    assert!(matches!(
        response_outcome(response).await,
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
}
