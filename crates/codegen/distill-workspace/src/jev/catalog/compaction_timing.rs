use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Question, QuestionId};

pub const COMPACTION_TIMING_QUESTION: &str = "independent_next_step";
pub const COMPACTION_TIMING_MIN_CONFIDENCE: f64 = 0.75;

pub fn compaction_timing_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    Ok(BTreeMap::from([(
        COMPACTION_TIMING_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "Is the next step independent of the detailed earlier conversation (a finished subtask), so the history can be summarized now without losing what the next steps need?",
            "The next step is independent",
            "The next step needs the detailed earlier conversation",
        ),
    )]))
}

pub fn compose_compaction_timing(answers: &JevAnswerSet) -> Option<bool> {
    match answers.answers.get(COMPACTION_TIMING_QUESTION)? {
        Answer::Noul { noul } if *noul >= COMPACTION_TIMING_MIN_CONFIDENCE => Some(true),
        Answer::Noul { noul } if 1.0 - *noul >= COMPACTION_TIMING_MIN_CONFIDENCE => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, noul};

    #[test]
    fn compose_requires_confident_yes_and_defers_other_answers() {
        assert_eq!(
            compose_compaction_timing(&answers(vec![(COMPACTION_TIMING_QUESTION, noul(0.8))])),
            Some(true)
        );
        assert_eq!(
            compose_compaction_timing(&answers(vec![(COMPACTION_TIMING_QUESTION, noul(0.7))])),
            None
        );
        assert_eq!(
            compose_compaction_timing(&answers(vec![(COMPACTION_TIMING_QUESTION, noul(0.2))])),
            Some(false)
        );
        assert_eq!(compose_compaction_timing(&answers(vec![])), None);
    }
}
