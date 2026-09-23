// Modified for Distill by Samuel Fajreldines, 2026.
//! Dashboard dispatchers: attach, overlays, rows, renames, and permissions.

use super::ctx::{
    SwitchCause, surface_yolo_launch_block_notice, switch_to_agent,
    sync_active_permission_mode_mirror,
};
use super::dashboard_telemetry::{
    log_dashboard_attached, log_dashboard_closed, log_dashboard_launched, log_dashboard_opened,
};
use super::modes::{dispatch_cycle_mode_and_sync, set_yolo_mode, yolo_enable_blocked};
use super::permissions::resolve_permission_queue_transition;
use super::queue::{maybe_drain_queue, note_peek_page_flip};
use super::router::dispatch;
use super::session::lifecycle::{
    dispatch_new_session_inner_with_id, dispatch_new_worktree_session,
};
use super::session::load::dispatch_load_session;
use super::session::load::focus_if_session_already_open;
use super::session::modal::{dispatch_sessions_confirm_close, remove_agent_and_cleanup};
use super::turn::dispatch_cancel_turn;
use super::voice::{merge_prompt_with_voice_interim, voice_stop_on_submit};
use crate::app::actions::{Action, Effect, PermissionModeKind};
use crate::app::agent::{AgentId, DeferredModelSwitch};
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView, DashboardReturn, TrustState};
use crate::app::cancel_latency::CancelOrigin;
use agent_client_protocol as acp;
use distill_telemetry::events::CancellationScope;

/// Keeps v1 config layout separate from v2 workspace layout.
fn dashboard_state_for_mode(app: &mut AppView) -> crate::views::dashboard::DashboardState {
    use crate::views::dashboard::{DashboardState, load_persisted};
    if app.workspace_dashboard_enabled {
        return DashboardState::new();
    }
    if app.dashboard_persisted.is_none() {
        app.dashboard_persisted = load_persisted();
    }
    let persisted = app
        .dashboard_persisted
        .clone()
        .unwrap_or_else(crate::views::dashboard::PersistedDashboard::defaults);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    DashboardState::from_persisted(&persisted, &resolver)
}

pub(super) fn rebind_workspace_identities(
    app: &mut AppView,
    old: &crate::views::dashboard::SessionIdResolver,
) {
    let workspace = app.workspace_membership.view();
    let new = crate::views::dashboard::SessionIdResolver::from_agents_and_workspace(
        &app.agents,
        workspace.as_ref(),
    );
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.rebind_workspace_identities(old, &new, &mut app.agents);
    }
}

pub(super) struct WorkspaceIdentityRebind(Option<crate::views::dashboard::SessionIdResolver>);

impl WorkspaceIdentityRebind {
    pub(super) fn capture(app: &AppView) -> Self {
        Self(
            (app.workspace_dashboard_enabled && app.dashboard.is_some()).then(|| {
                let workspace = app.workspace_membership.view();
                crate::views::dashboard::SessionIdResolver::from_agents_and_workspace(
                    &app.agents,
                    workspace.as_ref(),
                )
            }),
        )
    }

    pub(super) fn apply(self, app: &mut AppView) {
        if let Some(old) = self.0.as_ref() {
            rebind_workspace_identities(app, old);
        }
    }

    #[cfg(test)]
    pub(super) fn is_active(&self) -> bool {
        self.0.is_some()
    }
}

pub(super) fn ensure_dashboard_state(app: &mut AppView) {
    if app.dashboard.is_some() {
        return;
    }
    let mut state = dashboard_state_for_mode(app);
    let workspace = app.workspace_membership.view();
    state.gc_stale_refs(&dashboard_alive_fn(&app.agents, workspace.as_ref()));
    state.adopt_slash_mru(app.slash_mru.clone());
    state.adopt_command_tags(app.command_tags.clone());
    state.set_screen_mode(app.screen_mode);
    state.set_recap_visible(app.session_recap_available);
    state.set_voice_visible(app.voice_mode_enabled);
    state.set_restricted_commands(&app.tier_restricted_commands);
    let billing = app.grok_billing_surface_visible();
    let usage_cmd = app.usage_command_visible();
    state
        .dispatch
        .slash_controller
        .set_billing_surface_visible(billing);
    state
        .dispatch
        .slash_controller
        .set_usage_command_visible(usage_cmd);
    state
        .peek_reply
        .slash_controller
        .set_billing_surface_visible(billing);
    state
        .peek_reply
        .slash_controller
        .set_usage_command_visible(usage_cmd);
    app.dashboard = Some(state);
}

/// Configure the dashboard for display: snapshot app-wide state (cwd, models, plugins, permission mode) and clear the staged dispatch settings.
/// Shared by `dispatch_open_dashboard` and the overlay-cycle path; a no-op when the dashboard is unallocated.
fn configure_dashboard_state(app: &mut AppView) {
    let bootstrap_commands = app.bootstrap_acp_commands.clone();
    let models = app.models.clone();
    let disable_plugins = app.appearance.disable_plugins;
    let default_yolo = app.default_yolo;
    let default_auto = app.auto_mode_gate
        && !default_yolo
        && app.current_ui.permission_mode.as_deref() == Some("auto");
    let cwd = app.cwd.clone();
    let cwd_has_git_ancestor = app.cwd_has_git_ancestor;
    let has_agents = !app.agents.is_empty();
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
        d.location_picker = None;
        d.usage_modal = None;
        d.cwd = cwd.clone();
        d.cwd_has_git_ancestor = cwd_has_git_ancestor;
        d.dispatch_worktree = false;
        d.worktree_dialog = None;
        d.pending_worktree_prompt = None;
        d.pending_worktree_attach = false;
        d.focus_new_agent_button();
        d.list_focused = has_agents;
        d.dispatch.file_search.retarget(&cwd);
        // Tool gating disabled (None): the dashboard has no agent toolset.
        d.dispatch
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!disable_plugins);
        d.dispatch
            .sync_acp_commands(&bootstrap_commands, None, &models);
        d.models = models;
        d.pending_model = None;
        d.pending_mode = if default_yolo {
            crate::views::dashboard::DashboardDispatchMode::AlwaysApprove
        } else if default_auto {
            crate::views::dashboard::DashboardDispatchMode::Auto
        } else {
            crate::views::dashboard::DashboardDispatchMode::Normal
        };
    }
}

/// Open the dashboard view. Respects the [`crate::views::dashboard::dashboard_enabled`] feature flag (env var override and persisted setting).
/// The dashboard is independent of leader mode: it renders local sessions from `app.agents`.
/// When connected via a leader it also polls the leader roster (see the roster-poll gate in the event loop).
pub(super) fn dispatch_open_dashboard(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::dashboard_enabled;

    if !dashboard_enabled() {
        app.show_toast("Agent dashboard is disabled in this configuration");
        return vec![];
    }
    // Gate behind auth. Until login completes the backend rejects new sessions, and activating the dashboard view visually dismisses the auth UI.
    // Toast and stay put
    if !matches!(app.auth_state, crate::app::app_view::AuthState::Done) {
        app.show_toast("Sign in to open the dashboard");
        return vec![];
    }
    // Same for the terms notice: opening the dashboard would dismiss it unanswered.
    if matches!(
        app.consent_state,
        crate::app::consent::ConsentState::Pending { .. }
    ) {
        app.show_toast("Answer the terms notice to open the dashboard");
        return vec![];
    }
    // Same rationale for folder trust: opening the dashboard would visually dismiss the trust question with the folder still unanswered
    // Toast and stay put (mirrors the auth gate above) so the question is resolved first
    if matches!(app.trust_state, TrustState::Pending { .. }) {
        app.show_toast("Answer the folder-trust question to open the dashboard");
        return vec![];
    }
    // Opening from the dashboard view itself just closes: Ctrl+\ is a single-shot toggle between the agent view and the dashboard view
    // Closing only the popup stays bound to Esc inside the popup mouse/key cascade
    if matches!(app.active_view, ActiveView::AgentDashboard) {
        return dispatch_exit_dashboard(app);
    }
    // Stamp return target for this visit (clears any prior leftover).
    app.dashboard_return = match app.active_view {
        ActiveView::Agent(id) => Some(DashboardReturn::Agent(id)),
        ActiveView::Welcome => Some(DashboardReturn::Welcome),
        ActiveView::AgentDashboard => None,
    };
    // `app.dashboard.is_some()` means it was initialised before; keep the user's filter, dispatch text, hover, and selection across the reopen
    // Otherwise seed from persisted state
    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
    } else if let Some(d) = app.dashboard.as_mut() {
        let workspace = app.workspace_membership.view();
        d.gc_stale_refs(&dashboard_alive_fn(&app.agents, workspace.as_ref()));
        d.set_recap_visible(app.session_recap_available);
        d.set_voice_visible(app.voice_mode_enabled);
        d.set_restricted_commands(&app.tier_restricted_commands);
    }
    // Refresh each local agent's git context (branch, worktree, label) from disk; the last `git_head_changed` notification may be stale
    // Runs on open (a deliberate user action), never per frame
    // `compute_cwd_git_info` returns `None` for non-repo cwds, so existing values stay rather than being cleared
    let agent_cwds: Vec<(AgentId, std::path::PathBuf)> = app
        .agents
        .iter()
        .map(|(id, a)| (*id, a.session.cwd.clone()))
        .collect();
    for (id, cwd) in agent_cwds {
        if let Some(info) = crate::git_info::compute_cwd_git_info(&cwd)
            && let Some(agent) = app.agents.get_mut(&id)
        {
            agent.current_branch = info.branch;
            agent.is_worktree = info.is_worktree || agent.session.is_worktree;
            agent.main_repo = info.main_repo;
            agent.worktree_label = info.worktree_label;
        }
    }
    // Open shows only the dashboard; Enter on a row switches to the agent's fullscreen view (`dispatch_dashboard_attach`)
    // Always open in new-session mode: focus the `[+ New Agent]` button with no row selected, so a typed prompt dispatches a brand new agent
    // Pre-seeding `selected` used to silently arm reply, and the reply path never clears it, so every later dispatch stuck to the same agent
    configure_dashboard_state(app);
    app.active_view = ActiveView::AgentDashboard;
    log_dashboard_opened(app);
    if app.workspace_dashboard_enabled {
        app.dashboard_sessions_loading = app.workspace_membership.snapshot().is_none();
        crate::app::workspace_sync::activate(app);
        return crate::app::workspace_sync::drain(app);
    }
    app.dashboard_sessions_loading = true;
    if app.leader_mode {
        return vec![Effect::FetchRoster];
    }
    vec![Effect::FetchDashboardSessions]
}

fn dashboard_alive_fn<'a>(
    agents: &'a indexmap::IndexMap<AgentId, AgentView>,
    workspace: Option<&'a crate::app::workspace_layout::WorkspaceView>,
) -> impl Fn(&crate::views::dashboard::DashboardRowId) -> bool + 'a {
    move |id| match id {
        crate::views::dashboard::DashboardRowId::TopLevel(a) => agents.contains_key(a),
        crate::views::dashboard::DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get(parent)
            .is_some_and(|a| a.subagent_sessions.contains_key(child_session_id)),
        crate::views::dashboard::DashboardRowId::Workspace { session_id } => {
            workspace.is_some_and(|workspace| {
                workspace.members.iter().any(|member| {
                    matches!(member.kind, distill_dashboard_store::MemberKind::Build)
                        && member.session_id.as_ref() == session_id
                })
            })
        }
        crate::views::dashboard::DashboardRowId::Roster { .. } => false,
    }
}

