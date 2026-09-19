// Modified for Distill by Samuel Fajreldines, 2026.
//! PTY e2e tests for the experimental `--minimal` (scrollback-native) mode.
//!
//! Grouped under one `mod minimal;` so the parent `pty_e2e` module isn't interleaved with a dozen `minimal_*` entries.
//! A full-pager contributor can skip this whole subtree.
//! These exercise the sibling `distill-pager-minimal` crate end-to-end through the built binary (which installs the minimal hook).
//! They therefore live with the rest of the pty suite rather than in a separate crate.
//! Shared harness helpers are reached via `crate::common`, the root of the `pty_e2e` test crate.

mod minimal_cli_screen_mode_does_not_persist;
mod minimal_commits_response_to_scrollback;
mod minimal_commits_thinking_body_to_scrollback;
mod minimal_committed_content_survives_overlay_grow;
mod minimal_continue_reprints_transcript;
mod minimal_ctrl_c_arms_and_quits;
mod minimal_double_esc_committed_queued_prompt_single_render;
mod minimal_esc_mid_turn_hints_ctrl_c;
mod minimal_external_editor_round_trip;
mod minimal_feedback_draft_send;
mod minimal_feedback_modal_opens_and_inline_sends;
mod minimal_feedback_small_terminal_shows_hint;
mod minimal_feedback_write_send_and_dismiss;
mod minimal_flush_left_no_hpad;
mod minimal_help_opens_command_palette;
mod minimal_lookup_commits_one_line_summary;
mod minimal_native_copy_trims_pads;
mod minimal_new_session_keeps_history_and_resets;
mod minimal_parked_plan_commits_to_scrollback;
mod minimal_parked_plan_survives_quit;
mod minimal_queue_indicator_shows_while_running;
mod minimal_quit_resets_bracketed_paste;
mod minimal_resize_preserves_committed_scrollback;
mod minimal_settings_modal_opens_and_closes;
mod minimal_shift_tab_shows_mode_in_info_bar;
mod minimal_short_response_stays_on_screen;
mod minimal_slash_dropdown_dismisses_with_esc;
mod minimal_slash_switches_from_fullscreen;
mod minimal_slash_switches_to_fullscreen;
mod minimal_switch_exec_escape_hatch;
mod minimal_switch_mid_turn_keeps_streaming;
mod minimal_switch_preserves_queued_prompt;
mod minimal_switch_probe_failure_rolls_back;
mod minimal_switch_round_trip_no_reprint;
mod minimal_switch_round_trip_refolds_thinking;
mod minimal_thinking_is_visually_distinct_from_output;
mod minimal_transcript_opens_in_pager;
mod minimal_transcript_pager_restore_no_artifacts;
