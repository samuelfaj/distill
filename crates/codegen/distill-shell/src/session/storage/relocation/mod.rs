// Modified for Distill by Samuel Fajreldines, 2026.
//! Point-in-time candidate view of local session storage.
//!
//! Resolves session directories under the sessions root (`<encoded-cwd>/<session-id>/` buckets).
//! The journaled relocation transaction this module was named for never ran in production and was deleted.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum RelocationError {
    #[error("invalid relocation {field}: {value:?}")]
    InvalidComponent { field: &'static str, value: String },
    #[error("{operation} {path}: {source}", path = path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("decode {path}: {source}", path = path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub(crate) type Result<T> = std::result::Result<T, RelocationError>;

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> RelocationError {
    RelocationError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

fn validate_component(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(RelocationError::InvalidComponent {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

type SessionCandidates = HashMap<String, Vec<PathBuf>>;

pub(crate) struct RelocationView {
    sessions_root: PathBuf,
    all_candidates: SessionCandidates,
    persisted_candidates: SessionCandidates,
}

impl RelocationView {
    pub(crate) fn load(distill_home: &Path) -> Result<Self> {
        Self::load_for_sessions_root(&distill_home.join("sessions"))
    }

    pub(crate) fn load_for_sessions_root(sessions_root: &Path) -> Result<Self> {
        let (all_candidates, persisted_candidates) = load_candidates(sessions_root)?;
        Ok(Self {
            sessions_root: sessions_root.into(),
            all_candidates,
            persisted_candidates,
        })
    }

    /// Session directories with a persisted `summary.json`, optionally restricted to `cwd`.
    /// A session id present under multiple cwd buckets is ambiguous and skipped.
    pub(crate) fn session_dirs(&self, cwd: Option<&str>) -> Result<Vec<PathBuf>> {
        let cwd_parent = cwd.map(|cwd| {
            self.sessions_root
                .join(distill_config::encode_cwd_dirname(cwd))
        });
        Ok(self
            .persisted_candidates
            .values()
            .filter(|paths| {
                cwd_parent
                    .as_deref()
                    .is_none_or(|parent| paths.iter().any(|path| path.parent() == Some(parent)))
            })
            .filter_map(|paths| select(paths, cwd_parent.as_deref()))
            .collect())
    }

    pub(crate) fn find_persisted_session_dir(&self, session_id: &str) -> Result<Option<PathBuf>> {
        self.find_session_dir(session_id, &self.persisted_candidates)
    }

    pub(crate) fn find_any_session_dir(&self, session_id: &str) -> Result<Option<PathBuf>> {
        self.find_session_dir(session_id, &self.all_candidates)
    }

    fn find_session_dir(
        &self,
        session_id: &str,
        candidates: &SessionCandidates,
    ) -> Result<Option<PathBuf>> {
        validate_component("session id", session_id)?;
        let paths = candidates.get(session_id).map(Vec::as_slice).unwrap_or(&[]);
        Ok(select(paths, None))
    }
}

/// A candidate is unambiguous only when exactly one path carries the id.
fn select(paths: &[PathBuf], cwd_parent: Option<&Path>) -> Option<PathBuf> {
    let selected = (paths.len() == 1).then(|| paths.first().cloned()).flatten();
    selected.filter(|path| cwd_parent.is_none_or(|parent| path.parent() == Some(parent)))
}

fn load_candidates(sessions_root: &Path) -> Result<(SessionCandidates, SessionCandidates)> {
    let entries = match fs::read_dir(sessions_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((HashMap::new(), HashMap::new()));
        }
        Err(error) => return Err(io_error("read", sessions_root, error)),
    };
    let mut all = SessionCandidates::new();
    let mut persisted = SessionCandidates::new();
    for cwd_entry in entries {
        let cwd_entry = cwd_entry.map_err(|error| io_error("read", sessions_root, error))?;
        let cwd_path = cwd_entry.path();
        let cwd_type = cwd_entry
            .file_type()
            .map_err(|error| io_error("inspect", &cwd_path, error))?;
        if !cwd_type.is_dir() || cwd_type.is_symlink() {
            continue;
        }
        // A sibling test (or another process on a shared $HOME) can unlink this
        // cwd bucket after readdir(parent) and before we open it.
        let session_entries = match fs::read_dir(&cwd_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("read", &cwd_path, error)),
        };
        for session_entry in session_entries {
            let session_entry = match session_entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("read", &cwd_path, error)),
            };
            let path = session_entry.path();
            let file_type = match session_entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error("inspect", &path, error)),
            };
            let Some(id) = session_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !file_type.is_dir() || file_type.is_symlink() || id.starts_with('.') {
                continue;
            }
            all.entry(id.clone()).or_default().push(path.clone());
            let summary = path.join(super::SUMMARY_FILE);
            match fs::symlink_metadata(&summary) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    persisted.entry(id).or_default().push(path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error("inspect", &summary, error)),
                Ok(_) => {}
            }
        }
    }
    Ok((all, persisted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn load_skips_cwd_bucket_that_vanishes_during_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().to_path_buf();
        fs::create_dir_all(&sessions).unwrap();
        for i in 0..32 {
            let _ = fs::create_dir_all(sessions.join(format!("%2Fstable{i}")).join("sid"));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let writer_root = sessions.clone();
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let bucket = writer_root.join("%2Ftmp%2F.tmprace%2Frepo");
            while !writer_stop.load(Ordering::Relaxed) {
                let sid = bucket.join("sid");
                let _ = fs::create_dir_all(&sid);
                let _ = fs::write(sid.join("summary.json"), b"{}");
                let _ = fs::remove_dir_all(&bucket);
            }
        });

        for _ in 0..200 {
            RelocationView::load_for_sessions_root(&sessions)
                .expect("a cwd bucket deleted mid-scan must not fail listing");
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer thread");
    }
}
