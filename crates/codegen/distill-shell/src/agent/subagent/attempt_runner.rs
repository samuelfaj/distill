// Modified for Distill by Samuel Fajreldines, 2026.
//! Executes the existing single-prompt child attempt while its session actor is live.
use super::*;
use crate::session::commands::PromptTurnResult as SubagentPromptTurnResult;
use std::future::Future;
#[derive(Debug)]
pub(super) enum InitialChildPromptReadiness<T> {
    Cancelled,
    Admitted(oneshot::Sender<()>),
    AttemptCompleted(T),
    TimedOut,
}
impl<T> InitialChildPromptReadiness<T> {
    /// Only the admission deadline is a timeout; cancel and a failed `started` promotion stay cancelled.
    pub(super) fn unpromoted_disposition(&self) -> UnpromotedChildDisposition {
        match self {
            Self::TimedOut => UnpromotedChildDisposition::AdmissionTimedOut,
            Self::Cancelled | Self::Admitted(_) | Self::AttemptCompleted(_) => {
                UnpromotedChildDisposition::Cancelled
            }
        }
    }
}
/// Deterministic precedence: cancellation, then a successful readiness ack, then the attempt result, then the admission deadline.
pub(super) async fn wait_initial_child_prompt_readiness<Fut, T>(
    cancelled: impl Future<Output = ()>,
    readiness: oneshot::Receiver<oneshot::Sender<()>>,
    attempt: &mut Fut,
    timeout: std::time::Duration,
) -> InitialChildPromptReadiness<T>
where
    Fut: Future<Output = T> + Unpin,
{
    tokio::select! {
        biased;
        _ = cancelled => InitialChildPromptReadiness::Cancelled,
        Ok(release) = readiness => InitialChildPromptReadiness::Admitted(release),
        outcome = &mut *attempt => InitialChildPromptReadiness::AttemptCompleted(outcome),
        _ = tokio::time::sleep(timeout) => InitialChildPromptReadiness::TimedOut,
    }
}
pub(super) fn subagent_trace_prefix(session_id: &str, turn_number: u64) -> String {
    format!("{session_id}/turn_{turn_number}")
}
pub(super) struct OneTurnAttemptInput<'a> {
    pub child_handle: &'a SessionHandle,
    pub request: &'a SubagentRequest,
    pub worktree_path: Option<&'a Path>,
    pub task_prompt_text: &'a str,
    pub prompt_id: String,
    pub inherited_tool_overrides: Option<distill_sampling_types::ToolOverrides>,
    pub gcs_bucket_url: Option<&'a str>,
    pub gcs_upload_method: Option<&'a crate::session::repo_changes::UploadMethod>,
    pub turn_number: u64,
    pub cancel_token: CancellationToken,
    pub child_run_started_at: std::time::Instant,
    pub prompt_admitted: oneshot::Sender<oneshot::Sender<()>>,
    #[cfg(test)]
    pub initial_attempt_behavior: InitialAttemptBehavior,
}
pub(super) struct OneTurnTraceCapture {
    pub before_copy_rx:
        oneshot::Receiver<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
    pub child_prompt_id: String,
    pub turn_started_at: String,
    pub turn_token_totals: Option<(u64, u64, u64)>,
    pub turn_number: u64,
}
pub(super) struct OneTurnAttemptOutcome {
    pub result: SubagentResult,
    pub trace: OneTurnTraceCapture,
    pub cancellation_may_hide_usage: bool,
}
pub(super) struct OneTurnUsageInput<'a> {
    pub child_handle: &'a SessionHandle,
    pub task_budget_usage: Option<(u64, bool)>,
    pub cancellation_may_hide_usage: bool,
    pub parent_cmd_tx: Option<&'a mpsc::UnboundedSender<SessionCommand>>,
    pub parent_prompt_id: Option<&'a str>,
}
#[tracing::instrument(skip_all)]
pub(super) async fn run_one_turn_attempt(
    mut input: OneTurnAttemptInput<'_>,
) -> OneTurnAttemptOutcome {
    let (before_copy_tx, before_copy_rx) = oneshot::channel();
    let _ = input.child_handle.cmd_tx.send(SessionCommand::CopyFile {
        respond_to: before_copy_tx,
    });
    if let Some(overrides) = input.inherited_tool_overrides.take() {
        let _ = input
            .child_handle
            .cmd_tx
            .send(SessionCommand::SetToolOverrides { overrides });
    }
    let (prompt_tx, prompt_rx) = oneshot::channel::<SubagentPromptTurnResult>();
    #[cfg(test)]
    if input.initial_attempt_behavior == InitialAttemptBehavior::CompleteBeforeAdmission {
        drop(prompt_tx);
        drop(input.prompt_admitted);
        return OneTurnAttemptOutcome {
            result: SubagentResult {
                success: false,
                error: Some("injected pre-admission attempt failure".to_owned()),
                ..base_result(input.request, input.worktree_path, 0, 1, 0)
            },
            trace: OneTurnTraceCapture {
                before_copy_rx,
                child_prompt_id: input.prompt_id,
                turn_started_at: chrono::Utc::now().to_rfc3339(),
                turn_token_totals: None,
                turn_number: input.turn_number,
            },
            cancellation_may_hide_usage: false,
        };
    }
    let child_prompt_id = input.prompt_id;
    let turn_started_at = chrono::Utc::now().to_rfc3339();
    let _ = input.child_handle.cmd_tx.send(SessionCommand::Prompt {
        prompt_id: child_prompt_id.clone(),
        prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
            input.task_prompt_text.to_owned(),
        ))],
        prompt_mode: crate::session::plan_mode::PromptMode::Agent,
        artifact_upload_ctx: input.gcs_bucket_url.and_then(|_| {
            input
                .gcs_upload_method
                .map(|method| crate::upload::manifest::ArtifactUploadContext {
                    gcs_config: crate::session::repo_changes::TraceExportConfig {
                        bucket_url: input.gcs_bucket_url.map(str::to_owned),
                        service_account_key: None,
                        prefix_dir: None,
                        gcs_prefix: Some(subagent_trace_prefix(
                            &input.request.id,
                            input.turn_number,
                        )),
                        absolute_paths: false,
                        archive_name_override: None,
                        upload_method: method.clone(),
                    },
                    artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
                })
        }),
        client_identifier: None,
        screen_mode: None,
        verbatim: true,
        traceparent: distill_otel::current_traceparent(),
        json_schema: input.request.runtime_overrides.output_schema.clone(),
        send_now: false,
        admission: None,
        tool_overrides_update: None,
        respond_to: prompt_tx,
        prompt_admitted: Some(input.prompt_admitted),
        persist_ack: None,
        parsed_prompt_tx: None,
    });
    let mut turn_token_totals = None;
    let wait_outcome =
        await_subagent_turn_or_cancellation(prompt_rx, input.cancel_token.clone()).await;
    let duration_ms = input.child_run_started_at.elapsed().as_millis() as u64;
    let (result, cancellation_may_hide_usage) = match wait_outcome {
        SubagentWaitOutcome::Cancelled => {
            let counts = signals_snapshot_counts(input.child_handle).await;
            let may_hide_usage =
                counts.is_none_or(|(tool_calls, turns)| turns > 0 || tool_calls > 0);
            let (tool_calls, turns) = counts.unwrap_or((0, 0));
            (
                SubagentResult {
                    success: false,
                    cancelled: true,
                    error: Some("Subagent was cancelled".to_string()),
                    ..base_result(
                        input.request,
                        input.worktree_path,
                        tool_calls,
                        turns,
                        duration_ms,
                    )
                },
                may_hide_usage,
            )
        }
        SubagentWaitOutcome::TurnResult(turn_result) => {
            let was_cancelled = input.cancel_token.is_cancelled();
            let (tool_calls, turns) = match &*turn_result {
                Ok(Ok(crate::session::commands::PromptTurnOk {
                    turn_snapshot: Some(snapshot),
                    ..
                })) => {
                    turn_token_totals = Some((
                        snapshot.turn_input_tokens,
                        snapshot.turn_cached_input_tokens,
                        snapshot.turn_output_tokens,
                    ));
                    (
                        snapshot.current.tool_call_count,
                        snapshot.current.turn_count,
                    )
                }
                _ => signals_snapshot_counts(input.child_handle)
                    .await
                    .unwrap_or((0, 0)),
            };
            let final_text = super::handle_request::child_actor_query(
                "trailing_assistant_report",
                input
                    .child_handle
                    .chat_state_handle
                    .get_trailing_assistant_report(),
                None,
            )
            .await
            .unwrap_or_default();
            let result_tokens = super::handle_request::child_actor_query(
                "total_tokens",
                input.child_handle.chat_state_handle.get_total_tokens(),
                0,
            )
            .await;
            let success_summary = || {
                format!(
                    "Subagent '{}' ({}) completed successfully. {tool_calls} tool calls, \
                     {turns} turns.",
                    input.request.description.as_str(),
                    input.request.subagent_type.as_str()
                )
            };
            let max_turns_summary = |limit| {
                format!(
                    "Subagent '{}' ({}) hit max-turns limit ({limit}). {tool_calls} tool calls, \
                     {turns} turns.",
                    input.request.description.as_str(),
                    input.request.subagent_type.as_str()
                )
            };
            let cancelled_summary = || {
                format!(
                    "Subagent '{}' ({}) was cancelled. {tool_calls} tool calls, {turns} turns.",
                    input.request.description.as_str(),
                    input.request.subagent_type.as_str()
                )
            };
            let folded = super::prompt_turn_result::reduce_prompt_turn_result(
                super::prompt_turn_result::PromptTurnResultInput {
                    result: base_result(
                        input.request,
                        input.worktree_path,
                        tool_calls,
                        turns,
                        duration_ms,
                    ),
                    turn_result: *turn_result,
                    mode: super::prompt_turn_result::PromptTurnResultMode::Initial {
                        requires_structured_output: input
                            .request
                            .runtime_overrides
                            .output_schema
                            .is_some(),
                    },
                    final_text,
                    was_cancelled,
                    summaries: super::prompt_turn_result::PromptTurnResultSummaries {
                        success: &success_summary,
                        max_turns: &max_turns_summary,
                        cancelled: &cancelled_summary,
                    },
                    result_tokens,
                },
            );
            (folded.result, folded.cancellation_may_hide_usage)
        }
    };
    OneTurnAttemptOutcome {
        result,
        trace: OneTurnTraceCapture {
            before_copy_rx,
            child_prompt_id,
            turn_started_at,
            turn_token_totals,
            turn_number: input.turn_number,
        },
        cancellation_may_hide_usage,
    }
}
pub(super) fn canonical_total_tokens(totals: &distill_chat_state::UsageTotals) -> u64 {
    totals.total_tokens()
}
pub(super) fn usage_is_incomplete(
    ledger_incomplete: bool,
    cancellation_may_hide_usage: bool,
) -> bool {
    ledger_incomplete || cancellation_may_hide_usage
}
const LATE_USAGE_RECONCILE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(305);
const LATE_USAGE_RECONCILE_POLL: std::time::Duration = std::time::Duration::from_secs(1);

