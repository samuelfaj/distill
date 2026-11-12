//! C4 review battery for UltraCode, in shadow: two more notes about an
//! executed edit, asked in the same request and on the same state as the live
//! diff review, recorded and never acted on.
//!
//! Each note carries the evidence rule the live break question has (a
//! concrete defect the supplied evidence demonstrates; missing context or a
//! merely possible risk is not evidence), so a note can only rise on what the
//! change shows. The record also says what lower alert and second-opinion
//! floors would have flagged on the live answers and on these notes, so the
//! labelled precision of each floor can be compared before any floor moves.

use std::collections::BTreeMap;

use crate::jev::types::{JevAnswerSet, Question, QuestionId};

use super::noul_of;
use super::verify::{DIFF_BREAK_QUESTION, DIFF_INCOMPLETE_QUESTION, SECOND_OPINION_QUESTION};

/// Does the change mishandle a concrete edge case its code reaches?
pub const EDGE_CASE_QUESTION: &str = "edge_case_unhandled";
/// Does the change break a concrete invariant the code relies on?
pub const INVARIANT_QUESTION: &str = "invariant_at_risk";

/// Candidate alert floor recorded beside the live one
/// ([`super::verify::DIFF_REVIEW_FLAG_FLOOR`]); nothing acts on it.
pub const CANDIDATE_FLAG_FLOOR: f64 = 0.55;
/// Candidate second-opinion floor recorded beside the live one
/// ([`super::verify::SECOND_OPINION_FLOOR`]); nothing acts on it.
pub const CANDIDATE_SECOND_OPINION_FLOOR: f64 = 0.50;

/// Label prefix of the battery's shadow decisions.
pub const SHADOW_LABEL_PREFIX: &str = "shadow:c4:";

/// The two notes, worded like the live break question.
pub fn battery_questions() -> BTreeMap<QuestionId, Question> {
    BTreeMap::from([
        (
            EDGE_CASE_QUESTION.to_owned(),
            Question::noul_with_criteria(
                "Using `intent` and `change` from the state, does the supplied evidence demonstrate a concrete \
                 edge case the change mishandles: an empty, missing, boundary or failing input or state that \
                 its code reaches and gets wrong? Missing context or a merely possible case is not evidence.",
                "The evidence demonstrates an unhandled edge case",
                "No unhandled edge case is demonstrated",
            ),
        ),
        (
            INVARIANT_QUESTION.to_owned(),
            Question::noul_with_criteria(
                "Using `intent` and `change` from the state, does the supplied evidence demonstrate a concrete \
                 invariant the change breaks: an order, uniqueness, total, unit or unchanged value that the \
                 step or the surrounding code relies on and that no longer holds? Missing context or a merely \
                 possible risk is not evidence.",
                "The evidence demonstrates an invariant the change breaks",
                "No broken invariant is demonstrated",
            ),
        ),
    ])
}

/// The shadow label: `shadow:c4:` and what the candidate floors would have
/// flagged, joined by `.` (`none` when nothing; `defer` when a note is
/// unanswered). Live answers that are missing flag nothing.
pub fn shadow_label(live: Option<&JevAnswerSet>, battery: &JevAnswerSet) -> String {
    let (Some(edge), Some(invariant)) = (
        noul_of(battery, EDGE_CASE_QUESTION),
        noul_of(battery, INVARIANT_QUESTION),
    ) else {
        return format!("{SHADOW_LABEL_PREFIX}defer");
    };
    let live = |question: &str| live.and_then(|answers| noul_of(answers, question));
    let flags: Vec<&str> = [
        ("breaks", live(DIFF_BREAK_QUESTION), CANDIDATE_FLAG_FLOOR),
        (
            "incomplete",
            live(DIFF_INCOMPLETE_QUESTION),
            CANDIDATE_FLAG_FLOOR,
        ),
        ("edge_case", Some(edge), CANDIDATE_FLAG_FLOOR),
        ("invariant", Some(invariant), CANDIDATE_FLAG_FLOOR),
        (
            "second_opinion",
            live(SECOND_OPINION_QUESTION),
            CANDIDATE_SECOND_OPINION_FLOOR,
        ),
    ]
    .into_iter()
    .filter(|(_, p, floor)| p.is_some_and(|p| p >= *floor))
    .map(|(name, _, _)| name)
    .collect();
    if flags.is_empty() {
        format!("{SHADOW_LABEL_PREFIX}none")
    } else {
        format!("{SHADOW_LABEL_PREFIX}{}", flags.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, noul};
    use crate::jev::catalog::verify;

    /// Each note keeps the break question's evidence rule, and the battery
    /// shares no id with the live review, so riding along changes no live key.
    #[test]
    fn the_notes_carry_the_break_questions_evidence_rule() {
        let battery = battery_questions();
        let (_, live) = verify::diff_review_request("step", "change").expect("live battery");
        assert!(battery.keys().all(|id| !live.contains_key(id)));
        for question in battery.values() {
            let Question::Noul { instructions, .. } = question else {
                panic!("notes are nouls");
            };
            let text = instructions.as_str().expect("text");
            assert!(text.starts_with("Using `intent` and `change` from the state, does the supplied evidence demonstrate a concrete"));
            assert!(text.contains("is not evidence."));
        }
    }

    /// Candidate floors are lower than the live ones: the record shows what
    /// they would add, and nothing else.
    #[test]
    fn the_label_names_what_the_candidate_floors_flag() {
        const {
            assert!(CANDIDATE_FLAG_FLOOR < verify::DIFF_REVIEW_FLAG_FLOOR);
            assert!(CANDIDATE_SECOND_OPINION_FLOOR < verify::SECOND_OPINION_FLOOR);
        }
        let live = answers(vec![
            (verify::DIFF_BREAK_QUESTION, noul(0.6)),
            (verify::DIFF_INCOMPLETE_QUESTION, noul(0.2)),
            (verify::SECOND_OPINION_QUESTION, noul(0.5)),
        ]);
        let battery = answers(vec![
            (EDGE_CASE_QUESTION, noul(0.56)),
            (INVARIANT_QUESTION, noul(0.1)),
        ]);
        assert_eq!(
            shadow_label(Some(&live), &battery),
            "shadow:c4:breaks.edge_case.second_opinion"
        );
        let quiet = answers(vec![
            (EDGE_CASE_QUESTION, noul(0.1)),
            (INVARIANT_QUESTION, noul(0.1)),
        ]);
        assert_eq!(shadow_label(None, &quiet), "shadow:c4:none");
        assert_eq!(
            shadow_label(Some(&live), &answers(vec![(EDGE_CASE_QUESTION, noul(0.9))])),
            "shadow:c4:defer"
        );
        let all = answers(vec![
            (verify::DIFF_BREAK_QUESTION, noul(0.9)),
            (verify::DIFF_INCOMPLETE_QUESTION, noul(0.9)),
            (verify::SECOND_OPINION_QUESTION, noul(0.9)),
        ]);
        let loud = answers(vec![
            (EDGE_CASE_QUESTION, noul(0.9)),
            (INVARIANT_QUESTION, noul(0.9)),
        ]);
        // Event labels over 64 characters are replaced by a hash; the longest stays readable.
        assert!(shadow_label(Some(&all), &loud).len() <= 64);
    }
}
