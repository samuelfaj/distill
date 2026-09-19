// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `leader_pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Only `app::run`'s own seed makes a remote campaign visible to `resolve_dismissable_campaigns`. Without that seed
/// this test times out in the dismiss phase: the pick persists but no dismissal is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run with cargo test -p distill-pager-pty-harness --test leader_pty_e2e -- --ignored --test-threads=1"]
async fn campaign_leader_mode_remote_dismiss_on_model_pick() {
    const CONFIG_MODEL: &str = "config-model";
    const CAMPAIGN_MODEL: &str = "campaign-model";
    const CAMPAIGN_ID: &str = "e2e-leader-remote-nudge";

    let content = ContentController::start_with_models(vec![
        MockModel::new(CONFIG_MODEL),
        MockModel::new(CAMPAIGN_MODEL),
    ])
    .await
    .expect("start content with two models");

    // Serve the campaign from the settings endpoint (restating `allow_access`, which the preset otherwise provides)
    content.server().set_settings(json!({
        "allow_access": true,
        "campaigns": [
            { "id": CAMPAIGN_ID, "models": { "default": CAMPAIGN_MODEL } }
        ]
    }));

    // Seed config.toml with the user's own default model
    // Pin the leader socket under the shared GROK_HOME so every spawn elects or attaches to the same leader (mirrors `LeaderCluster`)
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create GROK_HOME");
    std::fs::write(
        distill_home.join("config.toml"),
        format!("[models]\ndefault = \"{CONFIG_MODEL}\"\n"),
    )
    .expect("write config.toml");
    let socket = distill_home.join("leader-e2e.sock");
    let socket = socket.to_str().expect("socket path is utf-8").to_owned();

    // Use session (OAuth) auth instead of the harness's default XAI_API_KEY
    // The settings fetch requires `auth_manager.auth()`: in ApiKey/BYOK mode the pager never requests `/v1/settings`
    // Without that request a remote campaign can never reach the pager (see `spawn_polling_session`'s doc)
    seed_fake_oauth(&content, "pty-campaign-leader");
    let binary = pager_binary().expect("resolve pager binary");
    let spawn = || -> PtyHarness {
        PtyHarness::spawn_with_content_env_ops(
            &binary,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            &content,
            &["--leader", "--leader-socket", &socket],
            &oauth_credential_ops(),
        )
        .expect("spawn leader-mode pager")
    };
    let state_path = distill_home.join("campaigns_state.json");
    let dismissed = |state_path: &std::path::Path| {
        std::fs::read_to_string(state_path)
            .map(|s| s.contains(CAMPAIGN_ID))
            .unwrap_or(false)
    };

    // ── Phases 1 and 2: nudge on a new session; a pick records the dismissal in the TUI process
    // Retries fresh TUI spawns (same leader) so a missed 2s prefetch window on a loaded runner can't hang the test
    let mut recorded = false;
    'attempts: for attempt in 0..3 {
        let mut h = spawn();
        // A cold start (leader election plus booting an unoptimized binary) can miss the welcome paint within LEADER_TIMEOUT on a loaded runner
        // The leader outlives this client, so a fresh spawn attaches to the already-running leader and paints promptly
        // Retry as for a missed campaign, and panic only once all attempts are exhausted
        if h.wait_for_text(WELCOME_SCREEN_SENTINEL, LEADER_TIMEOUT)
            .is_err()
        {
            let screen = h.screen_contents();
            h.quit().expect("clean quit");
            assert!(
                attempt < 2,
                "leader-mode welcome never rendered after 3 attempts\nscreen:\n{screen}"
            );
            continue;
        }
        if !wait_for_model_via_new_sessions(&mut h, CAMPAIGN_MODEL, Duration::from_secs(60)) {
            // The campaign never applied on this spawn; try a fresh TUI
            h.quit().expect("clean quit");
            continue;
        }
        h.inject_keys(format!("/model {CONFIG_MODEL}\r").as_bytes())
            .expect("pick model");
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            h.update(Duration::from_millis(200));
            if dismissed(&state_path) {
                recorded = true;
                h.quit().expect("clean quit");
                break 'attempts;
            }
        }
        // The regression under test: the pick persisted but the dismissal is missing
        // With the app::run seed present this only happens when the prefetch missed on this spawn; retry once more before declaring failure
        h.quit().expect("clean quit");
    }
    assert!(
        recorded,
        "leader-mode TUI must record the remote campaign dismissal in {state_path:?}"
    );

    // ── Phase 3: the dismissal is durable and the pick is persisted. The user's choice must be in config.toml, and
    // the campaign value must never be written there. A fresh client on the same leader socket is deliberately not
    // asserted on-screen here.
    let config = std::fs::read_to_string(distill_home.join("config.toml")).expect("read config.toml");
    assert!(
        config.contains(&format!("default = \"{CONFIG_MODEL}\"")),
        "the user's pick must be persisted to config.toml:\n{config}"
    );
    assert!(
        !config.contains(CAMPAIGN_MODEL),
        "the campaign value must never be written to config.toml:\n{config}"
    );
    assert!(
        dismissed(&state_path),
        "the dismissal must survive on disk after the client exits"
    );
}
