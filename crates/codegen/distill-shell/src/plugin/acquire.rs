// Modified for Distill by Samuel Fajreldines, 2026.
//! The ONE plugin acquisition pipeline: every code-fetching path resolves sources and gates here.
//! LocalSet invariant: acquisition blocks — hop via [`run_blocking`], never the session LocalSet.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use distill_agent::plugins::git_install;
use distill_agent::plugins::install_registry::{
    InstallError, InstallKind, InstallRegistry, InstalledRepo, MarketplaceProvenance,
};
use distill_plugin_marketplace::git::{self, SourceCacheLease};
use distill_plugin_marketplace::{
    MarketplaceEntry, MarketplaceRelativePath, MarketplaceSource, SourceKind, installer,
    scan_marketplace,
};

/// A completed marketplace install (registry saved by the installer).
#[derive(Debug)]
pub(crate) struct Installed {
    pub(crate) repo_key: String,
    pub(crate) source_name: String,
    pub(crate) plugin_relative_path: String,
}

/// [`marketplace_install`] failure. Carries data, not per-surface text;
/// only the gate refusals (identical on every surface) are pre-formatted.
#[derive(Debug)]
pub(crate) enum InstallAcquireError {
    /// An acquisition gate refused the source; `reason` is the full gate
    /// message ("Plugin install blocked: …").
    Blocked { reason: String },
    /// The requested identity is not a configured, allowlist-surviving source.
    SourceNotFound { source: String },
    /// Already installed from this source.
    AlreadyInstalled { repo_key: String },
    /// Git source sync failed; `detail` is the sync error.
    Sync { detail: String },
    /// The resolved entry failed to install.
    Entry(EntryInstallError),
}

/// [`marketplace_update`] failure.
#[derive(Debug)]
pub(crate) enum UpdateAcquireError {
    /// An acquisition gate refused the source; `reason` is the full gate
    /// message ("Plugin update blocked: …").
    Blocked { reason: String },
    /// Update provenance no longer resolves to a configured source. `message`
    /// is [`ProvenanceUpdateError::NotConfigured`]'s text.
    NotConfigured { message: String },
    /// The (normalized) plugin path is missing from the synced scan.
    EntryNotFound { plugin_relative_path: String },
    /// The stored plugin relative path failed to parse.
    InvalidPluginPath { detail: String },
    /// Git source sync failed; `detail` is the sync error.
    Sync { detail: String },
    /// Underlying installer failure, passed through for per-surface display.
    Install(InstallError),
}

/// Failure of an async [`run_marketplace_install`] / [`run_marketplace_update`]
/// wrapper: the operation's own error, or the lock/blocking-task plumbing.
#[derive(Debug)]
pub(crate) enum RunError<E> {
    Op(E),
    /// The install-registry flock could not be acquired (see
    /// [`lock_install_registry`]); `detail` names the path and timeout.
    RegistryLock {
        detail: String,
    },
    /// The blocking task did not complete.
    TaskJoin(tokio::task::JoinError),
}

/// Exclusive flock guard over the install registry; drop releases it.
#[derive(Debug)]
pub(crate) struct InstallRegistryLock {
    _file: std::fs::File,
}

/// Deliberately 30s: a waiter can queue behind a full `update_plugins` sync
/// loop, and a timeout reads as "another plugin operation is in progress".
const REGISTRY_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// THE registry-mutation chokepoint: every writer holds this flock from load past save, or
/// interleaved windows lose entries. Lock order: config-init ⊃ registry ⊃ cache; never on the LocalSet.
pub(crate) fn lock_install_registry() -> Result<InstallRegistryLock, String> {
    lock_install_registry_in(
        &InstallRegistry::resolve_install_dir(),
        REGISTRY_LOCK_TIMEOUT,
    )
}

