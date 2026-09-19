//! Atomic file writes, shared by the managed-cache marker, the signature sidecar, and downstream identifier caches (e.g. the telemetry agent id).
//! The temp file is fsynced before it is published, so a crash cannot leave the final path holding a truncated file.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static WRITE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Follow no more hops than the kernel: Linux 40, Darwin/BSD 32, Windows 63 relative / 31 fully-qualified.
#[cfg(not(windows))]
const SYMLINK_FOLLOW_LIMIT: u8 = if cfg!(target_os = "linux") { 40 } else { 32 };
#[cfg(windows)]
const SYMLINK_FOLLOW_LIMIT_RELATIVE: u8 = 63;
#[cfg(windows)]
const SYMLINK_FOLLOW_LIMIT_FULLY_QUALIFIED: u8 = 31;

/// Leaf file-symlink policy for [`resolve_destination`].
#[derive(Clone, Copy)]
enum LeafSymlink {
    /// tmp+rename the referent. A user symlink into a git repo survives.
    Follow,
    /// `rename` replaces the leaf inode (managed slots, project config).
    Replace,
}

/// Follow parent and leaf hops up to the kernel cap, then tmp+rename the referent.
/// `..` through a non-directory is ENOTDIR; through a missing hop is ENOENT.
pub fn resolve_atomic_destination(path: &Path) -> io::Result<PathBuf> {
    resolve_destination(path, LeafSymlink::Follow)
}

/// Like [`resolve_atomic_destination`], but a leaf symlink is left in place so
/// `rename` replaces that inode (project `.grok/config.toml`, managed slots).
pub fn resolve_atomic_slot(path: &Path) -> io::Result<PathBuf> {
    resolve_destination(path, LeafSymlink::Replace)
}

fn resolve_destination(path: &Path, leaf: LeafSymlink) -> io::Result<PathBuf> {
    if path_names_directory(path) {
        return Err(not_a_directory(path));
    }

    let abs = absolute_path_for_resolution(path)?;
    if path_names_directory(&abs) {
        return Err(not_a_directory(path));
    }

    let mut hops = HopBudget::default();
    let mut resolved = PathBuf::new();
    let mut rest = VecDeque::new();
    enqueue_components(&abs, &mut resolved, &mut rest);
    // `\\?\` / reparse substitutes do not lexically collapse `missing\..`.
    let mut strict_dotdot = windows_path_is_verbatim(&abs);

    while let Some(comp) = rest.pop_front() {
        if is_parent_dir_name(&comp) {
            if cfg!(windows) && !strict_dotdot {
                // Ordinary Win32 lexically collapses `file\..` / `missing\..`.
                pop_parent(&mut resolved, true)?;
                continue;
            }
            // Unix, verbatim `\\?\`, and symlink substitutes: probe. Excess
            // `..` at a volume root is an error (no clamp to `D:\victim`).
            let meta = std::fs::symlink_metadata(&resolved)?;
            if !is_walkable_directory(&meta) {
                return Err(not_a_directory(&resolved));
            }
            require_searchable_directory(&resolved)?;
            pop_parent(&mut resolved, false)?;
            continue;
        }
        resolved.push(&comp);
        let meta = match std::fs::symlink_metadata(&resolved) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if rest.iter().any(is_parent_dir_name) {
                    if cfg!(windows) && !strict_dotdot {
                        continue;
                    }
                    return Err(e);
                }
                for leftover in rest {
                    resolved.push(leftover);
                }
                return Ok(resolved);
            }
            Err(e) => return Err(e),
        };
        if !meta.file_type().is_symlink() {
            // Leaf directory inode: kernel rename of a file onto it is EISDIR.
            if rest.is_empty() && meta.is_dir() {
                return Err(is_a_directory(&resolved));
            }
            continue;
        }
        // Bind reparse identity before keep-logical vs flatten. A junction/WCI
        // swap after the first metadata must not keep a stale classification.
        if cfg!(windows) && !rest.is_empty() && meta.file_type().is_symlink() {
            let id_before = file_id_nofollow(&resolved, &meta)?;
            let mid = std::fs::symlink_metadata(&resolved)?;
            if !mid.file_type().is_symlink() {
                resolved.pop();
                rest.push_front(comp);
                continue;
            }
            let id_after = file_id_nofollow(&resolved, &mid)?;
            if id_after != id_before {
                resolved.pop();
                rest.push_front(comp);
                continue;
            }
            if windows_bound_keeps_logical(&id_after) {
                hops.consume_fully_qualified(&resolved)?;
                continue;
            }
            if !is_windows_symlink_dir(&mid) {
                return Err(not_a_directory(&resolved));
            }
        }
        if rest.is_empty() && matches!(leaf, LeafSymlink::Replace) {
            // Shared writers: rename replaces this inode under the resolved parent.
            return Ok(resolved);
        }
        if rest.is_empty() && windows_is_wci(&resolved, &meta)? {
            // WCI has no Win32 substitute; the kernel follow is the dest.
            return windows_kernel_follow(&resolved);
        }
        let target = read_followable_link_target(&resolved)?;
        hops.consume_target(&target, &resolved)?;
        // Leaf file write: a directory-marked target (`foo/`, `foo/.`) or a
        // Windows directory reparse must not become a regular file after
        // `components()` strips the marker.
        if rest.is_empty() && leaf_symlink_names_directory(&meta, &target) {
            return Err(not_a_directory(&resolved));
        }
        let _ = resolved.pop();
        rest = splice_symlink_target(&mut resolved, &target, rest)?;
        // Substitute `missing\..\victim` must keep ENOENT/ENOTDIR provenance.
        strict_dotdot = true;
    }
    Ok(resolved)
}

