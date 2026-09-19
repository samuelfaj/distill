// Modified for Distill by Samuel Fajreldines, 2026.
use tokio::sync::{mpsc, oneshot};

use distill_sampling_types::{ConversationRequest, ConversationResponse, SamplingError};

use crate::actor::request_task::CompletionResult;
use crate::commands::SamplerCommand;
use crate::config::SamplerConfig;
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// One doom-loop recovery action performed during a request.
pub struct DoomLoopRecoveryAttempt {
    pub triggers: Vec<String>,
    pub aborted_at_chunk: Option<u64>,
}

/// Awaited sampling result plus request-level detector metadata.
pub struct CollectedSamplingResult {
    /// Terminal response and metrics, or the rich sampling error.
    pub result: Result<(ConversationResponse, InferenceLatencyStats), SamplingError>,
    /// Whether a terminal event was successfully queued on the event rail.
    pub terminal_event_queued: bool,
    /// Deduplicated raw labels observed across every attempt in the request.
    pub doom_loop_signals: Vec<String>,
    /// Doom-loop recovery actions performed for this request.
    pub doom_loop_recovery_attempts: Vec<DoomLoopRecoveryAttempt>,
}

/// Cheaply-cloneable handle to the sampler actor.
/// Internally just an `mpsc::UnboundedSender<SamplerCommand>`.
/// All methods are non-blocking (fire-and-forget) except for the `*_async` queries which return a future awaiting an `oneshot::Receiver`.
#[derive(Clone)]
pub struct SamplerHandle {
    cmd_tx: mpsc::UnboundedSender<SamplerCommand>,
}

impl SamplerHandle {
    /// `pub(crate)` because only [`SamplerActor::spawn`](crate::actor::SamplerActor::spawn) produces one of these.
    pub(crate) fn new(cmd_tx: mpsc::UnboundedSender<SamplerCommand>) -> Self {
        Self { cmd_tx }
    }

    /// Create a no-op handle that discards all commands.
    ///
    /// Useful for tests and callers that need a `SamplerHandle` field before the actor is wired up.
    pub fn noop() -> Self {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        // Receiver is dropped immediately; sends will fail but every send-site uses `let _ = ...` so that is fine
        Self { cmd_tx }
    }

    /// Test seam: `notify` fires when the first command is submitted.
    #[cfg(any(test, feature = "test-support"))]
    pub fn notify_on_submit(notify: std::sync::Arc<tokio::sync::Notify>) -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            if cmd_rx.recv().await.is_some() {
                notify.notify_one();
            }
            while cmd_rx.recv().await.is_some() {}
        });
        Self { cmd_tx }
    }

    /// Fire-and-forget: results arrive via the shared event channel.
    pub fn submit(&self, request_id: RequestId, request: ConversationRequest) {
        let _ = self.cmd_tx.send(SamplerCommand::Submit {
            request_id,
            request: Box::new(request),
            config: None,
            completion_tx: None,
        });
    }

    /// Submit a sampling request with an explicit per-request config override (e.g., a different model than the actor's default).
    pub fn submit_with_config(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
        config: SamplerConfig,
    ) {
        let _ = self.cmd_tx.send(SamplerCommand::Submit {
            request_id,
            request: Box::new(request),
            config: Some(Box::new(config)),
            completion_tx: None,
        });
    }

    /// No-op if the request id is unknown (already finished or never submitted).
    pub fn cancel(&self, request_id: RequestId) {
        let _ = self.cmd_tx.send(SamplerCommand::Cancel { request_id });
    }

    /// Update the default sampling config (e.g., after model switch or auth refresh).
    /// The next request submitted without an override will use it.
    pub fn update_config(&self, config: SamplerConfig) {
        let _ = self.cmd_tx.send(SamplerCommand::UpdateConfig {
            config: Box::new(config),
        });
    }

    /// Query whether a request is still in flight.
    /// Returns `false` for unknown / finished / cancelled ids.
    pub async fn is_active(&self, request_id: RequestId) -> bool {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(SamplerCommand::IsActive {
            request_id,
            reply: reply_tx,
        });
        reply_rx.await.unwrap_or(false)
    }

    /// Query the number of in-flight requests.
    /// Returns 0 if the actor has been shut down.
    pub async fn active_count(&self) -> usize {
        let (reply_tx, reply_rx) = oneshot::channel();
        let _ = self
            .cmd_tx
            .send(SamplerCommand::ActiveCount { reply: reply_tx });
        reply_rx.await.unwrap_or(0)
    }

    /// Submit a request and await its completion. Events still flow to the shared channel for live UI updates.
    /// This method also awaits the per-request completion oneshot so the caller gets a clean `Result` without filtering events.
    /// Sequential callers (compaction, summary, `/btw` side questions) use this.
    pub async fn submit_and_collect(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
    ) -> CompletionResult {
        self.submit_and_collect_with_metadata(request_id, request)
            .await
            .result
    }

    /// Submit and also return detector labels observed on discarded attempts.
    pub async fn submit_and_collect_with_metadata(
        &self,
        request_id: RequestId,
        request: ConversationRequest,
    ) -> CollectedSamplingResult {
        // RAII guard: when this future is dropped (cancel, panic, or normal return), tell the sampler actor to cancel the in-flight request_id
        // No-op if the actor already finished and removed it from its active set
        struct CancelOnDrop {
            cmd_tx: mpsc::UnboundedSender<SamplerCommand>,
            request_id: RequestId,
        }
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                let _ = self.cmd_tx.send(SamplerCommand::Cancel {
                    request_id: self.request_id.clone(),
                });
            }
        }

        let (completion_tx, completion_rx) = oneshot::channel();
        let cancel_id = request_id.clone();

        // Only create the guard if Submit actually reached the actor
        let _guard = self
            .cmd_tx
            .send(SamplerCommand::Submit {
                request_id,
                request: Box::new(request),
                config: None,
                completion_tx: Some(completion_tx),
            })
            .ok()
            .map(|_| CancelOnDrop {
                cmd_tx: self.cmd_tx.clone(),
                request_id: cancel_id,
            });
        completion_rx
            .await
            .unwrap_or_else(|_| CollectedSamplingResult {
                result: Err(SamplingError::auth_unknown(
                    "sampler actor dropped before completion",
                )),
                terminal_event_queued: false,
                doom_loop_signals: Vec::new(),
                doom_loop_recovery_attempts: Vec::new(),
            })
    }
}
