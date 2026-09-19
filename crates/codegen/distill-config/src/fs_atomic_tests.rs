use super::*;

fn assert_follow_refused(err: &std::io::Error) {
    assert!(
        err.kind() == std::io::ErrorKind::InvalidInput || is_follow_hard_error(err),
        "expected hop-limit or follow-loop, got {err:?}"
    );
}

#[test]
fn write_atomically_replaces_and_if_absent_refuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("marker");

    write_atomically_if_absent(&path, "first", None).expect("nothing there yet");
    let err = write_atomically_if_absent(&path, "second", None).expect_err("target exists");
    assert_eq!(std::io::ErrorKind::AlreadyExists, err.kind());
    assert_eq!("first", std::fs::read_to_string(&path).expect("read"));

    write_atomically(&path, "third", None).expect("rename replaces");
    assert_eq!("third", std::fs::read_to_string(&path).expect("read"));
    assert_eq!(
        1,
        std::fs::read_dir(dir.path()).expect("dir").count(),
        "no temp file left behind"
    );
}

/// Every racing first writer learns the same outcome: one wins, the file holds that writer's bytes,
/// and every loser sees `AlreadyExists` rather than silently replacing the winner.
#[test]
fn concurrent_first_writers_agree_on_one_winner() {
    for _ in 0..20 {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("marker");
        let barrier = std::sync::Barrier::new(4);
        let results: Vec<Result<(), std::io::ErrorKind>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|i| {
                    let (path, barrier) = (&path, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        write_atomically_if_absent(path, &format!("writer {i}"), None)
                            .map_err(|e| e.kind())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });
        let winners: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(i, r)| r.is_ok().then_some(i))
            .collect();
        let [winner] = winners[..] else {
            panic!("exactly one writer must win: {results:?}");
        };
        assert!(
            results
                .iter()
                .all(|r| matches!(r, Ok(()) | Err(std::io::ErrorKind::AlreadyExists))),
            "{results:?}"
        );
        assert_eq!(
            format!("writer {winner}"),
            std::fs::read_to_string(&path).expect("read")
        );
        assert_eq!(1, std::fs::read_dir(dir.path()).expect("dir").count());
    }
}

fn follow_write(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let dest = resolve_atomic_destination(path)?;
    write_atomically(&dest, contents, None)
}

/// Shared writers replace a planted leaf symlink (managed policy / agent_id).
/// Following would publish onto an outside referent that fail-closed readers
/// never see.
#[cfg(unix)]
#[test]
fn write_atomically_replaces_leaf_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("outside");
    let victim = outside.join("managed.toml");
    std::fs::write(&victim, "keep").expect("seed victim");
    let link = dir.path().join("managed.toml");
    std::os::unix::fs::symlink(&victim, &link).expect("symlink");

    write_atomically(&link, "healed", None).expect("replace link inode");

    let meta = std::fs::symlink_metadata(&link).expect("slot meta");
    assert!(
        !meta.file_type().is_symlink(),
        "managed slot must be a regular file after save: {:?}",
        meta.file_type()
    );
    assert_eq!("healed", std::fs::read_to_string(&link).expect("slot"));
    assert_eq!(
        "keep",
        std::fs::read_to_string(&victim).expect("outside untouched")
    );
}

/// User config.toml / pager.toml opt into following via [`resolve_atomic_destination`].
#[cfg(unix)]
#[test]
fn resolve_atomic_destination_follows_leaf_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("dotfiles");
    std::fs::create_dir_all(&repo).expect("dotfiles dir");
    let target = repo.join("config.toml");
    std::fs::write(&target, "before").expect("seed target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let dest = resolve_atomic_destination(&link).expect("follow");
    assert_eq!(target, dest);
    follow_write(&link, "after").expect("write through symlink");

    let meta = std::fs::symlink_metadata(&link).expect("link meta");
    assert!(
        meta.file_type().is_symlink(),
        "leaf symlink must survive: {:?}",
        meta.file_type()
    );
    assert_eq!(target, std::fs::read_link(&link).expect("read_link"));
    assert_eq!("after", std::fs::read_to_string(&target).expect("target"));
}

#[cfg(unix)]
#[test]
fn write_atomically_if_absent_refuses_existing_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("real");
    std::fs::write(&target, "first").expect("seed");
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let err = write_atomically_if_absent(&link, "second", None).expect_err("target exists");
    assert_eq!(std::io::ErrorKind::AlreadyExists, err.kind());
    assert_eq!(
        "first",
        std::fs::read_to_string(&target).expect("unchanged")
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
fn symlink_chain(dir: &std::path::Path, hops: usize) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = dunce::canonicalize(dir).expect("canonicalize chain root");
    let leaf = dir.join("leaf");
    std::fs::write(&leaf, "before").expect("leaf");
    let mut current = leaf.clone();
    for i in (0..hops).rev() {
        let link = dir.join(format!("l{i}"));
        std::os::unix::fs::symlink(&current, &link).expect("symlink hop");
        current = link;
    }
    (current, leaf)
}

/// `access` is `home/l0` where `home` → `real` and `real` holds a `leaf_hops` chain.
/// Kernel `open` counts the parent hop plus each file hop.
#[cfg(unix)]
fn parent_plus_leaf_chain(
    root: &std::path::Path,
    leaf_hops: usize,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = dunce::canonicalize(root).expect("canonicalize tmp");
    let real = root.join("real");
    std::fs::create_dir_all(&real).expect("real home");
    let (head_in_real, leaf) = symlink_chain(&real, leaf_hops);
    let home = root.join("home");
    std::os::unix::fs::symlink(&real, &home).expect("symlinked home");
    let name = head_in_real.file_name().expect("chain head name");
    (home.join(name), leaf, home)
}

/// A chain at the platform `MAXSYMLINKS` cap must still write the leaf.
#[cfg(unix)]
#[test]
fn resolve_follows_chain_at_platform_hop_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hops = usize::from(SYMLINK_FOLLOW_LIMIT);
    let (head, leaf) = symlink_chain(dir.path(), hops);

    follow_write(&head, "after").expect("platform-limit chain");

    assert!(
        std::fs::symlink_metadata(&head)
            .expect("head")
            .file_type()
            .is_symlink(),
        "head of the chain must remain a symlink"
    );
    assert_eq!("after", std::fs::read_to_string(&leaf).expect("leaf"));
}

/// Darwin `MAXSYMLINKS` is 32. A 33-link chain is past `open`/`read` (`ELOOP`);
/// refuse so tmp+rename cannot publish onto a referent the kernel cannot read.
#[cfg(all(unix, not(target_os = "linux")))]
#[test]
fn resolve_refuses_chain_past_darwin_maxsymlinks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (head, leaf) = symlink_chain(dir.path(), 33);

    let err = resolve_atomic_destination(&head).expect_err("Darwin ELOOP length");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert!(
        std::fs::symlink_metadata(&head)
            .expect("head")
            .file_type()
            .is_symlink()
    );
    assert_eq!("before", std::fs::read_to_string(&leaf).expect("leaf"));
}

