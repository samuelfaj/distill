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
    use xai_grok_workspace::jev::policy::{DecisionRecord, DecisionSink};
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JevTurnActivity {
    /// Decisions recorded inside the window (the current turn, when known).
    pub decisions: u32,
    /// True when one of them was a refusal: the brake stopping a call.
    pub refused: bool,
    /// Calls in flight **right now** — Jev is being consulted for this step.
    pub in_flight: u32,
    /// Latency of the most recent decision in the window (0 without one).
    pub last_latency_ms: u64,
}

impl JevTurnActivity {
    /// Nothing to show: no decision in the window and nothing in flight.
    pub const fn is_quiet(&self) -> bool {
        self.decisions == 0 && self.in_flight == 0
    }

    /// The chip text, e.g. `jev…`, `jev·veto`, `jev ×3`. `None` when quiet.
    ///
    /// Kept short by construction: it shares one row with the running tool and
    /// the turn timer.
    pub fn label(&self) -> Option<String> {
        if self.in_flight > 0 && self.decisions == 0 {
            return Some("jev…".to_owned());
        }
        if self.refused {
            return Some("jev·veto".to_owned());
        }
        match self.decisions {
            0 => (self.in_flight > 0).then(|| "jev…".to_owned()),
            1 => Some(format!("jev {:.1}s", self.last_latency_ms as f64 / 1000.0)),
            n => Some(format!("jev ×{n}")),
        }
    }
}

/// Most recent decisions kept for the row; the row only ever looks back one turn.
const ACTIVITY_RING: usize = 64;

/// Decisions whose label means "the call was refused".
const REFUSALS: &[&str] = &["block", "veto", "deny", "refuse", "refused"];

#[derive(Debug, Default)]
struct ActivityState {
    ring: std::collections::VecDeque<JevActivity>,
    in_flight: u32,
}

fn activity_state() -> &'static std::sync::Mutex<ActivityState> {
    static STATE: OnceLock<std::sync::Mutex<ActivityState>> = OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(ActivityState::default()))
}

/// Remembers one decision for the turn-status row. Never fails the caller.
pub fn note_decision(lever: &str, decision: &str, latency_ms: u64) {
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    if state.ring.len() >= ACTIVITY_RING {
        state.ring.pop_front();
    }
    state.ring.push_back(JevActivity {
        lever: lever.to_owned(),
        decision: decision.to_owned(),
        latency_ms,
        at: std::time::Instant::now(),
    });
}

/// Marks a Jev call as started (`+1`) or finished (`-1`) for the row's `jev…` state.
pub fn note_in_flight(started: bool) {
    let Ok(mut state) = activity_state().lock() else {
        return;
    };
    state.in_flight = if started {
        state.in_flight.saturating_add(1)
    } else {
        state.in_flight.saturating_sub(1)
    };
}

