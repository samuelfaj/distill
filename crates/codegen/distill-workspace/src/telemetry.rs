//! Stable `tracing` target for workspace telemetry events.
//!
//! Every tool_state / environment telemetry event routes through [`dc_log!`] onto the [`TELEMETRY_TARGET`] target.
//! Only the closed, structured field set below is emitted on this target:
//!
//! - `session_id`, `turn_number`, `phase`
//! - `bytes`, `file_count`, `pending`, `pending_bytes`, `sample_period_secs`
//! - `error_category`, `outcome`
//! - `skip_reason` / `drain_reason`, whose values are only enum `…::as_str()` literals
//! - the drain counters `grace_ms` / `active_at_start` / `pending_at_start` / `producers_at_start`
//!
//! Free-form `reason` / `error` names are deliberately never emitted here.

pub(crate) const TELEMETRY_TARGET: &str = "workspace::telemetry";

/// Emit a telemetry `tracing` event on [`TELEMETRY_TARGET`], pinned so a call site cannot land elsewhere.
/// `$level` is a level-macro name; use only this module's field vocabulary.
macro_rules! dc_log {
    ($level:ident, $($rest:tt)*) => {
        ::tracing::$level!(target: $crate::telemetry::TELEMETRY_TARGET, $($rest)*)
    };
}
pub(crate) use dc_log;
