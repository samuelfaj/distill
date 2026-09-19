// Modified for Distill by Samuel Fajreldines, 2026.
//! Marketplace browsing and install endpoints for the pager modal.
//! Scanning and install logic live in the `distill-plugin-marketplace` crate.

use agent_client_protocol as acp;
use distill_hooks_plugins_types::{
    MarketplaceAction, MarketplaceActionRequest, MarketplaceListResponse, MarketplacePluginEntry,
    MarketplaceScanResult,
};

use crate::agent::MvpAgent;
use crate::plugin::add_marketplace_source;
use crate::util::config::acquire_init_lock;

type ExtResult = Result<acp::ExtResponse, acp::Error>;

fn load_filtered_marketplace_sources() -> Vec<distill_plugin_marketplace::MarketplaceSource> {
    crate::plugin::load_filtered_marketplace_sources()
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/marketplace/list" => handle_list().await,
        "x.ai/marketplace/action" => handle_action(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_list() -> ExtResult {
    let t0 = std::time::Instant::now();
    let sources = load_filtered_marketplace_sources();
    let source_names: Vec<String> = sources
        .iter()
        .map(|s| {
            let url = match &s.kind {
                distill_plugin_marketplace::SourceKind::Git { url, .. } => url.as_str(),
                distill_plugin_marketplace::SourceKind::Local { path } => {
                    path.to_str().unwrap_or("?")
                }
            };
            format!("{}={}", s.name, url)
        })
        .collect();
    distill_telemetry::unified_log::info(
        "marketplace handle_list: sources loaded",
        None,
        Some(serde_json::json!({
            "source_count": sources.len(),
            "sources": source_names,
            "load_sources_ms": t0.elapsed().as_millis() as u64,
        })),
    );

    // Scan all sources in parallel using blocking tasks (git operations are sync).
    let scan_handles: Vec<_> = sources
        .iter()
        .map(|source| {
            let source = source.clone();
            tokio::task::spawn_blocking(move || scan_source(&source))
        })
        .collect();
    let mut results = Vec::with_capacity(scan_handles.len());
    for (i, (source, handle)) in sources.iter().zip(scan_handles).enumerate() {
        let (scan, catalog_loaded) = handle.await.unwrap_or_else(|e| {
            (
                MarketplaceScanResult {
                    source_name: source.name.clone(),
                    source_kind: String::new(),
                    source_url_or_path: String::new(),
                    plugins: Vec::new(),
                    error: Some(format!("scan task failed: {e}")),
                },
                false,
            )
        });
        let components_present = scan
            .plugins
            .iter()
            .filter(|p| p.components.is_some())
            .count();
        distill_telemetry::unified_log::info(
            "marketplace handle_list: source scanned",
            None,
            Some(serde_json::json!({
                "source_index": i,
                "source_name": source.name,
                "scan_ms": 0, // per-source timing is unavailable when scans run in parallel
                "plugin_count": scan.plugins.len(),
                "catalog_loaded": catalog_loaded,
                "components_present": components_present,
                "components_absent": scan.plugins.len() - components_present,
                "error": scan.error,
            })),
        );
        results.push(scan);
    }

    distill_telemetry::unified_log::info(
        "marketplace handle_list: complete",
        None,
        Some(serde_json::json!({
            "total_ms": t0.elapsed().as_millis() as u64,
        })),
    );

    let response = MarketplaceListResponse { sources: results };
    super::to_ext_response(Ok(response))
}

async fn handle_action(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: MarketplaceActionRequest = super::parse_params(args)?;
    let sid = acp::SessionId::new(req.session_id);

    let outcome = match req.action {
        MarketplaceAction::Refresh { source_url_or_path } => {
            // Force re-sync git caches (local sources are re-scanned on next
            // list) on the blocking pool (LocalSet invariant: plugin/acquire.rs).
            let sources = load_filtered_marketplace_sources();
            let filter = source_url_or_path;
            match tokio::task::spawn_blocking(move || refresh_sources(&sources, filter.as_deref()))
                .await
            {
                Ok(outcome) => outcome,
                Err(e) => distill_hooks_plugins_types::ActionOutcome {
                    status: distill_hooks_plugins_types::OutcomeStatus::InternalError,
                    message: format!("Refresh task failed: {e}"),
                    requires_reload: false,
                    requires_restart: false,
                },
            }
        }
        MarketplaceAction::Install {
            source_url_or_path,
            plugin_relative_path,
        } => handle_install(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::Update {
            source_url_or_path,
            plugin_relative_path,
        } => handle_update(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::Uninstall {
            source_url_or_path,
            plugin_relative_path,
        } => handle_uninstall(agent, &sid, &source_url_or_path, &plugin_relative_path).await,
        MarketplaceAction::AddSource { url } => handle_add_source(&url).await,
        MarketplaceAction::RemoveSource { source_url_or_path } => {
            handle_remove_source(&source_url_or_path).await
        }
    };

    super::to_ext_response(Ok(outcome))
}

fn refresh_sources(
    sources: &[distill_plugin_marketplace::MarketplaceSource],
    source_url_or_path: Option<&str>,
) -> distill_hooks_plugins_types::ActionOutcome {
    let mut refreshed = 0;
    let mut errors = Vec::new();
    for source in sources {
        if let Some(filter) = source_url_or_path
            && source.identity() != filter
        {
            continue;
        }
        if let distill_plugin_marketplace::SourceKind::Git { url, branch } = &source.kind {
            let cache_root = distill_plugin_marketplace::git::default_cache_root();
            if let Err(e) = distill_plugin_marketplace::git::force_sync_source_cache(
                url,
                branch.as_deref(),
                &cache_root,
            ) {
                errors.push(format!("{}: {e}", source.name));
            }
        }
        refreshed += 1;
    }

    let msg = if errors.is_empty() {
        format!("Refreshed {refreshed} source(s).")
    } else {
        format!(
            "Refreshed {refreshed} source(s) with {} error(s): {}",
            errors.len(),
            errors.join("; ")
        )
    };
    distill_hooks_plugins_types::ActionOutcome {
        status: distill_hooks_plugins_types::OutcomeStatus::Success,
        message: msg,
        requires_reload: false,
        requires_restart: false,
    }
}

/// Auto-enable a freshly installed/updated repo's plugins, logging warnings; `post_install_plugin`
/// polls the config-init flock, so blocking pool only, never the LocalSet.
async fn run_post_install(repo_key: &str) {
    let repo_key = repo_key.to_string();
    let post_warnings =
        tokio::task::spawn_blocking(move || crate::config::post_install_plugin(&repo_key).1)
            .await
            .unwrap_or_else(|e| vec![format!("post-install task failed: {e}")]);
    for w in &post_warnings {
        tracing::warn!("{w}");
    }
}

async fn handle_update(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin::acquire;
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    // Session-start auto-update fires this once per outdated plugin; acquire::run_marketplace_update
    // owns the blocking-pool hop (LocalSet invariant: plugin/acquire.rs).
    let updated = acquire::run_marketplace_update(
        distill_agent::plugins::install_registry::MarketplaceProvenance {
            source_url_or_path: source_url_or_path.to_string(),
            // Refreshed from the resolved source (the wire request carries no
            // display name).
            source_display_name: String::new(),
            plugin_subdir: plugin_relative_path.to_string(),
        },
        true,
    )
    .await;

    match updated {
        Ok(result) => {
            run_post_install(&result.repo_key).await;
            let reload_outcome = agent
                .execute_plugins_action(sid, distill_hooks_plugins_types::PluginsAction::Reload)
                .await;
            let mut msg = format!(
                "Updated {} ({} -> {})",
                result.repo_key,
                result.old_version.as_deref().unwrap_or("?"),
                result.new_version.as_deref().unwrap_or("?")
            );
            if reload_outcome.is_none() {
                msg.push_str("\nRestart or run /plugins reload to activate the update.");
            }
            ActionOutcome {
                status: OutcomeStatus::Success,
                message: msg,
                requires_reload: false,
                requires_restart: false,
            }
        }
        Err(e) => update_error_outcome(e),
    }
}

/// Map an Update acquisition failure to its user-facing [`ActionOutcome`].
fn update_error_outcome(
    e: crate::plugin::acquire::RunError<crate::plugin::acquire::UpdateAcquireError>,
) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin::acquire::{RunError, UpdateAcquireError};
    use distill_agent::plugins::install_registry::InstallError;
    use distill_hooks_plugins_types::OutcomeStatus;

    match e {
        RunError::Op(e) => match e {
            UpdateAcquireError::Blocked { reason } => {
                action_outcome(OutcomeStatus::ValidationError, reason)
            }
            UpdateAcquireError::NotConfigured { message } => {
                action_outcome(OutcomeStatus::NotFound, message)
            }
            UpdateAcquireError::EntryNotFound {
                plugin_relative_path,
            } => action_outcome(
                OutcomeStatus::NotFound,
                format!("Marketplace plugin not found: {plugin_relative_path}"),
            ),
            UpdateAcquireError::InvalidPluginPath { detail } => action_outcome(
                OutcomeStatus::ValidationError,
                format!("Invalid plugin path: {detail}"),
            ),
            UpdateAcquireError::Sync { detail } => action_outcome(
                OutcomeStatus::InternalError,
                format!("Git sync failed: {detail}"),
            ),
            UpdateAcquireError::Install(InstallError::PluginNotFound { name }) => action_outcome(
                OutcomeStatus::NotFound,
                format!("Plugin not installed from this marketplace source: {name}"),
            ),
            UpdateAcquireError::Install(e) => {
                action_outcome(OutcomeStatus::InternalError, format!("Update failed: {e}"))
            }
        },
        RunError::RegistryLock { detail } => action_outcome(
            OutcomeStatus::InternalError,
            format!("Another plugin operation is in progress: {detail}"),
        ),
        RunError::TaskJoin(e) => action_outcome(
            OutcomeStatus::InternalError,
            format!("Update task failed: {e}"),
        ),
    }
}

async fn handle_install(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin::acquire;
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    // acquire::run_marketplace_install owns the blocking-pool hop (LocalSet
    // invariant: plugin/acquire.rs).
    let installed = acquire::run_marketplace_install(
        source_url_or_path.to_string(),
        plugin_relative_path.to_string(),
    )
    .await;

    match installed {
        Ok(acquire::Installed {
            repo_key,
            source_name,
            plugin_relative_path,
        }) => {
            // Auto-enable installed plugin so it's active after reload.
            run_post_install(&repo_key).await;
            let reload_outcome = agent
                .execute_plugins_action(sid, distill_hooks_plugins_types::PluginsAction::Reload)
                .await;
            let mut msg =
                format!("Installed from {source_name}: {plugin_relative_path} (key: {repo_key})");
            if reload_outcome.is_none() {
                msg.push_str("\nRestart or run /plugins reload to activate the plugin.");
            }
            ActionOutcome {
                status: OutcomeStatus::Success,
                message: msg,
                requires_reload: false,
                requires_restart: false,
            }
        }
        Err(e) => install_error_outcome(e),
    }
}

/// Map an Install acquisition failure to its user-facing [`ActionOutcome`].
fn install_error_outcome(
    e: crate::plugin::acquire::RunError<crate::plugin::acquire::InstallAcquireError>,
) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin::acquire::{EntryInstallError, InstallAcquireError, RunError};
    use distill_hooks_plugins_types::OutcomeStatus;

    match e {
        RunError::Op(e) => match e {
            InstallAcquireError::Blocked { reason } => {
                action_outcome(OutcomeStatus::ValidationError, reason)
            }
            InstallAcquireError::SourceNotFound { source } => action_outcome(
                OutcomeStatus::NotFound,
                format!("Marketplace source not found: {source}"),
            ),
            InstallAcquireError::AlreadyInstalled { repo_key } => action_outcome(
                OutcomeStatus::ValidationError,
                format!("Already installed (key: {repo_key}). Use Update to reinstall."),
            ),
            InstallAcquireError::Sync { detail } => action_outcome(
                OutcomeStatus::InternalError,
                format!("Git sync failed: {detail}"),
            ),
            InstallAcquireError::Entry(EntryInstallError::InvalidPluginPath { detail }) => {
                action_outcome(
                    OutcomeStatus::ValidationError,
                    format!("Invalid plugin path: {detail}"),
                )
            }
            InstallAcquireError::Entry(EntryInstallError::PluginDirNotFound { dir }) => {
                action_outcome(
                    OutcomeStatus::NotFound,
                    format!("Plugin directory not found: {}", dir.display()),
                )
            }
            InstallAcquireError::Entry(EntryInstallError::Install(e)) => {
                action_outcome(OutcomeStatus::InternalError, format!("Install failed: {e}"))
            }
        },
        RunError::RegistryLock { detail } => action_outcome(
            OutcomeStatus::InternalError,
            format!("Another plugin operation is in progress: {detail}"),
        ),
        RunError::TaskJoin(e) => action_outcome(
            OutcomeStatus::InternalError,
            format!("Install task failed: {e}"),
        ),
    }
}

fn action_outcome(
    status: distill_hooks_plugins_types::OutcomeStatus,
    message: String,
) -> distill_hooks_plugins_types::ActionOutcome {
    distill_hooks_plugins_types::ActionOutcome {
        status,
        message,
        requires_reload: false,
        requires_restart: false,
    }
}

async fn handle_uninstall(
    agent: &MvpAgent,
    sid: &acp::SessionId,
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> distill_hooks_plugins_types::ActionOutcome {
    use distill_hooks_plugins_types::OutcomeStatus;

    // Registry + fs work on the blocking pool under the registry flock
    // (never block the LocalSet on fs or the flock poll).
    let source = source_url_or_path.to_string();
    let path = plugin_relative_path.to_string();
    let outcome = match tokio::task::spawn_blocking(move || uninstall_locked(&source, &path)).await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            return action_outcome(
                OutcomeStatus::InternalError,
                format!("Uninstall task failed: {e}"),
            );
        }
    };
    if outcome.status != OutcomeStatus::Success {
        return outcome;
    }

    // Trigger plugin reload so the removed plugin disappears from the session.
    let _ = agent
        .execute_plugins_action(sid, distill_hooks_plugins_types::PluginsAction::Reload)
        .await;

    outcome
}

/// Blocking core of [`handle_uninstall`], whole window under the registry
/// flock; save failures surface (an unpersisted deregister is not success).
fn uninstall_locked(
    source_url_or_path: &str,
    plugin_relative_path: &str,
) -> distill_hooks_plugins_types::ActionOutcome {
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};
    use distill_plugin_marketplace::installer;

    let _registry_lock = match crate::plugin::acquire::lock_install_registry() {
        Ok(lock) => lock,
        Err(detail) => {
            return action_outcome(
                OutcomeStatus::InternalError,
                format!("Another plugin operation is in progress: {detail}"),
            );
        }
    };
    let mut registry = distill_agent::plugins::install_registry::InstallRegistry::load();

    // Find the installed entry by marketplace provenance.
    let found = installer::find_installed_marketplace_plugin(
        &registry,
        source_url_or_path,
        plugin_relative_path,
    );

    let repo_key = match found {
        Some((key, _version)) => key,
        None => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Plugin not installed from marketplace: {plugin_relative_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    // Delete the installed plugin directory.
    let plugin_dir = registry.install_dir().join(&repo_key);
    if plugin_dir.is_dir()
        && let Err(e) = std::fs::remove_dir_all(&plugin_dir)
    {
        return ActionOutcome {
            status: OutcomeStatus::InternalError,
            message: format!("Failed to remove plugin directory: {e}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // Remove from registry and save.
    registry.remove(&repo_key);
    if let Err(e) = registry.save() {
        return ActionOutcome {
            status: OutcomeStatus::InternalError,
            message: format!("Failed to save registry: {e}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    ActionOutcome {
        status: OutcomeStatus::Success,
        message: format!("Uninstalled {plugin_relative_path} (key: {repo_key})"),
        requires_reload: false,
        requires_restart: false,
    }
}

fn scan_source(
    source: &distill_plugin_marketplace::MarketplaceSource,
) -> (MarketplaceScanResult, bool) {
    let lease;
    let (source_kind, source_url_or_path, root) = match &source.kind {
        distill_plugin_marketplace::SourceKind::Local { path } => {
            lease = None;
            (
                "local".to_string(),
                path.display().to_string(),
                Some(path.clone()),
            )
        }
        distill_plugin_marketplace::SourceKind::Git { url, branch } => {
            let cache_root = distill_plugin_marketplace::git::default_cache_root();
            let t_git = std::time::Instant::now();
            match distill_plugin_marketplace::git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                &cache_root,
                distill_plugin_marketplace::git::SyncMode::UseTtl,
            ) {
                Ok(cache_lease) => {
                    let cached_path = cache_lease.path.clone();
                    lease = Some(cache_lease);
                    distill_telemetry::unified_log::info(
                        "scan_source: git sync done",
                        None,
                        Some(serde_json::json!({
                            "url": url,
                            "git_sync_ms": t_git.elapsed().as_millis() as u64,
                        })),
                    );
                    ("git".to_string(), url.clone(), Some(cached_path))
                }
                Err(e) => {
                    return (
                        MarketplaceScanResult {
                            source_name: source.name.clone(),
                            source_kind: "git".to_string(),
                            source_url_or_path: url.clone(),
                            plugins: Vec::new(),
                            error: Some(format!("Git sync failed: {e}")),
                        },
                        false,
                    );
                }
            }
        }
    };

    let root = match root {
        Some(r) if r.is_dir() => r,
        _ => {
            return (
                MarketplaceScanResult {
                    source_name: source.name.clone(),
                    source_kind,
                    source_url_or_path: source_url_or_path.clone(),
                    plugins: Vec::new(),
                    error: Some(format!("Directory not found: {source_url_or_path}")),
                },
                false,
            );
        }
    };

    let scan = distill_plugin_marketplace::scan_marketplace(&root);
    let catalog_loaded = scan.catalog_loaded;
    let discovered = scan.entries;
    drop(lease);

    // Cross-reference with install registry.
    let registry = distill_agent::plugins::install_registry::InstallRegistry::load();
    let plugins = discovered
        .into_iter()
        .map(|p| {
            let (install_status, installed_version) =
                match distill_plugin_marketplace::installer::find_installed_marketplace_plugin(
                    &registry,
                    &source_url_or_path,
                    &p.relative_path,
                ) {
                    Some((_key, ver)) => {
                        if p.version.is_none()
                            || p.version.as_deref() == Some(ver.as_str())
                            || ver.is_empty()
                        {
                            ("installed".to_string(), Some(ver))
                        } else {
                            ("update_available".to_string(), Some(ver))
                        }
                    }
                    None => ("not_installed".to_string(), None),
                };

            to_plugin_entry(p, install_status, installed_version)
        })
        .collect();

    (
        MarketplaceScanResult {
            source_name: source.name.clone(),
            source_kind,
            source_url_or_path,
            plugins,
            error: None,
        },
        catalog_loaded,
    )
}

fn to_plugin_entry(
    p: distill_plugin_marketplace::MarketplaceEntry,
    install_status: String,
    installed_version: Option<String>,
) -> MarketplacePluginEntry {
    MarketplacePluginEntry {
        name: p.name,
        version: p.version,
        description: p.description,
        category: p.category,
        author: p.author,
        tags: p.tags,
        keywords: p.keywords,
        domains: p.domains,
        homepage: p.homepage,
        relative_path: p.relative_path,
        skill_count: p.skill_count,
        has_hooks: p.has_hooks,
        has_agents: p.has_agents,
        has_mcp: p.has_mcp,
        install_status,
        installed_version,
        components: p.components,
        remote_url: p.remote_url,
        remote_ref: p.remote_ref,
        remote_sha: p.remote_sha,
        remote_subdir: p.remote_subdir,
    }
}

/// Add a new git or local-path marketplace source to `~/.grok/config.toml`.
async fn handle_add_source(url: &str) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin::{self, MarketplaceAddInput};
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let url = url.trim();
    if url.is_empty() {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: "URL cannot be empty.".into(),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let input = plugin::classify_marketplace_add_input(url, &cwd);

    // Fail fast on a missing local path: stored as a git URL, it would only error after network clone attempts
    if let MarketplaceAddInput::LocalPath(path) = &input
        && !path.is_dir()
    {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!(
                "Local marketplace path not found (or is not a directory): {}",
                path.display()
            ),
            requires_reload: false,
            requires_restart: false,
        };
    }

    let identity = match &input {
        MarketplaceAddInput::GitUrl(u) => u.clone(),
        MarketplaceAddInput::LocalPath(p) => p.display().to_string(),
    };

    let allowlist =
        &distill_workspace::permission::resolution::managed_settings().marketplace_allowlist;
    if let Some(reason) = allowlist.add_block_reason(&identity) {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!("Marketplace source blocked: {reason}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // Dedupe against the FULL unfiltered source list (config + settings
    // extras + managed pins) by canonical git-URL identity.
    let existing = crate::plugin::load_marketplace_sources();
    let already_configured = match &input {
        MarketplaceAddInput::GitUrl(git_url) => {
            use distill_workspace::permission::resolution::normalize_git_url;
            let normalized = normalize_git_url(git_url);
            existing.iter().any(|s| {
                matches!(&s.kind, distill_plugin_marketplace::SourceKind::Git { url: u, .. }
                    if normalize_git_url(u) == normalized)
            })
        }
        MarketplaceAddInput::LocalPath(path) => existing.iter().any(|s| {
            matches!(&s.kind, distill_plugin_marketplace::SourceKind::Local { path: p }
                if p == path)
        }),
    };
    if already_configured {
        return ActionOutcome {
            status: OutcomeStatus::ValidationError,
            message: format!("Marketplace source already configured: {identity}"),
            requires_reload: false,
            requires_restart: false,
        };
    }

    // Reject URLs that aren't reachable git repos (e.g. MCP endpoints pasted into the wrong tab) before persisting.
    // The probe blocks on a git subprocess, so run it off the LocalSet
    if let MarketplaceAddInput::GitUrl(git_url) = &input {
        let probe_url = git_url.clone();
        let probe = tokio::task::spawn_blocking(move || {
            distill_plugin_marketplace::git::probe_git_remote(&probe_url)
        })
        .await;
        match probe {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return ActionOutcome {
                    status: OutcomeStatus::ValidationError,
                    message: format!(
                        "{e}. Not a reachable git repository — to add it anyway (e.g. a \
                         VPN-gated host), run: grok plugin marketplace add {url} --force"
                    ),
                    requires_reload: false,
                    requires_restart: false,
                };
            }
            Err(e) => {
                return ActionOutcome {
                    status: OutcomeStatus::InternalError,
                    message: format!("Probe task failed: {e}"),
                    requires_reload: false,
                    requires_restart: false,
                };
            }
        }
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

    // Run the write under the config write guard (SAVE_LOCK + init flock), off the reactor; an
    // unguarded add is exactly the read-modify-write race the guard prevents.
    let config_path = distill_config::distill_home().join("config.toml");
    let save_guard = match crate::util::config::lock_config_writes().await {
        Ok(guard) => guard,
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Another config write is in progress: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };
    let write = {
        let name = name.clone();
        save_guard
            .run_blocking(move || add_marketplace_source(&config_path, &name, &input, is_official))
            .await
    };
    match write {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Failed to write config: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Config write task failed: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    }

    ActionOutcome {
        status: OutcomeStatus::Success,
        message: format!("Added marketplace source: {name} ({identity})"),
        requires_reload: true,
        requires_restart: false,
    }
}

/// Remove a marketplace source from `~/.grok/config.toml` and uninstall all
/// plugins that were installed from it.
async fn handle_remove_source(
    source_url_or_path: &str,
) -> distill_hooks_plugins_types::ActionOutcome {
    let src = source_url_or_path.to_string();
    // Guard (SAVE_LOCK + init flock) held across the whole blocking read-modify-write so a
    // concurrent auto-register can't re-add the source mid-removal.
    let save_guard = match crate::util::config::lock_config_writes().await {
        Ok(guard) => guard,
        Err(e) => {
            return distill_hooks_plugins_types::ActionOutcome {
                status: distill_hooks_plugins_types::OutcomeStatus::InternalError,
                message: format!("Another config write is in progress: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };
    match save_guard
        .run_blocking(move || remove_source_locked(&src))
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => distill_hooks_plugins_types::ActionOutcome {
            status: distill_hooks_plugins_types::OutcomeStatus::InternalError,
            message: format!("Config write task failed: {e}"),
            requires_reload: false,
            requires_restart: false,
        },
    }
}

/// Sync body of [`handle_remove_source`]; the caller holds the config write
/// guard across this whole read-modify-write.
fn remove_source_locked(source_url_or_path: &str) -> distill_hooks_plugins_types::ActionOutcome {
    use crate::plugin;
    use distill_hooks_plugins_types::{ActionOutcome, OutcomeStatus};

    let distill_home = distill_config::distill_home();

    // Fail closed before touching config: removing the source while its
    // installs can't be deregistered would orphan them.
    let uninstalled = match plugin::uninstall_marketplace_source_plugins(source_url_or_path) {
        Ok(keys) => keys,
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: e.to_string(),
                requires_reload: false,
                requires_restart: false,
            };
        }
    };

    let config_path = distill_home.join("config.toml");
    match plugin::remove_marketplace_source_from_stores(&config_path, source_url_or_path) {
        Ok(plugin::MarketplaceSourceRemoval::NotFound) => {
            return ActionOutcome {
                status: OutcomeStatus::NotFound,
                message: format!("Source not found in config: {source_url_or_path}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
        Ok(_) => {}
        Err(e) => {
            return ActionOutcome {
                status: OutcomeStatus::InternalError,
                message: format!("Failed to update config: {e}"),
                requires_reload: false,
                requires_restart: false,
            };
        }
    }

    let msg = if uninstalled.is_empty() {
        format!("Removed marketplace source: {source_url_or_path}")
    } else {
        format!(
            "Removed marketplace source and uninstalled {} plugin(s): {}",
            uninstalled.len(),
            uninstalled.join(", ")
        )
    };
    ActionOutcome {
        status: OutcomeStatus::Success,
        message: msg,
        requires_reload: true,
        requires_restart: false,
    }
}

use crate::plugin::{
    OFFICIAL_MARKETPLACE_FLAG, set_marketplace_bool_flag, set_official_marketplace_auto_installed,
};

fn read_marketplace_bool_flag(config_path: &std::path::Path, key: &str) -> bool {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed
        .get("marketplace")
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn read_official_marketplace_auto_installed(config_path: &std::path::Path) -> bool {
    read_marketplace_bool_flag(config_path, OFFICIAL_MARKETPLACE_FLAG)
}

fn is_default_skills_plugin_subdir(plugin_subdir: &str) -> bool {
    plugin_subdir == "default-skills"
}

fn default_skills_repo_keys<'a>(
    repos: impl IntoIterator<
        Item = (
            &'a str,
            &'a distill_agent::plugins::install_registry::InstalledRepo,
        ),
    >,
) -> Vec<&'a str> {
    repos
        .into_iter()
        .filter_map(|(key, repo)| {
            repo.marketplace
                .as_ref()
                .filter(|mp| is_default_skills_plugin_subdir(&mp.plugin_subdir))
                .map(|_| key)
        })
        .collect()
}

fn set_default_skills_installs_purged(config_path: &std::path::Path) -> std::io::Result<()> {
    set_marketplace_bool_flag(config_path, "default_skills_installs_purged")
}

fn read_default_skills_installs_purged(config_path: &std::path::Path) -> bool {
    read_marketplace_bool_flag(config_path, "default_skills_installs_purged")
}

/// One-shot purge of legacy marketplace `default-skills` installs.
/// Gated by the sticky `default_skills_installs_purged` flag in config.toml.
/// Best-effort: errors are logged and never block startup.
pub(crate) fn purge_default_skills_installs(distill_home: &std::path::Path) {
    let install_dir =
        distill_agent::plugins::install_registry::InstallRegistry::resolve_install_dir();
    purge_default_skills_installs_impl(distill_home, &install_dir, || {
        distill_agent::plugins::install_registry::InstallRegistry::try_load_from(
            install_dir.clone(),
        )
    });
}

/// Short registry-lock wait for the startup purge: contention means another
/// plugin operation is live, and the unset sticky flag retries next startup.
const PURGE_REGISTRY_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn purge_default_skills_installs_impl(
    distill_home: &std::path::Path,
    install_dir: &std::path::Path,
    load_registry: impl FnOnce() -> Result<
        distill_agent::plugins::install_registry::InstallRegistry,
        distill_agent::plugins::install_registry::InstallError,
    >,
) {
    let config_path = distill_home.join("config.toml");

    if read_default_skills_installs_purged(&config_path) {
        return;
    }

    let _lock = match acquire_init_lock(distill_home) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %distill_home.join(".config-init.lock").display(),
                "skipping default-skills purge: failed to acquire init lock"
            );
            return;
        }
    };

    if read_default_skills_installs_purged(&config_path) {
        return;
    }

    // Registry flock across load→mutate→save like every other writer
    // (init ⊃ registry lock order); a timeout skips and retries next startup.
    let _registry_lock = match crate::plugin::acquire::lock_install_registry_in(
        install_dir,
        PURGE_REGISTRY_LOCK_TIMEOUT,
    ) {
        Ok(lock) => lock,
        Err(detail) => {
            tracing::warn!(
                %detail,
                "skipping default-skills purge: failed to acquire registry lock"
            );
            return;
        }
    };

    let mut registry = match load_registry() {
        Ok(reg) => reg,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "skipping default-skills purge: failed to load install registry"
            );
            return;
        }
    };
    let keys: Vec<String> = default_skills_repo_keys(registry.list())
        .into_iter()
        .map(|k| k.to_string())
        .collect();

    for key in &keys {
        let path = registry
            .get_repo(key)
            .map(|r| r.path.clone())
            .unwrap_or_else(|| registry.install_dir().join(key));
        if path.exists()
            && let Err(e) = std::fs::remove_dir_all(&path)
        {
            let _ = std::fs::remove_file(&path);
            if path.exists() {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    repo_key = %key,
                    "failed to remove default-skills install dir"
                );
            }
        }
        registry.remove(key);
    }

    if !keys.is_empty() {
        if let Err(e) = registry.save() {
            tracing::warn!(error = %e, "failed to save registry after default-skills purge");
            return;
        }
        tracing::info!(
            count = keys.len(),
            "purged legacy default-skills marketplace installs"
        );
    }

    if let Err(e) = set_default_skills_installs_purged(&config_path) {
        tracing::warn!(
            error = %e,
            path = %config_path.display(),
            "failed to set default_skills_installs_purged flag"
        );
    }
}

/// Auto-register the official Grok marketplace source on first run. Gated by the caller (`init_process`); see `Config::resolve_official_marketplace_auto_register`. No-op once `official_marketplace_auto_installed` is set.
/// Under a process-wide flock it adds the source (or just sets the flag if it's already present in config.toml or a JSON store). Best-effort: errors are logged and never block startup.
pub(crate) fn ensure_official_marketplace_source(distill_home: &std::path::Path) {
    ensure_official_marketplace_source_with(
        distill_home,
        &distill_workspace::permission::resolution::managed_settings().marketplace_allowlist,
    );
}

/// [`ensure_official_marketplace_source`] with the marketplace policy injected — the OnceLock
/// seam, so tests can pin the blocked-skip behavior.
fn ensure_official_marketplace_source_with(
    distill_home: &std::path::Path,
    policy: &distill_workspace::permission::resolution::MarketplacePolicy,
) {
    let config_path = distill_home.join("config.toml");

    if read_official_marketplace_auto_installed(&config_path) {
        return;
    }

    // Auto-register is a `marketplace add` on the user's behalf: it fails closed against every
    // strict list; the flag stays unset so the register retries if the policy lifts.
    if policy
        .add_block_reason(distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL)
        .is_some()
    {
        // Log the full-path reason (the add gate's refusal reduces the
        // policy file to its name for users).
        tracing::info!(
            reason = %policy.block_reason(
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
                distill_workspace::permission::resolution::PolicySubjectOrigin::Foreign,
            ),
            "skipping official marketplace auto-register: blocked by marketplace policy"
        );
        return;
    }

    let _lock = match acquire_init_lock(distill_home) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %distill_home.join(".config-init.lock").display(),
                "skipping official marketplace auto-register: failed to acquire init lock"
            );
            return;
        }
    };

    // Re-check under the lock: another process may have registered meanwhile.
    if read_official_marketplace_auto_installed(&config_path) {
        return;
    }

    let raw = match crate::util::config::read_to_string_or_empty(&config_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "skipping official marketplace auto-register: cannot read config.toml");
            return;
        }
    };
    let parsed: toml::Value = match toml::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "skipping official marketplace auto-register: invalid config.toml");
            return;
        }
    };

    // "Already present" means the official URL is in the config.toml sources or in a JSON store (settings.json, known_marketplaces.json) under distill_home
    // The scan is scoped to distill_home only (not ~/.claude) to keep tests hermetic
    // A user with the URL solely in ~/.claude gets one duplicate entry that the UI dedupes by URL
    let toml_sources = distill_plugin_marketplace::load_sources(&parsed);
    let json_sources = distill_plugin_marketplace::load_extra_sources_from_settings_in(
        &toml_sources,
        std::slice::from_ref(&distill_home.to_path_buf()),
    );
    let already_present = toml_sources.iter().chain(json_sources.iter()).any(|s| {
        matches!(&s.kind, distill_plugin_marketplace::SourceKind::Git { url, .. }
            if distill_plugin_marketplace::is_official_source_url(url))
    });

    let write_result = if already_present {
        // Already present: just set the flag.
        set_official_marketplace_auto_installed(&config_path)
    } else {
        add_marketplace_source(
            &config_path,
            distill_plugin_marketplace::OFFICIAL_SOURCE_NAME,
            &crate::plugin::MarketplaceAddInput::GitUrl(
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL.to_string(),
            ),
            true,
        )
    };

    match write_result {
        Ok(()) if !already_present => {
            tracing::info!(
                url = distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
                "auto-registered official xAI marketplace source"
            );
        }
        Ok(()) => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to auto-register official marketplace source");
        }
    }
}

