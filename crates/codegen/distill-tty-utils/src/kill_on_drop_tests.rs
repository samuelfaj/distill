use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::KillOnDrop;

fn spawn_sleeper() -> std::process::Child {
    let mut cmd = Command::new("sleep");
    cmd.arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::detach_std_command(&mut cmd);
    #[allow(clippy::disallowed_methods)] // test fixture; guarded/reaped by the test
    cmd.spawn().expect("spawn sleeper")
}

/// Zombie-tolerant bounded probe: the contract is that the child stops
/// *running*; whether the corpse is reaped promptly is environmental.
fn assert_stops_running(pid: u32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !crate::process_not_running(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        crate::process_not_running(pid),
        "{what} still running after drop"
    );
}

#[test]
fn drop_kills_and_reaps_the_child() {
    let guard = KillOnDrop::new(spawn_sleeper());
    let pid = guard.id();
    assert!(!crate::process_not_running(pid), "sanity: sleeper running");

    drop(guard);

    assert_stops_running(pid, "KillOnDrop child");
}

#[test]
fn into_inner_disarms_without_killing() {
    let guard = KillOnDrop::new(spawn_sleeper());
    let pid = guard.id();

    let mut child = guard.into_inner();

    assert!(
        !crate::process_not_running(pid),
        "into_inner must release the child without killing it"
    );
    child.kill().expect("kill released child");
    child.wait().expect("reap released child");
}

#[test]
fn drop_after_in_handle_reap_is_a_no_op() {
    let mut cmd = Command::new("true");
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::detach_std_command(&mut cmd);
    #[allow(clippy::disallowed_methods)] // test fixture; reaped through the guard
    let mut guard = KillOnDrop::new(cmd.spawn().expect("spawn true"));

    let status = guard.wait().expect("in-handle reap through the guard");
    assert!(status.success(), "true exits 0");

    // Drop after the in-handle reap must not panic and must not signal a
    // recycled PID (std's Child::kill refuses already-waited children).
    drop(guard);
}
