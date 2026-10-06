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
    /// What the harness saw of this request: failed calls and recorded checks.
    pub(crate) facts: super::turn_facts::TurnFacts,
    /// B1 intent for the current human turn.
    pub(crate) turn_intent: Option<String>,
    /// B6: the human request whose delegation hint waits for the next model
    /// request.
    pub(crate) delegation_hint: Option<String>,
    pub(crate) last_execution: Option<(String, Option<distill_sampling_types::ReasoningEffort>)>,
    /// The session's optional tool families. Session-scoped: draining the turn
    /// keeps it, so the tools array only grows when a later request needs
    /// another family.
    pub(crate) tool_families: distill_workspace::jev::catalog::routing::ToolFamilySelection,
    /// E6: the utility route of an `explore` child failed or no longer fits,
    /// so the rest of the child stays on its own model. Session-scoped:
    /// draining the turn keeps it.
    pub(crate) cheap_agent_off: bool,
    /// Model and effort the last main request was sent with. Session-scoped:
    /// a compaction or recap at the start of the next turn replays that
    /// request's prefix and must send the same effort to read its cache.
    last_main_request: Option<(String, Option<distill_sampling_types::ReasoningEffort>)>,
    /// The model and effort this prompt turn's main rounds stay on where the
    /// effort is part of the cached prefix. Set by a round that chose freely;
    /// dropped by a compaction and by the turn drain.
    turn_route: Option<TurnRouteAnchor>,
    /// Prompt tokens of the last main response: the prefix the next main
    /// request extends, i.e. what an effort switch would re-read uncached.
    /// Session-scoped, like the cache it describes.
    last_main_prompt_tokens: Option<u64>,
    /// Item hashes of the last main Messages request, to name where a later
    /// cache read broke. Session-scoped, like the cache it traces.
    pub(crate) prefix_trace: super::prompt_cache::PrefixTrace,
}

/// Prompt size under which a main round may still change its effort mid-turn.
///
/// On a backend whose cache is keyed by effort, a switch re-reads the whole
/// prompt uncached. Under 16k tokens that costs about what a cached read of a
/// 160k-token prompt does (cached input bills at roughly a tenth), so one
/// ordinary late round; above it the miss grows with every round the turn has
/// run, which is the cost the turn route exists to avoid.
pub(crate) const EFFORT_SWITCH_FREE_PROMPT_TOKENS: u64 = 16_384;

/// A main round's model and effort, as the turn route keeps them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnRoute {
    pub model: String,
    pub effort: Option<distill_sampling_types::ReasoningEffort>,
}

#[derive(Debug, Clone)]
struct TurnRouteAnchor {
    /// Prompt turn the route was chosen in; another turn chooses again.
    turn: u64,
    /// Session model the round started from, so a model switch chooses again.
    session_model: String,
    route: TurnRoute,
}

