// Modified for Distill by Samuel Fajreldines, 2026.
//! This manager coordinates:
//! - Signal tracking via SessionSignalsHandle
//! - Heuristics evaluation to determine when to request feedback
//! - Periodic sync of signals to the feedback/analytics backend
//! - Background loading of feedback configuration from the backend
//! - Creating feedback request records when triggered
//! - Sending feedback request notifications to clients
//!
//! ## Usage
//! ```ignore
//! // Create the manager when a session starts
//! let manager = FeedbackManager::new(session_id, feedback_api_url, user_token);
//!
//! // Get the signals handle to pass around for event tracking
//! let signals = manager.signals_handle();
//!
//! // Spawn the background sync task (also loads config)
//! tokio::spawn(manager.run_sync_loop());
//!
//! // Track events
//! signals.increment_turn();
//! signals.record_tool_call("read_file");
//!
//! // Check for feedback after each turn
//! // This also records the request with the feedback API if triggered
//! if let Some(request) = manager.maybe_request_feedback(None).await {
//!     // Send FeedbackRequest notification to client
//! }
//! ```

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::agent::feedback_client::{
    FeedbackApiError, FeedbackClient, signals_to_update, snapshot_to_turn_delta,
};
use crate::session::feedback::{
    FeedbackHeuristics, FeedbackRequest, FeedbackTier, TriggerCondition,
};
use crate::session::signals::{SessionSignalsActor, SessionSignalsHandle, TurnDeltaSnapshot};

use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, ContextType, CreateFeedbackRequestInput, FeedbackContent, FeedbackMode,
    FeedbackSubmission, FeedbackToolOutcome, SessionTurnDelta, TurnOutcome,
};

use crate::session::persistence::{LocalFeedbackEntry, PersistenceMsg, UserFeedbackEntry};
use distill_feedback::{parse_structured_feedback, wire_value};
use distill_telemetry::events::{FeedbackSendOutcome, UserFeedback};

pub(crate) enum SubmitOutcome {
    Submitted,
    /// No server configured for this session.
    LocalOnly,
    /// Server request failed.
    Failed(anyhow::Error),
}

/// The `user_feedback` event for one workflow call. `source` and the taxonomy ride only as the
/// frozen wire values of the parsed enums, never the client's raw strings.
fn user_feedback_event(
    submission: &FeedbackSubmission,
    outcome: &SubmitOutcome,
    solicited: bool,
) -> UserFeedback {
    let structured = parse_structured_feedback(submission.metadata.as_ref());
    let taxonomy = structured.map(|s| s.taxonomy).unwrap_or_default();
    UserFeedback {
        session_id: submission.session_id.clone(),
        has_feedback_text: submission
            .feedback_text
            .as_ref()
            .is_some_and(|text| !text.is_empty()),
        model_id: submission.model_id.clone(),
        rating_value: submission.rating_value,
        is_solicited: solicited,
        outcome: match outcome {
            SubmitOutcome::Submitted => FeedbackSendOutcome::Submitted,
            SubmitOutcome::LocalOnly => FeedbackSendOutcome::LocalOnly,
            SubmitOutcome::Failed(_) => FeedbackSendOutcome::Failed,
        },
        source: structured.and_then(|s| s.source).and_then(wire_value),
        feedback_type: taxonomy.r#type.and_then(wire_value),
        task_category: taxonomy.task_category.and_then(wire_value),
        failure_mode: taxonomy.failure_mode.and_then(wire_value),
    }
}

pub(crate) fn new_submission(
    session_id: String,
    client_type: ClientType,
    content: FeedbackContent,
) -> FeedbackSubmission {
    let mut s = FeedbackSubmission::with_content(session_id, client_type, content);
    s.shell_version = Some(distill_version::VERSION.to_string());
    s
}

#[derive(Debug)]
pub(crate) struct SubmitFeedbackOptions {
    pub solicited: bool,
    pub telemetry_enabled: bool,
    pub author_identity: Option<crate::util::user_identity::ResolvedUserIdentity>,
}

pub(crate) async fn submit_feedback_workflow(
    submission: &mut FeedbackSubmission,
    feedback_client: Option<&FeedbackClient>,
    persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
    opts: SubmitFeedbackOptions,
) -> SubmitOutcome {
    let SubmitFeedbackOptions {
        solicited,
        telemetry_enabled,
        author_identity,
    } = opts;

    if let Some(mut user_meta) = crate::util::parse_json_object_env("GROK_USER_METADATA") {
        // `structured_feedback` is reserved for the client's typed envelope. The shallow merge
        // is later-wins, so an env copy would silently replace the client's enums (or invent
        // the key on reports that carry none); every other env key keeps later-wins.
        if let Some(user_meta) = user_meta.as_object_mut() {
            user_meta.remove("structured_feedback");
        }
        if user_meta.as_object().is_some_and(|meta| !meta.is_empty()) {
            submission.merge_metadata(user_meta);
        }
    }
    // Exhaustive destructure (no `..`) so a new field must be handled, not dropped.
    if let Some(crate::util::user_identity::ResolvedUserIdentity { name, email }) = author_identity
    {
        if let Some(name) = name {
            submission.author_name = Some(name);
        }
        if let Some(email) = email {
            submission.author_email = Some(email);
        }
    }

    if let Some(tx) = persistence_tx {
        // Persist the image inventory, never the payloads feedback.jsonl rides in trace archives with a hard per-file size cap (one screenshot's base64 would sink the whole record).
        // The bytes are also cleartext terminal captures.
        // Take/restore around the clone so the megabytes are never copied either.
        let images = std::mem::take(&mut submission.images);
        let mut persisted = submission.clone();
        persisted.images = images
            .iter()
            .map(
                |i| prod_mc_cli_chat_proxy_types::feedback_types::FeedbackImage {
                    data: format!("<{} base64 bytes stripped>", i.data.len()),
                    mime_type: i.mime_type.clone(),
                    file_name: i.file_name.clone(),
                },
            )
            .collect();
        submission.images = images;
        let entry = LocalFeedbackEntry::UserFeedback(UserFeedbackEntry {
            submitted_at: chrono::Utc::now(),
            session_id: submission.session_id.clone(),
            turn_number: submission.turn_number,
            solicited,
            request_id: submission.request_id.clone(),
            dismissed: false,
            submission: Some(persisted),
        });
        if tx.send(PersistenceMsg::Feedback(entry)).is_err() {
            tracing::warn!(
                session_id = %submission.session_id,
                "feedback persistence channel closed; entry dropped",
            );
        }
    }

    let request_id = submission.request_id.clone();
    let appearance_id = request_id.clone();

    // Send the full submission: the feedback backend shows these triage fields, so session context and metadata are intentionally not stripped here

    let outcome = if let Some(client) = feedback_client {
        let result = if let Some(req_id) = request_id {
            with_one_shot_auth_retry(client, || async {
                client
                    .complete_request(&req_id, submission)
                    .await
                    .map(|_| ())
            })
            .await
        } else {
            with_one_shot_auth_retry(client, || async {
                client.submit_feedback(submission).await.map(|_| ())
            })
            .await
        };
        match result {
            Ok(()) => SubmitOutcome::Submitted,
            Err(error) => {
                tracing::warn!(%error, "feedback submission failed");
                SubmitOutcome::Failed(error)
            }
        }
    } else {
        SubmitOutcome::LocalOnly
    };

    if telemetry_enabled {
        let event = user_feedback_event(submission, &outcome, solicited);
        let feedback_span = tracing::info_span!(
            "feedback.survey",
            survey_type = "session",
            event_type = "responded",
            appearance_id = %appearance_id.as_deref().unwrap_or(""),
            has_feedback_text = event.has_feedback_text,
            rating = tracing::field::Empty,
            is_solicited = solicited,
        );
        // Record `rating` only for star ratings; text-only feedback has no rating and must not export a fake 0
        if let Some(rating) = event.rating_value {
            feedback_span.record("rating", rating);
        }
        feedback_span.in_scope(|| {});
        distill_telemetry::session_ctx::log_event(event);
    }

    outcome
}

/// Chat-state fields the session actor passes to [`FeedbackManager::submit_text_feedback`].
pub(crate) struct SessionFeedbackData {
    pub model_id: Option<String>,
    pub resolved_model_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub client_version: Option<String>,
    pub session_cwd: String,
}

