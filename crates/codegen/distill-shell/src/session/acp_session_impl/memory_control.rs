// Modified for Distill by Samuel Fajreldines, 2026.
//! Memory listing, on/off toggling, and the manual flush and Dream commands behind the
//! `x.ai/memory/{list,toggle,flush,dream}` extension methods.

use std::sync::Arc;

use distill_telemetry::memory_telemetry::MemoryV2FailureClass;

use crate::config::MemoryMode;
use crate::extensions::memory::{
    MemoryDreamDisposition, MemoryDreamResponse, MemoryFlushDisposition, MemoryFlushResponse,
    MemoryListing, MemoryToggleResponse,
};
use crate::extensions::notification::{MemoryDisabledReason, MemoryFileInfo};
use crate::session::acp_session::SessionActor;
use crate::session::memory::MemoryStorage;
use distill_tools::types::memory_v2::MemoryV2AccessResource;

/// Outcome of rendering the system prompt for a memory-state change.
enum MemoryPromptRender {
    /// The agent's prompt already matches; nothing to install.
    Current,
    Rendered(Box<distill_agent::RenderedPrompt>),
    /// Template rendering failed; the previous prompt stays and the swap is retried later.
    Failed,
}

/// Outcome of bringing the system prompt in line with the live memory state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MemoryPromptSync {
    /// Installed now (or already current).
    Applied,
    /// A turn pins the agent; retried before the next turn is promoted.
    DeferredForTurn,
    /// Rendering failed; retried before the next turn is promoted.
    RenderFailed,
}

/// A memory-v2 bring-up stage that failed. Memory stays off; the user can restart to retry.
pub(crate) struct V2MemoryInitFailure {
    pub stage: &'static str,
    pub error: String,
}

/// Bring memory v2 up for a session: storage layout, tombstone reconciliation, the file-access
/// policy that ordinary file tools route memory paths through, and (optionally) the one-time
/// carry-over of curated legacy `MEMORY.md` files.
///
/// Spawn and the `/memory` toggle both call this so that enabling memory mid-session installs
/// exactly what a session that started with memory on gets. Every step is idempotent.
pub(crate) async fn initialize_v2_memory(
    storage: &MemoryStorage,
    carry_over_legacy: bool,
) -> Result<MemoryV2AccessResource, V2MemoryInitFailure> {
    crate::session::memory_state::initialize_memory_storage(storage.clone())
        .await
        .map_err(|error| V2MemoryInitFailure {
            stage: "storage initialization",
            error: error.to_string(),
        })?;
    // SQLite migration/reconciliation and root canonicalization: keep them off the actor's LocalSet.
    let blocking_storage = storage.clone();
    let policy = tokio::task::spawn_blocking(move || {
        let storage = blocking_storage;
        let clock = distill_memory::system_v2_clock();
        for (scope_dir, scope) in [
            (storage.global_dir(), distill_memory::V2MemoryScope::Global),
            (
                storage.workspace_dir(),
                distill_memory::V2MemoryScope::Workspace,
            ),
        ] {
            distill_memory::V2MaintenanceStore::open_with_clock(
                scope_dir,
                scope,
                storage.global_dir(),
                storage.workspace_dir(),
                clock.clone(),
            )
            .map_err(|error| V2MemoryInitFailure {
                stage: "tombstone reconciliation",
                error: error.to_string(),
            })?;
        }
        crate::session::memory::V2MemoryAccessPolicy::new(
            storage.global_dir(),
            storage.workspace_dir(),
        )
        .map_err(|error| V2MemoryInitFailure {
            stage: "file access policy initialization",
            error: error.to_string(),
        })
    })
    .await
    .map_err(|error| V2MemoryInitFailure {
        stage: "initialization task",
        error: error.to_string(),
    })??;
    let policy = Arc::new(policy);
    if carry_over_legacy {
        super::memory_carryover::carry_over_legacy_memory_into_v2(
            storage,
            &distill_memory::default_legacy_memory_root(),
            policy.clone(),
        )
        .await;
    }
    Ok(MemoryV2AccessResource(policy))
}

