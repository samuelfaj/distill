#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn driver_prompt_complete_without_prompt_id_arms_reconcile_not_finish() {
        // Driver still owns the turn via PromptResponse: prompt_complete must NOT finish immediately
        // Missing wire promptId (legacy shells) arms lost-PR reconcile on current_prompt_id so grace teardown can run if the RPC never arrives
        // Turn state stays TurnRunning
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            agent.turn_started_at = Some(std::time::Instant::now());
            assert!(!agent.attached_as_viewer);
        }

        let affected = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);
        assert!(
            affected,
            "arming reconcile must schedule ticks for background-tab recovery"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "driver's running turn must NOT be finished by prompt_complete"
        );
        assert_eq!(
            agent.session.current_prompt_id.as_deref(),
            Some("pid-local"),
            "driver's current_prompt_id must be untouched at arm time"
        );
        assert!(agent.turn_started_at.is_some());
        assert_eq!(
            agent
                .pending_turn_end_reconcile
                .as_ref()
                .map(|p| p.prompt_id.as_str()),
            Some("pid-local"),
        );
    }

    #[test]
    fn driver_prompt_complete_with_matching_prompt_id_arms_reconcile() {
        // Lost-response recovery: the driver receives the turn-end broadcast for the exact turn it is awaiting
        // It must ARM the deferred reconcile without finishing the turn immediately
        // The RPC response normally lands ms later and carries richer context; finishing here would double-finish every turn
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-stuck".into());
            agent.session.cancel_turn(&mut agent.scrollback); // CancelTurn → TurnCancelling
            assert!(!agent.attached_as_viewer);
        }

        let affected = handle_ext_notification(
            &prompt_complete_ext_with_prompt_id("sess-drive", "pid-stuck", "cancelled"),
            &mut app,
        );
        assert!(
            affected,
            "arming must report a state change — the event loop only calls \
             schedule_tick on changed ACP batches, and the reconcile sweep \
             runs on the animation tick (a dormant background tab would \
             otherwise never get swept)"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_cancelling(),
            "turn state must be untouched at arm time (RPC may still arrive)"
        );
        let pending = agent
            .pending_turn_end_reconcile
            .as_ref()
            .expect("reconcile must be armed for the driver's awaited turn");
        assert_eq!(pending.prompt_id, "pid-stuck");
        assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
    }

    #[test]
    fn driver_prompt_complete_with_mismatched_prompt_id_does_not_arm() {
        // A broadcast for some OTHER prompt must not arm a reconcile against the turn this client is actually driving
        // Other prompts here: a stale one, or a queued prompt that resolved server-side
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-current".into());
        }

        let _ = handle_ext_notification(
            &prompt_complete_ext_with_prompt_id("sess-drive", "pid-other", "end_turn"),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.pending_turn_end_reconcile.is_none());
        assert!(matches!(agent.session.state, AgentState::TurnRunning));
    }

    #[test]
    fn driver_prompt_complete_without_prompt_id_arms_on_current() {
        // Older shells omit `promptId`; arm reconcile on current_prompt_id when not mid-tool (see arm_driver_turn_end_reconcile)
        // The turn is not finished here
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-current".into());
        }

        let _ = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .pending_turn_end_reconcile
                .as_ref()
                .map(|p| p.prompt_id.as_str()),
            Some("pid-current"),
        );
        assert!(matches!(agent.session.state, AgentState::TurnRunning));
    }

    #[test]
    fn driver_prompt_complete_pushes_no_marker() {
        // The driver emits its own marker via PromptResponse; prompt_complete must not double-push one for it (or push any block at all)
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            agent.turn_started_at = Some(std::time::Instant::now());
            assert!(!agent.attached_as_viewer);
        }

        let len_before = app.agents.get(&AgentId(0)).unwrap().scrollback.len();
        let _ = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "the driver must not get any new block from prompt_complete"
        );
    }

    #[test]
    fn live_turn_completed_finalizes_viewer_turn_and_duplicate_is_noop() {
        // The durable `TurnCompleted` is the viewer's non-interactive exit from TurnRunning on the replayed rail
        // It parallels the fire-and-forget `prompt_complete`
        // A viewer adopting the driver's live turn must drop back to Idle with a marker when it arrives
        let mut app = make_app_with_agent("sess-view");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-view", "chunk", "pid-driver", false),
            &mut app,
        );
        assert!(matches!(
            app.agents.get(&AgentId(0)).unwrap().session.state,
            AgentState::TurnRunning
        ));

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-view", "pid-driver", "end_turn", false),
            &mut app,
        );
        assert!(affected, "finalizing the active viewer turn should redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_idle(),
            "a live TurnCompleted must drop a viewer back to Idle"
        );
        assert!(agent.session.current_prompt_id.is_none());
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { .. })
        ));

        // A duplicate/stale terminal for the now-finished turn is a no-op.
        let len_before = app.agents.get(&AgentId(0)).unwrap().scrollback.len();
        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-view", "pid-driver", "end_turn", false),
            &mut app,
        );
        assert!(!affected, "a duplicate TurnCompleted must be a no-op");
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap().scrollback.len(),
            len_before,
            "a duplicate TurnCompleted must not push a second marker"
        );
    }

    #[test]
    fn unknown_error_kind_from_wire_is_never_sniff_reclassified() {
        // A NEWER shell's kind the pager doesn't know arrives through the real ingress
        // The result quotes a truncation phrase and carries no status
        // A present kind blocks the sniff reclassification, so it renders generic copy, not truncation
        let mut app = make_app_with_agent("sess-view");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-view", "chunk", "pid-driver", false),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_failed_with_error_kind(
                "sess-view",
                "pid-driver",
                "a future failure quoting: response truncated by max_tokens",
                "a_future_kind",
                false,
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert!(
                    error.starts_with("Request failed"),
                    "unknown kind must keep generic copy, got {error:?}"
                );
                assert!(!error.contains("Response truncated"));
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn live_turn_completed_error_kind_renders_truncation_copy() {
        let mut app = make_app_with_agent("sess-view");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-view", "chunk", "pid-driver", false),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_failed_with_error_kind(
                "sess-view",
                "pid-driver",
                "turn ended early",
                "max_tokens_truncation",
                false,
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert_eq!(error, "Response truncated: turn ended early")
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn live_turn_completed_driver_arms_reconcile() {
        // For the driver the `PromptResponse` RPC owns the lifecycle
        // A live TurnCompleted for the turn it is driving arms the lost-RPC reconcile WITHOUT finishing the turn
        // This mirrors the `prompt_complete` driver path
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            assert!(!agent.attached_as_viewer);
        }

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-drive", "pid-local", "cancelled", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "the driver's turn must NOT be finished by a live TurnCompleted"
        );
        let pending = agent
            .pending_turn_end_reconcile
            .as_ref()
            .expect("the driver's awaited turn must arm a reconcile");
        assert_eq!(pending.prompt_id, "pid-local");
        assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
    }

    #[test]
    fn silent_wake_turn_completed_is_markerless() {
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks(&mut app, "sess-wake");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif(
                "sess-wake",
                "task-completed-bg1",
                Some(1_700_000_000_000 + 5_000),
            ),
            &mut app,
        );
        assert!(affected, "the wake back-to-idle point still redraws");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_idle(),
            "a wake turn is never adopted — the pager stays idle around it"
        );
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "a silent wake turn pushes no marker"
        );
        assert_eq!(
            agent.watchers().commands,
            2,
            "the running commands stay on the status-row watchers cue"
        );
    }

    #[test]
    fn chatty_wake_turn_completed_pushes_one_marker() {
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        assert_eq!(count_turn_markers(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"))), 0);

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert!(affected);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            count_turn_markers(agent),
            1,
            "a chatty wake closes with exactly one marker"
        );
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn duplicate_wake_terminal_pushes_no_second_marker() {
        // `finish_wake_turn` snapshots the output epoch, so a duplicate sees no new output.
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert_eq!(count_turn_markers(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"))), 1);

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert_eq!(
            count_turn_markers(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry"))),
            1,
            "a duplicate wake terminal must not push a second marker"
        );
    }

    #[test]
    fn wake_turn_stop_affordance_offered_then_cleared_at_terminal() {
        // The pane stays Idle around a wake turn, so the stop control is keyed on `running_wake_turn`
        // That flag is set by the first live wake delta and cleared by the wake terminal
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.wake_display_state(), Some(AgentState::TurnRunning)),
            "a streaming wake turn must offer the running chrome (and [stop])"
        );

        // A delta arriving mid-cancel must not reset the cancelling phase.
        if let Some(wake) = app
            .agents
            .get_mut(&AgentId(0))
            .unwrap()
            .running_wake_turn
            .as_mut()
        {
            wake.cancel_sent = true;
        }
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 6_000),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.wake_display_state(), Some(AgentState::TurnCancelling)),
            "a later delta must not clobber the cancelling phase"
        );

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.running_wake_turn.is_none() && agent.wake_display_state().is_none(),
            "the wake terminal must retire the stop affordance"
        );

        // Deltas and the terminal arrive on separate channels: a late delta for the finished wake must not revive the stop control
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 7_000),
            &mut app,
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).running_wake_turn.is_none(),
            "a late delta after the terminal must not revive the stop affordance"
        );

        // A second wake finishing must not forget the first: bg1's late delta stays dead after bg2's terminal lands too
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg2", 8_000),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg2", None),
            &mut app,
        );
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 9_000),
            &mut app,
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).running_wake_turn.is_none(),
            "an earlier finished wake stays finished after later terminals"
        );
    }

    #[test]
    fn wake_terminal_drains_parked_follow_up() {
        use crate::app::actions::Effect;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .enqueue_prompt("follow-up after wake".into());

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert!(
            app.pending_effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { text, .. } if text == "follow-up after wake")),
            "wake terminal must drain the parked follow-up; effects = {:?}",
            app.pending_effects
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).session.pending_prompts.is_empty(),
            "the parked row must leave the local queue"
        );
    }

    #[test]
    fn wake_terminal_does_not_drain_while_reconnect_pending() {
        use crate::app::actions::Effect;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .enqueue_prompt("follow-up after wake".into());
        app.reconnect_pending = true;

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert!(
            !app.pending_effects
                .iter()
                .any(|e| matches!(e, Effect::SendPrompt { .. })),
            "reconnect must hold the parked follow-up; effects = {:?}",
            app.pending_effects
        );
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).session.pending_prompts.len(),
            1,
            "the parked row must stay queued until reconnect drains"
        );
    }

    #[test]
    fn wake_terminal_finishes_in_flight_streamed_entry() {
        // The terminal is a wake's ONLY flush site (wakes skip PromptResponse).
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.has_running_entries(),
            "the streamed wake chunk opens a live entry"
        );

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert!(
            !app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.has_running_entries(),
            "the wake terminal must finish the streamed entry"
        );
    }

    #[test]
    fn wake_turn_completed_in_replay_records_pid_and_visible_marker() {
        // A visible wake still records its pid and also gets a marker
        // The chunk must be isReplay: live output_epoch does not count
        let mut app = make_app_with_agent("sess-wake");
        begin_replay(&mut app);
        let _ = handle(
            make_replay_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let started_at = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).turn_started_at;

        let affected = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-wake",
                "task-completed-bg1",
                "end_turn",
                Some(1500),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );

        assert!(!affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent
                .replayed_terminal_prompts
                .contains("task-completed-bg1"),
            "the replay arm must keep recording wake pids"
        );
        assert_eq!(agent.scrollback.len(), len_before + 1);
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { elapsed: Some(_) })
        ));
        assert_eq!(agent.turn_started_at, started_at, "replay must not finalize");
    }

    #[test]
    fn scheduler_fired_turn_completed_keeps_adopted_path() {
        // `/loop` turns are client-driven with a real finalize path, never the wake shortcut
        let mut app = make_app_with_agent("sess-cron");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-cron", "scheduler-fired-abc", Some(1_000)),
            &mut app,
        );

        assert!(!affected);
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            len_before,
            "a scheduler-fired terminal must not push a wake marker"
        );
    }

    #[test]
    fn silent_errored_wake_pushes_failure_marker() {
        // Failures are shown even when the wake is invisible: the standing instruction silently stopped
        let mut app = make_app_with_agent("sess-wake");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(agent.scrollback.len(), len_before + 1);
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
    }

    #[test]
    fn errored_wake_error_kind_renders_truncation_copy() {
        let mut app = make_app_with_agent("sess-wake");

        let _ = handle_ext_notification(
            &xai_turn_completed_failed_with_error_kind(
                "sess-wake",
                "task-completed-bg1",
                "turn ended early",
                "max_tokens_truncation",
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert_eq!(error, "Response truncated: turn ended early")
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn errored_wake_during_local_turn_error_kind_renders_truncation_copy() {
        // The busy-wake pierce arm reads the typed kind itself (it never reaches `finish_wake_turn`)
        use crate::app::agent::AgentState;

        let mut app = make_app_with_agent("sess-wake");
        app.agents.get_mut(&AgentId(0)).unwrap().session.state = AgentState::TurnRunning;

        let _ = handle_ext_notification(
            &xai_turn_completed_failed_with_error_kind(
                "sess-wake",
                "task-completed-bg1",
                "turn ended early",
                "max_tokens_truncation",
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert_eq!(error, "Response truncated: turn ended early")
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn rate_limited_wake_during_local_turn_keeps_rate_limit_copy() {
        // The busy-wake piercing path must pass rate-limit copy through untouched like `finish_wake_turn` does
        // The generic formatter would strip the upgrade URL and headline it "Request failed"
        let mut app = make_app_with_agent("sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
        }
        let rate_limit_copy = "You've hit the rate limit for your plan. Upgrade your \
                               subscription for higher limits: https://grok.com/supergrok";
        let payload = SessionNotification {
            session_id: acp::SessionId::new("sess-wake"),
            update: XaiSessionUpdate::TurnCompleted {
                prompt_id: "task-completed-bg1".into(),
                stop_reason: "rate_limit".into(),
                agent_result: Some(rate_limit_copy.into()),
                error_kind: None,
                usage: None,
                elapsed_ms: None,
            },
            meta: Some(serde_json::json!({ "isReplay": false })),
        };
        let notif = acp::ExtNotification::new(
            "x.ai/session/update",
            std::sync::Arc::from(serde_json::value::to_raw_value(&payload).unwrap()),
        );

        let _ = handle_ext_notification(&notif, &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert_eq!(error, rate_limit_copy, "copy must pass through untouched");
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn errored_wake_skips_marker_when_banner_already_on_screen() {
        // The retry-state rail already pushed the formatted RequestFailed banner for this failure
        // The wake rail must not add a second near-identical warning line (same dedupe as the local rails)
        let mut app = make_app_with_agent("sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::session_event(
                    SessionEvent::RequestFailed {
                        status: Some(400),
                        headline: "Bad request (400)".into(),
                        detail: "The server rejected this request.".into(),
                    },
                ));
        }
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "banner already covers the failure; no TurnFailed marker"
        );
        // The failure is still recorded, so the other wake rail stays quiet too.
        assert_eq!(
            agent.failed_wake_marker_for.as_deref(),
            Some("task-completed-bg1")
        );
    }

    /// Same dedupe on the busy-wake rail (a local turn is running, so the terminal takes the `is_busy` branch instead of `finish_wake_turn`).
    #[test]
    fn errored_wake_during_local_turn_skips_marker_when_banner_on_screen() {
        use crate::app::agent::AgentState;

        let mut app = make_app_with_agent("sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent
                .scrollback
                .push_block(crate::scrollback::block::RenderBlock::session_event(
                    SessionEvent::RequestFailed {
                        status: Some(500),
                        headline: "Server error (500)".into(),
                        detail: String::new(),
                    },
                ));
        }
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "banner already covers the failure; no TurnFailed marker"
        );
        assert_eq!(
            agent.failed_wake_marker_for.as_deref(),
            Some("task-completed-bg1")
        );
    }

    #[test]
    fn silent_errored_wake_ignores_stale_turn_start_ms() {
        // A silent wake streamed no deltas, so the stored `turn_start_ms` is an earlier turn's.
        let mut app = make_app_with_agent("sess-wake");
        app.agents.get_mut(&AgentId(0)).unwrap().turn_start_ms =
            Some(chrono::Utc::now().timestamp_millis() - 600_000);

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { elapsed: None, .. })
        ));
    }

    #[test]
    fn goal_terminal_snapshots_epoch_so_next_silent_wake_stays_markerless() {
        // A dirty output epoch made the NEXT silent wake inherit the goal turn's output.
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "goal-summary-g1", 5_000),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "goal-summary-g1", "end_turn", false),
            &mut app,
        );
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "end_turn", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "a silent wake after a goal turn must not inherit its output"
        );
        assert_eq!(count_turn_markers(agent), 0);
    }

    #[test]
    fn errored_wake_terminal_during_local_turn_still_pushes_failure() {
        // Failure visibility survives the busy skip: no tracker finish, no elapsed (the anchor is the local turn's), but the row must land
        use crate::app::agent::AgentState;

        let mut app = make_app_with_agent("sess-wake");
        app.agents.get_mut(&AgentId(0)).unwrap().session.state = AgentState::TurnRunning;
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        for _ in 0..2 {
            let _ = handle_ext_notification(
                &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
                &mut app,
            );
        }

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(agent.scrollback.len(), len_before + 1, "one row, deduped");
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { elapsed: None, .. })
        ));
    }

    #[test]
    fn wake_terminal_during_command_snapshots_epoch_for_next_silent_wake() {
        // A client command (e.g. /compact) skips the wake finish but must not leave the epoch dirty.
        // The next silent wake would claim the skipped wake's output
        use crate::app::agent::{AgentCommand, AgentState};
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        app.agents.get_mut(&AgentId(0)).unwrap().session.state = AgentState::CommandRunning {
            command: AgentCommand::Compact,
            started_at: std::time::Instant::now(),
        };
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "end_turn", false),
            &mut app,
        );
        app.agents.get_mut(&AgentId(0)).unwrap().session.state = AgentState::Idle;
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg2", "end_turn", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "silent wake after a command-skipped terminal must stay markerless"
        );
        assert_eq!(count_turn_markers(agent), 0);
    }

    #[test]
    fn chatty_wake_with_foreign_turn_start_anchor_omits_elapsed() {
        // `turn_start_ms` stamped by another prompt's deltas must not become this wake's elapsed
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 600_000),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg2", "end_turn", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { elapsed: None })
        ));
    }

    #[test]
    fn silent_errored_wake_after_goal_turn_has_no_elapsed() {
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "goal-summary-g1", 5_000),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "goal-summary-g1", "end_turn", false),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { elapsed: None, .. })
        ));
    }

    #[test]
    fn duplicate_errored_wake_terminal_pushes_one_failure_marker() {
        // Failures bypass the output-epoch dedupe, so duplicates are deduped by prompt id.
        let mut app = make_app_with_agent("sess-wake");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        for _ in 0..2 {
            let _ = handle_ext_notification(
                &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
                &mut app,
            );
        }

        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            len_before + 1,
            "one failure marker for the wake, duplicates dropped"
        );
    }

    #[test]
    fn silent_cancelled_or_rate_limited_wake_stays_markerless() {
        // Rate limits arrive through the retry notifications instead, matching the real-turn rails
        let mut app = make_app_with_agent("sess-wake");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        for stop_reason in ["cancelled", "rate_limit"] {
            let _ = handle_ext_notification(
                &xai_turn_completed_notif("sess-wake", "task-completed-bg1", stop_reason, false),
                &mut app,
            );
        }

        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len(),
            len_before,
            "cancelled/rate-limited silent wake terminals push nothing"
        );
    }

    #[test]
    fn chatty_send_now_cancelled_wake_is_markerless() {
        // A wake with output cancelled by send-now must stay silent, the same suppression the other three turn-end rails already apply
        use crate::app::agent_view::test_fixtures::count_turn_markers;

        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif_with_cancel_trigger(
                "sess-wake",
                "task-completed-bg1",
                "cancelled",
                "send_now",
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "a send-now cancelled chatty wake must push no marker"
        );
        assert_eq!(count_turn_markers(agent), 0);
        assert!(
            !matches!(
                last_session_event(&agent.scrollback),
                Some(SessionEvent::TurnCancelled { .. })
            ),
            "send_now must not surface as Turn cancelled by user"
        );
    }

    #[test]
    fn chatty_user_cancelled_wake_pushes_cancelled_marker() {
        // Genuine cancel (Ctrl+C / Esc, no wire trigger) still shows the marker.
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "cancelled", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCancelled { .. })
        ));
    }

    #[test]
    fn foreign_send_now_arm_does_not_suppress_wake_cancel_marker() {
        // A flag armed for a different (user) prompt must not eat this wake's genuine cancel marker, and must stay armed after close-out
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .expect_send_now_cancel = Some("user-prompt-other".into());

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "cancelled", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCancelled { .. })
        ));
        assert_eq!(
            agent.expect_send_now_cancel.as_deref(),
            Some("user-prompt-other"),
            "wake close-out must not clear a foreign send-now arm"
        );
    }

    #[test]
    fn chatty_rate_limited_wake_closes_with_failure_marker() {
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "rate_limit", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
    }

    #[test]
    fn chatty_errored_wake_pushes_failure_marker_not_worked_for() {
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "error", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
    }

    #[test]
    fn dead_wake_pushes_no_status_line() {
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks(&mut app, "sess-wake");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "cancelled", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "a dead wake must not push a work-only status line"
        );
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "a dead wake pushes nothing"
        );
        assert_eq!(
            agent.watchers().commands,
            2,
            "the still-running work feeds the status-row cue instead"
        );
    }

    #[test]
    fn wake_terminal_during_local_turn_pushes_nothing() {
        // FIFO can deliver a wake's terminal after a fresh local prompt starts; a foreign "Worked for" under that prompt would misattribute
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks(&mut app, "sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
        }
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", Some(6_000)),
            &mut app,
        );

        assert!(!affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "no marker and no status line may land under the fresh local prompt"
        );
        assert!(
            agent.session.state.is_turn_running(),
            "the local turn is untouched"
        );
    }

    #[test]
    fn between_turns_completion_pushes_chip_only() {
        let mut app = make_app_with_agent("sess-chip-only");
        seed_two_bg_tasks(&mut app, "sess-chip-only");
        assert!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).session.state.is_idle());
        assert_eq!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).watchers().commands, 2);

        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-chip-only", "task-1", "sleep 98", Some(0)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "no work-only status line after a between-turns completion"
        );
        assert_eq!(
            agent.watchers().commands,
            1,
            "the status-row cue counts down instead"
        );

        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-chip-only", "task-2", "sleep 99", Some(0)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(work_status_lines(&agent.scrollback).is_empty());
        assert_eq!(agent.watchers().commands, 0, "zero left — cue disappears");
    }

    #[test]
    fn mid_turn_completion_pushes_chip_only() {
        let mut app = make_app_with_agent("sess-midturn");
        seed_two_bg_tasks(&mut app, "sess-midturn");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("p1".into());
        }

        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-midturn", "task-1", "sleep 98", Some(0)),
            &mut app,
        );
        assert!(
            work_status_lines(&app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback).is_empty(),
            "a completion inside an active turn pushes its chip only"
        );
    }

    #[test]
    fn subagent_finished_between_turns_pushes_no_status_line() {
        let mut app = make_app_with_parent_and_child("sess-sub-quiet", "child-1");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("sess-sub-quiet", "tc-1", "task-1", "sleep 98"),
            &mut app,
        );

        let _ = handle(
            make_ext_session_notification("sess-sub-quiet", test_subagent_finished("child-1")),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "a finished subagent pushes no work-only status line"
        );
        assert_eq!(
            agent.watchers().commands,
            1,
            "the remaining bg command stays on the status-row cue"
        );
    }

    #[test]
    fn will_wake_flag_is_ignored_wire_compat_pin() {
        // `will_wake` is a wire-compat field the TUI no longer reads.
        let mut app = make_app_with_agent("sess-wake-skip");
        seed_two_bg_tasks(&mut app, "sess-wake-skip");

        let _ = handle_ext_notification(
            &task_completed_notif("sess-wake-skip", "task-1", "sleep 98", Some(0), None, true),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "a wake-bound completion pushes its chip only"
        );
    }

    #[test]
    fn child_session_completions_never_spam_root_status() {
        // A background subagent's own task traffic routes to the CHILD view
        // It never counts toward the root's watchers, so its completions must not push root status lines
        let mut app = make_app_with_parent_and_child("sess-child-quiet", "child-1");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("child-1", "tc-c1", "task-c1", "sleep 97"),
            &mut app,
        );
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("child-1", "tc-c2", "task-c2", "sleep 98"),
            &mut app,
        );
        assert!(app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).session.state.is_idle());

        let _ = handle_ext_notification(
            &make_task_completed_notif("child-1", "task-c1", "sleep 97", Some(0)),
            &mut app,
        );
        let _ = handle_ext_notification(
            &make_task_completed_notif("child-1", "task-c2", "sleep 98", Some(0)),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "child-session completions must not spawn root status lines"
        );
        let child = agent.subagent_views.get("child-1").unwrap();
        assert!(
            work_status_lines(&child.scrollback).is_empty(),
            "and none in the child view either (chips only)"
        );

        // Nested analogue: a SubagentFinished carrying a CHILD session id routes to the child handler, which has no status site
        let _ = handle(
            make_ext_session_notification("child-1", test_subagent_finished("grandchild-1")),
            &mut app,
        );
        assert!(
            work_status_lines(&app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback).is_empty(),
            "nested subagent traffic must not spawn root status lines"
        );
    }

    /// The core reattach-finalization: a `TurnCompleted` seen during a load's replay window records its prompt id.
    /// The post-replay `SessionLoaded` adoption then SKIPS that same id.
    /// A viewer that re-attached after the turn ended does not re-strand on "Waiting…".
    #[test]
    fn replayed_turn_completed_blocks_session_loaded_adoption() {
        use crate::app::dispatch::dispatch;
        use crate::app::actions::{Action, TaskResult};

        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.loading_replay = true;

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-1", "p-run", "end_turn", true),
            &mut app,
        );
        assert!(
            !affected,
            "a replayed terminal records adoption state, not a redraw"
        );
        assert!(
            app.agents.get(&id).unwrap_or_else(|| panic!("missing map entry")).replayed_terminal_prompts.contains("p-run"),
            "a replayed TurnCompleted must record its prompt id"
        );

        dispatch(
            Action::TaskComplete(TaskResult::SessionLoaded {
                agent_id: id,
                session_id: acp::SessionId::new("sess-1"),
                models: None,
                modes: None,
                code_restored: false,
                restore_summary: None,
                restore_degree: None,
                running_prompt_id: Some("p-run".to_string()),
            }),
            &mut app,
        );

        let agent = &app.agents.get(&id).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.session.current_prompt_id.is_none(),
            "a terminal-in-replay prompt must NOT be adopted on load"
        );
        assert!(
            agent.session.state.is_idle(),
            "adopting an already-ended turn would re-strand the viewer on Waiting…"
        );
    }

    /// Arming the lost-RPC reconcile from a live `TurnCompleted` must STILL report a change.
    /// Otherwise `event_loop` skips `schedule_tick` and `reconcile_overdue_turn_ends` never fires, stranding the turn on "Waiting…".
    /// The reconcile-arm return must NOT be gated on `is_active`.
    #[test]
    fn background_driver_live_turn_completed_arms_reconcile_and_reports_change() {
        let mut app = make_app_with_agent("sess-bg");
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-bg".into());
            assert!(!agent.attached_as_viewer);
        }
        // Make the driver a background tab: the active view is elsewhere.
        app.active_view = ActiveView::Welcome;
        assert!(!is_matched_agent_active(&app, id));

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-bg", "pid-bg", "cancelled", false),
            &mut app,
        );
        assert!(
            affected,
            "a background driver's reconcile-arm must report a change so the tick is scheduled"
        );
        let agent = app.agents.get(&id).unwrap();
        assert!(
            agent.pending_turn_end_reconcile.is_some(),
            "the lost-RPC reconcile must be armed"
        );
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "arming must NOT finish the driver's turn"
        );
    }

    /// The replay set never leaks across loads.
    /// A second load enters a fresh replay window via `begin_replay_window`.
    /// That resets ALL coupled fields (the terminal set AND `unexpected_replay_drops`) together.
    #[test]
    fn second_load_does_not_inherit_first_loads_replay_window_state() {
        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        // First load replay records a terminal; also seed a prior stray-replay drop count so the reset of every coupled field is observable
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.loading_replay = true;
            agent.unexpected_replay_drops = 3;
        }
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-1", "p-first", "end_turn", true),
            &mut app,
        );
        assert!(
            app.agents.get(&id).unwrap_or_else(|| panic!("missing map entry"))
                .replayed_terminal_prompts
                .contains("p-first")
        );

        // A second load (reconnect) enters a fresh replay window
        // An armed cancel resend belongs to the pre-reload turn and must drop with it
        app.agents.get_mut(&id).unwrap().pending_cancel_resend =
            Some(crate::app::agent_view::PendingCancelResend {
                prompt_id: Some("p-first".into()),
                sent_at: std::time::Instant::now(),
                attempts: 1,
                confirmed: false,
                cancel_subagents: true,
                trigger: crate::app::actions::CancelTrigger::DashboardStop,
            });
        app.agents.get_mut(&id).unwrap().begin_session_reload(1);
        let agent = &app.agents.get(&id).unwrap_or_else(|| panic!("missing map entry"));
        assert!(
            agent.replayed_terminal_prompts.is_empty(),
            "the second load must not inherit the first load's terminal set"
        );
        assert_eq!(
            agent.unexpected_replay_drops, 0,
            "begin_replay_window must reset every replay-coupled field together"
        );
        assert!(
            agent.pending_cancel_resend.is_none(),
            "an armed cancel resend must not survive into the reload window"
        );
        assert!(agent.session.loading_replay);
    }


    fn begin_replay(app: &mut AppView) {
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;
    }

    #[test]
    fn replay_completed_with_elapsed_pushes_worked_for() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "end_turn",
                Some(2500),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnCompleted { elapsed: Some(d) }) => {
                assert_eq!(d, std::time::Duration::from_millis(2500));
            }
            other => panic!("expected Worked-for marker, got {other:?}"),
        }
        assert!(agent.turn_started_at.is_none());
        assert!(agent.expect_send_now_cancel.is_none());
    }

    #[test]
    fn replay_completed_without_elapsed_is_turn_completed_not_zero() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-1", "p1", "end_turn", true),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(ev @ SessionEvent::TurnCompleted { elapsed: None }) => {
                assert_eq!(ev.message(), "Turn completed.");
            }
            other => panic!("expected markerless-elapsed completed, got {other:?}"),
        }
    }

    #[test]
    fn replay_cancelled_pushes_cancelled_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "cancelled",
                Some(800),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCancelled {
                cause: crate::scrollback::blocks::CancelledBy::Unspecified,
                ..
            })
        ));
    }

    #[test]
    fn replay_session_close_names_session_closed() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "cancelled",
                Some(800),
                None,
                serde_json::json!({ "cancelTrigger": "session_close" }),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(ev @ SessionEvent::TurnCancelled { .. }) => {
                assert_eq!(
                    ev.message(),
                    "Turn cancelled because the session closed in 0.8s."
                );
            }
            other => panic!("expected named session-close cancel, got {other:?}"),
        }
    }

    #[test]
    fn replay_hook_denied_pushes_blocked_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "cancelled",
                Some(400),
                None,
                serde_json::json!({ "cancellationCategory": "HookDenied" }),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnBlockedByHook { .. })
        ));
    }

    #[test]
    fn replay_failed_without_banner_pushes_failed_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "error",
                Some(100),
                Some("boom"),
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
    }

    #[test]
    fn replay_failed_error_kind_renders_truncation_copy() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_failed_with_error_kind(
                "sess-1",
                "p1",
                "turn ended early",
                "max_tokens_truncation",
                true,
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        match last_session_event(&agent.scrollback) {
            Some(SessionEvent::TurnFailed { error, .. }) => {
                assert_eq!(error, "Response truncated: turn ended early")
            }
            other => panic!("expected TurnFailed, got {other:?}"),
        }
    }

    #[test]
    fn replay_failed_with_banner_skips_failed_marker() {
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.scrollback.push_block(
                crate::scrollback::block::RenderBlock::session_event(SessionEvent::RequestFailed {
                    status: Some(400),
                    headline: "Bad request (400)".into(),
                    detail: "The server rejected this request.".into(),
                }),
            );
        }
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "error",
                Some(100),
                Some("boom"),
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_rate_limit_records_pid_no_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "rate_limit",
                Some(50),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_chatty_rate_limited_wake_paints_failure_marker() {
        // Live `finish_wake_turn` paints TurnFailed for a chatty rate-limited wake; replay must not drop that footer
        let mut app = make_app_with_agent("sess-wake");
        begin_replay(&mut app);
        let _ = handle(
            make_replay_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-wake",
                "task-completed-bg1",
                "rate_limit",
                Some(50),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("task-completed-bg1"));
        assert_eq!(agent.scrollback.len(), len_before + 1);
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
        assert_eq!(
            agent.failed_wake_marker_for.as_deref(),
            Some("task-completed-bg1")
        );
    }

    #[test]
    fn replay_silent_rate_limited_wake_stays_markerless() {
        let mut app = make_app_with_agent("sess-wake");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "rate_limit", true),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("task-completed-bg1"));
        assert_eq!(agent.scrollback.len(), len_before);
        assert!(agent.failed_wake_marker_for.is_none());
    }

    #[test]
    fn replay_send_now_records_pid_no_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "cancelled",
                Some(50),
                None,
                serde_json::json!({ "cancelTrigger": "send_now" }),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_unknown_stop_reason_pushes_completed_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "brand_new_token",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn replay_duplicate_pid_one_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let n = xai_turn_completed_replay(
            "sess-1",
            "p1",
            "end_turn",
            Some(10),
            None,
            serde_json::json!({}),
        );
        let _ = handle_ext_notification(&n, &mut app);
        let _ = handle_ext_notification(&n, &mut app);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            crate::app::agent_view::test_fixtures::count_turn_markers(agent),
            1
        );
    }

    #[test]
    fn replay_two_pids_two_markers() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p1",
                "end_turn",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p2",
                "end_turn",
                Some(20),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p1"));
        assert!(agent.replayed_terminal_prompts.contains("p2"));
        assert_eq!(
            crate::app::agent_view::test_fixtures::count_turn_markers(agent),
            2
        );
    }

    #[test]
    fn replay_silent_wake_records_pid_no_marker() {
        let mut app = make_app_with_agent("sess-wake");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "end_turn", true),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("task-completed-bg1"));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_wake_suppressed_tool_only_records_pid_no_marker() {
        let mut app = make_app_with_agent("sess-wake");
        begin_replay(&mut app);
        send_replay_suppressed_tool_call(&mut app, "sess-wake", "task-completed-bg1");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-wake",
                "task-completed-bg1",
                "end_turn",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("task-completed-bg1"));
        assert!(
            !agent.replayed_visible_prompts.contains("task-completed-bg1"),
            "a suppressed TodoWrite must not count as visible output"
        );
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_goal_summary_error_stays_markerless() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "goal-summary-g1",
                "error",
                Some(10),
                Some("classifier failed"),
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("goal-summary-g1"));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    #[test]
    fn replay_direct_bash_records_pid_no_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        send_replay_bash_tool_call(&mut app, "sess-1", "p-bash");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p-bash",
                "end_turn",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p-bash"));
        assert!(agent.replayed_bash_prompts.contains("p-bash"));
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "direct bash must not paint a Worked-for marker"
        );
    }

    #[test]
    fn replay_direct_bash_cancelled_paints_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        send_replay_bash_tool_call(&mut app, "sess-1", "p-bash");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p-bash",
                "cancelled",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p-bash"));
        assert!(agent.replayed_bash_prompts.contains("p-bash"));
        assert_eq!(
            agent.scrollback.len(),
            len_before + 1,
            "cancelled direct bash must paint TurnCancelled"
        );
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCancelled { .. })
        ));
    }

    #[test]
    fn replay_direct_bash_error_paints_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        send_replay_bash_tool_call(&mut app, "sess-1", "p-bash");
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "p-bash",
                "error",
                Some(10),
                Some("boom"),
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains("p-bash"));
        assert!(agent.replayed_bash_prompts.contains("p-bash"));
        assert_eq!(
            agent.scrollback.len(),
            len_before + 1,
            "failed direct bash must paint TurnFailed"
        );
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnFailed { .. })
        ));
    }

    #[test]
    fn replay_empty_prompt_id_records_no_marker() {
        let mut app = make_app_with_agent("sess-1");
        begin_replay(&mut app);
        let len_before = app.agents.get(&AgentId(0)).unwrap_or_else(|| panic!("missing map entry")).scrollback.len();
        let _ = handle_ext_notification(
            &xai_turn_completed_replay(
                "sess-1",
                "",
                "end_turn",
                Some(10),
                None,
                serde_json::json!({}),
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.replayed_terminal_prompts.contains(""));
        assert_eq!(agent.scrollback.len(), len_before);
    }

    /// Builds a live `LastTurnSummary` notification.
    fn xai_last_turn_summary_notif(
        session_id: &str,
        summary: &str,
        prompt_id: Option<&str>,
    ) -> acp::ExtNotification {
        let payload = SessionNotification {
            session_id: acp::SessionId::new(session_id),
            update: XaiSessionUpdate::LastTurnSummary {
                summary: summary.into(),
                prompt_id: prompt_id.map(String::from),
            },
            meta: None,
        };
        acp::ExtNotification::new(
            "x.ai/session/update",
            std::sync::Arc::from(serde_json::value::to_raw_value(&payload).unwrap()),
        )
    }

    /// Show-until-replaced: a summary stays on the row across a later cancelled turn (the shell generates none for it).
    /// It survives turn start/finish untouched and is replaced by the next delivery.
    /// Viewer-mode, mirroring `live_turn_completed_finalizes_viewer_turn`.
    #[test]
    fn last_turn_summary_shows_until_replaced() {
        let mut app = make_app_with_agent("sess-lts");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;

        // Turn A runs, completes, and its summary arrives.
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-lts", "chunk", "pid-a", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-lts", "pid-a", "end_turn", false),
            &mut app,
        );
        let affected = handle_ext_notification(
            &xai_last_turn_summary_notif("sess-lts", "Did the thing", Some("pid-a")),
            &mut app,
        );
        assert!(affected);
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap().last_turn_summary.as_deref(),
            Some("Did the thing")
        );

        // Turn B runs and is cancelled (no replacement summary): A's summary stays; the row keeps showing the last successful turn's work
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-lts", "chunk", "pid-b", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-lts", "pid-b", "cancelled", false),
            &mut app,
        );
        assert!(app.agents.get(&AgentId(0)).unwrap().session.state.is_idle());
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap().last_turn_summary.as_deref(),
            Some("Did the thing"),
            "a cancelled turn must not blank the previous summary"
        );

        // Turn C succeeds; its summary replaces A's.
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-lts", "chunk", "pid-c", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-lts", "pid-c", "end_turn", false),
            &mut app,
        );
        let affected = handle_ext_notification(
            &xai_last_turn_summary_notif("sess-lts", "Did the next thing", Some("pid-c")),
            &mut app,
        );
        assert!(affected);
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap().last_turn_summary.as_deref(),
            Some("Did the next thing")
        );
    }
