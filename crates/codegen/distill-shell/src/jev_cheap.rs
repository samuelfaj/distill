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

use distill_workspace::jev::cheap::{DEFAULT_BASE_URL, DEFAULT_MODELS};
use distill_workspace::jev::flags::JevLever;
use distill_workspace::jev::tasks;

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
        if !crate::jev::lever_active(lever) {
            return None;
        }
        // Serialised: one cheap generation at a time across the whole process.
        let _one_at_a_time = lane_queue().lock().await;
        let (session_id, turn_id, round_id) = crate::jev::telemetry_context();
        let span = tracing::info_span!(target: "jev.decision", "utility_context",
            session_id, turn_id, round_id, utility_call_id = %uuid::Uuid::new_v4());
        let outcome = tracing::Instrument::instrument(
            tasks::run(&self.client, task_id, payload, question),
            span,
        )
        .await;
        match &outcome {
            Some(outcome) => {
                note_success(lever);
                crate::jev::record_cheap_answer_usage(&outcome.answer);
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
}
