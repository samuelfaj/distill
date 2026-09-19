use super::*;
use tempfile::TempDir;

#[test]
fn absolute_hooks_paths_only_and_fixed_slots() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let nested = dir.join("extra");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        dir.join("hooks-paths"),
        format!("{}\nrelative/x\n", nested.display()),
    )
    .unwrap();

    let resolved = resolve_global_hook_sources(Some(dir), false).unwrap();
    assert!(resolved.configured_error.is_none());
    let sources = &resolved.sources;
    assert!(
        sources.iter().any(|s| {
            s.path == dir.join("hooks") && s.kind == GlobalHookSourceKind::HookDirectory
        })
    );
    assert!(sources.iter().any(|s| {
        s.path == dir.join("hooks-paths") && s.kind == GlobalHookSourceKind::RegistryFile
    }));
    assert!(
        sources
            .iter()
            .any(|s| { s.path == nested && s.kind == GlobalHookSourceKind::ConfiguredSource })
    );
    assert!(!sources.iter().any(|s| s.path.ends_with("relative/x")));
    assert!(missing_configured_sources(sources).is_empty());

    // Discovery must never treat the registry file as a hook source.
    let discovery: Vec<_> = resolved
        .discovery_sources()
        .map(|s| s.path.clone())
        .collect();
    assert!(!discovery.iter().any(|p| p == &dir.join("hooks-paths")));
    assert!(discovery.iter().any(|p| p == &dir.join("hooks")));
    assert!(discovery.iter().any(|p| p == &nested));
}

#[test]
fn missing_configured_is_reported() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let missing = dir.join("nope").join("hooks");
    std::fs::write(dir.join("hooks-paths"), format!("{}\n", missing.display())).unwrap();
    let resolved = resolve_global_hook_sources(Some(dir), false).unwrap();
    assert!(resolved.configured_error.is_none());
    let miss = missing_configured_sources(&resolved.sources);
    assert!(miss.iter().any(|p| p == &missing));
    assert!(!miss.iter().any(|p| p == &dir.join("hooks")));
}

#[test]
fn hooks_paths_read_error_keeps_fixed_slots() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    // A directory named hooks-paths makes read_to_string fail with IsADirectory
    std::fs::create_dir_all(dir.join("hooks-paths")).unwrap();
    let resolved = resolve_global_hook_sources(Some(dir), false).unwrap();
    assert!(resolved.is_incomplete());
    assert!(matches!(
        resolved.configured_error,
        Some(GlobalHookSourceError::HooksPathsRead { .. })
    ));
    assert!(
        resolved.sources.iter().any(|s| {
            s.path == dir.join("hooks") && s.kind == GlobalHookSourceKind::HookDirectory
        })
    );
    assert!(resolved.sources.iter().any(|s| {
        s.path == dir.join("hooks-paths") && s.kind == GlobalHookSourceKind::RegistryFile
    }));
    assert!(
        !resolved
            .sources
            .iter()
            .any(|s| s.kind == GlobalHookSourceKind::ConfiguredSource)
    );
}

#[test]
#[cfg(unix)]
fn reject_symlinked_configured_source() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let real = tmp.path().join("real-hooks");
    std::fs::create_dir_all(&real).unwrap();
    let link = dir.join("link-hooks");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::fs::write(dir.join("hooks-paths"), format!("{}\n", link.display())).unwrap();
    let err = resolve_global_hook_sources(Some(dir), true).unwrap_err();
    assert!(matches!(err, GlobalHookSourceError::SymlinkedSource { .. }));
}

#[test]
fn not_found_hooks_paths_is_ok_empty_configured() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    let resolved = resolve_global_hook_sources(Some(dir), false).unwrap();
    assert!(resolved.configured_error.is_none());
    assert!(missing_configured_sources(&resolved.sources).is_empty());
    assert!(
        resolved
            .sources
            .iter()
            .any(|s| s.kind == GlobalHookSourceKind::HookDirectory)
    );
    assert!(
        resolved
            .sources
            .iter()
            .any(|s| s.kind == GlobalHookSourceKind::RegistryFile)
    );
}

