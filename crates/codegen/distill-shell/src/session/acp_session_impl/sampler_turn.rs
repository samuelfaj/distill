// Modified for Distill by Samuel Fajreldines, 2026.
//! Sampler-turn pipeline for `SessionActor`: tool definitions, model auth facts/gates and retry, and sampler config reconstruction.
//! It also covers sampling-failure recovery and per-response usage recording.

use super::*;
use distill_login::backend::{ActiveAuthBackend, AuthBackend};
use distill_telemetry::region;
use distill_telemetry::region::Parent;

const CLASSIFIER_REQUEST_TOKEN_RESERVE: u64 = 16_384;

fn sampler_attribution_endpoint(
    config: &distill_sampling_types::SamplingConfig,
) -> Option<String> {
    let path = match config.api_backend.clone() {
        distill_sampling_types::ApiBackend::ChatCompletions => "chat/completions",
        distill_sampling_types::ApiBackend::Responses => "responses",
        distill_sampling_types::ApiBackend::Messages => "messages",
    };
    let base = config.base_url.trim_end_matches('/');
    (!base.is_empty()).then(|| format!("{base}/{path}"))
}

fn usage_is_complete(usage: Option<&distill_sampling_types::TokenUsage>) -> bool {
    usage.is_some()
}

fn classifier_request_fits_context(input_tokens: u64, context_window: u64) -> bool {
    input_tokens <= context_window.saturating_sub(CLASSIFIER_REQUEST_TOKEN_RESERVE)
}

/// Per-prompt cap on the Length tool-call salvage streak. Matches the agent implementation's `MAX_RETRY_ITERATIONS`.
pub(super) const MAX_OUTPUT_TOKEN_LIMIT_RETRIES: u32 = 5;

/// Resubmits per sampling step: automates the manual "Continue" that revives dead sessions.
pub(super) const MAX_TRANSIENT_TURN_RETRIES: u32 = 3;

/// Cumulative resubmit cap for the whole prompt.
/// Long agentic prompts re-earn the per-step budget every successful sample, so without this a partial outage multiplies retries by round count.
/// Auto-recovery, stop-hook continuations, and the goal loop re-enter `process_conversation_turn` within one prompt and would reset a local.
pub(super) const MAX_TRANSIENT_RETRIES_PER_PROMPT: u32 = 10;

/// Wall-clock budget per recovery episode (first transient failure after a success until the next success).
/// This bounds how many idle stalls can stack up: each stalled attempt burns a full idle-detector cycle before it even fails.
pub(super) const MAX_TRANSIENT_RETRY_WINDOW: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

/// Turn-loop retry state for the transient arm.
/// The window is evaluated at failure time (`tokio::time::Instant` so paused-clock tests can drive it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct TransientRetryState {
    /// Resubmits used this sampling step (resets on success/compaction).
    pub(crate) step_attempts: u32,
    /// Resubmits used across the whole prompt (never resets mid-prompt).
    pub(crate) prompt_attempts: u32,
    /// First transient failure of the current recovery episode (`None` until one happens; cleared on success).
    pub(crate) episode_start: Option<tokio::time::Instant>,
    /// Spawn-resolved kill switch (foreground root sessions only).
    pub(crate) enabled: bool,
}

/// Ceiling shown to the user (`Retrying (N/M)`): attempts used this step plus whatever further attempts both remaining budgets allow.
pub(super) fn transient_display_ceiling(step_attempts: u32, prompt_attempts: u32) -> u32 {
    step_attempts
        + MAX_TRANSIENT_TURN_RETRIES
            .saturating_sub(step_attempts)
            .min(MAX_TRANSIENT_RETRIES_PER_PROMPT.saturating_sub(prompt_attempts))
}

impl TransientRetryState {
    fn budget_remaining(&self) -> bool {
        self.step_attempts < MAX_TRANSIENT_TURN_RETRIES
            && self.prompt_attempts < MAX_TRANSIENT_RETRIES_PER_PROMPT
            && self
                .episode_start
                .is_none_or(|s| s.elapsed() < MAX_TRANSIENT_RETRY_WINDOW)
    }
}

/// Slower than the sampler's ladder (already spent).
/// Jittered at the sleep site: idle timeouts skip the sampler's jitter and fire in lockstep.
pub(super) const TRANSIENT_TURN_RETRY_BACKOFF: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(2),
    std::time::Duration::from_secs(10),
    std::time::Duration::from_secs(30),
];

const _: () = assert!(MAX_TRANSIENT_TURN_RETRIES as usize == TRANSIENT_TURN_RETRY_BACKOFF.len());

/// Delay before resubmit `attempts_used + 1`, clamped to the last rung.
pub(super) fn transient_backoff_delay(attempts_used: u32) -> std::time::Duration {
    TRANSIENT_TURN_RETRY_BACKOFF
        .get(attempts_used as usize)
        .or(TRANSIENT_TURN_RETRY_BACKOFF.last())
        .copied()
        .unwrap_or(std::time::Duration::from_secs(30))
}

/// Stream stalls (never retried internally), transport errors, retryable 5xx.
/// Vetoes mirror `is_retry_vetoed`. Status-less `Api` fails closed; other kinds keep their dedicated recovery or terminal path.
/// A 4xx (minus 408/429) or a serialization failure: the endpoint refused the
/// request itself, so retrying it unchanged is pointless.
pub(super) fn is_client_rejection(error: &distill_sampler::SamplingErrorInfo) -> bool {
    use distill_sampler::SamplingErrorKind;
    if matches!(error.kind, SamplingErrorKind::Serialization) {
        return true;
    }
    error
        .status_code
        .is_some_and(|status| (400..500).contains(&status) && status != 408 && status != 429)
}

pub(super) fn transient_retry_eligible(error: &distill_sampler::SamplingErrorInfo) -> bool {
    use distill_sampler::SamplingErrorKind;
    if error.should_retry == Some(false)
        || distill_sampling_types::is_context_length_error(&error.message)
        // Deterministic rejection however the proxy wrapped it (400 or 500); the sampler's own classifier already stripped images and gave up
        || matches!(
            error.error_code,
            Some(distill_sampling_types::ApiErrorCode::InvalidImage)
        )
    {
        return false;
    }
    match error.kind {
        // `clone_error` erases reqwest retryability, so all Http kinds retry here (bounded)
        SamplingErrorKind::IdleTimeout | SamplingErrorKind::Http => true,
        // The canonical retryable-status rule; 429 is excluded explicitly because it classifies as `RateLimited`
        SamplingErrorKind::Api => error.status_code.is_some_and(|status| {
            status != 429
                && reqwest::StatusCode::from_u16(status)
                    .is_ok_and(distill_sampling_types::is_retryable_api_status)
        }),
        SamplingErrorKind::Auth
        | SamplingErrorKind::Serialization
        | SamplingErrorKind::RateLimited
        | SamplingErrorKind::EmptyResponse
        | SamplingErrorKind::MaxTokensTruncation
        | SamplingErrorKind::DoomLoopDetected => false,
    }
}

/// Injected once per Length tool-call salvage streak so the model knows its output was cut mid-turn.
/// The text-continuation path injects `length_salvage::LENGTH_CONTINUE_REMINDER_BODY` instead.
pub(super) const OUTPUT_TOKEN_LIMIT_REMINDER: &str = "Your response was cut off because it exceeded the output token limit. \
     Please break your work into smaller pieces. Continue from where you left off.";

/// Consecutive Length-salvaged tool-call samples within one prompt.
/// Each salvage grows the context, so under a hard window cap an unbounded streak could loop forever once compaction stops freeing room.
/// Past the cap the turn fails like it did before salvage existed.
#[derive(Default)]
pub(super) struct LengthSalvageStreak {
    consecutive: u32,
}

/// What the turn loop does with a completed sample, per the salvage streak.
#[derive(Debug)]
pub(super) enum LengthSalvageAction {
    /// Not a Length-salvaged tool-call sample; the streak resets.
    NotSalvage,
    /// Execute the salvaged calls.
    /// `inject_reminder` is set on the first salvage of a streak (the reminder stays in context afterwards).
    Proceed { inject_reminder: bool },
    /// The streak exceeded [`MAX_OUTPUT_TOKEN_LIMIT_RETRIES`]; fail the turn.
    Exhausted,
}

impl LengthSalvageStreak {
    pub(super) fn on_sample(&mut self, length_with_tool_calls: bool) -> LengthSalvageAction {
        if !length_with_tool_calls {
            self.consecutive = 0;
            return LengthSalvageAction::NotSalvage;
        }
        self.consecutive += 1;
        if self.consecutive > MAX_OUTPUT_TOKEN_LIMIT_RETRIES {
            LengthSalvageAction::Exhausted
        } else {
            LengthSalvageAction::Proceed {
                inject_reminder: self.consecutive == 1,
            }
        }
    }
}

/// 403 is deliberately excluded because it means "authenticated but forbidden" (content-safety blocks, ZDR-gated requests, remote settings gates).
/// String fallbacks remain for tools that report auth failures without going through the structured `HttpFailure` path.
/// Examples: JSON-only `invalid_token` payloads, BYOK key-validation messages.
pub(super) fn is_auth_tool_error(err: &distill_tool_runtime::ToolError) -> bool {
    // When the error carries a structured HTTP status code in details, trust it as the authoritative signal
    // This replaces the legacy `HttpFailure { status, .. }` variant matching.
    if let Some(details) = &err.details
        && let Some(status) = details
            .get(HTTP_STATUS_DETAILS_KEY)
            .and_then(|s| s.as_u64())
    {
        return status == 401;
    }
    // String fallback for errors without structured status (e.g. BYOK key-validation messages, OAuth `invalid_token` payloads).
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_token")
}

/// Gate inputs bundled with the composed decision so the 401-recovery log can report the components.
#[derive(Clone, Copy)]
struct SessionTokenAuthGate {
    is_session_based: bool,
    model_byok: crate::agent::auth_method::ModelByok,
    /// Whether the request targets a first-party host.
    /// Lets an `Unknown` BYOK status still refresh against cli-chat-proxy / `*.x.ai` without risking a session-token leak to a third-party BYOK endpoint.
    endpoint_is_first_party: bool,
}

impl SessionTokenAuthGate {
    /// Single place `is_session_based` / `endpoint_is_first_party` are derived, so all call sites assemble the gate identically.
    fn new(
        auth_method_id: Option<&acp::AuthMethodId>,
        model_byok: crate::agent::auth_method::ModelByok,
        base_url: &str,
    ) -> Self {
        let has_codex_auth = !base_url.trim().is_empty()
            && crate::codex_auth::is_trusted_inference_base_url(base_url);
        Self {
            // `None` (pre-`authenticate`) classifies as non-session-based, so the gate stays inactive until a method is selected
            is_session_based: auth_method_id
                .is_some_and(crate::agent::auth_method::is_session_based_method)
                && !has_codex_auth,
            model_byok,
            endpoint_is_first_party: crate::util::is_xai_api_url(base_url),
        }
    }

    fn active(self) -> bool {
        crate::agent::auth_method::session_token_auth_gate(
            self.is_session_based,
            self.model_byok,
            self.endpoint_is_first_party,
        )
    }
}

/// Run a tool call; on an auth-shaped failure, attempt recovery via `AuthManager` and one retry.
/// When `shared_recovery` is `Some`, concurrent 401s in the same batch deduplicate via `OnceCell::get_or_init`.
pub(super) async fn call_with_auth_retry<F, Fut>(
    auth_manager: Option<&std::sync::Arc<distill_login::AuthManager>>,
    shared_recovery: Option<&tokio::sync::OnceCell<bool>>,
    tool_name: &str,
    mut call: F,
) -> Result<distill_tools::types::output::ToolRunResult, distill_tool_runtime::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                distill_tools::types::output::ToolRunResult,
                distill_tool_runtime::ToolError,
            >,
        >,
{
    let result = call().await;
    let Err(ref err) = result else { return result };
    if !is_auth_tool_error(err) {
        return result;
    }
    let Some(am) = auth_manager else {
        return result;
    };
    // Tool-call 401s show up as tool errors, not the ReAuthRequired banner
    let src = distill_login::recovery::RecoverySource::Background;
    let recovered = match shared_recovery {
        Some(cell) => *cell.get_or_init(|| am.try_recover_unauthorized(src)).await,
        None => am.try_recover_unauthorized(src).await,
    };
    if recovered {
        tracing::info!(
            tool = tool_name,
            "auth recovery: tool 401, recovered, retrying"
        );
        call().await
    } else {
        tracing::warn!(tool = tool_name, "auth recovery: tool 401, refresh failed");
        distill_telemetry::unified_log::warn(
            "auth recovery: tool 401, refresh failed",
            None,
            Some(serde_json::json!({ "tool": tool_name })),
        );
        result
    }
}

