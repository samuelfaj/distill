//! Conversation-to-branch binding for git-backed app workspaces.
//!
//! Pure data and resolution logic shared by the control plane.
//! The two product invariants this encodes:
//!
//! - **One branch per conversation.** Every conversation forks its own `conv/<conversation_id>` branch off a chosen base.
//!   It never writes the base directly; [`conv_branch_name`] is the single source of that naming.
//! - **Repo(s) on the project, chosen per conversation.** A project configures remotes; a conversation binds a *set* of `(remote, branch)`.
//!   [`resolve_repo_sources`] turns `(remotes, bindings)` into the repo set the sandbox provisioner consumes.
//!   The binding, not free-form client input, is the source of the repos.
//!
//! This crate is pure data (no async, no I/O); the git mutations happen in the workspace server via WorkspaceOps and the sandbox provisioner.

use serde::{Deserialize, Serialize};

/// Branch-name prefix for the per-conversation fork. `conv/<conversation_id>`.
pub const CONV_BRANCH_PREFIX: &str = "conv/";

/// The single-writer conversation branch for `conversation_id` (`conv/<conversation_id>`).
/// The mapping is 1:1 with the conversation (or its v5 session id, the same identity), so there are no free-form agent branches.
pub fn conv_branch_name(conversation_id: &str) -> String {
    format!("{CONV_BRANCH_PREFIX}{conversation_id}")
}

/// `true` if `branch` is a conversation branch.
pub fn is_conv_branch(branch: &str) -> bool {
    branch
        .strip_prefix(CONV_BRANCH_PREFIX)
        .is_some_and(|id| !id.is_empty())
}

/// The `conversation_id` encoded in a `conv/<id>` branch, if any.
pub fn conversation_id_of(branch: &str) -> Option<&str> {
    branch
        .strip_prefix(CONV_BRANCH_PREFIX)
        .filter(|id| !id.is_empty())
}

/// Mirrors the DB `project_git_remote_host` enum; kept here so the resolver has no DB dependency.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteHost {
    /// First-party git host (the default backing store).
    #[default]
    AppGit,
    /// A BYO GitHub remote (opt-in). Same push protocol as `AppGit`.
    Github,
}

/// Default in-sandbox workspace root used when joining a multi-repo mount.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";

/// Relative product slug for a remote (`apps/<name>`).
/// Callers that need a mount path the provisioner accepts must join this under the workspace root via [`absolute_mount_path`].
pub fn mount_slug(remote_name: &str) -> String {
    format!("apps/{remote_name}")
}

/// Absolute mount path the provisioner accepts (`validate_mount_path` requires a leading `/`).
/// Single-repo stays at the workspace root (`None`); multi-repo uses `{workspace_root}/apps/<name>`.
pub fn absolute_mount_path(workspace_root: &str, remote_name: &str) -> String {
    let root = workspace_root.trim_end_matches('/');
    format!("{root}/{}", mount_slug(remote_name))
}

/// A remote configured on a project.
/// `name` is the stable per-project slug the binding references (and the `apps/<name>/` mount slug for multi-repo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRemote {
    pub name: String,
    pub url: String,
    pub host: RemoteHost,
    /// The remote's integration branch; the `Main` base resolves to this and publish merges back into it.
    pub default_branch: String,
}

/// The base a conversation forks its `conv/<id>` branch off of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingBase {
    /// The remote's default (integration) branch.
    Main,
    /// A specific branch tip.
    Branch { name: String },
    /// A specific commit (time-travel start).
    Commit { sha: String },
}

/// One entry of a conversation's binding set: which project remote (by `name`), the fork base, and (optionally) the merge-back target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoBinding {
    /// References [`ProjectRemote::name`].
    pub remote_name: String,
    pub base: BindingBase,
    /// Explicit merge-back target branch.
    /// `None` defaults to the base branch (for a `Branch` base) or the remote default branch.
    /// A conversation forked off `feature/x` merges back into `feature/x`, not `main`.
    #[serde(default)]
    pub merge_target: Option<String>,
}

/// A resolved repo source: plan input for start/provision (bindings, then env/picker).
/// Not a proto `GitSource` (that type has no mount) and not a sandbox `RepoSpec` until a caller joins [`absolute_mount_path`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRepoSource {
    /// Per-project remote slug (stable identity for logs/mounts).
    pub name: String,
    /// Fetch/push URL passed to the provisioner.
    pub url: String,
    pub host: RemoteHost,
    /// The conversation branch to check out and commit onto (`conv/<id>`).
    /// The provisioner forks it off `base_ref` if it does not yet exist on the remote, and never writes `base_ref` directly.
    pub session_branch: String,
    /// The ref to fork `session_branch` from: the remote default branch for a `Main` base, else the bound branch name / commit SHA.
    pub base_ref: String,
    /// The branch publish merges `session_branch` back into (never a commit).
    pub merge_target: String,
    /// Mount path the provisioner accepts; callers must pass this through.
    /// `None` means the workspace root (single repo); multi-repo uses `Some("/workspace/apps/<name>")`, absolute as `validate_mount_path` requires.
    pub mount_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingResolveError {
    /// A binding referenced a remote name not configured on the project.
    /// The binding is authoritative (not free-form client input), so an unknown remote is a hard error rather than a silent skip.
    #[error("binding references unknown remote '{0}'")]
    UnknownRemote(String),
    /// Two bindings referenced the same remote (a binding is a *set*).
    #[error("duplicate binding for remote '{0}'")]
    DuplicateRemote(String),
}

