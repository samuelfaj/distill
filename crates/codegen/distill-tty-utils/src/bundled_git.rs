//! The MinGit the grok Windows installer places under
//! `%LOCALAPPDATA%\grok\git\<version>\cmd\git.exe`.
//!
//! Located once per process. A production spawn that resolves `git` by name
//! calls [`prepend_bundled_git_path`] itself to put the `cmd` directory ahead
//! of the user's `PATH` entries in the child's `PATH`, so that child resolves
//! `git` to the version grok was tested with (a caller that set its own
//! `PATH` on the `Command` keeps it, prepended; a caller that replaced the
//! environment says so with [`PathBase::ExplicitOnly`] and never receives this
//! process's `PATH`; `cmd.exe`'s cwd-first lookup is untouched). The detach
//! helpers do not prepend: they only detach, so a hermetic test child that
//! `env_clear`ed and installed its own `PATH` keeps exactly that. The user's
//! own `PATH` and git installation are never modified. Off Windows, or when
//! the payload is absent, nothing changes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One installed MinGit payload.
#[derive(Debug)]
pub struct BundledGit {
    /// `<version>\cmd`, the directory prepended to children's `PATH`.
    pub cmd_dir: PathBuf,
    /// `<version>\cmd\git.exe`, the launcher that sets up MinGit's own paths.
    pub exe: PathBuf,
    /// The directory holding MinGit's helper executables (`git-upload-pack.exe`
    /// and the `git-remote-*` transports): `<tree>\libexec\git-core`, else
    /// `<tree>\bin`, for the first platform tree present (`mingw64`,
    /// `clangarm64` on ARM64 MinGit, `clang64`, `mingw32`). What a `file://`
    /// transport that spawns `git-upload-pack` by name needs on its `PATH`,
    /// and the `GIT_EXEC_PATH` matching `exe`. Never `cmd`: it carries a
    /// `git-upload-pack.exe` wrapper but not the exec tree (`git-remote-https`
    /// and the rest), so naming it as `GIT_EXEC_PATH` would break transports.
    /// `None` when no tree has the helper.
    pub helper_dir: Option<PathBuf>,
}

impl BundledGit {
    /// The platform tree's `bin` beside [`Self::helper_dir`]: where MinGit
    /// keeps the DLLs its helpers load (`libcurl`, `libiconv`, `zlib`, the
    /// runtime). `cmd\git.exe` adds it itself, but a helper spawned by name
    /// (`git-upload-pack` from gix's `file://` transport) does not start
    /// without it on `PATH`. `None` when there is no helper dir or the tree
    /// has no `bin`.
    #[must_use]
    pub fn dll_dir(&self) -> Option<PathBuf> {
        let helper = self.helper_dir.as_deref()?;
        let bin = if helper.file_name().is_some_and(|n| n == "bin") {
            helper.to_path_buf()
        } else {
            // `<tree>\libexec\git-core` -> `<tree>\bin`.
            helper.parent()?.parent()?.join("bin")
        };
        bin.is_dir().then_some(bin)
    }

    /// Whether helpers spawned by name can run from this payload: a
    /// [`Self::helper_dir`] and its [`Self::dll_dir`] both exist. The single
    /// definition of "complete" that the payload picker ranks by and that
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.helper_dir.is_some() && self.dll_dir().is_some()
    }
}

/// The bundled MinGit of this machine, memoized for the process lifetime.
/// `None` off Windows, when `%LOCALAPPDATA%` is unset, or when no payload is
/// installed.
#[must_use]
pub fn bundled_git() -> Option<&'static BundledGit> {
    static FOUND: OnceLock<Option<BundledGit>> = OnceLock::new();
    FOUND.get_or_init(locate).as_ref()
}

fn locate() -> Option<BundledGit> {
    if !cfg!(windows) {
        return None;
    }
    let local = std::env::var_os("LOCALAPPDATA")?;
    bundled_git_in(&PathBuf::from(local).join("grok").join("git"))
}

