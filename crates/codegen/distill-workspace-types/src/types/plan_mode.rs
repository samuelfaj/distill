//! Plan-mode transition shapes used by [`ToolChunk::NeedPlanModeChange`](crate::chunks::ToolChunk::NeedPlanModeChange).
//! They also appear in [`ToolResponse::PlanModeChange`](crate::chunks::ToolResponse::PlanModeChange).
//!
//! Plan mode entry / exit go through the same bidirectional pattern as permission and user-question.
//! The workspace yields a `Need*` chunk on the tool's stream.
//! The sampler forwards it to the UI for approval and replies with the matching [`ToolResponse`] variant.
//! After approval the workspace applies the state change and the tool's `Final` chunk carries the new mode back to the sampler.
//!
//! Plan mode transitions are deliberately **not** broadcast on the EventBus; state changes caused by the sampler never go there.
//! The sampler is the only consumer that needs to know the new mode and it learns it from the tool's `Final` payload.

use serde::{Deserialize, Serialize};

/// Direction of a plan-mode transition the tool wants to make.
/// Adjacent tagging matches every other wire enum; see "# Wire format".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PlanModeTransition {
    /// Tool wants to enter plan mode.
    /// `plan` is the proposed plan content (`None` at the moment of entry; populated later in the same session as the plan develops).
    Enter {
        /// Optional initial plan text to seed the UI preview.
        #[serde(default)]
        plan: Option<String>,
    },
    /// Tool wants to exit plan mode and resume normal operation.
    /// `final_plan` is what the model will execute; the UI may render it for review.
    Exit {
        /// Optional final plan text the model will execute on exit.
        #[serde(default)]
        final_plan: Option<String>,
    },
}

/// User's decision on a proposed plan-mode transition. `Defer` is "not right now", not `Reject`; the model may re-propose.
/// Adjacent tagging is uniform across variant shapes and avoids the nested-`decision` hazard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PlanModeDecision {
    /// Approve the transition; the tool applies the change.
    Approve,
    /// Reject the transition; the tool emits `Err(WorkspaceError::Permission { .. })`.
    Reject {
        /// Optional user-provided context for the model.
        #[serde(default)]
        feedback: Option<String>,
    },
    /// Defer the transition; the tool emits a non-error `Final` indicating no change was made.
    /// The model may re-propose later.
    Defer,
}