/// After the hop cap the leaf is still a symlink: refuse so rename cannot replace it.
#[cfg(unix)]
#[test]
fn resolve_refuses_chain_past_hop_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hops = usize::from(SYMLINK_FOLLOW_LIMIT) + 1;
    let (head, leaf) = symlink_chain(dir.path(), hops);

    let err = resolve_atomic_destination(&head).expect_err("over cap");
    assert_follow_refused(&err);
    assert!(
        std::fs::symlink_metadata(&head)
            .expect("head")
            .file_type()
            .is_symlink()
    );
    assert_eq!("before", std::fs::read_to_string(&leaf).expect("leaf"));
}

/// A leaf-only cap would allow `LIMIT` file hops through a symlinked parent;
/// kernel `open` counts the parent too and `ELOOP`s. Refuse so the write cannot
/// publish onto a referent the caller path cannot read (agent_id / config.toml).
#[cfg(unix)]
#[test]
fn resolve_refuses_parent_symlink_plus_leaf_chain_over_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hops = usize::from(SYMLINK_FOLLOW_LIMIT);
    let (head, leaf, home) = parent_plus_leaf_chain(dir.path(), hops);

    let err = resolve_atomic_destination(&head).expect_err("parent+leaf over cap");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert!(
        std::fs::symlink_metadata(&home)
            .expect("home")
            .file_type()
            .is_symlink()
    );
    assert_eq!("before", std::fs::read_to_string(&leaf).expect("leaf"));
    assert!(
        std::fs::read_to_string(&head).is_err(),
        "caller path must stay unreadably long rather than see a rewritten referent"
    );
}

#[cfg(unix)]
#[test]
fn resolve_follows_parent_symlink_plus_leaf_chain_at_limit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hops = usize::from(SYMLINK_FOLLOW_LIMIT).saturating_sub(1);
    let (head, leaf, home) = parent_plus_leaf_chain(dir.path(), hops);

    follow_write(&head, "after").expect("parent + LIMIT-1 leaf hops");

    assert!(
        std::fs::symlink_metadata(&home)
            .expect("home")
            .file_type()
            .is_symlink()
    );
    assert_eq!("after", std::fs::read_to_string(&leaf).expect("leaf"));
    assert_eq!("after", std::fs::read_to_string(&head).expect("via home"));
}

#[cfg(unix)]
#[test]
fn resolve_refuses_symlink_cycle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::os::unix::fs::symlink(&b, &a).expect("a -> b");
    std::os::unix::fs::symlink(&a, &b).expect("b -> a");

    let err = resolve_atomic_destination(&a).expect_err("cycle");
    assert_follow_refused(&err);
    assert!(
        std::fs::symlink_metadata(&a)
            .expect("a")
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(&b)
            .expect("b")
            .file_type()
            .is_symlink()
    );
}

/// `file/../victim` must not resolve to `victim` (kernel `ENOTDIR`).
#[cfg(unix)]
#[test]
fn resolve_refuses_parent_through_regular_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("file");
    let victim = dir.path().join("victim");
    std::fs::write(&file, "file").expect("file");
    std::fs::write(&victim, "keep").expect("victim");
    let sneaky = file.join("..").join("victim");

    let err = resolve_atomic_destination(&sneaky).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("victim"));

    let err = write_atomically(&sneaky, "clobber", None).expect_err("write");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("victim"));
}

/// `missing/../link` must not skip inspection of `link` (kernel `ENOENT`).
#[cfg(unix)]
#[test]
fn resolve_refuses_parent_through_missing_then_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing");
    let target = dir.path().join("target");
    let link = dir.path().join("link");
    std::fs::write(&target, "keep").expect("target");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    let sneaky = missing.join("..").join("link");

    let err = resolve_atomic_destination(&sneaky).expect_err("ENOENT");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!("keep", std::fs::read_to_string(&target).expect("target"));

    let err = write_atomically(&sneaky, "clobber", None).expect_err("write");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!("keep", std::fs::read_to_string(&target).expect("target"));
}

/// `dir/../sibling` is a real directory walk; write the sibling, not a lexical skip.
#[cfg(unix)]
#[test]
fn resolve_parent_through_directory_reaches_sibling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    let victim = dir.path().join("victim");
    let via = nested.join("..").join("victim");

    assert_eq!(
        &victim,
        resolve_atomic_destination(&via).expect("via dir").as_path()
    );
    write_atomically(&via, "ok", None).expect("write sibling");
    assert_eq!("ok", std::fs::read_to_string(&victim).expect("victim"));
}

/// Execute-only (no read) is enough to search; do not require `readdir`.
#[cfg(unix)]
#[test]
fn resolve_parent_through_execute_only_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).expect("nested");
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "keep").expect("victim");
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o100)).expect("chmod 0100");
    let via = nested.join("..").join("victim");

    write_atomically(&via, "ok", None).expect("search does not need list");
    assert_eq!("ok", std::fs::read_to_string(&victim).expect("victim"));
    let _ = std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755));
}

/// `lstat(blocked)` succeeds on mode 000; kernel `blocked/../victim` is EACCES.
#[cfg(unix)]
#[test]
fn resolve_refuses_parent_through_unsearchable_directory() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let blocked = dir.path().join("blocked");
    std::fs::create_dir(&blocked).expect("blocked");
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "keep").expect("victim");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");
    struct Restore<'a>(&'a std::path::Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o755));
        }
    }
    let _restore = Restore(&blocked);
    let via = blocked.join("..").join("victim");

    let run = || {
        let resolve_err = resolve_atomic_destination(&via).err().map(|e| e.kind());
        let write_err = write_atomically(&via, "clobber", None)
            .err()
            .map(|e| e.kind());
        let contents = std::fs::read_to_string(&victim).expect("victim");
        (resolve_err, write_err, contents)
    };

    #[cfg(target_os = "linux")]
    {
        let (resolve_err, write_err, contents) = drop_dac_override_and_search(run);
        assert_eq!(Some(std::io::ErrorKind::PermissionDenied), resolve_err);
        assert_eq!(Some(std::io::ErrorKind::PermissionDenied), write_err);
        assert_eq!("keep", contents);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (resolve_err, write_err, contents) = run();
        match (resolve_err, write_err) {
            (Some(rk), Some(wk)) => {
                assert_eq!(std::io::ErrorKind::PermissionDenied, rk);
                assert_eq!(std::io::ErrorKind::PermissionDenied, wk);
                assert_eq!("keep", contents);
            }
            (None, None) => {
                // uid 0 with DAC: kernel allows the walk; pin that we match rather than skip.
                assert_eq!("clobber", contents);
            }
            other => panic!("resolve/write disagree on unsearchable dir: {other:?}"),
        }
    }
}

