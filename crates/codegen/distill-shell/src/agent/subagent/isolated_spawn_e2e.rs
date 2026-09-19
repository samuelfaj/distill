// Modified for Distill by Samuel Fajreldines, 2026.
//! One isolated `run_shell_child` spawn for grove-e2e. Feature `test-support`.
use super::spawn::present_child_completion;
use super::{ShellChildRuntime, ShellCompletionData, SubagentSpawnContext, run_shell_child};
use crate::session::SessionCommand;
use crate::util::config::RemoteSettings;
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_tool_types::SubagentIsolationMode;
use distill_tools::implementations::distill::task::coordinator::{
    ChildCompletion, ChildRunOutput, ChildRunner, LocalBoxFuture, SubagentCoordinator,
};
use distill_tools::implementations::distill::task::root_control::NoRootControl;
use distill_tools::implementations::distill::task::types::{
    SubagentDescribeOutcome, SubagentOwner, SubagentRequest, SubagentValidateTypeOutcome,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
fn test_gateway() -> GatewaySender {
    let (tx, _rx) = mpsc::unbounded_channel();
    GatewaySender::new(tx)
}
fn test_gateway_with_receiver() -> (
    GatewaySender,
    mpsc::UnboundedReceiver<<acp::AgentSide as distill_acp_lib::AcpSide>::OutMessage>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (GatewaySender::new(tx), rx)
}
fn spawn_ctx(parent_cwd: PathBuf) -> SubagentSpawnContext {
    let (tx, _rx) = mpsc::unbounded_channel();
    SubagentSpawnContext {
        lsp: None,
        process_scope: None,
        parent_max_turns: None,
        client_hooks: Default::default(),
        sampling_config: distill_sampler::SamplerConfig {
            context_window: 256_000,
            ..Default::default()
        },
        alpha_test_key: None,
        auth_method_id: acp::AuthMethodId::new("test"),
        model_id: acp::ModelId::new("test"),
        auth: None,
        parent_cwd,
        parent_session_id: "grove-e2e-parent".into(),
        active_message_parent_prompt_index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        inherited_tool_overrides: None,
        yolo_mode: false,
        subagent_event_tx: tx,
        hunk_tracker_handle: distill_hunk_tracker::HunkTrackerHandle::noop(),
        hunk_tracking_enabled: false,
        fs: Arc::new(distill_workspace::file_system::LocalFs::new(PathBuf::from(
            "/tmp",
        ))),
        terminal: Arc::new(crate::terminal::TerminalRunner::new(
            Arc::new(test_gateway()),
            acp::SessionId::new("test"),
        )),
        session_env: Arc::new(HashMap::new()),
        memory_config: None,
        memory_mode: crate::config::MemoryMode::Legacy,
        web_search_sampling_config: None,
        web_fetch_config: Default::default(),
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        write_file_enabled: true,
        active_agent_messages_enabled: false,
        goal_enabled: false,
        background_workflows_enabled: false,
        ask_user_question_enabled: false,
        parent_non_interactive: false,
        parent_cmd_tx: None,
        spawner_address_target: None,
        parent_session_info: None,
        subagent_roles: HashMap::new(),
        subagent_personas: HashMap::new(),
        parent_chat_state: None,
        available_models: indexmap::IndexMap::new(),
        subagent_model_overrides: HashMap::new(),
        subagent_toggle: HashMap::new(),
        disable_web_search: false,
        todo_gate: false,
        remote_settings: None,
        laziness_debug_log: None,
        backend_tools_enabled: true,
        respect_gitignore: false,
        path_not_found_hints: false,
        tool_params_json: Default::default(),
        plugin_registry: None,
        models_manager: Default::default(),
        file_tool_overrides: None,
        agent_config: None,
        gcs_bucket_url: None,
        gcs_upload_method: None,
        hook_registry: None,
        parent_depth: 0,
        subagents_max_depth: distill_tools::implementations::distill::task::MAX_SUBAGENT_DEPTH,
        workflow_max_concurrent_agents:
            crate::session::workflow::host_service::DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS,
        media_gen_batch_limits: distill_tools::media_gen_limits::MediaGenBatchLimits::default(),
        inference_idle_timeout_secs: 600,
        parent_compaction: crate::session::CompactionPins::default(),
        auto_compact_threshold_tiers: super::AutoCompactThresholdTiers::default(),
        permission_handle: None,
        worktree_type: crate::util::config::WorktreeType::Linked,
        api_key_provider: None,
        image_description_model: "test-model".to_owned(),
        workspace_ops: distill_workspace::WorkspaceOps::for_test(),
        auth_manager: Arc::new(distill_login::AuthManager::new(
            std::path::Path::new("/tmp/nonexistent-grok-test"),
            distill_login::GrokComConfig::default(),
        )),
        attribution_callback: None,
        parent_agent_name: None,
        parent_model_agent_type: None,
        allowed_subagent_types: None,
        parent_mcp_configs: vec![],
        managed_mcp_state: crate::session::managed_mcp::ManagedMcpStateHandle::default(),
        managed_mcp_proxy_base_url: String::new(),
        parent_mcp_pool: None,
        parent_tool_definitions: None,
        parent_skills: None,
        parent_skills_config: distill_agent::prompt::skills::SkillsConfig::default(),
        parent_compat: distill_tools::types::compat::CompatConfig::default(),
        parent_paths_config: Default::default(),
        synthetic_trace_tx: None,
        task_output_tool_name: distill_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL
            .to_string(),
        scheduler_delete_tool_name: None,
        scheduler_create_tool_name: None,
        auto_wake_enabled: true,
        goal_loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        parent_terminal_backend: None,
        parent_notification_handle: None,
        parent_scheduler_handle: None,
        subagent_sampling_semaphore: Arc::new(tokio::sync::Semaphore::new(
            distill_tools::implementations::distill::task::admission::DEFAULT_MAX_CONCURRENT,
        )),
    }
}
const SPAWN_TIMEOUT: Duration = Duration::from_secs(90);
/// Outcome of one isolated subagent spawn (worktree kept; snapshot dispose off).
pub struct IsolatedSubagentSpawn {
    pub success: bool,
    pub worktree_path: Option<PathBuf>,
    pub error: Option<String>,
}
#[derive(Clone)]
struct Runner {
    ctx: Arc<parking_lot::Mutex<Option<SubagentSpawnContext>>>,
    gateway: GatewaySender,
}
impl ChildRunner for Runner {
    type Control = ShellChildRuntime;
    type RootControl = NoRootControl;
    type CompletionData = ShellCompletionData;
    type RunFuture = LocalBoxFuture<ChildRunOutput<ShellCompletionData>>;
    type ValidateFuture = LocalBoxFuture<SubagentValidateTypeOutcome>;
    type DescribeFuture = LocalBoxFuture<SubagentDescribeOutcome>;
    fn run(
        &self,
        run: distill_tools::implementations::distill::task::coordinator::ChildRunRequest<
            Self::Control,
        >,
    ) -> Self::RunFuture {
        let ctx = self.ctx.lock().take().expect("run context");
        let gateway = self.gateway.clone();
        let completion_data = ShellCompletionData::from_context(&ctx, run.attempt_id.clone(), None);
        Box::pin(async move { run_shell_child(run, ctx, completion_data, gateway, None).await })
    }
    fn validate_type(
        &self,
        _subagent_type: String,
        _parent_session_id: String,
    ) -> Self::ValidateFuture {
        Box::pin(std::future::ready(SubagentValidateTypeOutcome::Ok))
    }
    fn describe_type(
        &self,
        _subagent_type: String,
        _harness_agent_type: Option<String>,
        _parent_session_id: String,
    ) -> Self::DescribeFuture {
        Box::pin(std::future::ready(SubagentDescribeOutcome::Unavailable))
    }
    fn supports_wake(&self) -> bool {
        true
    }
    fn on_completed(
        &self,
        completion: ChildCompletion<Self::CompletionData>,
        terminal_published: Box<dyn FnOnce() + Send>,
    ) {
        present_child_completion(completion, &self.gateway, false);
        terminal_published();
    }
}
/// Spawn one isolated subagent from `parent_cwd`. Must run on a `LocalSet`.
pub async fn spawn_isolated_subagent_for_e2e(
    parent_cwd: &Path,
    remote: RemoteSettings,
    mock_base_url: &str,
) -> IsolatedSubagentSpawn {
    use distill_tools::implementations::distill::task::backend::{ChannelBackend, SubagentBackend};
    use distill_tools::implementations::distill::task::coordinator::CoordinatorConfig;
    let mut ctx = spawn_ctx(parent_cwd.to_path_buf());
    ctx.remote_settings = Some(remote);
    ctx.sampling_config.base_url = mock_base_url.to_owned();
    ctx.sampling_config.model = "test-model".into();
    ctx.sampling_config.api_backend = crate::sampling::ApiBackend::Responses;
    ctx.model_id = acp::ModelId::new("test-model");
    let (parent_cmd_tx, parent_cmd_rx) = mpsc::unbounded_channel();
    ctx.parent_cmd_tx = Some(parent_cmd_tx);
    let usage_ack = tokio::task::spawn_local(acknowledge_parent_usage(parent_cmd_rx));
    let (gateway, _gateway_rx) = test_gateway_with_receiver();
    let (command_tx, command_rx) = SubagentCoordinator::<Runner>::channel();
    let coordinator = tokio::task::spawn_local(
        SubagentCoordinator::from_channel(
            command_rx,
            Runner {
                ctx: Arc::new(parking_lot::Mutex::new(Some(ctx))),
                gateway,
            },
            CoordinatorConfig::default(),
        )
        .run(),
    );
    let backend = ChannelBackend::for_coordinator_session(command_tx, "grove-e2e-parent");
    let id = uuid::Uuid::now_v7().to_string();
    let mut request = SubagentRequest {
        id: id.clone(),
        prompt: String::new(),
        description: "grove-e2e isolated".into(),
        subagent_type: "general-purpose".into(),
        parent_session_id: "grove-e2e-parent".into(),
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
    };
    request.runtime_overrides.isolation = Some(SubagentIsolationMode::Worktree);
    let spawned = tokio::time::timeout(SPAWN_TIMEOUT, backend.spawn(request, None)).await;
    drop(backend);
    let _ = coordinator.await;
    usage_ack.abort();
    match spawned {
        Ok(Ok(result)) => IsolatedSubagentSpawn {
            success: result.success,
            worktree_path: result.worktree_path.map(PathBuf::from),
            error: result.error,
        },
        Ok(Err(e)) => IsolatedSubagentSpawn {
            success: false,
            worktree_path: None,
            error: Some(e.to_string()),
        },
        Err(_) => IsolatedSubagentSpawn {
            success: false,
            worktree_path: None,
            error: Some("isolated subagent spawn timed out".into()),
        },
    }
}
async fn acknowledge_parent_usage(mut parent_cmd_rx: mpsc::UnboundedReceiver<SessionCommand>) {
    while let Some(command) = parent_cmd_rx.recv().await {
        if let SessionCommand::RecordSubagentUsage { respond_to, .. } = command {
            let _ = respond_to.send(());
        }
    }
}
