// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
// Unix only: the blocked-write mechanics (dup'd tty fd, FIONREAD) have no Windows analogue.
#![cfg(unix)]
#[allow(unused_imports)]
use crate::common::*;

use std::io::{Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upper bound on waiting for the writer to park. Must stay well under the blocked-report
/// deadline (`WRITER_BLOCKED_WARN_AFTER`, 5 s) so FocusGained is injected before the
/// marker can fire; the pre-injection assertion below guards that ordering.
const PARK_DEADLINE: Duration = Duration::from_secs(4);

/// Wait until the writer thread is parked in its tty write.
fn wait_until_writer_parks(master_fd: RawFd) {
    let deadline = Instant::now() + PARK_DEADLINE;
    while Instant::now() < deadline {
        let mut pfd = libc::pollfd {
            fd: master_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll reads one pollfd we own for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready > 0 && pfd.revents & libc::POLLIN != 0 {
            std::thread::sleep(Duration::from_millis(300));
            return;
        }
    }
    panic!("pty output never backed up while streaming: the writer did not park");
}

/// All pty output captured so far plus the drain gate. While the gate is
/// closed the reader thread parks WITHOUT reading, so pty output backs up
/// exactly like a terminal that stopped consuming.
struct PtyTap {
    output: Mutex<Vec<u8>>,
    draining: AtomicBool,
}

impl PtyTap {
    fn wait_for_bytes(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            {
                let output = self.output.lock().expect("tap lock");
                if output.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }
}

/// Regression test for the blocked-tty mid-turn freeze: pre-fix, a terminal that stopped reading
/// the pty parked the writer thread in its blocking write holding the stderr lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn writer_blocked_tty_keeps_loop_alive() {
    let content = ContentController::start().await.expect("start content");
    let long_body = (0..2000)
        .map(|i| format!("{MOCK_RESPONSE_SENTINEL} stream line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    content.set_response(long_body);
    // Paced chunks keep the turn (and its frame flow) alive for the whole scenario
    content.set_chunk_delay(Some(Duration::from_millis(15)));

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let binary = pager_binary().expect("resolve pager binary");
    let mut cmd = portable_pty::CommandBuilder::new(&binary);
    cmd.cwd(content.sandbox().home());
    // Hermetic like every harness spawn: ambient vars (GROK_SCROLL_LOG, TMUX, ...) must not
    // leak state outside the sandbox or shift which escape-emission path the test exercises.
    cmd.env_clear();
    for (key, value) in content.sandbox().env() {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    // Direct spawn (not `PtyHarness`): its reader thread drains the pty eagerly,
    // and this test needs the pty to back up. Enroll the child so it is reaped
    // on any failure path, same as the harness's own spawn.
    #[allow(clippy::disallowed_methods)]
    let mut child = pair.slave.spawn_command(cmd).expect("spawn pager");
    let _process_tree = child
        .process_id()
        .map(|pid| distill_test_support::TestProcessTree::attach(pid, "grok PTY child"));
    drop(pair.slave);

    let master_fd = pair.master.as_raw_fd().expect("pty master fd");
    let mut reader = pair.master.try_clone_reader().expect("clone reader");
    let mut writer = pair.master.take_writer().expect("take writer");

    let tap = Arc::new(PtyTap {
        output: Mutex::new(Vec::new()),
        draining: AtomicBool::new(true),
    });
    let reader_tap = tap.clone();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            if !reader_tap.draining.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    reader_tap
                        .output
                        .lock()
                        .expect("tap lock")
                        .extend_from_slice(&buf[..n]);
                }
            }
        }
    });

    // Boot to the welcome screen, submit the prompt, confirm streaming reaches
    // the screen — same sentinels as the harness-based siblings.
    assert!(
        tap.wait_for_bytes(WELCOME_SCREEN_SENTINEL, Duration::from_secs(45)),
        "welcome screen never rendered"
    );
    writer
        .write_all(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    writer.flush().expect("flush prompt");
    assert!(
        tap.wait_for_bytes(MOCK_RESPONSE_SENTINEL, Duration::from_secs(45)),
        "mock response never reached the pty output"
    );

    // Stop draining mid-stream and wait for the writer thread to park in its tty write.
    tap.draining.store(false, Ordering::Release);
    wait_until_writer_parks(master_fd);

    // The report must not have fired yet (see PARK_DEADLINE): a pre-injection marker
    // would make the loop-alive check below pass vacuously, green even under a binary
    // where FocusGained wedges the loop.
    let log_path = unified_log_path(&content);
    let log_at_injection = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log_at_injection.contains("term.writer.blocked"),
        "term.writer.blocked logged before FocusGained was injected; the loop-alive \
         proof would be vacuous. Lower PARK_DEADLINE below WRITER_BLOCKED_WARN_AFTER."
    );

    // The pre-fix deadlock trigger: FocusGained took the stderr lock inline.
    writer.write_all(b"\x1b[I").expect("inject FocusGained");
    writer.flush().expect("flush FocusGained");

    // Loop-alive proof: the blocked-writer report fires from the event loop's
    // own select arm, after FocusGained was handled, while the pty stays unread.
    let report_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("term.writer.blocked") {
            break;
        }
        assert!(
            Instant::now() < report_deadline,
            "term.writer.blocked never logged: the event loop wedged while the tty was blocked \
             (pre-fix stderr-lock deadlock). Log so far:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // Recovery: the terminal reads again, the stuck frame acks, the loop logs it.
    tap.draining.store(true, Ordering::Release);
    let recovery_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("term.writer.recovered") {
            break;
        }
        assert!(
            Instant::now() < recovery_deadline,
            "term.writer.recovered never logged after the pty resumed draining. Log so far:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }

    // Clean quit end-to-end (Ctrl-U clears any typed residue first).
    writer.write_all(b"\x15/quit\r").expect("send /quit");
    writer.flush().expect("flush /quit");
    let quit_deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        assert!(
            Instant::now() < quit_deadline,
            "pager did not exit after /quit"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(status.success(), "pager exited non-zero: {status:?}");

    drop(writer);
    drop(pair.master);
    let _ = reader_thread.join();
}