pub(super) async fn record_subagent_usage(
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    by_model: Option<Vec<(String, distill_chat_state::UsageTotals)>>,
    parent_prompt_id: Option<String>,
    incomplete: bool,
) -> bool {
    record_subagent_usage_with_attributions(
        parent_cmd_tx,
        by_model,
        Vec::new(),
        parent_prompt_id,
        incomplete,
    )
    .await
}

pub(super) async fn record_subagent_usage_with_attributions(
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    by_model: Option<Vec<(String, distill_chat_state::UsageTotals)>>,
    attributions: Vec<distill_chat_state::UsageAttribution>,
    parent_prompt_id: Option<String>,
    incomplete: bool,
) -> bool {
    record_subagent_usage_with_attributions_and_pending(
        parent_cmd_tx,
        by_model,
        attributions,
        Vec::new(),
        parent_prompt_id,
        incomplete,
    )
    .await
}

pub(super) async fn record_subagent_usage_with_attributions_and_pending(
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    by_model: Option<Vec<(String, distill_chat_state::UsageTotals)>>,
    attributions: Vec<distill_chat_state::UsageAttribution>,
    pending_attempts: Vec<String>,
    parent_prompt_id: Option<String>,
    incomplete: bool,
) -> bool {
    match by_model {
        None => false,
        Some(by_model)
            if by_model.is_empty()
                && attributions.is_empty()
                && pending_attempts.is_empty()
                && !incomplete =>
        {
            true
        }
        Some(by_model) => {
            let Some(cmd_tx) = parent_cmd_tx else {
                return false;
            };
            let (respond_to, ack) = oneshot::channel();
            if cmd_tx
                .send(SessionCommand::RecordSubagentUsage {
                    by_model,
                    attributions,
                    pending_attempts,
                    parent_prompt_id,
                    incomplete,
                    respond_to,
                })
                .is_err()
            {
                return false;
            }
            match tokio::time::timeout(super::handle_request::PARENT_ACK_TIMEOUT, ack).await {
                Ok(acked) => acked.is_ok(),
                Err(_) => false,
            }
        }
    }
}

