// Modified for Distill by Samuel Fajreldines, 2026.
//! Filesystem scanning for plugin directories.
//!
//! Discovers plugins from multiple sources in priority order:
//! 1. CLI `--plugin-dir` paths (scope: `CliOverride`)
//! 2. `.grok/plugins/*/` (scope: `Project`, walked from cwd to worktree root)
//! 3. `.claude/plugins/*/` (scope: `Project`, compat)
//! 4. `~/.grok/plugins/*/` (scope: `User`)
//! 5. `~/.claude/plugins/*/` (scope: `User`, compat)
//!    `~/.grok/installed-plugins/*/` (scope: `User`, marketplace installs)
//!    Installed plugins from `~/.claude/plugins/installed_plugins.json` (scope: `User`)
//! 6. Paths from `[plugins].paths` in config (scope: `ConfigPath`)
//!
//! Deduplicates by canonical path and resolves name conflicts via the canonical source precedence.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::manifest::{ManifestLoadResult, PluginManifest, load_manifest, name_from_dirname};
use super::trust::TrustStore;

// ── Public types ──────────────────────────────────────────────────────

/// Where a plugin was discovered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginScope {
    /// `--plugin-dir` (highest priority, always trusted)
    CliOverride = 0,
    /// `.grok/plugins/` or `.claude/plugins/` in project (requires trust)
    Project = 1,
    /// `~/.grok/plugins/` or `~/.claude/plugins/` (always trusted)
    User = 2,
    /// `[plugins].paths` in config (trust depends on location)
    ConfigPath = 3,
}

impl PluginScope {
    /// Label used in `PluginId` format.
    pub fn id_label(&self) -> &'static str {
        match self {
            Self::CliOverride => "cli",
            Self::Project => "project",
            Self::User => "user",
            Self::ConfigPath => "config",
        }
    }
}

impl std::fmt::Display for PluginScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CliOverride => write!(f, "cli"),
            Self::Project => write!(f, "project"),
            Self::User => write!(f, "user"),
            Self::ConfigPath => write!(f, "config"),
        }
    }
}

/// The concrete discovery source a plugin came from.
/// Recorded at scan time so consumers do not re-derive it from paths. Not part of [`PluginId`], which stays scope-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOrigin {
    /// CLI `--plugin-dir`.
    CliOverride,
    /// Project `.grok/plugins/`.
    ProjectGrok,
    /// Project `.claude/plugins/`.
    ProjectClaude,
    /// `$GROK_HOME/plugins/`.
    UserGrok,
    /// `~/.claude/plugins/`.
    UserClaude,
    /// A compat marketplace clone (project `extraKnownMarketplaces` or user `known_marketplaces.json`).
    ClaudeMarketplace {
        /// Marketplace name from the settings/registry entry.
        marketplace: String,
    },
    /// Install recorded in `~/.claude/plugins/installed_plugins.json`.
    ClaudeInstalled {
        /// Marketplace name from the `name@marketplace` JSON key, when present.
        marketplace: Option<String>,
    },
    /// Grok's install registry (`~/.grok/installed-plugins`).
    MarketplaceInstall {
        /// Marketplace source display name (None for direct git/local installs).
        source_name: Option<String>,
        /// Git URL of the installed repo (None for local installs).
        git_url: Option<String>,
    },
    /// `[plugins].paths` in config.
    ConfigPath,
}

/// Stable internal identity for a plugin.
/// Format: `<scope>/<hex8>/<name>`, where hex8 is the first 8 hex chars of SHA-256 of the canonical plugin root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub String);

impl PluginId {
    /// Construct a plugin ID from its components.
    pub fn new(scope: PluginScope, canonical_root: &Path, name: &str) -> Self {
        let path_str = canonical_root.to_string_lossy();
        let mut hasher = Sha256::new();
        hasher.update(path_str.as_bytes());
        let hash = hasher.finalize();
        let hex8: String = hash.iter().take(4).map(|b| format!("{b:02x}")).collect();
        Self(format!("{}/{}/{}", scope.id_label(), hex8, name))
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A plugin candidate discovered on the filesystem.
#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    /// Parsed manifest (or synthetic for convention-based plugins).
    pub manifest: PluginManifest,
    /// Stable internal identity.
    pub id: PluginId,
    /// Absolute path to the plugin root directory.
    pub root: PathBuf,
    /// Canonical (symlink-resolved) root path.
    pub canonical_root: PathBuf,
    /// Where this plugin was discovered.
    pub scope: PluginScope,
    /// The concrete discovery source this plugin came from.
    pub origin: PluginOrigin,
    /// Whether the plugin is trusted for executable operations.
    pub trusted: bool,
    /// Resolved skill directories.
    pub skill_dirs: Vec<PathBuf>,
    pub command_dirs: Vec<PathBuf>,
    /// Resolved agent directories.
    pub agent_dirs: Vec<PathBuf>,
    /// Resolved hooks file path.
    pub hooks_path: Option<PathBuf>,
    /// Resolved MCP config file path.
    pub mcp_config_path: Option<PathBuf>,
    /// Resolved LSP config file path.
    pub lsp_config_path: Option<PathBuf>,
    /// Warning message when this plugin won a name collision.
    pub conflict: Option<String>,
}

impl DiscoveredPlugin {
    /// User-facing plugin name (from manifest or directory name).
    pub fn plugin_name(&self) -> &str {
        &self.manifest.name
    }
}

/// Configuration for plugin discovery.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryConfig {
    /// CLI `--plugin-dir` paths.
    pub cli_plugin_dirs: Vec<PathBuf>,
    /// `[plugins].paths` from config.
    pub config_paths: Vec<PathBuf>,
    /// `[plugins].disabled` plugin IDs or names.
    pub disabled: Vec<String>,
    /// `[plugins].enabled` plugin IDs or names (overrides default-disabled for project plugins).
    pub enabled: Vec<String>,
}

impl DiscoveryConfig {
    /// Ensure every discovered plugin appears in either `enabled` or `disabled`.
    /// Auto-enabled scopes (`CliOverride`, `ConfigPath`) go to `enabled`; `User` and `Project` go to `disabled`.
    /// Plugins already present in either list are left untouched.
    pub fn populate_plugin_lists(&mut self, discovered: &[DiscoveredPlugin]) {
        for dp in discovered {
            let name = &dp.manifest.name;
            let already_listed = self.enabled.iter().any(|e| e == name || e == &dp.id.0)
                || self.disabled.iter().any(|d| d == name || d == &dp.id.0);
            if already_listed {
                tracing::debug!(plugin = %name, id = %dp.id.0, "plugin already in enabled/disabled list");
                continue;
            }
            if matches!(dp.scope, PluginScope::CliOverride | PluginScope::ConfigPath) {
                tracing::debug!(plugin = %name, scope = ?dp.scope, "auto-adding to enabled list");
                self.enabled.push(name.clone());
            } else {
                tracing::debug!(plugin = %name, scope = ?dp.scope, "auto-adding to disabled list");
                self.disabled.push(name.clone());
            }
        }
    }
}

