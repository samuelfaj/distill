// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use distill_tool_types::{
    KillTaskOutput, KillTaskResult, MultiTaskOutputResult, SubagentCompletedOutput,
    TaskOutputOutput, TaskOutputResult,
};
use distill_tools::reminders::task_completion::consumed_completion_ids;
use distill_tools::types::output::{BashOutput, TextOutput, ToolOutput};
fn input_with_origin(prompt_id: &str, origin: crate::session::PromptOrigin) -> InputItem {
    input_with_origin_rx(prompt_id, origin).0
}
fn task_completed_input(task_id: &str) -> InputItem {
    input_with_origin(
        &format!("task-completed-{task_id}"),
        crate::session::PromptOrigin::TaskCompleted {
            task_id: task_id.to_string(),
        },
    )
}
fn subagent_completed_input(subagent_id: &str) -> InputItem {
    input_with_origin(
        &format!("subagent-completed-{subagent_id}"),
        crate::session::PromptOrigin::SubagentCompleted {
            subagent_id: subagent_id.to_string(),
        },
    )
}
fn notification_drain_input(prompt_id: &str) -> InputItem {
    input_with_origin(prompt_id, crate::session::PromptOrigin::NotificationDrain)
}
fn goal_summary_input(prompt_id: &str) -> InputItem {
    input_with_origin(prompt_id, crate::session::PromptOrigin::GoalSummary)
}
fn user_input(prompt_id: &str) -> InputItem {
    input_with_origin(prompt_id, crate::session::PromptOrigin::User)
}
/// Same body `inject_subagent_completed_prompt` sends.
fn wake_body(subagent_id: &str) -> String {
    distill_tools::reminders::wrap_reminder(
        &distill_tools::reminders::task_completion::format_subagent_completion(
            &subagent_summary(subagent_id),
            Some("get_command_or_subagent_output"),
            None,
            None,
        ),
    )
}
async fn run_wake_turn(
    actor: &std::sync::Arc<SessionActor>,
    subagent_id: &str,
    persist_ack: Option<oneshot::Sender<()>>,
) -> PromptTurnResult {
    actor
        .handle_prompt(
            &format!("subagent-completed-{subagent_id}"),
            vec![acp::ContentBlock::Text(acp::TextContent::new(wake_body(
                subagent_id,
            )))],
            PromptMode::Agent,
            None,
            None,
            None,
            None,
            true,
            false,
            None,
            persist_ack,
            None,
        )
        .await
}
/// Aborts the wake turn after its commit: the test sampler never answers.
async fn run_wake_turn_to_commit(actor: &std::sync::Arc<SessionActor>, subagent_id: &str) {
    let (ack_tx, ack_rx) = oneshot::channel();
    let actor_for_turn = actor.clone();
    let subagent_id = subagent_id.to_owned();
    let turn = tokio::task::spawn_local(async move {
        run_wake_turn(&actor_for_turn, &subagent_id, Some(ack_tx)).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx)
        .await
        .expect("wake turn must reach the prompt commit")
        .expect("persist ack must resolve");
    turn.abort();
}
fn task_output_result(task_id: &str, status: &str) -> TaskOutputResult {
    TaskOutputResult {
        task_id: task_id.to_string(),
        command: "echo test".into(),
        status: status.to_string(),
        exit_code: Some(0),
        started: "2026-01-01T00:00:00Z".into(),
        ended: Some("2026-01-01T00:00:01Z".into()),
        duration_secs: 1.0,
        output: "test".into(),
        output_file: "/tmp/out.log".into(),
        truncated: false,
        truncation_hint: String::new(),
        raw_output_bytes: 4,
    }
}
fn bash_completed_notification(task_id: &str) -> PendingNotification {
    PendingNotification {
        prompt_id: format!("bash-completed-{task_id}"),
        prompt_blocks: vec![],
        priority: NotificationPriority::Later,
        source: NotificationSource::BashTaskCompleted {
            task_id: task_id.to_string(),
        },
    }
}
fn monitor_completed_notification(task_id: &str) -> PendingNotification {
    PendingNotification {
        prompt_id: format!("monitor-completed-{task_id}"),
        prompt_blocks: vec![],
        priority: NotificationPriority::Later,
        source: NotificationSource::MonitorCompleted {
            task_id: task_id.to_string(),
        },
    }
}
fn task_wake_admission(
    task_id: &str,
    source: NotificationSource,
) -> (
    crate::session::commands::TaskWakeAdmission,
    oneshot::Receiver<bool>,
) {
    let (respond_to, response_rx) = oneshot::channel();
    (
        crate::session::commands::TaskWakeAdmission {
            respond_to,
            fallback: crate::session::commands::TaskWakeFallback {
                prompt_id: format!("deferred-{task_id}"),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
                    "completion {task_id}"
                )))],
                source,
            },
        },
        response_rx,
    )
}
fn monitor_event_notification(task_id: &str) -> PendingNotification {
    PendingNotification {
        prompt_id: format!("monitor-{task_id}"),
        prompt_blocks: vec![],
        priority: NotificationPriority::Next,
        source: NotificationSource::MonitorEvent {
            task_id: task_id.to_string(),
        },
    }
}
#[test]
fn pending_notification_cap_keeps_newest_entries() {
    let mut state = State {
        running_task: None,
        finalization_gate: Default::default(),
        message_delivery: Default::default(),
        pending_inputs: std::collections::VecDeque::new(),
        edit_holds: HashMap::new(),
        pending_notifications: Vec::new(),
        notifications_suppressed: true,
        rewindable: false,
        front_message_committed: false,
        hook_block_hold: Default::default(),
        nudges_used_this_session: 0,
    };
    for index in 0..(MAX_PENDING_NOTIFICATIONS + 3) {
        SessionActor::push_pending_notification(
            &mut state,
            bash_completed_notification(&format!("task-{index}")),
        );
    }
    assert_eq!(state.pending_notifications.len(), MAX_PENDING_NOTIFICATIONS);
    assert_eq!(
        state
            .pending_notifications
            .first()
            .map(|n| n.source.task_id()),
        Some("task-3")
    );
    let newest = format!("task-{}", MAX_PENDING_NOTIFICATIONS + 2);
    assert_eq!(
        state.pending_notifications.last().unwrap().source.task_id(),
        newest
    );
}
#[tokio::test(flavor = "current_thread")]
async fn drain_batches_monitor_notifications_into_formatted_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                distill_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
            let monitor_notif = |task: &str, line: &str| PendingNotification {
                prompt_id: format!("monitor-{task}"),
                prompt_blocks: vec![agent_client_protocol::ContentBlock::Text(
                    agent_client_protocol::TextContent::new(format!(
                            "<monitor-event description=\"watch\" task_id=\"{task}\">\n{line}\n</monitor-event>"
                        )),
                )],
                priority: NotificationPriority::Next,
                source: NotificationSource::MonitorEvent {
                    task_id: task.to_string(),
                },
            };
            let mut bash = bash_completed_notification("bg-1");
            bash.prompt_blocks = vec![agent_client_protocol::ContentBlock::Text(
                agent_client_protocol::TextContent::new("Background task \"bg-1\" completed."),
            )];
            let mut state = actor.state.lock().await;
            let drained = SessionActor::drain_notifications_into_turn(
                &mut state,
                vec![
                    monitor_notif("mon-1", "tick 1"),
                    bash,
                    monitor_notif("mon-1", "tick 2"),
                ],
                "get_task_output",
            );
            assert!(drained);
            let item = state.pending_inputs.back().expect("drained turn queued");
            let text = item
                .prompt_blocks
                .iter()
                .filter_map(|b| match b {
                    agent_client_protocol::ContentBlock::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("2 monitor events from 1 monitor"),
                "monitor entries must collapse into one formatted batch: {text}"
            );
            assert!(
                text.contains(
                    "<monitor description=\"watch\" task_id=\"mon-1\">\n[1] tick 1\n[2] tick 2"
                ),
                "batch must group + label the ticks: {text}"
            );
            assert_eq!(
                text.matches("<monitor-event").count(),
                0,
                "raw per-event wrappers must not survive the drain: {text}"
            );
            assert!(
                text.contains("Background task \"bg-1\" completed."),
                "non-monitor notification keeps its raw block: {text}"
            );
            assert_eq!(
                text.matches("---").count(),
                1,
                "one separator between the batch and the bash block: {text}"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn closed_admission_ack_stores_fallback_before_prompt_rejection() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let origin = crate::session::PromptOrigin::TaskCompleted {
                task_id: "mon-timeout".to_string(),
            };
            let (admission, response_rx) = task_wake_admission(
                "mon-timeout",
                NotificationSource::MonitorCompleted {
                    task_id: "mon-timeout".to_string(),
                },
            );
            drop(response_rx);
            assert!(
                actor
                    .admit_task_completion_wake(&origin, admission)
                    .await
                    .is_none()
            );
            let state = actor.state.lock().await;
            assert!(matches!(
                state.pending_notifications.as_slice(),
                [PendingNotification {
                    source: NotificationSource::MonitorCompleted { task_id },
                    ..
                }] if task_id == "mon-timeout"
            ));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn non_task_prompt_is_not_subject_to_task_wake_barrier() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.state.lock().await.notifications_suppressed = true;
            let (admission, response_rx) = task_wake_admission(
                "sub-1",
                NotificationSource::BashTaskCompleted {
                    task_id: "sub-1".to_string(),
                },
            );
            assert!(
                actor
                    .admit_task_completion_wake(
                        &crate::session::PromptOrigin::SubagentCompleted {
                            subagent_id: "sub-1".to_string(),
                        },
                        admission,
                    )
                    .await
                    .is_some(),
                "subagent completion is outside terminal task-wake suppression scope"
            );
            assert_eq!(response_rx.await, Ok(true));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn task_completion_wake_is_admitted_without_cancel_barrier() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            let origin = crate::session::PromptOrigin::TaskCompleted {
                task_id: "bg-normal".to_string(),
            };
            actor
                .tool_context
                .task_completion_reservations
                .as_ref()
                .expect("completion reservations")
                .reserve("bg-normal".to_string());
            let (admission, response_rx) = task_wake_admission(
                "bg-normal",
                NotificationSource::BashTaskCompleted {
                    task_id: "bg-normal".to_string(),
                },
            );
            let fallback = actor
                .admit_task_completion_wake(&origin, admission)
                .await
                .expect("normal task wake should be admitted");
            assert_eq!(response_rx.await, Ok(true));
            let (respond_to, _rx) = oneshot::channel();
            let _ = actor
                .queue_input(QueueInputRequest {
                    verbatim: true,
                    task_wake_fallback: Some(fallback),
                    ..queue_input_request(vec![], "task-completed-bg-normal", respond_to)
                })
                .await;
            let state = actor.state.lock().await;
            assert_eq!(state.pending_inputs.len(), 1);
            assert!(matches!(
                state
                    .pending_inputs
                    .front()
                    .map(|item| item.input_origin.as_prompt_origin()),
                Some(crate::session::PromptOrigin::TaskCompleted { task_id })
                    if task_id == "bg-normal"
            ));
            drop(state);
            assert!(
                !already_reported(&actor, "bg-normal").await,
                "queue acceptance alone must not mark the completion reported"
            );
            let actor_for_turn = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                actor_for_turn
                    .handle_prompt(
                        "task-completed-bg-normal",
                        vec![acp::ContentBlock::Text(acp::TextContent::new("done"))],
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        true,
                        false,
                        None,
                        None,
                        None,
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if already_reported(&actor, "bg-normal").await {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("synthetic turn marked completion reported");
            turn.abort();
            assert!(
                already_reported(&actor, "bg-normal").await,
                "actual synthetic turn start must mark the completion reported"
            );
            assert!(
                actor
                    .tool_context
                    .task_completion_reservations
                    .as_ref()
                    .is_none_or(|ids| !ids.contains("bg-normal"))
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn disk_full_refusal_still_clears_task_completion_reservation() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = disk_full_actor().await;
            actor
                .tool_context
                .task_completion_reservations
                .as_ref()
                .expect("completion reservations")
                .reserve("bg-disk".to_string());
            let actor = std::sync::Arc::new(actor);
            let error = actor
                .handle_prompt(
                    "task-completed-bg-disk",
                    vec![acp::ContentBlock::Text(acp::TextContent::new("done"))],
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    true,
                    false,
                    None,
                    None,
                    None,
                )
                .await
                .expect_err("latched disk-full must refuse the wake");
            assert_eq!(error.message, "No space left on device");
            assert!(
                !already_reported(&actor, "bg-disk").await,
                "a refused wake never reached the model, so a later drain must still be able to surface it"
            );
            assert!(
                actor
                    .tool_context
                    .task_completion_reservations
                    .as_ref()
                    .is_none_or(|ids| !ids.contains("bg-disk")),
                "disk-full refusal must release the completion reservation"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn genuine_user_start_consumes_deferred_completions_without_notification_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            let body = distill_tools::reminders::task_completion::format_monitor_completion(
                &distill_tools::types::TaskSnapshot {
                    task_id: "mon-quiet".to_string(),
                    command: "tail -f quiet.log".to_string(),
                    display_command: Some("[monitor] quiet logs".to_string()),
                    cwd: String::new(),
                    start_time: std::time::SystemTime::now(),
                    end_time: Some(std::time::SystemTime::now()),
                    output: String::new(),
                    output_file: std::path::PathBuf::new(),
                    truncated: false,
                    exit_code: Some(0),
                    signal: None,
                    completed: true,
                    kind: distill_tools::computer::types::TaskKind::Monitor,
                    block_waited: false,
                    explicitly_killed: false,
                    kill_result_delivered: false,
                    owner_session_id: None,
                    description: None,
                    is_backgrounded: false,
                    output_total_bytes: 0,
                },
                Some("get_command_or_subagent_output"),
            );
            {
                let mut state = actor.state.lock().await;
                state.notifications_suppressed = true;
                state
                    .pending_notifications
                    .push(monitor_event_notification("mon-quiet"));
                let mut monitor_completion = monitor_completed_notification("mon-quiet");
                monitor_completion.prompt_blocks =
                    vec![acp::ContentBlock::Text(acp::TextContent::new(body))];
                state.pending_notifications.push(monitor_completion);
                let mut bash_completion = bash_completed_notification("bash-deferred");
                bash_completion.prompt_blocks = vec![acp::ContentBlock::Text(
                    acp::TextContent::new("Background task bash-deferred completed."),
                )];
                state.pending_notifications.push(bash_completion);
            }
            let reservations = actor
                .tool_context
                .task_completion_reservations
                .as_ref()
                .expect("completion reservations");
            reservations.reserve("mon-quiet".to_string());
            reservations.reserve("bash-deferred".to_string());
            actor
                .tool_context
                .task_wake_suppressed
                .as_ref()
                .expect("task-wake gate")
                .set(true);
            let actor_for_turn = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                actor_for_turn
                    .handle_prompt(
                        "user-deferred-completions",
                        vec![acp::ContentBlock::Text(acp::TextContent::new("continue"))],
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
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if actor.state.lock().await.pending_notifications.is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("user turn consumed deferred completions");
            turn.abort();
            tokio::task::yield_now().await;
            let state = actor.state.lock().await;
            assert!(state.notifications_suppressed);
            assert!(state.pending_notifications.is_empty());
            assert!(state.pending_inputs.iter().all(|input| !matches!(
                input.input_origin.as_prompt_origin(),
                crate::session::PromptOrigin::NotificationDrain
            )));
            drop(state);
            let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
            SessionActor::maybe_drain_notifications(actor.clone(), completion_tx).await;
            let state = actor.state.lock().await;
            assert!(state.pending_inputs.iter().all(|input| !matches!(
                input.input_origin.as_prompt_origin(),
                crate::session::PromptOrigin::NotificationDrain
            )));
            drop(state);
            assert!(!reservations.contains("mon-quiet"));
            assert!(!reservations.contains("bash-deferred"));
            assert!(
                !actor
                    .tool_context
                    .task_wake_suppressed
                    .as_ref()
                    .expect("task-wake gate")
                    .get()
            );
            let conversation = actor.chat_state_handle.get_conversation().await;
            let text = conversation
                .iter()
                .map(|item| item.text_content())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("Monitor \"mon-quiet\" ended"));
            assert!(text.contains("Background task bash-deferred completed."));
            assert!(!text.contains("<monitor-event"));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn accepted_reservation_survives_user_start() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            let reservations = actor
                .tool_context
                .task_completion_reservations
                .as_ref()
                .expect("completion reservations");
            reservations.reserve("accepted-race".to_string());
            actor
                .tool_context
                .task_wake_suppressed
                .as_ref()
                .expect("task-wake gate")
                .set(true);
            let actor_for_turn = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                actor_for_turn
                    .handle_prompt(
                        "user-accepted-race",
                        vec![acp::ContentBlock::Text(acp::TextContent::new("continue"))],
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
                    )
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if actor
                        .tool_context
                        .task_wake_suppressed
                        .as_ref()
                        .is_none_or(|gate| !gate.get())
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("user turn started");
            assert!(reservations.contains("accepted-race"));
            turn.abort();
            reservations.release("accepted-race");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn same_id_bash_completion_does_not_suppress_monitor_event() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                distill_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx)
                .await;
            let monitor = PendingNotification {
                prompt_id: "monitor-shared".to_string(),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "<monitor-event description=\"watch\" task_id=\"shared\">\nstdout\n</monitor-event>",
                ))],
                priority: NotificationPriority::Next,
                source: NotificationSource::MonitorEvent {
                    task_id: "shared".to_string(),
                },
            };
            let mut bash = bash_completed_notification("shared");
            bash.prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                "Background task shared completed.",
            ))];
            let mut state = actor.state.lock().await;
            SessionActor::drain_notifications_into_turn(
                &mut state,
                vec![monitor, bash],
                "get_command_or_subagent_output",
            );
            let text = state
                .pending_inputs
                .back()
                .expect("drained turn")
                .prompt_blocks
                .iter()
                .filter_map(|block| match block {
                    acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("<monitor-event task_id=\"shared\">"));
            assert!(text.contains("Background task shared completed."));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn task_output_completed_drops_matching_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-other"));
                state.pending_inputs.push_back(user_input("user-real"));
            }
            let output = ToolOutput::TaskOutput(TaskOutputOutput::Result(task_output_result(
                "bg-target",
                "completed",
            )));
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["bg-target"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["task-completed-bg-other", "user-real"],
                "only the matching synthetic input should be dropped"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn task_output_failed_drops_matching_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-failed"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-running"));
            }
            let output =
                ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(MultiTaskOutputResult {
                    mode: "wait_all".into(),
                    results: vec![
                        task_output_result("bg-failed", "failed"),
                        task_output_result("bg-running", "running"),
                    ],
                    summary: String::new(),
                }));
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["bg-failed"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(remaining_ids, vec!["task-completed-bg-running"]);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn interrupted_wait_placeholder_keeps_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor
                .state
                .lock()
                .await
                .pending_inputs
                .push_back(task_completed_input("bg-still-running"));
            let output = ToolOutput::TaskOutput(TaskOutputOutput::Result(task_output_result(
                "bg-still-running",
                "cancelled",
            )));
            let consumed = consumed_completion_ids(&output);
            assert!(consumed.is_empty());
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(remaining_ids, vec!["task-completed-bg-still-running"]);
        })
        .await;
}
/// An auto-wake turn polls its own task's output, so the consumed id matches the front `task-completed-{id}` entry, which IS the in-flight turn.
/// (`maybe_start_running_task` promotes the front without popping it.).
/// Deleting it shifts whatever is queued behind (a real user prompt) to index 0, which the next interactive cancel resolves as Cancelled.
#[tokio::test(flavor = "current_thread")]
async fn sweep_never_drops_running_turns_own_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("task-completed-bg-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-target"));
                state.pending_inputs.push_back(user_input("user-real"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-other"));
            }
            actor
                .drop_pending_items_for_consumed_completions(&["bg-target", "bg-other"])
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["task-completed-bg-target", "user-real"],
                "the running turn's own front slot must survive the sweep; \
                 only the queued non-running synthetic is dropped"
            );
        })
        .await;
}
/// `queue_input`'s user-priority preempt is the second sweep over `pending_inputs` and needs the same guard.
/// Otherwise the user prompt lands at index 0 and the next interactive cancel destroys it.
#[tokio::test(flavor = "current_thread")]
async fn user_prompt_preempt_keeps_running_synthetic_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let reservations = actor
                .tool_context
                .task_completion_reservations
                .as_ref()
                .expect("completion reservations");
            reservations.reserve("bg-other".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("task-completed-bg-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-other"));
            }
            let (respond_to, _rx) = oneshot::channel();
            let _ = actor
                .queue_input(queue_input_request(vec![], "user-clarify", respond_to))
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["task-completed-bg-target", "user-clarify"],
                "the running synthetic turn's slot must survive the user-priority \
                 preempt; only the queued non-running synthetic is dropped"
            );
            assert!(
                !reservations.contains("bg-other"),
                "ordinary user-priority preemption releases ownership immediately"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn await_text_completed_drops_matching_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-other"));
                state.pending_inputs.push_back(user_input("user-real"));
            }
            let output = ToolOutput::Text(TextOutput {
                text: "Task completed in 100ms with exit code: 0.".into(),
                consumed_completion_task_id: Some("bg-target".into()),
            });
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["bg-target"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["task-completed-bg-other", "user-real"],
                "only the matching synthetic input should be dropped"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn kill_task_drops_matching_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-killed"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-alive"));
            }
            let output = ToolOutput::KillTask(KillTaskOutput::Result(KillTaskResult {
                task_id: "bg-killed".into(),
                outcome: "killed".into(),
                message: "Task was terminated successfully".into(),
            }));
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["bg-killed"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(remaining_ids, vec!["task-completed-bg-alive"]);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn subagent_completed_drops_matching_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(subagent_completed_input("sub-target"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-survives"));
                state
                    .pending_notifications
                    .push(bash_completed_notification("sub-target"));
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-survives"));
            }
            let output = ToolOutput::SubagentCompleted(SubagentCompletedOutput {
                output: "done".into(),
                subagent_id: "sub-target".into(),
                subagent_type: "general-purpose".into(),
                tool_calls: 1,
                turns: 1,
                duration_ms: 500,
                worktree_path: None,
                persona: None,
                resume_from_hint: "sub-target".into(),
                persona_hint: None,
            });
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["sub-target"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(remaining_ids, vec!["task-completed-bg-survives"]);
            let remaining_notif: Vec<&str> = state
                .pending_notifications
                .iter()
                .map(|n| n.source.task_id())
                .collect();
            assert_eq!(
                remaining_notif,
                vec!["bg-survives"],
                "pending notification for the consumed subagent id must be dropped"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn multi_task_output_drops_each_completed_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-done-1"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-done-2"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-running"));
            }
            let output =
                ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(MultiTaskOutputResult {
                    mode: "all".into(),
                    results: vec![
                        task_output_result("bg-done-1", "completed"),
                        task_output_result("bg-done-2", "completed"),
                        task_output_result("bg-running", "running"),
                    ],
                    summary: String::new(),
                }));
            let consumed = consumed_completion_ids(&output);
            assert_eq!(consumed, vec!["bg-done-1", "bg-done-2"]);
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(remaining_ids, vec!["task-completed-bg-running"]);
        })
        .await;
}
/// A "running" result is a poll snapshot, not a consumption of the completion.
#[tokio::test(flavor = "current_thread")]
async fn task_output_running_does_not_drop_pending_input() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-running"));
            }
            let output = ToolOutput::TaskOutput(TaskOutputOutput::Result(task_output_result(
                "bg-running",
                "running",
            )));
            let consumed = consumed_completion_ids(&output);
            assert!(
                consumed.is_empty(),
                "running status must not yield a consumed completion id"
            );
            actor
                .drop_pending_items_for_consumed_completions(&consumed)
                .await;
            let state = actor.state.lock().await;
            assert_eq!(
                state.pending_inputs.len(),
                1,
                "running status must NOT drop the queued synthetic prompt"
            );
        })
        .await;
}
/// Negative-case coverage for the exhaustive match in `consumed_completion_ids`.
/// The compiler already enforces exhaustiveness via the match.
/// These tests pin the no-op arms so a future contributor who adds a real id to one of them breaks a test.
#[tokio::test(flavor = "current_thread")]
async fn task_not_found_does_not_consume() {
    let out = ToolOutput::TaskOutput(TaskOutputOutput::TaskNotFound("missing".into()));
    assert!(consumed_completion_ids(&out).is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn kill_task_not_found_does_not_consume() {
    let out = ToolOutput::KillTask(KillTaskOutput::TaskNotFound("missing".into()));
    assert!(consumed_completion_ids(&out).is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn unrelated_tool_output_does_not_consume() {
    let bash = BashOutput {
        output: b"hi".to_vec(),
        output_for_prompt: "hi".into(),
        exit_code: 0,
        command: "echo hi".into(),
        truncated: false,
        signal: None,
        timed_out: false,
        description: None,
        current_dir: "/tmp".into(),
        output_file: String::new(),
        total_bytes: 2,
        output_delta: None,
        was_bare_echo: false,
    };
    let out = ToolOutput::Bash(bash);
    assert!(consumed_completion_ids(&out).is_empty());
}
#[tokio::test(flavor = "current_thread")]
async fn sweep_clears_matching_pending_notifications() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-A"));
                state
                    .pending_notifications
                    .push(monitor_event_notification("bg-A"));
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-B"));
            }
            actor
                .drop_pending_items_for_consumed_completions(&["bg-A"])
                .await;
            let state = actor.state.lock().await;
            let remaining: Vec<&str> = state
                .pending_notifications
                .iter()
                .map(|n| n.source.task_id())
                .collect();
            assert_eq!(
                remaining,
                vec!["bg-B"],
                "all notifications with the consumed task_id must be cleared \
                     (including MonitorEvent shapes — by design, since the model just \
                     learned the task is done so any pending monitor stdout is stale)"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn shutdown_drops_pending_synthetic_inputs() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_input("user-1"));
                state.pending_inputs.push_back(task_completed_input("bg-1"));
                state.pending_inputs.push_back(user_input("user-2"));
                state
                    .pending_inputs
                    .push_back(subagent_completed_input("sub-1"));
                state
                    .pending_inputs
                    .push_back(notification_drain_input("notifications-019e0000"));
                state
                    .pending_inputs
                    .push_back(goal_summary_input("goal-summary-019e2d3e"));
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-2"));
                state
                    .pending_notifications
                    .push(monitor_event_notification("mon-1"));
            }
            actor.drop_pending_synthetic_items().await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["user-1", "user-2"],
                "real user inputs must be preserved; every synthetic origin \
                     (TaskCompleted, SubagentCompleted, NotificationDrain, GoalSummary) \
                     must be dropped"
            );
            assert!(
                state.pending_notifications.is_empty(),
                "all pending notifications must be cleared on shutdown"
            );
        })
        .await;
}
/// A regression that removes or moves the call from `handle_bridge_tool_success` will be caught here even though the helper unit-tests still pass.
/// It mirrors the call shape used by `execute_tool_calls`.
#[tokio::test(flavor = "current_thread")]
async fn handle_bridge_tool_success_runs_consumed_completion_sweep() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-foo"));
                state
                    .pending_inputs
                    .push_back(task_completed_input("bg-other"));
            }
            let output = ToolOutput::TaskOutput(TaskOutputOutput::Result(task_output_result(
                "bg-foo",
                "completed",
            )));
            let result = ToolRunResult {
                output,
                prompt_text: "ok".into(),
                effective_tool_name: None,
            };
            let parsed_args = serde_json::json!({});
            let _ = actor
                .handle_bridge_tool_success(BridgeToolSuccess {
                    tool_call_id: &acp::ToolCallId::new("tc-1"),
                    call_id: "tc-1",
                    requested_tool_name: "get_task_output",
                    effective_tool_name: "get_task_output",
                    drained: DrainedToolSuccess::new(result),
                    concatenated_json_count: 0,
                    model_id: "test-model",
                    tool_parsed_args: &parsed_args,
                    model_output_override: None,
                })
                .await;
            let state = actor.state.lock().await;
            let remaining_ids: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                remaining_ids,
                vec!["task-completed-bg-other"],
                "handle_bridge_tool_success must run the Fix 1 sweep — the matching \
                     synthetic prompt for bg-foo should be gone"
            );
        })
        .await;
}
/// Read-only: safe to call before a drain.
async fn already_reported(actor: &SessionActor, task_id: &str) -> bool {
    use distill_tools::reminders::task_completion::ReportedTaskCompletions;
    use distill_tools::types::resources::State;
    let bridge = actor.agent.borrow().tool_bridge().clone();
    let resources = bridge.shared_resources().await;
    let res = resources.lock().await;
    res.get::<State<ReportedTaskCompletions>>()
        .is_some_and(|reported| reported.is_reported(task_id))
}
/// Pure decision: a goal-turn-origin task is dropped even when the blanket goal Active/Complete gate is OFF (status Blocked / paused / None).
/// That is the exact bug.
#[tokio::test(flavor = "current_thread")]
async fn split_drops_goal_turn_origin_when_blanket_gate_off() {
    let mut goal_turn = std::collections::HashSet::new();
    goal_turn.insert("bg-goal".to_string());
    let notifications = vec![
        bash_completed_notification("bg-goal"),
        bash_completed_notification("bg-user"),
        monitor_event_notification("bg-goal"),
    ];
    let (surface, dropped) = SessionActor::split_goal_suppressed(false, &goal_turn, notifications);
    assert_eq!(
        dropped, 2,
        "both goal-origin entries (bash + monitor) dropped"
    );
    let surfaced: Vec<&str> = surface.iter().map(|n| n.source.task_id()).collect();
    assert_eq!(
        surfaced,
        vec!["bg-user"],
        "only the non-goal-origin completion survives"
    );
}
/// This guards against an over-suppression regression.
#[tokio::test(flavor = "current_thread")]
async fn split_surfaces_normal_completions_with_no_goal() {
    let goal_turn = std::collections::HashSet::new();
    let notifications = vec![
        bash_completed_notification("bg-1"),
        bash_completed_notification("bg-2"),
    ];
    let (surface, dropped) = SessionActor::split_goal_suppressed(false, &goal_turn, notifications);
    assert_eq!(dropped, 0, "no goal => no suppression");
    let surfaced: Vec<&str> = surface.iter().map(|n| n.source.task_id()).collect();
    assert_eq!(surfaced, vec!["bg-1", "bg-2"]);
}
#[tokio::test(flavor = "current_thread")]
async fn split_blanket_gate_drops_all() {
    let goal_turn = std::collections::HashSet::new();
    let notifications = vec![
        bash_completed_notification("bg-1"),
        monitor_event_notification("mon-1"),
    ];
    let (surface, dropped) = SessionActor::split_goal_suppressed(true, &goal_turn, notifications);
    assert!(surface.is_empty(), "blanket gate surfaces nothing");
    assert_eq!(dropped, 2);
}
#[tokio::test(flavor = "current_thread")]
async fn drain_drops_goal_turn_origin_when_status_none_and_marks_reported() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            set_goal_harness_for_tests(&actor);
            actor
                .goal_turn_task_ids
                .lock()
                .insert("bg-goal".to_string());
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-goal"));
            }
            let (completion_tx, _completion_rx) =
                tokio::sync::mpsc::unbounded_channel::<TurnCompletionMsg>();
            std::sync::Arc::clone(&actor)
                .maybe_drain_notifications(completion_tx)
                .await;
            {
                let state = actor.state.lock().await;
                assert!(
                    state.pending_notifications.is_empty(),
                    "the notification must be taken from the queue"
                );
                assert!(
                    state.running_task.is_none(),
                    "a goal-turn-origin completion must be DROPPED, not surfaced as a turn"
                );
            }
            assert!(
                already_reported(&actor, "bg-goal").await,
                "the dropped completion must be marked reported so it can't resurface"
            );
        })
        .await;
}
/// Regression: a harness verifier subagent's reparented server is suppressed even when the goal flips to Blocked/None before the reparent lands.
/// The gate on the reparent record path is the stable harness flag, not the racy `Active` status.
#[tokio::test(flavor = "current_thread")]
async fn reparented_harness_subagent_task_suppressed_when_status_not_active() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            set_goal_harness_for_tests(&actor);
            actor.record_goal_turn_task_ids(["bg-skeptic".to_string()]);
            assert!(
                !actor.goal_turn_task_ids.lock().contains("bg-skeptic"),
                "the active-goal record path is correctly a no-op when not Active"
            );
            actor.record_reparented_goal_turn_task_ids(["bg-skeptic".to_string()]);
            {
                let mut state = actor.state.lock().await;
                state
                    .pending_notifications
                    .push(bash_completed_notification("bg-skeptic"));
            }
            let (completion_tx, _completion_rx) =
                tokio::sync::mpsc::unbounded_channel::<TurnCompletionMsg>();
            std::sync::Arc::clone(&actor)
                .maybe_drain_notifications(completion_tx)
                .await;
            let state = actor.state.lock().await;
            assert!(
                state.pending_notifications.is_empty(),
                "the reparented harness server's completion must be taken"
            );
            assert!(
                state.running_task.is_none(),
                "a final-round skeptic's leftover server must be DROPPED even at status None"
            );
        })
        .await;
}
/// The reparent record path is gated on the (stable) goal harness flag, so it is a no-op in a non-goal session.
#[tokio::test(flavor = "current_thread")]
async fn reparented_record_is_noop_without_goal_harness() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.record_reparented_goal_turn_task_ids(["bg-user".to_string()]);
            assert!(
                actor.goal_turn_task_ids.lock().is_empty(),
                "no goal harness => reparented ids are not recorded (no over-suppression)"
            );
        })
        .await;
}
/// Three children finished in one idle window before the first wake ran.
#[tokio::test(flavor = "current_thread")]
async fn wake_turn_digest_coalesces_unreported_siblings() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            drain_persistence(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let ids = ["sa-1", "sa-2", "sa-3"];
            let fake = spawn_fake_coordinator(
                &mut actor,
                ids.iter().map(|id| subagent_summary(id)).collect(),
            );
            let actor = std::sync::Arc::new(actor);
            run_wake_turn_to_commit(&actor, "sa-1").await;
            assert_eq!(
                fake.peeks(),
                1,
                "the wake turn reads the buffer exactly once"
            );
            assert!(
                fake.returned().is_empty(),
                "the wake turn must not drain the coordinator: {:?}",
                fake.returned()
            );
            assert_eq!(
                fake.suppress_seen(),
                [ids.map(str::to_owned)],
                "the wake turn's own drain leaves every digested completion buffered"
            );
            actor.drain_between_turn_completions(&[]).await;
            assert_eq!(
                fake.returned(),
                ids,
                "the next drain hands the committed copies back"
            );
            let conversation = actor.chat_state_handle.get_conversation().await;
            let mentions: Vec<&ConversationItem> = conversation
                .iter()
                .filter(|i| {
                    let text = i.text_content();
                    ids.iter().any(|id| text.contains(id))
                })
                .collect();
            assert!(
                matches!(
                    mentions.as_slice(),
                    [ConversationItem::User(u)]
                        if u.synthetic_reason == SyntheticReason::SubagentCompleted
                ),
                "only the wake turn may name the children: {mentions:?}"
            );
            let Some(digest) = mentions.first().map(|m| m.text_content()) else {
                panic!("only the wake turn may name the children: {mentions:?}");
            };
            assert!(
                digest.contains("While you were idle"),
                "the wake turn's message must be the digest: {digest}"
            );
            for id in ids {
                assert!(digest.contains(id), "digest must name {id}: {digest}");
                assert!(
                    already_reported(&actor, id).await,
                    "{id} must be marked reported at the digest's commit"
                );
            }
        })
        .await;
}
/// A reported wake is queued ahead of an unreported sibling.
#[tokio::test(flavor = "current_thread")]
async fn promotion_drops_reported_wake_without_running_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.mark_completions_reported(&["sa-dup"]).await;
            let (respond_to, mut dup_rx) = oneshot::channel();
            let _ = actor
                .queue_input(QueueInputRequest {
                    verbatim: true,
                    ..queue_input_request(vec![], "subagent-completed-sa-dup", respond_to)
                })
                .await;
            let (respond_to, mut live_rx) = oneshot::channel();
            let _ = actor
                .queue_input(QueueInputRequest {
                    verbatim: true,
                    ..queue_input_request(vec![], "subagent-completed-sa-live", respond_to)
                })
                .await;
            let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
            actor.clone().maybe_start_running_task(completion_tx).await;
            {
                let mut state = actor.state.try_lock().expect("uncontended test state");
                assert_eq!(
                    state.running_prompt_id(),
                    Some("subagent-completed-sa-live"),
                    "the unreported sibling behind the dead wake must be promoted"
                );
                let queued: Vec<&str> = state
                    .pending_inputs
                    .iter()
                    .map(|i| i.prompt_id.as_str())
                    .collect();
                assert_eq!(queued, ["subagent-completed-sa-live"]);
                if let Some(task) = state.running_task.take() {
                    task.abort();
                }
            }
            assert!(
                matches!(
                    dup_rx.try_recv(),
                    Ok(Ok(crate::session::commands::PromptTurnOk {
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        ..
                    }))
                ),
                "the dropped wake must resolve as removed from the queue"
            );
            assert!(
                matches!(live_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
                "the promoted wake must not be resolved as removed"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn preempted_wake_is_redelivered_by_next_between_turn_drain() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let fake = spawn_fake_coordinator(&mut actor, vec![subagent_summary("sa-pre")]);
            actor.state.lock().await.running_task = Some(running_task_stub("user-running"));
            let (respond_to, _wake_rx) = oneshot::channel();
            let _ = actor
                .queue_input(QueueInputRequest {
                    verbatim: true,
                    ..queue_input_request(vec![], "subagent-completed-sa-pre", respond_to)
                })
                .await;
            assert!(
                !already_reported(&actor, "sa-pre").await,
                "a queued wake is not delivered yet and must not be marked reported"
            );
            let (respond_to, _user_rx) = oneshot::channel();
            let _ = actor
                .queue_input(queue_input_request(vec![], "user-typed", respond_to))
                .await;
            let remaining: Vec<String> = actor
                .state
                .lock()
                .await
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.clone())
                .collect();
            assert_eq!(remaining, ["user-typed"]);
            assert!(
                !already_reported(&actor, "sa-pre").await,
                "a dropped wake must leave the ledger untouched"
            );
            actor.drain_between_turn_completions(&[]).await;
            assert_eq!(fake.suppress_seen(), [Vec::<String>::new()]);
            assert_eq!(fake.returned(), ["sa-pre"]);
            let conversation = actor.chat_state_handle.get_conversation().await;
            let digests: Vec<&ConversationItem> = conversation
                .iter()
                .filter(|i| i.text_content().contains("sa-pre"))
                .collect();
            assert!(
                matches!(
                    digests.as_slice(),
                    [ConversationItem::User(u)]
                        if u.synthetic_reason == SyntheticReason::SystemReminder
                ),
                "the dropped wake must be redelivered as exactly one digest: {digests:?}"
            );
            assert!(already_reported(&actor, "sa-pre").await);
        })
        .await;
}
/// Disk-full refuses the wake before the peek.
#[tokio::test(flavor = "current_thread")]
async fn refused_wake_leaves_completion_unreported() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = disk_full_actor().await;
            let fake = spawn_fake_coordinator(&mut actor, vec![subagent_summary("sa-refused")]);
            let actor = std::sync::Arc::new(actor);
            run_wake_turn(&actor, "sa-refused", None)
                .await
                .expect_err("latched disk-full must refuse the wake");
            assert_eq!(
                fake.peeks(),
                0,
                "the refusal comes before the buffer is read"
            );
            assert!(
                !already_reported(&actor, "sa-refused").await,
                "a wake the model never saw must stay unreported"
            );
            actor.drain_between_turn_completions(&[]).await;
            assert_eq!(fake.returned(), ["sa-refused"]);
            let conversation = actor.chat_state_handle.get_conversation().await;
            assert!(
                conversation
                    .iter()
                    .any(|i| i.text_content().contains("sa-refused")),
                "the refused wake's completion must be redelivered"
            );
            assert!(already_reported(&actor, "sa-refused").await);
        })
        .await;
}
/// Backstop behind the promotion-time drop.
#[tokio::test(flavor = "current_thread")]
async fn already_reported_wake_ends_turn_without_sampling() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let fake = spawn_fake_coordinator(&mut actor, vec![subagent_summary("sa-dup")]);
            actor.mark_completions_reported(&["sa-dup"]).await;
            let actor = std::sync::Arc::new(actor);
            let conversation_len_before = actor.chat_state_handle.get_conversation().await.len();
            let result = run_wake_turn(&actor, "sa-dup", None).await;
            assert!(
                matches!(
                    result,
                    Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        completion_kind: PromptCompletionKind::Completed,
                        ..
                    })
                ),
                "a reported wake ends the turn silently: {result:?}"
            );
            assert_eq!(
                actor.chat_state_handle.get_conversation().await.len(),
                conversation_len_before,
                "a silent wake pushes no message"
            );
            assert!(fake.returned().is_empty(), "a silent wake drains nothing");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn wake_missing_from_buffer_falls_back_to_injected_body() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            drain_gateway(gateway_rx);
            let (persistence_tx, persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            drain_persistence(persistence_rx);
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let fake = spawn_fake_coordinator(&mut actor, vec![]);
            let actor = std::sync::Arc::new(actor);
            run_wake_turn_to_commit(&actor, "sa-evicted").await;
            assert_eq!(fake.peeks(), 1);
            let conversation = actor.chat_state_handle.get_conversation().await;
            let mentions: Vec<&ConversationItem> = conversation
                .iter()
                .filter(|i| i.text_content().contains("sa-evicted"))
                .collect();
            assert!(
                matches!(
                    mentions.as_slice(),
                    [ConversationItem::User(u)]
                        if u.synthetic_reason == SyntheticReason::SubagentCompleted
                ),
                "the wake turn carries exactly one message: {mentions:?}"
            );
            assert_eq!(
                mentions.first().map(|m| m.text_content()).as_deref(),
                Some(wake_body("sa-evicted")).as_deref(),
                "without a buffered copy the injected body is what the model sees"
            );
            assert!(already_reported(&actor, "sa-evicted").await);
            assert_eq!(
                fake.suppress_seen(),
                [["sa-evicted"]],
                "the body path still runs the normal drain, suppressing only its own id"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn wake_turn_under_goal_loop_is_silent_and_marks() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            set_goal_harness_for_tests(&actor);
            actor.goal_tracker.lock().create_goal(
                "g".into(),
                "obj".into(),
                None,
                0,
                "2026-01-01T00:00:00Z".into(),
                None,
            );
            assert!(actor.goal_loop_active(), "precondition: goal loop active");
            let fake = spawn_fake_coordinator(
                &mut actor,
                vec![
                    subagent_summary("sa-goal"),
                    subagent_summary("sa-goal-sibling"),
                ],
            );
            let actor = std::sync::Arc::new(actor);
            let conversation_len_before = actor.chat_state_handle.get_conversation().await.len();
            let result = run_wake_turn(&actor, "sa-goal", None).await;
            assert!(
                matches!(
                    result,
                    Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::EndTurn,
                        total_tokens: 0,
                        ..
                    })
                ),
                "a wake under the goal loop ends silently: {result:?}"
            );
            assert_eq!(
                actor.chat_state_handle.get_conversation().await.len(),
                conversation_len_before
            );
            for id in ["sa-goal", "sa-goal-sibling"] {
                assert!(
                    already_reported(&actor, id).await,
                    "{id} must be dropped as reported"
                );
            }
            assert!(fake.returned().is_empty());
        })
        .await;
}
/// That gate is the shared `Arc` the notification bridge (bash auto-wake) and subagent spawn contexts read.
/// Both the `true` set and the `false` reset funnel through this single method, so this also covers the reset paths.
/// Deleting the `store` line (or cloning the wrong Arc into the bridge) would break production suppression silently.
#[tokio::test(flavor = "current_thread")]
async fn set_goal_loop_active_resource_mirrors_into_gate() {
    use std::sync::atomic::Ordering::Relaxed;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(!actor.tool_context.goal_loop_active_gate.load(Relaxed));
            actor.set_goal_loop_active_resource(true).await;
            assert!(
                actor.tool_context.goal_loop_active_gate.load(Relaxed),
                "set(true) must mirror into the shared gate"
            );
            actor.set_goal_loop_active_resource(false).await;
            assert!(
                !actor.tool_context.goal_loop_active_gate.load(Relaxed),
                "set(false) must clear the shared gate"
            );
        })
        .await;
}
/// It lets the bash arm of the between-turn drain (`drain_between_turn_bash_completions` calling `list_tasks`) run without a real background command.
#[derive(Debug)]
struct OneTaskTerminal {
    tasks: Vec<distill_tools::computer::types::TaskSnapshot>,
}
#[async_trait::async_trait]
impl distill_tools::computer::types::TerminalBackend for OneTaskTerminal {
    async fn run(
        &self,
        _: distill_tools::computer::types::TerminalRunRequest,
    ) -> Result<
        distill_tools::computer::types::TerminalRunResult,
        distill_tools::computer::types::ComputerError,
    > {
        unimplemented!()
    }
    async fn run_background(
        &self,
        _: distill_tools::computer::types::TerminalRunRequest,
    ) -> Result<
        distill_tools::computer::types::BackgroundHandle,
        distill_tools::computer::types::ComputerError,
    > {
        unimplemented!()
    }
    async fn get_task(&self, _: &str) -> Option<distill_tools::computer::types::TaskSnapshot> {
        None
    }
    async fn kill_task(&self, _: &str) -> distill_tools::computer::types::KillOutcome {
        distill_tools::computer::types::KillOutcome::NotFound
    }
    async fn wait_for_completion(
        &self,
        _: &str,
        _: Option<std::time::Duration>,
    ) -> Option<distill_tools::computer::types::TaskSnapshot> {
        None
    }
    async fn list_tasks(&self) -> Vec<distill_tools::computer::types::TaskSnapshot> {
        self.tasks.clone()
    }
}
fn completed_bash_task(id: &str) -> distill_tools::computer::types::TaskSnapshot {
    distill_tools::computer::types::TaskSnapshot {
        task_id: id.into(),
        command: "echo done".into(),
        display_command: None,
        cwd: String::new(),
        start_time: std::time::SystemTime::now(),
        end_time: Some(std::time::SystemTime::now()),
        output: String::new(),
        output_file: std::path::PathBuf::new(),
        truncated: false,
        exit_code: Some(0),
        signal: None,
        completed: true,
        kind: Default::default(),
        block_waited: false,
        explicitly_killed: false,
        kill_result_delivered: false,
        owner_session_id: None,
        description: None,
        is_backgrounded: false,
        output_total_bytes: 0,
    }
}
/// It exercises the production computation the leader's idle-unload decision depends on, rather than the test fake actor.
#[tokio::test(flavor = "current_thread")]
async fn state_is_busy_reflects_queued_inputs() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            {
                let state = actor.state.lock().await;
                assert!(
                    !state_is_busy(&state),
                    "an idle actor (no turn, empty queue) must report not busy"
                );
            }
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(user_input("queued-1"));
                assert!(
                    state_is_busy(&state),
                    "a non-empty pending_inputs queue must report busy"
                );
            }
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.clear();
                assert!(
                    !state_is_busy(&state),
                    "clearing the queue must return to not busy"
                );
            }
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn is_busy_reflects_active_work() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(
                !actor.is_busy().await,
                "a fresh actor (no turn, no queue, no work) must not be busy"
            );
            let guard = crate::session::handle::WorkGuard::new(actor.active_work.clone());
            assert!(
                actor.is_busy().await,
                "a work unit in flight must keep the session busy"
            );
            drop(guard);
            assert!(
                !actor.is_busy().await,
                "dropping the last work unit returns to idle"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn is_busy_reflects_parked_plan_approval() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(!actor.is_busy().await, "a fresh actor must not be busy");
            actor.pending_interactions.lock().unwrap().insert(
                "exit-plan-mode-resume".to_string(),
                crate::session::pending_interaction::PendingKind::PlanApproval,
            );
            assert!(
                actor.is_busy().await,
                "a parked plan-approval must keep the session busy"
            );
            actor.pending_interactions.lock().unwrap().clear();
            actor.pending_interactions.lock().unwrap().insert(
                "perm-1".to_string(),
                crate::session::pending_interaction::PendingKind::Permission,
            );
            assert!(
                !actor.is_busy().await,
                "a bare permission park must not by itself keep the session busy"
            );
        })
        .await;
}
/// Regression: `InjectNotification` must gate on THIS session's turn, not the agent-wide flag.
/// In a multi-session process (dashboard, leader) another session's turn kept the shared flag `true`,
/// so an idle session parked its monitor events in the mid-turn buffer, where they sat unseen until its
/// next user prompt. The event must instead become a pending notification that wakes the idle session.
#[tokio::test(flavor = "current_thread")]
async fn monitor_event_for_idle_session_is_not_parked_while_another_session_runs_a_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                distill_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let (mut actor, event_rx) = create_test_actor_ex(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
            let agent_wide_turn_active = std::sync::Arc::new(
                std::sync::atomic::AtomicBool::new(true),
            );
            let shared_buffer = distill_tools::implementations::distill::monitor::types::MonitorEventBuffer::new();
            actor.tool_context.is_turn_active = Some(agent_wide_turn_active);
            actor.tool_context.monitor_event_buffer = Some(shared_buffer.clone());
            actor.state.lock().await.notifications_suppressed = true;
            let actor = std::sync::Arc::new(actor);
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<
                SessionCommand,
            >();
            let (_chat_tx, chat_rx) = tokio::sync::mpsc::unbounded_channel::<
                distill_chat_state::ChatStateEvent,
            >();
            let codebase_indexes = std::sync::Arc::new(
                parking_lot::Mutex::new(
                    distill_workspace::file_system::CodebaseIndexManager::new(),
                ),
            );
            tokio::task::spawn_local(
                super::run_session(
                    actor.clone(),
                    cmd_rx,
                    chat_rx,
                    event_rx,
                    None,
                    codebase_indexes,
                    std::path::PathBuf::from("/tmp"),
                    crate::session::fs_watch::FsWatchCapabilities::none(),
                ),
            );
            cmd_tx
                .send(SessionCommand::InjectNotification {
                    prompt_id: "monitor-idle".to_string(),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "<monitor-event task_id=\"watch-1\">\nnew message\n</monitor-event>",
                    ))],
                    priority: NotificationPriority::Next,
                    source: NotificationSource::MonitorEvent {
                        task_id: "watch-1".to_string(),
                    },
                })
                .expect("run_session must be receiving commands");
            for _ in 0..100 {
                if !actor.state.lock().await.pending_notifications.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert!(
                shared_buffer.is_empty(),
                "an idle session must not park its monitor event in the mid-turn buffer because another session is mid-turn"
            );
            let state = actor.state.lock().await;
            assert_eq!(
                state
                    .pending_notifications
                    .iter()
                    .map(|n| n.source.task_id())
                    .collect::<Vec<_>>(),
                vec!["watch-1"],
                "the monitor event must queue as a wake for this idle session"
            );
        })
        .await;
}
/// The mid-turn buffer is still the right place when THIS session is the one running a turn.
#[tokio::test(flavor = "current_thread")]
async fn monitor_event_during_own_turn_is_buffered_for_the_turn_loop() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) =
                tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let (mut actor, event_rx) =
                create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let shared_buffer =
                distill_tools::implementations::distill::monitor::types::MonitorEventBuffer::new();
            actor.tool_context.monitor_event_buffer = Some(shared_buffer.clone());
            actor
                .session_turn_active
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let actor = std::sync::Arc::new(actor);
            let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<SessionCommand>();
            let (_chat_tx, chat_rx) =
                tokio::sync::mpsc::unbounded_channel::<distill_chat_state::ChatStateEvent>();
            let codebase_indexes = std::sync::Arc::new(parking_lot::Mutex::new(
                distill_workspace::file_system::CodebaseIndexManager::new(),
            ));
            tokio::task::spawn_local(super::run_session(
                actor.clone(),
                cmd_rx,
                chat_rx,
                event_rx,
                None,
                codebase_indexes,
                std::path::PathBuf::from("/tmp"),
                crate::session::fs_watch::FsWatchCapabilities::none(),
            ));
            cmd_tx
                .send(SessionCommand::InjectNotification {
                    prompt_id: "monitor-busy".to_string(),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "<monitor-event task_id=\"watch-2\">\nnew message\n</monitor-event>",
                    ))],
                    priority: NotificationPriority::Next,
                    source: NotificationSource::MonitorEvent {
                        task_id: "watch-2".to_string(),
                    },
                })
                .expect("run_session must be receiving commands");
            for _ in 0..100 {
                if !shared_buffer.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                shared_buffer.len(),
                1,
                "own-turn monitor events go to the turn loop's buffer"
            );
            assert!(
                actor.state.lock().await.pending_notifications.is_empty(),
                "own-turn monitor events must not also queue a wake"
            );
        })
        .await;
}