/// [`lock_install_registry`] at an explicit install dir with a caller-chosen timeout, for callers
/// that resolve the dir themselves or cannot afford the full 30s wait.
pub(crate) fn lock_install_registry_in(
    install_dir: &Path,
    timeout: std::time::Duration,
) -> Result<InstallRegistryLock, String> {
    std::fs::create_dir_all(install_dir).map_err(|e| {
        format!(
            "failed to create install dir {}: {e}",
            install_dir.display()
        )
    })?;
    lock_install_registry_at(&install_dir.join("registry.lock"), timeout)
}

fn lock_install_registry_at(
    lock_path: &Path,
    timeout: std::time::Duration,
) -> Result<InstallRegistryLock, String> {
    git::acquire_file_lock("registry", lock_path, timeout)
        .map(|file| InstallRegistryLock { _file: file })
}

/// One generic blocking-pool hop for async callers (see module docs); running `f` inline
/// re-freezes the session actor (regression-pinned).
pub(crate) async fn run_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(f).await
}

/// Registry lock + fresh load + `op`, on the blocking pool. Marketplace
/// installers persist the registry themselves before the lock drops.
async fn run_locked<T, E>(
    op: impl FnOnce(&mut InstallRegistry) -> Result<T, E> + Send + 'static,
) -> Result<T, RunError<E>>
where
    T: Send + 'static,
    E: Send + 'static,
{
    match run_blocking(move || {
        let _registry_lock =
            lock_install_registry().map_err(|detail| RunError::RegistryLock { detail })?;
        let mut registry = InstallRegistry::load();
        op(&mut registry).map_err(RunError::Op)
    })
    .await
    {
        Ok(result) => result,
        Err(e) => Err(RunError::TaskJoin(e)),
    }
}

/// Async [`marketplace_install`] (the `x.ai/marketplace/action` Install
/// handler).
pub(crate) async fn run_marketplace_install(
    source_url_or_path: String,
    plugin_relative_path: String,
) -> Result<Installed, RunError<InstallAcquireError>> {
    run_locked(move |registry| {
        marketplace_install(&source_url_or_path, &plugin_relative_path, registry)
    })
    .await
}

/// Async [`marketplace_update`] (the `x.ai/marketplace/action` Update
/// handler).
pub(crate) async fn run_marketplace_update(
    provenance: MarketplaceProvenance,
    refresh_display_name: bool,
) -> Result<installer::MarketplaceUpdateResult, RunError<UpdateAcquireError>> {
    run_locked(move |registry| {
        let mut source_cache = HashMap::new();
        marketplace_update(
            provenance,
            refresh_display_name,
            registry,
            &mut source_cache,
            &MarketplaceSourceLists::load(),
        )
    })
    .await
}

/// Marketplace install by source identity + plugin relative path: source resolution, gates, git
/// sync, scan, and the registry apply (saved by the installer).
pub(crate) fn marketplace_install(
    source_url_or_path: &str,
    plugin_relative_path: &str,
    registry: &mut InstallRegistry,
) -> Result<Installed, InstallAcquireError> {
    let sources = super::load_filtered_marketplace_sources();
    let source = sources
        .iter()
        .find(|s| s.identity() == source_url_or_path)
        .ok_or_else(|| {
            install_source_missing_error(
                &super::load_marketplace_sources(),
                &distill_workspace::permission::resolution::managed_settings()
                    .marketplace_allowlist,
                source_url_or_path,
            )
        })?;

    // Sync once (TTL-cached) and hold the lease across scan + install so the
    // source cache can't be evicted mid-use.
    let marketplace_root = match &source.kind {
        SourceKind::Local { path } => MarketplaceSourceRoot {
            path: path.clone(),
            _lease: None,
        },
        SourceKind::Git { url, branch } => {
            let lease = git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                &git::default_cache_root(),
                git::SyncMode::UseTtl,
            )
            .map_err(|detail| InstallAcquireError::Sync { detail })?;
            MarketplaceSourceRoot {
                path: lease.path.clone(),
                _lease: Some(lease),
            }
        }
    };

    // A missing path takes the entry core's local-directory fork, whose
    // is_dir check reports the not-found case.
    let entry = scan_marketplace(&marketplace_root.path)
        .entries
        .into_iter()
        .find(|entry| entry.relative_path == plugin_relative_path);

    match install_marketplace_entry(
        source,
        &marketplace_root.path,
        plugin_relative_path,
        entry.as_ref(),
        registry,
    ) {
        Ok(installer::MarketplaceInstallResult::Installed { repo_key }) => Ok(Installed {
            repo_key,
            source_name: source.name.clone(),
            plugin_relative_path: plugin_relative_path.to_string(),
        }),
        Ok(installer::MarketplaceInstallResult::AlreadyInstalled { repo_key }) => {
            Err(InstallAcquireError::AlreadyInstalled { repo_key })
        }
        Err(e) => Err(InstallAcquireError::Entry(e)),
    }
}