/// The row's view of Jev: decisions recorded at or after `since`.
///
/// `None` (no turn anchor — a wake turn, or a row rendered outside a turn)
/// counts whatever is still in the ring, which is at most the last
/// [`ACTIVITY_RING`] decisions of this process.
pub fn turn_activity(since: Option<std::time::Instant>) -> JevTurnActivity {
    let Ok(state) = activity_state().lock() else {
        return JevTurnActivity::default();
    };
    let mut activity = JevTurnActivity {
        in_flight: state.in_flight,
        ..Default::default()
    };
    for entry in state.ring.iter().rev() {
        if let Some(since) = since
            && entry.at < since
        {
            continue;
        }
        activity.decisions = activity.decisions.saturating_add(1);
        if REFUSALS.contains(&entry.decision.as_str()) {
            activity.refused = true;
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
        state.ring.clear();
        state.in_flight = 0;
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

impl xai_grok_workspace::jev::policy::DecisionSink for ActivitySink {
    fn record(&self, record: &xai_grok_workspace::jev::policy::DecisionRecord) {
        xai_grok_workspace::jev::policy::TracingSink.record(record);
        note_decision(&record.lever, &record.decision, record.latency_ms);
    }
}

/// Wraps a Jev asker so a consultation shows as `jev…` while it runs.
pub struct ObservedAsker {
    inner: std::sync::Arc<dyn xai_grok_workspace::jev::permission::JevAsker>,
}

impl ObservedAsker {
    pub fn new(inner: std::sync::Arc<dyn xai_grok_workspace::jev::permission::JevAsker>) -> Self {
        Self { inner }
    }
}

impl xai_grok_workspace::jev::permission::JevAsker for ObservedAsker {
    fn ask<'a>(
        &'a self,
        state: &'a serde_json::Value,
        questions: &'a std::collections::BTreeMap<
            xai_grok_workspace::jev::types::QuestionId,
            xai_grok_workspace::jev::types::Question,
        >,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        xai_grok_workspace::jev::types::JevAnswerSet,
                        xai_grok_workspace::jev::error::JevError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let in_flight = JevInFlight::begin();
        Box::pin(async move {
            let result = self.inner.ask(state, questions).await;
            drop(in_flight);
            result
        })
    }
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

    #[serial_test::serial]
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

    /// The turn-status row reads only what happened: a decision inside the
    /// window is counted (refusals flagged), anything older is not, and a call
    /// in flight shows as such.
    #[serial_test::serial]
    #[test]
    fn turn_activity_counts_the_turn_window_and_refusals() {
        reset_activity_for_test();
        let before = std::time::Instant::now();
        note_decision("p5_call_validation", "ask", 410);
        note_decision("yolo_veto", "block", 380);
        // A window that opens *after* the two decisions, offset past any clock
        // granularity, so "was it in this turn?" cannot depend on timer detail.
        let after = before + std::time::Duration::from_millis(50);

        let turn = turn_activity(Some(before));
        assert_eq!(turn.decisions, 2);
        assert!(turn.refused, "a refusal must be visible on the row");
        assert_eq!(
            turn.last_latency_ms, 380,
            "the newest latency is the one shown"
        );
        assert!(!turn.is_quiet());

        // A window that opens after the decisions saw none of them.
        assert!(turn_activity(Some(after)).is_quiet());

        // An unknown window (no turn anchor) still reports the ring.
        assert_eq!(turn_activity(None).decisions, 2);

        let guard = JevInFlight::begin();
        let in_flight = turn_activity(Some(after));
        assert_eq!(in_flight.in_flight, 1);
        assert_eq!(in_flight.label().as_deref(), Some("jev…"));
        drop(guard);
        assert_eq!(turn_activity(Some(after)).in_flight, 0);
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
            }
            .label()
            .as_deref(),
            Some("jev·veto")
        );
    }

    /// The observer must keep the in-flight count honest across the future's
    /// whole lifetime, not just the call that builds it.
    #[serial_test::serial]
    #[tokio::test]
    async fn the_observed_asker_marks_flight_for_the_whole_call() {
        use std::sync::Arc;
        use xai_grok_workspace::jev::permission::JevAsker;

        reset_activity_for_test();
        /// An asker that reports when it starts and waits to be released, so
        /// the counter can be read mid-call without a race.
        struct Gated {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
        }
        impl JevAsker for Gated {
            fn ask<'a>(
                &'a self,
                _state: &'a serde_json::Value,
                _questions: &'a std::collections::BTreeMap<
                    xai_grok_workspace::jev::types::QuestionId,
                    xai_grok_workspace::jev::types::Question,
                >,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                xai_grok_workspace::jev::types::JevAnswerSet,
                                xai_grok_workspace::jev::error::JevError,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                let entered = Arc::clone(&self.entered);
                let release = Arc::clone(&self.release);
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Err(xai_grok_workspace::jev::error::JevError::invalid("gated"))
                })
            }
        }

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let observed = ObservedAsker::new(Arc::new(Gated {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }));
        let state = serde_json::json!({});
        let questions = std::collections::BTreeMap::new();
        let ask = observed.ask(&state, &questions);
        assert_eq!(
            turn_activity(None).in_flight,
            1,
            "the counter opens with the call"
        );
        // Driver: waits until the inner future has actually started, reads the
        // counter there, then releases it.
        let driver = async {
            entered.notified().await;
            let mid = turn_activity(None).in_flight;
            release.notify_one();
            mid
        };
        let (mid, _) = tokio::join!(driver, ask);
        assert_eq!(mid, 1, "in flight while the inner future runs");
        assert_eq!(
            turn_activity(None).in_flight,
            0,
            "the guard is dropped with the call"
        );
        reset_activity_for_test();
    }
}
