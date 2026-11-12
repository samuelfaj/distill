// Modified for Distill by Samuel Fajreldines, 2026.
//! Public, cached view of the Jev decision path for surfaces outside the
//! session — today the TUI badge in the prompt footer.
//!
//! Resolution lives here (and in the session wiring, which calls into this
//! module) so the badge and the wiring can never disagree about whether the
//! path is on. Nothing here touches the network: it reads configuration, the
//! environment tier, and whether a credential is *resolvable* — never its value.

use std::future::Future;
use std::sync::OnceLock;

use distill_workspace::jev::client::{JevClientConfig, credential_in_env};
use distill_workspace::jev::flags::{JevFlags, JevLadderOverlay};

pub use distill_workspace::jev::flags::JevStatus;

use crate::agent::config::{JevConfig, JevLocalConfig};

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
    distill_env::env_bool(distill_config_types::Feature::Jev.env())
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
            // Jev optimizes execution; only the harness decides permissions.
            e_crushers: cfg.ladder.e_crushers,
            e_retention: cfg.ladder.e_retention,
            e_importance: cfg.ladder.e_importance,
            e_cheap_compress: cfg.ladder.e_cheap_compress,
            e_read_reuse: cfg.ladder.e_read_reuse,
            e_cheap_agent: cfg.ladder.e_cheap_agent,
            e_prompt_blocks: cfg.ladder.e_prompt_blocks,
            p1_tool_family: cfg.ladder.p1_tool_family,
            p2_read_shortlist: cfg.ladder.p2_read_shortlist,
            p3_compaction_recorte: cfg.ladder.p3_compaction_recorte,
            p6_skill_suggestion: cfg.ladder.p6_skill_suggestion,
            a1_file_to_edit: cfg.ladder.a1_file_to_edit,
            a4_web_results: cfg.ladder.a4_web_results,
            a5_memory_rank: cfg.ladder.a5_memory_rank,
            a6_test_to_run: cfg.ladder.a6_test_to_run,
            b1_intent_routing: cfg.ladder.b1_intent_routing,
            b2_model_tier: cfg.ladder.b2_model_tier,
            b2_micro_effort: cfg.ladder.b2_micro_effort,
            b2_execution_tier: cfg.ladder.b2_execution_tier,
            b2_local_model: cfg.ladder.b2_local_model,
            b3_subagent_type: cfg.ladder.b3_subagent_type,
            b6_delegation_hint: cfg.ladder.b6_delegation_hint,
            c1_premature_stop: cfg.ladder.c1_premature_stop,
            c3_completion_check: cfg.ladder.c3_completion_check,
            c4_diff_risk: cfg.ladder.c4_diff_risk,
            c5_error_priority: cfg.ladder.c5_error_priority,
            c6_injection_screen: cfg.ladder.c6_injection_screen,
            d2_big_output_retention: cfg.ladder.d2_big_output_retention,
            d3_post_compaction: cfg.ladder.d3_post_compaction,
            d4_compaction_timing: cfg.ladder.d4_compaction_timing,
            d5_memory_capture_gate: cfg.ladder.d5_memory_capture_gate,
            d6_history_eviction: cfg.ladder.d6_history_eviction,
            d6_warm_batches: cfg.ladder.d6_warm_batches,
            b7_subagent_model: cfg.ladder.b7_subagent_model,
            b8_execution_mode: cfg.ladder.b8_execution_mode,
            b8_stable_tools: cfg.ladder.b8_stable_tools,
            b8_review_hygiene: cfg.ladder.b8_review_hygiene,
            f1_followup_label: cfg.ladder.f1_followup_label,
            b8_criteria_v2: cfg.ladder.b8_criteria_v2,
            b8_up_only: cfg.ladder.b8_up_only,
        },
    )
}

/// Validates `[jev]` transport settings and applies safe defaults.
pub fn client_config_from(cfg: &JevConfig) -> Result<JevClientConfig, distill_workspace::jev::JevError> {
    use distill_workspace::jev::provider::{JevProvider, ReasoningShape};
    let defaults = JevClientConfig::default();
    let provider = match cfg.provider.as_deref() {
        None => JevProvider::OpenRouterDecisions,
        Some(name) => JevProvider::from_name(name)
            .ok_or_else(|| distill_workspace::jev::JevError::invalid("unknown Jev provider"))?,
    };
    if provider != JevProvider::OpenRouterDecisions {
        return Err(distill_workspace::jev::JevError::invalid("Jev requires OpenRouter Decisions"));
    }
    let base_url = cfg.base_url.clone().unwrap_or(defaults.base_url);
    if base_url.trim_end_matches('/') != distill_workspace::jev::client::DEFAULT_BASE_URL {
        return Err(distill_workspace::jev::JevError::invalid("Jev endpoint must use https://openrouter.ai/api"));
    }
    if cfg.api_key_env.as_deref().is_some_and(|key| key != "OPENROUTER_API_KEY") {
        return Err(distill_workspace::jev::JevError::invalid("Jev requires OPENROUTER_API_KEY"));
    }
    let model = cfg.model.clone().unwrap_or(defaults.model);
    if !(model.starts_with("~typesafe/jev") || model.starts_with("typesafe/jev")) {
        return Err(distill_workspace::jev::JevError::invalid("Jev model must be a typesafe/jev model"));
    }
    let reasoning_shape = cfg.reasoning_shape.as_deref()
        .map(|name| ReasoningShape::from_name(name).ok_or_else(|| distill_workspace::jev::JevError::invalid("unknown Jev reasoning shape")))
        .transpose()?.unwrap_or(defaults.reasoning_shape);
    Ok(JevClientConfig {
        base_url, model,
        timeout: cfg.timeout_ms.map(core::time::Duration::from_millis).unwrap_or(defaults.timeout),
        api_key_env: "OPENROUTER_API_KEY".to_owned(),
        max_state_bytes: cfg.max_state_bytes.unwrap_or(defaults.max_state_bytes), provider,
        reasoning_shape, reasoning_effort: cfg.reasoning_effort.clone().unwrap_or(defaults.reasoning_effort),
        max_completion_tokens: cfg.max_completion_tokens.unwrap_or(defaults.max_completion_tokens),
        item_budget: cfg.item_budget_ms.map(core::time::Duration::from_millis).unwrap_or(defaults.item_budget),
    })
}

/// Whether the Jev path can act, for display: enabled, not shadowed, and a
/// credential is resolvable under the configured environment variable.
pub fn status_from(cfg: &JevConfig) -> JevStatus {
    let flags = flags_from(cfg);
    flags.status(client_config_from(cfg).ok().is_some_and(|config| credential_in_env(&config.api_key_env)))
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

    /// An unset or mistyped variant must still route by price (`floor`); only an explicit `none` drops the suffix.
    #[test]
    fn openrouter_variant_resolves_unset_none_and_invalid() {
        assert_eq!(resolve_openrouter_variant(None).as_deref(), Some("floor"));
        assert_eq!(resolve_openrouter_variant(Some("")).as_deref(), Some("floor"));
        assert_eq!(resolve_openrouter_variant(Some("none")), None);
        assert_eq!(resolve_openrouter_variant(Some("nitro")).as_deref(), Some("nitro"));
        assert_eq!(resolve_openrouter_variant(Some("exacto")).as_deref(), Some("exacto"));
        assert_eq!(resolve_openrouter_variant(Some("bogus")).as_deref(), Some("floor"));
    }

    /// A pinned mode's tier only fills an unset `main_variant`: the user's own variant wins,
    /// and with no tier the resolution is today's.
    #[test]
    fn a_provider_tier_defaults_only_an_unset_main_variant() {
        use distill_sampler::ProviderTier::{Economy, Fast};
        for raw in [
            None,
            Some(""),
            Some("none"),
            Some("nitro"),
            Some("exacto"),
            Some("bogus"),
        ] {
            assert_eq!(
                resolve_main_variant_for_tier(raw, None),
                resolve_openrouter_variant(raw),
                "{raw:?}"
            );
        }
        assert_eq!(
            resolve_main_variant_for_tier(None, Some(Fast)).as_deref(),
            Some("nitro")
        );
        assert_eq!(
            resolve_main_variant_for_tier(Some(" "), Some(Fast)).as_deref(),
            Some("nitro")
        );
        assert_eq!(
            resolve_main_variant_for_tier(None, Some(Economy)).as_deref(),
            Some("floor")
        );
        assert_eq!(
            resolve_main_variant_for_tier(Some("exacto"), Some(Fast)).as_deref(),
            Some("exacto")
        );
        assert_eq!(
            resolve_main_variant_for_tier(Some("none"), Some(Fast)),
            None
        );
        assert_eq!(
            resolve_main_variant_for_tier(Some("nitro"), Some(Economy)).as_deref(),
            Some("nitro")
        );
    }

    #[tokio::test]
    async fn telemetry_scope_isolates_turns_and_numbers_rounds() {
        let first = with_session_scope("one", async {
            begin_model_round();
            let first = telemetry_context();
            begin_model_round();
            assert_eq!(telemetry_context().2, 2);
            assert_eq!(telemetry_context().1, first.1);
            first
        })
        .await;
        let second = with_session_scope("one", async {
            begin_model_round();
            telemetry_context()
        })
        .await;
        assert_eq!(first.0, "one");
        assert_eq!(first.2, 1);
        assert_eq!(second.2, 1);
        assert_ne!(first.1, second.1);
        assert_eq!(telemetry_context(), (String::new(), String::new(), 0));
    }

    #[tokio::test]
    async fn decision_log_contains_numeric_round_and_session_correlation() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let writer = file.reopen().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.try_clone().unwrap())
            .finish();
        with_session_scope("correlation-test", async {
            begin_model_round();
            tracing::subscriber::with_default(subscriber, || {
                record_item(
                    distill_workspace::jev::flags::JevLever::C4DiffRisk,
                    "review:ok",
                    "test",
                    None,
                    None,
                );
            });
        })
        .await;
        let text = std::fs::read_to_string(file.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(value["fields"]["session_id"], "correlation-test");
        assert_eq!(value["fields"]["round_id"], 1);
        assert!(!value["fields"]["turn_id"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batched_items_keep_flags_ids_and_count_usage_once() {
        use distill_workspace::jev::flags::JevLever;
        use distill_workspace::jev::types::{Answer, JevAnswerSet, Question, Usage};
        let mut flags = JevFlags::harness_default();
        flags.c6_injection_screen = true;
        flags.c5_error_priority = false;
        let pack = || {
            Some(
                [("same_id".to_owned(), Question::noul("Is this actionable?"))]
                    .into_iter()
                    .collect(),
            )
        };
        set_test_decision_answers([Some(JevAnswerSet {
            model: "test".to_owned(),
            answers: [
                ("0:same_id".to_owned(), Answer::Noul { noul: 0.9 }),
                ("2:same_id".to_owned(), Answer::Noul { noul: 0.7 }),
            ]
            .into_iter()
            .collect(),
            usage: Usage {
                input_tokens: Some(500),
                output_tokens: Some(30),
            },
            latency_ms: 100,
            request_id: Some("shared-request".to_owned()),
        })]);
        let [first, disabled, last] = ask_items_with_flags(
            serde_json::json!({}),
            [
                (JevLever::C4DiffRisk, pack()),
                (JevLever::C5ErrorPriority, pack()),
                (JevLever::C6InjectionScreen, pack()),
            ],
            &flags,
        )
        .await;
        let first = first.expect("first pack answered");
        let last = last.expect("last pack answered");
        assert!(disabled.is_none());
        assert_eq!(first.noul("same_id"), Some(0.9));
        assert_eq!(last.noul("same_id"), Some(0.7));
        assert_eq!(first.usage.input() + last.usage.input(), 500);
        assert_eq!(first.usage.output() + last.usage.output(), 30);
        assert_eq!(first.request_id, last.request_id);
        assert_eq!(test_decision_answers_remaining(), 0);
        flags.enabled = false;
        set_test_decision_answers([None]);
        assert!(
            ask_items_with_flags(
                serde_json::json!({}),
                [(JevLever::C4DiffRisk, pack()),],
                &flags
            )
            .await[0]
                .is_none()
        );
        assert_eq!(
            test_decision_answers_remaining(),
            1,
            "disabled packs never ask"
        );
        clear_test_decision_answers();
    }

    /// B6 runs in shadow: it joins a request an active pack already makes and
    /// never costs a request of its own.
    #[tokio::test]
    async fn a_shadow_pack_rides_along_but_never_asks_alone() {
        use distill_workspace::jev::flags::JevLever;
        use distill_workspace::jev::types::{Answer, JevAnswerSet, Question};
        // B6 is the only lever asked in shadow; switch its hint off to test that path.
        let flags = JevFlags {
            b6_delegation_hint: false,
            ..JevFlags::harness_default()
        };
        let pack = || {
            Some(
                [("q".to_owned(), Question::noul("Is it?"))]
                    .into_iter()
                    .collect(),
            )
        };
        set_test_decision_answers([None]);
        let [alone] = ask_items_with_flags(
            serde_json::json!({}),
            [(JevLever::B6DelegationHint, pack())],
            &flags,
        )
        .await;
        assert!(alone.is_none());
        assert_eq!(test_decision_answers_remaining(), 1, "no request of its own");
        set_test_decision_answers([Some(JevAnswerSet {
            model: "test".to_owned(),
            answers: [
                ("0:q".to_owned(), Answer::Noul { noul: 0.2 }),
                ("1:q".to_owned(), Answer::Noul { noul: 0.8 }),
            ]
            .into_iter()
            .collect(),
            usage: Default::default(),
            latency_ms: 1,
            request_id: None,
        })]);
        let [active, shadow] = ask_items_with_flags(
            serde_json::json!({}),
            [
                (JevLever::B1IntentRouting, pack()),
                (JevLever::B6DelegationHint, pack()),
            ],
            &flags,
        )
        .await;
        assert_eq!(active.and_then(|a| a.noul("q")), Some(0.2));
        assert_eq!(shadow.and_then(|a| a.noul("q")), Some(0.8));
        clear_test_decision_answers();
    }

    #[test]
    fn jev_config_cannot_enable_permission_decisions() {
        let config: JevConfig = toml::from_str(
            r#"enabled = true
[ladder]
permission_classifier = true
yolo_veto = true
p5_call_validation = true
e_cheap_task = true
e_lane_choice = true
e_breaker = true
a3_log_lines = true
c2_failure_triage = true
c7_change_type = true"#,
        )
        .unwrap();
        let flags = flags_from_tiers(&config, Some(true));
        assert!(flags.enabled && flags.p1_tool_family && flags.b1_intent_routing);
        assert_eq!(flags, flags_from_tiers(&JevConfig::default(), Some(true)));
    }

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
        assert_eq!(on.status(false).label(), "jev:no-key");

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
        let config = client_config_from(&JevConfig::default()).expect("defaults valid");
        assert_eq!(config.base_url, "https://openrouter.ai/api");
        assert_eq!(config.model, "~typesafe/jev-latest");
        assert_eq!(config.api_key_env, "OPENROUTER_API_KEY");
        assert_eq!(config.endpoint(), "https://openrouter.ai/api/alpha/decisions");
    }

    #[test]
    fn config_rejects_legacy_provider_host_and_key() {
        for cfg in [
            JevConfig { provider: Some("typesafe".into()), ..JevConfig::default() },
            JevConfig { provider: Some("openrouter".into()), ..JevConfig::default() },
            JevConfig { provider: Some("bogus".into()), ..JevConfig::default() },
            JevConfig { base_url: Some("https://api.typesafe.ai".into()), ..JevConfig::default() },
            JevConfig { api_key_env: Some("JEV_API_KEY".into()), ..JevConfig::default() },
        ] { assert!(client_config_from(&cfg).is_err()); }
    }

    #[tokio::test]
    async fn partial_workspace_usage_keeps_known_tokens_and_marks_incomplete() {
        use distill_workspace::jev::types::{AttemptRecord, AttemptStatus, Usage, UsageBilling};

        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let recorder = distill_chat_state::ChatStateActor::spawn(
            Vec::new(),
            distill_sampling_types::SamplingConfig::default(),
            Box::new(distill_chat_state::NullChatPersistence),
            event_tx,
            cancellation.clone(),
        );

        record_workspace_attempt(
            AttemptRecord {
                attempt_id: "partial-attempt".to_owned(),
                request_id: Some("request-1".to_owned()),
                requested_model: "jev-model".to_owned(),
                response_model: Some("jev-model".to_owned()),
                endpoint: "https://api.typesafe.ai/v1/systemone".to_owned(),
                requested_effort: Some("none".to_owned()),
                applied_effort: Some("absent".to_owned()),
                usage: Some(Usage {
                    input_tokens: Some(7),
                    output_tokens: None,
                }),
                billing: UsageBilling {
                    cost_usd_ticks: Some(0),
                    ..UsageBilling::default()
                },
                reason: None,
                bytes_in: None,
                bytes_out: None,
                status: AttemptStatus::Completed,
                latency_ms: 3,
            },
            "jev",
            Some("routing".to_owned()),
            Some("turn-1".to_owned()),
            recorder.clone(),
            false,
        );

        let ledger = recorder
            .try_get_session_usage()
            .await
            .expect("chat-state actor must acknowledge the usage query");
        assert_eq!(ledger.totals.input_tokens, 7);
        assert_eq!(ledger.totals.output_tokens, 0);
        assert_eq!(ledger.totals.model_calls, 1);
        assert_eq!(ledger.totals.cost_usd_ticks, Some(0));
        assert_eq!(ledger.totals.cost_missing_calls, 0);
        assert!(ledger.incomplete);
        assert!(!ledger
            .attributions
            .first()
            .expect("partial attempt attribution")
            .usage_complete);

        cancellation.cancel();
    }
}

// ---------------------------------------------------------------------------
// Runtime helper for catalogue call sites (todo.md areas A–D)
// ---------------------------------------------------------------------------

/// Bump when the meaning of a cached Jev decision changes. The descriptor is
/// hashed before it reaches the client memo, so user state and question text do
/// not remain in the cache key.
const DECISION_MEMO_VERSION: u8 = 1;

fn decision_memo_key(
    client: &distill_workspace::jev::JevClient,
    lever: distill_workspace::jev::flags::JevLever,
    state: &serde_json::Value,
    questions: &std::collections::BTreeMap<
        String,
        distill_workspace::jev::types::Question,
    >,
) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};

    let config = client.config();
    let state_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(state).ok()?)
    );
    let questions_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(questions).ok()?)
    );
    let descriptor = serde_json::json!({
        "version": DECISION_MEMO_VERSION,
        "session_id": active_session_id(),
        "lever": lever.as_str(),
        "state_sha256": state_hash,
        "questions_sha256": questions_hash,
        "endpoint": config.endpoint(),
        "model": config.model,
        "provider": config.provider.as_str(),
        "reasoning_shape": serde_json::to_value(config.reasoning_shape).ok()?,
        "reasoning_effort": config.reasoning_effort,
        "max_completion_tokens": config.max_completion_tokens,
        "max_state_bytes": config.max_state_bytes,
        "timeout_ms": config.timeout.as_millis(),
        "item_budget_ms": config.item_budget.as_millis(),
    });
    let bytes = serde_json::to_vec(&descriptor).ok()?;
    Some(Sha256::digest(bytes).into())
}

