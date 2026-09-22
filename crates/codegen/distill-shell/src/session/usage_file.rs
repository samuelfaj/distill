// Modified for Distill by Samuel Fajreldines, 2026.
//! Turn deltas come from this process's last applied live ledger, not from persisted session totals (those stay large after resume).

use distill_chat_state::{UsageAttribution, UsageCostBasis, UsageLedger};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageFile {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
    #[serde(default)]
    pub session: UsageSummary,
    #[serde(default)]
    pub turns: Vec<TurnUsage>,
    #[serde(skip)]
    last_incoming_turn: Option<u32>,
    #[serde(skip)]
    last_written_turn: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cost_is_partial: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usage_is_incomplete: bool,
    /// Sticky incompleteness from a terminal/missing-usage path. Pending
    /// attempt IDs are intentionally separate so a successful terminal row
    /// cannot erase this historical condition.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permanent_incomplete: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub turn_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub model_usage: IndexMap<String, UsageSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attributions: Vec<UsageAttribution>,
    /// Pending attempt IDs make a persisted incomplete row distinguishable
    /// from an older row whose incompleteness is already permanent/unknown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_attempt_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub turn_number: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ended_at: String,
    #[serde(flatten)]
    pub usage: UsageSummary,
}

impl UsageSummary {
    pub fn from_ledger(ledger: &UsageLedger) -> Self {
        let pending_attempt_ids = ledger.pending_attempts.iter().cloned().collect::<Vec<_>>();
        let mut model_usage = IndexMap::new();
        for (model, totals) in &ledger.by_model {
            let mut usage =
                Self::from_totals(totals, ledger.is_incomplete(), ledger.incomplete);
            usage.pending_attempt_ids = pending_attempt_ids.clone();
            model_usage.insert(model.clone(), usage);
        }
        let mut summary =
            Self::from_totals(&ledger.totals, ledger.is_incomplete(), ledger.incomplete);
        summary.primary_model_id = primary_model(&model_usage);
        summary.model_usage = model_usage;
        summary.attributions = ledger.attributions.clone();
        summary.pending_attempt_ids = pending_attempt_ids;
        summary
    }

    fn from_totals(
        totals: &distill_chat_state::UsageTotals,
        incomplete: bool,
        permanent_incomplete: bool,
    ) -> Self {
        Self {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cached_read_tokens: totals.cached_read_tokens,
            cache_creation_tokens: totals.cache_creation_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            total_tokens: totals.total_tokens(),
            model_calls: totals.model_calls,
            cost_usd_ticks: totals.cost_usd_ticks,
            cost_is_partial: totals.cost_is_partial(),
            usage_is_incomplete: incomplete,
            permanent_incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
            attributions: Vec::new(),
            pending_attempt_ids: Vec::new(),
        }
    }

    /// True when `self` is a same-process continuation of `previous` (no bucket shrank), so the turn delta is a subtract, not a full clone.
    pub fn covers(&self, previous: &Self) -> bool {
        self.input_tokens >= previous.input_tokens
            && self.output_tokens >= previous.output_tokens
            && self.model_calls >= previous.model_calls
            && attributions_cover(&self.attributions, &previous.attributions)
    }

    pub fn saturating_add(&self, other: &Self) -> Self {
        let mut model_usage = self.model_usage.clone();
        for (model, row) in &other.model_usage {
            let entry = model_usage.entry(model.clone()).or_default();
            let mut merged = entry.saturating_add_row(row);
            if let Some(attributed_cost) = complete_reported_cost_for_model(
                &self.attributions,
                &other.attributions,
                model,
                merged.model_calls,
            ) {
                if merged.cost_usd_ticks.is_none() && attributed_cost == 0 {
                    merged.cost_usd_ticks = Some(0);
                }
                if merged.cost_usd_ticks == Some(attributed_cost) {
                    merged.cost_is_partial = false;
                }
            }
            *entry = merged;
        }
        let mut out = self.saturating_add_row(other);
        out.attributions = merge_attributions(&self.attributions, &other.attributions);
        out.primary_model_id = primary_model(&model_usage);
        out.model_usage = model_usage;
        out.turn_count = self.turn_count.saturating_add(other.turn_count);
        out
    }

