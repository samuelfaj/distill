//! Git index operations used during worktree creation.

use std::fs::Metadata;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::copy::cow::clone_file;
use crate::git::discovery::find_worktree_git_dir;

/// Copy the index, following `gitdir:` pointers on both sides. Reflink when
/// possible. `true` if copied, `false` if the source has no index.
pub(crate) fn copy_git_index(source: &Path, dest_worktree: &Path) -> Result<bool> {
    let source_git_dir = find_worktree_git_dir(source)?;
    let dest_git_dir = find_worktree_git_dir(dest_worktree)?;

    let source_index = source_git_dir.join("index");
    let dest_index = dest_git_dir.join("index");

    if source_index.exists() {
        // reflink_or_copy cannot overwrite — remove destination first
        if dest_index.exists() {
            let _ = std::fs::remove_file(&dest_index);
        }

        clone_file(&source_index, &dest_index).with_context(|| {
            format!(
                "failed to copy index from {} to {}",
                source_index.display(),
                dest_index.display()
            )
        })?;

        // Split index: `sharedindex.<hash>` must sit beside the index. Linked
        // worktrees keep it in the common git dir, so symlink it into dest or
        // gix cannot resolve it.
        link_shared_indexes(&source_git_dir, &dest_git_dir)?;

        tracing::debug!(
            source = %source_index.display(),
            dest = %dest_index.display(),
            "copied git index (reflink)"
        );
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Symlink `sharedindex.*` beside the dest index. gix looks in the same
/// directory; linked worktrees store them in the common dir or the worktree
/// git dir. No-op if split index is unused.
fn link_shared_indexes(source_git_dir: &Path, dest_git_dir: &Path) -> Result<()> {
    // Resolve the common dir: for a linked worktree the `commondir` file
    // points to the shared `.git/`. For a regular repo the git dir IS the
    // common dir.
    let source_common_dir = resolve_common_dir(source_git_dir);

    // Always scan the common dir. A linked worktree may also have created
    // shared index files in its own git dir.
    let mut dirs_to_scan: Vec<&Path> = vec![&source_common_dir];
    if source_git_dir != source_common_dir {
        dirs_to_scan.push(source_git_dir);
    }

    let mut linked = 0u32;
    for scan_dir in &dirs_to_scan {
        let entries = match std::fs::read_dir(scan_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("sharedindex.") {
                continue;
            }

            let src = entry.path();
            let dst = dest_git_dir.join(&name);

            // Skip if already present (e.g. dest IS the common dir, or
            // already linked from a previous scan_dir iteration).
            if dst.exists() {
                continue;
            }

            // Symlink is ideal: instant, zero-copy, shared index is
            // read-only content-addressed data.
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&src, &dst).with_context(|| {
                    format!(
                        "failed to symlink sharedindex {} -> {}",
                        dst.display(),
                        src.display()
                    )
                })?;
            }
            #[cfg(not(unix))]
            {
                // Fallback: reflink/copy on Windows.
                clone_file(&src, &dst).with_context(|| {
                    format!(
                        "failed to copy sharedindex {} -> {}",
                        src.display(),
                        dst.display()
                    )
                })?;
            }

            linked += 1;
        }
    }

    if linked > 0 {
        tracing::debug!(
            source_common_dir = %source_common_dir.display(),
            dest_git_dir = %dest_git_dir.display(),
            linked,
            "linked sharedindex files for split-index support"
        );
    }

    Ok(())
}

/// Common git dir. Linked worktrees store a relative path in `commondir`;
/// a regular repo's git dir is already the common dir.
fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let commondir_file = git_dir.join("commondir");
    if let Ok(content) = std::fs::read_to_string(&commondir_file) {
        let relative = content.trim();
        let resolved = git_dir.join(relative);
        // Canonicalize to clean up `../..` etc.
        dunce::canonicalize(&resolved).unwrap_or(resolved)
    } else {
        git_dir.to_path_buf()
    }
}

/// Refresh the stat cache for copied files so a full `git update-index
/// --refresh` is unnecessary.
pub(crate) fn update_index_stats(
    worktree_path: &Path,
    file_metadata: &[(PathBuf, Metadata)],
) -> Result<()> {
    if file_metadata.is_empty() {
        return Ok(());
    }

    let start = std::time::Instant::now();
    let git_dir = find_worktree_git_dir(worktree_path)?;
    let index_path = git_dir.join("index");

    // If index doesn't exist yet, there's nothing to update
    if !index_path.exists() {
        tracing::debug!(
            path = %worktree_path.display(),
            "index file doesn't exist yet, skipping update"
        );
        return Ok(());
    }

    // Guard against empty index files — gix-index panics when the file
    // is 0 bytes because it tries to slice the trailing hash from an
    // empty mmap (integer underflow in the slice range).
    if index_path.metadata().map_or(true, |m| m.len() == 0) {
        tracing::debug!(
            path = %worktree_path.display(),
            "index file is empty, skipping update"
        );
        return Ok(());
    }

    // Open the index file directly for modification
    let mut index = gix::index::File::at(
        &index_path,
        gix::hash::Kind::Sha1,
        false,
        Default::default(),
    )
    .context("failed to open git index")?;

    // Update stat info for each file that was copied
    let mut updated_count = 0;
    for entry in file_metadata.iter() {
        let (path, metadata) = (&entry.0, &entry.1);

        // Convert path to BStr for gix
        let path_str = path.to_string_lossy();
        let path_bytes: &gix::bstr::BStr = path_str.as_bytes().into();

        // Find the entry in the index
        if let Ok(entry_index) = index.entry_index_by_path(path_bytes)
            && let Some(entry) = index.entries_mut().get_mut(entry_index)
        {
            updated_count += 1;

            // Update stat fields from metadata
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;

                // mtime (modification time)
                entry.stat.mtime.secs = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);

                entry.stat.mtime.nsecs = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);

                // ctime (change time) - MUST be the actual ctime, not mtime
                entry.stat.ctime.secs = metadata.ctime() as u32;
                entry.stat.ctime.nsecs = metadata.ctime_nsec() as u32;

                // Other stat fields
                entry.stat.size = metadata.len() as u32;
                entry.stat.dev = metadata.dev() as u32;
                entry.stat.ino = metadata.ino() as u32;
                entry.stat.uid = metadata.uid();
                entry.stat.gid = metadata.gid();
                // Note: mode is on the entry itself, not stat
            }

            #[cfg(not(unix))]
            {
                // On non-Unix systems, use mtime for both
                entry.stat.mtime.secs = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);

                entry.stat.mtime.nsecs = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.subsec_nanos())
                    .unwrap_or(0);

                entry.stat.ctime.secs = entry.stat.mtime.secs;
                entry.stat.ctime.nsecs = entry.stat.mtime.nsecs;
                entry.stat.size = metadata.len() as u32;
            }
        }
    }

    // Count how many entries were actually updated
    let num_updated = updated_count;

    // Write the updated index
    index.write(Default::default())?;

    tracing::debug!(
        path = %worktree_path.display(),
        files_updated = num_updated,
        elapsed = ?start.elapsed(),
        "updated index stat cache"
    );

    Ok(())
}
