// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;
    use crate::app::actions::PermissionLabel;
    use distill_shell::extensions::notification::SessionUpdate as XaiSessionUpdate;

    #[test]
    fn exit_plan_mode_auto_opens_inline_cursor_plan_preview() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
        }
        let (ext, _rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# Cursor Plan"));

        assert!(handle_exit_plan_mode(ext, &mut app));
        let agent = app.agents.get(&AgentId(0)).unwrap();

        assert!(agent.plan_approval_view.is_some());
        assert_eq!(
            agent
                .line_viewer
                .as_ref()
                .and_then(|v| v.markdown_content_for_test()),
            Some("# Cursor Plan")
        );
    }

    #[test]
    fn exit_plan_keeps_inline_plan_preview_available() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
        }
        let (ext, _rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# First Plan"));

        assert!(handle_exit_plan_mode(ext, &mut app));
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            assert_eq!(
                agent.plan_approval_view.as_ref().map(|s| s.source),
                Some(crate::views::plan_approval_view::PlanReviewSource::Inline)
            );
            agent.line_viewer = None;
            agent.show_plan_preview();
            assert_eq!(
                agent
                    .line_viewer
                    .as_ref()
                    .and_then(|v| v.markdown_content_for_test()),
                Some("# First Plan")
            );
        }
    }

    #[test]
    fn exit_plan_without_inline_content_uses_file_backed_source() {
        let mut app = make_app_with_agent("sess-1");
        let (ext, _rx) = make_exit_plan_ext(Some("# File Plan"));

        assert!(handle_exit_plan_mode(ext, &mut app));
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(agent.kept_plan.body().is_none());

        assert_eq!(
            agent.plan_approval_view.as_ref().map(|s| s.source),
            Some(crate::views::plan_approval_view::PlanReviewSource::FileBacked)
        );
        // A file-backed body still opens from the request's plan_content even when plan.md is not on disk under the agent's cwd
        assert_eq!(
            agent
                .line_viewer
                .as_ref()
                .and_then(|v| v.markdown_content_for_test()),
            Some("# File Plan")
        );
    }

    #[test]
    fn exit_plan_mode_empty_opens_placeholder_preview() {
        // An empty plan.md must still open the approval UI
        // Otherwise the user only sees "Waiting on plan approval" with a dead Tab:plan and thinks the session is stuck
        let mut app = make_app_with_agent("sess-1");
        let (ext, _rx) = make_exit_plan_ext(None);

        assert!(handle_exit_plan_mode(ext, &mut app));
        let agent = app.agents.get(&AgentId(0)).unwrap();

        let pav = agent
            .plan_approval_view
            .as_ref()
            .expect("plan_approval_view must be set");
        assert!(!pav.has_plan);
        assert_eq!(
            pav.focus,
            crate::views::plan_approval_view::PlanApprovalFocus::Preview,
            "empty approval must keep Preview focus once the placeholder opens"
        );
        assert_eq!(
            agent
                .line_viewer
                .as_ref()
                .and_then(|v| v.markdown_content_for_test()),
            Some(crate::views::plan_approval_view::EMPTY_PLAN_PLACEHOLDER)
        );
    }

    #[test]
    fn exit_plan_mode_dismisses_open_modal() {
        // Regression: if the Ctrl+P command palette is open when the agent calls exit_plan_mode, the modal must be dismissed
        // Otherwise the modal hides the line viewer in draw order while input routes to the invisible line viewer, leaving the user stuck
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
            agent.active_modal = Some(crate::views::modal::ActiveModal::CommandPalette {
                entries: crate::views::modal::default_palette_entries(
                    agent.sharing_enabled,
                    &agent.prompt.slash_controller,
                ),
                state: crate::views::picker::PickerState::input_active(),
                window: crate::views::modal_window::ModalWindowState::new(),
            });
        }

        let (ext, _rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# Cursor Plan"));
        assert!(handle_exit_plan_mode(ext, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.active_modal.is_none(),
            "exit_plan_mode must dismiss the open modal so the plan preview is visible"
        );
        assert!(agent.plan_approval_view.is_some());
        assert!(agent.line_viewer.is_some());
    }

    #[test]
    fn exit_plan_mode_dismisses_open_block_viewer() {
        // Regression: if an Edit/tool block_viewer is open when exit_plan_mode opens, dismiss it so wheel scroll reaches the plan line_viewer
        // Draw returns on line_viewer (the plan stays visible), but handle_scroll prefers block_viewer while it remains in state
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
            agent.block_viewer = Some(crate::views::block_viewer::BlockViewerPane::for_plain_text(
                "edit",
                "diff content",
            ));
        }

        let (ext, _rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# Cursor Plan"));
        assert!(handle_exit_plan_mode(ext, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.block_viewer.is_none(),
            "exit_plan_mode must dismiss open block_viewer so the plan can scroll"
        );
        assert!(agent.plan_approval_view.is_some());
        assert!(agent.line_viewer.is_some());
    }

    #[test]
    fn later_empty_exit_plan_request_clears_stale_inline_plan() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
        }
        let (first, _first_rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# First Plan"));
        let (second, _second_rx) = make_exit_plan_ext(None);

        assert!(handle_exit_plan_mode(first, &mut app));
        {
            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                agent.kept_plan.body(),
                Some("# First Plan")
            );
        }
        assert!(handle_exit_plan_mode(second, &mut app));
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(agent.kept_plan.body().is_none());
        // An empty approval still opens the placeholder, not a silent "no plan" toast, so the user always sees a way to proceed
        assert_eq!(
            agent
                .line_viewer
                .as_ref()
                .and_then(|v| v.markdown_content_for_test()),
            Some(crate::views::plan_approval_view::EMPTY_PLAN_PLACEHOLDER)
        );
    }

    #[test]
    fn later_oversized_exit_plan_request_clears_stale_inline_plan() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "create-plan-call", "CreatePlan");
        }
        let (first, _first_rx) =
            make_exit_plan_ext_with_tool_call_id("create-plan-call", Some("# First Plan"));
        let over = "x".repeat(
            usize::try_from(crate::app::agent_view::MAX_KEPT_PLAN_FILE_BYTES.saturating_add(1))
                .expect("cap fits usize"),
        );
        let (second, _second_rx) = make_exit_plan_ext(Some(over.as_str()));

        assert!(handle_exit_plan_mode(first, &mut app));
        assert!(handle_exit_plan_mode(second, &mut app));
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            !agent.kept_plan.is_kept(),
            "oversized inline must not leave the prior keep"
        );
        assert_ne!(
            agent
                .line_viewer
                .as_ref()
                .and_then(|v| v.markdown_content_for_test()),
            Some("# First Plan"),
            "preview must not fall back to the stale keep"
        );
    }

    #[test]
    fn exit_plan_mode_shows_overlay() {
        let mut app = make_app_with_agent("sess-A");
        assert!(!app.agents.get(&AgentId(0)).unwrap().session.is_yolo());

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-normal".into(),
            plan_content: Some("# Plan\nDo stuff".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        let msg = AcpClientMessage::ExtMethod(distill_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(affected, "opening the overlay should need a redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.plan_approval_view.is_some(),
            "plan_approval_view must be set for interactive approval"
        );
        assert!(
            rx.try_recv().is_err(),
            "response must NOT have been sent yet (waiting for user)"
        );
    }

    #[test]
    fn exit_plan_mode_shows_overlay_even_in_yolo() {
        let mut app = make_app_with_agent("sess-A");
        app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = true;

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-yolo".into(),
            plan_content: Some("# Plan\nDo stuff".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        let msg = AcpClientMessage::ExtMethod(distill_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(affected, "overlay should open even in yolo mode");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.plan_approval_view.is_some(),
            "plan_approval_view must be set even in always-approve mode"
        );
        assert!(
            rx.try_recv().is_err(),
            "response must NOT have been sent yet (waiting for user)"
        );
    }

    #[test]
    fn exit_plan_mode_routes_to_background_session_not_active_view() {
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-B".into(),
            tool_call_id: "tc-bg-plan".into(),
            plan_content: Some("# Plan".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        let msg = AcpClientMessage::ExtMethod(distill_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(
            !affected,
            "a background-session plan approval must not redraw the active view"
        );
        assert!(
            app.agents
                .get(&AgentId(1))
                .unwrap()
                .plan_approval_view
                .is_some(),
            "plan approval must be parked on the session that asked (background agent B)"
        );
        assert!(
            app.agents
                .get(&AgentId(0))
                .unwrap()
                .plan_approval_view
                .is_none(),
            "plan approval must NOT land on the unrelated active agent A"
        );
        assert!(rx.try_recv().is_err(), "response must NOT be sent yet");
    }

    /// Regression: tool-call titles containing `"enter_plan_mode"` must not flip plan mode.
    /// The substring matcher used to flip it on any tool mentioning the phrase, e.g. a Grep with that pattern, leaving the session stuck.
    #[test]
    fn tool_call_with_enter_plan_mode_substring_does_not_activate_plan_mode() {
        let mut agent = make_agent(Some("s1"));
        assert!(!agent.plan_mode_active);

        let updates = [
            make_tool_call("enter_plan_mode"),
            make_tool_call_update("enter_plan_mode"),
            make_tool_call("Execute `rg enter_plan_mode`"),
            make_tool_call_update("Execute `rg enter_plan_mode`"),
            make_tool_call_update("Plan mode entered"),
            make_tool_call("mcp__foo__enter_plan_mode"),
        ];
        for update in &updates {
            let transition = detect_plan_mode_change_replayed(update, &mut agent, false);
            assert_eq!(
                transition,
                None,
                "tool-call title (not a CurrentModeUpdate) must not request refresh"
            );
            assert!(
                !agent.plan_mode_active,
                "tool-call title must not flip plan mode"
            );
        }
    }

    /// Symmetric: tool-call titles containing `"exit_plan_mode"` must not deactivate plan mode either.
    /// Exit is signaled by `CurrentModeUpdate`.
    #[test]
    fn tool_call_with_exit_plan_mode_substring_does_not_deactivate_plan_mode() {
        let mut agent = make_agent(Some("s1"));
        agent.plan_mode_active = true;

        let updates = [
            make_tool_call("exit_plan_mode"),
            make_tool_call_update("exit_plan_mode"),
            make_tool_call_update("Plan mode exited"),
            make_tool_call("Execute `rg exit_plan_mode`"),
        ];
        for update in &updates {
            let transition = detect_plan_mode_change_replayed(update, &mut agent, false);
            assert_eq!(transition, None);
            assert!(
                agent.plan_mode_active,
                "tool-call title must not flip plan mode"
            );
        }
    }

    #[test]
    fn detect_plan_mode_change_classifies_transitions() {
        // (staged pending, was_active, mode id; `None` is a tool-call update) -> transition
        let cases = [
            (None, false, Some("plan"), Some(PlanModeTransition::EnteredByAgent)),
            (Some(true), false, Some("plan"), Some(PlanModeTransition::EnteredByUser)),
            (Some(false), false, Some("plan"), Some(PlanModeTransition::EnteredByUser)),
            (Some(true), true, Some("plan"), Some(PlanModeTransition::Unchanged)),
            (Some(true), true, Some("default"), Some(PlanModeTransition::Exited)),
            (None, true, Some("browser_use"), Some(PlanModeTransition::Exited)),
            (Some(true), false, None, None),
        ];
        for (pending, was_active, mode_id, expected) in cases {
            let label = format!("pending={pending:?} was_active={was_active} mode={mode_id:?}");
            let mut agent = make_agent(Some("s1"));
            agent.plan_mode_active = was_active;
            agent.plan_mode_pending = pending;
            let update = match mode_id {
                Some(id) => make_current_mode_update(id),
                None => make_tool_call("enter_plan_mode"),
            };

            let transition = detect_plan_mode_change_replayed(&update, &mut agent, false);

            assert_eq!(transition, expected, "{label}");
            if transition.is_some() {
                assert_eq!(agent.plan_mode_active, mode_id == Some("plan"), "{label}");
                if pending == Some(true) && mode_id == Some("default") {
                    assert_eq!(agent.plan_mode_pending, Some(true), "{label}");
                } else {
                    assert!(agent.plan_mode_pending.is_none(), "{label}");
                }
            } else {
                assert_eq!(agent.plan_mode_active, was_active, "{label}");
                assert_eq!(agent.plan_mode_pending, pending, "{label}");
            }
        }
    }

    fn current_mode_update_msg(session_id: &str, mode_id: &str, is_replay: bool) -> AcpClientMessage {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::SessionNotification::new(
            acp::SessionId::new(session_id),
            make_current_mode_update(mode_id),
        )
        .meta(serde_json::json!({ "isReplay": is_replay }).as_object().cloned());
        AcpClientMessage::SessionNotification(distill_acp_lib::AcpArgs {
            request,
            response_tx: tx,
        })
    }

    fn plan_entry_rows(app: &AppView) -> Vec<PermissionLabel> {
        test_agent(app, AgentId(0))
            .scrollback
            .session_events()
            .into_iter()
            .filter_map(|event| match event {
                SessionEvent::PlanModeEnteredByAgent { permission } => Some(permission),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn agent_plan_entry_pushes_scrollback_row_once() {
        let mut app = make_app_with_agent("sess-plan");
        app.current_ui.permission_mode = Some("auto".into());
        app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = true;

        let _ = handle(current_mode_update_msg("sess-plan", "plan", false), &mut app);
        assert_eq!(plan_entry_rows(&app), vec![PermissionLabel::AlwaysApprove]);
        assert!(test_agent(&app, AgentId(0)).plan_mode_active);

        // `loading_replay` makes the replayed update acceptable, so the gate (not the drop) suppresses the row
        let _ = handle(current_mode_update_msg("sess-plan", "default", false), &mut app);
        app.agents.get_mut(&AgentId(0)).unwrap().session.loading_replay = true;
        let _ = handle(current_mode_update_msg("sess-plan", "plan", true), &mut app);
        assert_eq!(plan_entry_rows(&app).len(), 1, "replayed entry must not add a row");
        assert!(test_agent(&app, AgentId(0)).plan_mode_active, "state still applies on replay");

        let _ = handle(current_mode_update_msg("sess-plan", "default", false), &mut app);
        let _ = handle(current_mode_update_msg("sess-plan", "plan", false), &mut app);
        assert_eq!(plan_entry_rows(&app).len(), 1, "entry during a session load must not add a row");

        app.agents.get_mut(&AgentId(0)).unwrap().session.loading_replay = false;
        for staged in [Some(true), Some(false)] {
            let _ = handle(current_mode_update_msg("sess-plan", "default", false), &mut app);
            app.agents.get_mut(&AgentId(0)).unwrap().plan_mode_pending = staged;
            let _ = handle(current_mode_update_msg("sess-plan", "plan", false), &mut app);
            assert_eq!(
                plan_entry_rows(&app).len(),
                1,
                "user-driven entry (staged {staged:?}) must not add a row"
            );
            assert!(test_agent(&app, AgentId(0)).plan_mode_active);
        }
    }

    fn plan_kept_msg(session_id: &str, plan_uri: &str, content: &str) -> AcpClientMessage {
        make_ext_session_notification(
            session_id,
            XaiSessionUpdate::PlanKept {
                plan_uri: plan_uri.to_owned(),
                content: content.to_owned(),
            },
        )
    }

    fn create_plan_tool_call(text: &str) -> AcpClientMessage {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::SessionNotification::new(
            acp::SessionId::new("sess-plan"),
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new("tc-plan"), "CreatePlan".to_owned())
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Completed)
                    .content(vec![acp::ToolCallContent::Content(acp::Content::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    ))]),
            ),
        );
        AcpClientMessage::SessionNotification(distill_acp_lib::AcpArgs {
            request,
            response_tx: tx,
        })
    }

    #[test]
    fn tool_call_create_plan_does_not_become_a_keep() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
        }
        let _ = handle(create_plan_tool_call("# Build it\n"), &mut app);
        assert!(
            !test_agent(&app, AgentId(0)).kept_plan.is_kept(),
            "keep comes from PlanKept, not a tool-call title"
        );
    }

    #[test]
    fn default_confirm_does_not_cancel_in_turn_review() {
        let mut app = make_app_with_agent("sess-1");
        let (ext, mut rx) = make_exit_plan_ext(Some("# Held plan"));
        assert!(handle_exit_plan_mode(ext, &mut app));
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.plan_mode_active = true;

        let transition = detect_plan_mode_change_replayed(&make_current_mode_update("default"), agent, false);
        assert_eq!(transition, Some(PlanModeTransition::Exited));
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_in_turn()),
            "Shift+Tab Default must not drop a held in-turn review"
        );
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "exit_plan_mode must stay outstanding"
        );
    }

    #[test]
    fn default_confirm_still_dismisses_post_turn_review() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.plan_mode_active = true;
        agent.post_turn_plan_review = true;
        agent.kept_plan = crate::app::agent_view::KeptPlan::kept(Some("# Build it\n".to_owned()), None);
        agent.open_post_turn_plan_review();
        assert!(agent.plan_approval_view.as_ref().is_some_and(|pav| pav.is_after_turn()));

        detect_plan_mode_change_replayed(&make_current_mode_update("default"), agent, false);
        assert!(
            agent.plan_approval_view.is_none(),
            "Default still drops a post-turn review after last_plan is gone"
        );
        assert!(!agent.kept_plan.is_kept());
    }

    #[test]
    fn handle_end_turn_then_plan_kept_opens_post_turn_review() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
            agent.begin_local_turn("p-plan");
        }
        prompt_response(&mut app, "p-plan");
        assert!(
            test_agent(&app, AgentId(0)).plan_approval_view.is_none(),
            "EndTurn with no keep must not mount approve/build"
        );
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .and_then(|pav| pav.plan_content.as_deref()),
            Some("# Build it\n"),
        );
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_after_turn()),
            "late PlanKept after EndTurn must open approve/build"
        );
    }

    #[test]
    fn handle_plan_kept_then_end_turn_opens_post_turn_review() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
            agent.begin_local_turn("p-plan");
        }
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        assert_eq!(
            Some("# Build it\n"),
            test_agent(&app, AgentId(0)).kept_plan.body(),
            "PlanKept must set the keep through handle()"
        );
        prompt_response(&mut app, "p-plan");
        let agent = test_agent(&app, AgentId(0));
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .and_then(|pav| pav.plan_content.as_deref()),
            Some("# Build it\n"),
        );
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_after_turn()),
            "EndTurn must open approve/build from PlanKept"
        );
    }

    #[test]
    fn plan_kept_replaces_the_keep_and_refreshes_mounted_review() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
        }
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/first.plan.md", "# First body\n"),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .open_post_turn_plan_review();
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/second.plan.md", "# Second body\n"),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .and_then(|pav| pav.plan_content.as_deref()),
            Some("# Second body\n"),
        );
        assert_eq!(
            agent.kept_plan.path(),
            Some(std::path::Path::new("/tmp/second.plan.md")),
        );
    }

    #[test]
    fn plan_cleared_forgets_the_keep() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
        }
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .open_post_turn_plan_review();
        let _ = handle(
            make_ext_session_notification("sess-plan", XaiSessionUpdate::PlanCleared),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert!(!agent.kept_plan.is_kept());
        assert!(agent.plan_approval_view.is_none());
    }

    #[test]
    fn plan_cleared_during_staged_reentry_keeps_the_review() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
        }
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_post_turn_plan_review();
            agent.stage_plan_mode(true);
        }
        // confirm_mode: CurrentModeUpdate(Default) then PlanCleared.
        let _ = handle(current_mode_update_msg("sess-plan", "default", false), &mut app);
        let _ = handle(
            make_ext_session_notification("sess-plan", XaiSessionUpdate::PlanCleared),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert!(
            agent.kept_plan.is_kept(),
            "PlanCleared during staged re-entry must keep the plan"
        );
        assert!(
            agent
                .plan_approval_view
                .as_ref()
                .is_some_and(|pav| pav.is_after_turn()),
            "PlanCleared during staged re-entry must leave approve/build mounted"
        );
        assert_eq!(agent.plan_mode_pending, Some(true));
    }

    #[test]
    fn plan_executing_commits_approved() {
        let mut app = make_app_with_agent("sess-plan");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.post_turn_plan_review = true;
            agent.plan_mode_active = true;
        }
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .open_post_turn_plan_review();
        let _ = handle(
            make_ext_session_notification(
                "sess-plan",
                XaiSessionUpdate::PlanExecuting,
            ),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert!(agent.plan_approval_view.is_none());
        assert!(!agent.plan_mode_active);
    }

    #[test]
    fn replayed_plan_kept_does_not_set_a_keep() {
        let mut app = make_app_with_agent("sess-plan");
        app.agents.get_mut(&AgentId(0)).unwrap().session.loading_replay = true;
        let _ = handle(
            plan_kept_msg("sess-plan", "file:///tmp/p.plan.md", "# Build it\n"),
            &mut app,
        );
        assert!(!test_agent(&app, AgentId(0)).kept_plan.is_kept());
    }