pub(super) fn dispatch_exit_dashboard(app: &mut AppView) -> Vec<Effect> {
    app.dashboard_session_picker = None;
    // Also clear any popup attachment so a fresh reopen lands on the row list, not on a stale popup (`close_popup()` clears the hit rects too.)
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
        // Dashboard state is preserved across reopen; clear a leftover exit alias so the next Enter does not quit
        if crate::slash::commands::exit::is_exit_alias(d.dispatch.text()) {
            d.dispatch.set_text("");
            d.error_toast = None;
        }
    }
    log_dashboard_closed(app);
    let preferred = app.dashboard_return.take();
    if matches!(preferred, Some(DashboardReturn::Welcome)) {
        // Keep the unused home session hidden; do not `show_welcome` (that clears `home_session_agent` and would orphan the husk).
        // clears `home_session_agent` and would orphan the husk).
        app.active_view = ActiveView::Welcome;
        return vec![];
    }
    let preferred =
        preferred.filter(|t| t.agent_id().is_some_and(|id| app.agents.contains_key(&id)));
    // Overlay chrome only when the preferred target is still alive. Do not fall back to the unused home husk (insertion-order first).
    // fall back to the unused home husk (insertion-order first).
    let (return_id, rearm_overlay) = match preferred {
        Some(t) => (t.agent_id(), t.is_overlay()),
        None => (
            app.agents
                .keys()
                .copied()
                .find(|id| app.home_session_agent != Some(*id)),
            false,
        ),
    };
    if let Some(id) = return_id {
        app.active_view = ActiveView::Agent(id);
        if rearm_overlay {
            rearm_session_overlay(app, id);
        }
        surface_yolo_launch_block_notice(app, id);
    } else {
        app.active_view = ActiveView::Welcome;
    }
    vec![]
}

/// Restore session-overlay chrome (`attached_agent` and the row cursor).
/// Keeps a live subagent takeover; otherwise clears it and selects TopLevel.
fn rearm_session_overlay(app: &mut AppView, id: AgentId) {
    use crate::views::dashboard::DashboardRowId;
    let live_child = app.agents.get(&id).and_then(|a| {
        a.active_subagent
            .as_ref()
            .filter(|c| a.subagent_sessions.contains_key(*c))
            .cloned()
    });
    let row = match live_child {
        Some(child_session_id) => DashboardRowId::Subagent {
            parent: id,
            child_session_id,
        },
        None => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.close_subagent_fullscreen();
            }
            DashboardRowId::TopLevel(id)
        }
    };
    if let Some(d) = app.dashboard.as_mut() {
        d.focus_row(row);
        d.attached_agent = Some(id);
    }
}

pub(super) fn dispatch_dashboard_open_session_picker(app: &mut AppView) -> Vec<Effect> {
    use crate::views::session_picker::SourceFilter;
    use crate::views::session_picker_surface::{SessionPickerHost, SessionPickerSurface};

    if !app.workspace_dashboard_enabled
        || !matches!(app.active_view, ActiveView::AgentDashboard)
        || app.dashboard_session_picker.is_some()
    {
        return vec![];
    }

    let cwd = app
        .dashboard
        .as_ref()
        .map_or_else(|| app.cwd.clone(), |dashboard| dashboard.cwd.clone());
    let generation = app.alloc_picker_generation();
    let mut surface = SessionPickerSurface::new(generation);
    surface.source_filter = SourceFilter::Local;
    surface.loading = true;
    surface.list_seq += 1;
    let seq = surface.list_seq;
    let headless_policy = surface.source_filter.headless_policy();
    app.dashboard_session_picker = Some(surface);

    vec![Effect::FetchSessionList {
        host: SessionPickerHost::Dashboard,
        cwd_override: Some(cwd),
        generation,
        query: None,
        seq,
        kind_filter: Some(vec!["build".to_owned()]),
        headless_policy,
    }]
}

pub(super) fn dispatch_dashboard_close_session_picker(app: &mut AppView) -> Vec<Effect> {
    if let Some(surface) = app.dashboard_session_picker.as_mut() {
        surface.state.hit_areas = None;
    }
    app.dashboard_session_picker = None;
    vec![]
}

fn dispatch_dashboard_load_local_build(
    app: &mut AppView,
    session_id: String,
    cwd_hint: Option<std::path::PathBuf>,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let resolved = cwd_hint
        .and_then(|cwd| {
            distill_shell::session::resolve_local_session(&session_id, &cwd.to_string_lossy())
                .map(|resolved_id| (resolved_id, cwd))
        })
        .or_else(|| {
            distill_shell::session::resolve_local_session_any_cwd(&session_id)
                .map(|cwd| (session_id, std::path::PathBuf::from(cwd)))
        });

    let Some((resolved_id, resolved_cwd)) = resolved else {
        app.show_toast("Session not found locally");
        return vec![];
    };

    #[cfg(feature = "local-workspace")]
    {
        app.welcome_history_load_as_build = true;
    }
    if let Some(existing_id) = focus_if_session_already_open(app, resolved_id.as_str(), false) {
        #[cfg(feature = "local-workspace")]
        {
            app.welcome_history_load_as_build = false;
        }
        crate::app::workspace_sync::allow_loaded_session(app, &resolved_id);
        log_dashboard_attached(&DashboardRowId::TopLevel(existing_id));
        return vec![];
    }

    let effects = dispatch_load_session(app, resolved_id, Some(resolved_cwd), false);
    if let Some(new_id) = effects.iter().find_map(|effect| match effect {
        Effect::LoadSession { agent_id, .. } => Some(*agent_id),
        _ => None,
    }) {
        if let Some(dashboard) = app.dashboard.as_mut() {
            dashboard.focus_row(DashboardRowId::TopLevel(new_id));
            dashboard.attached_agent = Some(new_id);
        }
        log_dashboard_attached(&DashboardRowId::TopLevel(new_id));
    }
    effects
}

pub(super) fn dispatch_dashboard_pick_session(app: &mut AppView, index: usize) -> Vec<Effect> {
    let entry = app
        .dashboard_session_picker
        .as_ref()
        .and_then(|surface| surface.entries.as_ref())
        .and_then(|entries| entries.get(index))
        .cloned();
    app.dashboard_session_picker = None;

    let Some(entry) = entry else {
        return vec![];
    };
    let cwd_hint = (!entry.cwd.is_empty()).then(|| std::path::PathBuf::from(entry.cwd));
    dispatch_dashboard_load_local_build(app, entry.id, cwd_hint)
}

pub(super) fn dispatch_dashboard_attach(
    app: &mut AppView,
    id: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;
    // Attach switches to the fullscreen agent view AND signals session-overlay mode via `attached_agent`
    // Input goes straight to the agent because `active_view = Agent(id)`, so Enter, Shift+Tab, etc. work as in any regular agent view.
    // Attaching re-targets the overlay: a stop-confirm armed on a previously attached agent must not follow the user in The legacy popup row-click path reaches here without a key press, so the key-press disarm never ran
    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
    }
    match id {
        DashboardRowId::TopLevel(agent_id) => {
            if !app.agents.contains_key(&agent_id) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Session no longer exists");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                // Drop any prior subagent takeover so the agent view paints the parent, not whatever subagent the user last opened fullscreen
                agent.close_subagent_fullscreen();
            }
            if let Some(d) = app.dashboard.as_mut() {
                // `focus_row`, not a bare `selected` assignment, so the section cursor and button focus are cleared
                // Exactly one cursor target stays active
                d.focus_row(DashboardRowId::TopLevel(agent_id));
                // Signal session-overlay mode: render the agent wrapped in the bordered frame with cycle/close affordances at the top right
                d.attached_agent = Some(agent_id);
            }
            switch_to_agent(app, agent_id, SwitchCause::Picker);
            log_dashboard_attached(&DashboardRowId::TopLevel(agent_id));
            surface_yolo_launch_block_notice(app, agent_id);
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            // If the subagent finished or disappeared between row build and attach, toast and stay put rather than silently switching to the parent
            let alive = app
                .agents
                .get(&parent)
                .is_some_and(|a| a.subagent_sessions.contains_key(&child_session_id));
            if !alive {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Subagent no longer running");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&parent) {
                agent.open_subagent_fullscreen(child_session_id.clone());
            }
            let row_id = DashboardRowId::Subagent {
                parent,
                child_session_id,
            };
            if let Some(d) = app.dashboard.as_mut() {
                d.focus_row(row_id.clone());
                d.attached_agent = Some(parent);
            }
            app.active_view = ActiveView::Agent(parent);
            log_dashboard_attached(&row_id);
            surface_yolo_launch_block_notice(app, parent);
        }
        DashboardRowId::Roster { session_id } => {
            // A roster row is a leader-hosted session this client is not locally attached to Attaching issues an ACP `session/load`
            // The leader adds this client to the session's subscriber set (replay and live broadcast) instead of stealing ownership
            // Inserting a leader-hosted, possibly cross-cwd session into this cwd's recent list would be wrong

            // Resolve cwd and origin first; the origin kind also feeds the focus-if-already-open check below
            let (session_cwd, conversation_entry) = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id)
                .map(|e| {
                    let is_conversation = e.origin.kind == "conversation";
                    (
                        // Conversation rows have no cwd to re-home into.
                        (!is_conversation).then(|| std::path::PathBuf::from(&e.cwd)),
                        is_conversation,
                    )
                })
                .unwrap_or((None, false));

            // Already local (e.g. double-click after the row converted): focus only.
            if let Some(existing_id) =
                focus_if_session_already_open(app, session_id.as_str(), conversation_entry)
            {
                log_dashboard_attached(&DashboardRowId::TopLevel(existing_id));
                return vec![];
            }

            // Mirror the picker resume path: allocate a new local agent and emit `Effect::LoadSession` (strict load)
            // `dispatch_load_session` already sets `app.active_view` to the new agent
            // Conversation-origin roster rows carry the conversation-entry bit, so they take the direct chat load, never local/GCS resolution
            let effects = dispatch_load_session(app, session_id, session_cwd, conversation_entry);
            if let Some(new_id) = effects.iter().find_map(|e| match e {
                Effect::LoadSession { agent_id, .. } => Some(*agent_id),
                _ => None,
            }) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.focus_row(DashboardRowId::TopLevel(new_id));
                    d.attached_agent = Some(new_id);
                }
                log_dashboard_attached(&DashboardRowId::TopLevel(new_id));
            }
            return effects;
        }
        DashboardRowId::Workspace { session_id, .. } => {
            let cwd_hint = app.workspace_membership.snapshot().and_then(|snapshot| {
                snapshot
                    .members
                    .iter()
                    .find(|member| member.session_id.as_ref() == session_id)
                    .and_then(|member| member.cwd.as_deref())
                    .map(std::path::PathBuf::from)
            });
            return dispatch_dashboard_load_local_build(app, session_id, cwd_hint);
        }
    }
    vec![]
}

/// Exit the dashboard's session-overlay: dismiss the bordered chrome and return to the dashboard view.
pub(super) fn dispatch_dashboard_overlay_exit(app: &mut AppView) -> Vec<Effect> {
    // Capture before close_popup() clears attached_agent.
    if let ActiveView::Agent(id) = app.active_view {
        app.dashboard_return = Some(DashboardReturn::Overlay(id));
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
        // The modal returns `Unchanged` for control chords, so Ctrl+\ can reach the overlay with it still open; don't bring it back
        d.usage_modal = None;
    }
    // Leaving the overlay by mouse (`[Dashboard]` click) doesn't pass through the key-press disarm in `handle_input`
    // An armed stop-confirm would survive the exit and let a later Ctrl+X on the dashboard close a session with a single press
    // The confirm is bound to this overlay and this agent; clear it on the way out
    clear_pending_overlay_stop(app);
    app.active_view = ActiveView::AgentDashboard;
    vec![]
}

/// Disarm a pending overlay stop-confirm (see [`dispatch_dashboard_overlay_stop`]).
/// Called from every overlay navigation that can happen WITHOUT a key press (mouse clicks on `[Dashboard]` / `‹` / `›`).
/// Key presses already disarm via the pending-action fast path in `AppView::handle_input`.
fn clear_pending_overlay_stop(app: &mut AppView) {
    if app
        .pending_action
        .as_ref()
        .is_some_and(|p| matches!(p.action, Action::DashboardOverlayStop))
    {
        app.pending_action = None;
    }
}

