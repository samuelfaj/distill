// Modified for Distill by Samuel Fajreldines, 2026.
//! `UserPromptSubmit` prompt-gate enforcement.
//!
//! These drive the SHIPPED turn path (`handle_prompt` calling `handle_turn_input`) with a real on-disk hook registry.
//! A blocking hook must cancel the turn as `HookDenied` before the sampler runs.
//! A synthetic origin must stay observe-only.
//! A hook-denied completion must park the queue until the user re-engages.

use super::support::*;
use super::*;

use tokio::sync::mpsc;

fn prompt_gate_registry(script: &str) -> distill_hooks::discovery::HookRegistry {
    let (mut registry, _) = distill_hooks::discovery::load_hooks(None, None);
    registry.append_specs(vec![distill_hooks::config::HookSpec {
        name: "test/promptgate".into(),
        event: distill_hooks::event::HookEventName::UserPromptSubmit,
        handler_type: distill_hooks::config::HandlerType::Command,
        configured_matcher: None,
        matcher: None,
        enabled: true,
        command: Some(std::path::PathBuf::from(script)),
        command_raw: Some(script.to_string()),
        url: None,
        url_raw: None,
        timeout_ms: 5000,
        source_dir: std::path::PathBuf::from("/tmp"),
        extra_env: std::collections::HashMap::new(),
        layer: distill_hooks::config::HookProvenance::File,
    }]);
    registry
}

fn text_prompt(text: &str) -> Vec<acp::ContentBlock> {
    vec![acp::ContentBlock::Text(acp::TextContent::new(text))]
}

/// Service the persistence channel: answer every `FlushAndAck` barrier and collect `HookAnnotation` messages for assertions.
/// The turn-end epilogue awaits the ack.
/// The `LocalSet` is single-thread, so `Rc<RefCell<..>>` is fine.
fn spawn_persistence_drain(
    mut rx: mpsc::UnboundedReceiver<PersistenceMsg>,
) -> std::rc::Rc<std::cell::RefCell<Vec<String>>> {
    let annotations = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = annotations.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                PersistenceMsg::FlushAndAck { respond_to } => {
                    let _ = respond_to.send(Ok(()));
                }
                PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) => {
                    if let XaiSessionUpdate::HookAnnotation { message, .. } = n.update {
                        sink.borrow_mut().push(message);
                    }
                }
                _ => {}
            }
        }
    });
    annotations
}

/// A blocking hook cancels a real user turn as `HookDenied` before the sampler runs.
/// The turn resolves `Cancelled` even though no model server exists, and the context carries the hook name and the user-facing reason.
#[tokio::test(flavor = "current_thread")]
async fn blocked_user_prompt_cancels_turn_without_sampling() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'no prod deploys' >&2; exit 2",
            )));

            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("deploy to prod"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;

            let ok = result.expect("a hook block must resolve Ok(Cancelled), not an error");
            assert_eq!(ok.stop_reason, acp::StopReason::Cancelled);
            match ok.completion_kind {
                PromptCompletionKind::Cancelled { category, context } => {
                    assert_eq!(
                        category,
                        Some(crate::session::events::CancellationCategory::HookDenied)
                    );
                    let ctx = context.expect("context must carry the hook detail");
                    assert_eq!(ctx.hook_name.as_deref(), Some("test/promptgate"));
                    assert_eq!(ctx.reason.as_deref(), Some("no prod deploys"));
                }
                other => panic!("expected Cancelled(HookDenied), got {other:?}"),
            }

            tokio::task::yield_now().await;
            let seen = annotations.borrow().clone();
            assert!(
                seen.iter()
                    .any(|m| m.contains("Prompt blocked by test/promptgate")
                        && m.contains("no prod deploys")),
                "the block reason must reach the user as an annotation: {seen:?}"
            );

            // The turn arms the hold at the verdict, before any completion handling: a user cancel racing the epilogue must not skip it
            assert!(
                actor.state.lock().await.hook_block_held(),
                "the queue hold must be armed by the turn itself"
            );
        })
        .await;
}

