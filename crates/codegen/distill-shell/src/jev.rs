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
            e_cheap_task: cfg.ladder.e_cheap_task,
            e_read_reuse: cfg.ladder.e_read_reuse,
            e_lane_choice: cfg.ladder.e_lane_choice,
            e_cheap_agent: cfg.ladder.e_cheap_agent,
            e_prompt_blocks: cfg.ladder.e_prompt_blocks,
            e_breaker: cfg.ladder.e_breaker,
            p1_tool_family: cfg.ladder.p1_tool_family,
            p2_read_shortlist: cfg.ladder.p2_read_shortlist,
            p3_compaction_recorte: cfg.ladder.p3_compaction_recorte,
            p6_skill_suggestion: cfg.ladder.p6_skill_suggestion,
            a1_file_to_edit: cfg.ladder.a1_file_to_edit,
            a3_log_lines: cfg.ladder.a3_log_lines,
            a4_web_results: cfg.ladder.a4_web_results,
            a5_memory_rank: cfg.ladder.a5_memory_rank,
            a6_test_to_run: cfg.ladder.a6_test_to_run,
            b1_intent_routing: cfg.ladder.b1_intent_routing,
            b2_model_tier: cfg.ladder.b2_model_tier,
            b2_micro_effort: cfg.ladder.b2_micro_effort,
            b2_local_model: cfg.ladder.b2_local_model,
            b2_reasoning_model: cfg.ladder.b2_reasoning_model,
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

/// Item budget for a chat backend, where the answer is generated token by token
/// (measured: 8 s for the permission battery on `qwen/qwen3.7-flash`).
const CHAT_ITEM_BUDGET: core::time::Duration = core::time::Duration::from_millis(15_000);

