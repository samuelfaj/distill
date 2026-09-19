// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::ProfileName;
use crate::test_util::skip_if_host_hook_write_deny_unresolvable;
use std::path::PathBuf;

fn temp_workspace(tag: &str, toml_body: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ws = std::env::temp_dir().join(format!("grok-rdv-{tag}-{}-{nanos}", std::process::id()));
    let grok = ws.join(".grok");
    std::fs::create_dir_all(&grok).unwrap();
    std::fs::write(
        grok.join(distill_config::SANDBOX_CONFIG_FILENAME),
        toml_body,
    )
    .unwrap();
    ws
}

fn temp_workspace_with_secret_deny(tag: &str) -> PathBuf {
    temp_workspace(
        tag,
        "[profiles.readdeny]\nextends = \"workspace\"\ndeny = [\"secret.pem\"]\n",
    )
}

fn profile() -> ProfileName {
    ProfileName::Custom("readdeny".to_string())
}

#[test]
fn readable_deny_path_fails_verification() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let ws = temp_workspace_with_secret_deny("readable");
    std::fs::write(ws.join("secret.pem"), "top-secret").unwrap();
    let err =
        verify_resolved_read_deny_masks(&profile(), &ws).expect_err("readable path must fail");
    assert!(err.contains("secret.pem"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn missing_placeholder_fails_verification() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let ws = temp_workspace_with_secret_deny("missing");
    let err =
        verify_resolved_read_deny_masks(&profile(), &ws).expect_err("missing mount must fail");
    assert!(
        err.contains("no placeholder mount"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn live_socket_at_deny_path_fails_verification() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let ws = temp_workspace_with_secret_deny("socket");
    let _listener = std::os::unix::net::UnixListener::bind(ws.join("secret.pem")).unwrap();
    let err = verify_resolved_read_deny_masks(&profile(), &ws).expect_err("live socket must fail");
    assert!(err.contains("live socket"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn no_access_path_on_writable_mount_fails_verification() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let ws = temp_workspace_with_secret_deny("masked");
    let path = ws.join("secret.pem");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = verify_resolved_read_deny_masks(&profile(), &ws)
        .expect_err("mode-000 path on a writable mount must fail");
    assert!(
        err.contains("not the mountpoint") || err.contains("not read-only"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn mountinfo_parser_decodes_paths_and_reads_per_mount_flags() {
    let entry = parse_mountinfo_entry(
        r"42 31 8:1 /source /tmp/deny\040target ro,nosuid,nodev shared:7 - ext4 /dev/sda rw",
    )
    .expect("parse mountinfo row");
    assert_eq!(
        entry,
        MountInfoEntry {
            id: 42,
            mountpoint: PathBuf::from("/tmp/deny target"),
            is_read_only: true,
        }
    );
}

#[test]
fn mountinfo_parser_rejects_invalid_escape() {
    let err = parse_mountinfo_entry(r"42 31 8:1 /source /tmp/deny\09x rw - ext4 /dev/sda rw")
        .expect_err("invalid mountinfo escape must fail closed");
    assert!(
        err.contains("invalid mountinfo escape"),
        "unexpected error: {err}"
    );
}

#[test]
fn exact_read_only_mount_rejects_symlink() {
    let parent = temp_parent("exact-mount-symlink");
    let target = parent.join("target");
    std::fs::create_dir(&target).unwrap();
    let alias = parent.join("data");
    std::os::unix::fs::symlink(&target, &alias).unwrap();

    let err = verify_exact_read_only_mount(&alias).expect_err("symlink must not be a mountpoint");
    assert!(err.contains("symlink"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn data_mount_entry_requires_exact_read_only_mountpoint() {
    let cases = [
        (
            Path::new("/readonly/data"),
            MountInfoEntry {
                id: 42,
                mountpoint: PathBuf::from("/readonly"),
                is_read_only: true,
            },
            Some("not the mountpoint"),
        ),
        (
            Path::new("/data"),
            MountInfoEntry {
                id: 42,
                mountpoint: PathBuf::from("/data"),
                is_read_only: false,
            },
            Some("not read-only"),
        ),
        (
            Path::new("/data"),
            MountInfoEntry {
                id: 42,
                mountpoint: PathBuf::from("/real/data"),
                is_read_only: true,
            },
            Some("not the mountpoint"),
        ),
        (
            Path::new("/data"),
            MountInfoEntry {
                id: 42,
                mountpoint: PathBuf::from("/data"),
                is_read_only: true,
            },
            None,
        ),
    ];

    for (path, entry, expected_error) in cases {
        let result = verify_exact_read_only_mount_entry(path, 42, &[entry]);
        match expected_error {
            Some(expected) => {
                let err = result.expect_err("forged /data mount must fail");
                assert!(err.contains(expected), "unexpected error: {err}");
            }
            None => result.expect("exact read-only /data mountpoint must pass"),
        }
    }
}

#[test]
fn empty_deny_set_fails_without_sentinel_mount() {
    let ws = temp_workspace(
        "empty",
        "[profiles.netempty]\nextends = \"devbox\"\nrestrict_network = true\n",
    );
    let err = verify_read_deny_enforced(&ProfileName::Custom("netempty".to_string()), &ws)
        .expect_err("empty deny set must not pass without the sentinel mount");
    assert!(err.contains("sentinel"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&ws);
}

fn temp_parent(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "grok-sentinel-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A symlink sentinel must be rejected at the `O_NOFOLLOW` open, before the target's filesystem is ever consulted.
/// A link to an existing read-only filesystem (the spoof) then fails exactly like a link to any other directory.
#[test]
fn symlinked_sentinel_is_rejected_and_replaced() {
    let parent = temp_parent("symlink");
    let target = parent.join("target-dir");
    std::fs::create_dir_all(&target).unwrap();
    let sentinel = parent.join(SENTINEL_DIR_NAME);
    std::os::unix::fs::symlink(&target, &sentinel).unwrap();

    let err = verify_sentinel_under(&parent).expect_err("symlink sentinel must be rejected");
    assert!(
        err.contains("without following symlinks"),
        "unexpected error: {err}"
    );

    // Creation must replace the symlink with a real directory
    // A plain create_dir_all would silently keep the symlink and bwrap would mount its target
    let ensured = ensure_sentinel_dir_under(&parent).expect("ensure replaces the symlink");
    let meta = std::fs::symlink_metadata(&ensured).unwrap();
    assert!(
        meta.file_type().is_dir() && !meta.file_type().is_symlink(),
        "sentinel must be a real directory after ensure"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn plain_directory_sentinel_is_rejected() {
    let parent = temp_parent("plaindir");
    ensure_sentinel_dir_under(&parent).expect("create plain sentinel dir");
    let err = verify_sentinel_under(&parent).expect_err("plain directory must be rejected");
    assert!(
        err.contains("not a read-only mount"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_dir_all(&parent);
}

/// A user's explicit deny at a well-known runtime-socket path in a profile with restrict_network=false must use strict placeholder verification.
/// The per-spawn child network filter is not installed there, so the lenient socket arm would silently drop the user's read-deny.
/// Strictness makes this fail outside a genuine re-exec whether the host endpoint is absent (no placeholder mount) or a live socket.
#[test]
fn user_socket_deny_stays_strict_without_restrict_network() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let ws = temp_workspace(
        "user-sock",
        "[profiles.socknorestrict]\nextends = \"workspace\"\nrestrict_network = false\n\
         deny = [\"/var/run/docker.sock\"]\n",
    );
    let err =
        verify_resolved_read_deny_masks(&ProfileName::Custom("socknorestrict".to_string()), &ws)
            .expect_err("explicit socket deny must stay strict without restrict_network");
    assert!(err.contains("docker.sock"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&ws);
}

/// Auto runtime-socket denials tolerate a socket that appeared after the launch-time plan and a vanished endpoint.
/// But a placeholder must satisfy the same durable mount invariant as every other accepted strict deny target.
#[test]
fn runtime_socket_arm_tolerates_only_live_or_missing_endpoints() {
    use std::os::unix::fs::PermissionsExt;
    let dir = temp_parent("sock-arm");
    let sock = dir.join("api.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    verify_runtime_socket_deny(&sock).expect("live socket must not refuse startup");
    verify_runtime_socket_deny(&dir.join("missing.sock")).expect("absent endpoint tolerated");
    let alias = dir.join("alias.sock");
    std::os::unix::fs::symlink(&sock, &alias).unwrap();
    let err = verify_runtime_socket_deny(&alias).expect_err("runtime socket symlink must fail");
    assert!(err.contains("symlink"), "unexpected error: {err}");
    let masked = dir.join("masked");
    std::fs::write(&masked, "").unwrap();
    std::fs::set_permissions(&masked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let err = verify_runtime_socket_deny(&masked)
        .expect_err("plain mode-000 runtime path must not count as a mounted placeholder");
    assert!(
        err.contains("not the mountpoint"),
        "unexpected error: {err}"
    );
    let exposed = dir.join("exposed");
    std::fs::write(&exposed, "x").unwrap();
    let err = verify_runtime_socket_deny(&exposed).expect_err("exposed content must fail");
    assert!(err.contains("exposed"), "unexpected error: {err}");
    let _ = std::fs::remove_dir_all(&dir);
}