/// Win32 `GetFullPathName` (logical per-drive cwd, lexical `..`).
/// Do not `join` a drive-relative `C:..` onto the process cwd.
/// Do not `canonicalize` (that flattens a junction in the per-drive cwd).
fn absolute_path_for_resolution(path: &Path) -> io::Result<PathBuf> {
    if cfg!(windows) {
        std::path::absolute(path)
    } else if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn enqueue_components(path: &Path, resolved: &mut PathBuf, rest: &mut VecDeque<OsString>) {
    for c in path.components() {
        match c {
            Component::Prefix(p) => resolved.push(p.as_os_str()),
            Component::RootDir => resolved.push(c),
            Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => {
                rest.push_back(c.as_os_str().to_os_string());
            }
        }
    }
}

/// Apply a symlink's stored target onto `resolved`, then the remaining components.
/// Windows root-relative targets keep Prefix. Drive-relative `C:foo` is rejected
/// (it depends on the process per-drive cwd). NT-native `\Device\…` stays opaque
/// so component walking cannot treat it as `X:\Device\…`.
fn splice_symlink_target(
    resolved: &mut PathBuf,
    target: &Path,
    rest: VecDeque<OsString>,
) -> io::Result<VecDeque<OsString>> {
    if windows_reparse_has_slash(target) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Windows reparse target {} uses '/' which the kernel does not follow",
                target.display()
            ),
        ));
    }
    let mut new_rest = VecDeque::new();
    if is_windows_nt_native_path(target) {
        // Win32 cannot open `\Device\…` / `\??\…`. Map to `\\?\GLOBALROOT` / `\\?\`.
        *resolved = windows_path_for_win32_apis(target);
    } else if target.is_absolute() {
        resolved.clear();
        enqueue_components(target, resolved, &mut new_rest);
    } else if target.has_root() {
        let prefix = resolved.components().next().and_then(|c| match c {
            Component::Prefix(p) => Some(PathBuf::from(p.as_os_str())),
            _ => None,
        });
        resolved.clear();
        if let Some(prefix) = prefix {
            *resolved = prefix;
        }
        enqueue_components(target, resolved, &mut new_rest);
    } else if cfg!(windows)
        && target
            .components()
            .next()
            .is_some_and(|c| matches!(c, Component::Prefix(_)))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "drive-relative Windows reparse target {} depends on the process cwd",
                target.display()
            ),
        ));
    } else {
        enqueue_components(target, resolved, &mut new_rest);
    }
    new_rest.extend(rest);
    Ok(new_rest)
}

#[cfg(test)]
fn hop_limit_for_target(target: &Path) -> u8 {
    HopBudget {
        hops: 0,
        seen_fully_qualified: reparse_target_is_fully_qualified(target),
    }
    .limit()
}

fn reparse_target_is_fully_qualified(target: &Path) -> bool {
    // Root-relative `\l2` is still SYMLINK_FLAG_RELATIVE (63), not the 31-hop FQ cap.
    // Native `\Device\HarddiskVolume…` is fully-qualified even though Rust's
    // `is_absolute` is false (no DOS drive prefix).
    target.is_absolute() || is_windows_nt_native_path(target)
}

fn is_windows_nt_native_path(path: &Path) -> bool {
    nt_native_to_win32(&path.as_os_str().to_string_lossy()).is_some()
}

/// `\Device\HarddiskVolume…` → `\\?\GLOBALROOT\Device\…`; `\??\C:\…` → `\\?\C:\…`.
fn nt_native_to_win32(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix(r"\??\") {
        Some(format!(r"\\?\{rest}"))
    } else if s.starts_with(r"\Device\") {
        Some(format!(r"\\?\GLOBALROOT{s}"))
    } else {
        None
    }
}

fn windows_path_for_win32_apis(path: &Path) -> PathBuf {
    match nt_native_to_win32(&path.as_os_str().to_string_lossy()) {
        Some(mapped) => PathBuf::from(mapped),
        None => path.to_path_buf(),
    }
}

