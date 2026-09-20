// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
use super::*;
use crate::session::SessionThread;
use super::spawn::{
    inject_subagent_completed_prompt, join_worker_task, present_child_completion,
    should_auto_wake_subagent, will_wake_for, AutoWakeInputs, InjectParams,
};
use super::prompt_turn_receipt::{
    PromptTurnReceiptDisposition, PromptTurnSettlementInput,
    reduce_prompt_turn_settlement,
};
use super::attempt_runner::{
    OneTurnAttemptInput, canonical_total_tokens, record_subagent_usage,
    run_one_turn_attempt, usage_is_incomplete,
};
use super::handle_request::{
    CHILD_ACTOR_ACK_TIMEOUT, PARENT_ACK_TIMEOUT, agent_memory_scope_for_mode,
    child_actor_query, mark_child_usage_not_applied_with_fallback,
    reparent_surviving_child_tasks, resolve_child_model, take_child_streaming_partial,
    take_child_turn_messages,
};
use crate::test_support::lsp_runtime::{ctx_with_toggle, test_gateway_with_receiver};
use distill_subagent_resolution::resolve_effective_overrides;
use distill_tools::implementations::distill::task::coordinator::{
    ChildCompletion, CompletionDisposition,
};
use distill_tools::implementations::distill::task::terminal_snapshot;
use distill_tools::reminders::task_completion::INLINE_SUBAGENT_OUTPUT_BYTES;
fn test_snapshot(
    request: &SubagentRequest,
    result: &SubagentResult,
) -> SubagentSnapshot {
    terminal_snapshot(request, result, None, None, 0)
}
#[test]
fn v2_disables_legacy_agent_memory_scope() {
    let scope = Some(distill_agent::config::MemoryScope::Project);
    assert_eq!(
            agent_memory_scope_for_mode(scope, crate::config::MemoryMode::V2),
            None
        );
    assert_eq!(
            agent_memory_scope_for_mode(scope, crate::config::MemoryMode::Legacy),
            scope
        );
}
#[test]
fn canonical_total_tokens_does_not_double_count_reasoning() {
    let totals = distill_chat_state::UsageTotals {
        input_tokens: 100,
        output_tokens: 40,
        reasoning_tokens: 25,
        ..Default::default()
    };
    assert_eq!(canonical_total_tokens(&totals), 140);
}
#[test]
fn cancellation_makes_an_otherwise_complete_usage_snapshot_incomplete() {
    assert!(usage_is_incomplete(false, true));
    assert!(!usage_is_incomplete(false, false));
    assert!(usage_is_incomplete(true, false));
}
#[tokio::test]
async fn usage_ack_precedes_terminal_presentation() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let by_model = vec![(
            "test-model".to_string(),
            distill_chat_state::UsageTotals {
                input_tokens: 10,
                output_tokens: 4,
                ..Default::default()
            },
        )];
    let mut fold = Box::pin(
        record_subagent_usage(
            ctx.parent_cmd_tx.as_ref(),
            Some(by_model),
            Some("parent-prompt".to_string()),
            false,
        ),
    );
    let command = tokio::select! {
            command = parent_cmd_rx.recv() => command.expect("usage command"),
            result = &mut fold => panic!("usage fold returned before parent command: {result}"),
        };
    let SessionCommand::RecordSubagentUsage { respond_to, .. } = command else {
        panic!("expected RecordSubagentUsage");
    };
    assert!(
            tokio::time::timeout(std::time::Duration::ZERO, &mut fold)
                .await
                .is_err(),
            "child return must wait for usage acknowledgement"
        );
    assert!(parent_cmd_rx.try_recv().is_err());
    respond_to.send(()).expect("usage ack");
    assert!(fold.await);
    let (gateway, _gateway_rx) = test_gateway_with_receiver();
    let mut request = auto_wake_test_request("usage-order");
    request.run_in_background = false;
    let completion_data = ShellCompletionData::from_context(
        &ctx,
        distill_message_delivery_core::AttemptId::mint(1),
        None,
    );
    completion_data.mark_spawned_notification_emitted();
    let result = SubagentResult {
        success: true,
        subagent_id: "usage-order".to_string(),
        child_session_id: "usage-order".to_string(),
        ..Default::default()
    };
    let completion = ChildCompletion {
        snapshot: test_snapshot(&request, &result),
        request,
        result,
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: true,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    assert!(matches!(
            parent_cmd_rx.try_recv(),
            Ok(SessionCommand::XaiSessionNotification {
                notification: SessionNotification {
                    update: SessionUpdate::SubagentFinished { .. },
                    ..
                }
            })
        ));
}
/// Regression: reads of the child session's own actors (chat state, signals) must not park teardown when the child thread is synchronously blocked — the actors share its current-thread runtime — and must degrade to the cheap fallback in bounded time.
#[tokio::test(start_paused = true)]
async fn child_actor_query_is_bounded_when_the_actor_never_answers() {
    let tokens = tokio::time::timeout(
            20 * CHILD_ACTOR_ACK_TIMEOUT,
            child_actor_query("test_query", std::future::pending::<u64>(), 7),
        )
        .await
        .expect("child-actor queries must complete in bounded time");
    assert_eq!(tokens, 7, "a starved query must degrade to the fallback");
}
/// Regression: a wedged child actor that never answers `TakeTurnMessages`
/// must not park teardown ahead of the bounded upload set, and the
/// timed-out take must surface as the recorded miss, not an empty turn.
#[tokio::test(start_paused = true)]
async fn turn_message_take_is_bounded_and_records_the_wedge() {
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let taken = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            take_child_turn_messages(&cmd_tx, deadline),
        )
        .await
        .expect("the take must be bounded under a wedged child actor");
    assert!(matches!(
            taken,
            crate::upload::turn::TurnMessages::Missing(
                crate::upload::turn::MissingTurnMessages::TakeTimedOut
            )
        ));
}
/// The dead-actor takes — command channel closed, or responder dropped
/// without an answer — are recorded misses, not genuinely empty turns.
#[tokio::test(start_paused = true)]
async fn turn_message_take_records_the_miss_when_the_actor_is_gone() {
    use crate::upload::turn::{MissingTurnMessages, TurnMessages};
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let (closed_tx, closed_rx) = mpsc::unbounded_channel();
    drop(closed_rx);
    let taken = take_child_turn_messages(&closed_tx, deadline).await;
    assert!(
            matches!(
                taken,
                TurnMessages::Missing(MissingTurnMessages::ChannelDropped)
            ),
            "a closed command channel must not pass for an empty turn"
        );
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let actor = tokio::spawn(async move {
        match cmd_rx.recv().await {
            Some(SessionCommand::TakeTurnMessages { respond_to }) => drop(respond_to),
            _ => panic!("expected TakeTurnMessages"),
        }
    });
    let taken = take_child_turn_messages(&cmd_tx, deadline).await;
    actor.await.expect("actor task");
    assert!(
            matches!(
                taken,
                TurnMessages::Missing(MissingTurnMessages::ChannelDropped)
            ),
            "a dropped responder must not pass for an empty turn"
        );
}
/// A real `SessionHandle` whose child-session actors never answer: the command channel is held open but unserviced and the signals actor is never run — the shape a tool synchronously blocking the child session thread leaves behind. The receiver and actor must stay alive so sends succeed but never get answered.
fn wedged_child_handle() -> (
    SessionHandle,
    mpsc::UnboundedReceiver<SessionCommand>,
    crate::session::signals::SessionSignalsActor,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
    let (hunk_event_tx, _hunk_event_rx) = mpsc::unbounded_channel();
    let hunk_tracker_handle = distill_hunk_tracker::HunkTrackerActor::spawn(
        "test".to_string(),
        PathBuf::from("/tmp"),
        hunk_event_tx,
        distill_hunk_tracker::TrackingMode::AllDirty,
        CancellationToken::new(),
    );
    let (signals_handle, signals_actor) = crate::session::signals::SessionSignalsActor::new();
    let handle = SessionHandle {
        cmd_tx,
        persistence_tx,
        registry_write_order: Default::default(),
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        info: SessionInfo {
            id: acp::SessionId::new("test"),
            cwd: "/tmp".to_string(),
        },
        max_turns: None,
        resolved_tool_overrides: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        spawn_snapshot: crate::session::SpawnSnapshot {
            applied_tool_overrides: None,
            memory_mode: None,
        },
        hunk_tracker_handle,
        chat_state_handle: distill_chat_state::ChatStateHandle::noop(),
        signals_handle,
        gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        emit_local_background_tasks: std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true),
        ),
        client_caps: crate::session::notifications::SessionClientCaps::new(false, true),
        mcp_servers: vec![],
        initial_client_mcp_servers: vec![],
        display_cwd: None,
        feedback_manager: std::sync::Arc::new(
            crate::session::feedback_manager::FeedbackManager::local_only("test"),
        ),
        upload_queue: std::sync::Arc::new(std::sync::OnceLock::new()),
        upload_failures_since_success: std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(0),
        ),
        tool_context: crate::tools::ToolContext::new_local_context(
            distill_paths::AbsPathBuf::new(PathBuf::from("/tmp")).unwrap(),
            std::sync::Arc::new(
                distill_workspace::file_system::LocalFs::new(PathBuf::from("/tmp")),
            ),
            std::sync::Arc::new(crate::terminal::LocalTerminalRunner),
        ),
        model_id: acp::ModelId::new("test-model"),
        reasoning_effort: None,
        jev_effort_auto: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        yolo_mode: false,
        origin_client: None,
        code_nav_enabled: false,
        ask_user_question_enabled: true,
        non_interactive: false,
        plan_mode: std::sync::Arc::new(
            parking_lot::Mutex::new(
                crate::session::plan_mode::PlanModeTracker::new(PathBuf::from("/tmp")),
            ),
        ),
        force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        permission_handle: distill_workspace::permission::PermissionHandle::allow_all(),
        attribution_callback: None,
        agent_name: "Distill".to_string(),
        managed_mcp_proxy_base_url: String::new(),
        session_default_agent_profile: None,
        allowed_subagent_types: None,
        hook_registry: None,
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        terminal_backend: None,
        tools_notification_handle: None,
        scheduler_handle: None,
    };
    (handle, cmd_rx, signals_actor)
}
/// Regression: a cancel that cannot read the child's signals (wedged actor) must fail closed — "no answer" is not "no work done" — so the usage fold marks the parent's bill incomplete instead of folding a clean ledger over the cancelled turn's in-flight sampling.
#[tokio::test(start_paused = true)]
async fn cancelled_attempt_fails_closed_when_the_signals_read_never_answers() {
    let (child_handle, _cmd_rx, _signals_actor) = wedged_child_handle();
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();
    let request = auto_wake_test_request("cancel-wedge");
    let outcome = tokio::time::timeout(
            20 * CHILD_ACTOR_ACK_TIMEOUT,
            run_one_turn_attempt(OneTurnAttemptInput {
                child_handle: &child_handle,
                request: &request,
                worktree_path: None,
                task_prompt_text: "task",
                prompt_id: uuid::Uuid::now_v7().to_string(),
                inherited_tool_overrides: None,
                gcs_bucket_url: None,
                gcs_upload_method: None,
                turn_number: 0,
                cancel_token,
                child_run_started_at: std::time::Instant::now(),
                prompt_admitted: tokio::sync::oneshot::channel().0,
                initial_attempt_behavior: InitialAttemptBehavior::Normal,
            }),
        )
        .await
        .expect(
            "a cancelled attempt must complete in bounded time under a wedged child",
        );
    assert!(outcome.result.cancelled);
    assert!(
            outcome.cancellation_may_hide_usage,
            "an unanswered signals read must not pass for 'no work done'"
        );
}
/// Regression: a wedged child actor must not park teardown on the
/// streaming-capture take; the capture is skipped in bounded time.
#[tokio::test(start_paused = true)]
async fn streaming_partial_take_is_bounded_when_the_child_actor_is_wedged() {
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let capture = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            take_child_streaming_partial(
                &cmd_tx,
                deadline,
                "prompt-1".into(),
                false,
                None,
            ),
        )
        .await
        .expect("the streaming take must be bounded under a wedged child actor");
    assert!(capture.is_none(), "a timed-out take skips the capture");
}
/// Regression: the resolved-model read for turn_result.json must degrade
/// to the configured model id in bounded time under a wedged child actor.
#[tokio::test(start_paused = true)]
async fn resolved_model_read_is_bounded_and_falls_back_to_configured() {
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let resolved = tokio::time::timeout(
            std::time::Duration::from_secs(3600),
            resolve_child_model(&cmd_tx, deadline, Some("configured-model".into())),
        )
        .await
        .expect("the model read must be bounded under a wedged child actor");
    assert_eq!(resolved.as_deref(), Some("configured-model"));
}
/// Regression: a completed child's usage fold must not park forever behind
/// a parent actor that never services `cmd_rx`; that leaked the child's
/// session thread, fs watchers, and fds.
#[tokio::test(start_paused = true)]
async fn usage_fold_is_bounded_when_parent_never_services_commands() {
    let (parent_cmd_tx, _parent_cmd_rx) = mpsc::unbounded_channel();
    let by_model = vec![(
            "test-model".to_string(),
            distill_chat_state::UsageTotals {
                input_tokens: 10,
                ..Default::default()
            },
        )];
    let folded = tokio::time::timeout(
            20 * PARENT_ACK_TIMEOUT,
            record_subagent_usage(
                Some(&parent_cmd_tx),
                Some(by_model),
                Some("parent-prompt".to_string()),
                false,
            ),
        )
        .await
        .expect("usage fold must complete in bounded time under a starved parent");
    assert!(
            !folded,
            "an unacked fold must report a miss so the sticky fallback marks the bill incomplete"
        );
}
/// Regression: when the parent never acks, the usage-not-applied mark must
/// fall through to the coordinator's report-level sticky in bounded time.
#[tokio::test(start_paused = true)]
async fn usage_not_applied_mark_falls_back_to_coordinator_when_parent_is_starved() {
    let (parent_cmd_tx, _parent_cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mark = mark_child_usage_not_applied_with_fallback(
        Some(&parent_cmd_tx),
        &event_tx,
        "parent-sess",
        Some("prompt-1".to_string()),
    );
    let coordinator = async {
        let event = event_rx.recv().await.expect("coordinator fallback event");
        let SubagentEvent::MarkUsageNotApplied(req) = event else {
            panic!("expected MarkUsageNotApplied");
        };
        assert_eq!(req.parent_session_id, "parent-sess");
        assert_eq!(req.prompt_id, "prompt-1");
        let _ = req.respond_to.send(());
    };
    tokio::time::timeout(
            20 * PARENT_ACK_TIMEOUT,
            async { tokio::join!(mark, coordinator) },
        )
        .await
        .expect(
            "usage-not-applied mark must complete in bounded time under a starved parent",
        );
}
/// A parent that acks the mark in time owns it: the coordinator fallback
/// must not fire (a double mark would double-report the sticky).
#[tokio::test]
async fn usage_not_applied_mark_skips_coordinator_when_parent_acks() {
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mark = mark_child_usage_not_applied_with_fallback(
        Some(&parent_cmd_tx),
        &event_tx,
        "parent-sess",
        Some("prompt-1".to_string()),
    );
    let parent = async {
        match parent_cmd_rx.recv().await.expect("parent mark command") {
            SessionCommand::MarkSubagentUsageNotApplied { respond_to, .. } => {
                let _ = respond_to.send(());
            }
            _ => panic!("expected MarkSubagentUsageNotApplied"),
        }
    };
    tokio::join!(mark, parent);
    assert!(
            event_rx.try_recv().is_err(),
            "a parent-acked mark must not also mark the coordinator sticky"
        );
}
/// With no parent command channel the mark goes straight to the
/// coordinator's report-level sticky.
#[tokio::test]
async fn usage_not_applied_mark_goes_straight_to_coordinator_without_parent() {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mark = mark_child_usage_not_applied_with_fallback(
        None,
        &event_tx,
        "parent-sess",
        Some("prompt-1".to_string()),
    );
    let coordinator = async {
        let event = event_rx.recv().await.expect("coordinator fallback event");
        let SubagentEvent::MarkUsageNotApplied(req) = event else {
            panic!("expected MarkUsageNotApplied");
        };
        assert_eq!(req.parent_session_id, "parent-sess");
        assert_eq!(req.prompt_id, "prompt-1");
        let _ = req.respond_to.send(());
    };
    tokio::join!(mark, coordinator);
}
/// Terminal backend whose actor never answers — models a parent terminal
/// actor starved by a busy turn.
struct StarvedTerminal;
#[async_trait::async_trait]
impl distill_tools::computer::types::TerminalBackend for StarvedTerminal {
    async fn run(
        &self,
        _request: distill_tools::computer::types::TerminalRunRequest,
    ) -> Result<
        distill_tools::computer::types::TerminalRunResult,
        distill_tools::computer::types::ComputerError,
    > {
        std::future::pending().await
    }
    async fn run_background(
        &self,
        _request: distill_tools::computer::types::TerminalRunRequest,
    ) -> Result<
        distill_tools::computer::types::BackgroundHandle,
        distill_tools::computer::types::ComputerError,
    > {
        std::future::pending().await
    }
    async fn get_task(
        &self,
        _task_id: &str,
    ) -> Option<distill_tools::computer::types::TaskSnapshot> {
        std::future::pending().await
    }
    async fn kill_task(
        &self,
        _task_id: &str,
    ) -> distill_tools::computer::types::KillOutcome {
        std::future::pending().await
    }
    async fn wait_for_completion(
        &self,
        _task_id: &str,
        _timeout: Option<std::time::Duration>,
    ) -> Option<distill_tools::computer::types::TaskSnapshot> {
        std::future::pending().await
    }
    async fn list_tasks(&self) -> Vec<distill_tools::computer::types::TaskSnapshot> {
        std::future::pending().await
    }
    async fn reparent_notifications(
        &self,
        _old_owner_session_id: &str,
        _new_owner_session_id: &str,
        _new_handle: distill_tools::notification::types::ToolNotificationHandle,
        _backend_weak: std::sync::Weak<
            dyn distill_tools::computer::types::TerminalBackend,
        >,
    ) {
        std::future::pending().await
    }
}
/// Regression: the goal-task snapshot and the notification reparent must not park a completed child before Shutdown when the parent terminal actor never answers; a timed-out snapshot skips goal-turn tagging instead of sending a partial record.
#[tokio::test(start_paused = true)]
async fn reparent_is_bounded_when_terminal_actor_never_answers() {
    let parent_tb: std::sync::Arc<
        dyn distill_tools::computer::types::TerminalBackend,
    > = std::sync::Arc::new(StarvedTerminal);
    let notif_handle = distill_tools::notification::types::ToolNotificationHandle::noop();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
            20 * PARENT_ACK_TIMEOUT,
            reparent_surviving_child_tasks(
                &parent_tb,
                &notif_handle,
                Some(&cmd_tx),
                "child-sess",
                "parent-sess",
                false,
            ),
        )
        .await
        .expect("teardown must reach Shutdown under a starved terminal actor");
    assert!(
            cmd_rx.try_recv().is_err(),
            "a timed-out list_tasks must not record goal-turn task ids"
        );
}
/// Invariant: resolving a subagent applies the parent session's `--tools`/`--disallowed-tools`/`--permission-mode` — driven through `resolve_agent_definition` so the spawn path can't skip them.
/// Invariant: resolving a subagent applies the parent session's `--tools`/`--disallowed-tools`/`--permission-mode`.
/// They flow through `resolve_agent_definition` so the spawn path can't skip them.
#[tokio::test]
async fn subagent_inherits_session_cli_overrides() {
    use distill_agent::config::{AgentDefinition, PermissionMode};
    let mut probe = AgentDefinition::general_purpose();
    probe.name = "session-override-probe".into();
    probe.permission_mode = PermissionMode::Plan;
    probe.disallowed_tools = vec!["write".into()];
    let mut config = crate::agent::config::Config::default();
    config.cli_agents = vec![probe];
    config.cli_agent_overrides = crate::agent::config::CliAgentOverrides {
        tools: Some(vec!["read_file".into(), "grep".into()]),
        disallowed_tools: Some(vec!["web_search".into(), "write".into()]),
        permission_mode: Some(PermissionMode::AcceptEdits),
        ..Default::default()
    };
    let mut ctx = ctx_with_toggle(std::collections::HashMap::new());
    ctx.agent_config = Some(config);
    let def = resolve_agent_definition("session-override-probe", &ctx)
        .expect("cli agent resolves");
    assert_eq!(
            def.session_tools_allowlist.as_deref(),
            Some(["read_file".into(), "grep".into()].as_slice())
        );
    assert_eq!(
            def.session_tools_denylist.as_deref(),
            Some(["web_search".into(), "write".into()].as_slice())
        );
    assert_eq!(def.disallowed_tools, vec!["write"]);
    assert_eq!(def.permission_mode, PermissionMode::AcceptEdits);
}
#[test]
fn subagent_bypass_permission_mode_gated_by_policy_pin() {
    use distill_agent::config::PermissionMode;
    const PIN: &str = distill_workspace::permission::resolution::YoloPinReason::DisableBypassPermissionsMode
        .message();
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false, None),
            PermissionMode::BypassPermissions,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, false, Some(PIN)),
            PermissionMode::Default,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::Plan, false, Some(PIN)),
            PermissionMode::Plan,
        );
    assert_eq!(
            resolve_subagent_permission_mode(PermissionMode::BypassPermissions, true, None),
            PermissionMode::Default,
        );
}
/// The subagent emitter's persist hop (`SessionCommand`) and live broadcast must carry the SAME `eventId`, minted before the fork.
/// Divergent or missing ids degrade cursor reconnects to full replays or re-applied lines.
#[tokio::test]
async fn emit_subagent_notification_stamps_one_event_id_on_both_paths() {
    use crate::test_support::lsp_runtime::test_gateway_with_receiver;
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    emit_subagent_notification(
        &gateway,
        "parent-sess",
        SessionUpdate::SubagentFinished {
            attempt_id: None,
            subagent_id: "sa-1".into(),
            child_session_id: "child-1".into(),
            status: "completed".into(),
            error: None,
            tool_calls: 0,
            turns: 0,
            duration_ms: 5,
            tokens_used: 0,
            output: None,
            will_wake: false,
        },
        Some(&cmd_tx),
    );
    let persisted_id = match cmd_rx.try_recv().expect("persist hop must fire") {
        SessionCommand::XaiSessionNotification { notification } => {
            notification
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("persisted subagent lines must carry an eventId")
                .to_string()
        }
        _ => panic!("expected XaiSessionNotification"),
    };
    assert!(persisted_id.starts_with("parent-sess-"));
    let broadcast_id = match gateway_rx.try_recv().expect("broadcast must fire") {
        distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
            let params: serde_json::Value = serde_json::from_str(
                    args.request.params.get(),
                )
                .unwrap();
            params
                .get("_meta")
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("broadcast must carry eventId")
                .to_string()
        }
        _ => panic!("expected ExtNotification"),
    };
    assert_eq!(persisted_id, broadcast_id);
}
#[test]
fn subagent_max_turns_definition_wins_else_inherits_parent() {
    assert_eq!(super::resolve_subagent_max_turns(Some(2), Some(5)), Some(2));
    assert_eq!(super::resolve_subagent_max_turns(None, Some(5)), Some(5));
}
#[test]
fn resume_worktree_action_covers_three_outcomes() {
    use super::{ResumeWorktreeAction, resume_worktree_action};
    assert_eq!(
            resume_worktree_action(true, Some("refs/grok/subagents/x")),
            ResumeWorktreeAction::Rehydrate
        );
    assert_eq!(
            resume_worktree_action(false, Some("refs/grok/subagents/x")),
            ResumeWorktreeAction::Rehydrate
        );
    assert_eq!(
            resume_worktree_action(true, None),
            ResumeWorktreeAction::Reuse
        );
    assert_eq!(
            resume_worktree_action(false, None),
            ResumeWorktreeAction::Shared
        );
}
#[test]
fn should_auto_wake_subagent_truth_table() {
    let wakeable = AutoWakeInputs {
        run_in_background: true,
        cancelled: false,
        auto_wake_enabled: true,
        block_waited: false,
        explicitly_killed: false,
        goal_loop_active: false,
        parent_channel_open: true,
    };
    assert!(should_auto_wake_subagent(wakeable));
    let suppressed = [
        AutoWakeInputs {
            run_in_background: false,
            ..wakeable
        },
        AutoWakeInputs {
            cancelled: true,
            ..wakeable
        },
        AutoWakeInputs {
            auto_wake_enabled: false,
            ..wakeable
        },
        AutoWakeInputs {
            block_waited: true,
            ..wakeable
        },
        AutoWakeInputs {
            explicitly_killed: true,
            ..wakeable
        },
        AutoWakeInputs {
            goal_loop_active: true,
            ..wakeable
        },
        AutoWakeInputs {
            parent_channel_open: false,
            ..wakeable
        },
    ];
    for (i, inputs) in suppressed.into_iter().enumerate() {
        assert!(!should_auto_wake_subagent(inputs), "suppressed case {i}");
    }
}
fn auto_wake_test_request(id: &str) -> SubagentRequest {
    SubagentRequest {
        id: id.into(),
        prompt: String::new(),
        description: "explore".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: true,
        surface_completion: true,
        await_to_completion: false,
        fork_context: false,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
        spawn_root: Default::default(),
    }
}
fn prompt_text(blocks: &[acp::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            acp::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}
#[test]
fn completed_followup_wakes_parent_with_exactly_one_prompt() {
    let (gateway, _gateway_rx) = test_gateway_with_receiver();
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    let folded = reduce_prompt_turn_settlement(PromptTurnSettlementInput {
        result: SubagentResult {
            subagent_id: "sa-followup".into(),
            child_session_id: "sa-followup".into(),
            turns: 2,
            ..Default::default()
        },
        disposition: PromptTurnReceiptDisposition::Completed,
        final_receipt: Some(Ok(crate::session::commands::ok_end_turn(1, None))),
        final_text: "follow-up result".to_string(),
        was_cancelled: false,
    });
    assert!(folded.result.success);
    assert!(!folded.result.cancelled);
    let request = auto_wake_test_request("sa-followup");
    let completion = ChildCompletion {
        snapshot: test_snapshot(&request, &folded.result),
        request,
        result: folded.result,
        completion_data: ShellCompletionData {
            auto_wake_enabled: true,
            parent_cmd_tx: Some(parent_cmd_tx),
            task_output_tool_name: "get_command_or_subagent_output".into(),
            ..Default::default()
        },
        disposition: CompletionDisposition {
            foreground_delivered: false,
            backgrounded: true,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: true,
        },
    };
    let will_wake = will_wake_for(&completion);
    assert!(will_wake);
    present_child_completion(completion, &gateway, will_wake);
    let mut finish_count = 0;
    let mut prompt_count = 0;
    while let Ok(command) = parent_cmd_rx.try_recv() {
        match command {
            SessionCommand::XaiSessionNotification {
                notification: SessionNotification {
                    update: SessionUpdate::SubagentFinished {
                        status,
                        turns,
                        will_wake,
                        ..
                    },
                    ..
                },
            } => {
                assert_eq!((status.as_str(), turns, will_wake), ("completed", 2, true));
                finish_count += 1;
            }
            SessionCommand::Prompt { prompt_id, .. } => {
                assert!(prompt_id.starts_with("subagent-completed-"));
                prompt_count += 1;
            }
            _ => panic!("unexpected parent command"),
        }
    }
    assert_eq!((finish_count, prompt_count), (1, 1));
}
#[test]
fn inject_subagent_completed_prompt_sends_prompt() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let mut request = auto_wake_test_request("sa-1");
    request.runtime_overrides.loop_task_id = Some("loop-123".into());
    let result = SubagentResult {
        success: true,
        output: std::sync::Arc::from("PING"),
        subagent_id: "sa-1".into(),
        child_session_id: "sa-1".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-1",
        result: &result,
        request: &request,
        snapshot: &test_snapshot(&request, &result),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: Some("renamed_scheduler_delete"),
        scheduler_create_tool_name: Some("renamed_scheduler_create"),
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    match cmd_rx.try_recv().expect("expected synthetic Prompt") {
        SessionCommand::Prompt { prompt_id, prompt_blocks, verbatim, .. } => {
            assert!(prompt_id.starts_with("subagent-completed-"));
            assert!(verbatim);
            let prompt = prompt_text(&prompt_blocks);
            let block = prompt
                .find("\n=== Output ===\nPING\n\n<subagent_meta>")
                .expect("inlined task output");
            let cleanup = prompt
                .find("If this schedule is no longer relevant")
                .expect("cleanup hint");
            assert!(block < cleanup, "task output precedes the hints: {prompt}");
            assert!(prompt.contains(
                    "Check the subagent output using get_command_or_subagent_output(\"sa-1\")"
                ));
            assert!(prompt.contains("renamed_scheduler_delete(\"loop-123\")"));
            assert!(prompt.contains(
                    "renamed_scheduler_create(new_prompt, interval, \"loop-123\")"
                ));
            assert!(!prompt.contains("update it with scheduler_create("));
        }
        _ => panic!("expected SessionCommand::Prompt"),
    }
}
#[test]
fn inject_subagent_completed_prompt_copies_capped_task_output() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let output = "x".repeat(20_000);
    let request = auto_wake_test_request("sa-1");
    let result = SubagentResult {
        success: true,
        output: std::sync::Arc::from(output.as_str()),
        subagent_id: "sa-1".into(),
        child_session_id: "sa-1".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-1",
        result: &result,
        request: &request,
        snapshot: &test_snapshot(&request, &result),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: None,
        scheduler_create_tool_name: None,
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    let SessionCommand::Prompt { prompt_blocks, .. } = cmd_rx
        .try_recv()
        .expect("expected synthetic Prompt") else {
        panic!("expected SessionCommand::Prompt");
    };
    let prompt = prompt_text(&prompt_blocks);
    assert!(prompt.contains("\n=== Task sa-1 ===\n"), "{prompt}");
    assert!(
            prompt.contains(&format!(
                "\n[output truncated: {INLINE_SUBAGENT_OUTPUT_BYTES} of 20000 bytes shown]\n\
                 Use get_command_or_subagent_output(\"sa-1\") to see the full output.\n\n\
                 <subagent_meta>id=sa-1, "
            )),
            "{}",
            prompt
                .len()
                .checked_sub(400)
                .and_then(|i| prompt.get(i..))
                .unwrap_or(prompt.as_str())
        );
    let len = prompt.len();
    let threshold = crate::session::acp_session::LARGE_PROMPT_THRESHOLD;
    assert!(len < threshold, "wake prompt was {len} bytes, threshold {threshold}");
}
#[test]
fn inject_subagent_completed_prompt_omits_cleanup_without_loop_task() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let request = auto_wake_test_request("sa-no-loop");
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-no-loop".into(),
        child_session_id: "sa-no-loop".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-no-loop",
        result: &result,
        request: &request,
        snapshot: &test_snapshot(&request, &result),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: Some("scheduler_delete"),
        scheduler_create_tool_name: Some("scheduler_create"),
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    let SessionCommand::Prompt { prompt_blocks, .. } = cmd_rx
        .try_recv()
        .expect("expected synthetic Prompt") else {
        panic!("expected SessionCommand::Prompt");
    };
    let prompt = prompt_text(&prompt_blocks);
    assert!(!prompt.contains("scheduler_delete"));
    assert!(!prompt.contains("no longer relevant"));
    assert!(!prompt.contains("Check the subagent output"));
}
#[test]
fn inject_subagent_completed_prompt_bails_when_goal_loop_activates_in_gap() {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    let request = auto_wake_test_request("sa-goal");
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-goal".into(),
        child_session_id: "sa-goal".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-goal",
        result: &result,
        request: &request,
        snapshot: &test_snapshot(&request, &result),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: None,
        scheduler_create_tool_name: None,
        synthetic_trace_tx: &None,
        goal_loop_active: &std::sync::atomic::AtomicBool::new(true),
    });
    assert!(cmd_rx.try_recv().is_err(), "no prompt when the goal loop owns the cadence");
}
#[test]
fn inject_subagent_completed_prompt_bails_when_parent_closed() {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCommand>();
    drop(cmd_rx);
    let (trace_tx, mut trace_rx) = mpsc::unbounded_channel();
    let request = auto_wake_test_request("sa-closed");
    let result = SubagentResult {
        success: true,
        subagent_id: "sa-closed".into(),
        child_session_id: "sa-closed".into(),
        ..Default::default()
    };
    inject_subagent_completed_prompt(InjectParams {
        subagent_id: "sa-closed",
        result: &result,
        request: &request,
        snapshot: &test_snapshot(&request, &result),
        parent_cmd_tx: Some(&cmd_tx),
        task_output_tool_name: "get_command_or_subagent_output",
        scheduler_delete_tool_name: None,
        scheduler_create_tool_name: None,
        synthetic_trace_tx: &Some(trace_tx),
        goal_loop_active: &std::sync::atomic::AtomicBool::new(false),
    });
    assert!(trace_rx.try_recv().is_err(), "no prompt was sent, so no trace request follows");
}
#[test]
fn persist_gate_only_persists_successful_nonempty_outputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ok = SubagentResult {
        success: true,
        output: std::sync::Arc::from("text"),
        ..Default::default()
    };
    assert_eq!(
            persist_subagent_output(dir.path(), &ok),
            Some(dir.path().to_path_buf())
        );
    let empty = SubagentResult {
        success: true,
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), &empty), None);
    let failed = SubagentResult {
        success: false,
        output: std::sync::Arc::from("partial"),
        ..Default::default()
    };
    assert_eq!(persist_subagent_output(dir.path(), &failed), None);
}
#[test]
fn subagent_output_roundtrips_through_output_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = "line one\nline two with unicode ✓";
    assert!(write_subagent_output(dir.path(), output));
    assert_eq!(read_subagent_output(dir.path()).as_deref(), Some(output));
    assert_eq!(read_subagent_output(&dir.path().join("missing")), None);
    std::fs::write(dir.path().join("output.json"), "not json").expect("corrupt file");
    assert_eq!(read_subagent_output(dir.path()), None);
}
#[test]
fn partial_override_fills_from_role() {
    let overrides = SubagentRuntimeOverrides {
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let role = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("execute".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert_eq!(resolved.model.as_deref(), Some("explicit-model"));
    assert_eq!(
            resolved.capability_mode,
            Some(distill_tool_types::SubagentCapabilityMode::Execute)
        );
}
#[test]
fn invalid_role_capability_mode_ignored() {
    let overrides = SubagentRuntimeOverrides::default();
    let role = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        default_capability_mode: Some("invalid-mode".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        Some(&role),
        &HashMap::new(),
        None,
        None,
    );
    assert!(
            resolved.capability_mode.is_none(),
            "invalid role mode should not produce a capability_mode"
        );
}
#[test]
fn persona_resolved_from_config() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("researcher".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "researcher".to_string(),
            distill_subagent_resolution::config::SubagentPersona {
                instructions: Some("Be thorough.".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(&overrides, None, &personas, None, None);
    assert_eq!(resolved.persona.as_deref(), Some("researcher"));
    assert_eq!(
            resolved.persona_instructions.as_deref(),
            Some("Be thorough.")
        );
}
#[test]
fn persona_inline_plus_file_merged_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("extra.md"), "File-based content.").unwrap();
    let overrides = SubagentRuntimeOverrides {
        persona: Some("combo".into()),
        ..Default::default()
    };
    let mut personas = HashMap::new();
    personas
        .insert(
            "combo".to_string(),
            distill_subagent_resolution::config::SubagentPersona {
                instructions: Some("Inline first.".into()),
                instructions_file: Some("extra.md".into()),
                ..Default::default()
            },
        );
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &personas,
        Some(tmp.path()),
        None,
    );
    let pi = resolved.persona_instructions.as_deref().unwrap();
    assert!(
            pi.starts_with("Inline first."),
            "inline should come first: {pi}"
        );
    assert!(
            pi.contains("File-based content."),
            "file content should be included: {pi}"
        );
}
#[test]
fn model_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            distill_subagent_resolution::config::SubagentPersona {
                model: Some("persona-model".into()),
                ..Default::default()
            },
        );
    let role = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        model: Some("role-model".into()),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        model: Some("explicit-model".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("explicit-model"));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.model.as_deref(), Some("role-model"));
    let role_no_model = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_model),
        &personas,
        None,
        None,
    );
    assert_eq!(r.model.as_deref(), Some("persona-model"));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.model.is_none());
}
#[test]
fn reasoning_effort_precedence_explicit_over_role_over_persona() {
    let mut personas = HashMap::new();
    personas
        .insert(
            "dev".to_string(),
            distill_subagent_resolution::config::SubagentPersona {
                reasoning_effort: Some("low".into()),
                ..Default::default()
            },
        );
    let role = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        reasoning_effort: Some("medium".into()),
        ..Default::default()
    };
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        reasoning_effort: Some("high".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort.as_deref(), Some("high"));
    let overrides = SubagentRuntimeOverrides {
        persona: Some("dev".into()),
        ..Default::default()
    };
    let r = resolve_effective_overrides(&overrides, Some(&role), &personas, None, None);
    assert_eq!(r.reasoning_effort.as_deref(), Some("medium"));
    let role_no_re = distill_subagent_resolution::config::SubagentRole {
        description: "test".into(),
        ..Default::default()
    };
    let r = resolve_effective_overrides(
        &overrides,
        Some(&role_no_re),
        &personas,
        None,
        None,
    );
    assert_eq!(r.reasoning_effort.as_deref(), Some("low"));
    let overrides = SubagentRuntimeOverrides::default();
    let r = resolve_effective_overrides(&overrides, None, &HashMap::new(), None, None);
    assert!(r.reasoning_effort.is_none());
}