/// Drop `CAP_DAC_OVERRIDE` / `CAP_DAC_READ_SEARCH` on this thread so mode-000
/// directories deny search even as uid 0.
#[cfg(target_os = "linux")]
fn drop_dac_override_and_search<T>(f: impl FnOnce() -> T) -> T {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_DAC_OVERRIDE: u32 = 1;
    const CAP_DAC_READ_SEARCH: u32 = 2;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    unsafe extern "C" {
        fn capget(hdrp: *mut CapHeader, datap: *mut CapData) -> i32;
        fn capset(hdrp: *const CapHeader, datap: *const CapData) -> i32;
    }

    let mut hdr = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    assert_eq!(
        0,
        // SAFETY: `hdr`/`data` match `_LINUX_CAPABILITY_VERSION_3` layout.
        unsafe { capget(&mut hdr, data.as_mut_ptr()) },
        "capget"
    );
    let saved = data;
    struct Restore {
        hdr: CapHeader,
        data: [CapData; 2],
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            self.hdr.version = LINUX_CAPABILITY_VERSION_3;
            // SAFETY: `self.data` was captured from a successful `capget`.
            let _ = unsafe { capset(&self.hdr, self.data.as_ptr()) };
        }
    }
    let _restore = Restore { hdr, data: saved };
    let mask = !((1 << CAP_DAC_OVERRIDE) | (1 << CAP_DAC_READ_SEARCH));
    let [first, _] = &mut data;
    first.effective &= mask;
    first.permitted &= mask;
    hdr.version = LINUX_CAPABILITY_VERSION_3;
    assert_eq!(
        0,
        // SAFETY: `data` is the capget buffer with DAC bits cleared.
        unsafe { capset(&hdr, data.as_ptr()) },
        "capset drop"
    );
    f()
}

/// A dangling file symlink is followed by user-config resolve (first save into
/// a not-yet-created dotfiles file). Shared `write_atomically` replaces the link.
#[cfg(unix)]
#[test]
fn dangling_file_symlink_follow_vs_replace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("dotfiles.toml");
    let follow_link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &follow_link).expect("dangling follow");

    let dest = resolve_atomic_destination(&follow_link).expect("follow dangling");
    assert_eq!(target, dest);
    follow_write(&follow_link, "created").expect("create referent");
    assert!(
        std::fs::symlink_metadata(&follow_link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!("created", std::fs::read_to_string(&target).expect("target"));

    let replace_dir = tempfile::tempdir().expect("tempdir");
    let victim = replace_dir.path().join("outside.toml");
    let replace_link = replace_dir.path().join("managed.toml");
    std::os::unix::fs::symlink(&victim, &replace_link).expect("dangling replace");
    write_atomically(&replace_link, "healed", None).expect("replace dangling");
    assert!(
        !std::fs::symlink_metadata(&replace_link)
            .expect("slot")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        "healed",
        std::fs::read_to_string(&replace_link).expect("slot")
    );
    assert!(!victim.exists(), "must not create the planted referent");
}

/// `Path::components()` drops a trailing `/`. Creating a regular `missing-target`
/// would make later reads through the symlink `ENOTDIR`.
#[cfg(unix)]
#[test]
fn resolve_refuses_trailing_slash_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("missing-target/", &link).expect("symlink");

    let err = resolve_atomic_destination(&link).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert!(
        !missing.exists(),
        "must not create a regular file after stripping the trailing slash"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn resolve_refuses_trailing_dot_symlink_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("missing-target/.", &link).expect("symlink");

    let err = resolve_atomic_destination(&link).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert!(!missing.exists(), "must not create a regular sibling");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

/// An existing directory inode at the leaf is `EISDIR`; do not tmp+rename a file onto it.
#[cfg(unix)]
#[test]
fn write_atomically_refuses_existing_directory_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let squat = dir.path().join("config.toml");
    std::fs::create_dir(&squat).expect("directory squat");

    let err = resolve_atomic_destination(&squat).expect_err("EISDIR");
    assert_eq!(std::io::ErrorKind::IsADirectory, err.kind());
    let err = write_atomically(&squat, "clobber", None).expect_err("EISDIR");
    assert_eq!(std::io::ErrorKind::IsADirectory, err.kind());
    assert!(
        squat.is_dir(),
        "directory squat must not be replaced by a regular file"
    );
}

/// Kernel `open("real/")` is `ENOTDIR` when `real` is a regular file; do not overwrite.
#[cfg(unix)]
#[test]
fn resolve_refuses_trailing_slash_over_existing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("real");
    std::fs::write(&target, "keep").expect("seed");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink("real/", &link).expect("symlink");

    let err = resolve_atomic_destination(&link).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&target).expect("unchanged"));
}

/// Trailing `/` or `/.` on the original destination, not only symlink targets.
#[cfg(unix)]
#[test]
fn write_atomically_refuses_trailing_slash_on_destination() {
    let dir = tempfile::tempdir().expect("tempdir");
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "keep").expect("seed");

    let slash = {
        let mut p = victim.clone().into_os_string();
        p.push("/");
        std::path::PathBuf::from(p)
    };
    let err = write_atomically(&slash, "clobber", None).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("unchanged"));

    let dot = {
        let mut p = victim.clone().into_os_string();
        p.push("/.");
        std::path::PathBuf::from(p)
    };
    let err = resolve_atomic_destination(&dot).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    let err = write_atomically(&dot, "clobber", None).expect_err("ENOTDIR");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("unchanged"));
}

/// On Unix `\` is a filename byte, not a directory marker.
#[cfg(unix)]
#[test]
fn path_names_directory_unix_backslash_is_filename() {
    assert!(path_names_directory(std::path::Path::new("foo/")));
    assert!(path_names_directory(std::path::Path::new("foo/.")));
    assert!(path_names_directory(std::path::Path::new(".")));
    assert!(!path_names_directory(std::path::Path::new("foo\\")));
    assert!(!path_names_directory(std::path::Path::new("foo\\.")));
    assert!(!path_names_directory(std::path::Path::new("target\\")));
}

/// Live symlink to a `target\` file must follow, not `ENOTDIR`.
#[cfg(unix)]
#[test]
fn resolve_follows_unix_backslash_filename_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target\\");
    std::fs::write(&target, "before").expect("backslash filename");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let dest = resolve_atomic_destination(&link).expect("follow backslash name");
    assert_eq!(target, dest);
    follow_write(&link, "after").expect("save through backslash name");
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
    assert_eq!("after", std::fs::read_to_string(&target).expect("target"));
}

