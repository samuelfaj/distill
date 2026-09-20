// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use distill_chat_state::conversation_util::replace_or_insert_system_head;
impl SessionActor {
    pub(super) async fn handle_set_session_model(
        self: &std::sync::Arc<Self>,
        switch: crate::session::SessionModelSwitch,
    ) -> Result<acp::ModelId, acp::Error> {
        let crate::session::SessionModelSwitch {
            mut sampling_config,
            canonical_model_id,
            model_selection_intent,
            use_concise,
            is_family_switch,
            apply_prompt_override,
            skip_prompt_rewrite,
            auto_compact_threshold_percent,
            system_prompt_label,
        } = switch;
        if let Some(current) = self.chat_state_handle.get_sampling_config().await
            && let Some(id) = current.conversation_group_id
        {
            sampling_config.conversation_group_id = Some(id);
        }
        let model_id = acp::ModelId::new(sampling_config.model.clone());
        let canonical_model_id = canonical_model_id.unwrap_or_else(|| model_id.clone());
        let new_context_window = self.compaction.context_window_override.unwrap_or_else(|| {
            std::num::NonZeroU64::new(sampling_config.context_window).unwrap_or_else(|| {
                std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                    .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
            })
        });
        let prev_threshold = self.compaction.threshold_percent.get();
        if prev_threshold != auto_compact_threshold_percent {
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_model = %sampling_config.model,
                old_threshold = prev_threshold,
                new_threshold = auto_compact_threshold_percent,
                "auto_compact_threshold_percent updated for model switch"
            );
        }
        self.compaction
            .threshold_percent
            .set(auto_compact_threshold_percent);
        self.supports_backend_search
            .set(sampling_config.supports_backend_search);
        self.compactions_remaining
            .set(sampling_config.compactions_remaining);
        self.compaction_at_tokens
            .set(sampling_config.compaction_at_tokens);
        distill_telemetry::unified_log::info(
            "backend_search: model switch",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "new_model": &sampling_config.model,
                "api_backend": format!("{:?}", sampling_config.api_backend),
                "supports_backend_search": sampling_config.supports_backend_search,
            })),
        );
        self.chat_state_handle
            .update_sampling_config(distill_sampling_types::SamplingConfig {
                base_url: sampling_config.base_url.clone(),
                mtls_cert_dir: sampling_config.mtls_cert_dir.clone(),
                model: sampling_config.model.clone(),
                max_completion_tokens: sampling_config.max_completion_tokens,
                temperature: sampling_config.temperature,
                top_p: sampling_config.top_p,
                max_retries: Some(distill_sampler::resolve_max_retries(
                    sampling_config.max_retries,
                )),
                rate_limit_retry_threshold: sampling_config.rate_limit_retry_threshold,
                api_backend: sampling_config.api_backend.clone(),
                extra_headers: sampling_config.extra_headers.clone(),
                conversation_group_id: sampling_config.conversation_group_id.clone(),
                query_params: sampling_config.query_params.clone(),
                env_http_headers: sampling_config.env_http_headers.clone(),
                context_window: new_context_window,
                reasoning_effort: sampling_config.reasoning_effort,
                reasoning_shape: sampling_config.reasoning_shape,
                reasoning_summary: sampling_config.reasoning_summary,
                stream_tool_calls: Some(sampling_config.stream_tool_calls),
            });
        let estimated_total = self.chat_state_handle.get_estimated_total_tokens().await;
        self.signals_handle()
            .update_context_usage(estimated_total, new_context_window.get());
        let _ = self.signals_handle().snapshot().await;
        let existing = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key));
        self.chat_state_handle
            .update_credentials(distill_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type(
                    sampling_config.model.as_str(),
                    session_key.as_deref(),
                    existing.auth_type,
                ),
                alpha_test_key: existing.alpha_test_key,
                client_version: sampling_config.client_version.clone(),
            });
        self.invalidate_model_auth_memo();
        self.signals_handle()
            .record_model_usage(&sampling_config.model);
        if apply_prompt_override && !skip_prompt_rewrite {
            if self.state.lock().await.running_task.is_some() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    model_id = %model_id.0,
                    "system_prompt_label relabel skipped: turn in flight"
                );
            } else {
                self.relabel_agent_system_prompt(system_prompt_label).await;
            }
            let mut conversation = self.chat_state_handle.get_conversation().await;
            for item in conversation.iter_mut() {
                if let ConversationItem::System(sys) = item {
                    if use_concise {
                        sys.content = std::sync::Arc::<str>::from(
                            distill_agent::prompt::template::COMPACT_SYSTEM_PROMPT,
                        );
                    } else {
                        sys.content =
                            std::sync::Arc::<str>::from(self.agent.borrow().system_prompt());
                    }
                    break;
                }
            }
            self.chat_state_handle.replace_conversation(conversation);
        } else if !apply_prompt_override {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt override (apply_prompt_override=false)"
            );
        } else {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt rewrite (just rebuilt harness)"
            );
        }
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: model_id.clone(),
                agent_name: Some(agent_name),
                reasoning_effort: Some(sampling_config.reasoning_effort),
            });
        self.canonical_model_id.replace(canonical_model_id);
        if self.startup_hints.is_subagent && model_selection_intent {
            self.model_routing_locked.set(true);
        }
        self.emit_status_snapshot_detached();
        let turn_in_flight = self.state.lock().await.running_task.is_some();
        if turn_in_flight && is_family_switch {
            tracing::warn!("Family-switch compact skipped: turn in flight");
        }
        if is_family_switch && !turn_in_flight && self.history_has_model_minted_items().await {
            self.abort_and_clear_prefire().await;
            let estimated_total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
            let context_window = new_context_window.get();
            let trigger_info = compaction::AutoCompactTriggerInfo {
                tokens_used: estimated_total_tokens,
                context_window,
                percentage: distill_token_estimation::usage_percentage_u8(
                    estimated_total_tokens,
                    context_window,
                ),
                reason_override: Some(
                    crate::extensions::notification::MODEL_FAMILY_SWITCH_COMPACT_BANNER,
                ),
            };
            tracing::info!("Family-switch compact: -> {}", sampling_config.model);
            if let Err(e) = self.run_compact_only(trigger_info, true).await {
                tracing::error!(error = %e, "Family-switch compaction failed; switching anyway");
            }
        }
        Ok(model_id)
    }
    /// `running_task` is re-checked with no await before the `borrow_mut`: a running turn pins `Ref<Agent>`.
    pub(super) async fn relabel_agent_system_prompt(&self, system_prompt_label: String) {
        let (mut prompt_context, tool_bridge) = {
            let agent = self.agent.borrow();
            if agent.prompt_context().system_prompt_label == system_prompt_label {
                return;
            }
            (
                agent.prompt_context().clone(),
                std::sync::Arc::clone(agent.tool_bridge()),
            )
        };
        prompt_context.system_prompt_label = system_prompt_label.clone();
        let Some(rendered) = prompt_context.render_paired(&tool_bridge).await else {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                %system_prompt_label,
                "system prompt re-render failed; keeping the previous identity label"
            );
            return;
        };
        if self.state.lock().await.running_task.is_some() {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                "system_prompt_label relabel skipped: turn started during re-render"
            );
            return;
        }
        self.agent.borrow_mut().set_rendered_prompt(rendered);
        self.abort_and_clear_prefire().await;
        let agent = self.agent.borrow();
        let mut persisted_context = agent.prompt_context().clone();
        persisted_context.normalize_for_persistence();
        save_prompt_context(&self.session_info, &persisted_context);
        save_system_prompt(&self.session_info, agent.system_prompt());
        tracing::info!(
            session_id = %self.session_info.id.0,
            system_prompt_label = %agent.prompt_context().system_prompt_label,
            "system_prompt_label updated for model switch"
        );
    }
    /// Set the reasoning effort on the live sampling config, applying the same
    /// support check and per-effort model routing as `apply_supported_effort`.
    pub(super) async fn handle_set_reasoning_effort(
        self: &std::sync::Arc<Self>,
        effort: distill_sampling_types::ReasoningEffort,
    ) -> Result<acp::ModelId, acp::Error> {
        let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await else {
            return Err(acp::Error::internal_error().data("session has no sampling config"));
        };
        if !self
            .models_manager
            .model_supports_reasoning_effort(&cfg.model)
        {
            return Err(acp::Error::invalid_params()
                .data("the session's current model does not support reasoning effort"));
        }
        if let Some(routed) = self.models_manager.model_for_effort(&cfg.model, effort) {
            cfg.model = routed;
        }
        cfg.reasoning_effort = Some(effort);
        self.jev_effort_auto
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let model_id = acp::ModelId::new(cfg.model.clone());
        self.chat_state_handle.update_sampling_config(cfg);
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: model_id.clone(),
                agent_name: Some(agent_name),
                reasoning_effort: Some(Some(effort)),
            });
        self.emit_status_snapshot_detached();
        Ok(model_id)
    }
    /// Handle [`SessionCommand::RebuildAgentForDefinition`].
    /// Builds a fresh [`distill_agent::Agent`] from the cached [`crate::session::agent_rebuild::AgentRebuildSpec`] and the supplied definition.
    /// Triggered from `MvpAgent::set_session_model` only when the new model's `agent_type` differs from the session's `active_agent_type`.
    pub(super) async fn handle_rebuild_agent_for_definition(
        &self,
        definition: distill_agent::AgentDefinition,
        system_prompt_label: String,
    ) -> Result<(), acp::Error> {
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %definition.name,
                    "handle_rebuild_agent_for_definition: turn in flight, rejecting rebuild"
                );
                return Err(acp::Error::internal_error()
                    .data("rebuild_agent: turn in flight, refusing to rebuild harness"));
            }
        }
        let new_agent_name = definition.name.clone();
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: rebuilding harness"
        );
        let new_agent = self
            .rebuild_spec
            .build_agent(definition, system_prompt_label)
            .await
            .map_err(|e| {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %new_agent_name,
                    error = %e,
                    "handle_rebuild_agent_for_definition: AgentBuilder::build failed"
                );
                acp::Error::internal_error().data(format!(
                    "rebuild_agent: build failed for agent_type={new_agent_name}: {e}"
                ))
            })?;
        let new_system_prompt = new_agent.system_prompt().to_string();
        let mut new_prompt_context = new_agent.prompt_context().clone();
        new_prompt_context.normalize_for_persistence();
        self.abort_and_clear_prefire().await;
        *self.agent.borrow_mut() = new_agent;
        *self.active_agent_type.lock() = Some(new_agent_name.clone());
        self.emit_resolved_tool_overrides();
        self.queue_exit_reminder_on_approved_exit.store(
            self.is_cursor_harness(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Err(e) = self.workspace_ops.bind_local_session(
            &self.session_id_string(),
            self.tool_context.cwd.as_path().to_path_buf(),
            self.tool_context.hunk_tracker_handle.clone(),
            self.agent.borrow().tool_bridge().toolset(),
            None,
        ) {
            tracing::warn!(error = %e, "failed to rebind local session toolset after agent rebuild");
        }
        {
            let bridge = self.agent.borrow().tool_bridge().clone();
            let snapshot = self.tool_metadata_snapshot.clone();
            let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
            bridge
                .update_resource(distill_tools::types::tool_index::ToolIndex(
                    std::sync::Arc::new(tool_index),
                ))
                .await;
            if let Some(client) = self.rebuild_spec.managed_gateway_tool_client.clone() {
                bridge.update_resource(client).await;
            }
            let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
            bridge
                .update_resource(distill_tools::types::resources::PlanFilePath(plan_path))
                .await;
            if let Some(display_cwd) = self.display_cwd.get() {
                bridge
                    .set_display_cwd(std::path::PathBuf::from(display_cwd))
                    .await;
            }
            bridge
                .update_resource(
                    distill_tools::implementations::distill::workflow::WorkflowLaunchHandle(
                        self.workflow_launch_tx.clone(),
                    ),
                )
                .await;
            if !self.goal_runs_on_workflow_engine() {
                bridge
                    .update_resource(
                        distill_tools::implementations::distill::update_goal::GoalUpdateHandle(
                            self.goal_update_tx.clone(),
                        ),
                    )
                    .await;
            }
            if let Some(reservations) = self.tool_context.task_completion_reservations.clone() {
                bridge.update_resource(reservations).await;
            }
            if let Some(gate) = self.tool_context.task_wake_suppressed.clone() {
                bridge.update_resource(gate).await;
            }
            self.inject_deny_read_globs().await;
        }
        let claim = self.restart_mcp_init(&mut *self.mcp_state.lock().await);
        self.re_register_mcp_tools_on_rebuilt_bridge().await;
        self.run_mcp_init_with_claim(claim).await;
        self.deferred_prefix.cancel();
        let new_user_prefix = self
            .build_prefix_after_mcp_wait(self.requires_full_mcp_wait())
            .await;
        {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            let _ = replace_or_insert_system_head(&mut conversation, &new_system_prompt);
            let drop_startup_skill_reminder = false;
            Self::rewrite_zero_turn_prefix(
                &mut conversation,
                new_user_prefix,
                drop_startup_skill_reminder,
            );
            if !conversation_has_project_instructions(&conversation)
                && let Some(agents_md_reminder) = self.agent.borrow().agents_md_user_reminder()
            {
                let agents_md_at = conversation.len().min(2);
                conversation.insert(
                    agents_md_at,
                    ConversationItem::project_instructions(agents_md_reminder),
                );
            }
            self.inject_baseline_skill_reminder(&mut conversation).await;
            self.chat_state_handle.replace_conversation(conversation);
        }
        save_prompt_context(&self.session_info, &new_prompt_context);
        save_system_prompt(&self.session_info, &new_system_prompt);
        let snapshot = self.chat_state_handle.get_conversation().await;
        persist_chat_history_jsonl_sync(&self.session_info, &snapshot);
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.send_available_commands_update(AdvertiseTrigger::HarnessRebuild)
            .await;
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: harness rebuild complete"
        );
        Ok(())
    }
    /// Apply a client-supplied `systemPromptOverride` on attach without wiping user/assistant history: swap only the leading `System` message.
    /// The swap happens atomically inside the `ChatStateActor`.
    /// `system_prompt.txt` (not owned by the persistence actor) is saved directly, even on a head no-op, so a diverged secondary artifact self-heals.
    pub(super) async fn handle_replace_system_prompt(&self, system_prompt: String) {
        if self.startup_hints.preserve_inherited_system {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: skipped (preserve_inherited_system)"
            );
            return;
        }
        let Some(changed) = self
            .chat_state_handle
            .replace_system_head(&system_prompt)
            .await
        else {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: chat-state actor unavailable; override not applied"
            );
            return;
        };
        save_system_prompt(&self.session_info, &system_prompt);
        if changed {
            tracing::info!(
                session_id = %self.session_info.id.0,
                prompt_len = system_prompt.len(),
                "handle_replace_system_prompt: client override applied"
            );
        } else {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: head already matches, no-op"
            );
        }
    }
    /// Whether the conversation has anything a family switch must compact away.
    async fn history_has_model_minted_items(&self) -> bool {
        self.chat_state_handle
            .get_conversation()
            .await
            .iter()
            .any(|item| {
                matches!(
                    item,
                    distill_sampling_types::ConversationItem::Assistant(_)
                        | distill_sampling_types::ConversationItem::Reasoning(_)
                        | distill_sampling_types::ConversationItem::BackendToolCall(_)
                )
            })
    }
    /// Abort and join an in-flight prefire pass-1 and drop its NOTE1 cache.
    pub(super) async fn abort_and_clear_prefire(&self) {
        if let Some(handle) = self.compaction.prefire.take_handle() {
            handle.abort();
            let _ = handle.await;
            self.compaction.prefire.finish();
        }
        self.compaction.prefire.clear();
    }
}
