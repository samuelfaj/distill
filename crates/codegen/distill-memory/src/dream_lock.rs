//! Coordination for background memory consolidation ("dream").
//! [`DreamLock`] is a mutex file acquired with an atomic exclusive create and tagged with an
//! owner token (`<pid> <nonce>`): at most one acquirer can create it, and a guard removes only
//! the file it still owns, so a stale reclaim can never make a guard delete another holder's lock.
//! A separate marker records the last consolidation, stamped only when a dream commits so a crash
//! reopens the gate. [`sessions_since`] counts session files modified after a given timestamp.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MUTEX_FILE_NAME: &str = ".dream-mutex";
const CONSOLIDATED_FILE_NAME: &str = ".dream-consolidated";
/// Pre-upgrade workspaces recorded the last consolidation as this file's mtime; new code never writes it, so it serves as a read-only fallback.
const LEGACY_MARKER_FILE_NAME: &str = ".dream-lock";

fn consolidated_path(mutex_path: &Path) -> PathBuf {
    mutex_path.with_file_name(CONSOLIDATED_FILE_NAME)
}

fn write_consolidated_marker(mutex_path: &Path) -> io::Result<()> {
    let marker = consolidated_path(mutex_path);
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&marker, "")
}

/// The owner id written into the mutex file: `<pid> <nonce>`. The pid drives liveness reclaim; the
/// nonce lets a guard confirm it still owns the file before removing it.
fn owner_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let nonce = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{} {nonce}", std::process::id())
}

/// Parses the PID from a token's first field, or `None` if the body is empty or unparseable.
fn token_pid(content: &str) -> Option<u32> {
    content.split_whitespace().next()?.parse().ok()
}

/// Copy of `crate::util::is_process_alive`, kept local so the memory subsystem can move to its own crate.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    // Signal 0 probes existence; EPERM means alive under a different UID.
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    // SAFETY: OpenProcess returns Err on absence/permission failure;
    // PROCESS_SYNCHRONIZE is the minimum right needed for WaitForSingleObject.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        return false;
    };

    // SAFETY: handle is valid; timeout 0 means "poll, don't block."
    let wait_result = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: handle is owned by us; close regardless of wait result.
    let _ = unsafe { CloseHandle(handle) };

    wait_result == WAIT_TIMEOUT
}

pub struct DreamLock {
    path: PathBuf,
}

