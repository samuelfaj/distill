//! Grove artifact-directory redirection events, drained from the daemon's
//! ring over IPC and translated by the workspace host. Every field is a closed
//! enum or a count, never a path, repo name, or user text.
//!
//! The daemon mirrors these types without linking this crate, so the wire
//! fixture tests in `events/mod.rs` and grove's `redirect/events_tests.rs`
//! pin the same JSON literals and the two copies cannot drift.

use serde::{Deserialize, Serialize};

/// Kernel transport of the mount the entry lives on.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectTransport {
    Fuse,
    Nfs,
    Projfs,
}

/// Mechanism the transport applied.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectMechanismKind {
    Bind,
    Symlink,
    Image,
    Junction,
}

/// What the declared entry asked for.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectTypeKind {
    Bind,
    Symlink,
}

/// Which source declared the entry.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectSourceKind {
    User,
    Repo,
    Auto,
}

/// What asked the fixup worker to run.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectTriggerKind {
    Attach,
    KillSwitch,
    IndexPublish,
    DestTreeChanged,
    PurgeContinue,
    Ipc,
}

/// Probe state of the entry before the fixup acted on it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectStateKind {
    Ok,
    OkFallback,
    UnknownMount,
    NotMounted,
    SymlinkMissing,
    SymlinkIncorrect,
    Conflict,
    Busy,
    CapabilityUnavailable,
}

/// Why an applied entry took the fallback mechanism.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectFallbackReason {
    ImageAttachFailed,
    ImageCap,
}

/// Every `busy` and `conflict` reason plus the apply-time errors.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectFailureReason {
    Ebusy,
    SharingViolation,
    DemoteBudget,
    InFlight,
    LiveDir,
    ForeignObject,
    ForeignLink,
    ForeignMount,
    Occupied,
    Overlap,
    ParentIsLink,
    NotIgnored,
    IndexTracked,
    UnattributedMount,
    ConversionFailed,
    ImageTxnPending,
    ImageScanOverflow,
    DestClaimed,
    Eperm,
    AttachTimeout,
    AttachFailed,
    CreateFailed,
    RemountFailed,
    CopyFailed,
    VerifyFailed,
    RepoFileInvalid,
    PurgeFailed,
    IdentityRefused,
    Cancelled,
    Io,
}

/// How existing contents reached the jail.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum ReplicationKind {
    None,
    Clonefile,
    Copy,
    Move,
}

/// What fixup did with a populated plain directory at a configured path.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum OverwriteDisposition {
    Replicated,
    Refused,
    Forced,
}

/// Why a live entry was demoted.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectDemoteReason {
    IndexTracked,
    Ipc,
    Shutdown,
    Cleanup,
    StuckLazy,
    Convert,
    Fixup,
    Del,
    KillSwitch,
}

/// Which enforced cap a limit event names.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum RedirectLimitKind {
    AutoCandidates,
    RepoFileEntries,
    RepoFileBytes,
    UserEntries,
    ImagesPerMount,
    PurgeBudget,
    ImageScanEntries,
}

/// What happened when a cap was hit.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, strum::VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum LimitDisposition {
    Truncated,
    Rejected,
    FallbackSymlink,
    Continued,
    Refused,
}

/// One entry applied by a fixup run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RedirectApplied {
    pub transport: RedirectTransport,
    pub mechanism: RedirectMechanismKind,
    pub kind: RedirectTypeKind,
    pub source: RedirectSourceKind,
    pub trigger: RedirectTriggerKind,
    pub apply_ms: u64,
    pub replication: ReplicationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<RedirectFallbackReason>,
}

/// One entry a fixup run could not bring to `ok`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RedirectFixupFailed {
    pub transport: RedirectTransport,
    pub mechanism: RedirectMechanismKind,
    pub kind: RedirectTypeKind,
    pub source: RedirectSourceKind,
    pub trigger: RedirectTriggerKind,
    pub initial_state: RedirectStateKind,
    pub reason: RedirectFailureReason,
    pub fixup_ms: u64,
}

/// One live entry torn down.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RedirectDemoted {
    pub transport: RedirectTransport,
    pub mechanism: RedirectMechanismKind,
    pub reason: RedirectDemoteReason,
    pub demote_ms: u64,
}

/// A populated plain directory met at a configured path.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RedirectOverwrite {
    pub transport: RedirectTransport,
    pub mechanism: RedirectMechanismKind,
    pub disposition: OverwriteDisposition,
    pub replication: ReplicationKind,
    pub entries_moved: u64,
    pub replicate_ms: u64,
}

/// One cap crossing, once per run per cap.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RedirectLimitHit {
    pub limit_kind: RedirectLimitKind,
    pub limit: u64,
    pub observed: u64,
    pub disposition: LimitDisposition,
}

/// Envelope the daemon ring carries; the tag is the `telemetry_event!` name.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "event")]
pub enum RedirectEvent {
    #[serde(rename = "redirect_applied")]
    Applied(RedirectApplied),
    #[serde(rename = "redirect_fixup_failed")]
    FixupFailed(RedirectFixupFailed),
    #[serde(rename = "redirect_demoted")]
    Demoted(RedirectDemoted),
    #[serde(rename = "redirect_overwrite")]
    Overwrite(RedirectOverwrite),
    #[serde(rename = "redirect_limit_hit")]
    LimitHit(RedirectLimitHit),
}