/// V1 cancels foreground work or confirms a local close. V2 stops all actionable work
/// before confirming archive, and silently blocks archive while replay/loading is busy.
pub(super) fn dispatch_dashboard_overlay_stop(app: &mut AppView) -> Vec<Effect> {
    let Some(id) = app.dashboard.as_ref().and_then(|d| d.attached_agent) else {
        return vec![];
    };
    if app.workspace_dashboard_enabled {
        let Some(readiness) = app.agents.get(&id).map(dashboard_stop_readiness) else {
            return vec![];
        };
        return match readiness {
            DashboardStopReadiness::Stoppable => {
                if app
                    .agents
                    .get_mut(&id)
                    .is_some_and(|agent| agent.arm_dashboard_stop())
                {
                    dispatch_cancel_turn(app)
                } else {
                    app.agents
                        .get_mut(&id)
                        .and_then(stop_top_level_activity)
                        .unwrap_or_default()
                }
            }
            DashboardStopReadiness::Busy => vec![],
            DashboardStopReadiness::Archiveable | DashboardStopReadiness::LocallyClosable => {
                archive_dashboard_row(app, crate::views::dashboard::DashboardRowId::TopLevel(id))
            }
        };
    }
    if app
        .agents
        .get_mut(&id)
        .is_some_and(|agent| agent.arm_dashboard_stop())
    {
        return dispatch_cancel_turn(app);
    }
    // Land on the dashboard BEFORE closing: `dispatch_sessions_confirm_close` only re-targets the active view when it shows the closed agent
    // Switching first keeps it from picking a fallback agent
    // A refused close (e.g. "Cannot close the only session") also returns to the dashboard; `AppView::show_toast` shows the refusal there.
    let neighbor =
        dashboard_neighbor_row(app, &crate::views::dashboard::DashboardRowId::TopLevel(id));
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
        d.usage_modal = None;
    }
    app.active_view = ActiveView::AgentDashboard;
    let effects = dispatch_sessions_confirm_close(app, id);
    if !app.agents.contains_key(&id)
        && let Some(d) = app.dashboard.as_mut()
    {
        // Move the cursor onto the neighbouring row, or the always-present `+ New Agent` button when none is left
        // Both go through the focus helpers so the "exactly one cursor active" invariant holds
        // A bare `selected = None` would leave no cursor and drop the footer into its defensive fallback
        match neighbor {
            Some(n) => d.focus_row(n),
            None => d.focus_new_agent_button(),
        }
        // Success feedback; the close path itself is silent
        // Mirrors the "Session closed" toast the render path shows when an attached agent disappears externally (`app_view.rs`, AgentDashboard arm)
        // Don't clobber a refusal toast planted by the close path above.
        if d.error_toast.is_none() {
            d.error_toast = Some(format!("{} Session closed", crate::glyphs::check_mark()));
        }
    }
    effects
}

/// Toggle worktree-dispatch mode for the dashboard (bound to Ctrl+W).
/// When the mode is on, the next dispatch spawns the agent in a fresh git worktree and the `[+ New Agent]` button reads `[+ New Worktree]`.
/// Worktrees require a git repo, so outside one the toggle no-ops with a toast and never leaves the dashboard in worktree mode.
pub(super) fn dispatch_dashboard_toggle_worktree(app: &mut AppView) -> Vec<Effect> {
    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        if has_git {
            d.dispatch_worktree = !d.dispatch_worktree;
        } else {
            d.dispatch_worktree = false;
            d.set_error_toast("Not a git repository: worktrees need one");
        }
    }
    vec![]
}

/// Toggle auto-approve (YOLO mode) on the selected dashboard row's owning agent.
/// Subagents inherit their parent's mode, so a subagent selection routes to the parent.
/// This keeps the drain / persist / toast logic in a single code path instead of duplicating it.
pub(super) fn dispatch_dashboard_toggle_auto_approve(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    let Some(selected) = d.selected.as_ref() else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Select a session first");
        }
        return vec![];
    };
    let agent_id = match selected {
        DashboardRowId::TopLevel(id) => *id,
        DashboardRowId::Subagent { parent, .. } => *parent,
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }
    let agent = app.agents.get(&agent_id).expect("checked above");
    let new = !agent.session.yolo_mode;

    // Managed policy pins always-approve off; toast on the dashboard's own error slot (the inner gate's agent toast is invisible from here)
    if let Some(warning) = yolo_enable_blocked(app, new) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(warning);
        }
        return vec![];
    }

    // Temporarily borrow active_view so `set_yolo_mode` targets the dashboard's selected agent rather than whichever view is active
    // Restored before returning
    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = set_yolo_mode(app, new);
    app.active_view = saved_view;
    effects
}

fn snapshot_prompt_widget(
    prompt: &mut crate::views::prompt_widget::PromptWidget,
    text: String,
) -> crate::views::prompt_widget::StashedPrompt {
    if prompt.text() == text
        || !prompt.images.is_empty()
        || !prompt.textarea().elements().is_empty()
    {
        prompt.stash().with_transformed_text(text)
    } else {
        crate::views::prompt_widget::StashedPrompt::from_submission(text, Vec::new(), Vec::new())
    }
}

/// Open the worktree-label dialog and stash the dispatch prompt until confirm.
fn open_dashboard_worktree_dialog(
    app: &mut AppView,
    prompt: Option<String>,
    attach: bool,
) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        d.pending_worktree_prompt =
            prompt.map(|text| snapshot_prompt_widget(&mut d.dispatch, text));
        d.pending_worktree_attach = attach;
        d.worktree_dialog = Some(crate::app::app_view::NewWorktreeDialogState::new());
        d.dispatch.set_text("");
        d.error_toast = None;
    }
    vec![]
}

fn resolve_pending_dispatch_mode(
    app: &mut AppView,
) -> (
    crate::views::dashboard::DashboardDispatchMode,
    Option<&'static str>,
) {
    use crate::views::dashboard::DashboardDispatchMode;

    let staged = app
        .dashboard
        .as_ref()
        .map(|dashboard| dashboard.pending_mode)
        .unwrap_or_default();
    let (resolved, warning) = match staged {
        DashboardDispatchMode::Auto if !app.auto_mode_gate => (DashboardDispatchMode::Normal, None),
        DashboardDispatchMode::AlwaysApprove if app.yolo_policy_block.is_some() => {
            (DashboardDispatchMode::Normal, app.yolo_policy_block)
        }
        _ => (staged, None),
    };
    if resolved != staged
        && let Some(dashboard) = app.dashboard.as_mut()
    {
        dashboard.pending_mode = resolved;
    }
    (resolved, warning)
}

fn permission_mode_for_dispatch(
    mode: crate::views::dashboard::DashboardDispatchMode,
) -> PermissionModeKind {
    use crate::views::dashboard::DashboardDispatchMode;
    match mode {
        DashboardDispatchMode::Normal | DashboardDispatchMode::Plan => PermissionModeKind::Ask,
        DashboardDispatchMode::Auto => PermissionModeKind::Auto,
        DashboardDispatchMode::AlwaysApprove => PermissionModeKind::AlwaysApprove,
    }
}

fn set_create_permission_mode(
    effects: &mut [Effect],
    mode: crate::views::dashboard::DashboardDispatchMode,
) {
    let mode = Some(permission_mode_for_dispatch(mode));
    for effect in effects {
        match effect {
            Effect::CreateSession {
                permission_mode_override,
                ..
            }
            | Effect::CreateWorktreeSession {
                permission_mode_override,
                ..
            } => *permission_mode_override = mode,
            _ => {}
        }
    }
}

/// Create a new session AND switch into its detail view.
/// Routed from the `+ New Agent` button, or Enter on an empty prompt while the button is focused.
/// Mirrors `dispatch_dashboard_dispatch`'s new-session arm with `attach=true`, minus the prompt enqueue.
pub(super) fn dispatch_dashboard_create_new_agent_with_detail(app: &mut AppView) -> Vec<Effect> {
    let _ = voice_stop_on_submit(app);
    // Worktree mode on in a git repo: open the label dialog (it spawns the agent in a fresh worktree on confirm) instead of a plain session
    // The button opens the detail view, so confirm attaches (`attach = true`)
    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, None, /* attach */ true);
    }
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    log_dashboard_launched("new_agent_button");
    let (new_id, mut effects) = dispatch_new_session_inner_with_id(app, model_id, false);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(agent) = app.agents.get_mut(&new_id) {
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        // Clear the dispatch input even though nothing is enqueued: a stray paste while the button is focused shouldn't survive the view switch
        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;
        // Snap the cursor onto the new row so the overlay's `i/n` indicator matches the active view
        // The chrome's `‹` / `›` cycle then anchors on the right starting point
        d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
        d.attached_agent = Some(new_id);
    }
    app.dashboard_return = Some(DashboardReturn::Overlay(new_id));
    app.active_view = ActiveView::Agent(new_id);
    sync_active_permission_mode_mirror(app);
    surface_yolo_launch_block_notice(app, new_id);
    effects
}

/// Open the dashboard's shortcuts cheatsheet modal.
/// Builds the entry list from the registry, scoped to the `DashboardFocused` and `Always` contexts.
/// Mirrors `ActionId::ShortcutsHelp`'s agent-view handler.
pub(super) fn dispatch_dashboard_open_shortcuts_help(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    if d.shortcuts_modal.is_some() {
        // Idempotent: re-pressing Ctrl+. while the modal is open is a no-op rather than blowing away the user's search query and scroll position.
        return;
    }
    use crate::actions::When;
    let contexts = [When::DashboardFocused, When::Always];
    let entries = crate::views::shortcuts_help::build_entries(
        &contexts,
        &app.registry,
        /* vim_mode */ false,
    );
    let state = crate::views::shortcuts_help::build_initial_picker_state(&entries);
    d.shortcuts_modal = Some(Box::new(crate::views::dashboard::ShortcutsModalState {
        entries,
        state,
        window: Default::default(),
        filter_active: false,
        collapsed_sections: crate::views::shortcuts_help::default_collapsed(),
        expanded_ids: std::collections::HashSet::new(),
        mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
    }));
}

/// Short display label for a directory in the location picker: the basename (truncated), or `~` for the home directory itself.
fn location_picker_label(path: &std::path::Path) -> String {
    if distill_dirs::home_dir().is_some_and(|h| h == path) {
        return "~".to_string();
    }
    let raw = path.file_name().and_then(|n| n.to_str()).unwrap_or("/");
    crate::render::line_utils::truncate_str(raw, 30)
}

/// Resolve a raw location-picker / `/cd` path string to an absolute path, expanding a leading `~` and joining relative paths against `cwd`.
/// Returns `None` for empty input or when `~` can't be expanded. The caller validates that the result is a directory.
pub(super) fn resolve_location_input(
    input: &str,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded: std::path::PathBuf = if trimmed == "~" {
        distill_dirs::home_dir()?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        distill_dirs::home_dir()?.join(rest)
    } else {
        std::path::PathBuf::from(trimmed)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(cwd.join(expanded))
    }
}

/// Open the dashboard's location picker.
/// Seeds the candidate list with the current cwd (marked `(current)`) followed by recent project directories from session history.
/// Idempotent: a no-op if the picker is already open or the dashboard isn't active.
pub(super) fn dispatch_dashboard_open_location_picker(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::{LocationCandidate, LocationPickerState};

    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        // `/cd` reached from a non-dashboard surface; the location picker is a dashboard affordance, so guide the user there
        // Gate on the dashboard being the foreground view
        // `app.dashboard.is_some()` stays true for the rest of the session once the dashboard has been opened even once
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }
    // Idempotent: re-triggering while open keeps the current query
    if app
        .dashboard
        .as_ref()
        .is_some_and(|d| d.location_picker.is_some())
    {
        return vec![];
    }

    let cwd = app.cwd.clone();
    // The recent-dirs source is async; block the current runtime thread briefly to collect it.
    let recent = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(crate::recent_dirs::collect_recent_dirs(10))
    });

    // Worktree label index keyed by root path, built once and reused to tag both recents and live directory suggestions
    let worktrees = crate::git_info::worktree_label_index();
    let worktree_label = |path: &std::path::Path| -> Option<String> {
        let key = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        worktrees.get(&key).cloned()
    };

    let mut candidates: Vec<LocationCandidate> = Vec::new();
    candidates.push(LocationCandidate {
        label: location_picker_label(&cwd),
        detail: format!("{}  (current)", crate::recent_dirs::display_path(&cwd)),
        worktree: worktree_label(&cwd),
        path: cwd.clone(),
    });
    for (path, ts) in recent.into_iter().filter(|(p, _)| p != &cwd) {
        let detail = format!(
            "{}  ({})",
            crate::recent_dirs::display_path(&path),
            crate::views::session_title::format_relative_time(
                (chrono::Utc::now() - ts).to_std().unwrap_or_default()
            ),
        );
        candidates.push(LocationCandidate {
            label: location_picker_label(&path),
            detail,
            worktree: worktree_label(&path),
            path,
        });
    }

    if let Some(d) = app.dashboard.as_mut() {
        let mut lp = LocationPickerState::new(candidates, cwd, worktrees);
        // Reflect the dashboard's current worktree mode so reopening the picker shows the toggle in its existing state
        lp.worktree_mode = d.dispatch_worktree;
        d.location_picker = Some(lp);
    }
    crate::unified_log::info("dashboard.location_picker.opened", None, None);
    vec![]
}