#[test]
fn child_jev_auto_inherits_only_without_an_explicit_child_policy() {
    use crate::agent::subagent::resolve_child_jev_effort_auto;

    let mut request = auto_wake_test_request("policy");
    let runtime = EffectiveRuntimeConfig::default();
    assert!(resolve_child_jev_effort_auto(
        true, &request, &runtime, None
    ));
    assert!(!resolve_child_jev_effort_auto(
        false, &request, &runtime, None
    ));

    request.runtime_overrides.model = Some("pinned-model".into());
    assert!(!resolve_child_jev_effort_auto(
        true, &request, &runtime, None
    ));

    request.runtime_overrides.model = None;
    request.runtime_overrides.reasoning_effort = Some("low".into());
    assert!(!resolve_child_jev_effort_auto(
        true, &request, &runtime, None
    ));
    request.runtime_overrides.reasoning_effort = Some("auto".into());
    assert!(resolve_child_jev_effort_auto(
        false, &request, &runtime, None
    ));

    request.runtime_overrides.reasoning_effort = None;
    let source = ResumeSourceData {
        subagent_id: "source".into(),
        child_session_id: "child".into(),
        child_cwd: "/workspace".into(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".into(),
        persona: None,
        model_id: Some("pinned-model".into()),
        effort_auto: Some(true),
        model_routing_locked: Some(true),
    };
    assert!(resolve_child_jev_effort_auto(
        false,
        &request,
        &runtime,
        Some(&source),
    ));
    let mut legacy_source = source.clone();
    legacy_source.effort_auto = None;
    assert!(!resolve_child_jev_effort_auto(
        true,
        &request,
        &runtime,
        Some(&legacy_source),
    ));

    request.fork_context = true;
    assert!(resolve_child_jev_effort_auto(
        true, &request, &runtime, None
    ));
    request.runtime_overrides.model = Some("fork-model".into());
    assert!(!resolve_child_jev_effort_auto(
        true, &request, &runtime, None
    ));
}
#[test]
fn persona_not_found_produces_error() {
    let overrides = SubagentRuntimeOverrides {
        persona: Some("missing".into()),
        ..Default::default()
    };
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona_error.is_some());
    assert!(
            resolved
                .persona_error
                .as_deref()
                .unwrap()
                .contains("not found"),
        );
}
#[test]
fn prompt_assembly_ordering() {
    let role_prompt = Some(
        "<role-instructions>\nRole content\n</role-instructions>".to_string(),
    );
    let persona_instructions = Some(
        "<persona>\nPersona content\n</persona>".to_string(),
    );
    let task = "Do the task";
    let mut sections = Vec::new();
    sections.push("<fork-context>...</fork-context>".to_string());
    if let Some(ref rp) = role_prompt {
        sections.push(rp.clone());
    }
    if let Some(ref pi) = persona_instructions {
        sections.push(pi.clone());
    }
    sections.push(task.to_string());
    let assembled = sections.join("\n\n");
    let fork_pos = assembled.find("<fork-context>").unwrap();
    let role_pos = assembled.find("<role-instructions>").unwrap();
    let persona_pos = assembled.find("<persona>").unwrap();
    let task_pos = assembled.find("Do the task").unwrap();
    assert!(fork_pos < role_pos, "fork before role");
    assert!(role_pos < persona_pos, "role before persona");
    assert!(persona_pos < task_pos, "persona before task");
}
#[test]
fn no_persona_produces_none() {
    let overrides = SubagentRuntimeOverrides::default();
    let resolved = resolve_effective_overrides(
        &overrides,
        None,
        &HashMap::new(),
        None,
        None,
    );
    assert!(resolved.persona.is_none());
    assert!(resolved.persona_instructions.is_none());
}
#[test]
fn forked_initial_context_normalizes_parent_history() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("UNIQUE_FORK_MARKER_abc123 implement multi-repo fix"),
            ConversationItem::assistant("noted"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(ctx.copy_error.is_none());
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let Some(ConversationItem::User(u)) = ctx.conversation.get(1) {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                distill_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("<background_context>"));
        assert!(
                text.contains("UNIQUE_FORK_MARKER_abc123"),
                "distinctive parent token must appear in background: {text}"
            );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_inherits_parent_across_reasoning() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::Reasoning(distill_sampling_types::synthesized_reasoning_item(
                "deliberating",
            )),
            ConversationItem::assistant("ack"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert_eq!(ctx.prefix_len, Some(2));
    assert_eq!(ctx.conversation.len(), 2);
    if let Some(ConversationItem::User(u)) = ctx.conversation.get(1) {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                distill_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(
                text.contains("<background_context>"),
                "background wrapper must be present: {text}"
            );
        assert!(
                text.contains("UNIQUE_FORK_MARKER_TEST"),
                "parent context must be inherited across the reasoning sibling: {text}"
            );
    } else {
        panic!("expected User background at [1]");
    }
}
#[test]
fn forked_initial_context_empty_fails_open_to_new() {
    let ctx = forked_initial_context(vec![]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn resume_vs_fork_helper_shapes_differ() {
    use distill_sampling_types::conversation::ConversationItem;
    let resume_items = vec![
            ConversationItem::system("child system"),
            ConversationItem::user("prior subagent work"),
            ConversationItem::assistant("done"),
        ];
    let resumed = resume_initial_context(resume_items.clone(), false);
    let forked = forked_initial_context(resume_items);
    assert_eq!(resumed.source, InitialContextSource::Resumed);
    assert_eq!(forked.source, InitialContextSource::Forked);
    assert!(resumed.conversation.len() > forked.conversation.len());
    assert!(!matches!(
            resumed.conversation.get(1),
            Some(ConversationItem::User(u))
                if u.content.iter().any(|p| matches!(
                    p,
                    distill_sampling_types::conversation::ContentPart::Text { text }
                        if text.contains("<background_context>")
                ))
        ));
}
#[test]
fn forked_initial_context_applies_fork_filter_before_normalize() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("complete user"),
            ConversationItem::assistant("complete asst"),
            ConversationItem::user("INCOMPLETE_TRAILING"),
        ];
    let ctx = forked_initial_context(items);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    if let Some(ConversationItem::User(u)) = ctx.conversation.get(1) {
        let text: String = u
            .content
            .iter()
            .filter_map(|p| match p {
                distill_sampling_types::conversation::ContentPart::Text { text } => {
                    Some(text.as_ref())
                }
                _ => None,
            })
            .collect();
        assert!(text.contains("complete user"));
        assert!(
                !text.contains("INCOMPLETE_TRAILING"),
                "fork_filter must truncate incomplete trailing turn: {text}"
            );
    } else {
        panic!("expected background user");
    }
}
#[test]
fn verbatim_fork_keeps_items_byte_for_byte_when_small() {
    use distill_sampling_types::conversation::{
        ContentPart, ConversationItem, SyntheticReason, UserItem,
    };
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("remember UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::User(UserItem {
                content: vec![ContentPart::Text {
                    text: "SYNTHETIC_KEEP_ME".into(),
                }],
                synthetic_reason: SyntheticReason::SystemReminder,
                ..Default::default()
            }),
            ConversationItem::Reasoning(distill_sampling_types::synthesized_reasoning_item(
                "thinking",
            )),
            ConversationItem::assistant("ack"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            ctx.verbatim_fork,
            "a small, complete-tail parent must mirror verbatim"
        );
    assert_eq!(ctx.prefix_len, Some(5));
    assert_eq!(ctx.conversation.len(), 5);
    assert!(matches!(
            ctx.conversation.first(),
            Some(ConversationItem::System(_))
        ));
    assert!(matches!(
            ctx.conversation.last(),
            Some(ConversationItem::Assistant(_))
        ));
    let text_present = |needle: &str| {
        ctx
            .conversation
            .iter()
            .any(|i| {
                matches!(i, ConversationItem::User(u)
                    if u.content.iter().any(|p| matches!(p,
                        ContentPart::Text { text } if text.contains(needle))))
            })
    };
    assert!(
            text_present("UNIQUE_FORK_MARKER_TEST"),
            "marker must survive verbatim"
        );
    assert!(
            text_present("SYNTHETIC_KEEP_ME"),
            "synthetic-reason item must be preserved verbatim, NOT stripped"
        );
    assert!(
            ctx.conversation
                .iter()
                .any(|i| matches!(i, ConversationItem::User(u) if !u.synthetic_reason.is_human())),
            "the synthetic_reason marker itself must remain in the verbatim mirror"
        );
    assert!(
            !text_present("<background_context>"),
            "verbatim fork must NOT summarize into a background blob"
        );
}
#[test]
fn verbatim_fork_falls_back_to_summary_on_incomplete_tail() {
    use distill_sampling_types::conversation::{
        AssistantItem, ContentPart, ConversationItem, ToolCall,
    };
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("q1 UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::assistant("a1"),
            ConversationItem::user("q2"),
            ConversationItem::Assistant(AssistantItem {
                content: String::new().into(),
                tool_calls: vec![ToolCall {
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            }),
        ];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            !ctx.verbatim_fork,
            "an incomplete (dangling tool call) tail must fall back to summary"
        );
    assert_eq!(ctx.prefix_len, Some(2));
    assert!(
            ctx.conversation.iter().any(|i| {
                matches!(i, ConversationItem::User(u)
                    if u.content.iter().any(|p| matches!(p,
                        ContentPart::Text { text } if text.contains("<background_context>"))))
            }),
            "summarized fallback must produce a background_context blob"
        );
}
#[test]
fn summarized_fork_is_not_a_verbatim_mirror() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![
            ConversationItem::system("parent system prompt"),
            ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST"),
            ConversationItem::assistant("ack"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(!ctx.verbatim_fork);
    let verbatim_mirror_fork = ctx.source == InitialContextSource::Forked
        && ctx.verbatim_fork;
    assert!(
            !verbatim_mirror_fork,
            "a summarized fork must NOT be treated as a verbatim mirror"
        );
}
#[test]
fn verbatim_fork_falls_back_to_summary_when_oversize() {
    use distill_sampling_types::conversation::{ContentPart, ConversationItem};
    let items = vec![
            ConversationItem::system("parent system"),
            ConversationItem::user("turn one UNIQUE_FORK_MARKER_TEST with some text"),
            ConversationItem::assistant("ack one"),
        ];
    let ctx = verbatim_or_normalize_fork(items, 1);
    assert_eq!(ctx.source, InitialContextSource::Forked);
    assert!(
            !ctx.verbatim_fork,
            "oversize parent must fall back to summary"
        );
    assert_eq!(ctx.prefix_len, Some(2));
    let has_blob = ctx
        .conversation
        .iter()
        .any(|i| {
            matches!(i, ConversationItem::User(u)
                if u.content.iter().any(|p| matches!(p,
                    ContentPart::Text { text } if text.contains("<background_context>"))))
        });
    assert!(
            has_blob,
            "oversize fallback must produce a background_context blob"
        );
}
#[test]
fn verbatim_fork_empty_after_filter_fails_open_to_new() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::user("/goal do the thing")];
    let ctx = verbatim_or_normalize_fork(items, 256_000);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(!ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
}
#[test]
fn forked_initial_context_system_only_fails_open_to_new() {
    use distill_sampling_types::conversation::ConversationItem;
    let ctx = forked_initial_context(vec![ConversationItem::system("sys")]);
    assert_eq!(ctx.source, InitialContextSource::New);
    assert!(!ctx.verbatim_fork);
    assert!(ctx.conversation.is_empty());
    assert!(ctx.copy_error.is_some());
}
#[test]
fn fork_context_normalized_only_for_summarized() {
    assert!(!fork_context_normalized(
            &InitialContextSource::Forked,
            true
        ));
    assert!(fork_context_normalized(
            &InitialContextSource::Forked,
            false
        ));
    assert!(!fork_context_normalized(&InitialContextSource::New, false));
    assert!(!fork_context_normalized(
            &InitialContextSource::Resumed,
            false
        ));
    use distill_sampling_types::conversation::ConversationItem;
    let verbatim = verbatim_or_normalize_fork(
        vec![
                ConversationItem::system("sys"),
                ConversationItem::user("q"),
                ConversationItem::assistant("a"),
            ],
        256_000,
    );
    assert!(verbatim.verbatim_fork);
    assert!(!fork_context_normalized(
            &verbatim.source,
            verbatim.verbatim_fork
        ));
    let summarized = verbatim_or_normalize_fork(
        vec![
                ConversationItem::system("sys"),
                ConversationItem::user("q with text"),
                ConversationItem::assistant("a"),
            ],
        1,
    );
    assert!(!summarized.verbatim_fork);
    assert!(fork_context_normalized(
            &summarized.source,
            summarized.verbatim_fork
        ));
}
fn bootstrap_test_request(fork_context: bool) -> SubagentRequest {
    SubagentRequest {
        id: "bootstrap-test".into(),
        prompt: "plan".into(),
        description: "d".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "parent".into(),
        parent_prompt_id: None,
        resume_from: None,
        cwd: None,
        runtime_overrides: Default::default(),
        run_in_background: false,
        surface_completion: false,
        await_to_completion: false,
        fork_context,
        owner: SubagentOwner::Task,
        cancel_token: CancellationToken::new(),
        spawn_root: Default::default(),
    }
}
#[tokio::test]
async fn bootstrap_in_place_resume_reads_existing_transcript() {
    use crate::session::storage::StorageAdapter;
    use crate::session::storage::jsonl::JsonlStorageAdapter;
    use distill_sampling_types::conversation::ConversationItem;
    let temp = tempfile::TempDir::new().unwrap();
    let child = SessionInfo {
        id: acp::SessionId::new("same-child"),
        cwd: temp.path().to_string_lossy().into_owned(),
    };
    let child_dir = temp.path().join("child-session");
    let storage = JsonlStorageAdapter::with_explicit_session_dir(child_dir.clone());
    storage.init_session(&child, acp::ModelId::new("test-model")).await.unwrap();
    storage
        .append_chat_message(&child, &ConversationItem::system("system"))
        .await
        .unwrap();
    storage
        .append_chat_message(&child, &ConversationItem::user("previous work"))
        .await
        .unwrap();
    let request = bootstrap_test_request(false);
    let source = distill_subagent_resolution::ResumeSourceData {
        subagent_id: "same-child".to_owned(),
        child_session_id: "same-child".to_owned(),
        child_cwd: child.cwd.clone(),
        worktree_path: None,
        snapshot_ref: None,
        subagent_type: "general-purpose".to_owned(),
        persona: None,
        model_id: Some("test-model".to_owned()),
        effort_auto: None,
        model_routing_locked: None,
    };
    let out = bootstrap_initial_context(
            &request,
            Some(&source),
            &ctx_with_toggle(HashMap::new()),
            &child,
            &child_dir,
            "test-model",
            super::resume_window::ResumeWindowPolicy {
                context_window: 128_000,
                auto_compact_threshold_percent: 85,
            },
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(initial) => {
            assert_eq!(initial.source, InitialContextSource::Resumed);
            assert_eq!(initial.conversation.len(), 2);
        }
        BootstrapInitialContext::ResumeAbort(message) => {
            panic!("unexpected abort: {message}")
        }
    }
}
#[tokio::test]
async fn bootstrap_no_fork_is_new() {
    let req = bootstrap_test_request(false);
    let ctx = ctx_with_toggle(HashMap::new());
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            super::resume_window::ResumeWindowPolicy {
                context_window: 128_000,
                auto_compact_threshold_percent: 85,
            },
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.conversation.is_empty());
            assert!(ic.copy_error.is_none());
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn bootstrap_fork_without_parent_fails_open() {
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_chat_state = None;
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot2"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            super::resume_window::ResumeWindowPolicy {
                context_window: 128_000,
                auto_compact_threshold_percent: 85,
            },
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::New);
            assert!(ic.copy_error.is_some());
        }
        BootstrapInitialContext::ResumeAbort(m) => {
            panic!("fork must fail open, not abort: {m}")
        }
    }
}
#[tokio::test]
async fn bootstrap_fork_live_parent_chat_state_is_forked_with_marker() {
    use distill_sampling_types::conversation::ConversationItem;
    const MARKER: &str = "UNIQUE_LIVE_FORK_MARKER_xyz789";
    let req = bootstrap_test_request(true);
    let mut ctx = ctx_with_toggle(HashMap::new());
    let chat = spawn_test_parent_chat_state("grok-4.5");
    chat.replace_conversation(
        vec![
            ConversationItem::system("parent system"),
            ConversationItem::user(format!("{MARKER} implement multi-repo fix")),
            ConversationItem::assistant("noted the multi-repo work"),
        ],
    );
    ctx.parent_chat_state = Some(chat);
    ctx.parent_session_info = None;
    let child = SessionInfo {
        id: acp::SessionId::new("child-boot-live"),
        cwd: "/tmp".into(),
    };
    let out = bootstrap_initial_context(
            &req,
            None,
            &ctx,
            &child,
            Path::new("/tmp"),
            "m",
            super::resume_window::ResumeWindowPolicy {
                context_window: 128_000,
                auto_compact_threshold_percent: 85,
            },
        )
        .await;
    match out {
        BootstrapInitialContext::Ready(ic) => {
            assert_eq!(ic.source, InitialContextSource::Forked);
            assert!(ic.copy_error.is_none());
            assert!(
                    ic.verbatim_fork,
                    "small complete-tail parent must mirror verbatim"
                );
            assert_eq!(ic.conversation.len(), 3);
            assert_eq!(ic.prefix_len, Some(3));
            assert!(matches!(
                    ic.conversation.first(),
                    Some(ConversationItem::System(_))
                ));
            assert!(matches!(
                    ic.conversation.get(1),
                    Some(ConversationItem::User(_))
                ));
            assert!(matches!(
                    ic.conversation.get(2),
                    Some(ConversationItem::Assistant(_))
                ));
            let text: String = ic
                .conversation
                .iter()
                .filter_map(|item| match item {
                    ConversationItem::User(u) => {
                        Some(
                            u
                                .content
                                .iter()
                                .filter_map(|p| match p {
                                    distill_sampling_types::conversation::ContentPart::Text {
                                        text,
                                    } => Some(text.as_ref()),
                                    _ => None,
                                })
                                .collect::<String>(),
                        )
                    }
                    _ => None,
                })
                .collect();
            assert!(
                    text.contains(MARKER),
                    "live parent marker must appear verbatim: {text}"
                );
            assert!(
                    !text.contains("<background_context>"),
                    "verbatim mirror must NOT wrap items in a background_context blob: {text}"
                );
        }
        BootstrapInitialContext::ResumeAbort(m) => panic!("unexpected abort: {m}"),
    }
}
#[tokio::test]
async fn copy_session_data_preserves_parent_chat_history() {
    use crate::sampling::ConversationItem;
    use crate::session::storage::StorageAdapter;
    use crate::session::storage::jsonl::JsonlStorageAdapter;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let adapter = JsonlStorageAdapter::with_root(root.to_path_buf());
    let parent_info = SessionInfo {
        id: acp::SessionId::new("parent-fork-test"),
        cwd: "/workspace".to_string(),
    };
    adapter.init_session(&parent_info, acp::ModelId::new("test-model")).await.unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::user("What files?"))
        .await
        .unwrap();
    adapter
        .append_chat_message(&parent_info, &ConversationItem::assistant("listed"))
        .await
        .unwrap();
    let child_info = SessionInfo {
        id: acp::SessionId::new("child-fork-test"),
        cwd: "/workspace".to_string(),
    };
    let result = adapter
        .copy_session_data_sync(
            &parent_info,
            &child_info,
            crate::session::storage::CopySessionOptions {
                parent_session_id: Some("parent-fork-test".to_string()),
                new_model_id: Some("test-model".to_string()),
                session_kind: Some("subagent_fork".to_string()),
                fork_context_source: Some("forked".to_string()),
                copy_plan_state: false,
                copy_plan_mode_state: false,
                copy_signals: false,
                copy_usage: false,
                copy_tool_state: false,
                fork_filter: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(result.chat_messages_copied > 0, "should copy chat history");
    let child_data = adapter.load_session(&child_info).await.unwrap();
    assert_eq!(
            child_data.summary.session_kind.as_deref(),
            Some("subagent_fork")
        );
    assert_eq!(
            child_data.summary.fork_context_source.as_deref(),
            Some("forked")
        );
    assert_eq!(
            child_data.summary.parent_session_id.as_deref(),
            Some("parent-fork-test")
        );
    assert!(
            !child_data.chat_history.is_empty(),
            "child should have inherited parent chat history"
        );
}
fn make_validation_ctx(toggle: HashMap<String, bool>) -> SubagentValidationContext {
    SubagentValidationContext {
        parent_cwd: PathBuf::from("/tmp"),
        subagent_toggle: toggle,
        ..Default::default()
    }
}
#[test]
fn validate_subagent_type_returns_ok_for_known_enabled_agent() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
            matches!(outcome, SubagentValidateTypeOutcome::Ok),
            "expected Ok, got {outcome:?}",
        );
}
#[test]
fn validate_subagent_type_returns_unknown_for_invented_type() {
    let ctx = make_validation_ctx(HashMap::new());
    let outcome = validate_subagent_type("totally-invented-agent-name", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::Unknown { available } => {
            for expected in ["general-purpose", "explore", "plan"] {
                assert!(
                        available.iter().any(|n| n == expected),
                        "available list must include built-in {expected:?}: {available:?}",
                    );
            }
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_returns_disabled_when_toggled_off() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    let outcome = validate_subagent_type("explore", &ctx);
    assert!(
            matches!(outcome, SubagentValidateTypeOutcome::Disabled),
            "expected Disabled, got {outcome:?}",
        );
}
#[test]
fn validate_subagent_type_returns_not_allowed_when_outside_allow_list() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.allowed_subagent_types = Some(vec!["plan".to_string()]);
    let outcome = validate_subagent_type("explore", &ctx);
    match outcome {
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            assert_eq!(allowed, vec!["plan".to_string()]);
        }
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_allow_list_is_case_insensitive() {
    for (requested, allowed) in [
        ("explore", vec!["EXPLORE".to_string()]),
        ("EXPLORE", vec!["explore".to_string()]),
        ("Explore", vec!["eXpLoRe".to_string()]),
        ("explore", vec!["plan".to_string(), "EXPLORE".to_string()]),
    ] {
        let mut ctx = make_validation_ctx(HashMap::new());
        ctx.cli_agent_names = vec![requested.to_string()];
        ctx.allowed_subagent_types = Some(allowed.clone());
        assert!(
                matches!(
                    validate_subagent_type(requested, &ctx),
                    SubagentValidateTypeOutcome::Ok,
                ),
                "{requested:?} should be permitted by allow-list {allowed:?}",
            );
    }
}
#[test]
fn validate_subagent_type_unknown_includes_cli_agents_in_available() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined-agent".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                    available.iter().any(|n| n == "user-defined-agent"),
                    "cli agent name missing from available list: {available:?}",
                );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_dedupes_cli_against_builtins() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["explore".to_string()];
    match validate_subagent_type("invented", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            let count = available.iter().filter(|n| n.as_str() == "explore").count();
            assert_eq!(count, 1, "explore must appear once: {available:?}");
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_unknown_omits_disabled_types_from_available_list() {
    let toggle = HashMap::from([("explore".to_string(), false)]);
    let ctx = make_validation_ctx(toggle);
    match validate_subagent_type("explor", &ctx) {
        SubagentValidateTypeOutcome::Unknown { available } => {
            assert!(
                    !available.iter().any(|n| n == "explore"),
                    "disabled type must not appear in available: {available:?}",
                );
            assert!(
                    available.iter().any(|n| n == "general-purpose"),
                    "non-disabled built-ins must still appear: {available:?}",
                );
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
#[test]
fn validate_subagent_type_recognizes_cli_agent_by_name() {
    let mut ctx = make_validation_ctx(HashMap::new());
    ctx.cli_agent_names = vec!["user-defined".to_string()];
    assert!(matches!(
            validate_subagent_type("user-defined", &ctx),
            SubagentValidateTypeOutcome::Ok,
        ));
}
#[test]
fn summarize_tool_config_uses_name_override_and_strips_namespace() {
    use distill_tools::registry::types::{ToolConfig, ToolServerConfig};
    use distill_tools::types::tool::ToolKind;
    let mut read = ToolConfig::from_id("Distill:read_file");
    read.kind = Some(ToolKind::Read);
    let mut read_dup = ToolConfig::from_id("Codex:read_file");
    read_dup.kind = Some(ToolKind::Read);
    read_dup.name_override = Some("codex_read".to_string());
    let mut grep = ToolConfig::from_id("OpenCode:grep");
    grep.kind = Some(ToolKind::Search);
    grep.name_override = Some("alt_grep".to_string());
    let mcp = ToolConfig::from_id("MCP:custom");
    let config = ToolServerConfig {
        tools: vec![read, read_dup, grep, mcp],
        behavior_preset: None,
    };
    let summary = summarize_tool_config(&config);
    assert_eq!(
            summary.tool_names.get(&ToolKind::Read).unwrap(),
            "read_file"
        );
    assert_eq!(
            summary.tool_names.get(&ToolKind::Search).unwrap(),
            "alt_grep"
        );
    assert!(summary.can_read && summary.can_search && !summary.can_execute);
    assert_eq!(summary.tool_names.len(), 2);
}
#[test]
fn describe_subagent_type_unknown_returns_sorted_available() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type("totally-invented-type", None, &ctx) {
        SubagentDescribeOutcome::Unknown { available } => {
            let mut sorted = available.clone();
            sorted.sort();
            assert_eq!(available, sorted, "available must be sorted");
            assert!(available.iter().any(|n| n == "general-purpose"));
        }
        other => panic!("expected Unknown, got {other:?}"),
    }
}
/// Regression guard for the DEFAULT Distill host, the primary `/goal` host. There the only `general-purpose` tool that edits files is `search_replace` (`ToolKind::Edit`).
/// The `write` tool (`ToolKind::Write`) is only injected later, so the pre-injection describe probe never lists it. The planner gate must therefore key on the Edit capability.
#[test]
fn describe_default_host_general_purpose_has_edit_not_write() {
    use distill_tools::types::tool::ToolKind;
    let ctx = ctx_with_toggle(HashMap::new());
    let SubagentDescribeOutcome::Ok(summary) = describe_subagent_type(
        "general-purpose",
        None,
        &ctx,
    ) else {
        panic!("expected Ok for default-host general-purpose");
    };
    assert!(summary.can_read, "default host reads (read_file)");
    assert!(
            summary.tool_names.contains_key(&ToolKind::Edit),
            "default host's file-mutator is search_replace (Edit): {:?}",
            summary.tool_names,
        );
    assert!(
            !summary.tool_names.contains_key(&ToolKind::Write),
            "the injection-only `write` tool must NOT be in the pre-injection probe",
        );
}
/// An `agent_type` that does not resolve to a harness `AgentDefinition` reports `Unknown`.
/// The `/goal` resolver maps that to a `ToolsetUnknown` fail-open to the session harness.
#[test]
fn goal_harness_override_unresolvable_returns_unknown() {
    let ctx = ctx_with_toggle(HashMap::new());
    match describe_subagent_type(
        "general-purpose",
        Some("totally-bogus-harness"),
        &ctx,
    ) {
        SubagentDescribeOutcome::Unknown { .. } => {}
        other => {
            panic!("an unresolvable harness override must fail open as Unknown: {other:?}")
        }
    }
}
/// The model fallback only fires for a strict harness.
/// A custom profile running a stock/vision model leaves subagents on the default harness, so they keep native image input.
#[test]
fn subagent_keeps_default_flavor_when_parent_model_is_non_strict() {
    use distill_agent::config::BuiltinAgentName;
    let mut ctx = ctx_with_toggle(HashMap::new());
    ctx.parent_agent_name = Some("ai-oncall-bot".to_string());
    ctx.parent_model_agent_type = Some(
        BuiltinAgentName::DistillPlan.as_ref().to_string(),
    );
    let mut def = resolve_agent_definition("general-purpose", &ctx).expect("resolves");
    resolve_subagent_toolset("general-purpose", None, &ctx, &mut def);
    assert!(
            !crate::session::is_cursor_user_template(&def.user_message_template),
            "a non-strict parent model must leave subagents on the default harness",
        );
}
fn test_gcs_context(ctx: &SubagentSpawnContext) -> GcsUploadContext {
    GcsUploadContext {
        bucket_url: None,
        upload_method: None,
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: None,
        role_name: None,
        parent_prompt_id: None,
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    }
}
#[tokio::test]
async fn cancel_pending_shell_child_presents_one_cancelled_finish() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let request = auto_wake_test_request("promote-cancel");
    let meta_dir = tempfile::tempdir().expect("meta dir");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            &request.id,
            &acp::SessionId::new(request.id.clone()),
            meta_dir.path(),
            None,
            false,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::Cancelled,
            true,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
    assert!(!result.success);
    let completion_data = ShellCompletionData::from_context(
        &ctx,
        distill_message_delivery_core::AttemptId::mint(1),
        None,
    );
    completion_data.mark_spawned_notification_emitted();
    let completion = ChildCompletion {
        snapshot: test_snapshot(&request, &result),
        request,
        result,
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: false,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    let mut persisted = 0;
    while let Ok(command) = parent_cmd_rx.try_recv() {
        if matches!(
                command,
                SessionCommand::XaiSessionNotification {
                    notification: SessionNotification {
                        update: SessionUpdate::SubagentFinished { status, .. },
                        ..
                    }
                } if status == "cancelled"
            ) {
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1);
    let mut live = 0;
    while let Ok(message) = gateway_rx.try_recv() {
        if matches!(
                message,
                distill_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.params.get().contains("\"status\":\"cancelled\"")
            ) {
            live += 1;
        }
    }
    assert_eq!(live, 1);
}
async fn run_promote_cancel_with_worktree(
    worktree: &Path,
    worktree_freshly_created: bool,
) {
    let ctx = ctx_with_toggle(HashMap::new());
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let meta_dir = tempfile::tempdir().expect("meta dir");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            "worktree-cancel",
            &acp::SessionId::new("worktree-cancel"),
            meta_dir.path(),
            Some(worktree),
            worktree_freshly_created,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::Cancelled,
            true,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
}
/// A pending cancel removes a freshly-created worktree but preserves a resumed child worktree owned by its source.
#[tokio::test]
async fn cancel_pending_at_promote_removes_fresh_worktree_preserves_resumed() {
    distill_test_utils::require_git!();
    use distill_test_utils::git::{git_commit_all, init_git_repo};
    let temp = tempfile::TempDir::new().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "original").unwrap();
    git_commit_all(&repo, "initial");
    let fresh = temp.path().join("subagent-fresh");
    distill_fast_worktree::WorktreeBuilder::new(&repo, &fresh)
        .standalone(true)
        .create()
        .unwrap();
    assert!(fresh.exists());
    run_promote_cancel_with_worktree(&fresh, true).await;
    assert!(
            !fresh.exists(),
            "freshly-created worktree must be removed on pending-kill"
        );
    let resumed = temp.path().join("subagent-resumed");
    distill_fast_worktree::WorktreeBuilder::new(&repo, &resumed)
        .standalone(true)
        .create()
        .unwrap();
    std::fs::write(resumed.join("tracked.txt"), "source edit").unwrap();
    assert!(resumed.exists());
    run_promote_cancel_with_worktree(&resumed, false).await;
    assert!(
            resumed.exists(),
            "resumed subagent's reused worktree must be preserved (source owns it)"
        );
    assert_eq!(
            std::fs::read_to_string(resumed.join("tracked.txt")).unwrap(),
            "source edit",
            "the source's working state must be left untouched"
        );
}
fn running_meta_json(id: &str) -> String {
    format!(
            r#"{{
                "subagent_id": "{id}",
                "parent_session_id": "test-parent",
                "child_session_id": "{id}",
                "subagent_type": "explore",
                "description": "",
                "prompt": "",
                "status": "running",
                "started_at": "2026-01-01T00:00:00Z"
            }}"#
        )
}
#[tokio::test]
async fn unproven_thread_exit_preserves_fresh_worktree() {
    let ctx = ctx_with_toggle(HashMap::new());
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let meta_dir = tempfile::tempdir().expect("meta dir");
    std::fs::write(meta_dir.path().join("meta.json"), running_meta_json("unproven-exit"))
        .expect("write running meta");
    let worktree = tempfile::tempdir().expect("worktree");
    let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
    let thread = SessionThread::from_handle(
        std::thread::spawn(move || {
            let _ = hold_rx.recv();
        }),
    );
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            thread,
            &ctx.workspace_ops,
            "unproven-exit",
            &acp::SessionId::new("unproven-exit"),
            meta_dir.path(),
            Some(worktree.path()),
            true,
            42,
            &test_gcs_context(&ctx),
            std::time::Duration::ZERO,
            UnpromotedChildDisposition::Cancelled,
            true,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(result.cancelled);
    assert!(
            worktree.path().exists(),
            "worktree must stay when actor exit is not proven"
        );
    let meta: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                .expect("read meta"),
        )
        .expect("parse meta");
    assert_eq!(meta.status, "cancelled");
    assert!(meta.completed_at.is_some());
    assert_eq!(meta.error.as_deref(), Some("Subagent was cancelled"));
    drop(hold_tx);
}
#[tokio::test]
async fn startup_admission_timeout_is_failed_not_cancelled() {
    let mut ctx = ctx_with_toggle(HashMap::new());
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let (child_cmd_tx, mut child_cmd_rx) = mpsc::unbounded_channel();
    let (gateway, mut gateway_rx) = test_gateway_with_receiver();
    let request = auto_wake_test_request("promote-timeout");
    let meta_dir = tempfile::tempdir().expect("meta dir");
    std::fs::write(meta_dir.path().join("meta.json"), running_meta_json(&request.id))
        .expect("write running meta");
    let result = cancel_pending_shell_child(
            &child_cmd_tx,
            SessionThread::from_handle(std::thread::spawn(|| {})),
            &ctx.workspace_ops,
            &request.id,
            &acp::SessionId::new(request.id.clone()),
            meta_dir.path(),
            None,
            false,
            42,
            &test_gcs_context(&ctx),
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            UnpromotedChildDisposition::AdmissionTimedOut,
            true,
        )
        .await;
    assert!(matches!(child_cmd_rx.try_recv(), Ok(SessionCommand::Cancel(_))));
    assert!(matches!(
            child_cmd_rx.try_recv(),
            Ok(SessionCommand::Shutdown(_))
        ));
    assert!(!result.cancelled);
    assert!(!result.success);
    assert_eq!(result.status(), "failed");
    assert_eq!(
            result.error.as_deref(),
            Some("Subagent initial prompt was not admitted before the deadline")
        );
    let meta: SubagentMeta = serde_json::from_str(
            &std::fs::read_to_string(meta_dir.path().join("meta.json"))
                .expect("read meta"),
        )
        .expect("parse meta");
    assert_eq!(meta.status, "failed");
    let completion_data = ShellCompletionData::from_context(
        &ctx,
        distill_message_delivery_core::AttemptId::mint(1),
        None,
    );
    completion_data.mark_spawned_notification_emitted();
    let completion = ChildCompletion {
        snapshot: test_snapshot(&request, &result),
        request,
        result,
        completion_data,
        disposition: CompletionDisposition {
            foreground_delivered: false,
            backgrounded: false,
            waiter_delivered: false,
            explicitly_killed: false,
            should_surface: false,
        },
    };
    let will_wake = will_wake_for(&completion);
    present_child_completion(completion, &gateway, will_wake);
    let mut persisted = 0;
    while let Ok(command) = parent_cmd_rx.try_recv() {
        if matches!(
                command,
                SessionCommand::XaiSessionNotification {
                    notification: SessionNotification {
                        update: SessionUpdate::SubagentFinished { status, .. },
                        ..
                    }
                } if status == "failed"
            ) {
            persisted += 1;
        }
    }
    assert_eq!(persisted, 1);
    let mut live = 0;
    while let Ok(message) = gateway_rx.try_recv() {
        if matches!(
                message,
                distill_acp_lib::AcpClientMessage::ExtNotification(args)
                    if args.request.params.get().contains("\"status\":\"failed\"")
            ) {
            live += 1;
        }
    }
    assert_eq!(live, 1);
}
fn test_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        info: crate::agent::config::ModelInfo {
            model: model_id.to_string(),
            context_window: std::num::NonZeroU64::new(256_000).unwrap(),
            ..Default::default()
        },
        mtls_cert_dir: None,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}