/// Resolve a conversation's bindings into the provisionable repo set. Each becomes a [`ResolvedRepoSource`] on `conv/<conversation_id>` from the fork base.
/// A single repo mounts at the workspace root; multi-repo mounts at [`absolute_mount_path`].
pub fn resolve_repo_sources(
    conversation_id: &str,
    remotes: &[ProjectRemote],
    bindings: &[RepoBinding],
) -> Result<Vec<ResolvedRepoSource>, BindingResolveError> {
    let session_branch = conv_branch_name(conversation_id);
    let multi = bindings.len() > 1;
    let mut seen: Vec<&str> = Vec::with_capacity(bindings.len());
    let mut out = Vec::with_capacity(bindings.len());

    for binding in bindings {
        if seen.contains(&binding.remote_name.as_str()) {
            return Err(BindingResolveError::DuplicateRemote(
                binding.remote_name.clone(),
            ));
        }
        seen.push(&binding.remote_name);

        let remote = remotes
            .iter()
            .find(|r| r.name == binding.remote_name)
            .ok_or_else(|| BindingResolveError::UnknownRemote(binding.remote_name.clone()))?;

        let base_ref = match &binding.base {
            BindingBase::Main => remote.default_branch.clone(),
            BindingBase::Branch { name } => name.clone(),
            BindingBase::Commit { sha } => sha.clone(),
        };

        // Merge-back target: non-empty explicit override, else the base branch (a `Branch` fork merges back to its branch), else the remote default
        // Empty `Some("")` is treated as unset so publish cannot target "".
        let merge_target = binding
            .merge_target
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| match &binding.base {
                BindingBase::Branch { name } if !name.trim().is_empty() => name.clone(),
                _ => remote.default_branch.clone(),
            });

        out.push(ResolvedRepoSource {
            name: remote.name.clone(),
            url: remote.url.clone(),
            host: remote.host,
            session_branch: session_branch.clone(),
            base_ref,
            merge_target,
            mount_path: multi.then(|| absolute_mount_path(DEFAULT_WORKSPACE_ROOT, &remote.name)),
        });
    }

    Ok(out)
}

/// Default `.gitignore` seeded into a fresh app workspace (secrets never enter git).
/// Kept in lockstep with the export seed (`.project_id`, `.github_repo`) and local `info/exclude` (`.grok/`).
/// That keeps machine state out of BYO remotes and user commits.
pub const DEFAULT_GITIGNORE: &str = "\
# Seeded by Grok app workspaces. Secrets and machine state never belong in git;
# they live in the env/secret store, not the working tree.

# Secrets / env
.env
.env.*
*.pem
*.key

# Dependencies / package manager state
node_modules/
.venv/
venv/
__pycache__/
.pnp.*

# Build output / caches
dist/
build/
.next/
.turbo/
.cache/
*.log

# OS / editor cruft
.DS_Store
.idea/
.vscode/

