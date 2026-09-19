// Modified for Distill by Samuel Fajreldines, 2026.
//! Startup connect budget: the default agent-ready wait and its env override.

use std::time::Duration;

use distill_shell::managed_config::LaunchProfile;

macro_rules! connect_ui_timeout_env {
    () => {
        "GROK_CONNECT_UI_TIMEOUT_SECS"
    };
}

pub(super) const CONNECT_UI_TIMEOUT_ENV: &str = connect_ui_timeout_env!();
pub(super) const CONNECT_UI_TIMEOUT_TRY_COMMAND: &str =
    concat!(connect_ui_timeout_env!(), "=60 grok");
pub(super) const DEFAULT_CONNECT_UI_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_CONNECT_UI_TIMEOUT_SECS: u64 = 6;
const PERSONAL_CONNECT_UI_SLACK: Duration = Duration::from_secs(2);
// Floors the personal budget above the assert's connect-future sum
// (5.5 settings window + 5 eager-auth + 2 slack = 12.5s). No managed preamble.
const PERSONAL_CONNECT_UI_FLOOR: Duration = Duration::from_millis(12_500);
const _: () = assert!(
    PERSONAL_CONNECT_UI_FLOOR.as_millis()
        >= distill_shell::http::STARTUP_SETTINGS_WAIT_DEADLINE.as_millis()
            + distill_shell::http::STARTUP_AUTH_REFRESH_TIMEOUT.as_millis()
            + PERSONAL_CONNECT_UI_SLACK.as_millis(),
    "the personal connect floor must cover the full personal connect future: settings window, \
     post-gate eager-auth refresh, and slack"
);
// Floors the managed budget above the assert's connect-future sum
// (8+8 preamble + 25 settings window + 5 eager-auth + 4 slack = 50s) with 1s margin.
const MANAGED_CONNECT_UI_FLOOR: Duration = Duration::from_secs(51);
const MANAGED_CONNECT_UI_SLACK: Duration = Duration::from_secs(4);
const _: () = assert!(
    MANAGED_CONNECT_UI_FLOOR.as_millis()
        >= distill_shell::managed_config::SESSION_START_AUTH_DEADLINE.as_millis()
            + distill_shell::managed_config::SESSION_START_SYNC_DEADLINE.as_millis()
            + distill_shell::http::MANAGED_STARTUP_SETTINGS_WAIT_DEADLINE.as_millis()
            + distill_shell::http::STARTUP_AUTH_REFRESH_TIMEOUT.as_millis()
            + MANAGED_CONNECT_UI_SLACK.as_millis(),
    "the managed connect floor must cover the full connect future: the policy preamble \
     (session-start auth + sync), the settings kill-switch window, the post-gate eager-auth refresh, \
     and slack for connect glue"
);

pub(super) fn resolve(env: Option<&str>, profile: LaunchProfile) -> Duration {
    let base = match env.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
        None | Some(0) => DEFAULT_CONNECT_UI_TIMEOUT,
        Some(secs) => Duration::from_secs(secs.max(MIN_CONNECT_UI_TIMEOUT_SECS)),
    };
    match profile {
        LaunchProfile::Managed => base.max(MANAGED_CONNECT_UI_FLOOR),
        LaunchProfile::Personal => base.max(PERSONAL_CONNECT_UI_FLOOR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cases() {
        use LaunchProfile::Personal;
        assert_eq!(resolve(None, Personal), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some(""), Personal), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some(" 45 "), Personal), Duration::from_secs(45));
        assert_eq!(resolve(Some("0"), Personal), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(
            resolve(Some("garbage"), Personal),
            DEFAULT_CONNECT_UI_TIMEOUT
        );
        assert_eq!(resolve(Some("-5"), Personal), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("1e3"), Personal), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("1"), Personal), PERSONAL_CONNECT_UI_FLOOR);
        assert_eq!(resolve(Some("9999"), Personal), Duration::from_secs(9999));
    }

    #[test]
    fn managed_launch_floors_the_budget_above_the_settings_window() {
        use LaunchProfile::Managed;
        // Managed override + floor; the floor-vs-connect-future coupling is enforced
        // by the compile-time asserts above, and Personal resolution by `resolve_cases`.
        assert_eq!(resolve(Some("1"), Managed), MANAGED_CONNECT_UI_FLOOR);
        assert_eq!(resolve(Some("5"), Managed), MANAGED_CONNECT_UI_FLOOR);
        assert_eq!(resolve(Some("9999"), Managed), Duration::from_secs(9999));
    }
}
