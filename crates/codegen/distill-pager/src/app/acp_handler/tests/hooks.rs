// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    //! Hook notifications: success leaves no trace, a failed run gets one bulleted `HookOutcome` line, a deny gets none here (the shell's annotation carries it).
    use super::*;
    use crate::acp::tracker::WaitingReason;
    use distill_shell::extensions::notification::{HookRunEntryDto, HookRunStatusDto};

    fn xai_hook_run_started_notif(session_id: &str, event_name: &str, count: usize) -> acp::ExtNotification {
        xai_hook_run_started_notif_for(session_id, event_name, count, None, None)
    }

    fn xai_hook_run_started_notif_for_prompt(
        session_id: &str,
        event_name: &str,
        count: usize,
        prompt_id: Option<&str>,
    ) -> acp::ExtNotification {
        xai_hook_run_started_notif_for(session_id, event_name, count, None, prompt_id)
    }

    fn xai_hook_run_started_notif_for(
        session_id: &str,
        event_name: &str,
        count: usize,
        tool_name: Option<&str>,
        prompt_id: Option<&str>,
    ) -> acp::ExtNotification {
        let payload = SessionNotification {
            session_id: acp::SessionId::new(session_id),
            update: XaiSessionUpdate::HookRunStarted {
                event_name: event_name.into(),
                tool_name: tool_name.map(str::to_string),
                prompt_id: prompt_id.map(str::to_string),
                count,
            },
            meta: None,
        };
        acp::ExtNotification::new(
            "x.ai/session/update",
            serde_json::value::to_raw_value(&payload).unwrap().into(),
        )
    }

    /// A successful outcome for a tool batch; `xai_hook_execution_notif_with_runs` sends no tool name.
    fn xai_hook_execution_notif_for_tool(session_id: &str, event_name: &str, tool_name: &str) -> acp::ExtNotification {
        let payload = SessionNotification {
            session_id: acp::SessionId::new(session_id),
            update: XaiSessionUpdate::HookExecution {
                event_name: event_name.into(),
                tool_name: Some(tool_name.into()),
                prompt_id: None,
                runs: vec![run("global/notify", HookRunStatusDto::Success { elapsed_ms: 12 })],
            },
            meta: Some(serde_json::json!({ "isReplay": false })),
        };
        acp::ExtNotification::new(
            "x.ai/session/update",
            serde_json::value::to_raw_value(&payload).unwrap().into(),
        )
    }

    fn run(name: &str, status: HookRunStatusDto) -> HookRunEntryDto {
        HookRunEntryDto { name: name.into(), status, output: None }
    }

    /// Every failed-hook line; a plain `HookAnnotation` here would mean the line lost its tool-row bullet.
    fn annotation_lines(sb: &ScrollbackState) -> Vec<String> {
        (0..sb.len())
            .filter_map(|i| match sb.get(i).map(|e| &e.block) {
                Some(RenderBlock::SessionEvent(b)) => match &b.event {
                    SessionEvent::HookOutcome { message } => Some(message.clone()),
                    SessionEvent::HookAnnotation { message } => panic!("failed-hook line pushed without the tool bullet: {message}"),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    #[test]
    fn successful_batch_leaves_no_scrollback_trace() {
        let mut app = make_app_with_agent("sess-hooks");
        let len_before = test_agent(&app, AgentId(0)).scrollback.len();
        for event in ["pre_tool_use", "post_tool_use", "user_prompt_submit", "stop", "session_start"] {
            let affected = handle_ext_notification(
                &xai_hook_execution_notif_with_runs(
                    "sess-hooks",
                    event,
                    Some("p1"),
                    false,
                    vec![
                        run("global/lint", HookRunStatusDto::Success { elapsed_ms: 12 }),
                        run("global/off", HookRunStatusDto::Skipped),
                    ],
                ),
                &mut app,
            );
            assert!(!affected, "{event}: nothing changed on screen, so no redraw");
        }
        assert_eq!(
            test_agent(&app, AgentId(0)).scrollback.len(),
            len_before,
            "successful and skipped runs must not push any block"
        );
    }

    #[test]
    fn failed_run_gets_one_line_and_a_tier_source_names_only_the_event() {
        let mut app = make_app_with_agent("sess-hooks");
        let affected = handle_ext_notification(
            &xai_hook_execution_notif_with_runs(
                "sess-hooks",
                "pre_tool_use",
                Some("p1"),
                false,
                vec![
                    run("global/lint", HookRunStatusDto::Success { elapsed_ms: 3 }),
                    run(
                        "requirements/system:pre_tool_use[0].hooks[0]",
                        HookRunStatusDto::Failed {
                            error: "timed out after 1000ms\nsecond line".into(),
                            elapsed_ms: 1000,
                            blocked: false,
                        },
                    ),
                ],
            ),
            &mut app,
        );
        assert!(affected);
        assert_eq!(
            annotation_lines(&test_agent(&app, AgentId(0)).scrollback),
            vec!["pre_tool_use hook failed, ignored: timed out after 1000ms".to_string()],
            "one line per failed run, first error line only; the config spec path never reaches the user"
        );
    }

    #[test]
    fn named_hook_failure_carries_the_name_and_the_runner_error() {
        let mut app = make_app_with_agent("sess-hooks");
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_with_runs(
                "sess-hooks",
                "post_tool_use",
                Some("p1"),
                false,
                vec![run(
                    "global/qa:post_tool_use[1].hooks[0]",
                    HookRunStatusDto::Failed {
                        error: "exit code 1: lint: 3 errors".into(),
                        elapsed_ms: 40,
                        blocked: false,
                    },
                )],
            ),
            &mut app,
        );
        assert_eq!(
            annotation_lines(&test_agent(&app, AgentId(0)).scrollback),
            vec!["post_tool_use hook (global/qa) failed, ignored: exit code 1: lint: 3 errors".to_string()],
        );
    }

    #[test]
    fn blocked_run_is_not_reported_twice() {
        // The shell annotates every deny with its reason; the batch outcome must not add a second line
        let mut app = make_app_with_agent("sess-hooks");
        let len_before = test_agent(&app, AgentId(0)).scrollback.len();
        let affected = handle_ext_notification(
            &xai_hook_execution_notif_with_runs(
                "sess-hooks",
                "pre_tool_use",
                Some("p1"),
                false,
                vec![run(
                    "global/policy",
                    HookRunStatusDto::Failed {
                        error: "rm is not allowed".into(),
                        elapsed_ms: 5,
                        blocked: true,
                    },
                )],
            ),
            &mut app,
        );
        assert!(!affected);
        assert_eq!(test_agent(&app, AgentId(0)).scrollback.len(), len_before);
    }

    /// The phase ends only on the outcome whose `(event, tool)` identity armed it.
    #[test]
    fn run_started_arms_spinner_phase_and_execution_ends_it() {
        let mut app = make_app_with_agent("sess-hooks");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
        }
        let affected = handle_ext_notification(
            &xai_hook_run_started_notif_for("sess-hooks", "pre_tool_use", 2, Some("read_file"), None),
            &mut app,
        );
        assert!(!affected, "arming the phase draws nothing until the reveal delay passes");
        let gate = TurnActivity::Waiting(WaitingReason::Hooks { event_name: "pre_tool_use".into(), count: 2 });
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.tracker.backdate_hooks_running(crate::acp::tracker::HOOK_REVEAL_DELAY);
            assert_eq!(agent.session.tracker.activity(), Some(gate.clone()));
        }
        let affected = handle_ext_notification(&xai_hook_execution_notif_for_tool("sess-hooks", "pre_tool_use", "grep"), &mut app);
        assert!(!affected);
        assert_eq!(
            test_agent(&app, AgentId(0)).session.tracker.activity(),
            Some(gate),
            "another tool's pre_tool_use outcome must not end this gate"
        );
        let affected = handle_ext_notification(&xai_hook_execution_notif_for_tool("sess-hooks", "pre_tool_use", "read_file"), &mut app);
        assert!(affected, "ending a revealed phase redraws the spinner");
        let agent = test_agent(&app, AgentId(0));
        assert!(
            !matches!(agent.session.tracker.activity(), Some(TurnActivity::Waiting(WaitingReason::Hooks { .. }))),
            "the batch outcome ends the phase"
        );
        assert!(annotation_lines(&agent.scrollback).is_empty(), "a successful batch still leaves no line");
    }

    /// A `session_start` outcome finishing during a prompt gate names another batch and must not end the gate.
    #[test]
    fn another_batch_outcome_leaves_the_current_phase_alone() {
        let mut app = make_app_with_agent("sess-hooks");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.tracker.set_hooks_running_since(
                crate::acp::tracker::HookBatchId { event_name: "user_prompt_submit".into(), tool_name: None },
                1,
                std::time::Instant::now() - crate::acp::tracker::HOOK_REVEAL_DELAY,
                None,
            );
        }
        let gate = TurnActivity::Waiting(WaitingReason::Hooks { event_name: "user_prompt_submit".into(), count: 1 });

        let affected = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-hooks", "session_start", None, false),
            &mut app,
        );
        assert!(!affected);
        assert_eq!(
            test_agent(&app, AgentId(0)).session.tracker.activity(),
            Some(gate),
            "a session_start outcome says nothing about the prompt gate"
        );

        let affected = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-hooks", "user_prompt_submit", None, false),
            &mut app,
        );
        assert!(affected, "the gate's own outcome ends it");
        assert!(!matches!(
            test_agent(&app, AgentId(0)).session.tracker.activity(),
            Some(TurnActivity::Waiting(WaitingReason::Hooks { .. }))
        ));
    }

    #[test]
    fn foreign_turn_batch_leaves_the_current_phase_alone() {
        // A cancelled turn's `stop_cancelled` report can land after the next queued prompt started; it must not touch that gate
        let mut app = make_app_with_agent("sess-hooks");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("p-new".into());
        }
        let _ = handle_ext_notification(
            &xai_hook_run_started_notif_for_prompt("sess-hooks", "user_prompt_submit", 1, Some("p-new")),
            &mut app,
        );
        let own_phase = TurnActivity::Waiting(WaitingReason::Hooks { event_name: "user_prompt_submit".into(), count: 1 });
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.tracker.backdate_hooks_running(crate::acp::tracker::HOOK_REVEAL_DELAY);
            assert_eq!(agent.session.tracker.activity(), Some(own_phase.clone()));
        }

        let affected = handle_ext_notification(
            &xai_hook_run_started_notif_for_prompt("sess-hooks", "stop_cancelled", 1, Some("p-old")),
            &mut app,
        );
        assert!(!affected);
        assert_eq!(
            test_agent(&app, AgentId(0)).session.tracker.activity(),
            Some(own_phase.clone()),
            "the foreign start must not steal the label or restart the reveal delay"
        );

        let affected = handle_ext_notification(
            &xai_hook_execution_notif_with_runs(
                "sess-hooks",
                "stop_cancelled",
                Some("p-old"),
                false,
                vec![run("global/qa", HookRunStatusDto::Failed { error: "boom".into(), elapsed_ms: 1, blocked: false })],
            ),
            &mut app,
        );
        assert!(affected, "the foreign batch's failure line still renders");
        let agent = test_agent(&app, AgentId(0));
        assert_eq!(agent.session.tracker.activity(), Some(own_phase), "the foreign outcome must not end the current gate");
        assert_eq!(
            annotation_lines(&agent.scrollback),
            vec!["stop_cancelled hook (global/qa) failed, ignored: boom".to_string()]
        );

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-hooks", "user_prompt_submit", Some("p-new"), false),
            &mut app,
        );
        assert!(
            !matches!(test_agent(&app, AgentId(0)).session.tracker.activity(), Some(TurnActivity::Waiting(WaitingReason::Hooks { .. }))),
            "the gate's own outcome ends it"
        );
    }

    #[test]
    fn hook_notifications_are_inert_when_plugins_are_disabled() {
        let mut app = make_app_with_agent("sess-hooks");
        app.appearance.disable_plugins = true;
        let len_before = test_agent(&app, AgentId(0)).scrollback.len();
        let _ = handle_ext_notification(&xai_hook_run_started_notif("sess-hooks", "pre_tool_use", 1), &mut app);
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_with_runs(
                "sess-hooks",
                "pre_tool_use",
                Some("p1"),
                false,
                vec![run("global/lint", HookRunStatusDto::Failed { error: "boom".into(), elapsed_ms: 1, blocked: false })],
            ),
            &mut app,
        );
        let agent = test_agent(&app, AgentId(0));
        assert_eq!(agent.scrollback.len(), len_before);
        assert!(!matches!(agent.session.tracker.activity(), Some(TurnActivity::Waiting(WaitingReason::Hooks { .. }))));
    }
