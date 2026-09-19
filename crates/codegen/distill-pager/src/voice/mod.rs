// Modified for Distill by Samuel Fajreldines, 2026.
//! Voice input: STT pipeline integration and prompt-box dictation.
//!
//! Layering (pager-owned):
//! - **Voice gate**: on by default in GA. Remote `voice_mode_enabled: false` is a kill switch that hides voice everywhere and shows no toast.
//!   An absent remote value falls through to on. `GROK_VOICE_MODE` overrides for local dev; the env var beats the remote flag, which beats the default.
//!   Free/X Basic still get the SuperGrok upsell via tier gates, not this flag.
//! - **Session mode** (`voice_ui_active`): this CLI run only; shows the mic.
//! - **Capture chord**: `/voice` or `Ctrl+Space` start dictation, and Esc or Enter stops it.
//!   `Ctrl+Space` decodes identically on every terminal, so the cheatsheet shows it whenever voice is enabled.
//! - **Hold-to-talk**: on terminals that report key releases (Kitty protocol), hold `Ctrl+Space` to record and release to stop.
//!   Elsewhere the same chord toggles: press starts, press again stops. Handled in `app::event_loop`.
//!
//! Final transcripts are inserted at the current cursor position in the recording target's prompt while capture stays open across speech pauses.
//! The target, captured at start via [`crate::app::app_view::VoiceTarget`], is the agent prompt or the dashboard's input for dispatching a new agent.
//! The user always submits with Enter; nothing is auto-sent.
//! Submit promotes any remaining interim text into the bound prompt, then resets capture completely.

mod auth;
mod handle;

pub use auth::build_voice_auth;
pub use handle::handle_voice_event;
pub(crate) use handle::{
    VoiceInterimCommit, commit_interim_into_prompt, merge_voice_fragment, prompt_blank_for_voice,
    space_voice_fragment,
};
// Re-exported for the composition-root binary, which links the pager library rather than the voice crate
// It intercepts the hidden `__mic-capture` helper mode (macOS captures the mic out of process) and runs at the very top of `main`
pub use distill_voice::maybe_run_capture_subprocess;