    fn saturating_add_row(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_add(other.cached_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            model_calls: self.model_calls.saturating_add(other.model_calls),
            cost_usd_ticks: merge_cost_ticks(self, other),
            cost_is_partial: self.cost_is_partial
                || other.cost_is_partial
                || has_untrusted_cost(self)
                || has_untrusted_cost(other),
            usage_is_incomplete: self.usage_is_incomplete || other.usage_is_incomplete,
            permanent_incomplete: self.permanent_incomplete || other.permanent_incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
            attributions: Vec::new(),
            pending_attempt_ids: merge_pending_attempt_ids(
                &self.pending_attempt_ids,
                &other.pending_attempt_ids,
            ),
        }
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        let previous_attempt_ids = other
            .attributions
            .iter()
            .filter(|attribution| !attribution.attempt_id.is_empty())
            .map(|attribution| attribution.attempt_id.as_str())
            .collect::<HashSet<_>>();
        let delta_attributions = self
            .attributions
            .iter()
            .filter(|attribution| {
                attribution.attempt_id.is_empty()
                    || !previous_attempt_ids.contains(attribution.attempt_id.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut model_usage = IndexMap::new();
        for (model, row) in &self.model_usage {
            let prev = other.model_usage.get(model).cloned().unwrap_or_default();
            let mut delta = row.saturating_sub_row(&prev);
            if let Some(attributed_cost) =
                complete_reported_cost_for_model(&delta_attributions, &[], model, delta.model_calls)
            {
                if delta.cost_usd_ticks.is_none() && attributed_cost == 0 {
                    delta.cost_usd_ticks = Some(0);
                }
                if delta.cost_usd_ticks == Some(attributed_cost) {
                    delta.cost_is_partial = false;
                }
            }
            if !delta.is_zero() {
                model_usage.insert(model.clone(), delta);
            }
        }
        let mut out = self.saturating_sub_row(other);
        out.attributions = delta_attributions;
        out.cost_usd_ticks = sub_cost_ticks(self, other, &out.attributions, out.model_calls);
        out.primary_model_id = primary_model(&model_usage);
        out.model_usage = model_usage;
        out
    }

    fn saturating_sub_row(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_sub(other.cached_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_sub(other.cache_creation_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_sub(other.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_sub(other.total_tokens),
            model_calls: self.model_calls.saturating_sub(other.model_calls),
            cost_usd_ticks: sub_cost_ticks(
                self,
                other,
                &[],
                self.model_calls.saturating_sub(other.model_calls),
            ),
            cost_is_partial: self.cost_is_partial
                || other.cost_is_partial
                || has_untrusted_cost(self)
                || has_untrusted_cost(other),
            usage_is_incomplete: self.usage_is_incomplete,
            permanent_incomplete: self.permanent_incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
            attributions: Vec::new(),
            pending_attempt_ids: self.pending_attempt_ids.clone(),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cached_read_tokens == 0
            && self.cache_creation_tokens == 0
            && self.reasoning_tokens == 0
            && self.model_calls == 0
            && self.cost_usd_ticks.is_none()
            && self.attributions.is_empty()
            && self.pending_attempt_ids.is_empty()
    }

    fn normalize_legacy_incomplete(&mut self) {
        if self.usage_is_incomplete
            && !self.permanent_incomplete
            && self.pending_attempt_ids.is_empty()
        {
            self.permanent_incomplete = true;
        }
        for row in self.model_usage.values_mut() {
            row.normalize_legacy_incomplete();
        }
    }

    fn refresh_incomplete_projection(&mut self) {
        self.usage_is_incomplete =
            self.permanent_incomplete || !self.pending_attempt_ids.is_empty();
        for row in self.model_usage.values_mut() {
            row.refresh_incomplete_projection();
        }
    }

    pub(crate) fn late_refresh_from_attributions(&self, live: &Self, persisted_turn: u32) -> Self {
        self.merge_attribution_snapshot(live, Some(persisted_turn), false)
    }

    pub(crate) fn reconcile_overlapping_snapshot(&self, live: &Self) -> Option<Self> {
        let known_attempt_ids = self
            .attributions
            .iter()
            .filter(|attribution| !attribution.attempt_id.is_empty())
            .map(|attribution| attribution.attempt_id.as_str())
            .collect::<HashSet<_>>();
        if !live.attributions.iter().any(|attribution| {
            !attribution.attempt_id.is_empty()
                && known_attempt_ids.contains(attribution.attempt_id.as_str())
        }) {
            return None;
        }
        Some(self.merge_attribution_snapshot(live, None, true))
    }

    fn merge_attribution_snapshot(
        &self,
        live: &Self,
        max_turn: Option<u32>,
        preserve_incoming_pending: bool,
    ) -> Self {
        let known_attempt_ids = self
            .attributions
            .iter()
            .filter(|attribution| !attribution.attempt_id.is_empty())
            .map(|attribution| attribution.attempt_id.as_str())
            .collect::<HashSet<_>>();
        let mut late = Self::default();
        let mut attributed = Self::default();
        let mut terminal_attempt_ids = HashSet::new();
        for attribution in &live.attributions {
            let belongs_to_persisted_turn = max_turn.is_none_or(|persisted_turn| {
                !attribution
                    .turn_id
                    .as_deref()
                    .and_then(|turn| turn.parse::<u32>().ok())
                    .is_some_and(|turn| turn > persisted_turn)
            });
            if !belongs_to_persisted_turn {
                continue;
            }
            if !attribution.attempt_id.is_empty() {
                terminal_attempt_ids.insert(attribution.attempt_id.as_str());
            }
            let mut ledger = UsageLedger::default();
            ledger.record_attribution(attribution.clone());
            let one = Self::from_ledger(&ledger);
            attributed = attributed.saturating_add(&one);
            if !attribution.attempt_id.is_empty()
                && known_attempt_ids.contains(attribution.attempt_id.as_str())
            {
                continue;
            }
            late = late.saturating_add(&one);
        }

        let mut baseline = self.clone();
        baseline.normalize_legacy_incomplete();
        let mut projected = baseline.saturating_add(&late);
        projected
            .pending_attempt_ids
            .retain(|attempt_id| !terminal_attempt_ids.contains(attempt_id.as_str()));
        for row in projected.model_usage.values_mut() {
            row.pending_attempt_ids
                .retain(|attempt_id| !terminal_attempt_ids.contains(attempt_id.as_str()));
        }
        if preserve_incoming_pending {
            let incoming_residual = aggregate_residual(live, &attributed);
            let baseline_attributed = summary_from_attributions(&baseline.attributions);
            let existing_residual = aggregate_residual(&baseline, &baseline_attributed);
            if !incoming_residual.is_zero() {
                if existing_residual.usage_covers(&incoming_residual) {
                    // This aggregate residual is already present in the
                    // retained precursor; a repeated stale snapshot must not
                    // add it a second time.
                } else if incoming_residual.usage_covers(&existing_residual) {
                    let residual_delta = incoming_residual.saturating_sub(&existing_residual);
                    projected = projected.saturating_add(&residual_delta);
                } else {
                    // Aggregate-only usage has no identity key. Preserve the
                    // attributed portion and keep the ambiguity explicit
                    // instead of guessing between duplicate and new spend.
                    projected.mark_reconciliation_unknown();
                }
            }
            projected.permanent_incomplete |= live.permanent_incomplete;
            let pending_attempt_ids = live
                .pending_attempt_ids
                .iter()
                .filter(|attempt_id| {
                    let attempt_id = (*attempt_id).as_str();
                    !projected
                        .attributions
                        .iter()
                        .any(|attribution| attribution.attempt_id == attempt_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            projected.pending_attempt_ids =
                merge_pending_attempt_ids(&projected.pending_attempt_ids, &pending_attempt_ids);
            for row in projected.model_usage.values_mut() {
                row.pending_attempt_ids =
                    merge_pending_attempt_ids(&row.pending_attempt_ids, &pending_attempt_ids);
            }
        }
        projected.refresh_incomplete_projection();
        projected
    }

    fn usage_covers(&self, previous: &Self) -> bool {
        self.input_tokens >= previous.input_tokens
            && self.output_tokens >= previous.output_tokens
            && self.cached_read_tokens >= previous.cached_read_tokens
            && self.cache_creation_tokens >= previous.cache_creation_tokens
            && self.reasoning_tokens >= previous.reasoning_tokens
            && self.total_tokens >= previous.total_tokens
            && self.model_calls >= previous.model_calls
            && previous.model_usage.iter().all(|(model, row)| {
                self.model_usage
                    .get(model)
                    .is_some_and(|current| current.usage_covers(row))
            })
    }

    fn mark_reconciliation_unknown(&mut self) {
        self.permanent_incomplete = true;
        self.usage_is_incomplete = true;
        for row in self.model_usage.values_mut() {
            row.permanent_incomplete = true;
            row.usage_is_incomplete = true;
        }
    }
}

pub enum UsageLoad {
    SessionNotFound,
    NoUsage,
    Ready(Box<SessionUsageFile>),
}

impl SessionUsageFile {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::default()
        }
    }

    pub fn load_for_session(session_id: &str) -> std::io::Result<UsageLoad> {
        load_for_session_in_root(
            session_id,
            &crate::util::distill_home::distill_home().join("sessions"),
        )
    }

    pub fn turn(&self, turn_number: u32) -> Option<&TurnUsage> {
        self.turns
            .iter()
            .find(|turn| turn.turn_number == turn_number)
    }

    pub fn retain_turns_through(&mut self, max_turn: u32) {
        self.normalize_legacy_incomplete();
        let permanent_incomplete = self.session.permanent_incomplete;
        self.turns.retain(|turn| turn.turn_number <= max_turn);
        let mut session = UsageSummary::default();
        for turn in &self.turns {
            session = session.saturating_add(&turn.usage);
        }
        session.permanent_incomplete |= permanent_incomplete;
        session.refresh_incomplete_projection();
        session.turn_count = self.turns.len() as u64;
        session.primary_model_id = primary_model(&session.model_usage);
        self.session = session;
    }

    fn normalize_legacy_incomplete(&mut self) {
        self.session.normalize_legacy_incomplete();
        for turn in &mut self.turns {
            turn.usage.normalize_legacy_incomplete();
        }
    }

    pub fn apply_turn(
        &mut self,
        turn_number: u32,
        ended_at: impl Into<String>,
        live: &UsageSummary,
        prev_live: Option<&UsageSummary>,
    ) -> u32 {
        let fold_into = (self.last_incoming_turn == Some(turn_number))
            .then_some(self.last_written_turn)
            .flatten();
        let written = self.apply_turn_internal(turn_number, ended_at, live, prev_live, fold_into);
        self.last_incoming_turn = Some(turn_number);
        self.last_written_turn = Some(written);
        written
    }

    pub(crate) fn restore_apply_cursor(
        &mut self,
        last_incoming_turn: Option<u32>,
        last_written_turn: Option<u32>,
    ) {
        self.last_incoming_turn = last_incoming_turn;
        self.last_written_turn = last_written_turn;
    }

    pub(crate) fn apply_cursor(&self) -> (Option<u32>, Option<u32>) {
        (self.last_incoming_turn, self.last_written_turn)
    }

    /// Fold into `fold_into` when this process already persisted the same incoming turn number (late interjection).
    /// Otherwise a colliding inherited row is renumbered.
    fn apply_turn_internal(
        &mut self,
        mut turn_number: u32,
        ended_at: impl Into<String>,
        live: &UsageSummary,
        prev_live: Option<&UsageSummary>,
        fold_into: Option<u32>,
    ) -> u32 {
        let ended_at = ended_at.into();
        self.updated_at = ended_at.clone();
        self.normalize_legacy_incomplete();

        let mut turn_usage = match prev_live {
            Some(prev) if live.covers(prev) => live.saturating_sub(prev),
            _ => live.clone(),
        };

        if let Some(fold_n) = fold_into
            && let Some(existing) = self
                .turns
                .iter_mut()
                .find(|turn| turn.turn_number == fold_n)
        {
            if turn_usage.is_zero() {
                existing.usage.permanent_incomplete |= live.permanent_incomplete;
                existing.usage.pending_attempt_ids = live.pending_attempt_ids.clone();
                existing.usage.usage_is_incomplete = existing.usage.permanent_incomplete
                    || !existing.usage.pending_attempt_ids.is_empty();
                for row in existing.usage.model_usage.values_mut() {
                    row.permanent_incomplete |= live.permanent_incomplete;
                    row.pending_attempt_ids = live.pending_attempt_ids.clone();
                    row.usage_is_incomplete =
                        row.permanent_incomplete || !row.pending_attempt_ids.is_empty();
                }
                self.refresh_incomplete_flags_from_turns(live);
                return fold_n;
            }
            existing.ended_at = ended_at;
            existing.usage = existing.usage.saturating_add(&turn_usage);
            existing.usage.permanent_incomplete |= live.permanent_incomplete;
            existing.usage.pending_attempt_ids = live.pending_attempt_ids.clone();
            existing.usage.usage_is_incomplete = existing.usage.permanent_incomplete
                || !existing.usage.pending_attempt_ids.is_empty();
            for row in existing.usage.model_usage.values_mut() {
                row.permanent_incomplete |= live.permanent_incomplete;
                row.pending_attempt_ids = live.pending_attempt_ids.clone();
                row.usage_is_incomplete =
                    row.permanent_incomplete || !row.pending_attempt_ids.is_empty();
            }
            existing.usage.turn_count = 1;
            existing.usage.primary_model_id = primary_model(&existing.usage.model_usage);
            self.session = self.session.saturating_add(&turn_usage);
            self.session.turn_count = self.turns.len() as u64;
            self.refresh_incomplete_flags_from_turns(live);
            return fold_n;
        }

        if self
            .turns
            .iter()
            .any(|turn| turn.turn_number == turn_number)
        {
            turn_number = self
                .turns
                .iter()
                .map(|turn| turn.turn_number)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
        }

        turn_usage.turn_count = 1;
        turn_usage.primary_model_id = primary_model(&turn_usage.model_usage);

        self.turns.push(TurnUsage {
            turn_number,
            ended_at,
            usage: turn_usage.clone(),
        });

        self.session = self.session.saturating_add(&turn_usage);
        self.session.turn_count = self.turns.len() as u64;
        self.session.primary_model_id = primary_model(&self.session.model_usage);
        self.refresh_incomplete_flags_from_turns(live);
        turn_number
    }

    fn refresh_incomplete_flags_from_turns(&mut self, live: &UsageSummary) {
        if self.turns.is_empty() {
            return;
        }

        self.normalize_legacy_incomplete();
        let terminal_attempt_ids = live
            .attributions
            .iter()
            .map(|attribution| attribution.attempt_id.as_str())
            .collect::<HashSet<_>>();
        for turn in &mut self.turns {
            turn.usage
                .pending_attempt_ids
                .retain(|attempt_id| !terminal_attempt_ids.contains(attempt_id.as_str()));
            for row in turn.usage.model_usage.values_mut() {
                row.pending_attempt_ids
                    .retain(|attempt_id| !terminal_attempt_ids.contains(attempt_id.as_str()));
            }
            turn.usage.refresh_incomplete_projection();
        }

        let mut session_pending_attempt_ids = Vec::new();
        for turn in &self.turns {
            session_pending_attempt_ids = merge_pending_attempt_ids(
                &session_pending_attempt_ids,
                &turn.usage.pending_attempt_ids,
            );
        }
        self.session.pending_attempt_ids = session_pending_attempt_ids;
        self.session.permanent_incomplete |= live.permanent_incomplete
            || self
                .turns
                .iter()
                .any(|turn| turn.usage.permanent_incomplete);
        self.session.usage_is_incomplete = self.session.permanent_incomplete
            || self
                .turns
                .iter()
                .any(|turn| !turn.usage.pending_attempt_ids.is_empty());
        for (model, row) in &mut self.session.model_usage {
            let mut pending_attempt_ids = Vec::new();
            row.permanent_incomplete |= self.turns.iter().any(|turn| {
                turn.usage
                    .model_usage
                    .get(model)
                    .is_some_and(|model_usage| model_usage.permanent_incomplete)
            });
            for turn in &self.turns {
                if let Some(model_usage) = turn.usage.model_usage.get(model) {
                    pending_attempt_ids = merge_pending_attempt_ids(
                        &pending_attempt_ids,
                        &model_usage.pending_attempt_ids,
                    );
                }
            }
            row.pending_attempt_ids = pending_attempt_ids;
            row.usage_is_incomplete =
                row.permanent_incomplete || !row.pending_attempt_ids.is_empty();
        }
    }
}

fn load_for_session_in_root(
    session_id: &str,
    sessions_root: &std::path::Path,
) -> std::io::Result<UsageLoad> {
    let Some(dir) = crate::session::persistence::find_persisted_session_dir_by_id_in_root_result(
        session_id,
        sessions_root,
    )?
    else {
        return Ok(UsageLoad::SessionNotFound);
    };
    let path = dir.join(crate::session::storage::USAGE_FILE);
    if !path.is_file() {
        return Ok(UsageLoad::NoUsage);
    }
    let data = std::fs::read(&path)?;
    if data.iter().all(u8::is_ascii_whitespace) {
        return Ok(UsageLoad::NoUsage);
    }
    match serde_json::from_slice(&data) {
        Ok(file) => Ok(UsageLoad::Ready(Box::new(file))),
        Err(e) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
    }
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn primary_model(model_usage: &IndexMap<String, UsageSummary>) -> Option<String> {
    model_usage
        .iter()
        .max_by_key(|(_, row)| (row.model_calls, row.total_tokens))
        .map(|(name, _)| name.clone())
}

fn attributions_cover(live: &[UsageAttribution], previous: &[UsageAttribution]) -> bool {
    let live_attempt_ids = live
        .iter()
        .filter(|attribution| !attribution.attempt_id.is_empty())
        .map(|attribution| attribution.attempt_id.as_str())
        .collect::<HashSet<_>>();
    previous
        .iter()
        .all(|old| old.attempt_id.is_empty() || live_attempt_ids.contains(old.attempt_id.as_str()))
}

fn merge_attributions(
    first: &[UsageAttribution],
    second: &[UsageAttribution],
) -> Vec<UsageAttribution> {
    let mut seen = HashSet::new();
    first
        .iter()
        .chain(second)
        .filter(|attribution| {
            attribution.attempt_id.is_empty()
                || seen.insert(attribution.attempt_id.as_str().to_owned())
        })
        .cloned()
        .collect()
}

fn merge_pending_attempt_ids(first: &[String], second: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    first
        .iter()
        .chain(second)
        .filter(|attempt_id| seen.insert(attempt_id.as_str()))
        .cloned()
        .collect()
}

fn summary_from_attributions(attributions: &[UsageAttribution]) -> UsageSummary {
    let mut summary = UsageSummary::default();
    for attribution in attributions {
        let mut ledger = UsageLedger::default();
        ledger.record_attribution(attribution.clone());
        summary = summary.saturating_add(&UsageSummary::from_ledger(&ledger));
    }
    summary
}

fn aggregate_residual(live: &UsageSummary, attributed: &UsageSummary) -> UsageSummary {
    let mut residual = live.saturating_sub(attributed);
    residual.attributions.clear();
    residual.pending_attempt_ids.clear();
    residual.usage_is_incomplete = residual.permanent_incomplete;
    for row in residual.model_usage.values_mut() {
        row.attributions.clear();
        row.pending_attempt_ids.clear();
        row.usage_is_incomplete = row.permanent_incomplete;
    }
    residual
}

fn complete_reported_cost_for_model(
    first: &[UsageAttribution],
    second: &[UsageAttribution],
    model_id: &str,
    expected_calls: u64,
) -> Option<i64> {
    if expected_calls == 0 {
        return None;
    }
    let mut count = 0_u64;
    let mut total = 0_i64;
    for attribution in first.iter().chain(second) {
        if attribution.model_id != model_id {
            continue;
        }
        let cost = (attribution.cost_basis == UsageCostBasis::Reported)
            .then_some(attribution.cost_usd_ticks)
            .flatten()
            .filter(|&cost| cost >= 0)?;
        count = count.saturating_add(1);
        total = total.saturating_add(cost);
    }
    (count == expected_calls).then_some(total)
}

fn has_complete_reported_free(attributions: &[UsageAttribution], expected_calls: u64) -> bool {
    expected_calls > 0
        && u64::try_from(attributions.len()).ok() == Some(expected_calls)
        && attributions.iter().all(|attribution| {
            attribution.cost_usd_ticks == Some(0)
                && attribution.cost_basis == UsageCostBasis::Reported
        })
}

fn trusted_cost_ticks(summary: &UsageSummary) -> Option<i64> {
    match summary.cost_usd_ticks {
        Some(cost) if cost > 0 => Some(cost),
        Some(0) if has_complete_reported_free(&summary.attributions, summary.model_calls) => {
            Some(0)
        }
        _ => None,
    }
}

fn has_untrusted_cost(summary: &UsageSummary) -> bool {
    if summary.cost_is_partial {
        return true;
    }
    let has_calls = summary.model_calls > 0 || !summary.attributions.is_empty();
    match summary.cost_usd_ticks {
        None => has_calls,
        Some(0) => {
            has_calls && !has_complete_reported_free(&summary.attributions, summary.model_calls)
        }
        Some(_) => false,
    }
}

fn merge_cost_ticks(a: &UsageSummary, b: &UsageSummary) -> Option<i64> {
    match (trusted_cost_ticks(a), trusted_cost_ticks(b)) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

fn sub_cost_ticks(
    live: &UsageSummary,
    previous: &UsageSummary,
    delta_attributions: &[UsageAttribution],
    delta_model_calls: u64,
) -> Option<i64> {
    let live = trusted_cost_ticks(live)?;
    let previous = trusted_cost_ticks(previous).unwrap_or(0);
    let delta = live.saturating_sub(previous);
    if delta > 0 || has_complete_reported_free(delta_attributions, delta_model_calls) {
        Some(delta)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "usage_file_tests.rs"]
mod tests;