/// Ask independent packs about the same state once. Each pack keeps its flag
/// and answer ids. The first active pack owns the request's usage and latency;
/// subsequent decision records carry zero usage so the total is counted once.
/// A pack that only runs in shadow (B6) rides along on a request an active
/// pack makes and never sends one alone.
pub async fn ask_items<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
) -> [Option<distill_workspace::jev::types::JevAnswerSet>; N] {
    ask_packs(state, items, None, &flags_cached()).await.0
}

/// [`ask_items`] plus, with `[jev] shadow_packs` on, one shadow pack that
/// joins the same request on the same state. The shadow pack never makes a
/// request alone and never owns its usage; its answers come back second, for
/// the caller to record with a `shadow:` label and never act on. With the
/// switch off, or `shadow` `None`, the request is the one [`ask_items`] sends.
pub async fn ask_items_with_shadow<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
    shadow: Option<(
        distill_workspace::jev::flags::JevLever,
        std::collections::BTreeMap<String, distill_workspace::jev::types::Question>,
    )>,
) -> (
    [Option<distill_workspace::jev::types::JevAnswerSet>; N],
    Option<distill_workspace::jev::types::JevAnswerSet>,
) {
    let shadow = shadow.filter(|_| crate::jev_shadow::packs_on());
    ask_packs(state, items, shadow, &flags_cached()).await
}

async fn ask_items_with_flags<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
    flags: &JevFlags,
) -> [Option<distill_workspace::jev::types::JevAnswerSet>; N] {
    ask_packs(state, items, None, flags).await.0
}

/// Shadow question ids are `shadow:<id>`, after every `<index>:` key.
const SHADOW_KEY_PREFIX: &str = "shadow:";

async fn ask_packs<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
    shadow: Option<(
        distill_workspace::jev::flags::JevLever,
        std::collections::BTreeMap<String, distill_workspace::jev::types::Question>,
    )>,
    flags: &JevFlags,
) -> (
    [Option<distill_workspace::jev::types::JevAnswerSet>; N],
    Option<distill_workspace::jev::types::JevAnswerSet>,
) {
    use distill_workspace::jev::types::Usage;
    use std::collections::BTreeMap;

    let mut questions = BTreeMap::new();
    let mut first = None;
    let mut ids: [Vec<String>; N] = std::array::from_fn(|_| Vec::new());
    // Each pack's own lever and question hash, so its decision event matches
    // the hash a solo ask of the same pack would carry.
    let mut traced: [Option<(distill_workspace::jev::flags::JevLever, String)>; N] =
        std::array::from_fn(|_| None);
    for (index, (lever, pack)) in items.into_iter().enumerate() {
        let Some(pack) = pack.filter(|pack| !pack.is_empty()) else {
            continue;
        };
        if !flags.lever_asks(lever) {
            emit_no_answer(
                lever,
                distill_session_events::JevNoAnswerKind::LeverOff,
                Some(question_hash(&pack)),
                0,
            );
            continue;
        }
        if let Some(slot) = traced.get_mut(index) {
            *slot = Some((lever, question_hash(&pack)));
        }
        for (id, question) in pack {
            if flags.lever_active(lever) {
                first.get_or_insert(lever);
            }
            questions.insert(format!("{index}:{id}"), question);
            ids[index].push(id);
        }
    }
    let mut result = std::array::from_fn(|_| None);
    let Some(lever) = first else {
        return (result, None);
    };
    // Rides only a request a live pack makes; its failure is that request's
    // no-answer event, already emitted for the live packs.
    let shadow = shadow
        .filter(|(lever, pack)| !pack.is_empty() && flags.lever_asks(*lever))
        .map(|(lever, pack)| {
            let hash = question_hash(&pack);
            let ids: Vec<String> = pack.keys().cloned().collect();
            for (id, question) in pack {
                questions.insert(format!("{SHADOW_KEY_PREFIX}{id}"), question);
            }
            (lever, hash, ids)
        });
    let (mut answers, memo) = match ask_once(lever, state, questions).await {
        Ok(answered) => answered,
        Err((kind, latency_ms)) => {
            for (lever, hash) in traced.into_iter().flatten() {
                emit_no_answer(lever, kind, Some(hash), latency_ms);
            }
            return (result, None);
        }
    };
    let shadow = shadow.map(|(lever, hash, ids)| {
        let mut pack = answers.clone();
        pack.answers = ids
            .into_iter()
            .filter_map(|id| {
                let answer = answers
                    .answers
                    .get(&format!("{SHADOW_KEY_PREFIX}{id}"))
                    .cloned();
                answer.map(|answer| (id, answer))
            })
            .collect();
        pack.usage = Usage::default();
        pack.latency_ms = 0;
        note_answer_trace(lever, &pack, hash, memo);
        pack
    });
    for (index, ids) in ids.into_iter().enumerate() {
        if ids.is_empty() {
            continue;
        }
        let mut pack = answers.clone();
        pack.answers = ids
            .into_iter()
            .filter_map(|id| {
                answers
                    .answers
                    .get(&format!("{index}:{id}"))
                    .cloned()
                    .map(|answer| (id, answer))
            })
            .collect();
        if let Some((lever, hash)) = traced.get_mut(index).and_then(Option::take) {
            note_answer_trace(lever, &pack, hash, memo);
        }
        result[index] = Some(pack);
        answers.usage = Usage::default();
        answers.latency_ms = 0;
    }
    (result, shadow)
}

/// Drains the serialized state and questions of the requests answered from
/// the test queue.
#[cfg(test)]
pub(crate) fn take_test_asked_bodies() -> Vec<String> {
    TEST_ASKED_BODIES.with(|asked| std::mem::take(&mut *asked.borrow_mut()))
}

#[cfg(test)]
mod shadow_pack_tests {
    use super::*;
    use distill_workspace::jev::flags::JevLever;
    use distill_workspace::jev::types::{Answer, JevAnswerSet, Question};
    use std::collections::BTreeMap;

    fn pack(id: &str) -> BTreeMap<String, Question> {
        [(id.to_owned(), Question::noul(format!("Is {id} so?")))]
            .into_iter()
            .collect()
    }

    fn answers(entries: &[(&str, f64)]) -> JevAnswerSet {
        JevAnswerSet {
            model: "test".to_owned(),
            answers: entries
                .iter()
                .map(|(id, noul)| ((*id).to_owned(), Answer::Noul { noul: *noul }))
                .collect(),
            usage: Default::default(),
            latency_ms: 7,
            request_id: None,
        }
    }

    fn reset() {
        clear_test_decision_answers();
        take_test_asked_bodies();
        clear_test_flags();
        crate::jev_shadow::set_test_switches(None, None);
    }

    /// With `shadow_packs` off a caller's shadow pack is dropped and the live
    /// request is byte for byte the one `ask_items` sends.
    #[tokio::test]
    async fn with_shadow_packs_off_the_live_request_is_unchanged() {
        reset();
        set_test_flags(JevFlags::harness_default());
        crate::jev_shadow::set_test_switches(Some(false), None);
        let state = serde_json::json!({"request": "synthetic"});
        let items = || {
            [
                (JevLever::C4DiffRisk, Some(pack("live"))),
                (JevLever::C4DiffRisk, Some(pack("kind"))),
            ]
        };
        let reply = answers(&[("0:live", 0.2), ("1:kind", 0.3)]);
        set_test_decision_answers([Some(reply.clone()), Some(reply)]);
        let plain = ask_items(state.clone(), items()).await;
        let (offered, shadow) =
            ask_items_with_shadow(state, items(), Some((JevLever::C4DiffRisk, pack("extra"))))
                .await;
        let bodies = take_test_asked_bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies[0], bodies[1],
            "same bytes as before shadow packs existed"
        );
        assert!(!bodies[1].contains("shadow:"));
        assert!(shadow.is_none());
        assert_eq!(plain, offered);
        reset();
    }

    /// On, the shadow pack joins the same request and comes back apart: the
    /// live packs see only their own answers and keep the usage.
    #[tokio::test]
    async fn a_shadow_pack_rides_the_live_request_and_comes_back_apart() {
        reset();
        set_test_flags(JevFlags::harness_default());
        crate::jev_shadow::set_test_switches(Some(true), None);
        set_test_decision_answers([Some(answers(&[("0:live", 0.2), ("shadow:extra", 0.9)]))]);
        let ([live], shadow) = ask_items_with_shadow(
            serde_json::json!({}),
            [(JevLever::C4DiffRisk, Some(pack("live")))],
            Some((JevLever::C4DiffRisk, pack("extra"))),
        )
        .await;
        assert_eq!(
            take_test_asked_questions(),
            vec![vec!["0:live".to_owned(), "shadow:extra".to_owned()]],
            "one request"
        );
        let live = live.expect("live answers");
        assert_eq!(live.answers.len(), 1);
        assert_eq!(live.noul("live"), Some(0.2));
        assert_eq!(live.latency_ms, 7, "the live pack owns the request");
        let shadow = shadow.expect("shadow answers");
        assert_eq!(shadow.answers.len(), 1);
        assert_eq!(shadow.noul("extra"), Some(0.9));
        assert_eq!(shadow.latency_ms, 0);
        reset();
    }

    /// A shadow pack never costs a request: with no active live pack nothing
    /// is sent, even with the switch on.
    #[tokio::test]
    async fn a_shadow_pack_never_sends_a_request_alone() {
        reset();
        set_test_flags(JevFlags {
            c4_diff_risk: false,
            ..JevFlags::harness_default()
        });
        crate::jev_shadow::set_test_switches(Some(true), None);
        set_test_decision_answers([None]);
        let ([live], shadow) = ask_items_with_shadow(
            serde_json::json!({}),
            [(JevLever::C4DiffRisk, Some(pack("live")))],
            Some((JevLever::B8ExecutionMode, pack("mode"))),
        )
        .await;
        let ([none], alone) = ask_items_with_shadow(
            serde_json::json!({}),
            [(JevLever::P6SkillSuggestion, None)],
            Some((JevLever::B8ExecutionMode, pack("mode"))),
        )
        .await;
        assert!(live.is_none() && shadow.is_none() && none.is_none() && alone.is_none());
        assert_eq!(test_decision_answers_remaining(), 1, "no request was sent");
        reset();
    }

    /// A calibration entry records the hash the decision events carry.
    #[test]
    fn the_calibration_hash_is_the_event_hash() {
        let items = ["user request".to_owned()];
        for questions in [
            pack("x"),
            distill_workspace::jev::catalog::verify::premature_stop_questions(&items)
                .expect("catalog"),
        ] {
            assert_eq!(
                question_hash(&questions),
                distill_workspace::jev::catalog::thresholds::question_hash(&questions)
            );
        }
    }
}

