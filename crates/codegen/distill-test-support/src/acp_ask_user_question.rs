// Modified for Distill by Samuel Fajreldines, 2026.
//! The shell's `x.ai/ask_user_question` extension request as the client reads and answers it. The typed request
//! (`AskUserQuestionExtRequest`) lives in `distill-tools`, which this crate does not depend on, so its wire
//! shape is mirrored here; a shell wire change touches only this module.

use agent_client_protocol as acp;
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub(crate) const ASK_USER_QUESTION_METHOD: &str = "x.ai/ask_user_question";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AskUserQuestionRequest {
    pub(crate) session_id: acp::SessionId,
    #[serde(default)]
    questions: Vec<Question>,
}

#[derive(Deserialize)]
struct Question {
    question: String,
    #[serde(default)]
    options: Vec<QuestionOption>,
}

#[derive(Deserialize)]
struct QuestionOption {
    label: String,
}

/// The reply that accepts every question of `request` with its first option; a question without options stays
/// unanswered. Answers are keyed by question text, which is how the shell maps them back to its questions.
pub(crate) fn accepted_reply(request: &AskUserQuestionRequest) -> Value {
    let answers: Map<String, Value> = request
        .questions
        .iter()
        .filter_map(|question| {
            let first = question.options.first()?;
            Some((question.question.clone(), json!([first.label])))
        })
        .collect();
    json!({ "outcome": "accepted", "answers": answers })
}

pub(crate) fn cancelled_reply() -> Value {
    json!({ "outcome": "cancelled" })
}
