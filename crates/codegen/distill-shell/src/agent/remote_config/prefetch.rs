// Modified for Distill by Samuel Fajreldines, 2026.
//! Startup model-catalog prefetch and handoff.

use indexmap::IndexMap;

use super::{
    Commit, ModelFetchAuth, ModelsCacheScope, ModelsPrefetch, evaluate_models_commit,
    fetch_models_uncommitted, resolve_disk_auth,
};
use crate::agent::config::{self, ModelEntry};
use distill_login::{GrokAuth, GrokComConfig};

pub(crate) struct PrefetchInputs {
    pub(crate) auth: Option<GrokAuth>,
    pub(crate) endpoints: config::EndpointsConfig,
    pub(crate) model_fetch_auth: ModelFetchAuth,
}

/// Resolves startup endpoints from the effective config rather than env vars alone, so the prefetch cannot leak the bearer to api.x.ai.
pub(in crate::agent::remote_config) fn resolve_startup_endpoints() -> config::EndpointsConfig {
    let mut endpoints = config::EndpointsConfig::from_effective_config();
    if endpoints.deployment_key.is_none() {
        endpoints.deployment_key = crate::managed_config::resolve_deployment_key();
    }
    endpoints
}

pub(crate) fn resolve_prefetch_inputs_from_parts(
    auth: Option<GrokAuth>,
    endpoints: config::EndpointsConfig,
    remote_fetch_enabled: bool,
) -> Option<PrefetchInputs> {
    if !remote_fetch_enabled {
        tracing::info!("startup model/settings prefetch skipped: remote_fetch disabled");
        return None;
    }

    let model_fetch_auth = ModelFetchAuth::resolve(&endpoints, auth.is_some());

    if auth.is_none()
        && !endpoints.has_custom_endpoint()
        && model_fetch_auth == ModelFetchAuth::Session
    {
        return None;
    }

    Some(PrefetchInputs {
        auth,
        endpoints,
        model_fetch_auth,
    })
}

/// Joinable initial catalog prefetch. Runs on its own OS thread and reports
/// through a `oneshot`; a timeout or cancel drops the receiver while the thread
/// still lands its monotonic cache write for the next boot.
#[must_use]
pub(crate) struct InitialModelsLoad(
    tokio::sync::oneshot::Receiver<Option<IndexMap<String, ModelEntry>>>,
);

impl InitialModelsLoad {
    pub(crate) async fn join(
        self,
        cancel: &tokio_util::sync::CancellationToken,
        timeout: std::time::Duration,
    ) -> Option<IndexMap<String, ModelEntry>> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            received = tokio::time::timeout(timeout, self.0) => match received {
                Ok(Ok(models)) => models,
                Ok(Err(_sender_dropped)) => None,
                Err(_elapsed) => {
                    tracing::info!(
                        "initial models prefetch timed out; catalog freeze uses bundled defaults"
                    );
                    None
                }
            },
        }
    }
}

/// Catalog from the async pre-resolve. Outer `Option`: whether pre-resolve ran;
/// inner: the catalog, or `None` on a failed or skipped fetch. Carried by value
/// in `BootstrapPrefetch` from the boot's settings resolve to sync bootstrap.
pub(crate) type ResolvedModels = Option<IndexMap<String, ModelEntry>>;

/// Resolved inputs for a models prefetch, or `None` when none would run.
///
/// Never syncs managed config: the refresh supervisor owns that, so a live
/// server cannot heal a tampered policy ahead of the fail-closed gate.
/// `grok_com_config` scopes the disk-auth read; the default config sees only env.
fn models_prefetch_inputs(
    grok_com_config: Option<GrokComConfig>,
    warmed_auth: Option<GrokAuth>,
) -> Option<ModelsPrefetchPlan> {
    if crate::managed_config::policy_repair_pending() {
        return None;
    }
    let remote = crate::util::config::resolve_remote_fetch_enabled();
    // Prefer the live in-memory session so a just-refreshed or just-logged-in
    // credential drives the catalog fetch, not a stale or absent disk token.
    let auth = warmed_auth.or_else(|| resolve_disk_auth(grok_com_config.clone()));
    let endpoints = resolve_startup_endpoints();
    let env = resolve_prefetch_inputs_from_parts(auth.clone(), endpoints, remote)?;
    let expected = ModelsCacheScope::resolve(&env.endpoints, env.model_fetch_auth, auth.as_ref());
    Some(ModelsPrefetchPlan {
        env,
        expected,
        commit_config: grok_com_config,
    })
}

