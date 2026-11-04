// Modified for Distill by Samuel Fajreldines, 2026.
//! Session rename / close helpers (shared with the dashboard).
//!
//! The `/sessions` picker modal was removed; `/rename` and the dashboard's close action still use these dispatchers.
use crate::app::actions::Effect;
use crate::app::agent::{AgentCommand, AgentId};
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView};
use crate::app::dispatch::ctx::{SwitchCause, show_welcome, switch_to_agent};
use crate::app::dispatch::dashboard::{
    DashboardStopPlan, close_retired_dashboard_agent, ensure_dashboard_state,
};
use crate::app::dispatch::task_result::unregister_session_effect;
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;
use crate::views::dashboard::DashboardRowId;
use std::time::Instant;
/// Removes the agent and clears `forked_from` pointers to it on surviving agents.
pub(in crate::app::dispatch) fn remove_agent_and_cleanup(app: &mut AppView, agent_id: AgentId) {
    let identity_rebind = super::super::dashboard::WorkspaceIdentityRebind::capture(app);
    if let Some(dashboard) = app.dashboard.as_mut() {
        dashboard.prepare_agent_unbind(
            &std::collections::HashSet::from([agent_id]),
            &mut app.agents,
        );
    }
    let removed = app.agents.shift_remove(&agent_id);
    for agent in app.agents.values_mut() {
        if agent.session.forked_from == Some(agent_id) {
            agent.session.forked_from = None;
        }
    }
    if removed.is_some() {
        drop(removed);
        crate::memory_release::release_retained_memory("agent-close");
    }
    identity_rebind.apply(app);
}
/// Minimal has no dashboard: `/new` replaces the visible session. A leftover prior
/// AgentView makes a later `/resume` of it focus-only (no `LoadSession`; GB-4877).
/// Each dropped view that already has a session id must leave the crash-recovery registry.
#[must_use]
pub(in crate::app::dispatch) fn drop_other_agents_in_minimal(
    app: &mut AppView,
    keep: AgentId,
) -> Vec<Effect> {
    if !app.screen_mode.is_minimal() {
        return vec![];
    }
    let stale: Vec<_> = app
        .agents
        .iter()
        .filter(|(id, _)| **id != keep)
        .map(|(id, agent)| (*id, agent.session.session_id.clone()))
        .collect();
    let mut effects = Vec::new();
    for (id, session_id) in stale {
        effects.extend(unregister_session_effect(session_id));
        remove_agent_and_cleanup(app, id);
    }
    effects
}
/// Close (drop from this pager's in-memory list) the given agent.
/// Refuse to close the only alive agent (toast "Cannot close the only session -- use /home to exit").
/// Clear `forked_from` references on surviving agents so dangling parent pointers cannot resurface.
pub(in crate::app::dispatch) fn dispatch_sessions_confirm_close(
    app: &mut AppView,
    closed_id: AgentId,
) -> Vec<Effect> {
    if !app.agents.contains_key(&closed_id) {
        return vec![];
    }
    if app.agents.len() == 1 {
        app.show_toast("Cannot close the only session -- use /home to exit");
        return vec![];
    }
    if matches!(app.active_view, ActiveView::Agent(id) if id == closed_id) {
        let parent = app
            .agents
            .get(&closed_id)
            .and_then(|a| a.side_parent.or(a.session.forked_from))
            .filter(|p| app.agents.contains_key(p));
        let fallback = parent.or_else(|| app.agents.keys().copied().find(|id| *id != closed_id));
        if let Some(target) = fallback {
            switch_to_agent(app, target, SwitchCause::Picker);
        } else {
            show_welcome(app);
        }
    }
    let effects = unregister_session_effect(
        app.agents
            .get(&closed_id)
            .and_then(|a| a.session.session_id.clone()),
    );
    remove_agent_and_cleanup(app, closed_id);
    effects
}

fn retire_target_agent(app: &AppView) -> Option<AgentId> {
    if let Some(id) = app.dashboard.as_ref().and_then(|d| d.attached_agent)
        && matches!(app.active_view, ActiveView::Agent(active) if active == id)
    {
        return Some(id);
    }
    if matches!(app.active_view, ActiveView::AgentDashboard) {
        return match app.dashboard.as_ref().and_then(|d| d.selected.clone()) {
            Some(DashboardRowId::TopLevel(id)) => Some(id),
            _ => None,
        };
    }
    match app.active_view {
        ActiveView::Agent(id) => Some(id),
        _ => None,
    }
}

