// Modified for Distill by Samuel Fajreldines, 2026.
use crate::file_system::{AsyncFileSystem, FsError};
use agent_client_protocol as acp;
use std::path::{Path, PathBuf};
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;

pub struct AcpSessionFs {
    root: PathBuf,
    gateway: GatewaySender,
    session_id: acp::SessionId,
    /// When set, any path under `display_cwd` is rewritten to `root` before the extension sees it.
    /// Defense in depth for AB overlay isolation: a tool that passes the display path still reads and writes the overlay.
    display_cwd: Option<PathBuf>,
}

impl AcpSessionFs {
    pub fn new(root: PathBuf, session_id: acp::SessionId, gateway: GatewaySender) -> Self {
        Self {
            root,
            session_id,
            gateway,
            display_cwd: None,
        }
    }

    /// When AB FS isolation is active, the model sees `display_cwd` (e.g., `/testbed/project`) but writes should go to `root` (the overlay path).
    /// Any path under `display_cwd` is rewritten to the equivalent path under `root`.
    pub fn with_display_cwd(mut self, display_cwd: PathBuf) -> Self {
        self.display_cwd = Some(display_cwd);
        self
    }

    /// Rewrite a display path to the overlay path if needed.
    fn resolve_path(&self, path: &Path) -> PathBuf {
        if let Some(ref display) = self.display_cwd
            && let Ok(suffix) = path.strip_prefix(display)
        {
            let resolved = self.root.join(suffix);
            tracing::debug!(
                display_path = %path.display(),
                resolved_path = %resolved.display(),
                "AcpSessionFs: rewrote display path to overlay path"
            );
            return resolved;
        }
        path.to_path_buf()
    }
}

#[async_trait::async_trait]
impl AsyncFileSystem for AcpSessionFs {
    fn root(&self) -> &Path {
        &self.root
    }

    async fn exists(&self, path: &Path) -> Result<bool, FsError> {
        let resolved = self.resolve_path(path);
        let read_req = acp::ReadTextFileRequest::new(self.session_id.clone(), resolved).limit(0);
        match self.gateway.send(read_req).await {
            Ok(_) => Ok(true),
            Err(e) if e.code == acp::ErrorCode::ResourceNotFound => Ok(false),
            Err(e) => Err(FsError::Other(e.to_string())),
        }
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, FsError> {
        let resolved = self.resolve_path(path);
        let read_req = acp::ReadTextFileRequest::new(self.session_id.clone(), resolved);
        let response = self
            .gateway
            .send(read_req)
            .await
            .map_err(|e| FsError::Other(e.to_string()))?;
        Ok(response.content.into_bytes())
    }

    async fn try_read_file(&self, path: &Path) -> Result<Option<Vec<u8>>, FsError> {
        let resolved = self.resolve_path(path);
        let read_req = acp::ReadTextFileRequest::new(self.session_id.clone(), resolved);
        match self.gateway.send(read_req).await {
            Ok(response) => Ok(Some(response.content.into_bytes())),
            Err(e) if e.code == acp::ErrorCode::ResourceNotFound => Ok(None),
            Err(e) => Err(FsError::Other(e.to_string())),
        }
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FsError> {
        let resolved = self.resolve_path(path);
        let write_req = acp::WriteTextFileRequest::new(
            self.session_id.clone(),
            resolved,
            String::from_utf8(data.to_vec()).map_err(|e| FsError::Other(e.to_string()))?,
        );
        self.gateway
            .send(write_req)
            .await
            .map_err(|e| FsError::Other(e.to_string()))?;
        Ok(())
    }

    async fn delete_file(&self, path: &Path) -> Result<(), FsError> {
        // ACP protocol doesn't support file deletion yet
        tracing::warn!(?path, "ACP filesystem does not support file deletion");
        Err(FsError::Other(
            "File deletion not supported via ACP".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // resolve_path only uses self.root and self.display_cwd, so this helper inlines the same logic without needing a gateway
    fn test_resolve(root: &str, display_cwd: Option<&str>, input: &str) -> PathBuf {
        let root = PathBuf::from(root);
        let display = display_cwd.map(PathBuf::from);
        if let Some(ref display) = display
            && let Ok(suffix) = Path::new(input).strip_prefix(display)
        {
            return root.join(suffix);
        }
        PathBuf::from(input)
    }

    #[test]
    fn resolve_path_rewrites_display_to_overlay() {
        let result = test_resolve(
            "/root/.grok/worktrees/proj/ab-123-b-overlay",
            Some("/testbed/proj"),
            "/testbed/proj/src/main.rs",
        );
        assert_eq!(
            result,
            PathBuf::from("/root/.grok/worktrees/proj/ab-123-b-overlay/src/main.rs")
        );
    }

    #[test]
    fn resolve_path_passes_through_overlay_path() {
        let overlay_path = "/root/.grok/worktrees/proj/ab-123-b-overlay/src/main.rs";
        let result = test_resolve(
            "/root/.grok/worktrees/proj/ab-123-b-overlay",
            Some("/testbed/proj"),
            overlay_path,
        );
        assert_eq!(result, PathBuf::from(overlay_path));
    }

    #[test]
    fn resolve_path_no_display_cwd_passthrough() {
        let result = test_resolve(
            "/root/.grok/worktrees/proj/ab-123-b-overlay",
            None,
            "/testbed/proj/src/main.rs",
        );
        assert_eq!(result, PathBuf::from("/testbed/proj/src/main.rs"));
    }

    #[test]
    fn resolve_path_relative_path_passthrough() {
        let result = test_resolve(
            "/root/.grok/worktrees/proj/ab-123-b-overlay",
            Some("/testbed/proj"),
            "src/main.rs",
        );
        assert_eq!(result, PathBuf::from("src/main.rs"));
    }
}
