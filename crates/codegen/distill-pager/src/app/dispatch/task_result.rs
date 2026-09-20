// Modified for Distill by Samuel Fajreldines, 2026.
//! Async task-result application: routes task results into state.
use super::auth::{
    ensure_login_method, handle_auth_complete, handle_auth_url_ready, handle_mcp_auth_trigger_done,
    handle_mcp_setup_submit_done,
};
use super::billing::{
    PAYWALL_AUTO_CHECK_TIMEOUT, apply_auto_topup, handle_billing_fetched,
    handle_check_subscription_complete, handle_credit_limit_recheck_complete,
    handle_gate_refreshed, handle_gate_verify_timeout,
};
use super::cta::{
    handle_cta_plugin_install_done, handle_cta_plugin_reload_done,
    handle_plugin_cta_catalog_loaded, handle_plugin_cta_debounce_expired,
    handle_plugin_cta_mcps_loaded,
};
use super::ctx::{find_agent_by_session_id, get_active_agent_mut};
use super::notes::{handle_btw_response, handle_memory_note_saved};
use super::prompt::{
    defer_to_open_reload_window, handle_compact_complete, handle_memory_command_complete,
    handle_prompt_response, handle_suggestion_debounce_expired,
};
use super::queue::push_and_page_flip;
use super::rewind::{
    dispatch_rewind_success, handle_rewind_execute_failed, handle_rewind_points_loaded,
};
use super::router::{dispatch, dispatch_action_result};
use super::session::foreign::{
    handle_foreign_sessions_scanned, handle_session_list_failed, handle_session_list_loaded,
};
use super::session::fork::{
    handle_fork_session_failed, handle_fork_session_ready, handle_worktree_forked,
};
use super::session::lifecycle::{
    dispatch_exit_session, handle_session_created, handle_session_failed,
    handle_switch_model_complete, handle_worktree_session_created, handle_worktree_session_failed,
};
use super::session::load::{
    handle_card_detail_loaded, handle_deep_search_results, handle_session_load_failed,
    handle_session_loaded, handle_session_restore_failed, handle_session_restored,
    handle_session_search_debounce_expired, remove_session_from_pickers,
};
use super::session::modal::remove_agent_and_cleanup;
use super::session::picker_routing::PickerRequest;
use super::settings::ui::apply_setting_rollback;
use super::status::{
    handle_coding_data_sharing_failed, handle_coding_data_sharing_updated,
    handle_context_info_complete, handle_session_usage_result, scrub_error_for_toast,
    usage_modal_state_mut,
};
use super::transcript::{
    handle_hooks_list_loaded, handle_marketplace_list_loaded, handle_marketplace_updates_available,
    handle_mcp_toggle_done, handle_plugins_list_loaded, handle_skills_toggle_done,
};
use super::turn::handle_bg_task_killed;
use crate::app::actions::{
    Action, ClipboardPasteCompletion, ClipboardPasteContext, ClipboardPasteFailure,
    ClipboardPasteTarget, DoctorFixTarget, DoctorPlanningOutcome, Effect, ProbedAttachment,
    SubagentKillOutcome, TaskResult,
};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentDeferredSend;
use crate::app::app_view::{ActiveView, AppView, AuthState};
use crate::app::command_catalog::CommandCatalogSource;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::MemoryCommandKind;
use agent_client_protocol as acp;
pub(super) fn unregister_session_effect(session_id: Option<acp::SessionId>) -> Vec<Effect> {
    session_id
        .map(|sid| Effect::UnregisterActiveSession { session_id: sid })
        .into_iter()
        .collect()
}
pub(super) fn unregister_all_active_sessions(app: &AppView) -> Vec<Effect> {
    app.agents
        .values()
        .filter_map(|a| {
            a.session
                .session_id
                .as_ref()
                .map(|sid| Effect::UnregisterActiveSession {
                    session_id: sid.clone(),
                })
        })
        .collect()
}
fn displaced_draft_feedback_notice(
    outcome: distill_shell::session::FeedbackOutcome,
) -> &'static str {
    match outcome {
        distill_shell::session::FeedbackOutcome::Submitted => super::notes::FEEDBACK_THANKS_NOTICE,
        distill_shell::session::FeedbackOutcome::SubmittedCleanupFailed => {
            "Feedback was sent, but the stored draft could not be deleted. Delete it manually; do not resend."
        }
        distill_shell::session::FeedbackOutcome::LocalOnly => {
            "Feedback was saved locally but was not sent. The draft was kept."
        }
        distill_shell::session::FeedbackOutcome::OutcomeUnknown => {
            "The remote outcome is unknown. The draft was kept; do not resend it yet."
        }
        _ => "The remote outcome is unknown. The draft was kept; do not resend it yet.",
    }
}
pub(super) const X11_PRIMARY_PASTE_HINT: &str = "Try Shift+Insert to paste selected text";
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LiveSessionKind {
    Missing,
    ConversationOnly,
    IncludesBuild,
}
pub(super) fn live_session_kind(app: &AppView, session_id: &str) -> LiveSessionKind {
    let mut found = false;
    for agent in app.agents.values().filter(|agent| {
        agent
            .session
            .session_id
            .as_ref()
            .is_some_and(|id| id.0.as_ref() == session_id)
    }) {
        found = true;
        if !agent.conversation_entry {
            return LiveSessionKind::IncludesBuild;
        }
    }
    if found {
        LiveSessionKind::ConversationOnly
    } else {
        LiveSessionKind::Missing
    }
}
fn apply_workspace_transition(
    app: &mut AppView,
    transition: crate::app::workspace_membership::WorkspaceTransition,
) -> Vec<Effect> {
    use crate::app::workspace_membership::WorkspaceNotice;
    for notice in transition.notices {
        match notice {
            WorkspaceNotice::ArchiveRejectedReadOnly => {
                app.show_toast("Could not archive session: dashboard workspace is read-only");
            }
            WorkspaceNotice::LoadFailed { error } => {
                tracing::warn!(error = %error, "dashboard workspace load failed");
                app.show_toast(&format!("Could not load dashboard workspace: {error}"));
            }
            WorkspaceNotice::Refreshing { error } => {
                tracing::warn!(error = %error, "workspace snapshot after write failed");
                app.show_toast("Dashboard workspace changed; refreshing");
            }
            WorkspaceNotice::SyncFailed {
                count,
                session_id,
                error,
            } => {
                tracing::warn!(
                    failed = count,
                    session_id,
                    error,
                    "dashboard workspace sync partially failed"
                );
                app.show_toast(&format!(
                    "Could not sync {count} dashboard session{}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            WorkspaceNotice::ArchiveFailed {
                count,
                session_id,
                error,
            } => {
                tracing::warn!(
                    failed = count,
                    session_id,
                    error,
                    "dashboard workspace archive partially failed"
                );
                app.show_toast(&format!(
                    "Could not archive {count} dashboard session{}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            WorkspaceNotice::ReadOnly => {
                app.show_toast("Dashboard workspace is read-only in this Grok version");
            }
            WorkspaceNotice::WriterFailed { error } => {
                tracing::error!(error = %error, "dashboard workspace writer failed");
                app.show_toast("Dashboard workspace writer failed; reopening");
            }
            WorkspaceNotice::LayoutFailed { error } => {
                tracing::warn!(error = %error, "dashboard workspace layout write failed");
                app.show_toast("Could not save dashboard layout");
            }
            WorkspaceNotice::RefreshFailed { error } => {
                tracing::warn!(error = %error, "dashboard workspace refresh failed");
            }
        }
    }
    transition.effects
}
struct WorkspaceCompletionContext {
    before: Vec<crate::views::dashboard::Focusable>,
    old_resolver: crate::views::dashboard::SessionIdResolver,
    live_ids: std::collections::HashSet<distill_dashboard_store::SessionId>,
}
impl WorkspaceCompletionContext {
    fn capture(app: &AppView) -> Self {
        let workspace = app.workspace_membership.view();
        Self {
            before: super::dashboard::dashboard_focusables(app),
            old_resolver: crate::views::dashboard::SessionIdResolver::from_agents_and_workspace(
                &app.agents,
                workspace.as_ref(),
            ),
            live_ids: crate::app::workspace_sync::live_session_ids(app),
        }
    }
}
fn finish_workspace_result(
    app: &mut AppView,
    context: WorkspaceCompletionContext,
    transition: crate::app::workspace_membership::WorkspaceTransition,
) -> Vec<Effect> {
    use crate::views::dashboard::Focusable;
    let workspace = app.workspace_membership.view();
    let new_resolver = crate::views::dashboard::SessionIdResolver::from_agents_and_workspace(
        &app.agents,
        workspace.as_ref(),
    );
    let before = context
        .before
        .into_iter()
        .map(|focusable| match focusable {
            Focusable::Row(row) => {
                let rebound = context
                    .old_resolver
                    .to_persisted(&row)
                    .and_then(|persisted| new_resolver.resolve(&persisted))
                    .unwrap_or(row);
                Focusable::Row(rebound)
            }
            other => other,
        })
        .collect::<Vec<_>>();
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.rebind_workspace_identities(
            &context.old_resolver,
            &new_resolver,
            &mut app.agents,
        );
    }
    let after = super::dashboard::dashboard_focusables(app);
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.reconcile_visible_rows(&before, &after, &mut app.agents);
    }
    apply_workspace_transition(app, transition)
}
fn show_clipboard_toast(target: &ClipboardPasteTarget, message: &str, app: &mut AppView) {
    match target {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. }
        | ClipboardPasteTarget::FeedbackModal { agent_id, .. } => {
            if let Some(agent) = app.agents.get_mut(agent_id) {
                agent.show_toast(message);
            }
        }
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => {
            if let Some(dashboard) = app.dashboard.as_mut() {
                dashboard.error_toast = Some(message.to_owned());
            }
        }
    }
}
pub(super) fn maybe_show_x11_primary_paste_hint(
    eligible: bool,
    completion: ClipboardPasteCompletion,
    target: &ClipboardPasteTarget,
    app: &mut AppView,
) {
    if !eligible || completion != ClipboardPasteCompletion::FullMiss {
        return;
    }
    show_clipboard_toast(target, X11_PRIMARY_PASTE_HINT, app);
}
/// A clean `FullMiss` always qualifies; a remote read *error* (`AttachmentRead`) qualifies too.
/// Inside `grok wrap` the authoritative pasteboard is the local host's, not the (absent) remote one.
/// Every other failure (`TextRead`, `TargetInsertion`, `AlreadyReported`) is a real dead end and must keep toasting.
pub(super) fn wrap_host_image_request_eligible(completion: ClipboardPasteCompletion) -> bool {
    matches!(
        completion,
        ClipboardPasteCompletion::FullMiss
            | ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AttachmentRead)
    )
}
pub(super) fn show_clipboard_failure(
    target: &ClipboardPasteTarget,
    failure: ClipboardPasteFailure,
    app: &mut AppView,
) {
    let message = match failure {
        ClipboardPasteFailure::AlreadyReported => return,
        ClipboardPasteFailure::TextRead => "Couldn't read clipboard text",
        ClipboardPasteFailure::AttachmentRead => "Couldn't read clipboard contents",
        ClipboardPasteFailure::TargetInsertion => "Couldn't paste clipboard contents",
    };
    show_clipboard_toast(target, message, app);
}
fn apply_clipboard_paste_result(
    ctx: ClipboardPasteContext,
    image: ProbedAttachment,
    file_urls: Option<String>,
    app: &mut AppView,
) -> ClipboardPasteCompletion {
    match ctx.target.clone() {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return ClipboardPasteCompletion::Dropped;
            };
            agent.complete_clipboard_attachment_paste(ctx, image, file_urls)
        }
        ClipboardPasteTarget::FeedbackModal { agent_id, .. } => app
            .agents
            .get_mut(&agent_id)
            .map_or(ClipboardPasteCompletion::Dropped, |agent| {
                agent.complete_feedback_modal_attachment_paste(ctx, image)
            }),
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => app
            .dashboard
            .as_mut()
            .map_or(ClipboardPasteCompletion::Dropped, |dashboard| {
                dashboard.complete_clipboard_attachment_paste(ctx, image, file_urls)
            }),
    }
}
fn drain_clipboard_target(
    target: &ClipboardPasteTarget,
    hold_feedback_submit: bool,
    app: &mut AppView,
) -> Vec<Effect> {
    match target {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. } => {
            let is_active = app.active_view == ActiveView::Agent(*agent_id);
            let Some(agent) = app.agents.get_mut(agent_id) else {
                return vec![];
            };
            let resend = agent.take_deferred_send_after_paste();
            let action = resend
                .filter(|kind| is_active || matches!(kind, AgentDeferredSend::Stash))
                .and_then(|kind| agent.resume_deferred_send(kind));
            let mut effects = std::mem::take(&mut agent.pending_effects);
            if let Some(action) = action {
                effects.extend(dispatch(action, app));
            }
            effects
        }
        ClipboardPasteTarget::FeedbackModal {
            agent_id,
            modal_id,
            composition_id,
        } => {
            let is_active = app.active_view == ActiveView::Agent(*agent_id);
            let Some(agent) = app.agents.get_mut(agent_id) else {
                return vec![];
            };
            let resume = !hold_feedback_submit
                && is_active
                && agent
                    .feedback_modal
                    .as_mut()
                    .filter(|modal| {
                        modal.matches_id(*modal_id) && modal.matches_composition(*composition_id)
                    })
                    .is_some_and(|modal| modal.take_deferred_submit());
            if hold_feedback_submit
                && let Some(modal) = agent
                    .feedback_modal
                    .as_mut()
                    .filter(|modal| modal.matches_id(*modal_id))
            {
                modal.cancel_deferred_submit();
            }
            let mut effects = std::mem::take(&mut agent.pending_effects);
            if resume {
                effects.extend(dispatch(
                    Action::SubmitFeedbackModal {
                        modal_id: *modal_id,
                    },
                    app,
                ));
            }
            effects
        }
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => {
            let Some(dashboard) = app.dashboard.as_mut() else {
                return vec![];
            };
            let resends = dashboard.take_deferred_sends_after_paste();
            let mut effects = std::mem::take(&mut dashboard.pending_effects);
            if matches!(app.active_view, ActiveView::AgentDashboard) {
                for action in resends {
                    effects.extend(dispatch(action, app));
                }
            }
            effects
        }
    }
}
pub(crate) fn current_doctor_target(
    app: &AppView,
    target: &DoctorFixTarget,
) -> Option<DoctorFixTarget> {
    let agent = app.agents.get(&target.agent_id)?;
    if agent.session.cwd != target.cwd {
        return None;
    }
    match (&target.session_id, &agent.session.session_id) {
        (Some(expected), Some(current))
            if expected == current
                && target.session_binding_epoch == agent.session_binding_epoch =>
        {
            Some(target.clone())
        }
        (None, Some(current))
            if agent.session_binding_epoch == target.session_binding_epoch.wrapping_add(1) =>
        {
            Some(DoctorFixTarget {
                session_id: Some(current.clone()),
                session_binding_epoch: agent.session_binding_epoch,
                ..target.clone()
            })
        }
        (None, None) if target.session_binding_epoch == agent.session_binding_epoch => {
            Some(target.clone())
        }
        _ => None,
    }
}
pub(crate) fn deliver_doctor_message(app: &mut AppView, preferred: AgentId, message: String) {
    let destination = app
        .agents
        .contains_key(&preferred)
        .then_some(preferred)
        .or_else(|| match app.active_view {
            ActiveView::Agent(id) if app.agents.contains_key(&id) => Some(id),
            _ => app.agents.keys().next().copied(),
        });
    if let Some(destination) = destination
        && let Some(agent) = app.agents.get_mut(&destination)
    {
        agent.scrollback.push_block(RenderBlock::system(message));
        return;
    }
    app.startup_warnings.push(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Info,
        message,
        action: None,
    });
}
pub(super) fn dispatch_task_result(result: TaskResult, app: &mut AppView) -> Vec<Effect> {
    let result = match result {
        TaskResult::WithPinnedMemoryMode {
            agent_id,
            memory_mode,
            result,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.memory_mode = memory_mode;
            }
            *result
        }
        result => result,
    };
    if result.ends_startup() {
        app.finish_startup(distill_telemetry::startup::StartupOutcome::Ok);
    }
    if !matches!(
        &result,
        TaskResult::WorkspaceSnapshotLoaded { .. }
            | TaskResult::WorkspaceWriteCompleted { .. }
            | TaskResult::WorkspaceWriteTaskFailed { .. }
            | TaskResult::WorkspaceRefreshed { .. }
            | TaskResult::WorkspaceRefreshTaskFailed { .. }
    ) {
        crate::app::workspace_sync::request(app);
    }
    match result {
        TaskResult::WithPinnedMemoryMode { .. } => {
            unreachable!("pinned memory mode wrapper is removed before task-result dispatch")
        }
        TaskResult::SessionCreated {
            agent_id,
            session_id,
            models: new_models,
            modes,
        } => handle_session_created(app, agent_id, session_id, new_models, modes),
        TaskResult::SessionFailed {
            agent_id,
            error,
            timed_out,
        } => handle_session_failed(app, agent_id, error, timed_out),
        TaskResult::WorktreeSessionCreated {
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            models: new_models,
            modes,
            strategy_summary,
        } => handle_worktree_session_created(
            app,
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            new_models,
            modes,
            strategy_summary,
        ),
        TaskResult::WorktreeForked {
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            code_restored,
            restore_summary,
            restore_degree,
            resume_session_id,
            strategy_summary,
        } => handle_worktree_forked(
            app,
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            code_restored,
            restore_summary,
            restore_degree,
            resume_session_id,
            strategy_summary,
        ),
        TaskResult::WorktreeSessionFailed {
            agent_id,
            error,
            orphaned_worktree_root,
            timed_out,
        } => {
            handle_worktree_session_failed(app, agent_id, error, orphaned_worktree_root, timed_out)
        }
        TaskResult::ForkSessionReady {
            agent_id,
            new_session_id,
            cwd,
            parent_session_id,
        } => handle_fork_session_ready(app, agent_id, new_session_id, cwd, parent_session_id),
        TaskResult::ForkSessionFailed { agent_id, error } => {
            handle_fork_session_failed(app, agent_id, error)
        }
        TaskResult::BillingFetched {
            agent_id,
            balance,
            silent,
            subscription_tier,
            autotopup,
            nonce,
        } => handle_billing_fetched(
            app,
            agent_id,
            balance,
            silent,
            subscription_tier,
            autotopup,
            nonce,
        ),
        TaskResult::BillingError {
            agent_id,
            error,
            silent,
            nonce,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if let Some(state) = usage_modal_state_mut(agent)
                    && state.fetch_nonce == nonce
                {
                    state.billing_loading = false;
                    state.billing_error = Some(error.clone());
                }
                if !silent {
                    agent.scrollback.push_block(RenderBlock::System(
                        crate::scrollback::blocks::SystemMessageBlock::new(format!(
                            "Billing error: {error}"
                        )),
                    ));
                }
            }
            vec![]
        }
        TaskResult::AppBillingFetched {
            balance,
            autotopup,
            nonce,
        } => {
            app.credit_balance = balance;
            apply_auto_topup(&mut app.auto_topup, &autotopup);
            if let Some(state) = app.dashboard.as_mut().and_then(|d| d.usage_modal.as_mut())
                && state.fetch_nonce == nonce
            {
                state.billing_loading = false;
                state.billing_error = None;
            }
            vec![]
        }
        TaskResult::AppBillingError { error, nonce } => {
            if let Some(state) = app.dashboard.as_mut().and_then(|d| d.usage_modal.as_mut())
                && state.fetch_nonce == nonce
            {
                state.billing_loading = false;
                state.billing_error = Some(error);
            }
            vec![]
        }
        TaskResult::GateRefreshed { settings } => handle_gate_refreshed(app, settings),
        TaskResult::SessionLoaded {
            agent_id,
            session_id,
            models: new_models,
            modes,
            code_restored,
            restore_summary,
            restore_degree,
            running_prompt_id,
        } => handle_session_loaded(
            app,
            agent_id,
            session_id,
            new_models,
            modes,
            code_restored,
            restore_summary,
            restore_degree,
            running_prompt_id,
        ),
        TaskResult::SessionMetaFromDisk {
            agent_id,
            title,
            last_turn_summary,
            last_turn_summary_gen,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if let Some((raw, is_manual)) = title
                    && let Some(t) =
                        distill_shell::session::persistence::sanitize_and_cap_title(&raw)
                {
                    if is_manual && agent.display_name.is_none() {
                        agent.display_name = Some(t.clone());
                    }
                    if agent.generated_session_title.is_none() {
                        agent.generated_session_title = Some(t);
                    }
                }
                if agent.last_turn_summary_gen == last_turn_summary_gen
                    && agent.last_turn_summary.is_none()
                {
                    agent.last_turn_summary = last_turn_summary;
                }
            }
            vec![]
        }
        TaskResult::SessionLoadFailed {
            agent_id,
            session_id,
            error,
        } => handle_session_load_failed(app, agent_id, session_id, error),
        TaskResult::SessionListLoaded {
            host,
            generation,
            sessions,
            partial,
            scope,
            seq,
            query,
        } => handle_session_list_loaded(
            app,
            PickerRequest {
                host,
                generation,
                seq,
            },
            sessions,
            partial,
            scope,
            query,
        ),
        TaskResult::ForeignSessionsScanned { entries, seq } => {
            handle_foreign_sessions_scanned(app, entries, seq)
        }
        TaskResult::ForeignResumeCwdCanonicalized {
            requested_cwd,
            canonical_cwd,
            launch_token,
        } => {
            let accepted_cwd = canonical_cwd.clone();
            if app.accept_foreign_resume_canonical_cwd(launch_token, &requested_cwd, canonical_cwd)
                && let Some(canonical_cwd) = accepted_cwd
            {
                vec![Effect::DetectForeignResumeHint {
                    canonical_cwd,
                    compat: app.foreign_session_compat,
                    distill_home: distill_tools::util::distill_home::distill_home(),
                    launch_token,
                }]
            } else {
                vec![]
            }
        }
        TaskResult::ForeignResumeHintDetected {
            canonical_cwd,
            launch_token,
            hint,
        } => {
            app.apply_foreign_resume_detection(launch_token, &canonical_cwd, hint);
            vec![]
        }
        TaskResult::SessionListFailed {
            host,
            generation,
            error,
            seq,
            query,
        } => handle_session_list_failed(
            app,
            PickerRequest {
                host,
                generation,
                seq,
            },
            error,
            query,
        ),
        TaskResult::SessionSearchDebounceExpired {
            host,
            generation,
            query,
            seq,
        } => handle_session_search_debounce_expired(
            app,
            PickerRequest {
                host,
                generation,
                seq,
            },
            query,
        ),
        TaskResult::RosterLoaded { sessions } => {
            app.leader_roster = sessions;
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::RosterFailed { error } => {
            tracing::debug!(error = %error, "leader roster fetch failed");
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::DashboardSessionsLoaded { sessions } => {
            app.dashboard_local_sessions = sessions;
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::WorkspaceSnapshotLoaded { store, snapshot } => {
            let context = WorkspaceCompletionContext::capture(app);
            app.dashboard_sessions_loading = false;
            let transition =
                app.workspace_membership
                    .on_store_opened(store, snapshot, &context.live_ids);
            finish_workspace_result(app, context, transition)
        }
        TaskResult::WorkspaceSnapshotFailed { error, retryable } => {
            let transition = app
                .workspace_membership
                .on_store_open_failed(error, retryable);
            app.dashboard_sessions_loading = transition
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::LoadWorkspaceSnapshot { .. }));
            apply_workspace_transition(app, transition)
        }
        TaskResult::WorkspaceWriteCompleted { store, completion } => {
            let context = WorkspaceCompletionContext::capture(app);
            let transition =
                app.workspace_membership
                    .on_write_completed(store, completion, &context.live_ids);
            finish_workspace_result(app, context, transition)
        }
        TaskResult::WorkspaceWriteTaskFailed { db_path, error } => {
            let transition = app.workspace_membership.on_write_task_lost(db_path, error);
            apply_workspace_transition(app, transition)
        }
        TaskResult::WorkspaceRefreshed { store, snapshot } => {
            if !matches!(snapshot, Ok(Some(_))) {
                let transition = app.workspace_membership.on_refresh_completed(
                    store,
                    snapshot,
                    &std::collections::HashSet::new(),
                );
                return apply_workspace_transition(app, transition);
            }
            let context = WorkspaceCompletionContext::capture(app);
            let transition =
                app.workspace_membership
                    .on_refresh_completed(store, snapshot, &context.live_ids);
            finish_workspace_result(app, context, transition)
        }
        TaskResult::WorkspaceRefreshTaskFailed { db_path, error } => {
            let transition = app
                .workspace_membership
                .on_refresh_task_lost(db_path, error);
            apply_workspace_transition(app, transition)
        }
        TaskResult::CardDetailLoaded {
            host,
            generation,
            source,
            session_id,
            seq,
            detail,
        } => handle_card_detail_loaded(
            app,
            PickerRequest {
                host,
                generation,
                seq,
            },
            source,
            session_id,
            detail,
        ),
        TaskResult::SessionRestored {
            agent_id,
            local_session_id,
        } => handle_session_restored(app, agent_id, local_session_id),
        TaskResult::SessionRestoreFailed { agent_id, error } => {
            handle_session_restore_failed(app, agent_id, error)
        }
        TaskResult::SessionRestoreProgress { agent_id, message } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && !defer_to_open_reload_window(agent, agent_id, "SessionRestoreProgress")
            {
                agent.scrollback.push_block(RenderBlock::system(message));
            }
            vec![]
        }
        TaskResult::PromptResponse {
            agent_id,
            result,
            http_status,
            prompt_id,
        } => {
            let effects = handle_prompt_response(app, agent_id, result, http_status, prompt_id);
            app.refresh_status_line_for(agent_id);
            effects
        }
        TaskResult::SendPromptNowFailed {
            agent_id,
            session_id,
            prompt_id,
            error,
            blocks,
        } => {
            let sid = session_id.0.to_string();
            super::queue::retire_optimistic_echo(
                &mut app.optimistic_prompt_echoes,
                &mut app.shared_prompt_queues,
                &sid,
                &prompt_id,
            );
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.shared_queue.retain(|e| e.id != prompt_id);
                agent.note_queue_echo_retired(&prompt_id);
                if agent.expect_send_now_cancel.as_deref() == Some(prompt_id.as_str())
                    || agent.follow_without_jump_prompt_id.as_deref() == Some(prompt_id.as_str())
                {
                    agent.clear_send_now_expectation();
                }
                agent.retire_send_now_painted_block(&prompt_id);
                let text = blocks
                    .iter()
                    .find_map(|b| match b {
                        acp::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let id = agent.session.next_queue_id;
                agent.session.next_queue_id += 1;
                agent
                    .session
                    .pending_prompts
                    .push_front(crate::app::agent::QueuedPrompt {
                        wire_blocks: Some(blocks),
                        ..crate::app::agent::QueuedPrompt::plain(
                            id,
                            &text,
                            crate::app::agent::QueueEntryKind::Prompt,
                        )
                    });
                agent.show_toast(&format!("Send now failed. Requeued: {error}"));
            }
            vec![]
        }
        TaskResult::PreferredModelPersisted { result } => {
            if let Err(err) = result
                && let Some(agent) = get_active_agent_mut(app)
            {
                agent.scrollback.push_block(RenderBlock::system(format!(
                    "Couldn't save preferred model: {err} (still active for this session)"
                )));
            }
            vec![]
        }
        TaskResult::CancelComplete => {
            tracing::trace!("Cancel notification sent successfully");
            vec![]
        }
        TaskResult::SetSessionModeFailed { session_id } => {
            if let Some(agent) = find_agent_by_session_id(&mut app.agents, session_id.0.as_ref()) {
                agent.plan_mode_pending = None;
                agent.session_mode_pending = None;
                agent.pending_post_turn_commit = None;
            }
            vec![]
        }
        TaskResult::ConsentPersistFailed { error } => {
            tracing::warn!(%error, "consent answer not persisted; the notice re-arms next launch");
            app.show_toast(
                "\u{2717} Could not save your answer, so this notice returns next launch",
            );
            vec![]
        }
        TaskResult::ConsentRecorded { notice_id, version } => match app.account_email.clone() {
            Some(account) => {
                vec![Effect::PersistConsentAnswer {
                    account: Some(account),
                    notice_id,
                    version,
                    acked: true,
                }]
            }
            None => vec![],
        },
        TaskResult::KillSubagentComplete {
            session_id,
            subagent_id,
            attempt_id,
            outcome,
        } => {
            if let SubagentKillOutcome::NothingLive { status } = outcome {
                let status = status.as_deref().unwrap_or("cancelled");
                crate::app::acp_handler::finalize_killed_subagent(
                    app,
                    &session_id,
                    &subagent_id,
                    attempt_id.as_deref(),
                    status,
                );
            }
            vec![]
        }
        TaskResult::CompactComplete { agent_id, result } => {
            handle_compact_complete(app, agent_id, result)
        }
        TaskResult::MemoryFlushComplete { agent_id, result } => handle_memory_command_complete(
            app,
            agent_id,
            MemoryCommandKind::Flush,
            result.map(|r| (r.summary(), r.succeeded())),
        ),
        TaskResult::MemoryDreamComplete { agent_id, result } => handle_memory_command_complete(
            app,
            agent_id,
            MemoryCommandKind::Dream,
            result.map(|r| (r.summary(), r.succeeded())),
        ),
        TaskResult::SwitchModelComplete {
            agent_id,
            model_id,
            effort,
            result,
            prev_model_id,
        } => handle_switch_model_complete(app, agent_id, model_id, effort, result, prev_model_id),
        TaskResult::EffortAutoSet {
            agent_id,
            model_id,
            result,
        } => {
            if let Err(message) = result {
                if let Some(agent) = app.agents.get_mut(&agent_id) {
                    // The flag was set optimistically at dispatch: a rejection
                    // means the harness never enabled auto, so undo it.
                    agent.session.models.effort_auto = false;
                    let display_name = agent.session.models.display_name_for(&model_id);
                    agent.scrollback.push_block(RenderBlock::system(format!(
                        "Couldn't enable auto effort for {display_name}: {message} \
                             (fixed effort unchanged)"
                    )));
                }
            }
            vec![]
        }
        TaskResult::BgTaskKilled {
            session_id,
            task_id,
            outcome,
        } => handle_bg_task_killed(app, session_id, task_id, outcome),
        TaskResult::BgTaskKillFailed {
            session_id,
            task_id,
            error,
        } => {
            tracing::warn!(task_id = %task_id, error = %error, "Failed to kill bg task");
            if let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id)
                && let Some(task) = agent.session.bg_tasks.get_mut(&task_id)
            {
                task.pending_kill = false;
                task.kill_requested_at = None;
            }
            vec![]
        }
        TaskResult::ChangelogFetched { markdown, entries } => {
            app.changelog_markdown = markdown;
            app.changelog_bullets =
                distill_shell::util::changelog::bullets_from_entries(&entries, 3);
            vec![]
        }
        TaskResult::ClipboardAttachmentProbed {
            ctx,
            image,
            file_urls,
        } => {
            let is_clipboard_key = ctx.source.is_clipboard_key();
            let primary_hint_eligible = is_clipboard_key
                && !app.screen_mode.is_minimal()
                && crate::clipboard::x11_primary_guidance_available();
            let target = ctx.target.clone();
            let wrap_text = if is_clipboard_key {
                ctx.source.text().map(str::to_owned)
            } else {
                None
            };
            let completion = apply_clipboard_paste_result(ctx, image, file_urls, app);
            let wrap_request_emitted = wrap_host_image_request_eligible(completion)
                && is_clipboard_key
                && crate::wrap_clipboard_image::maybe_request_wrap_host_image(
                    None,
                    wrap_text.as_deref(),
                    None,
                );
            let effects = drain_clipboard_target(&target, wrap_request_emitted, app);
            maybe_show_x11_primary_paste_hint(
                primary_hint_eligible && !wrap_request_emitted,
                completion,
                &target,
                app,
            );
            if let ClipboardPasteCompletion::Failed(failure) = completion
                && !wrap_request_emitted
            {
                show_clipboard_failure(&target, failure, app);
            }
            effects
        }
        TaskResult::FeedbackImageRehydrated {
            agent_id,
            modal_id,
            image_identity,
            result,
        } => {
            let is_active = app.active_view == ActiveView::Agent(agent_id);
            let resume = app
                .agents
                .get_mut(&agent_id)
                .and_then(|agent| agent.feedback_modal.as_mut())
                .filter(|modal| modal.matches_id(modal_id))
                .is_some_and(|modal| {
                    modal.apply_rehydrated_image(image_identity, result);
                    is_active && modal.take_deferred_submit()
                });
            if resume {
                dispatch(Action::SubmitFeedbackModal { modal_id }, app)
            } else {
                vec![]
            }
        }
        TaskResult::PromptImagePreviewPrepared => vec![],
        TaskResult::DoctorFixPlanned { target, result } => {
            let Some(target) = current_doctor_target(app, &target) else {
                deliver_doctor_message(
                    app,
                    target.agent_id,
                    "This fix was cancelled because the session changed. Run `/doctor fix` again."
                        .to_owned(),
                );
                return vec![];
            };
            match result {
                Ok(DoctorPlanningOutcome::Listing(listing)) => {
                    deliver_doctor_message(app, target.agent_id, listing);
                }
                Ok(DoctorPlanningOutcome::Plan(plan)) => {
                    super::prompt::open_doctor_fix_question(app, target, plan);
                }
                Ok(DoctorPlanningOutcome::RunLocally(command)) => {
                    deliver_doctor_message(
                        app,
                        target.agent_id,
                        format!(
                            "This fix configures your local computer, not this SSH session.\nOn your local computer, run: {command}"
                        ),
                    );
                }
                Err(error) => deliver_doctor_message(
                    app,
                    target.agent_id,
                    if error.starts_with("Could not prepare the fix:") {
                        error
                    } else {
                        format!("Could not prepare the fix: {error}")
                    },
                ),
            }
            vec![]
        }
        TaskResult::DoctorFixApplied { target, result } => {
            let message = match result {
                Ok(outcome) => crate::diagnostics::format_fix_success(&outcome),
                Err(error) if error.starts_with("Could not apply the fix:") => error,
                Err(error) => format!("Could not apply the fix: {error}"),
            };
            deliver_doctor_message(app, target.agent_id, message);
            vec![]
        }
        TaskResult::AnnouncementsHiddenPersisted { result } => {
            if let Err(e) = result {
                tracing::warn!("Failed to persist announcements hidden state: {}", e);
            }
            vec![]
        }
        TaskResult::PromptHistoryLoaded { agent_id, prompts } => {
            use distill_tools::implementations::skills::skill::extract_skill_display_text;
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.session.prompt_history_loading = false;
                let fetched: Vec<String> = prompts
                    .into_iter()
                    .map(|p| extract_skill_display_text(&p).unwrap_or(p))
                    .collect();
                let local: std::collections::HashSet<String> = agent
                    .session
                    .prompt_history
                    .iter()
                    .flat_map(|p| {
                        let t = p.trim();
                        [t.to_owned(), t.strip_prefix("! ").unwrap_or(t).to_owned()]
                    })
                    .collect();
                let local_entries = agent.session.prompt_history.len();
                let fetched_entries = fetched.len();
                agent
                    .session
                    .prompt_history
                    .extend(fetched.into_iter().filter(|p| !local.contains(p.trim())));
                agent
                    .session
                    .prompt_history
                    .truncate(crate::app::agent::PROMPT_HISTORY_CAP);
                tracing::info!(
                    history.local_entries = local_entries,
                    history.fetched_entries = fetched_entries,
                    history.merged_entries = agent.session.prompt_history.len(),
                    "history.fetch_merged"
                );
                if agent.prompt.history_search.is_active() {
                    let history = agent.combined_prompt_history();
                    agent.prompt.history_search.refresh_items(&history);
                    if !agent.prompt.history_search.is_browse() {
                        let query = agent.prompt.text().to_owned();
                        agent.prompt.history_search.update_query(&query);
                    }
                }
            }
            vec![]
        }
        TaskResult::ProviderLoginFinished {
            provider,
            attempt_id,
            result,
        } => {
            if app.provider_login_pending != Some(provider)
                || app.provider_login_attempt_id != Some(attempt_id)
            {
                return vec![];
            }
            app.provider_login_pending = None;
            app.provider_login_attempt_id = None;
            app.provider_login_cancel = None;
            if let Some(state) = app.provider_auth.as_mut() {
                state.chatgpt = distill_shell::codex_auth::is_logged_in();
                state.openrouter = distill_shell::openrouter_auth::is_logged_in();
            }
            let succeeded = result.is_ok();
            if let Some(state) = app.onboarding.as_mut() {
                state.set_auth_result(
                    succeeded,
                    match &result {
                        Ok(message) => message.clone(),
                        Err(error) => format!("{} login failed: {error}", provider.name()),
                    },
                );
            }
            let content = match result {
                Ok(message) => message,
                Err(error) => format!(
                    "{} login failed: {error}\n\nSelect Log in with {} to try again.",
                    provider.name(),
                    provider.name()
                ),
            };
            app.show_toast(&content);
            super::status::dispatch_show_release_notes(
                app,
                format!("{} login", provider.name()),
                content,
            )
        }
        TaskResult::ProviderLogoutFinished { provider, result } => {
            app.provider_login_pending = None;
            app.provider_login_attempt_id = None;
            app.provider_login_cancel = None;
            if let Some(state) = app.provider_auth.as_mut() {
                state.chatgpt = distill_shell::codex_auth::is_logged_in();
                state.openrouter = distill_shell::openrouter_auth::is_logged_in();
            }
            app.show_toast(&match result {
                Ok(()) => format!("Logged out of {}.", provider.name()),
                Err(error) => format!("{} logout failed: {error}", provider.name()),
            });
            vec![]
        }
        TaskResult::ProviderLoginBrowserFallback {
            provider,
            attempt_id,
            url,
        } => {
            if app.provider_login_pending != Some(provider)
                || app.provider_login_attempt_id != Some(attempt_id)
            {
                return vec![];
            }
            if let Some(state) = app.onboarding.as_mut()
                && state.auth_pending
                && state.auth_provider == Some(provider)
            {
                state.set_auth_browser_fallback(&url);
            }
            super::status::dispatch_show_release_notes(
                app,
                format!("{} login", provider.name()),
                format!(
                    "The browser could not open automatically. Open this URL to finish signing in:\n\n{url}"
                ),
            )
        }
        TaskResult::AuthComplete { request_seq, meta } => {
            let onboarding_login_pending = app
                .onboarding
                .as_ref()
                .is_some_and(|state| state.auth_pending && state.auth_provider.is_none());
            let effects = handle_auth_complete(app, request_seq, meta);
            if onboarding_login_pending
                && matches!(app.auth_state, AuthState::Done)
                && let Some(state) = app.onboarding.as_mut()
            {
                state.set_auth_result(true, "Grok connected. Choose a primary model below.");
            }
            effects
        }
        TaskResult::AuthFailed { request_seq, error } => {
            if let AuthState::Authenticating {
                request_seq: current_seq,
                ..
            } = &app.auth_state
                && *current_seq == request_seq
            {
                app.auth_state = AuthState::Pending { error: Some(error) };
                app.auth_code_input.reset();
                if let Some(state) = app.onboarding.as_mut()
                    && state.auth_pending
                    && state.auth_provider.is_none()
                {
                    state.set_auth_result(
                        false,
                        "Grok login failed. Retry or choose another provider.",
                    );
                }
            }
            vec![]
        }
        TaskResult::AuthUrlReady {
            request_seq,
            auth_url,
            external,
            mode,
        } => handle_auth_url_ready(app, request_seq, auth_url, external, mode),
        TaskResult::AuthCodeSubmitted { .. } => vec![],
        TaskResult::AuthCancelComplete => vec![],
        TaskResult::McpsListLoaded { agent_id, result } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.pending_action = None;
                modal.pending_entry_index = None;
                modal.mcps_data = match result {
                    Ok(response) => TabDataState::Loaded(response),
                    Err(e) => TabDataState::Error(e),
                };
            }
            vec![]
        }
        TaskResult::McpAuthTriggerDone {
            agent_id,
            server_name,
            result,
        } => handle_mcp_auth_trigger_done(app, agent_id, server_name, result),
        TaskResult::McpSetupSubmitDone {
            agent_id,
            server_name,
            result,
        } => handle_mcp_setup_submit_done(app, agent_id, server_name, result),
        TaskResult::HooksListLoaded { agent_id, result } => {
            handle_hooks_list_loaded(app, agent_id, result)
        }
        TaskResult::PluginsListLoaded { agent_id, result } => {
            handle_plugins_list_loaded(app, agent_id, result)
        }
        TaskResult::MemoryListLoaded { agent_id, result } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                match result {
                    Ok(_)
                        if !matches!(
                            agent.active_modal,
                            None | Some(crate::views::modal::ActiveModal::MemoryBrowser { .. })
                        ) => {}
                    Ok(listing) => {
                        agent.active_modal =
                            Some(crate::views::modal::ActiveModal::MemoryBrowser {
                                state: Box::new(
                                    crate::views::memory_modal::MemoryModalState::from_listing(
                                        listing,
                                    ),
                                ),
                            });
                    }
                    Err(error) => {
                        agent.scrollback.push_block(RenderBlock::system(error));
                    }
                }
            }
            vec![]
        }
        TaskResult::MemoryToggleResult { agent_id, result } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if let Some(crate::views::modal::ActiveModal::MemoryBrowser { state }) =
                    agent.active_modal.as_mut()
                {
                    state.apply_toggle_result(result);
                } else {
                    let text = match result {
                        Ok(response) => response.message,
                        Err(error) => error,
                    };
                    agent.scrollback.push_block(RenderBlock::system(text));
                }
            }
            vec![]
        }
        TaskResult::MemoryForgetResult {
            agent_id,
            path,
            result,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some(crate::views::modal::ActiveModal::MemoryBrowser { state }) =
                    agent.active_modal.as_mut()
            {
                state.apply_forget_result(&path, result);
            }
            vec![]
        }
        TaskResult::HooksActionResult { agent_id, result }
        | TaskResult::PluginsActionResult { agent_id, result }
        | TaskResult::MarketplaceActionResult { agent_id, result } => {
            dispatch_action_result(app, agent_id, result)
        }
        TaskResult::CtaPluginInstallDone {
            agent_id,
            plugin_name,
            result,
        } => handle_cta_plugin_install_done(app, agent_id, plugin_name, result),
        TaskResult::CtaPluginReloadDone {
            agent_id,
            plugin_name,
            result,
        } => handle_cta_plugin_reload_done(app, agent_id, plugin_name, result),
        TaskResult::PluginCtaMcpsLoaded {
            agent_id,
            plugin_name,
            result,
        } => handle_plugin_cta_mcps_loaded(app, agent_id, plugin_name, result),
        TaskResult::CtaInstalledDismissTimeout {
            agent_id,
            plugin_name,
        } => {
            use crate::app::agent_view::CtaPhase;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let CtaPhase::Installed { name } = &agent.plugin_cta.phase
                && *name == plugin_name
            {
                agent.plugin_cta.phase = CtaPhase::Hidden;
            }
            vec![]
        }
        TaskResult::McpToggleDone { agent_id, result } => {
            handle_mcp_toggle_done(app, agent_id, result)
        }
        TaskResult::MarketplaceUpdatesAvailable { agent_id, updates } => {
            handle_marketplace_updates_available(app, agent_id, updates)
        }
        TaskResult::MarketplaceListLoaded { agent_id, result } => {
            handle_marketplace_list_loaded(app, agent_id, result)
        }
        TaskResult::PluginCtaCatalogLoaded { agent_id, result } => {
            handle_plugin_cta_catalog_loaded(app, agent_id, result)
        }
        TaskResult::SkillsListLoaded { agent_id, result } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.skills_data = match result {
                    Ok(skills) => {
                        modal.seed_skills_groups_once(&skills);
                        TabDataState::Loaded(skills)
                    }
                    Err(e) => TabDataState::Error(e),
                };
                modal.pending_action = None;
                modal.pending_entry_index = None;
            }
            vec![]
        }
        TaskResult::WorkflowsListLoaded {
            agent_id,
            session_id,
            result,
        } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && agent.session.session_id.as_ref() == Some(&session_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.workflows_data = match result {
                    Ok(workflows) => TabDataState::Loaded(workflows),
                    Err(e) => TabDataState::Error(e),
                };
            }
            vec![]
        }
        TaskResult::SkillsToggleDone { agent_id, result } => {
            handle_skills_toggle_done(app, agent_id, result)
        }
        TaskResult::ShareSessionComplete {
            agent_id,
            share_url,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Session shared: {share_url}"
                    )));
            }
            vec![]
        }
        TaskResult::ShareSessionFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't share session: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::SessionAgentNameResolved {
            agent_id,
            agent_name,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.session_agent_name = agent_name.clone();
                if let Some(modal) = agent.agents_modal.as_mut() {
                    modal.active_agent = agent_name;
                }
            }
            vec![]
        }
        TaskResult::SessionInfoComplete {
            agent_id,
            session_id,
            info,
            text,
            fields,
            nonce,
        } => {
            let minimal = app.screen_mode.is_minimal();
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if agent.session.session_id.as_ref() != Some(&session_id) {
                    return vec![];
                }
                if let Some(state) = usage_modal_state_mut(agent)
                    && state.fetch_nonce != nonce
                {
                    return vec![];
                }
                agent.session_agent_name = info.data.agent_name.clone();
                if let Some(modal) = agent.agents_modal.as_mut() {
                    modal.active_agent = info.data.agent_name.clone();
                }
                agent.apply_full_context_info(info.data.context);
                if let Some(state) = usage_modal_state_mut(agent) {
                    state.session_fields = Some(fields);
                    state.session_error = None;
                } else if minimal {
                    push_and_page_flip(
                        &mut agent.scrollback,
                        crate::scrollback::block::RenderBlock::system(text),
                    );
                }
            }
            vec![]
        }
        TaskResult::SessionInfoFailed {
            agent_id,
            session_id,
            error,
            nonce,
        } => {
            let minimal = app.screen_mode.is_minimal();
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if agent.session.session_id.as_ref() != Some(&session_id) {
                    return vec![];
                }
                if let Some(state) = usage_modal_state_mut(agent) {
                    if state.fetch_nonce == nonce {
                        state.session_error = Some(error);
                    }
                } else if minimal {
                    push_and_page_flip(
                        &mut agent.scrollback,
                        crate::scrollback::block::RenderBlock::system(format!(
                            "Couldn't load session info: {error}"
                        )),
                    );
                }
            }
            vec![]
        }
        TaskResult::CodingDataSharingUpdated {
            agent_id,
            opted_in,
            seq,
        } => handle_coding_data_sharing_updated(app, agent_id, opted_in, seq),
        TaskResult::CodingDataSharingFailed {
            agent_id,
            error,
            seq,
        } => handle_coding_data_sharing_failed(app, agent_id, error, seq),
        TaskResult::RenameSessionComplete { agent_id, title } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                let safe = crate::views::session_title::sanitize_display_text(&title);
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Session renamed to \"{safe}\""
                    )));
            }
            vec![]
        }
        TaskResult::RenameSessionFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't rename session: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::ResetSessionTitleComplete { agent_id } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.title_unpin_committed = false;
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(
                        "Session title reset to auto",
                    ));
            }
            vec![]
        }
        TaskResult::ResetSessionTitleFailed {
            agent_id,
            error,
            previous_display_name,
            previous_generated_title,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                if agent.title_unpin_committed {
                    agent.title_unpin_committed = false;
                    agent
                        .scrollback
                        .push_block(crate::scrollback::block::RenderBlock::system(
                            "Session title reset to auto",
                        ));
                } else {
                    agent.display_name = previous_display_name;
                    agent.generated_session_title = previous_generated_title;
                    agent
                        .scrollback
                        .push_block(crate::scrollback::block::RenderBlock::system(format!(
                            "Couldn't reset session title: {error}"
                        )));
                }
            }
            vec![]
        }
        TaskResult::DeleteSessionComplete {
            source,
            session_id,
            after,
        } => {
            use crate::app::actions::AfterSessionDelete;
            remove_session_from_pickers(
                app,
                &source,
                &session_id,
                after != AfterSessionDelete::Stay,
            );
            let live_kind = live_session_kind(app, &session_id);
            let deleted_build =
                source != "current" || live_kind != LiveSessionKind::ConversationOnly;
            let removal_cause =
                if after == AfterSessionDelete::Stay && live_kind != LiveSessionKind::Missing {
                    crate::app::workspace_membership::RemovalCause::HistoryDeletedWithRetainedView
                } else {
                    crate::app::workspace_membership::RemovalCause::Archive
                };
            let membership_removal_failed = deleted_build
                && !crate::app::workspace_sync::request_removal(app, &session_id, removal_cause);
            let delete_notice = if membership_removal_failed {
                "Session deleted, but dashboard membership could not be removed"
            } else {
                "Session deleted"
            };
            if after == AfterSessionDelete::Stay {
                app.dashboard_local_sessions
                    .retain(|entry| entry.session_id != session_id);
                app.leader_roster
                    .retain(|entry| entry.session_id != session_id);
                app.show_toast(delete_notice);
                return vec![];
            }
            if after == AfterSessionDelete::UnusedHusk {
                app.dashboard_local_sessions
                    .retain(|entry| entry.session_id != session_id);
                app.leader_roster
                    .retain(|entry| entry.session_id != session_id);
                return vec![];
            }
            let sid = acp::SessionId::new(session_id.clone());
            let to_remove: Vec<_> = app
                .agents
                .iter()
                .filter(|(_, agent)| agent.session.session_id.as_ref() == Some(&sid))
                .map(|(id, _)| *id)
                .collect();
            let foreground =
                matches!(app.active_view, ActiveView::Agent(id) if to_remove.contains(&id));
            let roster_row = crate::views::dashboard::DashboardRowId::Roster {
                session_id: session_id.clone(),
            };
            let closed_rows: Vec<_> = to_remove
                .iter()
                .copied()
                .map(crate::views::dashboard::DashboardRowId::TopLevel)
                .chain(std::iter::once(roster_row))
                .collect();
            let selected = app.dashboard.as_ref().and_then(|d| d.selected.clone());
            let neighbor = if after == AfterSessionDelete::Dashboard
                && let Some(sel) = selected.as_ref().filter(|sel| closed_rows.contains(sel))
            {
                super::dashboard::dashboard_neighbor_row(app, sel)
            } else {
                None
            };
            app.dashboard_local_sessions
                .retain(|entry| entry.session_id != session_id);
            app.leader_roster
                .retain(|entry| entry.session_id != session_id);
            let attached_was_removed = app
                .dashboard
                .as_ref()
                .and_then(|d| d.attached_agent)
                .is_some_and(|id| to_remove.contains(&id));
            for id in to_remove {
                remove_agent_and_cleanup(app, id);
            }
            let mut effects = unregister_session_effect(Some(sid));
            if after == AfterSessionDelete::Dashboard {
                if let Some(d) = app.dashboard.as_mut() {
                    d.delete_confirm = None;
                    if attached_was_removed {
                        d.close_popup();
                    }
                    let selected_closed = d
                        .selected
                        .as_ref()
                        .is_some_and(|sel| closed_rows.contains(sel));
                    match (selected_closed, neighbor) {
                        (true, Some(n)) => d.focus_row(n),
                        (true, None) => d.focus_new_agent_button(),
                        _ => {}
                    }
                }
                if foreground {
                    super::dashboard::ensure_dashboard_state(app);
                    app.active_view = ActiveView::AgentDashboard;
                }
            } else if foreground && after == AfterSessionDelete::Welcome {
                effects.extend(dispatch_exit_session(app));
            }
            app.show_toast(delete_notice);
            effects
        }
        TaskResult::DeleteSessionFailed {
            source,
            session_id,
            error,
        } => {
            tracing::warn!(source, session_id = %session_id, error = %error, "session delete failed");
            if source != "unused-home" {
                app.show_toast(&format!("Couldn't delete session: {error}"));
            }
            vec![]
        }
        TaskResult::ContextInfoComplete {
            agent_id,
            session_id,
            info,
            nonce,
        } => handle_context_info_complete(app, agent_id, &session_id, info, nonce),
        TaskResult::ContextInfoFailed {
            agent_id,
            session_id,
            error,
            nonce,
        } => {
            let minimal = app.screen_mode.is_minimal();
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            if agent.session.session_id.as_ref() != Some(&session_id) {
                return vec![];
            }
            if let Some(state) = usage_modal_state_mut(agent) {
                if state.fetch_nonce == nonce {
                    state.context_error = Some(error);
                }
            } else if minimal {
                push_and_page_flip(
                    &mut agent.scrollback,
                    crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't load context info: {error}"
                    )),
                );
            }
            vec![]
        }
        TaskResult::SessionUsageComplete {
            agent_id,
            session_id,
            usage,
            nonce,
        } => handle_session_usage_result(
            app,
            agent_id,
            &session_id,
            crate::app::status_blocks::session_usage_block_text(&usage),
            nonce,
        ),
        TaskResult::SessionUsageFailed {
            agent_id,
            session_id,
            error,
            nonce,
        } => handle_session_usage_result(
            app,
            agent_id,
            &session_id,
            format!("Couldn't load session usage: {error}"),
            nonce,
        ),
        TaskResult::FeedbackComplete {
            agent_id,
            origin,
            outcome,
            trace_upload_token,
        } => {
            if matches!(origin, crate::app::actions::FeedbackSendOrigin::Immediate)
                && matches!(
                    outcome,
                    distill_shell::session::FeedbackOutcome::OutcomeUnknown
                )
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent
                    .scrollback
                    .push_block(
                        crate::scrollback::block::RenderBlock::system(
                            "Feedback was enqueued, but the response did not arrive in time. The send may still complete; do not resend it yet."
                                .to_owned(),
                        ),
                    );
            }
            if let crate::app::actions::FeedbackSendOrigin::Modal {
                submission_id,
                modal_id,
                is_draft,
            } = origin
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                let mut unknown_copy = None;
                let mut unknown_draft_request = None;
                if is_draft {
                    let has_matching_modal = agent
                        .feedback_modal
                        .as_ref()
                        .is_some_and(|modal| modal.matches_id(modal_id));
                    if has_matching_modal {
                        match outcome {
                            distill_shell::session::FeedbackOutcome::Submitted => {
                                agent.feedback_modal = None;
                                agent.scrollback.push_block(
                                    crate::scrollback::block::RenderBlock::system(
                                        super::notes::FEEDBACK_THANKS_NOTICE.to_owned(),
                                    ),
                                );
                            }
                            distill_shell::session::FeedbackOutcome::SubmittedCleanupFailed => {
                                if let Some(modal) = agent.feedback_modal.as_mut() {
                                    modal.mark_draft_cleanup_failed();
                                }
                            }
                            distill_shell::session::FeedbackOutcome::LocalOnly => {
                                if let Some(modal) = agent.feedback_modal.as_mut() {
                                    modal
                                        .mark_draft_send_error(
                                            "Feedback was saved locally but was not sent. The draft was kept."
                                                .to_owned(),
                                        );
                                }
                            }
                            distill_shell::session::FeedbackOutcome::OutcomeUnknown
                            | distill_shell::session::FeedbackOutcome::Other => {
                                if let Some(modal) = agent.feedback_modal.as_mut() {
                                    unknown_copy = modal.mark_draft_submit_unknown();
                                    unknown_draft_request = modal.take_pending_request();
                                }
                            }
                            _ => {
                                if let Some(modal) = agent.feedback_modal.as_mut() {
                                    unknown_copy = modal.mark_draft_submit_unknown();
                                    unknown_draft_request = modal.take_pending_request();
                                }
                            }
                        }
                    } else {
                        agent
                            .scrollback
                            .push_block(crate::scrollback::block::RenderBlock::system(
                                displaced_draft_feedback_notice(outcome).to_owned(),
                            ));
                    }
                } else if matches!(
                    outcome,
                    distill_shell::session::FeedbackOutcome::OutcomeUnknown
                ) {
                    agent
                        .scrollback
                        .push_block(
                            crate::scrollback::block::RenderBlock::system(
                                "Feedback was enqueued, but the response did not arrive in time. The send may still complete; do not resend it yet."
                                    .to_owned(),
                            ),
                        );
                }
                if let Some(text) = unknown_copy.as_deref() {
                    agent.copy_to_clipboard(text);
                }
                let mut effects = Vec::new();
                if let Some(request) = unknown_draft_request
                    && let Some(session_id) = agent.session.session_id.clone()
                {
                    effects.push(Effect::FeedbackDraftRequest {
                        agent_id,
                        session_id,
                        request,
                    });
                }
                let posted = matches!(
                    outcome,
                    distill_shell::session::FeedbackOutcome::Submitted
                        | distill_shell::session::FeedbackOutcome::SubmittedCleanupFailed
                );
                if posted {
                    if let Some(trace_upload_token) = trace_upload_token
                        && let Some(consent) =
                            agent.take_parked_feedback_trace_consent(submission_id)
                    {
                        agent.register_pending_trace_upload(submission_id);
                        effects.push(Effect::UploadFeedbackTrace {
                            agent_id,
                            session_id: agent_client_protocol::SessionId::new(consent.session_id),
                            submission_id: Some(submission_id),
                            intent: Some(consent.intent),
                            trace_upload_token: Some(trace_upload_token),
                        });
                    }
                } else {
                    let _ = agent.take_parked_feedback_trace_consent(submission_id);
                }
                return effects;
            }
            vec![]
        }
        TaskResult::FeedbackFailed {
            agent_id,
            origin,
            feedback_text,
            image_count,
            error,
        } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            let failure = format!("Couldn't send feedback: {error}");
            if let crate::app::actions::FeedbackSendOrigin::Modal {
                submission_id,
                modal_id,
                is_draft,
            } = origin
            {
                let _ = agent.take_parked_feedback_trace_consent(submission_id);
                if is_draft {
                    if let Some(modal) = agent
                        .feedback_modal
                        .as_mut()
                        .filter(|modal| modal.matches_id(modal_id))
                    {
                        modal.mark_draft_send_error(format!("{failure}. The draft was kept."));
                    } else {
                        agent
                            .scrollback
                            .push_block(crate::scrollback::block::RenderBlock::system(failure));
                    }
                    return vec![];
                }
            }
            super::notes::keep_unsent_feedback_report(
                agent,
                super::notes::UnsentFeedbackReport {
                    text: &feedback_text,
                    image_count,
                    failure: &failure,
                },
            );
            vec![]
        }
        TaskResult::FeedbackDraftListComplete {
            agent_id,
            modal_id,
            generation,
            result,
        } => {
            if let Some(modal) = app
                .agents
                .get_mut(&agent_id)
                .and_then(|agent| agent.feedback_modal.as_mut())
            {
                match result {
                    Ok(rows) => modal.apply_draft_list(modal_id, generation, rows),
                    Err(error) => modal.fail_draft_list(modal_id, generation, error),
                }
            }
            vec![]
        }
        TaskResult::FeedbackDraftLoadComplete {
            agent_id,
            load,
            result,
        } => {
            let agent = app.agents.get(&agent_id);
            let session_id = agent.and_then(|agent| agent.session.session_id.clone());
            let session_dir = agent.and_then(|agent| agent.session.local_session_dir());
            let request = app
                .agents
                .get_mut(&agent_id)
                .and_then(|agent| agent.feedback_modal.as_mut())
                .and_then(|modal| {
                    match result {
                        Ok(draft) => {
                            let draft_id = draft.id.clone();
                            let applied = modal.apply_draft_load(&load, draft);
                            if applied {
                                if let Some(session_dir) = session_dir.as_deref() {
                                    super::inline_feedback::attach_saved_draft_images(
                                        modal,
                                        session_dir,
                                        &draft_id,
                                    );
                                }
                                modal.recapture_write_baseline();
                            }
                        }
                        Err(error) => modal.fail_draft_load(&load, error),
                    }
                    modal.take_pending_request()
                });
            match (session_id, request) {
                (Some(session_id), Some(request)) => {
                    vec![Effect::FeedbackDraftRequest {
                        agent_id,
                        session_id,
                        request,
                    }]
                }
                _ => vec![],
            }
        }
        TaskResult::FeedbackDraftUpdateComplete {
            agent_id,
            update,
            result,
        } => {
            if let Some(modal) = app
                .agents
                .get_mut(&agent_id)
                .and_then(|agent| agent.feedback_modal.as_mut())
            {
                modal.apply_draft_update_complete(&update, result.err().as_deref());
            }
            vec![]
        }
        TaskResult::FeedbackDraftDeleteComplete {
            agent_id,
            delete,
            result,
        } => {
            if let Some(modal) = app
                .agents
                .get_mut(&agent_id)
                .and_then(|agent| agent.feedback_modal.as_mut())
            {
                match result {
                    Ok(()) => modal.apply_draft_delete(&delete),
                    Err(error) => modal.fail_draft_delete(&delete, error),
                }
            }
            vec![]
        }
        TaskResult::FeedbackTraceUploaded {
            agent_id,
            submission_id,
            error,
        } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            if let Some(submission_id) = submission_id
                && !agent.take_pending_trace_upload(submission_id)
            {
                return vec![];
            }
            if let Some(error) = error {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't upload a session trace; your feedback was still sent. {error}"
                    )));
            }
            vec![]
        }
        TaskResult::MemoryNoteSaved { agent_id, result } => {
            handle_memory_note_saved(app, agent_id, result)
        }
        TaskResult::MemoryNoteRewritten {
            agent_id,
            result,
            nonce,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Ok(markdown) = result
                && let Some(crate::views::modal::ActiveModal::RememberNoteReview {
                    ref mut enhanced_content,
                    ref mut cached_lines,
                    rewrite_nonce,
                    ..
                }) = agent.active_modal
                && rewrite_nonce == nonce
            {
                *enhanced_content = Some(markdown);
                *cached_lines = None;
            }
            vec![]
        }
        TaskResult::BundleStatusReady {
            has_cache,
            version,
            personas,
            roles,
            agents,
            skills,
            persona_details,
            role_details,
        } => {
            app.bundle_state.has_cache = has_cache;
            app.bundle_state.version = version.unwrap_or_default();
            app.bundle_state.personas = personas;
            app.bundle_state.roles = roles;
            app.bundle_state.agents = agents;
            app.bundle_state.skills = skills;
            app.bundle_state.persona_details = persona_details;
            app.bundle_state.role_details = role_details;
            vec![]
        }
        TaskResult::BundleStatusFailed { error } => {
            tracing::warn!(error = %error, "bundle status fetch failed");
            vec![]
        }
        TaskResult::CatalogEntryReady {
            kind,
            name,
            content,
        } => {
            if let ActiveView::Agent(id) = app.active_view
                && let Some(agent) = app.agents.get_mut(&id)
            {
                let title = format!("{kind}: {name}");
                agent.show_block_viewer(
                    crate::views::block_viewer::BlockViewerPane::for_plain_text(&title, &content),
                );
            }
            vec![]
        }
        TaskResult::CatalogEntryFailed { error } => {
            tracing::warn!(error = %error, "catalog entry fetch failed");
            if let ActiveView::Agent(id) = app.active_view
                && let Some(agent) = app.agents.get_mut(&id)
            {
                agent
                    .scrollback
                    .push_block(RenderBlock::system(format!("Couldn't load entry: {error}")));
            }
            vec![]
        }
        TaskResult::BtwResponse {
            agent_id,
            result,
            minimal_request_id,
            image_notice,
        } => handle_btw_response(app, agent_id, result, minimal_request_id, image_notice),
        TaskResult::InterjectQueued { .. } => vec![],
        TaskResult::RecapRequested {
            session_id,
            auto,
            error,
        } => {
            if let Some(error) = error {
                tracing::debug!(%error, "recap request failed");
                if !auto
                    && let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id.0)
                    && let Some(pending_id) = agent.pending_recap_entry.take()
                {
                    agent.scrollback.remove_entry(pending_id);
                    agent.show_toast(super::recap_unavailable_toast(
                        super::scrollback_has_user_messages(&agent.scrollback),
                    ));
                }
            }
            vec![]
        }
        TaskResult::InterjectFailed {
            agent_id,
            error,
            remaining,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                for (text, interjection_id, _blocks) in remaining.into_iter().rev() {
                    agent.self_interjection_ids.remove(&interjection_id);
                    if let Some(entry_id) =
                        agent.interjection_painted_blocks.remove(&interjection_id)
                    {
                        agent.scrollback.remove_entry(entry_id);
                    }
                    let images = agent
                        .interjection_retry_images
                        .remove(&interjection_id)
                        .unwrap_or_default();
                    let id = agent.session.next_queue_id;
                    agent.session.next_queue_id += 1;
                    agent
                        .session
                        .pending_prompts
                        .push_front(crate::app::agent::QueuedPrompt {
                            id,
                            text,
                            kind: crate::app::agent::QueueEntryKind::Prompt,
                            wire_blocks: None,
                            images,
                            display_as_skill: false,
                            chip_elements: Vec::new(),
                            skill_token_ranges: Vec::new(),
                            combined_texts: Vec::new(),
                        });
                }
                agent.show_toast(&format!("Interjection failed. Requeued: {error}"));
            }
            vec![]
        }
        TaskResult::AvailableCommandsRefreshed { agent_id, commands } => {
            if !commands.is_empty()
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent
                    .session
                    .replace_available_commands(commands, CommandCatalogSource::CommandsList);
                super::super::acp_handler::refresh_workflow_run_capabilities(agent);
            }
            vec![]
        }
        TaskResult::StatusLineCommandFinished { id, outcome } => {
            app.on_status_line_command_finished(id, outcome);
            vec![]
        }
        TaskResult::AuthCopyFeedbackTimeout { generation } => {
            if generation == app.auth_clipboard_feedback_generation {
                app.auth_clipboard_delivery = None;
            }
            vec![]
        }
        TaskResult::PaywallCheckTick => {
            let timed_out = app
                .paywall_check_started
                .is_some_and(|t| t.elapsed() >= PAYWALL_AUTO_CHECK_TIMEOUT);
            if !app.has_access() && !timed_out {
                vec![
                    Effect::CheckSubscription { verify: None },
                    Effect::SchedulePaywallCheck,
                ]
            } else {
                vec![]
            }
        }
        TaskResult::CheckSubscriptionComplete { verify, meta } => {
            handle_check_subscription_complete(app, verify, meta)
        }
        TaskResult::GateVerifyTimeout { generation } => handle_gate_verify_timeout(app, generation),
        TaskResult::CreditLimitRecheckComplete { agent_id, meta } => {
            handle_credit_limit_recheck_complete(app, agent_id, meta)
        }
        TaskResult::LogoutFailed(error) => {
            app.show_toast(&format!("Grok logout failed: {error}"));
            vec![]
        }
        TaskResult::LogoutComplete => {
            app.auth_state = if let Some(state) = app.provider_auth.as_mut() {
                state.grok = false;
                AuthState::Done
            } else {
                AuthState::Pending { error: None }
            };
            app.access_gate_shown_logged = false;
            app.announcement_cta_impressions_logged.clear();
            app.gate = None;
            app.pending_gate_verification = None;
            app.last_subscription_check_at = None;
            app.login_method_id = None;
            ensure_login_method(app);
            app.auth_clipboard_delivery = None;
            let effects = dispatch_exit_session(app);
            app.welcome_prompt_focused = false;
            effects
        }
        TaskResult::DeepSearchResults {
            host,
            generation,
            results,
            seq,
        } => handle_deep_search_results(
            app,
            PickerRequest {
                host,
                generation,
                seq,
            },
            results,
        ),
        TaskResult::RewindPointsLoaded { agent_id, points } => {
            handle_rewind_points_loaded(app, agent_id, points)
        }
        TaskResult::RewindPointsFailed { agent_id, error } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            agent.rewind_state = None;
            app.show_toast(&format!("Undo failed: {error}"));
            vec![]
        }
        TaskResult::RewindExecuteComplete { agent_id, response } => {
            dispatch_rewind_success(app, agent_id, response)
        }
        TaskResult::RewindExecuteFailed { agent_id, error } => {
            handle_rewind_execute_failed(app, agent_id, error)
        }
        TaskResult::SuggestionDebounceExpired {
            agent_id,
            generation,
        } => handle_suggestion_debounce_expired(app, agent_id, generation),
        TaskResult::PluginCtaDebounceExpired {
            agent_id,
            generation,
        } => handle_plugin_cta_debounce_expired(app, agent_id, generation),
        TaskResult::ShellSuggestionsLoaded {
            agent_id,
            response,
            request_text,
            request_cursor,
        } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            if agent.prompt_input_mode != crate::app::agent_view::PromptInputMode::Bash {
                return vec![];
            }
            let generation = response.generation;
            agent
                .prompt
                .suggestions
                .on_suggestions_loaded(response, &request_text, request_cursor);
            let text = agent.prompt.text().to_owned();
            agent.prompt.suggestions.set_last_request_text(&text);
            let mark = agent.pending_effects.len();
            if agent.prompt.suggestions.take_pending_tab(generation) {
                agent.shell_completion_tab();
            }
            agent.pending_effects.split_off(mark)
        }
        TaskResult::PromptSuggestionLoaded {
            agent_id,
            suggestion,
            generation,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .prompt
                    .prompt_suggestion
                    .on_loaded(suggestion, generation);
                agent.refresh_prompt_suggestion_gate();
                agent.log_prompt_suggestion_shown_if_visible();
            }
            vec![]
        }
        TaskResult::SettingPersisted { key, value } => {
            tracing::trace!(target: "settings", ?key, ?value, "setting persisted");
            if key == "onboarding_completed" {
                app.current_ui.onboarding_completed = true;
                app.onboarding = None;
                app.onboarding_resume = None;
            } else if let Some(state) = app.onboarding.as_mut() {
                match key {
                    "default_model" => state.finish_setting_persistence(
                        key,
                        true,
                        "Primary model saved. Runtime switching follows the existing /model path.",
                    ),
                    "tier_light" => state.finish_setting_persistence(
                        key,
                        true,
                        "Worker model saved. Continue when ready.",
                    ),
                    _ => {}
                }
            }
            vec![]
        }
        TaskResult::SettingPersistFailed {
            key,
            rollback_value,
            error,
        } => {
            if key == "onboarding_completed" {
                if let Some(state) = app.onboarding.as_mut() {
                    state.set_persistence_error(&error);
                }
                let scrubbed = scrub_error_for_toast(&error);
                app.show_toast(&format!(
                    "✗ Could not save onboarding completion: {scrubbed}"
                ));
                return vec![];
            }
            let rollback_effects = apply_setting_rollback(app, key, &rollback_value);
            tracing::warn!(target: "settings", ?key, ?rollback_value, %error, "setting persist failed; rolled back");
            let scrubbed = scrub_error_for_toast(&error);
            if let Some(state) = app.onboarding.as_mut() {
                match key {
                    "default_model" => state.finish_setting_persistence(
                        key,
                        false,
                        format!("Primary model was not saved: {scrubbed}. Try again."),
                    ),
                    "tier_light" => state.finish_setting_persistence(
                        key,
                        false,
                        format!("Worker model was not saved: {scrubbed}. Try again."),
                    ),
                    _ => {}
                }
            }
            app.show_toast(&format!("\u{2717} Could not save {key}: {scrubbed}"));
            rollback_effects
        }
        TaskResult::SettingPersistFailedBestEffort { key, error } => {
            tracing::warn!(
                target: "settings",
                ?key, %error,
                "setting persist failed (best-effort); in-memory state stays at optimistic value",
            );
            let scrubbed = scrub_error_for_toast(&error);
            if key == "tier_light"
                && let Some(state) = app.onboarding.as_mut()
            {
                state.finish_setting_persistence(
                    key,
                    false,
                    format!("Worker model was not saved: {scrubbed}. Try again."),
                );
            }
            app.show_toast(&format!("\u{2717} Could not save {key}: {scrubbed}"));
            vec![]
        }
    }
}
