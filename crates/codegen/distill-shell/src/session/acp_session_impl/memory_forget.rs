// Modified for Distill by Samuel Fajreldines, 2026.
//! `x.ai/memory/forget`: delete one note from the `/memory` modal.
//! v2 goes through `V2MaintenanceStore::forget` (tombstone, index, manifest); legacy session
//! logs have no ledger and are unlinked directly.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::extensions::memory::{
    MEMORY_FORGET_MAX_FILE_BYTES, MemoryForgetRejection, MemoryForgetResponse,
};
use crate::session::acp_session::SessionActor;
use crate::session::memory::{MemoryStorage, V2MemoryScope};

impl SessionActor {
    pub(crate) async fn memory_forget(
        &self,
        path: &str,
        expected_content_hash: &str,
    ) -> MemoryForgetResponse {
        let Some(storage) = self.memory.storage() else {
            return rejected(
                MemoryForgetRejection::MemoryDisabled,
                "Memory is off for this session.",
            );
        };
        let path = PathBuf::from(path);
        let hash = expected_content_hash.to_owned();
        let session_id = self.session_info.id.to_string();
        let result = tokio::task::spawn_blocking(move || forget_blocking(&storage, &path, &hash))
            .await
            .unwrap_or_else(|error| {
                rejected(
                    MemoryForgetRejection::Failed,
                    format!("Delete task failed: {error}"),
                )
            });
        tracing::info!(
            target: distill_telemetry::memory_log::TARGET,
            session_id,
            ?result,
            "memory forget: user deleted a note from the /memory modal",
        );
        result
    }
}

fn rejected(reason: MemoryForgetRejection, message: impl Into<String>) -> MemoryForgetResponse {
    MemoryForgetResponse::Rejected {
        reason,
        message: message.into(),
    }
}

fn forget_blocking(storage: &MemoryStorage, path: &Path, hash: &str) -> MemoryForgetResponse {
    if storage.mode().is_v2() {
        forget_v2(storage, path, hash)
    } else {
        forget_legacy(storage, path, hash)
    }
}

fn forget_v2(storage: &MemoryStorage, path: &Path, hash: &str) -> MemoryForgetResponse {
    let scopes = [
        (storage.workspace_dir(), V2MemoryScope::Workspace),
        (storage.global_dir(), V2MemoryScope::Global),
    ];
    let Some((scope_dir, scope, relative)) = scopes.into_iter().find_map(|(dir, scope)| {
        path.strip_prefix(dir)
            .ok()
            .map(|relative| (dir, scope, relative))
    }) else {
        return rejected(
            MemoryForgetRejection::NotDeletable,
            "This file is not part of the memory store.",
        );
    };
    let clock = distill_memory::system_v2_clock();
    let store = match distill_memory::V2MaintenanceStore::open_with_clock(
        scope_dir,
        scope,
        storage.global_dir(),
        storage.workspace_dir(),
        clock.clone(),
    ) {
        Ok(store) => store,
        Err(error) => return map_v2_error(error),
    };
    let request = distill_memory::ForgetRequest {
        relative_path: relative.to_path_buf(),
        expected_content_hash: hash.to_owned(),
        reason: distill_memory::ForgetReason::UserRequest,
        now: clock.now_unix_seconds(),
    };
    match store.forget(&request) {
        Ok(result) => MemoryForgetResponse::Forgotten {
            was_already_forgotten: result.was_already_forgotten,
        },
        Err(error) => map_v2_error(error),
    }
}

fn map_v2_error(error: distill_memory::V2MaintenanceError) -> MemoryForgetResponse {
    use distill_memory::V2MaintenanceError as E;
    match error {
        E::Protected | E::Invalid(_) => rejected(
            MemoryForgetRejection::NotDeletable,
            "Only topic and observation notes can be deleted.",
        ),
        E::EvidenceMismatch => rejected(
            MemoryForgetRejection::Changed,
            "This note changed since you opened it. Reopen /memory to delete it.",
        ),
        E::ActiveLease => rejected(
            MemoryForgetRejection::DreamRunning,
            "Dream is organizing memory right now. Try again in a moment.",
        ),
        other => rejected(
            MemoryForgetRejection::Failed,
            format!("Couldn't delete the note: {other}"),
        ),
    }
}