fn retire_blocked_reason(agent: &AgentView) -> Option<&'static str> {
    if agent.session.loading_replay {
        Some("Cannot retire while session is replaying")
    } else if agent.blocking_card().is_some() {
        Some("Cannot retire while a question or permission is pending")
    } else if !agent.session.state.is_idle() {
        Some("Cannot retire a busy session")
    } else if DashboardStopPlan::would_stop_anything(agent) {
        Some("Cannot retire while queued, background, or scheduled work is pending")
    } else if !agent.shared_queue.is_empty() {
        Some("Cannot retire while queued work is pending")
    } else if !agent.prompt.text().trim().is_empty() || !agent.prompt.images.is_empty() {
        Some("Cannot retire while a draft is in the composer")
    } else {
        None
    }
}

/// Compact, wait for durable resume checkpoint via CompactComplete, then close/archive.
pub(in crate::app::dispatch) fn dispatch_retire_session(app: &mut AppView) -> Vec<Effect> {
    let Some(agent_id) = retire_target_agent(app) else {
        app.show_toast("Open a session before retiring");
        return vec![];
    };
    if app.dashboard.as_ref().is_some_and(|dashboard| {
        dashboard
            .peek
            .as_ref()
            .is_some_and(|peek| peek.row == DashboardRowId::TopLevel(agent_id))
            && (!dashboard.peek_reply.text().trim().is_empty()
                || !dashboard.peek_reply.images.is_empty())
    }) {
        app.show_toast("Send or clear the reply draft before retiring");
        return vec![];
    }
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if agent.retire_after_compact.is_some() || agent.retire_after_workspace.is_some() {
        app.show_toast("Retire already in progress");
        return vec![];
    }
    if let Some(reason) = retire_blocked_reason(agent) {
        app.show_toast(reason);
        return vec![];
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        app.show_toast("Open a session before retiring");
        return vec![];
    };
    agent.retire_after_compact = Some(session_id.0.to_string());
    agent.session.start_command(AgentCommand::Compact);
    agent.running_wake_turn = None;
    agent.turn_started_at = Some(Instant::now());
    agent
        .scrollback
        .push_block(RenderBlock::session_event(SessionEvent::CompactStarted));
    vec![Effect::Compact {
        agent_id,
        session_id,
        user_context: None,
    }]
}

pub(in crate::app::dispatch) fn finish_retire_after_compact(
    app: &mut AppView,
    agent_id: AgentId,
    expected_session_id: String,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get(&agent_id) else {
        return vec![];
    };
    let current_sid = agent.session.session_id.as_ref().map(|id| id.0.to_string());
    if current_sid.as_deref() != Some(expected_session_id.as_str()) {
        app.show_toast("Retire cancelled: session identity changed");
        return vec![];
    }
    if let Some(reason) = retire_blocked_reason(agent) {
        app.show_toast(reason);
        return vec![];
    }
    if app.workspace_dashboard_enabled {
        if !crate::app::workspace_sync::request_removal(
            app,
            &expected_session_id,
            crate::app::workspace_membership::RemovalCause::Archive,
        ) {
            return vec![];
        }
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.retire_after_workspace = Some(expected_session_id);
        }
        return crate::app::workspace_sync::drain(app);
    }
    close_retired_agent(app, agent_id)
}

fn close_retired_agent(app: &mut AppView, closed_id: AgentId) -> Vec<Effect> {
    if !app.agents.contains_key(&closed_id) {
        return vec![];
    }
    if matches!(app.active_view, ActiveView::Agent(id) if id == closed_id) {
        let parent = app
            .agents
            .get(&closed_id)
            .and_then(|a| a.side_parent.or(a.session.forked_from))
            .filter(|p| app.agents.contains_key(p));
        let fallback = parent.or_else(|| app.agents.keys().copied().find(|id| *id != closed_id));
        if let Some(target) = fallback {
            switch_to_agent(app, target, SwitchCause::Picker);
        } else if app.workspace_dashboard_enabled || app.dashboard.is_some() {
            ensure_dashboard_state(app);
            app.active_view = ActiveView::AgentDashboard;
        } else {
            show_welcome(app);
        }
    }
    let effects = unregister_session_effect(
        app.agents
            .get(&closed_id)
            .and_then(|a| a.session.session_id.clone()),
    );
    remove_agent_and_cleanup(app, closed_id);
    effects
}

