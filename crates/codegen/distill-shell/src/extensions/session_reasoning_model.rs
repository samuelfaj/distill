// Modified for Distill by Samuel Fajreldines, 2026.
//! Conversation-scoped Reasoning-model selection for ACP clients.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSessionReasoningModelRequest {
    session_id: String,
    /// Empty explicitly disables Reasoning for this conversation.
    model_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetSessionReasoningModelResponse {
    reasoning_model_id: Option<String>,
    reasoning_enabled: bool,
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if args.method.as_ref() != "x.ai/session/reasoning_model/set" {
        return Err(acp::Error::method_not_found());
    }
    let request: SetSessionReasoningModelRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(request.session_id.as_str());
    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            request.session_id
        ))));
    };
    let model_id = (!request.model_id.trim().is_empty()).then(|| request.model_id.trim().to_owned());
    let (responds_to, response) = tokio::sync::oneshot::channel();
    handle
        .cmd_tx
        .send(crate::session::SessionCommand::SetReasoningModel {
            model_id,
            responds_to,
        })
        .map_err(|_| acp::Error::internal_error().data("session actor is unavailable"))?;
    let model_id = response
        .await
        .map_err(|_| acp::Error::internal_error().data("session actor dropped Reasoning update"))??;
    to_raw_response(&SetSessionReasoningModelResponse {
        reasoning_enabled: model_id.is_some(),
        reasoning_model_id: model_id,
    })
}