/// Runs **one** catalogue decision.
///
/// Every gate lives here so no call site can forget one: the master switch and
/// the item's own flag (`JevLever`), a resolvable credential, a single attempt
/// inside a bounded budget, and `None` on anything else — which means the caller
/// keeps today's behaviour (fail-defer, invariant I-5 of the plan).
pub async fn ask_item(
    lever: distill_workspace::jev::flags::JevLever,
    state: serde_json::Value,
    questions: std::collections::BTreeMap<
        distill_workspace::jev::types::QuestionId,
        distill_workspace::jev::types::Question,
    >,
) -> Option<distill_workspace::jev::types::JevAnswerSet> {
    if questions.is_empty() {
        return None;
    }
    let hash = question_hash(&questions);
    match ask_once(lever, state, questions).await {
        Ok((answers, memo)) => {
            note_answer_trace(lever, &answers, hash, memo);
            Some(answers)
        }
        Err((kind, latency_ms)) => {
            emit_no_answer(lever, kind, Some(hash), latency_ms);
            None
        }
    }
}

/// One gated attempt for [`ask_item`] and [`ask_items`]: the answers and
/// whether they came from the memo, or why there are none and how long the
/// attempt took.
async fn ask_once(
    lever: distill_workspace::jev::flags::JevLever,
    state: serde_json::Value,
    questions: std::collections::BTreeMap<
        distill_workspace::jev::types::QuestionId,
        distill_workspace::jev::types::Question,
    >,
) -> Result<
    (distill_workspace::jev::types::JevAnswerSet, bool),
    (distill_session_events::JevNoAnswerKind, u64),
