// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Campaign nudge via the real remote path. A new session, possibly not the first (see
/// [`wait_for_model_via_new_sessions`]), opens on the campaign model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn campaign_remote_settings_nudge_and_dismiss() {
    const CONFIG_MODEL: &str = "config-model";
    const CAMPAIGN_MODEL: &str = "campaign-model";
    const CAMPAIGN_ID: &str = "e2e-remote-nudge";

    let content = ContentController::start_with_models(vec![
        MockModel::new(CONFIG_MODEL),
        MockModel::new(CAMPAIGN_MODEL),
    ])
    .await
    .expect("start content with two models");

    // Serve the campaign from the settings endpoint
    // This replaces the preset settings, so `allow_access` must be restated or the pager parks on the upsell screen
    content.server().set_settings(json!({
        "allow_access": true,
        "campaigns": [
            { "id": CAMPAIGN_ID, "models": { "default": CAMPAIGN_MODEL } }
        ]
    }));

    // Seed config.toml with the user's own default model.
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create GROK_HOME");
    std::fs::write(
        distill_home.join("config.toml"),
        format!("[models]\ndefault = \"{CONFIG_MODEL}\"\n"),
    )
    .expect("write config.toml");

    // Use session (OAuth) auth, not the harness's default XAI_API_KEY: the settings fetch requires `auth_manager.auth()`
    // In ApiKey/BYOK mode the pager never requests `/v1/settings`, so a remote campaign could never arrive (see `spawn_polling_session`'s doc)
    seed_fake_oauth(&content, "pty-campaign-remote");
    let binary = pager_binary().expect("resolve pager binary");
    let spawn = || -> PtyHarness {
        PtyHarness::spawn_with_content_env_ops(
            &binary,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            &content,
            &[],
            &oauth_credential_ops(),
        )
        .expect("spawn pager")
    };

    // ── Phases 1 and 2: the campaign applies to a new session; a pick dismisses. ──
    {
        let mut h = spawn();
        h.wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
            .expect("welcome renders");
        assert!(
            wait_for_model_via_new_sessions(&mut h, CAMPAIGN_MODEL, Duration::from_secs(60)),
            "a new session should open on the remote campaign model\nscreen:\n{}",
            h.screen_contents()
        );
        assert!(
            !h.contains_text("panicked"),
            "pager panicked\n{}",
            h.screen_contents()
        );

        // Picking the config model explicitly persists it as the default and dismisses the campaign
        h.inject_keys(format!("/model {CONFIG_MODEL}\r").as_bytes())
            .expect("pick model");

        // Deterministically wait for the dismiss to land on disk.
        let state_path = distill_home.join("campaigns_state.json");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            h.update(Duration::from_millis(200));
            let dismissed = std::fs::read_to_string(&state_path)
                .map(|s| s.contains(CAMPAIGN_ID))
                .unwrap_or(false);
            if dismissed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "remote campaign id should be recorded dismissed in {state_path:?}\nscreen:\n{}",
                h.screen_contents()
            );
        }
        h.quit().expect("clean quit");
    }

    // ── Phase 3: reboot against the SAME settings; the config model wins. ──
    {
        let mut h = spawn();
        h.wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
            .expect("welcome renders after reboot");
        h.wait_for_text(CONFIG_MODEL, Duration::from_secs(20))
            .unwrap_or_else(|_| {
                panic!(
                    "after dismissal the config model must show\nscreen:\n{}",
                    h.screen_contents()
                )
            });
        // Give the settings fetch time to land, then prove a fresh session still resolves to the user's model
        // The dismissed campaign never re-applies, even once the fetch has put it back in the cache
        let _ = h.inject_keys(b"/new\r");
        h.update(Duration::from_millis(4000));
        h.wait_for_text(CONFIG_MODEL, Duration::from_secs(10))
            .expect("config model after post-fetch /new");
        assert!(
            !h.contains_text(CAMPAIGN_MODEL),
            "a dismissed remote campaign must not re-nudge the model\nscreen:\n{}",
            h.screen_contents()
        );
        h.quit().expect("clean quit");
    }
}
