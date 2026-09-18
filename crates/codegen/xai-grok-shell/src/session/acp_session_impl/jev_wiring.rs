//! Wiring for the Jev (TypeSafe System One) decision path in the shell.
//!
//! Source of truth: `plan/plan.md` §1.2 (items B and D1), §1.6 (I-1, I-3, I-5,
//! I-6) and §12.5 (contract, defaults, divergences).
//!
//! While `GROK_JEV` is off, nothing here runs: the incumbent classifier is
//! returned untouched (invariant I-1). Enablement is never read from a
//! checked-out repository — general settings come from user/system/managed
//! layers only, so a project config cannot turn third-party egress on or
//! repoint the endpoint (invariant I-6).

use std::sync::Arc;
use std::time::Duration;

use xai_grok_workspace::jev::JevClient;
use xai_grok_workspace::jev::permission::{JevAsker, JevAuthority, JevPermissionClassifier};
use xai_grok_workspace::jev::policy::DecisionSink;
use xai_grok_workspace::jev::questions::PermissionThresholds;
use xai_grok_workspace::permission::{ClassifierVerdict, FixedClassifier, SharedClassifier};

use crate::agent::config::JevConfig;
use crate::jev::{
    ActivitySink, ObservedAsker, client_config_from as jev_client_config_from,
    flags_from as jev_flags_from,
};

/// Wraps the incumbent classifier with the Jev seam when the flags ask for it.
///
/// `budget` is the caller-owned end-to-end deadline (invariant I-5): the Jev
/// attempt gets it whole, makes a single attempt and never retries; the
/// incumbent path runs whatever remains. The returned classifier carries Jev
/// provenance only for its own decisions, which is what keeps a Jev allow from
/// clearing the denial ratchet (invariant I-4, enforced in the manager).
pub(crate) fn maybe_wrap_with_jev(
    cfg: &JevConfig,
    incumbent: SharedClassifier,
    budget: Duration,
) -> SharedClassifier {
    let flags = jev_flags_from(cfg);
    if !flags.enabled || !flags.permission_classifier {
        return incumbent;
    }
    let client = match JevClient::new(jev_client_config_from(cfg)) {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "jev client unavailable; keeping the incumbent classifier");
            return incumbent;
        }
    };
    if !client.credential_present() {
        tracing::warn!(
            env = client.config().api_key_env.as_str(),
            "jev credential env is unset; keeping the incumbent classifier"
        );
        return incumbent;
    }
    let asker: Arc<dyn JevAsker> = Arc::new(ObservedAsker::new(Arc::new(client)));
    let sink: Arc<dyn DecisionSink> = Arc::new(ActivitySink);
    match JevPermissionClassifier::new(
        asker,
        Arc::clone(&incumbent),
        PermissionThresholds::default(),
        flags,
        JevAuthority::AllowRoutine,
        sink,
        budget,
    ) {
        Ok(classifier) => {
            tracing::info!(
                shadow = flags.shadow,
                budget_ms = budget.as_millis() as u64,
                "Wired the Jev permission classifier ahead of the incumbent path"
            );
            Arc::new(classifier)
        }
        Err(error) => {
            tracing::warn!(%error, "jev question catalog rejected; keeping the incumbent classifier");
            incumbent
        }
    }
}