/// Feedback feature flags threaded through session spawn.
#[derive(Debug, Clone, Default)]
pub(crate) struct FeedbackFlags {
    pub enabled: bool,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

/// Two concerns are gated by separate flags (`feedback_enabled`, `telemetry_enabled`).
/// Both default to `false`.
#[derive(Debug, Clone)]
pub struct FeedbackManagerConfig {
    /// Interval for syncing signals to the analytics backend (default: 30s)
    pub sync_interval: Duration,
    /// Whether user-facing feedback features are enabled (popups, `/feedback`, ratings).
    /// Gated by `GROK_FEEDBACK_ENABLED`.
    pub feedback_enabled: bool,
    /// Whether session analytics (signal sync, turn deltas) are enabled.
    /// Gated by `GROK_TELEMETRY_ENABLED`.
    /// These are analytics data that flow continuously without user action.
    pub telemetry_enabled: bool,
    pub client_type: ClientType,
    /// Whether LOC attribution tracking is enabled for this session.
    /// Propagated into every `SessionTurnDelta`.
    /// The server can then distinguish "tracking off" (zeros are noise) from "tracking on, no code changed" (zeros are real data).
    pub loc_tracking_enabled: bool,
    /// Preferred timeout for draining the upload queue on shutdown (default: 30s).
    /// Process exit clamps this under [`SHUTDOWN_DRAIN_CAP`] (or `GROK_SESSION_EXIT_DRAIN_SECS`) so a hung upload cannot exceed the agent join grace.
    /// Abandoned durable pairs are recovered on next-session startup.
    pub drain_timeout: Duration,
    pub user: Option<crate::agent::config::FeedbackUserConfig>,
}

impl Default for FeedbackManagerConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(60),
            feedback_enabled: false,
            telemetry_enabled: false,
            client_type: ClientType::Agent,
            loc_tracking_enabled: false,
            drain_timeout: Duration::from_secs(30),
            user: None,
        }
    }
}

/// Manages feedback collection for a single session.
pub struct FeedbackManager {
    session_id: String,
    /// Handle for sending signals (cheap to clone)
    signals_handle: SessionSignalsHandle,
    heuristics: Arc<RwLock<FeedbackHeuristics>>,
    /// REST client for the feedback/analytics backend
    feedback_client: Option<FeedbackClient>,
    config: FeedbackManagerConfig,
    config_loaded: Arc<AtomicBool>,
    /// GCS upload queue stats for periodic snapshots into signals.
    /// Set once after the first upload queue is created via `set_upload_queue_stats()`.
    /// `OnceLock` because `FeedbackManager` is behind `Arc` and this is set after construction.
    upload_queue_stats: std::sync::OnceLock<Arc<distill_file_utils::queue::UploadQueueStats>>,
    /// Turn deltas post from one worker in send order, so the backend's append-only rows arrive
    /// in turn order and [`Self::shutdown`] can wait for an exit cancel's row. `None` without a client.
    turn_delta_tx: Option<mpsc::Sender<TurnDeltaJob>>,
}

/// Queued for the turn-delta worker. Bounded at [`TURN_DELTA_QUEUE_CAPACITY`]: a stalled backend
/// drops the newest deltas (logged) instead of growing without limit.
enum TurnDeltaJob {
    Post(Box<SessionTurnDelta>),
    /// Answered once every job queued before it has posted.
    Barrier(oneshot::Sender<()>),
}

/// One delta per turn, so this only fills when the backend is unreachable for many turns.
const TURN_DELTA_QUEUE_CAPACITY: usize = 64;

/// The shared HTTP client bounds only connection setup; without this a stalled response would
/// hold every later row behind it.
const TURN_DELTA_POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Post queued turn deltas in order until every sender is dropped.
fn spawn_turn_delta_worker(
    session_id: String,
    client: FeedbackClient,
) -> mpsc::Sender<TurnDeltaJob> {
    let (tx, mut rx) = mpsc::channel(TURN_DELTA_QUEUE_CAPACITY);
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            match job {
                TurnDeltaJob::Post(delta) => {
                    let result = tokio::time::timeout(
                        TURN_DELTA_POST_TIMEOUT,
                        with_one_shot_auth_retry(&client, || {
                            client.send_turn_delta(&session_id, &delta)
                        }),
                    )
                    .await;
                    match result {
                        Ok(Ok(_)) => tracing::debug!(
                            session_id = %session_id,
                            turn = delta.turn_number,
                            "Turn delta sent to analytics backend"
                        ),
                        Ok(Err(e)) => tracing::warn!(
                            session_id = %session_id,
                            turn = delta.turn_number,
                            error = %e,
                            "Failed to send turn delta (non-fatal)"
                        ),
                        Err(_) => tracing::warn!(
                            session_id = %session_id,
                            turn = delta.turn_number,
                            timeout_secs = TURN_DELTA_POST_TIMEOUT.as_secs(),
                            "Turn delta post timed out (non-fatal)"
                        ),
                    }
                }
                TurnDeltaJob::Barrier(reached) => {
                    let _ = reached.send(());
                }
            }
        }
    });
    tx
}

impl FeedbackManager {
    /// If `feedback_client` is None, signal syncing is disabled but local tracking and heuristics evaluation still work.
    pub fn new(
        session_id: impl Into<String>,
        feedback_client: Option<FeedbackClient>,
        config: FeedbackManagerConfig,
    ) -> Self {
        let (signals_handle, actor) = SessionSignalsActor::with_sync_interval(config.sync_interval);

        tokio::spawn(actor.run());

        let session_id = session_id.into();
        let feedback_client = feedback_client.map(|c| c.with_session_id(session_id.clone()));
        let turn_delta_tx = feedback_client
            .as_ref()
            .map(|client| spawn_turn_delta_worker(session_id.clone(), client.clone()));
        tracing::info!(
            session_id = %session_id,
            feedback_enabled = config.feedback_enabled,
            telemetry_enabled = config.telemetry_enabled,
            has_client = feedback_client.is_some(),
            "FeedbackManager initialized"
        );

        Self {
            session_id,
            signals_handle,
            heuristics: Arc::new(RwLock::new(FeedbackHeuristics::new())),
            feedback_client,
            config,
            config_loaded: Arc::new(AtomicBool::new(false)),
            upload_queue_stats: std::sync::OnceLock::new(),
            turn_delta_tx,
        }
    }

    pub fn local_only(session_id: impl Into<String>) -> Self {
        Self::new(session_id, None, FeedbackManagerConfig::default())
    }

    pub(crate) fn set_upload_queue_stats(
        &self,
        stats: Arc<distill_file_utils::queue::UploadQueueStats>,
    ) {
        let _ = self.upload_queue_stats.set(stats);
    }

    pub fn signals_handle(&self) -> SessionSignalsHandle {
        self.signals_handle.clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn is_enabled(&self) -> bool {
        self.config.feedback_enabled
    }

    pub fn feedback_client(&self) -> Option<&FeedbackClient> {
        self.feedback_client.as_ref()
    }

    pub fn client_type(&self) -> prod_mc_cli_chat_proxy_types::feedback_types::ClientType {
        self.config.client_type
    }

    /// Build and submit text feedback from the `/feedback` slash command.
    pub(crate) async fn submit_text_feedback(
        &self,
        text: String,
        session_data: SessionFeedbackData,
        persistence_tx: Option<&tokio::sync::mpsc::UnboundedSender<PersistenceMsg>>,
        telemetry_enabled: bool,
    ) -> SubmitOutcome {
        let sh = self.signals_handle();
        let (signals, tool_outcomes) = tokio::join!(sh.snapshot(), sh.last_turn_tool_outcomes());
        let signals = signals.unwrap_or_default();
        let turn_number = signals.turn_count.saturating_sub(1) as i64;
        let tool_outcomes: Vec<FeedbackToolOutcome> = tool_outcomes
            .into_iter()
            .map(|o| FeedbackToolOutcome {
                tool_name: o.tool_name,
                calls: o.successes + o.failures,
                failures: o.failures,
            })
            .collect();

        let mut submission = new_submission(
            self.session_id.clone(),
            self.config.client_type,
            FeedbackContent::Text(text),
        );
        submission.turn_number = Some(turn_number);
        submission.model_id = session_data.model_id;
        submission.resolved_model_id = session_data.resolved_model_id;
        submission.reasoning_effort = session_data
            .reasoning_effort
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        submission.last_user_message = None;
        submission.last_assistant_message = None;
        submission.tool_outcomes = tool_outcomes;
        submission.session_cwd = Some(session_data.session_cwd);
        submission.compaction_count = Some(signals.compaction_count as i64);
        submission.context_window_usage = Some(signals.context_window_usage);
        submission.context_tokens_used = Some(signals.context_tokens_used);
        submission.context_window_tokens = Some(signals.context_window_tokens);
        submission.client_version = session_data.client_version;

        let author_identity =
            crate::util::user_identity::cached_identity(self.config.user.as_ref()).await;

        submit_feedback_workflow(
            &mut submission,
            self.feedback_client.as_ref(),
            persistence_tx,
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled,
                author_identity,
            },
        )
        .await
    }

    /// Load feedback heuristics config from the backend.
    /// This is called automatically in run_sync_loop but can be called manually.
    /// Does not block: errors are logged and defaults are used.
    #[tracing::instrument(name = "feedback.load_config", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn load_config(&self) {
        let Some(client) = &self.feedback_client else {
            return; // No client, use defaults
        };

