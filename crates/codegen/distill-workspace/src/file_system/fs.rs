// Modified for Distill by Samuel Fajreldines, 2026.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use distill_paths::ToAbsPath;

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Other(String),
}

// TODO: handle atomic write
#[async_trait::async_trait]
pub trait AsyncFileSystem: Send + Sync {
    /// Callers resolve relative paths via `ToAbsPath::to_abs_path(fs.root())`.
    fn root(&self) -> &Path;

    /// Re-root this filesystem. Default is a no-op.
    /// [`LocalFs`] overrides it with a remount that can be set once, so `/workspace` can follow a later `session_root` without dropping the `Arc`.
    fn remount_root(&self, _root: PathBuf) {}

    async fn exists(&self, path: &Path) -> Result<bool, FsError>;

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, FsError>;

    /// Read a file if it exists, returning `Ok(None)` when not found.
    /// The default is `exists()` then `read_file()`; backends should collapse that into one RPC or syscall.
    async fn try_read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, FsError> {
        if self.exists(path).await? {
            Ok(Some(self.read_file(path).await?))
        } else {
            Ok(None)
        }
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FsError>;

    /// Delete a file (for rewind)
    async fn delete_file(&self, path: &Path) -> Result<(), FsError>;
}

pub fn bytes_to_string(file_bytes: Vec<u8>) -> Result<String, FsError> {
    String::from_utf8(file_bytes).map_err(|e| FsError::Other(e.to_string()))
}

/// Wrapper around `AsyncFileSystem` that accepts any `ToAbsPath` and resolves it against the filesystem root.
#[derive(Clone)]
pub struct AsyncFsWrapper {
    inner: Arc<dyn AsyncFileSystem>,
}

impl AsyncFsWrapper {
    pub fn new(fs: Arc<dyn AsyncFileSystem>) -> Self {
        Self { inner: fs }
    }

    /// Raw access to the underlying filesystem without the path conversion layer.
    pub fn inner(&self) -> &Arc<dyn AsyncFileSystem> {
        &self.inner
    }

    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    pub fn remount_root(&self, root: PathBuf) {
        self.inner.remount_root(root);
    }

    pub async fn exists<P: ToAbsPath>(&self, path: P) -> Result<bool, FsError> {
        self.inner.exists(&path.to_abs_path(self.root())).await
    }

    pub async fn read_file<P: ToAbsPath>(&self, path: P) -> Result<Vec<u8>, FsError> {
        self.inner.read_file(&path.to_abs_path(self.root())).await
    }

    pub async fn read_to_string<P: ToAbsPath>(&self, path: P) -> Result<String, FsError> {
        let bytes = self.inner.read_file(&path.to_abs_path(self.root())).await?;
        bytes_to_string(bytes)
    }

    /// Read a file as bytes if it exists, returning `Ok(None)` when not found.
    /// Uses a single backend operation instead of separate `exists()` and `read_file()` calls.
    pub async fn try_read_file<P: ToAbsPath>(&self, path: P) -> Result<Option<Vec<u8>>, FsError> {
        self.inner
            .try_read_file(&path.to_abs_path(self.root()))
            .await
    }

    /// Read a file as a UTF-8 string if it exists, returning `Ok(None)` when not found.
    /// Uses a single backend operation instead of separate `exists()` and `read_to_string()` calls.
    pub async fn try_read_to_string<P: ToAbsPath>(
        &self,
        path: P,
    ) -> Result<Option<String>, FsError> {
        match self
            .inner
            .try_read_file(&path.to_abs_path(self.root()))
            .await?
        {
            Some(bytes) => Ok(Some(bytes_to_string(bytes)?)),
            None => Ok(None),
        }
    }

    pub async fn write_file<P: ToAbsPath>(&self, path: P, data: &[u8]) -> Result<(), FsError> {
        self.inner
            .write_file(&path.to_abs_path(self.root()), data)
            .await
    }

    pub async fn delete_file<P: ToAbsPath>(&self, path: P) -> Result<(), FsError> {
        self.inner.delete_file(&path.to_abs_path(self.root())).await
    }
}
