// Modified for Distill by Samuel Fajreldines, 2026.
use crate::util::config::RemoteSettings;
use distill_sampling_types::ReasoningEffort;
use toml::Value as TomlValue;

pub(crate) const ENV_PROMPT_SUGGESTIONS: &str = "GROK_PROMPT_SUGGESTIONS";

const PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MIN: u32 = 16;
const PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_DEFAULT: u32 = 64;
const PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MAX: u32 = 256;
/// Low temperature keeps the prediction close to the obvious next step.
const PROMPT_SUGGEST_TEMPERATURE_DEFAULT: f32 = 0.2;

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PromptSuggestConfig {
    pub enabled: Option<bool>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Malformed payloads are ignored so remote settings are applied atomically.
fn coerce_prompt_suggest_json(value: serde_json::Value) -> Option<PromptSuggestConfig> {
    match serde_json::from_value(value) {
        Ok(cfg) => Some(cfg),
        Err(e) => {
            tracing::warn!(error = %e, "[prompt_suggestions]: dropped malformed remote payload");
            None
        }
    }
}

fn coerce_remote_prompt_suggest(remote: Option<&RemoteSettings>) -> Option<PromptSuggestConfig> {
    coerce_prompt_suggest_json(remote?.prompt_suggestions.clone()?)
}

pub(crate) fn remote_prompt_suggestions_enabled(remote: Option<&RemoteSettings>) -> Option<bool> {
    coerce_remote_prompt_suggest(remote).and_then(|c| c.enabled)
}

fn prompt_suggestions_from_toml(v: Option<&TomlValue>) -> Option<bool> {
    v?.get("ui")?.get("prompt_suggestions")?.as_bool()
}

fn resolve_prompt_suggestions_layers(
    requirement: Option<bool>,
    config: Option<bool>,
    managed: Option<bool>,
    feature_flag: Option<bool>,
) -> crate::agent::config::Resolved<bool> {
    use crate::agent::config::BoolFlag;
    BoolFlag::env(ENV_PROMPT_SUGGESTIONS)
        .requirement(requirement)
        .config(config)
        .managed(managed)
        .feature_flag(feature_flag)
        .default(true)
        .resolve()
}

/// Precedence: requirements, env, user config, managed config, remote, then true.
pub fn resolve_prompt_suggestions_enabled(
    requirements: Option<&TomlValue>,
    user: Option<&TomlValue>,
    managed: Option<&TomlValue>,
    remote: Option<&RemoteSettings>,
) -> crate::agent::config::Resolved<bool> {
    resolve_prompt_suggestions_layers(
        prompt_suggestions_from_toml(requirements),
        prompt_suggestions_from_toml(user),
        prompt_suggestions_from_toml(managed),
        coerce_remote_prompt_suggest(remote).and_then(|c| c.enabled),
    )
}

static REMOTE_PROMPT_SUGGEST_CONFIG: std::sync::RwLock<Option<PromptSuggestConfig>> =
    std::sync::RwLock::new(None);

pub fn cache_remote_prompt_suggestions(value: Option<serde_json::Value>) {
    let coerced = value.and_then(coerce_prompt_suggest_json);
    if let Ok(mut guard) = REMOTE_PROMPT_SUGGEST_CONFIG.write() {
        *guard = coerced;
    }
}

pub fn cache_remote_prompt_suggestions_enabled(value: Option<bool>) {
    if let Ok(mut guard) = REMOTE_PROMPT_SUGGEST_CONFIG.write() {
        guard.get_or_insert_with(Default::default).enabled = value;
    }
}

pub fn cached_remote_prompt_suggestions_enabled() -> Option<bool> {
    REMOTE_PROMPT_SUGGEST_CONFIG
        .read()
        .ok()
        .and_then(|g| g.as_ref().and_then(|c| c.enabled))
}

/// Resolves the in-memory pager gate without reading config from disk.
pub fn prompt_suggestions_enabled_for_config(config: Option<bool>) -> bool {
    resolve_prompt_suggestions_layers(
        None,
        config,
        None,
        cached_remote_prompt_suggestions_enabled(),
    )
    .value
}

fn prompt_suggest_config_from_toml(v: Option<&TomlValue>) -> Option<PromptSuggestConfig> {
    let table = v?.get("prompt_suggestions")?.clone();
    table
        .try_into()
        .map_err(
            |e| tracing::warn!(error = %e, "[prompt_suggestions]: dropped malformed local table"),
        )
        .ok()
}

fn prompt_suggestions_enabled_from_layers(
    layers: &crate::config::ConfigLayers,
    requirements: Option<&TomlValue>,
    remote: Option<bool>,
) -> bool {
    let crate::config::ConfigLayers {
        system_managed,
        managed,
        user,
        env_overlay: _,
        user_requirements: _,
        system_requirements: _,
        mdm_requirements: _,
        campaigns: _,
    } = layers;
    resolve_prompt_suggestions_layers(
        prompt_suggestions_from_toml(requirements),
        prompt_suggestions_from_toml(Some(user)),
        prompt_suggestions_from_toml(Some(managed))
            .or_else(|| prompt_suggestions_from_toml(Some(system_managed))),
        remote,
    )
    .value
}

pub fn prompt_suggestions_enabled_from_disk() -> bool {
    let requirements = crate::config::load_merged_requirements();
    let layers = match crate::config::ConfigLayers::load() {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, "prompt_suggestions: failed to load config layers");
            crate::config::ConfigLayers::default()
        }
    };
    prompt_suggestions_enabled_from_layers(
        &layers,
        requirements.as_ref(),
        cached_remote_prompt_suggestions_enabled(),
    )
}

