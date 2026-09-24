// Modified for Distill by Samuel Fajreldines, 2026.
//! The actor's `Submit` handler spawns this task; it owns the retry loop and consumes a Layer 2 stream from the matching backend transform.
//! Cancellation is cooperative via `CancellationToken`.

use std::pin::pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use distill_sampling_types::{
    ApiErrorCode, ConversationRequest, ConversationResponse, EmptyResponseContext, SamplingError,
    SentCredential, error::Result as SamplingResult,
};

use crate::actor::request_metadata::{
    CompletionState, SamplingResultWithMetrics, merge_signal_labels,
};
use crate::client::{ApiBackend, CodexTurnAffinity, SamplingClient};
use crate::config::{RetryPolicy, SamplerConfig};
use crate::doom_loop_recovery::{FailedResponseCapture, append_recovery_context};
use crate::events::{SamplingErrorInfo, SamplingErrorKind, SamplingEvent, StripReason};
use crate::handle::CollectedSamplingResult;
use crate::metrics::InferenceLatencyStats;
use crate::retry::{
    self as retry_mod, RetryDecision, classify_error, clone_error, resolve_max_retries,
};
use crate::stream::responses::stream_responses_tracked;
use crate::stream::{stream_chat_completions, stream_messages};
use crate::types::RequestId;

/// Default per-chunk idle timeout when neither config nor caller supplies one.
/// Matches the shell's session-level default of 5 minutes.
/// That is long enough for cold-start reasoning and short enough to detect dead streams before the user gives up.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

/// Public result returned by `SamplerHandle::submit_and_collect`.
pub type CompletionResult = Result<(ConversationResponse, InferenceLatencyStats), SamplingError>;

#[derive(Debug)]
enum AttemptOutcome {
    /// Stream emitted [`SamplingEvent::Completed`] with a non-empty response.
    Completed {
        response: Box<ConversationResponse>,
        metrics: InferenceLatencyStats,
    },
    /// Stream emitted [`SamplingEvent::Completed`] but the response was empty (no text, no tool calls).
    /// The retry loop treats this as a transient failure (the model returned reasoning-only or the stream was truncated).
    /// Metrics from the empty attempt are discarded; a successful retry produces fresh ones.
    Empty {
        context: EmptyResponseContext,
        doom_loop_signals: Vec<String>,
    },
    /// Stream emitted [`SamplingEvent::Failed`].
    /// The captured raw error is what the retry loop classifies.
    /// If no rich error was captured (e.g. the failure was synthesised inside the L2 transform), `error` was rebuilt from the [`SamplingErrorInfo`].
    Failed {
        error: SamplingError,
        doom_loop_signals: Vec<String>,
        recovery_items: Vec<distill_sampling_types::ConversationItem>,
    },
    /// `cancel_token` fired mid-attempt.
    /// The retry loop bails out without further attempts.
    Cancelled,
    /// Failed to construct the underlying raw stream (e.g., HTTP connect error before any chunks arrive).
    InitFailed { error: SamplingError },
}

