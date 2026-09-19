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
}

impl SummaryGenerator {
    pub(crate) fn new(config: SummaryConfig) -> Self {
        Self {
            state: State::Idle,
            config,
        }
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

                // Transition to Done so subsequent ContentChunk messages don't spawn duplicate title generation tasks
                self.state = State::Done;

                let sampling_client = self.config.sampling_client.clone();
                let model = self.config.model.clone();
                let persistence_tx = self.config.persistence_tx.clone();

                // A background task runs the LLM call so the persistence actor keeps processing messages (updates, flushes)
                tokio::spawn(async move {
                    let mut title =
                        generate_session_summary(content.clone(), sampling_client, &model).await;
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
}
