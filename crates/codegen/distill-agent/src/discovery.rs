// Modified for Distill by Samuel Fajreldines, 2026.
//! Searches `.grok/agents/` and `.claude/agents/` from cwd to repo root,
//! then `~/.grok/agents/`, then `~/.claude/agents/`. Name-based dedup keeps
//! highest priority.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use distill_tools::types::config_source::ConfigSource;

use crate::config::{AgentDefinition, AgentScope, BuiltinAgentName};
use crate::error::AgentBuildError;
use crate::prompt::context::TemplateOverride;

/// Project-level agent directories to scan (`.grok/agents/` plus `.claude/agents/` for compat).
const PROJECT_AGENT_SUBDIRS: &[&str] = &[".grok/agents", ".claude/agents"];

/// Existing project-level agent dirs (`.grok/agents` / `.claude/agents`), walked from `cwd` up to the git worktree root (inclusive).
/// Returns `(existing dirs, git_root)`.
/// Mirrors [`crate::plugins::project_plugin_dirs`].
pub fn project_agent_dirs(cwd: Option<&Path>) -> (Vec<PathBuf>, Option<PathBuf>) {
    let Some(cwd) = cwd else {
        return (Vec::new(), None);
    };
    let chain = crate::repo::RepoDirChain::resolve(cwd);
    (project_agent_dirs_in(&chain.dirs), chain.git_root)
}

/// Existing project agent dirs under each dir of a cwd-to-git-root chain.
/// The only place that walks `PROJECT_AGENT_SUBDIRS`. The folder-trust detector reuses it so detection cannot drift from discovery.
pub fn project_agent_dirs_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    crate::repo::existing_subdirs_along(chain_dirs, PROJECT_AGENT_SUBDIRS)
}

// ── Subagent entry types ─────────────────────────────────────────────

