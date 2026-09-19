// Modified for Distill by Samuel Fajreldines, 2026.
use std::collections::HashSet;

use tokio::sync::{mpsc, oneshot};
use distill_message_delivery_core::{AgentAddress, AgentId, AttemptId};

use super::*;
use crate::implementations::distill::task::admission::{
    Admission, LimitBehavior, SubagentLimits,
};
use crate::implementations::distill::task::agent_message_sender::AgentMessageSender;
use crate::implementations::distill::task::backend::SubagentCoordinatorSender;
use crate::implementations::distill::task::coordinator::active_message::tests::{
    AdmissionCall, TestControl, TestCoordinator, begin_send, finish_child, fixture,
    insert_child_with, insert_pending, promote_pending, recv_with_timeout, release_admission,
    response_outcome,
};
use crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT;
use crate::implementations::distill::task::coordinator::{
    ActiveChildGeneration, ActiveMessageAdmission,
};
use crate::implementations::distill::task::coordinator_state::{
    ChildRunOutput, InternalEvent, StartedChild,
};
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageRequest, SubagentActiveMessageRequest,
    SubagentCancelOutcome, SubagentOwner, SubagentResult,
};

const IDS: [&str; 6] = [
    "019b0000-0000-7000-8000-000000000001",
    "019b0000-0000-7000-8000-000000000002",
    "019b0000-0000-7000-8000-000000000003",
    "019b0000-0000-7000-8000-000000000004",
    "019b0000-0000-7000-8000-000000000005",
    "019b0000-0000-7000-8000-000000000006",
];
fn snapshot(c: &TestCoordinator) -> impl std::fmt::Debug + PartialEq + use<> {
    let active = c
        .active
        .iter()
        .map(|(id, child)| (id.clone(), child.active_messages.is_finalizing()))
        .collect::<HashSet<_>>();
    (
        active,
        c.pending.keys().cloned().collect::<HashSet<_>>(),
        c.completed.keys().cloned().collect::<HashSet<_>>(),
        c.completed_order.clone(),
    )
}
fn add(
    c: &mut TestCoordinator,
    tx: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
    root: &str,
    spawner: Option<&str>,
) {
    insert_child_with(c, tx, id, root, spawner, SubagentOwner::Task);
}
pub(in super::super) fn holder(
    c: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    spawner: Option<&str>,
) -> AgentMessageSender {
    let sender =
        AgentMessageSender::mint_for_child(c.agent_message_sender_factory.as_ref(), IDS[2], true)
            .sender
            .unwrap();
    add(c, admissions, IDS[2], "root", spawner);
    let grant = sender.holder();
    let child = c.active.get_mut(IDS[2]).unwrap();
    child.attempt_id = grant.attempt_id().clone();
    child.generation = grant.generation();
    sender
}
fn begin(
    c: &mut TestCoordinator,
    tx: &SubagentCoordinatorSender,
    sender: &AgentMessageSender,
    target: ActiveMessageTarget,
) -> oneshot::Receiver<ActiveAgentMessageOutcome> {
    let (respond_to, response) = oneshot::channel();
    tx.try_send_active_message(SubagentActiveMessageRequest {
        request: ActiveAgentMessageRequest::try_from_parts(
            target,
            "hello",
            ActiveAgentMessageOperation::Steer,
        )
        .unwrap(),
        sender_context: ActiveMessageSenderContext::GrantedChild {
            holder: sender.holder().clone(),
        },
        respond_to,
    })
    .unwrap();
    let ingress = c
        .active_message_ingress
        .as_mut()
        .unwrap()
        .try_recv()
        .unwrap();
    c.handle_send_active_message(ingress);
    response
}
/// Start `id` with admission deferred, as a host that must still commit the wake does.
fn promote_deferred(
    c: &mut TestCoordinator,
    admissions: mpsc::UnboundedSender<AdmissionCall>,
    id: &str,
) {
    let (respond_to, _response) = oneshot::channel();
    c.handle_internal(InternalEvent::Started {
        subagent_id: id.to_owned(),
        child: StartedChild {
            child_session_id: id.to_owned(),
            persona: None,
            resumed_from: None,
            child_cwd: String::new(),
            worktree_path: None,
            effective_model_id: "test-model".to_owned(),
            definition_background: false,
            control: TestControl { admissions },
        },
        defer_admission: true,
        respond_to,
    });
}
/// The host refused the deferred start; teardown cancels the token too.
fn reject_deferred(c: &mut TestCoordinator, id: &str) {
    let (respond_to, _response) = oneshot::channel();
    c.handle_internal(InternalEvent::SettleDeferredStart {
        subagent_id: id.to_owned(),
        is_committed: false,
        respond_to,
    });
}
fn id(index: usize) -> &'static str {
    IDS.get(index).unwrap()
}