const STREAM_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamDrainOutcome {
    Acknowledged,
    TimedOut,
    Revoked,
}

async fn stream_drain_outcome(rx: tokio::sync::oneshot::Receiver<()>) -> StreamDrainOutcome {
    match tokio::time::timeout(STREAM_DRAIN_TIMEOUT, rx).await {
        Ok(Ok(())) => StreamDrainOutcome::Acknowledged,
        Ok(Err(_)) => StreamDrainOutcome::Revoked,
        Err(_) => StreamDrainOutcome::TimedOut,
    }
}

fn error_after_stream_drain(
    outcome: StreamDrainOutcome,
    original: distill_sampler::SamplingErrorInfo,
) -> distill_sampler::SamplingErrorInfo {
    if outcome == StreamDrainOutcome::Revoked {
        revoked_sampling_info()
    } else {
        original
    }
}

fn revoked_sampling_info() -> distill_sampler::SamplingErrorInfo {
    distill_sampler::SamplingErrorInfo {
        kind: distill_sampler::SamplingErrorKind::Api,
        status_code: None,
        message: "sampling result revoked by turn cancellation or rewind".to_string(),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: distill_sampling_types::SentCredential::Unknown,
    }
}

impl SessionActor {
    pub(super) async fn prepare_tool_definitions_timed(&self) -> (Vec<ToolDefinition>, u64) {
        let prep_span = region::Region::from_span(tracing::info_span!(
            "turn.tool_prep",
            mcp_wait_ms = tracing::field::Empty,
            total_prep_ms = tracing::field::Empty,
            tool_count = tracing::field::Empty,
        ));
        let mcp_wait_start = tokio::time::Instant::now();
        let full_wait = self.requires_full_mcp_wait();
        if (full_wait || matches!(self.mcp_strategy.get(), McpInitStrategy::Blocking))
            && !self.mcp_state.lock().await.is_initialized()
        {
            if full_wait {
                tracing::info!(
                    "delivery tools: waiting for MCP initialization before first prompt..."
                );
                self.wait_for_mcp_initialized_bounded_once().await;
            } else {
                self.wait_for_mcp_startup_grace().await;
            }
        }
        let mcp_wait_ms = mcp_wait_start.elapsed().as_millis() as u64;

        let defs = self.prepare_tool_definitions_inner().await;
        prep_span.span().record("mcp_wait_ms", mcp_wait_ms as i64);
        prep_span
            .span()
            .record("total_prep_ms", mcp_wait_start.elapsed().as_millis() as i64);
        prep_span.span().record("tool_count", defs.len() as i64);
        prep_span.close();
        (defs, mcp_wait_ms)
    }

