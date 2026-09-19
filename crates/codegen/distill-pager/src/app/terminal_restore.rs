// Modified for Distill by Samuel Fajreldines, 2026.
//! Terminal teardown shared by the post-loop restore, the panic hook and the signal path.
//!
//! [`emit_terminal_teardown_sequences`] defines the on-wire teardown byte order exactly once; the kitty keyboard pop
//! inside it happens at most once per push. The panic hook and a restore whose writer join timed out run it through
//! [`run_bounded_teardown`]; the signal path calls it directly, without draining the writer.
//! The post-loop restore alone joins the stdin reader first and fences the pop with a DA1 round trip before raw mode ends.

use std::io::{self, Write};
use std::panic;
use std::sync::atomic::Ordering;

use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{self, LeaveAlternateScreen};

use crate::app::reader_thread::{READER_JOIN_GRACE, ReaderJoin, ReaderThread};
use crate::app::teardown_fence::{TeardownFence, TeardownFenceReport};
use crate::app::{
    CURSOR_STYLE_FORCED, MOUSE_CAPTURE_ENABLED, ScreenMode, current_screen_mode,
    pop_gboom_keyboard_flags_inline, signal_handler,
};
use crate::render::draw::{PagerTerminal, WriterJoin, WriterThread};

/// How long teardown waits for the writer thread to drain before detaching it. Same order as the panic hook's grace: a terminal that stopped reading must not turn `/quit` into a hang.
/// the panic hook's grace: a terminal that stopped reading must not turn `/quit` into a hang.
const WRITER_JOIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Drop the terminal (closing the writer mpsc channel) and join the writer thread within
/// [`WRITER_JOIN_GRACE`]. Every `EscapeWriter` clone is already gone here: they all live in `AppView`, which is local to `event_loop::run` and dropped when it returns.
/// A `TimedOut` join means the writer thread may still hold the stderr lock inside its tty write, so the caller must not take that lock unbounded.
fn drain_writer_thread_before_teardown(
    terminal: PagerTerminal,
    writer_thread: WriterThread,
) -> io::Result<WriterJoin> {
    drop(terminal);
    let join = writer_thread.join_within(WRITER_JOIN_GRACE)?;
    if join == WriterJoin::TimedOut {
        crate::unified_log::warn(
            "term.writer.join_timeout",
            None,
            Some(serde_json::json!({ "grace_ms": WRITER_JOIN_GRACE.as_millis() as u64 })),
        );
    }
    Ok(join)
}

/// Write raw CSI sequences to disable mouse tracking and bracketed paste.
///
/// Best-effort: failures are silently ignored since this runs on teardown and panic paths where stderr may already be broken.
fn disable_mouse_paste_raw() {
    distill_shell::util::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(distill_crash_handler::terminal::MOUSE_PASTE_RESET);
        let _ = stderr.flush();
    });
}

