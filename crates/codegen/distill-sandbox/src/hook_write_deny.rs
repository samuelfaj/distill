// Modified for Distill by Samuel Fajreldines, 2026.
//! Grok-owned hook write-deny: plan, identity revalidation, and post-reexec checks.
//! Namespace lockdown is in [`crate::child_net`].

#[cfg(any(target_os = "linux", all(unix, test), all(feature = "enforce", unix),))]
use std::path::Path;
use std::path::PathBuf;

use distill_config::{
    GlobalHookSource, missing_configured_sources, resolve_global_hook_sources,
    resolve_trust_boundary_sources,
};

#[cfg(unix)]
use distill_config::validated_hook_json_files_for_sources;
#[cfg(target_os = "linux")]
use distill_config::{ensure_grok_hook_slots, unique_ancestors_rootward};

use crate::paths::distill_home;
use crate::profiles::ProfileName;

pub fn profile_enforces_hook_write_deny(profile: &ProfileName) -> bool {
    !matches!(profile, ProfileName::Devbox | ProfileName::Off)
}

#[derive(Debug, thiserror::Error)]
pub enum HookWriteDenyError {
    #[error("{0}")]
    Resolve(String),
    #[error(
        "configured absolute hooks-paths target(s) do not exist: {0}. \
             Create them outside the sandbox or remove them from hooks-paths."
    )]
    MissingConfigured(String),
    #[cfg(target_os = "linux")]
    #[error("required hook write-deny path is not effectively read-only: {path}")]
    NotReadOnly { path: PathBuf },
    #[error("cannot verify hook write-deny path {path}: {detail}")]
    VerifyIo { path: PathBuf, detail: String },
    #[cfg(any(target_os = "linux", all(unix, test)))]
    #[error("hook write-deny path identity changed before apply (possible rename race): {path}")]
    IdentityChanged { path: PathBuf },
    #[cfg(any(target_os = "linux", all(unix, test)))]
    #[error("hook write-deny path is a symlink (retargetable): {path}")]
    Symlink { path: PathBuf },
    #[error(
        "protected regular file has hard-link aliases (st_nlink={nlink}): {path}; \
         refuse sandbox rather than leave a writable alias"
    )]
    HardLink { path: PathBuf, nlink: u64 },
    #[cfg(target_os = "linux")]
    #[error("hook directory JSON snapshot changed before apply: {dir}")]
    JsonSnapshotChanged { dir: PathBuf },
}

impl From<distill_config::GlobalHookSourceError> for HookWriteDenyError {
    fn from(e: distill_config::GlobalHookSourceError) -> Self {
        Self::Resolve(e.to_string())
    }
}

#[cfg(any(target_os = "linux", all(unix, test)))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathIdentity {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    pub is_dir: bool,
    /// Regular files must stay `1` (no hard-link aliases).
    pub nlink: u64,
}

/// Captures identity without following symlinks; regular files require `st_nlink == 1`.
#[cfg(any(target_os = "linux", all(unix, test)))]
pub fn capture_path_identity(path: &Path) -> Result<PathIdentity, HookWriteDenyError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|e| HookWriteDenyError::VerifyIo {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    if meta.file_type().is_symlink() {
        return Err(HookWriteDenyError::Symlink {
            path: path.to_path_buf(),
        });
    }
    let is_dir = meta.file_type().is_dir();
    let nlink = meta.nlink();
    if !is_dir && nlink != 1 {
        return Err(HookWriteDenyError::HardLink {
            path: path.to_path_buf(),
            nlink,
        });
    }
    Ok(PathIdentity {
        path: path.to_path_buf(),
        dev: meta.dev(),
        ino: meta.ino(),
        is_dir,
        nlink,
    })
}

