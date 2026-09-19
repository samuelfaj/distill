// Modified for Distill by Samuel Fajreldines, 2026.
//! Degraded-startup cause metrics.

use crate::managed_config::LaunchProfile;

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
pub(crate) enum DegradedStartCause {
    #[strum(serialize = "settings fetch failed")]
    FetchFailed,
    #[strum(serialize = "deadline missed")]
    DeadlineMissed,
}

pub(crate) fn record_degraded_start(
    cause: DegradedStartCause,
    profile: LaunchProfile,
    deadline: std::time::Duration,
    wait: std::time::Duration,
) {
    distill_telemetry::unified_log::emit(
        degraded_log_level(profile),
        "startup proceeding without remote settings",
        None,
        Some(serde_json::json!({
            "cause": cause.as_ref(),
            "deadline_ms": deadline.as_millis() as u64,
            "wait_ms": wait.as_millis() as u64,
            "outcome": "settings unavailable at gate time",
        })),
    );
}

pub(crate) fn degraded_log_level(
    profile: LaunchProfile,
) -> distill_telemetry::unified_log::LogLevel {
    use distill_telemetry::unified_log::LogLevel;
    match profile {
        LaunchProfile::Managed => LogLevel::Warn,
        LaunchProfile::Personal => LogLevel::Debug,
    }
}
