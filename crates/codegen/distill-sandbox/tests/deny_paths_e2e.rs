// Modified for Distill by Samuel Fajreldines, 2026.
//! E2E path-deny and Grok hook write-deny (subprocess; arm64-tagged).
//! Soft-skips when enforcement is unavailable; only `SANDBOX_E2E_REQUIRE_ENFORCEMENT` hard-requires a usable backend.
#![cfg(all(unix, feature = "enforce"))]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
const SCENARIO_ENV: &str = "SANDBOX_E2E_SCENARIO";
const WORKSPACE_ENV: &str = "SANDBOX_E2E_WORKSPACE";
const GROK_HOME_ENV: &str = "SANDBOX_E2E_GROK_HOME";
const HOME_ENV: &str = "SANDBOX_E2E_HOME";
const PROFILE_ENV: &str = "SANDBOX_E2E_PROFILE";
const TARGETS_ENV: &str = "SANDBOX_E2E_TARGETS";
const CONTROLS_ENV: &str = "SANDBOX_E2E_CONTROLS";
const POSTLAUNCH_ENV: &str = "SANDBOX_E2E_POSTLAUNCH";
/// Set when the spoof parent leaves `/data` on a read-only ancestor mount.
/// The subprocess then knows exact-mountpoint verification must reject the forgery.
const DATA_STAGED_ENV: &str = "SANDBOX_E2E_DATA_STAGED";
const MARKER: &str = "deny-paths-e2e-marker-9f3c1a";
const REQUIRE_ENV: &str = "SANDBOX_E2E_REQUIRE_ENFORCEMENT";
fn apply_fixture_env(cmd: &mut Command, home: &Path, distill_home: &Path, workspace: &Path) {
    cmd.env(WORKSPACE_ENV, workspace.as_os_str())
        .env(HOME_ENV, home.as_os_str())
        .env(GROK_HOME_ENV, distill_home.as_os_str())
        .env("HOME", home.as_os_str())
        .env("GROK_HOME", distill_home.as_os_str());
}
/// Re-invoke this test binary as a subprocess driving `profile` over `targets` (denied) and `controls` (must stay readable).
/// `postlaunch` paths are created AFTER apply to exercise the macOS runtime-regex (post-launch) coverage.
fn run_scenario(
    home: &Path,
    distill_home: &Path,
    workspace: &Path,
    profile: &str,
    targets: &[&str],
    controls: &[&str],
    postlaunch: &[&str],
) -> (std::process::ExitStatus, String) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    apply_fixture_env(&mut cmd, home, distill_home, workspace);
    let output = cmd
        .env(SCENARIO_ENV, "block_deny")
        .env(PROFILE_ENV, profile)
        .env(TARGETS_ENV, targets.join(","))
        .env(CONTROLS_ENV, controls.join(","))
        .env(POSTLAUNCH_ENV, postlaunch.join(","))
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .arg("subprocess_entry")
        .output()
        .expect("failed to spawn subprocess");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
/// Re-invoke as a subprocess for the direct-hook write-deny scenarios.
fn run_hook_write_deny_scenario(
    home: &Path,
    distill_home: &Path,
    workspace: &Path,
    scenario: &str,
) -> (std::process::ExitStatus, String) {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    apply_fixture_env(&mut cmd, home, distill_home, workspace);
    let output = cmd
        .env(SCENARIO_ENV, scenario)
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .arg("subprocess_entry")
        .output()
        .expect("failed to spawn subprocess");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}
