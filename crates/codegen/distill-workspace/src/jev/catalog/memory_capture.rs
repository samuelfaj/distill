use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Question, QuestionId};

pub const MEMORY_CAPTURE_GATE_QUESTION: &str = "durable_knowledge";
pub const MEMORY_CAPTURE_NO_CONFIDENCE: f64 = 0.70;

pub fn memory_capture_gate_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    Ok(BTreeMap::from([(
        MEMORY_CAPTURE_GATE_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "Did this turn produce durable knowledge worth remembering across sessions: user preferences or corrections, conventions, decisions, or stable project facts?",
            "Durable knowledge was produced",
            "No durable knowledge was produced",
        ),
    )]))
}

pub fn compose_memory_capture_gate(answers: &JevAnswerSet) -> Option<bool> {
    match answers.answers.get(MEMORY_CAPTURE_GATE_QUESTION)? {
        Answer::Noul { noul } if *noul >= 0.5 => Some(true),
        Answer::Noul { noul } if 1.0 - *noul >= MEMORY_CAPTURE_NO_CONFIDENCE => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jev::catalog::test_support::{answers, noul};

    #[test]
    fn gate_requires_positive_yes_or_confident_no() {
        assert_eq!(
            compose_memory_capture_gate(&answers(vec![(MEMORY_CAPTURE_GATE_QUESTION, noul(0.8))])),
            Some(true)
        );
        assert_eq!(
            compose_memory_capture_gate(&answers(vec![(MEMORY_CAPTURE_GATE_QUESTION, noul(0.3))])),
            Some(false)
        );
        assert_eq!(
            compose_memory_capture_gate(&answers(vec![(MEMORY_CAPTURE_GATE_QUESTION, noul(0.4))])),
            None
        );
        assert_eq!(compose_memory_capture_gate(&answers(vec![])), None);
    }
}