fn prompt_suggest_config_overlay_free(layers: &crate::config::ConfigLayers) -> PromptSuggestConfig {
    prompt_suggest_config_from_toml(Some(&layers.effective_config_base_without_overlay()))
        .unwrap_or_default()
}

fn merge_prompt_suggest_config(
    config: PromptSuggestConfig,
    remote: PromptSuggestConfig,
) -> PromptSuggestConfig {
    PromptSuggestConfig {
        enabled: config.enabled.or(remote.enabled),
        max_output_tokens: config.max_output_tokens.or(remote.max_output_tokens),
        temperature: config.temperature.or(remote.temperature),
        reasoning_effort: config.reasoning_effort.or(remote.reasoning_effort),
    }
}

pub(crate) fn resolve_prompt_suggest_config_from_disk() -> PromptSuggestConfig {
    let config = match crate::config::ConfigLayers::load() {
        Ok(layers) => prompt_suggest_config_overlay_free(&layers),
        Err(_) => PromptSuggestConfig::default(),
    };
    let remote = REMOTE_PROMPT_SUGGEST_CONFIG
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    merge_prompt_suggest_config(config, remote)
}

/// Bounds the visible-output budget independently from a model-specific reasoning reserve.
pub(crate) fn prompt_suggest_sampling_defaults(
    cfg: &PromptSuggestConfig,
) -> (u32, f32, Option<ReasoningEffort>) {
    let max_output_tokens = cfg
        .max_output_tokens
        .unwrap_or(PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_DEFAULT)
        .clamp(
            PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MIN,
            PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MAX,
        );
    let temperature = cfg
        .temperature
        .unwrap_or(PROMPT_SUGGEST_TEMPERATURE_DEFAULT);
    // Pass through. Unset stays None so the non-reasoning model pin still applies.
    // Low is chosen later, and only if the selected model supports reasoning.
    (max_output_tokens, temperature, cfg.reasoning_effort)
}

/// This alias resolves server-side with `alias_default_effort = none`.
pub(crate) const NON_REASONING_PROMPT_SUGGEST_MODEL: &str = "grok-4-1-fast-non-reasoning";

pub(crate) fn prompt_suggest_reasoning_is_off(configured: Option<ReasoningEffort>) -> bool {
    matches!(configured, None | Some(ReasoningEffort::None))
}