/// Run a single sampling request to completion (or final failure).
///
/// Returns the request id so the actor can clean it up from `active_requests` via [`tokio::task::JoinSet::join_next`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_request_task(
    request_id: RequestId,
    request: ConversationRequest,
    config: SamplerConfig,
    retry_policy: RetryPolicy,
    event_tx: mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: CancellationToken,
    completion: Option<oneshot::Sender<CollectedSamplingResult>>,
    codex_turn_affinity: CodexTurnAffinity,
) -> RequestId {
    let mut completion = CompletionState::new(completion);
    let idle_timeout = Duration::from_secs(
        config
            .idle_timeout_secs
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS),
    );
    let configured_max_retries = config.max_retries.or(Some(retry_policy.max_retries));
    let max_retries = if configured_max_retries == Some(0) {
        0
    } else {
        resolve_max_retries(configured_max_retries)
    };

    // Build the initial client
    // Configuration errors here are fatal (no point retrying with the same broken config)
    let mut client = match SamplingClient::new(config.clone()) {
        Ok(mut c) => {
            c.codex_turn_affinity = Some(codex_turn_affinity);
            c
        }
        Err(err) => {
            let terminal_event_queued = emit_failed(&event_tx, &request_id, &err);
            send_completion(&mut completion, Err(err), terminal_event_queued);
            return request_id;
        }
    };

    let sampling_span = crate::sampling_log::request_span(
        &request_id,
        &config.model,
        &format!("{:?}", client.api_backend()),
        &config.base_url,
        &client.auth_info(),
    );
    if let Some(eff) = config.reasoning_effort {
        sampling_span.record("reasoning_effort", eff.as_ref());
    }

    let mut request = request;
    let mut retry_count: u32 = 0;
    // Doom-loop recovery keeps its own resample budget, independent of the transport/empty budget above
    let doom_policy = config.doom_loop_recovery;
    let doom_max_retries = doom_policy.map_or(0, |p| p.max_retries);
    let mut doom_retry_count: u32 = 0;
    let output_observed = Arc::new(AtomicBool::new(false));

    loop {
        sampling_span.record(
            "total_attempts",
            (retry_count + doom_retry_count + 1) as i64,
        );
        if cancel_token.is_cancelled() {
            handle_cancellation(&event_tx, &request_id, &mut completion);
            return request_id;
        }

        // Once the resample budget is spent, the attempt runs with the abort disarmed so it can complete and be accepted as-is
        let doom_check = doom_policy.filter(|_| doom_retry_count < doom_max_retries);
        let outcome = run_one_attempt(
            &client,
            request.clone(),
            request_id.clone(),
            idle_timeout,
            &event_tx,
            &cancel_token,
            doom_check,
            Arc::clone(&output_observed),
        )
        .instrument(sampling_span.clone())
        .await;

        let effective_max_retries =
            if retry_policy.retry_only_before_output && output_observed.load(Ordering::Relaxed) {
                0
            } else {
                max_retries
            };

        match outcome {
            AttemptOutcome::Completed {
                response,
                mut metrics,
            } => {
                completion.merge_doom_loop_signals(
                    response
                        .doom_loop_signals
                        .iter()
                        .map(|signal| signal.raw.clone()),
                );
                metrics.attempts = retry_count + doom_retry_count + 1;
                if let Some(policy) = doom_policy {
                    let confident = policy.confident_triggers(&response.doom_loop_signals);
                    if !confident.is_empty() {
                        tracing::warn!(
                            target: crate::sampling_log::TARGET,
                            triggers = ?confident,
                            attempt = doom_retry_count + 1,
                            outcome = "accepted_after_budget",
                            "doom-loop recovery: resample budget spent; accepting as-is"
                        );
                    }
                }
                // Record token usage on the sampling span alongside effort
                if let Some(usage) = response.usage.as_ref() {
                    sampling_span.record("output_tokens", usage.completion_tokens);
                    sampling_span.record("reasoning_tokens", usage.reasoning_tokens);
                }
                // Emit Completed only after the loop succeeds; the L2 stream's terminal event was suppressed by `run_one_attempt`
                let terminal_event_queued = event_tx
                    .send(SamplingEvent::Completed {
                        request_id: request_id.clone(),
                        response: response.clone(),
                        metrics: metrics.clone(),
                    })
                    .is_ok();
                send_completion(
                    &mut completion,
                    Ok((*response, metrics)),
                    terminal_event_queued,
                );
                return request_id;
            }
            AttemptOutcome::Empty {
                context,
                doom_loop_signals,
            } => {
                completion.merge_doom_loop_signals(doom_loop_signals);
                tracing::warn!(
                    target: crate::sampling_log::TARGET,
                    empty_response = true,
                    empty_reason = context.reason.as_ref(),
                    had_reasoning = context.had_reasoning,
                    content_len = context.content_len,
                    tool_call_count = context.tool_call_count,
                    completion_tokens = context.completion_tokens.unwrap_or(0),
                    reasoning_tokens = context.reasoning_tokens.unwrap_or(0),
                    finish_reason = context.finish_reason_str(),
                    first_choice_seen = context.first_choice_seen,
                    model = %context.model,
                    "empty response from model: {reason} (retrying)",
                    reason = context.reason,
                );
                let err = SamplingError::EmptyResponse { context };
                if !apply_retry_decision(
                    &err,
                    &mut retry_count,
                    effective_max_retries,
                    &retry_policy,
                    &event_tx,
                    &request_id,
                    &mut request,
                    &mut client,
                    &config,
                    &cancel_token,
                    &mut completion,
                    &sampling_span,
                )
                .await
                {
                    return request_id;
                }
            }
            AttemptOutcome::Failed {
                error,
                doom_loop_signals,
                recovery_items,
            } => {
                completion.merge_doom_loop_signals(doom_loop_signals);
                // Doom-loop resamples run on their own budget and never consult the transport classifier
                // No classifier change can silently debit the transport budget for a doom failure
                if let SamplingError::DoomLoopDetected { .. } = &error {
                    // Callers that opted into `retry_only_before_output` cannot retract text already handed to them
                    // A resample would leave the poisoned prefix in the accepted output, so fail the request instead
                    if retry_policy.retry_only_before_output
                        && output_observed.load(Ordering::Relaxed)
                    {
                        let terminal_event_queued = emit_failed(&event_tx, &request_id, &error);
                        send_completion(
                            &mut completion,
                            Err(clone_error(&error)),
                            terminal_event_queued,
                        );
                        return request_id;
                    }
                    let backoff = retry_mod::doom_loop_backoff(doom_retry_count + 1);
                    doom_retry_count += 1;
                    let (recovery_triggers, aborted_at_chunk) = match &error {
                        SamplingError::DoomLoopDetected {
                            triggers,
                            aborted_at_chunk,
                        } => (triggers.clone(), *aborted_at_chunk),
                        _ => unreachable!("doom-loop branch requires DoomLoopDetected"),
                    };
                    completion.record_recovery_attempt(recovery_triggers, aborted_at_chunk);
                    append_recovery_context(&mut request, recovery_items);
                    tracing::warn!(
                        target: crate::sampling_log::TARGET,
                        reason = %error,
                        attempt = doom_retry_count,
                        max_retries = doom_max_retries,
                        outcome = "resampled_with_reminder",
                        "doom-loop recovery: retaining the failed response and retrying with guidance"
                    );
                    emit_retrying(
                        &event_tx,
                        &request_id,
                        doom_retry_count,
                        doom_max_retries,
                        &error,
                    );
                    if sleep_or_cancel(backoff, &cancel_token, doom_retry_count, &sampling_span)
                        .await
                    {
                        continue;
                    }
                    handle_cancellation(&event_tx, &request_id, &mut completion);
                    return request_id;
                }
                if !apply_retry_decision(
                    &error,
                    &mut retry_count,
                    effective_max_retries,
                    &retry_policy,
                    &event_tx,
                    &request_id,
                    &mut request,
                    &mut client,
                    &config,
                    &cancel_token,
                    &mut completion,
                    &sampling_span,
                )
                .await
                {
                    return request_id;
                }
            }
            AttemptOutcome::Cancelled => {
                handle_cancellation(&event_tx, &request_id, &mut completion);
                return request_id;
            }
            AttemptOutcome::InitFailed { error } => {
                if !apply_retry_decision(
                    &error,
                    &mut retry_count,
                    effective_max_retries,
                    &retry_policy,
                    &event_tx,
                    &request_id,
                    &mut request,
                    &mut client,
                    &config,
                    &cancel_token,
                    &mut completion,
                    &sampling_span,
                )
                .await
                {
                    return request_id;
                }
            }
        }
    }
}

