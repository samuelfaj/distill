// Modified for Distill by Samuel Fajreldines, 2026.
//! Verifies that a profile's read-deny enforcement is durable in this mount namespace.
//! The `__GROK_INSIDE_BWRAP` marker and sentinel are reproducible by a caller, so neither proves identity.
//! Strict deny targets must instead be exact read-only mountpoints.
//! Namespace seccomp prevents those mounts from being changed after verification.
//! The sentinel only makes an empty dynamic deny set fail closed outside the expected mount-namespace shape.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// Name of the fixed directory the bwrap re-exec always self-binds read-only.
#[cfg(target_os = "linux")]
const SENTINEL_DIR_NAME: &str = "sandbox-bwrap-sentinel";

#[cfg(target_os = "linux")]
const MOUNTINFO_MAX_BYTES: u64 = 8 * 1024 * 1024;
#[cfg(target_os = "linux")]
const STATX_REQUIRED_MASK: u32 = 0x1003;
#[cfg(target_os = "linux")]
const FILE_TYPE_MASK: u16 = 0o170000;
#[cfg(target_os = "linux")]
const FILE_TYPE_SYMLINK: u16 = 0o120000;
// Read only by the enforce-gated `verify_path_masked`.
#[cfg(all(feature = "enforce", target_os = "linux"))]
const FILE_TYPE_SOCKET: u16 = 0o140000;
#[cfg(all(feature = "enforce", target_os = "linux"))]
const PERMISSION_MASK: u16 = 0o7777;

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct MountInfoEntry {
    id: u64,
    mountpoint: PathBuf,
    is_read_only: bool,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatx {
    stx_mask: u32,
    _before_mode: [u8; 24],
    stx_mode: u16,
    _before_mount_id: [u8; 114],
    stx_mnt_id: u64,
    _after_mount_id: [u8; 104],
}

#[cfg(target_os = "linux")]
struct PathStatus {
    mount_id: u64,
    mode: u16,
}

/// Create the sentinel directory on the host before the re-exec, replacing a hostile non-directory or symlink entry.
/// `create_dir_all` would silently accept a symlink-to-directory, and bwrap would then mount onto its target.
/// The re-exec must not proceed without it: the inner verification requires the read-only sentinel mount unconditionally.
#[cfg(target_os = "linux")]
pub(crate) fn ensure_bwrap_sentinel_dir() -> Result<PathBuf, String> {
    let parent = crate::paths::distill_home();
    std::fs::create_dir_all(&parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    ensure_sentinel_dir_under(&parent)
}

#[cfg(target_os = "linux")]
fn ensure_sentinel_dir_under(parent: &Path) -> Result<PathBuf, String> {
    let path = parent.join(SENTINEL_DIR_NAME);
    // symlink_metadata does not follow: a symlink (even to a directory) is replaced by a real directory
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            std::fs::remove_file(&path).map_err(|e| {
                format!(
                    "could not replace non-directory sentinel {}: {e}",
                    path.display()
                )
            })?;
            std::fs::create_dir(&path)
                .map_err(|e| format!("could not create sentinel {}: {e}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&path)
                .map_err(|e| format!("could not create sentinel {}: {e}", path.display()))?;
        }
        Err(e) => {
            return Err(format!(
                "could not inspect sentinel {}: {e}",
                path.display()
            ));
        }
    }
    // Fail closed on a swap race; the inner O_NOFOLLOW verification is the authoritative gate either way
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(path),
        _ => Err(format!(
            "sentinel {} is not a real directory",
            path.display()
        )),
    }
}

/// Verify the sentinel's expected read-only self-bind shape. This is an empty-set fail-closed gate, not an identity
/// proof: an unprivileged caller can reproduce it with a mount namespace. `statvfs` on the path would also follow a
/// symlink, so this opens the writable parent and the child relative to it with `O_NOFOLLOW`.
#[cfg(all(feature = "enforce", target_os = "linux"))]
pub(crate) fn verify_bwrap_sentinel() -> Result<(), String> {
    verify_sentinel_under(&crate::paths::distill_home())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_sentinel_under(parent: &Path) -> Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let parent_dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(parent)
        .map_err(|e| {
            format!(
                "bwrap sentinel parent {} could not be opened: {e}",
                parent.display()
            )
        })?;
    if fstatvfs_is_read_only(parent_dir.as_raw_fd(), parent)? {
        return Err(format!(
            "bwrap sentinel parent {} is read-only; the sentinel mount shape cannot be verified",
            parent.display()
        ));
    }

    let name = std::ffi::CString::new(SENTINEL_DIR_NAME)
        .map_err(|_| "sentinel name contains interior NUL".to_string())?;
    // SAFETY: the parent fd is open for the duration of the call and the name
    // is a valid NUL-terminated string.
    let child_fd = unsafe {
        libc::openat(
            parent_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if child_fd < 0 {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is not a directory reachable without \
             following symlinks: {}",
            parent.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: openat succeeded, so the fd is valid and owned here.
    let child_dir = unsafe { std::fs::File::from_raw_fd(child_fd) };

    if !fstatvfs_is_read_only(child_dir.as_raw_fd(), &parent.join(SENTINEL_DIR_NAME))? {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is not a read-only mount; this namespace \
             is not bwrap-confined",
            parent.display()
        ));
    }

    let parent_dev = parent_dir
        .metadata()
        .map_err(|e| format!("bwrap sentinel parent metadata failed: {e}"))?
        .dev();
    let child_dev = child_dir
        .metadata()
        .map_err(|e| format!("bwrap sentinel metadata failed: {e}"))?
        .dev();
    if parent_dev != child_dev {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is a mount of a foreign filesystem, not \
             the self-bind the re-exec creates",
            parent.display()
        ));
    }
    Ok(())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn fstatvfs_is_read_only(fd: std::os::fd::RawFd, path: &Path) -> Result<bool, String> {
    // SAFETY: fd is open for the duration of the call; buf is a valid
    // zero-initialized out-pointer.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(fd, &mut buf) } != 0 {
        return Err(format!(
            "bwrap sentinel statvfs on {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(buf.f_flag & libc::ST_RDONLY != 0)
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo_field(field: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let Some(&b) = bytes.get(index) else { break };
        if b == b'\\' {
            let Some(octal) = bytes.get(index + 1..index + 4) else {
                return Err(format!("invalid mountinfo escape in {field:?}"));
            };
            let [d1, d2, d3] = octal else {
                return Err(format!("invalid mountinfo escape in {field:?}"));
            };
            if !((b'0'..=b'7').contains(d1)
                && (b'0'..=b'7').contains(d2)
                && (b'0'..=b'7').contains(d3))
            {
                return Err(format!("invalid mountinfo escape in {field:?}"));
            }
            let decoded_byte = (*d1 - b'0') * 64 + (*d2 - b'0') * 8 + (*d3 - b'0');
            if !matches!(decoded_byte, b' ' | b'\t' | b'\n' | b'\\') {
                return Err(format!("unsupported mountinfo escape in {field:?}"));
            }
            decoded.push(decoded_byte);
            index += 4;
        } else {
            decoded.push(b);
            index += 1;
        }
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(target_os = "linux")]
fn parse_mountinfo_entry(line: &str) -> Result<MountInfoEntry, String> {
    let mut fields = line.split_ascii_whitespace();
    let id = fields
        .next()
        .ok_or_else(|| "mountinfo row has no mount ID".to_string())?
        .parse()
        .map_err(|e| format!("invalid mountinfo ID: {e}"))?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no parent ID".to_string())?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no device".to_string())?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no root".to_string())?;
    let mountpoint = unescape_mountinfo_field(
        fields
            .next()
            .ok_or_else(|| "mountinfo row has no mountpoint".to_string())?,
    )?;
    let options = fields
        .next()
        .ok_or_else(|| "mountinfo row has no mount options".to_string())?;
    let is_read_only = options.split(',').any(|option| option == "ro");
    Ok(MountInfoEntry {
        id,
        mountpoint,
        is_read_only,
    })
}

#[cfg(target_os = "linux")]
fn read_mountinfo() -> Result<Vec<MountInfoEntry>, String> {
    use std::io::Read;

    let file = std::fs::File::open("/proc/self/mountinfo")
        .map_err(|e| format!("read-deny verification could not open mountinfo: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MOUNTINFO_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read-deny verification could not read mountinfo: {e}"))?;
    if bytes.len() as u64 > MOUNTINFO_MAX_BYTES {
        return Err("read-deny verification mountinfo exceeded 8 MiB".to_string());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| format!("read-deny verification mountinfo is not UTF-8: {e}"))?;
    text.lines().map(parse_mountinfo_entry).collect()
}

/// Read mode and visible mount ID without an `O_PATH` open, which access controls may reject.
#[cfg(target_os = "linux")]
fn path_status(path: &Path) -> Result<PathStatus, std::io::Error> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains interior NUL",
        )
    })?;
    // Linux statx is a fixed 256-byte ABI structure.
    // This private definition keeps the query available on older musl libc headers.
    // SAFETY: all-zero bytes are a valid initial state for the kernel output struct.
    let mut statx: LinuxStatx = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated path and statx is a valid
    // fixed-size output buffer for the kernel statx ABI.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            STATX_REQUIRED_MASK,
            &raw mut statx,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if statx.stx_mask & STATX_REQUIRED_MASK != STATX_REQUIRED_MASK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "mount ID or mode is unavailable",
        ));
    }
    Ok(PathStatus {
        mount_id: statx.stx_mnt_id,
        mode: statx.stx_mode,
    })
}

