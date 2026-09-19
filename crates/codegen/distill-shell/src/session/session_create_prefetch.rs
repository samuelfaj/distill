// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use tokio_util::task::AbortOnDropHandle;

use crate::agent::folder_trust::{self, TrustScan};

/// `session/new` / `session/load` `_meta` key carrying per-session plugin roots.
pub(crate) const SESSION_PLUGIN_DIRS_META_KEY: &str = "pluginDirs";

pub(crate) fn parse_session_plugin_dirs(meta: Option<&acp::Meta>) -> Vec<PathBuf> {
    let Some(entries) = meta
        .and_then(|m| m.get(SESSION_PLUGIN_DIRS_META_KEY))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let Some(raw) = entry.as_str() else {
            tracing::warn!(?entry, "pluginDirs entry is not a string; skipping");
            continue;
        };
        let path = PathBuf::from(raw);
        if !path.is_absolute() {
            tracing::warn!("pluginDirs entry is not absolute; skipping");
            continue;
        }
        let canonical = dunce::canonicalize(&path).unwrap_or(path);
        if !canonical.is_dir() {
            tracing::warn!("pluginDirs entry is not a directory; skipping");
            continue;
        }
        if !dirs.contains(&canonical) {
            dirs.push(canonical);
        }
    }
    dirs
}

pub(crate) struct SessionCreatePrefetchInputs {
    pub cwd: PathBuf,
    pub scan: TrustScan,
    pub plugin_handle: distill_agent::plugins::SharedPluginRegistryHandle,
    pub session_plugin_dirs: Vec<PathBuf>,
}

pub(crate) struct SessionCreatePrefetch {
    cwd: PathBuf,
    scan: TrustScan,
    plugin_handle: distill_agent::plugins::SharedPluginRegistryHandle,
    session_plugin_dirs: Vec<PathBuf>,
    /// Reconciled once; later callers share this verdict with the plugin refresh.
    reconciled: Option<bool>,
    plugins: PrefetchSlot<Option<Arc<distill_agent::plugins::PluginRegistry>>>,
}

impl SessionCreatePrefetch {
    /// Capture inputs only. Disk refresh starts in [`Self::resolve_trust`] with the reconciled verdict.
    pub(crate) fn begin(inputs: SessionCreatePrefetchInputs) -> Self {
        let SessionCreatePrefetchInputs {
            cwd,
            scan,
            plugin_handle,
            session_plugin_dirs,
        } = inputs;
        Self {
            cwd,
            scan,
            plugin_handle,
            session_plugin_dirs,
            reconciled: None,
            plugins: PrefetchSlot::empty(),
        }
    }

    pub(crate) fn launch_from_meta(
        cwd: &Path,
        scan: TrustScan,
        plugin_handle: distill_agent::plugins::SharedPluginRegistryHandle,
        session_meta: Option<&acp::Meta>,
    ) -> Self {
        Self::begin(SessionCreatePrefetchInputs {
            cwd: cwd.to_path_buf(),
            scan,
            plugin_handle,
            session_plugin_dirs: parse_session_plugin_dirs(session_meta),
        })
    }

    pub(crate) fn ready(
        plugin_registry: Option<Arc<distill_agent::plugins::PluginRegistry>>,
    ) -> Self {
        Self {
            cwd: PathBuf::new(),
            scan: TrustScan::skipped(),
            plugin_handle: distill_agent::plugins::SharedPluginRegistryHandle::new(
                None,
                Vec::new(),
            ),
            session_plugin_dirs: Vec::new(),
            reconciled: Some(true),
            plugins: PrefetchSlot::ready(plugin_registry),
        }
    }

    pub(crate) fn scan(&self) -> TrustScan {
        self.scan
    }

    /// Reconcile folder trust, then start plugin refresh with that verdict (not a launch-time snapshot).
    pub(crate) fn resolve_trust(
        &mut self,
        cwd: &Path,
        remote: Option<&crate::util::config::RemoteSettings>,
    ) -> bool {
        if let Some(verdict) = self.reconciled {
            return verdict;
        }
        let verdict = folder_trust::resolve_and_record_from_scan(cwd, remote, false, self.scan);
        self.reconciled = Some(verdict);
        self.start_plugins(verdict);
        verdict
    }