fn agent(index: usize) -> ActiveMessageTarget {
    ActiveMessageTarget::Agent {
        agent_id: AgentId::parse(id(index)).unwrap(),
    }
}

fn target(index: usize, use_address: bool, address: AgentAddress) -> ActiveMessageTarget {
    if use_address {
        ActiveMessageTarget::Address(address)
    } else {
        ActiveMessageTarget::ChildId(id(index).to_owned())
    }
}

#[tokio::test]
async fn target_resolution_table_uses_real_ingress() {
    replay_target_resolution_table().await;
}

pub(in super::super) async fn replay_target_resolution_table() {
    let names = "holder_missing|holder_wrong_session|holder_stale_attempt|holder_stale_generation|holder_workflow|holder_cancelled|holder_killed|holder_finalizing|parent_root|parent_inactive|parent_workflow|parent_cancelled|parent_finalizing|typed_self_send|agent_active|agent_pending|agent_completed|agent_workflow|agent_finalizing|agent_missing|agent_active_cancelled|agent_active_killed|agent_pending_cancelled|agent_pending_killed|agent_completed_killed|agent_completed_cancelled|legacy_completed_killed";
    for (case, name) in names.split('|').enumerate() {
        let (mut c, tx, admissions, mut calls) = fixture();
        if (9..=12).contains(&case) {
            add(&mut c, admissions.clone(), IDS[1], "root", None);
        }
        let sender = holder(
            &mut c,
            admissions.clone(),
            (9..=12).contains(&case).then_some(IDS[1]),
        );
        if matches!(case, 15 | 22 | 23) {
            insert_pending(&mut c, IDS[3], "other");
        } else if matches!(case, 14 | 16..=18 | 20 | 21 | 24..=26) {
            add(&mut c, admissions.clone(), IDS[3], "other", None);
        }
        match case {
            0 => drop(c.active.remove(IDS[2])),
            1 => c.active.get_mut(IDS[2]).unwrap().child_session_id = "wrong".to_owned(),
            2 => c.active.get_mut(IDS[2]).unwrap().attempt_id = AttemptId::mint(9),
            3 => c.active.get_mut(IDS[2]).unwrap().generation = ActiveChildGeneration::new(),
            4 => c.active.get_mut(IDS[2]).unwrap().request.owner = SubagentOwner::workflow("run"),
            5 => c.active.get(IDS[2]).unwrap().cancellation.cancel(),
            6 => c.active.get_mut(IDS[2]).unwrap().explicitly_killed = true,
            7 => {
                let _ = c
                    .active
                    .get_mut(IDS[2])
                    .unwrap()
                    .active_messages
                    .start_terminalizing();
            }
            9 => drop(c.active.remove(IDS[1])),
            10 => c.active.get_mut(IDS[1]).unwrap().request.owner = SubagentOwner::workflow("run"),
            11 => c.active.get(IDS[1]).unwrap().cancellation.cancel(),
            12 => {
                let _ = c
                    .active
                    .get_mut(IDS[1])
                    .unwrap()
                    .active_messages
                    .start_terminalizing();
            }
            16 => finish_child(&mut c, IDS[3]),
            17 => c.active.get_mut(IDS[3]).unwrap().request.owner = SubagentOwner::workflow("run"),
            18 => {
                let _ = c
                    .active
                    .get_mut(IDS[3])
                    .unwrap()
                    .active_messages
                    .start_terminalizing();
            }
            20 => c.active.get(IDS[3]).unwrap().cancellation.cancel(),
            21 => c.active.get_mut(IDS[3]).unwrap().explicitly_killed = true,
            22 => c.pending.get(IDS[3]).unwrap().cancellation.cancel(),
            23 => c.pending.get_mut(IDS[3]).unwrap().explicitly_killed = true,
            24 | 26 => {
                assert!(matches!(
                    c.cancel_one(IDS[3], Some("other"), true),
                    SubagentCancelOutcome::Cancelled
                ));
                finish_child(&mut c, IDS[3]);
            }
            25 => {
                c.active.get(IDS[3]).unwrap().cancellation.cancel();
                finish_child(&mut c, IDS[3]);
            }
            8 | 13..=15 | 19 => {}
            _ => unreachable!("fixed table"),
        }
        let before = snapshot(&c);
        let permits = sender.available_permits();
        let mut response = if case == 26 {
            begin_send(&mut c, &tx, IDS[3], "other")
        } else {
            let target = if case <= 12 {
                ActiveMessageTarget::Parent
            } else if case == 13 {
                agent(2)
            } else {
                agent(3)
            };
            begin(&mut c, &tx, &sender, target)
        };
        match case {
            14 => assert_eq!(
                ActiveMessageRoute::Peer,
                recv_with_timeout(&mut calls).await.late_delivery.route()
            ),
            15 => {
                assert!(response.try_recv().is_err(), "{name}");
                promote_pending(&mut c, admissions, IDS[3]);
                assert_eq!(
                    ActiveMessageRoute::Peer,
                    recv_with_timeout(&mut calls).await.late_delivery.route()
                );
            }
            16 | 26 => {
                assert!(c.pending.contains_key(IDS[3]), "{name}");
                assert!(response.try_recv().is_err(), "{name}");
            }
            _ => {
                let expected = match case {
                    8 | 19 => ActiveAgentMessageOutcome::Unsupported,
                    10 | 13 | 17 => ActiveAgentMessageOutcome::NotFoundOrNotOwned,
                    _ => ActiveAgentMessageOutcome::NotActiveOrFinalizing,
                };
                assert_eq!(expected, response_outcome(response).await, "{name}");
                assert_eq!(permits, sender.available_permits(), "{name}");
                assert_eq!(before, snapshot(&c), "{name}");
                assert!(calls.try_recv().is_err(), "{name}");
            }
        }
    }
}

