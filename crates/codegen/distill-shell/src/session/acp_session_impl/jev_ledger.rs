// Modified for Distill by Samuel Fajreldines, 2026.
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
    /// Whether the pending route is a utility/local request whose payload must
    /// be rewritten for the routed endpoint. Model selection routes keep the
    /// normal reasoning payload and only change the request model.
    pending_route_local: bool,
    /// Final effort to copy onto the request built before sampler preparation.
    /// The outer option distinguishes "prepared and no effort" from "not
    /// prepared".
    pending_request_effort: Option<Option<distill_sampling_types::ReasoningEffort>>,
    /// Palette level the auto decision chose for the next round, when it chose
    /// one (the value that level maps onto travels in the sampler config).
    pending_effort_label: Option<String>,
    /// Pending effort increase for the next call only.
    effort_floor: Option<(String, distill_sampling_types::ReasoningEffort)>,
    /// A review may reserve one higher-effort call per turn, not a sticky floor.
    review_escalated: bool,
    /// C4 asked for an independent reasoning review of the last executed edit.
    reasoning_review_pending: Option<String>,
    /// What the reasoning gates know about this request.
    pub(crate) reasoning: super::reasoning_gates::ReasoningGates,
    pub(crate) last_execution: Option<(String, Option<distill_sampling_types::ReasoningEffort>)>,
    /// Stable schemas within a turn; invalidate when the request or available names change.
    pub(crate) tool_selection: Option<(String, Vec<String>)>,
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

    /// Adds a side call's usage (the reasoning model advising the main one) to
    /// its own row, without touching the round the next response belongs to.
    pub(crate) fn add_side_usage(
        &mut self,
        model: impl Into<String>,
        effort: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    ) {
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
            row.input_tokens = row.input_tokens.saturating_add(input_tokens);
            row.output_tokens = row.output_tokens.saturating_add(output_tokens);
        }
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
        self.pending_route_local = false;
    }

    /// Records a local/utility route that needs the local payload rules.
    pub(crate) fn set_pending_local_route(&mut self, model: impl Into<String>) {
        self.pending_route = Some(model.into());
        self.pending_route_local = true;
    }

    /// Replaces the final route while retaining whether it is a local utility
    /// route. The final sampler config is the source of truth for the model.
    pub(crate) fn set_pending_route_with_locality(
        &mut self,
        model: impl Into<String>,
        local: bool,
    ) {
        self.pending_route = Some(model.into());
        self.pending_route_local = local;
    }

    /// Remembers the palette level the decision chose for the next round.
    pub(crate) fn set_pending_effort_label(&mut self, level: impl Into<String>) {
        self.pending_effort_label = Some(level.into());
    }

    /// Takes that level: one round reports it, the next one derives its own.
    pub(crate) fn take_pending_effort_label(&mut self) -> Option<String> {
        self.pending_effort_label.take()
    }

    /// Reserves at most one review-driven increase per turn.
    pub(crate) fn raise_effort_floor(
        &mut self,
        level: impl Into<String>,
        value: distill_sampling_types::ReasoningEffort,
    ) -> bool {
        if self.review_escalated {
            return false;
        }
        self.review_escalated = true;
        self.effort_floor = Some((level.into(), value));
        true
    }

    /// The turn's effort floor, as `(level, value)`, when a redo set one.
    pub(crate) fn effort_floor(
        &self,
    ) -> Option<&(String, distill_sampling_types::ReasoningEffort)> {
        self.effort_floor.as_ref()
    }

    pub(crate) fn take_effort_floor(
        &mut self,
    ) -> Option<(String, distill_sampling_types::ReasoningEffort)> {
        self.effort_floor.take()
    }

    pub(crate) fn request_reasoning_review(&mut self, change: String) {
        self.reasoning_review_pending = Some(change);
    }

    pub(crate) fn take_reasoning_review(&mut self) -> Option<String> {
        std::mem::take(&mut self.reasoning_review_pending)
    }

    /// Whether a routed model is waiting for this round's request.
    pub(crate) fn has_pending_route(&self) -> bool {
        self.pending_route.is_some()
    }

    /// The routed model waiting for this round, for the row's engine label.
    pub(crate) fn pending_route_model(&self) -> Option<String> {
        self.pending_route.clone()
    }

    pub(crate) fn pending_route_is_local(&self) -> bool {
        self.pending_route_local
    }

    pub(crate) fn set_pending_request_effort(
        &mut self,
        effort: Option<distill_sampling_types::ReasoningEffort>,
    ) {
        self.pending_request_effort = Some(effort);
    }

    pub(crate) fn take_pending_request_effort(
        &mut self,
    ) -> Option<Option<distill_sampling_types::ReasoningEffort>> {
        self.pending_request_effort.take()
    }

    /// Takes the pending route, if any: the request carries it exactly once.
    pub(crate) fn take_pending_route(&mut self) -> Option<(String, bool)> {
        self.pending_route.take().map(|model| {
            let local = self.pending_route_local;
            self.pending_route_local = false;
            (model, local)
        })
    }

    /// Drains the turn: rows biggest first, then the ledger is empty again.
    pub(crate) fn take_rows(&mut self) -> Vec<LedgerRow> {
        let mut rows = std::mem::take(&mut self.rows);
        self.pending = None;
        self.started = None;
        self.pending_route = None;
        self.pending_route_local = false;
        self.pending_request_effort = None;
        self.pending_effort_label = None;
        self.effort_floor = None;
        self.review_escalated = false;
        self.reasoning_review_pending = None;
        self.reasoning = Default::default();
        self.last_execution = None;
        self.tool_selection = None;
        rows.sort_by(|a, b| {
            b.tokens()
                .cmp(&a.tokens())
                .then_with(|| a.model.cmp(&b.model))
                .then_with(|| a.effort.cmp(&b.effort))
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reasoning model's advice is billed on its own row, like the main
    /// model's rounds, and never takes the usage of the round still pending.
    #[test]
    fn side_usage_gets_its_own_row_and_keeps_the_pending_round() {
        let mut ledger = JevTurnLedger::default();
        ledger.note_round("GPT-6-Luna (ChatGPT)", Some("medium".to_owned()));
        ledger.add_side_usage("GPT-6-Sol (ChatGPT)", Some("high".to_owned()), 4_000, 900);
        ledger.add_usage(10_000, 500);
        ledger.add_side_usage("GPT-6-Sol (ChatGPT)", Some("high".to_owned()), 1_000, 100);

        let rows = ledger.take_rows();
        let main = rows.iter().find(|row| row.model.starts_with("GPT-6-Luna")).unwrap();
        assert_eq!((main.requests, main.input_tokens, main.output_tokens), (1, 10_000, 500));
        let reasoning = rows.iter().find(|row| row.model.starts_with("GPT-6-Sol")).unwrap();
        assert_eq!(reasoning.effort.as_deref(), Some("high"));
        assert_eq!(
            (reasoning.requests, reasoning.input_tokens, reasoning.output_tokens),
            (2, 5_000, 1_000)
        );
    }

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

    #[test]
    fn review_escalation_expires_after_one_call_and_cannot_stack() {
        use distill_sampling_types::ReasoningEffort as E;
        let mut ledger = JevTurnLedger::default();
        assert!(ledger.raise_effort_floor("high", E::High));
        assert!(!ledger.raise_effort_floor("max", E::Max));
        assert_eq!(ledger.take_effort_floor(), Some(("high".into(), E::High)));
        assert!(ledger.take_effort_floor().is_none());
        assert!(!ledger.raise_effort_floor("max", E::Max));
        ledger.take_rows();
        assert!(ledger.raise_effort_floor("medium", E::Medium));
    }

    /// A routed round hands its model id to the request exactly once.
    #[test]
    fn the_pending_route_is_consumed_once() {
        let mut ledger = JevTurnLedger::default();
        assert_eq!(ledger.take_pending_route(), None);
        ledger.set_pending_route("Qwen3.8-27B-4bit");
        assert_eq!(
            ledger.take_pending_route(),
            Some(("Qwen3.8-27B-4bit".to_owned(), false))
        );
        assert_eq!(
            ledger.take_pending_route(),
            None,
            "the next round must not inherit the route"
        );
    }

    #[test]
    fn local_route_keeps_its_payload_kind_separate_from_model_selection() {
        let mut ledger = JevTurnLedger::default();
        ledger.set_pending_local_route("local-model");
        assert_eq!(
            ledger.take_pending_route(),
            Some(("local-model".to_owned(), true))
        );
        ledger.set_pending_route("worker-model");
        assert_eq!(
            ledger.take_pending_route(),
            Some(("worker-model".to_owned(), false))
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
