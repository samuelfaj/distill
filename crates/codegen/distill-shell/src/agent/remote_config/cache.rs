// Modified for Distill by Samuel Fajreldines, 2026.
//! Unsigned on-disk model-catalog cache.

use chrono::{DateTime, Utc};
use indexmap::IndexMap;

use super::cache_file::{CacheLoadError, is_fresh, read_capped, write_atomic};
use super::{CacheAuthMethod, ModelsCacheScope};
use crate::agent::config::ModelEntry;

pub(crate) const MODELS_CACHE_FILE: &str = "models_cache.json";
pub(crate) const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Cap on the unsigned models cache read, mirroring the settings cache: a
/// corrupt or oversized file is a miss, not an unbounded read into memory.
const MODELS_CACHE_MAX_BYTES: u64 = 4 << 20;

/// Serializes every read-check-write of the cache file so a concurrent startup
/// commit and TTL renewal cannot both pass the monotonic check and let the
/// older fetch win.
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ModelsCache {
    /// Fetch-initiation time. The monotonic content key: an older fetch never
    /// replaces a newer stored catalog.
    pub(crate) fetched_at: DateTime<Utc>,
    /// TTL/freshness clock, bumped by a TTL renewal. Kept separate from
    /// `fetched_at` so renewing the TTL cannot shadow a newer-content write.
    /// Absent means freshness is measured from `fetched_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) renewed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) grok_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_method: Option<CacheAuthMethod>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
    /// Per-account-and-alpha scope, like the settings cache: a different user or
    /// alpha cohort must miss. A legacy `None` entry misses and refetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    pub(crate) models: IndexMap<String, ModelEntry>,
}

pub(in crate::agent::remote_config) struct CacheResult {
    pub(crate) models: IndexMap<String, ModelEntry>,
    pub(crate) etag: Option<String>,
}

pub(crate) struct ModelsCacheManager {
    pub(crate) path: std::path::PathBuf,
    pub(crate) ttl: std::time::Duration,
}

impl ModelsCacheManager {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::util::distill_home::distill_home().join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    pub(in crate::agent::remote_config) fn load_fresh(
        &self,
        scope: &ModelsCacheScope,
    ) -> Option<CacheResult> {
        match self.try_load_fresh(scope) {
            Ok(hit) => Some(hit),
            Err(status) => {
                status.log(&self.path);
                None
            }
        }
    }

    fn try_load_fresh(&self, scope: &ModelsCacheScope) -> Result<CacheResult, CacheLoadError> {
        let data =
            read_capped(&self.path, MODELS_CACHE_MAX_BYTES).ok_or(CacheLoadError::NotFound)?;
        let cache: ModelsCache =
            serde_json::from_slice(&data).map_err(|_| CacheLoadError::ParseFailed)?;
        if cache.grok_version.as_deref() != Some(distill_version::VERSION) {
            return Err(CacheLoadError::VersionMismatch);
        }
        if cache.auth_method.as_ref() != Some(&scope.auth_method) {
            return Err(CacheLoadError::ScopeMismatch("auth method"));
        }
        if cache.origin.as_deref() != Some(scope.origin.as_str()) {
            return Err(CacheLoadError::ScopeMismatch("origin"));
        }
        if cache.identity.as_deref() != Some(scope.identity.as_str()) {
            return Err(CacheLoadError::ScopeMismatch("identity"));
        }
        if !is_fresh(cache.renewed_at.unwrap_or(cache.fetched_at), self.ttl) {
            return Err(CacheLoadError::Stale);
        }
        tracing::debug!(count = cache.models.len(), "loaded models from disk cache");
        Ok(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    /// `fetched_at` is the fetch-initiation time. Monotonic: never replace a
    /// newer same-scope catalog with an older fetch, so a slow load cannot roll
    /// the cache back.
    pub(crate) fn persist(
        &self,
        models: &IndexMap<String, ModelEntry>,
        etag: Option<&str>,
        scope: &ModelsCacheScope,
        fetched_at: DateTime<Utc>,
    ) {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if self
            .disk_fetched_at(scope)
            .is_some_and(|disk| disk > fetched_at)
        {
            tracing::debug!("models cache commit skipped: a newer fetch is already stored");
            return;
        }
        let cache = ModelsCache {
            fetched_at,
            renewed_at: None,
            grok_version: Some(distill_version::VERSION.to_string()),
            auth_method: Some(scope.auth_method.clone()),
            origin: Some(scope.origin.clone()),
            identity: Some(scope.identity.clone()),
            etag: etag.map(|s| s.to_string()),
            models: models.clone(),
        };
        self.atomic_write(&cache);
    }

    /// The on-disk fetch time for a matching scope; `None` when absent,
    /// unparseable, a different scope, or future-dated (a corrupt file or clock
    /// rollback must not pin the cache).
    fn disk_fetched_at(&self, scope: &ModelsCacheScope) -> Option<DateTime<Utc>> {
        let data = read_capped(&self.path, MODELS_CACHE_MAX_BYTES)?;
        let cache: ModelsCache = serde_json::from_slice(&data).ok()?;
        if cache.auth_method.as_ref() != Some(&scope.auth_method)
            || cache.origin.as_deref() != Some(scope.origin.as_str())
            || cache.identity.as_deref() != Some(scope.identity.as_str())
        {
            return None;
        }
        (cache.fetched_at <= Utc::now()).then_some(cache.fetched_at)
    }

    /// Bump `renewed_at` forward when the on-disk catalog is unchanged, extending
    /// its TTL without touching the `fetched_at` content key. The read-modify-write
    /// holds `WRITE_LOCK` and re-reads inside it, so a concurrent in-process commit
    /// is never clobbered. `WRITE_LOCK` is process-local, so a cross-process commit
    /// can still race; that self-heals on the next etag-driven refresh.
    pub(crate) async fn renew_ttl(&self, scope: &ModelsCacheScope) {
        let path = self.path.clone();
        let ttl = self.ttl;
        let scope = scope.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let Some(data) = read_capped(&path, MODELS_CACHE_MAX_BYTES) else {
                return;
            };
            let Ok(mut cache) = serde_json::from_slice::<ModelsCache>(&data) else {
                return;
            };
            if cache.auth_method.as_ref() != Some(&scope.auth_method)
                || cache.origin.as_deref() != Some(scope.origin.as_str())
                || cache.identity.as_deref() != Some(scope.identity.as_str())
            {
                tracing::debug!("models cache TTL renewal skipped: scope mismatch");
                return;
            }
            cache.renewed_at = Some(Utc::now());
            ModelsCacheManager { path, ttl }.atomic_write(&cache);
            tracing::debug!("models cache TTL renewed");
        })
        .await;
    }

    pub(crate) fn invalidate(&self) {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!("models disk cache invalidated"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "failed to invalidate models disk cache"),
        }
    }

    pub(crate) fn atomic_write(&self, cache: &ModelsCache) {
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        write_atomic(&self.path, self.ttl, &json, false);
    }
}
