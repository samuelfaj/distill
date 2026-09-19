// Modified for Distill by Samuel Fajreldines, 2026.
//! Session rename / close helpers (shared with the dashboard).
//!
//! The `/sessions` picker modal was removed; `/rename` and the dashboard's close action still use these dispatchers.
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView};
use crate::app::dispatch::ctx::{SwitchCause, show_welcome, switch_to_agent};
use crate::app::dispatch::task_result::unregister_session_effect;
use crate::scrollback::block::RenderBlock;
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
            .and_then(|a| a.session.forked_from)
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