/// Builds the Jev **brake** for YOLO / always-approve sessions.
///
/// The brake is `VetoOnly`: it may refuse a confident catastrophe and nothing
/// else — it never allows anything the mode would not already allow, never
/// prompts, and fails open, so a missing credential or an unreachable service
/// leaves the session exactly as it was. That is why it needs no incumbent and
/// no side-query worker.
pub(crate) fn maybe_wrap_with_jev_veto(
    cfg: &JevConfig,
    budget: Duration,
) -> Option<SharedClassifier> {
    let flags = jev_flags_from(cfg);
    if !flags.enabled || !flags.yolo_veto {
        return None;
    }
    let client = match JevClient::new(jev_client_config_from(cfg)) {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(%error, "jev brake unavailable; always-approve keeps running unchecked");
            return None;
        }
    };
    if !client.credential_present() {
        tracing::warn!(
            env = client.config().api_key_env.as_str(),
            "jev brake has no credential; always-approve keeps running unchecked"
        );
        return None;
    }
    let asker: Arc<dyn JevAsker> = Arc::new(ObservedAsker::new(Arc::new(client)));
    let sink: Arc<dyn DecisionSink> = Arc::new(ActivitySink);
    // Inert fallback: `VetoOnly` never escalates, so nothing can reach it. Using
    // a fixed classifier keeps that explicit instead of implying an LLM behind it.
    let inert: SharedClassifier = Arc::new(FixedClassifier(ClassifierVerdict::Allow));
    match JevPermissionClassifier::new(
        asker,
        inert,
        PermissionThresholds::default(),
        flags,
        JevAuthority::VetoOnly,
        sink,
        budget,
    ) {
        Ok(brake) => {
            tracing::info!(
                budget_ms = budget.as_millis() as u64,
                "Built the Jev brake for always-approve (YOLO) mode"
            );
            Some(Arc::new(brake))
        }
        Err(error) => {
            tracing::warn!(%error, "jev question catalog rejected; always-approve keeps running unchecked");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::flags_from_tiers;
    use xai_grok_workspace::permission::HeuristicPermissionClassifier;

    #[test]
    fn an_unset_config_uses_the_harness_default_of_everything_on() {
        let flags = flags_from_tiers(&JevConfig::default(), None);
        assert!(
            flags.enabled,
            "unset config resolves to the harness default"
        );
        assert!(flags.permission_classifier);
        assert!(!flags.shadow, "active by default, not shadow");
    }

    #[test]
    fn explicit_values_still_win_and_the_kill_switch_works() {
        let off = JevConfig {
            enabled: Some(false),
            ..JevConfig::default()
        };
        assert!(!flags_from_tiers(&off, None).enabled);
        assert!(!flags_from_tiers(&JevConfig::default(), Some(false)).enabled);
        assert!(flags_from_tiers(&JevConfig::default(), Some(true)).enabled);
        // A config value is authoritative over the environment tier.
        assert!(!flags_from_tiers(&off, Some(true)).enabled);
        // A single lever can be disabled on its own key.
        let mut ladder_off = JevConfig::default();
        ladder_off.ladder.permission_classifier = Some(false);
        let flags = flags_from_tiers(&ladder_off, None);
        assert!(flags.enabled);
        assert!(!flags.permission_classifier);
        assert!(flags.p1_tool_family);
    }

    #[test]
    fn shadow_can_be_turned_on_explicitly() {
        let cfg = JevConfig {
            shadow: Some(true),
            ..JevConfig::default()
        };
        assert!(flags_from_tiers(&cfg, None).shadow);
    }

    #[test]
    fn client_config_defaults_match_the_contract() {
        let config = jev_client_config_from(&JevConfig::default());
        assert_eq!(config.base_url, "https://api.typesafe.ai");
        assert_eq!(config.model, "jev-latest");
        assert_eq!(config.timeout, Duration::from_secs(10));
        assert_eq!(config.api_key_env, "JEV_API_KEY");
        assert_eq!(config.endpoint(), "https://api.typesafe.ai/v1/systemone");
    }

    #[test]
    fn a_disabled_config_returns_the_incumbent_untouched() {
        let incumbent: SharedClassifier = Arc::new(HeuristicPermissionClassifier);
        let disabled = JevConfig {
            enabled: Some(false),
            ..JevConfig::default()
        };
        let wrapped =
            maybe_wrap_with_jev(&disabled, Arc::clone(&incumbent), Duration::from_secs(1));
        assert!(
            Arc::ptr_eq(&wrapped, &incumbent),
            "an off flag must not replace or wrap the incumbent classifier"
        );
    }

    #[test]
    fn a_missing_credential_keeps_the_incumbent_classifier() {
        // Defaults are on, but without a resolvable credential the seam must not
        // replace the incumbent path (and must not panic).
        let incumbent: SharedClassifier = Arc::new(HeuristicPermissionClassifier);
        let previous = std::env::var("JEV_API_KEY").ok();
        if previous.is_none() {
            let wrapped = maybe_wrap_with_jev(
                &JevConfig::default(),
                Arc::clone(&incumbent),
                Duration::from_secs(1),
            );
            assert!(Arc::ptr_eq(&wrapped, &incumbent));
        }
    }
}
