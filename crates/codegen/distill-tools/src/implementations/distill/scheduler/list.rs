// Modified for Distill by Samuel Fajreldines, 2026.
use crate::types::requirements::{Expr, ToolRequirement};

use crate::types::tool::{ToolKind, ToolNamespace};

use super::interval::interval_to_human;
use super::types::{SchedulerCommand, SchedulerHandle};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerListInput {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTaskSummary {
    pub id: String,
    pub prompt: String,
    pub interval_human: String,
    pub next_fire_at: String,
    pub created_at: String,
    pub recurring: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SchedulerListOutput {
    pub tasks: Vec<ScheduledTaskSummary>,
}

impl distill_tool_runtime::ToolOutput for SchedulerListOutput {}

#[derive(Debug, Default)]
pub struct SchedulerListTool;

impl crate::types::tool_metadata::ToolMetadata for SchedulerListTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }

    fn description_template(&self) -> &str {
        "List all active scheduled tasks with their IDs, prompts, intervals, and next fire times."
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        super::scheduler_bundle_requires_expr()
    }
}

impl distill_tool_runtime::Tool for SchedulerListTool {
    type Args = SchedulerListInput;
    type Output = SchedulerListOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("scheduler_list").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            "scheduler_list",
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

    #[tracing::instrument(name = "tool.scheduler_list", skip_all)]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        _input: SchedulerListInput,
    ) -> Result<SchedulerListOutput, distill_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let sender = {
            let res = resources.lock().await;
            res.get::<SchedulerHandle>()
                .ok_or_else(|| {
                    distill_tool_runtime::ToolError::custom(
                        "missing_dependency",
                        "missing dependency: SchedulerHandle",
                    )
                })?
                .0
                .clone()
        };

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(SchedulerCommand::List { reply: reply_tx })
            .map_err(|_| {
                distill_tool_runtime::ToolError::execution(
                    distill_tool_protocol::ToolId::new("scheduler_list").expect("valid"),
                    "Scheduler actor stopped",
                )
            })?;

        let snapshot = reply_rx.await.map_err(|_| {
            distill_tool_runtime::ToolError::execution(
                distill_tool_protocol::ToolId::new("scheduler_list").expect("valid"),
                "Scheduler actor dropped reply",
            )
        })?;

        let summaries = snapshot
            .tasks
            .into_iter()
            .map(|t| {
                let next_fire = t.next_fire_at().to_rfc3339();
                let created = t.created_at.to_rfc3339();
                let prompt = if t.prompt.len() > 80 {
                    format!("{}...", crate::util::truncate_str(&t.prompt, 80))
                } else {
                    t.prompt
                };
                ScheduledTaskSummary {
                    id: t.id,
                    prompt,
                    interval_human: interval_to_human(t.interval_secs),
                    next_fire_at: next_fire,
                    created_at: created,
                    recurring: t.recurring,
                }
            })
            .collect();

        Ok(SchedulerListOutput { tasks: summaries })
    }
}
