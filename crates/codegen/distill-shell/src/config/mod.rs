// Modified for Distill by Samuel Fajreldines, 2026.
pub mod reloader;
pub mod watcher;
use crate::bundle;
pub use distill_config_types::{
    DEFAULT_RECENCY_DECAY, MemoryConfig, MemoryDreamConfig, MemoryDreamSettings,
    MemoryEmbeddingConfig, MemoryEmbeddingSettings, MemoryFlushConfig, MemoryFlushSettings,
    MemoryGcConfig, MemoryGcSettings, MemoryIndexConfig, MemoryIndexSettings,
    MemoryInitialInjectionConfig, MemoryInitialInjectionSettings, MemoryMode, MemorySearchConfig,
    MemorySearchSettings, MemorySessionConfig, MemorySessionSettings, MemorySettings,
    MemoryV2Config, MemoryV2Rollout, MemoryV2Settings, MemoryWatcherConfig, MemoryWatcherSettings,
    MmrConfig, MmrSettings, PruningConfig, PruningSettings, TemporalDecayConfig,
    TemporalDecaySettings,
};
use serde::Deserialize;
use std::sync::atomic::{AtomicU8, Ordering};
/// Read the memory mode selected by the current effective config.
///
/// Session actors use their already-resolved [`MemoryConfig`] instead. This
/// helper is for standalone commands that do not own a session.
static STANDALONE_MEMORY_MODE: AtomicU8 = AtomicU8::new(0);
pub fn cache_standalone_memory_mode(mode: MemoryMode) {
    let encoded = match mode {
        MemoryMode::Legacy => 1,
        MemoryMode::V2 => 2,
    };
    STANDALONE_MEMORY_MODE.store(encoded, Ordering::Release);
}
pub fn load_memory_mode() -> std::io::Result<MemoryMode> {
    match STANDALONE_MEMORY_MODE.load(Ordering::Acquire) {
        1 => Ok(MemoryMode::Legacy),
        2 => Ok(MemoryMode::V2),
        _ => load_memory_mode_with_remote(None),
    }
}
pub fn load_memory_mode_with_remote(
    remote: Option<&crate::util::config::RemoteSettings>,
) -> std::io::Result<MemoryMode> {
    let config = load_effective_config()?;
    Ok(resolve_standalone_memory_mode(&config, remote))
}
fn resolve_standalone_memory_mode(
    config: &toml::Value,
    remote: Option<&crate::util::config::RemoteSettings>,
) -> MemoryMode {
    MemoryConfig::resolve(false, false, config, remote).mode
}
/// Configuration for subagent (task tool) support.
/// Parsed from the `[subagents]` section of `~/.grok/config.toml` or `.grok/config.toml`.
/// Enabled by default; can be disabled via the `GROK_SUBAGENTS=0` env var or `[subagents] enabled = false` in config.toml.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SubagentsConfig {
    /// Whether subagent support is enabled.
    pub enabled: bool,
    /// Raw `[subagents] max_depth` (i64 so out-of-range parses; clamped to at least 1 at resolve).
    #[serde(default)]
    pub max_depth: Option<i64>,
    #[serde(default)]
    pub max_concurrent: Option<i64>,
    /// Concurrent subagent turn-sampling limit. See [`Self::resolve_sampling_limit`].
    #[serde(default)]
    pub sampling_limit: Option<i64>,
    /// `"queue"` or `"fail"`.
    #[serde(default)]
    pub limit_behavior: Option<String>,
    #[serde(default)]
    pub workflow_max_concurrent: Option<i64>,
    /// Per-subagent model ID overrides.
    /// Keys are agent names, values are model IDs that must exist in the available models registry.
    /// Parsed from `[subagents.models]` in config.toml.
    #[serde(default)]
    pub models: std::collections::HashMap<String, String>,
    /// Per-subagent enable/disable toggles.
    /// Keys are agent names, values are booleans.
    /// Omitted agents default to enabled (`true`).
    #[serde(default)]
    pub toggle: std::collections::HashMap<String, bool>,
    /// Declarative subagent role definitions.
    #[serde(default)]
    pub roles: std::collections::HashMap<String, SubagentRole>,
    /// Named persona/SOUL definitions.
    #[serde(default)]
    pub personas: std::collections::HashMap<String, SubagentPersona>,
}
use distill_subagent_resolution::config::{SubagentPersona, SubagentRole};
impl SubagentsConfig {
    fn discover_personas_in_dir(&mut self, dir: &std::path::Path) {
        if !dir.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to read personas directory");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if self.personas.contains_key(&name) {
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<SubagentPersona>(&content) {
                    Ok(mut persona) => {
                        persona.source_dir = path.parent().map(|p| p.to_path_buf());
                        persona.source_path = Some(path.display().to_string());
                        tracing::debug!(persona = %name, "Loaded persona from file");
                        self.personas.insert(name, persona);
                    }
                    Err(e) => {
                        tracing::warn!(persona = %name, error = %e, "Failed to parse persona file");
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to read persona file");
                }
            }
        }
    }
    fn discover_roles_in_dir(&mut self, dir: &std::path::Path) {
        if !dir.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to read roles directory");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if self.roles.contains_key(&name) {
                tracing::debug!(role = %name, "Skipping file-based role, higher-priority config takes precedence");
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => match toml::from_str::<SubagentRole>(&content) {
                    Ok(mut role) => {
                        role.source_dir = path.parent().map(|p| p.to_path_buf());
                        tracing::debug!(role = %name, "Loaded role from file");
                        self.roles.insert(name, role);
                    }
                    Err(e) => {
                        tracing::warn!(
                            role = %name,
                            path = %path.display(),
                            error = %e,
                            "Failed to parse role file"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to read role file"
                    );
                }
            }
        }
    }
    /// Check if a subagent is enabled.
    /// Returns `true` if the agent is not in the toggle map (default enabled).
    pub fn is_subagent_enabled(&self, name: &str) -> bool {
        self.toggle.get(name).copied().unwrap_or(true)
    }
    /// Look up a role by name.
    pub fn get_role(&self, name: &str) -> Option<&SubagentRole> {
        self.roles.get(name)
    }
    /// Look up a persona by name.
    pub fn get_persona(&self, name: &str) -> Option<&SubagentPersona> {
        self.personas.get(name)
    }
    /// Discover personas from `.grok/personas/` directory. File-based personas are loaded from `{cwd}/.grok/personas/*.toml`. Each file defines a single `SubagentPersona`. The file stem becomes the persona name.
    /// Inline config takes precedence.
    pub(crate) fn discover_personas(&mut self, cwd: &std::path::Path) {
        let dir = cwd.join(".grok").join("personas");
        self.discover_personas_in_dir(&dir);
    }
    /// Validate all role definitions. Returns a list of (role_name, error_message) for invalid entries.
    pub fn validate_roles(&self) -> Vec<(String, String)> {
        let valid_modes = ["read-only", "read-write", "execute", "all"];
        let mut errors = Vec::new();
        for (name, role) in &self.roles {
            if role.description.is_empty() {
                errors.push((name.clone(), "description is required".to_string()));
            }
            if let Some(ref mode) = role.default_capability_mode
                && !valid_modes.contains(&mode.as_str())
            {
                errors.push((
                    name.clone(),
                    format!(
                        "invalid default_capability_mode \"{mode}\", \
                         must be one of: {}",
                        valid_modes.join(", ")
                    ),
                ));
            }
            if let Some(ref pf) = role.prompt_file
                && pf.trim().is_empty()
            {
                errors.push((
                    name.clone(),
                    "prompt_file must not be empty or whitespace".to_string(),
                ));
            }
        }
        errors
    }
    /// Discover roles from `.grok/roles/` directory and merge with inline config. File-based roles are loaded from `{cwd}/.grok/roles/*.toml`.
    /// Each file defines a single `SubagentRole` (same schema as inline `[subagents.roles.*]`). The file stem becomes the role name. Precedence: inline config roles override file-based roles with the same name.
    pub(crate) fn discover_roles(&mut self, cwd: &std::path::Path) {
        let roles_dir = cwd.join(".grok").join("roles");
        self.discover_roles_in_dir(&roles_dir);
    }
    pub const ENV_MAX_DEPTH: &'static str = "GROK_SUBAGENTS_MAX_DEPTH";
    pub const DEFAULT_MAX_DEPTH: u32 = 1;
    /// Clamp to `1..=u32::MAX`. Values below 1 (including 0 and negatives) warn and become 1 so nesting is never accidentally disabled.
    pub(crate) fn clamp_max_depth(raw: i64, source: &str) -> u32 {
        if raw < i64::from(Self::DEFAULT_MAX_DEPTH) {
            tracing::warn!(
                source,
                value = raw,
                "subagents max_depth < 1; clamping to 1"
            );
            Self::DEFAULT_MAX_DEPTH
        } else if raw > i64::from(u32::MAX) {
            tracing::warn!(
                source,
                value = raw,
                "subagents max_depth exceeds u32::MAX; clamping"
            );
            u32::MAX
        } else {
            raw as u32
        }
    }
    /// Precedence: env > TOML > remote > [`Self::DEFAULT_MAX_DEPTH`].
    /// Depth 0 is the top-level session; a child is parent+1. Spawn is rejected when `depth >= max`.
    /// So `max = 1` allows only top-level spawns; nested spawns from a first-level subagent need `max >= 2`.
    pub(crate) fn resolve_max_depth(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
    ) -> u32 {
        if let Some(raw) = env {
            match raw.trim().parse::<i64>() {
                Ok(v) => return Self::clamp_max_depth(v, "env"),
                Err(_) => {
                    tracing::warn!(
                        value = %raw,
                        "invalid GROK_SUBAGENTS_MAX_DEPTH (expected integer); ignoring"
                    );
                }
            }
        }
        if let Some(v) = config {
            return Self::clamp_max_depth(v, "config");
        }
        if let Some(v) = remote {
            return Self::clamp_max_depth(i64::from(v), "remote");
        }
        Self::DEFAULT_MAX_DEPTH
    }
    pub const ENV_MAX_CONCURRENT: &'static str = "GROK_MAX_CONCURRENT_SUBAGENTS";
    pub const ENV_SAMPLING_LIMIT: &'static str = "GROK_SUBAGENT_SAMPLING_LIMIT";
    pub const ENV_LIMIT_BEHAVIOR: &'static str = "GROK_SUBAGENT_LIMIT_BEHAVIOR";
    pub const ENV_WORKFLOW_MAX_CONCURRENT: &'static str = "GROK_WORKFLOW_MAX_CONCURRENT_AGENTS";
    pub(crate) fn resolve_max_concurrent(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
    ) -> usize {
        resolve_positive_count(
            Self::ENV_MAX_CONCURRENT,
            env,
            config,
            remote,
            distill_tools::implementations::distill::task::admission::DEFAULT_MAX_CONCURRENT,
        )
    }
    /// Resolve the subagent turn-sampling limit, clamped to [`crate::agent::subagent::MAX_SUBAGENT_SAMPLING_LIMIT`].
    /// `default` is the resolved concurrent-subagent bound (`GROK_MAX_CONCURRENT_SUBAGENTS`).
    /// A lower `GROK_SUBAGENT_SAMPLING_LIMIT`, `[subagents] sampling_limit`, or remote value caps sampling further.
    pub(crate) fn resolve_sampling_limit(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
        default: usize,
    ) -> usize {
        let max = crate::agent::subagent::MAX_SUBAGENT_SAMPLING_LIMIT;
        let resolved =
            resolve_positive_count(Self::ENV_SAMPLING_LIMIT, env, config, remote, default);
        if resolved > max {
            tracing::warn!(
                name = Self::ENV_SAMPLING_LIMIT,
                resolved,
                max,
                "subagent sampling limit exceeds the ceiling; clamping"
            );
        }
        resolved.min(max)
    }
    pub(crate) fn resolve_workflow_max_concurrent(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
    ) -> usize {
        resolve_positive_count(
            Self::ENV_WORKFLOW_MAX_CONCURRENT,
            env,
            config,
            remote,
            crate::session::workflow::host_service::DEFAULT_WORKFLOW_MAX_CONCURRENT_AGENTS,
        )
    }
    pub(crate) fn resolve_limit_behavior(
        env: Option<&str>,
        config: Option<&str>,
        remote: Option<&str>,
    ) -> distill_tools::implementations::distill::task::admission::LimitBehavior {
        use distill_tools::implementations::distill::task::admission::LimitBehavior;
        for (source, value) in [("env", env), ("config", config), ("remote", remote)] {
            let Some(value) = value else { continue };
            if value.eq_ignore_ascii_case("fail") {
                return LimitBehavior::Fail;
            }
            if value.eq_ignore_ascii_case("queue") {
                return LimitBehavior::Queue;
            }
            tracing::warn!(
                source,
                %value,
                "subagent limit_behavior is neither `queue` nor `fail`; ignoring"
            );
        }
        LimitBehavior::Queue
    }
    /// Resolve the final subagents config from all sources (in priority order): CLI flag `--subagents` (absolute highest, always enables) `GROK_SUBAGENTS` env var: `1`/`true` enables, `0`/`false` force-disables
    /// Config file `[subagents]` section Default (enabled) `enabled` is deliberately not remotely gated. Only explicit local intent (CLI flag, `GROK_SUBAGENTS`, `[subagents] enabled`) changes the default.
    /// Project files are excluded from this trust-independent base; Task boundaries overlay them using the parent cwd's authoritative trust verdict.
    pub fn resolve(cli_flag: bool, config: &toml::Value) -> Self {
        let user_grok_root = distill_config::user_distill_home();
        Self::resolve_base_with_sources(
            cli_flag,
            config,
            user_grok_root.as_deref(),
            &bundle::bundled_root(),
        )
    }
    pub(crate) fn resolve_base_with_sources(
        cli_flag: bool,
        config: &toml::Value,
        user_grok_root: Option<&std::path::Path>,
        bundled_root: &std::path::Path,
    ) -> Self {
        let mut result: Self = config
            .get("subagents")
            .and_then(|v| v.clone().try_into().ok())
            .unwrap_or_default();
        let resolved = crate::agent::config::resolve_enabled(
            if cli_flag { Some(true) } else { None },
            "GROK_SUBAGENTS",
            result.enabled,
            config.get("subagents").is_some(),
            None,
            true,
        );
        result.enabled = resolved.value;
        if let Some(root) = user_grok_root {
            result.discover_roles_in_dir(&root.join("roles"));
            result.discover_personas_in_dir(&root.join("personas"));
        }
        result.discover_roles_in_dir(&bundled_root.join("roles"));
        result.discover_personas_in_dir(&bundled_root.join("personas"));
        result
    }
    pub(crate) fn effective_definition_maps(
        roles: &std::collections::HashMap<String, SubagentRole>,
        personas: &std::collections::HashMap<String, SubagentPersona>,
        cwd: &std::path::Path,
        project_trusted: bool,
    ) -> (
        std::collections::HashMap<String, SubagentRole>,
        std::collections::HashMap<String, SubagentPersona>,
    ) {
        let mut project = Self::default();
        if project_trusted {
            project.discover_roles(cwd);
            project.discover_personas(cwd);
        }
        for (name, role) in roles {
            if role.source_dir.is_none() || !project.roles.contains_key(name) {
                project.roles.insert(name.clone(), role.clone());
            }
        }
        for (name, persona) in personas {
            if persona.source_path.is_none() || !project.personas.contains_key(name) {
                project.personas.insert(name.clone(), persona.clone());
            }
        }
        (project.roles, project.personas)
    }
}
/// Managed MCP connector fetching config (`[managed_mcps]` in config.toml).
///
/// See [`Self::resolve`] for full priority chain.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ManagedMcpsConfig {
    pub enabled: bool,
    pub gateway_tools_enabled: bool,
}
impl Default for ManagedMcpsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gateway_tools_enabled: false,
        }
    }
}
impl ManagedMcpsConfig {
    /// Priority: env var > TOML > remote > default (enabled interactive, disabled headless).
    pub fn resolve(
        config: &toml::Value,
        remote: Option<&crate::util::config::RemoteSettings>,
        is_headless: bool,
    ) -> Self {
        let mut result: Self = config
            .get("managed_mcps")
            .and_then(|v| v.clone().try_into().ok())
            .unwrap_or(Self {
                enabled: !is_headless,
                gateway_tools_enabled: false,
            });
        let managed_mcps_table = config.get("managed_mcps").and_then(|v| v.as_table());
        let has_local_enabled = managed_mcps_table.is_some_and(|t| t.contains_key("enabled"));
        let resolved = crate::agent::config::resolve_enabled(
            None,
            "GROK_MANAGED_MCPS_ENABLED",
            result.enabled,
            has_local_enabled,
            remote.and_then(|r| r.managed_mcps_enabled),
            !is_headless,
        );
        result.enabled = resolved.value;
        let has_local_gateway_tools =
            managed_mcps_table.is_some_and(|t| t.contains_key("gateway_tools_enabled"));
        let gateway_resolved = crate::agent::config::resolve_enabled(
            None,
            "GROK_MANAGED_MCP_GATEWAY_TOOLS_ENABLED",
            result.gateway_tools_enabled,
            has_local_gateway_tools,
            remote.and_then(|r| r.managed_mcp_gateway_tools_enabled),
            false,
        );
        result.gateway_tools_enabled = result.enabled && gateway_resolved.value;
        result
    }
}
/// Auxiliary model overrides under `[models]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub(crate) struct ModelOverrideConfig {
    pub web_search: String,
    /// `None` = current model.
    pub session_summary: Option<String>,
    /// Compiled default (`grok-4.6`) when unset locally, remotely, and via env.
    pub image_description: Option<String>,
    /// Next-prompt suggestion model pin.
    /// Unlike the other overrides this does NOT fill a compiled default; see [`PromptSuggestModelPin`].
    #[serde(skip)]
    pub prompt_suggestion: PromptSuggestModelPin,
}
impl Default for ModelOverrideConfig {
    fn default() -> Self {
        Self {
            web_search: crate::models::default_web_search_model().to_owned(),
            session_summary: None,
            image_description: None,
            prompt_suggestion: PromptSuggestModelPin::Unpinned,
        }
    }
}
/// Resolved model pin for the next-prompt suggestion call (tab-autocomplete ghost text). Precedence is `env > config.toml > remote`; see [`ModelOverrideConfig::resolve`].
/// Unlike the other auxiliary overrides this does not collapse to a plain model string. The consumer (`handle_suggest_prompt`) must distinguish an explicit pin from "unpinned".
/// When unpinned, the client hint wins; otherwise reasoning-disabled sampling uses the alias and reasoning-enabled sampling uses the session model. Every effective model is catalog-guarded. A model missing from the shell's catalog skips the per-turn suggestion request instead of firing one that must fail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PromptSuggestModelPin {
    /// `GROK_PROMPT_SUGGESTIONS_MODEL`: catalog-guarded explicit pin.
    Env(String),
    /// `[models] prompt_suggestion` in config.toml, or the remote `prompt_suggestion_model` (remote settings); catalog-guarded.
    Pinned(String),
    /// No explicit pin: the client hint, then the built-in default apply (both catalog-guarded).
    #[default]
    Unpinned,
}
/// Drop whitespace-only auxiliary model overrides (treat like unset).
fn non_empty_model_override(value: Option<&str>) -> Option<String> {
    value.and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}
