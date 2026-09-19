// Modified for Distill by Samuel Fajreldines, 2026.
//! `grok plugin` CLI subcommand: manage plugins and marketplace sources.
//!
//! Follows the `memory_cmd.rs`, `sessions_cmd.rs`, and `worktree_cmd` pattern: clap args and handler logic co-located in a dedicated module.
//! The pager's `main.rs` dispatches here with a one-liner.
//!
//! Business logic lives in `distill_shell::plugin` and lower crates (`distill-agent`, `distill-plugin-marketplace`).
//! This module is a thin CLI wrapper: parse args, call ops, format output, emit telemetry.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::Subcommand;
use serde::Serialize;

use distill_agent::plugins::install_registry::{InstallKind, InstallRegistry};
use distill_agent::plugins::manifest::{ManifestLoadResult, PluginManifest, load_manifest};
use distill_plugin_marketplace::SourceKind;
use distill_plugin_marketplace::git::SourceCacheLease;
use distill_shell::plugin::{self, RepoUpdateOutcome, UninstallError};

// ── JSON output types ───────────────────────────────────────────────

/// Typed entry for `grok plugin list --json`.
/// The `status` field is the discriminator.
/// `"installed"` entries have repo and path fields, `"available"` entries have description and component fields.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PluginEntry {
    Installed {
        name: String,
        repo_key: String,
        version: Option<String>,
        path: PathBuf,
        source: String,
        marketplace: Option<String>,
    },
    Available {
        name: String,
        version: Option<String>,
        description: Option<String>,
        marketplace: String,
        skill_count: usize,
        has_hooks: bool,
        has_agents: bool,
        has_mcp: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        components: Option<distill_hooks_plugins_types::PluginComponents>,
    },
}

/// Typed entry for `grok plugin marketplace list --json`.
#[derive(Serialize)]
struct MarketplaceSourceEntry {
    name: String,
    kind: String,
    source: MarketplaceSourceDetail,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MarketplaceSourceDetail {
    Git { url: String, branch: Option<String> },
    Local { path: PathBuf },
}

impl MarketplaceSourceDetail {
    fn kind(&self) -> &'static str {
        match self {
            Self::Git { .. } => "git",
            Self::Local { .. } => "local",
        }
    }
}

// ── CLI arg definitions ─────────────────────────────────────────────

#[derive(Debug, clap::Args, Clone)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum PluginCommand {
    /// List installed plugins
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
        /// Include available plugins from marketplace sources. Requires --json.
        #[arg(long, requires = "json")]
        available: bool,
    },
    /// Install a plugin from a git URL or local path
    Install {
        /// Git URL, GitHub shorthand (user/repo), or local path.
        /// Supports @ref suffix (e.g. user/repo@v1.0) and #subdir.
        source: String,
        /// Trust the plugin immediately (skip confirmation prompt).
        #[arg(long)]
        trust: bool,
    },
    /// Uninstall an installed plugin by name
    #[command(visible_alias = "rm", visible_alias = "remove")]
    Uninstall {
        /// Plugin name (as shown by `grok plugin list`).
        name: String,
        /// Skip confirmation for multi-plugin repos.
        #[arg(long)]
        confirm: bool,
        /// Preserve the plugin's persistent data directory.
        #[arg(long)]
        keep_data: bool,
    },
    /// Update installed plugin(s)
    Update {
        /// Plugin name to update. Omit to update all.
        name: Option<String>,
    },
    /// Enable a disabled plugin
    Enable {
        /// Plugin name to enable.
        name: String,
    },
    /// Disable a plugin without uninstalling it
    Disable {
        /// Plugin name to disable.
        name: String,
    },
    /// Show a plugin's component inventory
    Details {
        /// Plugin name.
        name: String,
    },
    /// Validate a plugin manifest
    Validate {
        /// Path to plugin directory (default: current directory).
        #[arg(default_value = ".")]
        path: String,
    },
    /// Create a release git tag from the plugin's manifest version
    Tag {
        /// Path to plugin directory (default: current directory).
        #[arg(default_value = ".")]
        path: String,
        /// Push the tag to the remote after creating it.
        #[arg(long)]
        push: bool,
        /// Create the tag even if the working tree is dirty or tag exists.
        #[arg(long, short = 'f')]
        force: bool,
        /// Print what would be tagged without creating the tag.
        #[arg(long)]
        dry_run: bool,
    },
    /// Manage marketplace sources
    Marketplace(MarketplaceArgs),
}

#[derive(Debug, clap::Args, Clone)]
pub struct MarketplaceArgs {
    #[command(subcommand)]
    pub command: MarketplaceCommand,
}