        if self.config.feedback_enabled {
            match client.get_feedback_config().await {
                Ok(config) => {
                    let mut heuristics = self.heuristics.write().await;
                    heuristics.update_config(&config);
                    self.config_loaded.store(true, Ordering::Relaxed);
                    tracing::info!(
                        session_id = %self.session_id,
                        config_id = %config.config_id,
                        config_version = config.config_version,
                        enabled = config.enabled,
                        "Loaded feedback heuristics config from server"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to load feedback heuristics config, using defaults"
                    );
                }
            }
        }
    }

    /// Evaluate heuristics and return a FeedbackRequest if one should be sent.
    /// Call this after each turn to check if feedback should be requested.
    /// The tier was already triggered this session.
    #[tracing::instrument(name = "feedback.maybe_request_feedback", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub async fn maybe_request_feedback(
        &self,
        prompt_id: Option<String>,
    ) -> Option<FeedbackRequest> {
        if !self.config.feedback_enabled {
            return None;
        }

        let signals = self.signals_handle.snapshot().await?;
        let mut heuristics = self.heuristics.write().await;

        // Check if heuristics are globally enabled (from server config)
        if !heuristics.is_enabled() {
            return None;
        }

        let eval = heuristics.evaluate(&signals);

        if let (true, Some(trigger_condition)) =
            (eval.should_request, eval.trigger_condition.as_ref())
        {
            let tier = trigger_condition.tier;
            let feedback_mode = heuristics.feedback_mode(tier);
            let dismissible = heuristics.dismissible(tier);
            let prompt = heuristics.prompt(tier);
            let request = FeedbackRequest::with_mode(
                self.session_id.clone(),
                trigger_condition.clone(),
                feedback_mode,
                dismissible,
                Some(prompt),
            );
            tracing::info!(
                session_id = %self.session_id,
                tier = ?request.tier,
                trigger_type = %request.trigger_type,
                feedback_mode = ?request.feedback_mode,
                "Feedback request triggered"
            );

            self.record_feedback_request(&request, trigger_condition, feedback_mode, prompt_id)
                .await;

            return Some(request);
        }

        None
    }

    /// Engineers developing clients can call this via the `x.ai/debug/trigger_feedback` ACP extension method.
    /// When a `feedback_client` is configured, the request is also recorded via the feedback API, exactly like a real trigger.
    /// The subsequent `complete_request` / `dismiss_request` round-trip from the client then works end-to-end.
    #[tracing::instrument(name = "feedback.force_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    pub(crate) async fn force_feedback_request(
        &self,
        tier: FeedbackTier,
        mode: FeedbackMode,
    ) -> FeedbackRequest {
        use crate::session::feedback::TriggerSignalSnapshot;

        let condition = TriggerCondition {
            tier,
            condition: "debug/trigger_feedback (manual test trigger)".to_string(),
            signal_snapshot: TriggerSignalSnapshot {
                turn_count: 0,
                tool_calls_count: 0,
                compactions_count: 0,
                errors_count: 0,
                cancellations_count: 0,
                has_reverted: false,
            },
        };

        // Manual/debug triggers are always dismissible regardless of tier config
        let request = FeedbackRequest::with_mode(
            self.session_id.clone(),
            condition.clone(),
            mode,
            true,
            None,
        );

        self.record_feedback_request(&request, &condition, mode, None)
            .await;

        request
    }

    /// This is a best-effort operation: errors are logged but do not prevent the request from being sent to the client.
    #[tracing::instrument(name = "feedback.record_feedback_request", skip_all, fields(
        session_id = %self.session_id,
    ))]
    async fn record_feedback_request(
        &self,
        request: &FeedbackRequest,
        trigger_condition: &TriggerCondition,
        feedback_mode: FeedbackMode,
        prompt_id: Option<String>,
    ) {
        let Some(client) = &self.feedback_client else {
            return;
        };

        let input = CreateFeedbackRequestInput {
            request_id: request.request_id.clone(),
            session_id: self.session_id.clone(),
            client_type: self.config.client_type,
            feedback_mode,
            feedback_prompt: Some(request.prompt.clone()),
            priority: tier_to_priority(trigger_condition.tier),
            trigger_type: request.trigger_type.clone(),
            trigger_reason: Some(trigger_condition.trigger_reason()),
            context_type: Some(ContextType::Session),
            context_message_ids: vec![],
            expires_at: None,
            experiment_id: None,
            trigger_condition: serde_json::to_value(trigger_condition).ok(),
            prompt_id,
        };

        match with_one_shot_auth_retry(client, || client.create_feedback_request(&input)).await {
            Ok(response) => {
                tracing::debug!(
                    request_id = %response.request_id,
                    "Feedback request recorded with feedback API"
                );
            }
            Err(e) => {
                tracing::warn!(
                    request_id = %request.request_id,
                    error = %e,
                    "Failed to record feedback request (continuing anyway)"
                );
            }
        }
    }

    /// The signals actor accumulates tool calls, errors, and latency continuously, so the snapshot captures the full diff since the previous turn's.
    /// The caller provides the `TurnDeltaSnapshot` it took (each take advances the delta baseline), so this never advances it again.
    /// The POST runs on the turn-delta worker; errors are logged and never block the turn flow.
    #[tracing::instrument(skip_all, fields(session_id = %self.session_id))]
    pub(crate) async fn send_turn_delta_with_snapshot(
        &self,
        snapshot: Option<&TurnDeltaSnapshot>,
        request_id: Option<String>,
        turn_duration_ms: Option<i64>,
        turn_outcome: TurnOutcome,
    ) {
        if !self.config.telemetry_enabled {
            tracing::debug!("Turn delta skipped: telemetry is disabled");
            return;
        }

        tracing::debug!("Turn delta: sending pre-captured snapshot");

        let Some(turn_delta_tx) = &self.turn_delta_tx else {
            tracing::debug!("Turn delta skipped: no feedback client configured");
            return;
        };

        let Some(snapshot) = snapshot else {
            tracing::debug!(
                ?turn_outcome,
                "Turn delta skipped: no snapshot (signals actor shut down)"
            );
            return;
        };

        let (fb_requests_sent, fb_last_request_at) = {
            let h = self.heuristics.read().await;
            (h.requests_sent(), h.last_request_at())
        };
        let delta = snapshot_to_turn_delta(
            snapshot,
            self.config.client_type,
            request_id,
            fb_requests_sent,
            fb_last_request_at,
            self.config.loc_tracking_enabled,
            turn_duration_ms,
            turn_outcome,
        );

        let turn = delta.turn_number;
        if let Err(err) = turn_delta_tx.try_send(TurnDeltaJob::Post(Box::new(delta))) {
            tracing::warn!(
                session_id = %self.session_id,
                turn,
                capacity = TURN_DELTA_QUEUE_CAPACITY,
                error = %err,
                "Turn delta dropped: the post queue is full or its worker is gone"
            );
        }
    }

    /// Wait until every turn delta queued so far has posted, or `deadline` passes.
    async fn flush_turn_deltas(&self, deadline: tokio::time::Instant) {
        let Some(turn_delta_tx) = &self.turn_delta_tx else {
            return;
        };
        let (reached, wait) = oneshot::channel();
        let flushed = tokio::time::timeout_at(deadline, async {
            turn_delta_tx
                .send(TurnDeltaJob::Barrier(reached))
                .await
                .ok()?;
            wait.await.ok()
        })
        .await;
        if flushed.is_err() {
            tracing::warn!(
                session_id = %self.session_id,
                "Turn deltas still posting at exit; not waiting for them"
            );
        }
    }

    /// Returns Ok(()) if sync succeeded or was skipped (no client).
    pub async fn sync_signals(&self) -> anyhow::Result<()> {
        self.sync_signals_inner(false).await
    }

    /// Force-sync current signals to the analytics backend, bypassing the cooldown check.
    /// Used for the final sync on shutdown to ensure no data is lost.
    pub(crate) async fn force_sync_signals(&self) -> anyhow::Result<()> {
        self.sync_signals_inner(true).await
    }

    /// On `force` (the process-exit final sync), skip the HTTP POST when the session has no turns or tool calls, so exit cannot hang on slow egress.
    /// Periodic sync (`force=false`) still may send empty snapshots.
    async fn sync_signals_inner(&self, force: bool) -> anyhow::Result<()> {
        if !self.config.telemetry_enabled {
            return Ok(());
        }

        let Some(client) = &self.feedback_client else {
            return Ok(());
        };

        if !force && !self.signals_handle.check_and_mark_sync().await {
            return Ok(());
        }

        if let Some(stats) = self.upload_queue_stats.get() {
            self.signals_handle.snapshot_gcs_queue(stats);
        }

        let Some(signals) = self.signals_handle.snapshot().await else {
            return Ok(());
        };

        // Empty session at exit: no reportable activity, so skip the analytics POST
        if force && !signals_are_reportable(&signals) {
            tracing::debug!(
                session_id = %self.session_id,
                "Skipping final signal sync on empty session"
            );
            return Ok(());
        }

        let update = signals_to_update(&signals, self.config.client_type);

        match client.update_signals(&self.session_id, &update).await {
            Ok(_) => {
                tracing::debug!(
                    session_id = %self.session_id,
                    turns = signals.turn_count,
                    "Synced session signals to analytics backend"
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to sync session signals"
                );
                Err(e)
            }
        }
    }

    /// Attempt OIDC token refresh after a 401 and retry the signal sync once.
    /// Returns the classified outcome for `handle_auth_outcome` to act on.
    /// Prefers waiting for the proactive-refresh task or main-request-path recovery over driving a `ServerRejected` refresh itself.
    async fn try_refresh_and_retry_sync(&self) -> SyncAuthOutcome {
        let Some(client) = &self.feedback_client else {
            return SyncAuthOutcome::Unrecoverable;
        };
        if !client.has_token_refresher() {
            return SyncAuthOutcome::Unrecoverable;
        }
        // 1. Wait briefly for the proactive refresh or main-path recovery to land a fresh token before driving our own ServerRejected.
        let refreshed = client.wait_for_token_refresh(Duration::from_secs(3)).await;
        // 2. If nobody refreshed, drive our own recovery as fallback.
        if !refreshed && !client.try_refresh_credentials().await {
            if client.is_auth_permanently_failed() {
                return SyncAuthOutcome::Permanent;
            }
            return SyncAuthOutcome::Transient;
        }
        // Any error after a successful refresh counts as a transient tick, so the consecutive-failure cap stays in effect
        // That covers a pathological flap (401, then refresh OK, then 5xx)
        // It also covers the rare sibling-rotation race where the IdP cache got re-set between our refresh and our retry
        match self.sync_signals().await {
            Ok(_) => SyncAuthOutcome::Recovered,
            Err(e) if client.is_auth_permanently_failed() => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Signal sync retry failed; IdP confirmed permanent failure"
                );
                SyncAuthOutcome::Permanent
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Signal sync retry failed after successful token refresh"
                );
                SyncAuthOutcome::Transient
            }
        }
    }

    /// Loads feedback heuristics config on startup.
    /// This should be spawned as a background task.
    #[tracing::instrument(skip_all, parent = None, fields(session_id = %self.session_id))]
    pub async fn run_sync_loop(self: Arc<Self>, cancel: tokio_util::sync::CancellationToken) {
        self.load_config().await;

        let mut interval = tokio::time::interval(self.config.sync_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut consecutive_auth_failures: u8 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // Exit paths that need a final sync call `FeedbackManager::shutdown`, which owns the budgeted force-sync and drain
                    // Cancel alone must not double-POST or hang on a dead analytics endpoint
                    tracing::debug!("Feedback sync loop cancelled");
                    break;
                }
                _ = interval.tick() => {
                    match self.sync_signals().await {
                        Ok(_) => {
                            consecutive_auth_failures = 0;
                        }
                        Err(e) if is_forbidden_error(&e) => {
                            tracing::warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "Signals sync loop stopped: 403 forbidden (session ownership mismatch)"
                            );
                            distill_telemetry::unified_log::warn(
                                "signals sync loop stopped: 403 forbidden",
                                Some(&self.session_id),
                                None,
                            );
                            break;
                        }
                        Err(e) if is_auth_error(&e) => {
                            let outcome = self.try_refresh_and_retry_sync().await;
                            if handle_auth_outcome(
                                outcome,
                                &mut consecutive_auth_failures,
                                &self.session_id,
                            )
                            .is_break()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                session_id = %self.session_id,
                                error = %e,
                                "Periodic signal sync failed"
                            );
                            // Non-auth error does not touch the counter.
                        }
                    }
                }
            }
        }
    }

    /// Force-sync with [`SHUTDOWN_SIGNAL_SYNC_TIMEOUT`].
    /// Empty sessions and telemetry-off are no-ops inside [`Self::sync_signals_inner`].
    async fn bounded_final_sync(&self, deadline: tokio::time::Instant) {
        match tokio::time::timeout_at(deadline, self.force_sync_signals()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Final signal sync failed during exit"
                );
            }
            Err(_) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    timeout_ms = SHUTDOWN_SIGNAL_SYNC_TIMEOUT.as_millis() as u64,
                    "Final signal sync timed out during exit; continuing"
                );
            }
        }
    }

    /// Shutdown: turn-delta flush and final signal sync (one shared budget), optional upload drain, then signals actor stop.
    /// For an empty session (no turns or tool calls), `sync_signals_inner(force)` skips the analytics POST; drain is skipped when pending is also 0.
    /// Non-empty drains use `min(config.drain_timeout, cap)` (default cap 5s, `GROK_SESSION_EXIT_DRAIN_SECS` up to hard max 7s).
    pub async fn shutdown(&self, queue: Option<&distill_file_utils::queue::UploadQueue>) {
        let pending = queue
            .map(|q| q.stats().pending.load(Ordering::Relaxed))
            .unwrap_or(0);
        // Snapshot before final sync/shutdown so drain can still see activity after the signals actor is torn down later
        let reportable = match self.signals_handle.snapshot().await {
            Some(s) => signals_are_reportable(&s),
            None => false,
        };

        // (1) Queued turn deltas (an exit cancel's row is the last one), then the final force-sync,
        // under one budget so exit stays inside the agent join grace.
        // The telemetry-off and empty-session checks live in sync_signals_inner.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_SIGNAL_SYNC_TIMEOUT;
        self.flush_turn_deltas(deadline).await;
        self.bounded_final_sync(deadline).await;

        // (2) Drain only when needed: an empty session with zero pending is skipped
        if let Some(queue) = queue {
            if pending == 0 && !reportable {
                tracing::debug!(
                    session_id = %self.session_id,
                    "Skipping upload drain on empty session with nothing pending"
                );
            } else {
                let budget = if pending == 0 {
                    SHUTDOWN_EMPTY_DRAIN_TIMEOUT
                } else {
                    nonempty_drain_budget(self.config.drain_timeout)
                };
                let remaining = queue.drain(budget).await;
                if remaining > 0 {
                    let pending_bytes = queue.stats().pending_bytes.load(Ordering::Relaxed);
                    tracing::warn!(
                        session_id = %self.session_id,
                        remaining,
                        pending_bytes,
                        budget_ms = budget.as_millis() as u64,
                        "Upload queue drain incomplete, {} items deferred to next-session recovery ({} bytes pending)",
                        remaining,
                        pending_bytes
                    );
                } else {
                    tracing::debug!(
                        session_id = %self.session_id,
                        "Upload queue drained successfully"
                    );
                }
            }
        }

        self.signals_handle.shutdown();
    }
}