/// A hook's top-level `systemMessage` reaches the user verbatim on the rendered `HookAnnotation` channel, alongside the hook's decision.
#[tokio::test(flavor = "current_thread")]
async fn hook_system_message_reaches_user_as_annotation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                r#"echo '{"systemMessage":"heads up","decision":"block","reason":"nope"}'"#,
            )));

            let result = Box::pin(actor.handle_prompt(
                "p-msg",
                text_prompt("deploy to prod"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            let ok = result.expect("a hook block must resolve Ok(Cancelled)");
            assert_eq!(ok.stop_reason, acp::StopReason::Cancelled);

            tokio::task::yield_now().await;
            let seen = annotations.borrow().clone();
            assert!(
                seen.iter().any(|m| m == "heads up"),
                "the hook systemMessage must reach the user verbatim: {seen:?}"
            );
        })
        .await;
}

/// The hold makes the session non-idle for injection.
/// Notification drain and other idle injectors must not queue synthetic rows that would outrun the user's next prompt when the hold releases.
#[tokio::test(flavor = "current_thread")]
async fn hold_suppresses_idle_injection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let mut state = actor.state.lock().await;
            assert!(crate::session::acp_session::is_session_idle_for_injection(
                &state
            ));
            state.arm_hook_block_hold();
            assert!(
                !crate::session::acp_session::is_session_idle_for_injection(&state),
                "a held queue is not idle for injection"
            );
        })
        .await;
}

/// A stranded interjection flushed after a blocked turn stays queued under the hold.
/// It was typed before the block verdict was visible, so it must not auto-run as if the blocked prompt had succeeded.
#[tokio::test(flavor = "current_thread")]
async fn flushed_interjection_stays_parked_under_hold() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let actor =
                Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
            actor.state.lock().await.arm_hook_block_hold();
            actor
                .pending_interjections
                .push(crate::session::acp_session::PendingInterjection {
                    text: "steer text typed during the blocked turn".to_string(),
                    attachments: vec![],
                });

            assert_eq!(actor.flush_stranded_interjections().await, 1);
            let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
            actor.clone().maybe_start_running_task(completion_tx).await;

            let state = actor.state.try_lock().expect("uncontended");
            assert!(
                state.running_task.is_none(),
                "the flushed interjection must stay parked under the hold"
            );
            assert_eq!(state.pending_inputs.len(), 1, "the row stays queued");
        })
        .await;
}

/// A synthetic (auto-wake) origin is observe-only: the same blocking hook must not cancel the turn.
/// The turn proceeds toward the sampler, so the test watches for the not-enforced annotation and then aborts the turn.
#[tokio::test(flavor = "current_thread")]
async fn synthetic_prompt_ignores_hook_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'blocked' >&2; exit 2",
            )));

            // `task-completed-*` derives a TaskCompleted (synthetic) origin.
            let turn_actor = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                Box::pin(turn_actor.handle_prompt(
                    "task-completed-t1",
                    text_prompt("task finished"),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                ))
                .await
            });

            let found = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if annotations
                        .borrow()
                        .iter()
                        .any(|m| m.contains("not enforced for this origin"))
                    {
                        return true;
                    }
                    if turn.is_finished() {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the not-enforced annotation must arrive before the timeout");
            assert!(
                found,
                "a synthetic origin must record the block as observe-only, not cancel"
            );
            assert!(
                !actor.state.lock().await.hook_block_held(),
                "an unenforced block must not arm the queue hold"
            );
            turn.abort();
        })
        .await;
}

/// A real user prompt on a subagent session is observe-only: subagent sessions never enforce prompt blocks (`should_enforce_prompt_block`).
#[tokio::test(flavor = "current_thread")]
async fn subagent_session_ignores_hook_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            actor.startup_hints.is_subagent = true;
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'blocked' >&2; exit 2",
            )));

            let turn_actor = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                Box::pin(turn_actor.handle_prompt(
                    "p-user",
                    text_prompt("a real user prompt"),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                ))
                .await
            });

            let found = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if annotations
                        .borrow()
                        .iter()
                        .any(|m| m.contains("not enforced for this origin"))
                    {
                        return true;
                    }
                    if turn.is_finished() {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the not-enforced annotation must arrive before the timeout");
            assert!(
                found,
                "a subagent session must record the block as observe-only, not cancel"
            );
            assert!(
                !actor.state.lock().await.hook_block_held(),
                "an unenforced block must not arm the queue hold"
            );
            turn.abort();
        })
        .await;
}

