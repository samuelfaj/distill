//! Build a `memory.tar.gz` archive containing session logs and MEMORY.md files.
//!
//! The archive is uploaded to GCS at session finalize time.
//! The reconstruct pipeline injects these files into the Docker image so a replayed session sees the same memory.

use anyhow::{Context, Result};

use super::MemoryStorage;

/// Per-file cap. Memory notes are markdown of at most tens of KB; anything
/// larger is not legitimate memory content and a growing file (e.g. a
/// planted `/dev/zero` symlink) must not balloon the process.
const MAX_MEMORY_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Open `path` without following a final-component symlink, without blocking on a FIFO, and only if it is a regular file. The memory dir is writable to sandboxed agents, so a planted symlink must not smuggle sandbox-denied files (`~/.ssh/id_rsa`, …) into the uploaded archive, and a special file must not hang or grow the non-abortable build.
fn open_regular_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// Snapshot `path`'s bytes and append them as `name`: the tar header size must consolidation, `/flush`) resizes the live file mid-build. Skips (never fails the archive) on: vanished files, symlinks, non-regular files, and files over [`MAX_MEMORY_FILE_BYTES`].
fn append_file_snapshot<W: std::io::Write>(
    ar: &mut tar::Builder<W>,
    path: &std::path::Path,
    name: &str,
) -> Result<()> {
    use std::io::Read as _;

    let file = match open_regular_nofollow(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "skipping unreadable memory file");
            return Ok(());
        }
    };
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "skipping unstatable memory file");
            return Ok(());
        }
    };
    if meta.len() > MAX_MEMORY_FILE_BYTES {
        tracing::warn!(path = %path.display(), bytes = meta.len(), "skipping oversized memory file");
        return Ok(());
    }
    // take() guards a file growing past the cap between stat and read.
    let mut data = Vec::new();
    if let Err(e) = file.take(MAX_MEMORY_FILE_BYTES + 1).read_to_end(&mut data) {
        tracing::warn!(path = %path.display(), error = %e, "skipping unreadable memory file");
        return Ok(());
    }
    if data.len() as u64 > MAX_MEMORY_FILE_BYTES {
        tracing::warn!(path = %path.display(), bytes = data.len(), "skipping oversized memory file");
        return Ok(());
    }
    let mtime = meta
        .modified()
        .unwrap_or_else(|_| std::time::SystemTime::now())
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(mtime);
    ar.append_data(&mut header, name, data.as_slice())
        .with_context(|| format!("archive {name}"))
}

