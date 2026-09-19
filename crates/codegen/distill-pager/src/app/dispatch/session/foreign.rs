// Modified for Distill by Samuel Fajreldines, 2026.
use super::picker_routing::{PickerRequest, PickerSeqKind, PickerTarget, accept_picker_result};
use crate::app::actions::Effect;
use crate::app::app_view::{AppView, SessionPickerEntry};
use crate::app::dispatch::ctx::{get_active_agent, get_active_agent_mut};
use crate::app::effects::ConversationsPartial;
use crate::views::modal::ActiveModal;
use crate::views::session_picker::repo_name_from_cwd;
use crate::views::session_picker_surface::SessionPickerHost;

use distill_shell::session::unified_list::ListScope;

/// The history `kind` filter for the welcome screen's multi-source history under `--chat`.
/// Sandbox maps to `chat` (gateway); Local maps to `build` (local-disk).
/// Modal and non-welcome fetches get `None` so the shell keeps its default of forcing chat mode.
pub(in crate::app::dispatch) fn welcome_history_kind_filter(app: &AppView) -> Option<Vec<String>> {
    #[cfg(feature = "local-workspace")]
    {
        if app.chat_mode && matches!(app.active_view, crate::app::app_view::ActiveView::Welcome) {
            return Some(vec![
                app.welcome_workspace_mode.history_kind_filter().to_string(),
            ]);
        }
    }
    let _ = app;
    None
}

/// Server-side headless policy for the picker that will consume the fetch (modal first, then welcome, the same order as the loaded handler).
/// `Only` on the Headless page, `Exclude` everywhere else.
pub(in crate::app::dispatch) fn active_picker_headless_policy(
    app: &AppView,
) -> distill_shell::session::unified_list::HeadlessPolicy {
    let filter = if let Some(agent) = get_active_agent(app)
        && let Some(ActiveModal::SessionPicker { source_filter, .. }) = agent.active_modal.as_ref()
    {
        *source_filter
    } else {
        app.session_picker_source_filter
    };
    filter.headless_policy()
}

pub(in crate::app::dispatch) fn next_picker_list_generation(app: &mut AppView) -> u64 {
    app.session_picker_list_seq = app.session_picker_list_seq.wrapping_add(1);
    app.session_picker_list_seq
}

pub(in crate::app::dispatch) fn dispatch_fetch_session_list(app: &mut AppView) -> Vec<Effect> {
    // The wipe below destroys the welcome picker state that any still-pending result was issued for So the welcome generation reallocates on every call, even when the fetch belongs to an open modal, which gets its own fresh generation
    // So the welcome generation reallocates on every call, even when the fetch belongs to an open modal, which gets its own fresh generation
    app.session_picker_generation = app.alloc_picker_generation();
    let modal_generation = matches!(
        get_active_agent(app).and_then(|agent| agent.active_modal.as_ref()),
        Some(ActiveModal::SessionPicker { .. })
    )
    .then(|| app.alloc_picker_generation());
    app.session_picker_loading = true;
    app.session_picker_entries = None;
    app.session_picker_state.selected = 0;
    app.session_picker_state.set_query("");
    app.session_picker_state.search_active = false;
    app.session_picker_state.expanded.clear();
    app.session_picker_content_results = None;
    app.session_picker_content_loading = false;
    app.session_picker_entries_query = None;
    app.session_picker_pending_delete = None;
    let seq = next_picker_list_generation(app);
    app.foreign_session_scan_seq += 1;
    let foreign_seq = app.foreign_session_scan_seq;
    let kind_filter = welcome_history_kind_filter(app);
    #[cfg(feature = "local-workspace")]
    crate::views::welcome::workspace_mode::log_history_source(
        "session_list_fetch_dispatch",
        Some(app.welcome_workspace_mode),
        kind_filter.as_deref(),
        None,
    );
    let (host, generation) = match modal_generation {
        Some(generation) => (SessionPickerHost::AgentModal, generation),
        None => (SessionPickerHost::Welcome, app.session_picker_generation),
    };
    let mut effects = vec![Effect::FetchSessionList {
        host,
        cwd_override: None,
        generation,
        query: None,
        seq,
        kind_filter,
        headless_policy: active_picker_headless_policy(app),
    }];
    let foreign_effect = if app.chat_mode {
        app.foreign_scan_coordinator.begin_request(foreign_seq);
        None
    } else {
        let distill_home = distill_tools::util::distill_home::distill_home();
        crate::app::foreign_sessions::scan_effect(
            &app.cwd,
            app.foreign_session_compat,
            &distill_home,
            app.foreign_scan_coordinator.clone(),
            foreign_seq,
        )
    };
    let foreign_loading = foreign_effect.is_some();
    let mut modal_lanes_set = false;
    if let Some(agent) = get_active_agent_mut(app)
        && let Some(ActiveModal::SessionPicker {
            lanes, generation, ..
        }) = agent.active_modal.as_mut()
    {
        lanes.foreign_loading = foreign_loading;
        lanes.pending_notice = None;
        if let Some(fresh) = modal_generation {
            *generation = fresh;
        }
        modal_lanes_set = true;
    }
    app.session_picker_lanes.foreign_loading = foreign_loading && !modal_lanes_set;
    app.session_picker_lanes.pending_notice = None;
    effects.extend(foreign_effect);
    effects
}

