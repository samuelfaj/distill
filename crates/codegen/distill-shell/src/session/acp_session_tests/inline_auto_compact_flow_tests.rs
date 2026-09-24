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
/// Create a minimal SessionActor for testing auto-compact logic.
async fn create_test_actor(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
) -> SessionActor {
    let cwd = AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
    let fs = Arc::new(MockFs::new(cwd.to_path_buf()));
    let terminal = Arc::new(DummyTerminal {});
    let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_tracker_handle = distill_hunk_tracker::HunkTrackerActor::spawn(
        "test-auto-compact".to_string(),
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
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let chat_state_handle = distill_chat_state::ChatStateActor::spawn(
        vec![],
        distill_sampling_types::SamplingConfig {
            base_url: "http://localhost".to_string(),
            model: "test".to_string(),
            context_window: std::num::NonZeroU64::new(context_window)
                .expect("test context_window must be non-zero"),
            ..Default::default()
        },
        Box::new(distill_chat_state::NullChatPersistence),
        event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    chat_state_handle.record_token_usage(total_tokens);
    SessionActor {
        repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(
        ),
        transient_retry_enabled: true,
        transient_retries_prompt_total: std::cell::Cell::new(0),
        transient_episode_start: std::cell::Cell::new(None),
        status_wake: Default::default(),
        active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        session_info: SessionInfo {
            id: acp::SessionId::new("test-auto-compact"),
            cwd: cwd.as_str().to_string(),
        },
        rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
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
        chat_state_handle,
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
            threshold_percent: std::cell::Cell::new(threshold_percent),
            force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_window_override: None,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode: distill_chat_state::CompactionMode::Transcript,
            verbatim_input: true,
            tool_choice: crate::util::config::CompactionToolChoice::Auto,
            route_context_caps: Default::default(),
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
        event_tx: {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            tx
        },
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
        queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
        image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
        sampler_handle: distill_sampler::SamplerHandle::noop(),
        sampling_gate: None,
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        trace_config_template: std::cell::RefCell::new(None),
    }
}
#[tokio::test(flavor = "current_thread")]
async fn test_should_auto_compact_triggers_at_threshold() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(85_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let result =
                actor.should_auto_compact(85_000, std::num::NonZeroU64::new(100_000).unwrap());
            assert!(result.is_some(), "Should trigger at exactly 85%");
            let info = result.unwrap();
            assert_eq!(info.tokens_used, 85_000);
            assert_eq!(info.context_window, 100_000);
            assert_eq!(info.percentage, 85);
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn test_should_auto_compact_below_threshold() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(84_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let result =
                actor.should_auto_compact(84_000, std::num::NonZeroU64::new(100_000).unwrap());
            assert!(result.is_none(), "Should NOT trigger at 84%");
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn test_check_auto_compact_needed_uses_state() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(90_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let result = actor.check_auto_compact_needed().await;
            assert!(result.is_some(), "Should trigger at 90%");
            let info = result.unwrap();
            assert_eq!(info.percentage, 90);
        })
        .await;
}
/// Overriding context_window on the sampling config changes auto-compact behavior.
/// A forked session must use the new model's context window, not the source session's, or auto-compact fires at the wrong threshold.
#[tokio::test(flavor = "current_thread")]
async fn test_context_window_override_affects_auto_compact() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(86_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let result = actor.check_auto_compact_needed().await;
            assert!(result.is_some(), "Should trigger at 86% of 100K window");
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = "larger-model".to_string();
                cfg.context_window = std::num::NonZeroU64::new(200_000).unwrap();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            let result = actor.check_auto_compact_needed().await;
            assert!(
                result.is_none(),
                "Should NOT trigger at 43% of 200K window after context_window override"
            );
        })
        .await;
}
/// Test the reverse direction: overriding to a smaller context window should make auto-compact trigger sooner.
#[tokio::test(flavor = "current_thread")]
async fn test_context_window_override_to_smaller_triggers_compact() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(86_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let result = actor.check_auto_compact_needed().await;
            assert!(result.is_none(), "Should NOT trigger at 43% of 200K window");
            if let Some(mut cfg) = actor.chat_state_handle.get_sampling_config().await {
                cfg.model = "smaller-model".to_string();
                cfg.context_window = std::num::NonZeroU64::new(100_000).unwrap();
                actor.chat_state_handle.update_sampling_config(cfg);
            }
            let result = actor.check_auto_compact_needed().await;
            assert!(
                result.is_some(),
                "Should trigger at 86% of 100K window after context_window override"
            );
        })
        .await;
}
/// `handle_model_metadata_update` must reject a smaller context_window from a response header and accept a larger one.
#[tokio::test(flavor = "current_thread")]
async fn test_response_header_context_window_downgrade_rejected() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(200_000, 500_000, 85, gateway_tx, persistence_tx).await;
            let cfg_before = actor.chat_state_handle.get_sampling_config().await.unwrap();
            assert_eq!(cfg_before.context_window.get(), 500_000);
            actor
                .handle_model_metadata_update(crate::sampling::ResponseModelMetadata {
                    context_window: Some(256_000),
                    max_completion_tokens: None,
                    models_etag: None,
                })
                .await;
            let cfg_after = actor.chat_state_handle.get_sampling_config().await.unwrap();
            assert_eq!(
                cfg_after.context_window.get(),
                500_000,
                "context_window must NOT be downgraded by response header"
            );
            actor
                .handle_model_metadata_update(crate::sampling::ResponseModelMetadata {
                    context_window: Some(1_000_000),
                    max_completion_tokens: None,
                    models_etag: None,
                })
                .await;
            let cfg_upgraded = actor.chat_state_handle.get_sampling_config().await.unwrap();
            assert_eq!(
                cfg_upgraded.context_window.get(),
                1_000_000,
                "context_window upgrade from response header must be accepted"
            );
        })
        .await;
}
#[allow(clippy::field_reassign_with_default)]
async fn create_test_actor_with_memory(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
    memory_config: Option<crate::config::MemoryConfig>,
) -> SessionActor {
    let tmp = tempfile::TempDir::new().unwrap();
    let cwd_path = tmp.path().to_path_buf();
    let cwd = AbsPathBuf::new(cwd_path.clone()).unwrap();
    let fs = Arc::new(MockFs::new(cwd.to_path_buf()));
    let terminal = Arc::new(DummyTerminal {});
    let (hunk_tx, _) = tokio::sync::mpsc::unbounded_channel();
    let hunk_tracker_handle = distill_hunk_tracker::HunkTrackerActor::spawn(
        "test-memory".to_string(),
        cwd.to_path_buf(),
        hunk_tx,
        distill_hunk_tracker::TrackingMode::AgentOnly,
        tokio_util::sync::CancellationToken::new(),
    );
    let tool_context = ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
    let memory_storage = memory_config
        .as_ref()
        .filter(|mc| mc.enabled)
        .map(|_| crate::session::memory::MemoryStorage::new(&cwd_path, None));
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
    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let chat_state_handle = distill_chat_state::ChatStateActor::spawn(
        vec![],
        distill_sampling_types::SamplingConfig {
            base_url: "http://localhost".to_string(),
            model: "test".to_string(),
            context_window: std::num::NonZeroU64::new(context_window)
                .expect("test context_window must be non-zero"),
            ..Default::default()
        },
        Box::new(distill_chat_state::NullChatPersistence),
        event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    chat_state_handle.record_token_usage(total_tokens);
    std::mem::forget(tmp);
    let memory_initial_injection_config = memory_config
        .as_ref()
        .map_or_else(Default::default, |mc| mc.initial_injection.clone());
    SessionActor {
        repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(
        ),
        transient_retry_enabled: true,
        transient_retries_prompt_total: std::cell::Cell::new(0),
        transient_episode_start: std::cell::Cell::new(None),
        status_wake: Default::default(),
        active_work: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        session_info: SessionInfo {
            id: acp::SessionId::new("test-memory"),
            cwd: cwd.as_str().to_string(),
        },
        rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
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
        chat_state_handle,
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        current_prompt_id: std::sync::Arc::new(std::sync::Mutex::new(None)),
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
            threshold_percent: std::cell::Cell::new(threshold_percent),
            force_compact: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            context_window_override: None,
            count: std::sync::atomic::AtomicU64::new(0),
            auto_compact_suppressed: std::sync::atomic::AtomicU8::new(0),
            previous_model: std::cell::Cell::new(None),
            compaction_mode: distill_chat_state::CompactionMode::Transcript,
            verbatim_input: true,
            tool_choice: crate::util::config::CompactionToolChoice::Auto,
            route_context_caps: Default::default(),
            prefire: crate::session::compaction_config::PrefireState::default(),
            prefix_released: std::sync::atomic::AtomicBool::new(false),
            cancel: Default::default(),
        },
        memory: crate::session::memory_state::SessionMemory {
            configured_mode: memory_config.as_ref().map(|mc| mc.mode),
            v2_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.v2),
            configured_storage: None,
            process_disabled: false,
            config_opt_out: false,
            v2_legacy_carryover: false,
            prompt_sync_pending: std::sync::atomic::AtomicBool::new(false),
            flush_config: memory_config
                .as_ref()
                .map_or_else(Default::default, |mc| mc.flush.clone()),
            is_flushing: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            capture_worker: std::cell::RefCell::new(None),
            dream_workers: crate::session::memory_state::V2DreamWorkers::default(),
            last_capture_failure: std::cell::RefCell::new(None),
            last_flush_compaction: std::sync::atomic::AtomicU64::new(0),
            storage: std::cell::RefCell::new(memory_storage),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: memory_initial_injection_config,
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
        idle_flush_timeout: memory_config
            .as_ref()
            .and_then(|mc| mc.flush.idle_timeout_secs)
            .map(std::time::Duration::from_secs),
        dream_check_timeout: memory_config
            .as_ref()
            .filter(|mc| mc.dream.enabled)
            .and_then(|mc| mc.dream.check_interval_secs)
            .filter(|&s| s > 0)
            .map(std::time::Duration::from_secs),
        last_idle_flush_conversation_len: std::sync::atomic::AtomicUsize::new(0),
        event_tx: {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            tx
        },
        buffering_settings: None,
        client_identifier: None,
        origin_client: None,
        feedback_manager: Arc::new(FeedbackManager::local_only("test-memory")),
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
        pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
        image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
        sampler_handle: distill_sampler::SamplerHandle::noop(),
        sampling_gate: None,
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(HashMap::new()),
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        trace_config_template: std::cell::RefCell::new(None),
    }
}
/// Unit test of the `compare_exchange` pattern `run_memory_flush` uses to prevent concurrent flushes.
/// Exercises the acquire/reject/release/re-acquire cycle on a standalone `AtomicBool`.
/// A full `SessionActor` would need a sampling client and a persistence channel.
#[test]
fn test_is_flushing_compare_exchange_prevents_double_entry() {
    let is_flushing = std::sync::atomic::AtomicBool::new(false);
    assert!(
        is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok(),
        "first compare_exchange should succeed"
    );
    assert!(
        is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_err(),
        "second compare_exchange should fail — flush already in progress"
    );
    is_flushing.store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(
        is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok(),
        "should succeed after release"
    );
}
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::field_reassign_with_default)]
async fn test_dream_check_timeout_from_config() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel();
            let (persistence_tx, _) = mpsc::unbounded_channel();
            let mut config = crate::config::MemoryConfig::default();
            config.enabled = true;
            config.dream.enabled = true;
            config.dream.check_interval_secs = Some(600);
            let actor = create_test_actor_with_memory(
                50_000,
                100_000,
                85,
                gateway_tx.clone(),
                persistence_tx,
                Some(config),
            )
            .await;
            assert_eq!(
                actor.dream_check_timeout,
                Some(std::time::Duration::from_secs(600))
            );
            let (persistence_tx2, _) = mpsc::unbounded_channel();
            let mut config2 = crate::config::MemoryConfig::default();
            config2.enabled = true;
            config2.dream.enabled = true;
            config2.dream.check_interval_secs = None;
            let actor2 = create_test_actor_with_memory(
                50_000,
                100_000,
                85,
                gateway_tx.clone(),
                persistence_tx2,
                Some(config2),
            )
            .await;
            assert_eq!(actor2.dream_check_timeout, None);
            let (persistence_tx3, _) = mpsc::unbounded_channel();
            let mut config3 = crate::config::MemoryConfig::default();
            config3.enabled = true;
            config3.dream.enabled = false;
            config3.dream.check_interval_secs = Some(600);
            let actor3 = create_test_actor_with_memory(
                50_000,
                100_000,
                85,
                gateway_tx.clone(),
                persistence_tx3,
                Some(config3),
            )
            .await;
            assert_eq!(actor3.dream_check_timeout, None);
            let (persistence_tx4, _) = mpsc::unbounded_channel();
            let mut config4 = crate::config::MemoryConfig::default();
            config4.enabled = true;
            config4.dream.enabled = true;
            config4.dream.check_interval_secs = Some(0);
            let actor4 = create_test_actor_with_memory(
                50_000,
                100_000,
                85,
                gateway_tx,
                persistence_tx4,
                Some(config4),
            )
            .await;
            assert_eq!(actor4.dream_check_timeout, None);
        })
        .await;
}
/// Verify that `last_idle_flush_conversation_len` is reset after compaction shrinks the conversation.
/// Without the reset the interval flush guard (`current_len > last_len`) stays false.
/// The compacted conversation is shorter than the stored pre-compaction length.
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::field_reassign_with_default)]
async fn test_idle_flush_conversation_len_reset_after_compaction() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel();
            let (persistence_tx, _) = mpsc::unbounded_channel();
            let mut config = crate::config::MemoryConfig::default();
            config.enabled = true;
            config.flush.idle_timeout_secs = Some(60);
            let actor = create_test_actor_with_memory(
                50_000,
                100_000,
                85,
                gateway_tx,
                persistence_tx,
                Some(config),
            )
            .await;
            for _ in 0..80 {
                actor
                    .chat_state_handle
                    .push_user_message(ConversationItem::user("hello".to_string()));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            actor
                .last_idle_flush_conversation_len
                .store(80, std::sync::atomic::Ordering::Relaxed);
            {
                let current_len = actor.chat_state_handle.get_conversation_len().await;
                let last_len = actor
                    .last_idle_flush_conversation_len
                    .load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(current_len, 80);
                assert!(
                    current_len <= last_len,
                    "guard should block: no new messages"
                );
            }
            {
                let compacted = vec![ConversationItem::user("compacted summary".to_string())];
                let new_len = compacted.len();
                actor
                    .chat_state_handle
                    .replace_conversation_for_compaction(compacted);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                actor
                    .last_idle_flush_conversation_len
                    .store(new_len, std::sync::atomic::Ordering::Relaxed);
            }
            {
                actor
                    .chat_state_handle
                    .push_user_message(ConversationItem::user("new message".to_string()));
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let current_len = actor.chat_state_handle.get_conversation_len().await;
                let last_len = actor
                    .last_idle_flush_conversation_len
                    .load(std::sync::atomic::Ordering::Relaxed);
                assert_eq!(current_len, 2, "summary + new message");
                assert_eq!(last_len, 1, "reset to post-compaction length");
                assert!(
                    current_len > last_len,
                    "guard should allow flush after compaction + new message"
                );
            }
        })
        .await;
}
fn api_error_with_context_window(context_window: u64) -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        kind: distill_sampler::SamplingErrorKind::Api,
        status_code: Some(400),
        message: "prompt is too long".into(),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: Some(crate::sampling::ResponseModelMetadata {
            context_window: Some(context_window),
            max_completion_tokens: None,
            models_etag: None,
        }),
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: distill_sampling_types::SentCredential::Unknown,
    }
}
/// Primary scenario: remote settings shrinks the context window mid-session.
/// The shell's last-known token count (214K) exceeds the new limit (200K), so should_compact_on_error must return true and the session can recover.
#[tokio::test(flavor = "current_thread")]
async fn test_compact_on_error_triggers_when_tokens_exceed_new_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(214_000, 1_000_000, 85, gateway_tx, persistence_tx).await;
            let err = api_error_with_context_window(200_000);
            assert!(actor.should_compact_on_error(&err).await);
        })
        .await;
}
/// When tracked tokens are within the new limit, the error was not a context overflow, so do not compact.
#[tokio::test(flavor = "current_thread")]
async fn test_compact_on_error_no_trigger_when_tokens_within_new_window() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(150_000, 1_000_000, 85, gateway_tx, persistence_tx).await;
            let err = api_error_with_context_window(200_000);
            assert!(!actor.should_compact_on_error(&err).await);
        })
        .await;
}
/// If the proxy hasn't been updated yet, model_metadata is None, and the check must be a no-op for backwards compatibility.
#[tokio::test(flavor = "current_thread")]
async fn test_compact_on_error_noop_without_model_metadata() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(500_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let err = distill_sampler::SamplingErrorInfo {
                kind: distill_sampler::SamplingErrorKind::Api,
                status_code: Some(400),
                message: "prompt is too long".into(),
                is_retryable: false,
                retry_after_secs: None,
                should_retry: None,
                error_code: None,
                model_metadata: None,
                empty_response_context: None,
                doom_loop_triggers: None,
                doom_loop_aborted_at_chunk: None,
                credential: distill_sampling_types::SentCredential::Unknown,
            };
            assert!(!actor.should_compact_on_error(&err).await);
        })
        .await;
}
/// A fresh session emits `x-compactions-remaining: 1`; once the chat-state reflects a compaction, the next reconstructed config emits `0`.
#[tokio::test(flavor = "current_thread")]
async fn compactions_remaining_header_flips_after_compaction() {
    use distill_sampling_types::CompactionsRemaining;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _g) = mpsc::unbounded_channel();
            let (persistence_tx, _p) = mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor
                .compactions_remaining
                .set(Some(CompactionsRemaining::Dynamic(true)));
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compactions-remaining")
                    .cloned()
                    .as_deref(),
                Some("1"),
                "fresh session emits x-compactions-remaining: 1"
            );
            actor.chat_state_handle.record_compaction_at(0);
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compactions-remaining")
                    .cloned()
                    .as_deref(),
                Some("0"),
                "after a compaction the header flips to 0"
            );
        })
        .await;
}
/// `Fixed(n)` sends the constant `n` and never flips: the header stays the same across a compaction, unlike the dynamic variant's 1 to 0.
#[tokio::test(flavor = "current_thread")]
async fn compactions_remaining_fixed_does_not_flip_after_compaction() {
    use distill_sampling_types::CompactionsRemaining;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _g) = mpsc::unbounded_channel();
            let (persistence_tx, _p) = mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor
                .compactions_remaining
                .set(Some(CompactionsRemaining::Fixed(1)));
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compactions-remaining")
                    .cloned()
                    .as_deref(),
                Some("1"),
                "fixed constant emits x-compactions-remaining: 1 on a fresh session"
            );
            actor.chat_state_handle.record_compaction_at(0);
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compactions-remaining")
                    .cloned()
                    .as_deref(),
                Some("1"),
                "fixed constant stays 1 after a compaction (does not flip)"
            );
        })
        .await;
}
/// With the calculated form enabled, a fresh session emits `x-compaction-at = context_window * threshold_percent / 100`.
/// Once the chat-state reflects a compaction, the next reconstructed config drops the header.
#[tokio::test(flavor = "current_thread")]
async fn compaction_at_tokens_header_flips_after_compaction() {
    use distill_sampling_types::CompactionAtTokens;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _g) = mpsc::unbounded_channel();
            let (persistence_tx, _p) = mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor
                .compaction_at_tokens
                .set(Some(CompactionAtTokens::Enabled(true)));
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compaction-at")
                    .cloned()
                    .as_deref(),
                Some("217600"),
                "fresh session emits the calculated x-compaction-at"
            );
            actor.chat_state_handle.record_compaction_at(0);
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compaction-at")
                    .is_none(),
                "after a compaction the x-compaction-at header is dropped"
            );
        })
        .await;
}
/// `Fixed(n)` sends the exact constant; the default (`None`) never emits the header.
#[tokio::test(flavor = "current_thread")]
async fn compaction_at_tokens_fixed_and_disabled() {
    use distill_sampling_types::CompactionAtTokens;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _g) = mpsc::unbounded_channel();
            let (persistence_tx, _p) = mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compaction-at")
                    .is_none(),
                "header is not sent when disabled (None) by default"
            );
            actor
                .compaction_at_tokens
                .set(Some(CompactionAtTokens::Fixed(367_000)));
            assert_eq!(
                actor
                    .reconstruct_full_config()
                    .await
                    .extra_headers
                    .get("x-compaction-at")
                    .cloned()
                    .as_deref(),
                Some("367000"),
                "fixed override sends the exact constant"
            );
        })
        .await;
}