/// Apply a location-picker / `/cd` selection.
/// Resolves and validates the path; on success updates `app.cwd` and the process cwd (so newly dispatched sessions spawn there) and closes the modal.
/// On failure the modal stays open with an inline error and the cwd is unchanged.
pub(super) fn dispatch_dashboard_change_location(app: &mut AppView, input: String) -> Vec<Effect> {
    // Gate on the dashboard being the foreground view
    // `/cd <path>` typed from another surface (welcome, an agent session, the dashboard overlay) would otherwise silently change the process cwd
    // `app.dashboard` stays `Some` for the rest of the session once opened.
    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }
    let path = match resolve_location_input(&input, &app.cwd).filter(|p| p.is_dir()) {
        Some(p) => p,
        None => {
            if let Some(lp) = app
                .dashboard
                .as_mut()
                .and_then(|d| d.location_picker.as_mut())
            {
                lp.error = Some(format!("Not a directory: {}", input.trim()));
            } else if let Some(d) = app.dashboard.as_mut() {
                // `/cd <bad path>` typed into the dispatch box (no picker open): show the error as a dashboard toast
                d.set_error_toast(&format!("Not a directory: {}", input.trim()));
            }
            return vec![];
        }
    };

    crate::unified_log::info(
        "dashboard.location_picker.changed",
        None,
        Some(serde_json::json!({ "path": path.display().to_string() })),
    );

    let changed = app.cwd != path;
    let display = crate::recent_dirs::display_path(&path);
    app.cwd = path.clone();
    // Keep the git-repo flag in sync with the new cwd (it's otherwise only computed at startup). Worktree dispatch reads it.
    app.cwd_has_git_ancestor = path.ancestors().any(|p| p.join(".git").exists());
    // Warm the per-cwd git cache the header and top bar read (keyed on the new cwd) so the new branch and worktree label show on the next frame
    // Otherwise they wait for the first lazy refresh; git never spawns on the render path
    crate::git_info::populate_from_cwd_async(path.clone());

    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        // Keep the dashboard's cwd and git-repo snapshot in sync with the new cwd so the header tracks it immediately
        // The process cwd changes later, via `Effect::SetWorkingDir`
        d.cwd = path.clone();
        d.cwd_has_git_ancestor = has_git;
        // Carry the picker's worktree toggle onto the dashboard so the next dispatch honors it after the modal closes, but only in a git repo
        // Worktrees require a repo, so navigating to a non-git directory forces the mode off; the dashboard is never in worktree mode outside a repo
        if let Some(wt) = d.location_picker.as_ref().map(|lp| lp.worktree_mode) {
            d.dispatch_worktree = wt && has_git;
        } else if !has_git {
            d.dispatch_worktree = false;
        }
        d.location_picker = None;
        if changed {
            // Re-root the dispatch box's `@` file-context picker at the new cwd so completions walk the same tree newly dispatched sessions run in Both are keyed off `app.cwd`; the walk is cheap and only spawns once the user actually types `@`
            // Both are keyed off `app.cwd`; the walk is cheap and only spawns once the user actually types `@`
            d.dispatch.file_search.retarget(&path);
            d.error_toast = Some(format!("\u{2192} {display}"));
        }
    }

    // Change the process cwd via an effect, not inline, so the reducer stays side-effect free
    // Parallel tests would otherwise leak the process cwd into each other
    vec![Effect::SetWorkingDir { path }]
}

/// Confirm the dashboard worktree-label dialog: create the agent in a fresh worktree at `app.cwd`, replaying any prompt stashed at dialog open.
/// The dialog itself was already cleared by the input handler.
/// Shows a dashboard toast (instead of creating) when the cwd isn't a git repository.
pub(super) fn dispatch_dashboard_confirm_worktree(
    app: &mut AppView,
    label: Option<String>,
) -> Vec<Effect> {
    // Apply the prompt, attach choice, and staged model/mode together.
    // The worktree path must honor all of these exactly like the normal dispatch path (`dispatch_dashboard_dispatch`)
    let (mut prompt, attach) = match app.dashboard.as_mut() {
        Some(d) => (
            d.pending_worktree_prompt.take(),
            std::mem::replace(&mut d.pending_worktree_attach, false),
        ),
        None => (None, false),
    };
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    if !app.cwd_has_git_ancestor {
        if let Some(d) = app.dashboard.as_mut() {
            // Restore the typed prompt if the cwd stopped being a repo.
            if let Some(p) = prompt {
                d.dispatch.restore(p);
            }
            d.set_error_toast("Not a git repository: can't create a worktree here");
        }
        return vec![];
    }
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let (prompt_text, mut images, chip_elements) = if let Some(stashed) = prompt.take() {
        let (text, images, chip_elements) = stashed.into_submission();
        (Some(text), images, chip_elements)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let mut effects =
        dispatch_new_worktree_session(app, None, label, prompt_text, model_id, None, None);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(new_id) = effects.iter().find_map(|e| match e {
        Effect::CreateWorktreeSession { agent_id, .. } => Some(*agent_id),
        _ => None,
    }) {
        // Apply the staged effort / plan / always-approve mode to the new worktree agent (the base model is seeded via the effect's `model_id`)
        // Then carry any pasted images onto the replayed prompt; both mirror `dispatch_dashboard_dispatch`
        if let Some(agent) = app.agents.get_mut(&new_id) {
            apply_pending_dispatch_config(
                agent,
                pending_model.as_ref(),
                pending_mode,
                policy_block,
            );
            if let Some(entry) = agent.session.pending_prompts.back_mut() {
                entry.images = std::mem::take(&mut images);
                entry.chip_elements = chip_elements;
            }
        }
        if attach {
            // Send+open: attach the dashboard's detail-view overlay onto the agent (`dispatch_new_worktree_session` already set `active_view`)
            if let Some(d) = app.dashboard.as_mut() {
                d.restore_peek_viewport(&mut app.agents);
                d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
                d.attached_agent = Some(new_id);
            }
            sync_active_permission_mode_mirror(app);
        } else {
            // Plain Enter: undo the view switch `dispatch_new_worktree_session` made and stay on the dashboard
            app.active_view = ActiveView::AgentDashboard;
            if let Some(warning) = policy_block
                && let Some(dashboard) = app.dashboard.as_mut()
            {
                dashboard.set_error_toast(warning);
            }
        }
    }
    crate::prompt_images::drain_and_cleanup(
        crate::prompt_images::SessionPathPolicy::Preserve,
        &mut images,
    );
    effects
}

/// Cycle the dashboard overlay to the prev (-1) / next (+1) agent in the visible row order, wrapping at the ends.
/// Attaches overlay chrome on the first cycle from a session not opened via the dashboard.
pub(super) fn dispatch_dashboard_overlay_cycle(app: &mut AppView, delta: i32) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    // Anchor on the agent on screen, not a possibly-stale `attached_agent`: cycling stays correct after a session switch that never re-attached
    // Cycling is only ever invoked from an agent view
    let ActiveView::Agent(current) = app.active_view else {
        return vec![];
    };
    if app.agents.len() <= 1 {
        return vec![];
    }
    let order = if app.workspace_dashboard_enabled {
        let filter = app
            .dashboard
            .as_ref()
            .map_or(&crate::views::dashboard::Filter::None, |d| &d.filter);
        workspace_rows(app, filter)
            .0
            .into_iter()
            .filter_map(|row| match row.id {
                DashboardRowId::TopLevel(id) if !row.is_more_placeholder => Some(id),
                _ => None,
            })
            .collect()
    } else {
        match app.dashboard.as_ref() {
            Some(d) => crate::views::dashboard::overlay_cycle_order(d, &app.agents),
            None => {
                // Building state here must honor the same gates as `dispatch_open_dashboard` (feature flag AND auth)
                // Cycling must not reach an ungated dashboard on back-out
                if !crate::views::dashboard::dashboard_enabled()
                    || !matches!(app.auth_state, crate::app::app_view::AuthState::Done)
                {
                    return vec![];
                }
                let transient = dashboard_state_for_mode(app);
                crate::views::dashboard::overlay_cycle_order(&transient, &app.agents)
            }
        }
    };
    if order.len() <= 1 {
        return vec![];
    }
    let Some(idx) = order.iter().position(|id| *id == current) else {
        return vec![];
    };
    let n = order.len() as i32;
    let next_idx = (((idx as i32) + delta).rem_euclid(n)) as usize;
    let Some(&next_id) = order.get(next_idx) else {
        return vec![];
    };
    if next_id == current {
        return vec![];
    }
    // Allocate and configure only on a real switch
    // Otherwise a cycle-created dashboard renders bare on back-out (default cwd, empty `/model`, wrong auto-approve)
    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
        configure_dashboard_state(app);
    }
    // Drop any prior subagent takeover on the next agent so the overlay paints the parent view, mirroring the attach path
    if let Some(agent) = app.agents.get_mut(&next_id) {
        agent.close_subagent_fullscreen();
    }
    // A stop-confirm armed on the CURRENT agent must not carry over to the next one
    // A mouse click on `‹` / `›` lands here without the key-press disarm ever running
    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.attached_agent = Some(next_id);
        d.focus_row(DashboardRowId::TopLevel(next_id));
    }
    app.active_view = ActiveView::Agent(next_id);
    surface_yolo_launch_block_notice(app, next_id);
    vec![]
}

pub(super) fn dispatch_dashboard_dispatch(
    app: &mut AppView,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));
    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread
    // Stash this send and re-issue it once the probe completes so the image is never dropped from the dispatched prompt's content blocks
    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_dispatch_send =
            Some(crate::views::dashboard::state::DeferredDispatchSend { attach });
        return vec![];
    }
    let trimmed = text.trim().to_string();
    // Match the agent prompt's send behaviour; do not spawn a session
    if crate::slash::commands::exit::is_exit_alias(&trimmed) {
        if let Some(d) = app.dashboard.as_mut() {
            d.dispatch.set_text("");
            d.error_toast = None;
        }
        return dispatch(Action::Quit, app);
    }
    // Reject only an empty / whitespace-only prompt; any non-empty input (even a single character) dispatches a new session
    // The keyboard path already filters empty input upstream (see `DashboardState::handle_key`), so this guard protects the slash-fallback callers
    if trimmed.is_empty() {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Type a prompt to dispatch a session");
        }
        return vec![];
    }
    // Cap dispatch text at 64 KiB so a runaway paste doesn't queue a multi-megabyte prompt
    // The toast shows both the character count (what the user thinks in) and the byte budget (the actual limit), so the unit mismatch reads plainly
    const MAX_DISPATCH_BYTES: usize = 64 * 1024;
    if text.len() > MAX_DISPATCH_BYTES {
        let chars = text.chars().count();
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(&format!(
                "Prompt too long ({chars} chars / {} bytes; max ~64 KiB)",
                text.len()
            ));
        }
        return vec![];
    }

    // Worktree mode on (via the location picker) AND the cwd is a git repo: stash the prompt and open the worktree-label dialog
    // Confirming it spawns the agent in a fresh worktree and replays this prompt (see `Action::DashboardConfirmWorktree`)
    // The `attach` flag rides along so confirm honors the same choice as a normal dispatch (bare Enter stays; Ctrl+S opens the detail view)
    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, Some(text), attach);
    }

    // The dashboard's dispatch input ALWAYS spawns a new session; it is never a reply target
    // A selected row is purely the overview navigation cursor (Enter on it OPENS the agent); it must not turn the input into "reply to that agent"
    // Return the new AgentId from the inner constructor so we don't have to rely on `app.agents.last()`
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.dispatch, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (prompt_text, mut pasted_images, chip_elements) = prompt_state.into_submission();
    log_dashboard_launched("prompt");
    let (new_id, mut effects) = dispatch_new_session_inner_with_id(app, model_id, false);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(agent) = app.agents.get_mut(&new_id) {
        agent.session.enqueue_prompt(prompt_text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.images = std::mem::take(&mut pasted_images);
            entry.chip_elements = chip_elements;
        }
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    crate::prompt_images::drain_and_cleanup(
        crate::prompt_images::SessionPathPolicy::Preserve,
        &mut pasted_images,
    );
    // Only clear the input AFTER successful session creation
    // If the agent failed to register the prompt above we would have already returned without effects
    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;
    }
    if attach {
        // Ctrl+S (Send+Open): walk into the new agent's detail view AND paint the session-overlay chrome
        // Mirrors `dispatch_dashboard_attach` and `dispatch_dashboard_create_new_agent_with_detail`
        // Both `attached_agent` and `selected` follow the new row so the header's `‹ i/n ›` switcher has an anchor
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
            d.attached_agent = Some(new_id);
        }
        app.active_view = ActiveView::Agent(new_id);
        sync_active_permission_mode_mirror(app);
        surface_yolo_launch_block_notice(app, new_id);
    } else {
        // Plain Enter (Send) stays on the dashboard, no auto-select.
        // The fresh row is left unselected so the navigation cursor stays where the user had it
        // The input always dispatches a NEW session regardless of selection, so there's no reply state to worry about here
        app.active_view = ActiveView::AgentDashboard;
        // `apply_pending_dispatch_config` clamps a pinned always-approve to Normal and toasts the new agent
        // That toast is invisible while the view stays on the dashboard; mirror it on the dashboard's error slot
        if let Some(warning) = policy_block
            && let Some(d) = app.dashboard.as_mut()
        {
            d.set_error_toast(warning);
        }
    }
    effects
}

