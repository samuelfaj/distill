// Modified for Distill by Samuel Fajreldines, 2026.
use crate::types::requirements::{Expr, ToolRequirement};

use crate::types::tool::{ToolKind, ToolNamespace};

use super::types::{SchedulerCommand, SchedulerHandle, scheduler_tool_error};

/// Canonical tool name advertised by `SchedulerDeleteTool::id()`.
/// See note on `SCHEDULER_CREATE_TOOL_NAME`.
pub const SCHEDULER_DELETE_TOOL_NAME: &str = "scheduler_delete";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerDeleteInput {
    /// The scheduled task ID to cancel.
    #[schemars(description = "The task ID to cancel (from scheduler_create output)")]
    pub id: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerDeleteOutput {
    pub success: bool,
    pub message: String,
}

impl distill_tool_runtime::ToolOutput for SchedulerDeleteOutput {}

#[derive(Debug, Default)]
pub struct SchedulerDeleteTool;

impl crate::types::tool_metadata::ToolMetadata for SchedulerDeleteTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }

    fn description_template(&self) -> &str {
        r#"Cancel a scheduled task by ID.

Returns success: true if the task was found and removed, false if no task with that ID exists."#
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["ScheduledTaskRemoved"]
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        super::scheduler_bundle_requires_expr()
    }
}

impl distill_tool_runtime::Tool for SchedulerDeleteTool {
    type Args = SchedulerDeleteInput;
    type Output = SchedulerDeleteOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("scheduler_delete").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            "scheduler_delete",
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

    #[tracing::instrument(
        name = "tool.scheduler_delete",
        skip_all,
        fields(id = %input.id)
    )]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: SchedulerDeleteInput,
    ) -> Result<SchedulerDeleteOutput, distill_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let sender = {
            let res = resources.lock().await;
            res.get::<SchedulerHandle>()
                .ok_or_else(|| {
                    distill_tool_runtime::ToolError::custom("missing_resource", "SchedulerHandle")
                })?
                .0
                .clone()
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(SchedulerCommand::Delete {
                id: input.id.clone(),
                reply: reply_tx,
            })
            .map_err(|_| {
                distill_tool_runtime::ToolError::custom("process_manager", "Scheduler actor stopped")
            })?;

        let removed = reply_rx
            .await
            .map_err(|_| {
                distill_tool_runtime::ToolError::custom(
                    "process_manager",
                    "Scheduler actor dropped reply",
                )
            })?
            .map_err(scheduler_tool_error)?;

        if removed {
            Ok(SchedulerDeleteOutput {
                success: true,
                message: format!("Scheduled task {} cancelled.", input.id),
            })
        } else {
            Ok(SchedulerDeleteOutput {
                success: false,
                message: format!(
                    "No scheduled task with ID {} found. Use scheduler_list to see active tasks.",
                    input.id
                ),
            })
        }
    }
}