#[test]
fn ensure_creates_hooks_dir_and_empty_registry() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    ensure_grok_hook_slots(&dir).unwrap();
    let hooks = dir.join("hooks");
    let reg = dir.join("hooks-paths");
    assert!(hooks.is_dir());
    assert!(reg.is_file());
    assert_eq!(std::fs::read(&reg).unwrap(), b"");
    for name in TRUST_BOUNDARY_FILENAMES {
        let path = dir.join(name);
        assert!(path.is_file(), "{name} must be a real file");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"",
            "{name} first-run is empty"
        );
    }
    // Idempotent: does not truncate existing registry content
    std::fs::write(&reg, b"/abs/extra\n").unwrap();
    std::fs::write(dir.join("config.toml"), b"model = \"keep\"\n").unwrap();
    ensure_grok_hook_slots(&dir).unwrap();
    assert_eq!(std::fs::read(&reg).unwrap(), b"/abs/extra\n");
    assert_eq!(
        std::fs::read(dir.join("config.toml")).unwrap(),
        b"model = \"keep\"\n"
    );
}

#[test]
fn trust_boundary_sources_are_not_discovery() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    ensure_grok_trust_boundary_slots(&dir).unwrap();
    let sources = resolve_trust_boundary_sources(&dir).unwrap();
    assert_eq!(sources.len(), TRUST_BOUNDARY_FILENAMES.len());
    for name in TRUST_BOUNDARY_FILENAMES {
        let path = dir.join(name);
        let source = sources
            .iter()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("missing trust-boundary file {name}"));
        assert_eq!(source.kind, GlobalHookSourceKind::TrustBoundaryFile);
        assert!(!source.is_dir());
        assert!(!source.is_discovery_source());
    }
}

#[test]
#[cfg(unix)]
fn ensure_rejects_preexisting_symlink_hooks_dir() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    let real = tmp.path().join("real-hooks");
    std::fs::create_dir_all(&real).unwrap();
    std::os::unix::fs::symlink(&real, dir.join("hooks")).unwrap();
    let err = ensure_grok_hook_slots(&dir).unwrap_err();
    assert!(matches!(
        err,
        GlobalHookSourceError::InvalidHooksDir { .. }
            | GlobalHookSourceError::SymlinkedSource { .. }
    ));
}

#[test]
#[cfg(unix)]
fn ensure_rejects_preexisting_symlink_registry() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    let target = tmp.path().join("evil-registry");
    std::fs::write(&target, b"attacker\n").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("hooks-paths")).unwrap();
    let err = ensure_grok_hook_slots(&dir).unwrap_err();
    // create_new hits EEXIST on the symlink and require_real_file rejects it, or O_NOFOLLOW stops the open; nothing writes through the symlink
    assert!(matches!(
        err,
        GlobalHookSourceError::InvalidRegistryFile { .. }
            | GlobalHookSourceError::SymlinkedSource { .. }
            | GlobalHookSourceError::CreateRegistryFile { .. }
    ));
    // Nothing wrote through the symlink
    assert_eq!(std::fs::read(&target).unwrap(), b"attacker\n");
}

#[test]
#[cfg(unix)]
fn ensure_trust_boundary_rejects_preexisting_symlink_persist_file() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    let target = tmp.path().join("evil-persist");
    std::fs::write(&target, b"attacker\n").unwrap();
    let Some(persist_name) = TRUST_BOUNDARY_FILENAMES.first() else {
        panic!("TRUST_BOUNDARY_FILENAMES is non-empty");
    };
    let persist = dir.join(persist_name);
    std::os::unix::fs::symlink(&target, &persist).unwrap();
    let err = ensure_grok_trust_boundary_slots(&dir).unwrap_err();
    // create_new hits EEXIST on the symlink and require_real_file rejects it, or O_NOFOLLOW stops the open; nothing writes through the symlink
    assert!(
        matches!(
            err,
            GlobalHookSourceError::InvalidRegistryFile { .. }
                | GlobalHookSourceError::SymlinkedSource { .. }
                | GlobalHookSourceError::CreateRegistryFile { .. }
        ),
        "symlink trust-boundary file must be rejected, got {err:?}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"attacker\n");
}

