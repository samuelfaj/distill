// Modified for Distill by Samuel Fajreldines, 2026.
//! Outbound updates for `SessionActor`: `send_update` and its buffered/transient/direct variants.
//! Also Grok-notification handling and the gateway-bridge dispatch shims.
use super::*;
/// Hook / image-intake diagnostics and background_tasks snapshots leave the no-output rewind window open; every other variant closes it.
/// `HookRunStarted` fires for the prompt gate before any output, so a Ctrl+C during a slow gate must still rewind the prompt.
pub(super) fn closes_cancel_rewind_window(update: &XaiSessionUpdate) -> bool {
    !matches!(
        update,
        XaiSessionUpdate::HookRunStarted { .. }
            | XaiSessionUpdate::HookExecution { .. }
            | XaiSessionUpdate::ImageCompressed { .. }
            | XaiSessionUpdate::ImageDropped { .. }
            | XaiSessionUpdate::BackgroundTasks { .. }
    )
}
fn scrub_inbound_session_summary(
    notification: &mut crate::extensions::notification::SessionNotification,
) {
    use crate::extensions::notification::{SessionUpdate, TITLE_IS_MANUAL_META_KEY};
    use crate::session::persistence::sanitize_and_cap_title;
    let SessionUpdate::SessionSummaryGenerated { session_summary } = &mut notification.update
    else {
        return;
    };
    *session_summary = sanitize_and_cap_title(session_summary).unwrap_or_default();
    if let Some(serde_json::Value::Object(m)) = notification.meta.as_mut() {
        m.remove(TITLE_IS_MANUAL_META_KEY);
    }
}
#[cfg(test)]
mod inbound_title_scrub_tests {
    use super::scrub_inbound_session_summary;
    use crate::extensions::notification::{
        SessionNotification, SessionUpdate, TITLE_IS_MANUAL_META_KEY,
    };
    #[test]
    fn strips_controls_caps_and_drops_manual_meta() {
        use crate::session::persistence::MAX_TITLE_SCALARS;
        let mut n = SessionNotification {
            session_id: agent_client_protocol::SessionId::new("s"),
            update: SessionUpdate::SessionSummaryGenerated {
                session_summary: format!("\u{1b}]0;X\u{07}{}", "é".repeat(MAX_TITLE_SCALARS + 8)),
            },
            meta: Some(serde_json::json!({ TITLE_IS_MANUAL_META_KEY: true, "eventId": "keep" })),
        };
        scrub_inbound_session_summary(&mut n);
        let SessionUpdate::SessionSummaryGenerated { session_summary } = &n.update else {
            panic!("variant kept");
        };
        const PREFIX: &str = "]0;X";
        let expected = format!(
            "{PREFIX}{}",
            "é".repeat(MAX_TITLE_SCALARS - PREFIX.chars().count())
        );
        assert_eq!(session_summary, &expected);
        let meta = n.meta.as_ref().unwrap();
        assert!(meta.get(TITLE_IS_MANUAL_META_KEY).is_none());
        assert_eq!(meta.get("eventId").and_then(|v| v.as_str()), Some("keep"));
    }
    #[test]
    fn leaves_other_variants_untouched() {
        let mut n = SessionNotification {
            session_id: agent_client_protocol::SessionId::new("s"),
            update: SessionUpdate::MemoryFlushStarted,
            meta: Some(serde_json::json!({ TITLE_IS_MANUAL_META_KEY: true })),
        };
        scrub_inbound_session_summary(&mut n);
        assert_eq!(
            n.meta
                .as_ref()
                .and_then(|m| m.get(TITLE_IS_MANUAL_META_KEY)),
            Some(&serde_json::json!(true))
        );
    }
}
#[cfg(test)]
mod inbound_summary_persist_scrub_tests {
    use super::support::create_test_actor;
    use super::*;
    use crate::extensions::notification::TITLE_IS_MANUAL_META_KEY;
    use crate::session::persistence::MAX_TITLE_SCALARS;
    /// Drive inbound `_x.ai/session/update` through `handle_xai_session_notification`.
    /// Removing the `scrub_inbound_session_summary` call site makes this test fail.
    #[tokio::test]
    async fn persist_path_scrubs_title_and_drops_manual_meta() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                let dirty = format!("\u{1b}]0;X\u{07}{}", "é".repeat(MAX_TITLE_SCALARS + 8));
                actor
                    .handle_xai_session_notification(XaiSessionNotification {
                        session_id: acp::SessionId::new("test-actor"),
                        update: XaiSessionUpdate::SessionSummaryGenerated {
                            session_summary: dirty,
                        },
                        meta: Some(serde_json::json!({
                            TITLE_IS_MANUAL_META_KEY: true,
                            "eventId": "keep"
                        })),
                    })
                    .await;
                const PREFIX: &str = "]0;X";
                let expected = format!(
                    "{PREFIX}{}",
                    "é".repeat(MAX_TITLE_SCALARS - PREFIX.chars().count())
                );
                loop {
                    match prx.try_recv().expect("inbound summary must be persisted") {
                        PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(
                            notif,
                        )) => {
                            let XaiSessionUpdate::SessionSummaryGenerated { session_summary } =
                                &notif.update
                            else {
                                continue;
                            };
                            assert_eq!(session_summary, &expected);
                            let meta = notif.meta.as_ref().expect("meta kept for eventId");
                            assert!(
                                meta.get(TITLE_IS_MANUAL_META_KEY).is_none(),
                                "persist rail must drop forged titleIsManual"
                            );
                            assert_eq!(meta.get("eventId").and_then(|v| v.as_str()), Some("keep"));
                            break;
                        }
                        _ => continue,
                    }
                }
            })
            .await;
    }
}
/// Result of folding subagent usage into the parent's ledgers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SubagentUsageApply {
    /// Tokens attributed to the live open prompt (and session).
    AttributedToPrompt,
    /// Tokens landed on the session ledger only (pin mismatch / no live pin).
    /// Sticky report only: do not stain ledgers for "missing" spend.
    SessionOnly,
}
impl SessionActor {
    /// Apply subagent usage. `Ok` once the chat-state handle acks; `Err` if the apply failed.
    pub(super) async fn record_subagent_usage(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
    ) -> Result<SubagentUsageApply, ()> {
        self.record_subagent_usage_with_attributions(
            by_model,
            &[],
            parent_prompt_id,
            incomplete,
        )
        .await
    }