/// A resolved marketplace entry failed to install (see
/// [`install_marketplace_entry`]).
#[derive(Debug)]
pub(crate) enum EntryInstallError {
    /// The plugin relative path failed to parse; `detail` is the parse error.
    InvalidPluginPath { detail: String },
    /// Local-directory fork: plugin directory missing under the root.
    PluginDirNotFound { dir: PathBuf },
    /// Underlying installer failure.
    Install(InstallError),
}

/// The ONE install core for a resolved marketplace entry (remote-vs-local fork, provenance,
/// installer dispatch), shared by [`marketplace_install`] and the CLI by-name path.
pub(crate) fn install_marketplace_entry(
    source: &MarketplaceSource,
    marketplace_root: &Path,
    plugin_relative_path: &str,
    entry: Option<&MarketplaceEntry>,
    registry: &mut InstallRegistry,
) -> Result<installer::MarketplaceInstallResult, EntryInstallError> {
    let require_sha = marketplace_require_sha();
    if let Some((entry, remote_url)) =
        entry.and_then(|entry| entry.remote_url.as_deref().map(|url| (entry, url)))
    {
        // URL-sourced plugin: clone from its remote git URL. Provenance keeps
        // the request's raw path (matching the marketplace index entry).
        let provenance = MarketplaceProvenance {
            source_url_or_path: source.identity(),
            source_display_name: source.name.clone(),
            plugin_subdir: plugin_relative_path.to_string(),
        };
        installer::install_from_remote_url(
            remote_url,
            entry.remote_ref.as_deref(),
            entry.remote_sha.as_deref(),
            entry.remote_subdir.as_deref(),
            plugin_relative_path,
            provenance,
            registry,
            require_sha,
        )
        .map_err(EntryInstallError::Install)
    } else {
        // Local-sourced plugin: resolve from the marketplace directory.
        let plugin_path = MarketplaceRelativePath::parse(plugin_relative_path).map_err(|e| {
            EntryInstallError::InvalidPluginPath {
                detail: e.to_string(),
            }
        })?;
        let plugin_dir = plugin_path.join_under(marketplace_root).map_err(|e| {
            EntryInstallError::InvalidPluginPath {
                detail: e.to_string(),
            }
        })?;
        if !plugin_dir.is_dir() {
            return Err(EntryInstallError::PluginDirNotFound { dir: plugin_dir });
        }
        let plugin_relative_path = plugin_path.as_str();

        let provenance = MarketplaceProvenance {
            source_url_or_path: source.identity(),
            source_display_name: source.name.clone(),
            plugin_subdir: plugin_relative_path.to_string(),
        };
        installer::install_from_marketplace(
            marketplace_root,
            plugin_relative_path,
            provenance,
            registry,
        )
        .map_err(EntryInstallError::Install)
    }
}

