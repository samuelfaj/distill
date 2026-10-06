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
//! - **Pending attempt IDs** — an admitted call that has not reached a terminal
//!   attribution. The projection is incomplete while any ID is pending, but a
//!   successful terminal row clears that pending state without leaving a false
//!   permanent unknown.
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
use std::collections::{BTreeMap, BTreeSet};

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
    /// Provider-authoritative cost. `Some(0)` is a reported free call, not a
    /// missing legacy tick value.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_out: Option<u64>,
    /// Utility attempts only: what the request was about (`shell`, `recap`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_kind: Option<String>,
    /// Utility attempts only: `used`, `nothing`, `consumer_rejected`,
    /// `review_rejected`, `rejected`, `failed` or `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_decision: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// The one-hour part of `cache_creation_tokens`; the rest is five-minute.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cache_creation_1h_tokens: u64,
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
        Self::from_optional_call_raw(
            usage,
            api_duration_ms,
            distill_sampling_types::reported_cost_ticks(cost_usd_ticks),
        )
    }

    fn from_optional_call_raw(
        usage: Option<&TokenUsage>,
        api_duration_ms: Option<u64>,
        cost_usd_ticks: Option<i64>,
    ) -> Self {
        let usage = usage.cloned().unwrap_or_default();
        Self {
            input_tokens: u64::from(usage.prompt_tokens),
            output_tokens: u64::from(usage.completion_tokens),
            cached_read_tokens: u64::from(usage.cached_prompt_tokens),
            cache_creation_tokens: u64::from(usage.cache_creation_prompt_tokens),
            cache_creation_1h_tokens: u64::from(usage.cache_creation_1h_prompt_tokens),
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
            cache_creation_1h_tokens,
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
        self.cache_creation_1h_tokens = self
            .cache_creation_1h_tokens
            .saturating_add(*cache_creation_1h_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(*reasoning_tokens);
        self.model_calls = self.model_calls.saturating_add(*model_calls);
        self.api_duration_ms = self.api_duration_ms.saturating_add(*api_duration_ms);
        self.cost_missing_calls = self.cost_missing_calls.saturating_add(*cost_missing_calls);
        self.cost_usd_ticks = merge_cost_ticks(self.cost_usd_ticks, *cost_usd_ticks);
    }
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn merge_cost_ticks(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// Content-free utility outcome counters for one source kind. Sizes and labels
/// only, so a secret in a tool result can never reach the ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtilityOutcomeCounts {
    /// Final decision per eligible result (`compress`, `not_shorter`,
    /// `defer:required-dominates`, `keep:lane-unavailable`, …). Keys starting
    /// with `request:` count single utility requests refused before dispatch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub decisions: BTreeMap<String, u64>,
    /// Utility requests the results planned.
    #[serde(default)]
    pub chunks: u64,
    /// Bytes before the decision.
    #[serde(default)]
    pub bytes_in: u64,
    /// Bytes that entered the context after it (the original when kept).
    #[serde(default)]
    pub bytes_out: u64,
}

impl UtilityOutcomeCounts {
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.chunks == 0 && self.bytes_in == 0 && self.bytes_out == 0
    }

    pub fn saturating_add(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (decision, count) in &other.decisions {
            let entry = out.decisions.entry(decision.clone()).or_default();
            *entry = entry.saturating_add(*count);
        }
        out.chunks = out.chunks.saturating_add(other.chunks);
        out.bytes_in = out.bytes_in.saturating_add(other.bytes_in);
        out.bytes_out = out.bytes_out.saturating_add(other.bytes_out);
        out
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (decision, count) in &other.decisions {
            if let Some(entry) = out.decisions.get_mut(decision) {
                *entry = entry.saturating_sub(*count);
            }
        }
        out.decisions.retain(|_, count| *count > 0);
        out.chunks = out.chunks.saturating_sub(other.chunks);
        out.bytes_in = out.bytes_in.saturating_sub(other.bytes_in);
        out.bytes_out = out.bytes_out.saturating_sub(other.bytes_out);
        out
    }

    /// Element-wise maximum: two snapshots of the same monotonic ledger.
    pub fn max(&self, other: &Self) -> Self {
        let mut out = self.clone();
        for (decision, count) in &other.decisions {
            let entry = out.decisions.entry(decision.clone()).or_default();
            *entry = (*entry).max(*count);
        }
        out.chunks = out.chunks.max(other.chunks);
        out.bytes_in = out.bytes_in.max(other.bytes_in);
        out.bytes_out = out.bytes_out.max(other.bytes_out);
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Admitted provider attempts whose terminal attribution has not arrived.
    /// This is part of the canonical ledger lifecycle, not a second accounting store.
    pub pending_attempts: BTreeSet<String>,
    /// Utility outcomes per source kind. Not billing: it never changes totals.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub utility_outcomes: BTreeMap<String, UtilityOutcomeCounts>,
    /// The last main request whose cache read fell far below the previous
    /// prompt. Telemetry, not billing: no total reads it, and subagent folds
    /// leave it alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cache_break: Option<CacheBreak>,
}

/// Where the last prompt-cache break happened: the first item of the request
/// that differed from the previous one, or none when the prefix was intact and
/// the cache entry had expired or been evicted. Hashes and kinds only, never
/// content.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheBreak {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_changed_index: Option<u64>,
    /// Kind of the changed item (`system`, `user`, `tool_result`, …), as sent
    /// before the break, or after it when the history shrank there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed_kind: Option<String>,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Seconds between the previous request and the one that missed.
    #[serde(default)]
    pub secs_since_previous: u64,
    /// The items were intact but the tools or request settings sent with them changed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settings_changed: bool,
    /// The cache lifetime the previous request got, when its endpoint's is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_lifetime_secs: Option<u64>,
}

impl UsageLedger {
    /// Admit one provider attempt before dispatch. The attempt id is also the
    /// exactly-once key used by the terminal attribution row.
    pub fn register_pending_attempt(&mut self, attempt_id: String) {
        if !self
            .attributions
            .iter()
            .any(|existing| existing.attempt_id == attempt_id)
        {
            self.pending_attempts.insert(attempt_id);
        }
    }

    /// Whether this ledger can still be missing the result of an admitted call.
    pub fn is_incomplete(&self) -> bool {
        self.incomplete || !self.pending_attempts.is_empty()
    }

    /// Fold one attributed attempt exactly once. The attempt id is the local
    /// deduplication key; provider request ids are not guaranteed to exist or
    /// to be unique across providers.
    pub fn record_attribution(&mut self, attribution: UsageAttribution) {
        self.record_attribution_inner(attribution, true);
    }

    /// Fold child-attempt metadata without turning nested model calls into
    /// foreground loop turns. The child attempt id remains the exactly-once
    /// key, so a delayed coordinator fold cannot rebill the parent.
    pub fn record_subagent_attributions(
        &mut self,
        attributions: &[UsageAttribution],
        incomplete: bool,
    ) {
        self.record_subagent_attributions_with_pending(attributions, &[], incomplete);
    }

    /// Fold child-attempt metadata and preserve any child calls that were
    /// admitted but had not reached a terminal row at the fold boundary.
    pub fn record_subagent_attributions_with_pending(
        &mut self,
        attributions: &[UsageAttribution],
        pending_attempts: &[String],
        incomplete: bool,
    ) {
        for attempt_id in pending_attempts {
            self.register_pending_attempt(attempt_id.clone());
        }
        for attribution in attributions {
            self.record_attribution_inner(attribution.clone(), false);
        }
        if incomplete {
            self.incomplete = true;
        }
    }

    /// Fold a child snapshot using identity rows when they cover the child
    /// ledger, otherwise retain the historical aggregate-only fold. Callers
    /// must not pass both a partial attribution set and aggregate totals: that
    /// would intentionally be ambiguous rather than silently double-counted.
    pub fn record_subagent_usage(
        &mut self,
        by_model: &[(String, UsageTotals)],
        attributions: &[UsageAttribution],
        incomplete: bool,
    ) {
        self.record_subagent_usage_with_pending(by_model, attributions, &[], incomplete);
    }

    /// Fold a child snapshot while carrying admitted-but-not-terminal attempt
    /// IDs across the parent boundary.
    pub fn record_subagent_usage_with_pending(
        &mut self,
        by_model: &[(String, UsageTotals)],
        attributions: &[UsageAttribution],
        pending_attempts: &[String],
        incomplete: bool,
    ) {
        if attributions.is_empty() {
            self.record_subagent_with_pending(by_model, pending_attempts, incomplete);
        } else {
            self.record_subagent_attributions_with_pending(
                attributions,
                pending_attempts,
                incomplete,
            );
        }
    }

    /// [`Self::record_subagent_usage_with_pending`] that also returns the slice
    /// this ledger newly accepted. Rows already folded (same `attempt_id`)
    /// stay out of the slice, so it can be applied to one more record without
    /// double counting.
    pub fn record_subagent_usage_slice(
        &mut self,
        by_model: &[(String, UsageTotals)],
        attributions: &[UsageAttribution],
        pending_attempts: &[String],
        incomplete: bool,
    ) -> UsageLedger {
        let folded = |attempt_id: &str| {
            self.attributions
                .iter()
                .any(|existing| existing.attempt_id == attempt_id)
        };
        let pending = pending_attempts
            .iter()
            .filter(|attempt_id| !folded(attempt_id))
            .cloned()
            .collect::<Vec<_>>();
        let fresh = attributions
            .iter()
            .filter(|attribution| !folded(&attribution.attempt_id))
            .cloned()
            .collect::<Vec<_>>();
        let mut slice = UsageLedger::default();
        if attributions.is_empty() {
            slice.record_subagent_with_pending(by_model, &pending, incomplete);
        } else {
            slice.record_subagent_attributions_with_pending(&fresh, &pending, incomplete);
        }
        self.record_subagent_usage_with_pending(by_model, attributions, pending_attempts, incomplete);
        slice
    }

    fn record_attribution_inner(&mut self, attribution: UsageAttribution, count_main: bool) {
        self.pending_attempts.remove(&attribution.attempt_id);
        if self
            .attributions
            .iter()
            .any(|existing| existing.attempt_id == attribution.attempt_id)
        {
            return;
        }
        let cost = match attribution.cost_basis {
            UsageCostBasis::Reported => attribution.cost_usd_ticks.filter(|&ticks| ticks >= 0),
            UsageCostBasis::Estimated | UsageCostBasis::Unknown => {
                distill_sampling_types::reported_cost_ticks(attribution.cost_usd_ticks)
            }
        };
        let call = UsageTotals::from_optional_call_raw(
            attribution.usage.as_ref(),
            attribution.api_duration_ms,
            cost,
        );
        if count_main && attribution.role == "main" {
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
        self.record_subagent_with_pending(by_model, &[], incomplete);
    }

    /// Fold aggregate child usage and preserve admitted pending attempt IDs.
    pub fn record_subagent_with_pending(
        &mut self,
        by_model: &[(String, UsageTotals)],
        pending_attempts: &[String],
        incomplete: bool,
    ) {
        for attempt_id in pending_attempts {
            self.register_pending_attempt(attempt_id.clone());
        }
        for (model_id, totals) in by_model {
            self.fold_entry(model_id, totals);
        }
        if incomplete {
            self.incomplete = true;
        }
    }

    /// Count one utility decision for `source_kind` (see [`UtilityOutcomeCounts`]).
    pub fn record_utility_outcome(
        &mut self,
        source_kind: &str,
        decision: &str,
        chunks: u64,
        bytes_in: u64,
        bytes_out: u64,
    ) {
        let row = self.utility_outcomes.entry(source_kind.to_owned()).or_default();
        let count = row.decisions.entry(decision.to_owned()).or_default();
        *count = count.saturating_add(1);
        row.chunks = row.chunks.saturating_add(chunks);
        row.bytes_in = row.bytes_in.saturating_add(bytes_in);
        row.bytes_out = row.bytes_out.saturating_add(bytes_out);
    }

    pub fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    pub fn record_cache_break(&mut self, cache_break: CacheBreak) {
        self.last_cache_break = Some(cache_break);
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
            cache_creation_1h_prompt_tokens: 0,
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
    fn attributed_reported_zero_cost_is_free_not_unknown() {
        let mut ledger = UsageLedger::default();
        ledger.record_attribution(UsageAttribution {
            attempt_id: "reported-free".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: Some("provider-free".to_owned()),
            role: "main".to_owned(),
            model_id: "free-model".to_owned(),
            endpoint: None,
            requested_effort: None,
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
            applied_effort: None,
            status: UsageCallStatus::Completed,
            usage: Some(tu(1, 1)),
            usage_complete: true,
            api_duration_ms: None,
            cost_usd_ticks: Some(0),
            cost_basis: UsageCostBasis::Reported,
        });

        assert_eq!(ledger.totals.cost_usd_ticks, Some(0));
        assert_eq!(ledger.totals.cost_missing_calls, 0);
        assert!(!ledger.totals.cost_is_partial());

        let mut legacy = UsageLedger::default();
        legacy.record_attribution(UsageAttribution {
            attempt_id: "legacy-zero".to_owned(),
            cost_usd_ticks: Some(0),
            cost_basis: UsageCostBasis::Unknown,
            ..ledger.attributions[0].clone()
        });
        assert_eq!(legacy.totals.cost_usd_ticks, None);
        assert_eq!(legacy.totals.cost_missing_calls, 1);
    }

    #[test]
    fn child_attributions_fold_once_without_foreground_turns() {
        let first = UsageAttribution {
            attempt_id: "child-1".to_owned(),
            task_id: Some("child-task".to_owned()),
            turn_id: Some("child-turn".to_owned()),
            request_id: Some("provider-1".to_owned()),
            role: "main".to_owned(),
            model_id: "child-model".to_owned(),
            endpoint: Some("https://provider.test".to_owned()),
            requested_effort: Some("low".to_owned()),
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
            applied_effort: Some("effort:low".to_owned()),
            status: UsageCallStatus::Completed,
            usage: Some(tu(7, 2)),
            usage_complete: true,
            api_duration_ms: Some(4),
            cost_usd_ticks: Some(3),
            cost_basis: UsageCostBasis::Reported,
        };
        let second = UsageAttribution {
            attempt_id: "child-2".to_owned(),
            task_id: Some("child-task".to_owned()),
            turn_id: Some("child-turn".to_owned()),
            request_id: None,
            role: "auxiliary".to_owned(),
            model_id: "child-model".to_owned(),
            endpoint: None,
            requested_effort: None,
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
            applied_effort: Some("absent".to_owned()),
            status: UsageCallStatus::Failed,
            usage: None,
            usage_complete: false,
            api_duration_ms: None,
            cost_usd_ticks: None,
            cost_basis: UsageCostBasis::Unknown,
        };
        let mut ledger = UsageLedger::default();
        ledger.record_subagent_usage(
            &[],
            &[first.clone(), first, second.clone()],
            true,
        );

        assert_eq!(ledger.attributions.len(), 2);
        assert_eq!(ledger.totals.model_calls, 2);
        assert_eq!(ledger.totals.input_tokens, 7);
        assert_eq!(ledger.totals.cost_usd_ticks, Some(3));
        assert_eq!(ledger.main_loop_model_calls, 0);
        assert!(ledger.incomplete);
        assert_eq!(ledger.attributions[1], second);
    }

    #[test]
    fn physical_main_retry_is_billed_without_becoming_an_extra_loop_round() {
        let mut ledger = UsageLedger::default();
        for (attempt_id, role, prompt) in [("main", "main", 10), ("retry", "main_retry", 4)] {
            ledger.record_attribution(UsageAttribution {
                attempt_id: attempt_id.to_owned(),
                task_id: Some("task".to_owned()),
                turn_id: Some("turn".to_owned()),
                request_id: None,
                role: role.to_owned(),
                model_id: "model".to_owned(),
                endpoint: Some("https://provider.test/chat".to_owned()),
                requested_effort: None,
                reason: None,
                bytes_in: None,
                source_kind: None,
                final_decision: None,
                bytes_out: None,
                applied_effort: Some("absent".to_owned()),
                status: if role == "main" {
                    UsageCallStatus::Completed
                } else {
                    UsageCallStatus::Failed
                },
                usage: Some(tu(prompt, 1)),
                usage_complete: true,
                api_duration_ms: Some(1),
                cost_usd_ticks: Some(1),
                cost_basis: UsageCostBasis::Reported,
            });
        }

        assert_eq!(ledger.totals.model_calls, 2);
        assert_eq!(ledger.main_loop_model_calls, 1);
        assert_eq!(ledger.totals.input_tokens, 14);
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
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
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
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
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

    #[test]
    fn pending_attempt_is_incomplete_until_terminal_row_arrives() {
        let mut child = UsageLedger::default();
        child.record_main_loop_call("child-model", &tu(3, 1), Some(5), Some(5));
        child.register_pending_attempt("initial-title:pending".to_owned());
        assert!(!child.incomplete);
        assert!(child.is_incomplete());

        let pending = child.pending_attempts.iter().cloned().collect::<Vec<_>>();
        let mut parent = UsageLedger::default();
        parent.record_subagent_usage_with_pending(
            &[("child-model".to_owned(), child.totals.clone())],
            &[],
            &pending,
            false,
        );
        assert!(parent.is_incomplete());
        assert_eq!(parent.totals.model_calls, 1);
        assert_eq!(parent.totals.cost_usd_ticks, Some(5));

        let title = UsageAttribution {
            attempt_id: "initial-title:pending".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: Some("title-request".to_owned()),
            role: "auxiliary".to_owned(),
            model_id: "title-model".to_owned(),
            endpoint: Some("https://provider.test/chat".to_owned()),
            requested_effort: None,
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
            applied_effort: Some("absent".to_owned()),
            status: UsageCallStatus::Completed,
            usage: Some(tu(4, 2)),
            usage_complete: true,
            api_duration_ms: Some(5),
            cost_usd_ticks: Some(7),
            cost_basis: UsageCostBasis::Reported,
        };
        child.record_attribution(title.clone());

        assert!(child.pending_attempts.is_empty());
        assert!(!child.is_incomplete());
        assert_eq!(child.totals.model_calls, 2);
        assert_eq!(child.totals.cost_usd_ticks, Some(12));

        parent.record_subagent_usage_with_pending(&[], &[title.clone()], &[], false);
        parent.record_subagent_usage_with_pending(&[], &[title], &[], false);
        assert!(!parent.is_incomplete());
        assert_eq!(parent.totals.model_calls, 2);
        assert_eq!(parent.totals.cost_usd_ticks, Some(12));
        assert_eq!(parent.attributions.len(), 1);
    }

    #[test]
    fn subagent_usage_slice_counts_each_attempt_once() {
        let row = UsageAttribution {
            attempt_id: "child-1".to_owned(),
            task_id: None,
            turn_id: None,
            request_id: None,
            role: "main".to_owned(),
            model_id: "child-model".to_owned(),
            endpoint: None,
            requested_effort: None,
            reason: None,
            bytes_in: None,
            source_kind: None,
            final_decision: None,
            bytes_out: None,
            applied_effort: None,
            status: UsageCallStatus::Completed,
            usage: Some(tu(7, 2)),
            usage_complete: true,
            api_duration_ms: None,
            cost_usd_ticks: Some(3),
            cost_basis: UsageCostBasis::Reported,
        };
        let mut session = UsageLedger::default();
        session.record_main_loop_call("parent-model", &tu(10, 1), None, Some(1));

        let slice = session.record_subagent_usage_slice(
            &[],
            std::slice::from_ref(&row),
            &["child-title".to_owned()],
            false,
        );
        assert_eq!(slice.totals.input_tokens, 7);
        assert_eq!(slice.attributions.len(), 1);
        assert!(slice.pending_attempts.contains("child-title"));
        assert_eq!(session.totals.input_tokens, 17);

        // A resent attempt must not reach the spawning turn a second time.
        let before = session.clone();
        let duplicate = session.record_subagent_usage_slice(&[], &[row], &[], false);
        assert_eq!(duplicate.totals, UsageTotals::default());
        assert!(duplicate.attributions.is_empty());
        assert_eq!(session.totals, before.totals);
        assert_eq!(session.attributions, before.attributions);

        // Aggregate-only folds carry no identity, so the slice is the fold itself.
        let aggregate = session.record_subagent_usage_slice(
            &[(
                "child-model".to_owned(),
                UsageTotals {
                    input_tokens: 5,
                    model_calls: 1,
                    ..Default::default()
                },
            )],
            &[],
            &[],
            false,
        );
        assert_eq!(aggregate.totals.input_tokens, 5);
        assert_eq!(session.totals.input_tokens, 22);
    }

    #[test]
    fn utility_attempt_metadata_serializes_into_ledger_row() {
        let attribution = UsageAttribution {
            attempt_id: "utility-attempt".to_owned(),
            task_id: Some("cite_spans".to_owned()),
            turn_id: None,
            request_id: None,
            role: "utility".to_owned(),
            model_id: "gpt-6-luna".to_owned(),
            endpoint: None,
            requested_effort: None,
            applied_effort: None,
            reason: Some("defer:consumer-rejected".to_owned()),
            bytes_in: Some(321),
            source_kind: None,
            final_decision: None,
            bytes_out: Some(88),
            status: UsageCallStatus::Rejected,
            usage: None,
            usage_complete: false,
            api_duration_ms: Some(4),
            cost_usd_ticks: None,
            cost_basis: UsageCostBasis::Unknown,
        };
        let mut ledger = UsageLedger::default();
        ledger.record_attribution(attribution);
        let json = serde_json::to_value(&ledger).expect("serialize usage ledger");
        assert_eq!(json["attributions"][0]["reason"], "defer:consumer-rejected");
        assert_eq!(json["attributions"][0]["bytes_in"], 321);
        assert_eq!(json["attributions"][0]["bytes_out"], 88);
        let mut old = json.clone();
        let row = old["attributions"][0].as_object_mut().expect("ledger row");
        row.remove("reason");
        row.remove("bytes_in");
        row.remove("bytes_out");
        serde_json::from_value::<UsageLedger>(old).expect("old usage ledger format");
    }

    /// Why eligible results stay raw must be answerable from the ledger alone,
    /// and the counters are telemetry: they must never move a bill.
    #[test]
    fn utility_outcomes_count_per_source_without_touching_billing() {
        let mut ledger = UsageLedger::default();
        ledger.record_utility_outcome("shell", "compress", 2, 10_000, 3_000);
        ledger.record_utility_outcome("shell", "not_shorter", 1, 5_000, 5_000);
        ledger.record_utility_outcome("shell", "compress", 1, 8_000, 2_000);
        ledger.record_utility_outcome("mcp", "keep:lane-unavailable", 0, 4_096, 4_096);

        let shell = &ledger.utility_outcomes["shell"];
        assert_eq!(shell.decisions["compress"], 2);
        assert_eq!(shell.decisions["not_shorter"], 1);
        assert_eq!((shell.chunks, shell.bytes_in, shell.bytes_out), (4, 23_000, 10_000));
        assert_eq!(ledger.utility_outcomes["mcp"].decisions["keep:lane-unavailable"], 1);
        assert_eq!(ledger.totals, UsageTotals::default());
        assert!(!ledger.is_incomplete());

        let json = serde_json::to_value(&ledger).expect("serialize outcomes");
        assert_eq!(json["utility_outcomes"]["shell"]["bytes_in"], 23_000);
        let mut old = json.clone();
        old.as_object_mut().expect("ledger").remove("utility_outcomes");
        let old: UsageLedger = serde_json::from_value(old).expect("ledger without outcomes");
        assert!(old.utility_outcomes.is_empty());

        let later = ledger.utility_outcomes["shell"].saturating_add(&shell.clone());
        assert_eq!(later.saturating_sub(shell), *shell, "a delta is what was added");
        assert_eq!(shell.max(&later), later);
    }

    /// A utility row says which source it served and how it ended, so
    /// acceptance per source is measurable without the opt-in decision log.
    #[test]
    fn utility_attribution_tags_round_trip_and_stay_optional() {
        let mut row = UsageAttribution {
            attempt_id: "tagged".to_owned(),
            task_id: Some("select_units".to_owned()),
            turn_id: None,
            request_id: None,
            role: "utility".to_owned(),
            model_id: "utility-model".to_owned(),
            endpoint: None,
            requested_effort: None,
            applied_effort: None,
            reason: None,
            bytes_in: Some(2_048),
            bytes_out: Some(512),
            source_kind: Some("shell".to_owned()),
            final_decision: Some("used".to_owned()),
            status: UsageCallStatus::Completed,
            usage: Some(tu(10, 2)),
            usage_complete: true,
            api_duration_ms: None,
            cost_usd_ticks: None,
            cost_basis: UsageCostBasis::Unknown,
        };
        let json = serde_json::to_value(&row).expect("serialize tagged row");
        assert_eq!(json["source_kind"], "shell");
        assert_eq!(json["final_decision"], "used");
        row.source_kind = None;
        row.final_decision = None;
        let json = serde_json::to_value(&row).expect("serialize untagged row");
        assert!(json.get("source_kind").is_none() && json.get("final_decision").is_none());
    }

    /// `/usage` prices one-hour writes (2x) apart from five-minute ones (1.25x), so the split
    /// must survive every fold, per model and in the session total.
    #[test]
    fn one_hour_cache_writes_fold_per_model_and_in_total() {
        let mut ledger = UsageLedger::default();
        let mut call = tu(1_000, 10);
        call.cache_creation_prompt_tokens = 300;
        call.cache_creation_1h_prompt_tokens = 200;
        ledger.record_main_loop_call("a", &call, None, None);
        ledger.record_subagent(
            &[(
                "b".into(),
                UsageTotals {
                    cache_creation_tokens: 50,
                    cache_creation_1h_tokens: 50,
                    model_calls: 1,
                    ..Default::default()
                },
            )],
            false,
        );
        assert_eq!(ledger.by_model["a"].cache_creation_1h_tokens, 200);
        assert_eq!(ledger.by_model["b"].cache_creation_1h_tokens, 50);
        assert_eq!(ledger.totals.cache_creation_tokens, 350);
        assert_eq!(ledger.totals.cache_creation_1h_tokens, 250);
    }

    /// A child snapshot from before the split existed has no field: it reads as all five-minute.
    #[test]
    fn totals_without_the_split_read_as_five_minute_writes() {
        let totals: UsageTotals = serde_json::from_value(serde_json::json!({
            "input_tokens": 1, "output_tokens": 1, "cached_read_tokens": 0,
            "cache_creation_tokens": 40, "reasoning_tokens": 0, "model_calls": 1,
            "api_duration_ms": 0, "cost_usd_ticks": null, "cost_missing_calls": 1
        }))
        .expect("old totals still parse");
        assert_eq!(totals.cache_creation_1h_tokens, 0);
    }

    /// The cache-break note is telemetry for `/usage`: recording one never moves a billed total.
    #[test]
    fn cache_break_is_kept_without_touching_totals() {
        let mut ledger = UsageLedger::default();
        ledger.record_main_loop_call("a", &tu(100, 10), None, Some(5));
        let before = ledger.totals.clone();
        ledger.record_cache_break(CacheBreak {
            first_changed_index: Some(3),
            changed_kind: Some("tool_result".into()),
            prompt_tokens: 52_000,
            cache_read_tokens: 1_000,
            secs_since_previous: 12,
            ..Default::default()
        });
        assert_eq!(ledger.totals, before);
        assert_eq!(
            ledger
                .last_cache_break
                .as_ref()
                .and_then(|b| b.first_changed_index),
            Some(3)
        );
    }
}