/// Intermediate directory symlink with a trailing slash is a real walk, not a leaf file.
#[cfg(unix)]
#[test]
fn write_atomically_follows_dir_symlink_with_trailing_slash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("real");
    std::fs::create_dir(&real).expect("real");
    let target = real.join("config.toml");
    std::fs::write(&target, "before").expect("seed");
    let home = dir.path().join("home");
    let mut dir_target = real.clone().into_os_string();
    dir_target.push("/");
    std::os::unix::fs::symlink(&dir_target, &home).expect("dir symlink");
    let via = home.join("config.toml");

    write_atomically(&via, "after", None).expect("via dir symlink");
    assert_eq!("after", std::fs::read_to_string(&target).expect("target"));
    assert!(
        std::fs::symlink_metadata(&home)
            .expect("home")
            .file_type()
            .is_symlink()
    );
}

/// First-write must not follow a dangling dest symlink and create its referent
/// (workspaced keygen: a planted `device.key` link must not redirect the PEM).
#[cfg(unix)]
#[test]
fn write_atomically_if_absent_refuses_dangling_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("outside.key");
    let link = dir.path().join("device.key");
    std::os::unix::fs::symlink(&target, &link).expect("dangling");

    let err = write_atomically_if_absent(&link, "secret", None).expect_err("symlink exists");
    assert_eq!(std::io::ErrorKind::AlreadyExists, err.kind());
    assert!(
        !target.exists(),
        "must not create the referent outside the intended directory"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

#[cfg(windows)]
#[test]
fn path_names_directory_windows_separators() {
    assert!(path_names_directory(std::path::Path::new("foo/")));
    assert!(path_names_directory(std::path::Path::new("foo\\")));
    assert!(path_names_directory(std::path::Path::new("foo/.")));
    assert!(path_names_directory(std::path::Path::new("foo\\.")));
    assert!(path_names_directory(std::path::Path::new(".")));
    assert!(path_names_directory(std::path::Path::new(".\\")));
}

/// Windows `\dotfiles\config.toml` must keep RootDir (drive root), not the link parent.
#[cfg(windows)]
#[test]
fn splice_preserves_windows_root_relative_symlink_target() {
    let mut resolved = std::path::PathBuf::from(r"C:\Users\me\.grok\config.toml");
    let rest = splice_symlink_target(
        &mut resolved,
        std::path::Path::new(r"\dotfiles\config.toml"),
        VecDeque::new(),
    )
    .expect("rooted target");
    for c in rest {
        resolved.push(c);
    }
    assert_eq!(
        std::path::Path::new(r"C:\dotfiles\config.toml"),
        resolved.as_path()
    );
}

/// Drive-relative `C:foo` depends on the process per-drive cwd. Reject it.
#[cfg(windows)]
#[test]
fn splice_rejects_windows_drive_relative_symlink_target() {
    let mut resolved = std::path::PathBuf::from(r"D:\Users\me\.grok\config.toml");
    let err = splice_symlink_target(
        &mut resolved,
        std::path::Path::new(r"C:foo.toml"),
        VecDeque::new(),
    )
    .expect_err("drive-relative target");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
}

#[test]
fn hop_limit_matches_kernel() {
    #[cfg(target_os = "linux")]
    {
        assert_eq!(40, hop_limit_for_target(std::path::Path::new("rel")));
        assert_eq!(40, hop_limit_for_target(std::path::Path::new("/abs")));
    }
    #[cfg(windows)]
    {
        assert_eq!(63, hop_limit_for_target(std::path::Path::new("rel")));
        assert_eq!(63, hop_limit_for_target(std::path::Path::new(r"..\x")));
        assert_eq!(63, hop_limit_for_target(std::path::Path::new(r"\rooted")));
        assert_eq!(63, hop_limit_for_target(std::path::Path::new(r"\l2")));
        assert_eq!(31, hop_limit_for_target(std::path::Path::new(r"C:\abs")));
        assert_eq!(
            31,
            hop_limit_for_target(std::path::Path::new(r"\Device\HarddiskVolume1\foo"))
        );
        assert_eq!(
            31,
            hop_limit_for_target(std::path::Path::new(r"\??\C:\foo"))
        );
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        assert_eq!(32, hop_limit_for_target(std::path::Path::new("rel")));
    }
}

/// Unix `..` after a directory symlink is physical (parent of the target).
#[cfg(unix)]
#[test]
fn resolve_parent_through_dir_symlink_is_physical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(dir.path()).expect("canon");
    let base = root.join("base");
    let profile = root.join("profile");
    std::fs::create_dir_all(&base).expect("base");
    std::fs::create_dir_all(&profile).expect("profile");
    let home = base.join("home");
    std::os::unix::fs::symlink(&profile, &home).expect("home -> profile");
    let logical = base.join("victim");
    let physical = root.join("victim");
    std::fs::write(&logical, "logical").expect("logical");
    std::fs::write(&physical, "physical").expect("physical");
    let via = home.join("..").join("victim");

    assert_eq!(
        physical,
        resolve_atomic_destination(&via).expect("physical ..")
    );
    follow_write(&via, "after").expect("write physical");
    assert_eq!(
        "after",
        std::fs::read_to_string(&physical).expect("physical")
    );
    assert_eq!(
        "logical",
        std::fs::read_to_string(&logical).expect("logical untouched")
    );
}

/// Overlayfs may reuse `st_ino`; identity must still see regular → symlink.
#[cfg(unix)]
#[test]
fn same_file_identity_detects_replaced_leaf() {
    let dir = tempfile::tempdir().expect("tempdir");
    let slot = dir.path().join("slot");
    std::fs::write(&slot, "regular").expect("seed");
    let regular = std::fs::symlink_metadata(&slot).expect("regular meta");
    std::fs::remove_file(&slot).expect("unlink");
    std::os::unix::fs::symlink("/tmp/victim", &slot).expect("plant");
    let planted = std::fs::symlink_metadata(&slot).expect("planted meta");
    assert!(
        !same_file_identity(&regular, &planted),
        "regular → symlink must be a different file identity"
    );
    assert!(same_file_identity(
        &planted,
        &std::fs::symlink_metadata(&slot).expect("still planted")
    ));
}

#[cfg(unix)]
#[test]
fn read_followable_link_target_refuses_regular_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("regular");
    std::fs::write(&path, "x").expect("seed");
    let err = read_followable_link_target(&path).expect_err("not a symlink");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
}

#[cfg(unix)]
#[test]
fn read_followable_link_target_returns_stable_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target.toml");
    std::fs::write(&target, "keep").expect("target");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    assert_eq!(
        target,
        read_followable_link_target(&link).expect("stable follow")
    );
}