    pub(super) async fn record_subagent_usage_with_attributions(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        attributions: &[distill_chat_state::UsageAttribution],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
    ) -> Result<SubagentUsageApply, ()> {
        self.record_subagent_usage_with_attributions_and_pending(
            by_model,
            attributions,
            &[],
            parent_prompt_id,
            incomplete,
        )
        .await
    }

    pub(super) async fn record_subagent_usage_with_attributions_and_pending(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        attributions: &[distill_chat_state::UsageAttribution],
        pending_attempts: &[String],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
    ) -> Result<SubagentUsageApply, ()> {
        if by_model.is_empty()
            && attributions.is_empty()
            && pending_attempts.is_empty()
            && !incomplete
        {
            return Ok(SubagentUsageApply::AttributedToPrompt);
        }
        let current = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        let attributable = parent_prompt_id.is_some() && parent_prompt_id == current.as_deref();
        if !self
            .chat_state_handle
            .record_subagent_usage_with_attributions_and_pending(
                by_model.to_vec(),
                attributions.to_vec(),
                pending_attempts.to_vec(),
                attributable,
                incomplete,
            )
            .await
        {
            return Err(());
        }
        Ok(if attributable {
            SubagentUsageApply::AttributedToPrompt
        } else {
            SubagentUsageApply::SessionOnly
        })
    }
    /// `SessionCommand::RecordSubagentUsage` handler: fold the child's usage, then ack.
    /// A pin mismatch lands session-only and gets the report-level sticky (the stamped prompt's bill under-counts); an attributed fold needs no sticky since any nested incomplete is already on the ledger.
    /// A failed apply drops the ack so the child's true-miss fallback runs.
    pub(super) async fn handle_record_subagent_usage_command(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
        respond_to: tokio::sync::oneshot::Sender<()>,
    ) {
        self.handle_record_subagent_usage_command_with_attributions(
            by_model,
            &[],
            parent_prompt_id,
            incomplete,
            respond_to,
        )
        .await;
    }

    pub(super) async fn handle_record_subagent_usage_command_with_attributions(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        attributions: &[distill_chat_state::UsageAttribution],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
        respond_to: tokio::sync::oneshot::Sender<()>,
    ) {
        self.handle_record_subagent_usage_command_with_attributions_and_pending(
            by_model,
            attributions,
            &[],
            parent_prompt_id,
            incomplete,
            respond_to,
        )
        .await;
    }