/// No-op queue mutations report `false`/unmutated, so the run loop keeps the hook-block hold: a stale or foreign request is not user re-engagement.
#[tokio::test(flavor = "current_thread")]
async fn noop_queue_mutations_report_unchanged() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            assert!(
                !actor.handle_remove_queued_prompt("ghost", 0, None).await,
                "a stale remove must report unchanged"
            );
            assert!(
                !actor
                    .handle_edit_queued_prompt("ghost", "new text".into(), None)
                    .await,
                "an edit of a missing row must report unchanged"
            );
            assert!(
                !actor.handle_clear_queue(None).await,
                "clearing an empty queue must report unchanged"
            );
            assert!(
                !actor.handle_reorder_queue(&["ghost".to_string()]).await,
                "an identity reorder must report unchanged"
            );
            let send_now = actor
                .handle_interject_queued_prompt("ghost", 0, None, None)
                .await;
            assert!(
                !send_now.mutated && !send_now.cancel_running_turn,
                "a stale send-now must report unchanged"
            );
        })
        .await;
}

/// A `HookDenied` completion parks the queue: the follower must not promote until `release_hook_block_hold`.
/// The hold notice names the held rows.
#[tokio::test(flavor = "current_thread")]
async fn hook_denied_completion_holds_queue_until_release() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            // A running front (the blocked turn) plus one queued follower.
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-hook".to_string());
            let (front, _front_rx) = super::turn_completion_emit_tests::pending_input("p-hook");
            let (follower, _follower_rx) =
                super::turn_completion_emit_tests::pending_input("p-follower");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-hook"));
                state.pending_inputs.push_back(front);
                state.pending_inputs.push_back(follower);
                // The turn arms the hold at the block verdict; completion handling only announces it
                state.arm_hook_block_hold();
            }

            actor
                .handle_completion(
                    "p-hook".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Cancelled {
                            category: Some(
                                crate::session::events::CancellationCategory::HookDenied,
                            ),
                            context: None,
                        },
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    None,
                )
                .await;

            let actor = Arc::new(actor);
            {
                let state = actor.state.lock().await;
                assert!(
                    state.hook_block_held(),
                    "a HookDenied completion must park the queue"
                );
                assert_eq!(state.pending_inputs.len(), 1, "the follower stays queued");
            }
            tokio::task::yield_now().await;
            {
                let seen = annotations.borrow().clone();
                assert!(
                    seen.iter().any(|m| m.contains("on hold after the block")),
                    "the hold notice must reach the user: {seen:?}"
                );
            }

            let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
            actor
                .clone()
                .maybe_start_running_task(completion_tx.clone())
                .await;
            {
                let state = actor.state.try_lock().expect("uncontended");
                assert!(
                    state.running_task.is_none(),
                    "a held queue must not auto-promote the follower"
                );
            }

            actor.release_hook_block_hold("test_release").await;
            actor.clone().maybe_start_running_task(completion_tx).await;
            {
                let state = actor.state.try_lock().expect("no await since promote");
                assert_eq!(
                    state.running_prompt_id(),
                    Some("p-follower"),
                    "release must let the follower promote"
                );
                assert!(!state.hook_block_held());
                if let Some(task) = state.running_task.as_ref() {
                    task.handle.abort();
                }
            }
        })
        .await;
}