#[derive(Debug, Subcommand, Clone)]
pub enum MarketplaceCommand {
    /// List configured marketplace sources and their plugins
    List {
        /// Emit machine-readable JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Add a marketplace source (git URL, GitHub shorthand, or local path)
    Add {
        /// Git URL, GitHub shorthand (e.g. user/repo), or local directory path.
        url: String,
        /// Skip the reachability probe (e.g. for hosts only reachable on VPN).
        #[arg(long)]
        force: bool,
    },
    /// Remove a marketplace source and uninstall its plugins
    Remove {
        /// Name, git URL, or local path of the source to remove.
        source: String,
    },
    /// Refresh marketplace source(s) and sync git caches
    Update {
        /// Source URL to refresh. Omit to refresh all.
        name: Option<String>,
    },
}

// ── Helpers ─────────────────────────────────────────────────────────

fn kind_label(kind: &InstallKind) -> String {
    match kind {
        InstallKind::Git { url, .. } => format!("git: {url}"),
        InstallKind::Local { source_path, .. } => format!("local: {}", source_path.display()),
    }
}

fn print_component_summary(manifest: &PluginManifest, root: &Path) {
    let skills = manifest.skill_dirs(root);
    let commands = manifest.command_dirs(root);
    let agents = manifest.agent_dirs(root);
    let has_hooks = manifest.hooks_path(root).is_some() || manifest.inline_hooks().is_some();
    let has_mcp =
        manifest.mcp_config_path(root).is_some() || manifest.inline_mcp_servers().is_some();
    let has_lsp =
        manifest.lsp_config_path(root).is_some() || manifest.inline_lsp_servers().is_some();
    println!(
        "  components: {} skill dir(s), {} command dir(s), {} agent dir(s){}{}{}",
        skills.len(),
        commands.len(),
        agents.len(),
        if has_hooks { ", hooks" } else { "" },
        if has_mcp { ", MCP servers" } else { "" },
        if has_lsp { ", LSP servers" } else { "" },
    );
}

fn abbreviated_commit(c: Option<&str>) -> &str {
    c.and_then(|s| s.get(..7.min(s.len()))).unwrap_or("?")
}

fn trust_prompt(subject: &str, source_arg: &str) -> String {
    format!(
        "Installing {subject} requires confirmation.\n\
         Plugins can run hooks, MCP servers, and skills on your machine, so installation needs explicit trust.\n\
         \n\
         To proceed, re-run with --trust:\n  grok plugin install {source_arg} --trust"
    )
}

// ── Top-level dispatch ──────────────────────────────────────────────

pub async fn run(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List { json, available } => cmd_list(json, available),
        PluginCommand::Install { source, trust } => cmd_install(&source, trust),
        PluginCommand::Uninstall {
            name,
            confirm,
            keep_data,
        } => cmd_uninstall(&name, confirm, keep_data),
        PluginCommand::Update { name } => cmd_update(name.as_deref()),
        PluginCommand::Enable { name } => cmd_enable(&name),
        PluginCommand::Disable { name } => cmd_disable(&name),
        PluginCommand::Details { name } => cmd_details(&name),
        PluginCommand::Validate { path } => cmd_validate(&path),
        PluginCommand::Tag {
            path,
            push,
            force,
            dry_run,
        } => cmd_tag(&path, push, force, dry_run),
        PluginCommand::Marketplace(mp) => run_marketplace(mp.command).await,
    }
}

// ── Plugin subcommands ──────────────────────────────────────────────

fn cmd_list(json: bool, available: bool) -> Result<()> {
    let registry = InstallRegistry::load();
    let repos = registry.list();

    if json {
        let mut entries = installed_plugins(&repos);
        if available {
            entries.extend(available_plugins(&registry));
        }
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if repos.is_empty() {
        println!("No plugins installed. Run `grok plugin install --help` to get started.");
    } else {
        for (repo_key, repo) in &repos {
            let mp = repo
                .marketplace
                .as_ref()
                .map(|mp| format!(" ({})", mp.source_display_name))
                .unwrap_or_default();
            let names: Vec<&str> = repo.plugins.keys().map(|s| s.as_str()).collect();
            println!(
                "  {repo_key}: {} [{}]{mp}",
                names.join(", "),
                kind_label(&repo.kind)
            );
        }
    }
    Ok(())
}

fn installed_plugins(
    repos: &[(
        &str,
        &distill_agent::plugins::install_registry::InstalledRepo,
    )],
) -> Vec<PluginEntry> {
    repos
        .iter()
        .flat_map(|(repo_key, repo)| {
            let source = match &repo.kind {
                InstallKind::Git { url, .. } => url.clone(),
                InstallKind::Local { source_path, .. } => source_path.display().to_string(),
            };
            let marketplace = repo
                .marketplace
                .as_ref()
                .map(|mp| mp.source_display_name.clone());
            repo.plugins
                .iter()
                .map(move |(name, plugin)| PluginEntry::Installed {
                    name: name.clone(),
                    repo_key: repo_key.to_string(),
                    version: plugin.version.clone(),
                    path: repo.path.clone(),
                    source: source.clone(),
                    marketplace: marketplace.clone(),
                })
        })
        .collect()
}

fn available_plugins(registry: &InstallRegistry) -> Vec<PluginEntry> {
    // Policy-filtered: blocked marketplaces are neither scanned nor advertised.
    let sources = distill_shell::plugin::load_filtered_marketplace_sources();

    let mut entries = Vec::new();
    for source in &sources {
        let identity = source.identity();
        let root = resolve_marketplace_root(source);
        let Some((root, lease)) = root else { continue };

        for plugin in distill_plugin_marketplace::scan_marketplace(&root).entries {
            let already_installed =
                distill_plugin_marketplace::installer::find_installed_marketplace_plugin(
                    registry,
                    &identity,
                    &plugin.relative_path,
                )
                .is_some();
            if already_installed {
                continue;
            }
            entries.push(PluginEntry::Available {
                name: plugin.name,
                version: plugin.version,
                description: plugin.description,
                marketplace: source.name.clone(),
                skill_count: plugin.skill_count,
                has_hooks: plugin.has_hooks,
                has_agents: plugin.has_agents,
                has_mcp: plugin.has_mcp,
                components: plugin.components,
            });
        }
        drop(lease);
    }
    entries
}

fn resolve_marketplace_root(
    source: &distill_plugin_marketplace::MarketplaceSource,
) -> Option<(std::path::PathBuf, Option<SourceCacheLease>)> {
    match &source.kind {
        SourceKind::Local { path } if path.is_dir() => Some((path.clone(), None)),
        SourceKind::Git { url, branch } => {
            let cache = distill_plugin_marketplace::git::default_cache_root();
            distill_plugin_marketplace::git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                &cache,
                distill_plugin_marketplace::git::SyncMode::UseTtl,
            )
            .map(|lease| (lease.path.clone(), Some(lease)))
            .map_err(|e| tracing::warn!("failed to sync marketplace {url}: {e}"))
            .ok()
        }
        _ => None,
    }
}

