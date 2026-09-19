#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;
    use crate::app::agent_view::ChildLink;

    #[test]
    fn replayed_subagent_finished_marks_orphan_terminal() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        let spawned = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_spawned",
                "subagent_id": "sa-1",
                "parent_session_id": "sess-1",
                "child_session_id": "child-1",
                "subagent_type": "general-purpose",
                "description": "orphan review",
            }),
            "sess-1-1",
        );
        handle_ext_notification(&spawned, &mut app);

        let finished = subagent_ext_replay(
            "sess-1",
            serde_json::json!({
                "sessionUpdate": "subagent_finished",
                "subagent_id": "sa-1",
                "child_session_id": "child-1",
                "status": "cancelled",
                "error": "interrupted by process restart",
                "tool_calls": 0,
                "turns": 0,
                "duration_ms": 1000,
                "tokens_used": 0,
            }),
            "sess-1-2",
        );
        handle_ext_notification(&finished, &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent
            .subagent_sessions
            .get("child-1")
            .expect("subagent present after replay");
        assert!(
            info.is_finished(),
            "orphan must be terminal after replayed subagent_finished"
        );
        assert_eq!(info.attempt.status.as_deref(), Some("cancelled"));
    }

    #[test]
    fn killing_a_stuck_orphan_records_the_shell_reported_status() {
        // The kill call carries the shell's terminal status: "cancelled" is the default when nothing was live, "completed" is a real terminal report
        for status in ["cancelled", "completed"] {
            let mut app = make_app_with_agent("sess-1");
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            let spawned = subagent_ext_replay(
                "sess-1",
                serde_json::json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagent_id": "sa-1",
                    "parent_session_id": "sess-1",
                    "child_session_id": "child-1",
                    "subagent_type": "general-purpose",
                    "description": "orphan review",
                }),
                "sess-1-1",
            );
            handle_ext_notification(&spawned, &mut app);

            {
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                agent.session.loading_replay = false;
                let info = agent.subagent_sessions.get_mut("child-1").unwrap();
                assert!(info.is_running());
                info.attempt.pending_kill = true;
                info.attempt.kill_requested_at = Some(std::time::Instant::now());
            }

            let finalized = finalize_killed_subagent(
                &mut app,
                &acp::SessionId::new("sess-1".to_owned()),
                "sa-1",
                None,
                status,
            );
            assert!(finalized, "the stuck orphan row must be finalized");

            let agent = app.agents.get(&AgentId(0)).unwrap();
            let info = agent.subagent_sessions.get("child-1").unwrap();
            assert!(info.is_finished(), "kill must finalize the stuck orphan");
            assert_eq!(info.attempt.status.as_deref(), Some(status));
            assert!(!info.attempt.pending_kill, "pending_kill must clear so it can't revert");
            assert!(info.attempt.kill_requested_at.is_none());
            assert!(finalize_killed_subagent(
                &mut app,
                &acp::SessionId::new("sess-1".to_owned()),
                "sa-1",
                None,
                "cancelled",
            ));
            let info = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get("child-1").unwrap_or_else(|| panic!("missing map entry"));
            assert!(info.is_finished());
            assert_eq!(info.attempt.status.as_deref(), Some(status));
        }
    }

    /// The spawn arm links the child to the wire's parent session; nothing on the wire makes it addressable.
    #[test]
    fn spawn_links_child_to_wire_parent_session() {
        let mut app = make_app_with_agent("sess-1");
        let spawn = make_ext_session_notification(
            "sess-1",
            test_subagent_spawned("sess-1", "child-1"),
        );
        assert!(handle(spawn, &mut app));
        assert_eq!(
            Some(&ChildLink::unaddressable(acp::SessionId::new("sess-1"))),
            app.agents
                .get(&AgentId(0))
                .unwrap_or_else(|| panic!("missing root agent"))
                .subagent_view("child-1")
                .expect("spawn inserts the child view")
                .child_link()
        );
    }

    #[test]
    fn killing_an_already_finished_subagent_keeps_its_real_status() {
        let mut app = make_app_with_agent("sess-1");
        let spawn = make_ext_session_notification(
            "sess-1",
            test_subagent_spawned("sess-1", "child-1"),
        );
        assert!(handle(spawn, &mut app));
        let finish = make_ext_session_notification(
            "sess-1",
            XaiSessionUpdate::SubagentFinished {
                subagent_id: "child-1".into(),
                attempt_id: Some("at1.one".into()),
                child_session_id: "child-1".into(),
                status: "failed".into(),
                error: Some("real failure".into()),
                tool_calls: 7,
                turns: 3,
                duration_ms: 9_876,
                tokens_used: 543,
                output: None,
                will_wake: false,
            },
        );
        assert!(handle(finish, &mut app));
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut("child-1")
            .unwrap()
            .attempt.pending_kill = true;

        assert!(finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "child-1",
            Some("at1.one"),
            "cancelled",
        ));

        let info = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get("child-1").unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.is_finished());
        assert_eq!(
            info.attempt.status.as_deref(),
            Some("failed"),
            "retained terminal status must win over the kill-call default"
        );
        assert_eq!(info.attempt.error.as_deref(), Some("real failure"));
        assert_eq!(info.attempt.tool_calls, Some(7));
        assert_eq!(info.attempt.turns, Some(3));
        assert_eq!(info.attempt.duration_ms, Some(9_876));
        assert_eq!(info.attempt.tokens_used, Some(543));
        assert!(!info.attempt.pending_kill);
        let entry_id = info.attempt.scrollback_entry_id.unwrap();
        let entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected subagent row");
        };
        assert!(
            matches!(sb.kind, SubagentBlockKind::Failed { .. }),
            "parent row must keep Failed, not repaint as Cancelled/Completed"
        );
    }

    #[test]
    fn child_spawn_sets_read_only_pane() {
        use crate::views::queue_mutation::QueueMutation;

        let mut app = make_app_with_agent("sess-1");
        assert!(handle(
            make_ext_session_notification("sess-1", test_subagent_spawned("sess-1", "child-1")),
            &mut app,
        ));
        let root = app
            .agents
            .get(&AgentId(0))
            .unwrap_or_else(|| panic!("missing root agent"));
        assert_eq!(QueueMutation::PerRowKind, root.queue.mutation());
        let child = root
            .subagent_views
            .get("child-1")
            .unwrap_or_else(|| panic!("missing child view"));
        assert_eq!(QueueMutation::ReadOnly, child.queue.mutation());
    }

    #[test]
    fn refreshing_a_killed_background_child_adds_no_duplicate_row_or_footer() {
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .tracker
            .task_tool_background
            .insert("child-bg".into(), true);
        assert!(handle(
            make_ext_session_notification("sess-1", test_subagent_spawned("sess-1", "child-bg")),
            &mut app,
        ));
        assert!(handle(
            make_ext_session_notification("sess-1", test_subagent_finished("child-bg")),
            &mut app,
        ));
        let terminal_rows = |app: &crate::app::app_view::AppView| {
            (0..app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len())
                .filter(|&idx| {
                    matches!(
                        app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.entry(idx).map(|e| &e.block),
                        Some(RenderBlock::Subagent(sb))
                            if sb.child_session_id == "child-bg"
                                && !matches!(sb.kind, SubagentBlockKind::Started)
                    )
                })
                .count()
        };
        let footer_count = |app: &crate::app::app_view::AppView| {
            count_turn_markers(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get("child-bg").unwrap_or_else(|| panic!("missing map entry")))
        };
        assert_eq!(terminal_rows(&app), 1, "first finish appends one terminal row");
        assert_eq!(footer_count(&app), 1, "first finish appends one footer");

        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut("child-bg")
            .unwrap()
            .attempt.pending_kill = true;
        assert!(finalize_killed_subagent(
            &mut app,
            &acp::SessionId::new("sess-1".to_owned()),
            "child-bg",
            Some("at1.one"),
            "completed",
        ));

        assert_eq!(terminal_rows(&app), 1, "re-finalize must not add a second row");
        assert_eq!(footer_count(&app), 1, "re-finalize must not add a second footer");
        let info = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get("child-bg").unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.is_finished() && info.attempt.is_background);
        assert_eq!(info.attempt.status.as_deref(), Some("completed"));
    }

    #[test]
    fn multi_turn_child_keeps_second_footer_on_re_finalize() {
        use crate::app::agent_view::test_fixtures::count_turn_markers;
        use crate::app::subagent::finalize_finished_child_view;

        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .tracker
            .task_tool_background
            .insert("child-multi".into(), true);
        assert!(handle(
            make_ext_session_notification(
                "sess-1",
                test_subagent_spawned("sess-1", "child-multi"),
            ),
            &mut app,
        ));
        // Seed an intermediate turn marker, then later content, then finalize.
        {
            let child = app
                .agents
                .get_mut(&AgentId(0))
                .unwrap()
                .subagent_views
                .get_mut("child-multi")
                .unwrap();
            child
                .scrollback
                .push_block(RenderBlock::session_event(SessionEvent::TurnCompleted {
                    elapsed: Some(std::time::Duration::from_secs(1)),
                }));
            child
                .scrollback
                .push_block(RenderBlock::system("turn-2 content"));
            assert_eq!(
                count_turn_markers(child),
                1,
                "precondition: one earlier-turn footer exists"
            );
            finalize_finished_child_view(child, std::time::Duration::from_secs(2));
            assert_eq!(
                count_turn_markers(child),
                2,
                "second-turn finalize must append its own trailing footer"
            );
            // Re-finalize with no new content must stay idempotent on the tail.
            finalize_finished_child_view(child, std::time::Duration::from_secs(3));
            assert_eq!(
                count_turn_markers(child),
                2,
                "re-finalize must not append a third footer"
            );
        }
    }

    #[test]
    fn spawning_many_subagents_starts_no_history_search_threads() {
        const SUBAGENTS: usize = 50;

        let mut app = make_app_with_agent("sess-parent");
        for i in 0..SUBAGENTS {
            let child_sid = format!("child-storm-{i}");
            handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_spawned("sess-parent", &child_sid),
                ),
                &mut app,
            );
            handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_finished(&child_sid),
                ),
                &mut app,
            );
        }

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.subagent_views.len(),
            SUBAGENTS,
            "every spawn must have created a child view (the leak's unit)"
        );
        let daemons = agent
            .subagent_views
            .values()
            .filter(|v| v.prompt.history_search.daemon_built())
            .count();
        assert_eq!(
            daemons, 0,
            "subagent child views must never spawn history-search matcher threads"
        );
    }

    #[test]
    fn a_spawn_then_finish_updates_the_parent_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-sess-replay";

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-parent",
                "x.ai/session/update",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );
        assert!(
            affected,
            "SubagentSpawned on the active agent must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent
            .subagent_sessions
            .get(child_sid)
            .expect("SubagentSpawned must register subagent_sessions");
        assert_eq!(info.description.as_ref(), "scan src/");
        assert_eq!(info.subagent_type.as_ref(), "explore");
        assert!(
            agent.subagent_views.contains_key(child_sid),
            "SubagentSpawned must create subagent_views eagerly"
        );
        let entry_id = info
            .attempt.scrollback_entry_id
            .expect("spawn must stash scrollback_entry_id on SubagentInfo");
        assert_eq!(agent.scrollback.len(), 1);
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("SubagentSpawned must push a SubagentBlock to parent scrollback");
        };
        assert_eq!(sb.child_session_id, child_sid);
        assert!(matches!(sb.kind, SubagentBlockKind::Started));
        assert!(agent.scrollback.needs_animation());

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-parent",
                "x.ai/session/update",
                test_subagent_finished(child_sid),
            ),
            &mut app,
        );
        assert!(
            affected,
            "SubagentFinished on the active agent must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert!(info.is_finished());
        assert_eq!(info.attempt.status.as_deref(), Some("completed"));
        assert_eq!(info.attempt.tool_calls, Some(2));
        assert_eq!(info.attempt.turns, Some(1));
        assert_eq!(info.attempt.duration_ms, Some(500));
        assert_eq!(info.attempt.scrollback_entry_id, Some(entry_id));

        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("finished subagent must keep the started scrollback entry");
        };
        match &sb.kind {
            SubagentBlockKind::Completed { elapsed } => {
                assert_eq!(*elapsed, std::time::Duration::from_millis(500));
            }
            other => {
                panic!("blocking subagent must mutate started block to Completed, got {other:?}")
            }
        }
        assert!(!entry.is_running, "finish_running must clear running flag");
        assert!(
            !agent.scrollback.needs_animation(),
            "finished subagent entry must not keep scrollback animation"
        );
    }

    #[test]
    fn late_unique_subagent_lifecycle_event_is_not_dropped() {
        let mut app = make_app_with_agent("sess-parent");
        let notification = |update: serde_json::Value, event_seq: u64| {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            let payload = serde_json::json!({
                "sessionId": "sess-parent",
                "update": update,
                "_meta": { "eventId": format!("sess-parent-{event_seq}") },
            });
            let raw = serde_json::value::to_raw_value(&payload).unwrap();
            AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
                request: acp::ExtNotification::new("x.ai/session_notification", raw.into()),
                response_tx: tx,
            })
        };
        let spawned = |child_sid: &str, event_seq: u64| {
            notification(
                serde_json::to_value(test_subagent_spawned("sess-parent", child_sid)).unwrap(),
                event_seq,
            )
        };
        let finished = |child_sid: &str, event_seq: u64| {
            notification(
                serde_json::to_value(test_subagent_finished(child_sid)).unwrap(),
                event_seq,
            )
        };

        for event_seq in 1..=7 {
            assert!(handle(
                spawned(&format!("child-{event_seq}"), event_seq),
                &mut app,
            ));
        }

        // A persisted active goal update can arrive ahead of a lower-ID spawn; it advances the Remote-Code highwater without adding a scrollback block
        assert!(handle(
            notification(
                serde_json::json!({
                    "sessionUpdate": "goal_updated",
                    "goal_id": "goal-1",
                    "objective": "track lifecycle events",
                    "status": "active",
                    "phase": "executing",
                    "tokens_used": 0,
                    "elapsed_ms": 0,
                    "total_deliverables": 0,
                    "completed_deliverables": 0,
                    "total_worker_rounds": 0,
                    "total_verify_rounds": 0,
                    "token_baseline": 0,
                    "finished_subagent_tokens": 0,
                }),
                100,
            ),
            &mut app,
        ));
        assert_eq!(
            (
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).last_applied_xai_event_seq,
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            ),
            (Some(100), 7)
        );

        let _ = handle(finished("child-1", 50), &mut app);
        assert!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get("child-1").unwrap_or_else(|| panic!("missing map entry")).is_finished());
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).last_applied_xai_event_seq,
            Some(100),
            "a late lower-ID lifecycle event must not regress the scalar xAI highwater"
        );

        // A restarted producer can reuse an eventId for a different child.
        let _ = handle(spawned("child-reused-id", 1), &mut app);
        assert!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"))
                .subagent_sessions
                .contains_key("child-reused-id"),
            "raw eventId reuse must not suppress a new child lifecycle"
        );

        let _ = handle(spawned("child-8", 8), &mut app);
        assert_eq!(
            (
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.len(),
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            ),
            (9, 9),
            "a unique late subagent lifecycle event must not be treated as a duplicate"
        );
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).last_seen_event_id.as_deref(),
            Some("sess-parent-100"),
            "applied late lifecycle must not move the reconnect cursor backwards"
        );

        let _ = handle(finished("child-2", 9), &mut app);
        let _ = handle(spawned("child-8", 8), &mut app);
        let _ = handle(finished("child-2", 9), &mut app);
        assert_eq!(
            (
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.len(),
                app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            ),
            (9, 9),
            "exact spawn and finish redeliveries must remain idempotent"
        );
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).last_seen_event_id.as_deref(),
            Some("sess-parent-100"),
            "dropped duplicates and late lower-ID applies must not move the reconnect cursor"
        );
    }

    #[test]
    fn a_duplicate_live_spawn_keeps_the_existing_child_view() {
        let mut app = make_app_with_agent("sess-parent");
        let spawn = make_ext_session_notification(
            "sess-parent",
            test_subagent_spawned("sess-parent", "child-live-duplicate"),
        );
        assert!(handle(spawn, &mut app));
        let entry_id = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get("child-live-duplicate").unwrap_or_else(|| panic!("missing map entry"))
            .attempt.scrollback_entry_id
            .unwrap();
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .scrollback
            .remove_entry(entry_id);
        let first_view = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get("child-live-duplicate").unwrap_or_else(|| panic!("missing map entry"))
            .as_ref() as *const AgentView;

        let duplicate = make_ext_session_notification(
            "sess-parent",
            test_subagent_spawned("sess-parent", "child-live-duplicate"),
        );
        assert!(!handle(duplicate, &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(agent.scrollback.is_empty());
        assert_eq!(
            agent.subagent_views.get("child-live-duplicate").unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView,
            first_view,
            "live duplicate spawn must not replace the child view"
        );
        assert_eq!(
            agent.subagent_sessions.get("child-live-duplicate").unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id,
            Some(entry_id),
            "live duplicate spawn must preserve retained domain state"
        );
    }

    #[test]
    fn a_duplicate_workflow_replay_spawn_keeps_the_existing_child_view() {
        let mut app = make_app_with_agent("sess-parent");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;
        let spawn = || {
            subagent_ext_replay(
                "sess-parent",
                serde_json::to_value(test_subagent_spawned_for_workflow(
                    "sess-parent",
                    "workflow-child",
                    Some("workflow-run".to_string()),
                ))
                .unwrap(),
                "sess-parent-1",
            )
        };

        assert!(handle_ext_notification(&spawn(), &mut app));
        let first_view = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get("workflow-child").unwrap_or_else(|| panic!("missing map entry"))
            .as_ref() as *const AgentView;
        assert!(!handle_ext_notification(&spawn(), &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(agent.scrollback.is_empty());
        assert_eq!(
            agent.subagent_views.get("workflow-child").unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView,
            first_view,
            "duplicate replay must not replace the workflow child's AgentView"
        );
        assert_eq!(
            agent.last_seen_event_id.as_deref(),
            Some("sess-parent-1"),
            "the duplicate replay must not consume the cursor again"
        );
    }

    #[test]
    fn a_subagents_activity_label_updates_live_and_clears_on_finish() {
        let mut app = make_app_with_agent("sess-parent");
        let child_sid = "child-activity";
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_spawned("sess-parent", child_sid),
            ),
            &mut app,
        );

        // A live child message chunk resolves "Responding" and stamps both the block and the info
        let _ = handle(
            make_agent_chunk_with_event(child_sid, "child text", "p-child", None),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert_eq!(info.attempt.activity_label.as_deref(), Some("Responding"));
        let entry_id = info.attempt.scrollback_entry_id.unwrap();
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected Subagent block");
        };
        assert_eq!(sb.activity_label, info.attempt.activity_label);

        // SubagentProgress recomputes from the child tracker and restamps.
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .subagent_sessions
            .get_mut(child_sid)
            .unwrap()
            .attempt.activity_label = None;
        let _ = handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_progress("sess-parent", child_sid),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .subagent_sessions
                .get(child_sid)
                .unwrap()
                .attempt.activity_label
                .as_deref(),
            Some("Responding")
        );

        let _ = handle(
            make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let info = agent.subagent_sessions.get(child_sid).unwrap();
        assert!(
            info.attempt.activity_label.is_none(),
            "finish must clear the info label"
        );
        let entry = agent.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("expected Subagent block");
        };
        assert!(
            sb.activity_label.is_none(),
            "finish must clear the block label"
        );
    }

    #[test]
    fn a_write_tool_call_labels_a_live_child_only() {
        enum Case {
            Live,
            ReloadingTranscript,
            Unregistered,
        }
        for (case, child_sid) in [
            (Case::Live, "child-writing-live"),
            (Case::ReloadingTranscript, "child-writing-reloading"),
            (Case::Unregistered, "child-writing-unregistered"),
        ] {
            let mut app = make_app_with_agent("sess-parent");
            let _ = handle(
                make_ext_session_notification(
                    "sess-parent",
                    test_subagent_spawned("sess-parent", child_sid),
                ),
                &mut app,
            );
            match case {
                Case::Live => {}
                Case::ReloadingTranscript => {
                    app.agents
                        .get_mut(&AgentId(0))
                        .unwrap()
                        .subagent_views
                        .get_mut(child_sid)
                        .unwrap()
                        .session
                        .loading_replay = true;
                }
                Case::Unregistered => {
                    app.agents
                        .get_mut(&AgentId(0))
                        .unwrap()
                        .subagent_sessions
                        .remove(child_sid);
                }
            }

            let changed = handle(
                make_ext_session_notification(
                    child_sid,
                    XaiSessionUpdate::ToolCallDeltaChunk {
                        tool_call_id: Some("call_1".into()),
                        tool_index: 0,
                        name: Some("write".into()),
                        arguments_delta: None,
                    },
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            match case {
                Case::Live => {
                    assert!(changed, "a live child's write delta must redraw");
                    assert_eq!(
                        agent.subagent_sessions.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).attempt.activity_label.as_deref(),
                        Some("Writing file…")
                    );
                }
                Case::ReloadingTranscript => {
                    assert!(!changed, "a delta must be ignored while the child reloads its transcript");
                    assert!(agent.subagent_sessions.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).attempt.activity_label.is_none());
                    assert_eq!(
                        agent.subagent_views.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).session.tracker.activity(),
                        None,
                        "the reloading tracker must not pick up the delta"
                    );
                }
                Case::Unregistered => {
                    assert!(!changed, "a delta for an unregistered child must not redraw");
                }
            }
        }
    }

    #[test]
    fn a_finished_subagents_label_is_not_restamped_by_late_events() {
        enum LateEvent {
            AcpChunk,
            ToolCallDelta,
        }
        for (event, child_sid) in [
            (LateEvent::AcpChunk, "child-late-acp"),
            (LateEvent::ToolCallDelta, "child-late-delta"),
        ] {
            let mut app = make_app_with_agent("sess-parent");
            let _ = handle(
                make_ext_session_notification(
                    "sess-parent",
                    test_subagent_spawned("sess-parent", child_sid),
                ),
                &mut app,
            );
            let _ = handle(
                make_ext_session_notification("sess-parent", test_subagent_finished(child_sid)),
                &mut app,
            );

            let changed = match event {
                LateEvent::AcpChunk => {
                    // Simulate the race: the child view still looks live after the finish
                    app.agents
                        .get_mut(&AgentId(0))
                        .unwrap()
                        .subagent_views
                        .get_mut(child_sid)
                        .unwrap()
                        .session
                        .state = AgentState::TurnRunning;
                    handle(
                        make_agent_chunk_with_event(child_sid, "late text", "p-child", None),
                        &mut app,
                    )
                }
                LateEvent::ToolCallDelta => handle(
                    make_ext_session_notification(
                        child_sid,
                        XaiSessionUpdate::ToolCallDeltaChunk {
                            tool_call_id: Some("call_1".into()),
                            tool_index: 0,
                            name: Some("write".into()),
                            arguments_delta: None,
                        },
                    ),
                    &mut app,
                ),
            };
            if matches!(event, LateEvent::ToolCallDelta) {
                assert!(!changed, "a delta after finish must not redraw");
            }

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert!(
                agent.subagent_sessions.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).attempt.activity_label.is_none(),
                "a finished subagent's label must stay cleared after a late event"
            );
        }
    }

    #[test]
    fn a_replay_tagged_spawn_outside_a_session_load_is_dropped() {
        with_replay_disk_home(|_| {
            let child_sid = "child-unexpected-replay";
            let mut app = make_app_with_agent("sess-parent");
            assert!(!app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).session.loading_replay);
            write_child_updates_jsonl(
                replay_disk_test_home(),
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );
            let spawned = subagent_ext_replay(
                "sess-parent",
                serde_json::json!({
                    "sessionUpdate": "subagent_spawned",
                    "subagent_id": child_sid,
                    "parent_session_id": "sess-parent",
                    "child_session_id": child_sid,
                    "subagent_type": "explore",
                    "description": "scan src/",
                }),
                "sess-parent-1",
            );
            handle_ext_notification(&spawned, &mut app);
            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert!(
                agent.subagent_sessions.is_empty(),
                "unexpected replay spawn must not register"
            );
            assert!(agent.subagent_views.is_empty());
        });
    }

    #[test]
    fn a_stray_replay_is_accepted_briefly_after_a_load_then_a_live_update_stops_it() {
        let replay = || crate::acp::meta::NotificationMeta {
            is_replay: true,
            ..crate::acp::meta::NotificationMeta::default()
        };
        let mut app = make_app_with_agent("sess-late");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(!agent.session.loading_replay);
        agent.arm_late_replay_grace();

        assert!(
            !drop_unexpected_replay(agent, &replay(), "sess-late", "test"),
            "a stray replay must still apply during the post-load grace"
        );

        let live = crate::acp::meta::NotificationMeta::default();
        assert!(!drop_unexpected_replay(agent, &live, "sess-late", "test"));
        assert!(
            drop_unexpected_replay(agent, &replay(), "sess-late", "test"),
            "a live update ends the grace so later replays are dropped"
        );
    }

    #[test]
    fn subagent_spawned_during_resume_defers_child_replay_until_open() {
        with_replay_disk_home(|_| {
            let child_sid = "child-resume-defer";
            let mut app = make_app_with_agent("sess-parent");
            // Simulate resume: the parent agent is replaying its own session.
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "resume spawn must NOT eagerly replay the child transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "resume spawn must leave the transcript NeedsReplay for the first open"
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening the subagent after resume must replay its transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| !i.transcript.needs_replay()),
                "the first open must record the disk copy"
            );
        });
    }

    #[test]
    fn live_spawn_burst_defers_child_replay_until_open() {
        // The burst size that froze the TUI in the field.
        const BURST_CHILDREN: usize = 25;

        with_replay_disk_home(|_| {
            let mut app = make_app_with_agent("sess-parent");
            let child_sids: Vec<String> = (0..BURST_CHILDREN)
                .map(|i| format!("child-burst-{i}"))
                .collect();
            let reads_before = crate::app::subagent::test_support::transcript_reads();
            for sid in &child_sids {
                spawn_subagent_with_optional_updates(
                    &mut app,
                    sid,
                    Some(&(child_tool_line(sid) + "\n")),
                );
            }
            assert_eq!(
                crate::app::subagent::test_support::transcript_reads(),
                reads_before,
                "a spawn burst must not open a single child transcript"
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            for sid in &child_sids {
                assert_eq!(
                    child_scrollback_tool_call_count(agent, sid),
                    0,
                    "a live spawn must not replay the on-disk transcript"
                );
                assert!(
                    agent
                        .subagent_sessions
                        .get(sid.as_str())
                        .is_some_and(|i| i.transcript.needs_replay()),
                    "a live spawn must leave the transcript NeedsReplay for the first open"
                );
            }

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sids.first().unwrap_or_else(|| panic!("missing index")).clone());
            assert_eq!(
                crate::app::subagent::test_support::transcript_reads(),
                reads_before + 1,
                "the first open must read exactly the opened child's transcript"
            );
            assert_eq!(child_scrollback_tool_call_count(agent, child_sids.first().unwrap_or_else(|| panic!("missing index"))), 1);
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sids.get(1).unwrap_or_else(|| panic!("missing index"))),
                0,
                "opening one child must not read its siblings"
            );
        });
    }

    #[test]
    fn resumed_child_keeps_inherited_history_when_a_live_block_arrives_first() {
        with_replay_disk_home(|home| {
            let child_sid = "child-resume-live-first";
            write_child_updates_jsonl(home, child_sid, &(child_tool_line(child_sid) + "\n"));

            let mut app = make_app_with_agent("sess-parent");
            let mut spawned = test_subagent_spawned("sess-parent", child_sid);
            let XaiSessionUpdate::SubagentSpawned { resumed_from, .. } = &mut spawned else {
                unreachable!();
            };
            *resumed_from = Some("orig-child".into());
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    spawned,
                ),
                &mut app,
            );
            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "a resumed spawn must defer the read like any other"
            );

            // A background child streams its first live block before it is opened.
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .subagent_sessions
                .get_mut(child_sid)
                .unwrap()
                .attempt.is_background = true;
            let _ = handle(make_agent_chunk_message(child_sid, "working on it"), &mut app);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "the inherited tool call must be read before the live block lands"
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening the child must show the inherited history exactly once"
            );
        });
    }

    #[test]
    fn subagent_resume_finished_then_open_shows_full_transcript() {
        with_replay_disk_home(|_| {
            let child_sid = "child-resume-finished";
            let mut app = make_app_with_agent("sess-parent");
            app.agents
                .get_mut(&AgentId(0))
                .unwrap()
                .session
                .loading_replay = true;

            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_finished(child_sid),
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "resume must not eagerly load the finished subagent transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "finished-during-resume must leave the transcript NeedsReplay"
            );
            assert!(
                matches!(
                    agent.subagent_views.get(child_sid).unwrap().session.state,
                    AgentState::Idle
                ),
                "finished subagent must be Idle after resume, not TurnRunning"
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening a finished subagent after resume must show its transcript"
            );
            let child = agent.subagent_views.get(child_sid).unwrap();
            assert!(
                (0..child.scrollback.len()).any(|i| child
                    .scrollback
                    .entry(i)
                    .is_some_and(|e| matches!(e.block, RenderBlock::SessionEvent(_)))),
                "opened finished subagent must show a TurnCompleted footer"
            );
        });
    }

    #[test]
    fn an_open_resumed_child_that_finishes_without_streaming_hydrates_in_place() {
        with_replay_disk_home(|home| {
            let child_sid = "child-open-resume-finish";

            // A resumed child spawns live; its inherited transcript has not flushed to disk yet
            let mut app = make_app_with_agent("sess-parent");
            let mut spawned = test_subagent_spawned("sess-parent", child_sid);
            let XaiSessionUpdate::SubagentSpawned { resumed_from, .. } = &mut spawned else {
                unreachable!();
            };
            *resumed_from = Some("orig-child".into());
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    spawned,
                ),
                &mut app,
            );

            // Open it fullscreen before any transcript exists: the read finds nothing, so the view stays empty
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 0);

            // The inherited transcript flushes, then the child finishes while still open, having streamed no live block
            write_child_updates_jsonl(home, child_sid, &(child_tool_line(child_sid) + "\n"));
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    test_subagent_finished(child_sid),
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "finishing must hydrate the open resumed child in place, not just stamp a footer"
            );
        });
    }

    /// Each scenario owns one parent plus a child driven through the real notification handler.
    /// It also owns the child's on-disk transcript under the shared replay test home.
    mod eviction_and_rebuild {
        use super::*;
        use crate::app::subagent::ChildTranscript;

        struct Scenario {
            app: AppView,
            child_sid: &'static str,
        }

        impl Drop for Scenario {
            fn drop(&mut self) {
                crate::app::subagent::set_replay_grok_home_for_tests(None);
            }
        }

        impl Scenario {
            /// Live-spawn `child_sid`, first writing `updates` to the child's `updates.jsonl` (`None` persists nothing).
            fn spawn(child_sid: &'static str, updates: Option<String>) -> Self {
                let home = replay_disk_test_home();
                crate::app::subagent::set_replay_grok_home_for_tests(Some(home.to_path_buf()));
                let mut app = make_app_with_agent("sess-parent");
                spawn_subagent_with_optional_updates(&mut app, child_sid, updates.as_deref());
                Scenario { app, child_sid }
            }

            fn spawn_sibling(&mut self, child_sid: &str, updates: Option<String>) {
                spawn_subagent_with_optional_updates(&mut self.app, child_sid, updates.as_deref());
            }

            fn agent(&self) -> &AgentView {
                self.app.agents.get(&AgentId(0)).unwrap()
            }

            fn agent_mut(&mut self) -> &mut AgentView {
                self.app.agents.get_mut(&AgentId(0)).unwrap()
            }

            fn transcript(&self) -> ChildTranscript {
                self.agent().subagent_sessions.get(self.child_sid).unwrap_or_else(|| panic!("missing map entry")).transcript
            }

            fn set_transcript(&mut self, state: ChildTranscript) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_sessions
                    .get_mut(sid)
                    .unwrap()
                    .transcript = state;
            }

            fn set_background(&mut self) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_sessions
                    .get_mut(sid)
                    .unwrap()
                    .attempt.is_background = true;
            }

            /// Deliver the child's `SubagentFinished` through the real handler.
            fn finish(&mut self) {
                let _ = handle(
                    make_ext_session_notification_with_method(
                        "sess-parent",
                        "x.ai/session/update",
                        test_subagent_finished(self.child_sid),
                    ),
                    &mut self.app,
                );
            }

            fn open(&mut self) {
                self.open_child(self.child_sid);
            }

            fn open_child(&mut self, child_sid: &str) {
                let sid = child_sid.to_string();
                self.agent_mut().open_subagent_fullscreen(sid);
            }

            fn close(&mut self) {
                self.agent_mut().close_subagent_fullscreen();
            }

            /// Deliver a live ACP update on the child's own session id (`SessionMatch::Child`).
            fn live_child_update(&mut self, update: acp::SessionUpdate) {
                let (tx, _rx) = tokio::sync::oneshot::channel();
                let request =
                    acp::SessionNotification::new(acp::SessionId::new(self.child_sid), update)
                        .meta(
                            serde_json::json!({ "agentTimestampMs": 1 })
                                .as_object()
                                .cloned(),
                        );
                handle(
                    AcpClientMessage::SessionNotification(xai_acp_lib::AcpArgs {
                        request,
                        response_tx: tx,
                    }),
                    &mut self.app,
                );
            }

            /// The shell's user-echo of a child prompt: the one writer of the prompt line.
            fn live_user_echo(&mut self, text: &str) {
                self.live_child_update(acp::SessionUpdate::UserMessageChunk(
                    acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text))),
                ));
            }

            fn live_tool_call(&mut self) {
                self.live_child_update(make_tool_call("Read foo"));
            }

            fn push_child_block(&mut self, block: RenderBlock) {
                let sid = self.child_sid;
                self.agent_mut()
                    .subagent_views
                    .get_mut(sid)
                    .unwrap()
                    .scrollback
                    .push_block(block);
            }

            fn tool_calls(&self) -> usize {
                self.tool_calls_for(self.child_sid)
            }

            fn tool_calls_for(&self, child_sid: &str) -> usize {
                child_scrollback_tool_call_count(self.agent(), child_sid)
            }

            fn session_events(&self) -> usize {
                child_scrollback_session_event_count(self.agent(), self.child_sid)
            }

            fn prompts_matching(&self, prompt: &str) -> usize {
                child_scrollback_matching_prompt_count(self.agent(), self.child_sid, prompt)
            }

            fn has_system_block(&self) -> bool {
                let child = self.agent().subagent_views.get(self.child_sid).unwrap();
                (0..child.scrollback.len()).any(|i| {
                    matches!(
                        child.scrollback.entry(i).map(|e| &e.block),
                        Some(RenderBlock::System(_))
                    )
                })
            }

            fn compaction_markers(&self) -> usize {
                let child = self.agent().subagent_views.get(self.child_sid).unwrap();
                (0..child.scrollback.len())
                    .filter(|i| {
                        matches!(
                            child.scrollback.entry(*i).map(|e| &e.block),
                            Some(RenderBlock::SessionEvent(b)) if matches!(
                                b.event,
                                SessionEvent::CompactionStarted { .. }
                                    | SessionEvent::CompactionCompleted { .. }
                            )
                        )
                    })
                    .count()
            }

            fn updates_path(&self) -> std::path::PathBuf {
                replay_disk_test_home()
                    .join("sessions")
                    .join(urlencoding::encode("/tmp").as_ref())
                    .join(self.child_sid)
                    .join("updates.jsonl")
            }

            /// Replace `updates.jsonl` with a directory so a rebuild read fails (`Err`, not the missing-file `Empty`).
            fn break_transcript(&self) {
                let path = self.updates_path();
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }

            fn replace_transcript(&self, content: &str) {
                let path = self.updates_path();
                if path.is_dir() {
                    std::fs::remove_dir(&path).unwrap();
                }
                std::fs::write(&path, content).unwrap();
            }

            /// Remove the child session dir so a rebuild resolves `Empty`.
            fn remove_session_dir(&self) {
                std::fs::remove_dir_all(self.updates_path().parent().unwrap()).unwrap();
            }
        }

        fn child_compaction_started_line(child_sid: &str) -> String {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"auto_compact_started","tokens_used":9000,"context_window":10000,"percentage":90,"reason":"threshold"}}}}}}"#
            )
        }

        fn child_compaction_completed_line(child_sid: &str) -> String {
            format!(
                r#"{{"method":"_x.ai/session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"auto_compact_completed","tokens_after":100,"elapsed_ms":5}}}}}}"#
            )
        }

        #[test]
        fn a_finished_foreground_child_is_evicted_then_rebuilds_on_open() {
            enum Trigger {
                FinishWhileClosed,
                CloseWhileOpen,
                SwitchToSibling,
            }
            for (trigger, child_sid) in [
                (Trigger::FinishWhileClosed, "child-evict-finish"),
                (Trigger::CloseWhileOpen, "child-evict-close"),
                (Trigger::SwitchToSibling, "child-evict-switch"),
            ] {
                let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
                match trigger {
                    Trigger::FinishWhileClosed => {
                        s.open();
                        s.close();
                        s.finish();
                        let child = s.agent().subagent_views.get(child_sid).unwrap();
                        assert_eq!(child.scrollback.len(), 0, "finish must evict the transcript");
                        assert!(matches!(child.session.state, AgentState::Idle));
                    }
                    Trigger::CloseWhileOpen => {
                        s.open();
                        s.finish();
                        assert_eq!(s.tool_calls(), 1, "an open child is guarded at finish");
                        s.close();
                    }
                    Trigger::SwitchToSibling => {
                        let sibling = "child-evict-sibling";
                        s.spawn_sibling(sibling, Some(child_tool_line(sibling) + "\n"));
                        s.open();
                        s.finish();
                        s.open_child(sibling);
                    }
                }
                assert_eq!(s.tool_calls(), 0, "the finished child must be evicted");
                assert_eq!(
                    s.transcript(),
                    ChildTranscript::NeedsReplay,
                    "eviction must schedule the deferred on-open replay again"
                );

                s.open();
                assert_eq!(s.tool_calls(), 1, "opening the evicted child must rebuild it");
                assert_eq!(
                    s.session_events(),
                    1,
                    "the rebuilt transcript ends with the TurnCompleted footer"
                );
            }
        }

        #[test]
        fn an_evict_guard_keeps_the_transcript_in_place() {
            enum Guard {
                OpenAtFinish,
                RunningOnClose,
            }
            for (guard, child_sid, footer) in [
                (Guard::OpenAtFinish, "child-guard-finish", 1),
                (Guard::RunningOnClose, "child-guard-close", 0),
            ] {
                let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
                s.open();
                match guard {
                    Guard::OpenAtFinish => s.finish(),
                    Guard::RunningOnClose => s.close(),
                }
                assert_eq!(s.tool_calls(), 1, "the guarded transcript must survive");
                assert_eq!(s.session_events(), footer);
            }
        }

        #[test]
        fn the_persisted_echo_paints_the_prompt_once_on_open() {
            // Same single paint whether the view is freshly spawned or was first evicted back to empty
            enum Entry {
                FreshSpawn,
                Evicted,
            }
            for (entry, child_sid) in [
                (Entry::FreshSpawn, "child-echo-fresh"),
                (Entry::Evicted, "child-echo-evicted"),
            ] {
                let task = "scan src/ for auth";
                write_subagent_meta_json(replay_disk_test_home(), "sess-parent", child_sid, task);
                let updates = format!(
                    "{}\n{}",
                    child_user_message_line(child_sid, task),
                    child_tool_line(child_sid)
                );
                let mut s = Scenario::spawn(child_sid, Some(updates));

                assert!(
                    s.agent().subagent_views.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).scrollback.is_empty(),
                    "spawn seeds nothing and reads nothing"
                );
                assert!(
                    !child_tracker_expects_user_echo(s.agent(), child_sid),
                    "spawn arms no echo skip"
                );

                if matches!(entry, Entry::Evicted) {
                    s.finish();
                    assert!(
                        s.agent().subagent_views.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).scrollback.is_empty(),
                        "eviction resets to the empty baseline"
                    );
                    assert!(
                        !child_tracker_expects_user_echo(s.agent(), child_sid),
                        "eviction arms no echo skip"
                    );
                }

                s.open();
                assert_eq!(s.prompts_matching(task), 1, "the replayed echo paints the prompt once");
                assert_eq!(s.tool_calls(), 1);
            }
        }

        #[test]
        fn a_live_echo_then_open_of_a_running_child_keeps_one_prompt() {
            let child_sid = "child-echo-live-then-open";
            let task = "scan src/ for auth";
            write_subagent_meta_json(replay_disk_test_home(), "sess-parent", child_sid, task);
            // Disk mirrors what the live stream delivers to a fresh child
            let mut s = Scenario::spawn(child_sid, Some(child_user_message_line(child_sid, task)));

            s.live_user_echo(task);
            assert_eq!(s.prompts_matching(task), 1, "the live echo paints the prompt");

            let reads_before = crate::app::subagent::test_support::transcript_reads();
            s.open();
            assert_eq!(
                s.prompts_matching(task),
                1,
                "opening a running prompt-only child must not paint a second copy"
            );
            assert_eq!(
                crate::app::subagent::test_support::transcript_reads(),
                reads_before,
                "the live echo closed the replay window: disk is not read"
            );
            assert_eq!(s.transcript(), ChildTranscript::NeedsReplay);

            s.live_tool_call();
            assert_eq!(s.tool_calls(), 1, "live blocks after the echo still land");
            assert_eq!(s.prompts_matching(task), 1);
        }

        #[test]
        fn a_later_different_user_message_still_appears() {
            let child_sid = "child-echo-second-prompt";
            let task = "scan src/ for auth";
            let follow_up = "now check tests/ too";
            write_subagent_meta_json(replay_disk_test_home(), "sess-parent", child_sid, task);
            let mut s = Scenario::spawn(child_sid, None);

            s.live_user_echo(task);
            s.open();
            s.live_tool_call();
            s.live_user_echo(follow_up);

            assert_eq!(s.prompts_matching(task), 1, "the task prompt stays single");
            assert_eq!(
                s.prompts_matching(follow_up),
                1,
                "a later, different user message is painted, not swallowed by a skip"
            );
            assert_eq!(s.tool_calls(), 1);
        }

        #[test]
        fn a_resumed_child_keeps_its_first_historical_prompt_and_paints_the_new_task_once() {
            let child_sid = "child-echo-resumed-history";
            let old_task = "old task from the source session";
            let new_task = "continue where you left off";
            write_subagent_meta_json(replay_disk_test_home(), "sess-parent", child_sid, new_task);
            // The inherited transcript copied into the child dir at spawn: its first block is the source's own user prompt
            let inherited = format!(
                "{}\n{}\n",
                child_user_message_line(child_sid, old_task),
                child_tool_line(child_sid)
            );
            write_child_updates_jsonl(replay_disk_test_home(), child_sid, &inherited);
            crate::app::subagent::set_replay_grok_home_for_tests(Some(
                replay_disk_test_home().to_path_buf(),
            ));
            let mut app = make_app_with_agent("sess-parent");
            let mut spawned = test_subagent_spawned("sess-parent", child_sid);
            let XaiSessionUpdate::SubagentSpawned { resumed_from, .. } = &mut spawned else {
                unreachable!();
            };
            *resumed_from = Some("orig-child".into());
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    spawned,
                ),
                &mut app,
            );
            let mut s = Scenario { app, child_sid };

            assert!(
                s.agent().subagent_views.get(child_sid).unwrap_or_else(|| panic!("missing map entry")).scrollback.is_empty(),
                "a resumed spawn seeds nothing and reads nothing"
            );

            // The first live block is the shell's echo of the new task; the funnel hydrates the inherited history first
            s.live_user_echo(new_task);
            assert_eq!(
                s.prompts_matching(old_task),
                1,
                "the inherited first user prompt must not be swallowed by an echo skip"
            );
            assert_eq!(s.tool_calls(), 1, "the inherited tool call is hydrated before the echo");
            assert_eq!(s.prompts_matching(new_task), 1, "the new task is painted once, by its echo");

            s.open();
            assert_eq!(s.prompts_matching(old_task), 1, "open must not repaint the inherited prompt");
            assert_eq!(s.prompts_matching(new_task), 1, "open must not repaint the new task");
            assert_eq!(s.tool_calls(), 1);
        }

        #[test]
        fn late_block_after_evict_still_rebuilds_on_open() {
            let child_sid = "child-late-event";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();
            // Late block sneaks into the evicted (prompt-less) scrollback.
            s.push_child_block(RenderBlock::system("late notice".to_string()));

            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "a late block must not stop the on-open rebuild"
            );
        }

        #[test]
        fn a_nonemitting_rebuild_of_an_evicted_view_retries_until_disk_lands() {
            // An evicted view holds nothing to restore, so a non-emitting read stays NeedsReplay and retries once real content lands
            // A read error applies no footer; an Empty read (flush not landed yet) still stamps the finished footer on the bare view
            enum Outcome {
                ReadError,
                Empty,
            }
            for (outcome, child_sid, footer_before_flush) in [
                (Outcome::ReadError, "child-evict-io-retry", 0),
                (Outcome::Empty, "child-evict-flush-race", 1),
            ] {
                let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
                s.finish();

                match outcome {
                    Outcome::ReadError => s.break_transcript(),
                    Outcome::Empty => s.replace_transcript(""),
                }
                s.open();
                assert_eq!(s.tool_calls(), 0, "a non-emitting rebuild reads nothing");
                assert_eq!(
                    s.transcript(),
                    ChildTranscript::NeedsReplay,
                    "a non-emitting rebuild of an evicted view stays NeedsReplay"
                );
                assert_eq!(s.session_events(), footer_before_flush);

                s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
                s.open();
                assert_eq!(s.tool_calls(), 1, "the retry rebuilds once disk content lands");
                assert_eq!(s.session_events(), 1, "the rebuilt view ends with one footer");
            }
        }

        #[test]
        fn partial_flush_rebuild_self_heals_on_next_open_cycle() {
            let child_sid = "child-partial-flush";
            // Only a prefix of the final transcript is on disk at finish time.
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.finish();

            s.open();
            assert_eq!(s.tool_calls(), 1, "the first open rebuilds the prefix");
            assert_eq!(s.transcript(), ChildTranscript::DiskBacked);
            s.close();
            assert_eq!(
                s.transcript(),
                ChildTranscript::NeedsReplay,
                "closing a disk-backed view must schedule the replay again"
            );

            // The rest of the flush lands after the partial rebuild.
            let second_line = child_tool_line(child_sid)
                .replace(r#""toolCallId":"tc1""#, r#""toolCallId":"tc2""#);
            s.replace_transcript(&format!(
                "{}\n{}\n",
                child_tool_line(child_sid),
                second_line
            ));
            s.open();
            assert_eq!(
                s.tool_calls(),
                2,
                "the open after the late append must replay the completed file"
            );
        }

        #[test]
        fn finished_background_child_with_content_never_rebuilds() {
            let child_sid = "child-bg-no-rebuild";
            let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
            s.set_background();
            // The view is populated but its transcript was never read back from disk
            s.open();
            assert_eq!(s.tool_calls(), 1);
            s.close();
            s.set_transcript(ChildTranscript::NeedsReplay);

            s.finish();
            assert_eq!(s.tool_calls(), 1);

            s.open();
            assert_eq!(
                s.tool_calls(),
                1,
                "opening a populated background child must not append the disk transcript"
            );
        }

        #[test]
        fn a_nonemitting_rebuild_restores_the_populated_view() {
            // Content the replay never read back is restored when the rebuild reads nothing
            // A read error stays retriable; an Empty read pins the only copy MemoryOnly
            enum Outcome {
                ReadError,
                Empty,
            }
            for (outcome, child_sid) in [
                (Outcome::ReadError, "child-restore-err"),
                (Outcome::Empty, "child-restore-empty"),
            ] {
                let mut s = Scenario::spawn(child_sid, Some(child_tool_line(child_sid) + "\n"));
                s.open();
                s.set_transcript(ChildTranscript::NeedsReplay);
                s.finish();
                assert_eq!(s.tool_calls(), 1);
                assert_eq!(s.session_events(), 1);

                match outcome {
                    Outcome::ReadError => s.break_transcript(),
                    Outcome::Empty => s.remove_session_dir(),
                }
                s.open();
                assert_eq!(
                    s.tool_calls(),
                    1,
                    "a non-emitting rebuild restores the populated view"
                );
                assert_eq!(s.session_events(), 1, "the restored view keeps its single footer");

                match outcome {
                    Outcome::ReadError => {
                        s.replace_transcript(&(child_tool_line(child_sid) + "\n"));
                        s.open();
                        assert_eq!(s.tool_calls(), 1, "the repaired disk rebuilds cleanly");
                        assert_eq!(
                            s.transcript(),
                            ChildTranscript::DiskBacked,
                            "a read error is retriable and records the disk copy once repaired"
                        );
                    }
                    Outcome::Empty => {
                        assert_eq!(
                            s.transcript(),
                            ChildTranscript::MemoryOnly,
                            "nothing on disk pins the restored copy as the only one"
                        );
                        s.close();
                        assert_eq!(s.tool_calls(), 1, "close must not drop the memory-only copy");
                        s.open();
                        assert_eq!(s.tool_calls(), 1);
                    }
                }
            }
        }

        #[test]
        fn finish_keeps_the_only_copy_when_disk_cannot_rebuild_it() {
            // Two ways disk cannot rebuild: nothing persisted, or a non-emitting file.
            for (child_sid, updates) in [
                ("child-no-disk-copy", None),
                (
                    "child-non-emitting-disk",
                    Some(child_compaction_started_line("child-non-emitting-disk") + "\n"),
                ),
            ] {
                let mut s = Scenario::spawn(child_sid, updates);
                // This block models live-streamed content that exists only in memory
                s.push_child_block(RenderBlock::system("streamed output".to_string()));
                assert_eq!(s.transcript(), ChildTranscript::NeedsReplay);

                s.finish();
                assert_eq!(
                    s.transcript(),
                    ChildTranscript::NeedsReplay,
                    "a probe that proves nothing must keep retrying"
                );
                assert!(s.has_system_block(), "the only copy must survive the finish");

                s.open();
                assert_eq!(
                    s.transcript(),
                    ChildTranscript::MemoryOnly,
                    "the open's Empty rebuild records the restored copy as memory-only"
                );
                assert!(s.has_system_block());
                s.close();
                assert!(
                    s.has_system_block(),
                    "close must never drop the memory-only copy"
                );
            }
        }

        #[test]
        fn rebuild_after_evict_preserves_child_compaction_markers() {
            let child_sid = "child-xai-marker";
            let updates = format!(
                "{}\n{}\n{}\n",
                child_tool_line(child_sid),
                child_compaction_started_line(child_sid),
                child_compaction_completed_line(child_sid),
            );
            let mut s = Scenario::spawn(child_sid, Some(updates));
            // Read the deferred transcript, then leave the child running.
            s.open();
            assert!(
                s.compaction_markers() >= 1,
                "the on-open replay must render the compaction marker"
            );
            assert_eq!(s.tool_calls(), 1);
            s.close();

            s.finish();
            assert_eq!(s.tool_calls(), 0, "finish must evict the transcript");

            s.open();
            assert_eq!(s.tool_calls(), 1);
            assert_eq!(
                s.compaction_markers(),
                2,
                "the rebuilt transcript must keep both compaction markers"
            );
        }
    }

    #[test]
    fn subagent_spawn_seeds_no_prompt_and_reads_no_disk() {
        // meta.json still enriches SubagentInfo.prompt (tasks pane, status rows), but the child view gets no copy of it
        let cases: &[Option<&str>] = &[Some("explore handlers only"), Some("   "), None];
        for (idx, meta) in cases.iter().enumerate() {
            let child_sid = format!("child-inject-{idx}");
            with_replay_disk_home(|home| {
                let parent_sid = "sess-parent";
                if let Some(meta) = meta {
                    write_subagent_meta_json(home, parent_sid, &child_sid, meta);
                }
                let mut app = make_app_with_agent(parent_sid);
                let reads_before = crate::app::subagent::test_support::transcript_reads();
                spawn_subagent_with_optional_updates(&mut app, &child_sid, None);

                let agent = app.agents.get(&AgentId(0)).unwrap();
                assert_eq!(
                    agent.subagent_sessions.get(child_sid.as_str()).unwrap_or_else(|| panic!("missing map entry")).prompt.as_deref(),
                    *meta,
                    "meta.json enrichment for {meta:?}"
                );
                assert!(
                    agent.subagent_views.get(&child_sid).unwrap().scrollback.is_empty(),
                    "spawn leaves the child view empty for {meta:?}"
                );
                assert!(
                    !child_tracker_expects_user_echo(agent, &child_sid),
                    "spawn arms no echo skip for {meta:?}"
                );
                assert_eq!(
                    crate::app::subagent::test_support::transcript_reads(),
                    reads_before,
                    "nothing read on spawn for {meta:?}"
                );
                assert!(
                    agent
                        .subagent_sessions
                        .get(&child_sid)
                        .is_some_and(|i| i.transcript.needs_replay()),
                    "the transcript stays NeedsReplay for the first open for {meta:?}"
                );
            });
        }
    }

    #[test]
    fn subagent_spawn_and_open_replay_is_idempotent() {
        with_replay_disk_home(|_| {
            let child_sid = "child-idempotent";
            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(
                &mut app,
                child_sid,
                Some(&(child_tool_line(child_sid) + "\n")),
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "spawn must defer the replay to first open"
            );
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(child_scrollback_tool_call_count(agent, child_sid), 1);
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "re-opening must not duplicate the replay once the disk copy is recorded"
            );
        });
    }

    #[test]
    fn subagent_spawn_live_foreign_cwd_is_never_read() {
        with_replay_disk_home(|home| {
            let child_sid = "child-foreign-cwd";
            write_child_updates_jsonl_under_cwd(
                home,
                "/other/cwd",
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );

            let mut app = make_app_with_agent("sess-parent");
            spawn_subagent_with_optional_updates(&mut app, child_sid, None);

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "live spawn must not scan a foreign-cwd transcript"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "an Empty hinted miss stays NeedsReplay"
            );

            // The retry on open stays hinted-only for a live child, so the foreign-cwd transcript is still not read
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "opening a live child must not scan a foreign-cwd transcript"
            );
        });
    }

    #[test]
    fn subagent_open_resumed_child_reads_foreign_cwd_transcript() {
        with_replay_disk_home(|home| {
            let child_sid = "child-resume-foreign";
            write_child_updates_jsonl_under_cwd(
                home,
                "/other/cwd",
                child_sid,
                &(child_tool_line(child_sid) + "\n"),
            );

            let mut app = make_app_with_agent("sess-parent");
            let mut spawned = test_subagent_spawned("sess-parent", child_sid);
            let XaiSessionUpdate::SubagentSpawned { resumed_from, .. } = &mut spawned else {
                unreachable!();
            };
            *resumed_from = Some("orig-child".into());
            let _ = handle(
                make_ext_session_notification_with_method(
                    "sess-parent",
                    "x.ai/session/update",
                    spawned,
                ),
                &mut app,
            );

            let agent = app.agents.get(&AgentId(0)).unwrap();
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                0,
                "a resumed spawn must defer the replay like any other"
            );
            assert!(
                agent
                    .subagent_sessions
                    .get(child_sid)
                    .is_some_and(|i| i.transcript.needs_replay()),
                "a resumed spawn must leave the transcript NeedsReplay"
            );

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child_sid.to_string());
            assert_eq!(
                child_scrollback_tool_call_count(agent, child_sid),
                1,
                "opening a resumed child must scan for the relocated transcript"
            );
        });
    }

    #[test]
    fn both_session_transports_produce_the_same_subagent_row() {
        let child_sid = "child-equiv";
        let (spawn_notif, finish_notif) =
            run_subagent_lifecycle_via_method("x.ai/session_notification", child_sid);
        let (spawn_update, finish_update) =
            run_subagent_lifecycle_via_method("x.ai/session/update", child_sid);

        assert_eq!(spawn_notif.description, spawn_update.description);
        assert_eq!(spawn_notif.subagent_type, spawn_update.subagent_type);
        assert_eq!(spawn_notif.has_child_view, spawn_update.has_child_view);
        assert_eq!(spawn_notif.scrollback_len, spawn_update.scrollback_len);
        assert_eq!(spawn_notif.child_session_id, child_sid);
        assert_eq!(spawn_update.child_session_id, child_sid);
        assert!(matches!(spawn_notif.block_kind, SubagentBlockKind::Started));
        assert!(matches!(
            spawn_update.block_kind,
            SubagentBlockKind::Started
        ));
        assert_eq!(
            spawn_notif.scrollback_entry_id,
            spawn_update.scrollback_entry_id
        );
        assert!(spawn_notif.scrollback_entry_id.is_some());

        assert!(finish_notif.finished);
        assert!(finish_update.finished);
        assert_eq!(finish_notif.status.as_deref(), Some("completed"));
        assert_eq!(finish_update.status.as_deref(), Some("completed"));
        assert_eq!(finish_notif.tool_calls, Some(2));
        assert_eq!(finish_update.tool_calls, Some(2));
        assert_eq!(finish_notif.turns, Some(1));
        assert_eq!(finish_update.turns, Some(1));
        assert_eq!(finish_notif.duration_ms, Some(500));
        assert_eq!(finish_update.duration_ms, Some(500));
        assert!(matches!(
            finish_notif.block_kind,
            SubagentBlockKind::Completed { .. }
        ));
        assert!(matches!(
            finish_update.block_kind,
            SubagentBlockKind::Completed { .. }
        ));
    }

    #[test]
    fn spawning_on_a_background_agent_registers_it_without_redrawing() {
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let child_sid = "child-inactive";
        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-A",
                "x.ai/session/update",
                test_subagent_spawned("sess-A", child_sid),
            ),
            &mut app,
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        let info = agent_a
            .subagent_sessions
            .get(child_sid)
            .expect("SubagentSpawned must register on inactive agent A");
        assert!(
            agent_a.subagent_views.contains_key(child_sid),
            "SubagentSpawned must create subagent_views on inactive agent A"
        );
        assert_eq!(agent_a.scrollback.len(), 1);
        let entry_id = info
            .attempt.scrollback_entry_id
            .expect("inactive spawn must stash scrollback_entry_id");
        let entry = agent_a.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("inactive spawn must push SubagentBlock");
        };
        assert!(matches!(sb.kind, SubagentBlockKind::Started));
        assert!(
            !affected,
            "SubagentSpawned on inactive agent must not request a redraw"
        );

        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-A",
                "x.ai/session/update",
                test_subagent_finished(child_sid),
            ),
            &mut app,
        );
        assert!(
            !affected,
            "SubagentFinished on inactive agent must not request a redraw"
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        let info = agent_a.subagent_sessions.get(child_sid).unwrap();
        assert!(info.is_finished());
        assert_eq!(info.attempt.status.as_deref(), Some("completed"));
        let entry = agent_a.scrollback.get_by_id(entry_id).unwrap();
        let RenderBlock::Subagent(sb) = &entry.block else {
            panic!("inactive finish must keep SubagentBlock");
        };
        assert!(matches!(sb.kind, SubagentBlockKind::Completed { .. }));
    }

    #[test]
    fn a_spawn_for_an_unknown_session_is_ignored() {
        let mut app = make_app_with_agent("sess-A");
        let affected = handle(
            make_ext_session_notification_with_method(
                "sess-unknown",
                "x.ai/session/update",
                test_subagent_spawned("sess-unknown", "child-unknown"),
            ),
            &mut app,
        );

        assert!(!affected, "unknown session_id must not request a redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.subagent_sessions.is_empty(),
            "SubagentSpawned for unknown session must not register subagent_sessions"
        );
        assert!(
            agent.scrollback.is_empty(),
            "SubagentSpawned for unknown session must not push scrollback"
        );
    }

    #[test]
    fn a_malformed_session_notification_is_ignored() {
        let mut app = make_app_with_agent("sess-A");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        // Valid JSON but not a SessionNotification: parse must fail quietly.
        let raw =
            serde_json::value::to_raw_value(&serde_json::json!({"unexpected": true})).unwrap();
        let request = acp::ExtNotification::new("x.ai/session/update", raw.into());
        let msg = AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
            request,
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(
            !affected,
            "malformed x.ai/session/update params must not redraw"
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap().scrollback.is_empty(),
            "malformed notification must not mutate scrollback"
        );
    }

    #[test]
    fn a_notification_reaches_its_target_agent_even_when_another_is_active() {
        // AutoCompactCompleted on the Remote-Code ext path resets the context bar numerator via refresh_context_used
        // That side effect must run on the matched agent regardless of which view is currently active
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        // Seed A with a stale context-used reading so we can prove the notification reset it
        {
            let agent_a = app.agents.get_mut(&AgentId(0)).unwrap();
            agent_a.apply_context_used(90_000, 131_072);
        }
        switch_active_to(&mut app, AgentId(1));

        let affected = handle(
            make_ext_session_notification(
                "sess-A",
                XaiSessionUpdate::AutoCompactCompleted {
                    tokens_before: Some(90_000),
                    tokens_after: 25_000,
                    elapsed_ms: Some(300),
                    summary_preview: None,
                },
            ),
            &mut app,
        );

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.context_state.as_ref().map(|c| c.used),
            Some(25_000),
            "AutoCompactCompleted must reset A's context_used even when B is active"
        );
        assert!(
            !affected,
            "ext notification routed to a non-active agent must not request a redraw"
        );
    }

    /// The wake replaces A's tracker; B keeps the one its spawn created. Both share the root's registry,
    /// so each names the other, and A's label is the wake's description and persona, not the first spawn's.
    #[test]
    fn spawn_labels_are_recorded_overwritten_on_wake_and_shared_with_child_trackers() {
        use crate::acp::meta::NotificationMeta;
        use crate::scrollback::block::RenderBlock;
        use crate::scrollback::blocks::tool::ToolCallBlock;
        use xai_grok_tools::implementations::grok_build::send_subagent_message::{
            SEND_SUBAGENT_MESSAGE_TOOL_NAME, SendSubagentMessageOutput,
        };
        use xai_grok_tools::types::output::ToolOutput;

        let mut app = make_app_with_agent("sess-parent");
        let spawn = |child: &str, attempt: &str, description: &str, persona: Option<&str>| {
            let mut update = test_subagent_spawned_for_attempt("sess-parent", child, Some(attempt));
            let XaiSessionUpdate::SubagentSpawned { description: wire, persona: wire_persona, .. } =
                &mut update
            else {
                panic!("spawn fixture must be a SubagentSpawned update");
            };
            *wire = description.to_owned();
            *wire_persona = persona.map(str::to_owned);
            make_ext_session_notification("sess-parent", update)
        };
        assert!(handle(spawn("child-a", "at1.one", "scan src/", None), &mut app));
        assert!(handle(spawn("child-b", "at1.one", "index docs/", None), &mut app));
        assert!(handle(
            make_ext_session_notification(
                "sess-parent",
                test_subagent_finished_for_attempt("child-a", Some("at1.one")),
            ),
            &mut app,
        ));
        assert!(handle(
            spawn("child-a", "at1.two", "[review] recheck the callers", Some("scout")),
            &mut app,
        ));

        let accepted = serde_json::to_value(ToolOutput::SendSubagentMessage(
            SendSubagentMessageOutput::Accepted { message_id: "message-1".into() },
        ))
        .unwrap();
        for (sender, target, expected_header) in [
            ("child-a", "child-b", "Message sent to Explore \u{201c}index docs/\u{201d}"),
            ("child-b", "child-a", "Message sent to Scout \u{201c}recheck the callers\u{201d}"),
        ] {
            let send = acp::ToolCall::new(
                acp::ToolCallId::new("send-1"),
                SEND_SUBAGENT_MESSAGE_TOOL_NAME,
            )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Completed)
                .raw_input(Some(serde_json::json!({"subagent_id": target, "text": "follow up"})))
                .raw_output(Some(accepted.clone()));
            let child = app
                .agents
                .get_mut(&AgentId(0))
                .unwrap()
                .subagent_view_mut(sender)
                .expect("child view");
            child.session.tracker.handle_update(
                acp::SessionUpdate::ToolCall(send),
                &NotificationMeta::default(),
                &mut child.scrollback,
            );

            let entry = child.scrollback.get(0).expect("the send row");
            let RenderBlock::ToolCall(ToolCallBlock::SentMessage(block)) = &entry.block else {
                panic!("expected a sent-message block, got {:?}", entry.block);
            };
            assert_eq!(expected_header, block.header_text(), "{sender} -> {target}");
            assert_eq!(Some(target), block.child_session_id(), "{sender} -> {target}");
        }
    }
