//! Secure enumeration of files visible through memory v2 browsing.

use std::path::{Path, PathBuf};

use crate::v2::{MAX_DIRECTORY_ENTRIES, MAX_DISCOVERED_FILES, excluded_manifest_paths};

pub(super) fn list_memory_files(
    global_dir: &Path,
    workspace_dir: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    list_memory_files_with_caps(
        global_dir,
        workspace_dir,
        MAX_DIRECTORY_ENTRIES,
        MAX_DISCOVERED_FILES,
    )
}

/// Enumerate browsable files with explicit caps so the bounds are testable.
///
/// A directory holding more than `max_entries` entries is an error rather than
/// something to walk to exhaustion, and at most `max_files` paths are returned.
/// Files the state ledger hides (record-only/shadow observations, tombstones
/// awaiting unlink) are omitted so browse matches the manifest.
fn list_memory_files_with_caps(
    global_dir: &Path,
    workspace_dir: &Path,
    max_entries: usize,
    max_files: usize,
) -> std::io::Result<Vec<PathBuf>> {
    let storage_root = global_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "v2 global directory has no storage root",
        )
    })?;
    let canonical_root = match dunce::canonicalize(storage_root) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut files = Vec::new();
    for scope_dir in [global_dir, workspace_dir] {
        reject_symlink(scope_dir)?;
        let canonical_scope = match dunce::canonicalize(scope_dir) {
            Ok(scope) => scope,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        ensure_descendant(&canonical_scope, &canonical_root)?;
        let excluded = excluded_manifest_paths(scope_dir).map_err(std::io::Error::other)?;

        let manifest = scope_dir.join("MEMORY.md");
        if is_safe_markdown_file(&manifest, &canonical_scope)? && files.len() < max_files {
            files.push(manifest);
        }

        for relative in ["topics", "observations/_inbox"] {
            let directory = scope_dir.join(relative);
            reject_symlink_components(scope_dir, &directory)?;
            let canonical_directory = match dunce::canonicalize(&directory) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            ensure_descendant(&canonical_directory, &canonical_scope)?;

            for (entry_index, entry) in std::fs::read_dir(&directory)?.enumerate() {
                if entry_index >= max_entries {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "v2 memory directory {} exceeds the {max_entries}-entry safety limit",
                            directory.display()
                        ),
                    ));
                }
                let path = entry?.path();
                if path.extension().and_then(|value| value.to_str()) != Some("md")
                    || !is_safe_markdown_file(&path, &canonical_scope)?
                {
                    continue;
                }
                let is_excluded = path
                    .strip_prefix(scope_dir)
                    .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                    .is_ok_and(|relative| excluded.contains(&relative));
                if is_excluded {
                    continue;
                }
                if files.len() >= max_files {
                    tracing::warn!(
                        directory = %directory.display(),
                        limit = max_files,
                        "v2 memory listing reached its file cap; omitting remaining files"
                    );
                    break;
                }
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn reject_symlink_components(root: &Path, target: &Path) -> std::io::Result<()> {
    let relative = target.strip_prefix(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "v2 memory path {} escapes {}",
                target.display(),
                root.display()
            ),
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        reject_symlink(&current)?;
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "symbolic links are not allowed in memory v2: {}",
                path.display()
            ),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_safe_markdown_file(path: &Path, canonical_scope: &Path) -> std::io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "symbolic links are not allowed in memory v2: {}",
                path.display()
            ),
        ));
    }
    if !metadata.is_file() {
        return Ok(false);
    }
    ensure_descendant(&dunce::canonicalize(path)?, canonical_scope)?;
    Ok(true)
}

fn ensure_descendant(path: &Path, root: &Path) -> std::io::Result<()> {
    if path.starts_with(root) {
        return Ok(());
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!(
            "v2 memory path {} escapes {}",
            path.display(),
            root.display()
        ),
    ))
}

#[cfg(test)]
#[path = "storage_v2_tests.rs"]
mod tests;