/// A configured-but-policy-dropped install source reports the policy instead of "not found" —
/// the twin of [`provenance_update_source`]'s Blocked leg.
fn install_source_missing_error(
    unfiltered: &[MarketplaceSource],
    policy: &distill_workspace::permission::resolution::MarketplacePolicy,
    source_url_or_path: &str,
) -> InstallAcquireError {
    if unfiltered
        .iter()
        .any(|s| s.identity() == source_url_or_path)
    {
        let reason = policy
            .add_block_reason(source_url_or_path)
            .unwrap_or_else(|| "source not in strictKnownMarketplaces".to_string());
        InstallAcquireError::Blocked {
            reason: format!("Plugin install blocked: {reason}"),
        }
    } else {
        InstallAcquireError::SourceNotFound {
            source: source_url_or_path.to_string(),
        }
    }
}

/// Both marketplace source lists, loaded once per pipeline invocation (each load re-reads
/// config.toml, managed pins, and the JSON stores).
pub(crate) struct MarketplaceSourceLists {
    /// Allowlist-surviving sources (the resolvable set).
    filtered: Vec<MarketplaceSource>,
    /// All configured sources, to tell policy-blocked from unconfigured.
    unfiltered: Vec<MarketplaceSource>,
}

impl MarketplaceSourceLists {
    pub(crate) fn load() -> Self {
        Self {
            filtered: super::load_filtered_marketplace_sources(),
            unfiltered: super::load_marketplace_sources(),
        }
    }
}

/// Update one installed repo from its marketplace provenance (registry saved by the installer);
/// `source_cache` reuses synced roots so one `plugin update` syncs each source once.
pub(crate) fn marketplace_update(
    mut provenance: MarketplaceProvenance,
    refresh_display_name: bool,
    registry: &mut InstallRegistry,
    source_cache: &mut HashMap<String, MarketplaceSourceRoot>,
    sources: &MarketplaceSourceLists,
) -> Result<installer::MarketplaceUpdateResult, UpdateAcquireError> {
    // Gate first (fail closed): a policy-blocked source reports the policy,
    // an unconfigured one refuses the raw-URL force-sync.
    let source = provenance_update_source(
        &sources.filtered,
        &sources.unfiltered,
        &distill_workspace::permission::resolution::managed_settings().marketplace_allowlist,
        &provenance.source_url_or_path,
    )
    .map_err(|e| match &e {
        ProvenanceUpdateError::Blocked { .. } => UpdateAcquireError::Blocked {
            reason: e.to_string(),
        },
        ProvenanceUpdateError::NotConfigured { .. } => UpdateAcquireError::NotConfigured {
            message: e.to_string(),
        },
    })?;

    let entry_path = MarketplaceRelativePath::parse(&provenance.plugin_subdir).map_err(|e| {
        UpdateAcquireError::InvalidPluginPath {
            detail: e.to_string(),
        }
    })?;

    let marketplace_root = update_source_root(&source, source_cache)?;
    let entry = scan_marketplace(&marketplace_root.path)
        .entries
        .into_iter()
        .find(|entry| entry.relative_path == entry_path.as_str())
        .ok_or_else(|| UpdateAcquireError::EntryNotFound {
            plugin_relative_path: entry_path.as_str().to_string(),
        })?;

    if refresh_display_name {
        provenance.source_display_name = source.name.clone();
    }
    installer::update_from_marketplace_entry_transactional(
        &marketplace_root.path,
        &entry,
        provenance,
        registry,
        marketplace_require_sha(),
    )
    .map_err(UpdateAcquireError::Install)
}

