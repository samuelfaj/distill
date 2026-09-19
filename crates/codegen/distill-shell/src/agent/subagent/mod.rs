// Modified for Distill by Samuel Fajreldines, 2026.
//! Shell child runtime adapter and presentation.
//!
//! Lifecycle state and command scheduling live in the shared `distill-tools` coordinator actor.
//! This module keeps shell-specific child-session construction, ACP presentation, persistence, and trace work.
//! The parent-side lifecycle and presentation entry points live in `spawn.rs`.
//!
//! ## Design
//!
//! - `run_shell_child()` runs one shell child behind `ChildRunner`.
//! - Pending/active/completed, waiters, deadlines, and cancellation are actor-owned.
//! - Child sessions share the parent's hunk tracker, filesystem, terminal, and env
//!   so that edits, bash commands, and file reads go through the same backends.
#![deny(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
use crate::agent::config::{resolve_credentials, sampling_config_for_model};
use crate::agent::remote_config::resolve_catalog_key;
use crate::extensions::notification::{SessionNotification, SessionUpdate};
use crate::session::{
    self, SessionCommand, SessionHandle, commands::PromptTurnResult as SubagentPromptTurnResult,
    fs_watch::FsWatchCapabilities, info::Info as SessionInfo,
};
use crate::terminal::AsyncTerminalRunner;
use crate::tools::ToolContext;
use crate::upload::trace::{
    GCS_SCHEMA_VERSION, PromptMetadata, TurnResultMetadata, local_sandbox_telemetry,
    upload_metadata, upload_session_state, upload_subagent_metadata, upload_turn_result,
};
use crate::upload::turn::{PromptTraceContext, complete_prompt_trace};
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_agent::config::{McpInheritance, ModelOverride, PermissionMode};
use distill_hunk_tracker::HunkTrackerHandle;
use distill_sampling_types::conversation::ConversationItem;
use distill_session_events::types::CancellationCategory;
use distill_subagent_resolution::ResumeSourceData;
use distill_tools::implementations::distill::monitor::types::MonitorEventBuffer;
use distill_tools::implementations::distill::task::types::*;
use distill_tools::types::tool::ToolKind;
use distill_workspace::file_system::AsyncFileSystem;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
mod attempt_runner;
mod jev_type;
mod spawn;
mod start_artifact_publication;
pub(crate) use distill_tools::implementations::distill::task::coordinator::{
    ChildRunOutput, StartedChild,
};
pub(crate) use spawn::{
    emit_subagent_notification, spawn_subagent_coordinator, subagent_coordinator_channel,
    worker_runtime,
};
mod attempt_store;
mod child_runtime;
mod handle_request;
mod prompt_turn_receipt;
mod prompt_turn_result;
mod resume_window;
pub(crate) use child_runtime::{
    ShellChildRuntime, UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT, UnpromotedResourceFate,
    await_session_thread_exit,
};
pub(crate) use handle_request::run_shell_child;
pub(crate) use prompt_turn_receipt::PromptTurnReceipt;
/// Clamp for a resolved sampling-limit override; `Semaphore::new` panics past `Semaphore::MAX_PERMITS`.
/// See [`SubagentsConfig::resolve_sampling_limit`].
/// [`SubagentsConfig::resolve_sampling_limit`]: crate::config::SubagentsConfig::resolve_sampling_limit
pub(crate) const MAX_SUBAGENT_SAMPLING_LIMIT: usize = 512;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitialContextSource {
    /// Fresh session: no inherited history.
    New,
    /// Parent history as `<background_context>` (harness-only chat-prefix fork).
    Forked,
    /// Resumed from a previously completed peer subagent.
    /// The child inherits the source's raw transcript, tool state, and model.
    /// System prompt and prompt context are freshly rendered from the current agent definition.
    Resumed,
}
/// Captured parent-side tier inputs for resolving `auto_compact_threshold_percent` once the subagent's actual model id is known.
/// Stored on [`SubagentSpawnContext`] so the resolver can run at spawn time and the per-model lookup honors the SUBAGENT's model, not the parent's.
#[derive(Debug, Clone, Default)]
pub(crate) struct AutoCompactThresholdTiers {
    /// `cfg.session.auto_compact_threshold_percent` (user global TOML).
    pub user_session: Option<u8>,
    /// Subset of `cfg.config_models` whose `auto_compact_threshold_percent` is set, keyed by the model entry's id (the table key in `[model.<id>]`).
    pub user_per_model: std::collections::HashMap<String, u8>,
    /// `cfg.remote_settings.auto_compact_threshold_percent` (GB global).
    pub remote_global: Option<u8>,
}
impl AutoCompactThresholdTiers {
    /// Slice the parent's `Config` into the four tier inputs we'll resolve against later.
    /// Only fields relevant to the auto-compact threshold are captured; the parent's `Config` is not held by reference.
    pub(crate) fn capture(cfg: &crate::agent::config::Config) -> Self {
        let user_per_model = cfg
            .config_models
            .iter()
            .filter_map(|(k, v)| v.auto_compact_threshold_percent.map(|t| (k.clone(), t)))
            .collect();
        Self {
            user_session: cfg.session.auto_compact_threshold_percent,
            user_per_model,
            remote_global: cfg
                .remote_settings
                .as_ref()
                .and_then(|r| r.auto_compact_threshold_percent),
        }
    }
}
/// Live-gateway target for a nested child's pre-reparent spawner session.
/// Address only: never `XaiSessionNotification` / spawn bookkeeping on this session.
#[derive(Clone)]
pub(crate) struct SpawnerAddressTarget {
    pub session_id: String,
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) enum SubagentSetupFailure {
    SamplingClient {
        meta_dir: std::path::PathBuf,
    },
    Persistence {
        meta_dir: std::path::PathBuf,
        persistence_dir: std::path::PathBuf,
    },
}
#[cfg(test)]
impl SubagentSetupFailure {
    fn meta_dir(&self) -> &Path {
        match self {
            Self::SamplingClient { meta_dir } | Self::Persistence { meta_dir, .. } => meta_dir,
        }
    }
    fn persistence_dir(&self) -> Option<&Path> {
        match self {
            Self::SamplingClient { .. } => None,
            Self::Persistence {
                persistence_dir, ..
            } => Some(persistence_dir),
        }
    }
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InitialAttemptBehavior {
    Normal,
    CompleteBeforeAdmission,
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RunShellChildHarnessConfig {
    meta_dir: std::path::PathBuf,
    initial_attempt_behavior: InitialAttemptBehavior,
    hold_wake_start_flush_ack: bool,
    hold_wake_abort_flush_ack: bool,
    reject_deferred_start_commit: bool,
}
#[cfg(test)]
impl RunShellChildHarnessConfig {
    fn new(meta_dir: std::path::PathBuf, initial_attempt_behavior: InitialAttemptBehavior) -> Self {
        Self {
            meta_dir,
            initial_attempt_behavior,
            hold_wake_start_flush_ack: false,
            hold_wake_abort_flush_ack: false,
            reject_deferred_start_commit: false,
        }
    }
    fn hold_wake_flush_acks(mut self) -> Self {
        self.hold_wake_start_flush_ack = true;
        self.hold_wake_abort_flush_ack = true;
        self
    }
    fn reject_deferred_start_commit(mut self) -> Self {
        self.reject_deferred_start_commit = true;
        self
    }
}
/// Everything the coordinator needs from MvpAgent to spawn a child session.
/// Avoids passing `&MvpAgent` (which would require the coordinator to know about the full agent struct).
/// Built by `MvpAgent::build_subagent_spawn_context()`.
pub(crate) struct SubagentSpawnContext {
    /// Parent's LSP runtime, inherited via ToolContext, same as fs/terminal.
    pub lsp: Option<std::sync::Arc<dyn distill_tools::implementations::lsp::LspBackend>>,
    /// Root session's process scope, inherited so the subagent's own child processes are reaped when the parent session closes. It is the root's, not an intermediate parent's.
    /// The Spawn arm of `handle_command` in distill-tools task/coordinator.rs re-parents nested Spawn requests to the root parent. Every subagent therefore resolves back to the root session.
    pub process_scope: Option<distill_tty_utils::ProcessScope>,
    /// Parent's client-registered hooks, inherited so the subagent's tool calls hit the same PreToolUse gate. Its events fire the same observe hooks over the parent's connection. Empty when the parent has none.
    /// Filled by the coordinator after the context is built (an async snapshot from the parent session actor).
    pub client_hooks: crate::extensions::hooks::ClientHooks,
    pub sampling_config: distill_sampler::SamplerConfig,
    #[cfg(test)]
    pub setup_failure: Option<SubagentSetupFailure>,
    #[cfg(test)]
    pub run_shell_child_harness: Option<RunShellChildHarnessConfig>,
    #[cfg(test)]
    pub fail_start_metadata_write: bool,
    pub managed_mcp_proxy_base_url: String,
    /// The staging auth header value propagated from the parent.
    /// Used when building subagent `SamplerConfig`s for auth-flow tracking and for `inject_url_derived_headers` in the construction helpers.
    pub alpha_test_key: Option<String>,
    pub auth_method_id: acp::AuthMethodId,
    pub model_id: acp::ModelId,
    pub auth: Option<distill_login::GrokAuth>,
    pub parent_cwd: PathBuf,
    pub parent_session_id: String,
    /// Shell-owned source used only to freeze active-message parent attribution synchronously.
    pub active_message_parent_prompt_index: Arc<std::sync::atomic::AtomicUsize>,
    /// The parent's cutoff at spawn, applied to the child's first turn. `None` if unset.
    pub inherited_tool_overrides: Option<distill_sampling_types::ToolOverrides>,
    pub yolo_mode: bool,
    pub subagent_event_tx: mpsc::UnboundedSender<SubagentEvent>,
    pub parent_depth: u32,
    pub subagents_max_depth: u32,
    pub workflow_max_concurrent_agents: usize,
    pub media_gen_batch_limits: distill_tools::media_gen_limits::MediaGenBatchLimits,
    /// Inference idle timeout (secs), resolved from the parent's model config at spawn-context creation time.
    pub inference_idle_timeout_secs: u64,
    pub parent_compaction: crate::session::CompactionPins,
    /// Lazy because the subagent may be assigned a different model from the parent (via `[subagents.models]` or `AgentDefinition.model`).
    /// Call [`Self::resolve_auto_compact_threshold_percent`] once the subagent's `effective_sampling_config.model` is known.
    pub auto_compact_threshold_tiers: AutoCompactThresholdTiers,
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// Parent's hunk-tracking gate.
    /// Inherited so a disabled parent's subagent also skips the per-event forward instead of paying it into a noop handle.
    pub hunk_tracking_enabled: bool,
    /// Parent's filesystem implementation (LocalFs or AcpSessionFs), shared so the child reads/writes the same working tree.
    pub fs: Arc<dyn AsyncFileSystem>,
    /// Parent's terminal runner, shared so bash commands run in the same terminal environment (env vars, cwd, color settings).
    pub terminal: Arc<dyn AsyncTerminalRunner>,
    /// Parent's terminal backend, shared so background tasks, monitors, and scheduled tasks survive subagent exit.
    /// When `Some`, the subagent session reuses this backend instead of creating a new `LocalTerminalBackend`.
    pub parent_terminal_backend: Option<Arc<dyn distill_tools::computer::types::TerminalBackend>>,
    /// Parent's notification handle for reparenting on subagent exit.
    /// When a subagent exits, its surviving tasks (monitors, bg commands) need their notification handles swapped to this.
    /// Events then route to the parent's notification bridge.
    pub parent_notification_handle:
        Option<distill_tools::notification::types::ToolNotificationHandle>,
    /// Parent's scheduler handle.
    /// When `Some`, the subagent reuses the parent's scheduler actor so scheduled tasks survive subagent exit.
    pub parent_scheduler_handle:
        Option<distill_tools::implementations::distill::scheduler::types::SchedulerHandle>,
    /// Parent's session environment variables (.envrc and color settings), shared so the child inherits the same env without re-loading.
    pub session_env: Arc<HashMap<String, String>>,
    /// Parent's memory config, shared so the child can access the same cross-session memory store.
    pub memory_config: Option<crate::config::MemoryConfig>,
    /// Parent's selected memory implementation, retained even when memory is disabled.
    pub memory_mode: crate::config::MemoryMode,
    pub web_search_sampling_config: Option<distill_sampler::SamplerConfig>,
    pub web_fetch_config: distill_tools::implementations::distill::web_fetch::WebFetchConfig,
    pub image_gen_config: distill_tools::implementations::distill::image_gen::ImageGenConfig,
    pub video_gen_config: distill_tools::implementations::distill::video_gen::VideoGenConfig,
    pub app_builder_deployer_config:
        distill_tools::implementations::distill::app_builder::AppBuilderDeployerConfig,
    pub write_file_enabled: bool,
    pub active_agent_messages_enabled: bool,
    /// Whether goal mode (`/goal`) is enabled.
    pub goal_enabled: bool,
    pub background_workflows_enabled: bool,
    /// Child policy for exposing `ask_user_question`.
    /// Always false; the parent session's setting must not cross the subagent boundary.
    pub ask_user_question_enabled: bool,
    /// Whether the parent session is non-interactive (headless `-p` / SDK).
    /// Copied onto the child's `StartupHints` so its prompt omits interactive guidance.
    pub parent_non_interactive: bool,
    /// Parent session command channel.
    /// Carries lifecycle notifications the parent persists (`SubagentSpawned` / `SubagentFinished`).
    /// When goal mode is on, it also carries transient `SubagentProgress` ticks the parent consumes for token accounting without persisting.
    pub parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
    /// Pre-reparent spawner session that can receive a live-only `agentAddress`.
    /// `None` when this spawn was not reparented or that session cannot be targeted.
    pub spawner_address_target: Option<SpawnerAddressTarget>,
    /// Parent session info, used to locate the parent session directory.
    pub parent_session_info: Option<SessionInfo>,
    /// Subagent roles config for role-based config layering.
    pub subagent_roles:
        std::collections::HashMap<String, distill_subagent_resolution::config::SubagentRole>,
    /// Subagent personas config for persona/SOUL layering.
    pub subagent_personas:
        std::collections::HashMap<String, distill_subagent_resolution::config::SubagentPersona>,
    /// Parent session's ChatStateHandle, used to read the actual live sampling config and credentials from the parent session actor (async).
    /// Cheap Clone (mpsc sender). `None` when the parent SessionHandle is not found.
    pub parent_chat_state: Option<distill_chat_state::ChatStateHandle>,
    /// Parent session's resolved turn limit, for subagent inheritance.
    pub parent_max_turns: Option<usize>,
    /// All available models for resolving model IDs from overrides.
    pub available_models: indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    /// Per-subagent model ID overrides from config.toml `[subagents.models]`.
    pub subagent_model_overrides: std::collections::HashMap<String, String>,
    /// Per-subagent enable/disable toggles from config.toml `[subagents.toggle]`.
    /// Omitted agents default to enabled (`true`).
    pub subagent_toggle: std::collections::HashMap<String, bool>,
    /// Whether web search is force-disabled via `--disable-web-search`.
    /// Inherited from the parent session.
    pub disable_web_search: bool,
    /// Whether the runtime turn-end TodoGate is force-enabled via `--todo-gate`.
    /// Inherited from the parent session.
    pub todo_gate: bool,
    /// Remote settings snapshot from the parent session, the remote tier for every
    /// `resolve_*` on this context. `run_shell_child` reads it again after the child
    /// spawn, so the child gets a clone, not a move.
    pub remote_settings: Option<crate::util::config::RemoteSettings>,
    /// Inherited `--laziness-debug-log <path>` from the parent session.
    /// Subagent classifier fires append to the same log file.
    /// `None` when the parent did not enable debug mode.
    pub laziness_debug_log: Option<std::path::PathBuf>,
    pub backend_tools_enabled: bool,
    /// Whether tools should respect `.gitignore` patterns.
    /// Inherited from the parent session.
    pub respect_gitignore: bool,
    /// Whether to enrich path-not-found errors with hints.
    /// Inherited from the parent session.
    pub path_not_found_hints: bool,
    /// Per-tool params resolved from the parent's config, forwarded verbatim to the child spawn.
    /// A default here silently drops the child's bash tool to the compiled 5-minute ceiling.
    pub tool_params_json: crate::session::agent_rebuild::ResolvedToolParamsJson,
    /// Plugin registry for plugin-aware agent lookup.
    pub plugin_registry: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    /// Shared models manager for etag-triggered refresh.
    pub models_manager: crate::agent::remote_config::ModelsManager,
    /// Pre-resolved file tool overrides (hashline vs standard) from the parent.
    /// `None` means use the standard (default) file tools.
    pub file_tool_overrides: Option<Vec<distill_tools::registry::types::ToolConfig>>,
    /// Parent session's agent config snapshot.
    pub agent_config: Option<crate::agent::config::Config>,
    /// GCS bucket URL for trace uploads.
    /// For proxy upload mode this is a placeholder; the actual bucket is determined by the proxy from user ACLs.
    pub gcs_bucket_url: Option<String>,
    /// GCS upload method (direct or proxy).
    pub gcs_upload_method: Option<crate::session::repo_changes::UploadMethod>,
    pub hook_registry: Option<std::sync::Arc<distill_hooks::discovery::HookRegistry>>,
    pub permission_handle: Option<distill_workspace::permission::PermissionHandle>,
    pub worktree_type: crate::util::config::WorktreeType,
    pub api_key_provider: Option<distill_tools::types::SharedApiKeyProvider>,
    pub image_description_model: String,
    pub workspace_ops: distill_workspace::WorkspaceOps,
    pub auth_manager: std::sync::Arc<distill_login::AuthManager>,
    /// The parent SessionActor's live `Auth401AttributionCallback`, captured at spawn time. Subagents inherit this so the child's `OaiCompatClient` 401 sites emit attribution under the parent's session id.
    /// It is joined with the parent's live `AuthManager`. Reading from `ctx.sampling_config.attribution_callback` would not work.
    /// The baseline `MvpAgent.sampling_config` goes through `agent/config.rs::sampling_config_for_model`, which always sets that field to `None`.
    pub attribution_callback: Option<distill_sampler::SharedAttributionCallback>,
    /// Parent session's agent name (e.g. "Distill").
    pub parent_agent_name: Option<String>,
    /// `agent_type` of the parent's current model: the harness-flavor fallback when `parent_agent_name` is not a recognized harness.
    /// For example, a custom client profile keeps its own name but runs a strict-harness model.
    /// `None` when the model is not in the catalog.
    pub parent_model_agent_type: Option<String>,
    pub allowed_subagent_types: Option<Vec<String>>,
    /// Parent's MCP server configs for resolving named references in agent mcpServers. NOTE: This is a snapshot from `SessionHandle` (populated at spawn_session_actor time).
    /// Servers added later via `UpdateMcpServers` (managed MCPs, plugin reload) will not appear here. Named references only resolve against the initial config.
    pub parent_mcp_configs: Vec<agent_client_protocol::McpServer>,
    /// Parent's managed MCP state handle (Arc-shared, no re-fetch).
    pub managed_mcp_state: crate::session::managed_mcp::ManagedMcpStateHandle,
    /// Snapshot of the parent session's MCP client pool at spawn time.
    pub parent_mcp_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    /// Exact parent tool schema for verbatim non-workflow forks.
    pub parent_tool_definitions: Option<Vec<distill_sampling_types::ToolSpec>>,
    /// Pre-discovered skills from the parent session, captured at spawn time.
    pub parent_skills: Option<Vec<distill_tools::implementations::skills::types::SkillInfo>>,
    /// Parent's skills config for the child's SkillManager.
    pub parent_skills_config: distill_agent::prompt::skills::SkillsConfig,
    /// Parent's resolved vendor-compat config, inherited by the child so its skills / rules / AGENTS.md discovery honors the same vendor toggles.
    pub parent_compat: distill_tools::types::compat::CompatConfig,
    /// Parent's `[paths]` config, inherited for the same reason as `parent_compat`.
    pub parent_paths_config: distill_agent::prompt::paths::PathsConfig,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    pub synthetic_trace_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    /// Resolved name of the `BackgroundTaskAction` tool in the parent's toolset.
    pub task_output_tool_name: String,
    /// Resolved name of the scheduled-task deletion tool in the parent's toolset.
    pub scheduler_delete_tool_name: Option<String>,
    pub scheduler_create_tool_name: Option<String>,
    /// Whether auto-wake is enabled.
    /// When `false`, subagent completions are not injected as synthetic prompts.
    pub auto_wake_enabled: bool,
    /// Parent's live goal-loop gate (shared `Arc`).
    /// When set, the subagent auto-wake synthetic prompt is suppressed so an async completion wake doesn't derail the parent mid-`/goal`.
    /// Surfaces 2/3 still drain it.
    pub goal_loop_active: Arc<std::sync::atomic::AtomicBool>,
    /// The process tree's shared subagent turn-sampling semaphore.
    /// See [`crate::config::SubagentsConfig::resolve_sampling_limit`].
    pub subagent_sampling_semaphore: Arc<tokio::sync::Semaphore>,
}
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<SubagentSpawnContext>()
};
impl SubagentSpawnContext {
    /// Would installing a live bearer resolver strip this subagent's only credential? A wired resolver is the sampler's sole auth source, so with no session key at spawn it must not displace a fallback key (env `XAI_API_KEY`).
    /// Keyed on the resolved config key, not the session cache alone. The cache is empty in exactly the post-wake / mid-refresh states the resolver targets, and gating on it would freeze the subagent for life.
    /// Shared by all three resolver-wiring paths so they cannot drift.
    fn would_strip_fallback_key(&self, resolved_api_key: Option<&str>) -> bool {
        self.auth.is_none() && resolved_api_key.is_some()
    }
    /// Resolve `auto_compact_threshold_percent` for the subagent's actual model id, picked by `resolve_subagent_sampling_config`, not the parent's.
    /// Walks the same precedence as the main session's resolver: env > user [model.<id>] > user [session] > GB per-model > GB global > 85.
    /// The GB per-model tier is read from `available_models` (the same catalog used to pick the subagent's `SamplerConfig`). User TOML and GB global tiers are sourced from the parent's snapshot captured at spawn-context build time.
    pub(crate) fn resolve_auto_compact_threshold_percent(&self, subagent_model_id: &str) -> u8 {
        let gb_per_model =
            crate::agent::config::find_model_by_id(&self.available_models, subagent_model_id)
                .and_then(|e| e.info.auto_compact_threshold_percent);
        crate::util::config::resolve_auto_compact_threshold_percent_from_tiers(
            self.auto_compact_threshold_tiers
                .user_per_model
                .get(subagent_model_id)
                .copied(),
            self.auto_compact_threshold_tiers.user_session,
            gb_per_model,
            self.auto_compact_threshold_tiers.remote_global,
        )
    }
    /// Resolve the 429 wait-attempt budget against the subagent's own model id.
    pub(crate) fn resolve_subagent_rate_limit_max_attempts(&self, subagent_model_id: &str) -> u32 {
        let per_model =
            crate::agent::config::find_model_by_id(&self.available_models, subagent_model_id)
                .and_then(|e| e.info.subagent_rate_limit_max_attempts);
        let remote = self
            .remote_settings
            .as_ref()
            .and_then(|s| s.subagent_rate_limit_max_attempts);
        crate::agent::mvp_agent::resolve_subagent_rate_limit_max_attempts(
            per_model,
            remote,
            crate::agent::mvp_agent::subagent_rate_limit_max_attempts_env(),
        )
    }
    /// Bind a spawned subagent by the parent session's `--tools`/`--disallowed-tools`/`--permission-mode` restrictions.
    fn apply_session_cli_overrides(&self, def: &mut distill_agent::config::AgentDefinition) {
        if let Some(ref cfg) = self.agent_config {
            cfg.cli_agent_overrides.apply_to_subagent_definition(def);
        }
    }
    /// Not `Config::feature`: the parent's tiers resolve against the subagent's own remote settings snapshot.
    pub(crate) fn resolve_feature(&self, feature: crate::agent::config::Feature) -> bool {
        use crate::agent::config::FeatureSources;
        let mut sources = self.agent_config.as_ref().map_or_else(
            || FeatureSources::from_process_env(feature),
            |parent| parent.feature_sources(feature),
        );
        sources.remote = feature.remote_value(self.remote_settings.as_ref());
        feature.resolve(sources).value
    }
    pub(crate) fn resolve_compaction_verbatim_input(&self) -> bool {
        self.resolve_feature(crate::agent::config::Feature::CompactionVerbatimInput)
    }
    pub(crate) fn resolve_compaction_tool_choice(
        &self,
    ) -> crate::util::config::CompactionToolChoice {
        crate::util::config::resolve_compaction_tool_choice_from(
            crate::agent::config::env_string(crate::util::config::ENV_COMPACTION_TOOL_CHOICE)
                .as_deref(),
            self.agent_config
                .as_ref()
                .and_then(|c| c.features.compaction_tool_choice.as_deref()),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_tool_choice.as_deref()),
        )
    }
    pub(crate) fn snapshot_parent_compaction_pins(
        resolved_mode: distill_chat_state::CompactionMode,
        resolved_two_pass: bool,
        parent_agent_name: Option<&str>,
        parent_model_agent_type: Option<&str>,
        parent_cwd: &Path,
    ) -> crate::session::CompactionPins {
        let current = parent_agent_name
            .and_then(|name| distill_agent::discovery::by_name_in_cwd(name, parent_cwd))
            .map(|d| d.user_message_template)
            .unwrap_or_default();
        let template = crate::agent::mvp_agent::inherited_harness_template(
            &current,
            parent_model_agent_type,
            parent_cwd,
        )
        .unwrap_or(current);
        crate::session::cursor_compaction_pins(
            resolved_mode,
            resolved_two_pass,
            crate::session::is_cursor_user_template(&template),
        )
    }
    pub(crate) fn compaction_pins_for_child(
        &self,
        child_template: &distill_agent::prompt::user_message::UserMessageTemplate,
    ) -> crate::session::CompactionPins {
        crate::session::cursor_compaction_pins(
            self.parent_compaction.mode,
            self.parent_compaction.two_pass,
            crate::session::is_cursor_user_template(child_template),
        )
    }
    /// Env > parent config features > this context's remote settings > default.
    pub(crate) fn resolve_compaction_mode(&self) -> distill_chat_state::CompactionMode {
        crate::agent::config::resolve_compaction_mode_from(
            crate::agent::config::env_string("GROK_COMPACTION_MODE").as_deref(),
            self.agent_config
                .as_ref()
                .and_then(|c| c.features.compaction_mode.as_deref()),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_mode.as_deref()),
        )
        .with_segment_detail(crate::agent::config::resolve_compaction_detail_from(
            crate::agent::config::env_string("GROK_COMPACTION_DETAIL").as_deref(),
            self.agent_config
                .as_ref()
                .and_then(|c| c.features.compaction_detail.as_deref()),
            self.remote_settings
                .as_ref()
                .and_then(|r| r.compaction_detail.as_deref()),
        ))
    }
    /// Whether a completed subagent's working copy is saved into the repo as a git ref and its directory deleted.
    pub(crate) fn resolve_subagent_worktree_snapshot_enabled(&self) -> bool {
        self.resolve_feature(crate::agent::config::Feature::SubagentWorktreeSnapshot)
    }
}
#[derive(Default)]
struct ShellCompletionState {
    telemetry_tokens: u64,
    spawned_notification_emitted: bool,
    persisted_output_dir: Option<PathBuf>,
}
#[derive(Clone, Default)]
pub(crate) struct ShellCompletionData {
    auto_wake_enabled: bool,
    parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
    task_output_tool_name: String,
    scheduler_delete_tool_name: Option<String>,
    scheduler_create_tool_name: Option<String>,
    synthetic_trace_tx:
        Option<mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    goal_loop_active: Arc<std::sync::atomic::AtomicBool>,
    attempt_id: Option<distill_message_delivery_core::AttemptId>,
    turn_number: Option<u64>,
    state: Arc<parking_lot::Mutex<ShellCompletionState>>,
}
impl ShellCompletionData {
    fn from_context(
        ctx: &SubagentSpawnContext,
        attempt_id: distill_message_delivery_core::AttemptId,
        turn_number: Option<u64>,
    ) -> Self {
        ShellCompletionData {
            auto_wake_enabled: ctx.auto_wake_enabled,
            parent_cmd_tx: ctx.parent_cmd_tx.clone(),
            task_output_tool_name: ctx.task_output_tool_name.clone(),
            scheduler_delete_tool_name: ctx.scheduler_delete_tool_name.clone(),
            scheduler_create_tool_name: ctx.scheduler_create_tool_name.clone(),
            synthetic_trace_tx: ctx.synthetic_trace_tx.clone(),
            goal_loop_active: Arc::clone(&ctx.goal_loop_active),
            attempt_id: Some(attempt_id),
            turn_number,
            state: Default::default(),
        }
    }
    pub(crate) fn persisted_output_dir(&self) -> Option<PathBuf> {
        self.state.lock().persisted_output_dir.clone()
    }
    fn set_persisted_output_dir(&self, path: Option<PathBuf>) {
        self.state.lock().persisted_output_dir = path;
    }
    fn telemetry_tokens(&self) -> u64 {
        self.state.lock().telemetry_tokens
    }
    fn set_telemetry_tokens(&self, tokens: u64) {
        self.state.lock().telemetry_tokens = tokens;
    }
    fn has_emitted_spawned_notification(&self) -> bool {
        self.state.lock().spawned_notification_emitted
    }
    fn mark_spawned_notification_emitted(&self) {
        self.state.lock().spawned_notification_emitted = true;
    }
}
pub(crate) struct SubagentPresentation {
    is_turn_active: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) synthetic_trace_tx:
        Option<mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
}
impl SubagentPresentation {
    pub(crate) fn new() -> Self {
        Self {
            is_turn_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            synthetic_trace_tx: None,
        }
    }
    pub(crate) fn turn_active_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.is_turn_active)
    }
}
/// Resolve the sampling config and model ID for a subagent. Precedence: `[subagents.models].{agent_name}` config override > explicit `AgentDefinition` model > the parent session's live sampling config.
/// Unknown pins warn and fall through. The caller applies runtime model overrides before this runs.
#[tracing::instrument(level = "debug", skip_all)]
async fn resolve_subagent_sampling_config(
    agent_name: &str,
    agent_model: &distill_agent::config::ModelOverride,
    ctx: &SubagentSpawnContext,
) -> (distill_sampler::SamplerConfig, acp::ModelId) {
    let (parent_config, parent_mid) = read_parent_sampling_config(ctx).await;
    let try_pin = |model_id: &str, source: &'static str, unknown_msg: &'static str| {
        match resolve_model_override_to_config(model_id, ctx) {
            Some((config, canonical_id)) => {
                log_subagent_model_resolution(
                    agent_name,
                    source,
                    &config,
                    &canonical_id,
                    &parent_config,
                );
                Some((config, canonical_id))
            }
            None => {
                tracing::warn!(agent = agent_name, model_id, "{unknown_msg}");
                None
            }
        }
    };
    if let Some(model_id) = ctx.subagent_model_overrides.get(agent_name)
        && let Some(resolved) = try_pin(
            model_id,
            "config_override",
            "Subagent model override references unknown model, falling through to inherit",
        )
    {
        return resolved;
    }
    if let ModelOverride::Override(model_id) = agent_model
        && let Some(resolved) = try_pin(
            model_id,
            "agent_definition",
            "Agent definition model references unknown model, falling through to inherit",
        )
    {
        return resolved;
    }
    log_subagent_model_resolution(
        agent_name,
        "inherit_parent",
        &parent_config,
        &parent_mid,
        &parent_config,
    );
    (parent_config, parent_mid)
}
/// Resolve a subagent's effective sampling config and model id, honoring the model-resolution precedence.
/// An explicit `runtime_override_model` is the goal role model or a persona override carried on `effective_runtime.model`.
/// It is resolved HERE, BEFORE [`resolve_subagent_sampling_config`], where the user `[subagents.models]` pin and `AgentDefinition.model` apply. So a goal/persona override WINS over a user per-agent pin. An override that does not resolve to a known model warns and falls through to the pin path. Extracted from `run_shell_child` so the precedence is unit-testable without spawning a child session.
#[tracing::instrument(level = "debug", skip_all)]
async fn resolve_effective_model_config(
    runtime_override_model: Option<&str>,
    subagent_type: &str,
    definition_model: &distill_agent::config::ModelOverride,
    ctx: &SubagentSpawnContext,
) -> (distill_sampler::SamplerConfig, acp::ModelId) {
    if let Some(model_id) = runtime_override_model {
        if let Some(resolved) = resolve_model_override_to_config(model_id, ctx) {
            return resolved;
        }
        tracing::warn!(
            model_id,
            "Runtime model override references unknown model, falling through"
        );
    }
    resolve_subagent_sampling_config(subagent_type, definition_model, ctx).await
}
/// Truncate an API key to a safe prefix for logging.
/// Counts characters, not bytes: a configured key with a multi-byte character would panic a byte slice, and this only ever runs to build a log line.
fn key_prefix(key: &Option<String>) -> String {
    match key {
        Some(k) => k.chars().take(8).collect(),
        None => "<none>".to_string(),
    }
}
/// Emit a unified log entry recording which model and credentials a subagent resolved to, and how they compare to the parent's.
fn log_subagent_model_resolution(
    agent_name: &str,
    priority: &str,
    resolved: &distill_sampler::SamplerConfig,
    resolved_id: &acp::ModelId,
    parent: &distill_sampler::SamplerConfig,
) {
    let child_key = key_prefix(&resolved.api_key);
    let parent_key = key_prefix(&parent.api_key);
    let keys_match = resolved.api_key == parent.api_key;
    distill_telemetry::unified_log::debug(
        "subagent model resolved",
        None,
        Some(serde_json::json!({
            "agent": agent_name,
            "priority": priority,
            "child_model": resolved_id.0.as_ref(),
            "child_base_url": &resolved.base_url,
            "child_key_prefix": child_key,
            "parent_model": &parent.model,
            "parent_base_url": &parent.base_url,
            "parent_key_prefix": parent_key,
            "keys_match": keys_match,
        })),
    );
}
/// Session-token bearer resolver for a subagent config, over the parent's `AuthManager` (wire-valid only).
/// Without it the subagent runs forever on the `api_key` frozen at spawn and 401s once the parent rotates the token.
/// Gated exactly like the parent session's resolver (`auth_method::session_token_auth_gate`); all three subagent config paths go through this.
fn session_bearer_resolver(
    ctx: &SubagentSpawnContext,
    byok: crate::agent::auth_method::ModelByok,
    base_url: &str,
) -> Option<distill_sampler::SharedBearerResolver> {
    use crate::agent::auth_method;
    auth_method::session_token_auth_gate(
        auth_method::is_session_based_method(&ctx.auth_method_id),
        byok,
        crate::util::is_xai_api_url(base_url),
    )
    .then(|| {
        distill_login::credential_provider::WireValidBearerResolver::shared(
            ctx.auth_manager.clone(),
        )
    })
}
/// [`session_bearer_resolver`] for an inherited config, where only the model string is known: BYOK comes from the catalog memo.
fn inherited_bearer_resolver(
    ctx: &SubagentSpawnContext,
    model: &str,
    base_url: &str,
) -> Option<distill_sampler::SharedBearerResolver> {
    let byok = crate::agent::config::resolve_model_auth_facts_and_provider(model)
        .0
        .byok;
    session_bearer_resolver(ctx, byok, base_url)
}
fn parent_catalog_model_id(ctx: &SubagentSpawnContext, routing_model: &str) -> acp::ModelId {
    let models = ctx.models_manager.models();
    resolve_catalog_key(&models, &ctx.model_id)
        .or_else(|| resolve_catalog_key(&models, &acp::ModelId::new(routing_model)))
        .unwrap_or_else(|| ctx.model_id.clone())
}
/// Read the parent session's actual current sampling config. Prefers the live state from `ChatStateHandle` (authoritative). Falls back to the baseline on `SubagentSpawnContext` if the actor is unavailable.
/// The returned [`acp::ModelId`] is the parent session catalog id (`ctx.model_id`), not the process-global default or chat-state routing slug.
#[tracing::instrument(level = "debug", skip_all)]
async fn read_parent_sampling_config(
    ctx: &SubagentSpawnContext,
) -> (distill_sampler::SamplerConfig, acp::ModelId) {
    if let Some(ref chat_state) = ctx.parent_chat_state {
        if let Some(cfg) = chat_state.get_sampling_config().await {
            let creds = chat_state.get_credentials().await;
            let mut extra_headers = cfg.extra_headers;
            crate::agent::config::inject_url_derived_headers(
                &mut extra_headers,
                creds.alpha_test_key.as_deref(),
                &cfg.base_url,
            );
            let auth_scheme = crate::agent::config::try_resolve_model_credentials(&cfg.model, None)
                .map(|r| r.auth_scheme)
                .unwrap_or_default();
            let inherited_base_url = cfg.base_url.clone();
            let strip_guard = ctx.would_strip_fallback_key(creds.api_key.as_deref());
            let catalog_model_id = parent_catalog_model_id(ctx, &cfg.model);
            let supports_backend_search = ctx
                .models_manager
                .model_supports_backend_search(catalog_model_id.0.as_ref());
            let extra_response_includes = crate::agent::config::response_include_extensions(
                supports_backend_search,
                &cfg.api_backend,
                &cfg.base_url,
            );
            let inherited = distill_sampler::SamplerConfig {
                api_key: creds.api_key,
                base_url: cfg.base_url,
                mtls_cert_dir: cfg.mtls_cert_dir,
                model: cfg.model.clone(),
                max_completion_tokens: cfg.max_completion_tokens,
                temperature: cfg.temperature,
                top_p: cfg.top_p,
                api_backend: cfg.api_backend,
                auth_scheme,
                request_compression: crate::util::config::request_compression_for_url(
                    &inherited_base_url,
                ),
                extra_headers,
                extra_response_includes,
                conversation_group_id: cfg.conversation_group_id,
                query_params: cfg.query_params.clone(),
                env_http_headers: cfg.env_http_headers.clone(),
                context_window: cfg.context_window.get(),
                client_version: creds.client_version,
                reasoning_effort: cfg.reasoning_effort,
                reasoning_summary: cfg.reasoning_summary,
                force_http1: false,
                max_retries: cfg.max_retries.or(ctx.sampling_config.max_retries),
                rate_limit_retry_threshold: cfg.rate_limit_retry_threshold,
                stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
                idle_timeout_secs: None,
                client_identifier: ctx.sampling_config.client_identifier.clone(),
                deployment_id: ctx.sampling_config.deployment_id.clone(),
                user_id: ctx.sampling_config.user_id.clone(),
                origin_client: ctx.sampling_config.origin_client.clone(),
                attribution_callback: ctx.attribution_callback.clone(),
                bearer_resolver: if strip_guard {
                    None
                } else {
                    inherited_bearer_resolver(ctx, &cfg.model, &inherited_base_url)
                },
                supports_backend_search,
                compactions_remaining: ctx
                    .models_manager
                    .model_compactions_remaining(catalog_model_id.0.as_ref()),
                compaction_at_tokens: ctx
                    .models_manager
                    .model_compaction_at_tokens(catalog_model_id.0.as_ref()),
                doom_loop_recovery: ctx.sampling_config.doom_loop_recovery,
                reasoning_shape: ctx.sampling_config.reasoning_shape,
                header_injector: ctx.sampling_config.header_injector.clone(),
            };
            let model_id = ctx.model_id.clone();
            let global_model_id = ctx.models_manager.current_model_id();
            distill_telemetry::unified_log::debug(
                "subagent read parent config (live)",
                None,
                Some(serde_json::json!({
                    "parent_model": &inherited.model,
                    "parent_base_url": &inherited.base_url,
                    "parent_key_prefix": key_prefix(&inherited.api_key),
                    "session_model_id": model_id.0.as_ref(),
                    "global_model_id": global_model_id.0.as_ref(),
                    "source": "chat_state",
                })),
            );
            return (inherited, model_id);
        }
        tracing::warn!(
            "Parent chat state actor returned None for sampling config, \
             falling back to spawn context baseline"
        );
    }
    distill_telemetry::unified_log::warn(
        "subagent read parent config (fallback)",
        None,
        Some(serde_json::json!({
            "parent_model": &ctx.sampling_config.model,
            "parent_base_url": &ctx.sampling_config.base_url,
            "parent_key_prefix": key_prefix(&ctx.sampling_config.api_key),
            "source": "spawn_context_baseline",
            "has_chat_state": ctx.parent_chat_state.is_some(),
        })),
    );
    let mut fallback = ctx.sampling_config.clone();
    fallback.bearer_resolver = if ctx.would_strip_fallback_key(fallback.api_key.as_deref()) {
        None
    } else {
        inherited_bearer_resolver(ctx, &fallback.model, &fallback.base_url)
    };
    let catalog_model_id = parent_catalog_model_id(ctx, &fallback.model);
    fallback.supports_backend_search = ctx
        .models_manager
        .model_supports_backend_search(catalog_model_id.0.as_ref());
    fallback.extra_response_includes = crate::agent::config::response_include_extensions(
        fallback.supports_backend_search,
        &fallback.api_backend,
        &fallback.base_url,
    );
    fallback.compactions_remaining = ctx
        .models_manager
        .model_compactions_remaining(catalog_model_id.0.as_ref());
    fallback.compaction_at_tokens = ctx
        .models_manager
        .model_compaction_at_tokens(catalog_model_id.0.as_ref());
    (fallback, ctx.model_id.clone())
}
/// `AuthType` for a subagent: BYOK gets `ApiKey` (don't overwrite the BYOK key).
/// A session-based ACP method gets `SessionToken` (keep refresh wired); otherwise `ApiKey`.
fn subagent_auth_type(
    model: Option<&crate::agent::config::ModelEntry>,
    auth_method_id: &acp::AuthMethodId,
) -> distill_chat_state::AuthType {
    if model.is_some_and(|m| m.has_own_credentials()) {
        distill_chat_state::AuthType::ApiKey
    } else if crate::agent::auth_method::is_session_based_method(auth_method_id) {
        distill_chat_state::AuthType::SessionToken
    } else {
        distill_chat_state::AuthType::ApiKey
    }
}
/// Resolve a model override string (config key or model ID) to a `(SamplerConfig, ModelId)` pair.
fn resolve_model_override_to_config(
    model_id: &str,
    ctx: &SubagentSpawnContext,
) -> Option<(distill_sampler::SamplerConfig, acp::ModelId)> {
    let entry = crate::agent::config::find_model_by_id(&ctx.available_models, model_id).cloned()?;
    if !entry.info.user_selectable {
        let user_picker_only = ctx
            .agent_config
            .as_ref()
            .is_some_and(|c| !c.requirements.allowed_models.is_pinned());
        if !user_picker_only {
            return None;
        }
    }
    let canonical_model_id = if ctx.available_models.contains_key(model_id) {
        acp::ModelId::new(model_id)
    } else {
        acp::ModelId::new(entry.info().model.clone())
    };
    let session_key = ctx.auth.as_ref().map(|a| a.key.as_str());
    let has_session_key = session_key.is_some();
    let mut credentials = resolve_credentials(&entry, session_key);
    credentials.auth_type = subagent_auth_type(Some(&entry), &ctx.auth_method_id);
    let resolved_auth_type = credentials.auth_type;
    let mut config = sampling_config_for_model(
        &entry,
        credentials,
        ctx.alpha_test_key.clone(),
        ctx.sampling_config.client_version.clone(),
        ctx.sampling_config.deployment_id.clone(),
        ctx.sampling_config.user_id.clone(),
    );
    config.bearer_resolver = if !ctx.would_strip_fallback_key(config.api_key.as_deref())
        && resolved_auth_type == distill_chat_state::AuthType::SessionToken
    {
        session_bearer_resolver(
            ctx,
            if entry.has_own_credentials() {
                crate::agent::auth_method::ModelByok::Byok
            } else {
                crate::agent::auth_method::ModelByok::NotByok
            },
            &config.base_url,
        )
    } else {
        None
    };
    distill_telemetry::unified_log::debug(
        "subagent resolve_model_override_to_config",
        None,
        Some(serde_json::json!({
            "model_id": model_id,
            "canonical_model": canonical_model_id.0.as_ref(),
            "resolved_model_raw": &config.model,
            "base_url": &config.base_url,
            "key_prefix": key_prefix(&config.api_key),
            "has_own_credentials": entry.has_own_credentials(),
            "has_session_key": has_session_key,
            "auth_type": format!("{:?}", resolved_auth_type),
            "auth_method_id": ctx.auth_method_id.0.as_ref(),
        })),
    );
    Some((config, canonical_model_id))
}
struct InitialContext {
    source: InitialContextSource,
    copy_error: Option<String>,
    prefix_len: Option<usize>,
    conversation: Vec<distill_sampling_types::conversation::ConversationItem>,
    force_compact: bool,
    /// True only for a verbatim mirror-fork (parent items copied byte-for-byte).
    /// Gates sending the parent tool snapshot so the child's full request prefix matches the parent.
    /// A summarized-fork fallback leaves this false.
    verbatim_fork: bool,
}
/// Resume bootstrap: preserve only the System head (see `resume_inherited_prefix_len`).
fn resume_initial_context(
    conversation: Vec<distill_sampling_types::conversation::ConversationItem>,
    force_compact: bool,
) -> InitialContext {
    InitialContext {
        source: InitialContextSource::Resumed,
        copy_error: None,
        prefix_len: Some(resume_window::resume_inherited_prefix_len(&conversation)),
        conversation,
        force_compact,
        verbatim_fork: false,
    }
}
/// Apply `fork_filter_chat` then normalize; empty or System-only input (no `<background_context>` produced) fails open to `New`.
fn forked_initial_context(
    mut items: Vec<distill_sampling_types::conversation::ConversationItem>,
) -> InitialContext {
    crate::sampling::fork_filter_chat(&mut items);
    if items.is_empty() {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("empty parent conversation".to_string()),
            prefix_len: None,
            conversation: vec![],
            force_compact: false,
            verbatim_fork: false,
        };
    }
    let (conversation, prefix_len) =
        distill_subagent_resolution::context::normalize_forked_context(items);
    if prefix_len < 2 {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("no inheritable parent content".to_string()),
            prefix_len: None,
            conversation: vec![],
            force_compact: false,
            verbatim_fork: false,
        };
    }
    InitialContext {
        source: InitialContextSource::Forked,
        copy_error: None,
        prefix_len: Some(prefix_len),
        conversation,
        force_compact: false,
        verbatim_fork: false,
    }
}
/// A verbatim mirror requires a coherent tail: the conversation must end on a plain assistant text response (a clean turn boundary).
/// A dangling assistant (unanswered tool calls), a trailing ToolResult (mid-turn), or a trailing user/reasoning means the prefix would be incoherent.
/// The caller then falls back to the summarized path instead of partial-trimming.
fn conversation_tail_is_complete(
    items: &[distill_sampling_types::conversation::ConversationItem],
) -> bool {
    matches!(
        items.last(),
        Some(ConversationItem::Assistant(a)) if a.tool_calls.is_empty()
    )
}
/// Decide the live-fork context. Verbatim mirror (the cache-preserving path): keep the items BYTE-FOR-BYTE. It applies when the parent fits the fork's 80% guard and ends at a clean turn boundary.
/// We deliberately do NOT run `fork_filter_chat` here. At planner spawn the conversation is between turns (the `/goal` user message is not yet pushed). (This is the ONLY path that filters; the verbatim path never does.)
/// Input that is empty or only `System` item(s), before OR after filtering, inherited nothing, so it fails open to `New` rather than a hollow fork.
fn verbatim_or_normalize_fork(
    items: Vec<distill_sampling_types::conversation::ConversationItem>,
    child_context_window: u64,
) -> InitialContext {
    if !items
        .iter()
        .any(|i| !matches!(i, ConversationItem::System(_)))
    {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("forked parent conversation has no inheritable content".to_string()),
            prefix_len: None,
            conversation: vec![],
            force_compact: false,
            verbatim_fork: false,
        };
    }
    let estimated_tokens = distill_chat_state::estimate_conversation_tokens(&items);
    const SAFE_FORK_PERCENT: u64 = 80;
    let threshold = child_context_window * SAFE_FORK_PERCENT / 100;
    if estimated_tokens <= threshold && conversation_tail_is_complete(&items) {
        let prefix_len = items.len();
        return InitialContext {
            source: InitialContextSource::Forked,
            copy_error: None,
            prefix_len: Some(prefix_len),
            conversation: items,
            force_compact: false,
            verbatim_fork: true,
        };
    }
    let mut filtered = items;
    crate::sampling::fork_filter_chat(&mut filtered);
    if !filtered
        .iter()
        .any(|i| !matches!(i, ConversationItem::System(_)))
    {
        return InitialContext {
            source: InitialContextSource::New,
            copy_error: Some("no inheritable parent content after filtering".to_string()),
            prefix_len: None,
            conversation: vec![],
            force_compact: false,
            verbatim_fork: false,
        };
    }
    let (conversation, prefix_len) =
        distill_subagent_resolution::context::normalize_forked_context(filtered);
    InitialContext {
        source: InitialContextSource::Forked,
        copy_error: None,
        prefix_len: Some(prefix_len),
        conversation,
        force_compact: false,
        verbatim_fork: false,
    }
}
/// `true` only when the fork actually summarized (ran `normalize_forked_context`).
/// A verbatim mirror-fork inherits items as-is and never normalizes, so it reports `false` even though its source is `Forked`.
fn fork_context_normalized(source: &InitialContextSource, verbatim_fork: bool) -> bool {
    matches!(source, InitialContextSource::Forked) && !verbatim_fork
}
/// Stamp `subagent_fork` / `forked` on the child summary (live path; disk copy already stamps).
fn stamp_live_fork_session_metadata(
    child_session_info: &SessionInfo,
    parent_session_id: &str,
    parent_prompt_id: Option<String>,
    model_id: &str,
    inherited_prefix_len: Option<usize>,
    fork_context_source: &str,
) {
    let dir = match session::persistence::ensure_owner_only_session_dir(child_session_info) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!(error = %e, "live fork: could not create child session dir for metadata stamp");
            return;
        }
    };
    let summary_path = dir.join("summary.json");
    let model = acp::ModelId::new(model_id);
    let mut summary = std::fs::read(&summary_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .or_else(|| session::persistence::Summary::new(child_session_info, model).ok());
    let Some(ref mut summary) = summary else {
        tracing::warn!("live fork: could not load or create child summary");
        return;
    };
    summary.session_kind = Some("subagent_fork".to_string());
    summary.source_workspace_dir = None;
    summary.fork_context_source = Some(fork_context_source.to_string());
    summary.parent_session_id = Some(parent_session_id.to_string());
    summary.fork_parent_prompt_id = parent_prompt_id;
    summary.inherited_prefix_len = inherited_prefix_len;
    summary.forked_at = Some(chrono::Utc::now());
    if let Ok(bytes) = serde_json::to_vec_pretty(summary)
        && let Err(e) = std::fs::write(&summary_path, bytes)
    {
        tracing::warn!(error = %e, "live fork: failed to write forked session summary");
    }
}
enum BootstrapInitialContext {
    Ready(InitialContext),
    /// Explicit resume_from failed: abort spawn (fail closed).
    ResumeAbort(String),
}
/// Phase 3: resume (fail-closed on copy error) > fork (live then disk, fail-open) > New.
/// Unresolved non-empty resume is aborted by the caller before this runs.
#[tracing::instrument(skip_all)]
async fn bootstrap_initial_context(
    request: &SubagentRequest,
    resume_source: Option<&ResumeSourceData>,
    ctx: &SubagentSpawnContext,
    child_session_info: &SessionInfo,
    child_session_dir: &std::path::Path,
    effective_model_id: &str,
    window: resume_window::ResumeWindowPolicy,
) -> BootstrapInitialContext {
    if request.fork_context && request.resume_from.is_some() {
        tracing::info!(
            subagent_id = %request.id,
            resume_from = ?request.resume_from,
            resume_resolved = resume_source.is_some(),
            "resume_from and fork_context both set; resolved resume wins (fail-closed on copy error, never forks)"
        );
    }
    if let Some(source) = resume_source {
        let source_session_info = SessionInfo {
            id: acp::SessionId::new(source.child_session_id.clone()),
            cwd: source.child_cwd.clone(),
        };
        let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
            crate::util::distill_home::distill_home(),
        );
        let copy_options = crate::session::storage::CopySessionOptions {
            parent_session_id: Some(source.child_session_id.clone()),
            new_model_id: Some(effective_model_id.to_string()),
            session_kind: Some("subagent_resume".to_string()),
            fork_context_source: Some("resumed".to_string()),
            fork_parent_prompt_id: request.parent_prompt_id.clone(),
            copy_plan_state: false,
            copy_plan_mode_state: false,
            copy_signals: false,
            copy_usage: true,
            copy_tool_state: true,
            fork_filter: false,
            ..Default::default()
        };
        use crate::session::storage::StorageAdapter as _;
        let is_same_session = source.child_session_id == child_session_info.id.0.as_ref()
            && source.child_cwd == child_session_info.cwd;
        let copy_result = if is_same_session {
            None
        } else {
            match storage
                .copy_session_data(&source_session_info, child_session_info, copy_options)
                .await
            {
                Ok(result) => Some(result),
                Err(e) => {
                    return BootstrapInitialContext::ResumeAbort(format!(
                        "Cannot resume from subagent '{}': failed to copy source session data: {e}",
                        source.subagent_id,
                    ));
                }
            }
        };
        let conversation = match storage.load_chat_history_from_dir(child_session_dir) {
            Ok(items) if !items.is_empty() => items,
            Ok(_) => {
                return BootstrapInitialContext::ResumeAbort(format!(
                    "Cannot resume from subagent '{}': transcript is empty",
                    source.subagent_id,
                ));
            }
            Err(e) => {
                return BootstrapInitialContext::ResumeAbort(format!(
                    "Cannot resume from subagent '{}': failed to load transcript: {e}",
                    source.subagent_id,
                ));
            }
        };
        let estimated_tokens = distill_chat_state::estimate_conversation_tokens(&conversation);
        let context_window = window.context_window;
        if !window.fits(estimated_tokens) {
            let limit = window.token_limit();
            return BootstrapInitialContext::ResumeAbort(format!(
                "Cannot resume from subagent '{}': source transcript \
                 (~{estimated_tokens} tokens) exceeds the resume limit \
                 ({limit} of {context_window} tokens). Compact the source \
                 first, or resume on a model with a larger context window.",
                source.subagent_id,
            ));
        }
        let threshold_percent = window.auto_compact_threshold_percent;
        let force_compact = match window.plan_force_compact(
            estimated_tokens,
            request.runtime_overrides.output_token_budget.is_some(),
        ) {
            resume_window::ResumeForceCompact::AbortBudgeted => {
                return BootstrapInitialContext::ResumeAbort(format!(
                    "Cannot resume from subagent '{}': source transcript \
                     (~{estimated_tokens} tokens) is over the auto-compact \
                     threshold ({threshold_percent}% of {context_window}), but \
                     this child has an output token budget and cannot compact \
                     a fat transcript. Compact the source first, or resume \
                     without a budget.",
                    source.subagent_id,
                ));
            }
            resume_window::ResumeForceCompact::Arm => true,
            resume_window::ResumeForceCompact::NotNeeded => false,
        };
        let (chat_messages, tool_state) = copy_result.as_ref().map_or(
            (
                conversation.len(),
                child_session_dir.join("tool_state.json").is_file(),
            ),
            |result| (result.chat_messages_copied, result.tool_state_copied),
        );
        tracing::info!(
            subagent_id = %request.id,
            source_subagent = %source.subagent_id,
            chat_messages,
            tool_state,
            estimated_tokens,
            "Loaded source child session data for resume"
        );
        return BootstrapInitialContext::Ready(resume_initial_context(conversation, force_compact));
    }
    if !request.fork_context {
        return BootstrapInitialContext::Ready(InitialContext {
            source: InitialContextSource::New,
            copy_error: None,
            prefix_len: None,
            conversation: vec![],
            force_compact: false,
            verbatim_fork: false,
        });
    }
    let live_items = match ctx.parent_chat_state.as_ref() {
        Some(chat_state) => {
            let items = chat_state.get_conversation().await;
            if items.is_empty() { None } else { Some(items) }
        }
        None => None,
    };
    if let Some(items) = live_items {
        let ctx_out = verbatim_or_normalize_fork(items, window.context_window);
        tracing::info!(
            subagent_id = %request.id,
            subagent_type = %request.subagent_type,
            loaded_items = ctx_out.conversation.len(),
            source = ?ctx_out.source,
            verbatim = ctx_out.verbatim_fork,
            "Forked context from live parent_chat_state"
        );
        if matches!(ctx_out.source, InitialContextSource::Forked) {
            let marker = if ctx_out.verbatim_fork {
                "forked_verbatim"
            } else {
                "forked_summarized"
            };
            stamp_live_fork_session_metadata(
                child_session_info,
                &ctx.parent_session_id,
                request.parent_prompt_id.clone(),
                effective_model_id,
                ctx_out.prefix_len,
                marker,
            );
        }
        return BootstrapInitialContext::Ready(ctx_out);
    }
    if let Some(ref parent_info) = ctx.parent_session_info {
        let storage = crate::session::storage::jsonl::JsonlStorageAdapter::with_root(
            crate::util::distill_home::distill_home(),
        );
        let copy_options = crate::session::storage::CopySessionOptions {
            parent_session_id: Some(ctx.parent_session_id.clone()),
            new_model_id: Some(effective_model_id.to_string()),
            session_kind: Some("subagent_fork".to_string()),
            fork_context_source: Some("forked".to_string()),
            fork_parent_prompt_id: request.parent_prompt_id.clone(),
            copy_plan_state: false,
            copy_plan_mode_state: false,
            copy_signals: false,
            copy_usage: false,
            copy_tool_state: false,
            fork_filter: true,
            ..Default::default()
        };
        use crate::session::storage::StorageAdapter as _;
        return match storage
            .copy_session_data(parent_info, child_session_info, copy_options)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    subagent_id = %request.id,
                    subagent_type = %request.subagent_type,
                    chat_messages = result.chat_messages_copied,
                    tool_state = result.tool_state_copied,
                    "Fork-copied parent session data into child (disk fallback)"
                );
                let items = storage
                    .load_chat_history_from_dir(child_session_dir)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            error = %e,
                            "Failed to load forked chat history, starting with empty context"
                        );
                        vec![]
                    });
                BootstrapInitialContext::Ready(forked_initial_context(items))
            }
            Err(e) => {
                let err_msg = format!("{e}");
                tracing::warn!(
                    subagent_id = %request.id,
                    subagent_type = %request.subagent_type,
                    error = %e,
                    "Failed to fork-copy parent session, falling back to fresh"
                );
                BootstrapInitialContext::Ready(InitialContext {
                    source: InitialContextSource::New,
                    copy_error: Some(err_msg),
                    prefix_len: None,
                    conversation: vec![],
                    force_compact: false,
                    verbatim_fork: false,
                })
            }
        };
    }
    tracing::warn!(
        subagent_id = %request.id,
        subagent_type = %request.subagent_type,
        "fork_context=true but no live parent conversation or parent_session_info; falling back to fresh"
    );
    BootstrapInitialContext::Ready(InitialContext {
        source: InitialContextSource::New,
        copy_error: Some("parent conversation unavailable".to_string()),
        prefix_len: None,
        conversation: vec![],
        force_compact: false,
        verbatim_fork: false,
    })
}
/// Resolve the effective working directory for a child session.
/// Precedence: worktree path > `override_cwd` (non-empty) > parent cwd.
/// The caller selects `override_cwd`: a resumed child inherits the source's effective cwd, a fresh spawn honors its `request.cwd`.
fn resolve_child_cwd(
    worktree_path: Option<&Path>,
    override_cwd: Option<&str>,
    parent_cwd: &Path,
) -> PathBuf {
    worktree_path
        .map(Path::to_path_buf)
        .or_else(|| override_cwd.filter(|s| !s.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| parent_cwd.to_path_buf())
}
/// The cwd a resumed child inherits from its source subagent, or `None` when there is nothing to inherit (the caller falls back to the parent cwd).
/// Only non-worktree sources inherit here; worktree-backed sources are reused by the worktree path.
/// The cwd is existence-checked because a source can be pinned into a sibling's worktree that the snapshot stack later disposes. Resume otherwise skips cwd validation.
fn resume_inherited_cwd(source: Option<&ResumeSourceData>) -> Option<&str> {
    let source = source?;
    if source.worktree_path.is_some() || source.child_cwd.is_empty() {
        return None;
    }
    if !Path::new(&source.child_cwd).is_dir() {
        tracing::warn!(
            source_subagent_id = %source.subagent_id,
            child_cwd = %source.child_cwd,
            "Resume source cwd no longer exists; using parent workspace"
        );
        return None;
    }
    Some(source.child_cwd.as_str())
}
/// Select the cwd override for a child: a resume inherits the source's cwd (never its own `request.cwd`); a fresh spawn uses `request.cwd`.
fn select_override_cwd<'a>(
    resume_source: Option<&'a ResumeSourceData>,
    request_cwd: Option<&'a str>,
) -> Option<&'a str> {
    if resume_source.is_some() {
        resume_inherited_cwd(resume_source)
    } else {
        request_cwd
    }
}
fn durable_resume_source_for(
    id: &str,
    parent_session_id: &str,
    parent_cwd: &Path,
) -> Option<ResumeSourceData> {
    let parent_info = SessionInfo {
        id: acp::SessionId::new(parent_session_id),
        cwd: parent_cwd.to_string_lossy().into_owned(),
    };
    let meta_path = session::persistence::session_dir(&parent_info)
        .join("subagents")
        .join(id)
        .join("meta.json");
    durable_resume_source_from_meta(&meta_path, parent_session_id)
}
fn durable_resume_source_from_meta(
    meta_path: &Path,
    parent_session_id: &str,
) -> Option<ResumeSourceData> {
    let data = std::fs::read_to_string(meta_path).ok()?;
    let meta: SubagentMeta = serde_json::from_str(&data).ok()?;
    if meta.parent_session_id != parent_session_id
        || !matches!(meta.status.as_str(), "completed" | "failed" | "cancelled")
    {
        return None;
    }
    Some(ResumeSourceData {
        subagent_id: meta.subagent_id,
        child_session_id: meta.child_session_id,
        child_cwd: meta.child_cwd.unwrap_or_default(),
        worktree_path: meta.worktree_path.map(PathBuf::from),
        snapshot_ref: meta.snapshot_ref,
        subagent_type: meta.subagent_type,
        persona: meta.persona,
        model_id: meta.effective_model_id,
    })
}
/// Resolve the MCP pool a child subagent should import from its parent. Inheritance applies to **every** agent source (built-in, user, project, and plugin).
/// Plugin agents are not excluded: the parent already connected these servers for the session. Agent-owned `mcpServers` (spawned by the child itself) are handled separately and remain blocked for plugins.
fn resolve_inherited_mcp_pool(
    parent_pool: Option<crate::session::mcp_servers::SharedMcpPool>,
    inheritance: &distill_agent::config::McpInheritance,
) -> Option<crate::session::mcp_servers::SharedMcpPool> {
    parent_pool.and_then(|pool| filter_pool_by_inheritance(pool, inheritance))
}
/// Apply `McpInheritance` filtering to a parent MCP pool snapshot.
/// Returns `None` for `McpInheritance::None` (no pool at all, which avoids an empty import call downstream).
/// For `Named`/`Except`, retains or removes the matching server names in-place.
fn filter_pool_by_inheritance(
    mut pool: crate::session::mcp_servers::SharedMcpPool,
    inheritance: &distill_agent::config::McpInheritance,
) -> Option<crate::session::mcp_servers::SharedMcpPool> {
    match inheritance {
        McpInheritance::All => Some(pool),
        McpInheritance::None => None,
        McpInheritance::Named(names) => {
            let before = pool.server_names().count();
            pool.retain_clients(|name| names.iter().any(|n| n == name));
            tracing::debug!(
                before,
                after = pool.server_names().count(),
                ?names,
                "MCP inheritance: Named filter applied"
            );
            Some(pool)
        }
        McpInheritance::Except(names) => {
            let before = pool.server_names().count();
            pool.retain_clients(|name| !names.iter().any(|n| n == name));
            tracing::debug!(
                before,
                after = pool.server_names().count(),
                ?names,
                "MCP inheritance: Except filter applied"
            );
            Some(pool)
        }
    }
}
/// Resolve a subagent type name to its `AgentDefinition`, with the parent session's CLI tool/permission overrides already applied.
/// The spawn path can therefore never obtain a definition that skips them.
fn resolve_agent_definition(
    subagent_type: &str,
    ctx: &SubagentSpawnContext,
) -> Option<distill_agent::config::AgentDefinition> {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    let resolution_context = distill_subagent_resolution::DefinitionResolutionContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agents,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    let mut def =
        distill_subagent_resolution::discover_agent_definition(subagent_type, &resolution_context)?;
    ctx.apply_session_cli_overrides(&mut def);
    Some(def)
}
fn available_agent_names(ctx: &SubagentSpawnContext) -> Vec<String> {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    distill_subagent_resolution::available_agent_names(
        &distill_subagent_resolution::DefinitionResolutionContext {
            cwd: &ctx.parent_cwd,
            plugins: ctx.plugin_registry.as_deref(),
            cli_agents,
            toggles: &ctx.subagent_toggle,
            allowed_types: ctx.allowed_subagent_types.as_deref(),
        },
    )
}
/// Minimal per-session context for `validate_subagent_type`.
/// Avoids the heavy `SubagentSpawnContext` clone on the validation hot path.
#[derive(Default)]
pub(crate) struct SubagentValidationContext {
    pub parent_cwd: PathBuf,
    pub plugin_registry: Option<Arc<distill_agent::plugins::PluginRegistry>>,
    pub subagent_toggle: HashMap<String, bool>,
    pub allowed_subagent_types: Option<Vec<String>>,
    pub cli_agent_names: Vec<String>,
}
/// Synchronously validate a subagent type against discovery, toggle, and allow-list.
/// `Unknown { available }` is sorted by `str::cmp` for stable rendering.
pub(crate) fn validate_subagent_type(
    subagent_type: &str,
    ctx: &SubagentValidationContext,
) -> SubagentValidateTypeOutcome {
    let context = distill_subagent_resolution::DefinitionValidationContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agent_names: &ctx.cli_agent_names,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    match distill_subagent_resolution::validate_agent_name(subagent_type, &context) {
        Ok(()) => SubagentValidateTypeOutcome::Ok,
        Err(distill_subagent_resolution::ResolutionError::Unknown { available, .. }) => {
            SubagentValidateTypeOutcome::Unknown { available }
        }
        Err(distill_subagent_resolution::ResolutionError::Disabled { .. }) => {
            SubagentValidateTypeOutcome::Disabled
        }
        Err(distill_subagent_resolution::ResolutionError::NotAllowed { allowed, .. }) => {
            SubagentValidateTypeOutcome::NotAllowed { allowed }
        }
        Err(
            distill_subagent_resolution::ResolutionError::PersonaResolution(_)
            | distill_subagent_resolution::ResolutionError::ResumeValidation(_),
        ) => SubagentValidateTypeOutcome::ValidationUnavailable,
    }
}
/// Gate an already-resolved subagent type against the `[subagents.toggle]` disable map and the parent's allow-list.
/// The caller must have already confirmed the type resolves to an `AgentDefinition`; this checks ONLY the toggle and allow-list gates.
/// Returns `Ok` when the type may run and `Disabled` / `NotAllowed` otherwise (never `Unknown` / `ValidationUnavailable`). Shared by [`run_shell_child`] and [`describe_subagent_type`] so both apply identical gates.
fn gate_subagent_type(
    subagent_type: &str,
    ctx: &SubagentSpawnContext,
) -> SubagentValidateTypeOutcome {
    let cli_agents = ctx
        .agent_config
        .as_ref()
        .map(|config| config.cli_agents.as_slice())
        .unwrap_or_default();
    let resolution_context = distill_subagent_resolution::DefinitionResolutionContext {
        cwd: &ctx.parent_cwd,
        plugins: ctx.plugin_registry.as_deref(),
        cli_agents,
        toggles: &ctx.subagent_toggle,
        allowed_types: ctx.allowed_subagent_types.as_deref(),
    };
    match distill_subagent_resolution::gate_agent_definition(subagent_type, &resolution_context) {
        Ok(()) => SubagentValidateTypeOutcome::Ok,
        Err(distill_subagent_resolution::ResolutionError::Disabled { .. }) => {
            SubagentValidateTypeOutcome::Disabled
        }
        Err(distill_subagent_resolution::ResolutionError::NotAllowed { allowed, .. }) => {
            SubagentValidateTypeOutcome::NotAllowed { allowed }
        }
        Err(
            distill_subagent_resolution::ResolutionError::Unknown { .. }
            | distill_subagent_resolution::ResolutionError::PersonaResolution(_)
            | distill_subagent_resolution::ResolutionError::ResumeValidation(_),
        ) => SubagentValidateTypeOutcome::ValidationUnavailable,
    }
}
pub(crate) fn subagent_harness_flavor_is_representable(agent_type: &str) -> bool {
    distill_subagent_resolution::subagent_harness_flavor_is_representable(agent_type)
}
/// Apply the harness-dependent toolset/prompt re-selection to a resolved agent definition.
/// The harness flavor (alternate vs Distill) normally follows the PARENT: `DistillOrchestrator` parents give children the alternate harness.
/// The orchestrator keeps children lean, and other parents inherit the file-tool override (hashline vs standard). A `/goal` role may pass `harness_agent_type` to OVERRIDE that flavor regardless of the parent. So a Distill session can run an alternate-harness verifier and vice-versa. Extracted so both [`run_shell_child`] (real spawn) and [`describe_subagent_type`] (read-only probe) build the SAME `tool_config`.
fn resolve_subagent_toolset(
    subagent_type: &str,
    harness_agent_type: Option<&str>,
    ctx: &SubagentSpawnContext,
    definition: &mut distill_agent::config::AgentDefinition,
) {
    let resolution_context = distill_subagent_resolution::HarnessToolsetContext {
        harness_override: harness_agent_type,
        parent_agent_name: ctx.parent_agent_name.as_deref(),
        parent_model_agent_type: ctx.parent_model_agent_type.as_deref(),
        file_tool_overrides: ctx.file_tool_overrides.as_deref(),
    };
    distill_subagent_resolution::apply_harness_toolset(
        subagent_type,
        &resolution_context,
        definition,
    );
}
/// Map a resolved `ToolServerConfig` into a [`SubagentTypeSummary`]. Keys on each entry's `ToolConfig.kind` (first tool per kind wins). Entries with `kind: None` (`from_id`/MCP/custom tools) are SKIPPED.
/// So this is NOT a byte-for-byte equivalent of the finalize-time `kind_to_name` map, which keys on the registry `entry.kind`.
/// The two agree for the builtin goal toolsets, where every tool's kind is populated by `From<&T: Tool>`. They diverge for `kind: None` tools, which carry no capability signal anyway. The client-facing name is `ToolConfig::resolve_client_name(default_id)`, so a `name_override` is reflected.
fn summarize_tool_config(
    config: &distill_tools::registry::types::ToolServerConfig,
) -> SubagentTypeSummary {
    let mut tool_names: HashMap<ToolKind, String> = HashMap::new();
    for tc in &config.tools {
        let Some(kind) = tc.kind else { continue };
        let default_id = tc.id.rsplit(':').next().unwrap_or(tc.id.as_str());
        let client_name = tc.resolve_client_name(default_id);
        tool_names.entry(kind).or_insert(client_name);
    }
    SubagentTypeSummary {
        can_read: tool_names.contains_key(&ToolKind::Read),
        can_search: tool_names.contains_key(&ToolKind::Search),
        can_execute: tool_names.contains_key(&ToolKind::Execute),
        tool_names,
    }
}
/// Describe a subagent type's resolved toolset WITHOUT spawning it. The parent uses the summary for the per-role capability gate and prompt rendering before committing a `/goal` `{model, agent_type}` pair.
/// `harness_agent_type` is the `/goal`-only harness override. When set it must resolve to an `AgentDefinition` via this module's [`resolve_agent_definition`].
/// It matches the main session for builtin harness names but does NOT apply the main session's env / ACP-profile / strict-harness precedence. An unresolvable harness returns `Unknown` so the `/goal` caller fails open to the session harness. Otherwise it decides the summarized toolset's flavor.
pub(crate) fn describe_subagent_type(
    subagent_type: &str,
    harness_agent_type: Option<&str>,
    ctx: &SubagentSpawnContext,
) -> SubagentDescribeOutcome {
    if let Some(harness) = harness_agent_type
        && resolve_agent_definition(harness, ctx).is_none()
    {
        return SubagentDescribeOutcome::Unknown {
            available: available_agent_names(ctx),
        };
    }
    let Some(mut definition) = resolve_agent_definition(subagent_type, ctx) else {
        return SubagentDescribeOutcome::Unknown {
            available: available_agent_names(ctx),
        };
    };
    match gate_subagent_type(subagent_type, ctx) {
        SubagentValidateTypeOutcome::Disabled => return SubagentDescribeOutcome::Disabled,
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            return SubagentDescribeOutcome::NotAllowed { allowed };
        }
        SubagentValidateTypeOutcome::Unknown { available } => {
            return SubagentDescribeOutcome::Unknown { available };
        }
        SubagentValidateTypeOutcome::ValidationUnavailable => {
            return SubagentDescribeOutcome::Unavailable;
        }
        SubagentValidateTypeOutcome::CoordinatorGone => {
            return SubagentDescribeOutcome::Unavailable;
        }
        SubagentValidateTypeOutcome::Ok => {}
        _ => return SubagentDescribeOutcome::Unavailable,
    }
    resolve_subagent_toolset(subagent_type, harness_agent_type, ctx, &mut definition);
    SubagentDescribeOutcome::Ok(summarize_tool_config(&definition.tool_config))
}
/// Resolve a subagent's turn limit: its own `maxTurns` wins, else inherit the parent's.
fn resolve_subagent_max_turns(
    definition_max_turns: Option<u32>,
    parent_max_turns: Option<usize>,
) -> Option<usize> {
    definition_max_turns
        .map(|v| v as usize)
        .or(parent_max_turns)
}
/// What to do with a resumed subagent's isolated worktree directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResumeWorktreeAction {
    /// Directory on disk and no snapshot ref: reuse it as-is.
    Reuse,
    /// Directory gone but a snapshot ref exists: rehydrate from it.
    Rehydrate,
    /// Directory gone and no snapshot: fall back to the shared workspace.
    Shared,
}
/// Decide how to recover a resumed subagent's worktree from its on-disk state and whether a durable snapshot is available.
/// Pure so the three outcomes are unit-testable without git/async.
fn resume_worktree_action(dir_exists: bool, snapshot_ref: Option<&str>) -> ResumeWorktreeAction {
    if snapshot_ref.is_some() {
        ResumeWorktreeAction::Rehydrate
    } else if dir_exists {
        ResumeWorktreeAction::Reuse
    } else {
        ResumeWorktreeAction::Shared
    }
}
/// The parent session's working directory: the source path for a subagent worktree.
fn parent_source_cwd(ctx: &SubagentSpawnContext) -> std::path::PathBuf {
    ctx.parent_session_info
        .as_ref()
        .map(|i| std::path::PathBuf::from(&i.cwd))
        .unwrap_or_else(|| std::path::PathBuf::from(&ctx.parent_cwd))
}
/// Effective permission mode for a spawned subagent. Plugin agents never honor a non-default mode. Under the pin, `bypassPermissions` downgrades to `Default` so a repo/profile/`--agents` def can't restore auto-approve.
/// Caller logs it.
fn resolve_subagent_permission_mode(
    requested: distill_agent::config::PermissionMode,
    is_plugin: bool,
    policy_block: Option<&'static str>,
) -> distill_agent::config::PermissionMode {
    if is_plugin {
        return PermissionMode::Default;
    }
    if policy_block.is_some() && requested == PermissionMode::BypassPermissions {
        return PermissionMode::Default;
    }
    requested
}
/// Main repo root for a subagent's source: the durable repo a completion snapshot is transferred into and the repo a resume rehydrates from.
/// Both arms MUST resolve this identically.
fn resolve_subagent_source_repo(ctx: &SubagentSpawnContext) -> std::path::PathBuf {
    let source_cwd = parent_source_cwd(ctx);
    distill_workspace::session::git::find_main_repo_root_from_path(&source_cwd)
        .unwrap_or(source_cwd)
}
enum SubagentWaitOutcome {
    Cancelled,
    TurnResult(Box<Result<SubagentPromptTurnResult, oneshot::error::RecvError>>),
}
#[tracing::instrument(skip_all)]
async fn await_subagent_turn_or_cancellation(
    prompt_rx: oneshot::Receiver<SubagentPromptTurnResult>,
    cancel_token: CancellationToken,
) -> SubagentWaitOutcome {
    tokio::select! {
        _ = cancel_token.cancelled() => SubagentWaitOutcome::Cancelled,
        turn_result = prompt_rx => SubagentWaitOutcome::TurnResult(Box::new(turn_result)),
    }
}
/// `None` when the signals actor never answered (wedged or dead) — callers
/// must not mistake the failed read for zero counts.
async fn signals_snapshot_counts(child_handle: &SessionHandle) -> Option<(u32, u32)> {
    handle_request::child_actor_query(
        "signals_snapshot",
        child_handle.signals_handle.snapshot(),
        None,
    )
    .await
    .map(|snapshot| (snapshot.tool_call_count, snapshot.turn_count))
}
fn cancellation_error_message(
    category: Option<distill_session_events::types::CancellationCategory>,
    context: Option<&crate::session::commands::CancellationContext>,
) -> String {
    let detail = context.and_then(|ctx| {
        let tool = ctx.tool_name.as_deref();
        let reason = ctx.reason.as_deref();
        let hook = ctx.hook_name.as_deref();
        match (tool, reason, hook) {
            (Some(t), Some(r), Some(h)) => Some(format!("{r} for tool `{t}` (hook: {h})")),
            (Some(t), Some(r), None) => Some(format!("{r} for tool `{t}`")),
            (Some(t), None, _) => Some(format!("tool `{t}`")),
            _ => None,
        }
    });
    match (category, &detail) {
        (Some(CancellationCategory::PermissionRejected), Some(d)) => {
            format!("Subagent turn was cancelled: user rejected permission — {d}")
        }
        (Some(CancellationCategory::PermissionRejected), None) => {
            "Subagent turn was cancelled: user rejected a permission prompt".to_string()
        }
        (Some(CancellationCategory::PermissionCancelled), _) => {
            "Subagent turn was cancelled: user cancelled a permission prompt".to_string()
        }
        (Some(CancellationCategory::HookDenied), Some(d)) => {
            format!("Subagent turn was cancelled: hook denied — {d}")
        }
        (Some(CancellationCategory::HookDenied), None) => {
            "Subagent turn was cancelled: blocked by a hook".to_string()
        }
        (Some(CancellationCategory::MidTurnAbort), _) => {
            "Subagent turn was cancelled: aborted mid-turn".to_string()
        }
        _ => "Subagent turn was cancelled".to_string(),
    }
}
fn telemetry_owner_kind(request: &SubagentRequest) -> distill_telemetry::events::SubagentOwnerKind {
    if request.owner.is_workflow() {
        distill_telemetry::events::SubagentOwnerKind::Workflow
    } else if request.from_scheduler_loop() {
        distill_telemetry::events::SubagentOwnerKind::SchedulerLoop
    } else {
        distill_telemetry::events::SubagentOwnerKind::Task
    }
}
fn failure_result(request: &SubagentRequest, error: &str) -> SubagentResult {
    SubagentResult::failed(request.id.clone(), request.id.clone(), error)
}
fn cancelled_result(request: &SubagentRequest, error: &str) -> SubagentResult {
    SubagentResult::cancelled(request.id.clone(), request.id.clone(), error)
}
fn child_run_output(
    result: SubagentResult,
    completion_data: ShellCompletionData,
    snapshot_ref: Option<String>,
) -> ChildRunOutput<ShellCompletionData> {
    ChildRunOutput {
        result,
        completion_data,
        snapshot_ref,
    }
}
fn setup_failure_output(
    error: &str,
    request: &SubagentRequest,
    child_session_id: &acp::SessionId,
    subagent_meta_dir: &Path,
    gcs_ctx: &GcsUploadContext,
    may_persist_terminal: bool,
    completion_data: ShellCompletionData,
) -> ChildRunOutput<ShellCompletionData> {
    let result = if may_persist_terminal {
        fail_subagent(
            error,
            &request.id,
            child_session_id,
            subagent_meta_dir,
            0,
            gcs_ctx,
        )
    } else {
        failure_result(request, error)
    };
    child_run_output(result, completion_data, None)
}
fn fail_subagent(
    error: &str,
    subagent_id: &str,
    child_session_id: &acp::SessionId,
    subagent_meta_dir: &Path,
    duration_ms: u64,
    gcs_ctx: &GcsUploadContext,
) -> SubagentResult {
    let result = SubagentResult {
        duration_ms,
        ..SubagentResult::failed(subagent_id, &*child_session_id.0, error)
    };
    persist_subagent_completion(subagent_meta_dir, &result, gcs_ctx);
    result
}
/// Why an unpromoted child is being torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnpromotedChildDisposition {
    Cancelled,
    AdmissionTimedOut,
}
impl UnpromotedChildDisposition {
    fn result(self, subagent_id: &str, child_session_id: &str, duration_ms: u64) -> SubagentResult {
        match self {
            Self::Cancelled => SubagentResult {
                duration_ms,
                ..SubagentResult::cancelled(subagent_id, child_session_id, "Subagent was cancelled")
            },
            Self::AdmissionTimedOut => SubagentResult {
                duration_ms,
                ..SubagentResult::failed(
                    subagent_id,
                    child_session_id,
                    "Subagent initial prompt was not admitted before the deadline",
                )
            },
        }
    }
}
/// Tear down a child whose pending-to-active promotion lost to cancellation or whose initial prompt was not admitted before the readiness deadline.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(level = "debug", skip_all)]
async fn cancel_pending_shell_child(
    child_cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    child_thread: crate::session::SessionThread,
    workspace_ops: &distill_workspace::WorkspaceOps,
    subagent_id: &str,
    child_session_id: &acp::SessionId,
    subagent_meta_dir: &Path,
    worktree_path: Option<&Path>,
    worktree_freshly_created: bool,
    duration_ms: u64,
    gcs_ctx: &GcsUploadContext,
    thread_exit_timeout: std::time::Duration,
    disposition: UnpromotedChildDisposition,
    may_persist_terminal: bool,
) -> SubagentResult {
    prompt_turn_receipt::cancel_shell_child_turn(child_cmd_tx);
    let _ = child_cmd_tx.send(SessionCommand::Shutdown(
        crate::session::ShutdownKind::Graceful,
    ));
    let thread_exited = await_session_thread_exit(&child_thread, thread_exit_timeout).await;
    let fate = UnpromotedResourceFate::from_thread_exit(thread_exited);
    let result = disposition.result(subagent_id, child_session_id.0.as_ref(), duration_ms);
    if may_persist_terminal {
        persist_subagent_completion(subagent_meta_dir, &result, gcs_ctx);
    }
    if !fate.should_release() {
        tracing::warn!(
            subagent_id,
            child_session_id = %child_session_id.0,
            "unpromoted child actor did not exit before the teardown bound; preserving worktree and workspace binding"
        );
        return result;
    }
    workspace_ops.end_local_session(child_session_id.0.as_ref());
    if worktree_freshly_created
        && let Some(wt_path) = worktree_path
        && let Err(e) = crate::session::worktree::remove_subagent_worktree(wt_path).await
    {
        tracing::warn!(
            subagent_id,
            worktree_path = %wt_path.display(),
            error = %e,
            "failed to remove pristine worktree for killed-while-pending subagent"
        );
    }
    result
}
const PROGRESS_PUBLISH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Change signature for the progress-publisher dedupe: `(turn_count, tool_call_count, context_usage_pct, error_count, tokens_used)`.
/// `tokens_used` is part of the signature so rising child token spend always publishes a tick. Goal token accounting (subagent records, live totals, and the turn-end budget check) keys off prompt token movement.
/// That movement can climb while turn/tool counts and the coarse context-usage *percent* bucket stay flat.
type ProgressSignature = (u32, u32, u8, u32, u64);
fn progress_tick_should_emit(
    prev: ProgressSignature,
    cur: ProgressSignature,
    heartbeat_due: bool,
) -> bool {
    cur != prev || heartbeat_due
}
/// Parent-actor tick channel for [`spawn_progress_publisher`]: goal token accounting is the only consumer.
/// A goal-disabled session sends no per-tick commands.
fn goal_tick_cmd_tx(
    goal_enabled: bool,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
) -> Option<mpsc::UnboundedSender<SessionCommand>> {
    if goal_enabled {
        parent_cmd_tx.cloned()
    } else {
        None
    }
}
/// Spawn a background task that periodically emits `SubagentProgress` notifications on the parent session's notification channel. The publisher samples the child's `SessionSignalsHandle` every [`PROGRESS_PUBLISH_INTERVAL`].
/// It stops automatically when `cancel_token` is cancelled (subagent completion/cancellation).
/// When `parent_cmd_tx` is `Some`, each tick also goes to the parent `SessionActor` so goal mode can advance its live subagent token accounting. The actor's `SubagentProgress` arm never persists these ticks. Notifications are **not** persisted to JSONL; they are transient UI hints, not authoritative lifecycle events.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_progress_publisher(
    signals_handle: crate::session::signals::SessionSignalsHandle,
    gateway: GatewaySender,
    parent_session_id: String,
    subagent_id: String,
    attempt_id: Option<String>,
    child_session_id: String,
    started_at: std::time::Instant,
    cancel_token: tokio_util::sync::CancellationToken,
    parent_cmd_tx: Option<mpsc::UnboundedSender<SessionCommand>>,
) -> tokio_util::task::AbortOnDropHandle<()> {
    tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        let mut interval = tokio::time::interval(PROGRESS_PUBLISH_INTERVAL);
        interval.tick().await;
        let mut last_signature: ProgressSignature = (0, 0, 0, 0, 0);
        let mut last_emit_at = tokio::time::Instant::now();
        let heartbeat_max = tokio::time::Duration::from_secs(8);
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }
            let signals = match signals_handle.snapshot().await {
                Some(s) => s,
                None => break,
            };
            let sig: ProgressSignature = (
                signals.turn_count,
                signals.tool_call_count,
                signals.context_window_usage,
                signals.error_count,
                signals.context_tokens_used,
            );
            let heartbeat_due = last_emit_at.elapsed() >= heartbeat_max;
            if !progress_tick_should_emit(last_signature, sig, heartbeat_due) {
                continue;
            }
            last_signature = sig;
            last_emit_at = tokio::time::Instant::now();
            let duration_ms = started_at.elapsed().as_millis() as u64;
            let update = SessionUpdate::SubagentProgress {
                subagent_id: subagent_id.clone(),
                attempt_id: attempt_id.clone(),
                parent_session_id: parent_session_id.clone(),
                child_session_id: child_session_id.clone(),
                duration_ms,
                turn_count: signals.turn_count,
                tool_call_count: signals.tool_call_count,
                tokens_used: signals.context_tokens_used,
                context_window_tokens: signals.context_window_tokens,
                context_usage_pct: signals.context_window_usage,
                tools_used: signals.tools_used,
                error_count: signals.error_count,
            };
            let notification = SessionNotification {
                session_id: acp::SessionId::new(parent_session_id.clone()),
                update,
                meta: None,
            };
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(ref cmd_tx) = parent_cmd_tx {
                let _ = cmd_tx.send(SessionCommand::XaiSessionNotification { notification });
            }
            if let Some(params) = params {
                let ext_notification =
                    acp::ExtNotification::new("x.ai/session_notification", params.into());
                gateway.forward_fire_and_forget(ext_notification);
            }
        }
    }))
}
#[cfg(test)]
mod progress_publisher_tests {
    use super::{ProgressSignature, progress_tick_should_emit};
    const BASE: ProgressSignature = (3, 7, 12, 0, 30_000);
    #[test]
    fn token_only_change_emits() {
        let cur: ProgressSignature = (3, 7, 12, 0, 45_000);
        assert!(progress_tick_should_emit(BASE, cur, false));
    }
    #[test]
    fn unchanged_without_heartbeat_skips() {
        assert!(!progress_tick_should_emit(BASE, BASE, false));
    }
    #[test]
    fn heartbeat_forces_emit_when_unchanged() {
        assert!(progress_tick_should_emit(BASE, BASE, true));
    }
}
/// Metadata stored as `meta.json` in the child session directory.
/// Links the child session back to its parent.
/// For the GCS-persisted artifact (`subagent.json`), see [`SubagentSessionMetadata`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SubagentMeta {
    pub subagent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub subagent_type: String,
    pub description: String,
    pub prompt: String,
    /// "running" | "completed" | "failed" | "cancelled"
    pub status: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Effective context source after bootstrap: "new" or "resumed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_context_source: Option<String>,
    /// True only for a summarized (normalized) fork; false for verbatim mirror-forks, resume, and new sessions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context_normalized: bool,
    /// Error message if fork-copy failed and fell back to fresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_copy_error: Option<String>,
    /// Named persona applied to this subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// ID of the source subagent this session was resumed from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    /// Effective cwd used by the child session.
    /// Persisted for durable `resume_from` reconstruction after in-memory cache eviction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_cwd: Option<String>,
    /// Worktree path if the child used `isolation=worktree`.
    /// Persisted for durable `resume_from` reconstruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// Durable git ref holding a snapshot of the child's worktree working state.
    /// Persisted so a deleted worktree can be rehydrated on resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_ref: Option<String>,
    /// Effective model ID used by the child session.
    /// Persisted for durable `resume_from` identity validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_model_id: Option<String>,
}
/// Canonical subagent metadata for GCS persistence (`subagent.json`).
/// Uploaded to `{session_id}/subagent.json` in GCS and optionally mirrored locally.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubagentSessionMetadata {
    pub schema_version: u32,
    pub session_id: String,
    pub session_kind: String,
    pub subagent_id: String,
    pub child_session_id: String,
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_prompt_id: Option<String>,
    pub subagent_type: String,
    /// Human-readable spawn description: the task tool's `description` argument, or the fixed role label for harness-spawned goal subagents.
    /// Role labels are "goal plan writer", "goal achievement skeptic", and so on.
    /// All goal roles share `subagent_type = "general-purpose"`, so this is what identifies them in the artifact.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    #[serde(default)]
    pub context_normalized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isolation_mode: Option<String>,
    #[serde(default)]
    pub depth: u32,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_copy_error: Option<String>,
    /// ID of the source subagent this session was resumed from (`resume_from`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
}
impl SubagentSessionMetadata {
    pub(crate) const SCHEMA_VERSION: u32 = 1;
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_meta(
        meta: &SubagentMeta,
        model_id: Option<&str>,
        cwd: Option<&str>,
        worktree_path: Option<&str>,
        isolation_mode: Option<&str>,
        capability_mode: Option<&str>,
        reasoning_effort: Option<&str>,
        role: Option<&str>,
        parent_prompt_id: Option<&str>,
        depth: u32,
    ) -> Self {
        let session_kind = if meta.resumed_from.is_some() {
            "subagent_resume"
        } else {
            "subagent"
        };
        Self {
            schema_version: Self::SCHEMA_VERSION,
            session_id: meta.child_session_id.clone(),
            session_kind: session_kind.to_string(),
            subagent_id: meta.subagent_id.clone(),
            child_session_id: meta.child_session_id.clone(),
            parent_session_id: meta.parent_session_id.clone(),
            parent_prompt_id: parent_prompt_id.map(str::to_string),
            subagent_type: meta.subagent_type.clone(),
            description: meta.description.clone(),
            role: role.map(str::to_string),
            persona: meta.persona.clone(),
            context_normalized: meta.context_normalized,
            capability_mode: capability_mode.map(str::to_string),
            reasoning_effort: reasoning_effort.map(str::to_string),
            model_id: model_id.map(str::to_string),
            cwd: cwd.map(str::to_string),
            worktree_path: worktree_path.map(str::to_string),
            isolation_mode: isolation_mode.map(str::to_string),
            depth,
            started_at: meta.started_at.to_rfc3339(),
            completed_at: meta.completed_at.map(|t| t.to_rfc3339()),
            status: meta.status.clone(),
            duration_ms: meta.duration_ms,
            tool_calls: meta.tool_calls,
            turns: meta.turns,
            error: meta.error.clone(),
            fork_copy_error: meta.fork_copy_error.clone(),
            resumed_from: meta.resumed_from.clone(),
        }
    }
}
/// Write via a same-directory temp file and rename, so a crash mid-write cannot leave a torn `meta.json` or `output.json`.
fn atomic_write(path: &Path, contents: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), contents)?;
    tmp.persist(path)?;
    Ok(())
}
/// Write `meta.json`.
/// Returns `true` on success so callers on the resume-pointer path can gate worktree disposal on a durable write.
pub(super) fn write_subagent_meta(dir: &Path, meta: &SubagentMeta) -> bool {
    let json = match serde_json::to_string_pretty(meta) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize subagent meta");
            return false;
        }
    };
    if let Err(e) = atomic_write(&dir.join("meta.json"), &json) {
        tracing::warn!(error = %e, "failed to write subagent meta");
        return false;
    }
    true
}
/// Borrowed output schema so persistence does not copy the text.
#[derive(serde::Serialize)]
struct SubagentOutputFileRef<'a> {
    schema_version: u32,
    output: &'a str,
}
const SUBAGENT_OUTPUT_SCHEMA_VERSION: u32 = 1;
fn write_subagent_output(dir: &Path, output: &str) -> bool {
    let file = SubagentOutputFileRef {
        schema_version: SUBAGENT_OUTPUT_SCHEMA_VERSION,
        output,
    };
    let json = match serde_json::to_string(&file) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize subagent output");
            return false;
        }
    };
    if let Err(e) = atomic_write(&dir.join("output.json"), &json) {
        tracing::warn!(error = %e, "failed to write subagent output");
        return false;
    }
    true
}
pub(crate) fn read_subagent_output(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct OutputFile {
        schema_version: u32,
        output: String,
    }
    let data = std::fs::read_to_string(dir.join("output.json")).ok()?;
    let file: OutputFile = serde_json::from_str(&data).ok()?;
    (file.schema_version == SUBAGENT_OUTPUT_SCHEMA_VERSION).then_some(file.output)
}
/// Extra runtime context for GCS artifact upload.
/// `SubagentMeta` doesn't persist these fields, so they're carried from the spawn site.
#[derive(Clone)]
struct GcsUploadContext {
    bucket_url: Option<String>,
    upload_method: Option<crate::session::repo_changes::UploadMethod>,
    model_id: Option<String>,
    cwd: Option<String>,
    isolation_mode: Option<String>,
    capability_mode: Option<String>,
    reasoning_effort: Option<String>,
    role_name: Option<String>,
    parent_prompt_id: Option<String>,
    depth: u32,
    auth_manager: std::sync::Arc<distill_login::AuthManager>,
}
/// Persist the durable worktree `snapshot_ref` into the on-disk `meta.json` after completion. `resumable_source_for` can then rehydrate the disposed worktree on resume. Returns `true` only when the ref is persisted to disk.
/// Any read/parse/write failure is `warn!`-logged (this is the critical resume pointer). The caller then keeps the worktree rather than removing it without a recoverable ref. Also re-asserts the terminal `status`.
/// A failed `persist_subagent_completion` write otherwise leaves a non-terminal record that `resumable_source_for` rejects once the worktree is gone.
fn update_subagent_meta_snapshot_ref(dir: &Path, snapshot_ref: &str, status: &str) -> bool {
    let meta_path = dir.join("meta.json");
    let mut meta = match std::fs::read_to_string(&meta_path) {
        Ok(data) => match serde_json::from_str::<SubagentMeta>(&data) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse subagent meta; snapshot_ref not persisted (resume pointer lost)");
                return false;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to read subagent meta; snapshot_ref not persisted (resume pointer lost)");
            return false;
        }
    };
    meta.snapshot_ref = Some(snapshot_ref.to_string());
    meta.status = status.to_string();
    write_subagent_meta(dir, &meta)
}
#[must_use]
fn persist_subagent_output(dir: &Path, result: &SubagentResult) -> Option<PathBuf> {
    (result.success && !result.output.is_empty() && write_subagent_output(dir, &result.output))
        .then(|| dir.to_path_buf())
}
fn persist_subagent_completion(dir: &Path, result: &SubagentResult, gcs_ctx: &GcsUploadContext) {
    let meta_path = dir.join("meta.json");
    if let Ok(data) = std::fs::read_to_string(&meta_path)
        && let Ok(mut meta) = serde_json::from_str::<SubagentMeta>(&data)
    {
        meta.status = result.status().to_string();
        meta.completed_at = Some(chrono::Utc::now());
        meta.duration_ms = Some(result.duration_ms);
        meta.tool_calls = Some(result.tool_calls);
        meta.turns = Some(result.turns);
        meta.error = result.error.clone();
        write_subagent_meta(dir, &meta);
        if let (Some(bucket), Some(method)) = (&gcs_ctx.bucket_url, &gcs_ctx.upload_method) {
            let gcs_meta = SubagentSessionMetadata::from_meta(
                &meta,
                gcs_ctx.model_id.as_deref(),
                gcs_ctx.cwd.as_deref(),
                result.worktree_path.as_deref(),
                gcs_ctx.isolation_mode.as_deref(),
                gcs_ctx.capability_mode.as_deref(),
                gcs_ctx.reasoning_effort.as_deref(),
                gcs_ctx.role_name.as_deref(),
                gcs_ctx.parent_prompt_id.as_deref(),
                gcs_ctx.depth,
            );
            let bucket = bucket.clone();
            let method = method.clone();
            let auth_for_spawn = gcs_ctx.auth_manager.clone();
            tokio::spawn(async move {
                upload_subagent_metadata(&gcs_meta, &bucket, method, auth_for_spawn).await;
            });
        }
    }
}
pub(crate) const ORPHAN_RECONCILE_REASON: &str = "interrupted by process restart";
pub(crate) const LIVE_ORPHAN_RECONCILE_REASON: &str = "orphaned while parent session stayed live";
/// `SubagentFinished` for a force-terminated orphan; interrupt counts are zeroed.
fn cancelled_orphan_finish(
    subagent_id: String,
    attempt_id: Option<String>,
    child_session_id: String,
    duration_ms: u64,
    reason: &str,
) -> SessionUpdate {
    SessionUpdate::SubagentFinished {
        attempt_id,
        subagent_id,
        child_session_id,
        status: "cancelled".to_string(),
        error: Some(reason.to_owned()),
        tool_calls: 0,
        turns: 0,
        duration_ms,
        tokens_used: 0,
        output: None,
        will_wake: false,
    }
}
/// Flip a stale `running` meta to `cancelled` and emit the missing finish.
/// On meta-write failure returns `false` and skips the notify, so a reload re-heals.
fn finalize_orphaned_subagent(
    subagent_meta_dir: &Path,
    mut meta: SubagentMeta,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    reason: &str,
) -> bool {
    if !is_on_disk_meta_running(subagent_meta_dir) {
        return false;
    }
    let completed_at = chrono::Utc::now();
    let duration_ms = (completed_at - meta.started_at).num_milliseconds().max(0) as u64;
    meta.status = "cancelled".to_string();
    meta.completed_at = Some(completed_at);
    meta.duration_ms = Some(duration_ms);
    meta.tool_calls = Some(0);
    meta.turns = Some(0);
    meta.error = Some(reason.to_owned());
    if !write_subagent_meta(subagent_meta_dir, &meta) {
        return false;
    }
    emit_subagent_notification(
        gateway,
        &meta.parent_session_id,
        cancelled_orphan_finish(
            meta.subagent_id,
            meta.attempt_id,
            meta.child_session_id,
            duration_ms,
            reason,
        ),
        parent_cmd_tx,
    );
    true
}
/// Persist a coordinator-held terminal outcome over a stale `running` meta.
/// Same write-then-emit durability as [`finalize_orphaned_subagent`].
fn persist_running_meta_as_finish(
    subagent_meta_dir: &Path,
    mut meta: SubagentMeta,
    finish: &SessionUpdate,
) -> bool {
    let SessionUpdate::SubagentFinished {
        status,
        error,
        tool_calls,
        turns,
        duration_ms,
        ..
    } = finish
    else {
        return false;
    };
    if !is_on_disk_meta_running(subagent_meta_dir) {
        return false;
    }
    meta.status = status.clone();
    meta.completed_at = Some(chrono::Utc::now());
    meta.duration_ms = Some(*duration_ms);
    meta.tool_calls = Some(*tool_calls);
    meta.turns = Some(*turns);
    meta.error = error.clone();
    write_subagent_meta(subagent_meta_dir, &meta)
}
fn is_on_disk_meta_running(subagent_meta_dir: &Path) -> bool {
    let Ok(data) = std::fs::read_to_string(subagent_meta_dir.join("meta.json")) else {
        return false;
    };
    serde_json::from_str::<SubagentMeta>(&data).is_ok_and(|meta| meta.status == "running")
}
/// Parse `meta_path` and return it only when it is a stale `running` orphan owned by `parent_session_id` and not tracked live.
/// Malformed metas yield `None`.
fn running_orphan_meta(meta_path: &Path, parent_session_id: &str) -> Option<SubagentMeta> {
    let data = std::fs::read_to_string(meta_path).ok()?;
    let meta: SubagentMeta = serde_json::from_str(&data).ok()?;
    if meta.status != "running" || meta.parent_session_id != parent_session_id {
        return None;
    }
    Some(meta)
}
fn completed_finish_from_inspection(
    inspection: &SubagentInspection,
    attempt_id: Option<String>,
) -> Option<SessionUpdate> {
    let (status, error, tool_calls, turns) = match &inspection.snapshot.status {
        SubagentSnapshotStatus::Completed {
            tool_calls, turns, ..
        } => ("completed", None, *tool_calls, *turns),
        SubagentSnapshotStatus::Failed { error } => ("failed", Some(error.clone()), 0, 0),
        SubagentSnapshotStatus::Cancelled { reason } => ("cancelled", reason.clone(), 0, 0),
        SubagentSnapshotStatus::Initializing | SubagentSnapshotStatus::Running { .. } => {
            return None;
        }
    };
    Some(SessionUpdate::SubagentFinished {
        attempt_id,
        subagent_id: inspection.snapshot.subagent_id.clone(),
        child_session_id: inspection.child_session_id.clone(),
        status: status.to_owned(),
        error,
        tool_calls,
        turns,
        duration_ms: inspection.snapshot.duration_ms,
        tokens_used: 0,
        output: None,
        will_wake: false,
    })
}
/// Heal subagents stuck "Running" after a dead process: emit exactly one `SubagentFinished` per id. Two id-keyed sources are unioned, so a crash orphan present in both heals once.
/// They are `unfinished` (replayed spawns whose finish a rewind dropped, or a forked-in subagent with no meta) and on-disk `running` metas. Ids still active or pending are skipped.
/// A `running` meta becomes `cancelled`, unless the coordinator still holds its terminal result, which is then re-emitted. Runs after replay so the finish orders after the spawn. Pre-existing recovery entry point: args are independent handles/sources from two call sites, not one groupable object
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all)]
pub(crate) async fn reconcile_orphaned_subagents_with_backend(
    unfinished: &[crate::session::storage::UnfinishedSubagent],
    backend: &distill_tools::implementations::distill::task::backend::ChannelBackend,
    session_dir: &Path,
    parent_session_id: &str,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    orphan_reason: &str,
    heal_lock: Arc<tokio::sync::Mutex<()>>,
) {
    let _heal_guard = heal_lock.lock().await;
    let subagents_dir = session_dir.join("subagents");
    let mut candidates: std::collections::BTreeMap<
        String,
        Option<crate::session::storage::UnfinishedSubagent>,
    > = std::collections::BTreeMap::new();
    for unfinished in unfinished {
        candidates.insert(unfinished.subagent_id.clone(), Some(unfinished.clone()));
    }
    if let Ok(entries) = std::fs::read_dir(&subagents_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if running_orphan_meta(&entry.path().join("meta.json"), parent_session_id).is_some()
                && let Some(id) = name.to_str()
            {
                candidates.entry(id.to_string()).or_insert(None);
            }
        }
    }
    for (subagent_id, replayed_spawn) in candidates {
        let inspection = backend.inspect(&subagent_id).await;
        if inspection
            .as_ref()
            .is_some_and(|inspection| inspection.snapshot.is_running())
        {
            continue;
        }
        let subagent_dir = subagents_dir.join(&subagent_id);
        let meta = std::fs::read_to_string(subagent_dir.join("meta.json"))
            .ok()
            .and_then(|data| serde_json::from_str::<SubagentMeta>(&data).ok());
        match meta {
            Some(m) if m.parent_session_id != parent_session_id => {}
            Some(m) if m.status == "running" => {
                if let Some(finish) = inspection.as_ref().and_then(|inspection| {
                    completed_finish_from_inspection(inspection, m.attempt_id.clone())
                }) {
                    if persist_running_meta_as_finish(&subagent_dir, m, &finish) {
                        tracing::info!(
                            subagent_id = %subagent_id,
                            parent_session_id,
                            "Re-emitting finish for completed subagent with a lost terminal meta write"
                        );
                        emit_subagent_notification(
                            gateway,
                            parent_session_id,
                            finish,
                            parent_cmd_tx,
                        );
                    }
                } else {
                    tracing::info!(
                        subagent_id = %m.subagent_id,
                        parent_session_id,
                        "Reconciling orphaned subagent left running by a previous process"
                    );
                    finalize_orphaned_subagent(
                        &subagent_dir,
                        m,
                        gateway,
                        parent_cmd_tx,
                        orphan_reason,
                    );
                }
            }
            Some(m) => {
                if replayed_spawn.is_none() {
                    continue;
                }
                tracing::info!(
                    subagent_id = %subagent_id,
                    parent_session_id,
                    status = %m.status,
                    "Re-emitting finish for rewound subagent (terminal meta survived)"
                );
                emit_subagent_notification(
                    gateway,
                    parent_session_id,
                    SessionUpdate::SubagentFinished {
                        attempt_id: m.attempt_id,
                        subagent_id,
                        child_session_id: m.child_session_id,
                        status: m.status,
                        error: m.error,
                        tool_calls: m.tool_calls.unwrap_or(0),
                        turns: m.turns.unwrap_or(0),
                        duration_ms: m.duration_ms.unwrap_or(0),
                        tokens_used: 0,
                        output: None,
                        will_wake: false,
                    },
                    parent_cmd_tx,
                );
            }
            None => {
                let Some(replayed_spawn) = replayed_spawn else {
                    continue;
                };
                tracing::info!(
                    subagent_id = %subagent_id,
                    parent_session_id,
                    "Reconciling inherited subagent with no local meta (cancelled)"
                );
                emit_subagent_notification(
                    gateway,
                    parent_session_id,
                    cancelled_orphan_finish(
                        subagent_id,
                        replayed_spawn.attempt_id,
                        replayed_spawn.child_session_id,
                        0,
                        orphan_reason,
                    ),
                    parent_cmd_tx,
                );
            }
        }
    }
    drop(_heal_guard);
}
/// Same heal as resume, for a parent that never restarts.
/// `unfinished` is empty so only on-disk `running` metas are candidates.
/// Persist-first plus the per-parent lock make a second tick, sequential or overlapping, a no-op.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn reconcile_live_orphaned_subagents(
    backend: &distill_tools::implementations::distill::task::backend::ChannelBackend,
    session_dir: &Path,
    parent_session_id: &str,
    gateway: &GatewaySender,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    heal_lock: Arc<tokio::sync::Mutex<()>>,
) {
    reconcile_orphaned_subagents_with_backend(
        &[],
        backend,
        session_dir,
        parent_session_id,
        gateway,
        parent_cmd_tx,
        LIVE_ORPHAN_RECONCILE_REASON,
        heal_lock,
    )
    .await;
}
#[cfg(feature = "test-support")]
pub(crate) mod isolated_spawn_e2e;
#[cfg(test)]
mod tests;