# App-workspace machine state (must not enter BYO remotes)
.project_id
.github_repo
.grok/
";

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(name: &str, default_branch: &str) -> ProjectRemote {
        ProjectRemote {
            name: name.to_owned(),
            url: format!("https://git.example.test/acme/{name}"),
            host: RemoteHost::AppGit,
            default_branch: default_branch.to_owned(),
        }
    }

    fn binding(name: &str, base: BindingBase) -> RepoBinding {
        RepoBinding {
            remote_name: name.to_owned(),
            base,
            merge_target: None,
        }
    }

    #[test]
    fn conv_branch_naming_round_trips() {
        let name = conv_branch_name("abc-123");
        assert_eq!("conv/abc-123", name);
        assert!(is_conv_branch(&name));
        assert_eq!(Some("abc-123"), conversation_id_of(&name));

        assert!(!is_conv_branch("main"));
        assert!(!is_conv_branch("conv/")); // empty id is not a conv branch
        assert_eq!(None, conversation_id_of("feature/x"));
    }

    #[test]
    fn single_binding_main_forks_off_default_branch_at_workspace_root() {
        let remotes = [remote("app", "main")];
        let bindings = [binding("app", BindingBase::Main)];

        let sources = resolve_repo_sources("c1", &remotes, &bindings).unwrap();
        assert_eq!(1, sources.len());
        let Some(s) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert_eq!("conv/c1", s.session_branch);
        assert_eq!("main", s.base_ref);
        assert_eq!("main", s.merge_target);
        assert_eq!(None, s.mount_path, "single repo mounts at workspace root");
    }

    #[test]
    fn branch_and_commit_bases_set_fork_ref() {
        let remotes = [remote("app", "main")];

        let from_branch = resolve_repo_sources(
            "c1",
            &remotes,
            &[binding(
                "app",
                BindingBase::Branch {
                    name: "feature/x".to_owned(),
                },
            )],
        )
        .unwrap();
        let Some(from_branch) = from_branch.first() else {
            panic!("expected one source: {from_branch:?}");
        };
        assert_eq!("feature/x", from_branch.base_ref);
        assert_eq!("conv/c1", from_branch.session_branch);
        // A `Branch` fork merges back to its branch by default, not `main`.
        assert_eq!("feature/x", from_branch.merge_target);

        let from_commit = resolve_repo_sources(
            "c1",
            &remotes,
            &[binding(
                "app",
                BindingBase::Commit {
                    sha: "deadbeef".to_owned(),
                },
            )],
        )
        .unwrap();
        let Some(from_commit) = from_commit.first() else {
            panic!("expected one source: {from_commit:?}");
        };
        assert_eq!("deadbeef", from_commit.base_ref);
        // A commit has no branch, so merge-back defaults to the remote default.
        assert_eq!("main", from_commit.merge_target);
    }

    #[test]
    fn explicit_merge_target_overrides_default() {
        let remotes = [remote("app", "main")];
        let sources = resolve_repo_sources(
            "c1",
            &remotes,
            &[RepoBinding {
                remote_name: "app".to_owned(),
                base: BindingBase::Branch {
                    name: "feature/x".to_owned(),
                },
                merge_target: Some("release".to_owned()),
            }],
        )
        .unwrap();
        let Some(source) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert_eq!("release", source.merge_target);
    }

    #[test]
    fn multi_repo_mounts_under_apps_slug() {
        let remotes = [remote("app", "main"), remote("lib", "trunk")];
        let bindings = [
            binding("app", BindingBase::Main),
            binding("lib", BindingBase::Main),
        ];

        let sources = resolve_repo_sources("c9", &remotes, &bindings).unwrap();
        assert_eq!(2, sources.len());
        let [app, lib] = sources.as_slice() else {
            panic!("expected two sources: {sources:?}");
        };
        assert_eq!(Some("/workspace/apps/app".to_owned()), app.mount_path);
        assert_eq!(Some("/workspace/apps/lib".to_owned()), lib.mount_path);
        // The `lib` base resolves to *its* default branch, not app's.
        assert_eq!("trunk", lib.base_ref);
        assert_eq!("trunk", lib.merge_target);
        // Both forks share the one conversation branch.
        assert!(sources.iter().all(|s| s.session_branch == "conv/c9"));
    }

    #[test]
    fn unknown_remote_is_a_hard_error() {
        let remotes = [remote("app", "main")];
        let err = resolve_repo_sources("c1", &remotes, &[binding("ghost", BindingBase::Main)])
            .unwrap_err();
        assert_eq!(BindingResolveError::UnknownRemote("ghost".to_owned()), err);
    }

    #[test]
    fn duplicate_binding_for_same_remote_is_rejected() {
        let remotes = [remote("app", "main")];
        let err = resolve_repo_sources(
            "c1",
            &remotes,
            &[
                binding("app", BindingBase::Main),
                binding(
                    "app",
                    BindingBase::Branch {
                        name: "x".to_owned(),
                    },
                ),
            ],
        )
        .unwrap_err();
        assert_eq!(BindingResolveError::DuplicateRemote("app".to_owned()), err);
    }

    #[test]
    fn empty_binding_set_resolves_to_no_sources() {
        let remotes = [remote("app", "main")];
        assert!(
            resolve_repo_sources("c1", &remotes, &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_merge_target_falls_back_to_base_or_default() {
        let remotes = [remote("app", "main")];
        let sources = resolve_repo_sources(
            "c1",
            &remotes,
            &[RepoBinding {
                remote_name: "app".to_owned(),
                base: BindingBase::Branch {
                    name: "feature/x".to_owned(),
                },
                merge_target: Some("  ".to_owned()),
            }],
        )
        .unwrap();
        let Some(source) = sources.first() else {
            panic!("expected one source: {sources:?}");
        };
        assert_eq!("feature/x", source.merge_target);
    }

    #[test]
    fn mount_helpers_join_workspace_and_slug() {
        assert_eq!("apps/lib", mount_slug("lib"));
        assert_eq!(
            "/workspace/apps/lib",
            absolute_mount_path(DEFAULT_WORKSPACE_ROOT, "lib")
        );
        assert_eq!("/data/apps/lib", absolute_mount_path("/data/", "lib"));
    }
}