/// Resolve the marketplace root for an update, force-syncing git sources; caches key on the
/// RESOLVED source identity — a raw-provenance key would re-enter the held source-cache flock.
fn update_source_root<'cache>(
    source: &MarketplaceSource,
    source_cache: &'cache mut HashMap<String, MarketplaceSourceRoot>,
) -> Result<&'cache MarketplaceSourceRoot, UpdateAcquireError> {
    match source_cache.entry(source.identity()) {
        std::collections::hash_map::Entry::Occupied(entry) => Ok(entry.into_mut()),
        std::collections::hash_map::Entry::Vacant(entry) => {
            let root = match &source.kind {
                SourceKind::Git { url, branch } => {
                    let lease = git::sync_source_cache_with_mode(
                        url,
                        branch.as_deref(),
                        &git::default_cache_root(),
                        git::SyncMode::Force,
                    )
                    .map_err(|detail| UpdateAcquireError::Sync { detail })?;
                    MarketplaceSourceRoot {
                        path: lease.path.clone(),
                        _lease: Some(lease),
                    }
                }
                SourceKind::Local { path } => MarketplaceSourceRoot {
                    path: path.clone(),
                    _lease: None,
                },
            };
            Ok(entry.insert(root))
        }
    }
}

/// [`direct_install`] failure: the gate refusal or the underlying installer.
#[derive(Debug)]
pub(crate) enum DirectInstallError {
    /// The acquisition gate refused the source; `reason` is the full
    /// pre-formatted "Plugin install blocked: …" message.
    Blocked {
        reason: String,
    },
    Install(InstallError),
}

/// Direct install (git URL / local path) — the CLI `grok plugin install` path; registry mutated
/// but NOT saved: the caller owns the save, holding the registry lock across load→install→save.
pub(crate) fn direct_install(
    source: &git_install::InstallSource,
    registry: &mut InstallRegistry,
) -> Result<String, DirectInstallError> {
    // Direct installs must not bypass the marketplace lockdown (hooks execute).
    let policy =
        &distill_workspace::permission::resolution::managed_settings().marketplace_allowlist;
    if let Some(reason) = direct_install_block_reason(policy, source) {
        return Err(DirectInstallError::Blocked { reason });
    }
    let result = git_install::install_from_source(source, registry, marketplace_require_sha())
        .map_err(DirectInstallError::Install)?;
    let repo = git_install::build_installed_repo(&result, source);
    registry.insert(result.repo_key.clone(), repo);
    Ok(result.repo_key)
}

/// Fail-closed direct-install gate (same rules as the marketplace add gate).
fn direct_install_block_reason(
    policy: &distill_workspace::permission::resolution::MarketplacePolicy,
    source: &git_install::InstallSource,
) -> Option<String> {
    let identity = match source {
        git_install::InstallSource::Git { url, .. } => url.clone(),
        git_install::InstallSource::Local { path, .. } => path.display().to_string(),
    };
    policy
        .add_block_reason(&identity)
        .map(|reason| format!("Plugin install blocked: {reason}"))
}

/// The update twin of [`direct_install_block_reason`]: only git repos fetch on update; a `Local`
/// install's update is a no-op (`UpdateStatus::LiveLocal`), so it stays ungated.
pub(crate) fn direct_update_block_reason(
    policy: &distill_workspace::permission::resolution::MarketplacePolicy,
    repo: &InstalledRepo,
) -> Option<String> {
    let InstallKind::Git { url, .. } = &repo.kind else {
        return None;
    };
    policy
        .add_block_reason(url)
        .map(|reason| format!("Plugin update blocked: {reason}"))
}

/// Why an installed plugin's provenance no longer resolves to an updatable
/// source (see [`provenance_update_source`]).
#[derive(Debug)]
pub(crate) enum ProvenanceUpdateError {
    /// Configured, but dropped by the managed marketplace policy.
    Blocked { reason: String },
    /// Not configured at all — the raw provenance URL must not be synced.
    NotConfigured { source: String },
}

impl std::fmt::Display for ProvenanceUpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blocked { reason } => write!(f, "Plugin update blocked: {reason}"),
            Self::NotConfigured { source } => write!(
                f,
                "marketplace source is no longer configured: {source}; \
                 re-add it with `grok plugin marketplace add` or reinstall the plugin"
            ),
        }
    }
}

