//! Frozen v1 feedback taxonomy: the serde snake_case spellings are the wire values downstream
//! consumers allowlist, so a variant rename must keep its spelling via `#[serde(rename)]`.

use serde::{Deserialize, Serialize};
use strum::{EnumIter, IntoEnumIterator};

const STRUCTURED_FEEDBACK_SCHEMA_VERSION: u64 = 1;

/// Where a report was composed: the modal's Write tab or a saved draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, EnumIter)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    Write,
    Draft,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FeedbackDraftId(String);

impl FeedbackDraftId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for FeedbackDraftId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for FeedbackDraftId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for FeedbackDraftId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, EnumIter,
)]
#[schemars(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FeedbackType {
    Bug,
    Idea,
    MissingCapability,
}

impl FeedbackType {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Bug => "Bug",
            Self::Idea => "Idea",
            Self::MissingCapability => "Missing capability",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, EnumIter,
)]
#[schemars(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FeedbackTaskCategory {
    CodeEdit,
    Debug,
    Explain,
    Plan,
    Shell,
    Search,
    Review,
    Other,
}

impl FeedbackTaskCategory {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::CodeEdit => "Code edit",
            Self::Debug => "Debug",
            Self::Explain => "Explain",
            Self::Plan => "Plan",
            Self::Shell => "Shell",
            Self::Search => "Search",
            Self::Review => "Review",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackFailureModeGroup {
    WrongAmountOfWork,
    WrongOutputs,
    Style,
}

impl FeedbackFailureModeGroup {
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::WrongAmountOfWork => "Doing the wrong amount of work",
            Self::WrongOutputs => "Wrong outputs",
            Self::Style => "Style",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, EnumIter,
)]
#[schemars(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FeedbackFailureMode {
    #[serde(alias = "did_too_much")]
    Overeager,
    #[serde(alias = "gave_up_early")]
    StoppedEarly,
    UnwantedScope,
    DidntAskForHelp,
    ExcessiveQuestions,
    SubagentOverspawn,
    OverCorrection,
    #[serde(alias = "ignored_direction")]
    IgnoredInstructions,
    #[serde(alias = "wrong_or_made_up")]
    Hallucinated,
    SloppyCode,
    #[serde(alias = "broke_something")]
    Destructive,
    LostContext,
    #[serde(alias = "stuck_in_loop")]
    StuckInALoop,
    ModelRegression,
    Disputed,
    WrongTone,
    UnclearOutput,
    Other,
}

impl FeedbackFailureMode {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Overeager => "Overeager",
            Self::StoppedEarly => "Stopping early",
            Self::UnwantedScope => "Unwanted scope",
            Self::DidntAskForHelp => "Didn't ask for help",
            Self::ExcessiveQuestions => "Excessive questions",
            Self::SubagentOverspawn => "Subagent overspawn",
            Self::OverCorrection => "Over correction",
            Self::IgnoredInstructions => "Instruction following",
            Self::Hallucinated => "Overconfidence and hallucination",
            Self::SloppyCode => "Code quality",
            Self::Destructive => "Destructive actions",
            Self::LostContext => "Context and memory",
            Self::StuckInALoop => "Repetition and looping",
            Self::ModelRegression => "Model regression",
            Self::Disputed => "Dispute or decline",
            Self::WrongTone => "Tone or preachiness",
            Self::UnclearOutput => "Unclear output",
            Self::Other => "Other",
        }
    }

    #[must_use]
    pub(crate) fn meaning(&self) -> &'static str {
        match self {
            Self::Overeager => {
                "Did more than asked, acted before being told, jumped in without enough info"
            }
            Self::StoppedEarly => "Quit early, handed back work that could have been finished",
            Self::UnwantedScope => "Not stopping",
            Self::DidntAskForHelp => "Didn't ask the user for help when stuck",
            Self::ExcessiveQuestions => {
                "Asked clarifying questions when there was enough to proceed"
            }
            Self::SubagentOverspawn => "Launched more subagents than the task warranted",
            Self::OverCorrection => "Fixed feedback by swinging too far the other way",
            Self::IgnoredInstructions => "Ignored or missed explicit instructions or constraints",
            Self::Hallucinated => "Stated something confidently that was wrong or fabricated",
            Self::SloppyCode => "Buggy, sloppy, or poorly structured code",
            Self::Destructive => "Did or risked something hard to reverse",
            Self::LostContext => {
                "Lost earlier context, forgot established facts, contradicted itself"
            }
            Self::StuckInALoop => "Repeated output or retried the same failing action",
            Self::ModelRegression => "Behavior noticeably worse than a previous model version",
            Self::Disputed => "Refused or argued against a reasonable request",
            Self::WrongTone => "Wrong tone — moralizing, condescending, sycophantic, verbose",
            Self::UnclearOutput => "Output was hard to read or interpret",
            Self::Other => "Model-behavior issue fitting none of the above",
        }
    }

    #[must_use]
    pub(crate) fn group(self) -> FeedbackFailureModeGroup {
        match self {
            Self::Overeager
            | Self::StoppedEarly
            | Self::UnwantedScope
            | Self::DidntAskForHelp
            | Self::ExcessiveQuestions
            | Self::SubagentOverspawn
            | Self::OverCorrection => FeedbackFailureModeGroup::WrongAmountOfWork,
            Self::IgnoredInstructions
            | Self::Hallucinated
            | Self::SloppyCode
            | Self::Destructive
            | Self::LostContext
            | Self::StuckInALoop
            | Self::ModelRegression => FeedbackFailureModeGroup::WrongOutputs,
            Self::Disputed | Self::WrongTone | Self::UnclearOutput | Self::Other => {
                FeedbackFailureModeGroup::Style
            }
        }
    }
}

