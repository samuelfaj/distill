// Modified for Distill by Samuel Fajreldines, 2026.
//! Applies a reasoning-effort hint only when the model supports it; shared by session creation, model switch, and the summary client.

use agent_client_protocol as acp;
use distill_sampler::SamplerConfig;
use distill_sampling_types::ReasoningEffort;

use crate::agent::remote_config::ModelsManager;
use crate::sampling::EffortTarget;

impl ModelsManager {
    pub(crate) fn apply_supported_effort(
        &self,
        sampling: &mut SamplerConfig,
        effort: Option<ReasoningEffort>,
        session_id: &acp::SessionId,
        target: EffortTarget,
    ) {
        let Some(effort) = effort else {
            return;
        };
        if !self.model_supports_reasoning_effort(&sampling.model) {
            // SummaryClient stays quiet; the spawn or switch that carried this effort already warned that the model does not support it
            if matches!(target, EffortTarget::NewSession | EffortTarget::ModelSwitch) {
                tracing::warn!(
                    session_id = %session_id.0,
                    model = %sampling.model,
                    effort = %effort,
                    "reasoning_effort: model does not support effort; ignoring it"
                );
            }
            return;
        }
        // Some models are a different model id at each effort, so swap in the id this effort asks for.
        // Do this before the log, or the log records an id we are not sending.
        if let Some(routed) = self.model_for_effort(&sampling.model, effort) {
            sampling.model = routed;
        }
        // Same fields at every target; only the level differs
        // tracing bakes the level into a static callsite, so match a const level per arm
        macro_rules! log_applied {
            ($level:expr) => {
                tracing::event!(
                    $level,
                    session_id = %session_id.0,
                    model = %sampling.model,
                    effort = %effort,
                    target = %target.as_ref(),
                    "reasoning_effort: applied effort"
                )
            };
        }
        match target {
            EffortTarget::NewSession | EffortTarget::ModelSwitch => {
                log_applied!(tracing::Level::INFO)
            }
            EffortTarget::SummaryClient => log_applied!(tracing::Level::DEBUG),
        }
        sampling.reasoning_effort = Some(effort);
    }
}

/// At most one variant carries the hint, so the spawn and switch consumers can never both fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NewSessionEffort {
    /// Seed the spawned session's sampling config (default-model path).
    Spawn(ReasoningEffort),
    /// Apply after spawn through the model switch (explicit `modelId` path).
    Switch(ReasoningEffort),
    None,
}

/// Precedence: an explicit `_meta.reasoningEffort` wins over the process-wide last-used or `[models].default_reasoning_effort` value.
/// The catalog default is the last resort and is left on the sampling config when this returns `None`.
pub(crate) fn resolve_new_session_effort_hint(
    meta_hint: Option<ReasoningEffort>,
    current: Option<ReasoningEffort>,
) -> Option<ReasoningEffort> {
    meta_hint.or(current)
}

pub(crate) fn split_new_session_effort(
    resolved_custom_model: Option<&str>,
    hint: Option<ReasoningEffort>,
) -> NewSessionEffort {
    match hint {
        None => NewSessionEffort::None,
        Some(effort) if resolved_custom_model.is_some() => NewSessionEffort::Switch(effort),
        Some(effort) => NewSessionEffort::Spawn(effort),
    }
}

/// Main-model choices a client can pass in `session/new|load|resume` `_meta`.
/// They apply to that one session; nothing here touches process-wide defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionMainMeta {
    pub model_id: Option<String>,
    pub effort: Option<ReasoningEffort>,
    /// `_meta.reasoningEffortAuto`; `None` when the client did not say.
    pub auto: Option<bool>,
}

impl SessionMainMeta {
    pub(crate) fn from_meta(meta: Option<&acp::Meta>) -> Self {
        Self {
            model_id: meta
                .and_then(|m| m.get("modelId"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
            effort: distill_sampling_types::parse_reasoning_effort_meta(meta),
            auto: meta
                .and_then(|m| m.get(distill_sampling_types::REASONING_EFFORT_AUTO_META_KEY))
                .and_then(serde_json::Value::as_bool),
        }
    }

    /// An explicit level wins over auto; otherwise the client's flag, else `default`.
    pub(crate) fn effort_auto(&self, default: bool) -> bool {
        if self.effort.is_some() {
            false
        } else {
            self.auto.unwrap_or(default)
        }
    }

    /// The effort fields as `session/set_model` `_meta`, for applying them through the switch path.
    pub(crate) fn switch_meta(&self) -> Option<acp::Meta> {
        let mut meta = acp::Meta::new();
        if let Some(effort) = self.effort {
            meta.insert(
                distill_sampling_types::REASONING_EFFORT_META_KEY.to_owned(),
                distill_sampling_types::reasoning_effort_meta_value(effort),
            );
        }
        if let Some(auto) = self.auto {
            meta.insert(
                distill_sampling_types::REASONING_EFFORT_AUTO_META_KEY.to_owned(),
                serde_json::Value::Bool(auto),
            );
        }
        (!meta.is_empty()).then_some(meta)
    }
}
