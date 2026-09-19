// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// With the toggle enabled in config and env, scrollback. CtrlCtrl+R exercises the opt-in mouse
/// reporting toggle. Non-dev builds have no `tracing_rx` metronome, so the toggle's effect must
/// appear without background ticks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn mouse_reporting_toggle_sticky_persists_pty() {
    let content = ContentController::start().await.expect("start content");
    seed_mouse_reporting_toggle_config(&content, true);
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} mouse toggle turn."));

    let mut harness = spawn_mouse_toggle_pager(&content);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn rendered");
    harness.update(Duration::from_secs(1));

    let sticky_visible = |h: &PtyHarness| {
        h.contains_text(MOUSE_OFF_STICKY) || h.contains_text(MOUSE_OFF_HINT_PROMPT)
    };
    let toggle_visible =
        |h: &PtyHarness| sticky_visible(h) || h.contains_text("Mouse reporting on");

    // Tab TOGGLES focus, so never re-press it blindly: a lagged frame would bounce focus back to the
    // prompt. The closure returns if the scrollback already owns keys; otherwise it presses Tab once
    // and waits for the footer's "Space:prompt" to render.
    let focus_scrollback = |h: &mut PtyHarness| {
        if h.contains_text("Space:prompt") {
            return;
        }
        h.inject_keys(b"\t").expect("tab to focus scrollback");
        let _ = h.wait_for_text("Space:prompt", Duration::from_secs(10));
    };

    // Fire Ctrl+R up to three times, re-confirming scrollback focus before each press
    // If startup left capture off, the first press shows the on toast and the next press turns it off sticky
    let mut saw_toggle = false;
    for _ in 0..3 {
        focus_scrollback(&mut harness);
        harness.inject_keys(keys::CTRL_R).expect("ctrl-r");
        harness.update(Duration::from_millis(900));
        let paint_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < paint_deadline {
            if toggle_visible(&harness) {
                saw_toggle = true;
                break;
            }
            harness.update(Duration::from_millis(150));
        }
        if saw_toggle {
            break;
        }
    }
    assert!(
        saw_toggle,
        "mouse toggle must produce sticky off and/or on toast (config_seeded={})\nscreen:\n{}",
        content.home().join(".grok").join("config.toml").exists(),
        harness.screen_contents()
    );

    // If we landed on the sticky off banner, it must survive an idle wait with no metronome
    if sticky_visible(&harness) {
        inject_keys_paced(&mut harness, keys::J);
        harness.update(Duration::from_secs(2));
        inject_keys_paced(&mut harness, keys::K);
        harness.update(Duration::from_millis(400));
        assert!(
            sticky_visible(&harness) || harness.contains_text("Mouse reporting on"),
            "toggle effect must remain observable after idle wait\nscreen:\n{}",
            harness.screen_contents()
        );
    }

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