/// Remaining-reparse budget. Any fully-qualified hop tightens the cap to 31.
#[derive(Clone, Copy, Debug, Default)]
struct HopBudget {
    hops: u8,
    seen_fully_qualified: bool,
}

impl HopBudget {
    fn consume_target(&mut self, target: &Path, at: &Path) -> io::Result<()> {
        if reparse_target_is_fully_qualified(target) {
            self.seen_fully_qualified = true;
        }
        let limit = self.limit();
        consume_hop(&mut self.hops, limit, at)
    }

    fn consume_fully_qualified(&mut self, at: &Path) -> io::Result<()> {
        self.seen_fully_qualified = true;
        let limit = self.limit();
        consume_hop(&mut self.hops, limit, at)
    }

    fn limit(self) -> u8 {
        #[cfg(windows)]
        {
            if self.seen_fully_qualified {
                SYMLINK_FOLLOW_LIMIT_FULLY_QUALIFIED
            } else {
                SYMLINK_FOLLOW_LIMIT_RELATIVE
            }
        }
        #[cfg(not(windows))]
        {
            SYMLINK_FOLLOW_LIMIT
        }
    }
}

fn consume_hop(hops: &mut u8, limit: u8, at: &Path) -> io::Result<()> {
    if *hops >= limit {
        return Err(hop_limit_error(at, limit));
    }
    *hops = hops.saturating_add(1);
    Ok(())
}

fn is_parent_dir_name(name: impl AsRef<std::ffi::OsStr>) -> bool {
    name.as_ref() == Component::ParentDir.as_os_str()
}

/// Bind `read_link` to one symlink inode. Refuse a follow probe of a different file.
const FOLLOW_IDENTITY_RETRIES: u8 = 4;

fn read_followable_link_target(link: &Path) -> io::Result<PathBuf> {
    for _ in 0..FOLLOW_IDENTITY_RETRIES {
        let before = std::fs::symlink_metadata(link)?;
        if !before.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path is no longer a symlink: {}", link.display()),
            ));
        }
        let before_id = file_id_nofollow(link, &before)?;
        refuse_protected_sticky_symlink(link, &before)?;
        let target = std::fs::read_link(link)?;
        let mid = std::fs::symlink_metadata(link)?;
        if !mid.file_type().is_symlink() {
            continue;
        }
        let mid_id = file_id_nofollow(link, &mid)?;
        if mid_id != before_id {
            continue;
        }
        match probe_follow(link) {
            Ok(Some(followed_id)) => {
                if !followed_id_matches_target(link, &target, followed_id) {
                    continue;
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        let after = std::fs::symlink_metadata(link)?;
        if !after.file_type().is_symlink() {
            continue;
        }
        let after_id = file_id_nofollow(link, &after)?;
        if after_id != before_id {
            continue;
        }
        return Ok(target);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "unstable symlink at {} (replaced during lookup)",
            link.display()
        ),
    ))
}

fn probe_follow(link: &Path) -> io::Result<Option<FileId>> {
    match follow_file_id(link) {
        Ok(id) => Ok(Some(id)),
        Err(e) if is_follow_hard_error(&e) => Err(e),
        Err(_) => Ok(None),
    }
}

fn is_follow_hard_error(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || is_reparse_limit_os_error(e)
}

fn is_reparse_limit_os_error(e: &io::Error) -> bool {
    match e.raw_os_error() {
        Some(40) if cfg!(target_os = "linux") => true,
        Some(62)
            if cfg!(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd"
            )) =>
        {
            true
        }
        Some(1921) | Some(4392) if cfg!(windows) => true,
        _ => false,
    }
}

fn followed_id_matches_target(link: &Path, target: &Path, followed_id: FileId) -> bool {
    let target_path = if is_windows_nt_native_path(target) {
        windows_path_for_win32_apis(target)
    } else if reparse_target_is_fully_qualified(target) {
        target.to_path_buf()
    } else if let Some(parent) = link.parent() {
        parent.join(target)
    } else {
        target.to_path_buf()
    };
    match follow_file_id(&target_path) {
        Ok(target_id) => target_id == followed_id,
        Err(_) => false,
    }
}

// Identity is the inode, not mutable metadata. chmod/touch/xattr/nlink
// must not look like a dest swap. Overlayfs can reuse `st_ino` after
// unlink+create; file type + symlink target cookie still distinguish that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    is_symlink: bool,
    // Overlayfs inode reuse: nofollow identity of a symlink is the target.
    #[cfg(unix)]
    symlink_cookie: u64,
    #[cfg(windows)]
    volume_serial: u64,
    #[cfg(windows)]
    file_id: [u8; 16],
    // Bound to the same open as `file_id`. A tag swap is a different reparse.
    #[cfg(windows)]
    reparse_tag: u32,
    #[cfg(not(any(unix, windows)))]
    _unused: (),
}

