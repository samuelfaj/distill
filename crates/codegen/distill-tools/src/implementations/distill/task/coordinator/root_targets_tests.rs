// Modified for Distill by Samuel Fajreldines, 2026.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::mpsc;
use distill_message_delivery_core::{AgentId, AttemptId};

use super::*;
use crate::implementations::distill::task::coordinator::active_message::tests::{
    TestCoordinator, finish_child, fixture, insert_child_with, insert_pending, promote_pending,
    recv_with_timeout, release_admission, response_outcome,
};
use crate::implementations::distill::task::coordinator::agent_targets::tests::holder;
use crate::implementations::distill::task::coordinator::{
    ActiveMessageAdmission, SendBoxFuture,
};
use crate::implementations::distill::task::root_control::{
    RootReceiptSink, UserInputGeneration,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageRequest, ActiveMessageSenderContext,
    ActiveMessageTarget, SubagentActiveMessageRequest, SubagentOwner,
};

const ROOT_A: &str = "019b0000-0000-7000-8000-000000000011";
const ROOT_B: &str = "019b0000-0000-7000-8000-000000000012";
const CHILD: &str = "019b0000-0000-7000-8000-000000000003";

thread_local! {
    static ROOTS: RefCell<HashMap<AgentId, TestRootControl>> = RefCell::new(HashMap::new());
}

#[derive(Clone)]
pub(in super::super) struct TestRootControl {
    agent_id: AgentId,
    session_id: &'static str,
    attempt_id: AttemptId,
    generation: AgentMessageGeneration,
    is_receipt_closed: bool,
    routes: mpsc::UnboundedSender<ActiveMessageRoute>,
}

impl RootReceiptSink for bool {
    fn is_closed(&self) -> bool {
        *self
    }
}

impl RootControl for TestRootControl {
    type ReceiptSink = bool;

    fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }
    fn session_id(&self) -> &str {
        self.session_id
    }
    fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }
    fn generation(&self) -> AgentMessageGeneration {
        self.generation
    }
    fn user_input_generation(&self) -> UserInputGeneration {
        UserInputGeneration::new(7)
    }
    fn label(&self) -> &str {
        self.session_id
    }
    fn receipt_sink(&self) -> bool {
        self.is_receipt_closed
    }
    fn deliver(
        &self,
        delivery: ActiveAgentMessageDelivery,
    ) -> SendBoxFuture<ActiveMessageAdmission> {
        let routes = self.routes.clone();
        Box::pin(async move {
            let admitted = delivery
                .commit_admission(|| routes.send(delivery.route()))
                .is_some_and(|result| result.is_ok());
            if admitted {
                ActiveMessageAdmission::Admitted
            } else {
                ActiveMessageAdmission::Rejected
            }
        })
    }
}

pub(in super::super) fn resolve_root(id: &AgentId) -> Option<TestRootControl> {
    ROOTS.with_borrow(|roots| roots.get(id).cloned())
}

pub(in super::super) fn resolve_root_session(session_id: &str) -> Option<TestRootControl> {
    ROOTS.with_borrow(|roots| {
        roots
            .values()
            .find(|root| root.session_id == session_id)
            .cloned()
    })
}

fn install_roots(routes: mpsc::UnboundedSender<ActiveMessageRoute>) {
    ROOTS.with_borrow_mut(|roots| {
        *roots = [(ROOT_A, "root-a"), (ROOT_B, "root-b")]
            .map(|(id, session_id)| {
                let agent_id = AgentId::parse(id).expect("root id");
                (
                    agent_id.clone(),
                    TestRootControl {
                        agent_id,
                        session_id,
                        attempt_id: AttemptId::mint(1),
                        generation: AgentMessageGeneration::mint(1),
                        is_receipt_closed: false,
                        routes: routes.clone(),
                    },
                )
            })
            .into();
    });
}

fn agent(id: &str) -> ActiveMessageTarget {
    ActiveMessageTarget::Agent {
        agent_id: AgentId::parse(id).expect("agent id"),
    }
}

