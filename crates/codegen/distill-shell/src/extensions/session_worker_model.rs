// Modified for Distill by Samuel Fajreldines, 2026.
//! Session-scoped worker model and effort for ACP clients (`x.ai/session/worker_model/set`).
//! The choice lives in memory on the session handle and never touches `config.toml`.

use agent_client_protocol as acp;
use distill_sampling_types::ReasoningEffort;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::handle::{SessionWorker, SessionWorkerState};

pub const SET_METHOD: &str = "x.ai/session/worker_model/set";
const MODEL_META_KEY: &str = "workerModelId";
const EFFORT_META_KEY: &str = "workerEffort";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSessionWorkerModelRequest {
    session_id: String,
    /// `null` clears the override, `""` disables the worker, otherwise a catalog model.
    model_id: Option<String>,
    /// `null` or `"auto"` lets Jev pick per call; otherwise a level the model offers.
    effort: Option<String>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EffectiveWorker {
    /// `None` when no worker runs and the main model does all the work.
    pub model_id: Option<String>,
    pub effort: String,
    pub source: &'static str,
}

/// `None` and `"auto"` are auto; anything else must parse as a level.
fn parse_effort(raw: Option<&str>) -> Result<Option<ReasoningEffort>, acp::Error> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) if value.eq_ignore_ascii_case("auto") => Ok(None),
        Some(value) => value
            .parse()
            .map(Some)
            .map_err(|err: String| acp::Error::invalid_params().data(err)),
    }
}

/// Reads `_meta.workerModelId` / `_meta.workerEffort` from `session/new|load|resume`.
/// Absent model id inherits the config (effort is then ignored); the catalog is not
/// consulted, so an unavailable id falls back to the parent model when a child spawns.
pub(crate) fn worker_from_meta(
    meta: Option<&acp::Meta>,
) -> Result<Option<SessionWorker>, acp::Error> {
    let Some(meta) = meta else {
        return Ok(None);
    };
    let model_id = match meta.get(MODEL_META_KEY) {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(id)) => id.trim().to_owned(),
        Some(_) => {
            return Err(acp::Error::invalid_params()
                .data(format!("_meta.{MODEL_META_KEY} must be a string")));
        }
    };
    if model_id.is_empty() {
        return Ok(Some(SessionWorker {
            model_id: None,
            effort: None,
        }));
    }
    let effort = match meta.get(EFFORT_META_KEY) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(level)) => parse_effort(Some(level))?,
        Some(_) => {
            return Err(acp::Error::invalid_params()
                .data(format!("_meta.{EFFORT_META_KEY} must be a string")));
        }
    };
    Ok(Some(SessionWorker {
        model_id: Some(model_id),
        effort,
    }))
}

/// An explicit worker (including disable) survives Auto delegation until Auto is selected again.
pub(crate) fn pin_from_meta(
    headers: &mut indexmap::IndexMap<String, String>,
    meta: Option<&acp::Meta>,
    requested_auto: bool,
) {
    if requested_auto {
        headers.shift_remove("x-distill-worker-pin");
    } else if meta
        .and_then(|meta| meta.get(MODEL_META_KEY))
        .is_some_and(|id| id.is_string())
    {
        headers.insert("x-distill-worker-pin".into(), "1".into());
    }
}

/// Snapshot the configured worker when a session starts.
pub(crate) fn config_worker_snapshot() -> SessionWorker {
    let model_id = crate::jev::worker_model();
    let effort = model_id.as_ref().and_then(|_| crate::jev::worker_effort());
    SessionWorker { model_id, effort }
}

/// Apply session metadata or initialize an unconfigured session from its start-time snapshot.
pub(crate) fn apply_meta_or_snapshot(
    state: &SessionWorkerState,
    meta_worker: Option<SessionWorker>,
    snapshot: SessionWorker,
) {
    let mut state = state.write();
    if let Some(worker) = meta_worker {
        *state = Some(worker);
    } else if state.is_none() {
        *state = Some(snapshot);
    }
}

/// The worker a model-delegated child would get right now.
pub(crate) fn effective_worker(state: &SessionWorkerState) -> EffectiveWorker {
    let effort_label = |effort: Option<ReasoningEffort>| {
        effort.map_or_else(|| "auto".to_owned(), |level| level.to_string())
    };
    match state.read().clone() {
        Some(SessionWorker { model_id: None, .. }) => EffectiveWorker {
            model_id: None,
            effort: "auto".to_owned(),
            source: "session",
        },
        Some(SessionWorker { model_id, effort }) => EffectiveWorker {
            model_id,
            effort: effort_label(effort),
            source: "session",
        },
        None => {
            let model_id = crate::jev::worker_model();
            let effort = if model_id.is_some() {
                effort_label(crate::jev::worker_effort())
            } else {
                "auto".to_owned()
            };
            EffectiveWorker {
                model_id,
                effort,
                source: "config",
            }
        }
    }
}

