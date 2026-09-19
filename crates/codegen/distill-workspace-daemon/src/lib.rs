// Modified for Distill by Samuel Fajreldines, 2026.
//! How the workspace-server daemonizes itself and supervises its preview-proxy child.
//!
//! Two halves of one concern, both used only by the `distill-workspace-server` binary and never by the `distill-workspace` library:
//!
//! - [`daemonize`] takes the process into its own session (double-fork and `setsid()` on Unix, stdio redirection on Windows).
//!   It also holds the single-instance pidfile lock.
//! - [`preview_supervisor`] runs after that, supervising the in-sandbox preview-proxy child and scraping its loopback control endpoints.
//!
//! Both open every daemon-owned file the same way (`O_NOFOLLOW` and mode `0600`), which is why they live together here.
//!
//! This crate deliberately does not depend on `distill-workspace`.
//! The activity scraper reports through the [`preview_supervisor::PreviewActivitySink`] trait.
//! The binary implements that trait over the workspace `ActivityTracker`.

#![deny(clippy::indexing_slicing)]

pub mod daemonize;
pub mod preview_supervisor;
