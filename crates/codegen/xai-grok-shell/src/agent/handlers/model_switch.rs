//! Applies a model switch to a session, the ungated path.
//! `set_session_model` enforces the `allowed_models` gate before delegating here.
//! Internal callers (`new_session`, `load_session`) call `apply` directly.
use crate::agent::config;
use crate::agent::mvp_agent::{
    MvpAgent, agent_name_after_model_switch, harnesses_are_compatible, resolve_required_agent_type,
};
use crate::sampling::EffortTarget;
use crate::session::{SessionCommand, SessionModelSwitch};
use agent_client_protocol::{self as acp};
use tokio::sync::oneshot;
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_sampling_types::{parse_reasoning_effort_auto_meta, parse_reasoning_effort_meta};
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigNotice {
    Send,
    Skip,
}
/// How a model switch resolves the session's reasoning effort.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchEffort {
    /// Keep the session's current effort, revalidated for the new model (a plain user switch).
    Preserve,
    /// Use this effort; `None` falls back to the new model's default (session setup and restore).
    Set(Option<ReasoningEffort>),
}
/// Apply a model switch to a session (no gate; `set_session_model` gates first).
pub(crate) async fn apply(
    agent: &MvpAgent,
    args: acp::SetSessionModelRequest,
    effort: SwitchEffort,
    config_notice: ConfigNotice,
) -> Result<acp::SetSessionModelResponse, acp::Error> {
    tracing::info!("Received set session model request {args:?}");
    xai_grok_telemetry::unified_log::info(
        "model changed",
        Some(args.session_id.0.as_ref()),
        Some(serde_json::json!({"model": args.model_id.0.as_ref()})),
    );
    tracing::debug!("session_session_model::mvp_agent: {:?}", &args);
    // Auto effort (`/effort auto`) rides the same request as an explicit meta
    // flag: it asks the decision layer to pick the effort for each model call.
    // An explicit level in the same request (or a later switch) turns it back
    // off, so the request's own effort stays the fallback.
    let wants_auto_effort = parse_reasoning_effort_auto_meta(args.meta.as_ref());
    let explicit_effort = parse_reasoning_effort_meta(args.meta.as_ref());
    let acp::SetSessionModelRequest {
        session_id,
        model_id,
        ..
    } = args;
    let handle = agent
        .session_handle_waiting_for_load(&session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
    let _config_guard = agent.config_mutation_lock(&session_id).lock_owned().await;
    let handle = agent.resident_handle(&session_id).unwrap_or(handle);
    let model = agent.resolve_model_id(&model_id)?;
    let use_concise = model.info().use_concise;
    let session_default = handle
        .session_default_agent_profile
        .as_deref()
        .unwrap_or(&handle.agent_name);
    let required_agent_type =
        resolve_required_agent_type(Some(model.info().agent_type.as_str()), session_default);
    let previous_model_id = handle.model_id.0.clone();
    let is_family_switch = {
        let models = agent.models_manager.models();
        let old_family = config::find_model_by_id(&models, &previous_model_id)
            .and_then(|e| e.info.model_family.as_deref());
        matches!(
            (old_family, model.info().model_family.as_deref()),
            (Some(a), Some(b)) if a != b
        )
    };
    let mut pending_rebuild_definition: Option<xai_grok_agent::AgentDefinition> = None;
    {
        let required = &required_agent_type;
        let turn_count = handle
            .signals_handle
            .snapshot()
            .await
            .map(|s| s.turn_count)
            .unwrap_or(0);
        let (agent_tx, agent_rx) = oneshot::channel();
        let _ = handle.cmd_tx.send(SessionCommand::GetActiveAgent {
            responds_to: agent_tx,
        });
        let active_agent_type = agent_rx.await.ok().flatten();
        let is_mismatch = active_agent_type
            .as_ref()
            .is_some_and(|active| !harnesses_are_compatible(active, required));
        tracing::info!(
            session_id = %session_id.0,
            model_id = %model_id.0,
            ?required_agent_type,
            ?active_agent_type,
            turn_count,
            is_mismatch,
            "set_session_model: agent type compatibility check"
        );
        if is_mismatch && turn_count > 0 {
            tracing::warn!(
                session_id = %session_id.0,
                model_id = %model_id.0,
                active_agent = ?active_agent_type,
                required_agent = %required,
                turn_count,
                "set_session_model: agent type mismatch rejected"
            );
            xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ModelSwitched {
                session_id: session_id.0.to_string(),
                previous_model_id: previous_model_id.to_string(),
                new_model_id: model_id.0.to_string(),
                success: false,
                error_code: Some(config::MODEL_SWITCH_INCOMPATIBLE_AGENT.to_string()),
                required_agent_type: Some(required.clone()),
                current_agent_type: active_agent_type.clone(),
            });
            let err_payload = config::ModelSwitchIncompatibleAgentError {
                code: config::MODEL_SWITCH_INCOMPATIBLE_AGENT.to_string(),
                active_agent_type: active_agent_type.unwrap_or_else(|| "unknown".to_owned()),
                required_agent_type: required.clone(),
                model_id: model_id.0.to_string(),
                suggestion: "start_new_session".to_string(),
            };
            return Err(err_payload.into_acp_error());
        }
        if is_mismatch && turn_count == 0 {
            let cwd = handle.tool_context.cwd.as_path();
            let resolved = xai_grok_agent::discovery::by_name_in_cwd_with_plugins(
                required,
                cwd,
                agent.plugin_registry_handle.snapshot().as_deref(),
            );
            match resolved {
                Some(def) => {
                    tracing::info!(
                        session_id = %session_id.0,
                        model_id = %model_id.0,
                        required_agent_type = %required,
                        agent_def_name = %def.name,
                        "set_session_model: zero-turn harness switch — queued agent rebuild"
                    );
                    pending_rebuild_definition = Some(def);
                }
                None => {
                    tracing::warn!(
                        session_id = %session_id.0,
                        model_id = %model_id.0,
                        required_agent_type = %required,
                        "set_session_model: zero-turn harness switch — could not resolve agent definition; proceeding with stale harness"
                    );
                }
            }
        }
    }
    let effective_effort = match effort {
        SwitchEffort::Set(explicit) => explicit,
        SwitchEffort::Preserve => handle
            .chat_state_handle
            .get_sampling_config()
            .await
            .and_then(|cfg| cfg.reasoning_effort),
    };
    // Auto effort is a session mode, not a level: an explicit level in this
    // request (or any later one) clears it, and asking for auto keeps whatever
    // level the session already had as the fallback.
    if wants_auto_effort && explicit_effort.is_none() {
        agent.models_manager.set_current_effort_auto(true);
        tracing::info!(
            session_id = %session_id.0,
            model_id = %model_id.0,
            fallback_effort = ?effective_effort,
            "set_session_model: auto effort enabled; the decision layer picks the effort per model call"
        );
    } else if explicit_effort.is_some() {
        agent.models_manager.set_current_effort_auto(false);
    }
    let mut model_sampling =
        agent.prepare_sampling_config_for_model(&model, handle.origin_client.clone());
    agent.models_manager.apply_supported_effort(
        &mut model_sampling,
        effective_effort,
        &session_id,
        EffortTarget::ModelSwitch,
    );
    let applied_effort = model_sampling.reasoning_effort;
    let (new_threshold, system_prompt_label) = {
        let cfg = agent.cfg.borrow();
        (
            crate::util::config::resolve_auto_compact_threshold_percent(
                &cfg,
                model_sampling.model.as_str(),
                Some(model.info()),
            ),
            crate::util::config::resolve_system_prompt_label(
                &cfg,
                model_id.0.as_ref(),
                Some(model.info()),
            ),
        )
    };
    let gate_closed = !handle
        .gateway_enabled
        .load(std::sync::atomic::Ordering::Relaxed);
    let apply_prompt_override = !gate_closed;
    if gate_closed {
        tracing::info!(
            session_id = %session_id.0,
            model_id = %model_id.0,
            "set_session_model: gateway gate closed, prompt override suppressed"
        );
        pending_rebuild_definition = None;
    }
    let did_rebuild = if let Some(def) = pending_rebuild_definition {
        let (rebuild_tx, rebuild_rx) = oneshot::channel();
        let _ = handle
            .cmd_tx
            .send(SessionCommand::RebuildAgentForDefinition {
                definition: def,
                system_prompt_label: system_prompt_label.clone(),
                responds_to: rebuild_tx,
            });
        let rebuild_result = rebuild_rx
            .await
            .map_err(|_| acp::Error::internal_error().data("rebuild_agent: actor closed"))?;
        match rebuild_result {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    session_id = %session_id.0,
                    model_id = %model_id.0,
                    error = ?e,
                    "set_session_model: zero-turn harness rebuild failed; aborting model switch"
                );
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::events::ModelSwitched {
                        session_id: session_id.0.to_string(),
                        previous_model_id: previous_model_id.to_string(),
                        new_model_id: model_id.0.to_string(),
                        success: false,
                        error_code: Some(config::MODEL_SWITCH_REBUILD_FAILED.to_string()),
                        required_agent_type: Some(required_agent_type.clone()),
                        current_agent_type: None,
                    },
                );
                return Err(e);
            }
        }
    } else {
        false
    };
    let model_unchanged = previous_model_id == model_id.0;
    let (tx, rx) = oneshot::channel();
    let _ = handle.cmd_tx.send(SessionCommand::SetSessionModel {
        switch: SessionModelSwitch {
            sampling_config: model_sampling,
            use_concise,
            is_family_switch,
            apply_prompt_override,
            skip_prompt_rewrite: did_rebuild || model_unchanged,
            auto_compact_threshold_percent: new_threshold,
            system_prompt_label,
        },
        responds_to: tx,
    });
    let updated_model = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("failed to set session model"))?;
    agent.with_resident_mut(&session_id, |handle| {
        handle.model_id = model_id.clone();
        handle.reasoning_effort = applied_effort;
        handle.agent_name =
            agent_name_after_model_switch(did_rebuild, &required_agent_type, &handle.agent_name);
    });
    notify_model_changed(
        agent,
        &session_id,
        model_id.0.as_ref(),
        applied_effort.map(|eff| eff.to_string()),
    );
    if config_notice == ConfigNotice::Send {
        notify_config_options(agent, &session_id).await;
    }
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ModelSwitched {
        session_id: session_id.0.to_string(),
        previous_model_id: previous_model_id.to_string(),
        new_model_id: model_id.0.to_string(),
        success: true,
        error_code: None,
        required_agent_type: Some(required_agent_type.clone()),
        current_agent_type: None,
    });
    if agent.cfg.borrow().mode != config::AgentMode::Leader {
        agent.models_manager.set_current_model_id(model_id.clone());
        agent
            .models_manager
            .set_current_reasoning_effort(applied_effort);
    }
    agent.sync_process_static_api_key(Some(model_id.0.as_ref()));
    Ok(acp::SetSessionModelResponse::new().meta(
        serde_json::json!({
            "model": updated_model,
        })
        .as_object()
        .cloned(),
    ))
}
/// Apply a reasoning-effort change to a session's current model, without a model switch (see [`SessionCommand::SetReasoningEffort`]).
/// `value_id` is the selector the client picked; it is resolved under the config lock so the level is validated against the model the effort will actually run on, and the command goes to that model's live actor even if a switch or reload landed while this request was blocked.
pub(crate) async fn apply_reasoning_effort(
    agent: &MvpAgent,
    session_id: acp::SessionId,
    value_id: &str,
    config_notice: ConfigNotice,
) -> Result<acp::SetSessionModelResponse, acp::Error> {
    let handle = agent
        .session_handle_waiting_for_load(&session_id)
        .await
        .ok_or_else(|| acp::Error::invalid_params().data("unknown session id"))?;
    let _config_guard = agent.config_mutation_lock(&session_id).lock_owned().await;
    let handle = agent.resident_handle(&session_id).unwrap_or(handle);
    let model_id = handle.model_id.clone();
    let effort = agent
        .resolve_reasoning_effort_value(&session_id, &model_id, value_id)
        .ok_or_else(|| acp::Error::invalid_params().data("unknown reasoning_effort value"))?;
    let (tx, rx) = oneshot::channel();
    let _ = handle.cmd_tx.send(SessionCommand::SetReasoningEffort {
        effort,
        responds_to: tx,
    });
    rx.await
        .map_err(|_| acp::Error::internal_error().data("failed to set reasoning effort"))??;
    agent.with_resident_mut(&session_id, |handle| {
        handle.reasoning_effort = Some(effort);
    });
    notify_model_changed(
        agent,
        &session_id,
        model_id.0.as_ref(),
        Some(effort.to_string()),
    );
    if config_notice == ConfigNotice::Send {
        notify_config_options(agent, &session_id).await;
    }
    if agent.cfg.borrow().mode != config::AgentMode::Leader {
        agent
            .models_manager
            .set_current_reasoning_effort(Some(effort));
    }
    Ok(acp::SetSessionModelResponse::new().meta(
        serde_json::json!({ "model" : model_id.0.as_ref() })
            .as_object()
            .cloned(),
    ))
}
fn notify_model_changed(
    agent: &MvpAgent,
    session_id: &acp::SessionId,
    model_id: &str,
    reasoning_effort: Option<String>,
) {
    let notification = crate::extensions::notification::SessionNotification {
        session_id: session_id.clone(),
        update: crate::extensions::notification::SessionUpdate::ModelChanged {
            model_id: model_id.to_owned(),
            reasoning_effort,
        },
        meta: None,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&notification) {
        agent
            .gateway
            .forward_fire_and_forget(acp::ExtNotification::new(
                "x.ai/session_notification",
                params.into(),
            ));
    }
}
async fn notify_config_options(agent: &MvpAgent, session_id: &acp::SessionId) {
    let options = agent.acp_config_options_for_session(session_id).await;
    agent
        .gateway
        .forward_fire_and_forget(acp::SessionNotification::new(
            session_id.clone(),
            acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(options)),
        ));
}
