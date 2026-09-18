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

    /// Drains the turn: rows biggest first, then the ledger is empty again.
    pub(crate) fn take_rows(&mut self) -> Vec<LedgerRow> {
        let mut rows = std::mem::take(&mut self.rows);
        self.pending = None;
        self.started = None;
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