> {
    use distill_session_events::JevNoAnswerKind;
    // A scripted `None` stands for a failed request.
    #[cfg(test)]
    if let Some(answer) = TEST_DECISION_ANSWERS.with(|queue| {
        queue.borrow_mut().as_mut().map(|answers| {
            TEST_ASKED_QUESTIONS
                .with(|asked| asked.borrow_mut().push(questions.keys().cloned().collect()));
            TEST_ASKED_BODIES.with(|asked| {
                let body = serde_json::json!({"state": &state, "questions": &questions});
                asked.borrow_mut().push(body.to_string());
            });
            TEST_ASK_LOG.with(|log| {
                log.borrow_mut().push(TestAsk {
                    lever: lever.as_str(),
                    detached: DETACHED_DECISION.try_with(|_| ()).is_ok(),
                    dispatched_before: TEST_DISPATCHES.with(std::cell::Cell::get),
                });
            });
            answers.pop_front().unwrap_or(None)
        })
    }) {
        return answer
            .map(|answer| (answer, false))
            .ok_or((JevNoAnswerKind::Error, 0));
    }
    let flags = flags_cached();
    if !flags.lever_active(lever) {
        return Err((JevNoAnswerKind::LeverOff, 0));
    }
    let Some(client) = client_cached().filter(|client| client.credential_present()) else {
        return Err((JevNoAnswerKind::NoCredential, 0));
    };
    crate::jev_shadow::capture(lever, &client.config().model, &state, &questions);
    let session_id = active_session_id();
    let memo_key = decision_memo_key(client, lever, &state, &questions);
    let observed_client = observed_client(client, lever.as_str());
    let request_client = observed_client.as_ref().unwrap_or(client);
    let budget = item_budget(request_client);
    let started = std::time::Instant::now();
    let in_flight = JevInFlight::begin();
    let outcome = match memo_key {
        Some(key) => {
            tokio::time::timeout(
                budget,
                request_client.ask_memoized(&session_id, key, &state, &questions),
            )
            .await
        }
        None => tokio::time::timeout(budget, request_client.ask(&state, &questions)).await,
    };
    drop(in_flight);
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match outcome {
        // A memo hit comes back with its latency zeroed; a request never takes 0 ms.
        Ok(Ok(answers)) => {
            let memo = memo_key.is_some() && answers.latency_ms == 0;
            Ok((answers, memo))
        }
        Ok(Err(error)) => {
            tracing::debug!(
                lever = lever.as_str(),
                %error,
                "jev item call failed; keeping the current path"
            );
            // A silent failure is undiagnosable: the record says the item ran
            // and what the endpoint said, without ever logging bodies. The
            // session event is the caller's no-answer event, not a decision.
            log_item(lever, "error", &format!("{error}"), None, None);
            Err((JevNoAnswerKind::Error, latency_ms))
        }
        Err(_) => {
            tracing::debug!(
                lever = lever.as_str(),
                budget_ms = budget.as_millis() as u64,
                "jev item call timed out; keeping the current path"
            );
            log_item(
                lever,
                "timeout",
                &format!("budget {} ms", budget.as_millis()),
                None,
                None,
            );
            Err((JevNoAnswerKind::Timeout, latency_ms))
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Answers consumed by the real `ask_item` path in focused routing tests.
    /// Keeping this test-only and task-local avoids changing production routing
    /// or sharing decisions between concurrent test sessions.
    static TEST_DECISION_ANSWERS: std::cell::RefCell<
        Option<std::collections::VecDeque<Option<distill_workspace::jev::types::JevAnswerSet>>>,
    > = const { std::cell::RefCell::new(None) };
    /// Question ids of each request that consumed one of those answers.
    static TEST_ASKED_QUESTIONS: std::cell::RefCell<Vec<Vec<String>>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The serialized state and questions of each such request.
    static TEST_ASKED_BODIES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn set_test_decision_answers(
    answers: impl IntoIterator<Item = Option<distill_workspace::jev::types::JevAnswerSet>>,
) {
    TEST_DECISION_ANSWERS.with(|queue| {
        *queue.borrow_mut() = Some(answers.into_iter().collect());
    });
}

#[cfg(test)]
pub(crate) fn clear_test_decision_answers() {
    TEST_DECISION_ANSWERS.with(|queue| *queue.borrow_mut() = None);
    TEST_ASKED_QUESTIONS.with(|asked| asked.borrow_mut().clear());
}

/// Drains the question ids of the requests answered from the test queue.
#[cfg(test)]
pub(crate) fn take_test_asked_questions() -> Vec<Vec<String>> {
    TEST_ASKED_QUESTIONS.with(|asked| std::mem::take(&mut *asked.borrow_mut()))
}

#[cfg(test)]
pub(crate) fn test_decision_answers_remaining() -> usize {
    TEST_DECISION_ANSWERS.with(|queue| {
        queue
            .borrow()
            .as_ref()
            .map_or(0, std::collections::VecDeque::len)
    })
}

tokio::task_local! {
    /// Set on a decision started by [`in_current_turn`]: nothing awaits it.
    static DETACHED_DECISION: ();
}

/// `future` scoped to the current session, turn, round and usage recorder,
/// for a decision a detached task runs while the turn goes on: its records
/// land in the same turn as an awaited decision's would.
pub(crate) fn in_current_turn<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let session = active_session_id();
    let turn = ACTIVE_TURN_ID.try_with(Clone::clone).unwrap_or_default();
    let round = ACTIVE_ROUND_ID
        .try_with(std::cell::Cell::get)
        .unwrap_or_default();
    ACTIVE_SESSION_ID.scope(
        session,
        ACTIVE_TURN_ID.scope(
            turn,
            ACTIVE_USAGE_RECORDER.scope(
                std::cell::RefCell::new(active_usage_recorder()),
                ACTIVE_ROUND_ID.scope(
                    std::cell::Cell::new(round),
                    DETACHED_DECISION.scope((), future),
                ),
            ),
        ),
    )
}

/// One request answered from the test queue: its lever, whether a detached
/// task asked it, and how many main requests had been dispatched before it.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TestAsk {
    pub(crate) lever: &'static str,
    pub(crate) detached: bool,
    pub(crate) dispatched_before: u64,
}

#[cfg(test)]
thread_local! {
    static TEST_ASK_LOG: std::cell::RefCell<Vec<TestAsk>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Main requests dispatched on this thread (see [`note_route`]).
    static TEST_DISPATCHES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Drains the asks answered from the test queue since the last call and
/// restarts the dispatch count.
#[cfg(test)]
pub(crate) fn take_test_ask_log() -> Vec<TestAsk> {
    TEST_DISPATCHES.with(|count| count.set(0));
    TEST_ASK_LOG.with(|log| std::mem::take(&mut *log.borrow_mut()))
}

/// Records one catalogue decision so it lands in `~/.grok/logs/jev.jsonl`
/// through the shared decision sink and log target, and as a content-free
/// `jev_decision` event in the session's `events.jsonl`.
pub fn record_item(
    lever: distill_workspace::jev::flags::JevLever,
    decision: &str,
    reason: &str,
    confidence: Option<f64>,
    answers: Option<&distill_workspace::jev::types::JevAnswerSet>,
) {
    log_item(lever, decision, reason, confidence, answers);
    let opaque = options_from_user_content(lever);
    // A6's label is the chosen test, apart from its one fixed fallback.
    let label = if lever == distill_workspace::jev::flags::JevLever::A6TestToRun
        && decision != "default_suite"
    {
        opaque_id(decision)
    } else {
        event_id(decision)
    };
    count_jev_outcome(lever, &label);
    // `reason` stays out of the event: it carries paths and model names.
    let (question_hash, memo) = answers
        .and_then(|answers| answer_trace(lever, answers))
        .map_or((None, None), |(hash, memo)| (Some(hash), Some(memo)));
    emit_jev_event(|seq, round| distill_session_events::Event::JevDecision {
        seq,
        lever: lever.as_str(),
        question_hash,
        model_id: answers
            .map(|answers| answers.model.clone())
            .filter(|model| !model.is_empty()),
        answers: answers.map_or_else(Default::default, |answers| {
            answers
                .answers
                .iter()
                .map(|(id, answer)| (event_id(id), answer_summary(answer, opaque)))
                .collect()
        }),
        label,
        confidence,
        memo,
        latency_ms: answers.map_or(0, |answers| answers.latency_ms),
        round,
    });
}

/// Records one enforcement of a per-mode policy rule: counted under
/// `utilityOutcomes["policy:<mode>"]` in usage.json, kept for the turn
/// outcome record, and written as a content-free `mode_policy` event. `rule`
/// is a short identifier; anything else is replaced by its hash.
pub(crate) fn record_policy(mode: distill_tool_types::ExecutionMode, rule: &str) {
    let rule = event_id(rule);
    if let Some(recorder) = active_usage_recorder() {
        recorder.record_utility_outcome(&format!("policy:{}", mode.id()), &rule, 0, 0, 0);
    }
    crate::session::acp_session::mode_policy::note_policy_row(&active_session_id(), mode, &rule);
    emit_jev_event(|seq, round| distill_session_events::Event::ModePolicy {
        seq,
        mode: mode.id(),
        rule,
        round,
    });
}

/// The decision log and turn-status row half of [`record_item`].
fn log_item(
    lever: distill_workspace::jev::flags::JevLever,
    decision: &str,
    reason: &str,
    confidence: Option<f64>,
    answers: Option<&distill_workspace::jev::types::JevAnswerSet>,
) {
    use distill_workspace::jev::policy::{DecisionRecord, DecisionSink};
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
        session_id: None,
        turn_id: None,
        round_id: None,
        escalated: false,
    };
    ActivitySink.record(&record);
}

#[cfg(test)]
mod decision_event_tests {
    use super::*;
    use distill_workspace::jev::flags::JevLever;
    use distill_workspace::jev::types::{Answer, JevAnswerSet, Question};
    use std::collections::BTreeMap;

    /// Runs `body` as a turn of a session whose `events.jsonl` is in `dir`
    /// and returns the events it wrote.
    async fn session_events<F: Future<Output = ()>>(
        dir: &std::path::Path,
        session_id: &str,
        body: F,
    ) -> Vec<serde_json::Value> {
        let writer = distill_session_events::EventWriter::open(dir);
        register_session_events(session_id, &writer);
        with_session_scope(session_id, body).await;
        drop(writer);
        std::fs::read_to_string(dir.join("events.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("event line is JSON"))
            .collect()
    }

    fn answer_set(answers: Vec<(&str, Answer)>, latency_ms: u64) -> JevAnswerSet {
        JevAnswerSet {
            model: "jev-test".to_owned(),
            answers: answers
                .into_iter()
                .map(|(id, answer)| (id.to_owned(), answer))
                .collect(),
            usage: Default::default(),
            latency_ms,
            request_id: Some("request".to_owned()),
        }
    }

    fn mode_questions() -> BTreeMap<String, Question> {
        BTreeMap::from([
            (
                "mode".to_owned(),
                Question::choice(
                    "Which mode fits?",
                    BTreeMap::from([
                        ("normal".to_owned(), serde_json::Value::Null),
                        ("ultrafast".to_owned(), serde_json::Value::Null),
                    ]),
                )
                .expect("valid choice"),
            ),
            ("risk".to_owned(), Question::noul("Is it risky?")),
        ])
    }

    /// Each `record_item` is one event: the answer the call site acted on,
    /// with the question it answered, so a threshold can be judged later.
    #[tokio::test]
    #[serial_test::serial]
    async fn every_recorded_decision_is_one_event_with_its_answers() {
        clear_test_decision_answers();
        let dir = tempfile::tempdir().unwrap();
        let questions = mode_questions();
        let expected_hash = question_hash(&questions);
        set_test_decision_answers([Some(answer_set(
            vec![
                (
                    "mode",
                    Answer::Choice {
                        choice: "normal".to_owned(),
                        probabilities: BTreeMap::from([
                            ("normal".to_owned(), 0.75),
                            ("ultrafast".to_owned(), 0.25),
                        ]),
                        confidence: Some(0.5),
                    },
                ),
                ("risk", Answer::Noul { noul: 0.125 }),
            ],
            120,
        ))]);
        let events = session_events(dir.path(), "decision-event-session", async {
            begin_model_round();
            let answers = ask_item(JevLever::B8ExecutionMode, serde_json::json!({}), questions)
                .await
                .expect("scripted answer");
            record_item(
                JevLever::B8ExecutionMode,
                "normal",
                "request start",
                Some(0.5),
                Some(&answers),
            );
            record_item(JevLever::B8ExecutionMode, "defer", "no answer", None, None);
        })
        .await;
        clear_test_decision_answers();

        let [decided, deferred] = events.as_slice() else {
            panic!("one event per record_item: {events:?}");
        };
        let lever = JevLever::B8ExecutionMode.as_str();
        assert_eq!(decided["type"], "jev_decision");
        assert_eq!(decided["seq"], 0);
        assert_eq!(decided["lever"], lever);
        assert_eq!(decided["question_hash"], expected_hash.as_str());
        assert_eq!(expected_hash.len(), 12);
        assert_eq!(decided["model_id"], "jev-test");
        assert_eq!(decided["answers"]["mode"]["choice"], "normal");
        assert_eq!(
            decided["answers"]["mode"]["probabilities"]["ultrafast"],
            0.25
        );
        assert_eq!(decided["answers"]["mode"]["confidence"], 0.5);
        assert_eq!(decided["answers"]["risk"]["noul"], 0.125);
        assert_eq!(decided["label"], "normal");
        assert_eq!(decided["confidence"], 0.5);
        assert_eq!(decided["memo"], false);
        assert_eq!(decided["latency_ms"], 120);
        assert_eq!(decided["round"], 1);

        // A decision taken without an answer is still one event.
        assert_eq!(deferred["seq"], 1);
        assert_eq!(deferred["label"], "defer");
        assert!(deferred.get("answers").is_none());
        assert!(deferred.get("question_hash").is_none());
        assert!(
            deferred.get("memo").is_none(),
            "provenance unknown, not `false`"
        );
    }

    /// State, question text and reason carry user content (paths, model
    /// names); none of it may reach the event file, and path-like ids are
    /// hashed.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_state_question_text_or_reason_reaches_an_event() {
        const SENTINEL: &str = "SENTINEL7f3a";
        clear_test_decision_answers();
        let dir = tempfile::tempdir().unwrap();
        let path_option = format!("tests/{SENTINEL}.rs::case");
        let mut options: BTreeMap<String, serde_json::Value> = (0..9)
            .map(|index| (format!("option{index}"), serde_json::json!(SENTINEL)))
            .collect();
        options.insert(path_option.clone(), serde_json::json!(SENTINEL));
        let questions = BTreeMap::from([(
            "test_to_run".to_owned(),
            Question::choice(format!("Which test? {SENTINEL}"), options.clone())
                .expect("valid choice"),
        )]);
        let probabilities: BTreeMap<String, f64> = options
            .keys()
            .enumerate()
            .map(|(rank, id)| (id.clone(), 1.0 / (rank as f64 + 2.0)))
            .collect();
        set_test_decision_answers([Some(answer_set(
            vec![(
                "test_to_run",
                Answer::Choice {
                    choice: path_option.clone(),
                    probabilities,
                    confidence: None,
                },
            )],
            7,
        ))]);
        let events = session_events(dir.path(), "sentinel-event-session", async {
            let answers = ask_item(
                JevLever::A6TestToRun,
                serde_json::json!({ "evidence": SENTINEL }),
                questions,
            )
            .await
            .expect("scripted answer");
            record_item(
                JevLever::A6TestToRun,
                &path_option,
                &format!("{SENTINEL} at /home/{SENTINEL}"),
                None,
                Some(&answers),
            );
        })
        .await;
        clear_test_decision_answers();

        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(
            !text.contains(SENTINEL),
            "content leaked into events: {text}"
        );
        let [decided] = events.as_slice() else {
            panic!("one decision event: {events:?}");
        };
        let answer = &decided["answers"]["test_to_run"];
        assert!(answer["choice"].as_str().unwrap().starts_with("h:"));
        assert_eq!(answer["probabilities"].as_object().unwrap().len(), 5);
        assert_eq!(answer["omitted"], 5);
        assert!(decided["label"].as_str().unwrap().starts_with("h:"));
    }

    /// A skill name is a short identifier, so the rule that hashes paths lets
    /// it through; the skill lever's option ids are hashed whatever they look
    /// like, and its fixed label stays readable.
    #[tokio::test]
    #[serial_test::serial]
    async fn skill_names_never_reach_an_event() {
        const SKILL: &str = "private-skill-7f3a";
        clear_test_decision_answers();
        let dir = tempfile::tempdir().unwrap();
        let options = BTreeMap::from([
            (SKILL.to_owned(), serde_json::Value::Null),
            ("other-skill".to_owned(), serde_json::Value::Null),
        ]);
        let questions = BTreeMap::from([(
            "skill".to_owned(),
            Question::choice("Which skill fits?", options).expect("valid choice"),
        )]);
        set_test_decision_answers([Some(answer_set(
            vec![(
                "skill",
                Answer::Choice {
                    choice: SKILL.to_owned(),
                    probabilities: BTreeMap::from([
                        (SKILL.to_owned(), 0.75),
                        ("other-skill".to_owned(), 0.25),
                    ]),
                    confidence: None,
                },
            )],
            9,
        ))]);
        let events = session_events(dir.path(), "skill-name-event-session", async {
            let answers = ask_item(
                JevLever::P6SkillSuggestion,
                serde_json::json!({}),
                questions,
            )
            .await
            .expect("scripted answer");
            record_item(
                JevLever::P6SkillSuggestion,
                "suggest",
                "",
                None,
                Some(&answers),
            );
        })
        .await;
        clear_test_decision_answers();

        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(
            !text.contains(SKILL) && !text.contains("other-skill"),
            "skill name leaked into events: {text}"
        );
        let [decided] = events.as_slice() else {
            panic!("one decision event: {events:?}");
        };
        assert_eq!(decided["label"], "suggest");
        let answer = &decided["answers"]["skill"];
        assert!(answer["choice"].as_str().unwrap().starts_with("h:"));
        assert!(
            answer["probabilities"]
                .as_object()
                .unwrap()
                .keys()
                .all(|id| id.starts_with("h:"))
        );
    }

    /// A failed ask is one `jev_no_answer` event per lever that asked, with
    /// no decision event of its own.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_ask_is_one_no_answer_event_per_lever() {
        clear_test_decision_answers();
        let dir = tempfile::tempdir().unwrap();
        let mut flags = JevFlags::harness_default();
        flags.c6_injection_screen = true;
        flags.c5_error_priority = false;
        let pack = || {
            Some(BTreeMap::from([(
                "same_id".to_owned(),
                Question::noul("Is this actionable?"),
            )]))
        };
        let events = session_events(dir.path(), "no-answer-event-session", async {
            set_test_decision_answers([None]);
            assert!(
                ask_item(
                    JevLever::B8ExecutionMode,
                    serde_json::json!({}),
                    mode_questions()
                )
                .await
                .is_none()
            );
            set_test_decision_answers([None]);
            let [c4, c5, c6] = ask_items_with_flags(
                serde_json::json!({}),
                [
                    (JevLever::C4DiffRisk, pack()),
                    (JevLever::C5ErrorPriority, pack()),
                    (JevLever::C6InjectionScreen, pack()),
                ],
                &flags,
            )
            .await;
            assert!(c4.is_none() && c5.is_none() && c6.is_none());
            clear_test_decision_answers();
            set_test_flags(JevFlags::default());
            assert!(
                ask_item(
                    JevLever::B8ExecutionMode,
                    serde_json::json!({}),
                    mode_questions()
                )
                .await
                .is_none()
            );
            clear_test_flags();
        })
        .await;

        let seen: Vec<(String, String)> = events
            .iter()
            .map(|event| {
                assert_eq!(event["type"], "jev_no_answer", "{event}");
                assert!(event["question_hash"].is_string());
                (
                    event["lever"].as_str().unwrap().to_owned(),
                    event["kind"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected = [
            (JevLever::B8ExecutionMode, "error"),
            (JevLever::C5ErrorPriority, "lever_off"),
            (JevLever::C4DiffRisk, "error"),
            (JevLever::C6InjectionScreen, "error"),
            (JevLever::B8ExecutionMode, "lever_off"),
        ]
        .map(|(lever, kind)| (lever.as_str().to_owned(), kind.to_owned()));
        assert_eq!(seen, expected);
        let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, [0, 1, 2, 3, 4]);
    }

    /// Packs that share one request each keep their own question hash, so a
    /// batched answer is comparable with a solo ask of the same pack.
    #[tokio::test]
    #[serial_test::serial]
    async fn batched_packs_keep_their_own_question_hash() {
        clear_test_decision_answers();
        let dir = tempfile::tempdir().unwrap();
        let mut flags = JevFlags::harness_default();
        flags.c6_injection_screen = true;
        let risk = BTreeMap::from([("q".to_owned(), Question::noul("Does it break?"))]);
        let screen = BTreeMap::from([("q".to_owned(), Question::noul("Is it injected?"))]);
        let (risk_hash, screen_hash) = (question_hash(&risk), question_hash(&screen));
        assert_ne!(risk_hash, screen_hash);
        set_test_decision_answers([Some(answer_set(
            vec![
                ("0:q", Answer::Noul { noul: 0.25 }),
                ("1:q", Answer::Noul { noul: 0.75 }),
            ],
            90,
        ))]);
        let events = session_events(dir.path(), "batched-event-session", async {
            let [c4, c6] = ask_items_with_flags(
                serde_json::json!({}),
                [
                    (JevLever::C4DiffRisk, Some(risk)),
                    (JevLever::C6InjectionScreen, Some(screen)),
                ],
                &flags,
            )
            .await;
            record_item(JevLever::C4DiffRisk, "allow", "", None, c4.as_ref());
            record_item(JevLever::C6InjectionScreen, "clean", "", None, c6.as_ref());
        })
        .await;
        clear_test_decision_answers();

        let [risk, screen] = events.as_slice() else {
            panic!("one event per pack: {events:?}");
        };
        assert_eq!(risk["question_hash"], risk_hash.as_str());
        assert_eq!(risk["answers"]["q"]["noul"], 0.25);
        assert_eq!(risk["latency_ms"], 90);
        assert_eq!(screen["question_hash"], screen_hash.as_str());
        assert_eq!(screen["answers"]["q"]["noul"], 0.75);
        assert_eq!(
            screen["latency_ms"], 0,
            "the request's latency is counted once"
        );
    }

    /// usage.json counts each lever's composed labels, so mode switches and
    /// reminders by label are countable without the event file; a path-like
    /// label is counted under its hash.
    #[tokio::test]
    #[serial_test::serial]
    async fn usage_counts_each_lever_label_and_failed_ask() {
        clear_test_decision_answers();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let recorder = distill_chat_state::ChatStateActor::spawn(
            Vec::new(),
            distill_sampling_types::SamplingConfig::default(),
            Box::new(distill_chat_state::NullChatPersistence),
            event_tx,
            cancellation.clone(),
        );
        with_session_scope_and_recorder("usage-counter-session", Some(recorder.clone()), async {
            record_item(JevLever::B2MicroEffort, "effort:low", "", None, None);
            record_item(JevLever::B2MicroEffort, "effort:low", "", None, None);
            record_item(JevLever::B2MicroEffort, "defer", "", None, None);
            record_item(JevLever::A6TestToRun, "tests/a.rs::case", "", None, None);
            set_test_decision_answers([None]);
            assert!(
                ask_item(
                    JevLever::B2MicroEffort,
                    serde_json::json!({}),
                    mode_questions()
                )
                .await
                .is_none()
            );
        })
        .await;
        clear_test_decision_answers();

        let ledger = recorder
            .try_get_session_usage()
            .await
            .expect("chat-state actor must acknowledge the usage query");
        let counts = |lever: JevLever| {
            ledger.utility_outcomes[&format!("jev:{}", lever.as_str())]
                .decisions
                .clone()
        };
        let effort = counts(JevLever::B2MicroEffort);
        assert_eq!(effort["effort:low"], 2);
        assert_eq!(effort["defer"], 1);
        assert_eq!(effort["no_answer:error"], 1);
        let tests = counts(JevLever::A6TestToRun);
        assert!(
            tests.keys().all(|label| label.starts_with("h:")),
            "{tests:?}"
        );
        cancellation.cancel();
    }

    /// Outside a registered session nothing is written and nothing fails.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_unregistered_session_records_no_event() {
        let dir = tempfile::tempdir().unwrap();
        let writer = distill_session_events::EventWriter::open(dir.path());
        with_session_scope("unregistered-event-session", async {
            record_item(JevLever::B8ExecutionMode, "defer", "", None, None);
        })
        .await;
        record_item(JevLever::B8ExecutionMode, "defer", "", None, None);
        drop(writer);
        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        assert!(text.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Activity for the turn-status row ("Jev was used here")
// ---------------------------------------------------------------------------

/// One recorded decision, as the turn-status row reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JevActivity {
    pub lever: String,
    /// `allow` | `block` | `escalate` | `rank` | `defer` | … (the recorded label).
    pub decision: String,
    pub latency_ms: u64,
    pub at: std::time::Instant,
}

/// What the row shows about Jev for one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JevTurnActivity {
    /// Decisions recorded inside the window (the current turn, when known).
    pub decisions: u32,
    /// True when a Jev optimization was refused and the normal path is retained.
    pub refused: bool,
    /// Calls in flight **right now** — Jev is being consulted for this step.
    pub in_flight: u32,
    /// Latency of the most recent decision in the window (0 without one).
    pub last_latency_ms: u64,
    /// Model calls this turn that the decision layer ran on the **local** model.
    pub local_runs: u32,
    /// Routing of the call that is running now, as the row shows it:
    /// `local low`, `high`, `local`, … (`None` when the round is untouched).
    pub route: Option<String>,
}

impl JevTurnActivity {
    /// Nothing to show: no decision in the window and nothing in flight.
    pub const fn is_quiet(&self) -> bool {
        self.decisions == 0 && self.in_flight == 0
    }

    /// The chip text, e.g. `jev…`, `jev 0.4s`, `jev ×3`. A final route is
    /// also rendered when Jev made no decision, so the row still names the
    /// model/effort that actually ran.
    ///
    /// Kept short by construction: it shares one row with the running tool and
    /// the turn timer.
    pub fn label(&self) -> Option<String> {
        if self.in_flight > 0 && self.decisions == 0 && self.route.is_none() {
            return Some("jev…".to_owned());
        }
        // The current micro-action's routing when the decision set one, else the
        // turn's own local marker.
        let suffix = match &self.route {
            Some(route) => format!(" ·{route}"),
            None if self.local_runs > 0 => " ·local".to_owned(),
            None => String::new(),
        };
        match self.decisions {
            0 if self.route.is_some() => Some(format!("model{suffix}")),
            0 if self.in_flight > 0 => Some("jev…".to_owned()),
            0 => None,
            1 => Some(format!(
                "jev {:.1}s{suffix}",
                self.last_latency_ms as f64 / 1000.0
            )),
            n => Some(format!("jev ×{n}{suffix}")),
        }
    }
}

/// The content the model has already been given verbatim in one session, by
/// hash. The index is process-global only for synchronization; its key is
/// session-qualified so a child or sibling never receives a reuse pointer for
/// bytes that are absent from its own conversation.
fn read_index() -> &'static std::sync::Mutex<std::collections::HashMap<(String, String), String>> {
    static INDEX: OnceLock<std::sync::Mutex<std::collections::HashMap<(String, String), String>>> =
        OnceLock::new();
    INDEX.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Remembers `payload` as read and reports where it was first seen.
///
/// `None` means "tell the model about it": the same bytes are already in the
/// conversation, so sending them again buys nothing. The label names the call
/// that first carried them, so the note can point at something the reader has.
pub fn note_payload_read(hash: &str, label: &str) -> Option<String> {
    let Ok(mut index) = read_index().lock() else {
        return None;
    };
    let key = (active_session_id(), hash.to_owned());
    if let Some(first) = index.get(&key) {
        return Some(first.clone());
    }
    index.insert(key, label.to_owned());
    None
}

/// Forgets payloads that the active session may have lost from its
/// model-visible conversation, such as after a successful compaction rewrite.
/// A poisoned index is treated as an empty reuse decision so compaction never
/// fails because this optimization could not be invalidated.
pub fn invalidate_payload_reads_for_active_session() {
    let session_id = active_session_id();
    if let Some(client) = client_cached() {
        client.clear_memo_for_session(&session_id);
    }
    forget_test_failures(&session_id);
    let Ok(mut index) = read_index().lock() else {
        return;
    };
    index.retain(|(owner, _), _| owner != &session_id);
}

/// Forgets only `session_id`'s read index, after a history eviction replaced
/// copies a reuse note could point at. Utility answers stay valid, so the
/// selection memo is kept.
pub fn invalidate_payload_reads_for_session(session_id: &str) {
    forget_test_failures(session_id);
    let Ok(mut index) = read_index().lock() else {
        return;
    };
    index.retain(|(owner, _), _| owner != session_id);
}

/// Failure blocks of a test run, by test name, with a hash of each block.
pub(crate) type TestFailures = std::collections::BTreeMap<String, String>;

/// The failure blocks the latest filtered run of each test command showed the
/// model, keyed like the read index and forgotten with it: after a compaction
/// or an eviction the copy a rerun would point at may be gone.
fn test_failure_index()
-> &'static std::sync::Mutex<std::collections::HashMap<(String, String), TestFailures>> {
    static INDEX: OnceLock<
        std::sync::Mutex<std::collections::HashMap<(String, String), TestFailures>>,
    > = OnceLock::new();
    INDEX.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The failure blocks the previous filtered run of `command` showed; empty
/// when there was none or the index is poisoned, so every block stays.
pub(crate) fn previous_test_failures(command: &str) -> TestFailures {
    test_failure_index()
        .lock()
        .ok()
        .and_then(|index| {
            index
                .get(&(active_session_id(), command.to_owned()))
                .cloned()
        })
        .unwrap_or_default()
}

/// Records the failure blocks the model has now seen for `command`.
pub(crate) fn note_test_failures(command: &str, failures: TestFailures) {
    if let Ok(mut index) = test_failure_index().lock() {
        index.insert((active_session_id(), command.to_owned()), failures);
    }
}

/// Forgets `command`'s baseline: its latest run reached the model verbatim,
/// so a fold against an older run would describe text the model did not see last.
pub(crate) fn forget_test_failures_of(command: &str) {
    if let Ok(mut index) = test_failure_index().lock() {
        index.remove(&(active_session_id(), command.to_owned()));
    }
}

fn forget_test_failures(session_id: &str) {
    if let Ok(mut index) = test_failure_index().lock() {
        index.retain(|(owner, _), _| owner != session_id);
    }
}

/// The `file:line` sites the latest failing terminal output cited, per session.
/// Nothing is quoted from them: they only count how often a read follows.
fn error_site_index()
-> &'static std::sync::Mutex<std::collections::HashMap<String, Vec<(String, usize)>>> {
    static INDEX: OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<(String, usize)>>>,
    > = OnceLock::new();
    INDEX.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Replaces the session's cited error sites.
pub(crate) fn note_error_sites(sites: Vec<(String, usize)>) {
    if let Ok(mut index) = error_site_index().lock() {
        index.insert(active_session_id(), sites);
    }
}

/// Removes and reports the first cited site `covers` accepts, so a site
/// counts once.
pub(crate) fn take_error_site(covers: impl Fn(&str, usize) -> bool) -> bool {
    let Ok(mut index) = error_site_index().lock() else {
        return false;
    };
    let Some(sites) = index.get_mut(&active_session_id()) else {
        return false;
    };
    let Some(position) = sites.iter().position(|(path, line)| covers(path, *line)) else {
        return false;
    };
    sites.remove(position);
    true
}

/// How many payloads the process remembers (tests, and a bound on the map).
pub fn remembered_reads() -> usize {
    read_index().lock().map(|index| index.len()).unwrap_or(0)
}

/// Forgets every remembered payload (tests: the index is process-wide).
pub fn reset_read_index_for_test() {
    if let Ok(mut index) = read_index().lock() {
        index.clear();
    }
}

/// Decisions whose label means a proposed optimization was refused.
const REFUSALS: &[&str] = &["block", "veto", "deny", "refuse", "refused"];

/// Lever and label of a call that ran on the local model (see `JevLever::B2LocalModel`).
const LOCAL_LEVER: &str = "b2_local_model";
const LOCAL_DECISION: &str = "local";

tokio::task_local! {
    static ACTIVE_SESSION_ID: String;
    static ACTIVE_TURN_ID: String;
    static ACTIVE_ROUND_ID: std::cell::Cell<u64>;
    static ACTIVE_USAGE_RECORDER: std::cell::RefCell<Option<distill_chat_state::ChatStateHandle>>;
}

pub(crate) fn telemetry_context() -> (String, String, u64) {
    (
        active_session_id(),
        ACTIVE_TURN_ID.try_with(Clone::clone).unwrap_or_default(),
        ACTIVE_ROUND_ID
            .try_with(std::cell::Cell::get)
            .unwrap_or_default(),
    )
}

pub(crate) fn begin_model_round() {
    let _ = ACTIVE_ROUND_ID.try_with(|round| round.set(round.get().saturating_add(1)));
}

pub(crate) fn active_usage_recorder() -> Option<distill_chat_state::ChatStateHandle> {
    ACTIVE_USAGE_RECORDER
        .try_with(|recorder| recorder.borrow().clone())
        .ok()
        .flatten()
}

/// Answers per session whose provenance a later [`record_item`] can still find.
const ANSWER_TRACE_CAPACITY: usize = 32;

/// What [`ask_item`] knew about an answer set that [`record_item`] is not given.
struct AnswerTrace {
    lever: &'static str,
    fingerprint: u64,
    question_hash: String,
    memo: bool,
}

/// One session's Jev decision events: its `events.jsonl` writer, held weakly
/// so a closed session's file is not kept open, the next sequence number and
/// the provenance of recent answers.
struct SessionJevEvents {
    writer: distill_session_events::WeakEventWriter,
    next_seq: u64,
    answers: std::collections::VecDeque<AnswerTrace>,
}

/// Keyed by the session id of the task-local scope. A session that never
/// registered (tests, detached helpers) records no event.
fn session_jev_events()
-> &'static std::sync::Mutex<std::collections::HashMap<String, SessionJevEvents>> {
    static EVENTS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, SessionJevEvents>>> =
        OnceLock::new();
    EVENTS.get_or_init(Default::default)
}

/// Makes `writer` the target of the Jev decision events of turns scoped to
/// `session_id`. A reloaded session keeps its sequence numbers.
pub(crate) fn register_session_events(
    session_id: &str,
    writer: &distill_session_events::EventWriter,
) {
    let Ok(mut sessions) = session_jev_events().lock() else {
        return;
    };
    sessions.retain(|_, session| session.writer.upgrade().is_some());
    sessions
        .entry(session_id.to_owned())
        .and_modify(|session| session.writer = writer.downgrade())
        .or_insert_with(|| SessionJevEvents {
            writer: writer.downgrade(),
            next_seq: 0,
            answers: std::collections::VecDeque::new(),
        });
}

/// Writes one Jev event for the active session; a no-op outside a registered one.
fn emit_jev_event(build: impl FnOnce(u64, Option<u64>) -> distill_session_events::Event) {
    let session_id = active_session_id();
    let Some((writer, seq)) = session_jev_events().lock().ok().and_then(|mut sessions| {
        let session = sessions.get_mut(&session_id)?;
        let writer = session.writer.upgrade()?;
        let seq = session.next_seq;
        session.next_seq = seq.saturating_add(1);
        Some((writer, seq))
    }) else {
        return;
    };
    let round = ACTIVE_ROUND_ID.try_with(std::cell::Cell::get).ok();
    writer.emit(build(seq, round));
}

/// Writes one outcome fact (a check, a reminder and what followed it, a mode
/// switch, a raised effort floor, a skill hint or read) to the events of
/// `session_id`, or of the active session when `None`; a no-op outside a
/// registered session. Facts take no decision sequence number, and only the
/// active session's facts carry its round.
pub(crate) fn emit_outcome_event(
    session_id: Option<&str>,
    build: impl FnOnce(Option<u64>) -> distill_session_events::Event,
) {
    let round = match session_id {
        Some(_) => None,
        None => ACTIVE_ROUND_ID.try_with(std::cell::Cell::get).ok(),
    };
    let session_id = session_id.map_or_else(active_session_id, str::to_owned);
    let Some(writer) = session_jev_events()
        .lock()
        .ok()
        .and_then(|sessions| sessions.get(&session_id)?.writer.upgrade())
    else {
        return;
    };
    writer.emit(build(round));
}

/// 12 hex over a command, skill name or working-tree marker: an outcome fact
/// never carries the text itself.
pub(crate) fn outcome_hash(text: &str) -> String {
    short_hash(text.as_bytes())
}

/// An id an outcome fact carries, hashed unless it is a short identifier.
pub(crate) fn outcome_id(raw: &str) -> String {
    event_id(raw)
}

/// One `mode_switched` fact, only when the effective mode really changed.
pub(crate) fn emit_mode_switched(
    session_id: Option<&str>,
    from: distill_tool_types::ExecutionMode,
    to: distill_tool_types::ExecutionMode,
    trigger: distill_session_events::ModeSwitchTrigger,
) {
    use distill_tool_types::ExecutionMode;
    let id = |mode| match mode {
        ExecutionMode::Normal => "normal",
        ExecutionMode::Ultrafast => "ultrafast",
        ExecutionMode::Ultracode => "ultracode",
        ExecutionMode::Ultrasave => "ultrasave",
    };
    if from != to {
        emit_outcome_event(session_id, |round| {
            distill_session_events::Event::ModeSwitched {
                from: id(from),
                to: id(to),
                trigger,
                round,
            }
        });
    }
}

/// One `skill_suggested` or `skill_read` fact for the skill called `name`.
pub(crate) fn emit_skill_fact(name: &str, read: bool) {
    let name_hash = outcome_hash(name);
    emit_outcome_event(None, |round| {
        if read {
            distill_session_events::Event::SkillRead { name_hash, round }
        } else {
            distill_session_events::Event::SkillSuggested { name_hash, round }
        }
    });
}

/// Runs `body` in a turn scope of `session_id` whose events go to a fresh
/// file; returns that file's text.
#[cfg(test)]
pub(crate) async fn session_events_text_for_test(
    session_id: &str,
    body: impl Future<Output = ()>,
) -> String {
    let dir = tempfile::tempdir().expect("events dir");
    let writer = distill_session_events::EventWriter::open(dir.path());
    register_session_events(session_id, &writer);
    with_session_scope(session_id.to_owned(), body).await;
    drop(writer);
    std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap_or_default()
}

/// Per-lever counts of each composed label in the session's usage.json
/// (`utilityOutcomes["jev:<lever>"]`); nothing without a usage recorder.
fn count_jev_outcome(lever: distill_workspace::jev::flags::JevLever, label: &str) {
    if let Some(recorder) = active_usage_recorder() {
        recorder.record_utility_outcome(&format!("jev:{}", lever.as_str()), label, 0, 0, 0);
    }
}

fn emit_no_answer(
    lever: distill_workspace::jev::flags::JevLever,
    kind: distill_session_events::JevNoAnswerKind,
    question_hash: Option<String>,
    latency_ms: u64,
) {
    let kind_label: &'static str = kind.into();
    count_jev_outcome(lever, &format!("no_answer:{kind_label}"));
    emit_jev_event(|seq, round| distill_session_events::Event::JevNoAnswer {
        seq,
        lever: lever.as_str(),
        kind,
        question_hash,
        latency_ms,
        round,
    });
}

/// Identifies an answer set across the clones call sites pass around.
fn answers_fingerprint(answers: &distill_workspace::jev::types::JevAnswerSet) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    answers.model.hash(&mut hasher);
    serde_json::to_vec(&answers.answers)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn note_answer_trace(
    lever: distill_workspace::jev::flags::JevLever,
    answers: &distill_workspace::jev::types::JevAnswerSet,
    question_hash: String,
    memo: bool,
) {
    let session_id = active_session_id();
    let Ok(mut sessions) = session_jev_events().lock() else {
        return;
    };
    let Some(session) = sessions.get_mut(&session_id) else {
        return;
    };
    if session.answers.len() >= ANSWER_TRACE_CAPACITY {
        session.answers.pop_front();
    }
    session.answers.push_back(AnswerTrace {
        lever: lever.as_str(),
        fingerprint: answers_fingerprint(answers),
        question_hash,
        memo,
    });
}

/// The question hash and memo flag of the newest matching ask, if any.
fn answer_trace(
    lever: distill_workspace::jev::flags::JevLever,
    answers: &distill_workspace::jev::types::JevAnswerSet,
) -> Option<(String, bool)> {
    let session_id = active_session_id();
    let fingerprint = answers_fingerprint(answers);
    let sessions = session_jev_events().lock().ok()?;
    sessions
        .get(&session_id)?
        .answers
        .iter()
        .rev()
        .find(|trace| trace.lever == lever.as_str() && trace.fingerprint == fingerprint)
        .map(|trace| (trace.question_hash.clone(), trace.memo))
}

fn short_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 12 hex over the question ids and instruction texts: the same question
/// keeps its hash whatever the state or the candidate options.
fn question_hash(
    questions: &std::collections::BTreeMap<
        distill_workspace::jev::types::QuestionId,
        distill_workspace::jev::types::Question,
    >,
) -> String {
    use distill_workspace::jev::types::Question;
    let mut bytes = Vec::new();
    for (id, question) in questions {
        let (Question::Noul { instructions, .. }
        | Question::Choice { instructions, .. }
        | Question::Score { instructions, .. }) = question;
        bytes.extend_from_slice(id.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(instructions.to_string().as_bytes());
        bytes.push(0);
    }
    short_hash(&bytes)
}

/// A label, question id or option id as an event carries it. Some ids are
/// built from the user's files (test names, paths), so anything beyond a
/// short identifier is replaced by its hash.
fn event_id(raw: &str) -> String {
    if raw.len() <= 64
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '.'))
    {
        raw.to_owned()
    } else {
        format!("h:{}", short_hash(raw.as_bytes()))
    }
}

/// Levers whose option ids name things in the user's setup or files (skills,
/// tests, read targets). A short name passes [`event_id`] as an identifier, so
/// these ids are always hashed.
const fn options_from_user_content(lever: distill_workspace::jev::flags::JevLever) -> bool {
    use distill_workspace::jev::flags::JevLever;
    matches!(
        lever,
        JevLever::P6SkillSuggestion | JevLever::A6TestToRun | JevLever::P2ReadShortlist
    )
}

fn opaque_id(raw: &str) -> String {
    format!("h:{}", short_hash(raw.as_bytes()))
}

/// Above this many options a distribution keeps only its most likely entries.
const EVENT_FULL_DISTRIBUTION: usize = 8;
const EVENT_KEPT_PROBABILITIES: usize = 5;

/// The distribution an event carries and how many entries it left out.
fn event_distribution(
    probabilities: &std::collections::BTreeMap<String, f64>,
    opaque: bool,
) -> (std::collections::BTreeMap<String, f64>, usize) {
    let id = |raw: &str| if opaque { opaque_id(raw) } else { event_id(raw) };
    if probabilities.len() <= EVENT_FULL_DISTRIBUTION {
        return (
            probabilities.iter().map(|(raw, p)| (id(raw), *p)).collect(),
            0,
        );
    }
    let mut ranked: Vec<_> = probabilities.iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(a.1));
    let kept = ranked
        .into_iter()
        .take(EVENT_KEPT_PROBABILITIES)
        .map(|(raw, p)| (id(raw), *p))
        .collect();
    (
        kept,
        probabilities.len().saturating_sub(EVENT_KEPT_PROBABILITIES),
    )
}

/// `opaque` hashes a choice's option ids whatever they look like; a score's
/// keys are its levels and stay readable.
fn answer_summary(
    answer: &distill_workspace::jev::types::Answer,
    opaque: bool,
) -> distill_session_events::JevAnswerSummary {
    use distill_session_events::JevAnswerSummary;
    use distill_workspace::jev::types::Answer;
    match answer {
        Answer::Noul { noul } => JevAnswerSummary::Noul { noul: *noul },
        Answer::Choice {
            choice,
            probabilities,
            confidence,
        } => {
            let (probabilities, omitted) = event_distribution(probabilities, opaque);
            JevAnswerSummary::Choice {
                choice: if opaque {
                    opaque_id(choice)
                } else {
                    event_id(choice)
                },
                probabilities,
                confidence: *confidence,
                omitted,
            }
        }
        Answer::Score {
            score,
            probabilities,
            confidence,
            ..
        } => {
            let (probabilities, omitted) = event_distribution(probabilities, false);
            JevAnswerSummary::Score {
                score: *score,
                probabilities,
                confidence: *confidence,
                omitted,
            }
        }
    }
}

pub(crate) fn record_workspace_attempt(
    attempt: distill_workspace::jev::types::AttemptRecord,
    role: &str,
    task_id: Option<String>,
    turn_id: Option<String>,
    recorder: distill_chat_state::ChatStateHandle,
    attribute_to_prompt: bool,
) {
    record_tagged_workspace_attempt(
        attempt,
        role,
        task_id,
        turn_id,
        recorder,
        attribute_to_prompt,
        None,
    );
}

/// [`record_workspace_attempt`] for a utility attempt: `tags` is the source
/// kind and the final decision the row carries (see `UsageAttribution`).
pub(crate) fn record_tagged_workspace_attempt(
    attempt: distill_workspace::jev::types::AttemptRecord,
    role: &str,
    task_id: Option<String>,
    turn_id: Option<String>,
    recorder: distill_chat_state::ChatStateHandle,
    attribute_to_prompt: bool,
    tags: Option<(&str, &str)>,
) {
    use distill_chat_state::{UsageAttribution, UsageCallStatus, UsageCostBasis};

    let usage = attempt.usage.as_ref().and_then(|usage| {
        (!usage.is_empty()).then(|| distill_sampling_types::TokenUsage {
            prompt_tokens: usage.input_tokens.unwrap_or(0).min(u64::from(u32::MAX)) as u32,
            completion_tokens: usage.output_tokens.unwrap_or(0).min(u64::from(u32::MAX)) as u32,
            total_tokens: usage
                .input_tokens
                .unwrap_or(0)
                .saturating_add(usage.output_tokens.unwrap_or(0))
                .min(u64::from(u32::MAX)) as u32,
            reasoning_tokens: attempt
                .billing
                .reasoning_tokens
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            cached_prompt_tokens: attempt
                .billing
                .cached_input_tokens
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            cache_creation_prompt_tokens: attempt
                .billing
                .cache_creation_input_tokens
                .unwrap_or(0)
                .min(u64::from(u32::MAX)) as u32,
            cache_creation_1h_prompt_tokens: 0,
        })
    });
    let cost_usd_ticks = attempt.billing.cost_usd_ticks.filter(|&cost| cost >= 0);
    let status = match attempt.status {
        distill_workspace::jev::types::AttemptStatus::Completed => UsageCallStatus::Completed,
        distill_workspace::jev::types::AttemptStatus::Rejected => UsageCallStatus::Rejected,
        distill_workspace::jev::types::AttemptStatus::Failed => UsageCallStatus::Failed,
        distill_workspace::jev::types::AttemptStatus::Cancelled => UsageCallStatus::Cancelled,
    };
    let model_id = attempt
        .response_model
        .filter(|model| !model.trim().is_empty())
        .unwrap_or(attempt.requested_model);
    recorder.record_usage_attribution(
        UsageAttribution {
            attempt_id: attempt.attempt_id,
            task_id,
            turn_id,
            request_id: attempt.request_id,
            role: role.to_owned(),
            model_id,
            endpoint: Some(attempt.endpoint),
            requested_effort: attempt.requested_effort,
            applied_effort: attempt.applied_effort,
            reason: attempt.reason,
            bytes_in: attempt.bytes_in,
            source_kind: tags.map(|(source_kind, _)| source_kind.to_owned()),
            final_decision: tags.map(|(_, decision)| decision.to_owned()),
            bytes_out: attempt.bytes_out,
            status,
            usage,
            usage_complete: attempt
                .usage
                .as_ref()
                .is_some_and(|usage| {
                    usage.input_tokens.is_some() && usage.output_tokens.is_some()
                }),
            api_duration_ms: Some(attempt.latency_ms),
            cost_usd_ticks,
            cache_lifetime_secs: None,
            cost_basis: if cost_usd_ticks.is_some() {
                UsageCostBasis::Reported
            } else {
                UsageCostBasis::Unknown
            },
        },
        attribute_to_prompt,
    );
}

#[derive(Debug, Default)]
struct ActivityState {
    sessions: std::collections::HashMap<String, SessionActivityState>,
    /// Child session id → parent session id, so a subagent's decisions also count for its parent.
    parents: std::collections::HashMap<String, String>,
}

/// Longest parent chain `note_decision` follows.
const MAX_ANCESTOR_DEPTH: usize = 8;

#[derive(Debug, Default)]
struct SessionActivityState {
    ring: std::collections::VecDeque<JevActivity>,
    in_flight: u32,
    route: Option<String>,
}

fn activity_state() -> &'static std::sync::Mutex<ActivityState> {
    static STATE: OnceLock<std::sync::Mutex<ActivityState>> = OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(ActivityState::default()))
}

