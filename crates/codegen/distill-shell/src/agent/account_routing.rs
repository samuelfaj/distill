//! Per-session account routing. Credential storage belongs to `accounts`.
use super::config::{
    ModelEntry, ResolvedCredentials, find_model_by_id, sampling_config_for_account,
};
use crate::accounts::{
    AccountStore, AutoOption, AutoStrategy, Provider, QuotaSnapshot, QuotaStatus,
};
use anyhow::{Result, anyhow, ensure};
use chrono::{Duration, Utc};
use distill_sampler::SamplerConfig;
use distill_sampling_types::ConversationItem;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub(crate) const ROUTE_HEADER: &str = "x-distill-account-route";
pub(crate) const PIN_HEADER: &str = "x-distill-model-pin";
pub(crate) const CREDENTIAL_AUTHORITY_HEADER: &str = "x-distill-credential-authority";

/// The option's account always owns Main. A worker may authenticate independently.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerCredential {
    #[default]
    MainAccount,
    Model,
    Account(String),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AccountRoute {
    pub account_id: String,
    pub option_id: Option<String>,
    pub main_model: String,
    pub worker_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_fallback_model: Option<String>,
    #[serde(default)]
    pub using_worker_fallback: bool,
    #[serde(default)]
    pub worker_credential: WorkerCredential,
    pub worker: bool,
    /// An explicit model keeps its model authority even with global Auto enabled.
    pub pinned: bool,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub failed_options: Vec<String>,
}
impl AccountRoute {
    pub(crate) fn read(headers: &IndexMap<String, String>) -> Result<Option<Self>> {
        headers
            .get(ROUTE_HEADER)
            .map(|value| serde_json::from_str(value).map_err(Into::into))
            .transpose()
    }
    pub(crate) fn write(&self, config: &mut SamplerConfig) -> Result<()> {
        config
            .extra_headers
            .insert(ROUTE_HEADER.into(), serde_json::to_string(self)?);
        Ok(())
    }
    pub(crate) fn credential_account(&self) -> Option<&str> {
        match (&self.worker_credential, self.worker) {
            (WorkerCredential::Model, true) => None,
            (WorkerCredential::Account(id), true) => Some(id),
            _ => Some(&self.account_id),
        }
    }
    fn same_credential(&self, other: &Self) -> bool {
        match (self.credential_account(), other.credential_account()) {
            (None, None) => self.model() == other.model(),
            (left, right) => left == right,
        }
    }
    pub(crate) fn model(&self) -> &str {
        if self.worker && self.using_worker_fallback {
            self.worker_fallback_model
                .as_deref()
                .unwrap_or(&self.worker_model)
        } else if self.worker {
            &self.worker_model
        } else {
            &self.main_model
        }
    }
}

pub(crate) fn account_history_needs_portability(
    previous: &distill_sampling_types::SamplingConfig,
    next: &SamplerConfig,
) -> bool {
    previous.model != next.model
        || previous.base_url != next.base_url
        || previous.api_backend != next.api_backend
        // A re-signed account keeps its id; its reasoning is not portable.
        || previous.extra_headers.iter().any(|(name, value)| {
            is_identity_anchor(name)
                && next.extra_headers.get(name).is_some_and(|next| next != value)
        })
        || match (
            AccountRoute::read(&previous.extra_headers).ok().flatten(),
            AccountRoute::read(&next.extra_headers).ok().flatten(),
        ) {
            (Some(old), Some(new)) => !old.same_credential(&new),
            (None, None) => false,
            _ => true,
        }
}

/// Remove model-minted reasoning and retain backend tool context as text.
pub(crate) fn make_account_history_portable(items: &mut Vec<ConversationItem>) -> bool {
    if !items.iter().any(|item| {
        matches!(
            item,
            ConversationItem::Reasoning(_) | ConversationItem::BackendToolCall(_)
        )
    }) {
        return false;
    }
    *items = distill_chat_state::compaction_utils::strip_reasoning_blocks(std::mem::take(items))
        .into_iter()
        .map(|item| match item {
            ConversationItem::BackendToolCall(call) => {
                ConversationItem::assistant(call.text_summary())
            }
            other => other,
        })
        .collect();
    true
}

