// Modified for Distill by Samuel Fajreldines, 2026.
use crate::agent::subagent::SubagentSpawnContext;
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
pub(crate) type GatewayOut = <acp::AgentSide as distill_acp_lib::AcpSide>::OutMessage;
pub(crate) fn test_gateway() -> GatewaySender {
    let (tx, _rx) = mpsc::unbounded_channel();
    GatewaySender::new(tx)
}
/// Like `test_gateway` but returns the receiver; keep it alive for the test.
pub(crate) fn test_gateway_with_receiver() -> (GatewaySender, mpsc::UnboundedReceiver<GatewayOut>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (GatewaySender::new(tx), rx)
}
pub(crate) fn ctx_with_toggle(toggle: HashMap<String, bool>) -> SubagentSpawnContext {
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
        setup_failure: None,
        run_shell_child_harness: None,
        fail_start_metadata_write: false,
        alpha_test_key: None,
        auth_method_id: acp::AuthMethodId::new("test"),
        model_id: acp::ModelId::new("test"),
        parent_effort_auto: true,
        auth: None,
        parent_cwd: PathBuf::from("/tmp"),
        parent_session_id: "test-parent".into(),
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
        subagent_toggle: toggle,
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
        auto_compact_threshold_tiers: crate::agent::subagent::AutoCompactThresholdTiers::default(),
        permission_handle: None,
        worktree_type: crate::util::config::WorktreeType::Linked,
        api_key_provider: None,
        image_description_model: crate::test_support::TEST_MODEL.to_owned(),
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
        goal_loop_active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        parent_terminal_backend: None,
        parent_notification_handle: None,
        parent_scheduler_handle: None,
        subagent_sampling_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
            distill_tools::implementations::distill::task::admission::DEFAULT_MAX_CONCURRENT,
        )),
    }
}
