// Modified for Distill by Samuel Fajreldines, 2026.
use super::attempt_runner::{
    InitialChildPromptReadiness, OneTurnAttemptInput, OneTurnAttemptOutcome, OneTurnTraceCapture,
    OneTurnUsageInput, capture_and_fold_one_turn_usage, run_one_turn_attempt,
    wait_initial_child_prompt_readiness,
};
use super::prompt_turn_receipt::{
    ACTIVE_MESSAGE_RECEIPT_CAPACITY, AdmissionSettlement, FinalPromptTurnReceipt,
    PromptTurnReceiptDrain, PromptTurnReceiptOutcome, PromptTurnSettlementInput,
    reduce_prompt_turn_settlement,
};
use super::start_artifact_publication::{
    PreparedStartArtifacts, PublicationBoundary, StartArtifactPublication,
};
use super::*;
use crate::upload::trace::PromptMetadataParams;
use distill_sampling_types::ReasoningEffort;
use distill_telemetry::region;
use distill_telemetry::region::Parent;
use distill_telemetry::subagent_spawn::{SubagentSpawnPhase, phase_region};
use distill_tools::implementations::distill::task::types::ActiveAgentMessageSource;
use distill_tools::implementations::distill::task::types::SubagentCapabilityModeExt;
use distill_tools::implementations::{distill, opencode};
use distill_tools::types::tool::ToolKind;
static SUBAGENTS_ACTIVE: distill_telemetry::activity::ActivityGauge =
    distill_telemetry::activity::ActivityGauge::work(
        distill_telemetry::activity::SUBAGENTS_ACTIVE_KEY,
    );
