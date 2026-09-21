// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;

pub(super) fn handle_permission_request(
    perm: distill_acp_lib::AcpArgs<acp::RequestPermissionRequest>,
    app: &mut AppView,
) -> bool {
    let matched = match find_session_match(app, &perm.request.session_id) {
        Some(m) => m,
        None => {
            tracing::warn!(
                session_id = %perm.request.session_id.0,
                "Permission request for unknown session_id; cancelling"
            );
            cancel_permission(perm);
            return false;
        }
    };
    let owning_agent_id = matched.agent_id();
    let is_active = is_matched_agent_active(app, owning_agent_id);
    let Some(agent) = app.agents.get_mut(&owning_agent_id) else {
        cancel_permission(perm);
        return false;
    };

    if agent.session.is_yolo()
        && let Some(allow) = perm
            .request
            .options
            .iter()
            .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
    {
        let option_id = allow.option_id.clone();
        perm.response_tx
            .send(Ok(acp::RequestPermissionResponse::new(
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    option_id,
                )),
            )))
            .ok();
        return false;
    }

    if !app
        .notification_service
        .should_suppress_permission_notification()
    {
        app.notification_service.notify(NotificationEvent {
            kind: NotificationEventKind::ApprovalRequired,
            title: "Grok".into(),
            body: NotificationEventKind::ApprovalRequired.as_ref().into(),
            session_id: Some(perm.request.session_id.0.to_string()),
        });
        app.notification_service.mark_permission_notified();
    }

    let needs_redraw = enqueue_permission(perm, agent);
    needs_redraw && is_active
}

fn enqueue_permission(
    perm: distill_acp_lib::AcpArgs<acp::RequestPermissionRequest>,
    agent: &mut AgentView,
) -> bool {
    // Mandatory ingress wins: evict an open feedback modal before the permission stashes the composer.
    agent.displace_feedback_modal(
        crate::views::feedback_modal::FeedbackModalDisplacement::Permission,
    );

    let bash_highlights: Option<BashCommandHighlights> = perm
        .request
        .meta
        .as_ref()
        .and_then(|meta| serde_json::from_value(serde_json::Value::Object(meta.clone())).ok());
    let bash_selection_count = bash_highlights
        .as_ref()
        .map(|h| distill_workspace::permission::default_always_allow_scope(&h.highlighted_words))
        .unwrap_or(0);
    let bash_deny_selection_count = bash_highlights
        .as_ref()
        .map(|h| distill_workspace::permission::default_always_deny_scope(&h.highlighted_words))
        .unwrap_or(0);

    if let Some(h) = bash_highlights.as_ref() {
        let offers_allow_row = perm.request.options.iter().any(|o| {
            o.option_id.0.as_ref() == crate::views::permission_view::ALLOW_ALWAYS_COMMAND_OPTION_ID
        });
        if offers_allow_row
            && !distill_workspace::permission::always_allow_scope_persists(h, bash_selection_count)
        {
            tracing::warn!(
                scope = bash_selection_count,
                words = ?h.highlighted_words,
                "allow-always-command row offered but default scope does not persist"
            );
        }
    }

    let mcp_scope = perm
        .request
        .options
        .iter()
        .find(|o| o.option_id.0.as_ref() == "allow-always-mcp")
        .and_then(|opt| opt.meta.as_ref())
        .and_then(|m| {
            serde_json::from_value::<distill_workspace::permission::McpToolPermission>(
                serde_json::Value::Object(m.clone()),
            )
            .ok()
        })
        .map(|perm| McpScopeState {
            tool_name: perm.tool_name,
            server_prefix: perm.server_prefix,
            selected: McpScope::Tool,
        });

    let subagent_label = resolve_subagent_label(agent, &perm.request.session_id);

    let (title, description, bash_command_raw) = build_permission_display(
        &perm.request,
        bash_highlights.as_ref(),
        #[cfg(feature = "local-workspace")]
        matches!(
            agent.workspace_mode,
            crate::views::welcome::WelcomeWorkspaceMode::LocalWorkspace
        ),
        #[cfg(not(feature = "local-workspace"))]
        false,
    );

    let perm_id = agent.next_perm_req_id;
    agent.next_perm_req_id += 1;

    if agent.permission_queue.is_empty() && agent.permission_stashed_prompt.is_none() {
        agent.permission_stashed_prompt = Some(agent.prompt.stash());
        agent.prompt.set_text("");
    }

    if agent.permission_queue.is_empty()
        && agent.active_pane == AgentPane::Scrollback
        && agent.permission_stashed_pane.is_none()
    {
        agent.permission_stashed_pane = Some(AgentPane::Scrollback);
        agent.set_active_pane(AgentPane::Prompt, true);
    }

    let options = perm.request.options.clone();

    let active_idx = crate::appearance::permission_cursor::resolve_initial_cursor(&options);

    agent.permission_queue.push_back(PermissionViewState {
        request: perm,
        id: perm_id,
        focus: PermissionFocus::Options,
        options,
        active_idx,
        bash_highlights,
        bash_selection_count,
        bash_deny_selection_count,
        bash_command_raw,
        mcp_scope,
        title,
        description,
        args_expanded: false,
        desc_scroll: 0,
        subagent_label,
        options_area_height: 0,
        options_scroll_offset: 0,
    });

    agent.last_active_at = Some(std::time::Instant::now());

    true
}

