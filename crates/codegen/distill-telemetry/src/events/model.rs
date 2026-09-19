//! Model-switching product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct ModelSwitched {
    pub session_id: String,
    pub previous_model_id: String,
    pub new_model_id: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_agent_type: Option<String>,
}