/// Bound on the final signals POST at process exit.
/// Prefer a fast exit over waiting out a hung analytics endpoint; session metrics are best-effort.
pub(crate) const SHUTDOWN_SIGNAL_SYNC_TIMEOUT: Duration = Duration::from_secs(2);

/// Default ceiling on non-empty upload-queue drain at process exit.
/// Keeps sync and drain under [`crate::agent::activity::SESSION_FLUSH_GRACE`] with residual time for hooks/memory.
/// Override with `GROK_SESSION_EXIT_DRAIN_SECS` (still hard-capped by [`SHUTDOWN_DRAIN_HARD_MAX`]).
const SHUTDOWN_DRAIN_CAP: Duration = Duration::from_secs(5);

/// Absolute max non-empty drain at process exit.
/// The 10s flush grace, less the 2s signal-sync budget and the turn-end queue's two 250ms waits, leaves 7.5s; held at 7s for residual.
pub(crate) const SHUTDOWN_DRAIN_HARD_MAX: Duration = Duration::from_secs(7);

/// When nothing is pending, only wait this long for the upload worker to exit after the shutdown signal; in practice this takes milliseconds.
const SHUTDOWN_EMPTY_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Non-empty drain wait: honor config (tests / shorter defaults) but never exceed the process-exit cap.
/// Fleets on slow networks may raise it with `GROK_SESSION_EXIT_DRAIN_SECS`, up to [`SHUTDOWN_DRAIN_HARD_MAX`].
/// Raising it does not slow the empty-session fast path.
fn nonempty_drain_budget(config_timeout: Duration) -> Duration {
    let cap = std::env::var("GROK_SESSION_EXIT_DRAIN_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(SHUTDOWN_DRAIN_CAP)
        .min(SHUTDOWN_DRAIN_HARD_MAX)
        // A zero or garbage env value must not skip the drain entirely
        .max(Duration::from_secs(1));
    config_timeout.min(cap)
}

/// Used to skip the exit force_sync POST and the empty upload drain when a session exits with nothing to report.
fn signals_are_reportable(s: &crate::session::signals::SessionSignals) -> bool {
    s.turn_count > 0 || s.tool_call_count > 0
}

// Auth outcome handler used by run_sync_loop on 401.

/// Max consecutive failed sync ticks tolerated before stopping the loop.
/// ~10 minutes at the default 60s interval.
const MAX_CONSECUTIVE_AUTH_FAILURES: u8 = 10;

/// Telemetry `reason` discriminators on the `signals sync loop stopped permanently` event.
/// Pinned because alerts filter on these strings.
const REASON_AUTH_PERMANENT_FAILURE: &str = "auth_permanent_failure";
const REASON_NO_CLIENT_OR_REFRESHER: &str = "no_client_or_refresher";

const LOG_TITLE_TRANSIENT: &str = "signals sync transient auth failure";
const LOG_TITLE_STOPPED_PERMANENTLY: &str = "signals sync loop stopped permanently";

/// Classification of one 401-recovery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncAuthOutcome {
    /// Refresh and retry succeeded.
    Recovered,
    /// Refresh or retry failed transiently (lock timeout, network, sibling race, post-refresh 5xx).
    /// Increment the counter and retry next tick.
    Transient,
    /// IdP confirmed a terminal failure (`invalid_grant` / `invalid_client`).
    /// Only re-login will recover.
    Permanent,
    /// No client or no refresher configured, so nothing to retry.
    Unrecoverable,
}

