// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression tests: a test process that dies without running any userspace
//! cleanup must not leak its PTY child.
//!
//! Context: PTY e2e children used to survive an ungraceful test-runner death
//! (CI timeouts deliver SIGTERM then SIGKILL — neither unwinds, so no Drop
//! runs), re-parent to init, and accumulate on CI hosts. The fix arms
//! `PR_SET_PDEATHSIG(SIGKILL)` on the child at spawn, so the *kernel* reaps it
//! when the spawning test process dies. Residual gap: pdeathsig covers only
//! the direct PTY child — on a SIGKILLed runner the pager's own Drop/quit
//! never runs, so live tool subprocesses it spawned can still re-parent and
//! outlive it (the group kill covers them only on userspace teardown paths).
//! That net is Linux-only; on other Unix
//! hosts only the Drop path (covered by `pty_drop_tree_cleanup_is_bounded_and_
//! reaps_grandchild` in `src/pty.rs`) protects, so these tests are
//! Linux-gated.
#![cfg(target_os = "linux")]

use std::io::{BufRead as _, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Running (not gone, not a zombie). Zombie-tolerant because the killed child
/// re-parents to pid 1 and whether that init reaps it promptly is
/// environmental; the harness's contract is that it stops *running*.
fn pid_is_running(pid: u32) -> bool {
    !distill_tty_utils::process_not_running(pid)
}

fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit(')').next()?.trim_start();
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn original_child_running(pid: u32, starttime: u64) -> bool {
    proc_starttime(pid) == Some(starttime) && pid_is_running(pid)
}

/// Holder-fixture binary: cargo sets `CARGO_BIN_EXE_pty_orphan_holder`; Bazel
/// wires `PTY_ORPHAN_HOLDER_BIN` (runfiles-relative, hence absolutize).
fn holder_binary() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("PTY_ORPHAN_HOLDER_BIN") {
        return std::path::absolute(&path).expect("absolutize PTY_ORPHAN_HOLDER_BIN");
    }
    if let Some(path) = option_env!("CARGO_BIN_EXE_pty_orphan_holder") {
        return path.into();
    }
    panic!(
        "pty_orphan_holder not resolvable: set PTY_ORPHAN_HOLDER_BIN (Bazel) or run via \
         cargo test (CARGO_BIN_EXE_pty_orphan_holder)"
    );
}

/// Spawn the holder fixture, read the PTY child PID it prints, kill the holder
/// with `signal` (bypassing all userspace cleanup), and assert the PTY child
/// is gone within a bounded wait.
fn assert_no_orphan_after_holder_killed_by(signal: libc::c_int) {
    // The holder is this crate's own spawn path: it creates a PtyController
    // exactly like every PTY e2e test does.
    let mut cmd = Command::new(holder_binary());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // Detach from the test runner's controlling TTY (workspace spawn hygiene;
    // composable with the pdeathsig arm below — setsid does not clear it).
    distill_tty_utils::detach_std_command(&mut cmd);
    // The regression test must not itself leak: if THIS test process dies
    // ungracefully while blocked below, the kernel takes the holder (and the
    // holder's death takes its pdeathsig-armed PTY child).
    distill_tty_utils::kill_on_parent_death_std(&mut cmd);
    // Killed+reaped on drop: an assertion failure between spawn and the
    // explicit kill (bad stdout, parse failure, sanity assert) must not leak
    // the holder's infinite loop either.
    #[allow(clippy::disallowed_methods)] // guarded by KillOnDrop below
    let mut holder =
        distill_tty_utils::KillOnDrop::new(cmd.spawn().expect("spawn pty_orphan_holder fixture"));

    // Bounded read of the CHILD_PID line: a wedged holder must fail the test
    // (and be killed by the guard), not hang it. The reader thread exits on
    // pipe EOF once the holder dies.
    let stdout = holder.stdout.take().expect("holder stdout pipe");
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = line_tx.send(BufReader::new(stdout).lines().next());
    });
    let child_pid: u32 = line_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("holder printed CHILD_PID within 30s")
        .expect("holder printed a line")
        .expect("read holder stdout")
        .strip_prefix("CHILD_PID=")
        .expect("holder line format")
        .parse()
        .expect("holder child pid");

    assert!(
        pid_is_running(child_pid),
        "sanity: PTY child {child_pid} must be running while the holder runs"
    );
    let starttime = proc_starttime(child_pid).expect("PTY child /proc/pid/stat starttime");

    // Ungraceful death: SIGTERM/SIGKILL terminate the holder without
    // unwinding, so no Drop and no other userspace teardown runs.
    // SAFETY: holder.id() is a live direct child of this test.
    let rc = unsafe { libc::kill(holder.id() as libc::pid_t, signal) };
    assert_eq!(rc, 0, "deliver signal {signal} to holder");
    holder.wait().expect("reap holder");

    let deadline = Instant::now() + Duration::from_secs(5);
    while original_child_running(child_pid, starttime) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !original_child_running(child_pid, starttime),
        "PTY child {child_pid} (starttime {starttime}) survived the holder's \
         ungraceful death (signal {signal}) — orphan leak"
    );
}

#[test]
fn sigkilled_test_process_leaves_no_pty_child() {
    assert_no_orphan_after_holder_killed_by(libc::SIGKILL);
}

#[test]
fn sigtermed_test_process_leaves_no_pty_child() {
    assert_no_orphan_after_holder_killed_by(libc::SIGTERM);
}
