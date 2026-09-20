// Modified for Distill by Samuel Fajreldines, 2026.
use super::super::support::*;
fn at<T>(xs: &[T], i: usize) -> &T {
    let Some(x) = xs.get(i) else {
        panic!("expected index {i}, len {}", xs.len());
    };
    x
}
use super::super::*;
use super::{AutoCompactTriggerInfo, SuppressReason};
use crate::session::acp_session::McpReminderMode;
use crate::terminal::AsyncTerminalRunner;
use crate::terminal::runner::{TerminalError, TerminalRunRequest, TerminalRunResult};
use distill_paths::AbsPathBuf;
use distill_workspace::file_system::MockFs;
use distill_workspace::permission::PermissionHandle;
use std::sync::OnceLock;
use std::sync::atomic::Ordering::Relaxed;
use tokio::sync::mpsc;
#[derive(Debug)]
struct DummyTerminal;
#[async_trait::async_trait]
impl AsyncTerminalRunner for DummyTerminal {
    async fn run(&self, _request: TerminalRunRequest) -> Result<TerminalRunResult, TerminalError> {
        Err(TerminalError::Other("dummy terminal".into()))
    }
}
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
    let (chat_event_tx, _chat_event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (event_tx, _event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::session::replay_events::SessionEvent>();
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
        chat_event_tx,
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
        unattributed_background_usage: std::sync::atomic::AtomicBool::new(false),
        session_info: SessionInfo {
            id: acp::SessionId::new("test-auto-compact"),
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
        permissions: PermissionHandle::allow_all(),
        tool_context,
        deny_read_globs: Vec::new(),
        mcp_state: Arc::new(TokioMutex::new(McpState::new(vec![]))),
        mcp_strategy: std::cell::Cell::new(McpInitStrategy::Blocking),
        delivery_tools: std::cell::RefCell::new(Vec::new()),
        attach_non_interactive: std::rc::Rc::new(std::cell::Cell::new(false)),
        chat_state_handle,
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
        inference_idle_timeout: std::time::Duration::from_secs(300),
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
        streaming_turn_capture: parking_lot::Mutex::new(
            crate::session::acp_session::StreamingTurnCapture::default(),
        ),
        stream_apply_span: parking_lot::Mutex::new(None),
        current_turn_span_id: parking_lot::Mutex::new(None),
        turn_stream_drained: parking_lot::Mutex::new(std::collections::HashMap::new()),
        pending_image_strip: parking_lot::Mutex::new(std::collections::HashMap::new()),
        image_strip_rewrite_barrier: ImageStripRewriteBarrier::new(),
        sampler_handle: distill_sampler::SamplerHandle::noop(),
        sampling_gate: None,
        rebuild_spec: crate::session::agent_rebuild::test_rebuild_spec_default(),
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
        image_describe_cache: Arc::new(crate::session::image_describe::ImageDescribeCache::new()),
        subagent_token_records: parking_lot::Mutex::new(std::collections::HashMap::new()),
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        trace_config_template: std::cell::RefCell::new(None),
    }
}
/// Suppression gates both AUTO paths; the reset scope depends on the reason.
/// `other` clears next turn, `credit_block` holds until a successful model call, `size` is sticky until a full reset (success/rewind/model switch).
#[tokio::test(flavor = "current_thread")]
async fn suppression_gates_and_reset_is_reason_scoped() {
    use crate::session::compaction_config::{SUPPRESS_NONE, SUPPRESS_TURN, SUPPRESS_UNTIL_SUCCESS};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let err = api_error_with_context_window(200_000);
            assert!(actor.check_auto_compact_needed().await.is_some());
            assert!(actor.should_compact_on_error(&err).await);
            actor
                .suppress_auto_compaction(SuppressReason::Other, "", 1_000, 200_000)
                .await;
            assert!(actor.check_auto_compact_needed().await.is_none());
            assert!(!actor.should_compact_on_error(&err).await);
            let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                SUPPRESS_TURN,
                SUPPRESS_NONE,
                Relaxed,
                Relaxed,
            );
            assert!(actor.check_auto_compact_needed().await.is_some());
            actor
                .suppress_auto_compaction(SuppressReason::CreditBlock, "", 1_000, 200_000)
                .await;
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_UNTIL_SUCCESS
            );
            assert!(actor.check_auto_compact_needed().await.is_none());
            assert!(!actor.should_compact_on_error(&err).await);
            let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                SUPPRESS_TURN,
                SUPPRESS_NONE,
                Relaxed,
                Relaxed,
            );
            assert!(
                actor.check_auto_compact_needed().await.is_none(),
                "credit-block suppression must survive the per-turn reset"
            );
            let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                SUPPRESS_UNTIL_SUCCESS,
                SUPPRESS_NONE,
                Relaxed,
                Relaxed,
            );
            assert!(actor.check_auto_compact_needed().await.is_some());
            actor
                .suppress_auto_compaction(SuppressReason::Size, "", 1_000, 200_000)
                .await;
            assert!(actor.check_auto_compact_needed().await.is_none());
            let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                SUPPRESS_TURN,
                SUPPRESS_NONE,
                Relaxed,
                Relaxed,
            );
            assert!(
                actor.check_auto_compact_needed().await.is_none(),
                "sticky suppression must survive the per-turn reset"
            );
            actor
                .compaction
                .auto_compact_suppressed
                .store(SUPPRESS_NONE, Relaxed);
            assert!(actor.check_auto_compact_needed().await.is_some());
        })
        .await;
}
/// The background two-pass prefire is an AUTO trigger: suppression must gate
/// it (else it silently re-sends the doomed request) and resets re-enable it.
#[tokio::test(flavor = "current_thread")]
async fn suppression_gates_prefire_two_pass() {
    use crate::session::compaction_config::{SUPPRESS_NONE, SUPPRESS_TURN};
    use std::sync::atomic::Ordering::Relaxed;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await;
            assert!(actor.should_prefire_two_pass().await);
            actor
                .suppress_auto_compaction(SuppressReason::Size, "", 1_000, 200_000)
                .await;
            assert!(
                !actor.should_prefire_two_pass().await,
                "suppressed prefire must not fire"
            );
            actor
                .compaction
                .auto_compact_suppressed
                .store(SUPPRESS_NONE, Relaxed);
            assert!(actor.should_prefire_two_pass().await);
            actor
                .suppress_auto_compaction(SuppressReason::Other, "", 1_000, 200_000)
                .await;
            assert!(!actor.should_prefire_two_pass().await);
            let _ = actor.compaction.auto_compact_suppressed.compare_exchange(
                SUPPRESS_TURN,
                SUPPRESS_NONE,
                Relaxed,
                Relaxed,
            );
            assert!(actor.should_prefire_two_pass().await);
        })
        .await;
}
/// A model switch clears suppression the switch (or the fresh budget-driven trigger) can resolve — sticky size/schema and a stale per-turn `other` — so the gates re-evaluate against the new window.
/// Account-state credit/auth is covered by `model_switch_keeps_account_state_suppression`.
#[tokio::test(flavor = "current_thread")]
async fn model_switch_clears_sticky_suppression() {
    use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_NONE};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await);
            for reason in [SuppressReason::Size, SuppressReason::Other] {
                actor
                    .suppress_auto_compaction(reason, "", 1_000, 200_000)
                    .await;
                assert_ne!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_NONE,
                    "{reason:?} should set suppression"
                );
                actor.compaction.previous_model.set(Some(PreviousModelInfo {
                    model_slug: "old-small-model".to_string(),
                    context_window: 100_000,
                }));
                actor
                    .maybe_compact_on_model_switch()
                    .await
                    .expect("non-auth model-switch path must not abort");
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    SUPPRESS_NONE,
                    "model switch must clear {reason:?} suppression so the gates re-evaluate"
                );
            }
        })
        .await;
}
/// Model switch must not clear credit/auth suppress or compact under it.
#[tokio::test(flavor = "current_thread")]
async fn model_switch_keeps_account_state_suppression() {
    use crate::session::compaction_config::{
        PreviousModelInfo, SUPPRESS_AUTH, SUPPRESS_UNTIL_SUCCESS,
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await);
            for (reason, expected) in [
                (SuppressReason::CreditBlock, SUPPRESS_UNTIL_SUCCESS),
                (SuppressReason::Auth, SUPPRESS_AUTH),
            ] {
                actor
                    .suppress_auto_compaction(reason, "", 1_000, 200_000)
                    .await;
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    expected,
                    "{reason:?} suppress state"
                );
                actor.compaction.previous_model.set(Some(PreviousModelInfo {
                    model_slug: "old-big-model".to_string(),
                    context_window: 400_000,
                }));
                actor
                    .maybe_compact_on_model_switch()
                    .await
                    .expect("suppressed model-switch path must not abort");
                assert_eq!(
                    actor.compaction.auto_compact_suppressed.load(Relaxed),
                    expected,
                    "model switch must NOT clear {reason:?} suppression"
                );
                actor
                    .compaction
                    .auto_compact_suppressed
                    .store(crate::session::compaction_config::SUPPRESS_NONE, Relaxed);
            }
        })
        .await;
}
/// Auth suppress clears on credential recovery, not on a model 200.
#[tokio::test(flavor = "current_thread")]
async fn auth_suppress_clears_on_credential_recovery() {
    use crate::session::compaction_config::{SUPPRESS_AUTH, SUPPRESS_NONE};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await;
            actor
                .suppress_auto_compaction(SuppressReason::Auth, "", 1_000, 200_000)
                .await;
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH
            );
            assert!(actor.check_auto_compact_needed().await.is_none());
            actor.clear_auth_compact_suppression();
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE
            );
            assert!(actor.check_auto_compact_needed().await.is_some());
        })
        .await;
}
/// Auth recovery must not clear credit suppress.
#[tokio::test(flavor = "current_thread")]
async fn clear_auth_suppress_leaves_credit_suppress() {
    use crate::session::compaction_config::SUPPRESS_UNTIL_SUCCESS;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await;
            actor
                .suppress_auto_compaction(SuppressReason::CreditBlock, "", 1_000, 200_000)
                .await;
            actor.clear_auth_compact_suppression();
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_UNTIL_SUCCESS,
                "credential recovery must not clear a credit-block suppress"
            );
        })
        .await;
}
/// After /login, clearing auth suppress must re-enable pre-sampling compact before the next sample.
/// This ordering broke when prepare_sampler ran after the gate.
#[tokio::test(flavor = "current_thread")]
async fn clear_auth_suppress_rearms_pre_sampling_compact_gate() {
    use crate::session::compaction_config::SUPPRESS_AUTH;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await;
            actor
                .suppress_auto_compaction(SuppressReason::Auth, "", 1_000, 200_000)
                .await;
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH
            );
            assert!(
                actor.check_auto_compact_needed().await.is_none(),
                "auth suppress must block pre-sampling compact"
            );
            actor.clear_auth_compact_suppression();
            assert!(
                actor.check_auto_compact_needed().await.is_some(),
                "after credential recovery, pre-sampling compact must re-arm"
            );
        })
        .await;
}
#[test]
fn is_auth_compact_error_classifies_401_messages() {
    let auth =
        acp::Error::internal_error().data("compact failed: API error (status 401 Unauthorized)");
    assert!(SessionActor::is_auth_compact_error(&auth));
    let credit = acp::Error::internal_error().data("compact failed: out of credits");
    assert!(!SessionActor::is_auth_compact_error(&credit));
    let size = acp::Error::internal_error()
        .data("compact failed: The prompt is too long for this model's context window.");
    assert!(!SessionActor::is_auth_compact_error(&size));
}
#[tokio::test(flavor = "current_thread")]
async fn surface_compact_auth_failure_emits_reauthable_retry_state() {
    use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
    use crate::session::storage::SessionUpdate;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(10_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let err = acp::Error::internal_error()
                .data("compact failed: API error (status 401 Unauthorized)");
            let out = actor.surface_compact_auth_failure(err).await;
            assert_eq!(out.code, acp::Error::auth_required().code);
            let mut saw_retry_auth = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(SessionUpdate::Xai(notif)) = msg
                    && let XaiSessionUpdate::RetryState(
                        crate::extensions::notification::RetryState::Failed {
                            error_type,
                            message,
                        },
                    ) = &notif.update
                {
                    assert_eq!(error_type, "auth");
                    assert!(
                        message.contains("Unauthorized (401)") || message.contains("401"),
                        "message={message}"
                    );
                    saw_retry_auth = true;
                }
            }
            assert!(
                saw_retry_auth,
                "expected RetryState::Failed auth notification"
            );
        })
        .await;
}
/// The suppression notification text is tailored to the failure reason; the unclassified `Other` bucket carries the normalized real error.
#[test]
fn suppression_notification_message_is_reason_specific() {
    let msg = SessionActor::suppress_notification_message;
    let detail = "compact failed: API error (status 500 Internal Server Error)";
    assert_eq!(
        msg(SuppressReason::CreditBlock, detail),
        "out of credits or over your spending limit. Add credits and retry."
    );
    assert_eq!(
        msg(SuppressReason::Auth, detail),
        "authentication problem — re-authenticate using /login and retry."
    );
    assert_eq!(
        msg(SuppressReason::Size, detail),
        "this conversation is too large to compact."
    );
    assert_eq!(
        msg(SuppressReason::Schema, detail),
        "this conversation can't be summarized."
    );
    assert_eq!(
        msg(SuppressReason::Other, detail),
        "it'll retry on the next turn, or start a new session using /new.\n\
         API error (status 500 Internal Server Error)"
    );
    assert_eq!(
        msg(SuppressReason::Other, ""),
        "it'll retry on the next turn, or start a new session using /new."
    );
    assert_eq!(
        msg(SuppressReason::Other, " \n\t "),
        "it'll retry on the next turn, or start a new session using /new."
    );
    let long_detail = format!("compact failed: {}", "x".repeat(600));
    let truncated = msg(SuppressReason::Other, &long_detail);
    let (headline, detail_line) = truncated.split_once('\n').expect("two-line composition");
    assert_eq!(
        headline,
        "it'll retry on the next turn, or start a new session using /new."
    );
    assert!(
        detail_line.starts_with('x'),
        "detail line starts with the capped detail (no indent): {detail_line}"
    );
    assert!(
        !detail_line.contains(&"x".repeat(400)),
        "detail must be truncated: {} chars",
        detail_line.len()
    );
    assert!(
        detail_line.ends_with('…'),
        "truncation marker: {detail_line}"
    );
}
/// The suppress transition emits one `AutoCompactFailed` carrying exactly the composed, scrubbed message.
#[tokio::test(flavor = "current_thread")]
async fn suppression_emits_composed_notification() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor = create_test_actor(10_000, 200_000, 85, gateway_tx, persistence_tx).await;
            let Some(&(service, replacement)) =
                crate::sampling::error::SERVICE_NAME_REWRITES.first()
            else {
                panic!("SERVICE_NAME_REWRITES must be non-empty");
            };
            let detail = format!("compact failed: {service}: upstream timeout");
            actor
                .suppress_auto_compaction(SuppressReason::Other, &detail, 1_000, 200_000)
                .await;
            let mut text = None;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) =
                    msg
                    && let crate::extensions::notification::SessionUpdate::AutoCompactFailed {
                        error,
                    } = &notif.update
                {
                    text = Some(error.clone());
                }
            }
            let text = text.expect("expected an AutoCompactFailed notification");
            assert_eq!(
                text,
                SessionActor::suppress_notification_message(SuppressReason::Other, &detail)
            );
            assert!(
                !text.contains(service),
                "service names must never reach the notification: {text}"
            );
            assert!(
                text.contains(&format!("{replacement}: upstream timeout")),
                "scrubbed detail must survive: {text}"
            );
        })
        .await;
}
async fn spawn_deterministic_400_server() -> String {
    spawn_status_body_server(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"bad schema"}}"#,
    )
    .await
}
async fn spawn_deterministic_401_server() -> String {
    spawn_status_body_server(
        401,
        r#"{"error":{"type":"authentication_error","message":"Unauthorized (401)"}}"#,
    )
    .await
}
async fn spawn_transient_500_server() -> String {
    spawn_status_body_server(
        500,
        r#"{"error":{"type":"internal_error","message":"upstream exploded"}}"#,
    )
    .await
}
async fn spawn_status_body_server(status: u16, body: &'static str) -> String {
    spawn_capturing_status_body_server(status, body).await.0
}
/// Like [`spawn_status_body_server`] but also captures each request body (in
/// arrival order), for tests that assert how many attempts a flow made and
/// what each attempt sent.
async fn spawn_capturing_status_body_server(
    status: u16,
    body: &'static str,
) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let status_line = match status {
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        413 => "413 Payload Too Large",
        500 => "500 Internal Server Error",
        other => panic!("add status line for {other}"),
    };
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut raw = Vec::new();
                let mut buf = [0u8; 8192];
                let request_body = loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break String::new(),
                        Ok(n) => {
                            let Some(chunk) = buf.get(..n) else {
                                break String::new();
                            };
                            raw.extend_from_slice(chunk);
                        }
                    }
                    if let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let Some(header_bytes) = raw.get(..header_end) else {
                            break String::new();
                        };
                        let headers = String::from_utf8_lossy(header_bytes).to_ascii_lowercase();
                        let content_length: usize = headers
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let body_start = header_end + 4;
                        if let Some(body) = raw.get(body_start..body_start + content_length) {
                            break String::from_utf8_lossy(body).into_owned();
                        }
                    }
                };
                sink.lock().unwrap().push(request_body);
                let resp = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://{addr}"), captured)
}
fn switch_target_config(model: &str, base_url: String) -> distill_sampler::SamplerConfig {
    distill_sampler::SamplerConfig {
        api_key: Some("test-key".to_string()),
        base_url,
        model: model.to_string(),
        context_window: 256_000,
        api_backend: crate::sampling::ApiBackend::Responses,
        ..Default::default()
    }
}
/// A family switch compacts with the new model over the lossy view: the request must contain nothing but plain `{role, content}` text messages.
#[tokio::test(flavor = "current_thread")]
async fn family_switch_compacts_lossy_with_new_model() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(10_000, 200_000, 85, gateway_tx, persistence_tx).await);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
                ConversationItem::Reasoning(distill_sampling_types::rs::ReasoningItem {
                    id: "tco_res-uuid_call-uuid-0".to_string(),
                    summary: vec![],
                    content: None,
                    encrypted_content: Some("tco_SEALEDCIPHERTEXT".to_string()),
                    status: None,
                }),
                ConversationItem::assistant_tool_calls(vec![distill_sampling_types::ToolCall {
                    id: std::sync::Arc::<str>::from("call_xai_minted_id"),
                    name: "run_terminal_command".to_string(),
                    arguments: std::sync::Arc::<str>::from(r#"{"command":"ls"}"#),
                }]),
                ConversationItem::ToolResult(distill_sampling_types::ToolResultItem {
                    tool_call_id: "call_xai_minted_id".to_string(),
                    content: std::sync::Arc::<str>::from("file listing"),
                    images: Vec::new(),
                }),
                ConversationItem::assistant("done"),
            ]);
            let server = distill_test_support::MockInferenceServer::start()
                .await
                .expect("mock inference server");
            actor
                .handle_set_session_model(crate::session::SessionModelSwitch {
                    sampling_config: switch_target_config("new-model", server.url()),
                    canonical_model_id: None,
                    model_selection_intent: true,
                    use_concise: false,
                    is_family_switch: true,
                    apply_prompt_override: false,
                    skip_prompt_rewrite: true,
                    auto_compact_threshold_percent: 85,
                    system_prompt_label: distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_owned(),
                })
                .await
                .expect("compact failure is log-only; the switch must succeed");
            let requests = server.requests();
            assert!(
                !requests.is_empty(),
                "family switch must fire a compaction sample"
            );
            let body = at(&requests, 0).body.as_ref().unwrap();
            assert_eq!(
                j(body, "model"),
                "new-model",
                "summarizer must be the NEW model"
            );
            for message in j(body, "input").as_array().unwrap() {
                let keys: Vec<&String> = message.as_object().unwrap().keys().collect();
                assert!(
                    keys.iter()
                        .all(|k| *k == "type" || *k == "role" || *k == "content"),
                    "lossy view must send plain text messages, got keys {keys:?} in {message}"
                );
                assert_eq!(j(message, "type"), "message", "non-message item: {message}");
                assert!(
                    j(message, "content").is_string(),
                    "non-text content in {message}"
                );
            }
        })
        .await;
}
/// 401 auto-compact: SUPPRESS_AUTH and a reauthable RetryState (abort for /login).
#[tokio::test(flavor = "current_thread")]
async fn e2e_auto_compact_401_suppresses_auth_and_surfaces_reauth() {
    use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
    use crate::session::compaction_config::SUPPRESS_AUTH;
    use crate::session::storage::SessionUpdate;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let base_url = spawn_deterministic_401_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
                ConversationItem::assistant("hi"),
                ConversationItem::user("compact me"),
            ]);
            let err = actor
                .run_compact_only(
                    AutoCompactTriggerInfo {
                        tokens_used: 180_000,
                        context_window: 200_000,
                        percentage: 90,
                        reason_override: None,
                    },
                    false,
                )
                .await
                .expect_err("401 mock must fail auto-compact");
            assert!(
                SessionActor::is_auth_compact_error(&err),
                "401 compact failure must classify as auth: {err:?}"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH,
                "auth compact failure must use SUPPRESS_AUTH (cleared on re-login)"
            );
            let surfaced = actor.surface_compact_auth_failure(err).await;
            assert_eq!(surfaced.code, acp::Error::auth_required().code);
            let mut saw_retry_auth = false;
            let mut saw_auto_failed = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(SessionUpdate::Xai(notif)) = msg {
                    match &notif.update {
                        XaiSessionUpdate::RetryState(
                            crate::extensions::notification::RetryState::Failed {
                                error_type,
                                message,
                            },
                        ) => {
                            assert_eq!(error_type, "auth");
                            assert!(
                                message.contains("Unauthorized") || message.contains("401"),
                                "message={message}"
                            );
                            saw_retry_auth = true;
                        }
                        XaiSessionUpdate::AutoCompactFailed { error } => {
                            assert!(
                                error.contains("/login") || error.contains("authentication"),
                                "auto-failed={error}"
                            );
                            saw_auto_failed = true;
                        }
                        _ => {}
                    }
                }
            }
            assert!(saw_auto_failed, "expected AutoCompactFailed notification");
            assert!(
                saw_retry_auth,
                "expected RetryState::Failed auth so pager can stash + reauth"
            );
            actor.clear_auth_compact_suppression();
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                crate::session::compaction_config::SUPPRESS_NONE
            );
        })
        .await;
}
/// A 413 with a GENERIC body must walk the whole input ladder — verbatim →
/// verbatim_fitted → lossy, one request per stage — and only then suppress
/// as sticky `size`, with the "too large to compact" notification.
#[tokio::test(flavor = "current_thread")]
async fn e2e_auto_compact_413_steps_ladder_then_sticky_size_suppress() {
    use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
    use crate::session::compaction_config::SUPPRESS_STICKY;
    use crate::session::storage::SessionUpdate;
    use std::sync::atomic::Ordering::Relaxed;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(180_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let (base_url, requests) = spawn_capturing_status_body_server(
                413,
                r#"{"error":{"type":"request_error","message":"Request failed."}}"#,
            )
            .await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
                ConversationItem::assistant_tool_calls(vec![distill_sampling_types::ToolCall {
                    id: std::sync::Arc::<str>::from("call_1"),
                    name: "run_terminal_command".to_string(),
                    arguments: std::sync::Arc::<str>::from(r#"{"command":"ls"}"#),
                }]),
                ConversationItem::ToolResult(distill_sampling_types::ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    content: std::sync::Arc::<str>::from("file listing"),
                    images: Vec::new(),
                }),
                ConversationItem::assistant("hi"),
                ConversationItem::user("compact me"),
            ]);
            actor.chat_state_handle.record_token_usage(180_000);
            actor
                .run_compact_only(
                    AutoCompactTriggerInfo {
                        tokens_used: 180_000,
                        context_window: 200_000,
                        percentage: 90,
                        reason_override: None,
                    },
                    false,
                )
                .await
                .expect_err("413 mock must fail auto-compact");
            let bodies = requests.lock().unwrap().clone();
            assert_eq!(
                bodies.len(),
                3,
                "413 must step the input ladder exactly once per stage"
            );
            assert!(
                !at(&bodies, 0).is_empty(),
                "server must capture request bodies"
            );
            assert_ne!(
                at(&bodies, 2),
                at(&bodies, 0),
                "lossy stage must send a degraded input, not the verbatim payload"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_STICKY,
                "ladder exhaustion on 413 must suppress as sticky size, not per-turn other"
            );
            let mut saw_size_notification = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(SessionUpdate::Xai(notif)) = msg
                    && let XaiSessionUpdate::AutoCompactFailed { error } = &notif.update
                {
                    assert!(
                        error.contains("too large to compact"),
                        "413 exhaustion must surface the size notification, got: {error}"
                    );
                    saw_size_notification = true;
                }
            }
            assert!(
                saw_size_notification,
                "expected the size AutoCompactFailed notification"
            );
        })
        .await;
}
/// Model-switch compact 401 must surface reauth (same path as pre-sampling).
#[tokio::test(flavor = "current_thread")]
async fn e2e_model_switch_compact_401_surfaces_reauth() {
    use crate::extensions::notification::SessionUpdate as XaiSessionUpdate;
    use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_AUTH};
    use crate::session::storage::SessionUpdate;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let base_url = spawn_deterministic_401_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
                ConversationItem::assistant("hi"),
                ConversationItem::user("compact me"),
            ]);
            actor.chat_state_handle.record_token_usage(214_000);
            actor.compaction.previous_model.set(Some(PreviousModelInfo {
                model_slug: "old-big-model".to_string(),
                context_window: 400_000,
            }));
            let err = actor
                .maybe_compact_on_model_switch()
                .await
                .expect_err("model-switch 401 compact must abort for reauth");
            assert_eq!(err.code, acp::Error::auth_required().code);
            assert!(
                SessionActor::is_auth_compact_error(&err)
                    || err.message.to_ascii_lowercase().contains("unauthorized")
                    || format!("{err:?}").contains("401"),
                "surfaced error should be reauthable auth: {err:?}"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH,
                "auth compact failure must use SUPPRESS_AUTH"
            );
            let mut saw_retry_auth = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(SessionUpdate::Xai(notif)) = msg
                    && let XaiSessionUpdate::RetryState(
                        crate::extensions::notification::RetryState::Failed {
                            error_type,
                            message,
                        },
                    ) = &notif.update
                {
                    assert_eq!(error_type, "auth");
                    assert!(
                        message.contains("Unauthorized") || message.contains("401"),
                        "message={message}"
                    );
                    saw_retry_auth = true;
                }
            }
            assert!(
                saw_retry_auth,
                "expected RetryState::Failed auth so pager can stash + reauth"
            );
        })
        .await;
}
/// Non-auth model-switch compact failures stay log-only (turn continues).
#[tokio::test(flavor = "current_thread")]
async fn e2e_model_switch_compact_non_auth_failure_does_not_abort() {
    use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_NONE};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let base_url = spawn_deterministic_400_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
            ]);
            actor.chat_state_handle.record_token_usage(214_000);
            actor.compaction.previous_model.set(Some(PreviousModelInfo {
                model_slug: "old-big-model".to_string(),
                context_window: 400_000,
            }));
            actor
                .maybe_compact_on_model_switch()
                .await
                .expect("non-auth model-switch compact failure must not abort the turn");
            assert_ne!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "schema/other compact failure must suppress after attempt"
            );
        })
        .await;
}
/// After clearing auth suppress, a switch to a smaller window can re-evaluate and compact.
#[tokio::test(flavor = "current_thread")]
async fn clear_auth_suppress_allows_model_switch_compact_reeval() {
    use crate::session::compaction_config::{PreviousModelInfo, SUPPRESS_AUTH, SUPPRESS_NONE};
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(214_000, 200_000, 85, gateway_tx, persistence_tx).await);
            actor
                .suppress_auto_compaction(SuppressReason::Auth, "", 1_000, 200_000)
                .await;
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH
            );
            actor.compaction.previous_model.set(Some(PreviousModelInfo {
                model_slug: "old-big-model".to_string(),
                context_window: 400_000,
            }));
            actor
                .maybe_compact_on_model_switch()
                .await
                .expect("suppressed switch must not abort");
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_AUTH
            );
            actor.clear_auth_compact_suppression();
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE
            );
            actor.compaction.previous_model.set(Some(PreviousModelInfo {
                model_slug: "old-big-model".to_string(),
                context_window: 400_000,
            }));
            let base_url = spawn_deterministic_400_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
            ]);
            actor.chat_state_handle.record_token_usage(214_000);
            actor
                .maybe_compact_on_model_switch()
                .await
                .expect("post-clear switch compact re-eval must not abort on non-auth");
            assert_ne!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "post-clear switch must re-evaluate and attempt compact"
            );
        })
        .await;
}
/// A deterministic failure suppresses auto-compaction only on the AUTO path, never for a bare manual `/compact`.
#[tokio::test(flavor = "current_thread")]
async fn bare_manual_compact_failure_does_not_suppress_auto() {
    use crate::session::compaction_config::SUPPRESS_NONE;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let base_url = spawn_deterministic_400_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
            ]);
            let result = actor.run_compact(None).await;
            let err = result.expect_err("mock 400 must fail the compaction");
            assert_eq!(
                crate::session::helpers::session_compact::compact_error_kind(&err),
                Some(crate::session::helpers::session_compact::CompactErrorKind::Failed),
                "manual failures must carry the typed failure kind"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "manual /compact (even without args) must never set auto-compact suppression"
            );
            let result = actor
                .run_compact_only(
                    AutoCompactTriggerInfo {
                        tokens_used: 180_000,
                        context_window: 200_000,
                        percentage: 90,
                        reason_override: None,
                    },
                    false,
                )
                .await;
            assert!(result.is_err(), "mock 400 must fail the compaction");
            assert_ne!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "the same deterministic failure on the AUTO path must suppress"
            );
        })
        .await;
}
/// A transient failure (500, retries exhausted) on the AUTO path notifies with guidance and the normalized error.
/// The test takes ~6s: real retry delays run.
#[tokio::test(flavor = "current_thread")]
async fn transient_auto_compact_failure_notifies_with_real_error() {
    use crate::session::compaction_config::SUPPRESS_NONE;
    use std::sync::atomic::Ordering::Relaxed;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let base_url = spawn_transient_500_server().await;
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = base_url;
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hello"),
            ]);
            let result = actor
                .run_compact_only(
                    AutoCompactTriggerInfo {
                        tokens_used: 180_000,
                        context_window: 200_000,
                        percentage: 90,
                        reason_override: None,
                    },
                    false,
                )
                .await;
            assert!(result.is_err(), "mock 500 must fail the compaction");
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "a transient failure must not suppress auto-compaction"
            );
            let mut error_text = None;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) =
                    msg
                    && let crate::extensions::notification::SessionUpdate::AutoCompactFailed {
                        error,
                    } = &notif.update
                {
                    error_text = Some(error.clone());
                }
            }
            let error_text = error_text.expect("transient failure must emit AutoCompactFailed");
            let (headline, detail_line) = error_text
                .split_once('\n')
                .expect("guidance + detail composition");
            assert_eq!(
                headline,
                "it'll retry on the next turn, or start a new session using /new."
            );
            assert!(
                detail_line.contains("500"),
                "must surface the upstream status: {detail_line}"
            );
            assert!(
                !detail_line
                    .to_ascii_lowercase()
                    .starts_with("compact failed:"),
                "internal prefix must be stripped: {detail_line}"
            );
        })
        .await;
}
/// A successful compaction lets failed-server announcements fire again.
/// The failure reminder was dropped with the compacted context (unlike connected servers, which the compaction context carries).
/// So the announced episodes clear and the MCP reminder goes dirty for a re-announcement at the next injection.
#[tokio::test(flavor = "current_thread")]
async fn compaction_rearms_failed_server_announcements() {
    use distill_test_support::MockInferenceServer;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let actor =
                Arc::new(create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await);
            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("Summary of prior work. ".repeat(30));
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            actor.chat_state_handle.update_sampling_config(cfg);
            let filler = "x".repeat(8_000);
            actor.chat_state_handle.replace_conversation(vec![
                ConversationItem::system("sys"),
                ConversationItem::user(format!("u0 {filler}")),
                ConversationItem::assistant(format!("a0 {filler}")),
                ConversationItem::user("final query"),
            ]);
            actor
                .mcp_announcements
                .lock()
                .failed
                .insert("dead".to_string(), Default::default());
            actor
                .mcp_reminder_dirty
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let result = actor.run_compact(None).await;
            assert!(result.is_ok(), "compaction should succeed: {result:?}");
            assert!(
                actor.mcp_announcements.lock().failed.is_empty(),
                "compaction must re-arm failed-server announcements"
            );
            assert!(
                actor
                    .mcp_reminder_dirty
                    .load(std::sync::atomic::Ordering::Relaxed),
                "compaction must mark the MCP reminder dirty"
            );
        })
        .await;
}
/// A forked session whose whole-transcript inherited prefix alone exceeds the auto-compact threshold releases the prefix on compaction.
/// That lets the conversation actually shrink below the threshold.
/// The release stays sticky across further compactions (no unbounded compaction loop).
#[tokio::test(flavor = "current_thread")]
async fn forked_prefix_released_under_pressure_and_stays_released() {
    use crate::session::compaction_config::SUPPRESS_NONE;
    use distill_test_support::MockInferenceServer;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel();
            let filler = "x".repeat(8_000);
            let mut conv = vec![ConversationItem::system("small system prompt")];
            for i in 0..9 {
                conv.push(ConversationItem::user(format!("u{i} {filler}")));
                conv.push(ConversationItem::assistant(format!("a{i} {filler}")));
            }
            conv.push(ConversationItem::user("final query"));
            let prefix_len = conv.len();
            let mut actor = create_test_actor(0, 40_000, 80, gateway_tx, persistence_tx).await;
            actor.startup_hints.inherited_prefix_len = Some(prefix_len);
            let actor = Arc::new(actor);
            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("Summary of prior work. ".repeat(30));
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(conv);
            let threshold_tokens = 40_000u64 * 80 / 100;
            let before = actor.chat_state_handle.get_total_tokens().await;
            assert!(
                before > threshold_tokens,
                "seed must exceed threshold: {before} <= {threshold_tokens}"
            );
            let result = actor.run_compact(None).await;
            assert!(result.is_ok(), "compaction should succeed: {result:?}");
            assert!(
                actor.compaction.prefix_released.load(Relaxed),
                "prefix must be released under pressure"
            );
            let after = actor.chat_state_handle.get_total_tokens().await;
            assert!(
                after < threshold_tokens,
                "released history must drop below threshold: {after} >= {threshold_tokens}"
            );
            assert!(
                actor.chat_state_handle.get_conversation_len().await < prefix_len,
                "conversation must shrink below the pinned prefix floor"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_NONE,
                "a shrunk conversation must not suppress AUTO"
            );
            let result = actor.run_compact(None).await;
            assert!(
                result.is_ok(),
                "second compaction should succeed: {result:?}"
            );
            assert!(
                actor.compaction.prefix_released.load(Relaxed),
                "release must stay sticky across compactions"
            );
            let after2 = actor.chat_state_handle.get_total_tokens().await;
            assert!(
                after2 < threshold_tokens,
                "sticky release must keep the session under threshold: {after2}"
            );
        })
        .await;
}
/// The pathological case: even the released (summarized) history exceeds the threshold because the system prompt alone is over budget.
/// A forked session then sets sticky suppression instead of clearing it, WITHOUT a user-facing failure event.
/// So AUTO is not immediately re-enabled while the compaction itself still reports success.
#[tokio::test(flavor = "current_thread")]
async fn forked_release_still_over_threshold_suppresses_auto() {
    use crate::session::compaction_config::SUPPRESS_STICKY;
    use distill_test_support::MockInferenceServer;
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
            let (persistence_tx, mut persistence_rx) = mpsc::unbounded_channel();
            let huge_system = "s".repeat(150_000);
            let conv = vec![
                ConversationItem::system(huge_system),
                ConversationItem::user("q"),
                ConversationItem::assistant("a"),
                ConversationItem::user("final query"),
            ];
            let prefix_len = conv.len();
            let mut actor = create_test_actor(0, 40_000, 80, gateway_tx, persistence_tx).await;
            actor.startup_hints.inherited_prefix_len = Some(prefix_len);
            let actor = Arc::new(actor);
            let server = MockInferenceServer::start().await.unwrap();
            server.set_response("Summary. ".repeat(70));
            let mut cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            cfg.base_url = server.url();
            actor.chat_state_handle.update_sampling_config(cfg);
            actor.chat_state_handle.replace_conversation(conv);
            let threshold_tokens = 40_000u64 * 80 / 100;
            let before = actor.chat_state_handle.get_total_tokens().await;
            assert!(
                before > threshold_tokens,
                "seed must exceed threshold: {before}"
            );
            let result = actor.run_compact(None).await;
            assert!(result.is_ok(), "compaction should succeed: {result:?}");
            assert!(
                actor.compaction.prefix_released.load(Relaxed),
                "prefix must be released under pressure"
            );
            assert_eq!(
                actor.compaction.auto_compact_suppressed.load(Relaxed),
                SUPPRESS_STICKY,
                "an over-threshold released history must set sticky suppression"
            );
            let mut saw_failure = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) =
                    msg
                    && matches!(
                        &notif.update,
                        crate::extensions::notification::SessionUpdate::AutoCompactFailed { .. }
                    )
                {
                    saw_failure = true;
                }
            }
            assert!(
                !saw_failure,
                "successful compaction must not emit AutoCompactFailed"
            );
        })
        .await;
}
/// The cancel error carries the typed kind AND still extracts to the plain cancel text for text-only consumers (old pagers, log sinks).
#[test]
fn cancelled_error_is_typed_and_extracts_to_cancel_text() {
    use crate::session::helpers::session_compact::{COMPACT_CANCELLED_MSG, CompactFailure};
    use crate::session::helpers::session_compact::{CompactErrorKind, compact_error_kind};
    let err = CompactFailure::cancelled_error();
    assert_eq!(compact_error_kind(&err), Some(CompactErrorKind::Cancelled));
    assert_eq!(
        crate::sampling::error::acp_error_message(&err),
        COMPACT_CANCELLED_MSG
    );
    assert_eq!(
        SessionActor::user_facing_compact_error(&crate::sampling::error::acp_error_message(&err)),
        COMPACT_CANCELLED_MSG
    );
}
/// Raw producer input is scrubbed, single-lined, and capped at the chokepoint; already-normalized input passes through byte-identical.
#[test]
fn compact_error_data_scrubs_and_caps_raw_producer_input() {
    use crate::session::helpers::session_compact::{CompactErrorKind, compact_error_data};
    let Some(&(service, replacement)) = crate::sampling::error::SERVICE_NAME_REWRITES.first()
    else {
        panic!("SERVICE_NAME_REWRITES must be non-empty");
    };
    let raw = format!("{service} exploded:\nsecond line {}", "z".repeat(400));
    let data = compact_error_data(CompactErrorKind::Failed, &raw);
    let message = j(&data, "message").as_str().expect("message key");
    assert!(
        !message.contains(service),
        "service names must be scrubbed at the wire: {message}"
    );
    assert!(
        message.starts_with(&format!("{replacement} exploded: second line")),
        "scrubbed and single-lined: {message}"
    );
    assert!(message.len() <= 300, "capped: {} bytes", message.len());
    assert!(message.ends_with('…'), "truncation marker: {message}");
    let cased = service.to_ascii_uppercase();
    assert_eq!(
        j(
            &compact_error_data(CompactErrorKind::Failed, &format!("{cased} timed out")),
            "message",
        ),
        &format!("{replacement} timed out")
    );
    let normalized = "API error (status 400 Bad Request): invalid_image: too big";
    assert_eq!(
        j(
            &compact_error_data(CompactErrorKind::Failed, normalized),
            "message",
        ),
        normalized
    );
}
/// Prefix strip (nested wrappers included), single-line, and cap.
#[test]
fn user_facing_compact_error_strips_prefix_single_lines_and_caps() {
    use crate::session::helpers::session_compact::{COMPACT_CANCELLED_MSG, COMPACT_FAILED_PREFIX};
    assert_eq!(
        SessionActor::user_facing_compact_error("compact failed: API error\n  detail  line\t2"),
        "API error detail line 2"
    );
    let nested = distill_compaction::sampler::CompactionSampleError::Build(format!(
        "{COMPACT_FAILED_PREFIX}API error (status 400 Bad Request): invalid_image: too big"
    ))
    .to_string();
    assert_eq!(
        SessionActor::user_facing_compact_error(&nested),
        "API error (status 400 Bad Request): invalid_image: too big"
    );
    assert_eq!(
        SessionActor::user_facing_compact_error(
            "COMPACT FAILED: Compaction Sampler Start Failed: connection refused"
        ),
        "connection refused",
        "prefixes strip case-insensitively and in any order"
    );
    assert_eq!(
        SessionActor::user_facing_compact_error(&format!(
            "{}conversation is empty",
            super::COMPACTION_FAILED_GUARD_PREFIX
        )),
        "conversation is empty"
    );
    assert_eq!(
        SessionActor::user_facing_compact_error(COMPACT_CANCELLED_MSG),
        COMPACT_CANCELLED_MSG
    );
    for (pattern, replacement) in crate::sampling::error::SERVICE_NAME_REWRITES {
        assert_eq!(
            SessionActor::user_facing_compact_error(&format!(
                "compact failed: {pattern}: connection reset"
            )),
            format!("{replacement}: connection reset")
        );
    }
    assert_eq!(
        SessionActor::user_facing_compact_error("  no prefix here  "),
        "no prefix here"
    );
    let capped = SessionActor::user_facing_compact_error(&"y".repeat(600));
    assert!(capped.len() <= 300, "capped to {} bytes", capped.len());
    assert!(capped.ends_with('…'), "truncation marker: {capped}");
}
/// `classify_suppress_reason` maps each deterministic-failure shape to its fixed [`SuppressReason`].
#[test]
fn classify_suppress_reason_maps_error_text() {
    let classify = SessionActor::classify_suppress_reason;
    assert_eq!(
        classify("caller does not have permission … spending-limit reached"),
        SuppressReason::CreditBlock
    );
    assert_eq!(
        classify("you have run out of credits"),
        SuppressReason::CreditBlock
    );
    assert_eq!(
        classify("API error (status 402 Payment Required): Distill usage balance exhausted"),
        SuppressReason::CreditBlock
    );
    assert_eq!(
        classify("Distill usage limit reached"),
        SuppressReason::CreditBlock
    );
    assert_eq!(
        classify("This model's maximum prompt length is 500000"),
        SuppressReason::Size
    );
    assert_eq!(
        classify("compact failed: The prompt is too long for this model's context window."),
        SuppressReason::Size
    );
    assert_eq!(
        classify("provider error: context_length_exceeded"),
        SuppressReason::Size
    );
    assert_eq!(
        classify("API error (status 401 Unauthorized)"),
        SuppressReason::Auth
    );
    assert_eq!(
        classify("provider returned invalid_request_error: messages.3"),
        SuppressReason::Schema
    );
    assert_eq!(
        classify("upstream 500 internal error"),
        SuppressReason::Other
    );
}
/// `SuppressReason::as_str` is the stable telemetry wire value: BQ/OTLP and dashboards key off these exact strings.
/// Lock them so a rename can't break monitoring.
#[test]
fn suppress_reason_as_str_is_stable() {
    assert_eq!(SuppressReason::CreditBlock.as_ref(), "credit_block");
    assert_eq!(SuppressReason::Size.as_ref(), "size");
    assert_eq!(SuppressReason::Auth.as_ref(), "auth");
    assert_eq!(SuppressReason::Schema.as_ref(), "schema");
    assert_eq!(SuppressReason::Other.as_ref(), "other");
}
mod preserve_prefix {
    use super::super::preserve_inherited_prefix;
    use super::super::project_preserved_reseed_tokens;
    use super::at;
    use distill_sampling_types::conversation::ConversationItem;
    #[test]
    fn splices_inherited_with_compacted_suffix() {
        let conversation = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("parent q1"),
            ConversationItem::assistant("parent a1"),
            ConversationItem::user("child q1"),
        ];
        let compacted = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("summary"),
        ];
        let items = preserve_inherited_prefix(&conversation, compacted, 3).expect("Ok");
        assert_eq!(items.len(), 4);
        assert!(matches!(at(&items, 0), ConversationItem::System(_)));
    }
    /// Invariant: a head-only prefix lets compaction shrink the conversation; a whole-transcript prefix does not.
    /// That pinned floor is what causes the compaction loop.
    #[test]
    fn head_only_shrinks_full_transcript_does_not() {
        let mut conversation = vec![ConversationItem::system("sys")];
        for i in 0..8 {
            conversation.push(ConversationItem::user(format!("u{i}")));
            conversation.push(ConversationItem::assistant(format!("a{i}")));
        }
        let compacted = vec![
            ConversationItem::system("sys"),
            ConversationItem::assistant("summary"),
        ];
        let fixed = preserve_inherited_prefix(&conversation, compacted.clone(), 1).expect("Ok");
        assert!(fixed.len() < conversation.len(), "head-only shrinks");
        let buggy =
            preserve_inherited_prefix(&conversation, compacted, conversation.len()).expect("Ok");
        assert!(
            buggy.len() >= conversation.len(),
            "full prefix never shrinks"
        );
    }
    /// The reseed projection calibrates the bytes/4 estimate to real tokens (ratio != 1) and caps at the pre-compaction total.
    /// The release decision then reflects what the trigger applies next turn.
    #[test]
    fn project_preserved_reseed_tokens_calibrates_and_caps() {
        assert_eq!(
            project_preserved_reseed_tokens(30_000, 100_000, 50_000),
            60_000
        );
        assert_eq!(
            project_preserved_reseed_tokens(40_000, 70_000, 35_000),
            70_000
        );
        assert_eq!(
            project_preserved_reseed_tokens(20_000, 40_000, 40_000),
            20_000
        );
        assert_eq!(project_preserved_reseed_tokens(10, 5, 0), 5);
    }
    /// Both prefix and re-injected suffix may carry AGENTS.md; the splice must leave exactly one (else the model sees project instructions twice).
    #[test]
    fn does_not_duplicate_agents_md() {
        let conversation = vec![
            ConversationItem::system("sys"),
            ConversationItem::project_instructions("AGENTS.md"),
            ConversationItem::user("work"),
        ];
        let compacted = vec![
            ConversationItem::system("sys"),
            ConversationItem::project_instructions("AGENTS.md"),
            ConversationItem::user("summary"),
        ];
        let items = preserve_inherited_prefix(&conversation, compacted, 2).expect("Ok");
        let pi = items
            .iter()
            .filter(|i| super::super::is_project_instructions(i))
            .count();
        assert_eq!(pi, 1, "exactly one project-instructions item, not two");
    }
    #[test]
    fn keeps_reinjected_agents_md_when_prefix_lacks_it() {
        let conversation = vec![
            ConversationItem::system("sys"),
            ConversationItem::user("work"),
        ];
        let compacted = vec![
            ConversationItem::system("sys"),
            ConversationItem::project_instructions("AGENTS.md"),
            ConversationItem::user("summary"),
        ];
        let items = preserve_inherited_prefix(&conversation, compacted, 1).expect("Ok");
        let pi = items
            .iter()
            .filter(|i| super::super::is_project_instructions(i))
            .count();
        assert_eq!(
            pi, 1,
            "re-injected AGENTS.md preserved when prefix lacks one"
        );
    }
}
fn api_error_with_context_window(context_window: u64) -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        kind: distill_sampler::SamplingErrorKind::Api,
        status_code: Some(400),
        message: "prompt is too long".to_string(),
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
/// Pre-sampling check uses estimated tokens (includes tool-result delta).
#[tokio::test(flavor = "current_thread")]
async fn test_pre_sampling_uses_estimated_tokens() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(80_000, 100_000, 85, gateway_tx, persistence_tx).await;
            let result = actor.check_auto_compact_needed().await;
            assert!(result.is_none(), "80% should not trigger at 85% threshold");
            actor.chat_state_handle.record_token_usage(90_000);
            let result = actor.check_auto_compact_needed().await;
            assert!(result.is_some(), "90% should trigger");
            assert_eq!(result.unwrap().percentage, 90);
        })
        .await;
}
/// Model-switch compaction fires when switching to a smaller context window.
#[tokio::test(flavor = "current_thread")]
async fn test_model_switch_compaction_triggers_on_downgrade() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _) = mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _) = mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(86_000, 100_000, 85, gateway_tx, persistence_tx).await;
            actor.compaction.previous_model.set(Some(
                crate::session::compaction_config::PreviousModelInfo {
                    model_slug: "large-model".to_string(),
                    context_window: 200_000,
                },
            ));
            let prev = actor.compaction.previous_model.take();
            assert!(prev.is_some());
            let prev = prev.unwrap();
            assert_eq!(prev.context_window, 200_000);
            let cfg = actor.chat_state_handle.get_sampling_config().await.unwrap();
            assert!(prev.context_window > cfg.context_window.get());
            let total = actor.chat_state_handle.get_estimated_total_tokens().await;
            let trigger = actor.should_auto_compact(total, cfg.context_window);
            assert!(trigger.is_some(), "86% > 85% threshold, should trigger");
            actor.compaction.previous_model.set(Some(
                crate::session::compaction_config::PreviousModelInfo {
                    model_slug: "small-model".to_string(),
                    context_window: 50_000,
                },
            ));
            let prev = actor.compaction.previous_model.take().unwrap();
            assert!(prev.context_window <= cfg.context_window.get());
        })
        .await;
}
#[tokio::test(flavor = "current_thread")]
async fn get_transcript_path_returns_some_when_file_exists() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) = mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor =
                create_test_actor(50_000, 200_000, 85, gateway_tx, persistence_tx).await;
            actor.compaction.compaction_mode = distill_chat_state::CompactionMode::Transcript;
            let session_dir = crate::session::persistence::session_dir(&actor.session_info);
            std::fs::create_dir_all(&session_dir).unwrap();
            let updates_path = session_dir.join("updates.jsonl");
            std::fs::write(&updates_path, "{}\n").unwrap();
            let result = actor.get_transcript_path();
            assert!(result.is_some(), "file exists → Some");
            assert!(
                result.as_ref().unwrap().ends_with("updates.jsonl"),
                "path should end with updates.jsonl, got: {:?}",
                result,
            );
            let hint = actor.transcript_hint().expect("transcript hint present");
            assert!(hint.contains("read the full transcript"));
            assert!(hint.ends_with("updates.jsonl"));
            actor.compaction.compaction_mode = distill_chat_state::CompactionMode::Summary;
            assert!(actor.transcript_hint().is_none());
            let _ = std::fs::remove_file(&updates_path);
            let _ = std::fs::remove_dir_all(&session_dir);
        })
        .await;
}
