//! CLI-update product telemetry events.

use serde::Serialize;

/// Outcome of one CLI binary install/update attempt.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliUpdateOutcome {
    Success,
    Failed,
}

/// Smoke kinds are post-download `--version` checks; other kinds cover download/activation/misc errors.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliUpdateErrorKind {
    SmokeTimeout,
    SmokeNonzero,
    SmokeSpawn,
    Download,
    Activate,
    Other,
}

/// Wire values match the persisted installer strings; `Other` covers unknown persisted values.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliUpdateInstaller {
    #[serde(rename = "npm")]
    Npm,
    #[serde(rename = "gh-release")]
    GhRelease,
    #[serde(rename = "internal")]
    Internal,
    #[serde(rename = "other")]
    Other,
}

impl CliUpdateInstaller {
    /// Kept next to the wire values above so they cannot drift apart.
    pub fn from_installer_str(installer: &str) -> Self {
        match installer {
            "npm" => Self::Npm,
            "gh-release" => Self::GhRelease,
            "internal" => Self::Internal,
            _ => Self::Other,
        }
    }
}

/// [`CliUpdateTrigger`]'s strum string and `FromStr` are the only rendering; tests pin the round trip with the wire
/// values. Volume caveat: one-shot `grok update` resolves telemetry from disk and env only. So `user_command`
/// under-reports relative to the in-process `leader_converge`; the triggers are not directly comparable.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CliUpdateTrigger {
    /// A human ran `grok update` or accepted an update prompt.
    UserCommand,
    /// TUI/stdio launch check spawned a detached update child.
    AutoBackground,
    /// The leader daemon's hourly in-process converge.
    LeaderConverge,
}

impl std::str::FromStr for CliUpdateTrigger {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user_command" => Ok(Self::UserCommand),
            "auto_background" => Ok(Self::AutoBackground),
            "leader_converge" => Ok(Self::LeaderConverge),
            other => Err(format!("unknown update trigger: {other}")),
        }
    }
}

/// Release channel bucketed to the known set: channel is free-text user config, and recording it verbatim would leak private mirror names.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CliUpdateChannel {
    Stable,
    Alpha,
    Enterprise,
    Other,
}

impl CliUpdateChannel {
    /// Empty means stable, the installers' default (mirrors the updater's `is_stable_channel`).
    pub fn from_channel_str(raw: &str) -> Self {
        match raw.trim() {
            "" | "stable" => Self::Stable,
            "alpha" => Self::Alpha,
            "enterprise" => Self::Enterprise,
            _ => Self::Other,
        }
    }
}

/// One attempt to download and activate a new `grok` binary.
/// Analytics name: `grok-shell-cli_update`.
/// Emitted on failure too; failures carry the typed `error_kind` only (freeform strings leak home paths).
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CliUpdate {
    pub outcome: CliUpdateOutcome,
    pub trigger: CliUpdateTrigger,
    pub from_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_version: Option<String>,
    pub channel: CliUpdateChannel,
    pub installer: CliUpdateInstaller,
    /// `{os}-{arch}` from platform detection; closed by construction.
    pub platform: String,
    pub rosetta: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<CliUpdateErrorKind>,
}
