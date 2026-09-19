// Modified for Distill by Samuel Fajreldines, 2026.
//! Model-id resolution: catalog keys, routing slugs, and selection.

use globset::{Glob, GlobSet, GlobSetBuilder};
use indexmap::IndexMap;

use crate::agent::config::{self, ModelEntry};
use agent_client_protocol as acp;
use distill_sampling_types::ReasoningEffort;

/// Map a model id (catalog key or routing slug) to its catalog key.
pub(crate) fn resolve_catalog_key(
    models: &IndexMap<String, ModelEntry>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    let id_str = id.0.as_ref();
    if models.contains_key(id_str) {
        return Some(id.clone());
    }
    models
        .iter()
        .rev()
        .find(|(_, entry)| entry.info.has_model_id(id_str))
        .map(|(key, _)| acp::ModelId::new(key.clone()))
}

/// Catalog key for a persisted session model id, restricted to **selectable** entries.
pub(crate) fn selectable_catalog_key_for_persisted(
    models: &IndexMap<String, ModelEntry>,
    available: &IndexMap<acp::ModelId, acp::ModelInfo>,
    id: &acp::ModelId,
) -> Option<acp::ModelId> {
    if available.contains_key(id) {
        return Some(id.clone());
    }
    let id_str = id.0.as_ref();
    if let Some((key, _)) = models.iter().rev().find(|(key, entry)| {
        available.contains_key(&acp::ModelId::new((*key).clone()))
            && entry.info.has_model_id(id_str)
    }) {
        return Some(acp::ModelId::new(key.clone()));
    }
    resolve_catalog_key(models, id).filter(|key| available.contains_key(key))
}

/// A "campaign-only" preferred flip: the default changed and either side's value is an active campaign default.
pub(crate) fn is_campaign_only_flip(
    old_preferred: &Option<String>,
    new_preferred: &Option<String>,
    campaign_defaults: &std::collections::HashSet<String>,
) -> bool {
    if new_preferred == old_preferred || new_preferred.is_none() {
        return false;
    }
    new_preferred
        .as_ref()
        .is_some_and(|p| campaign_defaults.contains(p))
        || old_preferred
            .as_ref()
            .is_some_and(|p| campaign_defaults.contains(p))
}

/// Pick the default model: CLI > env > config > remote-settings hint, falling back to the first visible model, then the bundled default.
pub(crate) fn resolve_default_model(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> (String, ModelEntry, config::ConfigSource) {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.info.visible_for_auth(is_session_auth) && e.info.user_selectable)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let model_pref = config::resolve_string_flag(
        cfg.default_model_override.as_deref(),
        "GROK_DEFAULT_MODEL",
        cfg.models.default.as_deref(),
        cfg.remote_settings
            .as_ref()
            .and_then(|rs| rs.default_model.as_deref()),
    );

    let first_or_fallback = || -> (String, ModelEntry) {
        if let Some((key, first)) = visible.first() {
            return (key.clone(), first.clone());
        }
        if let Some((key, entry)) = catalog.iter().find(|(_, e)| e.info.user_selectable) {
            tracing::warn!("no auth-visible selectable model; using first selectable entry");
            return (key.clone(), entry.clone());
        }
        tracing::warn!("no selectable models; falling back to bundled default (pre-catalog)");
        let default_id = crate::models::default_model().to_string();
        let mut entry = ModelEntry::fallback(&default_id, &cfg.endpoints);
        entry.info.user_selectable = model_is_allowlisted(cfg, &default_id, &default_id);
        (default_id, entry)
    };

    match &model_pref {
        None => {
            let (key, first) = first_or_fallback();
            (key, first, config::ConfigSource::Default)
        }
        Some(pref) => {
            let found = visible
                .get_key_value(&pref.value)
                .or_else(|| visible.iter().find(|(_, m)| m.has_model_id(&pref.value)));

            if let Some((key, entry)) = found {
                (key.clone(), entry.clone(), pref.source)
            } else {
                let is_explicit = matches!(
                    pref.source,
                    config::ConfigSource::Cli
                        | config::ConfigSource::Env
                        | config::ConfigSource::Config
                );
                if is_explicit {
                    tracing::warn!(
                        model_id = %pref.value, source = %pref.source,
                        "preferred model not in available models, falling back"
                    );
                } else {
                    tracing::debug!(
                        model_id = %pref.value, source = %pref.source,
                        "remote default_model not in available models, skipping"
                    );
                }
                let campaign_pref_missing = cfg.models.default_is_campaign_driven
                    && matches!(pref.source, config::ConfigSource::Config);
                if campaign_pref_missing
                    && let Some(prev) = cfg
                        .models
                        .pre_campaign_default
                        .as_deref()
                        .filter(|s| !s.is_empty())
                    && let Some((key, entry)) = visible
                        .get_key_value(prev)
                        .or_else(|| visible.iter().find(|(_, m)| m.has_model_id(prev)))
                {
                    tracing::info!(
                        unavailable = %pref.value, fallback = %prev,
                        "campaign-driven default unavailable in catalog; recovering the pre-campaign default"
                    );
                    return (key.clone(), entry.clone(), config::ConfigSource::Config);
                }
                let (key, first) = first_or_fallback();
                (key, first, config::ConfigSource::Default)
            }
        }
    }
}