fn install_kind(is_git: bool) -> distill_telemetry::events::InstallKind {
    if is_git {
        distill_telemetry::events::InstallKind::Git
    } else {
        distill_telemetry::events::InstallKind::Local
    }
}

fn log_plugin_installed(
    install_kind: distill_telemetry::events::InstallKind,
    success: bool,
    error_category: Option<String>,
) {
    distill_telemetry::session_ctx::log_event(distill_telemetry::events::PluginInstalled {
        install_kind,
        success,
        trust: true,
        error_category,
    });
}

fn cmd_install(source: &str, trust: bool) -> Result<()> {
    if let Some(mref) = distill_plugin_marketplace::install_resolve::parse_marketplace_ref(source) {
        return cmd_install_marketplace(source, &mref, trust);
    }

    let cwd = std::env::current_dir().unwrap_or_default();

    if !trust {
        use distill_agent::plugins::git_install::{self, InstallSource};
        let subject = match git_install::parse_install_source(source, &cwd) {
            InstallSource::Git { url, .. } => format!("from git repo {url}"),
            InstallSource::Local { path, .. } => format!("from directory {}", path.display()),
        };
        eprintln!("{}", trust_prompt(&subject, source));
        std::process::exit(1);
    }

    match plugin::install_plugin(source, &cwd) {
        Ok(outcome) => {
            for w in &outcome.warnings {
                tracing::warn!("{w}");
            }
            log_plugin_installed(install_kind(!outcome.is_local), true, None);
            println!(
                "Installed {} plugin(s) from {source}: {}",
                outcome.plugin_names.len(),
                outcome.plugin_names.join(", "),
            );
            Ok(())
        }
        Err(e) => {
            // On failure we don't know the kind; default to Git (matches canonical).
            log_plugin_installed(
                distill_telemetry::events::InstallKind::Git,
                false,
                Some(e.category()),
            );
            bail!("{e}");
        }
    }
}

fn cmd_install_marketplace(
    source: &str,
    mref: &distill_plugin_marketplace::install_resolve::MarketplaceRef,
    trust: bool,
) -> Result<()> {
    if !trust {
        let from = match &mref.qualifier {
            Some(qualifier) => match plugin::resolve_qualified_source_name(qualifier) {
                Ok(_display) => qualifier.clone(),
                Err(e) => bail!("{e}"),
            },
            None => match plugin::resolve_marketplace_source_name(&mref.name, None) {
                Ok(display) => display,
                Err(e) => bail!("{e}"),
            },
        };
        let subject = format!("\"{}\" from marketplace \"{from}\"", mref.name);
        eprintln!("{}", trust_prompt(&subject, source));
        std::process::exit(1);
    }

    match plugin::install_marketplace_plugin(&mref.name, mref.qualifier.as_deref()) {
        Ok(outcome) => {
            for w in &outcome.warnings {
                tracing::warn!("{w}");
            }
            if outcome.already_installed {
                log_plugin_installed(
                    install_kind(outcome.source_is_git),
                    false,
                    Some("already_installed".to_string()),
                );
                let update_name = outcome
                    .plugin_names
                    .first()
                    .map(String::as_str)
                    .unwrap_or(&mref.name);
                println!(
                    "Plugin \"{}\" is already installed from {}. \
                     Run `grok plugin update {}` to update it.",
                    mref.name, outcome.source_display_name, update_name,
                );
                return Ok(());
            }
            log_plugin_installed(install_kind(outcome.source_is_git), true, None);
            if let Some(note) = &outcome.other_copies_note {
                println!("{note}");
            }
            println!(
                "Installed {} plugin(s) from {}: {}",
                outcome.plugin_names.len(),
                outcome.source_display_name,
                outcome.plugin_names.join(", "),
            );
            Ok(())
        }
        Err(e) => {
            // On failure we don't know the kind; default to Git (matches canonical).
            log_plugin_installed(
                distill_telemetry::events::InstallKind::Git,
                false,
                Some(e.category()),
            );
            bail!("{e}");
        }
    }
}