fn handle_auth_outcome(
    outcome: SyncAuthOutcome,
    consecutive_auth_failures: &mut u8,
    session_id: &str,
) -> ControlFlow<()> {
    match outcome {
        SyncAuthOutcome::Recovered => {
            *consecutive_auth_failures = 0;
            tracing::info!(
                session_id = %session_id,
                "Signal sync recovered after token refresh"
            );
            ControlFlow::Continue(())
        }
        SyncAuthOutcome::Transient => {
            *consecutive_auth_failures = consecutive_auth_failures.saturating_add(1);
            tracing::warn!(
                session_id = %session_id,
                consecutive_failures = *consecutive_auth_failures,
                max = MAX_CONSECUTIVE_AUTH_FAILURES,
                "Signals sync transient auth failure"
            );
            distill_telemetry::unified_log::warn(
                LOG_TITLE_TRANSIENT,
                Some(session_id),
                Some(serde_json::json!({
                    "consecutive_failures": *consecutive_auth_failures,
                    "max": MAX_CONSECUTIVE_AUTH_FAILURES,
                })),
            );
            if *consecutive_auth_failures >= MAX_CONSECUTIVE_AUTH_FAILURES {
                tracing::warn!(
                    session_id = %session_id,
                    consecutive_failures = *consecutive_auth_failures,
                    "Signals sync loop stopped: consecutive transient auth failures"
                );
                distill_telemetry::unified_log::warn(
                    "signals sync loop stopped: consecutive transient auth failures",
                    Some(session_id),
                    Some(serde_json::json!({
                        "consecutive_failures": *consecutive_auth_failures,
                        "max": MAX_CONSECUTIVE_AUTH_FAILURES,
                    })),
                );
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        }
        SyncAuthOutcome::Permanent => {
            tracing::warn!(
                session_id = %session_id,
                reason = REASON_AUTH_PERMANENT_FAILURE,
                "Signals sync loop stopped: IdP confirmed permanent auth failure"
            );
            distill_telemetry::unified_log::warn(
                LOG_TITLE_STOPPED_PERMANENTLY,
                Some(session_id),
                Some(serde_json::json!({ "reason": REASON_AUTH_PERMANENT_FAILURE })),
            );
            ControlFlow::Break(())
        }
        SyncAuthOutcome::Unrecoverable => {
            tracing::warn!(
                session_id = %session_id,
                reason = REASON_NO_CLIENT_OR_REFRESHER,
                "Signals sync loop stopped: no client or no refresher configured"
            );
            distill_telemetry::unified_log::warn(
                LOG_TITLE_STOPPED_PERMANENTLY,
                Some(session_id),
                Some(serde_json::json!({ "reason": REASON_NO_CLIENT_OR_REFRESHER })),
            );
            ControlFlow::Break(())
        }
    }
}

/// Check if an error is an HTTP 401 Unauthorized response.
/// Uses typed downcast on [`FeedbackApiError`] instead of string matching, so it stays correct even if error messages change.
fn is_auth_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_unauthorized())
}

/// Check if an error is an HTTP 403 Forbidden response.
/// 403 from the signals endpoint means the session does not belong to the current user, a permanent condition that will never self-resolve.
fn is_forbidden_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<FeedbackApiError>()
        .is_some_and(|e| e.is_forbidden())
}

