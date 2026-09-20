// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    /// Stale or duplicate gens short-circuit BEFORE the apply (and its config disk loads) runs; nothing observable may change.
    #[test]
    #[serial_test::serial]
    fn announcements_update_stale_gen_short_circuits_before_apply() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");
        app.announcements_last_gen = 5;
        app.active_announcements = vec![critical_announcement("current")];
        // This marker cannot survive an apply: it matches no pushed announcement, so any apply would prune it (and queue a persist)
        app.hidden_announcement_ids = ["stale-key".to_string()].into_iter().collect();

        for stale_gen in [4, 5] {
            let changed = handle_ext_notification(
                &announcements_update_notif(stale_gen, &[critical_announcement("stale-push")]),
                &mut app,
            );
            assert!(!changed, "gen {stale_gen} must be dropped at the watermark");
        }

        assert_eq!(
            app.active_announcements,
            vec![critical_announcement("current")]
        );
        assert_eq!(app.announcements_last_gen, 5);
        assert!(app.hidden_announcement_ids.contains("stale-key"));
        assert!(
            !app.pending_effects
                .iter()
                .any(|e| matches!(e, Effect::PersistAnnouncementsHidden { .. })),
            "short-circuit must not reach prune/persist, got {:?}",
            app.pending_effects
        );
    }

    /// The watermark lasts one connection: the event loop resets it to 0 on leader reconnect.
    /// A re-elected shell's fresh (possibly lower) gen sequence then applies, and a second copy of the seed broadcast stays a no-op.
    #[test]
    #[serial_test::serial]
    fn announcements_update_applies_after_reconnect_watermark_reset() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");
        // The previous connection left a watermark ahead of the new shell's gens
        app.announcements_last_gen = 9_999_999_999;
        // The event loop's leader-reconnected branch does this reset
        app.announcements_last_gen = 0;

        let first = handle_ext_notification(
            &announcements_update_notif(1, &[critical_announcement("fresh")]),
            &mut app,
        );
        assert!(first, "gen 1 must apply after the reconnect reset");
        assert_eq!(app.announcements_last_gen, 1);
        assert!(
            app.active_announcements
                .iter()
                .any(|a| a.id.as_deref() == Some("fresh")),
            "pushed announcement must land"
        );

        // The per-client seed broadcast can deliver the same gen twice.
        let dup = handle_ext_notification(
            &announcements_update_notif(1, &[critical_announcement("fresh")]),
            &mut app,
        );
        assert!(!dup, "duplicate seed copy must be idempotent");
        assert_eq!(app.announcements_last_gen, 1);
    }

    /// A push prunes hidden ids whose announcement is gone and schedules a persist so the on-disk set cannot grow unboundedly. Driven through the
    /// layer-injected seam so the developer's real `~/.grok` cannot leak in.
    #[test]
    #[serial_test::serial]
    fn announcements_update_prunes_stale_hidden_ids_and_persists() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");
        app.hidden_announcement_ids = ["gone".to_string(), "live".to_string()]
            .into_iter()
            .collect();

        apply_announcements_update(
            &mut app,
            1,
            &[critical_announcement("live")],
            None,
            None,
            None,
        );

        assert_eq!(app.announcements_last_gen, 1);
        let expected: std::collections::BTreeSet<String> =
            ["live".to_string()].into_iter().collect();
        assert_eq!(app.hidden_announcement_ids, expected);
        assert!(
            app.pending_effects.iter().any(|e| matches!(
                e,
                Effect::PersistAnnouncementsHidden { hidden_ids } if hidden_ids == &expected
            )),
            "prune must persist the shrunken set, got {:?}",
            app.pending_effects
        );
        assert_eq!(
            shown_banner_id(&app),
            None,
            "surviving hidden id still hides its banner"
        );
    }

    /// A pushed critical with a NEW id must re-show the banner even though an older critical was hidden (the whole point of per-ID hide). Driven
    /// through the layer-injected seam (no real `~/.grok` reads).
    #[test]
    #[serial_test::serial]
    fn announcements_update_new_critical_id_rearms_hidden_banner() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");
        apply_announcements_update(
            &mut app,
            1,
            &[critical_announcement("outage-a")],
            None,
            None,
            None,
        );
        app.hidden_announcement_ids.insert("outage-a".to_string());
        assert_eq!(shown_banner_id(&app), None);

        apply_announcements_update(
            &mut app,
            2,
            &[critical_announcement("outage-b")],
            None,
            None,
            None,
        );

        assert_eq!(app.announcements_last_gen, 2);
        assert_eq!(
            shown_banner_id(&app).as_deref(),
            Some("outage-b"),
            "new critical id must re-show the banner"
        );
        // The stale hide key was pruned with the list replacement.
        assert!(app.hidden_announcement_ids.is_empty());
    }

    /// A push must not drop config-layer announcements, and prune must not erase their persisted hide keys.
    /// Config layers re-resolve every launch, so a dropped key would re-show a critical the user already hid.
    #[test]
    #[serial_test::serial]
    fn announcements_update_remerges_config_layers_and_keeps_their_hide_keys() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");
        let user_cfg: toml::Value = toml::from_str(
            r#"
            [[announcements]]
            id = "cfg-crit"
            title = "Config outage"
            message = "from user config"
            severity = "critical"
            "#,
        )
        .unwrap();
        app.hidden_announcement_ids = ["cfg-crit".to_string()].into_iter().collect();

        apply_announcements_update(
            &mut app,
            1,
            &[critical_announcement("live")],
            None,
            Some(&user_cfg),
            None,
        );

        assert_eq!(app.announcements_last_gen, 1);
        let ids: Vec<_> = app
            .active_announcements
            .iter()
            .filter_map(|a| a.id.as_deref())
            .collect();
        assert_eq!(
            ids,
            ["live", "cfg-crit"],
            "config-layer announcement must survive the push (remote > user order)"
        );
        assert!(
            app.hidden_announcement_ids.contains("cfg-crit"),
            "config-layer hide key must survive prune"
        );
        assert!(
            !app.pending_effects
                .iter()
                .any(|e| matches!(e, Effect::PersistAnnouncementsHidden { .. })),
            "unchanged hidden set must not schedule a persist, got {:?}",
            app.pending_effects
        );
        assert_eq!(
            shown_banner_id(&app).as_deref(),
            Some("live"),
            "pushed critical shows; the hidden config-layer one stays skipped"
        );
    }

    /// A mid-session push must open the `/announcements` gate on already-live subagent child views, not just top-level agents. Driven through the
    /// layer-injected seam (no real `~/.grok` reads).
    #[test]
    #[serial_test::serial]
    fn announcements_update_fans_slash_gate_to_live_subagent_views() {
        let _announcements =
            distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_parent_and_child("parent-sess", "child-sess");
        assert!(
            !test_subagent(test_agent(&app, AgentId(0)), "child-sess")
                .prompt
                .slash_controller
                .has_session_announcements(),
            "gate starts closed"
        );

        apply_announcements_update(
            &mut app,
            1,
            &[critical_announcement("outage-a")],
            None,
            None,
            None,
        );

        let agent = test_agent(&app, AgentId(0));
        assert!(
            agent.prompt.slash_controller.has_session_announcements(),
            "parent gate open"
        );
        assert!(
            test_subagent(agent, "child-sess")
                .prompt
                .slash_controller
                .has_session_announcements(),
            "live child view gate open"
        );
    }

    /// The owner's switch, and the reason it is the default: a backend push must
    /// not put a provider's banner on the welcome screen of a harness that routes
    /// to several providers. `DISTILL_ANNOUNCEMENTS=1` (or
    /// `[announcements] enabled = true`) is what brings them back.
    #[test]
    #[serial_test::serial]
    fn a_backend_push_is_dropped_unless_announcements_are_enabled() {
        let _off = distill_test_support::env::EnvGuard::unset("DISTILL_ANNOUNCEMENTS");
        let mut app = make_app_with_agent("sess-ann");
        apply_announcements_update(&mut app, 1, &[critical_announcement("pushed")], None, None, None);
        assert!(app.active_announcements.is_empty(), "{:?}", app.active_announcements);
        assert_eq!(shown_banner_id(&app), None, "no banner, no announcement");
        assert_eq!(
            app.announcements_last_gen, 1,
            "the watermark still advances: the push was read, not ignored"
        );

        // Both producers must agree, and this is the producer the settings
        // path owns; the event loop calls the same gate.
        let _on = distill_test_support::env::EnvGuard::set("DISTILL_ANNOUNCEMENTS", "1");
        apply_announcements_update(&mut app, 2, &[critical_announcement("pushed")], None, None, None);
        assert!(
            app.active_announcements
                .iter()
                .any(|a| a.id.as_deref() == Some("pushed")),
            "opted in, the push lands"
        );
    }

    /// Existing installations may still use the pre-Distill override. Keep
    /// it as a fallback, while all current product surfaces use the canonical
    /// Distill variable above.
    #[test]
    #[serial_test::serial]
    fn legacy_announcement_env_remains_a_compatibility_fallback() {
        let _canonical = distill_test_support::env::EnvGuard::unset("DISTILL_ANNOUNCEMENTS");
        let _legacy =
            distill_test_support::env::EnvGuard::set("REMOTE_CODE_ANNOUNCEMENTS", "1");
        let mut app = make_app_with_agent("sess-ann");

        apply_announcements_update(
            &mut app,
            1,
            &[critical_announcement("legacy")],
            None,
            None,
            None,
        );

        assert!(
            app.active_announcements
                .iter()
                .any(|announcement| announcement.id.as_deref() == Some("legacy")),
            "the legacy env remains readable during migration"
        );
    }