/// Shared by `restore_terminal` and `set_panic_hook` so the on-wire byte order is defined exactly once.
/// Does NOT call `disable_raw_mode`.
/// Callers should drain queued writer-thread frames first when possible; the panic hook can't (it would deadlock).
pub(super) fn emit_terminal_teardown_sequences(mode: ScreenMode, inline_cursor_row: Option<u16>) {
    #[cfg(windows)]
    use crate::app::win_native_selection;

    // Clear the OSC 9;4 progress bar unconditionally
    // Emitting a no-op clear to terminals that don't support it is harmless
    // This path runs from signal/panic handlers that cannot access NotificationService
    distill_shell::util::with_locked_stderr(|stderr| {
        let _ = stderr.write_all(crate::notifications::progress::OSC_CLEAR.as_bytes());
        let _ = stderr.flush();
    });

    distill_shell::util::with_locked_stderr(|stderr| {
        let _ = execute!(stderr, crossterm::terminal::EndSynchronizedUpdate);
    });
    crate::theme::reset_cursor_color_if_applied();

    // https://github.com/helix-editor/helix/issues/6638
    disable_mouse_paste_raw();
    if MOUSE_CAPTURE_ENABLED.swap(false, Ordering::AcqRel) {
        // On Windows the enable was a winapi SetConsoleMode replace (crossterm never emits the ?100x escapes there)
        // Conhost keeps console modes across process exit
        // Restore via crossterm's winapi path so a fullscreen/inline run doesn't leave the window with QuickEdit off
        #[cfg(windows)]
        distill_shell::util::with_locked_stderr(|stderr| {
            let _ = execute!(stderr, event::DisableMouseCapture);
        });
    }
    distill_shell::util::with_locked_stderr(|stderr| {
        let _ = execute!(stderr, event::DisableFocusChange);
    });

    // Per the kitty spec the pop must happen at most once per push and on the same screen
    // Use swap so concurrent teardown paths (panic hook, restore_terminal) cannot both pop
    // Pop the /gboom layer first (it sits on top of the base layer) if the game was still open
    pop_gboom_keyboard_flags_inline();
    if crate::terminal::take_kitty_flags_pushed() {
        distill_shell::util::with_locked_stderr(|stderr| {
            let _ = execute!(stderr, event::PopKeyboardEnhancementFlags);
        });
    }

    // Reset the cursor style only if startup forced one (`CURSOR_STYLE_FORCED`); under inherit a `0 q` would clobber a style the pager never touched
    let restore_style = CURSOR_STYLE_FORCED.load(Ordering::Acquire);
    if mode.is_fullscreen() {
        distill_shell::util::with_locked_stderr(|stderr| {
            if restore_style {
                let _ = execute!(stderr, SetCursorStyle::DefaultUserShape);
            }
            let _ = execute!(stderr, cursor::Show, LeaveAlternateScreen);
        });
    } else {
        let rows = crossterm::terminal::size().map(|(_, r)| r).unwrap_or(24);
        let last = rows.saturating_sub(1);
        // In minimal mode the viewport is not bottom-pinned, so moving to the screen bottom would strand the shell prompt / resume hint
        // They would sit far below the pager's last line with a screen of blank space between
        // `inline_cursor_row` (the live viewport's bottom) lands it directly under the prompt instead
        let target = inline_cursor_row.unwrap_or(last).min(last);
        distill_shell::util::with_locked_stderr(|stderr| {
            if restore_style {
                let _ = execute!(stderr, SetCursorStyle::DefaultUserShape);
            }
            let _ = execute!(stderr, cursor::MoveTo(0, target), cursor::Show);
            let _ = writeln!(stderr);
            let _ = stderr.flush();
        });
    }

    // Restore the stdin console mode changed by minimal-mode's native-selection setup (no-op if it never ran)
    // Last on purpose: it is the outermost snapshot
    // On the minimal-to-inline downgrade path the crossterm DisableMouseCapture above restores to the mode *including* our QuickEdit assert
    #[cfg(windows)]
    win_native_selection::restore_stdin_mode();
}