/// Resolve an installed plugin's provenance to a configured, allowlist-surviving source before
/// any sync: a raw provenance URL must never be force-synced (it may now be blocked or removed).
pub(crate) fn provenance_update_source(
    filtered: &[MarketplaceSource],
    unfiltered: &[MarketplaceSource],
    policy: &distill_workspace::permission::resolution::MarketplacePolicy,
    source_url_or_path: &str,
) -> Result<MarketplaceSource, ProvenanceUpdateError> {
    use distill_workspace::permission::resolution::normalize_git_url;
    let matches = |source: &MarketplaceSource| match &source.kind {
        // Normalized comparison: provenance records the config spelling at install time, which may
        // drift (.git suffix, host case) from the current entry.
        SourceKind::Git { url, .. } => {
            normalize_git_url(url) == normalize_git_url(source_url_or_path)
        }
        SourceKind::Local { path } => Path::new(source_url_or_path) == path,
    };
    if let Some(source) = filtered.iter().find(|s| matches(s)) {
        return Ok(source.clone());
    }
    if unfiltered.iter().any(matches) {
        let reason = policy
            .add_block_reason(source_url_or_path)
            .unwrap_or_else(|| "source not in strictKnownMarketplaces".to_string());
        Err(ProvenanceUpdateError::Blocked { reason })
    } else {
        Err(ProvenanceUpdateError::NotConfigured {
            source: source_url_or_path.to_string(),
        })
    }
}

/// The require-sha pin for remote plugin code: disk config + env, both tighten-only, read from
/// the overlay-free layer merge so no `GROK_CONFIG` overlay can relax a disk-set `true`.
pub(crate) fn marketplace_require_sha() -> bool {
    require_sha_policy(distill_config::ConfigLayers::load())
}

/// Fail closed: an unreadable config may hide a disk-set `require_sha = true`, so a load error
/// pins the requirement instead of falling back to the (relaxable) env knob.
fn require_sha_policy(layers: std::io::Result<distill_config::ConfigLayers>) -> bool {
    match layers {
        Ok(layers) => distill_plugin_marketplace::load_require_sha(
            &layers.effective_config_base_without_overlay(),
        ),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "config unreadable; requiring sha-pinned plugin sources (fail closed)"
            );
            true
        }
    }
}

/// A usable marketplace root: a local directory or a synced git cache
/// checkout whose lease keeps the cache from being evicted while in use.
pub(crate) struct MarketplaceSourceRoot {
    pub(crate) path: PathBuf,
    pub(crate) _lease: Option<SourceCacheLease>,
}