/// Apply a [`RetryDecision`]. Returns `true` if the loop should continue, `false` if the request is finished (either fatal or emit-to-session).
/// Performs the side-effects of the decision: sleeping, rebuilding the client, stripping images, emitting the `Retrying` event.
#[allow(clippy::too_many_arguments)]
async fn apply_retry_decision(
    err: &SamplingError,
    retry_count: &mut u32,
    max_retries: u32,
    retry_policy: &RetryPolicy,
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    request_id: &RequestId,
    request: &mut ConversationRequest,
    client: &mut SamplingClient,
    config: &SamplerConfig,
    cancel_token: &CancellationToken,
    completion: &mut CompletionState,
    parent: &tracing::Span,
) -> bool {
    let rate_limit_threshold = config
        .rate_limit_retry_threshold
        .unwrap_or(retry_policy.rate_limit_retry_threshold);
    let decision = classify_error(err, *retry_count, max_retries, rate_limit_threshold);

    // Connection-reset / broken-pipe on body upload often means nginx rejected an oversized payload before responding 413
    // Strip images proactively before any retry of those errors so we don't burn budget re-uploading the same large body
    // A Fatal (budget exhausted) must not mutate the request or tell the user images were "left out of the retry"
    let will_retry = matches!(
        decision,
        RetryDecision::Retry { .. }
            | RetryDecision::RetryWithBackoff { .. }
            | RetryDecision::RetryWithClientRebuild { .. }
    );
    if will_retry && err.is_likely_body_rejected() {
        let stripped_urls = request.strip_images();
        if !stripped_urls.is_empty() {
            tracing::warn!(
                stripped = stripped_urls.len(),
                "stripped {} image(s) before retry (likely nginx 413 via connection reset)",
                stripped_urls.len()
            );
            emit_images_stripped(
                event_tx,
                request_id,
                stripped_urls,
                StripReason::PayloadHeuristic,
            );
        }
    }

    match decision {
        RetryDecision::Retry { backoff } => {
            *retry_count += 1;
            emit_retrying(event_tx, request_id, *retry_count, max_retries, err);
            if sleep_or_cancel(backoff, cancel_token, *retry_count, parent).await {
                true
            } else {
                handle_cancellation(event_tx, request_id, completion);
                false
            }
        }
        RetryDecision::RetryWithBackoff { backoff, .. } => {
            *retry_count += 1;
            emit_retrying(event_tx, request_id, *retry_count, max_retries, err);
            if sleep_or_cancel(backoff, cancel_token, *retry_count, parent).await {
                true
            } else {
                handle_cancellation(event_tx, request_id, completion);
                false
            }
        }
        RetryDecision::RetryWithImageStrip => {
            let stripped_urls = request.strip_images();
            if stripped_urls.is_empty() {
                // Nothing left to strip; upgrade to fatal.
                let terminal_event_queued = emit_failed(event_tx, request_id, err);
                send_completion(completion, Err(clone_error(err)), terminal_event_queued);
                return false;
            }
            let reason = strip_reason_for_image_error(err);
            tracing::warn!(
                stripped = stripped_urls.len(),
                reason = reason.as_ref(),
                error = %err,
                "stripped {} image(s) after an image-related error; retrying without them",
                stripped_urls.len()
            );
            emit_images_stripped(event_tx, request_id, stripped_urls, reason);
            *retry_count += 1;
            emit_retrying(event_tx, request_id, *retry_count, max_retries, err);
            true
        }
        RetryDecision::RetryWithClientRebuild { backoff } => {
            *retry_count += 1;
            emit_retrying(event_tx, request_id, *retry_count, max_retries, err);
            if !sleep_or_cancel(backoff, cancel_token, *retry_count, parent).await {
                handle_cancellation(event_tx, request_id, completion);
                return false;
            }

            // Rebuild client with HTTP/1.1 fallback to escape poisoned HTTP/2 connection pools
            let mut http1_config = config.clone();
            http1_config.force_http1 = true;
            match SamplingClient::new(http1_config) {
                Ok(mut fresh) => {
                    fresh.codex_turn_affinity = client.codex_turn_affinity.take();
                    *client = fresh;
                    tracing::info!("rebuilt sampling client with HTTP/1.1 fallback for retry");
                }
                Err(rebuild_err) => {
                    tracing::warn!(
                        error = %rebuild_err,
                        "failed to rebuild HTTP/1.1 client for retry; reusing existing client"
                    );
                }
            }
            true
        }
        RetryDecision::EmitToSession(emitted_err) => {
            let terminal_event_queued = emit_failed(event_tx, request_id, &emitted_err);
            send_completion(completion, Err(emitted_err), terminal_event_queued);
            false
        }
        RetryDecision::Fatal(fatal_err) => {
            // Emit only on true budget exhaustion (hit the retry / rate-limit cap), mirroring `classify_error`'s Fatal conditions
            // A server `x-should-retry: false` or a non-retryable error is also Fatal but is not "exhausted"
            let next_attempt = *retry_count + 1;
            let server_said_stop = matches!(err.should_retry_header(), Some(false));
            let budget_exhausted = !server_said_stop
                && if err.is_rate_limited() {
                    next_attempt >= max_retries.min(rate_limit_threshold)
                } else {
                    err.is_retryable() && next_attempt >= max_retries
                };
            if budget_exhausted {
                let exhausted_span = tracing::info_span!(
                    "http.retries_exhausted",
                    total_attempts = next_attempt as i64,
                    model = %config.model,
                    error = %err,
                    status_code = tracing::field::Empty,
                );
                let status_code = match err {
                    SamplingError::Api { status, .. } => Some(status.as_u16()),
                    SamplingError::Http(e) => e.status().map(|s| s.as_u16()),
                    _ => None,
                };
                if let Some(status) = status_code {
                    exhausted_span.record("status_code", status as i64);
                }
                exhausted_span.in_scope(|| {});
            }
            let terminal_event_queued = emit_failed(event_tx, request_id, &fatal_err);
            send_completion(completion, Err(fatal_err), terminal_event_queued);
            false
        }
    }
}