/// `readlink` is not subject to `protected_symlinks`; `stat`/`open` is.
/// A foreign symlink in a sticky dir must not be textually followed.
#[cfg(target_os = "linux")]
#[test]
fn resolve_refuses_protected_symlink_in_sticky_dir() {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let enabled = std::fs::read_to_string("/proc/sys/fs/protected_symlinks")
        .map(|s| s.trim() != "0")
        .unwrap_or(true);
    if !enabled {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let sticky = dir.path().join("sticky");
    std::fs::create_dir(&sticky).expect("sticky");
    std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).expect("chmod 1777");
    let victim = dir.path().join("outside.toml");
    std::fs::write(&victim, "keep").expect("victim");
    let link = sticky.join("config.toml");
    std::os::unix::fs::symlink(&victim, &link).expect("symlink");

    unsafe extern "C" {
        fn geteuid() -> u32;
        fn lchown(path: *const std::ffi::c_char, owner: u32, group: u32) -> i32;
    }
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { geteuid() };
    if euid != 0 {
        // Same-uid sticky symlink is allowed; just pin we do not false-deny.
        follow_write(&link, "owner").expect("owner may follow own symlink");
        assert_eq!("owner", std::fs::read_to_string(&victim).expect("victim"));
        return;
    }

    let cstr = std::ffi::CString::new(link.as_os_str().as_bytes()).expect("no NUL");
    // SAFETY: `cstr` is a valid path; lchown reads it only.
    let rc = unsafe { lchown(cstr.as_ptr(), 65534, 65534) };
    if rc != 0 {
        return;
    }

    let err = resolve_atomic_destination(&link).expect_err("protected EACCES");
    assert_eq!(std::io::ErrorKind::PermissionDenied, err.kind());
    let err = follow_write(&link, "clobber").expect_err("write");
    assert_eq!(std::io::ErrorKind::PermissionDenied, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("untouched"));
}

/// Windows `Path::components` treats `/` as a separator; the kernel does not
/// follow a relative substitute such as `subdir/config.toml`.
#[cfg(windows)]
#[test]
fn splice_refuses_windows_relative_forward_slash_target() {
    let mut resolved = std::path::PathBuf::from(r"C:\base\config.toml");
    let err = splice_symlink_target(
        &mut resolved,
        std::path::Path::new("subdir/config.toml"),
        VecDeque::new(),
    )
    .expect_err("slash substitute");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
}

/// Root-relative `\dotfiles/config.toml` is still `SYMLINK_FLAG_RELATIVE`.
/// `has_root()` must not skip slash rejection and invent `C:\dotfiles\...`.
#[cfg(windows)]
#[test]
fn splice_refuses_windows_root_relative_forward_slash_target() {
    let mut resolved = std::path::PathBuf::from(r"C:\Users\me\.grok\config.toml");
    let err = splice_symlink_target(
        &mut resolved,
        std::path::Path::new(r"\dotfiles/config.toml"),
        VecDeque::new(),
    )
    .expect_err("root-relative slash substitute");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
}

#[test]
fn reparse_slash_rejects_relative_root_relative_and_absolute() {
    assert!(reparse_target_has_unreadable_slash(
        br"\dotfiles/config.toml"
    ));
    assert!(!reparse_target_has_unreadable_slash(
        br"\dotfiles\config.toml"
    ));
    assert!(reparse_target_has_unreadable_slash(br"C:\abs/foo"));
    assert!(!reparse_target_has_unreadable_slash(br"C:\abs\foo"));
    assert!(reparse_target_has_unreadable_slash(b"subdir/config.toml"));
}

/// Absolute `C:\safe/file` must not skip slash rejection via `is_absolute`.
#[cfg(windows)]
#[test]
fn splice_refuses_windows_absolute_forward_slash_target() {
    let mut resolved = std::path::PathBuf::from(r"C:\Users\me\.grok\config.toml");
    let err = splice_symlink_target(
        &mut resolved,
        std::path::Path::new(r"C:\safe/file"),
        VecDeque::new(),
    )
    .expect_err("absolute slash substitute");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
}

#[test]
fn nt_native_device_path_is_fully_qualified() {
    assert!(is_windows_nt_native_path(std::path::Path::new(
        r"\Device\HarddiskVolume2\Users\me\config.toml"
    )));
    assert!(is_windows_nt_native_path(std::path::Path::new(
        r"\??\C:\Users\me\config.toml"
    )));
    assert!(reparse_target_is_fully_qualified(std::path::Path::new(
        r"\Device\HarddiskVolume2\Users\me\config.toml"
    )));
    assert!(!is_windows_nt_native_path(std::path::Path::new(
        r"\DeviceX\HarddiskVolume2\foo"
    )));
    assert!(!reparse_target_is_fully_qualified(std::path::Path::new(
        r"\rooted"
    )));
}

/// `\Device\…` must not be walked as drive-root-relative (`X:\Device\…`).
/// Win32 opens it via `\\?\GLOBALROOT\Device\…`.
#[test]
fn splice_maps_nt_native_device_path_for_win32() {
    let mut resolved = std::path::PathBuf::from(r"C:\Users\me\.grok\config.toml");
    let rest = splice_symlink_target(
        &mut resolved,
        std::path::Path::new(r"\Device\HarddiskVolume2\Users\me\config.toml"),
        VecDeque::new(),
    )
    .expect("nt-native target");
    assert!(rest.is_empty());
    assert_eq!(
        std::path::Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolume2\Users\me\config.toml"),
        resolved.as_path()
    );
    let mut dos = std::path::PathBuf::from(r"C:\Users\me\.grok\config.toml");
    splice_symlink_target(
        &mut dos,
        std::path::Path::new(r"\??\C:\Users\me\config.toml"),
        VecDeque::new(),
    )
    .expect("nt dos");
    assert_eq!(
        std::path::Path::new(r"\\?\C:\Users\me\config.toml"),
        dos.as_path()
    );
}

#[test]
fn nt_native_to_win32_maps_device_and_dos_device() {
    assert_eq!(
        Some(r"\\?\GLOBALROOT\Device\HarddiskVolume1\foo".to_owned()),
        nt_native_to_win32(r"\Device\HarddiskVolume1\foo")
    );
    assert_eq!(
        Some(r"\\?\C:\Users\me\config.toml".to_owned()),
        nt_native_to_win32(r"\??\C:\Users\me\config.toml")
    );
    assert_eq!(None, nt_native_to_win32(r"C:\Users\me\config.toml"));
    assert_eq!(None, nt_native_to_win32(r"\rooted"));
}