fn active_session_id() -> String {
    ACTIVE_SESSION_ID
        .try_with(|session_id| session_id.clone())
        .unwrap_or_default()
}

/// Runs a session turn with activity isolated from every other session.
pub async fn with_session_scope<F>(session_id: impl Into<String>, future: F) -> F::Output
where
    F: Future,
{
    with_session_scope_and_recorder(session_id, None, future).await
}

/// Runs a session turn with its existing ChatState handle installed before any
/// turn work starts. This captures Jev and utility calls that happen before
/// sampler preparation while keeping the recorder task-local per session.
pub(crate) async fn with_session_scope_and_recorder<F>(
    session_id: impl Into<String>,
    recorder: Option<distill_chat_state::ChatStateHandle>,
    future: F,
) -> F::Output
where
    F: Future,
{
    ACTIVE_SESSION_ID
        .scope(
            session_id.into(),
            ACTIVE_TURN_ID.scope(
                uuid::Uuid::new_v4().to_string(),
                ACTIVE_USAGE_RECORDER.scope(
                    std::cell::RefCell::new(recorder),
                    ACTIVE_ROUND_ID.scope(
                        std::cell::Cell::new(0),
                        crate::jev_cheap::with_optional_compression_scope(future),
                    ),
                ),
            ),
        )
        .await
}