#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct FeedbackTaxonomy {
    /// Feedback classification.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<FeedbackType>,
    /// Category of task the feedback concerns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_category: Option<FeedbackTaskCategory>,
    /// Optional failure classification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_mode: Option<FeedbackFailureMode>,
}

#[derive(Serialize)]
struct StructuredFeedbackEnvelope {
    schema_version: u64,
    source: FeedbackSource,
    #[serde(flatten)]
    taxonomy: FeedbackTaxonomy,
}

/// The POST `metadata` bag carrying the versioned `structured_feedback` envelope. Always present,
/// even with no enum set, so the read side can attribute every send to its `source`; absent enums
/// are omitted, never `null`.
#[must_use]
pub fn structured_feedback(
    source: FeedbackSource,
    taxonomy: FeedbackTaxonomy,
) -> serde_json::Value {
    serde_json::json!({
        "structured_feedback": StructuredFeedbackEnvelope {
            schema_version: STRUCTURED_FEEDBACK_SCHEMA_VERSION,
            source,
            taxonomy,
        }
    })
}

/// The read side of [`structured_feedback`]. Each field is parsed on its own, so an unknown value
/// from a newer client drops that field rather than the whole envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredFeedback {
    pub source: Option<FeedbackSource>,
    pub taxonomy: FeedbackTaxonomy,
}

/// Parses the `structured_feedback` envelope out of a POST `metadata` bag; `None` unless it carries
/// `schema_version == 1`.
#[must_use]
pub fn parse_structured_feedback(
    metadata: Option<&serde_json::Value>,
) -> Option<StructuredFeedback> {
    let envelope = metadata?.get("structured_feedback")?;
    if envelope.get("schema_version")?.as_u64()? != STRUCTURED_FEEDBACK_SCHEMA_VERSION {
        return None;
    }
    Some(StructuredFeedback {
        source: parse_envelope_field(envelope, "source"),
        taxonomy: FeedbackTaxonomy {
            r#type: parse_envelope_field(envelope, "type"),
            task_category: parse_envelope_field(envelope, "task_category"),
            failure_mode: parse_envelope_field(envelope, "failure_mode"),
        },
    })
}

fn parse_envelope_field<T: serde::de::DeserializeOwned>(
    envelope: &serde_json::Value,
    key: &str,
) -> Option<T> {
    T::deserialize(envelope.get(key)?).ok()
}

/// The frozen wire spelling of one taxonomy value; serde is the spelling authority.
#[must_use]
pub fn wire_value<T: Serialize>(value: T) -> Option<String> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(wire)) => Some(wire),
        _ => None,
    }
}

/// English failure-mode list for tool prompts. Labels only — no wire values.
#[must_use]
pub fn taxonomy_prompt() -> String {
    let mut sections: Vec<(FeedbackFailureModeGroup, Vec<FeedbackFailureMode>)> = Vec::new();
    for mode in FeedbackFailureMode::iter() {
        let group = mode.group();
        if let Some((_, modes)) = sections.iter_mut().find(|(existing, _)| *existing == group) {
            modes.push(mode);
        } else {
            sections.push((group, vec![mode]));
        }
    }

    let mut out = String::new();
    for (index, (group, modes)) in sections.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(group.label());
        out.push('\n');
        for mode in modes {
            out.push_str("- ");
            out.push_str(mode.label());
            out.push_str(": ");
            out.push_str(mode.meaning());
            out.push('\n');
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
#[path = "taxonomy_tests.rs"]
mod tests;
