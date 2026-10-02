use std::collections::BTreeMap;

use crate::jev::error::JevError;
use crate::jev::types::{Answer, JevAnswerSet, Question, QuestionId};

pub const SUBAGENT_MODEL_QUESTION: &str = "worker_model_suitable";
pub const SUBAGENT_MODEL_MIN_CONFIDENCE: f64 = 0.75;

pub fn subagent_model_questions() -> Result<BTreeMap<QuestionId, Question>, JevError> {
    Ok(BTreeMap::from([(
        SUBAGENT_MODEL_QUESTION.to_owned(),
        Question::noul_with_criteria(
            "Can the worker model do this task as well as the main model? Prefer the worker for well-scoped reading, searching or mechanical checks; keep the main model for open design questions or subtle correctness reviews.",
            "The worker model can do this task as well as the main model",
            "The main model is needed for this task",
        ),
    )]))
}

pub fn compose_subagent_model(answers: &JevAnswerSet) -> Option<bool> {
    match answers.answers.get(SUBAGENT_MODEL_QUESTION)? {
        Answer::Noul { noul } if *noul >= SUBAGENT_MODEL_MIN_CONFIDENCE => Some(true),
        Answer::Noul { noul } if 1.0 - *noul >= SUBAGENT_MODEL_MIN_CONFIDENCE => Some(false),
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
            compose_subagent_model(&answers(vec![(SUBAGENT_MODEL_QUESTION, noul(0.8))])),
            Some(true)
        );
        assert_eq!(
            compose_subagent_model(&answers(vec![(SUBAGENT_MODEL_QUESTION, noul(0.7))])),
            None
        );
        assert_eq!(
            compose_subagent_model(&answers(vec![(SUBAGENT_MODEL_QUESTION, noul(0.1))])),
            Some(false)
        );
        assert_eq!(compose_subagent_model(&answers(vec![])), None);
    }
}
