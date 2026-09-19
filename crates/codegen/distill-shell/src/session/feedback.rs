//! Session signals are checked against tiered criteria, and probability sampling keeps requests rare enough not to overwhelm users.

use serde::{Deserialize, Serialize};

use super::signals::SessionSignals;
use crate::util::probabilistic_sample;

pub use prod_mc_cli_chat_proxy_types::feedback_types::{
    FeedbackHeuristicsConfig, FeedbackMode, TierConfig,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTier {
    /// Sustained engagement without issues (0.05% sample rate)
    Tier1,
    /// Complex session with some friction (0.02% sample rate)
    Tier2,
    /// Recovery from issues or session end (0.01% sample rate)
    Tier3,
}

impl FeedbackTier {
    pub fn trigger_type(&self) -> &'static str {
        match self {
            FeedbackTier::Tier1 => "tier1_engagement",
            FeedbackTier::Tier2 => "tier2_complex_recovery",
            FeedbackTier::Tier3 => "tier3_friction_recovery",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerCondition {
    pub tier: FeedbackTier,
    /// Specific condition that was met (e.g., "turns >= 10 AND tool_calls >= 5 AND compactions >= 2 AND cancellations == 0")
    pub condition: String,
    pub signal_snapshot: TriggerSignalSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerSignalSnapshot {
    pub turn_count: u32,
    pub tool_calls_count: u32,
    pub compactions_count: u32,
    pub errors_count: u32,
    pub cancellations_count: u32,
    pub has_reverted: bool,
}

impl TriggerCondition {
    pub fn tier1(signals: &SessionSignals) -> Self {
        Self {
            tier: FeedbackTier::Tier1,
            condition:
                "turns >= 10 AND tool_calls >= 5 AND compactions >= 2 AND cancellations == 0"
                    .to_string(),
            signal_snapshot: TriggerSignalSnapshot::from_signals(signals),
        }
    }

    pub fn tier2(signals: &SessionSignals) -> Self {
        Self {
            tier: FeedbackTier::Tier2,
            condition: "turns >= 15 AND tool_calls >= 10 AND compactions >= 3 AND errors >= 1"
                .to_string(),
            signal_snapshot: TriggerSignalSnapshot::from_signals(signals),
        }
    }

    pub fn tier3(signals: &SessionSignals, had_cancellation: bool, had_revert: bool) -> Self {
        let recovery_condition = if had_cancellation && had_revert {
            "(cancellations > 0 OR has_reverted)"
        } else if had_cancellation {
            "cancellations > 0"
        } else {
            "has_reverted"
        };

        Self {
            tier: FeedbackTier::Tier3,
            condition: format!("turns >= 20 AND {}", recovery_condition),
            signal_snapshot: TriggerSignalSnapshot::from_signals(signals),
        }
    }

    pub fn trigger_reason(&self) -> String {
        let snapshot = &self.signal_snapshot;
        match self.tier {
            FeedbackTier::Tier1 => format!(
                "Tier 1: Sustained engagement (turns={}, tools={}, compactions={}, no cancellations)",
                snapshot.turn_count, snapshot.tool_calls_count, snapshot.compactions_count
            ),
            FeedbackTier::Tier2 => format!(
                "Tier 2: Complex session with errors (turns={}, tools={}, compactions={}, errors={})",
                snapshot.turn_count,
                snapshot.tool_calls_count,
                snapshot.compactions_count,
                snapshot.errors_count
            ),
            FeedbackTier::Tier3 => format!(
                "Tier 3: Recovery from friction (turns={}, cancellations={}, reverted={})",
                snapshot.turn_count, snapshot.cancellations_count, snapshot.has_reverted
            ),
        }
    }
}

impl TriggerSignalSnapshot {
    pub(crate) fn from_signals(signals: &SessionSignals) -> Self {
        Self {
            turn_count: signals.turn_count,
            tool_calls_count: signals.tool_call_count,
            compactions_count: signals.compaction_count,
            errors_count: signals.error_count,
            cancellations_count: signals.cancellation_count,
            has_reverted: signals.has_reverted,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedbackEvaluation {
    /// The trigger condition if criteria were met
    pub trigger_condition: Option<TriggerCondition>,
    /// Whether feedback should actually be requested (after sampling)
    pub should_request: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct FeedbackHeuristics {
    /// Whether feedback collection is globally enabled
    enabled: bool,

    cooldown_seconds: u64,
    max_requests_per_session: u32,

    /// Tier 1 configuration
    tier1_enabled: bool,
    tier1_sample_rate: f64,
    tier1_min_turns: u32,
    tier1_min_tool_calls: u32,
    tier1_min_compactions: u32,
    tier1_no_cancellations: bool,
    tier1_feedback_mode: FeedbackMode,
    tier1_dismissible: bool,
    tier1_prompt: String,
    tier1_max_triggers: u32,

    /// Tier 2 configuration
    tier2_enabled: bool,
    tier2_sample_rate: f64,
    tier2_min_turns: u32,
    tier2_min_tool_calls: u32,
    tier2_min_compactions: u32,
    tier2_min_errors: u32,
    tier2_feedback_mode: FeedbackMode,
    tier2_dismissible: bool,
    tier2_prompt: String,
    tier2_max_triggers: u32,

    /// Tier 3 configuration
    tier3_enabled: bool,
    tier3_sample_rate: f64,
    tier3_min_turns: u32,
    tier3_requires_cancellation: bool,
    tier3_requires_revert: bool,
    tier3_requires_recovery: bool,
    tier3_feedback_mode: FeedbackMode,
    tier3_dismissible: bool,
    tier3_prompt: String,
    tier3_max_triggers: u32,

    /// A tier can trigger up to its configured max_triggers times (0 means unlimited).
    trigger_counts: std::collections::HashMap<FeedbackTier, u32>,

    requests_sent: u32,
    /// Time of the last feedback request (for cooldown tracking)
    last_request_time: Option<std::time::Instant>,
    /// Wall-clock timestamp of the last feedback request (for BQ ingestion)
    last_request_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for FeedbackHeuristics {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedbackHeuristics {
    pub fn new() -> Self {
        Self {
            enabled: true,

            // Global limits
            cooldown_seconds: 300, // 5 minutes
            max_requests_per_session: 3,

            // Tier 1: Standard engagement
            tier1_enabled: true,
            tier1_sample_rate: 0.0005, // 0.05%
            tier1_min_turns: 10,
            tier1_min_tool_calls: 5,
            tier1_min_compactions: 2,
            tier1_no_cancellations: true,
            tier1_feedback_mode: FeedbackMode::Thumbs,
            tier1_dismissible: true,
            tier1_prompt:
                "You've been using Grok Code productively! Would you mind sharing quick feedback?"
                    .to_string(),

            // Tier 2: Complex session with friction
            tier2_enabled: true,
            tier2_sample_rate: 0.0002, // 0.02%
            tier2_min_turns: 15,
            tier2_min_tool_calls: 10,
            tier2_min_compactions: 3,
            tier2_min_errors: 1,
            tier2_feedback_mode: FeedbackMode::ThumbsText,
            tier2_dismissible: true,
            tier2_prompt:
                "You've worked through a complex session. Your feedback would help us improve."
                    .to_string(),

            // Tier 3: Recovery or significant milestone
            tier3_enabled: true,
            tier3_sample_rate: 0.0001, // 0.01%
            tier3_min_turns: 20,
            tier3_requires_cancellation: false,
            tier3_requires_revert: false,
            tier3_requires_recovery: true, // requires cancellation OR revert
            tier3_feedback_mode: FeedbackMode::StarsText,
            tier3_dismissible: true,
            tier3_prompt:
                "Thanks for sticking with us through that session. Got a moment to share feedback?"
                    .to_string(),

            tier1_max_triggers: 1,
            tier2_max_triggers: 1,
            tier3_max_triggers: 1,
            trigger_counts: std::collections::HashMap::new(),
            requests_sent: 0,
            last_request_time: None,
            last_request_at: None,
        }
    }

    /// Preserves the triggered_tiers state and request tracking.
    pub fn update_config(&mut self, config: &FeedbackHeuristicsConfig) {
        use prod_mc_cli_chat_proxy_types::feedback_types::parse_feedback_mode_str;

        self.enabled = config.enabled;

        // Global limits
        self.cooldown_seconds = config.cooldown_seconds as u64;
        self.max_requests_per_session = config.max_requests_per_session as u32;

        // Tier 1
        self.tier1_enabled = config.tier1_enabled;
        self.tier1_sample_rate = config.tier1_sample_rate;
        self.tier1_min_turns = config.tier1_min_turns as u32;
        self.tier1_min_tool_calls = config.tier1_min_tool_calls as u32;
        self.tier1_min_compactions = config.tier1_min_compactions as u32;
        self.tier1_no_cancellations = config.tier1_no_cancellations;
        self.tier1_feedback_mode = parse_feedback_mode_str(&config.tier1_feedback_mode);
        self.tier1_dismissible = config.tier1_dismissible;
        self.tier1_prompt = config.tier1_prompt.clone();
        self.tier1_max_triggers = config.tier1_max_triggers as u32;

        // Tier 2
        self.tier2_enabled = config.tier2_enabled;
        self.tier2_sample_rate = config.tier2_sample_rate;
        self.tier2_min_turns = config.tier2_min_turns as u32;
        self.tier2_min_tool_calls = config.tier2_min_tool_calls as u32;
        self.tier2_min_compactions = config.tier2_min_compactions as u32;
        self.tier2_min_errors = config.tier2_min_errors as u32;
        self.tier2_feedback_mode = parse_feedback_mode_str(&config.tier2_feedback_mode);
        self.tier2_dismissible = config.tier2_dismissible;
        self.tier2_prompt = config.tier2_prompt.clone();
        self.tier2_max_triggers = config.tier2_max_triggers as u32;

        // Tier 3
        self.tier3_enabled = config.tier3_enabled;
        self.tier3_sample_rate = config.tier3_sample_rate;
        self.tier3_min_turns = config.tier3_min_turns as u32;
        self.tier3_requires_cancellation = config.tier3_requires_cancellation;
        self.tier3_requires_revert = config.tier3_requires_revert;
        self.tier3_requires_recovery = config.tier3_requires_recovery;
        self.tier3_feedback_mode = parse_feedback_mode_str(&config.tier3_feedback_mode);
        self.tier3_dismissible = config.tier3_dismissible;
        self.tier3_prompt = config.tier3_prompt.clone();
        self.tier3_max_triggers = config.tier3_max_triggers as u32;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn sample_rate(&self, tier: FeedbackTier) -> f64 {
        match tier {
            FeedbackTier::Tier1 => self.tier1_sample_rate,
            FeedbackTier::Tier2 => self.tier2_sample_rate,
            FeedbackTier::Tier3 => self.tier3_sample_rate,
        }
    }

    pub(crate) fn feedback_mode(&self, tier: FeedbackTier) -> FeedbackMode {
        match tier {
            FeedbackTier::Tier1 => self.tier1_feedback_mode,
            FeedbackTier::Tier2 => self.tier2_feedback_mode,
            FeedbackTier::Tier3 => self.tier3_feedback_mode,
        }
    }

    pub fn dismissible(&self, tier: FeedbackTier) -> bool {
        match tier {
            FeedbackTier::Tier1 => self.tier1_dismissible,
            FeedbackTier::Tier2 => self.tier2_dismissible,
            FeedbackTier::Tier3 => self.tier3_dismissible,
        }
    }

    pub fn prompt(&self, tier: FeedbackTier) -> &str {
        match tier {
            FeedbackTier::Tier1 => &self.tier1_prompt,
            FeedbackTier::Tier2 => &self.tier2_prompt,
            FeedbackTier::Tier3 => &self.tier3_prompt,
        }
    }

    /// Uses probabilistic sampling based on tier rates.
    pub fn evaluate(&mut self, signals: &SessionSignals) -> FeedbackEvaluation {
        if !self.enabled {
            return FeedbackEvaluation {
                trigger_condition: None,
                should_request: false,
                reason: "Feedback collection is disabled".to_string(),
            };
        }

        if self.requests_sent >= self.max_requests_per_session {
            return FeedbackEvaluation {
                trigger_condition: None,
                should_request: false,
                reason: format!(
                    "Max feedback requests reached ({}/{})",
                    self.requests_sent, self.max_requests_per_session
                ),
            };
        }

        if let Some(last_time) = self.last_request_time {
            let elapsed = last_time.elapsed();
            let cooldown = std::time::Duration::from_secs(self.cooldown_seconds);
            if elapsed < cooldown {
                let remaining = cooldown - elapsed;
                return FeedbackEvaluation {
                    trigger_condition: None,
                    should_request: false,
                    reason: format!(
                        "In cooldown period ({:.0}s remaining)",
                        remaining.as_secs_f64()
                    ),
                };
            }
        }

        // Check tiers in order of priority (Tier 3 is most specific, check first)
        if let Some(condition) = self.check_tier3(signals) {
            return self.maybe_request(condition);
        }

        if let Some(condition) = self.check_tier2(signals) {
            return self.maybe_request(condition);
        }

        if let Some(condition) = self.check_tier1(signals) {
            return self.maybe_request(condition);
        }

        FeedbackEvaluation {
            trigger_condition: None,
            should_request: false,
            reason: "No feedback tier criteria met".to_string(),
        }
    }

    /// Tier criteria without the sampling step.
    #[cfg(test)]
    fn check_tier(&self, tier: FeedbackTier, signals: &SessionSignals) -> bool {
        match tier {
            FeedbackTier::Tier1 => self.check_tier1(signals).is_some(),
            FeedbackTier::Tier2 => self.check_tier2(signals).is_some(),
            FeedbackTier::Tier3 => self.check_tier3(signals).is_some(),
        }
    }

    fn tier_exhausted(&self, tier: FeedbackTier) -> bool {
        let max = match tier {
            FeedbackTier::Tier1 => self.tier1_max_triggers,
            FeedbackTier::Tier2 => self.tier2_max_triggers,
            FeedbackTier::Tier3 => self.tier3_max_triggers,
        };
        max > 0 && self.trigger_counts.get(&tier).copied().unwrap_or(0) >= max
    }

    fn check_tier1(&self, signals: &SessionSignals) -> Option<TriggerCondition> {
        if !self.tier1_enabled {
            return None;
        }

        if self.tier_exhausted(FeedbackTier::Tier1) {
            return None;
        }

        // Tier 1: Sustained engagement without major issues
        let cancellation_check = if self.tier1_no_cancellations {
            signals.cancellation_count == 0
        } else {
            true
        };

        if signals.turn_count >= self.tier1_min_turns
            && signals.tool_call_count >= self.tier1_min_tool_calls
            && signals.compaction_count >= self.tier1_min_compactions
            && cancellation_check
        {
            return Some(TriggerCondition::tier1(signals));
        }

        None
    }

    fn check_tier2(&self, signals: &SessionSignals) -> Option<TriggerCondition> {
        if !self.tier2_enabled {
            return None;
        }

        if self.tier_exhausted(FeedbackTier::Tier2) {
            return None;
        }

        // Tier 2: Complex session with some friction but recovery
        if signals.turn_count >= self.tier2_min_turns
            && signals.tool_call_count >= self.tier2_min_tool_calls
            && signals.compaction_count >= self.tier2_min_compactions
            && signals.error_count >= self.tier2_min_errors
        {
            return Some(TriggerCondition::tier2(signals));
        }

        None
    }

    fn check_tier3(&self, signals: &SessionSignals) -> Option<TriggerCondition> {
        if !self.tier3_enabled {
            return None;
        }

        if self.tier_exhausted(FeedbackTier::Tier3) {
            return None;
        }

        // Tier 3: Recovery from significant issues
        let had_cancellation = signals.cancellation_count > 0;
        let had_revert = signals.has_reverted;

        let has_recovery_signal = if self.tier3_requires_recovery {
            had_cancellation || had_revert
        } else {
            // Check specific requirements
            let cancellation_ok = !self.tier3_requires_cancellation || had_cancellation;
            let revert_ok = !self.tier3_requires_revert || had_revert;
            cancellation_ok && revert_ok
        };

        if signals.turn_count >= self.tier3_min_turns && has_recovery_signal {
            return Some(TriggerCondition::tier3(
                signals,
                had_cancellation,
                had_revert,
            ));
        }

        None
    }

    fn maybe_request(&mut self, condition: TriggerCondition) -> FeedbackEvaluation {
        let tier = condition.tier;
        let should_sample = probabilistic_sample(self.sample_rate(tier));

        if should_sample {
            *self.trigger_counts.entry(tier).or_insert(0) += 1;
            // Track request for cooldown and max requests limits
            self.requests_sent += 1;
            self.last_request_time = Some(std::time::Instant::now());
            self.last_request_at = Some(chrono::Utc::now());
        }

        let reason = condition.trigger_reason();
        FeedbackEvaluation {
            trigger_condition: Some(condition),
            should_request: should_sample,
            reason: format!(
                "{}. {}",
                reason,
                if should_sample {
                    "Sampled for feedback."
                } else {
                    "Not sampled."
                }
            ),
        }
    }

    /// Number of feedback requests sent this session.
    pub fn requests_sent(&self) -> u32 {
        self.requests_sent
    }

    pub(crate) fn last_request_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.last_request_at
    }
}

/// A feedback request to be sent to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackRequest {
    pub request_id: String,
    pub session_id: String,
    pub tier: FeedbackTier,
    pub feedback_mode: FeedbackMode,
    pub stars: bool,
    pub thumbs: bool,
    pub text: bool,
    pub prompt: String,
    pub dismissible: bool,
    /// Trigger type identifier (e.g., "tier1_engagement", "tier2_complex_recovery")
    pub trigger_type: String,
    pub trigger_condition: TriggerCondition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

impl FeedbackRequest {
    pub fn with_mode(
        session_id: String,
        trigger_condition: TriggerCondition,
        feedback_mode: FeedbackMode,
        dismissible: bool,
        prompt_override: Option<&str>,
    ) -> Self {
        let request_id = uuid::Uuid::now_v7().to_string();
        let tier = trigger_condition.tier;
        let prompt = match prompt_override {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => match tier {
                FeedbackTier::Tier1 => {
                    "You've been using Grok Code productively! Would you mind sharing quick feedback?".to_string()
                }
                FeedbackTier::Tier2 => {
                    "You've worked through a complex session. Your feedback would help us improve.".to_string()
                }
                FeedbackTier::Tier3 => {
                    "Thanks for sticking with us through that session. Got a moment to share feedback?".to_string()
                }
            },
        };

        let (stars, thumbs, text) = match feedback_mode {
            FeedbackMode::Stars => (true, false, false),
            FeedbackMode::Thumbs => (false, true, false),
            FeedbackMode::Text => (false, false, true),
            FeedbackMode::StarsText => (true, false, true),
            FeedbackMode::ThumbsText => (false, true, true),
            _ => (false, true, false),
        };

        Self {
            request_id,
            session_id,
            tier,
            feedback_mode,
            prompt,
            dismissible,
            trigger_type: tier.trigger_type().to_string(),
            trigger_condition,
            context: None,
            stars,
            thumbs,
            text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_signals(
        turns: u32,
        tools: u32,
        compactions: u32,
        errors: u32,
        cancellations: u32,
    ) -> SessionSignals {
        SessionSignals {
            turn_count: turns,
            tool_call_count: tools,
            compaction_count: compactions,
            error_count: errors,
            cancellation_count: cancellations,
            context_window_usage: 50,
            tools_used: vec!["read_file".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn test_tier1_criteria() {
        let heuristics = FeedbackHeuristics::new();

        // Below threshold
        let signals = make_signals(5, 3, 1, 0, 0);
        assert!(!heuristics.check_tier(FeedbackTier::Tier1, &signals));

        // Meets threshold, no cancellations
        let signals = make_signals(10, 5, 2, 0, 0);
        assert!(heuristics.check_tier(FeedbackTier::Tier1, &signals));

        // Has cancellations
        let signals = make_signals(10, 5, 2, 0, 1);
        assert!(!heuristics.check_tier(FeedbackTier::Tier1, &signals));
    }

    #[test]
    fn test_tier2_criteria() {
        let heuristics = FeedbackHeuristics::new();

        // Below threshold
        let signals = make_signals(10, 5, 2, 0, 0);
        assert!(!heuristics.check_tier(FeedbackTier::Tier2, &signals));

        // Meets threshold with the required error
        let signals = make_signals(15, 10, 3, 1, 0);
        assert!(heuristics.check_tier(FeedbackTier::Tier2, &signals));

        // No errors
        let signals = make_signals(15, 10, 3, 0, 0);
        assert!(!heuristics.check_tier(FeedbackTier::Tier2, &signals));
    }

    #[test]
    fn test_tier3_criteria() {
        let heuristics = FeedbackHeuristics::new();

        // Below the turn threshold
        let signals = make_signals(15, 10, 3, 1, 1);
        assert!(!heuristics.check_tier(FeedbackTier::Tier3, &signals));

        // Meets threshold with cancellation
        let signals = make_signals(20, 10, 3, 1, 1);
        assert!(heuristics.check_tier(FeedbackTier::Tier3, &signals));

        // Meets threshold with revert
        let mut signals = make_signals(20, 10, 3, 1, 0);
        signals.has_reverted = true;
        assert!(heuristics.check_tier(FeedbackTier::Tier3, &signals));

        // No recovery signal
        let signals = make_signals(20, 10, 3, 1, 0);
        assert!(!heuristics.check_tier(FeedbackTier::Tier3, &signals));
    }

    #[test]
    fn test_tier_deduplication() {
        let mut heuristics = FeedbackHeuristics::new();

        let signals = make_signals(10, 5, 2, 0, 0);

        // First evaluation finds Tier 1
        let eval = heuristics.evaluate(&signals);
        assert!(eval.trigger_condition.is_some());
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier1
        );

        // Simulate that the request was sent
        *heuristics
            .trigger_counts
            .entry(FeedbackTier::Tier1)
            .or_insert(0) += 1;

        // Second evaluation with the same signals does not trigger Tier 1 again
        let eval = heuristics.evaluate(&signals);
        assert!(
            eval.trigger_condition.is_none()
                || eval.trigger_condition.as_ref().unwrap().tier != FeedbackTier::Tier1
        );
    }

    #[test]
    fn test_feedback_request_creation() {
        let signals = make_signals(10, 5, 2, 0, 0);
        let condition = TriggerCondition::tier1(&signals);
        let request = FeedbackRequest::with_mode(
            "session-123".to_string(),
            condition,
            FeedbackMode::Thumbs,
            true,
            None,
        );

        assert!(!request.request_id.is_empty());
        assert_eq!(request.session_id, "session-123");
        assert_eq!(request.tier, FeedbackTier::Tier1);
        assert!(request.dismissible);
        assert_eq!(request.trigger_type, "tier1_engagement");
        assert_eq!(
            request.trigger_condition.condition,
            "turns >= 10 AND tool_calls >= 5 AND compactions >= 2 AND cancellations == 0"
        );
        assert!(
            request
                .trigger_condition
                .trigger_reason()
                .contains("turns=10")
        );
        assert!(
            request
                .trigger_condition
                .trigger_reason()
                .contains("tools=5")
        );

        // Test Tier2
        let signals2 = make_signals(15, 10, 3, 1, 0);
        let condition2 = TriggerCondition::tier2(&signals2);
        let request2 = FeedbackRequest::with_mode(
            "session-456".to_string(),
            condition2,
            FeedbackMode::Thumbs,
            true,
            None,
        );
        assert_eq!(request2.trigger_type, "tier2_complex_recovery");
        assert_eq!(
            request2.trigger_condition.condition,
            "turns >= 15 AND tool_calls >= 10 AND compactions >= 3 AND errors >= 1"
        );

        // Test Tier3
        let mut signals3 = make_signals(20, 10, 3, 1, 2);
        signals3.has_reverted = true;
        let condition3 = TriggerCondition::tier3(&signals3, true, true);
        let request3 = FeedbackRequest::with_mode(
            "session-789".to_string(),
            condition3,
            FeedbackMode::Thumbs,
            true,
            None,
        );
        assert_eq!(request3.trigger_type, "tier3_friction_recovery");
        assert!(request3.trigger_condition.condition.contains("turns >= 20"));
    }

    #[test]
    fn test_feedback_request_non_dismissible() {
        let signals = make_signals(15, 10, 3, 1, 0);
        let condition = TriggerCondition::tier2(&signals);
        let request = FeedbackRequest::with_mode(
            "session-mandatory".to_string(),
            condition,
            FeedbackMode::StarsText,
            false,
            None,
        );
        assert!(!request.dismissible);
        assert_eq!(request.tier, FeedbackTier::Tier2);
    }

    #[test]
    fn test_heuristics_dismissible_per_tier() {
        let mut heuristics = FeedbackHeuristics::new();

        // Defaults are all true
        assert!(heuristics.dismissible(FeedbackTier::Tier1));
        assert!(heuristics.dismissible(FeedbackTier::Tier2));
        assert!(heuristics.dismissible(FeedbackTier::Tier3));

        // Override from config
        let config = FeedbackHeuristicsConfig {
            tier2_dismissible: false,
            tier3_dismissible: false,
            ..FeedbackHeuristicsConfig::default()
        };
        heuristics.update_config(&config);

        assert!(heuristics.dismissible(FeedbackTier::Tier1));
        assert!(!heuristics.dismissible(FeedbackTier::Tier2));
        assert!(!heuristics.dismissible(FeedbackTier::Tier3));
    }

    #[test]
    fn test_tier_repeatable_when_max_triggers_zero() {
        let mut h = FeedbackHeuristics::new();
        h.tier1_max_triggers = 0; // unlimited
        h.tier1_sample_rate = 1.0;
        h.cooldown_seconds = 0;
        h.max_requests_per_session = 100;
        h.tier1_min_turns = 1;
        h.tier1_min_tool_calls = 0;
        h.tier1_min_compactions = 0;
        h.tier1_no_cancellations = false;
        h.tier2_enabled = false;
        h.tier3_enabled = false;

        let signals = make_signals(5, 3, 1, 0, 0);

        // With max_triggers=0 (unlimited), the tier fires on every evaluation
        for i in 0..5 {
            let eval = h.evaluate(&signals);
            assert!(
                eval.should_request,
                "iteration {i}: unlimited tier should keep triggering"
            );
        }
        assert_eq!(
            h.trigger_counts
                .get(&FeedbackTier::Tier1)
                .copied()
                .unwrap_or(0),
            5
        );
    }

    #[test]
    fn test_tier_limited_to_max_triggers() {
        let mut h = FeedbackHeuristics::new();
        h.tier1_max_triggers = 3;
        h.tier1_sample_rate = 1.0;
        h.cooldown_seconds = 0;
        h.max_requests_per_session = 100;
        h.tier1_min_turns = 1;
        h.tier1_min_tool_calls = 0;
        h.tier1_min_compactions = 0;
        h.tier1_no_cancellations = false;
        h.tier2_enabled = false;
        h.tier3_enabled = false;

        let signals = make_signals(5, 3, 1, 0, 0);

        // The tier fires exactly 3 times
        for i in 0..3 {
            let eval = h.evaluate(&signals);
            assert!(eval.should_request, "iteration {i}: should trigger");
        }
        let eval = h.evaluate(&signals);
        assert!(!eval.should_request, "4th trigger should be blocked");
    }

    #[test]
    fn test_default_max_triggers_preserves_dedup() {
        let mut h = FeedbackHeuristics::new();
        h.tier1_sample_rate = 1.0;
        h.cooldown_seconds = 0;
        h.max_requests_per_session = 100;
        h.tier1_min_turns = 1;
        h.tier1_min_tool_calls = 0;
        h.tier1_min_compactions = 0;
        h.tier1_no_cancellations = false;
        h.tier2_enabled = false;
        h.tier3_enabled = false;
        // max_triggers defaults to 1

        let signals = make_signals(5, 3, 1, 0, 0);

        let eval = h.evaluate(&signals);
        assert!(eval.should_request, "first trigger should fire");

        let eval = h.evaluate(&signals);
        assert!(
            !eval.should_request,
            "second trigger should be blocked (max_triggers=1)"
        );
    }

    #[test]
    fn test_mixed_tier_max_triggers() {
        let mut h = FeedbackHeuristics::new();
        h.tier1_max_triggers = 0; // unlimited
        h.tier2_max_triggers = 2;
        // tier3_max_triggers stays at 1 (default)
        h.tier1_sample_rate = 1.0;
        h.tier2_sample_rate = 1.0;
        h.tier3_sample_rate = 1.0;
        h.cooldown_seconds = 0;
        h.max_requests_per_session = 100;
        // Set thresholds so all tiers can fire
        h.tier1_min_turns = 1;
        h.tier1_min_tool_calls = 0;
        h.tier1_min_compactions = 0;
        h.tier1_no_cancellations = false;
        h.tier2_min_turns = 1;
        h.tier2_min_tool_calls = 0;
        h.tier2_min_compactions = 0;
        h.tier2_min_errors = 0;
        h.tier3_min_turns = 1;
        h.tier3_requires_recovery = false;
        h.tier3_requires_cancellation = false;
        h.tier3_requires_revert = false;

        let signals = make_signals(10, 5, 2, 1, 0);

        // Tier 3 exhausts after 1 trigger (evaluate checks tier 3 first, then 2, then 1)
        let eval = h.evaluate(&signals);
        assert!(eval.should_request);
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier3
        );

        // Tier 2 fires next (tier3 exhausted)
        let eval = h.evaluate(&signals);
        assert!(eval.should_request);
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier2
        );

        // Tier 2 fires again (max_triggers=2)
        let eval = h.evaluate(&signals);
        assert!(eval.should_request);
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier2
        );

        // Tier 2 is exhausted, so evaluation falls through to tier 1 (unlimited)
        let eval = h.evaluate(&signals);
        assert!(eval.should_request);
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier1
        );

        // Tier 1 keeps firing
        let eval = h.evaluate(&signals);
        assert!(eval.should_request);
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            FeedbackTier::Tier1
        );
    }
}