    pub(super) async fn handle_record_subagent_usage_command_with_attributions_and_pending(
        &self,
        by_model: &[(String, distill_chat_state::UsageTotals)],
        attributions: &[distill_chat_state::UsageAttribution],
        pending_attempts: &[String],
        parent_prompt_id: Option<&str>,
        incomplete: bool,
        respond_to: tokio::sync::oneshot::Sender<()>,
    ) {
        match self
            .record_subagent_usage_with_attributions_and_pending(
                by_model,
                attributions,
                pending_attempts,
                parent_prompt_id,
                incomplete,
            )
            .await
        {
            Ok(SubagentUsageApply::AttributedToPrompt) => {
                self.persist_live_usage().await;
                let _ = respond_to.send(());
            }
            Ok(SubagentUsageApply::SessionOnly) => {
                let _ = self.mark_subagent_usage_not_applied(parent_prompt_id).await;
                self.persist_live_usage().await;
                let _ = respond_to.send(());
            }
            Err(()) => {}
        }
    }
    /// `SessionCommand::MarkSubagentUsageNotApplied` handler: sticky +
    /// pin-aware ledger marks, acked only when a mark actually landed.
    pub(super) async fn handle_mark_subagent_usage_not_applied_command(
        &self,
        parent_prompt_id: Option<&str>,
        respond_to: tokio::sync::oneshot::Sender<()>,
    ) {
        if self.mark_apply_miss_incomplete(parent_prompt_id).await {
            let _ = respond_to.send(());
        }
    }
    /// True-miss / unpinned fail-closed: sticky for freeze report + pin-aware
    /// ledger marks. Prompt ledger is stained only when the stamped pin is the
    /// live open prompt (never stain a different live turn). Session always.
    pub(super) async fn mark_apply_miss_incomplete(&self, stamped_pin: Option<&str>) -> bool {
        let sticky = self.mark_subagent_usage_not_applied(stamped_pin).await;
        let live = self.current_prompt_id.lock().ok().and_then(|g| g.clone());
        let stain_prompt = match (stamped_pin, live.as_deref()) {
            (Some(pin), Some(live_id)) => pin == live_id,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => false,
        };
        let ledger_ok = self
            .chat_state_handle
            .mark_usage_incomplete(stain_prompt, true)
            .await;
        sticky || ledger_ok
    }
    /// Shared finalize for freeze and cancel: ledgers are marked incomplete only on `fail_closed`; the other outcome flags only shape the report.
    /// Clears the sticky not-applied report after the snapshot.
    pub(super) async fn finalize_usage_from_outcome(
        &self,
        prompt_id: &str,
        outcome: super::turn::UsageDrainOutcome,
    ) -> Option<crate::extensions::notification::PromptUsage> {
        if outcome.fail_closed {
            let _ = self
                .chat_state_handle
                .mark_usage_incomplete(true, true)
                .await;
        }
        let usage = self
            .snapshot_prompt_usage_marked(outcome.report_incomplete())
            .await;
        self.clear_subagent_usage_not_applied(prompt_id);
        usage
    }
    /// Sends an update to the persistence layer and the gateway.
    /// Optionally includes a `chunk_index` for tracking LLM streaming chunks.
    pub(super) async fn send_update(&self, update: acp::SessionUpdate, chunk_index: Option<u64>) {
        self.send_update_full(update, chunk_index, None, false)
            .await;
    }
    async fn send_update_full(
        &self,
        update: acp::SessionUpdate,
        chunk_index: Option<u64>,
        agent_timestamp_ms_override: Option<i64>,
        is_replay: bool,
    ) {
        self.close_rewind_window().await;
        if let acp::SessionUpdate::ToolCall(tool_call) = &update
            && matches!(tool_call.kind, acp::ToolKind::Edit)
        {
            let cwd = self.tool_context.cwd.as_path();
            for loc in &tool_call.locations {
                let mut p = loc.path.clone();
                if p.is_absolute() {
                    if let Ok(rel) = p.strip_prefix(cwd) {
                        p = rel.to_path_buf();
                    } else {
                        continue;
                    }
                }
                if !p.as_os_str().is_empty() {
                    self.chat_state_handle
                        .record_agent_edited_path(p.to_string_lossy().to_string());
                }
            }
        }
        let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
        let meta_info = self.chat_state_handle.get_notification_meta().await;
        let (stream_start_ms, turn_start_ms) = meta_info
            .map(|m| (m.stream_start_ms, m.turn_start_ms))
            .unwrap_or((None, None));
        let event_id = self.generate_event_id();
        let agent_timestamp_ms =
            agent_timestamp_ms_override.unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
        let (update_type, update_params) = Self::extract_update_info(&update);
        let mut meta = json!({
            "totalTokens": total_tokens,
            "eventId": event_id,
            "agentTimestampMs": agent_timestamp_ms,
        });
        let obj = meta
            .as_object_mut()
            .expect("json! literal is always an Object");
        if let Some(pid) = self.current_prompt_id.lock().ok().and_then(|g| g.clone()) {
            obj.insert("promptId".to_string(), pid.into());
        }
        if let Some(ms) = stream_start_ms {
            obj.insert("streamStartMs".to_string(), ms.into());
        }
        if let Some(ms) = turn_start_ms {
            obj.insert("turnStartMs".to_string(), ms.into());
        }
        if let Some(update_type) = update_type {
            obj.insert("updateType".to_string(), update_type.into());
        }
        if let Some(update_params) = update_params {
            obj.insert("updateParams".to_string(), update_params);
        }
        if let Some(idx) = chunk_index {
            obj.insert("chunkId".to_string(), idx.into());
        }
        if is_replay {
            obj.insert("isReplay".to_string(), true.into());
        }
        let notification = acp::SessionNotification::new(self.session_info.id.clone(), update)
            .meta(meta.as_object().cloned());
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Routes through `event_tx`, the `ReplayBuffer`, and `emit_buffered`, so chunks are merged, debounced, and emitted.
    /// For one-shot Grok events (RetryState, ImageCompressed, HookExecution, AutoCompactCompleted, etc.), use `send_xai_notification` instead.
    /// The frequency-based split (`send_buffered_xai_update` vs `send_xai_notification`) mirrors the ACP-side split.
    pub(super) async fn send_buffered_xai_update(&self, update: XaiSessionUpdate) {
        self.close_rewind_window().await;
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: None,
        };
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Enqueue a `CurrentModeUpdate` on the FIFO event pipeline, stamped at enqueue time like `send_update`.
    /// A direct `emit_notification_direct` here would mint a HIGHER id that is delivered BEFORE those chunks.
    /// The client's in-order dedup would then drop the chunks as stale: silent text loss on a mid-stream plan-mode toggle.
    pub(super) fn enqueue_current_mode_update(&self, current_mode_id: acp::SessionModeId) {
        let notification = acp::SessionNotification::new(
            self.session_info.id.clone(),
            acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(current_mode_id)),
        )
        .meta(self.build_notification_meta().as_object().cloned());
        let _ = self
            .event_tx
            .send(SessionEvent::Notification(notification.into()));
    }
    /// Emit a notification that has come out of the high-frequency streaming path (after the `ReplayBuffer` has decided to flush it).
    /// ACP (`AgentMessageChunk`, `AgentThoughtChunk`) delegates to `emit_notification_direct` (persists and forwards to the gateway).
    /// Grok (`ToolCallDeltaChunk`) inlines a gateway forward as `ExtNotification` only, with no persistence and no hook dispatch.
    pub(super) async fn emit_buffered(&self, notification: SessionNotification) {
        match notification {
            SessionNotification::Acp(n) => {
                self.emit_notification_direct(*n).await;
            }
            SessionNotification::Xai(n) => {
                self.log_outbound_xai_buffered(&n);
                if self
                    .notifications
                    .gateway_enabled
                    .load(std::sync::atomic::Ordering::Relaxed)
                    && let Ok(value) = serde_json::to_value(&*n)
                    && let Ok(params) = serde_json::value::to_raw_value(&value)
                {
                    self.notifications
                        .gateway
                        .forward_fire_and_forget(acp::ExtNotification::new(
                            "x.ai/session_notification",
                            params.into(),
                        ));
                }
            }
        }
    }
    /// Tracing log for buffered Grok notifications emerging from emit_buffered.
    /// Mirrors `log_outbound_notification` for ACP.
    /// Visible with `RUST_LOG=acp_event=info`.
    fn log_outbound_xai_buffered(&self, notification: &XaiSessionNotification) {
        if !matches!(
            notification.update,
            XaiSessionUpdate::ToolCallDeltaChunk { .. }
        ) {
            return;
        }
        tracing::info!(
            target: "acp_event",
            event = "xai_buffered_notification_sent",
            session_id = %self.session_info.id,
            "Sending buffered xAI session notification"
        );
    }
    fn log_outbound_notification(&self, notification: &acp::SessionNotification) {
        let meta = notification.meta.as_ref();
        let event_id = meta
            .and_then(|m| m.get("eventId"))
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        let agent_timestamp_ms = meta
            .and_then(|m| m.get("agentTimestampMs"))
            .and_then(|v| v.as_i64().or_else(|| v.as_u64().map(|u| u as i64)))
            .unwrap_or(0);
        let update_type = meta
            .and_then(|m| m.get("updateType"))
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        let chunk_index = meta
            .and_then(|m| m.get("chunkIndex"))
            .and_then(|v| v.as_u64());
        tracing::info!(
            target: "acp_event",
            event = "agent_message_sent",
            event_id = %event_id,
            session_id = %self.session_info.id,
            agent_timestamp_ms = agent_timestamp_ms,
            update_type = %update_type,
            chunk_index = ?chunk_index,
            "Sending session update"
        );
    }
    pub(crate) async fn emit_notification_direct(
        &self,
        mut notification: acp::SessionNotification,
    ) {
        crate::util::event_id::ensure_event_id_meta(
            &self.session_info.id.0,
            &mut notification.meta,
        );
        self.log_outbound_notification(&notification);
        if !matches!(
            notification.update,
            acp::SessionUpdate::AvailableCommandsUpdate(_)
        ) {
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Update(
                    crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
                ));
        }
        let suppress_live_user_echo =
            matches!(notification.update, acp::SessionUpdate::UserMessageChunk(_))
                && !self.notifications.live_user_message_echo()
                && !notification
                    .meta
                    .as_ref()
                    .and_then(|m| m.get("isReplay"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
        if self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
            && !suppress_live_user_echo
        {
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }
    }
    /// Send a notification to the live client without persisting it.
    /// Use this for cosmetic/transient UI updates that should NOT be replayed on session reload.
    /// The underlying resource state is the source of truth; this only adjusts what the live client sees right now.
    pub(super) fn emit_transient_notification(&self, notification: acp::SessionNotification) {
        self.log_outbound_notification(&notification);
        if self
            .notifications
            .gateway_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.notifications
                .gateway
                .forward_fire_and_forget(notification);
        }
    }
    /// Skips the rewind-window close and notification hooks.
    /// Must not stamp an `eventId`: a reconnect cursor that points at an id absent from `updates.jsonl` never resolves and forces a full replay.
    /// Timestamp-only meta keeps the client clock without advancing the cursor.
    pub(super) fn send_xai_notification_transient(&self, update: XaiSessionUpdate) {
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: Some(serde_json::json!({
                "agentTimestampMs": chrono::Utc::now().timestamp_millis(),
            })),
        };
        let params = serde_json::to_value(&notification)
            .and_then(|v| serde_json::value::to_raw_value(&v))
            .ok();
        if let (Some(params), true) = (
            params,
            self.notifications
                .gateway_enabled
                .load(std::sync::atomic::Ordering::Relaxed),
        ) {
            self.notifications
                .gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/session_notification",
                    params.into(),
                ));
        }
    }
    pub(super) async fn ensure_session_disk_writable(&self) -> Result<(), acp::Error> {
        if !self.notifications.is_disk_full() {
            return Ok(());
        }
        let (respond_to, response) = tokio::sync::oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::ProbeWritable { respond_to })
            .is_err()
        {
            return self.disk_full_acp_error(None);
        }
        match response.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => self.disk_full_acp_error(Some(&error)),
            Err(_) => self.disk_full_acp_error(None),
        }
    }
    pub(super) fn disk_full_acp_error(
        &self,
        flush_error: Option<&std::io::Error>,
    ) -> Result<(), acp::Error> {
        use crate::session::persistence::{io_error_to_acp, is_disk_full_io_error};
        match flush_error {
            Some(error) if is_disk_full_io_error(error) => Err(io_error_to_acp(error)),
            _ if self.notifications.is_disk_full() => Err(io_error_to_acp(&std::io::Error::from(
                std::io::ErrorKind::StorageFull,
            ))),
            _ => Ok(()),
        }
    }
    /// Flush buffered notifications and drain the persistence merge buffer to disk.
    /// Blocks until the persistence actor confirms the write is complete.
    /// Must NOT be called from within `run_session()`: the flush goes through `event_tx`, which the same select loop consumes (deadlock, 5s timeout).
    #[tracing::instrument(
        name = "session.flush_to_disk",
        skip_all,
        fields(session_id = %self.session_info.id.0)
    )]
    pub(super) async fn flush_to_disk(&self) -> std::io::Result<()> {
        if let Err(e) = crate::session::replay_events::flush_replay_actor(&self.event_tx).await {
            tracing::warn!(?e, "flush_replay_actor failed");
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck { respond_to: tx })
            .is_err()
        {
            return Ok(());
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "session persistence actor stopped before flush acknowledgement",
            )),
        }
    }
    /// Extracts the update type name and relevant parameters for logging
    fn extract_update_info(
        update: &acp::SessionUpdate,
    ) -> (Option<String>, Option<serde_json::Value>) {
        match update {
            acp::SessionUpdate::UserMessageChunk(_) => (Some("UserMessageChunk".to_string()), None),
            acp::SessionUpdate::AgentMessageChunk(_) => {
                (Some("AgentMessageChunk".to_string()), None)
            }
            acp::SessionUpdate::AgentThoughtChunk(_) => {
                (Some("AgentThoughtChunk".to_string()), None)
            }
            acp::SessionUpdate::ToolCall(tool_call) => (
                Some("ToolCall".to_string()),
                Some(json!({
                    "toolCallId": tool_call.tool_call_id.0,
                    "title": tool_call.title,
                    "kind": format!("{:?}", tool_call.kind),
                    "status": format!("{:?}", tool_call.status),
                })),
            ),
            acp::SessionUpdate::ToolCallUpdate(tool_update) => (
                Some("ToolCallUpdate".to_string()),
                Some(json!({
                    "toolCallId": tool_update.tool_call_id.0,
                    "status": tool_update.fields.status.as_ref().map(|s| format!("{:?}", s)),
                })),
            ),
            acp::SessionUpdate::Plan(plan) => (
                Some("Plan".to_string()),
                Some(json!({
                    "planSteps": plan.entries.len(),
                })),
            ),
            acp::SessionUpdate::AvailableCommandsUpdate(update) => (
                Some("AvailableCommandsUpdate".to_string()),
                Some(json!({
                    "commandsCount": update.available_commands.len(),
                })),
            ),
            acp::SessionUpdate::CurrentModeUpdate(update) => (
                Some("CurrentModeUpdate".to_string()),
                Some(json!({
                    "currentModeId": update.current_mode_id,
                })),
            ),
            _ => (None, None),
        }
    }
    /// Generates a unique event ID for correlation across agent/relay/client
    fn generate_event_id(&self) -> String {
        crate::util::event_id::generate_event_id(&self.session_info.id.0)
    }
    /// Builds notification meta with event ID and timestamp.
    /// Use this for all notifications (including user message chunks) so the relay deduplicates on a consistent event ID format.
    pub(super) fn build_notification_meta(&self) -> serde_json::Value {
        let event_id = self.generate_event_id();
        let agent_timestamp_ms = chrono::Utc::now().timestamp_millis();
        json!({
            "eventId": event_id,
            "agentTimestampMs": agent_timestamp_ms,
        })
    }
    /// Handle Grok session notifications: store them in persistence.
    /// These are client-side events (like diff reviews) that should be part of session history.
    /// Exception: `SubagentProgress` ticks are transient and return before the store.
    pub(super) async fn handle_xai_session_notification(
        &self,
        mut notification: XaiSessionNotification,
    ) {
        if !matches!(
            notification.update,
            XaiSessionUpdate::SubagentProgress { .. }
        ) {
            tracing::debug!("storing xAI session notification");
        }
        {
            let mut meta_map = notification.meta.take().and_then(|v| match v {
                serde_json::Value::Object(m) => Some(m),
                _ => None,
            });
            crate::util::event_id::ensure_event_id_meta(&self.session_info.id.0, &mut meta_map);
            notification.meta = meta_map.map(serde_json::Value::Object);
        }
        scrub_inbound_session_summary(&mut notification);
        match &notification.update {
            XaiSessionUpdate::SubagentSpawned {
                subagent_id,
                attempt_id,
                subagent_type,
                description,
                resumed_from,
                model,
                ..
            } => {
                {
                    let goal_id = self
                        .goal_tracker
                        .lock()
                        .snapshot()
                        .map(|o| o.goal_id.clone());
                    let mut records = self.subagent_token_records.lock();
                    let anchor = resumed_from
                        .as_deref()
                        .map(|pid| match records.get(pid) {
                            Some(record) => record.last_cumulative_reported,
                            None => {
                                tracing::debug!(
                                    parent_id = %pid,
                                    subagent_id = %subagent_id,
                                    "resume parent not in token registry; anchoring at 0"
                                );
                                0
                            }
                        })
                        .unwrap_or(0);
                    let record = records
                        .entry(subagent_id.clone())
                        .or_insert_with(|| SubagentTokenRecord::new(anchor));
                    match record.spawn(goal_id, attempt_id.clone(), model.clone()) {
                        SubagentSpawnOutcome::Accepted => {}
                        SubagentSpawnOutcome::Duplicate => {
                            tracing::debug!(
                                subagent_id = %subagent_id,
                                attempt_id,
                                "duplicate subagent spawn; dropped"
                            );
                            return;
                        }
                        SubagentSpawnOutcome::Stale => {
                            tracing::debug!(
                                subagent_id = %subagent_id,
                                attempt_id,
                                active_attempt_id = ?record
                                    .active_attempt
                                    .as_ref()
                                    .and_then(|active| active.attempt_id.as_deref()),
                                "spawn for a different live subagent attempt; dropped"
                            );
                            return;
                        }
                    }
                }
                if self.goal_harness_enabled() {
                    let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                    if !self.goal_runs_on_workflow_engine() {
                        self.drain_goal_updates(current_tokens, DrainPurpose::MidTurn)
                            .await;
                    }
                    let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(
                        &mut self.goal_tracker.lock(),
                        tokens_used,
                        finished_marginal,
                    );
                }
                let envelope = self.fire_hook(
                    distill_hooks::event::HookEventName::SubagentStart,
                    None,
                    distill_hooks::event::HookPayload::SubagentStart {
                        subagent_id: subagent_id.clone(),
                        subagent_type: subagent_type.clone(),
                        description: Some(description.clone()),
                    },
                );
                let hook_registry_snapshot = self.hook_registry.borrow().clone();
                if let Some(registry) = hook_registry_snapshot {
                    let ctx = self.hook_run_ctx();
                    let _ = distill_hooks::dispatcher::dispatch_non_blocking(
                        &registry,
                        distill_hooks::event::HookEventName::SubagentStart,
                        &envelope,
                        &ctx,
                    )
                    .await;
                }
            }
            XaiSessionUpdate::SubagentFinished {
                subagent_id,
                attempt_id,
                child_session_id,
                status,
                error,
                tool_calls,
                turns,
                duration_ms,
                tokens_used,
                output,
                will_wake,
            } => {
                let payload = SubagentFinishPayload {
                    child_session_id: child_session_id.clone(),
                    status: status.clone(),
                    error: error.clone(),
                    tool_calls: *tool_calls,
                    turns: *turns,
                    duration_ms: *duration_ms,
                    tokens_used: *tokens_used,
                    output: output.clone(),
                    will_wake: *will_wake,
                };
                let finish_outcome = {
                    let mut records = self.subagent_token_records.lock();
                    records
                        .get_mut(subagent_id)
                        .map(|record| record.finish(attempt_id.as_deref(), payload))
                };
                match finish_outcome {
                    Some(SubagentFinishOutcome::Accepted) => {
                        let mut tracker = self.goal_tracker.lock();
                        if let Some(o) = tracker.snapshot_mut() {
                            o.live_subagent_tokens = 0;
                            o.live_context_pct = 0;
                            o.live_turn_count = 0;
                            o.live_tool_call_count = 0;
                            o.live_tokens_by_model.clear();
                        }
                    }
                    Some(SubagentFinishOutcome::Correction) => {}
                    Some(SubagentFinishOutcome::Duplicate) => {
                        tracing::debug!(
                            subagent_id = %subagent_id,
                            attempt_id,
                            "duplicate finish for completed subagent attempt; dropped"
                        );
                        return;
                    }
                    Some(SubagentFinishOutcome::Stale) => {
                        tracing::debug!(
                            subagent_id = %subagent_id,
                            attempt_id,
                            "finish for stale subagent attempt; dropped"
                        );
                        return;
                    }
                    None => {
                        tracing::debug!(
                            subagent_id = %subagent_id,
                            attempt_id,
                            "finish for unregistered subagent; persisting legacy lifecycle"
                        );
                    }
                }
                if self.goal_harness_enabled() && self.goal_tracker.lock().snapshot().is_some() {
                    let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                    let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                    let notify = self.goal_notify_sender();
                    notify.emit_goal_updated(
                        &mut self.goal_tracker.lock(),
                        tokens_used,
                        finished_marginal,
                    );
                }
            }
            XaiSessionUpdate::SubagentProgress {
                subagent_id,
                attempt_id,
                turn_count,
                tool_call_count,
                tokens_used,
                context_window_tokens,
                context_usage_pct,
                ..
            } => {
                let goal_id = self
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .map(|o| o.goal_id.clone());
                let progress = {
                    let mut records = self.subagent_token_records.lock();
                    match records.get_mut(subagent_id).map(|record| {
                        record.progress(goal_id.as_deref(), attempt_id.as_deref(), *tokens_used)
                    }) {
                        Some(SubagentProgressOutcome::Accepted {
                            advanced,
                            live_display_tokens,
                        }) => Some((advanced, live_display_tokens)),
                        Some(SubagentProgressOutcome::InactiveGoal) => None,
                        Some(SubagentProgressOutcome::Stale) => {
                            tracing::debug!(
                                subagent_id = %subagent_id,
                                attempt_id,
                                "progress for stale subagent attempt; dropped"
                            );
                            None
                        }
                        None => {
                            tracing::debug!(
                                subagent_id = %subagent_id,
                                "progress tick for unregistered subagent; dropped"
                            );
                            None
                        }
                    }
                };
                if let Some((advanced, ratcheted_tokens)) = progress
                    && self.goal_harness_enabled()
                {
                    let model_id = self
                        .chat_state_handle
                        .get_sampling_config()
                        .await
                        .map(|c| c.model)
                        .unwrap_or_default();
                    let tokens_by_model = self.goal_tokens_by_model(&model_id);
                    self.goal_tracker.lock().update_live_progress(
                        ratcheted_tokens,
                        tokens_by_model,
                        *context_window_tokens,
                        *context_usage_pct,
                        *turn_count,
                        *tool_call_count,
                    );
                    if advanced {
                        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
                        let (goal_tokens_used, finished_marginal) =
                            self.goal_tokens(current_tokens);
                        let notify = self.goal_notify_sender();
                        notify.emit_goal_updated_ephemeral(
                            &mut self.goal_tracker.lock(),
                            goal_tokens_used,
                            finished_marginal,
                        );
                    }
                }
                return;
            }
            _ => {}
        }
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification)),
            ));
    }
    /// Persist a Distill extension notification to `updates.jsonl` **without** sending it to the gateway/UI.
    /// Used for internal bookkeeping updates like `CompactionCheckpoint` and `RewindMarker` that are only relevant during replay.
    pub(super) fn persist_xai_update_only(&self, update: XaiSessionUpdate) {
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: Some(self.build_notification_meta()),
        };
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification)),
            ))
            .is_err()
        {
            tracing::warn!("Failed to send xAI update to persistence channel");
        }
    }
    /// Dispatch a `Notification` hook for a user-attention event.
    pub(super) async fn dispatch_notification_hook(
        &self,
        notification_type: &str,
        message: Option<String>,
        title: Option<String>,
        level: Option<String>,
    ) {
        let envelope = self.fire_hook(
            distill_hooks::event::HookEventName::Notification,
            None,
            distill_hooks::event::HookPayload::Notification {
                notification_type: notification_type.to_string(),
                message,
                title,
                level,
            },
        );
        let hook_registry_snapshot = self.hook_registry.borrow().clone();
        let Some(registry) = hook_registry_snapshot else {
            return;
        };
        let ctx = self.hook_run_ctx();
        let _ = distill_hooks::dispatcher::dispatch_non_blocking(
            &registry,
            distill_hooks::event::HookEventName::Notification,
            &envelope,
            &ctx,
        )
        .await;
    }
    pub(super) async fn dispatch_permission_prompt_notification(&self, message: &str) {
        self.dispatch_notification_hook(
            "permission_prompt",
            Some(message.to_owned()),
            None,
            Some("info".into()),
        )
        .await;
    }
    /// Call only from the session that owns the manager (`owns_permission_manager` at spawn).
    /// Inherited/cloned handles must not call this; first-writer-wins on the handle is the backstop.
    /// `Weak` so the listener cannot keep a dropped session alive.
    pub(super) fn wire_permission_prompt_notification(self: &std::sync::Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        self.permissions.set_user_prompt_notify(tx);
        let session = std::sync::Arc::downgrade(self);
        tokio::task::spawn_local(async move {
            while rx.recv().await.is_some() {
                let Some(session) = session.upgrade() else {
                    break;
                };
                session
                    .dispatch_permission_prompt_notification("Tool permission requested")
                    .await;
            }
        });
    }
    #[tracing::instrument(skip_all)]
    pub(super) async fn send_xai_notification(&self, update: XaiSessionUpdate) {
        self.send_xai_notification_with_extra_meta(
            update,
            None,
            crate::session::storage::jsonl::AppendDurability::Buffered,
        )
        .await;
    }
    /// Build the per-response boundary update, projecting the response's usage
    /// into the Messages API `message.usage` shape (uncached `input_tokens`).
    pub(super) fn response_completed_update(
        &self,
        response: &distill_sampling_types::ConversationResponse,
    ) -> XaiSessionUpdate {
        let usage =
            response
                .usage
                .as_ref()
                .map(|u| crate::extensions::notification::ResponseUsage {
                    input_tokens: u64::from(
                        u.prompt_tokens
                            .saturating_sub(u.cached_prompt_tokens)
                            .saturating_sub(u.cache_creation_prompt_tokens),
                    ),
                    output_tokens: u64::from(u.completion_tokens),
                    cache_read_input_tokens: u64::from(u.cached_prompt_tokens),
                    cache_creation_input_tokens: u64::from(u.cache_creation_prompt_tokens),
                    reasoning_tokens: u64::from(u.reasoning_tokens),
                });
        let signature = response
            .reasoning_items()
            .find_map(|r| r.encrypted_content.clone());
        XaiSessionUpdate::ResponseCompleted {
            message_id: response.message_id.clone(),
            stop_reason: response.raw_stop_reason.clone(),
            usage,
            signature,
            stop_sequence: response.stop_sequence.clone(),
        }
    }
    /// [`Self::send_xai_notification`] with caller-supplied `_meta` keys merged into the standard eventId/timestamp meta.
    /// `durability` picks the persistence rail: `Buffered` rides the merge buffer (page cache until the next barrier).
    /// `Durable` drains pending updates first and fsyncs the record when written.
    #[tracing::instrument(skip_all)]
    pub(super) async fn send_xai_notification_with_extra_meta(
        &self,
        update: XaiSessionUpdate,
        extra_meta: Option<serde_json::Map<String, serde_json::Value>>,
        durability: crate::session::storage::jsonl::AppendDurability,
    ) {
        if closes_cancel_rewind_window(&update) {
            self.close_rewind_window().await;
        }
        let meta = {
            let mut meta = self.build_notification_meta();
            if let (Some(obj), Some(extra)) = (meta.as_object_mut(), extra_meta) {
                obj.extend(extra);
            }
            meta
        };
        let notification = XaiSessionNotification {
            session_id: self.session_info.id.clone(),
            update,
            meta: Some(meta),
        };
        let persisted_update =
            crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone()));
        match durability {
            crate::session::storage::jsonl::AppendDurability::Buffered => {
                let _ = self
                    .notifications
                    .persistence_tx
                    .send(PersistenceMsg::Update(persisted_update));
            }
            crate::session::storage::jsonl::AppendDurability::Durable => {
                let (respond_to, _) = tokio::sync::oneshot::channel();
                let _ = self.notifications.persistence_tx.send(
                    PersistenceMsg::AppendUpdateDurablyAndAck {
                        update: persisted_update,
                        respond_to,
                    },
                );
            }
        }
        let params = serde_json::to_value(&notification)
            .and_then(|v| serde_json::value::to_raw_value(&v))
            .ok();
        if let Some(params) = params {
            let ext_notification =
                acp::ExtNotification::new("x.ai/session_notification", params.into());
            self.notifications
                .gateway
                .forward_fire_and_forget(ext_notification);
        }
        if let Some((notification_type, message, title, level)) =
            notification_hook_for_update(&notification.update)
        {
            self.dispatch_notification_hook(&notification_type, message, title, level)
                .await;
        }
    }
}
#[cfg(test)]
mod xai_event_id_stamping_tests {
    use super::support::create_test_actor;
    use super::*;
    fn persisted_xai_event_id(
        prx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
    ) -> String {
        loop {
            match prx.try_recv().expect("a Distill line must be persisted") {
                PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                    return notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .expect("persisted xAI lines must carry an eventId")
                        .to_string();
                }
                _ => continue,
            }
        }
    }
    /// The actor is the chokepoint where every persisted line gets an `eventId`.
    /// Both persist paths must stamp: `send_xai_notification` (own emission) and `handle_xai_session_notification` (inbound/forwarded, meta-less).
    /// An id-less line degrades every later cursor reconnect of the session to a full replay.
    #[tokio::test]
    async fn actor_persisted_xai_lines_carry_event_id() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .send_xai_notification(XaiSessionUpdate::HookAnnotation {
                        message: "own emission".into(),
                        kind: Default::default(),
                    })
                    .await;
                let own_id = persisted_xai_event_id(&mut prx);
                assert!(own_id.starts_with("test-actor-"));
                actor
                    .handle_xai_session_notification(XaiSessionNotification {
                        session_id: acp::SessionId::new("test-actor"),
                        update: XaiSessionUpdate::HookAnnotation {
                            message: "inbound".into(),
                            kind: Default::default(),
                        },
                        meta: None,
                    })
                    .await;
                let inbound_id = persisted_xai_event_id(&mut prx);
                assert!(inbound_id.starts_with("test-actor-"));
                assert_ne!(own_id, inbound_id);
                actor.persist_xai_update_only(XaiSessionUpdate::HookAnnotation {
                    message: "persist-only".into(),
                    kind: Default::default(),
                });
                let persist_only_id = persisted_xai_event_id(&mut prx);
                assert!(persist_only_id.starts_with("test-actor-"));
                assert_ne!(inbound_id, persist_only_id);
            })
            .await;
    }
    /// `emit_notification_direct` is the actor's ACP persist/broadcast fork: it must stamp any direct caller that didn't stamp at enqueue.
    /// None exist today; this is the safety net for the next one, so every persisted ACP line stays cursor-addressable.
    #[tokio::test]
    async fn emit_notification_direct_stamps_unstamped_acp_lines() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .emit_notification_direct(acp::SessionNotification::new(
                        acp::SessionId::new("test-actor"),
                        acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
                            acp::SessionModeId::new("plan"),
                        )),
                    ))
                    .await;
                match prx.try_recv().expect("must persist") {
                    PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => {
                        assert!(
                            notif
                                .meta
                                .as_ref()
                                .and_then(|m| m.get("eventId"))
                                .and_then(|v| v.as_str())
                                .is_some_and(|id| id.starts_with("test-actor-")),
                            "the chokepoint must stamp meta-less ACP notifications"
                        );
                    }
                    _ => panic!("expected Acp update"),
                }
            })
            .await;
    }
    fn user_chunk_notification(replay: bool) -> acp::SessionNotification {
        let update = acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
            acp::ContentBlock::Text(acp::TextContent::new("hi")),
        ));
        let n = acp::SessionNotification::new(acp::SessionId::new("test-actor"), update);
        if replay {
            n.meta(serde_json::json!({ "isReplay": true }).as_object().cloned())
        } else {
            n
        }
    }
    #[tokio::test]
    async fn live_user_message_chunk_persists_without_forward_when_echo_cap_is_off() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .notifications
                    .gateway_enabled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                actor
                    .notifications
                    .client_caps
                    .user_message_echo
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                actor
                    .emit_notification_direct(user_chunk_notification(false))
                    .await;
                assert!(
                    matches!(
                        prx.try_recv(),
                        Ok(PersistenceMsg::Update(
                            crate::session::storage::SessionUpdate::Acp(_)
                        ))
                    ),
                    "persist-only echo still writes updates.jsonl"
                );
                assert!(
                    gateway_rx.try_recv().is_err(),
                    "vanilla ACP clients must not receive a live user-message echo"
                );
            })
            .await;
    }
    #[tokio::test]
    async fn live_user_message_chunk_forwards_when_echo_cap_is_on() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, _prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .notifications
                    .gateway_enabled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                actor
                    .notifications
                    .client_caps
                    .user_message_echo
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                actor
                    .emit_notification_direct(user_chunk_notification(false))
                    .await;
                assert!(matches!(
                    gateway_rx.try_recv(),
                    Ok(distill_acp_lib::AcpClientMessage::SessionNotification(_))
                ));
            })
            .await;
    }
    #[tokio::test]
    async fn replay_user_message_chunk_forwards_when_echo_cap_is_off() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, mut gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, _prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
                actor
                    .notifications
                    .gateway_enabled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                actor
                    .notifications
                    .client_caps
                    .user_message_echo
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                actor
                    .emit_notification_direct(user_chunk_notification(true))
                    .await;
                assert!(matches!(
                    gateway_rx.try_recv(),
                    Ok(distill_acp_lib::AcpClientMessage::SessionNotification(_))
                ));
            })
            .await;
    }
    /// Mid-stream plan toggle: the plan-mode `CurrentModeUpdate` must ride the FIFO event pipeline BEHIND already-queued chunks.
    /// Pins the enter AND exit legs of `handle_session_mode`; each leg must emit.
    /// The abandoned site shares the same helper but needs an `ext_method` round-trip harness to drive, so it is not pinned here.
    #[tokio::test]
    async fn plan_mode_current_mode_update_rides_event_pipeline_in_id_order() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (gateway_tx, _gateway_rx) =
                    tokio::sync::mpsc::unbounded_channel::<distill_acp_lib::AcpClientMessage>();
                let (persistence_tx, mut prx) =
                    tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
                let (actor, mut event_rx) = super::support::create_test_actor_ex(
                    0,
                    256_000,
                    85,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
                actor
                    .send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new("queued text")),
                        )),
                        None,
                    )
                    .await;
                actor
                    .handle_session_mode(acp::SessionModeId::new("plan"))
                    .await;
                while let Ok(msg) = prx.try_recv() {
                    assert!(
                        !matches!(msg, PersistenceMsg::Update(_)),
                        "the mode update must not short-circuit the event queue"
                    );
                }
                let mut queued = Vec::new();
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        SessionEvent::Notification(n) => queued.push(n),
                        other => {
                            panic!("expected only Notification events, got {other:?}")
                        }
                    }
                }
                assert_eq!(queued.len(), 2, "chunk + mode update must be queued");
                let [_, mode_update] = queued.as_slice() else {
                    panic!("chunk + mode update must be queued: {queued:?}");
                };
                match mode_update {
                    SessionNotification::Acp(n) => {
                        assert!(matches!(n.update, acp::SessionUpdate::CurrentModeUpdate(_)));
                        assert!(
                            n.meta.as_ref().and_then(|m| m.get("eventId")).is_some(),
                            "the queued mode update must already carry its enqueue-time id"
                        );
                    }
                    other => {
                        panic!("expected the mode update behind the chunk, got {other:?}")
                    }
                }
                let mut replay_buffer = crate::agent::update_chunk_merge::ReplayBuffer::new(
                    actor.buffering_settings.clone(),
                );
                for notification in queued {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                let numeric_seq = |n: &acp::SessionNotification| -> u64 {
                    n.meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .and_then(|id| id.rsplit('-').next())
                        .and_then(|s| s.parse().ok())
                        .expect("persisted ACP lines must carry a numeric eventId")
                };
                let mut persisted = Vec::new();
                while let Ok(msg) = prx.try_recv() {
                    if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n)) =
                        msg
                    {
                        persisted.push(*n);
                    }
                }
                assert_eq!(persisted.len(), 2, "both lines must persist on drain");
                let [chunk, mode] = persisted.as_slice() else {
                    panic!("both lines must persist on drain: {persisted:?}");
                };
                assert!(matches!(
                    chunk.update,
                    acp::SessionUpdate::AgentMessageChunk(_)
                ));
                assert!(matches!(
                    mode.update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    numeric_seq(chunk) < numeric_seq(mode),
                    "delivery order must match id order — the dedup premise"
                );
                actor
                    .send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new("queued before exit")),
                        )),
                        None,
                    )
                    .await;
                actor
                    .handle_session_mode(acp::SessionModeId::new("default"))
                    .await;
                while let Ok(msg) = prx.try_recv() {
                    assert!(
                        !matches!(msg, PersistenceMsg::Update(_)),
                        "the exit mode update must not short-circuit the event queue"
                    );
                }
                let mut queued = Vec::new();
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        SessionEvent::Notification(n) => queued.push(n),
                        other => {
                            panic!("expected only Notification events, got {other:?}")
                        }
                    }
                }
                assert_eq!(queued.len(), 2, "chunk + exit mode update must be queued");
                let [_, mode_update] = queued.as_slice() else {
                    panic!("chunk + mode update must be queued: {queued:?}");
                };
                match mode_update {
                    SessionNotification::Acp(n) => match &n.update {
                        acp::SessionUpdate::CurrentModeUpdate(cmu) => {
                            assert_eq!(
                                cmu.current_mode_id.0.as_ref(),
                                "default",
                                "the exit emission must carry the new mode id"
                            );
                            assert!(
                                n.meta.as_ref().and_then(|m| m.get("eventId")).is_some(),
                                "the queued exit mode update must carry its enqueue-time id"
                            );
                        }
                        other => panic!("expected CurrentModeUpdate, got {other:?}"),
                    },
                    other => {
                        panic!("expected the mode update behind the chunk, got {other:?}")
                    }
                }
                for notification in queued {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                let mut persisted = Vec::new();
                while let Ok(msg) = prx.try_recv() {
                    if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n)) =
                        msg
                    {
                        persisted.push(*n);
                    }
                }
                let [first, second] = persisted.as_slice() else {
                    panic!("exit leg must persist both lines: {persisted:?}");
                };
                assert!(matches!(
                    second.update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    numeric_seq(first) < numeric_seq(second),
                    "exit-leg delivery order must match id order too"
                );
            })
            .await;
    }
}
