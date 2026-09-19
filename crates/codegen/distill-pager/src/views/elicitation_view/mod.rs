//! MCP elicitation card (`x.ai/mcp/elicit`).
//! Form and URL elicitation cards behave like the other cards that wait on the user (permission, question, plan approval).
//! State transitions live in [`state`], painting in [`render`].

mod render;
mod state;
#[cfg(test)]
mod tests;

pub use render::{ElicitHit, elicitation_view_height, render_elicitation_view};
pub use state::{
    ElicitResponseTx, ElicitationActionFocus, ElicitationFocus, ElicitationStage,
    ElicitationViewState, FieldValueUi, FormFieldUi, FormStage, UrlConsentStage, UrlDisplay,
    UrlWaitingStage,
};
