//! Capped tar.gz archive of a session directory for a user-consented `/feedback` trace upload.

pub struct ArchiveCaps {
    /// Total packed bytes; packing stops (truncating the archive) once hit.
    pub archive_bytes: u64,
    /// Per-file bytes; larger files are skipped.
    pub file_bytes: u64,
}

pub const FEEDBACK_ARCHIVE_CAPS: ArchiveCaps = ArchiveCaps {
    archive_bytes: 50 * 1024 * 1024,
    file_bytes: 10 * 1024 * 1024,
};

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("pack session file: {0}")]
    Pack(#[from] std::io::Error),
    #[error("finalize archive: {0}")]
    Finalize(#[source] std::io::Error),
    #[error("session archive would be empty")]
    Empty,
}

pub fn build_session_archive(
    session_dir: &std::path::Path,
    session_id: &str,
) -> Result<Vec<u8>, ArchiveError> {
    build_session_archive_with_caps(session_dir, session_id, &FEEDBACK_ARCHIVE_CAPS)
}

fn build_session_archive_with_caps(
    session_dir: &std::path::Path,
    session_id: &str,
    caps: &ArchiveCaps,
) -> Result<Vec<u8>, ArchiveError> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut archive_data = Vec::new();
    {
        let encoder = GzEncoder::new(&mut archive_data, Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let packed = add_dir_to_tar(&mut archive, session_dir, session_id, caps)?;
        // Skips (oversized files, races with live writers) can leave nothing packed; an empty gzip helps nobody and must not report `uploaded`
        if packed == 0 {
            return Err(ArchiveError::Empty);
        }
        archive
            .into_inner()
            .and_then(|encoder| encoder.finish())
            .map_err(ArchiveError::Finalize)?;
    }
    Ok(archive_data)
}

/// Pack `dir` into `archive`, returning how many files were packed.
fn add_dir_to_tar<W: std::io::Write>(
    archive: &mut tar::Builder<W>,
    dir: &std::path::Path,
    prefix: &str,
    caps: &ArchiveCaps,
) -> Result<usize, ArchiveError> {
    use std::path::Component;

    let mut total = 0u64;
    let mut packed = 0usize;
    let artifacts = crate::FeedbackDraftArtifactSet::for_session(dir);
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        // Prunes every draft artifact name, images dir included, before any metadata, open, or size step
        .filter_entry(|entry| !crate::is_feedback_draft_artifact_name(entry.file_name()))
        .filter_map(|e| e.ok())
    {
        if entry.path_is_symlink() || entry.file_type().is_dir() || !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(dir) else {
            continue;
        };
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            continue;
        }
        let room = caps.archive_bytes.saturating_sub(total);
        let limit = caps.file_bytes.min(room);
        if limit == 0 {
            // The cap truncates the archive; what is already packed is still useful for debugging, so stop instead of failing the upload
            break;
        }
        // Skip-on-error: the session dir has live writers, so entries can disappear while packing.
        let Ok(Some(mut file)) = artifacts.open_non_artifact(path) else {
            continue;
        };
        let mut buf = Vec::new();
        let n = std::io::copy(&mut std::io::Read::take(&mut file, limit + 1), &mut buf)?;
        if n > limit {
            continue;
        }
        total = total.saturating_add(n);
        let name = format!("{prefix}/{}", rel.to_string_lossy());
        let mut header = tar::Header::new_gnu();
        header.set_size(n);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append_data(&mut header, name, buf.as_slice())?;
        packed += 1;
    }
    Ok(packed)
}

#[cfg(test)]
#[path = "feedback_archive_tests.rs"]
mod tests;