/// A user re-engagement between the block verdict and completion handling clears the hold.
/// The `HookDenied` completion must not re-arm it (the turn is the single writer), and no stale hold notice may be sent.
#[tokio::test(flavor = "current_thread")]
async fn hook_denied_completion_does_not_rearm_cleared_hold() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let annotations = spawn_persistence_drain(persistence_rx);
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-hook".to_string());
            let (front, _front_rx) = super::turn_completion_emit_tests::pending_input("p-hook");
            let (follower, _follower_rx) =
                super::turn_completion_emit_tests::pending_input("p-follower");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-hook"));
                state.pending_inputs.push_back(front);
                state.pending_inputs.push_back(follower);
                // The hold was armed at the verdict, then cleared by user re-engagement before the completion is handled
                state.take_hook_block_hold();
            }

            actor
                .handle_completion(
                    "p-hook".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Cancelled {
                            category: Some(
                                crate::session::events::CancellationCategory::HookDenied,
                            ),
                            context: None,
                        },
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    None,
                )
                .await;

            assert!(
                !actor.state.lock().await.hook_block_held(),
                "a HookDenied completion must not re-arm a cleared hold"
            );
            tokio::task::yield_now().await;
            let seen = annotations.borrow().clone();
            assert!(
                !seen.iter().any(|m| m.contains("on hold after the block")),
                "no stale hold notice after re-engagement: {seen:?}"
            );
        })
        .await;
}

/// Completions that are not hook denials must not park the queue.
#[tokio::test(flavor = "current_thread")]
async fn non_hook_cancel_does_not_hold_queue() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p-user".to_string());
            let (front, _front_rx) = super::turn_completion_emit_tests::pending_input("p-user");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("p-user"));
                state.pending_inputs.push_back(front);
            }

            actor
                .handle_completion(
                    "p-user".to_string(),
                    TurnEpoch::default(),
                    &completion_identity(&actor),
                    Ok(PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        total_tokens: 0,
                        turn_snapshot: None,
                        completion_kind: PromptCompletionKind::Cancelled {
                            category: Some(
                                crate::session::events::CancellationCategory::MidTurnAbort,
                            ),
                            context: None,
                        },
                        structured_output: None,
                        usage: None,
                        tool_overrides: None,
                    }),
                    None,
                )
                .await;

            assert!(
                !actor.state.lock().await.hook_block_held(),
                "a user abort must not park the queue"
            );
        })
        .await;
}

/// The observe-only scope must not inherit the storage boundary.
/// A blocking hook on a synthetic (auto-wake) origin still commits the wake text to chat state.
/// The turn proceeds toward the sampler, so the test watches for the commit and then aborts the turn.
#[tokio::test(flavor = "current_thread")]
async fn synthetic_prompt_commits_despite_blocking_hook() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'blocked' >&2; exit 2",
            )));

            // `task-completed-*` derives a TaskCompleted (synthetic) origin.
            let turn_actor = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                Box::pin(turn_actor.handle_prompt(
                    "task-completed-t1",
                    text_prompt("the wake text must commit"),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                    None,
                    None,
                    None,
                ))
                .await
            });

            let committed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let serialized =
                        serde_json::to_string(&actor.chat_state_handle.get_conversation().await)
                            .expect("serialize conversation");
                    if serialized.contains("the wake text must commit") {
                        return true;
                    }
                    if turn.is_finished() {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the commit must land before the timeout");
            assert!(
                committed,
                "an observe-only block must not stop the wake text from committing"
            );
            turn.abort();
        })
        .await;
}

/// Storage boundary: a blocked prompt never enters conversation history, so no later turn can carry it as context.
/// The allow path still commits before the sampler runs.
#[tokio::test(flavor = "current_thread")]
async fn blocked_prompt_never_enters_chat_state() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "if grep -q secret; then echo 'no secrets' >&2; exit 2; fi",
            )));

            let before = actor.chat_state_handle.get_conversation_len().await;
            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("deploy the secret token"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            let ok = result.expect("a hook block must resolve Ok(Cancelled)");
            assert_eq!(ok.stop_reason, acp::StopReason::Cancelled);

            let conversation = actor.chat_state_handle.get_conversation().await;
            assert_eq!(
                conversation.len(),
                before,
                "a blocked prompt must not add conversation items"
            );
            let serialized = serde_json::to_string(&conversation).expect("serialize conversation");
            assert!(
                !serialized.contains("secret"),
                "blocked text leaked into chat state: {serialized}"
            );

            // Control for the probe: an allowed prompt commits before the sampler runs
            // The turn then fails (no model server), but the commit precedes sampling
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Box::pin(actor.handle_prompt(
                    "p-allowed",
                    text_prompt("a clean prompt"),
                    PromptMode::Agent,
                    /* trace_gcs_config */ None,
                    /* artifact_tracker */ None,
                    /* client_identifier */ None,
                    /* screen_mode */ None,
                    /* verbatim */ false,
                    /* send_now */ false,
                    /* json_schema */ None,
                    /* persist_ack */ None,
                    /* parsed_prompt_tx */ None,
                )),
            )
            .await
            .expect("the allowed turn must finish (sampler error is fine)");
            let serialized =
                serde_json::to_string(&actor.chat_state_handle.get_conversation().await)
                    .expect("serialize conversation");
            assert!(
                serialized.contains("a clean prompt"),
                "the allow path must still commit the prompt: {serialized}"
            );
        })
        .await;
}