/// Client configuration from `[jev]`, falling back to the plan's defaults.
pub fn client_config_from(cfg: &JevConfig) -> JevClientConfig {
    use distill_workspace::jev::provider::{JevProvider, ReasoningShape};

    let defaults = JevClientConfig::default();
    // An unknown provider/shape name is ignored rather than fatal: the decision
    // path always has a working default, and a typo must not take it down.
    let provider = cfg
        .provider
        .as_deref()
        .and_then(JevProvider::from_name)
        .unwrap_or(defaults.provider);
    let reasoning_shape = cfg
        .reasoning_shape
        .as_deref()
        .and_then(ReasoningShape::from_name)
        .unwrap_or(defaults.reasoning_shape);
    JevClientConfig {
        base_url: cfg.base_url.clone().unwrap_or(defaults.base_url),
        model: cfg.model.clone().unwrap_or(defaults.model),
        timeout: cfg
            .timeout_ms
            .map(core::time::Duration::from_millis)
            .unwrap_or(defaults.timeout),
        api_key_env: cfg.api_key_env.clone().unwrap_or(defaults.api_key_env),
        max_state_bytes: cfg.max_state_bytes.unwrap_or(defaults.max_state_bytes),
        provider,
        reasoning_shape,
        reasoning_effort: cfg
            .reasoning_effort
            .clone()
            .unwrap_or(defaults.reasoning_effort),
        max_completion_tokens: cfg
            .max_completion_tokens
            .unwrap_or(defaults.max_completion_tokens),
        item_budget: cfg
            .item_budget_ms
            .map(core::time::Duration::from_millis)
            .unwrap_or_else(|| {
                if provider.generates_text() {
                    // A chat backend generates the answer before it can return one.
                    CHAT_ITEM_BUDGET
                } else {
                    // Both TypeSafe hosts answer the battery in about a second
                    // (measured 1.2 s for nine questions through OpenRouter).
                    defaults.item_budget
                }
            }),
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
        flags.c2_failure_triage = true;
        flags.a3_log_lines = true;
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
                (JevLever::C2FailureTriage, pack()),
                (JevLever::C5ErrorPriority, pack()),
                (JevLever::A3LogLines, pack()),
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
                [(JevLever::C2FailureTriage, pack()),],
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

    #[test]
    fn jev_config_cannot_enable_permission_decisions() {
        let config: JevConfig = toml::from_str(
            r#"enabled = true
[ladder]
permission_classifier = true
yolo_veto = true
p5_call_validation = true"#,
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
pub async fn ask_items<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
) -> [Option<distill_workspace::jev::types::JevAnswerSet>; N] {
    ask_items_with_flags(state, items, &flags_cached()).await
}

async fn ask_items_with_flags<const N: usize>(
    state: serde_json::Value,
    items: [(
        distill_workspace::jev::flags::JevLever,
        Option<std::collections::BTreeMap<String, distill_workspace::jev::types::Question>>,
    ); N],
    flags: &JevFlags,
) -> [Option<distill_workspace::jev::types::JevAnswerSet>; N] {
    use distill_workspace::jev::types::Usage;
    use std::collections::BTreeMap;

    let mut questions = BTreeMap::new();
    let mut first = None;
    let mut ids: [Vec<String>; N] = std::array::from_fn(|_| Vec::new());
    for (index, (lever, pack)) in items.into_iter().enumerate() {
        if !flags.lever_active(lever) {
            continue;
        }
        for (id, question) in pack.into_iter().flatten() {
            first.get_or_insert(lever);
            questions.insert(format!("{index}:{id}"), question);
            ids[index].push(id);
        }
    }
    let mut result = std::array::from_fn(|_| None);
    let Some(lever) = first else { return result };
    let Some(mut answers) = ask_item(lever, state, questions).await else {
        return result;
    };
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
        result[index] = Some(pack);
        answers.usage = Usage::default();
        answers.latency_ms = 0;
    }
    result
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
    #[cfg(test)]
    if let Some(answer) = TEST_DECISION_ANSWERS.with(|queue| {
        queue
            .borrow_mut()
            .as_mut()
            .map(|answers| answers.pop_front().unwrap_or(None))
    }) {
        return answer;
    }
    let flags = flags_cached();
    if !flags.lever_active(lever) {
        return None;
    }
    let client = client_cached()?;
    if !client.credential_present() {
        return None;
    }
    let session_id = active_session_id();
    let memo_key = decision_memo_key(client, lever, &state, &questions);
    let observed_client = active_usage_recorder().map(|recorder| {
        let turn_id = telemetry_context().1;
        let observer: distill_workspace::jev::types::AttemptObserver =
            std::sync::Arc::new(move |attempt| {
                record_workspace_attempt(
                    attempt,
                    "jev",
                    Some(lever.as_str().to_owned()),
                    Some(turn_id.clone()),
                    recorder.clone(),
                    true,
                );
            });
        client.with_call_observer(observer)
    });
    let request_client = observed_client.as_ref().unwrap_or(client);
    let budget = item_budget(request_client);
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
    match outcome {
        Ok(Ok(answers)) => Some(answers),
        Ok(Err(error)) => {
            tracing::debug!(
                lever = lever.as_str(),
                %error,
                "jev item call failed; keeping the current path"
            );
            // A silent failure is undiagnosable: the record says the item ran
            // and what the endpoint said, without ever logging bodies.
            record_item(lever, "error", &format!("{error}"), None, None);
            None
        }
        Err(_) => {
            tracing::debug!(
                lever = lever.as_str(),
                budget_ms = budget.as_millis() as u64,
                "jev item call timed out; keeping the current path"
            );
            record_item(
                lever,
                "timeout",
                &format!("budget {} ms", budget.as_millis()),
                None,
                None,
            );
            None
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

/// Records one catalogue decision so it lands in `~/.grok/logs/jev.jsonl`
/// through the shared decision sink and log target.
pub fn record_item(
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
    /// The reasoning model planning or reviewing this step **right now**, as
    /// `gpt-6-sol high` (`None` while the main model works alone).
    pub reasoning: Option<String>,
}

impl JevTurnActivity {
    /// Nothing to show: no decision in the window and nothing in flight.
    pub const fn is_quiet(&self) -> bool {
        self.decisions == 0 && self.in_flight == 0 && self.reasoning.is_none()
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
        // While the reasoning model advises, the row names it instead of the
        // main model's route: that call is the one the turn is waiting on.
        let suffix = match (&self.reasoning, &self.route) {
            (Some(reasoning), _) => format!(" ·reasoning {reasoning}"),
            (None, Some(route)) => format!(" ·{route}"),
            (None, None) if self.local_runs > 0 => " ·local".to_owned(),
            (None, None) => String::new(),
        };
        match self.decisions {
            0 if self.route.is_some() || self.reasoning.is_some() => {
                Some(format!("model{suffix}"))
            }
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
    let Ok(mut index) = read_index().lock() else {
        return;
    };
    index.retain(|(owner, _), _| owner != &session_id);
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

pub(crate) fn record_workspace_attempt(
    attempt: distill_workspace::jev::types::AttemptRecord,
    role: &str,
    task_id: Option<String>,
    turn_id: Option<String>,
    recorder: distill_chat_state::ChatStateHandle,
    attribute_to_prompt: bool,
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
}

#[derive(Debug, Default)]
struct SessionActivityState {
    ring: std::collections::VecDeque<JevActivity>,
    in_flight: u32,
    route: Option<String>,
    reasoning: Option<String>,
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

/// Remembers one decision for the turn-status row. Never fails the caller.
pub fn note_decision(lever: &str, decision: &str, latency_ms: u64) {
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    let session = state.sessions.entry(active_session_id()).or_default();
    session.ring.push_back(JevActivity {
        lever: lever.to_owned(),
        decision: decision.to_owned(),
        latency_ms,
        at: std::time::Instant::now(),
    });
}

/// Sets the routing of the call that is about to run, as the row shows it.
///
/// Called once per model call, after the decision layer. `engine` names the
/// model the decision moved the call to (`openrouter-qwen37`), so the row says
/// which model is about to run and not only that something changed; `None` means
/// the session's own model, and then only the level shows (`high`).
pub fn note_route(engine: Option<&str>, level: Option<&str>) {
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
        reasoning: session.reasoning.clone(),
        ..Default::default()
    };
    for entry in session.ring.iter().rev() {
        if let Some(since) = since
            && entry.at < since
        {
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
    }
}

/// Shows the reasoning model on the row for as long as it advises the main
/// model, and clears it on every exit (answer, error, or a cancelled turn).
pub struct ReasoningInFlight {
    session: String,
}

impl ReasoningInFlight {
    /// `label` is what the row shows after `reasoning`, e.g. `gpt-6-sol high`.
    pub fn begin(label: String) -> Self {
        let session = active_session_id();
        if let Ok(mut state) = activity_state().lock() {
            state.sessions.entry(session.clone()).or_default().reasoning = Some(label);
        }
        Self { session }
    }
}

impl Drop for ReasoningInFlight {
    fn drop(&mut self) {
        if let Ok(mut state) = activity_state().lock()
            && let Some(session) = state.sessions.get_mut(&self.session)
        {
            session.reasoning = None;
        }
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
    /// `Some(None)` pins "no reasoning model" regardless of the disk config.
    static TEST_REASONING_MODEL: std::cell::RefCell<Option<Option<String>>> =
        const { std::cell::RefCell::new(None) };
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

/// The configured reasoning model (`[models].reasoning`), or `None` when the
/// main model works alone. Read from the effective config on every call, so a
/// `/reasoning-model` pick in the pager reaches the session process at once.
pub fn reasoning_model() -> Option<String> {
    #[cfg(test)]
    if let Some(model) = TEST_REASONING_MODEL.with(|model| model.borrow().clone()) {
        return model;
    }
    crate::config::load_effective_config()
        .ok()?
        .get("models")?
        .get("reasoning")?
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
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
pub(crate) fn set_test_reasoning_model(model: Option<String>) {
    TEST_REASONING_MODEL.with(|current| *current.borrow_mut() = Some(model));
}

#[cfg(test)]
pub(crate) fn clear_test_reasoning_model() {
    TEST_REASONING_MODEL.with(|current| *current.borrow_mut() = None);
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

/// The flags resolved once per process (configuration does not change mid-run).
fn flags_cached() -> distill_workspace::jev::flags::JevFlags {
    static FLAGS: OnceLock<distill_workspace::jev::flags::JevFlags> = OnceLock::new();
    *FLAGS.get_or_init(|| flags_from(&resolve_config_from_disk()))
}

/// One client per process, built lazily and only when a credential exists.
fn client_cached() -> Option<&'static distill_workspace::jev::JevClient> {
    static CLIENT: OnceLock<Option<distill_workspace::jev::JevClient>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let cfg = resolve_config_from_disk();
            match distill_workspace::jev::JevClient::new(client_config_from(&cfg)) {
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
        let client = distill_workspace::jev::JevClient::new(client_config_from(&cfg))
            .expect("client builds without I/O");
        let budget = item_budget(&client);
        assert!(budget <= std::time::Duration::from_millis(4_000));
        assert!(!budget.is_zero());

        // A chat backend generates its answer token by token; measured at 8 s for
        // the permission battery, so the cap must let it finish (and never
        // exceed the client's own deadline).
        let chat = JevConfig {
            provider: Some("openrouter".to_owned()),
            ..JevConfig::default()
        };
        let client = distill_workspace::jev::JevClient::new(client_config_from(&chat))
            .expect("client builds without I/O");
        let budget = item_budget(&client);
        assert!(
            budget > std::time::Duration::from_millis(8_000),
            "a chat backend gets room to answer, got {budget:?}"
        );
        assert!(budget <= client.config().timeout);
    }

    #[test]
    fn decision_memo_key_tracks_state_questions_and_model() {
        use distill_workspace::jev::flags::JevLever;
        use distill_workspace::jev::types::Question;
        use std::collections::BTreeMap;

        let client = distill_workspace::jev::JevClient::new(
            client_config_from(&JevConfig::default()),
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
            model: Some("jev-pinned".to_owned()),
            ..JevConfig::default()
        };
        let changed_client = distill_workspace::jev::JevClient::new(
            client_config_from(&changed_model),
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
        note_decision("b2_reasoning_model", "refused", 380);
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

    /// While the reasoning model advises, the row must say so: the turn is
    /// waiting on that call, not on the main model. Once it answers (or the
    /// call fails), the row goes back to the main model's route.
    #[serial_test::serial]
    #[test]
    fn the_row_names_the_reasoning_model_only_while_it_advises() {
        reset_activity_for_test();
        note_decision("b2_reasoning_model", "consult", 300);
        note_route(Some("gpt-6-luna"), Some("medium"));
        assert_eq!(
            turn_activity(None).label().as_deref(),
            Some("jev 0.3s ·gpt-6-luna medium")
        );
        let advising = ReasoningInFlight::begin("gpt-6-sol high".to_owned());
        assert_eq!(
            turn_activity(None).label().as_deref(),
            Some("jev 0.3s ·reasoning gpt-6-sol high")
        );
        drop(advising);
        assert_eq!(
            turn_activity(None).label().as_deref(),
            Some("jev 0.3s ·gpt-6-luna medium")
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
}
