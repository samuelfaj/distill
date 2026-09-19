// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

/// Cached `mcp.push_server_status` flag resolution.
/// Resolution mirrors the `mcp.liveness_watchers` flag pattern but only stacks the env and default layers.
/// The pager process does not load `config.toml` / `requirements.toml`.
pub(super) fn push_server_status_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        distill_shell::util::config::resolve_mcp_push_server_status(
            /* requirements */ None, /* user */ None, /* managed */ None,
        )
    })
}

pub(super) fn handle_mcp_init_progress(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        total: u32,
        connected: u32,
        #[serde(default)]
        session_id: Option<String>,
    }
    let Ok(payload) = serde_json::from_str::<Payload>(notif.params.get()) else {
        return false;
    };
    let Some((is_active, agent)) = mcp_target_agent(app, payload.session_id.as_deref()) else {
        return false;
    };
    agent.mcp_init_progress = Some(super::super::agent_view::McpInitProgress {
        total: payload.total,
        connected: payload.connected,
    });
    is_active
}

/// So the sessionId branch is the primary path for current builds.
/// Older shells / forward-compat (**`tools_changed` only**): a payload with no `sessionId` falls back to `app.active_view`.
/// The sessionId branch (step 1) is thus the only matched-build path for `mcp_initialized`.
pub(super) fn handle_mcp_tools_changed(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let method = notif.method.as_ref();

    // Both `x.ai/mcp_initialized` and (newer shell) `x.ai/mcp/tools_changed` carry `sessionId`
    // Route by it so a background agent's notification updates *its* state, not whichever agent is foregrounded
    // Unknown and subagent (child) sessions are dropped; a missing sessionId falls back to the active agent (legacy shells)
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        #[serde(default)]
        session_id: Option<String>,
    }
    let session_id = serde_json::from_str::<Payload>(notif.params.get())
        .ok()
        .and_then(|p| p.session_id);
    let target: Option<(bool, AgentId)> = match session_id.as_deref() {
        Some(sid) => {
            let sid = acp::SessionId::new(sid);
            match find_session_match(app, &sid) {
                // Subagent (child) sessions don't own the top-level MCP modal / connecting indicator; drop them
                Some(SessionMatch::Child(_)) => None,
                Some(matched) => {
                    let id = matched.agent_id();
                    Some((is_matched_agent_active(app, id), id))
                }
                None => None,
            }
        }
        None => match app.active_view {
            ActiveView::Agent(id) => Some((true, id)),
            _ => None,
        },
    };
    let Some((is_active, id)) = target else {
        return false;
    };

    let mut redraw = false;

    // `mcp_initialized` clears the matched agent's connecting indicator.
    if method == "x.ai/mcp_initialized"
        && let Some(agent) = app.agents.get_mut(&id)
        && agent.mcp_init_progress.take().is_some()
    {
        redraw |= is_active;
    }

    // Modal refresh: schedule a debounced refetch for the OWNING agent (routed by sessionId, not active_view), per-agent coalesced
    let modal_open = app
        .agents
        .get(&id)
        .is_some_and(|a| a.extensions_modal.is_some());
    if modal_open
        && !agent_has_pending_mcps_fetch(app, id)
        && let Some(session_id) = app
            .agents
            .get(&id)
            .and_then(|a| a.session.session_id.clone())
    {
        app.pending_effects.push(Effect::FetchMcpsList {
            agent_id: id,
            session_id,
            cache: true,
        });
        redraw |= is_active;
    }
    redraw
}

/// Per-agent coalescing test for [`Effect::FetchMcpsList`].
/// An earlier approach used `matches!(e, FetchMcpsList { .. })`, which collapsed across agents.
/// A pending fetch on agent A would drop the push for agent B.
pub(super) fn agent_has_pending_mcps_fetch(app: &AppView, agent_id: AgentId) -> bool {
    app.pending_effects.iter().any(|e| {
        matches!(
            e,
            Effect::FetchMcpsList { agent_id: a, .. } if *a == agent_id
        )
    })
}