/// Serialize every content-bearing persistence message (updates, summary content chunks, chat items).
/// A test can then assert what reached, or never reached, the disk-bound channels.
/// It answers `FlushAndAck` like `spawn_persistence_drain` does.
fn spawn_persistence_capture(
    mut rx: mpsc::UnboundedReceiver<PersistenceMsg>,
) -> std::rc::Rc<std::cell::RefCell<Vec<String>>> {
    let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = captured.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                PersistenceMsg::FlushAndAck { respond_to } => {
                    let _ = respond_to.send(Ok(()));
                }
                PersistenceMsg::Update(u) => {
                    if let Ok(s) = serde_json::to_string(&u) {
                        sink.borrow_mut().push(s);
                    }
                }
                PersistenceMsg::ContentChunk(c) => sink.borrow_mut().push(format!("{c:?}")),
                PersistenceMsg::Chat(item) => {
                    if let Ok(s) = serde_json::to_string(&item) {
                        sink.borrow_mut().push(s);
                    }
                }
                _ => {}
            }
        }
    });
    captured
}

/// The strongest storage boundary: a blocked prompt's text reaches NO persistence channel.
/// Chat-history rebuilds and resume scrollback replay both read the `updates.jsonl` stream.
/// An allowed prompt's echo is the control proving the capture sees persistence traffic.
#[tokio::test(flavor = "current_thread")]
async fn blocked_prompt_never_reaches_persistence() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let captured = spawn_persistence_capture(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            // The reason deliberately shares no word with the prompt
            // The block annotation persists legitimately, so the probe below must only ever match the prompt text itself
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "if grep -q secret; then echo 'not allowed' >&2; exit 2; fi",
            )));

            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("deploy the secret token"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            let ok = result.expect("a hook block must resolve Ok(Cancelled)");
            assert_eq!(ok.stop_reason, acp::StopReason::Cancelled);

            tokio::task::yield_now().await;
            {
                let seen = captured.borrow().clone();
                assert!(
                    !seen.iter().any(|s| s.contains("secret")),
                    "blocked text reached a persistence channel: {seen:?}"
                );
            }

            // Control: the allowed prompt's user echo must reach persistence
            // That proves the capture observes the channels the assertion above clears
            // (The turn itself then fails, no model server.)
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Box::pin(actor.handle_prompt(
                    "p-allowed",
                    text_prompt("a clean visible prompt"),
                    PromptMode::Agent,
                    /* trace_gcs_config */ None,
                    /* artifact_tracker */ None,
                    /* client_identifier */ None,
                    /* screen_mode */ None,
                    /* verbatim */ false,
                    /* send_now */ false,
                    /* json_schema */ None,
                    /* persist_ack */ None,
                    /* parsed_prompt_tx */ None,
                )),
            )
            .await
            .expect("the allowed turn must finish (sampler error is fine)");
            tokio::task::yield_now().await;
            let seen = captured.borrow().clone();
            assert!(
                seen.iter().any(|s| s.contains("a clean visible prompt")),
                "the allow path must persist the user echo: {seen:?}"
            );
        })
        .await;
}

/// The caller's persist barrier resolves even though a blocked prompt is never pushed or flushed.
/// A client awaiting durable persistence must not hang on a block.
#[tokio::test(flavor = "current_thread")]
async fn blocked_prompt_resolves_persist_ack() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'nope' >&2; exit 2",
            )));

            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("anything"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ Some(ack_tx),
                /* parsed_prompt_tx */ None,
            ))
            .await;
            result.expect("a hook block must resolve Ok(Cancelled)");
            assert!(
                matches!(ack_rx.await, Ok(())),
                "persist_ack must resolve on a blocked prompt"
            );
        })
        .await;
}

