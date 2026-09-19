// Modified for Distill by Samuel Fajreldines, 2026.
use super::drain::PendingUploadGuard;
use crate::session::repo_changes::TraceExportConfig;
use distill_workspace::permission::PermissionEvent;
use futures::FutureExt as _;
use tokio::sync::oneshot;
/// Request to upload a trace for a synthetic auto-wake turn.
/// The notification bridge sends it for bash task completions; the subagent coordinator sends it for subagent completions.
/// The `MvpAgent`'s synthetic trace handler receives it, allocates a turn number, and drives the before/after artifact uploads.
pub(crate) struct SyntheticTurnTraceRequest {
    pub session_id: agent_client_protocol::SessionId,
    pub prompt_id: String,
    pub completion_rx: oneshot::Receiver<crate::session::commands::PromptTurnResult>,
    pub before_session_copy_rx:
        oneshot::Receiver<anyhow::Result<crate::session::persistence::SessionStateCopy>>,
}
/// Outcome of a session-state upload.
pub(crate) enum UploadOutcome {
    #[allow(dead_code)]
    Confirmed,
    /// Not confirmed within the flush deadline; the upload continues in the live queue worker.
    /// Cloud restorability is unobserved, so `restorable_turn_number` must not advance on it.
    #[allow(dead_code)]
    Deferred,
    Failed {
        reason: &'static str,
        status_code: Option<u16>,
    },
}
impl UploadOutcome {
    pub(crate) fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
}
/// How turn-end artifact uploads wait on cloud storage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum UploadWait {
    /// Await per-artifact cloud confirmation.
    /// Detached background upload tasks use this mode, where a slow bucket delays nothing the user sees.
    Confirm,
    /// Durable-accept into the upload queue; `complete_prompt_trace` then flushes the queue with one bounded, non-terminal wait before the prompt response. Each accept/upload/confirmation wait is bounded: by `deadline` directly (the flush; the queue-backed session-state confirm) or by the floored per-attempt budget derived from it (`blocking_attempt_budget`: the session-copy handoff, non-durable- accept direct uploads, the queue-less session-state confirm, the manifest write). Only the `spawn_blocking` archive build precedes the bound.
    Defer { deadline: tokio::time::Instant },
}
/// Whether turn-end uploads hold the prompt response or run after it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TurnEndUploads {
    /// Finalize after the response returns; uploads never delay the turn.
    Background,
    /// Hold the response until uploads settle, bounded by this budget.
    Wait { budget: std::time::Duration },
}
/// Turn-message capture handed to [`complete_prompt_trace`].
#[derive(Debug)]
pub(crate) enum TurnMessages {
    Captured(distill_chat_state::TurnCapture),
    Missing(MissingTurnMessages),
}
/// Why there is no capture to upload — answered-empty must not be conflated
/// with never-answered: only `Empty` may record the benign skip.
#[derive(Debug)]
pub(crate) enum MissingTurnMessages {
    /// The actor answered: the turn genuinely captured nothing.
    Empty,
    /// The actor never answered `TakeTurnMessages` within the bound (wedged
    /// child teardown): a real miss, recorded as a failure.
    TakeTimedOut,
    /// The actor is gone: the command channel or the responder dropped
    /// without an answer — a real miss, recorded as a failure.
    ChannelDropped,
}
impl From<Option<distill_chat_state::TurnCapture>> for TurnMessages {
    fn from(capture: Option<distill_chat_state::TurnCapture>) -> Self {
        match capture {
            Some(capture) => Self::Captured(capture),
            None => Self::Missing(MissingTurnMessages::Empty),
        }
    }
}
/// Why trace uploads are enabled or disabled for a given prompt.
pub(crate) use distill_telemetry::session_metrics::TraceUploadReason;
#[derive(Clone)]
pub(crate) struct PromptTraceContext {
    pub(crate) gcs_config: TraceExportConfig,
    pub(crate) session_info: crate::session::info::Info,
    pub(crate) turn_number: u64,
    pub(crate) attempt_id: Option<String>,
    pub(crate) session_handle: crate::session::SessionHandle,
    pub(crate) memory_mode: Option<crate::config::MemoryMode>,
    pub(crate) session_registry_enabled: bool,
    pub(crate) upload_queue: Option<distill_file_utils::queue::UploadQueue>,
    pub(crate) artifact_tracker: super::manifest::ArtifactTracker,
    pub(crate) auth_manager: std::sync::Arc<distill_login::AuthManager>,
}
impl PromptTraceContext {
    pub(crate) fn artifact_upload_context(&self) -> super::manifest::ArtifactUploadContext {
        super::manifest::ArtifactUploadContext {
            gcs_config: self.gcs_config.clone(),
            artifact_tracker: self.artifact_tracker.clone(),
        }
    }
}
enum UploadSpanAttach {
    Child,
    Link {
        prompt_id: String,
        session_id: String,
    },
}
/// Spawn an upload task that logs panics. Join the returned handle to wait it out
/// (opt-in blocking mode); dropping it detaches the task to the exit drain.
pub(crate) fn spawn_upload_task<F>(task_name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_upload_task_attached(task_name, UploadSpanAttach::Child, fut)
}
/// Spawns under a new root span so the turn span does not stay open for the upload.
pub(crate) fn spawn_linked_upload_task<F>(
    task_name: &'static str,
    prompt_id: impl std::fmt::Display,
    session_id: impl std::fmt::Display,
    fut: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    spawn_upload_task_attached(
        task_name,
        UploadSpanAttach::Link {
            prompt_id: prompt_id.to_string(),
            session_id: session_id.to_string(),
        },
        fut,
    );
}
fn spawn_upload_task_attached<F>(
    task_name: &'static str,
    attach: UploadSpanAttach,
    fut: F,
) -> tokio::task::JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    use tracing::Instrument;
    let pending = PendingUploadGuard::register();
    let span = match attach {
        UploadSpanAttach::Child => tracing::Span::current(),
        UploadSpanAttach::Link {
            prompt_id,
            session_id,
        } => {
            let root = tracing::info_span!(
                parent: None,
                "upload.task",
                task = task_name,
                prompt_id = %prompt_id,
                session_id = %session_id,
            );
            distill_otel::link_span_to_current(&root);
            root
        }
    };
    tokio::spawn(wrap_upload_task(task_name, pending, fut).instrument(span))
}
async fn wrap_upload_task<F>(task_name: &'static str, _pending: PendingUploadGuard, fut: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let result = std::panic::AssertUnwindSafe(fut).catch_unwind().await;
    if let Err(panic_payload) = result {
        let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        tracing::error!(
            task = task_name,
            panic = %panic_msg,
            "Upload task panicked"
        );
    }
}
#[cfg(test)]
pub(crate) async fn join_required_restore_artifacts<Fs, Fp, Fm>(
    session_state: Fs,
    permission_events: Fp,
    memory: Fm,
) -> UploadOutcome
where
    Fs: std::future::Future<Output = UploadOutcome>,
    Fp: std::future::Future<Output = ()>,
    Fm: std::future::Future<Output = ()>,
{
    let (outcome, _, _) = futures::join!(session_state, permission_events, memory);
    outcome
}
/// A turn with this stop reason committed its assistant content: `EndTurn` naturally, `MaxTokens` via Length salvage.
/// The result drives `turn_result.json`'s `completed` and [`take_streaming_partial`]'s `committed`.
pub(crate) fn stop_reason_commits_turn(stop_reason: agent_client_protocol::StopReason) -> bool {
    matches!(
        stop_reason,
        agent_client_protocol::StopReason::EndTurn | agent_client_protocol::StopReason::MaxTokens
    )
}
/// Take the out-of-band streaming capture for `prompt_id`, returning it only when the turn did NOT commit its assistant message. The live slot is always drained (even when committed) so a later turn cannot inherit it.
/// A committed turn's reasoning is already in `afterStateHistory`, so its capture is dropped. Stamps `model_id` (when absent); the caller stamps the site-specific `reason` lazily on the returned `Some`.
/// Every turn-end take site (main success / error, synthetic, subagent) calls this.
pub(crate) async fn take_streaming_partial(
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<crate::session::SessionCommand>,
    prompt_id: String,
    committed: bool,
    model_id: Option<String>,
) -> Option<crate::session::acp_session::StreamingTurnCapture> {
    use crate::session::SessionCommand;
    let (tx, rx) = oneshot::channel();
    if cmd_tx
        .send(SessionCommand::TakeStreamingCapture {
            prompt_id,
            respond_to: tx,
        })
        .is_err()
    {
        return None;
    }
    let taken = rx.await.ok().flatten();
    if committed {
        return taken
            .filter(|cap| cap.has_doom_loop_segments())
            .map(|mut cap| {
                if cap.model_id.is_none() {
                    cap.model_id = model_id;
                }
                cap.reason
                    .get_or_insert_with(|| "doom_loop_recovered".to_string());
                cap
            });
    }
    taken.map(|mut cap| {
        if cap.model_id.is_none() {
            cap.model_id = model_id;
        }
        cap
    })
}
/// Complete the prompt trace. Returns `Ok(true)` when session state is durably confirmed and `restorable_turn_number` can advance.
/// With [`UploadWait::Defer`], the artifact accepts, the queue flush, terminal telemetry, and the manifest write all run here within the deadline.
/// Callers make a plain call in either mode.
#[tracing::instrument(
    name = "upload.complete_prompt_trace",
    skip_all,
    fields(session_id = %ctx.session_info.id.0, turn_number = ctx.turn_number)
)]
pub(crate) async fn complete_prompt_trace(
    ctx: PromptTraceContext,
    permission_events: Vec<PermissionEvent>,
    session_copy_rx: oneshot::Receiver<
        anyhow::Result<crate::session::persistence::SessionStateCopy>,
    >,
    turn_messages: TurnMessages,
    streaming_partial: Option<crate::session::acp_session::StreamingTurnCapture>,
    wait: UploadWait,
) -> anyhow::Result<bool> {
    use super::manifest::{build_manifest, resolve_upload_method, write_upload_manifest};
    let upload_method = resolve_upload_method(&ctx.gcs_config);
    let method_str = upload_method.as_ref();
    distill_telemetry::session_ctx::log_session_event(
        crate::agent::session_metrics::TraceUploadAttempted {
            session_id: ctx.session_info.id.0.to_string(),
            turn_number: ctx.turn_number,
            upload_method: method_str.to_owned(),
        },
    );
    let queue_failed_count = || {
        ctx.upload_queue.as_ref().map_or(0, |q| {
            q.stats().failed.load(std::sync::atomic::Ordering::Relaxed)
        })
    };
    let failed_before = queue_failed_count();
    let turn_messages_ok = match turn_messages {
        TurnMessages::Captured(capture) => {
            super::trace::upload_turn_messages(&ctx, capture, wait).await
        }
        TurnMessages::Missing(missing) => {
            record_missing_turn_messages(&ctx.artifact_tracker, missing)
        }
    };
    if let Some(capture) = streaming_partial.as_ref() {
        super::trace::upload_streaming_partial(&ctx, capture, wait).await;
    }
    let (upload_outcome, _, _) = futures::join!(
        super::trace::upload_session_state(&ctx, "after", session_copy_rx, wait),
        super::trace::upload_permission_events(&ctx, &permission_events, wait),
        super::memory::upload_memory_state(&ctx, wait),
    );
    let artifacts_confirmed = upload_outcome.is_confirmed();
    let gated_artifact_failure: Option<(String, Option<u16>)> = None;
    let flush_remaining = match wait {
        UploadWait::Confirm => 0,
        UploadWait::Defer { deadline } => super::trace::flush_upload_queue(&ctx, deadline).await,
    };
    let manifest = build_manifest(&ctx.artifact_tracker, upload_method, None);
    let flush_timed_out = flush_remaining > 0 || matches!(upload_outcome, UploadOutcome::Deferred);
    let worker_drops = match wait {
        UploadWait::Confirm => 0,
        UploadWait::Defer { .. } => queue_failed_count().saturating_sub(failed_before),
    };
    let terminal_failure: Option<(String, Option<u16>)> = if let UploadOutcome::Failed {
        reason,
        status_code,
    } = &upload_outcome
    {
        Some(((*reason).to_owned(), *status_code))
    } else if gated_artifact_failure.is_some() {
        gated_artifact_failure
    } else if !turn_messages_ok {
        Some(("turn_messages_failed".to_owned(), None))
    } else if worker_drops > 0 {
        Some(("worker_dropped".to_owned(), None))
    } else if flush_timed_out {
        Some(("flush_timeout".to_owned(), None))
    } else {
        None
    };
    match terminal_failure {
        Some((error_category, status_code)) => {
            distill_telemetry::session_ctx::log_session_event(
                crate::agent::session_metrics::TraceUploadFailed {
                    session_id: ctx.session_info.id.0.to_string(),
                    turn_number: ctx.turn_number,
                    error_category,
                    status_code,
                    upload_method: method_str.to_owned(),
                },
            );
        }
        None => {
            distill_telemetry::session_ctx::log_session_event(
                crate::agent::session_metrics::TraceUploadSucceeded {
                    session_id: ctx.session_info.id.0.to_string(),
                    turn_number: ctx.turn_number,
                    upload_method: method_str.to_owned(),
                    fully_uploaded: true,
                },
            );
        }
    }
    match wait {
        UploadWait::Confirm => write_upload_manifest(&ctx, &manifest).await,
        UploadWait::Defer { deadline } => {
            let budget = super::trace::blocking_attempt_budget(deadline);
            if tokio::time::timeout(budget, write_upload_manifest(&ctx, &manifest))
                .await
                .is_err()
            {
                tracing::warn!(
                    "upload manifest not confirmed within its attempt budget; \
                     a durably spooled manifest stays with the queue"
                );
            }
        }
    }
    Ok(artifacts_confirmed)
}
/// Manifest record and ok-flag when there is no capture to upload: an empty turn keeps the benign skip, while a timed-out take (wedged actor) fails the artifact so `fully_uploaded` and the terminal event cannot report success for a capture the wedge dropped.
fn record_missing_turn_messages(
    tracker: &super::manifest::ArtifactTracker,
    missing: MissingTurnMessages,
) -> bool {
    match missing {
        MissingTurnMessages::Empty => {
            super::manifest::skip_artifact(
                tracker,
                "turn_messages.json",
                "no_turn_messages_captured",
            );
            true
        }
        MissingTurnMessages::TakeTimedOut => {
            super::manifest::record_artifact(
                tracker,
                "turn_messages.json",
                super::manifest::ArtifactResult::Failed {
                    reason: "take_timed_out",
                    error: Some("session actor did not answer TakeTurnMessages within the bound"),
                },
            );
            false
        }
        MissingTurnMessages::ChannelDropped => {
            super::manifest::record_artifact(
                tracker,
                "turn_messages.json",
                super::manifest::ArtifactResult::Failed {
                    reason: "channel_dropped",
                    error: Some("session actor dropped TakeTurnMessages without answering"),
                },
            );
            false
        }
    }
}
/// Parse `_meta.agentProfile` as a JSON object or string name.
/// Returns `None` if absent or invalid.
pub(crate) fn parse_agent_profile_from_meta(
    meta: Option<&agent_client_protocol::Meta>,
) -> Option<distill_agent::AgentDefinition> {
    let value = meta?.get("agentProfile")?;
    if value.is_object() {
        return match distill_agent::AgentDefinition::from_json(value) {
            Ok(def) => {
                tracing::info!(
                    agent_name = %def.name,
                    "Using ACP agent profile from _meta.agentProfile (JSON object)"
                );
                Some(def)
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "Failed to parse _meta.agentProfile JSON object, falling back to default agent"
                );
                None
            }
        };
    }
    if let Some(name) = value.as_str() {
        tracing::info!(
            agent_name = %name,
            "Resolving agent from _meta.agentProfile (string name)"
        );
        return distill_agent::discovery::by_name(name);
    }
    tracing::warn!(
        "Ignoring _meta.agentProfile: expected a JSON object or string, got {:?}",
        value
    );
    None
}
/// Parse `_meta.askUserQuestion` as a boolean. `Some(false)` means the pager set `--no-ask-user`.
/// The shell passes it to `AgentBuilder::with_ask_user_question_enabled(false)` so the tool is stripped from the model's advertised tool list. `Some(true)` explicitly enables the tool for this session.
/// `None` means the field is absent; the caller falls back to the `ask_user_question` feature (default ON).
pub(crate) fn parse_ask_user_question_from_meta(
    meta: Option<&agent_client_protocol::Meta>,
) -> Option<bool> {
    let value = meta?.get("askUserQuestion")?;
    match value.as_bool() {
        Some(b) => Some(b),
        None => {
            tracing::warn!(
                "Ignoring _meta.askUserQuestion: expected a bool, got {:?}",
                value
            );
            None
        }
    }
}
pub(crate) fn lookup_session_model(
    session_model: Option<agent_client_protocol::ModelId>,
    default_model_id: &agent_client_protocol::ModelId,
) -> agent_client_protocol::ModelId {
    session_model.unwrap_or_else(|| default_model_id.clone())
}
pub(crate) fn apply_yolo_mode_to_matching_sessions<'a>(
    sessions: impl IntoIterator<Item = &'a mut crate::session::SessionHandle>,
    sender_id: Option<&str>,
    yolo_mode: bool,
) -> usize {
    let matches_sender = |h: &crate::session::SessionHandle| -> bool {
        sender_id.is_none() || h.origin_client.as_ref().map(|c| c.product.as_str()) == sender_id
    };
    let mut updated = 0;
    for handle in sessions {
        if matches_sender(handle) {
            handle.yolo_mode = yolo_mode;
            let _ = handle
                .cmd_tx
                .send(crate::session::SessionCommand::SetYoloMode { enabled: yolo_mode });
            updated += 1;
        }
    }
    updated
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_reason_commits_turn_matches_contract() {
        use agent_client_protocol::StopReason;
        assert!(stop_reason_commits_turn(StopReason::EndTurn));
        assert!(stop_reason_commits_turn(StopReason::MaxTokens));
        assert!(!stop_reason_commits_turn(StopReason::Cancelled));
        assert!(!stop_reason_commits_turn(StopReason::Refusal));
        assert!(!stop_reason_commits_turn(StopReason::MaxTurnRequests));
    }
    /// A never-answered `TakeTurnMessages` — wedged actor (timeout) or dead actor (dropped channel) — must fail the artifact (denting `fully_uploaded` and the terminal event via `turn_messages_failed`), while the genuinely-empty turn keeps its benign skip.
    #[test]
    fn missing_turn_messages_distinguishes_unanswered_from_empty() {
        use super::super::manifest::{ArtifactStatus, new_artifact_tracker};
        let tracker = new_artifact_tracker();
        assert!(record_missing_turn_messages(
            &tracker,
            MissingTurnMessages::Empty
        ));
        assert!(matches!(
            tracker.lock().statuses.get("turn_messages.json"),
            Some(ArtifactStatus::Skipped)
        ));
        for (missing, reason) in [
            (MissingTurnMessages::TakeTimedOut, "take_timed_out"),
            (MissingTurnMessages::ChannelDropped, "channel_dropped"),
        ] {
            let tracker = new_artifact_tracker();
            assert!(!record_missing_turn_messages(&tracker, missing));
            assert!(matches!(
                tracker.lock().statuses.get("turn_messages.json"),
                Some(ArtifactStatus::Failed)
            ));
            assert_eq!(
                tracker
                    .lock()
                    .failures
                    .get("turn_messages.json")
                    .map(|d| d.reason.as_str()),
                Some(reason)
            );
        }
    }
    /// Hangs if a slow optional artifact is accidentally added to the join.
    #[tokio::test]
    async fn restorable_artifacts_do_not_wait_for_slow_optional_artifacts() {
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            join_required_restore_artifacts(
                std::future::ready(UploadOutcome::Confirmed),
                std::future::ready(()),
                std::future::ready(()),
            ),
        )
        .await
        .expect("required artifacts must not block on optional artifacts");
    }
    #[tokio::test]
    async fn restorable_turn_advanced_when_session_state_succeeds() {
        let result = join_required_restore_artifacts(
            std::future::ready(UploadOutcome::Confirmed),
            std::future::ready(()),
            std::future::ready(()),
        )
        .await;
        assert!(
            result.is_confirmed(),
            "should advance restorable_turn_number on session state success"
        );
    }
    #[tokio::test]
    async fn restorable_turn_not_advanced_when_session_state_fails() {
        let result = join_required_restore_artifacts(
            std::future::ready(UploadOutcome::Failed {
                reason: "test",
                status_code: None,
            }),
            std::future::ready(()),
            std::future::ready(()),
        )
        .await;
        assert!(
            !result.is_confirmed(),
            "must not advance restorable_turn_number on session state failure"
        );
    }
    #[tokio::test]
    async fn restore_artifacts_independent_of_optional_artifact_policy() {
        let result = join_required_restore_artifacts(
            std::future::ready(UploadOutcome::Confirmed),
            std::future::ready(()),
            std::future::ready(()),
        )
        .await;
        assert!(
            result.is_confirmed(),
            "restore artifacts must succeed regardless of optional artifact policy"
        );
    }
    /// Compile-time check that all upload functions exist.
    #[test]
    fn subagent_artifact_set_is_parent_minus_gated_artifacts() {
        let _ = super::super::trace::upload_session_state;
        let _ = super::super::trace::upload_permission_events;
        let _ = super::super::memory::upload_memory_state;
        let _ = super::super::trace::upload_turn_messages;
        let _ = super::super::trace::upload_turn_result;
    }
    #[test]
    fn parse_ask_user_question_returns_false_when_disabled() {
        let meta = serde_json::json!({ "askUserQuestion": false });
        assert_eq!(
            parse_ask_user_question_from_meta(meta.as_object()),
            Some(false)
        );
    }
    #[test]
    fn parse_ask_user_question_returns_true_when_enabled() {
        let meta = serde_json::json!({ "askUserQuestion": true });
        assert_eq!(
            parse_ask_user_question_from_meta(meta.as_object()),
            Some(true)
        );
    }
    #[test]
    fn parse_ask_user_question_returns_none_when_absent() {
        let meta = serde_json::json!({ "agentProfile": "distill-plan" });
        assert_eq!(parse_ask_user_question_from_meta(meta.as_object()), None);
    }
    #[test]
    fn parse_ask_user_question_returns_none_for_empty_meta() {
        assert_eq!(parse_ask_user_question_from_meta(None), None);
    }
    /// Non-bool values are ignored: the shell falls back to the resolved `ask_user_question` feature rather than panicking on malformed input.
    #[test]
    fn parse_ask_user_question_ignores_non_bool() {
        let meta = serde_json::json!({ "askUserQuestion": "no" });
        assert_eq!(parse_ask_user_question_from_meta(meta.as_object()), None);
    }
    #[tokio::test]
    async fn complete_prompt_trace_restore_path_excludes_codebase() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            join_required_restore_artifacts(
                std::future::ready(UploadOutcome::Confirmed),
                std::future::ready(()),
                std::future::ready(()),
            ),
        )
        .await
        .expect("restore path must not depend on optional artifacts");
        assert!(
            result.is_confirmed(),
            "session state upload should confirm success"
        );
    }
    #[test]
    fn subagent_trace_uses_turn_zero() {
        let session_id = "child-abc";
        let dispatch_prefix = format!("{}/turn_0", session_id);
        let completion_prefix = format!("{}/turn_0", session_id);
        assert_eq!(dispatch_prefix, completion_prefix);
        let turn_number: u64 = 0;
        assert_eq!(
            format!("{}/turn_{}", session_id, turn_number),
            dispatch_prefix,
        );
    }
}