impl SessionActor {
    /// Files the `/memory` modal shows plus the enabled/capability flags that drive its notices.
    /// Errors when the store cannot be listed; callers must not render that as an empty store.
    pub(crate) fn memory_listing(&self) -> Result<MemoryListing, String> {
        let disabled_reason = self.memory.disabled_reason();
        // Disabled sessions list the configured store: the notes still exist on disk, and the
        // modal hides them behind the disabled notice until `t` turns memory back on.
        let live = self.memory.storage.borrow();
        let files = match live.as_ref().or(self.memory.configured_storage.as_ref()) {
            Some(storage) => {
                let paths = storage.list_memory_files().map_err(|e| {
                    tracing::warn!(
                        session_id = %self.session_info.id.0,
                        error = %e,
                        "failed to list memory files",
                    );
                    format!("Failed to list memory files: {e}")
                })?;
                paths
                    .into_iter()
                    .map(|path| {
                        let meta = match std::fs::metadata(&path) {
                            Ok(m) => Some(m),
                            Err(e) => {
                                tracing::debug!(
                                    path = %path.display(),
                                    error = %e,
                                    "skipping memory file with unreadable metadata",
                                );
                                None
                            }
                        };
                        let generated = storage.mode().is_v2()
                            && (path == storage.global_memory_file()
                                || path == storage.workspace_memory_file());
                        MemoryFileInfo {
                            source: storage.classify_source(&path).to_string(),
                            generated,
                            title: observation_title(&path),
                            path: path.display().to_string(),
                            size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                            modified_epoch_secs: meta
                                .and_then(|m| m.modified().ok())
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs()),
                        }
                    })
                    .collect()
            }
            None => vec![],
        };
        drop(live);
        // Legacy saves at compaction/session end when `save_on_end`; its `/dream` is always available.
        let v2 = &self.memory.v2_config;
        let (capture_enabled, dream_enabled) = if self.memory.mode() == Some(MemoryMode::V2) {
            (v2.capture_enabled, v2.manual_dream_enabled)
        } else {
            (self.memory.save_on_end, true)
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            file_count = files.len(),
            enabled = disabled_reason.is_none(),
            ?disabled_reason,
            "memory browse: listing files",
        );
        Ok(MemoryListing {
            files,
            enabled: disabled_reason.is_none(),
            disabled_reason,
            capture_enabled,
            dream_enabled,
        })
    }

    /// `x.ai/memory/toggle`: toggle, then attach a fresh listing so an open modal can resync.
    /// The toggle has already happened by the time listing runs, so a listing failure must not
    /// fail the reply; the modal keeps its rows and shows the toggle message.
    pub(crate) async fn memory_toggle_and_list(
        self: &Arc<Self>,
        enabled: bool,
    ) -> MemoryToggleResponse {
        let message = self.memory_toggle(enabled).await;
        let disabled_reason = self.memory.disabled_reason();
        MemoryToggleResponse {
            message,
            enabled: disabled_reason.is_none(),
            disabled_reason,
            listing: self.memory_listing().ok(),
        }
    }

    /// Turn memory on or off for this session and return the message for the user.
    pub(crate) async fn memory_toggle(self: &Arc<Self>, enabled: bool) -> String {
        tracing::info!(
            session_id = %self.session_info.id.0,
            enabled,
            "memory toggle",
        );
        let msg = if enabled && !self.memory.is_enabled() {
            self.enable_memory_for_session().await
        } else if !enabled && self.memory.is_enabled() {
            self.disable_memory_for_session().await
        } else {
            let state = if enabled { "enabled" } else { "disabled" };
            format!("Memory is already {state}.")
        };
        self.refresh_goal_harness_enabled().await;
        msg
    }

    async fn enable_memory_for_session(self: &Arc<Self>) -> String {
        let storage = match (
            self.memory.disabled_reason(),
            self.memory.configured_storage.clone(),
        ) {
            (Some(MemoryDisabledReason::ProcessDisabled), _) => {
                return "Memory cannot be enabled: it was turned off for this process \
                        (`--no-memory` or `GROK_MEMORY=0`). Start a new session without it."
                    .to_owned();
            }
            (Some(MemoryDisabledReason::RolloutRestricted), _) => {
                return "Memory cannot be enabled because this session's pinned rollout controls disable it."
                    .to_owned();
            }
            (
                Some(MemoryDisabledReason::SessionToggle | MemoryDisabledReason::ConfigOptOut),
                Some(storage),
            ) => storage,
            _ => return "Memory cannot be enabled (not configured for this session).".to_owned(),
        };
        // The toggle is session-scoped; say so when the persistent default still says off.
        let enabled_message = if self.memory.config_opt_out {
            "Memory enabled for this session. `[memory] enabled = false` in config.toml still \
             applies to new sessions; edit it to make this permanent."
        } else {
            "Memory enabled for this session."
        };
        if self.memory.mode() == Some(MemoryMode::V2) {
            return self.enable_v2_memory(storage, enabled_message).await;
        }
        if let Err(e) =
            crate::session::memory_state::initialize_memory_storage(storage.clone()).await
        {
            tracing::warn!(error = %e, "failed to initialize memory storage on re-enable");
            return format!("Memory could not be enabled: {e}");
        }
        let Some(ref params) = self.memory.backend_params else {
            return "Memory cannot be enabled (legacy backend not configured for this session)."
                .to_owned();
        };
        let backend =
            crate::session::memory::MemoryBackendImpl::from_session_params(storage.clone(), params);
        *self.memory.search_counter.borrow_mut() = Some(backend.search_counter.clone());
        let backend: Arc<dyn distill_tools::types::memory_backend::MemoryBackend> =
            Arc::new(backend);
        let bridge = self.agent.borrow().tool_bridge().clone();
        bridge.update_resource(backend.clone()).await;
        if let Err(e) = self.register_memory_tools(&bridge).await {
            tracing::warn!(error = %e, "memory tool registration failed during toggle");
        }
        *self.memory.storage.borrow_mut() = Some(storage);
        enabled_message.to_owned()
    }

    /// Same bring-up as a spawn with memory on: storage, file-access policy on the tool bridge and
    /// in the rebuild spec, the `<memory>` prompt section, first-turn manifest injection, capture.
    async fn enable_v2_memory(
        self: &Arc<Self>,
        storage: MemoryStorage,
        enabled_message: &str,
    ) -> String {
        // Store bring-up is idempotent and touches nothing the session reads, so it runs before the
        // turn check; the agent install below is what must not interleave with a turn.
        let access = match initialize_v2_memory(&storage, self.memory.v2_legacy_carryover).await {
            Ok(access) => access,
            Err(failure) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    stage = failure.stage,
                    error = %failure.error,
                    "MEMORY_INIT: memory-v2 {} failed on /memory on; memory stays disabled",
                    failure.stage,
                );
                return format!(
                    "Memory could not be enabled: {} failed: {}",
                    failure.stage, failure.error
                );
            }
        };
        // Render before touching session state; the paths come from the policy, not the agent.
        let rendered = self.render_memory_prompt(Some(&access)).await;
        let render_failed = matches!(rendered, MemoryPromptRender::Failed);
        // Installed before the turn check and undone on refusal. The policy only constrains file
        // tools (containment, protected paths, manifest refresh); memory paths are plain files
        // without it, so a turn that overlaps this window is safer with the policy than without.
        let bridge = self.agent.borrow().tool_bridge().clone();
        bridge.update_resource(access.clone()).await;
        {
            // Sync-only section: a running turn pins `Ref<Agent>`, and `state` must not be held
            // across an await.
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                drop(state);
                bridge
                    .update_resources_with(|resources| {
                        resources.remove::<MemoryV2AccessResource>();
                    })
                    .await;
                return "Memory cannot be enabled while a turn is running; try again when it finishes."
                    .to_owned();
            }
            self.rebuild_spec.memory_v2_access.set(Some(access));
            *self.memory.storage.borrow_mut() = Some(storage);
            // The first-turn latch may have been set while memory was off; a persisted manifest
            // block is still reused, so this never double-injects.
            self.memory
                .context_injected
                .store(false, std::sync::atomic::Ordering::Relaxed);
            match rendered {
                MemoryPromptRender::Rendered(rendered) => {
                    self.agent.borrow_mut().set_rendered_prompt(*rendered);
                    self.memory
                        .prompt_sync_pending
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                MemoryPromptRender::Current => {
                    self.memory
                        .prompt_sync_pending
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                MemoryPromptRender::Failed => {
                    self.memory
                        .prompt_sync_pending
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
        if render_failed {
            // Memory itself is on (tools, policy, capture); only the instructions lag.
            self.resume_v2_capture().await;
            return format!(
                "{enabled_message} The system prompt could not be re-rendered; its memory \
                 instructions are retried before the next turn."
            );
        }
        self.publish_agent_prompt().await;
        self.resume_v2_capture().await;
        enabled_message.to_owned()
    }

    async fn disable_memory_for_session(self: &Arc<Self>) -> String {
        let is_v2 = self.memory.mode() == Some(MemoryMode::V2);
        if is_v2 {
            self.memory.stop_capture_worker().await;
            self.memory.dream_workers.cancel_and_join().await;
        }
        let bridge = self.agent.borrow().tool_bridge().clone();
        if !bridge.unregister_tool_by_name(
            distill_tools::implementations::memory::MEMORY_SEARCH_TOOL_NAME,
        ) {
            tracing::debug!("memory_search tool was not registered during unregister");
        }
        if !bridge
            .unregister_tool_by_name(distill_tools::implementations::memory::MEMORY_GET_TOOL_NAME)
        {
            tracing::debug!("memory_get tool was not registered during unregister");
        }
        *self.memory.storage.borrow_mut() = None;
        *self.memory.search_counter.borrow_mut() = None;
        if is_v2 {
            self.rebuild_spec.memory_v2_access.set(None);
            bridge
                .update_resources_with(|resources| {
                    resources.remove::<MemoryV2AccessResource>();
                })
                .await;
            match self.sync_v2_memory_prompt().await {
                MemoryPromptSync::Applied => {}
                MemoryPromptSync::DeferredForTurn => {
                    return "Memory disabled for this session. The system prompt drops its memory \
                            instructions when the current turn finishes."
                        .to_owned();
                }
                MemoryPromptSync::RenderFailed => {
                    return "Memory disabled for this session. The system prompt could not be \
                            re-rendered; dropping its memory instructions is retried before the \
                            next turn."
                        .to_owned();
                }
            }
        }
        "Memory disabled for this session.".to_owned()
    }

    /// Render the system prompt with its `<memory>` section matching `access` (present and
    /// exposed => on). Takes no session locks; the paths come from the policy's scope roots.
    async fn render_memory_prompt(
        &self,
        access: Option<&MemoryV2AccessResource>,
    ) -> MemoryPromptRender {
        let memory_v2_enabled = self.memory.v2_config.can_expose_memory() && access.is_some();
        let (mut prompt_context, tool_bridge) = {
            let agent = self.agent.borrow();
            (
                agent.prompt_context().clone(),
                Arc::clone(agent.tool_bridge()),
            )
        };
        let scope_paths = access.map(|access| {
            access
                .0
                .scope_roots()
                .map(|root| root.to_string_lossy().into_owned())
        });
        let paths_current = scope_paths.as_ref().is_none_or(|[global, workspace]| {
            prompt_context.memory_global_path.as_deref() == Some(global.as_str())
                && prompt_context.memory_workspace_path.as_deref() == Some(workspace.as_str())
        });
        if prompt_context.memory_v2_enabled == memory_v2_enabled && paths_current {
            return MemoryPromptRender::Current;
        }
        prompt_context.memory_v2_enabled = memory_v2_enabled;
        if let Some([global, workspace]) = scope_paths {
            prompt_context.memory_global_path = Some(global);
            prompt_context.memory_workspace_path = Some(workspace);
        }
        match prompt_context.render_paired(&tool_bridge).await {
            Some(rendered) => MemoryPromptRender::Rendered(Box::new(rendered)),
            None => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    memory_v2_enabled,
                    "system prompt re-render failed; keeping the previous memory section"
                );
                MemoryPromptRender::Failed
            }
        }
    }

    /// Bring the system prompt's `<memory>` section in line with the live v2 state now, or leave
    /// it pending for the next turn promotion when a turn is running (the turn pins `Ref<Agent>`)
    /// or the render failed.
    ///
    /// `state` is held only around the synchronous agent swap, never across an await.
    pub(super) async fn sync_v2_memory_prompt(&self) -> MemoryPromptSync {
        let access = self.rebuild_spec.memory_v2_access.get();
        let rendered = match self.render_memory_prompt(access.as_ref()).await {
            MemoryPromptRender::Rendered(rendered) => rendered,
            MemoryPromptRender::Current => {
                self.memory
                    .prompt_sync_pending
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                return MemoryPromptSync::Applied;
            }
            MemoryPromptRender::Failed => {
                self.memory
                    .prompt_sync_pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return MemoryPromptSync::RenderFailed;
            }
        };
        {
            let state = self.state.lock().await;
            if state.running_task.is_some() {
                self.memory
                    .prompt_sync_pending
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    session_id = %self.session_info.id.0,
                    "memory prompt sync deferred: turn in flight"
                );
                return MemoryPromptSync::DeferredForTurn;
            }
            self.agent.borrow_mut().set_rendered_prompt(*rendered);
            self.memory
                .prompt_sync_pending
                .store(false, std::sync::atomic::Ordering::Relaxed);
        }
        self.publish_agent_prompt().await;
        MemoryPromptSync::Applied
    }

    /// After a prompt swap on the agent: drop the stale prefire cache, persist the prompt
    /// artifacts, and align the conversation head. The head swap is atomic in the chat-state
    /// actor and serializes with turn pushes, so it is safe even if a turn started meanwhile.
    /// An injected manifest block on the head is kept only while memory is on; turning memory
    /// off must also take the remembered notes out of context, as resume does.
    async fn publish_agent_prompt(&self) {
        self.abort_and_clear_prefire().await;
        let (system_prompt, persisted_context, memory_v2_enabled) = {
            let agent = self.agent.borrow();
            let mut persisted_context = agent.prompt_context().clone();
            persisted_context.normalize_for_persistence();
            (
                agent.system_prompt().to_string(),
                persisted_context,
                agent.prompt_context().memory_v2_enabled,
            )
        };
        super::save_prompt_context(&self.session_info, &persisted_context);
        super::save_system_prompt(&self.session_info, &system_prompt);

        let conversation = self.chat_state_handle.get_conversation().await;
        let manifest_block = match conversation.first() {
            Some(distill_sampling_types::ConversationItem::System(sys)) if memory_v2_enabled => sys
                .content
                .find(distill_chat_state::MEMORY_CONTEXT_OPEN_TAG)
                .and_then(|start| sys.content.get(start..))
                .map(str::to_owned),
            _ => None,
        };
        let head = match manifest_block {
            Some(block) => format!("{}\n\n{block}", system_prompt.trim_end_matches('\n')),
            None => system_prompt,
        };
        self.chat_state_handle.replace_system_head(&head).await;
        tracing::info!(
            session_id = %self.session_info.id.0,
            memory_v2_enabled,
            "system prompt memory section updated for /memory toggle"
        );
    }

    /// `x.ai/memory/flush`: capture every completed turn now and wait for it to land.
    pub(crate) async fn memory_flush_command(self: &Arc<Self>) -> MemoryFlushResponse {
        use crate::session::memory::v2_capture::FlushResult;
        if !self.memory.is_enabled() {
            return MemoryFlushResponse {
                flushed: false,
                disposition: MemoryFlushDisposition::Disabled,
                through_turn: None,
            };
        }
        if self.memory.mode() != Some(MemoryMode::V2) {
            let flushed = self.run_memory_flush("user_requested", None).await;
            return MemoryFlushResponse {
                flushed,
                disposition: if flushed {
                    MemoryFlushDisposition::Flushed
                } else {
                    MemoryFlushDisposition::Busy
                },
                through_turn: None,
            };
        }
        let (result, through_turn) = self.flush_v2_capture().await;
        let disposition = match result {
            FlushResult::Success => MemoryFlushDisposition::Flushed,
            FlushResult::RetryableFailure(_) => MemoryFlushDisposition::RetryRequired,
            FlushResult::TerminalFailure(MemoryV2FailureClass::Disabled) => {
                MemoryFlushDisposition::Disabled
            }
            FlushResult::TerminalFailure(_) => MemoryFlushDisposition::Failed,
            FlushResult::Timeout => MemoryFlushDisposition::TimedOut,
        };
        MemoryFlushResponse {
            flushed: disposition == MemoryFlushDisposition::Flushed,
            disposition,
            through_turn,
        }
    }

    /// `x.ai/memory/dream`: consolidate now, bypassing the automatic Dream gates.
    pub(crate) async fn memory_dream_command(self: &Arc<Self>) -> MemoryDreamResponse {
        if !self.memory.is_enabled() {
            return MemoryDreamResponse::new(MemoryDreamDisposition::Disabled);
        }
        self.run_dream_slash_command().await
    }
}