/// Bounds each parent-side await in the child completion path. The parent's biased select polls its event channels ahead of `cmd_rx`, so a busy turn can starve `cmd_rx` and park a completed child (leaking its session thread, fs watchers, and fds) forever.
pub(super) const PARENT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// `PARENT_ACK_TIMEOUT`-bounded acks in the completion path: usage fold,
/// not-applied mark, list_tasks snapshot, notification reparent.
const PARENT_ACK_SITES: u128 = 4;
pub(super) fn agent_memory_scope_for_mode(
    scope: Option<distill_agent::config::MemoryScope>,
    mode: crate::config::MemoryMode,
) -> Option<distill_agent::config::MemoryScope> {
    scope.filter(|_| !mode.is_v2())
}
const _: () = assert!(
    PARENT_ACK_SITES * PARENT_ACK_TIMEOUT.as_millis()
        <= crate::session::acp_session::SUBAGENT_USAGE_DRAIN.as_millis(),
    "sequential parent acks must fit the turn-freeze usage drain; a new \
     PARENT_ACK_TIMEOUT-bounded await must bump PARENT_ACK_SITES"
);
/// Bounds each read of the child session's own actors (chat state, signals) in the completion path. They are spawned on the child session's current-thread runtime, so a tool synchronously blocking that thread freezes them together with the session actor; teardown must still reach
/// Shutdown on the cheap fallbacks.
pub(super) const CHILD_ACTOR_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// One bounded query against a child-session actor, degrading to `fallback`
/// when the actor never answers.
pub(super) async fn child_actor_query<T>(
    what: &'static str,
    query: impl std::future::Future<Output = T>,
    fallback: T,
) -> T {
    match tokio::time::timeout(CHILD_ACTOR_ACK_TIMEOUT, query).await {
        Ok(value) => value,
        Err(_) => {
            tracing::warn!(
                query = what,
                "child actor did not answer in time; using fallback"
            );
            fallback
        }
    }
}
/// Fallback when the usage fold missed: ask the parent session to mark the bill incomplete (sticky + pin-aware ledger marks); if the parent doesn't ack, mark the shared coordinator's report-level sticky directly.
/// A merely-slow parent can later apply both the queued fold and this mark; both are idempotent, so the worst case is a conservative incomplete flag, never a double-count.
/// (Distinct from `SessionActor::mark_subagent_usage_not_applied`, which is the parent-side coordinator-only mark.)
pub(super) async fn mark_child_usage_not_applied_with_fallback(
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    subagent_event_tx: &mpsc::UnboundedSender<SubagentEvent>,
    parent_session_id: &str,
    parent_prompt_id: Option<String>,
) {
    let marked_by_parent = if let Some(cmd_tx) = parent_cmd_tx {
        let (respond_to, ack) = oneshot::channel();
        if cmd_tx
            .send(SessionCommand::MarkSubagentUsageNotApplied {
                parent_prompt_id: parent_prompt_id.clone(),
                respond_to,
            })
            .is_ok()
        {
            matches!(
                tokio::time::timeout(PARENT_ACK_TIMEOUT, ack).await,
                Ok(Ok(()))
            )
        } else {
            false
        }
    } else {
        false
    };
    if !marked_by_parent && let Some(pid) = parent_prompt_id {
        let (respond_to, ack) = oneshot::channel();
        if subagent_event_tx
            .send(SubagentEvent::MarkUsageNotApplied(
                SubagentMarkUsageNotAppliedRequest {
                    parent_session_id: parent_session_id.to_string(),
                    prompt_id: pid,
                    respond_to,
                },
            ))
            .is_ok()
        {
            let _ = ack.await;
        }
    }
}
/// Bounded turn-message take from the child's chat state. A take the actor never answers — wedged (timeout) or gone (dropped channel) — is a recorded miss; `complete_prompt_trace` must not mistake it for a genuinely empty turn.
pub(super) async fn take_child_turn_messages(
    cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    upload_deadline: tokio::time::Instant,
) -> crate::upload::turn::TurnMessages {
    use crate::upload::turn::{MissingTurnMessages, TurnMessages};
    let (tx, rx) = tokio::sync::oneshot::channel();
    if cmd_tx
        .send(SessionCommand::TakeTurnMessages { respond_to: tx })
        .is_err()
    {
        return TurnMessages::Missing(MissingTurnMessages::ChannelDropped);
    }
    match tokio::time::timeout(
        crate::upload::trace::blocking_attempt_budget(upload_deadline),
        rx,
    )
    .await
    {
        Ok(Ok(taken)) => taken.into(),
        Ok(Err(_)) => {
            tracing::warn!("TakeTurnMessages responder dropped; recording the miss");
            TurnMessages::Missing(MissingTurnMessages::ChannelDropped)
        }
        Err(_) => {
            tracing::warn!("TakeTurnMessages not answered in time; recording the miss");
            TurnMessages::Missing(MissingTurnMessages::TakeTimedOut)
        }
    }
}
/// Bounded out-of-band streaming-capture take: a wedged child actor skips the
/// capture instead of parking teardown.
pub(super) async fn take_child_streaming_partial(
    cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    upload_deadline: tokio::time::Instant,
    prompt_id: String,
    committed: bool,
    model_id: Option<String>,
) -> Option<crate::session::acp_session::StreamingTurnCapture> {
    tokio::time::timeout(
        crate::upload::trace::blocking_attempt_budget(upload_deadline),
        crate::upload::turn::take_streaming_partial(cmd_tx, prompt_id, committed, model_id),
    )
    .await
    .unwrap_or_else(|_| {
        tracing::warn!("TakeStreamingCapture not answered in time; skipping capture");
        None
    })
}
/// Bounded conversation-global resolved-model read for `turn_result.json`,
/// falling back to `configured` when the child actor is wedged or has served
/// no assistant turn.
pub(super) async fn resolve_child_model(
    cmd_tx: &mpsc::UnboundedSender<SessionCommand>,
    upload_deadline: tokio::time::Instant,
    configured: Option<String>,
) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let resolved = if cmd_tx
        .send(SessionCommand::GetModelMetadata { responds_to: tx })
        .is_ok()
    {
        match tokio::time::timeout(
            crate::upload::trace::blocking_attempt_budget(upload_deadline),
            rx,
        )
        .await
        {
            Ok(meta) => meta.unwrap_or_default().resolved_model_id,
            Err(_) => {
                tracing::warn!("GetModelMetadata not answered in time; using configured model");
                None
            }
        }
    } else {
        None
    };
    resolved.or(configured)
}
/// Snapshot goal-internal task ids, tag them goal-turn origin, then reparent surviving background tasks (monitors, bg commands) from the child to the parent's notification bridge. Both awaits ride the parent terminal actor's channel and are bounded so the child Shutdown stays reachable behind a starved actor.
pub(super) async fn reparent_surviving_child_tasks(
    parent_tb: &std::sync::Arc<dyn distill_tools::computer::types::TerminalBackend>,
    parent_notif_handle: &distill_tools::notification::types::ToolNotificationHandle,
    parent_cmd_tx: Option<&mpsc::UnboundedSender<SessionCommand>>,
    child_session_id: &str,
    parent_session_id: &str,
    surface_completion: bool,
) {
    if !surface_completion {
        match tokio::time::timeout(PARENT_ACK_TIMEOUT, parent_tb.list_tasks()).await {
            Ok(tasks) => {
                let reparented_task_ids: Vec<String> = tasks
                    .into_iter()
                    .filter(|t| {
                        !t.completed && t.owner_session_id.as_deref() == Some(child_session_id)
                    })
                    .map(|t| t.task_id)
                    .collect();
                if !reparented_task_ids.is_empty()
                    && let Some(cmd_tx) = parent_cmd_tx
                {
                    let _ = cmd_tx.send(SessionCommand::RecordGoalTurnTaskIds {
                        task_ids: reparented_task_ids,
                    });
                }
            }
            Err(_) => {
                tracing::warn!(
                    child_session_id = %child_session_id,
                    parent_session_id = %parent_session_id,
                    "list_tasks not answered in time; skipping goal-turn task-id recording"
                );
            }
        }
    }
    let parent_backend_weak = std::sync::Arc::downgrade(parent_tb);
    if tokio::time::timeout(
        PARENT_ACK_TIMEOUT,
        parent_tb.reparent_notifications(
            child_session_id,
            parent_session_id,
            parent_notif_handle.clone(),
            parent_backend_weak,
        ),
    )
    .await
    .is_err()
    {
        tracing::warn!(
            child_session_id = %child_session_id,
            parent_session_id = %parent_session_id,
            "reparent_notifications not acked in time; proceeding to child shutdown"
        );
    }
}
const INITIAL_PROMPT_ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const CHILD_COMPLETION_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(not(test))]
const WAKE_START_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const WAKE_START_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
#[derive(Clone)]
enum WakePersistenceContext {
    Ordinary,
    Wake {
        prior: crate::session::persistence::WakeSummaryState,
    },
}
impl WakePersistenceContext {
    fn turn_number(&self, ordinary: Option<u64>) -> u64 {
        match self {
            Self::Ordinary => ordinary.unwrap_or(0),
            Self::Wake { prior } => prior.next_trace_turn,
        }
    }
    async fn abort(
        &self,
        persistence_tx: &mpsc::UnboundedSender<crate::session::persistence::PersistenceMsg>,
        hold_ack: bool,
        subagent_id: &str,
        turn_number: u64,
    ) {
        let Self::Wake { prior } = self else {
            return;
        };
        let (respond_to, response_rx) = oneshot::channel();
        if persistence_tx
            .send(crate::session::persistence::PersistenceMsg::WakeAbort {
                prior: prior.clone(),
                respond_to,
            })
            .is_err()
        {
            return;
        }
        let response = async move {
            if hold_ack {
                std::future::pending().await
            } else {
                response_rx.await
            }
        };
        if tokio::time::timeout(WAKE_START_FLUSH_TIMEOUT, response)
            .await
            .is_err()
        {
            tracing::warn!(
                subagent_id,
                turn_number,
                "wake abort did not persist before the deadline"
            );
        }
    }
}
pub(super) fn task_model_override_error(
    requested: Option<&str>,
    provenance: ModelOverrideProvenance,
    is_resume: bool,
    available: &indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
    is_session_auth: bool,
) -> Option<String> {
    if provenance != ModelOverrideProvenance::Tool || is_resume {
        return None;
    }
    let requested = requested?;
    crate::agent::remote_config::task_model_error_for_catalog(requested, available, is_session_auth)
}
#[tracing::instrument(
    name = "subagent.handle_request",
    skip_all,
    fields(
        subagent_id = %run.request.id,
        parent_session_id = %ctx.parent_session_id,
        subagent_type = %run.request.subagent_type,
    )
)]
pub(crate) async fn run_shell_child(
    run: distill::task::coordinator::ChildRunRequest<ShellChildRuntime>,
    mut ctx: SubagentSpawnContext,
    mut completion_data: ShellCompletionData,
    gateway: GatewaySender,
    mut spawn_root: Option<tracing::Span>,
) -> ChildRunOutput<ShellCompletionData> {
    if let Some(tp) = run.request.spawn_root.traceparent() {
        distill_otel::link_current_span_to_meta(&serde_json::json!({ "traceparent": tp }));
    }
    let distill::task::coordinator::ChildRunRequest {
        mut request,
        cancellation: cancel_token,
        reporter,
        attempt_id: attempt_identity,
        generation: target_generation,
        mut agent_message_sender,
        wake_origin,
        queued_for,
        session_running,
        agent_address,
        spawner_session_id: _,
    } = run;
    let is_wake = wake_origin.is_some();
    let (wake_message_source, wake_message_id) = match wake_origin {
        Some(origin) => (Some(origin.source), Some(origin.message_id)),
        None => (None, None),
    };
    let Some(agent_id) = distill_message_delivery_core::AgentId::from_uuid_v7(request.id.clone())
    else {
        let completion_data = ShellCompletionData {
            attempt_id: None,
            turn_number: None,
            ..completion_data
        };
        return child_run_output(
            failure_result(&request, "Subagent id must be a UUIDv7"),
            completion_data,
            None,
        );
    };
    request.id = agent_id.to_string();
    let start = std::time::Instant::now();
    let spawn_timer = distill_telemetry::subagent_spawn::SubagentSpawnTimer::new_shared();
    if let Some(queued) = queued_for {
        spawn_timer.record(SubagentSpawnPhase::QueueWait, queued);
    }
    let spawn_prepare_span = phase_region(SubagentSpawnPhase::SpawnPrepare);
    crate::waterfall::mark(&request.id, crate::waterfall::stage::CHILD_ENTER);
    let attempt_id = Some(attempt_identity.to_string());
    if request.owner.is_workflow() && cancel_token.is_cancelled() {
        return child_run_output(
            cancelled_result(&request, "Subagent was cancelled"),
            completion_data,
            None,
        );
    }
    if resolve_agent_definition(&request.subagent_type, &ctx).is_none()
        && let Some(resolved) = jev_type::jev_resolve_unknown_subagent_type(&request, &ctx).await
    {
        tracing::info!(
            requested = %request.subagent_type,
            resolved,
            "jev: unknown subagent type resolved to an allowed definition"
        );
        request.subagent_type = resolved;
    }
    let Some(mut definition) = resolve_agent_definition(&request.subagent_type, &ctx) else {
        let msg = format!("Unknown subagent type: {}", request.subagent_type);
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    };
    match gate_subagent_type(&request.subagent_type, &ctx) {
        SubagentValidateTypeOutcome::Disabled => {
            let msg = format!(
                "Subagent '{}' is disabled via [subagents.toggle] in config.toml",
                request.subagent_type
            );
            return child_run_output(failure_result(&request, &msg), completion_data, None);
        }
        SubagentValidateTypeOutcome::NotAllowed { allowed } => {
            let msg = format!(
                "agent can only spawn: {}; '{}' not allowed",
                allowed.join(", "),
                request.subagent_type
            );
            return child_run_output(failure_result(&request, &msg), completion_data, None);
        }
        SubagentValidateTypeOutcome::Ok => {}
        _ => {
            let msg = format!("Cannot validate subagent '{}'", request.subagent_type);
            return child_run_output(failure_result(&request, &msg), completion_data, None);
        }
    }
    resolve_subagent_toolset(
        &request.subagent_type,
        request.runtime_overrides.harness_agent_type.as_deref(),
        &ctx,
        &mut definition,
    );
    let cwd = ctx
        .parent_session_info
        .as_ref()
        .map(|i| std::path::Path::new(&i.cwd));
    let mut effective_runtime = distill_subagent_resolution::resolve_runtime_config(
        &request.subagent_type,
        &request.runtime_overrides,
        &ctx.subagent_roles,
        &ctx.subagent_personas,
        cwd,
        &definition,
    );
    let prompt = request.prompt.clone();
    if let Some(ref err) = effective_runtime.persona_error {
        tracing::error!(
            subagent_id = %request.id,
            error = err,
            "Persona resolution failed, aborting subagent spawn"
        );
        return child_run_output(failure_result(&request, err), completion_data, None);
    }
    if let Some(ref warn) = effective_runtime.role_prompt_warning {
        tracing::warn!(
            subagent_id = %request.id,
            warning = warn,
            "Role prompt_file degraded, continuing without role prompt"
        );
    }
    let resume_source = if is_wake {
        #[cfg(test)]
        {
            ctx.run_shell_child_harness
                .as_ref()
                .map(|harness| harness.meta_dir.as_path())
                .or_else(|| {
                    ctx.setup_failure
                        .as_ref()
                        .map(SubagentSetupFailure::meta_dir)
                })
                .and_then(|meta_dir| {
                    durable_resume_source_from_meta(
                        &meta_dir.join("meta.json"),
                        &ctx.parent_session_id,
                    )
                })
                .or_else(|| {
                    durable_resume_source_for(&request.id, &ctx.parent_session_id, &ctx.parent_cwd)
                })
        }
        #[cfg(not(test))]
        {
            durable_resume_source_for(&request.id, &ctx.parent_session_id, &ctx.parent_cwd)
        }
    } else if let Some(resume_id) = request
        .resume_from
        .as_deref()
        .filter(|s| distill_tool_types::is_not_sentinel(s))
    {
        match reporter
            .resume_source(resume_id, &ctx.parent_session_id)
            .await
        {
            SubagentResumeLookup::Active => {
                let error = format!(
                    "Cannot resume from subagent '{resume_id}': it is still running. \
                     Wait for it to complete before resuming."
                );
                return child_run_output(failure_result(&request, &error), completion_data, None);
            }
            SubagentResumeLookup::Completed(info) => Some(ResumeSourceData::from(*info)),
            SubagentResumeLookup::Missing => {
                match durable_resume_source_for(resume_id, &ctx.parent_session_id, &ctx.parent_cwd)
                {
                    Some(info) => Some(info),
                    None => {
                        let error = format!(
                            "Cannot resume from subagent '{resume_id}': not found. \
                             The subagent may have been evicted or the ID is invalid."
                        );
                        return child_run_output(
                            failure_result(&request, &error),
                            completion_data,
                            None,
                        );
                    }
                }
            }
        }
    } else {
        None
    };
    // The in-memory coordinator source predates durable Jev policy metadata.
    // Merge the on-disk terminal record before resolving the child: it carries
    // live `/model` and `/effort` changes made after the child was created.
    // Legacy records stay `None` and use the conservative policy below.
    let mut resume_source = resume_source;
    if let Some(source) = resume_source.as_mut()
        && let Some(durable) =
            durable_resume_source_for(&source.subagent_id, &ctx.parent_session_id, &ctx.parent_cwd)
    {
        if source.effort_auto.is_none() {
            source.effort_auto = durable.effort_auto;
        }
        if source.model_routing_locked.is_none() {
            source.model_routing_locked = durable.model_routing_locked;
        }
        // The durable record is the completed source's effective model, so it
        // wins over a stale in-memory spawn snapshot. A caller-supplied model
        // is applied later and therefore still has precedence.
        if durable.model_id.is_some() {
            source.model_id = durable.model_id;
        }
    }
    if is_wake && resume_source.is_none() {
        let error = format!(
            "Cannot reactivate subagent '{}': persisted session state is unavailable.",
            request.id
        );
        return child_run_output(failure_result(&request, &error), completion_data, None);
    }
    let explicit_model_override = request.runtime_overrides.model.is_some();
    let explicit_effort_override = request
        .runtime_overrides
        .reasoning_effort
        .as_deref()
        .is_some_and(|raw| !raw.eq_ignore_ascii_case("auto"));
    let model_routing_locked = explicit_model_override
        || resume_source
            .as_ref()
            .and_then(|source| source.model_routing_locked)
            .unwrap_or(false);
    if let Some(ref source) = resume_source {
        if explicit_model_override {
            tracing::debug!(
                subagent_id = %request.id,
                "Keeping explicit caller model override on resume"
            );
        } else {
            effective_runtime.model = None;
        }
        if let Err(e) = distill_subagent_resolution::validate_resume_identity(
            &request.subagent_type,
            request.runtime_overrides.persona.as_deref(),
            source,
        ) {
            return child_run_output(
                failure_result(&request, &e.to_string()),
                completion_data,
                None,
            );
        }
    }
    if let Some(error) = task_model_override_error(
        request.runtime_overrides.model.as_deref(),
        request.runtime_overrides.model_override_provenance,
        resume_source.is_some(),
        &ctx.available_models,
        ctx.auth_manager
            .current_or_expired()
            .is_some_and(|a| a.is_session_auth()),
    ) {
        return child_run_output(failure_result(&request, &error), completion_data, None);
    }
    let worktree_path = if let Some(ref source) = resume_source {
        if effective_runtime.isolation != distill_tool_types::SubagentIsolationMode::None
            && source.worktree_path.is_none()
        {
            tracing::info!(
                subagent_id = %request.id,
                "Ignoring isolation=worktree override: resumed source had no worktree"
            );
        }
        match source.worktree_path.as_deref() {
            None => None,
            Some(dest) => {
                match resume_worktree_action(dest.is_dir(), source.snapshot_ref.as_deref()) {
                    ResumeWorktreeAction::Reuse => Some(dest.to_path_buf()),
                    ResumeWorktreeAction::Rehydrate => {
                        let snapshot_ref = source.snapshot_ref.clone().unwrap_or_default();
                        let source_repo = resolve_subagent_source_repo(&ctx);
                        match crate::session::worktree::rehydrate_subagent_worktree(
                            dest,
                            &source_repo,
                            &snapshot_ref,
                            Some(source.subagent_id.as_str()),
                        )
                        .await
                        {
                            Ok(path) => {
                                tracing::info!(
                                    subagent_id = %request.id,
                                    worktree_path = %path.display(),
                                    snapshot_ref = %snapshot_ref,
                                    "Rehydrated subagent worktree from snapshot for resume"
                                );
                                Some(path)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    subagent_id = %request.id,
                                    error = %e,
                                    "Failed to rehydrate subagent worktree, falling back to shared workspace"
                                );
                                None
                            }
                        }
                    }
                    ResumeWorktreeAction::Shared => {
                        tracing::warn!(
                            subagent_id = %request.id,
                            worktree = %dest.display(),
                            "Resumed subagent worktree dir missing with no snapshot; using shared workspace"
                        );
                        None
                    }
                }
            }
        }
    } else if effective_runtime.isolation != distill_tool_types::SubagentIsolationMode::None {
        let source_cwd = parent_source_cwd(&ctx);
        let dest = match crate::session::worktree::worktree_base_dir_for_source(&source_cwd) {
            Ok(base) => base.join(format!("subagent-{}", request.id)),
            Err(e) => {
                tracing::warn!(
                    subagent_id = %request.id,
                    error = %e,
                    "Could not resolve worktree base dir, using temp dir for subagent worktree"
                );
                std::env::temp_dir()
                    .join("grok-subagent-worktrees")
                    .join(&request.id)
            }
        };
        let source_clone = source_cwd;
        let subagent_id = request.id.clone();
        let creation_mode: distill_fast_worktree::CreationMode = ctx.worktree_type.into();
        let btrfs_delegate = crate::session::worktree::btrfs_delegate_from_env();
        let (grove_enabled, grove_gate_source) =
            crate::util::config::grove_worktree_gate(ctx.remote_settings.as_ref());
        let worktree_create_span = region!(
            "subagent_spawn.worktree_create",
            Parent::Explicit(spawn_prepare_span.span())
        );
        let create_span = worktree_create_span.span().clone();
        let created = match tokio::task::spawn_blocking(move || {
            create_span.in_scope(|| {
                let mut builder = distill_fast_worktree::WorktreeBuilder::new(&source_clone, &dest)
                    .working_tree_mode(distill_fast_worktree::WorkingTreeMode::PreserveWorkingTree)
                    .creation_mode(creation_mode)
                    .worktree_kind(distill_fast_worktree::WorktreeKind::Subagent)
                    .session_id(subagent_id);
                if let Some(opts) =
                    crate::util::config::grove_worktree_opts_if_enabled(grove_enabled)
                {
                    builder = builder.grove_worktree(opts);
                }
                if let Some(delegate) = btrfs_delegate {
                    builder = builder.btrfs_delegate(delegate);
                }
                builder.create()
            })
        })
        .await
        {
            Ok(Ok(report)) => {
                tracing::info!(
                    subagent_id = %request.id,
                    worktree_path = %report.worktree_path.display(),
                    commit = %report.commit,
                    resolved_strategy = report.resolved_strategy,
                    skipped = %distill_fast_worktree::render_arm_skips(&report.skipped),
                    grove_worktree = grove_enabled,
                    grove_gate_source,
                    "Created isolated worktree for subagent"
                );
                Some(report.worktree_path)
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    subagent_id = %request.id,
                    error = %e,
                    resolved_strategy = "none",
                    skipped = "",
                    grove_worktree = grove_enabled,
                    grove_gate_source,
                    "Failed to create worktree (grove gate {grove_gate_source}), falling back to shared workspace"
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    subagent_id = %request.id,
                    error = %e,
                    resolved_strategy = "none",
                    skipped = "",
                    grove_worktree = grove_enabled,
                    grove_gate_source,
                    "Worktree creation task panicked (grove gate {grove_gate_source}), falling back to shared workspace"
                );
                None
            }
        };
        worktree_create_span.close();
        created
    } else {
        None
    };
    let worktree_freshly_created = resume_source.is_none() && worktree_path.is_some();
    if let Some(root) = &spawn_root {
        let isolation = if worktree_path.is_some() {
            "worktree"
        } else {
            "shared"
        };
        root.record("isolation", isolation);
    }
    if let Some(raw_cwd) = request.cwd.as_deref() {
        match sanitize_cwd_value(raw_cwd) {
            Some(cwd_path) => {
                if worktree_path.is_none() && resume_source.is_none() {
                    let p = Path::new(&cwd_path);
                    if !p.is_dir() {
                        let msg = if p.exists() {
                            format!("cwd \"{cwd_path}\" exists but is not a directory")
                        } else {
                            format!("cwd \"{cwd_path}\" does not exist")
                        };
                        return child_run_output(
                            failure_result(&request, &msg),
                            completion_data,
                            None,
                        );
                    }
                }
                request.cwd = Some(cwd_path);
            }
            None => request.cwd = None,
        }
    }
    if effective_runtime.reasoning_effort.is_some() || effective_runtime.capability_mode.is_some() {
        tracing::info!(
            subagent_id = %request.id,
            reasoning_effort = ?effective_runtime.reasoning_effort,
            capability_mode = ?effective_runtime.capability_mode,
            "Resolved runtime overrides for subagent"
        );
    }
    effective_runtime.capability_mode = distill_subagent_resolution::intersect_capability_modes(
        effective_runtime.capability_mode,
        definition.capability_mode,
    );
    definition.capability_mode = effective_runtime.capability_mode;
    let child_depth = distill_subagent_resolution::resolve_child_depth(
        ctx.parent_depth,
        request.runtime_overrides.spawn_depth,
    );
    let tools_before_policy = definition.tool_config.tools.len();
    let allow_nested_subagents = child_depth < ctx.subagents_max_depth;
    distill_subagent_resolution::apply_child_tool_policy(
        &mut definition,
        effective_runtime.capability_mode,
        allow_nested_subagents,
    );
    if let Some(mode) = effective_runtime.capability_mode {
        tracing::info!(
            subagent_id = %request.id,
            capability_mode = ?mode,
            tools_remaining = definition.tool_config.tools.len(),
            "Applied capability mode filter to agent tool config"
        );
    }
    if !allow_nested_subagents && definition.tool_config.tools.len() < tools_before_policy {
        tracing::info!(
            subagent_id = %request.id,
            child_depth,
            "Stripped task tool from child at max depth"
        );
    }
    if request.owner.is_workflow() {
        definition.tool_config.tools.retain(|tool| {
            !matches!(
                tool.id.rsplit(':').next(),
                Some("scheduler_create" | "scheduler_list" | "scheduler_delete")
            )
        });
    }
    if request.fork_context && !explicit_model_override {
        effective_runtime.model = Some(ctx.model_id.0.to_string());
    }
    let (mut effective_sampling_config, mut effective_model_id) = resolve_effective_model_config(
        effective_runtime.model.as_deref(),
        &request.subagent_type,
        &definition.model,
        &ctx,
    )
    .await;
    let subagent_max_turns = resolve_subagent_max_turns(definition.max_turns, ctx.parent_max_turns);
    {
        let model_str = &effective_sampling_config.model;
        let model_unknown = !model_str.is_empty()
            && !ctx.available_models.is_empty()
            && !ctx.available_models.contains_key(model_str)
            && !ctx
                .available_models
                .values()
                .any(|e| e.info().has_model_id(model_str));
        if model_unknown {
            let (parent_config, parent_mid) = read_parent_sampling_config(&ctx).await;
            tracing::warn!(
                subagent_id = %request.id,
                resolved_model = %model_str,
                parent_model = %parent_config.model,
                "Resolved subagent model not found in available models — \
                 falling back to parent model"
            );
            effective_sampling_config = parent_config;
            effective_model_id = parent_mid;
        }
    }
    if let Some(ref source) = resume_source
        && !explicit_model_override
        && let Some(ref source_model) = source.model_id
        && effective_model_id.0.as_ref() != source_model.as_str()
    {
        if let Some(resolved) = resolve_model_override_to_config(source_model, &ctx) {
            tracing::info!(
                subagent_id = %request.id,
                resolved_model = %effective_model_id.0,
                source_model = source_model,
                "Pinning resumed child to source model"
            );
            effective_sampling_config = resolved.0;
            effective_model_id = resolved.1;
        } else {
            let msg = format!(
                "Cannot resume from subagent '{}': source model '{source_model}' \
                 is no longer available in the model catalogue.",
                source.subagent_id,
            );
            return child_run_output(failure_result(&request, &msg), completion_data, None);
        }
    }
    let caller_effort_override = request.runtime_overrides.reasoning_effort.is_some();
    let caller_manual_effort = request
        .runtime_overrides
        .reasoning_effort
        .as_deref()
        .is_some_and(|raw| !raw.eq_ignore_ascii_case("auto"));
    // Resolve the Jev policy before applying a numeric sampler effort. A
    // durable/forked `auto` policy must not be replaced by a fresh role or
    // definition number; an explicit caller number still wins.
    let child_jev_effort_auto = crate::agent::subagent::resolve_child_jev_effort_auto(
        ctx.parent_effort_auto,
        &request,
        &effective_runtime,
        resume_source.as_ref(),
    );
    let apply_numeric_effort =
        caller_manual_effort || (!caller_effort_override && !child_jev_effort_auto);
    if apply_numeric_effort
        && let Some(raw) = effective_runtime.reasoning_effort.as_deref()
        && ctx
            .models_manager
            .model_supports_reasoning_effort(effective_model_id.0.as_ref())
    {
        match raw.parse::<ReasoningEffort>() {
            Ok(eff) => ctx.models_manager.apply_supported_effort(
                &mut effective_sampling_config,
                Some(eff),
                &acp::SessionId::new(request.id.clone()),
                crate::sampling::EffortTarget::NewSession,
            ),
            Err(err) => {
                tracing::warn!(
                    value = raw,
                    error = %err,
                    "subagent reasoning_effort: parse failed, ignoring override"
                )
            }
        }
    }
    if child_jev_effort_auto && !caller_manual_effort {
        effective_sampling_config.reasoning_effort = None;
    }
    if effective_sampling_config.conversation_group_id.is_none() {
        let inherited_group_id = if let Some(parent_chat_state) = ctx.parent_chat_state.as_ref() {
            parent_chat_state
                .get_sampling_config()
                .await
                .and_then(|config| config.conversation_group_id)
        } else {
            ctx.sampling_config.conversation_group_id.clone()
        };
        effective_sampling_config.conversation_group_id =
            Some(inherited_group_id.unwrap_or_else(|| {
                crate::sampling::derive_conversation_group_id(&ctx.parent_session_id)
            }));
    }
    // Explicit model/effort choices win. A resumed source restores its durable
    // auto/manual policy, while a missing legacy field conservatively stays
    // manual; ordinary forked children inherit the parent's policy.
    let subagent_model_id = effective_sampling_config.model.clone();
    let auto_compact_threshold_percent =
        ctx.resolve_auto_compact_threshold_percent(&subagent_model_id);
    let subagent_id = request.id.clone();
    let child_session_id = acp::SessionId::new(subagent_id.clone());
    let override_cwd = select_override_cwd(resume_source.as_ref(), request.cwd.as_deref());
    let effective_cwd = resolve_child_cwd(worktree_path.as_deref(), override_cwd, &ctx.parent_cwd)
        .to_string_lossy()
        .into_owned();
    let child_session_info = SessionInfo {
        id: child_session_id.clone(),
        cwd: effective_cwd,
    };
    let child_session_dir = session::persistence::session_dir(&child_session_info);
    if let Err(e) = crate::util::distill_home::ensure_sessions_cwd_dir(&child_session_info.cwd) {
        tracing::warn!(?e, "failed to ensure sessions cwd dir for subagent session");
    }
    let parent_session_dir = session::persistence::session_dir(&SessionInfo {
        id: acp::SessionId::new(ctx.parent_session_id.clone()),
        cwd: ctx.parent_cwd.to_string_lossy().to_string(),
    });
    #[cfg(test)]
    let subagent_meta_dir = ctx
        .run_shell_child_harness
        .as_ref()
        .map(|harness| harness.meta_dir.clone())
        .or_else(|| {
            ctx.setup_failure
                .as_ref()
                .map(|failure| failure.meta_dir().to_path_buf())
        })
        .unwrap_or_else(|| parent_session_dir.join("subagents").join(&subagent_id));
    #[cfg(not(test))]
    let subagent_meta_dir = parent_session_dir.join("subagents").join(&subagent_id);
    let context_bootstrap_span = region!(
        "subagent_spawn.context_bootstrap",
        Parent::Explicit(spawn_prepare_span.span())
    );
    let InitialContext {
        source: context_source,
        copy_error: fork_copy_error,
        prefix_len: inherited_prefix_len,
        conversation: forked_conversation,
        force_compact: force_compact_on_first_turn,
        verbatim_fork: context_verbatim_fork,
    } = match bootstrap_initial_context(
        &request,
        resume_source.as_ref(),
        &ctx,
        &child_session_info,
        &child_session_dir,
        effective_model_id.0.as_ref(),
        super::resume_window::ResumeWindowPolicy {
            context_window: effective_sampling_config.context_window,
            auto_compact_threshold_percent,
        },
    )
    .await
    {
        BootstrapInitialContext::Ready(ctx) => ctx,
        BootstrapInitialContext::ResumeAbort(msg) => {
            tracing::error!(
                subagent_id = %request.id,
                error = %msg,
                "Resume-copy failed, aborting subagent spawn"
            );
            return child_run_output(failure_result(&request, &msg), completion_data, None);
        }
    };
    context_bootstrap_span.close();
    let verbatim_mirror_fork =
        context_source == InitialContextSource::Forked && context_verbatim_fork;
    let task_prompt_text = prompt.clone();
    let (mut forked_conversation, mut inherited_prefix_len) =
        (forked_conversation, inherited_prefix_len.unwrap_or(0));
    if context_source != InitialContextSource::Resumed
        && !verbatim_mirror_fork
        && let Some(ref pi) = effective_runtime.persona_instructions
    {
        let reminder = distill_sampling_types::conversation::ConversationItem::system_reminder(
            format!("<system-reminder>\n{pi}\n</system-reminder>"),
        );
        let insert_at = inherited_prefix_len.min(forked_conversation.len());
        forked_conversation.insert(insert_at, reminder);
        inherited_prefix_len += 1;
    }
    let effective_source_str = match &context_source {
        InitialContextSource::New => "new",
        InitialContextSource::Forked => "forked",
        InitialContextSource::Resumed => "resumed",
    };
    let subagent_meta = SubagentMeta {
        subagent_id: subagent_id.clone(),
        attempt_id: attempt_id.clone(),
        parent_session_id: ctx.parent_session_id.clone(),
        child_session_id: child_session_id.0.to_string(),
        subagent_type: request.subagent_type.clone(),
        description: request.description.clone(),
        prompt: request.prompt.clone(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some(effective_source_str.to_string()),
        context_normalized: fork_context_normalized(&context_source, context_verbatim_fork),
        fork_copy_error: fork_copy_error.clone(),
        persona: effective_runtime.persona.clone(),
        resumed_from: request.resume_from.clone(),
        child_cwd: Some(child_session_info.cwd.clone()),
        worktree_path: worktree_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        snapshot_ref: None,
        effective_model_id: Some(effective_model_id.0.to_string()),
        effort_auto: Some(child_jev_effort_auto),
        model_routing_locked: Some(model_routing_locked),
    };
    let mut gcs_upload_ctx = GcsUploadContext {
        bucket_url: ctx.gcs_bucket_url.clone(),
        upload_method: ctx.gcs_upload_method.clone(),
        model_id: Some(effective_model_id.0.to_string()),
        cwd: Some(child_session_info.cwd.clone()),
        reasoning_effort: effective_runtime.reasoning_effort.clone(),
        role_name: effective_runtime.role_name.clone(),
        parent_prompt_id: request.parent_prompt_id.clone(),
        auth_manager: ctx.auth_manager.clone(),
        isolation_mode: Some(format!("{:?}", effective_runtime.isolation)),
        capability_mode: effective_runtime
            .capability_mode
            .as_ref()
            .map(|m| format!("{m:?}")),
        depth: child_depth,
    };
    let advertised_address = distill_message_delivery_core::advertised_address(
        agent_address.as_ref().map(|address| address.as_str()),
        request.owner.is_workflow(),
    )
    .map(str::to_owned);
    let spawned = SessionUpdate::SubagentSpawned {
        subagent_id: subagent_id.clone(),
        attempt_id: attempt_id.clone(),
        child_session_id: child_session_id.0.to_string(),
        parent_session_id: ctx.parent_session_id.clone(),
        parent_prompt_id: request.parent_prompt_id.clone(),
        subagent_type: request.subagent_type.clone(),
        description: request.description.clone(),
        effective_context_source: Some(effective_source_str.to_string()),
        context_normalized: fork_context_normalized(&context_source, context_verbatim_fork),
        capability_mode: effective_runtime
            .capability_mode
            .and_then(|m| serde_json::to_value(m).ok())
            .and_then(|v| v.as_str().map(String::from)),
        persona: effective_runtime.persona.clone(),
        role: effective_runtime.role_name.clone(),
        model: Some(effective_model_id.0.to_string()),
        resumed_from: request.resume_from.clone(),
        workflow_run_id: request.owner.workflow_run_id().map(str::to_string),
        agent_address: advertised_address.clone(),
    };
    let publication_boundary = if is_wake {
        PublicationBoundary::Started
    } else {
        PublicationBoundary::Prepared
    };
    let metadata_parent = (!is_wake).then(|| spawn_prepare_span.span().clone());
    let mut start_artifacts = StartArtifactPublication::new(
        publication_boundary,
        metadata_parent,
        PreparedStartArtifacts {
            meta_dir: subagent_meta_dir.clone(),
            meta: subagent_meta,
            spawned,
            advertised_address,
            reasoning_effort: effective_runtime.reasoning_effort.clone(),
            role_name: effective_runtime.role_name.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
        },
        gateway.clone(),
        reporter.clone(),
        &ctx,
    );
    start_artifacts.publish_at(PublicationBoundary::Prepared);
    if !is_wake {
        completion_data.mark_spawned_notification_emitted();
    }
    let early_gcs_ctx = GcsUploadContext {
        bucket_url: ctx.gcs_bucket_url.clone(),
        upload_method: ctx.gcs_upload_method.clone(),
        model_id: None,
        cwd: None,
        isolation_mode: None,
        capability_mode: None,
        reasoning_effort: effective_runtime.reasoning_effort.clone(),
        role_name: effective_runtime.role_name.clone(),
        parent_prompt_id: request.parent_prompt_id.clone(),
        depth: 0,
        auth_manager: ctx.auth_manager.clone(),
    };
    #[cfg(test)]
    if matches!(
        ctx.setup_failure,
        Some(SubagentSetupFailure::SamplingClient { .. })
    ) {
        return setup_failure_output(
            "Sampling client error: injected test failure",
            &request,
            &child_session_id,
            &subagent_meta_dir,
            &early_gcs_ctx,
            start_artifacts.terminal_persistence_allowed(),
            completion_data,
        );
    }
    let sampling_client = match crate::sampling::Client::new(effective_sampling_config.clone()) {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("Sampling client error: {e}");
            return setup_failure_output(
                &msg,
                &request,
                &child_session_id,
                &subagent_meta_dir,
                &early_gcs_ctx,
                start_artifacts.terminal_persistence_allowed(),
                completion_data,
            );
        }
    };
    #[cfg(test)]
    let persistence_dir = ctx
        .setup_failure
        .as_ref()
        .and_then(SubagentSetupFailure::persistence_dir)
        .map_or_else(|| child_session_dir.clone(), Path::to_path_buf);
    #[cfg(not(test))]
    let persistence_dir = child_session_dir.clone();
    let wake_persistence = if is_wake {
        match session::persistence::read_wake_summary_state_from_dir(persistence_dir.clone()).await
        {
            Ok(prior) => WakePersistenceContext::Wake { prior },
            Err(error) => {
                let msg = format!("Persistence error: {error}");
                return setup_failure_output(
                    &msg,
                    &request,
                    &child_session_id,
                    &subagent_meta_dir,
                    &early_gcs_ctx,
                    start_artifacts.terminal_persistence_allowed(),
                    completion_data,
                );
            }
        }
    } else {
        WakePersistenceContext::Ordinary
    };
    let turn_number = wake_persistence.turn_number(completion_data.turn_number);
    completion_data.turn_number = Some(turn_number);
    let persistence = match session::persistence::new_with_explicit_dir(
        &child_session_info,
        persistence_dir,
        effective_model_id.clone(),
        sampling_client,
        effective_sampling_config.model.clone(),
        if is_wake {
            crate::session::persistence::ExplicitSessionOpen::Wake
        } else {
            crate::session::persistence::ExplicitSessionOpen::New {
                identity: Some(crate::session::persistence::ExplicitSessionIdentity {
                    agent_id: agent_id.clone(),
                    attempt_id: attempt_identity.clone(),
                }),
                next_trace_turn: Some(turn_number.saturating_add(1)),
            }
        },
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            let msg = format!("Persistence error: {e}");
            return setup_failure_output(
                &msg,
                &request,
                &child_session_id,
                &subagent_meta_dir,
                &early_gcs_ctx,
                start_artifacts.terminal_persistence_allowed(),
                completion_data,
            );
        }
    };
    let child_cwd = resolve_child_cwd(worktree_path.as_deref(), override_cwd, &ctx.parent_cwd);
    let covered_by_parent = distill_fsnotify::watch_root_covers(&ctx.parent_cwd, &child_cwd);
    let subagent_fs_watch = FsWatchCapabilities {
        hunk_tracking: ctx.hunk_tracking_enabled && !covered_by_parent,
        ..FsWatchCapabilities::none()
    };
    let child_cwd_abs = distill_paths::AbsPathBuf::new(child_cwd).unwrap_or_else(|_| {
        distill_paths::AbsPathBuf::new(std::env::current_dir().unwrap_or_default())
            .expect("current_dir should be absolute")
    });
    let mut tool_ctx = ToolContext::with_preloaded_env(
        child_cwd_abs,
        Some(gateway.clone()),
        Some(child_session_id.clone()),
        ctx.fs.clone(),
        ctx.terminal.clone(),
        ctx.hunk_tracker_handle.clone(),
        (*ctx.session_env).clone(),
    )
    .with_hunk_tracking_enabled(ctx.hunk_tracking_enabled);
    tool_ctx.subagent_event_tx = Some(ctx.subagent_event_tx.clone());
    let active_agent_messages_enabled = ctx.active_agent_messages_enabled
        && agent_message_sender.is_some()
        && definition
            .capability_mode
            .is_none_or(|mode| mode.allows_tool_kind(ToolKind::ActiveAgentMessage));
    if !active_agent_messages_enabled {
        drop(agent_message_sender.take());
    }
    let task_output_budget = request
        .runtime_overrides
        .output_token_budget
        .map(crate::tools::tool_context::TaskOutputTokenBudget::limited);
    tool_ctx.task_output_token_budget = task_output_budget.clone();
    tool_ctx.sampler_retry_only_before_output = task_output_budget.is_some();
    tool_ctx.monitor_event_buffer = Some(MonitorEventBuffer::default());
    tool_ctx.subagent_depth = child_depth;
    tool_ctx.lsp = ctx.lsp.clone();
    tool_ctx.process_scope = ctx.process_scope.clone();
    let parent_traceparent = distill_otel::current_traceparent();
    let tracker_child_cwd = child_session_info.cwd.clone();
    let tracker_model_id = effective_model_id.0.to_string();
    let initial_child_tokens =
        distill_chat_state::estimate_conversation_tokens(&forked_conversation);
    let model_entry = crate::agent::config::find_model_by_id(
        &ctx.available_models,
        effective_model_id.0.as_ref(),
    );
    let model_has_own_creds = model_entry.is_some_and(|entry| entry.has_own_credentials());
    let inherited_auth_type = subagent_auth_type(model_entry, &ctx.auth_method_id);
    let credentials = distill_chat_state::Credentials {
        api_key: effective_sampling_config.api_key.clone(),
        auth_type: inherited_auth_type,
        alpha_test_key: ctx.alpha_test_key.clone(),
        client_version: effective_sampling_config.client_version.clone(),
    };
    distill_telemetry::unified_log::info(
        "subagent spawn credentials",
        None,
        Some(serde_json::json!({
            "subagent_id": &request.id,
            "subagent_type": &request.subagent_type,
            "effective_model": effective_model_id.0.as_ref(),
            "effective_model_raw": &effective_sampling_config.model,
            "base_url": &effective_sampling_config.base_url,
            "key_prefix": key_prefix(&effective_sampling_config.api_key),
            "auth_type": format!("{:?}", inherited_auth_type),
            "model_has_own_creds": model_has_own_creds,
            "auth_method_id": ctx.auth_method_id.0.as_ref(),
            "parent_model": ctx.model_id.0.as_ref(),
            "parent_key_prefix": key_prefix(&ctx.sampling_config.api_key),
            "context_window": effective_sampling_config.context_window,
        })),
    );
    let attribution_callback: Option<distill_sampler::SharedAttributionCallback> =
        effective_sampling_config.attribution_callback.clone();
    let agent_memory_scope = agent_memory_scope_for_mode(definition.memory, ctx.memory_mode);
    let agent_name_for_memory = definition.name.clone();
    let is_plugin_agent = definition.plugin_name.is_some();
    let yolo_policy_block = distill_workspace::permission::resolution::yolo_disabled_by_policy();
    let agent_permission_mode = resolve_subagent_permission_mode(
        definition.permission_mode.clone(),
        is_plugin_agent,
        yolo_policy_block,
    );
    if agent_permission_mode != definition.permission_mode {
        if is_plugin_agent {
            tracing::warn!(
                agent = %definition.name,
                plugin = ?definition.plugin_name,
                "ignoring permissionMode on plugin agent (not supported for security)"
            );
        } else {
            tracing::warn!(
                agent = %definition.name,
                "ignoring subagent permissionMode=bypassPermissions: always-approve disabled by managed policy"
            );
        }
    }
    if let Some(scope) = agent_memory_scope {
        let memory_tools: Vec<distill_tools::registry::types::ToolConfig> = vec![
            (&distill::ReadFileTool).into(),
            (&distill::SearchReplaceTool).into(),
            (&opencode::OpenCodeWriteTool).into(),
        ];
        for tc in memory_tools {
            if !definition.tool_config.tools.iter().any(|t| t.id == tc.id) {
                definition.tool_config.tools.push(tc);
            }
        }
        let resolved_mem = scope.resolve_dir(&agent_name_for_memory, &ctx.parent_cwd);
        let memory_dir = &resolved_mem.path;
        let memory_md = memory_dir.join("MEMORY.md");
        if memory_md.is_file()
            && let Ok(content) = std::fs::read_to_string(&memory_md)
        {
            const MAX_LINES: usize = 200;
            const MAX_BYTES: usize = 25 * 1024;
            let truncated: String = content
                .lines()
                .take(MAX_LINES)
                .collect::<Vec<_>>()
                .join("\n");
            let truncated =
                distill_tools::util::truncate::truncate_str(&truncated, MAX_BYTES).to_string();
            if !truncated.is_empty() {
                let injection = format!(
                    "\n\n<agent-memory>\nMemory directory: {}\n\n{truncated}\n</agent-memory>",
                    memory_dir.display()
                );
                definition.prompt_body =
                    Some(definition.prompt_body.unwrap_or_default() + injection.as_str());
            }
        }
    }
    let is_plugin_agent = definition.plugin_name.is_some();
    if let Some(ref hooks_config) = definition.hooks {
        if is_plugin_agent {
            tracing::warn!(
                agent = %definition.name,
                plugin = ?definition.plugin_name,
                "ignoring hooks on plugin agent (not supported for security)"
            );
        } else if !crate::agent::folder_trust::agent_inline_hooks_allowed(definition.scope, || {
            crate::agent::folder_trust::project_scope_allowed(&ctx.parent_cwd)
        }) {
            tracing::warn!(
                agent = %definition.name,
                "ignoring hooks on untrusted project agent (folder not trusted; re-run with --trust)"
            );
        } else {
            let hooks_val = hooks_config.as_value();
            let (specs, errors) = distill_hooks::config::parse_hooks_from_value_with_dir(
                &hooks_val,
                &format!(
                    "{}{}",
                    distill_hooks::config::AGENT_HOOK_PREFIX,
                    definition.name
                ),
                &ctx.parent_cwd,
            );
            for e in &errors {
                tracing::warn!(agent = %definition.name, error = ?e, "agent hook parse error");
            }
            if !specs.is_empty() {
                let specs: Vec<_> = specs
                    .into_iter()
                    .map(|mut s| {
                        if s.event == distill_hooks::event::HookEventName::Stop {
                            s.event = distill_hooks::event::HookEventName::SubagentStop;
                        }
                        s
                    })
                    .collect();
                let mut registry = ctx
                    .hook_registry
                    .as_ref()
                    .map(|r| (**r).clone())
                    .unwrap_or_default();
                registry.append_specs(specs);
                ctx.hook_registry = Some(std::sync::Arc::new(registry));
            }
        }
    }
    let agent_mcp_servers: Vec<_> = if definition.mcp_servers.is_empty() {
        vec![]
    } else if is_plugin_agent {
        tracing::warn!(
            agent = %definition.name,
            plugin = ?definition.plugin_name,
            "ignoring mcpServers on plugin agent (not supported for security)"
        );
        vec![]
    } else if !crate::agent::folder_trust::agent_inline_hooks_allowed(definition.scope, || {
        crate::agent::folder_trust::project_scope_allowed(&ctx.parent_cwd)
    }) {
        tracing::warn!(
            agent = %definition.name,
            "ignoring mcpServers on untrusted project agent (folder not trusted; re-run with --trust)"
        );
        vec![]
    } else {
        definition
                .mcp_servers
                .iter()
                .filter_map(|entry| match entry {
                    distill_agent::config::McpServerRef::Named(name) => {
                        ctx.parent_mcp_configs
                            .iter()
                            .find(|s| {
                                crate::session::mcp_servers::mcp_server_name(s) == name
                            })
                            .cloned()
                            .or_else(|| {
                                tracing::warn!(agent = %definition.name, server = name, "mcpServers: named ref not found in parent");
                                None
                            })
                    }
                    distill_agent::config::McpServerRef::Inline { name, config } => {
                        if let serde_json::Value::Object(obj) = config
                            && obj.contains_key("type")
                        {
                            let mut flat = obj.clone();
                            flat.insert(
                                "name".to_string(),
                                serde_json::Value::String(name.clone()),
                            );
                            if let Ok(server) = serde_json::from_value::<
                                agent_client_protocol::McpServer,
                            >(serde_json::Value::Object(flat)) {
                                return Some(server);
                            }
                            tracing::debug!(agent = %definition.name, server = name, "ACP wire format parse failed, trying map-keyed");
                        }
                        if let Some(inner_obj) = config.as_object() {
                            let mut flat = inner_obj.clone();
                            flat.insert(
                                "name".to_string(),
                                serde_json::Value::String(name.clone()),
                            );
                            if let Ok(server) = serde_json::from_value::<
                                agent_client_protocol::McpServer,
                            >(serde_json::Value::Object(flat)) {
                                return Some(server);
                            }
                        }
                        tracing::warn!(agent = %definition.name, server = name, "mcpServers: inline config could not be parsed");
                        None
                    }
                })
                .collect()
    };
    let agent_mcp_servers = crate::session::managed_mcp::filter_policy_blocked_agent_mcp(
        agent_mcp_servers,
        &ctx.parent_cwd,
    );
    let parent_mcp_pool =
        resolve_inherited_mcp_pool(ctx.parent_mcp_pool.take(), &definition.mcp_inheritance);
    let mcp_inherited_count = parent_mcp_pool
        .as_ref()
        .map(|p| p.len() as u32)
        .unwrap_or(0);
    if mcp_inherited_count > 0 {
        tracing::info!(
            subagent_id = %request.id,
            mcp_count = mcp_inherited_count,
            "Subagent inherited MCP servers from parent pool"
        );
    }
    let inherit_skills = definition.inherit_skills;
    let definition_background = definition.background.unwrap_or(false);
    if inherit_skills && ctx.parent_skills.is_none() {
        let parent_cwd_str = ctx.parent_cwd.to_string_lossy().to_string();
        ctx.parent_skills = Some(
            distill_agent::prompt::skills::list_skills_with_plugins(
                Some(&parent_cwd_str),
                &ctx.parent_skills_config,
                ctx.plugin_registry.as_deref(),
                ctx.parent_compat,
                crate::agent::folder_trust::project_scope_allowed(&ctx.parent_cwd),
            )
            .await,
        );
    }
    let skills_inherited_count = if inherit_skills {
        ctx.parent_skills
            .as_ref()
            .map(|s| s.len() as u32)
            .unwrap_or(0)
    } else {
        0
    };
    if skills_inherited_count > 0 {
        tracing::info!(
            subagent_id = %request.id,
            skills_count = skills_inherited_count,
            "Subagent inherited skills from parent"
        );
    }
    let mcp_owned_count = agent_mcp_servers.len() as u32;
    let _active = SUBAGENTS_ACTIVE.enter();
    debug_assert!(
        SUBAGENTS_ACTIVE.get() >= 1,
        "SubagentLaunched must stamp a self-inclusive count"
    );
    distill_telemetry::session_ctx::log_event(distill_telemetry::events::SubagentLaunched {
        subagent_id: request.id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        subagent_type: request.subagent_type.clone(),
        owner: telemetry_owner_kind(&request),
        workflow_run_id: request.owner.workflow_run_id().map(str::to_string),
        queued_ms: queued_for.map(|queued| u64::try_from(queued.as_millis()).unwrap_or(u64::MAX)),
        session_running: u32::try_from(session_running).unwrap_or(u32::MAX),
        persona: request.runtime_overrides.persona.clone(),
        fork_context: matches!(context_source, InitialContextSource::Forked),
        resume_from: request.resume_from.clone(),
        isolated_worktree: worktree_path.is_some(),
        mcp_inherited_count,
        mcp_owned_count,
        skills_inherited_count,
    });
    let wake_model_id = effective_model_id.clone();
    let wake_agent_name = definition.name.clone();
    let wake_reasoning_effort = effective_sampling_config.reasoning_effort;
    let subagent_session_default_agent_profile = Some(definition.name.clone());
    let wake_persistence_tx = is_wake.then(|| persistence.tx.clone());
    if !is_wake {
        let _ = persistence
            .tx
            .send(crate::session::persistence::PersistenceMsg::CurrentModel {
                model_id: effective_model_id.clone(),
                agent_name: Some(definition.name.clone()),
                reasoning_effort: Some(effective_sampling_config.reasoning_effort),
            });
    }
    crate::waterfall::mark(&request.id, crate::waterfall::stage::SESSION_SPAWN);
    spawn_timer.record(SubagentSpawnPhase::SpawnPrepare, start.elapsed());
    spawn_prepare_span.close();
    let session_bootstrap_span = phase_region(SubagentSpawnPhase::SessionBootstrap);
    let spawn_phase_parent = session_bootstrap_span.span().clone();
    let bootstrap_started_at = std::time::Instant::now();
    let pins = ctx.compaction_pins_for_child(&definition.user_message_template);
    let spawn_result = session::spawn_session_on_thread(
        child_session_info,
        gateway.clone(),
        effective_sampling_config,
        credentials,
        crate::agent::auth_method::new_shared_auth_method_id(Some(ctx.auth_method_id.clone())),
        Some(ctx.auth_manager.clone()),
        attribution_callback,
        tool_ctx,
        agent_mcp_servers,
        vec![],
        Default::default(),
        parent_mcp_pool,
        Vec::new(),
        true,
        false,
        None,
        persistence,
        forked_conversation,
        None,
        None,
        initial_child_tokens,
        crate::session::StartupHints {
            inherited_prefix_len: Some(inherited_prefix_len),
            is_subagent: true,
            non_interactive: ctx.parent_non_interactive,
            parent_session_id: Some(ctx.parent_session_id.clone()),
            subagent_type: Some(request.subagent_type.clone()),
            preserve_inherited_system: verbatim_mirror_fork,
            explicit_model_override: model_routing_locked,
            explicit_effort_override,
            ..Default::default()
        },
        distill_workspace::permission::ClientType::Generic,
        auto_compact_threshold_percent,
        distill_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_string(),
        pins.mode,
        ctx.resolve_compaction_verbatim_input(),
        ctx.resolve_compaction_tool_choice(),
        pins.two_pass,
        None,
        None,
        std::sync::Arc::new(parking_lot::Mutex::new(
            distill_workspace::file_system::CodebaseIndexManager::new(),
        )),
        false,
        subagent_fs_watch,
        crate::session::notifications::SessionClientCaps::new(false, true),
        None,
        None,
        None,
        None,
        false,
        false,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        definition,
        subagent_session_default_agent_profile,
        if inherit_skills {
            ctx.parent_skills_config.clone()
        } else {
            distill_agent::prompt::skills::SkillsConfig::default()
        },
        if inherit_skills {
            ctx.parent_skills.take()
        } else {
            None
        },
        ctx.parent_compat,
        ctx.parent_paths_config.clone(),
        false,
        None,
        None,
        None,
        Vec::new(),
        None,
        if verbatim_mirror_fork {
            None
        } else if let Some(scope) = agent_memory_scope {
            ctx.memory_config.as_ref().map(|mc| {
                let mut c = mc.clone();
                let resolved = scope.resolve_dir(&agent_name_for_memory, &ctx.parent_cwd);
                c.enabled = true;
                c.root_dir_override = Some(resolved.path);
                c.flat_memory_root = resolved.is_project_scoped;
                c
            })
        } else {
            ctx.memory_config.clone()
        },
        false,
        Default::default(),
        ctx.managed_mcp_state.clone(),
        ctx.managed_mcp_proxy_base_url.clone(),
        effective_model_id.clone(),
        ctx.yolo_mode
            || matches!(
                agent_permission_mode,
                distill_agent::config::PermissionMode::BypassPermissions
            ),
        false,
        child_jev_effort_auto,
        None,
        ctx.inference_idle_timeout_secs,
        None,
        ctx.resolve_subagent_rate_limit_max_attempts(&subagent_model_id),
        ctx.web_search_sampling_config.clone(),
        ctx.web_fetch_config.clone(),
        ctx.image_gen_config.clone(),
        ctx.video_gen_config.clone(),
        ctx.app_builder_deployer_config.clone(),
        ctx.write_file_enabled,
        active_agent_messages_enabled,
        agent_message_sender,
        ctx.goal_enabled,
        ctx.background_workflows_enabled,
        true,
        ctx.subagents_max_depth,
        ctx.workflow_max_concurrent_agents,
        ctx.media_gen_batch_limits,
        ctx.ask_user_question_enabled,
        ctx.client_hooks.clone(),
        None,
        std::collections::HashMap::new(),
        Vec::new(),
        distill_agent::prompt::context::PromptAudience::Subagent,
        effective_runtime.role_prompt.clone(),
        None,
        ctx.disable_web_search,
        ctx.backend_tools_enabled,
        ctx.respect_gitignore,
        ctx.path_not_found_hints,
        std::mem::take(&mut ctx.tool_params_json),
        crate::session::session_create_prefetch::SessionCreatePrefetch::ready(
            ctx.plugin_registry.clone(),
        ),
        None,
        ctx.models_manager.clone(),
        parent_traceparent,
        ctx.permission_handle.clone(),
        ctx.api_key_provider.clone(),
        ctx.image_description_model.clone(),
        ctx.hook_registry.clone(),
        ctx.workspace_ops.clone(),
        vec![],
        ctx.todo_gate,
        ctx.remote_settings.clone(),
        std::mem::take(&mut ctx.laziness_debug_log),
        ctx.parent_terminal_backend.clone(),
        if request.owner.is_workflow() {
            None
        } else {
            ctx.parent_scheduler_handle.clone()
        },
        subagent_max_turns,
        if verbatim_mirror_fork && !request.owner.is_workflow() {
            std::mem::take(&mut ctx.parent_tool_definitions)
        } else {
            None
        },
        false,
        Some(distill_telemetry::subagent_spawn::SpawnPhaseContext {
            timer: spawn_timer.clone(),
            parent: spawn_phase_parent,
        }),
        Some(ctx.subagent_sampling_semaphore.clone()),
        None,
    )
    .await;
    crate::waterfall::mark(&request.id, crate::waterfall::stage::SESSION_UP);
    spawn_timer.record(
        SubagentSpawnPhase::SessionBootstrap,
        bootstrap_started_at.elapsed(),
    );
    session_bootstrap_span.close();
    let session_ready_at = std::time::Instant::now();
    let (child_init, child_thread) = match spawn_result {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Failed to spawn child session: {e}");
            let result = if completion_data.has_emitted_spawned_notification() {
                fail_subagent(
                    &msg,
                    &subagent_id,
                    &child_session_id,
                    &subagent_meta_dir,
                    start.elapsed().as_millis() as u64,
                    &gcs_upload_ctx,
                )
            } else {
                failure_result(&request, &msg)
            };
            return child_run_output(result, completion_data, None);
        }
    };
    let session::SessionInitResult {
        handle: child_handle,
        permission_events_rx: mut permission_rx,
        toolset: child_toolset,
        ..
    } = child_init;
    session::bind_installed_toolset(
        &ctx.workspace_ops,
        &child_handle.info.id,
        child_handle.tool_context.cwd.as_path(),
        &child_handle.hunk_tracker_handle,
        &child_toolset,
    );
    let ready_to_first_turn_span = phase_region(SubagentSpawnPhase::ReadyToFirstTurn);
    let (receipt_sink, receipt_stream) = mpsc::channel(ACTIVE_MESSAGE_RECEIPT_CAPACITY);
    let receipt_drain = PromptTurnReceiptDrain::start(
        receipt_stream,
        child_handle.cmd_tx.clone(),
        cancel_token.clone(),
    );
    let (prompt_admitted, prompt_admitted_rx) = oneshot::channel();
    super::resume_window::arm_force_compact(
        &child_handle.force_compact,
        force_compact_on_first_turn,
    );
    let child_prompt_id = wake_message_id.unwrap_or_else(|| match wake_message_source {
        Some(ActiveAgentMessageSource::Agent) => {
            format!("parent-agent-message-{}", uuid::Uuid::now_v7())
        }
        Some(ActiveAgentMessageSource::Human) => {
            format!("parent-message-{}", uuid::Uuid::now_v7())
        }
        None => uuid::Uuid::now_v7().to_string(),
    });
    let mut attempt = Box::pin(run_one_turn_attempt(OneTurnAttemptInput {
        child_handle: &child_handle,
        request: &request,
        worktree_path: worktree_path.as_deref(),
        task_prompt_text: &task_prompt_text,
        prompt_id: child_prompt_id,
        inherited_tool_overrides: ctx.inherited_tool_overrides.clone(),
        gcs_bucket_url: ctx.gcs_bucket_url.as_deref(),
        gcs_upload_method: ctx.gcs_upload_method.as_ref(),
        turn_number,
        cancel_token: cancel_token.clone(),
        child_run_started_at: start,
        prompt_admitted,
        #[cfg(test)]
        initial_attempt_behavior: ctx
            .run_shell_child_harness
            .as_ref()
            .map_or(InitialAttemptBehavior::Normal, |harness| {
                harness.initial_attempt_behavior
            }),
    }));
    let readiness = wait_initial_child_prompt_readiness(
        cancel_token.cancelled(),
        prompt_admitted_rx,
        &mut attempt,
        INITIAL_PROMPT_ADMISSION_TIMEOUT,
    )
    .await;
    let unpromoted_disposition = readiness.unpromoted_disposition();
    let mut completed_before_ack = None;
    let mut restore_wake_after_teardown = false;
    let promoted = match readiness {
        InitialChildPromptReadiness::Admitted(release) => {
            let child = StartedChild {
                child_session_id: child_session_id.0.to_string(),
                persona: effective_runtime.persona.clone(),
                resumed_from: request.resume_from.clone(),
                child_cwd: tracker_child_cwd,
                worktree_path: worktree_path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                effective_model_id: tracker_model_id.clone(),
                definition_background,
                control: ShellChildRuntime {
                    child_cmd_tx: child_handle.cmd_tx.clone(),
                    message_delivery: child_handle.message_delivery(),
                    active_message_target_session_id: child_session_id.0.to_string(),
                    active_message_target_agent_id: agent_id.clone(),
                    active_message_target_generation: target_generation,
                    child_signals: child_handle.signals_handle.clone(),
                    _child_thread: None,
                    receipt_sink,
                    #[cfg(test)]
                    force_queue_envelope: false,
                    active_message_parent_session_id: ctx.parent_session_id.clone(),
                    active_message_parent_prompt_index: ctx
                        .active_message_parent_prompt_index
                        .clone(),
                },
            };
            let promoted = if is_wake {
                reporter.started_deferred(child).await
            } else {
                reporter.started(child).await
            };
            if !promoted {
                drop(release);
                false
            } else {
                let wake_start_abort = tokio_util::sync::CancellationToken::new();
                #[cfg(test)]
                let hold_wake_start_flush_ack = ctx
                    .run_shell_child_harness
                    .as_ref()
                    .is_some_and(|harness| harness.hold_wake_start_flush_ack);
                #[cfg(not(test))]
                let hold_wake_start_flush_ack = false;
                let persisted = match (wake_persistence_tx.as_ref(), &wake_persistence) {
                    (Some(wake_persistence_tx), WakePersistenceContext::Wake { prior }) => {
                        let (respond_to, response_rx) = oneshot::channel();
                        if wake_persistence_tx
                            .send(crate::session::persistence::PersistenceMsg::WakeStart {
                                start: crate::session::persistence::WakeStart {
                                    prior: prior.clone(),
                                    attempt_id: attempt_identity.to_string(),
                                    next_trace_turn: turn_number.saturating_add(1),
                                    model_id: wake_model_id.clone(),
                                    agent_name: Some(wake_agent_name.clone()),
                                    reasoning_effort: Some(wake_reasoning_effort),
                                },
                                abort: wake_start_abort.clone(),
                                respond_to,
                            })
                            .is_err()
                        {
                            false
                        } else {
                            let response = async move {
                                if hold_wake_start_flush_ack {
                                    std::future::pending().await
                                } else {
                                    response_rx.await
                                }
                            };
                            matches!(
                                tokio::time::timeout(WAKE_START_FLUSH_TIMEOUT, response).await,
                                Ok(Ok(Ok(())))
                            )
                        }
                    }
                    (None, WakePersistenceContext::Ordinary) => true,
                    _ => false,
                };
                if !persisted {
                    tracing::warn!(
                        subagent_id = %request.id,
                        turn_number,
                        "accepted wake trace turn did not persist before the deadline"
                    );
                    wake_start_abort.cancel();
                    #[cfg(test)]
                    let hold_wake_abort_flush_ack = ctx
                        .run_shell_child_harness
                        .as_ref()
                        .is_some_and(|harness| harness.hold_wake_abort_flush_ack);
                    #[cfg(not(test))]
                    let hold_wake_abort_flush_ack = false;
                    if let Some(wake_persistence_tx) = wake_persistence_tx.as_ref() {
                        wake_persistence
                            .abort(
                                wake_persistence_tx,
                                hold_wake_abort_flush_ack,
                                &request.id,
                                turn_number,
                            )
                            .await;
                    }
                    if is_wake {
                        let _ = reporter.settle_deferred_start(false).await;
                    }
                    drop(release);
                    false
                } else {
                    #[cfg(test)]
                    let reject_deferred_start_commit = ctx
                        .run_shell_child_harness
                        .as_ref()
                        .is_some_and(|harness| harness.reject_deferred_start_commit);
                    #[cfg(not(test))]
                    let reject_deferred_start_commit = false;
                    let is_committed = !is_wake
                        || (!reject_deferred_start_commit
                            && reporter.settle_deferred_start(true).await);
                    if !is_committed {
                        drop(release);
                        restore_wake_after_teardown = true;
                        wake_start_abort.cancel();
                        let _ = reporter.settle_deferred_start(false).await;
                        start_artifacts.forbid_terminal_persistence();
                        false
                    } else {
                        if start_artifacts.publish_at(PublicationBoundary::Started) {
                            completion_data.mark_spawned_notification_emitted();
                        }
                        let _ = release.send(());
                        true
                    }
                }
            }
        }
        InitialChildPromptReadiness::AttemptCompleted(outcome) => {
            drop(receipt_sink);
            completed_before_ack = Some(outcome);
            false
        }
        InitialChildPromptReadiness::Cancelled | InitialChildPromptReadiness::TimedOut => {
            drop(receipt_sink);
            false
        }
    };
    if !promoted && completed_before_ack.is_none() {
        ready_to_first_turn_span.close();
        drop(spawn_root.take());
        let result = cancel_pending_shell_child(
            &child_handle.cmd_tx,
            child_thread,
            &ctx.workspace_ops,
            &subagent_id,
            &child_session_id,
            &subagent_meta_dir,
            worktree_path.as_deref(),
            worktree_freshly_created,
            start.elapsed().as_millis() as u64,
            &gcs_upload_ctx,
            UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT,
            unpromoted_disposition,
            start_artifacts.terminal_persistence_allowed(),
        )
        .await;
        if restore_wake_after_teardown {
            if let Some(wake_persistence_tx) = wake_persistence_tx.as_ref() {
                wake_persistence
                    .abort(wake_persistence_tx, false, &request.id, turn_number)
                    .await;
            }
            let _ = reporter.settle_deferred_start(false).await;
        }
        return child_run_output(result, completion_data, None);
    }
    let _progress_publisher = spawn_progress_publisher(
        child_handle.signals_handle.clone(),
        gateway.clone(),
        ctx.parent_session_id.clone(),
        request.id.clone(),
        attempt_id,
        child_session_id.0.to_string(),
        start,
        cancel_token.clone(),
        goal_tick_cmd_tx(ctx.goal_enabled, ctx.parent_cmd_tx.as_ref()),
    );
    spawn_timer.record(
        SubagentSpawnPhase::ReadyToFirstTurn,
        session_ready_at.elapsed(),
    );
    ready_to_first_turn_span.close();
    drop(spawn_root.take());
    let attempt_outcome = match completed_before_ack {
        Some(outcome) => outcome,
        None => attempt.as_mut().await,
    };
    drop(attempt);
    crate::waterfall::mark(&request.id, crate::waterfall::stage::TURN_DONE);
    let OneTurnAttemptOutcome {
        mut result,
        trace,
        mut cancellation_may_hide_usage,
    } = attempt_outcome;
    let OneTurnTraceCapture {
        before_copy_rx,
        child_prompt_id,
        turn_started_at,
        turn_token_totals,
        turn_number,
    } = trace;
    let admission = if promoted && !reporter.finalizing().await {
        AdmissionSettlement::Uncertain
    } else {
        AdmissionSettlement::Settled
    };
    let receipt_settlement = receipt_drain.settle(admission).await;
    let receipt_disposition = receipt_settlement.disposition;
    let mut final_prompt_id = child_prompt_id;
    let mut final_turn_tokens = turn_token_totals;
    let mut final_receipt = None;
    let mut final_receipt_telemetry = None;
    if let Some(FinalPromptTurnReceipt {
        prompt_id,
        outcome,
        telemetry,
    }) = receipt_settlement.final_receipt
    {
        final_prompt_id = prompt_id;
        final_receipt_telemetry = Some(telemetry);
        if let PromptTurnReceiptOutcome::Settled(receipt) = outcome {
            final_receipt = Some(*receipt);
        }
    }
    if let Some(Ok(Ok(crate::session::commands::PromptTurnOk {
        turn_snapshot: Some(snapshot),
        ..
    }))) = final_receipt.as_ref()
    {
        final_turn_tokens = Some((
            snapshot.turn_input_tokens,
            snapshot.turn_cached_input_tokens,
            snapshot.turn_output_tokens,
        ));
    }
    let child_stop_reason: Option<acp::StopReason> = final_receipt
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .and_then(|r| r.as_ref().ok())
        .map(|ok| ok.stop_reason);
    let final_text = if final_receipt.is_some() {
        child_actor_query(
            "trailing_assistant_report",
            child_handle
                .chat_state_handle
                .get_trailing_assistant_report(),
            None,
        )
        .await
        .unwrap_or_default()
    } else {
        String::new()
    };
    let folded_settlement = reduce_prompt_turn_settlement(PromptTurnSettlementInput {
        result,
        disposition: receipt_disposition,
        final_receipt,
        final_text,
        was_cancelled: cancel_token.is_cancelled(),
    });
    result = folded_settlement.result;
    cancellation_may_hide_usage |= folded_settlement.cancellation_may_hide_usage;
    crate::session::telemetry::record_settlement(
        final_receipt_telemetry,
        folded_settlement.settlement_status,
    )
    .await;
    let trace_token_totals = child_actor_query(
        "session_usage",
        child_handle.chat_state_handle.try_get_session_usage(),
        Err(()),
    )
    .await
    .ok()
    .map(|usage| usage.totals);
    result.tokens_used = trace_token_totals
        .as_ref()
        .map(distill_chat_state::UsageTotals::total_tokens)
        .unwrap_or(0);
    let (tool_calls, turns) = signals_snapshot_counts(&child_handle)
        .await
        .unwrap_or((0, 0));
    result.tool_calls = tool_calls;
    result.turns = turns;
    result.duration_ms = start.elapsed().as_millis() as u64;
    let (model_tx, model_rx) = oneshot::channel();
    let final_model = if child_handle
        .cmd_tx
        .send(SessionCommand::GetCurrentModel {
            responds_to: model_tx,
        })
        .is_ok()
    {
        child_actor_query("final_model_policy", async { model_rx.await.ok() }, None).await
    } else {
        None
    };
    let final_model_id = final_model
        .as_ref()
        .and_then(|model| model.canonical_id.clone())
        .or_else(|| Some(effective_model_id.0.to_string()));
    gcs_upload_ctx.model_id = final_model_id.clone();
    let final_model_routing_locked = final_model
        .as_ref()
        .map(|model| model.model_routing_locked)
        .unwrap_or(model_routing_locked);
    let final_effort_auto = child_handle
        .jev_effort_auto
        .load(std::sync::atomic::Ordering::Relaxed);
    if let Some(trace_gcs_config) = gcs_upload_ctx.upload_method.as_ref().map(|method| {
        crate::session::repo_changes::TraceExportConfig {
            bucket_url: gcs_upload_ctx.bucket_url.clone(),
            service_account_key: None,
            prefix_dir: None,
            gcs_prefix: Some(super::attempt_runner::subagent_trace_prefix(
                child_session_id.0.as_ref(),
                turn_number,
            )),
            absolute_paths: false,
            archive_name_override: None,
            upload_method: method.clone(),
        }
    }) {
        let upload_deadline =
            tokio::time::Instant::now() + crate::util::config::load_upload_wait_config_sync().1;
        let upload_wait = crate::upload::turn::UploadWait::Defer {
            deadline: upload_deadline,
        };
        let (copy_tx, session_copy_rx) = tokio::sync::oneshot::channel();
        let _ = child_handle.cmd_tx.send(SessionCommand::CopyFile {
            respond_to: copy_tx,
        });
        let turn_messages = take_child_turn_messages(&child_handle.cmd_tx, upload_deadline).await;
        let child_committed = child_stop_reason
            .map(crate::upload::turn::stop_reason_commits_turn)
            .unwrap_or(result.success);
        let streaming_partial = take_child_streaming_partial(
            &child_handle.cmd_tx,
            upload_deadline,
            final_prompt_id.clone(),
            child_committed,
            gcs_upload_ctx.model_id.clone(),
        )
        .await
        .map(|mut cap| {
            cap.reason = Some(if result.cancelled {
                "subagent_cancel".to_string()
            } else {
                "subagent_non_completed".to_string()
            });
            cap
        });
        let mut permission_events = Vec::new();
        while let Ok(event) = permission_rx.try_recv() {
            permission_events.push(event);
        }
        let trace_ctx = PromptTraceContext {
            gcs_config: trace_gcs_config,
            session_info: child_handle.info.clone(),
            turn_number,
            attempt_id: Some(attempt_identity.to_string()),
            memory_mode: child_handle.spawn_snapshot.memory_mode,
            session_handle: child_handle.clone(),
            session_registry_enabled: false,
            upload_queue: None,
            artifact_tracker: crate::upload::manifest::new_artifact_tracker(),
            auth_manager: ctx.auth_manager.clone(),
        };
        let session_dir = crate::session::persistence::session_dir(&child_handle.info);
        if let Ok(prompt_bytes) = std::fs::read(session_dir.join("system_prompt.txt")) {
            let gcs_path = format!("{}/system_prompt.txt", child_session_id.0);
            crate::upload::trace::upload_small_artifact(
                &trace_ctx,
                &prompt_bytes,
                &gcs_path,
                "text/plain",
                "system_prompt",
                upload_wait,
            )
            .await;
        }
        if let Ok(ctx_bytes) = std::fs::read(session_dir.join("prompt_context.json")) {
            let gcs_path = format!("{}/prompt_context.json", child_session_id.0);
            crate::upload::trace::upload_small_artifact(
                &trace_ctx,
                &ctx_bytes,
                &gcs_path,
                "application/json",
                "prompt_context",
                upload_wait,
            )
            .await;
        }
        upload_session_state(&trace_ctx, "before", before_copy_rx, upload_wait).await;
        let subagent_auth = ctx.auth_manager.current();
        let metadata = PromptMetadata::new(PromptMetadataParams {
            schema_version: GCS_SCHEMA_VERSION.to_string(),
            session_id: child_session_id.0.to_string(),
            turn_number,
            request_id: final_prompt_id.clone(),
            attempt_id: trace_ctx.attempt_id.clone(),
            turn_started_at: turn_started_at.clone(),
            user_id: subagent_auth.as_ref().map(|a| a.user_id.clone()),
            user_email: subagent_auth.as_ref().and_then(|a| a.email.clone()),
            team_id: subagent_auth.as_ref().and_then(|a| a.team_id.clone()),
            client_source: Some("subagent".to_string()),
            client_version: ctx.sampling_config.client_version.clone(),
            model: gcs_upload_ctx.model_id.clone().unwrap_or_default(),
            reasoning_effort: child_handle
                .reasoning_effort
                .map(|e| e.as_ref().to_string()),
            host_os: std::env::consts::OS.to_string(),
            host_arch: std::env::consts::ARCH.to_string(),
            prompt_has_image: Some(false),
            prompt_was_truncated: Some(false),
            prompt_verbatim: Some(true),
            cwd: Some(child_handle.info.cwd.clone()),
            agent_type: Some(request.subagent_type.clone()),
            shell_version: Some(distill_version::VERSION.to_string()),
            sandbox: local_sandbox_telemetry(),
            ..Default::default()
        });
        upload_metadata(&trace_ctx, metadata, upload_wait).await;
        let resolved_model = resolve_child_model(
            &child_handle.cmd_tx,
            upload_deadline,
            gcs_upload_ctx.model_id.clone(),
        )
        .await;
        let turn_result_meta = TurnResultMetadata {
            schema_version: "1",
            request_id: final_prompt_id,
            completed: child_committed,
            stop_reason: child_stop_reason.map(|sr| format!("{sr:?}")),
            total_tokens: trace_token_totals
                .as_ref()
                .map(distill_chat_state::UsageTotals::total_tokens),
            input_tokens: final_turn_tokens.map(|tokens| tokens.0),
            cached_input_tokens: final_turn_tokens.map(|tokens| tokens.1),
            output_tokens: final_turn_tokens.map(|tokens| tokens.2),
            error: result.error.clone(),
            finished_at: chrono::Utc::now().to_rfc3339(),
            signals: None,
            turn_delta: None,
            start_prompt_mode: Some(crate::session::plan_mode::PromptMode::Agent.to_string()),
            end_prompt_mode: Some(crate::session::plan_mode::PromptMode::Agent.to_string()),
            resolved_model,
            subagents_spawned: vec![],
        };
        upload_turn_result(&trace_ctx, &turn_result_meta, upload_wait).await;
        match complete_prompt_trace(
            trace_ctx,
            permission_events,
            session_copy_rx,
            turn_messages,
            streaming_partial,
            upload_wait,
        )
        .await
        {
            Ok(_) => {
                tracing::debug!(
                    subagent_id = %request.id,
                    child_session_id = %child_session_id.0,
                    "Subagent trace artifacts uploaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    subagent_id = %request.id,
                    error = %e,
                    "Subagent trace upload failed (non-fatal)"
                );
            }
        }
    }
    let terminal_persistence_allowed = start_artifacts.terminal_persistence_allowed();
    if terminal_persistence_allowed {
        completion_data
            .set_persisted_output_dir(persist_subagent_output(&subagent_meta_dir, &result));
        persist_subagent_completion(
            &subagent_meta_dir,
            &result,
            &gcs_upload_ctx,
            final_model_id.as_deref(),
            Some(final_effort_auto),
            Some(final_model_routing_locked),
        );
    }
    let final_status = result.status().to_string();
    let snapshot_dispose_enabled =
        terminal_persistence_allowed && ctx.resolve_subagent_worktree_snapshot_enabled();
    let telemetry_tokens = if result.tool_calls > 0 || result.success {
        child_actor_query(
            "total_tokens",
            child_handle.chat_state_handle.get_total_tokens(),
            0,
        )
        .await
    } else {
        0
    };
    completion_data.set_telemetry_tokens(telemetry_tokens);
    let fold_acked = capture_and_fold_one_turn_usage(
        &mut result,
        OneTurnUsageInput {
            child_handle: &child_handle,
            task_budget_usage: task_output_budget.as_ref().map(|budget| budget.usage()),
            cancellation_may_hide_usage,
            parent_cmd_tx: ctx.parent_cmd_tx.as_ref(),
            parent_prompt_id: request.parent_prompt_id.as_deref(),
        },
    )
    .await;
    if !fold_acked {
        tracing::warn!(
            subagent_id = %request.id,
            parent_prompt_id = ?request.parent_prompt_id,
            "subagent usage fold unacked; marking parent bill incomplete via fallback"
        );
        mark_child_usage_not_applied_with_fallback(
            ctx.parent_cmd_tx.as_ref(),
            &ctx.subagent_event_tx,
            &ctx.parent_session_id,
            request.parent_prompt_id.clone(),
        )
        .await;
    }
    let outcome = if result.success {
        distill_telemetry::events::Outcome::Completed
    } else if result.cancelled {
        distill_telemetry::events::Outcome::Cancelled
    } else {
        distill_telemetry::events::Outcome::Error
    };
    let mut completed = distill_telemetry::events::SubagentCompleted {
        subagent_id: request.id.clone(),
        parent_session_id: request.parent_session_id.clone(),
        owner: telemetry_owner_kind(&request),
        workflow_run_id: request.owner.workflow_run_id().map(str::to_string),
        outcome,
        duration_ms: result.duration_ms,
        tool_calls: result.tool_calls,
        tokens_used: if telemetry_tokens > 0 {
            Some(telemetry_tokens)
        } else {
            None
        },
        queue_wait_ms: None,
        spawn_prepare_ms: None,
        session_bootstrap_ms: None,
        agent_build_ms: None,
        tool_setup_ms: None,
        ready_to_first_turn_ms: None,
    };
    spawn_timer.write_event_phases(&mut completed);
    distill_telemetry::session_ctx::log_event(completed);
    match (
        &ctx.parent_terminal_backend,
        &ctx.parent_notification_handle,
    ) {
        (Some(parent_tb), Some(parent_notif_handle)) => {
            reparent_surviving_child_tasks(
                parent_tb,
                parent_notif_handle,
                ctx.parent_cmd_tx.as_ref(),
                &child_session_id.0,
                &ctx.parent_session_id,
                request.surface_completion,
            )
            .await;
        }
        (Some(_), None) | (None, Some(_)) => {
            tracing::warn!(
                child_session_id = %child_session_id.0,
                parent_session_id = %ctx.parent_session_id,
                has_terminal_backend = ctx.parent_terminal_backend.is_some(),
                has_notification_handle = ctx.parent_notification_handle.is_some(),
                "skipping reparent_notifications: parent_terminal_backend and \
                 parent_notification_handle must both be Some"
            );
        }
        (None, None) => {}
    }
    {
        let (respond_to, ack) = oneshot::channel();
        if child_handle
            .cmd_tx
            .send(SessionCommand::FlushComplete { respond_to })
            .is_ok()
        {
            match tokio::time::timeout(CHILD_COMPLETION_FLUSH_TIMEOUT, ack).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    tracing::warn!(
                        subagent_id = %request.id,
                        %error,
                        "child transcript flush failed before completion"
                    )
                }
                Ok(Err(_)) => {
                    tracing::warn!(
                        subagent_id = %request.id,
                        "child session dropped the transcript flush ack before completion"
                    )
                }
                Err(_) => {
                    tracing::warn!(
                        subagent_id = %request.id,
                        "child transcript flush timed out before completion"
                    )
                }
            }
        }
    }
    crate::waterfall::mark(&request.id, crate::waterfall::stage::FLUSH_DONE);
    let _ = child_handle.cmd_tx.send(SessionCommand::Shutdown(
        crate::session::ShutdownKind::Graceful,
    ));
    drop(child_handle);
    if !await_session_thread_exit(&child_thread, UNPROMOTED_SESSION_THREAD_EXIT_TIMEOUT).await {
        tracing::warn!(
            subagent_id = %request.id,
            child_session_id = %child_session_id.0,
            "completed child actor did not exit before the teardown bound"
        );
    }
    ctx.workspace_ops
        .end_local_session(child_session_id.0.as_ref());
    let mut disposed_snapshot_ref: Option<String> = None;
    let mut worktree_removed = false;
    if let Some(ref wt_path) = worktree_path {
        if snapshot_dispose_enabled {
            let disposal = dispose_worktree_after_completion(
                wt_path,
                &resolve_subagent_source_repo(&ctx),
                &subagent_meta_dir,
                &final_status,
                &request.id,
            )
            .await;
            worktree_removed = disposal.worktree_removed();
            disposed_snapshot_ref = disposal.snapshot_ref().map(str::to_owned);
        } else {
            tracing::info!(
                subagent_id = %request.id,
                worktree_path = %wt_path.display(),
                "Worktree preserved for review"
            );
        }
    }
    if worktree_removed {
        result.worktree_path = None;
    }
    let success = result.success && !result.cancelled;
    let preview = crate::util::truncate(&result.output, 200);
    let level_fn = if success {
        distill_telemetry::unified_log::info
    } else {
        distill_telemetry::unified_log::error
    };
    level_fn(
        if success {
            "subagent completed"
        } else {
            "subagent failed"
        },
        None,
        Some(serde_json::json!({
            "subagent_id": &request.id,
            "subagent_type": &request.subagent_type,
            "effective_model": tracker_model_id,
            "success": success,
            "cancelled": result.cancelled,
            "duration_ms": result.duration_ms,
            "turns": result.turns,
            "tool_calls": result.tool_calls,
            "output_preview": preview,
            "error": &result.error,
        })),
    );
    crate::waterfall::mark(&request.id, crate::waterfall::stage::CHILD_DONE);
    child_run_output(result, completion_data, disposed_snapshot_ref)
}
pub(crate) enum Disposal {
    Kept,
    Snapshotted {
        snapshot_ref: String,
        worktree_removed: bool,
    },
}
impl Disposal {
    pub(crate) fn worktree_removed(&self) -> bool {
        matches!(
            self,
            Disposal::Snapshotted {
                worktree_removed: true,
                ..
            }
        )
    }
    pub(crate) fn snapshot_ref(&self) -> Option<&str> {
        match self {
            Disposal::Kept => None,
            Disposal::Snapshotted { snapshot_ref, .. } => Some(snapshot_ref),
        }
    }
}
#[tracing::instrument(skip_all)]
pub(crate) async fn dispose_worktree_after_completion(
    worktree: &std::path::Path,
    source_repo: &std::path::Path,
    meta_dir: &std::path::Path,
    final_status: &str,
    subagent_id: &str,
) -> Disposal {
    let ref_name = format!("refs/grok/subagents/{subagent_id}");
    let snapshot_ref = match crate::session::worktree::snapshot_subagent_worktree(
        worktree,
        source_repo,
        &ref_name,
    )
    .await
    {
        Ok(snapshot_ref) => snapshot_ref,
        Err(e) => {
            tracing::warn!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                error = %e,
                "Failed to snapshot subagent worktree; preserving for review"
            );
            return Disposal::Kept;
        }
    };
    let checked_path = worktree.to_path_buf();
    let checked_source_repo = source_repo.to_path_buf();
    let checked_snapshot = snapshot_ref.clone();
    let reclaim_span = region!("worktree.reclaim_check", Parent::Inherit);
    let reclaim = tokio::task::spawn_blocking(move || {
        distill_fast_worktree::reclaimable_after_snapshot(
            &checked_path,
            Some(&checked_source_repo),
            &checked_snapshot,
        )
    })
    .await;
    reclaim_span.close();
    match reclaim {
        Ok(distill_fast_worktree::Reclaim::Now { .. }) => {}
        Ok(distill_fast_worktree::Reclaim::Keep(reason)) => {
            tracing::info!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                %reason,
                "subagent worktree kept: removal would not preserve every byte"
            );
            return Disposal::Kept;
        }
        Ok(distill_fast_worktree::Reclaim::Unnamed(error)) => {
            tracing::warn!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                %error,
                "the discarded commits were not named; preserving the worktree"
            );
            return Disposal::Kept;
        }
        Err(error) => {
            tracing::warn!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                %error,
                "the safety check did not finish; preserving the worktree"
            );
            return Disposal::Kept;
        }
    }
    if !update_subagent_meta_snapshot_ref(meta_dir, &snapshot_ref, final_status) {
        tracing::warn!(
            subagent_id = %subagent_id,
            worktree_path = %worktree.display(),
            "snapshot_ref not persisted; preserving worktree for resume"
        );
        return Disposal::Kept;
    }
    let worktree_removed = match crate::session::worktree::remove_subagent_worktree(worktree).await
    {
        Ok(()) => {
            tracing::info!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                "snapshotted and removed subagent worktree"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                subagent_id = %subagent_id,
                worktree_path = %worktree.display(),
                error = %e,
                "snapshotted subagent worktree but removal failed; ref persisted for resume"
            );
            false
        }
    };
    Disposal::Snapshotted {
        snapshot_ref,
        worktree_removed,
    }
}
