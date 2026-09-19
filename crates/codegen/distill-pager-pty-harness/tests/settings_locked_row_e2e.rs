// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: the settings modal's locked coding-data row, driven off the seeded auth entry through the full pipeline.
//! The pipeline runs auth.json, shell `GrokAuth`, auth meta, `AppView::coding_data_sharing_lock()`, `PagerLocalSnapshot`, then the render:
//!
//! - ZDR team (`team_blocked_reasons` = `BLOCKED_REASON_NO_LOGS`): the value column shows exactly `ZDR` (no Opt in / Opt out) and no `›` chevron.
//!   Expanding the row shows only "Your team has Zero Data Retention."
//! - Team non-admin (`team_role` = `MEMBER`): the value shows `Opt out · Admin Managed` and no chevron.
//!   Expanding shows only "Managed by your team admin."
//!
//! Both accounts also suppress the welcome privacy banner even with `GROK_PRIVACY_NOTICE_ROLLOUT=1`.
//! That is asserted on the authenticated welcome screen before opening settings.
//! Row/input details are unit-tested in `distill-pager` (`views/settings_modal/tests.rs`, `locked_coding_*`).
//! This suite covers the auth-to-render pipeline.
//!
//! ```bash
//! cargo test -p distill-pager-pty-harness --test settings_locked_row_e2e \
//!   -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use distill_pager_pty_harness::{
    ContentController, EnvOp, PtyHarness, keys, pager_binary, seed_fake_oauth_team_member,
    seed_fake_oauth_zdr_team,
};