/// Filter hidden and auth-gated entries out of `catalog` and convert to ACP wire format.
pub(crate) fn available_models(
    catalog: &IndexMap<String, ModelEntry>,
    is_session_auth: bool,
) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    let visible: IndexMap<String, ModelEntry> = catalog
        .iter()
        .filter(|(_, e)| e.info.visible_for_auth(is_session_auth))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    config::to_acp_model_info(&visible)
}

/// Compiled glob matcher shared by `allowed_models`, `disabled_models`, and `hidden_models` (matched against catalog key or model id).
pub(crate) struct ModelGlobSet(GlobSet);

impl ModelGlobSet {
    /// Compile a filter list (`Ok(None)` for `None`/empty). Fails **closed**: an invalid pattern returns `Err` listing every bad one.
    pub(crate) fn compile(patterns: Option<&[String]>) -> Result<Option<Self>, Vec<String>> {
        let patterns = match patterns {
            Some(p) if !p.is_empty() => p,
            _ => return Ok(None),
        };
        let mut builder = GlobSetBuilder::new();
        let mut invalid = Vec::new();
        for pat in patterns {
            match Glob::new(pat) {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => invalid.push(pat.clone()),
            }
        }
        if !invalid.is_empty() {
            return Err(invalid);
        }
        builder
            .build()
            .map(|set| Some(Self(set)))
            .map_err(|e| vec![e.to_string()])
    }

    fn matches(&self, key: &str, model: &str) -> bool {
        self.0.is_match(key) || self.0.is_match(model)
    }

    fn matches_model(&self, model: &str) -> bool {
        self.0.is_match(model)
    }
}

/// Resolved allowlist: fleet pin, user/project list, or unrestricted.
enum EffectiveAllowlist<'a> {
    Unrestricted,
    Invalid,
    User(&'a [String]),
    Fleet(&'a [String]),
}

fn effective_allowlist(cfg: &config::Config) -> EffectiveAllowlist<'_> {
    use crate::agent::config::AllowlistPin;
    match cfg.requirements.allowed_models.pin_ref() {
        Some(AllowlistPin::FailClosed) => EffectiveAllowlist::Invalid,
        Some(AllowlistPin::List(patterns)) if patterns.is_empty() => {
            EffectiveAllowlist::Unrestricted
        }
        Some(AllowlistPin::List(patterns)) => EffectiveAllowlist::Fleet(patterns),
        None => match cfg.models.allowed_models.as_deref() {
            Some(patterns) if !patterns.is_empty() => EffectiveAllowlist::User(patterns),
            _ => EffectiveAllowlist::Unrestricted,
        },
    }
}

impl EffectiveAllowlist<'_> {
    fn is_unrestricted(&self) -> bool {
        matches!(self, Self::Unrestricted)
    }

    fn is_fleet(&self) -> bool {
        matches!(self, Self::Fleet(_) | Self::Invalid)
    }

    fn is_selected(&self, key: &str, model: &str) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::Invalid => false,
            Self::Fleet(patterns) | Self::User(patterns) => {
                match ModelGlobSet::compile(Some(patterns)) {
                    Ok(None) => true,
                    Ok(Some(set)) => {
                        if matches!(self, Self::Fleet(_)) {
                            set.matches_model(model)
                        } else {
                            set.matches(key, model)
                        }
                    }
                    Err(_) => false,
                }
            }
        }
    }

    fn apply_selectability(&self, catalog: &mut IndexMap<String, ModelEntry>) {
        match self {
            Self::Unrestricted => {
                for entry in catalog.values_mut() {
                    entry.info.user_selectable = true;
                }
            }
            Self::Invalid => {
                for entry in catalog.values_mut() {
                    entry.info.user_selectable = false;
                }
            }
            Self::Fleet(patterns) | Self::User(patterns) => {
                match ModelGlobSet::compile(Some(patterns)) {
                    Ok(None) => {
                        for entry in catalog.values_mut() {
                            entry.info.user_selectable = true;
                        }
                    }
                    Ok(Some(set)) => {
                        let fleet = matches!(self, Self::Fleet(_));
                        for (key, entry) in catalog.iter_mut() {
                            entry.info.user_selectable = if fleet {
                                set.matches_model(&entry.model)
                            } else {
                                set.matches(key, &entry.model)
                            };
                        }
                    }
                    Err(bad) => {
                        tracing::error!(
                            patterns = ?bad,
                            "allowed_models: invalid glob(s); marking nothing selectable"
                        );
                        for entry in catalog.values_mut() {
                            entry.info.user_selectable = false;
                        }
                    }
                }
            }
        }
    }
}

/// Catalog-key match is user-config only. A fleet pin matches the routing
/// slug so a user `[model.grok-4-anything]` cannot satisfy `grok-4*`.
fn model_is_allowlisted(cfg: &config::Config, key: &str, model: &str) -> bool {
    effective_allowlist(cfg).is_selected(key, model)
}

