// Modified for Distill by Samuel Fajreldines, 2026.
use super::support::*;
use super::*;
use crate::terminal::AsyncTerminalRunner;
use crate::terminal::runner::{TerminalError, TerminalRunRequest, TerminalRunResult};
use distill_paths::AbsPathBuf;
use distill_workspace::file_system::MockFs;
use distill_workspace::permission::PermissionHandle;
use tokio::sync::mpsc;
#[derive(Debug)]
struct DummyTerminal;
#[async_trait::async_trait]
impl AsyncTerminalRunner for DummyTerminal {
    async fn run(&self, _request: TerminalRunRequest) -> Result<TerminalRunResult, TerminalError> {
        Err(TerminalError::Other("dummy terminal".into()))
    }
}
fn agent_msg_update(text: &str) -> acp::SessionUpdate {
    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
        acp::TextContent::new(text.to_string()),
    )))
}
fn extract_text(n: &acp::SessionNotification) -> Option<String> {
    match &n.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            acp::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        },
        _ => None,
    }
}
pub(super) struct ReplaySendUpdateFixture {
    pub(super) actor: SessionActor,
    pub(super) event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    pub(super) sent: Arc<tokio::sync::Mutex<Vec<acp::SessionNotification>>>,
    pub(super) persistence_rx: mpsc::UnboundedReceiver<PersistenceMsg>,
}
pub(super) async fn make_replay_send_update_fixture() -> ReplaySendUpdateFixture {
    let (gateway_tx, mut gateway_rx) =
        mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let gateway = GatewaySender::new(gateway_tx);
    let sent = Arc::new(tokio::sync::Mutex::new(
        Vec::<acp::SessionNotification>::new(),
    ));
    let sent_for_task = sent.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                sent_for_task.lock().await.push(args.request);
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    let (persistence_tx, persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
    let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
    let fs = Arc::new(MockFs::new(cwd.to_path_buf()));
    let terminal = Arc::new(DummyTerminal {});
    let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_tracker_handle = distill_hunk_tracker::HunkTrackerActor::spawn(
        "test-session".to_string(),
        cwd.to_path_buf(),
        hunk_tx,
        distill_hunk_tracker::TrackingMode::AgentOnly,
        tokio_util::sync::CancellationToken::new(),
    );
    let tool_context = ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
    let state = TokioMutex::new(State {
        running_task: None,
        finalization_gate: Default::default(),
        message_delivery: Default::default(),
        pending_inputs: VecDeque::new(),
        edit_holds: HashMap::new(),
        pending_notifications: Vec::new(),
        notifications_suppressed: false,
        rewindable: false,
        front_message_committed: false,
        hook_block_hold: Default::default(),
        nudges_used_this_session: 0,
    });
    let (event_tx, event_rx) = mpsc::unbounded_channel::<SessionEvent>();
    let actor = SessionActor {
        repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(
        ),
        transient_retry_enabled: true,
        transient_retries_prompt_total: std::cell::Cell::new(0),
        transient_episode_start: std::cell::Cell::new(None),
        status_wake: Default::default(),
        session_info: SessionInfo {
            id: acp::SessionId::new("test-session"),
            cwd: cwd.as_str().to_string(),
        },
        auth_method_id: test_auth_method_id("test-auth"),
        model_auth_memo: std::cell::RefCell::new(None),
        jev_ledger: std::cell::RefCell::new(Default::default()),
        jev_effort_auto: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        canonical_model_id: std::cell::RefCell::new(acp::ModelId::new("test")),
        model_routing_locked: std::cell::Cell::new(false),
        attribution_callback: None,
        auth_manager: None,
        is_chat_kind: false,
        state,
        notifications: NotificationSender {
            gateway,
            gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            persistence_tx,
            disk_full: crate::session::notifications::idle_disk_full_rx(),
            client_caps: crate::session::notifications::SessionClientCaps::new(false, true),
        },
        permissions: PermissionHandle::allow_all(),
        tool_context,
        deny_read_globs: Vec::new(),
        mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
        mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
        delivery_tools: std::cell::RefCell::new(Vec::new()),
        attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
        chat_state_handle: distill_chat_state::ChatStateHandle::noop(),
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
        active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        telemetry_enabled: false,
        supports_backend_search: std::cell::Cell::new(false),
        tool_overrides: std::cell::RefCell::new(None),
        resolved_tool_overrides: std::sync::Arc::new(arc_swap::ArcSwapOption::empty()),
        compactions_remaining: std::cell::Cell::new(None),
        compaction_at_tokens: std::cell::Cell::new(None),
        doom_loop_recovery: None,
        doom_loop_turn_tally: Default::default(),
        file_state_tracker: Arc::new(FileStateTracker::new()),
        rewind_pending_prompt: std::sync::Mutex::new(None),
        startup_hints: StartupHints::default(),
        forked_tool_override: None,
        compaction: crate::session::compaction_config::CompactionConfig {
            threshold_percent: std::cell::Cell::new(85),
            force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_window_override: None,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode: distill_chat_state::CompactionMode::Transcript,
            verbatim_input: true,
            tool_choice: crate::util::config::CompactionToolChoice::Auto,
            prefire: crate::session::compaction_config::PrefireState::default(),
            prefix_released: std::sync::atomic::AtomicBool::new(false),
            cancel: Default::default(),
        },
        memory: crate::session::memory_state::SessionMemory {
            configured_mode: None,
            v2_config: Default::default(),
            configured_storage: None,
            process_disabled: false,
            config_opt_out: false,
            v2_legacy_carryover: false,
            prompt_sync_pending: std::sync::atomic::AtomicBool::new(false),
            flush_config: crate::config::MemoryFlushConfig::default(),
            is_flushing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            capture_worker: std::cell::RefCell::new(None),
            dream_workers: crate::session::memory_state::V2DreamWorkers::default(),
            last_capture_failure: std::cell::RefCell::new(None),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(None),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: Default::default(),
            context_injected: std::sync::atomic::AtomicBool::new(false),
            flush_count: std::sync::atomic::AtomicU64::new(0),
            last_flush_content: std::cell::RefCell::new(None),
            flush_success_count: std::sync::atomic::AtomicU64::new(0),
            flush_error_count: std::sync::atomic::AtomicU64::new(0),
            search_counter: std::cell::RefCell::new(None),
            injection_count: std::sync::atomic::AtomicU64::new(0),
            compaction_recovery_count: std::sync::atomic::AtomicU64::new(0),
            chunks_added: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            init_reindex_handle: std::cell::RefCell::new(None),
            dream_config: Default::default(),
            dream_count: std::sync::atomic::AtomicU64::new(0),
            dream_success_count: std::sync::atomic::AtomicU64::new(0),
            dream_error_count: std::sync::atomic::AtomicU64::new(0),
            token_totals: Default::default(),
        },
        session_start: std::time::Instant::now(),
        inference_idle_timeout: Duration::from_secs(300),
        uncharged_401_park_enabled: true,
        max_retries: 3,
        rate_limit_waits: crate::session::acp_session::RateLimitWaitConfig::default(),
        max_turns: None,
        pending_interjections: InterjectionBuffer::new(),
        pending_skill_reminders: Mutex::new(Vec::new()),
        idle_flush_timeout: None,
        dream_check_timeout: None,
        last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(0),
        event_tx,
        buffering_settings: Some(BufferingSettings {
            max_items: 100,
            max_bytes: 1_000_000,
            max_duration_ms: 50,
        }),
        client_identifier: None,
        origin_client: None,
        feedback_manager: Arc::new(FeedbackManager::local_only("test-session")),
        upload_queue: Arc::new(OnceLock::new()),
        sync_loop_cancel: None,
        agent: std::cell::RefCell::new(test_agent_default().await),
        last_reported_branch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
        git_head_enabled: false,
        status_line_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        models_manager: Default::default(),
        display_cwd: std::sync::OnceLock::new(),
        active_agent_type: parking_lot::Mutex::new(None),
        queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        emit_local_background_tasks: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        active_skill: parking_lot::Mutex::new(None),
        current_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
        turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
        turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
        plan_mode: Arc::new(parking_lot::Mutex::new(
            crate::session::plan_mode::PlanModeTracker::new(std::path::PathBuf::from(
                "/tmp/test-session",
            )),
        )),
        goal_enabled: false,
        background_workflows_enabled: false,
        goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
        goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(false),
        goal_tracker: Arc::new(parking_lot::Mutex::new(
            crate::session::goal_tracker::GoalTracker::new(std::path::PathBuf::from(
                "/tmp/test-session",
            )),
        )),
        goal_turn_task_ids: parking_lot::Mutex::new(std::collections::HashSet::new()),
        goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
        goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
        goal_update_rx: std::cell::RefCell::new(None),
        goal_update_tx: tokio::sync::mpsc::unbounded_channel().0,
        workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle().0,
        workflow_launch_tx: tokio::sync::mpsc::unbounded_channel().0,
        goal_classifier_enabled: false,
        goal_planner_enabled: false,
        goal_summary_enabled: false,
        length_salvage_remote_budget: None,
        goal_verifier_skeptic_count: 1,
        goal_role_models: Default::default(),
        goal_use_current_model_only: false,
        goal_classifier_max_runs: crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
        goal_strategist_every: 5,
        goal_reverify_after: crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
        goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
        pending_classifier_completions: parking_lot::Mutex::new(std::collections::VecDeque::new()),
        goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
        managed_mcp_handle: Default::default(),
        initial_client_mcp_servers: vec![],
        tool_metadata_snapshot: Arc::new(std::sync::Mutex::new(Default::default())),
        mcp_announcements: Default::default(),
        mcp_reminder_mode: McpReminderMode::Delta,
        mcp_reminder_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        mcp_connecting_reminder_injected: std::cell::Cell::new(false),
        mcp_refresh_gate: Arc::new(tokio::sync::Mutex::new(())),
        user_input_generation: std::sync::atomic::AtomicU64::new(0),
        laziness_debug_log: None,
        last_live_orphan_reconcile: std::cell::Cell::new(None),
        deferred_prefix: DeferredPrefix::new(),
        mcp_startup_waits: Default::default(),
        mcp_init_tasks: Default::default(),
        weak_self: std::sync::Weak::new(),
        startup_tasks: Default::default(),
        extension_registry: distill_agent_lifecycle::LocalExtensionRegistry::default(),
        last_announced_local_date: std::cell::Cell::new(chrono::Local::now().date_naive()),
        prefix_carries_fallback_date: std::cell::Cell::new(false),
        last_search_prompt_index: std::sync::atomic::AtomicI64::new(-1),
        last_api_request_at: std::sync::atomic::AtomicI64::new(0),
        hook_registry: std::cell::RefCell::new(None),
        hook_disabled: Default::default(),
        turn_report: Default::default(),
        turn_abort: Default::default(),
        turn_end_tx: Default::default(),
        client_hooks: Default::default(),
        hook_resolved_workspace_root: String::new(),
        vcs_kind: distill_workspace::session::git::VcsKind::Git,
        hook_load_errors: std::cell::RefCell::new(Vec::new()),
        plugin_registry: std::cell::RefCell::new(None),
        plugin_registry_handle: None,
        events: crate::session::events::EventTracker::new(std::path::Path::new("/tmp")),
        observability_bridge: noop_observability_bridge(),
        current_turn_number: std::cell::Cell::new(0),
        turn_phases: std::sync::Arc::default(),
        last_recap_main_turn: std::cell::Cell::new(0),
        recap_in_flight: std::cell::Cell::new(false),
        recap_epoch: std::cell::Cell::new(0),
        turn_summary_task: std::cell::RefCell::new(None),
        turn_summary_generation: std::cell::Cell::new(0),
        title_refresh_task: std::cell::RefCell::new(None),
        title_refresh_generation: std::cell::Cell::new(0),
        next_title_refresh_idx: std::cell::Cell::new(0),
        turn_summary_enabled: false,
        title_refresh_enabled: false,
        session_turn_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        streaming_turn_capture: parking_lot::Mutex::new(StreamingTurnCapture::default()),
        stream_apply_span: parking_lot::Mutex::new(None),
        current_turn_span_id: parking_lot::Mutex::new(None),
        turn_stream_drained: parking_lot::Mutex::new(std::collections::HashMap::new()),
        pending_image_strip: parking_lot::Mutex::new(HashMap::new()),
        image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
        sampler_handle: distill_sampler::SamplerHandle::noop(),
        sampling_gate: None,
        rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        trace_config_template: std::cell::RefCell::new(None),
    };
    ReplaySendUpdateFixture {
        actor,
        event_rx,
        sent,
        persistence_rx,
    }
}
#[tokio::test(flavor = "current_thread")]
async fn send_update_buffers_streaming_chunks_and_flush_sends_merged_notification() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ReplaySendUpdateFixture {
                actor,
                mut event_rx,
                sent,
                mut persistence_rx,
            } = make_replay_send_update_fixture().await;
            actor.send_update(agent_msg_update("he"), Some(1)).await;
            actor.send_update(agent_msg_update("llo"), Some(2)).await;
            assert!(
                sent.lock().await.is_empty(),
                "buffering enabled: no outbound notifications expected yet"
            );
            assert!(
                persistence_rx.try_recv().is_err(),
                "buffering enabled: nothing should be persisted until emitted"
            );
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                let SessionEvent::Notification(notification) = event else {
                    unreachable!("send_update should only enqueue replay notifications")
                };
                let _ = replay_buffer.consume_chunk(notification);
            }
            let flushed = replay_buffer
                .flush()
                .expect("flush should emit pending chunk");
            actor.emit_buffered(flushed).await;
            tokio::task::yield_now().await;
            let sent_msgs = sent.lock().await.clone();
            assert_eq!(sent_msgs.len(), 1);
            let Some(first) = sent_msgs.first() else {
                panic!("expected one sent message: {sent_msgs:?}");
            };
            assert_eq!(extract_text(first).as_deref(), Some("hello"));
            let mut persisted = vec![];
            while let Ok(msg) = persistence_rx.try_recv() {
                persisted.push(msg);
            }
            let persisted_updates = persisted
                .into_iter()
                .filter(|m| matches!(m, PersistenceMsg::Update(_)))
                .count();
            assert_eq!(persisted_updates, 1);
        })
        .await;
}
/// Regression test: a cancel during a long reasoning stream must not lose buffered chunks from the trace upload.
/// The `SessionCommand::Cancel` and `SessionCommand::CopyFile` handlers in `run_session` must flush the actor-owned `ReplayBuffer`.
/// Without the flush, the tail of a long reasoning stream sitting in the buffer at Ctrl+C never reaches disk.
#[tokio::test(flavor = "current_thread")]
async fn cancel_and_copyfile_handlers_flush_buffered_chunks_to_persistence() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ReplaySendUpdateFixture {
                actor,
                mut event_rx,
                sent: _sent,
                mut persistence_rx,
            } = make_replay_send_update_fixture().await;
            actor
                .send_update(agent_msg_update("partial reasoning tail"), Some(1))
                .await;
            assert!(
                persistence_rx.try_recv().is_err(),
                "no Update should land while the chunk is still in flight",
            );
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(notification) = event {
                    let _ = replay_buffer.consume_chunk(notification);
                }
            }
            assert!(
                persistence_rx.try_recv().is_err(),
                "chunk should still be buffered, not yet persisted",
            );
            if let Some(notification) = replay_buffer.flush() {
                actor.emit_buffered(notification).await;
            }
            tokio::task::yield_now().await;
            let mut got_chunk_text: Option<String> = None;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(update) = msg
                    && let crate::session::storage::SessionUpdate::Acp(notif) = update
                    && let acp::SessionUpdate::AgentMessageChunk(chunk) = &notif.update
                    && let acp::ContentBlock::Text(t) = &chunk.content
                {
                    got_chunk_text = Some(t.text.clone());
                    break;
                }
            }
            assert_eq!(
                got_chunk_text.as_deref(),
                Some("partial reasoning tail"),
                "Cancel/CopyFile handler flush must persist the buffered \
                     reasoning chunk before the trace upload snapshots \
                     updates.jsonl"
            );
            drop(actor);
        })
        .await;
}
/// Negative control for `cancel_and_copyfile_handlers_flush_buffered_chunks_to_persistence`.
/// Without the flush, a buffered chunk does not reach persistence on its own, so the cancel path needs the explicit flush call.
#[tokio::test(flavor = "current_thread")]
async fn buffered_chunk_does_not_reach_persistence_without_explicit_flush() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ReplaySendUpdateFixture {
                actor,
                mut event_rx,
                sent: _sent,
                mut persistence_rx,
            } = make_replay_send_update_fixture().await;
            actor
                .send_update(agent_msg_update("would-be lost reasoning"), Some(1))
                .await;
            let mut replay_buffer = ReplayBuffer::new(actor.buffering_settings.clone());
            while let Ok(event) = event_rx.try_recv() {
                if let SessionEvent::Notification(notification) = event {
                    let _ = replay_buffer.consume_chunk(notification);
                }
            }
            tokio::task::yield_now().await;
            let mut saw_update = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if matches!(msg, PersistenceMsg::Update(_)) {
                    saw_update = true;
                    break;
                }
            }
            assert!(
                !saw_update,
                "without an explicit flush, the buffered chunk must \
                     remain stranded in `replay_buffer.pending` — this is \
                     the exact bug the Cancel/CopyFile patch fixes",
            );
            drop(actor);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn available_commands_update_is_forwarded_but_not_persisted() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let ReplaySendUpdateFixture {
                actor,
                event_rx: _event_rx,
                sent,
                mut persistence_rx,
            } = make_replay_send_update_fixture().await;
            let session_id = acp::SessionId::new("test-session");
            actor
                .emit_notification_direct(
                    acp::SessionNotification::new(
                        session_id.clone(),
                        acp::SessionUpdate::AvailableCommandsUpdate(
                            acp::AvailableCommandsUpdate::new(vec![]),
                        ),
                    ),
                )
                .await;
            actor
                .emit_notification_direct(
                    acp::SessionNotification::new(session_id, agent_msg_update("hello")),
                )
                .await;
            for _ in 0..50 {
                if sent.lock().await.len() >= 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(
                sent.lock().await.len(),
                2,
                "both updates must be forwarded to the live client (command palette must stay current)",
            );
            let mut persisted = vec![];
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(
                    crate::session::storage::SessionUpdate::Acp(n),
                ) = msg {
                    persisted.push(n);
                }
            }
            assert_eq!(
                persisted.len(),
                1,
                "exactly one update must be persisted; available_commands_update must be skipped",
            );
            assert!(
                matches!(persisted.first(), Some(n) if matches!(n.update, acp::SessionUpdate::AgentMessageChunk(_))),
                "the persisted update must be the agent message, not available_commands_update",
            );
            drop(actor);
        })
        .await;
}