/// Root resolution for the CLI marketplace install-by-name path (TTL-cached
/// git sync; a missing local directory is a hard error there).
pub(crate) fn resolve_source_root_for_install(
    source: &MarketplaceSource,
    cache_root: &Path,
) -> Result<MarketplaceSourceRoot, String> {
    match &source.kind {
        SourceKind::Local { path } => {
            if path.is_dir() {
                Ok(MarketplaceSourceRoot {
                    path: path.clone(),
                    _lease: None,
                })
            } else {
                Err(format!(
                    "local source directory not found: {}",
                    path.display()
                ))
            }
        }
        SourceKind::Git { url, branch } => {
            let lease = git::sync_source_cache_with_mode(
                url,
                branch.as_deref(),
                cache_root,
                git::SyncMode::UseTtl,
            )?;
            Ok(MarketplaceSourceRoot {
                path: lease.path.clone(),
                _lease: Some(lease),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_fixtures::{git_source, marketplace_allowlist};
    use std::path::PathBuf;

    /// Direct-install gate: fail-closed under a restricted policy.
    #[test]
    fn direct_install_gate_fails_closed_under_restricted_policy() {
        use distill_agent::plugins::git_install::InstallSource;

        let restricted = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let git = |url: &str| InstallSource::Git {
            url: url.into(),
            git_ref: None,
            git_sha: None,
            subdir: None,
        };
        assert!(
            direct_install_block_reason(&restricted, &git("https://github.com/ok/repo.git"))
                .is_none()
        );
        assert!(
            direct_install_block_reason(&restricted, &git("https://github.com/evil/repo.git"))
                .is_some()
        );
        assert!(
            direct_install_block_reason(
                &restricted,
                &InstallSource::Local {
                    path: PathBuf::from("/tmp/mp"),
                    subdir: None
                }
            )
            .is_some()
        );

        let unrestricted = distill_workspace::permission::resolution::MarketplacePolicy::default();
        assert!(
            direct_install_block_reason(&unrestricted, &git("https://github.com/any/repo.git"))
                .is_none()
        );
    }

    /// Update resolves provenance against the filtered sources: blocked reports the policy, unconfigured refuses the force-sync.
    #[test]
    fn provenance_update_source_gates_blocked_and_unconfigured() {
        let policy = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let ok = git_source("Allowed", "https://github.com/ok/repo.git");
        let blocked = git_source("Blocked", "https://github.com/evil/mp.git");
        let unfiltered = vec![ok.clone(), blocked];
        let filtered = vec![ok];

        let resolved = provenance_update_source(
            &filtered,
            &unfiltered,
            &policy,
            "https://github.com/ok/repo.git",
        )
        .expect("allowed source resolves");
        assert_eq!(resolved.name, "Allowed");

        let err = provenance_update_source(
            &filtered,
            &unfiltered,
            &policy,
            "https://github.com/evil/mp.git",
        )
        .expect_err("blocked source must not sync");
        assert!(
            err.to_string().contains("Plugin update blocked"),
            "got: {err}"
        );

        let err = provenance_update_source(
            &filtered,
            &unfiltered,
            &policy,
            "https://github.com/gone/mp.git",
        )
        .expect_err("unconfigured provenance must not force-sync");
        assert!(
            err.to_string().contains("no longer configured"),
            "got: {err}"
        );
    }

    /// An unreadable config must pin require-sha (fail closed): the disk may
    /// carry `require_sha = true`, and the env knob must not relax it.
    #[test]
    fn unreadable_config_fails_closed_to_require_sha() {
        assert!(require_sha_policy(Err(std::io::Error::other(
            "corrupt config.toml"
        ))));
    }

    /// A configured-but-policy-blocked install source reports the policy
    /// refusal; only a truly unconfigured one reports "not found".
    #[test]
    fn install_source_missing_reports_blocked_vs_not_found() {
        let policy = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let configured = vec![git_source("Blocked", "https://github.com/evil/mp.git")];

        let err =
            install_source_missing_error(&configured, &policy, "https://github.com/evil/mp.git");
        match &err {
            InstallAcquireError::Blocked { reason } => {
                assert!(reason.contains("Plugin install blocked"), "got: {reason}")
            }
            other => panic!("configured blocked source must report the policy, got {other:?}"),
        }

        let err =
            install_source_missing_error(&configured, &policy, "https://github.com/gone/mp.git");
        assert!(
            matches!(&err, InstallAcquireError::SourceNotFound { source }
                if source == "https://github.com/gone/mp.git"),
            "unconfigured source must stay not-found, got {err:?}"
        );
    }

    /// Pins the blocking-pool hop behind every async acquisition: an inline [`run_blocking`] re-freezes the session actor.
    #[test]
    fn run_blocking_keeps_the_caller_local_set_live() {
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::Duration;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        let ticks = Rc::new(Cell::new(0u32));
        let ticker_ticks = Rc::clone(&ticks);
        rt.block_on(local.run_until(async move {
            tokio::task::spawn_local(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    ticker_ticks.set(ticker_ticks.get() + 1);
                }
            });
            let out = run_blocking(|| {
                // A stalled acquisition (git fetch, lock poll).
                std::thread::sleep(Duration::from_millis(200));
                42
            })
            .await
            .expect("blocking task completes");
            assert_eq!(out, 42);
            assert!(
                ticks.get() >= 10,
                "LocalSet starved during a blocking acquisition: {} ticks",
                ticks.get()
            );
        }));
    }

    /// Writer exclusion (per-OFD flock, in-process too): a second writer times out while held, then acquires after release.
    #[test]
    fn registry_lock_excludes_second_holder_until_released() {
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let lock_path = dir.path().join("registry.lock");
        let held = lock_install_registry_at(&lock_path, Duration::from_millis(50))
            .expect("first lock acquires");
        let detail = lock_install_registry_at(&lock_path, Duration::from_millis(150))
            .expect_err("second holder must time out while the lock is held");
        assert!(
            detail.contains("registry lock timeout"),
            "timeout must name the registry lock, got: {detail}"
        );
        drop(held);
        lock_install_registry_at(&lock_path, Duration::from_millis(50))
            .expect("lock re-acquirable after release");
    }

    /// Update cache key: two provenance spellings of one source share ONE entry — a raw key re-syncs and self-conflicts on the flock.
    #[test]
    fn provenance_spellings_of_one_source_share_one_update_cache_entry() {
        use std::collections::HashMap;

        let configured = git_source("Corp", "https://github.com/grok-test-nonexistent/mp");
        let policy = distill_workspace::permission::resolution::MarketplacePolicy::default();
        let seeded_root = PathBuf::from("/seeded-by-first-repo");
        let mut cache: HashMap<String, MarketplaceSourceRoot> = HashMap::new();
        // The first repo's update already synced and cached this source.
        cache.insert(
            configured.identity(),
            MarketplaceSourceRoot {
                path: seeded_root.clone(),
                _lease: None,
            },
        );
        // The second repo's provenance spells the same source differently.
        for spelling in [
            "https://GitHub.com/grok-test-nonexistent/mp.git",
            "https://github.com/grok-test-nonexistent/mp",
        ] {
            let source = provenance_update_source(
                std::slice::from_ref(&configured),
                std::slice::from_ref(&configured),
                &policy,
                spelling,
            )
            .expect("both spellings resolve to the configured source");
            let root = update_source_root(&source, &mut cache).expect(
                "cache hit — a sync attempt means the cache key regressed to the raw provenance spelling",
            );
            assert_eq!(root.path, seeded_root);
        }
        assert_eq!(
            cache.len(),
            1,
            "one configured source must occupy one cache entry"
        );
    }

    /// The update twin of the direct-install gate: a gate-failing git URL must not fetch; local installs stay ungated.
    #[test]
    fn direct_update_gate_blocks_git_repo_only() {
        let repo = |kind: InstallKind| InstalledRepo {
            kind,
            installed_at: String::new(),
            updated_at: String::new(),
            path: PathBuf::new(),
            plugins: std::collections::HashMap::new(),
            marketplace: None,
        };
        let restricted = marketplace_allowlist(&["https://github.com/ok/repo.git"]);
        let git_repo = |url: &str| {
            repo(InstallKind::Git {
                url: url.into(),
                git_ref: None,
                commit: String::new(),
                subdir: None,
            })
        };
        assert!(
            direct_update_block_reason(&restricted, &git_repo("https://github.com/ok/repo.git"))
                .is_none()
        );
        let reason =
            direct_update_block_reason(&restricted, &git_repo("https://github.com/evil/mp.git"))
                .expect("blocked git repo must not update");
        assert!(reason.contains("Plugin update blocked"), "got: {reason}");
        assert!(
            direct_update_block_reason(
                &restricted,
                &repo(InstallKind::Local {
                    source_path: PathBuf::from("/tmp/p"),
                    subdir: None,
                })
            )
            .is_none(),
            "local installs never fetch on update"
        );
    }
}
