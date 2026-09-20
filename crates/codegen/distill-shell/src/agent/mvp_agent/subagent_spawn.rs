// Modified for Distill by Samuel Fajreldines, 2026.
//! Builds the snapshots of parent state that `crate::agent::subagent::spawn` hands to a child.
//!
//! These builders live inside `mvp_agent` (`use super::*`) so they can read `MvpAgent`'s private state directly.
//! `crate::agent::subagent::spawn` drives the child's lifecycle and reaches back in only through the `pub(crate)` functions below.
//!
//! - `start_subagent_coordinator`: takes the event receiver and presentation state and starts the coordinator via `spawn_subagent_coordinator`.
//! - `build_subagent_validation_context` and `try_build_subagent_spawn_context`: snapshot config and the parent handle for the child.
use super::*;
use crate::session::repo_changes::UploadMethod;
impl MvpAgent {
    /// Starts the shared coordinator actor; idempotent.
    /// Takes the event receiver and the concurrency limits off private state and passes them to `spawn_subagent_coordinator`.
    /// `LocalRef` lets the `!Send` runner touch `self`.
    pub(super) fn start_subagent_coordinator(&self) {
        let Some(rx) = self.subagent_event_rx.borrow_mut().take() else {
            return;
        };
        let agent_ref = LocalRef::new(self);
        let limits = distill_tools::implementations::distill::task::admission::SubagentLimits {
            max_concurrent: self.cfg.borrow().subagents_max_concurrent,
            behavior: self.cfg.borrow().subagents_limit_behavior,
        };
        crate::agent::subagent::spawn_subagent_coordinator(agent_ref.clone(), rx, limits);
        let (trace_tx, mut trace_rx) = tokio::sync::mpsc::unbounded_channel::<
            crate::upload::turn::SyntheticTurnTraceRequest,
        >();
        self.subagent_presentation.borrow_mut().synthetic_trace_tx = Some(trace_tx);
        tokio::task::spawn_local({
            let agent_ref = agent_ref.clone();
            async move {
                while let Some(request) = trace_rx.recv().await {
                    tokio::task::spawn_local({
                        let agent_ref = agent_ref.clone();
                        async move {
                            handle_synthetic_turn_trace(agent_ref, request).await;
                        }
                    });
                }
            }
        });
    }
    /// Lightweight context for the `SubagentEvent::ValidateType` drain arm.
    /// Tolerates an evicted parent session: returns built-in defaults and warns.
    pub(crate) fn build_subagent_validation_context(
        &self,
        parent_session_id: &str,
    ) -> crate::agent::subagent::SubagentValidationContext {
        let parent_sid = acp::SessionId::new(parent_session_id);
        let (parent_cwd, allowed_subagent_types) = {
            let ps = self.resident_handle(&parent_sid);
            warn_on_missing_parent_session_for_validate_type(parent_session_id, ps.is_some());
            (
                ps.as_ref()
                    .map(|h| std::path::PathBuf::from(&h.info.cwd))
                    .unwrap_or_default(),
                ps.as_ref().and_then(|h| h.allowed_subagent_types.clone()),
            )
        };
        let (cli_agent_names, subagent_toggle) = {
            let cfg = self.cfg.borrow();
            (
                cfg.cli_agents.iter().map(|d| d.name.clone()).collect(),
                cfg.subagent_toggle.clone(),
            )
        };
        crate::agent::subagent::SubagentValidationContext {
            parent_cwd,
            plugin_registry: self.plugin_registry_handle.snapshot(),
            subagent_toggle,
            allowed_subagent_types,
            cli_agent_names,
        }
    }
    pub(crate) async fn send_human_subagent_message(
        &self,
        parent_session_id: &str,
        agent_address: String,
        text: String,
        operation: distill_tools::implementations::distill::task::types::ActiveAgentMessageOperation,
    ) -> distill_tools::implementations::distill::task::types::ActiveAgentMessageOutcome {
        use distill_tools::implementations::distill::task::backend::SubagentBackend;
        if !self
            .cfg
            .borrow()
            .is_feature_enabled(crate::agent::config::Feature::ActiveAgentMessages)
        {
            return distill_tools::implementations::distill::task::types::ActiveAgentMessageOutcome::Unsupported;
        }
        if self
            .session_handle_waiting_for_load(&acp::SessionId::new(parent_session_id))
            .await
            .is_none()
        {
            return distill_tools::implementations::distill::task::types::ActiveAgentMessageOutcome::NotFoundOrNotOwned;
        }
        let request = match distill_tools::implementations::distill::task::types::ActiveAgentMessageRequest::try_new_from_human(
            agent_address,
            text,
            operation,
        ) {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        distill_tools::implementations::distill::task::backend::ChannelBackend::for_coordinator_session(
                self.subagent_event_tx.clone(),
                parent_session_id,
            )
            .send_active_message(request)
            .await
    }
    /// Test-only infallible wrapper; production uses the fallible variant.
    #[cfg(test)]
    pub(super) fn build_subagent_spawn_context(
        &self,
        parent_session_id: &str,
    ) -> crate::agent::subagent::SubagentSpawnContext {
        self.try_build_subagent_spawn_context(parent_session_id)
            .expect("parent session must exist when spawning subagents")
    }
    /// Build a `SubagentSpawnContext` from agent state and the parent's shared resources; `None` when the parent handle is gone.
    /// The many short-lived `self.cfg.borrow()` calls below MUST stay separate.
    /// The `prepare_*` and `resolve_*` helpers borrow `self.cfg` internally, so hoisting them under one outer borrow panics with a double borrow.
    pub(crate) fn try_build_subagent_spawn_context(
        &self,
        parent_session_id: &str,
    ) -> Option<crate::agent::subagent::SubagentSpawnContext> {
        let parent_sid = acp::SessionId::new(parent_session_id);
        let parent_handle = self.resident_handle(&parent_sid);
        let ps = parent_handle.as_ref();
        let parent_model_id = ps
            .map(|h| h.model_id.clone())
            .unwrap_or_else(|| self.models_manager.current_model_id());
        let parent_chat_state = ps.map(|h| h.chat_state_handle.clone());
        let parent_cmd_tx = ps.map(|h| h.cmd_tx.clone());
        let parent_cwd = ps
            .map(|h| std::path::PathBuf::from(&h.info.cwd))
            .unwrap_or_default();
        let yolo_mode = ps.map(|h| h.yolo_mode).unwrap_or(self.default_yolo_mode);
        let parent_effort_auto = ps
            .map(|h| {
                h.jev_effort_auto
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or_else(|| self.models_manager.current_effort_auto());
        let parent_depth = ps.map(|h| h.tool_context.subagent_depth).unwrap_or(0);
        let hunk_tracker_handle = ps
            .map(|h| h.tool_context.hunk_tracker_handle.clone())
            .unwrap_or_else(distill_hunk_tracker::HunkTrackerHandle::noop);
        let hunk_tracking_enabled = ps
            .map(|h| h.tool_context.hunk_tracking_enabled)
            .unwrap_or(false);
        let fs = ps
            .map(|h| h.tool_context.fs.inner().clone())
            .unwrap_or_else(|| {
                std::sync::Arc::new(distill_workspace::file_system::LocalFs::new(
                    parent_cwd.clone(),
                ))
            });
        let terminal = ps
            .map(|h| h.tool_context.terminal.clone())
            .unwrap_or_else(|| {
                std::sync::Arc::new(crate::terminal::TerminalRunner::new(
                    std::sync::Arc::new(self.gateway.clone()),
                    parent_sid.clone(),
                ))
            });
        let session_env = ps
            .map(|h| h.tool_context.session_env.clone())
            .unwrap_or_else(|| std::sync::Arc::new(std::collections::HashMap::new()));
        let parent_attribution_callback = ps.and_then(|h| h.attribution_callback.clone());
        let parent_agent_name = ps.map(|h| h.agent_name.clone());
        let parent_managed_mcp_proxy_base_url = ps.map(|h| h.managed_mcp_proxy_base_url.clone());
        let (
            parent_workspace_ops,
            parent_terminal_backend,
            parent_notification_handle,
            parent_scheduler_handle,
        ) = parent_handle.as_ref().map(|ps| {
            (
                ps.workspace_ops.clone(),
                ps.terminal_backend.clone(),
                ps.tools_notification_handle.clone(),
                ps.scheduler_handle.clone(),
            )
        })?;
        let available_models = self.models_manager.models();
        let (parent_lsp, parent_process_scope) = {
            let parent = parent_handle.as_ref();
            (
                parent.as_ref().and_then(|h| h.tool_context.lsp.clone()),
                parent
                    .as_ref()
                    .and_then(|h| h.tool_context.process_scope.clone()),
            )
        };
        let am = self.auth_manager.clone();
        let inference_idle_timeout_secs = {
            let per_model = config::find_model_by_id(&available_models, parent_model_id.0.as_ref())
                .and_then(|e| e.info.inference_idle_timeout_secs);
            let cfg = self.cfg.borrow();
            let remote = cfg
                .remote_settings
                .as_ref()
                .and_then(|s| s.inference_idle_timeout_secs);
            per_model.or(remote).unwrap_or(600).max(10)
        };
        let parent_hook_registry = parent_handle.as_ref().and_then(|h| h.hook_registry.clone());
        let parent_max_turns = parent_handle.as_ref().and_then(|h| h.max_turns);
        let parent_model_agent_type =
            config::find_model_by_id(&available_models, parent_model_id.0.as_ref())
                .map(|e| e.info.agent_type.clone());
        let parent_non_interactive = parent_handle
            .as_ref()
            .map(|h| h.non_interactive)
            .unwrap_or(false);
        let (gcs_upload_method, gcs_bucket_url) = match self.trace_upload_config_snapshot() {
            Some(method) => {
                let bucket = match &method {
                    UploadMethod::Direct { .. } => self
                        .cfg
                        .borrow()
                        .endpoints
                        .resolve_trace_bucket_url()
                        .map(|r| r.value),
                    UploadMethod::Proxy { .. } => Some("proxy-managed".to_string()),
                    UploadMethod::S3 { bucket, .. } => Some(format!("s3://{bucket}")),
                };
                match bucket {
                    Some(url) => (Some(method), Some(url)),
                    None => (None, None),
                }
            }
            None => (None, None),
        };
        let project_trusted = crate::agent::folder_trust::project_scope_allowed(&parent_cwd);
        let (base_roles, base_personas, subagent_model_overrides, subagent_toggle) = {
            let cfg = self.cfg.borrow();
            (
                cfg.subagent_roles.clone(),
                cfg.subagent_personas.clone(),
                cfg.subagent_model_overrides.clone(),
                cfg.subagent_toggle.clone(),
            )
        };
        let (subagent_roles, subagent_personas) =
            crate::config::SubagentsConfig::effective_definition_maps(
                &base_roles,
                &base_personas,
                &parent_cwd,
                project_trusted,
            );
        let inherited_tool_overrides = parent_handle
            .as_ref()
            .and_then(|ps| ps.resolved_tool_overrides.load_full().map(|o| (*o).clone()));
        let memory_config = self.memory_config_snapshot();
        let mut ctx = crate::agent::subagent::SubagentSpawnContext {
            lsp: parent_lsp,
            process_scope: parent_process_scope,
            client_hooks: Default::default(),
            sampling_config: self.sampling_config.borrow().clone(),
            #[cfg(test)]
            setup_failure: None,
            #[cfg(test)]
            run_shell_child_harness: None,
            #[cfg(test)]
            fail_start_metadata_write: false,
            managed_mcp_proxy_base_url: parent_managed_mcp_proxy_base_url
                .unwrap_or_else(|| self.cli_chat_proxy_base_url()),
            alpha_test_key: self.alpha_test_key(),
            auth_method_id: self
                .auth_method_id
                .load()
                .as_deref()
                .cloned()
                .unwrap_or_else(|| acp::AuthMethodId::new("default")),
            model_id: parent_model_id,
            parent_effort_auto,
            auth: self.current_or_buffered_auth(),
            parent_cwd: parent_cwd.clone(),
            parent_session_id: parent_session_id.to_string(),
            active_message_parent_prompt_index: parent_handle
                .as_ref()?
                .tool_context
                .active_message_parent_prompt_index
                .clone(),
            inherited_tool_overrides,
            yolo_mode,
            subagent_event_tx: self.subagent_event_tx.event_sender().0,
            parent_depth,
            subagents_max_depth: self.cfg.borrow().subagents_max_depth,
            workflow_max_concurrent_agents: self.cfg.borrow().workflow_max_concurrent_agents,
            media_gen_batch_limits: self.cfg.borrow().media_gen_batch_limits,
            inference_idle_timeout_secs,
            parent_compaction: crate::session::CompactionPins::default(),
            auto_compact_threshold_tiers:
                crate::agent::subagent::AutoCompactThresholdTiers::capture(&self.cfg.borrow()),
            hunk_tracker_handle,
            hunk_tracking_enabled,
            fs,
            terminal,
            session_env,
            memory_config: memory_config.clone(),
            memory_mode: memory_config
                .as_ref()
                .map(|config| config.mode)
                .unwrap_or_else(|| {
                    if self.cfg.borrow().memory_v2.enabled == Some(true) {
                        crate::config::MemoryMode::V2
                    } else {
                        crate::config::MemoryMode::Legacy
                    }
                }),
            web_search_sampling_config: self.prepare_web_search_sampling_config(),
            web_fetch_config: self.prepare_web_fetch_config(),
            image_gen_config: self.prepare_image_gen_config(),
            video_gen_config: self.prepare_video_gen_config(),
            app_builder_deployer_config: self.prepare_app_builder_deployer_config(),
            write_file_enabled: self
                .cfg
                .borrow()
                .is_feature_enabled(crate::agent::config::Feature::WriteFile),
            active_agent_messages_enabled: self
                .cfg
                .borrow()
                .is_feature_enabled(crate::agent::config::Feature::ActiveAgentMessages),
            goal_enabled: self.cfg.borrow().resolve_goal().value,
            background_workflows_enabled: self.cfg.borrow().resolve_workflows().value,
            ask_user_question_enabled: false,
            parent_non_interactive,
            parent_cmd_tx: parent_cmd_tx.clone(),
            spawner_address_target: None,
            parent_session_info: parent_handle.as_ref().map(|h| crate::session::info::Info {
                id: parent_sid.clone(),
                cwd: h.info.cwd.clone(),
            }),
            parent_chat_state,
            parent_max_turns,
            available_models,
            subagent_model_overrides,
            subagent_toggle,
            subagent_roles,
            subagent_personas,
            disable_web_search: self.cfg.borrow().disable_web_search,
            todo_gate: self.cfg.borrow().todo_gate,
            remote_settings: self.cfg.borrow().remote_settings.clone(),
            laziness_debug_log: self.cfg.borrow().laziness_debug_log.clone(),
            backend_tools_enabled: self
                .cfg
                .borrow()
                .is_feature_enabled(crate::agent::config::Feature::BackendTools),
            respect_gitignore: self.cfg.borrow().respect_gitignore,
            path_not_found_hints: self.cfg.borrow().path_not_found_hints,
            tool_params_json: {
                let cfg = self.cfg.borrow();
                crate::session::agent_rebuild::ResolvedToolParamsJson {
                    bash: Some(
                        cfg.toolset
                            .bash
                            .to_bash_params_json_with_remote(cfg.remote_settings.as_ref()),
                    ),
                    ask_user_question: None,
                }
            },
            plugin_registry: self.plugin_registry_handle.snapshot(),
            models_manager: self.models_manager.clone(),
            file_tool_overrides: {
                let cfg = self.cfg.borrow();
                let effective = cfg
                    .toolset
                    .resolve_file_toolset(cfg.remote_settings.as_ref());
                if effective != crate::tools::FileToolset::Standard {
                    effective.tool_configs(&cfg.toolset.hashline).ok()
                } else {
                    None
                }
            },
            gcs_bucket_url,
            agent_config: Some(self.cfg.borrow().clone()),
            gcs_upload_method,
            hook_registry: parent_hook_registry,
            permission_handle: parent_handle.as_ref().map(|h| h.permission_handle.clone()),
            worktree_type: self.worktree_type,
            api_key_provider: Some(Arc::new(distill_login::manager::SharedAuthKeyProvider(
                am.clone(),
            ))),
            image_description_model: self.resolve_image_description_model(),
            workspace_ops: parent_workspace_ops.clone(),
            auth_manager: am.clone(),
            attribution_callback: parent_attribution_callback,
            parent_agent_name,
            parent_model_agent_type,
            allowed_subagent_types: parent_handle
                .as_ref()
                .and_then(|h| h.allowed_subagent_types.clone()),
            parent_mcp_configs: parent_handle
                .as_ref()
                .map(|h| h.mcp_servers.clone())
                .unwrap_or_default(),
            managed_mcp_state: self.managed_mcp_cache.clone(),
            parent_mcp_pool: None,
            parent_tool_definitions: None,
            parent_skills: None,
            parent_skills_config: self.cfg.borrow().skills.clone(),
            parent_compat: self.cfg.borrow().compat_resolved,
            parent_paths_config: self.cfg.borrow().paths.clone(),
            synthetic_trace_tx: parent_handle
                .as_ref()
                .and_then(|h| h.tool_context.synthetic_trace_tx.clone()),
            task_output_tool_name: parent_handle
                .as_ref()
                .map(|h| h.tool_context.task_output_tool_name.clone())
                .unwrap_or_else(|| {
                    distill_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string()
                }),
            scheduler_delete_tool_name: parent_handle
                .as_ref()
                .and_then(|h| h.tool_context.scheduler_delete_tool_name.clone()),
            scheduler_create_tool_name: parent_handle
                .as_ref()
                .and_then(|h| h.tool_context.scheduler_create_tool_name.clone()),
            auto_wake_enabled: self
                .cfg
                .borrow()
                .is_feature_enabled(crate::agent::config::Feature::AutoWake),
            goal_loop_active: parent_handle
                .as_ref()
                .map(|h| h.tool_context.goal_loop_active_gate.clone())
                .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))),
            parent_terminal_backend: parent_terminal_backend.clone(),
            parent_notification_handle: parent_notification_handle.clone(),
            parent_scheduler_handle: parent_scheduler_handle.clone(),
            subagent_sampling_semaphore: self.subagent_sampling_semaphore.clone(),
        };
        ctx.parent_compaction =
            crate::agent::subagent::SubagentSpawnContext::snapshot_parent_compaction_pins(
                ctx.resolve_compaction_mode(),
                ctx.resolve_feature(crate::agent::config::Feature::TwoPassCompaction),
                ctx.parent_agent_name.as_deref(),
                ctx.parent_model_agent_type.as_deref(),
                &ctx.parent_cwd,
            );
        Some(ctx)
    }
}