fn cmd_uninstall(name: &str, confirm: bool, keep_data: bool) -> Result<()> {
    match plugin::uninstall_plugin(name, confirm, keep_data) {
        Ok(outcome) => {
            distill_telemetry::session_ctx::log_event(
                distill_telemetry::events::PluginUninstalled {
                    confirmed: true,
                    success: true,
                },
            );
            let suffix = if keep_data { " (data preserved)" } else { "" };
            println!(
                "Uninstalled {} plugin(s): {}{suffix}",
                outcome.removed_plugins.len(),
                outcome.removed_plugins.join(", "),
            );
            Ok(())
        }
        Err(UninstallError::NeedsConfirm {
            name,
            repo_key,
            other_plugins,
            total,
        }) => bail!(
            "Plugin \"{name}\" belongs to repo \"{repo_key}\" which also contains:\n\
             {}\n\n\
             Uninstalling will remove all {total} plugin(s). To proceed:\n\
               grok plugin uninstall {name} --confirm",
            other_plugins
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Err(
            e @ (UninstallError::NotFound { .. }
            | UninstallError::RegistryLock { .. }
            | UninstallError::RegistrySave { .. }),
        ) => {
            bail!("{e}")
        }
    }
}

fn cmd_update(name: Option<&str>) -> Result<()> {
    let outcomes = plugin::update_plugins(name).map_err(|e| anyhow::anyhow!("{e}"))?;

    if outcomes.is_empty() {
        println!("No installed plugins to update.");
        return Ok(());
    }

    for o in &outcomes {
        match o {
            RepoUpdateOutcome::Updated {
                repo_key,
                old_commit,
                new_commit,
            } => {
                println!(
                    "{repo_key}: updated ({} -> {})",
                    abbreviated_commit(old_commit.as_deref()),
                    abbreviated_commit(new_commit.as_deref()),
                );
            }
            RepoUpdateOutcome::AlreadyUpToDate { repo_key } => {
                println!("{repo_key}: already up to date");
            }
            RepoUpdateOutcome::Pinned { repo_key, ref_name } => {
                println!("{repo_key}: pinned to {ref_name}, skipping");
            }
            RepoUpdateOutcome::LiveLocal { repo_key } => {
                println!("{repo_key}: local symlink, already live");
            }
            RepoUpdateOutcome::Failed { repo_key, error } => {
                eprintln!("{repo_key}: update failed: {error}");
            }
        }
    }
    if let Some(summary) = update_failure_summary(&outcomes) {
        bail!("{summary}");
    }
    Ok(())
}

/// Nonzero-exit summary when any repo update failed (policy blocks, sync
/// errors), so scripts and CI can detect the failure; `None` when all passed.
fn update_failure_summary(outcomes: &[RepoUpdateOutcome]) -> Option<String> {
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o, RepoUpdateOutcome::Failed { .. }))
        .count();
    (failed > 0).then(|| format!("{failed} of {} plugin update(s) failed", outcomes.len()))
}

fn cmd_enable(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    if registry.find_plugin(name).is_none() {
        bail!(
            "Plugin \"{name}\" not found.\n\
               Run `grok plugin list` to see installed plugins."
        );
    }
    if let Err(e) = distill_shell::config::remove_disabled_plugin(name) {
        tracing::warn!("failed to remove from disabled list: {e}");
    }
    distill_shell::config::add_enabled_plugin(name)
        .map_err(|e| anyhow::anyhow!("Failed to enable plugin: {e}"))?;
    println!("Enabled plugin: {name}");
    Ok(())
}

fn cmd_disable(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    if registry.find_plugin(name).is_none() {
        bail!(
            "Plugin \"{name}\" not found.\n\
               Run `grok plugin list` to see installed plugins."
        );
    }
    if let Err(e) = distill_shell::config::remove_enabled_plugin(name) {
        tracing::warn!("failed to remove from enabled list: {e}");
    }
    distill_shell::config::add_disabled_plugin(name)
        .map_err(|e| anyhow::anyhow!("Failed to disable plugin: {e}"))?;
    println!("Disabled plugin: {name}");
    Ok(())
}