/// The dashboard has no session context, so the execution path is more limited than the agent view's:
/// The text spawns a new session as its first prompt, so a plugin or skill invoked from the dashboard is never silently dropped.
/// Registered, session-scoped (hidden on this surface): clear the dispatch and toast; do not spawn with the slash as a prompt.
pub(super) fn dispatch_dashboard_dispatch_slash(app: &mut AppView, text: String) -> Vec<Effect> {
    use crate::slash::command::{CommandExecCtx, CommandResult};
    use crate::slash::parse_invocation;

    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return vec![];
    }

    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let coding_data_sharing_lock_from_app = app.coding_data_sharing_lock();
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();
    // Dashboard commands run before any session exists, so the startup seed is the only answer available here

    // Build the execution context from app-wide state. The dashboard is session-less, so `session_id` is `None`.
    // Session-less opt-ins (`/model`, `/plan`) and pager-global commands still run and may toast if a dispatcher needs an agent
    let result = {
        let Some(invocation) = parse_invocation(trimmed.as_str()) else {
            return vec![];
        };

        // Get the slash registry from the dashboard's prompt widget.
        // The dashboard owns its own registry, populated at open time with builtins and ACP commands from `bootstrap_acp_commands`
        let Some(dashboard) = app.dashboard.as_ref() else {
            return vec![];
        };
        let reg = dashboard.dispatch.slash_controller.registry();

        {
            use distill_telemetry::events::{PagerCommandSource, PagerSlashCommand};
            use distill_telemetry::session_ctx::log_event;
            let source = if reg.is_builtin(invocation.token) {
                PagerCommandSource::Builtin
            } else {
                PagerCommandSource::NonBuiltin
            };
            log_event(PagerSlashCommand {
                command_name: invocation.token.to_string(),
                source,
            });
        }

        // Tier-restricted commands stay visible for discoverability but must not execute
        // They must not fall through to the unknown-command path below, which would spawn a session with the raw slash text as its first prompt
        // The dashboard has no question modal, so upsell via the feedback toast
        if reg.is_restricted(invocation.token) {
            let token = invocation.token.to_string();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!(
                    "/{token} requires SuperGrok: upgrade at {}",
                    super::billing::UPSELL_URL_UPGRADE
                ));
            }
            return vec![];
        }

        let Some(command) = reg.get(invocation.token).cloned() else {
            // Unknown command. Fall back to the regular dispatch path so the text becomes a new session's prompt.
            return dispatch_dashboard_dispatch(app, text, /* attach */ false);
        };
        // Registered but not offered on this surface:
        //   - session-scoped: toast; never spawn with the slash as a prompt
        //   - `visible() == false` (e.g. `/usage` on external auth): still `run()` so the command owns the refusal message
        if !dashboard
            .dispatch
            .slash_controller
            .is_command_offered(command.as_ref(), &app.models)
            && command.session_scoped()
            && !command.offered_when_session_less()
        {
            let name = command.name();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!("/{name} only works in a session"));
            }
            return vec![];
        }
        if let Some(dashboard) = app.dashboard.as_mut() {
            // Records MRU and queues an off-thread persist internally.
            dashboard
                .dispatch
                .slash_controller
                .record_command_use(invocation.token, invocation.token);
        }

        let dashboard_multiline = app.dashboard.as_ref().is_some_and(|d| d.multiline_mode);
        let mut ctx = CommandExecCtx {
            models: &app.models,
            session_id: None,
            bundle_state: &app.bundle_state,
            screen_mode: app.screen_mode,
            billing_surface_visible: app.grok_billing_surface_visible(),
            usage_command_visible: app.usage_command_visible(),
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: dashboard_multiline,
                yolo_mode: app.default_yolo,
                auto_mode: app.current_ui.permission_mode.as_deref() == Some("auto")
                    && !app.default_yolo,
                current_model_name: app.models.current_model_name(),
                available_models: app
                    .models
                    .available
                    .iter()
                    .map(|(id, info)| (info.name.clone(), id.clone()))
                    .collect(),
                coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
                coding_data_sharing_lock: coding_data_sharing_lock_from_app,
                plan_mode_active: false,
                show_tips: show_tips_from_app,
                auto_update: auto_update_from_app,
                vim_mode: crate::appearance::cache::load_vim_mode(),
                scroll_speed: crate::appearance::cache::load_scroll_speed(),
                respect_manual_folds: respect_manual_folds_from_app,
                auto_mode_gate: auto_mode_gate_from_app,
                ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
                voice_stt_language: voice_stt_language_from_app,
            },
        };
        command.run(&mut ctx, invocation.args)
    };

    match result {
        CommandResult::Handled => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            vec![]
        }
        CommandResult::Error(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                // Command errors are plain strings ("Unknown model: …", "Usage: /model <name> [effort]") with no glyph of their own
                // Route through `set_error_toast` so the badge shows the `✗` error marker
                // `Message` results below stay verbatim: they carry their own glyph (e.g. `✓ Theme: …`).
                d.set_error_toast(&msg);
            }
            vec![]
        }
        CommandResult::Message(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = Some(msg);
            }
            vec![]
        }
        CommandResult::Action(Action::ExitSession) => {
            // ExitSession from a session-less surface is meaningless; collapse it to `/dashboard`'s exit behaviour
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
            }
            dispatch(Action::ExitDashboard, app)
        }
        // A session-scoped switch can still stage the next dashboard agent.
        CommandResult::Action(Action::SwitchModel { model_id, effort }) => {
            stage_dashboard_model(app, model_id, effort);
            vec![]
        }
        CommandResult::Action(Action::SetTierLight(model, effort)) => {
            if model.is_empty() {
                app.cli_model_override = None;
                app.cli_effort_token = None;
                if let Some(reasoning) = app.models.reasoning_model.clone()
                    && app.models.available.contains_key(&reasoning)
                {
                    app.models.set_current(reasoning, None);
                }
                if let Some(d) = app.dashboard.as_mut() {
                    d.pending_model = None;
                    d.dispatch.set_text("");
                    d.models = app.models.clone();
                    d.error_toast = Some("✓ Worker model: removed".into());
                }
            } else {
                let model_id = acp::ModelId::new(model.clone());
                app.models.set_current(model_id.clone(), effort);
                app.cli_model_override = Some(model_id.clone());
                app.cli_effort_token = effort.map(|level| level.to_string());
                stage_dashboard_model(app, model_id, effort);
            }
            vec![Effect::PersistTierModel { worker: true, model, effort }]
        }
        CommandResult::Action(Action::SetDefaultModel(model_id)) => {
            let effects = dispatch(Action::SetDefaultModel(model_id), app);
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.models.reasoning_model = app.models.reasoning_model.clone();
            }
            effects
        }
        // `/plan` toggles whether the next spawned agent starts in plan mode
        // The command always reports `On` here (the dashboard's `plan_mode_active` snapshot is always false)
        // Flip between Plan and Normal so there is a session-less way to turn it back off
        CommandResult::Action(Action::SetPlanMode(_)) => {
            use crate::views::dashboard::DashboardDispatchMode;
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
                d.pending_mode = if d.pending_mode == DashboardDispatchMode::Plan {
                    DashboardDispatchMode::Normal
                } else {
                    DashboardDispatchMode::Plan
                };
            }
            vec![]
        }
        // `/plan <description>`: stage plan mode AND spawn immediately with the description as the first prompt
        CommandResult::Action(Action::EnterPlanMode { description }) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.pending_mode = crate::views::dashboard::DashboardDispatchMode::Plan;
            }
            match description {
                Some(desc) => {
                    dispatch_dashboard_dispatch(app, desc, /* attach */ false)
                }
                None => {
                    if let Some(d) = app.dashboard.as_mut() {
                        d.dispatch.set_text("");
                        d.error_toast = None;
                    }
                    vec![]
                }
            }
        }
        // `/plan` with an existing plan has nothing to show on the session-less dashboard
        CommandResult::Action(Action::ShowPlan) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("No plan to show on the dashboard");
            }
            vec![]
        }
        CommandResult::Action(action) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            dispatch(action, app)
        }
        CommandResult::Doctor(_) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("Open a session to run /doctor.");
            }
            vec![]
        }
        CommandResult::QueueCommand(_)
        | CommandResult::InjectSkill { .. }
        | CommandResult::PassThrough(_) => {
            // These results all expect an agent session to consume them
            // The dashboard has none, so route the text through the "new session with this as the first prompt" path so the user's typing isn't lost
            dispatch_dashboard_dispatch(app, text, /* attach */ false)
        }
    }
}

/// Stage a model (and optional reasoning effort) for the next agent the dashboard spawns.
/// Resolves the display name from the app's model catalog (or the raw id) so the renderer can show the indicator without a live `ModelState`.
/// Clears the dispatch input and any error toast.
fn stage_dashboard_model(
    app: &mut AppView,
    model_id: acp::ModelId,
    effort: Option<distill_shell::sampling::types::ReasoningEffort>,
) {
    let display = app
        .models
        .available
        .get(&model_id)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| model_id.0.to_string());
    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;
        // Mirror the staged choice into the dashboard catalog snapshot so `/model` marks this model `(current)` and its effort `(active)`
        // Otherwise the dropdown shows the app default the snapshot was seeded with at open, one step behind `pending_model`
        d.models.set_current(model_id.clone(), effort);
        d.pending_model = Some(crate::views::dashboard::PendingDispatchModel {
            id: model_id,
            effort,
            display,
        });
    }
}