impl JevTurnLedger {
    pub(crate) fn set_turn_intent(&mut self, intent: impl Into<String>) {
        self.turn_intent = Some(intent.into());
    }

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
        self.last_main_prompt_tokens = Some(input_tokens);
    }

    /// Prompt tokens of the last main response, when one reported usage.
    pub(crate) fn last_main_prompt_tokens(&self) -> Option<u64> {
        self.last_main_prompt_tokens
    }

    /// The route a main round of `turn` started from `session_model` must keep:
    /// the one an earlier round of the same turn chose. `None` lets the round
    /// choose: a new turn, another session model, no round chosen yet (or a
    /// compaction since), or a prompt still small enough that a cache miss is
    /// cheap.
    pub(crate) fn kept_turn_route(&self, turn: u64, session_model: &str) -> Option<TurnRoute> {
        let anchor = self.turn_route.as_ref()?;
        if anchor.turn != turn || anchor.session_model != session_model {
            return None;
        }
        if self
            .last_main_prompt_tokens
            .is_none_or(|tokens| tokens < EFFORT_SWITCH_FREE_PROMPT_TOKENS)
        {
            return None;
        }
        Some(anchor.route.clone())
    }

    /// Records the route a freely chosen main round of `turn` runs with.
    pub(crate) fn anchor_turn_route(
        &mut self,
        turn: u64,
        session_model: impl Into<String>,
        route: TurnRoute,
    ) {
        self.turn_route = Some(TurnRouteAnchor {
            turn,
            session_model: session_model.into(),
            route,
        });
    }

    /// A compaction rewrote the prompt, so the cache the turn route protects is
    /// gone and the next round may choose again.
    pub(crate) fn note_compaction(&mut self) {
        self.turn_route = None;
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

    pub(crate) fn note_main_request(
        &mut self,
        model: Option<&str>,
        effort: Option<distill_sampling_types::ReasoningEffort>,
    ) {
        self.last_main_request = model.map(|model| (model.to_owned(), effort));
    }

    /// Effort for a request that replays the main prefix on `model`: the one
    /// the last main request sent, since a different effort (thinking setting
    /// on Messages, reasoning field elsewhere) misses the cached prefix. Falls
    /// back to `configured` before any main request, or when the last one ran
    /// on another model (whose cache this request cannot read anyway).
    pub(crate) fn main_replay_effort(
        &self,
        model: &str,
        configured: Option<distill_sampling_types::ReasoningEffort>,
    ) -> Option<distill_sampling_types::ReasoningEffort> {
        match &self.last_main_request {
            Some((last_model, effort)) if last_model == model => *effort,
            _ => configured,
        }
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
        self.facts = Default::default();
        self.turn_intent = None;
        self.last_execution = None;
        self.turn_route = None;
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

    /// Compaction usually fires at the start of a turn, after the previous
    /// turn's rows were drained; it must still replay that turn's effort.
    #[test]
    fn replay_effort_is_the_last_main_requests_and_survives_the_turn() {
        use distill_sampling_types::ReasoningEffort;
        let mut ledger = JevTurnLedger::default();
        assert_eq!(
            ledger.main_replay_effort("sol", Some(ReasoningEffort::Medium)),
            Some(ReasoningEffort::Medium),
            "before any main request the configured effort is all there is"
        );
        ledger.note_main_request(Some("sol"), Some(ReasoningEffort::Low));
        let _ = ledger.take_rows();
        assert_eq!(
            ledger.main_replay_effort("sol", Some(ReasoningEffort::Medium)),
            Some(ReasoningEffort::Low),
            "the auto-chosen effort, not the configured default, is what the cache holds"
        );
        ledger.note_main_request(Some("sol"), None);
        assert_eq!(
            ledger.main_replay_effort("sol", Some(ReasoningEffort::Medium)),
            None,
            "a main request sent without effort is replayed without one"
        );
        ledger.note_main_request(Some("utility"), Some(ReasoningEffort::High));
        assert_eq!(
            ledger.main_replay_effort("sol", Some(ReasoningEffort::Medium)),
            Some(ReasoningEffort::Medium),
            "a round routed to another model says nothing about this model's cache"
        );
    }

    /// On a backend whose cache is keyed by effort, a mid-turn switch re-read
    /// the whole prompt uncached (74% full misses on ChatGPT). Later rounds of
    /// a turn keep its route; only a new turn, a model switch, a compaction or
    /// a still-small prompt lets a round choose again.
    #[test]
    fn a_turn_keeps_its_route_until_switching_is_cheap() {
        use distill_sampling_types::ReasoningEffort;
        let high = TurnRoute {
            model: "sol".to_owned(),
            effort: Some(ReasoningEffort::High),
        };
        let mut ledger = JevTurnLedger::default();
        assert_eq!(
            ledger.kept_turn_route(3, "sol"),
            None,
            "the turn's first round chooses"
        );
        ledger.anchor_turn_route(3, "sol", high.clone());
        assert_eq!(
            ledger.kept_turn_route(3, "sol"),
            None,
            "without a reported prompt size the round behaves as before"
        );
        ledger.note_round("sol", Some("high".to_owned()));
        ledger.add_usage(EFFORT_SWITCH_FREE_PROMPT_TOKENS - 1, 10);
        assert_eq!(
            ledger.kept_turn_route(3, "sol"),
            None,
            "a miss on a small prompt is cheap, so the round may still choose"
        );
        ledger.add_usage(80_000, 10);
        assert_eq!(ledger.kept_turn_route(3, "sol"), Some(high.clone()));
        assert_eq!(
            ledger.kept_turn_route(4, "sol"),
            None,
            "a new prompt turn chooses again"
        );
        assert_eq!(
            ledger.kept_turn_route(3, "luna"),
            None,
            "after a model switch the old route's cache is not this model's"
        );
        ledger.note_compaction();
        assert_eq!(
            ledger.kept_turn_route(3, "sol"),
            None,
            "a compaction rewrote the prompt, so its cache is gone anyway"
        );
        ledger.anchor_turn_route(3, "sol", high);
        let _ = ledger.take_rows();
        assert_eq!(
            ledger.kept_turn_route(3, "sol"),
            None,
            "the drain ends the turn"
        );
        assert_eq!(
            ledger.last_main_prompt_tokens(),
            Some(80_000),
            "the prompt size describes the session's cache, not the turn"
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
