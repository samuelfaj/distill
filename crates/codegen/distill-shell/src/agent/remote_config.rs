//! Remote settings and model-catalog fetching, caching, and startup wiring.

mod cache;
mod cache_file;
mod endpoint;
mod fetch;
mod manager;
mod metrics;
mod model_fetch_auth;
mod prefetch;
mod resolution;
mod scope;
mod settings_cache;
pub mod settings_get;
mod settings_refresh;

pub(in crate::agent::remote_config) use cache::ModelsCacheManager;
pub(crate) use endpoint::{HttpModelsEndpoint, ModelsEndpoint};
pub(crate) use fetch::prefetch_models_blocking;
pub(in crate::agent::remote_config) use fetch::{ModelsPrefetch, fetch_models_uncommitted};
pub(crate) use manager::ModelsManager;
pub(crate) use metrics::{DegradedStartCause, record_degraded_start};
pub(in crate::agent::remote_config) use model_fetch_auth::ModelsCacheScope;
pub(crate) use model_fetch_auth::{CacheAuthMethod, ModelFetchAuth, task_model_error_for_catalog};
pub(in crate::agent::remote_config) use prefetch::resolve_startup_endpoints;
pub(crate) use prefetch::{
    ResolvedModels, fetch_initial_models_blocking, start_initial_models_load,
};
pub(crate) use resolution::{
    ModelGlobSet, allowlist_denied_message, allowlist_excludes_all_message,
    allowlist_matches_nothing, available_models, is_campaign_only_flip, resolve_catalog_key,
    resolve_default_model, resolve_model_catalog, selectable_catalog_key_for_persisted,
    validate_selectable,
};
pub(in crate::agent::remote_config) use scope::{
    Commit, evaluate_commit, evaluate_models_commit, resolve_disk_auth,
};
pub(crate) use settings_cache::SettingsCacheManager;
pub(in crate::agent::remote_config) use settings_cache::settings_cache_disabled;
pub(in crate::agent) use settings_refresh::SettingsRefresh;

// Re-exports reached only through the manager test module.
#[cfg(test)]
pub(crate) use cache::{CACHE_TTL, MODELS_CACHE_FILE, ModelsCache};
#[cfg(test)]
pub(crate) use endpoint::ModelsFetchFuture;
#[cfg(test)]
pub(crate) use fetch::build_prefetched_map;
#[cfg(test)]
pub(crate) use metrics::degraded_log_level;
#[cfg(test)]
pub(crate) use prefetch::resolve_prefetch_inputs_from_parts;