/// Apply the dashboard's staged model effort and plan mode to a freshly spawned agent.
/// The base model is already seeded via `CreateSession`'s `model_id`.
/// The reasoning effort is stashed here and pushed to the shell once the session exists, mirroring the agent-view flow.
pub(super) fn apply_pending_dispatch_config(
    agent: &mut AgentView,
    pending_model: Option<&crate::views::dashboard::PendingDispatchModel>,
    pending_mode: crate::views::dashboard::DashboardDispatchMode,
    policy_block: Option<&'static str>,
) {
    use crate::views::dashboard::DashboardDispatchMode;

    if let Some(m) = pending_model {
        // The base model is seeded via `CreateSession.model_id`; only stash a deferred switch when an explicit effort must be pushed
        // Setting it (or clearing to `None`) also overrides any CLI `-m` default so the dashboard's `/model` choice wins
        agent.session.deferred_model_switch = m.effort.map(|e| DeferredModelSwitch {
            model_id: m.id.clone(),
            effort: Some(e),
            // Effort-only push; no display change to roll back.
            prev_model_id: None,
        });
    }
    let pending_mode =
        if policy_block.is_some() && pending_mode == DashboardDispatchMode::AlwaysApprove {
            DashboardDispatchMode::Normal
        } else {
            pending_mode
        };
    agent.session.yolo_mode = pending_mode == DashboardDispatchMode::AlwaysApprove;
    agent.session.auto_mode = pending_mode == DashboardDispatchMode::Auto;
    match pending_mode {
        DashboardDispatchMode::Normal
        | DashboardDispatchMode::Auto
        | DashboardDispatchMode::AlwaysApprove => {}
        DashboardDispatchMode::Plan => {
            agent.deferred_session_mode = Some(distill_tools::types::SessionMode::Plan);
            // Optimistic so the agent view reflects plan mode immediately when opened via Ctrl+S, before the ACP round-trip confirms it.
            // opened via Ctrl+S, before the ACP round-trip confirms it.
            agent.plan_mode_pending = Some(true);
        }
    }
    if let Some(warning) = policy_block {
        agent.show_toast(warning);
    }
}

/// Cycle the peeked agent's live mode using the agent prompt's gated rotation, the peek-panel counterpart to `DashboardCycleMode`.
/// The peek then behaves exactly like Shift+Tab inside that agent's chat view; the bottom-border badge reflects the new mode on the next frame.
/// Only top-level agents have a mode to cycle; subagents are parent-driven.
pub(super) fn dispatch_dashboard_peek_cycle_mode(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(row) = app
        .dashboard
        .as_ref()
        .and_then(|d| d.peek.as_ref().map(|p| p.row.clone()))
    else {
        return vec![];
    };
    let agent_id = match row {
        DashboardRowId::TopLevel(id) => id,
        DashboardRowId::Subagent { .. } => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_error_toast("Can't change a subagent's mode");
            }
            return vec![];
        }
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    // Temporarily target the peeked agent so `dispatch_cycle_mode_and_sync` (which reads `active_view`) acts on it; restored before returning
    // Uses the telemetry-free cycle body: the user is viewing the dashboard, not this agent's prompt
    // A plan nudge still within TTL must not record a spurious acceptance
    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = dispatch_cycle_mode_and_sync(app);
    app.active_view = saved_view;
    effects
}

/// An idle agent sends it immediately (a turn starts); a mid-turn agent keeps it queued so it drains after the current turn finishes.
/// This is the same queue and drain pipeline the agent view's own prompt input uses, so the two surfaces behave identically.
/// Subagent rows can't be replied to (they're driven by their parent), so they surface a toast and leave the peek open.
pub(super) fn dispatch_dashboard_peek_reply(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));

    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread
    // Stash this reply and re-issue it once the probe completes so the image is never dropped from the reply's content blocks
    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_peek_send =
            Some(crate::views::dashboard::state::DeferredPeekSend { row, attach });
        return vec![];
    }

    // Only top-level agents accept replies. Subagents are read-only peeks driven by their parent turn.
    let DashboardRowId::TopLevel(agent_id) = row else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Can't reply to a subagent");
        }
        return vec![];
    };

    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.peek_reply, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (text, images, chip_elements) = prompt_state.into_submission();

    if text.trim().is_empty() && images.is_empty() {
        return vec![];
    }

    let drain = {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
                d.set_error_toast("Session no longer exists");
            }
            return vec![];
        };

        // Enqueue then drain: an idle agent sends now, a running one stays queued
        // Untrimmed so `chip_elements` byte ranges stay aligned with the stored text.
        agent.session.enqueue_prompt(text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.chip_elements = chip_elements;
            if !images.is_empty() {
                entry.images = images;
            }
        }
        maybe_drain_queue(agent)
    };
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    let effects = drain.effects;

    // Clear the reply draft now that it's been accepted, and drop any stale error toast
    if let Some(d) = app.dashboard.as_mut() {
        d.clear_peek_reply();
        d.error_toast = None;
    }

    if attach {
        // Ctrl+S (Send+Open): walk into the agent's detail view with overlay chrome, mirroring `dispatch_dashboard_dispatch`'s attach branch
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(DashboardRowId::TopLevel(agent_id));
            d.attached_agent = Some(agent_id);
        }
        app.active_view = ActiveView::Agent(agent_id);
        surface_yolo_launch_block_notice(app, agent_id);
    }

    effects
}

/// The committed member a pin or reorder gesture acts on.
struct LayoutTarget {
    key: distill_dashboard_store::MemberKey,
    pinned: bool,
}

/// Why a pin or reorder gesture on the selected row cannot proceed.
enum LayoutRefusal {
    /// Subagent and roster rows have no store member and no layout of their own.
    NotWorkspaceRow,
    /// No committed member and no live agent behind the row.
    NotFound,
    /// Store writes are disabled, so a provisional row can never persist.
    ReadOnly,
    /// A live row whose upsert has not landed yet.
    NotSavedYet,
}

impl From<crate::app::workspace_membership::LayoutRequestError> for LayoutRefusal {
    fn from(error: crate::app::workspace_membership::LayoutRequestError) -> Self {
        match error {
            crate::app::workspace_membership::LayoutRequestError::ReadOnly => Self::ReadOnly,
            crate::app::workspace_membership::LayoutRequestError::MemberNotFound => Self::NotFound,
        }
    }
}

/// Pins and manual order write store ranks, so they need a committed member; a provisional live row has none yet.
fn workspace_layout_target(
    app: &AppView,
    row: &crate::views::dashboard::DashboardRowId,
) -> Result<LayoutTarget, LayoutRefusal> {
    use crate::views::dashboard::DashboardRowId;
    let session_id = match row {
        DashboardRowId::TopLevel(agent_id) => app
            .agents
            .get(agent_id)
            .and_then(|agent| agent.session.session_id.as_ref())
            .and_then(|session_id| {
                distill_dashboard_store::SessionId::new(session_id.0.to_string()).ok()
            }),
        DashboardRowId::Workspace { session_id } => {
            distill_dashboard_store::SessionId::new(session_id.clone()).ok()
        }
        DashboardRowId::Subagent { .. } | DashboardRowId::Roster { .. } => {
            return Err(LayoutRefusal::NotWorkspaceRow);
        }
    };
    let target = session_id
        .map(|session_id| distill_dashboard_store::MemberKey {
            session_id,
            kind: distill_dashboard_store::MemberKind::Build,
        })
        .and_then(|key| {
            let pinned = app.workspace_membership.effective_pinned(&key)?;
            Some(LayoutTarget { key, pinned })
        });
    let Some(target) = target else {
        let is_live_agent_row =
            matches!(row, DashboardRowId::TopLevel(id) if app.agents.contains_key(id));
        return Err(if !is_live_agent_row {
            LayoutRefusal::NotFound
        } else if app.workspace_membership.writes_disabled() {
            LayoutRefusal::ReadOnly
        } else {
            LayoutRefusal::NotSavedYet
        });
    };
    Ok(target)
}

fn refuse_workspace_layout(app: &mut AppView, refusal: impl Into<LayoutRefusal>) {
    let message = match refusal.into() {
        LayoutRefusal::NotWorkspaceRow => return,
        LayoutRefusal::NotFound => "Session is no longer in the workspace",
        LayoutRefusal::ReadOnly => "Dashboard workspace is read-only",
        LayoutRefusal::NotSavedYet => "Session isn't saved to the workspace yet",
    };
    app.show_toast(message);
}

