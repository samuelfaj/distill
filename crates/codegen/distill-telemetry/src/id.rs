// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::{Once, OnceLock};

/// Overrides the agent ID for this process; nothing is computed or persisted.
const ENV_AGENT_ID: &str = "GROK_AGENT_ID";

static AGENT_ID: OnceLock<String> = OnceLock::new();
static AGENT_INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Returns the stable agent ID: `GROK_AGENT_ID` if set, else the value cached in `$GROK_HOME/agent_id`.
/// Otherwise a machine-derived UUID is computed once and persisted there.
/// The first call in a process may block while the computation runs; [`prefetch_agent_id`] starts it early.
pub fn agent_id() -> String {
    AGENT_ID.get_or_init(load_or_compute_agent_id).clone()
}

/// Reads [`agent_id`] without stalling async workers on the first computation.
pub async fn agent_id_async() -> String {
    if let Some(id) = AGENT_ID.get() {
        return id.clone();
    }
    match tokio::task::spawn_blocking(agent_id).await {
        Ok(id) => id,
        Err(err) => {
            tracing::warn!(error = %err, "agent id blocking task failed; reading inline");
            agent_id()
        }
    }
}

/// Starts the agent ID computation on a background thread so later calls to [`agent_id`] find the value ready, or wait only for the remaining work.
pub fn prefetch_agent_id() {
    static PREFETCH: Once = Once::new();
    PREFETCH.call_once(|| {
        if let Err(err) = std::thread::Builder::new()
            .name("agent-id-fetch".into())
            .spawn(|| {
                agent_id();
            })
        {
            tracing::warn!(error = %err, "failed to spawn the agent id prefetch thread");
        }
    });
}

/// Returns a per-process instance ID: stable across reconnects within the process, new on restart.
pub fn agent_instance_id() -> String {
    AGENT_INSTANCE_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn load_or_compute_agent_id() -> String {
    if let Ok(id) = std::env::var(ENV_AGENT_ID) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }

    let cache_path = distill_config::distill_home().join("agent_id");
    let should_persist = match read_agent_id_cache(&cache_path) {
        Ok(Some(cached)) => {
            tighten_agent_id_cache_perms(&cache_path);
            return cached;
        }
        // Missing, empty, dangling leaf, or invalid UTF-8: persist a repaired id.
        Ok(None) => true,
        Err(e) => {
            let replaceable = cache_error_is_replaceable_leaf(&e, &cache_path);
            tracing::warn!(
                path = %cache_path.display(),
                error = %e,
                replaceable,
                "agent_id cache unreadable; replacing broken leaf when possible"
            );
            replaceable
        }
    };

    let hash = compute_machine_hash();
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, hash.as_bytes()).to_string();
    if should_persist {
        let _ = write_agent_id_cache(&cache_path, &id);
    }
    id
}

/// `Ok(Some)` is a cache hit. `Ok(None)` is missing, empty, or invalid UTF-8.
/// `Err` is a hard read. ELOOP/ENOTDIR/EACCES-through-writable-parent replace
/// the leaf; EIO stays ephemeral.
fn read_agent_id_cache(path: &std::path::Path) -> std::io::Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(cached) => {
                let cached = cached.trim();
                if cached.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(cached.to_string()))
                }
            }
            Err(_) => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Self-cycle / hop-limit / unreadable-but-replaceable leaves can be rewritten
/// under `$GROK_HOME`. EIO must not clobber a file we could not read.
fn cache_error_is_replaceable_leaf(e: &std::io::Error, path: &std::path::Path) -> bool {
    if matches!(
        e.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::IsADirectory
    ) || is_eloop_os_error(e)
        || is_invalid_reparse_os_error(e)
    {
        return true;
    }
    e.kind() == std::io::ErrorKind::PermissionDenied && leaf_replaceable_through_parent(path)
}

/// chmod-000 / unreadable dest: rename through a writable parent still heals
/// the leaf (same as main). EACCES on the parent itself cannot.
fn leaf_replaceable_through_parent(path: &std::path::Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        const W_OK: i32 = 2;
        const X_OK: i32 = 1;
        let cstr = match std::ffi::CString::new(parent.as_os_str().as_bytes()) {
            Ok(c) => c,
            Err(_) => return false,
        };
        unsafe extern "C" {
            fn access(pathname: *const std::ffi::c_char, amode: i32) -> i32;
        }
        // SAFETY: `cstr` is a valid NUL-terminated path; POSIX `access` reads it only.
        unsafe { access(cstr.as_ptr(), W_OK | X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(parent).is_ok()
    }
}

fn is_invalid_reparse_os_error(e: &std::io::Error) -> bool {
    // ERROR_INVALID_REPARSE_DATA (4392): malformed Windows reparse. The leaf
    // cannot be followed; replace it so a UUIDv4 fallback stays stable.
    cfg!(windows) && e.raw_os_error() == Some(4392)
}

fn is_eloop_os_error(e: &std::io::Error) -> bool {
    match e.raw_os_error() {
        Some(40) if cfg!(target_os = "linux") => true,
        Some(62)
            if cfg!(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd"
            )) =>
        {
            true
        }
        Some(1921) if cfg!(windows) => true,
        _ => false,
    }
}

/// - macOS: mid uses unique hardware IDs (serial, UUID, SEID).
/// - Linux: /etc/machine-id is shared across containers from the same base image, so include $HOSTNAME (container/host name) for uniqueness.
/// - Fallback: random UUIDv4 if mid or hostname are unavailable.
fn compute_machine_hash() -> String {
    if cfg!(target_os = "linux") {
        match std::env::var("HOSTNAME") {
            Ok(hostname) if !hostname.is_empty() => {
                let key = format!("agent_id:{hostname}");
                mid::get(&key).unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
            }
            _ => uuid::Uuid::new_v4().to_string(),
        }
    } else {
        mid::get("agent_id").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    }
}

