// Modified for Distill by Samuel Fajreldines, 2026.
//! The cheap lanes on the session side: who may call, how often, and what is
//! recorded.
//!
//! The transports and the task contracts live in `distill_workspace::jev`
//! (`cheap`, `tasks`); this module is the policy around them, and it exists so
//! every lane answers the same four questions the same way:
//!
//! * **May it run?** the master switch, the lane's own key and a resolvable
//!   credential, read from one place.
//! * **How is the cheap model reached?** from a resolved model entry (base URL,
//!   slug, key), never from a second copy of the config.
//! * **How many times?** required calls are never suppressed. Optional
//!   compression gets a small, turn-scoped refusal budget keyed to the exact
//!   endpoint/model/task/effort, so a flaky endpoint is visible without
//!   repeatedly paying for the same unusable opportunity.
//! * **What gets recorded?** one content-free line per call — lane, task id,
//!   decision, model, tokens, latency — through the same recorder as every other
//!   decision, so the TUI, the turn report and the log cannot disagree.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex, OnceLock};
use std::time::Duration;

use distill_workspace::jev::cheap::{DEFAULT_BASE_URL, DEFAULT_MODELS};
use distill_workspace::jev::flags::JevLever;
use distill_workspace::jev::tasks;
use distill_sampling_types::{ConversationItem, ConversationRequest, LengthPolicy, ReasoningEffort};

/// A byte is the conservative upper bound for one input token when the
/// tokenizer is not available at this layer.  The worker request also keeps a
/// fixed framing reserve for the system/task/question wrapper.
const WORKER_OVERHEAD_TOKENS: u64 = 256;
const WORKER_FRAMING_BYTES: usize = 4 * 1024;
const OPTIONAL_COMPRESSION_FAILURE_LIMIT: u8 = 2;
const OPTIONAL_COMPRESSION_RECOVERY_ROUNDS: u64 = 2;
/// The direct utility lane is for tiny closed tasks, never whole-agent work.
const UTILITY_MAX_PAYLOAD_BYTES: usize = 24 * 1024;
const UTILITY_MAX_QUESTION_BYTES: usize = 2 * 1024;
const UTILITY_MAX_DECISION_STATE_BYTES: usize = 32 * 1024;
const UTILITY_JEV_CONFIDENCE_THRESHOLD: f64 = 0.98;
const UTILITY_PRE_APPROVAL: &str = "pre_approval";
const UTILITY_POST_REVIEW: &str = "post_review";
const UTILITY_DECISION_ID: &str = "decision";
const UTILITY_TASK_ALLOWLIST: &[&str] = &[tasks::DISPLAY_FRAGMENT_TASK];

/// Identity of one optional compression opportunity.
///
/// The task-local scope already gives this state the lifetime of one session
/// turn. Keeping the session/turn in the key as well makes accidental reuse
/// across nested scopes impossible and makes the isolation contract explicit.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OptionalCompressionKey {
    session_id: String,
    turn_id: String,
    endpoint: String,
    model: String,
    task_id: String,
    effort: String,
}

#[derive(Clone, Copy, Debug, Default)]
struct OptionalCompressionHealth {
    failures: u8,
    retry_after_round: u64,
}

tokio::task_local! {
    static OPTIONAL_COMPRESSION_FAILURES:
        RefCell<HashMap<OptionalCompressionKey, OptionalCompressionHealth>>;
}

/// Gives optional compression a fresh budget for one production session turn.
pub(crate) async fn with_optional_compression_scope<F>(future: F) -> F::Output
where
    F: Future,
{
    OPTIONAL_COMPRESSION_FAILURES
        .scope(RefCell::new(HashMap::new()), future)
        .await
}

/// Build an identity without retaining the source itself in the turn state.
pub(crate) fn optional_compression_key(
    endpoint: &str,
    model: &str,
    task_id: &str,
    effort: &str,
) -> OptionalCompressionKey {
    let (session_id, turn_id, _) = crate::jev::telemetry_context();
    OptionalCompressionKey {
        session_id,
        turn_id,
        endpoint: endpoint.to_owned(),
        model: model.to_owned(),
        task_id: task_id.to_owned(),
        effort: effort.to_owned(),
    }
}

/// Whether an optional compression attempt may pay for another request.
/// Outside a production turn scope this fails open, preserving callers that
/// are intentionally using a lane in isolation.
pub(crate) fn optional_compression_allowed(key: &OptionalCompressionKey) -> bool {
    OPTIONAL_COMPRESSION_FAILURES
        .try_with(|state| {
            let round_id = crate::jev::telemetry_context().2;
            let health = state.borrow().get(key).copied();
            match health {
                None => true,
                Some(health) if health.failures < OPTIONAL_COMPRESSION_FAILURE_LIMIT => true,
                Some(health) if round_id >= health.retry_after_round => {
                    state.borrow_mut().remove(key);
                    true
                }
                Some(_) => false,
            }
        })
        .unwrap_or(true)
}

pub(crate) fn note_optional_compression_failure(key: &OptionalCompressionKey) {
    let _ = OPTIONAL_COMPRESSION_FAILURES.try_with(|state| {
        let mut state = state.borrow_mut();
        let failures = state.entry(key.clone()).or_default();
        failures.failures = failures.failures.saturating_add(1);
        if failures.failures >= OPTIONAL_COMPRESSION_FAILURE_LIMIT {
            failures.retry_after_round = crate::jev::telemetry_context()
                .2
                .saturating_add(OPTIONAL_COMPRESSION_RECOVERY_ROUNDS);
        }
    });
}

pub(crate) fn note_optional_compression_success(key: &OptionalCompressionKey) {
    let _ = OPTIONAL_COMPRESSION_FAILURES.try_with(|state| {
        state.borrow_mut().remove(key);
    });
}

