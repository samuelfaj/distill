// Modified for Distill by Samuel Fajreldines, 2026.
//! Turn execution for `SessionActor`: `handle_prompt`, the sampling loop, and turn-end handling.
use super::*;
use crate::session::{InputAuthority, SlashAuthority};
use crate::util::dual_clock::DualClock;
use distill_tools::implementations::distill::task::types::{
    SubagentEvent, SubagentMarkUsageNotAppliedRequest, SubagentWaitPromptDrainedRequest,
};
use distill_tools::types::tool::ToolKind;
use tracing::Instrument;
static TURNS_ACTIVE: distill_telemetry::activity::ActivityGauge =
    distill_telemetry::activity::ActivityGauge::work(distill_telemetry::activity::TURNS_ACTIVE_KEY);
/// Synthetic tool for schema-constrained final answers on backends without native
/// output constraints (Messages API); intercepted in the loop, never really executed.
const STRUCTURED_OUTPUT_TOOL: &str = "StructuredOutput";
/// Max times the model may re-call `StructuredOutput` with non-conforming args before the turn ends with the last validation error.
const STRUCTURED_OUTPUT_MAX_RETRIES: u32 = 3;
/// Turn-freeze usage drain: bounds a wedged foreground child at turn end; folds normally land in one or two polls because they run before worktree dispose.
/// The child completion path's bounded parent awaits are sized to fit inside this (compile-time asserted at `PARENT_ACK_TIMEOUT`).
pub(crate) const SUBAGENT_USAGE_DRAIN: std::time::Duration = std::time::Duration::from_secs(120);
/// What a `StructuredOutput` tool call means for the turn (see
/// `handle_structured_output_tool_call`).
enum StructuredOutputStep {
    /// Accepted, or retries exhausted: the carried result is the final output.
    Complete(Result<serde_json::Value, String>),
    /// Non-conforming args; a corrective tool_result was pushed, so re-sample.
    Retry,
    /// No sole StructuredOutput call (absent, or co-emitted with real tools that should run this round).
    Proceed,
}
const TASK_ABORTED_CANCELLATION_CATEGORY: &str = "task_aborted";
struct ClearActiveDispatchOnDrop(crate::session::signals::SessionSignalsHandle);

