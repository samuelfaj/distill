// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared commit-scope checks for the settings and model loads.

use super::{ModelsCacheScope, SettingsCacheManager};
use distill_login::{AuthManager, GrokAuth, GrokComConfig};

/// Remote fetch enabled, no repair pending, and the live origin still matches
/// the one the value was fetched under.
fn origin_still_current(current_origin: &str, expected_origin: &str) -> bool {
    crate::util::config::resolve_remote_fetch_enabled()
        && !crate::managed_config::policy_repair_pending()
        && current_origin == expected_origin
}

/// The identity half of the commit scope, shared by the settings and models
/// commits: re-resolve disk auth through the load's config and compare, so a
/// like-for-like check detects real credential changes only. An empty
/// `expected_identity` (an unauthenticated fetch) stays current only while disk
/// auth is still absent.
fn identity_still_current(
    expected_identity: &str,
    auth_config: Option<&GrokComConfig>,
    alpha: Option<&str>,
) -> bool {
    match resolve_disk_auth(auth_config.cloned()) {
        Some(auth) => SettingsCacheManager::identity(&auth, alpha) == expected_identity,
        None => expected_identity.is_empty(),
    }
}

/// How far a successful live fetch may be applied. Exhaustive, so a caller
/// cannot silently forget a case.
pub(in crate::agent::remote_config) enum Commit {
    /// Origin, policy, and identity all current: write the cache and serve.
    CacheAndServe,
    /// Origin and policy current, identity not yet persisted to disk: serve in
    /// memory this boot without writing the cache.
    ServeInMemory,
    /// A policy repair is in flight: transient, so re-run a later warm rather
    /// than record a sticky failure.
    Retry,
    /// Origin or policy changed for good: abandon the fetch.
    Abandon,
}

/// Evaluate the commit scope for a successful fetch, shared by settings and
/// models. The caller resolves `current_origin` (settings: proxy URL; models:
/// catalog origin) and `alpha` with the same inputs it fetched with.
// TODO: newtype the origin (proxy vs catalog) and identity strings so a
// mismatched-origin comparison cannot compile.
pub(in crate::agent::remote_config) fn evaluate_commit(
    current_origin: &str,
    expected_origin: &str,
    expected_identity: &str,
    auth_config: Option<&GrokComConfig>,
    alpha: Option<&str>,
) -> Commit {
    if crate::managed_config::policy_repair_pending() {
        return Commit::Retry;
    }
    if !origin_still_current(current_origin, expected_origin) {
        return Commit::Abandon;
    }
    if identity_still_current(expected_identity, auth_config, alpha) {
        Commit::CacheAndServe
    } else {
        Commit::ServeInMemory
    }
}

/// Commit scope for a models fetch: compare the scope captured at fetch start to
/// a live re-resolve. Unlike the settings gate, the identity already folds in the
/// full scope (mode, credential, alpha), so an alpha flip or key rotation mid
/// fetch yields `ServeInMemory` rather than caching under the wrong scope.
pub(in crate::agent::remote_config) fn evaluate_models_commit(
    expected: &ModelsCacheScope,
    live: &ModelsCacheScope,
) -> Commit {
    if crate::managed_config::policy_repair_pending() {
        return Commit::Retry;
    }
    if !crate::util::config::resolve_remote_fetch_enabled() || live.origin != expected.origin {
        return Commit::Abandon;
    }
    if live.identity == expected.identity {
        Commit::CacheAndServe
    } else {
        Commit::ServeInMemory
    }
}

pub(in crate::agent::remote_config) fn resolve_disk_auth(
    grok_com_config: Option<GrokComConfig>,
) -> Option<GrokAuth> {
    let distill_home = crate::util::distill_home::distill_home();
    AuthManager::new_with_proxy_base_url(
        &distill_home,
        grok_com_config.unwrap_or_default(),
        crate::agent::config::EndpointsConfig::from_effective_config().proxy_url(),
    )
    .current()
}