async fn sleep_or_cancel(
    duration: Duration,
    cancel_token: &CancellationToken,
    attempt: u32,
    parent: &tracing::Span,
) -> bool {
    let _backoff = crate::span_timing::Region::from_span(tracing::info_span!(
        parent: parent,
        "sampling.retry_backoff",
        attempt = attempt as i64,
        backoff_ms = duration.as_millis() as i64,
    ));
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

/// Run a single attempt: build the raw stream, drive it through the matching L2 transform, and forward all non-terminal events to `event_tx`.
/// `None` disarms the mid-stream abort and the terminal confidence check so the attempt completes and its response can be accepted.
#[allow(clippy::too_many_arguments)]
async fn run_one_attempt(
    client: &SamplingClient,
    request: ConversationRequest,
    request_id: RequestId,
    idle_timeout: Duration,
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: &CancellationToken,
    doom_check: Option<distill_sampling_types::DoomLoopRecoveryPolicy>,
    output_observed: Arc<AtomicBool>,
) -> AttemptOutcome {
    let length_policy = request.length_policy;
    match client.api_backend() {
        ApiBackend::ChatCompletions => {
            let (raw, metadata) = match client.conversation_stream(request).await {
                Ok(pair) => pair,
                Err(e) => return AttemptOutcome::InitFailed { error: e },
            };
            let (teed, captured) = tee_errors(raw);
            let l2 = stream_chat_completions(teed, metadata, request_id.clone(), idle_timeout);
            drive_l2(
                l2,
                request_id,
                event_tx,
                cancel_token,
                captured,
                None,
                FailedResponseCapture::default(),
                output_observed,
                length_policy,
            )
            .await
        }
        ApiBackend::Responses => {
            let (raw, metadata, doom_loop) =
                match client.conversation_stream_responses(request).await {
                    Ok(parts) => parts,
                    Err(e) => return AttemptOutcome::InitFailed { error: e },
                };
            if doom_check.is_none()
                && let Some(collector) = &doom_loop
            {
                collector.disarm_abort();
            }
            let (teed, captured) = tee_errors(raw);
            // Only an armed attempt can replay its failed turn, so only an armed attempt pays for buffering it
            let failed_response = if doom_check.is_some() {
                FailedResponseCapture::armed()
            } else {
                FailedResponseCapture::default()
            };
            let l2 = stream_responses_tracked(
                teed,
                metadata,
                request_id.clone(),
                idle_timeout,
                doom_loop,
                Arc::clone(&output_observed),
                failed_response.clone(),
            );
            drive_l2(
                l2,
                request_id,
                event_tx,
                cancel_token,
                captured,
                doom_check,
                failed_response,
                output_observed,
                length_policy,
            )
            .await
        }
        ApiBackend::Messages => {
            let (raw, metadata) = match client.conversation_stream_messages(request).await {
                Ok(pair) => pair,
                Err(e) => return AttemptOutcome::InitFailed { error: e },
            };
            let (teed, captured) = tee_errors(raw);
            let l2 = stream_messages(teed, metadata, request_id.clone(), idle_timeout);
            drive_l2(
                l2,
                request_id,
                event_tx,
                cancel_token,
                captured,
                None,
                FailedResponseCapture::default(),
                output_observed,
                length_policy,
            )
            .await
        }
    }
}

/// Captured-error cell shared between the tee adapter and the per-request task.
type ErrorCell = Arc<Mutex<Option<SamplingError>>>;

/// Wrap a raw chunk stream so its first error is captured into a shared cell.
/// The wrapped stream still yields the original `Result<T, SamplingError>` items unchanged.
/// The L2 transform sees them and converts them to `SamplingErrorInfo` for events.
fn tee_errors<'a, T: Send + 'a>(
    raw: BoxStream<'a, SamplingResult<T>>,
) -> (BoxStream<'a, SamplingResult<T>>, ErrorCell) {
    let cell: ErrorCell = Arc::new(Mutex::new(None));
    let cell_clone = Arc::clone(&cell);
    let teed = raw
        .map(move |item| {
            if let Err(ref e) = item
                && let Ok(mut guard) = cell_clone.lock()
                && guard.is_none()
            {
                // Capture only the first error; subsequent errors on a torn-down stream are usually secondary effects of the same disconnect
                *guard = Some(clone_error(e));
            }
            item
        })
        .boxed();
    (teed, cell)
}

/// Drive an L2 event stream: forward non-terminal events to `event_tx` and watch `cancel_token`.
/// `doom_check`, when set, turns a completed response carrying confident doom-loop signals into a retryable failure.
#[allow(clippy::too_many_arguments)]
async fn drive_l2(
    l2: impl futures_util::Stream<Item = SamplingEvent>,
    request_id: RequestId,
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    cancel_token: &CancellationToken,
    captured: ErrorCell,
    doom_check: Option<distill_sampling_types::DoomLoopRecoveryPolicy>,
    failed_response: FailedResponseCapture,
    output_observed: Arc<AtomicBool>,
    length_policy: distill_sampling_types::LengthPolicy,
) -> AttemptOutcome {
    let mut l2 = pin!(l2);
    let mut doom_loop_signals = Vec::new();
    let mut await_first_output_span = Some(tracing::info_span!("sampling.await_first_output"));
    loop {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {
                return AttemptOutcome::Cancelled;
            }
            next = l2.next() => match next {
                Some(SamplingEvent::Completed { response, metrics, .. }) => {
                    output_observed.store(true, Ordering::Relaxed);
                    await_first_output_span.take();
                    let mut all_triggers = Vec::new();
                    merge_signal_labels(
                        &mut all_triggers,
                        response.doom_loop_signals.iter().map(|signal| &signal.raw),
                    );
                    if !all_triggers.is_empty() {
                        let _ = event_tx.send(SamplingEvent::DoomLoopSignals {
                            request_id: request_id.clone(),
                            triggers: all_triggers.clone(),
                        });
                    }
                    // Doom outranks the truncation/empty classes: a confident loop poisons the attempt whatever else it looks like
                    if let Some(policy) = doom_check {
                        let triggers = policy.confident_triggers(&response.doom_loop_signals);
                        if !triggers.is_empty() {
                            return AttemptOutcome::Failed {
                                error: SamplingError::DoomLoopDetected {
                                    triggers,
                                    aborted_at_chunk: None,
                                },
                                doom_loop_signals: all_triggers,
                                recovery_items: failed_response.take_items(),
                            };
                        }
                    }
                    // Fail-vs-salvage is centralized in `apply_length_policy`; detector metadata survives a failing verdict
                    // A salvaged response has no empty reason, so it falls through to `Completed` below
                    let response =
                        match crate::client::apply_length_policy(length_policy, *response) {
                            Ok(response) => Box::new(response),
                            Err(error) => {
                                return AttemptOutcome::Failed {
                                    error,
                                    doom_loop_signals: all_triggers,
                                    recovery_items: Vec::new(),
                                };
                            }
                        };
                    // A content-filtered turn (Anthropic refusal, OpenAI
                    // content_filter stop reason) is legitimately content-less and
                    // deterministic — resampling it would retry-storm.
                    let content_filtered = response.stop_reason
                        == Some(distill_sampling_types::StopReason::ContentFilter);
                    if !content_filtered && let Some(reason) = response.empty_reason() {
                        let context = build_empty_context(reason, &response);
                        return AttemptOutcome::Empty {
                            context,
                            doom_loop_signals: all_triggers,
                        };
                    }
                    return AttemptOutcome::Completed { response, metrics };
                }
                Some(SamplingEvent::Failed { error: info, .. }) => {
                    await_first_output_span.take();
                    let raw = captured
                        .lock()
                        .ok()
                        .and_then(|mut g| g.take());
                    let error = raw.unwrap_or_else(|| synthesize_from_info(&info));
                    let recovery_items = if matches!(error, SamplingError::DoomLoopDetected { .. }) {
                        failed_response.take_items()
                    } else {
                        Vec::new()
                    };
                    return AttemptOutcome::Failed {
                        error,
                        doom_loop_signals,
                        recovery_items,
                    };
                }
                Some(SamplingEvent::DoomLoopSignals { triggers, .. }) => {
                    merge_signal_labels(&mut doom_loop_signals, triggers.iter().cloned());
                    let _ = event_tx.send(SamplingEvent::DoomLoopSignals {
                        request_id: request_id.clone(),
                        triggers,
                    });
                }
                Some(other) => {
                    if matches!(
                        other,
                        SamplingEvent::FirstToken { .. }
                            | SamplingEvent::ChannelToken { .. }
                            | SamplingEvent::ToolCallDelta { .. }
                            | SamplingEvent::BackendToolCallStarted { .. }
                            | SamplingEvent::BackendToolCallCompleted { .. }
                    ) {
                        output_observed.store(true, Ordering::Relaxed);
                        await_first_output_span.take();
                    }
                    let _ = event_tx.send(retag(other, &request_id));
                }
                None => {
                    // L2 streams always terminate with Completed or Failed
                    // Reaching None means the producer was dropped without termination; treat it as a synthetic transport error
                    return AttemptOutcome::Failed {
                        error: SamplingError::EventStreamError(
                            "stream dropped without terminal event".to_string(),
                        ),
                        doom_loop_signals,
                        recovery_items: Vec::new(),
                    };
                }
            }
        }
    }
}