fn resolve_subagent_label(agent: &AgentView, session_id: &acp::SessionId) -> Option<String> {
    let sid = session_id.0.as_ref();
    if let Some(ref root_sid) = agent.session.session_id
        && root_sid.0.as_ref() == sid
    {
        return None;
    }
    if let Some(info) = agent.subagent_sessions.get(sid) {
        return Some(format!(
            "Subagent \"{}\" ({}):",
            info.description, info.subagent_type
        ));
    }
    Some("Child session (untracked):".to_string())
}

pub(super) fn build_permission_display(
    req: &acp::RequestPermissionRequest,
    bash_highlights: Option<&BashCommandHighlights>,
    session_local_workspace: bool,
) -> (String, Vec<String>, Option<String>) {
    let is_bash = bash_highlights.is_some();

    let bash_input = req.tool_call.fields.raw_input.as_ref().and_then(|v| {
        serde_json::from_value::<distill_tools::implementations::BashToolInput>(v.clone()).ok()
    });

    let ask = hook_ask(req);
    let acp_title = req
        .tool_call
        .fields
        .title
        .as_deref()
        .map(|title| match &ask {
            Some(ask) => ask.strip_prompt_header(title),
            None => title,
        });

    let raw_command = bash_input.as_ref().map(|b| b.command.clone()).or_else(|| {
        acp_title
            .and_then(|t| t.strip_prefix("Execute `"))
            .and_then(|t| t.strip_suffix('`'))
            .map(|s| s.to_string())
    });

    let bash_description = bash_input.map(|b| b.description);

    let is_execute = is_bash
        || req.tool_call.fields.kind == Some(acp::ToolKind::Execute)
        || raw_command.is_some();

    let title = if is_execute {
        bash_description
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .unwrap_or_else(
                || match bash_highlights.and_then(|h| h.highlighted_words.first()) {
                    Some(bin) => format!("Allow `{bin}`?"),
                    None => "Allow Execute?".to_string(),
                },
            )
    } else if is_edit_permission(req) {
        let file_path = req
            .tool_call
            .fields
            .raw_input
            .as_ref()
            .and_then(|v| v.get("file_path"))
            .and_then(|v| v.as_str());
        if let Some(path) = file_path {
            format!("Allow Edit to {}?", path)
        } else if let Some(t) = acp_title {
            format!(
                "Allow {}?",
                distill_workspace::permission::mcp_pretty_name_if_qualified(t)
            )
        } else {
            "Allow Edit?".to_string()
        }
    } else if let Some(t) = acp_title {
        format!(
            "Allow {}?",
            distill_workspace::permission::mcp_pretty_name_if_qualified(t)
        )
    } else {
        match req.tool_call.fields.kind {
            Some(acp::ToolKind::Edit) => "Allow Edit?".to_string(),
            Some(acp::ToolKind::Execute) => "Allow Execute?".to_string(),
            Some(acp::ToolKind::Delete) => "Allow Delete?".to_string(),
            _ => "Allow?".to_string(),
        }
    };

    let title = qualify_permission_title_for_local_workspace(title, session_local_workspace);
    let description = permission_description_lines(req, ask.as_ref());
    let bash_cmd = if is_execute { raw_command } else { None };
    (title, description, bash_cmd)
}

