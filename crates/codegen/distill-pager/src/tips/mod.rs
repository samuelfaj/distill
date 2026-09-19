//! Ephemeral tips: one hint line at a time, rendered in the banner rect above the prompt input and cleared after a TTL.
//!
//! Unlike the toast, an ephemeral tip survives typing: only TTL expiry, prompt-box submission, or an explicit clear removes it.
//! A tip that carries a seen-count key stops appearing once `AppView::tip_seen_counts` says it has shown often enough this run.
//! That map is in-memory only, so the counts reset every run.

pub mod clear_detector;
pub mod clipboard_focus;
pub mod ephemeral;
pub mod export_copy;
pub mod plan_nudge;
pub mod render;
pub mod send_now;
pub mod small_screen;
pub mod ssh_wrap;
pub mod word_select;

pub use ephemeral::{DEFAULT_TIP_TICKS, EphemeralTip, EphemeralTipState, tip_row_renderable};
