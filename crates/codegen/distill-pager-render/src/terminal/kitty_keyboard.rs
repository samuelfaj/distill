// Modified for Distill by Samuel Fajreldines, 2026.
//! Which Kitty keyboard enhancement flags the pager negotiates at startup, and the process-global record of the set it actually pushed.
//!
//! "Flags pushed" is not "releases arrive", and conflating them is the bug this module exists to prevent.
//! Alacritty 0.14.x and older is pushed `DISAMBIGUATE_ESCAPE_CODES` without `REPORT_EVENT_TYPES`, so the protocol is live and teardown owes a pop.
//! `Shift+Enter` works, yet no release ever comes, and a hold-to-talk started there could only end on Esc.
//!
//! The gate reads the reported version rather than watching behaviour because there is nothing to watch.
//! A conforming terminal reports no release for these keys either (kitty spec).
//! So healthy and broken differ by one byte per keystroke, gone by the time events are decoded.
//! An earlier design died on it.
//!
//! Deliberately uncovered: [`super::da2`] is skipped under CSI-intercepting multiplexers but [`super::TerminalContext::kitty_skip_reason`] is not.
//! So an affected Alacritty inside tmux 3.3 or newer answers nothing, keeps `REPORT_EVENT_TYPES` and still double-submits Enter.
//! Downgrading everything that answers nothing would cost far more healthy sessions than that slice.
//!
//! What losing `REPORT_EVENT_TYPES` costs an affected session:
//!
//! - Voice hold-to-talk degrades to a tap toggle (`voice_chord_action`; the `voice_capture_mode` setting hides its `hold` choice).
//! - `is_link_modifier_for_key`'s non-macOS Ctrl-release case (`distill-pager` `src/app/agent_view/mod.rs`) never fires.
//!   So link-hover clears on the next non-Ctrl key instead of when Ctrl lifts.
//! - `KeyEventKind::Repeat` disappears: held keys arrive as repeated `Press`, as on every non-KKP terminal.
//!   But `is_pasteable_key_event` (`distill-pager` `src/app/event_loop.rs`) excludes `Repeat` on purpose.
//!   So auto-repeat counts toward paste coalescing again.

use std::sync::atomic::{AtomicU8, Ordering};

use crossterm::event::KeyboardEnhancementFlags;

/// Highest packed library version that emits a duplicate legacy release for Backspace/Tab/Enter/Escape.
/// Crossterm reads that as a second Press, so Enter submits twice. Threshold is the 0.14.0 library (`2401`); it only gets retired.
/// Git builds strip `-dev` before packing, so a pre-fix `0.24.2-dev` reports `2402` and escapes this gate.
pub const ALACRITTY_BROKEN_EVENT_TYPES_MAX_PACKED: u32 = 2401;

/// Empty means push nothing. Unknown version never downgrades — DA2 is often skipped, and only a positively identified broken version pays the cost.
/// No brand check: [`super::da2`]'s gate admits Alacritty alone, so widening that gate widens this one.
pub fn negotiated_kitty_flags(
    skip_reason: Option<&str>,
    da2_packed: Option<u32>,
) -> KeyboardEnhancementFlags {
    if skip_reason.is_some() {
        return KeyboardEnhancementFlags::empty();
    }
    let mut flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
    let mis_encodes_releases =
        da2_packed.is_some_and(|packed| packed <= ALACRITTY_BROKEN_EVENT_TYPES_MAX_PACKED);
    if !mis_encodes_releases {
        flags |= KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
    }
    flags
}

/// Bits actually pushed (`0` is empty), not a classification, so the predicates cannot drift.
/// Relaxed: no other memory is published, and readers are ordered after `init_terminal`.
static PUSHED_KITTY_FLAGS: AtomicU8 = AtomicU8::new(0);

/// The exact flag set `init_terminal` pushed; the suspend/resume path re-pushes this verbatim so the two can never drift.
pub fn pushed_kitty_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::from_bits_truncate(PUSHED_KITTY_FLAGS.load(Ordering::Relaxed))
}

pub fn set_pushed_kitty_flags(flags: KeyboardEnhancementFlags) {
    PUSHED_KITTY_FLAGS.store(flags.bits(), Ordering::Relaxed);
}

/// True only if flags were actually pushed. False means modified keys arrive as legacy bytes.
/// This is not "key releases arrive"; use [`kitty_releases_reported`].
pub fn kitty_flags_pushed() -> bool {
    !pushed_kitty_flags().is_empty()
}

/// Whether the terminal reports key *release* events.
/// Every feature that waits for a release (hold-to-talk, modifier-lift tracking) must gate on this, not on [`kitty_flags_pushed`].
/// The two differ on Alacritty 0.14.x and older.
pub fn kitty_releases_reported() -> bool {
    pushed_kitty_flags().contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
}

/// Whether the version workaround engaged: pushed, but without `REPORT_EVENT_TYPES`.
/// Not `!kitty_releases_reported()`, which is also true when nothing was pushed at all.
pub fn kitty_event_types_withheld() -> bool {
    let flags = pushed_kitty_flags();
    !flags.is_empty() && !flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES)
}

/// Clears the record as it reads, so concurrent teardown paths cannot both pop.
pub fn take_kitty_flags_pushed() -> bool {
    PUSHED_KITTY_FLAGS.swap(0, Ordering::Relaxed) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISAMBIGUATE: KeyboardEnhancementFlags =
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
    const EVENT_TYPES: KeyboardEnhancementFlags = KeyboardEnhancementFlags::REPORT_EVENT_TYPES;

    /// The exact boundary a careless `<`/`<=` edit breaks: `alacritty_terminal` 0.24.1 is Alacritty 0.14.0 (broken), 0.24.2 is 0.15.0 (fixed).
    #[test]
    fn downgrade_boundary_is_the_last_broken_library_version() {
        // Non-empty either side: a downgrade is still a push, teardown owes a pop.
        assert_eq!(negotiated_kitty_flags(None, Some(2401)), DISAMBIGUATE);
        assert_eq!(
            negotiated_kitty_flags(None, Some(2402)),
            DISAMBIGUATE | EVENT_TYPES
        );
    }

    /// DA2 is skipped under multiplexers and off unix, so "no answer" is the common case and must not cost a healthy terminal its release events.
    #[test]
    fn absent_version_does_not_downgrade() {
        assert_eq!(
            negotiated_kitty_flags(None, None),
            DISAMBIGUATE | EVENT_TYPES
        );
    }

    /// A skip reason outranks any version, so teardown owes no pop.
    #[test]
    fn a_skip_reason_pushes_nothing() {
        for packed in [None, Some(2401), Some(2402)] {
            assert_eq!(
                negotiated_kitty_flags(Some("vscode"), packed),
                KeyboardEnhancementFlags::empty(),
                "da2_packed={packed:?}"
            );
        }
    }
}
