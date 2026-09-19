// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: the coding-data privacy upsell banner.
//! It shows on the welcome screen for an opted-out OAuth user under the `privacy_notice_rollout` flag and persists into the agent view.
//! Both buttons write the choice through the shell's `PUT /privacy/coding-data-retention` round trip and stamp
//! `[privacy].privacy_banner_acked` only once that succeeds. The banner hides as soon as the write is pending, so
//! hiding and acknowledging are separate steps.
//!
//! Drives the real pager binary through a PTY against the shared mock inference server (isolated `$HOME`).
//! A seeded opted-out OAuth entry is the active auth (`XAI_API_KEY` removed) and the rollout is forced on via `GROK_PRIVACY_NOTICE_ROLLOUT=1`.
//!
//! ```bash
//! cargo test -p distill-pager-pty-harness --test privacy_banner_e2e \
//!   -- --ignored --nocapture
//! ```

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use distill_pager_pty_harness::{
    ContentController, EnvOp, PtyExitPoll, PtyHarness, keys, pager_binary,
    seed_fake_oauth_coding_data_opted_out,
};

const ROWS: u16 = 50;
const COLS: u16 = 120;
const BANNER_TITLE: &str = "Help improve Grok";
const OPT_OUT: &str = "[Opt out]";
const OPT_IN: &str = "[Opt in]";
const ACK: &str = "BANNERACK";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // opt-in: spawns the real pager binary in a PTY (CI runs with --ignored)
async fn privacy_banner_welcome_opt_out_ack_persists() {
    run_opt_out().await.expect("privacy banner opt-out e2e");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore] // opt-in: spawns the real pager binary in a PTY (CI runs with --ignored)
async fn privacy_banner_persists_into_agent_view_and_opt_in_shares() {
    run_opt_in().await.expect("privacy banner opt-in e2e");
}

/// The banner's two preconditions: force the rollout flag on (the env override beats remote settings) and remove the sandbox's fake `XAI_API_KEY`.
/// Removing the key makes the seeded opted-out OAuth entry the active auth.
fn banner_env_ops() -> [EnvOp<'static>; 2] {
    [
        EnvOp::set("GROK_PRIVACY_NOTICE_ROLLOUT", "1"),
        EnvOp::remove("XAI_API_KEY"),
    ]
}

async fn run_opt_out() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start mock server")?;
    seed_fake_oauth_coding_data_opted_out(&content, "pty-privacy-user");

    let project = tempfile::tempdir().context("project dir")?;
    std::fs::create_dir_all(project.path().join(".git")).context("create .git")?;
    let binary = pager_binary().context("resolve pager binary")?;

    let mut pager = spawn_pager(&binary, &content, project.path()).context("spawn pager")?;
    wait_for_banner(&mut pager)?;
    assert!(
        pager.contains_text(OPT_IN),
        "welcome banner is missing {OPT_IN}:\n{}",
        pager.screen_contents()
    );

    click_text(&mut pager, OPT_OUT).context("click Opt out")?;

    // The banner hides while the write is pending, and must not detour into settings; the ack below waits on the reply
    pager
        .wait_for_text_absent(BANNER_TITLE, Duration::from_secs(10))
        .context("banner dismissed by [Opt out]")?;
    assert!(
        !pager.contains_text("Coding data,"),
        "[Opt out] must answer the question, not open settings:\n{}",
        pager.screen_contents()
    );

    // The config write is async, so poll for it
    wait_for_ack_on_disk(&mut pager, content.home(), Duration::from_secs(10))?;

    quit_via_double_ctrl_c(&mut pager)?;
    drop(pager);

    // Relaunch with the same sandbox: the acked banner must not re-show.
    // Sync on "New worktree", which renders only on the authenticated welcome menu
    // "Quit" also appears while auth is still pending, where the banner is gated off regardless of the ack
    let mut relaunched =
        spawn_pager(&binary, &content, project.path()).context("relaunch pager")?;
    relaunched
        .wait_for_text("New worktree", Duration::from_secs(20))
        .context("relaunched authenticated welcome screen")?;
    relaunched.update(Duration::from_secs(2));
    assert!(
        !relaunched.contains_text(BANNER_TITLE),
        "acked banner re-showed after relaunch:\n{}",
        relaunched.screen_contents()
    );
    Ok(())
}

