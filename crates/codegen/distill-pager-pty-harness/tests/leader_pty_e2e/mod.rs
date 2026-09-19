// Modified for Distill by Samuel Fajreldines, 2026.
//! Leader-mode PTY e2e tests, split out of the shared `pty_e2e` target.
//!
//! These cases spawn multi-process leader clusters: each test boots 2-3 full pager processes plus a leader subprocess.
//! Running them interleaved with the ~45-test `pty_e2e` suite is what made `LEADER_TIMEOUT` flake and grow from 60s to 240s.
//! As their own `[[test]]` target they get their own Bazel test action (serialized from the main PTY pool) and can be invoked in isolation:
//!
//! ```bash
//! cargo test -p distill-pager-pty-harness --test leader_pty_e2e -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Binary resolution and harness setup are identical to `pty_e2e` (see that target's `mod.rs`).
//! The shared helpers these tests need live in this directory's `common.rs`.

mod common;

mod campaign_leader_mode_remote_dismiss_on_model_pick;
mod leader_n_clients_shared_session;
mod leader_reattach_cancellation_roundtrips_durable_log;
mod leader_reattach_completion_roundtrips_durable_log;
mod leader_two_clients_shared_session;