#[cfg(target_os = "linux")]
fn verify_exact_read_only_mount(path: &Path) -> Result<(), String> {
    let status = path_status(path).map_err(|error| {
        format!(
            "read-deny path {} status query failed: {error}",
            path.display()
        )
    })?;
    if status.mode & FILE_TYPE_MASK == FILE_TYPE_SYMLINK {
        return Err(format!(
            "read-deny path {} is a symlink, not a mountpoint",
            path.display()
        ));
    }
    verify_exact_read_only_mount_entry(path, status.mount_id, &read_mountinfo()?)
}

#[cfg(target_os = "linux")]
fn verify_exact_read_only_mount_entry(
    path: &Path,
    mount_id: u64,
    mountinfo: &[MountInfoEntry],
) -> Result<(), String> {
    let Some(entry) = mountinfo
        .iter()
        .rev()
        .find(|entry| entry.id == mount_id && entry.mountpoint == path)
    else {
        return Err(format!(
            "read-deny path {} is not the mountpoint for its visible mount",
            path.display()
        ));
    };
    if !entry.is_read_only {
        return Err(format!(
            "read-deny path {} mount is not read-only",
            path.display()
        ));
    }
    Ok(())
}

/// Installs namespace lockdown before inspecting mounts and requires the unconditional sentinel so an empty deny set
/// fails closed. Returns a message naming the missing sentinel, the first unmasked path, or the resolve/expansion failure
/// when the deny set cannot be recomputed. All of them must refuse startup.
#[cfg(all(feature = "enforce", target_os = "linux"))]
pub fn verify_read_deny_enforced(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    crate::hook_write_deny::ensure_namespace_lockdown()?;
    verify_bwrap_sentinel()?;
    verify_resolved_read_deny_masks(profile, workspace)
}