impl Drop for ClearActiveDispatchOnDrop {
    fn drop(&mut self) {
        self.0.clear_active_dispatch();
    }
}
/// Outcome-specific fields of a `grok_code.turn_completed` row.
struct TurnTelemetryOutcome {
    outcome: distill_telemetry::events::Outcome,
    cancellation_category: Option<String>,
    error_category: Option<String>,
    error_code: Option<String>,
    error_detail: Option<String>,
}
impl TurnTelemetryOutcome {
    fn from_result(result: &Result<TurnOutcome, acp::Error>) -> Self {
        use distill_telemetry::events::Outcome;
        let (outcome, cancellation_category, error_category, error_code, error_detail) =
            match result {
                Ok(TurnOutcome::Completed { .. }) => (Outcome::Completed, None, None, None, None),
                Ok(TurnOutcome::StationarityEnded) => (
                    Outcome::Completed,
                    Some(crate::session::commands::ACTION_STATIONARITY_CATEGORY.to_string()),
                    None,
                    None,
                    None,
                ),
                Ok(TurnOutcome::Cancelled { category, .. }) => (
                    Outcome::Cancelled,
                    category.map(|c| crate::session::commands::meta_category_str(c).to_string()),
                    None,
                    None,
                    None,
                ),
                Ok(TurnOutcome::MaxTurnsReached { .. }) => (
                    Outcome::Cancelled,
                    Some(crate::session::commands::MAX_TURNS_REACHED_CATEGORY.to_string()),
                    None,
                    None,
                    None,
                ),
                Err(err) => {
                    let (error_category, error_code, error_detail) =
                        SessionActor::turn_error_fields(err);
                    (
                        Outcome::Error,
                        None,
                        Some(error_category),
                        Some(error_code),
                        error_detail,
                    )
                }
            };
        Self {
            outcome,
            cancellation_category,
            error_category,
            error_code,
            error_detail,
        }
    }
    fn task_aborted() -> Self {
        Self {
            outcome: distill_telemetry::events::Outcome::Cancelled,
            cancellation_category: Some(TASK_ABORTED_CANCELLATION_CATEGORY.to_string()),
            error_category: None,
            error_code: None,
            error_detail: None,
        }
    }
    fn error(err: &acp::Error) -> Self {
        let (error_category, error_code, error_detail) = SessionActor::turn_error_fields(err);
        Self {
            outcome: distill_telemetry::events::Outcome::Error,
            cancellation_category: None,
            error_category: Some(error_category),
            error_code: Some(error_code),
            error_detail,
        }
    }
}
/// Emits exactly one `grok_code.turn_completed` per turn task: [`emit`](Self::emit) once the outcome
/// is known, else a `task_aborted` fallback on [`Drop`] when the turn future is aborted first.
struct TurnCompletionEmitter {
    session: Arc<SessionActor>,
    model_id: String,
    started: std::time::Instant,
    emitted: bool,
    context_tokens: Option<u64>,
    turn_tokens: Option<u64>,
}
impl TurnCompletionEmitter {
    fn new(session: Arc<SessionActor>, model_id: String, started: std::time::Instant) -> Self {
        Self {
            session,
            model_id,
            started,
            emitted: false,
            context_tokens: None,
            turn_tokens: None,
        }
    }
    fn record_turn_usage(&mut self, turn_tokens: u64, context_tokens: Option<u64>) {
        self.turn_tokens = Some(turn_tokens);
        self.context_tokens = context_tokens;
    }
    /// Re-anchor the model and duration clock when the turn's work starts (post-setup), so an abort
    /// during the turn measures like a completed turn instead of counting setup time.
    fn begin_turn_work(&mut self, model_id: String, started: std::time::Instant) {
        self.model_id = model_id;
        self.started = started;
    }
    /// Emit an `error` outcome for a setup failure that returns before the turn loop.
    fn emit_error(&mut self, err: &acp::Error) {
        let duration_ms =
            super::turn_task::elapsed_ms_saturating(self.started, std::time::Instant::now());
        let tool_call_count = self.session.events.tool_count_this_turn();
        self.emit(
            TurnTelemetryOutcome::error(err),
            duration_ms,
            tool_call_count,
        );
    }
    fn emit(&mut self, outcome: TurnTelemetryOutcome, duration_ms: u64, tool_call_count: u32) {
        self.emitted = true;
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::TurnCompleted {
            outcome: outcome.outcome,
            duration_ms,
            tool_call_count,
            model_id: self.model_id.clone(),
            session_id: Some(self.session.session_info.id.0.to_string()),
            cancellation_category: outcome.cancellation_category,
            error_category: outcome.error_category,
            error_code: outcome.error_code,
            error_detail: outcome.error_detail,
            context_tokens: self.context_tokens,
            turn_tokens: self.turn_tokens,
        });
    }
}
impl Drop for TurnCompletionEmitter {
    fn drop(&mut self) {
        if self.emitted {
            return;
        }
        let duration_ms =
            super::turn_task::elapsed_ms_saturating(self.started, std::time::Instant::now());
        let tool_call_count = self.session.events.tool_count_this_turn();
        self.emit(
            TurnTelemetryOutcome::task_aborted(),
            duration_ms,
            tool_call_count,
        );
    }
}
/// Parse `raw` as JSON and validate it against a `validator` compiled once per turn.
/// Returns the value on success, or a human-readable error (shown to the model on retry and to the client as `structuredOutputError`).
/// A `validator` of `Err` means the user's schema itself was invalid.
fn validate_structured_output(
    validator: &Result<jsonschema::Validator, String>,
    raw: &str,
) -> Result<serde_json::Value, String> {
    let validator = validator.as_ref().map_err(Clone::clone)?;
    let value: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| format!("model output was not valid JSON: {e}"))?;
    match validator.validate(&value) {
        Ok(()) => Ok(value),
        Err(e) => Err(format!("output does not match the required schema: {e}")),
    }
}
/// Result of the turn-end usage drain (and cancel's no-drain snapshot).
/// Only [`Self::fail_closed`] marks the ledgers.
/// Sticky and background live are report-level only (tokens still land on the session ledger).
pub(super) struct UsageDrainOutcome {
    /// Query failure, or a foreground child still live after timeout/cancel.
    /// Marks both the prompt and session bills incomplete.
    /// (True apply-miss stains ledgers at fold time via `mark_apply_miss_incomplete`, not here.)
    pub(super) fail_closed: bool,
    /// A background child is still running: only this prompt's report is incomplete; its spend reaches the session ledger at completion.
    pub(super) background_live: bool,
    /// Pin-scoped sticky (session-only attribution or apply-miss report).
    /// Marks the report incomplete only; it does not stain ledgers by itself.
    pub(super) sticky_report: bool,
}
impl UsageDrainOutcome {
    /// Wire / attach incomplete: fail-closed, background, or sticky.
    pub(super) fn report_incomplete(&self) -> bool {
        self.fail_closed || self.background_live || self.sticky_report
    }
    /// Map an outstanding reply without a multi-second drain (cancel path).
    /// Same policy as freeze's terminal outcome: a live foreground child is fail-closed; sticky and background are report-only.
    pub(super) fn from_outstanding_reply(
        reply: Option<
            &distill_tools::implementations::distill::task::types::SubagentOutstandingReply,
        >,
    ) -> Self {
        match reply {
            None => Self {
                fail_closed: true,
                background_live: false,
                sticky_report: false,
            },
            Some(r) => Self {
                fail_closed: !r.live_ids.is_empty(),
                background_live: r.background_live,
                sticky_report: r.subagent_usage_not_applied,
            },
        }
    }
}
/// Accumulates a round's per-call token usage and tool-call presence across the agentic loop's model calls, recording running totals on the round span.
/// Kept out of the loop body so telemetry bookkeeping doesn't obscure control flow.
#[derive(Default)]
struct TurnSpanTotals {
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    has_tool_call: bool,
}
/// Sums of every completed round's [`TurnSpanTotals`], stamped on the turn's one turn-end
/// snapshot (`take_completed_turn_snapshot`).
#[derive(Default)]
pub(super) struct TurnSampling {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) cache_read_tokens: u64,
}
impl TurnSampling {
    /// Takes the round by value: a late interjection continues the loop after bookkeeping, and
    /// the next fold must see only the new samples.
    fn fold(&mut self, round: TurnSpanTotals) {
        self.input_tokens += round.input_tokens.max(0) as u64;
        self.output_tokens += round.output_tokens.max(0) as u64;
        self.cache_read_tokens += round.cache_read_tokens.max(0) as u64;
    }
}
impl TurnSpanTotals {
    /// Fold one model response into the totals and update the span.
    /// Tokens sum (each call is billed its full prompt); has_tool_call ORs (the final call has none).
    /// `stop_reason` is last-wins (the terminal reason), not summed.
    fn record(&mut self, span: &tracing::Span, response: &ConversationResponse) {
        if let Some(u) = response.usage.as_ref() {
            self.input_tokens += i64::from(u.prompt_tokens);
            self.output_tokens += i64::from(u.completion_tokens);
            self.cache_read_tokens += i64::from(u.cached_prompt_tokens);
            span.record("input_tokens", self.input_tokens);
            span.record("output_tokens", self.output_tokens);
            span.record("cache_read_tokens", self.cache_read_tokens);
        }
        if let Some(sr) = response.stop_reason {
            span.record("stop_reason", sr.as_ref());
        }
        self.has_tool_call |= !response.tool_calls().is_empty();
        span.record("response.has_tool_call", self.has_tool_call);
        record_last_sample(
            span,
            response
                .stop_reason
                .map_or(LAST_SAMPLE_STOP_REASON_UNREPORTED, |sr| sr.into()),
            !response.tool_calls().is_empty(),
            response
                .usage
                .as_ref()
                .map_or(0, |u| i64::from(u.completion_tokens)),
        );
    }
}
/// Single writer for the `last_sample.*` fields, shared by the delivered and failed paths so the field set cannot drift between them.
fn record_last_sample(
    span: &tracing::Span,
    stop_reason: &str,
    has_tool_call: bool,
    output_tokens: i64,
) {
    debug_assert!(
        span.is_disabled() || span.has_field("last_sample.stop_reason"),
        "current span does not declare last_sample.* fields"
    );
    span.record("last_sample.stop_reason", stop_reason);
    span.record("last_sample.has_tool_call", has_tool_call);
    span.record("last_sample.output_tokens", output_tokens);
}
/// Recorded when a response reports no stop reason, so no stale previous-sample value survives.
const LAST_SAMPLE_STOP_REASON_UNREPORTED: &str = "unreported";
/// `last_sample.stop_reason` for a failed call with no reported stop reason.
const LAST_SAMPLE_STOP_REASON_ERROR: &str = "error";
/// Record `last_sample.*` for a failed call `TurnSpanTotals::record` never sees; otherwise the span keeps describing the previous sample.
/// A resubmit's next response overwrites these fields.
pub(super) fn record_failed_sample_on_turn_span(
    span: &tracing::Span,
    kind: distill_sampler::SamplingErrorKind,
) {
    let stop_reason = match kind {
        distill_sampler::SamplingErrorKind::MaxTokensTruncation => {
            distill_sampling_types::StopReason::Length.as_ref()
        }
        _ => LAST_SAMPLE_STOP_REASON_ERROR,
    };
    record_last_sample(span, stop_reason, false, 0);
}
/// How the turn's per-block user-message echo is published to clients / `updates.jsonl`.
/// Turns whose content must not render as a user prompt (notification drain) are hidden by the *pager* via the `hideFromScrollback` chunk meta.
/// The persisted line is never omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserEchoMode {
    /// Broadcast live and persist (real user / cron / skill turns).
    Broadcast,
    /// Persist without live broadcast.
    /// Interject-fallback: panes already rendered the text, so a live echo would duplicate it.
    /// Notification drain: model-only content (the UI shows it via side channels: monitor gutter, task pane) that no pane should render live.
    PersistOnly,
}
fn user_echo_mode(prompt_id: &str, input_origin: &InputOrigin) -> UserEchoMode {
    if super::interjection::is_interject_fallback(prompt_id)
        || matches!(
            input_origin.as_prompt_origin(),
            PromptOrigin::NotificationDrain
        )
    {
        UserEchoMode::PersistOnly
    } else {
        UserEchoMode::Broadcast
    }
}
impl SessionActor {
    /// Exactly once per turn: the cancel path and the turn's own post-loop both announce.
    pub(super) async fn notify_turn_abort(
        &self,
        epoch: TurnEpoch,
        reason: distill_agent_lifecycle::TurnAbortReason,
    ) {
        if !self.turn_abort.try_mark_announced(epoch) {
            return;
        }
        let input = distill_agent_lifecycle::TurnAbortInput::new(reason);
        for contributor in self.extension_registry.turn_lifecycle_contributors() {
            contributor.on_turn_abort(&input).await;
        }
    }
    /// Run the image-normalization pipeline (re-encode caps, min-side and integrity checks) and return the surviving images.
    /// Compression, re-encode-fallback, and dropped notices are appended to `text_out` and mirrored as `ImageCompressed`/`ImageDropped`.
    /// Only text is appended; image data never enters a string.
    pub(crate) async fn normalize_images_with_notices(
        &self,
        text_out: &mut String,
        images: Vec<acp::ImageContent>,
        is_cursor: bool,
    ) -> Vec<acp::ImageContent> {
        let mut norm_result =
            crate::session::image_normalize::normalize_images(images, is_cursor).await;
        let user_images = std::mem::take(&mut norm_result.images);
        use crate::extensions::notification::ImageCompressedEntry;
        if !norm_result.compressed.is_empty() {
            text_out.push_str(&crate::session::image_normalize::render_compression_notice(
                &norm_result.compressed,
                is_cursor,
            ));
            let message = norm_result
                .compressed
                .iter()
                .map(|c| c.display())
                .collect::<Vec<_>>()
                .join("; ");
            let images = norm_result
                .compressed
                .iter()
                .map(ImageCompressedEntry::from)
                .collect();
            self.send_xai_notification(XaiSessionUpdate::ImageCompressed { images, message })
                .await;
        }
        if !norm_result.re_encode_fallbacks.is_empty() {
            text_out.push_str(
                &crate::session::image_normalize::render_re_encode_fallback_notice(
                    &norm_result.re_encode_fallbacks,
                    is_cursor,
                ),
            );
            self.send_xai_notification(XaiSessionUpdate::ImageCompressed {
                images: vec![],
                message: norm_result.re_encode_fallbacks.join(" "),
            })
            .await;
        }
        if let Some((notice, notes)) = crate::session::image_normalize::dropped_to_envelope(
            std::mem::take(&mut norm_result.dropped),
            is_cursor,
        ) {
            text_out.push_str(&notice);
            self.send_xai_notification(XaiSessionUpdate::ImageDropped { notes })
                .await;
        }
        user_images
    }
    pub(super) fn persist_host_turn_user_echo(&self, text: &str, prompt_id: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let mut chunk_meta = serde_json::Map::new();
        chunk_meta.insert(
            crate::session::storage::HOST_TURN_META_KEY.into(),
            serde_json::json!(true),
        );
        if super::super::PromptOrigin::from_prompt_id(prompt_id).hide_user_echo_from_scrollback() {
            chunk_meta.insert("hideFromScrollback".into(), serde_json::json!(true));
        }
        let update = acp::SessionUpdate::UserMessageChunk(
            acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(
                text.to_string(),
            )))
            .meta(Some(chunk_meta)),
        );
        let notification_meta = self.build_notification_meta();
        let notification = acp::SessionNotification::new(self.session_info.id.clone(), update)
            .meta(notification_meta.as_object().cloned());
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Acp(Box::new(notification)),
            ));
    }
    #[cfg(test)]
    pub(super) async fn handle_prompt(
        self: &Arc<Self>,
        prompt_id: &str,
        prompt_blocks: Vec<acp::ContentBlock>,
        prompt_mode: PromptMode,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        prompt_client_identifier: Option<String>,
        prompt_screen_mode: Option<String>,
        verbatim: bool,
        send_now: bool,
        json_schema: Option<serde_json::Value>,
        persist_ack: Option<oneshot::Sender<()>>,
        parsed_prompt_tx: Option<oneshot::Sender<ParsedPromptInfo>>,
    ) -> PromptTurnResult {
        self.signals_handle().increment_turn();
        self.handle_turn_input(TurnInputRequest {
            prompt_id: prompt_id.to_string(),
            input_origin: InputOrigin::from_prompt_id(prompt_id),
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier: prompt_client_identifier,
            screen_mode: prompt_screen_mode,
            verbatim,
            send_now,
            json_schema,
            persist_ack,
            parsed_prompt_tx,
            traceparent: None,
            start_gate: None,
        })
        .await
    }
    pub(super) async fn handle_turn_input(
        self: &Arc<Self>,
        request: TurnInputRequest,
    ) -> PromptTurnResult {
        let span = tracing::info_span!(
            "session.handle_prompt",
            session_id = %self.session_info.id.0,
            prompt_id = %request.prompt_id,
            prompt_length = tracing::field::Empty,
            command_name = tracing::field::Empty,
            command_source = tracing::field::Empty,
        );
        if let Some(ref tp) = request.traceparent {
            distill_otel::link_span_to_meta(&span, &serde_json::json!({ "traceparent": tp }));
        }
        self.handle_turn_input_inner(request).instrument(span).await
    }
    async fn handle_turn_input_inner(
        self: &Arc<Self>,
        request: TurnInputRequest,
    ) -> PromptTurnResult {
        let _active = TURNS_ACTIVE.enter();
        let _work = crate::session::handle::WorkGuard::new(self.active_work.clone());
        let TurnInputRequest {
            prompt_id,
            input_origin,
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier: prompt_client_identifier,
            screen_mode: prompt_screen_mode,
            verbatim,
            send_now,
            json_schema,
            mut persist_ack,
            parsed_prompt_tx,
            traceparent: _,
            start_gate: _,
        } = request;
        let prompt_id = prompt_id.as_str();
        let handle_prompt_start = std::time::Instant::now();
        *self.active_skill.lock() = None;
        distill_telemetry::unified_log::info(
            "shell.handle_prompt.start",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": prompt_id,
                "block_count": prompt_blocks.len(),
            })),
        );
        let policy = input_origin.policy();
        self.open_subagent_spawn_admission();
        if let Some(reservations) = &self.tool_context.task_completion_reservations
            && let Some(completion_id) = input_origin.completion_id()
        {
            reservations.release(completion_id);
        }
        if policy.authority.is_human_intent() {
            self.invalidate_side_calls_for_new_prompt();
        }
        self.ensure_session_disk_writable().await?;
        let wake_message = match input_origin.as_prompt_origin() {
            super::super::PromptOrigin::SubagentCompleted { subagent_id } => {
                Some(self.build_wake_turn_message(subagent_id).await)
            }
            _ => None,
        };
        let (prompt_blocks, commit_ids) = match wake_message {
            Some(WakeTurnMessage::Digest { text, ids }) => (
                vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                ids,
            ),
            Some(WakeTurnMessage::Silent) => {
                tracing::info!(prompt_id, "ending wake turn without sampling");
                return ok_end_turn(0, None);
            }
            Some(WakeTurnMessage::KeepBody) | None => (
                prompt_blocks,
                input_origin
                    .completion_id()
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
            ),
        };
        let prompt_length: usize = prompt_blocks
            .iter()
            .map(|b| match b {
                acp::ContentBlock::Text(t) => t.text.len(),
                _ => 0,
            })
            .sum();
        tracing::Span::current().record("prompt_length", prompt_length as i64);
        let prompt_mode =
            self.resolve_turn_prompt_mode(input_origin.as_prompt_origin(), prompt_mode);
        *self.turn_start_prompt_mode.lock() = prompt_mode;
        *self.turn_prompt_mode.lock() = prompt_mode;
        let _turn_active_guard =
            TurnActiveGuard::activate(self.tool_context.is_turn_active.as_ref());
        let _session_turn_active_guard = TurnActiveGuard::activate(Some(&self.session_turn_active));
        let turn_start_input =
            distill_agent_lifecycle::TurnStartInput::new(input_origin.is_synthetic());
        for contributor in self.extension_registry.turn_lifecycle_contributors() {
            contributor
                .on_turn_start_with_policy(&turn_start_input, policy)
                .await;
        }
        if let Ok(mut pending) = self.rewind_pending_prompt.lock()
            && let Some(prev_text) = pending.take()
        {
            let new_text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
                if let acp::ContentBlock::Text(t) = b {
                    acc.push_str(&t.text);
                }
                acc
            });
            if new_text.trim() == prev_text.trim() {
                self.signals_handle().record_regeneration();
            } else {
                self.signals_handle().record_edit_and_retry();
            }
        }
        if policy.authority != InputAuthority::ModelAuthoredUntrusted
            && let Some(bash_command) = Self::extract_bash_command(&prompt_blocks)
        {
            return self
                .handle_direct_bash_command(prompt_id, bash_command, &prompt_blocks)
                .await;
        }
        let mut pending_skill_information: Option<String> = None;
        let original_prompt_text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
            if let acp::ContentBlock::Text(t) = b {
                acc.push_str(&t.text);
            }
            acc
        });
        let mut otel_command_name: Option<String> = None;
        let (resolved, slash_skills, workflow_registry) = match policy.slash {
            SlashAuthority::HumanCatalog => {
                let slash_skills = self.slash_skills_for_resolve().await;
                let skill_rewrite = if crate::session::is_cursor_user_template(
                    &self.agent.borrow().definition().user_message_template,
                ) {
                    slash_commands::SkillSlashRewrite::Passthrough
                } else {
                    slash_commands::SkillSlashRewrite::RewriteToRun
                };
                let availability = self.command_availability().await;
                let (workflow_registry, named_workflows) = self.named_workflow_snapshot();
                let resolved = slash_commands::resolve_human_intent(
                    prompt_blocks,
                    &slash_skills,
                    availability,
                    skill_rewrite,
                    &named_workflows,
                );
                (resolved, slash_skills, Some(workflow_registry))
            }
            SlashAuthority::Inert => (Ok(prompt_blocks), Vec::new(), None),
            SlashAuthority::ModelAuthored => {
                match crate::session::slash_authority::resolve(
                    &prompt_blocks,
                    slash_commands::BUILTIN_COMMANDS,
                ) {
                    crate::session::slash_authority::AuthorityResolution::StaticBuiltin(
                        action,
                    ) => (Err(SlashCommandOutcome::Builtin(action)), Vec::new(), None),
                    crate::session::slash_authority::AuthorityResolution::ModelAuthoredSkillCandidate {
                        command_name,
                        args,
                    } => {
                        let command_name = command_name.to_string();
                        let args = args.to_string();
                        let slash_skills = self.slash_skills_for_resolve().await;
                        let availability = self
                            .command_availability_for_skill_projection()
                            .await;
                        let bridge = self.tool_bridge_handle();
                        let has_skill_loader = bridge
                            .tool_for_kind(distill_tools::types::tool::ToolKind::Read)
                            .await
                            .is_some()
                            || bridge
                                .tool_for_kind(distill_tools::types::tool::ToolKind::Skill)
                                .await
                                .is_some();
                        let resolved = slash_commands::resolve_model_authored_skill(
                            prompt_blocks,
                            &command_name,
                            &args,
                            &slash_skills,
                            availability,
                            has_skill_loader,
                        );
                        (resolved, slash_skills, None)
                    }
                    crate::session::slash_authority::AuthorityResolution::NotSlash => {
                        (Ok(prompt_blocks), Vec::new(), None)
                    }
                }
            }
        };
        let prompt_blocks = match resolved {
            Ok(blocks) => blocks,
            Err(SlashCommandOutcome::Builtin(action)) => {
                let text_block =
                    |text: String| acp::ContentBlock::Text(acp::TextContent::new(text));
                let slash_used = distill_telemetry::events::SlashCommandUsed {
                    command: action.command_name().to_string(),
                    args_provided: action.args_provided(),
                };
                {
                    let span = tracing::Span::current();
                    span.record("command_name", action.command_name());
                    span.record("command_source", "builtin");
                }
                otel_command_name = Some(action.command_name().to_string());
                match action {
                    BuiltinAction::GoalSet {
                        objective,
                        token_budget,
                    } => {
                        distill_telemetry::session_ctx::log_event(slash_used);
                        match self.setup_goal(&objective, token_budget).await {
                            GoalSetupOutcome::Inference { reminder } => {
                                vec![text_block(reminder)]
                            }
                            GoalSetupOutcome::Message(msg) => {
                                self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                                self.mark_front_message_committed().await;
                                self.send_host_turn_slash_command_output(&msg).await;
                                return ok_end_turn(0, None);
                            }
                        }
                    }
                    BuiltinAction::GoalResume => {
                        distill_telemetry::session_ctx::log_event(slash_used);
                        match self.resume_goal().await {
                            GoalResumeOutcome::Inference { reminder, user_msg } => {
                                self.send_slash_command_output(&user_msg).await;
                                vec![text_block(reminder)]
                            }
                            GoalResumeOutcome::Message(msg) => {
                                self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                                self.mark_front_message_committed().await;
                                self.send_host_turn_slash_command_output(&msg).await;
                                return ok_end_turn(0, None);
                            }
                        }
                    }
                    BuiltinAction::WorkflowLaunch { name, input } => {
                        let Some(workflow_registry) = workflow_registry.as_ref() else {
                            unreachable!("workflow slash commands require human authority")
                        };
                        self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                        self.mark_front_message_committed().await;
                        let msg = self
                            .launch_named_workflow(workflow_registry, &name, &input)
                            .await;
                        self.send_host_turn_slash_command_output(&msg).await;
                        return ok_end_turn(0, None);
                    }
                    _ => {
                        if policy.authority.is_human_intent() {
                            self.persist_host_turn_user_echo(&original_prompt_text, prompt_id);
                        }
                        return self.execute_builtin_slash_command(action).await;
                    }
                }
            }
            Err(SlashCommandOutcome::InvokeSkill {
                blocks: original_blocks,
                skills: parsed_skills,
            }) => {
                if let Some(first) = parsed_skills.first() {
                    *self.active_skill.lock() = Some(first.name.clone());
                    otel_command_name = Some(first.name.clone());
                    let span = tracing::Span::current();
                    span.record("command_name", first.name.as_str());
                    span.record(
                        "command_source",
                        if first.plugin_name.is_some() {
                            "plugin"
                        } else {
                            "skill"
                        },
                    );
                }
                for sk in &parsed_skills {
                    distill_telemetry::session_ctx::log_event(
                        distill_telemetry::events::SlashCommandUsed {
                            command: sk.name.clone(),
                            args_provided: !sk.args.is_empty(),
                        },
                    );
                    let skill_source = crate::session::telemetry::skill_source(
                        sk.scope,
                        sk.plugin_name.as_deref(),
                    );
                    distill_telemetry::session_ctx::log_event(
                        distill_telemetry::events::SkillDispatched {
                            skill_name: sk.name.clone(),
                            plugin_source: sk.plugin_name.clone(),
                            trigger: distill_telemetry::events::SkillTrigger::SlashCommand,
                            skill_source: Some(skill_source.to_owned()),
                        },
                    );
                    distill_telemetry::event_span!(
                        "skill.activated",
                        skill_name = %sk.name,
                        invocation_trigger = "slash_command",
                        skill_source = skill_source,
                    );
                    if let Some(ref pname) = sk.plugin_name {
                        distill_telemetry::session_ctx::log_event(
                            distill_telemetry::events::PluginUsed {
                                plugin_id: pname.clone(),
                                plugin_name: pname.clone(),
                                skill_name: Some(sk.name.clone()),
                                hook_event: None,
                                success: true,
                            },
                        );
                        distill_telemetry::event_span!(
                            "plugin.used",
                            plugin_name = %pname,
                            skill_name = %sk.name,
                        );
                    }
                }
                pending_skill_information = slash_commands::build_skill_information_for_refs(
                    &parsed_skills,
                    &slash_skills,
                    &self.session_id_string(),
                )
                .await;
                original_blocks
            }
        };
        *self.doom_loop_turn_tally.lock() = Default::default();
        self.retain_timed_out_image_strips_for_new_turn();
        self.turn_stream_drained.lock().clear();
        self.events.begin_turn();
        let model_id = self.current_model_id().await;
        let turn_number = self.chat_state_handle.get_prompt_index().await as u64;
        self.current_turn_number.set(turn_number);
        self.transient_retries_prompt_total.set(0);
        self.transient_episode_start.set(None);
        let yolo_mode = self.permissions.is_yolo_mode();
        let msg_count = self.chat_state_handle.get_conversation_len().await;
        let redirect_kind = if policy.authority.is_human_intent() {
            self.events.take_prior_redirect_kind()
        } else {
            None
        };
        self.emit_event(crate::session::events::Event::TurnStarted {
            session_id: self.session_id_string(),
            turn_number,
            model_id: model_id.clone(),
            yolo_mode,
            conversation_message_count: msg_count,
            session_relationship: crate::session::events::SessionRelationship::Primary,
            schema_version: crate::session::events::EVENT_SCHEMA_VERSION.into(),
            redirect_kind,
        });
        self.observability_bridge
            .emit(
                distill_tool_protocol::session_event::SessionEvent::TurnStarted {
                    turn_number,
                    model_id: model_id.clone(),
                    yolo_mode,
                },
            )
            .await;
        let mut turn_completion_emitter = TurnCompletionEmitter::new(
            Arc::clone(self),
            model_id.clone(),
            std::time::Instant::now(),
        );
        self.send_before_turn_event(distill_tool_protocol::turn_hook::BeforeTurnPayload {
            turn_number: self.chat_state_handle.get_prompt_index().await as u64,
            model_id: model_id.clone(),
            yolo_mode: self.permissions.is_yolo_mode(),
            conversation_message_count: msg_count,
            session_relationship: distill_tool_protocol::turn_hook::DEFAULT_SESSION_RELATIONSHIP
                .to_string(),
            schema_version: crate::session::events::EVENT_SCHEMA_VERSION.to_string(),
        })
        .await;
        let turn_idx = self.chat_state_handle.get_prompt_index().await as u64;
        distill_telemetry::session_ctx::log_session_event(crate::agent::session_metrics::Turn {
            session_id: self.session_info.id.0.to_string(),
            turn_number: turn_idx,
        });
        let current_prompt_index = self.chat_state_handle.get_prompt_index().await;
        distill_telemetry::session_ctx::begin_prompt_id();
        let mut chunk_meta = serde_json::Map::new();
        chunk_meta.insert("modelId".into(), serde_json::json!(model_id));
        chunk_meta.insert(
            "promptIndex".into(),
            serde_json::json!(current_prompt_index),
        );
        if input_origin
            .as_prompt_origin()
            .hide_user_echo_from_scrollback()
        {
            chunk_meta.insert("hideFromScrollback".into(), serde_json::json!(true));
        }
        let user_chunk_meta = Some(chunk_meta);
        let text = prompt_blocks.iter().fold(String::new(), |mut acc, b| {
            if let acp::ContentBlock::Text(t) = b {
                acc.push_str(&t.text);
            }
            acc
        });
        let prompt_block = if policy.authority != InputAuthority::ModelAuthoredUntrusted {
            let prompt_gate_verdict = self
                .dispatch_prompt_submit_hook(
                    distill_hooks::event::HookPayload::UserPromptSubmit {
                        prompt: Some(text.clone()),
                        subagent_type: self.subagent_type_label(),
                    },
                    Some(prompt_id),
                )
                .await;
            match (
                self.should_enforce_prompt_block(&policy),
                prompt_gate_verdict,
            ) {
                (true, distill_hooks::result::PromptDecision::Block { reason, hook_name }) => {
                    Some((hook_name, reason))
                }
                (false, distill_hooks::result::PromptDecision::Block { reason, hook_name }) => {
                    tracing::info!(%hook_name, %reason, "user_prompt_submit block ignored for non-user origin");
                    self.send_hook_annotation(
                            &format!(
                        "\u{26a0} Prompt block requested by {} (not enforced for this origin): {reason}",
                        distill_hooks::config::hook_display_name(&hook_name)
                    ),
                        )
                        .await;
                    None
                }
                (_, distill_hooks::result::PromptDecision::Allow) => None,
            }
        } else {
            None
        };
        if prompt_block.is_some() {
            if let Some(ack) = persist_ack.take() {
                let _ = ack.send(());
            }
        } else {
            self.chat_state_handle.increment_prompt_index();
            *self.tool_context.prompt_index.lock().await = current_prompt_index;
            self.tool_context
                .active_message_parent_prompt_index
                .store(current_prompt_index, std::sync::atomic::Ordering::Release);
            self.file_state_tracker
                .begin_prompt(current_prompt_index)
                .await;
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                self.chat_state_handle.cache_prompt_text(trimmed);
            }
            let echo_mode = user_echo_mode(prompt_id, &input_origin);
            for block in prompt_blocks.iter() {
                let update = acp::SessionUpdate::UserMessageChunk(
                    acp::ContentChunk::new(block.clone()).meta(user_chunk_meta.clone()),
                );
                let notification_meta = self.build_notification_meta();
                let notification =
                    acp::SessionNotification::new(self.session_info.id.clone(), update)
                        .meta(notification_meta.as_object().cloned());
                if echo_mode == UserEchoMode::PersistOnly {
                    let _ = self
                        .notifications
                        .persistence_tx
                        .send(PersistenceMsg::Update(
                            crate::session::storage::SessionUpdate::Acp(Box::new(notification)),
                        ));
                } else {
                    self.emit_notification_direct(notification).await;
                }
            }
            let crate::session::prompt_parser::ParsedPrompt {
                mut context,
                query,
                skill_information: skill_info,
                images: mut raw_images,
                is_cursor,
            } = match parse_prompt_with_skills(
                &prompt_blocks,
                self.tool_context.cwd.to_path_buf(),
                &self.session_info,
                policy.authority,
                verbatim,
                self.is_cursor_harness(),
                pending_skill_information.take().unwrap_or_default(),
            )
            .await
            {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("Invalid prompt: {}", err.message);
                    turn_completion_emitter.emit_error(&err);
                    return Err(err);
                }
            };
            let recovered = if policy.authority == InputAuthority::ModelAuthoredUntrusted {
                0
            } else {
                crate::session::placeholder_images::recover_orphan_placeholders(
                    &query,
                    &mut raw_images,
                    std::path::Path::new(&self.session_info.cwd),
                )
            };
            if recovered > 0 {
                tracing::info!(
                    session_id = %self.session_info.id,
                    recovered,
                    "server-side placeholder fallback: loaded orphan image(s) from disk",
                );
            }
            let query =
                crate::session::placeholder_images::strip_paths_from_image_placeholders(query);
            let query = if send_now && !verbatim {
                distill_interjection_core::frame_user_turn(
                    distill_interjection_core::INTERJECTION_NOTE,
                    &query,
                )
            } else {
                query
            };
            let user_images = self
                .normalize_images_with_notices(&mut context, raw_images, is_cursor)
                .await;
            let (query, extra_images) = if !self.is_cursor_harness() {
                let extraction = distill_tools::util::base64_images::extract_base64_images(query);
                if extraction.images.is_empty() {
                    (extraction.text, Vec::new())
                } else {
                    let cleaned_text = extraction.text;
                    let count = extraction.images.len();
                    tracing::info!(
                        session_id = %self.session_info.id,
                        count,
                        "base64 images extracted from user query",
                    );
                    let acp_imgs: Vec<agent_client_protocol::ImageContent> = extraction
                        .images
                        .into_iter()
                        .map(|img| {
                            agent_client_protocol::ImageContent::new(img.data, img.mime_type)
                        })
                        .collect();
                    let nr =
                        crate::session::image_normalize::normalize_images(acp_imgs, false).await;
                    if !nr.re_encode_fallbacks.is_empty() {
                        tracing::warn!(
                            session_id = %self.session_info.id,
                            notes = %nr.re_encode_fallbacks.join(" "),
                            "Extracted user query image kept original after re-encode failure",
                        );
                    }
                    (cleaned_text, nr.images)
                }
            } else {
                (query, Vec::new())
            };
            let assembled = crate::session::prompt_parser::ParsedPrompt::assemble_parts_with_skills(
                &context,
                &query,
                &skill_info,
                is_cursor,
            );
            let pre_truncation_text = assembled.clone();
            let (user_message, truncated_local_path) = if verbatim {
                (assembled, None)
            } else {
                self.maybe_truncate_large_prompt_with_skills(
                    context,
                    query,
                    skill_info,
                    is_cursor,
                    current_prompt_index,
                )
                .await
            };
            let was_truncated = truncated_local_path.is_some();
            if let Some(tx) = parsed_prompt_tx {
                let _ = tx.send(ParsedPromptInfo {
                    text: user_message.clone(),
                    full_text: if was_truncated {
                        Some(pre_truncation_text)
                    } else {
                        None
                    },
                    local_path: truncated_local_path,
                });
            }
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
                    prompt_blocks.to_vec(),
                )));
            let model_id = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_default();
            if policy.analytics.is_human_prompt()
                && (self.telemetry_enabled || distill_telemetry::external::is_active())
            {
                let effective_client_identifier =
                    prompt_client_identifier.or_else(|| self.client_identifier.clone());
                let ev = distill_telemetry::events::PromptSubmitted {
                    prompt_length: user_message.len(),
                    model_id,
                    client_identifier: effective_client_identifier,
                    screen_mode: prompt_screen_mode,
                    prompt_text: distill_telemetry::external::is_active()
                        .then(|| user_message.to_owned()),
                    command_name: otel_command_name,
                };
                distill_telemetry::session_ctx::log_event_dual(self.telemetry_enabled, ev);
            }
            self.maybe_inject_mcp_reminder().await;
            self.maybe_inject_date_rollover_reminder().await;
            self.inject_plan_mode_reminders().await;
            self.inject_fork_reminder().await;
            self.inject_resumed_tasks_reminder();
            if policy.authority.is_human_intent() {
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                distill_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "handle_prompt_user_start" })),
                );
                self.consume_deferred_completions_for_user_turn().await;
            }
            self.drain_between_turn_completions(&commit_ids).await;
            self.inject_workflow_status_reminder().await;
            let user_message = if user_images.is_empty() {
                user_message
            } else if self.is_cursor_harness() {
                self.transcribe_user_images(user_message, &user_images)
                    .await
                    .inspect_err(|err| turn_completion_emitter.emit_error(err))?
            } else {
                let session_dir = crate::session::persistence::ensure_owner_only_session_dir(
                    &crate::session::info::Info {
                        id: self.session_info.id.clone(),
                        cwd: self.session_info.cwd.clone(),
                    },
                )
                .map_err(|e| {
                    crate::sampling::error::local_error(
                        "session_dir_create_failed",
                        format!("failed to create session dir: {:?}", e.kind()),
                    )
                })
                .inspect_err(|err| turn_completion_emitter.emit_error(err))?;
                crate::session::image_describe::persist_and_prepend_image_files(
                    &session_dir,
                    &user_images,
                    &user_message,
                )
                .map_err(|e| {
                    crate::sampling::error::local_error(
                        "assets_save_failed",
                        format!("failed to save user images to assets dir: {:?}", e.kind()),
                    )
                })
                .inspect_err(|err| turn_completion_emitter.emit_error(err))?
            };
            let attached_image_refs = if self.is_cursor_harness() {
                Vec::new()
            } else {
                crate::session::placeholder_images::attached_image_references(&user_images)
            };
            self.tool_bridge_handle()
                .update_resource(distill_tools::types::resources::AttachedImages(
                    attached_image_refs,
                ))
                .await;
            if trace_gcs_config.is_some() {
                self.chat_state_handle.begin_turn_capture();
            }
            let mut user_chat = match input_origin.as_prompt_origin() {
                super::super::PromptOrigin::TaskCompleted { .. } => {
                    ConversationItem::task_completed(user_message)
                }
                super::super::PromptOrigin::SubagentCompleted { .. } => {
                    ConversationItem::subagent_completed(user_message)
                }
                super::super::PromptOrigin::ParentAgentMessage { .. }
                | super::super::PromptOrigin::ParentHumanMessage { .. } => {
                    ConversationItem::agent_message(user_message)
                }
                super::super::PromptOrigin::WorkflowCompleted { .. } => {
                    ConversationItem::notification_drain(user_message)
                }
                super::super::PromptOrigin::NotificationDrain => {
                    ConversationItem::notification_drain(user_message)
                }
                super::super::PromptOrigin::GoalSummary => {
                    ConversationItem::goal_summary(user_message)
                }
                super::super::PromptOrigin::GoalClassifierNudge => {
                    ConversationItem::goal_classifier_nudge(user_message)
                }
                super::super::PromptOrigin::SchedulerFired => {
                    ConversationItem::scheduler_fired(user_message)
                }
                super::super::PromptOrigin::PlanResume => ConversationItem::user(user_message),
                super::super::PromptOrigin::User => {
                    let mut item = ConversationItem::user(
                        self.maybe_apply_interrupt_envelope(user_message, verbatim),
                    );
                    if let Some(interrupt) = self
                        .events
                        .take_prior_interrupt_category()
                        .and_then(crate::session::events::prior_turn_interrupt_from_cancellation)
                    {
                        item.set_prior_turn_interrupt(interrupt);
                    }
                    item
                }
            };
            user_chat.set_prompt_index(current_prompt_index);
            if !self.is_cursor_harness() {
                for image in &user_images {
                    user_chat.add_image(pick_user_image_url(image));
                }
                for image in &extra_images {
                    user_chat.add_image(format!("data:{};base64,{}", image.mime_type, image.data));
                }
            }
            if self
                .chat_state_handle
                .push_user_message_and_ack(user_chat)
                .await
                .is_some()
            {
                self.mark_front_message_committed().await;
                let commit_ids: Vec<&str> = commit_ids.iter().map(String::as_str).collect();
                self.mark_completions_reported(&commit_ids).await;
                let (flush_tx, flush_rx) = oneshot::channel();
                if self
                    .notifications
                    .persistence_tx
                    .send(PersistenceMsg::FlushAndAck {
                        respond_to: flush_tx,
                    })
                    .is_ok()
                    && matches!(flush_rx.await, Ok(Ok(())))
                {
                    let session_dir = crate::session::persistence::session_dir(&self.session_info);
                    crate::session::fork_status::commit_claim(&session_dir);
                    if let Some(ack) = persist_ack {
                        let _ = ack.send(());
                    }
                } else {
                    tracing::error!(
                        session_id = %self.session_info.id.0,
                        prompt_id = %prompt_id,
                        "user prompt flush barrier failed"
                    );
                }
            } else {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    prompt_id = %prompt_id,
                    "user prompt commit skipped: chat-state actor unavailable"
                );
            }
        }
        let turn_scope_guard =
            TurnSubagentScopeGuard::new(self.current_prompt_id.clone(), prompt_id.to_string());
        let turn_model_id = self.current_model_id().await;
        let doom_event_model = turn_model_id.clone();
        let turn_timer = std::time::Instant::now();
        turn_completion_emitter.begin_turn_work(turn_model_id.clone(), turn_timer);
        let mut turn_sampling = TurnSampling::default();
        let prompt_was_blocked = prompt_block.is_some();
        let mut result = if let Some((hook_name, reason)) = prompt_block {
            self.state.lock().await.arm_hook_block_hold();
            if let Some(kind) = redirect_kind {
                self.events.set_prior_redirect_kind(kind);
            }
            distill_telemetry::session_ctx::log_event(distill_telemetry::events::HookBlocked {
                hook_name: hook_name.clone(),
                cause: distill_telemetry::events::HookBlockCause::PromptBlocked,
            });
            self.send_hook_annotation(&format!(
                "\u{26a0} Prompt blocked by {}: {reason}",
                distill_hooks::config::hook_display_name(&hook_name)
            ))
            .await;
            Ok(TurnOutcome::Cancelled {
                category: Some(crate::session::events::CancellationCategory::HookDenied),
                context: Some(crate::session::commands::CancellationContext {
                    hook_name: Some(hook_name),
                    reason: Some(reason),
                    ..Default::default()
                }),
            })
        } else {
            let mut round_trace = trace_gcs_config;
            let mut round_artifact = artifact_tracker;
            let mut stop_continuations_this_turn: u32 = 0;
            let mut salvage =
                super::length_salvage::LengthSalvage::new(self.length_salvage_budget());
            loop {
                if self.goal_harness_enabled() {
                    let goal_loop_active = self.goal_tracker.lock().status()
                        == Some(crate::session::goal_tracker::GoalStatus::Active);
                    self.set_goal_loop_active_resource(goal_loop_active).await;
                }
                let round = self
                    .process_conversation_turn_with_recovery(
                        prompt_id,
                        round_trace.take(),
                        round_artifact.take(),
                        json_schema.clone(),
                        &mut salvage,
                        &mut turn_sampling,
                    )
                    .await;
                if !matches!(round, Ok(TurnOutcome::Completed { .. })) {
                    break round;
                }
                if matches!(
                    round,
                    Ok(TurnOutcome::Completed {
                        stop: CompletedStop::Refusal(_),
                        ..
                    })
                ) {
                    self.auto_pause_goal_if_active_with_message(
                        crate::session::goal_tracker::GoalPauseReason::Infra,
                        "The model provider refused this goal round. Use /goal resume to retry."
                            .to_string(),
                    )
                    .await;
                    break round;
                }
                let goal_active = laziness_injection_active(
                    self.goal_harness_enabled(),
                    self.goal_tracker.lock().status(),
                );
                if goal_active {
                    if self.has_runnable_queued_user_row().await {
                        distill_telemetry::unified_log::info(
                            "shell.goal.yielded_to_queued_input",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({ "prompt_id": prompt_id })),
                        );
                        tracing::info!(
                            "goal turn: yielding to queued user prompts; continuation re-arms \
                             at turn end"
                        );
                        break round;
                    }
                    if crate::session::PromptOrigin::from_prompt_id(prompt_id).is_synthetic()
                        || !self.has_pending_goal_continuation().await
                    {
                        let decision = if self.goal_runs_on_workflow_engine() {
                            self.run_goal_round_end().await
                        } else {
                            self.run_goal_round_end_legacy().await
                        };
                        if let GoalRoundDecision::Continue(directive) = decision {
                            salvage.round_boundary();
                            self.inject_goal_continuation_message(directive).await;
                            continue;
                        }
                    } else {
                        tracing::info!(
                            "goal turn: user prompt runs standalone; a queued continuation \
                             resumes the goal"
                        );
                    }
                }
                match self
                    .run_stop_gate(prompt_id, stop_continuations_this_turn)
                    .await
                {
                    StopGateDecision::AllowStop => break round,
                    StopGateDecision::KeepWorking { feedback } => {
                        stop_continuations_this_turn += 1;
                        salvage.round_boundary();
                        self.chat_state_handle
                            .push_user_message(ConversationItem::stop_hook_feedback(feedback));
                    }
                }
            }
        };
        if matches!(
            result,
            Ok(TurnOutcome::Cancelled { .. }) | Ok(TurnOutcome::MaxTurnsReached { .. })
        ) {
            self.cancel_running_turn_subagents(prompt_id);
        }
        let mut flush_error = self.flush_to_disk().await.err();
        self.file_state_tracker
            .end_prompt(&self.tool_context.fs, current_prompt_index)
            .await;
        if let Some(mut rewind_point) = self
            .file_state_tracker
            .get_rewind_point(current_prompt_index)
            .await
        {
            rewind_point.normalize_to_relative(self.tool_context.cwd.as_ref());
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::RewindPoint(rewind_point));
            if flush_error.is_none() {
                flush_error = self.flush_to_disk().await.err();
            }
        }
        if matches!(
            &result,
            Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded)
        ) && let Err(error) = self.disk_full_acp_error(flush_error.as_ref())
        {
            result = Err(error);
        }
        let turn_duration_ms =
            super::turn_task::elapsed_ms_saturating(turn_timer, std::time::Instant::now());
        let handle_prompt_elapsed_ms = handle_prompt_start.elapsed().as_millis() as u64;
        distill_telemetry::unified_log::info(
            "shell.handle_prompt.done",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "prompt_id": prompt_id,
                "total_elapsed_ms": handle_prompt_elapsed_ms,
                "turn_elapsed_ms": turn_duration_ms,
                "pre_turn_ms": handle_prompt_elapsed_ms.saturating_sub(turn_duration_ms),
                "ok": result.is_ok(),
            })),
        );
        let turn_tool_count = self.events.tool_count_this_turn();
        if !prompt_was_blocked
            && let Ok(Some(bill)) = self.chat_state_handle.try_get_prompt_usage().await
        {
            turn_completion_emitter.record_turn_usage(
                bill.totals.total_tokens(),
                self.chat_state_handle
                    .try_get_estimated_total_tokens()
                    .await,
            );
        }
        turn_completion_emitter.emit(
            TurnTelemetryOutcome::from_result(&result),
            turn_duration_ms,
            turn_tool_count,
        );
        let bridge_outcome = turn_result_to_hook_outcome(&result);
        self.observability_bridge
            .emit(
                distill_tool_protocol::session_event::SessionEvent::TurnEnded {
                    turn_number: current_prompt_index as u64,
                    outcome: bridge_outcome,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                },
            )
            .await;
        if distill_telemetry::external::is_active() {
            let committed = self
                .chat_state_handle
                .get_assistant_text_in_turn()
                .await
                .unwrap_or_default();
            let captured = self.streaming_turn_capture.lock().assembled_response_text();
            let trust_committed = matches!(
                &result,
                Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded)
            );
            let response_text = crate::session::streaming_capture::StreamingTurnCapture::merge_assistant_response_for_otel(
                committed,
                &captured,
                trust_committed,
            );
            distill_telemetry::external::emit(&distill_telemetry::events::AssistantResponse {
                response_length: response_text.len(),
                response_text: (!response_text.is_empty()).then_some(response_text),
            });
        }
        match &result {
            Ok(TurnOutcome::Completed { stop, .. }) => {
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Completed,
                    None,
                    None,
                );
                if let CompletedStop::Refusal(explanation) = stop {
                    self.report_turn_end(
                        prompt_id,
                        TurnEnd::Failed {
                            error: distill_hooks::event::StopFailureKind::InvalidRequest,
                            error_details: None,
                            last_assistant_message: (!explanation.is_empty())
                                .then(|| explanation.clone()),
                        },
                    );
                }
                self.send_after_turn_event(distill_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: distill_tool_protocol::turn_hook::TurnHookOutcome::Completed,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                })
                .await;
            }
            Ok(TurnOutcome::StationarityEnded) => {
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Completed,
                    None,
                    None,
                );
                self.send_after_turn_event(distill_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: distill_tool_protocol::turn_hook::TurnHookOutcome::Completed,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: Some(
                        crate::session::commands::ACTION_STATIONARITY_CATEGORY.to_string(),
                    ),
                    cancellation_context: None,
                })
                .await;
            }
            Ok(TurnOutcome::Cancelled { category, context }) => {
                let context_json = context.as_ref().and_then(|c| serde_json::to_value(c).ok());
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Cancelled,
                    *category,
                    context_json.clone(),
                );
                if let Some(cause) = category
                    && crate::session::events::prior_turn_interrupt_from_cancellation(*cause)
                        .is_some()
                {
                    self.events.set_prior_interrupt_category(*cause);
                }
                self.send_after_turn_event(distill_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: distill_tool_protocol::turn_hook::TurnHookOutcome::Cancelled,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: cancellation_category_to_wire_string(*category),
                    cancellation_context: context_json,
                })
                .await;
            }
            Ok(TurnOutcome::MaxTurnsReached { limit }) => {
                tracing::info!(limit, "turn ended: max_turns reached");
                self.emit_turn_ended(
                    crate::session::events::TurnOutcomeLabel::Cancelled,
                    None,
                    Some(serde_json::json!({
                        "reason": "max_turns_reached",
                        "limit": limit,
                    })),
                );
                self.send_after_turn_event(distill_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: distill_tool_protocol::turn_hook::TurnHookOutcome::Cancelled,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: Some(serde_json::json!({
                        "reason": "max_turns_reached",
                        "limit": limit,
                    })),
                })
                .await;
            }
            Err(err) => {
                self.emit_turn_ended(crate::session::events::TurnOutcomeLabel::Error, None, None);
                self.send_after_turn_event(distill_tool_protocol::turn_hook::AfterTurnPayload {
                    turn_number: current_prompt_index as u64,
                    outcome: distill_tool_protocol::turn_hook::TurnHookOutcome::Error,
                    duration_ms: turn_duration_ms,
                    tool_call_count: turn_tool_count,
                    model_id: turn_model_id.clone(),
                    written_repo_paths: Vec::new(),
                    cancellation_category: None,
                    cancellation_context: None,
                })
                .await;
                distill_telemetry::session_ctx::log_session_event(
                    distill_telemetry::events::ApiError {
                        error_category: Self::turn_error_fields(err).0,
                        model_id: turn_model_id.clone(),
                        status_code: None,
                        duration_ms: Some(turn_duration_ms),
                    },
                );
                self.report_turn_end(
                    prompt_id,
                    TurnEnd::Failed {
                        error: Self::stop_failure_error_type(err),
                        error_details: Self::turn_error_detail(err),
                        last_assistant_message: Some(Self::format_turn_error_message(err)),
                    },
                );
            }
        }
        distill_telemetry::session_ctx::log_session_event(
            crate::agent::session_metrics::TurnCompletedLifecycle {
                session_id: self.session_info.id.0.to_string(),
                turn_number: current_prompt_index as u64,
            },
        );
        let doom_tally = std::mem::take(&mut *self.doom_loop_turn_tally.lock());
        if doom_tally.detected() {
            let summary = doom_tally.detection_summary();
            distill_telemetry::session_ctx::log_session_event(
                crate::agent::session_metrics::DoomLoopDetected {
                    session_id: self.session_info.id.0.to_string(),
                    turn_number: current_prompt_index as u64,
                    trigger_count: doom_tally.triggers.len() as u32,
                    detector_kinds: summary.detector_kinds,
                    channels: summary.channels,
                    tightest_tail_threshold: summary.tightest_tail_threshold,
                    max_exact_sequence_tokens: summary.max_exact_sequence_tokens,
                    max_exact_repeat_count: summary.max_exact_repeat_count,
                    recovery_attempts: doom_tally.attempts,
                    model: doom_event_model.clone(),
                },
            );
        }
        if doom_tally.fired() {
            distill_telemetry::session_ctx::log_session_event(
                crate::agent::session_metrics::DoomLoopRecovery {
                    session_id: self.session_info.id.0.to_string(),
                    turn_number: current_prompt_index as u64,
                    attempts: doom_tally.attempts,
                    accepted_after_budget: doom_tally.accepted_after_budget,
                    top_trigger: doom_tally.top_trigger,
                    model: doom_event_model,
                },
            );
        }
        match &result {
            Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded) => {
                for contributor in self.extension_registry.turn_lifecycle_contributors() {
                    contributor
                        .on_turn_done(&distill_agent_lifecycle::TurnDoneInput)
                        .await;
                }
            }
            Ok(TurnOutcome::Cancelled { .. }) | Ok(TurnOutcome::MaxTurnsReached { .. }) => {
                self.notify_turn_abort(
                    self.turn_report.epoch(),
                    distill_agent_lifecycle::TurnAbortReason::Interrupted,
                )
                .await;
            }
            Err(err) => {
                let message = err.to_string();
                let input = distill_agent_lifecycle::TurnErrorInput { message: &message };
                for contributor in self.extension_registry.turn_lifecycle_contributors() {
                    contributor.on_turn_error(&input).await;
                }
            }
        }
        let usage = self.freeze_prompt_usage(prompt_id).await;
        self.persist_live_usage().await;
        drop(turn_scope_guard);
        match result {
            Ok(outcome) => {
                self.chat_state_handle.flush();
                let total_tokens = self.chat_state_handle.get_total_tokens().await;
                let (stop_reason, completion_kind, structured_output) = match outcome {
                    TurnOutcome::Completed {
                        structured_output,
                        stop,
                        ..
                    } => (
                        match stop {
                            CompletedStop::Refusal(_) => acp::StopReason::Refusal,
                            CompletedStop::MaxTokens => acp::StopReason::MaxTokens,
                            CompletedStop::EndTurn => acp::StopReason::EndTurn,
                        },
                        PromptCompletionKind::Completed,
                        structured_output,
                    ),
                    TurnOutcome::StationarityEnded => (
                        acp::StopReason::EndTurn,
                        PromptCompletionKind::StationarityEnded,
                        None,
                    ),
                    TurnOutcome::Cancelled { category, context } => (
                        acp::StopReason::Cancelled,
                        PromptCompletionKind::Cancelled { category, context },
                        None,
                    ),
                    TurnOutcome::MaxTurnsReached { limit } => (
                        acp::StopReason::Cancelled,
                        PromptCompletionKind::MaxTurnsReached { limit },
                        None,
                    ),
                };
                let turn_snapshot = match completion_kind {
                    PromptCompletionKind::Completed | PromptCompletionKind::StationarityEnded => {
                        self.take_completed_turn_snapshot(&turn_sampling).await
                    }
                    _ => None,
                };
                Ok(crate::session::commands::PromptTurnOk {
                    stop_reason,
                    total_tokens,
                    turn_snapshot,
                    completion_kind,
                    structured_output,
                    usage,
                    tool_overrides: None,
                })
            }
            Err(e) => Err(crate::sampling::error::attach_prompt_usage(e, usage)),
        }
    }
    /// Wait for turn-blocking subagents (up to [`SUBAGENT_USAGE_DRAIN`] on the turn task), snapshot, clear sticky.
    /// Background children never gate the drain: the prompt report is marked incomplete immediately and their spend reaches the session ledger when they finish.
    /// Cancel intentionally skips this multi-second drain (actor-loop safety).
    #[tracing::instrument(
        name = "session.freeze_prompt_usage",
        skip_all,
        fields(session_id = %self.session_info.id.0, prompt_id = %prompt_id)
    )]
    pub(super) async fn freeze_prompt_usage(
        &self,
        prompt_id: &str,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        self.freeze_prompt_usage_bounded(prompt_id, SUBAGENT_USAGE_DRAIN)
            .await
    }
    /// [`freeze_prompt_usage`] with an explicit drain bound, for tests.
    pub(super) async fn freeze_prompt_usage_bounded(
        &self,
        prompt_id: &str,
        max_wait: std::time::Duration,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let drain = self
            .drain_subagent_usage_for_prompt_bounded(prompt_id, max_wait)
            .await;
        let mut usage = self.finalize_usage_from_outcome(prompt_id, drain).await;
        // The turn report rides the same payload as the bill: one terminal, one
        // place to read where the task went.
        if let Some(usage) = usage.as_mut() {
            self.attach_turn_distribution(usage);
        }
        usage
    }
    /// Must run on the turn task, not the session actor loop, so folds can land.
    pub(super) async fn drain_subagent_usage_for_prompt_bounded(
        &self,
        prompt_id: &str,
        max_wait: std::time::Duration,
    ) -> UsageDrainOutcome {
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return UsageDrainOutcome {
                fail_closed: false,
                background_live: false,
                sticky_report: false,
            };
        };
        let coordinator_gone = || {
            tracing::warn!(
                prompt_id,
                "outstanding subagent query failed; treating usage as incomplete"
            );
            UsageDrainOutcome {
                fail_closed: true,
                background_live: false,
                sticky_report: false,
            }
        };
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::WaitPromptDrained(
                SubagentWaitPromptDrainedRequest {
                    parent_session_id: self.session_id_string(),
                    prompt_id: prompt_id.to_string(),
                    respond_to,
                },
            ))
            .is_err()
        {
            return coordinator_gone();
        }
        match tokio::time::timeout(max_wait, rx).await {
            Ok(Ok(reply)) => UsageDrainOutcome {
                fail_closed: false,
                background_live: reply.background_live,
                sticky_report: reply.subagent_usage_not_applied,
            },
            Ok(Err(_)) => coordinator_gone(),
            Err(_) => {
                tracing::warn!(
                    prompt_id,
                    max_wait_ms = max_wait.as_millis() as u64,
                    "subagent usage drain timed out; usage may under-count"
                );
                UsageDrainOutcome {
                    fail_closed: true,
                    background_live: false,
                    sticky_report: false,
                }
            }
        }
    }
    pub(super) async fn snapshot_prompt_usage_marked(
        &self,
        incomplete: bool,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let actor_background_spend = self
            .unattributed_background_usage
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let shared_background_spend = self
            .tool_context
            .unattributed_background_usage
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let incomplete = incomplete || actor_background_spend || shared_background_spend;
        match self.chat_state_handle.try_get_prompt_usage().await {
            Ok(ledger) => {
                let incomplete = incomplete || ledger.as_ref().is_some_and(|l| l.incomplete);
                crate::extensions::notification::PromptUsage::project_from_ledger(
                    ledger.as_ref(),
                    incomplete,
                )
            }
            Err(()) => {
                crate::extensions::notification::PromptUsage::project_from_ledger(None, true)
            }
        }
    }
    /// When freeze did not attach: incomplete if billed or may under-count; else omit.
    pub(super) async fn error_path_usage_fallback(
        &self,
        prompt_id: &str,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        let may_undercount = Self::usage_incomplete_from_reply(
            self.outstanding_reply_for_prompt(prompt_id).await.as_ref(),
        );
        match self.chat_state_handle.try_get_prompt_usage().await {
            Ok(ledger) => crate::extensions::notification::PromptUsage::for_error_path(
                ledger.as_ref(),
                may_undercount,
            ),
            Err(()) => crate::extensions::notification::PromptUsage::for_error_path(None, true),
        }
    }
    /// Sticky incomplete for `prompt_id`, or the live pin when `None`.
    /// Returns true only if the coordinator acked the mark.
    pub(super) async fn mark_subagent_usage_not_applied(&self, prompt_id: Option<&str>) -> bool {
        let resolved = prompt_id
            .map(str::to_owned)
            .or_else(|| self.current_prompt_id.lock().ok().and_then(|g| g.clone()));
        let Some(pid) = resolved else {
            self.unattributed_background_usage
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.tool_context
                .unattributed_background_usage
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return false;
        };
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return false;
        };
        let (respond_to, ack) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::MarkUsageNotApplied(
                SubagentMarkUsageNotAppliedRequest {
                    parent_session_id: self.session_id_string(),
                    prompt_id: pid,
                    respond_to,
                },
            ))
            .is_err()
        {
            return false;
        }
        ack.await.is_ok()
    }
    /// The message is tagged `SyntheticReason::SystemReminder` so compaction/fork/pruning skip it.
    /// The latter persists a `UserMessageChunk` to `updates.jsonl`, which resume replays; the raw XML would render as a user prompt.
    /// Clients see monitor events only via the structured `x.ai/monitor_event` channel.
    pub(crate) async fn inject_pending_monitor_events(&self) {
        let Some(buffer) = &self.tool_context.monitor_event_buffer else {
            return;
        };
        let mine = distill_tools::implementations::distill::monitor::types::drain_owned(
            buffer,
            Some(self.session_info.id.0.as_ref()),
        );
        if mine.is_empty() {
            return;
        }
        let Some(body) = distill_tools::reminders::task_completion::format_monitor_events(
            &mine,
            Some(&self.tool_context.task_output_tool_name),
        ) else {
            return;
        };
        let wrapped = distill_tools::reminders::wrap_reminder(&body);
        self.chat_state_handle
            .push_user_message(ConversationItem::system_reminder(wrapped));
        tracing::info!(
            session_id = %self.session_info.id.0,
            count = mine.len(),
            "injected mid-turn monitor events as hidden synthetic user message"
        );
    }
    /// Success. Reset `goal_continuation_streak` to 0. Then call `maybe_queue_goal_continuation` unless `suppress_goal_continuation` (stationarity silent EndTurn).
    /// When the goal is not `Active` (`goal_active_now == false`), both branches are skipped.
    /// Neither streak moves and the existing pause cause is preserved.
    pub(crate) async fn handle_turn_end(
        &self,
        turn_succeeded: bool,
        suppress_goal_continuation: bool,
    ) {
        let goal_active_now = laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        );
        if turn_succeeded && goal_active_now {
            self.goal_continuation_streak
                .store(0, std::sync::atomic::Ordering::Relaxed);
            if !suppress_goal_continuation {
                self.maybe_queue_goal_continuation().await;
            }
            return;
        }
        if !turn_succeeded && goal_active_now {
            let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
            if self.enforce_goal_token_budget(current_tokens).await {
                return;
            }
            let streak = self
                .goal_continuation_streak
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if streak >= GOAL_CONTINUATION_BACKOFF_THRESHOLD {
                self.goal_continuation_streak
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                self.auto_pause_goal_if_active(
                    crate::session::goal_tracker::GoalPauseReason::BackOff,
                )
                .await;
                self.send_slash_command_output(&format!(
                    "Goal auto-paused after {GOAL_CONTINUATION_BACKOFF_THRESHOLD} consecutive \
                     non-completing turns. The model is not making progress. \
                     Use /goal resume to retry or /goal clear to abandon."
                ))
                .await;
            }
        }
    }
    /// Agents with a `completion_requirement` in their definition require the model to call a specific tool before finishing.
    /// If a prompt turn ends without that tool called, this method injects the recovery prompt and re-runs the turn with exponential backoff.
    /// Agents without `completion_requirement` bypass this entirely.
    #[tracing::instrument(
        name = "session.process_conversation_turn_with_recovery",
        skip_all,
        err,
        fields(req_id = %req_id, session_id = %self.session_info.id.0)
    )]
    pub(super) async fn process_conversation_turn_with_recovery(
        self: &Arc<Self>,
        req_id: &str,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<crate::upload::manifest::ArtifactTracker>,
        json_schema: Option<serde_json::Value>,
        salvage: &mut super::length_salvage::LengthSalvage,
        turn_sampling: &mut TurnSampling,
    ) -> Result<TurnOutcome, acp::Error> {
        let _ = self.compaction.auto_compact_suppressed.compare_exchange(
            crate::session::compaction_config::SUPPRESS_TURN,
            crate::session::compaction_config::SUPPRESS_NONE,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        );
        let agent_ref = self.agent.borrow();
        let completion_req = match agent_ref.completion_requirement() {
            Some(req) => req,
            None => {
                return self
                    .process_conversation_turn(
                        req_id,
                        trace_gcs_config,
                        artifact_tracker.as_ref(),
                        json_schema,
                        &mut *salvage,
                        &mut *turn_sampling,
                    )
                    .await;
            }
        };
        let recovery = match &completion_req.recovery {
            Some(r) => r.clone(),
            None => {
                return self
                    .process_conversation_turn(
                        req_id,
                        trace_gcs_config,
                        artifact_tracker.as_ref(),
                        json_schema,
                        &mut *salvage,
                        &mut *turn_sampling,
                    )
                    .await;
            }
        };
        let required_tool = completion_req.tool.clone();
        let recovery_prompt = completion_req.reminder.clone();
        let mut result = self
            .process_conversation_turn(
                req_id,
                trace_gcs_config.clone(),
                artifact_tracker.as_ref(),
                json_schema.clone(),
                &mut *salvage,
                &mut *turn_sampling,
            )
            .await;
        if matches!(
            result,
            Ok(TurnOutcome::MaxTurnsReached { .. }) | Ok(TurnOutcome::StationarityEnded)
        ) {
            return result;
        }
        if let Ok(TurnOutcome::Completed {
            ref tools_called, ..
        }) = result
            && tools_called.iter().any(|name| name == &required_tool)
        {
            tracing::info!(
                "Completion requirement satisfied (tool '{}' called) for session {}",
                required_tool,
                self.session_info.id.0,
            );
            return result;
        }
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let error_desc = match &result {
                Ok(_) => "Agent finished without completing required task".into(),
                Err(e) => format!("{e:?}"),
            };
            if attempt > recovery.max_retries {
                tracing::error!(
                    "Auto-recovery exhausted after {attempt} attempts for session {}: {error_desc}",
                    self.session_info.id.0,
                );
                self.send_xai_notification(XaiSessionUpdate::AutoRecoveryExhausted {
                    attempts: attempt,
                    error: error_desc,
                })
                .await;
                return result;
            }
            let delay_ms = std::cmp::min(
                recovery.base_delay_ms * 2u64.pow(attempt.saturating_sub(1)),
                recovery.max_delay_ms,
            );
            let delay = std::time::Duration::from_millis(delay_ms);
            tracing::warn!(
                "Auto-recovery attempt {}/{} for session {}: {error_desc}. Retrying in {}ms",
                attempt,
                recovery.max_retries,
                self.session_info.id.0,
                delay.as_millis(),
            );
            self.send_xai_notification(XaiSessionUpdate::AutoRecoveryStarted {
                attempt,
                max_retries: recovery.max_retries,
                error: error_desc,
                delay_ms: delay.as_millis() as u64,
            })
            .await;
            sleep(delay).await;
            salvage.round_boundary();
            let recovery_message = ConversationItem::auto_recovery(recovery_prompt.clone());
            self.chat_state_handle.push_user_message(recovery_message);
            result = self
                .process_conversation_turn(
                    req_id,
                    trace_gcs_config.clone(),
                    artifact_tracker.as_ref(),
                    None,
                    &mut *salvage,
                    &mut *turn_sampling,
                )
                .await;
            if matches!(
                result,
                Ok(TurnOutcome::MaxTurnsReached { .. }) | Ok(TurnOutcome::StationarityEnded)
            ) {
                return result;
            }
            if let Ok(TurnOutcome::Completed {
                ref tools_called, ..
            }) = result
                && tools_called.iter().any(|name| name == &required_tool)
            {
                tracing::info!(
                    "Completion requirement satisfied after {} recovery attempt(s) \
                     (tool '{}' called) for session {}",
                    attempt,
                    required_tool,
                    self.session_info.id.0,
                );
                return result;
            }
        }
    }
    pub(super) fn is_first_turn_memory_score_visible(score: f64) -> bool {
        format!("{score:.2}") != "0.00"
    }
    /// Compute the first-turn memory reminder, if one should be injected.
    /// A block persisted by an earlier session segment (a prior `--resume` process, or a turn before a compaction) is reused verbatim.
    /// [`conversation_has_memory_context`]: crate::session::helpers::memory_context::conversation_has_memory_context.
    pub(crate) async fn first_turn_memory_reminder(&self) -> Option<String> {
        if self
            .memory
            .context_injected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let is_v2_mode =
            self.memory.is_enabled() && self.memory.mode() == Some(crate::config::MemoryMode::V2);
        let is_v2 = self.memory.can_expose_v2();
        self.memory
            .context_injected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if is_v2_mode && !is_v2 {
            return None;
        }
        if is_v2 {
            let conversation = self.chat_state_handle.get_conversation().await;
            if crate::session::helpers::memory_context::conversation_has_memory_context(
                &conversation,
            ) {
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    "MEMORY_INJECT: persisted v2 manifests reused to preserve prompt cache"
                );
                let block = conversation.first().and_then(|item| match item {
                    distill_sampling_types::ConversationItem::System(sys) => sys
                        .content
                        .find(distill_chat_state::MEMORY_CONTEXT_OPEN_TAG)
                        .and_then(|start| sys.content.get(start..)),
                    _ => None,
                });
                let injected_bytes = block.map(|text| text.len() as u64).unwrap_or(0);
                let estimated_tokens = block
                    .map(distill_token_estimation::estimate_tokens)
                    .unwrap_or(0);
                if injected_bytes > 0 {
                    self.memory.record_injected_bytes(injected_bytes);
                }
                crate::session::memory_observation::log_memory_injection(
                    self.session_info.id.to_string(),
                    distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Skipped,
                    crate::session::memory_observation::MemoryInjectionMetrics {
                        injected_bytes,
                        estimated_tokens,
                        was_reused: true,
                        ..Default::default()
                    },
                );
                return None;
            }
            let inject_start = std::time::Instant::now();
            let storage = self.memory.storage()?;
            let context = tokio::task::spawn_blocking(move || {
                crate::session::helpers::memory_context::format_v2_memory_context(&storage)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(std::convert::identity);
            return match context {
                Ok(context) => {
                    let injected_bytes = context.content.len() as u64;
                    self.memory.record_injected_bytes(injected_bytes);
                    crate::session::memory_observation::log_memory_injection(
                        self.session_info.id.to_string(),
                        distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Results,
                        crate::session::memory_observation::MemoryInjectionMetrics {
                            result_count: context
                                .global_entry_count
                                .saturating_add(context.workspace_entry_count),
                            injected_bytes,
                            estimated_tokens: distill_token_estimation::estimate_tokens(
                                &context.content,
                            ),
                            global_entry_count: context.global_entry_count,
                            workspace_entry_count: context.workspace_entry_count,
                            duration_ms: inject_start.elapsed().as_millis() as u64,
                            ..Default::default()
                        },
                    );
                    self.memory
                        .injection_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(context.content)
                }
                Err(error) => {
                    tracing::warn!(
                        target: distill_telemetry::memory_log::TARGET,
                        %error,
                        "MEMORY_INJECT: failed to generate v2 manifest context"
                    );
                    crate::session::memory_observation::log_memory_injection(
                        self.session_info.id.to_string(),
                        distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Error,
                        Default::default(),
                    );
                    None
                }
            };
        }
        if !self.memory.initial_injection_config.enabled {
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_INJECT: first-turn injection disabled by config"
            );
            crate::session::memory_observation::log_memory_injection(
                self.session_info.id.to_string(),
                distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Skipped,
                Default::default(),
            );
            return None;
        }
        let (Some(storage), Some(params)) =
            (self.memory.storage(), self.memory.backend_params.as_ref())
        else {
            crate::session::memory_observation::log_memory_injection(
                self.session_info.id.to_string(),
                distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Skipped,
                Default::default(),
            );
            return None;
        };
        let conversation = self.chat_state_handle.get_conversation().await;
        if crate::session::helpers::memory_context::conversation_has_memory_context(&conversation) {
            tracing::info!(
                target: distill_telemetry::memory_log::TARGET,
                "MEMORY_INJECT: existing memory-context block present in system message -- skipping re-injection to preserve prompt cache"
            );
            crate::session::memory_observation::log_memory_injection(
                self.session_info.id.to_string(),
                distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Skipped,
                Default::default(),
            );
            return None;
        }
        use distill_tools::types::memory_backend::MemoryBackend as _;
        let (injection_params, configured_min_score) =
            build_initial_injection_backend_params(params, &self.memory.initial_injection_config);
        let backend = crate::session::memory::MemoryBackendImpl::from_session_params(
            storage,
            &injection_params,
        );
        let raw_query =
            crate::session::helpers::session_compact::extract_last_real_user_query(&conversation)
                .unwrap_or_default();
        let was_greeting = raw_query.is_empty()
            || raw_query.len() < 20
            || crate::session::helpers::memory_context::is_greeting(&raw_query);
        let query = if was_greeting {
            "project conventions preferences architecture".to_string()
        } else {
            raw_query
        };
        let inject_start = std::time::Instant::now();
        let inject_search_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
            "memory.inject_search",
            result_count = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        ));
        let search_result = backend.search(&query, 6, configured_min_score).await;
        inject_search_span
            .span()
            .record("elapsed_ms", inject_start.elapsed().as_millis() as i64);
        inject_search_span.span().record(
            "result_count",
            search_result.as_ref().map(|r| r.len()).unwrap_or(0) as i64,
        );
        inject_search_span.close();
        let (outcome, mut inject_results) = match search_result {
            Ok(results) if results.is_empty() => (
                distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Empty,
                results,
            ),
            Ok(results) => (
                distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Results,
                results,
            ),
            Err(error) => {
                tracing::warn!(
                    target: distill_telemetry::memory_log::TARGET,
                    %error,
                    "MEMORY_INJECT_SEARCH: search failed"
                );
                (
                    distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Error,
                    Vec::new(),
                )
            }
        };
        inject_results.retain(|result| Self::is_first_turn_memory_score_visible(result.score));
        // A5: rank the candidates with Jev before injecting them. The search
        // already narrowed; this pass only removes what looks irrelevant.
        inject_results = self.jev_rank_memory(&query, inject_results).await;
        let outcome = if outcome
            == distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Results
            && inject_results.is_empty()
        {
            distill_telemetry::memory_telemetry::MemoryInjectionOutcome::Empty
        } else {
            outcome
        };
        let result_count = inject_results.len();
        let top_score = inject_results.first().map_or(0.0, |result| result.score);
        let total_snippet_chars = inject_results
            .iter()
            .map(|result| result.snippet.len())
            .sum();
        tracing::info!(
            target: distill_telemetry::memory_log::TARGET,
            configured_min_score,
            result_count,
            "MEMORY_INJECT_SEARCH: completed"
        );
        crate::session::memory_observation::log_memory_injection(
            self.session_info.id.to_string(),
            outcome,
            crate::session::memory_observation::MemoryInjectionMetrics {
                is_greeting_fallback: was_greeting,
                result_count,
                total_snippet_chars,
                top_score,
                configured_min_score,
                duration_ms: inject_start.elapsed().as_millis() as u64,
                ..Default::default()
            },
        );
        crate::session::helpers::memory_context::format_memory_reminder(&inject_results)
    }
    /// Inspect `tool_calls` for a `StructuredOutput` call and decide the turn's next step, pushing the call's `tool_result` as a side effect.
    /// The pushed result is a correction, a retry error, or the terminal answer.
    /// Validates the args against `validator` and bumps `retries` on a non-conforming retry.
    async fn handle_structured_output_tool_call(
        &self,
        tool_calls: &mut Vec<distill_sampling_types::conversation::ToolCall>,
        validator: &Result<jsonschema::Validator, String>,
        retries: &mut u32,
    ) -> StructuredOutputStep {
        let Some(pos) = tool_calls
            .iter()
            .position(|tc| tc.name == STRUCTURED_OUTPUT_TOOL)
        else {
            return StructuredOutputStep::Proceed;
        };
        if tool_calls.len() > 1 {
            for tc in tool_calls
                .iter()
                .filter(|tc| tc.name == STRUCTURED_OUTPUT_TOOL)
            {
                self.chat_state_handle
                    .push_tool_result(ConversationItem::tool_result(
                        tc.id.as_ref().to_owned(),
                        "Call StructuredOutput alone, exactly once, after all other tools finish.",
                    ));
            }
            tool_calls.retain(|tc| tc.name != STRUCTURED_OUTPUT_TOOL);
            return StructuredOutputStep::Proceed;
        }
        let Some(tc) = tool_calls.get(pos) else {
            return StructuredOutputStep::Proceed;
        };
        let call_id = tc.id.as_ref().to_owned();
        let validated = validate_structured_output(validator, &tc.arguments);
        if let Err(err) = &validated
            && *retries < STRUCTURED_OUTPUT_MAX_RETRIES
        {
            *retries += 1;
            self.chat_state_handle
                .push_tool_result(ConversationItem::tool_result(
                    call_id,
                    format!("{err}\nFix the arguments and call StructuredOutput again."),
                ));
            return StructuredOutputStep::Retry;
        }
        self.chat_state_handle
            .push_tool_result(ConversationItem::tool_result(
                call_id,
                match &validated {
                    Ok(_) => "Structured output accepted.".to_string(),
                    Err(err) => err.clone(),
                },
            ));
        StructuredOutputStep::Complete(validated)
    }
    /// Single shell tool call whose parsed command is `true` (via ToolBridge).
    async fn is_run_true_step(
        &self,
        tool_calls: &[distill_sampling_types::conversation::ToolCall],
    ) -> bool {
        let [tc] = tool_calls else {
            return false;
        };
        let Ok(args) = serde_json::from_str::<serde_json::Value>(tc.arguments.as_ref()) else {
            return false;
        };
        let Ok(input) = self.tool_bridge_handle().try_parse(&tc.name, args).await else {
            return false;
        };
        match input {
            ToolInput::Bash(b) => command_is_true(&b.command),
            _ => false,
        }
    }
    async fn persist_live_usage(&self) {
        let Some(signals) = self.signals_handle().snapshot().await else {
            return;
        };
        if signals.turn_count == 0 {
            return;
        }
        match self.chat_state_handle.try_get_session_usage().await {
            Ok(ledger) => {
                let _ = self
                    .notifications
                    .persistence_tx
                    .send(PersistenceMsg::UsageTurn {
                        turn_number: signals.turn_count,
                        live: crate::session::usage_file::UsageSummary::from_ledger(&ledger),
                    });
            }
            Err(()) => {
                tracing::warn!(
                    turn_number = signals.turn_count,
                    "failed to snapshot session usage for persist"
                );
            }
        }
    }
    /// Shared round-completion bookkeeping (plan cleanup, cancel-streak reset, token sums, feedback prompt).
    /// Runs identically for the native and StructuredOutput-tool completion paths.
    /// The turn-end snapshot is taken once per turn after the last round (`take_completed_turn_snapshot`), and the analytics delta posts from the turn's terminal.
    async fn finalize_turn_bookkeeping(
        &self,
        req_id: &str,
        round: TurnSpanTotals,
        turn_sampling: &mut TurnSampling,
    ) {
        self.emit_turn_end_plan_cleanup().await;
        self.signals_handle().record_turn_complete();
        turn_sampling.fold(round);
        if let Some(request) = self
            .feedback_manager
            .maybe_request_feedback(Some(req_id.to_string()))
            .await
        {
            self.send_feedback_notification(request).await;
        }
    }
    /// The completed turn's one turn-end snapshot, stamped with its token sums for the turn
    /// upload; the terminal posts it as the analytics delta. `None` when the signals actor is
    /// shut down.
    pub(super) async fn take_completed_turn_snapshot(
        &self,
        sampling: &TurnSampling,
    ) -> Option<TurnDeltaSnapshot> {
        let mut snapshot = self.signals_handle().take_turn_end_snapshot().await?;
        snapshot.turn_input_tokens = sampling.input_tokens;
        snapshot.turn_output_tokens = sampling.output_tokens;
        snapshot.turn_cached_input_tokens = sampling.cache_read_tokens;
        self.apply_prompt_modes_to_snapshot(&mut snapshot);
        Some(snapshot)
    }
    /// Persist the turn-end `snapshot` and post it as the turn's analytics delta.
    /// `None` (signals actor shut down) posts nothing.
    pub(super) async fn report_turn_delta(
        &self,
        req_id: &str,
        snapshot: Option<&TurnDeltaSnapshot>,
        turn_duration_ms: Option<u64>,
        turn_outcome: prod_mc_cli_chat_proxy_types::feedback_types::TurnOutcome,
    ) {
        if let Some(snap) = snapshot {
            for pr in &snap.delta.prs_created_this_turn {
                distill_telemetry::session_ctx::log_event(distill_telemetry::events::PrCreated {
                    source: pr.source,
                    had_commit_in_session: pr.had_commit_in_session,
                });
            }
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Signals(snap.current.clone()));
        }
        self.feedback_manager
            .send_turn_delta_with_snapshot(
                snapshot,
                Some(req_id.to_string()),
                turn_duration_ms.map(|ms| i64::try_from(ms).unwrap_or(i64::MAX)),
                turn_outcome,
            )
            .await;
    }
    async fn process_conversation_turn(
        self: &Arc<Self>,
        req_id: &str,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<&crate::upload::manifest::ArtifactTracker>,
        json_schema: Option<serde_json::Value>,
        salvage: &mut super::length_salvage::LengthSalvage,
        turn_sampling: &mut TurnSampling,
    ) -> Result<TurnOutcome, acp::Error> {
        let result = crate::jev::with_session_scope_and_recorder(
            self.session_info.id.0.to_string(),
            Some(self.chat_state_handle.clone()),
            self.process_conversation_turn_inner(
                req_id,
                trace_gcs_config,
                artifact_tracker,
                json_schema,
                salvage,
                turn_sampling,
            ),
        )
        .await;
        self.turn_phases.emit_pending_latency();
        result
    }
    #[tracing::instrument(
        name = "session.process_conversation_turn",
        skip_all,
        err,
        fields(
            session_id = %self.session_info.id.0,
            model_id,
            turn_tool_count,
            turn_model_calls,
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            cache_read_tokens = tracing::field::Empty,
            stop_reason = tracing::field::Empty,
            response.has_tool_call = tracing::field::Empty,
            last_sample.stop_reason = tracing::field::Empty,
            last_sample.has_tool_call = tracing::field::Empty,
            last_sample.output_tokens = tracing::field::Empty,
            request_id = tracing::field::Empty,
            ttft_ms = tracing::field::Empty,
            mcp_server.name = tracing::field::Empty,
            mcp_tool.name = tracing::field::Empty,
            agent.name = tracing::field::Empty,
            skill.name = tracing::field::Empty,
            query_source = tracing::field::Empty,
            effort = tracing::field::Empty,
            attempt = tracing::field::Empty,
            parent_agent_id = tracing::field::Empty,
        )
    )]
    async fn process_conversation_turn_inner(
        self: &Arc<Self>,
        req_id: &str,
        trace_gcs_config: Option<crate::session::repo_changes::TraceExportConfig>,
        artifact_tracker: Option<&crate::upload::manifest::ArtifactTracker>,
        json_schema: Option<serde_json::Value>,
        salvage: &mut super::length_salvage::LengthSalvage,
        turn_sampling: &mut TurnSampling,
    ) -> Result<TurnOutcome, acp::Error> {
        *self.current_turn_span_id.lock() = tracing::Span::current().id();
        let _clear_active_dispatch = ClearActiveDispatchOnDrop(self.signals_handle());
        struct ClearTurnSpanId<'a>(&'a parking_lot::Mutex<Option<tracing::Id>>);
        impl Drop for ClearTurnSpanId<'_> {
            fn drop(&mut self) {
                *self.0.lock() = None;
            }
        }
        let _clear_turn_span_id = ClearTurnSpanId(&self.current_turn_span_id);
        let conv_turn_start = std::time::Instant::now();
        let conv_turn_clock = DualClock::now();
        let turn_phases = self.turn_phases.clone();
        turn_phases.start();
        self.maybe_refresh_model_metadata_on_resume().await;
        self.maybe_compact_on_model_switch().await?;
        self.chat_state_handle
            .record_turn_start(chrono::Utc::now().timestamp_millis());
        {
            let span = tracing::Span::current();
            if let Some(agent) = self.active_agent_type.lock().clone() {
                span.record("agent.name", agent.as_str());
            }
            if let Some(skill) = self.active_skill.lock().clone() {
                span.record("skill.name", skill.as_str());
            }
            span.record(
                "query_source",
                if self.startup_hints.is_subagent {
                    "subagent"
                } else {
                    "main"
                },
            );
            if let Some(parent) = self.startup_hints.parent_session_id.as_deref() {
                span.record("parent_agent_id", parent);
            }
        }
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        if let Some(cfg) = sampling_config.as_ref() {
            let span = tracing::Span::current();
            span.record("model_id", cfg.model.as_str());
            if let Some(effort) = cfg.reasoning_effort {
                span.record("effort", effort.as_ref());
            }
        }
        let mut prompt_timing = Some(crate::session::prompt_timing::PromptTiming::start());
        let tool_prep_start = std::time::Instant::now();
        let (tool_definitions, mcp_wait_ms) = self.prepare_tool_definitions_timed().await;
        let total_prep_ms = tool_prep_start.elapsed().as_millis() as u64;
        self.maybe_inject_mcp_connecting_reminder().await;
        self.maybe_inject_mcp_reminder().await;
        if let Some(ref mut pt) = prompt_timing {
            pt.record_tool_prep(mcp_wait_ms, total_prep_ms);
        }
        distill_telemetry::unified_log::info(
            "shell.turn.tool_prep_done",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "tool_count": tool_definitions.len(),
                "mcp_wait_ms": mcp_wait_ms,
                "total_prep_ms": total_prep_ms,
                "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
            })),
        );
        if let Some(ref gcs_config) = trace_gcs_config {
            let gcs_cfg = gcs_config.clone();
            let tool_defs = tool_definitions.clone();
            let manifest_clone = artifact_tracker.cloned();
            let auth_manager = self.auth_manager.clone();
            tokio::spawn(async move {
                crate::upload::trace::upload_tool_definitions(
                    gcs_cfg,
                    auth_manager,
                    &tool_defs,
                    manifest_clone.as_ref(),
                )
                .await;
            });
        }
        self.record_turn_model().await;
        let mut metrics_drop_guard = TurnMetrics::new();
        let mut turn_tools_called: Vec<String> = Vec::new();
        let mut tool_turn_count: usize = 1;
        let mut loop_index: u32 = 0;
        let mut identical_tool_calls = IdenticalToolCallRun::default();
        let mut todo_gate_fires: u32 = 0;
        let mut length_salvage_streak = LengthSalvageStreak::default();
        let mut auth_retry_schedule = AuthRetrySchedule::new();
        let mut rate_limit_waits = self.rate_limit_wait_budget(
            sampling_config
                .as_ref()
                .and_then(|config| config.rate_limit_retry_threshold),
        );
        let mut transient_retry_attempts: u32 = 0;
        let transient_retry_enabled =
            self.transient_retry_enabled && !self.attach_non_interactive.get();
        let mut turn_span_totals = TurnSpanTotals::default();
        let mut structured_output_retries: u32 = 0;
        let mut media_gen_resamples: u32 = 0;
        let structured_output_validator = json_schema.as_ref().map(|schema| {
            jsonschema::validator_for(schema).map_err(|e| format!("invalid output schema: {e}"))
        });
        let schema_ok = matches!(structured_output_validator, Some(Ok(_)));
        let native_backend = if json_schema.is_some() {
            match self.chat_state_handle.get_sampling_config().await {
                Some(c) => c.api_backend.supports_native_schema(),
                None => {
                    tracing::warn!(
                        "structured output: no sampling config; using StructuredOutput tool"
                    );
                    false
                }
            }
        } else {
            false
        };
        let structured_output_native = schema_ok && native_backend;
        let structured_output_tool = schema_ok && !native_backend;
        if structured_output_tool {
            self.push_system_reminder(
                "A response schema is required. After any tool use, call the \
                 `StructuredOutput` tool exactly once with your final answer as its \
                 arguments; do not return the answer as text.",
            );
        }
        loop {
            self.emit_event(crate::session::events::Event::LoopStarted { loop_index });
            loop_index += 1;
            if identical_tool_calls.run_len >= identical_tool_calls.hard_stop_threshold() {
                let run_len = identical_tool_calls.run_len;
                let tool_name = identical_tool_calls.tool_name.clone();
                let true_noop = identical_tool_calls.is_true_noop_run;
                let problematically_repeating = identical_tool_calls.is_problematically_repeating();
                tracing::warn!(
                    session_id = %self.session_info.id,
                    tool_name = %tool_name,
                    run_len,
                    true_noop,
                    "action stationarity: ending turn after repeated identical tool calls"
                );
                distill_telemetry::unified_log::warn(
                    "shell.turn.action_stationarity_stop",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "loop_index": loop_index,
                        "tool_name": tool_name,
                        "run_len": run_len,
                        "true_noop": true_noop,
                        "problematically_repeating": problematically_repeating,
                    })),
                );
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::ActionStationarityStop {
                        true_noop,
                        problematically_repeating,
                        run_len,
                        tool_name: tool_name.clone(),
                    },
                );
                self.finalize_turn_bookkeeping(
                    req_id,
                    std::mem::take(&mut turn_span_totals),
                    turn_sampling,
                )
                .await;
                return Ok(TurnOutcome::StationarityEnded);
            }
            if identical_tool_calls.take_nudge() {
                let run_len = identical_tool_calls.run_len;
                let tool_name = identical_tool_calls.tool_name.clone();
                let problematically_repeating = identical_tool_calls.is_problematically_repeating();
                tracing::warn!(
                    session_id = %self.session_info.id,
                    tool_name = %tool_name,
                    run_len,
                    "action stationarity: nudging model to break repeated identical tool calls"
                );
                distill_telemetry::unified_log::warn(
                    "shell.turn.action_stationarity_nudge",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "loop_index": loop_index,
                        "tool_name": tool_name,
                        "run_len": run_len,
                        "problematically_repeating": problematically_repeating,
                    })),
                );
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::ActionStationarityNudge {
                        problematically_repeating,
                        run_len,
                        tool_name: tool_name.clone(),
                    },
                );
                let reminder = self
                    .tool_bridge_handle()
                    .render_prompt(
                        ACTION_STATIONARITY_NUDGE_TEMPLATE,
                        &serde_json::json!({
                            "tool_name": tool_name,
                            "run_len": run_len,
                        }),
                    )
                    .await
                    .unwrap_or_else(|| ACTION_STATIONARITY_NUDGE_TEMPLATE.to_string());
                self.push_system_reminder(&reminder);
            }
            if !salvage.awaiting_continuation() {
                self.drain_interjections_at_safe_point().await;
                self.flush_pending_skill_reminders().await;
                self.inject_pending_monitor_events().await;
            }
            let memory_reminder = self.first_turn_memory_reminder().await;
            if memory_reminder.is_some() {
                self.memory
                    .injection_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    target: distill_telemetry::memory_log::TARGET,
                    "MEMORY_INJECT: first-turn memory context injected"
                );
            }
            if !salvage.awaiting_continuation() {
                self.maybe_inject_mcp_reminder().await;
            }
            let turn_parked = if self.uncharged_401_park_enabled && auth_retry_schedule.is_parked()
            {
                TurnParkState::Parked
            } else {
                TurnParkState::Fresh
            };
            if self.tool_context.task_output_token_budget.is_none()
                && !turn_parked.is_parked()
                && self.two_pass_active()
                && !self.compaction.prefire.has_cache()
                && self.should_prefire_two_pass().await
                && self.compaction.prefire.try_begin()
            {
                let actor = std::sync::Arc::clone(self);
                let handle = tokio::task::spawn_local(async move {
                    actor.run_prefire_pass1().await;
                });
                self.compaction.prefire.set_handle(handle);
            }
            if self.tool_context.task_output_token_budget.is_none() && !turn_parked.is_parked() {
                self.refresh_token_if_expired().await;
            }
            if self.tool_context.task_output_token_budget.is_none()
                && !turn_parked.is_parked()
                && !salvage.awaiting_continuation()
                && let Some(trigger_info) = self.check_auto_compact_needed().await
                && let Err(e) = self.run_compact_only(trigger_info, false).await
            {
                tracing::error!(error = %e, "Pre-sampling auto-compaction failed");
                if Self::is_auth_compact_error(&e) {
                    return Err(self.surface_compact_auth_failure(e).await);
                }
            }
            let backend_search_active = self.backend_search_active();
            tracing::debug!(
                backend_search_active,
                "backend_search: turn tool resolution"
            );
            let bridge = self.agent.borrow().tool_bridge().clone();
            let messaging_grant = if bridge
                .read_resource::<
                    distill_tools::implementations::distill::task::types::AgentMessageSenderResource,
                >()
                .await
                .is_some()
                && bridge
                    .tool_for_kind(
                        distill_tools::types::tool::ToolKind::ActiveAgentMessage,
                    )
                    .await
                    .is_some()
            {
                child_tool_projection::ChildMessagingGrant::Granted
            } else {
                child_tool_projection::ChildMessagingGrant::Ungranted
            };
            let mut effective_tools: Vec<ToolSpec> =
                if let Some(ref override_tools) = self.forked_tool_override {
                    child_tool_projection::child_safe_tool_specs(
                        override_tools.clone(),
                        child_tool_projection::ChildToolProjection::VerbatimMirror,
                        messaging_grant,
                        |name| bridge.tool_kind(name),
                    )
                } else {
                    let tools = self.turn_base_tool_specs(&tool_definitions);
                    if self.startup_hints.is_subagent {
                        child_tool_projection::child_safe_tool_specs(
                            tools,
                            child_tool_projection::ChildToolProjection::Rebuilt,
                            messaging_grant,
                            |name| bridge.tool_kind(name),
                        )
                    } else {
                        tools
                    }
                };
            if structured_output_tool && let Some(schema) = json_schema.clone() {
                effective_tools.push(ToolSpec {
                    name: STRUCTURED_OUTPUT_TOOL.to_string(),
                    description: Some(
                        "Return your final answer as JSON matching the required schema. \
                         Call this exactly once, at the end."
                            .to_string(),
                    ),
                    parameters: schema,
                });
            }
            self.persist_tool_definitions_artifact(&effective_tools)
                .await;
            let build_req_start = std::time::Instant::now();
            let build_request_span =
                distill_telemetry::region::Region::from_span(tracing::info_span!(
                    "turn.build_request",
                    build_request_ms = tracing::field::Empty,
                    item_count = tracing::field::Empty,
                ));
            let request = self
                .chat_state_handle
                .build_request(
                    effective_tools,
                    memory_reminder,
                    self.memory.is_enabled(),
                    trace_gcs_config
                        .clone()
                        .map(|cfg| -> Box<dyn crate::sampling::TraceContext> {
                            Box::new(crate::sampling::ConversationRequestTrace {
                                gcs_config: cfg,
                                artifact_tracker: artifact_tracker.cloned(),
                            })
                        }),
                    self.session_info.id.to_string(),
                    req_id.to_owned(),
                )
                .await
                .expect("chat state actor should be alive");
            build_request_span.span().record(
                "build_request_ms",
                build_req_start.elapsed().as_millis() as i64,
            );
            build_request_span
                .span()
                .record("item_count", request.items.len() as i64);
            build_request_span.close();
            distill_telemetry::unified_log::debug(
                "shell.turn.build_request_done",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "build_request_ms": build_req_start.elapsed().as_millis() as u64,
                    "loop_index": loop_index,
                })),
            );
            let mut request = request;
            request.x_grok_session_id = Some(self.session_info.id.to_string());
            request.x_grok_turn_idx =
                Some(self.chat_state_handle.get_prompt_index().await.to_string());
            request.x_grok_agent_id = Some(distill_telemetry::id::agent_id());
            request.x_grok_transient_retry =
                (transient_retry_attempts > 0).then(|| transient_retry_attempts.to_string());
            if request.x_grok_deployment_id.is_none() {
                request.x_grok_deployment_id = crate::managed_config::resolve_deployment_id(
                    crate::managed_config::resolve_deployment_key().as_deref(),
                );
            }
            if structured_output_native {
                request.json_schema = json_schema.clone();
            }
            request.hosted_tools = self.hosted_tools_for_turn();
            request.max_output_tokens = self
                .tool_context
                .clamp_task_model_request(request.max_output_tokens)
                .map_err(|message| {
                    crate::sampling::error::local_error("max_output_tokens_clamp_failed", message)
                })?;
            if salvage.enabled() {
                request.length_policy = distill_sampling_types::LengthPolicy::CompletePartial;
            }
            self.emit_event(crate::session::events::Event::PhaseChanged {
                phase: crate::session::events::Phase::WaitingForModel,
            });
            self.observability_bridge
                .emit(
                    distill_tool_protocol::session_event::SessionEvent::PhaseChanged {
                        phase: distill_tool_protocol::session_event::SessionPhase::Sampling,
                    },
                )
                .await;
            distill_telemetry::unified_log::info(
                "shell.turn.inference_start",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "loop_index": loop_index,
                    "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
                    // Nonzero means this submission is a transient resubmit
                    "transient_retry_attempts": transient_retry_attempts,
                })),
            );
            let model_timer = std::time::Instant::now();
            let model_sampler_outcome = self
                .run_turn_via_sampler(
                    request.clone(),
                    &mut rate_limit_waits,
                    TransientRetryState {
                        step_attempts: transient_retry_attempts,
                        prompt_attempts: self.transient_retries_prompt_total.get(),
                        episode_start: self.transient_episode_start.get(),
                        enabled: transient_retry_enabled,
                    },
                    salvage.awaiting_continuation(),
                    turn_parked,
                )
                .await;
            let (response, latency, usage_context) = match model_sampler_outcome {
                Ok(SamplerTurnOutcome::Response(r, latency, usage_context)) => {
                    salvage.response_arrived();
                    (r, latency, usage_context)
                }
                Err(error) => {
                    if salvage.awaiting_continuation()
                        && crate::sampling::error::is_max_tokens_turn_error(&error)
                    {
                        salvage.response_arrived();
                        distill_telemetry::unified_log::warn(
                            "shell.turn.length_empty_continuation",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "continue_attempts": salvage.continues(),
                                "continue_budget": salvage.budget(),
                                "cause": error
                                    .data
                                    .as_ref()
                                    .and_then(|d| {
                                        d.get(crate::sampling::error::SALVAGE_CAUSE_KEY)
                                    })
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(crate::sampling::error::SALVAGE_CAUSE_EMPTY),
                            })),
                        );
                        if self.drain_interjections_at_safe_point().await {
                            salvage.round_boundary();
                            tracing::info!(
                                "Drained interjection(s) after an empty continuation; continuing"
                            );
                            continue;
                        }
                        self.chat_state_handle.pop_stranded_continue_reminder();
                        let structured_output = match structured_output_validator.as_ref() {
                            Some(validator) => self
                                .chat_state_handle
                                .get_trailing_assistant_report()
                                .await
                                .map(|text| validate_structured_output(validator, &text)),
                            None => None,
                        };
                        self.finalize_turn_bookkeeping(
                            req_id,
                            std::mem::take(&mut turn_span_totals),
                            turn_sampling,
                        )
                        .await;
                        return Ok(TurnOutcome::Completed {
                            tools_called: turn_tools_called,
                            structured_output,
                            stop: CompletedStop::MaxTokens,
                        });
                    }
                    salvage.response_arrived();
                    salvage.step_boundary();
                    self.tool_context.fail_task_output_usage_closed();
                    return Err(error);
                }
                Ok(SamplerTurnOutcome::RetryTransient { kind, status_code }) => {
                    if matches!(kind, distill_sampler::SamplingErrorKind::Api) {
                        auth_retry_schedule.reset_incident_keeping_park();
                    }
                    let delay = distill_sampler::jitter_backoff(transient_backoff_delay(
                        transient_retry_attempts,
                    ));
                    transient_retry_attempts += 1;
                    let prompt_total = self.transient_retries_prompt_total.get() + 1;
                    self.transient_retries_prompt_total.set(prompt_total);
                    if self.transient_episode_start.get().is_none() {
                        self.transient_episode_start
                            .set(Some(tokio::time::Instant::now()));
                    }
                    let display_max =
                        transient_display_ceiling(transient_retry_attempts, prompt_total);
                    distill_telemetry::unified_log::warn(
                        "shell.turn.transient_retry_backoff",
                        Some(self.session_info.id.0.as_ref()),
                        Some(serde_json::json!({
                            "loop_index": loop_index,
                            "kind": kind.as_ref(),
                            "status_code": status_code,
                            "attempt": transient_retry_attempts,
                            "max_retries": display_max,
                            "delay_ms": delay.as_millis() as u64,
                        })),
                    );
                    let cause = match kind {
                        distill_sampler::SamplingErrorKind::IdleTimeout => "Response stalled",
                        distill_sampler::SamplingErrorKind::Http => "Connection problem",
                        _ => "Server error",
                    };
                    self.send_xai_notification(XaiSessionUpdate::RetryState(
                        crate::extensions::notification::RetryState::Retrying {
                            attempt: transient_retry_attempts,
                            max_retries: display_max,
                            reason: format!("{cause}; retrying request"),
                            error_type: Some(kind.as_ref().to_string()),
                        },
                    ))
                    .await;
                    sleep(delay).await;
                    turn_phases.record_sampling_retries(1);
                    continue;
                }
                Ok(SamplerTurnOutcome::CompactAndResubmit) => {
                    auth_retry_schedule.reset_on_success();
                    transient_retry_attempts = 0;
                    turn_phases.record_sampling_retries(1);
                    continue;
                }
                Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { credential, store }) => {
                    if auth_retry_schedule.reset_if_incident_spans_suspend() {
                        tracing::info!("auth 401 retry: incident spanned a suspend; budget reset");
                        distill_telemetry::unified_log::info(
                            "shell.turn.auth_retry_reset_after_suspend",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({ "loop_index": loop_index })),
                        );
                    }
                    match auth_retry_schedule.on_recovered_401(credential) {
                        AuthRetryDecision::UnchargedResubmit { resubmit, delay } => {
                            tracing::warn!(
                                resubmit,
                                delay_ms = delay.as_millis() as u64,
                                "auth 401 retry: no credential was sent; resubmitting uncharged"
                            );
                            distill_telemetry::unified_log::warn(
                                "shell.turn.auth_resubmit_uncharged",
                                Some(self.session_info.id.0.as_ref()),
                                Some(serde_json::json!({
                                    "loop_index": loop_index,
                                    "resubmit": resubmit,
                                    "max_resubmits": AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS,
                                    "delay_ms": delay.as_millis() as u64,
                                })),
                            );
                            self.send_xai_notification(XaiSessionUpdate::RetryState(
                                crate::extensions::notification::RetryState::Retrying {
                                    attempt: resubmit,
                                    max_retries: AuthRetrySchedule::MAX_UNCHARGED_RESUBMITS,
                                    reason: "Re-authenticating after 401 (request carried no \
                                             credential); retrying request"
                                        .to_string(),
                                    error_type: None,
                                },
                            ))
                            .await;
                            pace_uncharged_resubmit(store, self.auth_manager.as_deref(), delay)
                                .await;
                            turn_phases.record_sampling_retries(1);
                            continue;
                        }
                        AuthRetryDecision::Backoff { attempt, delay } => {
                            let delay_ms = delay.as_millis() as u64;
                            tracing::warn!(
                                attempt,
                                delay_ms,
                                "auth 401 retry: backing off before resubmit"
                            );
                            distill_telemetry::unified_log::warn(
                                "shell.turn.auth_retry_backoff",
                                Some(self.session_info.id.0.as_ref()),
                                Some(serde_json::json!({
                                    "loop_index": loop_index,
                                    "attempt": attempt,
                                    "max_retries": AuthRetrySchedule::MAX_RETRIES,
                                    "delay_ms": delay_ms,
                                })),
                            );
                            self.send_xai_notification(XaiSessionUpdate::RetryState(
                                crate::extensions::notification::RetryState::Retrying {
                                    attempt,
                                    max_retries: AuthRetrySchedule::MAX_RETRIES,
                                    reason: "Re-authenticated after 401; retrying request"
                                        .to_string(),
                                    error_type: None,
                                },
                            ))
                            .await;
                            sleep(delay).await;
                            turn_phases.record_sampling_retries(1);
                            continue;
                        }
                        decision @ (AuthRetryDecision::Exhausted
                        | AuthRetryDecision::RunawayGuard { .. }) => {
                            let (awake, wall, suspended) = conv_turn_clock.elapsed_split();
                            let duration_note = if suspended >= std::time::Duration::from_secs(1) {
                                format!(
                                    " Turn ran {} wall-clock, {} of it suspended.",
                                    human_duration(wall),
                                    human_duration(suspended)
                                )
                            } else {
                                format!(" Turn ran {} wall-clock.", human_duration(wall))
                            };
                            let (rejections, authenticated) = auth_retry_schedule.incident_counts();
                            let uncharged = auth_retry_schedule.uncharged_rejections();
                            let msg = match decision {
                                AuthRetryDecision::RunawayGuard { rejections } => {
                                    format!(
                                        "{rejections} requests were rejected (401) before a \
                                     credential could be sent, with no successful response \
                                     in between; stopping as a runaway guard.{duration_note}"
                                    )
                                }
                                _ if authenticated == rejections => {
                                    format!(
                                        "Auth recovery succeeded but {rejections} authenticated \
                                     inference requests were still rejected (401); giving up \
                                     after {} retries.{duration_note}",
                                        AuthRetrySchedule::MAX_RETRIES
                                    )
                                }
                                _ => {
                                    format!(
                                        "Auth retry budget exhausted after {rejections} \
                                     post-recovery 401s ({authenticated} provably carried a \
                                     credential).{duration_note}"
                                    )
                                }
                            };
                            tracing::error!(msg);
                            distill_telemetry::unified_log::error(
                                "shell.turn.auth_retry_exhausted",
                                Some(self.session_info.id.0.as_ref()),
                                Some(serde_json::json!({
                                    "loop_index": loop_index,
                                    "decision": match decision {
                                        AuthRetryDecision::RunawayGuard { .. } => "runaway_guard",
                                        _ => "exhausted",
                                    },
                                    "rejections": rejections,
                                    "authenticated": authenticated,
                                    "uncharged": uncharged,
                                    "wall_secs": wall.as_secs(),
                                    "awake_secs": awake.as_secs(),
                                    "suspended_secs": suspended.as_secs(),
                                })),
                            );
                            return Err(self.fail_turn_auth_budget_exhausted(msg).await);
                        }
                    }
                }
            };
            auth_retry_schedule.reset_on_success();
            transient_retry_attempts = 0;
            self.transient_episode_start.set(None);
            let model_elapsed_ms = model_timer.elapsed().as_millis() as u64;
            let usage = response.usage.as_ref();
            let prompt_tokens = usage.map(|u| u.prompt_tokens);
            let cached_prompt_tokens = usage.map(|u| u.cached_prompt_tokens);
            let completion_tokens = usage.map(|u| u.completion_tokens);
            let reasoning_tokens = usage.map(|u| u.reasoning_tokens);
            let (jev_session_id, jev_turn_id, jev_round_id) = crate::jev::telemetry_context();
            let execution = self.jev_ledger.borrow().last_execution.clone();
            tracing::info!(target: "jev.decision", event_kind = "llm_usage",
                session_id = jev_session_id, turn_id = jev_turn_id, round_id = jev_round_id,
                model = execution.as_ref().map(|(model, _)| model.as_str()).unwrap_or("unknown"),
                effort = execution.as_ref().and_then(|(_, effort)| *effort).map(|effort| effort.as_ref().to_owned()),
                loop_index, model_elapsed_ms, attempts = latency.attempts,
                prompt_tokens, cached_prompt_tokens,
                completion_tokens, reasoning_tokens,
                cost_usd_ticks = response.cost_usd_ticks, usage_reported = usage.is_some(),
                "main model usage; absent usage is unknown, not zero");
            let ttft_ms = latency.time_to_first_token_ms;
            let tokens_per_sec = match completion_tokens {
                Some(ct) if ct > 0 => {
                    let decode_ms = match ttft_ms {
                        Some(ttft) if model_elapsed_ms > ttft => model_elapsed_ms - ttft,
                        _ => model_elapsed_ms,
                    };
                    (decode_ms > 0).then(|| {
                        let tps = f64::from(ct) * 1000.0 / decode_ms as f64;
                        (tps * 10.0).round() / 10.0
                    })
                }
                _ => None,
            };
            distill_telemetry::unified_log::info(
                "shell.turn.inference_done",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "loop_index": loop_index,
                    "model_elapsed_ms": model_elapsed_ms,
                    "elapsed_since_turn_start_ms": conv_turn_start.elapsed().as_millis() as u64,
                    "ttft_ms": ttft_ms,
                    "itl_p50_ms": latency.itl_p50_ms,
                    "attempts": latency.attempts,
                    "prompt_tokens": prompt_tokens,
                    "cached_prompt_tokens": cached_prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "reasoning_tokens": reasoning_tokens,
                    "tokens_per_sec": tokens_per_sec,
                })),
            );
            if let Some(usage) = response.usage.as_ref() {
                self.chat_state_handle
                    .record_token_usage(u64::from(usage.total_tokens));
                self.add_round_usage_for_turn_report(
                    u64::from(usage.prompt_tokens),
                    u64::from(usage.completion_tokens),
                );
                self.send_available_commands_update(AdvertiseTrigger::UsageMeta)
                    .await;
            }
            turn_span_totals.record(&tracing::Span::current(), &response);
            let _ = self.compaction.auto_compact_suppressed.compare_exchange(
                crate::session::compaction_config::SUPPRESS_UNTIL_SUCCESS,
                crate::session::compaction_config::SUPPRESS_NONE,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            );
            self.clear_auth_compact_suppression();
            let model_duration_ms = model_timer.elapsed().as_millis() as u64;
            {
                let model_id = self.current_model_id().await;
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::ModelResponseReceived {
                        model_id,
                        duration_ms: model_duration_ms,
                        stop_reason: response
                            .stop_reason
                            .as_ref()
                            .map(|r| format!("{r:?}").to_ascii_lowercase()),
                        prompt_tokens: response.usage.as_ref().map(|u| u.prompt_tokens),
                        completion_tokens: response.usage.as_ref().map(|u| u.completion_tokens),
                        reasoning_tokens: response.usage.as_ref().map(|u| u.reasoning_tokens),
                        cached_prompt_tokens: response
                            .usage
                            .as_ref()
                            .map(|u| u.cached_prompt_tokens),
                        cache_creation_tokens: response
                            .usage
                            .as_ref()
                            .map(|u| u.cache_creation_prompt_tokens),
                        context_tokens: response.usage.as_ref().map(|u| u.total_tokens),
                        cost_usd_ticks: response.cost_usd_ticks,
                    },
                );
            }
            self.record_response_token_usage_with_context(
                &response,
                Some(model_duration_ms),
                usage_context,
            );
            let response_completed = self.response_completed_update(&response);
            if let Some(mut pt) = prompt_timing.take() {
                pt.record_stream_latency(latency.time_to_last_byte_ms);
                pt.record_model_result(
                    latency.attempts,
                    response.usage.as_ref().map(|u| u.completion_tokens),
                );
                let mcp_count = self.mcp_state.lock().await.configs.len() as u32;
                let mcp_tools = self
                    .agent
                    .borrow()
                    .tool_bridge()
                    .tool_definitions()
                    .await
                    .iter()
                    .filter(|t| t.function.name.contains("__"))
                    .count() as u32;
                let turn_index = self
                    .chat_state_handle
                    .get_prompt_index()
                    .await
                    .saturating_sub(1) as u32;
                if turn_index == 0
                    && let Some(repo_status_wait_ms) = self.repo_status_prefetch.take_wait_ms()
                {
                    pt.record_repo_status_wait(repo_status_wait_ms);
                }
                turn_phases.arm_latency(pt.build(
                    model_duration_ms,
                    turn_index,
                    mcp_count,
                    mcp_tools,
                    self.mcp_strategy.get(),
                    self.current_model_id().await,
                ));
            }
            let mut tool_calls = response.tool_calls().to_vec();
            let over_cap = self.media_gen_over_cap(&tool_calls);
            if distill_tools::media_gen_limits::should_resample_egregious(
                &over_cap,
                media_gen_resamples,
                MAX_MEDIA_GEN_OVER_CAP_RESAMPLES,
            ) {
                media_gen_resamples += 1;
                let egregious: Vec<_> = over_cap.into_iter().filter(|o| o.is_egregious()).collect();
                let reminder = distill_tools::media_gen_limits::resample_reminder(&egregious);
                tracing::warn!(
                    session_id = %self.session_info.id,
                    resample = media_gen_resamples,
                    "media_gen 2x over-cap — discarding generation and resampling"
                );
                distill_telemetry::unified_log::info(
                    "shell.media_gen.batch_resampled",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "over": egregious.iter().map(|o| serde_json::json!({
                            "tool_name": o.name,
                            "total": o.total,
                            "max": o.max,
                        })).collect::<Vec<_>>(),
                        "attempt": media_gen_resamples,
                        "max_retries": MAX_MEDIA_GEN_OVER_CAP_RESAMPLES,
                    })),
                );
                self.send_xai_notification(XaiSessionUpdate::RetryState(
                    crate::extensions::notification::RetryState::Retrying {
                        attempt: media_gen_resamples,
                        max_retries: MAX_MEDIA_GEN_OVER_CAP_RESAMPLES,
                        reason: "Too many parallel media-gen calls; retrying".to_string(),
                        error_type: None,
                    },
                ))
                .await;
                self.push_system_reminder(&reminder);
                self.turn_phases.discard_uncommitted_first_token();
                self.turn_phases.discard_uncommitted_first_meaningful();
                continue;
            }
            metrics_drop_guard.record_model_response(tool_calls.len());
            if !tool_calls.is_empty() {
                self.record_turn_first_token(None);
            }
            self.turn_phases.commit_first_token();
            self.turn_phases.commit_first_meaningful();
            if let Some(fp) = response
                .assistant()
                .and_then(|a| a.model_fingerprint.clone())
            {
                self.signals_handle().record_model_fingerprint(fp);
            }
            let fallback_text = response.fallback_text();
            let stop_reason = response.stop_reason;
            let response_is_empty = response.is_empty();
            let turn_refused =
                stop_reason == Some(distill_sampling_types::StopReason::ContentFilter);
            let refusal_explanation = response.stop_message.clone();
            let final_answer_text = json_schema.is_some().then(|| response.assistant_text());
            match length_salvage_streak.on_sample(
                stop_reason == Some(distill_sampling_types::StopReason::Length)
                    && !tool_calls.is_empty(),
            ) {
                LengthSalvageAction::Exhausted => {
                    tracing::error!(
                        session_id = %self.session_info.id,
                        max = MAX_OUTPUT_TOKEN_LIMIT_RETRIES,
                        "consecutive Length-salvaged tool-call samples hit the cap — failing the turn"
                    );
                    self.tool_context.fail_task_output_usage_closed();
                    return Err(self.fail_turn_length_salvage_exhausted().await);
                }
                LengthSalvageAction::Proceed { inject_reminder } => {
                    if inject_reminder {
                        self.push_system_reminder(OUTPUT_TOKEN_LIMIT_REMINDER);
                    }
                }
                LengthSalvageAction::NotSalvage => {}
            }
            let usage_reported = response.usage.is_some();
            let response_item_count = response.items.len() as i64;
            let record_response_span = distill_telemetry::region::Region::from_span(
                tracing::info_span!("turn.record_response", item_count = response_item_count),
            );
            self.record_response_items(response.items, usage_reported)
                .await;
            record_response_span.close();
            if let Some(text) = fallback_text {
                tracing::warn!(
                    text_len = text.len(),
                    "emitting fallback AgentMessageChunk — no text chunks were streamed"
                );
                self.record_turn_first_token(None);
                self.record_turn_first_meaningful_output(None);
                self.send_update(
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    )),
                    None,
                )
                .await;
            }
            if turn_refused && response_is_empty {
                let mut notice = "The model provider refused to generate a response \
                     for this turn (content filter)."
                    .to_string();
                if let Some(explanation) = refusal_explanation.as_deref() {
                    notice.push_str("\n\nProvider explanation: ");
                    notice.push_str(explanation);
                }
                tracing::warn!(
                    has_explanation = refusal_explanation.is_some(),
                    "model response was a provider refusal — emitting notice chunk"
                );
                self.send_update(
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(notice)),
                    )),
                    None,
                )
                .await;
            }
            self.send_buffered_xai_update(response_completed).await;
            let schema_complete_at_cap = if stop_reason
                == Some(distill_sampling_types::StopReason::Length)
                && tool_calls.is_empty()
                && salvage.enabled()
                && let Some(validator) = structured_output_validator.as_ref()
            {
                let report = self.chat_state_handle.get_trailing_assistant_report().await;
                report.and_then(|text| validate_structured_output(validator, &text).ok())
            } else {
                None
            };
            if let Some(value) = &schema_complete_at_cap {
                distill_telemetry::unified_log::info(
                    "shell.turn.length_schema_complete_at_cap",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "continue_attempts": salvage.continues(),
                        "continue_budget": salvage.budget(),
                        "document_len": value.to_string().len(),
                    })),
                );
            }
            if stop_reason == Some(distill_sampling_types::StopReason::Length)
                && tool_calls.is_empty()
                && schema_complete_at_cap.is_none()
            {
                match salvage.on_length_stop() {
                    super::length_salvage::SalvageStep::Continue { inject_reminder } => {
                        if inject_reminder {
                            let tag = self.reminder_wrapper_tag();
                            self.chat_state_handle.push_user_message(
                                ConversationItem::length_continue_reminder(format!(
                                    "<{tag}>{}</{tag}>",
                                    super::length_salvage::LENGTH_CONTINUE_REMINDER_BODY
                                )),
                            );
                        }
                        tracing::warn!(
                            session_id = %self.session_info.id,
                            retry = salvage.continues(),
                            max = salvage.budget(),
                            "Output token limit exceeded — injecting reminder and retrying"
                        );
                        distill_telemetry::unified_log::warn(
                            "shell.turn.length_truncation_continue",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "continue_attempts": salvage.continues(),
                                "continue_budget": salvage.budget(),
                            })),
                        );
                        continue;
                    }
                    super::length_salvage::SalvageStep::Exhaust => {
                        tracing::error!(
                            session_id = %self.session_info.id,
                            retries = salvage.continues(),
                            "Output token limit retries exhausted, completing the turn truncated"
                        );
                        distill_telemetry::unified_log::warn(
                            "shell.turn.length_truncation_exhausted",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!({
                                "continue_attempts": salvage.continues(),
                                "continue_budget": salvage.budget(),
                            })),
                        );
                    }
                    super::length_salvage::SalvageStep::None => {}
                }
            }
            if tool_calls.is_empty() {
                if !schema_ok
                    && !turn_refused
                    && !salvage.is_truncated()
                    && let Some(gate_cfg) = self.todo_gate_policy()
                {
                    let collected = self.collect_todo_gate_input(req_id).await;
                    let input = collected.as_input();
                    if let TodoGateDecision::Nudge { reminder, reason } = evaluate_todo_gate(&input)
                    {
                        if todo_gate_fires < gate_cfg.max_fires_per_prompt {
                            todo_gate_fires += 1;
                            tracing::info!(
                                prompt_id = %req_id,
                                pending = ?input.pending,
                                unbacked_in_progress = ?input.in_progress_unbacked,
                                backed_in_progress = ?input.in_progress_backed,
                                backing_task_count = input.backing_task_count,
                                todo_gate_fires,
                                reason = reason.as_str(),
                                "turn-end TodoGate: nudging model to advance remaining todos"
                            );
                            self.events
                                .emit(crate::session::events::Event::TodoGateFired {
                                    fires: todo_gate_fires,
                                    pending: input.pending.len(),
                                    in_progress: input.in_progress_unbacked.len()
                                        + input.in_progress_backed.len(),
                                    reason: reason.as_str(),
                                });
                            let rendered = self
                                .tool_bridge_handle()
                                .render_prompt(&reminder, &serde_json::json!({}))
                                .await
                                .unwrap_or(reminder);
                            self.push_system_reminder(&rendered);
                            salvage.step_boundary();
                            continue;
                        }
                        let cap = gate_cfg.max_fires_per_prompt;
                        tracing::warn!(
                            prompt_id = %req_id,
                            todo_gate_cap = cap,
                            "turn-end TodoGate: exhausted retries, falling through"
                        );
                        self.events
                            .emit(crate::session::events::Event::TodoGateExhausted {
                                pending: input.pending.len(),
                            });
                        self.push_system_reminder(&format!(
                            "The agent attempted to end this turn {cap} times \
                             with todos still pending or in_progress. Falling through \
                             to user. If you want autonomous progress, prompt the agent \
                             to continue explicitly, or clean up the todo list."
                        ));
                    }
                }
                if self.drain_interjections_at_safe_point().await {
                    salvage.round_boundary();
                    tracing::info!("Drained interjection(s) before turn completion; continuing");
                    continue;
                }
                self.finalize_turn_bookkeeping(
                    req_id,
                    std::mem::take(&mut turn_span_totals),
                    turn_sampling,
                )
                .await;
                if self.drain_admitted_messages_at_safe_point().await {
                    salvage.round_boundary();
                    tracing::info!(
                        "Drained late interjection(s) or parent Steer(s) during turn-end bookkeeping; continuing"
                    );
                    continue;
                }
                let structured_output = match (
                    structured_output_validator.as_ref(),
                    final_answer_text.as_ref(),
                ) {
                    _ if schema_complete_at_cap.is_some() => schema_complete_at_cap.map(Ok),
                    (Some(validator), Some(text)) => {
                        let joined = if salvage.any_continues() {
                            self.chat_state_handle.get_trailing_assistant_report().await
                        } else {
                            None
                        };
                        Some(validate_structured_output(
                            validator,
                            joined.as_deref().unwrap_or(text),
                        ))
                    }
                    _ => None,
                };
                return Ok(TurnOutcome::Completed {
                    tools_called: turn_tools_called,
                    structured_output,
                    stop: if turn_refused {
                        CompletedStop::Refusal(refusal_explanation.clone().unwrap_or_default())
                    } else if salvage.is_truncated() {
                        CompletedStop::MaxTokens
                    } else {
                        CompletedStop::EndTurn
                    },
                });
            }
            salvage.step_boundary();
            if structured_output_tool && let Some(validator) = structured_output_validator.as_ref()
            {
                match self
                    .handle_structured_output_tool_call(
                        &mut tool_calls,
                        validator,
                        &mut structured_output_retries,
                    )
                    .await
                {
                    StructuredOutputStep::Complete(validated) => {
                        turn_tools_called.push(STRUCTURED_OUTPUT_TOOL.to_string());
                        self.finalize_turn_bookkeeping(
                            req_id,
                            std::mem::take(&mut turn_span_totals),
                            turn_sampling,
                        )
                        .await;
                        return Ok(TurnOutcome::Completed {
                            tools_called: turn_tools_called,
                            structured_output: Some(validated),
                            stop: if salvage.is_truncated() {
                                CompletedStop::MaxTokens
                            } else {
                                CompletedStop::EndTurn
                            },
                        });
                    }
                    StructuredOutputStep::Retry => continue,
                    StructuredOutputStep::Proceed => {}
                }
            }
            for tc in &tool_calls {
                if let Some((server, tool)) =
                    crate::session::mcp_servers::parse_mcp_tool_name(&tc.name)
                {
                    let span = tracing::Span::current();
                    span.record("mcp_server.name", server.as_str());
                    span.record("mcp_tool.name", tool.as_str());
                }
                turn_tools_called.push(tc.name.clone());
            }
            let step_signature = step_signature(&tool_calls);
            let step_tool_name = tool_calls
                .iter()
                .map(|tc| tc.name.clone())
                .min()
                .unwrap_or_default();
            let tool_bridge = self.tool_bridge_handle();
            let step_tool_kinds = tool_calls
                .iter()
                .map(|tc| tool_bridge.tool_kind(&tc.name))
                .collect::<Vec<_>>();
            let step_problematic = step_is_problematically_repeating(&step_tool_kinds);
            let is_true_noop = self.is_run_true_step(&tool_calls).await;
            identical_tool_calls.observe(
                &step_signature,
                &step_tool_name,
                step_problematic,
                is_true_noop,
            );
            if is_true_noop {
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::ShellTrueNoop {
                        tool_name: step_tool_name.clone(),
                    },
                );
            }
            let tool_call_responses: Vec<ToolCallResponse> = tool_calls
                .into_iter()
                .map(|tc| ToolCallResponse {
                    id: tc.id.as_ref().to_owned(),
                    kind: "function".to_string(),
                    function: crate::sampling::types::ToolCallFunction {
                        name: tc.name,
                        arguments: tc.arguments.as_ref().to_owned(),
                    },
                })
                .collect();
            self.emit_event(crate::session::events::Event::PhaseChanged {
                phase: crate::session::events::Phase::ToolExecution,
            });
            self.observability_bridge
                .emit(
                    distill_tool_protocol::session_event::SessionEvent::PhaseChanged {
                        phase: distill_tool_protocol::session_event::SessionPhase::ToolExecution,
                    },
                )
                .await;
            let execute_tool_calls_result = {
                let _tool_phase = turn_phases.begin_tool_blocking();
                self.execute_tool_calls(tool_call_responses).await
            };
            match execute_tool_calls_result {
                Ok(ToolLoop::PermissionReject { tool_name, reason }) => {
                    return Ok(TurnOutcome::Cancelled {
                        category: Some(
                            crate::session::events::CancellationCategory::PermissionRejected,
                        ),
                        context: Some(crate::session::commands::CancellationContext {
                            tool_name: Some(tool_name),
                            reason: Some(reason),
                            ..Default::default()
                        }),
                    });
                }
                Ok(ToolLoop::HookDenied { .. }) => {}
                Ok(ToolLoop::Cancelled) => {
                    return Ok(TurnOutcome::Cancelled {
                        category: Some(
                            crate::session::events::CancellationCategory::PermissionCancelled,
                        ),
                        context: None,
                    });
                }
                Ok(ToolLoop::FollowupMessage(followup_message)) => {
                    self.add_followup_message_as_user_turn(&followup_message)
                        .await;
                    continue;
                }
                _ => {}
            }
            let next_turn = tool_turn_count + 1;
            if let Some(limit) = self.max_turns
                && next_turn > limit
            {
                tracing::info!(
                    session_id = %self.session_info.id,
                    tool_turn_count,
                    limit,
                    "max-turns limit reached, stopping"
                );
                return Ok(TurnOutcome::MaxTurnsReached { limit });
            }
            tool_turn_count = next_turn;
            if self.tool_context.task_output_token_budget.is_none()
                && let Some(trigger_info) = self.check_preflight_overflow().await
            {
                if let Err(e) = self.run_compact_only(trigger_info, false).await {
                    tracing::error!(error = %e, "Preflight overflow compaction failed");
                    if Self::is_auth_compact_error(&e) {
                        return Err(self.surface_compact_auth_failure(e).await);
                    }
                }
                continue;
            }
        }
    }
}
/// Discard an egregious (2x cap) media-gen generation and re-sample this many times; later over-caps in the same turn use first-K.
const MAX_MEDIA_GEN_OVER_CAP_RESAMPLES: u32 = 1;
/// Tool kinds whose identical repeats are almost never productive, so they get tighter thresholds than everything else.
/// A production turn repeated one `ToolKind::Plan` call (`todo_write`) with byte-identical arguments 12 times (224 in the turn).
/// Names are client-renameable and vary by toolset (`read_file`, `hashline_read`, `Read`; `todo_write`, `todowrite`); the registered kind does not.
fn is_problematically_repeating_kind(kind: Option<ToolKind>) -> bool {
    matches!(kind, Some(ToolKind::Read | ToolKind::Plan))
}
/// Whether a whole sampling step belongs in the tight tier: every call in it must be a problematically repeating kind.
/// Order-insensitive, like [`step_signature`]: a reordered step is the same step, so it must not flip tiers.
/// Requiring *every* call, rather than any, keeps a mixed step in the looser tier.
fn step_is_problematically_repeating(kinds: &[Option<ToolKind>]) -> bool {
    !kinds.is_empty()
        && kinds
            .iter()
            .all(|kind| is_problematically_repeating_kind(*kind))
}
pub(super) const NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS: u32 = 4;
pub(super) const NUDGE_AFTER_IDENTICAL_TOOL_CALLS: u32 = 8;
pub(super) const MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS: u32 = 8;
pub(super) const MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS: u32 = 12;
const MAX_CONSECUTIVE_TRUE_NOOPS: u32 = 4;
const _: () = assert!(
    NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS < MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS
);
const _: () = assert!(NUDGE_AFTER_IDENTICAL_TOOL_CALLS < MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS);
const ACTION_STATIONARITY_NUDGE_TEMPLATE: &str = "You have called the same tool \
     (`${{ tool_name }}`) with the exact same arguments ${{ run_len }} times in a row — \
     you appear to be stuck in a polling loop. Stop repeating this call. If you are \
     waiting on a long-running job or command, use a background task${%- if tools.by_kind.monitor %} \
     or the `${{ tools.by_kind.monitor }}` tool${%- endif %}, or run a single `sleep` and \
     then check once — do not poll in a tight loop. If you cannot make progress, stop and \
     tell the user what you are waiting for. This turn will be halted automatically if the \
     identical call keeps repeating.";
