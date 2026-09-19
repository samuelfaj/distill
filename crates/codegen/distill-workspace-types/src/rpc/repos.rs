//! Provisioned-repo listing (`workspace.repos_list`) and the on-disk in-sandbox manifest contract (`{workspace}/.grok/repos.json`).
//!
//! The sandbox provisioner writes this manifest; the workspace list op reads it.
//! Field names are the frontend/integration API: add optional fields with `#[serde(default)]` rather than renaming existing ones.

use serde::{Deserialize, Serialize};

use super::{RpcActivityClass, WorkspaceRpc};

/// Relative path of the provisioner manifest from the **sandbox** `workspace_directory` (init root, usually `/workspace`).
/// It is not relative to the agent / workspace-server `--cwd` after a single-repo rewrite (`/workspace/app`).
/// Writers and `workspace.repos_list` must join this to that sandbox root.
pub const REPOS_MANIFEST_RELATIVE_PATH: &str = ".grok/repos.json";

/// Current on-disk / wire manifest version.
pub const REPOS_MANIFEST_VERSION: u32 = 1;

/// `workspace.repos_list`: list repos materialized into this workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReposListReq {}

impl WorkspaceRpc for ReposListReq {
    const METHOD: &'static str = "workspace.repos_list";
    const ACTIVITY: RpcActivityClass = RpcActivityClass::Read;
    type Response = ReposListResponse;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReposListResponse {
    /// On-disk manifest version (`REPOS_MANIFEST_VERSION` for a missing file).
    #[serde(default)]
    pub version: u32,
    pub repos: Vec<ProvisionedRepo>,
}

/// How a provisioned repo was materialized into the sandbox.
/// Absent on older manifests; unknown values must not fail parse of a snapshot-preserved `repos.json`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoBackend {
    Grove,
    CloneScript,
    #[serde(other)]
    Unknown,
}

/// One provisioned repository as exposed to frontend / workspace callers.
///
/// Expandable: new optional fields should use `#[serde(default, skip_serializing_if = "Option::is_none")]`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvisionedRepo {
    /// Short directory / display name (usually the git repo name).
    pub name: String,
    /// Repository identity (`owner/repo` or normalized URL form).
    pub repository: String,
    /// Absolute in-sandbox (or workspace-relative absolute) mount path.
    pub mount_path: String,
    /// Fork-from ref. Empty means unset (missing session branch is fatal).
    /// `"HEAD"` means the remote default. Do not treat empty as HEAD.
    pub base_branch: String,
    /// Session working branch created at provision time.
    pub session_branch: String,
    /// Materialization backend. `None` on manifests written before this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_backend: Option<RepoBackend>,
}

/// On-disk manifest written by the sandbox provisioner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoManifest {
    pub version: u32,
    pub repos: Vec<ProvisionedRepo>,
}

impl RepoManifest {
    pub fn new(repos: Vec<ProvisionedRepo>) -> Self {
        Self {
            version: REPOS_MANIFEST_VERSION,
            repos,
        }
    }

    /// Parse bytes from `{workspace}/.grok/repos.json`.
    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Every distinct provisioned mount, or `[workspace_root]` when the manifest is empty (single-tree / no repos.json).
    /// Prompt, graph, and fs-notify walk this list so multi-repo workspaces are not limited to the primary repo.
    pub fn materialized_mounts(&self, workspace_root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out: Vec<std::path::PathBuf> = Vec::new();
        for repo in &self.repos {
            let raw = repo.mount_path.trim();
            if raw.is_empty() {
                continue;
            }
            let mount = std::path::PathBuf::from(raw);
            // Confine to the workspace: a malicious `.grok/repos.json` must not point walks outside it
            // Reject `..` and any mount not under `workspace_root`, matching `unnamed_cwd` / `confine_mount_under_workspace`
            if mount
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
                || !mount.starts_with(workspace_root)
            {
                continue;
            }
            if !out
                .iter()
                .any(|existing| existing.components().eq(mount.components()))
            {
                out.push(mount);
            }
        }
        if out.is_empty() {
            out.push(workspace_root.to_path_buf());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trip() {
        let manifest = RepoManifest::new(vec![
            ProvisionedRepo {
                name: "app".into(),
                repository: "acme/app".into(),
                mount_path: "/workspace/app".into(),
                base_branch: "main".into(),
                session_branch: "grok/s1".into(),
                repo_backend: Some(RepoBackend::Grove),
            },
            ProvisionedRepo {
                name: "lib".into(),
                repository: "acme/lib".into(),
                mount_path: "/workspace/lib".into(),
                base_branch: "HEAD".into(),
                session_branch: "feat/x".into(),
                repo_backend: Some(RepoBackend::CloneScript),
            },
        ]);
        let bytes = manifest.to_json_bytes().expect("serialize");
        let recovered = RepoManifest::from_json_bytes(&bytes).expect("parse");
        assert_eq!(manifest, recovered);
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("\"repo_backend\": \"grove\""), "{text}");
        assert!(
            text.contains("\"repo_backend\": \"clone_script\""),
            "{text}"
        );
    }

    #[test]
    fn legacy_manifest_without_repo_backend_deserializes() {
        let json = r#"{
            "version": 1,
            "repos": [{
                "name": "app",
                "repository": "acme/app",
                "mount_path": "/workspace/app",
                "base_branch": "main",
                "session_branch": "grok/s1"
            }]
        }"#;
        let recovered = RepoManifest::from_json_bytes(json.as_bytes()).expect("parse");
        let Some(repo) = recovered.repos.first() else {
            panic!("expected one repo: {:?}", recovered.repos);
        };
        assert_eq!(repo.name, "app");
        assert_eq!(repo.repo_backend, None);
        let out = String::from_utf8(recovered.to_json_bytes().expect("serialize")).expect("utf8");
        assert!(
            !out.contains("repo_backend"),
            "None must omit the field: {out}"
        );
    }

