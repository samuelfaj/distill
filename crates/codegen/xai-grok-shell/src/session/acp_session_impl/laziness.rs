//! Laziness (stop-detector) concern for `SessionActor`.

use super::*;

/// Per-fire metadata captured at the top of `maybe_fire_laziness_check` when `--laziness-debug-log` is set.
/// Passed through `maybe_write_laziness_debug_log` so every JSONL line references the same snapshot point.
pub(crate) struct LazinessFireMeta {
    pub session_id: String,
    pub todo_snapshot: Vec<DebugTodoSnapshot>,
    pub backing_task_count: usize,
}

/// The outcome of one classifier fire, used only to shape the JSONL debug line.
/// Production code never matches on this; it uses `LazinessDecision` and `LazinessAbortReason` directly.
pub(crate) enum LazinessFireOutcome {
    /// User input, model switch, timeout, or sampler/HTTP failure before a verdict was produced.
    /// `error_detail` is `Some` only for `ClassifierError` reasons (sampler/HTTP or `prepare_chat_completion` failure).
    Aborted {
        reason: LazinessAbortReason,
        error_detail: Option<String>,
    },
    /// Sampler returned text but `parse_classifier_output` rejected it.
    /// Both the raw text and the parse-error detail are kept for offline analysis.
    ParseError {
        raw_text: String,
        parse_error_detail: String,
    },
    /// Sampler returned text and it parsed cleanly.
    /// `build_laziness_debug_line` computes the debug decision (`WouldNudge`, `NoNudgeLowConfidence`, or `NoNudgeNotStalled`) from `parsed`.
    Verdict {
        parsed: ClassifierOutput,
        raw_text: String,
    },
    /// Classifier returned `Nudge` but harness policy blocked injection (e.g. session not in an active goal).
    Suppressed {
        reason: LazinessSuppressReason,
        parsed: ClassifierOutput,
        raw_text: String,
    },
}

/// Why a `Nudge` verdict was not injected into the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LazinessSuppressReason {
    /// Post-classifier gate: goal harness is off or goal is not `Active`.
    NotGoalMode,
}

/// Build a `LazinessDebugLogLine` from the captured per-fire metadata and a classifier outcome.
/// Free of `SessionActor` so the JSON-line shape stays unit-testable.
/// Consumes `meta` so its `String` and `Vec` fields move into the output line without clones.
pub(crate) fn build_laziness_debug_line(
    meta: LazinessFireMeta,
    model_id: &str,
    items_sent: usize,
    classifier_elapsed_ms: u64,
    outcome: LazinessFireOutcome,
) -> LazinessDebugLogLine {
    let LazinessFireMeta {
        session_id,
        todo_snapshot,
        backing_task_count,
    } = meta;
    let base = LazinessDebugLogLine {
        timestamp: chrono::Utc::now().to_rfc3339(),
        session_id,
        model_id: model_id.to_owned(),
        items_sent,
        todo_snapshot,
        backing_task_count,
        classifier_raw_output: None,
        parsed: None,
        decision: DebugDecision::Aborted,
        abort_reason: None,
        error_detail: None,
        classifier_elapsed_ms,
    };
    match outcome {
        LazinessFireOutcome::Aborted {
            reason,
            error_detail,
        } => LazinessDebugLogLine {
            abort_reason: Some(reason.as_const_str()),
            error_detail,
            ..base
        },
        LazinessFireOutcome::ParseError {
            raw_text,
            parse_error_detail,
        } => LazinessDebugLogLine {
            classifier_raw_output: Some(raw_text),
            abort_reason: Some(LazinessAbortReason::ClassifierError.as_const_str()),
            error_detail: Some(parse_error_detail),
            ..base
        },
        LazinessFireOutcome::Verdict { parsed, raw_text } => {
            let decision = classify_debug_decision(&parsed, LAZINESS_DEFAULT_MIN_CONFIDENCE);
            LazinessDebugLogLine {
                classifier_raw_output: Some(raw_text),
                parsed: Some(DebugClassifierOutput::from(parsed)),
                decision,
                ..base
            }
        }
        LazinessFireOutcome::Suppressed {
            reason,
            parsed,
            raw_text,
        } => {
            let decision = match reason {
                LazinessSuppressReason::NotGoalMode => DebugDecision::SuppressedNotGoalMode,
            };
            LazinessDebugLogLine {
                classifier_raw_output: Some(raw_text),
                parsed: Some(DebugClassifierOutput::from(parsed)),
                decision,
                ..base
            }
        }
    }
}