fn byok_model_entry(model_id: &str) -> crate::agent::config::ModelEntry {
    crate::agent::config::ModelEntry {
        api_key: Some("byok-key".to_string()),
        ..test_model_entry(model_id)
    }
}
#[test]
fn subagent_auth_type_rule() {
    use crate::agent::auth_method::{CACHED_TOKEN_AUTH_METHOD_ID, XAI_API_KEY_METHOD_ID};
    use distill_chat_state::AuthType;
    let session = acp::AuthMethodId::new(CACHED_TOKEN_AUTH_METHOD_ID);
    let api_key = acp::AuthMethodId::new(XAI_API_KEY_METHOD_ID);
    let byok = byok_model_entry("grok-byok");
    let plain = test_model_entry("grok-plain");
    assert_eq!(
            super::subagent_auth_type(Some(&byok), &session),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(Some(&byok), &api_key),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(Some(&plain), &session),
            AuthType::SessionToken,
        );
    assert_eq!(
            super::subagent_auth_type(Some(&plain), &api_key),
            AuthType::ApiKey
        );
    assert_eq!(
            super::subagent_auth_type(None, &session),
            AuthType::SessionToken
        );
    assert_eq!(super::subagent_auth_type(None, &api_key), AuthType::ApiKey);
}
#[test]
fn fresh_tool_model_accepts_visible_key_and_internal_id() {
    let mut models = indexmap::IndexMap::new();
    models.insert("grok-3".to_string(), test_model_entry("grok-3-2025-02-15"));
    assert!(
            super::handle_request::task_model_override_error(
                Some("grok-3"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .is_none(),
            "key lookup should succeed"
        );
    assert!(
            super::handle_request::task_model_override_error(
                Some("grok-3-2025-02-15"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .is_none(),
            "info().model lookup should succeed"
        );
}
#[test]
fn fresh_tool_model_rejects_unavailable_exact_key_over_visible_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    models.insert("visible-alias".to_string(), test_model_entry("collision"));
    let mut unavailable_exact = test_model_entry("hidden-internal");
    unavailable_exact.info.hidden = true;
    models.insert("collision".to_string(), unavailable_exact);
    assert_eq!(
            super::handle_request::task_model_override_error(
                Some("collision"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .as_deref(),
            Some(
                "Unknown Task.model slug 'collision'. Valid model slugs: visible-alias. \
                 Omit `model` to inherit the parent model."
            ),
            "validation must inspect the unavailable exact-key entry selected by execution"
        );
}
#[test]
fn fresh_tool_model_rejects_unavailable_first_slug_collision() {
    let mut models = indexmap::IndexMap::new();
    let mut unavailable_first = test_model_entry("shared-routing-slug");
    unavailable_first.info.user_selectable = false;
    models.insert("blocked-first".to_string(), unavailable_first);
    models.insert("visible-second".to_string(), test_model_entry("shared-routing-slug"));
    assert_eq!(
            super::handle_request::task_model_override_error(
                Some("shared-routing-slug"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .as_deref(),
            Some(
                "Unknown Task.model slug 'shared-routing-slug'. Valid model slugs: \
                 visible-second. Omit `model` to inherit the parent model."
            ),
            "validation must inspect the first routing-slug entry selected by execution"
        );
}
#[test]
fn fresh_tool_model_rejects_unknown_and_nonavailable_entries() {
    let mut models = indexmap::IndexMap::new();
    models.insert("zeta".to_string(), test_model_entry("zeta-internal"));
    let mut hidden = test_model_entry("hidden-internal");
    hidden.info.hidden = true;
    models.insert("hidden".to_string(), hidden);
    let mut not_selectable = test_model_entry("disabled-internal");
    not_selectable.info.user_selectable = false;
    models.insert("disabled".to_string(), not_selectable);
    let mut oauth_only = test_model_entry("oauth-only-internal");
    oauth_only.info.supported_in_api = false;
    models.insert("oauth-only".to_string(), oauth_only);
    models.insert("alpha".to_string(), test_model_entry("alpha-internal"));
    for requested in [
        "stale-model",
        "hidden",
        "hidden-internal",
        "disabled",
        "disabled-internal",
        "oauth-only",
        "oauth-only-internal",
    ] {
        let error = super::handle_request::task_model_override_error(
                Some(requested),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                false,
            )
            .unwrap();
        assert_eq!(
                error,
                format!(
                    "Unknown Task.model slug '{requested}'. Valid model slugs: alpha, zeta. \
                     Omit `model` to inherit the parent model."
                )
            );
        assert!(!error.contains("grok models"));
    }
    assert!(
            super::handle_request::task_model_override_error(
                Some("oauth-only"),
                ModelOverrideProvenance::Tool,
                false,
                &models,
                true,
            )
            .is_none(),
            "OAuth-only model should resolve for session auth"
        );
}
#[test]
fn resumed_tool_model_override_is_ignored() {
    let empty = indexmap::IndexMap::new();
    assert!(
            super::handle_request::task_model_override_error(
                Some("stale-model"),
                ModelOverrideProvenance::Tool,
                true,
                &empty,
                false,
            )
            .is_none(),
            "resume must preserve source-model pinning"
        );
}
#[test]
fn harness_model_override_keeps_internal_fallback_behavior() {
    let empty = indexmap::IndexMap::new();
    assert!(
            super::handle_request::task_model_override_error(
                Some("internal-model"),
                ModelOverrideProvenance::Harness,
                false,
                &empty,
                false,
            )
            .is_none(),
            "internal role/config pins must retain downstream soft fallback"
        );
}
#[test]
fn normalize_forked_context_empty_parent() {
    use distill_sampling_types::conversation::ConversationItem;
    let items = vec![ConversationItem::system("sys prompt")];
    let (conv, prefix_len) = distill_subagent_resolution::context::normalize_forked_context(
        items,
    );
    assert_eq!(conv.len(), 1);
    assert_eq!(prefix_len, 1);
    assert!(matches!(conv.first(), Some(ConversationItem::System(_))));
}
fn test_sampling_config(model_slug: &str) -> distill_sampling_types::SamplingConfig {
    use std::num::NonZeroU64;
    distill_sampling_types::SamplingConfig {
        base_url: "https://api.test/v1".to_string(),
        model: model_slug.to_string(),
        context_window: NonZeroU64::new(256_000).expect("non-zero context window"),
        ..Default::default()
    }
}
fn spawn_test_parent_chat_state(model_slug: &str) -> distill_chat_state::ChatStateHandle {
    let (mock, _persistence_rx) = distill_chat_state::MockChatPersistence::new();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let token = tokio_util::sync::CancellationToken::new();
    distill_chat_state::ChatStateActor::spawn(
        vec![],
        test_sampling_config(model_slug),
        Box::new(mock),
        event_tx,
        token,
    )
}
mod rest;
mod wake;
#[tokio::test]
async fn panicked_announced_foreground_child_emits_one_typed_finish() {
    let (gateway, _gateway_rx) = test_gateway_with_receiver();
    let (parent_cmd_tx, mut parent_cmd_rx) = mpsc::unbounded_channel();
    let mut request = auto_wake_test_request("panic-after-spawn");
    request.run_in_background = false;
    let completion_data = ShellCompletionData {
        parent_cmd_tx: Some(parent_cmd_tx),
        attempt_id: Some(distill_message_delivery_core::AttemptId::mint(0xface)),
        ..Default::default()
    };
    let worker_completion_data = completion_data.clone();
    let inner = super::worker_runtime()
        .expect("worker runtime")
        .spawn(async move {
            worker_completion_data.mark_spawned_notification_emitted();
            panic!("worker boom");
        });
    let output = join_worker_task(
            inner,
            ChildRunOutput {
                result: SubagentResult {
                    success: false,
                    error: Some("Subagent runtime panicked".to_owned()),
                    subagent_id: request.id.clone(),
                    child_session_id: request.id.clone(),
                    ..Default::default()
                },
                completion_data,
                snapshot_ref: None,
            },
        )
        .await;
    let snapshot = test_snapshot(&request, &output.result);
    present_child_completion(
        ChildCompletion {
            snapshot,
            request,
            result: output.result,
            completion_data: output.completion_data,
            disposition: CompletionDisposition {
                foreground_delivered: true,
                backgrounded: false,
                waiter_delivered: false,
                explicitly_killed: false,
                should_surface: false,
            },
        },
        &gateway,
        false,
    );
    let finishes = std::iter::from_fn(|| parent_cmd_rx.try_recv().ok())
        .filter_map(|command| match command {
            SessionCommand::XaiSessionNotification {
                notification: SessionNotification {
                    update: SessionUpdate::SubagentFinished { attempt_id, .. },
                    ..
                },
            } => Some(attempt_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finishes, vec![Some("at1.face".to_owned())]);
}
#[tokio::test]
async fn join_worker_task_drop_aborts_worker() {
    struct SendOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for SendOnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let inner = super::worker_runtime()
        .expect("worker runtime")
        .spawn(async move {
            let _probe = SendOnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
    started_rx.await.expect("worker started");
    let mut fut = Box::pin(join_worker_task(inner, ()));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
            std::future::Future::poll(fut.as_mut(), &mut cx).is_pending(),
            "worker is pending until aborted"
        );
    drop(fut);
    tokio::time::timeout(std::time::Duration::from_secs(5), dropped_rx)
        .await
        .expect("abort must reach the worker task")
        .expect("drop probe fires on abort");
}
