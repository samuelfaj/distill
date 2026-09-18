//! Per-turn distribution ledger: which model, at which effort, did how much.
//!
//! Every round of the agent loop notes the model and effort it will run with
//! (after the decision layer has had its say), and the response's usage lands on
//! the round noted last. At turn end the rows become the "where did this task
//! go" report that rides the turn-completed payload.

use std::time::Instant;

/// One (model, effort) pair as the turn end reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LedgerRow {
    pub model: String,
    pub effort: Option<String>,
    /// Model calls made with this pair (a retry counts: it was another call).
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl LedgerRow {
    /// Total tokens billed for this pair (input + output).
    pub(crate) fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The turn's model/effort split. Cheap by construction: one row per pair the
/// turn actually used, so a normal turn holds two or three.
#[derive(Debug, Default)]
pub(crate) struct JevTurnLedger {
    rows: Vec<LedgerRow>,
    /// Row the next response's usage belongs to.
    pending: Option<usize>,
    /// When the turn's first call was noted, i.e. the window its decisions
    /// belong to.
    started: Option<Instant>,
    /// Model id the next request must name, when the decision layer moved the
    /// round off the session model (the chat state still builds the request with
    /// the session id, and the request's own id wins on the wire).
    pending_route: Option<String>,
    /// Calls this turn's validation gate held back, so it cannot wedge the turn.
    holds: u32,
    /// Why the local model is off for the rest of this turn, after its endpoint
    /// refused a routed call (the routing is an optimization, never a single
    /// point of failure).
    local_failed: Option<String>,
    /// Palette level the auto decision chose for the next round, when it chose
    /// one (the value that level maps onto travels in the sampler config).
    pending_effort_label: Option<String>,
    /// Effort floor for the rest of the turn, set when a change review asked
    /// for a redo with more thinking: later rounds never run below it.
    effort_floor: Option<(String, xai_grok_sampling_types::ReasoningEffort)>,
}

impl JevTurnLedger {
    /// Notes the model and effort the next call will run with.
    ///
    /// Re-noting the same pair (a retry) bumps its call count instead of adding
    /// a row.
    pub(crate) fn note_round(&mut self, model: impl Into<String>, effort: Option<String>) {
        let model = model.into();
        let index = match self
            .rows
            .iter()
            .position(|row| row.model == model && row.effort == effort)
        {
            Some(index) => index,
            None => {
                self.rows.push(LedgerRow {
                    model,
                    effort,
                    ..Default::default()
                });
                self.rows.len() - 1
            }
        };
        if let Some(row) = self.rows.get_mut(index) {
            row.requests = row.requests.saturating_add(1);
        }
        self.pending = Some(index);
        self.started.get_or_insert_with(Instant::now);
    }

    /// Attributes one response's usage to the round noted last.
    ///
    /// Usage that arrives with no pending round (a subagent fold, a side call)
    /// is ignored rather than guessed at: the report only claims what it knows.
    pub(crate) fn add_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        let Some(index) = self.pending else {
            return;
        };
        if let Some(row) = self.rows.get_mut(index) {
            row.input_tokens = row.input_tokens.saturating_add(input_tokens);
            row.output_tokens = row.output_tokens.saturating_add(output_tokens);
        }
    }

    /// When this turn's first call was noted (the window its decisions belong to).
    pub(crate) fn window_start(&self) -> Option<Instant> {
        self.started
    }

    /// Remembers the model id this round's request must name (a routed call).
    pub(crate) fn set_pending_route(&mut self, model: impl Into<String>) {
        self.pending_route = Some(model.into());
    }

    /// Remembers the palette level the decision chose for the next round.
    pub(crate) fn set_pending_effort_label(&mut self, level: impl Into<String>) {
        self.pending_effort_label = Some(level.into());
    }

    /// Takes that level: one round reports it, the next one derives its own.
    pub(crate) fn take_pending_effort_label(&mut self) -> Option<String> {
        self.pending_effort_label.take()
    }

    /// Raises the turn's effort floor to `level`, keeping the highest so far.
    pub(crate) fn raise_effort_floor(
        &mut self,
        level: impl Into<String>,
        value: xai_grok_sampling_types::ReasoningEffort,
    ) {
        let level = level.into();
        self.effort_floor = match self.effort_floor.take() {
            // ReasoningEffort has no Ord (deliberately): the cost order is a
            // judgement, and the floor only ever has to keep the highest ask.
            Some((current_level, current_value))
                if effort_rank_of(current_value) >= effort_rank_of(value) =>
            {
                Some((current_level, current_value))
            }
            _ => Some((level, value)),
        };
    }

    /// The turn's effort floor, as `(level, value)`, when a redo set one.
    pub(crate) fn effort_floor(
        &self,
    ) -> Option<&(String, xai_grok_sampling_types::ReasoningEffort)> {
        self.effort_floor.as_ref()
    }

    /// Records that the local endpoint refused a routed call.
    pub(crate) fn note_local_failure(&mut self, reason: impl Into<String>) {
        self.local_failed = Some(reason.into());
    }

    /// Why the local model is off for this turn, if it is.
    pub(crate) fn local_failed_reason(&self) -> Option<String> {
        self.local_failed.clone()
    }

    /// How many calls this turn's validation gate has held.
    pub(crate) fn holds(&self) -> u32 {
        self.holds
    }

    /// Counts one held call.
    pub(crate) fn note_hold(&mut self) {
        self.holds = self.holds.saturating_add(1);
    }

    /// Whether a routed model is waiting for this round's request.
    pub(crate) fn has_pending_route(&self) -> bool {
        self.pending_route.is_some()
    }

    /// Takes the pending route, if any: the request carries it exactly once.
    pub(crate) fn take_pending_route(&mut self) -> Option<String> {
        self.pending_route.take()
    }

    /// Drains the turn: rows biggest first, then the ledger is empty again.
    pub(crate) fn take_rows(&mut self) -> Vec<LedgerRow> {
        let mut rows = std::mem::take(&mut self.rows);
        self.pending = None;
        self.started = None;
        self.pending_route = None;
        self.holds = 0;
        self.local_failed = None;
        self.pending_effort_label = None;
        self.effort_floor = None;
        rows.sort_by(|a, b| {
            b.tokens()
                .cmp(&a.tokens())
                .then_with(|| a.model.cmp(&b.model))
                .then_with(|| a.effort.cmp(&b.effort))
        });
        rows
    }
}