/// Build a `memory.tar.gz` archive with session logs and MEMORY.md files.
pub fn build_memory_archive(storage: &MemoryStorage) -> Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let buf = Vec::new();
    let enc = GzEncoder::new(buf, Compression::default());
    let mut ar = tar::Builder::new(enc);

    // Session logs
    let sessions_dir = storage.workspace_dir().join("sessions");
    if sessions_dir.is_dir() {
        for entry in std::fs::read_dir(&sessions_dir)
            .context("read sessions dir")?
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = format!("workspace/sessions/{}", entry.file_name().to_string_lossy());
                append_file_snapshot(&mut ar, &path, &name)?;
            }
        }
    }

    // MEMORY.md files
    let global_mem = storage.global_memory_file();
    if global_mem.is_file() {
        append_file_snapshot(&mut ar, &global_mem, "global/MEMORY.md")?;
    }

    let workspace_mem = storage.workspace_memory_file();
    if workspace_mem.is_file() {
        let ws_dir_name = storage
            .workspace_dir()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let archive_path = format!("{ws_dir_name}/MEMORY.md");
        append_file_snapshot(&mut ar, &workspace_mem, &archive_path)?;
    }

    let enc = ar.into_inner().context("finalize tar")?;
    enc.finish().context("compress tar.gz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    #[test]
    fn test_build_archive_includes_memory_md() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        std::fs::write(storage.global_memory_file(), "# Global Memory").unwrap();
        std::fs::write(storage.workspace_memory_file(), "# Workspace Memory").unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        let entries = tar_entry_names(&archive);
        assert!(entries.contains(&"global/MEMORY.md".to_string()));
        assert!(entries.contains(&"test_ws/MEMORY.md".to_string()));
    }

    fn tar_entry_names(gz_bytes: &[u8]) -> Vec<String> {
        use flate2::read::GzDecoder;
        let decoder = GzDecoder::new(gz_bytes);
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().display().to_string())
            .collect()
    }

    /// A planted symlink must not smuggle its target into the archive: the
    /// memory dir is agent-writable while the target may be sandbox-denied.
    #[cfg(unix)]
    #[test]
    fn symlink_to_regular_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();
        let secret = tmp.path().join("id_rsa");
        std::fs::write(&secret, "PRIVATE KEY").unwrap();
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&secret, sessions.join("exfil.md")).unwrap();
        std::fs::write(sessions.join("real.md"), "# notes").unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        let entries = tar_entry_names(&archive);
        assert!(
            entries.contains(&"workspace/sessions/real.md".to_string()),
            "regular file must still be packed: {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.contains("exfil")),
            "symlink must be skipped, not packed under its name: {entries:?}"
        );
    }

    /// A FIFO planted as a session file must not block the build (plain
    /// open(2) on a FIFO waits for a writer) and must not be packed.
    #[cfg(unix)]
    #[test]
    fn fifo_is_skipped_without_hanging() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let fifo = sessions.join("pipe.md");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) }, 0);

        let archive = build_memory_archive(&storage).unwrap();
        assert!(
            !tar_entry_names(&archive).iter().any(|e| e.contains("pipe")),
            "FIFO must be skipped"
        );
    }

    /// A file over the per-file cap is skipped instead of ballooning the
    /// archive (and the process) — the cap also bounds `/dev/zero`-style
    /// endlessly-readable plants.
    #[test]
    fn oversized_file_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let big = std::fs::File::create(sessions.join("big.md")).unwrap();
        big.set_len(MAX_MEMORY_FILE_BYTES + 1).unwrap();
        std::fs::write(sessions.join("small.md"), "# ok").unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        let entries = tar_entry_names(&archive);
        assert!(
            entries.contains(&"workspace/sessions/small.md".to_string()),
            "small file must still be packed: {entries:?}"
        );
        assert!(
            !entries.iter().any(|e| e.contains("big")),
            "oversized file must be skipped: {entries:?}"
        );
    }

    /// A session file that vanishes between `read_dir` and the byte snapshot
    /// (emulated by a dangling symlink) must not fail the whole build.
    #[cfg(unix)]
    #[test]
    fn test_build_archive_skips_vanished_session_file() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("a.md"), "# kept").unwrap();
        std::os::unix::fs::symlink(sessions.join("missing"), sessions.join("b.md")).unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        let entries = tar_entry_names(&archive);
        assert!(entries.contains(&"workspace/sessions/a.md".to_string()));
        assert!(!entries.iter().any(|e| e.ends_with("b.md")));
    }

    /// A same-process writer (dream consolidation, `/flush`) appending to a
    /// session log mid-build must not desync the tar: every produced archive
    /// stays parseable with the trailing MEMORY.md entries intact.
    #[test]
    fn test_build_archive_consistent_under_concurrent_append() {
        use std::io::Read;

        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(storage.global_dir()).unwrap();

        // Incompressible payload so the gzip copy leaves a wide append window.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut chunk = vec![0u8; 64 << 10];
        for b in chunk.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = seed as u8;
        }
        let growing = sessions.join("2026-01-01-growing.md");
        std::fs::write(&growing, chunk.repeat(128)).unwrap(); // 8 MiB
        // Appended after the sessions entries: desync garbles or drops it.
        std::fs::write(storage.global_memory_file(), "# global sentinel").unwrap();

        // Bounded so the file cannot outgrow the reader indefinitely.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let done = done.clone();
            let growing = growing.clone();
            let chunk = chunk.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&growing)
                    .unwrap();
                for _ in 0..150 {
                    f.write_all(&chunk).unwrap();
                    f.sync_data().ok();
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                done.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        };

        let mut failures = Vec::new();
        let mut i = 0;
        while !done.load(std::sync::atomic::Ordering::Relaxed) && i < 20 {
            match build_memory_archive(&storage) {
                Err(e) => failures.push(format!("iteration {i}: build failed: {e:#}")),
                Ok(archive) => {
                    use flate2::read::GzDecoder;
                    let mut ar = tar::Archive::new(GzDecoder::new(archive.as_slice()));
                    let mut sentinel = None;
                    for entry in ar.entries().unwrap() {
                        let Ok(mut entry) = entry else {
                            failures.push(format!("iteration {i}: unparseable tar entry"));
                            break;
                        };
                        let path = entry.path().unwrap().display().to_string();
                        if path == "global/MEMORY.md" {
                            let mut s = String::new();
                            entry.read_to_string(&mut s).unwrap();
                            sentinel = Some(s);
                        }
                    }
                    if sentinel.as_deref() != Some("# global sentinel") {
                        failures.push(format!(
                            "iteration {i}: global/MEMORY.md desynced: {sentinel:?}"
                        ));
                    }
                }
            }
            i += 1;
        }
        writer.join().unwrap();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