#[cfg(any(target_os = "linux", all(unix, test)))]
pub fn revalidate_path_identity(id: &PathIdentity) -> Result<(), HookWriteDenyError> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(&id.path).map_err(|e| HookWriteDenyError::VerifyIo {
        path: id.path.clone(),
        detail: e.to_string(),
    })?;
    if meta.file_type().is_symlink() {
        return Err(HookWriteDenyError::Symlink {
            path: id.path.clone(),
        });
    }
    let is_dir = meta.file_type().is_dir();
    let nlink = meta.nlink();
    if !is_dir && nlink != 1 {
        return Err(HookWriteDenyError::HardLink {
            path: id.path.clone(),
            nlink,
        });
    }
    if meta.dev() != id.dev || meta.ino() != id.ino || is_dir != id.is_dir || nlink != id.nlink {
        return Err(HookWriteDenyError::IdentityChanged {
            path: id.path.clone(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn reject_hardlinked_files(sources: &[GlobalHookSource]) -> Result<(), HookWriteDenyError> {
    use std::os::unix::fs::MetadataExt;
    use distill_config::GlobalHookSourceKind;
    for s in sources {
        let is_file_slot = matches!(
            s.kind,
            GlobalHookSourceKind::RegistryFile
                | GlobalHookSourceKind::ConfiguredSource
                | GlobalHookSourceKind::TrustBoundaryFile
        );
        if !is_file_slot || !s.path.exists() || s.path.is_dir() {
            continue;
        }
        let meta =
            std::fs::symlink_metadata(&s.path).map_err(|e| HookWriteDenyError::VerifyIo {
                path: s.path.clone(),
                detail: e.to_string(),
            })?;
        if meta.file_type().is_file() && meta.nlink() != 1 {
            return Err(HookWriteDenyError::HardLink {
                path: s.path.clone(),
                nlink: meta.nlink(),
            });
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn reject_hardlinked_files(_sources: &[GlobalHookSource]) -> Result<(), HookWriteDenyError> {
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct DirJsonSnapshot {
    pub dir: PathBuf,
    pub files: Vec<PathIdentity>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub struct HookWriteDenyBwrapPlan {
    pub ancestor_rw_binds: Vec<PathBuf>,
    pub leaves: Vec<PathIdentity>,
    pub dir_json_snapshots: Vec<DirJsonSnapshot>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub enum HookWriteDenyPrepare {
    NotRequired,
    Plan(HookWriteDenyBwrapPlan),
}

pub fn resolve_hook_write_deny_snapshot() -> Result<Vec<GlobalHookSource>, HookWriteDenyError> {
    let grok = distill_home();
    let resolved =
        resolve_global_hook_sources(Some(grok.as_path()), /* reject_symlinks */ true)?;
    if let Some(e) = resolved.configured_error {
        return Err(HookWriteDenyError::Resolve(e.to_string()));
    }
    let missing = missing_configured_sources(&resolved.sources);
    if !missing.is_empty() {
        return Err(HookWriteDenyError::MissingConfigured(
            missing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    let mut sources = resolved.sources;
    sources.extend(resolve_trust_boundary_sources(grok.as_path())?);
    reject_hardlinked_files(&sources)?;
    #[cfg(unix)]
    {
        validated_hook_json_files_for_sources(&sources)?;
    }
    Ok(sources)
}

#[cfg(target_os = "linux")]
pub fn prepare_hook_write_deny(
    profile: &ProfileName,
) -> Result<HookWriteDenyPrepare, HookWriteDenyError> {
    if !profile_enforces_hook_write_deny(profile) {
        return Ok(HookWriteDenyPrepare::NotRequired);
    }
    let grok = distill_home();
    ensure_grok_hook_slots(grok.as_path())?;
    let sources = resolve_hook_write_deny_snapshot()?;
    let plan = build_bwrap_plan(&sources)?;
    Ok(HookWriteDenyPrepare::Plan(plan))
}

pub fn profile_hook_write_deny(profile: &ProfileName) -> anyhow::Result<Vec<GlobalHookSource>> {
    if !profile_enforces_hook_write_deny(profile) {
        return Ok(Vec::new());
    }
    resolve_hook_write_deny_snapshot().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Returns the top-level source paths plus the validated hook JSON files directly under each directory source.
#[cfg(target_os = "linux")]
pub fn enforcement_leaf_paths(
    sources: &[GlobalHookSource],
) -> Result<Vec<PathBuf>, HookWriteDenyError> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in sources {
        if seen.insert(s.path.clone()) {
            out.push(s.path.clone());
        }
    }
    for f in validated_hook_json_files_for_sources(sources)? {
        if seen.insert(f.clone()) {
            out.push(f);
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
fn capture_dir_json_snapshot(dir: &Path) -> Result<DirJsonSnapshot, HookWriteDenyError> {
    use distill_config::{list_direct_hook_json_files, validate_direct_hook_json_file};
    let listed = list_direct_hook_json_files(dir).map_err(|e| HookWriteDenyError::VerifyIo {
        path: dir.to_path_buf(),
        detail: e.to_string(),
    })?;
    let mut files = Vec::new();
    for f in listed {
        validate_direct_hook_json_file(&f)?;
        files.push(capture_path_identity(&f)?);
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DirJsonSnapshot {
        dir: dir.to_path_buf(),
        files,
    })
}

#[cfg(target_os = "linux")]
pub fn build_bwrap_plan(
    sources: &[GlobalHookSource],
) -> Result<HookWriteDenyBwrapPlan, HookWriteDenyError> {
    let mut leaves = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut dir_json_snapshots = Vec::new();

    for src in sources {
        if !src.path.exists() {
            return Err(HookWriteDenyError::Resolve(format!(
                "required hook write-deny path is missing: {}",
                src.path.display()
            )));
        }
        if seen.insert(src.path.clone()) {
            leaves.push(capture_path_identity(&src.path)?);
        }
        if src.is_dir() && src.path.is_dir() {
            let snap = capture_dir_json_snapshot(&src.path)?;
            for f in &snap.files {
                if seen.insert(f.path.clone()) {
                    leaves.push(f.clone());
                }
            }
            dir_json_snapshots.push(snap);
        }
    }

    let leaf_paths: Vec<PathBuf> = leaves.iter().map(|l| l.path.clone()).collect();
    let ancestor_rw_binds = unique_ancestors_rootward(sources)
        .into_iter()
        .filter(|a| !leaf_paths.iter().any(|l| l == a))
        .collect();
    Ok(HookWriteDenyBwrapPlan {
        ancestor_rw_binds,
        leaves,
        dir_json_snapshots,
    })
}

#[cfg(target_os = "linux")]
pub fn revalidate_plan(plan: &HookWriteDenyBwrapPlan) -> Result<(), HookWriteDenyError> {
    for leaf in &plan.leaves {
        revalidate_path_identity(leaf)?;
    }
    for snap in &plan.dir_json_snapshots {
        let now = capture_dir_json_snapshot(&snap.dir)?;
        if now.files.len() != snap.files.len() {
            return Err(HookWriteDenyError::JsonSnapshotChanged {
                dir: snap.dir.clone(),
            });
        }
        for (a, b) in snap.files.iter().zip(now.files.iter()) {
            if a.path != b.path || a.dev != b.dev || a.ino != b.ino || a.nlink != b.nlink {
                return Err(HookWriteDenyError::JsonSnapshotChanged {
                    dir: snap.dir.clone(),
                });
            }
        }
    }
    for anc in &plan.ancestor_rw_binds {
        let meta = std::fs::symlink_metadata(anc).map_err(|e| HookWriteDenyError::VerifyIo {
            path: anc.clone(),
            detail: e.to_string(),
        })?;
        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
            return Err(HookWriteDenyError::IdentityChanged { path: anc.clone() });
        }
        if !anc.exists() {
            return Err(HookWriteDenyError::Resolve(format!(
                "required ancestor for hook write-deny is missing: {}",
                anc.display()
            )));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn append_hook_plan_binds(
    cmd: &mut std::process::Command,
    plan: &HookWriteDenyBwrapPlan,
) -> Result<(), HookWriteDenyError> {
    revalidate_plan(plan)?;
    for anc in &plan.ancestor_rw_binds {
        cmd.arg("--bind").arg(anc).arg(anc);
    }
    for leaf in &plan.leaves {
        cmd.arg("--ro-bind").arg(&leaf.path).arg(&leaf.path);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn path_is_effectively_readonly(path: &Path) -> Result<bool, HookWriteDenyError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| HookWriteDenyError::VerifyIo {
            path: path.to_path_buf(),
            detail: "path contains interior NUL".into(),
        })?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(HookWriteDenyError::VerifyIo {
            path: path.to_path_buf(),
            detail: err.to_string(),
        });
    }
    Ok(buf.f_flag & libc::ST_RDONLY != 0)
}

#[cfg(target_os = "linux")]
pub fn verify_required_hook_write_denies(paths: &[PathBuf]) -> Result<(), HookWriteDenyError> {
    for path in paths {
        if !path_is_effectively_readonly(path)? {
            return Err(HookWriteDenyError::NotReadOnly { path: path.clone() });
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_namespace_lockdown() -> Result<(), String> {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            // SAFETY: after bwrap re-exec / at apply; TSYNC covers all threads.
            unsafe { crate::child_net::install_namespace_lockdown_filter() }
                .map_err(|e| format!("namespace lockdown seccomp failed: {e}"))
        })
        .clone()
}

#[cfg(target_os = "linux")]
pub fn verify_hook_write_deny_enforced() -> Result<(), String> {
    ensure_namespace_lockdown()?;
    let sources = resolve_hook_write_deny_snapshot().map_err(|e| e.to_string())?;
    let paths = enforcement_leaf_paths(&sources).map_err(|e| e.to_string())?;
    verify_required_hook_write_denies(&paths).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn verify_hook_write_deny_enforced() -> Result<(), String> {
    Ok(())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
pub fn maybe_install_namespace_lockdown_inside_bwrap(
    profile: &ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    let protects_mounts = crate::requires_hook_write_deny(profile, workspace)
        || crate::requires_read_deny(profile, workspace)
        || crate::requires_data_write_deny(profile, workspace);
    if protects_mounts && crate::is_inside_bwrap() {
        ensure_namespace_lockdown()?;
    }
    Ok(())
}

#[cfg(all(feature = "enforce", unix, not(target_os = "linux")))]
pub fn maybe_install_namespace_lockdown_inside_bwrap(
    _profile: &ProfileName,
    _workspace: &Path,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "hook_write_deny_tests.rs"]
mod tests;
