// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-turn prompt latency measurement.
//!
//! Implementation lives in `distill-telemetry::prompt_timing`.
//! This shim keeps `crate::session::prompt_timing::PromptTiming` resolving at the original path so callers don't need to change imports.

pub(crate) use distill_telemetry::prompt_timing::PromptTiming;
