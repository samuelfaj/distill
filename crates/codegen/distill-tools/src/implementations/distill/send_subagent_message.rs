// Modified for Distill by Samuel Fajreldines, 2026.
use crate::implementations::distill::task::backend::SubagentBackendResource;
use crate::implementations::distill::task::types::{
    ActiveAgentMessageOperation, ActiveAgentMessageOutcome, ActiveAgentMessageQuotaKind,
    ActiveAgentMessageRequest, ActiveMessageTarget, AgentMessageSenderResource,
    SubagentDepthCounter,
};
use crate::types::tool::{ToolKind, ToolNamespace};

pub const SEND_SUBAGENT_MESSAGE_TOOL_NAME: &str = "send_subagent_message";

/// How the message reaches an active subagent. An inactive subagent always
/// wakes and runs the text as its next turn.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SendSubagentMessageDelivery {
    /// Join the current turn at its next safe point (default).
    Steer,
    /// Wait as a later turn.
    Queue,
    /// Urgent: delivered ahead of pending steers at the earliest safe point; also interrupts a
    /// wait on background work.
    Interject,
}

impl From<SendSubagentMessageDelivery> for ActiveAgentMessageOperation {
    fn from(delivery: SendSubagentMessageDelivery) -> Self {
        match delivery {
            SendSubagentMessageDelivery::Steer => ActiveAgentMessageOperation::Steer,
            SendSubagentMessageDelivery::Queue => ActiveAgentMessageOperation::Queue,
            SendSubagentMessageDelivery::Interject => ActiveAgentMessageOperation::Interject,
        }
    }
}

/// The one place the legacy `queue` flag becomes an operation.
pub fn resolve_delivery(
    delivery: Option<SendSubagentMessageDelivery>,
    legacy_queue: bool,
) -> ActiveAgentMessageOperation {
    match delivery {
        Some(delivery) => ActiveAgentMessageOperation::from(delivery),
        None if legacy_queue => ActiveAgentMessageOperation::Queue,
        None => ActiveAgentMessageOperation::Steer,
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SendSubagentMessageInput {
    /// `parent` or the durable agent ID of the target subagent.
    pub subagent_id: String,
    /// Text to send to the subagent.
    pub text: String,
    /// Delivery operation; omitted means `steer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<SendSubagentMessageDelivery>,
    /// Legacy `queue: true`; accepted on the wire, hidden from the schema,
    /// ignored when `delivery` is present.
    #[serde(default)]
    #[schemars(skip)]
    pub queue: bool,
}

impl SendSubagentMessageInput {
    pub fn operation(&self) -> ActiveAgentMessageOperation {
        resolve_delivery(self.delivery, self.queue)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[non_exhaustive]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SendSubagentMessageOutput {
    Accepted {
        message_id: String,
    },
    NotFoundOrNotOwned,
    NotActiveOrFinalizing,
    Saturated {
        max_in_flight: usize,
    },
    QuotaExceeded {
        kind: ActiveAgentMessageQuotaKind,
        limit: usize,
    },
    AdmissionUncertain,
    NotAcceptedBeforeDeadline,
    Unsupported,
    Limit {
        max_bytes: usize,
        observed_bytes: usize,
    },
    ChannelClosed,
}

/// Delivery classification shared by tool hosts and presentations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendSubagentMessageDisposition {
    /// Admission was confirmed.
    Accepted,
    /// Admission was definitely rejected.
    Rejected,
    /// Admission or delivery could not be confirmed.
    Unconfirmed,
}

impl SendSubagentMessageOutput {
    /// Classify this output without collapsing uncertainty into failure.
    pub fn disposition(&self) -> SendSubagentMessageDisposition {
        match self {
            Self::Accepted { .. } => SendSubagentMessageDisposition::Accepted,
            Self::AdmissionUncertain => SendSubagentMessageDisposition::Unconfirmed,
            Self::NotFoundOrNotOwned
            | Self::NotActiveOrFinalizing
            | Self::Saturated { .. }
            | Self::QuotaExceeded { .. }
            | Self::NotAcceptedBeforeDeadline
            | Self::Unsupported
            | Self::Limit { .. }
            | Self::ChannelClosed => SendSubagentMessageDisposition::Rejected,
        }
    }
}

impl From<ActiveAgentMessageOutcome> for SendSubagentMessageOutput {
    fn from(outcome: ActiveAgentMessageOutcome) -> Self {
        match outcome {
            ActiveAgentMessageOutcome::Accepted { message_id } => Self::Accepted { message_id },
            ActiveAgentMessageOutcome::NotFoundOrNotOwned => Self::NotFoundOrNotOwned,
            ActiveAgentMessageOutcome::NotActiveOrFinalizing => Self::NotActiveOrFinalizing,
            ActiveAgentMessageOutcome::Saturated { max_in_flight } => {
                Self::Saturated { max_in_flight }
            }
            ActiveAgentMessageOutcome::QuotaExceeded { kind, limit } => {
                Self::QuotaExceeded { kind, limit }
            }
            ActiveAgentMessageOutcome::AdmissionUncertain => Self::AdmissionUncertain,
            ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline => Self::NotAcceptedBeforeDeadline,
            ActiveAgentMessageOutcome::Unsupported => Self::Unsupported,
            ActiveAgentMessageOutcome::Limit {
                max_bytes,
                observed_bytes,
            } => Self::Limit {
                max_bytes,
                observed_bytes,
            },
            ActiveAgentMessageOutcome::ChannelClosed => Self::ChannelClosed,
        }
    }
}

impl std::fmt::Display for SendSubagentMessageOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accepted { message_id } => {
                write!(f, "Message accepted (message_id: {message_id}).")
            }
            Self::NotFoundOrNotOwned => {
                f.write_str("Subagent not found or not owned by this session.")
            }
            Self::NotActiveOrFinalizing => f.write_str("Subagent is not active or is finalizing."),
            Self::Saturated { max_in_flight } => write!(
                f,
                "Message admission is saturated (maximum {max_in_flight} in flight)."
            ),
            Self::QuotaExceeded { kind, limit } => {
                write!(f, "Agent-message quota exceeded ({kind:?}, limit {limit}).")
            }
            Self::AdmissionUncertain => f.write_str(
                "Message admission could not be confirmed; the message may or may not have been accepted.",
            ),
            Self::NotAcceptedBeforeDeadline => {
                f.write_str("Message was not accepted before the delivery deadline.")
            }
            Self::Unsupported => {
                f.write_str("Active agent messages are unsupported in this context.")
            }
            Self::Limit {
                max_bytes,
                observed_bytes,
            } => write!(
                f,
                "Message size is invalid: observed {observed_bytes} bytes; maximum is {max_bytes} bytes."
            ),
            Self::ChannelClosed => {
                f.write_str("Message was not accepted because the subagent channel closed.")
            }
        }
    }
}

