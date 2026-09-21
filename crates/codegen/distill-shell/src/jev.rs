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

use crate::agent::config::{JevConfig, JevLocalConfig, JevTiersConfig};

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
            b2_light_model: cfg.ladder.b2_light_model,
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
}

// ---------------------------------------------------------------------------
// Runtime helper for catalogue call sites (todo.md areas A–D)
// ---------------------------------------------------------------------------

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
    let budget = item_budget(client);
    let in_flight = JevInFlight::begin();
    let outcome = tokio::time::timeout(budget, client.ask(&state, &questions)).await;
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
}

impl JevTurnActivity {
    /// Nothing to show: no decision in the window and nothing in flight.
    pub const fn is_quiet(&self) -> bool {
        self.decisions == 0 && self.in_flight == 0
    }

    /// The chip text, e.g. `jev…`, `jev·fallback`, `jev ×3`. A final route is
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
        if self.refused {
            return Some(format!("jev·fallback{suffix}"));
        }
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
    ACTIVE_SESSION_ID.scope(session_id.into(), future).await
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
        distill_workspace::jev::policy::TracingSink.record(record);
        note_decision(&record.lever, &record.decision, record.latency_ms);
    }
}

static LOCAL_MODEL_CONFIG: OnceLock<parking_lot::RwLock<JevLocalConfig>> = OnceLock::new();
static MODEL_TIERS: OnceLock<parking_lot::RwLock<JevTiersConfig>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Focused routing tests override only their own current-thread session;
    /// production still reads the process cache below.
    static TEST_LOCAL_MODEL_CONFIG: std::cell::RefCell<Option<JevLocalConfig>> =
        const { std::cell::RefCell::new(None) };
    static TEST_MODEL_TIERS: std::cell::RefCell<Option<JevTiersConfig>> =
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

/// Snapshot the worker selection used by both the TUI and the next model call.
pub fn tiers_cached() -> JevTiersConfig {
    #[cfg(test)]
    if let Some(config) = TEST_MODEL_TIERS.with(|config| config.borrow().clone()) {
        return config;
    }
    MODEL_TIERS
        .get_or_init(|| parking_lot::RwLock::new(resolve_config_from_disk().tiers))
        .read()
        .clone()
}

/// Publish a selection only after its atomic config write succeeds.
pub(crate) fn update_tier_model_cache(worker: bool, model: String, effort: String) {
    if worker {
        let mut config = MODEL_TIERS
            .get_or_init(|| parking_lot::RwLock::new(resolve_config_from_disk().tiers))
            .write();
        config.light = Some(model);
        config.light_effort = Some(effort);
    } else {
        let mut config = LOCAL_MODEL_CONFIG
            .get_or_init(|| parking_lot::RwLock::new(resolve_config_from_disk().local))
            .write();
        config.model = Some(model);
        config.effort = Some(effort);
    }
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
pub(crate) fn set_test_tier_config(config: JevTiersConfig) {
    TEST_MODEL_TIERS.with(|current| *current.borrow_mut() = Some(config));
}

#[cfg(test)]
pub(crate) fn clear_test_tier_config() {
    TEST_MODEL_TIERS.with(|current| *current.borrow_mut() = None);
}

/// What the `[jev.tiers]` block resolves to, for the surfaces that report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LightTierStatus {
    /// No light sibling configured: the tier question is never asked.
    Unset,
    /// Configured, but unusable, with the reason.
    Refused(String),
    /// Configured and usable.
    Ready {
        id: String,
        name: String,
        effort: String,
        window: u64,
    },
}

/// Whether two models can stand in for each other for one round.
///
/// Same family means the same provider, the same wire backend and the same
/// credential scheme: the pair must be interchangeable for one round of the same
/// conversation. A swap that changes the transport mid-conversation is not a
/// routing decision, it is a second session, so anything else is refused with
/// the reason instead of being attempted.
pub fn same_family(
    hard: &crate::agent::config::ModelInfo,
    light: &crate::agent::config::ModelInfo,
) -> Result<(), String> {
    if light.base_url != hard.base_url {
        return Err(format!(
            "`{}` runs on {} while the session model runs on {}: not the same provider",
            light.model,
            light.base_url.trim_end_matches('/'),
            hard.base_url.trim_end_matches('/'),
        ));
    }
    if light.api_backend != hard.api_backend {
        return Err(format!(
            "`{}` speaks {:?} while the session model speaks {:?}",
            light.model, light.api_backend, hard.api_backend
        ));
    }
    if light.auth_scheme != hard.auth_scheme {
        return Err(format!(
            "`{}` and the session model sign in differently; one credential must cover both",
            light.model
        ));
    }
    Ok(())
}

/// Catalog keys that can share the reasoning model's conversation.
pub fn compatible_worker_models(reasoning: &str) -> Vec<String> {
    let Ok(raw) = crate::config::load_effective_config() else {
        return Vec::new();
    };
    let Ok(cfg) = crate::agent::config::Config::new_from_toml_cfg(&raw) else {
        return Vec::new();
    };
    let models = crate::agent::config::resolve_model_list(&cfg, None);
    let Some(hard) = crate::agent::config::find_model_by_id(&models, reasoning) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|(_, entry)| same_family(&hard.info, &entry.info).is_ok())
        .map(|(id, _)| id.clone())
        .collect()
}

