// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::atomic::AtomicBool;

use super::*;
use crate::app::teardown_fence::FenceDecision;

/// No tty I/O, and the pushed-flags global is never read (a parallel test sets it).
fn no_op_fence(reader: ReaderJoin, _writer_timed_out: bool) -> TeardownFenceReport {
    TeardownFenceReport {
        decision: FenceDecision::SkipNoFlags,
        fence: None,
        reader,
    }
}

/// The panic hook's teardown writes stay bounded so a wedged stderr lock cannot
/// keep the hook from restoring raw mode and reaching the delegated hook/abort.
#[test]
fn panic_teardown_is_bounded_when_the_stderr_lock_is_wedged() {
    fn takes_the_lock() {
        let _guard = distill_shell::util::stderr_lock();
    }
    // Park a fake writer thread on the lock for the whole call.
    let _guard = distill_shell::util::stderr_lock();
    let started = std::time::Instant::now();
    run_bounded_teardown(takes_the_lock, std::time::Duration::from_millis(100));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "bounded teardown must give up on a wedged stderr lock"
    );
}

fn test_terminal_and_writer_thread() -> (PagerTerminal, WriterThread) {
    // These tests never draw, so the frame receiver can drop here.
    let (terminal, _frame_rx) = crate::test_util::test_terminal();
    let (writer_tx, _writer_sync, _events, writer_thread) =
        crate::render::draw::spawn_writer_thread().expect("spawn test writer thread");
    drop(writer_tx);
    (terminal, writer_thread)
}

/// The fence runs after teardown: the pop must be on the wire before the DA1 query.
#[test]
fn restore_runs_teardown_even_when_writer_failed() {
    distill_telemetry::unified_log::redirect_to_temp_for_tests();
    let (terminal, writer_thread) = test_terminal_and_writer_thread();
    let teardown_called = std::sync::Arc::new(AtomicBool::new(false));
    let observed = std::sync::Arc::clone(&teardown_called);
    let teardown_before_fence = std::sync::Arc::clone(&teardown_called);

    let result = restore_terminal_with(
        terminal,
        writer_thread,
        ReaderThread::detached(),
        ScreenMode::Inline,
        |terminal, writer_thread| {
            drop(terminal);
            drop(writer_thread);
            Err(io::Error::other("injected drain failure"))
        },
        move |_, _| observed.store(true, Ordering::Release),
        move |reader, writer_timed_out| {
            assert!(teardown_before_fence.load(Ordering::Acquire));
            assert_eq!((ReaderJoin::Absent, false), (reader, writer_timed_out));
            no_op_fence(reader, writer_timed_out)
        },
    );

    assert!(result.is_err());
    assert!(teardown_called.load(Ordering::Acquire));
}

/// A timed-out writer join leaves the writer thread possibly parked on the stderr lock,
/// so the teardown that follows must be bounded: `/quit` returns even if teardown wedges.
#[test]
fn restore_bounds_teardown_after_a_timed_out_writer_join() {
    distill_telemetry::unified_log::redirect_to_temp_for_tests();
    let (terminal, writer_thread) = test_terminal_and_writer_thread();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();

    let started = std::time::Instant::now();
    let result = restore_terminal_with(
        terminal,
        writer_thread,
        ReaderThread::detached(),
        ScreenMode::Inline,
        |terminal, writer_thread| {
            drop(terminal);
            drop(writer_thread);
            Ok(WriterJoin::TimedOut)
        },
        // Stands in for a teardown wedged on the stderr lock held by the parked writer.
        move |_, _| {
            let _ = release_rx.recv();
        },
        // The fence must learn about the wedged writer so it skips the round trip nobody would answer
        |reader, writer_timed_out| {
            assert!(writer_timed_out);
            no_op_fence(reader, writer_timed_out)
        },
    );

    // The timeout is surfaced so the caller skips the post-teardown stderr writes.
    assert!(matches!(result, Ok(WriterJoin::TimedOut)));
    assert!(
        started.elapsed() < TEARDOWN_GRACE + std::time::Duration::from_secs(5),
        "restore must give up on a wedged teardown after TEARDOWN_GRACE"
    );
    let _ = release_tx.send(());
}
