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
//! * **How many times?** no cap: a lane keeps being called for every
//!   micro-action that wants it. A per-lane counter records how often it ran and
//!   how often it failed, so a flaky endpoint is visible without ever silencing
//!   the lane.
//! * **What gets recorded?** one content-free line per call — lane, task id,
//!   decision, model, tokens, latency — through the same recorder as every other
//!   decision, so the TUI, the turn report and the log cannot disagree.

use std::sync::{Mutex, OnceLock};
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
                    crate::sampling::types::ReasoningShape::MaxTokens => {
                        distill_workspace::jev::provider::ReasoningShape::MaxTokens
                    }
                    _ => distill_workspace::jev::provider::ReasoningShape::Effort,
                }
            } else {
                distill_workspace::jev::provider::ReasoningShape::Disabled
            },
            max_completion_tokens: cfg
                .max_completion_tokens
                .unwrap_or(distill_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS),
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
        self.run_task_with_acceptance(lever, task_id, payload, question, |_| true)
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
        accepts: F,
    ) -> Option<tasks::TaskOutcome>
    where
        F: Fn(&str) -> bool,
    {
        if !crate::jev::lever_active(lever) {
            return None;
        }
        // Serialised: one cheap generation at a time across the whole process.
        let _one_at_a_time = lane_queue().lock().await;
        let (session_id, turn_id, round_id) = crate::jev::telemetry_context();
        let span = tracing::info_span!(target: "jev.decision", "utility_context",
            session_id, turn_id, round_id, utility_call_id = %uuid::Uuid::new_v4());
        let recorder = crate::jev::active_usage_recorder();
        let completed_attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_client = recorder.as_ref().map(|recorder| {
            let completed_attempts = completed_attempts.clone();
            let recorder = recorder.clone();
            let task_id = task_id.to_owned();
            let turn_id = turn_id.clone();
            let observer: distill_workspace::jev::types::AttemptObserver =
                std::sync::Arc::new(move |attempt| {
                    if matches!(
                        attempt.status,
                        distill_workspace::jev::types::AttemptStatus::Completed
                    ) {
                        completed_attempts
                            .lock()
                            .expect("utility attempt lock")
                            .push(attempt);
                    } else {
                        // Failed, rejected, and cancelled attempts are already
                        // final at the transport boundary. Record each one
                        // immediately so fallback chains and cancellation do
                        // not disappear behind the consumer gate.
                        crate::jev::record_workspace_attempt(
                            attempt,
                            "utility",
                            Some(task_id.clone()),
                            Some(turn_id.clone()),
                            recorder.clone(),
                            true,
                        );
                    }
                });
            self.client.with_call_observer(observer)
        });
        let request_client = observed_client.as_ref().unwrap_or(&self.client);
        let outcome = tracing::Instrument::instrument(
            tasks::run(request_client, task_id, payload, question),
            span,
        )
        .await;
        let accepted_by_consumer = outcome
            .as_ref()
            .is_some_and(|outcome| accepts(&outcome.text));
        let mut completed_attempts = completed_attempts
            .lock()
            .expect("utility attempt lock")
            .drain(..)
            .collect::<Vec<_>>();
        if (outcome.is_none() || !accepted_by_consumer)
            && let Some(attempt) = completed_attempts.last_mut()
        {
            // `tasks::run` applies its own source-span guard after the cheap
            // transport has returned. Keep that one physical response
            // rejected in the existing attempt row; never emit a second row.
            attempt.status = distill_workspace::jev::types::AttemptStatus::Rejected;
        }
        if let Some(recorder) = recorder {
            for attempt in completed_attempts {
                crate::jev::record_workspace_attempt(
                    attempt,
                    "utility",
                    Some(task_id.to_owned()),
                    Some(turn_id.clone()),
                    recorder.clone(),
                    true,
                );
            }
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
    use distill_workspace::jev::flags::JevLever;

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
    fn a_lane_needs_a_real_model_entry_to_be_built() {
        // No key ⇒ no lane: the caller keeps today's bytes rather than sending a
        // request the endpoint would refuse.
        let mut cfg = distill_sampler::SamplerConfig {
            api_key: None,
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            model: "qwen/qwen3.7-flash".to_owned(),
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
        // The lane asks for no thinking: it is a closed task, not a chat.
        assert_eq!(
            lane.client.config().reasoning_shape,
            distill_workspace::jev::provider::ReasoningShape::Disabled
        );

        cfg.base_url = "  ".to_owned();
        assert!(CheapLane::from_sampler_config(&cfg).is_none());
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