/// Validate a worker candidate against the currently selected reasoning model.
/// Both entries must exist in the resolved model catalog because the worker
/// needs the same endpoint, backend and credential scheme: the rule
/// [`same_family`] states. The conversation window is not part of it; a worker
/// too small for one round is handled where the round is routed.
pub fn validate_light_tier_candidate(hard_model: &str, light_model: &str) -> Result<(), String> {
    let raw = crate::config::load_effective_config()
        .map_err(|_| "the model catalog could not be read".to_owned())?;
    let cfg = crate::agent::config::Config::new_from_toml_cfg(&raw)
        .map_err(|_| "the model catalog could not be read".to_owned())?;
    let models = crate::agent::config::resolve_model_list(&cfg, None);
    let hard = crate::agent::config::find_model_by_id(&models, hard_model)
        .ok_or_else(|| format!("`{hard_model}` is not in the model catalog"))?;
    let light = crate::agent::config::find_model_by_id(&models, light_model)
        .ok_or_else(|| format!("`{light_model}` is not in the model catalog"))?;
    same_family(&hard.info, &light.info)
}

/// The light tier against the on-disk catalog, for the surfaces that report it.
///
/// `hard_model` is the session's own model, which only the caller knows. The
/// session itself resolves the same rule against the live catalog.
pub fn light_tier_status(hard_model: &str) -> LightTierStatus {
    let Some(id) = tiers_cached()
        .light
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
    else {
        return LightTierStatus::Unset;
    };
    let catalog = || {
        let raw = crate::config::load_effective_config().ok()?;
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&raw).ok()?;
        Some(crate::agent::config::resolve_model_list(&cfg, None))
    };
    let Some(models) = catalog() else {
        return LightTierStatus::Refused("the config could not be read".to_owned());
    };
    let Some(hard) = crate::agent::config::find_model_by_id(&models, hard_model) else {
        return LightTierStatus::Refused(format!("`{hard_model}` is not in the catalog"));
    };
    let Some(light) = crate::agent::config::find_model_by_id(&models, &id) else {
        return LightTierStatus::Refused(format!(
            "`{id}` is not a catalog entry: add a [model.{id}] block so the harness knows its \
             endpoint and window"
        ));
    };
    if let Err(reason) = same_family(&hard.info, &light.info) {
        return LightTierStatus::Refused(reason);
    }
    LightTierStatus::Ready {
        id,
        name: light
            .info
            .name
            .clone()
            .unwrap_or_else(|| light.info.model.clone()),
        effort: if let Some(effort) = tiers_cached().light_effort.as_deref() {
            effort.to_owned()
        } else if effort_auto_cached() {
            "auto".to_owned()
        } else {
            light
                .info
                .reasoning_effort
                .map(|effort| effort.to_string())
                .unwrap_or_else(|| "auto".to_owned())
        },
        window: light.info.context_window.get(),
    }
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
        note_decision("b2_light_model", "refused", 380);
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

    /// The chip text itself: quiet renders nothing, one answer shows its
    /// latency, repeats show a count, and a refusal outranks both.
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
            Some("jev·fallback ·local"),
            "a refusal keeps the local activity marker visible"
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

#[cfg(test)]
mod tier_rule_tests {
    use super::*;
    use crate::agent::config::ModelInfo;
    use std::num::NonZeroU64;

    fn model(slug: &str, base_url: &str) -> ModelInfo {
        ModelInfo {
            model: slug.to_owned(),
            base_url: base_url.to_owned(),
            context_window: NonZeroU64::new(272_000).unwrap(),
            ..ModelInfo::default()
        }
    }

    /// The rule the user asked for: hard and light must be the same family and
    /// share the conversation. Same provider, same backend, same credential —
    /// anything else is refused with the reason, before a round can be routed
    /// onto a transport the conversation never ran on.
    #[test]
    fn the_same_family_rule_takes_a_sibling_and_refuses_a_stranger() {
        let hard = model("gpt-6-astra", "https://chatgpt.com/backend-api/codex");
        let sibling = model("gpt-5.6-luna", "https://chatgpt.com/backend-api/codex");
        same_family(&hard, &sibling).expect("same host, backend and scheme");

        let other_provider = model("grok-4.6", "https://api.x.ai/v1");
        let reason = same_family(&hard, &other_provider).expect_err("a stranger is refused");
        assert!(reason.contains("not the same provider"), "{reason}");

        let mut other_backend = sibling.clone();
        other_backend.api_backend = Default::default();
        if other_backend.api_backend != hard.api_backend {
            let reason = same_family(&hard, &other_backend)
                .expect_err("a different wire backend is refused");
            assert!(reason.contains("speaks"), "{reason}");
        }

        let mut other_scheme = sibling.clone();
        other_scheme.auth_scheme = Default::default();
        if other_scheme.auth_scheme != hard.auth_scheme {
            let reason =
                same_family(&hard, &other_scheme).expect_err("a different sign-in is refused");
            assert!(reason.contains("sign in"), "{reason}");
        }
    }

    /// The trailing slash is not a family difference: endpoints are compared as
    /// configured, and the harness already normalises the ones it sends to.
    #[test]
    fn the_rule_reads_the_host_it_will_actually_call() {
        let hard = model("a", "https://host/v1");
        let light = model("b", "https://host/v1/");
        let reason =
            same_family(&hard, &light).expect_err("a trailing slash is a different string");
        assert!(reason.contains("not the same provider"), "{reason}");
    }
}
