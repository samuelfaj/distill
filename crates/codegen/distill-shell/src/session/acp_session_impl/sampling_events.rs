// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

impl SessionActor {
    async fn record_sampler_retry_usage(
        &self,
        request_id: &distill_sampler::RequestId,
        attempt: u32,
        kind: distill_sampler::SamplingErrorKind,
        captured: Option<UsageAttemptContext>,
    ) {
        let mut context = captured.unwrap_or_else(|| UsageAttemptContext {
            attempt_id: format!("sampler:{}", request_id.as_str()),
            task_id: None,
            turn_id: None,
            request_id: None,
            role: "main_retry".to_owned(),
            model_id: "<unknown>".to_owned(),
            endpoint: None,
            requested_effort: None,
            applied_effort: None,
        });
        context.attempt_id = format!(
            "{}:retry:{}:{attempt}",
            context.attempt_id,
            kind.as_ref()
        );
        // This is a physical sampler retry, not another foreground loop round.
        // Keep it in the aggregate bill while the ledger's `main` role remains
        // reserved for the round that owns `numTurns`.
        context.role = "main_retry".to_owned();
        self.chat_state_handle.record_usage_attribution(
            context.into_attribution(
                distill_chat_state::UsageCallStatus::Failed,
                None,
                false,
                None,
                None,
            ),
            true,
        );
    }

