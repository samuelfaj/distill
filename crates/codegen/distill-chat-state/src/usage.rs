// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-prompt and per-session billing ledgers (not serialized).
//!
//! `total_tokens()` is input + output: Responses wire `total` is live context
//! length. Compaction and other side calls use `record_auxiliary_call` instead
//! of `record_main_loop_call`.
//!
//! # Completeness ownership
//!
//! Wire incomplete is the OR of these stores (each has a distinct role):
//!
//! - **`UsageLedger.incomplete`** — durable on the bill snapshot. Set by nested
//!   subagent incomplete fold, drain timeout, true apply-miss, and
//!   `mark_usage_incomplete`. Monotonic for a ledger instance.
//! - **Sticky (`subagent_usage_not_applied` on the coordinator)** — pin-scoped
//!   **report** signal (session-only attribution or apply-miss report). Not a
//!   second token sink; does not stain ledgers by itself.
//! - **Foreground live IDs** — fold may still land; freeze drains ≤120s or fails
//!   closed. Cancel skips multi-second drain (actor-loop safety).
//! - **Background live** — never waits; prompt report incomplete immediately;
//!   spend still folds into the session ledger at completion (no session-ledger
//!   incomplete).
//!
//! Freeze and cancel share one outcome policy: ledger marks only on fail-closed;
//! sticky and background_live are report-level only.
//!
//! Projection (`PromptUsage`) never invents tokens; it only ORs completeness
//! and scrubs costs when partial or incomplete.

use distill_sampling_types::TokenUsage;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Outcome of one dispatched provider attempt. Rejected responses still count;
/// they are distinct from a local preflight refusal, which never creates one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCallStatus {
    Completed,
    Rejected,
    Failed,
    Cancelled,
}

/// How the recorded cost was obtained. Unknown is intentionally not rendered
/// as zero and is kept alongside the aggregate cost-missing counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCostBasis {
    Reported,
    Estimated,
    Unknown,
}

/// Identity and provider metadata for one billable attempt.
///
/// `attempt_id` is a local unique identity used for exactly-once folding;
/// `request_id` is the provider identity when the response supplies one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAttribution {
    pub attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub role: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_effort: Option<String>,
    pub status: UsageCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// Both provider token components were present. Known one-sided counts are
    /// still retained in `usage`; this flag keeps the missing side explicit.
    #[serde(default)]
    pub usage_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
    pub cost_basis: UsageCostBasis,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub model_calls: u64,
    pub api_duration_ms: u64,
    /// USD ticks (1e10 per USD). Absent when no call reported cost.
    pub cost_usd_ticks: Option<i64>,
    pub cost_missing_calls: u64,
}

impl UsageTotals {
    fn from_call(
        usage: &TokenUsage,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) -> Self {
        Self::from_optional_call(Some(usage), api_duration_ms, cost_usd_ticks)
    }

