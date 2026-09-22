// Modified for Distill by Samuel Fajreldines, 2026.
//! Session summary (title) generation.
//!
//! Checks whether a summary exists, generates one via the LLM, persists it, syncs to remote, updates the session registry, and notifies the client.
//! The persistence actor just calls [`SummaryGenerator::update`]; all state transitions are internal.

use crate::extensions::notification::{SessionNotification, SessionUpdate as XaiSessionUpdate};
use crate::sampling::Client as OaiCompatClient;
use crate::session::helpers::session_summary::generate_session_summary;
use crate::session::info::Info;
use crate::session::persistence::PersistenceMsg;
use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_sampling_types::ConversationResponse;
use std::time::Instant;
use tokio::sync::mpsc;

enum State {
    /// No summary generated yet. The next [`SummaryGenerator::update`] call will attempt one.
    Idle,
    /// Summary generation has been attempted (spawned or already on disk). No further work needed.
    Done,
}

pub(crate) struct SummaryConfig {
    pub(crate) sampling_client: OaiCompatClient,
    pub(crate) model: String,
    /// Channel back to the persistence actor for sequential storage writes.
    /// Weak: a strong sender here would keep the actor's own channel and task alive.
    pub(crate) persistence_tx: mpsc::WeakUnboundedSender<PersistenceMsg>,
}

/// Created once per persistence actor. The only public method is [`update`], which is called from the `ContentChunk` handler.
pub(crate) struct SummaryGenerator {
    state: State,
    config: SummaryConfig,
    /// Weak: ChatStateActor owns the persistence sender, so a permanent strong
    /// recorder here would keep the two actor lifetimes cyclically alive.
    initial_title_recorder: Option<distill_chat_state::WeakChatStateHandle>,
}

impl SummaryGenerator {
    pub(crate) fn new(config: SummaryConfig) -> Self {
        Self {
            state: State::Idle,
            config,
            initial_title_recorder: None,
        }
    }

    pub(crate) fn register_initial_title_recorder(
        &mut self,
        recorder: distill_chat_state::WeakChatStateHandle,
    ) {
        self.initial_title_recorder = Some(recorder);
    }

    /// Generate a session summary from the first content chunk.
    /// Idle: checks disk for an existing summary, spawns a background task for LLM title generation so the persistence actor is not blocked.
    /// Empty content is skipped (stays Idle) so the next chunk can retry.
    pub(crate) fn update(&mut self, content: String) {
        match self.state {
            State::Done => {}
            State::Idle => {
                // No text to generate a title from (e.g. image-only message).
                // Stay Idle so the next ContentChunk with actual text retries.
                if content.trim().is_empty() {
                    return;
                }

                let Some(recorder) = self
                    .initial_title_recorder
                    .as_ref()
                    .and_then(|weak| weak.upgrade())
                else {
                    tracing::debug!(
                        "session title generation skipped because its usage recorder is unavailable"
                    );
                    return;
                };

                let attempt_id = format!("initial-title:{}", uuid::Uuid::new_v4());
                if recorder
                    .register_pending_usage_attempt(attempt_id.clone(), false)
                    .is_err()
                {
                    tracing::debug!(
                        "session title generation skipped because its usage recorder closed during admission"
                    );
                    return;
                }

                // Transition to Done so subsequent ContentChunk messages don't spawn duplicate title generation tasks
                self.state = State::Done;

                let sampling_client = self.config.sampling_client.clone();
                let model = self.config.model.clone();
                let persistence_tx = self.config.persistence_tx.clone();

                // A background task runs the LLM call so the persistence actor keeps processing messages (updates, flushes)
                tokio::spawn(async move {
                    // The strong recorder exists only for this bounded call and is
                    // released with the task, avoiding a permanent actor cycle.
                    let mut attempt = InitialTitleAttempt::new(
                        recorder,
                        &sampling_client,
                        &model,
                        attempt_id,
                        persistence_tx.clone(),
                    );
                    let generated =
                        generate_session_summary(content.clone(), sampling_client, &model).await;
                    attempt.record(generated.status, generated.response.as_ref());
                    let mut title = generated.title;
                    if title.trim().is_empty() {
                        title =
                            crate::session::helpers::session_summary::title_fallback_from_user_text(
                                &content,
                            );
                    }

                    // The actor persists the title (only if the session has no title yet) and notifies the client there
                    // If a manual `/rename` won the race, the actor rejects the generated title, so it never reaches the client
                    match persistence_tx.upgrade() {
                        Some(tx) => {
                            let _ = tx.send(PersistenceMsg::GeneratedTitle(title));
                        }
                        None => tracing::debug!("session closed before its title was generated"),
                    }
                });
            }
        }
    }

