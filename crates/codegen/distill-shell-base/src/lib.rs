// Modified for Distill by Samuel Fajreldines, 2026.
//! Foundation modules shared by the grok shell crate family.
//! Extracted from `distill-shell` (which re-exports them at their original paths) so they build in parallel and stop rebuilding on shell edits.

#![deny(clippy::indexing_slicing)]

pub mod cpu_profile;
pub mod env;
pub mod util;
