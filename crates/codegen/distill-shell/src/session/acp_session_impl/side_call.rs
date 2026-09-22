// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared cache-aligned request building for recap-style auxiliary model calls (recap, turn summary).
//! `/btw` reuses the request skeleton.

use super::*;

use crate::remote::DEFAULT_CONTEXT_WINDOW;

#[derive(Debug, PartialEq)]
struct PromptCacheUsage {
    prompt_tokens: u32,
    cached_prompt_tokens: u32,
    cache_creation_prompt_tokens: u32,
    uncached_prompt_tokens: u32,
    cache_read_rate: f64,
    cache_write_rate: f64,
}

impl From<&distill_sampling_types::TokenUsage> for PromptCacheUsage {
    fn from(usage: &distill_sampling_types::TokenUsage) -> Self {
        let prompt_tokens = usage.prompt_tokens;
        let uncached_prompt_tokens = prompt_tokens
            .saturating_sub(usage.cached_prompt_tokens)
            .saturating_sub(usage.cache_creation_prompt_tokens);
        let rate = |tokens| {
            if prompt_tokens == 0 {
                0.0
            } else {
                (f64::from(tokens) / f64::from(prompt_tokens) * 1_000.0).round() / 1_000.0
            }
        };
        Self {
            prompt_tokens,
            cached_prompt_tokens: usage.cached_prompt_tokens,
            cache_creation_prompt_tokens: usage.cache_creation_prompt_tokens,
            uncached_prompt_tokens,
            cache_read_rate: rate(usage.cached_prompt_tokens),
            cache_write_rate: rate(usage.cache_creation_prompt_tokens),
        }
    }
}

/// Logs the provider-reported prompt cache buckets for one auxiliary call.
pub(crate) fn log_prompt_cache_usage(
    call: &str,
    backend: crate::sampling::ApiBackend,
    response: &distill_sampling_types::ConversationResponse,
) {
    let Some(usage) = response.usage.as_ref() else {
        return;
    };
    let usage = PromptCacheUsage::from(usage);
    tracing::info!(
        call,
        backend = ?backend,
        prompt_tokens = usage.prompt_tokens,
        cached_prompt_tokens = usage.cached_prompt_tokens,
        cache_creation_prompt_tokens = usage.cache_creation_prompt_tokens,
        uncached_prompt_tokens = usage.uncached_prompt_tokens,
        cache_read_rate = usage.cache_read_rate,
        cache_write_rate = usage.cache_write_rate,
        cache_key_forwarded = backend.forwards_prompt_cache_key(),
        "auxiliary call prompt cache usage"
    );
}

/// Attribute one completed auxiliary response exactly once, even when its
/// text is empty or later discarded as stale. A missing provider usage stays a
/// counted, incomplete call in the shared UsageLedger.
pub(crate) fn record_auxiliary_response(
    actor: &SessionActor,
    call: &str,
    configured_model: &str,
    response: &distill_sampling_types::ConversationResponse,
    api_duration_ms: Option<u64>,
    attribute_to_prompt: bool,
) {
    let model_id = response
        .assistant()
        .and_then(|assistant| assistant.model_id.clone())
        .filter(|model| !model.is_empty())
        .or_else(|| (!configured_model.is_empty()).then(|| configured_model.to_owned()));
    actor.chat_state_handle.record_auxiliary_call_usage(
        model_id,
        response.usage.clone(),
        api_duration_ms,
        response.cost_usd_ticks,
        attribute_to_prompt,
        false,
    );
    tracing::debug!(
        call,
        usage_reported = response.usage.is_some(),
        cost_reported = response.cost_usd_ticks.is_some(),
        "recorded auxiliary model response"
    );
}

/// Record attempts for which the transport returned no provider usage. This
/// is deliberately separate from a successful response: the ledger reports
/// the attempt and its missing cost instead of manufacturing a zero.
pub(crate) fn record_auxiliary_failures(
    actor: &SessionActor,
    configured_model: &str,
    attempts: u32,
    attribute_to_prompt: bool,
) {
    let model_id = (!configured_model.is_empty()).then(|| configured_model.to_owned());
    for _ in 0..attempts {
        actor.chat_state_handle.record_auxiliary_call_usage(
            model_id.clone(),
            None,
            None,
            None,
            attribute_to_prompt,
            true,
        );
    }
}