    fn from_optional_call(
        usage: Option<&TokenUsage>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) -> Self {
        let usage = usage.cloned().unwrap_or_default();
        let cost_usd_ticks = distill_sampling_types::reported_cost_ticks(cost_usd_ticks);
        Self {
            input_tokens: u64::from(usage.prompt_tokens),
            output_tokens: u64::from(usage.completion_tokens),
            cached_read_tokens: u64::from(usage.cached_prompt_tokens),
            cache_creation_tokens: u64::from(usage.cache_creation_prompt_tokens),
            reasoning_tokens: u64::from(usage.reasoning_tokens),
            model_calls: 1,
            api_duration_ms: api_duration_ms.unwrap_or(0),
            cost_usd_ticks,
            cost_missing_calls: u64::from(cost_usd_ticks.is_none()),
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// True whenever one or more calls lack provider-reported cost.
    /// `cost_usd_ticks == None` distinguishes the all-absent case from a
    /// partially reported total; neither case is treated as free.
    pub fn cost_is_partial(&self) -> bool {
        self.cost_missing_calls > 0
    }

    fn fold_totals(&mut self, other: &UsageTotals) {
        let Self {
            input_tokens,
            output_tokens,
            cached_read_tokens,
            cache_creation_tokens,
            reasoning_tokens,
            model_calls,
            api_duration_ms,
            cost_usd_ticks,
            cost_missing_calls,
        } = other;
        self.input_tokens = self.input_tokens.saturating_add(*input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(*output_tokens);
        self.cached_read_tokens = self.cached_read_tokens.saturating_add(*cached_read_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(*cache_creation_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(*reasoning_tokens);
        self.model_calls = self.model_calls.saturating_add(*model_calls);
        self.api_duration_ms = self.api_duration_ms.saturating_add(*api_duration_ms);
        self.cost_missing_calls = self.cost_missing_calls.saturating_add(*cost_missing_calls);
        self.cost_usd_ticks = merge_cost_ticks(self.cost_usd_ticks, *cost_usd_ticks);
    }
}

fn merge_cost_ticks(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageLedger {
    pub totals: UsageTotals,
    pub by_model: IndexMap<String, UsageTotals>,
    /// One row per dispatched auxiliary/main attempt with provider metadata.
    /// Aggregate totals remain the billable token/cost projection.
    pub attributions: Vec<UsageAttribution>,
    /// Main-agent loop rounds for `num_turns` (subagents excluded).
    pub main_loop_model_calls: u64,
    /// Bill may under-count (drain timeout, nested subagent incomplete, apply failure).
    pub incomplete: bool,
}

impl UsageLedger {
    /// Fold one attributed attempt exactly once. The attempt id is the local
    /// deduplication key; provider request ids are not guaranteed to exist or
    /// to be unique across providers.
    pub fn record_attribution(&mut self, attribution: UsageAttribution) {
        if self
            .attributions
            .iter()
            .any(|existing| existing.attempt_id == attribution.attempt_id)
        {
            return;
        }
        let cost = distill_sampling_types::reported_cost_ticks(attribution.cost_usd_ticks);
        let call = UsageTotals::from_optional_call(
            attribution.usage.as_ref(),
            attribution.api_duration_ms,
            cost,
        );
        if attribution.role == "main" {
            self.main_loop_model_calls = self.main_loop_model_calls.saturating_add(1);
        }
        self.fold_entry(&attribution.model_id, &call);
        if !attribution.usage_complete
            || attribution.usage.is_none()
            || matches!(
                attribution.status,
                UsageCallStatus::Failed | UsageCallStatus::Cancelled
            )
        {
            self.incomplete = true;
        }
        self.attributions.push(attribution);
    }

    /// Fold one main-agent-loop model call. This is the only writer of
    /// `main_loop_model_calls` (the wire `numTurns`); side calls such as
    /// compaction must not use it.
    pub fn record_main_loop_call(
        &mut self,
        model_id: &str,
        usage: &TokenUsage,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) {
        let call = UsageTotals::from_call(usage, api_duration_ms, cost_usd_ticks);
        self.main_loop_model_calls = self.main_loop_model_calls.saturating_add(1);
        self.fold_entry(model_id, &call);
    }

    /// Fold a main-loop call whose response did not report token usage.
    /// The call is counted, but the token and cost fields stay explicitly
    /// absent and the ledger is marked incomplete.
    pub fn record_main_loop_call_without_usage(
        &mut self,
        model_id: &str,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) {
        let call = UsageTotals::from_optional_call(None, api_duration_ms, cost_usd_ticks);
        self.main_loop_model_calls = self.main_loop_model_calls.saturating_add(1);
        self.fold_entry(model_id, &call);
        self.incomplete = true;
    }

    /// Fold one non-main model call without changing the main-loop count.
    /// Missing usage is counted as an incomplete call; missing provider cost is
    /// tracked separately by `cost_missing_calls`.
    pub fn record_auxiliary_call(
        &mut self,
        model_id: &str,
        usage: Option<&TokenUsage>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
        incomplete: bool,
    ) {
        let call = UsageTotals::from_optional_call(usage, api_duration_ms, cost_usd_ticks);
        self.fold_entry(model_id, &call);
        if incomplete || usage.is_none() {
            self.incomplete = true;
        }
    }

    /// Fold subagent usage without incrementing `main_loop_model_calls`.
    pub fn record_subagent(&mut self, by_model: &[(String, UsageTotals)], incomplete: bool) {
        for (model_id, totals) in by_model {
            self.fold_entry(model_id, totals);
        }
        if incomplete {
            self.incomplete = true;
        }
    }

    pub fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    fn fold_entry(&mut self, model_id: &str, totals: &UsageTotals) {
        self.totals.fold_totals(totals);
        self.by_model
            .entry(model_id.to_owned())
            .or_default()
            .fold_totals(totals);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tu(prompt: u32, completion: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: 999_999,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        }
    }

    #[test]
    fn ledger_sums_partial_subagent_and_zero_cost() {
        let mut ledger = UsageLedger::default();
        ledger.record_main_loop_call("m", &tu(1, 1), None, Some(0));
        assert_eq!(ledger.totals.cost_usd_ticks, None);
        assert_eq!(ledger.totals.cost_missing_calls, 1);
        assert!(ledger.totals.cost_is_partial());

        ledger.record_main_loop_call("a", &tu(100, 10), Some(100), None);
        ledger.record_main_loop_call("a", &tu(50, 5), Some(50), Some(70));
        assert_eq!(ledger.totals.cost_usd_ticks, Some(70));
        assert!(ledger.totals.cost_is_partial());
        assert_eq!(ledger.main_loop_model_calls, 3);

        ledger.record_subagent(
            &[(
                "b".into(),
                UsageTotals {
                    input_tokens: 5,
                    model_calls: 1,
                    ..Default::default()
                },
            )],
            false,
        );
        assert_eq!(ledger.by_model.get("b").map(|m| m.input_tokens), Some(5));
        assert_eq!(ledger.main_loop_model_calls, 3);
        assert_eq!(ledger.totals.model_calls, 4);
        assert!(!ledger.incomplete);

        ledger.record_subagent(&[], true);
        assert!(ledger.incomplete);
    }

    #[test]
    fn auxiliary_and_unreported_main_calls_are_counted_once_without_free_cost() {
        let mut ledger = UsageLedger::default();
        ledger.record_auxiliary_call("recap-model", None, Some(12), None, true);
        ledger.record_main_loop_call_without_usage("main-model", Some(8), Some(30));

        assert_eq!(ledger.main_loop_model_calls, 1);
        assert_eq!(ledger.totals.model_calls, 2);
        assert_eq!(ledger.totals.cost_usd_ticks, Some(30));
        assert_eq!(ledger.totals.cost_missing_calls, 1);
        assert!(ledger.totals.cost_is_partial());
        assert!(ledger.incomplete);
        assert_eq!(ledger.by_model["recap-model"].model_calls, 1);
        assert_eq!(ledger.by_model["main-model"].model_calls, 1);
    }

    #[test]
    fn attributed_attempts_are_deduplicated_and_keep_unknown_cost_explicit() {
        let usage = tu(9, 3);
        let attribution = UsageAttribution {
            attempt_id: "attempt-1".to_owned(),
            task_id: Some("utility-task".to_owned()),
            turn_id: Some("turn-1".to_owned()),
            request_id: Some("provider-1".to_owned()),
            role: "utility".to_owned(),
            model_id: "cheap-model".to_owned(),
            endpoint: Some("https://provider.test/chat".to_owned()),
            requested_effort: Some("low".to_owned()),
            applied_effort: None,
            status: UsageCallStatus::Rejected,
            usage: Some(usage),
            usage_complete: true,
            api_duration_ms: Some(12),
            cost_usd_ticks: None,
            cost_basis: UsageCostBasis::Unknown,
        };
        let mut ledger = UsageLedger::default();
        ledger.record_attribution(attribution.clone());
        ledger.record_attribution(attribution);

        assert_eq!(ledger.attributions.len(), 1);
        assert_eq!(ledger.totals.model_calls, 1);
        assert_eq!(ledger.totals.input_tokens, 9);
        assert_eq!(ledger.totals.cost_missing_calls, 1);
        assert!(ledger.totals.cost_is_partial());
        assert!(!ledger.incomplete);
    }

    #[test]
    fn attributed_partial_usage_keeps_known_tokens_and_marks_incomplete() {
        let mut ledger = UsageLedger::default();
        ledger.record_attribution(UsageAttribution {
            attempt_id: "input-only".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: None,
            role: "utility".to_owned(),
            model_id: "jev".to_owned(),
            endpoint: None,
            requested_effort: None,
            applied_effort: None,
            status: UsageCallStatus::Completed,
            usage: Some(tu(11, 0)),
            usage_complete: false,
            api_duration_ms: None,
            cost_usd_ticks: None,
            cost_basis: UsageCostBasis::Unknown,
        });

        assert_eq!(ledger.totals.input_tokens, 11);
        assert_eq!(ledger.totals.output_tokens, 0);
        assert!(ledger.incomplete);
    }
}
