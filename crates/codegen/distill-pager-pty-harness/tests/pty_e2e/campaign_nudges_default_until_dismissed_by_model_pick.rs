// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A campaign nudges a soft default model that overrides the config default for new sessions; an
/// explicit `/model` pick dismisses it (the user wins).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn campaign_nudges_default_until_dismissed_by_model_pick() {
    const CONFIG_MODEL: &str = "config-model";
    const CAMPAIGN_MODEL: &str = "campaign-model";
    const CAMPAIGN_ID: &str = "e2e-nudge";

    let content = ContentController::start_with_models(vec![
        MockModel::new(CONFIG_MODEL),
        MockModel::new(CAMPAIGN_MODEL),
    ])
    .await
    .expect("start content with two models");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} ok."));

    // Seed config.toml with the user's own default model.
    let distill_home = content.home().join(".grok");
    std::fs::create_dir_all(&distill_home).expect("create GROK_HOME");
    std::fs::write(
        distill_home.join("config.toml"),
        format!("[models]\ndefault = \"{CONFIG_MODEL}\"\n"),
    )
    .expect("write config.toml");

    // The env var stands in for a remotely served campaign; it nudges new sessions to CAMPAIGN_MODEL
    let campaign_env = (
        "GROK_CAMPAIGNS_OVERRIDE".to_string(),
        format!(r#"[{{"id":"{CAMPAIGN_ID}","models":{{"default":"{CAMPAIGN_MODEL}"}}}}]"#),
    );
    let binary = pager_binary().expect("resolve pager binary");

    let spawn = |extra: &(String, String)| -> PtyHarness {
        let overrides: Vec<(String, String)> = vec![extra.clone()];
        let env_refs: Vec<(&str, &str)> = overrides
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        PtyHarness::spawn_with_content_env(
            &binary,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            &content,
            &[],
            &env_refs,
        )
        .expect("spawn pager")
    };

    // ── Phase 1: a fresh boot shows the campaign model, not the config one. ──
    {
        let mut h = spawn(&campaign_env);
        h.wait_for_text(CAMPAIGN_MODEL, WELCOME_TIMEOUT)
            .unwrap_or_else(|_| {
                panic!(
                    "welcome should show the campaign-nudged model\nscreen:\n{}",
                    h.screen_contents()
                )
            });
        assert!(
            !h.contains_text("panicked"),
            "pager panicked\n{}",
            h.screen_contents()
        );
        h.quit().expect("clean quit");
    }

    // ── Phase 2: a session and an explicit `/model` pick dismiss the campaign. ──
    {
        let mut h = spawn(&campaign_env);
        h.wait_for_text(CAMPAIGN_MODEL, WELCOME_TIMEOUT)
            .expect("welcome shows campaign model");
        h.inject_keys(format!("{PROMPT}\r").as_bytes())
            .expect("submit prompt");
        h.wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
            .expect("response rendered");
        // Picking the config model explicitly persists the default and dismisses the campaign
        h.inject_keys(format!("/model {CONFIG_MODEL}\r").as_bytes())
            .expect("pick model");

        // Deterministically wait for the dismiss to land on disk.
        let state_path = distill_home.join("campaigns_state.json");
        let deadline = Instant::now() + Duration::from_secs(15);
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
                "campaign id should be recorded dismissed in {state_path:?}\nscreen:\n{}",
                h.screen_contents()
            );
        }
        h.quit().expect("clean quit");
    }

    // ── Phase 3: reboot with the SAME campaign env; the config model wins. ──
    {
        let mut h = spawn(&campaign_env);
        h.wait_for_text(CONFIG_MODEL, WELCOME_TIMEOUT)
            .unwrap_or_else(|_| {
                panic!(
                    "after dismissal the welcome must show the config model\nscreen:\n{}",
                    h.screen_contents()
                )
            });
        assert!(
            !h.contains_text(CAMPAIGN_MODEL),
            "a dismissed campaign must not re-nudge the model\nscreen:\n{}",
            h.screen_contents()
        );
        h.quit().expect("clean quit");
    }
}