#[tokio::test]
async fn parked_sends_revalidate_the_holder_at_promotion() {
    for invalidation in "killed|completed|finalizing|replaced_attempt".split('|') {
        let (mut c, tx, admissions, mut calls) = fixture();
        let sender = holder(&mut c, admissions.clone(), None);
        insert_pending(&mut c, IDS[3], "other");
        let permits = sender.available_permits();
        let mut response = begin(&mut c, &tx, &sender, agent(3));
        assert!(response.try_recv().is_err(), "{invalidation}");
        assert_eq!(permits - 1, sender.available_permits(), "{invalidation}");
        match invalidation {
            "killed" => c.active.get_mut(IDS[2]).unwrap().explicitly_killed = true,
            "completed" => finish_child(&mut c, IDS[2]),
            "finalizing" => {
                let _ = c
                    .active
                    .get_mut(IDS[2])
                    .unwrap()
                    .active_messages
                    .start_terminalizing();
            }
            "replaced_attempt" => c.active.get_mut(IDS[2]).unwrap().attempt_id = AttemptId::mint(9),
            _ => unreachable!("fixed table"),
        }
        promote_pending(&mut c, admissions, IDS[3]);
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(response).await,
            "{invalidation}"
        );
        assert!(c.active.contains_key(IDS[3]), "{invalidation}");
        assert_eq!(permits, sender.available_permits(), "{invalidation}");
        assert!(calls.try_recv().is_err(), "{invalidation}");
    }
}