impl ModelOverrideConfig {
    /// CLI flag > env var > config.toml > remote settings > compiled default. `image_description` and `session_summary` always resolve to `Some(_)` (default `grok-4.6`), never the session model.
    /// `prompt_suggestion` resolves to a [`PromptSuggestModelPin`] instead of a model string. It has no CLI flag; the default and the catalog guard live at the consumer, `handle_suggest_prompt`.
    pub(crate) fn resolve(
        cli_web_search_model: Option<&str>,
        cli_session_summary_model: Option<&str>,
        config: &toml::Value,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) -> Self {
        let models_table = config.get("models");
        let parsed_models: crate::agent::config::ModelsConfig = models_table
            .and_then(|v| v.clone().try_into().ok())
            .unwrap_or_default();
        let mut result = Self {
            web_search: parsed_models
                .web_search
                .unwrap_or_else(|| crate::models::default_web_search_model().to_owned()),
            session_summary: non_empty_model_override(parsed_models.session_summary.as_deref()),
            image_description: non_empty_model_override(parsed_models.image_description.as_deref()),
            prompt_suggestion: non_empty_model_override(parsed_models.prompt_suggestion.as_deref())
                .map(PromptSuggestModelPin::Pinned)
                .unwrap_or_default(),
        };
        let has_local_ws = models_table.and_then(|m| m.get("web_search")).is_some();
        let has_local_ss = models_table
            .and_then(|m| m.get("session_summary"))
            .is_some();
        let has_local_id = models_table
            .and_then(|m| m.get("image_description"))
            .is_some();
        if let Some(remote) = remote {
            if !has_local_ws && let Some(ref v) = remote.web_search_model {
                result.web_search = v.clone();
            }
            if !has_local_ss {
                result.session_summary =
                    non_empty_model_override(remote.session_summary_model.as_deref());
            }
            if !has_local_id {
                result.image_description =
                    non_empty_model_override(remote.image_description_model.as_deref());
            }
            if result.prompt_suggestion == PromptSuggestModelPin::Unpinned
                && let Some(v) = non_empty_model_override(remote.prompt_suggestion_model.as_deref())
            {
                result.prompt_suggestion = PromptSuggestModelPin::Pinned(v);
            }
        }
        if let Ok(v) = std::env::var("GROK_WEB_SEARCH_MODEL") {
            let v = v.trim();
            if !v.is_empty() {
                result.web_search = v.to_owned();
            }
        }
        if let Ok(v) = std::env::var("GROK_SESSION_SUMMARY_MODEL") {
            result.session_summary = non_empty_model_override(Some(v.as_str()));
        }
        if let Ok(v) = std::env::var("GROK_IMAGE_DESCRIPTION_MODEL") {
            result.image_description = non_empty_model_override(Some(v.as_str()));
        }
        if let Ok(v) = std::env::var("GROK_PROMPT_SUGGESTIONS_MODEL")
            && let Some(v) = non_empty_model_override(Some(v.as_str()))
        {
            result.prompt_suggestion = PromptSuggestModelPin::Env(v);
        }
        if let Some(v) = cli_web_search_model {
            result.web_search = v.to_owned();
        }
        if let Some(v) = cli_session_summary_model {
            result.session_summary = non_empty_model_override(Some(v));
        }
        if result.session_summary.is_none() {
            result.session_summary =
                Some(crate::models::default_session_summary_model().to_owned());
        }
        if result.image_description.is_none() {
            result.image_description =
                Some(crate::models::default_image_description_model().to_owned());
        }
        result
    }
}
/// Raw `[tools.media_gen]` counts; resolve via [`ToolsConfig::resolve_max_parallel_image_gen_calls`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MediaGenToolsConfig {
    #[serde(default)]
    pub max_parallel_image_gen_calls: Option<i64>,
    #[serde(default)]
    pub max_parallel_video_gen_calls: Option<i64>,
}
/// Tool behavior configuration (`[tools]` in config.toml).
/// Controls cross-cutting tool behavior such as `.gitignore` filtering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// When `true`, all tools (including `read_file`) filter gitignored files.
    /// When `false` (default), each tool picks its own default.
    pub respect_gitignore: bool,
    /// Restrict tools whose Grok API requires server-side artifact storage (currently just the video tools).
    /// Without a valid `[tools.zdr_video_output_s3]` bucket they stay advertised but return setup guidance at call time.
    /// Intended for ZDR-bound teams via `~/.grok/managed_config.toml`. Defaults to `false`.
    pub disable_zdr_incompatible_tools: bool,
    /// Optional S3 bucket config for ZDR video output. When present (and valid), video tools presign an upload URL and pass it to the API.
    /// The generated video then lands in a team-owned bucket instead of being downloaded locally. Only effective when `disable_zdr_incompatible_tools` is `true`. Populated from `[tools.zdr_video_output_s3]` in config.
    pub zdr_video_output_s3:
        Option<distill_tools::implementations::distill::video_gen::ZdrVideoOutputS3Config>,
    pub media_gen: MediaGenToolsConfig,
}
impl ToolsConfig {
    pub const ENV_MAX_PARALLEL_IMAGE_GEN_CALLS: &'static str = "GROK_MAX_PARALLEL_IMAGE_GEN_CALLS";
    pub const ENV_MAX_PARALLEL_VIDEO_GEN_CALLS: &'static str = "GROK_MAX_PARALLEL_VIDEO_GEN_CALLS";
    /// Resolve the final tools config, in priority order: Env vars `GROK_RESPECT_GITIGNORE` and `GROK_DISABLE_ZDR_INCOMPATIBLE_TOOLS` (`0`/`false` off, `1`/`true` on). `[tools]` block from the merged effective config.
    /// Defaults (both `false`). Fields are read individually. A malformed `[tools.zdr_video_output_s3]` therefore cannot wipe `disable_zdr_incompatible_tools` or any other tools flag.
    pub fn resolve(config: &toml::Value) -> Self {
        let tools = config.get("tools");
        let mut result = Self {
            respect_gitignore: tools
                .and_then(|t| t.get("respect_gitignore"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            disable_zdr_incompatible_tools: tools
                .and_then(|t| t.get("disable_zdr_incompatible_tools"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            zdr_video_output_s3: tools
                .and_then(|t| t.get("zdr_video_output_s3"))
                .and_then(|s3_val| match s3_val
                    .clone()
                    .try_into::<
                        distill_tools::implementations::distill::video_gen::ZdrVideoOutputS3Config,
                    >()
                {
                    Ok(cfg) if cfg.is_valid() => Some(cfg),
                    Ok(_) => {
                        tracing::warn!(
                                "tools.zdr_video_output_s3 is present but incomplete; ignoring ZDR video output config"
                            );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                                error = %e,
                                "tools.zdr_video_output_s3 failed to parse; ignoring ZDR video output config"
                            );
                        None
                    }
                }),
            media_gen: MediaGenToolsConfig {
                max_parallel_image_gen_calls: tools
                    .and_then(|t| t.get("media_gen"))
                    .and_then(|m| m.get("max_parallel_image_gen_calls"))
                    .and_then(|v| v.as_integer()),
                max_parallel_video_gen_calls: tools
                    .and_then(|t| t.get("media_gen"))
                    .and_then(|m| m.get("max_parallel_video_gen_calls"))
                    .and_then(|v| v.as_integer()),
            },
        };
        match std::env::var("GROK_RESPECT_GITIGNORE").as_deref() {
            Ok("0") | Ok("false") => {
                result.respect_gitignore = false;
            }
            Ok("1") | Ok("true") => {
                result.respect_gitignore = true;
            }
            _ => {}
        }
        match std::env::var("GROK_DISABLE_ZDR_INCOMPATIBLE_TOOLS").as_deref() {
            Ok("0") | Ok("false") => {
                result.disable_zdr_incompatible_tools = false;
            }
            Ok("1") | Ok("true") => {
                result.disable_zdr_incompatible_tools = true;
            }
            _ => {}
        }
        result
    }
    pub(crate) fn resolve_max_parallel_image_gen_calls(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
    ) -> usize {
        resolve_clamped_count(
            Self::ENV_MAX_PARALLEL_IMAGE_GEN_CALLS,
            env,
            config,
            remote,
            distill_tools::media_gen_limits::DEFAULT_MAX_PARALLEL_IMAGE_GEN,
        )
    }
    pub(crate) fn resolve_max_parallel_video_gen_calls(
        env: Option<&str>,
        config: Option<i64>,
        remote: Option<u32>,
    ) -> usize {
        resolve_clamped_count(
            Self::ENV_MAX_PARALLEL_VIDEO_GEN_CALLS,
            env,
            config,
            remote,
            distill_tools::media_gen_limits::DEFAULT_MAX_PARALLEL_VIDEO_GEN,
        )
    }
}
/// Media-gen ladder: env > TOML > remote > default, with every numeric layer clamping `< 1` to `1`.
/// Non-numeric env warns and falls through.
fn resolve_clamped_count(
    env_name: &str,
    env: Option<&str>,
    config: Option<i64>,
    remote: Option<u32>,
    default: usize,
) -> usize {
    if let Some(raw) = env {
        match raw.trim().parse::<i64>() {
            Ok(v) => return clamp_positive_count(v, "env", env_name),
            Err(_) => {
                tracing::warn!(
                    name = env_name,
                    %raw,
                    "invalid env value (expected a whole number); ignoring"
                );
            }
        }
    }
    if let Some(v) = config {
        return clamp_positive_count(v, "config", env_name);
    }
    if let Some(v) = remote {
        return clamp_positive_count(i64::from(v), "remote", env_name);
    }
    default
}
/// Positive whole-number ladder: env > TOML > remote > default.
/// Invalid/non-positive env warns and falls through; TOML/remote `< 1` clamp to 1.
pub(crate) fn resolve_positive_count(
    env_name: &str,
    env: Option<&str>,
    config: Option<i64>,
    remote: Option<u32>,
    default: usize,
) -> usize {
    if let Some(value) = env {
        match distill_tools::util::env::parse_positive(value.trim()) {
            Some(parsed) => return usize::try_from(parsed).unwrap_or(usize::MAX),
            None => {
                tracing::warn!(
                    name = env_name,
                    %value,
                    "invalid env value (expected a positive whole number); ignoring"
                );
            }
        }
    }
    if let Some(v) = config {
        return clamp_positive_count(v, "config", env_name);
    }
    if let Some(v) = remote {
        return clamp_positive_count(i64::from(v), "remote", env_name);
    }
    default
}
fn clamp_positive_count(value: i64, source: &str, name: &str) -> usize {
    if value < 1 {
        tracing::warn!(source, name, value, "positive count < 1; clamping to 1");
        1
    } else {
        usize::try_from(value).unwrap_or(usize::MAX)
    }
}
/// Storage mode for session persistence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StorageMode {
    /// Local JSONL only (default)
    #[default]
    Local,
    /// Local JSONL plus an HTTP flush at end of turn
    Writeback,
}
impl StorageMode {
    /// Resolve from all sources: CLI > env var > remote settings > default (Local).
    pub fn resolve(
        cli_override: Option<&str>,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) -> Self {
        if let Some(mode) = cli_override {
            match mode {
                "writeback" => return Self::Writeback,
                "local" => return Self::Local,
                other => {
                    tracing::warn!(mode = other, "unknown --storage-mode value, ignoring");
                }
            }
        }
        match std::env::var("GROK_STORAGE_MODE").as_deref() {
            Ok("writeback") => return Self::Writeback,
            Ok("local") => return Self::Local,
            _ => {}
        }
        if let Some(remote) = remote
            && remote.writeback_enabled == Some(true)
        {
            return Self::Writeback;
        }
        Self::Local
    }
    /// Resolve from remote settings, enforcing the rule that `Writeback` requires grok.com auth (it syncs session history to the user's account).
    /// This is the single home for that gate.
    /// It is used at boot ([`crate::agent::init`]) and by the post-readiness self-heal (`MvpAgent::reapply_storage_mode`).
    pub(crate) fn from_remote_gated(
        remote: Option<&crate::util::config::RemoteSettings>,
        has_xai_auth: bool,
    ) -> Self {
        match Self::resolve(None, remote) {
            Self::Writeback if !has_xai_auth => Self::Local,
            mode => mode,
        }
    }
}
pub use distill_config::ConfigLayers;
pub use distill_config::{
    GROK_CONFIG_ENV, GROK_CONFIG_PATH_ENV, MDM_REQUIREMENTS_SOURCE, OverlaySource,
    RequirementsLayer, RequirementsSource, ResolvedOverlay, ServingIdentity, SyncMarker,
    claude_managed_settings_probe_path, confirmed_team_switch, confirmed_team_switch_at,
    is_managed_config_hard_stale_for, is_managed_config_stale_for, load_config_file,
    load_from_disk, load_managed_config, load_merged_requirements, load_system_managed_config,
    load_toml_file, managed_config_identity_changed_at, managed_deployment_id,
    managed_policy_compromised_for, mark_managed_config_synced, mark_managed_config_synced_at,
    normalize_identity, requirements_layers, resolved_env_overlay, system_config_dir,
    user_distill_home,
};
/// Map of "dotted.path" to which config file the value came from.
pub(crate) fn config_origins(
    layers: &ConfigLayers,
) -> std::collections::HashMap<String, crate::agent::config::ConfigSource> {
    use crate::agent::config::ConfigSource;
    let mut origins = std::collections::HashMap::new();
    if layers.has_system_managed() {
        walk_toml(
            &layers.system_managed,
            &mut vec![],
            ConfigSource::SystemManagedConfig,
            &mut origins,
        );
    }
    if layers.has_managed() {
        walk_toml(
            &layers.managed,
            &mut vec![],
            ConfigSource::ManagedConfig,
            &mut origins,
        );
    }
    walk_toml(
        &layers.user,
        &mut vec![],
        ConfigSource::UserConfig,
        &mut origins,
    );
    if let Some(overlay) = &layers.env_overlay {
        walk_toml(overlay, &mut vec![], ConfigSource::EnvOverlay, &mut origins);
    }
    origins
}
fn walk_toml(
    value: &toml::Value,
    path: &mut Vec<String>,
    source: crate::agent::config::ConfigSource,
    origins: &mut std::collections::HashMap<String, crate::agent::config::ConfigSource>,
) {
    match value {
        toml::Value::Table(table) => {
            for (k, v) in table {
                path.push(k.clone());
                walk_toml(v, path, source, origins);
                path.pop();
            }
        }
        _ => {
            origins.insert(path.join("."), source);
        }
    }
}
/// The `[skills]` table from an effective config, shared by the reload dispatch and `grok inspect`.
pub(crate) use crate::config::reloader::parse_skills_config;
/// Effective config: the layers plus the campaign overlay (remote cache and `GROK_CAMPAIGNS_OVERRIDE`).
pub use crate::util::config::load_effective_config;
/// Effective config with disk campaigns only, for one-shot entrypoints that never fetch remote settings.
/// This avoids resolving against a never-seeded cache.
pub use crate::util::config::load_effective_config_disk_only;
/// Where a requirement or permission rule was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    Unknown,
    Requirements { path: std::path::PathBuf },
    ManagedSettings { path: std::path::PathBuf },
    Config { path: std::path::PathBuf },
    Settings { path: std::path::PathBuf },
}
impl RequirementSource {
    pub fn path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Unknown => None,
            Self::Requirements { path }
            | Self::ManagedSettings { path }
            | Self::Config { path }
            | Self::Settings { path } => Some(path),
        }
    }
}
impl std::fmt::Display for RequirementSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("<unknown>"),
            Self::Requirements { path } => write!(f, "{} (requirements)", path.display()),
            Self::ManagedSettings { path } => {
                write!(f, "{} (managed-settings)", path.display())
            }
            Self::Config { path } => write!(f, "{} (config)", path.display()),
            Self::Settings { path } => write!(f, "{} (settings)", path.display()),
        }
    }
}
/// A value paired with the source it came from.
#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub source: RequirementSource,
}
/// A config field clamped by requirements.
#[derive(Debug, Clone)]
pub(crate) struct EnforcedField {
    pub path: &'static str,
    pub value: String,
    pub source: RequirementSource,
}
impl std::fmt::Display for EnforcedField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {} ({})", self.path, self.value, self.source)
    }
}
/// Apply overrides from external `managed-settings.json`.
/// Called before `apply_requirements()` so requirements.toml can override.
pub(crate) fn apply_managed_settings_features(
    config: &mut crate::agent::config::Config,
) -> Vec<EnforcedField> {
    let ms = distill_workspace::permission::resolution::managed_settings();
    apply_managed_settings_features_inner(config, &ms.features)
}
fn apply_managed_settings_features_inner(
    config: &mut crate::agent::config::Config,
    features: &distill_workspace::permission::resolution::ManagedSettingsFeatures,
) -> Vec<EnforcedField> {
    let Some(ref path) = features.source_path else {
        return Vec::new();
    };
    let source = RequirementSource::ManagedSettings { path: path.clone() };
    let mut enforced: Vec<EnforcedField> = Vec::new();
    if features.disable_telemetry == Some(true) {
        config.features.telemetry = Some(crate::agent::config::TelemetryMode::Disabled);
        enforced.push(EnforcedField {
            path: "features.telemetry",
            value: "false (DISABLE_TELEMETRY)".to_string(),
            source: source.clone(),
        });
    }
    if features.disable_feedback == Some(true) {
        use crate::agent::config::Feature;
        config.feature_values.insert(Feature::Feedback, false);
        enforced.push(EnforcedField {
            path: Feature::Feedback.path(),
            value: "false (DISABLE_FEEDBACK_COMMAND)".to_string(),
            source: source.clone(),
        });
    }
    enforced
}
/// Load the on-disk config for a one-shot command and clamp it with policy.
/// Without the clamp a pinned value reads as an ordinary config value, which the environment outranks.
pub fn load_agent_config_disk_only() -> Result<crate::agent::config::Config, String> {
    let effective = load_effective_config_disk_only().map_err(|e| e.to_string())?;
    let mut config = crate::agent::config::Config::new_from_toml_cfg(&effective)?;
    apply_policy(&mut config);
    Ok(config)
}
/// Clamp a config with managed settings and then requirements pins, logging each field a policy took over.
/// Requirements run second so a pin wins a conflict.
pub(crate) fn apply_policy(config: &mut crate::agent::config::Config) {
    let managed = apply_managed_settings_features(config);
    let pinned = apply_requirements(config);
    for field in managed.iter().chain(&pinned) {
        tracing::info!(
            field = %field.path, value = %field.value, source = %field.source,
            "policy override"
        );
    }
}
/// Clamp `AgentConfig` fields per `requirements.toml`. No-op if absent.
/// System pins win over user pins on conflict.
pub(crate) fn apply_requirements(config: &mut crate::agent::config::Config) -> Vec<EnforcedField> {
    let enforced: Vec<EnforcedField> = requirements_layers()
        .into_iter()
        .flat_map(|layer| {
            apply_requirements_inner(
                config,
                &layer.value,
                &RequirementSource::Requirements {
                    path: std::path::PathBuf::from(layer.source.label().as_ref()),
                },
            )
        })
        .collect();
    keep_the_deciding_layer(enforced)
}
/// Layers arrive user first, system last, and the last write is the pin that holds.
/// Report that one, so an operator reading the log sees the file that decided rather than the first that asked.
/// Keyed by value as well as path, because one layer can enforce the same path twice for different reasons.
fn keep_the_deciding_layer(mut enforced: Vec<EnforcedField>) -> Vec<EnforcedField> {
    let mut seen = std::collections::HashSet::new();
    enforced.reverse();
    enforced.retain(|field| seen.insert((field.path, field.value.clone())));
    enforced.reverse();
    enforced
}
fn apply_requirements_inner(
    config: &mut crate::agent::config::Config,
    req: &toml::Value,
    source: &RequirementSource,
) -> Vec<EnforcedField> {
    fn req_bool(req: &toml::Value, section: &str, key: &str) -> Option<bool> {
        let value = req.get(section)?.get(key)?;
        let parsed = value.as_bool();
        if parsed.is_none() {
            tracing::error!(
                section,
                key,
                kind = value.type_str(),
                "requirements value is not a boolean; the constraint is not applied"
            );
        }
        parsed
    }
    fn req_str<'a>(req: &'a toml::Value, section: &str, key: &str) -> Option<&'a str> {
        req.get(section)?.get(key)?.as_str()
    }
    enum ReqStrArray {
        Absent,
        Value(Vec<String>),
        Malformed,
    }
    fn req_str_array(req: &toml::Value, section: &str, key: &str) -> ReqStrArray {
        let Some(value) = req.get(section).and_then(|s| s.get(key)) else {
            return ReqStrArray::Absent;
        };
        let Some(arr) = value.as_array() else {
            tracing::error!(
                section,
                key,
                kind = value.type_str(),
                "requirements value is not an array; the constraint fail-closes"
            );
            return ReqStrArray::Malformed;
        };
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            let Some(s) = item.as_str() else {
                tracing::error!(
                    section,
                    key,
                    kind = item.type_str(),
                    "requirements array entry is not a string; the constraint fail-closes"
                );
                return ReqStrArray::Malformed;
            };
            out.push(s.to_owned());
        }
        ReqStrArray::Value(out)
    }
    let mut enforced: Vec<EnforcedField> = Vec::new();
    let mut push = |path: &'static str, value: String| {
        enforced.push(EnforcedField {
            path,
            value,
            source: source.clone(),
        });
    };
    macro_rules! pin_feature {
        ($name:ident) => {
            if let Some(val) = req_bool(req, "features", stringify!($name)) {
                config.requirements.$name.pin(val, source.clone());
                config.features.$name = Some(val);
                // Unconditional, like the registry loop
                // A later layer repeating the pin must report, or the dedupe keeps the first layer that asked instead of the one that decided
                push(concat!("features.", stringify!($name)), format!("{val}"));
            }
        };
    }
    macro_rules! enforce_opt {
        ($section:expr, $key:expr, $field:expr) => {
            if let Some(val) = req_bool(req, $section, $key)
                && $field != Some(val)
            {
                $field = Some(val);
                push(concat!($section, ".", $key), format!("{val}"));
            }
        };
    }
    macro_rules! enforce_val {
        ($section:expr, $key:expr, $field:expr) => {
            if let Some(val) = req_bool(req, $section, $key)
                && $field != val
            {
                $field = val;
                push(concat!($section, ".", $key), format!("{val}"));
            }
        };
    }
    use crate::agent::config::TelemetryMode;
    let req_telemetry_mode = req_str(req, "features", "telemetry")
        .and_then(TelemetryMode::parse)
        .or_else(|| req_bool(req, "features", "telemetry").map(TelemetryMode::from));
    if let Some(mode) = req_telemetry_mode {
        config.requirements.telemetry.pin(mode, source.clone());
        if config.features.telemetry != Some(mode) {
            config.features.telemetry = Some(mode);
            push("features.telemetry", format!("{mode}"));
        }
    }
    macro_rules! pin_requirement_only {
        ($name:ident) => {
            if let Some(val) = req_bool(req, "features", stringify!($name)) {
                config.requirements.$name.pin(val, source.clone());
                push(concat!("features.", stringify!($name)), format!("{val}"));
            }
        };
    }
    pin_feature!(image_gen);
    pin_requirement_only!(image_edit);
    pin_feature!(video_gen);
    for spec in crate::agent::config::FEATURES {
        let Some(value) = req
            .get("features")
            .and_then(|features| features.get(spec.key))
        else {
            continue;
        };
        let Some(val) = value.as_bool() else {
            tracing::error!(
                path = spec.path,
                kind = value.type_str(),
                source = %source,
                "requirements pin is not a boolean; the pin is ignored until the next launch, \
                 which will refuse to start"
            );
            continue;
        };
        config
            .requirements
            .pin_feature(spec.id, val, source.clone());
        push(spec.path, format!("{val}"));
    }
    pin_requirement_only!(remote_fetch);
    pin_requirement_only!(title_refresh);
    if let Some(val) = req_bool(req, "telemetry", "trace_upload") {
        config.requirements.trace_upload.pin(val, source.clone());
        if config.telemetry.trace_upload != Some(val) {
            config.telemetry.trace_upload = Some(val);
            push("telemetry.trace_upload", format!("{val}"));
        }
    }
    enforce_opt!("cli", "auto_update", config.cli.auto_update);
    enforce_opt!("cli", "use_leader", config.cli.use_leader);
    enforce_opt!("cli", "show_tips", config.cli.show_tips);
    enforce_opt!("memory", "enabled", config.memory.enabled);
    enforce_val!("subagents", "enabled", config.subagents.enabled);
    enforce_val!("managed_mcps", "enabled", config.managed_mcps.enabled);
    if let Some(val) = req_bool(req, "tools", "respect_gitignore") {
        config
            .requirements
            .respect_gitignore
            .pin(val, source.clone());
        push("tools.respect_gitignore", format!("{val}"));
    }
    if let Some(val) = req_bool(req, "ui", "yolo") {
        if config.ui.yolo != val {
            config.ui.yolo = val;
            push("ui.yolo", format!("{val}"));
        }
        if !val && config.default_yolo_mode {
            config.default_yolo_mode = false;
            push("ui.yolo", "--yolo blocked".to_string());
        }
    }
    macro_rules! enforce_str {
        ($section:expr, $key:expr, $field:expr) => {
            if let Some(val) = req_str(req, $section, $key)
                && $field.as_deref() != Some(val)
            {
                $field = Some(val.to_owned());
                push(concat!($section, ".", $key), val.to_owned());
            }
        };
        ($section:expr, $key:expr, $field:expr, redacted) => {
            if let Some(val) = req_str(req, $section, $key)
                && $field.as_deref() != Some(val)
            {
                $field = Some(val.to_owned());
                push(concat!($section, ".", $key), "[redacted]".to_owned());
            }
        };
    }
    use crate::agent::config::AllowlistPin;
    macro_rules! pin_str_array {
        ($section:expr, $key:expr, $pin:expr) => {
            match req_str_array(req, $section, $key) {
                ReqStrArray::Absent => {}
                ReqStrArray::Value(val) => {
                    let reported = if val.is_empty() {
                        "(unrestricted)".to_owned()
                    } else {
                        val.join(", ")
                    };
                    $pin.pin(AllowlistPin::List(val), source.clone());
                    push(concat!($section, ".", $key), reported);
                }
                ReqStrArray::Malformed => {
                    $pin.pin(AllowlistPin::FailClosed, source.clone());
                    push(
                        concat!($section, ".", $key),
                        "(invalid; nothing selectable)".to_owned(),
                    );
                }
            }
        };
    }
    enforce_str!("models", "default", config.models.default);
    enforce_str!("models", "web_search", config.models.web_search);
    pin_str_array!(
        "models",
        "allowed_models",
        config.requirements.allowed_models
    );
    enforce_str!("cli", "channel", config.cli.channel);
    enforce_str!("cli", "minimum_version", config.cli.minimum_version);
    enforce_str!("cli", "maximum_version", config.cli.maximum_version);
    enforce_str!(
        "cli",
        "required_minimum_version",
        config.cli.required_minimum_version
    );
    enforce_str!(
        "cli",
        "required_maximum_version",
        config.cli.required_maximum_version
    );
    if let Some(val) = req_str(req, "endpoints", "xai_api_base_url")
        && config.endpoints.xai_api_base_url != val
    {
        config.endpoints.xai_api_base_url = val.to_owned();
        push("endpoints.xai_api_base_url", val.to_owned());
    }
    if let Some(val) = req_str(req, "endpoints", "cli_chat_proxy_base_url")
        && config.endpoints.cli_chat_proxy_base_url.as_deref() != Some(val)
    {
        config.endpoints.cli_chat_proxy_base_url = Some(val.to_owned());
        push("endpoints.cli_chat_proxy_base_url", val.to_owned());
    }
    enforce_str!(
        "endpoints",
        "models_base_url",
        config.endpoints.models_base_url
    );
    enforce_str!(
        "endpoints",
        "models_list_url",
        config.endpoints.models_list_url
    );
    if let Some(val) = req_str(req, "sandbox", "profile") {
        config
            .requirements
            .sandbox_profile
            .pin(val.to_owned(), source.clone());
        if config.sandbox.profile.as_deref() != Some(val) {
            config.sandbox.profile = Some(val.to_owned());
            push("sandbox.profile", val.to_owned());
        }
    }
    if let Some(val) = req_bool(req, "sandbox", "auto_allow_bash") {
        config
            .requirements
            .sandbox_auto_allow_bash
            .pin(val, source.clone());
        if config.sandbox.auto_allow_bash != Some(val) {
            config.sandbox.auto_allow_bash = Some(val);
            push("sandbox.auto_allow_bash", format!("{val}"));
        }
    }
    enforce_str!(
        "endpoints",
        "trace_upload_url",
        config.endpoints.trace_upload_url
    );
    enforce_str!(
        "endpoints",
        "feedback_base_url",
        config.endpoints.feedback_base_url
    );
    enforce_str!(
        "endpoints",
        "deployment_key",
        config.endpoints.deployment_key,
        redacted
    );
    enforce_str!("telemetry", "events_url", config.telemetry.events_url);
    enforce_str!(
        "telemetry",
        "events_api_key",
        config.telemetry.events_api_key,
        redacted
    );
    enforce_val!(
        "telemetry",
        "mixpanel_enabled",
        config.telemetry.mixpanel_enabled
    );
    enforce_str!(
        "telemetry",
        "mixpanel_token",
        config.telemetry.mixpanel_token,
        redacted
    );
    enforce_str!(
        "endpoints",
        "trace_upload_bucket",
        config.endpoints.trace_upload_bucket
    );
    enforce_str!(
        "endpoints",
        "trace_upload_region",
        config.endpoints.trace_upload_region
    );
    enforce_str!(
        "endpoints",
        "trace_upload_credentials_file",
        config.endpoints.trace_upload_credentials_file
    );
    enforce_str!(
        "endpoints",
        "trace_upload_endpoint_url",
        config.endpoints.trace_upload_endpoint_url
    );
    enforce_str!(
        "endpoints",
        "trace_upload_credentials",
        config.endpoints.trace_upload_credentials,
        redacted
    );
    if let Some(val) = req.get("features").and_then(|f| f.get("codebase_indexing")) {
        use crate::agent::config::CodebaseIndexingSetting;
        match val {
            toml::Value::Boolean(b) => {
                config.features.codebase_indexing = CodebaseIndexingSetting::Enabled(*b);
                push("features.codebase_indexing", format!("{b}"));
            }
            toml::Value::Array(_) => {
                if let Ok(patterns) = val.clone().try_into::<Vec<String>>() {
                    push("features.codebase_indexing", format!("{patterns:?}"));
                    config.features.codebase_indexing = CodebaseIndexingSetting::Patterns(patterns);
                }
            }
            _ => {}
        }
    }
    if !enforced.is_empty() {
        tracing::info!(
            enforced = ?enforced.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
            "deployment requirements enforced"
        );
    }
    enforced
}
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
enum BwrapStartup<T> {
    ReexecRequired(T),
    ReexecOptional(T),
    Verify,
    Refuse,
    Continue,
}
#[cfg(target_os = "linux")]
fn route_bwrap_startup<T>(
    command: Option<T>,
    is_inside_bwrap: bool,
    requires_bwrap: bool,
) -> BwrapStartup<T> {
    match command {
        Some(command) if requires_bwrap => BwrapStartup::ReexecRequired(command),
        Some(command) => BwrapStartup::ReexecOptional(command),
        None if requires_bwrap && is_inside_bwrap => BwrapStartup::Verify,
        None if requires_bwrap => BwrapStartup::Refuse,
        None => BwrapStartup::Continue,
    }
}
/// Resolve sandbox profile and apply OS-level enforcement. Called once at startup.
/// `cli_profile` is the resumed/forced base profile (a resumed session's saved profile, or an explicit `--sandbox`).
/// It wins over a fresh env/config read.
pub fn apply_sandbox(
    sandbox_config: Option<&crate::agent::config::SandboxSettingsConfig>,
    cli_profile: Option<&str>,
    cwd: Option<&std::path::Path>,
) {
    let owned;
    let config = match sandbox_config {
        Some(c) => c,
        None => {
            owned = crate::agent::config::SandboxSettingsConfig::from_effective_config();
            &owned
        }
    };
    let req = load_merged_requirements();
    let profile_req = req
        .as_ref()
        .and_then(|v| v.get("sandbox")?.get("profile")?.as_str());
    let auto_allow_req = req
        .as_ref()
        .and_then(|v| v.get("sandbox")?.get("auto_allow_bash")?.as_bool());
    let resolved = config.resolve_profile(cli_profile, profile_req);
    distill_sandbox::set_auto_allow_bash(config.resolve_auto_allow_bash(auto_allow_req).value);
    let sandbox_profile: distill_sandbox::ProfileName =
        resolved.value.parse().unwrap_or_else(|e| {
            eprintln!("warning: {e}, defaulting to no sandbox");
            distill_sandbox::ProfileName::Off
        });
    distill_sandbox::set_configured_profile(&resolved.value);
    let workspace = cwd
        .and_then(|p| dunce::canonicalize(p).ok())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    #[cfg(target_os = "linux")]
    let requires_read_deny = distill_sandbox::requires_read_deny(&sandbox_profile, &workspace);
    #[cfg(target_os = "linux")]
    let requires_hook_write_deny =
        distill_sandbox::requires_hook_write_deny(&sandbox_profile, &workspace);
    #[cfg(target_os = "linux")]
    let requires_data_write_deny =
        distill_sandbox::requires_data_write_deny(&sandbox_profile, &workspace);
    #[cfg(target_os = "linux")]
    let requires_bwrap = requires_read_deny || requires_hook_write_deny || requires_data_write_deny;
    #[cfg(target_os = "linux")]
    {
        let refuse_unprotected = |cause: &str| {
            eprintln!(
                "error: this sandbox could not enforce its deny list on Linux: \
                 {cause} Refusing to start with denied paths unprotected."
            );
        };
        let command = distill_sandbox::bwrap_reexec_for_profile(&sandbox_profile, &workspace);
        match route_bwrap_startup(command, distill_sandbox::is_inside_bwrap(), requires_bwrap) {
            BwrapStartup::ReexecRequired(mut cmd) => {
                use std::os::unix::process::CommandExt;
                let err = cmd.exec();
                refuse_unprotected(&format!(
                    "bwrap exec failed: {err}. Install bubblewrap with \
                     `apt install -y bubblewrap`."
                ));
                std::process::exit(1);
            }
            BwrapStartup::ReexecOptional(mut cmd) => {
                use std::os::unix::process::CommandExt;
                let err = cmd.exec();
                eprintln!(
                    "WARNING: bwrap exec failed: {err}. \
                     Falling back to Landlock sandbox. \
                     Install bubblewrap: apt install -y bubblewrap"
                );
            }
            BwrapStartup::Verify => {
                if requires_hook_write_deny
                    && let Err(e) = distill_sandbox::verify_hook_write_deny_enforced()
                {
                    eprintln!(
                        "error: sandbox reports bwrap but required hook write-deny \
                         mounts are missing or writable ({e}); refusing to start \
                         (possible __GROK_INSIDE_BWRAP spoof)"
                    );
                    std::process::exit(1);
                }
                if requires_read_deny
                    && let Err(e) =
                        distill_sandbox::verify_read_deny_enforced(&sandbox_profile, &workspace)
                {
                    eprintln!(
                        "error: sandbox reports bwrap but required read-deny mounts \
                         are not in effect ({e}); refusing to start \
                         (possible __GROK_INSIDE_BWRAP spoof)"
                    );
                    std::process::exit(1);
                }
                if requires_data_write_deny
                    && let Err(e) = distill_sandbox::verify_data_write_deny_enforced(
                        &sandbox_profile,
                        &workspace,
                    )
                {
                    eprintln!(
                        "error: sandbox reports bwrap but the required /data write-deny \
                         mount is not in effect ({e}); refusing to start \
                         (possible __GROK_INSIDE_BWRAP spoof)"
                    );
                    std::process::exit(1);
                }
            }
            BwrapStartup::Refuse => {
                refuse_unprotected(
                    "the required bwrap plan could not be prepared; see the error above \
                     for the specific cause.",
                );
                std::process::exit(1);
            }
            BwrapStartup::Continue => {}
        }
    }
    if sandbox_profile != distill_sandbox::ProfileName::Off {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let requires_protection = {
            let is_custom = matches!(sandbox_profile, distill_sandbox::ProfileName::Custom(_));
            let needs_hooks =
                distill_sandbox::requires_hook_write_deny(&sandbox_profile, &workspace);
            is_custom || needs_hooks
        };
        let mut sandbox = distill_sandbox::SandboxManager::new(sandbox_profile, &workspace);
        if let Err(e) = sandbox.apply(&workspace) {
            eprintln!("warning: sandbox could not be applied: {e}");
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let unappliable = requires_protection && !sandbox.is_applied();
            if unappliable {
                eprintln!(
                    "error: could not apply the '{}' sandbox profile; see the \
                     warning above for the cause. Refusing to start with its \
                     protections missing.",
                    sandbox.profile()
                );
                std::process::exit(1);
            }
            #[cfg(target_os = "linux")]
            if requires_hook_write_deny
                && distill_sandbox::is_inside_bwrap()
                && let Err(e) = distill_sandbox::verify_hook_write_deny_enforced()
            {
                eprintln!(
                    "error: required hook write-deny mounts not verified after apply ({e}); \
                     refusing to start"
                );
                std::process::exit(1);
            }
        }
        sandbox.install();
    }
}
pub use distill_workspace::project_config::find_project_configs;
/// Resolve the effective `[plugins]` config for a working directory the same way a session does at reload time: global/user config ([`load_effective_config`]), plus every ancestor project `.grok/config.toml` ([`find_project_configs`], extending `paths` and `disabled`), plus the imported `enabledPlugins` merge.
/// All three must discover the same plugins for a given cwd.
/// Centralizing it prevents the paths/disabled/discovered-command drift those callers would otherwise accumulate.
pub(crate) fn resolve_effective_plugins_config(
    cwd: &std::path::Path,
) -> crate::agent::config::PluginsConfig {
    let extract = |toml_val: &toml::Value| -> Option<crate::agent::config::PluginsConfig> {
        toml_val
            .get("plugins")
            .and_then(|v| v.clone().try_into().ok())
    };
    let mut plugins_cfg = load_effective_config()
        .ok()
        .and_then(|t| extract(&t))
        .unwrap_or_default();
    let project_trusted = crate::agent::folder_trust::project_scope_allowed(cwd);
    for config_path in find_project_configs(cwd) {
        if let Ok(toml_val) = load_config_file(&config_path)
            && let Some(proj) = extract(&toml_val)
        {
            if project_trusted {
                plugins_cfg.paths.extend(proj.paths);
            }
            plugins_cfg.disabled.extend(proj.disabled);
        }
    }
    plugins_cfg.merge_claude_enabled_plugins(Some(cwd));
    plugins_cfg
}
pub use distill_config::{deep_merge_toml, expand_env_vars_in_string, expand_env_vars_in_toml};
/// Locked read-modify-write of `~/.grok/config.toml`: the whole window runs under the config-init
/// flock and lands via atomic replace; unchanged configs skip the write.
pub(crate) fn update_config_toml_locked(
    distill_home: &std::path::Path,
    mutate: impl FnOnce(&mut toml::value::Table) -> Result<bool, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = distill_home.join("config.toml");
    let _flock = crate::util::config::acquire_init_lock(distill_home)?;
    let (dest, content) = crate::util::config::read_follow_bound(&config_path)?;
    let mut config: toml::Value = if content.is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(&content).map_err(|e| format!("failed to parse config.toml: {e}"))?
    };
    let table = config
        .as_table_mut()
        .ok_or("config.toml root is not a table")?;
    if !mutate(table)? {
        return Ok(());
    }
    crate::util::config::atomic_write_follow_bound(
        &config_path,
        &dest,
        &toml::to_string_pretty(&config)?,
    )?;
    Ok(())
}
/// Append `value` to the `[plugins].<list>` string array (created if missing)
/// unless already present. Returns whether the config changed.
fn plugins_list_add(
    table: &mut toml::value::Table,
    list: &str,
    value: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    let plugins = table
        .entry("plugins".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or("[plugins] is not a table")?;
    let entries = plugins
        .entry(list.to_string())
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| format!("[plugins].{list} is not an array"))?;
    if entries
        .iter()
        .any(|v| v.as_str().is_some_and(|s| s == value))
    {
        return Ok(false);
    }
    entries.push(toml::Value::String(value.to_string()));
    Ok(true)
}
/// Drop `value` from the `[plugins].<list>` string array; a missing list or
/// entry is a no-op. Returns whether the config changed.
fn plugins_list_remove(table: &mut toml::value::Table, list: &str, value: &str) -> bool {
    let Some(entries) = table
        .get_mut("plugins")
        .and_then(|v| v.as_table_mut())
        .and_then(|p| p.get_mut(list))
        .and_then(|v| v.as_array_mut())
    else {
        return false;
    };
    let before = entries.len();
    entries.retain(|v| v.as_str().is_none_or(|s| s != value));
    entries.len() != before
}
/// Run one `update_config_toml_locked` writer on the blocking pool: the flock poll blocks, so
/// LocalSet callers must hop here. Errors are stringified to cross the spawn boundary.
async fn config_write_blocking<F>(write: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), Box<dyn std::error::Error>> + Send + 'static,
{
    tokio::task::spawn_blocking(move || write().map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("config write task failed: {e}"))?
}
/// Async [`add_plugin_path`] for session callers (see [`config_write_blocking`]).
pub(crate) async fn run_add_plugin_path(path: String) -> Result<(), String> {
    config_write_blocking(move || add_plugin_path(&path)).await
}
/// Async [`remove_plugin_path`] for session callers (see [`config_write_blocking`]).
pub(crate) async fn run_remove_plugin_path(path: String) -> Result<(), String> {
    config_write_blocking(move || remove_plugin_path(&path)).await
}
/// Flip a plugin's `[plugins]` enabled/disabled list pair for session callers (see
/// [`config_write_blocking`]); both writes always run, as the modal arms did inline.
pub(crate) async fn run_set_plugin_enabled(plugin_id: String, enabled: bool) -> Result<(), String> {
    config_write_blocking(move || {
        let (r1, r2) = if enabled {
            (
                add_enabled_plugin(&plugin_id),
                remove_disabled_plugin(&plugin_id),
            )
        } else {
            (
                add_disabled_plugin(&plugin_id),
                remove_enabled_plugin(&plugin_id),
            )
        };
        r1.and(r2)
    })
    .await
}
/// Add a plugin path to `[plugins].paths` in `~/.grok/config.toml`.
/// Deduplicates: if the path is already present, this is a no-op.
pub(crate) fn add_plugin_path(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        plugins_list_add(table, "paths", path)
    })
}
/// Remove a plugin path from `[plugins].paths` in `~/.grok/config.toml`.
///
/// If the path is not found, this is a no-op (returns Ok).
pub(crate) fn remove_plugin_path(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        Ok(plugins_list_remove(table, "paths", path))
    })
}
/// Add a plugin to `[plugins].disabled` in `~/.grok/config.toml`.
/// Deduplicates: if already present, this is a no-op.
pub fn add_disabled_plugin(plugin_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        plugins_list_add(table, "disabled", plugin_id)
    })
}
/// Remove a plugin from `[plugins].disabled` in `~/.grok/config.toml`.
///
/// If the plugin is not in the disabled list, this is a no-op.
pub fn remove_disabled_plugin(plugin_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        Ok(plugins_list_remove(table, "disabled", plugin_id))
    })
}
/// Async [`add_dismissed_plugin_cta`] for UI callers (see [`config_write_blocking`]): the locked
/// write sleep-polls the config-init flock, so it must stay off the render path.
pub async fn run_add_dismissed_plugin_cta(plugin_id: String) -> Result<(), String> {
    config_write_blocking(move || add_dismissed_plugin_cta(&plugin_id)).await
}
/// Add a plugin to `[plugin_cta].dismissed` in `~/.grok/config.toml`.
/// Creates the `[plugin_cta]` section and `dismissed` array if they don't exist.
/// Deduplicates: if already present, this is a no-op.
pub fn add_dismissed_plugin_cta(plugin_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = crate::util::distill_home::distill_home().join("config.toml");
    add_dismissed_plugin_cta_to_file(plugin_id, &config_path)
}
/// Add a dismissed plugin CTA to a specific config file (path-parameterized for tests); runs
/// under the config-init flock with an atomic replace like every config.toml writer.
#[doc(hidden)]
pub fn add_dismissed_plugin_cta_to_file(
    plugin_id: &str,
    config_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let distill_home = config_path
        .parent()
        .ok_or("config.toml path has no parent directory")?;
    update_config_toml_locked(distill_home, |table| {
        let plugin_cta = table
            .entry("plugin_cta".to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            .as_table_mut()
            .ok_or("[plugin_cta] is not a table")?;
        let dismissed = plugin_cta
            .entry("dismissed".to_string())
            .or_insert_with(|| toml::Value::Array(vec![]))
            .as_array_mut()
            .ok_or("[plugin_cta].dismissed is not an array")?;
        if dismissed
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s == plugin_id))
        {
            return Ok(false);
        }
        dismissed.push(toml::Value::String(plugin_id.to_string()));
        Ok(true)
    })
}
/// All plugin ids listed in `[plugin_cta].dismissed` in `~/.grok/config.toml`.
///
/// Read once (e.g. on catalog load) and cached so the matched-debounce recompute doesn't parse the config from disk on the UI thread.
pub fn dismissed_plugin_ctas() -> std::collections::HashSet<String> {
    let config_path = crate::util::distill_home::distill_home().join("config.toml");
    dismissed_plugin_ctas_in_file(&config_path)
}
/// Read the dismissed plugin CTA set from a specific config file (for tests).
#[doc(hidden)]
pub fn dismissed_plugin_ctas_in_file(
    config_path: &std::path::Path,
) -> std::collections::HashSet<String> {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return std::collections::HashSet::new();
    };
    let Ok(config) = toml::from_str::<toml::Value>(&content) else {
        return std::collections::HashSet::new();
    };
    config
        .as_table()
        .and_then(|t| t.get("plugin_cta"))
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("dismissed"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
/// Validate that a hook path is safe to add to `~/.grok/hooks-paths`.
/// CWE-427: Only paths under `~/.grok/` are allowed to prevent arbitrary hook path injection that bypasses the project trust gate.
/// Paths are canonicalized (resolving symlinks and `..`) before checking.
pub(crate) fn validate_hooks_path(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let candidate = std::path::Path::new(path);
    if !candidate.is_absolute() {
        return Err("Hook path must be absolute.".into());
    }
    let distill_home = crate::util::distill_home::distill_home();
    let canonical = dunce::canonicalize(candidate)
        .or_else(|_| {
            let mut base = candidate.to_path_buf();
            let mut tail = Vec::new();
            while !base.exists() {
                if let Some(file_name) = base.file_name() {
                    tail.push(file_name.to_os_string());
                    base.pop();
                } else {
                    break;
                }
            }
            let mut resolved = dunce::canonicalize(&base)?;
            for component in tail.into_iter().rev() {
                resolved.push(component);
            }
            Ok(resolved)
        })
        .map_err(|e: std::io::Error| format!("Cannot resolve hook path: {e}"))?;
    let canonical_home =
        dunce::canonicalize(&distill_home).unwrap_or_else(|_| distill_home.clone());
    if !canonical.starts_with(&canonical_home) {
        return Err(format!(
            "Hook path must be under ~/.grok/ ({}). Got: {}",
            canonical_home.display(),
            canonical.display()
        )
        .into());
    }
    Ok(())
}
/// Post-install steps for a newly installed plugin repo.
/// Auto-enables all plugins in the repo so they are active after the next reload.
/// Returns `(plugin_names, warnings)` for status messaging.
pub(crate) fn post_install_plugin(repo_key: &str) -> (Vec<String>, Vec<String>) {
    let registry = distill_agent::plugins::InstallRegistry::load();
    let Some(repo) = registry.get_repo(repo_key) else {
        return (
            vec![],
            vec![format!("repo not found in registry: {repo_key}")],
        );
    };
    let names: Vec<String> = repo.plugins.keys().cloned().collect();
    let warnings = auto_enable_plugins(&names);
    (names, warnings)
}
/// Auto-enable each plugin in `[plugins].enabled`, returning warnings for failures; takes the
/// config-init flock per write — callers must not hold the registry flock (init ⊃ registry).
pub(crate) fn auto_enable_plugins(names: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    for name in names {
        if let Err(e) = add_enabled_plugin(name) {
            warnings.push(format!("auto-enable {name}: {e}"));
        }
    }
    warnings
}
/// Add a plugin to `[plugins].enabled` in `~/.grok/config.toml`.
/// Used for project-scope plugins that are disabled by default.
/// Deduplicates: if already present, this is a no-op.
pub fn add_enabled_plugin(plugin_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        plugins_list_add(table, "enabled", plugin_id)
    })
}
/// Remove a plugin from `[plugins].enabled` in `~/.grok/config.toml`.
pub fn remove_enabled_plugin(plugin_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    update_config_toml_locked(&crate::util::distill_home::distill_home(), |table| {
        Ok(plugins_list_remove(table, "enabled", plugin_id))
    })
}
/// Add a hook path to `~/.grok/hooks-paths` (one path per line).
/// If the path is already present (exact string match), this is a no-op.
/// CWE-427: The path is validated to be under `~/.grok/` before writing.
pub(crate) fn add_hooks_path(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    validate_hooks_path(path)?;
    add_hooks_path_to_file(
        path,
        &crate::util::distill_home::distill_home().join("hooks-paths"),
    )
}
/// Add a hook path to a specific file (for tests).
pub(crate) fn add_hooks_path_to_file(
    path: &str,
    paths_file: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = paths_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(paths_file).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == path) {
        return Ok(());
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths_file)?;
    writeln!(file, "{}", path)?;
    Ok(())
}
/// The user-registered hook directories (`~/.grok/hooks-paths` lines) —
/// exactly what `remove_hooks_path` can remove (same exact-string match).
pub(crate) fn registered_hook_paths() -> std::collections::HashSet<String> {
    let path = crate::util::distill_home::distill_home().join("hooks-paths");
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    }
}
/// Remove a hook path from `~/.grok/hooks-paths`.
/// Returns whether the path was present (exact string match, like `add_hooks_path`).
/// On `false` nothing was removed and callers must not claim success.
pub(crate) fn remove_hooks_path(path: &str) -> Result<bool, Box<dyn std::error::Error>> {
    remove_hooks_path_from_file(
        path,
        &crate::util::distill_home::distill_home().join("hooks-paths"),
    )
}
/// Remove a hook path from a specific file (for tests).
pub(crate) fn remove_hooks_path_from_file(
    path: &str,
    paths_file: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let content = match std::fs::read_to_string(paths_file) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut found = false;
    let new_lines: Vec<&str> = content
        .lines()
        .filter(|l| {
            if l.trim() == path {
                found = true;
                false
            } else {
                true
            }
        })
        .collect();
    if !found {
        return Ok(false);
    }
    if let Some(parent) = paths_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        paths_file,
        new_lines.join("\n") + (if new_lines.is_empty() { "" } else { "\n" }),
    )?;
    Ok(true)
}
#[cfg(test)]
mod tests;
