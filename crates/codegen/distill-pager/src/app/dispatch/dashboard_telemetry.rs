// Modified for Distill by Samuel Fajreldines, 2026.
use crate::app::app_view::AppView;
use crate::views::dashboard::DashboardRowId;
use distill_telemetry::events::{
    DashboardAgentAttached, DashboardAgentLaunched, DashboardClosed, DashboardOpened,
};
use distill_telemetry::session_ctx::log_event;

pub(super) fn log_dashboard_opened(app: &AppView) {
    let subagents: usize = app.agents.values().map(|a| a.subagent_sessions.len()).sum();
    log_event(DashboardOpened {
        agents: app.agents.len(),
        subagents,
        leader_mode: app.leader_mode,
    });
}

pub(super) fn log_dashboard_closed(app: &AppView) {
    log_event(DashboardClosed {
        agents: app.agents.len(),
    });
}

pub(super) fn log_dashboard_attached(id: &DashboardRowId) {
    let kind = match id {
        DashboardRowId::TopLevel(_) => "top_level",
        DashboardRowId::Subagent { .. } => "subagent",
        DashboardRowId::Roster { .. } => "roster",
        DashboardRowId::Workspace { .. } => "workspace",
    };
    log_event(DashboardAgentAttached { kind });
}

pub(super) fn log_dashboard_launched(source: &'static str) {
    log_event(DashboardAgentLaunched { source });
}