fn hash_step_signature(signature: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signature.hash(&mut hasher);
    hasher.finish()
}
fn command_is_true(cmd: &str) -> bool {
    cmd.trim().eq_ignore_ascii_case("true")
}
/// Recursively sort object keys so the same arguments compare equal however the model happened to serialize them.
/// `{"path":"x","limit":10}` and `{"limit":10,"path":"x"}` are the same call.
/// `serde_json` is built with `preserve_order` here, so a `Map` keeps insertion order and re-inserting in sorted order is what makes this canonical.
fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, canonicalize_json(v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}
/// Signature of one sampling step: every tool call it emitted, each canonicalized, then sorted.
/// Re-emitting the same set of parallel calls in a different order thus does not read as progress.
/// Arguments that do not parse as JSON fall back to their trimmed raw text, which is the pre-canonicalization behaviour.
fn step_signature(tool_calls: &[distill_sampling_types::conversation::ToolCall]) -> String {
    let mut parts: Vec<String> = tool_calls
        .iter()
        .map(|tc| {
            let args = serde_json::from_str::<serde_json::Value>(tc.arguments.as_ref())
                .map(|v| canonicalize_json(v).to_string())
                .unwrap_or_else(|_| tc.arguments.trim().to_string());
            format!("{}\u{1f}{}", tc.name, args)
        })
        .collect();
    parts.sort();
    parts.join("\u{1e}")
}
#[derive(Default)]
struct IdenticalToolCallRun {
    last_signature_hash: Option<u64>,
    tool_name: String,
    /// Whether the repeated step is in the tight threshold tier.
    /// Decided by the registered kinds of every call in it (see [`step_is_problematically_repeating`]).
    problematically_repeating_step: bool,
    run_len: u32,
    is_true_noop_run: bool,
    nudged: bool,
}
impl IdenticalToolCallRun {
    fn observe(
        &mut self,
        signature: &str,
        tool_name: &str,
        problematically_repeating_step: bool,
        is_true_noop: bool,
    ) -> u32 {
        let hash = hash_step_signature(if is_true_noop {
            "\0true_noop"
        } else {
            signature
        });
        if self.last_signature_hash == Some(hash) {
            self.run_len += 1;
        } else {
            self.run_len = 1;
            self.last_signature_hash = Some(hash);
            self.is_true_noop_run = is_true_noop;
            self.nudged = false;
        }
        self.tool_name = tool_name.to_string();
        self.problematically_repeating_step = problematically_repeating_step;
        self.run_len
    }
    /// Whether this run gets the tighter nudge and hard-stop thresholds (see [`step_is_problematically_repeating`]).
    fn is_problematically_repeating(&self) -> bool {
        !self.is_true_noop_run && self.problematically_repeating_step
    }
    fn nudge_threshold(&self) -> u32 {
        if self.is_problematically_repeating() {
            NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS
        } else {
            NUDGE_AFTER_IDENTICAL_TOOL_CALLS
        }
    }
    /// Once per identical run at/after the nudge threshold. Call only after results are committed.
    ///
    /// `true` keepalive runs are exempt: they end the turn silently at [`MAX_CONSECUTIVE_TRUE_NOOPS`] rather than being told to stop polling.
    fn take_nudge(&mut self) -> bool {
        let fire = !self.is_true_noop_run && self.run_len >= self.nudge_threshold() && !self.nudged;
        self.nudged |= fire;
        fire
    }
    fn hard_stop_threshold(&self) -> u32 {
        if self.is_true_noop_run {
            MAX_CONSECUTIVE_TRUE_NOOPS
        } else if self.is_problematically_repeating() {
            MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS
        } else {
            MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
        }
    }
}
#[cfg(test)]
mod identical_tool_call_run_tests {
    use super::{
        IdenticalToolCallRun, MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS,
        MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS, MAX_CONSECUTIVE_TRUE_NOOPS,
        NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS, NUDGE_AFTER_IDENTICAL_TOOL_CALLS, ToolKind,
        command_is_true, step_is_problematically_repeating, step_signature,
    };
    #[test]
    fn identical_non_true_resets_and_caps_at_the_hard_limit() {
        let mut run = IdenticalToolCallRun::default();
        assert_eq!(run.observe("a", "a", false, false), 1);
        assert_eq!(run.observe("a", "a", false, false), 2);
        assert_eq!(run.observe("b", "b", false, false), 1);
        let mut last = 0;
        for _ in 0..MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS {
            last = run.observe("same", "same", false, false);
        }
        assert_eq!(last, MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS);
        assert_eq!(
            run.hard_stop_threshold(),
            MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
        );
    }
    #[test]
    fn true_noops_chain_across_args_and_stop_at_4() {
        let mut run = IdenticalToolCallRun::default();
        for i in 1..=4 {
            assert_eq!(run.observe(&format!("sig{i}"), "bash", false, true), i);
        }
        assert!(run.is_true_noop_run);
        assert_eq!(run.hard_stop_threshold(), MAX_CONSECUTIVE_TRUE_NOOPS);
        assert_eq!(run.observe("squeue", "bash", false, false), 1);
        assert!(!run.is_true_noop_run);
    }
    /// Reordering argument keys, or reordering the calls within one step, is the same step.
    /// Otherwise a loop could evade the counter by shuffling either one.
    #[test]
    fn step_signature_ignores_key_order_and_call_order() {
        let call = |name: &str, args: &str| distill_sampling_types::conversation::ToolCall {
            id: "id".into(),
            name: name.to_string(),
            arguments: args.into(),
        };
        assert_eq!(
            step_signature(&[call("read_file", r#"{"path":"a","limit":10}"#)]),
            step_signature(&[call("read_file", r#"{"limit":10,"path":"a"}"#)]),
            "key order must not matter"
        );
        assert_eq!(
            step_signature(&[call("read_file", r#"{"a":{"x":1,"y":2}}"#)]),
            step_signature(&[call("read_file", r#"{"a":{"y":2,"x":1}}"#)]),
            "nested key order must not matter"
        );
        let a = call("read_file", r#"{"path":"a"}"#);
        let b = call("read_file", r#"{"path":"b"}"#);
        assert_eq!(
            step_signature(&[a.clone(), b.clone()]),
            step_signature(&[b, a]),
            "order of parallel calls must not matter"
        );
        assert_ne!(
            step_signature(&[call("read_file", r#"{"path":"a"}"#)]),
            step_signature(&[call("read_file", r#"{"path":"b"}"#)])
        );
        assert_ne!(
            step_signature(&[call("todo_write", r#"{"todos":[1,2]}"#)]),
            step_signature(&[call("todo_write", r#"{"todos":[2,1]}"#)])
        );
        assert_ne!(
            step_signature(&[call("x", "not json a")]),
            step_signature(&[call("x", "not json b")])
        );
    }
    #[test]
    fn command_is_true_trim_and_case() {
        assert!(command_is_true("true"));
        assert!(command_is_true(" TRUE "));
        assert!(!command_is_true("true && echo hi"));
        assert!(!command_is_true("lisa status"));
    }
    #[test]
    fn nudge_latch_fires_once_per_run_after_threshold() {
        let mut run = IdenticalToolCallRun::default();
        for i in 1..NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
            assert_eq!(run.observe("poll", "get_task_output", false, false), i);
            assert!(
                !run.take_nudge(),
                "must not nudge before threshold; run_len={i}"
            );
        }
        assert_eq!(
            run.observe("poll", "get_task_output", false, false),
            NUDGE_AFTER_IDENTICAL_TOOL_CALLS
        );
        assert!(run.take_nudge());
        assert!(!run.take_nudge());
        assert_eq!(
            run.observe("poll", "get_task_output", false, false),
            NUDGE_AFTER_IDENTICAL_TOOL_CALLS + 1
        );
        assert!(!run.take_nudge());
        assert_eq!(run.observe("other", "bash", false, false), 1);
        assert!(!run.nudged);
        assert!(!run.take_nudge());
    }
    /// `ToolKind::Read` / `ToolKind::Plan` (`read_file` / `todo_write`) nudge and stop earlier than everything else.
    /// Everything else includes tools with no registered kind.
    #[test]
    fn problematically_repeating_tools_use_the_tighter_thresholds() {
        for tool in ["read_file", "todo_write"] {
            let mut run = IdenticalToolCallRun::default();
            for i in 1..NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS {
                assert_eq!(run.observe("same", tool, true, false), i);
                assert!(!run.take_nudge(), "{tool} must not nudge at run_len={i}");
            }
            assert_eq!(
                run.observe("same", tool, true, false),
                NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS
            );
            assert!(run.take_nudge(), "{tool} must nudge at its own threshold");
            assert!(run.is_problematically_repeating());
            assert_eq!(
                run.hard_stop_threshold(),
                MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS
            );
        }
        let mut run = IdenticalToolCallRun::default();
        for i in 1..NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
            assert_eq!(run.observe("same", "run_terminal_command", false, false), i);
            assert!(
                !run.take_nudge(),
                "loose tier must not nudge at run_len={i}"
            );
        }
        assert_eq!(
            run.observe("same", "run_terminal_command", false, false),
            NUDGE_AFTER_IDENTICAL_TOOL_CALLS
        );
        assert!(run.take_nudge());
        assert!(!run.is_problematically_repeating());
        assert_eq!(
            run.hard_stop_threshold(),
            MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
        );
    }
    /// A step is in the tight tier only when every call in it is `Read`/`Plan`.
    /// The answer must not depend on the order the model emitted them in.
    #[test]
    fn step_tier_needs_every_call_and_ignores_order() {
        let read = Some(ToolKind::Read);
        let plan = Some(ToolKind::Plan);
        let exec = Some(ToolKind::Execute);
        assert!(step_is_problematically_repeating(&[read]));
        assert!(step_is_problematically_repeating(&[plan]));
        assert!(step_is_problematically_repeating(&[read, plan]));
        assert!(!step_is_problematically_repeating(&[exec]));
        assert!(!step_is_problematically_repeating(&[None]));
        assert!(!step_is_problematically_repeating(&[]));
        assert!(!step_is_problematically_repeating(&[read, exec]));
        assert!(!step_is_problematically_repeating(&[exec, read]));
    }
    /// A `true` keepalive run must end the turn silently at MAX_CONSECUTIVE_TRUE_NOOPS instead of being told to stop polling.
    /// This holds even though it passes the nudge threshold.
    #[test]
    fn true_noop_runs_are_never_nudged() {
        let mut run = IdenticalToolCallRun::default();
        for i in 1..=MAX_CONSECUTIVE_TRUE_NOOPS {
            assert_eq!(run.observe(&format!("sig{i}"), "bash", false, true), i);
            assert!(
                !run.take_nudge(),
                "keepalive run must not nudge; run_len={i}"
            );
        }
        assert_eq!(run.hard_stop_threshold(), MAX_CONSECUTIVE_TRUE_NOOPS);
        let mut last = 0;
        for _ in 0..NUDGE_AFTER_IDENTICAL_TOOL_CALLS {
            last = run.observe("poll", "get_task_output", false, false);
        }
        assert_eq!(last, NUDGE_AFTER_IDENTICAL_TOOL_CALLS);
        assert!(run.take_nudge());
    }
}
#[cfg(test)]
mod user_echo_broadcast_tests {
    use super::{InputOrigin, PromptOrigin, UserEchoMode, user_echo_mode};
    fn origin(prompt_id: &str) -> InputOrigin {
        InputOrigin::new(PromptOrigin::from_prompt_id(prompt_id))
    }
    /// Notification-drain: persisted (rewind/fork count user-chunk runs as turn boundaries) but never broadcast live.
    /// The pager hides it via the `hideFromScrollback` chunk meta.
    #[test]
    fn notification_drain_turn_is_persist_only() {
        assert_eq!(
            user_echo_mode(
                "notifications-019e0000-0000-7000-8000-0000000000aa",
                &origin("notifications-uuid"),
            ),
            UserEchoMode::PersistOnly
        );
    }
    /// Real user prompts, cron (`/loop`) fires, and other turns still broadcast live so multi-client / dashboard viewers stay in sync.
    #[test]
    fn user_and_cron_turns_broadcast_live() {
        assert_eq!(
            user_echo_mode("my-prompt", &origin("my-prompt")),
            UserEchoMode::Broadcast
        );
        assert_eq!(
            user_echo_mode("scheduler-fired-abc", &origin("scheduler-fired-abc")),
            UserEchoMode::Broadcast
        );
        assert_eq!(
            user_echo_mode("task-completed-bg-1", &origin("task-completed-bg-1")),
            UserEchoMode::Broadcast
        );
        assert_eq!(
            user_echo_mode("subagent-completed-xyz", &origin("subagent-completed-xyz")),
            UserEchoMode::Broadcast
        );
    }
    /// Interject-fallback turns are persist-only.
    /// Every pane already rendered the text from the `x.ai/session/interjection` broadcast, so a live echo would duplicate the block.
    #[test]
    fn interject_fallback_turn_is_persist_only() {
        assert_eq!(
            user_echo_mode(
                "interject-fallback-019e24b7",
                &origin("interject-fallback-019e24b7")
            ),
            UserEchoMode::PersistOnly
        );
    }
}
#[cfg(test)]
mod structured_output_validation_tests {
    use super::validate_structured_output;
    fn validator() -> Result<jsonschema::Validator, String> {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
            "required": ["name", "age"],
            "additionalProperties": false,
        });
        jsonschema::validator_for(&schema).map_err(|e| e.to_string())
    }
    #[test]
    fn accepts_conforming_json() {
        let v = validate_structured_output(&validator(), r#"{"name":"alice","age":30}"#).unwrap();
        assert_eq!(
            v.pointer("/name").unwrap_or(&serde_json::Value::Null),
            "alice"
        );
    }
    #[test]
    fn rejects_non_json() {
        let err = validate_structured_output(&validator(), "not json").unwrap_err();
        assert!(err.starts_with("model output was not valid JSON: "));
    }
    #[test]
    fn rejects_schema_violation() {
        let err = validate_structured_output(&validator(), r#"{"name":"alice"}"#).unwrap_err();
        assert!(err.starts_with("output does not match the required schema: "));
    }
    #[test]
    fn surfaces_invalid_schema_error() {
        let bad: Result<jsonschema::Validator, String> = Err("invalid output schema: boom".into());
        let err = validate_structured_output(&bad, r#"{"name":"alice","age":1}"#).unwrap_err();
        assert_eq!(err, "invalid output schema: boom");
    }
}
#[cfg(test)]
mod last_sample_span_tests {
    use super::{TurnSpanTotals, record_failed_sample_on_turn_span};
    use distill_sampling_types::conversation::{
        ConversationItem, ConversationResponse, StopReason, TokenUsage, ToolCall,
    };
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;
    use tracing_subscriber::util::SubscriberInitExt;
    /// Last-write value per field, the view the OTel bridge exports.
    #[derive(Default)]
    struct Fields {
        strs: BTreeMap<String, String>,
        i64s: BTreeMap<String, i64>,
        bools: BTreeMap<String, bool>,
    }
    struct FieldVisitor<'a>(&'a mut Fields);
    impl Visit for FieldVisitor<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0
                .strs
                .insert(field.name().to_string(), value.to_string());
        }
        fn record_i64(&mut self, field: &Field, value: i64) {
            self.0.i64s.insert(field.name().to_string(), value);
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.bools.insert(field.name().to_string(), value);
        }
        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }
    struct RecordCapture {
        fields: Arc<Mutex<Fields>>,
    }
    impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RecordCapture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::Id,
            _ctx: Context<'_, S>,
        ) {
            attrs.record(&mut FieldVisitor(&mut self.fields.lock().unwrap()));
        }
        fn on_record(
            &self,
            _id: &tracing::Id,
            values: &tracing::span::Record<'_>,
            _ctx: Context<'_, S>,
        ) {
            values.record(&mut FieldVisitor(&mut self.fields.lock().unwrap()));
        }
    }
    fn capture() -> (Arc<Mutex<Fields>>, tracing::subscriber::DefaultGuard) {
        let fields = Arc::new(Mutex::new(Fields::default()));
        let subscriber = tracing_subscriber::registry().with(RecordCapture {
            fields: fields.clone(),
        });
        (fields, subscriber.set_default())
    }
    /// Declares the same fields as the real turn span's `#[instrument]`.
    fn turn_span() -> tracing::Span {
        tracing::info_span!(
            "session.process_conversation_turn",
            input_tokens = tracing::field::Empty,
            output_tokens = tracing::field::Empty,
            cache_read_tokens = tracing::field::Empty,
            stop_reason = tracing::field::Empty,
            response.has_tool_call = tracing::field::Empty,
            last_sample.stop_reason = tracing::field::Empty,
            last_sample.has_tool_call = tracing::field::Empty,
            last_sample.output_tokens = tracing::field::Empty,
        )
    }
    fn sample(
        stop_reason: Option<StopReason>,
        with_tool_call: bool,
        completion_tokens: u32,
    ) -> ConversationResponse {
        let mut response = ConversationResponse {
            items: vec![ConversationItem::assistant("")],
            stop_reason,
            usage: Some(TokenUsage {
                prompt_tokens: 100,
                completion_tokens,
                total_tokens: 100 + completion_tokens,
                reasoning_tokens: 0,
                cached_prompt_tokens: 0,
                cache_creation_prompt_tokens: 0,
            }),
            cost_usd_ticks: None,
            message_chunks_emitted: 0,
            doom_loop_signals: Vec::new(),
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        };
        if with_tool_call && let Some(a) = response.assistant_mut() {
            a.tool_calls.push(ToolCall {
                id: "call-1".into(),
                name: "bash".to_string(),
                arguments: "{}".into(),
            });
        }
        response
    }
    /// Two delivered samples: legacy fields aggregate, `last_sample.*` tracks the final one.
    #[test]
    fn last_sample_fields_track_final_sample_while_legacy_fields_aggregate() {
        let (fields, _guard) = capture();
        let span = turn_span();
        let mut totals = TurnSpanTotals::default();
        totals.record(&span, &sample(Some(StopReason::ToolCalls), true, 40));
        totals.record(&span, &sample(Some(StopReason::Length), false, 7));
        let f = fields.lock().unwrap();
        assert_eq!(
            f.strs.get("stop_reason").map(String::as_str),
            Some(StopReason::Length.as_ref())
        );
        assert_eq!(f.bools.get("response.has_tool_call"), Some(&true));
        assert_eq!(f.i64s.get("output_tokens"), Some(&47));
        assert_eq!(
            f.strs.get("last_sample.stop_reason").map(String::as_str),
            Some(StopReason::Length.as_ref())
        );
        assert_eq!(f.bools.get("last_sample.has_tool_call"), Some(&false));
        assert_eq!(f.i64s.get("last_sample.output_tokens"), Some(&7));
    }
    /// tool_calls success then MaxTokensTruncation failure: `last_sample.*` attributes the dying sample.
    #[test]
    fn failed_length_sample_is_attributed_while_legacy_stop_reason_keeps_last_write() {
        let (fields, _guard) = capture();
        let span = turn_span();
        let mut totals = TurnSpanTotals::default();
        totals.record(&span, &sample(Some(StopReason::ToolCalls), true, 40));
        record_failed_sample_on_turn_span(
            &span,
            distill_sampler::SamplingErrorKind::MaxTokensTruncation,
        );
        let f = fields.lock().unwrap();
        assert_eq!(
            f.strs.get("stop_reason").map(String::as_str),
            Some(StopReason::ToolCalls.as_ref())
        );
        assert_eq!(f.bools.get("response.has_tool_call"), Some(&true));
        assert_eq!(f.i64s.get("output_tokens"), Some(&40));
        assert_eq!(
            f.strs.get("last_sample.stop_reason").map(String::as_str),
            Some(StopReason::Length.as_ref())
        );
        assert_eq!(f.bools.get("last_sample.has_tool_call"), Some(&false));
        assert_eq!(f.i64s.get("last_sample.output_tokens"), Some(&0));
    }
    /// `error` / `unreported` sentinels overwrite; no stale previous-sample value survives.
    #[test]
    fn non_length_failure_and_unreported_stop_reason_overwrite_last_sample() {
        let (fields, _guard) = capture();
        let span = turn_span();
        let mut totals = TurnSpanTotals::default();
        totals.record(&span, &sample(Some(StopReason::ToolCalls), true, 40));
        record_failed_sample_on_turn_span(&span, distill_sampler::SamplingErrorKind::Api);
        assert_eq!(
            fields
                .lock()
                .unwrap()
                .strs
                .get("last_sample.stop_reason")
                .map(String::as_str),
            Some(super::LAST_SAMPLE_STOP_REASON_ERROR)
        );
        totals.record(&span, &sample(None, true, 3));
        let f = fields.lock().unwrap();
        assert_eq!(
            f.strs.get("last_sample.stop_reason").map(String::as_str),
            Some(super::LAST_SAMPLE_STOP_REASON_UNREPORTED)
        );
        assert_eq!(f.bools.get("last_sample.has_tool_call"), Some(&true));
        assert_eq!(f.i64s.get("last_sample.output_tokens"), Some(&3));
    }
}
