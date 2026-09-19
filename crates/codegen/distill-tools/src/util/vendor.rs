// Modified for Distill by Samuel Fajreldines, 2026.
use std::io::Write;
use std::path::{Path, PathBuf};

/// A failure while installing a bundled binary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstallError {
    /// The bundled bytes failed verification; the binary must not be run.
    #[error("{0}")]
    Integrity(String),
    /// Installation could not complete for an operational reason; a `PATH`
    /// binary may be used instead.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Installs a bundled binary on first use and returns its path. Returns `None` when installation is
/// skipped or fails for an operational reason, so the caller can fall back to a `PATH` binary.
/// Returns an error only when the embedded bytes fail their integrity check.
#[cfg(any(bundle_rg, bundle_fd, bundle_bfs, bundle_ugrep))]
pub(crate) fn resolve(
    versioned_name: &str,
    compressed: &[u8],
    expected_sha256: &str,
) -> Result<Option<PathBuf>, InstallError> {
    let dir = crate::util::distill_home().join("vendor");
    match install(&dir, versioned_name, compressed, expected_sha256) {
        Ok(path) => Ok(Some(path)),
        Err(InstallError::Io(err)) => {
            tracing::debug!("bundled {versioned_name} unavailable, falling back to PATH: {err}");
            Ok(None)
        }
        Err(err) => Err(err),
    }
}

fn install(
    dir: &Path,
    versioned_name: &str,
    compressed: &[u8],
    expected_sha256: &str,
) -> Result<PathBuf, InstallError> {
    let dest = dir.join(versioned_name);
    if is_verified(&dest, expected_sha256) {
        return Ok(dest);
    }

    let decoded = zstd::decode_all(compressed).map_err(|e| {
        InstallError::Integrity(format!(
            "bundled {versioned_name} failed to decompress: {e}"
        ))
    })?;
    if distill_file_utils::sha256_hex(&decoded) != expected_sha256 {
        return Err(InstallError::Integrity(format!(
            "bundled {versioned_name} does not match its pinned SHA-256"
        )));
    }

    std::fs::create_dir_all(dir)?;
    // Stage in the destination directory and rename atomically, so an
    // interrupted or concurrent first use never publishes a partial binary.
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(&decoded)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    staged.persist(&dest).map_err(|e| e.error)?;
    Ok(dest)
}

fn is_verified(path: &Path, expected_sha256: &str) -> bool {
    // Reject a planted symlink instead of following it during verification.
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        && distill_file_utils::sha256_hex_from_file(path, None)
            .is_ok_and(|actual| actual == expected_sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zst(bytes: &[u8]) -> Vec<u8> {
        zstd::encode_all(bytes, 0).expect("zstd encode")
    }

    #[test]
    fn installs_then_reuses_verified_binary() {
        let dir = tempfile::tempdir().unwrap();
        let body = b"#!/bin/sh\necho hi\n";
        let sha = distill_file_utils::sha256_hex(body);

        let path = install(dir.path(), "tool-1", &zst(body), &sha).expect("install");
        assert_eq!(std::fs::read(&path).unwrap(), body);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }

        assert_eq!(
            install(dir.path(), "tool-1", &zst(body), &sha).expect("reuse"),
            path
        );
    }

    #[test]
    fn corrupt_archive_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        match install(dir.path(), "tool-2", b"not-zstd", "00") {
            Err(InstallError::Integrity(msg)) => assert!(msg.contains("tool-2")),
            other => panic!("expected an integrity error, got {other:?}"),
        }
        assert!(!dir.path().join("tool-2").exists());
    }

    #[test]
    fn sha_mismatch_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        match install(dir.path(), "tool-3", &zst(b"real"), &"0".repeat(64)) {
            Err(InstallError::Integrity(msg)) => assert!(msg.contains("tool-3")),
            other => panic!("expected an integrity error, got {other:?}"),
        }
        assert!(!dir.path().join("tool-3").exists());
    }

    #[test]
    fn stale_cache_reextracts_from_trusted_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool-4"), b"stale or tampered").unwrap();
        let body = b"trusted";
        let sha = distill_file_utils::sha256_hex(body);

        let path = install(dir.path(), "tool-4", &zst(body), &sha).expect("self-heal");
        assert_eq!(std::fs::read(path).unwrap(), body);
    }
}