impl distill_tool_runtime::ToolOutput for SendSubagentMessageOutput {}

#[derive(Debug, Default)]
pub struct SendSubagentMessageTool;

impl crate::types::tool_metadata::ToolMetadata for SendSubagentMessageTool {
    fn kind(&self) -> ToolKind {
        ToolKind::ActiveAgentMessage
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }

    fn description_template(&self) -> &str {
        "Send a follow-up message to a subagent owned by this session. When called by a subagent, `parent` targets an active parent subagent, and a known agent ID targets another local subagent; an eligible completed subagent resumes with the same identity. For an active target, `delivery` selects how the message lands: `steer` (default) joins the current turn at its next safe point; `queue` waits as a later turn; `interject` is urgent — it is delivered ahead of pending steers at the earliest safe point and interrupts a subagent blocked waiting on background work."
    }
}

impl distill_tool_runtime::Tool for SendSubagentMessageTool {
    type Args = SendSubagentMessageInput;
    type Output = SendSubagentMessageOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new(SEND_SUBAGENT_MESSAGE_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            SEND_SUBAGENT_MESSAGE_TOOL_NAME,
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
        name = "tool.send_subagent_message",
        skip_all,
        fields(subagent_id = %input.subagent_id)
    )]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: SendSubagentMessageInput,
    ) -> Result<SendSubagentMessageOutput, distill_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (depth, backend, sender) = {
            let res = resources.lock().await;
            (
                res.get::<SubagentDepthCounter>().map(|value| value.0),
                res.get::<SubagentBackendResource>().cloned(),
                res.get::<AgentMessageSenderResource>().cloned(),
            )
        };
        let operation = input.operation();
        let outcome = if let Some(sender) = sender {
            let target = if input.subagent_id == "parent" {
                ActiveMessageTarget::Parent
            } else {
                let agent_id = distill_message_delivery_core::AgentId::parse(&input.subagent_id)
                    .ok_or_else(|| {
                        distill_tool_runtime::ToolError::invalid_arguments(
                            "subagent_id must be `parent` or a valid agent ID",
                        )
                    })?;
                ActiveMessageTarget::Agent { agent_id }
            };
            let request =
                match ActiveAgentMessageRequest::try_from_parts(target, input.text, operation) {
                    Ok(request) => request,
                    Err(outcome) => return Ok(outcome.into()),
                };
            sender.0.send(request).await
        } else {
            // Only a root may use the coordinator backend; an ungranted nested child is refused here.
            let (Some(0), Some(backend)) = (depth, backend) else {
                return Ok(SendSubagentMessageOutput::Unsupported);
            };
            let request = match ActiveAgentMessageRequest::try_new_with_operation(
                input.subagent_id,
                input.text,
                operation,
            ) {
                Ok(request) => request,
                Err(outcome) => return Ok(outcome.into()),
            };
            backend.backend().send_active_message(request).await
        };

        Ok(outcome.into())
    }
}

#[cfg(test)]
#[path = "send_subagent_message_tests.rs"]
mod tests;