fn qualify_permission_title_for_local_workspace(
    title: String,
    session_local_workspace: bool,
) -> String {
    if !session_local_workspace {
        return title;
    }
    if title.contains("on your machine") {
        return title;
    }
    if let Some(stripped) = title.strip_suffix('?') {
        return format!("{stripped} (on your machine)?");
    }
    format!("{title} (on your machine)")
}

fn permission_description_lines(
    req: &acp::RequestPermissionRequest,
    hook_ask: Option<&distill_workspace::permission::HookAsk>,
) -> Vec<String> {
    let mut lines = mcp_args_lines(req);
    if is_edit_permission(req)
        && let Some(desc) = protected_edit_description(req)
    {
        lines.insert(0, desc);
    }
    if let Some(ask) = hook_ask {
        lines.insert(0, ask.ask_line());
    }
    lines
}

fn hook_ask(req: &acp::RequestPermissionRequest) -> Option<distill_workspace::permission::HookAsk> {
    let value = req
        .meta
        .as_ref()?
        .get(distill_workspace::permission::HOOK_ASK_META_KEY)?;
    serde_json::from_value(value.clone()).ok()
}

fn protected_edit_description(req: &acp::RequestPermissionRequest) -> Option<String> {
    let meta = req.meta.as_ref()?;
    let protected: distill_workspace::permission::ProtectedEditPermission =
        serde_json::from_value(serde_json::Value::Object(meta.clone())).ok()?;
    protected.description.filter(|s| !s.is_empty())
}

pub(super) const MCP_ARGS_MAX_LINES: usize = 200;

pub(super) const MCP_ARGS_MAX_LINE_CHARS: usize = 2000;

pub(super) fn mcp_args_lines(req: &acp::RequestPermissionRequest) -> Vec<String> {
    let Some(raw) = req.tool_call.fields.raw_input.as_ref() else {
        return Vec::new();
    };
    let is_mcp = matches!(
        raw.get("variant").and_then(|v| v.as_str()),
        Some("UseTool") | Some("MCPTool")
    );
    if !is_mcp {
        return Vec::new();
    }
    // A tool that takes no arguments arrives as `{}`; there is nothing to show for it.
    let args = match raw.get("tool_input") {
        Some(serde_json::Value::Null) | None => return Vec::new(),
        Some(serde_json::Value::Object(map)) if map.is_empty() => return Vec::new(),
        Some(serde_json::Value::Array(items)) if items.is_empty() => return Vec::new(),
        Some(args) => args,
    };
    let pretty = match serde_json::to_string_pretty(args) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = pretty
        .lines()
        .map(|l| match l.char_indices().nth(MCP_ARGS_MAX_LINE_CHARS) {
            Some((byte_idx, _)) => match l.get(..byte_idx) {
                Some(prefix) => format!("{prefix}…"),
                None => l.to_owned(),
            },
            None => l.to_owned(),
        })
        .collect();
    if lines.len() > MCP_ARGS_MAX_LINES {
        let hidden = lines.len() - MCP_ARGS_MAX_LINES;
        lines.truncate(MCP_ARGS_MAX_LINES);
        lines.push(format!("… (+{hidden} more lines)"));
    }
    lines
}

fn is_edit_permission(req: &acp::RequestPermissionRequest) -> bool {
    req.options.iter().any(|o| {
        o.kind == acp::PermissionOptionKind::AllowAlways && o.name.to_lowercase().contains("edit")
    })
}

