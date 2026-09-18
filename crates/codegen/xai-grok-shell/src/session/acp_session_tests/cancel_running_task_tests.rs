use super::support::*;
use super::*;
use crate::session::storage::StorageAdapter;
use crate::terminal::AsyncTerminalRunner;
use crate::terminal::runner::{TerminalError, TerminalRunRequest, TerminalRunResult};
use xai_grok_paths::AbsPathBuf;
#[derive(Debug)]
struct DummyTerminal;
#[async_trait::async_trait]
impl AsyncTerminalRunner for DummyTerminal {
    async fn run(&self, _request: TerminalRunRequest) -> Result<TerminalRunResult, TerminalError> {
        Err(TerminalError::Other("dummy terminal".into()))
    }
}
#[tokio::test(flavor = "current_thread")]
async fn persist_ack_waits_for_disk_flush_before_success() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tmp = tempfile::TempDir::new().unwrap();
            let session_dir = tmp.path().join("session");
            let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
            let fs = Arc::new(xai_grok_workspace::file_system::MockFs::new(
                cwd.to_path_buf(),
            ));
            let terminal = Arc::new(DummyTerminal {});
            let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
            let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
                "test-persist-ack".to_string(),
                cwd.to_path_buf(),
                hunk_tx,
                xai_hunk_tracker::TrackingMode::AgentOnly,
                tokio_util::sync::CancellationToken::new(),
            );
            let tool_context =
                ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
            let session_info = SessionInfo {
                id: acp::SessionId::new("test-persist-ack"),
                cwd: cwd.as_str().to_string(),
            };
            let sampling_client = crate::sampling::Client::new(xai_grok_sampler::SamplerConfig {
                api_key: Some("test-key".to_string()),
                base_url: "http://localhost".to_string(),
                model: "test".to_string(),
                context_window: 100_000,
                ..Default::default()
            })
            .expect("sampling client should build for persistence actor");
            let persistence = crate::session::persistence::new_with_explicit_dir(
                &crate::session::info::Info {
                    id: session_info.id.clone(),
                    cwd: session_info.cwd.clone(),
                },
                session_dir.clone(),
                acp::ModelId::new("test-model"),
                sampling_client,
                crate::test_support::TEST_MODEL.to_owned(),
                crate::session::persistence::ExplicitSessionOpen::New {
                    identity: None,
                    next_trace_turn: None,
                },
            )
            .await
            .expect("persistence actor should start");
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
            let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
            let chat_state_handle = xai_chat_state::ChatStateActor::spawn(
                vec![],
                xai_grok_sampling_types::SamplingConfig {
                    base_url: "http://localhost".to_string(),
                    model: "test".to_string(),
                    context_window: std::num::NonZeroU64::new(100_000).unwrap(),
                    ..Default::default()
                },
                Box::new(
                    crate::session::chat_persistence::ChannelChatPersistence::new(
                        persistence.tx.clone(),
                    ),
                ),
                chat_event_tx,
                tokio_util::sync::CancellationToken::new(),
            );
            let actor = Arc::new(SessionActor {
                repo_status_prefetch:
                    crate::session::repo_status_prefix::RepoStatusPrefetchState::default(),
                transient_retry_enabled: true,
                transient_retries_prompt_total: std::cell::Cell::new(0),
                transient_episode_start: std::cell::Cell::new(None),
                status_wake: Default::default(),
                session_info,
                auth_method_id: test_auth_method_id("test-auth"),
                model_auth_memo: std::cell::RefCell::new(None),
                jev_ledger: std::cell::RefCell::new(Default::default()),
                attribution_callback: None,
                auth_manager: None,
                is_chat_kind: false,
                state: TokioMutex::new(State {
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
                }),
                notifications: NotificationSender {
                    gateway: GatewaySender::new(gateway_tx),
                    gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                    persistence_tx: persistence.tx.clone(),
                    disk_full: persistence.subscribe_disk_full(),
                    client_caps: crate::session::notifications::SessionClientCaps::new(false, true),
                },
                permissions: PermissionHandle::allow_all(),
                tool_context,
                deny_read_globs: Vec::new(),
                mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
                mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
                delivery_tools: std::cell::RefCell::new(Vec::new()),
                attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
                chat_state_handle,
                unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
                current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
                active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                current_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
                turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
                turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
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
                    compaction_mode: xai_chat_state::CompactionMode::Transcript,
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
                buffering_settings: None,
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
                queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                emit_local_background_tasks: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                active_skill: parking_lot::Mutex::new(None),
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
                workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle()
                    .0,
                workflow_launch_tx: tokio::sync::mpsc::unbounded_channel().0,
                goal_classifier_enabled: false,
                goal_planner_enabled: false,
                goal_summary_enabled: false,
                length_salvage_remote_budget: None,
                goal_verifier_skeptic_count: 1,
                goal_role_models: Default::default(),
                goal_use_current_model_only: false,
                goal_classifier_max_runs:
                    crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
                goal_strategist_every: 5,
                goal_reverify_after: crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
                goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
                pending_classifier_completions: parking_lot::Mutex::new(
                    std::collections::VecDeque::new(),
                ),
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
                extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
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
                vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
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
                pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
                image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
                sampler_handle: xai_grok_sampler::SamplerHandle::noop(),
                sampling_gate: None,
                rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
                image_description_model: crate::test_support::TEST_MODEL.to_owned(),
                image_describe_cache: Arc::new(
                    crate::session::image_describe::ImageDescribeCache::new(),
                ),
                subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
                workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
                trace_config_template: std::cell::RefCell::new(None),
            });
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                "hello persist".to_string(),
            ))];
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let actor_for_prompt = actor.clone();
            let prompt_task = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "persist-ack-test",
                        prompt_blocks,
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        true,
                        false,
                        None,
                        Some(ack_tx),
                        None,
                    )
                    .await
            });
            assert!(ack_rx.await.is_ok(), "persist ack should resolve");
            let storage = crate::session::storage::JsonlStorageAdapter::with_explicit_session_dir(
                session_dir,
            );
            let loaded = storage
                .load_session_without_updates(&actor.session_info)
                .await
                .unwrap();
            assert!(
                loaded
                    .chat_history
                    .iter()
                    .any(|item| item.text_content().contains("hello persist")),
                "loaded chat history should contain the just-persisted prompt"
            );
            let _ = prompt_task.await.expect("prompt task should complete");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn plain_user_prompt_without_persist_ack_still_sends_flush_barrier_behind_its_echo() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let observed = std::rc::Rc::new(std::cell::RefCell::new(Vec::<&'static str>::new()));
            let recorder = observed.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = persistence_rx.recv().await {
                    match msg {
                        PersistenceMsg::FlushAndAck { respond_to } => {
                            recorder.borrow_mut().push("flush_and_ack");
                            let _ = respond_to.send(Ok(()));
                        }
                        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n))
                            if matches!(n.update, acp::SessionUpdate::UserMessageChunk(_)) =>
                        {
                            recorder.borrow_mut().push("user_message_chunk");
                        }
                        _ => {}
                    }
                }
            });
            let actor = std::sync::Arc::new(
                create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await,
            );
            let actor_for_prompt = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "plain-user-prompt",
                        vec![acp::ContentBlock::Text(acp::TextContent::new("hello"))],
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
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if observed.borrow().contains(&"flush_and_ack") {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("a plain user prompt (no persist_ack) must send a FlushAndAck barrier");
            turn.abort();
            let observed = observed.borrow();
            let echo_index = observed
                .iter()
                .position(|kind| *kind == "user_message_chunk")
                .expect("the prompt's UserMessageChunk echo must reach persistence");
            let barrier_index = observed
                .iter()
                .position(|kind| *kind == "flush_and_ack")
                .expect("barrier observed above");
            assert!(
                echo_index < barrier_index,
                "the flush barrier must land FIFO-behind the prompt's persisted echo"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn first_turn_memory_injection_persists_to_chat_history() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let session_dir = tempfile::tempdir().expect("tempdir");
            let session_info = crate::session::info::Info {
                id: acp::SessionId::new("persist-memory"),
                cwd: session_dir.path().to_string_lossy().to_string(),
            };
            let sampling_client = crate::sampling::Client::new(xai_grok_sampler::SamplerConfig {
                    api_key: Some("test-key".to_string()),
                    base_url: "http://localhost".to_string(),
                    model: "test-model".to_string(),
                    context_window: 100_000,
                    ..Default::default()
                })
                .expect("sampling client should build for persistence actor");
            let persistence = crate::session::persistence::new_with_explicit_dir(
                    &crate::session::info::Info {
                        id: session_info.id.clone(),
                        cwd: session_info.cwd.clone(),
                    },
                    session_dir.path().to_path_buf(),
                    acp::ModelId::new("test-model"),
                    sampling_client,
                    crate::test_support::TEST_MODEL.to_owned(),
                    crate::session::persistence::ExplicitSessionOpen::New {
                        identity: None,
                        next_trace_turn: None,
                    },
                )
                .await
                .expect("persistence actor should start");
            let (_event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<
                SessionEvent,
            >();
            let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
            let chat_state_handle = xai_chat_state::ChatStateActor::spawn(
                vec![
                        ConversationItem::system("sys"),
                        ConversationItem::user("<user_info>OS Version: macos</user_info>"),
                    ],
                xai_grok_sampling_types::SamplingConfig {
                    base_url: "http://localhost".to_string(),
                    model: "test".to_string(),
                    context_window: std::num::NonZeroU64::new(100_000).unwrap(),
                    ..Default::default()
                },
                Box::new(
                    crate::session::chat_persistence::ChannelChatPersistence::new(
                        persistence.tx.clone(),
                    ),
                ),
                chat_event_tx,
                tokio_util::sync::CancellationToken::new(),
            );
            let request = chat_state_handle
                .build_request(
                    vec![],
                    Some(
                        "## Relevant Memory from Past Sessions\n\nPersist this memory reminder."
                            .to_string(),
                    ),
                    true,
                    None,
                    session_info.id.to_string(),
                    "persist-memory".to_string(),
                )
                .await
                .expect("request should build");
            assert!(matches!(request.items.first(), Some(ConversationItem::System(sys)) if sys.content.contains("Persist this memory reminder.")));
            let storage = crate::session::storage::JsonlStorageAdapter::with_explicit_session_dir(
                session_dir.path().to_path_buf(),
            );
            let (flush_tx, flush_rx) = tokio::sync::oneshot::channel();
            persistence
                .tx
                .send(PersistenceMsg::FlushAndAck {
                    respond_to: flush_tx,
                })
                .unwrap();
            flush_rx.await.unwrap().unwrap();
            let loaded = storage
                .load_session_without_updates(&session_info)
                .await
                .unwrap();
            assert!(matches!(loaded.chat_history.first(), Some(ConversationItem::System(sys)) if sys.content.contains("Persist this memory reminder.")));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn first_turn_memory_injection_disabled_does_not_persist_to_chat_history() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let session_dir = tempfile::tempdir().expect("tempdir");
            let session_info = crate::session::info::Info {
                id: acp::SessionId::new("persist-memory-disabled"),
                cwd: session_dir.path().to_string_lossy().to_string(),
            };
            let cwd = AbsPathBuf::new(session_dir.path().to_path_buf()).unwrap();
            let fs = Arc::new(xai_grok_workspace::file_system::MockFs::new(
                cwd.to_path_buf(),
            ));
            let terminal = Arc::new(DummyTerminal {});
            let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
            let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
                "test-memory-disabled".to_string(),
                cwd.to_path_buf(),
                hunk_tx,
                xai_hunk_tracker::TrackingMode::AgentOnly,
                tokio_util::sync::CancellationToken::new(),
            );
            let tool_context =
                ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
            let sampling_client = crate::sampling::Client::new(xai_grok_sampler::SamplerConfig {
                api_key: Some("test-key".to_string()),
                base_url: "http://localhost".to_string(),
                model: "test-model".to_string(),
                context_window: 100_000,
                ..Default::default()
            })
            .expect("sampling client should build for persistence actor");
            let persistence = crate::session::persistence::new_with_explicit_dir(
                &crate::session::info::Info {
                    id: session_info.id.clone(),
                    cwd: session_info.cwd.clone(),
                },
                session_dir.path().to_path_buf(),
                acp::ModelId::new("test-model"),
                sampling_client,
                crate::test_support::TEST_MODEL.to_owned(),
                crate::session::persistence::ExplicitSessionOpen::New {
                    identity: None,
                    next_trace_turn: None,
                },
            )
            .await
            .expect("persistence actor should start");
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
            let initial_conversation = vec![
                ConversationItem::system("sys"),
                ConversationItem::user(
                    "<user_info>OS Version: macos</user_info>\n\n<user_query>hello</user_query>",
                ),
            ];
            let chat_state_handle = xai_chat_state::ChatStateActor::spawn(
                initial_conversation.clone(),
                xai_grok_sampling_types::SamplingConfig {
                    base_url: "http://localhost".to_string(),
                    model: "test".to_string(),
                    context_window: std::num::NonZeroU64::new(100_000).unwrap(),
                    ..Default::default()
                },
                Box::new(
                    crate::session::chat_persistence::ChannelChatPersistence::new(
                        persistence.tx.clone(),
                    ),
                ),
                chat_event_tx,
                tokio_util::sync::CancellationToken::new(),
            );
            chat_state_handle.replace_conversation(initial_conversation);
            let memory_storage =
                crate::session::memory::MemoryStorage::new(session_dir.path(), None);
            memory_storage.ensure_initialized().unwrap();
            let memory_backend_params = crate::session::memory::MemoryBackendParams {
                session_id: session_info.id.to_string(),
                embed_config: None,
                embed_base_url: "http://localhost".to_string(),
                embed_api_key: None,
                search_config: crate::config::MemorySearchConfig::default(),
                watcher: None,
                stale_claim_secs: 60,
                search_source: crate::session::memory::MemorySearchSource::Tool,
                observation_sink: crate::session::memory::noop_memory_observation_sink(),
                embedding_credentials: crate::session::memory::EndpointScopedCredentials::none(),
            };
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
            let actor = Arc::new(SessionActor {
                repo_status_prefetch:
                    crate::session::repo_status_prefix::RepoStatusPrefetchState::default(),
                transient_retry_enabled: true,
                transient_retries_prompt_total: std::cell::Cell::new(0),
                transient_episode_start: std::cell::Cell::new(None),
                status_wake: Default::default(),
                session_info: session_info.clone(),
                auth_method_id: test_auth_method_id("test-auth"),
                model_auth_memo: std::cell::RefCell::new(None),
                jev_ledger: std::cell::RefCell::new(Default::default()),
                attribution_callback: None,
                auth_manager: None,
                is_chat_kind: false,
                state: TokioMutex::new(State {
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
                }),
                notifications: NotificationSender {
                    gateway: GatewaySender::new(gateway_tx),
                    gateway_enabled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                    persistence_tx: persistence.tx.clone(),
                    disk_full: persistence.subscribe_disk_full(),
                    client_caps: crate::session::notifications::SessionClientCaps::new(false, true),
                },
                permissions: PermissionHandle::allow_all(),
                tool_context,
                deny_read_globs: Vec::new(),
                mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
                mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
                delivery_tools: std::cell::RefCell::new(Vec::new()),
                attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
                chat_state_handle,
                unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
                current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
                active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                pending_interactions: std::sync::Arc::new(std::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
                current_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
                turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
                turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
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
                    compaction_mode: xai_chat_state::CompactionMode::Transcript,
                    verbatim_input: true,
                    tool_choice: crate::util::config::CompactionToolChoice::Auto,
                    prefire: crate::session::compaction_config::PrefireState::default(),
                    prefix_released: std::sync::atomic::AtomicBool::new(false),
                    cancel: Default::default(),
                },
                memory: crate::session::memory_state::SessionMemory {
                    configured_mode: Some(crate::config::MemoryMode::Legacy),
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
                    storage: std::cell::RefCell::new(Some(memory_storage)),
                    save_on_end: true,
                    backend_params: Some(memory_backend_params),
                    initial_injection_config: crate::config::MemoryInitialInjectionConfig {
                        enabled: false,
                        min_score: Some(0.8),
                    },
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
                buffering_settings: None,
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
                queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                emit_local_background_tasks: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                active_skill: parking_lot::Mutex::new(None),
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
                workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle()
                    .0,
                workflow_launch_tx: tokio::sync::mpsc::unbounded_channel().0,
                goal_classifier_enabled: false,
                goal_planner_enabled: false,
                goal_summary_enabled: false,
                length_salvage_remote_budget: None,
                goal_verifier_skeptic_count: 1,
                goal_role_models: Default::default(),
                goal_use_current_model_only: false,
                goal_classifier_max_runs:
                    crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT,
                goal_strategist_every: 5,
                goal_reverify_after: crate::session::acp_session::GOAL_REVERIFY_AFTER_DEFAULT,
                goal_plan_reconciled: std::sync::atomic::AtomicBool::new(false),
                pending_classifier_completions: parking_lot::Mutex::new(
                    std::collections::VecDeque::new(),
                ),
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
                extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
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
                vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
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
                pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
                image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
                sampler_handle: xai_grok_sampler::SamplerHandle::noop(),
                sampling_gate: None,
                rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
                image_description_model: crate::test_support::TEST_MODEL.to_owned(),
                image_describe_cache: Arc::new(
                    crate::session::image_describe::ImageDescribeCache::new(),
                ),
                subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
                workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
                trace_config_template: std::cell::RefCell::new(None),
            });
            let _ = actor
                .process_conversation_turn_with_recovery(
                    "disabled-memory",
                    None,
                    None,
                    None,
                    &mut length_salvage::LengthSalvage::new(None),
                    &mut Default::default(),
                )
                .await;
            let (flush_tx, flush_rx) = tokio::sync::oneshot::channel();
            persistence
                .tx
                .send(PersistenceMsg::FlushAndAck {
                    respond_to: flush_tx,
                })
                .unwrap();
            flush_rx.await.unwrap().unwrap();
            let storage = crate::session::storage::JsonlStorageAdapter::with_explicit_session_dir(
                session_dir.path().to_path_buf(),
            );
            let loaded = storage
                .load_session_without_updates(&session_info)
                .await
                .unwrap();
            assert!(
                matches!(loaded.chat_history.first(), Some(ConversationItem::System(sys))
                if sys.content.as_ref() == "sys")
            );
            assert_eq!(
                0,
                actor
                    .memory
                    .injection_count
                    .load(std::sync::atomic::Ordering::Relaxed)
            );
        })
        .await;
}
/// Hard teardown (`kill_background_tasks = true`, the subagent-shutdown path) aborts the running turn AND drains every queued prompt.
/// Each drained prompt gets a `Cancelled` response.
/// Interactive cancel preserves the queue instead; see `cancel_running_task_interactive_preserves_queued_work`.
#[tokio::test(flavor = "current_thread")]
async fn cancel_running_task_teardown_clears_running_and_pending_work() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
            let fs = Arc::new(
                xai_grok_workspace::file_system::MockFs::new(cwd.to_path_buf()),
            );
            let terminal = Arc::new(DummyTerminal {});
            let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
            let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
                "test-cancel".to_string(),
                cwd.to_path_buf(),
                hunk_tx,
                xai_hunk_tracker::TrackingMode::AgentOnly,
                tokio_util::sync::CancellationToken::new(),
            );
            let tool_context = ToolContext::new(
                cwd.clone(),
                None,
                None,
                fs,
                terminal,
                hunk_tracker_handle,
            );
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
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<
                SessionEvent,
            >();
            let agent = test_agent_default().await;
            agent
                .tool_bridge()
                .update_resource(
                    xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                        "running".to_string(),
                    ),
                )
                .await;
            let actor = SessionActor {
                repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(),
                transient_retry_enabled: true,
                transient_retries_prompt_total: std::cell::Cell::new(0),
                transient_episode_start: std::cell::Cell::new(None),
                status_wake: Default::default(),
                session_info: SessionInfo {
                    id: acp::SessionId::new("test-cancel"),
                    cwd: cwd.as_str().to_string(),
                },
                auth_method_id: test_auth_method_id("test-auth"),
                model_auth_memo: std::cell::RefCell::new(None),
                jev_ledger: std::cell::RefCell::new(Default::default()),
                attribution_callback: None,
                auth_manager: None,
                is_chat_kind: false,
                state,
                notifications: NotificationSender::for_tests(
                    GatewaySender::new(gateway_tx),
                    persistence_tx,
                ),
                permissions: PermissionHandle::allow_all(),
                tool_context,
                deny_read_globs: Vec::new(),
                mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
                mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
                delivery_tools: std::cell::RefCell::new(Vec::new()),
                attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
                chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
                unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
                current_prompt_id: std::sync::Arc::new(
                    std::sync::Mutex::new(Some("running".to_string())),
                ),
                active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                pending_interactions: std::sync::Arc::new(
                    std::sync::Mutex::new(std::collections::HashMap::new()),
                ),
                current_prompt_mode: Arc::new(
                    parking_lot::Mutex::new(PromptMode::Agent),
                ),
                turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
                turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
                telemetry_enabled: false,
                supports_backend_search: std::cell::Cell::new(false),
                tool_overrides: std::cell::RefCell::new(None),
                resolved_tool_overrides: std::sync::Arc::new(
                    arc_swap::ArcSwapOption::empty(),
                ),
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
                    force_compact: std::sync::Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    ),
                    context_window_override: None,
                    count: std::sync::atomic::AtomicU64::new(0),
                    auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
                    previous_model: std::cell::Cell::new(None),
                    compaction_mode: xai_chat_state::CompactionMode::Transcript,
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
                    is_flushing: std::sync::Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    ),
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
                    chunks_added: std::sync::Arc::new(
                        std::sync::atomic::AtomicU64::new(0),
                    ),
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
                buffering_settings: None,
                client_identifier: None,
                origin_client: None,
                feedback_manager: Arc::new(FeedbackManager::local_only("test-session")),
                upload_queue: Arc::new(OnceLock::new()),
                sync_loop_cancel: None,
                agent: std::cell::RefCell::new(agent),
                last_reported_branch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                git_head_enabled: false,
                status_line_enabled: std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                models_manager: Default::default(),
                display_cwd: std::sync::OnceLock::new(),
                active_agent_type: parking_lot::Mutex::new(None),
                queue_exit_reminder_on_approved_exit: Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                emit_local_background_tasks: Arc::new(
                    std::sync::atomic::AtomicBool::new(true),
                ),
                active_skill: parking_lot::Mutex::new(None),
                plan_mode: Arc::new(
                    parking_lot::Mutex::new(
                        crate::session::plan_mode::PlanModeTracker::new(
                            std::path::PathBuf::from("/tmp/test-session"),
                        ),
                    ),
                ),
                goal_enabled: false,
                background_workflows_enabled: false,
                goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
                goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(
                    false,
                ),
                goal_tracker: Arc::new(
                    parking_lot::Mutex::new(
                        crate::session::goal_tracker::GoalTracker::new(
                            std::path::PathBuf::from("/tmp/test-session"),
                        ),
                    ),
                ),
                goal_turn_task_ids: parking_lot::Mutex::new(
                    std::collections::HashSet::new(),
                ),
                goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
                goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
                goal_update_rx: std::cell::RefCell::new(None),
                goal_update_tx: tokio::sync::mpsc::unbounded_channel().0,
                workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle()
                    .0,
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
                pending_classifier_completions: parking_lot::Mutex::new(
                    std::collections::VecDeque::new(),
                ),
                goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
                managed_mcp_handle: Default::default(),
                initial_client_mcp_servers: vec![],
                tool_metadata_snapshot: Arc::new(
                    std::sync::Mutex::new(Default::default()),
                ),
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
                extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
                last_announced_local_date: std::cell::Cell::new(
                    chrono::Local::now().date_naive(),
                ),
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
                vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
                hook_load_errors: std::cell::RefCell::new(Vec::new()),
                plugin_registry: std::cell::RefCell::new(None),
                plugin_registry_handle: None,
                events: crate::session::events::EventTracker::new(
                    std::path::Path::new("/tmp"),
                ),
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
                session_turn_active: std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                streaming_turn_capture: parking_lot::Mutex::new(
                    StreamingTurnCapture::default(),
                ),
                stream_apply_span: parking_lot::Mutex::new(None),
                current_turn_span_id: parking_lot::Mutex::new(None),
                turn_stream_drained: parking_lot::Mutex::new(
                    std::collections::HashMap::new(),
                ),
                pending_image_strip: parking_lot::Mutex::new(
                    std::collections::HashMap::new(),
                ),
                image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
                sampler_handle: xai_grok_sampler::SamplerHandle::noop(),
                sampling_gate: None,
                rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
                image_description_model: crate::test_support::TEST_MODEL.to_owned(),
                image_describe_cache: Arc::new(
                    crate::session::image_describe::ImageDescribeCache::new(),
                ),
                subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
                workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
                trace_config_template: std::cell::RefCell::new(None),
            };
            let (tx, rx) = tokio::sync::oneshot::channel();
            let bridge = actor.agent.borrow().tool_bridge().clone();
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(
                    AgentTask::new(
                        "running",
                        tokio::task::spawn_local(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(60))
                                    .await;
                            })
                            .abort_handle(),
                    ),
                );
                state
                    .pending_inputs
                    .push_back(InputItem {
                        prompt_id: "running".into(),
                        prompt_blocks: vec![],
                        prompt_mode: PromptMode::Agent,
                        trace_gcs_config: None,
                        artifact_tracker: None,
                        client_identifier: None,
                        screen_mode: None,
                        verbatim: false,
                        json_schema: None,
                        input_origin: InputOrigin::new(
                            crate::session::PromptOrigin::User,
                        ),
                        task_wake_fallback: None,
                        tool_overrides_update: None,
                        respond_to: tx,
                        persist_ack: None,
                        parsed_prompt_tx: None,
                        initial_child_prompt_ready: None,
                        queue_meta: None,
                        queue_mutation_policy: QueueMutationPolicy::hidden(),
                        send_now: false,
                        traceparent: None,
                    });
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    kill_background_tasks: true,
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            let scoped_prompt_id = bridge
                .read_resource::<
                    xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource,
                >()
                .await;
            assert!(
                    scoped_prompt_id.is_none()
                        || scoped_prompt_id.as_ref().is_some_and(|p| p.0.is_empty()),
                    "CurrentPromptIdResource should be cleared on cancellation"
                );
            assert!(
                    actor.current_prompt_id.lock().expect("current_prompt_id mutex poisoned").is_none(),
                    "current_prompt_id should be cleared on cancellation"
                );
            let state = actor.state.lock().await;
            assert!(state.running_task.is_none());
            assert!(state.pending_inputs.is_empty());
            drop(state);
            let turn_ok = rx
                .await
                .expect("queued prompt should receive cancellation")
                .expect("queued prompt result should be ok");
            assert_eq!(turn_ok.stop_reason, acp::StopReason::Cancelled);
        })
        .await;
}
/// Interactive cancel removes only the running front prompt and preserves every queued prompt so the next one can promote.
/// The running prompt stays at `pending_inputs.front()`, so preserving the whole queue would re-run the cancelled turn.
/// A Ctrl+C / ESC cancel records `MidTurnAbort` on the next real user prompt and consumes that marker exactly once.
#[tokio::test(flavor = "current_thread")]
async fn cancel_records_mid_turn_abort_interrupt_marker() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            let cancelled_strip = xai_grok_sampler::RequestId::from("cancelled-strip");
            let timed_out_strip = xai_grok_sampler::RequestId::from("older-timeout-strip");
            let (stream_tx, _stream_rx) = tokio::sync::oneshot::channel();
            actor.turn_stream_drained.lock().insert(
                cancelled_strip.clone(),
                crate::session::acp_session::StreamOwnership::with_waiter(Some(stream_tx)),
            );
            {
                let mut pending = actor.pending_image_strip.lock();
                pending.insert(
                    cancelled_strip.clone(),
                    PendingImageStrip {
                        urls: vec![std::sync::Arc::from("data:image/png;base64,cancelled")],
                        timed_out: false,
                        applying: false,
                    },
                );
                pending.insert(
                    timed_out_strip.clone(),
                    PendingImageStrip {
                        urls: vec![std::sync::Arc::from("data:image/png;base64,timed-out")],
                        timed_out: true,
                        applying: false,
                    },
                );
            }
            assert_eq!(actor.events.take_prior_interrupt_category(), None);
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert_eq!(
                actor.events.take_prior_interrupt_category(),
                Some(crate::session::events::CancellationCategory::MidTurnAbort),
                "cancel must record a MidTurnAbort interrupt marker"
            );
            assert_eq!(actor.events.take_prior_interrupt_category(), None);
            assert!(
                !actor
                    .turn_stream_drained
                    .lock()
                    .contains_key(&cancelled_strip),
                "cancel must revoke normal stream ownership"
            );
            let pending = actor.pending_image_strip.lock();
            assert!(
                !pending.contains_key(&cancelled_strip),
                "cancel must drop its turn's deferred image-strip state"
            );
            assert!(
                pending.contains_key(&timed_out_strip),
                "cancel must preserve timeout-owned work from an older turn"
            );
        })
        .await;
}
/// With no assistant text, the cancel does not arm the interrupt reminder.
#[tokio::test(flavor = "current_thread")]
async fn cancel_without_assistant_text_skips_interrupt_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            assert!(!actor.events.has_active_tool());
            assert!(!actor.events.take_pending_interrupt_reminder());
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(
                !actor.events.take_pending_interrupt_reminder(),
                "an abort with no assistant text must NOT arm the interrupt reminder"
            );
        })
        .await;
}
/// Send-now is a silent cancel-and-send: the user is continuing, not aborting.
/// Its cancel must arm neither the interrupt envelope (no interrupt lead-in on the continuation turn) nor the MidTurnAbort marker.
/// It must reset the blocking-wait depth so a zombie wait guard from the aborted turn can't auto-send-now-cancel (and drop) the next user prompt.
#[tokio::test(flavor = "current_thread")]
async fn send_now_cancel_arms_no_interrupt_signals_and_resets_wait_depth() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            let zombie_guard = crate::tools::tool_context::BlockingWaitGuard::enter(
                actor.tool_context.blocking_wait_depth.clone(),
            );
            assert!(!actor.events.has_active_tool());
            let mut replay_buffer = ReplayBuffer::new(None);
            let _ = actor.cancel_turn_for_send_now(&mut replay_buffer).await;
            assert!(
                !actor.events.take_pending_interrupt_reminder(),
                "send-now must not arm the interrupt reminder"
            );
            assert_eq!(
                actor.events.take_prior_interrupt_category(),
                None,
                "send-now must not record a MidTurnAbort interrupt marker"
            );
            let depth = &actor.tool_context.blocking_wait_depth;
            assert_eq!(depth.depth(), 0, "cancel must zero the wait window");
            drop(zombie_guard);
            assert_eq!(
                depth.depth(),
                0,
                "a late old-generation guard drop must be a no-op"
            );
        })
        .await;
}
/// Arming the reminder too would signal the abort twice.
/// During a permission prompt the tool-call is committed but NO tool is marked active yet (`has_active_tool()` is false).
/// Gating on the dangling state rather than `had_active_tool` keeps both cases covered.
#[tokio::test(flavor = "current_thread")]
async fn cancel_with_dangling_tool_call_skips_interrupt_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.chat_state_handle.push_assistant_response(
                ConversationItem::assistant_tool_calls(vec![xai_grok_sampling_types::ToolCall {
                    id: "call-1".into(),
                    name: "run_terminal_cmd".into(),
                    arguments: "{}".into(),
                }]),
            );
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            assert!(!actor.events.has_active_tool());
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(
                !actor.events.take_pending_interrupt_reminder(),
                "an abort with a dangling tool call is already covered by the \
                 repair; the reminder must stay disarmed"
            );
        })
        .await;
}
/// Once armed, `maybe_apply_interrupt_envelope` frames the next user query with the interjection envelope exactly once (one-shot).
#[tokio::test(flavor = "current_thread")]
async fn maybe_apply_interrupt_envelope_is_one_shot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.events.set_pending_interrupt_reminder();
            let assembled = "<user_query>\nfollow-up after interrupt\n</user_query>";
            let framed = actor.maybe_apply_interrupt_envelope(assembled.into(), false);
            assert_eq!(framed, frame_user_turn(INTERRUPT_NOTE, assembled));
            assert!(!actor.events.take_pending_interrupt_reminder());
            let again = actor.maybe_apply_interrupt_envelope(assembled.into(), false);
            assert_eq!(again, assembled, "interrupt envelope must be one-shot");
        })
        .await;
}
/// Headless `--verbatim` / ACP `_meta.verbatim` owns the exact prompt.
/// Framing would break that contract; the one-shot still fires so it cannot leak onto a later non-verbatim user turn.
#[tokio::test(flavor = "current_thread")]
async fn maybe_apply_interrupt_envelope_skips_verbatim() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _gateway_rx) = build_actor().await;
            actor.events.set_pending_interrupt_reminder();
            let assembled = "caller-owned follow-up";
            let framed = actor.maybe_apply_interrupt_envelope(assembled.into(), true);
            assert_eq!(framed, assembled, "verbatim text must stay byte-identical");
            assert!(
                !actor.events.take_pending_interrupt_reminder(),
                "verbatim still consumes the one-shot"
            );
        })
        .await;
}
/// Integration: with the one-shot armed, a real user turn driven through `handle_prompt` frames the query in the same envelope as an interjection.
/// The envelope is a lead-in, the `<user_query>` block, and an unfinished-task trailer, rather than a preceding `<system-reminder>`.
/// The test synchronizes on the persist-ack, which fires after the user item is pushed and before the model call.
#[tokio::test(flavor = "current_thread")]
async fn handle_prompt_frames_interrupt_on_user_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            actor.events.set_pending_interrupt_reminder();
            let query = "follow-up after interrupt";
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                query.to_string(),
            ))];
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let actor_for_prompt = actor.clone();
            let prompt_task = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "interrupt-wiring-test",
                        prompt_blocks,
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        false,
                        false,
                        None,
                        Some(ack_tx),
                        None,
                    )
                    .await
            });
            assert!(ack_rx.await.is_ok(), "persist ack should resolve");
            let conv = actor.chat_state_handle.get_conversation().await;
            let user = conv
                .iter()
                .find(|item| {
                    matches!(item, ConversationItem::User(u) if u.synthetic_reason.is_human())
                        && item.text_content().contains(query)
                })
                .expect("the user message must be in the conversation");
            let text = user.text_content();
            let expected_assembled = format!("<user_query>\n{query}\n</user_query>");
            assert_eq!(text, frame_user_turn(INTERRUPT_NOTE, &expected_assembled));
            assert!(!actor.events.take_pending_interrupt_reminder());
            prompt_task.abort();
        })
        .await;
}
/// Integration: a verbatim user turn must stay byte-identical to the caller text even when the interrupt one-shot is armed.
#[tokio::test(flavor = "current_thread")]
async fn handle_prompt_verbatim_skips_interrupt_envelope() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            actor.events.set_pending_interrupt_reminder();
            let query = "caller-owned follow-up";
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                query.to_string(),
            ))];
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let actor_for_prompt = actor.clone();
            let prompt_task = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "interrupt-verbatim-test",
                        prompt_blocks,
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        true,
                        false,
                        None,
                        Some(ack_tx),
                        None,
                    )
                    .await
            });
            assert!(ack_rx.await.is_ok(), "persist ack should resolve");
            let conv = actor.chat_state_handle.get_conversation().await;
            let user = conv
                .iter()
                .find(|item| {
                    matches!(item, ConversationItem::User(u) if u.synthetic_reason.is_human())
                        && item.text_content().contains(query)
                })
                .expect("the user message must be in the conversation");
            assert_eq!(user.text_content(), query);
            assert!(!user.text_content().contains(INTERRUPT_NOTE));
            assert!(!actor.events.take_pending_interrupt_reminder());
            prompt_task.abort();
        })
        .await;
}
/// Send-now must use the full interjection envelope, not the note prefix alone.
/// The envelope is the prefix, the already-wrapped `<user_query>`, and the unfinished-task trailer.
#[tokio::test(flavor = "current_thread")]
async fn handle_prompt_send_now_frames_interjection_envelope() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            let query = "create /tmp/A";
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                query.to_string(),
            ))];
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let actor_for_prompt = actor.clone();
            let prompt_task = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "send-now-envelope-test",
                        prompt_blocks,
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        false,
                        true,
                        None,
                        Some(ack_tx),
                        None,
                    )
                    .await
            });
            assert!(ack_rx.await.is_ok(), "persist ack should resolve");
            let conv = actor.chat_state_handle.get_conversation().await;
            let user = conv
                .iter()
                .find(|item| {
                    matches!(item, ConversationItem::User(u) if u.synthetic_reason.is_human())
                        && item.text_content().contains(query)
                })
                .expect("the send-now user message must be in the conversation");
            let expected_assembled = format!("<user_query>\n{query}\n</user_query>");
            assert_eq!(
                user.text_content(),
                frame_user_turn(
                    xai_interjection_core::INTERJECTION_NOTE,
                    &expected_assembled
                )
            );
            prompt_task.abort();
        })
        .await;
}
/// Integration: a synthetic-origin turn (here `scheduler-fired-*`) can run between the abort and the user's resend.
/// It must NOT consume the one-shot or inject the reminder; the one-shot has to survive to the next *genuine* user turn.
/// This guards the `PromptOrigin::User` gate on the injection call.
#[tokio::test(flavor = "current_thread")]
async fn handle_prompt_synthetic_origin_preserves_interrupt_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = actor_with_persistence_drain().await;
            actor.events.set_pending_interrupt_reminder();
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                "scheduler tick".to_string(),
            ))];
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            let actor_for_prompt = actor.clone();
            let prompt_task = tokio::task::spawn_local(async move {
                actor_for_prompt
                    .handle_prompt(
                        "scheduler-fired-test-1",
                        prompt_blocks,
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        true,
                        false,
                        None,
                        Some(ack_tx),
                        None,
                    )
                    .await
            });
            assert!(ack_rx.await.is_ok(), "persist ack should resolve");
            assert!(
                actor.events.take_pending_interrupt_reminder(),
                "a synthetic-origin turn must NOT consume the interrupt reminder"
            );
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                !conv
                    .iter()
                    .any(|item| item.text_content().contains(INTERRUPT_NOTE)),
                "a synthetic-origin turn must not inject the interrupt envelope"
            );
            prompt_task.abort();
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn cancel_running_task_interactive_preserves_queued_work() {
    use tokio::sync::oneshot::error::TryRecvError;
    fn make_item(
        prompt_id: &str,
        queue_id: &str,
    ) -> (InputItem, tokio::sync::oneshot::Receiver<PromptTurnResult>) {
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        let item = InputItem {
            prompt_id: prompt_id.to_string(),
            prompt_blocks: vec![],
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            json_schema: None,
            input_origin: InputOrigin::new(crate::session::PromptOrigin::User),
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: Some(crate::session::prompt_queue::QueueEntryMeta {
                id: queue_id.to_string(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".to_string(),
                text: String::new(),
                combined_texts: None,
            }),
            queue_mutation_policy: QueueMutationPolicy::editable(),
            send_now: false,
            traceparent: None,
        };
        (item, rx)
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (running_item, mut running_rx) = make_item("running", "running");
            let (q1_item, mut q1_rx) = make_item("q1-pid", "q1");
            let (q2_item, mut q2_rx) = make_item("q2-pid", "q2");
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state.pending_inputs.push_back(running_item);
                state.pending_inputs.push_back(q1_item);
                state.pending_inputs.push_back(q2_item);
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(
                actor
                    .current_prompt_id
                    .lock()
                    .expect("current_prompt_id mutex poisoned")
                    .is_none(),
                "current_prompt_id should be cleared on cancellation"
            );
            let state = actor.state.lock().await;
            assert!(state.running_task.is_none(), "running turn must be aborted");
            let surviving: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                surviving,
                vec!["q1-pid", "q2-pid"],
                "only the running turn is removed; every queued prompt is preserved"
            );
            drop(state);
            assert!(
                matches!(
                    running_rx.try_recv(),
                    Ok(Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        ..
                    }))
                ),
                "running turn must be resolved Cancelled"
            );
            assert!(
                matches!(q1_rx.try_recv(), Err(TryRecvError::Empty)),
                "front queued prompt must remain pending (it runs next), not be cancelled"
            );
            assert!(
                matches!(q2_rx.try_recv(), Err(TryRecvError::Empty)),
                "preserved prompt must remain pending, not be cancelled"
            );
        })
        .await;
}
/// The sweep must leave the running turn's own front slot alone.
/// If the sweep deletes the front, the user prompt shifts to index 0 and the cancel destroys it instead.
/// The message never runs and, since user messages are only persisted when their turn starts, it is silently lost from history.
#[tokio::test(flavor = "current_thread")]
async fn cancel_after_own_completion_sweep_preserves_queued_user_prompt() {
    use tokio::sync::oneshot::error::TryRecvError;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") =
                Some("task-completed-bg-1".to_string());
            let (wake_item, mut wake_rx) = input_with_origin_rx(
                "task-completed-bg-1",
                crate::session::PromptOrigin::TaskCompleted {
                    task_id: "bg-1".to_string(),
                },
            );
            let (user_item, mut user_rx) =
                input_with_origin_rx("user-clarify", crate::session::PromptOrigin::User);
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("task-completed-bg-1"));
                state.pending_inputs.push_back(wake_item);
                state.pending_inputs.push_back(user_item);
            }
            actor
                .drop_pending_items_for_consumed_completions(&["bg-1"])
                .await;
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            let state = actor.state.lock().await;
            let surviving: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                surviving,
                vec!["user-clarify"],
                "the queued user prompt must survive a cancel that lands during \
                 the auto-wake turn"
            );
            drop(state);
            assert!(
                matches!(
                    wake_rx.try_recv(),
                    Ok(Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        ..
                    }))
                ),
                "the front resolved with Cancelled must be the auto-wake turn"
            );
            assert!(
                matches!(user_rx.try_recv(), Err(TryRecvError::Empty)),
                "the user prompt must remain pending (it runs next), not be cancelled"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn interactive_cancel_drops_queued_task_wakes_and_promotes_user() {
    use tokio::sync::oneshot::error::TryRecvError;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let reservations = actor
                .tool_context
                .task_completion_reservations
                .clone()
                .expect("completion reservations");
            reservations.reserve("bg-queued".to_string());
            let actor = Arc::new(actor);
            let (running_item, mut running_rx) =
                input_with_origin_rx("user-running", crate::session::PromptOrigin::User);
            let (mut wake_item, mut wake_rx) = input_with_origin_rx(
                "task-completed-bg-queued",
                crate::session::PromptOrigin::TaskCompleted {
                    task_id: "bg-queued".to_string(),
                },
            );
            wake_item.task_wake_fallback = Some(crate::session::commands::TaskWakeFallback {
                prompt_id: "bash-completed-bg-queued".to_string(),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "completion bg-queued",
                ))],
                source: NotificationSource::BashTaskCompleted {
                    task_id: "bg-queued".to_string(),
                },
            });
            let (queued_user, mut queued_user_rx) =
                input_with_origin_rx("user-next", crate::session::PromptOrigin::User);
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("user-running"));
                state.pending_inputs.push_back(running_item);
                state.pending_inputs.push_back(wake_item);
                state.pending_inputs.push_back(queued_user);
            }
            let cancel = actor.cancel_running_task(crate::session::CancelOptions {
                cancel_subagents: true,
                trigger: Some(crate::session::CancelTrigger::CtrlC),
                user_initiated: true,
                ..Default::default()
            });
            tokio::pin!(cancel);
            tokio::select! {
                _ = &mut cancel => {}
                _ = tokio::task::yield_now() => {
                    assert!(
                        actor.state.try_lock().expect("state lock").notifications_suppressed,
                        "Ctrl+C must arm actor suppression before the first await"
                    );
                    assert!(
                        actor
                            .tool_context
                            .task_wake_suppressed
                            .as_ref()
                            .is_some_and(|gate| gate.get()),
                        "Ctrl+C must arm the reminder gate before the first await"
                    );
                    let _ = cancel.await;
                }
            }
            assert!(
                actor
                    .tool_context
                    .task_wake_suppressed
                    .as_ref()
                    .is_some_and(|gate| gate.get()),
                "Ctrl+C must synchronously arm the reminder gate"
            );
            {
                let state = actor.state.lock().await;
                let remaining: Vec<&str> = state
                    .pending_inputs
                    .iter()
                    .map(|item| item.prompt_id.as_str())
                    .collect();
                assert_eq!(remaining, vec!["user-next"]);
                assert!(matches!(
                    state.pending_notifications.as_slice(),
                    [PendingNotification {
                        source: NotificationSource::BashTaskCompleted { task_id },
                        ..
                    }] if task_id == "bg-queued"
                ));
                assert!(state.notifications_suppressed);
            }
            assert!(matches!(running_rx.try_recv(), Ok(Ok(_))));
            assert!(matches!(
                wake_rx.try_recv(),
                Ok(Ok(crate::session::commands::PromptTurnOk {
                    completion_kind: PromptCompletionKind::RemovedFromQueue,
                    ..
                }))
            ));
            assert!(matches!(
                queued_user_rx.try_recv(),
                Err(TryRecvError::Empty)
            ));
            assert!(reservations.contains("bg-queued"));
            actor.consume_deferred_completions_for_user_turn().await;
            {
                let state = actor.state.lock().await;
                assert!(
                    state.pending_notifications.is_empty(),
                    "the genuine user turn must consume the parked fallback exactly once"
                );
            }
            assert!(!reservations.contains("bg-queued"));
            actor.consume_deferred_completions_for_user_turn().await;
            assert!(
                actor.state.lock().await.pending_notifications.is_empty(),
                "a second user-start drain must not rediscover the completion"
            );
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn ctrl_c_clears_turn_active_before_background_completion_routes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let is_turn_active = Arc::new(std::sync::atomic::AtomicBool::new(true));
            actor.tool_context.is_turn_active = Some(is_turn_active.clone());
            let (running_item, _running_rx) =
                input_with_origin_rx("user-running", crate::session::PromptOrigin::User);
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("user-running"));
                state.pending_inputs.push_back(running_item);
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::CtrlC),
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(!is_turn_active.load(std::sync::atomic::Ordering::Relaxed));
            assert!(actor.state.lock().await.notifications_suppressed);
        })
        .await;
}
async fn assert_stop_trigger_arms_wake_barrier(trigger: &str) {
    use tokio::sync::oneshot::error::TryRecvError;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let reservations = actor
                .tool_context
                .task_completion_reservations
                .clone()
                .expect("completion reservations");
            reservations.reserve("bg-queued".to_string());
            let actor = Arc::new(actor);
            let (running_item, mut running_rx) =
                input_with_origin_rx("user-running", crate::session::PromptOrigin::User);
            let (mut wake_item, mut wake_rx) = input_with_origin_rx(
                "task-completed-bg-queued",
                crate::session::PromptOrigin::TaskCompleted {
                    task_id: "bg-queued".to_string(),
                },
            );
            wake_item.task_wake_fallback = Some(crate::session::commands::TaskWakeFallback {
                prompt_id: "bash-completed-bg-queued".to_string(),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    "completion bg-queued",
                ))],
                source: NotificationSource::BashTaskCompleted {
                    task_id: "bg-queued".to_string(),
                },
            });
            let (queued_user, mut queued_user_rx) =
                input_with_origin_rx("user-next", crate::session::PromptOrigin::User);
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(running_task_stub("user-running"));
                state.pending_inputs.push_back(running_item);
                state.pending_inputs.push_back(wake_item);
                state.pending_inputs.push_back(queued_user);
            }
            let barrier = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    trigger: Some(crate::session::CancelTrigger::from_client(trigger)),
                    user_initiated: true,
                    ..Default::default()
                })
                .await
                .barrier;
            assert_eq!(
                barrier,
                super::cancel::WakeBarrier::Armed,
                "{trigger}: outcome must report the armed barrier"
            );
            assert!(
                actor
                    .tool_context
                    .task_wake_suppressed
                    .as_ref()
                    .is_some_and(|gate| gate.get()),
                "{trigger}: an interactive stop must arm the reminder gate"
            );
            {
                let state = actor.state.lock().await;
                let remaining: Vec<&str> = state
                    .pending_inputs
                    .iter()
                    .map(|item| item.prompt_id.as_str())
                    .collect();
                assert_eq!(
                    remaining,
                    vec!["user-next"],
                    "{trigger}: wake dropped (cannot restart the model), user prompt kept"
                );
                assert!(
                    matches!(
                        state.pending_notifications.as_slice(),
                        [PendingNotification {
                            source: NotificationSource::BashTaskCompleted { task_id },
                            ..
                        }] if task_id == "bg-queued"
                    ),
                    "{trigger}: completion parks as a fallback"
                );
                assert!(
                    state.notifications_suppressed,
                    "{trigger}: suppression stays armed until the user re-engages"
                );
            }
            assert!(
                matches!(running_rx.try_recv(), Ok(Ok(_))),
                "{trigger}: the running turn resolves"
            );
            assert!(
                matches!(
                    wake_rx.try_recv(),
                    Ok(Ok(crate::session::commands::PromptTurnOk {
                        completion_kind: PromptCompletionKind::RemovedFromQueue,
                        ..
                    }))
                ),
                "{trigger}: the queued wake resolves as removed"
            );
            assert!(
                matches!(queued_user_rx.try_recv(), Err(TryRecvError::Empty)),
                "{trigger}: the queued user prompt stays pending"
            );
            assert!(
                reservations.contains("bg-queued"),
                "{trigger}: the dropped wake's completion is reserved for redelivery"
            );
            actor.consume_deferred_completions_for_user_turn().await;
            assert!(
                actor.state.lock().await.pending_notifications.is_empty(),
                "{trigger}: the next user turn consumes the fallback exactly once"
            );
            assert!(!reservations.contains("bg-queued"));
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn stop_gestures_arm_wake_barrier() {
    for trigger in ["esc", "mouse", "dashboard_stop", "some_future_gesture"] {
        assert_stop_trigger_arms_wake_barrier(trigger).await;
    }
}
#[tokio::test(flavor = "current_thread")]
async fn non_stop_cancels_preserve_queued_task_wakes_and_do_not_arm_barrier() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for trigger in [
                Some(crate::session::CancelTrigger::SendNow),
                Some(crate::session::CancelTrigger::SessionDelete),
                None,
            ] {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
                let (persistence_tx, _persistence_rx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                let (running_item, _running_rx) =
                    input_with_origin_rx("user-running", crate::session::PromptOrigin::User);
                let (wake_item, _wake_rx) = input_with_origin_rx(
                    "task-completed-bg-preserved",
                    crate::session::PromptOrigin::TaskCompleted {
                        task_id: "bg-preserved".to_string(),
                    },
                );
                let (queued_user, _queued_user_rx) =
                    input_with_origin_rx("user-next", crate::session::PromptOrigin::User);
                {
                    let mut state = actor.state.lock().await;
                    state.running_task = Some(running_task_stub("user-running"));
                    state.pending_inputs.push_back(running_item);
                    state.pending_inputs.push_back(wake_item);
                    state.pending_inputs.push_back(queued_user);
                }
                let barrier = actor
                    .cancel_running_task(crate::session::CancelOptions {
                        cancel_subagents: true,
                        trigger: trigger.clone(),
                        user_initiated: true,
                        ..Default::default()
                    })
                    .await
                    .barrier;
                assert_eq!(
                    barrier,
                    super::cancel::WakeBarrier::Clear,
                    "non-stop cancel {trigger:?} must report an unarmed barrier"
                );
                let state = actor.state.lock().await;
                let remaining: Vec<&str> = state
                    .pending_inputs
                    .iter()
                    .map(|item| item.prompt_id.as_str())
                    .collect();
                assert_eq!(
                    remaining,
                    vec!["task-completed-bg-preserved", "user-next"],
                    "non-stop cancel {trigger:?} must preserve the queued task wake"
                );
                assert!(
                    !state.notifications_suppressed,
                    "non-stop cancel {trigger:?} must not arm task-wake suppression"
                );
            }
        })
        .await;
}
/// Regression for the cancel-spinner hang.
/// Background: cancel is fire-and-forget on the client; the TUI spinner only returns to idle when the originating `session/prompt` resolves.
/// Earlier the running turn was resolved only when `running_task.is_some()` (`is_running_turn = idx == 0 && had_running_turn`).
#[tokio::test(flavor = "current_thread")]
async fn cancel_resolves_front_when_running_task_is_none() {
    use tokio::sync::oneshot::error::TryRecvError;
    fn make_item(
        prompt_id: &str,
        queue_id: Option<&str>,
    ) -> (InputItem, tokio::sync::oneshot::Receiver<PromptTurnResult>) {
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        let item = InputItem {
            prompt_id: prompt_id.to_string(),
            prompt_blocks: vec![],
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            json_schema: None,
            input_origin: InputOrigin::new(crate::session::PromptOrigin::User),
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: queue_id.map(|id| crate::session::prompt_queue::QueueEntryMeta {
                id: id.to_string(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".to_string(),
                text: String::new(),
                combined_texts: None,
            }),
            queue_mutation_policy: QueueMutationPolicy::editable(),
            send_now: false,
            traceparent: None,
        };
        (item, rx)
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            let (running_item, mut running_rx) = make_item("running", None);
            let (q2_item, mut q2_rx) = make_item("q2-pid", Some("q2"));
            {
                let mut state = actor.state.lock().await;
                state.running_task = None;
                state.pending_inputs.push_back(running_item);
                state.pending_inputs.push_back(q2_item);
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            assert!(
                matches!(
                    running_rx.try_recv(),
                    Ok(Ok(crate::session::commands::PromptTurnOk {
                        stop_reason: acp::StopReason::Cancelled,
                        ..
                    }))
                ),
                "front in-flight prompt must be resolved Cancelled even when running_task is None"
            );
            assert!(
                matches!(q2_rx.try_recv(), Err(TryRecvError::Empty)),
                "deeper queued prompt must remain pending, not be cancelled"
            );
            let state = actor.state.lock().await;
            let surviving: Vec<&str> = state
                .pending_inputs
                .iter()
                .map(|i| i.prompt_id.as_str())
                .collect();
            assert_eq!(
                surviving,
                vec!["q2-pid"],
                "front removed; the rest preserved"
            );
            drop(state);
            assert!(
                actor
                    .current_prompt_id
                    .lock()
                    .expect("current_prompt_id mutex poisoned")
                    .is_none(),
                "current_prompt_id should be cleared on cancellation"
            );
        })
        .await;
}
/// Regression: aborting `running_task` must propagate cancellation to the `SamplerHandle` so the sampler stops emitting.
#[tokio::test(flavor = "current_thread")]
async fn cancel_propagates_to_sampler_handle_so_no_further_emission() {
    use axum::Router;
    use axum::response::sse::{Event, Sse};
    use axum::routing::post;
    use futures_util::stream::{self, StreamExt};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let app = Router::new()
                .route(
                    "/v1/responses",
                    post(|| async {
                        let chunk = serde_json::json!({
                            "type": "response.output_text.delta",
                            "sequence_number": 1,
                            "item_id": "item-1",
                            "output_index": 0,
                            "content_index": 0,
                            "delta": "hi",
                        });
                        let first = Ok::<
                            _,
                            std::convert::Infallible,
                        >(Event::default().data(chunk.to_string()));
                        Sse::new(stream::iter(vec![first]).chain(stream::pending()))
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let cfg = xai_grok_sampler::SamplerConfig {
                api_key: Some("test-key".to_string()),
                base_url: format!("http://{addr}/v1"),
                model: "test-model".to_string(),
                api_backend: xai_grok_sampler::ApiBackend::Responses,
                context_window: 100_000,
                max_retries: Some(0),
                idle_timeout_secs: Some(60),
                ..Default::default()
            };
            let (sampler_event_tx, _sampler_event_rx) = tokio::sync::mpsc::unbounded_channel::<
                xai_grok_sampler::SamplingEvent,
            >();
            let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
                cfg,
                xai_grok_sampler::RetryPolicy::default(),
                sampler_event_tx,
            );
            let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel::<
                xai_acp_lib::AcpClientMessage,
            >();
            let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel::<
                PersistenceMsg,
            >();
            let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
            let fs = Arc::new(
                xai_grok_workspace::file_system::MockFs::new(cwd.to_path_buf()),
            );
            let terminal = Arc::new(DummyTerminal {});
            let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
            let hunk_tracker_handle = xai_hunk_tracker::HunkTrackerActor::spawn(
                "test-cancel-sampler".to_string(),
                cwd.to_path_buf(),
                hunk_tx,
                xai_hunk_tracker::TrackingMode::AgentOnly,
                tokio_util::sync::CancellationToken::new(),
            );
            let tool_context = ToolContext::new(
                cwd.clone(),
                None,
                None,
                fs,
                terminal,
                hunk_tracker_handle,
            );
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
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel::<
                SessionEvent,
            >();
            let agent = test_agent_default().await;
            agent
                .tool_bridge()
                .update_resource(
                    xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                        "running".to_string(),
                    ),
                )
                .await;
            let actor = SessionActor {
                repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(),
                transient_retry_enabled: true,
                transient_retries_prompt_total: std::cell::Cell::new(0),
                transient_episode_start: std::cell::Cell::new(None),
                status_wake: Default::default(),
                session_info: SessionInfo {
                    id: acp::SessionId::new("test-cancel-sampler"),
                    cwd: cwd.as_str().to_string(),
                },
                auth_method_id: test_auth_method_id("test-auth"),
                model_auth_memo: std::cell::RefCell::new(None),
                jev_ledger: std::cell::RefCell::new(Default::default()),
                attribution_callback: None,
                auth_manager: None,
                is_chat_kind: false,
                state,
                notifications: NotificationSender::for_tests(
                    GatewaySender::new(gateway_tx),
                    persistence_tx,
                ),
                permissions: PermissionHandle::allow_all(),
                tool_context,
                deny_read_globs: Vec::new(),
                mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
                mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
                delivery_tools: std::cell::RefCell::new(Vec::new()),
                attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
                chat_state_handle: xai_chat_state::ChatStateHandle::noop(),
                unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
                current_prompt_id: std::sync::Arc::new(
                    std::sync::Mutex::new(Some("running".to_string())),
                ),
                active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                pending_interactions: std::sync::Arc::new(
                    std::sync::Mutex::new(std::collections::HashMap::new()),
                ),
                current_prompt_mode: Arc::new(
                    parking_lot::Mutex::new(PromptMode::Agent),
                ),
                turn_start_prompt_mode: parking_lot::Mutex::new(PromptMode::Agent),
                turn_prompt_mode: Arc::new(parking_lot::Mutex::new(PromptMode::Agent)),
                telemetry_enabled: false,
                supports_backend_search: std::cell::Cell::new(false),
                tool_overrides: std::cell::RefCell::new(None),
                resolved_tool_overrides: std::sync::Arc::new(
                    arc_swap::ArcSwapOption::empty(),
                ),
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
                    force_compact: std::sync::Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    ),
                    context_window_override: None,
                    count: std::sync::atomic::AtomicU64::new(0),
                    auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
                    previous_model: std::cell::Cell::new(None),
                    compaction_mode: xai_chat_state::CompactionMode::Transcript,
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
                    is_flushing: std::sync::Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    ),
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
                    chunks_added: std::sync::Arc::new(
                        std::sync::atomic::AtomicU64::new(0),
                    ),
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
                buffering_settings: None,
                client_identifier: None,
                origin_client: None,
                feedback_manager: Arc::new(FeedbackManager::local_only("test-session")),
                upload_queue: Arc::new(OnceLock::new()),
                sync_loop_cancel: None,
                agent: std::cell::RefCell::new(agent),
                last_reported_branch: std::sync::Arc::new(parking_lot::Mutex::new(None)),
                git_head_enabled: false,
                status_line_enabled: std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                models_manager: Default::default(),
                display_cwd: std::sync::OnceLock::new(),
                active_agent_type: parking_lot::Mutex::new(None),
                queue_exit_reminder_on_approved_exit: Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                emit_local_background_tasks: Arc::new(
                    std::sync::atomic::AtomicBool::new(true),
                ),
                active_skill: parking_lot::Mutex::new(None),
                plan_mode: Arc::new(
                    parking_lot::Mutex::new(
                        crate::session::plan_mode::PlanModeTracker::new(
                            std::path::PathBuf::from("/tmp/test-session"),
                        ),
                    ),
                ),
                goal_enabled: false,
                background_workflows_enabled: false,
                goal_harness_enabled: std::sync::atomic::AtomicBool::new(false),
                goal_harness_availability_reconciled: std::sync::atomic::AtomicBool::new(
                    false,
                ),
                goal_tracker: Arc::new(
                    parking_lot::Mutex::new(
                        crate::session::goal_tracker::GoalTracker::new(
                            std::path::PathBuf::from("/tmp/test-session"),
                        ),
                    ),
                ),
                goal_turn_task_ids: parking_lot::Mutex::new(
                    std::collections::HashSet::new(),
                ),
                goal_continuation_streak: std::sync::atomic::AtomicU32::new(0),
                goal_blocked_streak: std::sync::atomic::AtomicU32::new(0),
                goal_update_rx: std::cell::RefCell::new(None),
                goal_update_tx: tokio::sync::mpsc::unbounded_channel().0,
                workflow_manager: crate::session::workflow::manager::WorkflowManager::test_bundle()
                    .0,
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
                pending_classifier_completions: parking_lot::Mutex::new(
                    std::collections::VecDeque::new(),
                ),
                goal_classifier_in_flight: std::sync::atomic::AtomicBool::new(false),
                managed_mcp_handle: Default::default(),
                initial_client_mcp_servers: vec![],
                tool_metadata_snapshot: Arc::new(
                    std::sync::Mutex::new(Default::default()),
                ),
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
                extension_registry: xai_agent_lifecycle::LocalExtensionRegistry::default(),
                last_announced_local_date: std::cell::Cell::new(
                    chrono::Local::now().date_naive(),
                ),
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
                vcs_kind: xai_grok_workspace::session::git::VcsKind::Git,
                hook_load_errors: std::cell::RefCell::new(Vec::new()),
                plugin_registry: std::cell::RefCell::new(None),
                plugin_registry_handle: None,
                events: crate::session::events::EventTracker::new(
                    std::path::Path::new("/tmp"),
                ),
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
                session_turn_active: std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ),
                streaming_turn_capture: parking_lot::Mutex::new(
                    StreamingTurnCapture::default(),
                ),
                stream_apply_span: parking_lot::Mutex::new(None),
                current_turn_span_id: parking_lot::Mutex::new(None),
                turn_stream_drained: parking_lot::Mutex::new(
                    std::collections::HashMap::new(),
                ),
                pending_image_strip: parking_lot::Mutex::new(
                    std::collections::HashMap::new(),
                ),
                image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
                sampler_handle: sampler_handle.clone(),
                sampling_gate: None,
                rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
                image_description_model: crate::test_support::TEST_MODEL.to_owned(),
                image_describe_cache: Arc::new(
                    crate::session::image_describe::ImageDescribeCache::new(),
                ),
                subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
                workspace_ops: xai_grok_workspace::WorkspaceOps::for_test(),
                trace_config_template: std::cell::RefCell::new(None),
            };
            let request_id = xai_grok_sampler::RequestId::random();
            let request_id_for_task = request_id.clone();
            let sampler_for_task = sampler_handle.clone();
            let request = ConversationRequest {
                items: vec![ConversationItem::User(
                        xai_grok_sampling_types::UserItem {
                            content: vec![xai_grok_sampling_types::ContentPart::Text {
                                text: "hi".into(),
                            }],
                            synthetic_reason: SyntheticReason::Human,
                            ..Default::default()
                        },
                    )],
                ..Default::default()
            };
            let task = tokio::task::spawn_local(async move {
                let _ = sampler_for_task
                    .submit_and_collect(request_id_for_task, request)
                    .await;
            });
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !sampler_handle.is_active(request_id.clone()).await {
                if tokio::time::Instant::now() >= deadline {
                    panic!("sampler never registered the in-flight request");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(
                    AgentTask::new("running", task.abort_handle()),
                );
                state.pending_inputs.push_back(user_item("running", "test"));
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            let mut still_active = true;
            while tokio::time::Instant::now() < deadline {
                if !sampler_handle.is_active(request_id.clone()).await {
                    still_active = false;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                    !still_active,
                    "cancel_running_task did not propagate to the sampler"
                );
            server_task.abort();
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn skill_reminder_deferred_while_turn_running_flushed_when_idle() {
    use xai_grok_tools::types::skill_discovery_tracker::{SkillUpdateEffects, SkillUpdateKind};
    fn effects() -> SkillUpdateEffects {
        SkillUpdateEffects {
            system_reminder: Some("New skill: pdf-tools".into()),
            send_available_commands: false,
            kind: SkillUpdateKind::Discovery,
        }
    }
    async fn reminders_in_conversation(actor: &SessionActor) -> usize {
        actor
            .chat_state_handle
            .get_conversation()
            .await
            .iter()
            .filter(|item| {
                matches!(item, ConversationItem::User(u) if u.content.iter().any(|p| matches!(
                    p,
                    xai_grok_sampling_types::ContentPart::Text { text } if text.contains("pdf-tools")
                )))
            })
            .count()
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (p_tx, _p_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 200_000, 80, gw_tx, p_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("p1".to_string());
            actor.apply_skill_update_effects(effects()).await;
            assert_eq!(
                actor.pending_skill_reminders.lock().len(),
                1,
                "reminder must be stashed while a turn is running"
            );
            assert_eq!(
                reminders_in_conversation(&actor).await,
                0,
                "reminder must not reach the conversation mid-turn"
            );
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = None;
            actor.flush_pending_skill_reminders().await;
            assert!(
                actor.pending_skill_reminders.lock().is_empty(),
                "flush must drain the stash once the turn is over"
            );
            assert_eq!(
                reminders_in_conversation(&actor).await,
                1,
                "flush must deliver the reminder to the conversation"
            );
            actor.apply_skill_update_effects(effects()).await;
            assert!(
                actor.pending_skill_reminders.lock().is_empty(),
                "idle apply must push immediately, not stash"
            );
            assert_eq!(
                reminders_in_conversation(&actor).await,
                2,
                "idle apply must deliver the reminder immediately"
            );
        })
        .await;
}
/// Cancel (Esc/Ctrl+C) with prompts waiting behind the running one: the queue broadcast to clients keeps waiting prompts in order.
/// It drops only the cancelled prompt and leaves the next free to start.
#[tokio::test(flavor = "current_thread")]
async fn cancel_keeps_remaining_queued_prompts_visible_to_clients() {
    fn make_item(prompt_id: &str, queue_id: &str) -> InputItem {
        let (respond_to, _rx) = tokio::sync::oneshot::channel();
        InputItem {
            prompt_id: prompt_id.to_string(),
            prompt_blocks: vec![],
            prompt_mode: PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: false,
            json_schema: None,
            input_origin: InputOrigin::new(crate::session::PromptOrigin::User),
            task_wake_fallback: None,
            tool_overrides_update: None,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            initial_child_prompt_ready: None,
            queue_meta: Some(crate::session::prompt_queue::QueueEntryMeta {
                id: queue_id.to_string(),
                version: 0,
                owner: None,
                last_editor: None,
                kind: "prompt".to_string(),
                text: String::new(),
                combined_texts: None,
            }),
            queue_mutation_policy: QueueMutationPolicy::editable(),
            send_now: false,
            traceparent: None,
        }
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            *actor
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned") = Some("running".to_string());
            {
                let mut state = actor.state.lock().await;
                state.running_task = Some(AgentTask::new(
                    "running",
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    })
                    .abort_handle(),
                ));
                state
                    .pending_inputs
                    .push_back(make_item("running", "running"));
                state.pending_inputs.push_back(make_item("q1-pid", "q1"));
                state.pending_inputs.push_back(make_item("q2-pid", "q2"));
            }
            let _ = actor
                .cancel_running_task(crate::session::CancelOptions {
                    cancel_subagents: true,
                    user_initiated: true,
                    ..Default::default()
                })
                .await;
            let state = actor.state.lock().await;
            let wire = actor.build_queue_wire(&state);
            let wire_ids: Vec<&str> = wire.iter().map(|e| e.id.as_str()).collect();
            assert_eq!(
                wire_ids,
                vec!["q1", "q2"],
                "clients must still see the waiting prompts, in order, cancelled one gone"
            );
            let [first, second] = wire.as_slice() else {
                panic!("expected two wire entries: {wire:?}");
            };
            assert_eq!(first.position, 0, "positions must renumber from 0");
            assert_eq!(second.position, 1);
            assert!(
                actor
                    .current_prompt_id
                    .lock()
                    .expect("current_prompt_id mutex poisoned")
                    .is_none(),
                "cancel must clear current_prompt_id so the next prompt can start"
            );
        })
        .await;
}