#[tokio::test]
async fn pre_start_wake_rollback_keeps_the_activation_disposition() {
    let exits =
        "killed|cancelled|queued_cancelled|setup_failure|deferred_rejected|deferred_cancelled";
    for exit in exits.split('|') {
        let (mut c, tx, admissions, mut calls) = fixture();
        let sender = holder(&mut c, admissions.clone(), None);
        add(&mut c, admissions.clone(), IDS[3], "other", None);
        finish_child(&mut c, IDS[3]);
        if exit == "queued_cancelled" {
            c.admission = Admission::new(SubagentLimits {
                max_concurrent: 1,
                behavior: LimitBehavior::Queue,
            });
            add(&mut c, admissions.clone(), IDS[4], "other", None);
        }
        let mut wake = begin(&mut c, &tx, &sender, agent(3));
        assert!(wake.try_recv().is_err(), "{exit}");
        let output = |result: SubagentResult| ChildRunOutput {
            result,
            completion_data: (),
            snapshot_ref: None,
        };
        match exit {
            "killed" | "cancelled" | "deferred_cancelled" | "deferred_rejected" => {
                assert!(c.pending.contains_key(IDS[3]), "{exit}");
                if exit.starts_with("deferred") {
                    promote_deferred(&mut c, admissions.clone(), IDS[3]);
                }
                if exit == "deferred_rejected" {
                    reject_deferred(&mut c, IDS[3]);
                } else {
                    assert!(matches!(
                        c.cancel_one(IDS[3], Some("other"), exit == "killed"),
                        SubagentCancelOutcome::Cancelled
                    ));
                }
                let result = SubagentResult::cancelled(IDS[3].to_owned(), IDS[3].to_owned(), "");
                c.finish_child(IDS[3], output(result));
            }
            "queued_cancelled" => {
                assert!(c.queued.contains_id(IDS[3]), "{exit}");
                assert!(matches!(
                    c.cancel_one(IDS[3], Some("other"), true),
                    SubagentCancelOutcome::Cancelled
                ));
            }
            "setup_failure" => {
                let result = SubagentResult::failed(IDS[3].to_owned(), IDS[3].to_owned(), "");
                c.finish_child(IDS[3], output(result));
            }
            _ => unreachable!("fixed table"),
        }
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(wake).await,
            "{exit}"
        );
        assert!(c.completed.contains_key(IDS[3]), "{exit}");
        let mut retry = begin(&mut c, &tx, &sender, agent(3));
        if exit == "setup_failure" || exit == "deferred_rejected" {
            assert!(retry.try_recv().is_err(), "{exit}");
            assert!(c.pending.contains_key(IDS[3]), "{exit}");
        } else {
            assert_eq!(
                ActiveAgentMessageOutcome::NotActiveOrFinalizing,
                response_outcome(retry).await,
                "{exit}"
            );
            assert!(!c.pending.contains_key(IDS[3]), "{exit}");
        }
        assert!(calls.try_recv().is_err(), "{exit}");
    }
}

#[tokio::test]
async fn granted_legacy_targets_refuse_killed_and_cancelled_completed_children() {
    for (is_killed, use_address) in [(true, false), (false, false), (true, true), (false, true)] {
        let (mut c, tx, admissions, mut calls) = fixture();
        let sender = holder(&mut c, admissions.clone(), None);
        add(&mut c, admissions, IDS[3], IDS[2], None);
        let address = c.active.get(IDS[3]).unwrap().agent_address.clone().unwrap();
        if is_killed {
            c.active.get_mut(IDS[3]).unwrap().explicitly_killed = true;
        } else {
            c.active.get(IDS[3]).unwrap().cancellation.cancel();
        }
        finish_child(&mut c, IDS[3]);
        let target = if use_address {
            ActiveMessageTarget::Address(address)
        } else {
            ActiveMessageTarget::ChildId(IDS[3].to_owned())
        };
        let before = snapshot(&c);
        assert_eq!(
            ActiveAgentMessageOutcome::NotActiveOrFinalizing,
            response_outcome(begin(&mut c, &tx, &sender, target)).await
        );
        assert_eq!((before, true), (snapshot(&c), calls.try_recv().is_err()));
    }
}

#[tokio::test]
async fn root_and_granted_wakes_reenter_resolution_after_publication() {
    replay_root_and_granted_wakes().await;
}

