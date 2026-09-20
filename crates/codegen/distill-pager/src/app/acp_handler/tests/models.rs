// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    /// Regression: a machine-wide `x.ai/models/update` broadcast carries each model's static catalog-default effort (`high`).
    /// It does not carry the session's chosen `xhigh` and must not clobber that per-session choice.
    #[test]
    fn models_update_preserves_user_reasoning_effort() {
        use distill_shell::sampling::types::ReasoningEffort;
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id = acp::ModelId::new(std::sync::Arc::from("reason-model"));
        let mut info = make_model_info("reason-model");
        info.meta = serde_json::json!({
            "supportsReasoningEffort": true,
            "reasoningEffort": "high",
        })
        .as_object()
        .cloned();
        agent.session.models.available.insert(id.clone(), info);
        agent
            .session
            .models
            .set_current(id, Some(ReasoningEffort::Xhigh));
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::Xhigh)
        );

        let notif = make_reasoning_models_update_notif("reason-model", "high");
        assert!(handle_models_update(&notif, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::Xhigh),
            "models/update broadcast must not clobber a user-set per-session effort"
        );
    }

    #[test]
    fn models_update_keeps_session_model_when_removed_from_catalog() {
        let mut app = make_app_with_agent("sess-1");

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let id_3 = acp::ModelId::new(std::sync::Arc::from("grok-3"));
        agent
            .session
            .models
            .available
            .insert(id_3.clone(), make_model_info("grok-3"));
        agent.session.models.current = Some(id_3);

        let notif = make_models_update_notif("grok-4.3", &["grok-4.3", "grok-4.5"]);
        handle_models_update(&notif, &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "catalog refresh must not change the displayed session model"
        );
        assert!(
            agent
                .session
                .models
                .available
                .contains_key(&acp::ModelId::new(std::sync::Arc::from("grok-4.5"))),
            "the /model list should reflect the new catalog"
        );
    }

    #[test]
    fn models_update_keeps_app_current_when_still_in_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppView::new(tx, ModelState::default(), Vec::new(), crate::render::draw::EscapeWriter::disconnected());
        let id = acp::ModelId::new(std::sync::Arc::from("grok-3"));
        app.models.available.insert(id.clone(), make_model_info("grok-3"));
        app.models.current = Some(id);

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-3"),
            "app-level current stays if it is still in the new catalog"
        );
    }

    #[test]
    fn models_update_adopts_broadcast_when_app_current_missing_from_catalog() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppView::new(tx, ModelState::default(), Vec::new(), crate::render::draw::EscapeWriter::disconnected());
        let old = acp::ModelId::new(std::sync::Arc::from("opus"));
        app.models.available.insert(old.clone(), make_model_info("opus"));
        app.models.current = Some(old);

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        assert_eq!(
            app.models.current.as_ref().map(|id| id.0.as_ref()),
            Some("grok-4"),
            "app-level current adopts the broadcast default when dropped from the catalog"
        );
    }

    #[test]
    fn models_update_preserves_each_agent_model_independently() {
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));

        {
            let agent_a = app.agents.get_mut(&AgentId(0)).unwrap();
            let id_3 = acp::ModelId::new(std::sync::Arc::from("grok-3"));
            agent_a
                .session
                .models
                .available
                .insert(id_3.clone(), make_model_info("grok-3"));
            agent_a.session.models.current = Some(id_3);
        }

        {
            let agent_b = app.agents.get_mut(&AgentId(1)).unwrap();
            let id = acp::ModelId::new(std::sync::Arc::from("grok-4.5"));
            agent_b
                .session
                .models
                .available
                .insert(id.clone(), make_model_info("grok-4.5"));
            agent_b.session.models.current = Some(id);
        }

        let notif = make_models_update_notif("grok-4", &["grok-3", "grok-4"]);
        handle_models_update(&notif, &mut app);

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "active agent's model must be preserved"
        );

        let agent_b = app.agents.get(&AgentId(1)).unwrap();
        assert_eq!(
            agent_b
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-4.5"),
            "inactive agent must keep its session model when the catalog drops it"
        );
    }

    /// A follower client (no in-flight switch of its own) receives the leader's `ModelChanged` broadcast and silently mirrors the new model.
    /// It pushes no scrollback entry and no toast; it updates just enough state for the status bar and the `/model` dropdown to render.
    #[test]
    fn model_changed_updates_state_silently_on_follower() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);
        let scrollback_before = agent.scrollback.len();
        // Follower: no local switch in flight.
        assert!(!agent.session.model_switch_pending);

        let notif = model_changed_ext("sess-1", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            changed,
            "follower's state changed → handler must request a redraw"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-4"),
            "follower must mirror the remote switch into its local model state",
        );
        assert_eq!(
            agent.scrollback.len(),
            scrollback_before,
            "follower must NOT push a 'Switched to' scrollback entry — that is \
             the invoking client's job (SwitchModelComplete owns the system message)"
        );
        assert!(
            !agent.session.model_switch_pending,
            "follower's pending flag must stay false (no local switch was issued)"
        );
    }

    #[test]
    fn model_changed_restamps_context_total_from_new_catalog_model() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let old_id = acp::ModelId::new(std::sync::Arc::from("reasoning-old"));
        let new_id = acp::ModelId::new(std::sync::Arc::from("reasoning-new"));
        let mut old_info = make_model_info("reasoning-old");
        old_info.meta = Some(
            serde_json::json!({"totalContextTokens": 128_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let mut new_info = make_model_info("reasoning-new");
        new_info.meta = Some(
            serde_json::json!({"totalContextTokens": 272_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        agent.session.models.available.insert(old_id.clone(), old_info);
        agent.session.models.available.insert(new_id.clone(), new_info);
        agent.session.models.current = Some(old_id);
        agent.session.models.override_context_window(300_000);
        agent.apply_context_used(2_100, 128_000);

        assert!(handle_ext_notification(
            &model_changed_ext("sess-1", "reasoning-new", None),
            &mut app,
        ));

        let context = app.agents.get(&AgentId(0)).unwrap().context_state.as_ref().unwrap();
        assert_eq!((context.used, context.total), (2_100, 272_000));
        assert_eq!(
            app.agents
                .get(&AgentId(0))
                .unwrap()
                .session
                .models
                .get_context_window(),
            Some(272_000),
        );
    }

    #[test]
    fn model_changed_uses_authoritative_session_window_even_when_it_matches_old_catalog() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        let old_id = acp::ModelId::new(std::sync::Arc::from("reasoning-old"));
        let new_id = acp::ModelId::new(std::sync::Arc::from("reasoning-new"));
        let mut old_info = make_model_info("reasoning-old");
        old_info.meta = Some(
            serde_json::json!({"totalContextTokens": 100_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let mut new_info = make_model_info("reasoning-new");
        new_info.meta = Some(
            serde_json::json!({"totalContextTokens": 272_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        agent.session.models.available.insert(old_id.clone(), old_info);
        agent.session.models.available.insert(new_id, new_info);
        agent.session.models.current = Some(old_id);
        agent.apply_context_used(2_100, 100_000);

        assert!(handle_ext_notification(
            &model_changed_ext_with_context("sess-1", "reasoning-new", None, Some(100_000)),
            &mut app,
        ));

        let context = app.agents.get(&AgentId(0)).unwrap().context_state.as_ref().unwrap();
        assert_eq!((context.used, context.total), (2_100, 100_000));

        // A normal token-only update must keep the producer's authoritative
        // denominator; it must not fall back to the new model's catalog size.
        let _ = handle(make_token_notification_message("sess-1", 2_200), &mut app);
        let context = app.agents.get(&AgentId(0)).unwrap().context_state.as_ref().unwrap();
        assert_eq!((context.used, context.total), (2_200, 100_000));
    }

    #[test]
    fn local_model_completion_preserves_status_window_through_token_updates() {
        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        let old_id = acp::ModelId::new(std::sync::Arc::from("chatgpt/gpt-5.6-luna"));
        let new_id = acp::ModelId::new(std::sync::Arc::from("chatgpt/gpt-6-astra"));
        let agent = app.agents.get_mut(&id).unwrap();
        let mut old_info = make_model_info("chatgpt/gpt-5.6-luna");
        old_info.meta = Some(
            serde_json::json!({"totalContextTokens": 100_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let mut new_info = make_model_info("chatgpt/gpt-6-astra");
        new_info.meta = Some(
            serde_json::json!({"totalContextTokens": 272_000})
                .as_object()
                .cloned()
                .unwrap(),
        );
        agent.session.models.available.insert(old_id.clone(), old_info);
        agent.session.models.available.insert(new_id.clone(), new_info);
        agent.session.models.current = Some(old_id);
        agent.session.model_switch_pending = true;
        agent.apply_context_used(2_100, 100_000);

        crate::app::dispatch::dispatch(
            crate::app::actions::Action::TaskComplete(
                crate::app::actions::TaskResult::SwitchModelComplete {
                    agent_id: id,
                    model_id: new_id.clone(),
                    effort: None,
                    result: Ok(Some(100_000)),
                    prev_model_id: None,
                },
            ),
            &mut app,
        );

        let agent = app.agents.get(&id).unwrap();
        assert_eq!(agent.session.models.current, Some(new_id.clone()));
        assert_eq!(agent.session.models.get_context_window(), Some(100_000));

        // Completion must establish the authoritative denominator before any
        // ordinary token update, even when no status row is available.
        let _ = handle(make_token_notification_message("sess-1", 2_200), &mut app);
        let context = app.agents.get(&id).unwrap().context_state.as_ref().unwrap();
        assert_eq!((context.used, context.total), (2_200, 100_000));

        // A later status is also authoritative, and a following ordinary
        // token-only update must retain it rather than rereading the catalog.
        let mut status = crate::app::status_line::test_context("/tmp");
        status.model.id = Some("chatgpt/gpt-6-astra".to_string());
        status.context_window.context_tokens = Some(2_200);
        status.context_window.context_window_size = Some(100_000);
        let payload = SessionNotification {
            session_id: acp::SessionId::new("sess-1"),
            update: XaiSessionUpdate::SessionStatus(Box::new(status)),
            meta: None,
        };
        let raw = serde_json::value::to_raw_value(&payload).unwrap();
        handle_ext_notification(
            &acp::ExtNotification::new(
                "x.ai/session_notification",
                std::sync::Arc::from(raw),
            ),
            &mut app,
        );
        let _ = handle(make_token_notification_message("sess-1", 2_300), &mut app);

        let context = app.agents.get(&id).unwrap().context_state.as_ref().unwrap();
        assert_eq!((context.used, context.total), (2_300, 100_000));
    }

    /// A live remote `ModelChanged` (the leader fanning out another client's switch) must apply even when a local `user_model_preference` is set.
    /// Otherwise the status bar desyncs from the gateway session; the preference is updated to track the new live model.
    /// Replayed history would silently revert the model; the shell suppresses that via `ReconnectState::user_selected_model`, not this handler.
    #[test]
    fn model_changed_applies_and_updates_user_model_preference() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "heavy", &["auto", "heavy"]);
        agent.session.user_model_preference =
            Some(acp::ModelId::new(std::sync::Arc::from("heavy")));
        assert!(!agent.session.model_switch_pending);

        let notif = model_changed_ext("sess-1", "auto", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            changed,
            "remote live ModelChanged must apply despite prior local preference"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("auto"),
            "selector must mirror the remote switch"
        );
        assert_eq!(
            agent
                .session
                .user_model_preference
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("auto"),
            "preference must track the applied remote switch"
        );
    }

    /// The broadcast handler must therefore be a no-op here, gated on `model_switch_pending == true`.
    /// The test checks the broadcast does not touch `models.current`, preserving the pre-response snapshot.
    /// If the broadcast updated state here, the response handler would see `prev == new`, mark it unchanged, and suppress the user-facing message.
    #[test]
    fn model_changed_skipped_when_local_switch_in_flight() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);
        // Invoker: a local switch is in flight (set by Action::SwitchModel or set_default_model before the SetSessionModelRequest is sent)
        agent.session.model_switch_pending = true;
        let scrollback_before = agent.scrollback.len();

        let notif = model_changed_ext("sess-1", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            !changed,
            "broadcast must be a no-op while local switch is pending"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "models.current must stay at the pre-response snapshot — \
             SwitchModelComplete owns the final apply + system message"
        );
        assert_eq!(
            agent.scrollback.len(),
            scrollback_before,
            "broadcast must not push any scrollback entry on the invoker"
        );
        assert!(
            agent.session.model_switch_pending,
            "pending flag must remain set until SwitchModelComplete arrives"
        );
    }

    /// A `ModelChanged` broadcast carrying a model id the local catalog doesn't know about must be dropped.
    /// Applying it would render an unresolvable id in the status bar and desync the `/model` dropdown.
    /// This can happen when the leader and a follower briefly disagree on the model catalog (etag drift, or a skewed custom-model config).
    #[test]
    fn model_changed_dropped_when_model_unknown_to_catalog() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-1", "grok-99-unknown", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(
            !changed,
            "unknown model must NOT trigger a redraw — no state changed"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "models.current must stay on the previously-known model"
        );
    }

    /// `reasoning_effort` round-trips through the broadcast: the follower applies it alongside the model id.
    /// The prompt header and status bar then show the right effort without waiting for a later `x.ai/models/update`.
    #[test]
    fn model_changed_applies_reasoning_effort_on_follower() {
        use distill_shell::sampling::types::ReasoningEffort;
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-1", "grok-4", Some("high"));
        assert!(handle_ext_notification(&notif, &mut app));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.session.models.reasoning_effort,
            Some(ReasoningEffort::High),
            "follower must mirror the broadcast's reasoning_effort"
        );
    }

    /// `ModelChanged` for a session this client doesn't own or hasn't loaded must be dropped; `find_session_match` returns `None`.
    /// The bug would be: client A switches a model on session X in leader mode, X was never opened here, and the change lands on the active agent.
    #[test]
    fn model_changed_dropped_for_unknown_session_id() {
        let mut app = make_app_with_agent("sess-1");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        seed_models(agent, "grok-3", &["grok-3", "grok-4"]);

        let notif = model_changed_ext("sess-OTHER", "grok-4", None);
        let changed = handle_ext_notification(&notif, &mut app);
        assert!(!changed);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .session
                .models
                .current
                .as_ref()
                .map(|id| id.0.as_ref()),
            Some("grok-3"),
            "unrelated-session broadcast must not touch this agent's model"
        );
    }
