// Modified for Distill by Samuel Fajreldines, 2026.
//! Session lifecycle event structs.
//!
//! The structs moved to `distill-telemetry` in the telemetry crate split.
//! This module re-exports them so the existing import path in shell keeps working.

pub(crate) use distill_telemetry::session_metrics::{
    DoomLoopDetected, DoomLoopRecovery, SessionContextSnapshot, SessionStartKind, SessionStarted,
    TraceUploadAttempted, TraceUploadFailed, TraceUploadSkipped, TraceUploadSucceeded, Turn,
    TurnCompletedLifecycle,
};