fn spawn_late_usage_reconciler(
    child_handle: SessionHandle,
    parent_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    parent_prompt_id: Option<String>,
    mut pending_attempts: Vec<String>,
) {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + LATE_USAGE_RECONCILE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                tracing::debug!(
                    pending_attempts = ?pending_attempts,
                    "late child usage reconciliation reached its bound"
                );
                break;
            }

            let query = tokio::time::timeout(
                remaining.min(super::handle_request::PARENT_ACK_TIMEOUT),
                child_handle.chat_state_handle.try_get_session_usage(),
            )
            .await;
            let Ok(Ok(usage)) = query else {
                break;
            };

            let terminal = usage
                .attributions
                .iter()
                .filter(|attribution| {
                    pending_attempts
                        .iter()
                        .any(|attempt_id| attempt_id == &attribution.attempt_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            let still_pending = pending_attempts
                .iter()
                .any(|attempt_id| usage.pending_attempts.contains(attempt_id));

            if !terminal.is_empty() {
                if !record_subagent_usage_with_attributions_and_pending(
                    Some(&parent_cmd_tx),
                    Some(Vec::new()),
                    terminal,
                    Vec::new(),
                    parent_prompt_id.clone(),
                    usage.incomplete,
                )
                .await
                {
                    break;
                }
                pending_attempts.retain(|attempt_id| usage.pending_attempts.contains(attempt_id));
                if pending_attempts.is_empty() {
                    break;
                }
            } else if !still_pending {
                break;
            }

            tokio::time::sleep(remaining.min(LATE_USAGE_RECONCILE_POLL)).await;
        }
    });
}

