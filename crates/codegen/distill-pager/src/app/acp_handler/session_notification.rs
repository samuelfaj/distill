// Modified for Distill by Samuel Fajreldines, 2026.
use super::subagent_activity::sync_subagent_dispatch;
use super::*;
use distill_shell::extensions::notification::HookAnnotationKind;
use distill_shell::sampling::error::format_rate_limited_user_message;
/// The one scrollback line a failed run gets; success gets none and a deny is already annotated by the shell.
/// "ignored" is literal (fail-open); a config-tier source has no name worth showing, so its line names only the event.
pub(super) fn failed_hook_line(
    event_name: &str,
    run: &distill_shell::extensions::notification::HookRunEntryDto,
) -> Option<String> {
    use distill_hooks::config::HookDisplayName;
    use distill_shell::extensions::notification::HookRunStatusDto;
    let HookRunStatusDto::Failed {
        error,
        blocked: false,
        ..
    } = &run.status
    else {
        return None;
    };
    let subject = match distill_hooks::config::hook_display_label(&run.name) {
        HookDisplayName::Named(name) => format!("{event_name} hook ({name})"),
        HookDisplayName::Tier(_) => format!("{event_name} hook"),
    };
    let error = error.lines().next().unwrap_or("").trim();
    Some(if error.is_empty() {
        format!("{subject} failed, ignored")
    } else {
        format!("{subject} failed, ignored: {error}")
    })
}
/// A batch stamped with another turn's prompt id: a late `stop_cancelled` / `stop_failure` report can land after the next
/// queued prompt started and must not touch its phase. Unstamped batches always belong to the running turn.
fn is_foreign_hook_batch(agent: &AgentView, batch_prompt_id: Option<&str>) -> bool {
    matches!(
        (batch_prompt_id, agent.session.current_prompt_id.as_deref()),
        (Some(batch), Some(current)) if batch != current
    )
}
pub(super) fn refresh_context_used(view: &mut AgentView, used: u64) {
    let total = view.session.models.get_context_window().unwrap_or(0);
    view.apply_context_used(used, total);
}
/// Context-bar refresh carried by a compaction lifecycle update, if any.
/// `AutoCompactStarted` carries the count the trigger fired on, and the banner percentage derives from it.
/// Without this refresh the bar shows the stale pre-turn number right next to the "N% full" banner.
pub(super) fn compaction_context_refresh(update: &XaiSessionUpdate) -> Option<u64> {
    match update {
        XaiSessionUpdate::AutoCompactStarted { tokens_used, .. } => Some(*tokens_used),
        XaiSessionUpdate::AutoCompactCompleted { tokens_after, .. } => Some(*tokens_after),
        _ => None,
    }
}
/// Refresh the bar and record `used` as the confirmed count for a pending compaction message; call only from the `meta.totalTokens` path.
pub(super) fn confirm_context_used(view: &mut AgentView, used: u64) {
    refresh_context_used(view, used);
    view.session.note_context_used(used);
}
/// Replay gate shared by the ACP and Grok session-update paths. Returns `true` when the update must be dropped.
/// Replay is only expected while a `session/load` is in flight for this agent (fresh-view load or reconnect reload window).
/// Applying a misrouted replay would append duplicated history below the live transcript.
pub(crate) fn drop_unexpected_replay(
    agent: &mut AgentView,
    meta: &NotificationMeta,
    session_id: &str,
    source: &'static str,
) -> bool {
    if !meta.is_replay {
        agent.late_replay_until = None;
        return false;
    }
    if agent.accepts_replayed_update() {
        agent.mark_reload_replay_seen();
        return false;
    }
    if agent.unexpected_replay_drops == 0 {
        tracing::warn!(
            session_id,
            source,
            event_id = meta.event_id.as_deref(),
            "Dropping unexpected replay update (no session load in flight); further drops logged at debug"
        );
    } else {
        tracing::debug!(
            session_id,
            source,
            event_id = meta.event_id.as_deref(),
            "Dropping unexpected replay update"
        );
    }
    agent.unexpected_replay_drops = agent.unexpected_replay_drops.saturating_add(1);
    true
}
/// Advance the reconnect cursor to an applied update's eventId.
/// Called from every applied arm (Plan, bg-stdout, tracker); dropped updates (dedup, promptId gate, unexpected replay) deliberately don't move it.
/// Forward-only via [`AgentView::advance_last_seen_event_id`].
pub(super) fn advance_reconnect_cursor(agent: &mut AgentView, meta: &mut NotificationMeta) {
    if let Some(id) = meta.event_id.take() {
        agent.advance_last_seen_event_id(id, meta.event_seq);
    }
}
/// A string field off a turn-terminal notification envelope's `_meta` (the cancel-qualifier keys; absent on older shells).
fn terminal_meta_str<'a>(meta: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    meta.and_then(|v| v.get(key)).and_then(|v| v.as_str())
}
/// Decode the HTML entities that appear in generated session summaries.
fn decode_html_entities(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains('&') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = s.to_string();
    out = out.replace("&amp;", "&");
    out = out.replace("&lt;", "<");
    out = out.replace("&gt;", ">");
    out = out.replace("&quot;", "\"");
    out = out.replace("&#39;", "'");
    out = out.replace("&#x27;", "'");
    out = out.replace("&apos;", "'");
    std::borrow::Cow::Owned(out)
}
/// Display-only end marker for a replayed `TurnCompleted`.
/// Reads cancel metadata once; chatty rate-limited wakes still paint `TurnFailed`.
fn synthesize_replay_turn_marker(
    agent: &mut AgentView,
    prompt_id: &str,
    stop_reason: &str,
    agent_result: Option<&str>,
    error_kind: Option<crate::app::error_display::WireErrorType>,
    elapsed_ms: Option<u64>,
    meta: Option<&serde_json::Value>,
) -> Option<crate::scrollback::blocks::SessionEvent> {
    use crate::app::turn_completion::{
        CANCEL_TRIGGER_KEY, CANCELLATION_CATEGORY_KEY, TerminalMarkerInput, TurnStopReason,
        terminal_marker,
    };
    let stop = TurnStopReason::from(stop_reason);
    let is_wake = is_wake_prompt(prompt_id);
    let visible = agent.replayed_visible_prompts.contains(prompt_id);
    let chatty_rate_limit = is_wake && stop == TurnStopReason::RateLimit && visible;
    if is_wake && (matches!(stop, TurnStopReason::Error) || chatty_rate_limit) {
        agent.failed_wake_marker_for = Some(prompt_id.to_string());
    }
    let suppress = super::prompt_origin::suppress_replay_marker_for_origin(
        agent.replayed_bash_prompts.contains(prompt_id),
        visible,
        prompt_id,
        stop,
    );
    let banner = crate::app::dispatch::scrollback_has_recent_error_banner(&agent.scrollback);
    let cancel_trigger = terminal_meta_str(meta, CANCEL_TRIGGER_KEY);
    let cancellation_category = terminal_meta_str(meta, CANCELLATION_CATEGORY_KEY);
    let paint_rate_limit_failure = chatty_rate_limit && !suppress && !banner;
    let marker = if suppress {
        None
    } else {
        terminal_marker(TerminalMarkerInput {
            stop,
            elapsed_ms,
            agent_result,
            send_now_cancel: cancel_trigger == Some("send_now"),
            cancel_trigger,
            cancellation_category,
            error_kind,
            error_banner_present: banner,
        })
    };
    marker.or_else(|| {
        paint_rate_limit_failure.then(|| {
            super::prompt_origin::rate_limited_wake_failure_event(
                agent_result,
                elapsed_ms.map(std::time::Duration::from_millis),
            )
        })
    })
}
/// Handle `x.ai/session_notification` and replay-path `x.ai/session/update`.
/// Routes by `session_id` so events for an inactive agent still mutate that agent's state.
/// The redraw decision is gated on whether the matched agent is the currently visible one.
pub(super) fn handle_session_notification(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    handle_session_notification_with_origin(notif, app, LifecycleOrigin::Stream)
}
pub(super) fn handle_session_notification_with_origin(
    notif: &acp::ExtNotification,
    app: &mut AppView,
    origin: LifecycleOrigin,
) -> bool {
    let Ok(session_notif) = serde_json::from_str::<SessionNotification>(notif.params.get()) else {
        tracing::warn!("Failed to parse {}", notif.method.as_ref());
        return false;
    };
    match &session_notif.update {
        XaiSessionUpdate::TaskBackgrounded { .. } => {
            return handle_task_backgrounded(notif, app);
        }
        XaiSessionUpdate::TaskCompleted { .. } => {
            return handle_task_completed(notif, app);
        }
        XaiSessionUpdate::ScheduledTaskCreated { .. } => {
            return handle_scheduled_task_created(notif, app);
        }
        XaiSessionUpdate::ScheduledTaskDeleted { .. } => {
            return handle_scheduled_task_deleted(notif, app);
        }
        _ => {}
    }
    let is_api_key_auth = app.is_api_key_auth;
    let matched = match find_session_match(app, &session_notif.session_id) {
        Some(m) => m,
        None => {
            tracing::debug!(
                session_id = session_notif.session_id.0.as_ref(),
                method = notif.method.as_ref(),
                "load-race: x.ai/session_notification DROPPED — no agent matches session_id"
            );
            return false;
        }
    };
    let parent_id = matched.agent_id();
    let is_active = is_matched_agent_active(app, parent_id);
    let agent = app
        .agents
        .get_mut(&parent_id)
        .expect("find_session_match returned an existing AgentId");
    if matches!(matched, SessionMatch::Child(_)) {
        let child_sid: &str = session_notif.session_id.0.as_ref();
        let changed = handle_child_session_notification(
            session_notif.update,
            child_sid,
            agent,
            is_api_key_auth,
        );
        return changed && is_active;
    }
    let meta = NotificationMeta::from_json(session_notif.meta.as_ref().and_then(|v| v.as_object()));
    if drop_unexpected_replay(
        agent,
        &meta,
        session_notif.session_id.0.as_ref(),
        "x.ai/session/update",
    ) {
        return false;
    }
    let is_workflow_update = matches!(
        &session_notif.update,
        XaiSessionUpdate::WorkflowUpdated { .. }
    );
    let mut committed_subagent_lifecycle = None;
    let is_subagent_lifecycle =
        if let Some(lifecycle) = classify_subagent_lifecycle(&session_notif.update, origin) {
            let Some(action) = prepare_tui_subagent_lifecycle(
                agent,
                &lifecycle,
                meta.is_replay,
                meta.event_seq,
                &session_notif,
                std::time::Instant::now(),
            ) else {
                return false;
            };
            let should_render = action.should_render();
            committed_subagent_lifecycle = action.commit(agent, std::time::Instant::now());
            if !should_render {
                return false;
            }
            true
        } else {
            false
        };
    if !is_workflow_update
        && !is_subagent_lifecycle
        && !meta.is_replay
        && meta.event_seq.is_some_and(|seq| {
            agent
                .last_applied_xai_event_seq
                .is_some_and(|last| seq <= last)
        })
    {
        tracing::debug!(
            session_id = session_notif.session_id.0.as_ref(),
            event_seq = meta.event_seq,
            last_applied = agent.last_applied_xai_event_seq,
            "x.ai/session update DROPPED by dedup highwater (event_seq <= last_applied)"
        );
        return false;
    }
    let mut plugins_changed_needs_skills_refetch = false;
    let mut status_snapshot_applied = false;
    let mut terminal_outcome: Option<super::super::turn_completion::TerminalApply> = None;
    let mut pending_finish_for_spawn = None;
    let root_session_id: &str = session_notif.session_id.0.as_ref();
    let changed = match session_notif.update {
        ref update @ (XaiSessionUpdate::AutoCompactStarted { .. }
        | XaiSessionUpdate::AutoCompactCompleted { .. }
        | XaiSessionUpdate::AutoCompactFailed { .. }
        | XaiSessionUpdate::AutoCompactCancelled { .. }
        | XaiSessionUpdate::RetryState(_)
        | XaiSessionUpdate::ImageDropped { .. }
        | XaiSessionUpdate::MemoryFlushCompleted { .. }
        | XaiSessionUpdate::MemoryCaptureActivity { .. }
        | XaiSessionUpdate::MemoryDreamCompleted { .. }
        | XaiSessionUpdate::MemorySessionSaved { .. }) => {
            let changed = apply_compaction_or_retry_update(agent, update, is_api_key_auth);
            if let XaiSessionUpdate::AutoCompactCompleted { .. } = update {
                agent.todo.update_todos(Vec::new());
            }
            changed
        }
        XaiSessionUpdate::ImageCompressed {
            ref images,
            ref message,
        } => apply_image_compressed(agent, images, message),
        XaiSessionUpdate::ToolCallDeltaChunk {
            ref name,
            tool_index,
            ..
        } => {
            if meta.is_replay || agent.session.loading_replay || agent.running_wake_turn.is_some() {
                false
            } else {
                let had_activity_before = agent.session.tracker.activity().is_some();
                let changed = agent
                    .session
                    .tracker
                    .note_tool_call_arguments_delta(name.as_deref(), tool_index);
                if !had_activity_before && agent.session.tracker.activity().is_some() {
                    note_first_turn_activity(agent);
                }
                changed
            }
        }
        XaiSessionUpdate::TurnCompleted {
            prompt_id,
            stop_reason,
            agent_result,
            error_kind,
            elapsed_ms,
            usage,
            ..
        } => {
            let error_kind = crate::app::error_display::wire_error_kind(error_kind.as_deref());
            // Where this turn went, from the shell's own record: one line per
            // engine that ran, then the decision layer's call count.
            let distribution = crate::views::turn_distribution::report(usage.as_ref());
            let handled = if agent.session.loading_replay {
                let first = agent.replayed_terminal_prompts.insert(prompt_id.clone());
                if first
                    && !prompt_id.is_empty()
                    && let Some(event) = synthesize_replay_turn_marker(
                        agent,
                        &prompt_id,
                        stop_reason.as_str(),
                        agent_result.as_deref(),
                        error_kind,
                        elapsed_ms,
                        session_notif.meta.as_ref(),
                    )
                {
                    agent.push_end_marker_block(event);
                }
                false
            } else if is_wake_prompt(&prompt_id) {
                if agent
                    .running_wake_turn
                    .as_ref()
                    .is_some_and(|wake| wake.prompt_id == prompt_id)
                {
                    agent.running_wake_turn = None;
                }
                agent.finished_wake_prompts.insert(prompt_id.to_string());
                if agent.session.state.is_busy() {
                    if agent.session.state.command_in_flight().is_some() {
                        agent.session.tracker.snapshot_output_epoch();
                    }
                    let errored = matches!(stop_reason.as_str(), "error" | "rate_limit");
                    if errored && agent.failed_wake_marker_for.as_deref() != Some(&*prompt_id) {
                        agent.failed_wake_marker_for = Some(prompt_id.clone());
                        if crate::app::dispatch::scrollback_has_recent_error_banner(
                            &agent.scrollback,
                        ) {
                            false
                        } else {
                            let event = if stop_reason == "rate_limit" {
                                super::prompt_origin::rate_limited_wake_failure_event(
                                    agent_result.as_deref(),
                                    None,
                                )
                            } else {
                                crate::app::turn_completion::failed_turn_event(
                                    error_kind,
                                    agent_result.as_deref(),
                                    None,
                                )
                            };
                            agent.push_end_marker_block(event);
                            true
                        }
                    } else {
                        false
                    }
                } else {
                    finish_wake_turn(
                        agent,
                        &prompt_id,
                        super::prompt_origin::WakeTerminal {
                            stop_reason: &stop_reason,
                            agent_result: agent_result.as_deref(),
                            cancel_trigger: terminal_meta_str(
                                session_notif.meta.as_ref(),
                                super::super::turn_completion::CANCEL_TRIGGER_KEY,
                            ),
                            cancellation_category: terminal_meta_str(
                                session_notif.meta.as_ref(),
                                super::super::turn_completion::CANCELLATION_CATEGORY_KEY,
                            ),
                            error_kind,
                        },
                    );
                    true
                }
            } else if is_server_initiated_prompt(&prompt_id)
                && !is_scheduler_fired_prompt(&prompt_id)
            {
                if agent.session.state.is_busy() {
                    if agent.session.state.command_in_flight().is_some() {
                        agent.session.tracker.snapshot_output_epoch();
                    }
                    false
                } else {
                    agent.session.tracker.finish_turn(&mut agent.scrollback);
                    true
                }
            } else {
                terminal_outcome =
                    Some(super::super::turn_completion::finalize_turn_from_terminal(
                        agent,
                        root_session_id,
                        super::super::turn_completion::TerminalSignal {
                            prompt_id: Some(&prompt_id),
                            stop_reason: Some(&stop_reason),
                            agent_result: agent_result.as_deref(),
                            cancel_trigger: terminal_meta_str(
                                session_notif.meta.as_ref(),
                                super::super::turn_completion::CANCEL_TRIGGER_KEY,
                            ),
                            cancellation_category: terminal_meta_str(
                                session_notif.meta.as_ref(),
                                super::super::turn_completion::CANCELLATION_CATEGORY_KEY,
                            ),
                            cancellation_context: session_notif.meta.as_ref().and_then(|m| {
                                m.get(super::super::turn_completion::CANCELLATION_CONTEXT_KEY)
                            }),
                            error_kind,
                        },
                    ));
                false
            };
            if let Some(distribution) = distribution {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(distribution));
            }
            handled
        }
        XaiSessionUpdate::SubagentSpawned {
            subagent_id,
            attempt_id: _,
            child_session_id,
            subagent_type,
            description,
            persona,
            role,
            model,
            effective_context_source,
            resumed_from,
            capability_mode,
            context_normalized,
            parent_prompt_id,
            parent_session_id,
            workflow_run_id,
            ..
        } => {
            tracing::info!(
                child_session_id = %child_session_id,
                subagent_type = %subagent_type,
                "Subagent spawned"
            );
            let committed_lifecycle = committed_subagent_lifecycle.take();
            let is_new_attempt = committed_lifecycle
                .as_ref()
                .is_some_and(|action| action.is_new_attempt);
            let is_wake = committed_lifecycle
                .as_ref()
                .is_some_and(|action| action.is_wake);
            let prior_background = (!is_new_attempt)
                .then(|| {
                    agent
                        .subagent_sessions
                        .get(&child_session_id)
                        .map(|info| info.attempt.is_background)
                })
                .flatten();
            let is_background = agent
                .session
                .tracker
                .task_tool_background
                .remove(&subagent_id)
                .or(prior_background)
                .unwrap_or(is_wake);
            let persona_display = persona.clone();
            let role_display = role.clone();
            let model_display = model.clone();
            let retained_terminal_finish = if meta.is_replay
                && committed_lifecycle
                    .as_ref()
                    .is_some_and(|action| action.rebuilt_current_attempt)
            {
                agent
                    .subagent_sessions
                    .get(&child_session_id)
                    .and_then(|info| {
                        info.is_finished().then(|| SessionNotification {
                            session_id: session_notif.session_id.clone(),
                            update: info.attempt.terminal_update(
                                &info.subagent_id,
                                &child_session_id,
                                "cancelled",
                            ),
                            meta: session_notif.meta.clone(),
                        })
                    })
            } else {
                None
            };
            let lifecycle = committed_lifecycle
                .as_ref()
                .map(|action| action.lifecycle.clone())
                .unwrap_or_default();
            let pending_finish = committed_lifecycle
                .as_ref()
                .and_then(|action| action.pending_finish.clone());
            let now = std::time::Instant::now();
            let prior_info = agent.subagent_sessions.remove(&child_session_id);
            let had_prior_info = prior_info.is_some();
            let terminal_entry_id = if is_new_attempt {
                None
            } else {
                prior_info
                    .as_ref()
                    .and_then(|info| info.attempt.terminal_entry_id)
                    .filter(|eid| agent.scrollback.get_by_id(*eid).is_some())
            };
            let attempt = crate::app::subagent::SubagentAttemptInfo {
                lifecycle,
                persona: persona.map(Arc::from),
                role: role.map(Arc::from),
                model: model.map(Arc::from),
                active_model: None,
                active_reasoning_effort: None,
                context_source: effective_context_source.map(Arc::from),
                resumed_from: resumed_from.map(Arc::from),
                capability_mode: capability_mode.map(Arc::from),
                workflow_run_id: workflow_run_id.clone().map(Arc::from),
                context_normalized,
                parent_prompt_id: parent_prompt_id.map(Arc::from),
                started_at: now,
                last_progress_at: now,
                status: None,
                error: None,
                duration_ms: None,
                tool_calls: None,
                turns: None,
                turn_count: None,
                tool_call_count: None,
                tokens_used: None,
                context_window_tokens: None,
                context_usage_pct: None,
                tools_used: Vec::new(),
                error_count: None,
                activity_label: None,
                is_background,
                pending_kill: false,
                kill_requested_at: None,
                scrollback_entry_id: None,
                terminal_entry_id,
            };
            let mut info = SubagentInfo::from_spawn(
                prior_info,
                crate::app::subagent::SubagentChildInfo {
                    subagent_id,
                    child_session_id: child_session_id.clone(),
                    description: description.clone(),
                    subagent_type: subagent_type.clone(),
                },
                attempt,
                is_new_attempt,
            );
            agent.session.tracker.subagent_labels.borrow_mut().record(
                &info.subagent_id,
                crate::app::subagent::subagent_display_label(&info),
                info.child_session_id.clone(),
            );
            if meta.is_replay && agent.session.loading_replay {
                info.transcript.begin_parent_replay();
            }
            agent
                .subagent_sessions
                .insert(child_session_id.clone(), info);
            if !had_prior_info
                && let Some(ref sid) = agent.session.session_id
                && let Some(info) = agent.subagent_sessions.get_mut(&child_session_id)
            {
                crate::app::subagent::enrich_from_meta(info, &agent.session.cwd, sid.0.as_ref());
            }
            let (effective_child_cwd, effective_is_worktree) = derive_child_cwd(
                &agent.session.cwd,
                agent.subagent_sessions.get(&child_session_id),
            );
            let labels = &agent.session.tracker.subagent_labels;
            if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id) {
                child_view.session.state = AgentState::TurnRunning;
                if is_new_attempt {
                    child_view.session.tracker = AcpUpdateTracker::sharing_labels(labels);
                }
            } else {
                let child_session = AgentSession {
                    id: AgentId(0),
                    acp_tx: agent.session.acp_tx.clone(),
                    session_id: Some(acp::SessionId::new(child_session_id.clone())),
                    models: agent.session.models.clone(),
                    state: AgentState::TurnRunning,
                    tracker: AcpUpdateTracker::sharing_labels(labels),
                    cwd: effective_child_cwd,
                    is_worktree: effective_is_worktree,
                    forked_from: None,
                    pending_prompts: std::collections::VecDeque::new(),
                    next_queue_id: 0,
                    yolo_mode: true,
                    auto_mode: false,
                    prompt_history: Vec::new(),
                    prompt_history_loading: false,
                    loading_replay: false,
                    restore_degree: None,
                    rate_limited: false,
                    model_incompatible: false,
                    credit_limit_blocked: false,
                    free_usage_blocked: false,
                    bg_tasks: std::collections::BTreeMap::new(),
                    bg_tool_call_to_task: std::collections::HashMap::new(),
                    scheduled_tasks: std::collections::HashMap::new(),
                    available_commands: Vec::new(),
                    available_commands_generation: 0,
                    available_tools: None,
                    model_switch_pending: false,
                    hook_block_hold: false,
                    blocked_prompt: None,
                    user_model_preference: None,
                    deferred_model_switch: None,
                    in_flight_prompt: None,
                    compact_held_prompt: None,
                    current_prompt_id: None,
                    created_via_new: false,
                };
                let mut child_scrollback = crate::scrollback::state::ScrollbackState::new();
                child_scrollback.set_appearance(agent.scrollback.appearance().clone());
                let mut child_view = AgentView::new(child_session, child_scrollback);
                child_view.set_input_mode(InputMode::Vim);
                child_view.set_sharing_enabled(agent.sharing_enabled);
                child_view.set_billing_surface_visible(agent.billing_surface_visible);
                child_view.set_usage_command_visible(agent.usage_command_visible);
                let dashboard_visible = agent
                    .prompt
                    .slash_controller
                    .registry()
                    .get("dashboard")
                    .is_some();
                child_view.set_dashboard_visible(dashboard_visible);
                child_view.set_has_session_announcements(
                    agent.prompt.slash_controller.has_session_announcements(),
                );
                child_view
                    .prompt
                    .set_screen_mode(agent.prompt.slash_controller.screen_mode());
                child_view.app_chat_mode = agent.app_chat_mode;
                let recap_visible = agent
                    .prompt
                    .slash_controller
                    .registry()
                    .get("recap")
                    .is_some();
                child_view.set_session_recap_available(recap_visible);
                let voice_visible = agent
                    .prompt
                    .slash_controller
                    .registry()
                    .get("voice")
                    .is_some();
                child_view.set_voice_mode_available(voice_visible);
                let restricted = agent
                    .prompt
                    .slash_controller
                    .registry()
                    .restricted_commands();
                child_view.set_restricted_commands(&restricted);
                let link = crate::app::agent_view::ChildLink::unaddressable(acp::SessionId::new(
                    parent_session_id,
                ));
                agent.insert_subagent_view(child_session_id.clone(), Box::new(child_view), link);
            }
            if workflow_run_id.is_none() {
                let block = crate::scrollback::blocks::SubagentBlock::started(
                    &description,
                    &child_session_id,
                    &subagent_type,
                    persona_display,
                    role_display,
                    model_display,
                    is_background,
                );
                let entry_id = agent.scrollback.push_block(RenderBlock::Subagent(block));
                agent.scrollback.set_last_running(true);
                if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                    info.attempt.scrollback_entry_id = Some(entry_id);
                    info.attempt.is_background = is_background;
                }
            } else if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.attempt.is_background = is_background;
            }
            pending_finish_for_spawn = retained_terminal_finish
                .or(pending_finish)
                .map(|payload| (payload, LifecycleOrigin::Reconciliation));
            true
        }
        XaiSessionUpdate::SubagentProgress {
            subagent_id: _,
            parent_session_id: _,
            attempt_id: _,
            child_session_id,
            duration_ms,
            turn_count,
            tool_call_count,
            tokens_used,
            context_window_tokens,
            context_usage_pct,
            tools_used,
            error_count,
            active_model,
            active_reasoning_effort,
        } => {
            if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.attempt.duration_ms = Some(duration_ms);
                info.attempt.turn_count = Some(turn_count);
                info.attempt.tool_call_count = Some(tool_call_count);
                info.attempt.tokens_used = Some(tokens_used);
                info.attempt.context_window_tokens = Some(context_window_tokens);
                info.attempt.context_usage_pct = Some(context_usage_pct);
                info.attempt.tools_used = tools_used.into_iter().map(Arc::from).collect();
                info.attempt.error_count = Some(error_count);
                info.attempt.active_model = active_model.as_deref().map(Arc::from);
                info.attempt.active_reasoning_effort =
                    active_reasoning_effort.as_deref().map(Arc::from);
                info.attempt.last_progress_at = std::time::Instant::now();
            }
            sync_subagent_dispatch(
                agent,
                &child_session_id,
                active_model.as_deref(),
                active_reasoning_effort.as_deref(),
            );
            if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id)
                && context_window_tokens > 0
            {
                child_view
                    .session
                    .models
                    .override_context_window(context_window_tokens);
                if let Some(used) = child_view.context_state.as_ref().map(|state| state.used) {
                    // Progress carries the effective worker window. Keep an
                    // already-created usage snapshot from displaying the
                    // inherited/configured model's older denominator.
                    child_view.apply_context_used(used, context_window_tokens);
                }
            }
            let activity_label = agent
                .subagent_views
                .get(&child_session_id)
                .and_then(|cv| subagent_activity_label(cv));
            sync_subagent_activity(agent, &child_session_id, activity_label);
            true
        }
        XaiSessionUpdate::SubagentFinished {
            attempt_id: _,
            child_session_id,
            status,
            error,
            tool_calls,
            turns,
            duration_ms,
            tokens_used,
            ..
        } => {
            tracing::info!(
                child_session_id = %child_session_id,
                status = %status,
                tool_calls = tool_calls,
                turns = turns,
                duration_ms = duration_ms,
                "Subagent finished"
            );
            let elapsed_dur = std::time::Duration::from_millis(duration_ms);
            let info_ref = agent.subagent_sessions.get(&child_session_id);
            let entry_id = info_ref.and_then(|s| s.attempt.scrollback_entry_id);
            let is_background = info_ref.is_some_and(|s| s.attempt.is_background);
            let description = info_ref.map(|s| s.description.clone()).unwrap_or_default();
            let existing_terminal = info_ref.and_then(|info| info.attempt.terminal_entry_id);
            if let Some(eid) = entry_id {
                agent.scrollback.finish_running(eid);
            }
            sync_subagent_activity(agent, &child_session_id, None);
            sync_subagent_dispatch(agent, &child_session_id, None, None);
            if is_background {
                let terminal_entry_id = if let Some(eid) = existing_terminal
                    && let Some(entry) = agent.scrollback.get_by_id_mut(eid)
                {
                    if let RenderBlock::Subagent(ref mut sb) = entry.block {
                        sb.kind = match status.as_str() {
                            "completed" => {
                                crate::scrollback::blocks::SubagentBlockKind::Completed {
                                    elapsed: elapsed_dur,
                                }
                            }
                            "cancelled" => {
                                crate::scrollback::blocks::SubagentBlockKind::Cancelled {
                                    elapsed: elapsed_dur,
                                }
                            }
                            _ => crate::scrollback::blocks::SubagentBlockKind::Failed {
                                elapsed: elapsed_dur,
                                error: error.clone(),
                            },
                        };
                    }
                    entry.invalidate_cache();
                    Some(eid)
                } else {
                    let block = match status.as_str() {
                        "completed" => RenderBlock::Subagent(
                            crate::scrollback::blocks::SubagentBlock::completed(
                                description.as_ref(),
                                child_session_id.as_str(),
                                elapsed_dur,
                            ),
                        ),
                        "cancelled" => RenderBlock::Subagent(
                            crate::scrollback::blocks::SubagentBlock::cancelled(
                                description.as_ref(),
                                child_session_id.as_str(),
                                elapsed_dur,
                            ),
                        ),
                        _ => {
                            RenderBlock::Subagent(crate::scrollback::blocks::SubagentBlock::failed(
                                description.as_ref(),
                                child_session_id.as_str(),
                                elapsed_dur,
                                error.clone(),
                            ))
                        }
                    };
                    Some(agent.scrollback.push_block(block))
                };
                if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                    info.attempt.terminal_entry_id = terminal_entry_id;
                }
            } else if let Some(eid) = entry_id
                && let Some(entry) = agent.scrollback.get_by_id_mut(eid)
            {
                if let RenderBlock::Subagent(ref mut sb) = entry.block {
                    match status.as_str() {
                        "completed" => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Completed {
                                elapsed: elapsed_dur,
                            };
                        }
                        "cancelled" => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Cancelled {
                                elapsed: elapsed_dur,
                            };
                        }
                        _ => {
                            sb.kind = crate::scrollback::blocks::SubagentBlockKind::Failed {
                                elapsed: elapsed_dur,
                                error: error.clone(),
                            };
                        }
                    }
                }
                entry.invalidate_cache();
            }
            if let Some(info) = agent.subagent_sessions.get_mut(&child_session_id) {
                info.attempt.status = Some(Arc::from(status));
                info.attempt.error = error.map(Arc::from);
                info.attempt.duration_ms = Some(duration_ms);
                info.attempt.tool_calls = Some(tool_calls);
                info.attempt.turns = Some(turns);
                if tokens_used > 0 {
                    info.attempt.tokens_used = Some(tokens_used);
                }
                info.seal_current_attempt_tokens();
                info.attempt.pending_kill = false;
                info.attempt.kill_requested_at = None;
                info.attempt.last_progress_at = std::time::Instant::now();
                info.transcript.retry_disk_after_finish();
            }
            let resuming = agent.session.loading_replay;
            if let Some(child_view) = agent.subagent_views.get_mut(&child_session_id) {
                child_view.session.state = AgentState::Idle;
            }
            if !resuming {
                let outcome =
                    crate::app::subagent::evict_finished_child_view(agent, &child_session_id);
                if outcome == crate::app::subagent::EvictOutcome::Retained
                    && let Some(child_view) =
                        agent.child_view_for_live_update_mut(&child_session_id)
                {
                    crate::app::subagent::finalize_finished_child_view(child_view, elapsed_dur);
                }
            }
            true
        }
        XaiSessionUpdate::HookAnnotation { message, kind } => {
            if app.appearance.disable_plugins {
                return false;
            }
            tracing::debug!("Hook annotation: {message}");
            let event = match kind {
                HookAnnotationKind::Note => SessionEvent::HookAnnotation { message },
                HookAnnotationKind::ToolOutcome => SessionEvent::HookOutcome { message },
            };
            agent
                .scrollback
                .push_block(RenderBlock::session_event(event));
            true
        }
        XaiSessionUpdate::HookRunStarted {
            event_name,
            tool_name,
            count,
            prompt_id,
        } => {
            if app.appearance.disable_plugins || is_foreign_hook_batch(agent, prompt_id.as_deref())
            {
                return false;
            }
            let batch = crate::acp::tracker::HookBatchId {
                event_name,
                tool_name,
            };
            agent
                .session
                .tracker
                .set_hooks_running(batch, count, meta.agent_timestamp_ms);
            false
        }
        XaiSessionUpdate::HookExecution {
            event_name,
            tool_name,
            runs,
            prompt_id,
        } => {
            let batch = crate::acp::tracker::HookBatchId {
                event_name: event_name.clone(),
                tool_name,
            };
            let mut redraw = !is_foreign_hook_batch(agent, prompt_id.as_deref())
                && agent.session.tracker.clear_hooks_running(&batch);
            if app.appearance.disable_plugins {
                return redraw;
            }
            for line in runs.iter().filter_map(|r| failed_hook_line(&event_name, r)) {
                agent.scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::HookOutcome { message: line },
                ));
                redraw = true;
            }
            redraw
        }
        XaiSessionUpdate::HooksChanged {
            hooks,
            project_trusted,
            load_errors,
        } => {
            if let Some(ref mut modal) = agent.extensions_modal {
                use crate::views::extensions_modal::TabDataState;
                modal.seed_hook_groups_once(&hooks);
                modal.hooks_data =
                    TabDataState::Loaded(distill_hooks_plugins_types::HooksListResponse {
                        hooks,
                        project_trusted,
                        load_errors,
                    });
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::PluginsChanged { plugins } => {
            if let Some(ref mut modal) = agent.extensions_modal {
                use crate::views::extensions_modal::TabDataState;
                modal.seed_plugin_groups_once(&plugins);
                modal.plugins_data =
                    TabDataState::Loaded(distill_hooks_plugins_types::PluginsListResponse {
                        plugins,
                    });
                if !matches!(modal.skills_data, TabDataState::Loading) {
                    modal.skills_data = TabDataState::Loading;
                    plugins_changed_needs_skills_refetch = true;
                }
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::SessionSummaryGenerated { session_summary } => {
            let title_is_manual = session_notif.meta.as_ref().and_then(|v| {
                v.get(distill_shell::extensions::notification::TITLE_IS_MANUAL_META_KEY)
                    .and_then(|v| v.as_bool())
            });
            match title_is_manual {
                Some(true) => {
                    if let Some(clean) = distill_shell::session::persistence::sanitize_and_cap_title(
                        &session_summary,
                    ) {
                        agent.display_name = Some(clean.clone());
                        agent.generated_session_title = Some(clean);
                        agent.title_unpin_committed = false;
                    }
                }
                other => {
                    let pin = if other == Some(false) {
                        agent.title_unpin_committed = true;
                        agent.display_name.take()
                    } else {
                        None
                    };
                    let decoded = decode_html_entities(&session_summary);
                    if let Some(clean) =
                        distill_shell::session::persistence::sanitize_and_cap_title(&decoded)
                    {
                        agent.generated_session_title = Some(clean);
                    } else if other == Some(false)
                        && agent.generated_session_title.as_deref() == pin.as_deref()
                    {
                        agent.generated_session_title = None;
                    }
                }
            }
            true
        }
        XaiSessionUpdate::LastTurnSummary {
            summary,
            prompt_id: _,
        } => {
            agent.set_last_turn_summary(Some(summary));
            true
        }
        XaiSessionUpdate::SessionRecap { summary, auto } => {
            use crate::scrollback::block::RenderBlock;
            use crate::scrollback::blocks::SessionEvent;
            if should_drop_late_auto_recap(auto, meta.is_replay, agent) {
                tracing::debug!("dropping late auto SessionRecap; CLI not idle for recap");
                false
            } else if should_drop_duplicate_auto_recap(auto, meta.is_replay, &agent.scrollback) {
                tracing::debug!(
                    "dropping duplicate live auto SessionRecap; recap already shown since last user turn"
                );
                app.notification_service.focus_tracker.mark_recap_shown();
                false
            } else {
                app.notification_service.focus_tracker.mark_recap_shown();
                let recap_block = RenderBlock::session_event(SessionEvent::Recap { summary, auto });
                apply_recap_block(agent, auto, recap_block);
                true
            }
        }
        XaiSessionUpdate::SessionRecapUnavailable => {
            if meta.is_replay {
                false
            } else if let Some(pending_id) = agent.pending_recap_entry.take() {
                agent.scrollback.remove_entry(pending_id);
                agent.show_toast(crate::app::dispatch::recap_unavailable_toast(
                    crate::app::dispatch::scrollback_has_user_messages(&agent.scrollback),
                ));
                true
            } else {
                false
            }
        }
        XaiSessionUpdate::ModelAutoSwitched {
            previous_model_id,
            new_model_id,
            reason,
        } => {
            use crate::scrollback::block::RenderBlock;
            use crate::scrollback::blocks::SessionEvent;
            let available_count = agent.session.models.available.len();
            let available_keys: Vec<&str> = agent
                .session
                .models
                .available
                .keys()
                .take(10)
                .map(|m| m.0.as_ref())
                .collect();
            tracing::warn!(
                session_id = session_notif.session_id.0.as_ref(),
                previous = %previous_model_id,
                new = %new_model_id,
                available_count,
                available_keys = ?available_keys,
                "Model auto-switched: previous model no longer available"
            );
            crate::unified_log::warn(
                "model auto-switched: previous model unavailable",
                Some(session_notif.session_id.0.as_ref()),
                Some(serde_json::json!({
                    "previous_model": previous_model_id.as_str(),
                    "new_model": new_model_id.as_str(),
                    "available_count": available_count,
                    "available_keys": available_keys,
                })),
            );
            agent.scrollback.push_block(RenderBlock::session_event(
                SessionEvent::ModelUnavailable {
                    previous_model_id,
                    new_model_id,
                    reason,
                },
            ));
            true
        }
        XaiSessionUpdate::ModelChanged {
            model_id,
            reasoning_effort,
            context_window,
        } => {
            if agent.session.model_switch_pending {
                tracing::debug!(
                    session_id = session_notif.session_id.0.as_ref(),
                    model_id = %model_id,
                    "ignoring ModelChanged broadcast — local switch is in flight"
                );
                return false;
            }
            use distill_shell::sampling::types::ReasoningEffort;
            let new_model_id = acp::ModelId::new(model_id.clone());
            if !agent.session.models.available.contains_key(&new_model_id) {
                if distill_shell::agent::chat_modes::process_chat_mode_enabled() {
                    agent.session.models.available.insert(
                        new_model_id.clone(),
                        acp::ModelInfo::new(new_model_id.clone(), model_id.clone()),
                    );
                } else {
                    tracing::warn!(
                        session_id = session_notif.session_id.0.as_ref(),
                        model_id = %model_id,
                        "ignoring ModelChanged broadcast — model not in local catalog"
                    );
                    return false;
                }
            }
            let effort = reasoning_effort
                .as_deref()
                .and_then(|s| s.parse::<ReasoningEffort>().ok());
            let prev_model = agent.session.models.current.clone();
            let prev_effort = agent.session.models.reasoning_effort;
            agent
                .session
                .models
                .set_current(new_model_id.clone(), effort);
            agent.session.user_model_preference = Some(new_model_id.clone());
            if let Some(window) = context_window {
                // Keep the producer's effective session window as the source
                // for later token-only updates; catalog metadata is only a
                // fallback when the sender omitted this field.
                agent.session.models.override_context_window(window);
            } else {
                agent.session.models.clear_context_window_override();
            }
            if let Some(used) = agent.context_state.as_ref().map(|state| state.used) {
                if let Some(window) = context_window {
                    agent.apply_context_used(used, window);
                } else {
                    // Older senders do not carry the authoritative session
                    // override; retain the catalog fallback for those wires.
                    refresh_context_used(agent, used);
                }
            }
            let resolved_effort = agent.session.models.reasoning_effort;
            let actually_changed =
                prev_model.as_ref() != Some(&new_model_id) || prev_effort != resolved_effort;
            if actually_changed {
                tracing::info!(
                    session_id = session_notif.session_id.0.as_ref(),
                    model_id = %model_id,
                    effort = ?resolved_effort,
                    "ModelChanged broadcast applied (remote switch)"
                );
            }
            actually_changed
        }
        XaiSessionUpdate::MemoryFiles {
            files,
            enabled,
            disabled_reason,
            capture_enabled,
            dream_enabled,
        } => {
            let listing = distill_shell::extensions::memory::MemoryListing {
                files,
                enabled,
                disabled_reason,
                capture_enabled,
                dream_enabled,
            };
            agent.active_modal = Some(crate::views::modal::ActiveModal::MemoryBrowser {
                state: Box::new(crate::views::memory_modal::MemoryModalState::from_listing(
                    listing,
                )),
            });
            true
        }
        update @ XaiSessionUpdate::WorkflowUpdated { .. } => ingest_workflow_update(agent, update),
        XaiSessionUpdate::GoalUpdated {
            goal_id,
            objective,
            status,
            phase,
            token_budget,
            tokens_used,
            elapsed_ms,
            total_deliverables,
            completed_deliverables,
            current_deliverable_id,
            current_deliverable_title,
            current_subagent_role,
            total_worker_rounds,
            total_verify_rounds,
            token_baseline,
            finished_subagent_tokens,
            live_subagent_tokens,
            live_tokens_by_model,
            live_context_pct,
            live_turn_count,
            live_tool_call_count,
            last_event,
            last_event_detail,
            last_event_timestamp,
            pause_message,
            classifier_runs_attempted,
            classifier_max_runs,
            last_classifier_verdict,
            last_classifier_details_path,
            verifying_completion,
            planning,
            ..
        } => {
            let new_status = GoalDisplayStatus::parse(&status);
            let just_completed = new_status == GoalDisplayStatus::Complete
                && agent
                    .goal_state
                    .as_ref()
                    .is_none_or(|g| g.status != GoalDisplayStatus::Complete);
            if status == "cleared" {
                if let Some(g) = agent.goal_state.take() {
                    agent.last_cleared_goal_id = Some(g.goal_id);
                }
                agent.show_goal_detail = false;
                true
            } else if agent.last_cleared_goal_id.as_deref() == Some(goal_id.as_str()) {
                false
            } else {
                let elapsed_floor_ms = agent
                    .goal_state
                    .as_ref()
                    .filter(|g| g.goal_id == goal_id)
                    .map(|g| g.live_elapsed_ms())
                    .unwrap_or(0)
                    .max(elapsed_ms);
                if just_completed {
                    agent.scrollback.push_block(RenderBlock::session_event(
                        SessionEvent::GoalCompleted {
                            elapsed: std::time::Duration::from_millis(elapsed_floor_ms),
                        },
                    ));
                }
                let last_classifier_details_exists = last_classifier_details_path
                    .as_deref()
                    .is_some_and(|p| std::path::Path::new(p).exists());
                agent.goal_state = Some(GoalDisplayState {
                    goal_id,
                    objective,
                    status: new_status,
                    phase: GoalDisplayPhase::parse(&phase),
                    token_budget,
                    tokens_used,
                    elapsed_ms,
                    total_deliverables,
                    completed_deliverables,
                    current_deliverable_id,
                    current_deliverable_title,
                    current_subagent_role,
                    total_worker_rounds,
                    total_verify_rounds,
                    live_subagent_tokens,
                    live_tokens_by_model,
                    live_context_pct,
                    live_turn_count,
                    live_tool_call_count,
                    last_event,
                    last_event_detail,
                    last_event_timestamp,
                    token_baseline,
                    finished_subagent_tokens,
                    deliverables: Vec::new(),
                    pause_message,
                    classifier_runs_attempted,
                    classifier_max_runs,
                    last_classifier_verdict,
                    last_classifier_details_path,
                    last_classifier_details_exists,
                    verifying_completion: verifying_completion.unwrap_or(false),
                    planning: planning.unwrap_or(false),
                    received_at: std::time::Instant::now(),
                    elapsed_floor_ms,
                });
                true
            }
        }
        XaiSessionUpdate::InteractionResolved { tool_call_id } => {
            agent.dismiss_resolved_interaction(&tool_call_id)
        }
        XaiSessionUpdate::PlanKept { plan_uri, content } => {
            if meta.is_replay || agent.session.loading_replay {
                false
            } else {
                let next = crate::app::agent_view::KeptPlan::from_signal(&plan_uri, content);
                if next.is_kept() {
                    agent.kept_plan = next;
                    if agent.session.state.is_turn_running() {
                        agent.refresh_post_turn_plan_review();
                    } else {
                        agent.open_post_turn_plan_review();
                    }
                    true
                } else {
                    false
                }
            }
        }
        XaiSessionUpdate::PlanCleared => {
            if meta.is_replay
                || agent.session.loading_replay
                || agent.plan_mode_pending == Some(true)
            {
                false
            } else {
                agent.forget_waiting_plan();
                true
            }
        }
        XaiSessionUpdate::PlanExecuting => {
            if meta.is_replay || agent.session.loading_replay {
                false
            } else {
                agent.commit_post_turn_plan_approved();
                true
            }
        }
        XaiSessionUpdate::SessionStatus(status) => {
            let context_snapshot = status
                .context_window
                .context_tokens
                .zip(status.context_window.context_window_size);
            agent.status_context = Some(*status);
            if let Some((used, window)) = context_snapshot {
                // SessionStatus is the existing transient status/context rail
                // for the main session. Keep the context bar paired with the
                // effective per-round window published by the shell.
                agent.session.models.override_context_window(window);
                agent.apply_context_used(used, window);
            }
            status_snapshot_applied = true;
            false
        }
        _ => {
            tracing::trace!(
                "Ignoring {}: {:?}",
                notif.method.as_ref(),
                std::mem::discriminant(&session_notif.update)
            );
            return false;
        }
    };
    let mut changed = changed;
    if status_snapshot_applied && is_active {
        app.refresh_status_line_now();
        changed |= app.status_line.take_changed();
    }
    if plugins_changed_needs_skills_refetch {
        if let Some(agent) = app.agents.get(&parent_id)
            && let Some(session_id) = agent.session.session_id.clone()
        {
            app.pending_effects.push(Effect::FetchSkillsList {
                agent_id: parent_id,
                session_id,
            });
        } else if let Some(agent) = app.agents.get_mut(&parent_id)
            && let Some(ref mut modal) = agent.extensions_modal
        {
            modal.skills_data =
                crate::views::extensions_modal::TabDataState::Error("No active session".into());
        } else {
            tracing::warn!("PluginsChanged: agent or modal disappeared before skills re-fetch");
        }
    }
    if let Some(agent) = app.agents.get_mut(&parent_id) {
        if let Some(seq) = meta.event_seq
            && !meta.is_replay
            && !is_workflow_update
        {
            agent.last_applied_xai_event_seq = Some(
                agent
                    .last_applied_xai_event_seq
                    .map_or(seq, |last| last.max(seq)),
            );
        }
        if let Some(id) = meta.event_id {
            agent.advance_last_seen_event_id(id, meta.event_seq);
        }
    }
    if let Some((payload, origin)) = pending_finish_for_spawn
        && let Ok(params) = serde_json::value::to_raw_value(&payload)
    {
        let deferred = acp::ExtNotification::new("x.ai/session/update", params.into());
        let _ = handle_session_notification_with_origin(&deferred, app, origin);
    }
    if !app.reconnect_pending
        && let Some(agent) = app.agents.get(&parent_id)
        && agent.running_wake_turn.is_none()
        && agent.session.state.is_idle()
        && !agent.session.pending_prompts.is_empty()
    {
        let effects = crate::app::dispatch::maybe_drain_queue_and_note_peek(app, parent_id);
        app.pending_effects.extend(effects);
    }
    if let Some(outcome) = terminal_outcome {
        return super::super::turn_completion::apply_terminal_outcome(
            outcome, app, parent_id, is_active,
        );
    }
    changed && is_active
}
/// Handle a Distill session notification that targets a child (subagent) session.
/// Events like compaction, retry, and memory flush are emitted by the child's `acp_session` with the *child's* `session_id`.
/// This routes them to the correct child view and updates `SubagentInfo` where appropriate.
pub(super) fn handle_child_session_notification(
    update: XaiSessionUpdate,
    child_sid: &str,
    agent: &mut AgentView,
    is_api_key_auth: bool,
) -> bool {
    match update {
        XaiSessionUpdate::AutoCompactStarted { .. }
        | XaiSessionUpdate::AutoCompactCompleted { .. }
        | XaiSessionUpdate::AutoCompactFailed { .. }
        | XaiSessionUpdate::AutoCompactCancelled { .. }
        | XaiSessionUpdate::RetryState(_)
        | XaiSessionUpdate::MemoryFlushCompleted { .. }
        | XaiSessionUpdate::MemoryDreamCompleted { .. }
        | XaiSessionUpdate::MemorySessionSaved { .. } => {
            let mut changed = false;
            if let Some(child_view) = agent.child_view_for_live_update_mut(child_sid) {
                changed = apply_child_view_session_event(child_view, &update, is_api_key_auth);
            }
            if let XaiSessionUpdate::AutoCompactCompleted { tokens_after, .. } = update
                && let Some(info) = agent.subagent_sessions.get_mut(child_sid)
            {
                info.attempt.tokens_used = Some(tokens_after);
                if let Some(cw) = info.attempt.context_window_tokens.filter(|&cw| cw > 0) {
                    info.attempt.context_usage_pct = Some(
                        distill_token_estimation::usage_percentage_u8(tokens_after, cw),
                    );
                }
            }
            changed
        }
        XaiSessionUpdate::ToolCallDeltaChunk {
            ref name,
            tool_index,
            ..
        } => {
            let Some(child_view) = agent.subagent_views.get_mut(child_sid) else {
                return false;
            };
            if child_view.session.loading_replay {
                return false;
            }
            let row_live = agent
                .subagent_sessions
                .get(child_sid)
                .is_some_and(|info| info.is_running());
            if !row_live {
                return false;
            }
            if !child_view
                .session
                .tracker
                .note_tool_call_arguments_delta(name.as_deref(), tool_index)
            {
                return false;
            }
            let activity_label = subagent_activity_label(child_view);
            sync_subagent_activity(agent, child_sid, activity_label);
            true
        }
        _ => false,
    }
}
/// Apply one Grok session event to a child view.
/// The live child routing above and the from-disk child replay (`crate::app::subagent::replay_inherited_updates`) share this rendering.
/// A rebuilt transcript therefore keeps the same compaction/retry markers the live one had.
pub(crate) fn apply_child_view_session_event(
    child_view: &mut AgentView,
    update: &XaiSessionUpdate,
    is_api_key_auth: bool,
) -> bool {
    apply_compaction_or_retry_update(child_view, update, is_api_key_auth)
}
fn apply_compaction_or_retry_update(
    agent: &mut AgentView,
    update: &XaiSessionUpdate,
    is_api_key_auth: bool,
) -> bool {
    use crate::app::agent::AgentCommand;
    use crate::app::cancel_latency::TurnEnd;
    use distill_shell::extensions::notification::MODEL_FAMILY_SWITCH_COMPACT_BANNER;
    use std::time::Instant;
    let changed = apply_session_event(
        update,
        &mut agent.session,
        &mut agent.scrollback,
        is_api_key_auth,
    );
    if let Some(used) = compaction_context_refresh(update) {
        refresh_context_used(agent, used);
    }
    match update {
        XaiSessionUpdate::AutoCompactStarted { reason, .. } => {
            if agent.session.state.is_idle()
                && !agent.session.loading_replay
                && reason == MODEL_FAMILY_SWITCH_COMPACT_BANNER
            {
                agent
                    .session
                    .start_command(AgentCommand::SwitchModelCompact);
                agent.running_wake_turn = None;
                agent.turn_started_at = Some(Instant::now());
            }
        }
        XaiSessionUpdate::AutoCompactCompleted { .. }
        | XaiSessionUpdate::AutoCompactFailed { .. }
        | XaiSessionUpdate::AutoCompactCancelled { .. } => {
            if agent.session.state.is_switch_model_compact() {
                agent.session.finish_command();
                agent.mark_turn_finished(TurnEnd::Completed);
                agent.activity_started_at = None;
                agent.last_activity = None;
            }
        }
        _ => {}
    }
    changed
}
/// Apply a compaction or retry event to a session's activity state and scrollback.
/// Test-only shim so dispatch tests can replay notification sequences (e.g. `RetryState::Retrying` then `Exhausted`) through the production handler.
/// The Retrying arm clears the `in_flight_prompt` rewind stash, which a fixture setting fields directly would miss.
#[cfg(test)]
pub(crate) fn apply_session_event_for_test(
    update: &XaiSessionUpdate,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
) -> bool {
    apply_session_event(update, session, scrollback, false)
}
fn memory_capture_system_message(
    activity: &str,
    from_turn: u32,
    through_turn: u32,
    attempt: u32,
) -> String {
    let activity = match activity {
        "queued" => "queued",
        "running" => "running",
        "completed" => "completed",
        "no_op" => "completed with no changes",
        "retry" => "scheduled for retry",
        "failed" => "failed",
        _ => "updated",
    };
    let attempt_suffix = if attempt > 1 {
        format!(" (attempt {attempt})")
    } else {
        String::new()
    };
    format!("Memory capture {activity} for turns {from_turn}-{through_turn}{attempt_suffix}")
}
pub(super) fn apply_session_event(
    update: &XaiSessionUpdate,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
    is_api_key_auth: bool,
) -> bool {
    match update {
        XaiSessionUpdate::AutoCompactStarted {
            percentage, reason, ..
        } => {
            tracing::info!("Auto-compact started: {percentage}% context used");
            if session.compact_held_prompt.is_none() {
                session.compact_held_prompt = session.in_flight_prompt.clone();
            }
            session.in_flight_prompt = None;
            session.set_compaction_activity(Some(TurnActivity::AutoCompacting));
            scrollback.push_block(RenderBlock::session_event(
                SessionEvent::CompactionStarted {
                    percentage: *percentage,
                    reason: reason.clone(),
                },
            ));
            true
        }
        XaiSessionUpdate::AutoCompactCompleted {
            tokens_before,
            tokens_after,
            elapsed_ms,
            ..
        } => {
            tracing::info!("Auto-compact completed: {tokens_after} tokens after");
            session.set_compaction_activity(None);
            session.compact_held_prompt = None;
            if session.loading_replay || session.state.is_switch_model_compact() {
                scrollback.push_block(RenderBlock::session_event(
                    SessionEvent::CompactionCompleted {
                        tokens_before: *tokens_before,
                        tokens_after: *tokens_after,
                        elapsed_ms: *elapsed_ms,
                    },
                ));
            } else {
                session.defer_compaction(*tokens_before, *tokens_after, *elapsed_ms);
            }
            true
        }
        XaiSessionUpdate::AutoCompactFailed { error } => {
            tracing::error!(error = %error, "Auto-compaction failed");
            session.set_compaction_activity(None);
            scrollback.push_block(RenderBlock::session_event(SessionEvent::CompactionFailed {
                error: error.clone(),
            }));
            true
        }
        XaiSessionUpdate::AutoCompactCancelled { .. } => {
            tracing::info!("Auto-compact cancelled");
            session.set_compaction_activity(None);
            session.compact_held_prompt = None;
            scrollback.push_block(RenderBlock::session_event(
                SessionEvent::CompactionCancelled,
            ));
            true
        }
        XaiSessionUpdate::RetryState(retry) => {
            tracing::debug!("Retry state: {retry:?}");
            apply_retry_state(retry, session, scrollback, is_api_key_auth);
            true
        }
        XaiSessionUpdate::ImageDropped { notes } => {
            let message = notes.join("\n");
            tracing::info!("Image dropped: {message}");
            scrollback.push_block(RenderBlock::system(message));
            true
        }
        XaiSessionUpdate::MemoryCaptureActivity {
            activity,
            from_turn,
            through_turn,
            attempt,
            detail: _,
            memories,
        } => {
            if activity == "completed" && !memories.is_empty() {
                scrollback.push_block(RenderBlock::memory_capture(
                    *from_turn,
                    *through_turn,
                    memories.clone(),
                ));
                return true;
            }
            scrollback.push_block(RenderBlock::system(memory_capture_system_message(
                activity,
                *from_turn,
                *through_turn,
                *attempt,
            )));
            true
        }
        _ => false,
    }
}
/// True if the trailing run of session/system blocks contains a [`SessionEvent::CompactionFailed`].
/// Used so we don't stack a [`SessionEvent::ContextTooLarge`] prompt on top of the compaction handler's "too large to compact" message.
pub(super) fn scrollback_has_recent_compaction_failed(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(ev.event, SessionEvent::CompactionFailed { .. }) {
                    return true;
                }
            }
            Some(RenderBlock::System(_)) => {}
            _ => break,
        }
    }
    false
}
/// Handle an `ImageCompressed` notification.
/// A successful compression is deliberately invisible in the TUI (log-only): it needs no user action.
/// Only the re-encode *fallback* (the oversized original was kept) shows, as a persistent scrollback warning that is rebuilt on session replay.
pub(super) fn apply_image_compressed(
    agent: &mut AgentView,
    images: &[distill_shell::extensions::notification::ImageCompressedEntry],
    message: &str,
) -> bool {
    if images.is_empty() {
        tracing::warn!("Image re-encode fallback: {message}");
        agent
            .scrollback
            .push_block(RenderBlock::system(message.to_owned()));
        return true;
    }
    tracing::info!("Image compressed: {message}");
    false
}
pub(super) fn apply_retry_state(
    retry: &distill_shell::extensions::notification::RetryState,
    session: &mut AgentSession,
    scrollback: &mut crate::scrollback::state::ScrollbackState,
    is_api_key_auth: bool,
) {
    let mut is_credit_limit = false;
    let mut is_reauth = false;
    use distill_shell::extensions::notification::RetryState;
    match retry {
        RetryState::Retrying {
            attempt,
            max_retries,
            reason,
            error_type,
        } => {
            session.set_retry_activity(Some(TurnActivity::Retrying {
                attempt: *attempt,
                max_retries: *max_retries,
                reason: reason.clone(),
                error_type: error_type.clone(),
            }));
        }
        RetryState::Exhausted {
            attempts,
            reason,
            is_rate_limited: rate_limited,
        } => {
            session.set_retry_activity(None);
            session.rate_limited = *rate_limited;
            if *rate_limited {
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::RateLimitHit {
                        model_id: session
                            .models
                            .current
                            .as_ref()
                            .map(|m| m.0.to_string())
                            .unwrap_or_default(),
                        attempts: *attempts,
                    },
                );
            }
            is_credit_limit = super::super::dispatch::is_credit_limit_error(None, reason);
            let is_free_usage = *rate_limited
                && distill_shell::sampling::error::is_free_usage_exhausted_error(reason);
            if is_credit_limit {
                session.credit_limit_blocked = true;
            } else if is_free_usage {
                session.free_usage_blocked = true;
            } else if !*rate_limited && is_reauthable_failure(None, reason) {
                is_reauth = true;
                scrollback.push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
            } else if *rate_limited {
                let error = crate::app::effects::sanitize_user_error(
                    &format_rate_limited_user_message(Some(reason.as_str()), is_api_key_auth),
                );
                scrollback.push_block(RenderBlock::session_event(SessionEvent::RetryFailed {
                    error,
                    error_type: None,
                }));
            } else {
                scrollback.push_block(RenderBlock::session_event(
                    crate::app::error_display::format_request_failure(None, None, reason)
                        .into_session_event(),
                ));
            }
        }
        RetryState::Failed {
            error_type,
            message,
        } => {
            session.set_retry_activity(None);
            let wire = crate::app::error_display::WireErrorType::parse(Some(error_type.as_str()));
            if wire == crate::app::error_display::WireErrorType::EncryptedContentMismatch {
                session.model_incompatible = true;
            }
            is_credit_limit = super::super::dispatch::is_credit_limit_error(None, message);
            if is_credit_limit {
                session.credit_limit_blocked = true;
            } else if is_reauthable_failure(Some(error_type.as_str()), message) {
                is_reauth = true;
                scrollback.push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
            } else if wire == crate::app::error_display::WireErrorType::DiskFull {
                if !crate::app::dispatch::scrollback_has_recent_disk_full(scrollback) {
                    scrollback.push_block(RenderBlock::session_event(SessionEvent::DiskFull));
                }
            } else if wire == crate::app::error_display::WireErrorType::ContextLength {
                if !scrollback_has_recent_compaction_failed(scrollback) {
                    scrollback.push_block(RenderBlock::session_event(
                        SessionEvent::ContextTooLarge {
                            model: session
                                .models
                                .current
                                .as_ref()
                                .map(|id| id.0.to_string()),
                            stated_limit: crate::app::error_display::stated_context_limit(message),
                        },
                    ));
                }
            } else if wire == crate::app::error_display::WireErrorType::EncryptedContentMismatch
                || wire == crate::app::error_display::WireErrorType::LegacyAuth
            {
                scrollback.push_block(RenderBlock::session_event(SessionEvent::RetryFailed {
                    error: message.clone(),
                    error_type: Some(error_type.clone()),
                }));
            } else {
                scrollback.push_block(RenderBlock::session_event(
                    crate::app::error_display::format_request_failure(None, Some(wire), message)
                        .into_session_event(),
                ));
            }
        }
    }
    if is_credit_limit {
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::CreditLimitHit {
            model_id: session
                .models
                .current
                .as_ref()
                .map(|m| m.0.to_string())
                .unwrap_or_default(),
        });
    } else if !is_reauth {
        session.in_flight_prompt = None;
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanModeTransition {
    Unchanged,
    /// Also claims another attached client's `session/set_mode`: the wire has no trigger field, and that misattribution is accepted.
    EnteredByAgent,
    EnteredByUser,
    /// No scrollback row here: exit rows carry the review verdict, which only the local `close_plan_review` knows; Shift+Tab and `/plan` exits already show a banner.
    Exited,
}
/// Do not be tempted to infer mode from tool-call titles.
/// A substring match would silently brick sessions whenever any tool happens to mention `enter_plan_mode`.
/// Entries are attributed locally: every pager-side path stages `plan_mode_pending` before its `session/set_mode`, and the agent's tool call never does.
/// Mode bits still apply on replay; keep/review side effects do not. PlanKept
/// and post-turn open already skip historical updates.
pub(crate) fn detect_plan_mode_change_replayed(
    update: &acp::SessionUpdate,
    agent: &mut AgentView,
    is_replay: bool,
) -> Option<PlanModeTransition> {
    use distill_tools::types::SessionMode;
    let acp::SessionUpdate::CurrentModeUpdate(cmu) = update else {
        return None;
    };
    let mode = SessionMode::from_id(cmu.current_mode_id.0.as_ref());
    let was_active = agent.plan_mode_active;
    let now_active = mode.is_plan();
    let staged = agent.plan_mode_pending;
    let user_requested = staged.is_some();
    let user_left = staged == Some(false);
    agent.plan_mode_active = now_active;
    agent.plan_mode_pending = None;
    agent.session_mode = mode;
    agent.session_mode_pending = None;
    let historical = is_replay || agent.session.loading_replay;
    if !historical && (!now_active && (was_active || agent.session_mode == SessionMode::Default)) {
        if staged == Some(true) {
            agent.plan_mode_pending = Some(true);
        } else {
            if let Some(commit) = agent.pending_post_turn_commit.take() {
                agent.commit_post_turn_plan_review(commit);
            } else if agent.session_mode == SessionMode::Default && user_left {
                agent.commit_post_turn_plan_abandoned();
            }
            agent.forget_waiting_plan();
        }
    }
    if !historical && !was_active && now_active {
        agent.open_post_turn_plan_review();
    }
    if was_active != now_active {
        tracing::info!(
            mode_id = %cmu.current_mode_id.0,
            plan_active = now_active,
            "Plan mode state updated (from CurrentModeUpdate)"
        );
    }
    Some(match (was_active, now_active) {
        (false, true) if user_requested => PlanModeTransition::EnteredByUser,
        (false, true) => PlanModeTransition::EnteredByAgent,
        (true, false) => PlanModeTransition::Exited,
        (false, false) | (true, true) => PlanModeTransition::Unchanged,
    })
}
#[cfg(test)]
mod memory_capture_privacy_tests {
    use super::memory_capture_system_message;
    #[test]
    fn trusted_capture_copy_only_accepts_typed_status_values() {
        let untrusted = "failed: /Users/alice/private\nhttps://untrusted.example";
        let message = memory_capture_system_message(untrusted, 2, 4, 3);
        assert_eq!(message, "Memory capture updated for turns 2-4 (attempt 3)");
        assert!(!message.contains(untrusted));
    }
}