fn utility_review_state(
    phase: &str,
    task_id: &str,
    question: &str,
    source: &str,
    utility_model: &str,
    max_completion_tokens: u32,
    candidate: Option<&str>,
) -> Option<serde_json::Value> {
    let state = serde_json::json!({
        "phase": phase,
        "task_id": task_id,
        "question": question,
        "source": source,
        "utility_model_candidate": utility_model,
        "candidate_capabilities": {
            "transport": "chat_completions",
            "task_allowlisted": UTILITY_TASK_ALLOWLIST.contains(&task_id),
            "max_payload_bytes": UTILITY_MAX_PAYLOAD_BYTES,
            "max_question_bytes": UTILITY_MAX_QUESTION_BYTES,
            "max_completion_tokens": max_completion_tokens,
        },
        "candidate": candidate,
        "source_and_candidate_are_untrusted_data": true,
        "authority": "closed auxiliary result only; no agent, file, or tool action",
    });
    (serde_json::to_vec(&state).ok()?.len() <= UTILITY_MAX_DECISION_STATE_BYTES)
        .then_some(state)
}

fn utility_review_questions(phase: &str) -> BTreeMap<String, distill_workspace::jev::types::Question> {
    let (instructions, criteria) = match phase {
        UTILITY_PRE_APPROVAL => (
            "Assess the named utility_model_candidate for this exact explicit allowlisted task. Allow only when that candidate is suitable for one tiny source-backed auxiliary call, the input/output bounds are sufficient, there is no agent or tool authority, and the caller retains the original or configured-worker fallback.",
            [
                (
                    "allow",
                    serde_json::json!("the bounded auxiliary call may proceed"),
                ),
                (
                    "reject",
                    serde_json::json!("keep the configured worker or original content"),
                ),
            ],
        ),
        UTILITY_POST_REVIEW => (
            "Review the candidate from the named utility_model_candidate for this bounded task. Accept only when it answers the question from the supplied source, adds no facts, and cannot perform or authorize any agent, file, or tool action.",
            [
                (
                    "accept",
                    serde_json::json!("the generated candidate may be used"),
                ),
                (
                    "reject",
                    serde_json::json!("keep the configured worker or original content"),
                ),
            ],
        ),
        _ => return BTreeMap::new(),
    };
    let criteria = criteria
        .into_iter()
        .map(|(label, description)| (label.to_owned(), description))
        .collect();
    [(
        UTILITY_DECISION_ID.to_owned(),
        distill_workspace::jev::types::Question::choice(instructions, criteria)
            .expect("utility review criteria are non-empty"),
    )]
    .into_iter()
    .collect()
}

fn utility_review_is_high_confidence(
    answers: &distill_workspace::jev::types::JevAnswerSet,
    expected: &str,
) -> bool {
    let confidence = answers.confidence(UTILITY_DECISION_ID);
    let confidence_ok = confidence.is_some_and(|value| {
        value.is_finite() && (UTILITY_JEV_CONFIDENCE_THRESHOLD..=1.0).contains(&value)
    });
    let probability_ok = answers
        .probability(UTILITY_DECISION_ID, expected)
        .is_none_or(|value| {
            value.is_finite() && (UTILITY_JEV_CONFIDENCE_THRESHOLD..=1.0).contains(&value)
        });
    answers.choice(UTILITY_DECISION_ID) == Some(expected) && confidence_ok && probability_ok
}

async fn ask_utility_review(
    lever: JevLever,
    phase: &str,
    expected: &str,
    task_id: &str,
    question: &str,
    source: &str,
    utility_model: &str,
    max_completion_tokens: u32,
    candidate: Option<&str>,
) -> Option<distill_workspace::jev::types::JevAnswerSet> {
    let state = utility_review_state(
        phase,
        task_id,
        question,
        source,
        utility_model,
        max_completion_tokens,
        candidate,
    )?;
    if phase == UTILITY_POST_REVIEW {
        test_post_review_pause_if_configured().await;
    }
    let answers = crate::jev::ask_item(lever, state, utility_review_questions(phase)).await;
    let approved = answers
        .as_ref()
        .is_some_and(|answers| utility_review_is_high_confidence(answers, expected));
    let confidence = answers
        .as_ref()
        .and_then(|answers| answers.confidence(UTILITY_DECISION_ID));
    crate::jev::record_item(
        lever,
        if approved { phase } else { "defer" },
        if approved {
            "bounded Jev utility gate approved"
        } else {
            "bounded Jev utility gate was missing, uncertain, or rejected"
        },
        confidence,
        answers.as_ref(),
    );
    approved.then(|| answers.expect("approved utility review has an answer"))
}

struct CompletedUtilityAttemptGuard {
    attempts: std::sync::Arc<std::sync::Mutex<Vec<distill_workspace::jev::types::AttemptRecord>>>,
    recorder: Option<distill_chat_state::ChatStateHandle>,
    task_id: String,
    turn_id: String,
    attribute_to_prompt: bool,
}

impl CompletedUtilityAttemptGuard {
    fn new(
        attempts: std::sync::Arc<
            std::sync::Mutex<Vec<distill_workspace::jev::types::AttemptRecord>>,
        >,
        recorder: Option<distill_chat_state::ChatStateHandle>,
        task_id: String,
        turn_id: String,
        attribute_to_prompt: bool,
    ) -> Self {
        Self {
            attempts,
            recorder,
            task_id,
            turn_id,
            attribute_to_prompt,
        }
    }

    fn take(&self) -> Vec<distill_workspace::jev::types::AttemptRecord> {
        self.attempts
            .lock()
            .expect("utility attempt lock")
            .drain(..)
            .collect()
    }

    fn record(&self, attempts: Vec<distill_workspace::jev::types::AttemptRecord>) {
        let Some(recorder) = self.recorder.as_ref() else {
            return;
        };
        for attempt in attempts {
            crate::jev::record_workspace_attempt(
                attempt,
                "utility",
                Some(self.task_id.clone()),
                Some(self.turn_id.clone()),
                recorder.clone(),
                self.attribute_to_prompt,
            );
        }
    }
}