const ROWS: u16 = 50;
const COLS: u16 = 120;
const BANNER_TITLE: &str = "Help improve Grok";
/// Head of the row's label (`Coding data, retention, and training`).
/// The modal truncates long labels, so match the stable prefix.
const ROW_LABEL: &str = "Coding data";
const CHEVRON: &str = "\u{203A}"; // ›
const ZDR_REASON: &str = "Your team has Zero Data Retention.";
const TEAM_REASON: &str = "Managed by your team admin.";
/// Head of the row's description in `settings/defs.rs`.
/// It is kept short so it can't span one of the modal's word wraps.
/// `contains_text` joins rows with `\n`, so a match on wrapped copy would silently never fire.
const DESCRIPTION_PREFIX: &str = "Opt-in to provide usage data";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // opt-in: spawns the real pager binary in a PTY (CI runs with --ignored)
async fn zdr_team_locks_row_and_suppresses_banner() {
    run_zdr().await.expect("zdr locked-row e2e");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // opt-in: spawns the real pager binary in a PTY (CI runs with --ignored)
async fn team_member_sees_admin_managed_row_and_no_banner() {
    run_team_member().await.expect("team-member locked-row e2e");
}

/// Rollout on so a plain opted-out user would see the banner. Fake API key removed so team OAuth is active.
/// ZDR access must be enabled or the blocked welcome never reaches settings. Lock and suppression key off `is_zdr`.
fn locked_row_env_ops() -> [EnvOp<'static>; 3] {
    [
        EnvOp::set("GROK_PRIVACY_NOTICE_ROLLOUT", "1"),
        EnvOp::set("GROK_ZDR_ACCESS_ENABLED", "1"),
        EnvOp::remove("XAI_API_KEY"),
    ]
}

async fn run_zdr() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start mock server")?;
    seed_fake_oauth_zdr_team(&content, "pty-zdr-user");

    let mut pager = launch(&content).context("launch pager")?;
    assert_no_banner_on_welcome(&mut pager)?;

    let line = open_settings_and_grab_row_line(&mut pager)?;
    assert!(
        line.contains("ZDR"),
        "ZDR lock must show `ZDR` on the {ROW_LABEL:?} row: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );
    assert!(
        !line.contains("Opt"),
        "ZDR lock must replace the Opt in/Opt out value: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );
    assert!(
        !line.contains(CHEVRON),
        "locked row must not render the `{CHEVRON}` enter affordance: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );
    // Wrong-variant guard: the team-managed lock reason must not appear.
    assert!(
        !pager.contains_text("Managed by your team admin"),
        "ZDR account rendered the team-managed lock:\n{}",
        pager.screen_contents()
    );

    // Expanded view: the lock reason REPLACES the registry description.
    expand_focused_row(&mut pager, ZDR_REASON)?;
    assert!(
        !pager.contains_text(DESCRIPTION_PREFIX),
        "locked expansion must replace the description, not append to it:\n{}",
        pager.screen_contents()
    );
    assert!(
        !pager.contains_text("Managed by your team admin"),
        "ZDR expansion rendered the team-managed reason:\n{}",
        pager.screen_contents()
    );
    Ok(())
}

async fn run_team_member() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start mock server")?;
    seed_fake_oauth_team_member(&content, "pty-team-user");

    let mut pager = launch(&content).context("launch pager")?;
    assert_no_banner_on_welcome(&mut pager)?;

    let line = open_settings_and_grab_row_line(&mut pager)?;
    assert!(
        line.contains("Opt out \u{00B7} Admin Managed"),
        "team-managed lock must show `Opt out · Admin Managed`: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );
    assert!(
        !line.contains("ZDR"),
        "team-managed lock must not show the ZDR value: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );
    assert!(
        !line.contains(CHEVRON),
        "locked row must not render the `{CHEVRON}` enter affordance: {line:?}\nscreen:\n{}",
        pager.screen_contents()
    );

    // Expanded view: the lock reason REPLACES the registry description.
    expand_focused_row(&mut pager, TEAM_REASON)?;
    assert!(
        !pager.contains_text(DESCRIPTION_PREFIX),
        "locked expansion must replace the description, not append to it:\n{}",
        pager.screen_contents()
    );
    // Wrong-variant guard: the ZDR reason must not appear.
    assert!(
        !pager.contains_text("Zero Data Retention"),
        "team-managed expansion rendered the ZDR reason:\n{}",
        pager.screen_contents()
    );
    Ok(())
}

fn launch(content: &ContentController) -> Result<PtyHarness> {
    let project = tempfile::tempdir().context("project dir")?;
    std::fs::create_dir_all(project.path().join(".git")).context("create .git")?;
    let binary = pager_binary().context("resolve pager binary")?;
    let pager = spawn_pager(&binary, content, project.path()).context("spawn pager")?;
    // Keep the project dir alive for the pager's lifetime.
    std::mem::forget(project);
    Ok(pager)
}

fn spawn_pager(binary: &Path, content: &ContentController, project: &Path) -> Result<PtyHarness> {
    PtyHarness::spawn_with_content_env_ops_in_dir(
        binary,
        ROWS,
        COLS,
        content,
        &[],
        &locked_row_env_ops(),
        Some(project),
    )
}

/// Sync on "New worktree", which renders only on the authenticated welcome menu, then assert the banner never shows.
/// "Quit" also renders while auth is still pending, where the banner is gated off regardless of team state.
fn assert_no_banner_on_welcome(pager: &mut PtyHarness) -> Result<()> {
    pager
        .wait_for_text("New worktree", Duration::from_secs(20))
        .context("authenticated welcome screen")?;
    pager.update(Duration::from_secs(2));
    assert!(
        !pager.contains_text(BANNER_TITLE),
        "team account must suppress the privacy banner:\n{}",
        pager.screen_contents()
    );
    Ok(())
}

/// F2 is AgentScreen-only, so Ctrl+N leaves home first. `/` filter then Enter keeps the row focused for [`expand_focused_row`].
fn open_settings_and_grab_row_line(pager: &mut PtyHarness) -> Result<String> {
    pager
        .inject_keys(keys::CTRL_N)
        .context("leave home into a session")?;
    pager
        .wait_for_text("Shift+Tab", Duration::from_secs(20))
        .context("agent view opened")?;
    pager.update(Duration::from_millis(500));
    pager.inject_keys(keys::F2).context("press F2")?;
    pager
        .wait_for_text("Appearance", Duration::from_secs(20))
        .context("settings modal opened")?;
    pager.inject_keys(b"/").context("focus filter")?;
    pager.update(Duration::from_millis(300));
    pager
        .inject_keys(b"coding data sharing")
        .context("type filter query")?;
    pager.update(Duration::from_millis(300));
    pager.inject_keys(keys::ENTER).context("commit filter")?;
    pager
        .wait_for_text(ROW_LABEL, Duration::from_secs(20))
        .context("coding-data row visible")?;
    pager.update(Duration::from_millis(500));
    let screen = pager.screen_contents();
    screen
        .lines()
        .find(|l| l.contains(ROW_LABEL))
        .map(str::to_owned)
        .with_context(|| format!("{ROW_LABEL:?} line not found:\n{screen}"))
}

/// Expand the focused row with `→` (Browse-mode `KeyCode::Right` inserts the focused key into `expanded_keys`) and wait for `reason` to render.
/// Callers reach here from [`open_settings_and_grab_row_line`], which leaves the coding-data row focused.
fn expand_focused_row(pager: &mut PtyHarness, reason: &str) -> Result<()> {
    pager.inject_keys(keys::RIGHT).context("expand row")?;
    pager.update(Duration::from_millis(300));
    pager
        .wait_for_text(reason, Duration::from_secs(20))
        .with_context(|| format!("expanded lock reason {reason:?} on screen"))
}