/// Verify an applicable Devbox `/data` write-deny in the current namespace. This does not require the read-deny sentinel:
/// profiles without read-deny only need the exact read-only `/data` mount plus namespace lockdown. Returns a message when
/// namespace lockdown fails or `/data` is not an exact read-only mountpoint while the profile requires that bind.
#[cfg(target_os = "linux")]
pub fn verify_data_write_deny_enforced(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    if !crate::requires_data_write_deny(profile, workspace) {
        return Ok(());
    }
    crate::hook_write_deny::ensure_namespace_lockdown()?;
    verify_exact_read_only_mount(Path::new("/data"))
        .map_err(|error| format!("devbox /data write-deny could not be verified: {error}"))
}

/// Verify every resolved read-deny path is durably masked in this namespace. The hook write-deny mounts have their own
/// verification, and their plan cannot be rebuilt inside the read-only mounts. Devbox `/data` is verified independently
/// without forcing the read-deny sentinel onto profiles that do not otherwise need it.
#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_resolved_read_deny_masks(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    let config = crate::profiles::load_sandbox_config(workspace);
    let (resolved, auto_sockets) = profile
        .resolve_profile_with_runtime_sockets(workspace, &config)
        .map_err(|e| format!("read-deny verification could not resolve the profile: {e}"))?;
    // Provenance comes from the outer plan handoff; static path membership alone must never make an explicit user deny lenient
    let (exact, globs) = crate::deny::partition_deny_entries(&resolved.deny);
    let mut paths = crate::deny::exact_deny_path_strings(workspace, &exact);
    if !globs.is_empty() {
        let expanded =
            crate::deny::expand_deny_globs(workspace, &globs, crate::deny::DENY_GLOB_CAPS)
                .map_err(|reason| {
                    format!("read-deny verification could not expand deny globs: {reason}")
                })?;
        paths.extend(expanded);
    }
    for path in &paths {
        let path = Path::new(path);
        if auto_sockets.iter().any(|socket| socket == path) {
            verify_runtime_socket_deny(path)?;
        } else {
            verify_path_masked(path)?;
        }
    }
    Ok(())
}