/// The newest usable `<root>\<version>` payload under `root`: `cmd\git.exe`
/// plus a platform tree holding `git-upload-pack.exe` and the `bin` with the
/// DLLs the helpers load ([`BundledGit::is_usable`]). Several versions
/// coexist across an update, and an update can leave a newer tree with the
/// launcher but no helpers, or with helpers but no `bin`, so usability ranks
/// above version: a usable older install beats a half-installed newer one
/// (`hermetic_git` pins the choice with no PATH fallback, so a payload whose
/// helpers are missing or cannot start would fail every `file://` helper spawn
/// for the process lifetime). Only when no payload is usable does the newest
/// helper-carrying one win, then the newest launcher-only one. Hidden entries
/// are never versions: the installer extracts into `.staging-*` and deletes
/// stale ones, so a tree there may vanish under a process that picked it.
fn bundled_git_in(root: &Path) -> Option<BundledGit> {
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = entry.file_name().to_str()?.to_owned();
            if version.starts_with('.') {
                return None;
            }
            bundled_git_at(&entry.path()).map(|git| (version, git))
        })
        .max_by(|(a, ga), (b, gb)| {
            ga.is_usable()
                .cmp(&gb.is_usable())
                .then_with(|| ga.helper_dir.is_some().cmp(&gb.helper_dir.is_some()))
                .then_with(|| version_key(a).cmp(&version_key(b)))
                .then_with(|| a.cmp(b))
        })
        .map(|(_, git)| git)
}

/// The payload in one `<root>\<version>` directory, if it has the launcher.
/// [`BundledGit::is_usable`] on the result is the completeness test an
/// installer must apply before publishing or skipping a version directory,
/// so that it and the picker above agree.
#[must_use]
pub fn bundled_git_at(version_dir: &Path) -> Option<BundledGit> {
    let exe = version_dir.join("cmd").join("git.exe");
    exe.is_file().then(|| {
        let cmd_dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let helper_dir = helper_dir_in(version_dir);
        BundledGit {
            cmd_dir,
            exe,
            helper_dir,
        }
    })
}

/// The platform tree of `version` that holds `git-upload-pack.exe`, probed in
/// MinGit's layout order (`libexec\git-core` before `bin`, x64 first).
fn helper_dir_in(version: &Path) -> Option<PathBuf> {
    PLATFORM_TREES
        .iter()
        .flat_map(|tree| {
            [
                version.join(tree).join("libexec").join("git-core"),
                version.join(tree).join("bin"),
            ]
        })
        .find(|dir| dir.join("git-upload-pack.exe").is_file())
}

/// MinGit's per-platform trees, in the order they are probed: x64 first (the
/// installer's default payload), then the ARM64 build, then the others MinGit
/// has shipped.
const PLATFORM_TREES: &[&str] = &["mingw64", "clangarm64", "clang64", "mingw32"];

/// Numeric dot-separated components (`2.47.1.windows.2` -> `[2, 47, 1, 2]`)
/// so `2.50.0` outranks `2.9.1`. The order the picker ranks version
/// directories by; an installer pruning them must use the same one.
#[must_use]
pub fn version_key(version: &str) -> Vec<u64> {
    version
        .split('.')
        .filter_map(|part| part.parse().ok())
        .collect()
}

/// Base for the child's `PATH` when `cmd` carries no explicit `PATH` of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathBase {
    /// This process's `PATH`: the child inherits the environment.
    Process,
    /// Nothing: the caller replaced the environment (`env_clear`), so this
    /// process's `PATH` must not leak back into it; only an explicit `PATH`
    /// is extended. std does not expose `env_clear`, so the caller says so.
    ExplicitOnly,
}

/// Prepend the bundled git's `cmd` directory to the child's `PATH` (no-op
/// without a payload). Production spawns that resolve `git` by name call this
/// with the base that matches their environment handling; the detach helpers
/// deliberately do not.
pub fn prepend_bundled_git_path(cmd: &mut std::process::Command, base: PathBase) {
    if let Some(git) = bundled_git() {
        prepend_child_path(cmd, &git.cmd_dir, base);
    }
}

/// Put `dir` first on the child's `PATH`. Honors a `PATH` the caller already
/// set on `cmd` (and a caller's explicit removal); without one, `base` says
/// whether this process's `PATH` may serve as the tail. Idempotent.
pub fn prepend_child_path(cmd: &mut std::process::Command, dir: &Path, base: PathBase) {
    use std::ffi::OsString;

    let explicit = cmd
        .get_envs()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.map(OsString::from));
    let tail = match (explicit, base) {
        // The caller removed PATH on purpose; do not resurrect it.
        (Some(None), _) => return,
        (Some(Some(value)), _) => value,
        (None, PathBase::Process) => std::env::var_os("PATH").unwrap_or_default(),
        (None, PathBase::ExplicitOnly) => return,
    };
    if std::env::split_paths(&tail)
        .next()
        .is_some_and(|first| first == dir)
    {
        return;
    }
    let mut path = dir.as_os_str().to_owned();
    if !tail.is_empty() {
        path.push(if cfg!(windows) { ";" } else { ":" });
        path.push(&tail);
    }
    cmd.env("PATH", path);
}

#[cfg(test)]
#[path = "bundled_git_tests.rs"]
mod tests;
