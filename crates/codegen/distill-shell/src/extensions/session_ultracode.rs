// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-scoped Ultracode toggle for ACP clients (`x.ai/session/ultracode/set`).
//! Root mode is persisted with the session; it never touches effort or `config.toml`.

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

#[derive(Serialize, Deserialize)]
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
    if handle.tool_context.subagent_depth > 0 {
        return Err(acp::Error::invalid_params()
            .data("Ultracode mode can only be changed on the root session"));
    }
    let flag = &handle.ultracode;
    let enabled = request
        .enabled
        .unwrap_or_else(|| !flag.load(std::sync::atomic::Ordering::Relaxed));
    // The atomic file writer is shared with the existing session state. Commit
    // mode before exposing success so a resume cannot contradict the ACP ack.
    persist_ultracode(&handle.info, enabled)
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    flag.store(enabled, std::sync::atomic::Ordering::Relaxed);
    to_raw_response(&SetSessionUltracodeResponse { enabled })
}

pub(crate) fn load_ultracode(info: &crate::session::info::Info) -> std::io::Result<bool> {
    let path = crate::session::persistence::session_dir(info).join("ultracode.json");
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<SetSessionUltracodeResponse>(&bytes)
            .map(|state| state.enabled)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn persist_ultracode(
    info: &crate::session::info::Info,
    enabled: bool,
) -> std::io::Result<()> {
    let dir = crate::session::persistence::ensure_owner_only_session_dir(info)?;
    let bytes = serde_json::to_vec(&SetSessionUltracodeResponse { enabled })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::session::storage::write_bytes_atomic(&dir.join("ultracode.json"), &bytes)
}