    /// Mark as Done (e.g. when disk already has a summary during load).
    pub(crate) fn mark_done(&mut self) {
        self.state = State::Done;
    }

    /// Inverse of [`mark_done`]: `/rename --auto` calls this so the next content chunk regenerates a title through the normal if-absent path.
    pub(crate) fn reset(&mut self) {
        self.state = State::Idle;
    }

    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self.state, State::Idle)
    }
}

struct InitialTitleAttempt {
    recorder: distill_chat_state::ChatStateHandle,
    attempt_id: String,
    configured_model: String,
    endpoint: String,
    applied_effort: Option<String>,
    started_at: Instant,
    recorded: bool,
    persistence_tx: mpsc::WeakUnboundedSender<PersistenceMsg>,
}

impl InitialTitleAttempt {
    fn new(
        recorder: distill_chat_state::ChatStateHandle,
        client: &OaiCompatClient,
        model: &str,
        attempt_id: String,
        persistence_tx: mpsc::WeakUnboundedSender<PersistenceMsg>,
    ) -> Self {
        Self {
            recorder,
            attempt_id,
            configured_model: model.to_owned(),
            endpoint: client.attribution_endpoint(),
            applied_effort: client.attribution_applied_effort(None, Some(100)),
            started_at: Instant::now(),
            recorded: false,
            persistence_tx,
        }
    }

    fn record(
        &mut self,
        status: distill_chat_state::UsageCallStatus,
        response: Option<&ConversationResponse>,
    ) {
        if self.recorded {
            return;
        }
        self.recorded = true;

        let model_id = response
            .and_then(|response| response.assistant())
            .and_then(|assistant| assistant.model_id.clone())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| self.configured_model.clone());
        let usage = response.and_then(|response| response.usage.clone());
        let cost_usd_ticks = response.and_then(|response| response.cost_usd_ticks);
        self.recorder.record_usage_attribution(
            distill_chat_state::UsageAttribution {
                attempt_id: self.attempt_id.clone(),
                task_id: None,
                turn_id: None,
                request_id: response.and_then(|response| response.message_id.clone()),
                role: "auxiliary".to_owned(),
                model_id,
                endpoint: Some(self.endpoint.clone()),
                requested_effort: None,
                applied_effort: self.applied_effort.clone(),
                status,
                usage: usage.clone(),
                usage_complete: usage.is_some(),
                api_duration_ms: Some(self.started_at.elapsed().as_millis() as u64),
                cost_usd_ticks,
                cost_basis: if cost_usd_ticks.is_some() {
                    distill_chat_state::UsageCostBasis::Reported
                } else {
                    distill_chat_state::UsageCostBasis::Unknown
                },
            },
            // Initial title is a detached session side-call; it has no prompt/task
            // identity, so fold it once into the session ledger only.
            false,
        );
        if let Some(tx) = self.persistence_tx.upgrade() {
            let _ = tx.send(PersistenceMsg::RefreshUsage {
                recorder: self.recorder.downgrade(),
            });
        }
    }
}

impl Drop for InitialTitleAttempt {
    fn drop(&mut self) {
        if !self.recorded {
            self.record(distill_chat_state::UsageCallStatus::Cancelled, None);
        }
    }
}