/// A subagent entry for the Task tool description and spawn-time validation.
#[derive(Debug, Clone)]
pub struct SubagentEntry {
    pub name: String,
    pub description: String,
    pub source: SubagentSource,
    /// If this entry shadows a built-in, which one.
    pub shadows_builtin: Option<BuiltinAgentName>,
    pub config_source: ConfigSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentSource {
    /// One of the 3 built-in subagent types, not shadowed by a user agent.
    Builtin(BuiltinAgentName),
    /// User-defined agent from project, user, or bundled discovery.
    UserDefined { scope: AgentScope },
}

// ── all_subagents ────────────────────────────────────────────────────

/// Build the complete list of enabled subagents: built-ins, then discovered user agents, minus toggles.
/// Project-level agents shadow built-ins. User-level and bundled agents with built-in names are skipped, keeping `visible == callable`.
pub fn all_subagents(cwd: &Path, toggle: &HashMap<String, bool>) -> Vec<SubagentEntry> {
    let grok = distill_config::user_distill_home();
    all_subagents_with_home(
        cwd,
        toggle,
        distill_dirs::home_dir().as_deref(),
        grok.as_deref(),
    )
}

fn all_subagents_with_home(
    cwd: &Path,
    toggle: &HashMap<String, bool>,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Vec<SubagentEntry> {
    let discovered = discover_with_home(cwd, home, distill_home);
    merge_subagents(discovered, toggle)
}

/// Internal: merge discovered agents with built-in subagents, apply toggles.
///
/// Separated from `all_subagents()` for testability without filesystem access.
fn merge_subagents(
    discovered: Vec<AgentDefinition>,
    toggle: &HashMap<String, bool>,
) -> Vec<SubagentEntry> {
    fn discovered_scope_priority(scope: AgentScope) -> usize {
        match scope {
            AgentScope::Project => 3,
            AgentScope::User => 2,
            AgentScope::Bundled => 1,
            AgentScope::BuiltIn => 0,
        }
    }

    // 1. Seed with built-in subagents
    let mut entries: Vec<SubagentEntry> = BuiltinAgentName::subagent_variants()
        .iter()
        .map(|b| {
            let def = b.definition();
            SubagentEntry {
                name: def.name,
                description: def.description,
                source: SubagentSource::Builtin(*b),
                shadows_builtin: None,
                config_source: ConfigSource::Builtin,
            }
        })
        .collect();

    // Only project-level agents can shadow built-ins. Matches spawn precedence: project > built-in > user > bundled.
    // A user-level explore.md must not shadow built-in explore in the visible list, or `visible == callable` breaks.
    for def in discovered {
        if def.scope == AgentScope::BuiltIn {
            continue;
        }

        let is_builtin_name = BuiltinAgentName::from_str(&def.name)
            .ok()
            .filter(|b| BuiltinAgentName::subagent_variants().contains(b));

        if is_builtin_name.is_some() && def.scope != AgentScope::Project {
            // This user-level agent has the same name as a built-in subagent, so skip it
            // It cannot shadow the built-in at runtime, so don't let it shadow in the visible list
            continue;
        }

        if let Some(pos) = entries.iter().position(|e| e.name == def.name) {
            let should_replace = match entries.get(pos).map(|e| &e.source) {
                Some(SubagentSource::Builtin(_)) => true,
                Some(SubagentSource::UserDefined { scope }) => {
                    discovered_scope_priority(def.scope) > discovered_scope_priority(*scope)
                }
                None => false,
            };
            if should_replace {
                let cs = source_from_agent_def(&def);
                if let Some(slot) = entries.get_mut(pos) {
                    *slot = SubagentEntry {
                        name: def.name,
                        description: def.description,
                        source: SubagentSource::UserDefined { scope: def.scope },
                        shadows_builtin: is_builtin_name,
                        config_source: cs,
                    };
                }
            }
        } else {
            // This name is new, so append it after the built-ins
            let cs = source_from_agent_def(&def);
            entries.push(SubagentEntry {
                name: def.name,
                description: def.description,
                source: SubagentSource::UserDefined { scope: def.scope },
                shadows_builtin: None,
                config_source: cs,
            });
        }
    }

    // 3. Filter by toggle (omitted means enabled)
    entries
        .into_iter()
        .filter(|e| toggle.get(&e.name).copied().unwrap_or(true))
        .collect()
}

/// Discover agent definitions from the filesystem. Deduplicates by name; higher priority wins.
/// Order: project `.grok/agents/` (cwd up to repo root), user `~/.grok`, compat `~/.claude`, then bundled.
/// `.grok` dirs resolve from `distill_home` plus legacy `~/.grok` when `GROK_HOME` points elsewhere.
pub(crate) fn user_agent_dirs(
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Vec<(std::path::PathBuf, AgentScope)> {
    // Legacy literal ~/.grok, included only when it differs from distill_home
    // (i.e. GROK_HOME points elsewhere) so agents left in the old location are
    // still discovered and stay consistent with scope_from_path classification.
    let legacy_grok = home
        .map(|h| h.join(".grok"))
        .filter(|legacy| distill_home != Some(legacy.as_path()));

    let mut dirs = Vec::new();
    if let Some(g) = distill_home {
        dirs.push((g.join("agents"), AgentScope::User));
    }
    if let Some(l) = &legacy_grok {
        dirs.push((l.join("agents"), AgentScope::User));
    }
    if let Some(h) = home {
        dirs.push((h.join(".claude").join("agents"), AgentScope::User));
    }
    if let Some(g) = distill_home {
        dirs.push((g.join("bundled").join("agents"), AgentScope::Bundled));
    }
    if let Some(l) = &legacy_grok {
        dirs.push((l.join("bundled").join("agents"), AgentScope::Bundled));
    }
    dirs
}

pub fn discover(cwd: &Path) -> Vec<AgentDefinition> {
    let grok = distill_config::user_distill_home();
    discover_with_home(cwd, distill_dirs::home_dir().as_deref(), grok.as_deref())
}

fn discover_with_home(
    cwd: &Path,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Vec<AgentDefinition> {
    let mut definitions = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    load_project_definitions(cwd, &mut definitions, &mut seen_names);

    for (dir, scope) in user_agent_dirs(home, distill_home) {
        if dir.is_dir() {
            load_definitions_from_dir(&dir, scope, &mut definitions, &mut seen_names);
        }
    }

    definitions
}

/// Checks built-ins first, then user-level dirs, then bundled.
pub fn by_name(name: &str) -> Option<AgentDefinition> {
    let grok = distill_config::user_distill_home();
    by_name_with_home(name, distill_dirs::home_dir().as_deref(), grok.as_deref())
}

fn by_name_with_home(
    name: &str,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Option<AgentDefinition> {
    if let Ok(builtin) = BuiltinAgentName::from_str(name) {
        return Some(builtin.definition());
    }

    {
        let home_dirs = user_agent_dirs(home, distill_home);
        for (agents_dir, scope) in home_dirs {
            if let Some(def) = load_definition_by_name(
                &agents_dir,
                name,
                "Failed to parse agent definition",
                Some(scope),
            ) {
                return Some(def);
            }
        }
    }

    None
}

/// Project-level `.grok/agents/` has highest priority, then falls back to built-ins, user-level, and finally bundled definitions.
pub fn by_name_in_cwd(name: &str, cwd: &Path) -> Option<AgentDefinition> {
    let grok = distill_config::user_distill_home();
    by_name_in_cwd_with_home(name, cwd, distill_dirs::home_dir().as_deref(), grok.as_deref())
}

fn by_name_in_cwd_with_home(
    name: &str,
    cwd: &Path,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Option<AgentDefinition> {
    if let Some(def) = load_project_definition_by_name(name, cwd) {
        return Some(def);
    }

    by_name_with_home(name, home, distill_home)
}

/// These are the pre-defined agent profiles (`general-purpose`, `explore`, `plan`) that the Task tool can launch.
/// User/project-level agent files can shadow these by name.
pub fn builtin_subagents() -> Vec<AgentDefinition> {
    BuiltinAgentName::subagent_variants()
        .iter()
        .map(|name| name.definition())
        .collect()
}

/// Return every built-in agent definition (all `BuiltinAgentName` variants, not just the subagent-launchable subset in [`builtin_subagents`]).
///
/// For cross-crate coverage and manifest checks: those crates pin a different `strum`, so they cannot call `BuiltinAgentName::iter()` themselves.
pub fn all_builtin_agent_definitions() -> Vec<AgentDefinition> {
    use strum::IntoEnumIterator;
    BuiltinAgentName::iter()
        .map(BuiltinAgentName::definition)
        .collect()
}

/// Parse only YAML frontmatter from an agent file, without loading the body.
pub fn parse_agent_frontmatter_only(path: &Path) -> Result<AgentDefinition, AgentBuildError> {
    AgentDefinition::from_file_frontmatter_only(path)
}

fn source_from_agent_def(def: &AgentDefinition) -> ConfigSource {
    let path = def.source_path.clone().unwrap_or_default();
    if let Some(ref pn) = def.plugin_name {
        ConfigSource::Plugin {
            plugin_name: pn.clone(),
            path,
        }
    } else {
        match def.scope {
            AgentScope::Project => ConfigSource::Project { path },
            AgentScope::User => ConfigSource::User { path },
            AgentScope::Bundled | AgentScope::BuiltIn => ConfigSource::Builtin,
        }
    }
}

// ── Plugin-aware variants ─────────────────────────────────────────────

/// One plugin-provided agent, addressable by its qualified `plugin:agent` name.
#[derive(Debug)]
pub struct PluginAgent {
    /// Qualified `plugin-name:agent-name` used to spawn (and toggle) the agent.
    pub qualified_name: String,
    /// Owning plugin's scope mapped to the agent scope model (project or user).
    pub scope: AgentScope,
    /// Parsed definition (`plugin_name` is set; `name` stays unqualified).
    pub definition: AgentDefinition,
}

/// Enumerate all agents provided by enabled plugins.
/// Loads every `*.md` in each enabled plugin's agent dirs.
/// Untrusted plugins are parsed frontmatter-only.
pub fn plugin_agents(registry: &crate::plugins::PluginRegistry) -> Vec<PluginAgent> {
    let mut agents = Vec::new();
    for plugin in registry.enabled_plugins() {
        for agent_dir in &plugin.agent_dirs {
            let Ok(entries) = std::fs::read_dir(agent_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let Some(def) = load_plugin_agent_definition(plugin, &path) else {
                    continue;
                };
                let scope = match plugin.scope {
                    crate::plugins::PluginScope::Project => AgentScope::Project,
                    _ => AgentScope::User,
                };
                agents.push(PluginAgent {
                    qualified_name: format!("{}:{}", plugin.name, def.name),
                    scope,
                    definition: def,
                });
            }
        }
    }
    agents
}

/// Build the complete list of enabled subagents, including plugin agents.
pub fn all_subagents_with_plugins(
    cwd: &Path,
    toggle: &HashMap<String, bool>,
    plugins: Option<&crate::plugins::PluginRegistry>,
) -> Vec<SubagentEntry> {
    let grok = distill_config::user_distill_home();
    all_subagents_with_plugins_and_home(
        cwd,
        toggle,
        plugins,
        distill_dirs::home_dir().as_deref(),
        grok.as_deref(),
    )
}

fn all_subagents_with_plugins_and_home(
    cwd: &Path,
    toggle: &HashMap<String, bool>,
    plugins: Option<&crate::plugins::PluginRegistry>,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Vec<SubagentEntry> {
    let discovered = discover_with_home(cwd, home, distill_home);
    let mut entries = merge_subagents(discovered, toggle);

    // Append plugin agents under qualified names
    if let Some(registry) = plugins {
        for agent in plugin_agents(registry) {
            // Skip if a native entry already has this qualified name
            if entries.iter().any(|e| e.name == agent.qualified_name) {
                continue;
            }

            // Toggles key on the qualified name (same name the list shows).
            if !toggle.get(&agent.qualified_name).copied().unwrap_or(true) {
                continue;
            }

            let config_source = ConfigSource::Plugin {
                plugin_name: agent.definition.plugin_name.clone().unwrap_or_default(),
                path: agent.definition.source_path.clone().unwrap_or_default(),
            };
            entries.push(SubagentEntry {
                name: agent.qualified_name,
                description: agent.definition.description,
                source: SubagentSource::UserDefined { scope: agent.scope },
                shadows_builtin: None,
                config_source,
            });
        }
    }

    entries
}

/// Checks project-level, built-ins, user-level, bundled, then plugin agents.
/// For plugin agents, the name can be qualified (e.g. `my-plugin:reviewer`).
pub fn by_name_in_cwd_with_plugins(
    name: &str,
    cwd: &Path,
    plugins: Option<&crate::plugins::PluginRegistry>,
) -> Option<AgentDefinition> {
    let grok = distill_config::user_distill_home();
    by_name_in_cwd_with_plugins_and_home(
        name,
        cwd,
        plugins,
        distill_dirs::home_dir().as_deref(),
        grok.as_deref(),
    )
}

fn by_name_in_cwd_with_plugins_and_home(
    name: &str,
    cwd: &Path,
    plugins: Option<&crate::plugins::PluginRegistry>,
    home: Option<&Path>,
    distill_home: Option<&Path>,
) -> Option<AgentDefinition> {
    // First try native resolution (project > built-in > user > bundled)
    if let Some(def) = by_name_in_cwd_with_home(name, cwd, home, distill_home) {
        return Some(def);
    }

    // Try plugin agents
    if let Some(registry) = plugins {
        // Check if name is qualified (plugin-name:agent-name)
        if let Some((plugin_name, agent_name)) = name.split_once(':')
            && let Some(plugin) = registry.get(plugin_name)
            && plugin.enabled
        {
            for agent_dir in &plugin.agent_dirs {
                let agent_file = agent_dir.join(format!("{agent_name}.md"));
                if agent_file.is_file()
                    && let Some(mut def) = load_plugin_agent_definition(plugin, &agent_file)
                {
                    substitute_plugin_vars(&mut def, plugin);
                    return Some(def);
                }
            }
        }

        // Bare name lookup: only resolve if exactly one plugin has this agent.
        let mut matches: Vec<(&crate::plugins::registry::LoadedPlugin, std::path::PathBuf)> =
            Vec::new();
        for plugin in registry.enabled_plugins() {
            for agent_dir in &plugin.agent_dirs {
                let agent_file = agent_dir.join(format!("{name}.md"));
                if agent_file.is_file() {
                    matches.push((plugin, agent_file));
                }
            }
        }
        if matches.len() == 1 {
            let (plugin, agent_file) = matches.first()?;
            if let Some(mut def) = load_plugin_agent_definition(plugin, agent_file) {
                substitute_plugin_vars(&mut def, plugin);
                return Some(def);
            }
        } else if matches.len() > 1 {
            let plugin_names: Vec<&str> = matches.iter().map(|(p, _)| p.name.as_str()).collect();
            tracing::warn!(
                agent_name = name,
                plugins = ?plugin_names,
                "ambiguous bare agent name matches multiple plugins; use qualified name (plugin:agent)"
            );
        }
    }

    None
}

/// Load one plugin-provided agent file, tagged with its owning plugin.
/// Untrusted plugins are parsed frontmatter-only so their prompt body never reaches the model before trust.
/// A parse failure drops the agent from discovery, so it is logged rather than swallowed.
fn load_plugin_agent_definition(
    plugin: &crate::plugins::LoadedPlugin,
    path: &Path,
) -> Option<AgentDefinition> {
    let loaded = if plugin.trusted {
        AgentDefinition::from_file(path)
    } else {
        AgentDefinition::from_file_frontmatter_only(path)
    };
    match loaded {
        Ok(mut def) => {
            def.plugin_name = Some(plugin.name.clone());
            Some(def)
        }
        Err(e) => {
            tracing::warn!(
                plugin = %plugin.name,
                path = %path.display(),
                error = %e,
                "Failed to parse plugin agent definition, skipping"
            );
            None
        }
    }
}

/// Expand `${CLAUDE_PLUGIN_ROOT}` / `${CLAUDE_PLUGIN_DATA}` (and the Grok aliases) in a plugin agent's body so the model receives absolute paths.
fn substitute_plugin_vars(def: &mut AgentDefinition, plugin: &crate::plugins::LoadedPlugin) {
    // Untrusted plugins are loaded frontmatter-only (body is None), and most agents use a built-in system prompt
    // Skip computing root/data paths when there is nothing to expand
    let has_custom_prompt = matches!(def.system_prompt, TemplateOverride::Custom(_));
    if def.prompt_body.is_none() && !has_custom_prompt {
        return;
    }
    let (root, data) = (plugin.root_str(), plugin.data_dir_str());
    if let Some(body) = def.prompt_body.take() {
        def.prompt_body = Some(crate::plugins::manifest::substitute_env_vars(
            &body, &root, &data,
        ));
    }
    if let TemplateOverride::Custom(tpl) = &def.system_prompt {
        def.system_prompt = TemplateOverride::Custom(
            crate::plugins::manifest::substitute_env_vars(tpl, &root, &data),
        );
    }
}

/// Load project agent definitions from every `.grok/agents` / `.claude/agents` dir that [`project_agent_dirs`] finds along the cwd-to-git-root walk.
fn load_project_definitions(
    cwd: &Path,
    definitions: &mut Vec<AgentDefinition>,
    seen_names: &mut std::collections::HashSet<String>,
) {
    for agents_dir in project_agent_dirs(Some(cwd)).0 {
        load_definitions_from_dir(&agents_dir, AgentScope::Project, definitions, seen_names);
    }
}

/// First project agent named `name` along the cwd-to-git-root walk from [`project_agent_dirs`], highest-priority dir first.
fn load_project_definition_by_name(name: &str, cwd: &Path) -> Option<AgentDefinition> {
    for agents_dir in project_agent_dirs(Some(cwd)).0 {
        let agent_file = agents_dir.join(format!("{name}.md"));
        if let Some(def) = load_definition_from_path(
            &agent_file,
            name,
            "Failed to parse project agent definition",
            Some(AgentScope::Project),
        ) {
            return Some(def);
        }
    }
    None
}

fn load_definition_by_name(
    dir: &Path,
    name: &str,
    error_message: &str,
    scope_override: Option<AgentScope>,
) -> Option<AgentDefinition> {
    let agent_file = dir.join(format!("{}.md", name));
    load_definition_from_path(&agent_file, name, error_message, scope_override)
}

fn load_definition_from_path(
    agent_file: &Path,
    name: &str,
    error_message: &str,
    scope_override: Option<AgentScope>,
) -> Option<AgentDefinition> {
    if !agent_file.is_file() {
        return None;
    }

    match AgentDefinition::from_file(agent_file) {
        Ok(mut def) => {
            if let Some(scope) = scope_override {
                def.scope = scope;
            }
            Some(def)
        }
        Err(e) => {
            tracing::warn!(
                name = name,
                path = %agent_file.display(),
                error = %e,
                context = error_message,
                "Failed to parse agent definition"
            );
            None
        }
    }
}

fn load_definitions_from_dir(
    dir: &Path,
    scope: AgentScope,
    definitions: &mut Vec<AgentDefinition>,
    seen_names: &mut std::collections::HashSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        match AgentDefinition::from_file(&path) {
            Ok(mut def) => {
                def.scope = scope;
                // Dedup by name; first occurrence (highest priority) wins
                if seen_names.insert(def.name.clone()) {
                    definitions.push(def);
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse agent definition, skipping"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::discovery::{PluginId, PluginScope};
    use crate::plugins::manifest::PluginManifest;
    use crate::plugins::{LoadedPlugin, PluginRegistry};
    use std::fs;
    use std::path::PathBuf;

    fn test_origin_for_scope(scope: PluginScope) -> crate::plugins::PluginOrigin {
        use crate::plugins::PluginOrigin;
        match scope {
            PluginScope::CliOverride => PluginOrigin::CliOverride,
            PluginScope::Project => PluginOrigin::ProjectGrok,
            PluginScope::User => PluginOrigin::UserGrok,
            PluginScope::ConfigPath => PluginOrigin::ConfigPath,
        }
    }

    fn write_agent_file(dir: &std::path::Path, filename: &str, name: &str, desc: &str) {
        let content = format!("---\nname: {name}\ndescription: {desc}\n---\n");
        fs::write(dir.join(filename), content).unwrap();
    }

    fn make_plugin_registry(
        plugin_name: &str,
        scope: PluginScope,
        agent_dirs: Vec<PathBuf>,
    ) -> PluginRegistry {
        let root = PathBuf::from(format!("/tmp/{plugin_name}"));
        let loaded = LoadedPlugin {
            name: plugin_name.to_string(),
            id: PluginId::new(scope, &root, plugin_name),
            root: root.clone(),
            canonical_root: root.clone(),
            scope,
            origin: test_origin_for_scope(scope),
            trusted: true,
            enabled: true,
            version: Some("1.0.0".to_string()),
            description: Some(format!("Plugin {plugin_name}")),
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs,
            hooks_path: None,
            mcp_config_path: None,
            skill_count: 0,
            agent_count: 0,
            skill_names: vec![],
            agent_names: vec![],
            has_hooks: false,
            hook_count: 0,
            has_inline_hooks_only: false,
            lsp_config_path: None,
            mcp_server_count: 0,
            has_inline_mcp_only: false,
            lsp_server_count: 0,
            has_inline_lsp_only: false,
            inline_hooks: None,
            inline_mcp_servers: None,
            inline_lsp_servers: None,
            conflict: None,
        };

        let LoadedPlugin { agent_dirs, .. } = loaded;
        let discovered = crate::plugins::DiscoveredPlugin {
            manifest: PluginManifest {
                name: plugin_name.to_string(),
                version: Some("1.0.0".to_string()),
                description: Some(format!("Plugin {plugin_name}")),
                ..Default::default()
            },
            id: PluginId::new(scope, &root, plugin_name),
            root: root.clone(),
            canonical_root: root,
            scope,
            origin: test_origin_for_scope(scope),
            trusted: true,
            skill_dirs: vec![],
            command_dirs: vec![],
            agent_dirs,
            hooks_path: None,
            mcp_config_path: None,
            lsp_config_path: None,
            conflict: None,
        };
        PluginRegistry::from_discovered(vec![discovered], &[], &[plugin_name.to_string()])
    }

    #[test]
    fn user_agent_dirs_includes_legacy_grok_when_distill_home_differs() {
        let home = Path::new("/home/u");
        let grok = Path::new("/custom/grokhome");
        let paths: Vec<_> = user_agent_dirs(Some(home), Some(grok))
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        assert!(paths.contains(&grok.join("agents")));
        assert!(paths.contains(&home.join(".grok").join("agents")));
        assert!(paths.contains(&home.join(".claude").join("agents")));
        assert!(paths.contains(&grok.join("bundled").join("agents")));
        assert!(paths.contains(&home.join(".grok").join("bundled").join("agents")));
    }

    #[test]
    fn user_agent_dirs_dedups_legacy_when_distill_home_is_dot_grok() {
        let home = Path::new("/home/u");
        let grok = home.join(".grok");
        let count = user_agent_dirs(Some(home), Some(&grok))
            .into_iter()
            .filter(|(p, _)| *p == grok.join("agents"))
            .count();
        assert_eq!(
            count, 1,
            "no duplicate ~/.grok/agents when distill_home == ~/.grok"
        );
    }

    #[test]
    fn test_by_name_unknown_agent_is_not_builtin() {
        // Not a built-in, so this returns None unless a project or user-level agent file exists with that name
        let def = by_name("not-a-builtin-agent");
        assert!(def.is_none());
    }

    #[test]
    fn test_by_name_builtin_browser_use() {
        let def = by_name("browser-use");
        assert!(def.is_some());
        assert_eq!(def.unwrap().name, "browser-use");
    }

    #[test]
    fn test_by_name_builtin_distill() {
        let def = by_name("distill");
        assert!(def.is_some());
        assert_eq!(def.unwrap().name, "distill");
    }

    #[test]
    fn test_by_name_builtin_codex() {
        let def = by_name("codex");
        assert!(def.is_some());
        assert_eq!(def.unwrap().name, "codex");
    }

    #[test]
    fn test_by_name_unknown_returns_none() {
        let def = by_name("nonexistent-agent-xyz");
        assert!(def.is_none());
    }

    #[test]
    fn test_discover_finds_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        write_agent_file(&agents_dir, "test-agent.md", "test-agent", "A test");
        write_agent_file(&agents_dir, "another.md", "another", "Another");

        let defs = discover_with_home(tmp.path(), None, None);
        assert_eq!(defs.len(), 2);
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"test-agent"));
        assert!(names.contains(&"another"));
    }

    #[test]
    fn test_discover_ignores_non_md_files() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        write_agent_file(&agents_dir, "valid.md", "valid", "Valid agent");
        fs::write(agents_dir.join("readme.txt"), "not an agent").unwrap();
        fs::write(agents_dir.join("config.yaml"), "key: value").unwrap();

        let defs = discover_with_home(tmp.path(), None, None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs.first().map(|d| d.name.as_str()), Some("valid"));
    }

    #[test]
    fn test_discover_invalid_md_logged_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        write_agent_file(&agents_dir, "good.md", "good", "Good agent");
        fs::write(agents_dir.join("bad.md"), "just text, no frontmatter").unwrap();

        let defs = discover_with_home(tmp.path(), None, None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs.first().map(|d| d.name.as_str()), Some("good"));
    }

    #[test]
    fn test_discover_dedup_by_name() {
        let tmp = tempfile::tempdir().unwrap();

        // Create two directories with same-named agents at different levels
        let inner_dir = tmp.path().join("subdir");
        fs::create_dir_all(&inner_dir).unwrap();

        let agents_dir_1 = tmp.path().join(".grok").join("agents");
        let agents_dir_2 = inner_dir.join(".grok").join("agents");
        fs::create_dir_all(&agents_dir_1).unwrap();
        fs::create_dir_all(&agents_dir_2).unwrap();

        write_agent_file(&agents_dir_1, "dup.md", "dup", "Parent version");
        write_agent_file(&agents_dir_2, "dup.md", "dup", "Child version");

        // Discover from the inner dir; the inner copy is found first and wins
        let defs = discover_with_home(&inner_dir, None, None);
        let dup_defs: Vec<_> = defs.iter().filter(|d| d.name == "dup").collect();
        assert_eq!(dup_defs.len(), 1, "Should dedup by name");
    }

    #[test]
    fn test_discover_includes_bundled_agents_at_lowest_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(
            &bundled_dir,
            "bundled-agent.md",
            "bundled-agent",
            "Bundled agent",
        );

        let defs = discover_with_home(&cwd, Some(&home), Some(&home.join(".grok")));
        assert_eq!(defs.len(), 1);
        let Some(def) = defs.first() else {
            panic!("expected bundled agent: {defs:?}");
        };
        assert_eq!(def.name, "bundled-agent");
        assert_eq!(def.scope, AgentScope::Bundled);
    }

    #[test]
    fn test_by_name_in_cwd_uses_bundled_when_no_higher_priority_match_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(
            &bundled_dir,
            "bundled-only.md",
            "bundled-only",
            "Bundled only",
        );

        let def =
            by_name_in_cwd_with_home("bundled-only", &cwd, Some(&home), Some(&home.join(".grok")))
                .unwrap();
        assert_eq!(def.scope, AgentScope::Bundled);
        assert_eq!(def.description, "Bundled only");
    }

    #[test]
    fn test_by_name_in_cwd_user_beats_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let user_dir = home.join(".grok").join("agents");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&user_dir).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(&user_dir, "reviewer.md", "reviewer", "User reviewer");
        write_agent_file(&bundled_dir, "reviewer.md", "reviewer", "Bundled reviewer");

        let def =
            by_name_in_cwd_with_home("reviewer", &cwd, Some(&home), Some(&home.join(".grok")))
                .unwrap();
        assert_eq!(def.scope, AgentScope::User);
        assert_eq!(def.description, "User reviewer");
    }

    #[test]
    fn test_by_name_in_cwd_builtin_beats_bundled_for_builtin_names() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(&bundled_dir, "explore.md", "explore", "Bundled explore");

        let def = by_name_in_cwd_with_home("explore", &cwd, Some(&home), Some(&home.join(".grok")))
            .unwrap();
        assert_eq!(def.scope, AgentScope::BuiltIn);
        assert_ne!(def.description, "Bundled explore");
    }

