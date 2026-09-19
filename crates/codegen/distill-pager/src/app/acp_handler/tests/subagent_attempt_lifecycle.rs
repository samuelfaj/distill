#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;
    use crate::app::subagent::ChildTranscript;

    fn sequenced(app: &mut crate::app::app_view::AppView, update: XaiSessionUpdate, seq: u64) -> bool {
        handle_ext_notification(&subagent_notification_with_seq("sess-parent", update, seq), app)
    }

    fn replay(app: &mut crate::app::app_view::AppView, update: XaiSessionUpdate, seq: u64) -> bool {
        handle_ext_notification(&subagent_ext_replay("sess-parent", serde_json::to_value(update).unwrap(), &format!("sess-parent-{seq}")), app)
    }

    fn spawn(app: &mut crate::app::app_view::AppView, child: &str, attempt: &str, seq: u64) -> bool {
        sequenced(app, test_subagent_spawned_for_attempt("sess-parent", child, Some(attempt)), seq)
    }

    fn finish_with_tokens(
        app: &mut crate::app::app_view::AppView,
        child: &str,
        attempt: &str,
        tokens_used: u64,
        seq: u64,
    ) -> bool {
        let mut update = test_subagent_finished_for_attempt(child, Some(attempt));
        let XaiSessionUpdate::SubagentFinished { tokens_used: tokens, .. } = &mut update else {
            unreachable!()
        };
        *tokens = tokens_used;
        sequenced(app, update, seq)
    }

    fn finish(app: &mut crate::app::app_view::AppView, child: &str, attempt: &str, seq: u64) -> bool {
        finish_with_tokens(app, child, attempt, 0, seq)
    }

    fn progress_update_for_attempt(
        child: &str,
        attempt: &str,
        duration_ms: u64,
        tokens_used: u64,
    ) -> XaiSessionUpdate {
        let mut update = test_subagent_progress("sess-parent", child);
        let XaiSessionUpdate::SubagentProgress {
            attempt_id,
            duration_ms: duration,
            tokens_used: tokens,
            ..
        } = &mut update else { unreachable!() };
        *attempt_id = Some(attempt.to_owned());
        *duration = duration_ms;
        *tokens = tokens_used;
        update
    }

    fn progress_update(child: &str, duration_ms: u64, tokens_used: u64) -> XaiSessionUpdate {
        progress_update_for_attempt(child, "at1.one", duration_ms, tokens_used)
    }

    fn legacy_finish(app: &mut crate::app::app_view::AppView, child: &str, seq: u64) -> bool {
        sequenced(app, test_subagent_finished_for_attempt(child, None), seq)
    }

    fn enable_replay(app: &mut crate::app::app_view::AppView) {
        app.agents.get_mut(&AgentId(0)).unwrap().session.loading_replay = true;
    }

    fn terminal_rows(app: &crate::app::app_view::AppView, child: &str) -> Vec<crate::scrollback::entry::EntryId> {
        (0..app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len()).filter_map(|index| {
            let entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.entry(index)?;
            matches!(&entry.block, RenderBlock::Subagent(block)
                if block.child_session_id == child && block.is_background
                    && !matches!(block.kind, SubagentBlockKind::Started)).then_some(entry.id)
        }).collect()
    }

    fn assert_attempt(app: &crate::app::app_view::AppView, child: &str, attempt: &str, finished: bool, background: bool) {
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(info.attempt.lifecycle.current_attempt_id(), Some(attempt));
        assert_eq!(info.is_finished(), finished);
        assert_eq!(info.attempt.is_background, background);
        let entry = agent.scrollback.get_by_id(info.attempt.scrollback_entry_id.unwrap()).unwrap();
        assert!(matches!(&entry.block, RenderBlock::Subagent(block) if block.is_background == background));
    }

    #[test]
    fn finish_before_spawn_is_applied_after_later_cursor_progress() {
        let mut app = make_app_with_agent("sess-parent");
        let notification = |update, seq| subagent_notification_with_seq("sess-parent", update, seq);
        assert!(!handle_ext_notification(&notification(test_subagent_finished("child"), 2), &mut app));
        assert!(handle_ext_notification(
            &notification(serde_json::from_value(goal_update_value("goal-1", "active", 0)).unwrap(), 3),
            &mut app,
        ));
        assert!(handle_ext_notification(&notification(test_subagent_spawned("sess-parent", "child"), 1), &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert!(agent.subagent_sessions.get("child").unwrap_or_else(|| panic!("missing map entry")).is_finished());
        assert_eq!(agent.last_seen_event_id.as_deref(), Some("sess-parent-3"));
    }

    #[test]
    fn replayed_wake_with_a_missing_prior_row_becomes_current_and_running() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-replayed-wake";
        enable_replay(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1));
        assert!(replay(&mut app, test_subagent_finished_for_attempt(child, Some("at1.one")), 2));
        let old_entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id.unwrap();
        app.agents.get_mut(&AgentId(0)).unwrap().scrollback.remove_entry(old_entry);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.two")), 3));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(info.attempt.lifecycle.current_attempt_id(), Some("at1.two"));
        assert!(info.is_running());
        assert!(info.attempt.is_background);
        let entry = agent.scrollback.get_by_id(info.attempt.scrollback_entry_id.unwrap()).unwrap();
        assert!(entry.is_running);
        assert!(matches!(&entry.block, RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Started)));
    }

    #[test]
    fn replayed_pending_finish_uses_the_new_attempt_terminal_payload() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-pending-wake-finish";
        enable_replay(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1));
        assert!(replay(&mut app, test_subagent_finished_for_attempt(child, Some("at1.one")), 2));
        let entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id.unwrap();
        app.agents.get_mut(&AgentId(0)).unwrap().scrollback.remove_entry(entry);

        let mut update = test_subagent_finished_for_attempt(child, Some("at1.two"));
        let XaiSessionUpdate::SubagentFinished { status, error, tool_calls, turns, duration_ms, tokens_used, .. } = &mut update else { unreachable!() };
        *status = "failed".into();
        *error = Some("wake failed".into());
        (*tool_calls, *turns, *duration_ms, *tokens_used) = (7, 3, 9_876, 543);
        assert!(!replay(&mut app, update, 4));
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.two")), 3));

        let attempt = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt;
        assert_eq!(attempt.status.as_deref(), Some("failed"));
        assert_eq!(attempt.error.as_deref(), Some("wake failed"));
        assert_eq!((attempt.tool_calls, attempt.turns, attempt.duration_ms, attempt.tokens_used),
            (Some(7), Some(3), Some(9_876), Some(543)));
    }

    #[test]
    fn replaying_a_spawn_rebuilds_a_removed_terminal_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-finished-before-rebuild";
        enable_replay(&mut app);
        let spawn = subagent_ext_replay("sess-parent", serde_json::to_value(test_subagent_spawned("sess-parent", child)).unwrap(), "sess-parent-1");
        let finish = subagent_ext_replay("sess-parent", serde_json::to_value(test_subagent_finished(child)).unwrap(), "sess-parent-2");
        assert!(handle_ext_notification(&spawn, &mut app));
        let first = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id.unwrap();
        app.agents.get_mut(&AgentId(0)).unwrap().scrollback.remove_entry(first);
        assert!(handle_ext_notification(&finish, &mut app));
        assert!(handle_ext_notification(&spawn, &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.is_finished());
        let rebuilt = info.attempt.scrollback_entry_id.unwrap();
        assert_ne!(rebuilt, first);
        assert!(matches!(&agent.scrollback.get_by_id(rebuilt).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. })));
        assert!(!agent.scrollback.needs_animation());
    }

    #[test]
    fn replaying_a_background_spawn_keeps_its_visible_terminal_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-bg-terminal-kept";
        enable_replay(&mut app);
        // The replayed task tool call re-marks the child background before each replayed spawn.
        let mark_background = |app: &mut crate::app::app_view::AppView| {
            app.agents.get_mut(&AgentId(0)).unwrap().session.tracker.task_tool_background.insert(child.into(), true);
        };
        mark_background(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1));
        assert!(replay(&mut app, test_subagent_finished_for_attempt(child, Some("at1.one")), 2));
        let attempt = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt;
        let started = attempt.scrollback_entry_id.unwrap();
        let terminal = attempt.terminal_entry_id.unwrap();
        assert_eq!(terminal_rows(&app, child), vec![terminal]);
        app.agents.get_mut(&AgentId(0)).unwrap().scrollback.remove_entry(started);

        mark_background(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.is_finished());
        assert_ne!(info.attempt.scrollback_entry_id.unwrap(), started);
        assert_eq!(info.attempt.terminal_entry_id, Some(terminal));
        assert_eq!(terminal_rows(&app, child), vec![terminal]);
        assert!(matches!(&agent.scrollback.get_by_id(terminal).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. }) && block.is_background));
    }

    #[test]
    fn replaying_a_wake_spawn_keeps_the_attempt_background() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-wake-rebuilt";
        // A wake is background only via the wake bit: no task tool call re-marks it on replay.
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(finish(&mut app, child, "at1.one", 2));
        assert!(spawn(&mut app, child, "at1.two", 3));
        assert!(finish(&mut app, child, "at1.two", 4));
        assert_attempt(&app, child, "at1.two", true, true);
        let attempt = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt;
        let started = attempt.scrollback_entry_id.unwrap();
        let terminal = attempt.terminal_entry_id.unwrap();
        assert_eq!(terminal_rows(&app, child), vec![terminal]);
        app.agents.get_mut(&AgentId(0)).unwrap().scrollback.remove_entry(started);

        enable_replay(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.two")), 3));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.is_finished());
        assert!(info.attempt.is_background);
        assert_ne!(info.attempt.scrollback_entry_id.unwrap(), started);
        assert!(matches!(&agent.scrollback.get_by_id(info.attempt.scrollback_entry_id.unwrap()).unwrap().block,
            RenderBlock::Subagent(block) if block.is_background));
        assert_eq!(info.attempt.terminal_entry_id, Some(terminal));
        assert_eq!(terminal_rows(&app, child), vec![terminal]);
        assert!(matches!(&agent.scrollback.get_by_id(terminal).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. }) && block.is_background));
    }

    #[test]
    fn full_reload_rebuilds_a_partial_background_child_from_persisted_tail() {
        with_replay_disk_home(|home| {
            let child = "child-reload-tail";
            let mut app = make_app_with_agent("sess-parent");
            assert!(spawn(&mut app, child, "at1.one", 1));
            assert!(handle(make_agent_chunk_message(child, "partial live output"), &mut app));
            {
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                let info = agent.subagent_sessions.get_mut(child).unwrap();
                info.attempt.is_background = true;
                info.transcript = ChildTranscript::DiskBacked;
                let started = info.attempt.scrollback_entry_id.unwrap();
                agent.scrollback.remove_entry(started);
                agent.session.loading_replay = true;
            }
            write_child_updates_jsonl(home, child, &(child_tool_line(child) + "\n"));

            let _ = replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1);
            assert!(replay(&mut app, test_subagent_finished_for_attempt(child, Some("at1.one")), 2));
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.open_subagent_fullscreen(child.to_owned());

            assert_eq!(child_scrollback_tool_call_count(agent, child), 1);
            assert_eq!(agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).transcript, ChildTranscript::DiskBacked);
        });
    }

    #[test]
    fn terminal_rebuild_keeps_late_replay_grace() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-late-terminal-rebuild";
        assert!(handle(make_ext_session_notification("sess-parent", test_subagent_spawned("sess-parent", child)), &mut app));
        assert!(handle(make_ext_session_notification("sess-parent", test_subagent_finished(child)), &mut app));
        let entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id.unwrap();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.scrollback.remove_entry(entry);
        agent.arm_late_replay_grace();

        assert!(replay(&mut app, test_subagent_spawned("sess-parent", child), 1));
        assert!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).late_replay_until.is_some());
        assert!(!replay(&mut app, test_subagent_progress("sess-parent", child), 2));
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.turn_count, None);
        assert!(agent.late_replay_until.is_some());
    }

    #[test]
    fn replayed_progress_respects_attempt_sequence_high_water() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-progress-high-water";
        enable_replay(&mut app);
        assert!(replay(&mut app, test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")), 1));
        assert!(replay(&mut app, progress_update(child, 500, 50), 5));
        assert!(!replay(&mut app, progress_update(child, 300, 30), 3));
        assert!(!sequenced(&mut app, progress_update(child, 400, 40), 4));

        let attempt = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt;
        assert_eq!(attempt.duration_ms, Some(500));
        assert_eq!(attempt.tokens_used, Some(50));
        assert_eq!(attempt.lifecycle.last_event_seq(), Some(5));
    }

    #[test]
    fn same_attempt_replay_rebuild_preserves_progress_counters() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-progress-rebuild";
        enable_replay(&mut app);
        assert!(replay(
            &mut app,
            test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")),
            1,
        ));
        assert!(replay(&mut app, progress_update(child, 1_000, 100), 2));
        let tokens_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).live_standalone_subagent_tokens();
        let entry = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"))
            .attempt
            .scrollback_entry_id
            .unwrap();
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .scrollback
            .remove_entry(entry);

        assert!(replay(
            &mut app,
            test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one")),
            1,
        ));
        assert!(!replay(&mut app, progress_update(child, 1_000, 100), 2));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let attempt = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt;
        assert_eq!(attempt.duration_ms, Some(1_000));
        assert_eq!(attempt.tokens_used, Some(100));
        assert_eq!(attempt.lifecycle.last_event_seq(), Some(2));
        assert_eq!(agent.live_standalone_subagent_tokens(), tokens_before);
    }

    #[test]
    fn completed_attempt_tokens_survive_wake_progress() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-wake-tokens";
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(sequenced(
            &mut app,
            progress_update_for_attempt(child, "at1.one", 100, 100),
            2,
        ));
        assert!(finish(&mut app, child, "at1.one", 3));
        assert!(spawn(&mut app, child, "at1.two", 4));
        assert!(sequenced(
            &mut app,
            progress_update_for_attempt(child, "at1.two", 30, 30),
            5,
        ));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(info.completed_attempt_tokens, 100);
        assert_eq!(info.attempt.tokens_used, Some(30));
        assert_eq!(agent.live_standalone_subagent_tokens(), 130);
        let mut goal = crate::app::agent::GoalDisplayState::test_stub();
        goal.tokens_used = 100;
        assert_eq!(
            goal.live_tokens_used(Some(0), agent.live_standalone_subagent_tokens()),
            130,
        );
    }

    #[test]
    fn record_only_finish_corrects_retired_attempt_tokens() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-retired-tokens";
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(sequenced(
            &mut app,
            progress_update_for_attempt(child, "at1.one", 100, 100),
            2,
        ));
        assert!(spawn(&mut app, child, "at1.two", 3));
        assert!(!finish_with_tokens(&mut app, child, "at1.one", 120, 4));

        let info = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(info.completed_attempt_tokens, 120);
        assert_eq!(info.attempt.lifecycle.current_attempt_id(), Some("at1.two"));
        assert!(info.is_running());
    }

    #[test]
    fn overlapping_wake_stays_background_when_prior_finish_arrives_late() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-overlap";
        assert!(spawn(&mut app, child, "at1.one", 1));
        let first = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.scrollback_entry_id.unwrap();
        assert!(spawn(&mut app, child, "at1.two", 2));
        assert_attempt(&app, child, "at1.two", false, true);
        assert!(!app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.get_by_id(first).unwrap().is_running);
        assert!(!finish(&mut app, child, "at1.one", 3));
        assert!(terminal_rows(&app, child).is_empty());
        assert!(finish(&mut app, child, "at1.two", 4));
        assert_attempt(&app, child, "at1.two", true, true);
        assert_eq!(terminal_rows(&app, child).len(), 1);
    }

    #[test]
    fn first_finish_before_spawn_is_foreground() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-first-finish";
        assert!(!finish(&mut app, child, "at1.one", 2));
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert_attempt(&app, child, "at1.one", true, false);
        assert!(terminal_rows(&app, child).is_empty());
        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert!(matches!(&agent.scrollback.get_by_id(info.attempt.scrollback_entry_id.unwrap()).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. }) && !block.is_background));
    }

    #[test]
    fn legacy_finish_before_typed_spawn_finishes_that_attempt() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-legacy-finish-first";
        assert!(!legacy_finish(&mut app, child, 2));
        assert!(spawn(&mut app, child, "at1.one", 1));

        assert_attempt(&app, child, "at1.one", true, false);
    }

    #[test]
    fn legacy_finish_before_two_typed_spawns_finishes_only_the_first() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-legacy-finish-before-wake";
        assert!(!legacy_finish(&mut app, child, 2));
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(spawn(&mut app, child, "at1.two", 3));

        assert_attempt(&app, child, "at1.two", false, true);
        assert!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"))
            .attempt
            .lifecycle
            .is_attempt_finished("at1.one"));
    }

    #[test]
    fn pending_wake_finish_renders_one_background_terminal_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-pending-wake";
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(finish(&mut app, child, "at1.one", 2));
        assert!(!finish(&mut app, child, "at1.two", 4));
        assert!(spawn(&mut app, child, "at1.two", 3));
        assert_attempt(&app, child, "at1.two", true, true);
        let rows = terminal_rows(&app, child);
        assert_eq!(rows.len(), 1);
        let Some(row) = rows.first().copied() else {
            panic!("missing terminal row");
        };
        assert!(matches!(&app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.get_by_id(row).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. }) && block.is_background));
    }

    #[test]
    fn wake_finish_gets_its_own_terminal_row() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-woken";
        app.agents.get_mut(&AgentId(0)).unwrap().session.tracker.task_tool_background.insert(child.into(), true);
        assert!(handle(make_ext_session_notification("sess-parent", test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.one"))), &mut app));
        assert!(handle(make_ext_session_notification("sess-parent", test_subagent_finished_for_attempt(child, Some("at1.one"))), &mut app));
        let first = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.terminal_entry_id.unwrap();
        assert!(handle(make_ext_session_notification("sess-parent", test_subagent_spawned_for_attempt("sess-parent", child, Some("at1.two"))), &mut app));
        let mut update = test_subagent_finished_for_attempt(child, Some("at1.two"));
        let XaiSessionUpdate::SubagentFinished { status, error, .. } = &mut update else { unreachable!() };
        *status = "failed".into();
        *error = Some("second attempt failed".into());
        assert!(handle(make_ext_session_notification("sess-parent", update), &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        let second = agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry")).attempt.terminal_entry_id.unwrap();
        assert_ne!(first, second);
        assert!(matches!(&agent.scrollback.get_by_id(first).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Completed { .. })));
        assert!(matches!(&agent.scrollback.get_by_id(second).unwrap().block,
            RenderBlock::Subagent(block) if matches!(block.kind, SubagentBlockKind::Failed { .. })));
    }

    #[test]
    fn new_attempt_resets_tool_delta_indexes_and_activity() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-tool-deltas";
        let delta = || XaiSessionUpdate::ToolCallDeltaChunk {
            tool_call_id: Some("call-1".into()),
            tool_index: 0,
            name: Some("write".into()),
            arguments_delta: None,
        };

        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(handle(make_ext_session_notification(child, delta()), &mut app));
        assert!(finish(&mut app, child, "at1.one", 2));
        let first_view = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView;
        assert!(spawn(&mut app, child, "at1.two", 3));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(agent.subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView, first_view);
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert!(info.attempt.activity_label.is_none());
        assert!(handle(make_ext_session_notification(child, delta()), &mut app));
        assert!(matches!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry"))
                .session
                .tracker
                .activity(),
            Some(TurnActivity::WritingToolCall(_))
        ));
    }

    #[test]
    fn live_wake_keeps_both_attempts_in_the_child_transcript() {
        let mut app = make_app_with_agent("sess-parent");
        let child = "child-live-wake-transcript";
        assert!(spawn(&mut app, child, "at1.one", 1));
        assert!(handle(make_agent_chunk_with_event(child, "first attempt", "p-child-1", None), &mut app));
        {
            let info = app.agents.get_mut(&AgentId(0)).unwrap().subagent_sessions.get_mut(child).unwrap();
            info.prompt = Some("preserved prompt".into());
            info.child_cwd = Some("/preserved/cwd".into());
            info.worktree_path = Some("/preserved/worktree".into());
            info.transcript = ChildTranscript::MemoryOnly;
        }
        assert!(finish(&mut app, child, "at1.one", 2));
        let first_view = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView;
        assert!(spawn(&mut app, child, "at1.two", 3));
        assert!(handle(make_agent_chunk_with_event(child, "second attempt", "p-child-2", None), &mut app));

        let agent = &app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(agent.subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).as_ref() as *const AgentView, first_view);
        let info = &agent.subagent_sessions.get(child).unwrap_or_else(|| panic!("missing map entry"));
        assert_eq!(info.prompt.as_deref(), Some("preserved prompt"));
        assert_eq!(info.child_cwd.as_deref(), Some("/preserved/cwd"));
        assert_eq!(info.worktree_path.as_deref(), Some("/preserved/worktree"));
        assert_eq!(info.transcript, ChildTranscript::MemoryOnly);
        let messages: Vec<_> = (0..agent.subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).scrollback.len()).filter_map(|index| {
            let entry = agent.subagent_views.get(child).unwrap_or_else(|| panic!("missing map entry")).scrollback.entry(index)?;
            let RenderBlock::AgentMessage(message) = &entry.block else { return None };
            Some(message.text())
        }).collect();
        assert_eq!(messages, ["first attempt", "second attempt"]);
    }
