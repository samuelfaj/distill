// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(dead_code)]
use super::*;
use distill_tools::implementations::distill::task::types::{
    SubagentCompletionSummary, SubagentSnapshot, SubagentSnapshotStatus,
};
pub(crate) fn at<T>(xs: &[T], i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}
pub(crate) fn dq_at<T>(xs: &std::collections::VecDeque<T>, i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}
const JSON_NULL: serde_json::Value = serde_json::Value::Null;
pub(crate) fn j<'a>(v: &'a serde_json::Value, k: &str) -> &'a serde_json::Value {
    v.get(k).unwrap_or(&JSON_NULL)
}
pub(crate) fn completion_identity(actor: &SessionActor) -> std::rc::Rc<()> {
    actor
        .state
        .try_lock()
        .expect("uncontended test state")
        .running_task
        .as_ref()
        .map(|task| task.identity.clone())
        .unwrap_or_else(|| std::rc::Rc::new(()))
}
pub(crate) fn test_auth_method_id(id: &str) -> crate::agent::auth_method::SharedAuthMethodId {
    crate::agent::auth_method::new_shared_auth_method_id(Some(acp::AuthMethodId::new(id)))
}
/// True when `events.jsonl` text `log` has a line of `type == ty` whose parsed JSON satisfies `predicate`.
pub(crate) fn has_event_with(
    log: &str,
    ty: &str,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> bool {
    log.lines().any(|line| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        v.get("type").and_then(|t| t.as_str()) == Some(ty) && predicate(&v)
    })
}
#[cfg(test)]
pub(crate) fn noop_observability_bridge() -> distill_computer_hub_sdk::ObservabilityBridge {
    distill_computer_hub_sdk::ObservabilityBridge::new(
        None,
        distill_tool_protocol::SessionId::new("test").expect("valid"),
    )
}
#[cfg(test)]
pub(crate) async fn test_agent_default() -> distill_agent::Agent {
    test_agent_with_tools(vec![]).await
}
#[cfg(test)]
pub(crate) async fn test_agent_backend_search(
    hosted_tools: Vec<distill_sampling_types::HostedTool>,
) -> distill_agent::Agent {
    let base = test_agent_default().await;
    distill_agent::Agent::new(
        base.definition().clone(),
        distill_agent::PromptContext::default(),
        String::new(),
        base.tool_bridge().clone(),
        distill_agent::ReminderPolicy::default(),
        distill_agent::CompactionPolicy::default(),
        hosted_tools,
        true,
    )
}
#[cfg(test)]
pub(crate) async fn test_agent_with_goal_tool() -> distill_agent::Agent {
    use distill_tools::implementations::distill::update_goal::UpdateGoalTool;
    use distill_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![ToolConfig::for_tool::<UpdateGoalTool>()]).await
}
#[cfg(test)]
pub(crate) async fn test_distill_agent_with_todo() -> distill_agent::Agent {
    use distill_tools::implementations::distill::todo::TodoWriteTool;
    use distill_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![ToolConfig::for_tool::<TodoWriteTool>()]).await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_active_message_tool() -> distill_agent::Agent {
    use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageTool;
    use distill_tools::implementations::distill::todo::TodoWriteTool;
    use distill_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![
        ToolConfig::for_tool::<SendSubagentMessageTool>(),
        ToolConfig::for_tool::<TodoWriteTool>(),
    ])
    .await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_plan_tools() -> distill_agent::Agent {
    use distill_tools::implementations::distill::enter_plan_mode::EnterPlanModeTool;
    use distill_tools::implementations::distill::exit_plan_mode::ExitPlanModeTool;
    use distill_tools::registry::types::ToolConfig;
    test_agent_with_tools(vec![
        ToolConfig::for_tool::<EnterPlanModeTool>(),
        ToolConfig::for_tool::<ExitPlanModeTool>(),
    ])
    .await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_tools(
    tools: Vec<distill_tools::registry::types::ToolConfig>,
) -> distill_agent::Agent {
    test_agent_from_config(
        distill_tools::registry::types::ToolServerConfig {
            tools,
            behavior_preset: None,
        },
        distill_agent::AgentDefinition::default_distill(),
        std::sync::Arc::new(distill_tools::computer::local::LocalTerminalBackend::new()),
    )
    .await
}
#[cfg(test)]
pub(crate) async fn test_agent_with_user_message_template(
    template: distill_agent::prompt::user_message::UserMessageTemplate,
) -> distill_agent::Agent {
    let mut definition = distill_agent::AgentDefinition::default_distill();
    definition.user_message_template = template;
    test_agent_from_config(
        distill_tools::registry::types::ToolServerConfig {
            tools: vec![],
            behavior_preset: None,
        },
        definition,
        std::sync::Arc::new(distill_tools::computer::local::LocalTerminalBackend::new()),
    )
    .await
}
#[cfg(test)]
async fn test_agent_from_config(
    config: distill_tools::registry::types::ToolServerConfig,
    definition: distill_agent::AgentDefinition,
    backend: std::sync::Arc<dyn distill_tools::computer::types::TerminalBackend>,
) -> distill_agent::Agent {
    use distill_tools::computer::local::LocalFs;
    use distill_tools::computer::types::AsyncFileSystem;
    use distill_tools::notification::ToolNotificationHandle;
    use distill_tools::registry::types::SessionContext;
    let builder = crate::tools::bridge::ToolBridge::get_builder();
    let fs: std::sync::Arc<dyn AsyncFileSystem> = std::sync::Arc::new(LocalFs);
    let ctx = SessionContext {
        backend,
        fs,
        cwd: std::path::PathBuf::from("/tmp"),
        session_folder: std::env::temp_dir().join("grok-test"),
        session_env: std::sync::Arc::new(std::collections::HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: std::path::PathBuf::from("/tmp/tool_state.json"),
        memory_backend: None,
        web_search_config: Default::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: distill_tools::reminders::DEFAULT_REMINDER_TAG,
    };
    let tool_bridge = crate::tools::bridge::ToolBridge::finalize_builder(builder, config, ctx)
        .await
        .expect("finalize_builder should succeed for tests");
    #[allow(clippy::arc_with_non_send_sync)]
    let tool_bridge = std::sync::Arc::new(tool_bridge);
    distill_agent::Agent::new(
        definition,
        distill_agent::PromptContext::default(),
        String::new(),
        tool_bridge,
        distill_agent::ReminderPolicy::default(),
        distill_agent::CompactionPolicy::default(),
        vec![],
        false,
    )
}
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct DummyTerminal;
#[cfg(test)]
#[async_trait::async_trait]
impl crate::terminal::AsyncTerminalRunner for DummyTerminal {
    async fn run(
        &self,
        _request: crate::terminal::runner::TerminalRunRequest,
    ) -> Result<crate::terminal::runner::TerminalRunResult, crate::terminal::runner::TerminalError>
    {
        Err(crate::terminal::runner::TerminalError::Other(
            "dummy terminal".into(),
        ))
    }
}
#[cfg(test)]
pub(crate) async fn create_test_actor_ex(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    create_test_actor_with_chat_persistence(
        total_tokens,
        context_window,
        threshold_percent,
        gateway_tx,
        persistence_tx,
        Box::new(distill_chat_state::NullChatPersistence),
    )
    .await
}
#[cfg(test)]
pub(crate) async fn create_test_actor_with_chat_persistence(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
    chat_persistence: Box<dyn distill_chat_state::ChatPersistence>,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    create_test_actor_inner(
        total_tokens,
        context_window,
        threshold_percent,
        gateway_tx,
        persistence_tx,
        Arc::new(DummyTerminal {}),
        chat_persistence,
    )
    .await
}
#[cfg(test)]
pub(crate) async fn create_test_actor_with_terminal(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
    terminal: Arc<dyn crate::terminal::AsyncTerminalRunner>,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    create_test_actor_inner(
        total_tokens,
        context_window,
        threshold_percent,
        gateway_tx,
        persistence_tx,
        terminal,
        Box::new(distill_chat_state::NullChatPersistence),
    )
    .await
}
#[cfg(test)]
async fn create_test_actor_inner(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
    terminal: Arc<dyn crate::terminal::AsyncTerminalRunner>,
    chat_persistence: Box<dyn distill_chat_state::ChatPersistence>,
) -> (
    SessionActor,
    tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let cwd = distill_paths::AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap();
    let fs = Arc::new(distill_workspace::file_system::MockFs::new(
        cwd.to_path_buf(),
    ));
    let (hunk_tx, _hunk_rx) = tokio::sync::mpsc::unbounded_channel();
    let hunk_tracker_handle = distill_hunk_tracker::HunkTrackerActor::spawn(
        "test-actor".to_string(),
        cwd.to_path_buf(),
        hunk_tx,
        distill_hunk_tracker::TrackingMode::AgentOnly,
        tokio_util::sync::CancellationToken::new(),
    );
    let mut tool_context =
        ToolContext::new(cwd.clone(), None, None, fs, terminal, hunk_tracker_handle);
    tool_context.task_completion_reservations =
        Some(distill_tools::reminders::task_completion::TaskCompletionReservations::default());
    tool_context.task_wake_suppressed =
        Some(distill_tools::reminders::task_completion::TaskWakeSuppressed::default());
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
    let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let chat_state_handle = distill_chat_state::ChatStateActor::spawn(
        vec![],
        distill_sampling_types::SamplingConfig {
            base_url: "http://localhost".to_string(),
            model: "test".to_string(),
            context_window: std::num::NonZeroU64::new(context_window)
                .expect("test context_window must be non-zero"),
            ..Default::default()
        },
        chat_persistence,
        chat_event_tx,
        tokio_util::sync::CancellationToken::new(),
    );
    chat_state_handle.record_token_usage(total_tokens);
    let actor = SessionActor {
        repo_status_prefetch: crate::session::repo_status_prefix::RepoStatusPrefetchState::default(
        ),
        transient_retry_enabled: true,
        transient_retries_prompt_total: std::cell::Cell::new(0),
        transient_episode_start: std::cell::Cell::new(None),
        status_wake: Default::default(),
        session_info: SessionInfo {
            id: acp::SessionId::new("test-actor"),
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
        notifications: NotificationSender::for_tests(
            GatewaySender::new(gateway_tx),
            persistence_tx,
        ),
        permissions: distill_workspace::permission::PermissionHandle::allow_all(),
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
    if let Some(reservations) = actor.tool_context.task_completion_reservations.clone() {
        actor
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(reservations)
            .await;
    }
    if let Some(gate) = actor.tool_context.task_wake_suppressed.clone() {
        actor
            .agent
            .borrow()
            .tool_bridge()
            .update_resource(gate)
            .await;
    }
    (actor, event_rx)
}
#[cfg(test)]
pub(crate) async fn create_test_actor(
    total_tokens: u64,
    context_window: u64,
    threshold_percent: u8,
    gateway_tx: tokio::sync::mpsc::UnboundedSender<distill_acp_lib::AcpClientMessage>,
    persistence_tx: tokio::sync::mpsc::UnboundedSender<PersistenceMsg>,
) -> SessionActor {
    create_test_actor_ex(
        total_tokens,
        context_window,
        threshold_percent,
        gateway_tx,
        persistence_tx,
    )
    .await
    .0
}
#[cfg(test)]
pub(crate) fn user_item_with_rx(
    id: &str,
    owner: &str,
) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let text = format!("text for {id}");
    let item = InputItem {
        prompt_id: id.to_string(),
        prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(text.clone()))],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: Some(owner.to_string()),
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
            id: id.to_string(),
            version: 0,
            owner: Some(owner.to_string()),
            last_editor: None,
            kind: "prompt".to_string(),
            text,
            combined_texts: None,
        }),
        queue_mutation_policy: QueueMutationPolicy::editable(),
        send_now: false,
        traceparent: None,
    };
    (item, rx)
}
#[cfg(test)]
pub(crate) fn user_item(id: &str, owner: &str) -> InputItem {
    user_item_with_rx(id, owner).0
}
#[cfg(test)]
pub(crate) fn input_with_origin_rx(
    prompt_id: &str,
    origin: crate::session::PromptOrigin,
) -> (InputItem, oneshot::Receiver<PromptTurnResult>) {
    let (respond_to, rx) = oneshot::channel();
    let verbatim = origin.is_synthetic();
    let input_origin = InputOrigin::new(origin);
    let item = InputItem {
        prompt_id: prompt_id.to_string(),
        prompt_blocks: vec![],
        prompt_mode: PromptMode::Agent,
        trace_gcs_config: None,
        artifact_tracker: None,
        client_identifier: None,
        screen_mode: None,
        verbatim,
        json_schema: None,
        input_origin,
        task_wake_fallback: None,
        tool_overrides_update: None,
        respond_to,
        persist_ack: None,
        parsed_prompt_tx: None,
        initial_child_prompt_ready: None,
        queue_meta: None,
        queue_mutation_policy: QueueMutationPolicy::hidden(),
        send_now: false,
        traceparent: None,
    };
    (item, rx)
}
#[cfg(test)]
pub(crate) fn queue_input_request(
    prompt_blocks: Vec<acp::ContentBlock>,
    prompt_id: &str,
    respond_to: oneshot::Sender<PromptTurnResult>,
) -> QueueInputRequest {
    QueueInputRequest::from_legacy_prompt_id(
        prompt_blocks,
        prompt_id.to_string(),
        PromptMode::Agent,
        respond_to,
    )
}
#[cfg(test)]
pub(crate) fn running_task_stub(prompt_id: &str) -> AgentTask {
    AgentTask::new(
        prompt_id,
        tokio::task::spawn_local(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        })
        .abort_handle(),
    )
}
#[cfg(test)]
pub(crate) async fn build_actor() -> (
    std::sync::Arc<SessionActor>,
    tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    let (persistence_tx, mut persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    tokio::spawn(async move {
        while let Some(message) = persistence_rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = message {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
    let actor =
        std::sync::Arc::new(create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await);
    (actor, gateway_rx)
}
#[cfg(test)]
pub(crate) fn test_image_content() -> acp::ImageContent {
    use base64::Engine as _;
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_pixel(32, 32, Rgba([128, 64, 32, 255]));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    acp::ImageContent::new(
        base64::engine::general_purpose::STANDARD.encode(&buf),
        "image/png".to_string(),
    )
}
#[cfg(test)]
pub(crate) fn set_goal_harness_for_tests(actor: &SessionActor) {
    actor
        .goal_harness_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(test)]
pub(crate) async fn drain_gateway_turns() {
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
}
#[cfg(test)]
pub(crate) async fn prepare_call(
    actor: &SessionActor,
    call: ToolCallResponse,
) -> Result<PreparedToolCall, ToolLoop> {
    let mut deferred = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        actor.prepare_tool_call(call, &mut deferred),
    )
    .await
    .expect("prepare_tool_call must not hang")
    .expect("prepare_tool_call must not error")
}
#[cfg(test)]
pub(crate) fn install_permission_manager(
    actor: &mut SessionActor,
    yolo: bool,
    gateway: distill_acp_lib::AcpAgentGatewaySender,
) {
    use distill_paths::AbsPathBuf;
    use distill_workspace::permission::{ClientType, spawn_permission_manager};
    let cwd = AbsPathBuf::new(std::path::PathBuf::from(actor.session_info.cwd.clone()))
        .unwrap_or_else(|_| AbsPathBuf::new(std::path::PathBuf::from("/tmp")).unwrap());
    let (handle, _ev) = spawn_permission_manager(
        actor.session_info.id.clone(),
        gateway,
        cwd,
        ClientType::Generic,
        None,
        vec![],
        vec![],
        yolo,
        None,
    );
    actor.permissions = handle;
}
#[cfg(test)]
pub(crate) fn read_file_call(id: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(
            "read_file",
            serde_json::json!({ "target_file": "/tmp/permission-hook.txt" }).to_string(),
        ),
    }
}
#[cfg(test)]
pub(crate) fn search_replace_call(id: &str) -> ToolCallResponse {
    search_replace_call_at(id, "/tmp/permission-hook.txt")
}
#[cfg(test)]
pub(crate) fn search_replace_call_at(id: &str, path: &str) -> ToolCallResponse {
    ToolCallResponse {
        id: id.to_string(),
        kind: "function".to_string(),
        function: crate::sampling::types::ToolCallFunction::new(
            "search_replace",
            serde_json::json!({
                "file_path": path,
                "old_string": "a",
                "new_string": "b",
            })
            .to_string(),
        ),
    }
}
#[cfg(test)]
pub(crate) fn read_and_edit_toolset() -> Vec<distill_tools::registry::types::ToolConfig> {
    use distill_tools::registry::types::ToolConfig;
    vec![
        ToolConfig::from_id("Distill:read_file"),
        ToolConfig {
            id: "Distill:search_replace".into(),
            params: Some(
                serde_json::from_value(serde_json::json!({
                    "skip_read_before_edit": true
                }))
                .unwrap(),
            ),
            name_override: None,
            params_name_overrides: None,
            description_override: None,
            behavior_version: None,
            kind: None,
        },
    ]
}
#[cfg(test)]
pub(crate) fn pre_tool_use_spec(
    name: &str,
    matcher: Option<&str>,
    script: &str,
) -> distill_hooks::config::HookSpec {
    distill_hooks::config::HookSpec {
        name: name.into(),
        event: distill_hooks::event::HookEventName::PreToolUse,
        handler_type: distill_hooks::config::HandlerType::Command,
        configured_matcher: matcher.map(str::to_string),
        matcher: matcher.map(|m| distill_hooks::matcher::HookMatcher::new(m).unwrap()),
        enabled: true,
        command: Some(std::path::PathBuf::from(script)),
        command_raw: Some(script.to_string()),
        url: None,
        url_raw: None,
        timeout_ms: 5000,
        source_dir: std::path::PathBuf::from("/tmp"),
        extra_env: std::collections::HashMap::new(),
        layer: distill_hooks::config::HookProvenance::File,
    }
}
#[cfg(test)]
pub(crate) fn post_tool_use_spec(
    name: &str,
    matcher: Option<&str>,
    script: &str,
) -> distill_hooks::config::HookSpec {
    distill_hooks::config::HookSpec {
        event: distill_hooks::event::HookEventName::PostToolUse,
        ..pre_tool_use_spec(name, matcher, script)
    }
}
#[cfg(test)]
pub(crate) fn post_tool_use_failure_spec(
    name: &str,
    matcher: Option<&str>,
    script: &str,
) -> distill_hooks::config::HookSpec {
    distill_hooks::config::HookSpec {
        event: distill_hooks::event::HookEventName::PostToolUseFailure,
        ..pre_tool_use_spec(name, matcher, script)
    }
}
#[cfg(test)]
pub(crate) fn install_pre_tool_use_hooks(
    actor: &mut SessionActor,
    specs: Vec<distill_hooks::config::HookSpec>,
) {
    let (mut registry, _) = distill_hooks::discovery::load_hooks(None, None);
    registry.append_specs(specs);
    actor.hook_resolved_workspace_root = "/tmp".to_string();
    *actor.hook_registry.borrow_mut() = Some(Arc::new(registry));
}
#[cfg(test)]
pub(crate) fn activate_plan_mode(actor: &SessionActor) {
    let mut tracker = actor.plan_mode.lock();
    assert!(tracker.enter_pending());
    assert!(tracker.activate());
}
#[cfg(test)]
pub(crate) async fn tool_result_text(actor: &SessionActor, call_id: &str) -> String {
    let conversation = actor.chat_state_handle.get_conversation().await;
    conversation
        .iter()
        .rev()
        .find_map(|item| match item {
            distill_sampling_types::ConversationItem::ToolResult(result)
                if result.tool_call_id == call_id =>
            {
                Some(result.content.to_string())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool_result for {call_id} in {conversation:?}"))
}
#[cfg(test)]
pub(crate) fn spawn_gateway_loop(
    gateway_rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    spawn_gateway_loop_counting_prompt_hooks(
        gateway_rx,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        false,
    )
}
#[cfg(test)]
pub(crate) fn spawn_gateway_loop_counting_prompt_hooks(
    gateway_rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
    permission_prompt_hooks: Arc<std::sync::atomic::AtomicUsize>,
    park_until_hook: bool,
) -> Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    use std::sync::atomic::Ordering;
    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let captured = updates.clone();
    let mut gateway_rx = gateway_rx;
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::RequestPermission(args) => {
                    let hooks = permission_prompt_hooks.clone();
                    tokio::task::spawn_local(async move {
                        if park_until_hook {
                            let start = std::time::Instant::now();
                            while hooks.load(Ordering::SeqCst) == 0 {
                                assert!(
                                    start.elapsed() < std::time::Duration::from_secs(3),
                                    "permission_prompt hook must fire before the user answers"
                                );
                                tokio::task::yield_now().await;
                            }
                        }
                        let _ = args
                            .response_tx
                            .send(Ok(acp::RequestPermissionResponse::new(
                                acp::RequestPermissionOutcome::Selected(
                                    acp::SelectedPermissionOutcome::new(
                                        acp::PermissionOptionId::new("allow-once"),
                                    ),
                                ),
                            )));
                    });
                }
                distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                    let params: serde_json::Value =
                        serde_json::from_str(args.request.params.get()).unwrap_or_default();
                    match args.request.method.as_ref() {
                        "x.ai/hooks/event" => {
                            if params.get("notificationType")
                                == Some(&serde_json::json!("permission_prompt"))
                            {
                                permission_prompt_hooks.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        "x.ai/session_notification" => {
                            captured.lock().unwrap().push(
                                params
                                    .get("update")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            );
                        }
                        _ => {}
                    }
                }
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    let _ = args.response_tx.send(Ok(()));
                }
                _ => {}
            }
        }
    });
    updates
}
/// Ack every gateway `SessionNotification` so a driven turn never blocks on the client.
/// Spawned on the current `LocalSet`.
pub(crate) fn drain_gateway(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let distill_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
}
/// Answer every `FlushAndAck` persistence barrier with `Ok` so a driven turn's `persist_ack` resolves.
/// Spawned on the current `LocalSet`.
pub(crate) fn drain_persistence(mut rx: tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>) {
    tokio::task::spawn_local(async move {
        while let Some(msg) = rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
}
/// An actor whose latched disk-full probe refuses every turn before its prompt commits.
pub(crate) async fn disk_full_actor() -> SessionActor {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    drain_gateway(gateway_rx);
    let (persistence_tx, mut persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    tokio::task::spawn_local(async move {
        while let Some(msg) = persistence_rx.recv().await {
            if let PersistenceMsg::ProbeWritable { respond_to } = msg {
                let _ = respond_to.send(Err(std::io::Error::from(std::io::ErrorKind::StorageFull)));
            }
        }
    });
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.notifications.disk_full = tokio::sync::watch::channel(true).1;
    actor
}
pub(crate) fn subagent_summary(id: &str) -> SubagentCompletionSummary {
    SubagentCompletionSummary {
        snapshot: SubagentSnapshot {
            subagent_id: id.into(),
            description: format!("desc {id}"),
            subagent_type: "general-purpose".into(),
            status: SubagentSnapshotStatus::Completed {
                output: String::new(),
                tool_calls: 3,
                turns: 1,
                worktree_path: None,
            },
            started_at_epoch_ms: 0,
            duration_ms: 1000,
            persona: None,
        },
        loop_task_id: None,
        tool_calls: 3,
        output: std::sync::Arc::from("done"),
        full_output_bytes: "done".len(),
    }
}
pub(crate) struct FakeCoordinator {
    suppress_seen: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    returned: Arc<std::sync::Mutex<Vec<String>>>,
    peeks: Arc<std::sync::atomic::AtomicUsize>,
}
impl FakeCoordinator {
    pub(crate) fn suppress_seen(&self) -> Vec<Vec<String>> {
        self.suppress_seen.lock().unwrap().clone()
    }
    pub(crate) fn returned(&self) -> Vec<String> {
        self.returned.lock().unwrap().clone()
    }
    pub(crate) fn peeks(&self) -> usize {
        self.peeks.load(std::sync::atomic::Ordering::Relaxed)
    }
}
/// Fakes the coordinator's `Completions` (destructive, honours `suppress_ids`) and `PeekCompletions` (clones) arms.
/// Spawns on the current `LocalSet`; call before the actor is shared.
pub(crate) fn spawn_fake_coordinator(
    actor: &mut SessionActor,
    buffer: Vec<SubagentCompletionSummary>,
) -> FakeCoordinator {
    use distill_tools::implementations::distill::task::types::SubagentEvent;
    let fake = FakeCoordinator {
        suppress_seen: Arc::default(),
        returned: Arc::default(),
        peeks: Arc::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    actor.tool_context.subagent_event_tx = Some(tx);
    let suppress_seen = Arc::clone(&fake.suppress_seen);
    let returned = Arc::clone(&fake.returned);
    let peeks = Arc::clone(&fake.peeks);
    tokio::task::spawn_local(async move {
        let mut buffer = buffer;
        while let Some(ev) = rx.recv().await {
            match ev {
                SubagentEvent::Completions(req) => {
                    suppress_seen.lock().unwrap().push(req.suppress_ids.clone());
                    let (drained, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut buffer)
                        .into_iter()
                        .partition(|c| !req.suppress_ids.iter().any(|id| id == c.subagent_id()));
                    buffer = kept;
                    returned
                        .lock()
                        .unwrap()
                        .extend(drained.iter().map(|c| c.subagent_id().to_owned()));
                    let _ = req.respond_to.send(drained);
                }
                SubagentEvent::PeekCompletions(req) => {
                    peeks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = req.respond_to.send(buffer.clone());
                }
                _ => {}
            }
        }
    });
    fake
}
/// An actor whose persistence channel answers the `FlushAndAck` barrier, so a turn driven with a `persist_ack` resolves.
/// Bare `build_actor` never acks.
#[cfg(test)]
#[allow(clippy::type_complexity)]
pub(crate) fn spawn_capturing_gateway_loop(
    gateway_rx: tokio::sync::mpsc::UnboundedReceiver<distill_acp_lib::AcpClientMessage>,
) -> (
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) {
    let acp_updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let xai_updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> = Arc::default();
    let acp_captured = acp_updates.clone();
    let xai_captured = xai_updates.clone();
    let mut gateway_rx = gateway_rx;
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            match msg {
                distill_acp_lib::AcpClientMessage::SessionNotification(args) => {
                    if let Ok(v) = serde_json::to_value(&args.request) {
                        acp_captured.lock().unwrap().push(v);
                    }
                    let _ = args.response_tx.send(Ok(()));
                }
                distill_acp_lib::AcpClientMessage::ExtNotification(args) => {
                    if args.request.method.as_ref() == "x.ai/session_notification" {
                        let params: serde_json::Value =
                            serde_json::from_str(args.request.params.get()).unwrap_or_default();
                        xai_captured.lock().unwrap().push(
                            params
                                .get("update")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        );
                    }
                }
                _ => {}
            }
        }
    });
    (acp_updates, xai_updates)
}
#[cfg(test)]
pub(crate) async fn actor_with_persistence_drain() -> std::sync::Arc<SessionActor> {
    actor_with_persistence_drain_and_sampler(distill_sampler::SamplerHandle::noop()).await
}
#[cfg(test)]
pub(crate) async fn actor_with_persistence_drain_and_sampler(
    sampler: distill_sampler::SamplerHandle,
) -> std::sync::Arc<SessionActor> {
    let (gateway_tx, mut gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
    tokio::task::spawn_local(async move { while gateway_rx.recv().await.is_some() {} });
    let (persistence_tx, mut persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    tokio::task::spawn_local(async move {
        while let Some(msg) = persistence_rx.recv().await {
            if let PersistenceMsg::FlushAndAck { respond_to } = msg {
                let _ = respond_to.send(Ok(()));
            }
        }
    });
    let (mut actor, _) = create_test_actor_with_terminal(
        0,
        256_000,
        85,
        gateway_tx,
        persistence_tx,
        Arc::new(DummyTerminal),
    )
    .await;
    actor.sampler_handle = sampler;
    std::sync::Arc::new(actor)
}
/// Fresh per-step transient-retry state for direct `handle_sampling_failure` calls: `step_attempts` used, full turn budget, no open episode.
pub(crate) fn transient_state(step_attempts: u32, enabled: bool) -> TransientRetryState {
    TransientRetryState {
        step_attempts,
        prompt_attempts: 0,
        episode_start: None,
        enabled,
    }
}
pub(crate) fn ms(n: u64) -> std::time::Duration {
    std::time::Duration::from_millis(n)
}
pub(crate) async fn plain_actor() -> SessionActor {
    let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
    create_test_actor(100, 256_000, 80, gw_tx, persist_tx).await
}
pub(crate) async fn actor_with_mcp(
    configs: Vec<acp::McpServer>,
    initialized: bool,
    initializing: Vec<String>,
) -> SessionActor {
    let a = plain_actor().await;
    {
        let mut st = a.mcp_state.lock().await;
        st.configs = configs;
        st.cancel_any_init();
        if initialized || !initializing.is_empty() {
            std::mem::forget(st.try_start_init().expect("fixture claims init"));
            let handshakes_pending = !initializing.is_empty();
            st.mark_servers_initializing(initializing);
            if initialized {
                st.finish_init();
                if !handshakes_pending {
                    st.complete_init();
                }
            }
        }
    }
    a
}
pub(crate) fn stdio(name: &str, cmd: &str) -> acp::McpServer {
    let args = if cmd == "sleep" {
        vec!["300".to_string()]
    } else {
        vec![]
    };
    acp::McpServer::Stdio(
        acp::McpServerStdio::new(name.to_string(), cmd)
            .args(args)
            .env(vec![]),
    )
}
pub(crate) async fn register_stub(bridge: &crate::tools::bridge::ToolBridge, name: &'static str) {
    bridge
        .register_mcp_tools(
            name.to_string(),
            StubMcpTool(name),
            Some(serde_json::json!({"type": "object"})),
        )
        .expect("stub registration");
}
#[derive(Debug, Clone)]
pub(crate) struct StubMcpTool(pub(crate) &'static str);
impl distill_tools::types::tool_metadata::ToolMetadata for StubMcpTool {
    fn kind(&self) -> distill_tools::types::tool::ToolKind {
        distill_tools::types::tool::ToolKind::Other
    }
    fn tool_namespace(&self) -> distill_tools::types::tool::ToolNamespace {
        distill_tools::types::tool::ToolNamespace::MCP
    }
    fn description_template(&self) -> &str {
        "stub MCP tool"
    }
}
impl distill_tool_runtime::Tool for StubMcpTool {
    type Args = serde_json::Value;
    type Output = distill_tools::types::output::ToolOutput;
    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(self.0).expect("valid tool id")
    }
    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(self.0, "stub MCP tool")
    }
    async fn run(
        &self,
        _ctx: distill_tool_runtime::ToolCallContext,
        _args: serde_json::Value,
    ) -> Result<Self::Output, distill_tool_runtime::ToolError> {
        Ok(distill_tools::types::output::ToolOutput::MCP(
            distill_tools::types::output::MCPOutput::errored(
                self.0.into(),
                "stub".into(),
                "unused".into(),
            ),
        ))
    }
}
/// The returned set plays the run loop: startup tasks live while it does.
pub(crate) fn with_run_loop(mut actor: SessionActor) -> (Arc<SessionActor>, StartupTaskSet) {
    let startup_tasks = StartupTaskSet::install(&actor);
    let actor = Arc::new_cyclic(|weak: &std::sync::Weak<SessionActor>| {
        actor.weak_self = weak.clone();
        actor
    });
    (actor, startup_tasks)
}
