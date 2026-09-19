// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-level telemetry helpers: product analytics and harness snapshots.

mod active_agent_message;
mod permission;

pub(crate) use active_agent_message::*;
pub(crate) use permission::*;

use distill_telemetry::events::SessionHarness;
use distill_tools::implementations::skills::types::SkillScope;

/// `duration_ms`, `tool_count`, and `error_type` are status-specific; pass `None` when not applicable.
pub(crate) fn emit_mcp_connection_span(
    status: &str,
    server_name: &str,
    transport_type: &str,
    server_scope: &str,
    duration_ms: Option<i64>,
    tool_count: Option<i64>,
    error_type: Option<&str>,
) {
    let span = tracing::info_span!(
        "mcp.server_connection",
        status,
        server_name,
        transport_type,
        server_scope,
        duration_ms = tracing::field::Empty,
        tool_count = tracing::field::Empty,
        error_type = tracing::field::Empty,
    );
    if let Some(d) = duration_ms {
        span.record("duration_ms", d);
    }
    if let Some(t) = tool_count {
        span.record("tool_count", t);
    }
    if let Some(e) = error_type {
        span.record("error_type", e);
    }
    span.in_scope(|| {});
}

/// Plugin id is `plugin_source`; SkillScope on a plugin skill is install location.
pub(crate) fn skill_source(scope: SkillScope, plugin_name: Option<&str>) -> &'static str {
    if plugin_name.is_some() {
        return "plugin";
    }
    match scope {
        SkillScope::Local => "local",
        SkillScope::Repo => "repo",
        SkillScope::User => "user",
        SkillScope::Server => "server",
        SkillScope::Bundled => "bundled",
        SkillScope::Plugin => "plugin",
    }
}

