// Modified for Distill by Samuel Fajreldines, 2026.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{PathBase, bundled_git_at, bundled_git_in, prepend_child_path, version_key};

/// Unique scratch root under the OS temp dir, removed on drop (no tempfile
/// dev-dep for this crate).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "distill-tty-utils-bundled-git-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn install(root: &Path, version: &str, with_exe: bool) {
    let cmd = root.join(version).join("cmd");
    std::fs::create_dir_all(&cmd).unwrap();
    if with_exe {
        std::fs::write(cmd.join("git.exe"), b"").unwrap();
    }
}

/// Place `git-upload-pack.exe` under `rel` inside `<root>\<version>`.
fn install_helper(root: &Path, version: &str, rel: &[&str]) {
    let mut dir = root.join(version);
    for part in rel {
        dir.push(part);
    }
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("git-upload-pack.exe"), b"").unwrap();
}

fn child_path(cmd: &Command) -> Option<Option<&OsStr>> {
    cmd.get_envs()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value)
}

#[test]
fn picks_the_highest_complete_version() {
    let tmp = TempRoot::new("pick");
    install(tmp.path(), "2.9.1", true);
    install(tmp.path(), "2.47.1.windows.1", true);
    // A half-installed newer payload must not win.
    install(tmp.path(), "2.50.0", false);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.47.1.windows.1").join("cmd"));
    assert_eq!(git.exe, git.cmd_dir.join("git.exe"));
    assert_eq!(git.helper_dir, None, "no helper anywhere in this payload");
}

/// Completeness outranks version: a newer tree with the launcher but no
/// helpers (an update that stopped half-way) must not beat a complete older
/// install, since the choice is pinned with no PATH fallback. Only with no
/// complete payload at all does the newest launcher-only one win.
#[test]
fn complete_older_payload_beats_launcher_only_newer_one() {
    let tmp = TempRoot::new("complete");
    install(tmp.path(), "2.47.1", true);
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    install(tmp.path(), "2.50.0", true);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.47.1").join("cmd"));
    assert_eq!(
        git.helper_dir,
        Some(
            tmp.path()
                .join("2.47.1")
                .join("mingw64")
                .join("libexec")
                .join("git-core")
        )
    );
    // Once the newer tree is complete too, version decides again.
    install_helper(tmp.path(), "2.50.0", &["mingw64", "bin"]);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.50.0").join("cmd"));
    assert_eq!(
        git.helper_dir,
        Some(tmp.path().join("2.50.0").join("mingw64").join("bin"))
    );
    // Two launcher-only trees: the newest, as before (nothing better exists).
    let tmp = TempRoot::new("launcher-only");
    install(tmp.path(), "2.47.1", true);
    install(tmp.path(), "2.50.0", true);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.50.0").join("cmd"));
    assert_eq!(git.helper_dir, None);
}

/// Helpers alone do not make a payload usable: `git-upload-pack.exe` under
/// `libexec\git-core` with no `<tree>\bin` beside it cannot start (no DLLs).
/// Such a newer tree must lose to an older one that has both, since the
#[test]
fn usable_older_payload_beats_newer_one_with_helpers_but_no_dlls() {
    let tmp = TempRoot::new("usable");
    let older = tmp.path().join("2.47.1");
    let newer = tmp.path().join("2.50.0");
    install(tmp.path(), "2.47.1", true);
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    std::fs::create_dir_all(older.join("mingw64").join("bin")).unwrap();
    install(tmp.path(), "2.50.0", true);
    install_helper(tmp.path(), "2.50.0", &["mingw64", "libexec", "git-core"]);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, older.join("cmd"), "newer tree has no bin");
    assert!(git.is_usable());
    assert_eq!(git.dll_dir(), Some(older.join("mingw64").join("bin")));
    // Once the newer tree has its bin too, version decides again.
    std::fs::create_dir_all(newer.join("mingw64").join("bin")).unwrap();
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, newer.join("cmd"));
    assert!(git.is_usable());
    // With no usable payload, helpers-without-DLLs still outrank launcher-only.
    let tmp = TempRoot::new("no-usable");
    install(tmp.path(), "2.47.1", true);
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    install(tmp.path(), "2.50.0", true);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.47.1").join("cmd"));
    assert!(git.helper_dir.is_some());
    assert!(!git.is_usable());
}