#[cfg(test)]
mod official_source_tests {
    use super::*;

    fn read_sources(
        config_path: &std::path::Path,
    ) -> Vec<distill_plugin_marketplace::MarketplaceSource> {
        let raw = std::fs::read_to_string(config_path).unwrap_or_default();
        let parsed: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        distill_plugin_marketplace::load_sources(&parsed)
    }

    fn read_flag(config_path: &std::path::Path) -> bool {
        let raw = std::fs::read_to_string(config_path).unwrap_or_default();
        let parsed: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|_| toml::Value::Table(Default::default()));
        parsed
            .get("marketplace")
            .and_then(|m| m.get("official_marketplace_auto_installed"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    #[test]
    fn set_official_flag_in_toml_preserves_other_content() {
        let content = "[ui]\ntheme = \"dark\"\n";
        let out = crate::plugin::set_official_flag_in_toml(content).unwrap();
        assert!(out.contains("theme = \"dark\""), "{out}");
        assert!(
            out.contains("official_marketplace_auto_installed = true"),
            "{out}"
        );
    }

    #[test]
    fn add_marketplace_source_local_path_writes_path_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        let dir = tmp.path().join("my-plugins");
        std::fs::create_dir_all(&dir).unwrap();

        let input = crate::plugin::MarketplaceAddInput::LocalPath(dir.clone());
        add_marketplace_source(&config_path, "my-plugins", &input, false).unwrap();
        // Idempotent on the same path.
        add_marketplace_source(&config_path, "my-plugins", &input, false).unwrap();

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1);
        let Some(source) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert_eq!(source.name, "my-plugins");
        assert!(matches!(
            &source.kind,
            distill_plugin_marketplace::SourceKind::Local { path } if path == &dir
        ));
        // The path must not be mangled into a git URL.
        let raw = std::fs::read_to_string(&config_path).unwrap();
        assert!(!raw.contains("git ="), "{raw}");
    }