/// Re-tag a forwarded event with the canonical request_id.
/// The L2 transform tags events with the id we passed in, so this is usually a no-op; keeping the helper makes the data-flow explicit.
fn retag(event: SamplingEvent, _request_id: &RequestId) -> SamplingEvent {
    event
}

/// Reconstruct a [`SamplingError`] from a [`SamplingErrorInfo`] when there is no captured raw error in the cell.
/// The L2 transform fires synthesised Failed events for idle timeouts, `ResponseFailed`, and server error events.
fn synthesize_from_info(info: &SamplingErrorInfo) -> SamplingError {
    match info.kind {
        SamplingErrorKind::IdleTimeout => SamplingError::IdleTimeout {
            elapsed_secs: info
                .message
                .split_whitespace()
                .find_map(|tok| tok.strip_suffix('s').and_then(|n| n.parse::<u64>().ok()))
                .unwrap_or(0),
        },
        SamplingErrorKind::Auth => SamplingError::Auth {
            message: info.message.clone(),
            credential: info.credential,
        },
        // Must stay Serialization: EventStreamError is retryable, and a response-parse failure is deterministic on retry
        // `info.message` is the variant's rendered Display, so rebuild via the constructor that owns the prefix-stripping
        SamplingErrorKind::Serialization => {
            SamplingError::serialization_from_rendered(&info.message)
        }
        SamplingErrorKind::Http => SamplingError::EventStreamError(info.message.clone()),
        SamplingErrorKind::Api | SamplingErrorKind::RateLimited => {
            let status = info
                .status_code
                .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
                .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
            SamplingError::Api {
                status,
                message: info.message.clone(),
                model_metadata: info.model_metadata.clone(),
                retry_after_secs: info.retry_after_secs,
                should_retry: info.should_retry,
                error_code: info.error_code.clone(),
            }
        }
        SamplingErrorKind::EmptyResponse => {
            if let Some(ctx) = &info.empty_response_context {
                SamplingError::EmptyResponse {
                    context: ctx.clone(),
                }
            } else {
                SamplingError::EventStreamError(info.message.clone())
            }
        }
        SamplingErrorKind::MaxTokensTruncation => SamplingError::MaxTokensTruncation,
        SamplingErrorKind::DoomLoopDetected => SamplingError::DoomLoopDetected {
            triggers: info.doom_loop_triggers.clone().unwrap_or_default(),
            aborted_at_chunk: info.doom_loop_aborted_at_chunk,
        },
    }
}

/// Build an [`EmptyResponseContext`] from a completed-but-empty response.
fn build_empty_context(
    reason: distill_sampling_types::EmptyReason,
    response: &ConversationResponse,
) -> EmptyResponseContext {
    let had_reasoning = response
        .reasoning_items()
        .any(|r| !r.summary.is_empty() || r.content.is_some() || r.encrypted_content.is_some());
    let (content_len, tool_call_count, model, first_choice_seen) = match response.assistant() {
        Some(a) => (
            a.content.len(),
            a.tool_calls.len(),
            a.model_id.clone().unwrap_or_default(),
            // If model_id is set, the L2 saw at least one choice.
            a.model_id.is_some(),
        ),
        None => (0, 0, String::new(), false),
    };

    let finish_reason = response.stop_reason.map(|sr| sr.as_ref().to_owned());
    let (completion_tokens, reasoning_tokens, prompt_tokens) = response
        .usage
        .as_ref()
        .map(|u| {
            (
                Some(u.completion_tokens),
                Some(u.reasoning_tokens),
                Some(u.prompt_tokens),
            )
        })
        .unwrap_or((None, None, None));

    EmptyResponseContext {
        reason,
        had_reasoning,
        content_len,
        tool_call_count,
        finish_reason,
        completion_tokens,
        reasoning_tokens,
        prompt_tokens,
        model,
        first_choice_seen,
    }
}

fn emit_failed(
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    request_id: &RequestId,
    err: &SamplingError,
) -> bool {
    let info = SamplingErrorInfo::from(err);
    event_tx
        .send(SamplingEvent::Failed {
            request_id: request_id.clone(),
            error: info,
        })
        .is_ok()
}

fn emit_retrying(
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    request_id: &RequestId,
    attempt: u32,
    max_retries: u32,
    err: &SamplingError,
) {
    let info = SamplingErrorInfo::from(err);
    let _ = event_tx.send(SamplingEvent::Retrying {
        request_id: request_id.clone(),
        attempt,
        max_retries,
        kind: info.kind,
        reason: err.detail_with_causes(),
        doom_loop_triggers: info.doom_loop_triggers,
        doom_loop_aborted_at_chunk: info.doom_loop_aborted_at_chunk,
    });
}