/// Canonicalizes both paths; one that cannot be canonicalized (synthetic paths like `chat-product://`) matches only when identical.
pub(crate) fn is_same_skill_file(
    skill_path: &std::path::Path,
    read_path: &std::path::Path,
) -> bool {
    if skill_path == read_path {
        return true;
    }
    match (
        dunce::canonicalize(skill_path),
        dunce::canonicalize(read_path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

pub(crate) fn format_hook_name(spec: &distill_hooks::config::HookSpec) -> String {
    let scope = spec.name.split(':').next().unwrap_or("unknown");
    match spec.configured_matcher.as_deref() {
        Some(m) if !m.is_empty() => format!("{scope}:{}:{}", spec.event, m.to_lowercase()),
        _ => format!("{scope}:{}", spec.event),
    }
}

/// Provenance for telemetry, mapped from the shared [`hook_origin`] classifier so this and `/hooks` inspect can't diverge.
fn format_hook_source(spec: &distill_hooks::config::HookSpec) -> &'static str {
    use distill_hooks::config::HookOrigin as O;
    match distill_hooks::config::hook_origin(spec) {
        O::SystemManaged | O::Managed => "managedConfig",
        O::Requirements => "requirementsConfig",
        O::UserConfig => "userConfig",
        O::UserFile => "userSettings",
        O::ProjectFile => "projectSettings",
        O::Plugin => "pluginHook",
        O::Agent => "agentHook",
        O::Unknown => "unknown",
    }
}

/// Per-hook inventory recorded as a `hook.registered` span at session start.
pub(crate) struct HookRegInfo {
    pub name: String,
    pub event: String,
    pub hook_type: String,
    pub source: &'static str,
}

impl HookRegInfo {
    pub(crate) fn from_spec(spec: &distill_hooks::config::HookSpec) -> Self {
        Self {
            name: format_hook_name(spec),
            event: spec.event.to_string(),
            hook_type: spec.handler_type.as_ref().to_string(),
            source: format_hook_source(spec),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SessionHarnessMetrics {
    pub session_id: String,
    pub client_identifier: Option<String>,
    pub model_id: String,
    pub agent_name: String,
    pub permission_mode: distill_telemetry::enums::PermissionMode,
    pub mcp_server_names: Vec<String>,
    pub lsp_server_names: Vec<String>,
    pub memory_enabled: bool,
    pub memory_retrieval_mode: distill_telemetry::events::MemoryRetrievalMode,
    pub auto_update: Option<bool>,
    pub cwd: String,
    /// Filled from the built agent's bridge so `into_event` doesn't re-walk the disk.
    pub skill_names: Vec<String>,
    /// Resolved vendor-compat config, so recorded AGENTS.md names match what the session actually discovers.
    pub compat: distill_tools::types::compat::CompatConfig,
    /// `[paths]` config, for the same reason as `compat`.
    pub paths_config: distill_agent::prompt::paths::PathsConfig,
    pub plugin_registry: Option<std::sync::Arc<distill_agent::plugins::PluginRegistry>>,
    pub plugin_names: Vec<String>,
}

impl SessionHarnessMetrics {
    pub(crate) async fn into_event(self, hooks: Vec<HookRegInfo>) -> SessionHarness {
        // One `plugin.loaded` span per enabled plugin at session start.
        if let Some(registry) = self.plugin_registry.as_deref() {
            for plugin in registry.enabled_plugins() {
                distill_telemetry::event_span!(
                    "plugin.loaded",
                    plugin_name = %plugin.name,
                    plugin_version = %plugin.version.as_deref().unwrap_or(""),
                    plugin_scope = plugin.scope.id_label(),
                    has_hooks = plugin.has_hooks,
                    has_mcp = plugin.mcp_server_count > 0,
                    skill_count = plugin.skill_count as i64,
                    agent_count = plugin.agent_count as i64,
                    command_path_count = plugin.command_dirs.len() as i64,
                );
            }
        }

        // One `hook.registered` span per configured hook at session start.
        for h in &hooks {
            distill_telemetry::event_span!(
                "hook.registered",
                hook_name = %h.name,
                hook_event = %h.event,
                hook_type = %h.hook_type,
                hook_source = %h.source,
            );
        }
        let hook_names: Vec<String> = hooks.into_iter().map(|h| h.name).collect();

        let agents_md_dir_names = distill_agent::prompt::agents_md::read_agents_config_with_paths(
            &self.cwd,
            self.compat,
            &self.paths_config,
            crate::agent::folder_trust::project_scope_allowed(std::path::Path::new(&self.cwd)),
        )
        .await
        .iter()
        .filter_map(|f| {
            std::path::Path::new(&f.file_path)
                .parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .collect();
        SessionHarness {
            session_id: self.session_id,
            client_identifier: self.client_identifier,
            model_id: self.model_id,
            agent_name: self.agent_name,
            permission_mode: self.permission_mode,
            mcp_server_names: self.mcp_server_names,
            plugin_names: self.plugin_names,
            skill_names: self.skill_names,
            lsp_server_names: self.lsp_server_names,
            hook_names,
            agents_md_dir_names,
            memory_enabled: self.memory_enabled,
            memory_retrieval_mode: self.memory_retrieval_mode,
            // Same signal `SessionNew` carries; recomputed here because this event is built off-thread, after spawn (cheap: repo discovery)
            is_git_repo: distill_telemetry::context::collect_git_context(&self.cwd).is_git_repo,
            auto_update: self.auto_update,
        }
    }
}

#[cfg(test)]
mod is_same_skill_file_tests {
    use super::is_same_skill_file;
    use std::path::Path;

    #[test]
    fn matches_identical_paths() {
        assert!(is_same_skill_file(
            Path::new("/home/u/.grok/skills/review/SKILL.md"),
            Path::new("/home/u/.grok/skills/review/SKILL.md")
        ));
    }

    #[test]
    fn rejects_a_different_skill() {
        assert!(!is_same_skill_file(
            Path::new("/home/u/.grok/skills/review/SKILL.md"),
            Path::new("/home/u/.grok/skills/design/SKILL.md")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn matches_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let skill = real.join("SKILL.md");
        std::fs::write(&skill, "---\nname: x\n---\n").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert!(is_same_skill_file(&skill, &link.join("SKILL.md")));
    }

    #[test]
    fn synthetic_product_path_does_not_match_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("SKILL.md");
        std::fs::write(&skill, "body").unwrap();

        assert!(!is_same_skill_file(
            Path::new("chat-product://commit"),
            &skill
        ));
    }
}

#[cfg(test)]
mod skill_source_tests {
    use super::skill_source;
    use distill_tools::implementations::skills::types::SkillScope;

    #[test]
    fn plugin_name_overrides_install_location_scope() {
        assert_eq!("plugin", skill_source(SkillScope::User, Some("acme")));
        assert_eq!("bundled", skill_source(SkillScope::Bundled, None));
        assert_eq!("plugin", skill_source(SkillScope::Plugin, None));
    }
}