pub(crate) fn provider_for_url(url: &str) -> Option<Provider> {
    if url.trim().is_empty() {
        return None;
    }
    if crate::codex_auth::is_trusted_inference_base_url(url) {
        Some(Provider::Chatgpt)
    } else if crate::claude_auth::is_claude_backend(url) {
        Some(Provider::Claude)
    } else if crate::openrouter_auth::is_openrouter_url(url) {
        Some(Provider::Openrouter)
    } else if crate::util::is_trusted_cli_chat_proxy_url(url) {
        Some(Provider::Grok)
    } else {
        None
    }
}

/// A stale, missing or incomparable quota is unknown, never a fabricated zero.
fn known_remaining(snapshot: Option<&QuotaSnapshot>) -> Option<f64> {
    let snapshot = snapshot?;
    let fetched = snapshot.fetched_at?;
    let age = Utc::now().signed_duration_since(fetched);
    if snapshot.status != QuotaStatus::Available
        || age < Duration::zero()
        || age > Duration::minutes(5)
    {
        return None;
    }
    snapshot
        .remaining_percent
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

pub(crate) fn rank_routes(
    mut routes: Vec<(AccountRoute, Option<QuotaSnapshot>)>,
    strategy: AutoStrategy,
) -> Vec<AccountRoute> {
    routes.retain(|(_, snapshot)| known_remaining(snapshot.as_ref()) != Some(0.0));
    // Stable order breaks ties and preserves configured order among unknown options.
    if strategy == AutoStrategy::HighestRemaining {
        routes.sort_by(|(_, a), (_, b)| {
            match (known_remaining(a.as_ref()), known_remaining(b.as_ref())) {
                (Some(a), Some(b)) if strategy == AutoStrategy::HighestRemaining => b.total_cmp(&a),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            }
        });
    }
    routes.into_iter().map(|(route, _)| route).collect()
}

pub(crate) async fn routes_for_turn(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    current: &SamplerConfig,
    auto_allowed: bool,
) -> Result<Vec<AccountRoute>> {
    let routing = store.load_routing()?;
    let previous = AccountRoute::read(&current.extra_headers)?;
    let pinned = !auto_allowed
        || current.extra_headers.contains_key(PIN_HEADER)
        || previous.as_ref().is_some_and(|route| route.pinned);
    let worker = previous.as_ref().is_some_and(|route| route.worker);
    let mut candidates = Vec::new();
    if routing.enabled && !pinned {
        for option in &routing.options {
            if previous
                .as_ref()
                .is_some_and(|route| route.failed_options.contains(&option.id))
            {
                continue;
            }
            let account = store.get(&option.account_id)?;
            if !store
                .status(&account.id, grok_for_provider(account.provider)?.as_ref())?
                .connected
            {
                continue;
            }
            let main = find_model_by_id(models, &option.main_model).ok_or_else(|| {
                anyhow!(
                    "Auto option `{}` references unavailable main model",
                    option.id
                )
            })?;
            ensure!(
                main.info.user_selectable
                    && provider_for_url(&main.info.base_url) == Some(account.provider),
                "Auto option `{}` has a provider/model mismatch",
                option.id
            );
            let mut route = route_for_option(option, worker);
            if let Some(previous) = previous.as_ref() {
                route.operation_id = previous.operation_id.clone();
                route.failed_options = previous.failed_options.clone();
            }
            candidates.push((route, store.quota_snapshot(&account.id)?));
        }
        refresh_stale_quotas(store, &mut candidates).await;
        apply_model_quotas(&mut candidates, models);
        let mut routes = rank_routes(candidates, routing.strategy);
        // A delegated worker begins on the pair selected by its parent, then falls back in configured order.
        if worker
            && let Some(previous) = &previous
            && let Some(index) = routes.iter().position(|route| {
                route.option_id == previous.option_id && route.account_id == previous.account_id
            })
        {
            let selected = routes.remove(index);
            routes.insert(0, selected);
        }
        if worker {
            let mut attempts = Vec::new();
            for route in routes {
                let resume_fallback = previous.as_ref().is_some_and(|old| {
                    old.using_worker_fallback
                        && old.option_id == route.option_id
                        && old.account_id == route.account_id
                        && old.worker_model == route.worker_model
                        && old.worker_fallback_model == route.worker_fallback_model
                });
                if !resume_fallback {
                    attempts.push(route.clone());
                }
                if route.worker_fallback_model.is_some() {
                    attempts.push(AccountRoute {
                        using_worker_fallback: true,
                        ..route
                    });
                }
            }
            return Ok(attempts);
        }
        return Ok(routes);
    }
    // An explicitly inherited Auto worker keeps its credential authority.
    if let Some(mut route) = previous
        .clone()
        .filter(|route| route.worker && route.option_id.is_some() && pinned)
    {
        route.pinned = true;
        return Ok(vec![route]);
    }
    // The selected entry stamps credential authority before its catalog id is
    // reduced to the wire slug, which may also name a subscription entry.
    if previous.is_none()
        && (current
            .extra_headers
            .contains_key(CREDENTIAL_AUTHORITY_HEADER)
            || (find_model_by_id(models, &current.model).is_none()
                && (current.api_key.is_some() || current.bearer_resolver.is_some())))
    {
        return Ok(Vec::new());
    }
    let Some(provider) = provider_for_url(&current.base_url) else {
        return Ok(Vec::new());
    };
    let mut connected = false;
    for account in store.list()? {
        if account.provider != provider
            || !store
                .status(&account.id, grok_for_provider(account.provider)?.as_ref())?
                .connected
        {
            continue;
        }
        connected = true;
        candidates.push((
            AccountRoute {
                account_id: account.id.clone(),
                option_id: None,
                main_model: current.model.clone(),
                worker_model: current.model.clone(),
                worker,
                pinned,
                operation_id: None,
                worker_fallback_model: None,
                using_worker_fallback: false,
                worker_credential: Default::default(),
                failed_options: Vec::new(),
            },
            store.quota_snapshot(&account.id)?,
        ));
    }
    refresh_stale_quotas(store, &mut candidates).await;
    apply_model_quotas(&mut candidates, models);
    let ranked = rank_routes(candidates, AutoStrategy::HighestRemaining);
    ensure!(
        !connected || !ranked.is_empty(),
        "All accounts for this provider have exhausted quota"
    );
    Ok(ranked.into_iter().take(1).collect())
}

/// Resolve account credentials before session admission/auxiliary clients are constructed.
pub(crate) async fn select_startup_config(
    current: &SamplerConfig,
    models: &IndexMap<String, ModelEntry>,
    pinned: bool,
) -> Result<SamplerConfig> {
    let store = AccountStore::default();
    let mut current = current.clone();
    if pinned {
        current.extra_headers.insert(PIN_HEADER.into(), "1".into());
    }
    let routes = routes_for_turn(&store, models, &current, !pinned).await?;
    if !routes.is_empty() {
        return config_for_routes(&store, models, &current, &routes).await;
    }
    ensure!(
        !store.load_routing()?.enabled || pinned,
        "All account routing options are exhausted or unavailable"
    );
    Ok(current)
}

/// `auto` is a logical choice and is never sent as a provider model slug.
pub(crate) async fn explicit_auto_config(
    current: &SamplerConfig,
    models: &IndexMap<String, ModelEntry>,
) -> Result<SamplerConfig> {
    ensure!(
        AccountStore::default().load_routing()?.enabled,
        "Auto routing is not enabled"
    );
    let mut current = current.clone();
    current.extra_headers.shift_remove(PIN_HEADER);
    current.extra_headers.shift_remove(ROUTE_HEADER);
    current.extra_headers.shift_remove("x-distill-worker-pin");
    select_startup_config(&current, models, false).await
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedAccountRoute {
    route: AccountRoute,
    identity_anchors: IndexMap<String, String>,
    worker_pinned: bool,
    reasoning_effort: Option<distill_sampling_types::ReasoningEffort>,
}

fn is_identity_anchor(name: &str) -> bool {
    matches!(
        name,
        "x-distill-codex-auth-anchor"
            | "x-distill-codex-account-anchor"
            | "x-distill-codex-user-anchor"
            | "x-distill-codex-workspace-anchor"
            | "x-distill-claude-auth-anchor"
            | "x-distill-claude-account-anchor"
            | "x-distill-grok-auth-anchor"
            | "x-distill-openrouter-key-anchor"
    )
}

pub(crate) fn save_session_config(
    info: &crate::session::info::Info,
    config: &SamplerConfig,
) -> Result<()> {
    use std::io::Write;
    let dir = crate::session::persistence::ensure_owner_only_session_dir(info)?;
    let durable = AccountRoute::read(&config.extra_headers)?.map(|route| PersistedAccountRoute {
        route,
        identity_anchors: config
            .extra_headers
            .iter()
            .filter(|(name, _)| is_identity_anchor(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        worker_pinned: config.extra_headers.contains_key("x-distill-worker-pin"),
        reasoning_effort: config.reasoning_effort,
    });
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    crate::util::secure_file::ensure_owner_only_permissions(file.path())?;
    serde_json::to_writer(file.as_file_mut(), &durable)?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(dir.join("account-route.json"))?;
    Ok(())
}

/// The session's saved route when enabled Auto chose it (not an explicit model pin).
pub(crate) fn session_auto_route(info: &crate::session::info::Info) -> Option<AccountRoute> {
    if !AccountStore::default()
        .load_routing()
        .is_ok_and(|routing| routing.enabled)
    {
        return None;
    }
    read_session_route(info)
        .ok()
        .flatten()
        .map(|saved| saved.route)
        .filter(|route| route.option_id.is_some() && !route.pinned)
}

fn read_session_route(info: &crate::session::info::Info) -> Result<Option<PersistedAccountRoute>> {
    let dir = crate::session::persistence::ensure_owner_only_session_dir(info)?;
    let bytes = match std::fs::read(dir.join("account-route.json")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(saved) = serde_json::from_slice::<Option<PersistedAccountRoute>>(&bytes)? else {
        return Ok(None);
    };
    ensure!(
        saved
            .identity_anchors
            .keys()
            .all(|name| is_identity_anchor(name)),
        "Invalid persisted identity anchor"
    );
    Ok(Some(saved))
}

/// Disk history and route metadata are separate writes. Account admissions cannot
/// prove that opaque records came from the saved route, even if its id matches.
/// Reading this authority never authenticates or refreshes the source account.
pub(crate) fn account_history_admission_needs_portability(
    source: &crate::session::info::Info,
    next: &SamplerConfig,
) -> bool {
    next.extra_headers.contains_key(ROUTE_HEADER)
        || !read_session_route(source).is_ok_and(|route| route.is_none())
}

pub(crate) async fn load_session_config(
    info: &crate::session::info::Info,
    models: &IndexMap<String, ModelEntry>,
) -> Result<Option<SamplerConfig>> {
    let Some(saved) = read_session_route(info)? else {
        return Ok(None);
    };
    let store = AccountStore::default();
    // The xAI proxy rejects a request without `x-grok-client-version` with
    // 426 "Grok CLI version (none) is outdated", so a restored route keeps it.
    let previous = SamplerConfig {
        reasoning_effort: saved.reasoning_effort,
        client_version: Some(distill_version::VERSION.to_string()),
        ..Default::default()
    };
    let mut config = model_config_for_route(&store, models, &previous, &saved.route)?;
    config.extra_headers.extend(saved.identity_anchors);
    if saved.worker_pinned {
        config
            .extra_headers
            .insert("x-distill-worker-pin".into(), "1".into());
    }
    if let Some(effort) = saved.reasoning_effort {
        let model = find_model_by_id(models, saved.route.model()).expect("validated route");
        if model
            .info
            .reasoning_efforts
            .iter()
            .any(|value| value.value == effort)
        {
            config.reasoning_effort = Some(effort);
        }
    }
    restore_credentials(&store, models, &mut config).await?;
    Ok(Some(config))
}

/// The saved route for a resumed session. An unpinned Auto route that no longer
/// restores is chosen again from the Auto options, as each turn already does;
/// an explicit account still fails closed.
pub(crate) async fn restore_session_config(
    info: &crate::session::info::Info,
    current: &SamplerConfig,
    models: &IndexMap<String, ModelEntry>,
) -> Result<Option<SamplerConfig>> {
    match load_session_config(info, models).await {
        Err(error) => {
            let Some(route) = session_auto_route(info) else {
                return Err(error);
            };
            tracing::warn!(%error, "Saved Auto route is unavailable; selecting Auto again");
            let mut current = current.clone();
            if route.worker {
                route.write(&mut current)?;
            }
            select_startup_config(&current, models, false)
                .await
                .map(Some)
        }
        result => result,
    }
}

fn route_for_option(option: &AutoOption, worker: bool) -> AccountRoute {
    AccountRoute {
        account_id: option.account_id.clone(),
        option_id: Some(option.id.clone()),
        main_model: option.main_model.clone(),
        worker_model: option.worker_model().to_owned(),
        worker,
        pinned: false,
        operation_id: None,
        worker_fallback_model: option.worker_fallback_model.clone(),
        using_worker_fallback: false,
        worker_credential: Default::default(),
        failed_options: Vec::new(),
    }
}

/// Start a delegate on its parent's option, including credential-admission fallback.
pub(crate) async fn select_worker_config(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    parent: &SamplerConfig,
    mut route: AccountRoute,
) -> Result<(SamplerConfig, String)> {
    route.worker = true;
    let mut current = parent.clone();
    route.write(&mut current)?;
    let routes = routes_for_turn(store, models, &current, true).await?;
    ensure!(
        routes
            .first()
            .is_some_and(|r| r.option_id.is_some() && r.worker),
        "Auto worker route unavailable"
    );
    let config = config_for_routes(store, models, parent, &routes).await?;
    let selected = AccountRoute::read(&config.extra_headers)?.expect("configured route");
    Ok((config, selected.model().to_owned()))
}

/// Select usable credentials in ranked order, advancing only through Auto options.
pub(crate) async fn config_for_routes(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    previous: &SamplerConfig,
    routes: &[AccountRoute],
) -> Result<SamplerConfig> {
    let mut last_error = anyhow!("All account routing options are exhausted or unavailable");
    for route in routes {
        match config_for_route(store, models, previous, route).await {
            Ok(config) => return Ok(config),
            // Auto can advance to its next option; explicit selections fail closed.
            Err(error) if route.option_id.is_some() && !route.pinned => last_error = error,
            Err(error) => return Err(error),
        }
    }
    Err(last_error)
}

/// Build all model parameters afresh; never carry a previous provider's secrets or headers.
pub(crate) async fn config_for_route(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    previous: &SamplerConfig,
    route: &AccountRoute,
) -> Result<SamplerConfig> {
    let mut route = route.clone();
    if route.worker && route.option_id.is_some() && !route.pinned {
        let model = find_model_by_id(models, route.model())
            .filter(|model| model.info.user_selectable)
            .ok_or_else(|| anyhow!("Worker model is unavailable"))?;
        let provider = provider_for_url(&model.info.base_url);
        let bound = store.get(&route.account_id)?;
        route.worker_credential = if route.model() != route.main_model
            && (model.has_configured_auth() || model.env_key.is_some() || provider.is_none())
        {
            WorkerCredential::Model
        } else if provider == Some(bound.provider) {
            WorkerCredential::MainAccount
        } else {
            let grok =
                grok_for_provider(provider.ok_or_else(|| anyhow!("Unknown worker provider"))?)?;
            let account = store
                .list()?
                .into_iter()
                .find(|account| {
                    Some(account.provider) == provider
                        && store
                            .status(&account.id, grok.as_ref())
                            .is_ok_and(|status| {
                                status.connected
                                    && known_remaining(status.quota.as_ref()) != Some(0.0)
                            })
                })
                .ok_or_else(|| anyhow!("No connected account for the worker provider"))?;
            WorkerCredential::Account(account.id)
        };
    }
    let mut config = model_config_for_route(store, models, previous, &route)?;
    if previous.base_url == config.base_url
        && AccountRoute::read(&previous.extra_headers)?
            .is_some_and(|old| old.same_credential(&route))
    {
        config.extra_headers.extend(
            previous
                .extra_headers
                .iter()
                .filter(|(name, _)| is_identity_anchor(name))
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    restore_credentials(store, models, &mut config).await?;
    Ok(config)
}

fn model_config_for_route(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    previous: &SamplerConfig,
    route: &AccountRoute,
) -> Result<SamplerConfig> {
    let model = find_model_by_id(models, route.model())
        .ok_or_else(|| anyhow!("Selected account model is unavailable"))?;
    if let Some(id) = route.credential_account() {
        ensure!(
            provider_for_url(&model.info.base_url) == Some(store.get(id)?.provider),
            "Selected account provider/model mismatch"
        );
    }
    let mut config = sampling_config_for_account(
        model,
        ResolvedCredentials {
            api_key: None,
            base_url: model.info.base_url.clone(),
            auth_type: distill_chat_state::AuthType::ApiKey,
            auth_scheme: model.info.auth_scheme,
        },
        None,
        previous.client_version.clone(),
        previous.deployment_id.clone(),
        None,
    );
    // Model headers must not supply a second credential path or a stale identity.
    config
        .extra_headers
        .retain(|name, _| !credential_header(name));
    config.env_http_headers.clear();
    config.api_key = None;
    config.bearer_resolver = None;
    config.attribution_callback = None;
    config.origin_client = previous.origin_client.clone();
    config.client_identifier = previous.client_identifier.clone();
    config.conversation_group_id = previous.conversation_group_id.clone();
    config.header_injector = previous.header_injector.clone();
    config.idle_timeout_secs = previous.idle_timeout_secs;
    config.doom_loop_recovery = previous.doom_loop_recovery;
    // A pinned mode's serving path (its tier and the OpenRouter variant it defaulted) stays
    // with the session model it was decided for.
    if previous.model == config.model
        && previous.base_url == config.base_url
        && previous.provider_tier.is_some()
    {
        config.provider_tier = previous.provider_tier;
        config.openrouter_variant = previous.openrouter_variant.clone();
    }
    if previous.model == config.model
        && let Some(effort) = previous.reasoning_effort
        && model
            .info
            .reasoning_efforts
            .iter()
            .any(|option| option.value == effort)
    {
        config.reasoning_effort = Some(effort);
    }
    if previous.extra_headers.contains_key("x-distill-worker-pin") {
        config
            .extra_headers
            .insert("x-distill-worker-pin".into(), "1".into());
    }
    route.write(&mut config)?;
    if route.option_id.is_some() && !route.pinned {
        config.max_retries = Some(0);
        config.rate_limit_retry_threshold = Some(1);
    }
    Ok(config)
}

fn apply_model_quotas(
    candidates: &mut [(AccountRoute, Option<QuotaSnapshot>)],
    models: &IndexMap<String, ModelEntry>,
) {
    for (route, snapshot) in candidates {
        let Some(snapshot) = snapshot else {
            continue;
        };
        let Some(detail) = snapshot.detail.as_ref() else {
            continue;
        };
        let Ok(usage) =
            serde_json::from_value::<crate::claude_auth::ClaudeUsageSnapshot>(detail.clone())
        else {
            continue;
        };
        // A separate worker provider has a separate quota. With a fallback, a
        // depleted primary worker must not eliminate the whole option.
        let main = find_model_by_id(models, &route.main_model);
        let worker = find_model_by_id(models, &route.worker_model).filter(|worker| {
            route.worker_fallback_model.is_none()
                && !worker.has_configured_auth()
                && main.is_some_and(|main| {
                    provider_for_url(&main.info.base_url) == provider_for_url(&worker.info.base_url)
                })
        });
        for model in main.into_iter().chain(worker) {
            let window = if model.info.model.contains("claude-opus") {
                usage.seven_day_opus.as_ref()
            } else if model.info.model.contains("claude-sonnet") {
                usage.seven_day_sonnet.as_ref()
            } else {
                None
            };
            if let Some(used) = window
                .and_then(|window| window.utilization)
                .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
            {
                let remaining = 100.0 - used;
                snapshot.remaining_percent = Some(
                    snapshot
                        .remaining_percent
                        .map_or(remaining, |global| global.min(remaining)),
                );
            }
        }
    }
}

fn grok_for_provider(provider: Provider) -> Result<Option<crate::accounts::GrokLoginConfig>> {
    if provider == Provider::Grok {
        grok_config().map(Some)
    } else {
        Ok(None)
    }
}

/// One bounded refresh per stale account at selection time, never per stream chunk.
async fn refresh_stale_quotas(
    store: &AccountStore,
    candidates: &mut [(AccountRoute, Option<QuotaSnapshot>)],
) {
    let unique: std::collections::HashSet<_> = candidates
        .iter()
        .map(|(route, _)| route.account_id.clone())
        .collect();
    if unique.len() < 2 {
        return;
    }
    let mut refreshing = std::collections::HashSet::new();
    let ids: Vec<_> = candidates
        .iter()
        .filter_map(|(route, snapshot)| {
            let fresh = snapshot.as_ref().is_some_and(|snapshot| {
                let age = Utc::now().signed_duration_since(snapshot.checked_at);
                age >= Duration::zero() && age <= Duration::minutes(5)
            });
            (!fresh && refreshing.insert(route.account_id.clone()))
                .then_some(route.account_id.clone())
        })
        .collect();
    let refreshes = ids.iter().map(|id| async {
        if let Ok(account) = store.get(id)
            && let Ok(grok) = grok_for_provider(account.provider)
        {
            let _ = store.refresh_usage(id, grok).await;
        }
    });
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        futures::future::join_all(refreshes),
    )
    .await;
    for (route, snapshot) in candidates {
        if let Ok(latest) = store.quota_snapshot(&route.account_id) {
            *snapshot = latest;
        }
    }
}

fn grok_config() -> Result<crate::accounts::GrokLoginConfig> {
    let raw = crate::config::load_effective_config()?;
    let config = raw
        .get("grok_com_config")
        .or_else(|| raw.get("auth"))
        .map(|config| config.clone().try_into())
        .transpose()?
        .unwrap_or_default();
    let proxy_base_url = super::config::EndpointsConfig::from_config_value(&raw).proxy_url();
    Ok(crate::accounts::GrokLoginConfig {
        config,
        device_flow: None,
        proxy_base_url,
    })
}

fn credential_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "authorization"
            | "x-api-key"
            | CREDENTIAL_AUTHORITY_HEADER
            | "chatgpt-account-id"
            | "x-openai-fedramp"
            | "x-xai-token-auth"
    ) || name.starts_with("x-distill-codex-")
        || name.starts_with("x-distill-claude-")
        || name.starts_with("x-distill-grok-")
        || name.starts_with("x-distill-openrouter-")
}

pub(crate) async fn restore_credentials(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    config: &mut SamplerConfig,
) -> Result<()> {
    let Some(route) = AccountRoute::read(&config.extra_headers)? else {
        return Ok(());
    };
    let Some(account_id) = route.credential_account() else {
        let model = find_model_by_id(models, route.model())
            .filter(|model| {
                model.info.base_url == config.base_url && model.info.model == config.model
            })
            .ok_or_else(|| anyhow!("Worker model or endpoint changed"))?;
        // Rehydrate only this catalog entry's credentials, including on a live
        // reconstruction where the chat state supplies no trusted bearer.
        let auth_header = |name: &str| {
            name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key")
        };
        config
            .extra_headers
            .retain(|name, _| !credential_header(name));
        config.extra_headers.extend(
            model
                .info
                .extra_headers
                .iter()
                .filter(|(name, _)| auth_header(name))
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        config.env_http_headers = model.info.env_http_headers.clone();
        config
            .env_http_headers
            .retain(|name, _| !credential_header(name) || auth_header(name));
        config.bearer_resolver = None;
        config.attribution_callback = None;
        config.auth_scheme = model.info.auth_scheme;
        config.api_key = match model.effective_auth_provider() {
            Some(provider) => provider.ensure_fresh_token(None).await.rotated(),
            None => super::config::first_own_credential(
                model.api_key.as_deref(),
                model.env_key.as_ref(),
            ),
        };
        ensure!(
            config.api_key.is_some()
                || config
                    .extra_headers
                    .iter()
                    .any(|(name, value)| auth_header(name) && !value.trim().is_empty())
                || config
                    .env_http_headers
                    .iter()
                    .any(|(name, var)| auth_header(name)
                        && std::env::var(var).is_ok_and(|v| !v.trim().is_empty())),
            "Worker model has no usable credential"
        );
        return Ok(());
    };
    let anchors: Vec<_> = config
        .extra_headers
        .iter()
        .filter(|(name, _)| {
            name.starts_with("x-distill-codex-")
                || name.starts_with("x-distill-claude-")
                || name.starts_with("x-distill-grok-")
                || name.starts_with("x-distill-openrouter-")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let grok = grok_for_provider(store.get(account_id)?.provider)?;
    store
        .apply_credentials(account_id, config, grok.as_ref())
        .await?;
    ensure!(
        anchors
            .iter()
            .all(|(key, value)| config.extra_headers.get(key) == Some(value)),
        "Selected account identity changed; sign in and select it again"
    );
    Ok(())
}

/// An unpinned Auto route whose stored identity no longer restores is chosen
/// again from the Auto options; an explicit account still fails closed.
pub(crate) async fn reresolve_or_block(
    store: &AccountStore,
    models: &IndexMap<String, ModelEntry>,
    config: &mut SamplerConfig,
    auto_allowed: bool,
) {
    let auto_route = auto_allowed
        && !config.extra_headers.contains_key(PIN_HEADER)
        && AccountRoute::read(&config.extra_headers)
            .ok()
            .flatten()
            .is_some_and(|route| route.option_id.is_some() && !route.pinned);
    if auto_route {
        let mut fresh = config.clone();
        fresh
            .extra_headers
            .retain(|name, _| !is_identity_anchor(name));
        if let Ok(routes) = routes_for_turn(store, models, &fresh, true).await
            && let Ok(next) = config_for_routes(store, models, &fresh, &routes).await
        {
            *config = next;
            return;
        }
    }
    block_credentials(config);
}

/// Restore errors must fail closed instead of falling back to a legacy bearer.
#[derive(Debug)]
struct UnavailableAccount;
impl distill_sampler::BearerResolver for UnavailableAccount {
    fn current_bearer(&self) -> Option<String> {
        None
    }
}
pub(crate) fn block_credentials(config: &mut SamplerConfig) {
    config
        .extra_headers
        .insert("x-distill-account-unavailable".into(), "1".into());
    config.api_key = None;
    config.bearer_resolver = Some(std::sync::Arc::new(UnavailableAccount));
    config.env_http_headers.clear();
    config
        .extra_headers
        .retain(|name, _| !credential_header(name));
}

pub(crate) fn quota_fallback_eligible(
    error: &distill_sampler::SamplingErrorInfo,
    provider: Provider,
) -> bool {
    error.should_retry != Some(false)
        && (error.kind == distill_sampler::SamplingErrorKind::RateLimited
            || (provider == Provider::Openrouter
                && error.kind == distill_sampler::SamplingErrorKind::Api
                && error.status_code == Some(402)))
}

#[cfg(test)]
#[path = "account_routing_tests.rs"]
pub(crate) mod tests;