    /// Pins add-source dedup: a respelled URL (`.git`, host case) of a
    /// configured source must not write a duplicate entry.
    #[test]
    fn add_marketplace_source_dedupes_respelled_git_url() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");

        let original = crate::plugin::MarketplaceAddInput::GitUrl(
            "https://github.com/org/repo.git".to_string(),
        );
        add_marketplace_source(&config_path, "org", &original, false).unwrap();
        let respelled =
            crate::plugin::MarketplaceAddInput::GitUrl("https://GitHub.com/org/repo".to_string());
        add_marketplace_source(&config_path, "org-respelled", &respelled, false).unwrap();

        let sources = read_sources(&config_path);
        assert_eq!(
            sources.len(),
            1,
            "respelled URL must dedupe, got {sources:?}"
        );
        let Some(source) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert!(matches!(
            &source.kind,
            distill_plugin_marketplace::SourceKind::Git { url, .. }
                if url == "https://github.com/org/repo.git"
        ));
    }

    #[test]
    fn removing_last_nonofficial_source_preserves_flag_and_blocks_readd() {
        // Regression: removing the last (non-official) source must not wipe the sticky flag, or a gated startup would re-add the removed official source
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        std::fs::write(
            &config_path,
            "[marketplace]\nofficial_marketplace_auto_installed = true\n\n\
             [[marketplace.sources]]\nname = \"custom\"\ngit = \"https://example.com/custom.git\"\n",
        )
        .unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let new_content = crate::plugin::remove_toml_marketplace_block(
            &content,
            "https://example.com/custom.git",
        )
        .expect("custom source should be removed");
        std::fs::write(&config_path, &new_content).unwrap();

        assert!(
            read_flag(&config_path),
            "flag must be preserved after removing the last source: {new_content}"
        );

        ensure_official_marketplace_source(home);
        let sources = read_sources(&config_path);
        assert!(
            !sources.iter().any(|s| matches!(&s.kind,
                distill_plugin_marketplace::SourceKind::Git { url, .. }
                    if distill_plugin_marketplace::is_official_source_url(url))),
            "official source must not be re-added after removal"
        );
    }

    #[test]
    fn first_run_creates_source_and_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(config_path.exists(), "config.toml should be created");

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1);
        let Some(source) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert_eq!(
            source.name,
            distill_plugin_marketplace::OFFICIAL_SOURCE_NAME
        );
        assert!(matches!(
            &source.kind,
            distill_plugin_marketplace::SourceKind::Git { url, .. }
                if url == distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL
        ));
        assert!(read_flag(&config_path));
    }

    /// A marketplace policy blocking the official URL skips the auto-register AND leaves the sticky flag unset.
    #[test]
    fn policy_blocked_first_run_skips_register_and_retries_after_lift() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        let blocked = crate::plugin::test_fixtures::marketplace_allowlist(&[
            "https://github.com/corp/approved.git",
        ]);
        ensure_official_marketplace_source_with(home, &blocked);
        assert!(
            read_sources(&config_path).is_empty(),
            "blocked policy must skip the register"
        );
        assert!(
            !read_flag(&config_path),
            "flag must stay unset so a lifted policy retries"
        );

        ensure_official_marketplace_source_with(
            home,
            &distill_workspace::permission::resolution::MarketplacePolicy::default(),
        );
        assert_eq!(
            read_sources(&config_path).len(),
            1,
            "lifted policy registers"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn second_run_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();

        ensure_official_marketplace_source(home);
        let after_first = std::fs::read_to_string(home.join("config.toml")).unwrap();

        ensure_official_marketplace_source(home);
        let after_second = std::fs::read_to_string(home.join("config.toml")).unwrap();

        assert_eq!(
            after_first, after_second,
            "second run must not modify config"
        );
    }

    #[test]
    fn removed_source_stays_removed_across_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        ensure_official_marketplace_source(home);
        assert_eq!(read_sources(&config_path).len(), 1);

        // Simulate removal: drop the source block, keep the flag.
        std::fs::write(
            &config_path,
            "[marketplace]\nofficial_marketplace_auto_installed = true\n",
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        assert!(
            read_sources(&config_path).is_empty(),
            "official source must not be re-added after removal"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_just_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        // User added the official source manually (flag not set).
        std::fs::write(
            &config_path,
            format!(
                "[[marketplace.sources]]\nname = \"{}\"\ngit = \"{}\"\n",
                distill_plugin_marketplace::OFFICIAL_SOURCE_NAME,
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1, "must not duplicate existing source");
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_in_known_marketplaces_skips_append() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let plugins_dir = home.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("known_marketplaces.json"),
            format!(
                r#"{{"xai-official":{{"source":{{"source":"git","url":"{}"}}}}}}"#,
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(
            read_sources(&config_path).is_empty(),
            "must not append to config.toml when the source is already present in known_marketplaces.json"
        );
        assert!(
            read_flag(&config_path),
            "must set the auto-installed flag so subsequent restarts skip the append check"
        );
    }

    #[test]
    fn pre_existing_official_source_in_extra_known_marketplaces_skips_append() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(
            home.join("settings.json"),
            format!(
                r#"{{"extraKnownMarketplaces":{{"xai-official":{{"source":{{"source":"git","url":"{}"}}}}}}}}"#,
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let config_path = home.join("config.toml");
        assert!(
            read_sources(&config_path).is_empty(),
            "must not append to config.toml when source is in extraKnownMarketplaces"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn pre_existing_official_source_with_branch_just_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");

        // Official source pinned to a non-main branch: dedup must match URL alone.
        std::fs::write(
            &config_path,
            format!(
                "[[marketplace.sources]]\nname = \"{}\"\ngit = \"{}\"\nbranch = \"some-branch\"\n",
                distill_plugin_marketplace::OFFICIAL_SOURCE_NAME,
                distill_plugin_marketplace::OFFICIAL_SOURCE_GIT_URL,
            ),
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 1, "must not duplicate existing source");
        assert!(
            std::fs::read_to_string(&config_path)
                .unwrap()
                .contains("branch = \"some-branch\""),
            "branch override must survive registration"
        );
        assert!(read_flag(&config_path));
    }

    #[test]
    fn preserves_existing_user_sources_and_comments() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let config_path = home.join("config.toml");
        std::fs::write(
            &config_path,
            "# my custom marketplaces\n[[marketplace.sources]]\nname = \"Local\"\npath = \"/tmp/mine\"\n",
        )
        .unwrap();

        ensure_official_marketplace_source(home);

        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            after.contains("# my custom marketplaces"),
            "comments preserved"
        );
        assert!(after.contains("Local"), "existing source preserved");
        let sources = read_sources(&config_path);
        assert_eq!(sources.len(), 2);
        assert!(read_flag(&config_path));
    }
}

#[cfg(test)]
mod default_skills_purge_tests {
    use super::*;
    use distill_agent::plugins::install_registry::{
        InstallKind, InstallRegistry, InstalledRepo, MarketplaceProvenance, RepoPlugin,
    };

    fn repo_at(path: &std::path::Path, plugin_subdir: Option<&str>) -> InstalledRepo {
        InstalledRepo {
            kind: InstallKind::Local {
                source_path: path.to_path_buf(),
                subdir: None,
            },
            installed_at: String::new(),
            updated_at: String::new(),
            path: path.to_path_buf(),
            plugins: std::collections::HashMap::from([(
                "p".into(),
                RepoPlugin {
                    subdir: None,
                    version: None,
                },
            )]),
            marketplace: plugin_subdir.map(|subdir| MarketplaceProvenance {
                source_url_or_path: "https://example.com/market.git".into(),
                source_display_name: "Test".into(),
                plugin_subdir: subdir.into(),
            }),
        }
    }

    #[test]
    fn match_is_exact_plugin_subdir_only() {
        assert!(is_default_skills_plugin_subdir("default-skills"));
        assert!(!is_default_skills_plugin_subdir("plugins/default-skills"));
        assert!(!is_default_skills_plugin_subdir("default-skills/extra"));
        assert!(!is_default_skills_plugin_subdir("defaults-skills"));
        assert!(!is_default_skills_plugin_subdir(""));
    }

    #[test]
    fn collects_only_default_skills_repo_keys() {
        let default_skills = repo_at(std::path::Path::new("/tmp/ds"), Some("default-skills"));
        let other = repo_at(std::path::Path::new("/tmp/office"), Some("plugins/office"));
        let no_marketplace = repo_at(std::path::Path::new("/tmp/local"), None);

        let keys = default_skills_repo_keys([
            ("ds-aaaa", &default_skills),
            ("office-bbbb", &other),
            ("local-cccc", &no_marketplace),
        ]);
        assert_eq!(keys, vec!["ds-aaaa"]);
    }

    #[test]
    fn purged_flag_toml_preserves_other_content() {
        let content =
            "[ui]\ntheme = \"dark\"\n[marketplace]\nofficial_marketplace_auto_installed = true\n";
        let out = crate::plugin::set_marketplace_bool_flag_in_toml(
            content,
            "default_skills_installs_purged",
        )
        .unwrap();
        assert!(out.contains("theme = \"dark\""), "{out}");
        assert!(
            out.contains("official_marketplace_auto_installed = true"),
            "{out}"
        );
        assert!(
            out.contains("default_skills_installs_purged = true"),
            "{out}"
        );
    }

    #[test]
    fn read_purged_flag_false_when_missing_or_wrong_type() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        assert!(!read_default_skills_installs_purged(&path));

        std::fs::write(
            &path,
            "[marketplace]\ndefault_skills_installs_purged = \"yes\"\n",
        )
        .unwrap();
        assert!(!read_default_skills_installs_purged(&path));

        std::fs::write(
            &path,
            "[marketplace]\ndefault_skills_installs_purged = true\n",
        )
        .unwrap();
        assert!(read_default_skills_installs_purged(&path));
    }

    #[test]
    fn purge_sets_flag_when_nothing_to_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        purge_default_skills_installs_impl(home, &install_dir, || {
            Ok(InstallRegistry::empty(install_dir.clone()))
        });
        let config_path = home.join("config.toml");
        assert!(read_default_skills_installs_purged(&config_path));

        let after_first = std::fs::read_to_string(&config_path).unwrap();
        purge_default_skills_installs_impl(home, &install_dir, || {
            Ok(InstallRegistry::empty(install_dir.clone()))
        });
        let after_second = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn purge_skips_flag_when_registry_load_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(install_dir.join("registry.json"), "{not-json").unwrap();

        purge_default_skills_installs_impl(home, &install_dir, || {
            InstallRegistry::try_load_from(install_dir.clone())
        });

        assert!(!read_default_skills_installs_purged(
            &home.join("config.toml")
        ));
    }

    /// While another writer holds the registry flock the purge must skip; the sticky flag stays unset so it retries.
    #[test]
    fn purge_skips_while_registry_flock_held() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        std::fs::create_dir_all(&install_dir).unwrap();

        let ds_path = install_dir.join("ds-aaaa");
        std::fs::create_dir_all(&ds_path).unwrap();
        let mut registry = InstallRegistry::empty(install_dir.clone());
        registry.insert("ds-aaaa".into(), repo_at(&ds_path, Some("default-skills")));
        registry.save().unwrap();

        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(install_dir.join("registry.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock_file).unwrap();

        let install_dir_for_load = install_dir.clone();
        purge_default_skills_installs_impl(home, &install_dir, move || {
            InstallRegistry::try_load_from(install_dir_for_load)
        });

        assert!(
            ds_path.exists(),
            "purge must not delete installs while the registry flock is held"
        );
        let reloaded = InstallRegistry::load_from(install_dir);
        assert!(
            reloaded.get_repo("ds-aaaa").is_some(),
            "purge must not save an unlocked registry while the flock is held"
        );
        assert!(
            !read_default_skills_installs_purged(&home.join("config.toml")),
            "a skipped purge must not set the sticky flag"
        );
    }

    #[test]
    fn purge_removes_default_skills_retains_others_and_sets_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let install_dir = home.join("installed-plugins");
        std::fs::create_dir_all(&install_dir).unwrap();

        let ds_path = install_dir.join("ds-aaaa");
        std::fs::create_dir_all(&ds_path).unwrap();
        std::fs::write(ds_path.join("marker"), "ds").unwrap();

        let other_path = install_dir.join("office-bbbb");
        std::fs::create_dir_all(&other_path).unwrap();
        std::fs::write(other_path.join("marker"), "office").unwrap();

        let mut registry = InstallRegistry::empty(install_dir.clone());
        registry.insert("ds-aaaa".into(), repo_at(&ds_path, Some("default-skills")));
        registry.insert(
            "office-bbbb".into(),
            repo_at(&other_path, Some("plugins/office")),
        );
        registry.save().unwrap();

        let install_dir_for_load = install_dir.clone();
        purge_default_skills_installs_impl(home, &install_dir, move || {
            InstallRegistry::try_load_from(install_dir_for_load)
        });

        assert!(
            !ds_path.exists(),
            "default-skills install dir must be removed"
        );
        assert!(other_path.exists(), "non-matching install must be retained");

        let reloaded = InstallRegistry::load_from(install_dir.clone());
        assert!(reloaded.get_repo("ds-aaaa").is_none());
        assert!(reloaded.get_repo("office-bbbb").is_some());

        let config_path = home.join("config.toml");
        assert!(read_default_skills_installs_purged(&config_path));

        let after_first = std::fs::read_to_string(&config_path).unwrap();
        let install_dir_for_reload = install_dir.clone();
        purge_default_skills_installs_impl(home, &install_dir, move || {
            InstallRegistry::try_load_from(install_dir_for_reload)
        });
        let after_second = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(after_first, after_second);
        assert!(other_path.exists());
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    #[test]
    fn to_plugin_entry_carries_homepage_and_keywords() {
        let entry = distill_plugin_marketplace::MarketplaceEntry {
            name: "demo".into(),
            version: Some("1.0.0".into()),
            description: Some("demo".into()),
            category: Some("dev".into()),
            author: Some("xai".into()),
            tags: vec!["cli".into()],
            keywords: vec!["search".into(), "rank".into()],
            domains: vec!["example.com".into()],
            homepage: Some("https://example.com/demo".into()),
            relative_path: "plugins/demo".into(),
            skill_count: 2,
            has_hooks: true,
            has_agents: false,
            has_mcp: false,
            remote_url: None,
            remote_ref: None,
            remote_sha: None,
            remote_subdir: Some("plugins/acme".into()),
            components: Some(distill_hooks_plugins_types::PluginComponents {
                skills: vec![distill_hooks_plugins_types::ComponentItem::new(
                    "code-review",
                    Some("Review staged changes".to_string()),
                )],
                ..Default::default()
            }),
        };

        let dto = to_plugin_entry(entry, "not_installed".to_string(), None);

        assert_eq!(dto.homepage.as_deref(), Some("https://example.com/demo"));
        assert_eq!(dto.keywords, vec!["search".to_string(), "rank".to_string()]);
        assert_eq!(dto.domains, vec!["example.com".to_string()]);
        assert_eq!(dto.tags, vec!["cli".to_string()]);
        assert_eq!(dto.install_status, "not_installed");
        assert_eq!(dto.remote_subdir.as_deref(), Some("plugins/acme"));
        let components = dto.components.expect("components passed through");
        assert_eq!(components.skills.len(), 1);
        let Some(skill) = components.skills.first() else {
            panic!("expected one skill: {:?}", components.skills);
        };
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description.as_deref(), Some("Review staged changes"));
    }
}
