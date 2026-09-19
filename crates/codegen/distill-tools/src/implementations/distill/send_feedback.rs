// Modified for Distill by Samuel Fajreldines, 2026.
//! `send_feedback` — save or update a local feedback draft for later review.

use std::sync::OnceLock;

use distill_feedback::{
    FeedbackDraft, FeedbackDraftId, FeedbackDraftInput, FeedbackDraftStore, FeedbackFailureMode,
    FeedbackStoreError, FeedbackTaskCategory, FeedbackType, UpdateOutcome,
};

use crate::types::resources::SessionFolder;
use crate::types::tool::{ToolKind, ToolNamespace};

pub const SEND_FEEDBACK_TOOL_NAME: &str = "send_feedback";
const SUCCESS_MESSAGE: &str = "Local feedback draft saved.";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SendFeedbackInput {
    /// Short summary for the draft.
    pub title: String,
    /// Short labeled bullets in this order: What happened, What the user said, Repro, optional Evidence, then optional verified Cause. Keep each to 1–3 lines; do not use narrative paragraphs.
    pub details: String,
    /// Optional product area; omit when unclear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Feedback classification.
    #[serde(rename = "type")]
    pub r#type: FeedbackType,
    /// Optional task category; omit when unclear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_category: Option<FeedbackTaskCategory>,
    /// Optional model-behavior failure mode; omit for a pure product or tool bug.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_mode: Option<FeedbackFailureMode>,
    /// Existing local draft to update. When set, this call does not append a second draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<String>,
}

impl SendFeedbackInput {
    /// Drop the draft id from user-facing text. The id is only a tool argument.
    fn without_draft_id_in_text(mut self) -> Self {
        let Some(id) = self
            .draft_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            return self;
        };
        self.title = strip_draft_id(&self.title, id);
        self.details = strip_draft_id(&self.details, id);
        if let Some(area) = self.area.as_mut() {
            *area = strip_draft_id(area, id);
        }
        self
    }
}