pub(in super::super) async fn replay_root_and_granted_wakes() {
    for granted in [false, true] {
        let (mut c, tx, admissions, mut calls) = fixture();
        let sender = granted.then(|| holder(&mut c, admissions.clone(), None));
        add(&mut c, admissions.clone(), IDS[3], "other", None);
        finish_child(&mut c, IDS[3]);
        c.completed
            .get(IDS[3])
            .unwrap()
            .terminal_published
            .store(false, std::sync::atomic::Ordering::Release);
        let mut send = || match sender.as_ref() {
            Some(sender) => begin(&mut c, &tx, sender, agent(3)),
            None => begin_send(&mut c, &tx, IDS[3], "other"),
        };
        let responses = [send(), send()];
        assert_eq!(c.pending_wakes.get(IDS[3]).unwrap().len(), 2);
        c.completed
            .get(IDS[3])
            .unwrap()
            .terminal_published
            .store(true, std::sync::atomic::Ordering::Release);
        c.handle_terminal_published(IDS[3].to_owned());
        let pending_id = c
            .pending
            .keys()
            .find(|id| id.as_str() != IDS[2])
            .cloned()
            .expect("wake pending");
        promote_pending(&mut c, admissions, &pending_id);
        let [first, second] = responses;
        assert!(matches!(
            response_outcome(first).await,
            ActiveAgentMessageOutcome::Accepted { .. }
        ));
        let call = recv_with_timeout(&mut calls).await;
        release_admission(&mut c, call, ActiveMessageAdmission::Admitted).await;
        assert!(matches!(
            response_outcome(second).await,
            ActiveAgentMessageOutcome::Accepted { .. }
        ));
    }
}

#[tokio::test]
async fn granted_legacy_wakes_keep_one_pair_incarnation() {
    for use_address in [false, true] {
        let (mut c, tx, admissions, _calls) = fixture();
        let sender = holder(&mut c, admissions.clone(), None);
        add(&mut c, admissions, IDS[3], IDS[2], None);
        let address = c.active.get(IDS[3]).unwrap().agent_address.clone().unwrap();
        finish_child(&mut c, IDS[3]);
        c.completed
            .get(IDS[3])
            .unwrap()
            .terminal_published
            .store(false, std::sync::atomic::Ordering::Release);

        let mut responses = vec![
            begin(
                &mut c,
                &tx,
                &sender,
                target(3, use_address, address.clone()),
            ),
            begin(
                &mut c,
                &tx,
                &sender,
                target(3, use_address, address.clone()),
            ),
        ];
        assert_eq!(vec![2], c.agent_message_quotas.pair_in_flight_counts());
        c.completed
            .get(IDS[3])
            .unwrap()
            .terminal_published
            .store(true, std::sync::atomic::Ordering::Release);
        c.handle_terminal_published(IDS[3].to_owned());
        assert_eq!(vec![2], c.agent_message_quotas.pair_in_flight_counts());
        responses.extend([
            begin(
                &mut c,
                &tx,
                &sender,
                target(3, use_address, address.clone()),
            ),
            begin(
                &mut c,
                &tx,
                &sender,
                target(3, use_address, address.clone()),
            ),
        ]);
        assert_eq!(
            ActiveAgentMessageOutcome::QuotaExceeded {
                kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
                limit: crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT,
            },
            response_outcome(begin(&mut c, &tx, &sender, target(3, use_address, address))).await,
        );
        drop(responses);
    }
}

#[tokio::test]
async fn root_wake_replay_keeps_the_incarnation_granted_senders_hold() {
    let (mut c, tx, admissions, _calls) = fixture();
    let sender = holder(&mut c, admissions.clone(), None);
    // A spawn that failed before start leaves an eligible record that never had an incarnation.
    insert_pending(&mut c, IDS[3], "other");
    let failed = SubagentResult::failed(IDS[3].to_owned(), IDS[3].to_owned(), "");
    c.finish_child(
        IDS[3],
        ChildRunOutput {
            result: failed,
            completion_data: (),
            snapshot_ref: None,
        },
    );
    assert!(c.completed.get(IDS[3]).unwrap().wake_eligible);
    c.completed
        .get(IDS[3])
        .unwrap()
        .terminal_published
        .store(false, std::sync::atomic::Ordering::Release);
    let root = begin_send(&mut c, &tx, IDS[3], "other");
    let granted = (0..MAX_SENDER_TARGET_IN_FLIGHT)
        .map(|_| begin(&mut c, &tx, &sender, agent(3)))
        .collect::<Vec<_>>();
    assert_eq!(
        vec![MAX_SENDER_TARGET_IN_FLIGHT],
        c.agent_message_quotas.pair_in_flight_counts()
    );
    c.completed
        .get(IDS[3])
        .unwrap()
        .terminal_published
        .store(true, std::sync::atomic::Ordering::Release);
    c.handle_terminal_published(IDS[3].to_owned());
    assert!(c.pending.contains_key(IDS[3]));
    assert_eq!(
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
            limit: MAX_SENDER_TARGET_IN_FLIGHT,
        },
        response_outcome(begin(&mut c, &tx, &sender, agent(3))).await,
    );
    drop((root, granted));
}