impl Drop for CompletedUtilityAttemptGuard {
    fn drop(&mut self) {
        // A completed provider response may be sitting behind the post-review
        // await. If that await is cancelled, retain its usage and billing but
        // mark the attempt rejected: the candidate was never accepted for use.
        let mut attempts = self.take();
        for attempt in &mut attempts {
            if matches!(
                attempt.status,
                distill_workspace::jev::types::AttemptStatus::Completed
            ) {
                attempt.status = distill_workspace::jev::types::AttemptStatus::Rejected;
            }
        }
        self.record(attempts);
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestPostReviewPause {
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
}

#[cfg(test)]
static TEST_POST_REVIEW_PAUSE: OnceLock<Mutex<Option<TestPostReviewPause>>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn begin_test_post_review_pause() -> (
    std::sync::Arc<tokio::sync::Notify>,
    std::sync::Arc<tokio::sync::Notify>,
) {
    let entered = std::sync::Arc::new(tokio::sync::Notify::new());
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    *TEST_POST_REVIEW_PAUSE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .expect("post-review pause lock") = Some(TestPostReviewPause {
        entered: entered.clone(),
        release: release.clone(),
    });
    (entered, release)
}

#[cfg(test)]
pub(crate) fn clear_test_post_review_pause() {
    if let Some(pause) = TEST_POST_REVIEW_PAUSE.get() {
        *pause.lock().expect("post-review pause lock") = None;
    }
}

#[cfg(test)]
async fn test_post_review_pause_if_configured() {
    let pause = TEST_POST_REVIEW_PAUSE
        .get()
        .and_then(|pause| pause.lock().ok().and_then(|pause| pause.clone()));
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.release.notified().await;
    }
}

#[cfg(not(test))]
async fn test_post_review_pause_if_configured() {}

#[cfg(test)]
pub(crate) fn test_utility_review_answer(
    choice: &str,
) -> distill_workspace::jev::types::JevAnswerSet {
    use distill_workspace::jev::types::{Answer, JevAnswerSet, Usage};

    JevAnswerSet {
        model: "test-jev".to_owned(),
        answers: [(
            UTILITY_DECISION_ID.to_owned(),
            Answer::Choice {
                choice: choice.to_owned(),
                probabilities: [(choice.to_owned(), 1.0)].into_iter().collect(),
                confidence: Some(1.0),
            },
        )]
        .into_iter()
        .collect(),
        usage: Usage::default(),
        request_id: None,
        latency_ms: 0,
    }
}

/// The cheap-model spec the harness ships with, in priority order.
///
/// Used when `[jev.local] model` says nothing: out of the box the cheap lanes
/// run on the shipped OpenRouter chain rather than staying off.
pub fn default_model_spec() -> String {
    DEFAULT_MODELS.join(",")
}

/// One cheap generation at a time, process-wide.
///
/// The app serialised its local model for the same reason this exists: several
/// lanes can want the worker at once (the tool-result path, a routed round, a
/// subagent), and a queue is cheaper than the contention — and it makes the
/// per-turn call count the number that actually happened.
fn lane_queue() -> &'static tokio::sync::Mutex<()> {
    static QUEUE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    QUEUE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// A resolved cheap worker: the client plus the model entry it came from.
pub struct CheapLane {
    pub client: distill_workspace::jev::cheap::CheapClient,
    /// The catalog id the entry resolved to, for the record.
    pub slug: String,
}

impl CheapLane {
    /// The lane for a cheap-model spec that is not a catalog entry: a
    /// comma-separated priority list of OpenRouter model ids, on the shipped
    /// OpenRouter transport and the saved or environment key.
    ///
    /// The transport is not invented here — it is the pair the cheap lane has
    /// always defaulted to ([`DEFAULT_BASE_URL`] and the OpenRouter credential store) —
    /// so a spec that names slugs needs no `[model.*]` entry of its own.
    pub fn from_spec(spec: &str) -> Option<Self> {
        Self::from_sampler_config(&Self::standalone_sampler_config(spec)?)
    }

    /// The sampler config behind [`Self::from_spec`], for the lane that routes a
    /// whole round rather than a closed task.
    ///
    /// A raw spec carries no catalog entry, so nothing declares its window or
    /// backend: the shipped OpenRouter pair is assumed for the transport, and the
    /// caller decides the context ceiling (`[jev.local] max_context_tokens` is
    /// the owner's own cap and needs no model metadata).
    pub fn standalone_sampler_config(spec: &str) -> Option<distill_sampler::SamplerConfig> {
        let api_key = crate::openrouter_auth::api_key().ok().flatten()?;
        Some(distill_sampler::SamplerConfig {
            api_key: Some(api_key),
            base_url: DEFAULT_BASE_URL.to_owned(),
            model: spec.to_owned(),
            ..Default::default()
        })
    }

    /// Builds the lane from a resolved model entry.
    ///
    /// The key is captured once, at resolution time, and handed to the client as
    /// a resolver: the client still reads it at call time, and nothing else in
    /// the process can see it.
    pub fn from_sampler_config(cfg: &distill_sampler::SamplerConfig) -> Option<Self> {
        // CheapClient is a closed Chat Completions transport with bearer auth.
        // Defer catalog entries that require another wire backend or auth
        // scheme so the configured worker path can keep its actual pins.
        if cfg.api_backend != distill_sampling_types::ApiBackend::ChatCompletions
            || cfg.auth_scheme != distill_sampler::AuthScheme::Bearer
        {
            return None;
        }
        let api_key = cfg.api_key.clone()?;
        if api_key.trim().is_empty()
            || cfg.base_url.trim().is_empty()
            || cfg.model.trim().is_empty()
        {
            return None;
        }
        let config = distill_workspace::jev::cheap::CheapConfig {
            base_url: cfg.base_url.trim_end_matches('/').to_owned(),
            model: cfg.model.clone(),
            reasoning_effort: crate::jev::local_config_cached()
                .effort
                .as_deref()
                .filter(|value| *value != "auto")
                .unwrap_or("none")
                .to_owned(),
            reasoning_shape: if crate::jev::local_config_cached()
                .effort
                .as_deref()
                .is_some_and(|value| value != "auto")
            {
                match cfg.reasoning_shape {
                    // Sampler `None` means no shape translation: preserve the
                    // caller's explicit effort spelling on Jev's effort wire.
                    crate::sampling::types::ReasoningShape::None => {
                        distill_workspace::jev::provider::ReasoningShape::Effort
                    }
                    crate::sampling::types::ReasoningShape::Effort => {
                        distill_workspace::jev::provider::ReasoningShape::Effort
                    }
                    crate::sampling::types::ReasoningShape::MaxTokens => {
                        distill_workspace::jev::provider::ReasoningShape::MaxTokens
                    }
                    crate::sampling::types::ReasoningShape::Disabled => {
                        distill_workspace::jev::provider::ReasoningShape::Disabled
                    }
                }
            } else {
                distill_workspace::jev::provider::ReasoningShape::Disabled
            },
            // A catalog entry may advertise a large general-purpose ceiling,
            // but this lane only sends closed auxiliary tasks. Keep the cap on
            // the actual wire request, before any paid response exists.
            max_completion_tokens: cfg
                .max_completion_tokens
                .unwrap_or(distill_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS)
                .min(distill_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS)
                .max(1),
            ..distill_workspace::jev::cheap::CheapConfig::default()
        };
        let slug = cfg.model.clone();
        let client = distill_workspace::jev::cheap::CheapClient::with_key_resolver(
            config,
            std::sync::Arc::new(move |_| Some(api_key.clone())),
        )
        .ok()?;
        Some(Self { client, slug })
    }