/// The helper dir is the one that actually holds `git-upload-pack.exe`, in
/// MinGit's layout order: `mingw64\libexec\git-core`, then `mingw64\bin`.
/// `cmd` never qualifies: its `git-upload-pack.exe` is a wrapper without the
/// exec tree beside it.
#[test]
fn helper_dir_is_where_git_upload_pack_lives() {
    let tmp = TempRoot::new("helper");
    install(tmp.path(), "2.47.1", true);
    let version = tmp.path().join("2.47.1");
    assert_eq!(bundled_git_in(tmp.path()).unwrap().helper_dir, None);
    install_helper(tmp.path(), "2.47.1", &["cmd"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().helper_dir,
        None,
        "cmd is not an exec tree"
    );
    install_helper(tmp.path(), "2.47.1", &["mingw64", "bin"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().helper_dir,
        Some(version.join("mingw64").join("bin"))
    );
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().helper_dir,
        Some(version.join("mingw64").join("libexec").join("git-core"))
    );
}

/// The DLL dir is the helper tree's `bin`, whether the helper lives in
/// `libexec\git-core` or in `bin` itself; without a helper there is none.
#[test]
fn dll_dir_is_the_helper_trees_bin() {
    let tmp = TempRoot::new("dll");
    install(tmp.path(), "2.47.1", true);
    let version = tmp.path().join("2.47.1");
    assert_eq!(bundled_git_in(tmp.path()).unwrap().dll_dir(), None);
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().dll_dir(),
        None,
        "no bin beside libexec yet"
    );
    std::fs::create_dir_all(version.join("mingw64").join("bin")).unwrap();
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().dll_dir(),
        Some(version.join("mingw64").join("bin"))
    );
    // Helper directly in bin: the DLL dir is that same directory.
    let tmp = TempRoot::new("dll-bin");
    install(tmp.path(), "2.47.1", true);
    install_helper(tmp.path(), "2.47.1", &["mingw64", "bin"]);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.dll_dir(), git.helper_dir);
}

