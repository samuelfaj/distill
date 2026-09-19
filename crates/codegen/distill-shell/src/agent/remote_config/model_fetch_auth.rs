// Modified for Distill by Samuel Fajreldines, 2026.
//! Auth resolution for `/v1/models` fetching.

use indexmap::IndexMap;

use super::SettingsCacheManager;
use crate::agent::auth_method::read_xai_api_key_env;
use crate::agent::config::{self, ModelEntry};
use crate::remote::{ModelSource, active_model_source};
use distill_login::{GrokAuth, GrokComConfig};

/// Credential for `/v1/models` fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFetchAuth {
    Session,
    ApiKey,
    Deployment,
    CustomEndpoint,
}

impl ModelFetchAuth {
    /// Precedence: custom_endpoint, then session, then deployment, then API key.
    pub(crate) fn resolve(endpoints: &config::EndpointsConfig, has_cached_session: bool) -> Self {
        if endpoints.has_custom_endpoint() {
            Self::CustomEndpoint
        } else if has_cached_session {
            Self::Session
        } else if endpoints.deployment_key.is_some() {
            Self::Deployment
        } else if crate::agent::auth_method::has_xai_api_key_env() {
            Self::ApiKey
        } else {
            Self::Session
        }
    }

    pub(in crate::agent::remote_config) fn cache_auth_method(&self) -> CacheAuthMethod {
        match self {
            Self::CustomEndpoint | Self::ApiKey => CacheAuthMethod::ApiKey,
            Self::Session => CacheAuthMethod::Session,
            Self::Deployment => CacheAuthMethod::Deployment,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheAuthMethod {
    Session,
    ApiKey,
    Deployment,
}

/// The full disk-cache scope for the model catalog, resolved atomically from the
/// effective fetch inputs so the load, the write, and the commit gate all agree.
/// `identity` captures the credential that actually fetches in each mode and
/// scope change rather than a silent cross-read.
#[derive(Clone, PartialEq, Eq)]
pub(in crate::agent::remote_config) struct ModelsCacheScope {
    pub(in crate::agent::remote_config) auth_method: CacheAuthMethod,
    pub(in crate::agent::remote_config) origin: String,
    pub(in crate::agent::remote_config) identity: String,
}

impl ModelsCacheScope {
    pub(in crate::agent::remote_config) fn resolve(
        endpoints: &config::EndpointsConfig,
        fetch_auth: ModelFetchAuth,
        auth: Option<&GrokAuth>,
    ) -> Self {
        let origin = active_model_source(endpoints, fetch_auth).cache_origin();
        let alpha = endpoints.alpha_test_key.as_deref();
        let identity = match fetch_auth {
            // Session identity matches the settings cache so a session boot keeps
            // hitting its existing entry; the empty fallback (no credential) still
            // misses safely.
            ModelFetchAuth::Session => auth
                .map(|a| SettingsCacheManager::identity(a, alpha))
                .unwrap_or_default(),
            ModelFetchAuth::ApiKey => {
                let key = read_xai_api_key_env().unwrap_or_default();
                scope_hash(&["models-api-key", key.as_str(), alpha.unwrap_or("")])
            }
            ModelFetchAuth::Deployment => scope_hash(&[
                "models-deployment",
                endpoints.deployment_key.as_deref().unwrap_or(""),
                alpha.unwrap_or(""),
            ]),
            // Custom endpoints authenticate with `XAI_API_KEY` or fall back to the session bearer (oai.rs).
            // A BYOK key is stable so it scopes two keys apart; the session bearer rotates, so key the
            // session case on the stable account identity (like the settings cache) to survive refresh.
            ModelFetchAuth::CustomEndpoint => match read_xai_api_key_env().ok() {
                Some(key) => scope_hash(&[
                    "models-custom-endpoint",
                    origin.as_str(),
                    key.as_str(),
                    alpha.unwrap_or(""),
                ]),
                None => match auth {
                    Some(a) => scope_hash(&[
                        "models-custom-endpoint-session",
                        origin.as_str(),
                        SettingsCacheManager::identity(a, alpha).as_str(),
                    ]),
                    None => scope_hash(&[
                        "models-custom-endpoint",
                        origin.as_str(),
                        "",
                        alpha.unwrap_or(""),
                    ]),
                },
            },
        };
        Self {
            auth_method: fetch_auth.cache_auth_method(),
            origin,
            identity,
        }
    }

    /// Re-resolve the scope for the commit gate under the fetch-time mode (so the
    /// origin reflects what was actually fetched), while reading LIVE disk auth
    /// for the identity. Re-deriving the mode from live auth would flip the origin
    /// (e.g. proxy to api.x.ai) in the just-logged-in / sign-out window and wrongly
    /// abandon a good catalog; the identity still owns real credential changes.
    pub(in crate::agent::remote_config) fn resolve_live(
        fetch_auth: ModelFetchAuth,
        commit_config: Option<&GrokComConfig>,
    ) -> Self {
        let endpoints = super::resolve_startup_endpoints();
        let auth = super::resolve_disk_auth(commit_config.cloned());
        Self::resolve(&endpoints, fetch_auth, auth.as_ref())
    }
}

/// Domain-separated SHA-256 over `parts`, mirroring `SettingsCacheManager::identity`:
/// each part gets a NUL terminator and the raw credential never lands on disk.
fn scope_hash(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn task_model_error_for_catalog(
    requested: &str,
    available: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    let is_available = |entry: &ModelEntry| {
        entry.info.user_selectable && entry.info.visible_for_auth(is_session_auth)
    };
    if config::find_model_by_id(available, requested).is_some_and(&is_available) {
        return None;
    }

    let mut slugs = available
        .iter()
        .filter(|(_, entry)| is_available(entry))
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    let guidance = if slugs.is_empty() {
        "No valid model slugs are currently available. Omit `model` to inherit the parent model."
            .to_string()
    } else {
        format!(
            "Valid model slugs: {}. Omit `model` to inherit the parent model.",
            slugs.join(", ")
        )
    };
    Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
}