/// Absolute directory symlink replaces the path; an inner relative `..`
/// file symlink must resolve against the physical target, not the logical dir.
#[cfg(windows)]
#[test]
fn resolve_flattens_windows_absolute_dir_symlink_for_relative_dotdot() {
    use std::os::windows::fs::{symlink_dir, symlink_file};
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let physical = base.join("physical");
    let logical_parent = base.join("logical");
    std::fs::create_dir_all(&physical).expect("physical");
    std::fs::create_dir_all(&logical_parent).expect("logical");
    let dirlink = logical_parent.join("dirlink");
    symlink_dir(&physical, &dirlink).expect("abs dir symlink");

    let logical_target = logical_parent.join("target.toml");
    let physical_target = base.join("target.toml");
    std::fs::write(&logical_target, "logical").expect("logical target");
    std::fs::write(&physical_target, "physical").expect("physical target");

    let link = dirlink.join("config.toml");
    symlink_file(r"..\target.toml", &link).expect("relative ..");

    let dest = resolve_atomic_destination(&link).expect("physical ..");
    assert_eq!(physical_target, dest);
    follow_write(&link, "after").expect("write physical");
    assert_eq!(
        "after",
        std::fs::read_to_string(&physical_target).expect("physical")
    );
    assert_eq!(
        "logical",
        std::fs::read_to_string(&logical_target).expect("logical untouched")
    );
}

/// Junctions keep the logical path; a later relative `..` must not flatten.
#[cfg(windows)]
#[test]
fn resolve_keeps_windows_junction_logical_for_relative_dotdot() {
    use std::os::windows::fs::symlink_file;
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let physical = base.join("physical");
    let logical_parent = base.join("logical");
    std::fs::create_dir_all(&physical).expect("physical");
    std::fs::create_dir_all(&logical_parent).expect("logical");
    let junc = logical_parent.join("junc");
    create_windows_junction(&junc, &physical).expect("junction");

    let logical_target = logical_parent.join("target.toml");
    let physical_target = base.join("target.toml");
    std::fs::write(&logical_target, "logical").expect("logical target");
    std::fs::write(&physical_target, "physical").expect("physical target");

    let link = junc.join("config.toml");
    symlink_file(r"..\target.toml", &link).expect("relative ..");

    let dest = resolve_atomic_destination(&link).expect("logical ..");
    assert_eq!(logical_target, dest);
    follow_write(&link, "after").expect("write logical");
    assert_eq!(
        "after",
        std::fs::read_to_string(&logical_target).expect("logical")
    );
    assert_eq!(
        "physical",
        std::fs::read_to_string(&physical_target).expect("physical untouched")
    );
}

/// Win32 lexically collapses `file\..` and `missing\..` before lookup.
#[cfg(windows)]
#[test]
fn resolve_windows_lexically_collapses_file_and_missing_dotdot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let blocker = base.join("blocker");
    std::fs::write(&blocker, "file").expect("blocker file");
    let dest = base.join("config.toml");

    let via_file = blocker.join("..").join("config.toml");
    assert_eq!(
        dest,
        resolve_atomic_destination(&via_file).expect("file\\..")
    );

    let via_missing = base.join("missing").join("..").join("config.toml");
    assert_eq!(
        dest,
        resolve_atomic_destination(&via_missing).expect("missing\\..")
    );

    follow_write(&via_file, "ok").expect("write after lexical collapse");
    assert_eq!("ok", std::fs::read_to_string(&dest).expect("dest"));
}

/// Drive-relative `C:..\target` must use the logical per-drive cwd, not a
/// `canonicalize` of `C:.` that flattens a junction.
#[cfg(windows)]
#[test]
fn resolve_drive_relative_dotdot_keeps_junction_logical() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let physical = base.join("physical");
    let logical = base.join("logical");
    std::fs::create_dir_all(&physical).expect("physical");
    std::fs::create_dir_all(&logical).expect("logical");
    let junc = logical.join("junc");
    create_windows_junction(&junc, &physical).expect("junction");

    let logical_target = logical.join("target.toml");
    let physical_target = base.join("target.toml");
    std::fs::write(&logical_target, "logical").expect("logical target");
    std::fs::write(&physical_target, "physical").expect("physical target");

    let prefix = match junc.components().next() {
        Some(std::path::Component::Prefix(p)) => p.as_os_str().to_os_string(),
        other => panic!("expected drive prefix, got {other:?}"),
    };
    let mut via = std::path::PathBuf::from(prefix);
    via.push("..");
    via.push("target.toml");

    let prev = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&junc).expect("cd junction");
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }
    let _restore = Restore(prev);

    let dest = resolve_atomic_destination(&via).expect("logical drive-relative ..");
    assert_eq!(logical_target, dest);
}

#[cfg(windows)]
fn create_windows_junction(
    link: &std::path::Path,
    target: &std::path::Path,
) -> std::io::Result<()> {
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("mklink /J failed"))
    }
}

/// A dangling Windows directory reparse is a directory, not a file to create.
#[cfg(windows)]
#[test]
fn resolve_refuses_dangling_windows_directory_symlink() {
    use std::os::windows::fs::symlink_dir;
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing-dir");
    let link = dir.path().join("config.toml");
    symlink_dir(&missing, &link).expect("dir symlink");

    let err = resolve_atomic_destination(&link).expect_err("directory reparse");
    assert_eq!(std::io::ErrorKind::NotADirectory, err.kind());
    assert!(
        !missing.exists(),
        "must not create a regular referent a directory link cannot follow"
    );
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );
}

/// Any fully-qualified hop tightens the remaining budget to 31, independent
/// of whether the FQ hop is first or last.
#[test]
fn mixed_windows_reparse_budget_is_order_independent() {
    fn walk(kinds: &[bool]) -> Result<u8, ()> {
        let mut budget = HopBudget::default();
        let at = std::path::Path::new("hop");
        for &fq in kinds {
            let target = if fq {
                std::path::Path::new(if cfg!(windows) { r"C:\abs" } else { "/abs" })
            } else {
                std::path::Path::new("rel")
            };
            budget.consume_target(target, at).map_err(|_| ())?;
        }
        Ok(budget.hops)
    }
    let sixty_one_rel_then_fq: Vec<bool> = std::iter::repeat_n(false, 61).chain([true]).collect();
    let one_fq_then_sixty_two_rel: Vec<bool> = std::iter::once(true)
        .chain(std::iter::repeat_n(false, 62))
        .collect();
    let thirty_rel_then_fq: Vec<bool> = std::iter::repeat_n(false, 30).chain([true]).collect();
    let one_fq_then_thirty_rel: Vec<bool> = std::iter::once(true)
        .chain(std::iter::repeat_n(false, 30))
        .collect();
    if cfg!(windows) {
        assert!(walk(&sixty_one_rel_then_fq).is_err());
        assert!(walk(&one_fq_then_sixty_two_rel).is_err());
        assert_eq!(Ok(31), walk(&thirty_rel_then_fq));
        assert_eq!(Ok(31), walk(&one_fq_then_thirty_rel));
    } else {
        assert_eq!(
            walk(&sixty_one_rel_then_fq).is_ok(),
            walk(&one_fq_then_sixty_two_rel).is_ok()
        );
    }
}