/// Notify the client that a session summary is available.
pub(crate) fn notify_client(gateway: &Option<GatewaySender>, info: &Info, title: &str) {
    let Some(gateway) = gateway else {
        return;
    };

    let notification = SessionNotification {
        session_id: info.id.clone(),
        update: XaiSessionUpdate::SessionSummaryGenerated {
            session_summary: title.to_owned(),
        },
        meta: None,
    };
    if let Ok(params) = serde_json::value::to_raw_value(&notification) {
        gateway.forward_fire_and_forget(acp::ExtNotification::new(
            "x.ai/session_notification",
            params.into(),
        ));
    }

    gateway.forward_fire_and_forget(session_info_update(info.id.clone(), title));
}

pub(crate) fn session_info_update(
    session_id: acp::SessionId,
    title: &str,
) -> acp::SessionNotification {
    // `updatedAt` is omitted, not refreshed: renaming is not activity, and `session/list` sorts on `last_active_at`, which a title write never moves
    acp::SessionNotification::new(
        session_id,
        acp::SessionUpdate::SessionInfoUpdate(
            acp::SessionInfoUpdate::new().title(title.to_owned()),
        ),
    )
}

/// Manual-rename fan-out: same payload as [`session_info_update`] plus `_meta.x.ai/titleIsManual`.
/// Old clients ignore the unknown key.
pub(crate) fn session_info_update_manual(
    session_id: acp::SessionId,
    title: &str,
) -> acp::SessionNotification {
    session_info_update(session_id, title).meta(
        crate::extensions::notification::title_is_manual_meta()
            .as_object()
            .cloned(),
    )
}

