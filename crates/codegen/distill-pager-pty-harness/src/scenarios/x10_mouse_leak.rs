//! Relay-mangled X10 mouse reports must not type into the composer, and refocus must re-assert mouse capture.
//!
//! This is a regression test for the ConPTY/WSL right-margin leak.
//! A relay that converts the byte stream char-wise to UTF-8 expands X10 coordinate bytes >= 0x80 (columns >= 95) into two bytes.
//! Crossterm mis-parses the report into an impossible mouse event plus the displaced row byte as a plain typed character.
//! Vertical mouse motion at the right margin typed ramping ASCII into the prompt.
//! The pager's `X10ReassemblyFilter` recombines the pair into the true mouse event.
//! `Event::FocusGained` re-asserts the mouse DECSETs so relays that strip DEC private modes are nudged back to SGR reporting.

use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::wait_for_welcome;
use crate::{ContentController, PtyHarness, pager_binary};

const DEFAULT_ROWS: u16 = 50;
const DEFAULT_COLS: u16 = 120;

/// The ASCII ramp the unfixed parser leaks into the composer: one row byte per any-motion report, rows 47..=57 (bytes 0x50..=0x5A).
const LEAKED_RAMP: &str = "PQRSTUVWXYZ";

/// `EnableMouseCapture`'s any-motion DECSET; startup emits it once, so the refocus assertion must only search output produced after the focus-in.
const ANY_MOTION_ENABLE: &[u8] = b"\x1b[?1003h";

/// Drive both X10-leak defenses in one pager session: mangled reports parse as mouse events (nothing typed), and focus-in re-emits the mouse DECSETs.
pub async fn assert_x10_leak_defenses() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start ContentController")?;
    let binary = pager_binary().context("resolve pager binary")?;
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .context("spawn pager")?;

    wait_for_welcome(&mut harness).await?;

    // Positive control: the composer is live and echoes typed text, so the negative assertion below is meaningful
    harness.inject_keys(b"abc").context("type control text")?;
    harness
        .wait_for_text("abc", Duration::from_secs(10))
        .context("control text visible in composer")?;

    // A vertical mouse sweep at column 100 as a UTF-8-converting relay delivers it:
    // CB 0x43 (any-motion, no button), Cx 0x84 expanded to C2 84, Cy ramping 0x50..=0x5A
    let mut sweep = Vec::new();
    for row_byte in LEAKED_RAMP.bytes() {
        sweep.extend_from_slice(b"\x1b[MC\xC2\x84");
        sweep.push(row_byte);
    }
    harness
        .inject_keys(&sweep)
        .context("inject mangled X10 sweep")?;

    // Ordering sentinel: input is processed in order
    // Once the trailing "xyz" renders, any characters the sweep leaked would be rendered too (between "abc" and "xyz")
    // Waiting for the ramp to be *absent* alone would pass vacuously before the leak renders
    harness.inject_keys(b"xyz").context("type sentinel text")?;
    harness
        .wait_for_text("xyz", Duration::from_secs(10))
        .context("sentinel text visible in composer")?;
    if !harness.contains_text("abcxyz") || harness.contains_text(&LEAKED_RAMP[..3]) {
        let composer_row = harness
            .screen_contents()
            .lines()
            .find(|l| l.contains("abc"))
            .unwrap_or_default()
            .trim()
            .to_string();
        bail!("the X10 sweep typed into the composer (composer row: {composer_row:?})");
    }

    // Refocus re-assert: focus-out then focus-in must re-emit the mouse DECSETs (only inspect output produced after the focus-in)
    let before = harness.raw_output().len();
    harness
        .inject_keys(b"\x1b[O\x1b[I")
        .context("inject focus-out/focus-in reports")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        harness.update(Duration::from_millis(50));
        let after = &harness.raw_output()[before..];
        if after
            .windows(ANY_MOTION_ENABLE.len())
            .any(|w| w == ANY_MOTION_ENABLE)
        {
            break;
        }
        if std::time::Instant::now() > deadline {
            bail!("focus-in did not re-assert mouse capture (no ?1003h after \\x1b[I)");
        }
    }

    harness.quit().context("quit pager")?;
    Ok(())
}