    /// Runs one registered catalogue task and records what happened.
    ///
    /// `None` means the caller keeps today's bytes — and that includes every
    /// reason the task could be refused, which are all recorded with their own
    /// label so a repeated refusal is visible instead of silent.
    pub async fn run_task(
        &self,
        lever: JevLever,
        task_id: &str,
        payload: &str,
        question: &str,
    ) -> Option<tasks::TaskOutcome> {
        self.run_task_with_acceptance(lever, task_id, payload, question, true, |_| true)
            .await
    }

    /// Runs one task while letting the caller apply its consumer-specific
    /// acceptance contract before the physical attempt is recorded. A task
    /// guard can accept a quoted answer that the final consumer still cannot
    /// use; that response is one rejected attempt, not a second generation.
    pub async fn run_task_with_acceptance<F>(
        &self,
        lever: JevLever,
        task_id: &str,
        payload: &str,
        question: &str,
        attribute_to_prompt: bool,
        accepts: F,
    ) -> Option<tasks::TaskOutcome>
    where
        F: Fn(&str) -> bool,
    {
        if !crate::jev::lever_active(lever) {
            return None;
        }
        if !UTILITY_TASK_ALLOWLIST.contains(&task_id) {
            crate::jev::record_item(
                lever,
                "defer:task-bound",
                &format!("task `{task_id}` is outside the bounded utility allowlist"),
                None,
                None,
            );
            return None;
        }
        if payload.len() > UTILITY_MAX_PAYLOAD_BYTES
            || question.len() > UTILITY_MAX_QUESTION_BYTES
        {
            crate::jev::record_item(
                lever,
                "defer:input-bound",
                &format!(
                    "task `{task_id}` exceeds the bounded utility input/question limits"
                ),
                None,
                None,
            );
            return None;
        }
        let Some(task_spec) = tasks::spec(task_id) else {
            crate::jev::record_item(
                lever,
                "defer:task-bound",
                &format!("task `{task_id}` is not registered in the task catalog"),
                None,
                None,
            );
            return None;
        };
        let Some(prepared_payload) = tasks::prepare(task_spec, payload) else {
            crate::jev::record_item(
                lever,
                "defer:input-bound",
                &format!("task `{task_id}` has an empty or precleaned-empty payload"),
                None,
                None,
            );
            return None;
        };
        if !tasks::task_for(task_spec, &prepared_payload, question)
            .fits(self.client.config().max_input_bytes)
        {
            crate::jev::record_item(
                lever,
                "defer:input-bound",
                &format!("task `{task_id}` does not fit the utility input bound"),
                None,
                None,
            );
            return None;
        }
        let optional_key = matches!(lever, JevLever::ECheapCompress).then(|| {
            optional_compression_key(
                &self.client.config().endpoint(),
                &self.client.config().model,
                task_id,
                &self.client.config().reasoning_effort,
            )
        });
        // Serialised: one cheap generation at a time across the whole process.
        let _one_at_a_time = lane_queue().lock().await;
        if let Some(key) = optional_key.as_ref()
            && !optional_compression_allowed(key)
        {
            crate::jev::record_item(
                lever,
                "defer:failure-bound",
                &format!("task `{task_id}` reached the optional compression failure bound"),
                None,
                None,
            );
            return None;
        }
        let utility_model = self.client.config().model.clone();
        let max_completion_tokens = self.client.config().max_completion_tokens;
        if ask_utility_review(
            lever,
            UTILITY_PRE_APPROVAL,
            "allow",
            task_id,
            question,
            payload,
            &utility_model,
            max_completion_tokens,
            None,
        )
        .await
        .is_none()
        {
            return None;
        }
        let (session_id, turn_id, round_id) = crate::jev::telemetry_context();
        let span = tracing::info_span!(target: "jev.decision", "utility_context",
            session_id, turn_id, round_id, utility_call_id = %uuid::Uuid::new_v4());
        let recorder = crate::jev::active_usage_recorder();
        let completed_attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let completed_attempt_guard = CompletedUtilityAttemptGuard::new(
            completed_attempts.clone(),
            recorder.clone(),
            task_id.to_owned(),
            turn_id.clone(),
            attribute_to_prompt,
        );
        let physical_attempt = std::sync::Arc::new(AtomicBool::new(false));
        let optional_failure_recorded = std::sync::Arc::new(AtomicBool::new(false));
        let observed_client = (recorder.is_some() || optional_key.is_some()).then(|| {
            let completed_attempts = completed_attempts.clone();
            let observer_recorder = recorder.clone();
            let task_id = task_id.to_owned();
            let turn_id = turn_id.clone();
            let optional_key = optional_key.clone();
            let physical_attempt = physical_attempt.clone();
            let optional_failure_recorded = optional_failure_recorded.clone();
            let observer: distill_workspace::jev::types::AttemptObserver =
                std::sync::Arc::new(move |attempt| {
                    physical_attempt.store(true, Ordering::Release);
                    if matches!(
                        attempt.status,
                        distill_workspace::jev::types::AttemptStatus::Completed
                    ) {
                        if observer_recorder.is_some() {
                            completed_attempts
                                .lock()
                                .expect("utility attempt lock")
                                .push(attempt);
                        }
                    } else {
                        if let Some(key) = optional_key.as_ref()
                            && !optional_failure_recorded.swap(true, Ordering::AcqRel)
                        {
                            note_optional_compression_failure(key);
                        }
                        // Failed, rejected, and cancelled attempts are already
                        // final at the transport boundary. Record each one
                        // immediately so fallback chains and cancellation do
                        // not disappear behind the consumer gate.
                        if let Some(recorder) = observer_recorder.as_ref() {
                            crate::jev::record_workspace_attempt(
                                attempt,
                                "utility",
                                Some(task_id.clone()),
                                Some(turn_id.clone()),
                                recorder.clone(),
                                attribute_to_prompt,
                            );
                        }
                    }
                });
            self.client.with_call_observer(observer)
        });
        let request_client = observed_client.as_ref().unwrap_or(&self.client);
        let mut outcome = tracing::Instrument::instrument(
            tasks::run(request_client, task_id, payload, question),
            span,
        )
        .await;
        let accepted_by_consumer = outcome
            .as_ref()
            .is_some_and(|outcome| accepts(&outcome.text));
        let post_review = outcome.as_ref().and_then(|outcome| {
            accepted_by_consumer.then(|| (outcome.text.clone(), outcome.answer.model.clone()))
        });
        let mut post_review_rejected = false;
        if let Some((candidate, post_review_model)) = post_review.as_ref()
            && ask_utility_review(
                lever,
                UTILITY_POST_REVIEW,
                "accept",
                task_id,
                question,
                payload,
                post_review_model.as_str(),
                max_completion_tokens,
                Some(candidate.as_str()),
            )
            .await
            .is_none()
        {
            post_review_rejected = true;
            outcome = None;
        }
        let mut completed_attempts = completed_attempt_guard.take();
        if (outcome.is_none() || !accepted_by_consumer)
            && let Some(attempt) = completed_attempts.last_mut()
        {
            // `tasks::run` applies its own source-span guard after the cheap
            // transport has returned. Keep that one physical response
            // rejected in the existing attempt row; never emit a second row.
            attempt.status = distill_workspace::jev::types::AttemptStatus::Rejected;
        }
        completed_attempt_guard.record(completed_attempts);
        if let Some(key) = optional_key.as_ref() {
            if outcome.is_some() && accepted_by_consumer {
                note_optional_compression_success(key);
            } else if physical_attempt.load(Ordering::Acquire)
                && !optional_failure_recorded.swap(true, Ordering::AcqRel)
            {
                // A completed transport response rejected by the task or the
                // consumer is one failed opportunity, not a second request.
                note_optional_compression_failure(key);
            }
        }
        if post_review_rejected {
            note_success(lever);
            note_rejection(lever);
            crate::jev::record_item(
                lever,
                "defer",
                &format!("task `{task_id}` answer failed the bounded Jev post-review"),
                None,
                None,
            );
            return None;
        }
        if outcome.is_some() && !accepted_by_consumer {
            note_success(lever);
            note_rejection(lever);
            crate::jev::record_item(
                lever,
                "defer",
                &format!(
                    "task `{task_id}` answer failed the consumer acceptance contract"
                ),
                None,
                None,
            );
            return None;
        }
        match &outcome {
            Some(outcome) => {
                note_success(lever);
                crate::jev::record_item(
                    lever,
                    "used",
                    &format!(
                        "task `{task_id}` on {} · answered {} chars",
                        self.slug,
                        outcome.text.len()
                    ),
                    None,
                    Some(&distill_workspace::jev::types::JevAnswerSet {
                        // The record's model field is what the TUI and the ledger
                        // show, so the cheap model has to be on it.
                        model: outcome.answer.model.clone(),
                        answers: std::collections::BTreeMap::new(),
                        usage: outcome.answer.usage,
                        request_id: outcome.answer.request_id.clone(),
                        latency_ms: outcome.answer.latency_ms,
                    }),
                );
            }
            None => {
                note_failure(lever);
                crate::jev::record_item(
                    lever,
                    "defer",
                    &format!("task `{task_id}` refused or failed; keeping today's bytes"),
                    None,
                    None,
                );
            }
        }
        outcome
    }
}

/// The configured `[jev.tiers].light` worker. Unlike [`CheapLane`], this keeps
/// the resolved sampler backend, endpoint, auth and effort intact; it is not a
/// Chat Completions wrapper around the local utility model.
pub struct WorkerLane {
    client: distill_sampler::SamplingClient,
    model: String,
    context_window: u64,
    max_output_tokens: u32,
    temperature: Option<f32>,
    top_p: Option<f32>,
    reasoning_effort: Option<ReasoningEffort>,
    idle_timeout: Duration,
}

impl WorkerLane {
    /// Build one worker from the catalog-resolved sampler config. Only the
    /// explicit light-tier effort may override the resolved model default.
    pub fn from_sampler_config(
        mut cfg: distill_sampler::SamplerConfig,
        configured_effort: Option<&str>,
    ) -> Option<Self> {
        let model = cfg.model.trim().to_owned();
        if model.is_empty() || cfg.base_url.trim().is_empty() || cfg.context_window == 0 {
            return None;
        }
        if let Some(raw_effort) = configured_effort
            .map(str::trim)
            .filter(|effort| !effort.is_empty() && !effort.eq_ignore_ascii_case("auto"))
        {
            let effort = raw_effort.parse::<ReasoningEffort>().ok()?;
            cfg.reasoning_effort = Some(effort);
        }
        let reasoning_effort = cfg.reasoning_effort;
        let max_output_tokens = cfg
            .max_completion_tokens
            .unwrap_or(distill_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS)
            .min(distill_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS)
            .max(1);
        let idle_timeout = Duration::from_secs(cfg.idle_timeout_secs.unwrap_or(300).max(1));
        let temperature = cfg.temperature;
        let top_p = cfg.top_p;
        let context_window = cfg.context_window;
        let client = distill_sampler::SamplingClient::new(cfg).ok()?;
        Some(Self {
            client,
            model,
            context_window,
            max_output_tokens,
            temperature,
            top_p,
            reasoning_effort,
            idle_timeout,
        })
    }