/// Auto runtime-socket denials are existence-gated at launch. A live socket here can have appeared after the plan (daemon
/// start or unlink/recreate). The per-spawn child network filter is the session-long control; refusing startup would not
/// add protection. A vanished endpoint is likewise safe. Only exposed non-socket content at these paths still fails.
#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_runtime_socket_deny(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "runtime-socket deny path {} could not be inspected: {e}",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "runtime-socket deny path {} is a symlink",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_socket() => Ok(()),
        Ok(meta) if meta.permissions().mode() & 0o7777 == 0 => verify_path_masked(path),
        Ok(_) => Err(format!(
            "runtime-socket deny path {} is exposed and is neither a placeholder nor a socket",
            path.display()
        )),
    }
}

/// A strict mask is the bwrap placeholder: an exact read-only mountpoint whose target has no permission bits and is not a live socket.
/// Type/mode checks are defense in depth; the mountpoint and per-mount `ro` flag make chmod unable to restore access after startup.
#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_path_masked(path: &Path) -> Result<(), String> {
    let status = path_status(path).map_err(|error| {
        format!(
            "read-deny path {} has no placeholder mount: {error}",
            path.display()
        )
    })?;
    let file_type = status.mode & FILE_TYPE_MASK;
    if file_type == FILE_TYPE_SYMLINK {
        return Err(format!(
            "read-deny path {} is a symlink, not a placeholder mount",
            path.display()
        ));
    }
    if file_type == FILE_TYPE_SOCKET {
        return Err(format!(
            "read-deny path {} is still a live socket",
            path.display()
        ));
    }
    if status.mode & PERMISSION_MASK != 0 {
        return Err(format!(
            "read-deny path {} is not a no-access placeholder",
            path.display()
        ));
    }
    verify_exact_read_only_mount_entry(path, status.mount_id, &read_mountinfo()?)
}

/// Without `enforce` on Linux nothing is kernel-read-denied (and the shell never requires read-deny).
/// Non-Linux enforces deny paths in-process via Seatbelt, not through a spoofable re-exec marker.
#[cfg(not(all(feature = "enforce", target_os = "linux")))]
pub fn verify_read_deny_enforced(
    _profile: &crate::ProfileName,
    _workspace: &Path,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn verify_data_write_deny_enforced(
    _profile: &crate::ProfileName,
    _workspace: &Path,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, feature = "enforce", target_os = "linux"))]
#[path = "read_deny_verify_tests.rs"]
mod tests;
