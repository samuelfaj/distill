use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::fs;

use crate::file_system::{AsyncFileSystem, FsError};

pub struct LocalFs {
    root: PathBuf,
    remount: OnceLock<PathBuf>,
}

impl LocalFs {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            remount: OnceLock::new(),
        }
    }
}

#[async_trait::async_trait]
impl AsyncFileSystem for LocalFs {
    fn root(&self) -> &Path {
        self.remount.get().unwrap_or(&self.root)
    }

    fn remount_root(&self, root: PathBuf) {
        let _ = self.remount.set(root);
    }

    async fn exists(&self, path: &Path) -> Result<bool, FsError> {
        Ok(fs::try_exists(path).await?)
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        Ok(fs::read(path).await?)
    }

    async fn try_read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, FsError> {
        match fs::read(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).await?;
        }
        fs::write(path, data).await?;
        Ok(())
    }

    async fn delete_file(&self, path: &Path) -> Result<(), FsError> {
        fs::remove_file(path).await?;
        Ok(())
    }
}
