// Modified for Distill by Samuel Fajreldines, 2026.
//! The session actor's main loop (`run_session`): command dispatch, the idle timer arms, and the free helpers only the loop consumes.
#![allow(clippy::items_after_test_module)]
use super::*;
use distill_telemetry::instrument_task;
use distill_telemetry::region::Parent;
use distill_telemetry::session_end::{self, Phase, SharedSessionEndTimer};
/// The `YoloToggled` event to emit after `set_yolo_mode(requested)`: `Some(actual)` only on a real change.
/// Callers MUST pass the post-call state read back via `is_yolo_mode()`, never the request.
/// Under the always-approve pin the manager clamps a requested ON to OFF, so reporting the request would announce a turn-on that never happened.
pub(super) fn yolo_toggle_report(was: bool, actual: bool) -> Option<bool> {
    (was != actual).then_some(actual)
}
#[cfg(test)]
mod yolo_toggle_report_tests {
    use super::yolo_toggle_report;
    /// A pin-clamped enable (requested ON but actual stays OFF) reports no change, so no spurious "turned on" event/telemetry is emitted.
    /// Real flips report the actual new state.
    #[test]
    fn reports_actual_state_change_only() {
        assert_eq!(yolo_toggle_report(false, false), None);
        assert_eq!(yolo_toggle_report(false, true), Some(true));
        assert_eq!(yolo_toggle_report(true, false), Some(false));
        assert_eq!(yolo_toggle_report(true, true), None);
    }
}
fn spawn_dream_check(session: &Arc<SessionActor>) -> tokio::task::JoinHandle<()> {
    let session = session.clone();
    tokio::task::spawn_local(async move {
        session.memory.await_init_reindex().await;
        session.maybe_run_dream().await;
    })
}
/// Abort a still-running dream and wait for it to stop, so session-end index work never overlaps it.
/// A finished dream makes this a no-op; a live one is parked on the model call, so the abort returns
/// at once without stamping the marker, leaving the gate open for the next launch.
async fn stop_dream(dream_task: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = dream_task.take() {
        handle.abort();
        let _ = handle.await;
    }
}
/// Best-effort removal of this session's scratch staging on teardown.
/// A no-op in builds without a scratch producer.
fn cleanup_session_scratch(_session: &SessionActor) {}
const DEFERRED_START_CANCEL_JOIN: std::time::Duration = std::time::Duration::from_millis(500);
pub(super) struct DeferredStart {
    cancel: tokio_util::sync::CancellationToken,
    task: Option<tokio_util::task::AbortOnDropHandle<()>>,
}
impl DeferredStart {
    pub(super) fn new() -> Self {
        Self {
            cancel: tokio_util::sync::CancellationToken::new(),
            task: None,
        }
    }
    fn arm(&mut self, handle: tokio::task::JoinHandle<()>) {
        let handle = tokio_util::task::AbortOnDropHandle::new(handle);
        if self.cancel.is_cancelled() {
            drop(handle);
            return;
        }
        self.task = Some(handle);
    }
    async fn seal_and_join(&mut self) {
        self.cancel.cancel();
        if let Some(mut start) = self.task.take() {
            tokio::select! {
                biased;
                _ = &mut start => {}
                _ = tokio::time::sleep(DEFERRED_START_CANCEL_JOIN) => {}
            }
        }
    }
}
/// SessionEnd hooks and stop dispatch.
/// Shared so the channel-closed and Shutdown paths cannot drift on hook ordering (memory save still runs after this).
pub(super) async fn fire_session_end_hooks(
    session: &SessionActor,
    reason: &str,
    timer: &SharedSessionEndTimer,
    start: &mut DeferredStart,
) {
    let span = session_end::span(Phase::Hooks);
    start.seal_and_join().await;
    let envelope = session.fire_hook(
        distill_hooks::event::HookEventName::SessionEnd,
        None,
        distill_hooks::event::HookPayload::SessionEnd {
            reason: reason.to_string(),
            subagent_type: session.subagent_type_label(),
            turn_count: None,
            tool_call_count: None,
        },
    );
    if let Some(registry) = session.hook_registry.borrow().clone() {
        let _dispatch = session_end::timed_child(timer, Phase::HooksDispatch, span.span());
        let ctx = session.hook_run_ctx();
        let results = distill_hooks::dispatcher::dispatch_non_blocking(
            &registry,
            distill_hooks::event::HookEventName::SessionEnd,
            &envelope,
            &ctx,
        )
        .await;
        session
            .send_hook_execution(&HookBatch::from_envelope(&envelope), &results)
            .await;
    }
    let _stop = session_end::timed_child(timer, Phase::HooksStop, span.span());
    session.dispatch_session_end_stop(reason).await;
}
/// Cancel the feedback sync loop, drain/sync under exit budgets, persist background-task state, and drop scratch.
/// Owns the single final signal sync via [`FeedbackManager::shutdown`]; the sync loop cancel arm does not sync.
/// `FeedbackManager::shutdown` short-circuits force_sync/drain when telemetry is off or the session is empty with nothing pending.
async fn finish_session_exit_feedback(session: &SessionActor, timer: &SharedSessionEndTimer) {
    let span = session_end::span(Phase::Feedback);
    if let Some(cancel) = &session.sync_loop_cancel {
        cancel.cancel();
    }
    {
        let _drain = session_end::timed_child(timer, Phase::FeedbackDrain, span.span());
        session
            .feedback_manager
            .shutdown(session.upload_queue.get())
            .await;
    }
    if !session.startup_hints.is_subagent {
        let _tasks = session_end::timed_child(timer, Phase::BackgroundTasksSave, span.span());
        session.persist_resume_status().await;
    }
    cleanup_session_scratch(session);
}
impl SessionActor {
    /// Admits or refuses a finished task's wake under the state lock, so an interactive cancel cannot race the admission.
    pub(super) async fn admit_task_completion_wake(
        &self,
        origin: &super::PromptOrigin,
        admission: TaskWakeAdmission,
    ) -> Option<TaskWakeFallback> {
        let TaskWakeAdmission {
            respond_to,
            fallback,
        } = admission;
        let super::PromptOrigin::TaskCompleted { task_id } = origin else {
            return respond_to.send(true).is_ok().then_some(fallback);
        };
        let gate_suppressed = self
            .tool_context
            .task_wake_suppressed
            .as_ref()
            .is_some_and(|gate| gate.get());
        let mut state = self.state.lock().await;
        let state_suppressed = state.notifications_suppressed;
        let admitted = !gate_suppressed && !state_suppressed;
        if !admitted {
            Self::push_task_wake_fallback(&mut state, fallback);
            drop(state);
            distill_telemetry::unified_log::info(
                "shell.task_wake.actor_admission",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "task_id": task_id,
                    "gate": gate_suppressed,
                    "state": state_suppressed,
                    "admitted": false,
                })),
            );
            let _ = respond_to.send(false);
            return None;
        }
        if respond_to.send(true).is_err() {
            Self::push_task_wake_fallback(&mut state, fallback);
            return None;
        }
        drop(state);
        distill_telemetry::unified_log::info(
            "shell.task_wake.actor_admission",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "task_id": task_id,
                "gate": gate_suppressed,
                "state": state_suppressed,
                "admitted": true,
            })),
        );
        Some(fallback)
    }
}
async fn shutdown_workflows(session: &SessionActor, timer: &SharedSessionEndTimer) {
    if !session.startup_hints.is_subagent {
        session.persist_resume_status().await;
    }
    let span = session_end::span(Phase::Workflows);
    {
        let _drain = session_end::timed_child(timer, Phase::WorkflowsDrain, span.span());
        if let Err(run_ids) = session
            .workflow_manager
            .lock()
            .await
            .cancel_all_and_drain(std::time::Duration::from_secs(7))
            .await
        {
            tracing::warn!(
                ?run_ids,
                "workflow shutdown completed with interrupted runs"
            );
        }
    }
    let _persist = session_end::timed_child(timer, Phase::WorkflowsPersist, span.span());
    let (respond_to, ack) = tokio::sync::oneshot::channel();
    if session
        .notifications
        .persistence_tx
        .send(PersistenceMsg::FlushAndAck { respond_to })
        .is_err()
    {
        tracing::warn!("workflow shutdown persistence channel closed before flush");
        return;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(2), ack).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(error))) => {
            tracing::warn!(%error, "workflow shutdown persistence flush failed")
        }
        Ok(Err(_)) => {
            tracing::warn!("workflow shutdown persistence actor dropped flush ack")
        }
        Err(_) => tracing::warn!("workflow shutdown persistence flush timed out"),
    }
}
async fn log_session_ended(session: &SessionActor) {
    let model_id = session.current_model_id().await;
    if let Some(signals) = session.signals_handle().snapshot().await {
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::SessionEnded {
            duration_secs: session.session_start.elapsed().as_secs(),
            turn_count: signals.turn_count as u64,
            tool_call_count: signals.tool_call_count as u64,
            compaction_count: signals.compaction_count as u64,
            model_id,
        });
    }
}
const SESSION_END_EMIT_BUDGET: Duration = Duration::from_secs(1);
async fn emit_session_end_timings(timer: &SharedSessionEndTimer, is_subagent: bool) {
    if is_subagent {
        return;
    }
    let mut event = distill_telemetry::events::SessionEndTimings::default();
    timer.write_event_phases(&mut event);
    event.total_ms = Some(timer.elapsed_ms());
    let _ = tokio::time::timeout(
        SESSION_END_EMIT_BUDGET,
        distill_telemetry::session_ctx::log_event_now(event),
    )
    .await;
}
/// The deferred startup jobs, owned by the run loop so a session ending mid-startup aborts them instead of leaving them detached.
struct StartupTasks {
    _mcp_init_prompt_promote: crate::util::AbortOnDrop,
    _context_snapshot: Option<crate::util::AbortOnDrop>,
    mcp_startup: StartupTaskSet,
}
/// Startup tasks handed over by `&self` actor methods, each holding a strong `Arc` to the actor. Owned by the run
/// loop, so no task keeps the session alive past it.
pub(super) struct StartupTaskSet(std::rc::Rc<std::cell::RefCell<tokio::task::JoinSet<()>>>);
impl StartupTaskSet {
    pub(super) fn install(actor: &SessionActor) -> Self {
        let set = Self(std::rc::Rc::default());
        actor
            .startup_tasks
            .0
            .set(std::rc::Rc::downgrade(&set.0))
            .unwrap_or_else(|_| unreachable!("one run loop installs the startup set once"));
        set
    }
}
/// The actor's side of a [`StartupTaskSet`]: empty until the run loop installs one.
#[derive(Default)]
pub(crate) struct StartupTaskHandle(
    std::cell::OnceCell<std::rc::Weak<std::cell::RefCell<tokio::task::JoinSet<()>>>>,
);
/// Before any run loop the future is handed back to run inline; after the run loop has ended it is dropped.
pub(super) enum StartupHandoff<F> {
    Spawned,
    NoRunLoopYet(F),
    RunLoopEnded,
}
impl StartupTaskHandle {
    pub(super) fn spawn_local<F: Future<Output = ()> + 'static>(
        &self,
        fut: F,
    ) -> StartupHandoff<F> {
        let Some(set) = self.0.get() else {
            return StartupHandoff::NoRunLoopYet(fut);
        };
        let Some(set) = set.upgrade() else {
            return StartupHandoff::RunLoopEnded;
        };
        spawn_after_reaping(&mut set.borrow_mut(), fut);
        StartupHandoff::Spawned
    }
}
impl StartupTaskSet {
    pub(super) fn spawn_local<F: Future<Output = ()> + 'static>(&self, fut: F) {
        spawn_after_reaping(&mut self.0.borrow_mut(), fut);
    }
}
pub(super) fn spawn_after_reaping<F: Future<Output = ()> + 'static>(
    tasks: &mut tokio::task::JoinSet<()>,
    fut: F,
) {
    while let Some(finished) = tasks.try_join_next() {
        if let Err(e) = finished
            && e.is_panic()
        {
            tracing::warn!(error = %e, "MCP startup task panicked");
        }
    }
    tasks.spawn_local(fut);
}
impl StartupTasks {
    fn spawn(
        session: &Arc<SessionActor>,
        completion_tx: mpsc::UnboundedSender<super::turn_task::TurnCompletionMsg>,
    ) -> Self {
        let mcp_startup = StartupTaskSet::install(session);
        let session_for_mcp = session.clone();
        let mcp_init_prompt_promote =
            crate::util::AbortOnDrop(tokio::task::spawn_local(async move {
                session_for_mcp.ensure_mcp_tools_initialized().await;
                SessionActor::maybe_start_running_task(session_for_mcp.clone(), completion_tx)
                    .await;
            }));
        let context_snapshot = if session.startup_hints.is_subagent {
            tracing::info!("session_context_snapshot: skipped (subagent)");
            None
        } else {
            let s = session.clone();
            Some(crate::util::AbortOnDrop(tokio::task::spawn_local(
                instrument_task!("session.context_snapshot", Parent::Inherit, async move {
                    s.wait_for_mcp_initialized().await;
                    s.emit_session_context_snapshot().await;
                }),
            )))
        };
        Self {
            _mcp_init_prompt_promote: mcp_init_prompt_promote,
            _context_snapshot: context_snapshot,
            mcp_startup,
        }
    }
}
pub(super) async fn run_session(
    session: Arc<SessionActor>,
    mut cmd_rx: mpsc::UnboundedReceiver<SessionCommand>,
    mut chat_state_event_rx: mpsc::UnboundedReceiver<distill_chat_state::ChatStateEvent>,
    mut event_rx: mpsc::UnboundedReceiver<SessionEvent>,
    fs_notify_config: Option<ClientFsConfig>,
    codebase_indexes: std::sync::Arc<parking_lot::Mutex<CodebaseIndexManager>>,
    index_root: std::path::PathBuf,
    fs_watch_caps: fs_watch::FsWatchCapabilities,
) {
    let (completion_tx, mut completion_rx) =
        mpsc::unbounded_channel::<super::turn_task::TurnCompletionMsg>();
    let mut turn_end_queue = super::turn_end_hooks::TurnEndQueue::spawn(session.clone());
    tracing::debug!("fs_notify_config: {:?}", fs_notify_config);
    let mut replay_buffer = ReplayBuffer::new(session.buffering_settings.clone());
    let event_tx_for_flush_timer = session.event_tx.clone();
    let buffering_flush_interval = replay_buffer.max_wait_duration_ms();
    if let Some(buffering_flush_interval) = buffering_flush_interval {
        tokio::task::spawn_local(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(std::cmp::max(
                20,
                buffering_flush_interval * 2,
            )));
            loop {
                interval.tick().await;
                let _ =
                    event_tx_for_flush_timer.send(SessionEvent::FlushReplay { respond_to: None });
            }
        });
    }
    let _workflow_watch = crate::config::watcher::ProjectDiscoveryWatcher::start(
        std::path::Path::new(session.session_info.cwd.as_str()),
        &crate::util::distill_home::distill_home(),
    )
    .map(|(mut watcher, mut changes)| {
        let session = session.clone();
        tokio::task::spawn_local(async move {
            while let Some(change) = changes.recv().await {
                watcher.refresh_new_dirs();
                match change {
                    crate::config::watcher::DiscoveryChange::Skills => {
                        session.reload_skills_from_disk().await;
                    }
                    crate::config::watcher::DiscoveryChange::Workflows => {
                        session
                            .send_available_commands_update(AdvertiseTrigger::WorkflowsChanged)
                            .await;
                    }
                }
            }
        })
    });
    let _fs_watch: Option<fs_watch::FsWatchHandle> = if fs_watch_caps.needs_watcher() {
        let deps = fs_watch::FsWatchDeps::from_session(
            &session,
            fs_notify_config.clone(),
            codebase_indexes.clone(),
            index_root.clone(),
        );
        tracing::debug!(?fs_watch_caps, "fs-notify: spawning");
        Some(fs_watch::spawn(fs_watch::FsWatchPlan::build(
            fs_watch_caps,
            deps,
        )))
    } else {
        tracing::debug!("fs-notify: skipped (no consumers)");
        None
    };
    {
        let s = session.clone();
        tokio::task::spawn_local(async move { s.maybe_notify_git_branch().await });
    }
    tokio::task::spawn_local(super::status_line::run_status_emitter(Arc::downgrade(
        &session,
    )));
    let liveness_watchers_enabled = {
        let user_cfg = crate::config::load_effective_config().ok();
        let requirements = crate::agent::config::read_requirements_toml();
        crate::util::config::resolve_mcp_liveness_watchers(
            requirements.as_ref(),
            user_cfg.as_ref(),
            None,
        )
    };
    let _elicitation_coordinator = if !session.startup_hints.is_subagent {
        let elicit_inbox = distill_mcp::elicitation::ElicitationInbox::new();
        {
            let mut mcp_state = session.mcp_state.lock().await;
            mcp_state.set_elicitation_tx(Some(elicit_inbox.clone()));
        }
        Some(
            crate::session::mcp_elicitation::spawn_elicitation_coordinator(
                elicit_inbox,
                session.notifications.gateway.clone(),
                session.session_info.id.clone(),
                session.pending_interactions.clone(),
                std::rc::Rc::clone(&session.attach_non_interactive),
            ),
        )
    } else {
        None
    };
    if !session.startup_hints.is_subagent {
        let (event_tx, event_rx) =
            tokio::sync::mpsc::unbounded_channel::<distill_mcp::servers::McpClientEvent>();
        {
            let mut mcp_state = session.mcp_state.lock().await;
            mcp_state.set_client_event_tx(Some(event_tx));
        }
        let dispatcher_session_id = session.session_info.id.0.to_string();
        let dispatcher_cwd = std::path::PathBuf::from(session.session_info.cwd.as_str());
        let dispatcher_gateway = session.notifications.gateway.clone();
        let dispatcher_mcp_state = Arc::clone(&session.mcp_state);
        let shutdown_state = crate::session::mcp_dispatcher::new_shutdown_state();
        let restart_actions: Option<std::rc::Rc<dyn crate::session::mcp_restart::RestartActions>> =
            if liveness_watchers_enabled {
                let auto_restart_enabled = {
                    let user_cfg = crate::config::load_effective_config().ok();
                    let requirements = crate::agent::config::read_requirements_toml();
                    crate::util::config::resolve_mcp_auto_restart(
                        requirements.as_ref(),
                        user_cfg.as_ref(),
                        None,
                    )
                };
                if auto_restart_enabled {
                    Some(std::rc::Rc::new(SessionRestartActions::new(
                        session.clone(),
                        Arc::clone(&shutdown_state),
                    )))
                } else {
                    None
                }
            } else {
                None
            };
        tokio::task::spawn_local(async move {
            crate::session::mcp_dispatcher::run_dispatcher(
                dispatcher_session_id,
                event_rx,
                dispatcher_gateway,
                dispatcher_mcp_state,
                shutdown_state,
                restart_actions,
                dispatcher_cwd,
            )
            .await;
        });
    }
    let startup_tasks = StartupTasks::spawn(&session, completion_tx.clone());
    session.resume_v2_capture().await;
    let mut model_switch_rx = session.models_manager.subscribe_model_switch();
    let _ = *model_switch_rx.borrow_and_update();
    let idle_flush_sleep = match session.idle_flush_timeout {
        Some(timeout) => tokio::time::sleep(timeout),
        None => tokio::time::sleep(std::time::Duration::MAX),
    };
    tokio::pin!(idle_flush_sleep);
    let dream_check_sleep = match session.dream_check_timeout {
        Some(timeout) => tokio::time::sleep(timeout),
        None => tokio::time::sleep(std::time::Duration::MAX),
    };
    tokio::pin!(dream_check_sleep);
    let mut dream_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut deferred_start = DeferredStart::new();
    if !session.startup_hints.is_subagent && session.memory.uses_legacy_pipeline() {
        dream_task = Some(spawn_dream_check(&session));
    }
    loop {
        tokio::select! {
                biased;
                // Idle flush timer fired: run background flush
                _ = &mut idle_flush_sleep, if session.idle_flush_timeout.is_some()
                    && session.memory.uses_legacy_pipeline()
                    && !session.memory.is_flushing.load(std::sync::atomic::Ordering::Relaxed) => {
                    // Skip if no new messages since last idle flush
                    let current_len = session.chat_state_handle.get_conversation_len().await;
                    let last_len = session.last_idle_flush_conversation_len
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if current_len > last_len {
                        tracing::info!(target: distill_telemetry::memory_log::TARGET,
                            "MEMORY_IDLE_FLUSH: timer fired (conversation {last_len} → {current_len})");
                        session.last_idle_flush_conversation_len
                            .store(current_len, std::sync::atomic::Ordering::Relaxed);
                        tokio::task::spawn_local({
                            let session = session.clone();
                            async move {
                                if !session.run_memory_flush("interval", None).await {
                                    tracing::info!(target: distill_telemetry::memory_log::TARGET,
                                        "MEMORY_IDLE_FLUSH: skipped — another flush already in progress");
                                }
                            }
                        });
                    } else {
                        tracing::debug!(target: distill_telemetry::memory_log::TARGET,
                            "MEMORY_IDLE_FLUSH: skipped, no new messages since last flush (len={current_len})");
                    }
                    // Reset for next idle period
                    if let Some(timeout) = session.idle_flush_timeout {
                        idle_flush_sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
                // Dream check timer: periodically run dream consolidation
                _ = &mut dream_check_sleep, if session.dream_check_timeout.is_some()
                    && session.memory.uses_legacy_pipeline()
                    && !session.startup_hints.is_subagent => {
                    tracing::debug!(target: distill_telemetry::memory_log::TARGET,
                        "MEMORY_DREAM_CHECK: timer fired");
                    // Only start a new dream when the previous one has finished; a shorter check
                    // interval must not abort an in-flight consolidation.
                    if dream_task.as_ref().is_none_or(|h| h.is_finished()) {
                        dream_task = Some(spawn_dream_check(&session));
                    }
                    if let Some(timeout) = session.dream_check_timeout {
                        dream_check_sleep.as_mut().reset(tokio::time::Instant::now() + timeout);
                    }
                }
                // Layer-3 LazinessDetector: zero the per-session nudge counter whenever the user switches models
                // The cap is per-(session, model); switching is a deliberate user action that resets expectations
                // `.changed()` only resolves on switches AFTER subscription, so there is no stored-permit hazard
                changed = model_switch_rx.changed() => {
                    if changed.is_ok() {
                        let new_gen = *model_switch_rx.borrow_and_update();
                        session.handle_model_switch_for_laziness(new_gen).await;
                    }
                }
                // Events from the ChatStateActor that the session loop must react to
                event = chat_state_event_rx.recv() => {
                    match event {
                        Some(distill_chat_state::ChatStateEvent::ConversationReset { new_len }) => {
                            // Reset idle-flush counter so next idle period flushes the new state.
                            session.last_idle_flush_conversation_len
                                .store(new_len, std::sync::atomic::Ordering::Relaxed);
                            // Reset the first-turn injection check after compaction; it re-searches only if no block persisted
                            session.memory.context_injected
                                .store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        Some(distill_chat_state::ChatStateEvent::ImageBudget {
                            body_bytes,
                            trigger_bytes,
                            reclaim_target_bytes,
                            inline_images,
                            needs_image_compaction,
                            evicted,
                            body_bytes_after,
                        }) => {
                            // Log to the unified log so image eviction can be verified locally
                            distill_telemetry::unified_log::info(
                                "shell.image_budget",
                                Some(session.session_info.id.0.as_ref()),
                                Some(serde_json::json!({
                                    "body_bytes": body_bytes,
                                    "body_bytes_after": body_bytes_after,
                                    "trigger_bytes": trigger_bytes,
                                    "reclaim_target_bytes": reclaim_target_bytes,
                                    "inline_images": inline_images,
                                    "images_remaining": inline_images.saturating_sub(evicted),
                                    "needs_image_compaction": needs_image_compaction,
                                    "evicted": evicted,
                                })),
                            );
                        }
                        Some(distill_chat_state::ChatStateEvent::PromptIndexChanged { .. }) |
                        Some(distill_chat_state::ChatStateEvent::TokensUpdated { .. }) => {
                            // Prompt index and token updates are informational; consumers query the actor directly when they need them
                        }
                        None => {
                            // Actor shut down; no more events
                        }
                    }
                }
                maybe_event = event_rx.recv() => {
                    if let Some(event) = maybe_event {
                        match event {
                            SessionEvent::Notification(notification) => {
                                let out = replay_buffer.consume_chunk(notification);
                                match out {
                                    None => {}
                                    Some((first, second)) => {
                                        session.emit_buffered(first).await;
                                        if let Some(second) = second {
                                            session.emit_buffered(second).await;
                                        }
                                    }
                                }
                            }
                            SessionEvent::FlushReplay { respond_to } => {
                                if let Some(notification) = replay_buffer.flush() {
                                    session.emit_buffered(notification).await;
                                }

                                // Always ack (independent of whether anything was buffered).
                                if let Some(tx) = respond_to {
                                    let _ = tx.send(());
                                }
                            }
                        }
                    }
                }
                maybe_cmd = cmd_rx.recv() => {
                    let Some(cmd) = maybe_cmd else {
                        session
                            .settle_all_parent_messages(
                                distill_message_delivery_core::TerminalCause::ActorDrop,
                            )
                            .await;
                        // ── session_end (channel-closed path) ────────
                        // Queued reports first, so an earlier turn's report precedes the session-end `Stop`
                        // Hooks fire BEFORE memory auto-save
                        let end_timer = session_end::SessionEndTimer::new_shared();
                        turn_end_queue.flush().await;
                        fire_session_end_hooks(&session, "channel_closed", &end_timer, &mut deferred_start).await;
                        session.memory.stop_capture_worker().await;
                        session.memory.dream_workers.cancel_and_join().await;
                        // Stop the dream before the end-pipeline reindex so their index writes cannot race.
                        stop_dream(&mut dream_task).await;
                        session
                            .run_session_end_memory_pipeline(
                                "channel closed, session summary saved",
                                &end_timer,
                            )
                            .await;
                        if let Some(notification) = replay_buffer.flush() {
                            session.emit_buffered(notification).await;
                        }
                        log_session_ended(&session).await;
                        shutdown_workflows(&session, &end_timer).await;
                        turn_end_queue.drain().await;
                        finish_session_exit_feedback(&session, &end_timer).await;
                        emit_session_end_timings(&end_timer, session.startup_hints.is_subagent)
                            .await;
                        return;
                    };

                    match cmd {
                        SessionCommand::Initialize { system_prompt } => {
                            session.initialize(system_prompt).await;
                            let s = session.clone();
                            let full_wait = session.requires_full_mcp_wait();
                            let handle = tokio::task::spawn_local(instrument_task!(
                                "session.prefix_task",
                                Parent::Inherit,
                                async move { s.build_prefix_after_mcp_wait(full_wait).await }
                            ));
                            session.deferred_prefix.arm(handle, full_wait);
                        }
                        SessionCommand::ReplaceSystemPrompt { system_prompt } => {
                            session.handle_replace_system_prompt(system_prompt).await;
                        }
                        SessionCommand::EmitStatusSnapshot => {
                            session.emit_status_snapshot_detached();
                        }
                        SessionCommand::RestorePlanApproval => {
                            // Spawn the restored plan-approval round-trip so the command loop is not blocked on the open-ended user decision.
                            // Detaching the handle is safe: the task lives on this session's `LocalSet` and is dropped when the session ends.
                            // Dropping it cancels the `request_plan_approval` future and clears `awaiting` via the guard `resume_plan_approval` also self-guards against a concurrent or duplicate restore via `pending_interactions`.
                            let s = session.clone();
                            let completion_tx = completion_tx.clone();
                            tokio::task::spawn_local(async move {
                                s.resume_plan_approval(completion_tx).await;
                            });
                        }
                        SessionCommand::TitleRenamed { manual } => {
                            session.on_title_renamed(manual);
                        }
                        SessionCommand::GetToolOverrides { respond_to } => {
                            let _ = respond_to.send(session.effective_tool_overrides());
                        }
                        SessionCommand::SetToolOverrides { overrides } => {
                            session.set_tool_overrides(overrides);
                        }
                        SessionCommand::Prompt { prompt_id, prompt_blocks, prompt_mode, artifact_upload_ctx, client_identifier, screen_mode, verbatim, traceparent, json_schema, send_now, admission, tool_overrides_update, respond_to, prompt_admitted, persist_ack, parsed_prompt_tx } => {
                            let origin = super::PromptOrigin::from_prompt_id(&prompt_id);
                            let (actor_admitted, task_wake_fallback) = match admission {
                                Some(admission) => {
                                    let fallback = session
                                        .admit_task_completion_wake(&origin, admission)
                                        .await;
                                    (fallback.is_some(), fallback)
                                }
                                None => (true, None),
                            };
                            if !actor_admitted {
                                SessionActor::respond_removed_prompt(respond_to);
                                continue;
                            }
                            session.ensure_prefix_ready().await;
                            // Clear suppression: the user is re-engaging
                            // Synthetic auto-wake prompts skip this; the user has not actually re-engaged, so post-cancel suppression must hold
                            if !origin.is_synthetic() {
                                if let Some(gate) = &session.tool_context.task_wake_suppressed {
                                    gate.set(false);
                                }
                                let mut state = session.state.lock().await;
                                state.notifications_suppressed = false;
                                if state.take_hook_block_hold() {
                                    distill_telemetry::unified_log::info(
                                        "shell.prompt.hook_block_hold_released",
                                        Some(session.session_info.id.0.as_ref()),
                                        Some(serde_json::json!({ "reason": "user_intake" })),
                                    );
                                }
                                distill_telemetry::unified_log::info(
                                    "shell.task_wake.gate_cleared",
                                    Some(session.session_info.id.0.as_ref()),
                                    Some(serde_json::json!({ "reason": "user_intake" })),
                                );
                                // Layer-3 LazinessDetector wake: bump the monotonic counter.
                                // Any classifier poll-loop already running snapshots a stale value and aborts.
                                // Synthetic prompts (NotificationDrain, GoalSummary, auto-wake) are not real user input and must NOT bump it.
                                session
                                    .user_input_generation
                                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            }
                            if origin.is_synthetic() {
                                let state = session.state.lock().await;
                                let has_running = state.running_task.is_some();
                                let queue_depth = state.pending_inputs.len();
                                drop(state);
                                tracing::info!(
                                    prompt_id = %prompt_id,
                                    has_running_task = has_running,
                                    queue_depth = queue_depth,
                                    "auto-wake: session actor received synthetic prompt"
                                );
                            }
                            let (trace_gcs_config, artifact_tracker) = match artifact_upload_ctx {
                                Some(tu) => (Some(tu.gcs_config), Some(tu.artifact_tracker)),
                                None => (None, None),
                            };
                            let cancel_for_send_now = session
                                .queue_input(QueueInputRequest {
                                    prompt_blocks,
                                    prompt_id,
                                    input_origin: InputOrigin::new(origin),
                                    prompt_mode,
                                    trace_gcs_config,
                                    artifact_tracker,
                                    client_identifier,
                                    screen_mode,
                                    verbatim,
                                    json_schema,
                                    send_now,
                                    task_wake_fallback,
                                    tool_overrides_update,
                                    respond_to,
                                    persist_ack,
                                    parsed_prompt_tx,
                                    initial_child_prompt_ready: prompt_admitted,
                                    traceparent,
                                })
                                .await;
                            // An unsettled cancel means another finalization owns the turn; its release will re-kick the queue.
                            let settled = if cancel_for_send_now {
                                session
                                    .cancel_turn_for_send_now(&mut replay_buffer)
                                    .await
                                    .settled
                            } else {
                                true
                            };
                            if settled {
                                SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                            }
                        }
                        SessionCommand::ParentAgentMessage {
                            delivery,
                            receipt_sink,
                            parent_telemetry_ctx,
                            respond_to,
                        } => {
                            session
                                .admit_parent_agent_message(
                                    delivery,
                                    receipt_sink,
                                    parent_telemetry_ctx,
                                    respond_to,
                                    completion_tx.clone(),
                                )
                                .await;
                        }
                        SessionCommand::SessionMode { session_mode, responds_to } => {
                            session.handle_session_mode(session_mode).await;
                            let _ = responds_to.send(());
                        }
                        SessionCommand::SetSessionModel { switch, responds_to } => {
                            let updated_model_id = session.handle_set_session_model(switch).await;
                            let _ = responds_to.send(updated_model_id);
                        }
                        SessionCommand::SetReasoningEffort { effort, responds_to } => {
                            let updated_model_id = session.handle_set_reasoning_effort(effort).await;
                            let _ = responds_to.send(updated_model_id);
                        }
                        SessionCommand::RebuildAgentForDefinition { definition, system_prompt_label, responds_to } => {
                            let outcome = session.handle_rebuild_agent_for_definition(definition, system_prompt_label).await;
                            let _ = responds_to.send(outcome);
                        }
                        SessionCommand::OverrideModelName { model_name, extra_headers, context_window } => {
                            // Update the actor's SamplingConfig model, headers, and context window
                            if let Some(mut cfg) = session.chat_state_handle.get_sampling_config().await {
                                tracing::info!(
                                    target: SESSION_LOG,
                                    session_id = %session.session_info.id,
                                    old_model = %cfg.model,
                                    new_model = %model_name,
                                    extra_header_count = extra_headers.len(),
                                    old_context_window = cfg.context_window.get(),
                                    new_context_window = ?context_window.map(|cw| cw.get()),
                                    "OVERRIDE_MODEL: changing model name in sampling config"
                                );
                                // Update signals so primaryModelId and modelsUsed reflect the override, not the agent default (e.g. "grok-4.5").
                                // set_primary_model also adds to models_used.
                                session.signals_handle().set_primary_model(&model_name);
                                cfg.model = model_name.clone();
                                cfg.extra_headers.extend(extra_headers);
                                if let Some(cw) = context_window
                                    && session.compaction.context_window_override.is_none()
                                {
                                    cfg.context_window = cw;
                                }
                                session.chat_state_handle.update_sampling_config(cfg);

                                let existing = session.chat_state_handle.get_credentials().await;
                                if let Some(r) = crate::agent::config::try_resolve_model_credentials(model_name.as_str(), existing.api_key.as_deref()) {
                                    session.chat_state_handle.update_credentials(distill_chat_state::Credentials {
                                        api_key: r.api_key,
                                        auth_type: r.auth_type,
                                        alpha_test_key: existing.alpha_test_key,
                                        client_version: existing.client_version,
                                    });
                                }
                                // Credentials changed under a possibly-unchanged model id.
                                session.invalidate_model_auth_memo();
                            }
                        }
                        SessionCommand::GetCurrentModel { responds_to } => {
                            let current = session.chat_state_handle.get_sampling_config().await
                                .map(|c| crate::session::CurrentModel {
                                    id: c.model,
                                    reasoning_effort: c.reasoning_effort,
                                    canonical_id: Some(
                                        session.canonical_model_id.borrow().0.to_string(),
                                    ),
                                    model_routing_locked: session.model_routing_locked.get()
                                        || session.startup_hints.explicit_model_override,
                                })
                                .unwrap_or_default();
                            let _ = responds_to.send(current);
                        }
                        SessionCommand::GetCurrentPromptMode { responds_to } => {
                            let mode = *session.current_prompt_mode.lock();
                            let _ = responds_to.send(mode);
                        }
                        SessionCommand::GetModelMetadata { responds_to } => {
                            let id = session.chat_state_handle.get_last_model_metadata().await;
                            let _ = responds_to.send(id);
                        }
                        SessionCommand::GetSessionInfo { responds_to } => {
                            let info = session.build_session_info().await;
                            let _ = responds_to.send(info);
                        }
                        SessionCommand::BackgroundForegroundCommand { tool_call_id, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .background_foreground_command(&tool_call_id)
                                .await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::KillBackgroundTask { task_id, source, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .kill_background_task(&task_id, source)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::DeleteScheduledTask { task_id, respond_to } => {
                            let result = session.agent.borrow().tool_bridge()
                                .delete_scheduled_task(&task_id)
                                .await
                                .map_err(|e| e.to_string());
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::ListTasks { respond_to } => {
                            let result = session.tool_bridge_handle().list_tasks().await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::EmitBackgroundTasksSnapshot {
                            respond_to,
                            pending,
                        } => {
                            session.emit_background_tasks_snapshot(pending).await;
                            if let Some(respond_to) = respond_to {
                                let _ = respond_to.send(());
                            }
                        }
                        SessionCommand::GetHooksList { respond_to } => {
                            let hooks = crate::extensions::hooks::current_hook_infos(
                                session.hook_registry.borrow().as_deref(),
                            );

                            // Report the folder-trust verdict so the flag matches the gated registry built above
                            let project_trusted =
                                crate::agent::folder_trust::project_scope_allowed(
                                    std::path::Path::new(&session.session_info.cwd),
                                );

                            let _ = respond_to.send(distill_hooks_plugins_types::HooksListResponse {
                                hooks,
                                project_trusted,
                                load_errors: session.hook_load_errors.borrow().clone(),
                            });
                        }
                        SessionCommand::HooksAction { action, respond_to } => {
                            let outcome = session.handle_hooks_action(action).await;
                            let _ = respond_to.send(outcome);
                        }
                        SessionCommand::NotifyPluginUpdates { updates } => {
                            session
                                .send_xai_notification(
                                    XaiSessionUpdate::PluginUpdatesInstalled { updates },
                                )
                                .await;
                        }
                        SessionCommand::PluginsAction { action, respond_to } => {
                            let outcome = session.handle_plugins_action(action).await;
                            let _ = respond_to.send(outcome);
                        }
                        SessionCommand::PluginsList { respond_to } => {
                            let _ = respond_to.send(session.plugin_registry.borrow().clone());
                        }
                        SessionCommand::DispatchNotificationHook {
                            notification_type,
                            message,
                            title,
                            level,
                        } => {
                            session
                                .dispatch_notification_hook(
                                    &notification_type,
                                    message,
                                    title,
                                    level,
                                )
                                .await;
                        }
                        SessionCommand::DropMonitorNotifications { task_id } => {
                            // Discard this task's pending and mid-turn-buffered monitor events
                            // A TaskCompleted auto-wake is then the sole signal the model sees for a natural exit
                            {
                                let mut state = session.state.lock().await;
                                state.pending_notifications.retain(|n| {
                                    !matches!(
                                        &n.source,
                                        NotificationSource::MonitorEvent { task_id: tid }
                                            if tid == &task_id
                                    )
                                });
                            }
                            if let Some(buffer) = &session.tool_context.monitor_event_buffer {
                                let dropped = buffer.drain_matching(|e| e.task_id == task_id);
                                if !dropped.is_empty() {
                                    tracing::debug!(
                                        task_id = %task_id,
                                        dropped = dropped.len(),
                                        "dropped buffered monitor events after TaskCompleted auto-wake"
                                    );
                                }
                            }
                        }
                        SessionCommand::InjectNotification { prompt_id, prompt_blocks, priority, source } => {
                            let is_turn_active = session
                                .session_turn_active
                                .load(std::sync::atomic::Ordering::SeqCst);

                            if is_turn_active && priority == NotificationPriority::Next {
                                // Mid-turn and `Next` priority: push to the shared buffer for the turn loop's `inject_pending_monitor_events`
                                if let Some(buffer) = &session.tool_context.monitor_event_buffer {
                                    let non_text_count = prompt_blocks.iter().filter(|b| !matches!(b, acp::ContentBlock::Text(_))).count();
                                    if non_text_count > 0 {
                                        tracing::debug!(
                                            non_text_count,
                                            "Non-text content blocks dropped in mid-turn monitor event routing"
                                        );
                                    }

                                    let event_text = prompt_blocks
                                        .iter()
                                        .filter_map(|b| {
                                            if let acp::ContentBlock::Text(t) = b {
                                                Some(t.text.clone())
                                            } else {
                                                None
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                        .join("\n");

                                    let task_id = source.task_id().to_owned();

                                    // Cap to prevent unbounded growth during long tool calls.
                                    const MAX_BUFFER_EVENTS: usize = 50;
                                    buffer.push_capped(
                                        distill_tools::implementations::distill::monitor::types::MonitorEventNotification {
                                            task_id: task_id.clone(),
                                            event_text,
                                            // Tag with this session's id so the shared (leader-mode) buffer drains show it only here
                                            // The bridge guard guarantees this event is owned by this session
                                            owner_session_id: Some(
                                                session.session_info.id.0.to_string(),
                                            ),
                                        },
                                        MAX_BUFFER_EVENTS,
                                    );

                                    tracing::debug!(
                                        task_id = %task_id,
                                        "Routed monitor event to mid-turn buffer"
                                    );
                                }
                            } else {
                                {
                                    let mut state = session.state.lock().await;
                                    SessionActor::push_pending_notification(
                                        &mut state,
                                        PendingNotification {
                                            prompt_id,
                                            prompt_blocks,
                                            priority,
                                            source,
                                        },
                                    );
                                }
                                SessionActor::maybe_drain_notifications(session.clone(), completion_tx.clone()).await;
                            }
                        }
                        SessionCommand::RecordGoalTurnTaskIds { task_ids } => {
                            session.record_reparented_goal_turn_task_ids(task_ids);
                        }
                        SessionCommand::RemoveQueuedPrompt { id, expected_version, owner } => {
                            // Releases below are gated on the handler reporting a real mutation: a stale/foreign no-op is not re-engagement
                            if session.handle_remove_queued_prompt(&id, expected_version, owner.as_deref()).await {
                                session.release_hook_block_hold("queue_remove").await;
                            }
                            // Re-kick: delete-while-editing keeps the hold until remove clears it.
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::ReorderQueue { ordered_ids } => {
                            if session.handle_reorder_queue(&ordered_ids).await {
                                session.release_hook_block_hold("queue_reorder").await;
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::ClearQueue { owner } => {
                            if session.handle_clear_queue(owner.as_deref()).await {
                                session.release_hook_block_hold("queue_clear").await;
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::EditQueuedPrompt { id, new_text, editor } => {
                            // Re-kick so a previously held front can start.
                            if session.handle_edit_queued_prompt(&id, new_text, editor.as_deref()).await {
                                session.release_hook_block_hold("queue_edit").await;
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::HoldEdit { id } => {
                            // insert (not entry/or_insert): re-entering edit after a dropped release must refresh the TTL stamp
                            session.handle_hold_edit(id).await;
                        }
                        SessionCommand::ReleaseEdit { id } => {
                            // No hook-hold release: ReleaseEdit also fires on a cancelled editor, and a peek is not re-engagement
                            // The save path (EditQueuedPrompt) releases.
                            session.handle_release_edit(&id).await;
                            // Unblocks an editable front that was parked under edit hold.
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::InterjectQueuedPrompt { id, expected_version, owner, new_text } => {
                            // Send-now: the handler promoted the row; cancel the running turn and start it.
                            let send_now = session.handle_interject_queued_prompt(&id, expected_version, owner.as_deref(), new_text.as_deref()).await;
                            // An unsettled cancel means another finalization owns the turn; its release will re-kick the queue.
                            let settled = if send_now.cancel_running_turn {
                                session
                                    .cancel_turn_for_send_now(&mut replay_buffer)
                                    .await
                                    .settled
                            } else {
                                true
                            };
                            if settled && send_now.mutated {
                                session.release_hook_block_hold("queue_interject").await;
                            }
                            if settled {
                                SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                            }
                        }
                        SessionCommand::Cancel(options) => {
                            // Flush the actor-owned replay buffer before tearing down the running turn.
                            // Chunks still pending at cancel (notably.
                            // Ctrl+C It must reach disk before the trace upload snapshots the session directory.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Clear, don't flush: converting interjections to prompt turns would restart the model after a stop
                            session.pending_interjections.clear();
                            // Do not abort turn summary here.
                            // Summaries spawn only after a successful turn, so a summary call still running describes that prior success.
                            // Cancel targets the current turn; the prior summary line shows until replaced, so it should still finish.
                            let is_rewind = matches!(
                                &options.history,
                                crate::session::CancelHistoryDisposition::RewindIfNoOutput { .. }
                            );
                            let cancel = session.cancel_running_task(options).await;
                            // A rewind settles on its own rail (`settled` stays false), so it still gets the post-cancel kick.
                            // An unsettled non-rewind cancel lost the finalization claim; the owner's release re-kicks the queue.
                            if cancel.settled || is_rewind {
                                // Auto-pause the active goal so timers stop and the pager shows "paused" instead of "active"
                                // Shared with the doom-loop and back-off paths via `auto_pause_goal_if_active`
                                session
                                    .auto_pause_goal_if_active(
                                        crate::session::goal_tracker::GoalPauseReason::User,
                                    )
                                    .await;
                                // Kick any already-queued prompt; the aborted task can't send the completion message it would otherwise wait for
                                SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                                // An armed barrier must outlive the cancel; only a clear one lets queued notifications drain
                                if cancel.barrier == super::cancel::WakeBarrier::Clear {
                                    SessionActor::maybe_drain_notifications(
                                        session.clone(),
                                        completion_tx.clone(),
                                    )
                                    .await;
                                }
                                if cancel.turn_stopped {
                                    session.emit_session_idle_if_idle().await;
                                }
                            }
                        }
                        SessionCommand::CompactSession { user_context, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let compact_session = s.run_compact(user_context).await;
                                let _ = respond_to.send(compact_session);
                            });
                        }
                        SessionCommand::ReloadPlugins { registry } => {
                            if !session.startup_hints.is_subagent {
                                // Fan-outs rebuild without per-session `_meta.pluginDirs`; re-merge this session's own dirs before adopting
                                let registry = session.preserve_session_plugin_dirs(registry);
                                session.apply_plugin_registry_snapshot(registry).await;
                            }
                        }
                        SessionCommand::ReloadHooks => {
                            // Re-discover the session's project hooks on the now-flipped folder-trust verdict.
                            // Run INLINE on the serialized command loop (not a spawned task) like `ReloadPlugins` `reload_hooks_impl` mutates `hook_registry`.
                            // The file-header `await_holding_refcell_ref` allow assumes no concurrent mutation; spawning would race turn tasks.
                            if !session.startup_hints.is_subagent {
                                let _ = session.reload_hooks_impl().await;
                            }
                        }
                        SessionCommand::RefreshSkillBaseline => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let cwd = s.tool_context.cwd.as_path().to_string_lossy();
                                let skills_config = crate::util::config::load_config().await.skills;
                                let pr = s.plugin_registry.borrow().clone();
                                let new_skills = distill_agent::prompt::skills::list_skills_with_plugins(
                                    Some(&cwd),
                                    &skills_config,
                                    pr.as_deref(),
                                    s.rebuild_spec.compat,
                                    crate::agent::folder_trust::project_scope_allowed(
                                        s.tool_context.cwd.as_path(),
                                    ),
                                )
                                .await;
                                tracing::info!(skills = new_skills.len(), "refreshed skill baseline");
                                let bridge = s.agent.borrow().tool_bridge().clone();
                                bridge.update_skill_baseline(new_skills).await;
                                if let Some(effects) = bridge.apply_pending_skill_update().await {
                                    s.apply_skill_update_effects(effects).await;
                                }
                            });
                        }
                        SessionCommand::FlushMemory { respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let _ = respond_to.send(s.memory_flush_command().await);
                            });
                        }
                        SessionCommand::MemoryDream { respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let _ = respond_to.send(s.memory_dream_command().await);
                            });
                        }
                        SessionCommand::MemoryList { respond_to } => {
                            let _ = respond_to.send(session.memory_listing());
                        }
                        SessionCommand::MemoryToggle { enabled, respond_to } => {
                            // Awaited inline so concurrent toggles cannot interleave mid-transition.
                            let response = session.memory_toggle_and_list(enabled).await;
                            let _ = respond_to.send(response);
                        }
                        SessionCommand::MemoryForget {
                            path,
                            expected_content_hash,
                            respond_to,
                        } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let response =
                                    s.memory_forget(&path, &expected_content_hash).await;
                                let _ = respond_to.send(response);
                            });
                        }
                        SessionCommand::SetYoloMode { enabled } => {
                            let was = session.permissions.is_yolo_mode();
                            tracing::info!("Session received SetYoloMode: {}", enabled);
                            session.permissions.set_yolo_mode(enabled);
                            // Report the ACTUAL state, not the request
                            // Under the always-approve pin the manager clamps a requested ON to OFF
                            // Emitting `enabled` would then announce a turn-on that never happened
                            let actual = session.permissions.is_yolo_mode();
                            if let Some(enabled) = yolo_toggle_report(was, actual) {
                                session.emit_event(crate::session::events::Event::YoloToggled { enabled });
                            }
                        }
                        SessionCommand::SetAutoMode { enabled } => {
                            // Feature gate: a runtime request to enable auto is honored only when the feature is enabled
                            // A client notification can't bypass the gate
                            let enabled = enabled
                                && crate::util::config::auto_permission_mode_enabled_from_disk();
                            tracing::info!("Session received SetAutoMode: {}", enabled);
                            session.permissions.set_auto_mode(enabled);
                            if enabled {
                                session.wire_permission_auto_llm_classifier().await;
                            } else {
                                session.permissions.set_llm_side_query_wired(false);
                            }
                        }
                        SessionCommand::ResetPermissionState => {
                            session.permissions.reset_state();
                            tracing::info!(
                                session_id = %session.session_info.id,
                                "Permission state reset via notification"
                            );
                        }
                        SessionCommand::Rewind { request, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_rewind(request).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::RepairHistory { dry_run, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_repair_history(dry_run).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::GetRewindPoints { respond_to } => {
                            let response = session.get_rewind_points().await;
                            let _ = respond_to.send(response);
                        }
                        SessionCommand::GetRewindFileCounts { respond_to } => {
                            let _ = respond_to.send(session.rewind_file_counts().await);
                        }
                        SessionCommand::ReconcileRewindTracker { target_prompt_index } => {
                            session.merge_rewind_tracker_from(target_prompt_index).await;
                        }
                        SessionCommand::AcquireImageStripRewrite { respond_to } => {
                            let guard = session.image_strip_rewrite_barrier.lock_rewind().await;
                            let _ = respond_to.send(guard);
                        }
                        SessionCommand::InvalidateImageStripsForRewind { respond_to } => {
                            session.cancel_active_sampling_requests();
                            session.cancel_pending_image_strips_for_rewind();
                            let _ = respond_to.send(());
                        }
                        SessionCommand::XaiSessionNotification { notification } => {
                            session.handle_xai_session_notification(notification).await;
                        }
                        SessionCommand::RecordSubagentUsage {
                            by_model,
                            parent_prompt_id,
                            incomplete,
                            respond_to,
                        } => {
                            session
                                .handle_record_subagent_usage_command(
                                    &by_model,
                                    parent_prompt_id.as_deref(),
                                    incomplete,
                                    respond_to,
                                )
                                .await;
                        }
                        SessionCommand::MarkSubagentUsageNotApplied {
                            parent_prompt_id,
                            respond_to,
                        } => {
                            session
                                .handle_mark_subagent_usage_not_applied_command(
                                    parent_prompt_id.as_deref(),
                                    respond_to,
                                )
                                .await;
                        }
                        SessionCommand::ErrorPathUsageFallback {
                            prompt_id,
                            respond_to,
                        } => {
                            let pid = prompt_id.or_else(|| {
                                session
                                    .current_prompt_id
                                    .lock()
                                    .ok()
                                    .and_then(|g| g.clone())
                            });
                            let usage = match pid.as_deref() {
                                Some(id) => session.error_path_usage_fallback(id).await,
                                None => {
                                    match session.chat_state_handle.try_get_prompt_usage().await {
                                        Ok(ledger) => {
                                            crate::extensions::notification::PromptUsage::for_error_path(
                                                ledger.as_ref(),
                                                false,
                                            )
                                        }
                                        Err(()) => {
                                            crate::extensions::notification::PromptUsage::for_error_path(
                                                None, true,
                                            )
                                        }
                                    }
                                }
                            };
                            let _ = respond_to.send(usage);
                        }
                        SessionCommand::SetNextTraceTurn {
                            next_trace_turn,
                            request_id,
                        } => {
                            let _ =
                                session.notifications.persistence_tx.send(PersistenceMsg::NextTraceTurn {
                                    next_trace_turn,
                                    request_id,
                                });
                        }
                        SessionCommand::CopyFile { respond_to } => {
                            // Flush the actor-owned replay buffer before tearing down the running turn.
                            // Chunks still pending at cancel (notably AgentThoughtChunk reasoning text) then get committed to updates.jsonl.
                            // A long reasoning stream's tail can sit in the buffer when the user hits Ctrl+C.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::CopyFile { one_shot: respond_to });
                        }
                        SessionCommand::IsBusy { respond_to } => {
                            let _ = respond_to.send(session.is_busy().await);
                        }
                        SessionCommand::FlushComplete { respond_to } => {
                            // Flush the actor-owned replay buffer inline
                            // This branch already runs inside `run_session()`
                            // Sending a replay flush event to `event_tx` would deadlock waiting for the same loop to process its own mailbox
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Chain through the persistence actor: signal only after flush_pending() completes on disk
                            // That makes FlushComplete a true sync barrier
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::FlushAndAck { respond_to });
                        }
                        #[cfg(test)]
                        SessionCommand::InjectTurnCompletion {
                            prompt_id,
                            epoch,
                            result,
                            elapsed_ms,
                            processed,
                        } => {
                            let task_identity = session
                                .state
                                .lock()
                                .await
                                .running_task
                                .as_ref()
                                .map(|task| task.identity.clone())
                                .unwrap_or_else(|| super::turn_task::TaskIdentity::new(()));
                            let _ = completion_tx.send(super::turn_task::TurnCompletionMsg {
                                prompt_id,
                                epoch,
                                task_identity,
                                result: *result,
                                elapsed_ms,
                                processed: Some(processed),
                            });
                        }
                        SessionCommand::UpdateAttachPolicy { startup_hints } => {
                            session.apply_attach_policy(&startup_hints);
                        }
                        SessionCommand::UpdateMcpServers { mcp_servers, respond_to } => {
                            if session.startup_hints.is_subagent {
                                tracing::debug!(
                                    session_id = %session.session_info.id.0,
                                    "Skipping UpdateMcpServers for subagent session",
                                );
                                let _ = respond_to.send(Ok(()));
                                continue;
                            }
                            tracing::info!(
                                "Updating MCP servers for session '{}' ({} servers)",
                                session.session_info.id.0,
                                mcp_servers.len()
                            );

                            // Re-seed the session-scoped MCP output cap (repo `[mcp] max_output_bytes`) BEFORE the unchanged-diff early-exit below: this command also fires for `<cwd>/.grok/config.toml` edits, and a cap-only edit changes no server.
                            session.reseed_mcp_output_cap().await;

                            // Capture the dispatcher's event sender alongside the diff
                            // `McpClientEvent::ConfigDiff` can then fan out right after the in-memory swap
                            // The emit happens without holding the `mcp_state` lock
                            let (change, dispatch_event_tx) = {
                                let mut mcp_state = session.mcp_state.lock().await;
                                let change = session.update_mcp_configs(&mut mcp_state, mcp_servers);
                                let tx = mcp_state.client_event_tx();
                                (change, tx)
                            };

                            let Some(change) = change else {
                                tracing::debug!(
                                    "MCP configs unchanged for session '{}', skipping re-initialization",
                                    session.session_info.id.0
                                );
                                let _ = respond_to.send(Ok(()));
                                continue;
                            };

                            session.apply_mcp_config_diff(&change.diff, dispatch_event_tx);
                            let session_for_mcp = session.clone();
                            startup_tasks.mcp_startup.spawn_local(async move {
                                session_for_mcp
                                    .start_mcp_servers_after_config_change(change)
                                    .await;
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::ToggleMcpServer { server_name, enabled, server_config, respond_to } => {
                            session.events.emit(distill_session_events::Event::McpServerToggled {
                                server_name: server_name.clone(),
                                enabled,
                            });
                            let mut mcp_state = session.mcp_state.lock().await;
                            let mut configs = mcp_state.configs.clone();

                            if enabled {
                                if let Some(config) = server_config {
                                    // Replace any prior entry so enabling after setup can swap an unresolved placeholder for a resolved URL
                                    configs.retain(|c| {
                                        crate::session::mcp_servers::mcp_server_name(c)
                                            != server_name
                                    });
                                    configs.push(config);
                                } else {
                                    let already_present = configs.iter().any(|c| {
                                        crate::session::mcp_servers::mcp_server_name(c)
                                            == server_name
                                    });
                                    if already_present {
                                        drop(mcp_state);
                                        let _ = respond_to.send(Ok(()));
                                        continue;
                                    }
                                    drop(mcp_state);
                                    let _ = respond_to.send(Err(acp::Error::invalid_params()
                                        .data(format!("server '{}' not found in config", server_name))));
                                    continue;
                                }
                            } else {
                                configs.retain(|c| crate::session::mcp_servers::mcp_server_name(c) != server_name);
                            }

                            let change = session.update_mcp_configs(&mut mcp_state, configs);
                            // Snapshot the dispatcher sender BEFORE dropping the lock so the emit below survives any later mutation
                            let dispatch_event_tx = mcp_state.client_event_tx();
                            drop(mcp_state);

                            let Some(change) = change else {
                                let _ = respond_to.send(Ok(()));
                                continue;
                            };

                            session.apply_mcp_config_diff(&change.diff, dispatch_event_tx);
                            let session_for_mcp = session.clone();
                            let sname = server_name.clone();
                            let session_cwd = session.session_info.cwd.clone();
                            // The preference outlives the session, so its write does not ride on startup.
                            tokio::task::spawn_local(async move {
                                if let Err(e) = crate::util::config::save_mcp_server_enabled_in(
                                    &sname,
                                    enabled,
                                    std::path::Path::new(&session_cwd),
                                )
                                .await
                                {
                                    tracing::warn!(
                                        server = sname.as_str(),
                                        error = %e,
                                        "Failed to persist server enabled state to config"
                                    );
                                }
                            });
                            startup_tasks.mcp_startup.spawn_local(async move {
                                session_for_mcp
                                    .start_mcp_servers_after_config_change(change)
                                    .await;
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::ToggleMcpTool { server_name, tool_name, enabled, is_managed_gateway, respond_to } => {
                            if is_managed_gateway {
                                let mut disabled_tools = crate::util::config::get_all_mcp_disabled_tools(std::path::Path::new(&session.session_info.cwd));
                                if tool_name.is_empty() {
                                    let set = disabled_tools
                                        .entry(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string())
                                        .or_default();
                                    if enabled {
                                        set.remove(&server_name);
                                    } else {
                                        set.insert(server_name.clone());
                                    }
                                    if set.is_empty() {
                                        disabled_tools.remove(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY);
                                    }
                                } else if enabled {
                                    if let Some(set) = disabled_tools.get_mut(&server_name) {
                                        set.remove(&tool_name);
                                        if set.is_empty() {
                                            disabled_tools.remove(&server_name);
                                        }
                                    }
                                } else {
                                    disabled_tools
                                        .entry(server_name.clone())
                                        .or_default()
                                        .insert(tool_name.clone());
                                }

                                session
                                    .refresh_mcp_snapshot_and_schedule_reminder_with_disabled(
                                        &disabled_tools,
                                    )
                                    .await;
                                session.refresh_goal_harness_enabled().await;

                                let disabled_vec: Vec<String> = if tool_name.is_empty() {
                                    disabled_tools
                                        .get(crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY)
                                        .map(|s| s.iter().cloned().collect())
                                        .unwrap_or_default()
                                } else {
                                    disabled_tools
                                        .get(&server_name)
                                        .map(|s| s.iter().cloned().collect())
                                        .unwrap_or_default()
                                };
                                let notifications = session.notifications.gateway.clone();
                                let session_id = session.session_info.id.0.clone();
                                let server_for_persist = if tool_name.is_empty() {
                                    crate::util::config::MANAGED_GATEWAY_DISABLED_CONNECTORS_KEY.to_string()
                                } else {
                                    server_name.clone()
                                };
                                tokio::task::spawn_local(async move {
                                    if let Err(e) = crate::util::config::save_mcp_disabled_tools(
                                        &server_for_persist,
                                        &disabled_vec,
                                    ).await {
                                        tracing::warn!(
                                            server = server_for_persist.as_str(),
                                            error = %e,
                                            "Failed to persist disabled_tools to config"
                                        );
                                    }
                                    let payload = crate::extensions::mcp::McpToolsChanged {
                                        session_id: session_id.to_string(),
                                        server_name: String::new(),
                                        tools: Vec::new(),
                                    };
                                    if let Ok(params) = serde_json::value::to_raw_value(&payload) {
                                        notifications.forward_fire_and_forget(acp::ExtNotification::new("x.ai/mcp/tools_changed", params.into()));
                                    }
                                    let _ = respond_to.send(Ok(()));
                                });
                                continue;
                            }
                            let qualified = format!(
                                "{}{}{}",
                                server_name,
                                crate::session::mcp_servers::MCP_TOOL_NAME_DELIMITER,
                                tool_name,
                            );
                            let mut mcp_state = session.mcp_state.lock().await;

                            if enabled {
                                // Re-enable: remove from disabled set, re-register from stashed registration.
                                if let Some(set) = mcp_state.disabled_tools.get_mut(&server_name) {
                                    set.remove(&tool_name);
                                    if set.is_empty() {
                                        mcp_state.disabled_tools.remove(&server_name);
                                    }
                                }
                                if let Some(reg) = mcp_state.disabled_tool_registrations.remove(&qualified)
                                    && reg.model_visible
                                {
                                    let bridge = session.agent.borrow().tool_bridge().clone();
                                    if let Err(e) = bridge
                                        .register_mcp_tools(reg.name, reg.tool, Some(reg.input_schema))
                                    {
                                        tracing::warn!(
                                            tool = qualified.as_str(),
                                            error = %e,
                                            "Failed to re-register toggled MCP tool"
                                        );
                                    }
                                }
                            } else {
                                // Disable: stash a registration so the tool can be re-enabled without a full re-init, then unregister
                                let bridge = session.agent.borrow().tool_bridge().clone();
                                let tool_def = bridge
                                    .tool_definitions()
                                    .await
                                    .into_iter()
                                    .find(|d| d.function.name == qualified);
                                if let Some(def) = tool_def {
                                    let meta = mcp_state.mcp_tool_meta.get(&qualified).cloned();
                                    let schema = def.function.parameters.clone();
                                    let mcp_tool = crate::session::mcp_servers::McpTool::new(
                                        tool_name.clone(),
                                        def.function.description.clone().unwrap_or_default(),
                                        server_name.clone(),
                                        session.mcp_state.clone(),
                                        schema,
                                        meta,
                                    );
                                    if let Ok(reg) = mcp_tool.into_registration() {
                                        mcp_state
                                            .disabled_tool_registrations
                                            .insert(qualified.clone(), reg);
                                    }
                                }
                                bridge.unregister_tool_by_name(&qualified);
                                mcp_state
                                    .disabled_tools
                                    .entry(server_name.clone())
                                    .or_default()
                                    .insert(tool_name.clone());
                            }

                            // Collect the new disabled set for this server before dropping lock.
                            let disabled_vec: Vec<String> = mcp_state
                                .disabled_tools
                                .get(&server_name)
                                .map(|s| s.iter().cloned().collect())
                                .unwrap_or_default();
                            drop(mcp_state);

                            session.refresh_mcp_snapshot_and_schedule_reminder().await;
                            session.refresh_goal_harness_enabled().await;

                            // Persist to config and emit notification in background.
                            let notifications = session.notifications.gateway.clone();
                            let session_id = session.session_info.id.0.clone();
                            let server_for_persist = server_name.clone();
                            tokio::task::spawn_local(async move {
                                if let Err(e) = crate::util::config::save_mcp_disabled_tools(
                                    &server_for_persist,
                                    &disabled_vec,
                                ).await {
                                    tracing::warn!(
                                        server = server_for_persist.as_str(),
                                        error = %e,
                                        "Failed to persist disabled_tools to config"
                                    );
                                }
                                // Emit the typed.
                                // The toggle-tool path is not server-scoped.
                                // So `server_name` / `tools` stay empty, and skip-if-empty drops them from the wire.
                                let payload = crate::extensions::mcp::McpToolsChanged {
                                    session_id: session_id.to_string(),
                                    server_name: String::new(),
                                    tools: Vec::new(),
                                };
                                if let Ok(params) =
                                    serde_json::value::to_raw_value(&payload)
                                {
                                    notifications.forward_fire_and_forget(acp::ExtNotification::new(crate::extensions::mcp::mcp_methods::TOOLS_CHANGED
                                            , params.into()));
                                }
                                let _ = respond_to.send(Ok(()));
                            });
                        }
                        SessionCommand::SnapshotMcpPool { respond_to } => {
                            let mcp_state = session.mcp_state.lock().await;
                            let pool = if mcp_state.owned_clients.is_empty() && mcp_state.shared_clients.is_empty() {
                                None
                            } else {
                                Some(crate::session::mcp_servers::SharedMcpPool::from_state(&mcp_state))
                            };
                            let _ = respond_to.send(pool);
                        }
                        SessionCommand::SnapshotClientHooks { respond_to } => {
                            let _ = respond_to.send(session.client_hooks.borrow().clone());
                        }
                        SessionCommand::SnapshotToolDefinitions { respond_to } => {
                            let defs = session.prepare_tool_definitions_inner().await;
                            let specs = session.turn_base_tool_specs(&defs);
                            let _ = respond_to.send(specs);
                        }
                        SessionCommand::SetClientHooks { hooks } => {
                            *session.client_hooks.borrow_mut() = hooks;
                        }
                        SessionCommand::GetMcpStatus { respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            let tool_bridge = session.agent.borrow().tool_bridge().clone();
                            let writer = session.events.writer();
                            tokio::task::spawn_local(async move {
                                let snapshot = crate::extensions::mcp::build_mcp_status(
                                    &mcp_state,
                                    &tool_bridge,
                                    Some(&writer),
                                ).await;
                                let _ = respond_to.send(snapshot);
                            });
                        }
                        SessionCommand::CallMcpTool { server_name, server_url, tool_name, arguments, respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let result = crate::extensions::mcp::call_mcp_tool(
                                    &mcp_state,
                                    &server_name,
                                    server_url.as_deref(),
                                    &tool_name,
                                    arguments,
                                ).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::ReadMcpResource { server_name, uri, respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let result = crate::extensions::mcp::read_mcp_resource(
                                    &mcp_state,
                                    &server_name,
                                    &uri,
                                ).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::McpAuthStatus { respond_to } => {
                            let mcp_state = session.mcp_state.clone();
                            tokio::task::spawn_local(async move {
                                let state = mcp_state.lock().await;
                                let entries: Vec<_> = state.auth_required.iter().map(|name| {
                                    crate::extensions::mcp::McpAuthStatusEntry {
                                        server_name: name.clone(),
                                        status: "needs_auth",
                                    }
                                }).collect();
                                let _ = respond_to.send(entries);
                            });
                        }
                        SessionCommand::McpAuthTrigger { server_name, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_mcp_auth_trigger(&server_name).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::GetManagedGatewayDisabledTools { respond_to } => {
                            let disabled_tools = crate::util::config::get_all_mcp_disabled_tools(
                                std::path::Path::new(&session.session_info.cwd),
                            );
                            let _ = respond_to.send(disabled_tools);
                        }
                        SessionCommand::RetryAuthRequiredServers { respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.retry_auth_required_servers().await;
                                s.retry_unreachable_servers().await;
                                let _ = respond_to.send(());
                            });
                        }
                        SessionCommand::RefreshMcpSearchIndex => {
                            session.refresh_mcp_snapshot_and_schedule_reminder().await;
                        }
                        SessionCommand::TriggerTestFeedback { tier, mode, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let request = s.feedback_manager.force_feedback_request(tier, mode).await;
                                let notification = crate::extensions::notification::FeedbackRequestNotification::from(request.clone());
                                s.send_feedback_notification(request).await;
                                let resp = ExtMethodResult::success(notification).to_ext_response();
                                let _ = respond_to.send(resp);
                            });
                        }
                        SessionCommand::PersistFeedback(entry) => {
                            let _ = session
                                .notifications.persistence_tx
                                .send(PersistenceMsg::Feedback(*entry));
                        }
                        SessionCommand::AdvertiseCommands { trigger } => {
                            session.send_available_commands_update(trigger).await;
                        }
                        SessionCommand::GetWorkflowCatalogState { respond_to } => {
                            let tool_names = session.registered_tool_names().await;
                            let has_runs = !session.workflow_tracker().await.lock().list().is_empty();
                            let availability =
                                session.build_command_availability(&tool_names, has_runs);
                            let _ = respond_to
                                .send((availability.workflows, availability.workflow_management));
                        }
                        SessionCommand::ListAvailableCommands { respond_to } => {
                            let skills = session.slash_skills_for_resolve().await;
                            let tool_names = session.registered_tool_names().await;
                            let has_runs = !session.workflow_tracker().await.lock().list().is_empty();
                            let availability =
                                session.build_command_availability(&tool_names, has_runs);
                            let (_, workflows) = session.named_workflow_snapshot();
                            let commands = slash_commands::available_commands(
                                &skills,
                                availability,
                                &workflows,
                            );
                            let _ = respond_to.send(slash_commands::ListCommandsResponse {
                                commands,
                                tools: Some(tool_names),
                            });
                        }
                        SessionCommand::ReloadSkills => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.reload_skills_from_disk().await;
                            });
                        }
                        SessionCommand::DispatchSessionStartHook { source } => {
                            // Observe hooks cannot gate; don't hold session/new on the command loop.
                            if !deferred_start.cancel.is_cancelled() {
                                let s = session.clone();
                                let cancel = deferred_start.cancel.clone();
                                let handle = tokio::task::spawn_local(async move {
                                    distill_hooks::runner::command::join_hook_group_reaps(async {
                                        let run = async {
                                            if cancel.is_cancelled() {
                                                return;
                                            }
                                            let envelope = s.fire_hook(
                                                distill_hooks::event::HookEventName::SessionStart,
                                                None,
                                                distill_hooks::event::HookPayload::SessionStart {
                                                    source,
                                                    model_id: None,
                                                    agent_type: None,
                                                },
                                            );
                                            let Some(registry) = s.hook_registry.borrow().clone()
                                            else {
                                                return;
                                            };
                                            let ctx = s.hook_run_ctx();
                                            let results =
                                                distill_hooks::dispatcher::dispatch_non_blocking(
                                                    &registry,
                                                    distill_hooks::event::HookEventName::SessionStart,
                                                    &envelope,
                                                    &ctx,
                                                )
                                                .await;
                                            if cancel.is_cancelled() {
                                                return;
                                            }
                                            s.send_hook_execution(
                                                &HookBatch::from_envelope(&envelope),
                                                &results,
                                            )
                                            .await;
                                        };
                                        tokio::select! {
                                            biased;
                                            _ = cancel.cancelled() => {}
                                            _ = run => {}
                                        }
                                    })
                                    .await;
                                });
                                deferred_start.arm(handle);
                            }
                        }
                        SessionCommand::GetFeedbackContext { turn_number, responds_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                use prod_mc_cli_chat_proxy_types::feedback_types::FeedbackToolOutcome;

                                let conv = s.chat_state_handle.get_conversation().await;
                                let turn_idx = turn_number
                                    .and_then(|n| usize::try_from(n).ok())
                                    .or_else(|| slash_feedback_last_turn(&conv));
                                let lookup = match turn_idx {
                                    Some(n) => turn_texts_for_feedback(&conv, n),
                                    None => FeedbackTurnLookup::default(),
                                };

                                let sh = s.signals_handle();
                                let (signals, tool_outcomes) = tokio::join!(
                                    sh.snapshot(),
                                    sh.last_turn_tool_outcomes(),
                                );
                                let signals = signals.unwrap_or_default();

                                let ctx = FeedbackContext {
                                    last_user_message: lookup.user_text,
                                    last_assistant_message: lookup.assistant_text,
                                    tool_outcomes: tool_outcomes
                                        .into_iter()
                                        .map(|o| FeedbackToolOutcome {
                                            tool_name: o.tool_name,
                                            calls: o.successes + o.failures,
                                            failures: o.failures,
                                        })
                                        .collect(),
                                    compaction_count: signals.compaction_count as i64,
                                    context_window_usage: signals.context_window_usage,
                                    context_tokens_used: signals.context_tokens_used,
                                    context_window_tokens: signals.context_window_tokens,
                                    session_cwd: s.tool_context.cwd.as_path().to_string_lossy().to_string(),
                                    reasoning_effort: lookup.reasoning_effort,
                                    model_id: lookup.model_id,
                                    model_fingerprint: lookup.model_fingerprint,
                                };
                                let _ = responds_to.send(ctx);
                            });
                        }
                        SessionCommand::GetActiveAgent { responds_to } => {
                            let agent_type = session.active_agent_type.lock().clone();
                            let _ = responds_to.send(agent_type);
                        }
                        SessionCommand::SideQuestion {
                            question,
                            images,
                            respond_to,
                        } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_side_question(&question, images).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::Recap { auto } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                s.handle_recap(auto).await;
                            });
                        }
                        SessionCommand::AISuggest { prefix, cwd, model_override, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_ai_suggest(&prefix, &cwd, model_override.as_deref()).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::SuggestPrompt { model_override, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_suggest_prompt(model_override.as_deref()).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::RewriteMemoryNote { raw_text, context_summary, respond_to } => {
                            let s = session.clone();
                            tokio::task::spawn_local(async move {
                                let result = s.handle_rewrite_memory_note(&raw_text, &context_summary).await;
                                let _ = respond_to.send(result);
                            });
                        }
                        SessionCommand::Interject { text, id, images } => {
                            // Broadcast to every attached client so every pane viewing this session renders the interjection block
                            // The originator dedups this echo by `id` against its optimistic local block; viewers render it
                            session.broadcast_interjection(&text, id.as_deref());
                            // Telemetry at enqueue (not drain) so it is recorded even when a cancel clears the buffer before the next drain point
                            session.events.emit(crate::session::events::Event::Interjected {
                                source: crate::session::events::InterjectionSource::Direct,
                                image_count: images.len() as u32,
                                redirect_kind: crate::session::events::RedirectKind::Interjection,
                            });
                            // Buffer only into an actually-running turn; only the turn loop drains the buffer
                            // The pager's running-state check races turn end, so an interjection can arrive while idle
                            // Buffered then, it would strand forever and silently drop the user's message; run it as its own prompt turn instead
                            let turn_running = session
                                .current_prompt_id
                                .lock()
                                .ok()
                                .and_then(|g| g.clone())
                                .is_some();
                            if turn_running {
                                session.pending_interjections.push(PendingInterjection {
                                    text,
                                    attachments: images,
                                });
                                tracing::info!("Queued mid-turn interjection");
                            } else {
                                session
                                    .queue_interjection_fallback_prompt(text, images, true)
                                    .await;
                                SessionActor::maybe_start_running_task(
                                    session.clone(),
                                    completion_tx.clone(),
                                )
                                .await;
                            }
                        }
                        SessionCommand::GoalSummaryTurn { prompt_text } => {
                            // Queue a synthetic prompt so the model gets a turn to print a visible progress summary
                            // Mirrors the pattern used by `maybe_drain_notifications`
                            let prompt_id = format!("goal-summary-{}", uuid::Uuid::now_v7());
                            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))];
                            let (respond_to, _) = tokio::sync::oneshot::channel();
                            {
                                let mut state = session.state.lock().await;
                                state.pending_inputs.push_back(InputItem {
                                    prompt_id,
                                    prompt_blocks,
                                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                                    trace_gcs_config: None,
                                    artifact_tracker: None,
                                    client_identifier: None,
                                    screen_mode: None,
                                    verbatim: true,
                                    json_schema: None,
                                    input_origin: InputOrigin::new(super::PromptOrigin::GoalSummary),
                                    task_wake_fallback: None,
                                    tool_overrides_update: None,
                                    respond_to,
                                    persist_ack: None,
                                    parsed_prompt_tx: None,
                                    initial_child_prompt_ready: None,
                                    queue_meta: None,
                                    queue_mutation_policy: QueueMutationPolicy::hidden(),
                                    send_now: false,
                                    traceparent: None,
                                });
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::WorkflowCompletionTurn { run_id, revision } => {
                            let state_suppressed = session.state.lock().await.notifications_suppressed;
                            let wake_suppressed = state_suppressed
                                || session.goal_loop_active()
                                || session
                                    .tool_context
                                    .task_wake_suppressed
                                    .as_ref()
                                    .is_some_and(|gate| gate.get());
                            let should_wake = if wake_suppressed {
                                false
                            } else {
                                let tracker = session.workflow_tracker().await;
                                tracker.lock().is_unreported_completion(&run_id, revision)
                            };
                            if !should_wake {
                                continue;
                            }
                            let prompt_id = format!("workflow-completed-{run_id}-{revision}");
                            let prompt_text = "A background workflow stopped. Review the workflow completion reminder, report the result to the user, and take any appropriate next action.";
                            let (respond_to, _) = tokio::sync::oneshot::channel();
                            {
                                let mut state = session.state.lock().await;
                                let workflow_wake_queued = state.pending_inputs.iter().any(|item| {
                                    matches!(
                                        item.input_origin.as_prompt_origin(),
                                        super::PromptOrigin::WorkflowCompleted { .. }
                                    )
                                });
                                if workflow_wake_queued {
                                    continue;
                                }
                                state.pending_inputs.push_back(InputItem {
                                    prompt_id,
                                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))],
                                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                                    trace_gcs_config: None,
                                    artifact_tracker: None,
                                    client_identifier: None,
                                    screen_mode: None,
                                    verbatim: true,
                                    json_schema: None,
                                    input_origin: InputOrigin::new(super::PromptOrigin::WorkflowCompleted {
                                        completion_id: format!("{run_id}-{revision}"),
                                    }),
                                    task_wake_fallback: None,
                                    tool_overrides_update: None,
                                    respond_to,
                                    persist_ack: None,
                                    parsed_prompt_tx: None,
                                    initial_child_prompt_ready: None,
                                    queue_meta: None,
                                    queue_mutation_policy: QueueMutationPolicy::hidden(),
                                    send_now: false,
                                    traceparent: None,
                                });
                            }
                            SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                        }
                        SessionCommand::TakeTurnMessages { respond_to } => {
                            let result = session.chat_state_handle.take_turn_messages().await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::TakeHarnessTraceTurns { respond_to } => {
                            let result = session.chat_state_handle.take_harness_trace_turns().await;
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::TakeStreamingCapture { prompt_id, respond_to } => {
                            // Out-of-band: never touches `chat_state`.
                            // The live slot is the only source of truth; there is no stash A queued prompt's `StreamStarted` racing this take resets the slot to the new prompt-id.
                            // We then log a tripwire before returning `None`.
                            let taken = {
                                let mut cap = session.streaming_turn_capture.lock();
                                if cap.prompt_id.as_deref() == Some(prompt_id.as_str()) {
                                    Some(std::mem::take(&mut *cap))
                                } else {
                                    // Race: the live slot now belongs to a different turn
                                    // Drop this take rather than misattribute the partial
                                    // The warn! is a production tripwire: if it ever fires in real traffic, add a per-prompt stash
                                    if !cap.is_empty() {
                                        tracing::warn!(
                                            requested_prompt_id = %prompt_id,
                                            slot_prompt_id = ?cap.prompt_id,
                                            "streaming_capture race: live slot belongs to a different prompt; \
                                             dropping streaming_partial.json for the requested turn",
                                        );
                                    }
                                    None
                                }
                            };
                            // Consolidate outside the lock: `finalize_for_upload` builds an up-to-8MB joined string
                            // It must not run while sampler events for a racing same-session turn contend for the mutex
                            // Keep only uncommitted generations; if the capture is empty afterwards there is nothing to upload
                            let result = taken.and_then(|mut cap| {
                                cap.finalize_for_upload();
                                (!cap.is_empty()).then_some(cap)
                            });
                            let _ = respond_to.send(result);
                        }
                        SessionCommand::PersistGitHead { commit, branch } => {
                            let _ = session.notifications.persistence_tx.send(
                                PersistenceMsg::GitHead { commit, branch },
                            );
                        }
                        SessionCommand::PersistResumeStatus { respond_to } => {
                            session.persist_resume_status().await;
                            let _ = respond_to.send(());
                        }
                        SessionCommand::Shutdown(kind) => {
                            let end_timer = session_end::SessionEndTimer::new_shared();
                            session.persist_resume_status().await;
                            shutdown_workflows(&session, &end_timer).await;
                            // Flush the actor-owned replay buffer so streamed chunks still pending at shutdown are committed to updates.jsonl.
                            // The commit must precede the session-directory snapshot for trace upload.
                            // Mirrors the same flush in the.
                            if let Some(notification) = replay_buffer.flush() {
                                session.emit_buffered(notification).await;
                            }
                            // Session is ending: do not leave a side-call holding an Arc into this actor
                            session.abort_turn_summary();
                            session.abort_title_refresh();
                            // This arm returns; an unanswered turn would race teardown and report `EndTurn` for unfinished work
                            if kind == crate::session::ShutdownKind::CancelRunningTurn {
                                // Shutdown is not a stop gesture: the session-end `Stop` reports this teardown
                                let _ = session
                                    .cancel_running_task(crate::session::CancelOptions {
                                        cancel_subagents: true,
                                        kill_background_tasks: true,
                                        history: crate::session::CancelHistoryDisposition::Keep,
                                        trigger: Some(crate::session::CancelTrigger::Shutdown),
                                        user_initiated: false,
                                    })
                                    .await;
                            }
                            session
                                .settle_all_parent_messages(
                                    distill_message_delivery_core::TerminalCause::HardTeardown,
                                )
                                .await;
                            // Drop any queued synthetic auto-wake prompts and pending notifications before running hooks A synthetic prompt can slip through the per-tool-result sweep A later persistence path would then flush it to chat_history.jsonl.
                            // That leaves a trailing `<system-reminder>` with no assistant reply.
                            // Placed BEFORE hook dispatch so the cleanup runs even if hooks abort.
                            session.drop_pending_synthetic_items().await;

                            // ── session_end (shutdown path) ────────────
                            // Hooks fire BEFORE memory auto-save
                            turn_end_queue.flush().await;
                            fire_session_end_hooks(&session, "shutdown", &end_timer, &mut deferred_start).await;
                            session.memory.stop_capture_worker().await;
                            session.memory.dream_workers.cancel_and_join().await;
                            // Stop the dream before the end-pipeline reindex so their index writes cannot race.
                            stop_dream(&mut dream_task).await;
                            session
                                .run_session_end_memory_pipeline(
                                    "session summary saved",
                                    &end_timer,
                                )
                                .await;
                            log_session_ended(&session).await;
                            turn_end_queue.drain().await;
                            finish_session_exit_feedback(&session, &end_timer).await;
                            emit_session_end_timings(&end_timer, session.startup_hints.is_subagent)
                                .await;
                            return;
                        }
                    }
            }
                // Prefer cmd_rx when both are already waiting so a queued hold/edit can land before the turn-end promotion (biased select)
                maybe_completion = completion_rx.recv() => {
                    let Some(super::turn_task::TurnCompletionMsg {
                        prompt_id,
                        epoch,
                        task_identity,
                        result,
                        elapsed_ms,
                        #[cfg(test)]
                        processed,
                    }) = maybe_completion
                    else {
                        session
                            .settle_all_parent_messages(
                                distill_message_delivery_core::TerminalCause::ActorDrop,
                            )
                            .await;
                        // Completion channel closed: full feedback teardown so the final signal sync and upload drain still run
                        // Cancel alone does not force-sync; shutdown owns that
                        // No session-end hooks here, but the flush still precedes `shutdown_workflows`, which makes a queued report's entry durable
                        let end_timer = session_end::SessionEndTimer::new_shared();
                        turn_end_queue.flush().await;
                        session.memory.stop_capture_worker().await;
                        session.memory.dream_workers.cancel_and_join().await;
                        // Stop the dream so it does not outlive the session holding the mutex.
                        stop_dream(&mut dream_task).await;
                        shutdown_workflows(&session, &end_timer).await;
                        turn_end_queue.drain().await;
                        finish_session_exit_feedback(&session, &end_timer).await;
                        emit_session_end_timings(&end_timer, session.startup_hints.is_subagent)
                            .await;
                        return;
                    };
                    // Flush any buffered turn deltas before `handle_completion` emits the durable `TurnCompleted`
                    // `TurnCompleted` then lands in updates.jsonl strictly after the turn's last `session/update` delta
                    // Mirrors the Cancel / Shutdown / FlushComplete arms
                    if let Some(notification) = replay_buffer.flush() {
                        session.emit_buffered(notification).await;
                    }
                    let (turn_succeeded, suppress_goal_continuation, infra_pause_message) =
                        SessionActor::post_turn_goal_degradation_plan(&result);
                    // A `RemovedFromQueue` completion never started a turn, so there is nothing to summarize below
                    let turn_ran = !matches!(
                        &result,
                        Ok(PromptTurnOk {
                            completion_kind: PromptCompletionKind::RemovedFromQueue,
                            ..
                        })
                    );
                    // Capture only a genuine root query that completed its tool loop with
                    // EndTurn. Synthetic wakes and built-ins also produce PromptTurnOk, but
                    // neither is durable conversation evidence for memory extraction.
                    let v2_capture_eligible = super::memory_capture::is_successful_query_loop(
                        &result,
                    ) && {
                        let state = session.state.lock().await;
                        state.pending_inputs.front().is_some_and(|input| {
                            input.prompt_id == prompt_id
                                && input.queue_meta.is_some()
                                && !input.input_origin.is_synthetic()
                                && crate::session::slash_authority::parse_slash_prefix(
                                    &input.prompt_blocks,
                                )
                                .is_none()
                        })
                    };
                    let v2_capture_source_prompt_index = if v2_capture_eligible {
                        Some(*session.tool_context.prompt_index.lock().await)
                    } else {
                        None
                    };
                    let completed_prompt_id = prompt_id.clone();
                    if !session
                        .handle_completion(prompt_id, epoch, &task_identity, result, elapsed_ms)
                        .await
                    {
                        #[cfg(test)]
                        if let Some(processed) = processed {
                            let _ = processed.send(());
                        }
                        continue;
                    }
                    if let Some(source_prompt_index) = v2_capture_source_prompt_index {
                        session
                            .enqueue_v2_completed_turn(source_prompt_index)
                            .await;
                    }
                    #[cfg(test)]
                    if let Some(processed) = processed {
                        let _ = processed.send(());
                    }
                    // Drain monitor events that were routed to the mid-turn buffer but arrived after the turn ended
                    // The is_turn_active check races the buffer push
                    session.drain_monitor_buffer_to_pending().await;
                    if let Some(message) = infra_pause_message {
                        session.apply_infra_pause_after_turn_err(message).await;
                    }
                    // Goal continuation (success) or back-off (non-success).
                    // Owns the streak-tracking and reminder-injection path.
                    session
                        .handle_turn_end(turn_succeeded, suppress_goal_continuation)
                        .await;
                    // Convert them to front-of-queue prompt turns so the message runs instead of stranding.
                    // INVARIANT: this flush must only ever see interjections aimed at the turn that just completed.
                    // Both cancel paths drain the buffer before their completion arrives.
                    let flushed_interjections = session.flush_stranded_interjections().await;
                    if flushed_interjections > 0 {
                        tracing::info!("Flushed stranded interjection(s) into prompt turns");
                        // Typed before the block verdict was visible, so they stay under the hook-block hold instead of auto-running
                        // The notice keeps them from being parked silently
                        if session.state.lock().await.hook_block_held() {
                            session
                                .send_hook_annotation(&format!(
                                    "\u{26a0} {flushed_interjections} interjection(s) joined the held queue. Send a prompt to resume."
                                ))
                                .await;
                        }
                    }
                    SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
                    // If no user prompt started, check for pending notifications
                    SessionActor::maybe_drain_notifications(session.clone(), completion_tx.clone()).await;
                    session.emit_session_idle_if_idle().await;
                    // LazinessDetector: spawn an idle-triggered classifier dispatch.
                    // The method is a no-op when the per-model `laziness_detector.enabled = false` (the v1 default for every model).
                    // No classification cost is incurred without explicit opt-in.
                    {
                        let s = session.clone();
                        tokio::task::spawn_local(async move {
                            s.maybe_fire_laziness_check().await;
                        });
                    }
                    // Per-turn dashboard summary (display-only side-call); spawned so the actor loop keeps accepting commands.
                    // `turn_succeeded` keeps cancelled/errored/refused turns from triggering a fresh model call.
                    // A user who hit Ctrl+C wants model activity to stop.
                    if turn_ran && turn_succeeded {
                        session.restart_turn_summary(completed_prompt_id);
                        // Early-session auto-title refresh (turns 3 and 6), then frozen
                        // Same success gating as the turn summary
                        session.maybe_refresh_title();
                    }
                }
        }
    }
}
pub(super) fn slash_feedback_last_turn(
    conversation: &[distill_sampling_types::ConversationItem],
) -> Option<usize> {
    use distill_sampling_types::ConversationItem;
    if let Some(n) = conversation.iter().rev().find_map(|item| match item {
        ConversationItem::User(u) => u.prompt_index,
        _ => None,
    }) {
        return Some(n);
    }
    let mut seen_unmarked_preamble = false;
    let mut n = 0usize;
    for item in conversation {
        let ConversationItem::User(u) = item else {
            continue;
        };
        if u.synthetic_reason.is_human() && !seen_unmarked_preamble {
            seen_unmarked_preamble = true;
            continue;
        }
        if !u.synthetic_reason.starts_prompt_turn() {
            continue;
        }
        n += 1;
    }
    n.checked_sub(1)
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct FeedbackTurnLookup {
    pub user_text: Option<String>,
    pub assistant_text: Option<String>,
    pub reasoning_effort: Option<crate::sampling::ReasoningEffort>,
    pub model_id: Option<String>,
    pub model_fingerprint: Option<String>,
}
pub(super) fn turn_texts_for_feedback(
    conversation: &[distill_sampling_types::ConversationItem],
    turn_number: usize,
) -> FeedbackTurnLookup {
    use distill_sampling_types::ConversationItem;
    let user_prompt_index = |item: &ConversationItem| match item {
        ConversationItem::User(u) => u.prompt_index,
        _ => None,
    };
    let is_unmarked_prompt_user = |item: &ConversationItem| match item {
        ConversationItem::User(u) => {
            u.prompt_index.is_none() && u.synthetic_reason.starts_prompt_turn()
        }
        _ => false,
    };
    let (start, exact) = if let Some(i) = conversation
        .iter()
        .position(|item| user_prompt_index(item) == Some(turn_number))
    {
        (i, true)
    } else {
        let mut seen_unmarked_preamble = false;
        let mut n = 0usize;
        let mut i = None;
        for (idx, item) in conversation.iter().enumerate() {
            let ConversationItem::User(u) = item else {
                continue;
            };
            if u.prompt_index.is_some() {
                break;
            }
            if u.synthetic_reason.is_human() && !seen_unmarked_preamble {
                seen_unmarked_preamble = true;
                continue;
            }
            if !u.synthetic_reason.starts_prompt_turn() {
                continue;
            }
            if n == turn_number {
                i = Some(idx);
                break;
            }
            n += 1;
        }
        let Some(i) = i else {
            return FeedbackTurnLookup::default();
        };
        (i, false)
    };
    let end = conversation
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(i, item)| {
            (user_prompt_index(item).is_some() || (!exact && is_unmarked_prompt_user(item)))
                .then_some(i)
        })
        .unwrap_or(conversation.len());
    let Some(raw) = conversation.get(start).map(|item| item.text_content()) else {
        return FeedbackTurnLookup::default();
    };
    let extracted = distill_chat_state::compaction_utils::extract_user_query(&raw);
    let user_text = (!extracted.is_empty()).then_some(extracted);
    let window = conversation.get(start + 1..end).unwrap_or(&[]);
    let assistant_text = window.iter().find_map(|item| {
        let ConversationItem::Assistant(a) = item else {
            return None;
        };
        (!a.content.trim().is_empty()).then(|| a.content.as_ref().to_owned())
    });
    let reasoning_effort = window.iter().find_map(|item| {
        let ConversationItem::Assistant(a) = item else {
            return None;
        };
        a.reasoning_effort
    });
    let (model_id, model_fingerprint) = window
        .iter()
        .find_map(|item| {
            let ConversationItem::Assistant(a) = item else {
                return None;
            };
            let id = a.model_id.clone()?;
            Some((Some(id), a.model_fingerprint.clone()))
        })
        .unwrap_or((None, None));
    FeedbackTurnLookup {
        user_text,
        assistant_text,
        reasoning_effort,
        model_id,
        model_fingerprint,
    }
}
