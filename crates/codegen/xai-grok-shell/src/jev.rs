//! Public, cached view of the Jev decision path for surfaces outside the
//! session — today the TUI badge in the prompt footer.
//!
//! Resolution lives here (and in the session wiring, which calls into this
//! module) so the badge and the wiring can never disagree about whether the
//! path is on. Nothing here touches the network: it reads configuration, the
//! environment tier, and whether a credential is *resolvable* — never its value.

use std::sync::OnceLock;

use xai_grok_workspace::jev::client::{JevClientConfig, credential_in_env};
use xai_grok_workspace::jev::flags::{JevFlags, JevLadderOverlay};

pub use xai_grok_workspace::jev::flags::JevStatus;

use crate::agent::config::JevConfig;

/// Reads `[jev]` from the merged, overlay-free config layers.
pub fn resolve_config_from_disk() -> JevConfig {
    match crate::config::ConfigLayers::load() {
        Ok(layers) => layers
            .effective_config_base_without_overlay()
            .get("jev")
            .and_then(|value| value.clone().try_into::<JevConfig>().ok())
            .unwrap_or_default(),
        Err(_) => JevConfig::default(),
    }
}

/// The environment tier of the `GROK_JEV` feature row. `None` means unset, which
/// (unlike `Some(false)`) leaves the harness default in place.
pub fn env_tier() -> Option<bool> {
    xai_grok_env::env_bool(xai_grok_config_types::Feature::Jev.env())
}

/// Resolves flags from config plus the live environment tier.
pub fn flags_from(cfg: &JevConfig) -> JevFlags {
    flags_from_tiers(cfg, env_tier())
}

/// Tier-explicit form (hermetic for tests).
///
/// Owner override of the plan's invariant I-1: the harness ships with the Jev
/// path **on**, so an unset `[jev]` (or an unset `GROK_JEV`) resolves to
/// [`JevFlags::harness_default`]. The kill switch still works — `[jev] enabled =
/// false` or `GROK_JEV=0` — and a lever whose evaluation gate fails can be
/// turned off on its own key.
pub fn flags_from_tiers(cfg: &JevConfig, env_enabled: Option<bool>) -> JevFlags {
    JevFlags::harness_default().overlaid(
        cfg.enabled,
        env_enabled,
        cfg.shadow,
        JevLadderOverlay {
            permission_classifier: cfg.ladder.permission_classifier,
            p1_tool_family: cfg.ladder.p1_tool_family,
            p2_read_shortlist: cfg.ladder.p2_read_shortlist,
            p3_compaction_recorte: cfg.ladder.p3_compaction_recorte,
            p5_call_validation: cfg.ladder.p5_call_validation,
            p6_skill_suggestion: cfg.ladder.p6_skill_suggestion,
            yolo_veto: cfg.ladder.yolo_veto,
            a1_file_to_edit: cfg.ladder.a1_file_to_edit,
            a3_log_lines: cfg.ladder.a3_log_lines,
            a4_web_results: cfg.ladder.a4_web_results,
            a5_memory_rank: cfg.ladder.a5_memory_rank,
            a6_test_to_run: cfg.ladder.a6_test_to_run,
            b1_intent_routing: cfg.ladder.b1_intent_routing,
            b2_model_tier: cfg.ladder.b2_model_tier,
            b3_subagent_type: cfg.ladder.b3_subagent_type,
            b6_delegation_hint: cfg.ladder.b6_delegation_hint,
            c1_premature_stop: cfg.ladder.c1_premature_stop,
            c2_failure_triage: cfg.ladder.c2_failure_triage,
            c3_completion_check: cfg.ladder.c3_completion_check,
            c4_diff_risk: cfg.ladder.c4_diff_risk,
            c5_error_priority: cfg.ladder.c5_error_priority,
            c6_injection_screen: cfg.ladder.c6_injection_screen,
            c7_change_type: cfg.ladder.c7_change_type,
            d2_big_output_retention: cfg.ladder.d2_big_output_retention,
            d3_post_compaction: cfg.ladder.d3_post_compaction,
        },
    )
}

/// Client configuration from `[jev]`, falling back to the plan's defaults.
pub fn client_config_from(cfg: &JevConfig) -> JevClientConfig {
    let defaults = JevClientConfig::default();
    JevClientConfig {
        base_url: cfg.base_url.clone().unwrap_or(defaults.base_url),
        model: cfg.model.clone().unwrap_or(defaults.model),
        timeout: cfg
            .timeout_ms
            .map(core::time::Duration::from_millis)
            .unwrap_or(defaults.timeout),
        api_key_env: cfg.api_key_env.clone().unwrap_or(defaults.api_key_env),
        max_state_bytes: cfg.max_state_bytes.unwrap_or(defaults.max_state_bytes),
    }
}

/// Whether the Jev path can act, for display: enabled, not shadowed, and a
/// credential is resolvable under the configured environment variable.
pub fn status_from(cfg: &JevConfig) -> JevStatus {
    let flags = flags_from(cfg);
    flags.status(credential_in_env(&client_config_from(cfg).api_key_env))
}