// ── Discovery entry point ─────────────────────────────────────────────

/// User plugin directories in priority order: `$GROK_HOME/plugins` then `~/.claude/plugins`.
/// Plugins are intentionally not discovered from legacy `~/.grok/plugins`.
/// Trust, persisted data, and install paths all resolve under `distill_home()`, so a legacy scan would be half-initialized.
fn user_plugin_dirs(home: Option<&Path>, grok: Option<&Path>) -> Vec<(PathBuf, PluginOrigin)> {
    let mut dirs = Vec::new();
    if let Some(g) = grok {
        dirs.push((g.join("plugins"), PluginOrigin::UserGrok));
    }
    if let Some(h) = home {
        dirs.push((h.join(".claude").join("plugins"), PluginOrigin::UserClaude));
    }
    dirs
}

/// Origin for a project plugins parent dir: `.claude/plugins` vs `.grok/plugins`.
fn project_plugins_dir_origin(plugins_dir: &Path) -> PluginOrigin {
    let is_claude = plugins_dir
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == ".claude");
    if is_claude {
        PluginOrigin::ProjectClaude
    } else {
        PluginOrigin::ProjectGrok
    }
}

/// Project plugin parent dirs along the walk from `cwd` to the git worktree root, plus that root.
/// Outside a git repo only `cwd` is checked. The folder-trust gate reuses this chain so detection cannot drift from discovery.
/// The returned root lets marketplace resolve reuse it instead of resolving the repo a second time.
pub fn project_plugin_dirs(cwd: Option<&Path>) -> (Vec<PathBuf>, Option<PathBuf>) {
    let Some(cwd) = cwd else {
        return (Vec::new(), None);
    };
    let chain = crate::repo::RepoDirChain::resolve(cwd);
    (project_plugin_dirs_in(&chain.dirs), chain.git_root)
}

/// Existing project plugin parent dirs (`.grok/plugins`, `.claude/plugins`) under each dir of a precomputed [`crate::repo::RepoDirChain`].
/// The folder-trust gate reuses its one shared chain here so detection and discovery can never drift.
pub fn project_plugin_dirs_in(chain_dirs: &[PathBuf]) -> Vec<PathBuf> {
    crate::repo::existing_subdirs_along(chain_dirs, &[".grok/plugins", ".claude/plugins"])
}

/// `cwd` is used to find the git worktree root for project-scope plugins.
/// `project_trusted` is the folder-trust verdict for `cwd`; it gates `Project`-scope plugins (CLI/User/ConfigPath scopes are unaffected).
/// Returns plugins deduplicated by canonical path, with name conflicts resolved by scope precedence.
pub fn discover_plugins(
    cwd: Option<&Path>,
    config: &DiscoveryConfig,
    trust_store: &TrustStore,
    project_trusted: bool,
) -> Vec<DiscoveredPlugin> {
    let _plugin_discovery_timer = crate::timing::timer("plugin_discovery");
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut candidates: Vec<DiscoveredPlugin> = Vec::new();

    // 1. CLI --plugin-dir paths
    for dir in &config.cli_plugin_dirs {
        if dir.is_dir() {
            collect_plugin(
                dir,
                PluginScope::CliOverride,
                PluginOrigin::CliOverride,
                trust_store,
                project_trusted,
                &mut seen_paths,
                &mut candidates,
            );
        } else {
            tracing::warn!(path = %dir.display(), "CLI --plugin-dir path is not a directory; skipping");
        }
    }

    // 2-3. Project plugins (.grok/plugins/, .claude/plugins/).
    // Scan the same dirs the folder-trust gate detects, via the shared `project_plugin_dirs` walk, so discovery and gating can never drift
    if let Some(cwd) = cwd {
        let (project_dirs, git_root) = project_plugin_dirs(Some(cwd));
        for plugins_dir in project_dirs {
            let origin = project_plugins_dir_origin(&plugins_dir);
            scan_plugin_dir(
                &plugins_dir,
                PluginScope::Project,
                origin,
                trust_store,
                project_trusted,
                &mut seen_paths,
                &mut candidates,
            );
        }

        // 3b. Marketplace plugins (extraKnownMarketplaces in .claude/settings.json).
        // Reuse the git root resolved above instead of walking the repo again
        if let Some(ref root) = git_root {
            for marketplace in &super::marketplace::resolve(root) {
                for dir in &marketplace.plugin_dirs {
                    collect_plugin(
                        dir,
                        PluginScope::Project,
                        PluginOrigin::ClaudeMarketplace {
                            marketplace: marketplace.name.clone(),
                        },
                        trust_store,
                        project_trusted,
                        &mut seen_paths,
                        &mut candidates,
                    );
                }
            }
        }
    }

    // 4-5. User plugins: $GROK_HOME/plugins, legacy ~/.grok/plugins, ~/.claude/plugins.
    // Gate the grok plugins dir on user_distill_home() so a project's .grok/plugins is never scanned as user-global when no home resolves
    let grok = distill_config::user_distill_home();
    let plugin_dirs = user_plugin_dirs(distill_dirs::home_dir().as_deref(), grok.as_deref());
    for (plugins_dir, origin) in plugin_dirs {
        if plugins_dir.is_dir() {
            scan_plugin_dir(
                &plugins_dir,
                PluginScope::User,
                origin,
                trust_store,
                project_trusted,
                &mut seen_paths,
                &mut candidates,
            );
        }
    }

    // 5a. Known marketplaces (~/.claude/plugins/known_marketplaces.json).
    // Marketplace repos are cloned locally and registered here.
    // Each marketplace has a plugins/ (and optionally external_plugins/) subdirectory.
    for marketplace in &super::marketplace::resolve_known_marketplaces() {
        for dir in &marketplace.plugin_dirs {
            collect_plugin(
                dir,
                PluginScope::User,
                PluginOrigin::ClaudeMarketplace {
                    marketplace: marketplace.name.clone(),
                },
                trust_store,
                project_trusted,
                &mut seen_paths,
                &mut candidates,
            );
        }
    }

    // 5b. Installed plugins (from install registry's managed directory)
    {
        // Installed plugins are always User scope (auto-trusted).
        // The user explicitly installed them via marketplace or CLI, so they should be trusted regardless of install_dir location
        let registry = super::install_registry::InstallRegistry::load();
        collect_installed_plugins(
            &registry,
            PluginScope::User,
            trust_store,
            project_trusted,
            &mut seen_paths,
            &mut candidates,
        );
    }

    // 5c. Installed plugins (~/.claude/plugins/installed_plugins.json).
    // Entries carry explicit installPath dirs (nested under cache/<marketplace>/<plugin>/<version>/)
    // The plugin name is extracted from the JSON key ("name@marketplace").
    if let Some(home) = distill_dirs::home_dir() {
        let installed_json = home
            .join(".claude")
            .join("plugins")
            .join("installed_plugins.json");
        for (name, marketplace, path) in read_claude_installed_plugins(&installed_json, cwd) {
            if path.is_dir() {
                let before = candidates.len();
                collect_plugin(
                    &path,
                    PluginScope::User,
                    PluginOrigin::ClaudeInstalled { marketplace },
                    trust_store,
                    project_trusted,
                    &mut seen_paths,
                    &mut candidates,
                );
                // Override dirname-derived name with the real name from the JSON key.
                if candidates.len() > before {
                    let plugin = candidates.last_mut().unwrap();
                    if plugin.manifest.name != name {
                        plugin.id = PluginId::new(plugin.scope, &plugin.canonical_root, &name);
                        plugin.manifest.name = name;
                    }
                }
            }
        }
    }

    // 6. Config-path plugins
    for dir in &config.config_paths {
        if dir.is_dir() {
            collect_plugin(
                dir,
                PluginScope::ConfigPath,
                PluginOrigin::ConfigPath,
                trust_store,
                project_trusted,
                &mut seen_paths,
                &mut candidates,
            );
        } else {
            tracing::warn!(path = %dir.display(), "[plugins].paths entry is not a directory; skipping");
        }
    }

    // Within the same plugin_name, the highest-priority scope wins; within same scope, first-found (alphabetical by canonical path) wins
    resolve_name_conflicts(&mut candidates);

    for p in &candidates {
        tracing::info!(
            name = %p.manifest.name,
            scope = %p.scope,
            root = %p.root.display(),
            skills = p.manifest.skill_dirs(&p.root).len(),
            agents = p.manifest.agent_dirs(&p.root).len(),
            has_hooks = p.hooks_path.is_some(),
            has_mcp = p.mcp_config_path.is_some(),
            has_lsp = p.lsp_config_path.is_some(),
            "plugin discovered"
        );
    }
    tracing::info!(
        discovered = candidates.len(),
        cli = candidates
            .iter()
            .filter(|p| p.scope == PluginScope::CliOverride)
            .count(),
        project = candidates
            .iter()
            .filter(|p| p.scope == PluginScope::Project)
            .count(),
        user = candidates
            .iter()
            .filter(|p| p.scope == PluginScope::User)
            .count(),
        config = candidates
            .iter()
            .filter(|p| p.scope == PluginScope::ConfigPath)
            .count(),
        "plugins: discovery complete"
    );

    candidates
}

