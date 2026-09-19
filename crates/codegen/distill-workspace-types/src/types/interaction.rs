// Modified for Distill by Samuel Fajreldines, 2026.
//! User-interaction shapes referenced from [`ToolChunk::NeedUserAnswer`](crate::chunks::ToolChunk::NeedUserAnswer).
//! They also appear in [`ToolResponse::UserAnswer`](crate::chunks::ToolResponse::UserAnswer).
//!
//! Used by the `ask_user_question` tool flow.
//! The workspace yields a `NeedUserAnswer` chunk carrying a `Vec<UserQuestion>`.
//! The sampler prompts the user and replies with a matching `Vec<UserAnswer>` on the tool's bidi response sender.
//!
//! TODO(workspace): align with the canonical question/answer types in
//! `distill-tools` once the `ask_user_question` tool is extracted
//! into the workspace crate.

use serde::{Deserialize, Serialize};

/// A single question in an `ask_user_question` invocation.
/// Wire-only strings and a bool; the runtime crate owns UI and option validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestion {
    /// The complete question text shown to the user.
    pub question: String,
    /// Available options the user can select from.
    #[serde(default)]
    pub options: Vec<UserQuestionOption>,
    /// If true, the user can pick more than one option.
    #[serde(default)]
    pub multi_select: bool,
}

/// One choice within a [`UserQuestion`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserQuestionOption {
    /// Display text for this option (the value the user sees and selects).
    pub label: String,
    /// Explanation of what this option means or what will happen if chosen.
    #[serde(default)]
    pub description: String,
    /// Optional preview content (mockup, code snippet, etc.) rendered when this option is focused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// The user's answer to a single [`UserQuestion`].
/// Adjacent tagging matches every other wire enum; see "# Wire format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UserAnswer {
    /// The user picked one of the listed options (by label).
    Selected(String),
    /// The user picked the "Other" choice with custom free-form text.
    Other(String),
    /// The user picked multiple options (only valid when the corresponding [`UserQuestion::multi_select`] is `true`).
    Multiple(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_question_option_omits_preview_when_none() {
        let opt = UserQuestionOption {
            label: "x".into(),
            description: "y".into(),
            preview: None,
        };
        let json = serde_json::to_string(&opt).unwrap();
        assert!(
            !json.contains("preview"),
            "preview should be skipped: {json}"
        );
    }
}