/// Bound on teardown writes when the stderr lock may be wedged: the panic hook, and a restore
/// whose writer thread is still parked in its tty write after a timed-out join.
const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Teardown still runs if draining fails, so terminal state is restored before returning that error.
/// Draining first prevents a late frame after `LeaveAlternateScreen`; `fence` runs after teardown, still in raw mode, and
/// receives the reader join plus whether the writer join timed out (production builds a `TeardownFence` from them).
fn restore_terminal_with(
    mut terminal: PagerTerminal,
    writer_thread: WriterThread,
    reader_thread: ReaderThread,
    mode: ScreenMode,
    drain: impl FnOnce(PagerTerminal, WriterThread) -> io::Result<WriterJoin>,
    teardown: impl FnOnce(ScreenMode, Option<u16>) + Send + 'static,
    fence: impl FnOnce(ReaderJoin, bool) -> TeardownFenceReport,
) -> io::Result<WriterJoin> {
    // Joined first so the fence is the sole stdin reader; `input_rx` died with the event loop, so this takes one poll cycle
    let reader = reader_thread.join_within(READER_JOIN_GRACE);
    if mode.is_fullscreen() && !writer_thread.writer_sync().failed() {
        let _ = terminal.clear();
        {
            use std::io::Write;
            let _ = terminal.backend_mut().flush();
        }
    }
    // Capture the live viewport's bottom row before dropping the terminal
    // Teardown can then place the cursor directly below the (non-bottom-pinned) live region rather than at the screen bottom
    let inline_cursor_row = (!mode.is_fullscreen()).then(|| terminal.viewport_area().bottom());
    let drain_result = drain(terminal, writer_thread);
    let writer_timed_out = matches!(drain_result, Ok(WriterJoin::TimedOut));
    if writer_timed_out {
        // The detached writer thread may still hold the stderr lock inside its tty write; an unbounded teardown here would turn a terminal that stopped reading into a /quit hang.
        // unbounded teardown here would turn a terminal that stopped reading into a /quit hang.
        run_bounded_teardown(move || teardown(mode, inline_cursor_row), TEARDOWN_GRACE);
    } else {
        teardown(mode, inline_cursor_row);
    }
    // Release events the terminal emitted before applying the pop may still be in flight; left alone they reach the shell as keystrokes
    // Still in raw mode: the fence reads the raw fd, and `disable_raw_mode` must follow the last read
    let report = fence(reader, writer_timed_out);
    let _ = terminal::disable_raw_mode();
    // Tell the signal handlers that the user's shell now owns the terminal
    // A SIGPIPE arriving on a late stderr write must not paint escape sequences into the user's prompt
    signal_handler::mark_restored();
    distill_crash_handler::disable_terminal_escape_restore();
    // Restore fd 2 to the real terminal so any post-TUI output (tracing flushes, Sentry flush, etc.) is visible
    distill_tty_utils::restore_native_stderr();
    report.record();
    drain_result
}

/// The `WriterJoin` tells the caller whether the terminal is still reading: after a `TimedOut` join every further stderr write blocks until the exit watchdog fires.
/// `TimedOut` join every further stderr write blocks until the exit watchdog fires.
pub(super) fn restore_terminal(
    terminal: PagerTerminal,
    writer_thread: WriterThread,
    reader_thread: ReaderThread,
    mode: ScreenMode,
) -> io::Result<WriterJoin> {
    // Read here, before `emit_terminal_teardown_sequences` swaps the record to zero while popping
    let flags_pushed = crate::terminal::kitty_flags_pushed();
    restore_terminal_with(
        terminal,
        writer_thread,
        reader_thread,
        mode,
        drain_writer_thread_before_teardown,
        emit_terminal_teardown_sequences,
        move |reader, writer_timed_out| {
            TeardownFence {
                reader,
                writer_timed_out,
                flags_pushed,
            }
            .run()
        },
    )
}

/// Run a best-effort teardown `f` on a helper thread, waiting at most `grace` for it.
/// For paths where the stderr lock may be wedged (the panic hook; a restore whose writer thread is still parked in its tty write): an unbounded teardown would hang forever, never restoring raw mode. On timeout the helper is detached; the process is exiting anyway. Runs `f` inline if no thread can spawn.
fn run_bounded_teardown(f: impl FnOnce() + Send + 'static, grace: std::time::Duration) {
    // Shared slot so the closure survives a failed spawn for the inline fallback.
    let slot = std::sync::Arc::new(parking_lot::Mutex::new(Some(f)));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let worker_slot = std::sync::Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("bounded-teardown".into())
        .spawn(move || {
            if let Some(f) = worker_slot.lock().take() {
                f();
            }
            let _ = done_tx.send(());
        });
    match spawned {
        Ok(_) => {
            let _ = done_rx.recv_timeout(grace);
        }
        Err(_) => {
            if let Some(f) = slot.lock().take() {
                f();
            }
        }
    }
}

/// Reads [`current_screen_mode`] at panic time; never capture a mode here, or an in-process mode switch tears down the wrong screen.
pub(super) fn set_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        run_bounded_teardown(
            || emit_terminal_teardown_sequences(current_screen_mode(), None),
            TEARDOWN_GRACE,
        );
        let _ = terminal::disable_raw_mode();
        signal_handler::mark_restored();
        distill_crash_handler::disable_terminal_escape_restore();
        distill_tty_utils::restore_native_stderr();
        distill_tty_utils::global_process_scope().kill_all();
        crate::memory_trace::record_crash_sample();
        hook(info);
    }));
}

#[cfg(test)]
#[path = "terminal_restore_tests.rs"]
mod tests;