/// Installs only the usage recorder: a display side call (recap, suggestion)
/// reaches usage.json without joining the session's activity ring, turn id or
/// optional-compression scope, so the turn-status row never shows it.
pub(crate) async fn with_usage_recorder<F>(
    recorder: distill_chat_state::ChatStateHandle,
    future: F,
) -> F::Output
where
    F: Future,
{
    ACTIVE_USAGE_RECORDER
        .scope(std::cell::RefCell::new(Some(recorder)), future)
        .await
}

/// Installs `recorder` only when no usage recorder is active, so a call made
/// from a detached task is attributed without resetting a running turn's scope.
pub(crate) async fn with_recorder_unless_scoped<F>(
    session_id: impl Into<String>,
    recorder: Option<distill_chat_state::ChatStateHandle>,
    future: F,
) -> F::Output
where
    F: Future,
{
    if recorder.is_none() || active_usage_recorder().is_some() {
        future.await
    } else {
        with_session_scope_and_recorder(session_id, recorder, future).await
    }
}

/// Remembers one decision for the turn-status row. Never fails the caller.
pub fn note_decision(lever: &str, decision: &str, latency_ms: u64) {
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    let own = active_session_id();
    let activity = JevActivity {
        lever: lever.to_owned(),
        decision: decision.to_owned(),
        latency_ms,
        at: std::time::Instant::now(),
    };
    let mut visited = vec![own.clone()];
    let mut current = own;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        let Some(parent) = state.parents.get(&current).cloned() else {
            break;
        };
        if visited.contains(&parent) {
            break;
        }
        visited.push(parent.clone());
        current = parent;
    }
    for session_id in visited {
        state
            .sessions
            .entry(session_id)
            .or_default()
            .ring
            .push_back(activity.clone());
    }
}

/// Links a child session to its parent so the child's decisions count for the parent too.
pub(crate) fn register_child_session(child: &str, parent: &str) {
    if child == parent {
        return;
    }
    if let Ok(mut state) = activity_state().lock() {
        state.parents.insert(child.to_owned(), parent.to_owned());
    }
}

/// Sessions kept in [`offered_tool_families`]; past this the map starts over,
/// and a child whose parent fell out keeps today's ask-and-fail-open path.
const MAX_FAMILY_SESSIONS: usize = 1024;

fn offered_tool_families_map()
-> &'static std::sync::Mutex<std::collections::HashMap<String, std::collections::BTreeSet<String>>>
{
    static MAP: OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::collections::BTreeSet<String>>>,
    > = OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// P1: records the optional tool families `session` offers after its
/// turn-start pass judged a human request, for its subagents to start from.
pub(crate) fn note_offered_tool_families(
    session: &str,
    families: std::collections::BTreeSet<String>,
) {
    if let Ok(mut map) = offered_tool_families_map().lock() {
        if map.len() >= MAX_FAMILY_SESSIONS && !map.contains_key(session) {
            map.clear();
        }
        map.insert(session.to_owned(), families);
    }
}

/// The optional tool families `session` offers, or `None` when it has not
/// judged a request in this process (the child then decides on its own).
pub(crate) fn offered_tool_families(session: &str) -> Option<std::collections::BTreeSet<String>> {
    offered_tool_families_map()
        .lock()
        .ok()
        .and_then(|map| map.get(session).cloned())
}

/// Why an [`ask_item`] for `lever` came back empty, for the decision log:
/// the lever is off, there is no credential, or the request failed or timed
/// out. Test answer queues report as the last case.
pub(crate) fn unanswered_reason(lever: distill_workspace::jev::flags::JevLever) -> &'static str {
    if !flags_cached().lever_active(lever) {
        "lever_off"
    } else if client_cached().is_none_or(|client| !client.credential_present()) {
        "no_credential"
    } else {
        "error_or_timeout"
    }
}

