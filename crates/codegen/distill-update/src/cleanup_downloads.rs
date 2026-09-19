//! Prune versioned leftovers in `~/.grok/downloads/` after a successful install.
//! Skip a leftover if a live process is executing it (best-effort).

use std::path::{Path, PathBuf};

use crate::auto_update::STALE_TMP_AGE;

pub(crate) async fn cleanup_old_downloads(dir: &Path, bin_prefix: &str, current_version: &str) {
    cleanup_old_downloads_with(dir, bin_prefix, current_version, executable_is_in_use).await;
}

pub(crate) async fn cleanup_old_downloads_with(
    dir: &Path,
    bin_prefix: &str,
    current_version: &str,
    is_in_use: impl Fn(&Path) -> bool,
) {
    let prefix = format!("{bin_prefix}-");
    let current_semver = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: invalid current version '{current_version}': {e}"
            );
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(
                "cleanup_old_downloads: failed to read {}: {e}",
                dir.display()
            );
            return;
        }
    };

    let mut versioned: Vec<(semver::Version, String)> = Vec::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(&prefix) {
            continue;
        }
        // A fresh `.tmp` may be a concurrent updater's in-flight download.
        if name.contains(".tmp") {
            let stale = match entry.metadata().await.and_then(|m| m.modified()) {
                Ok(modified) => std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|age| age > STALE_TMP_AGE)
                    .unwrap_or(false),
                Err(_) => false,
            };
            if stale && let Err(e) = tokio::fs::remove_file(entry.path()).await {
                tracing::warn!("failed to remove stale temp file {name}: {e}");
            }
            continue;
        }
        if let Ok(ft) = entry.file_type().await
            && ft.is_symlink()
        {
            continue;
        }
        let suffix = &name[prefix.len()..];
        if !suffix.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let Some(ver_str) = crate::version::version_from_versioned_binary_name(&name, bin_prefix)
        else {
            continue;
        };
        if let Ok(v) = semver::Version::parse(&ver_str) {
            if v == current_semver {
                continue;
            }
            versioned.push((v, name));
        }
    }

    versioned.sort_by(|a, b| b.0.cmp(&a.0));

    for (_, name) in versioned.iter().skip(1) {
        let path = dir.join(name);
        let fresh = tokio::fs::metadata(&path)
            .await
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= STALE_TMP_AGE);
        if fresh {
            continue;
        }
        if is_in_use(&path) {
            tracing::debug!(
                path = %path.display(),
                "keeping versioned download still executed by a live process"
            );
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!("failed to remove old binary {name}: {e}");
        }
    }
}

/// On macOS, treat an unreadable process table as in-use so a live binary
/// is not deleted. Linux can unlink a mapped file without killing the
/// process, so a scan error is treated as not-in-use.
pub(crate) fn executable_is_in_use(path: &Path) -> bool {
    match any_process_executing(path) {
        Ok(in_use) => in_use,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not list processes executing this binary"
            );
            cfg!(target_os = "macos")
        }
    }
}

fn any_process_executing(path: &Path) -> std::io::Result<bool> {
    let target = match ExecutableId::from_path(path) {
        Ok(id) => id,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for candidate in running_executable_paths()? {
        if target.matches(&candidate) {
            return Ok(true);
        }
    }
    Ok(false)
}

struct ExecutableId {
    canonical: Option<PathBuf>,
    #[cfg(unix)]
    dev_ino: Option<(u64, u64)>,
}

impl ExecutableId {
    fn from_path(path: &Path) -> std::io::Result<Self> {
        let canonical = dunce::canonicalize(path).ok();
        #[cfg(unix)]
        let dev_ino = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
        };
        #[cfg(unix)]
        if canonical.is_none() && dev_ino.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary has no path or inode",
            ));
        }
        #[cfg(not(unix))]
        if canonical.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "binary has no canonical path",
            ));
        }
        Ok(Self {
            canonical,
            #[cfg(unix)]
            dev_ino,
        })
    }

    fn matches(&self, candidate: &Path) -> bool {
        if let (Some(a), Ok(b)) = (self.canonical.as_ref(), dunce::canonicalize(candidate))
            && a == &b
        {
            return true;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let (Some((dev, ino)), Ok(meta)) = (self.dev_ino, std::fs::metadata(candidate))
                && meta.dev() == dev
                && meta.ino() == ino
            {
                return true;
            }
        }
        false
    }
}

fn running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        macos_running_executable_paths()
    }
    #[cfg(target_os = "linux")]
    {
        linux_running_executable_paths()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(Vec::new())
    }
}

/// `proc_listallpids` returns a pid count. A too-small buffer returns how many
/// fit, so a full buffer is truncated — grow and retry, then fail closed.
#[cfg(target_os = "macos")]
fn macos_running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    // SAFETY: `proc_listallpids(NULL, 0)` is the documented size probe and
    // writes nothing.
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut cap = (needed as usize).saturating_mul(2).saturating_add(32);
    let mut pids = None;
    for _ in 0..4 {
        let mut buf = vec![0i32; cap];
        let byte_len = buf.len().saturating_mul(std::mem::size_of::<i32>());
        // SAFETY: `buf` is `byte_len` bytes of i32 slots, matching the size arg.
        let filled = unsafe {
            libc::proc_listallpids(
                buf.as_mut_ptr().cast(),
                i32::try_from(byte_len).unwrap_or(i32::MAX),
            )
        };
        if filled <= 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = filled as usize;
        if n < buf.len() {
            buf.truncate(n);
            pids = Some(buf);
            break;
        }
        cap = n.saturating_mul(2).saturating_add(32);
    }
    let pids = pids.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Interrupted, "process list truncated")
    })?;

    let mut out = Vec::new();
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        // SAFETY: `buf` is PROC_PIDPATHINFO_MAXSIZE, the required dest size.
        let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        if n > 0
            && let Some(bytes) = buf.get(..n as usize)
            && let Ok(s) = std::str::from_utf8(bytes)
        {
            out.push(PathBuf::from(s));
        }
    }
    // A pid list with no resolvable paths is a failed scan, not "nothing in use".
    if out.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no executable paths from process table",
        ));
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn linux_running_executable_paths() -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let is_pid = name
            .to_str()
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid {
            continue;
        }
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            out.push(exe);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "cleanup_downloads_tests.rs"]
mod tests;