    pub(super) async fn prepare_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.prepare_tool_definitions_timed().await.0
    }

    /// The exact tool specs a turn sends before its structured-output append.
    /// Shared with `SnapshotToolDefinitions` so verbatim mirrors preserve the parent schema.
    pub(crate) fn turn_base_tool_specs(&self, defs: &[ToolDefinition]) -> Vec<ToolSpec> {
        let backend_search_active = self.backend_search_active();
        defs.iter()
            .filter(|td| !backend_search_active || td.function.name != "web_search")
            .cloned()
            .map(ToolSpec::from)
            .collect()
    }

    /// Hosted tools with overrides applied, plus the applied overrides to echo, in one pass.
    fn resolve_hosted(
        &self,
    ) -> (
        Vec<distill_sampling_types::HostedTool>,
        distill_sampling_types::ToolOverrides,
    ) {
        let mut tools = self.agent.borrow().hosted_tools().to_vec();
        let applied = distill_sampling_types::apply_tool_overrides(
            &mut tools,
            self.tool_overrides.borrow().as_ref(),
        );
        (tools, applied)
    }

    /// Ungated. Prefer [`Self::hosted_tools_for_turn`], which folds in the backend-search gate.
    pub(crate) fn effective_hosted_tools(&self) -> Vec<distill_sampling_types::HostedTool> {
        self.resolve_hosted().0
    }

    pub(crate) fn hosted_tools_for_turn(&self) -> Vec<distill_sampling_types::HostedTool> {
        if self.backend_search_active() {
            self.effective_hosted_tools()
        } else {
            Vec::new()
        }
    }

    /// The applied overrides to echo, or `None` when backend search is off.
    pub(crate) fn effective_tool_overrides(&self) -> Option<distill_sampling_types::ToolOverrides> {
        if !self.backend_search_active() {
            return None;
        }
        let applied = self.resolve_hosted().1;
        (!applied.is_empty()).then_some(applied)
    }

    pub(crate) fn backend_search_active(&self) -> bool {
        self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get()
    }

    /// Set the per-turn override and emit it before any turn runs, so a subagent spawned this turn inherits it.
    pub(crate) fn set_tool_overrides(&self, overrides: distill_sampling_types::ToolOverrides) {
        *self.tool_overrides.borrow_mut() = Some(overrides);
        self.emit_resolved_tool_overrides();
    }

    /// Fold a per-turn update at promotion: an object sets, `null` clears to the seed, absent leaves.
    pub(crate) fn apply_tool_overrides_update(
        &self,
        update: Option<distill_sampling_types::ToolOverridesUpdate>,
    ) {
        let Some(update) = update else { return };
        {
            let mut slot = self.tool_overrides.borrow_mut();
            *slot = update.apply(slot.take());
        }
        self.emit_resolved_tool_overrides();
    }

    /// Store this session's cutoff in the cell a subagent spawn reads.
    /// Not gated on backend search, so a bounded parent bounds a searching child even if it isn't searching.
    pub(crate) fn emit_resolved_tool_overrides(&self) {
        let seed = self.agent.borrow().definition().tool_overrides.clone();
        let effective = resolve_configured_cutoff(seed, self.tool_overrides.borrow().as_ref());
        self.resolved_tool_overrides
            .store((!effective.is_empty()).then(|| std::sync::Arc::new(effective)));
    }

    pub(super) async fn prepare_tool_definitions_inner(&self) -> Vec<ToolDefinition> {
        // Clone `ToolBridge` under a *short* `agent` RefCell borrow, then await on the Arc
        // Never hold `self.agent.borrow()` across `.await`
        // Prefire runs `spawn_local` on the same LocalSet as the turn loop, so a parked borrow here would panic if turn/compact/cancel also borrowed it
        let bridge = self.agent.borrow().tool_bridge().clone();

        // Local mode: tool search is always enabled
        let defs = bridge.tool_definitions_builtins_only().await;

        let plan_active = self.plan_mode.lock().is_active();
        let defs = filter_cursor_tools_by_plan_mode(defs, plan_active);
        // Jev turn-start pass: intent (B1), family pruning (B4/P1), delegation
        // hint (B6). Plan mode is left untouched inside the pass.
        self.jev_filter_tool_definitions(defs, plan_active).await
    }

    pub(super) fn model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        self.model_auth_state(model_id).0
    }

    pub(super) fn model_auth_provider(
        &self,
        model_id: &str,
    ) -> Option<distill_login::AuthProviderRef> {
        self.model_auth_state(model_id).1
    }

    /// Drop the memoized per-model auth state; see [`Self::model_auth_memo`] for why each model/credential chokepoint must call this.
    pub(crate) fn invalidate_model_auth_memo(&self) {
        self.model_auth_memo.replace(None);
    }

    /// Reads and populates [`Self::model_auth_memo`]; a fresh `Unknown` falls back to the last definite entry (see the field's contract).
    fn model_auth_state(
        &self,
        model_id: &str,
    ) -> (
        crate::agent::config::ModelAuthFacts,
        Option<distill_login::AuthProviderRef>,
    ) {
        use crate::agent::auth_method::ModelByok;
        use crate::session::acp_session::ModelAuthMemo;
        if let Some(memo) = self.model_auth_memo.borrow().as_ref()
            && memo.model_id == model_id
            && memo.facts.byok != ModelByok::Unknown
        {
            return (memo.facts, memo.provider.clone());
        }
        let (fresh, provider) =
            crate::agent::config::resolve_model_auth_facts_and_provider(model_id);
        if fresh.byok == ModelByok::Unknown {
            if let Some(memo) = self.model_auth_memo.borrow().as_ref()
                && memo.model_id == model_id
            {
                return (memo.facts, memo.provider.clone());
            }
            return (fresh, provider);
        }
        *self.model_auth_memo.borrow_mut() = Some(ModelAuthMemo {
            model_id: model_id.to_string(),
            facts: fresh,
            provider: provider.clone(),
        });
        (fresh, provider)
    }

    /// The single writer of a provider mint/rotation into chat-state credentials.
    async fn set_chat_api_key(&self, new_key: String) {
        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }

    /// Pre-turn arm for a provider-backed model: mint on a cold cache, re-mint near expiry, and adopt a rotation that chat state missed.
    /// No-op when `current_key` is already the fresh cached token.
    async fn refresh_provider_token_pre_turn(
        &self,
        provider: &distill_login::AuthProviderRef,
        current_key: Option<&str>,
        model_id: &str,
    ) {
        match provider.ensure_fresh_token(current_key).await {
            distill_login::ProviderRefreshOutcome::Rotated(new_key) => {
                tracing::info!(
                    model = %model_id,
                    provider = %provider.name,
                    cold = current_key.is_none(),
                    "auth provider token rotated pre-turn"
                );
                self.set_chat_api_key(new_key).await;
            }
            distill_login::ProviderRefreshOutcome::Unchanged => {}
            // A genuine mint failure; the 401 arm handles the rejection.
            distill_login::ProviderRefreshOutcome::MintFailed => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    provider = %provider.name,
                    model = %model_id,
                    "auth provider pre-turn refresh failed"
                );
                distill_telemetry::unified_log::warn(
                    "auth provider pre-turn refresh failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "provider": provider.name,
                        "model": model_id,
                        "cold": current_key.is_none(),
                    })),
                );
            }
            // Unusable provider: already warned once, no per-turn breadcrumb.
            distill_login::ProviderRefreshOutcome::Unusable => {}
        }
    }

    /// 401 arm for a provider-backed model: re-run the helper once and resubmit.
    /// A missing key means the cold mint failed and the request went out unauthenticated, so mint instead.
    /// Returns `false` when the fresh-mint guard blocked the re-run or the helper failed; the 401 then becomes a terminal error.
    async fn try_provider_401_recovery(&self, provider: &distill_login::AuthProviderRef) -> bool {
        let rejected_key = self.chat_state_handle.get_credentials().await.api_key;
        let recovered = match rejected_key {
            Some(ref rejected_key) => provider.recover_rejected_token(rejected_key).await,
            None => provider.ensure_fresh_token(None).await.rotated(),
        };
        let Some(new_key) = recovered else {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                provider = %provider.name,
                "auth recovery: sampler 401, provider re-mint declined or failed"
            );
            distill_telemetry::unified_log::warn(
                "auth recovery: sampler 401, provider re-mint declined or failed",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "provider": provider.name })),
            );
            return false;
        };
        tracing::info!(
            session_id = %self.session_info.id.0,
            provider = %provider.name,
            "auth recovery: sampler 401, auth provider re-mint, retrying"
        );
        distill_telemetry::unified_log::info(
            "auth recovery: sampler 401, auth provider re-mint, retrying",
            Some(self.session_info.id.0.as_ref()),
            None,
        );
        self.set_chat_api_key(new_key).await;
        true
    }

    /// Gate inputs for `model_id` routed to `base_url`.
    /// See [`crate::agent::auth_method::session_token_auth_gate`] for the rationale.
    /// `base_url` keeps an `Unknown` BYOK status refreshable only against first-party Grok hosts.
    fn auth_gate(&self, model_id: &str, base_url: &str) -> SessionTokenAuthGate {
        let byok = self.model_auth_facts(model_id).byok;
        let auth_method = self.auth_method_id.load();
        SessionTokenAuthGate::new(auth_method.as_deref(), byok, base_url)
    }

    /// Emit a unified-log breadcrumb whenever the session-token refresh gate sees an `Unknown` per-model BYOK status on a session-based method.
    /// The uploaded per-turn unified log then shows whether the first-party-endpoint fallback kept refresh active or withheld it.
    /// A burst of these is itself the signal that `Unknown` is being hit in the field.
    fn log_auth_gate_unknown(&self, site: &str, gate: SessionTokenAuthGate, base_url: &str) {
        use crate::agent::auth_method::ModelByok;
        if gate.model_byok != ModelByok::Unknown || !gate.is_session_based {
            return;
        }
        let refresh_active = gate.active();
        let ctx = serde_json::json!({
            "site": site,
            "model_byok": gate.model_byok.as_ref(),
            "is_session_based": gate.is_session_based,
            "endpoint_is_first_party": gate.endpoint_is_first_party,
            "refresh_active": refresh_active,
            "base_url": base_url,
        });
        let sid = Some(self.session_info.id.0.as_ref());
        if refresh_active {
            distill_telemetry::unified_log::info(
                "auth gate: Unknown BYOK on first-party endpoint — session-token refresh kept active",
                sid,
                Some(ctx),
            );
        } else {
            distill_telemetry::unified_log::warn(
                "auth gate: Unknown BYOK on non-first-party endpoint — refresh withheld (may surface stale-token 401)",
                sid,
                Some(ctx),
            );
        }
    }

    /// Reconstruct a full `SamplerConfig` (with credentials) by combining the actor's `SamplingConfig` and `Credentials`.
    /// Folds in the URL-derived headers (cli-chat-proxy auth, the staging auth header) so the sampler crate stays URL-agnostic.
    pub(super) async fn reconstruct_full_config(&self) -> SamplingConfig {
        #[allow(clippy::items_after_statements)]
        #[derive(Debug)]
        struct TraceContextInjector;
        impl distill_sampler::HeaderInjector for TraceContextInjector {
            fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
                if let Some(tp) = distill_otel::current_traceparent()
                    && let Ok(v) = reqwest::header::HeaderValue::from_str(&tp)
                {
                    headers.insert("traceparent", v);
                }
            }

            fn set_span_parent(&self, span: &tracing::Span, traceparent: &str) {
                if !distill_otel::set_parent_from_traceparent(span, traceparent) {
                    tracing::debug!(
                        traceparent = %traceparent,
                        "HTTP span did not adopt its trace parent"
                    );
                }
            }
        }

        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap_or_else(|| distill_sampling_types::SamplingConfig {
                base_url: String::new(),
                mtls_cert_dir: None,
                model: String::new(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                max_retries: None,
                rate_limit_retry_threshold: None,
                api_backend: Default::default(),
                extra_headers: Default::default(),
                conversation_group_id: None,
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                reasoning_shape: Default::default(),
                reasoning_summary: None,
                stream_tool_calls: None,
            });
        let creds = self.chat_state_handle.get_credentials().await;
        let model_facts = self.model_auth_facts(cfg.model.as_str());
        // Gate on the stable session classifier, not `creds.auth_type`; see `crate::agent::auth_method::session_token_auth_gate`
        // `cfg.base_url` keeps an `Unknown` BYOK status refreshable against first-party Grok hosts
        // That avoids leaking the session token to a third-party endpoint
        let auth_method = self.auth_method_id.load();
        let gate =
            SessionTokenAuthGate::new(auth_method.as_deref(), model_facts.byok, &cfg.base_url);
        let use_bearer_resolver = gate.active();
        self.log_auth_gate_unknown("reconstruct_full_config", gate, &cfg.base_url);
        // Refresh the session token before the sampler reads it; gated to sessions that use it.
        if use_bearer_resolver && let Some(am) = self.auth_manager.as_ref() {
            let _ = am.auth().await;
        }
        // Session path: only seed a wire-valid AT
        // Hard-expired keys must not land in default headers when the resolver has nothing to stamp
        let api_key = if use_bearer_resolver {
            // `use_bearer_resolver` means the endpoint is a first-party Grok URL.
            // A session from another authority must not seed the default headers either.
            self.auth_manager
                .as_ref()
                .filter(|_| ActiveAuthBackend::default().is_xai_authority())
                .and_then(|am| am.current_wire_valid().map(|a| a.key))
        } else {
            creds.api_key
        };
        let auth_scheme = model_facts.auth_scheme;
        let mut extra_headers = cfg.extra_headers;
        crate::agent::config::inject_url_derived_headers(
            &mut extra_headers,
            creds.alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        let compaction_at_tokens = self.compaction_at_tokens.get();
        let compactions_remaining = self.compactions_remaining.get();
        if compactions_remaining.is_some() || compaction_at_tokens.is_some() {
            let has_compaction_summary = self
                .chat_state_handle
                .get_last_compaction_prompt_index()
                .await
                .is_some();
            if let Some(value) =
                compactions_remaining.and_then(|c| c.resolve(has_compaction_summary))
            {
                extra_headers.insert("x-compactions-remaining".to_string(), value.to_string());
            }
            // Send on the uncompacted prefix; drop the header once the session compacts.
            if !has_compaction_summary
                && let Some(value) = compaction_at_tokens.and_then(|c| {
                    c.resolve(
                        cfg.context_window.get(),
                        self.compaction.threshold_percent.get(),
                    )
                })
            {
                extra_headers.insert("x-compaction-at".to_string(), value.to_string());
            }
        }
        let extra_response_includes = crate::agent::config::response_include_extensions(
            self.supports_backend_search.get(),
            &cfg.api_backend,
            &cfg.base_url,
        );
        let request_compression = crate::util::config::request_compression_for_url(&cfg.base_url);
        let bearer_resolver: Option<distill_sampler::config::SharedBearerResolver> =
            if use_bearer_resolver {
                self.auth_manager.as_ref().map(|am| {
                    distill_login::credential_provider::WireValidBearerResolver::shared(am.clone())
                })
            } else if api_key.is_none()
                && !cfg.base_url.is_empty()
                && crate::codex_auth::is_trusted_inference_base_url(&cfg.base_url)
            {
                if let Err(error) = crate::codex_auth::fresh_credentials().await {
                    tracing::warn!(%error, "Could not refresh ChatGPT credentials before inference");
                }
                // The serializable session state retains the identity anchor, but
                // cannot retain the resolver. Restore it for each inference call.
                Some(std::sync::Arc::new(
                    crate::codex_auth::CodexBearerResolver::from_headers(&extra_headers),
                ))
            } else {
                None
            };
        SamplingConfig {
            api_key,
            base_url: cfg.base_url,
            mtls_cert_dir: cfg.mtls_cert_dir,
            model: cfg.model,
            max_completion_tokens: cfg.max_completion_tokens,
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            api_backend: cfg.api_backend,
            auth_scheme,
            request_compression,
            extra_headers,
            conversation_group_id: cfg.conversation_group_id,
            extra_response_includes,
            query_params: cfg.query_params.clone(),
            env_http_headers: cfg.env_http_headers.clone(),
            context_window: cfg.context_window.get(),
            client_version: creds.client_version,
            reasoning_effort: cfg.reasoning_effort,
            reasoning_shape: cfg.reasoning_shape,
            reasoning_summary: cfg.reasoning_summary,
            force_http1: false,
            max_retries: cfg.max_retries.or(Some(self.max_retries)),
            rate_limit_retry_threshold: cfg.rate_limit_retry_threshold,
            stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
            idle_timeout_secs: None,
            client_identifier: self.client_identifier.clone(),
            deployment_id: crate::managed_config::resolve_deployment_id(
                crate::managed_config::resolve_deployment_key().as_deref(),
            ),
            user_id: self
                .auth_manager
                .as_ref()
                .and_then(|am| am.current_or_expired())
                .filter(|a| a.is_xai_auth())
                .map(|a| a.user_id),
            origin_client: self.origin_client.clone(),
            // Attribute sampler 401s against the bearer sent on the wire.
            // `None` for sessions spawned without an `AuthManager` (BYOK direct, certain test fixtures)
            attribution_callback: self.attribution_callback.clone(),
            bearer_resolver,
            supports_backend_search: self.supports_backend_search.get(),
            compactions_remaining: self.compactions_remaining.get(),
            compaction_at_tokens: self.compaction_at_tokens.get(),
            // The sampler sends the opt-in header itself when this is set.
            doom_loop_recovery: self.doom_loop_recovery,
            header_injector: Some(std::sync::Arc::new(TraceContextInjector)),
        }
    }

    /// Install the auto-mode permission classifier with a live LLM side-query.
    /// A channel bridges the `Send` permission actor.
    /// The heuristic runs only when the side-query errors or returns unparseable text.
    pub(crate) async fn wire_permission_auto_llm_classifier(self: &Arc<Self>) {
        // `is_auto_mode()` is only true when the gated `set_auto_mode` call sites (spawn and SetAutoMode) flipped the permission manager to auto
        // This guard already prevents wiring whenever the feature gate is off
        if !self.permissions.is_auto_mode() {
            return;
        }
        // Idempotency: if a side-query worker is already wired, don't spawn another.
        // Reconnect (load_session) and repeated SetAutoMode { enabled: true } can call this again
        // Re-wiring would leak the prior spawn_local worker blocked on a dropped receiver while a new task handles requests
        if self.permissions.has_llm_side_query() {
            return;
        }
        // Resolve the `[auto_mode]` config (local config, then remote, then the built-in default)
        // When auto mode is enabled and unconfigured, the classifier uses the current model at low reasoning effort (if the model supports it)
        // It uses the `just_command` prompt; local config and remote settings override these
        let auto_cfg = crate::util::config::resolve_auto_mode_config_from_disk();
        let session_model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        // Route the classifier to a dedicated model when a slug is configured
        // A None or unresolvable slug falls back to the session client and model
        let aux_classifier_sampler = match auto_cfg.classifier_model.as_deref() {
            Some(slug) => self.resolve_auto_classifier_sampler(slug).await,
            None => None,
        };
        // Built-in defaults: the just_command prompt, and low effort if the model ACTUALLY used supports it
        // That model is the resolved aux model, else the session model we fall back to when the slug is unset/unresolvable
        // Explicit config overrides
        let models = self.models_manager.models();
        let effective_supports_re = crate::agent::config::effective_classifier_supports_re(
            aux_classifier_sampler
                .as_ref()
                .map(|(_, model, _)| model.as_str()),
            &session_model,
            &models,
        );
        let (prompt_type, classifier_reasoning_effort) =
            crate::util::config::auto_mode_classifier_defaults(&auto_cfg, effective_supports_re);
        let classify_timeout = crate::util::config::auto_mode_classify_timeout(&auto_cfg);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
            Vec<distill_workspace::permission::ClassifierMessage>,
            tokio::sync::oneshot::Sender<
                Result<String, distill_workspace::permission::ClassifierFailure>,
            >,
        )>();
        let session = Arc::clone(self);
        // One shared worker serializes parent and subagent classifier requests.
        tokio::task::spawn_local(async move {
            while let Some((messages, respond_to)) = rx.recv().await {
                let request_span = region!("permission.classifier_request", Parent::Root);
                let result = async {
                    let (sampling_client, model, context_window) = match &aux_classifier_sampler {
                        Some((client, model, context_window)) => {
                            (client.clone(), model.clone(), *context_window)
                        }
                        None => {
                            session.refresh_token_if_expired().await;
                            let config = session.reconstruct_full_config().await;
                            let context_window = config.context_window;
                            let model = config.model.clone();
                            let client = distill_sampler::SamplingClient::new(config).map_err(|e| {
                                distill_workspace::permission::ClassifierFailure::TransportError(
                                    e.to_string(),
                                )
                            })?;
                            (client, model, context_window)
                        }
                    };
                    let session_id = session.session_info.id.to_string();
                    let items = messages
                        .into_iter()
                        .map(|m| match m.role {
                            distill_workspace::permission::ClassifierMessageRole::System => {
                                ConversationItem::system(m.text)
                            }
                            distill_workspace::permission::ClassifierMessageRole::User => {
                                ConversationItem::user(m.text)
                            }
                        })
                        .collect::<Vec<_>>();
                    let input_tokens = distill_chat_state::estimate_conversation_tokens(&items);
                    if !classifier_request_fits_context(input_tokens, context_window) {
                        return Err(
                            distill_workspace::permission::ClassifierFailure::TransportError(
                                "permission auto classifier request exceeds context window"
                                    .to_owned(),
                            ),
                        );
                    }
                    let request = ConversationRequest {
                        items,
                        tools: vec![],
                        hosted_tools: vec![],
                        tool_choice: None,
                        model: Some(model),
                        // Thinking modes can reject explicit temperature or output limits, so retain provider defaults; the schema bounds output
                        temperature: None,
                        max_output_tokens: None,
                        // Structured output: constrain the model to the {thinking, shouldBlock, reason} schema
                        // The response is then guaranteed parseable (parity with forced-classify tooling)
                        json_schema: Some(
                            distill_workspace::permission::classifier_output_json_schema(),
                        ),
                        // Resolved `[auto_mode]` effort: explicit config/remote, else the built-in `Low` default when the model supports it
                        // None means the provider default
                        reasoning_effort: classifier_reasoning_effort,
                        x_grok_conv_id: Some(format!("perm-classifier-{}", uuid::Uuid::new_v4())),
                        x_grok_req_id: Some(format!("xai-perm-auto-{}", uuid::Uuid::new_v4())),
                        x_grok_session_id: Some(session_id),
                        x_grok_agent_id: Some(distill_telemetry::id::agent_id()),
                        ..ConversationRequest::default()
                    };
                    let fut = sampling_client.conversation_collect(request);
                    let response = tokio::time::timeout(classify_timeout, fut)
                        .await
                        .map_err(|_| distill_workspace::permission::ClassifierFailure::Timeout)?
                        .map_err(|e| {
                            distill_workspace::permission::ClassifierFailure::TransportError(
                                e.to_string(),
                            )
                        })?;
                    Ok(response.assistant_text())
                }
                .await;
                if let Err(error) = &result {
                    tracing::warn!(%error, "permission auto classifier side-query failed");
                }
                request_span.close();
                let _ = respond_to.send(result);
            }
        });
        let llm_classifier =
            distill_workspace::permission::LlmPermissionClassifier::with_channel(tx, prompt_type);
        debug_assert!(
            llm_classifier.has_side_query(),
            "channel-wired classifier must report has_side_query"
        );
        // Permission decisions belong to the harness, independently of Jev.
        self.permissions
            .set_classifier_with_side_query(llm_classifier, true);
        tracing::info!(
            session_id = %self.session_info.id,
            "Wired live LLM permission auto-mode classifier (session sampling channel)"
        );
    }

    /// Resolve a standalone aux-model `SamplerConfig` for `slug` via the shared catalog routing, gathering the session-local auth context once.
    /// The routing is Tier-1 catalog creds / Tier-2 Grok-proxy via session token / `XAI_API_KEY` / deployment key.
    /// Shared by image-describe and the classifier so the gather can't drift.
    pub(super) async fn resolve_aux_sampler_config(
        &self,
        slug: &str,
    ) -> Option<distill_sampler::SamplerConfig> {
        let creds = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key.clone()));
        let models = self.models_manager.models();
        let endpoints = self.models_manager.endpoints();
        let disable_api_key_auth = self
            .auth_manager
            .as_ref()
            .map(|am| am.grok_com_config().api_key_auth_disabled())
            .unwrap_or(false);
        crate::agent::config::resolve_aux_model_sampling_config(
            slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            creds.alpha_test_key.clone(),
            creds.client_version.clone(),
        )
    }

    /// Resolve a dedicated sampler for the Auto-mode classifier model `slug`, stamping session-local auth/attribution like image-describe.
    /// Image-describe relies on the resolver, not a config override, for `base_url`/`api_backend` so credentials stay consistent.
    /// `None` means the caller falls back to the session client and model.
    async fn resolve_auto_classifier_sampler(
        &self,
        slug: &str,
    ) -> Option<(distill_sampler::SamplingClient, String, u64)> {
        let active_session_config = self.reconstruct_full_config().await;
        let mut cfg = self.resolve_aux_sampler_config(slug).await?;
        crate::agent::config::stamp_session_local_sampler_fields(
            &mut cfg,
            &active_session_config,
            self.client_identifier.clone(),
            active_session_config.max_retries,
        );
        let model = cfg.model.clone();
        let context_window = cfg.context_window;
        let client = distill_sampler::SamplingClient::new(cfg)
            .map_err(
                |e| tracing::warn!(error = %e, "auto classifier aux sampler build failed; using session model"),
            )
            .ok()?;
        Some((client, model, context_window))
    }

    #[tracing::instrument(
        name = "session.prepare_chat_completion",
        skip_all,
        fields(force_http1)
    )]
    pub(super) async fn prepare_chat_completion(
        &self,
        force_http1: bool,
    ) -> Result<distill_sampler::SamplingClient, acp::Error> {
        // Check if the JWT token is expired/near-expiration and refresh from config if needed
        self.refresh_token_if_expired().await;

        let mut full_config = self.reconstruct_full_config().await;
        full_config.force_http1 = force_http1;
        let sampling_client =
            distill_sampler::SamplingClient::new(full_config).map_err(|e| self.to_acp_error(e))?;

        Ok(sampling_client)
    }

    // -----------------------------------------------------------------
    // Sampler-driven turn: per-turn config refresh and failure recovery
    // -----------------------------------------------------------------

    // (See `SamplerFailureRecovery` enum near the bottom of the impl block.)

    /// Refresh auth and push a fresh `SamplerConfig` before each turn.
    pub(crate) async fn prepare_sampler_for_turn(&self) {
        // A retry/fallback can arrive after a prior round has already
        // published its route. Clear that attribution before selecting the
        // next final route so the parent never sees a stale active model.
        self.signals_handle().clear_active_dispatch();
        self.refresh_token_if_expired().await;
        let mut sampler_config = self.reconstruct_full_config().await;
        crate::jev::begin_model_round();
        let (session_id, turn_id, round_id) = crate::jev::telemetry_context();
        tracing::info!(target: "jev.decision", event_kind = "llm_round_start",
            session_id, turn_id, round_id, requested_model = sampler_config.model,
            requested_effort = sampler_config.reasoning_effort.map(|effort| effort.as_ref().to_owned()),
            "model round before Jev routing");
        // Choose this call's executor independently of each model's effort policy.
        // Explicit efforts stay fixed; auto efforts use that candidate's own menu.
        self.jev_choose_model_and_effort(&mut sampler_config).await;
        // B2 (local): with a local model configured, the free model takes the
        // call whenever it can fully do it.
        self.jev_route_micro_call(&mut sampler_config).await;
        // B2 (money lever): a routine turn may run at a cheaper setting; the
        // pass can only lower effort, and it is off until its gate passes.
        self.jev_apply_model_tier(&mut sampler_config).await;
        // A redo the change review asked for runs at the setting it asked for:
        // the floor wins over anything cheaper chosen above.
        self.jev_apply_effort_floor(&mut sampler_config).await;
        if self.tool_context.task_output_token_budget.is_some()
            || self.tool_context.sampler_retry_only_before_output
        {
            sampler_config.doom_loop_recovery = None;
        }
        // Route consumption happens when the request is sent, so publish the
        // final model after every tier/floor/override has run. A model route
        // retains the normal payload; only a local utility route is rewritten
        // by `run_turn_via_sampler`.
        let local_route = self.jev_ledger.borrow().pending_route_is_local();
        self.jev_ledger
            .borrow_mut()
            .set_pending_route_with_locality(sampler_config.model.clone(), local_route);
        // The turn report: this call runs on this final model, at this final
        // effort.
        self.jev_ledger
            .borrow_mut()
            .set_pending_request_effort(sampler_config.reasoning_effort);
        self.note_round_for_turn_report(&sampler_config);
        self.signals_handle().set_active_dispatch(
            sampler_config.model.clone(),
            sampler_config
                .reasoning_effort
                .map(|effort| effort.as_ref().to_string()),
            Some(sampler_config.context_window),
        );
        // Carry over the session's per-chunk idle timeout via `SamplerConfig.idle_timeout_secs`
        sampler_config.idle_timeout_secs = Some(self.inference_idle_timeout.as_secs());
        self.emit_usage_update().await;
        self.sampler_handle.update_config(sampler_config);
        // Ensure the transient status snapshot observes the final route's
        // signal before the detached emitter reads it.
        let _ = self.signals_handle().snapshot().await;
        self.emit_status_snapshot_detached();
    }

    /// Fold an auth remedy into a turn failure: its advice becomes the tail of the message.
    /// Its `turn_error_type` becomes the classification the client keys its re-auth prompt off.
    fn apply_auth_remedy(
        &self,
        remedy: &distill_login::AuthRemedy,
        message: String,
        status_code: Option<u16>,
    ) -> (&'static str, String) {
        distill_telemetry::unified_log::info(
            "auth: turn failure classified",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "status_code": status_code,
                "remedy": format!("{remedy:?}"),
            })),
        );
        let message = match remedy.advice() {
            Some(advice) => format!("{message}\n\n{advice}"),
            None => message,
        };
        (remedy.turn_error_type(), message)
    }

    /// Terminal failure when consecutive Length salvages exceed the cap: executing more calls is not converging, so surface the pre-salvage `MaxTokensTruncation` failure.
    /// The sampler emitted only Completed events for these samples, so the error signal is recorded here.
    pub(crate) async fn fail_turn_length_salvage_exhausted(&self) -> acp::Error {
        let kind = distill_sampler::SamplingErrorKind::MaxTokensTruncation;
        let message = distill_sampling_types::SamplingError::MaxTokensTruncation.to_string();
        self.signals_handle().record_error_typed(kind.as_ref());
        self.log_terminal_failure(kind.as_ref(), None, &message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: kind.as_ref().to_owned(),
                message: message.clone(),
            },
        ))
        .await;
        acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
            message, None, kind,
        ))
    }

    /// Terminal failure for a turn the auth-retry budget gave up on — outside
    /// [`Self::handle_sampling_failure`], so it owes the client its own
    /// `RetryState::Failed` (the pager's re-auth prompt keys off it).
    pub(crate) async fn fail_turn_auth_budget_exhausted(&self, message: String) -> acp::Error {
        const STATUS: Option<u16> = Some(401);
        let (error_type, message) = match self.auth_manager.as_ref() {
            Some(auth_manager) => self.apply_auth_remedy(
                &auth_manager.auth_remedy().after_retries_exhausted(),
                message,
                STATUS,
            ),
            None => ("auth", message),
        };
        self.log_terminal_failure(error_type, STATUS, &message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_owned(),
                message: message.clone(),
            },
        ))
        .await;
        acp::Error::internal_error().data(crate::sampling::error::error_data_with_status(
            message, STATUS,
        ))
    }

    fn log_terminal_failure(&self, error_type: &str, status_code: Option<u16>, message: &str) {
        let auth = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired());
        let reauthable = is_reauthable_failure(Some(error_type), message);
        distill_telemetry::unified_log::warn(
            "turn.terminal_failure",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "error_type": error_type,
                "status_code": status_code,
                "reauthable": reauthable,
                "auth_mode": auth.as_ref().map(|a| format!("{:?}", a.auth_mode)),
                "key_prefix": auth.as_ref().map(|a| distill_auth::bearer_suffix(&a.key).to_owned()),
                "expires_at": auth
                    .as_ref()
                    .and_then(|a| a.expires_at.map(|e| e.to_rfc3339())),
                "message": crate::util::truncate(message, 300),
            })),
        );
    }

    /// The failed request's usage never arrived: fail the task budget closed (budgeted children) or mark the session totals incomplete.
    /// Send-only, not spawned: the mark must be in the actor's mailbox before the completing turn's billing epilogue reads the ledger.
    /// It must also land before a queued prompt promotes and resets the ledger.
    fn mark_turn_usage_unaccounted(self: &Arc<Self>) {
        if self.tool_context.task_output_token_budget.is_some() {
            self.tool_context.fail_task_output_usage_closed();
        } else {
            self.chat_state_handle
                .mark_usage_incomplete_nowait(true, true);
        }
    }

    /// Park eligibility for a credential-less 401: kill switch on, provably no credential on the
    /// wire, a self-healing remedy (manual-login stays terminal). One predicate so the two
    /// fail-closed gates cannot drift.
    fn uncharged_401_park_eligible(
        &self,
        am: &distill_login::AuthManager,
        credential: distill_sampling_types::SentCredential,
    ) -> bool {
        self.uncharged_401_park_enabled
            && credential.is_missing()
            && am.auth_remedy().is_self_healing()
    }

    /// Park outcome plus its log line; `dispatch_skipped` marks the already-parked arm.
    fn park_uncharged_401(
        &self,
        credential: distill_sampling_types::SentCredential,
        dispatch_skipped: bool,
    ) -> SamplerFailureRecovery {
        tracing::warn!(
            session_id = %self.session_info.id.0,
            dispatch_skipped,
            "auth recovery: credential-less 401, parking on uncharged resubmit"
        );
        distill_telemetry::unified_log::warn(
            "auth recovery: credential-less 401, parking on uncharged resubmit",
            Some(self.session_info.id.0.as_ref()),
            dispatch_skipped.then(|| serde_json::json!({ "dispatch": "skipped_already_parked" })),
        );
        SamplerFailureRecovery::RefreshAuthAndResubmit {
            credential,
            store: RecoveredStore::SessionToken,
        }
    }

    /// Classify a terminal sampler failure and decide recovery.
    /// `transient`: turn-loop retry state (the loop owns the counters).
    /// `park`: already parked — a still-credential-less 401 re-parks without a recovery dispatch.
    pub(crate) async fn handle_sampling_failure(
        self: &Arc<Self>,
        error: distill_sampler::SamplingErrorInfo,
        rate_limit_waits: u32,
        transient: TransientRetryState,
        mid_salvage_continuation: bool,
        park: TurnParkState,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        use distill_sampler::SamplingErrorKind;

        // On an in-flight salvage continuation, a max-tokens failure or a probable context overflow is not terminal.
        // Return before the budgeted-child rewrites so the marker survives for workflow children.
        // The request's usage never arrived; account for it fail-closed.
        let encrypted_content_mismatch = matches!(error.kind, SamplingErrorKind::Api)
            && error.status_code == Some(400)
            && error.message.contains("encrypted_content");
        let quiet_mid_salvage = mid_salvage_continuation
            && (error.kind == SamplingErrorKind::MaxTokensTruncation
                || distill_sampling_types::is_context_length_error(&error.message)
                || (!matches!(
                    error.kind,
                    SamplingErrorKind::Auth | SamplingErrorKind::RateLimited
                ) && !encrypted_content_mismatch
                    && self.estimate_exceeds_error_context_window(&error).await));
        if quiet_mid_salvage {
            self.mark_turn_usage_unaccounted();
            let mut data = crate::sampling::error::terminal_error_data(
                error.message,
                error.status_code,
                SamplingErrorKind::MaxTokensTruncation,
            );
            // Two populations for rollout sizing: either the continuation was unsalvageable at the cap, or the request no longer fit.
            // Unsalvageable means empty or a truncated tool-call tail; the sampler folds both into `MaxTokensTruncation` `terminal_error_data` returns the object shape for `MaxTokensTruncation`.
            // Guard rather than index-assign so a shape change cannot panic here.
            if let Some(obj) = data.as_object_mut() {
                obj.insert(
                    crate::sampling::error::SALVAGE_CAUSE_KEY.to_owned(),
                    serde_json::json!(if error.kind == SamplingErrorKind::MaxTokensTruncation {
                        crate::sampling::error::SALVAGE_CAUSE_EMPTY
                    } else {
                        crate::sampling::error::SALVAGE_CAUSE_OVERFLOW
                    }),
                );
            }
            return Err(acp::Error::internal_error().data(data));
        }

        if self.tool_context.task_output_token_budget.is_some() {
            self.tool_context.fail_task_output_usage_closed();
            let message = format!(
                "budgeted workflow child model request failed; output grant exhausted: {}",
                error.message
            );
            self.log_terminal_failure("output_budget_usage_unknown", error.status_code, &message);
            return Err(acp::Error::internal_error().data(message));
        }
        if self.tool_context.sampler_retry_only_before_output {
            self.mark_turn_usage_unaccounted();
            let message = format!(
                "workflow child model request failed; usage may understate real spend: {}",
                error.message
            );
            self.log_terminal_failure(
                "workflow_child_sampling_failed",
                error.status_code,
                &message,
            );
            return Err(acp::Error::internal_error().data(message));
        }

        // Never compact mid-salvage: the rewrite would drop the continue reminder and split the joined report
        // Genuine overflows already completed truncated in the quiet arm above
        // The remaining mid-salvage kinds (rate limit) take their terminal arms below
        if !mid_salvage_continuation && self.should_compact_on_error(&error).await {
            // SAFETY: `should_compact_on_error` returned true only when `model_metadata.context_window` was Some(>0)
            let cw = error
                .model_metadata
                .as_ref()
                .and_then(|m| m.context_window)
                .expect("should_compact_on_error guarantees context_window");
            {
                let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
                let percentage = distill_token_estimation::usage_percentage_u8(total_tokens, cw);

                // Update the in-memory sampling config's `context_window` if the model reported a different value (mirror the legacy path's bookkeeping)
                if let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await
                    && let Some(new_cw) = std::num::NonZeroU64::new(cw)
                    && self.compaction.context_window_override.is_none()
                {
                    cfg.context_window = new_cw;
                    self.chat_state_handle.update_sampling_config(cfg);
                }

                let trigger_info = compaction::AutoCompactTriggerInfo {
                    tokens_used: total_tokens,
                    context_window: cw,
                    percentage,
                    reason_override: None,
                };
                if let Err(e) = self.run_compact_only(trigger_info, false).await {
                    if Self::is_auth_compact_error(&e) {
                        return Err(self.surface_compact_auth_failure(e).await);
                    }
                    return Err(e);
                }
                return Ok(SamplerFailureRecovery::CompactAndResubmit);
            }
        }

        // Telemetry and notification for terminal failures
        // The drainer already recorded `record_error_typed` from the `SamplingEvent::Failed` event
        // Here we send the `RetryState::Failed` notification, which the drainer intentionally skips because it would fire mid-retry
        let detailed_message = error.message.clone();

        // 2. Encrypted-content mismatch: friendly error, no retry.
        //    Detect via the BadRequest and "encrypted_content" message pattern that `SamplingError::is_encrypted_content_error` used in the legacy path
        if encrypted_content_mismatch {
            self.signals_handle()
                .record_error_typed("encrypted_content_mismatch");
            let friendly = "This session's conversation history is incompatible \
                            with the current model. Please start a new session."
                .to_string();
            self.log_terminal_failure("encrypted_content_mismatch", error.status_code, &friendly);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "encrypted_content_mismatch".to_string(),
                    message: friendly.clone(),
                },
            ))
            .await;
            return Err(acp::Error::invalid_params().data(friendly));
        }

        if matches!(error.kind, SamplingErrorKind::RateLimited) {
            self.log_terminal_failure("rate_limited", error.status_code, &detailed_message);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Exhausted {
                    attempts: rate_limit_waits,
                    reason: detailed_message.clone(),
                    is_rate_limited: true,
                },
            ))
            .await;
            let acp_err = acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "Rate limited".to_string(),
            )
            .data(detailed_message);
            return Err(acp_err);
        }

        // Auth-401 recovery only applies to refreshable session-token auth (the stable gate, not `creds.auth_type`).
        // A static api-key isn't refreshable, so retrying re-sends the same rejected bearer and 401-loops the turn See `crate::agent::auth_method::session_token_auth_gate`.
        // One sampling-config snapshot drives every arm-4 decision, so the provider resolution and the gate can't disagree mid-model-switch.
        let (failed_model_id, failed_base_url) = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| (c.model, c.base_url))
            .unwrap_or_default();

        // Provider-backed models recover via arm 4c below
        // The provider is resolved before the eligibility check so its warnings stay quiet for a 401 that 4c handles
        let auth_provider =
            if matches!(error.kind, SamplingErrorKind::Auth) || error.status_code == Some(401) {
                self.model_auth_provider(&failed_model_id)
            } else {
                None
            };

        let auth_recovery_eligible = matches!(error.kind, SamplingErrorKind::Auth) && {
            let gate = self.auth_gate(&failed_model_id, &failed_base_url);
            let eligible = gate.active();
            // Log the Unknown-BYOK decision (eligible or not) so a session still 401ing shows whether refresh fired
            self.log_auth_gate_unknown("handle_sampling_failure", gate, &failed_base_url);
            if !eligible && auth_provider.is_none() {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    is_session_based = gate.is_session_based,
                    model_byok = gate.model_byok.as_ref(),
                    endpoint_is_first_party = gate.endpoint_is_first_party,
                    "auth recovery: sampler 401 not refreshable (api-key auth) — surfacing 401",
                );
                distill_telemetry::unified_log::warn(
                    "auth recovery: sampler 401 not eligible (api-key auth)",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "kind": error.kind.as_ref(),
                        "status_code": error.status_code,
                        "is_session_based": gate.is_session_based,
                        "model_byok": gate.model_byok.as_ref(),
                        "endpoint_is_first_party": gate.endpoint_is_first_party,
                    })),
                );
            }
            eligible
        };

        // A provider-backed model is BYOK, so its gate is inactive and the session arm (4b) can't fire; provider recovery (4c) is exclusive
        // Assert it so a future gate change trips here, not double-recovers.
        debug_assert!(
            !(auth_recovery_eligible && auth_provider.is_some()),
            "a provider-backed model must not be session-recovery-eligible"
        );

        // Observability: a 401 that did NOT classify as `Auth` kind bypasses the session arm 4b; only provider-backed models recover (4c)
        // Make that decision visible; it is otherwise indistinguishable from a failed refresh in the unified log
        if !matches!(error.kind, SamplingErrorKind::Auth)
            && error.status_code == Some(401)
            && auth_provider.is_none()
        {
            distill_telemetry::unified_log::warn(
                "auth recovery: sampler 401 not eligible (non-auth error kind)",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "kind": error.kind.as_ref(),
                    "status_code": error.status_code,
                })),
            );
        }

        // Last-resort recovery used to short-circuit on whatever is in memory, so it reported success with the bearer the server had just rejected.
        // That bearer was resubmitted until the turn's retry budget ran out. Recovery already walks disk adoption and the authority first.
        if auth_recovery_eligible && let Some(ref am) = self.auth_manager {
            // Already parked and still credential-less: re-park without a recovery dispatch.
            // A parked turn makes at most one recovery attempt (the pre-park one): each dispatch is up to two counted.
            // IdP attempts and `OidcRefresher` escalates five into a process-wide permanent verdict, so 50 parked cycles must not drive refreshes — the escalation-aware proactive loop owns refresh while parked.
            if park.is_parked() && self.uncharged_401_park_eligible(am, error.credential) {
                return Ok(self.park_uncharged_401(error.credential, true));
            }
            if am
                .try_recover_unauthorized(distill_login::recovery::RecoverySource::Turn)
                .await
            {
                tracing::info!(session_id = %self.session_info.id.0, "auth recovery: sampler 401, recovered, retrying");
                distill_telemetry::unified_log::info(
                    "auth recovery: sampler 401, recovered, retrying",
                    Some(self.session_info.id.0.as_ref()),
                    None,
                );
                self.prepare_sampler_for_turn().await;
                return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                    credential: error.credential,
                    store: RecoveredStore::SessionToken,
                });
            }
            tracing::warn!(session_id = %self.session_info.id.0, "auth recovery: sampler 401, refresh failed");
            // `park_enabled` makes a kill-switch flip observable; `credential`
            // distinguishes a Sent/Unknown refusal from a remedy refusal.
            distill_telemetry::unified_log::warn(
                "auth recovery: sampler 401, refresh failed",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "park_enabled": self.uncharged_401_park_enabled,
                    "credential": error.credential,
                })),
            );
            // The 401 says nothing about the missing credential: park instead of failing the turn.
            // Do not add `prepare_sampler_for_turn` here — a prepare's refresh would count against the shared escalation budget.
            if self.uncharged_401_park_eligible(am, error.credential) {
                return Ok(self.park_uncharged_401(error.credential, false));
            }
        }

        // 4c. Auth failure or bare 401 on a provider-backed model (gateway 401s can classify under other error kinds).
        if let Some(ref provider) = auth_provider
            && self.try_provider_401_recovery(provider).await
        {
            self.prepare_sampler_for_turn().await;
            return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                credential: error.credential,
                store: RecoveredStore::AuthProvider,
            });
        }

        // 4d. Bounded resubmit, after the auth arms, before the terminal paths.
        //     Budgeted workflow children stay terminal (guards above)
        if transient_retry_eligible(&error) && transient.enabled {
            if transient.budget_remaining() {
                // Count intercepted attempts; section 5 sees only the final one.
                if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
                    self.signals_handle().record_idle_timeout();
                }
                return Ok(SamplerFailureRecovery::RetryTransient {
                    kind: error.kind,
                    status_code: error.status_code,
                });
            }
            distill_telemetry::unified_log::error(
                "shell.turn.transient_retry_exhausted",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "kind": error.kind.as_ref(),
                    "status_code": error.status_code,
                    "step_retries_used": transient.step_attempts,
                    "prompt_retries_used": transient.prompt_attempts,
                    "episode_elapsed_ms": transient
                        .episode_start
                        .map_or(0, |s| s.elapsed().as_millis() as u64),
                    "max_retries":
                        transient_display_ceiling(transient.step_attempts, transient.prompt_attempts),
                })),
            );
        }

        // 5. Idle timeout: record it; the generic notification and the fatal error follow.
        if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
            self.signals_handle().record_idle_timeout();
        }

        // 5b.
        // Empty response: log structured context for debugging.
        // The model completed the stream but produced no visible content This is common with reasoning-heavy models after a successful tool call Log the full context so this is diagnosable from telemetry.
        if matches!(error.kind, SamplingErrorKind::EmptyResponse) {
            if let Some(ref ctx) = error.empty_response_context {
                tracing::warn!(
                    empty_response = true,
                    empty_reason = ctx.reason.as_ref(),
                    had_reasoning = ctx.had_reasoning,
                    content_len = ctx.content_len,
                    tool_call_count = ctx.tool_call_count,
                    completion_tokens = ctx.completion_tokens.unwrap_or(0),
                    reasoning_tokens = ctx.reasoning_tokens.unwrap_or(0),
                    finish_reason = ctx.finish_reason_str(),
                    first_choice_seen = ctx.first_choice_seen,
                    model = %ctx.model,
                    "empty response after retries exhausted: {reason}",
                    reason = ctx.reason,
                );
                // Stamp the doomloop magnitude onto the out-of-band capture
                // `streaming_partial.json` then records reasoning-token volume even when the reasoning text itself was clipped at the byte cap
                // This runs on the turn loop before the turn-end take, so the counts are included when the capture is uploaded
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    cap.reasoning_tokens = ctx.reasoning_tokens;
                    cap.completion_tokens = ctx.completion_tokens;
                    cap.finish_reason = ctx.finish_reason.clone();
                    cap.empty_reason = Some(ctx.reason.as_ref().to_owned());
                }
            }
            self.signals_handle().record_error_typed("empty_response");
        }

        // 5c and 5d. Auth diagnostics for sampling failures.
        let auth_mode = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired())
            .map(|a| a.auth_mode)
            .unwrap_or(distill_login::AuthMode::ApiKey);
        let auth_mode_str = format!("{auth_mode:?}");
        let client_version = distill_version::VERSION;

        // 5c. Legacy WebLogin auth: always show a deprecation message regardless of error type.
        if auth_mode == distill_login::AuthMode::WebLogin {
            let msg = format!(
                "{detailed_message}\n\n\
                 You are using a deprecated authentication method (WebLogin).\n\
                 This auth method is no longer supported and will cause errors.\n\n\
                 To fix: run `grok update`, then `grok logout`, then `grok login` to re-authenticate with OAuth2.\n\n\
                 Version: {client_version}"
            );
            self.log_terminal_failure("legacy_auth", error.status_code, &msg);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "legacy_auth".to_string(),
                    message: msg.clone(),
                },
            ))
            .await;
            return Err(acp::Error::internal_error().data(msg));
        }

        // 5d. Enriched error for 404 model-not-found and 401 auth errors.
        //     Includes auth mode, client version, and available models.
        let is_model_404 =
            error.status_code == Some(404) && detailed_message.contains("does not exist");
        let is_auth_401 =
            error.status_code == Some(401) || matches!(error.kind, SamplingErrorKind::Auth);

        let detailed_message = if is_model_404 || is_auth_401 {
            let current_model = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_else(|| "unknown".to_string());

            let available: Vec<String> = self
                .models_manager
                .models()
                .values()
                .map(|m| m.model.clone())
                .collect();

            let mut msg = format!("{detailed_message}\n");
            msg.push_str(&format!("\n  Model:     {current_model}"));
            msg.push_str(&format!("\n  Auth:      {auth_mode_str}"));
            if let Some(ref provider) = auth_provider {
                msg.push_str(&format!(
                    "\n  Provider:  [auth_provider.{}] (check the provider command and the debug log)",
                    provider.name
                ));
            }
            msg.push_str(&format!("\n  Version:   {client_version}"));
            if available.is_empty() {
                msg.push_str("\n  Available: (none)");
            } else {
                msg.push_str(&format!("\n  Available: {}", available.join(", ")));
            }

            if is_model_404 && !available.iter().any(|m| m == &current_model) {
                msg.push_str(&format!(
                    "\n\n  '{}' is not in your available models.",
                    current_model
                ));
                msg.push_str("\n  Switch models with /model or start a new session.");
            }

            msg
        } else {
            detailed_message
        };

        // Generic terminal error.
        // Tag context-window overflow distinctly so the pager can collapse it into one actionable prompt.
        // Structured size code first, message text as fallback — same order as the compaction classifiers.
        let error_type = if error
            .error_code
            .as_ref()
            .is_some_and(distill_sampling_types::ApiErrorCode::is_size_overflow)
            || distill_sampling_types::is_context_length_error(&error.message)
        {
            crate::extensions::notification::CONTEXT_LENGTH_ERROR_TYPE
        } else {
            error.kind.as_ref()
        };
        let (error_type, detailed_message) = match self.auth_manager.as_ref() {
            Some(auth_manager) if error_type == "auth" => self.apply_auth_remedy(
                &auth_manager.auth_remedy(),
                detailed_message,
                error.status_code,
            ),
            _ => (error_type, detailed_message),
        };
        self.log_terminal_failure(error_type, error.status_code, &detailed_message);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_string(),
                message: detailed_message.clone(),
            },
        ))
        .await;
        Err(
            acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
                detailed_message,
                error.status_code,
                error.kind,
            )),
        )
    }

    async fn wait_for_stream_drain(
        &self,
        request_id: &distill_sampler::RequestId,
        stream_drained_rx: tokio::sync::oneshot::Receiver<()>,
        timeout_message: &'static str,
    ) -> StreamDrainOutcome {
        let outcome = stream_drain_outcome(stream_drained_rx).await;
        if outcome == StreamDrainOutcome::TimedOut {
            self.mark_stream_drain_timed_out(request_id);
            tracing::warn!("{timeout_message}");
        }
        outcome
    }

    fn apply_pending_sampler_config(
        &self,
        request: &mut ConversationRequest,
        routed_local: &mut bool,
    ) {
        if let Some(effort) = self.jev_ledger.borrow_mut().take_pending_request_effort() {
            request.reasoning_effort = effort;
        }
        if let Some((routed_model, is_local_route)) =
            self.jev_ledger.borrow_mut().take_pending_route()
        {
            request.model = Some(routed_model);
            *routed_local = is_local_route;
            if is_local_route {
                // Keep Reasoning on the request: a rejected local round
                // resubmits this same history, and thinking-mode models 400
                // unless `reasoning_content` is passed back.
                request.reasoning_effort = None;
            }
        }
    }

    /// Drive one turn through the sampler, pacing a subagent's 429s via `budget`.
    /// `Parked` resubmits skip every refresh-driving prepare (see the re-park arm);
    /// the wire bearer comes from the live resolver at send time.
    pub(crate) async fn run_turn_via_sampler(
        self: &Arc<Self>,
        mut request: ConversationRequest,
        budget: &mut RateLimitWaitBudget,
        transient: TransientRetryState,
        mid_salvage_continuation: bool,
        park: TurnParkState,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        // Per-turn auth refresh + sampler config push. Mirrors
        // `prepare_chat_completion(false)` from the legacy path.
        if !park.is_parked() {
            self.prepare_sampler_for_turn().await;
        }
        // A round the decision layer moved (e.g. to the local model) must name
        // that model in the request: the chat state built it with the session's.
        let mut routed_local = false;
        self.apply_pending_sampler_config(&mut request, &mut routed_local);

        if !budget.can_wait() {
            if routed_local {
                // Main sessions normally do not own a 429 wait budget, but a
                // local optimization still has the same one-shot rejection
                // fallback as a child. Retain the request until the local
                // endpoint has accepted it or the base-model replacement has
                // been attempted.
                return match self.submit_turn_request(request.clone()).await {
                    Ok(outcome) => Ok(outcome),
                    Err(info) if is_client_rejection(&info) => {
                        self.undo_local_route(&mut request, &info).await;
                        match self.submit_turn_request(request).await {
                            Ok(outcome) => Ok(outcome),
                            Err(info) => {
                                self.recover_from_sampling_failure(
                                    info,
                                    budget,
                                    transient,
                                    mid_salvage_continuation,
                                    park,
                                )
                                .await
                            }
                        }
                    }
                    Err(info) => {
                        self.recover_from_sampling_failure(
                            info,
                            budget,
                            transient,
                            mid_salvage_continuation,
                            park,
                        )
                        .await
                    }
                };
            }
            // Nothing will send this request a second time, so move it into the sampler instead of deep-cloning the whole message history on every main-session turn
            return match self.submit_turn_request(request).await {
                Ok(outcome) => Ok(outcome),
                Err(info) => {
                    self.recover_from_sampling_failure(
                        info,
                        budget,
                        transient,
                        mid_salvage_continuation,
                        park,
                    )
                    .await
                }
            };
        }

        loop {
            match self.submit_turn_request(request.clone()).await {
                Ok(outcome) => {
                    budget.record_submission_accepted();
                    return Ok(outcome);
                }
                Err(info) => {
                    // The endpoint behind a routed round may refuse the request
                    // (observed live: a local server rejecting the session
                    // model's reasoning payload). The routing is an optimization,
                    // so the round goes back to the session model and the rest
                    // of the turn stays there.
                    if routed_local && is_client_rejection(&info) {
                        self.undo_local_route(&mut request, &info).await;
                        routed_local = false;
                        continue;
                    }
                    let decision = budget.decide(&info);
                    let RateLimitWaitDecision::Wait { attempt, backoff } = decision else {
                        self.log_rate_limit_budget_spent(decision, &info);
                        return self
                            .recover_from_sampling_failure(
                                info,
                                budget,
                                transient,
                                mid_salvage_continuation,
                                park,
                            )
                            .await;
                    };
                    self.notify_rate_limit_wait(attempt, budget, backoff).await;
                    // Esc cancels a turn by aborting its task, so this await point is itself the cancellation point; no select needed
                    sleep(backoff).await;
                    // A token can expire across minutes of accumulated waits
                    // (parked turns skip it — see `run_turn_via_sampler`).
                    if !park.is_parked() {
                        self.prepare_sampler_for_turn().await;
                        self.apply_pending_sampler_config(&mut request, &mut routed_local);
                    }
                    self.turn_phases.record_sampling_retries(1);
                }
            }
        }
    }

    async fn submit_turn_request(
        self: &Arc<Self>,
        mut request: ConversationRequest,
    ) -> Result<SamplerTurnOutcome, distill_sampler::SamplingErrorInfo> {
        let request_id = distill_sampler::RequestId::random();
        let request_id_str = request_id.as_str().to_string();
        let usage_context = self.sampler_usage_context(&request_id_str, &request).await;
        self.turn_phases.record_sampling_request();
        let _sampling_phase = self.turn_phases.begin_sampling();
        let stream_drained_rx = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.turn_stream_drained.lock().insert(
                request_id.clone(),
                crate::session::acp_session::StreamOwnership {
                    generation: self.turn_phases.current_generation(),
                    waiter: Some(tx),
                    usage_context: Some(usage_context.clone()),
                },
            );
            rx
        };

        let collected = {
            let gate_span = region!("turn.sampling_gate", Parent::Inherit);
            let _permit = acquire_subagent_sampling_permit(&self.sampling_gate).await;
            gate_span.close();
            let sampling_span = region!("turn.sampling", Parent::Inherit);
            // The sampler task has no tracing ancestor; this parents its HTTP span under the region
            // without holding it open.
            request.traceparent = distill_otel::span_traceparent(sampling_span.span());
            self.sampler_handle
                .submit_and_collect_with_metadata(request_id.clone(), request)
                .await
        };
        let terminal_event_queued = collected.terminal_event_queued;
        let recovery_attempts = collected.doom_loop_recovery_attempts;
        let recovery_attempt_count = u32::try_from(recovery_attempts.len()).unwrap_or(u32::MAX);
        {
            // Cancel clears ownership before resetting the tally
            // Holding the ownership lock through reconciliation makes that boundary atomic
            // Either metadata lands before cancel and is then cleared, or cancel wins and this stale result cannot rebook the next turn
            let ownership = self.turn_stream_drained.lock();
            let counted = crate::session::doom_loop_telemetry::reconcile_request_metadata(
                &mut self.doom_loop_turn_tally.lock(),
                ownership.contains_key(&request_id),
                &request_id_str,
                &collected.doom_loop_signals,
                &recovery_attempts,
            );
            for (triggers, aborted_at_chunk) in counted {
                self.signals_handle()
                    .record_doom_loop_recovery_attempt(triggers, aborted_at_chunk);
            }
        }
        match collected.result {
            Ok((response, metrics)) => {
                // Current span is the turn span (this fn is inline-awaited from process_conversation_turn, no own #[instrument])
                let span = tracing::Span::current();
                span.record("request_id", request_id_str.as_str());
                if let Some(ttft) = metrics.time_to_first_token_ms {
                    span.record("ttft_ms", ttft as i64);
                }
                if metrics.attempts > 0 {
                    span.record("attempt", i64::from(metrics.attempts));
                }
                // A rewind/cancel clears ownership and drops the waiter
                // Do not commit that pre-boundary success or its metrics onto the restored turn
                // A real timeout remains fail-open
                if terminal_event_queued {
                    let _drain_span = region!("turn.stream_drain_barrier", Parent::Inherit);
                    if self
                        .wait_for_stream_drain(
                            &request_id,
                            stream_drained_rx,
                            "stream-drain barrier timed out; proceeding to emit tool calls",
                        )
                        .await
                        == StreamDrainOutcome::Revoked
                    {
                        let mut usage_context = usage_context.clone();
                        usage_context.update_from_response(&response);
                        self.chat_state_handle.record_usage_attribution(
                            usage_context.into_attribution(
                                distill_chat_state::UsageCallStatus::Cancelled,
                                response.usage.clone(),
                                usage_is_complete(response.usage.as_ref()),
                                Some(metrics.time_to_last_byte_ms),
                                response.cost_usd_ticks,
                            ),
                            false,
                        );
                        return Err(revoked_sampling_info());
                    }
                } else if !self.turn_stream_drained.lock().contains_key(&request_id) {
                    let mut usage_context = usage_context.clone();
                    usage_context.update_from_response(&response);
                    self.chat_state_handle.record_usage_attribution(
                        usage_context.into_attribution(
                            distill_chat_state::UsageCallStatus::Cancelled,
                            response.usage.clone(),
                            usage_is_complete(response.usage.as_ref()),
                            Some(metrics.time_to_last_byte_ms),
                            response.cost_usd_ticks,
                        ),
                        false,
                    );
                    return Err(revoked_sampling_info());
                }

                // The awaited result is authoritative for successful-request accounting once the request survives the turn boundary
                self.record_api_request_time();
                self.signals_handle()
                    .record_inference_metrics(metrics.clone());
                let confident_triggers = self
                    .doom_loop_recovery
                    .map(|policy| policy.confident_triggers(&response.doom_loop_signals))
                    .unwrap_or_default();
                if recovery_attempt_count > 0 && !confident_triggers.is_empty() {
                    let should_record = {
                        // Take the ownership lock, then the tally lock, the same order cancel uses to clear ownership and reset the tally
                        let ownership = self.turn_stream_drained.lock();
                        if ownership.contains_key(&request_id) {
                            let mut tally = self.doom_loop_turn_tally.lock();
                            let inserted = tally.mark_accepted_request(&request_id_str);
                            if inserted {
                                tally.merge_recovery_triggers(&confident_triggers);
                            }
                            inserted
                        } else {
                            false
                        }
                    };
                    if should_record {
                        self.signals_handle()
                            .record_doom_loop_accepted_after_budget(confident_triggers);
                    }
                }
                // A queued terminal event already released ownership when it was acknowledged above
                // Without one, release ownership after authoritative metadata accounting
                if !terminal_event_queued {
                    self.turn_stream_drained.lock().remove(&request_id);
                }
                let mut usage_context = usage_context;
                usage_context.update_from_response(&response);
                Ok(SamplerTurnOutcome::Response(
                    Box::new(response),
                    Box::new(metrics),
                    usage_context,
                ))
            }
            Err(rich_err) => {
                // Detector labels are already merged from the awaited result.
                // Wait briefly for the UI/error event rail, then fail open so a stuck drainer cannot prevent recovery or turn teardown
                let original = distill_sampler::SamplingErrorInfo::from(&rich_err);
                let outcome = if terminal_event_queued {
                    self.wait_for_stream_drain(
                        &request_id,
                        stream_drained_rx,
                        "failed-event drain barrier timed out; continuing recovery",
                    )
                    .await
                } else if self
                    .turn_stream_drained
                    .lock()
                    .remove(&request_id)
                    .is_some()
                {
                    StreamDrainOutcome::Acknowledged
                } else {
                    StreamDrainOutcome::Revoked
                };
                let final_info = error_after_stream_drain(outcome, original);
                let status = if outcome == StreamDrainOutcome::Revoked {
                    distill_chat_state::UsageCallStatus::Cancelled
                } else if is_client_rejection(&final_info) {
                    distill_chat_state::UsageCallStatus::Rejected
                } else {
                    distill_chat_state::UsageCallStatus::Failed
                };
                self.chat_state_handle.record_usage_attribution(
                    usage_context.into_attribution(status, None, false, None, None),
                    true,
                );
                Err(final_info)
            }
        }
    }

    async fn sampler_usage_context(
        &self,
        request_id: &str,
        request: &ConversationRequest,
    ) -> UsageAttemptContext {
        let config = self.chat_state_handle.get_sampling_config().await;
        let configured_model = config
            .as_ref()
            .map(|config| config.model.clone())
            .unwrap_or_default();
        let api_backend = config
            .as_ref()
            .map(|config| config.api_backend.clone())
            .unwrap_or_default();
        let reasoning_shape = config
            .as_ref()
            .map(|config| config.reasoning_shape)
            .unwrap_or_default();
        let configured_effort = request.reasoning_effort.or_else(|| {
            config.as_ref().and_then(|config| config.reasoning_effort)
        });
        let max_output_tokens = request.max_output_tokens.or_else(|| {
            config
                .as_ref()
                .and_then(|config| config.max_completion_tokens)
        });
        let model_id = request
            .model
            .clone()
            .filter(|model| !model.is_empty())
            .or_else(|| (!configured_model.is_empty()).then_some(configured_model))
            .unwrap_or_else(|| "<unknown>".to_owned());
        let task_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        UsageAttemptContext {
            attempt_id: format!("sampler:{request_id}"),
            task_id,
            turn_id: Some(self.current_turn_number.get().to_string()),
            request_id: None,
            role: "main".to_owned(),
            model_id,
            endpoint: config.as_ref().and_then(sampler_attribution_endpoint),
            requested_effort: configured_effort.map(|effort| effort.to_string()),
            applied_effort: distill_sampling_types::transmitted_reasoning_effort(
                api_backend,
                reasoning_shape,
                configured_effort,
                max_output_tokens,
            ),
        }
    }

    async fn recover_from_sampling_failure(
        self: &Arc<Self>,
        info: distill_sampler::SamplingErrorInfo,
        budget: &RateLimitWaitBudget,
        transient: TransientRetryState,
        mid_salvage_continuation: bool,
        park: TurnParkState,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        // Single funnel for every sampler-call failure.
        super::turn::record_failed_sample_on_turn_span(&tracing::Span::current(), info.kind);
        match self
            .handle_sampling_failure(
                info,
                budget.attempts_used(),
                transient,
                mid_salvage_continuation,
                park,
            )
            .await?
        {
            SamplerFailureRecovery::CompactAndResubmit => {
                Ok(SamplerTurnOutcome::CompactAndResubmit)
            }
            SamplerFailureRecovery::RefreshAuthAndResubmit { credential, store } => {
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { credential, store })
            }
            SamplerFailureRecovery::RetryTransient { kind, status_code } => {
                Ok(SamplerTurnOutcome::RetryTransient { kind, status_code })
            }
        }
    }

    /// Mirror the auth-retry path's `RetryState::Retrying` marker so the paced wait is observable to the client.
    async fn notify_rate_limit_wait(
        &self,
        attempt: u32,
        budget: &RateLimitWaitBudget,
        backoff: Duration,
    ) {
        // Debug, not warn: a burst of subagents would otherwise flood the log.
        tracing::debug!(
            attempt,
            delay_ms = backoff.as_millis() as u64,
            "subagent turn rate limited; waiting for sampling capacity"
        );
        // Per-wait unified-log marker so each pause is visible in session logs like auth backoff
        // The terminal give-up alone (the `subagent_rate_limit_exhausted` marker) is not enough
        distill_telemetry::unified_log::info(
            "shell.turn.subagent_rate_limit_backoff",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "attempt": attempt,
                "max_attempts": budget.max_attempts(),
                "delay_ms": backoff.as_millis() as u64,
            })),
        );
        // Whole seconds with a one-second floor: a sub-second jittered wait would otherwise render as "waiting 0s"
        let announced = Duration::from_secs(backoff.as_secs_f64().round().max(1.0) as u64);
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Retrying {
                attempt,
                max_retries: budget.max_attempts(),
                reason: format!(
                    "Too many requests in flight; waiting {} before trying again",
                    human_duration(announced)
                ),
                error_type: None,
            },
        ))
        .await;
    }

    fn log_rate_limit_budget_spent(
        &self,
        decision: RateLimitWaitDecision,
        error: &distill_sampler::SamplingErrorInfo,
    ) {
        let RateLimitWaitDecision::BudgetSpent { attempts, limit } = decision else {
            return;
        };
        tracing::warn!(
            attempts,
            cause = limit.as_ref(),
            retry_after_secs = ?error.retry_after_secs,
            "subagent stopped waiting out rate limits; failing the turn"
        );
        distill_telemetry::unified_log::warn(
            "shell.turn.subagent_rate_limit_exhausted",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "attempts": attempts,
                "cause": limit.as_ref(),
                "retry_after_secs": error.retry_after_secs,
                "status_code": error.status_code,
            })),
        );
    }

    /// Proactively refresh the auth token if near expiry.
    /// Session-token path is best-effort: on success, update credentials and return.
    /// On failure, do not fall through to the JWT/config.toml branch when the session gate was active; that path is for BYOK JWTs only.
    pub(crate) async fn refresh_token_if_expired(&self) {
        if let Some(ref am) = self.auth_manager {
            let creds = self.chat_state_handle.get_credentials().await;
            // Gate on the stable classifier, not `creds.auth_type`; this also heals a transient `ApiKey` flip by writing the refreshed token into `creds.api_key` below
            // See `crate::agent::auth_method::session_token_auth_gate`
            let (model_id, base_url) = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| (c.model, c.base_url))
                .unwrap_or_default();
            // Same condition as the seed guard above, so without this the re-seed puts the key back.
            if self.auth_gate(&model_id, &base_url).active()
                && ActiveAuthBackend::default().is_xai_authority()
            {
                match am.get_valid_token().await {
                    Ok(key) => {
                        if creds.api_key.as_deref() != Some(&key) {
                            let mut creds = creds;
                            creds.api_key = Some(key);
                            self.chat_state_handle.update_credentials(creds);
                        }
                        // Re-enable auth-suppressed compact; waiting for a 200 deadlocks over-window
                        self.clear_auth_compact_suppression();
                        return;
                    }
                    Err(e) => {
                        // Session path applied and failed
                        // Never fall through to JWT/config.toml reload (that branch is BYOK-only)
                        // Hard-expired: strip the chat-state seed so default headers cannot carry a dead AT (resolver is wire-valid only)
                        let hard_expired = !am.has_usable_token();
                        if hard_expired && creds.api_key.is_some() {
                            let mut cleared = creds;
                            cleared.api_key = None;
                            self.chat_state_handle.update_credentials(cleared);
                        }
                        tracing::warn!(
                            error = %e,
                            hard_expired,
                            model = %model_id,
                            "auth: preflight get_valid_token failed"
                        );
                        distill_telemetry::unified_log::warn(
                            "auth.preflight.refresh_failed",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "error": format!("{e}"),
                                "hard_expired": hard_expired,
                                "model": model_id,
                            })),
                        );
                        return;
                    }
                }
            }
        } else {
            distill_telemetry::unified_log::debug(
                "token refresh skipped: no auth manager",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }

        // JWT from config.toml: a separate mechanism for BYOK tokens
        use distill_login::{is_jwt_expired_or_near, parse_jwt_expiration};

        const REFRESH_THRESHOLD: chrono::Duration = chrono::Duration::minutes(5);

        let creds = self.chat_state_handle.get_credentials().await;
        let current_key = creds.api_key;
        let current_model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();

        // Provider-backed models mint and refresh here so `resolve_credentials` stays cache-only
        if let Some(provider) = self.model_auth_provider(&current_model_id) {
            self.refresh_provider_token_pre_turn(
                &provider,
                current_key.as_deref(),
                &current_model_id,
            )
            .await;
            // Provider models carry no session JWT, so skip the JWT refresh below.
            return;
        }

        let Some(ref key) = current_key else { return };

        if !is_jwt_expired_or_near(key, REFRESH_THRESHOLD) {
            if let Some(exp) = parse_jwt_expiration(key) {
                let remaining_secs = (exp - chrono::Utc::now()).num_seconds();
                tracing::debug!(
                    model = %current_model_id,
                    remaining_secs,
                    "JWT token valid, no refresh needed"
                );
            } else {
                tracing::debug!(
                    model = %current_model_id,
                    key_len = key.len(),
                    "Token is not a JWT, expiry-based refresh not applicable"
                );
            }
            return;
        }

        // is_jwt_expired_or_near(true) guarantees parse_jwt_expiration is Some
        let remaining_secs =
            parse_jwt_expiration(key).map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = %current_model_id,
            remaining_secs,
            "JWT near expiry, refreshing from config.toml"
        );

        let Some(new_key) = self.reload_api_key_from_config(&current_model_id) else {
            return; // specific failure already logged
        };

        if key == &new_key {
            tracing::warn!(
                model = %current_model_id,
                "Config.toml returned same token (not yet rotated by external process?)"
            );
            return;
        }

        let new_remaining_secs = parse_jwt_expiration(&new_key)
            .map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = %current_model_id,
            new_remaining_secs,
            key_len = new_key.len(),
            "Refreshed API token from config.toml"
        );

        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }

    fn reload_api_key_from_config(&self, current_model_id: &str) -> Option<String> {
        let raw_config = crate::config::load_effective_config()
            .map_err(|e| tracing::warn!(error = %e, "Failed to reload config"))
            .ok()?;

        let config = crate::agent::config::Config::new_from_toml_cfg(&raw_config)
            .map_err(|e| tracing::warn!(error = %e, "Failed to parse reloaded config.toml"))
            .ok()?;

        let config_model = config
            .config_models
            .iter()
            .find(|(k, v)| v.model.as_deref().unwrap_or(k.as_str()) == current_model_id)
            .map(|(_, v)| v);

        let Some(model) = config_model else {
            tracing::warn!(
                model = %current_model_id,
                available = ?config.config_models.keys().collect::<Vec<_>>(),
                "Model not found in config.toml [model.*]"
            );
            return None;
        };

        let key = crate::agent::config::first_own_credential(
            model.api_key.as_deref(),
            model.env_key.as_ref(),
        );

        if key.is_none() {
            tracing::warn!(
                model = %current_model_id,
                env_key = ?model.env_key,
                "No api_key or env_key resolved for model"
            );
        }

        key
    }

    /// Propagate the model-reported token usage from a turn response into chat state, the per-prompt usage ledger, and per-turn signals.
    /// Without it `state.total_tokens` would stay frozen at the `estimate_conversation_tokens` seed from `ChatState::new`.
    /// Resetting `estimated_tokens_since_model = 0` here also keeps the preflight-overflow guard accurate against the next turn's tool-result deltas.
    pub(crate) fn record_response_token_usage(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
    ) {
        let model_id = response
            .assistant()
            .and_then(|assistant| assistant.model_id.clone())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| "<unknown>".to_owned());
        self.record_response_token_usage_with_context(
            response,
            api_duration_ms,
            UsageAttemptContext {
                attempt_id: format!("response:{}", uuid::Uuid::new_v4()),
                task_id: None,
                turn_id: None,
                request_id: response.message_id.clone(),
                role: "main".to_owned(),
                model_id,
                endpoint: None,
                requested_effort: None,
                applied_effort: None,
            },
        );
    }

    pub(crate) fn record_response_token_usage_with_context(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
        mut usage_context: UsageAttemptContext,
    ) {
        usage_context.update_from_response(response);
        if let Some(ref u) = response.usage {
            self.tool_context
                .record_task_model_output(u64::from(u.completion_tokens));
            self.chat_state_handle
                .record_token_usage(u64::from(u.total_tokens));
            self.chat_state_handle.record_last_turn_usage(u.clone());
            self.chat_state_handle.record_usage_attribution(
                usage_context.into_attribution(
                    distill_chat_state::UsageCallStatus::Completed,
                    Some(u.clone()),
                    usage_is_complete(response.usage.as_ref()),
                    api_duration_ms,
                    response.cost_usd_ticks,
                ),
                true,
            );
            self.signals_handle()
                .record_token_usage(u.completion_tokens, u.reasoning_tokens);
        } else {
            self.chat_state_handle.record_usage_attribution(
                usage_context.into_attribution(
                    distill_chat_state::UsageCallStatus::Completed,
                    None,
                    false,
                    api_duration_ms,
                    response.cost_usd_ticks,
                ),
                true,
            );
            if self.tool_context.task_output_token_budget.is_some() {
                self.tool_context.fail_task_output_usage_closed();
                self.chat_state_handle
                    .mark_usage_incomplete_nowait(true, true);
            } else if self.tool_context.sampler_retry_only_before_output {
                self.chat_state_handle
                    .mark_usage_incomplete_nowait(true, true);
            }
        }
    }

    /// Persist one response's items without re-estimating model output when provider usage already includes it.
    pub(super) async fn record_response_items(
        &self,
        items: Vec<ConversationItem>,
        usage_reported: bool,
    ) {
        for item in items {
            match item {
                ConversationItem::Assistant(_) => {
                    self.record_assistant_response(item, usage_reported).await;
                }
                _ if usage_reported => self.chat_state_handle.push_model_output(item),
                _ => self.chat_state_handle.push_tool_result(item),
            }
        }
    }

    pub(super) async fn record_assistant_response(
        &self,
        assistant_item: ConversationItem,
        usage_reported: bool,
    ) {
        self.signals_handle().record_assistant_message();

        // DEBUG: Log the model_id on the assistant item being recorded
        if let ConversationItem::Assistant(ref a) = assistant_item {
            tracing::info!(model_id = ?a.model_id, "DEBUG record_assistant_response model_id");
        }

        if let ConversationItem::Assistant(ref a) = assistant_item
            && let Some(first_call) = a.tool_calls.first()
        {
            tracing::info!("Assistant requested tool call: {}", first_call.id);
        }

        if usage_reported {
            self.chat_state_handle
                .push_assistant_response(assistant_item);
        } else {
            self.chat_state_handle
                .push_unreported_model_output(assistant_item);
        }
    }
}