/// The `topic_hint` from a v2 inbox observation's front matter. Inbox filenames are storage keys
/// (`<session>__t<range>__n<ordinal>.md`), so the modal needs this to label them.
fn observation_title(path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let parent = path.parent()?;
    if parent.file_name()? != "_inbox" || parent.parent()?.file_name()? != "observations" {
        return None;
    }
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4096)
        .read_to_end(&mut head)
        .ok()?;
    // The cut may split a multi-byte character; keep the valid prefix.
    let head = match std::str::from_utf8(&head) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(head.get(..e.valid_up_to())?).ok()?,
    };
    let body = head.strip_prefix("---\n")?;
    let front_matter = body.split("\n---").next()?;
    let value = front_matter
        .lines()
        .find_map(|line| line.strip_prefix("topic_hint: "))?;
    // Written as a JSON string (or `null`) by the capture renderer.
    serde_json::from_str::<String>(value.trim())
        .ok()
        .filter(|title| !title.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::observation_title;

    #[test]
    fn observation_title_reads_topic_hint_only_for_inbox_files() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("observations/_inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let note = inbox.join("s__t000001-000001__n000.md");
        std::fs::write(
            &note,
            "---\nschema_version: 2\ntype: project\ntopic_hint: \"Bazel \\\"gazelle\\\" rules\"\nkeywords: []\n---\n\n# body\n",
        )
        .unwrap();
        assert_eq!(
            observation_title(&note).as_deref(),
            Some("Bazel \"gazelle\" rules")
        );

        // Body pushes a multi-byte character across the 4 KiB read boundary.
        let split = inbox.join("s__t000003-000003__n000.md");
        let mut long = String::from("---\ntopic_hint: \"long\"\n---\n");
        long.push_str(&"a".repeat(4096 - long.len() - 1));
        long.push_str("é and more");
        std::fs::write(&split, long).unwrap();
        assert_eq!(observation_title(&split).as_deref(), Some("long"));

        let untitled = inbox.join("s__t000002-000002__n000.md");
        std::fs::write(&untitled, "---\ntopic_hint: null\n---\n# body\n").unwrap();
        assert_eq!(observation_title(&untitled), None);

        let topic = dir.path().join("topics/rust.md");
        std::fs::create_dir_all(topic.parent().unwrap()).unwrap();
        std::fs::write(&topic, "---\ntopic_hint: \"nope\"\n---\n").unwrap();
        assert_eq!(observation_title(&topic), None);
    }
}
