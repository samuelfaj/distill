// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared constants for the leader-mode PTY e2e tests.
//!
//! Drive/seed helpers live in `distill_pager_pty_harness::flows` (one canonical copy shared with `pty_e2e`); only suite-local constants stay here.

pub(crate) use serde_json::json;
pub(crate) use std::time::{Duration, Instant};
pub(crate) use distill_pager_pty_harness::{
    ContentController, LeaderCluster, MockModel, PtyHarness, inference_request_count, keys,
    oauth_credential_ops, pager_binary, seed_fake_oauth, submit_turn, wait_for_labels_absent,
    wait_for_model_via_new_sessions,
};

/// Default PTY size used by every e2e test (same as `pty_e2e`).
pub(crate) const DEFAULT_ROWS: u16 = 50;

pub(crate) const DEFAULT_COLS: u16 = 120;

/// Substring we wait for on the welcome screen (matches the menu label).
pub(crate) const WELCOME_SCREEN_SENTINEL: &str = "Quit";

/// Prompt sent to the agent in content-driven tests.
pub(crate) const PROMPT: &str = "go";

/// Response sentinel the mock server streams back.
pub(crate) const MOCK_RESPONSE_SENTINEL: &str = "MOCKRESPONSE";

/// Cold leader-client bring-up budget. History: 60s, then 120s, then 240s while these cases ran interleaved with
/// the full `pty_e2e` suite. Each leader case spawns multiple full pager processes. suite-wide contention pushed
/// cold bring-up past two minutes.
pub(crate) const LEADER_TIMEOUT: Duration = Duration::from_secs(240);

/// Streamed-turn deadline in leader mode (same contention rationale).
pub(crate) const STREAM_TIMEOUT: Duration = Duration::from_secs(120);

/// Sentinel for leader-test turn `n`, short enough to never wrap at 120 cols (wrapping would break the exactly-once occurrence counts).
pub(crate) fn turn_sentinel(n: u8) -> String {
    format!("{MOCK_RESPONSE_SENTINEL}_T{n}")
}