    fn start_plugins(&mut self, project_trusted: bool) {
        if self.plugins.is_started() {
            return;
        }
        #[cfg(test)]
        {
            REFRESH_STARTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut verdicts) = REFRESH_VERDICTS.lock() {
                verdicts.push(project_trusted);
            }
        }
        self.plugins = PrefetchSlot::spawn(refresh_plugins_for_cwd(
            self.plugin_handle.clone(),
            self.cwd.clone(),
            std::mem::take(&mut self.session_plugin_dirs),
            project_trusted,
        ));
    }

    #[cfg(test)]
    fn launch_for_test(
        scan: TrustScan,
        plugins: impl std::future::Future<Output = Option<Arc<distill_agent::plugins::PluginRegistry>>>
        + Send
        + 'static,
    ) -> Self {
        Self {
            cwd: PathBuf::new(),
            scan,
            plugin_handle: distill_agent::plugins::SharedPluginRegistryHandle::new(
                None,
                Vec::new(),
            ),
            session_plugin_dirs: Vec::new(),
            reconciled: Some(true),
            plugins: PrefetchSlot::spawn(plugins),
        }
    }

    #[cfg(test)]
    fn refresh_started(&self) -> bool {
        self.plugins.is_started()
    }

    pub(crate) async fn join_plugin_registry(
        &mut self,
    ) -> Option<Arc<distill_agent::plugins::PluginRegistry>> {
        let _timer = crate::instrumentation_timer!("session.spawn_and_register.plugin_refresh");
        if !self.plugins.is_started() {
            let verdict = self.reconciled.unwrap_or_else(|| {
                folder_trust::resolve_and_record_from_scan(&self.cwd, None, false, self.scan)
            });
            self.reconciled = Some(verdict);
            self.start_plugins(verdict);
        }
        self.plugins.join().await
    }
}

async fn refresh_plugins_for_cwd(
    handle: distill_agent::plugins::SharedPluginRegistryHandle,
    cwd: PathBuf,
    session_plugin_dirs: Vec<PathBuf>,
    project_trusted: bool,
) -> Option<Arc<distill_agent::plugins::PluginRegistry>> {
    #[cfg(test)]
    if CAPTURE_TRUST_ONLY.load(std::sync::atomic::Ordering::SeqCst) {
        return Some(Arc::new(distill_agent::plugins::PluginRegistry::empty()));
    }
    let disk_cfg = crate::config::resolve_effective_plugins_config(&cwd).to_discovery_config();
    match tokio::task::spawn_blocking(move || {
        handle.refresh_and_build_for_cwd(&cwd, &disk_cfg, &session_plugin_dirs, project_trusted)
    })
    .await
    {
        Ok(registry) => registry,
        Err(err) => {
            tracing::warn!(error = %err, "plugin refresh task failed");
            None
        }
    }
}

/// The value is only available after [`PrefetchSlot::join`].
struct PrefetchSlot<T> {
    pending: Option<AbortOnDropHandle<T>>,
    ready: Option<T>,
}

impl<T: Clone + Default + Send + 'static> PrefetchSlot<T> {
    fn empty() -> Self {
        Self {
            pending: None,
            ready: None,
        }
    }

    fn spawn(fut: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        Self {
            pending: Some(AbortOnDropHandle::new(tokio::spawn(fut))),
            ready: None,
        }
    }

    fn ready(value: T) -> Self {
        Self {
            pending: None,
            ready: Some(value),
        }
    }

    fn is_started(&self) -> bool {
        self.pending.is_some() || self.ready.is_some()
    }

    async fn join(&mut self) -> T {
        if let Some(value) = &self.ready {
            return value.clone();
        }
        let Some(handle) = self.pending.take() else {
            return T::default();
        };
        let value = match handle.await {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "session-create prefetch task failed");
                T::default()
            }
        };
        self.ready = Some(value.clone());
        value
    }
}

#[cfg(test)]
pub(crate) static REFRESH_STARTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static REFRESH_VERDICTS: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
static CAPTURE_TRUST_ONLY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
#[path = "session_create_prefetch_tests.rs"]
mod session_create_prefetch_tests;