#[tokio::test]
async fn abandoned_incarnations_release_their_pair_permits() {
    let quota = |c: &TestCoordinator| {
        let mut counts = c.agent_message_quotas.pair_in_flight_counts();
        counts.sort_unstable();
        counts
    };
    for exit in "deferred_rejected|pending_cancelled|completed".split('|') {
        let (mut c, tx, admissions, _calls) = fixture();
        let sender = holder(&mut c, admissions.clone(), None);
        add(&mut c, admissions.clone(), IDS[3], "other", None);
        let mut held = Vec::new();
        if exit == "completed" {
            // Admitted onto the live incarnation; their futures have not settled when it completes.
            held.extend((0..2).map(|_| begin(&mut c, &tx, &sender, agent(3))));
        } else {
            finish_child(&mut c, IDS[3]);
            held.push(begin(&mut c, &tx, &sender, agent(3)));
            if exit == "deferred_rejected" {
                promote_deferred(&mut c, admissions.clone(), IDS[3]);
            }
            held.extend((0..2).map(|_| begin(&mut c, &tx, &sender, agent(3))));
            assert_eq!(vec![3], quota(&c), "{exit}");
            if exit == "deferred_rejected" {
                reject_deferred(&mut c, IDS[3]);
            } else {
                c.cancel_one(IDS[3], Some("other"), false);
            }
        }
        let result = if exit == "completed" {
            SubagentResult::failed(IDS[3].to_owned(), IDS[3].to_owned(), "")
        } else {
            SubagentResult::cancelled(IDS[3].to_owned(), IDS[3].to_owned(), "")
        };
        c.finish_child(
            IDS[3],
            ChildRunOutput {
                result,
                completion_data: (),
                snapshot_ref: None,
            },
        );
        assert!(c.completed.contains_key(IDS[3]), "{exit}");
        // A user cancel withdraws granted re-wakes, so the root wakes that one (taking no permit).
        let granted_wakes = if exit == "pending_cancelled" {
            held.push(begin_send(&mut c, &tx, IDS[3], "other"));
            0
        } else {
            held.push(begin(&mut c, &tx, &sender, agent(3)));
            1
        };
        assert!(c.pending.contains_key(IDS[3]), "{exit}");
        for _ in granted_wakes..MAX_SENDER_TARGET_IN_FLIGHT {
            let mut parked = begin(&mut c, &tx, &sender, agent(3));
            assert!(parked.try_recv().is_err(), "{exit}: within the cap");
            held.push(parked);
        }
        assert_eq!(
            vec![MAX_SENDER_TARGET_IN_FLIGHT],
            quota(&c),
            "{exit}: only the live incarnation's permits count"
        );
        assert_eq!(
            ActiveAgentMessageOutcome::QuotaExceeded {
                kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
                limit: MAX_SENDER_TARGET_IN_FLIGHT,
            },
            response_outcome(begin(&mut c, &tx, &sender, agent(3))).await,
            "{exit}"
        );
    }
}