    #[test]
    fn unknown_repo_backend_does_not_fail_parse() {
        let json = r#"{
            "version": 1,
            "repos": [{
                "name": "app",
                "repository": "acme/app",
                "mount_path": "/workspace",
                "base_branch": "main",
                "session_branch": "grok/s1",
                "repo_backend": "future_backend"
            }]
        }"#;
        let recovered = RepoManifest::from_json_bytes(json.as_bytes()).expect("parse");
        let Some(repo) = recovered.repos.first() else {
            panic!("expected one repo: {:?}", recovered.repos);
        };
        assert_eq!(repo.repo_backend, Some(RepoBackend::Unknown));
    }

    #[test]
    fn materialized_mounts_empty_falls_back_to_workspace_root() {
        let mounts =
            RepoManifest::new(Vec::new()).materialized_mounts(std::path::Path::new("/workspace"));
        assert_eq!(mounts, vec![std::path::PathBuf::from("/workspace")]);
    }

    #[test]
    fn materialized_mounts_lists_every_distinct_repo() {
        let mounts =
            nested_two_repo_manifest().materialized_mounts(std::path::Path::new("/workspace"));
        assert_eq!(
            mounts,
            vec![
                std::path::PathBuf::from("/workspace/app"),
                std::path::PathBuf::from("/workspace/lib"),
            ]
        );
    }

    #[test]
    fn materialized_mounts_rejects_out_of_workspace_and_traversal() {
        // Compromised repos.json must not escape the workspace; unsafe mounts fall back to the root.
        let manifest = RepoManifest::new(vec![
            ProvisionedRepo {
                name: "evil".into(),
                repository: "acme/evil".into(),
                mount_path: "/etc".into(),
                base_branch: "main".into(),
                session_branch: "conv/1".into(),
                repo_backend: None,
            },
            ProvisionedRepo {
                name: "traverse".into(),
                repository: "acme/traverse".into(),
                mount_path: "/workspace/../etc".into(),
                base_branch: "main".into(),
                session_branch: "conv/1".into(),
                repo_backend: None,
            },
        ]);
        let mounts = manifest.materialized_mounts(std::path::Path::new("/workspace"));
        assert_eq!(mounts, vec![std::path::PathBuf::from("/workspace")]);
    }

    #[test]
    fn materialized_mounts_keeps_safe_and_drops_unsafe() {
        let manifest = RepoManifest::new(vec![
            ProvisionedRepo {
                name: "app".into(),
                repository: "acme/app".into(),
                mount_path: "/workspace/app".into(),
                base_branch: "main".into(),
                session_branch: "conv/1".into(),
                repo_backend: None,
            },
            ProvisionedRepo {
                name: "evil".into(),
                repository: "acme/evil".into(),
                mount_path: "/tmp/evil".into(),
                base_branch: "main".into(),
                session_branch: "conv/1".into(),
                repo_backend: None,
            },
        ]);
        let mounts = manifest.materialized_mounts(std::path::Path::new("/workspace"));
        assert_eq!(mounts, vec![std::path::PathBuf::from("/workspace/app")]);
    }

    fn nested_two_repo_manifest() -> RepoManifest {
        RepoManifest::new(vec![
            ProvisionedRepo {
                name: "app".into(),
                repository: "acme/app".into(),
                mount_path: "/workspace/app".into(),
                base_branch: "main".into(),
                session_branch: "conv/1".into(),
                repo_backend: None,
            },
            ProvisionedRepo {
                name: "lib".into(),
                repository: "acme/lib".into(),
                mount_path: "/workspace/lib".into(),
                base_branch: "main".into(),
                session_branch: "feat/x".into(),
                repo_backend: None,
            },
        ])
    }
}