/// Re-uses the shell's canonical wire types instead of re-declaring a parallel pager enum.
/// Later variants (e.g. `RestartSucceeded` / `RestartFailed`) ride through automatically without a pager code change.
/// Returns `true` (request redraw) only when the row mutation happened AND the matched agent is the currently active view.
pub(super) fn handle_mcp_server_status(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    use crate::views::extensions_modal::TabDataState;
    use crate::views::mcps_modal::{McpServerDisplayStatus, McpToolDetail, patch_server_row};
    use distill_shell::extensions::mcp::{McpServerStatus, McpServerStatusPayload, McpToolEntry};

    let Ok(payload) = serde_json::from_str::<McpServerStatusPayload>(notif.params.get()) else {
        tracing::warn!(
            "Failed to parse x.ai/mcp/server_status: {}",
            &notif.params.get()
                [..crate::render::line_utils::floor_char_boundary(notif.params.get(), 100)]
        );
        return false;
    };

    let session_id = acp::SessionId::new(payload.session_id);
    let Some(matched) = find_session_match(app, &session_id) else {
        return false;
    };
    let id = matched.agent_id();
    let is_active = is_matched_agent_active(app, id);
    let Some(agent) = app.agents.get_mut(&id) else {
        return false;
    };
    // Cheap path: modal closed. Drop the push; the next `/mcps` open will fetch a fresh full list.
    let Some(modal) = agent.extensions_modal.as_mut() else {
        return false;
    };
    // Cheap path: list still loading / errored
    // Patching would produce incoherent state; the in-flight fetch will land a consistent snapshot momentarily
    let TabDataState::Loaded(ref mut servers) = modal.mcps_data else {
        return false;
    };
    let display_status = match payload.status {
        McpServerStatus::Ready => McpServerDisplayStatus::Ready,
        McpServerStatus::Initializing => McpServerDisplayStatus::Initializing,
        McpServerStatus::Unavailable => McpServerDisplayStatus::Unavailable,
        McpServerStatus::NeedsAuth => McpServerDisplayStatus::NeedsAuth,
    };
    // Decode `tools` loosely. Shell types this as `Option<serde_json::Value>` (always `null` today; reserved).
    // If the value is present but not an array of `McpToolEntry`-shaped objects we drop ONLY the tools update and still apply the status
    // The previous strict typing would have dropped the whole push on any shape mismatch
    let new_tools = payload.tools.and_then(|raw| {
        match serde_json::from_value::<Vec<McpToolEntry>>(raw) {
            Ok(entries) => Some(
                entries
                    .into_iter()
                    .map(|t| McpToolDetail {
                        name: t.name,
                        display_name: t.display_name,
                        description: t.description,
                        enabled: t.enabled,
                    })
                    .collect::<Vec<_>>(),
            ),
            Err(e) => {
                tracing::warn!(
                    server = %payload.name,
                    error = %e,
                    "x.ai/mcp/server_status: tools field present but not Vec<McpToolEntry>; status still applied"
                );
                None
            }
        }
    });
    let mutated = patch_server_row(servers, &payload.name, display_status, new_tools);
    mutated && is_active
}

/// Handle `x.ai/mcp/elicit_complete`: dismiss the matched agent's URL-mode elicitation card that is still waiting on this `elicitation_id`.
pub(super) fn handle_mcp_elicit_complete(notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    let Ok(payload) = serde_json::from_str::<
        distill_tools::mcp_elicitation::McpElicitCompletePayload,
    >(notif.params.get()) else {
        return false;
    };
    let session_id = acp::SessionId::new(payload.session_id);
    let Some(matched) = find_session_match(app, &session_id) else {
        return false;
    };
    let id = matched.agent_id();
    let Some(agent) = app.agents.get_mut(&id) else {
        return false;
    };
    agent.dismiss_waiting_elicitation(&payload.elicitation_id, payload.server_name.as_deref())
}

/// Handle `x.ai/mcp/servers_updated`.
/// An attempt to route by `sessionId` therefore always fell back to `app.active_view` and re-created the multi-agent bug.
/// Agents without an open modal drop the push (cheap path).
pub(super) fn handle_mcp_servers_updated(_notif: &acp::ExtNotification, app: &mut AppView) -> bool {
    // `_notif` is intentionally unread
    // Do NOT "fix" this back to per-session routing without re-reading the rustdoc above
    // Snapshot (agent_id, session_id, modal_open) up front so the mutable `pending_effects` borrow can proceed without aliasing `app.agents`
    let targets: Vec<(AgentId, acp::SessionId)> = app
        .agents
        .iter()
        .filter_map(|(id, agent)| {
            if agent.extensions_modal.is_some() {
                agent.session.session_id.clone().map(|sid| (*id, sid))
            } else {
                None
            }
        })
        .collect();
    if targets.is_empty() {
        return false;
    }
    let mut redraw = false;
    for (id, session_id) in targets {
        if agent_has_pending_mcps_fetch(app, id) {
            continue;
        }
        let is_active = is_matched_agent_active(app, id);
        app.pending_effects.push(Effect::FetchMcpsList {
            agent_id: id,
            session_id,
            cache: true,
        });
        redraw |= is_active;
    }
    redraw
}