/// A pending prior-interrupt marker survives a hook-blocked turn.
/// The blocked turn commits nothing and consumes nothing, so the next real prompt must still carry the tag from the earlier user interrupt.
#[tokio::test(flavor = "current_thread")]
async fn blocked_turn_preserves_prior_interrupt_marker() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'nope' >&2; exit 2",
            )));

            actor.events.set_prior_interrupt_category(
                crate::session::events::CancellationCategory::MidTurnAbort,
            );

            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("anything"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            result.expect("a hook block must resolve Ok(Cancelled)");

            assert_eq!(
                actor.events.take_prior_interrupt_category(),
                Some(crate::session::events::CancellationCategory::MidTurnAbort),
                "the blocked turn must not clobber the pending interrupt marker"
            );
        })
        .await;
}

/// The one-shot redirect marker (Ctrl+C then resend) survives a blocked turn.
/// It was consumed for the blocked `TurnStarted`, but that turn never ran, so the next real prompt must still carry it.
#[tokio::test(flavor = "current_thread")]
async fn blocked_turn_preserves_redirect_marker() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "echo 'nope' >&2; exit 2",
            )));

            actor
                .events
                .set_prior_redirect_kind(crate::session::events::RedirectKind::CancelThenSend);

            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("anything"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            result.expect("a hook block must resolve Ok(Cancelled)");

            assert!(
                matches!(
                    actor.events.take_prior_redirect_kind(),
                    Some(crate::session::events::RedirectKind::CancelThenSend)
                ),
                "the blocked turn must not consume the redirect marker"
            );
        })
        .await;
}

/// A blocked turn consumes no prompt index: `prompt_index == prompt_texts.len()` is a rewind invariant.
/// A gap would put rewind preview and edit-and-retry out of sync for every later prompt.
#[tokio::test(flavor = "current_thread")]
async fn blocked_prompt_consumes_no_prompt_index() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let _annotations = spawn_persistence_drain(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.hook_resolved_workspace_root = "/tmp".to_string();
            let actor = Arc::new(actor);
            *actor.hook_registry.borrow_mut() = Some(std::sync::Arc::new(prompt_gate_registry(
                "if grep -q secret; then echo 'not allowed' >&2; exit 2; fi",
            )));

            let before = actor.chat_state_handle.get_prompt_index().await;
            let result = Box::pin(actor.handle_prompt(
                "p-blocked",
                text_prompt("a secret prompt"),
                PromptMode::Agent,
                /* trace_gcs_config */ None,
                /* artifact_tracker */ None,
                /* client_identifier */ None,
                /* screen_mode */ None,
                /* verbatim */ false,
                /* send_now */ false,
                /* json_schema */ None,
                /* persist_ack */ None,
                /* parsed_prompt_tx */ None,
            ))
            .await;
            result.expect("a hook block must resolve Ok(Cancelled)");
            assert_eq!(
                actor.chat_state_handle.get_prompt_index().await,
                before,
                "a blocked turn must not consume a prompt index"
            );

            // Control: an allowed prompt consumes exactly one index (the turn then fails, no model server)
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Box::pin(actor.handle_prompt(
                    "p-allowed",
                    text_prompt("a clean prompt"),
                    PromptMode::Agent,
                    /* trace_gcs_config */ None,
                    /* artifact_tracker */ None,
                    /* client_identifier */ None,
                    /* screen_mode */ None,
                    /* verbatim */ false,
                    /* send_now */ false,
                    /* json_schema */ None,
                    /* persist_ack */ None,
                    /* parsed_prompt_tx */ None,
                )),
            )
            .await
            .expect("the allowed turn must finish (sampler error is fine)");
            assert_eq!(
                actor.chat_state_handle.get_prompt_index().await,
                before + 1,
                "an allowed prompt consumes exactly one index"
            );
        })
        .await;
}