/// Drop retired agents only after workspace membership removal has committed.
pub(in crate::app::dispatch) fn complete_retire_after_workspace(
    app: &mut AppView,
    removed: &std::collections::HashSet<distill_dashboard_store::SessionId>,
) -> Vec<Effect> {
    let Some(snapshot) = app.workspace_membership.snapshot() else {
        return vec![];
    };
    let snapshot_ids = snapshot
        .members
        .iter()
        .filter(|member| matches!(member.kind, distill_dashboard_store::MemberKind::Build))
        .map(|member| member.session_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut close_ids = Vec::new();
    let mut abort_ids = Vec::new();
    for (id, agent) in &app.agents {
        let Some(raw) = agent.retire_after_workspace.as_ref() else {
            continue;
        };
        let Ok(session_id) = distill_dashboard_store::SessionId::new(raw.clone()) else {
            abort_ids.push(*id);
            continue;
        };
        let identity_matches = agent
            .session
            .session_id
            .as_ref()
            .is_some_and(|current| current.0.as_ref() == raw.as_str());
        if !identity_matches || retire_blocked_reason(agent).is_some() {
            abort_ids.push(*id);
        } else if removed.contains(&session_id) && !snapshot_ids.contains(&session_id) {
            close_ids.push(*id);
        } else if !app.workspace_membership.is_session_hidden(&session_id) {
            abort_ids.push(*id);
        }
    }
    for id in abort_ids {
        if let Some(agent) = app.agents.get_mut(&id)
            && let Some(session_id) = agent.retire_after_workspace.take()
        {
            crate::app::workspace_sync::allow_loaded_session(app, &session_id);
            crate::app::workspace_sync::request(app);
        }
    }
    let mut effects = Vec::new();
    for id in close_ids {
        if let Some(agent) = app.agents.get_mut(&id) {
            agent.retire_after_workspace = None;
        }
        effects.extend(close_retired_dashboard_agent(app, id));
    }
    effects
}
/// Rename the current session via x.ai/session/rename.
/// Produces Effect::RenameSession which spawns an async ACP ext request.
/// On completion, TaskResult::RenameSessionComplete shows the result.
pub(in crate::app::dispatch) fn dispatch_rename_session(
    app: &mut AppView,
    title: String,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    let title = distill_shell::session::persistence::sanitize_rename_title(&title).into_owned();
    if title.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Couldn't rename session: title must not be blank".to_string(),
        ));
        return vec![];
    }
    agent.display_name = Some(title.clone());
    vec![Effect::RenameSession {
        agent_id: id,
        session_id,
        title,
        cwd: agent.session.cwd.clone(),
        kind: agent.rename_kind(),
    }]
}
/// Unpin the current session title via `x.ai/session/rename` with `resetToAuto`.
///
/// Chat-kind sessions have no local `SummaryGenerator` to restore, so they are refused here (no optimistic clear, no ext request).
pub(in crate::app::dispatch) fn dispatch_reset_session_title(app: &mut AppView) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    let kind = agent.rename_kind();
    if kind == distill_shell::session::unified_list::SessionKind::Chat {
        agent
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::system(
                "Chat conversations have no auto-title to restore",
            ));
        return vec![];
    }
    let previous_display_name = agent.display_name.clone();
    let previous_generated_title = agent.generated_session_title.clone();
    agent.title_unpin_committed = false;
    let pin = agent.display_name.take();
    if agent.generated_session_title.as_deref() == pin.as_deref() {
        agent.generated_session_title = None;
    }
    vec![Effect::ResetSessionTitle {
        agent_id: id,
        session_id,
        cwd: agent.session.cwd.clone(),
        kind,
        previous_display_name,
        previous_generated_title,
    }]
}