pub(super) async fn capture_and_fold_one_turn_usage(
    result: &mut SubagentResult,
    input: OneTurnUsageInput<'_>,
) -> bool {
    let (
        by_model,
        attributions,
        pending_attempts,
        incomplete,
        output_incomplete,
        output_tokens,
        total_tokens,
    ) =
        match super::handle_request::child_actor_query(
            "session_usage",
            input.child_handle.chat_state_handle.try_get_session_usage(),
            Err(()),
        )
        .await
        {
            Ok(usage) => {
                let output_tokens = usage.totals.output_tokens;
                let total_tokens = canonical_total_tokens(&usage.totals);
                let incomplete =
                    usage_is_incomplete(usage.incomplete, input.cancellation_may_hide_usage);
                let output_incomplete =
                    usage_is_incomplete(usage.is_incomplete(), input.cancellation_may_hide_usage);
                // A mixed child ledger cannot be represented by identity rows
                // without a residual aggregate calculation. Keep its proven
                // aggregate total; once every counted call has an attribution,
                // send rows only so the parent cannot add both projections.
                let fully_attributed = usage.attributions.len() as u64 == usage.totals.model_calls;
                let attributions = fully_attributed.then_some(usage.attributions).unwrap_or_default();
                let by_model = fully_attributed
                    .then_some(Vec::new())
                    .or_else(|| Some(usage.by_model.into_iter().collect::<Vec<_>>()));
                let pending_attempts = usage.pending_attempts.into_iter().collect::<Vec<_>>();
                (
                    by_model,
                    attributions,
                    pending_attempts,
                    incomplete,
                    output_incomplete,
                    (!output_incomplete).then_some(output_tokens),
                    Some(total_tokens),
                )
            }
            Err(()) => (None, Vec::new(), Vec::new(), true, true, None, None),
        };
    result.total_tokens_used = total_tokens.unwrap_or(0);
    if let Some((task_spent, task_incomplete)) = input.task_budget_usage {
        result.output_tokens_used = output_tokens.unwrap_or(task_spent);
        result.output_usage_incomplete =
            task_incomplete || output_incomplete || output_tokens.is_none();
    } else {
        result.output_tokens_used = output_tokens.unwrap_or(0);
        result.output_usage_incomplete = output_incomplete || output_tokens.is_none();
    }
    let late_pending_attempts = pending_attempts.clone();
    let fold_acked = record_subagent_usage_with_attributions_and_pending(
        input.parent_cmd_tx,
        by_model,
        attributions,
        pending_attempts,
        input.parent_prompt_id.map(str::to_owned),
        incomplete,
    )
    .await;
    if fold_acked && !late_pending_attempts.is_empty() {
        if let Some(parent_cmd_tx) = input.parent_cmd_tx.cloned() {
            spawn_late_usage_reconciler(
                input.child_handle.clone(),
                parent_cmd_tx,
                input.parent_prompt_id.map(str::to_owned),
                late_pending_attempts,
            );
        }
    }
    fold_acked
}
#[cfg(test)]
mod trace_turn_tests {
    use super::subagent_trace_prefix;
    #[test]
    fn child_attempts_use_toolbox_discoverable_turn_paths() {
        assert_eq!(subagent_trace_prefix("child", 0), "child/turn_0");
        assert_eq!(subagent_trace_prefix("child", 1), "child/turn_1");
        assert_eq!(subagent_trace_prefix("child", 2), "child/turn_2");
    }
}
fn base_result(
    request: &SubagentRequest,
    worktree_path: Option<&Path>,
    tool_calls: u32,
    turns: u32,
    duration_ms: u64,
) -> SubagentResult {
    SubagentResult {
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        tool_calls,
        turns,
        duration_ms,
        worktree_path: worktree_path.map(|path| path.to_string_lossy().into_owned()),
        ..Default::default()
    }
}
#[cfg(test)]
mod initial_child_prompt_readiness_tests {
    use super::{
        InitialChildPromptReadiness, UnpromotedChildDisposition,
        wait_initial_child_prompt_readiness,
    };
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;
    #[tokio::test]
    async fn simultaneous_readiness_and_attempt_prefers_readiness() {
        let (tx, rx) = oneshot::channel();
        tx.send(oneshot::channel().0)
            .expect("readiness already has a waiter");
        let mut attempt = Box::pin(async { "attempt" });
        let outcome = wait_initial_child_prompt_readiness(
            std::future::pending::<()>(),
            rx,
            &mut attempt,
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(matches!(outcome, InitialChildPromptReadiness::Admitted(_)));
        assert_eq!(attempt.await, "attempt");
    }
    #[tokio::test]
    async fn simultaneous_cancel_beats_readiness() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (tx, rx) = oneshot::channel();
        tx.send(oneshot::channel().0)
            .expect("readiness already has a waiter");
        let mut attempt = Box::pin(std::future::pending::<()>());
        let outcome = wait_initial_child_prompt_readiness(
            cancel.cancelled(),
            rx,
            &mut attempt,
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(matches!(outcome, InitialChildPromptReadiness::Cancelled));
    }
    #[tokio::test]
    async fn attempt_without_ack_keeps_the_real_result() {
        let (_tx, rx) = oneshot::channel::<oneshot::Sender<()>>();
        let mut attempt = Box::pin(async { 7u8 });
        let outcome = wait_initial_child_prompt_readiness(
            std::future::pending::<()>(),
            rx,
            &mut attempt,
            std::time::Duration::from_secs(1),
        )
        .await;
        assert!(matches!(
            outcome,
            InitialChildPromptReadiness::AttemptCompleted(7)
        ));
    }
    #[tokio::test]
    async fn zero_timeout_without_ready_branches_times_out() {
        let (_tx, rx) = oneshot::channel::<oneshot::Sender<()>>();
        let mut attempt = Box::pin(std::future::pending::<()>());
        let outcome = wait_initial_child_prompt_readiness(
            std::future::pending::<()>(),
            rx,
            &mut attempt,
            std::time::Duration::ZERO,
        )
        .await;
        assert!(matches!(outcome, InitialChildPromptReadiness::TimedOut));
    }
    #[test]
    fn timed_out_readiness_maps_to_admission_timed_out() {
        assert_eq!(
            InitialChildPromptReadiness::<()>::TimedOut.unpromoted_disposition(),
            UnpromotedChildDisposition::AdmissionTimedOut
        );
    }
    #[test]
    fn cancelled_and_failed_promotion_map_to_cancelled() {
        assert_eq!(
            InitialChildPromptReadiness::<()>::Cancelled.unpromoted_disposition(),
            UnpromotedChildDisposition::Cancelled
        );
        assert_eq!(
            InitialChildPromptReadiness::<()>::Admitted(oneshot::channel().0)
                .unpromoted_disposition(),
            UnpromotedChildDisposition::Cancelled
        );
        assert_eq!(
            InitialChildPromptReadiness::AttemptCompleted(()).unpromoted_disposition(),
            UnpromotedChildDisposition::Cancelled
        );
    }
}