    pub fn client(&self) -> &distill_sampler::SamplingClient {
        &self.client
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Conservative payload budget derived from this worker's own context
    /// window, including its task prompt and the reserved answer budget.
    pub fn max_payload_bytes(&self) -> usize {
        let input_budget = self
            .context_window
            .saturating_sub(u64::from(self.max_output_tokens) + WORKER_OVERHEAD_TOKENS)
            as usize;
        input_budget
            .saturating_sub(distill_workspace::jev::cheap::TASK_SYSTEM_PROMPT.len())
            .saturating_sub(WORKER_FRAMING_BYTES)
    }

    /// Build one tool-free, extractive task request. The request is rejected
    /// before transport when the rendered task does not fit this worker's own
    /// context plus its reserved output.
    pub fn task_request(
        &self,
        task_id: &str,
        payload: &str,
        question: &str,
    ) -> Option<ConversationRequest> {
        let task_spec = tasks::spec(task_id)?;
        if !matches!(task_spec.guard, tasks::Guard::Spans) {
            return None;
        }
        let task = tasks::task_for(task_spec, payload, question);
        let rendered = task.render();
        let input_budget = self
            .context_window
            .saturating_sub(u64::from(self.max_output_tokens) + WORKER_OVERHEAD_TOKENS)
            as usize;
        let rendered_budget = input_budget
            .saturating_sub(distill_workspace::jev::cheap::TASK_SYSTEM_PROMPT.len())
            .saturating_sub(WORKER_FRAMING_BYTES);
        if rendered.len() > rendered_budget {
            return None;
        }
        let request_id = format!("jev-tool-result-{}", uuid::Uuid::new_v4());
        Some(ConversationRequest {
            items: vec![
                ConversationItem::system(distill_workspace::jev::cheap::TASK_SYSTEM_PROMPT),
                ConversationItem::user(rendered),
            ],
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: None,
            model: Some(self.model.clone()),
            temperature: self.temperature,
            max_output_tokens: Some(self.max_output_tokens),
            top_p: self.top_p,
            x_grok_conv_id: Some(request_id.clone()),
            x_grok_req_id: Some(request_id),
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_transient_retry: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
            traceparent: None,
            reasoning_effort: self.reasoning_effort,
            json_schema: None,
            prompt_cache_key: None,
            length_policy: LengthPolicy::Fail,
        })
    }

    pub async fn collect(
        &self,
        request: ConversationRequest,
    ) -> (
        distill_sampling_types::Result<distill_sampling_types::ConversationResponse>,
        Option<distill_sampling_types::ConversationResponse>,
    ) {
        self.client
            .conversation_collect_with_idle_timeout_and_rejection(request, self.idle_timeout)
            .await
    }
}

/// One lane's turn-scoped health.
#[derive(Debug, Default, Clone, Copy)]
struct LaneHealth {
    failures: u32,
    calls: u32,
}

fn health() -> &'static Mutex<std::collections::HashMap<&'static str, LaneHealth>> {
    static HEALTH: OnceLock<Mutex<std::collections::HashMap<&'static str, LaneHealth>>> =
        OnceLock::new();
    HEALTH.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Counts one failure for a lane (and one call).
pub fn note_failure(lever: JevLever) {
    if let Ok(mut map) = health().lock() {
        let entry = map.entry(lever.as_str()).or_default();
        entry.failures = entry.failures.saturating_add(1);
        entry.calls = entry.calls.saturating_add(1);
    }
}

/// Counts a post-transport guard rejection without pretending a second
/// generation occurred. The call count is owned by the transport outcome;
/// this only marks its answer unusable.
pub fn note_rejection(lever: JevLever) {
    if let Ok(mut map) = health().lock() {
        let entry = map.entry(lever.as_str()).or_default();
        entry.failures = entry.failures.saturating_add(1);
    }
}

/// Counts one successful call for a lane.
pub fn note_success(lever: JevLever) {
    if let Ok(mut map) = health().lock() {
        let entry = map.entry(lever.as_str()).or_default();
        entry.calls = entry.calls.saturating_add(1);
    }
}

/// Calls and failures per lane, for the turn report and for tests.
pub fn lane_calls(lever: JevLever) -> (u32, u32) {
    health()
        .lock()
        .ok()
        .and_then(|map| map.get(lever.as_str()).copied())
        .map_or((0, 0), |entry| (entry.calls, entry.failures))
}

/// Every lane's `(calls, failures)` right now, biggest first — the turn report's
/// cheap-lane table.
pub fn lane_summary() -> Vec<(&'static str, u32, u32)> {
    let Ok(map) = health().lock() else {
        return Vec::new();
    };
    let mut rows: Vec<(&'static str, u32, u32)> = map
        .iter()
        .map(|(lane, entry)| (*lane, entry.calls, entry.failures))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    rows
}

/// Starts a new turn: a breaker that never reset would silence a lane forever.
pub fn reset_turn() {
    if let Ok(mut map) = health().lock() {
        map.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_sampling_types::{ApiBackend, ReasoningEffort};
    use distill_workspace::jev::{
        cheap::DEFAULT_MAX_COMPLETION_TOKENS,
        flags::JevLever,
        types::Question,
    };

    /// Counting is per lane; a lane that failed all turn is still called on the
    /// next micro-action, so failures never silence a lane.
    #[test]
    #[serial_test::serial]
    fn failures_are_counted_per_lane_and_never_silence_one() {
        reset_turn();
        for _ in 0..10 {
            note_failure(JevLever::ECheapTask);
        }
        let (calls, failures) = lane_calls(JevLever::ECheapTask);
        assert_eq!((calls, failures), (10, 10));
        // Another lane is untouched: counting is per lane, not global.
        assert_eq!(lane_calls(JevLever::ECheapCompress), (0, 0));

        note_success(JevLever::ECheapCompress);
        assert_eq!(lane_calls(JevLever::ECheapCompress), (1, 0));
        note_rejection(JevLever::ECheapCompress);
        assert_eq!(
            lane_calls(JevLever::ECheapCompress),
            (1, 1),
            "answer rejection records failure without a second call"
        );

        reset_turn();
        assert_eq!(lane_calls(JevLever::ECheapTask), (0, 0));
        assert!(lane_summary().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn optional_compression_budget_recovers_by_round_and_isolated_config() {
        crate::jev::with_session_scope("e3-compression-budget", async {
            let key = optional_compression_key(
                "http://utility.test/chat/completions",
                "utility-model",
                "cite_spans",
                "none",
            );
            assert!(optional_compression_allowed(&key));
            note_optional_compression_failure(&key);
            note_optional_compression_failure(&key);
            assert!(!optional_compression_allowed(&key));

            crate::jev::begin_model_round();
            assert!(!optional_compression_allowed(&key));
            crate::jev::begin_model_round();
            assert!(optional_compression_allowed(&key));

            note_optional_compression_failure(&key);
            note_optional_compression_success(&key);
            assert!(optional_compression_allowed(&key));

            let changed_model = optional_compression_key(
                "http://utility.test/chat/completions",
                "utility-model-v2",
                "cite_spans",
                "none",
            );
            assert!(optional_compression_allowed(&changed_model));
        })
        .await;

        crate::jev::with_session_scope("e3-compression-budget-new-session", async {
            let key = optional_compression_key(
                "http://utility.test/chat/completions",
                "utility-model",
                "cite_spans",
                "none",
            );
            assert!(optional_compression_allowed(&key));
        })
        .await;
    }

    /// The shipped default is the owner's chain, in order: the free tier first,
    /// then the paid variants, then the older cheap model.
    #[test]
    fn the_shipped_cheap_spec_is_the_owner_chain_in_order() {
        assert_eq!(
            default_model_spec(),
            "inclusionai/ling-3.0-flash-vl:free,inclusionai/ling-3.0-flash-vl,qwen/qwen3.7-flash"
        );
    }

    /// A spec that is not a catalog entry still builds a lane, on OpenRouter's
    /// own transport and key env — that is what makes a bare slug list work.
    #[test]
    #[serial_test::serial]
    fn a_standalone_spec_builds_a_lane_on_the_openrouter_defaults() {
        let _key = distill_test_support::env::EnvGuard::set("OPENROUTER_API_KEY", "sk-test");
        let lane = CheapLane::from_spec("inclusionai/ling-3.0-flash-vl:free,qwen/qwen3.7-flash")
            .expect("a slug list with a key present builds a lane");
        assert_eq!(
            lane.slug,
            "inclusionai/ling-3.0-flash-vl:free,qwen/qwen3.7-flash"
        );
        assert_eq!(
            lane.client.config().endpoint(),
            "https://openrouter.ai/api/v1/chat/completions",
            "the shipped transport, not the session's"
        );
        assert!(lane.client.credential_present());

        // The same config backs the lane that routes a whole round: same
        // endpoint, same key, so a chain can take a round without a catalog
        // entry — and never as a model id for the session's own provider.
        let round = CheapLane::standalone_sampler_config("inclusionai/ling-3.0-flash-vl:free")
            .expect("the round config exists while the key does");
        assert_eq!(round.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(round.model, "inclusionai/ling-3.0-flash-vl:free");
        assert_eq!(round.api_key.as_deref(), Some("sk-test"));

        let _no_key = distill_test_support::env::EnvGuard::unset("OPENROUTER_API_KEY");
        assert!(
            CheapLane::from_spec("qwen/qwen3.7-flash").is_none(),
            "no key, no lane: the caller keeps today's bytes"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_lane_needs_a_real_model_entry_to_be_built() {
        crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
            effort: Some("none".to_owned()),
            ..Default::default()
        });

        // No key ⇒ no lane: the caller keeps today's bytes rather than sending a
        // request the endpoint would refuse.
        let mut cfg = distill_sampler::SamplerConfig {
            api_key: None,
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            model: "qwen/qwen3.7-flash".to_owned(),
            max_completion_tokens: Some(131_072),
            reasoning_shape: crate::sampling::types::ReasoningShape::Disabled,
            ..Default::default()
        };
        assert!(CheapLane::from_sampler_config(&cfg).is_none());

        cfg.api_key = Some("sk-test".to_owned());
        let lane = CheapLane::from_sampler_config(&cfg).expect("a resolved entry builds a lane");
        assert_eq!(lane.slug, "qwen/qwen3.7-flash");
        assert_eq!(
            lane.client.config().endpoint(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert!(lane.client.credential_present());
        assert_eq!(
            lane.client.config().max_completion_tokens,
            DEFAULT_MAX_COMPLETION_TOKENS,
            "the utility wire cap must override an oversized catalog ceiling"
        );
        // The lane asks for no thinking: it is a closed task, not a chat.
        assert_eq!(
            lane.client.config().reasoning_shape,
            distill_workspace::jev::provider::ReasoningShape::Disabled
        );
        let disabled_body = distill_workspace::jev::provider::chat_message_body(
            &lane.client.config().model,
            "system",
            "user",
            lane.client.config().reasoning_shape,
            &lane.client.config().reasoning_effort,
            lane.client.config().max_completion_tokens,
        );
        assert_eq!(disabled_body["reasoning"]["enabled"], false);
        assert_eq!(disabled_body["max_tokens"], DEFAULT_MAX_COMPLETION_TOKENS);
        assert_eq!(
            distill_workspace::jev::provider::transmitted_reasoning_effort(
                distill_workspace::jev::provider::JevProvider::OpenRouter,
                lane.client.config().reasoning_shape,
                &lane.client.config().reasoning_effort,
                lane.client.config().max_completion_tokens,
            ),
            Some("disabled".to_owned())
        );

        crate::jev::set_test_local_config(crate::agent::config::JevLocalConfig {
            effort: Some("high".to_owned()),
            ..Default::default()
        });
        cfg.reasoning_shape = crate::sampling::types::ReasoningShape::None;
        let effort_lane = CheapLane::from_sampler_config(&cfg)
            .expect("explicit effort entry keeps the caller's no-shape setting");
        assert_eq!(
            effort_lane.client.config().reasoning_shape,
            distill_workspace::jev::provider::ReasoningShape::Effort
        );
        let effort_body = distill_workspace::jev::provider::chat_message_body(
            &effort_lane.client.config().model,
            "system",
            "user",
            effort_lane.client.config().reasoning_shape,
            &effort_lane.client.config().reasoning_effort,
            effort_lane.client.config().max_completion_tokens,
        );
        assert_eq!(effort_body["reasoning"]["effort"], "high");
        assert_eq!(
            distill_workspace::jev::provider::transmitted_reasoning_effort(
                distill_workspace::jev::provider::JevProvider::OpenRouter,
                effort_lane.client.config().reasoning_shape,
                &effort_lane.client.config().reasoning_effort,
                effort_lane.client.config().max_completion_tokens,
            ),
            Some("effort:high".to_owned())
        );

        crate::jev::clear_test_local_config();

        cfg.base_url = "  ".to_owned();
        assert!(CheapLane::from_sampler_config(&cfg).is_none());
    }

    #[test]
    fn utility_review_names_the_candidate_and_separates_gate_phases() {
        let pre_state = utility_review_state(
            UTILITY_PRE_APPROVAL,
            tasks::DISPLAY_FRAGMENT_TASK,
            "extract one source line",
            "source line",
            "utility-model-a,utility-model-b",
            DEFAULT_MAX_COMPLETION_TOKENS,
            None,
        )
        .expect("bounded pre-review state");
        assert_eq!(
            pre_state["utility_model_candidate"],
            "utility-model-a,utility-model-b"
        );
        assert_eq!(
            pre_state["candidate_capabilities"]["max_completion_tokens"],
            DEFAULT_MAX_COMPLETION_TOKENS
        );

        let pre = utility_review_questions(UTILITY_PRE_APPROVAL);
        let Question::Choice { criteria, .. } = &pre[UTILITY_DECISION_ID] else {
            panic!("pre-review must use a choice question");
        };
        assert!(criteria.contains_key("allow"));
        assert!(criteria.contains_key("reject"));
        assert!(!criteria.contains_key("accept"));

        let post_state = utility_review_state(
            UTILITY_POST_REVIEW,
            tasks::DISPLAY_FRAGMENT_TASK,
            "extract one source line",
            "source line",
            "utility-model-a,utility-model-b",
            DEFAULT_MAX_COMPLETION_TOKENS,
            Some("source line"),
        )
        .expect("bounded post-review state");
        assert_eq!(post_state["candidate"], "source line");
        let post = utility_review_questions(UTILITY_POST_REVIEW);
        let Question::Choice { criteria, .. } = &post[UTILITY_DECISION_ID] else {
            panic!("post-review must use a choice question");
        };
        assert!(criteria.contains_key("accept"));
        assert!(criteria.contains_key("reject"));
        assert!(!criteria.contains_key("allow"));
    }

    #[test]
    fn a_lane_defers_unsupported_backend_or_auth_before_transport() {
        assert!(
            CheapLane::from_sampler_config(&distill_sampler::SamplerConfig {
                api_key: Some("sk-test".to_owned()),
                base_url: "https://utility.example/v1".to_owned(),
                model: "pinned-model".to_owned(),
                ..Default::default()
            })
            .is_some(),
            "compatible Chat Completions + bearer config should build a lane"
        );

        for backend in [ApiBackend::Responses, ApiBackend::Messages] {
            assert!(
                CheapLane::from_sampler_config(&distill_sampler::SamplerConfig {
                    api_key: Some("sk-test".to_owned()),
                    base_url: "https://utility.example/v1".to_owned(),
                    model: "pinned-model".to_owned(),
                    api_backend: backend,
                    ..Default::default()
                })
                .is_none(),
                "unsupported backend must defer before the Chat Completions utility transport"
            );
        }

        assert!(
            CheapLane::from_sampler_config(&distill_sampler::SamplerConfig {
                api_key: Some("sk-test".to_owned()),
                base_url: "https://utility.example/v1".to_owned(),
                model: "pinned-model".to_owned(),
                auth_scheme: distill_sampler::AuthScheme::XApiKey,
                ..Default::default()
            })
            .is_none(),
            "unsupported auth must defer before the bearer-only utility transport"
        );
    }

    #[test]
    fn configured_worker_keeps_backend_effort_and_own_context_budget() {
        let lane = WorkerLane::from_sampler_config(
            distill_sampler::SamplerConfig {
                api_key: Some("test-key".to_owned()),
                base_url: "https://worker.example/v1".to_owned(),
                model: "catalog-light".to_owned(),
                context_window: 16_384,
                max_completion_tokens: Some(8_192),
                api_backend: ApiBackend::Responses,
                ..Default::default()
            },
            Some("high"),
        )
        .expect("resolved worker config builds");

        assert_eq!(lane.client.api_backend(), ApiBackend::Responses);
        assert_eq!(
            lane.client.attribution_endpoint(),
            "https://worker.example/v1/responses"
        );
        assert_eq!(lane.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(lane.max_output_tokens, 1_024);
        let request = lane
            .task_request("cite_spans", "error: failed at src/lib.rs:7", "status")
            .expect("extractive task fits");
        assert_eq!(request.model.as_deref(), Some("catalog-light"));
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(request.length_policy, LengthPolicy::Fail);
        assert!(lane.max_payload_bytes() < 16_384);

        let dense = "界".repeat(lane.max_payload_bytes().saturating_div(3) + 1);
        assert!(
            lane.task_request("cite_spans", &dense, "status").is_none(),
            "UTF-8 bytes must not be admitted using a bytes/4 estimate"
        );

        let narrow = WorkerLane::from_sampler_config(
            distill_sampler::SamplerConfig {
                api_key: Some("test-key".to_owned()),
                base_url: "https://worker.example/v1".to_owned(),
                model: "catalog-light".to_owned(),
                context_window: 256,
                ..Default::default()
            },
            None,
        )
        .expect("narrow worker config still builds");
        assert!(narrow.task_request("cite_spans", "error", "status").is_none());

        assert!(
            WorkerLane::from_sampler_config(
                distill_sampler::SamplerConfig {
                    api_key: Some("test-key".to_owned()),
                    base_url: "https://worker.example/v1".to_owned(),
                    model: "catalog-light".to_owned(),
                    context_window: 16_384,
                    ..Default::default()
                },
                Some("not-a-real-effort"),
            )
            .is_none(),
            "an invalid configured effort must defer rather than disappear into the default"
        );
    }
}
