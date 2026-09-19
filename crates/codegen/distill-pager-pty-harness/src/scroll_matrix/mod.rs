// Modified for Distill by Samuel Fajreldines, 2026.
//! The scroll matrix drives the pager binary in a PTY with `GROK_SCROLL_LOG` pointed at a tempfile.
//! It then validates the pager's flight-recorder JSONL (producer: `distill-pager/src/input/scroll_log.rs`) against gesture invariants.
//!
//! [`runner::run_cell`] executes the per-cell flow.
//! [`session`] spawns the primed pager and the cell's [`gestures`] table is replayed as timed SGR reports.
//! [`log`] parses and groups the recorder JSONL (finalize-synchronized, no fixed sleeps).
//! [`invariants`] judge the [`cells`] row's verdict, xfail-aware, and [`report`] renders the verdicts (table, `report.json`, exit code).
//! Entry points: the curated CI tier (`tests/scroll_matrix_curated.rs`) and the local full sweep (`src/bin/scroll_matrix.rs`).
//!
//! ## No pager dependency
//!
//! The harness reaches the pager **only** through the spawned binary (`PAGER_BINARY` / `env::pager_binary`).
//! This module deliberately re-declares the wire schema instead of importing pager types.
//! [`log::ScrollLogLine`] keeps every always-emitted producer field **required**, so a pager-side rename or removal fails deserialization here.
//! Unknown fields are tolerated so additive producer changes don't break older matrix code.
//! The producer-side twin lives in `distill-pager/src/input/mouse/tests.rs` (a wire-format fixture test asserting the same key set on raw JSON).

pub mod cells;
pub mod gestures;
pub mod invariants;
pub mod log;
pub mod report;
pub mod runner;
pub mod session;

pub use cells::{CELLS, ExpectedProfile, MatrixCell, Tier, curated};
pub use gestures::{GestureId, WheelStep, direction_counts};
pub use invariants::{InvariantId, InvariantResult, check_log_invariant};
pub use log::{
    EVT_FINALIZE, EVT_FLUSH, EVT_STREAM_START, ScrollLogLine, StreamGroup, group_streams,
    parse_jsonl, parse_jsonl_str, wait_for_finalize_count,
};
pub use report::{
    CellReport, CellStatus, InvariantReport, InvariantStatus, exit_code, summary_table,
    write_report_json,
};
pub use runner::run_cell;
pub use session::{
    SessionKind, marker_line, marker_response, marker_screen_row, spawn_marker_session,
    spawn_settled_marker_session, spawn_streaming_marker_session, topmost_visible_marker,
};