/// Soft-skip when the platform cannot enforce kernel denials.
/// Only `SANDBOX_E2E_REQUIRE_ENFORCEMENT` hard-requires enforcement; generic CI/`GITHUB_ACTIONS` alone must not (remote arm64 may lack usable bwrap).
fn skip_if_enforcement_unavailable() -> bool {
    let require = std::env::var(REQUIRE_ENV).is_ok();
    let support = distill_sandbox::SandboxManager::support_info();
    if !support.is_supported {
        if require {
            panic!(
                "enforcement required ({REQUIRE_ENV}) but sandbox unsupported: {}",
                support.details
            );
        }
        eprintln!("skipping: sandbox not supported ({})", support.details);
        return true;
    }
    #[cfg(target_os = "linux")]
    if !bwrap_available() {
        if require {
            panic!(
                "enforcement required ({REQUIRE_ENV}) but bwrap unavailable \
                 (required for Linux path / hook write-deny)"
            );
        }
        eprintln!("skipping: bwrap not installed (required for Linux path / hook write-deny)");
        return true;
    }
    false
}
fn unique_temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "grok-sandbox-e2e-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dunce::canonicalize(&dir).expect("canonicalize temp dir")
}
/// Decode a comma-joined env list; empty or missing means an empty vec.
fn list_from_env(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}
fn is_permission_denied(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EACCES) | Some(libc::EPERM) | Some(libc::EROFS)
    )
}
/// On Linux bubblewrap, unlink of a read-only bind-mounted leaf can return EBUSY (ResourceBusy) rather than EACCES/EPERM; that is still a denial.
fn is_unlink_denied(e: &std::io::Error) -> bool {
    is_permission_denied(e) || e.raw_os_error() == Some(libc::EBUSY)
}
/// Rename of a RO bind-mount leaf/mountpoint can return EXDEV or EBUSY; that is still a denial (no destination created).
fn is_rename_denied(e: &std::io::Error) -> bool {
    is_permission_denied(e) || matches!(e.raw_os_error(), Some(libc::EXDEV) | Some(libc::EBUSY))
}
/// Spawn a child command and `exit(1)` if its stdout exposes the secret MARKER.
/// Asserts marker absence rather than a non-zero exit.
/// A root reader of the mode-000 placeholder gets empty output, which still means the path is shadowed.
fn assert_child_cannot_read(label: &str, program: &str, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {program}: {e}"));
    if String::from_utf8_lossy(&out.stdout).contains(MARKER) {
        eprintln!("FAIL: {label} exposed MARKER");
        std::process::exit(1);
    }
}
/// Assert a denied file's bytes are unreadable via an in-process read, a `cat` child, and a nested `sh -c "cat"` child.
/// The children mimic the `bash`/`grep` tools and the shell a subagent shells out through.
/// The property is MARKER absence; EACCES/EPERM or empty output under root all satisfy it.
fn assert_read_blocked(label: &str, path: &Path) {
    if let Ok(content) = fs::read_to_string(path)
        && content.contains(MARKER)
    {
        eprintln!("FAIL: {label} in-process read exposed MARKER");
        std::process::exit(1);
    }
    let s = path.display().to_string();
    assert_child_cannot_read(label, "cat", &[s.as_str()]);
    let sh_cmd = format!("cat '{s}'");
    assert_child_cannot_read(label, "sh", &["-c", sh_cmd.as_str()]);
    eprintln!("OK: {label} read blocked");
}
/// Assert a denied file cannot be overwritten: the write must fail with EACCES/EPERM.
/// A permitted write would enable the relocation bypass below.
fn assert_write_denied(label: &str, path: &Path) {
    match fs::write(path, "overwrite-attempt") {
        Err(e) if is_permission_denied(&e) => eprintln!("OK: {label} write denied"),
        Err(e) => {
            eprintln!("FAIL: unexpected {label} write error: {e}");
            std::process::exit(1);
        }
        Ok(()) => {
            eprintln!("FAIL: {label} write was permitted (relocation bypass possible)");
            std::process::exit(1);
        }
    }
}
/// Assert the `mv x y && cat y` relocation bypass does not expose the bytes.
/// The rename must fail (unlink of the source is denied) so the secret never lands at the destination.
fn assert_rename_bypass_blocked(label: &str, path: &Path, workspace: &Path) {
    let name = path.file_name().unwrap().to_string_lossy();
    let moved = workspace.join(format!("exfil-{name}"));
    let _ = fs::rename(path, &moved);
    match fs::read_to_string(&moved) {
        Ok(c) if c.contains(MARKER) => {
            eprintln!("FAIL: {label} rename bypass exposed MARKER");
            std::process::exit(1);
        }
        _ => eprintln!("OK: {label} rename bypass blocked"),
    }
}
#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    Command::new("bwrap")
        .args(["--bind", "/", "/", "--", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
/// The custom profile under test, read from the env the parent set.
fn profile_from_env() -> distill_sandbox::ProfileName {
    distill_sandbox::ProfileName::Custom(std::env::var(PROFILE_ENV).expect(PROFILE_ENV))
}
/// `#[ignore]`d: only runs when invoked by the parent test via `run_scenario` or `run_hook_write_deny_scenario`.
#[test]
#[ignore]
fn subprocess_entry() {
    let scenario = match std::env::var(SCENARIO_ENV) {
        Ok(s) => s,
        Err(_) => return,
    };
    let workspace = std::env::var(WORKSPACE_ENV).expect(WORKSPACE_ENV);
    let workspace = dunce::canonicalize(&workspace).expect("canonicalize workspace");
    let workspace = workspace.as_path();
    let home = PathBuf::from(std::env::var(HOME_ENV).expect(HOME_ENV));
    let distill_home = PathBuf::from(std::env::var(GROK_HOME_ENV).expect(GROK_HOME_ENV));
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("GROK_HOME", &distill_home);
    }
    match scenario.as_str() {
        "block_deny" => subprocess_block_deny(workspace),
        "hook_write_deny" => subprocess_hook_write_deny(workspace, false),
        "hook_write_deny_first_run" => subprocess_hook_write_deny(workspace, true),
        "hook_write_deny_marker_spoof" => subprocess_hook_write_deny_marker_spoof(&distill_home),
        "read_deny_marker_spoof" => subprocess_read_deny_marker_spoof(workspace),
        "read_deny_forged_mounts" => subprocess_read_deny_forged_mounts(workspace),
        "read_deny_empty_set" => subprocess_read_deny_empty_set(workspace),
        "devbox_marker_spoof" => subprocess_devbox_marker_spoof(workspace),
        "devbox_genuine" => subprocess_devbox_genuine(workspace),
        other => {
            eprintln!("unknown scenario: {other}");
            std::process::exit(99);
        }
    }
}
fn subprocess_profile_and_bwrap_reexec(profile: &distill_sandbox::ProfileName, workspace: &Path) {
    #[cfg(target_os = "linux")]
    {
        if !distill_sandbox::is_inside_bwrap() {
            match distill_sandbox::bwrap_reexec_for_profile(profile, workspace) {
                Some(mut cmd) => {
                    use std::os::unix::process::CommandExt;
                    let err = cmd.exec();
                    eprintln!("bwrap re-exec failed: {err}");
                    std::process::exit(2);
                }
                None => {
                    eprintln!("FAIL: bwrap_reexec_for_profile returned None outside bwrap");
                    std::process::exit(2);
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (profile, workspace);
    }
}
fn subprocess_block_deny(workspace: &Path) {
    let targets = list_from_env(TARGETS_ENV);
    let controls = list_from_env(CONTROLS_ENV);
    let profile = profile_from_env();
    subprocess_profile_and_bwrap_reexec(&profile, workspace);
    let mut sandbox = distill_sandbox::SandboxManager::new(profile.clone(), workspace);
    if let Err(e) = sandbox.apply(workspace) {
        eprintln!("sandbox apply failed: {e}");
        std::process::exit(3);
    }
    if !sandbox.is_applied() {
        eprintln!("sandbox was not applied (unsupported platform?)");
        std::process::exit(4);
    }
    #[cfg(target_os = "linux")]
    match distill_sandbox::verify_read_deny_enforced(&profile, workspace) {
        Ok(()) => eprintln!("OK: read-deny mounts verified"),
        Err(e) => {
            eprintln!("FAIL: read-deny verification must pass inside bwrap: {e}");
            std::process::exit(1);
        }
    }
    for rel in &targets {
        let path = workspace.join(rel);
        assert_read_blocked(rel, &path);
        #[cfg(target_os = "linux")]
        match fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)) {
            Err(e) if is_permission_denied(&e) => {
                eprintln!("OK: {rel} chmod denied by read-only mount")
            }
            Err(e) => {
                eprintln!("FAIL: unexpected {rel} chmod error: {e}");
                std::process::exit(1);
            }
            Ok(()) => {
                eprintln!("FAIL: {rel} chmod restored access");
                std::process::exit(1);
            }
        }
        assert_write_denied(rel, &path);
        assert_rename_bypass_blocked(rel, &path, workspace);
    }
    #[cfg(target_os = "linux")]
    {
        let unmount_rc = unsafe { libc::umount2(c"/".as_ptr(), libc::MNT_DETACH) };
        let unmount_error = std::io::Error::last_os_error();
        if unmount_rc != -1 || unmount_error.raw_os_error() != Some(libc::EPERM) {
            eprintln!(
                "FAIL: namespace unmount was not blocked by seccomp: rc={unmount_rc}, \
                 errno={unmount_error}"
            );
            std::process::exit(1);
        }
        eprintln!("OK: namespace unmount blocked by seccomp");
        let remount_rc = unsafe {
            libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT,
                std::ptr::null(),
            )
        };
        let remount_error = std::io::Error::last_os_error();
        if remount_rc != -1 || remount_error.raw_os_error() != Some(libc::EPERM) {
            eprintln!(
                "FAIL: namespace remount was not blocked by seccomp: rc={remount_rc}, \
                 errno={remount_error}"
            );
            std::process::exit(1);
        }
        eprintln!("OK: namespace remount blocked by seccomp");
    }
    for rel in &controls {
        match fs::read_to_string(workspace.join(rel)) {
            Ok(c) if c.contains("hello") => eprintln!("OK: {rel} control readable"),
            Ok(_) => {
                eprintln!("FAIL: control {rel} readable but missing marker");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("FAIL: control {rel} should stay readable: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(target_os = "macos")]
    for rel in list_from_env(POSTLAUNCH_ENV) {
        match fs::write(workspace.join(&rel), MARKER) {
            Err(e) if is_permission_denied(&e) => {
                eprintln!("OK: {rel} post-launch write denied")
            }
            Err(e) => {
                eprintln!("FAIL: unexpected {rel} post-launch write error: {e}");
                std::process::exit(1);
            }
            Ok(()) => {
                eprintln!("FAIL: {rel} post-launch matching path was writable");
                std::process::exit(1);
            }
        }
    }
    #[cfg(target_os = "macos")]
    if !list_from_env(POSTLAUNCH_ENV).is_empty() {
        match fs::write(workspace.join("late-control.txt"), "hello") {
            Ok(()) => eprintln!("OK: post-launch control writable"),
            Err(e) => {
                eprintln!("FAIL: non-matching post-launch path should be writable: {e}");
                std::process::exit(1);
            }
        }
    }
    std::process::exit(0);
}
/// Assert a path cannot be created via `create_dir` (mkdir denied).
fn assert_mkdir_denied(label: &str, path: &Path) {
    match fs::create_dir(path) {
        Err(e) if is_permission_denied(&e) => eprintln!("OK: {label} mkdir denied"),
        Err(e) => {
            eprintln!("FAIL: unexpected {label} mkdir error: {e}");
            std::process::exit(1);
        }
        Ok(()) => {
            eprintln!("FAIL: {label} mkdir was permitted");
            let _ = fs::remove_dir(path);
            std::process::exit(1);
        }
    }
}
/// Assert a path cannot be unlinked.
fn assert_unlink_denied(label: &str, path: &Path) {
    match fs::remove_file(path) {
        Err(e) if is_unlink_denied(&e) => eprintln!("OK: {label} unlink denied"),
        other => {
            eprintln!("FAIL: {label} unlink expected denial, got {other:?}");
            std::process::exit(1);
        }
    }
}
/// Assert a rename of `from` out of the deny set fails.
fn assert_rename_denied(label: &str, from: &Path, to: &Path) {
    match fs::rename(from, to) {
        Err(e) if is_rename_denied(&e) => eprintln!("OK: {label} rename denied"),
        other => {
            eprintln!("FAIL: {label} rename expected denial, got {other:?}");
            std::process::exit(1);
        }
    }
}
/// Assert a non-denied sibling path is writable.
fn assert_write_ok(label: &str, path: &Path) {
    match fs::write(path, "ok") {
        Ok(()) => eprintln!("OK: {label} writable"),
        Err(e) => {
            eprintln!("FAIL: {label} should be writable: {e}");
            std::process::exit(1);
        }
    }
}
/// Marker spoof: claim to be inside bwrap without real RO mounts; verify must fail.
/// Linux-only (verify is a no-op on macOS). Isolated subprocess; no shared env mutation.
fn subprocess_hook_write_deny_marker_spoof(_distill_home: &Path) {
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("OK: marker spoof N/A on non-linux");
        std::process::exit(0);
    }
    #[cfg(target_os = "linux")]
    {
        unsafe {
            std::env::set_var("__GROK_INSIDE_BWRAP", "1");
        }
        match distill_sandbox::verify_hook_write_deny_enforced() {
            Ok(()) => {
                eprintln!("FAIL: marker alone must not satisfy write-deny verification");
                std::process::exit(1);
            }
            Err(msg) => {
                if msg.contains("read-only")
                    || msg.contains("NotReadOnly")
                    || msg.contains("hook write-deny")
                    || msg.contains("effectively read-only")
                {
                    eprintln!("OK: marker spoof refused ({msg})");
                    std::process::exit(0);
                }
                eprintln!("FAIL: unexpected verify error: {msg}");
                std::process::exit(1);
            }
        }
    }
}
/// Marker spoof: claim to be inside bwrap while a denied path is still readable; read-deny verification must fail.
/// Uses a devbox-extending restrict-network profile, the shape the hook write-deny arm does not cover.
/// Linux-only (verify is a Seatbelt no-op on macOS). Isolated subprocess.
fn subprocess_read_deny_marker_spoof(workspace: &Path) {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace;
        eprintln!("OK: read-deny marker spoof N/A on non-linux");
        std::process::exit(0);
    }
    #[cfg(target_os = "linux")]
    {
        unsafe {
            std::env::set_var("__GROK_INSIDE_BWRAP", "1");
        }
        let profile = profile_from_env();
        match distill_sandbox::verify_read_deny_enforced(&profile, workspace) {
            Ok(()) => {
                eprintln!("FAIL: marker alone must not satisfy read-deny verification");
                std::process::exit(1);
            }
            Err(msg) => {
                eprintln!("OK: read-deny marker spoof refused ({msg})");
                std::process::exit(0);
            }
        }
    }
}
/// Caller-created bwrap with a valid sentinel and a mode-000 deny inode on its ordinary writable mount must fail startup verification.
fn subprocess_read_deny_forged_mounts(workspace: &Path) {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace;
        eprintln!("OK: forged read-deny mounts N/A on non-linux");
        std::process::exit(0);
    }
    #[cfg(target_os = "linux")]
    {
        let profile = profile_from_env();
        match distill_sandbox::verify_read_deny_enforced(&profile, workspace) {
            Ok(()) => {
                eprintln!("FAIL: writable mode-000 deny path passed verification");
                std::process::exit(1);
            }
            Err(msg) if msg.contains("not the mountpoint") || msg.contains("not read-only") => {
                eprintln!("OK: forged read-deny mounts refused ({msg})");
                std::process::exit(0);
            }
            Err(msg) => {
                eprintln!("FAIL: unexpected forged-mount verification error: {msg}");
                std::process::exit(1);
            }
        }
    }
}
/// Genuine bwrap with an EMPTY dynamic deny set: no custom deny entries, and any runtime-socket denials are host-dependent.
/// The unconditional sentinel mount must let verification pass inside the real re-exec.
/// The re-exec itself must happen (devbox-based profiles always compose the /data write-deny plan).
fn subprocess_read_deny_empty_set(workspace: &Path) {
    let profile = profile_from_env();
    subprocess_profile_and_bwrap_reexec(&profile, workspace);
    #[cfg(target_os = "linux")]
    match distill_sandbox::verify_read_deny_enforced(&profile, workspace) {
        Ok(()) => {
            eprintln!("OK: empty-set read-deny verified inside bwrap");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("FAIL: empty-set verification must pass inside bwrap: {e}");
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("OK: empty-set read-deny N/A on non-linux");
        std::process::exit(0);
    }
}
/// Caller-created bwrap forgery: the marker AND a read-only sentinel self-bind are both present, the complete spoof an unprivileged caller can stage.
/// Yet devbox `apply` must still install Landlock; the mount-shape proof must never short-circuit enforcement.
/// The parent bound a writable dir over a devbox-excluded mountpoint, so only Landlock can deny the write below.
fn subprocess_devbox_marker_spoof(workspace: &Path) {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = workspace;
        eprintln!("OK: devbox marker spoof N/A on non-linux");
        std::process::exit(0);
    }
    #[cfg(target_os = "linux")]
    {
        if !distill_sandbox::is_inside_bwrap() {
            eprintln!("FAIL: spoof subprocess expected the forged marker");
            std::process::exit(2);
        }
        let verified = distill_sandbox::verify_data_write_deny_enforced(
            &distill_sandbox::ProfileName::Devbox,
            workspace,
        );
        if std::env::var(DATA_STAGED_ENV).is_ok() {
            match verified {
                Err(e) if e.contains("not the mountpoint") => {}
                Err(e) => {
                    eprintln!("FAIL: unexpected forged /data verification error: {e}");
                    std::process::exit(5);
                }
                Ok(()) => {
                    eprintln!("FAIL: verification accepted a forged /data alias");
                    std::process::exit(5);
                }
            }
        } else if let Err(e) = verified {
            eprintln!("FAIL: verification failed without a /data mount to check: {e}");
            std::process::exit(5);
        }
        let mut sandbox =
            distill_sandbox::SandboxManager::new(distill_sandbox::ProfileName::Devbox, workspace);
        if let Err(e) = sandbox.apply(workspace) {
            eprintln!("sandbox apply failed: {e}");
            std::process::exit(3);
        }
        if !sandbox.is_applied() {
            eprintln!("FAIL: devbox must apply Landlock despite the forged marker");
            std::process::exit(4);
        }
        match fs::write("/sys/spoof-probe.txt", b"x") {
            Err(e) if is_permission_denied(&e) => {
                eprintln!("OK: devbox write denied under forged bwrap");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("FAIL: unexpected probe write error: {e}");
                std::process::exit(1);
            }
            Ok(_) => {
                eprintln!("FAIL: forged bwrap skipped devbox enforcement (probe writable)");
                std::process::exit(1);
            }
        }
    }
}
/// Genuine devbox re-exec: with the marker fast path removed, Landlock must now also apply inside the real bwrap without breaking startup.
fn subprocess_devbox_genuine(workspace: &Path) {
    let profile = distill_sandbox::ProfileName::Devbox;
    subprocess_profile_and_bwrap_reexec(&profile, workspace);
    let mut sandbox = distill_sandbox::SandboxManager::new(profile, workspace);
    if let Err(e) = sandbox.apply(workspace) {
        eprintln!("sandbox apply failed: {e}");
        std::process::exit(3);
    }
    if !sandbox.is_applied() {
        eprintln!("FAIL: devbox must apply Landlock inside genuine bwrap");
        std::process::exit(4);
    }
    #[cfg(target_os = "linux")]
    if let Err(e) = distill_sandbox::verify_data_write_deny_enforced(
        &distill_sandbox::ProfileName::Devbox,
        workspace,
    ) {
        eprintln!("FAIL: genuine devbox bwrap failed startup verification: {e}");
        std::process::exit(5);
    }
    if Path::new("/data").exists()
        && let Err(e) = fs::read_dir("/data")
    {
        eprintln!("FAIL: genuine devbox /data must remain readable: {e}");
        std::process::exit(5);
    }
    eprintln!("OK: devbox enforcement applied inside genuine bwrap");
    std::process::exit(0);
}
/// Workspace-profile Grok-owned hook write-deny probes (existing sources and first-run).
fn subprocess_hook_write_deny(workspace: &Path, first_run: bool) {
    let home = PathBuf::from(std::env::var(GROK_HOME_ENV).expect(GROK_HOME_ENV));
    let profile = distill_sandbox::ProfileName::Workspace;
    subprocess_profile_and_bwrap_reexec(&profile, workspace);
    let mut sandbox = distill_sandbox::SandboxManager::new(profile, workspace);
    if let Err(e) = sandbox.apply(workspace) {
        eprintln!("sandbox apply failed: {e}");
        std::process::exit(3);
    }
    #[cfg(target_os = "macos")]
    if !sandbox.is_applied() {
        eprintln!("sandbox was not applied");
        std::process::exit(4);
    }
    let hooks_dir = home.join("hooks");
    let hooks_paths = home.join("hooks-paths");
    let trust_boundary_files: Vec<(&str, PathBuf)> = distill_config::TRUST_BOUNDARY_FILENAMES
        .iter()
        .copied()
        .map(|name| (name, home.join(name)))
        .collect();
    if first_run {
        if !hooks_dir.is_dir() {
            eprintln!("FAIL: first-run expected real hooks dir to be ensured");
            std::process::exit(1);
        }
        if !hooks_paths.is_file() {
            eprintln!("FAIL: first-run expected real hooks-paths file to be ensured");
            std::process::exit(1);
        }
        assert_write_denied("hooks-paths (first-run)", &hooks_paths);
        assert_mkdir_denied("hooks nested (first-run)", &hooks_dir.join("nested"));
        assert_write_denied(
            "hooks nested file (first-run)",
            &hooks_dir.join("planted.json"),
        );
        for (name, path) in &trust_boundary_files {
            if !path.is_file() {
                eprintln!("FAIL: first-run expected real {name} to be ensured");
                std::process::exit(1);
            }
            assert_write_denied(&format!("{name} (first-run)"), path);
        }
        eprintln!("OK: first-run Grok hook slots denied");
    } else {
        let keep = hooks_dir.join("keep.json");
        match fs::read_to_string(&keep) {
            Ok(c) if c.contains("keep-me") => eprintln!("OK: hooks readable"),
            other => {
                eprintln!("FAIL: expected readable hook, got {other:?}");
                std::process::exit(1);
            }
        }
        assert_write_denied("hooks file", &hooks_dir.join("planted.json"));
        assert_write_denied("hooks-paths", &hooks_paths);
        for (name, path) in &trust_boundary_files {
            assert_write_denied(name, path);
            assert_rename_denied(name, path, &home.join(format!("{name}.exfil")));
            assert_unlink_denied(name, path);
        }
        let dynamic = home.join("sessions").join("extra-hooks");
        assert_write_denied("dynamic target", &dynamic.join("x.json"));
        assert_unlink_denied("hooks-paths", &hooks_paths);
        assert_rename_denied("hooks", &keep, &home.join("keep.exfil"));
        assert_mkdir_denied("hooks nested dir", &hooks_dir.join("nested-deny"));
        let sessions = home.join("sessions");
        let sessions_old = home.join("sessions-old");
        match fs::rename(&sessions, &sessions_old) {
            Err(e) if is_rename_denied(&e) => {
                eprintln!("OK: parent rename denied");
            }
            other => {
                let _ = fs::rename(&sessions_old, &sessions);
                eprintln!("FAIL: parent rename expected denial, got {other:?}");
                std::process::exit(1);
            }
        }
        assert_write_ok(
            "sessions sibling",
            &sessions.join(format!("runtime-{}.lock", std::process::id())),
        );
        let ws_parent = workspace.join("extra-parent");
        let ws_hooks = ws_parent.join("vendor-hooks");
        if ws_hooks.is_dir() {
            assert_write_denied("ws configured", &ws_hooks.join("x.json"));
            let renamed = workspace.join("extra-parent-old");
            match fs::rename(&ws_parent, &renamed) {
                Err(e) if is_rename_denied(&e) => {
                    eprintln!("OK: workspace parent rename denied");
                }
                other => {
                    let _ = fs::rename(&renamed, &ws_parent);
                    eprintln!("FAIL: workspace parent rename expected denial, got {other:?}");
                    std::process::exit(1);
                }
            }
            assert_write_ok(
                "workspace sibling under parent",
                &ws_parent.join(format!("sib-{}.lock", std::process::id())),
            );
        }
    }
    #[cfg(target_os = "linux")]
    if !first_run {
        let planted = hooks_dir.join("userns-plant.json");
        let alias = home.join("userns-alias");
        let inner = format!(
            "mkdir -p '{alias}' && mount --bind '{home}' '{alias}' && \
             echo nested > '{alias}/hooks/userns-plant.json'",
            alias = alias.display(),
            home = home.display(),
        );
        let sh = format!("unshare -Ur -m sh -c {inner:?}");
        let which = Command::new("sh")
            .args(["-c", "command -v unshare"])
            .output()
            .expect("command -v unshare");
        if !which.status.success() {
            eprintln!("FAIL: unshare binary missing; cannot assert seccomp denial");
            std::process::exit(1);
        }
        let out = Command::new("sh")
            .args(["-c", &sh])
            .output()
            .expect("spawn unshare probe");
        if out.status.success() {
            eprintln!(
                "FAIL: unshare exploit succeeded (seccomp should EPERM); stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::process::exit(1);
        }
        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
        if !(err.contains("not permitted")
            || err.contains("operation not permitted")
            || err.contains("eperm")
            || out.status.code() == Some(1))
        {
            eprintln!(
                "FAIL: expected seccomp EPERM-style denial, got status={:?} stderr={err}",
                out.status
            );
            std::process::exit(1);
        }
        if planted.exists()
            && let Ok(c) = fs::read_to_string(&planted)
            && c.contains("nested")
        {
            eprintln!("FAIL: nested userns rewrote host hooks");
            std::process::exit(1);
        }
        eprintln!("OK: nested userns did not rewrite hooks");
    }
    #[cfg(target_os = "linux")]
    if !first_run {
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            let m = Command::new("mount")
                .args([
                    "-o",
                    "bind",
                    "/",
                    &home.join("cap-drop-probe").display().to_string(),
                ])
                .output();
            if let Ok(o) = m
                && o.status.success()
            {
                eprintln!("FAIL: mount succeeded despite --cap-drop ALL");
                std::process::exit(1);
            }
            eprintln!("OK: cap-drop mount denied as root");
        } else {
            eprintln!("OK: cap-drop root probe skipped (non-root)");
        }
    }
    assert_write_ok(
        "grok runtime sibling",
        &home.join(format!("leader-{}.lock", std::process::id())),
    );
    assert_write_ok("workspace sibling", &workspace.join("fresh.rs"));
    let tmp = std::env::temp_dir().join(format!("hook-wd-tmp-{}", std::process::id()));
    assert_write_ok("temp sibling", &tmp);
    let _ = fs::remove_file(&tmp);
    eprintln!("OK: hook write-deny e2e passed");
    std::process::exit(0);
}
/// Create isolated HOME and GROK_HOME fixture dirs for a scenario.
fn fixture_homes(
    tag: &str,
) -> (
    PathBuf,
    PathBuf,
    PathBuf,
    TempDirGuard,
    TempDirGuard,
    TempDirGuard,
) {
    let home = unique_temp_dir(&format!("{tag}-home"));
    let grok = unique_temp_dir(&format!("{tag}-grok"));
    let workspace = unique_temp_dir(&format!("{tag}-ws"));
    fs::write(grok.join(distill_config::SANDBOX_CONFIG_FILENAME), "")
        .expect("empty global sandbox.toml");
    (
        home.clone(),
        grok.clone(),
        workspace.clone(),
        TempDirGuard(home),
        TempDirGuard(grok),
        TempDirGuard(workspace),
    )
}
/// A custom profile gets `deny_entries` (exact paths and/or globs) as its `deny` list. Each `target` is created with the
/// MARKER, each `control` with readable content. An isolated subprocess then asserts every target is
/// read/write/rename-denied and every control stays readable.
fn run_deny_case(
    tag: &str,
    profile: &str,
    deny_entries: &[&str],
    targets: &[&str],
    controls: &[&str],
    postlaunch: &[&str],
) {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, tmp, _ch, _cg, _cw) = fixture_homes(tag);
    let deny_list = deny_entries
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::create_dir_all(tmp.join(".grok")).expect("mkdir .grok");
    fs::write(
        tmp.join(".grok")
            .join(distill_config::SANDBOX_CONFIG_FILENAME),
        format!("[profiles.{profile}]\nextends = \"workspace\"\ndeny = [{deny_list}]\n"),
    )
    .expect("write sandbox.toml");
    fs::create_dir_all(grok.join("hooks")).expect("mkdir fixture hooks");
    fs::write(grok.join("hooks-paths"), b"").expect("write fixture hooks-paths");
    for rel in targets {
        let path = tmp.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir denied parent");
        }
        fs::write(&path, format!("SECRET={MARKER}")).expect("write denied file");
    }
    for rel in controls {
        let path = tmp.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir control parent");
        }
        fs::write(&path, "hello workspace").expect("write control");
    }
    let (status, stderr) = run_scenario(&home, &grok, &tmp, profile, targets, controls, postlaunch);
    assert!(
        status.success(),
        "[{tag}] custom-profile deny should block read/write/rename\nstderr: {stderr}"
    );
    for rel in targets {
        assert!(
            stderr.contains(&format!("OK: {rel} read blocked")),
            "[{tag}] expected '{rel}' read block confirmation\nstderr: {stderr}"
        );
        #[cfg(target_os = "linux")]
        assert!(
            stderr.contains(&format!("OK: {rel} chmod denied by read-only mount")),
            "[{tag}] expected '{rel}' chmod to be denied\nstderr: {stderr}"
        );
        assert!(
            stderr.contains(&format!("OK: {rel} write denied")),
            "[{tag}] expected '{rel}' write to be denied\nstderr: {stderr}"
        );
        assert!(
            stderr.contains(&format!("OK: {rel} rename bypass blocked")),
            "[{tag}] expected '{rel}' rename bypass to be blocked\nstderr: {stderr}"
        );
    }
    for rel in controls {
        assert!(
            stderr.contains(&format!("OK: {rel} control readable")),
            "[{tag}] expected non-denied control '{rel}' to stay readable\nstderr: {stderr}"
        );
    }
    #[cfg(target_os = "linux")]
    {
        assert!(
            stderr.contains("OK: read-deny mounts verified"),
            "[{tag}] expected read-deny verification to pass inside bwrap\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("OK: namespace unmount blocked by seccomp")
                && stderr.contains("OK: namespace remount blocked by seccomp"),
            "[{tag}] expected mount namespace mutation to stay blocked\nstderr: {stderr}"
        );
    }
    #[cfg(target_os = "macos")]
    for rel in postlaunch {
        assert!(
            stderr.contains(&format!("OK: {rel} post-launch write denied")),
            "[{tag}] expected post-launch matching '{rel}' to be write-denied\nstderr: {stderr}"
        );
    }
    #[cfg(target_os = "macos")]
    if !postlaunch.is_empty() {
        assert!(
            stderr.contains("OK: post-launch control writable"),
            "[{tag}] expected non-matching post-launch path to stay writable\nstderr: {stderr}"
        );
    }
    assert!(
        !home.join(".claude").exists(),
        "generic deny must not create ~/.claude under fixture HOME"
    );
    assert!(
        !home.join(".cursor").exists(),
        "generic deny must not create ~/.cursor under fixture HOME"
    );
}
#[test]
fn deny_exact_paths_block_read_write_rename() {
    run_deny_case(
        "exact",
        "denytest",
        &[".env", "src/server.pem", "secretdir"],
        &[".env", "src/server.pem", "secretdir/inner.pem"],
        &["readable.txt"],
        &[],
    );
}
#[test]
fn deny_globs_block_read_write_rename() {
    run_deny_case(
        "glob",
        "denyglob",
        &["**/*.pem", "**/.env", "secrets/**"],
        &["sub/dir/key.pem", ".env", "sub/.env", "secrets/inner.key"],
        &["readable.txt", "sub/dir/keep.txt"],
        &["late.pem"],
    );
}
/// Spoofing `__GROK_INSIDE_BWRAP` must not pass read-deny verification while a denied path is readable.
/// Uses a devbox-extending restrict-network profile, the shape hook write-deny verification does not cover.
#[test]
#[cfg(target_os = "linux")]
fn read_deny_marker_spoof_refused() {
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("read-deny-spoof");
    fs::create_dir_all(workspace.join(".grok")).expect("mkdir .grok");
    fs::write(
            workspace.join(".grok").join(distill_config::SANDBOX_CONFIG_FILENAME),
            "[profiles.netspoof]\nextends = \"devbox\"\nrestrict_network = true\ndeny = [\"secret.pem\"]\n",
        )
        .expect("write sandbox.toml");
    fs::write(workspace.join("secret.pem"), format!("SECRET={MARKER}")).expect("write secret");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    apply_fixture_env(&mut cmd, &home, &grok, &workspace);
    let output = cmd
        .env(SCENARIO_ENV, "read_deny_marker_spoof")
        .env(PROFILE_ENV, "netspoof")
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .arg("subprocess_entry")
        .output()
        .expect("failed to spawn subprocess");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.contains("OK: read-deny marker spoof refused"),
        "marker spoof must be refused by read-deny verification\nstderr: {stderr}"
    );
}
/// A caller-created bwrap with a read-only sentinel but only a writable-mount mode-000 deny inode must fail startup verification.
#[test]
#[cfg(target_os = "linux")]
fn read_deny_forged_mounts_are_refused() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("read-deny-forged");
    fs::create_dir_all(workspace.join(".grok")).expect("mkdir .grok");
    fs::write(
        workspace
            .join(".grok")
            .join(distill_config::SANDBOX_CONFIG_FILENAME),
        "[profiles.forged]\nextends = \"devbox\"\ndeny = [\"secret.pem\"]\n",
    )
    .expect("write sandbox.toml");
    let secret = workspace.join("secret.pem");
    fs::write(&secret, format!("SECRET={MARKER}")).expect("write secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).expect("chmod secret");
    let sentinel = grok.join("sandbox-bwrap-sentinel");
    fs::create_dir_all(&sentinel).expect("mkdir sentinel");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new("bwrap");
    apply_fixture_env(&mut cmd, &home, &grok, &workspace);
    let output = cmd
        .env(SCENARIO_ENV, "read_deny_forged_mounts")
        .env(PROFILE_ENV, "forged")
        .env("__GROK_INSIDE_BWRAP", "1")
        .args(["--bind", "/", "/"])
        .arg("--ro-bind")
        .arg(&sentinel)
        .arg(&sentinel)
        .args(["--dev-bind", "/dev", "/dev"])
        .args(["--proc", "/proc"])
        .arg("--")
        .arg(exe)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"])
        .output()
        .expect("failed to spawn forged bwrap");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.contains("OK: forged read-deny mounts refused"),
        "caller-created mode-000 deny paths must fail verification\nstderr: {stderr}"
    );
}
/// An empty deny list (the zero-socket, absent-/data shape on hosts without container runtimes) must not cause a false refusal on genuine runs.
/// The devbox-extending restrict-network profile must still re-exec through bwrap and pass verification via the unconditional sentinel mount.
#[test]
#[cfg(target_os = "linux")]
fn read_deny_empty_set_verifies_inside_bwrap() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("read-deny-empty");
    fs::create_dir_all(workspace.join(".grok")).expect("mkdir .grok");
    fs::write(
        workspace
            .join(".grok")
            .join(distill_config::SANDBOX_CONFIG_FILENAME),
        "[profiles.netempty]\nextends = \"devbox\"\nrestrict_network = true\n",
    )
    .expect("write sandbox.toml");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    apply_fixture_env(&mut cmd, &home, &grok, &workspace);
    let output = cmd
        .env(SCENARIO_ENV, "read_deny_empty_set")
        .env(PROFILE_ENV, "netempty")
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .arg("subprocess_entry")
        .output()
        .expect("failed to spawn subprocess");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.contains("OK: empty-set read-deny verified inside bwrap"),
        "empty deny set must verify via the sentinel inside genuine bwrap\nstderr: {stderr}"
    );
}
/// The complete unprivileged forgery: a caller-run bwrap carrying the marker and a read-only sentinel self-bind, but no grok-managed deny mounts.
/// It must not skip devbox enforcement: Landlock still applies and a mount-writable, devbox-excluded path stays write-denied.
#[test]
#[cfg(target_os = "linux")]
fn devbox_marker_spoof_does_not_skip_enforcement() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("devbox-spoof");
    let sentinel = grok.join("sandbox-bwrap-sentinel");
    fs::create_dir_all(&sentinel).expect("mkdir sentinel");
    let sentinel_s = sentinel.to_string_lossy().to_string();
    let fake_sys = unique_temp_dir("devbox-spoof-sys");
    let _fake_guard = TempDirGuard(fake_sys.clone());
    let fake_sys_s = fake_sys.to_string_lossy().to_string();
    let stage_data = Path::new("/data").exists();
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new("bwrap");
    apply_fixture_env(&mut cmd, &home, &grok, &workspace);
    cmd.env(SCENARIO_ENV, "devbox_marker_spoof")
        .env("__GROK_INSIDE_BWRAP", "1")
        .args(["--bind", "/", "/"])
        .args(["--bind", &fake_sys_s, "/sys"]);
    if stage_data {
        cmd.env(DATA_STAGED_ENV, "1")
            .arg("--remount-ro")
            .arg("/")
            .arg("--bind")
            .arg(&grok)
            .arg(&grok);
    }
    let output = cmd
        .args(["--ro-bind", &sentinel_s, &sentinel_s])
        .args(["--dev-bind", "/dev", "/dev"])
        .args(["--proc", "/proc"])
        .arg("--")
        .arg(exe)
        .args(["--ignored", "--exact", "--nocapture", "subprocess_entry"])
        .output()
        .expect("failed to spawn forged bwrap");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stderr.contains("OK: devbox write denied under forged bwrap"),
        "a caller-created bwrap spoof must not skip devbox enforcement\nstderr: {stderr}"
    );
}
/// Genuine devbox startup still succeeds with the marker fast path removed: the re-exec happens, and Landlock applies inside the real bwrap.
#[test]
#[cfg(target_os = "linux")]
fn devbox_genuine_reexec_applies_enforcement() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("devbox-genuine");
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    apply_fixture_env(&mut cmd, &home, &grok, &workspace);
    let output = cmd
        .env(SCENARIO_ENV, "devbox_genuine")
        .arg("--ignored")
        .arg("--exact")
        .arg("--nocapture")
        .arg("subprocess_entry")
        .output()
        .expect("failed to spawn subprocess");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stderr.contains("OK: devbox enforcement applied inside genuine bwrap"),
        "genuine devbox re-exec must keep applying enforcement\nstderr: {stderr}"
    );
}
/// Hard-linked registry file must refuse sandbox startup (writable alias).
#[test]
fn hardlinked_hooks_paths_refuses_startup() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook-hl");
    fs::create_dir_all(grok.join("hooks")).unwrap();
    let reg = grok.join("hooks-paths");
    let alias = grok.join("hooks-paths-alias");
    fs::write(&reg, b"").unwrap();
    fs::hard_link(&reg, &alias).unwrap();
    let (status, stderr) =
        run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny");
    assert!(
        !status.success(),
        "hard-linked hooks-paths must refuse startup\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("hard-link")
            || stderr.contains("HardLink")
            || stderr.contains("hook write-deny")
            || stderr.contains("nlink"),
        "expected hard-link refusal signal\nstderr: {stderr}"
    );
}
/// Workspace profile: Grok-owned direct hook sources are write-denied but readable.
/// Create / overwrite / unlink / rename / mkdir fail; absolute hooks-paths targets are denied; parent rename is blocked.
/// Grok/CWD/temp siblings stay writable.
#[test]
fn workspace_protects_direct_hook_sources() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook");
    fs::create_dir_all(grok.join("hooks")).expect("mkdir hooks");
    fs::write(grok.join("hooks").join("keep.json"), r#"{"keep-me":true}"#)
        .expect("write keep.json");
    let dynamic = grok.join("sessions").join("extra-hooks");
    fs::create_dir_all(&dynamic).expect("mkdir dynamic hooks target");
    fs::write(dynamic.join("x.json"), r#"{"x":1}"#).expect("write dynamic hook");
    let ws_hooks = workspace.join("extra-parent").join("vendor-hooks");
    fs::create_dir_all(&ws_hooks).expect("mkdir ws vendor hooks");
    fs::write(ws_hooks.join("x.json"), r#"{"x":1}"#).expect("write ws hook");
    fs::write(
        grok.join("hooks-paths"),
        format!("{}\n{}\n", dynamic.display(), ws_hooks.display()),
    )
    .expect("write hooks-paths");
    let (status, stderr) =
        run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny");
    assert!(
        status.success(),
        "hook write-deny e2e failed: {status}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("OK: hook write-deny e2e passed"),
        "missing pass marker\nstderr: {stderr}"
    );
    for needle in [
        "OK: hooks readable",
        "OK: hooks file write denied",
        "OK: hooks-paths write denied",
        "OK: dynamic target write denied",
        "OK: hooks-paths unlink denied",
        "OK: hooks rename denied",
        "OK: hooks nested dir mkdir denied",
        "OK: parent rename denied",
        "OK: sessions sibling writable",
        "OK: workspace parent rename denied",
        "OK: workspace sibling under parent writable",
        "OK: grok runtime sibling writable",
        "OK: workspace sibling writable",
        "OK: temp sibling writable",
    ] {
        assert!(
            stderr.contains(needle),
            "expected '{needle}'\nstderr: {stderr}"
        );
    }
    for name in distill_config::TRUST_BOUNDARY_FILENAMES {
        for action in ["write", "unlink", "rename"] {
            let needle = format!("OK: {name} {action} denied");
            assert!(
                stderr.contains(&needle),
                "expected '{needle}'\nstderr: {stderr}"
            );
        }
    }
    #[cfg(target_os = "linux")]
    assert!(
        stderr.contains("OK: nested userns did not rewrite hooks"),
        "expected nested userns check\nstderr: {stderr}"
    );
}
/// Hard-linked or symlinked discovery JSON under hooks/ must refuse startup.
#[test]
fn hardlinked_hooks_json_refuses_startup() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook-json-hl");
    fs::create_dir_all(grok.join("hooks")).unwrap();
    fs::write(grok.join("hooks-paths"), b"").unwrap();
    let active = grok.join("hooks").join("active.json");
    let alias = grok.join("hooks").join("active-alias.json");
    fs::write(&active, r#"{"hooks":{}}"#).unwrap();
    fs::hard_link(&active, &alias).unwrap();
    let (status, stderr) =
        run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny");
    assert!(
        !status.success(),
        "hard-linked hooks JSON must refuse startup\nstderr: {stderr}"
    );
}
#[test]
#[cfg(unix)]
fn symlinked_hooks_json_refuses_startup() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook-json-sym");
    fs::create_dir_all(grok.join("hooks")).unwrap();
    fs::write(grok.join("hooks-paths"), b"").unwrap();
    let real = grok.join("real-active.json");
    let active = grok.join("hooks").join("active.json");
    fs::write(&real, r#"{"hooks":{}}"#).unwrap();
    std::os::unix::fs::symlink(&real, &active).unwrap();
    let (status, stderr) =
        run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny");
    assert!(
        !status.success(),
        "symlinked hooks JSON must refuse startup\nstderr: {stderr}"
    );
}
/// First-run: missing fixed slots are created as real Grok state before apply, then write-denied.
/// Parent asserts post-exit host tree is valid (no vendor stubs).
#[test]
fn workspace_protects_direct_hook_sources_first_run() {
    if skip_if_enforcement_unavailable() {
        return;
    }
    let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook-fr");
    let (status, stderr) =
        run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny_first_run");
    assert!(
        status.success(),
        "hook write-deny first-run e2e failed: {status}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("OK: hook write-deny e2e passed"),
        "missing pass marker\nstderr: {stderr}"
    );
    for needle in [
        "OK: first-run Grok hook slots denied",
        "OK: hooks-paths (first-run) write denied",
        "OK: hooks nested (first-run) mkdir denied",
        "OK: hooks nested file (first-run) write denied",
        "OK: grok runtime sibling writable",
        "OK: workspace sibling writable",
        "OK: temp sibling writable",
    ] {
        assert!(
            stderr.contains(needle),
            "expected '{needle}'\nstderr: {stderr}"
        );
    }
    for name in distill_config::TRUST_BOUNDARY_FILENAMES {
        let needle = format!("OK: {name} (first-run) write denied");
        assert!(
            stderr.contains(&needle),
            "expected '{needle}'\nstderr: {stderr}"
        );
    }
    assert!(
        grok.join("hooks").is_dir(),
        "post-exit: hooks dir must exist as a real directory"
    );
    assert!(
        grok.join("hooks-paths").is_file(),
        "post-exit: hooks-paths must exist as a real file"
    );
    assert_eq!(
        fs::read(grok.join("hooks-paths")).expect("read hooks-paths"),
        b"",
        "post-exit: first-run hooks-paths must be empty"
    );
    for name in distill_config::TRUST_BOUNDARY_FILENAMES {
        assert!(
            grok.join(name).is_file(),
            "post-exit: {name} must exist as a real file"
        );
    }
    assert!(
        !home.join(".claude").exists(),
        "post-exit: must not create ~/.claude"
    );
    assert!(
        !home.join(".cursor").exists(),
        "post-exit: must not create ~/.cursor"
    );
}
/// Marker spoof in an isolated subprocess (no env-mutating unit test).
#[test]
fn hook_write_deny_refuses_marker_spoof() {
    #[cfg(not(target_os = "linux"))]
    {}
    #[cfg(target_os = "linux")]
    {
        let (home, grok, workspace, _ch, _cg, _cw) = fixture_homes("hook-spoof");
        fs::create_dir_all(grok.join("hooks")).unwrap();
        fs::write(grok.join("hooks").join("x.json"), b"{}").unwrap();
        fs::write(grok.join("hooks-paths"), b"").unwrap();
        let (status, stderr) =
            run_hook_write_deny_scenario(&home, &grok, &workspace, "hook_write_deny_marker_spoof");
        assert!(
            status.success(),
            "marker spoof e2e failed: {status}\nstderr: {stderr}"
        );
        assert!(
            stderr.contains("OK: marker spoof refused"),
            "expected spoof refusal\nstderr: {stderr}"
        );
    }
}
struct TempDirGuard(std::path::PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