// ── Internal helpers ──────────────────────────────────────────────────

/// Scan a plugins parent directory (e.g. `~/.grok/plugins/`) and collect
/// each subdirectory as a plugin candidate.
fn scan_plugin_dir(
    plugins_dir: &Path,
    scope: PluginScope,
    origin: PluginOrigin,
    trust_store: &TrustStore,
    project_trusted: bool,
    seen_paths: &mut HashSet<PathBuf>,
    candidates: &mut Vec<DiscoveredPlugin>,
) {
    let entries = match std::fs::read_dir(plugins_dir) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                path = %plugins_dir.display(),
                error = %e,
                "failed to read plugins directory"
            );
            return;
        }
    };

    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir()) // follows symlinks
        .map(|e| e.path())
        .collect();

    // Sort for deterministic ordering within same scope
    subdirs.sort();

    for subdir in subdirs {
        collect_plugin(
            &subdir,
            scope,
            origin.clone(),
            trust_store,
            project_trusted,
            seen_paths,
            candidates,
        );
    }
}

fn collect_installed_plugins(
    registry: &super::install_registry::InstallRegistry,
    scope: PluginScope,
    trust_store: &TrustStore,
    project_trusted: bool,
    seen_paths: &mut HashSet<PathBuf>,
    candidates: &mut Vec<DiscoveredPlugin>,
) {
    for (_key, repo) in registry.list() {
        let origin = PluginOrigin::MarketplaceInstall {
            source_name: repo
                .marketplace
                .as_ref()
                .map(|mp| mp.source_display_name.clone()),
            git_url: match &repo.kind {
                super::install_registry::InstallKind::Git { url, .. } => Some(url.clone()),
                super::install_registry::InstallKind::Local { .. } => None,
            },
        };
        for plugin in repo.plugins.values() {
            let plugin_root = match plugin.subdir.as_deref() {
                Some(sub) => {
                    if subdir_escapes(sub) {
                        tracing::warn!(
                            repo = %repo.path.display(),
                            subdir = sub,
                            "skipping installed plugin: registry subdir escapes repo root"
                        );
                        continue;
                    }
                    repo.path.join(sub)
                }
                None => repo.path.clone(),
            };
            if plugin_root.is_dir() {
                collect_plugin(
                    &plugin_root,
                    scope,
                    origin.clone(),
                    trust_store,
                    project_trusted,
                    seen_paths,
                    candidates,
                );
            }
        }
    }
}