fn cmd_details(name: &str) -> Result<()> {
    let registry = InstallRegistry::load();
    let (repo_key, repo, _) = registry.find_plugin(name).ok_or_else(|| {
        anyhow::anyhow!(
            "Plugin \"{name}\" not found.\n\
             Run `grok plugin list` to see installed plugins."
        )
    })?;

    let mp = repo
        .marketplace
        .as_ref()
        .map(|mp| format!("\n  source: {}", mp.source_display_name))
        .unwrap_or_default();

    println!("{repo_key}");
    println!("  path: {}", repo.path.display());
    println!("  kind: {}{mp}", kind_label(&repo.kind));
    println!("  installed: {}", repo.installed_at);
    println!("  updated: {}", repo.updated_at);
    println!("  plugins ({}):", repo.plugins.len());
    for (pname, p) in &repo.plugins {
        let ver = p
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        let sub = p
            .subdir
            .as_deref()
            .map(|s| format!(" (subdir: {s})"))
            .unwrap_or_default();
        println!("    {pname}{ver}{sub}");
    }

    if let Ok(ManifestLoadResult::Found(manifest)) = load_manifest(&repo.path) {
        if let Some(ref desc) = manifest.description {
            println!("  description: {desc}");
        }
        print_component_summary(&manifest, &repo.path);
    }
    Ok(())
}

fn cmd_validate(path: &str) -> Result<()> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        bail!("Not a directory: {path}");
    }
    match load_manifest(&root) {
        Ok(ManifestLoadResult::Found(manifest)) => {
            manifest
                .validate()
                .map_err(|e| anyhow::anyhow!("Manifest validation failed: {e}"))?;
            println!("Plugin manifest is valid.");
            println!("  name: {}", manifest.name);
            if let Some(ref v) = manifest.version {
                println!("  version: {v}");
            }
            if let Some(ref d) = manifest.description {
                println!("  description: {d}");
            }
            print_component_summary(&manifest, &root);
            Ok(())
        }
        Ok(ManifestLoadResult::NotFound) => {
            println!(
                "No plugin.json found. Grok discovers skills, agents, and hooks \
                 automatically from standard directories. A manifest is only needed \
                 for custom paths or metadata."
            );
            Ok(())
        }
        Err(e) => bail!("Failed to load manifest: {e}"),
    }
}