/// Per-tool precedence: a non-empty `over` wins, else the non-empty `seed`.
fn prefer_non_empty<T>(
    over: Option<T>,
    seed: Option<T>,
    is_empty: impl Fn(&T) -> bool,
) -> Option<T> {
    over.filter(|o| !is_empty(o))
        .or_else(|| seed.filter(|s| !is_empty(s)))
}

/// Acquire the turn-sampling permit for this session, or `None` when the gate is `None` (ungated).
/// A subagent's excess turns queue on `acquire_owned`; the permit releases on drop.
/// The semaphore is never closed, so `.ok()` fails open to ungated for this turn.
async fn acquire_subagent_sampling_permit(
    gate: &Option<Arc<tokio::sync::Semaphore>>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    let semaphore = gate.as_ref()?;
    semaphore.clone().acquire_owned().await.ok()
}

/// The cutoff a subagent inherits: a non-empty per-turn `base` wins per tool, else the `seed`.
fn resolve_configured_cutoff(
    seed: Option<distill_sampling_types::ToolOverrides>,
    base: Option<&distill_sampling_types::ToolOverrides>,
) -> distill_sampling_types::ToolOverrides {
    use distill_sampling_types::{ToolOverrides, WebSearchOptions, XSearchOptions};
    let ToolOverrides {
        x_search: seed_x,
        web_search: seed_w,
    } = seed.unwrap_or_default();
    let (over_x, over_w) =
        base.map_or((None, None), |b| (b.x_search.clone(), b.web_search.clone()));
    ToolOverrides {
        x_search: prefer_non_empty(over_x, seed_x, XSearchOptions::is_empty),
        web_search: prefer_non_empty(over_w, seed_w, WebSearchOptions::is_empty),
    }
}

