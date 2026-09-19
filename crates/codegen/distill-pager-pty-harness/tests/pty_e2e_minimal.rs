//! PTY coverage for experimental minimal mode and its native-scrollback path.
//!
//! All cases are ignored for ordinary Cargo runs; Bazel opts in and caps this process-heavy family at four concurrent libtest workers.

// common.rs is shared by every PTY test binary, so not every item is used here
#[allow(dead_code, unused_imports)]
#[path = "pty_e2e/common.rs"]
mod common;

#[path = "pty_e2e/ansi_scrollback_content_integrity.rs"]
mod ansi_scrollback_content_integrity;
#[path = "pty_e2e/minimal/mod.rs"]
mod minimal;