pub(crate) fn prompt_suggest_reasoning_budget(
    visible_output_tokens: u32,
    reserve_reasoning_budget: bool,
) -> u32 {
    const LOW_REASONING_RESERVE_TOKENS: u32 = 256;
    if reserve_reasoning_budget {
        visible_output_tokens.saturating_add(LOW_REASONING_RESERVE_TOKENS)
    } else {
        visible_output_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::ConfigSource;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { std::env::remove_var(ENV_PROMPT_SUGGESTIONS) };
        guard
    }

    fn ui_config(enabled: bool) -> TomlValue {
        toml::from_str(&format!("[ui]\nprompt_suggestions = {enabled}\n")).unwrap()
    }

    #[test]
    fn config_beats_managed_and_remote() {
        let _guard = guard();
        let enabled = ui_config(true);
        let disabled = ui_config(false);
        let remote = RemoteSettings {
            prompt_suggestions: Some(serde_json::json!({ "enabled": false })),
            ..RemoteSettings::default()
        };

        let resolved = resolve_prompt_suggestions_enabled(
            None,
            Some(&enabled),
            Some(&disabled),
            Some(&remote),
        );
        assert!(resolved.value);
        assert_eq!(resolved.source, ConfigSource::Config);
    }

    #[test]
    fn malformed_remote_payload_is_ignored() {
        let _guard = guard();
        let remote = RemoteSettings {
            prompt_suggestions: Some(serde_json::json!({
                "enabled": false,
                "reasoning_effort": "turbo"
            })),
            ..RemoteSettings::default()
        };

        let resolved = resolve_prompt_suggestions_enabled(None, None, None, Some(&remote));
        assert!(resolved.value);
        assert_eq!(resolved.source, ConfigSource::Default);
    }

    #[test]
    fn sampling_defaults_use_short_suggestion_budget() {
        let (max_output_tokens, _, _) =
            prompt_suggest_sampling_defaults(&PromptSuggestConfig::default());
        assert_eq!(max_output_tokens, 64);
    }

    #[test]
    fn sampling_defaults_clamp_remote_output_tokens() {
        let lower = prompt_suggest_sampling_defaults(&PromptSuggestConfig {
            max_output_tokens: Some(0),
            ..PromptSuggestConfig::default()
        })
        .0;
        let upper = prompt_suggest_sampling_defaults(&PromptSuggestConfig {
            max_output_tokens: Some(PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MAX + 1),
            ..PromptSuggestConfig::default()
        })
        .0;
        assert_eq!(
            (lower, upper),
            (
                PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MIN,
                PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_MAX
            )
        );
    }

    #[test]
    fn sampling_defaults_leave_unset_reasoning_unset() {
        let (max_output_tokens, _, reasoning_effort) =
            prompt_suggest_sampling_defaults(&PromptSuggestConfig::default());
        assert_eq!(
            (max_output_tokens, reasoning_effort),
            (PROMPT_SUGGEST_MAX_OUTPUT_TOKENS_DEFAULT, None)
        );
    }

    #[test]
    fn sampling_defaults_preserve_explicit_high() {
        let (_, _, reasoning_effort) = prompt_suggest_sampling_defaults(&PromptSuggestConfig {
            reasoning_effort: Some(ReasoningEffort::High),
            ..PromptSuggestConfig::default()
        });
        assert_eq!(reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn sampling_defaults_preserve_configured_reasoning_and_output_budget() {
        let (max_output_tokens, _, reasoning_effort) =
            prompt_suggest_sampling_defaults(&PromptSuggestConfig {
                max_output_tokens: Some(128),
                reasoning_effort: Some(ReasoningEffort::Low),
                ..PromptSuggestConfig::default()
            });
        assert_eq!(
            (max_output_tokens, reasoning_effort),
            (128, Some(ReasoningEffort::Low))
        );
    }

    #[test]
    fn reasoning_off_uses_the_non_reasoning_sampling_path() {
        assert!(prompt_suggest_reasoning_is_off(None));
        assert!(prompt_suggest_reasoning_is_off(Some(ReasoningEffort::None)));
        assert!(!prompt_suggest_reasoning_is_off(Some(ReasoningEffort::Low)));
        assert_eq!(
            NON_REASONING_PROMPT_SUGGEST_MODEL,
            "grok-4-1-fast-non-reasoning"
        );
    }

    #[test]
    fn reasoning_budget_reserves_headroom_without_changing_visible_output() {
        assert_eq!(
            (
                prompt_suggest_reasoning_budget(64, false),
                prompt_suggest_reasoning_budget(64, true),
                prompt_suggest_reasoning_budget(128, true),
            ),
            (64, 320, 384)
        );
    }

    #[test]
    fn configured_reasoning_budget_exceeds_the_visible_floor() {
        let visible_output_tokens = prompt_suggest_sampling_defaults(&PromptSuggestConfig {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..PromptSuggestConfig::default()
        })
        .0;
        assert!(
            prompt_suggest_reasoning_budget(visible_output_tokens, true) > visible_output_tokens
        );
    }
}
