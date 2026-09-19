// Modified for Distill by Samuel Fajreldines, 2026.
use super::*;
use crate::profiles::{ProfileConfig, ProfileName, SandboxConfig, SandboxProfile};
use crate::test_util::{network_inheritance_config, skip_if_host_hook_write_deny_unresolvable};
use serial_test::serial;
use std::collections::HashMap;
use std::path::PathBuf;

#[test]
fn deny_list_covers_system_rootless_and_per_user_endpoints() {
    let paths = runtime_socket_deny_paths();
    for literal in SYSTEM_SOCKETS {
        assert!(
            paths.contains(&PathBuf::from(literal)),
            "missing {literal}: {paths:?}"
        );
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe.
        let uid = unsafe { libc::getuid() };
        assert!(
            paths.contains(&PathBuf::from(format!("/run/user/{uid}/docker.sock"))),
            "missing rootless endpoint: {paths:?}"
        );
    }
    if let Some(home) = distill_dirs::home_dir() {
        assert!(
            paths.contains(&home.join(".docker/desktop/docker.sock")),
            "missing Docker Desktop Linux endpoint: {paths:?}"
        );
        assert!(
            paths.contains(&home.join(".docker/run/docker.sock")),
            "missing Docker Desktop macOS endpoint: {paths:?}"
        );
    }
}

#[test]
fn dbus_deny_list_covers_system_and_per_user_endpoints() {
    let paths = dbus_socket_deny_paths();
    for literal in [
        "/run/dbus/system_bus_socket",
        "/var/run/dbus/system_bus_socket",
        "/run/systemd/private",
    ] {
        assert!(
            paths.contains(&PathBuf::from(literal)),
            "missing {literal}: {paths:?}"
        );
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid is always safe.
        let uid = unsafe { libc::getuid() };
        assert!(
            paths.contains(&PathBuf::from(format!("/run/user/{uid}/bus"))),
            "missing session bus: {paths:?}"
        );
        assert!(
            paths.contains(&PathBuf::from(format!("/run/user/{uid}/systemd/private"))),
            "missing per-user systemd private: {paths:?}"
        );
    }
}

fn assert_no_auto_socket_denies_except(profile: &SandboxProfile, except: &[PathBuf]) {
    for socket in runtime_socket_deny_paths() {
        if except.iter().any(|p| p == &socket) {
            continue;
        }
        assert!(
            !profile.deny.contains(&socket),
            "{name} must not deny {socket:?}; got {:?}",
            profile.deny,
            name = profile.name
        );
    }
}

fn assert_deny_eq_order_insensitive(profile: &SandboxProfile, expected: Vec<PathBuf>) {
    let mut expected = expected;
    let mut actual = profile.deny.clone();
    expected.sort();
    actual.sort();
    assert_eq!(expected, actual, "{} deny mismatch", profile.name);
}

fn materialized_dbus_socket_denies() -> Vec<PathBuf> {
    materialize_runtime_socket_deny_paths_from(dbus_socket_deny_paths())
        .expect("dbus socket deny paths materialize")
}

fn assert_materialized_auto_socket_denies(profile: &SandboxProfile) {
    let mut expected = materialize_runtime_socket_deny_paths_from(runtime_socket_deny_paths())
        .expect("socket deny paths materialize");
    for path in materialized_dbus_socket_denies() {
        if !expected.contains(&path) {
            expected.push(path);
        }
    }
    assert_deny_eq_order_insensitive(profile, expected);
}