fn begin(
    coordinator: &mut TestCoordinator,
    tx: &crate::implementations::distill::task::backend::SubagentCoordinatorSender,
    target: ActiveMessageTarget,
    sender_context: ActiveMessageSenderContext,
) -> tokio::sync::oneshot::Receiver<ActiveAgentMessageOutcome> {
    let (respond_to, response) = tokio::sync::oneshot::channel();
    tx.try_send_active_message(SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_from_parts(
            target,
            "hello",
            ActiveAgentMessageOperation::Steer,
        )
        .expect("request"),
        sender_context,
        respond_to,
    })
    .expect("ingress open");
    let ingress = coordinator
        .active_message_ingress
        .as_mut()
        .expect("paired ingress")
        .try_recv()
        .expect("request queued");
    coordinator.handle_send_active_message(ingress);
    response
}

async fn finish(
    coordinator: &mut TestCoordinator,
    routes: &mut mpsc::UnboundedReceiver<ActiveMessageRoute>,
    response: tokio::sync::oneshot::Receiver<ActiveAgentMessageOutcome>,
) -> ActiveMessageRoute {
    let completion = coordinator
        .active_messages
        .next()
        .await
        .expect("completion");
    let route = routes.recv().await.expect("root admission");
    coordinator.finish_active_message(completion);
    assert!(matches!(
        response.await.expect("response"),
        ActiveAgentMessageOutcome::Accepted { .. }
    ));
    route
}

