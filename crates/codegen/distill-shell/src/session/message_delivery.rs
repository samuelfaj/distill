// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;

use distill_message_delivery_core::{
    AgentSource, DeliveryEnvelope, DeliveryIdentity, HumanSource, Operation, OperationSet,
    authorize_operation,
};
use distill_tools::implementations::distill::task::coordinator::ActiveMessageAdmission;
use distill_tools::implementations::distill::task::types::ActiveAgentMessageDelivery;
use tokio::sync::{mpsc, oneshot};

use super::SessionCommand;

#[cfg(test)]
thread_local! {
    static HUMAN_SENDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_human_send_count() -> usize {
    HUMAN_SENDS.with(|count| count.replace(0))
}

pub(crate) struct HumanMessageId(String);
pub(crate) struct HumanPromptId(String);
pub(crate) type HumanDeliveryIdentity = DeliveryIdentity<HumanMessageId, HumanPromptId>;

pub(crate) fn human_delivery_identity(prompt_id: String) -> HumanDeliveryIdentity {
    DeliveryIdentity::new(HumanMessageId(prompt_id.clone()), HumanPromptId(prompt_id))
}

pub(crate) struct HumanPromptContent {
    pub(crate) prompt_blocks: Vec<agent_client_protocol::ContentBlock>,
    pub(crate) prompt_mode: crate::session::plan_mode::PromptMode,
    pub(crate) artifact_upload_ctx: Option<crate::upload::manifest::ArtifactUploadContext>,
    pub(crate) client_identifier: Option<String>,
    pub(crate) screen_mode: Option<String>,
    pub(crate) verbatim: bool,
    pub(crate) traceparent: Option<String>,
    pub(crate) json_schema: Option<serde_json::Value>,
    pub(crate) tool_overrides_update: Option<distill_sampling_types::ToolOverridesUpdate>,
    pub(crate) respond_to: oneshot::Sender<crate::session::commands::PromptTurnResult>,
    pub(crate) parsed_prompt_tx:
        Option<oneshot::Sender<crate::session::commands::ParsedPromptInfo>>,
}

impl HumanPromptContent {
    fn into_command(self, prompt_id: String) -> SessionCommand {
        SessionCommand::Prompt {
            prompt_id,
            prompt_blocks: self.prompt_blocks,
            prompt_mode: self.prompt_mode,
            artifact_upload_ctx: self.artifact_upload_ctx,
            client_identifier: self.client_identifier,
            screen_mode: self.screen_mode,
            verbatim: self.verbatim,
            traceparent: self.traceparent,
            json_schema: self.json_schema,
            send_now: false,
            admission: None,
            tool_overrides_update: self.tool_overrides_update,
            respond_to: self.respond_to,
            prompt_admitted: None,
            persist_ack: None,
            parsed_prompt_tx: self.parsed_prompt_tx,
        }
    }
}

pub(crate) struct ResidentHumanGrant {
    target_session_id: String,
}

impl ResidentHumanGrant {
    pub(crate) fn new(target_session_id: String) -> Self {
        Self { target_session_id }
    }
}

pub(crate) enum HumanDeliveryError {
    Rejected,
    Unsupported,
    ChannelClosed(String),
}

pub(crate) struct AgentMessageId(String);
pub(crate) struct AgentPromptId(String);
pub(crate) type AgentDeliveryIdentity = DeliveryIdentity<AgentMessageId, AgentPromptId>;

pub(crate) fn agent_delivery_identity(message_id: String) -> AgentDeliveryIdentity {
    DeliveryIdentity::new(
        AgentMessageId(message_id.clone()),
        AgentPromptId(format!("parent-message-{message_id}")),
    )
}

#[derive(Clone)]
pub(crate) struct MessageDeliveryHandle {
    cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    target_session_id: String,
}

impl MessageDeliveryHandle {
    pub(crate) fn new(
        cmd_tx: mpsc::UnboundedSender<SessionCommand>,
        target_session_id: String,
    ) -> Self {
        Self {
            cmd_tx,
            target_session_id,
        }
    }

    pub(crate) fn send_human(
        &self,
        envelope: DeliveryEnvelope<
            HumanSource,
            ResidentHumanGrant,
            HumanPromptContent,
            HumanDeliveryIdentity,
        >,
    ) -> Result<(), HumanDeliveryError> {
        #[cfg(test)]
        HUMAN_SENDS.with(|count| count.set(count.get() + 1));
        let (operation, content, identity, grant) = envelope.into_parts();
        let (HumanMessageId(message_id), HumanPromptId(prompt_id)) = identity.into_parts();
        if grant.target_session_id != self.target_session_id || message_id != prompt_id {
            return Err(HumanDeliveryError::Rejected);
        }
        if authorize_operation(OperationSet::QUEUE, operation).is_err() {
            return Err(HumanDeliveryError::Unsupported);
        }
        self.cmd_tx
            .send(content.into_command(prompt_id))
            .map_err(|error| HumanDeliveryError::ChannelClosed(error.to_string()))
    }

    pub(crate) async fn send(
        &self,
        envelope: DeliveryEnvelope<
            AgentSource,
            CoordinatorAgentDeliveryGrant,
            Arc<str>,
            AgentDeliveryIdentity,
        >,
        receipt_sink: mpsc::Sender<crate::agent::subagent::PromptTurnReceipt>,
        parent_telemetry_ctx: distill_telemetry::TelemetryCtx,
    ) -> ActiveMessageAdmission {
        let (operation, content, identity, grant) = envelope.into_parts();
        let delivery = grant.delivery;
        let _target_agent_id = grant.target_agent_id;
        let _target_generation = grant.target_generation;
        if grant.target_session_id != self.target_session_id {
            return ActiveMessageAdmission::Rejected;
        }
        let message = delivery.message();
        let (AgentMessageId(message_id), AgentPromptId(prompt_id)) = identity.into_parts();
        if content != message.text
            || message_id != message.message_id
            || prompt_id != format!("parent-message-{}", message.message_id)
        {
            return ActiveMessageAdmission::Rejected;
        }
        if operation != Operation::from(delivery.operation())
            || authorize_operation(OperationSet::QUEUE_STEER_AND_INTERJECT, operation).is_err()
        {
            return ActiveMessageAdmission::Unsupported;
        }

        let (respond_to, response_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(SessionCommand::ParentAgentMessage {
                delivery,
                receipt_sink,
                parent_telemetry_ctx,
                respond_to,
            })
            .is_err()
        {
            return ActiveMessageAdmission::ChannelClosed;
        }
        response_rx
            .await
            .unwrap_or(ActiveMessageAdmission::ChannelClosed)
    }
}

pub(crate) struct CoordinatorAgentDeliveryGrant {
    target_session_id: String,
    target_agent_id: distill_message_delivery_core::AgentId,
    target_generation:
        distill_tools::implementations::distill::task::root_control::AgentMessageGeneration,
    delivery: ActiveAgentMessageDelivery,
}

impl CoordinatorAgentDeliveryGrant {
    pub(crate) fn new(
        target_session_id: String,
        target_agent_id: distill_message_delivery_core::AgentId,
        target_generation: distill_tools::implementations::distill::task::root_control::AgentMessageGeneration,
        delivery: ActiveAgentMessageDelivery,
    ) -> Self {
        CoordinatorAgentDeliveryGrant {
            target_session_id,
            target_agent_id,
            target_generation,
            delivery,
        }
    }
}

#[cfg(test)]
#[path = "message_delivery_tests.rs"]
mod tests;