async fn run_opt_in() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start mock server")?;
    content.set_response(format!("{ACK} done."));
    seed_fake_oauth_coding_data_opted_out(&content, "pty-privacy-user");

    let project = tempfile::tempdir().context("project dir")?;
    std::fs::create_dir_all(project.path().join(".git")).context("create .git")?;
    let binary = pager_binary().context("resolve pager binary")?;

    let mut pager = spawn_pager(&binary, &content, project.path()).context("spawn pager")?;
    wait_for_banner(&mut pager)?;

    pager.inject_keys(b"hello").context("type prompt")?;
    pager.inject_keys(keys::ENTER).context("submit prompt")?;
    pager
        .wait_for_text(ACK, Duration::from_secs(30))
        .context("turn response rendered")?;
    pager.update(Duration::from_millis(1000));
    assert!(
        pager.contains_text(BANNER_TITLE),
        "banner did not persist into the agent view:\n{}",
        pager.screen_contents()
    );

    click_text(&mut pager, OPT_IN).context("click Opt in")?;

    // Ack only lands after the shell's PUT round trip confirms 2xx.
    pager
        .wait_for_text_absent(BANNER_TITLE, Duration::from_secs(20))
        .context("banner disappeared after [Opt in]")?;
    wait_for_ack_on_disk(&mut pager, content.home(), Duration::from_secs(10))?;

    let put_bodies: Vec<_> = content
        .requests()
        .iter()
        .filter(|e| e.method == "PUT" && e.path == "/v1/privacy/coding-data-retention")
        .filter_map(|e| e.body.clone())
        .collect();
    assert!(
        put_bodies
            .iter()
            .any(|b| b["codingDataRetentionOptOut"] == serde_json::json!(false)),
        "mock server did not see the opt-in PUT; got: {put_bodies:?}"
    );
    Ok(())
}

fn spawn_pager(binary: &Path, content: &ContentController, project: &Path) -> Result<PtyHarness> {
    PtyHarness::spawn_with_content_env_ops_in_dir(
        binary,
        ROWS,
        COLS,
        content,
        &[],
        &banner_env_ops(),
        Some(project),
    )
}

/// Wait for the welcome menu first (auth resolved) so a missing banner is a real failure rather than an early frame, then for the banner itself.
fn wait_for_banner(pager: &mut PtyHarness) -> Result<()> {
    pager
        .wait_for_text("Quit", Duration::from_secs(20))
        .context("welcome screen")?;
    pager
        .wait_for_text(BANNER_TITLE, Duration::from_secs(20))
        .context("privacy banner on screen")
}

/// Click `needle` by injecting an SGR (DECSET 1006) press and release at its first character.
/// The wire encoding is 1-based `col;row` (`screen_contents` line 0 is row 1).
/// The banner region is ASCII-only, so the byte offset within the line is the column.
fn click_text(pager: &mut PtyHarness, needle: &str) -> Result<()> {
    let screen = pager.screen_contents();
    let (row0, col0) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find(needle).map(|col| (row, col)))
        .with_context(|| format!("{needle:?} not on screen:\n{screen}"))?;
    let (row, col) = (row0 + 1, col0 + 1);
    pager
        .inject_keys(format!("\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m").as_bytes())
        .context("inject SGR click")?;
    pager.update(Duration::from_millis(250));
    Ok(())
}

/// Poll `<home>/.grok/config.toml` for the async `privacy_banner_acked` write.
/// Pumps PTY output between polls so the pager never blocks on a full output buffer.
fn wait_for_ack_on_disk(pager: &mut PtyHarness, home: &Path, timeout: Duration) -> Result<()> {
    let path = home.join(".grok").join("config.toml");
    let deadline = Instant::now() + timeout;
    loop {
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        if body.contains("privacy_banner_acked") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out after {timeout:?} waiting for privacy_banner_acked in {}\n\
                 config contents:\n{body}\nscreen:\n{}",
                path.display(),
                pager.screen_contents()
            );
        }
        pager.update(Duration::from_millis(100));
    }
}

/// The first Ctrl+C opens the quit confirmation on the empty prompt, the second confirms.
/// Retry the pair in case an overlay swallowed the first one.
fn quit_via_double_ctrl_c(pager: &mut PtyHarness) -> Result<()> {
    for _ in 0..3 {
        pager.inject_keys(keys::CTRL_C).context("ctrl-c arm")?;
        pager.update(Duration::from_millis(250));
        pager.inject_keys(keys::CTRL_C).context("ctrl-c confirm")?;
        pager.update(Duration::from_millis(250));
        match pager.wait_exit_code(Duration::from_secs(5))? {
            PtyExitPoll::Exited(code) => {
                assert_eq!(code, 0, "graceful quit should exit 0, got {code}");
                return Ok(());
            }
            _ => continue,
        }
    }
    bail!(
        "pager did not exit after repeated double Ctrl+C\nscreen:\n{}",
        pager.screen_contents()
    )
}
