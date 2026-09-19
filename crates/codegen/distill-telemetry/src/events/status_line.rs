//! Status-line product telemetry events.

use serde::Serialize;

/// Once per session. Carries no `command` string or script output.
#[derive(Serialize)]
pub struct StatusLineConfigured {
    /// `unset` when the config named no mode, which is adoption's denominator.
    pub kind: &'static str,
    /// Always `false` once the user wrote `type = "disabled"`, and reported even by a client that draws no row.
    pub row_shows_a_problem: bool,
    pub items: String,
    pub custom_items: bool,
}

/// How the status line fared, at shutdown, for every session that enabled it.
#[derive(Serialize)]
pub struct StatusLineHealth {
    pub kind: &'static str,
    /// A run's error text counts, a config diagnostic does not, so `false` can still mean a bar that showed one all session.
    pub had_content: bool,
    pub runs_ok: u64,
    /// Shown on the row as `[status line: …]`.
    pub runs_failed: u64,
    pub runs_timed_out: u64,
    /// Given up on; counted again under its outcome if it ever lands.
    pub runs_abandoned: u64,
    pub slowest_ms: u64,
}