fn subdir_escapes(subdir: &str) -> bool {
    let p = Path::new(subdir);
    p.is_absolute()
        || p.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

/// Attempt to load a single plugin directory and add it to candidates.
fn collect_plugin(
    plugin_root: &Path,
    scope: PluginScope,
    origin: PluginOrigin,
    trust_store: &TrustStore,
    project_trusted: bool,
    seen_paths: &mut HashSet<PathBuf>,
    candidates: &mut Vec<DiscoveredPlugin>,
) {
    // Canonicalize for dedup
    let canonical = match dunce::canonicalize(plugin_root) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                path = %plugin_root.display(),
                error = %e,
                "failed to canonicalize plugin root; skipping"
            );
            return;
        }
    };

    // Deduplicate by canonical path
    if !seen_paths.insert(canonical.clone()) {
        tracing::debug!(
            path = %plugin_root.display(),
            canonical = %canonical.display(),
            "plugin directory already discovered via another path; skipping"
        );
        return;
    }

    // Load manifest or derive from directory name
    let manifest = match load_manifest(plugin_root) {
        Ok(ManifestLoadResult::Found(m)) => *m,
        Ok(ManifestLoadResult::NotFound) => {
            // Convention-based: derive name from directory, check for skills/ or agents/ or .mcp.json or hooks/hooks.json
            let Some(name) = name_from_dirname(plugin_root) else {
                tracing::debug!(
                    path = %plugin_root.display(),
                    "cannot derive plugin name from directory; skipping"
                );
                return;
            };

            // Only treat as a plugin if it has at least one component
            let has_skills =
                plugin_root.join("skills").is_dir() || plugin_root.join("commands").is_dir();
            let has_agents = plugin_root.join("agents").is_dir();
            let has_mcp = plugin_root.join(".mcp.json").is_file();
            let has_lsp = plugin_root.join(".lsp.json").is_file();
            let has_hooks = plugin_root.join("hooks").join("hooks.json").is_file();

            if !has_skills && !has_agents && !has_mcp && !has_lsp && !has_hooks {
                tracing::debug!(
                    path = %plugin_root.display(),
                    "directory has no manifest and no recognized plugin components; skipping"
                );
                return;
            }

            PluginManifest {
                name,
                version: None,
                description: None,
                author: None,
                homepage: None,
                repository: None,
                license: None,
                keywords: vec![],
                skills: None,
                commands: None,
                agents: None,
                hooks: None,
                mcp_servers: None,
                lsp_servers: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                path = %plugin_root.display(),
                error = %e,
                "failed to load plugin manifest; skipping"
            );
            return;
        }
    };

    // Determine trust status
    // Exhaustive match so a new PluginScope variant is a compile error rather than a silent default
    let trusted = match scope {
        PluginScope::CliOverride | PluginScope::User => true,
        PluginScope::ConfigPath => {
            TrustStore::is_config_path_auto_trusted(plugin_root)
                || trust_store.is_trusted(plugin_root)
        }
        // Project trust comes from folder-trust (passed by the caller)
        PluginScope::Project => project_trusted,
    };

    let id = PluginId::new(scope, &canonical, &manifest.name);

    // Resolve component paths
    let skill_dirs = manifest.skill_dirs(plugin_root);
    let command_dirs = manifest.command_dirs(plugin_root);
    let agent_dirs = manifest.agent_dirs(plugin_root);
    let hooks_path = manifest.hooks_path(plugin_root);
    let mcp_config_path = manifest.mcp_config_path(plugin_root);
    let lsp_config_path = manifest.lsp_config_path(plugin_root);

    candidates.push(DiscoveredPlugin {
        manifest,
        id,
        root: plugin_root.to_path_buf(),
        canonical_root: canonical,
        scope,
        origin,
        trusted,
        skill_dirs,
        command_dirs,
        agent_dirs,
        hooks_path,
        mcp_config_path,
        lsp_config_path,
        conflict: None,
    });
}

/// Within each name group, keep only the highest-priority candidate (lowest scope ordinal).
/// Log warnings for dropped duplicates.
fn resolve_name_conflicts(candidates: &mut Vec<DiscoveredPlugin>) {
    let mut name_map: HashMap<String, usize> = HashMap::new();
    let mut to_remove: Vec<usize> = Vec::new();
    // (winner_idx, conflict message) pairs to apply after the scan.
    let mut conflict_msgs: Vec<(usize, String)> = Vec::new();

    for (idx, candidate) in candidates.iter().enumerate() {
        let name = candidate.manifest.name.clone();
        match name_map.get(&name) {
            Some(&existing_idx) => {
                let Some(existing) = candidates.get(existing_idx) else {
                    continue;
                };
                // A lower scope ordinal means higher priority
                if (candidate.scope as u8) < (existing.scope as u8) {
                    // New candidate wins
                    tracing::warn!(
                        plugin_name = %name,
                        winner = %candidate.root.display(),
                        loser = %existing.root.display(),
                        "plugin name collision resolved by scope precedence"
                    );
                    let msg = format!(
                        "Name collision: shadowing \"{}\" from {}",
                        name,
                        existing.root.display()
                    );
                    conflict_msgs.push((idx, msg));
                    to_remove.push(existing_idx);
                    name_map.insert(name, idx);
                } else {
                    // Existing wins
                    tracing::warn!(
                        plugin_name = %name,
                        winner = %existing.root.display(),
                        loser = %candidate.root.display(),
                        "plugin name collision resolved by scope precedence"
                    );
                    let msg = format!(
                        "Name collision: shadowing \"{}\" from {}",
                        name,
                        candidate.root.display()
                    );
                    conflict_msgs.push((existing_idx, msg));
                    to_remove.push(idx);
                }
            }
            None => {
                name_map.insert(name, idx);
            }
        }
    }

    // Apply conflict messages to winners.
    for (idx, msg) in conflict_msgs {
        if let Some(candidate) = candidates.get_mut(idx) {
            candidate.conflict = Some(msg);
        }
    }

    // Remove losers (reverse order to preserve indices)
    to_remove.sort_unstable();
    to_remove.dedup();
    for idx in to_remove.into_iter().rev() {
        candidates.remove(idx);
    }
}

// ── Compat installed_plugins.json types ───────────────────────────────

/// Compat `installed_plugins.json` format.
#[derive(serde::Deserialize)]
struct ClaudeInstalledPlugins {
    #[serde(default)]
    plugins: HashMap<String, Vec<ClaudeInstalledEntry>>,
}

/// A single install entry within `installed_plugins.json`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeInstalledEntry {
    install_path: PathBuf,
    scope: Option<String>,
    project_path: Option<PathBuf>,
}

/// Whether a compat install entry is visible for this session's `cwd`.
/// Project-tied entries are visible only when `cwd` is under `project_path`.
/// A missing project path cannot prove the session is in the project, so those entries stay hidden.
fn claude_install_visible(
    scope: Option<&str>,
    project_path: Option<&Path>,
    cwd: Option<&Path>,
) -> bool {
    let has_project_path = project_path.is_some_and(|p| !p.as_os_str().is_empty());
    let project_gated = matches!(scope, Some("local") | Some("project")) || has_project_path;
    if !project_gated {
        return true;
    }
    let Some(project) = project_path.filter(|p| !p.as_os_str().is_empty()) else {
        return false;
    };
    let Some(cwd) = cwd else {
        return false;
    };
    let cwd = dunce::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let project = dunce::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    cwd.starts_with(&project)
}