#[test]
fn follow_hard_error_includes_loop_not_notfound() {
    let loop_err = if cfg!(target_os = "linux") {
        std::io::Error::from_raw_os_error(40)
    } else if cfg!(windows) {
        std::io::Error::from_raw_os_error(1921)
    } else {
        std::io::Error::from_raw_os_error(62)
    };
    assert!(is_follow_hard_error(&loop_err));
    assert!(is_follow_hard_error(&std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "eacces"
    )));
    assert!(!is_follow_hard_error(&std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "dangling"
    )));
}

#[cfg(unix)]
#[test]
fn read_followable_propagates_symlink_loop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::os::unix::fs::symlink(&b, &a).expect("a->b");
    std::os::unix::fs::symlink(&a, &b).expect("b->a");
    let err = read_followable_link_target(&a).expect_err("loop");
    assert!(
        is_follow_hard_error(&err),
        "ELOOP must not be discarded: {err}"
    );
}

#[cfg(unix)]
#[test]
fn followed_id_rejects_decoy_that_is_not_the_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("target.toml");
    let decoy = dir.path().join("decoy.toml");
    std::fs::write(&target, "T").expect("target");
    std::fs::write(&decoy, "D").expect("decoy");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&target, &link).expect("link");
    let decoy_id = follow_file_id(&decoy).expect("decoy id");
    assert!(
        !followed_id_matches_target(&link, &target, decoy_id),
        "ABA decoy must not match the stored target"
    );
    let target_id = follow_file_id(&target).expect("target id");
    assert!(followed_id_matches_target(&link, &target, target_id));
}

#[cfg(unix)]
#[test]
fn require_same_follow_destination_detects_retarget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(&a, "A").expect("a");
    std::fs::write(&b, "B").expect("b");
    let link = dir.path().join("config.toml");
    std::os::unix::fs::symlink(&a, &link).expect("->A");
    let dest = resolve_atomic_destination(&link).expect("A");
    assert_eq!(a, dest);
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(&b, &link).expect("->B");
    let err = require_same_follow_destination(&link, &dest).expect_err("retarget");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
}

#[cfg(unix)]
#[test]
fn write_atomically_resolved_preserves_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("secret.toml");
    std::fs::write(&path, "old").expect("seed");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("0600");
    write_atomically_resolved(&path, "new", Some(0o600)).expect("write");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(0o600, mode);
    assert_eq!("new", std::fs::read_to_string(&path).expect("read"));
}

#[cfg(unix)]
#[test]
fn file_id_nofollow_distinguishes_replaced_symlink() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(&a, "A").expect("a");
    std::fs::write(&b, "B").expect("b");
    let slot = dir.path().join("slot");
    std::os::unix::fs::symlink(&a, &slot).expect("->A");
    let meta1 = std::fs::symlink_metadata(&slot).expect("m1");
    let id1 = file_id_nofollow(&slot, &meta1).expect("id1");
    std::fs::remove_file(&slot).expect("unlink");
    std::os::unix::fs::symlink(&b, &slot).expect("->B");
    let meta2 = std::fs::symlink_metadata(&slot).expect("m2");
    let id2 = file_id_nofollow(&slot, &meta2).expect("id2");
    assert_ne!(id1, id2);
    assert_eq!(id2, file_id_nofollow(&slot, &meta2).expect("stable"));
}

#[cfg(windows)]
#[test]
fn file_id_nofollow_distinguishes_replaced_windows_symlink() {
    use std::os::windows::fs::symlink_file;
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("a.toml");
    let b = dir.path().join("b.toml");
    std::fs::write(&a, "A").expect("a");
    std::fs::write(&b, "B").expect("b");
    let slot = dir.path().join("slot");
    symlink_file(&a, &slot).expect("->A");
    let meta1 = std::fs::symlink_metadata(&slot).expect("m1");
    let id1 = file_id_nofollow(&slot, &meta1).expect("id1");
    std::fs::remove_file(&slot).expect("unlink");
    symlink_file(&b, &slot).expect("->B");
    let meta2 = std::fs::symlink_metadata(&slot).expect("m2");
    let id2 = file_id_nofollow(&slot, &meta2).expect("id2");
    assert_ne!(id1, id2, "Windows BY_HANDLE identity must see the swap");
}

