// Modified for Distill by Samuel Fajreldines, 2026.
//! Pure decisions: no I/O, no locks, no clock reads — every input is passed in.

const MANAGED_POLICY_MISSING_MSG: &str = "Managed policy is required for this account but is \
missing or could not be verified, and could not be restored from the server.\nThis check needs \
network access: reconnect and start again. If you can't reconnect, contact your administrator.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedPolicyRefusal {
    Compromised,
    Busy,
    LockUnavailable { home: std::path::PathBuf },
}

impl std::fmt::Display for ManagedPolicyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compromised => f.write_str(MANAGED_POLICY_MISSING_MSG),
            Self::Busy => f.write_str(
                "Managed policy is being updated by another grok process or a \
                 background policy sync in this one, and could not be verified in \
                 time. Retry in a moment.",
            ),
            Self::LockUnavailable { home } => write!(
                f,
                "Managed policy could not be verified: the policy lock file under \
                 {} is not accessible. Fix permissions and start again.",
                home.display()
            ),
        }
    }
}

/// Named fields: adjacent bools would silently transpose.
pub(super) struct GateSnapshot {
    pub(super) managed_principal_present: bool,
    pub(super) policy_compromised: bool,
}

pub(super) fn managed_policy_gate_decision(
    snapshot: GateSnapshot,
) -> Result<(), ManagedPolicyRefusal> {
    if snapshot.managed_principal_present && snapshot.policy_compromised {
        return Err(ManagedPolicyRefusal::Compromised);
    }
    Ok(())
}

pub(super) fn auth_mode(
    has_deployment_key: bool,
    signed_in_team: &std::io::Result<bool>,
) -> distill_telemetry::startup::AuthMode {
    use distill_telemetry::startup::AuthMode;
    match (has_deployment_key, signed_in_team) {
        (true, _) => AuthMode::Deployment,
        (false, Ok(true)) => AuthMode::Team,
        (false, Ok(false)) => AuthMode::Personal,
        (false, Err(_)) => AuthMode::Unknown,
    }
}

pub(super) fn managed_config_enabled_from_layers(
    layers: &crate::config::ConfigLayers,
) -> Option<bool> {
    layers
        .effective_config_base_without_overlay()
        .get("features")?
        .get("managed_config")?
        .as_bool()
}

/// The principal a verified payload binds: `deployment_id`, else `team_id` (server parity).
pub(super) fn served_principal_of(
    payload: &distill_config::signed_policy::SignedPayload,
) -> Option<&str> {
    payload
        .deployment_id
        .as_deref()
        .or(payload.team_id.as_deref())
}

pub(super) fn claim_binds_to(
    claim: &distill_config::signed_policy::ManagedIdentityClaim,
    served_principal: Option<&str>,
) -> bool {
    served_principal == Some(claim.principal.as_str())
}

/// The sandbox write-deny class — all a verified refresh may stage past: Seatbelt/Landlock
/// surface EPERM/EACCES (`PermissionDenied`), a bwrap ro-bind EROFS on open
/// (`ReadOnlyFilesystem`) or EBUSY over the mountpoint (`ResourceBusy`). All else propagates.
pub(super) fn write_failure_is_deny(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::ResourceBusy
    )
}

/// Bundle loading is fail-open, so this error can be its only visible symptom.
pub(super) fn certificate_detail(
    detail: String,
    bundle_env: Option<&str>,
    loaded_roots: usize,
) -> String {
    match bundle_env {
        Some(env) if loaded_roots == 0 => format!(
            "{detail}; {env} is set but no usable roots were loaded from it: check that the file is readable, contains PEM certificates, and is under the size cap"
        ),
        Some(env) => format!("{detail}; {env} is set: verify it includes the issuing root CA"),
        None => detail,
    }
}