/// Sets the routing of the call that is about to run, as the row shows it.
///
/// Called once per model call, after the decision layer. `engine` names the
/// model the decision moved the call to (`openrouter-qwen37`), so the row says
/// which model is about to run and not only that something changed; `None` means
/// the session's own model, and then only the level shows (`high`).
pub fn note_route(engine: Option<&str>, level: Option<&str>) {
    #[cfg(test)]
    TEST_DISPATCHES.with(|count| count.set(count.get() + 1));
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    let session = state.sessions.entry(active_session_id()).or_default();
    session.route = match (engine, level) {
        (Some(engine), Some(level)) => Some(format!("{engine} {level}")),
        (Some(engine), None) => Some(engine.to_owned()),
        (None, Some(level)) => Some(level.to_owned()),
        (None, None) => None,
    };
}

/// Marks a Jev call as started (`+1`) or finished (`-1`) for the row's `jev…` state.
pub fn note_in_flight(started: bool) {
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    let session = state.sessions.entry(active_session_id()).or_default();
    session.in_flight = if started {
        session.in_flight.saturating_add(1)
    } else {
        session.in_flight.saturating_sub(1)
    };
}

/// The row's view of Jev: decisions recorded at or after `since`.
///
/// `None` (no turn anchor — a wake turn, or a row rendered outside a turn)
/// counts every decision still recorded for this session. There is no
/// ceiling: a long turn can consult Jev on every micro-action.
pub fn turn_activity(since: Option<std::time::Instant>) -> JevTurnActivity {
    turn_activity_for_session("", since)
}

/// Lever and label of each decision recorded for one session, oldest first.
#[cfg(test)]
pub(crate) fn recorded_decisions_for_test(session_id: &str) -> Vec<(String, String)> {
    let Ok(state) = activity_state().lock() else {
        return Vec::new();
    };
    state.sessions.get(session_id).map_or_else(Vec::new, |session| {
        session
            .ring
            .iter()
            .map(|entry| (entry.lever.clone(), entry.decision.clone()))
            .collect()
    })
}

/// Reads the status for one session without relying on whichever async task is
/// currently rendering the UI.
pub fn turn_activity_for_session(
    session_id: &str,
    since: Option<std::time::Instant>,
) -> JevTurnActivity {
    let Ok(state) = activity_state().lock() else {
        return JevTurnActivity::default();
    };
    let Some(session) = state.sessions.get(session_id) else {
        return JevTurnActivity::default();
    };
    let mut activity = JevTurnActivity {
        in_flight: session.in_flight,
        route: session.route.clone(),
        ..Default::default()
    };
    for entry in session.ring.iter().rev() {
        if let Some(since) = since
            && entry.at < since
        {
            continue;
        }
        // The escalation ratchet's rows are harness facts with no Jev call
        // behind them; they stay in the ring but are not Jev activity.
        if entry.lever == distill_workspace::jev::flags::JevLever::Escalation.as_str() {
            continue;
        }
        activity.decisions = activity.decisions.saturating_add(1);
        if REFUSALS.contains(&entry.decision.as_str()) {
            activity.refused = true;
        }
        if entry.lever == LOCAL_LEVER && entry.decision == LOCAL_DECISION {
            activity.local_runs = activity.local_runs.saturating_add(1);
        }
        if activity.last_latency_ms == 0 {
            activity.last_latency_ms = entry.latency_ms;
        }
    }
    activity
}

/// Drops everything the row remembers. Test-only: one process, one ring.
#[doc(hidden)]
pub fn reset_activity_for_test() {
    if let Ok(mut state) = activity_state().lock() {
        state.sessions.clear();
        state.parents.clear();
    }
}

/// Keeps the in-flight counter honest across every early return of a call.
pub struct JevInFlight {
    open: bool,
}

impl JevInFlight {
    /// Marks a call as started.
    pub fn begin() -> Self {
        note_in_flight(true);
        Self { open: true }
    }
}

impl Drop for JevInFlight {
    fn drop(&mut self) {
        if self.open {
            note_in_flight(false);
            self.open = false;
        }
    }
}

/// The decision sink every Jev seam reports through: the log line, plus the
/// row's activity.
#[derive(Debug, Default, Clone, Copy)]
pub struct ActivitySink;

impl distill_workspace::jev::policy::DecisionSink for ActivitySink {
    fn record(&self, record: &distill_workspace::jev::policy::DecisionRecord) {
        let (session, turn, round) = telemetry_context();
        let mut record = record.clone();
        record.session_id = (!session.is_empty()).then_some(session);
        record.turn_id = (!turn.is_empty()).then_some(turn);
        record.round_id = (round > 0).then_some(round);
        distill_workspace::jev::policy::TracingSink.record(&record);
        note_decision(&record.lever, &record.decision, record.latency_ms);
    }
}

static LOCAL_MODEL_CONFIG: OnceLock<parking_lot::RwLock<JevLocalConfig>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Focused routing tests override only their own current-thread session;
    /// production still reads the process cache below.
    static TEST_LOCAL_MODEL_CONFIG: std::cell::RefCell<Option<JevLocalConfig>> =
        const { std::cell::RefCell::new(None) };
    /// `Some(None)` pins "no worker model" regardless of the disk config.
    static TEST_WORKER_MODEL: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
    /// `Some(None)` pins the worker's effort to auto regardless of the disk config.
    static TEST_WORKER_EFFORT: std::cell::RefCell<Option<Option<distill_sampling_types::ReasoningEffort>>> =
        const { std::cell::RefCell::new(None) };
    /// Pins the resolved flags regardless of the disk config.
    static TEST_FLAGS: std::cell::Cell<Option<JevFlags>> = const { std::cell::Cell::new(None) };
}

/// Snapshot the configured utility model without reading disk on every call.
pub fn local_config_cached() -> JevLocalConfig {
    #[cfg(test)]
    if let Some(config) = TEST_LOCAL_MODEL_CONFIG.with(|config| config.borrow().clone()) {
        return config;
    }
    LOCAL_MODEL_CONFIG
        .get_or_init(|| parking_lot::RwLock::new(resolve_config_from_disk().local))
        .read()
        .clone()
}

/// The configured worker model (`[models].worker`) that runs delegated work,
/// or `None` when the main model does all the work. Read on every call, so a
/// change reaches sessions in other processes.
pub fn worker_model() -> Option<String> {
    #[cfg(test)]
    if let Some(model) = TEST_WORKER_MODEL.with(|model| model.borrow().clone()) {
        return model;
    }
    crate::config::load_effective_config()
        .ok()?
        .get("models")?
        .get("worker")?
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The worker's configured effort (`[models].worker_effort`): a level, or
/// `None` for auto (Jev picks it per call), which is also what an unset or
/// unreadable value means.
pub fn worker_effort() -> Option<distill_sampling_types::ReasoningEffort> {
    #[cfg(test)]
    if let Some(effort) = TEST_WORKER_EFFORT.with(|effort| *effort.borrow()) {
        return effort;
    }
    crate::config::load_effective_config()
        .ok()?
        .get("models")?
        .get("worker_effort")?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))?
        .parse()
        .ok()
}

/// Resolves a `[models]` variant value: unset is `floor`, `none` is no suffix,
/// anything outside floor|nitro|exacto|none falls back to `floor`.
pub(crate) fn resolve_openrouter_variant(raw: Option<&str>) -> Option<String> {
    let Some(raw) = raw.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some("floor".to_owned());
    };
    match raw.to_ascii_lowercase().as_str() {
        "none" => None,
        v @ ("floor" | "nitro" | "exacto") => Some(v.to_owned()),
        _ => {
            tracing::warn!(value = raw, "invalid OpenRouter variant; using floor");
            Some("floor".to_owned())
        }
    }
}

fn models_variant(key: &str) -> Option<String> {
    resolve_openrouter_variant(models_variant_setting(key).as_deref())
}

fn models_variant_setting(key: &str) -> Option<String> {
    let config = crate::config::load_effective_config().ok();
    config
        .as_ref()
        .and_then(|c| c.get("models")?.get(key)?.as_str().map(str::to_owned))
}

/// The main model's OpenRouter routing suffix (`[models].main_variant`), read per call.
pub fn main_variant() -> Option<String> {
    models_variant("main_variant")
}

/// [`main_variant`] for a session whose pinned mode asks for a provider tier.
pub(crate) fn main_variant_for_tier(tier: Option<distill_sampler::ProviderTier>) -> Option<String> {
    resolve_main_variant_for_tier(models_variant_setting("main_variant").as_deref(), tier)
}

/// An explicit `main_variant` wins; unset, `fast` defaults to `nitro` and `economy` to `floor`
/// (the unset default already), and no tier keeps today's resolution.
fn resolve_main_variant_for_tier(
    raw: Option<&str>,
    tier: Option<distill_sampler::ProviderTier>,
) -> Option<String> {
    let unset = raw.is_none_or(|value| value.trim().is_empty());
    if unset && tier == Some(distill_sampler::ProviderTier::Fast) {
        return Some("nitro".to_owned());
    }
    resolve_openrouter_variant(raw)
}

/// The worker model's OpenRouter routing suffix (`[models].worker_variant`), read per call.
pub fn worker_variant() -> Option<String> {
    models_variant("worker_variant")
}

/// Publish a utility selection only after its atomic config write succeeds.
pub(crate) fn update_local_model_cache(model: String, effort: String) {
    let mut config = LOCAL_MODEL_CONFIG
        .get_or_init(|| parking_lot::RwLock::new(resolve_config_from_disk().local))
        .write();
    config.model = Some(model);
    config.effort = Some(effort);
}

#[cfg(test)]
pub(crate) fn set_test_local_config(config: JevLocalConfig) {
    TEST_LOCAL_MODEL_CONFIG.with(|current| *current.borrow_mut() = Some(config));
}

#[cfg(test)]
pub(crate) fn clear_test_local_config() {
    TEST_LOCAL_MODEL_CONFIG.with(|current| *current.borrow_mut() = None);
}

#[cfg(test)]
pub(crate) fn set_test_worker_model(model: Option<String>) {
    TEST_WORKER_MODEL.with(|current| *current.borrow_mut() = Some(model));
}

#[cfg(test)]
pub(crate) fn clear_test_worker_model() {
    TEST_WORKER_MODEL.with(|current| *current.borrow_mut() = None);
}

#[cfg(test)]
pub(crate) fn set_test_worker_effort(effort: Option<distill_sampling_types::ReasoningEffort>) {
    TEST_WORKER_EFFORT.with(|current| *current.borrow_mut() = Some(effort));
}

#[cfg(test)]
pub(crate) fn clear_test_worker_effort() {
    TEST_WORKER_EFFORT.with(|current| *current.borrow_mut() = None);
}

#[cfg(test)]
pub(crate) fn set_test_flags(flags: JevFlags) {
    TEST_FLAGS.with(|current| current.set(Some(flags)));
}

#[cfg(test)]
pub(crate) fn clear_test_flags() {
    TEST_FLAGS.with(|current| current.set(None));
}

/// Whether a session starts in auto effort: the decision layer picks the effort
/// for each model call, and an explicit level turns the mode off for the session.
///
/// The same value `ModelsManager` seeds itself with (`[jev] effort_auto`, unset
/// ⇒ on), resolved once per process so a client that mirrors it — the pager's
/// footer — cannot disagree with the shell about the mode.
pub fn effort_auto_cached() -> bool {
    static AUTO: OnceLock<bool> = OnceLock::new();
    *AUTO.get_or_init(|| resolve_config_from_disk().effort_auto.unwrap_or(true))
}

/// Whether one catalogue item is active right now.
///
/// The zero-cost gate: a lane that can do its work without a model (the
/// deterministic reduction) asks this instead of paying for a battery it does
/// not need, and it stays off with the same flag.
pub fn lever_active(lever: distill_workspace::jev::flags::JevLever) -> bool {
    flags_cached().lever_active(lever)
}

/// D6 also evicts in warm batches (`d6_warm_batches`); off by default.
pub(crate) fn history_eviction_warm() -> bool {
    flags_cached().d6_warm_batches
}

/// The flags resolved once per process (configuration does not change mid-run).
fn flags_cached() -> distill_workspace::jev::flags::JevFlags {
    #[cfg(test)]
    if let Some(flags) = TEST_FLAGS.with(std::cell::Cell::get) {
        return flags;
    }
    static FLAGS: OnceLock<distill_workspace::jev::flags::JevFlags> = OnceLock::new();
    *FLAGS.get_or_init(|| flags_from(&resolve_config_from_disk()))
}

/// A copy of `client` that records each attempt in the active session's usage
/// under `label`; `None` outside a session with a usage recorder.
pub(crate) fn observed_client(
    client: &distill_workspace::jev::JevClient,
    label: &str,
) -> Option<distill_workspace::jev::JevClient> {
    let recorder = active_usage_recorder()?;
    let turn_id = telemetry_context().1;
    let label = label.to_owned();
    let observer: distill_workspace::jev::types::AttemptObserver =
        std::sync::Arc::new(move |attempt| {
            record_workspace_attempt(
                attempt,
                "jev",
                Some(label.clone()),
                Some(turn_id.clone()),
                recorder.clone(),
                true,
            );
        });
    Some(client.with_call_observer(observer))
}

/// One client per process, built lazily and only when a credential exists.
pub(crate) fn client_cached() -> Option<&'static distill_workspace::jev::JevClient> {
    static CLIENT: OnceLock<Option<distill_workspace::jev::JevClient>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let cfg = resolve_config_from_disk();
            let config = match client_config_from(&cfg) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!(%error, "jev configuration invalid; decisions are off");
                    return None;
                }
            };
            match distill_workspace::jev::JevClient::new(config) {
                Ok(client) if client.credential_present() => Some(client),
                Ok(_) => {
                    if flags_from(&cfg).enabled {
                        tracing::warn!(
                            "Jev is enabled but environment variable `{}` is not set; Jev decisions are off",
                            "OPENROUTER_API_KEY"
                        );
                    }
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "jev client unavailable; catalogue items stay off");
                    None
                }
            }
        })
        .as_ref()
}