pub(super) fn dispatch_dashboard_toggle_pin(app: &mut AppView) -> Vec<Effect> {
    if app.workspace_dashboard_enabled {
        let Some(row) = app.dashboard.as_ref().and_then(|d| d.selected.clone()) else {
            return vec![];
        };
        let LayoutTarget { key, pinned } = match workspace_layout_target(app, &row) {
            Ok(target) => target,
            Err(refusal) => {
                refuse_workspace_layout(app, refusal);
                return vec![];
            }
        };
        if let Err(error) = app.workspace_membership.request_pin(key, !pinned) {
            refuse_workspace_layout(app, error);
            return vec![];
        }
        return crate::app::workspace_sync::drain(app);
    }
    if let Some(d) = app.dashboard.as_mut() {
        let _ = d.toggle_pin_selected();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_begin_rename(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    let Some(sel) = d.selected.clone() else {
        return;
    };
    let crate::views::dashboard::DashboardRowId::TopLevel(agent_id) = &sel else {
        let message = if sel.is_subagent() {
            "Subagent rows can't be renamed"
        } else {
            "Load the session before renaming"
        };
        d.set_error_toast(message);
        return;
    };
    let prefill = app
        .agents
        .get(agent_id)
        .map(rename_prefill_title)
        .unwrap_or_default();
    if let Some(d) = app.dashboard.as_mut() {
        d.rename = Some(crate::views::dashboard::state::RenameDraft::new(
            sel, prefill,
        ));
    }
}

fn rename_prefill_title(agent: &AgentView) -> String {
    crate::views::session_title::rename_source_title(agent).unwrap_or_default()
}

pub(super) fn dispatch_dashboard_commit_rename(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(rn) = d.rename.take() else {
        return vec![];
    };
    // An empty or whitespace-only draft cancels without committing
    let trimmed = rn.text().trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let title: String = crate::views::session_title::sanitize_display_text(trimmed).into_owned();
    if title.is_empty() {
        return vec![];
    }
    let crate::views::dashboard::DashboardRowId::TopLevel(agent_id) = rn.row else {
        return vec![];
    };
    let mut effects = Vec::new();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        if let Some(session_id) = agent.session.session_id.clone() {
            let cwd = agent.session.cwd.clone();
            agent.display_name = Some(title.clone());
            effects.push(Effect::RenameSession {
                agent_id,
                session_id,
                title,
                cwd,
                kind: agent.rename_kind(),
            });
        } else {
            agent.display_name = Some(title);
        }
    }
    crate::app::workspace_sync::request(app);
    effects
}

/// Dashboard v2 rows in display order, the same build the renderer runs.
pub(super) fn workspace_rows(
    app: &AppView,
    filter: &crate::views::dashboard::Filter,
) -> (
    Vec<crate::views::dashboard::DashboardRow>,
    crate::views::dashboard::Grouping,
) {
    let source = crate::app::workspace_sync::WorkspaceRowSource::capture(
        &app.agents,
        &app.workspace_membership,
        app.home_session_agent,
        app.workspace_dashboard_enabled,
    );
    let inputs = source.inputs();
    let rows = crate::views::dashboard::build_rows_with_workspace(
        &app.agents,
        inputs,
        filter,
        crate::views::dashboard::render::cached_home(),
    );
    (rows, inputs.grouping())
}

pub(super) fn dashboard_focusables(app: &AppView) -> Vec<crate::views::dashboard::Focusable> {
    let Some(d) = app.dashboard.as_ref() else {
        return Vec::new();
    };
    let home = crate::views::dashboard::render::cached_home();
    let roster: &[crate::app::roster::RosterEntry] = if app.leader_mode {
        &app.leader_roster
    } else {
        &app.dashboard_local_sessions
    };
    let (rows, grouping) = if app.workspace_dashboard_enabled {
        workspace_rows(app, &d.filter)
    } else {
        (
            crate::views::dashboard::build_rows_with_roster(
                &app.agents,
                &d.pinned,
                &d.reorder,
                d.grouping,
                &d.filter,
                home,
                roster,
            ),
            d.grouping,
        )
    };
    crate::views::dashboard::render::focusables(
        &rows,
        grouping,
        &d.filter,
        &d.collapsed_sections,
        d.idle_show_all,
        d.search_mode,
    )
}

/// Chooses the next visible row after removal, falling back to the previous row.
pub(super) fn dashboard_neighbor_row(
    app: &AppView,
    closed: &crate::views::dashboard::DashboardRowId,
) -> Option<crate::views::dashboard::DashboardRowId> {
    use crate::views::dashboard::Focusable;
    let focusables = dashboard_focusables(app);
    let cur = focusables
        .iter()
        .position(|f| matches!(f, Focusable::Row(id) if id == closed))?;
    // Next row below (down 1); when the closed row was last, the previous row
    let next = focusables.get(cur + 1..).and_then(|rest| {
        rest.iter().find_map(|f| match f {
            Focusable::Row(id) => Some(id.clone()),
            Focusable::Section(_) | Focusable::IdleOverflow => None,
        })
    });
    next.or_else(|| {
        focusables.get(..cur).and_then(|prefix| {
            prefix.iter().rev().find_map(|f| match f {
                Focusable::Row(id) => Some(id.clone()),
                Focusable::Section(_) | Focusable::IdleOverflow => None,
            })
        })
    })
}

/// Busy top-level row: stop what keeps it busy (running turn, background tasks/monitors/`/loop`s, or queued prompts), never arm.
/// A busy roster row has no local work to stop, so it just reports it must be stopped first.
/// Delete only ever runs on an idle row, so it is never queued alongside a `CancelTurn`.
pub(super) fn dispatch_dashboard_stop(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;
    use std::time::Instant;

    let Some(sel) = app.dashboard.as_ref().and_then(|d| d.selected.clone()) else {
        return vec![];
    };
    match &sel {
        DashboardRowId::TopLevel(id) => {
            let id = *id;
            if app.workspace_dashboard_enabled {
                let Some(readiness) = app.agents.get(&id).map(dashboard_stop_readiness) else {
                    return vec![];
                };
                if !readiness.can_close()
                    && let Some(dashboard) = app.dashboard.as_mut()
                {
                    dashboard.delete_confirm = None;
                }
                return match readiness {
                    DashboardStopReadiness::Archiveable
                    | DashboardStopReadiness::LocallyClosable => arm_or_delete(app, sel),
                    DashboardStopReadiness::Stoppable => app
                        .agents
                        .get_mut(&id)
                        .and_then(stop_top_level_activity)
                        .unwrap_or_default(),
                    DashboardStopReadiness::Busy => vec![],
                };
            }
            let Some(agent) = app.agents.get_mut(&id) else {
                return vec![];
            };
            if !crate::views::dashboard::classify_top_level(agent).allows_delete() {
                let stopped = stop_top_level_activity(agent);
                if let Some(d) = app.dashboard.as_mut() {
                    d.delete_confirm = None;
                }
                return match stopped {
                    Some(effects) => effects,
                    None => {
                        app.show_toast("Stop the session before deleting");
                        vec![]
                    }
                };
            }
            arm_or_delete(app, sel)
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            let Some(agent) = app.agents.get_mut(parent) else {
                return vec![];
            };
            let Some(info) = agent.subagent_sessions.get_mut(child_session_id) else {
                return vec![];
            };
            let subagent_id = info.subagent_id.to_string();
            let attempt_id = info
                .attempt
                .lifecycle
                .current_attempt_id()
                .map(str::to_owned);
            info.attempt.pending_kill = true;
            info.attempt.kill_requested_at = Some(Instant::now());
            let session_id = agent.session.session_id.clone();
            session_id
                .map(|sid| Effect::KillSubagent {
                    session_id: sid,
                    subagent_id,
                    attempt_id,
                })
                .into_iter()
                .collect()
        }
        DashboardRowId::Roster { session_id } => {
            let entry = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id.as_str());
            match entry {
                None => {
                    app.show_toast("Session is no longer in the list");
                    vec![]
                }
                // Chat conversations can't be deleted from here yet, so don't arm a confirm that could never succeed
                Some(e) if e.origin.kind == "conversation" => {
                    app.show_toast("Deleting chat conversations isn't supported yet");
                    vec![]
                }
                // No local turn to cancel, so a busy roster row can't delete.
                Some(e)
                    if !crate::views::dashboard::roster_activity_to_state(e.activity)
                        .allows_delete() =>
                {
                    app.show_toast("Stop the session before deleting");
                    vec![]
                }
                Some(_) => arm_or_delete(app, sel),
            }
        }
        DashboardRowId::Workspace { .. } if app.workspace_dashboard_enabled => {
            arm_or_delete(app, sel)
        }
        DashboardRowId::Workspace { .. } => vec![],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DashboardStopReadiness {
    Archiveable,
    LocallyClosable,
    Stoppable,
    Busy,
}

impl DashboardStopReadiness {
    pub(crate) fn can_close(self) -> bool {
        matches!(self, Self::Archiveable | Self::LocallyClosable)
    }

    pub(crate) fn action(self) -> crate::views::dashboard::DashboardStopAction {
        match self {
            Self::Archiveable => crate::views::dashboard::DashboardStopAction::Archive,
            Self::LocallyClosable => crate::views::dashboard::DashboardStopAction::Close,
            Self::Stoppable | Self::Busy => crate::views::dashboard::DashboardStopAction::Stop,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DashboardStopPlan {
    cancel_foreground: bool,
    running_background_tasks: Vec<String>,
    scheduled_tasks: Vec<String>,
    discard_queued_prompts: bool,
}

impl DashboardStopPlan {
    pub(super) fn for_agent(agent: &crate::app::agent_view::AgentView) -> Self {
        let has_session = agent.session.session_id.is_some();
        Self {
            cancel_foreground: has_session
                && (!agent.session.state.is_idle() || agent.wake_turn_active()),
            running_background_tasks: if has_session {
                agent
                    .session
                    .bg_tasks
                    .values()
                    .filter(|task| task.status == crate::app::agent::BgTaskStatus::Running)
                    .map(|task| task.task_id.clone())
                    .collect()
            } else {
                Vec::new()
            },
            scheduled_tasks: if has_session {
                agent.session.scheduled_tasks.keys().cloned().collect()
            } else {
                Vec::new()
            },
            discard_queued_prompts: !agent.session.pending_prompts.is_empty(),
        }
    }

    /// Whether [`Self::for_agent`] would produce a non-empty plan, without collecting the task ids. Readiness is resolved
    /// on every frame of an overlay footer and every dashboard row, so it must not allocate; the owned plan is built only
    /// when a stop is actually dispatched.
    pub(super) fn would_stop_anything(agent: &crate::app::agent_view::AgentView) -> bool {
        let has_session = agent.session.session_id.is_some();
        (has_session
            && (!agent.session.state.is_idle()
                || agent.wake_turn_active()
                || agent.session.has_running_bg_tasks()
                || !agent.session.scheduled_tasks.is_empty()))
            || !agent.session.pending_prompts.is_empty()
    }

    pub(super) fn is_empty(&self) -> bool {
        !self.cancel_foreground
            && self.running_background_tasks.is_empty()
            && self.scheduled_tasks.is_empty()
            && !self.discard_queued_prompts
    }
}

pub(crate) fn dashboard_stop_readiness(
    agent: &crate::app::agent_view::AgentView,
) -> DashboardStopReadiness {
    if agent.session.loading_replay {
        DashboardStopReadiness::Busy
    } else if DashboardStopPlan::would_stop_anything(agent) {
        DashboardStopReadiness::Stoppable
    } else if agent.session.session_id.is_none() {
        DashboardStopReadiness::LocallyClosable
    } else if !matches!(
        crate::views::dashboard::classify_top_level(agent),
        crate::views::dashboard::RowState::Working
    ) {
        DashboardStopReadiness::Archiveable
    } else {
        DashboardStopReadiness::Busy
    }
}

fn stop_top_level_activity(agent: &mut crate::app::agent_view::AgentView) -> Option<Vec<Effect>> {
    let plan = DashboardStopPlan::for_agent(agent);
    if plan.is_empty() {
        return None;
    }
    let mut effects = Vec::new();
    if let Some(session_id) = agent.session.session_id.clone() {
        if plan.cancel_foreground {
            if agent.session.state.is_compact_running() {
                agent.cancel_and_arm(CancellationScope::Compaction, CancelOrigin::UserGesture);
            } else if agent.running_wake_turn.is_some() {
                agent.mark_wake_cancel_sent();
            } else if agent.session.state.is_turn_running() {
                agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
            }
            agent.cancel_trigger_hint = Some(crate::app::actions::CancelTrigger::DashboardStop);
            effects.push(super::turn::emit_cancel_turn(
                agent,
                session_id.clone(),
                /* cancel_subagents */ true,
                /* rewind_prompt_id */ None,
            ));
        }
        for task_id in plan.running_background_tasks {
            if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                task.pending_kill = true;
                task.kill_requested_at = Some(std::time::Instant::now());
            }
            effects.push(Effect::KillBgTask {
                session_id: session_id.clone(),
                task_id,
                source: distill_shell::extensions::task::TaskKillSource::Teardown,
            });
        }
        for task_id in plan.scheduled_tasks {
            agent.session.scheduled_tasks.remove(&task_id);
            effects.push(Effect::DeleteScheduledTask {
                session_id: session_id.clone(),
                task_id,
            });
        }
    }
    if plan.discard_queued_prompts {
        agent.session.pending_prompts.clear();
        agent.sync_queue_pane();
    }
    Some(effects)
}

/// A live arm on `sel` confirms and deletes; otherwise (re)arm.
fn arm_or_delete(app: &mut AppView, sel: crate::views::dashboard::DashboardRowId) -> Vec<Effect> {
    let armed = app
        .dashboard
        .as_mut()
        .and_then(|d| d.armed_delete_row())
        .as_ref()
        == Some(&sel);
    if armed {
        return delete_dashboard_row(app, sel);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.arm_delete(sel);
    }
    vec![]
}

pub(super) fn dispatch_dashboard_delete(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    // The `y` confirm: only fire on a row whose arm is still live AND is still the selected row
    // Reanchor / gc can drop `selected` without going through the focus helpers, which would otherwise let `y` delete a row the cursor has left
    let Some(sel) = d.armed_delete_row() else {
        return vec![];
    };
    if d.selected.as_ref() != Some(&sel) {
        d.delete_confirm = None;
        return vec![];
    }
    delete_dashboard_row(app, sel)
}

/// Delete `row`, which the caller has confirmed is idle and armed.
/// Takes `row` as a parameter (not read back off `delete_confirm`) and never cancels a turn or kills a task; delete is a settled-row operation.
fn delete_dashboard_row(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    if let Some(d) = app.dashboard.as_mut() {
        d.delete_confirm = None;
    }
    if app.workspace_dashboard_enabled {
        return archive_dashboard_row(app, row);
    }
    match row {
        DashboardRowId::TopLevel(id) => {
            let Some(agent) = app.agents.get(&id) else {
                return vec![];
            };
            // Defensive re-check via the SAME predicate the renderer and arm path use
            // State can change between arming and confirming (a new turn, `/loop`, queued prompt, replay, needs-input, or bg task)
            // Delete must never run on a non-settled row
            if !crate::views::dashboard::classify_top_level(agent).allows_delete() {
                app.show_toast("Stop the session before deleting");
                return vec![];
            }
            let Some(session_id) = agent.session.session_id.clone() else {
                app.show_toast("No session history to delete");
                return vec![];
            };
            let cwd = agent.session.cwd.display().to_string();
            app.show_toast("Deleting session\u{2026}");
            vec![Effect::DeleteSession {
                source: "current".into(),
                session_id: session_id.to_string(),
                cwd,
                after: crate::app::actions::AfterSessionDelete::Dashboard,
            }]
        }
        DashboardRowId::Subagent { .. } => {
            app.show_toast("Subagent rows can't be deleted from the dashboard");
            vec![]
        }
        DashboardRowId::Roster { session_id } => {
            let Some(entry) = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id)
                .cloned()
            else {
                app.show_toast("Session is no longer in the list");
                return vec![];
            };
            if entry.origin.kind == "conversation" {
                app.show_toast("Deleting chat conversations isn't supported yet");
                return vec![];
            }
            if !crate::views::dashboard::roster_activity_to_state(entry.activity).allows_delete() {
                app.show_toast("Stop the session before deleting");
                return vec![];
            }
            app.show_toast("Deleting session\u{2026}");
            vec![Effect::DeleteSession {
                source: "local".into(),
                session_id,
                cwd: entry.cwd,
                after: crate::app::actions::AfterSessionDelete::Dashboard,
            }]
        }
        DashboardRowId::Workspace { .. } => vec![],
    }
}

/// Closes loaded rows locally so live-agent adoption cannot recreate the archived membership.
fn archive_dashboard_row(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let (session_id, loaded_ids) = match &row {
        DashboardRowId::TopLevel(id) => {
            let Some(agent) = app.agents.get(id) else {
                return vec![];
            };
            let Some(session_id) = agent.session.session_id.as_ref() else {
                let loaded_ids = vec![*id];
                let neighbor = dashboard_neighbor_row(app, &row).filter(
                    |candidate| {
                        !matches!(candidate, DashboardRowId::TopLevel(id) if loaded_ids.contains(id))
                    },
                );
                return close_dashboard_agents(app, &loaded_ids, neighbor);
            };
            let session_id = session_id.0.to_string();
            let loaded_ids = app
                .agents
                .iter()
                .filter_map(|(candidate_id, candidate)| {
                    (!candidate.conversation_entry
                        && candidate
                            .session
                            .session_id
                            .as_ref()
                            .is_some_and(|candidate| candidate.0.as_ref() == session_id.as_str()))
                    .then_some(*candidate_id)
                })
                .collect::<Vec<_>>();
            if loaded_ids.iter().any(|id| {
                app.agents
                    .get(id)
                    .is_some_and(|agent| !dashboard_stop_readiness(agent).can_close())
            }) {
                app.show_toast("Session became active; stop it before archiving");
                return vec![];
            }
            (session_id, loaded_ids)
        }
        DashboardRowId::Workspace { session_id } => (session_id.clone(), Vec::new()),
        DashboardRowId::Subagent { .. } => {
            app.show_toast("Subagent rows can't be archived from the dashboard");
            return vec![];
        }
        DashboardRowId::Roster { .. } => return vec![],
    };

    let neighbor = dashboard_neighbor_row(app, &row).filter(
        |candidate| !matches!(candidate, DashboardRowId::TopLevel(id) if loaded_ids.contains(id)),
    );
    if !crate::app::workspace_sync::request_removal(
        app,
        &session_id,
        crate::app::workspace_membership::RemovalCause::Archive,
    ) {
        return vec![];
    }
    close_dashboard_agents(app, &loaded_ids, neighbor)
}

fn close_dashboard_agents(
    app: &mut AppView,
    loaded_ids: &[AgentId],
    neighbor: Option<crate::views::dashboard::DashboardRowId>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let foreground = matches!(
        app.active_view,
        ActiveView::Agent(active) if loaded_ids.contains(&active)
    );
    let attached = app
        .dashboard
        .as_ref()
        .and_then(|dashboard| dashboard.attached_agent)
        .is_some_and(|id| loaded_ids.contains(&id));
    if let Some(session_id) = loaded_ids.iter().find_map(|id| {
        app.agents
            .get(id)
            .and_then(|agent| agent.session.session_id.clone())
    }) && !app.agents.iter().any(|(id, agent)| {
        !loaded_ids.contains(id)
            && agent
                .session
                .session_id
                .as_ref()
                .is_some_and(|candidate| candidate == &session_id)
    }) {
        effects.push(Effect::UnregisterActiveSession { session_id });
    }
    for id in loaded_ids {
        remove_agent_and_cleanup(app, *id);
    }
    if foreground || attached {
        app.active_view = ActiveView::AgentDashboard;
    }

    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.delete_confirm = None;
        if dashboard
            .attached_agent
            .is_some_and(|id| loaded_ids.contains(&id))
        {
            dashboard.close_popup();
        }
        match neighbor {
            Some(row) => dashboard.focus_row(row),
            None => dashboard.focus_new_agent_button(),
        }
    }
    effects
}

pub(super) fn dispatch_dashboard_toggle_grouping(app: &mut AppView) -> Vec<Effect> {
    if app.workspace_dashboard_enabled {
        let Some(grouping) = app.workspace_membership.effective_grouping() else {
            return vec![];
        };
        let grouping = grouping.toggled();
        if let Err(error) = app.workspace_membership.request_grouping(grouping) {
            refuse_workspace_layout(app, error);
            return vec![];
        }
        if let Some(dashboard) = app.dashboard.as_mut() {
            dashboard.observe_workspace_grouping(grouping.into());
        }
        return crate::app::workspace_sync::drain(app);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.toggle_grouping();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_select(app: &mut AppView, next: bool) {
    let focusables = dashboard_focusables(app);
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    let set_cursor = |d: &mut crate::views::dashboard::DashboardState,
                      f: &crate::views::dashboard::Focusable| {
        match f {
            crate::views::dashboard::Focusable::Section(key) => d.focus_section(*key),
            crate::views::dashboard::Focusable::Row(id) => d.focus_row(id.clone()),
            crate::views::dashboard::Focusable::IdleOverflow => d.focus_idle_overflow(),
        }
    };
    // Actions-row button navigation contract:
    //   - Down moves to the first focusable section or row
    //   - Up stays on the current actions-row button
    if d.actions_focus.is_some() {
        if next && let Some(first) = focusables.first() {
            set_cursor(d, first);
            d.clear_manual_scroll();
        }
        return;
    }
    if focusables.is_empty() {
        // Nothing to land on AND the button isn't focused: fall back to the button so the cursor lives somewhere
        d.focus_new_agent_button();
        return;
    }
    // Current index from the active cursor (section header or row).
    let cur = focusables
        .iter()
        .position(|f| match f {
            crate::views::dashboard::Focusable::Section(key) => d.selected_section == Some(*key),
            crate::views::dashboard::Focusable::Row(id) => d.selected.as_ref() == Some(id),
            crate::views::dashboard::Focusable::IdleOverflow => d.selected_idle_overflow,
        })
        .unwrap_or(0);
    // Up on the first focusable focuses the `+ New Agent` button
    // The button acts as a sentinel above index 0, exactly like the agent's tabs in the agents modal
    if !next && cur == 0 {
        d.focus_new_agent_button();
        d.clear_manual_scroll();
        return;
    }
    let new = if next {
        (cur + 1).min(focusables.len() - 1)
    } else {
        cur.saturating_sub(1)
    };
    if let Some(item) = focusables.get(new) {
        set_cursor(d, item);
    }
    // Arrow-key nav is selection-driven: re-engage the clamp's snap-to-selection so the viewport tracks the cursor
    // Without this, ↑/↓ after a wheel scroll would leave the cursor selected on a row outside the viewport
    d.clear_manual_scroll();
}

pub(super) fn dispatch_dashboard_reorder(app: &mut AppView, up: bool) -> Vec<Effect> {
    if app.workspace_dashboard_enabled {
        let Some(selected) = app.dashboard.as_ref().and_then(|d| d.selected.clone()) else {
            return vec![];
        };
        let selected = match workspace_layout_target(app, &selected) {
            Ok(LayoutTarget { key, .. }) => key,
            Err(refusal) => {
                refuse_workspace_layout(app, refusal);
                return vec![];
            }
        };
        let mut order = app.workspace_membership.effective_manual_order();
        let position = order.iter().position(|key| *key == selected);
        if up {
            match position {
                Some(0) => {
                    order.remove(0);
                }
                Some(index) => order.swap(index, index - 1),
                None => order.insert(0, selected),
            }
        } else {
            match position {
                Some(index) if index + 1 < order.len() => order.swap(index, index + 1),
                Some(_) => {}
                None => order.push(selected),
            }
        }
        if let Err(error) = app.workspace_membership.request_manual_order(order) {
            refuse_workspace_layout(app, error);
            return vec![];
        }
        return crate::app::workspace_sync::drain(app);
    }
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(sel) = d.selected.clone() else {
        return vec![];
    };
    // Maintain a stable explicit ordering list. The renderer applies it as a "float to position" rule inside the row's group.
    let pos = d.reorder.iter().position(|r| *r == sel);
    if up {
        match pos {
            Some(0) => {
                d.reorder.remove(0);
            }
            Some(i) => {
                d.reorder.swap(i, i - 1);
            }
            None => {
                d.reorder.insert(0, sel);
            }
        }
    } else {
        match pos {
            Some(i) if i + 1 < d.reorder.len() => {
                d.reorder.swap(i, i + 1);
            }
            Some(_) => {
                // Already at the bottom — append to end.
            }
            None => {
                d.reorder.push(sel);
            }
        }
    }
    dispatch_dashboard_persist(app)
}

fn dispatch_dashboard_persist(app: &mut AppView) -> Vec<Effect> {
    if app.workspace_dashboard_enabled {
        return vec![];
    }
    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    // Don't hardcode `enabled = true`
    // Thread through the on-disk value so a user who set `enabled = false` doesn't get it overwritten on the next pin or grouping toggle
    let enabled = app
        .dashboard_persisted
        .as_ref()
        .map(|p| p.enabled)
        .unwrap_or(true);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    let persisted = d.to_persisted(enabled, &resolver);
    app.dashboard_persisted = Some(persisted.clone());
    vec![Effect::PersistDashboard(persisted)]
}

/// Answer a permission request from the dashboard peek panel without going through `PermissionSelect`, which needs `active_view == Agent(_)`.
/// Routes directly to the row's owning agent and verifies the request_id has not rotated since the peek snapshot was taken.
pub(super) fn dispatch_dashboard_permission_select(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    option_id: acp::PermissionOptionId,
) -> Vec<Effect> {
    // Determine the owning AgentId.
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    // Stale-snapshot guard.
    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed: re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    // Share the main dispatch's meta logic so dashboard peek honors an edited pattern (and the glob routing) identically instead of dropping it
    let edited_pattern = super::permissions::take_edited_pattern(agent, &perm);
    let meta = super::permissions::build_selection_meta(&perm, &option_id, edited_pattern);

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        )
        .meta(meta)))
        .ok();

    resolve_permission_queue_transition(agent);

    // Refresh the peek (it likely no longer has a question).
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Reject the peeked agent's pending permission with a typed feedback message: the peek panel's "No, type to add feedback" path.
/// Mirrors [`super::permissions::dispatch_permission_followup`]: resolve the front request with `RejectOnce` and the `followup_message` meta.
/// Targets the dashboard row's agent instead of the active view, with the same stale-request guard as [`dispatch_dashboard_permission_select`].
pub(super) fn dispatch_dashboard_permission_followup(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    text: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    // Stale-snapshot guard: the front request must still match
    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed: re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };
    // Resolve with the RejectOnce option and the feedback; cancel if the request somehow has no reject option
    let option_id = perm
        .options
        .iter()
        .find(|o| o.kind == acp::PermissionOptionKind::RejectOnce)
        .map(|o| o.option_id.clone());
    let outcome = match option_id {
        Some(option_id) => {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
        }
        None => acp::RequestPermissionOutcome::Cancelled,
    };
    let meta = if !text.trim().is_empty() {
        serde_json::json!({ "followup_message": text })
            .as_object()
            .cloned()
    } else {
        None
    };
    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(outcome).meta(meta)))
        .ok();
    resolve_permission_queue_transition(agent);
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Answer the peeked agent's pending `AskUserQuestion` (the Ask tool) from the dashboard peek panel.
/// `option_idx` selects an option; `None` with a non-empty `freeform` submits the "Other" free-text answer.
/// Delegates to [`AgentView::dashboard_answer_question`], which sends the ext-response; the peek closes once an answer is actually submitted.
pub(super) fn dispatch_dashboard_question_answer(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    option_idx: Option<usize>,
    freeform: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    match agent.dashboard_answer_question(option_idx, freeform) {
        // Whole form answered: close the peek
        crate::app::agent_view::PeekAnswerOutcome::Submitted => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
            }
        }
        // Advanced to the next question: keep the peek open but reset the per-question draft (option cursor, free text)
        // The next question starts fresh; the next render refreshes its content
        crate::app::agent_view::PeekAnswerOutcome::Advanced => {
            if let Some(d) = app.dashboard.as_mut() {
                if let Some(p) = d.peek.as_mut() {
                    p.selected_option = None;
                }
                d.clear_peek_reply();
            }
        }
        // Nothing happened (e.g. empty "Other"): leave the peek as-is.
        crate::app::agent_view::PeekAnswerOutcome::NoOp => {}
    }
    vec![]
}
