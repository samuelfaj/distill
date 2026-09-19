//! grok-clone and worktree product telemetry events (utility process).

use serde::Serialize;

/// History shape requested by the client or produced by the daemon.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneHistoryMode {
    Shallow,
    Full,
}

/// Whether `grok clone` finished the mount.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneOutcome {
    Success,
    Failed,
    Cancelled,
}

/// Where a failed `grok clone` stopped.
/// Closed set: no freeform strings.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneFailureStage {
    Configuration,
    Validation,
    Preflight,
    Daemon,
}

/// Local checkout vs remote fetch.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneSourceMode {
    Local,
    Remote,
}

impl CloneSourceMode {
    #[must_use]
    pub fn from_source_mode_str(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

/// Kernel transport of the mount. Linux FUSE is never `nfs`; Windows is `projfs`.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneTransport {
    Fuse,
    Nfs,
    Projfs,
}

impl CloneTransport {
    #[must_use]
    pub fn from_transport_str(s: &str) -> Option<Self> {
        match s {
            "fuse" => Some(Self::Fuse),
            "nfs" => Some(Self::Nfs),
            "projfs" => Some(Self::Projfs),
            _ => None,
        }
    }
}

/// Requested or resolved strategy name.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneStrategy {
    Grove,
    #[serde(rename = "grove-fuse")]
    GroveFuse,
    #[serde(rename = "grove-nfs")]
    GroveNfs,
    #[serde(rename = "grove-projfs")]
    GroveProjfs,
    Copy,
    Overlay,
    Btrfs,
    Git,
    Standalone,
    Linked,
}

impl CloneStrategy {
    #[must_use]
    pub fn from_strategy_str(s: &str) -> Option<Self> {
        match s {
            "grove" => Some(Self::Grove),
            "grove-fuse" => Some(Self::GroveFuse),
            "grove-nfs" => Some(Self::GroveNfs),
            "grove-projfs" => Some(Self::GroveProjfs),
            "copy" => Some(Self::Copy),
            "overlay" => Some(Self::Overlay),
            "btrfs" => Some(Self::Btrfs),
            "git" => Some(Self::Git),
            "standalone" => Some(Self::Standalone),
            "linked" => Some(Self::Linked),
            _ => None,
        }
    }
}

/// Why Grove was not used. Mapped from report copy; never a freeform string.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneFallbackReason {
    RemoteKill,
    RemoteUnavailable,
    CloneDisabled,
    FuseUnavailable,
    /// Windows: `ProjectedFSLib.dll` absent or the build predates 22621.
    ProjfsUnavailable,
    DaemonDown,
    DaemonOld,
    DaemonDeclined,
    SourceIsGrove,
    InFlight,
    Other,
}

/// Last clone phase observed at emit, including client-side `validating`.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClonePhase {
    Validating,
    Materializing,
    Attaching,
    Cancelling,
    Committed,
    Failed,
    Cancelled,
}

impl ClonePhase {
    #[must_use]
    pub fn from_phase_str(s: &str) -> Option<Self> {
        match s {
            "validating" => Some(Self::Validating),
            "materializing" => Some(Self::Materializing),
            "attaching" => Some(Self::Attaching),
            "cancelling" => Some(Self::Cancelling),
            "committed" => Some(Self::Committed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// How a cancelled clone ended, beyond `outcome = cancelled`.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneCancellationDisposition {
    ClientCancelled,
    CancelUnsupported,
    /// Cancel reached the daemon; the client was already gone when it finished.
    CancelAfterDisconnect,
}

/// Daemon Status capability grade.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloneDaemonCapabilityClass {
    Current,
    Old,
    Unknown,
}

impl CloneDaemonCapabilityClass {
    #[must_use]
    pub fn from_class_str(s: &str) -> Option<Self> {
        match s {
            "current" => Some(Self::Current),
            "old" => Some(Self::Old),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// One `grok clone` attempt. Content-free: no URL, dest, store, or repo name.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CloneEnded {
    pub requested_history: CloneHistoryMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_history: Option<CloneHistoryMode>,
    pub duration_ms: u64,
    pub outcome: CloneOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_stage: Option<CloneFailureStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_mode: Option<CloneSourceMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<CloneTransport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_strategy: Option<CloneStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_strategy: Option<CloneStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<CloneFallbackReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_phase: Option<ClonePhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_disposition: Option<CloneCancellationDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_capability_class: Option<CloneDaemonCapabilityClass>,
}

/// Which session-worktree lifecycle produced [`WorktreeEnded`].
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeLifecycle {
    Create,
    /// Git `WorktreeBuilder` forks only. `jj workspace add` is not in this series.
    Fork,
    Resume,
    Restore,
    Isolated,
}

/// One session worktree attempt. Content-free: no dest, source, store, or repo name.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct WorktreeEnded {
    pub lifecycle: WorktreeLifecycle,
    pub duration_ms: u64,
    pub outcome: CloneOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<CloneTransport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_strategy: Option<CloneStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_strategy: Option<CloneStrategy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<CloneFallbackReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancellation_disposition: Option<CloneCancellationDisposition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_capability_class: Option<CloneDaemonCapabilityClass>,
}