/// Legacy stores have no ledger; only per-session logs are deletable, and only if unchanged.
fn forget_legacy(storage: &MemoryStorage, path: &Path, hash: &str) -> MemoryForgetResponse {
    // `classify_source` labels every path outside the store "session", so it cannot gate deletion.
    // Canonicalize both sides so `..` and symlinks cannot point outside the sessions directory.
    let not_deletable = || {
        rejected(
            MemoryForgetRejection::NotDeletable,
            "Only session logs can be deleted from legacy memory.",
        )
    };
    let Ok(sessions_dir) = dunce::canonicalize(storage.sessions_dir()) else {
        return not_deletable();
    };
    let path = match dunce::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MemoryForgetResponse::Forgotten {
                was_already_forgotten: true,
            };
        }
        Err(_) => return not_deletable(),
    };
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return not_deletable();
    };
    if path.parent() != Some(sessions_dir.as_path())
        || path.extension().and_then(|e| e.to_str()) != Some("md")
        || !metadata.file_type().is_file()
    {
        return not_deletable();
    }
    let too_large = || {
        rejected(
            MemoryForgetRejection::NotDeletable,
            "This note is too large to delete from here.",
        )
    };
    if metadata.len() > MEMORY_FORGET_MAX_FILE_BYTES {
        return too_large();
    }
    let mut bytes = Vec::new();
    if let Err(error) = std::fs::File::open(&path).and_then(|file| {
        file.take(MEMORY_FORGET_MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
    }) {
        return rejected(
            MemoryForgetRejection::Failed,
            format!("Couldn't read the note: {error}"),
        );
    }
    if bytes.len() as u64 > MEMORY_FORGET_MAX_FILE_BYTES {
        return too_large();
    }
    if blake3::hash(&bytes).to_hex().as_str() != hash {
        return rejected(
            MemoryForgetRejection::Changed,
            "This note changed since you opened it. Reopen /memory to delete it.",
        );
    }
    match std::fs::remove_file(path) {
        Ok(()) => MemoryForgetResponse::Forgotten {
            was_already_forgotten: false,
        },
        Err(error) => rejected(
            MemoryForgetRejection::Failed,
            format!("Couldn't delete the note: {error}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryMode;

    fn hash_of(path: &Path) -> String {
        blake3::hash(&std::fs::read(path).unwrap())
            .to_hex()
            .to_string()
    }

    fn v2_storage() -> (tempfile::TempDir, MemoryStorage) {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("memory-v2");
        let cwd = temporary.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let storage = MemoryStorage::new_for_mode(&cwd, Some(&root), MemoryMode::V2);
        std::fs::create_dir_all(storage.global_dir()).unwrap();
        std::fs::create_dir_all(storage.workspace_dir()).unwrap();
        distill_memory::ensure_scope_initialized(
            &root,
            storage.global_dir(),
            V2MemoryScope::Global,
        )
        .unwrap();
        distill_memory::ensure_scope_initialized(
            &root,
            storage.workspace_dir(),
            V2MemoryScope::Workspace,
        )
        .unwrap();
        (temporary, storage)
    }

    #[test]
    fn v2_topic_is_tombstoned_and_manifest_refreshed() {
        let (_temporary, storage) = v2_storage();
        let topics = storage.workspace_dir().join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        let topic = topics.join("anyrun.md");
        std::fs::write(&topic, "# Anyrun\n\nSandbox notes.\n").unwrap();
        let hash = hash_of(&topic);
        distill_memory::regenerate_scope_manifest(
            storage.workspace_dir(),
            V2MemoryScope::Workspace,
            distill_memory::V2ManifestBudget::default(),
        )
        .unwrap();
        let manifest = storage.workspace_memory_file();
        assert!(
            std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("anyrun")
        );

        assert_eq!(
            forget_blocking(&storage, &topic, &hash),
            MemoryForgetResponse::Forgotten {
                was_already_forgotten: false
            }
        );
        assert!(!topic.exists());
        assert!(
            !std::fs::read_to_string(&manifest)
                .unwrap()
                .contains("anyrun")
        );
        // Idempotent for the same evidence.
        assert_eq!(
            forget_blocking(&storage, &topic, &hash),
            MemoryForgetResponse::Forgotten {
                was_already_forgotten: true
            }
        );
    }

    #[test]
    fn v2_rejections_map_to_user_facing_reasons() {
        let (temporary, storage) = v2_storage();
        let topics = storage.workspace_dir().join("topics");
        std::fs::create_dir_all(&topics).unwrap();
        let topic = topics.join("t.md");
        std::fs::write(&topic, "before\n").unwrap();
        let stale = hash_of(&topic);
        std::fs::write(&topic, "after\n").unwrap();
        assert!(matches!(
            forget_blocking(&storage, &topic, &stale),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::Changed,
                ..
            }
        ));
        assert!(topic.exists());

        let manifest = storage.workspace_memory_file();
        assert!(matches!(
            forget_blocking(&storage, &manifest, &hash_of(&manifest)),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::NotDeletable,
                ..
            }
        ));
        assert!(manifest.exists());

        let outside = temporary.path().join("elsewhere.md");
        std::fs::write(&outside, "x").unwrap();
        assert!(matches!(
            forget_blocking(&storage, &outside, &hash_of(&outside)),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::NotDeletable,
                ..
            }
        ));
    }

    #[test]
    fn legacy_session_log_is_unlinked_only_when_unchanged() {
        let temporary = tempfile::tempdir().unwrap();
        let global = temporary.path().join("global");
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let storage = MemoryStorage::with_paths(global, workspace.clone());
        std::fs::create_dir_all(storage.sessions_dir()).unwrap();
        let log = storage.sessions_dir().join("2026-09-14-session.md");
        std::fs::write(&log, "log\n").unwrap();

        // `classify_source` calls anything outside the store "session"; forget must not.
        let outside = temporary.path().join("elsewhere.md");
        std::fs::write(&outside, "x").unwrap();
        assert_eq!(storage.classify_source(&outside), "session");
        assert!(matches!(
            forget_blocking(&storage, &outside, &hash_of(&outside)),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::NotDeletable,
                ..
            }
        ));
        assert!(outside.exists());
        let traversal = storage.sessions_dir().join("../../elsewhere.md");
        assert!(matches!(
            forget_blocking(&storage, &traversal, &hash_of(&outside)),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::NotDeletable,
                ..
            }
        ));
        assert!(outside.exists());

        assert!(matches!(
            forget_blocking(&storage, &log, "00"),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::Changed,
                ..
            }
        ));
        assert!(log.exists());
        assert_eq!(
            forget_blocking(&storage, &log, &hash_of(&log)),
            MemoryForgetResponse::Forgotten {
                was_already_forgotten: false
            }
        );
        assert!(!log.exists());

        let memory_md = storage.workspace_memory_file();
        std::fs::write(&memory_md, "curated\n").unwrap();
        assert!(matches!(
            forget_blocking(&storage, &memory_md, &hash_of(&memory_md)),
            MemoryForgetResponse::Rejected {
                reason: MemoryForgetRejection::NotDeletable,
                ..
            }
        ));
    }
}