fn cancel_permission(perm: distill_acp_lib::AcpArgs<acp::RequestPermissionRequest>) {
    perm.response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Cancelled,
        )))
        .ok();
}

pub(super) fn should_drop_late_auto_recap(
    auto: bool,
    is_replay: bool,
    agent: &crate::app::agent_view::AgentView,
) -> bool {
    auto && !is_replay && !cli_is_idle_for_recap(agent)
}

fn cli_is_idle_for_recap(agent: &crate::app::agent_view::AgentView) -> bool {
    if !agent.session.state.is_idle() || agent.has_wake_source() {
        return false;
    }
    if agent.session.in_flight_prompt.is_some() || agent.has_held_user_queue() {
        return false;
    }
    if scrollback_waiting_on_user_turn(&agent.scrollback) {
        return false;
    }
    true
}

fn scrollback_waiting_on_user_turn(scrollback: &crate::scrollback::state::ScrollbackState) -> bool {
    use crate::scrollback::block::RenderBlock;

    for idx in (0..scrollback.len()).rev() {
        let Some(entry) = scrollback.get(idx) else {
            continue;
        };
        if entry.block.is_user_prompt() {
            return true;
        }
        if matches!(
            &entry.block,
            RenderBlock::AgentMessage(_) | RenderBlock::Thinking(_) | RenderBlock::ToolCall(_)
        ) {
            return false;
        }
        if let RenderBlock::SessionEvent(b) = &entry.block
            && session_event_settles_turn(&b.event)
        {
            return false;
        }
    }
    false
}

fn session_event_settles_turn(event: &crate::scrollback::blocks::SessionEvent) -> bool {
    use crate::scrollback::blocks::SessionEvent;

    event.is_turn_terminal()
        || matches!(
            event,
            SessionEvent::ReAuthRequired
                | SessionEvent::ContextTooLarge { .. }
                | SessionEvent::DiskFull
                | SessionEvent::CompactionFailed { .. }
                | SessionEvent::RetryFailed { .. }
        )
}

pub(super) fn should_drop_duplicate_auto_recap(
    auto: bool,
    is_replay: bool,
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    auto && !is_replay && scrollback_has_recap_since_last_user(scrollback)
}

fn scrollback_has_recap_since_last_user(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::blocks::SessionEvent;

    let mut recap_since_user = false;
    for (_, entry) in scrollback.iter_entries() {
        if entry.block.is_user_prompt() {
            recap_since_user = false;
            continue;
        }
        if let RenderBlock::SessionEvent(b) = &entry.block
            && matches!(b.event, SessionEvent::Recap { .. })
        {
            recap_since_user = true;
        }
    }
    recap_since_user
}

pub(super) fn apply_recap_block(agent: &mut AgentView, auto: bool, recap_block: RenderBlock) {
    let fill_id = if auto {
        None
    } else {
        agent
            .pending_recap_entry
            .take()
            .filter(|&id| agent.scrollback.get_by_id(id).is_some())
    };
    match fill_id {
        Some(id) if agent.scrollback.is_committed(id) => {
            agent.scrollback.remove_entry(id);
            agent.scrollback.push_block(recap_block);
        }
        Some(id) => {
            if let Some(entry) = agent.scrollback.get_by_id_mut(id) {
                entry.block = recap_block;
            }
            agent.scrollback.finish_running(id);
        }
        None => {
            agent.scrollback.push_block(recap_block);
        }
    }
}

#[cfg(all(test, feature = "local-workspace"))]
mod tests {
    use super::*;

    #[test]
    fn permission_title_qualifies_for_local_workspace() {
        assert_eq!(
            qualify_permission_title_for_local_workspace("Allow Edit?".into(), false),
            "Allow Edit?"
        );
        assert_eq!(
            qualify_permission_title_for_local_workspace("Allow Edit?".into(), true),
            "Allow Edit (on your machine)?"
        );
    }
}