struct ModelsPrefetchPlan {
    env: PrefetchInputs,
    expected: ModelsCacheScope,
    commit_config: Option<GrokComConfig>,
}

/// Fetch the catalog and commit it under the policy/identity gate. The commit is
/// monotonic, so a late abandoned run cannot roll the cache back.
fn run_models_prefetch(
    plan: ModelsPrefetchPlan,
    cancel: &tokio_util::sync::CancellationToken,
) -> Option<IndexMap<String, ModelEntry>> {
    if cancel.is_cancelled() {
        return None;
    }
    let ModelsPrefetchPlan {
        env,
        expected,
        commit_config,
    } = plan;
    match fetch_models_uncommitted(
        &env.endpoints,
        env.auth.as_ref(),
        env.model_fetch_auth,
        true,
    ) {
        ModelsPrefetch::Cached(models) => Some(models),
        ModelsPrefetch::Fetched(write) => {
            // Re-resolve the live scope under the fetch-time mode (stable origin) but with live disk
            // auth for identity, so an alpha flip, key rotation, or account switch is caught without
            // abandoning the catalog when disk auth is briefly absent.
            let live = ModelsCacheScope::resolve_live(env.model_fetch_auth, commit_config.as_ref());
            match evaluate_models_commit(&expected, &live) {
                Commit::CacheAndServe => Some(write.commit()),
                Commit::ServeInMemory => {
                    tracing::info!(
                        "models fetch served in memory; not cached until the session persists"
                    );
                    Some(write.into_models())
                }
                Commit::Retry | Commit::Abandon => {
                    tracing::info!("models load discarded fetch: policy or origin changed");
                    None
                }
            }
        }
        ModelsPrefetch::Unavailable => None,
    }
}

/// Run one catalog prefetch on its own OS thread, delivering the result through
/// `deliver`. An own thread, not `spawn_blocking`: the fetch creates and drops
/// its own runtime, which panics inside a tokio context.
fn spawn_prefetch_thread(
    name: &str,
    plan: ModelsPrefetchPlan,
    cancel: tokio_util::sync::CancellationToken,
    deliver: impl FnOnce(Option<IndexMap<String, ModelEntry>>) + Send + 'static,
) -> Option<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || deliver(run_models_prefetch(plan, &cancel)))
        .ok()
        .map(|_| ())
}

pub(crate) fn start_initial_models_load(
    cancel: tokio_util::sync::CancellationToken,
    grok_com_config: Option<GrokComConfig>,
    warmed_auth: Option<GrokAuth>,
) -> Option<InitialModelsLoad> {
    let plan = models_prefetch_inputs(grok_com_config, warmed_auth)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn_prefetch_thread("grok-models-prefetch", plan, cancel, move |models| {
        let _ = tx.send(models);
    })?;
    Some(InitialModelsLoad(rx))
}

/// Poll slice bounding the sync models wait so a bootstrap cancel is observed promptly.
const MODELS_WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Synchronous catalog prefetch for a bootstrap that had no async pre-resolve.
/// A cancel or the fetch timeout ends the wait; the thread still lands its
/// monotonic commit for the next boot.
pub(crate) fn fetch_initial_models_blocking(
    cancel: &tokio_util::sync::CancellationToken,
    grok_com_config: Option<GrokComConfig>,
    warmed_auth: Option<GrokAuth>,
) -> Option<IndexMap<String, ModelEntry>> {
    let plan = models_prefetch_inputs(grok_com_config, warmed_auth)?;
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_prefetch_thread(
        "grok-models-prefetch-sync",
        plan,
        cancel.clone(),
        move |models| {
            let _ = tx.send(models);
        },
    )?;
    let started = std::time::Instant::now();
    loop {
        if cancel.is_cancelled() {
            return None;
        }
        let remaining = crate::http::STARTUP_FETCH_TIMEOUT.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            tracing::info!(
                "initial models prefetch timed out; catalog freeze uses bundled defaults"
            );
            return None;
        }
        match rx.recv_timeout(remaining.min(MODELS_WAIT_POLL_INTERVAL)) {
            Ok(models) => return models,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}
