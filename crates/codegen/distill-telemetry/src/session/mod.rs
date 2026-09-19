//! Session-scoped telemetry: emission context, end timers, session metrics,
//! subagent spawn timing, and activity gauges.

pub mod activity;
pub mod session_ctx;
pub mod session_end;
pub mod session_metrics;
pub mod subagent_spawn;