#[cfg(windows)]
#[test]
fn pop_parent_rejects_excess_dotdot_above_volume_root() {
    let mut p = std::path::PathBuf::from(r"D:\");
    pop_parent(&mut p, false).expect_err("no clamp");
    let mut p = std::path::PathBuf::from(r"D:\");
    pop_parent(&mut p, true).expect("clamp");
    assert_eq!(std::path::Path::new(r"D:\"), p.as_path());
}

#[cfg(windows)]
#[test]
fn resolve_verbatim_missing_dotdot_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let victim = base.join("victim");
    std::fs::write(&victim, "keep").expect("victim");
    let verbatim = {
        let abs = base.display().to_string();
        let stripped = abs.strip_prefix(r"\\?\").unwrap_or(&abs);
        // One string: `PathBuf::join` lexically collapses `missing\..` even
        // on verbatim prefixes, which hid the ENOENT this test wants.
        std::path::PathBuf::from(format!(r"\\?\{stripped}\missing\..\victim"))
    };
    let err = resolve_atomic_destination(&verbatim).expect_err("strict ..");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("untouched"));
}

#[cfg(windows)]
#[test]
fn resolve_relative_symlink_missing_dotdot_is_not_found() {
    use std::os::windows::fs::symlink_file;
    let dir = tempfile::tempdir().expect("tempdir");
    let base = std::path::absolute(dir.path()).expect("abs");
    let victim = base.join("victim");
    std::fs::write(&victim, "keep").expect("victim");
    let link = base.join("config.toml");
    symlink_file(r"missing\..\victim", &link).expect("rel ..");
    let err = resolve_atomic_destination(&link).expect_err("strict substitute ..");
    assert_eq!(std::io::ErrorKind::NotFound, err.kind());
    assert_eq!("keep", std::fs::read_to_string(&victim).expect("untouched"));
}

/// A 255-byte basename plus pid/nonce used to exceed NAME_MAX. Temp names
/// must not include the dest basename.
#[cfg(unix)]
#[test]
fn write_atomically_resolved_succeeds_with_name_max_basename() {
    let dir = tempfile::tempdir().expect("tempdir");
    let name = "a".repeat(255);
    let path = dir.path().join(name);
    std::fs::write(&path, "old").expect("seed");
    write_atomically_resolved(&path, "new", None).expect("NAME_MAX write");
    assert_eq!("new", std::fs::read_to_string(&path).expect("read"));
}

/// Ancestor of the bound dest replaced with a symlink to B: the dest inode
/// at the same logical path is no longer A, so publish must refuse.
#[cfg(unix)]
#[test]
fn refuse_changed_destination_inode_detects_ancestor_retarget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nest = dir.path().join("nest");
    std::fs::create_dir(&nest).expect("nest");
    let dest = nest.join("config.toml");
    std::fs::write(&dest, "A").expect("A");
    let bound = follow_file_id(&dest).ok();
    refuse_changed_destination_inode(&dest, bound).expect("stable");

    let nest_a = dir.path().join("nest_a");
    std::fs::rename(&nest, &nest_a).expect("move A aside");
    let other = dir.path().join("other");
    std::fs::create_dir(&other).expect("other");
    std::fs::write(other.join("config.toml"), "B").expect("B");
    std::os::unix::fs::symlink(&other, &nest).expect("nest -> other");

    let err = refuse_changed_destination_inode(&dest, bound).expect_err("inode changed");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!(
        "A",
        std::fs::read_to_string(nest_a.join("config.toml")).expect("A kept")
    );
    assert_eq!(
        "B",
        std::fs::read_to_string(other.join("config.toml")).expect("B kept")
    );
}

/// Symlink identity ignores mutable size/ctime so utimensat/retarget-length
/// cannot yield a false "unstable symlink" on a stable (dev, ino).
#[cfg(unix)]
#[test]
fn unix_symlink_file_id_ignores_size_and_ctime() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("t");
    std::fs::write(&target, "x").expect("t");
    let slot = dir.path().join("slot");
    std::os::unix::fs::symlink(&target, &slot).expect("link");
    let meta1 = std::fs::symlink_metadata(&slot).expect("m1");
    let id1 = file_id_nofollow(&slot, &meta1).expect("id1");
    // Rewrite the referent: symlink inode is unchanged.
    std::fs::write(&target, "longer").expect("grow");
    let meta2 = std::fs::symlink_metadata(&slot).expect("m2");
    assert_eq!(meta1.ino(), meta2.ino());
    assert_eq!(id1, file_id_nofollow(&slot, &meta2).expect("stable"));
}

/// chmod/touch on a regular dest must not look like a dest swap.
#[cfg(unix)]
#[test]
fn unix_file_id_ignores_mode_and_ctime() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("dest.toml");
    std::fs::write(&path, "x").expect("seed");
    let meta1 = std::fs::metadata(&path).expect("m1");
    let id1 = file_id_from_unix_meta(&meta1);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    std::fs::write(&path, "xy").expect("grow ctime");
    let meta2 = std::fs::metadata(&path).expect("m2");
    assert_eq!(meta1.dev(), meta2.dev());
    assert_eq!(meta1.ino(), meta2.ino());
    assert_ne!(meta1.permissions().mode(), meta2.permissions().mode());
    assert_eq!(id1, file_id_from_unix_meta(&meta2));
}

#[test]
fn refuse_changed_destination_inode_fails_closed_when_unbound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("config.toml");
    std::fs::write(&dest, "A").expect("A");
    let err = refuse_changed_destination_inode(&dest, None).expect_err("unbound");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!("A", std::fs::read_to_string(&dest).expect("untouched"));
}

/// Same pathname, new inode after bind: publish must refuse (not fail-open).
#[cfg(unix)]
#[test]
fn write_atomically_bound_refuses_same_path_inode_replace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "A").expect("A");
    let bound = bind_follow_destination(&path).expect("bind A");
    // Overlayfs reuses st_ino after unlink+create; hold A so B gets a new inode.
    let _hold_a = std::fs::File::open(&path).expect("hold A");
    std::fs::remove_file(&path).expect("unlink A");
    std::fs::write(&path, "B").expect("B");
    let err = write_atomically_bound(&bound, "merged", None).expect_err("inode replace");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!("B", std::fs::read_to_string(&path).expect("B kept"));
}

/// Bind while absent, then a file appears: publish must refuse (not clobber B).
#[cfg(unix)]
#[test]
fn write_atomically_bound_refuses_absent_then_created() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let bound = bind_follow_destination(&path).expect("bind missing");
    std::fs::write(&path, "B").expect("created B");
    let err = write_atomically_bound(&bound, "A", None).expect_err("absent then created");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert_eq!("B", std::fs::read_to_string(&path).expect("B kept"));
}

/// chmod of the same inode is not a dest swap, and inherit uses the live mode.
#[cfg(unix)]
#[test]
fn write_atomically_bound_survives_chmod_and_inherits_live_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "old").expect("seed");
    let bound = bind_follow_destination(&path).expect("bind");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    write_atomically_bound(&bound, "new", None).expect("chmod is not identity");
    let mode = std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
    assert_eq!(0o600, mode);
    assert_eq!("new", std::fs::read_to_string(&path).expect("read"));
}

/// Stale `.PID.N.tmp` must not block persist or delete the foreign file.
#[test]
fn write_via_temp_retries_after_create_new_collision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("dest");
    let pid = std::process::id();
    let nonce = WRITE_NONCE.load(std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.path().join(format!(".{pid}.{nonce}.tmp"));
    std::fs::write(&tmp, "foreign").expect("plant foreign tmp");
    write_atomically(&dest, "new", None).expect("retry another nonce");
    assert_eq!("foreign", std::fs::read_to_string(&tmp).expect("foreign"));
    assert_eq!("new", std::fs::read_to_string(&dest).expect("published"));
}

/// Bind missing `A/config.toml`, then replace ancestor `A` with a symlink to B.
#[cfg(unix)]
#[test]
fn write_atomically_bound_refuses_absent_dest_ancestor_retarget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let a = dir.path().join("A");
    let b = dir.path().join("B");
    std::fs::create_dir_all(&a).expect("A");
    std::fs::create_dir_all(&b).expect("B");
    let dest = a.join("config.toml");
    let bound = bind_follow_destination(&dest).expect("bind missing in A");
    std::fs::remove_dir(&a).expect("remove A");
    std::os::unix::fs::symlink(&b, &a).expect("A -> B");
    let err = write_atomically_bound(&bound, "from_a", None).expect_err("ancestor retarget");
    assert_eq!(std::io::ErrorKind::InvalidInput, err.kind());
    assert!(
        !b.join("config.toml").exists(),
        "must not publish into retargeted B"
    );
}

/// chmod-000 slot leaf must still be replaceable through a writable parent.
#[cfg(unix)]
#[test]
fn write_atomically_replaces_unreadable_slot_leaf() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("agent_id");
    std::fs::write(&path, "blocked").expect("seed");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("000");
    write_atomically(&path, "healed", Some(0o600)).expect("replace unreadable leaf");
    assert_eq!("healed", std::fs::read_to_string(&path).expect("healed"));
}
