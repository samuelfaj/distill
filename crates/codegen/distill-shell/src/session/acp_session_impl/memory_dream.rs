// Modified for Distill by Samuel Fajreldines, 2026.
//! Memory concern for `SessionActor`: memory flush, the dream pipeline, memory tool registration, and note rewriting.

use super::*;
use distill_telemetry::session_end::{self, Phase};

const DREAM_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// Stale-lock floor: the whole dream (model call plus post-call reindex) must finish inside this, so it must exceed the model timeout; doubling it leaves reindex headroom.
const DREAM_LOCK_STALE_FLOOR_SECS: u64 = DREAM_MODEL_TIMEOUT.as_secs() * 2;

/// Whether a dream attempt reached the model call. `Ran` reports its own result; `Skipped` returned
/// before the model call, so a user-initiated caller surfaces the reason and `/dream` is never silent.
enum DreamAttempt {
    Ran(MemoryDreamDisposition),
    Skipped(MemoryDreamDisposition, &'static str),
}

#[derive(Debug)]
pub(super) struct MemoryFlushSnapshot {
    counts: distill_chat_state::ConversationCounts,
    chat_history: Vec<ConversationItem>,
}

/// Build first-turn injection backend params without mutating the shared session params.
/// Clones the session-wide params so tool-search and compaction-recovery backends keep their original `search_source` and search thresholds.
/// The returned effective min score keeps the historical first-turn default of `0.0` unless the injection config explicitly overrides it.
pub(super) fn build_initial_injection_backend_params(
    params: &crate::session::memory::MemoryBackendParams,
    initial_injection_config: &crate::config::MemoryInitialInjectionConfig,
) -> (crate::session::memory::MemoryBackendParams, f64) {
    let mut injection_params = params.clone();
    injection_params.search_source = crate::session::memory::MemorySearchSource::Injection;
    let effective_min_score = initial_injection_config
        .min_score
        .map(|min_score| {
            injection_params.search_config.min_score = min_score;
            min_score as f64
        })
        .unwrap_or(0.0);
    (injection_params, effective_min_score)
}

impl SessionActor {
    /// Re-register `memory_search` and `memory_get` tools on the tool bridge.
    /// The dynamic `register_mcp_tools` path puts the tools in the `LocalRegistry` for dispatch.
    /// The memory backend itself is already in `Resources`, inserted by the caller before this method.
    pub(super) async fn register_memory_tools(
        &self,
        bridge: &distill_tools::bridge::ToolBridge,
    ) -> Result<(), String> {
        use distill_tools::implementations::memory::{
            MEMORY_GET_TOOL_NAME, MEMORY_SEARCH_TOOL_NAME,
        };

        bridge
            .register_mcp_tools(
                MEMORY_SEARCH_TOOL_NAME.to_owned(),
                distill_tools::implementations::memory::search_tool::MemorySearchImpl,
                None,
            )
            .map_err(|e| format!("failed to register memory_search: {e}"))?;
        bridge
            .register_mcp_tools(
                MEMORY_GET_TOOL_NAME.to_owned(),
                distill_tools::implementations::memory::get_tool::MemoryGetImpl,
                None,
            )
            .map_err(|e| format!("failed to register memory_get: {e}"))?;
        Ok(())
    }