/// Owner-only and atomic: the id is a stable device identifier, and rewriting an older world-readable cache must not keep the loose mode.
fn write_agent_id_cache(path: &std::path::Path, id: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    distill_config::fs_atomic::write_atomically(path, id, Some(0o600))
}

/// Best effort: tightens caches written world-readable by older builds.
fn tighten_agent_id_cache_perms(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
    }

    #[test]
    fn agent_id_cache_written_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        write_agent_id_cache(&path, "test-agent-id-value").expect("write");
        assert_eq!(mode(&path), 0o600, "agent_id cache must be 0o600");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read").trim(),
            "test-agent-id-value"
        );
    }

    #[test]
    fn rewrite_over_loose_perms_cache_lands_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        std::fs::write(&path, "").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_agent_id_cache(&path, "fresh-id").expect("rewrite");
        assert_eq!(mode(&path), 0o600, "rewrite must not inherit loose perms");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "fresh-id");
    }

    #[test]
    fn older_world_readable_cache_is_tightened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        std::fs::write(&path, "legacy-id").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        tighten_agent_id_cache_perms(&path);
        assert_eq!(mode(&path), 0o600, "legacy cache must be tightened on read");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "legacy-id");
    }

    #[test]
    fn dangling_agent_id_symlink_is_replaced_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let victim = dir.path().join("outside").join("victim");
        std::fs::create_dir_all(victim.parent().expect("outside dir")).expect("outside");
        let path = dir.path().join("agent_id");
        std::os::unix::fs::symlink(&victim, &path).expect("dangling");

        write_agent_id_cache(&path, "healed-id").expect("replace link inode");
        assert!(!victim.exists(), "must not create the referent");
        assert!(
            !std::fs::symlink_metadata(&path)
                .expect("slot")
                .file_type()
                .is_symlink(),
            "dangling agent_id must be replaced with a regular cache file"
        );
        assert_eq!("healed-id", std::fs::read_to_string(&path).expect("healed"));
    }

    #[test]
    fn filesystem_loop_cache_error_is_replaceable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        let eloop = if cfg!(target_os = "linux") { 40 } else { 62 };
        assert!(cache_error_is_replaceable_leaf(
            &std::io::Error::from_raw_os_error(eloop),
            &path
        ));
        assert!(cache_error_is_replaceable_leaf(
            &std::io::Error::new(std::io::ErrorKind::InvalidInput, "hop limit"),
            &path
        ));
        assert!(!cache_error_is_replaceable_leaf(
            &std::io::Error::other("eio"),
            &path
        ));
        assert!(cache_error_is_replaceable_leaf(
            &std::io::Error::new(std::io::ErrorKind::NotADirectory, "enotdir"),
            &path
        ));
    }

    #[test]
    fn eacces_leaf_is_replaced_when_parent_is_writable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        std::fs::write(&path, "blocked").expect("seed");
        let eacces = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "eacces");
        assert!(
            cache_error_is_replaceable_leaf(&eacces, &path),
            "writable parent must allow leaf replace"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).expect("000");
        if let Err(err) = std::fs::read(&path) {
            assert_eq!(std::io::ErrorKind::PermissionDenied, err.kind());
            assert!(cache_error_is_replaceable_leaf(&err, &path));
        }
        write_agent_id_cache(&path, "healed-from-eacces").expect("replace");
        assert_eq!(
            Some("healed-from-eacces".to_string()),
            read_agent_id_cache(&path).expect("healed")
        );
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn eacces_parent_is_not_replaceable() {
        let missing_parent = std::path::Path::new("/no/such/grok/agent_id");
        assert!(
            !cache_error_is_replaceable_leaf(
                &std::io::Error::new(std::io::ErrorKind::PermissionDenied, "eacces"),
                missing_parent
            ),
            "EACCES through a missing/unwritable parent must stay ephemeral"
        );
    }

    #[test]
    fn self_cycle_agent_id_is_replaced_with_stable_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        std::os::unix::fs::symlink(&path, &path).expect("self-cycle");
        assert!(
            read_agent_id_cache(&path).is_err(),
            "self-cycle must be a hard read error"
        );
        write_agent_id_cache(&path, "stable-from-loop").expect("replace leaf");
        assert_eq!(
            Some("stable-from-loop".to_string()),
            read_agent_id_cache(&path).expect("healed")
        );
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn invalid_utf8_agent_id_cache_is_marked_for_repair() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent_id");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).expect("invalid utf8");
        assert_eq!(
            None,
            read_agent_id_cache(&path).expect("invalid utf8 is rewrite")
        );
        std::fs::write(&path, "  stable-id  \n").expect("valid");
        assert_eq!(
            Some("stable-id".to_string()),
            read_agent_id_cache(&path).expect("hit")
        );
        assert!(
            read_agent_id_cache(&dir.path().join("missing"))
                .expect("enoent")
                .is_none()
        );
    }
}

/// Coarse gate for features that need a full workspace checkout; external installs leave `XAI_ROOT` and `XAI_USER` unset.
pub fn has_workspace_env_markers() -> bool {
    std::env::var("XAI_ROOT").is_ok() && std::env::var("XAI_USER").is_ok()
}

/// Opt-in special-user gate for telemetry (`GROK_TELEMETRY_SPECIAL_USER`).
pub fn is_special_user() -> bool {
    matches!(
        std::env::var("GROK_TELEMETRY_SPECIAL_USER").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}