/// ARM64 MinGit ships `clangarm64\libexec\git-core` and no `mingw64`; the
/// helper dir must be that tree, not `cmd` (which also has the wrapper).
#[test]
fn arm64_payload_helper_dir_is_clangarm64() {
    let tmp = TempRoot::new("arm64");
    install(tmp.path(), "2.47.1", true);
    let version = tmp.path().join("2.47.1");
    install_helper(tmp.path(), "2.47.1", &["cmd"]);
    install_helper(tmp.path(), "2.47.1", &["clangarm64", "libexec", "git-core"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().helper_dir,
        Some(version.join("clangarm64").join("libexec").join("git-core"))
    );
    // x64 wins when both trees are present (the installer's default payload).
    install_helper(tmp.path(), "2.47.1", &["mingw64", "libexec", "git-core"]);
    assert_eq!(
        bundled_git_in(tmp.path()).unwrap().helper_dir,
        Some(version.join("mingw64").join("libexec").join("git-core"))
    );
}

/// A complete tree under a hidden name is an installer's staging directory
/// (or some other non-version entry) and must never be picked, even when it
/// is the only complete one: the installer deletes stale staging trees.
#[test]
fn hidden_entries_are_not_versions() {
    let tmp = TempRoot::new("hidden");
    install(tmp.path(), ".staging-0.2.10", true);
    install_helper(tmp.path(), ".staging-0.2.10", &["mingw64", "bin"]);
    assert!(bundled_git_in(tmp.path()).is_none());
    install(tmp.path(), "2.47.1", true);
    let git = bundled_git_in(tmp.path()).unwrap();
    assert_eq!(git.cmd_dir, tmp.path().join("2.47.1").join("cmd"));
}

#[test]
fn absent_payload_is_none() {
    let tmp = TempRoot::new("absent");
    assert!(bundled_git_in(tmp.path()).is_none());
    assert!(bundled_git_in(&tmp.path().join("missing")).is_none());
    install(tmp.path(), "2.47.1", false);
    assert!(bundled_git_in(tmp.path()).is_none());
}

#[test]
fn bundled_git_at_reports_one_version_dir() {
    let tmp = TempRoot::new("at");
    let dir = tmp.path().join("2.55.0");
    assert!(bundled_git_at(&dir).is_none());
    install(tmp.path(), "2.55.0", false);
    assert!(bundled_git_at(&dir).is_none(), "launcher required");
    install(tmp.path(), "2.55.0", true);
    let git = bundled_git_at(&dir).unwrap();
    assert_eq!(git.cmd_dir, dir.join("cmd"));
    assert!(!git.is_usable(), "launcher only");
    install_helper(tmp.path(), "2.55.0", &["mingw64", "bin"]);
    assert!(bundled_git_at(&dir).unwrap().is_usable());
}

#[test]
fn version_key_is_numeric_not_lexical() {
    assert!(version_key("2.50.0") > version_key("2.9.1"));
    assert_eq!(version_key("2.47.1.windows.2"), vec![2, 47, 1, 2]);
}

#[test]
fn prepend_honors_explicit_path_and_removal() {
    let dir = Path::new("/bundled/cmd");
    let sep = if cfg!(windows) { ";" } else { ":" };

    let mut cmd = Command::new("git");
    cmd.env("PATH", "/caller/bin");
    prepend_child_path(&mut cmd, dir, PathBase::Process);
    assert_eq!(
        child_path(&cmd).flatten().unwrap(),
        OsStr::new(&format!("/bundled/cmd{sep}/caller/bin"))
    );

    // Already first: idempotent.
    prepend_child_path(&mut cmd, dir, PathBase::Process);
    assert_eq!(
        child_path(&cmd).flatten().unwrap(),
        OsStr::new(&format!("/bundled/cmd{sep}/caller/bin"))
    );

    let mut removed = Command::new("git");
    removed.env_remove("PATH");
    prepend_child_path(&mut removed, dir, PathBase::Process);
    assert_eq!(child_path(&removed), Some(None));

    // No explicit PATH: the process PATH is the tail.
    let mut inherited = Command::new("git");
    prepend_child_path(&mut inherited, dir, PathBase::Process);
    let value = child_path(&inherited).flatten().unwrap().to_owned();
    assert_eq!(
        std::env::split_paths(&value).next().unwrap(),
        dir,
        "{value:?}"
    );
}

/// A replaced environment (`env_clear`) is invisible to `get_envs`; the caller
/// must say `ExplicitOnly` so grok's own PATH is not resurrected into it.
#[test]
fn explicit_only_never_resurrects_the_process_path() {
    let dir = Path::new("/bundled/cmd");
    let sep = if cfg!(windows) { ";" } else { ":" };

    let mut cleared = Command::new("git");
    cleared.env_clear();
    prepend_child_path(&mut cleared, dir, PathBase::ExplicitOnly);
    assert_eq!(child_path(&cleared), None);

    // A PATH the policy itself provides is still extended.
    let mut policy_path = Command::new("git");
    policy_path.env_clear();
    policy_path.env("PATH", "/policy/bin");
    prepend_child_path(&mut policy_path, dir, PathBase::ExplicitOnly);
    assert_eq!(
        child_path(&policy_path).flatten().unwrap(),
        OsStr::new(&format!("/bundled/cmd{sep}/policy/bin"))
    );
}