    pub(super) fn emit_memory_session_summary(
        &self,
        telem: &super::memory_state::MemoryTelemetry,
        total_chunks_at_end: usize,
        session_end_result: &str,
    ) {
        distill_telemetry::session_ctx::log_event(
            distill_telemetry::memory_telemetry::MemorySessionSummary {
                session_id: self.session_info.id.to_string(),
                memory_enabled: self.memory.is_enabled(),
                memory_mode: match self.memory.mode() {
                    Some(crate::config::MemoryMode::V2) => {
                        distill_telemetry::memory_telemetry::MemoryMode::V2
                    }
                    Some(crate::config::MemoryMode::Legacy) | None => {
                        distill_telemetry::memory_telemetry::MemoryMode::Legacy
                    }
                },
                session_duration_secs: self.session_start.elapsed().as_secs(),
                flush_count: telem.flush_count,
                flush_success_count: telem.flush_success_count,
                flush_error_count: telem.flush_error_count,
                tool_search_count: telem.tool_search_count,
                injection_count: telem.injection_count,
                recovery_search_count: telem.compaction_recovery_count,
                total_chunks_at_end,
                chunks_added_this_session: telem.chunks_added as usize,
                session_end_result: session_end_result.to_owned(),
                dream_count: telem.dream_count,
                dream_success_count: telem.dream_success_count,
                dream_error_count: telem.dream_error_count,
                capture_prompt_tokens: telem.capture_prompt_tokens,
                capture_completion_tokens: telem.capture_completion_tokens,
                capture_cost_usd_ticks: telem.capture_cost_usd_ticks,
                dream_prompt_tokens: telem.dream_prompt_tokens,
                dream_completion_tokens: telem.dream_completion_tokens,
                dream_cost_usd_ticks: telem.dream_cost_usd_ticks,
                injected_bytes: telem.injected_bytes,
            },
        );
    }

