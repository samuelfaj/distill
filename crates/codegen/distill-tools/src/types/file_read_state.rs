// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-session record of the file contents the tools last read, wrote or edited.
//!
//! The write and edit tools compare a file's current bytes with what the session last saw, so a
//! change made by another process or session is detected instead of silently overwritten. The state
//! lives in the tool [`SharedResources`], so it is scoped to one toolset: a subagent has its own.
//!
//! Each entry stores a content hash plus the mtime and size taken *before* the bytes were read.
//! Matching mtime and size short-circuits the hash comparison; any other case compares hashes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use crate::types::resources::SharedResources;

/// Appended to an edit result when the file changed outside this session since the session last saw it.
pub const CHANGED_OUTSIDE_NOTE: &str = " Note: this file changed outside this session since you last read it; the edit matched uniquely and was applied, but re-read the file to see its current content.";

/// A file modified more recently than this may change again within the filesystem's mtime
/// granularity without its stamp changing, so such a stamp is dropped and the hash decides.
const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// File mtime and size at one point in time. `FileStamp::default()` carries no stamp and forces a
/// hash comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileStamp(Option<(SystemTime, u64)>);

impl FileStamp {
    /// Stat `path`. Capture it *before* reading the file bytes so a concurrent change between the
    /// stat and the read can only cause an extra hash comparison, never a missed change.
    pub async fn capture(path: &Path) -> Self {
        match tokio::time::timeout(
            crate::util::fs::FS_SYSCALL_TIMEOUT,
            tokio::fs::metadata(path),
        )
        .await
        {
            Ok(Ok(meta)) => Self(
                meta.modified()
                    .ok()
                    .filter(|mtime| mtime.elapsed().is_ok_and(|age| age > RACY_WINDOW))
                    .map(|mtime| (mtime, meta.len())),
            ),
            _ => Self(None),
        }
    }
}

#[derive(Debug)]
struct Seen {
    hash: [u8; 32],
    stamp: FileStamp,
}

/// Path -> what this session last saw there. Keys are canonical paths.
#[derive(Debug, Default)]
pub struct FileReadState {
    seen: HashMap<PathBuf, Seen>,
}

impl FileReadState {
    fn record(&mut self, key: PathBuf, bytes: &[u8], stamp: FileStamp) {
        let hash = Sha256::digest(bytes).into();
        self.seen.insert(key, Seen { hash, stamp });
    }

    /// True only when the session saw `key` before and its content differs now.
    fn changed(&self, key: &Path, bytes: &[u8], stamp: FileStamp) -> bool {
        let Some(seen) = self.seen.get(key) else {
            return false;
        };
        if stamp.0.is_some() && stamp == seen.stamp {
            return false;
        }
        let hash: [u8; 32] = Sha256::digest(bytes).into();
        hash != seen.hash
    }
}

async fn key_for(path: &Path) -> PathBuf {
    crate::util::fs::canonicalize_with_timeout(path.to_path_buf()).await
}

/// Remember that the session now knows `bytes` as the content of `path` (after a read, write or edit).
pub async fn record_seen(resources: &SharedResources, path: &Path, bytes: &[u8], stamp: FileStamp) {
    let key = key_for(path).await;
    resources
        .lock()
        .await
        .get_or_default::<FileReadState>()
        .record(key, bytes, stamp);
}

/// Forget `path`, e.g. after the session deleted it.
pub async fn forget_seen(resources: &SharedResources, path: &Path) {
    let key = key_for(path).await;
    if let Some(state) = resources.lock().await.get_mut::<FileReadState>() {
        state.seen.remove(&key);
    }
}

/// Whether `path` (currently holding `bytes`, with `stamp` captured before reading them) changed
/// since the session last saw it. False for a path the session never saw.
pub async fn changed_since_seen(
    resources: &SharedResources,
    path: &Path,
    bytes: &[u8],
    stamp: FileStamp,
) -> bool {
    let key = key_for(path).await;
    resources
        .lock()
        .await
        .get::<FileReadState>()
        .is_some_and(|state| state.changed(&key, bytes, stamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unseen_path_is_never_changed() {
        let state = FileReadState::default();
        assert!(!state.changed(Path::new("/a"), b"x", FileStamp::default()));
    }

    #[test]
    fn same_content_is_unchanged_and_different_content_is_changed() {
        let mut state = FileReadState::default();
        state.record("/a".into(), b"one", FileStamp::default());
        assert!(!state.changed(Path::new("/a"), b"one", FileStamp::default()));
        assert!(state.changed(Path::new("/a"), b"two", FileStamp::default()));
    }

    #[test]
    fn matching_stamp_skips_the_hash_comparison() {
        let stamp = FileStamp(Some((SystemTime::UNIX_EPOCH, 3)));
        let mut state = FileReadState::default();
        state.record("/a".into(), b"one", stamp);
        // Different bytes under an identical stamp are trusted as unchanged (fast path).
        assert!(!state.changed(Path::new("/a"), b"two", stamp));
        let other = FileStamp(Some((SystemTime::UNIX_EPOCH, 4)));
        assert!(state.changed(Path::new("/a"), b"two", other));
    }

    fn write_with_mtime(path: &Path, content: &str, mtime: SystemTime) {
        std::fs::write(path, content).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[tokio::test]
    async fn recent_mtime_forces_a_hash_comparison_but_old_mtime_uses_the_fast_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let resources = crate::types::resources::Resources::new().into_shared();
        let file = tmp.path().join("f.txt");

        // Old file: same size and mtime means unchanged, without hashing.
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        write_with_mtime(&file, "one", old);
        let stamp = FileStamp::capture(&file).await;
        record_seen(&resources, &file, b"one", stamp).await;
        write_with_mtime(&file, "two", old);
        let stamp = FileStamp::capture(&file).await;
        assert!(!changed_since_seen(&resources, &file, b"two", stamp).await);

        // Freshly modified file: a same-size edit is still caught by the hash.
        let fresh = SystemTime::now();
        write_with_mtime(&file, "one", fresh);
        let stamp = FileStamp::capture(&file).await;
        record_seen(&resources, &file, b"one", stamp).await;
        write_with_mtime(&file, "two", fresh);
        let stamp = FileStamp::capture(&file).await;
        assert!(changed_since_seen(&resources, &file, b"two", stamp).await);
    }

    #[tokio::test]
    async fn paths_are_keyed_by_canonical_location() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("f.txt");
        std::fs::write(&file, "one").unwrap();
        let resources = crate::types::resources::Resources::new().into_shared();
        record_seen(&resources, &file, b"one", FileStamp::default()).await;

        let alias = tmp.path().join("sub/../f.txt");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        assert!(!changed_since_seen(&resources, &alias, b"one", FileStamp::default()).await);
        assert!(changed_since_seen(&resources, &alias, b"two", FileStamp::default()).await);

        forget_seen(&resources, &file).await;
        assert!(!changed_since_seen(&resources, &file, b"two", FileStamp::default()).await);
    }
}
