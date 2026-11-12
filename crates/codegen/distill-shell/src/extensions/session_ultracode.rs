// Modified for Distill by Samuel Fajreldines, 2026.
//! Exclusive session execution mode; the old UltraCode endpoint remains an alias.
use agent_client_protocol as acp;
use distill_tool_types::{ExecutionMode, ExecutionModeChoice, ExecutionModeOperation};
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;

pub const SET_METHOD: &str = "x.ai/session/ultracode/set";
pub const MODE_SET_METHOD: &str = "x.ai/session/execution_mode/set";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetModeRequest {
    session_id: String,
    mode: ExecutionModeChoice,
    #[serde(default)]
    operation: ExecutionModeOperation,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSessionUltracodeRequest {
    session_id: String,
    enabled: Option<bool>,
}

/// `mode` is the effective mode; `choice` is absent in files written before Auto.
#[derive(Serialize, Deserialize)]
struct ModeState {
    mode: ExecutionMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    choice: Option<ExecutionModeChoice>,
}

#[derive(Serialize, Deserialize)]
struct SetSessionUltracodeResponse {
    enabled: bool,
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let config = crate::config::load_effective_config().ok();
    handle_with_config(agent, args, config.as_ref()).await
}

/// [`handle`] with the effective config `root` that pinned-mode settings are read from.
pub(crate) async fn handle_with_config(
    agent: &MvpAgent,
    args: &acp::ExtRequest,
    root: Option<&toml::Value>,
) -> ExtResult {
    let (session_id, requested_mode, enabled) = match args.method.as_ref() {
        MODE_SET_METHOD => {
            let request: SetModeRequest = parse_params(args)?;
            (
                request.session_id,
                Some((request.mode, request.operation)),
                None,
            )
        }
        SET_METHOD => {
            let request: SetSessionUltracodeRequest = parse_params(args)?;
            (request.session_id, None, request.enabled)
        }
        _ => return Err(acp::Error::method_not_found()),
    };
    let Some(handle) = agent
        .session_handle_waiting_for_load(&acp::SessionId::new(session_id.as_str()))
        .await
    else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {session_id}"
        ))));
    };
    if handle.tool_context.subagent_depth > 0 {
        return Err(acp::Error::invalid_params()
            .data("Execution mode can only be changed on the root session"));
    }
    let current = handle.execution_mode.load();
    let current_choice = handle.execution_mode.choice();
    let choice = match requested_mode {
        Some((target, operation)) => match operation {
            ExecutionModeOperation::Set => target,
            ExecutionModeOperation::Toggle if current_choice != target => target,
            ExecutionModeOperation::Toggle | ExecutionModeOperation::Disable
                if current_choice == target =>
            {
                ExecutionModeChoice::Auto
            }
            _ => current_choice,
        },
        None => {
            if enabled.unwrap_or(current != ExecutionMode::Ultracode) {
                ExecutionModeChoice::Ultracode
            } else if current == ExecutionMode::Ultracode {
                ExecutionModeChoice::Normal
            } else {
                current_choice
            }
        }
    };
    // Auto keeps the mode it last picked until its next decision.
    let mode = choice.fixed().unwrap_or(current);
    // Persist before acknowledging. There is no await between read and commit.
    persist_mode(&handle.info, choice, mode)
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    if choice == ExecutionModeChoice::Auto {
        handle.execution_mode.store_auto(mode);
    } else {
        handle.execution_mode.store(mode);
    }
    crate::jev::emit_mode_switched(
        Some(handle.info.id.0.as_ref()),
        current,
        mode,
        distill_session_events::ModeSwitchTrigger::User,
    );
    if choice != current_choice {
        change_pinned_mode(agent, &handle.info, choice, root).await;
    }
    if requested_mode.is_some() {
        to_raw_response(&ModeState {
            mode,
            choice: Some(choice),
        })
    } else {
        to_raw_response(&SetSessionUltracodeResponse {
            enabled: mode == ExecutionMode::Ultracode,
        })
    }
}

/// Stores a loaded selection into the session's shared mode.
pub(crate) fn apply_mode(
    target: &distill_tool_types::AtomicExecutionMode,
    (choice, mode): (ExecutionModeChoice, ExecutionMode),
) {
    if choice == ExecutionModeChoice::Auto {
        target.store_auto(mode);
    } else {
        target.store(mode);
    }
}