/// What differs between the two calls that reuse the parent's prompt cache.
/// The shared parts live in [`SessionActor::parent_cached_request`].
pub(crate) struct AuxCall {
    pub(crate) items: Vec<ConversationItem>,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) hosted_tools: Vec<distill_sampling_types::HostedTool>,
    pub(crate) model: String,
    /// Must match the main turn's, or the prompt differs before the conversation history even starts.
    pub(crate) reasoning_effort: Option<distill_sampling_types::ReasoningEffort>,
    /// Says whether the cache key gets sent, which is what decides the conv id below.
    pub(crate) backend: crate::sampling::ApiBackend,
    pub(crate) conv_id: String,
    pub(crate) req_id: String,
}

/// Shared setup for a recap-style side-call; see [`SessionActor::prepare_side_call`].
pub(crate) struct SideCallSetup {
    pub(crate) client: distill_sampler::SamplingClient,
    pub(crate) strip_reasoning: bool,
    pub(crate) context_window: u64,
    pub(crate) model: String,
    /// Must match the main turn so the side-call shares the prompt-cache prefix.
    pub(crate) reasoning_effort: Option<distill_sampling_types::ReasoningEffort>,
}

pub(super) fn should_strip_side_call_reasoning(
    backend: crate::sampling::ApiBackend,
    reasoning_effort: Option<distill_sampling_types::ReasoningEffort>,
) -> bool {
    matches!(backend, crate::sampling::ApiBackend::Messages)
        && reasoning_effort
            .and_then(|effort| effort.to_messages_api())
            .is_none()
}

impl SessionActor {
    /// Request skeleton for an auxiliary call that replays the parent conversation under the parent's `prompt_cache_key`.
    /// Temperature stays unset: cli-chat-proxy may inject a `thinking` config, and the Messages API then requires temperature == 1.
    pub(crate) fn parent_cached_request(&self, call: AuxCall) -> ConversationRequest {
        let session_id = self.session_info.id.to_string();
        // Only the Responses mapping sends the cache key
        // On the other backends the conv id is what ties a call to its conversation, so it has to stay the parent session id
        // The `btw-`/`recap-` label still shows up in `x_grok_req_id`
        let conv_id = if call.backend.forwards_prompt_cache_key() {
            call.conv_id
        } else {
            session_id.clone()
        };
        ConversationRequest {
            items: distill_chat_state::compaction_utils::ModelRequestHistory::from_raw(call.items)
                .into_items(),
            tools: call.tools,
            hosted_tools: call.hosted_tools,
            model: Some(call.model),
            temperature: None,
            // Effort changes the prompt ahead of the conversation history, so dropping it here would share no prefix with the main turn.
            reasoning_effort: call.reasoning_effort,
            x_grok_conv_id: Some(conv_id),
            x_grok_req_id: Some(call.req_id),
            x_grok_session_id: Some(session_id.clone()),
            x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
            prompt_cache_key: Some(session_id),
            // Side calls persist text and never execute tools (the attached tools only align the prompt-cache prefix)
            // A Length sample must fail rather than salvage
            length_policy: distill_sampling_types::LengthPolicy::Fail,
            ..Default::default()
        }
    }

    /// Prepare the shared pieces of a recap-style side-call (recap and turn summary): the sampling client and the config both need.
    /// Recap-style side-calls preserve reasoning so their conversation prefix stays byte-identical to the parent turn.
    /// Messages strips reasoning only when the matching effort cannot emit a top-level thinking configuration.
    pub(crate) async fn prepare_side_call(&self) -> Result<SideCallSetup, acp::Error> {
        let client = self.prepare_chat_completion(false).await?;
        // One config read serves the window, model, and reasoning effort.
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let context_window = sampling_config
            .as_ref()
            .map(|c| c.context_window.get())
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        let reasoning_effort = sampling_config.as_ref().and_then(|c| c.reasoning_effort);
        let strip_reasoning =
            should_strip_side_call_reasoning(client.api_backend(), reasoning_effort);
        let model = sampling_config.map(|c| c.model).unwrap_or_default();
        Ok(SideCallSetup {
            client,
            strip_reasoning,
            context_window,
            model,
            reasoning_effort,
        })
    }