impl DreamLock {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            path: workspace_dir.join(MUTEX_FILE_NAME),
        }
    }

    /// Returns the last consolidation timestamp from the marker's mtime, falling back to the pre-upgrade `.dream-lock` mtime, or `None` if neither exists.
    pub fn last_consolidated_at(&self) -> io::Result<Option<SystemTime>> {
        let candidates = [
            consolidated_path(&self.path),
            self.path.with_file_name(LEGACY_MARKER_FILE_NAME),
        ];
        for path in candidates {
            match fs::metadata(&path) {
                Ok(meta) => return Ok(Some(meta.modified()?)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Acquisition is atomic-exclusive: the mutex file is created with `create_new`, so at most one acquirer — intraor inter-process — can create it. When the file already exists it is reclaimed (and the create retried) only if the holder is dead or the file is older than
    /// `stale_secs`; a live, fresh holder returns `Ok(None)`.
    fn try_acquire(&self, stale_secs: u64) -> io::Result<Option<String>> {
        // Bounded so racing acquirers that keep recreating the file cannot spin forever.
        const ACQUIRE_ATTEMPTS: usize = 8;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let token = owner_token();
        for _ in 0..ACQUIRE_ATTEMPTS {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)
            {
                Ok(mut file) => {
                    // Remove our own file if the write fails, so no fresh, unreclaimable mutex is left.
                    if let Err(e) = write!(file, "{token}") {
                        let _ = fs::remove_file(&self.path);
                        return Err(e);
                    }
                    return Ok(Some(token));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if !self.holder_is_reclaimable(stale_secs)? {
                        return Ok(None);
                    }
                    if let Err(e) = fs::remove_file(&self.path)
                        && e.kind() != io::ErrorKind::NotFound
                    {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(None)
    }

    /// Whether the current mutex holder can be reclaimed: the file is gone, older than `stale_secs`, or its token names a PID that is no longer alive.
    /// A fresh unparseable body (empty or garbage) is a holder still writing its token between the exclusive create and the write, so it is left alone; the age check reclaims it once stale.
    fn holder_is_reclaimable(&self, stale_secs: u64) -> io::Result<bool> {
        let meta = match fs::metadata(&self.path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(e),
        };

        let age = SystemTime::now()
            .duration_since(meta.modified()?)
            .unwrap_or_default()
            .as_secs();
        if age >= stale_secs {
            return Ok(true);
        }

        match fs::read_to_string(&self.path) {
            Ok(content) => match token_pid(&content) {
                Some(pid) => Ok(!is_process_alive(pid)),
                None => Ok(false),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e),
        }
    }

    pub fn acquire(&self, stale_secs: u64) -> io::Result<Option<DreamLockGuard>> {
        Ok(self.try_acquire(stale_secs)?.map(|token| DreamLockGuard {
            path: self.path.clone(),
            token,
        }))
    }

    /// Stamps the consolidation marker.
    #[cfg(test)]
    pub(crate) fn record_consolidation(&self) -> io::Result<()> {
        write_consolidated_marker(&self.path)
    }
}

pub struct DreamLockGuard {
    path: PathBuf,
    token: String,
}

impl DreamLockGuard {
    /// Writes the consolidation marker and returns whether it durably landed; `self` drops at the
    /// end, so the mutex is released either way. A `false` return means the caller should treat the
    /// dream as not consolidated (the gate stays open for a retry).
    #[must_use]
    pub fn commit(self) -> bool {
        write_consolidated_marker(&self.path).is_ok()
    }
}

impl Drop for DreamLockGuard {
    /// Release the mutex, but only if we still own it. After a stale reclaim (for example a wall-clock jump) the path may hold another session's token; removing it would let a third session join, so we leave it. A crash skips this, but the token names a now-dead PID that `holder_is_reclaimable` reclaims immediately.
    fn drop(&mut self) {
        if fs::read_to_string(&self.path).is_ok_and(|c| c.trim() == self.token) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Returns sorted file stems of `.md` files in `sessions_dir` modified after `since`, excluding the current session (`exclude_sid8`).
pub fn sessions_since(
    sessions_dir: &Path,
    since: SystemTime,
    exclude_sid8: Option<&str>,
) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(sessions_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut result = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }

        if let Some(exclude) = exclude_sid8
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.ends_with(exclude))
        {
            continue;
        }

        if entry.metadata()?.modified()? > since
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            result.push(stem.to_owned());
        }
    }

    result.sort();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::FileTime;
    use std::time::Duration;
    use tempfile::TempDir;

    // --- DreamLock tests ---

    #[test]
    fn no_file_means_no_prior_consolidation() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        assert!(lock.last_consolidated_at().unwrap().is_none());
    }

    #[test]
    fn acquire_on_empty_dir_writes_pid() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        assert!(
            lock.try_acquire(300).unwrap().is_some(),
            "should acquire via create_new on an empty dir"
        );

        let content = fs::read_to_string(&lock.path).unwrap();
        assert_eq!(
            token_pid(&content),
            Some(std::process::id()),
            "the token records our pid as its first field"
        );
        assert!(
            lock.last_consolidated_at().unwrap().is_none(),
            "acquire claims the mutex but records no consolidation"
        );
    }

    #[test]
    fn drop_without_commit_releases_mutex_and_writes_no_marker() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        {
            let _guard = lock.acquire(300).unwrap().expect("acquire");
            assert!(lock.path.exists(), "mutex is held while the guard is alive");
        }

        assert!(
            !lock.path.exists(),
            "dropping an uncommitted guard releases the mutex"
        );
        assert!(
            lock.last_consolidated_at().unwrap().is_none(),
            "an uncommitted guard leaves the gate open (no marker)"
        );
    }

    #[test]
    fn commit_writes_marker_then_drop_releases_mutex() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        let guard = lock.acquire(300).unwrap().expect("acquire");
        assert!(lock.path.exists(), "mutex is held while the guard is alive");

        assert!(guard.commit(), "commit writes the marker and returns true");
        assert!(
            lock.last_consolidated_at().unwrap().is_some(),
            "commit records the consolidation"
        );
        assert!(!lock.path.exists(), "commit releases the mutex");
    }

    #[test]
    fn dead_pid_is_reclaimed() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        fs::write(&lock.path, "4000000000").unwrap();
        assert!(
            lock.try_acquire(300).unwrap().is_some(),
            "dead PID should be reclaimable"
        );

        let content = fs::read_to_string(&lock.path).unwrap();
        assert_eq!(token_pid(&content), Some(std::process::id()));
    }

    #[test]
    fn live_pid_blocks_acquisition() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        assert!(lock.try_acquire(300).unwrap().is_some(), "first acquire");
        assert!(
            lock.try_acquire(300).unwrap().is_none(),
            "second acquire should be blocked by live PID"
        );
    }

    #[test]
    fn intra_process_acquire_is_exclusive() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        // Two same-process acquirers share a PID, so exclusion must come from the atomic create.
        let first = lock.acquire(300).unwrap().expect("first acquire wins");
        assert!(
            lock.acquire(300).unwrap().is_none(),
            "a second acquirer in the same process must not also win"
        );

        drop(first);
        assert!(
            lock.acquire(300).unwrap().is_some(),
            "after the holder drops, a fresh acquire succeeds"
        );
    }

    #[test]
    fn commit_records_consolidation_but_acquire_alone_does_not() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        {
            let _guard = lock.acquire(300).unwrap().expect("acquire");
            assert!(
                lock.last_consolidated_at().unwrap().is_none(),
                "holding the lock is not a consolidation"
            );
        }
        // Dropped without commit (crash/cancel): the gate stays open for a retry.
        assert!(lock.last_consolidated_at().unwrap().is_none());

        assert!(
            lock.acquire(300).unwrap().expect("re-acquire").commit(),
            "commit writes the marker and returns true"
        );
        assert!(
            lock.last_consolidated_at().unwrap().is_some(),
            "commit records the consolidation"
        );
        assert!(
            !lock.path.exists(),
            "commit releases the mutex so /dream is not blocked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_reports_false_when_marker_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());
        let guard = lock.acquire(300).unwrap().expect("acquire");

        // Make the workspace dir read-only so creating the marker file fails.
        let original = fs::metadata(dir.path()).unwrap().permissions();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();

        // A root/CAP_DAC_OVERRIDE environment ignores the mode and would let the write succeed,
        // so skip rather than assert a failure this platform cannot produce.
        let writable_anyway = fs::write(dir.path().join(".perm-probe"), b"x").is_ok();
        let _ = fs::remove_file(dir.path().join(".perm-probe"));
        if writable_anyway {
            fs::set_permissions(dir.path(), original).unwrap();
            return;
        }

        assert!(
            !guard.commit(),
            "commit must report false when the marker write fails"
        );

        fs::set_permissions(dir.path(), original).unwrap();
        assert!(
            lock.last_consolidated_at().unwrap().is_none(),
            "a failed marker write must not advance the gate"
        );
    }

    #[test]
    fn last_consolidated_falls_back_to_legacy_lock() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        let legacy = dir.path().join(".dream-lock");
        let t = SystemTime::now() - Duration::from_secs(3600);
        fs::write(&legacy, "").unwrap();
        filetime::set_file_mtime(&legacy, FileTime::from_system_time(t)).unwrap();

        let read = lock
            .last_consolidated_at()
            .unwrap()
            .expect("legacy fallback");
        let drift = read
            .duration_since(t)
            .or_else(|_| t.duration_since(read))
            .unwrap();
        assert!(
            drift.as_secs() < 2,
            "reads the legacy lock mtime when no marker"
        );
    }

    #[test]
    fn stale_age_allows_reclaim_even_if_alive() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        fs::write(&lock.path, std::process::id().to_string()).unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        filetime::set_file_mtime(&lock.path, FileTime::from_system_time(old)).unwrap();

        // Age 600 exceeds stale_secs 300, so the live PID does not block
        assert!(
            lock.try_acquire(300).unwrap().is_some(),
            "stale lock should be reclaimable"
        );
    }

    #[test]
    fn record_consolidation_creates_file() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        lock.record_consolidation().unwrap();

        let age = SystemTime::now()
            .duration_since(lock.last_consolidated_at().unwrap().unwrap())
            .unwrap_or_default();
        assert!(age.as_secs() < 5);
    }

    #[test]
    fn record_consolidation_updates_mtime() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        fs::write(&lock.path, "12345").unwrap();
        let old = SystemTime::now() - Duration::from_secs(7200);
        filetime::set_file_mtime(&lock.path, FileTime::from_system_time(old)).unwrap();

        lock.record_consolidation().unwrap();

        let age = SystemTime::now()
            .duration_since(lock.last_consolidated_at().unwrap().unwrap())
            .unwrap_or_default();
        assert!(age.as_secs() < 5, "mtime should be ~now");
    }

    #[test]
    fn full_lifecycle_acquire_consolidate_blocks_reacquire() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        assert!(lock.try_acquire(300).unwrap().is_some(), "first acquire");

        lock.record_consolidation().unwrap();

        assert!(
            lock.try_acquire(300).unwrap().is_none(),
            "the still-held live-PID mutex blocks re-acquire"
        );
    }

    #[test]
    fn fresh_unparseable_body_is_not_stolen() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        // A holder that created the file but has not yet written its token must not be reclaimed,
        // or two acquirers would run at once. Only the stale-age check may take it.
        fs::write(&lock.path, "").unwrap();
        assert!(
            lock.try_acquire(300).unwrap().is_none(),
            "a fresh empty body is a mid-init holder, not stealable"
        );

        fs::write(&lock.path, "not-a-pid").unwrap();
        assert!(
            lock.try_acquire(300).unwrap().is_none(),
            "a fresh garbage body is not stealable"
        );
    }

    #[test]
    fn stale_unparseable_body_is_reclaimable() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        fs::write(&lock.path, "not-a-pid").unwrap();
        let old = SystemTime::now() - Duration::from_secs(600);
        filetime::set_file_mtime(&lock.path, FileTime::from_system_time(old)).unwrap();
        assert!(
            lock.try_acquire(300).unwrap().is_some(),
            "a stale corrupt body is reclaimed by the age check"
        );
    }

    #[test]
    fn drop_removes_only_our_own_lock() {
        let dir = TempDir::new().unwrap();
        let lock = DreamLock::new(dir.path());

        let guard = lock.acquire(300).unwrap().expect("acquire");
        // A stale reclaim hands the path to another holder mid-hold; our Drop must not delete it.
        fs::write(&lock.path, "999999 42").unwrap();
        drop(guard);
        assert!(
            lock.path.exists(),
            "dropping our guard must leave another holder's lock intact"
        );
    }

    // --- sessions_since tests ---

    fn write_session(dir: &Path, name: &str, age_secs: u64) {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        fs::write(&path, "test").unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_secs);
        filetime::set_file_mtime(&path, FileTime::from_system_time(t)).unwrap();
    }

    #[test]
    fn filters_by_mtime() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        let cutoff = SystemTime::now() - Duration::from_secs(3600);

        write_session(&sessions, "2026-01-01-proj-aaa11111", 1800); // 30min ago, after cutoff
        write_session(&sessions, "2025-12-31-proj-bbb22222", 7200); // 2h ago, before cutoff

        let result = sessions_since(&sessions, cutoff, None).unwrap();
        assert_eq!(result, vec!["2026-01-01-proj-aaa11111"]);
    }

    #[test]
    fn mtime_at_exact_cutoff_is_excluded() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        let cutoff = SystemTime::now() - Duration::from_secs(3600);

        // Set mtime to the exact cutoff value (not strictly after)
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("2026-01-01-proj-exact000.md");
        fs::write(&path, "test").unwrap();
        filetime::set_file_mtime(&path, FileTime::from_system_time(cutoff)).unwrap();

        let result = sessions_since(&sessions, cutoff, None).unwrap();
        assert!(
            result.is_empty(),
            "mtime == cutoff should be excluded (strict >)"
        );
    }

    #[test]
    fn excludes_current_session() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        let cutoff = SystemTime::now() - Duration::from_secs(86400);

        write_session(&sessions, "2026-01-01-proj-aaa11111", 100);
        write_session(&sessions, "2026-01-01-proj-bbb22222", 100);

        let result = sessions_since(&sessions, cutoff, Some("bbb22222")).unwrap();
        assert_eq!(result, vec!["2026-01-01-proj-aaa11111"]);
    }

    #[test]
    fn empty_dir_returns_empty_vec() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        let result = sessions_since(&sessions, SystemTime::UNIX_EPOCH, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn nonexistent_dir_returns_empty_vec() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("nonexistent");

        let result = sessions_since(&sessions, SystemTime::UNIX_EPOCH, None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn ignores_non_md_files() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");
        fs::create_dir_all(&sessions).unwrap();

        write_session(&sessions, "2026-01-01-proj-aaa11111", 0);
        fs::write(sessions.join("notes.txt"), "not a session").unwrap();
        fs::write(sessions.join("data.json"), "{}").unwrap();

        let result = sessions_since(&sessions, SystemTime::UNIX_EPOCH, None).unwrap();
        assert_eq!(result, vec!["2026-01-01-proj-aaa11111"]);
    }

    #[test]
    fn returns_sorted_stems() {
        let dir = TempDir::new().unwrap();
        let sessions = dir.path().join("sessions");

        write_session(&sessions, "zzz-session", 0);
        write_session(&sessions, "aaa-session", 0);
        write_session(&sessions, "mmm-session", 0);

        let result = sessions_since(&sessions, SystemTime::UNIX_EPOCH, None).unwrap();
        assert_eq!(result, vec!["aaa-session", "mmm-session", "zzz-session"]);
    }
}