/// Item calls sit on the tool-result path rather than the permission actor, so
/// they get a shorter budget than the client default — but "shorter than the
/// client" is not the same as "short enough for any backend".
///
/// The System One service answers a whole battery in well under a second, so a
/// 4 s cap there is generous. A chat model has to *generate* the answer, and its
/// thinking budget, token by token: live measurement on `qwen/qwen3.7-flash` was
/// 8 s for the permission battery, which the 4 s cap turned into a timeout on
/// **every** call. The budget is therefore resolved per backend in
/// [`client_config_from`], and never exceeds the client's own deadline.
fn item_budget(client: &distill_workspace::jev::JevClient) -> std::time::Duration {
    client.config().item_budget.min(client.config().timeout)
}

#[cfg(test)]
mod catalogue_helper_tests {
    use super::*;

    #[test]
    fn item_budget_is_capped_per_backend() {
        // The decision service answers in well under a second: a short cap keeps
        // the tool-result path moving if it ever stops doing so.
        let cfg = JevConfig::default();
        let client = distill_workspace::jev::JevClient::new(client_config_from(&cfg).expect("valid Jev config"))
            .expect("client builds without I/O");
        let budget = item_budget(&client);
        assert!(budget <= std::time::Duration::from_millis(4_000));
        assert!(!budget.is_zero());

        let chat = JevConfig {
            provider: Some("openrouter".to_owned()),
            ..JevConfig::default()
        };
        assert!(client_config_from(&chat).is_err());
    }

    #[test]
    fn decision_memo_key_tracks_state_questions_and_model() {
        use distill_workspace::jev::flags::JevLever;
        use distill_workspace::jev::types::Question;
        use std::collections::BTreeMap;

        let client = distill_workspace::jev::JevClient::new(
            client_config_from(&JevConfig::default()).expect("valid Jev config"),
        )
        .expect("client builds without I/O");
        let state = serde_json::json!({"evidence": "first"});
        let questions = BTreeMap::from([(
            "criteria".to_owned(),
            Question::noul("Is the candidate eligible?"),
        )]);
        let first = decision_memo_key(&client, JevLever::A1FileToEdit, &state, &questions)
            .expect("memo key");

        assert_ne!(
            first,
            decision_memo_key(
                &client,
                JevLever::A1FileToEdit,
                &serde_json::json!({"evidence": "changed"}),
                &questions,
            )
            .expect("changed state key")
        );
        let changed_questions = BTreeMap::from([(
            "criteria".to_owned(),
            Question::noul("Is the candidate still eligible?"),
        )]);
        assert_ne!(
            first,
            decision_memo_key(
                &client,
                JevLever::A1FileToEdit,
                &state,
                &changed_questions,
            )
            .expect("changed criteria key")
        );
        let changed_model = JevConfig {
            model: Some("typesafe/jev-1.14-20261014".to_owned()),
            ..JevConfig::default()
        };
        let changed_client = distill_workspace::jev::JevClient::new(
            client_config_from(&changed_model).expect("valid Jev config"),
        )
        .expect("changed client builds without I/O");
        assert_ne!(
            first,
            decision_memo_key(
                &changed_client,
                JevLever::A1FileToEdit,
                &state,
                &questions,
            )
            .expect("changed model key")
        );
    }

    /// The reuse lane's rule: the first payload is remembered, the second
    /// identical one becomes a pointer, and a changed payload is never mistaken
    /// for a repeat.
    #[test]
    #[serial_test::serial]
    fn the_read_index_points_a_repeat_at_the_first_copy() {
        reset_read_index_for_test();
        assert_eq!(remembered_reads(), 0);

        assert_eq!(
            note_payload_read("hash-a", "read_file"),
            None,
            "the first read reports nothing: it is the copy the others point at"
        );
        assert_eq!(
            note_payload_read("hash-a", "read_file").as_deref(),
            Some("read_file")
        );
        assert_eq!(remembered_reads(), 1, "a repeat is not remembered twice");
        assert_eq!(
            note_payload_read("hash-b", "grep"),
            None,
            "different bytes are a different payload"
        );
        assert_eq!(remembered_reads(), 2);
        reset_read_index_for_test();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn read_reuse_isolated_between_sessions_but_repeats_within_one() {
        reset_read_index_for_test();
        with_session_scope("parent-read", async {
            assert_eq!(note_payload_read("same-bytes", "parent-read"), None);
            assert_eq!(
                note_payload_read("same-bytes", "parent-read").as_deref(),
                Some("parent-read")
            );
        })
        .await;
        with_session_scope("child-read", async {
            assert_eq!(
                note_payload_read("same-bytes", "child-read"),
                None,
                "a child must receive its first full payload"
            );
        })
        .await;
        assert_eq!(remembered_reads(), 2);
        reset_read_index_for_test();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn compaction_read_invalidation_clears_only_the_active_session() {
        reset_read_index_for_test();
        with_session_scope("active-read", async {
            assert_eq!(note_payload_read("same-bytes", "active-read"), None);
            assert_eq!(
                note_payload_read("same-bytes", "active-read").as_deref(),
                Some("active-read")
            );
        })
        .await;
        with_session_scope("other-read", async {
            assert_eq!(note_payload_read("same-bytes", "other-read"), None);
            assert_eq!(
                note_payload_read("same-bytes", "other-read").as_deref(),
                Some("other-read")
            );
        })
        .await;

        with_session_scope("active-read", async {
            invalidate_payload_reads_for_active_session();
            assert_eq!(
                note_payload_read("same-bytes", "active-read"),
                None,
                "the next post-rewrite read must send full content"
            );
        })
        .await;
        with_session_scope("other-read", async {
            assert_eq!(
                note_payload_read("same-bytes", "other-read").as_deref(),
                Some("other-read"),
                "another session keeps its valid reuse entry"
            );
        })
        .await;
        reset_read_index_for_test();
    }

    #[serial_test::serial]
    #[test]
    fn recording_an_item_decision_never_panics_without_answers() {
        record_item(
            distill_workspace::jev::flags::JevLever::A1FileToEdit,
            "rank",
            "no answers",
            None,
            None,
        );
    }

    /// The turn-status row reads only what happened: a decision inside the
    /// window is counted (refusals flagged), anything older is not, and a call
    /// in flight shows as such.
    #[serial_test::serial]
    #[test]
    fn turn_activity_counts_the_turn_window_and_refusals() {
        reset_activity_for_test();
        let before = std::time::Instant::now();
        note_decision("p1_tool_family", "defer", 410);
        note_decision("b2_micro_effort", "refused", 380);
        note_decision(LOCAL_LEVER, LOCAL_DECISION, 1200);
        // A window that opens *after* the two decisions, offset past any clock
        // granularity, so "was it in this turn?" cannot depend on timer detail.
        let after = before + std::time::Duration::from_millis(50);

        let turn = turn_activity(Some(before));
        assert_eq!(turn.decisions, 3);
        assert_eq!(turn.local_runs, 1, "a local route shows on the row");
        assert!(turn.refused, "a refusal must be visible on the row");
        assert_eq!(
            turn.last_latency_ms, 1200,
            "the newest latency is the one shown"
        );
        assert!(!turn.is_quiet());

        // A window that opens after the decisions saw none of them.
        assert!(turn_activity(Some(after)).is_quiet());

        // An unknown window (no turn anchor) still reports every recorded decision.
        assert_eq!(turn_activity(None).decisions, 3);

        let guard = JevInFlight::begin();
        let in_flight = turn_activity(Some(after));
        assert_eq!(in_flight.in_flight, 1);
        assert_eq!(in_flight.label().as_deref(), Some("jev…"));
        drop(guard);
        assert_eq!(turn_activity(Some(after)).in_flight, 0);
        note_decision("b2_micro_effort", "error", 10);
        note_decision("b2_micro_effort", "timeout", 20);
        note_decision(LOCAL_LEVER, "fallback", 30);
        let failed = turn_activity(Some(before));
        assert_eq!(failed.label().as_deref(), Some("jev ×6 ·local"));
        note_decision("b2_micro_effort", "keep", 40);
        assert_eq!(
            turn_activity(Some(before)).label().as_deref(),
            Some("jev ×7 ·local")
        );
        reset_activity_for_test();
    }

    /// A long turn consults Jev on every micro-action. The chip and the turn
    /// report must keep counting; a 64-slot ring used to freeze the label at
    /// `jev ×64` and under-count `Jev - Nx`.
    #[serial_test::serial]
    #[test]
    fn turn_activity_counts_every_decision_without_a_ceiling() {
        reset_activity_for_test();
        let start = std::time::Instant::now();
        for i in 0_u64..80 {
            note_decision("b2_micro_effort", "keep", i);
        }
        let turn = turn_activity(Some(start));
        assert_eq!(
            turn.decisions, 80,
            "every decision in the turn window is counted"
        );
        assert_eq!(
            turn.label().as_deref(),
            Some("jev ×80"),
            "the chip names the real count, not a ring size"
        );
        reset_activity_for_test();
    }

    /// The row names the model and effort the running call uses, next to
    /// the decision that chose them.
    #[serial_test::serial]
    #[test]
    fn the_row_names_the_route_of_the_running_call() {
        reset_activity_for_test();
        note_decision("b2_micro_effort", "high", 300);
        note_route(Some("gpt-6-sol"), Some("high"));
        assert_eq!(
            turn_activity(None).label().as_deref(),
            Some("jev 0.3s ·gpt-6-sol high")
        );
        reset_activity_for_test();
    }

    /// The chip text itself: quiet renders nothing, one answer shows its
    /// latency, repeats show a count, and preventive refusals are normal activity.
    #[test]
    fn the_chip_label_is_short_and_honest() {
        assert_eq!(JevTurnActivity::default().label(), None);
        assert_eq!(
            JevTurnActivity {
                in_flight: 1,
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev…")
        );
        assert_eq!(
            JevTurnActivity {
                decisions: 1,
                last_latency_ms: 420,
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev 0.4s")
        );
        assert_eq!(
            JevTurnActivity {
                decisions: 4,
                last_latency_ms: 420,
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev ×4")
        );
        assert_eq!(
            JevTurnActivity {
                decisions: 2,
                refused: true,
                in_flight: 1,
                last_latency_ms: 380,
                local_runs: 1,
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev ×2 ·local"),
            "a refused optimization is normal activity, not a Jev outage"
        );
        assert_eq!(
            JevTurnActivity {
                decisions: 3,
                last_latency_ms: 420,
                local_runs: 2,
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev ×3 ·local")
        );
        // The row names the routing of the call that is running: the model it
        // went to, plus the effort level in play.
        assert_eq!(
            JevTurnActivity {
                decisions: 3,
                last_latency_ms: 420,
                route: Some("local low".to_owned()),
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev ×3 ·local low")
        );
        assert_eq!(
            JevTurnActivity {
                decisions: 1,
                last_latency_ms: 900,
                route: Some("medium".to_owned()),
                ..Default::default()
            }
            .label()
            .as_deref(),
            Some("jev 0.9s ·medium")
        );
    }

    /// The route suffix says where the next call goes: the model the decision
    /// moved it to plus the level, a bare level, or nothing at all.
    #[test]
    #[serial_test::serial]
    fn the_route_suffix_describes_the_call() {
        reset_activity_for_test();
        note_route(Some("openrouter-qwen37"), Some("low"));
        assert_eq!(
            turn_activity(None).route.as_deref(),
            Some("openrouter-qwen37 low")
        );
        note_route(Some("qwen38-omlx"), None);
        assert_eq!(turn_activity(None).route.as_deref(), Some("qwen38-omlx"));
        note_route(None, Some("xhigh"));
        assert_eq!(turn_activity(None).route.as_deref(), Some("xhigh"));
        note_route(None, None);
        assert_eq!(
            turn_activity(None).route,
            None,
            "an untouched round adds nothing to the row"
        );
        reset_activity_for_test();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn activity_isolated_by_session_scope() {
        reset_activity_for_test();
        tokio::join!(
            with_session_scope("parent", async {
                note_decision("b2_micro_effort", "effort:low", 12);
                tokio::task::yield_now().await;
                note_route(Some("worker-model"), Some("low"));
            }),
            with_session_scope("child", async {
                note_decision("b2_micro_effort", "keep", 8);
                tokio::task::yield_now().await;
                note_route(Some("child-model"), Some("medium"));
            }),
        );

        let parent = turn_activity_for_session("parent", None);
        assert_eq!(parent.decisions, 1);
        assert_eq!(parent.route.as_deref(), Some("worker-model low"));
        let child = turn_activity_for_session("child", None);
        assert_eq!(child.decisions, 1);
        assert_eq!(child.route.as_deref(), Some("child-model medium"));
        assert!(turn_activity_for_session("other", None).is_quiet());
        reset_activity_for_test();
    }

    /// A subagent's decisions must reach the parent's chip and end-of-turn
    /// count, through every ancestor, without touching unrelated sessions.
    #[tokio::test]
    #[serial_test::serial]
    async fn child_decisions_count_for_every_ancestor() {
        reset_activity_for_test();
        register_child_session("child", "parent");
        register_child_session("grandchild", "child");
        with_session_scope("grandchild", async {
            note_decision("b2_micro_effort", "keep", 5);
        })
        .await;
        with_session_scope("child", async {
            note_decision("b2_micro_effort", "keep", 6);
        })
        .await;
        assert_eq!(turn_activity_for_session("grandchild", None).decisions, 1);
        assert_eq!(turn_activity_for_session("child", None).decisions, 2);
        assert_eq!(turn_activity_for_session("parent", None).decisions, 2);
        assert!(turn_activity_for_session("unrelated", None).is_quiet());
        // The parent's own decisions do not flow down to its children.
        with_session_scope("parent", async {
            note_decision("b2_micro_effort", "keep", 7);
        })
        .await;
        assert_eq!(turn_activity_for_session("child", None).decisions, 2);
        assert_eq!(turn_activity_for_session("parent", None).decisions, 3);
        reset_activity_for_test();
    }

    /// A link cycle must not loop or double-count a session.
    #[tokio::test]
    #[serial_test::serial]
    async fn child_links_that_cycle_count_once() {
        reset_activity_for_test();
        register_child_session("a", "b");
        register_child_session("b", "a");
        with_session_scope("a", async {
            note_decision("b2_micro_effort", "keep", 5);
        })
        .await;
        assert_eq!(turn_activity_for_session("a", None).decisions, 1);
        assert_eq!(turn_activity_for_session("b", None).decisions, 1);
        reset_activity_for_test();
    }
}