/// Run `op` once; on 401, wait for an in-flight refresh to land, then retry once.
/// Prefers waiting for the proactive-refresh task or main-request-path recovery over driving a `ServerRejected` refresh itself.
/// That avoids amplifying 401 bursts during token-expiry windows.
async fn with_one_shot_auth_retry<T, F, Fut>(
    client: &FeedbackClient,
    mut op: F,
) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match op().await {
        Ok(v) => Ok(v),
        Err(e) if is_auth_error(&e) => {
            // 1. Wait briefly for the proactive refresh or main-path recovery to land a fresh token.
            let refreshed = client.wait_for_token_refresh(Duration::from_secs(3)).await;
            // 2. If nobody refreshed, drive our own recovery as fallback.
            if refreshed || client.try_refresh_credentials().await {
                op().await
            } else {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Convert a FeedbackTier to a priority value (1-10, higher is more important).
fn tier_to_priority(tier: crate::session::feedback::FeedbackTier) -> i32 {
    use crate::session::feedback::FeedbackTier;
    match tier {
        FeedbackTier::Tier1 => 5, // Standard engagement
        FeedbackTier::Tier2 => 6, // Complex session with recovery
        FeedbackTier::Tier3 => 7, // Recovery from friction
    }
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_feedback_manager_local_only() {
        let manager = FeedbackManager::local_only("test-session-123");

        // Track some events
        let signals = manager.signals_handle();
        for _ in 0..10 {
            signals.increment_turn();
        }
        for _ in 0..5 {
            signals.record_tool_call("read_file");
        }
        for _ in 0..2 {
            signals.record_compaction(10_000);
        }

        // Give time for actor to process
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Check signals were tracked
        let snapshot = signals.snapshot().await.unwrap();
        assert_eq!(snapshot.turn_count, 10);
        assert_eq!(snapshot.tool_call_count, 5);
        assert_eq!(snapshot.compaction_count, 2);

        let eval = manager.heuristics.write().await.evaluate(&snapshot);
        assert!(eval.trigger_condition.is_some());
        assert_eq!(
            eval.trigger_condition.as_ref().unwrap().tier,
            crate::session::feedback::FeedbackTier::Tier1
        );

        manager.shutdown(None).await;
    }

    #[test]
    fn test_is_auth_error_detects_401() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "Invalid or expired credentials".to_string(),
        }
        .into();
        assert!(is_auth_error(&err));
    }

    #[test]
    fn test_is_auth_error_ignores_other_statuses() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err_500: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            context: "Signals update",
            body: "oops".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_500));

        let err_403: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            context: "Signals update",
            body: "ZDR team".to_string(),
        }
        .into();
        assert!(!is_auth_error(&err_403));
    }

    #[test]
    fn test_is_auth_error_ignores_non_api_errors() {
        assert!(!is_auth_error(&anyhow::anyhow!("network timeout")));
        assert!(!is_auth_error(&anyhow::anyhow!("connection refused")));
    }

    #[test]
    fn test_is_forbidden_error_detects_403() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::FORBIDDEN,
            context: "Signals update",
            body: "Access denied: session does not belong to this user".to_string(),
        }
        .into();
        assert!(is_forbidden_error(&err));
    }

    #[test]
    fn test_is_forbidden_error_ignores_other_statuses() {
        use crate::agent::feedback_client::FeedbackApiError;
        let err_401: anyhow::Error = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "Invalid credentials".to_string(),
        }
        .into();
        assert!(!is_forbidden_error(&err_401));
        assert!(!is_forbidden_error(&anyhow::anyhow!("network error")));
    }

    #[test]
    fn test_is_auth_error_works_through_anyhow_conversion() {
        use crate::agent::feedback_client::FeedbackApiError;
        // Verify the FeedbackApiError survives anyhow::Error round-trip (this is the actual path: send_json returns FeedbackApiError.into())
        let api_err = FeedbackApiError {
            status: reqwest::StatusCode::UNAUTHORIZED,
            context: "Signals update",
            body: "token expired".to_string(),
        };
        let anyhow_err: anyhow::Error = api_err.into();
        assert!(is_auth_error(&anyhow_err));
    }

    #[tokio::test]
    async fn test_feedback_manager_disabled() {
        let config = FeedbackManagerConfig {
            feedback_enabled: false,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session", None, config);

        // Even with signals, disabled manager should not request feedback
        let signals = manager.signals_handle();
        for _ in 0..20 {
            signals.increment_turn();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        let request = manager.maybe_request_feedback(None).await;
        assert!(request.is_none());

        manager.shutdown(None).await;
    }

    #[tokio::test]
    async fn test_shutdown_without_upload_queue_completes() {
        // This tests the None path in the drain logic.
        let manager = FeedbackManager::local_only("test-session-no-queue");

        manager.shutdown(None).await;

        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    #[tokio::test]
    async fn test_shutdown_with_upload_queue_drains() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use distill_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};
        use std::sync::Arc;

        struct MockResolver;
        impl TraceExportSource for MockResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Direct {
                        service_account_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(
            temp.path(),
            Arc::new(MockResolver),
            UploadRetryPolicy::default(),
        );

        let manager = FeedbackManager::local_only("test-session-with-queue");

        manager.shutdown(Some(&queue)).await;

        let snapshot = manager.signals_handle().snapshot().await;
        assert!(snapshot.is_none(), "Signals actor should be shut down");
    }

    /// Exit budgets must stay under the agent join grace so hung telemetry cannot push past `SESSION_FLUSH_GRACE` again.
    #[test]
    #[serial_test::serial]
    fn test_shutdown_budgets_fit_under_session_flush_grace() {
        use crate::agent::activity::SESSION_FLUSH_GRACE;
        // `nonempty_drain_budget` reads env; pin default regardless of CI presets.
        let _unset = distill_test_support::env::EnvGuard::unset("GROK_SESSION_EXIT_DRAIN_SECS");
        assert!(
            SHUTDOWN_SIGNAL_SYNC_TIMEOUT + SHUTDOWN_DRAIN_HARD_MAX <= SESSION_FLUSH_GRACE,
            "sync + hard-max drain must fit under flush grace"
        );
        assert!(
            SHUTDOWN_SIGNAL_SYNC_TIMEOUT + SHUTDOWN_DRAIN_CAP < SESSION_FLUSH_GRACE,
            "sync + default drain cap must leave residual grace for hooks/memory"
        );
        assert!(
            SHUTDOWN_EMPTY_DRAIN_TIMEOUT < SHUTDOWN_DRAIN_CAP,
            "empty drain must be strictly shorter than the non-empty cap"
        );
        assert_eq!(
            nonempty_drain_budget(Duration::from_secs(30)),
            SHUTDOWN_DRAIN_CAP,
            "default config 30s must clamp to the process-exit cap"
        );
        assert_eq!(
            nonempty_drain_budget(Duration::from_secs(2)),
            Duration::from_secs(2),
            "config shorter than the cap must be honored"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_nonempty_drain_budget_env_raises_cap() {
        {
            let _guard =
                distill_test_support::env::EnvGuard::set("GROK_SESSION_EXIT_DRAIN_SECS", "7");
            assert_eq!(
                nonempty_drain_budget(Duration::from_secs(30)),
                Duration::from_secs(7),
                "env may raise the cap for slow networks"
            );
        }
        {
            let _guard =
                distill_test_support::env::EnvGuard::set("GROK_SESSION_EXIT_DRAIN_SECS", "99");
            assert_eq!(
                nonempty_drain_budget(Duration::from_secs(30)),
                SHUTDOWN_DRAIN_HARD_MAX,
                "env must not exceed the hard max under flush grace"
            );
        }
    }

    /// Sync-loop cancel must return promptly once the loop is idle in `select!`.
    /// Final force-sync is owned by `shutdown` only (cancel does not re-POST).
    /// Uses a closed-port base URL so the immediate first tick's sync fails fast and parks on the long interval.
    #[tokio::test]
    async fn test_sync_loop_cancel_returns_without_force_sync() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::sync::Arc;
        use std::time::Instant;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .connect_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        // Nothing listening, so the first periodic sync errors quickly
        let client = FeedbackClient::with_client(http, "http://127.0.0.1:1", None);
        let config = FeedbackManagerConfig {
            telemetry_enabled: true,
            sync_interval: Duration::from_secs(3600),
            ..Default::default()
        };
        let manager = Arc::new(FeedbackManager::new(
            "test-cancel-no-sync",
            Some(client),
            config,
        ));
        let cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(manager.clone().run_sync_loop(cancel.clone()));
        // First tick and failed sync, then idle waiting on the long interval
        tokio::time::sleep(Duration::from_millis(400)).await;
        let started = Instant::now();
        cancel.cancel();
        loop_handle.await.expect("sync loop join");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "cancel must not wait out a force-sync budget, got {elapsed:?}"
        );
    }

    /// Helper: TCP accept that never answers (slow egress / hung analytics).
    async fn blackhole_http_addr() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind blackhole listener");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                    drop(stream);
                });
            }
        });
        format!("http://{addr}")
    }

    /// An empty session (no turns or tool calls) must not wait on a hung analytics endpoint at exit; the final force_sync is skipped entirely.
    #[tokio::test]
    async fn test_shutdown_empty_session_skips_force_sync_on_slow_egress() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::time::Instant;

        let base = blackhole_http_addr().await;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let client = FeedbackClient::with_client(http, base, None);
        let config = FeedbackManagerConfig {
            telemetry_enabled: true,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-empty-skip-sync", Some(client), config);
        // No increment_turn / record_tool_call, so the session is empty

        let started = Instant::now();
        manager.shutdown(None).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "empty session must skip force_sync and return immediately, got {elapsed:?}"
        );
    }

    /// Telemetry off: never hit the network on exit even with turns recorded.
    #[tokio::test]
    async fn test_shutdown_telemetry_off_skips_force_sync() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::time::Instant;

        let base = blackhole_http_addr().await;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let client = FeedbackClient::with_client(http, base, None);
        let config = FeedbackManagerConfig {
            telemetry_enabled: false,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-telemetry-off", Some(client), config);
        manager.signals_handle().increment_turn();

        let started = Instant::now();
        manager.shutdown(None).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "telemetry-off shutdown must not wait on the network, got {elapsed:?}"
        );
    }

    /// A hung analytics endpoint must not hold process exit for the full HTTP client timeout when the session *does* have reportable activity.
    /// `shutdown` wraps the final force-sync in `SHUTDOWN_SIGNAL_SYNC_TIMEOUT` (2s).
    #[tokio::test]
    async fn test_shutdown_force_sync_is_timeout_capped() {
        use crate::agent::feedback_client::FeedbackClient;
        use std::time::Instant;

        let base = blackhole_http_addr().await;
        let http = reqwest::Client::builder()
            // Deliberately longer than the shutdown budget so the outer timeout is what fires, not the client's
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        let client = FeedbackClient::with_client(http, base, None);
        let config = FeedbackManagerConfig {
            telemetry_enabled: true,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-session-hung-sync", Some(client), config);
        // Non-empty session so force_sync is not skipped.
        manager.signals_handle().increment_turn();

        let started = Instant::now();
        manager.shutdown(None).await;
        let elapsed = started.elapsed();
        // Outer budget is 2s; allow generous slack for slow CI hosts, but never approach the 60s client timeout
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown must return within the signal-sync budget + slack on a hung signals POST, got {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(1500),
            "expected to wait most of the force_sync budget when reportable, got {elapsed:?}"
        );
    }

    /// Turn deltas post from one worker in send order, and `shutdown` waits for the ones still
    /// queued, so an exit cancel's row reaches the backend after the rows before it.
    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // test client hits a localhost mock
    async fn test_turn_deltas_post_in_order_and_shutdown_drains_them() {
        use crate::agent::feedback_client::FeedbackClient;
        use axum::{Json, Router, routing::post};

        // Every POST is held briefly, so the deltas are all still queued when shutdown starts.
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<i64>();
        let router = Router::new().route(
            "/v1/sessions/{id}/turn-deltas",
            post(move |Json(body): Json<serde_json::Value>| {
                let seen_tx = seen_tx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let _ = seen_tx.send(
                        body.get("turnNumber")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(-1),
                    );
                    Json(serde_json::json!({
                        "sessionId": "test-fifo",
                        "turnNumber": 0,
                        "recordedAt": chrono::Utc::now(),
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let client =
            FeedbackClient::with_client(reqwest::Client::new(), format!("http://{addr}/v1"), None);
        let config = FeedbackManagerConfig {
            telemetry_enabled: true,
            ..Default::default()
        };
        let manager = FeedbackManager::new("test-fifo", Some(client), config);
        let signals = manager.signals_handle();
        for _ in 1..=5 {
            signals.increment_turn();
            let snapshot = signals
                .take_turn_end_snapshot()
                .await
                .expect("signals actor up");
            manager
                .send_turn_delta_with_snapshot(
                    Some(&snapshot),
                    None,
                    Some(1),
                    TurnOutcome::Cancelled,
                )
                .await;
        }

        manager.shutdown(None).await;

        let mut seen = Vec::new();
        while let Ok(turn) = seen_rx.try_recv() {
            seen.push(turn);
        }
        assert_eq!(
            seen,
            vec![1, 2, 3, 4, 5],
            "every queued delta posted, in order"
        );
    }

    /// Empty upload queue uses the short worker-exit budget.
    #[tokio::test]
    async fn test_shutdown_empty_queue_uses_short_drain_budget() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use distill_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};
        use std::sync::Arc;
        use std::time::Instant;

        struct MockResolver;
        impl TraceExportSource for MockResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Direct {
                        service_account_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let queue = UploadQueue::spawn(
            temp.path(),
            Arc::new(MockResolver),
            UploadRetryPolicy::default(),
        );
        let manager = FeedbackManager::local_only("test-session-empty-drain");

        let started = Instant::now();
        manager.shutdown(Some(&queue)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "empty-queue shutdown must finish well under the non-empty budget, got {elapsed:?}"
        );
    }

    /// Non-empty queue drain is clamped to the process-exit cap (default 5s), not left open-ended at the 30s config default.
    /// Abandoned durable-pair items stay on disk for next-session orphan recovery.
    #[tokio::test]
    async fn test_shutdown_nonempty_queue_clamps_drain_and_leaves_durable_pair() {
        use crate::session::repo_changes::{TraceExportConfig, UploadMethod};
        use axum::{Router, body::Body, http::StatusCode, response::IntoResponse, routing::post};
        use distill_file_utils::queue::{TraceExportSource, UploadQueue, UploadRetryPolicy};
        use std::sync::Arc;
        use std::time::Instant;

        async fn slow_handler(_body: Body) -> impl IntoResponse {
            tokio::time::sleep(Duration::from_secs(60)).await;
            (StatusCode::OK, "ok")
        }

        let app = Router::new().route("/v1/storage", post(slow_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        struct SlowProxyResolver {
            base: String,
        }
        impl TraceExportSource for SlowProxyResolver {
            fn resolve(&self) -> TraceExportConfig {
                TraceExportConfig {
                    bucket_url: Some("gs://test-bucket".to_string()),
                    service_account_key: None,
                    upload_method: UploadMethod::Proxy {
                        proxy_base_url: self.base.clone(),
                        user_token: "test-token".to_string(),
                        deployment_key: None,
                        alpha_test_key: None,
                    },
                    prefix_dir: None,
                    gcs_prefix: None,
                    absolute_paths: false,
                    archive_name_override: None,
                }
            }
        }

        let temp = tempfile::TempDir::new().unwrap();
        let policy = UploadRetryPolicy {
            max_attempts: 1,
            ..Default::default()
        };
        let queue = UploadQueue::spawn_with_concurrency(
            temp.path(),
            Arc::new(SlowProxyResolver {
                base: format!("http://{addr}/v1"),
            }),
            policy,
            1,
        );
        queue
            .enqueue(
                b"payload",
                "session/turn_0/slow.json",
                "application/json",
                "slow",
                "sess-shutdown-clamp",
                0,
            )
            .await
            .expect("enqueue");
        // Let the worker pick up the item so drain waits on a stuck upload.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            queue.stats().pending.load(Ordering::Relaxed) > 0,
            "item must still be pending against the slow handler"
        );

        let manager = FeedbackManager::local_only("test-session-nonempty-drain");
        let started = Instant::now();
        manager.shutdown(Some(&queue)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(8),
            "non-empty drain must clamp near 5s (+slack), got {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_secs(4),
            "expected to wait most of the non-empty budget, got {elapsed:?}"
        );

        let queue_dir = temp.path().join("upload_queue");
        let leftover: Vec<_> = std::fs::read_dir(&queue_dir)
            .expect("queue dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            !leftover.is_empty(),
            "abandoned durable-pair artifacts must remain for startup recovery"
        );
    }

    // ── handle_auth_outcome tests ──────────────────────────────────────────

    /// 9 transient failures must NOT break, and a subsequent `Recovered` must reset the counter to 0.
    #[test]
    fn test_sync_loop_continues_through_transient_auth_failures() {
        let mut counter: u8 = 0;
        for _ in 0..(MAX_CONSECUTIVE_AUTH_FAILURES - 1) {
            let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
            assert_eq!(flow, ControlFlow::Continue(()));
        }
        assert_eq!(counter, MAX_CONSECUTIVE_AUTH_FAILURES - 1);
        let flow = handle_auth_outcome(SyncAuthOutcome::Recovered, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(counter, 0, "Recovered must reset the counter");
    }

    /// Exactly `MAX_CONSECUTIVE_AUTH_FAILURES` consecutive `Transient` outcomes break the loop.
    #[test]
    fn test_sync_loop_breaks_after_max_transient_auth_failures() {
        let mut counter: u8 = 0;
        for i in 0..(MAX_CONSECUTIVE_AUTH_FAILURES - 1) {
            let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
            assert_eq!(
                flow,
                ControlFlow::Continue(()),
                "iteration {i} should still continue"
            );
        }
        // The 10th transient outcome reaches MAX_CONSECUTIVE_AUTH_FAILURES and breaks
        let flow = handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, MAX_CONSECUTIVE_AUTH_FAILURES);
    }

    /// `Permanent` breaks immediately and does not bump the counter.
    #[test]
    fn test_sync_loop_breaks_immediately_on_permanent_failure() {
        let mut counter: u8 = 0;
        let flow = handle_auth_outcome(SyncAuthOutcome::Permanent, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, 0);
    }

    /// 5 transient, then 1 recovered, then 5 more transient must not break.
    #[test]
    fn test_sync_loop_counter_resets_on_successful_sync() {
        let mut counter: u8 = 0;
        for _ in 0..5 {
            assert_eq!(
                handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s"),
                ControlFlow::Continue(())
            );
        }
        assert_eq!(counter, 5);
        assert_eq!(
            handle_auth_outcome(SyncAuthOutcome::Recovered, &mut counter, "s"),
            ControlFlow::Continue(())
        );
        assert_eq!(counter, 0, "Recovered must reset the counter");
        for _ in 0..5 {
            assert_eq!(
                handle_auth_outcome(SyncAuthOutcome::Transient, &mut counter, "s"),
                ControlFlow::Continue(())
            );
        }
        assert_eq!(counter, 5, "second burst should be re-counted from zero");
    }

    /// `Unrecoverable` breaks the loop and does not bump the counter.
    #[test]
    fn test_sync_loop_breaks_on_unrecoverable() {
        let mut counter: u8 = 0;
        let flow = handle_auth_outcome(SyncAuthOutcome::Unrecoverable, &mut counter, "s");
        assert_eq!(flow, ControlFlow::Break(()));
        assert_eq!(counter, 0);
    }

    /// `FeedbackClient::is_auth_permanently_failed` reflects the attached `AuthManager`'s `permanent_failure()` cache.
    /// Recording a failure makes it true; the verdict aging out makes it false.
    #[tokio::test]
    async fn test_is_auth_permanently_failed_reads_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        use distill_login::error::RefreshTokenFailedReason;
        use distill_login::{AuthManager, GrokAuth, GrokComConfig};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
        let client = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());

        assert!(!client.is_auth_permanently_failed());

        // The verdict is scoped to the live credential's key.
        am.hot_swap(GrokAuth {
            key: "tok".into(),
            ..GrokAuth::test_default()
        });
        // Use a non-sticky reason: only recoverable verdicts age out (a sticky `RefreshTokenRejected` never expires), which exercises the TTL path
        am.record_permanent_failure("tok".to_string(), RefreshTokenFailedReason::Other.into());
        assert!(client.is_auth_permanently_failed());

        am.force_permanent_failure_aged_out();
        assert!(!client.is_auth_permanently_failed());
    }

    #[test]
    fn test_is_auth_permanently_failed_without_auth_manager() {
        use crate::agent::feedback_client::FeedbackClient;
        let client = FeedbackClient::new("http://example/v1", None);
        assert!(!client.is_auth_permanently_failed());
    }

    /// `has_token_refresher` requires BOTH an `AuthManager` AND a refresher wired in.
    /// Otherwise a static-deployment-key session would be mis-classified as recoverable.
    #[tokio::test]
    async fn test_has_token_refresher_requires_refresher_attached() {
        use crate::agent::feedback_client::FeedbackClient;
        use distill_login::{AuthManager, GrokComConfig};
        use std::sync::Arc;

        struct NoOpRefresher;
        #[async_trait::async_trait]
        impl distill_login::refresh::TokenRefresher for NoOpRefresher {
            async fn refresh(
                &self,
                _reason: distill_login::refresh::RefreshReason,
            ) -> distill_login::refresh::RefreshOutcome {
                distill_login::refresh::RefreshOutcome::TransientFailure {
                    message: "noop".into(),
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));

        let bare = FeedbackClient::new("http://example/v1", None);
        assert!(!bare.has_token_refresher());

        let with_am = FeedbackClient::new("http://example/v1", None).with_auth_manager(am.clone());
        assert!(
            !with_am.has_token_refresher(),
            "AuthManager without a refresher must NOT be reported as recoverable"
        );

        am.set_refresher(std::sync::Arc::new(NoOpRefresher));
        assert!(with_am.has_token_refresher());
    }

    /// Outcome × envelope matrix: existing props keep their names, `outcome` is always present,
    /// and `source`/taxonomy ride only when the envelope is present.
    #[test]
    fn user_feedback_event_matrix() {
        let envelope = serde_json::json!({
            "structured_feedback": {
                "schema_version": 1,
                "source": "draft",
                "type": "bug",
                "task_category": "debug",
                "failure_mode": "sloppy_code",
            }
        });
        let outcomes = [
            (SubmitOutcome::Submitted, "submitted"),
            (SubmitOutcome::LocalOnly, "local_only"),
            (SubmitOutcome::Failed(anyhow::anyhow!("offline")), "failed"),
        ];
        for (outcome, wire) in &outcomes {
            for enveloped in [false, true] {
                let mut expected = serde_json::json!({
                    "session_id": "sess-1",
                    "has_feedback_text": true,
                    "model_id": "grok-4",
                    "is_solicited": false,
                    "outcome": wire,
                });
                if enveloped {
                    expected.as_object_mut().unwrap().extend([
                        ("source".to_owned(), "draft".into()),
                        ("feedback_type".to_owned(), "bug".into()),
                        ("task_category".to_owned(), "debug".into()),
                        ("failure_mode".to_owned(), "sloppy_code".into()),
                    ]);
                }
                let mut submission = new_submission(
                    "sess-1".to_owned(),
                    ClientType::Tui,
                    FeedbackContent::Text("great session".to_owned()),
                );
                submission.model_id = Some("grok-4".to_owned());
                submission.metadata = enveloped.then(|| envelope.clone());
                assert_eq!(
                    serde_json::to_value(user_feedback_event(&submission, outcome, false)).unwrap(),
                    expected,
                    "{wire} enveloped={enveloped}"
                );
            }
        }
    }
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
mod author_identity_tests {
    use super::*;
    use crate::util::user_identity::ResolvedUserIdentity;
    use axum::{Router, routing::post};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// Mock feedback backend: capture the POST /v1/feedback JSON body.
    async fn start_capture_server() -> (
        SocketAddr,
        Arc<parking_lot::Mutex<Option<serde_json::Value>>>,
    ) {
        let captured = Arc::new(parking_lot::Mutex::new(None::<serde_json::Value>));
        let captured_for_handler = captured.clone();
        let router = Router::new().route(
            "/v1/feedback",
            post(move |body: axum::Json<serde_json::Value>| {
                let captured = captured_for_handler.clone();
                async move {
                    *captured.lock() = Some(body.0);
                    axum::Json(serde_json::json!({
                        "feedbackId": "fb-1",
                        "createdAt": chrono::Utc::now(),
                    }))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, captured)
    }

    fn text_submission() -> FeedbackSubmission {
        let mut s = new_submission(
            "sess-1".to_string(),
            ClientType::Tui,
            FeedbackContent::Text("great session".to_string()),
        );
        s.model_id = Some("grok-4".to_string());
        s
    }

    /// End-to-end: an env var (as a device-management launcher would inject) referenced by `[feedback.user]` with `$VAR` is expanded at config load.
    /// The identity is then resolved, carried on the feedback POST alongside the rest of the submission, and retained on the local entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn env_var_identity_reaches_the_wire_end_to_end() {
        let _email =
            distill_test_support::env::EnvGuard::set("GROK_TEST_WORK_EMAIL", "ada@corp.example");
        let _name = distill_test_support::env::EnvGuard::set("GROK_TEST_WORK_NAME", "Ada Lovelace");

        // The loader expands `$VAR` at load, exactly as a trusted config tier ships it.
        let mut value = toml::from_str::<toml::Value>(
            r#"
[feedback.user]
name = ["$GROK_TEST_WORK_NAME"]
email = ["$GROK_TEST_WORK_EMAIL"]
"#,
        )
        .unwrap();
        crate::config::expand_env_vars_in_toml(&mut value);
        let cfg = crate::agent::config::Config::new_from_toml_cfg(&value).unwrap();
        let user = cfg.feedback.user.expect("[feedback.user] present");

        // Resolve through the real production entry point.
        let identity = crate::util::user_identity::cached_identity(Some(&user))
            .await
            .expect("identity resolved");
        assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(identity.email.as_deref(), Some("ada@corp.example"));

        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: Some(identity),
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        // Author identity rides on the same submission as the rest of the feedback; nothing is stripped here
        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("authorName"),
            Some(&serde_json::json!("Ada Lovelace"))
        );
        assert_eq!(
            body.get("authorEmail"),
            Some(&serde_json::json!("ada@corp.example"))
        );
        assert_eq!(body.get("modelId"), Some(&serde_json::json!("grok-4")));
        assert_eq!(
            body.get("feedbackText"),
            Some(&serde_json::json!("great session"))
        );

        // The local entry keeps the author fields and the full context.
        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(persisted.author_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(persisted.author_email.as_deref(), Some("ada@corp.example"));
        assert_eq!(persisted.model_id.as_deref(), Some("grok-4"));
    }

    /// `GROK_USER_METADATA` is merged into the submission and travels with it: onto the wire body for triage and onto the local feedback.jsonl entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_merges_user_metadata_into_submission() {
        let _guard = distill_test_support::env::EnvGuard::set(
            "GROK_USER_METADATA",
            r#"{"team": "platform-tools"}"#,
        );
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let mut submission = text_submission();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            Some(&tx),
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );

        let msg = rx.try_recv().expect("persistence entry was sent");
        let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
            panic!("expected a feedback persistence entry");
        };
        let persisted = entry.submission.expect("submission persisted");
        assert_eq!(
            persisted.metadata.as_ref().and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );
    }

    /// `metadata.structured_feedback` is the client's typed envelope: the later-wins env merge
    /// must neither replace it nor invent it, while other env keys keep merging.
    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_env_metadata_cannot_touch_structured_feedback() {
        let _guard = distill_test_support::env::EnvGuard::set(
            "GROK_USER_METADATA",
            r#"{"team": "platform-tools", "structured_feedback": {"type": "forged"}}"#,
        );
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );
        let envelope = serde_json::json!({
            "schema_version": 1,
            "source": "write",
            "type": "bug",
        });
        let mut submission = text_submission();
        submission.metadata = Some(serde_json::json!({ "structured_feedback": envelope.clone() }));

        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            None,
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));

        let body = captured.lock().clone().expect("server saw the POST");
        assert_eq!(
            body.get("metadata")
                .and_then(|m| m.get("structured_feedback")),
            Some(&envelope)
        );
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );

        // A report without the envelope must not grow one from the environment either.
        *captured.lock() = None;
        let mut submission = text_submission();
        let outcome = submit_feedback_workflow(
            &mut submission,
            Some(&client),
            None,
            SubmitFeedbackOptions {
                solicited: false,
                telemetry_enabled: false,
                author_identity: None,
            },
        )
        .await;
        assert!(matches!(outcome, SubmitOutcome::Submitted));
        let body = captured.lock().clone().expect("server saw the POST");
        assert!(
            body.get("metadata")
                .and_then(|m| m.get("structured_feedback"))
                .is_none()
        );
        assert_eq!(
            body.get("metadata").and_then(|m| m.get("team")),
            Some(&serde_json::json!("platform-tools"))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn workflow_without_identity_omits_author_fields() {
        let (addr, captured) = start_capture_server().await;
        let client = crate::agent::feedback_client::FeedbackClient::with_client(
            reqwest::Client::new(),
            format!("http://{addr}/v1"),
            Some("tok".into()),
        );

        // Both no opt-in and an unresolved opt-in must leave the author keys out of the body and the local entry
        for (case, author_identity) in [
            ("no opt-in", None),
            ("unresolved", Some(ResolvedUserIdentity::default())),
        ] {
            // Reset so this case can't pass on the previous case's body.
            *captured.lock() = None;
            let mut submission = text_submission();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let outcome = submit_feedback_workflow(
                &mut submission,
                Some(&client),
                Some(&tx),
                SubmitFeedbackOptions {
                    solicited: false,
                    telemetry_enabled: false,
                    author_identity,
                },
            )
            .await;
            assert!(matches!(outcome, SubmitOutcome::Submitted), "{case}");

            let body = captured.lock().clone().expect("server saw the POST");
            assert!(body.get("authorName").is_none(), "{case}: {body}");
            assert!(body.get("authorEmail").is_none(), "{case}: {body}");

            let msg = rx.try_recv().expect("persistence entry was sent");
            let PersistenceMsg::Feedback(LocalFeedbackEntry::UserFeedback(entry)) = msg else {
                panic!("expected a feedback persistence entry");
            };
            let persisted = entry.submission.expect("submission persisted");
            assert_eq!(persisted.author_name, None, "{case}");
            assert_eq!(persisted.author_email, None, "{case}");
        }
    }
}