/// The status the TUI badge renders, resolved once per process.
///
/// Cached because the badge is drawn every frame while the configuration and
/// the credential env are fixed for the life of the process (the session wiring
/// also resolves once, at session start).
pub fn current_status_cached() -> JevStatus {
    static STATUS: OnceLock<JevStatus> = OnceLock::new();
    *STATUS.get_or_init(|| status_from(&resolve_config_from_disk()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge must never claim Jev is active when the kill switch is on.
    #[test]
    fn status_reflects_the_kill_switch_and_the_credential() {
        let off = JevConfig {
            enabled: Some(false),
            ..JevConfig::default()
        };
        assert!(!flags_from_tiers(&off, None).enabled);

        // Pure projection: the shell layer only forwards flags + credential
        // presence, and that pairing is what the label encodes.
        let on = flags_from_tiers(&JevConfig::default(), None);
        assert_eq!(on.status(true).label(), "jev");
        assert_eq!(on.status(false).label(), "jev:off");

        let shadow = flags_from_tiers(
            &JevConfig {
                shadow: Some(true),
                ..JevConfig::default()
            },
            None,
        );
        assert_eq!(shadow.status(true).label(), "jev·shadow");
    }

    #[test]
    fn client_config_defaults_match_the_contract() {
        let config = client_config_from(&JevConfig::default());
        assert_eq!(config.base_url, "https://api.typesafe.ai");
        assert_eq!(config.api_key_env, "JEV_API_KEY");
        assert_eq!(config.endpoint(), "https://api.typesafe.ai/v1/systemone");
    }
}

// ---------------------------------------------------------------------------
// Runtime helper for catalogue call sites (todo.md areas A–D)
// ---------------------------------------------------------------------------

/// Runs **one** catalogue decision.
///
/// Every gate lives here so no call site can forget one: the master switch and
/// the item's own flag (`JevLever`), a resolvable credential, a single attempt
/// inside a bounded budget, and `None` on anything else — which means the caller
/// keeps today's behaviour (fail-defer, invariant I-5 of the plan).
pub async fn ask_item(
    lever: xai_grok_workspace::jev::flags::JevLever,
    state: serde_json::Value,
    questions: std::collections::BTreeMap<
        xai_grok_workspace::jev::types::QuestionId,
        xai_grok_workspace::jev::types::Question,
    >,
) -> Option<xai_grok_workspace::jev::types::JevAnswerSet> {
    let flags = flags_cached();
    if !flags.lever_active(lever) {
        return None;
    }
    let client = client_cached()?;
    if !client.credential_present() {
        return None;
    }
    let budget = item_budget(client);
    match tokio::time::timeout(budget, client.ask(&state, &questions)).await {
        Ok(Ok(answers)) => Some(answers),
        Ok(Err(error)) => {
            tracing::debug!(
                lever = lever.as_str(),
                %error,
                "jev item call failed; keeping the current path"
            );
            None
        }
        Err(_) => {
            tracing::debug!(
                lever = lever.as_str(),
                budget_ms = budget.as_millis() as u64,
                "jev item call timed out; keeping the current path"
            );
            None
        }
    }
}

/// Records one catalogue decision so it lands in `~/.grok/logs/jev.jsonl`
/// (through the same sink and target as the permission seam).
pub fn record_item(
    lever: xai_grok_workspace::jev::flags::JevLever,
    decision: &str,
    reason: &str,
    confidence: Option<f64>,
    answers: Option<&xai_grok_workspace::jev::types::JevAnswerSet>,
) {
    use xai_grok_workspace::jev::policy::{DecisionRecord, DecisionSink, TracingSink};
    let record = DecisionRecord {
        lever: lever.as_str().to_owned(),
        questions: Vec::new(),
        decision: decision.to_owned(),
        reason: reason.to_owned(),
        confidence,
        model: answers.map_or_else(|| "n/a".to_owned(), |a| a.model.clone()),
        latency_ms: answers.map_or(0, |a| a.latency_ms),
        input_tokens: answers.map_or(0, |a| a.usage.input()),
        output_tokens: answers.map_or(0, |a| a.usage.output()),
        request_id: answers.and_then(|a| a.request_id.clone()),
        escalated: false,
    };
    TracingSink.record(&record);
}

/// The flags resolved once per process (configuration does not change mid-run).
fn flags_cached() -> xai_grok_workspace::jev::flags::JevFlags {
    static FLAGS: OnceLock<xai_grok_workspace::jev::flags::JevFlags> = OnceLock::new();
    *FLAGS.get_or_init(|| flags_from(&resolve_config_from_disk()))
}

/// One client per process, built lazily and only when a credential exists.
fn client_cached() -> Option<&'static xai_grok_workspace::jev::JevClient> {
    static CLIENT: OnceLock<Option<xai_grok_workspace::jev::JevClient>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let cfg = resolve_config_from_disk();
            match xai_grok_workspace::jev::JevClient::new(client_config_from(&cfg)) {
                Ok(client) if client.credential_present() => Some(client),
                Ok(_) => None,
                Err(error) => {
                    tracing::warn!(%error, "jev client unavailable; catalogue items stay off");
                    None
                }
            }
        })
        .as_ref()
}

/// Item calls sit on the tool-result path rather than the permission actor, so
/// they get a shorter budget than the client default.
fn item_budget(client: &xai_grok_workspace::jev::JevClient) -> std::time::Duration {
    client
        .config()
        .timeout
        .min(std::time::Duration::from_millis(4_000))
}

#[cfg(test)]
mod catalogue_helper_tests {
    use super::*;

    #[test]
    fn item_budget_is_capped_for_the_tool_result_path() {
        let cfg = JevConfig::default();
        let client = xai_grok_workspace::jev::JevClient::new(client_config_from(&cfg))
            .expect("client builds without I/O");
        let budget = item_budget(&client);
        assert!(budget <= std::time::Duration::from_millis(4_000));
        assert!(!budget.is_zero());
    }

    #[test]
    fn recording_an_item_decision_never_panics_without_answers() {
        record_item(
            xai_grok_workspace::jev::flags::JevLever::A1FileToEdit,
            "rank",
            "no answers",
            None,
            None,
        );
    }
}