    async fn send_thought_chunk(&self, text: String, chunk_index: u64) {
        self.send_update(
            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new(text),
            ))),
            Some(chunk_index),
        )
        .await;
    }

    /// Stamp ttft on the first token of any channel (reasoning, text, or tool call).
    pub(crate) fn record_turn_first_token(&self, request_id: Option<&distill_sampler::RequestId>) {
        let Some(generation) = self.turn_generation(request_id) else {
            return;
        };
        self.turn_phases.record_first_token(generation);
    }

    /// Stamp ttfm on the first assistant text; reasoning and tool calls are excluded.
    pub(crate) fn record_turn_first_meaningful_output(
        &self,
        request_id: Option<&distill_sampler::RequestId>,
    ) {
        let Some(generation) = self.turn_generation(request_id) else {
            return;
        };
        self.turn_phases.record_first_meaningful_output(generation);
    }

    fn turn_generation(&self, request_id: Option<&distill_sampler::RequestId>) -> Option<u64> {
        match request_id {
            Some(id) => self
                .turn_stream_drained
                .lock()
                .get(id)
                .map(|ownership| ownership.generation),
            None => Some(self.turn_phases.current_generation()),
        }
    }

    fn open_stream_apply_span(&self, request_id: &distill_sampler::RequestId) {
        let parent_id = self.current_turn_span_id.lock().clone();
        let span = match parent_id {
            Some(id) => tracing::info_span!(
                parent: id,
                "turn.stream_apply",
                chunk_count = tracing::field::Empty,
                bytes = tracing::field::Empty,
            ),
            None => tracing::info_span!(
                "turn.stream_apply",
                chunk_count = tracing::field::Empty,
                bytes = tracing::field::Empty,
            ),
        };
        let region = distill_telemetry::region::Region::from_span(span);
        let prior = self.stream_apply_span.lock().replace(StreamApplySpan {
            request_id: request_id.clone(),
            region,
            chunk_count: 0,
            bytes: 0,
        });
        if let Some(prior) = prior {
            prior.record_and_close();
        }
    }

    fn bump_stream_apply_span(&self, request_id: &distill_sampler::RequestId, bytes: usize) {
        let mut guard = self.stream_apply_span.lock();
        if let Some(span) = guard.as_mut()
            && &span.request_id == request_id
        {
            span.chunk_count += 1;
            span.bytes += bytes as i64;
        }
    }

    fn close_stream_apply_span(&self, request_id: &distill_sampler::RequestId) {
        let ended = {
            let mut guard = self.stream_apply_span.lock();
            if guard.as_ref().is_some_and(|s| &s.request_id == request_id) {
                guard.take()
            } else {
                None
            }
        };
        if let Some(ended) = ended {
            ended.record_and_close();
        }
    }

    pub(crate) fn close_stream_apply_span_any(&self) {
        let ended = self.stream_apply_span.lock().take();
        if let Some(ended) = ended {
            ended.record_and_close();
        }
    }

    /// Translate one [`distill_sampler::SamplingEvent`] from the per-session sampler actor into the corresponding ACP / shell side-effects.
    /// Called from the drainer task spawned in `spawn_session_actor`, which loops `while let Some(event) = sampler_event_rx.recv().await`.
    /// Recovery runs there because it needs per-turn state and may call back into `sampler_handle.update_config` or resubmit.
    pub(crate) async fn handle_sampling_event(
        self: &Arc<Self>,
        event: distill_sampler::SamplingEvent,
    ) {
        use distill_sampler::{SamplingChannel, SamplingEvent};

        let request_owned = self
            .turn_stream_drained
            .lock()
            .contains_key(event.request_id());
        let owns_pending_strip = self
            .pending_image_strip
            .lock()
            .contains_key(event.request_id());
        // Presence in `turn_stream_drained` means the turn still owns every FIFO event for this request.
        // `None` only means the ordering waiter timed out; queued chunks stay valid until the terminal event or a turn boundary removes the entry.
        // A pending image strip admits only its own strip and terminal events.
        let closes_backend_tool = matches!(event, SamplingEvent::BackendToolCallCompleted { .. });
        let resolves_pending_strip = match &event {
            SamplingEvent::ImagesStripped {
                stripped_urls,
                reason,
                ..
            } => owns_pending_strip && Self::should_defer_image_strip(stripped_urls, reason),
            SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. } => owns_pending_strip,
            _ => false,
        };
        if matches!(
            event,
            SamplingEvent::Completed { .. }
                | SamplingEvent::Failed { .. }
                | SamplingEvent::Retrying { .. }
        ) {
            self.close_stream_apply_span(event.request_id());
        }
        if !request_owned && !closes_backend_tool && !resolves_pending_strip {
            return;
        }

        match event {
            SamplingEvent::StreamStarted {
                request_id,
                timestamp_ms,
            } => {
                // Begin a fresh per-generation segment A new turn (the prompt id changed) resets the whole accumulator, so a capture from an earlier turn cannot leak into this trace A same-turn restart, a doomloop's next.
                // That way every generation survives instead of only the last `current_prompt_id` / `current_turn_number` are set by the prompt handler before any sampler events arrive.
                // Panic on lock poison to match the file convention.
                {
                    let prompt_id = self
                        .current_prompt_id
                        .lock()
                        .expect("current_prompt_id mutex poisoned")
                        .clone();
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.as_deref() != prompt_id.as_deref() {
                        cap.begin_turn(prompt_id, self.current_turn_number.get());
                    }
                    cap.start_request_stream(request_id.as_str(), timestamp_ms);
                }
                self.chat_state_handle.record_stream_start(timestamp_ms);
                self.open_stream_apply_span(&request_id);
            }
            SamplingEvent::FirstToken { .. } => {
                self.emit_event(crate::session::events::Event::FirstToken);
            }
            SamplingEvent::ChannelToken {
                request_id,
                channel,
                text,
                chunk_index,
            } => match channel {
                SamplingChannel::Text => {
                    self.bump_stream_apply_span(&request_id, text.len());
                    // Append to the out-of-band trace accumulator; it never enters chat_state
                    // See `StreamingTurnCapture` for how the capture begins and ends
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            // `StreamStarted` was dropped; count this generation so `attempt_count` matches the path where `StreamStarted` arrived
                            // No timestamp is available, so none is stamped
                            cap.attempt_count += 1;
                        }
                        cap.claim_current_request(request_id.as_str());
                        cap.append(false, &text);
                    }

                    self.record_turn_first_token(Some(&request_id));
                    self.record_turn_first_meaningful_output(Some(&request_id));

                    // The phase change is emitted alongside each text delta so the UI flips to "streaming text" the moment content starts arriving
                    // The `PhaseChanged` event itself is idempotent on the consumer side
                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingText,
                    });
                    self.send_update(
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text)),
                        )),
                        Some(chunk_index),
                    )
                    .await;
                }
                SamplingChannel::Reasoning => {
                    self.bump_stream_apply_span(&request_id, text.len());
                    // Append to the out-of-band trace accumulator; it never enters chat_state
                    {
                        let mut cap = self.streaming_turn_capture.lock();
                        if cap.prompt_id.is_none() {
                            let prompt_id = self
                                .current_prompt_id
                                .lock()
                                .expect("current_prompt_id mutex poisoned")
                                .clone();
                            cap.begin_turn(prompt_id, self.current_turn_number.get());
                            // `StreamStarted` was dropped; count this generation so `attempt_count` matches the path where `StreamStarted` arrived
                            // No timestamp is available, so none is stamped
                            cap.attempt_count += 1;
                        }
                        cap.claim_current_request(request_id.as_str());
                        cap.append(true, &text);
                    }

                    // Reasoning is a token (ttft) but not meaningful output (ttfm).
                    self.record_turn_first_token(Some(&request_id));

                    self.emit_event(crate::session::events::Event::PhaseChanged {
                        phase: crate::session::events::Phase::StreamingReasoning,
                    });
                    self.send_thought_chunk(text, chunk_index).await;
                }
            },
            SamplingEvent::ToolCallDelta {
                request_id,
                tool_index,
                id,
                name,
                arguments_delta,
            } => {
                // Mark the capture's phase so a partial taken now records it was cut off mid tool call rather than mid reasoning or response
                // Mark only an already-active capture: a tool-call-only turn has no reasoning or text, so its capture stays empty and never uploads
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    if cap.prompt_id.is_some() {
                        cap.claim_current_request(request_id.as_str());
                        cap.phase = CapturePhase::ToolCall;
                    }
                }

                // A tool call is a token (ttft) but not meaningful text output (ttfm).
                self.record_turn_first_token(Some(&request_id));

                self.send_buffered_xai_update(XaiSessionUpdate::ToolCallDeltaChunk {
                    tool_call_id: id,
                    tool_index,
                    name,
                    arguments_delta,
                })
                .await;
            }
            SamplingEvent::ResponseStarted {
                message_id,
                model,
                input_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            } => {
                // Ride the buffered chunk rail (the same FIFO `event_tx` as `send_update`) so this lands ahead of the response's first agent chunk
                // That lets partial framing in headless mode emit the real `message_start` id and input usage in order
                self.send_buffered_xai_update(XaiSessionUpdate::ResponseStarted {
                    message_id: Some(message_id),
                    model: Some(model),
                    input_tokens,
                    cache_read_input_tokens,
                    cache_creation_input_tokens,
                })
                .await;
            }
            SamplingEvent::ReasoningCompleted { signature, .. } => {
                // Ride the buffered chunk rail so this lands right after the response's thought chunks and before its text
                // That lets partial framing in headless mode emit `signature_delta` before the thinking block's `content_block_stop`
                self.send_buffered_xai_update(XaiSessionUpdate::ReasoningCompleted {
                    signature: Some(signature),
                })
                .await;
            }
            SamplingEvent::DoomLoopSignals {
                request_id,
                triggers,
            } => {
                let matches_active_turn = self.turn_stream_drained.lock().contains_key(&request_id);
                if matches_active_turn {
                    self.doom_loop_turn_tally
                        .lock()
                        .merge_all_triggers(&triggers);
                }
            }
            SamplingEvent::Completed {
                request_id,
                response,
                metrics: _,
            } => {
                let request_updates_turn = request_owned;

                // Persist before the drain waiter is released so the next prompt cannot
                // reread the rejected image, and LocalSet shutdown cannot abort the write.
                self.apply_pending_image_strip(&request_id).await;
                // The awaited result is the authoritative source for which doom-loop signals fired
                // This merge on the event side keeps direct-event tests working, and it is request-bound so a late event cannot enter the next turn
                if request_updates_turn {
                    let all_triggers: Vec<String> = response
                        .doom_loop_signals
                        .iter()
                        .map(|signal| signal.raw.clone())
                        .collect();
                    self.doom_loop_turn_tally
                        .lock()
                        .merge_all_triggers(&all_triggers);
                }

                // Telemetry: a completed response still carrying confident doom-loop signals after a resample counts as accepted after budget
                // With no prior resample (attempts 0, the observe-only `max_retries = 0` policy) nothing was discarded
                // The signals then only warn: no counter, no analytics event, no capture stamp
                if request_updates_turn && let Some(policy) = self.doom_loop_recovery {
                    let triggers = policy.confident_triggers(&response.doom_loop_signals);
                    if !triggers.is_empty() {
                        let (attempts, should_count, should_stamp) = {
                            let mut tally = self.doom_loop_turn_tally.lock();
                            let attempts = tally.recovery_attempt_count(request_id.as_str());
                            if attempts == 0 {
                                (0, false, false)
                            } else {
                                let count = tally.mark_accepted_request(request_id.as_str());
                                if count {
                                    tally.merge_recovery_triggers(&triggers);
                                }
                                let stamp =
                                    tally.mark_accepted_request_stamped(request_id.as_str());
                                (attempts, count, stamp)
                            }
                        };
                        if should_stamp {
                            // Stamped BEFORE `clear_current_segment` below, which folds a text-free stamped segment for the trace
                            self.streaming_turn_capture.lock().stamp_request_doom_loop(
                                request_id.as_str(),
                                crate::session::streaming_capture::DoomLoopSegmentStamp {
                                    doom_loop_triggers: triggers.clone(),
                                    attempt: attempts.saturating_add(1),
                                    aborted_at_chunk: None,
                                    action: "accepted_after_budget".to_string(),
                                },
                            );
                        }
                        if should_count {
                            self.signals_handle()
                                .record_doom_loop_accepted_after_budget(triggers);
                        }
                    }
                }

                // The canonical assistant response is being committed via `record_assistant_response` in `process_conversation_turn`.
                // Discard the in-progress generation rather than wiping the whole capture A same-turn doomloop generation must not erase earlier uncommitted ones.
                // Its reasoning must not enter `segments` or count against the byte cap of later ones A terminal event admitted only for a pending strip owns no stream and must not touch the partial capture kept for turn reporting.
                if request_updates_turn {
                    self.streaming_turn_capture
                        .lock()
                        .clear_request_segment(request_id.as_str());
                }

                // This ordered rail only mutates the capture and releases the terminal barrier.
                // Release only after the terminal event is fully processed.
                // FIFO ordering then guarantees all preceding chunks and detector signals are visible before turn teardown proceeds.
                let sender = self
                    .turn_stream_drained
                    .lock()
                    .remove(&request_id)
                    .and_then(|ownership| ownership.waiter);
                if let Some(tx) = sender {
                    let _ = tx.send(());
                }
            }
            SamplingEvent::ModelMetadata { metadata, .. } => {
                self.handle_model_metadata_update(metadata).await;
            }
            SamplingEvent::ImagesStripped {
                request_id,
                stripped_urls,
                reason,
            } => {
                // Policy lives in `acp_session_impl/image_strip.rs`.
                self.handle_images_stripped(request_id, stripped_urls, reason)
                    .await;
            }
            SamplingEvent::Retrying {
                request_id,
                attempt,
                max_retries,
                kind,
                reason,
                doom_loop_triggers,
                doom_loop_aborted_at_chunk,
            } => {
                if !self.turn_stream_drained.lock().contains_key(&request_id) {
                    return;
                }
                let captured = self
                    .turn_stream_drained
                    .lock()
                    .get(&request_id)
                    .and_then(|ownership| ownership.usage_context.clone());
                self.record_sampler_retry_usage(&request_id, attempt, kind, captured)
                    .await;
                if kind == distill_sampler::SamplingErrorKind::DoomLoopDetected {
                    let triggers = doom_loop_triggers.unwrap_or_default();
                    let (should_count, should_stamp) = {
                        let mut tally = self.doom_loop_turn_tally.lock();
                        let count =
                            tally.record_recovery_attempt(request_id.as_str(), attempt, &triggers);
                        let stamp =
                            tally.mark_recovery_attempt_stamped(request_id.as_str(), attempt);
                        (count, stamp)
                    };
                    if should_stamp {
                        // The ordered event rail owns attaching doom-loop stamps to capture segments
                        self.streaming_turn_capture.lock().stamp_request_doom_loop(
                            request_id.as_str(),
                            crate::session::streaming_capture::DoomLoopSegmentStamp {
                                doom_loop_triggers: triggers.clone(),
                                attempt,
                                aborted_at_chunk: doom_loop_aborted_at_chunk,
                                action: "resampled".to_string(),
                            },
                        );
                    }
                    if should_count {
                        self.signals_handle().record_doom_loop_recovery_attempt(
                            triggers,
                            doom_loop_aborted_at_chunk,
                        );
                    }
                }
                distill_telemetry::unified_log::warn(
                    "shell.turn.inference_retry",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "sampler_request_id": request_id.as_str(),
                        "attempt": attempt,
                        "max_retries": max_retries,
                        "kind": kind.as_ref(),
                        "reason": crate::util::truncate(&reason, 300),
                    })),
                );
                self.send_xai_notification(XaiSessionUpdate::RetryState(
                    crate::extensions::notification::RetryState::Retrying {
                        attempt,
                        max_retries,
                        reason,
                        error_type: Some(kind.as_ref().to_string()),
                    },
                ))
                .await;
            }
            SamplingEvent::Failed { request_id, error } => {
                // Persist before releasing the drain waiter / waking the turn.
                self.apply_pending_image_strip(&request_id).await;
                if !request_owned {
                    self.turn_stream_drained.lock().remove(&request_id);
                    return;
                }
                // This arm only records telemetry
                // The terminal error fires through `submit_and_collect`'s Result branch
                // The turn loop's `handle_sampling_failure` decides whether to compact or show a friendly message
                distill_telemetry::unified_log::error(
                    "shell.turn.inference_failed",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "sampler_request_id": request_id.as_str(),
                        "kind": error.kind.as_ref(),
                        "status_code": error.status_code,
                        "is_retryable": error.is_retryable,
                        "message": crate::util::truncate(&error.message, 300),
                    })),
                );
                self.signals_handle()
                    .record_error_typed(error.kind.as_ref());
                if let Some(ref ctx) = error.empty_response_context {
                    tracing::info!(
                        empty_response = true,
                        empty_reason = ctx.reason.as_ref(),
                        had_reasoning = ctx.had_reasoning,
                        finish_reason = ctx.finish_reason_str(),
                        model = %ctx.model,
                        "sampler reported empty response (will retry if retryable)",
                    );
                }
                // Terminal failures must flush the same FIFO barrier as completions
                // This keeps preceding detector labels in the current turn even when no response is accepted
                let sender = self
                    .turn_stream_drained
                    .lock()
                    .remove(&request_id)
                    .and_then(|ownership| ownership.waiter);
                if let Some(tx) = sender {
                    let _ = tx.send(());
                }
            }
            // ── Backend-hosted tool progress ─────────────────────
            // These tools are executed server-side by the agentic sampler
            // We emit ACP ToolCall/ToolCallUpdate so the pager can show progress (e.g., "Searching the web…")
            SamplingEvent::BackendToolCallStarted {
                request_id,
                call_id,
                name,
            } => {
                self.signals_handle().record_tool_call(&name);
                // A backend tool start is a token (ttft) but not meaningful text output (ttfm).
                self.record_turn_first_token(Some(&request_id));
                let (title, kind, raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(
                            acp::ToolCallId::new(Arc::from(call_id.as_str())),
                            title,
                        )
                        .kind(kind)
                        .status(acp::ToolCallStatus::InProgress)
                        .content(vec![])
                        .locations(vec![])
                        .raw_input(Some(raw_input))
                        .meta(serde_json::json!({"backend": true}).as_object().cloned()),
                    ),
                    None,
                )
                .await;
            }
            SamplingEvent::BackendToolCallCompleted {
                call_id,
                name,
                result,
                ..
            } => {
                // Propagate the backend call's real success or failure: the payload's `status` decides the ACP terminal status
                // A backend-reported failure lands as `Failed`, reaching the headless `web_search_tool_result_error` branch
                let status = backend_tool_call_status(result.as_ref());
                if request_owned {
                    if status == acp::ToolCallStatus::Failed {
                        self.signals_handle().record_tool_failure(&name);
                    } else {
                        self.signals_handle().record_tool_success(&name);
                    }
                }
                let (title, _kind, _raw_input) = backend_tool_display(&name);
                self.send_update(
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(Arc::from(call_id.as_str())),
                        acp::ToolCallUpdateFields::new()
                            .status(Some(status))
                            .title(Some(title))
                            .raw_output(result),
                    )),
                    None,
                )
                .await;
            }
        }
    }
}

#[cfg(test)]
#[path = "sampling_events_tests.rs"]
mod tests;