#[cfg(test)]
#[path = "sampler_turn_tests.rs"]
mod tests;

#[cfg(test)]
mod stream_drain_tests {
    use super::{StreamDrainOutcome, error_after_stream_drain, stream_drain_outcome};

    #[tokio::test(start_paused = true)]
    async fn acknowledged_barrier_is_distinct_from_revocation() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(()).expect("receiver stays live");
        assert_eq!(
            stream_drain_outcome(rx).await,
            StreamDrainOutcome::Acknowledged
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        drop(tx);
        assert_eq!(stream_drain_outcome(rx).await, StreamDrainOutcome::Revoked);
    }

    #[tokio::test(start_paused = true)]
    async fn live_stalled_barrier_times_out() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        assert_eq!(stream_drain_outcome(rx).await, StreamDrainOutcome::TimedOut);
    }

    #[test]
    fn failed_drain_revocation_supersedes_original_error() {
        let original = distill_sampler::SamplingErrorInfo {
            kind: distill_sampler::SamplingErrorKind::RateLimited,
            status_code: Some(429),
            message: "original sampling failure".to_string(),
            is_retryable: true,
            retry_after_secs: Some(1),
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: distill_sampling_types::SentCredential::Unknown,
        };
        let result = error_after_stream_drain(StreamDrainOutcome::Revoked, original);
        assert_eq!(
            result.message,
            "sampling result revoked by turn cancellation or rewind"
        );
        assert!(!result.is_retryable);
    }
}