/// The saved selection and effective mode. Sessions without a saved
/// selection start in Auto.
pub(crate) fn load_mode(
    info: &crate::session::info::Info,
) -> std::io::Result<(ExecutionModeChoice, ExecutionMode)> {
    let dir = crate::session::persistence::session_dir(info);
    let invalid = |error| std::io::Error::new(std::io::ErrorKind::InvalidData, error);
    match std::fs::read(dir.join("execution_mode.json")) {
        Ok(bytes) => serde_json::from_slice::<ModeState>(&bytes)
            .map(|state| (state.choice.unwrap_or(state.mode.into()), state.mode))
            .map_err(invalid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Sessions saved before exclusive modes keep an explicit UltraCode.
            match std::fs::read(dir.join("ultracode.json")) {
                Ok(bytes) => serde_json::from_slice::<SetSessionUltracodeResponse>(&bytes)
                    .map(|state| {
                        if state.enabled {
                            (ExecutionModeChoice::Ultracode, ExecutionMode::Ultracode)
                        } else {
                            (ExecutionModeChoice::Auto, ExecutionMode::Normal)
                        }
                    })
                    .map_err(invalid),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok((ExecutionModeChoice::Auto, ExecutionMode::Normal))
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn persist_mode(
    info: &crate::session::info::Info,
    choice: ExecutionModeChoice,
    mode: ExecutionMode,
) -> std::io::Result<()> {
    let dir = crate::session::persistence::ensure_owner_only_session_dir(info)?;
    let bytes = serde_json::to_vec(&ModeState {
        mode,
        choice: Some(choice),
    })
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::session::storage::write_bytes_atomic(&dir.join("execution_mode.json"), &bytes)
}

const PINNED_MODE_FILE: &str = "pinned_mode.json";

/// What a pinned mode changed in the session, so leaving it can undo that. Absent when the mode
/// changed nothing, which is always the case with no `[modes.*]` key.
#[derive(Default, Serialize, Deserialize)]
struct PinnedModeState {
    /// The session's model before the mode's model; `auto` when account Auto routing chose it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_model: Option<String>,
    /// The mode's model as applied. A different session model on leave is the user's own later
    /// pick, which stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_model: Option<String>,
    /// Decided once when the session is pinned: changing it mid-session would invalidate the
    /// provider prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_tier: Option<distill_sampler::ProviderTier>,
}

fn load_pinned_state(info: &crate::session::info::Info) -> Option<PinnedModeState> {
    let path = crate::session::persistence::session_dir(info).join(PINNED_MODE_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(session_id = %info.id.0, %error, "pinned mode state unreadable");
            return None;
        }
    };
    serde_json::from_slice(&bytes)
        .inspect_err(|error| {
            tracing::warn!(session_id = %info.id.0, %error, "pinned mode state invalid");
        })
        .ok()
}

fn save_pinned_state(
    info: &crate::session::info::Info,
    state: &PinnedModeState,
) -> std::io::Result<()> {
    let path = crate::session::persistence::session_dir(info).join(PINNED_MODE_FILE);
    let changes_nothing = state.previous_model.is_none()
        && state.applied_model.is_none()
        && state.provider_tier.is_none();
    if changes_nothing {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let dir = crate::session::persistence::ensure_owner_only_session_dir(info)?;
    let bytes = serde_json::to_vec(state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::session::storage::write_bytes_atomic(&dir.join(PINNED_MODE_FILE), &bytes)
}

/// The serving path of a session pinned to a mode with `[modes.<mode>] provider_tier`.
pub(crate) fn pinned_provider_tier(
    info: &crate::session::info::Info,
) -> Option<distill_sampler::ProviderTier> {
    load_pinned_state(info)?.provider_tier
}

/// Whether this session was already told UltraSave has no cheaper model.
fn first_no_cheaper_model_notice(session_id: &str) -> bool {
    static NOTIFIED: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::LazyLock::new(Default::default);
    NOTIFIED.lock().insert(session_id.to_owned())
}

/// Leaving a pinned mode restores the model it replaced, unless the user picked another model
/// since; entering one applies its model as a session-scoped explicit pick (account routing
/// state on disk is never written) and decides its serving path. Auto applies nothing. A model
/// that cannot be applied leaves the session on its model.
async fn change_pinned_mode(
    agent: &MvpAgent,
    info: &crate::session::info::Info,
    choice: ExecutionModeChoice,
    root: Option<&toml::Value>,
) {
    let session_model = || {
        agent
            .resident_handle(&info.id)
            .map(|handle| handle.model_id.0.to_string())
    };
    if let Some(left) = load_pinned_state(info)
        && let (Some(previous), Some(applied)) = (left.previous_model, left.applied_model)
        && session_model().as_deref() == Some(applied.as_str())
    {
        switch_session_model(agent, info, &previous).await;
    }
    let mut state = PinnedModeState::default();
    if let Some(mode) = choice.fixed() {
        let settings = crate::agent::config::pinned_mode_settings(root, mode);
        state.provider_tier = settings.provider_tier;
        if mode == ExecutionMode::Ultrasave
            && settings.model.is_none()
            && first_no_cheaper_model_notice(&info.id.0)
        {
            tracing::info!(session_id = %info.id.0,
                "UltraSave has no cheaper model configured; the session keeps its model");
        }
        if let Some(model) = settings.model
            && let Some(current) = session_model().filter(|current| *current != model)
        {
            let previous = if crate::agent::account_routing::session_auto_route(info).is_some() {
                "auto".to_owned()
            } else {
                current
            };
            if switch_session_model(agent, info, &model).await {
                state.previous_model = Some(previous);
                state.applied_model = Some(model);
            }
        }
    }
    if let Err(error) = save_pinned_state(info, &state) {
        tracing::warn!(session_id = %info.id.0, %error, "pinned mode state not saved");
    }
}

async fn switch_session_model(
    agent: &MvpAgent,
    info: &crate::session::info::Info,
    model: &str,
) -> bool {
    let request = acp::SetSessionModelRequest::new(info.id.clone(), acp::ModelId::new(model));
    match Box::pin(agent.set_model_gated(request)).await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(session_id = %info.id.0, model, ?error,
                "pinned mode model not applied; the session keeps its model");
            false
        }
    }
}