#[test]
#[cfg(unix)]
fn ensure_rejects_directory_named_hooks_paths() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(dir.join("hooks-paths")).unwrap();
    let err = ensure_grok_hook_slots(&dir).unwrap_err();
    assert!(matches!(
        err,
        GlobalHookSourceError::InvalidRegistryFile { .. }
    ));
}

#[test]
#[cfg(unix)]
fn ensure_rejects_file_named_hooks_dir() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("grok");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hooks"), b"not-a-dir").unwrap();
    let err = ensure_grok_hook_slots(&dir).unwrap_err();
    assert!(matches!(err, GlobalHookSourceError::InvalidHooksDir { .. }));
}

#[test]
fn existing_ancestor_chain_lists_parents() {
    let tmp = TempDir::new().unwrap();
    let leaf = tmp.path().join("a").join("b").join("c");
    std::fs::create_dir_all(&leaf).unwrap();
    let chain = existing_ancestor_chain(&leaf);
    assert_eq!(chain.first(), Some(&tmp.path().join("a").join("b")));
    assert!(chain.iter().any(|p| p == &tmp.path().join("a")));
}

#[test]
fn list_direct_hook_json_files_matches_discovery_filter() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("active.json"), b"{}").unwrap();
    std::fs::write(dir.join(".hidden.json"), b"{}").unwrap();
    std::fs::write(dir.join("backup.json~"), b"{}").unwrap();
    std::fs::write(dir.join("notes.txt"), b"x").unwrap();
    let files = list_direct_hook_json_files(dir).unwrap();
    assert_eq!(files.len(), 1);
    let Some(first) = files.first() else {
        panic!("expected one hook json file: {files:?}");
    };
    assert!(first.ends_with("active.json"));
}

#[test]
#[cfg(unix)]
fn validate_direct_hook_json_rejects_hardlink_and_symlink() {
    let tmp = TempDir::new().unwrap();
    let f = tmp.path().join("a.json");
    let hl = tmp.path().join("b.json");
    std::fs::write(&f, b"{}").unwrap();
    std::fs::hard_link(&f, &hl).unwrap();
    assert!(matches!(
        validate_direct_hook_json_file(&f),
        Err(GlobalHookSourceError::HardLinkedHookFile { .. })
    ));
    let real = tmp.path().join("real.json");
    let link = tmp.path().join("link.json");
    std::fs::write(&real, b"{}").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(matches!(
        validate_direct_hook_json_file(&link),
        Err(GlobalHookSourceError::SymlinkedSource { .. })
    ));
}

#[test]
fn ancestors_to_pin_skips_mountpoints_but_continues_above() {
    let tmp = TempDir::new().unwrap();
    let outer = tmp.path().join("outer");
    let mid = outer.join("preexisting-bind");
    let leaf = mid.join("hooks");
    std::fs::create_dir_all(&leaf).unwrap();

    // Synthetic: treat `preexisting-bind` as already a mountpoint.
    let pin = ancestors_to_pin_as_mountpoints_with(&leaf, |p| p == mid);
    assert!(
        pin.iter().any(|p| p == &outer),
        "must pin renameable ancestor ABOVE an intermediate mountpoint: {pin:?}"
    );
    assert!(
        !pin.iter().any(|p| p == &mid),
        "must NOT re-bind an already-mounted ancestor: {pin:?}"
    );
    assert!(
        !pin.iter().any(|p| p == Path::new("/")),
        "must never pin /: {pin:?}"
    );

    let sources = [GlobalHookSource {
        path: leaf,
        kind: GlobalHookSourceKind::ConfiguredSource,
    }];
    // With the real mountpoint detector nothing under a temp dir is a mount, so the full chain comes back
    let rootward = unique_ancestors_rootward(&sources);
    for w in rootward.windows(2) {
        let [a, b] = w else {
            continue;
        };
        assert!(
            a.components().count() <= b.components().count(),
            "rootward order broken: {rootward:?}"
        );
    }
}