pub(in crate::app::dispatch) fn handle_session_list_loaded(
    app: &mut AppView,
    request: PickerRequest,
    mut sessions: Vec<SessionPickerEntry>,
    partial: Option<ConversationsPartial>,
    scope: ListScope,
    query: Option<String>,
) -> Vec<Effect> {
    let dashboard_request =
        request.host == crate::views::session_picker_surface::SessionPickerHost::Dashboard;
    if dashboard_request && app.workspace_dashboard_enabled {
        let live_by_session = app
            .agents
            .iter()
            .filter_map(|(agent_id, agent)| {
                agent
                    .session
                    .session_id
                    .as_ref()
                    .map(|session_id| (session_id.0.as_ref(), (*agent_id, agent)))
            })
            .collect::<std::collections::HashMap<_, _>>();
        let workspace = app.workspace_membership.view();
        let workspace_session_ids = workspace
            .as_ref()
            .map(|view| {
                view.members
                    .iter()
                    .filter(|member| {
                        let live = live_by_session.get(member.session_id.as_ref()).copied();
                        let pinned =
                            crate::views::dashboard::row::workspace_member_is_pinned(member);
                        crate::views::dashboard::row::workspace_member_is_visible(
                            member,
                            live.map(|(_, agent)| agent),
                            pinned,
                        )
                    })
                    .map(|member| member.session_id.as_ref())
                    .collect::<std::collections::HashSet<_>>()
            })
            .unwrap_or_default();
        sessions.retain(|entry| !workspace_session_ids.contains(entry.id.as_str()));
    }
    let chat_mode = app.chat_mode && !dashboard_request;
    let is_browse = query.is_none();
    let notice;
    {
        let Some(mut target) =
            accept_picker_result(app, request, PickerSeqKind::List, "session list")
        else {
            return vec![];
        };
        if let Some(partial) = partial {
            crate::unified_log::warn(
                "session.list.partial",
                None,
                Some(serde_json::json!({ "reason": format!("{partial:?}") })),
            );
        }
        let empty_notice = partial.map_or_else(
            || "No sessions found for this directory".to_owned(),
            |partial| partial.picker_notice().to_owned(),
        );
        let partial_notice = partial.map(ConversationsPartial::picker_notice);
        notice = target.native_loaded(sessions, query, chat_mode, empty_notice, partial_notice);
        *target.detail_seq += 1;
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    } else if !dashboard_request
        && scope.is_relaxed()
        && app.session_picker_relaxed_notified_for.as_deref() != Some(app.cwd.as_path())
        // Welcome view drops toasts; don't consume the one-shot notice unless it can render
        && !matches!(app.active_view, crate::app::app_view::ActiveView::Welcome)
    {
        // Notify once per directory; the browse is scoped to `app.cwd`.
        app.session_picker_relaxed_notified_for = Some(app.cwd.clone());
        let message = match scope {
            ListScope::Repo => {
                "No sessions in this directory. Showing other sessions from this repository."
            }
            _ => "No sessions in this directory. Showing sessions from other directories.",
        };
        app.show_toast(message);
    }
    // A cwd-scoped browse clears `session_picker_relaxed_notified_for` so a later relax re-notifies; search responses leave it alone
    if !dashboard_request && !scope.is_relaxed() && is_browse {
        app.session_picker_relaxed_notified_for = None;
    }
    vec![]
}

