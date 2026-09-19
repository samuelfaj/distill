//! `/voice` toggles dictation: it starts recording now, and Esc or Enter stops (Enter also sends). Nothing is written to `config.toml`.
//!
//! The keyboard chord is **Ctrl+Space** or **F8** (both work; F8 is a fallback where Ctrl+Space is taken, e.g. by macOS input switching).
//! The chord follows `[ui].voice_capture_mode`.
//! `toggle` means press starts and press again stops, like `/voice`; `hold`-to-talk means hold to record and release to stop.
//! `hold` needs a terminal that reports key releases (Kitty protocol) and falls back to toggle elsewhere.
//! The recording banner is the only feedback; there is no toast.
//!
//! Dictation works on the agent screen (into the prompt) and on the dashboard (into the dispatch / new-agent input).
//!
//! **Scope.** Voice mode stays on for the rest of the process; re-open `grok` for a clean slate.

use crate::app::actions::Action;
use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

pub struct VoiceCommand;

impl SlashCommand for VoiceCommand {
    slash_meta! {
        name: "voice",
        usage: "/voice",
        // Dictation targets a prompt box: the agent prompt in a live session, or the dashboard's dispatch (new-agent) input.
        // It is session-scoped (no effect on the welcome screen) but still offered on the dashboard.
        session_scoped: true,
        offered_when_session_less: true,
    }

    fn description(&self) -> &str {
        // Chord is Ctrl+Space or F8
        // Without key releases hold-to-talk is impossible, so the label says "Toggle"
        // With them the mode is configurable (toggle or hold via `voice_capture_mode`), so the label leaves the behavior unspecified
        if crate::app::kitty_releases_reported() {
            "Dictation (Ctrl+Space/F8; Esc/Enter to stop)"
        } else {
            "Toggle dictation (Ctrl+Space/F8; Esc/Enter to stop)"
        }
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        // Toggle, mirroring the voice key: starts dictation, or stops it if already recording (Esc/Enter also stop)
        CommandResult::Action(Action::VoiceToggle)
    }
}