fn file_id_nofollow(path: &Path, meta: &std::fs::Metadata) -> io::Result<FileId> {
    #[cfg(unix)]
    {
        use std::hash::{Hash, Hasher};
        let mut id = file_id_from_unix_meta(meta);
        if meta.file_type().is_symlink()
            && let Ok(target) = std::fs::read_link(path)
        {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            target.as_os_str().hash(&mut hasher);
            id.symlink_cookie = hasher.finish();
        }
        Ok(id)
    }
    #[cfg(windows)]
    {
        let _ = meta;
        windows_file_id(path, true)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (path, meta);
        Ok(FileId { _unused: () })
    }
}

fn follow_file_id(path: &Path) -> io::Result<FileId> {
    #[cfg(unix)]
    {
        let meta = std::fs::metadata(path)?;
        Ok(file_id_from_unix_meta(&meta))
    }
    #[cfg(windows)]
    {
        windows_file_id(path, false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        let _ = std::fs::metadata(path)?;
        Ok(FileId { _unused: () })
    }
}

#[cfg(unix)]
fn file_id_from_unix_meta(meta: &std::fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt as _;
    FileId {
        dev: meta.dev(),
        ino: meta.ino(),
        is_symlink: meta.file_type().is_symlink(),
        symlink_cookie: 0,
    }
}

#[cfg(test)]
fn same_file_identity(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        file_id_from_unix_meta(a) == file_id_from_unix_meta(b)
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Apply Linux `protected_symlinks` to the `lstat`'d inode, not a later `metadata()` decoy.
fn refuse_protected_sticky_symlink(link: &Path, link_meta: &std::fs::Metadata) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        refuse_linux_protected_sticky_symlink(link, link_meta)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (link, link_meta);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn refuse_linux_protected_sticky_symlink(
    link: &Path,
    link_meta: &std::fs::Metadata,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::sync::OnceLock;

    static PROTECTED: OnceLock<bool> = OnceLock::new();
    let enabled = *PROTECTED.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/fs/protected_symlinks")
            .map(|s| s.trim() != "0")
            .unwrap_or(true)
    });
    if !enabled {
        return Ok(());
    }
    let Some(parent) = link.parent() else {
        return Ok(());
    };
    let dir_meta = std::fs::symlink_metadata(parent)?;
    let dir_mode = dir_meta.permissions().mode();
    if dir_mode & 0o1002 != 0o1002 {
        return Ok(());
    }
    let link_uid = link_meta.uid();
    if link_uid == dir_meta.uid() || link_uid == linux_geteuid() {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("protected symlink in sticky directory: {}", link.display()),
    ))
}

#[cfg(target_os = "linux")]
fn linux_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions.
    unsafe { geteuid() }
}

fn is_walkable_directory(meta: &std::fs::Metadata) -> bool {
    meta.is_dir() || is_windows_symlink_dir(meta)
}

/// Ordinary Win32 `C:\..` stays at the drive root when `clamp_volume_root`.
/// A reparse substitute must not clamp excess `..` onto `D:\victim`.
fn pop_parent(resolved: &mut PathBuf, clamp_volume_root: bool) -> io::Result<()> {
    if resolved.pop() {
        return Ok(());
    }
    #[cfg(windows)]
    {
        pop_windows_beyond_root(resolved, clamp_volume_root)
    }
    #[cfg(not(windows))]
    {
        let _ = clamp_volume_root;
        Ok(())
    }
}

#[cfg(windows)]
fn pop_windows_beyond_root(resolved: &mut PathBuf, clamp_volume_root: bool) -> io::Result<()> {
    if resolved.has_root() {
        if clamp_volume_root {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("excess '..' above volume root at {}", resolved.display()),
        ));
    }
    // Logical per-drive cwd (`GetFullPathName`). `canonicalize` follows a
    // junction there and `C:..\target` would write the physical parent.
    *resolved = std::path::absolute(resolved.join("."))?;
    let _ = resolved.pop();
    Ok(())
}