    #[test]
    fn test_by_name_in_cwd_project_beats_bundled() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let project_dir = cwd.join(".grok").join("agents");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&project_dir).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(&project_dir, "reviewer.md", "reviewer", "Project reviewer");
        write_agent_file(&bundled_dir, "reviewer.md", "reviewer", "Bundled reviewer");

        let def =
            by_name_in_cwd_with_home("reviewer", &cwd, Some(&home), Some(&home.join(".grok")))
                .unwrap();
        assert_eq!(def.scope, AgentScope::Project);
        assert_eq!(def.description, "Project reviewer");
    }

    #[test]
    fn test_by_name_in_cwd_project_shadows_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        // Create a project-level "distill" that shadows the built-in
        write_agent_file(
            &agents_dir,
            "distill.md",
            "distill",
            "Custom distill",
        );

        let def = by_name_in_cwd("distill", tmp.path());
        assert!(def.is_some());
        let def = def.unwrap();
        assert_eq!(def.name, "distill");
        assert_eq!(def.description, "Custom distill");
    }

    #[test]
    fn test_by_name_in_cwd_falls_back_to_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        // No .grok/agents/ directory, so lookup falls back to the built-in

        let def = by_name_in_cwd("distill", tmp.path());
        assert!(def.is_some());
        let def = def.unwrap();
        assert_eq!(def.name, "distill");
        assert_eq!(def.scope, AgentScope::BuiltIn);
    }

    // ── all_subagents / merge_subagents tests ───────────────────────

    fn synthetic_agent(name: &str, desc: &str, scope: AgentScope) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: desc.to_string(),
            scope,
            agents_md: false,
            ..AgentDefinition::general_purpose()
        }
    }

    #[test]
    fn test_orchestrator_from_str_resolves() {
        use std::str::FromStr;
        let variant = BuiltinAgentName::from_str("distill-orchestrator")
            .expect("from_str must resolve distill-orchestrator");
        assert_eq!(variant, BuiltinAgentName::DistillOrchestrator);
        let def = variant.definition();
        assert_eq!(def.name, "distill-orchestrator");
        assert!(
            def.prompt_body.is_some(),
            "orchestrator must have prompt_body"
        );
    }

    #[test]
    fn test_orchestrator_by_name_in_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let def = by_name_in_cwd("distill-orchestrator", tmp.path())
            .expect("by_name_in_cwd must find distill-orchestrator");
        assert_eq!(def.name, "distill-orchestrator");
        assert!(def.prompt_body.is_some());
    }

    #[test]
    fn test_merge_returns_3_builtins_when_no_user_agents() {
        let entries = merge_subagents(vec![], &HashMap::new());
        assert_eq!(entries.len(), 3);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"general-purpose"));
        assert!(names.contains(&"explore"));
        assert!(names.contains(&"plan"));
        for entry in &entries {
            assert!(
                matches!(&entry.source, SubagentSource::Builtin(_)),
                "expected Builtin source for '{}'",
                entry.name
            );
            assert!(entry.shadows_builtin.is_none());
        }
    }

    #[test]
    fn test_merge_filters_toggled_off_builtins() {
        let toggle = HashMap::from([("plan".to_string(), false)]);
        let entries = merge_subagents(vec![], &toggle);
        assert_eq!(entries.len(), 2);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"general-purpose"));
        assert!(names.contains(&"explore"));
        assert!(!names.contains(&"plan"));
    }

    #[test]
    fn test_merge_includes_user_defined_agents() {
        let discovered = vec![synthetic_agent(
            "code-reviewer",
            "Reviews code",
            AgentScope::Project,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        assert_eq!(entries.len(), 4);
        let cr = entries.iter().find(|e| e.name == "code-reviewer").unwrap();
        assert_eq!(cr.description, "Reviews code");
        assert_eq!(
            cr.source,
            SubagentSource::UserDefined {
                scope: AgentScope::Project
            }
        );
        assert!(cr.shadows_builtin.is_none());
    }

    #[test]
    fn test_merge_filters_toggled_off_user_agents() {
        let discovered = vec![synthetic_agent(
            "code-reviewer",
            "Reviews code",
            AgentScope::Project,
        )];
        let toggle = HashMap::from([("code-reviewer".to_string(), false)]);
        let entries = merge_subagents(discovered, &toggle);
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| e.name != "code-reviewer"));
    }

    #[test]
    fn test_merge_project_agent_shadows_builtin() {
        let discovered = vec![synthetic_agent(
            "explore",
            "Custom explore agent",
            AgentScope::Project,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        assert_eq!(entries.len(), 3); // The project agent replaced the built-in instead of appending
        let explore = entries.iter().find(|e| e.name == "explore").unwrap();
        assert_eq!(explore.description, "Custom explore agent");
        assert_eq!(
            explore.source,
            SubagentSource::UserDefined {
                scope: AgentScope::Project
            }
        );
    }

    #[test]
    fn test_merge_shadowed_entry_has_shadows_builtin() {
        let discovered = vec![synthetic_agent(
            "explore",
            "Custom explore",
            AgentScope::Project,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        let explore = entries.iter().find(|e| e.name == "explore").unwrap();
        assert_eq!(
            explore.shadows_builtin,
            Some(BuiltinAgentName::Explore),
            "shadowed explore should record shadows_builtin"
        );
    }

    #[test]
    fn test_merge_user_level_builtin_name_is_skipped() {
        // A user-level (~/.grok/agents/) agent named "explore" should NOT shadow
        // the built-in — only project-level can do that.
        let discovered = vec![synthetic_agent(
            "explore",
            "User-level explore",
            AgentScope::User,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        assert_eq!(entries.len(), 3);
        let explore = entries.iter().find(|e| e.name == "explore").unwrap();
        assert!(
            matches!(
                &explore.source,
                SubagentSource::Builtin(BuiltinAgentName::Explore)
            ),
            "user-level explore should not shadow built-in"
        );
    }

    #[test]
    fn test_merge_bundled_builtin_name_is_skipped() {
        let discovered = vec![synthetic_agent(
            "explore",
            "Bundled explore",
            AgentScope::Bundled,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        let explore = entries.iter().find(|e| e.name == "explore").unwrap();
        assert!(matches!(
            &explore.source,
            SubagentSource::Builtin(BuiltinAgentName::Explore)
        ));
    }

    #[test]
    fn test_merge_user_unique_name_appended() {
        let discovered = vec![synthetic_agent(
            "migration-helper",
            "Helps with migrations",
            AgentScope::User,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        assert_eq!(entries.len(), 4);
        // Verify ordering: built-ins first, then user
        let [e0, e1, e2, e3] = entries.as_slice() else {
            panic!("expected four entries: {entries:?}");
        };
        assert!(matches!(&e0.source, SubagentSource::Builtin(_)));
        assert!(matches!(&e1.source, SubagentSource::Builtin(_)));
        assert!(matches!(&e2.source, SubagentSource::Builtin(_)));
        assert_eq!(e3.name, "migration-helper");
        assert_eq!(
            e3.source,
            SubagentSource::UserDefined {
                scope: AgentScope::User
            }
        );
    }

    #[test]
    fn test_merge_bundled_unique_name_appended() {
        let discovered = vec![synthetic_agent(
            "bundled-helper",
            "Helps from bundle",
            AgentScope::Bundled,
        )];
        let entries = merge_subagents(discovered, &HashMap::new());
        let Some(e3) = entries.get(3) else {
            panic!("expected four entries: {entries:?}");
        };
        assert_eq!(e3.name, "bundled-helper");
        assert_eq!(
            e3.source,
            SubagentSource::UserDefined {
                scope: AgentScope::Bundled
            }
        );
    }

    #[test]
    fn test_merge_all_toggled_off_returns_empty() {
        let toggle = HashMap::from([
            ("general-purpose".to_string(), false),
            ("explore".to_string(), false),
            ("plan".to_string(), false),
        ]);
        let entries = merge_subagents(vec![], &toggle);
        assert!(entries.is_empty(), "all toggled off should return empty");
    }

    #[test]
    fn test_merge_invalid_user_agent_preserves_builtin() {
        // Simulate: discover() skips invalid files (returns empty for that file).
        // So if a user's explore.md is invalid, discover() won't include it, and the built-in explore remains
        let discovered = vec![]; // no valid user agents discovered
        let entries = merge_subagents(discovered, &HashMap::new());
        assert_eq!(entries.len(), 3);
        let explore = entries.iter().find(|e| e.name == "explore").unwrap();
        assert!(matches!(
            &explore.source,
            SubagentSource::Builtin(BuiltinAgentName::Explore)
        ));
    }

    #[test]
    fn test_merge_project_user_duplicate_project_wins() {
        let discovered = vec![
            synthetic_agent("my-agent", "Project version", AgentScope::Project),
            synthetic_agent("my-agent", "User version", AgentScope::User),
        ];
        let entries = merge_subagents(discovered, &HashMap::new());
        let my_agent: Vec<_> = entries.iter().filter(|e| e.name == "my-agent").collect();
        assert_eq!(my_agent.len(), 1, "should dedup by name");
        assert_eq!(
            my_agent.first().map(|e| &e.source),
            Some(&SubagentSource::UserDefined {
                scope: AgentScope::Project
            })
        );
    }

    #[test]
    fn test_merge_user_beats_bundled_for_visible_equals_callable() {
        let discovered = vec![
            synthetic_agent("reviewer", "Bundled reviewer", AgentScope::Bundled),
            synthetic_agent("reviewer", "User reviewer", AgentScope::User),
        ];
        let entries = merge_subagents(discovered, &HashMap::new());
        let reviewer = entries.iter().find(|e| e.name == "reviewer").unwrap();
        assert_eq!(reviewer.description, "User reviewer");
        assert_eq!(
            reviewer.source,
            SubagentSource::UserDefined {
                scope: AgentScope::User
            }
        );
    }

    #[test]
    fn test_all_subagents_with_project_agent_file() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        write_agent_file(
            &agents_dir,
            "test-agent.md",
            "test-agent",
            "A test subagent",
        );

        let entries = all_subagents_with_home(tmp.path(), &HashMap::new(), None, None);
        assert_eq!(entries.len(), 4);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["general-purpose", "explore", "plan", "test-agent"]
        );
    }

    #[test]
    fn test_all_subagents_with_plugins_preserves_native_visibility() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let user_dir = home.join(".grok").join("agents");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&user_dir).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();

        write_agent_file(&user_dir, "reviewer.md", "reviewer", "User reviewer");
        write_agent_file(&bundled_dir, "reviewer.md", "reviewer", "Bundled reviewer");

        let plugin_root = tempfile::tempdir().unwrap();
        let plugin_agents = plugin_root.path().join("agents");
        fs::create_dir_all(&plugin_agents).unwrap();
        write_agent_file(&plugin_agents, "reviewer.md", "reviewer", "Plugin reviewer");

        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![plugin_agents]);
        let entries = all_subagents_with_plugins_and_home(
            &cwd,
            &HashMap::new(),
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        );

        let native = entries.iter().find(|e| e.name == "reviewer").unwrap();
        assert_eq!(native.description, "User reviewer");
        assert_eq!(
            native.source,
            SubagentSource::UserDefined {
                scope: AgentScope::User
            }
        );
        assert!(entries.iter().any(|e| e.name == "plugin-one:reviewer"));
    }

    #[test]
    fn test_plugin_agents_filtered_by_qualified_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();

        let plugin_root = tempfile::tempdir().unwrap();
        let plugin_agents = plugin_root.path().join("agents");
        fs::create_dir_all(&plugin_agents).unwrap();
        write_agent_file(&plugin_agents, "reviewer.md", "reviewer", "Plugin reviewer");

        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![plugin_agents]);

        let toggle = HashMap::from([("plugin-one:reviewer".to_string(), false)]);
        let entries = all_subagents_with_plugins_and_home(
            &cwd,
            &toggle,
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        );
        assert!(
            !entries.iter().any(|e| e.name == "plugin-one:reviewer"),
            "toggled-off plugin agent must not be callable"
        );

        let entries = all_subagents_with_plugins_and_home(
            &cwd,
            &HashMap::new(),
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        );
        assert!(entries.iter().any(|e| e.name == "plugin-one:reviewer"));
    }

    #[test]
    fn plugin_agent_with_unrecognized_color_is_still_discovered() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();

        let plugin_root = tempfile::tempdir().unwrap();
        let plugin_agents = plugin_root.path().join("agents");
        fs::create_dir_all(&plugin_agents).unwrap();
        fs::write(
            plugin_agents.join("painter.md"),
            "---\nname: painter\ndescription: Plugin painter\ncolor: chartreuse\n---\nBody.\n",
        )
        .unwrap();

        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![plugin_agents]);
        let entries = all_subagents_with_plugins_and_home(
            &cwd,
            &HashMap::new(),
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        );
        assert!(entries.iter().any(|e| e.name == "plugin-one:painter"));

        let def = by_name_in_cwd_with_plugins_and_home(
            "plugin-one:painter",
            &cwd,
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        )
        .expect("agent must resolve despite the unrecognized color");
        assert_eq!(def.color, None, "unrecognized color must be dropped");
    }

    #[test]
    fn test_by_name_in_cwd_with_plugins_prefers_native_over_plugin_bare_name() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let bundled_dir = home.join(".grok").join("bundled").join("agents");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&bundled_dir).unwrap();
        write_agent_file(&bundled_dir, "reviewer.md", "reviewer", "Bundled reviewer");

        let plugin_root = tempfile::tempdir().unwrap();
        let plugin_agents = plugin_root.path().join("agents");
        fs::create_dir_all(&plugin_agents).unwrap();
        write_agent_file(&plugin_agents, "reviewer.md", "reviewer", "Plugin reviewer");

        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![plugin_agents]);
        let def = by_name_in_cwd_with_plugins_and_home(
            "reviewer",
            &cwd,
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        )
        .unwrap();

        assert_eq!(def.scope, AgentScope::Bundled);
        assert_eq!(def.description, "Bundled reviewer");
        assert!(def.plugin_name.is_none());
    }

    #[test]
    fn test_plugin_agent_body_resolves_plugin_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();

        let plugin_dir = tempfile::tempdir().unwrap();
        let plugin_agents = plugin_dir.path().join("agents");
        fs::create_dir_all(&plugin_agents).unwrap();
        // Mirrors how enterprise plugin-dev agents reference the plugin root.
        let content = "---\nname: runner\ndescription: Runs a tool\n---\n\
            Run python3 \"${CLAUDE_PLUGIN_ROOT}/tools/x.py\"\n";
        fs::write(plugin_agents.join("runner.md"), content).unwrap();

        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![plugin_agents]);
        let expected_root = registry.get("plugin-one").unwrap().root_str();
        let resolved = format!("{expected_root}/tools/x.py");

        // Bare-name resolution path.
        let bare = by_name_in_cwd_with_plugins_and_home(
            "runner",
            &cwd,
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        )
        .unwrap();
        let bare_body = bare.prompt_body.as_deref().unwrap();
        assert!(
            bare_body.contains(&resolved),
            "expected resolved root in: {bare_body}"
        );
        assert!(
            !bare_body.contains("${CLAUDE_PLUGIN_ROOT}"),
            "literal token must be gone: {bare_body}"
        );

        // Qualified-name resolution path.
        let qualified = by_name_in_cwd_with_plugins_and_home(
            "plugin-one:runner",
            &cwd,
            Some(&registry),
            Some(&home),
            Some(&home.join(".grok")),
        )
        .unwrap();
        let qualified_body = qualified.prompt_body.as_deref().unwrap();
        assert!(
            qualified_body.contains(&resolved),
            "expected resolved root in: {qualified_body}"
        );
        assert!(!qualified_body.contains("${CLAUDE_PLUGIN_ROOT}"));
    }

    #[test]
    fn test_substitute_plugin_vars_resolves_custom_system_prompt() {
        // `system_prompt` is internal (not frontmatter-driven)
        // Construct the definition directly to exercise the `TemplateOverride::Custom` branch
        let registry = make_plugin_registry("plugin-one", PluginScope::User, vec![]);
        let plugin = registry.get("plugin-one").unwrap();

        let mut def = AgentDefinition::default_distill();
        def.prompt_body = Some("Body ${CLAUDE_PLUGIN_ROOT}/x".to_string());
        def.system_prompt =
            TemplateOverride::Custom("Data at ${CLAUDE_PLUGIN_DATA}/db".to_string());

        substitute_plugin_vars(&mut def, plugin);

        let expected_body = format!("Body {}/x", plugin.root_str());
        let expected_prompt = format!("Data at {}/db", plugin.data_dir_str());
        assert_eq!(def.prompt_body.as_deref(), Some(expected_body.as_str()));
        match &def.system_prompt {
            TemplateOverride::Custom(tpl) => {
                assert_eq!(tpl, &expected_prompt);
                assert!(!tpl.contains("${CLAUDE_PLUGIN_DATA}"));
            }
            other => panic!("expected Custom system_prompt, got {other:?}"),
        }
    }

    #[test]
    fn test_all_subagents_toggle_filters_project_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let agents_dir = tmp.path().join(".grok").join("agents");
        fs::create_dir_all(&agents_dir).unwrap();

        write_agent_file(
            &agents_dir,
            "test-agent.md",
            "test-agent",
            "A test subagent",
        );

        let toggle = HashMap::from([("test-agent".to_string(), false)]);
        let entries = all_subagents_with_home(tmp.path(), &toggle, None, None);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["general-purpose", "explore", "plan"]);
    }
}