pub(in crate::app::dispatch) fn handle_session_list_failed(
    app: &mut AppView,
    request: PickerRequest,
    error: String,
    query: Option<String>,
) -> Vec<Effect> {
    let chat_mode = app.chat_mode
        && request.host != crate::views::session_picker_surface::SessionPickerHost::Dashboard;
    let is_search = query.is_some();
    let notice;
    {
        let Some(mut target) =
            accept_picker_result(app, request, PickerSeqKind::List, "session list failure")
        else {
            return vec![];
        };
        tracing::warn!(error = %error, "session list fetch failed");
        let error_notice = format!("Couldn't load sessions: {error}");
        notice = target.native_failed(error_notice, is_search, chat_mode);
        *target.detail_seq += 1;
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    }
    vec![]
}

pub(in crate::app::dispatch) fn handle_foreign_sessions_scanned(
    app: &mut AppView,
    scanned: Vec<SessionPickerEntry>,
    seq: u64,
) -> Vec<Effect> {
    if app.chat_mode || seq != app.foreign_session_scan_seq {
        return vec![];
    }
    // The scan is not routed by host: it keeps its own seq and coordinator, applying to the modal first and falling back to the welcome picker
    let list_seq = app.session_picker_list_seq;
    let mut scanned = Some(scanned);
    let mut notice = None;
    let mut handled = false;
    if let Some(agent) = get_active_agent_mut(app) {
        let current_repo = repo_name_from_cwd(&agent.session.cwd.to_string_lossy());
        if let Some(ActiveModal::SessionPicker {
            entries,
            loading,
            lanes,
            state,
            content_results,
            content_loading,
            deep_search_seq,
            generation,
            detail_seq,
            entries_query,
            source_filter,
            ..
        }) = agent.active_modal.as_mut()
            && lanes.foreign_loading
        {
            handled = true;
            let mut target = PickerTarget {
                entries,
                loading,
                lanes,
                state,
                content_results,
                content_loading,
                entries_query,
                source_filter: *source_filter,
                grouped: true,
                current_repo,
                generation: *generation,
                list_seq,
                deep_search_seq: *deep_search_seq,
                detail_seq,
            };
            notice = target.foreign_loaded(scanned.take().unwrap_or_default());
            *target.detail_seq += 1;
        }
    }
    if !handled && app.session_picker_lanes.foreign_loading {
        let current_repo = repo_name_from_cwd(&app.cwd.to_string_lossy());
        let mut target = PickerTarget {
            entries: &mut app.session_picker_entries,
            loading: &mut app.session_picker_loading,
            lanes: &mut app.session_picker_lanes,
            state: &mut app.session_picker_state,
            content_results: &mut app.session_picker_content_results,
            content_loading: &mut app.session_picker_content_loading,
            entries_query: &mut app.session_picker_entries_query,
            source_filter: app.session_picker_source_filter,
            grouped: app.session_picker_grouped,
            current_repo,
            generation: app.session_picker_generation,
            list_seq,
            deep_search_seq: app.session_picker_deep_search_seq,
            detail_seq: &mut app.session_picker_detail_seq,
        };
        notice = target.foreign_loaded(scanned.unwrap_or_default());
        *target.detail_seq += 1;
    }
    if let Some(notice) = notice {
        app.show_toast(&notice);
    }
    vec![]
}

pub(in crate::app::dispatch) fn invalidate_foreign_picker(app: &mut AppView) {
    app.foreign_session_scan_seq += 1;
    app.foreign_scan_coordinator
        .begin_request(app.foreign_session_scan_seq);
    app.session_picker_lanes = Default::default();
}