pub(crate) fn insert_worker_meta(meta: &mut acp::Meta, state: &SessionWorkerState) {
    let worker = effective_worker(state);
    meta.insert(MODEL_META_KEY.into(), worker.model_id.into());
    meta.insert(EFFORT_META_KEY.into(), worker.effort.into());
}

/// The worker the session's main prompt names in `<environment>` for `<orchestration>`: the
/// worker its delegated children run on, kept only for a primary prompt with subagents enabled,
/// a catalog model, and not the catalog entry of the session's own main model.
pub(crate) fn orchestration_worker(
    state: &SessionWorkerState,
    models_manager: &crate::agent::remote_config::ModelsManager,
    session_model: &str,
    prompt_audience: distill_agent::prompt::context::PromptAudience,
    subagents_enabled: bool,
) -> Option<String> {
    if prompt_audience != distill_agent::prompt::context::PromptAudience::Primary
        || !subagents_enabled
    {
        return None;
    }
    let session_choice = state.read().as_ref().map(|worker| worker.model_id.clone());
    let worker = session_choice.unwrap_or_else(crate::jev::worker_model)?;
    let models = models_manager.models();
    let worker_entry = crate::agent::config::find_model_by_id(&models, &worker)?;
    let is_session_model = crate::agent::config::find_model_by_id(&models, session_model)
        .is_some_and(|main| std::ptr::eq(main, worker_entry));
    (!is_session_model).then_some(worker)
}

pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if args.method.as_ref() != SET_METHOD {
        return Err(acp::Error::method_not_found());
    }
    let request: SetSessionWorkerModelRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(request.session_id.as_str());
    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            request.session_id
        ))));
    };
    let next = validated_worker(
        &agent.models_manager,
        request.model_id.as_deref(),
        request.effort.as_deref(),
    )?;
    *handle.worker_override.write() = next;
    if let Some(mut config) = handle.chat_state_handle.get_sampling_config().await {
        if request.model_id.is_some() {
            config
                .extra_headers
                .insert("x-distill-worker-pin".into(), "1".into());
        } else {
            config.extra_headers.shift_remove("x-distill-worker-pin");
        }
        handle.chat_state_handle.update_sampling_config(config);
    }
    let _ = handle
        .cmd_tx
        .send(crate::session::SessionCommand::RefreshWorkerPrompt);
    to_raw_response(&effective_worker(&handle.worker_override))
}

/// `Ok(None)` clears the override; validation failures leave the session untouched.
fn validated_worker(
    models_manager: &crate::agent::remote_config::ModelsManager,
    model_id: Option<&str>,
    effort: Option<&str>,
) -> Result<Option<SessionWorker>, acp::Error> {
    let effort = parse_effort(effort)?;
    let Some(model_id) = model_id.map(str::trim) else {
        return Ok(None);
    };
    if model_id.is_empty() {
        return Ok(Some(SessionWorker {
            model_id: None,
            effort: None,
        }));
    }
    let models = models_manager.models();
    if !crate::agent::config::find_model_by_id(&models, model_id)
        .is_some_and(|entry| entry.info.user_selectable)
    {
        return Err(acp::Error::invalid_params().data(format!(
            "worker model `{model_id}` is not an available model"
        )));
    }
    if let Some(level) = effort
        && !models_manager.model_supports_reasoning_effort_value(model_id, level)
    {
        return Err(acp::Error::invalid_params().data(format!(
            "worker model `{model_id}` does not offer effort `{level}`"
        )));
    }
    Ok(Some(SessionWorker {
        model_id: Some(model_id.to_owned()),
        effort,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::handle::new_session_worker_state;

    #[test]
    fn worker_metadata_tracks_session_and_preserves_other_metadata() {
        let state = new_session_worker_state();
        let mut meta = acp::Meta::new();
        meta.insert("autoRoute".into(), serde_json::json!({"model": "main"}));
        for (model_id, effort, expected_effort) in [
            (Some("worker-a"), None, "auto"),
            (Some("worker-b"), Some("high".parse().unwrap()), "high"),
            (None, None, "auto"),
        ] {
            *state.write() = Some(SessionWorker {
                model_id: model_id.map(str::to_owned),
                effort,
            });
            insert_worker_meta(&mut meta, &state);
            assert_eq!(meta[MODEL_META_KEY], serde_json::json!(model_id));
            assert_eq!(meta[EFFORT_META_KEY], serde_json::json!(expected_effort));
            assert_eq!(meta["autoRoute"], serde_json::json!({"model": "main"}));
        }
    }
}