#[tokio::test(flavor = "current_thread")]
async fn root_resolution_routes_and_completion_are_exact() {
    let (mut coordinator, tx, admission_tx, _admissions) = fixture();
    let (routes_tx, mut routes) = mpsc::unbounded_channel();
    install_roots(routes_tx);
    let sender = holder(&mut coordinator, admission_tx, None);
    coordinator.graph.remove(CHILD);
    coordinator.graph.insert_root_child(CHILD, "root-a");
    let granted = ActiveMessageSenderContext::GrantedChild {
        holder: sender.holder().clone(),
    };
    for (index, (target, context, route)) in [
        (
            ActiveMessageTarget::Parent,
            granted.clone(),
            ActiveMessageRoute::DescendantToParent,
        ),
        (
            agent(ROOT_A),
            granted.clone(),
            ActiveMessageRoute::DescendantToParent,
        ),
        (agent(ROOT_B), granted, ActiveMessageRoute::Peer),
        (
            agent(ROOT_B),
            ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("root-a"),
            },
            ActiveMessageRoute::Peer,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let response = begin(&mut coordinator, &tx, target, context);
        assert!(!coordinator.active_messages.is_empty(), "case {index}");
        assert_eq!(route, finish(&mut coordinator, &mut routes, response).await);
    }
    assert!(coordinator.root_active_messages.is_empty());
    coordinator.terminal_outputs.insert(
        CHILD.to_owned(),
        crate::implementations::distill::task::coordinator::ChildRunOutput {
            result: crate::implementations::distill::task::types::SubagentResult {
                success: true,
                subagent_id: CHILD.to_owned(),
                output: "sentinel".into(),
                ..Default::default()
            },
            completion_data: (),
            snapshot_ref: None,
        },
    );
    let response = begin(
        &mut coordinator,
        &tx,
        agent(ROOT_B),
        ActiveMessageSenderContext::RootSession {
            session_id: Arc::from("root-a"),
        },
    );
    let _ = finish(&mut coordinator, &mut routes, response).await;
    assert!(coordinator.terminal_outputs.contains_key(CHILD));
    for (target, context) in [
        (
            agent(CHILD),
            ActiveMessageSenderContext::GrantedChild {
                holder: sender.holder().clone(),
            },
        ),
        (
            agent(ROOT_A),
            ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("root-a"),
            },
        ),
    ] {
        assert_eq!(
            ActiveAgentMessageOutcome::NotFoundOrNotOwned,
            begin(&mut coordinator, &tx, target, context)
                .await
                .expect("self response"),
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn closed_receipt_fails_before_delivery() {
    let (mut coordinator, tx, _admission_tx, _admissions) = fixture();
    let (routes_tx, mut routes) = mpsc::unbounded_channel();
    install_roots(routes_tx);
    ROOTS.with_borrow_mut(|roots| {
        roots
            .get_mut(&AgentId::parse(ROOT_A).expect("root id"))
            .expect("root")
            .is_receipt_closed = true;
    });
    assert_eq!(
        ActiveAgentMessageOutcome::ChannelClosed,
        begin(
            &mut coordinator,
            &tx,
            agent(ROOT_A),
            ActiveMessageSenderContext::RootSession {
                session_id: Arc::from("root-b")
            },
        )
        .await
        .expect("closed response"),
    );
    assert!(routes.try_recv().is_err());
    assert!(coordinator.root_active_messages.is_empty());
    ROOTS.with_borrow_mut(|roots| {
        roots
            .get_mut(&AgentId::parse(ROOT_A).expect("root id"))
            .expect("root")
            .is_receipt_closed = false;
    });
    let response = begin(
        &mut coordinator,
        &tx,
        agent(ROOT_A),
        ActiveMessageSenderContext::RootSession {
            session_id: Arc::from("root-b"),
        },
    );
    assert_eq!(
        ActiveMessageRoute::Peer,
        finish(&mut coordinator, &mut routes, response).await,
    );
}

#[tokio::test(flavor = "current_thread")]
async fn peer_root_reaches_another_roots_child_in_every_state() {
    for state in "active|pending|completed|completed_unpublished|completed_killed".split('|') {
        let (mut c, tx, admission_tx, mut admissions) = fixture();
        let (routes_tx, _routes) = mpsc::unbounded_channel();
        install_roots(routes_tx);
        if state == "pending" {
            insert_pending(&mut c, CHILD, "root-b");
        } else {
            insert_child_with(
                &mut c,
                admission_tx.clone(),
                CHILD,
                "root-b",
                None,
                SubagentOwner::Task,
            );
        }
        if state == "completed_killed" {
            c.active.get_mut(CHILD).unwrap().explicitly_killed = true;
        }
        if state.starts_with("completed") {
            finish_child(&mut c, CHILD);
        }
        if state == "completed_unpublished" {
            c.completed
                .get(CHILD)
                .unwrap()
                .terminal_published
                .store(false, std::sync::atomic::Ordering::Release);
        }
        let sender = ActiveMessageSenderContext::RootSession {
            session_id: Arc::from("root-a"),
        };
        let mut response = begin(&mut c, &tx, agent(CHILD), sender.clone());
        if state == "completed_killed" {
            assert_eq!(
                ActiveAgentMessageOutcome::NotActiveOrFinalizing,
                response_outcome(response).await
            );
            assert!(!c.pending.contains_key(CHILD) && admissions.try_recv().is_err());
            continue;
        }
        assert!(response.try_recv().is_err(), "{state}");
        if state == "completed_unpublished" {
            assert_eq!(1, c.pending_wakes.get(CHILD).unwrap().len(), "{state}");
            c.completed
                .get(CHILD)
                .unwrap()
                .terminal_published
                .store(true, std::sync::atomic::Ordering::Release);
            c.handle_terminal_published(CHILD.to_owned());
        }
        // A wake folds its first send into the prompt, so a second send exposes the route.
        let wake = state.starts_with("completed").then(|| {
            assert!(c.pending.contains_key(CHILD), "{state}");
            std::mem::replace(&mut response, begin(&mut c, &tx, agent(CHILD), sender))
        });
        if state != "active" {
            promote_pending(&mut c, admission_tx, CHILD);
        }
        let call = recv_with_timeout(&mut admissions).await;
        assert_eq!(
            (ActiveMessageRoute::Peer, "root-a"),
            (
                call.late_delivery.route(),
                call.late_delivery.message().sender_session_id.as_str(),
            ),
            "{state}"
        );
        release_admission(&mut c, call, ActiveMessageAdmission::Admitted).await;
        for response in wake.into_iter().chain([response]) {
            assert!(
                matches!(
                    response_outcome(response).await,
                    ActiveAgentMessageOutcome::Accepted { .. }
                ),
                "{state}"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn default_root_seam_replays_existing_park_wake_and_promotion_paths() {
    ROOTS.with_borrow_mut(HashMap::clear);
    super::super::agent_targets::tests::replay_root_and_granted_wakes().await;
    super::super::active_message::tests::replay_owned_queued_child_promotion().await;
    super::super::agent_targets::tests::replay_target_resolution_table().await;
    super::super::agent_targets::tests::replay_resolved_routes().await;
}
