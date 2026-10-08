// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-scoped Ultracode toggle for ACP clients (`x.ai/session/ultracode/set`).
//! The flag lives in memory on the session handle and never touches effort or `config.toml`.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;

pub const SET_METHOD: &str = "x.ai/session/ultracode/set";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSessionUltracodeRequest {
    session_id: String,
    /// Omitted toggles the current state.
    enabled: Option<bool>,
}

#[derive(Serialize)]
struct SetSessionUltracodeResponse {
    enabled: bool,
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if args.method.as_ref() != SET_METHOD {
        return Err(acp::Error::method_not_found());
    }
    let request: SetSessionUltracodeRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(request.session_id.as_str());
    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            request.session_id
        ))));
    };
    let flag = &handle.ultracode;
    let enabled = match request.enabled {
        Some(enabled) => {
            flag.store(enabled, std::sync::atomic::Ordering::Relaxed);
            enabled
        }
        None => !flag.fetch_xor(true, std::sync::atomic::Ordering::Relaxed),
    };
    to_raw_response(&SetSessionUltracodeResponse { enabled })
}