/// Coded `invalid_image` is `ServerRejected` at any status, including a
/// synthesized Responses 500. Exhaustive so a new `SamplingError` variant
/// must pick a label instead of falling through.
fn strip_reason_for_image_error(err: &SamplingError) -> StripReason {
    match err {
        SamplingError::Api {
            error_code: Some(ApiErrorCode::InvalidImage),
            ..
        }
        | SamplingError::StreamError {
            code: Some(ApiErrorCode::InvalidImage),
            ..
        } => StripReason::ServerRejected,
        SamplingError::Api { .. }
        | SamplingError::StreamError { .. }
        | SamplingError::Auth { .. }
        | SamplingError::InvalidConfiguration(_)
        | SamplingError::MtlsConfiguration(_)
        | SamplingError::Http(_)
        | SamplingError::Serialization(_)
        | SamplingError::EventStreamError(_)
        | SamplingError::IdleTimeout { .. }
        | SamplingError::EmptyResponse { .. }
        | SamplingError::MaxTokensTruncation
        | SamplingError::DoomLoopDetected { .. } => StripReason::PayloadHeuristic,
    }
}

fn emit_images_stripped(
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    request_id: &RequestId,
    stripped_urls: Vec<std::sync::Arc<str>>,
    reason: StripReason,
) {
    let _ = event_tx.send(SamplingEvent::ImagesStripped {
        request_id: request_id.clone(),
        stripped_urls,
        reason,
    });
}

fn handle_cancellation(
    event_tx: &mpsc::UnboundedSender<SamplingEvent>,
    request_id: &RequestId,
    completion: &mut CompletionState,
) {
    // No status code, no upstream API error: this is a client-side termination
    // Use kind=Api so consumers that switch on kind have a sensible default; the message clearly identifies it
    let info = SamplingErrorInfo {
        kind: SamplingErrorKind::Api,
        status_code: None,
        message: "request cancelled".to_string(),
        is_retryable: false,
        retry_after_secs: None,
        should_retry: None,
        error_code: None,
        model_metadata: None,
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: SentCredential::Unknown,
    };
    let terminal_event_queued = event_tx
        .send(SamplingEvent::Failed {
            request_id: request_id.clone(),
            error: info,
        })
        .is_ok();
    send_completion(
        completion,
        Err(SamplingError::auth_unknown("request cancelled")),
        terminal_event_queued,
    );
}

