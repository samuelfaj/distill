//! Shared types for Grove worktree enablement and status.
use serde::{Deserialize, Serialize};
use std::time::Duration;
/// Status capability marking a daemon new enough to abort an in-flight create.
pub const CAP_CANCEL_WORKTREE_CREATE: &str = "cancel_worktree_create";
/// Status capability: this daemon forks a second attach from a Grove parent backing.
pub const CAP_FORK_FROM_BACKING: &str = "fork_from_backing";
/// Explicit Grove enablement for [`crate::WorktreeBuilder`].
/// The library never reads pager config; callers resolve flags and pass the result.
#[derive(Clone, Debug)]
pub struct NfsWorktreeOpts {
    pub enabled: bool,
    /// Endpoint override. `None` uses the platform default.
    pub control_sock: Option<std::path::PathBuf>,
    /// Daemon state directory. `None` uses the platform default.
    pub data_dir: Option<std::path::PathBuf>,
    /// Daemon runtime directory. `None` uses the directory beside the endpoint.
    pub runtime_dir: Option<std::path::PathBuf>,
    pub ping_timeout: Duration,
    pub create_timeout: Duration,
    pub query_timeout: Duration,
    pub query_interval: Duration,
}
impl Default for NfsWorktreeOpts {
    fn default() -> Self {
        Self {
            enabled: false,
            control_sock: None,
            data_dir: None,
            runtime_dir: None,
            ping_timeout: Duration::from_millis(250),
            create_timeout: Duration::from_secs(180),
            query_timeout: Duration::from_secs(30),
            query_interval: Duration::from_millis(50),
        }
    }
}
/// Typed decline from a Grove create. Copy fallback must not run for the
/// variants [`grove_hard_fail`] maps.
#[derive(Debug)]
#[allow(dead_code)]
pub enum NfsTryError {
    StorageFull,
    InFlight { phase: String },
    IdentityConflict(String),
    DestStillMounted,
    Other(anyhow::Error),
}
impl From<anyhow::Error> for NfsTryError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}
impl std::fmt::Display for NfsTryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StorageFull => f.write_str(crate::OUT_OF_DISK_CONTEXT),
            Self::InFlight { phase } => {
                write!(
                    f,
                    "create declined (still in flight, phase={phase}); not falling back"
                )
            }
            Self::IdentityConflict(msg) => write!(f, "{msg}; not falling back to copy"),
            Self::DestStillMounted => {
                f.write_str("dest still mounted after adopt; not falling back to copy")
            }
            Self::Other(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for NfsTryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Other(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetachReply {
    pub phase: String,
    pub same_device: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SalvageReply {
    pub virtual_remaining: Vec<String>,
    pub gitdir_copied: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanArtifactsReply {
    pub purged_entries: u64,
    pub no_escapes: bool,
}
#[derive(Debug, Clone)]
pub struct NfsStatusView {
    pub hydration_percent: Option<f64>,
    pub raw: Option<serde_json::Value>,
    pub port: Option<u16>,
    pub mount_id: Option<String>,
    pub transport: Option<String>,
}
impl NfsStatusView {
    fn sole_mount(&self) -> Option<&serde_json::Value> {
        let mounts = self.raw.as_ref()?.get("mounts")?.as_array()?;
        let [mount] = mounts.as_slice() else {
            return None;
        };
        Some(mount)
    }
    /// True when status names exactly one local worktree mount.
    #[must_use]
    pub fn is_linked_local_view(&self) -> bool {
        self.sole_mount().is_some_and(|m| {
            m.get("kind").and_then(|v| v.as_str()) == Some("worktree")
                && m.get("source_mode").and_then(|v| v.as_str()) == Some("local")
        })
    }
    /// Exactly one mount this daemon will send to the fork arm. Older daemons omit the bit.
    #[must_use]
    pub fn is_forkable(&self) -> bool {
        self.sole_mount()
            .and_then(|m| m.get("forkable"))
            .and_then(|v| v.as_bool())
            == Some(true)
    }
    /// Same predicate `run_grove_arm` uses to send CreateWorktree.
    #[must_use]
    pub fn can_fork(&self) -> bool {
        self.has_capability(CAP_FORK_FROM_BACKING) && self.is_forkable()
    }
    /// Linked local view or cap+forkable. Facades issue one Status RPC and call this.
    #[must_use]
    pub fn keeps_grove_create(&self) -> bool {
        self.is_linked_local_view() || self.can_fork()
    }
    #[must_use]
    pub fn has_capability(&self, cap: &str) -> bool {
        self.raw
            .as_ref()
            .and_then(|raw| raw.get("capabilities"))
            .and_then(|c| c.as_array())
            .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some(cap)))
    }
    /// Transport of the one registered mount, when status names exactly one.
    #[must_use]
    pub fn mount_transport(&self) -> Option<&str> {
        if let Some(t) = self.transport.as_deref() {
            return Some(t);
        }
        self.sole_mount()?
            .get("nfs_transport")
            .and_then(|v| v.as_str())
    }
}
#[derive(Debug, Clone)]
pub struct NfsAdopted {
    pub dest: std::path::PathBuf,
    pub mount_id: String,
    pub port: u16,
    pub transport: String,
}
#[derive(Debug)]
pub enum NfsCreateDecision {
    Adopted(NfsAdopted),
    /// Typed decline, unreachable endpoint, or a terminal abort.
    Fallback,
}
/// Typed hard-fail that blocks copy fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroveHardFail {
    InFlight,
    StorageFull,
    IdentityConflict,
    DestStillMounted,
}
/// Classify a builder `Err` from the typed [`NfsTryError`] in the chain.
#[must_use]
pub fn grove_hard_fail(err: &anyhow::Error) -> Option<GroveHardFail> {
    for c in err.chain() {
        if let Some(nfs) = c.downcast_ref::<NfsTryError>() {
            return match nfs {
                NfsTryError::InFlight { .. } => Some(GroveHardFail::InFlight),
                NfsTryError::StorageFull => Some(GroveHardFail::StorageFull),
                NfsTryError::IdentityConflict(_) => Some(GroveHardFail::IdentityConflict),
                NfsTryError::DestStillMounted => Some(GroveHardFail::DestStillMounted),
                NfsTryError::Other(_) => None,
            };
        }
        if c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::StorageFull)
        {
            return Some(GroveHardFail::StorageFull);
        }
    }
    None
}
/// True when dispatch must not fall through to the copy engine.
pub(crate) fn nfs_error_blocks_fallback(err: &anyhow::Error) -> bool {
    grove_hard_fail(err).is_some()
}
/// Grade a daemon from its advertised Status capabilities.
/// `None` is a Status RPC that failed, which grades `unknown`.
#[must_use]
pub fn daemon_capability_class(capabilities: Option<&[String]>) -> &'static str {
    match capabilities {
        Some(caps) if caps.iter().any(|c| c == CAP_CANCEL_WORKTREE_CREATE) => "current",
        Some(_) => "old",
        None => "unknown",
    }
}
pub(crate) fn grove_resolved_strategy(transport: &str) -> &'static str {
    if transport.eq_ignore_ascii_case("fuse") {
        crate::worktree::STRATEGY_GROVE_FUSE
    } else if transport.eq_ignore_ascii_case("projfs") {
        crate::worktree::STRATEGY_GROVE_PROJFS
    } else {
        crate::worktree::STRATEGY_GROVE_NFS
    }
}
/// Inverse of [`grove_resolved_strategy`]: the wire transport a stored
/// `creation_mode` implies (`nfs` for the legacy `nfs` spelling too).
#[cfg_attr(not(feature = "metadata"), allow(dead_code))]
pub(crate) fn transport_for_strategy(strategy: &str) -> &'static str {
    match strategy {
        crate::worktree::STRATEGY_GROVE_FUSE => "fuse",
        crate::worktree::STRATEGY_GROVE_PROJFS => "projfs",
        _ => "nfs",
    }
}
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn grove_transport_name(transport: &str) -> &'static str {
    if transport.eq_ignore_ascii_case("fuse") {
        "fuse"
    } else if transport.eq_ignore_ascii_case("projfs") {
        "projfs"
    } else {
        "nfs"
    }
}
/// Transport written when the daemon omits mount info: the platform default.
#[must_use]
pub(crate) fn default_grove_transport() -> &'static str {
    if cfg!(target_os = "linux") {
        "fuse"
    } else if cfg!(windows) {
        "projfs"
    } else {
        "nfs"
    }
}
/// `creation_mode` for a rediscovered identity with no live mount fstype.
#[must_use]
#[cfg_attr(not(feature = "metadata"), allow(dead_code))]
pub(crate) fn default_grove_creation_mode() -> &'static str {
    grove_resolved_strategy(default_grove_transport())
}
/// Worktree ids name on-disk directories. Reject empty ids, a leading dot,
/// and path separators.
pub(crate) fn is_safe_worktree_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('\0')
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strategy_follows_transport_name() {
        assert_eq!(grove_resolved_strategy("fuse"), "grove-fuse");
        assert_eq!(grove_resolved_strategy("nfs"), "grove-nfs");
        assert_eq!(grove_resolved_strategy("PROJFS"), "grove-projfs");
        assert_eq!(grove_resolved_strategy(""), "grove-nfs");
        assert_eq!(grove_transport_name("projfs"), "projfs");
        assert_eq!(grove_transport_name("weird"), "nfs");
        for strategy in ["grove-fuse", "grove-nfs", "grove-projfs", "nfs"] {
            assert_eq!(
                grove_resolved_strategy(transport_for_strategy(strategy)),
                if strategy == "nfs" {
                    "grove-nfs"
                } else {
                    strategy
                }
            );
        }
    }
    #[test]
    fn platform_default_transport_is_the_daemon_default() {
        let expected = if cfg!(target_os = "linux") {
            "fuse"
        } else if cfg!(windows) {
            "projfs"
        } else {
            "nfs"
        };
        assert_eq!(default_grove_transport(), expected);
        assert!(crate::worktree::is_grove_strategy(
            default_grove_creation_mode()
        ));
    }
    #[test]
    fn status_view_transport_falls_back_to_the_mount_row() {
        let v = NfsStatusView {
            hydration_percent: None,
            raw: Some(serde_json::json!({
                "mounts":[{"kind":"worktree","nfs_transport":"projfs"}]
            })),
            port: None,
            mount_id: None,
            transport: None,
        };
        assert_eq!(v.mount_transport(), Some("projfs"));
        let two = NfsStatusView {
            raw: Some(serde_json::json!({"mounts":[{}, {}]})),
            ..v.clone()
        };
        assert_eq!(two.mount_transport(), None);
    }
    fn view(raw: serde_json::Value) -> NfsStatusView {
        NfsStatusView {
            hydration_percent: None,
            raw: Some(raw),
            port: None,
            mount_id: None,
            transport: None,
        }
    }
    #[test]
    fn is_forkable_requires_exactly_one_mount_with_the_bit() {
        assert!(!view(serde_json::json!({"mounts":[{"kind":"store"}]})).is_forkable());
        let store = view(serde_json::json!({
            "mounts":[{"kind":"store","forkable":true}]
        }));
        assert!(store.is_forkable());
        let linked = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"local","forkable":true}]
        }));
        assert!(linked.is_forkable());
        let bit_false = view(serde_json::json!({
            "mounts":[{"kind":"worktree","forkable":false}]
        }));
        assert!(!bit_false.is_forkable());
        let two = view(serde_json::json!({
            "mounts":[
                {"kind":"store","forkable":true},
                {"kind":"worktree","forkable":true}
            ]
        }));
        assert!(!two.is_forkable());
        assert!(!view(serde_json::json!({"mounts":[]})).is_forkable());
        assert!(
            !NfsStatusView {
                hydration_percent: None,
                raw: None,
                port: None,
                mount_id: None,
                transport: None,
            }
            .is_forkable()
        );
    }
    #[test]
    fn fork_source_mode_is_not_a_linked_local_view() {
        let fork = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"fork","forkable":true}]
        }));
        assert!(fork.is_forkable());
        assert!(!fork.is_linked_local_view());
    }
    #[test]
    fn keeps_grove_create_is_linked_or_can_fork() {
        let linked = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"local"}]
        }));
        assert!(linked.keeps_grove_create());
        assert!(!linked.can_fork());
        let fork = view(serde_json::json!({
            "capabilities": [CAP_FORK_FROM_BACKING],
            "mounts":[{"kind":"store","forkable":true}]
        }));
        assert!(fork.can_fork());
        assert!(fork.keeps_grove_create());
        assert!(!fork.is_linked_local_view());
        assert!(!view(serde_json::json!({"mounts":[{"kind":"store"}]})).keeps_grove_create());
    }
}