/// Removes the id; a line the id alone filled goes with it, blank lines the model wrote stay.
fn strip_draft_id(text: &str, id: &str) -> String {
    text.split('\n')
        .filter_map(|line| {
            if !line.contains(id) {
                return Some(line.to_owned());
            }
            let stripped = line.replace(id, "");
            (!stripped.trim().is_empty()).then_some(stripped)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl From<SendFeedbackInput> for FeedbackDraftInput {
    fn from(input: SendFeedbackInput) -> Self {
        Self {
            title: input.title,
            details: input.details,
            area: input.area,
            r#type: input.r#type,
            task_category: input.task_category,
            failure_mode: input.failure_mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SendFeedbackOutput {
    pub message: String,
}

impl distill_tool_runtime::ToolOutput for SendFeedbackOutput {}

impl From<SendFeedbackOutput> for crate::types::output::ToolOutput {
    fn from(output: SendFeedbackOutput) -> Self {
        Self::Text(output.message.into())
    }
}

#[derive(Debug, Default)]
pub struct SendFeedbackTool;

/// Absolute drafts file for a session folder. Tool descriptions only.
/// An empty folder (session-agnostic finalize, e.g. the workspace manifest)
/// yields an empty path so the description omits the sentence.
pub fn drafts_file_path(session_folder: &std::path::Path) -> String {
    if session_folder.as_os_str().is_empty() {
        return String::new();
    }
    session_folder
        .join(distill_feedback::FEEDBACK_DRAFTS_FILENAME)
        .display()
        .to_string()
}

/// Raw MiniJinja template. Built without `format!` so `${{ }}` / `${% %}`
/// survive; `format!` treats `{{` / `}}` as escaped braces and would cache a
/// one-brace peel that `TemplateRenderer` then skips.
fn build_description_template() -> String {
    let mut template = String::from(
        "# Overview\n\n\
Save or update user feedback for later review. Feedback is stored as local drafts and is never sent without explicit approval from the `/feedback` Drafts tab in the Grok TUI. This tool opens no UI and does not stop the current turn.\n\n\
# Invocation\n\n\
When the user types `/feedback` bare into the prompt bar, the form opens with the Write and Drafts tabs. The Write tab is only for the user to hand-write feedback.\n\
`/feedback <text>` sends the user's report immediately without involving you. Use the ${{ params.feedback.draft_id }} field only when the user explicitly asks you to update an existing feedback draft.\n\
When ${{ params.feedback.draft_id }} is set, update that existing draft. Do not duplicate drafts. ${{ params.feedback.draft_id }} is only a tool argument. Never write it into ${{ params.feedback.title }}, ${{ params.feedback.details }}, or ${{ params.feedback.area }}.\n\n\
When the user wants to share feedback implicitly, draft it with this tool, whether it is a product or model-behavior issue.\n\n\
# Usage\n\n\
Write ${{ params.feedback.details }} as short labeled bullets in this order: What happened, What the user said, Repro, optional Evidence, then optional verified Cause.\n\
Set ${{ params.feedback.failure_mode }} only for model-behavior feedback; omit it for a pure product or tool bug.\n\
${%- if tools.by_kind.ask_user %}\n\
If mapping feedback is incredibly unclear, only then may you use ${{ tools.by_kind.ask_user }} to confirm ambiguity with the user. Use this sparingly.\n\
${%- endif %}\n\n\
# Confirmation\n\n\
After drafting feedback and ending your turn, tell the user the draft is saved locally for this session. In the Grok CLI they review and send it by typing `/feedback` and opening the Drafts tab; from any other client, have them resume this session in the Grok CLI first.\n\n\
# Misc\n\n\
${%- if feedback_drafts_path %}\n\
This session's drafts file is ${{ feedback_drafts_path }}.\n\
${%- endif %}\n\
If the user's feedback can be answered from the docs (for example UI element locations or setup), read the Distill docs locally or online and answer alongside the created draft.\n\n\
",
    );
    template.push_str(&distill_feedback::taxonomy_prompt());
    template.push('\n');
    template
}

/// Render the model-facing description after `TemplateRenderer` exists.
fn render_advertised_description(
    renderer: &crate::types::template_renderer::TemplateRenderer,
    raw_desc: &str,
) -> String {
    let rendered = renderer.render(raw_desc).unwrap_or_else(|error| {
        crate::types::template_renderer::strip_markers_on_render_failure(raw_desc, &error)
    });
    strip_leaked_template_syntax(&rendered)
}

/// Remove real delimiters and the one-brace peel `format!` leaves behind.
fn strip_leaked_template_syntax(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        let Some(prefix) = rest.get(..start) else {
            break;
        };
        let Some(after) = rest.get(start + 2..) else {
            break;
        };
        let consumed = match close_template_span(after) {
            Some(end) => 2 + end,
            None => 2,
        };
        let Some(next) = rest.get(start + consumed..) else {
            break;
        };
        out.push_str(prefix);
        rest = next;
    }
    out.push_str(rest);
    out
}

fn close_template_span(after: &str) -> Option<usize> {
    if let Some(rest) = after.strip_prefix('{') {
        return rest.find("}}").map(|index| 1 + index + 2);
    }
    if let Some(rest) = after.strip_prefix('%') {
        return rest.find("%}").map(|index| 1 + index + 2);
    }
    if let Some(rest) = after.strip_prefix('#') {
        return rest.find("#}").map(|index| 1 + index + 2);
    }
    after.find('}').map(|index| index + 1)
}

impl crate::types::tool_metadata::ToolMetadata for SendFeedbackTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Feedback
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }

    fn description_template(&self) -> &str {
        static TEMPLATE: OnceLock<String> = OnceLock::new();
        TEMPLATE.get_or_init(build_description_template).as_str()
    }

    fn versioned_definition(
        &self,
        _contract_version: Option<&str>,
        client_name: &str,
        description_override: Option<&str>,
        renderer: &crate::types::template_renderer::TemplateRenderer,
        param_map: &std::collections::HashMap<String, String>,
        input_schema: &serde_json::Value,
        _effective_params: &serde_json::Value,
    ) -> crate::types::definition::ToolDefinition {
        let raw_desc = description_override.unwrap_or_else(|| self.description_template());
        let description = render_advertised_description(renderer, raw_desc);
        let remapped_schema = if param_map.is_empty() {
            input_schema.clone()
        } else {
            crate::util::remap::remap_schema_properties(input_schema, param_map)
        };
        crate::types::definition::ToolDefinition::function(
            client_name,
            Some(&description),
            remapped_schema,
        )
    }
}

impl distill_tool_runtime::Tool for SendFeedbackTool {
    type Args = SendFeedbackInput;
    type Output = SendFeedbackOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(SEND_FEEDBACK_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            SEND_FEEDBACK_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> distill_tool_protocol::ToolCapabilities {
        distill_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(distill_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "tool.send_feedback", skip_all)]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: SendFeedbackInput,
    ) -> Result<SendFeedbackOutput, distill_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let session_folder = resources.lock().await.require::<SessionFolder>()?.0.clone();
        let draft_id = input
            .draft_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| FeedbackDraftId::from(id.to_owned()));
        let input = FeedbackDraftInput::from(input.without_draft_id_in_text());
        let result = tokio::task::spawn_blocking(move || {
            let store = FeedbackDraftStore::new(session_folder);
            match draft_id {
                Some(draft_id) => match store.update_from_input(&draft_id, input)? {
                    UpdateOutcome::Updated => {
                        store
                            .get(&draft_id)?
                            .ok_or_else(|| FeedbackStoreError::DraftNotFound {
                                id: draft_id.clone(),
                            })
                    }
                    UpdateOutcome::NotFound => {
                        Err(FeedbackStoreError::DraftNotFound { id: draft_id })
                    }
                },
                None => store.append(input),
            }
        })
        .await;
        map_append_result(result)?;

        Ok(SendFeedbackOutput {
            message: SUCCESS_MESSAGE.to_owned(),
        })
    }
}

fn map_append_result(
    result: Result<Result<FeedbackDraft, FeedbackStoreError>, tokio::task::JoinError>,
) -> Result<FeedbackDraft, distill_tool_runtime::ToolError> {
    match result {
        Ok(Ok(draft)) => Ok(draft),
        Ok(Err(
            error @ (FeedbackStoreError::BlankTitle
            | FeedbackStoreError::BlankDetails
            | FeedbackStoreError::TitleTooLarge { .. }
            | FeedbackStoreError::DetailsTooLarge { .. }
            | FeedbackStoreError::AreaTooLarge { .. }
            | FeedbackStoreError::DraftNotFound { .. }),
        )) => Err(
            distill_tool_runtime::ToolError::invalid_arguments(error.to_string()).with_source(error),
        ),
        Ok(Err(error)) => Err(distill_tool_runtime::ToolError::new(
            distill_tool_runtime::ToolErrorKind::Execution,
            format!("Could not save the local feedback draft: {error}"),
        )
        .with_source(error)),
        Err(error) => Err(distill_tool_runtime::ToolError::new(
            distill_tool_runtime::ToolErrorKind::Execution,
            "Could not save the local feedback draft because the storage task failed.",
        )
        .with_source(error)),
    }
}

#[cfg(test)]
#[path = "send_feedback_tests.rs"]
mod tests;