#[cfg(unix)]
fn temp_runtime_root(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "grok-runtime-sockets-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
#[cfg(unix)]
fn materialized_socket_paths_canonicalize_and_deduplicate_parent_aliases() {
    let root = temp_runtime_root("aliases");
    let run = root.join("run");
    let var = root.join("var");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&var).unwrap();
    std::os::unix::fs::symlink("../run", var.join("run")).unwrap();
    let socket = run.join("docker.sock");
    let alias = var.join("run/docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let paths = materialize_runtime_socket_deny_paths_from([
        socket.clone(),
        alias,
        root.join("missing/desktop/docker.sock"),
    ])
    .unwrap();
    assert_eq!(paths, vec![socket]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn materialized_socket_path_canonicalizes_nested_parent_aliases() {
    let root = temp_runtime_root("nested-alias");
    let runtime = root.join("runtime");
    let alias = root.join("run/user/42");
    let socket = runtime.join("podman/podman.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&runtime, &alias).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let paths =
        materialize_runtime_socket_deny_paths_from([alias.join("podman/podman.sock")]).unwrap();
    assert_eq!(paths, vec![socket]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn restricted_explicit_runtime_alias_collapses_to_canonical_auto_deny() {
    let root = temp_runtime_root("explicit-alias");
    let run = root.join("run");
    let var = root.join("var");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&var).unwrap();
    std::os::unix::fs::symlink("../run", var.join("run")).unwrap();
    let canonical = run.join("docker.sock");
    let alias = var.join("run/docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&canonical).unwrap();
    let auto_sockets =
        materialize_runtime_socket_deny_paths_from([canonical.clone(), alias.clone()]).unwrap();
    let mut deny = vec![alias.clone()];
    let policy = [canonical.clone(), alias.clone()];
    let mut provenance = Vec::new();

    merge_runtime_socket_denies(&mut deny, &auto_sockets, &policy).unwrap();
    provenance.extend(auto_sockets.iter().cloned());

    assert_eq!(deny, vec![canonical]);
    assert_eq!(provenance, deny);
    assert!(!deny.contains(&alias));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn absent_automatic_endpoint_leaves_explicit_runtime_alias_strict() {
    let alias = PathBuf::from("/var/run/docker.sock");
    let mut deny = vec![alias.clone()];

    merge_runtime_socket_denies(&mut deny, &[], std::slice::from_ref(&alias)).unwrap();

    assert_eq!(deny, vec![alias]);
}

#[test]
#[cfg(unix)]
fn unrelated_symlink_deny_is_untouched() {
    let root = temp_runtime_root("unrelated-symlink");
    let run = root.join("run");
    let alias_parent = root.join("alias");
    std::fs::create_dir(&run).unwrap();
    std::os::unix::fs::symlink(&run, &alias_parent).unwrap();
    let alias = alias_parent.join("docker.sock");
    let canonical_socket = run.join("docker.sock");
    let mut deny = vec![alias.clone()];

    merge_runtime_socket_denies(
        &mut deny,
        std::slice::from_ref(&canonical_socket),
        std::slice::from_ref(&canonical_socket),
    )
    .unwrap();

    assert_eq!(deny, vec![alias, canonical_socket]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn materialized_socket_path_is_stable_after_mode_zero_mask() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_runtime_root("masked");
    let parent = root.join(".docker/desktop");
    std::fs::create_dir_all(&parent).unwrap();
    let socket = parent.join("docker.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let before = materialize_runtime_socket_deny_paths_from([socket.clone()]).unwrap();

    drop(listener);
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, "").unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).unwrap();
    let after = materialize_runtime_socket_deny_paths_from([socket.clone()]).unwrap();

    assert_eq!(before, vec![socket]);
    assert_eq!(after, before);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn materialized_socket_paths_reject_endpoint_symlinks() {
    let root = temp_runtime_root("endpoint-symlink");
    let target = root.join("target.sock");
    let endpoint = root.join("docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&target).unwrap();
    std::os::unix::fs::symlink(&target, &endpoint).unwrap();

    let error = materialize_runtime_socket_deny_paths_from([endpoint])
        .expect_err("an endpoint symlink must not redirect the automatic mask");
    assert!(
        error.to_string().contains("endpoint is a symlink"),
        "unexpected error: {error}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn materialized_socket_paths_propagate_parent_resolution_errors() {
    let root = temp_runtime_root("parent-error");
    let loop_parent = root.join("loop");
    std::os::unix::fs::symlink(&loop_parent, &loop_parent).unwrap();
    let candidate = loop_parent.join("docker.sock");

    let error = materialize_runtime_socket_deny_paths_from([candidate])
        .expect_err("parent resolution errors other than absence must propagate");
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(
        error
            .to_string()
            .contains("could not resolve runtime-socket deny path"),
        "unexpected error: {error}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn inside_bwrap_without_handoff_does_not_discover_runtime_sockets() {
    let root = temp_runtime_root("missing-handoff");
    let socket = root.join("docker.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let paths = runtime_socket_deny_paths_for_context_with_policy(
        Err(std::env::VarError::NotPresent),
        vec![socket],
    )
    .unwrap();

    assert!(paths.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn runtime_socket_handoff_accepts_canonical_parent_alias() {
    let root = temp_runtime_root("canonical-handoff-alias");
    let run = root.join("run");
    let var = root.join("var");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&var).unwrap();
    std::os::unix::fs::symlink("../run", var.join("run")).unwrap();
    let canonical = run.join("docker.sock");
    let encoded = encode_bwrap_runtime_socket_denies(std::slice::from_ref(&canonical)).unwrap();

    let paths =
        decode_bwrap_runtime_socket_denies_with_policy(&encoded, vec![var.join("run/docker.sock")])
            .expect("canonical parent alias belongs to the static policy");

    assert_eq!(paths, vec![canonical]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn runtime_socket_handoff_preserves_outer_set_without_endpoint_discovery() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_runtime_root("handoff");
    let parent = root.join(".docker/desktop");
    std::fs::create_dir_all(&parent).unwrap();
    let socket = parent.join("docker.sock");
    let absent = root.join("new-home/.docker/desktop/docker.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let outer =
        materialize_runtime_socket_deny_paths_from([socket.clone(), absent.clone()]).unwrap();
    let allowed = vec![socket.clone(), absent.clone()];
    let encoded = encode_bwrap_runtime_socket_denies(&outer).unwrap();

    drop(listener);
    std::fs::remove_file(&socket).unwrap();
    std::fs::write(&socket, "").unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::create_dir_all(absent.parent().unwrap()).unwrap();
    std::fs::write(&absent, "").unwrap();
    std::fs::set_permissions(&absent, std::fs::Permissions::from_mode(0o000)).unwrap();
    let inner = decode_bwrap_runtime_socket_denies_with_policy(&encoded, allowed.clone()).unwrap();
    let mut deny = Vec::new();
    merge_runtime_socket_denies(&mut deny, &inner, &allowed).unwrap();

    assert_eq!(deny, vec![socket.clone()]);
    assert_eq!(inner, deny);
    assert!(!deny.contains(&absent));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn runtime_socket_handoff_rejects_paths_outside_static_policy() {
    let root = temp_runtime_root("forged-handoff");
    let allowed = root.join("run/docker.sock");
    let forged = root.join("workspace/secret.pem");
    let encoded = encode_bwrap_runtime_socket_denies(std::slice::from_ref(&forged)).unwrap();

    let error = decode_bwrap_runtime_socket_denies_with_policy(&encoded, vec![allowed])
        .expect_err("arbitrary handoff paths must not gain automatic-socket provenance");
    assert!(
        error.to_string().contains("not in the automatic policy"),
        "unexpected error: {error}"
    );
    let mut deny = vec![forged.clone()];
    merge_runtime_socket_denies(&mut deny, &[], &[]).unwrap();
    assert_eq!(deny, vec![forged]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[cfg(unix)]
fn runtime_socket_handoff_deduplicates_entries() {
    let root = temp_runtime_root("handoff-dedupe");
    let allowed = root.join("run/docker.sock");
    let encoded = encode_bwrap_runtime_socket_denies(&[allowed.clone(), allowed.clone()]).unwrap();

    let paths = decode_bwrap_runtime_socket_denies_with_policy(&encoded, vec![allowed.clone()])
        .expect("duplicate policy paths are valid handoff entries");

    assert_eq!(paths, vec![allowed]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[serial(bwrap_env)]
fn restrict_network_profiles_deny_container_runtime_sockets() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let workspace = std::env::temp_dir();
    let config = SandboxConfig::default();
    for name in [ProfileName::Strict, ProfileName::ReadOnly] {
        let profile = name
            .resolve_profile(&workspace, &config)
            .expect("profile resolves");
        assert!(profile.restrict_network, "{name}");
        assert_materialized_auto_socket_denies(&profile);
    }
    let workspace_profile = ProfileName::Workspace
        .resolve_profile(&workspace, &config)
        .expect("profile resolves");
    assert!(!workspace_profile.restrict_network);
    assert_deny_eq_order_insensitive(&workspace_profile, materialized_dbus_socket_denies());

    let devbox = ProfileName::Devbox
        .resolve_profile(&workspace, &config)
        .expect("profile resolves");
    assert!(!devbox.restrict_network);
    assert!(devbox.deny.is_empty(), "devbox deny: {:?}", devbox.deny);
}

#[test]
#[serial(bwrap_env)]
fn custom_restrict_network_override_controls_auto_socket_denies() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let workspace = std::env::temp_dir();
    let config = network_inheritance_config();

    let unrestricted = ProfileName::Custom("strict-unrestricted".to_string())
        .resolve_profile(&workspace, &config)
        .expect("custom resolves");
    assert!(!unrestricted.restrict_network);
    assert_deny_eq_order_insensitive(&unrestricted, materialized_dbus_socket_denies());

    let restricted = ProfileName::Custom("workspace-restricted".to_string())
        .resolve_profile(&workspace, &config)
        .expect("custom resolves");
    assert!(restricted.restrict_network);
    assert_materialized_auto_socket_denies(&restricted);

    let devbox_net = ProfileName::Custom("devbox-net".to_string())
        .resolve_profile(
            &workspace,
            &SandboxConfig {
                profiles: HashMap::from([(
                    "devbox-net".to_string(),
                    ProfileConfig {
                        extends: Some("devbox".to_string()),
                        restrict_network: Some(true),
                        read_only: vec![],
                        read_write: vec![],
                        deny: vec![],
                    },
                )]),
            },
        )
        .expect("devbox custom resolves");
    assert!(devbox_net.restrict_network);
    // Network-restricted devbox-based customs still mask container-runtime sockets; not D-Bus.
    assert_deny_eq_order_insensitive(
        &devbox_net,
        materialize_runtime_socket_deny_paths_from(runtime_socket_deny_paths())
            .expect("socket deny paths materialize"),
    );
}

#[test]
fn custom_user_socket_deny_kept_when_restrict_network_false() {
    if skip_if_host_hook_write_deny_unresolvable() {
        return;
    }
    let workspace = std::env::temp_dir();
    let config = SandboxConfig {
        profiles: HashMap::from([(
            "keep-sock".to_string(),
            ProfileConfig {
                extends: Some("workspace".to_string()),
                restrict_network: Some(false),
                read_only: vec![],
                read_write: vec![],
                deny: vec!["/var/run/docker.sock".to_string()],
            },
        )]),
    };
    let profile = ProfileName::Custom("keep-sock".to_string())
        .resolve_profile(&workspace, &config)
        .expect("custom resolves");
    assert!(!profile.restrict_network);
    let user_sock = PathBuf::from("/var/run/docker.sock");
    let mut expected = vec![user_sock.clone()];
    for socket in materialized_dbus_socket_denies() {
        if !expected.contains(&socket) {
            expected.push(socket);
        }
    }
    assert_deny_eq_order_insensitive(&profile, expected);
    assert_no_auto_socket_denies_except(&profile, std::slice::from_ref(&user_sock));
}

#[test]
#[cfg(unix)]
fn materialized_dbus_candidates_skip_missing_keep_existing() {
    let root = temp_runtime_root("dbus-materialize");
    let present = root.join("run/dbus/system_bus_socket");
    std::fs::create_dir_all(present.parent().unwrap()).unwrap();
    std::fs::write(&present, b"").unwrap();
    let missing = root.join("run/systemd/private");
    let paths = materialize_runtime_socket_deny_paths_from([
        present.clone(),
        missing,
        root.join("var/run/dbus/system_bus_socket"),
    ])
    .unwrap();
    assert_eq!(paths, vec![present]);
    let _ = std::fs::remove_dir_all(root);
}