    /// Session-end memory save and summary telemetry, shared by the Shutdown and channel-closed arms.
    ///
    /// `log_suffix` is appended to the `MEMORY_SESSION_END:` log line so each arm keeps a distinct reason string in logs.
    pub(super) async fn run_session_end_memory_pipeline(
        &self,
        log_suffix: &str,
        timer: &distill_telemetry::session_end::SharedSessionEndTimer,
    ) {
        let span = session_end::span(Phase::Memory);
        if self.startup_hints.is_subagent {
            tracing::debug!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_SUBAGENT_SKIP: skipping on_session_end for subagent session"
            );
            return;
        }
        let mut session_end_result = "disabled";
        let mut total_chunks_at_end = 0usize;
        if self.memory.uses_legacy_pipeline()
            && let Some(storage) = self.memory.storage()
        {
            let _save = session_end::timed_child(timer, Phase::MemorySave, span.span());
            let conversation = self.chat_state_handle.get_conversation().await;
            let result = crate::session::memory::hooks::on_session_end(
                &storage,
                &conversation,
                &self.session_info.id.0,
                self.memory.save_on_end,
            );
            match &result {
                crate::session::memory::hooks::SessionEndResult::Written(path_str) => {
                    session_end_result = "written";
                    self.reindex_and_embed(std::path::Path::new(path_str), "session")
                        .await;
                    self.send_xai_notification(XaiSessionUpdate::MemorySessionSaved {
                        path: path_str.clone(),
                    })
                    .await;
                }
                crate::session::memory::hooks::SessionEndResult::Skipped => {
                    session_end_result = "skipped";
                }
                crate::session::memory::hooks::SessionEndResult::Failed(_) => {
                    session_end_result = "failed";
                }
            }
            total_chunks_at_end = storage.total_chunk_count();
            let telem = self.memory.telemetry_snapshot();
            let msg = format!("MEMORY_SESSION_END: {log_suffix}");
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                result = ?result,
                tool_searches = telem.tool_search_count,
                injection_searches = telem.injection_count,
                recovery_searches = telem.compaction_recovery_count,
                "{msg}"
            );
        }
        let telem = self.memory.telemetry_snapshot();
        self.emit_memory_session_summary(&telem, total_chunks_at_end, session_end_result);
    }

    /// Reindex a single file and embed any new chunks.
    /// Used after flush and session-end writes to keep the index and embeddings current immediately.
    pub(super) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        self.memory.reindex_and_embed(path, source).await;
    }

    /// Common setup for dream methods: storage, lock, sessions dir, and truncated session id.
    fn dream_context(
        &self,
    ) -> Option<(
        crate::session::memory::MemoryStorage,
        crate::session::memory::dream_lock::DreamLock,
        std::path::PathBuf,
        String,
    )> {
        if !self.memory.uses_legacy_pipeline() {
            return None;
        }
        let storage = self.memory.storage()?;
        let workspace_dir = storage.workspace_dir();
        let lock = crate::session::memory::dream_lock::DreamLock::new(workspace_dir);
        let sessions_dir = storage.sessions_dir();
        let sid = &self.session_info.id.0;
        let sid8 = sid.get(..8.min(sid.len())).unwrap_or(sid).to_owned();
        Some((storage, lock, sessions_dir, sid8))
    }

    /// Run dream consolidation if gates pass.
    pub(super) async fn maybe_run_dream(&self) {
        if self.startup_hints.is_subagent {
            tracing::debug!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_SUBAGENT_SKIP: skipping dream for subagent session"
            );
            return;
        }

        use crate::session::memory::dream::*;

        let Some((storage, lock, sessions_dir, sid8)) = self.dream_context() else {
            return;
        };

        // Cheap pre-check to filter out the common closed-gate case before taking the lock; the
        // authoritative gate is re-checked under the lock inside run_dream_inner.
        let gate = check_dream_gates(&self.memory.dream_config, &lock, &sessions_dir, Some(&sid8));
        let sessions = match gate {
            DreamGate::Open { sessions } => sessions,
            other => {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    gate = ?other,
                    "MEMORY_DREAM: gate check result, skipping"
                );
                return;
            }
        };

        tracing::info!(
            target: distill_telemetry::memory_log::TARGET,
            session_count = sessions.len(),
            "MEMORY_DREAM: gates passed, starting consolidation"
        );

        self.run_dream_inner(
            &storage,
            &lock,
            &sessions_dir,
            &sessions,
            Some(&sid8),
            "MEMORY_DREAM",
        )
        .await;
    }

    /// Run dream from the `/dream` slash command, bypassing the time and session gates.
    pub(super) async fn run_dream_slash_command(self: &Arc<Self>) -> MemoryDreamResponse {
        if self.memory.mode() == Some(crate::config::MemoryMode::V2) {
            return self.run_v2_dream_slash_command().await;
        }
        use crate::session::memory::dream_lock::sessions_since;

        let Some((storage, lock, sessions_dir, sid8)) = self.dream_context() else {
            return MemoryDreamResponse::new(MemoryDreamDisposition::Disabled);
        };

        let sessions = match sessions_since(
            &sessions_dir,
            std::time::SystemTime::UNIX_EPOCH,
            Some(&sid8),
        ) {
            Ok(s) if s.is_empty() => {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    "MEMORY_DREAM_SLASH: no session logs found, nothing to consolidate"
                );
                return MemoryDreamResponse::new(MemoryDreamDisposition::NoWork);
            }
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    error = %e,
                    "MEMORY_DREAM_SLASH: failed to list sessions"
                );
                return MemoryDreamResponse::new(MemoryDreamDisposition::Failed);
            }
        };

        tracing::info!(
            target: distill_telemetry::memory_log::TARGET,
            session_count = sessions.len(),
            "MEMORY_DREAM_SLASH: starting manual consolidation"
        );

        // `/dream` is user-initiated, so a skip must be surfaced rather than logged silently.
        match self
            .run_dream_inner(
                &storage,
                &lock,
                &sessions_dir,
                &sessions,
                None,
                "MEMORY_DREAM_SLASH",
            )
            .await
        {
            DreamAttempt::Ran(disposition) => MemoryDreamResponse::new(disposition),
            DreamAttempt::Skipped(disposition, reason) => {
                self.send_xai_notification(XaiSessionUpdate::MemoryDreamCompleted {
                    result: format!("skipped: {reason}"),
                    path: None,
                })
                .await;
                MemoryDreamResponse::new(disposition)
            }
        }
    }

    /// Shared dream execution: acquire the lock, re-check the gate under it, build the message,.
    /// call the model, execute, and record the result.
    /// `recheck_sid8` is `Some` for the gated (auto) path: the gate is re-evaluated while holding the.
    async fn run_dream_inner(
        &self,
        storage: &crate::session::memory::MemoryStorage,
        lock: &crate::session::memory::dream_lock::DreamLock,
        sessions_dir: &std::path::Path,
        sessions: &[String],
        recheck_sid8: Option<&str>,
        log_prefix: &str,
    ) -> DreamAttempt {
        use crate::session::memory::dream::*;

        // Acquire first, with a stale window floored above the whole dream, so a live lock is
        // never reclaimed mid-run.
        let stale_lock_secs = self
            .memory
            .dream_config
            .stale_lock_secs
            .max(DREAM_LOCK_STALE_FLOOR_SECS);
        let guard = match lock.acquire(stale_lock_secs) {
            Ok(Some(g)) => g,
            Ok(None) => {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    "{log_prefix}: lock held by another process, skipping"
                );
                return DreamAttempt::Skipped(
                    MemoryDreamDisposition::Busy,
                    "another consolidation is already running",
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    error = %e,
                    "{log_prefix}: lock acquire failed"
                );
                return DreamAttempt::Skipped(
                    MemoryDreamDisposition::Failed,
                    "could not acquire the consolidation lock",
                );
            }
        };

        // Re-check the gate under the lock: the pre-check ran before we held it, so the winner may
        // have consolidated and closed the gate in the meantime.
        let rechecked_sessions;
        let sessions: &[String] = match recheck_sid8 {
            Some(sid8) => {
                match check_dream_gates(&self.memory.dream_config, lock, sessions_dir, Some(sid8)) {
                    DreamGate::Open { sessions } => {
                        rechecked_sessions = sessions;
                        &rechecked_sessions
                    }
                    other => {
                        tracing::info!(
                            target: distill_telemetry::memory_log::TARGET,
                            gate = ?other,
                            "{log_prefix}: gate closed under lock, skipping"
                        );
                        return DreamAttempt::Skipped(
                            MemoryDreamDisposition::NoWork,
                            "nothing new to consolidate",
                        );
                    }
                }
            }
            None => sessions,
        };

        let existing_memory = std::fs::read_to_string(storage.workspace_memory_file()).ok();

        let dream_msg =
            match build_dream_user_message(sessions_dir, sessions, existing_memory.as_deref()) {
                Some(msg) => msg,
                None => {
                    tracing::info!(
                        target: distill_telemetry::memory_log::TARGET,
                        "{log_prefix}: no readable session content, skipping"
                    );
                    return DreamAttempt::Skipped(
                        MemoryDreamDisposition::NoWork,
                        "no readable session content",
                    );
                }
            };

        let model_response = match tokio::time::timeout(
            DREAM_MODEL_TIMEOUT,
            self.run_dream_model_call(&dream_msg.content),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    error = %e,
                    "{log_prefix}: model call failed"
                );
                self.memory.record_dream_result(false);
                return DreamAttempt::Ran(MemoryDreamDisposition::Failed);
            }
            Err(_) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    "{log_prefix}: model call timed out (30m)"
                );
                self.memory.record_dream_result(false);
                return DreamAttempt::Ran(MemoryDreamDisposition::Failed);
            }
        };

        let result = execute_dream(storage, &model_response, sessions.len());

        // Commit for Completed and NothingToConsolidate to close the gate; a Failed guard stays
        // uncommitted so its drop releases the mutex and reopens the gate for a retry. A commit that
        // cannot durably write the marker returns false: leave the gate open and do not claim success.
        let mut cleaned_stems: Vec<String> = Vec::new();
        let dream_path = match &result.status {
            DreamStatus::Completed { .. } => {
                let path = storage.workspace_memory_file();
                self.memory.reindex_and_embed(&path, "dream").await;

                cleaned_stems = clean_processed_sessions(sessions_dir, &dream_msg.processed_stems);

                // Purge index chunks only for files actually deleted; stems skipped by the recency guard stay on disk and searchable.
                if !cleaned_stems.is_empty() {
                    let deleted_paths: Vec<std::path::PathBuf> = cleaned_stems
                        .iter()
                        .map(|stem| sessions_dir.join(format!("{stem}.md")))
                        .collect();
                    self.memory.delete_paths_from_index(&deleted_paths);
                }

                // Commit last, after reindex and cleanup, so a mid-run cancel or failure releases the
                // lock without stamping the marker. Only record success once the marker lands.
                if guard.commit() {
                    self.memory.record_dream_result(true);
                    Some(path.display().to_string())
                } else {
                    tracing::warn!(
                        target: distill_telemetry::memory_log::TARGET,
                        "{log_prefix}: consolidation marker failed to write; gate stays open to retry"
                    );
                    None
                }
            }
            DreamStatus::NothingToConsolidate => {
                if guard.commit() {
                    self.memory.record_dream_neutral();
                } else {
                    tracing::warn!(
                        target: distill_telemetry::memory_log::TARGET,
                        "{log_prefix}: consolidation marker failed to write; gate stays open to retry"
                    );
                }
                None
            }
            DreamStatus::Failed(_) => {
                self.memory.record_dream_result(false);
                None
            }
        };

        let dream_result_str = match &result.status {
            DreamStatus::Completed { chars_written } => format!("written ({chars_written} chars)"),
            DreamStatus::NothingToConsolidate => "nothing to consolidate".into(),
            DreamStatus::Failed(err) => format!("failed: {err}"),
        };
        self.send_xai_notification(XaiSessionUpdate::MemoryDreamCompleted {
            result: dream_result_str,
            path: dream_path,
        })
        .await;

        tracing::info!(
            target: distill_telemetry::memory_log::TARGET,
            status = ?result.status,
            sessions_eligible = result.sessions_eligible,
            sessions_cleaned = cleaned_stems.len(),
            "{log_prefix}: consolidation complete"
        );

        DreamAttempt::Ran(match result.status {
            DreamStatus::Completed { .. } => MemoryDreamDisposition::Completed,
            DreamStatus::NothingToConsolidate => MemoryDreamDisposition::NoWork,
            DreamStatus::Failed(_) => MemoryDreamDisposition::Failed,
        })
    }

    /// Make the dream model call using the session's sampling client.
    async fn run_dream_model_call(&self, user_message: &str) -> Result<String, acp::Error> {
        let sampling_client = self.prepare_chat_completion(false).await?;
        let model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let session_id = self.session_info.id.to_string();
        let request = ConversationRequest {
            items: vec![
                ConversationItem::system(crate::session::memory::dream::DREAM_SYSTEM_PROMPT),
                ConversationItem::user(user_message),
            ],
            model: Some(model),
            x_grok_conv_id: Some(format!("dream-{}", uuid::Uuid::new_v4())),
            x_grok_req_id: Some(format!("xai-dream-{}", uuid::Uuid::new_v4())),
            x_grok_session_id: Some(session_id),
            x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
            ..Default::default()
        };
        let response = sampling_client
            .conversation_collect(request)
            .await
            .map_err(|e| {
                acp::Error::internal_error().data(format!("dream model call failed: {e}"))
            })?;
        Ok(response.assistant_text())
    }

    /// Run a memory flush turn that summarizes recent conversation into a session log.
    /// Sets `is_flushing` to suppress auto-compact during the call.
    /// Returns `true` if a flush was executed, `false` if skipped because another flush is already in progress.
    pub(super) async fn run_memory_flush(
        &self,
        trigger: &str,
        snapshot: Option<MemoryFlushSnapshot>,
    ) -> bool {
        use distill_memory::flush::*;

        if !self.memory.uses_legacy_pipeline() {
            tracing::debug!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: legacy flush is disabled for this memory mode (trigger={trigger})"
            );
            return false;
        }

        // Atomically acquire the flushing lock. If another flush is already running (idle timer, pre-compaction, or user-requested), skip.
        if !self.memory.try_acquire_flush_lock() {
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: skipped — another flush is already in progress (trigger={trigger})"
            );
            return false;
        }

        tracing::info!(target: distill_telemetry::memory_log::TARGET, "MEMORY_FLUSH: starting");
        let flush_start = std::time::Instant::now();

        self.send_xai_notification(XaiSessionUpdate::MemoryFlushStarted)
            .await;

        let result = async {
            let sampling_client = self.prepare_chat_completion(false).await?;
            let MemoryFlushSnapshot {
                counts,
                chat_history,
            } = match snapshot {
                Some(snapshot) => snapshot,
                None => self.snapshot_memory_flush_state().await,
            };
            distill_telemetry::session_ctx::log_event(
                distill_telemetry::memory_telemetry::MemoryFlushStart {
                    session_id: self.session_info.id.to_string(),
                    trigger: trigger.to_owned(),
                    conversation_len: counts.total,
                    user_message_count: counts.user,
                },
            );
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: conversation has {user} user, {assistant} assistant, {tool} tool messages ({total} total)",
                user = counts.user,
                assistant = counts.assistant,
                tool = counts.tool_result,
                total = counts.total,
            );
            let recent = crate::session::helpers::memory_flush_window::select_flush_window(
                chat_history,
                20,
            );

            let flush_count = self.memory.flush_count.load(std::sync::atomic::Ordering::Relaxed);
            let system_prompt = if flush_count > 0 {
                if let Some(prev) = self.memory.last_flush_content.borrow().as_deref() {
                    format!("{FLUSH_DELTA_SYSTEM_PROMPT}{prev}")
                } else {
                    FLUSH_SYSTEM_PROMPT.to_owned()
                }
            } else {
                FLUSH_SYSTEM_PROMPT.to_owned()
            };
            let mut items: Vec<ConversationItem> = vec![ConversationItem::system(system_prompt)];
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: sending {n} recent messages to model (+ system prompt + user closer)",
                n = recent.len(),
            );
            items.extend(
                distill_chat_state::compaction_utils::ModelRequestHistory::from_raw(recent).into_items(),
            );
            items.push(ConversationItem::user(
                "Now write the memory summary as described in the system prompt.",
            ));

            let model = match self.memory.flush_config.flush_model.clone() {
                Some(m) => m,
                None => self.chat_state_handle.get_sampling_config().await
                    .map(|c| c.model)
                    .unwrap_or_default(),
            };
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_FLUSH: using model={model}"
            );
            let session_id = self.session_info.id.to_string();
            let request = ConversationRequest {
                items,
                model: Some(model),
                x_grok_conv_id: Some(format!("flush-{}", uuid::Uuid::new_v4())),
                x_grok_req_id: Some(format!("xai-flush-{}", uuid::Uuid::new_v4())),
                x_grok_session_id: Some(session_id.clone()),
                x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                ..Default::default()
            };

            // Run on the multi-threaded runtime so it doesn't block the session's LocalSet
            let handle = tokio::spawn(async move {
                let response = sampling_client
                    .conversation_collect(request)
                    .await
                    .map_err(|e| format!("flush model call failed: {e}"))?;
                Ok::<_, String>(response.assistant_text())
            });
            // Abort the spawned task if this future is dropped (session cancellation), preventing orphan HTTP streams
            struct AbortOnDrop(tokio::task::AbortHandle);
            impl Drop for AbortOnDrop {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _guard = AbortOnDrop(handle.abort_handle());
            handle
                .await
                .map_err(|e| {
                    acp::Error::internal_error()
                        .data(format!("flush stream task panicked: {e}"))
                })?
                .map_err(|e| acp::Error::internal_error().data(e))
        }
        .await;

        // (outcome_string, response_length, accepted_length, was_truncated, written_path)
        let (outcome, response_len, accepted_len, was_truncated, flush_path) = match result {
            Ok(response_text) => {
                let resp_len = response_text.len();
                match process_flush_response(&response_text, &self.memory.flush_config) {
                    FlushResult::NothingToStore => {
                        tracing::debug!("memory flush: nothing to store");
                        ("nothing to store".to_string(), resp_len, 0, false, None)
                    }
                    FlushResult::Accepted(content) => {
                        let acc_len = content.len();
                        let truncated = acc_len < resp_len;

                        // Semantic dedup: check if this content overlaps with existing memory chunks before writing
                        let is_sem_dup = if let Some(storage) = self.memory.storage() {
                            if let Some(index) = self.memory.open_index(&storage) {
                                let provider = if let Some(ref params) = self.memory.backend_params
                                {
                                    params.make_embedding_provider().await
                                } else {
                                    None
                                };
                                let threshold = self
                                    .memory
                                    .flush_config
                                    .semantic_dedup_threshold
                                    .unwrap_or(SEMANTIC_DEDUP_SIMILARITY_THRESHOLD);
                                is_semantically_duplicate(
                                    &content,
                                    &index,
                                    provider.as_ref().map(|p| {
                                        p as &dyn crate::session::memory::embedding::EmbeddingProvider
                                    }),
                                    threshold,
                                )
                                .await
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        if is_sem_dup {
                            tracing::info!(
                                "memory flush: semantic duplicate detected, skipping write"
                            );
                            (
                                "semantic duplicate".to_string(),
                                resp_len,
                                acc_len,
                                truncated,
                                None,
                            )
                        } else if let Some(storage) = self.memory.storage() {
                            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
                            let session_id = &self.session_info.id.0;
                            match storage
                                .write_daily_log(&date, trigger, session_id, &content, true)
                            {
                                Ok(path) => {
                                    tracing::info!("memory flush wrote session log");
                                    self.reindex_and_embed(&path, "session").await;
                                    *self.memory.last_flush_content.borrow_mut() = Some(content);
                                    (
                                        "written".to_string(),
                                        resp_len,
                                        acc_len,
                                        truncated,
                                        Some(path.display().to_string()),
                                    )
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "memory flush write failed");
                                    (
                                        format!("write failed: {e}"),
                                        resp_len,
                                        acc_len,
                                        truncated,
                                        None,
                                    )
                                }
                            }
                        } else {
                            (
                                "storage not configured".to_string(),
                                resp_len,
                                acc_len,
                                truncated,
                                None,
                            )
                        }
                    }
                    FlushResult::Rejected(reason) => {
                        tracing::warn!(reason = %reason, "memory flush response rejected");
                        (format!("rejected: {reason}"), resp_len, 0, false, None)
                    }
                }
            }
            Err(e) => {
                let detail = e
                    .data
                    .as_ref()
                    .and_then(|d| d.as_str())
                    .unwrap_or("memory flush failed");
                tracing::warn!(error = detail, "memory flush failed, skipping");
                (format!("skipped: {detail}"), 0, 0, false, None)
            }
        };

        tracing::info!(target: distill_telemetry::memory_log::TARGET, outcome = %outcome, "MEMORY_FLUSH: completed");
        let flush_outcome = if outcome.starts_with("written") {
            "written"
        } else if outcome.starts_with("nothing") {
            "nothing_to_store"
        } else if outcome.starts_with("rejected") {
            "rejected"
        } else if outcome.starts_with("semantic duplicate") {
            "nothing_to_store"
        } else {
            "error"
        };
        self.memory.record_flush_result(flush_outcome);
        distill_telemetry::session_ctx::log_event(
            distill_telemetry::memory_telemetry::MemoryFlushComplete {
                session_id: self.session_info.id.to_string(),
                trigger: trigger.to_owned(),
                outcome: flush_outcome.to_owned(),
                duration_ms: flush_start.elapsed().as_millis() as u64,
                response_length: response_len,
                accepted_length: accepted_len,
                was_truncated,
            },
        );

        let flush_trigger = match trigger {
            "slash_command" => distill_telemetry::events::MemoryFlushTrigger::SlashCommand,
            "interval" => distill_telemetry::events::MemoryFlushTrigger::Interval,
            "pre_compaction" => distill_telemetry::events::MemoryFlushTrigger::PreCompaction,
            _ => distill_telemetry::events::MemoryFlushTrigger::UserRequested,
        };
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::MemoryFlushed {
            trigger: flush_trigger,
            success: flush_outcome == "written",
            duration_ms: flush_start.elapsed().as_millis() as u64,
            response_length: response_len,
        });

        self.memory.release_flush_lock();
        self.send_xai_notification(XaiSessionUpdate::MemoryFlushCompleted {
            result: outcome,
            path: flush_path,
        })
        .await;
        true
    }

    /// Capture the flush inputs before compaction mutates conversation history.
    pub(super) async fn snapshot_memory_flush_state(&self) -> MemoryFlushSnapshot {
        let (counts, conversation) = tokio::join!(
            self.chat_state_handle.get_conversation_counts(),
            self.chat_state_handle.get_conversation(),
        );
        let chat_history =
            distill_chat_state::compaction_utils::prepare_conversation_for_summarization(
                conversation,
            );
        MemoryFlushSnapshot {
            counts,
            chat_history,
        }
    }

    /// Rewrite a raw memory note into well-structured markdown via a one-shot LLM call to `grok-4.6`.
    ///
    /// Same pattern as [`handle_ai_suggest`]: prepare a sampling client, build a system and user prompt, collect with a short idle timeout.
    pub(super) async fn handle_rewrite_memory_note(
        &self,
        raw_text: &str,
        context_summary: &str,
    ) -> Result<String, String> {
        // Upper-bound check to prevent unbounded LLM input.
        const MAX_INPUT_BYTES: usize = 32 * 1024;
        let combined_len = raw_text.len() + context_summary.len();
        if combined_len > MAX_INPUT_BYTES {
            return Err(format!(
                "memory note input too large ({combined_len} bytes, max {MAX_INPUT_BYTES})"
            ));
        }

        let sampling_client = self
            .prepare_chat_completion(false)
            .await
            .map_err(|e| format!("failed to prepare client: {e}"))?;

        let system = "You are a memory note formatter. Rewrite the user's note into \
            well-structured markdown suitable for a persistent MEMORY.md file. The note should be:\n\
            - Concise but complete\n\
            - Start with a descriptive ## heading\n\
            - Include enough context to be useful months later\n\
            - Reference specific files, decisions, or patterns when relevant\n\
            - Use bullet points for multiple items\n\
            - Do NOT include timestamps or session IDs\n\
            - Do NOT add information that is not present in the original note\n\n\
            Return ONLY the formatted markdown, no explanations.";

        let user_msg = format!(
            "Session context:\n{context_summary}\n\nRewrite this note as a memory entry:\n\n{raw_text}"
        );

        let items = vec![
            ConversationItem::system(system.to_owned()),
            ConversationItem::user(user_msg),
        ];

        let request = ConversationRequest {
            items,
            tools: vec![],
            model: Some("grok-4.6".to_owned()),
            temperature: Some(0.3),
            max_output_tokens: Some(1024),
            ..Default::default()
        };

        // Collect via the client so the LengthPolicy gate applies: a note truncated at the 1024-token cap must not persist to MEMORY.md
        match sampling_client
            .conversation_collect_with_idle_timeout(request, std::time::Duration::from_secs(15))
            .await
        {
            Ok(response) => {
                let text = response.assistant_text();
                if text.is_empty() {
                    Err("LLM returned empty response".to_string())
                } else {
                    Ok(text)
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "memory note rewrite inference failed");
                Err(format!("rewrite inference failed: {e}"))
            }
        }
    }
}