/// Classify what production would have done with this parsed output at the given min-confidence threshold.
/// Free of `SessionActor` so it stays unit-testable.
pub(crate) fn classify_debug_decision(
    parsed: &ClassifierOutput,
    min_confidence: f32,
) -> DebugDecision {
    if !parsed.category.is_stalled() {
        return DebugDecision::NoNudgeNotStalled;
    }
    if parsed.confidence < min_confidence {
        return DebugDecision::NoNudgeLowConfidence;
    }
    DebugDecision::WouldNudge
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DebugDecision {
    /// Parsed verdict was `stalled_*` with confidence at or above the threshold.
    /// Production would have injected a nudge here (subject to its cap, which debug mode ignores).
    WouldNudge,
    /// Parsed verdict was `not_stalled_*`. Production would have fired classifier telemetry only.
    NoNudgeNotStalled,
    /// Parsed verdict was `stalled_*` but below the configured min-confidence threshold (default 0.7).
    NoNudgeLowConfidence,
    /// User input, model switch, timeout, or parse error before a verdict was produced. The `abort_reason` field names which.
    Aborted,
    /// Classifier returned `stalled_*` above threshold but injection was blocked because the session is not in an active goal.
    SuppressedNotGoalMode,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub(crate) struct DebugClassifierOutput {
    pub category: String,
    pub confidence: f32,
    pub evidence: String,
}

impl From<ClassifierOutput> for DebugClassifierOutput {
    fn from(p: ClassifierOutput) -> Self {
        Self {
            category: p.category.as_const_str().to_string(),
            confidence: p.confidence,
            evidence: p.evidence,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub(crate) struct DebugTodoSnapshot {
    pub id: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub(crate) struct LazinessDebugLogLine {
    pub timestamp: String,
    pub session_id: String,
    pub model_id: String,
    /// Number of conversation items sent to the classifier, excluding the prepended system prompt. Capped at `LAZINESS_CONTEXT_ITEM_LIMIT`.
    pub items_sent: usize,
    pub todo_snapshot: Vec<DebugTodoSnapshot>,
    pub backing_task_count: usize,
    /// Raw classifier output text. `None` when the call aborted before producing any output.
    pub classifier_raw_output: Option<String>,
    /// Parsed structured output. `None` when raw output failed `parse_classifier_output` or the call aborted.
    pub parsed: Option<DebugClassifierOutput>,
    pub decision: DebugDecision,
    /// One of the `LAZINESS_ABORT_*` consts when `decision == Aborted`; `None` when the classifier produced a verdict.
    pub abort_reason: Option<&'static str>,
    /// Verbatim failure text when the sampler call or response parsing failed, so an operator can see what the backend complained about.
    /// `None` for clean classifier runs and for non-error aborts (user_input, model_switch, timeout).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    pub classifier_elapsed_ms: u64,
}

/// Append a single JSONL line to the debug-log file. Creates the file on first write; subsequent writes append.
/// Concurrent callers rely on `O_APPEND` for atomic ordering (JSONL lines fit under `PIPE_BUF`).
pub(crate) async fn append_laziness_debug_log_line(
    log_handle: &std::sync::Arc<std::path::Path>,
    line: &LazinessDebugLogLine,
) -> std::io::Result<()> {
    let mut json = serde_json::to_string(line).map_err(std::io::Error::other)?;
    json.push('\n');
    let path = std::sync::Arc::clone(log_handle);
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&*path)?;
        file.write_all(json.as_bytes())?;
        file.flush()
    })
    .await
    .map_err(std::io::Error::other)?
}

impl SessionActor {
    /// Snapshot the generation counters that gate the classifier's abort arms.
    /// Later polls compare against this snapshot to detect a user prompt or model switch during the idle wait or sampler call.
    pub(super) fn laziness_abort_snapshot(&self) -> (u64, u64) {
        (
            self.user_input_generation
                .load(std::sync::atomic::Ordering::Acquire),
            self.models_manager.model_switch_generation(),
        )
    }

    /// Returns `Some(reason)` when a user prompt or model switch happened since the snapshot, `None` otherwise.
    /// User input is checked first so a near-simultaneous prompt and switch attributes to the user, the more actionable signal in dashboards.
    pub(super) fn laziness_abort_check(&self, snapshot: (u64, u64)) -> Option<LazinessAbortReason> {
        let (start_user, start_switch) = snapshot;
        if self
            .user_input_generation
            .load(std::sync::atomic::Ordering::Acquire)
            != start_user
        {
            return Some(LazinessAbortReason::UserInput);
        }
        if self.models_manager.model_switch_generation() != start_switch {
            return Some(LazinessAbortReason::ModelSwitch);
        }
        None
    }

    /// Emit a `LazinessClassifierAborted` telemetry event for the given reason.
    /// Every abort path goes through this one site so the closed `LAZINESS_ABORT_*` const set stays authoritative.
    pub(super) fn emit_laziness_abort(&self, reason: LazinessAbortReason) {
        self.events
            .emit(crate::session::events::Event::LazinessClassifierAborted {
                reason: reason.as_const_str(),
            });
    }

    /// React to a model-switch wakeup from [`crate::agent::remote_config::ModelsManager`].
    /// Zeroes `nudges_used_this_session` so the Layer-3 cap is per session and model rather than per session.
    /// Called from the actor's main `select!` loop's `model_switch_rx.changed()` arm and directly by unit tests.
    pub(crate) async fn handle_model_switch_for_laziness(&self, new_gen: u64) {
        let mut state = self.state.lock().await;
        if state.nudges_used_this_session != 0 {
            tracing::debug!(
                prior_count = state.nudges_used_this_session,
                new_gen,
                "laziness: resetting nudges_used_this_session on model switch",
            );
            state.nudges_used_this_session = 0;
        }
    }

    /// The sampler call goes through `prepare_chat_completion().conversation_collect()`, which never publishes on the per-session sampler channel.
    /// A stalled verdict only queues a `<system-reminder>` via `push_system_reminder`; nothing enters `pending_inputs`, so no synthetic turn fires.
    /// Debug mode adds logging; it does not bypass the production decision logic.
    pub(crate) async fn maybe_fire_laziness_check(self: Arc<Self>) {
        let model_id_acp = self.models_manager.current_model_id();
        let model_id = model_id_acp.0.to_string();
        let cfg = self.models_manager.laziness_detector_for(&model_id);
        let debug_mode = self.laziness_debug_log.is_some();

        // This gates the classifier fire only; the nudge cap check is inside `evaluate_laziness`
        // `debug_mode` forces a fire even when per-model `enabled = false`
        // That lets eval traffic be collected against any model without flipping its catalog entry
        if !cfg.enabled && !debug_mode {
            return;
        }
        // Idle predicate, the same conditions the Layer-2 notification drain consults
        // Skipped in debug mode (the dev flag fires on every turn end to maximize eval-set coverage)
        if !debug_mode {
            let state = self.state.lock().await;
            if !is_session_idle_for_injection(&state) {
                return;
            }
        }

        // Capture the per-fire metadata only in debug mode; production skips the (cheap but non-zero) tool-bridge reads
        // Held in an `Option` and consumed via `.take()` at whichever log-write site is reached
        // That moves the String and Vec fields into the log line instead of cloning them
        let mut meta = if debug_mode {
            Some(LazinessFireMeta {
                session_id: self.session_info.id.0.to_string(),
                todo_snapshot: self.snapshot_todos_for_debug_log().await,
                backing_task_count: self.snapshot_backing_task_count_for_debug_log().await,
            })
        } else {
            None
        };

        let started = std::time::Instant::now();
        let idle_threshold = if debug_mode {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(
                cfg.idle_threshold_ms
                    .unwrap_or(LAZINESS_DEFAULT_IDLE_THRESHOLD_MS),
            )
        };
        let poll_interval = std::time::Duration::from_millis(LAZINESS_ABORT_POLL_INTERVAL_MS);
        let abort_snapshot = self.laziness_abort_snapshot();

        // Idle wait: polls the generation counters so user input or a model switch is observed within `poll_interval`.
        // Snapshot-and-poll instead of `tokio::sync::Notify::notified()` avoids the stored-permit hazard A `notify_one()` issued before this task spawns would otherwise fire the abort arm on the very first poll.
        // In debug mode the threshold is 0 so this loop is a no-op.
        let deadline = tokio::time::Instant::now() + idle_threshold;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let next_wait = (deadline - now).min(poll_interval);
            tokio::time::sleep(next_wait).await;
            if let Some(reason) = self.laziness_abort_check(abort_snapshot) {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                // `items_sent: 0` means the abort fired before the conversation request was built, not that it was built empty
                self.maybe_write_laziness_debug_log(
                    meta.take(),
                    &model_id,
                    0,
                    elapsed_ms,
                    LazinessFireOutcome::Aborted {
                        reason,
                        error_detail: None,
                    },
                )
                .await;
                self.emit_laziness_abort(reason);
                return;
            }
        }

        // Re-check idle after the sleep and snapshot `nudges_used`.
        // Debug mode bypasses the idle gate (the dev flag forces a fire even mid-turn)
        // It still reads the counter so `evaluate_laziness` sees the same state as production
        let nudges_used = {
            let state = self.state.lock().await;
            if !debug_mode && !is_session_idle_for_injection(&state) {
                return;
            }
            state.nudges_used_this_session
        };

        // System(classifier prompt: "you are a strict JSON classifier, you are NOT the agent in the transcript") [1].
        // The request body never contains a `ConversationItem::Assistant`, so the model has no assistant turn to continue.
        // `items_sent` (surfaced to telemetry and the debug log) counts the source chat items the classifier saw, not the 2 wire items.
        // C1/C3 — the local decision layer goes first: a confident "work remains"
        // verdict raises the nudge now and skips the classifier call. It can only
        // tighten; anything else falls through to the classifier below unchanged.
        if let Some(precheck) = self.jev_laziness_precheck().await {
            let category = precheck.category;
            let confidence = precheck.confidence;
            let decision = evaluate_laziness(
                &precheck,
                &cfg,
                nudges_used,
                LAZINESS_DEFAULT_MIN_CONFIDENCE,
            );
            if let LazinessDecision::Nudge { evidence, .. } = decision {
                self.events
                    .emit(crate::session::events::Event::LazinessClassifierFired {
                        model_id: model_id.clone(),
                        category: category.as_const_str(),
                        confidence,
                    });
                let goal_active = laziness_injection_active(
                    self.goal_harness_enabled(),
                    self.goal_tracker.lock().status(),
                );
                if goal_active {
                    let names = self.resolve_goal_tool_names().await;
                    let nudge_text = build_laziness_nudge(category, &evidence, Some(&names.todo));
                    if !nudge_text.is_empty() {
                        // Mirrors the classifier path's injection step (same abort
                        // and idle re-checks, same cap accounting).
                        let mut state = self.state.lock().await;
                        if self.laziness_abort_check(abort_snapshot).is_none()
                            && (debug_mode || is_session_idle_for_injection(&state))
                        {
                            self.push_system_reminder(&nudge_text);
                            state.nudges_used_this_session =
                                state.nudges_used_this_session.saturating_add(1);
                            let nudges_remaining = cfg
                                .max_nudges_per_session
                                .saturating_sub(state.nudges_used_this_session);
                            self.events
                                .emit(crate::session::events::Event::LazinessNudgeFired {
                                    model_id: model_id.clone(),
                                    category: category.as_const_str(),
                                    nudges_remaining,
                                });
                        }
                    }
                }
                self.maybe_write_laziness_debug_log(
                    meta.take(),
                    &model_id,
                    0,
                    started.elapsed().as_millis() as u64,
                    LazinessFireOutcome::Verdict {
                        parsed: precheck,
                        raw_text: "jev-precheck".to_owned(),
                    },
                )
                .await;
                return;
            }
        }
        let mut source_items = self.chat_state_handle.get_conversation().await;
        let window_start = laziness_window_start(
            &source_items,
            LAZINESS_CONTEXT_ITEM_LIMIT,
            LAZINESS_MIN_USER_TURNS,
            LAZINESS_MIN_ASSISTANT_TURNS,
        );
        if window_start > 0 {
            source_items.drain(0..window_start);
        }
        let items_count_after_trim = source_items.len();
        // An absent per-model `include_reasoning` defers to `LAZINESS_INCLUDE_REASONING` rather than forcing a config edit on every model entry
        let include_reasoning = cfg.include_reasoning.unwrap_or(LAZINESS_INCLUDE_REASONING);
        let transcript_text = flatten_transcript_for_classifier(&source_items, include_reasoning);

        // Harness-truth signals: they come from the harness, not the agent, so a lazy agent cannot fabricate them to mask a stall.
        // `outstanding_subagents`: live `spawn_subagent` calls that haven't returned.
        // TodoState is deliberately not injected The todo list is agent-authored and can be fabricated alongside the prose, so including it would confirm the lie rather than catch it.
        let backing_task_count = self.snapshot_backing_task_count_for_debug_log().await;
        // Refresh the count captured on `meta` above so the debug log matches what the classifier saw
        if let Some(m) = meta.as_mut() {
            m.backing_task_count = backing_task_count;
        }
        // `turn_elapsed_seconds` is also harness-truth (snapped by the session actor at turn start), so the agent cannot fabricate it.
        // The classifier cross-checks it against prose claims like "overnight run" or "N hours of work" `turn_elapsed_seconds_from_start_ms` handles negative deltas and absent timestamps.
        // Sub-second deltas truncate to 0, which the classifier reads as an explicit "very recent" signal rather than an absent field.
        let turn_start_ms = self
            .chat_state_handle
            .get_notification_meta()
            .await
            .and_then(|m| m.turn_start_ms);
        let turn_elapsed_seconds = turn_elapsed_seconds_from_start_ms(
            turn_start_ms,
            chrono::Utc::now().timestamp_millis(),
        );
        let runtime_state = format_runtime_state_line(backing_task_count, turn_elapsed_seconds);

        let items = vec![
            ConversationItem::system(LAZINESS_CLASSIFIER_PROMPT),
            ConversationItem::user(format!(
                "{LAZINESS_USER_PREAMBLE}\
                 === BEGIN TRANSCRIPT ===\n\
                 {runtime_state}\
                 {transcript_text}\
                 === END TRANSCRIPT ===\n"
            )),
        ];

        // Telemetry attribution headers match `run_memory_flush`
        // Backend trace correlation can then distinguish "classifier fired in session X" from background traffic
        let session_id_str = self.session_info.id.to_string();
        let request = ConversationRequest {
            items,
            tools: vec![],
            hosted_tools: vec![],
            tool_choice: None,
            model: Some(model_id.clone()),
            temperature: Some(0.0),
            max_output_tokens: Some(LAZINESS_MAX_OUTPUT_TOKENS),
            // `grok-4.5` and other tool-flavoured variants reject `reasoning_effort` with `400: Model does not support parameter reasoningEffort`
            // Omitting it lets each model apply its own default, which suffices for one short JSON object
            reasoning_effort: None,
            x_grok_conv_id: Some(format!("trace-classifier-{}", uuid::Uuid::new_v4())),
            x_grok_req_id: Some(format!("{LAZINESS_REQ_ID_PREFIX}{}", uuid::Uuid::new_v4())),
            x_grok_session_id: Some(session_id_str),
            x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
            ..ConversationRequest::default()
        };

        // Build a fresh `SamplingClient` via `prepare_chat_completion` and call `conversation_collect` directly, keeping the fire invisible `run_memory_flush`, `run_dream_model_call`, and `image_describe` use the same pattern.
        // The per-session `sampler_handle` would forward streaming events on the shared sampler channel.
        // Every `ChannelToken { Text }` becomes an ACP `AgentMessageChunk`, so the pager UI would render mid-classifier reasoning and text deltas `conversation_collect` never publishes on that channel, so the client sees nothing.
        let sampling_client = match self.prepare_chat_completion(false).await {
            Ok(c) => c,
            Err(err) => {
                let detail = err.to_string();
                tracing::debug!(error = %detail, "laziness classifier: prepare_chat_completion failed");
                let elapsed_ms = started.elapsed().as_millis() as u64;
                self.maybe_write_laziness_debug_log(
                    meta.take(),
                    &model_id,
                    items_count_after_trim,
                    elapsed_ms,
                    LazinessFireOutcome::Aborted {
                        reason: LazinessAbortReason::ClassifierError,
                        error_detail: Some(detail),
                    },
                )
                .await;
                self.emit_laziness_abort(LazinessAbortReason::ClassifierError);
                return;
            }
        };

        // The sampler call is wrapped in a generation-poll loop and a wall-clock timeout
        // Dropping the `conversation_collect` future on abort cancels the HTTP request, so no actor-side cleanup is needed
        let timeout = tokio::time::sleep(std::time::Duration::from_millis(
            LAZINESS_CLASSIFIER_TIMEOUT_MS,
        ));
        tokio::pin!(timeout);
        let sampler_future = sampling_client.conversation_collect(request);
        tokio::pin!(sampler_future);
        let response = loop {
            tokio::select! {
                biased;
                out = &mut sampler_future => match out {
                    Ok(response) => break response,
                    Err(err) => {
                        let detail = err.to_string();
                        tracing::debug!(error = %detail, "laziness classifier sampler call failed");
                        let elapsed_ms = started.elapsed().as_millis() as u64;
                        self.maybe_write_laziness_debug_log(
                            meta.take(),
                            &model_id,
                            items_count_after_trim,
                            elapsed_ms,
                            LazinessFireOutcome::Aborted {
                                reason: LazinessAbortReason::ClassifierError,
                                error_detail: Some(detail),
                            },
                        )
                        .await;
                        self.emit_laziness_abort(LazinessAbortReason::ClassifierError);
                        return;
                    }
                },
                _ = &mut timeout => {
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    self.maybe_write_laziness_debug_log(
                        meta.take(),
                        &model_id,
                        items_count_after_trim,
                        elapsed_ms,
                        LazinessFireOutcome::Aborted {
                            reason: LazinessAbortReason::Timeout,
                            error_detail: None,
                        },
                    )
                    .await;
                    self.emit_laziness_abort(LazinessAbortReason::Timeout);
                    return;
                }
                _ = tokio::time::sleep(poll_interval) => {
                    if let Some(reason) = self.laziness_abort_check(abort_snapshot) {
                        let elapsed_ms = started.elapsed().as_millis() as u64;
                        self.maybe_write_laziness_debug_log(
                            meta.take(),
                            &model_id,
                            items_count_after_trim,
                            elapsed_ms,
                            LazinessFireOutcome::Aborted {
                                reason,
                                error_detail: None,
                            },
                        )
                        .await;
                        self.emit_laziness_abort(reason);
                        return;
                    }
                }
            }
        };

        let elapsed_ms = started.elapsed().as_millis() as u64;
        let raw_text = response.assistant_text();
        let parsed = match parse_classifier_output(&raw_text) {
            Ok(p) => p,
            Err(parse_err) => {
                let parse_error_detail = parse_err.to_string();
                // Truncate raw to 200 chars for offline analysis.
                let snippet: String = raw_text.chars().take(200).collect();
                tracing::debug!(
                    error = %parse_error_detail,
                    raw_truncated = %snippet,
                    "laziness classifier output not parseable",
                );
                self.maybe_write_laziness_debug_log(
                    meta.take(),
                    &model_id,
                    items_count_after_trim,
                    elapsed_ms,
                    LazinessFireOutcome::ParseError {
                        raw_text,
                        parse_error_detail,
                    },
                )
                .await;
                self.emit_laziness_abort(LazinessAbortReason::ClassifierError);
                return;
            }
        };

        let category = parsed.category;
        let confidence = parsed.confidence;
        let decision =
            evaluate_laziness(&parsed, &cfg, nudges_used, LAZINESS_DEFAULT_MIN_CONFIDENCE);

        let goal_active = laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        );

        let debug_outcome = match &decision {
            LazinessDecision::Nudge { .. } if !goal_active => LazinessFireOutcome::Suppressed {
                reason: LazinessSuppressReason::NotGoalMode,
                parsed: parsed.clone(),
                raw_text: raw_text.clone(),
            },
            _ => LazinessFireOutcome::Verdict { parsed, raw_text },
        };
        self.maybe_write_laziness_debug_log(
            meta.take(),
            &model_id,
            items_count_after_trim,
            elapsed_ms,
            debug_outcome,
        )
        .await;

        let LazinessDecision::Nudge { evidence, .. } = decision else {
            // No nudge. Telemetry still shows why: the `LazinessClassifierFired` category and the absence of a paired `LazinessNudgeFired`.
            self.events
                .emit(crate::session::events::Event::LazinessClassifierFired {
                    model_id,
                    category: category.as_const_str(),
                    confidence,
                });
            return;
        };
        // Nudge branch: clone once because both `LazinessClassifierFired` here and `LazinessNudgeFired` below need `model_id`
        self.events
            .emit(crate::session::events::Event::LazinessClassifierFired {
                model_id: model_id.clone(),
                category: category.as_const_str(),
                confidence,
            });

        if !goal_active {
            return;
        }

        let names = self.resolve_goal_tool_names().await;
        let nudge_text = build_laziness_nudge(category, &evidence, Some(&names.todo));
        if nudge_text.is_empty() {
            // Defensive: `evaluate_laziness` only returns Nudge for stalled_* variants, which all produce non-empty text
            return;
        }

        // Final injection step: hold the state lock from the idle re-check through the chat history push and the counter increment.
        // `push_system_reminder` delegates to the non-blocking `chat_state_handle.push_user_message` (a channel send).
        // It is safe to call under the `TokioMutex<State>` guard: no lock inversion, no deadlock.
        let mut state = self.state.lock().await;
        // The abort check runs before the idle re-check A prompt that lands between sampler return and this lock acquire trips both: it fills `pending_inputs` and bumps `user_input_generation`.
        // Checking abort first reports it via the typed `LazinessClassifierAborted` event instead of the idle re-check's silent return.
        // The same ordering covers a model switch landing in that window.
        if let Some(reason) = self.laziness_abort_check(abort_snapshot) {
            self.emit_laziness_abort(reason);
            return;
        }
        if !debug_mode && !is_session_idle_for_injection(&state) {
            // The idle predicate failed for a reason other than a fresh prompt or model switch
            // For example a notification drain queued a synthetic input, or the user cancelled and `notifications_suppressed` is true
            // Return silently: the condition we wanted to nudge no longer holds, and it is not a Laziness-typed abort either
            return;
        }
        self.push_system_reminder(&nudge_text);
        state.nudges_used_this_session = state.nudges_used_this_session.saturating_add(1);
        let nudges_remaining = cfg
            .max_nudges_per_session
            .saturating_sub(state.nudges_used_this_session);
        self.events
            .emit(crate::session::events::Event::LazinessNudgeFired {
                model_id,
                category: category.as_const_str(),
                nudges_remaining,
            });
    }

    /// Append a single line to the debug log file when `meta` is `Some` (`--laziness-debug-log` was set); no-op in production.
    /// Every return point in `maybe_fire_laziness_check` funnels through this one call.
    /// Takes `meta` by value; each callsite passes `meta.take()` so the `String` and `Vec` fields move into the log line without a clone.
    async fn maybe_write_laziness_debug_log(
        &self,
        meta: Option<LazinessFireMeta>,
        model_id: &str,
        items_sent: usize,
        classifier_elapsed_ms: u64,
        outcome: LazinessFireOutcome,
    ) {
        let Some(meta) = meta else { return };
        let Some(handle) = self.laziness_debug_log.as_ref() else {
            return;
        };
        let line =
            build_laziness_debug_line(meta, model_id, items_sent, classifier_elapsed_ms, outcome);
        if let Err(e) = append_laziness_debug_log_line(handle, &line).await {
            tracing::warn!(error = %e, "failed to append laziness debug log line");
        }
    }

    async fn snapshot_todos_for_debug_log(&self) -> Vec<DebugTodoSnapshot> {
        use crate::tools::todo::{TodoState, TodoStatus};
        use xai_grok_tools::types::resources::State;
        let bridge = self.tool_bridge_handle();
        bridge
            .read_resource::<State<TodoState>>()
            .await
            .map(|state| {
                state
                    .0
                    .todo_items_with_ids()
                    .map(|(id, item)| DebugTodoSnapshot {
                        id: id.clone(),
                        status: match item.status {
                            TodoStatus::Pending => "pending",
                            TodoStatus::InProgress => "in_progress",
                            TodoStatus::Completed => "completed",
                            TodoStatus::Cancelled => "cancelled",
                        },
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count of outstanding background terminal tasks.
    /// The gate's own count also includes the just-ended prompt's outstanding subagents.
    /// Operators correlating debug log lines with gate decisions can read the session's events.jsonl for the full subagent state.
    async fn snapshot_backing_task_count_for_debug_log(&self) -> usize {
        self.tool_bridge_handle()
            .list_background_tasks()
            .await
            .into_iter()
            .filter(xai_grok_tools::computer::types::TaskSnapshot::is_outstanding)
            .count()
    }
}