/// Read plugin names and install paths from compat `installed_plugins.json`.
/// Keys are `"plugin-name@marketplace"`. Returns empty on any error.
/// Project-tied entries are filtered by `cwd` vs `projectPath`.
fn read_claude_installed_plugins(
    json_path: &Path,
    cwd: Option<&Path>,
) -> Vec<(String, Option<String>, PathBuf)> {
    let content = match std::fs::read_to_string(json_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let registry: ClaudeInstalledPlugins = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                path = %json_path.display(),
                error = %e,
                "failed to parse installed_plugins.json"
            );
            return vec![];
        }
    };

    registry
        .plugins
        .into_iter()
        .flat_map(|(key, entries)| {
            let mut parts = key.splitn(2, '@');
            let name = parts.next().unwrap_or(&key).to_string();
            let marketplace = parts.next().map(String::from);
            entries
                .into_iter()
                .filter(|entry| {
                    claude_install_visible(
                        entry.scope.as_deref(),
                        entry.project_path.as_deref(),
                        cwd,
                    )
                })
                .map(move |entry| (name.clone(), marketplace.clone(), entry.install_path))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manifest_plugin(tmp: &Path, name: &str) -> PathBuf {
        let plugin_dir = tmp.join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            format!(r#"{{"name": "{}"}}"#, name),
        )
        .unwrap();
        plugin_dir
    }

    fn make_convention_plugin(tmp: &Path, name: &str) -> PathBuf {
        let plugin_dir = tmp.join(name);
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
        plugin_dir
    }

    #[test]
    fn user_plugin_dirs_are_grok_and_claude_only_no_legacy() {
        let home = Path::new("/home/u");
        let grok = Path::new("/custom/grokhome");
        let dirs = user_plugin_dirs(Some(home), Some(grok));
        assert!(dirs.contains(&(grok.join("plugins"), PluginOrigin::UserGrok)));
        assert!(dirs.contains(&(
            home.join(".claude").join("plugins"),
            PluginOrigin::UserClaude
        )));
        // Plugins are not discovered from the legacy ~/.grok tree.
        assert!(
            !dirs
                .iter()
                .any(|(p, _)| p == &home.join(".grok").join("plugins"))
        );
    }

    #[test]
    fn user_plugin_dirs_empty_without_home_or_grok() {
        assert!(user_plugin_dirs(None, None).is_empty());
    }

    #[test]
    fn discover_cli_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = make_manifest_plugin(tmp.path(), "cli-tool");

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        collect_plugin(
            &plugin_dir,
            PluginScope::CliOverride,
            PluginOrigin::CliOverride,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
        let Some(c) = candidates.first() else {
            panic!("expected one candidate: {candidates:?}");
        };
        assert_eq!(c.plugin_name(), "cli-tool");
        assert_eq!(c.scope, PluginScope::CliOverride);
        assert_eq!(&c.origin, &PluginOrigin::CliOverride);
        assert!(c.trusted);
    }

    #[test]
    fn project_plugins_dir_origin_distinguishes_grok_and_claude() {
        assert_eq!(
            project_plugins_dir_origin(Path::new("/repo/.grok/plugins")),
            PluginOrigin::ProjectGrok
        );
        assert_eq!(
            project_plugins_dir_origin(Path::new("/repo/.claude/plugins")),
            PluginOrigin::ProjectClaude
        );
    }

    #[test]
    fn discover_user_plugins() {
        let tmp = tempfile::tempdir().unwrap();

        // Create ~/.grok/plugins/ structure
        let grok_plugins = tmp.path().join(".grok").join("plugins");
        std::fs::create_dir_all(&grok_plugins).unwrap();
        make_manifest_plugin(&grok_plugins, "user-tool");

        // Override home dir by directly scanning
        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        scan_plugin_dir(
            &grok_plugins,
            PluginScope::User,
            PluginOrigin::UserGrok,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
        let Some(c) = candidates.first() else {
            panic!("expected one candidate: {candidates:?}");
        };
        assert_eq!(c.plugin_name(), "user-tool");
        assert!(c.trusted);
    }

    #[test]
    fn discover_convention_plugin_no_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join("plugins");
        make_convention_plugin(&plugins_dir, "my-tool");

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        scan_plugin_dir(
            &plugins_dir,
            PluginScope::User,
            PluginOrigin::UserGrok,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates.first().map(|c| c.plugin_name()), Some("my-tool"));
    }

    #[test]
    fn installed_plugins_load_at_registered_subdir_and_root() {
        use crate::plugins::install_registry::{
            InstallKind, InstallRegistry, InstalledRepo, RepoPlugin,
        };
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("installed-plugins");

        let sub_key = "acme-deadbeef";
        let sub_repo_path = install_dir.join(sub_key);
        let sub_plugin_dir = sub_repo_path.join("plugins").join("acme");
        std::fs::create_dir_all(&sub_plugin_dir).unwrap();
        std::fs::write(sub_plugin_dir.join("plugin.json"), r#"{"name": "acme"}"#).unwrap();

        let root_key = "cloud-cafef00d";
        let root_repo_path = install_dir.join(root_key);
        std::fs::create_dir_all(&root_repo_path).unwrap();
        std::fs::write(root_repo_path.join("plugin.json"), r#"{"name": "cloud"}"#).unwrap();

        let mut registry = InstallRegistry::empty(install_dir);
        registry.insert(
            sub_key.to_string(),
            InstalledRepo {
                kind: InstallKind::Local {
                    source_path: sub_repo_path.clone(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: sub_repo_path,
                plugins: HashMap::from([(
                    "acme".to_string(),
                    RepoPlugin {
                        subdir: Some("plugins/acme".to_string()),
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );
        registry.insert(
            root_key.to_string(),
            InstalledRepo {
                kind: InstallKind::Local {
                    source_path: root_repo_path.clone(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: root_repo_path,
                plugins: HashMap::from([(
                    "cloud".to_string(),
                    RepoPlugin {
                        subdir: None,
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        collect_installed_plugins(
            &registry,
            PluginScope::User,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        let names: Vec<&str> = candidates.iter().map(|p| p.plugin_name()).collect();
        assert_eq!(candidates.len(), 2, "got {names:?}");
        assert!(
            names.contains(&"acme"),
            "subdir plugin should load, got {names:?}"
        );
        assert!(
            names.contains(&"cloud"),
            "root plugin should load, got {names:?}"
        );
        for p in &candidates {
            assert_eq!(
                p.origin,
                PluginOrigin::MarketplaceInstall {
                    source_name: None,
                    git_url: None,
                },
                "direct local installs carry neither source name nor git URL"
            );
        }
    }

    #[test]
    fn installed_plugins_record_marketplace_and_git_origin() {
        use crate::plugins::install_registry::{
            InstallKind, InstallRegistry, InstalledRepo, MarketplaceProvenance, RepoPlugin,
        };
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("installed-plugins");

        let mp_repo_path = install_dir.join("mp-plugin-11111111");
        std::fs::create_dir_all(&mp_repo_path).unwrap();
        std::fs::write(mp_repo_path.join("plugin.json"), r#"{"name": "mp-plugin"}"#).unwrap();

        let git_repo_path = install_dir.join("git-plugin-22222222");
        std::fs::create_dir_all(&git_repo_path).unwrap();
        std::fs::write(
            git_repo_path.join("plugin.json"),
            r#"{"name": "git-plugin"}"#,
        )
        .unwrap();

        let mut registry = InstallRegistry::empty(install_dir);
        registry.insert(
            "mp-plugin-11111111".to_string(),
            InstalledRepo {
                kind: InstallKind::Git {
                    url: "https://example.com/mp.git".to_string(),
                    git_ref: None,
                    commit: "abc".to_string(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: mp_repo_path,
                plugins: HashMap::from([(
                    "mp-plugin".to_string(),
                    RepoPlugin {
                        subdir: None,
                        version: None,
                    },
                )]),
                marketplace: Some(MarketplaceProvenance {
                    source_url_or_path: "https://example.com/mp.git".to_string(),
                    source_display_name: "Demo Marketplace".to_string(),
                    plugin_subdir: "plugins/mp-plugin".to_string(),
                }),
            },
        );
        registry.insert(
            "git-plugin-22222222".to_string(),
            InstalledRepo {
                kind: InstallKind::Git {
                    url: "https://github.com/owner/repo.git".to_string(),
                    git_ref: None,
                    commit: "def".to_string(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: git_repo_path,
                plugins: HashMap::from([(
                    "git-plugin".to_string(),
                    RepoPlugin {
                        subdir: None,
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        collect_installed_plugins(
            &registry,
            PluginScope::User,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        let mp = candidates
            .iter()
            .find(|p| p.plugin_name() == "mp-plugin")
            .unwrap();
        assert_eq!(
            mp.origin,
            PluginOrigin::MarketplaceInstall {
                source_name: Some("Demo Marketplace".to_string()),
                git_url: Some("https://example.com/mp.git".to_string()),
            }
        );

        let git = candidates
            .iter()
            .find(|p| p.plugin_name() == "git-plugin")
            .unwrap();
        assert_eq!(
            git.origin,
            PluginOrigin::MarketplaceInstall {
                source_name: None,
                git_url: Some("https://github.com/owner/repo.git".to_string()),
            }
        );
    }

    #[test]
    fn installed_plugins_skip_subdir_that_escapes_repo() {
        use crate::plugins::install_registry::{
            InstallKind, InstallRegistry, InstalledRepo, RepoPlugin,
        };
        use std::collections::HashMap;

        let tmp = tempfile::tempdir().unwrap();
        let install_dir = tmp.path().join("installed-plugins");

        let escaped_dir = install_dir.join("escaped");
        std::fs::create_dir_all(&escaped_dir).unwrap();
        std::fs::write(
            escaped_dir.join("plugin.json"),
            r#"{"name": "escaped-evil"}"#,
        )
        .unwrap();

        let evil_key = "evil-deadbeef";
        let evil_repo_path = install_dir.join(evil_key);
        std::fs::create_dir_all(&evil_repo_path).unwrap();

        let good_key = "good-cafef00d";
        let good_repo_path = install_dir.join(good_key);
        let good_plugin_dir = good_repo_path.join("plugins").join("good");
        std::fs::create_dir_all(&good_plugin_dir).unwrap();
        std::fs::write(good_plugin_dir.join("plugin.json"), r#"{"name": "good"}"#).unwrap();

        let mut registry = InstallRegistry::empty(install_dir);
        registry.insert(
            evil_key.to_string(),
            InstalledRepo {
                kind: InstallKind::Local {
                    source_path: evil_repo_path.clone(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: evil_repo_path,
                plugins: HashMap::from([(
                    "escaped-evil".to_string(),
                    RepoPlugin {
                        subdir: Some("../escaped".to_string()),
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );
        registry.insert(
            good_key.to_string(),
            InstalledRepo {
                kind: InstallKind::Local {
                    source_path: good_repo_path.clone(),
                    subdir: None,
                },
                installed_at: String::new(),
                updated_at: String::new(),
                path: good_repo_path,
                plugins: HashMap::from([(
                    "good".to_string(),
                    RepoPlugin {
                        subdir: Some("plugins/good".to_string()),
                        version: None,
                    },
                )]),
                marketplace: None,
            },
        );

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        collect_installed_plugins(
            &registry,
            PluginScope::User,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        let names: Vec<&str> = candidates.iter().map(|p| p.plugin_name()).collect();
        assert!(
            names.contains(&"good"),
            "legit subdir plugin should load, got {names:?}"
        );
        assert!(
            !names.contains(&"escaped-evil"),
            "escaping subdir must be skipped, got {names:?}"
        );
    }

    #[test]
    fn dedup_by_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = make_manifest_plugin(tmp.path(), "dup-test");

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        // Add same plugin twice
        collect_plugin(
            &plugin_dir,
            PluginScope::CliOverride,
            PluginOrigin::CliOverride,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );
        collect_plugin(
            &plugin_dir,
            PluginScope::CliOverride,
            PluginOrigin::CliOverride,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn name_conflict_higher_priority_wins() {
        let tmp = tempfile::tempdir().unwrap();

        // Create two plugins with the same name but different scopes
        let cli_dir = tmp.path().join("cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(cli_dir.join("plugin.json"), r#"{"name": "my-plugin"}"#).unwrap();

        let user_dir = tmp.path().join("user-plugins");
        std::fs::create_dir_all(&user_dir).unwrap();
        let user_plugin = user_dir.join("my-plugin");
        std::fs::create_dir_all(&user_plugin).unwrap();
        std::fs::write(user_plugin.join("plugin.json"), r#"{"name": "my-plugin"}"#).unwrap();

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();

        // Add CLI plugin first (higher priority)
        collect_plugin(
            &cli_dir,
            PluginScope::CliOverride,
            PluginOrigin::CliOverride,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );
        // Add user plugin (lower priority)
        collect_plugin(
            &user_plugin,
            PluginScope::User,
            PluginOrigin::UserGrok,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        resolve_name_conflicts(&mut candidates);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates.first().map(|c| c.scope),
            Some(PluginScope::CliOverride)
        );
    }

    #[test]
    fn plugin_id_format() {
        let id = PluginId::new(
            PluginScope::User,
            Path::new("/home/user/.grok/plugins/my-plugin"),
            "my-plugin",
        );
        assert!(id.0.starts_with("user/"));
        assert!(id.0.ends_with("/my-plugin"));
        // Format: user/<hex8>/my-plugin
        let parts: Vec<&str> = id.0.split('/').collect();
        let [scope, hex, name] = parts.as_slice() else {
            panic!("expected three id parts: {parts:?}");
        };
        assert_eq!(*scope, "user");
        assert_eq!(hex.len(), 8);
        assert_eq!(*name, "my-plugin");
    }

    #[test]
    fn plugin_id_deterministic() {
        let id1 = PluginId::new(PluginScope::User, Path::new("/same/path"), "test");
        let id2 = PluginId::new(PluginScope::User, Path::new("/same/path"), "test");
        assert_eq!(id1, id2);
    }

    #[test]
    fn plugin_id_different_paths() {
        let id1 = PluginId::new(PluginScope::User, Path::new("/path/a"), "test");
        let id2 = PluginId::new(PluginScope::User, Path::new("/path/b"), "test");
        assert_ne!(id1, id2);
    }

    #[test]
    fn empty_dir_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let empty_dir = tmp.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        collect_plugin(
            &empty_dir,
            PluginScope::User,
            PluginOrigin::UserGrok,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        // No manifest and no skills/agents/mcp/hooks means the dir is skipped
        assert!(candidates.is_empty());
    }

    #[test]
    fn project_plugin_untrusted_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = make_manifest_plugin(tmp.path(), "project-tool");

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        // Untrusted folder (project_trusted = false) blocks the Project plugin.
        collect_plugin(
            &plugin_dir,
            PluginScope::Project,
            PluginOrigin::ProjectGrok,
            &trust,
            false,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
        assert!(!candidates.first().is_some_and(|c| c.trusted));
    }

    #[test]
    fn project_plugin_trusted_when_folder_trusted() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = make_manifest_plugin(tmp.path(), "trusted-tool");

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        // Trusted folder (project_trusted = true) allows the Project plugin.
        collect_plugin(
            &plugin_dir,
            PluginScope::Project,
            PluginOrigin::ProjectGrok,
            &trust,
            true,
            &mut seen,
            &mut candidates,
        );

        assert_eq!(candidates.len(), 1);
        assert!(candidates.first().is_some_and(|c| c.trusted));
    }

    #[test]
    fn non_project_scopes_unaffected_by_project_trusted() {
        // project_trusted = false gates Project scope only: CLI/User stay auto-trusted and ConfigPath keeps using its own trust store
        let tmp = tempfile::tempdir().unwrap();
        let cli_dir = make_manifest_plugin(tmp.path(), "cli-tool");
        let user_dir = make_manifest_plugin(tmp.path(), "user-tool");
        let config_dir = make_manifest_plugin(tmp.path(), "config-tool");

        let mut trust = TrustStore::load_from(tmp.path().join("trust"));
        trust.grant_trust(&config_dir).unwrap();

        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for (dir, scope, origin) in [
            (
                &cli_dir,
                PluginScope::CliOverride,
                PluginOrigin::CliOverride,
            ),
            (&user_dir, PluginScope::User, PluginOrigin::UserGrok),
            (
                &config_dir,
                PluginScope::ConfigPath,
                PluginOrigin::ConfigPath,
            ),
        ] {
            collect_plugin(
                dir,
                scope,
                origin,
                &trust,
                false,
                &mut seen,
                &mut candidates,
            );
        }

        assert_eq!(candidates.len(), 3);
        assert!(
            candidates.iter().all(|c| c.trusted),
            "CLI/User/ConfigPath plugins must stay trusted under project_trusted=false"
        );
    }

    #[test]
    fn discover_real_project_plugin_gated_on_project_trusted() {
        // Drives discover_plugins end to end with a repo-local `.grok/plugins/<x>/` plugin that has an MCP component
        // The plugin is trusted iff the folder-trust verdict (project_trusted) allows it
        // The plugin is found by name so any user-scoped plugins on the test host are irrelevant
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".grok").join("plugins").join("proj-mcp");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("plugin.json"), r#"{"name": "proj-mcp"}"#).unwrap();
        std::fs::write(plugin_dir.join(".mcp.json"), r#"{"mcpServers":{}}"#).unwrap();

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let config = DiscoveryConfig::default();

        // Untrusted folder: the project plugin comes back blocked.
        let untrusted = discover_plugins(Some(tmp.path()), &config, &trust, false);
        let p = untrusted
            .iter()
            .find(|p| p.manifest.name == "proj-mcp")
            .expect("project plugin discovered");
        assert_eq!(p.scope, PluginScope::Project);
        assert_eq!(p.origin, PluginOrigin::ProjectGrok);
        assert!(!p.trusted, "untrusted folder must block the project plugin");

        // Trusted folder: the same plugin is allowed.
        let trusted = discover_plugins(Some(tmp.path()), &config, &trust, true);
        let p = trusted
            .iter()
            .find(|p| p.manifest.name == "proj-mcp")
            .expect("project plugin discovered");
        assert!(p.trusted, "trusted folder must allow the project plugin");
    }

    #[test]
    fn discover_project_claude_plugin_records_claude_origin() {
        // Unique name: discover_plugins also scans the dev machine's real user dirs, and this test finds its plugin by name
        let name = format!("proj-claude-tool-{}", std::process::id());
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join(".claude").join("plugins").join(&name);
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            format!(r#"{{"name": "{name}"}}"#),
        )
        .unwrap();

        let trust = TrustStore::load_from(tmp.path().join("trust"));
        let config = DiscoveryConfig::default();
        let discovered = discover_plugins(Some(tmp.path()), &config, &trust, true);
        let p = discovered
            .iter()
            .find(|p| p.manifest.name == name)
            .expect("project claude plugin discovered");
        assert_eq!(p.scope, PluginScope::Project);
        assert_eq!(p.origin, PluginOrigin::ProjectClaude);
    }

    #[test]
    fn read_claude_installed_plugins_json() {
        let tmp = tempfile::tempdir().unwrap();

        // Create fake plugin directories matching the compat cache layout
        let cache = tmp.path().join("cache");
        let plugin_a = cache.join("marketplace-a").join("my-lsp").join("1.0.0");
        let plugin_b = cache.join("marketplace-b").join("my-tool").join("2.0.0");
        std::fs::create_dir_all(plugin_a.join("skills")).unwrap();
        std::fs::create_dir_all(&plugin_b).unwrap();
        std::fs::write(plugin_b.join("plugin.json"), r#"{"name": "my-tool"}"#).unwrap();

        // Write installed_plugins.json
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "version": 2,
            "plugins": {
                "my-lsp@marketplace-a": [{
                    "scope": "user",
                    "installPath": plugin_a.to_string_lossy(),
                    "version": "1.0.0"
                }],
                "my-tool@marketplace-b": [{
                    "scope": "user",
                    "installPath": plugin_b.to_string_lossy(),
                    "version": "2.0.0"
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        let results = read_claude_installed_plugins(&json_path, None);
        assert_eq!(results.len(), 2);

        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"my-lsp"));
        assert!(names.contains(&"my-tool"));

        let paths: Vec<&PathBuf> = results.iter().map(|(_, _, p)| p).collect();
        assert!(paths.contains(&&plugin_a));
        assert!(paths.contains(&&plugin_b));

        let lsp = results.iter().find(|(n, _, _)| n == "my-lsp").unwrap();
        assert_eq!(lsp.1.as_deref(), Some("marketplace-a"));
        let tool = results.iter().find(|(n, _, _)| n == "my-tool").unwrap();
        assert_eq!(tool.1.as_deref(), Some("marketplace-b"));
    }

    #[test]
    fn read_claude_installed_plugins_key_without_marketplace() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("bare");
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": { "bare": [{ "installPath": plugin.to_string_lossy() }] }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        let results = read_claude_installed_plugins(&json_path, None);
        assert_eq!(results.len(), 1);
        let Some(r) = results.first() else {
            panic!("expected one result: {results:?}");
        };
        assert_eq!(r.0, "bare");
        assert_eq!(r.1, None);
    }

    #[test]
    fn claude_local_install_included_when_cwd_under_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("src");
        let plugin = tmp.path().join("cache").join("local-plug");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "local-plug@mp": [{
                    "scope": "local",
                    "projectPath": project.to_string_lossy(),
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        let results = read_claude_installed_plugins(&json_path, Some(&nested));
        assert_eq!(results.len(), 1);
        let Some(r) = results.first() else {
            panic!("expected one result: {results:?}");
        };
        assert_eq!(r.0, "local-plug");
        assert_eq!(r.2, plugin);
    }

    #[test]
    fn claude_local_install_excluded_when_cwd_outside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let other = tmp.path().join("other");
        let plugin = tmp.path().join("cache").join("local-plug");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "local-plug@mp": [{
                    "scope": "local",
                    "projectPath": project.to_string_lossy(),
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        let results = read_claude_installed_plugins(&json_path, Some(&other));
        assert!(results.is_empty());
    }

    #[test]
    fn claude_user_and_unscoped_installs_included_regardless_of_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let other = tmp.path().join("other");
        let user_plugin = tmp.path().join("cache").join("user-plug");
        let bare_plugin = tmp.path().join("cache").join("bare-plug");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&user_plugin).unwrap();
        std::fs::create_dir_all(&bare_plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "user-plug@mp": [{
                    "scope": "user",
                    "installPath": user_plugin.to_string_lossy()
                }],
                "bare-plug@mp": [{
                    "installPath": bare_plugin.to_string_lossy()
                }],
                "local-plug@mp": [{
                    "scope": "local",
                    "projectPath": project.to_string_lossy(),
                    "installPath": tmp.path().join("cache").join("local-plug").to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        let results = read_claude_installed_plugins(&json_path, Some(&other));
        let names: Vec<&str> = results.iter().map(|(n, _, _)| n.as_str()).collect();
        assert!(names.contains(&"user-plug"));
        assert!(names.contains(&"bare-plug"));
        assert!(!names.contains(&"local-plug"));
    }

    #[test]
    fn claude_local_install_excluded_without_project_path_or_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin = tmp.path().join("cache").join("local-plug");
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "no-path@mp": [{
                    "scope": "local",
                    "installPath": plugin.to_string_lossy()
                }],
                "empty-path@mp": [{
                    "scope": "local",
                    "projectPath": "",
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        assert!(read_claude_installed_plugins(&json_path, Some(tmp.path())).is_empty());

        let project = tmp.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let json = serde_json::json!({
            "plugins": {
                "has-path@mp": [{
                    "scope": "local",
                    "projectPath": project.to_string_lossy(),
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();
        assert!(read_claude_installed_plugins(&json_path, None).is_empty());
    }

    #[test]
    fn claude_install_visible_path_boundary_not_string_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let sibling = tmp.path().join("proj-other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(claude_install_visible(
            Some("local"),
            Some(project.as_path()),
            Some(project.as_path())
        ));
        assert!(!claude_install_visible(
            Some("local"),
            Some(project.as_path()),
            Some(sibling.as_path())
        ));
    }

    #[test]
    fn claude_project_install_excluded_when_cwd_outside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let other = tmp.path().join("other");
        let plugin = tmp.path().join("cache").join("team-plug");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "team-plug@mp": [{
                    "scope": "project",
                    "projectPath": project.to_string_lossy(),
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        assert!(read_claude_installed_plugins(&json_path, Some(&other)).is_empty());
        let results = read_claude_installed_plugins(&json_path, Some(&project));
        assert_eq!(results.len(), 1);
        assert_eq!(results.first().map(|r| r.0.as_str()), Some("team-plug"));
    }

    #[test]
    fn claude_entry_with_project_path_gated_even_without_local_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let other = tmp.path().join("other");
        let plugin = tmp.path().join("cache").join("path-only");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&plugin).unwrap();
        let json_path = tmp.path().join("installed_plugins.json");
        let json = serde_json::json!({
            "plugins": {
                "path-only@mp": [{
                    "projectPath": project.to_string_lossy(),
                    "installPath": plugin.to_string_lossy()
                }]
            }
        });
        std::fs::write(&json_path, serde_json::to_string(&json).unwrap()).unwrap();

        assert!(read_claude_installed_plugins(&json_path, Some(&other)).is_empty());
        assert_eq!(
            read_claude_installed_plugins(&json_path, Some(&project)).len(),
            1
        );
    }
}
