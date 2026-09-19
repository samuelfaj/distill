// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E: the child network seccomp filter denies connecting to a unix socket regardless of when the socket was created.
//! The launch-time socket bind masks only cover endpoints that existed at startup.
//! So this per-spawn filter is what holds across daemon start and unlink/recreate.

#![cfg(target_os = "linux")]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const SOCKET_ENV: &str = "CHILD_NET_E2E_SOCKET";

#[test]
#[ignore]
fn subprocess_entry() {
    let Ok(path) = std::env::var(SOCKET_ENV) else {
        return;
    };
    match std::os::unix::net::UnixStream::connect(&path) {
        Ok(_) => std::process::exit(10),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => std::process::exit(11),
        Err(e) => {
            eprintln!("unexpected connect error: {e}");
            std::process::exit(12);
        }
    }
}

fn spawn_probe(socket: &Path, filtered: bool) -> std::process::Output {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.env(SOCKET_ENV, socket)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"]);
    if filtered {
        // Built in the parent, as production spawns do: the post-fork install must not allocate
        let filter = distill_sandbox::child_net::prebuilt_child_network_filter();
        // SAFETY: the closure only runs prctl against the parent-built program.
        unsafe {
            cmd.pre_exec(move || distill_sandbox::child_net::install_child_network_filter(filter));
        }
    }
    cmd.output().expect("spawn probe")
}

fn unique_socket_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grok-child-net-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create socket dir");
    dir.join("api.sock")
}

#[test]
fn filtered_child_cannot_connect_to_socket_created_after_launch() {
    // The socket appears only now, after this process (the "session") started
    // That mirrors a daemon that starts or unlink/recreates its endpoint mid-session, which no launch-time bind mask can cover
    let sock = unique_socket_path();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");

    let denied = spawn_probe(&sock, true);
    assert_eq!(
        denied.status.code(),
        Some(11),
        "filtered child must get EPERM on connect; stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    // Control: without the filter the same connect succeeds, proving the denial above comes from the filter, not the environment
    let allowed = spawn_probe(&sock, false);
    assert_eq!(
        allowed.status.code(),
        Some(10),
        "unfiltered control child must connect; stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );

    let _ = std::fs::remove_dir_all(sock.parent().unwrap());
}

/// Installing a network-restricting profile without a kernel apply must enable `restrict_child_network_std` and deny the child's connect.
/// `restrict_child_network_std` is the exact call the LSP client, notification hooks, and `.envrc` evaluators make.
/// This covers the degraded state (Landlock unavailable, apply failed) where the per-spawn filter is the only remaining child-network control.
#[test]
fn restrict_child_network_std_arms_from_config_without_apply() {
    let manager = distill_sandbox::SandboxManager::new(
        distill_sandbox::ProfileName::ReadOnly,
        Path::new("/tmp"),
    );
    assert!(!manager.is_applied(), "no kernel apply in this test");
    manager.install();
    assert!(
        distill_sandbox::should_restrict_child_network(),
        "resolved restrict_network alone must arm the filter"
    );

    let sock = unique_socket_path();
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.env(SOCKET_ENV, &sock)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"]);
    distill_sandbox::child_net::restrict_child_network_std(&mut cmd);
    let denied = cmd.output().expect("spawn probe");
    assert_eq!(
        denied.status.code(),
        Some(11),
        "std-wrapped child must get EPERM on connect; stderr: {}",
        String::from_utf8_lossy(&denied.stderr)
    );

    let _ = std::fs::remove_dir_all(sock.parent().unwrap());
}