fn windows_path_is_verbatim(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        matches!(
            path.components().next(),
            Some(Component::Prefix(p))
                if matches!(
                    p.kind(),
                    Prefix::Verbatim(_)
                        | Prefix::VerbatimUNC(_, _)
                        | Prefix::VerbatimDisk(_)
                        | Prefix::DeviceNS(_)
                )
        )
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn windows_reparse_has_slash(target: &Path) -> bool {
    cfg!(windows) && reparse_target_has_unreadable_slash(target.as_os_str().as_encoded_bytes())
}

/// Win32 substitutes do not treat `/` as a separator, including absolute ones.
fn reparse_target_has_unreadable_slash(bytes: &[u8]) -> bool {
    bytes.contains(&b'/')
}

/// True when the path names a directory (`foo/`, `foo/.`). `Path::components` drops those markers.
fn path_names_directory(path: &Path) -> bool {
    path_bytes_name_directory(path.as_os_str().as_encoded_bytes())
}

fn path_bytes_name_directory(bytes: &[u8]) -> bool {
    if matches!(bytes, b"." | b"./") {
        return true;
    }
    if bytes.ends_with(b"/") || bytes.ends_with(b"/.") {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(bytes, b".\\") || bytes.ends_with(b"\\") || bytes.ends_with(b"\\.")
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn leaf_symlink_names_directory(meta: &std::fs::Metadata, target: &Path) -> bool {
    path_names_directory(target) || is_windows_symlink_dir(meta)
}

fn is_windows_symlink_dir(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        meta.file_type().is_symlink_dir()
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        false
    }
}

fn windows_bound_keeps_logical(id: &FileId) -> bool {
    #[cfg(windows)]
    {
        const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
        const IO_REPARSE_TAG_WCI_LINK: u32 = 0xA000_0027;
        const IO_REPARSE_TAG_WCI_LINK_1: u32 = 0xA000_1027;
        matches!(
            id.reparse_tag,
            IO_REPARSE_TAG_MOUNT_POINT | IO_REPARSE_TAG_WCI_LINK | IO_REPARSE_TAG_WCI_LINK_1
        )
    }
    #[cfg(not(windows))]
    {
        let _ = id;
        false
    }
}

/// WCI name-surrogates (`IO_REPARSE_TAG_WCI_LINK` / `WCI_LINK_1`). Not Win32 `read_link`.
fn windows_is_wci(path: &Path, meta: &std::fs::Metadata) -> io::Result<bool> {
    #[cfg(windows)]
    {
        // MS-FSCC: WCI_LINK 0xA0000027, WCI_LINK_1 0xA0001027.
        const IO_REPARSE_TAG_WCI_LINK: u32 = 0xA000_0027;
        const IO_REPARSE_TAG_WCI_LINK_1: u32 = 0xA000_1027;
        if !meta.file_type().is_symlink() {
            return Ok(false);
        }
        let tag = windows_reparse_tag(path)?;
        Ok(tag == IO_REPARSE_TAG_WCI_LINK || tag == IO_REPARSE_TAG_WCI_LINK_1)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        let _ = meta;
        Ok(false)
    }
}

/// Kernel-follow a WCI (or any reparse the Win32 substitute cannot name).
fn windows_kernel_follow(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        std::fs::canonicalize(path)
    }
    #[cfg(not(windows))]
    {
        Ok(path.to_path_buf())
    }
}

/// Open for tag/identity with FILE_READ_ATTRIBUTES. Avoid GENERIC_READ.
#[cfg(windows)]
fn windows_open_attributes(path: &Path, open_reparse: bool) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if open_reparse {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(flags)
        .open(path)
}

#[cfg(windows)]
fn windows_file_id(path: &Path, open_reparse: bool) -> io::Result<FileId> {
    use std::os::windows::io::AsRawHandle as _;

    let file = windows_open_attributes(path, open_reparse)?;
    let handle = file.as_raw_handle();
    let (volume_serial, file_id) = windows_file_id_from_handle(handle)?;
    let reparse_tag = if open_reparse {
        windows_reparse_tag_from_handle(handle).unwrap_or(0)
    } else {
        0
    };
    Ok(FileId {
        volume_serial,
        file_id,
        reparse_tag,
    })
}

#[cfg(windows)]
fn windows_file_id_from_handle(handle: *mut core::ffi::c_void) -> io::Result<(u64, [u8; 16])> {
    const FILE_ID_INFO: i32 = 18;

    #[repr(C)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut core::ffi::c_void,
            class: i32,
            info: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    const INFO_SIZE: u32 = {
        let n = std::mem::size_of::<FileIdInfo>();
        assert!(n <= u32::MAX as usize);
        n as u32
    };
    let mut info = FileIdInfo {
        volume_serial_number: 0,
        file_id: [0; 16],
    };
    // SAFETY: `handle` is open; `info` is a writable FILE_ID_INFO (128-bit ReFS id).
    let ok = unsafe {
        GetFileInformationByHandleEx(handle, FILE_ID_INFO, (&raw mut info).cast(), INFO_SIZE)
    };
    if ok != 0 {
        return Ok((info.volume_serial_number, info.file_id));
    }
    windows_legacy_file_index(handle)
}

#[cfg(windows)]
fn windows_legacy_file_index(handle: *mut core::ffi::c_void) -> io::Result<(u64, [u8; 16])> {
    // FILETIME is two DWORDs. Modeling it as `u64` pads after attributes
    // and shifts volume/index, so replacements can share a colliding id.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct FileTime {
        dw_low_date_time: u32,
        dw_high_date_time: u32,
    }

    #[repr(C)]
    struct ByHandleFileInformation {
        dw_file_attributes: u32,
        ft_creation_time: FileTime,
        ft_last_access_time: FileTime,
        ft_last_write_time: FileTime,
        dw_volume_serial_number: u32,
        n_file_size_high: u32,
        n_file_size_low: u32,
        n_number_of_links: u32,
        n_file_index_high: u32,
        n_file_index_low: u32,
    }

    const _: () = assert!(std::mem::size_of::<ByHandleFileInformation>() == 52);

    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut core::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let zero_time = FileTime {
        dw_low_date_time: 0,
        dw_high_date_time: 0,
    };
    let mut info = ByHandleFileInformation {
        dw_file_attributes: 0,
        ft_creation_time: zero_time,
        ft_last_access_time: zero_time,
        ft_last_write_time: zero_time,
        dw_volume_serial_number: 0,
        n_file_size_high: 0,
        n_file_size_low: 0,
        n_number_of_links: 0,
        n_file_index_high: 0,
        n_file_index_low: 0,
    };
    // SAFETY: `handle` is open; `info` is a writable BY_HANDLE_FILE_INFORMATION.
    let ok = unsafe { GetFileInformationByHandle(handle, &raw mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let index = (u64::from(info.n_file_index_high) << 32) | u64::from(info.n_file_index_low);
    let mut file_id = [0u8; 16];
    file_id[..8].copy_from_slice(&index.to_le_bytes());
    Ok((u64::from(info.dw_volume_serial_number), file_id))
}

#[cfg(windows)]
fn windows_reparse_tag_from_handle(handle: *mut core::ffi::c_void) -> io::Result<u32> {
    const FILE_ATTRIBUTE_TAG_INFO: i32 = 9;

    #[repr(C)]
    struct FileAttributeTagInfo {
        file_attributes: u32,
        reparse_tag: u32,
    }

    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut core::ffi::c_void,
            class: i32,
            info: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    const TAG_INFO_SIZE: u32 = {
        let n = std::mem::size_of::<FileAttributeTagInfo>();
        assert!(n <= u32::MAX as usize);
        n as u32
    };
    let mut info = FileAttributeTagInfo {
        file_attributes: 0,
        reparse_tag: 0,
    };
    // SAFETY: `handle` is open; `info` is a writable FILE_ATTRIBUTE_TAG_INFO.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FILE_ATTRIBUTE_TAG_INFO,
            (&raw mut info).cast(),
            TAG_INFO_SIZE,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.reparse_tag)
}

#[cfg(windows)]
fn windows_reparse_tag(path: &Path) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle as _;
    let file = windows_open_attributes(path, true)?;
    windows_reparse_tag_from_handle(file.as_raw_handle())
}

