// Modified for Distill by Samuel Fajreldines, 2026.
//! Session synthesis for the load-perf and fork bench tests.
//!
//! This module lives in `distill-shell` (feature `test-support`) rather than `distill-test-support`.
//! Synthesis drives the real `JsonlStorageAdapter`, so the reverse dependency would be circular.

pub mod synth;