/// Unpin fan-out: no title (avoid blanking list-driven clients) plus `_meta.x.ai/titleIsManual: false`.
pub(crate) fn session_info_update_unpinned(session_id: acp::SessionId) -> acp::SessionNotification {
    acp::SessionNotification::new(
        session_id,
        acp::SessionUpdate::SessionInfoUpdate(acp::SessionInfoUpdate::new()),
    )
    .meta(
        crate::extensions::notification::title_is_unpinned_meta()
            .as_object()
            .cloned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_info_update_manual_carries_meta_and_raw_title() {
        let n = session_info_update_manual(acp::SessionId::new("s"), "a &amp; b");
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(
            v.get("_meta")
                .and_then(|m| m.get(crate::extensions::notification::TITLE_IS_MANUAL_META_KEY)),
            Some(&serde_json::Value::Bool(true))
        );
        let title = v
            .pointer("/update/title")
            .or_else(|| v.pointer("/update/sessionInfoUpdate/title"))
            .cloned();
        assert_eq!(title, Some(serde_json::json!("a &amp; b")), "{v}");
    }

    #[test]
    fn session_info_update_unpinned_stamps_false_meta_without_title() {
        let n = session_info_update_unpinned(acp::SessionId::new("s"));
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(
            v.get("_meta")
                .and_then(|m| m.get(crate::extensions::notification::TITLE_IS_MANUAL_META_KEY)),
            Some(&serde_json::Value::Bool(false))
        );
        let title = v
            .pointer("/update/title")
            .or_else(|| v.pointer("/update/sessionInfoUpdate/title"));
        assert!(
            title.is_none(),
            "unpin SessionInfoUpdate must omit title: {v}"
        );
    }

    #[test]
    fn auto_session_info_update_omits_manual_meta() {
        let n = session_info_update(acp::SessionId::new("s"), "Auto");
        let v = serde_json::to_value(&n).unwrap();
        assert!(
            v.get("_meta")
                .and_then(|m| m.get(crate::extensions::notification::TITLE_IS_MANUAL_META_KEY))
                .is_none(),
            "auto-title fan-out must not stamp titleIsManual: {v}"
        );
    }

    #[test]
    fn reset_returns_generator_to_idle() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sampling_client =
            OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap();
        let mut generator = SummaryGenerator::new(SummaryConfig {
            sampling_client,
            model: String::new(),
            persistence_tx: tx.downgrade(),
        });
        assert!(generator.is_idle());
        generator.mark_done();
        assert!(!generator.is_idle());
        generator.reset();
        assert!(generator.is_idle());
        generator.reset();
        assert!(generator.is_idle());
    }

    #[test]
    fn initial_title_stays_idle_without_usage_recorder() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sampling_client =
            OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap();
        let mut generator = SummaryGenerator::new(SummaryConfig {
            sampling_client,
            model: "configured-title-model".to_owned(),
            persistence_tx: tx.downgrade(),
        });

        generator.update("paid title generation must not run unrecorded".to_owned());

        assert!(generator.is_idle());
    }

    fn title_sampler_config(base_url: String) -> distill_sampler::SamplerConfig {
        let mut config = distill_sampler::SamplerConfig::default();
        config.base_url = base_url;
        config.model = "configured-title-model".to_owned();
        config
    }

    fn title_tool_call_events() -> Vec<distill_test_support::SseEvent> {
        vec![
            distill_test_support::SseEvent::data(
                serde_json::json!({
                    "id": "chatcmpl-initial-title",
                    "object": "chat.completion.chunk",
                    "created": 1234567890,
                    "model": "served-title-model",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "index": 0,
                                "id": "call-title",
                                "type": "function",
                                "function": {
                                    "name": "session_title",
                                    "arguments": "{\"session_title\":\"Captured initial title\"}"
                                }
                            }]
                        },
                        "finish_reason": null
                    }]
                })
                .to_string(),
            ),
            distill_test_support::SseEvent::data(
                serde_json::json!({
                    "id": "chatcmpl-initial-title",
                    "object": "chat.completion.chunk",
                    "created": 1234567890,
                    "model": "served-title-model",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 5,
                        "total_tokens": 15,
                        "cost_in_usd_ticks": 42
                    }
                })
                .to_string(),
            ),
            distill_test_support::SseEvent::data("[DONE]"),
        ]
    }

    fn title_length_rejected_events() -> Vec<distill_test_support::SseEvent> {
        vec![
            distill_test_support::SseEvent::data(
                serde_json::json!({
                    "id": "chatcmpl-initial-title-length",
                    "object": "chat.completion.chunk",
                    "created": 1234567890,
                    "model": "served-title-model",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": "partial title"},
                        "finish_reason": null
                    }]
                })
                .to_string(),
            ),
            distill_test_support::SseEvent::data(
                serde_json::json!({
                    "id": "chatcmpl-initial-title-length",
                    "object": "chat.completion.chunk",
                    "created": 1234567890,
                    "model": "served-title-model",
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }],
                    "usage": {
                        "prompt_tokens": 10,
                        "completion_tokens": 5,
                        "total_tokens": 15,
                        "cost_in_usd_ticks": 43
                    }
                })
                .to_string(),
            ),
            distill_test_support::SseEvent::data("[DONE]"),
        ]
    }

    fn chat_state_recorder() -> (
        distill_chat_state::ChatStateHandle,
        tokio_util::sync::CancellationToken,
    ) {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let handle = distill_chat_state::ChatStateActor::spawn(
            Vec::new(),
            distill_sampling_types::SamplingConfig::default(),
            Box::new(distill_chat_state::NullChatPersistence),
            event_tx,
            cancellation.clone(),
        );
        (handle, cancellation)
    }

    #[tokio::test]
    async fn initial_title_records_provider_identity_and_cost_once() {
        let server = distill_test_support::MockInferenceServer::start()
            .await
            .expect("mock inference server");
        server.enqueue_response(
            "/v1/chat/completions",
            distill_test_support::ScriptedResponse::sse(title_tool_call_events()),
        );
        let client = OaiCompatClient::new(title_sampler_config(server.url())).unwrap();
        let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        let (recorder, cancellation) = chat_state_recorder();
        let observer = recorder.clone();
        let mut generator = SummaryGenerator::new(SummaryConfig {
            sampling_client: client,
            model: "configured-title-model".to_owned(),
            persistence_tx: persistence_tx.downgrade(),
        });
        generator.register_initial_title_recorder(recorder.downgrade());

        generator.update("capture this initial title request".to_owned());

        let mut saw_refresh = false;
        loop {
            let message =
                tokio::time::timeout(std::time::Duration::from_secs(2), persistence_rx.recv())
                    .await
                    .expect("title generation did not finish")
                    .expect("persistence channel closed");
            match message {
                PersistenceMsg::RefreshUsage { .. } => saw_refresh = true,
                PersistenceMsg::GeneratedTitle(title) => {
                    assert_eq!(title, "Captured initial title");
                    break;
                }
                other => panic!("unexpected initial-title persistence message: {other:?}"),
            }
        }
        assert!(saw_refresh, "terminal title attribution must refresh usage");

        let ledger = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let ledger = observer
                    .try_get_session_usage()
                    .await
                    .expect("chat-state actor should answer usage");
                if !ledger.attributions.is_empty() {
                    break ledger;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("initial title usage was not recorded");
        assert_eq!(ledger.attributions.len(), 1);
        assert_eq!(ledger.totals.model_calls, 1);
        let [attribution] = ledger.attributions.as_slice() else {
            panic!(
                "expected one initial-title attribution: {:?}",
                ledger.attributions
            );
        };
        assert!(attribution.attempt_id.starts_with("initial-title:"));
        assert_eq!(
            attribution.request_id.as_deref(),
            Some("chatcmpl-initial-title")
        );
        assert_eq!(attribution.role, "auxiliary");
        assert_eq!(attribution.model_id, "served-title-model");
        assert_eq!(
            attribution.status,
            distill_chat_state::UsageCallStatus::Completed
        );
        assert_eq!(attribution.cost_usd_ticks, Some(42));
        assert_eq!(
            attribution.cost_basis,
            distill_chat_state::UsageCostBasis::Reported
        );
        assert!(attribution.usage.is_some());

        cancellation.cancel();
    }

    #[tokio::test]
    async fn initial_title_preserves_paid_length_rejection_metadata() {
        let server = distill_test_support::MockInferenceServer::start()
            .await
            .expect("mock inference server");
        server.enqueue_response(
            "/v1/chat/completions",
            distill_test_support::ScriptedResponse::sse(title_length_rejected_events()),
        );
        let client = OaiCompatClient::new(title_sampler_config(server.url())).unwrap();

        let generated = generate_session_summary(
            "capture this paid rejected title".to_owned(),
            client,
            "configured-title-model",
        )
        .await;

        assert_eq!(
            generated.status,
            distill_chat_state::UsageCallStatus::Rejected
        );
        assert_eq!(generated.title, "capture this paid rejected title");
        let response = generated
            .response
            .expect("length rejection must retain the paid response");
        assert_eq!(
            response.message_id.as_deref(),
            Some("chatcmpl-initial-title-length")
        );
        assert_eq!(
            response
                .assistant()
                .and_then(|assistant| assistant.model_id.as_deref()),
            Some("served-title-model")
        );
        assert_eq!(response.cost_usd_ticks, Some(43));
        assert!(response.usage.is_some());
    }

    #[tokio::test]
    async fn dropped_initial_title_attempt_records_cancelled_once() {
        let client = OaiCompatClient::new(distill_sampler::SamplerConfig::default()).unwrap();
        let (recorder, cancellation) = chat_state_recorder();
        let observer = recorder.clone();
        let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(InitialTitleAttempt::new(
            recorder,
            &client,
            "configured-title-model",
            "initial-title:cancelled".to_owned(),
            persistence_tx.downgrade(),
        ));

        let ledger = observer
            .try_get_session_usage()
            .await
            .expect("chat-state actor should answer usage");
        assert_eq!(ledger.attributions.len(), 1);
        assert_eq!(ledger.totals.model_calls, 1);
        assert_eq!(
            ledger.attributions[0].status,
            distill_chat_state::UsageCallStatus::Cancelled
        );
        assert_eq!(
            ledger.attributions[0].cost_basis,
            distill_chat_state::UsageCostBasis::Unknown
        );

        cancellation.cancel();
    }
}