/// Kernel path walk of `dir/..` must search/`X_OK` `dir`. `lstat(dir)` does not.
#[cfg(unix)]
fn require_searchable_directory(dir: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    const X_OK: i32 = 1;
    let cstr = std::ffi::CString::new(dir.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains interior NUL: {}", dir.display()),
        )
    })?;
    // SAFETY: `cstr` is a valid NUL-terminated path; POSIX `access` reads it only.
    let rc = unsafe { access(cstr.as_ptr(), X_OK) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
unsafe extern "C" {
    fn access(pathname: *const std::ffi::c_char, amode: i32) -> i32;
}

#[cfg(not(unix))]
fn require_searchable_directory(dir: &Path) -> io::Result<()> {
    let _ = dir;
    Ok(())
}

fn not_a_directory(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotADirectory,
        format!("not a directory: {}", path.display()),
    )
}

fn is_a_directory(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::IsADirectory,
        format!("is a directory: {}", path.display()),
    )
}

fn hop_limit_error(path: &Path, limit: u8) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "symlink hop limit ({limit}) exhausted at {}; refusing to replace a symlink inode",
            path.display()
        ),
    )
}

/// Unix mode of an existing path, for copying onto a temp before rename.
#[must_use]
pub fn unix_file_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).ok().map(|m| m.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Follow (or slot) destination bound at resolve time: path plus inode identity.
/// Publish must prove this dest is still the same file, or still absent.
/// Parent identity is bound so an absent dest cannot be retargeted via an ancestor swap.
#[derive(Clone, Debug)]
pub struct BoundDest {
    path: PathBuf,
    identity: DestIdentity,
    parent: DestIdentity,
    follow: bool,
    // Open dest fd pins the inode so overlayfs cannot reuse `(dev,ino)` after unlink.
    #[cfg(unix)]
    dest_hold: Option<std::sync::Arc<std::fs::File>>,
}

impl PartialEq for BoundDest {
    fn eq(&self, other: &BoundDest) -> bool {
        self.path == other.path
            && self.identity == other.identity
            && self.parent == other.parent
            && self.follow == other.follow
    }
}

impl Eq for BoundDest {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DestIdentity {
    Absent,
    Unreadable,
    Present(FileId),
}

impl BoundDest {
    #[must_use]
    pub fn as_path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<Path> for BoundDest {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// Bind the follow dest (path + inode) before a read-modify-write.
pub fn bind_follow_destination(path: &Path) -> io::Result<BoundDest> {
    bind_resolved(resolve_atomic_destination(path)?, true)
}

/// Bind a slot dest so `rename` replaces that leaf inode.
pub fn bind_slot_destination(path: &Path) -> io::Result<BoundDest> {
    bind_resolved(resolve_atomic_slot(path)?, false)
}

fn bind_resolved(path: PathBuf, follow: bool) -> io::Result<BoundDest> {
    let parent = bind_parent_identity(&path)?;
    let identity = match dest_file_id(&path, follow) {
        Ok(id) => DestIdentity::Present(id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => DestIdentity::Absent,
        Err(e) if !follow && e.kind() == io::ErrorKind::PermissionDenied => {
            DestIdentity::Unreadable
        }
        Err(e) => return Err(e),
    };
    Ok(BoundDest {
        #[cfg(unix)]
        dest_hold: open_dest_hold(&path, follow, identity),
        path,
        identity,
        parent,
        follow,
    })
}

fn bind_parent_identity(path: &Path) -> io::Result<DestIdentity> {
    let Some(parent) = path.parent() else {
        return Ok(DestIdentity::Absent);
    };
    if parent.as_os_str().is_empty() {
        return Ok(DestIdentity::Absent);
    }
    match std::fs::symlink_metadata(parent) {
        Ok(meta) => Ok(DestIdentity::Present(file_id_nofollow(parent, &meta)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DestIdentity::Absent),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(DestIdentity::Unreadable),
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn open_dest_hold(
    path: &Path,
    follow: bool,
    identity: DestIdentity,
) -> Option<std::sync::Arc<std::fs::File>> {
    if follow && matches!(identity, DestIdentity::Present(_)) {
        std::fs::File::open(path).ok().map(std::sync::Arc::new)
    } else {
        None
    }
}

fn dest_file_id(path: &Path, follow: bool) -> io::Result<FileId> {
    if follow {
        follow_file_id(path)
    } else {
        let meta = std::fs::symlink_metadata(path)?;
        file_id_nofollow(path, &meta)
    }
}

/// Refuse if `slot` no longer resolves to `expected` (path or inode).
pub fn require_same_bound_destination(slot: &Path, expected: &BoundDest) -> io::Result<BoundDest> {
    let now = if expected.follow {
        bind_follow_destination(slot)?
    } else {
        bind_slot_destination(slot)?
    };
    now.prove_same(expected)?;
    Ok(now)
}

impl BoundDest {
    fn prove_same(&self, expected: &BoundDest) -> io::Result<()> {
        if self.path != expected.path
            || self.identity != expected.identity
            || self.parent != expected.parent
        {
            return Err(changed_destination_error(&expected.path, Some(&self.path)));
        }
        Ok(())
    }

    fn prove_unchanged(&self) -> io::Result<()> {
        self.prove_parent_unchanged()?;
        let now = dest_file_id(&self.path, self.follow);
        match (self.identity, now) {
            (DestIdentity::Absent, Err(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            (DestIdentity::Unreadable, Err(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                Ok(())
            }
            (DestIdentity::Unreadable, Ok(_)) => Ok(()),
            (DestIdentity::Present(bound), Ok(now)) if now == bound => {
                self.prove_hold_matches(bound)
            }
            (_, Err(e)) => Err(e),
            _ => Err(changed_destination_error(&self.path, None)),
        }
    }

    fn prove_parent_unchanged(&self) -> io::Result<()> {
        let now = bind_parent_identity(&self.path)?;
        match (self.parent, now) {
            (bound, now) if bound == now => Ok(()),
            (DestIdentity::Absent, DestIdentity::Present(_)) => {
                let Some(parent) = self.path.parent() else {
                    return Err(changed_destination_error(&self.path, None));
                };
                let meta = std::fs::symlink_metadata(parent)?;
                if meta.file_type().is_symlink() || !is_walkable_directory(&meta) {
                    return Err(changed_destination_error(&self.path, None));
                }
                Ok(())
            }
            _ => Err(changed_destination_error(&self.path, None)),
        }
    }

    fn prove_hold_matches(&self, bound: FileId) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(hold) = &self.dest_hold {
            let held = file_id_from_unix_meta(&hold.metadata()?);
            if held != bound {
                return Err(changed_destination_error(&self.path, None));
            }
        }
        #[cfg(not(unix))]
        let _ = bound;
        Ok(())
    }
}

fn changed_destination_error(expected: &Path, now: Option<&Path>) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        match now {
            Some(now) if now != expected => format!(
                "follow destination changed from {} to {}",
                expected.display(),
                now.display()
            ),
            _ => format!(
                "follow destination inode for {} changed before publish",
                expected.display()
            ),
        },
    )
}

/// Refuse if `path` no longer follows to `expected` (read-A / publish-B).
pub fn require_same_follow_destination(path: &Path, expected: &Path) -> io::Result<PathBuf> {
    let now = bind_follow_destination(path)?;
    if now.as_path() != expected {
        return Err(changed_destination_error(expected, Some(now.as_path())));
    }
    Ok(now.path)
}

/// Resolve twice and refuse a retarget between the two walks.
pub fn resolve_follow_destination_stable(path: &Path) -> io::Result<PathBuf> {
    let first = bind_follow_destination(path)?;
    require_same_bound_destination(path, &first).map(|d| d.path)
}

/// tmp+rename onto an already-bound dest. Identity is fail-closed.
pub fn write_atomically_bound(
    dest: &BoundDest,
    contents: &str,
    mode: Option<u32>,
) -> io::Result<()> {
    dest.prove_unchanged()?;
    write_via_temp(&dest.path, contents, mode, |tmp, path| {
        dest.prove_unchanged()?;
        if mode.is_none() {
            apply_current_dest_mode(tmp, path)?;
        }
        dest.prove_unchanged()?;
        std::fs::rename(tmp, path)
    })
}

fn apply_current_dest_mode(tmp: &Path, dest: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Some(mode) = unix_file_mode(dest) {
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (tmp, dest);
    }
    Ok(())
}

/// tmp+rename onto an already-resolved path (no further leaf follow).
/// Prefer [`bind_follow_destination`] + [`write_atomically_bound`] for RMW.
pub fn write_atomically_resolved(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> io::Result<()> {
    let bound = bind_resolved(final_path.to_path_buf(), true)?;
    write_atomically_bound(&bound, contents, mode)
}

#[cfg(test)]
fn refuse_changed_destination_inode(dest: &Path, bound: Option<FileId>) -> io::Result<()> {
    let Some(bound) = bound else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("follow destination for {} was not bound", dest.display()),
        ));
    };
    match follow_file_id(dest) {
        Ok(now) if now == bound => Ok(()),
        Ok(_) => Err(changed_destination_error(dest, None)),
        Err(e) => Err(e),
    }
}

/// Write to a temp file then rename, so a torn write can't leave a half-written file.
/// The temp name is unique per writer (pid and counter) and `create_new`, so concurrent writers don't collide.
/// `mode` (unix only) is applied at temp-file creation, so the final file never exists with looser permissions.
///
/// Parent-directory symlinks are followed. A leaf file-symlink is replaced (managed slots).
/// User `config.toml` and `pager.toml` follow via [`resolve_atomic_destination`].
pub fn write_atomically(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    let dest = bind_slot_destination(final_path)?;
    write_atomically_bound(&dest, contents, mode)
}

/// [`write_atomically`], but the write lands only when `final_path` does not exist yet.
/// `hard_link` refuses an existing target where `rename` would replace it, so of several concurrent
/// first writers exactly one wins; the rest get `AlreadyExists` and the winner's file is untouched.
/// On a filesystem without hard links the write still lands, without that guarantee.
///
/// An existing dest symlink (dangling or live) is `AlreadyExists`.
pub fn write_atomically_if_absent(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    write_via_temp(final_path, contents, mode, |tmp, final_path| {
        let linked = match std::fs::hard_link(tmp, final_path) {
            // No hard links here (exFAT/FAT32, some network mounts): rename lands the file, but a concurrent first writer can replace it
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                return std::fs::rename(tmp, final_path);
            }
            linked => linked,
        };
        let _ = std::fs::remove_file(tmp);
        linked
    })
}

const TMP_NAME_RETRIES: u32 = 32;

fn write_via_temp(
    final_path: &Path,
    contents: &str,
    mode: Option<u32>,
    publish: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = final_path.parent().unwrap_or_else(|| Path::new("."));
    let mut last_exists = None;
    for _ in 0..TMP_NAME_RETRIES {
        let nonce = WRITE_NONCE.fetch_add(1, Ordering::Relaxed);
        // Do not prefix with the dest basename: a 255-byte name plus pid/nonce
        // exceeds NAME_MAX. The dest directory + unique suffix is enough.
        let tmp = dir.join(format!(".{}.{nonce}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        // Only remove a temp this writer created. `AlreadyExists` retries a
        // new nonce; a foreign `.PID.N.tmp` is left untouched.
        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last_exists = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let result = file
            .write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                #[cfg(unix)]
                if let Some(mode) = mode {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
                }
                Ok(())
            })
            .and_then(|()| publish(&tmp, final_path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        return result;
    }
    Err(last_exists.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AlreadyExists, "temp name retries exhausted")
    }))
}

#[cfg(test)]
#[path = "fs_atomic_tests.rs"]
mod tests;