#[tokio::test]
async fn pair_quota_releases_and_attempt_quota_does_not_refund() {
    let (mut c, tx, admissions, mut calls) = fixture();
    let sender = holder(&mut c, admissions.clone(), None);
    add(&mut c, admissions, IDS[3], "other", None);

    let mut held = Vec::new();
    for _ in 0..crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT {
        let response = begin(&mut c, &tx, &sender, agent(3));
        held.push((response, recv_with_timeout(&mut calls).await));
    }
    assert_eq!(
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
            limit: crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT,
        },
        response_outcome(begin(&mut c, &tx, &sender, agent(3))).await
    );
    release_admission(
        &mut c,
        held.pop().unwrap().1,
        crate::implementations::distill::task::coordinator::ActiveMessageAdmission::Rejected,
    )
    .await;
    let mut released = begin(&mut c, &tx, &sender, agent(3));
    let released_call = recv_with_timeout(&mut calls).await;
    assert!(released.try_recv().is_err());
    release_admission(
        &mut c,
        released_call,
        crate::implementations::distill::task::coordinator::ActiveMessageAdmission::Rejected,
    )
    .await;
    assert_eq!(
        ActiveAgentMessageOutcome::NotActiveOrFinalizing,
        response_outcome(released).await
    );

    for _ in 0..(crate::implementations::distill::task::coordinator::agent_quotas::MAX_ATTEMPT_OUTBOUND
        - crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT
        - 2)
    {
        assert_eq!(
            ActiveAgentMessageOutcome::Unsupported,
            response_outcome(begin(&mut c, &tx, &sender, ActiveMessageTarget::Parent)).await
        );
    }
    assert_eq!(
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::AttemptOutbound,
            limit: crate::implementations::distill::task::coordinator::agent_quotas::MAX_ATTEMPT_OUTBOUND,
        },
        response_outcome(begin(&mut c, &tx, &sender, ActiveMessageTarget::Parent)).await
    );
}

#[tokio::test]
async fn target_transitions_and_drop_preserve_pair_quota() {
    let (mut c, tx, admissions, mut calls) = fixture();
    let sender = holder(&mut c, admissions.clone(), None);
    insert_pending(&mut c, IDS[3], "other");
    let mut responses = Vec::new();
    for _ in 0..crate::implementations::distill::task::coordinator::agent_quotas::MAX_SENDER_TARGET_IN_FLIGHT {
        responses.push(begin(&mut c, &tx, &sender, agent(3)));
    }
    promote_pending(&mut c, admissions.clone(), IDS[3]);
    let mut held = Vec::new();
    for _ in 0..responses.len() {
        held.push(recv_with_timeout(&mut calls).await);
    }
    assert!(matches!(
        response_outcome(begin(&mut c, &tx, &sender, agent(3))).await,
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
            ..
        }
    ));
    drop(held);
    c.active_messages.clear();
    c.graph.remove(IDS[3]);
    add(&mut c, admissions.clone(), IDS[3], "other", None);
    finish_child(&mut c, IDS[3]);
    c.completed
        .get(IDS[3])
        .unwrap()
        .terminal_published
        .store(false, std::sync::atomic::Ordering::Release);
    let mut wake = begin(&mut c, &tx, &sender, agent(3));
    assert!(wake.try_recv().is_err());
    promote_pending(&mut c, admissions.clone(), IDS[3]);
    for _ in 0..3 {
        let mut response = begin(&mut c, &tx, &sender, agent(3));
        assert!(response.try_recv().is_err());
    }
    assert!(matches!(
        response_outcome(begin(&mut c, &tx, &sender, agent(3))).await,
        ActiveAgentMessageOutcome::QuotaExceeded {
            kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::SenderTargetInFlight,
            ..
        }
    ));
}

#[tokio::test]
async fn resolved_routes_cover_lineage_and_peers() {
    replay_resolved_routes().await;
}

pub(in super::super) async fn replay_resolved_routes() {
    let (mut c, tx, admissions, mut calls) = fixture();
    add(&mut c, admissions.clone(), IDS[0], "root", None);
    add(&mut c, admissions.clone(), IDS[1], "root", Some(IDS[0]));
    let sender = holder(&mut c, admissions.clone(), Some(IDS[1]));
    add(&mut c, admissions.clone(), IDS[3], "root", Some(IDS[2]));
    add(&mut c, admissions.clone(), IDS[4], "root", Some(IDS[1]));
    add(&mut c, admissions, IDS[5], "other", None);
    let targets = [
        ActiveMessageTarget::Parent,
        agent(0),
        agent(3),
        agent(4),
        agent(5),
    ];
    let routes = [
        ActiveMessageRoute::DescendantToParent,
        ActiveMessageRoute::DescendantToParent,
        ActiveMessageRoute::ParentToOwnedDescendant,
        ActiveMessageRoute::Peer,
        ActiveMessageRoute::Peer,
    ];
    for (target, expected) in targets.into_iter().zip(routes) {
        let _response = begin(&mut c, &tx, &sender, target);
        assert_eq!(
            expected,
            recv_with_timeout(&mut calls).await.late_delivery.route()
        );
    }
}