/// Cost order of the effort ladder, cheapest first (the ledger only compares).
fn effort_rank_of(effort: xai_grok_sampling_types::ReasoningEffort) -> u8 {
    use xai_grok_sampling_types::ReasoningEffort as E;
    match effort {
        E::None => 0,
        E::Minimal => 1,
        E::Low => 2,
        E::Medium => 3,
        E::High => 4,
        E::Xhigh => 5,
        E::Max => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_lands_on_the_round_it_belongs_to() {
        let mut ledger = JevTurnLedger::default();
        ledger.note_round("DeepSeek V4.1 Flash", Some("max".to_owned()));
        ledger.add_usage(1_000, 100);
        ledger.note_round("Qwen3.8 27B (local oMLX)", None);
        ledger.add_usage(200, 20);
        // The same pair again (a retry) bumps the count, not the row count.
        ledger.note_round("DeepSeek V4.1 Flash", Some("max".to_owned()));
        ledger.add_usage(500, 50);

        let rows = ledger.take_rows();
        assert_eq!(rows.len(), 2);
        let cloud = rows
            .iter()
            .find(|row| row.model.starts_with("DeepSeek"))
            .expect("cloud row");
        assert_eq!(cloud.requests, 2);
        assert_eq!(cloud.input_tokens, 1_500);
        assert_eq!(cloud.output_tokens, 150);
        let local = rows
            .iter()
            .find(|row| row.model.starts_with("Qwen"))
            .expect("local row");
        assert_eq!(local.requests, 1);
        assert_eq!(local.effort, None);
        assert_eq!(local.tokens(), 220);
        assert!(ledger.window_start().is_none(), "the drain resets the turn");
        assert!(ledger.take_rows().is_empty());
    }

    /// The level the decision chose is reported for exactly one round.
    #[test]
    fn the_chosen_level_labels_one_round() {
        let mut ledger = JevTurnLedger::default();
        assert_eq!(ledger.take_pending_effort_label(), None);
        ledger.set_pending_effort_label("medium");
        assert_eq!(
            ledger.take_pending_effort_label().as_deref(),
            Some("medium")
        );
        assert_eq!(
            ledger.take_pending_effort_label(),
            None,
            "the next round reports its own effort"
        );
    }

    /// A redo floor only holds the highest value asked for, and only this turn.
    #[test]
    fn the_effort_floor_keeps_the_highest_and_resets_with_the_turn() {
        use xai_grok_sampling_types::ReasoningEffort;
        let mut ledger = JevTurnLedger::default();
        assert!(ledger.effort_floor().is_none());
        ledger.raise_effort_floor("high", ReasoningEffort::High);
        assert_eq!(
            ledger.effort_floor().map(|(level, _)| level.as_str()),
            Some("high")
        );
        // A lower ask never lowers the floor.
        ledger.raise_effort_floor("low", ReasoningEffort::Low);
        assert_eq!(
            ledger.effort_floor().map(|(level, _)| level.as_str()),
            Some("high")
        );
        ledger.raise_effort_floor("xhigh", ReasoningEffort::Xhigh);
        assert_eq!(
            ledger.effort_floor().map(|(level, _)| level.as_str()),
            Some("xhigh")
        );
        ledger.note_round("m", None);
        let _ = ledger.take_rows();
        assert!(
            ledger.effort_floor().is_none(),
            "the next turn starts clean"
        );
    }

    /// A refused local call only ends local routing for that turn.
    #[test]
    fn a_local_failure_latches_until_the_turn_ends() {
        let mut ledger = JevTurnLedger::default();
        assert!(ledger.local_failed_reason().is_none());
        ledger.note_local_failure("400: reasoning_content must be passed back");
        assert!(
            ledger
                .local_failed_reason()
                .is_some_and(|reason| reason.contains("reasoning_content"))
        );
        ledger.note_round("m", None);
        let _ = ledger.take_rows();
        assert!(
            ledger.local_failed_reason().is_none(),
            "the next turn may try the local model again"
        );
    }

    /// The hold budget resets with the turn, so one bad turn cannot silence a
    /// later one — and it is what stops a systematic misfire from wedging work.
    #[test]
    fn holds_count_within_the_turn_and_reset_with_it() {
        let mut ledger = JevTurnLedger::default();
        assert_eq!(ledger.holds(), 0);
        ledger.note_hold();
        ledger.note_hold();
        assert_eq!(ledger.holds(), 2);
        ledger.note_round("m", None);
        let _ = ledger.take_rows();
        assert_eq!(ledger.holds(), 0, "a new turn starts with a full budget");
    }

    /// A routed round hands its model id to the request exactly once.
    #[test]
    fn the_pending_route_is_consumed_once() {
        let mut ledger = JevTurnLedger::default();
        assert_eq!(ledger.take_pending_route(), None);
        ledger.set_pending_route("Qwen3.8-27B-4bit");
        assert_eq!(
            ledger.take_pending_route().as_deref(),
            Some("Qwen3.8-27B-4bit")
        );
        assert_eq!(
            ledger.take_pending_route(),
            None,
            "the next round must not inherit the route"
        );
    }

    #[test]
    fn rows_come_back_biggest_first_and_orphan_usage_is_ignored() {
        let mut ledger = JevTurnLedger::default();
        // Usage before any round is noted: never guessed at.
        ledger.add_usage(9_999, 9_999);
        ledger.note_round("small", None);
        ledger.add_usage(10, 1);
        ledger.note_round("big", Some("max".to_owned()));
        ledger.add_usage(5_000, 500);

        let rows = ledger.take_rows();
        assert_eq!(rows.first().map(|row| row.model.as_str()), Some("big"));
        assert_eq!(rows.last().map(|row| row.model.as_str()), Some("small"));
        assert_eq!(
            rows.iter().map(LedgerRow::tokens).sum::<u64>(),
            5_511,
            "the orphan usage stayed out"
        );
        assert!(
            ledger.window_start().is_none(),
            "the drain closes the turn's window"
        );
        ledger.note_round("next turn", None);
        assert!(
            ledger.window_start().is_some(),
            "the next turn re-arms the window"
        );
    }
}
