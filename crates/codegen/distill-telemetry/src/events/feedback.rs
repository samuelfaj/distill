//! Content-free product telemetry for the feedback program: submissions and draft-store operations.

use serde::Serialize;

/// The shell's POST verdict for one feedback submission.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSendOutcome {
    Submitted,
    /// No proxy client; the report was only persisted locally.
    LocalOnly,
    Failed,
}

/// User feedback reached `submit_feedback_workflow`; one emission per call regardless of outcome.
/// `source` and the taxonomy props are the frozen v1 wire values; an unknown or absent value
/// leaves the prop out.
#[derive(Serialize)]
pub struct UserFeedback {
    pub session_id: String,
    pub has_feedback_text: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_value: Option<i32>,
    pub is_solicited: bool,
    pub outcome: FeedbackSendOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_mode: Option<String>,
}

/// How the `/feedback` modal was opened.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackModalEntry {
    Slash,
    Palette,
}

/// The `/feedback` modal opened (emitted once at open, by the pager).
#[derive(Serialize)]
pub struct FeedbackModalOpened {
    pub session_id: String,
    /// Absent for programmatic opens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<FeedbackModalEntry>,
    /// A bare open that lands on Drafts when any exist, else Write.
    pub peek: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDraftOpKind {
    /// `/feedback <text>` written as a predraft at the queue drain (pager).
    CreatePredraft,
    List,
    Load,
    /// The unknown-outcome recovery write over `drafts/update`; there is no user draft edit.
    Recover,
    Delete,
}

/// Variant-only class of a draft-store failure: the store's error strings embed the session path.
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackDraftOpError {
    Busy,
    NotFound,
    InvalidDocument,
    Io,
    Other,
}

/// One user-initiated feedback draft-store operation.
#[derive(Serialize)]
pub struct FeedbackDraftOp {
    pub session_id: String,
    pub op: FeedbackDraftOpKind,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<FeedbackDraftOpError>,
    /// `list` only: rows returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_count: Option<u32>,
    /// `list` only: unreadable rows hidden from the listing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<u32>,
}