pub(crate) fn allowlist_denied_message(cfg: &config::Config) -> &'static str {
    if effective_allowlist(cfg).is_fleet() {
        "This model isn't allowed by your organization's policy. Contact your administrator."
    } else {
        "This model isn't allowed by your allowed_models setting."
    }
}

pub(crate) fn allowlist_excludes_all_message(cfg: &config::Config) -> String {
    match effective_allowlist(cfg) {
        EffectiveAllowlist::Invalid => {
            "The organization model policy is invalid. Contact your administrator.".to_owned()
        }
        EffectiveAllowlist::Fleet(_) => {
            "None of your models are allowed by your organization's policy. Contact your administrator."
                .to_owned()
        }
        _ => "None of your models are allowed by allowed_models. \
             Broaden it or remove it from your config, then restart."
            .to_owned(),
    }
}

/// Single source of truth for the catalog: applies `disabled_models`, then `allowed_models`, then `hidden_models`.
pub(crate) fn resolve_model_catalog(
    cfg: &config::Config,
    prefetched: Option<IndexMap<String, ModelEntry>>,
) -> IndexMap<String, ModelEntry> {
    let mut catalog: IndexMap<String, ModelEntry> = config::resolve_model_list(cfg, prefetched);

    if let Ok(Some(disabled)) = ModelGlobSet::compile(cfg.models.disabled_models.as_deref()) {
        let before = catalog.len();
        catalog.retain(|key, entry| !disabled.matches(key, &entry.model));
        let removed = before - catalog.len();
        if removed > 0 {
            tracing::info!(count = removed, "disabled_models: removed from catalog");
        }
    }

    effective_allowlist(cfg).apply_selectability(&mut catalog);

    if let Ok(Some(hidden)) = ModelGlobSet::compile(cfg.models.hidden_models.as_deref()) {
        for (key, entry) in catalog.iter_mut() {
            if hidden.matches(key, &entry.model) {
                entry.info.hidden = true;
            }
        }
    }

    if let Some(effort) = cfg.models.default_reasoning_effort
        && let Some(default_id) = cfg.models.default.as_deref()
        && let Some(entry) = catalog.get_mut(default_id)
        && entry.info.supports_reasoning_effort
    {
        stamp_effort(&mut entry.info, effort);
    }

    if let Some(effort) = cfg.reasoning_effort_override {
        for entry in catalog.values_mut() {
            if model_offers_reasoning_effort(&entry.info, effort) {
                stamp_effort(&mut entry.info, effort);
            }
        }
    }

    catalog
}

/// The entry keeps its own model id, and `model_at` picks the id for this effort when a request is prepared.
fn stamp_effort(info: &mut config::ModelInfo, effort: ReasoningEffort) {
    info.reasoning_effort = Some(effort);
}

/// Whether `effort` is a value this model will accept on the wire.
fn model_offers_reasoning_effort(info: &config::ModelInfo, effort: ReasoningEffort) -> bool {
    if !info.supports_reasoning_effort {
        return false;
    }
    if info.reasoning_efforts.is_empty() {
        matches!(
            effort,
            ReasoningEffort::Low
                | ReasoningEffort::Medium
                | ReasoningEffort::High
                | ReasoningEffort::Xhigh
        )
    } else {
        info.reasoning_efforts.iter().any(|opt| opt.value == effort)
    }
}

/// True when an active `allowed_models` allowlist leaves no selectable model.
pub(crate) fn allowlist_matches_nothing(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> bool {
    !effective_allowlist(cfg).is_unrestricted() && !catalog.values().any(|e| e.info.user_selectable)
}

/// Reject an `allowed_models` allowlist that leaves no selectable model, or excludes an explicitly configured default.
/// Run only against a real catalog.
pub(crate) fn validate_selectable(
    cfg: &config::Config,
    catalog: &IndexMap<String, ModelEntry>,
) -> Result<(), String> {
    let allowlist = effective_allowlist(cfg);
    match allowlist {
        EffectiveAllowlist::Unrestricted => return Ok(()),
        EffectiveAllowlist::Invalid => {
            return Err(
                "The organization model policy is invalid. Contact your administrator.".to_owned(),
            );
        }
        EffectiveAllowlist::Fleet(_) | EffectiveAllowlist::User(_) => {}
    }
    if !catalog.values().any(|e| e.info.user_selectable) {
        return Err(allowlist_excludes_all_message(cfg));
    }
    for (src, id) in [
        ("default", cfg.models.default.as_deref()),
        ("-m flag", cfg.default_model_override.as_deref()),
    ] {
        if let Some(id) = id
            && let Some(entry) = catalog
                .get(id)
                .or_else(|| catalog.values().find(|e| e.has_model_id(id)))
            && !entry.info.user_selectable
        {
            return Err(if allowlist.is_fleet() {
                format!(
                    "\"{id}\" (your {src}) isn't allowed by your organization's policy. \
                     Contact your administrator."
                )
            } else {
                format!(
                    "\"{id}\" (your {src}) isn't allowed by allowed_models. \
                     Broaden the patterns or remove allowed_models, then try again."
                )
            });
        }
    }
    Ok(())
}
