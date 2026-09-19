//! Area D — context and cost (`todo.md` §2, D2…D3).
//!
//! These packs decide what the model gets to re-read on every following round,
//! which is where the token savings live. They are deliberately conservative:
//! dropping context is a downgrade, so the default answer is always "keep".
//!
//! (D1 — the compaction recorte — and D4 — call validation — live in
//! [`crate::jev::ladder`] because they shipped with the first ladder cut.)

use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Question, QuestionId};

use super::{Ranked, noul_of, rank_by_noul, score_of};

/// D2 — probability at or below which a big result "does not change the task".
pub const DISCARD_CHANGES_MAX: f64 = 0.20;
/// D2 — normalized utility at or below which a big result is not worth keeping.
pub const DISCARD_UTILITY_MAX: f64 = 0.25;
/// D3 — floor for re-injecting a retrieved chunk after compaction.
pub const POST_COMPACTION_FLOOR: f64 = 0.40;

/// D2: "does this result change the task?" plus a utility score.
pub fn big_output_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    let mut questions = BTreeMap::new();
    questions.insert(
        "changes_task".to_owned(),
        Question::noul_with_criteria(
            "Does this tool output change what the agent should do next (an error to fix, a value the task needs, a decision it enables)?",
            "It changes the next step",
            "It is informational only",
        ),
    );
    questions.insert(
        "utility".to_owned(),
        Question::score(
            "How useful is this output for the remaining work?",
            vec![
                serde_json::json!("No further use"),
                serde_json::json!("Might be needed later"),
                serde_json::json!("Needed for the remaining work"),
            ],
        )?,
    );
    Ok(questions)
}

/// D2: whether a large result may be dropped from the context.
#[derive(Debug, Clone, PartialEq)]
pub struct Retention {
    /// True when the result stays in the context (the default).
    pub keep: bool,
    pub changes: Option<f64>,
    pub utility: Option<f64>,
    /// True when the answers were unusable and the caller must keep the result.
    pub deferred: bool,
}

/// D2: drops a result only when *both* screens agree it is inert.
pub fn compose_retention(answers: &JevAnswerSet) -> Retention {
    let changes = noul_of(answers, "changes_task");
    let utility = score_of(answers, "utility");
    let (Some(changes), Some(utility)) = (changes, utility) else {
        return Retention {
            keep: true,
            changes,
            utility,
            deferred: true,
        };
    };
    Retention {
        keep: !(changes <= DISCARD_CHANGES_MAX && utility <= DISCARD_UTILITY_MAX),
        changes: Some(changes),
        utility: Some(utility),
        deferred: false,
    }
}

/// D3: one `noul` per retrieved chunk after a compaction — "still relevant?".
pub fn post_compaction_questions(
    chunks: &[String],
) -> Result<BTreeMap<QuestionId, Question>, JevError> {
    super::rank_questions(
        chunks,
        |id| {
            format!("Is chunk `{id}` still relevant to the work in progress after this compaction?")
        },
        "Still relevant",
        "No longer relevant",
    )
}

/// D3: keeps the still-relevant chunks; defers on doubt.
pub fn compose_post_compaction(answers: &JevAnswerSet, chunks: &[String]) -> Ranked {
    rank_by_noul(answers, chunks, POST_COMPACTION_FLOOR, true)
}

/// D2 helper: the size above which the retention screen is worth asking about.
/// Small results are never sent to Jev (the golden rule: no call for nothing).
pub const BIG_OUTPUT_BYTES: usize = 4_000;

/// D2 helper: true when a result is large enough to consider the screen.
pub fn is_big_output(byte_len: usize) -> bool {
    byte_len >= BIG_OUTPUT_BYTES
}

/// Convenience for call sites: whether the answer set even contains a usable
/// retention verdict (used to skip recording noise).
pub fn retention_is_usable(answers: &JevAnswerSet) -> bool {
    matches!(
        answers.answers.get("changes_task"),
        Some(Answer::Noul { .. })
    ) && answers.answers.contains_key("utility")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, ids, noul, score};

    #[test]
    fn d2_drops_only_inert_outputs_and_keeps_on_doubt() {
        let inert = compose_retention(&answers(vec![
            ("changes_task", noul(0.05)),
            ("utility", score(0.0)),
        ]));
        assert!(!inert.keep, "no change and no utility ⇒ safe to drop");
        assert!(!inert.deferred);

        let useful = compose_retention(&answers(vec![
            ("changes_task", noul(0.05)),
            ("utility", score(2.0)),
        ]));
        assert!(
            useful.keep,
            "high utility keeps it even without changing the task"
        );

        let changing = compose_retention(&answers(vec![
            ("changes_task", noul(0.9)),
            ("utility", score(0.0)),
        ]));
        assert!(changing.keep, "an error to fix is always kept");

        let partial = compose_retention(&answers(vec![("changes_task", noul(0.05))]));
        assert!(partial.keep && partial.deferred, "missing answers ⇒ keep");
        assert!(retention_is_usable(&answers(vec![
            ("changes_task", noul(0.0)),
            ("utility", score(1.0))
        ])));
    }

    #[test]
    fn d2_size_gate_keeps_small_results_out_of_jev() {
        assert!(!is_big_output(BIG_OUTPUT_BYTES - 1));
        assert!(is_big_output(BIG_OUTPUT_BYTES));
    }

    #[test]
    fn d3_reinjects_only_still_relevant_chunks() {
        let chunks = ids(3);
        let ranked = compose_post_compaction(
            &answers(vec![
                ("rank_cand-0", noul(0.9)),
                ("rank_cand-1", noul(0.1)),
                ("rank_cand-2", noul(0.5)),
            ]),
            &chunks,
        );
        assert_eq!(ranked.keep, vec!["cand-0".to_owned(), "cand-2".to_owned()]);
        let doubt = compose_post_compaction(&answers(vec![("rank_cand-0", noul(0.9))]), &chunks);
        assert!(doubt.is_deferred(), "incomplete answers ⇒ keep everything");
        assert!(post_compaction_questions(&chunks).is_ok());
    }
}
