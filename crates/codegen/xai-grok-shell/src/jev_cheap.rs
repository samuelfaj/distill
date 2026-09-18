//! The cheap lanes on the session side: who may call, how often, and what is
//! recorded.
//!
//! The transports and the task contracts live in `xai_grok_workspace::jev`
//! (`cheap`, `tasks`); this module is the policy around them, and it exists so
//! every lane answers the same four questions the same way:
//!
//! * **May it run?** the master switch, the lane's own key and a resolvable
//!   credential, read from one place.
//! * **How is the cheap model reached?** from a resolved model entry (base URL,
//!   slug, key), never from a second copy of the config.
//! * **How many times?** a per-lane counter plus a breaker: after
//!   [`BREAKER_TRIPS`] failures a lane stops being called for the rest of the
//!   turn, which is what keeps a flaky endpoint from costing time on every step.
//! * **What gets recorded?** one content-free line per call — lane, task id,
//!   decision, model, tokens, latency — through the same recorder as every other
//!   decision, so the TUI, the turn report and the log cannot disagree.

use std::sync::{Mutex, OnceLock};

use xai_grok_workspace::jev::tasks;
use xai_grok_workspace::jev::flags::JevLever;

/// Failures in one lane inside one turn before it stands down for that turn.
pub const BREAKER_TRIPS: u32 = 3;

/// A resolved cheap worker: the client plus the model entry it came from.
pub struct CheapLane {
    pub client: xai_grok_workspace::jev::cheap::CheapClient,
    /// The catalog id the entry resolved to, for the record.
    pub slug: String,
}

impl CheapLane {
    /// Builds the lane from a resolved model entry.
    ///
    /// The key is captured once, at resolution time, and handed to the client as
    /// a resolver: the client still reads it at call time, and nothing else in
    /// the process can see it.
    pub fn from_sampler_config(cfg: &xai_grok_sampler::SamplerConfig) -> Option<Self> {
        let api_key = cfg.api_key.clone()?;
        if api_key.trim().is_empty() || cfg.base_url.trim().is_empty() || cfg.model.trim().is_empty()
        {
            return None;
        }
        let config = xai_grok_workspace::jev::cheap::CheapConfig {
            base_url: cfg.base_url.trim_end_matches('/').to_owned(),
            model: cfg.model.clone(),
            max_completion_tokens: cfg
                .max_completion_tokens
                .unwrap_or(xai_grok_workspace::jev::cheap::DEFAULT_MAX_COMPLETION_TOKENS),
            ..xai_grok_workspace::jev::cheap::CheapConfig::default()
        };
        let slug = cfg.model.clone();
        let client = xai_grok_workspace::jev::cheap::CheapClient::with_key_resolver(
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
        if lane_tripped(lever) {
            crate::jev::record_item(
                lever,
                "tripped",
                &format!("lane stood down for this turn after {BREAKER_TRIPS} failures"),
                None,
                None,
            );
            return None;
        }
        if !crate::jev::lever_active(lever) {
            return None;
        }
        let outcome = tasks::run(&self.client, task_id, payload, question).await;
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
                    Some(&xai_grok_workspace::jev::types::JevAnswerSet {
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

/// Whether a lane has failed enough to stand down for the rest of the turn.
pub fn lane_tripped(lever: JevLever) -> bool {
    health()
        .lock()
        .ok()
        .and_then(|map| map.get(lever.as_str()).copied())
        .is_some_and(|entry| entry.failures >= BREAKER_TRIPS)
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
    use xai_grok_workspace::jev::flags::JevLever;

    #[test]
    #[serial_test::serial]
    fn the_breaker_stands_a_lane_down_after_repeated_failures_and_resets() {
        reset_turn();
        assert!(!lane_tripped(JevLever::ECheapTask));
        for _ in 0..BREAKER_TRIPS {
            assert!(!lane_tripped(JevLever::ECheapTask), "before the trip");
            note_failure(JevLever::ECheapTask);
        }
        assert!(lane_tripped(JevLever::ECheapTask), "after {BREAKER_TRIPS} failures");
        // Another lane is untouched: the breaker is per lane, not global.
        assert!(!lane_tripped(JevLever::ECheapCompress));
        let (calls, failures) = lane_calls(JevLever::ECheapTask);
        assert_eq!((calls, failures), (BREAKER_TRIPS, BREAKER_TRIPS));

        note_success(JevLever::ECheapCompress);
        assert_eq!(lane_calls(JevLever::ECheapCompress), (1, 0));

        reset_turn();
        assert!(!lane_tripped(JevLever::ECheapTask));
        assert_eq!(lane_calls(JevLever::ECheapTask), (0, 0));
        assert!(lane_summary().is_empty());
    }

    #[test]
    fn a_lane_needs_a_real_model_entry_to_be_built() {
        // No key ⇒ no lane: the caller keeps today's bytes rather than sending a
        // request the endpoint would refuse.
        let mut cfg = xai_grok_sampler::SamplerConfig {
            api_key: None,
            base_url: "https://openrouter.ai/api/v1".to_owned(),
            model: "qwen/qwen3.7-flash".to_owned(),
            ..Default::default()
        };
        assert!(CheapLane::from_sampler_config(&cfg).is_none());

        cfg.api_key = Some("sk-test".to_owned());
        let lane = CheapLane::from_sampler_config(&cfg).expect("a resolved entry builds a lane");
        assert_eq!(lane.slug, "qwen/qwen3.7-flash");
        assert_eq!(lane.client.config().endpoint(), "https://openrouter.ai/api/v1/chat/completions");
        assert!(lane.client.credential_present());
        // The lane asks for no thinking: it is a closed task, not a chat.
        assert_eq!(
            lane.client.config().reasoning_shape,
            xai_grok_workspace::jev::provider::ReasoningShape::Disabled
        );

        cfg.base_url = "  ".to_owned();
        assert!(CheapLane::from_sampler_config(&cfg).is_none());
    }
}
