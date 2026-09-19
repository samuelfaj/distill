// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn follow_ups_render_chips_on_active_agent() {
        let mut app = make_app_with_agent("sess-1");
        let affected = handle_ext_notification(
            &follow_ups_ext("resp-1", &["Tell me more", "Summarize"]),
            &mut app,
        );
        assert!(affected, "fresh chips on the active agent warrant a redraw");
        let fu = test_agent(&app, AgentId(0))
            .follow_ups
            .as_ref()
            .expect("chips set on the active agent");
        assert_eq!(fu.response_id, "resp-1");
        assert_eq!(fu.suggestions, vec!["Tell me more", "Summarize"]);
    }

    /// End-to-end through the wire: the stamped `promptId` flows from the notification params into the dedup.
    /// (a) a re-delivery of the active turn's follow_ups re-renders after a clear; (b) a prior turn's replay is rejected.
    #[test]
    fn follow_ups_prompt_id_makes_dedup_deterministic() {
        let mut app = make_app_with_agent("sess-1");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .current_prompt_id = Some("p1".into());

        // Apply the active turn's (p1) chips via the wire
        assert!(handle_ext_notification(
            &follow_ups_ext_with_prompt("resp-1", "p1", &["a"]),
            &mut app
        ));
        // Clear at the turn boundary (keeps the seen ring)
        app.agents.get_mut(&AgentId(0)).unwrap().clear_follow_ups();
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());

        // (a) Re-delivery of the active turn re-renders.
        assert!(
            handle_ext_notification(
                &follow_ups_ext_with_prompt("resp-1", "p1", &["a"]),
                &mut app
            ),
            "active-turn re-delivery must re-render via promptId match"
        );
        assert_eq!(
            test_agent(&app, AgentId(0))
                .follow_ups
                .as_ref()
                .unwrap()
                .response_id,
            "resp-1"
        );

        // Adopt a new turn p2, then clear the chips
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .current_prompt_id = Some("p2".into());
        app.agents.get_mut(&AgentId(0)).unwrap().clear_follow_ups();

        // (b) A prior turn (p1) replay must NOT revive the chips
        assert!(
            !handle_ext_notification(
                &follow_ups_ext_with_prompt("resp-1", "p1", &["a"]),
                &mut app
            ),
            "prior-turn replay must be rejected"
        );
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_replayed_meta_suppresses_chips() {
        let mut app = make_app_with_agent("sess-1");
        let params = serde_json::json!({
            "response_id": "resp-1",
            "suggestions": [{ "label": "x" }],
            "_meta": { "x.ai/replayed": true },
        });
        let notif = acp::ExtNotification::new(
            "x.ai/follow_ups",
            serde_json::value::to_raw_value(&params).unwrap().into(),
        );
        let affected = handle_ext_notification(&notif, &mut app);
        assert!(!affected, "a replayed chunk must not render chips");
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_malformed_params_are_ignored() {
        let mut app = make_app_with_agent("sess-1");
        let bad = [
            serde_json::Value::String("not an object".into()),
            serde_json::json!([1, 2, 3]),
            serde_json::json!({ "suggestions": 7 }),
            serde_json::json!({}),
        ];
        for params in bad {
            let notif = acp::ExtNotification::new(
                "x.ai/follow_ups",
                serde_json::value::to_raw_value(&params).unwrap().into(),
            );
            let affected = handle_ext_notification(&notif, &mut app);
            assert!(!affected, "malformed params must be ignored: {params}");
        }
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_sanitizes_control_characters() {
        let mut app = make_app_with_agent("sess-1");
        // A label carrying an ESC-based SGR sequence and a newline: control characters are stripped so a chip cannot inject terminal escapes
        handle_ext_notification(
            &follow_ups_ext("resp-1", &["safe\u{1b}[31mred\nmore"]),
            &mut app,
        );
        let fu = test_agent(&app, AgentId(0)).follow_ups.as_ref().unwrap();
        assert_eq!(fu.suggestions, vec!["safe[31mredmore"]);
        let Some(first) = fu.suggestions.first() else {
            panic!("expected a suggestion");
        };
        assert!(!first.contains('\u{1b}'));
        assert!(!first.contains('\n'));
    }

    #[test]
    fn follow_ups_empty_response_id_is_ignored() {
        let mut app = make_app_with_agent("sess-1");
        let affected = handle_ext_notification(&follow_ups_ext("", &["x"]), &mut app);
        assert!(
            !affected,
            "without a response_id there is no newest-wins key"
        );
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_blank_labels_yield_no_chips() {
        let mut app = make_app_with_agent("sess-1");
        let affected = handle_ext_notification(&follow_ups_ext("resp-1", &["   ", ""]), &mut app);
        assert!(!affected);
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_strips_bidi_and_zero_width() {
        let mut app = make_app_with_agent("sess-1");
        // U+202E RIGHT-TO-LEFT OVERRIDE and U+200B ZERO WIDTH SPACE are stripped
        // Stripping them stops server text visually disguising a leading `/` (Trojan Source)
        handle_ext_notification(
            &follow_ups_ext("resp-1", &["\u{202e}/rm\u{200b}-rf"]),
            &mut app,
        );
        let fu = test_agent(&app, AgentId(0)).follow_ups.as_ref().unwrap();
        assert_eq!(fu.suggestions, vec!["/rm-rf"]);
        let Some(first) = fu.suggestions.first() else {
            panic!("expected a suggestion");
        };
        assert!(!first.contains('\u{202e}'));
        assert!(!first.contains('\u{200b}'));
    }

    #[test]
    fn follow_ups_caps_count_and_label_length() {
        let mut app = make_app_with_agent("sess-1");
        let labels: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        handle_ext_notification(&follow_ups_ext("resp-1", &refs), &mut app);
        assert_eq!(
            test_agent(&app, AgentId(0))
                .follow_ups
                .as_ref()
                .unwrap()
                .suggestions
                .len(),
            super::MAX_FOLLOW_UPS,
            "suggestion count capped at ingestion"
        );
        let long = "x".repeat(10_000);
        handle_ext_notification(&follow_ups_ext("resp-2", &[&long]), &mut app);
        let Some(label) = test_agent(&app, AgentId(0))
            .follow_ups
            .as_ref()
            .and_then(|fu| fu.suggestions.first())
        else {
            panic!("expected a follow-up suggestion");
        };
        assert!(
            label.len() <= super::MAX_FOLLOW_UP_LABEL,
            "label length clamped"
        );
    }

    #[test]
    fn follow_ups_oversized_response_id_is_rejected() {
        // An oversized response_id is rejected rather than truncated (truncation could collide ids), so it can't bloat the retained seen ring
        let mut app = make_app_with_agent("sess-1");
        let big = "r".repeat(super::MAX_RESPONSE_ID_LEN + 1);
        let affected = handle_ext_notification(&follow_ups_ext(&big, &["x"]), &mut app);
        assert!(!affected, "an oversized response_id must be rejected");
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
        // A sane-length id still works.
        let ok = "r".repeat(super::MAX_RESPONSE_ID_LEN);
        assert!(handle_ext_notification(
            &follow_ups_ext(&ok, &["x"]),
            &mut app
        ));
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_some());
    }

    #[test]
    fn follow_ups_replayed_meta_false_renders() {
        let mut app = make_app_with_agent("sess-1");
        let params = serde_json::json!({
            "response_id": "resp-1",
            "suggestions": [{ "label": "x" }],
            "_meta": { "x.ai/replayed": false },
        });
        let notif = acp::ExtNotification::new(
            "x.ai/follow_ups",
            serde_json::value::to_raw_value(&params).unwrap().into(),
        );
        assert!(
            handle_ext_notification(&notif, &mut app),
            "_meta replayed=false must still render"
        );
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_some());
    }

    #[test]
    fn follow_ups_per_element_malformed_is_ignored() {
        let mut app = make_app_with_agent("sess-1");
        for bad in [
            serde_json::json!({ "response_id": "r", "suggestions": [{ "label": 7 }] }),
            serde_json::json!({ "response_id": "r", "suggestions": [null] }),
        ] {
            let notif = acp::ExtNotification::new(
                "x.ai/follow_ups",
                serde_json::value::to_raw_value(&bad).unwrap().into(),
            );
            assert!(
                !handle_ext_notification(&notif, &mut app),
                "a malformed suggestion element drops the notification: {bad}"
            );
        }
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_empty_array_renders_no_chips() {
        let mut app = make_app_with_agent("sess-1");
        let affected = handle_ext_notification(&follow_ups_ext("resp-1", &[]), &mut app);
        assert!(!affected);
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_empty_for_current_response_clears_chips() {
        let mut app = make_app_with_agent("sess-1");
        handle_ext_notification(&follow_ups_ext("resp-1", &["a"]), &mut app);
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_some());
        let affected = handle_ext_notification(&follow_ups_ext("resp-1", &[]), &mut app);
        assert!(affected, "empty for the shown response clears the chips");
        assert!(test_agent(&app, AgentId(0)).follow_ups.is_none());
    }

    #[test]
    fn follow_ups_viewer_turn_transition_renders_newer_chips() {
        // A viewer holding resp-1's chips adopts the driver's NEXT turn via a live delta, which clears the prior chips
        // resp-2's follow_ups then render; the held resp-1 must not suppress them
        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.attached_as_viewer = true;
            agent.session.current_prompt_id = Some("p1".into());
            agent.apply_follow_ups("resp-1".into(), vec!["old".into()]);
        }
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let request = acp::SessionNotification::new(
            acp::SessionId::new("sess-1"),
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
                acp::TextContent::new("resp-2 streaming"),
            ))),
        )
        .meta(
            serde_json::json!({ "promptId": "p2", "agentTimestampMs": 1 })
                .as_object()
                .cloned(),
        );
        handle(
            AcpClientMessage::SessionNotification(distill_acp_lib::AcpArgs {
                request,
                response_tx: tx,
            }),
            &mut app,
        );
        assert!(
            test_agent(&app, id).follow_ups.is_none(),
            "viewer adopting a new turn must clear the prior chips"
        );
        let affected = handle_ext_notification(&follow_ups_ext("resp-2", &["new"]), &mut app);
        assert!(affected);
        let fu = test_agent(&app, id).follow_ups.as_ref().unwrap();
        assert_eq!(fu.response_id, "resp-2");
        assert_eq!(fu.suggestions, vec!["new"]);
    }