fn cmd_tag(path: &str, push: bool, force: bool, dry_run: bool) -> Result<()> {
    let root = PathBuf::from(path);
    if !root.is_dir() {
        bail!("Not a directory: {path}");
    }
    let version = match load_manifest(&root) {
        Ok(ManifestLoadResult::Found(m)) => m.version.ok_or_else(|| {
            anyhow::anyhow!(
                "No `version` field in plugin.json. Set a version to use `grok plugin tag`."
            )
        })?,
        Ok(ManifestLoadResult::NotFound) => bail!("No plugin.json found in {path}."),
        Err(e) => bail!("Failed to load manifest: {e}"),
    };

    let tag = format!(
        "v{}",
        version
            .strip_prefix('v')
            .or_else(|| version.strip_prefix('V'))
            .unwrap_or(&version)
    );

    if !force {
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&root)
            .output()?;
        if !out.stdout.is_empty() {
            bail!("Working tree is dirty. Commit changes first, or use --force.");
        }
    }

    if dry_run {
        println!("Would create tag: {tag}");
        if push {
            println!("Would push tag to remote.");
        }
        return Ok(());
    }

    let mut cmd = std::process::Command::new("git");
    cmd.args(["tag", &tag]);
    if force {
        cmd.arg("--force");
    }
    let out = cmd.current_dir(&root).output()?;
    if !out.status.success() {
        bail!(
            "Failed to create tag: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    println!("Created tag: {tag}");

    if push {
        let mut push_cmd = std::process::Command::new("git");
        push_cmd.args(["push", "origin", &tag]);
        if force {
            push_cmd.arg("--force");
        }
        let out = push_cmd.current_dir(&root).output()?;
        if !out.status.success() {
            bail!(
                "Failed to push tag: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        println!("Pushed tag {tag} to origin.");
    }
    Ok(())
}

// ── Marketplace subcommands ─────────────────────────────────────────

async fn run_marketplace(cmd: MarketplaceCommand) -> Result<()> {
    // Policy-filtered: list/update must not touch blocked marketplaces.
    let sources = distill_shell::plugin::load_filtered_marketplace_sources();

    match cmd {
        MarketplaceCommand::List { json } => marketplace_list(&sources, json),
        MarketplaceCommand::Add { url, force } => marketplace_add(&url, force),
        // Remove is cleanup, not bypass: it must find blocked sources too.
        MarketplaceCommand::Remove { source } => {
            marketplace_remove(&distill_shell::plugin::load_marketplace_sources(), &source)
        }
        MarketplaceCommand::Update { name } => marketplace_update(&sources, name.as_deref()),
    }
}

fn marketplace_list(
    sources: &[distill_plugin_marketplace::MarketplaceSource],
    json: bool,
) -> Result<()> {
    if json {
        let entries: Vec<MarketplaceSourceEntry> = sources
            .iter()
            .map(|s| {
                let detail = match &s.kind {
                    SourceKind::Git { url, branch } => MarketplaceSourceDetail::Git {
                        url: url.clone(),
                        branch: branch.clone(),
                    },
                    SourceKind::Local { path } => {
                        MarketplaceSourceDetail::Local { path: path.clone() }
                    }
                };
                MarketplaceSourceEntry {
                    name: s.name.clone(),
                    kind: detail.kind().to_string(),
                    source: detail,
                }
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if sources.is_empty() {
        println!(
            "No marketplace sources configured.\n\
             Run `grok plugin marketplace add --help` to get started."
        );
    } else {
        for s in sources {
            println!("  {}: {}", s.name, s.identity());
        }
    }
    Ok(())
}

fn marketplace_add(url: &str, force: bool) -> Result<()> {
    use distill_shell::plugin::MarketplaceAddInput;

    let url = url.trim();
    if url.is_empty() {
        bail!("URL cannot be empty.");
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let input = plugin::classify_marketplace_add_input(url, &cwd);

    // Fail fast on missing local paths: otherwise a path input is stored as a git URL and only errors after network clone attempts
    if let MarketplaceAddInput::LocalPath(path) = &input
        && !path.is_dir()
    {
        bail!(
            "Local marketplace path not found (or is not a directory): {}",
            path.display()
        );
    }

    let identity = match &input {
        MarketplaceAddInput::GitUrl(u) => u.clone(),
        MarketplaceAddInput::LocalPath(p) => p.display().to_string(),
    };

    let allowlist =
        &distill_workspace::permission::resolution::managed_settings().marketplace_allowlist;
    if let Some(reason) = allowlist.add_block_reason(&identity) {
        bail!("Marketplace source blocked: {reason}");
    }

    // Dedupe against the FULL unfiltered source list by canonical git-URL identity, mirroring the
    // shell modal twin; the locked add core below re-checks under the flock.
    let existing = distill_shell::plugin::load_marketplace_sources();
    let already_configured = match &input {
        MarketplaceAddInput::GitUrl(git_url) => {
            use distill_workspace::permission::resolution::normalize_git_url;
            let normalized = normalize_git_url(git_url);
            existing.iter().any(|s| {
                matches!(&s.kind, SourceKind::Git { url: u, .. }
                    if normalize_git_url(u) == normalized)
            })
        }
        MarketplaceAddInput::LocalPath(path) => existing
            .iter()
            .any(|s| matches!(&s.kind, SourceKind::Local { path: p } if p == path)),
    };
    if already_configured {
        bail!("Marketplace source already configured: {identity}");
    }

    if !force && let MarketplaceAddInput::GitUrl(git_url) = &input {
        distill_plugin_marketplace::git::probe_git_remote(git_url).map_err(|e| {
            anyhow::anyhow!(
                "{e}\nNot adding \"{url}\": it doesn't look like a reachable git repository. \
                 Re-run with --force to add it anyway (e.g. a host only reachable on VPN)."
            )
        })?;
    }

    let is_official = matches!(&input, MarketplaceAddInput::GitUrl(u)
        if distill_plugin_marketplace::is_official_source_url(u));
    let name = if is_official {
        distill_plugin_marketplace::OFFICIAL_SOURCE_NAME.to_string()
    } else {
        match &input {
            MarketplaceAddInput::GitUrl(u) => plugin::name_from_url(u),
            MarketplaceAddInput::LocalPath(p) => plugin::name_from_path(p),
        }
    };

    // Shared locked add core (same as the shell modal): init flock across the
    // read-modify-write, idempotent normalized dedup, atomic replace.
    let distill_home = distill_config::distill_home();
    let _flock = distill_shell::util::config::acquire_init_lock(&distill_home)?;
    plugin::add_marketplace_source(
        &distill_home.join(distill_config::USER_CONFIG_FILENAME),
        &name,
        &input,
        is_official,
    )?;

    println!("Added marketplace source: {name} ({identity})");
    Ok(())
}

/// Resolve `remove` input to a source: exact name match first, then the same URL or path matching `marketplace add` uses.
fn find_removal_source<'a>(
    sources: &'a [distill_plugin_marketplace::MarketplaceSource],
    input: &str,
    cwd: &Path,
) -> Result<&'a distill_plugin_marketplace::MarketplaceSource, String> {
    let mut by_name = sources.iter().filter(|s| s.name == input);
    if let Some(first) = by_name.next() {
        if by_name.next().is_some() {
            let identities: Vec<String> = sources
                .iter()
                .filter(|s| s.name == input)
                .map(|s| s.identity())
                .collect();
            return Err(format!(
                "Multiple sources are named \"{input}\"; remove by URL/path instead: {}",
                identities.join(", ")
            ));
        }
        return Ok(first);
    }

    let expanded = plugin::expand_github_shorthand(input);
    // Full git-URL normalization (`.git`, host case, scp-vs-https spelling),
    // same matching `marketplace add` dedupes with.
    use distill_workspace::permission::resolution::normalize_git_url;
    let norm = normalize_git_url(input);
    let exp_norm = normalize_git_url(&expanded);
    // Loaded local sources carry expanded paths, so expand `~` and relative inputs the same way `marketplace add` does before comparing
    let local_input = match plugin::classify_marketplace_add_input(input, cwd) {
        distill_shell::plugin::MarketplaceAddInput::LocalPath(p) => Some(p),
        _ => None,
    };

    sources
        .iter()
        .find(|s| match &s.kind {
            SourceKind::Git { url: u, .. } => {
                let un = normalize_git_url(u);
                un == norm || un == exp_norm
            }
            SourceKind::Local { path } => {
                path.display().to_string() == input
                    || local_input.as_ref().is_some_and(|p| p == path)
            }
        })
        .ok_or_else(|| {
            let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
            if names.is_empty() {
                format!("Marketplace source \"{input}\" not found; no sources are configured.")
            } else {
                format!(
                    "Marketplace source \"{input}\" not found. Configured sources: {}",
                    names.join(", ")
                )
            }
        })
}

fn marketplace_remove(
    sources: &[distill_plugin_marketplace::MarketplaceSource],
    name_or_url: &str,
) -> Result<()> {
    let input = name_or_url.trim();
    if input.is_empty() {
        bail!("Provide the source name, git URL, or local path to remove.");
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let source = find_removal_source(sources, input, &cwd).map_err(|e| anyhow::anyhow!("{e}"))?;

    let identity = source.identity();

    // Uninstall + config rewrite under the init flock with atomic replace, mirroring the shell
    // modal twin (`remove_source_locked`); an unlocked remove is the lost-update race.
    let distill_home = distill_config::distill_home();
    let _flock = distill_shell::util::config::acquire_init_lock(&distill_home)?;

    let uninstalled = plugin::uninstall_marketplace_source_plugins(&identity)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Shared remove-write core (same as the shell modal): config.toml with the
    // official flag folded in, JSON stores as fallback.
    let config_path = distill_home.join(distill_config::USER_CONFIG_FILENAME);
    if plugin::remove_marketplace_source_from_stores(&config_path, &identity)?
        == plugin::MarketplaceSourceRemoval::NotFound
    {
        eprintln!(
            "Warning: source was found but could not be removed from config files.\n\
             It may be defined in a managed or read-only settings file."
        );
    }

    if uninstalled.is_empty() {
        println!("Removed marketplace source: {} ({identity})", source.name);
    } else {
        println!(
            "Removed marketplace source and uninstalled {} plugin(s): {}",
            uninstalled.len(),
            uninstalled.join(", "),
        );
    }
    Ok(())
}

fn marketplace_update(
    sources: &[distill_plugin_marketplace::MarketplaceSource],
    name: Option<&str>,
) -> Result<()> {
    marketplace_update_with_cache_root(
        sources,
        name,
        &distill_plugin_marketplace::git::default_cache_root(),
    )
}

fn marketplace_update_with_cache_root(
    sources: &[distill_plugin_marketplace::MarketplaceSource],
    name: Option<&str>,
    cache_root: &Path,
) -> Result<()> {
    let mut refreshed = 0;
    let mut errors = Vec::new();
    let mut name_matched = false;

    for source in sources {
        if let Some(filter) = name {
            if source.name != filter {
                continue;
            }
            name_matched = true;
        }
        if let SourceKind::Git { url, branch } = &source.kind {
            match distill_plugin_marketplace::git::force_sync_source_cache(
                url,
                branch.as_deref(),
                cache_root,
            ) {
                Ok(_) => {
                    println!("  {}: synced", source.name);
                    refreshed += 1;
                }
                Err(e) => errors.push(format!("{}: {e}", source.name)),
            }
        }
    }

    if refreshed == 0 && errors.is_empty() {
        if let Some(filter) = name {
            if name_matched {
                println!("Source \"{filter}\" is local, nothing to sync.");
            } else {
                bail!("Marketplace source \"{filter}\" not found.");
            }
        } else {
            println!("No marketplace sources configured.");
        }
    } else if errors.is_empty() {
        println!("Refreshed {refreshed} source(s).");
    } else {
        eprintln!(
            "Refreshed {refreshed} source(s) with {} error(s): {}",
            errors.len(),
            errors.join("; "),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_plugin_marketplace::MarketplaceSource;

    /// `grok plugin update` must exit nonzero when any update failed (e.g.
    /// every update policy-blocked), so scripts can detect the block.
    #[test]
    fn update_failure_summary_reports_failed_outcomes() {
        let outcomes = vec![
            RepoUpdateOutcome::AlreadyUpToDate {
                repo_key: "ok-repo".into(),
            },
            RepoUpdateOutcome::Failed {
                repo_key: "blocked-repo".into(),
                error: "Plugin update blocked: source not in strictKnownMarketplaces".into(),
            },
        ];
        assert_eq!(
            update_failure_summary(&outcomes).as_deref(),
            Some("1 of 2 plugin update(s) failed")
        );
        let (first, _) = outcomes.split_at_checked(1).expect("two outcomes");
        assert_eq!(update_failure_summary(first), None);
    }

    fn removal_fixture() -> Vec<MarketplaceSource> {
        vec![
            MarketplaceSource {
                name: "jira".into(),
                kind: SourceKind::Git {
                    url: "https://nova.example.com:4466/mcp/jira".into(),
                    branch: None,
                },
            },
            MarketplaceSource {
                name: "official".into(),
                kind: SourceKind::Git {
                    url: "https://github.com/example/plugin-marketplace.git".into(),
                    branch: None,
                },
            },
            MarketplaceSource {
                name: "local".into(),
                kind: SourceKind::Local {
                    path: "/tmp/my-marketplace".into(),
                },
            },
        ]
    }

    #[test]
    fn find_removal_source_matches_by_name() {
        let sources = removal_fixture();
        let found = find_removal_source(&sources, "jira", Path::new("/")).unwrap();
        assert_eq!(found.name, "jira");
    }

    #[test]
    fn find_removal_source_matches_by_url_ignoring_git_suffix() {
        let sources = removal_fixture();
        let found = find_removal_source(
            &sources,
            "https://github.com/example/plugin-marketplace",
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(found.name, "official");
    }

    #[test]
    fn find_removal_source_matches_local_path() {
        let sources = removal_fixture();
        let found = find_removal_source(&sources, "/tmp/my-marketplace", Path::new("/")).unwrap();
        assert_eq!(found.name, "local");
    }

    #[test]
    fn find_removal_source_not_found_lists_names() {
        let sources = removal_fixture();
        let err = find_removal_source(&sources, "nope", Path::new("/")).unwrap_err();
        assert!(err.contains("\"nope\" not found"), "{err}");
        assert!(err.contains("jira, official, local"), "{err}");
    }

    #[test]
    fn find_removal_source_duplicate_names_require_url() {
        let mut sources = removal_fixture();
        sources.push(MarketplaceSource {
            name: "jira".into(),
            kind: SourceKind::Git {
                url: "https://other.example.com/jira.git".into(),
                branch: None,
            },
        });
        let err = find_removal_source(&sources, "jira", Path::new("/")).unwrap_err();
        assert!(err.contains("Multiple sources are named \"jira\""), "{err}");
        assert!(
            err.contains("https://nova.example.com:4466/mcp/jira"),
            "{err}"
        );
        assert!(err.contains("https://other.example.com/jira.git"), "{err}");
    }

    #[test]
    fn trust_prompt_marketplace_has_no_error_framing() {
        let msg = trust_prompt(
            "\"sentry\" from marketplace \"Grok Official\"",
            "sentry@xai-org/plugin-marketplace",
        );
        assert!(
            msg.starts_with(
                "Installing \"sentry\" from marketplace \"Grok Official\" requires confirmation."
            ),
            "{msg}"
        );
        assert!(msg.contains("hooks, MCP servers, and skills"));
        assert!(msg.contains(
            "To proceed, re-run with --trust:\n  grok plugin install sentry@xai-org/plugin-marketplace --trust"
        ));
        assert!(!msg.contains("Error"));
        assert!(!msg.contains("Failed"));
        assert!(!msg.contains("Plugin source:"));
    }

    #[test]
    fn trust_prompt_git_and_local_subjects() {
        let git = trust_prompt("from git repo https://github.com/u/r", "u/r");
        assert!(
            git.starts_with(
                "Installing from git repo https://github.com/u/r requires confirmation."
            ),
            "{git}"
        );
        assert!(git.ends_with("  grok plugin install u/r --trust"), "{git}");
        let local = trust_prompt("from directory /tmp/p", "./p");
        assert!(
            local.starts_with("Installing from directory /tmp/p requires confirmation."),
            "{local}"
        );
    }

    #[test]
    fn marketplace_update_force_syncs_fresh_git_cache() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy().to_string();
        let source = MarketplaceSource {
            name: "test-marketplace".into(),
            kind: SourceKind::Git {
                url: url.clone(),
                branch: Some("main".into()),
            },
        };

        let cache_dir = distill_plugin_marketplace::git::sync_source_cache(
            &url,
            Some("main"),
            cache_root.path(),
        )
        .unwrap();
        let first_head = current_head(&cache_dir);
        add_commit(remote.path(), "second.txt", "second");

        marketplace_update_with_cache_root(&[source], None, cache_root.path()).unwrap();
        assert_ne!(current_head(&cache_dir), first_head);
    }

    fn init_remote_repo(path: &Path) {
        run_git(path, &["init", "--initial-branch", "main"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        add_commit(path, "file.txt", "initial");
    }

    fn add_commit(repo: &Path, file: &str, contents: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        run_git(repo, &["add", file]);
        run_git(repo, &["commit", "-m", file]);
    }

    fn current_head(repo: &Path) -> String {
        let output = distill_plugin_marketplace::git::git_command()
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn git_available() -> bool {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        std::process::Command::new(git_bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        let output = std::process::Command::new(git_bin)
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