fn send_completion(
    completion: &mut CompletionState,
    result: SamplingResultWithMetrics,
    terminal_event_queued: bool,
) {
    completion.send(result, terminal_event_queued);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use reqwest::StatusCode;
    use distill_sampling_types::ApiErrorCode;

    #[test]
    fn strip_reason_invalid_image_is_server_rejected_on_api_and_stream() {
        let api_400 = SamplingError::Api {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid PNG image.".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: Some(ApiErrorCode::InvalidImage),
        };
        assert_eq!(
            strip_reason_for_image_error(&api_400),
            StripReason::ServerRejected
        );

        // Responses `response.failed` is synthesized as Api 500 with the wire code.
        let api_500 = SamplingError::Api {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "invalid_image: Invalid PNG image.".into(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: Some(ApiErrorCode::InvalidImage),
        };
        assert_eq!(
            strip_reason_for_image_error(&api_500),
            StripReason::ServerRejected
        );

        let stream = SamplingError::StreamError {
            error_type: "invalid_request_error".into(),
            message: "Invalid PNG image.".into(),
            code: Some(ApiErrorCode::InvalidImage),
        };
        assert_eq!(
            strip_reason_for_image_error(&stream),
            StripReason::ServerRejected
        );

        let heuristic = SamplingError::StreamError {
            error_type: "overloaded_error".into(),
            message: "The server is overloaded.".into(),
            code: None,
        };
        assert_eq!(
            strip_reason_for_image_error(&heuristic),
            StripReason::PayloadHeuristic
        );
    }

    fn completed_response(
        stop_reason: Option<distill_sampling_types::StopReason>,
        content: &str,
    ) -> ConversationResponse {
        ConversationResponse {
            items: vec![distill_sampling_types::ConversationItem::assistant(
                content,
            )],
            stop_reason,
            usage: None,
            cost_usd_ticks: None,
            message_chunks_emitted: u64::from(!content.is_empty()),
            doom_loop_signals: vec![distill_sampling_types::doom_loop::DoomLoopSignal::parse(
                "exact_repetition:42x3@thinking",
            )],
            stop_message: None,
            message_id: None,
            raw_stop_reason: None,
            stop_sequence: None,
        }
    }

    fn length_completed_event(text: &str) -> SamplingEvent {
        let mut response =
            completed_response(Some(distill_sampling_types::StopReason::Length), text);
        response.doom_loop_signals.clear();
        SamplingEvent::Completed {
            request_id: RequestId::random(),
            response: Box::new(response),
            metrics: Default::default(),
        }
    }

    async fn drive_length_event(
        event: SamplingEvent,
        policy: distill_sampling_types::LengthPolicy,
    ) -> AttemptOutcome {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        drive_l2(
            stream::iter([event]),
            RequestId::random(),
            &event_tx,
            &CancellationToken::new(),
            Arc::new(Mutex::new(None)),
            None,
            FailedResponseCapture::default(),
            Arc::new(AtomicBool::new(false)),
            policy,
        )
        .await
    }

    async fn terminal_outcome(response: ConversationResponse) -> AttemptOutcome {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        drive_l2(
            stream::iter([SamplingEvent::Completed {
                request_id: RequestId::from("terminal-signals"),
                response: Box::new(response),
                metrics: InferenceLatencyStats::default(),
            }]),
            RequestId::from("terminal-signals"),
            &event_tx,
            &CancellationToken::new(),
            Arc::new(Mutex::new(None)),
            None,
            FailedResponseCapture::default(),
            Arc::new(AtomicBool::new(false)),
            distill_sampling_types::LengthPolicy::Fail,
        )
        .await
    }

    #[tokio::test]
    async fn length_and_empty_outcomes_retain_terminal_detector_signals() {
        let length = terminal_outcome(completed_response(
            Some(distill_sampling_types::StopReason::Length),
            "truncated",
        ))
        .await;
        assert!(matches!(
            length,
            AttemptOutcome::Failed {
                doom_loop_signals,
                ..
            } if doom_loop_signals == ["exact_repetition:42x3@thinking".to_string()]
        ));

        let empty = terminal_outcome(completed_response(None, "")).await;
        assert!(matches!(
            empty,
            AttemptOutcome::Empty {
                doom_loop_signals,
                ..
            } if doom_loop_signals == ["exact_repetition:42x3@thinking".to_string()]
        ));
    }

    #[tokio::test]
    async fn terminal_detector_signals_are_bounded_before_forwarding() {
        use crate::doom_loop::{MAX_COLLECTED_DOOM_LOOP_SIGNALS, MAX_DOOM_LOOP_SIGNAL_BYTES};

        let mut response = completed_response(
            Some(distill_sampling_types::StopReason::Length),
            "truncated",
        );
        response.doom_loop_signals =
            std::iter::once(distill_sampling_types::doom_loop::DoomLoopSignal::parse(
                &"x".repeat(MAX_DOOM_LOOP_SIGNAL_BYTES + 1),
            ))
            .chain((0..MAX_COLLECTED_DOOM_LOOP_SIGNALS + 20).map(|index| {
                distill_sampling_types::doom_loop::DoomLoopSignal::parse(&format!(
                    "unknown_{index}@thinking"
                ))
            }))
            .collect();

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let outcome = drive_l2(
            stream::iter([SamplingEvent::Completed {
                request_id: RequestId::from("bounded-terminal-signals"),
                response: Box::new(response),
                metrics: InferenceLatencyStats::default(),
            }]),
            RequestId::from("bounded-terminal-signals"),
            &event_tx,
            &CancellationToken::new(),
            Arc::new(Mutex::new(None)),
            None,
            FailedResponseCapture::default(),
            Arc::new(AtomicBool::new(false)),
            distill_sampling_types::LengthPolicy::Fail,
        )
        .await;

        let AttemptOutcome::Failed {
            doom_loop_signals, ..
        } = outcome
        else {
            panic!("length completion must fail");
        };
        assert_eq!(MAX_COLLECTED_DOOM_LOOP_SIGNALS, doom_loop_signals.len());
        assert!(
            doom_loop_signals
                .iter()
                .all(|label| label.len() <= MAX_DOOM_LOOP_SIGNAL_BYTES)
        );
        let SamplingEvent::DoomLoopSignals { triggers, .. } =
            event_rx.try_recv().expect("bounded signal event")
        else {
            panic!("expected detector signal event");
        };
        assert_eq!(doom_loop_signals, triggers);
    }

    #[tokio::test]
    async fn length_policy_fail_converts_completed_to_failed() {
        let outcome = drive_length_event(
            length_completed_event("partial"),
            distill_sampling_types::LengthPolicy::Fail,
        )
        .await;
        assert!(matches!(
            outcome,
            AttemptOutcome::Failed {
                error: SamplingError::MaxTokensTruncation,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn length_policy_salvage_completes_with_partial_content() {
        let outcome = drive_length_event(
            length_completed_event("partial"),
            distill_sampling_types::LengthPolicy::CompletePartial,
        )
        .await;
        let AttemptOutcome::Completed { response, .. } = outcome else {
            panic!("expected completed partial response");
        };
        assert_eq!(response.assistant_text(), "partial");
        assert_eq!(
            response.stop_reason,
            Some(distill_sampling_types::StopReason::Length)
        );
    }

    fn with_tool_call(mut event: SamplingEvent, arguments: &str) -> SamplingEvent {
        let SamplingEvent::Completed { response, .. } = &mut event else {
            unreachable!("helper builds Completed");
        };
        let Some(distill_sampling_types::ConversationItem::Assistant(a)) =
            response.items.last_mut()
        else {
            unreachable!("helper builds a trailing Assistant item");
        };
        a.tool_calls = vec![distill_sampling_types::ToolCall {
            id: "call_1".into(),
            name: "do_thing".into(),
            arguments: arguments.into(),
        }];
        event
    }

    /// Argument-truncated tool calls never execute; both salvaging policies still fail them.
    #[tokio::test]
    async fn length_policy_truncated_tool_call_arguments_still_fail() {
        for policy in [
            distill_sampling_types::LengthPolicy::CompleteToolCalls,
            distill_sampling_types::LengthPolicy::CompletePartial,
        ] {
            let event = with_tool_call(length_completed_event("partial"), "{\"x\": \"trunc");
            let outcome = drive_length_event(event, policy).await;
            match outcome {
                AttemptOutcome::Failed { error, .. } => {
                    assert!(matches!(error, SamplingError::MaxTokensTruncation));
                }
                other => panic!("expected Failed(MaxTokensTruncation), got {other:?}"),
            }
        }
    }

    /// Default policy: Length with completed tool calls is delivered for execution, with the Length stop reason kept visible for telemetry.
    #[tokio::test]
    async fn length_policy_default_completes_with_completed_tool_calls() {
        let event = with_tool_call(length_completed_event("partial"), "{\"x\": 1}");
        let outcome =
            drive_length_event(event, distill_sampling_types::LengthPolicy::default()).await;
        match outcome {
            AttemptOutcome::Completed { response, .. } => {
                assert_eq!(response.tool_calls().len(), 1);
                let Some(call) = response.tool_calls().first() else {
                    panic!("expected a tool call");
                };
                assert_eq!(call.arguments.as_ref(), "{\"x\": 1}");
                assert_eq!(
                    response.stop_reason,
                    Some(distill_sampling_types::StopReason::Length)
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn length_policy_salvage_empty_still_fails() {
        let outcome = drive_length_event(
            length_completed_event(""),
            distill_sampling_types::LengthPolicy::CompletePartial,
        )
        .await;
        assert!(matches!(
            outcome,
            AttemptOutcome::Failed {
                error: SamplingError::MaxTokensTruncation,
                ..
            }
        ));
    }

    #[test]
    fn synthesize_idle_timeout_extracts_elapsed_secs() {
        let info = SamplingErrorInfo {
            kind: SamplingErrorKind::IdleTimeout,
            status_code: None,
            message: "inference idle timeout after 240s with no chunks".to_string(),
            is_retryable: false,
            retry_after_secs: None,
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: SentCredential::Unknown,
        };
        let err = synthesize_from_info(&info);
        match err {
            SamplingError::IdleTimeout { elapsed_secs } => assert_eq!(elapsed_secs, 240),
            other => panic!("expected IdleTimeout, got {other:?}"),
        }
    }

    #[test]
    fn synthesize_api_500_round_trips() {
        let info = SamplingErrorInfo {
            kind: SamplingErrorKind::Api,
            status_code: Some(500),
            message: "boom".to_string(),
            is_retryable: true,
            retry_after_secs: None,
            should_retry: Some(false),
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: SentCredential::Unknown,
        };
        let err = synthesize_from_info(&info);
        match err {
            SamplingError::Api {
                status,
                message,
                should_retry,
                ..
            } => {
                assert_eq!(status.as_u16(), 500);
                assert_eq!(message, "boom");
                assert_eq!(should_retry, Some(false), "server veto must survive");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    /// A coded invalid-image error must survive the info round trip.
    /// The message alone would not classify, so losing the code would silently disable strip recovery on synthesized failures.
    #[test]
    fn synthesize_preserves_error_code_and_image_classification() {
        let original = SamplingError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "some future wording without the legacy phrase".to_string(),
            model_metadata: None,
            retry_after_secs: None,
            should_retry: None,
            error_code: Some(ApiErrorCode::InvalidImage),
        };
        assert!(original.is_image_processing_error());

        let info = SamplingErrorInfo::from(&original);
        assert_eq!(info.error_code, Some(ApiErrorCode::InvalidImage));

        let round_tripped = synthesize_from_info(&info);
        assert!(
            round_tripped.is_image_processing_error(),
            "round-tripped error must still classify: {round_tripped:?}"
        );
    }

    /// A StreamError-sourced info has `status_code: None`; synthesis falls back to a 500 Api error.
    /// That fallback must stay inside the classifier's 400|500 gate.
    /// Otherwise coded mid-stream image rejections silently stop stripping after the round trip.
    #[test]
    fn synthesize_stream_sourced_info_still_classifies_for_strip() {
        let original = SamplingError::StreamError {
            error_type: "invalid_request_error".into(),
            message: "bad image".into(),
            code: Some(ApiErrorCode::InvalidImage),
        };
        assert!(original.is_image_processing_error());

        let info = SamplingErrorInfo::from(&original);
        assert_eq!(info.status_code, None, "stream errors carry no status");

        let round_tripped = synthesize_from_info(&info);
        assert!(
            round_tripped.is_image_processing_error(),
            "stream-sourced round trip must still classify: {round_tripped:?}"
        );
    }

    #[test]
    fn synthesize_rate_limited_preserves_retry_after() {
        let info = SamplingErrorInfo {
            kind: SamplingErrorKind::RateLimited,
            status_code: Some(429),
            message: "slow down".to_string(),
            is_retryable: true,
            retry_after_secs: Some(7),
            should_retry: None,
            error_code: None,
            model_metadata: None,
            empty_response_context: None,
            doom_loop_triggers: None,
            doom_loop_aborted_at_chunk: None,
            credential: SentCredential::Unknown,
        };
        let err = synthesize_from_info(&info);
        match err {
            SamplingError::Api {
                status,
                retry_after_secs,
                ..
            } => {
                assert_eq!(status.as_u16(), 429);
                assert_eq!(retry_after_secs, Some(7));
            }
            other => panic!("expected Api(429), got {other:?}"),
        }
    }

    #[test]
    fn synthesize_serialization_stays_serialization() {
        // Round-trip a REAL error's Display so a Display-template rewording cannot silently reintroduce double-prefixing
        let original = SamplingError::Serialization(
            serde_json::from_str::<i32>("missing field `delta`").unwrap_err(),
        );
        let info = SamplingErrorInfo::from(&original);
        let err = synthesize_from_info(&info);
        assert!(
            matches!(err, SamplingError::Serialization(_)),
            "expected Serialization, got {err:?}"
        );
        assert!(!err.is_retryable());
        assert_eq!(
            err.to_string(),
            info.message,
            "rebuilt Display must round-trip without double-prefixing"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_sleep_returns_immediately_on_cancellation() {
        let cancel_token = CancellationToken::new();
        let parent = tracing::Span::none();
        let sleeper = sleep_or_cancel(Duration::from_secs(120), &cancel_token, 1, &parent);
        tokio::pin!(sleeper);

        cancel_token.cancel();
        assert!(!sleeper.await);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_decision_cancellation_emits_terminal_cancel() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (completion_tx, completion_rx) = oneshot::channel();
        let mut completion = CompletionState::new(Some(completion_tx));
        let mut retry_count = 0;
        let mut request = ConversationRequest::default();
        let config = SamplerConfig {
            base_url: "http://localhost".into(),
            model: "test-model".into(),
            ..Default::default()
        };
        let mut client = SamplingClient::new(config.clone()).expect("test client");
        let error = SamplingError::EventStreamError("retry me".into());

        let should_continue = apply_retry_decision(
            &error,
            &mut retry_count,
            2,
            &RetryPolicy::default(),
            &event_tx,
            &RequestId::from("cancel-backoff"),
            &mut request,
            &mut client,
            &config,
            &cancel_token,
            &mut completion,
            &tracing::Span::none(),
        )
        .await;

        assert!(!should_continue);
        assert!(matches!(
            event_rx.recv().await,
            Some(SamplingEvent::Retrying { .. })
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(SamplingEvent::Failed { .. })
        ));
        assert!(
            completion_rx
                .await
                .expect("completion sent")
                .result
                .is_err()
        );
    }

    #[tokio::test]
    async fn tee_captures_first_error_only() {
        let items: Vec<SamplingResult<u32>> = vec![
            Ok(1),
            Err(SamplingError::EventStreamError("first".into())),
            Err(SamplingError::EventStreamError("second".into())),
        ];
        let raw = stream::iter(items).boxed();
        let (mut teed, cell) = tee_errors(raw);
        while teed.next().await.is_some() {}
        let captured = cell.lock().unwrap().take().expect("error captured");
        match captured {
            SamplingError::EventStreamError(msg) => assert_eq!(msg, "first"),
            other => panic!("expected EventStreamError, got {other:?}"),
        }
    }
}