    /// Build the cache-aligned request for a recap-style side-call via [`Self::parent_cached_request`].
    /// Uses the main turn's tool and hosted-tool specs and matching reasoning effort so the prompt-cache prefix stays warm.
    /// The instructions keep outputs short and the clean helpers cap length as a safety net, so an explicit token cap isn't needed.
    pub(crate) async fn side_call_request(
        &self,
        setup: &SideCallSetup,
        items: Vec<ConversationItem>,
        x_grok_conv_id: String,
        x_grok_req_id: String,
    ) -> ConversationRequest {
        let tool_defs = self.prepare_tool_definitions().await;
        let tools = self.turn_base_tool_specs(&tool_defs);
        // Mirror the main turn's hosted tools (overrides folded in) so a side-call can't search past the active cutoff.
        let hosted_tools = self.hosted_tools_for_turn();
        self.parent_cached_request(AuxCall {
            items,
            tools,
            hosted_tools,
            model: setup.model.clone(),
            reasoning_effort: setup.reasoning_effort,
            backend: setup.client.api_backend(),
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
        })
    }

    /// Invalidate in-flight recap-style side-calls when a real user prompt is accepted (at queue time or turn start).
    /// Bumps the recap epoch so a finishing recap cannot commit, and aborts an in-flight turn summary.
    /// Idempotent under the queue-accept and turn-start double bump.
    pub(crate) fn invalidate_side_calls_for_new_prompt(&self) {
        self.recap_epoch.set(self.recap_epoch.get().wrapping_add(1));
        self.abort_turn_summary();
        // The title refresh is deliberately NOT aborted here.
        // It describes the whole conversation, so completing against the pre-prompt snapshot is still valid.
        // Aborting on every prompt would leave the checkpoint unconsumed and re-spawn a call each turn.
    }
}

#[cfg(test)]
mod tests {
    use super::PromptCacheUsage;
    use distill_sampling_types::TokenUsage;

    #[test]
    fn prompt_cache_usage_projects_provider_buckets_and_rates() {
        let usage = PromptCacheUsage::from(&TokenUsage {
            prompt_tokens: 1_000,
            cached_prompt_tokens: 700,
            cache_creation_prompt_tokens: 200,
            ..Default::default()
        });

        assert_eq!(usage.prompt_tokens, 1_000);
        assert_eq!(usage.cached_prompt_tokens, 700);
        assert_eq!(usage.cache_creation_prompt_tokens, 200);
        assert_eq!(usage.uncached_prompt_tokens, 100);
        assert_eq!(usage.cache_read_rate, 0.7);
        assert_eq!(usage.cache_write_rate, 0.2);

        let rounded = PromptCacheUsage::from(&TokenUsage {
            prompt_tokens: 144_860,
            cached_prompt_tokens: 141_663,
            cache_creation_prompt_tokens: 3_195,
            ..Default::default()
        });
        assert_eq!(rounded.cache_read_rate, 0.978);
        assert_eq!(rounded.cache_write_rate, 0.022);
    }

    #[test]
    fn prompt_cache_usage_saturates_invalid_buckets_and_zero_rates() {
        let usage = PromptCacheUsage::from(&TokenUsage {
            prompt_tokens: 0,
            cached_prompt_tokens: 10,
            cache_creation_prompt_tokens: 20,
            ..Default::default()
        });

        assert_eq!(usage.uncached_prompt_tokens, 0);
        assert_eq!(usage.cache_read_rate, 0.0);
        assert_eq!(usage.cache_write_rate, 0.0);
    }
}
