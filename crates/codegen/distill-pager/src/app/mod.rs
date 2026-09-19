// Modified for Distill by Samuel Fajreldines, 2026.
//! Application entry point and terminal management.
//!
//! Submodule overview:
//! - [`actions`] — Action, Effect, TaskResult enums
//! - [`agent`] — AgentSession, AgentId, TurnState (business types)
//! - [`agent_view`] — AgentView (per-agent view-model: input + draw)
//! - [`app_view`] — AppView (root component: input routing + draw)
//! - [`dispatch`] — Action → state mutation + Vec<Effect> (sync, testable)
//! - [`effects`] — Effect → async task spawning
//! - [`acp_handler`] — ACP notification routing
//! - [`event_loop`] — biased tokio::select! loop
pub mod actions;
pub mod agent;
pub mod agent_view;
pub mod app_view;
pub mod bundle;
pub(crate) mod cancel_latency;
pub mod cli;
pub(crate) mod command_catalog;
pub mod consent;
pub(crate) mod deferred_subagent_finishes;
pub use crate::link_opener;
use distill_telemetry::region;
use distill_telemetry::region::Parent;
/// Off-thread full-file syntax highlight upgrade for edit diffs.
pub mod edit_highlight_worker;
/// Off-thread Mermaid diagram render worker (out of process) + per-session cache.
pub mod mermaid_worker;
pub(crate) mod prompt_ack;
pub use distill_prompt_queue as prompt_queue;
mod acp_handler;
mod connect_timeout;
mod csi_filter;
mod dashboard_session_picker;
mod dispatch;
pub mod roster;
pub mod session_startup;
pub(crate) mod session_title_resolve;
pub mod status_blocks;
pub(crate) mod status_line;
mod status_line_policy;
pub mod subagent;
pub mod subscription;
pub(crate) mod worktree_session;
pub(crate) use dispatch::dashboard_stop_readiness;
/// Display-refresh probe + motion cadence + terminal telemetry at startup.
mod display_refresh_startup;
pub(crate) mod effects;
pub(crate) mod error_display;
mod x10_filter;
pub(crate) use effects::{cancel_notification_meta, sanitize_user_error};
mod event_loop;
mod event_loop_stall;
mod exit_timeout;
pub(crate) mod external_editor;
mod foreign_sessions;
mod inline_edit;
#[cfg(all(test, unix))]
mod leader_cluster;
mod modals;
pub(crate) mod mode_switch;
mod mouse;
mod queue_edit;
mod reader_thread;
pub(crate) mod screen_mode_relaunch;
mod session_load_barrier;
mod startup_failure;
use reader_thread::ReaderThread;
pub mod signal_handler;
mod teardown_fence;
mod terminal_restore;
use terminal_restore::{emit_terminal_teardown_sequences, restore_terminal, set_panic_hook};
#[cfg(test)]
pub(crate) mod agent_test_fixtures;
mod turn_completion;
pub(crate) mod workspace_layout;
pub(crate) mod workspace_membership;
pub(crate) mod workspace_sync;
#[cfg(test)]
pub(crate) mod workspace_test_fixtures;
mod xt_filter;
pub(crate) use crate::terminal::{kitty_flags_pushed, kitty_releases_reported};
use anyhow::Context as _;
pub use cli::{
    AgentArgs, AgentCmd, Command, HeadlessArgs, LeaderArgs, LeaderMgmtArgs, LeaderMgmtCommand,
    LeaderTargetArgs, OutputFormat, PagerArgs, ServeArgs, WrapArgs,
};
pub use cli::{WorkspaceMgmtArgs, WorkspaceMgmtCommand, WorkspaceStartArgs};
use crossterm::cursor::{self, SetCursorStyle};
use crossterm::event;
use crossterm::execute;
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, SetTitle};
use distill_shell::util::config;
pub use foreign_sessions::ForeignScanCoordinator;
pub(crate) use foreign_sessions::{
    badge_for_picker_source, foreign_tool_display_label, is_foreign_picker_source,
};
use ratatui::backend::CrosstermBackend;
pub use startup_failure::StartupFailure;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::CancellationToken;
pub(crate) use turn_completion::CANCELLATION_CATEGORY_KEY;
/// Tracks the extra Kitty keyboard layer pushed while the `/gboom` game is open (see [`push_gboom_keyboard_flags`]).
/// Kept separate from the base layer (`terminal::kitty_keyboard`) so teardown pops both, in LIFO order.
static GBOOM_KEYBOARD_PUSHED: AtomicBool = AtomicBool::new(false);
/// While the `/gboom` game owns input, additionally request `REPORT_ALL_KEYS_AS_ESCAPE_CODES` so plain letter keys (WASD) emit release events.
/// Tracking several keys held at once needs those release events.
/// Does nothing unless the Kitty keyboard protocol is active.
pub(crate) fn push_gboom_keyboard_flags(writer: &EscapeWriter) {
    if !kitty_flags_pushed() || GBOOM_KEYBOARD_PUSHED.swap(true, Ordering::AcqRel) {
        return;
    }
    let flags = event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | event::KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
    writer.emit_command(event::PushKeyboardEnhancementFlags(flags));
}
/// Pop the extra keyboard layer pushed by [`push_gboom_keyboard_flags`], via the writer queue.
pub(crate) fn pop_gboom_keyboard_flags(writer: &EscapeWriter) {
    if GBOOM_KEYBOARD_PUSHED.swap(false, Ordering::AcqRel) {
        writer.emit_command(event::PopKeyboardEnhancementFlags);
    }
}
/// Teardown/panic variant of [`pop_gboom_keyboard_flags`]: writes inline because the writer thread is already drained (or being abandoned) on those paths.
/// writer thread is already drained (or being abandoned) on those paths.
fn pop_gboom_keyboard_flags_inline() {
    if GBOOM_KEYBOARD_PUSHED.swap(false, Ordering::AcqRel) {
        distill_shell::util::with_locked_stderr(|stderr| {
            let _ = execute!(stderr, event::PopKeyboardEnhancementFlags);
        });
    }
}
/// Tracks whether mouse capture (the five DEC modes enabled by crossterm `EnableMouseCapture`, plus bracketed paste) is currently active.
pub(crate) static MOUSE_CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);
/// Whether minimal was auto-selected because the terminal leaks mouse reports as raw text (JediTerm/Windows) and the user expressed no preference.
/// Gates the idle-hint "auto-set" note so it never misleads users who chose minimal themselves.
static MINIMAL_AUTO_SET_FOR_MOUSE_LEAK: AtomicBool = AtomicBool::new(false);
/// See [`MINIMAL_AUTO_SET_FOR_MOUSE_LEAK`].
pub fn minimal_auto_set_for_mouse_leak() -> bool {
    MINIMAL_AUTO_SET_FOR_MOUSE_LEAK.load(Ordering::Acquire)
}
/// Set after a `/minimal` re-exec that actually stayed minimal (idle-status cue).
static MINIMAL_SHOW_SWITCH_BACK_TO_FULLSCREEN: AtomicBool = AtomicBool::new(false);
pub fn minimal_show_switch_back_to_fullscreen() -> bool {
    MINIMAL_SHOW_SWITCH_BACK_TO_FULLSCREEN.load(Ordering::Acquire)
}
#[cfg(any(test, feature = "test-support"))]
pub fn set_minimal_show_switch_back_to_fullscreen_for_test(on: bool) {
    MINIMAL_SHOW_SWITCH_BACK_TO_FULLSCREEN.store(on, Ordering::Release);
}
/// Whether startup actually applied a forced cursor style.
/// Teardown (and the panic hook, which can't thread parameters) resets the style only when this is true.
/// Under inherit, `0 q` would clobber a shell-chosen style.
pub(crate) static CURSOR_STYLE_FORCED: AtomicBool = AtomicBool::new(false);
/// The screen the terminal is ACTUALLY on, for teardown paths that cannot thread parameters (panic hook, signal handler, post-loop restore).
/// It is updated eagerly at every screen flip so mid-switch failures tear down correctly.
static CURRENT_SCREEN_MODE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(ScreenMode::INITIAL_U8);
pub(crate) fn set_current_screen_mode(mode: ScreenMode) {
    CURRENT_SCREEN_MODE.store(mode.to_u8(), Ordering::Release);
    signal_handler::set_mode(mode);
}
pub(crate) fn current_screen_mode() -> ScreenMode {
    ScreenMode::from_u8(CURRENT_SCREEN_MODE.load(Ordering::Acquire))
}
/// Exists for code like `AgentView::handle_input` that needs minimal-mode behavior but sits below `AppView` and cannot see `AppView::screen_mode`.
/// Do NOT gate behavior off the styling globals (`modal_window::embedded()`, `scrollbar hidden`, …): those are mode-agnostic render toggles.
/// A future embedded host flipping them must not inherit minimal's key remaps or scrollback writes.
static MINIMAL_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the process runs in minimal (scrollback-native) mode.
/// See [`MINIMAL_MODE_ACTIVE`]; prefer `AppView::screen_mode.is_minimal()` wherever the screen mode is already in reach.
pub(crate) fn minimal_mode_active() -> bool {
    MINIMAL_MODE_ACTIVE.load(Ordering::Acquire)
}
/// Test-only override for [`minimal_mode_active`] (unit tests exercising minimal-gated input paths without a terminal).
/// Save/restore around use: this is process-global state.
#[cfg(test)]
pub(crate) fn set_minimal_mode_active_for_test(on: bool) {
    MINIMAL_MODE_ACTIVE.store(on, Ordering::Release);
}
/// Whether the opt-in mouse-reporting toggle feature is enabled (`[ui] mouse_reporting_toggle` / `GROK_MOUSE_REPORTING_TOGGLE`).
/// Seeded once at startup; gates both the `Ctrl+R` shortcut registration and the `/toggle-mouse-reporting` slash command's visibility/execution.
pub(crate) static MOUSE_REPORTING_TOGGLE_ENABLED: AtomicBool = AtomicBool::new(false);
/// Read the cached opt-in mouse-reporting toggle flag (see [`MOUSE_REPORTING_TOGGLE_ENABLED`]).
/// Set once at startup from layered config.
pub(crate) fn mouse_reporting_toggle_enabled() -> bool {
    MOUSE_REPORTING_TOGGLE_ENABLED.load(Ordering::Acquire)
}
/// Process-global voice gate for view code without an `AppView`.
/// Written only by [`crate::app::app_view::AppView::apply_voice_mode_enabled`].
pub(crate) static VOICE_MODE_ENABLED: AtomicBool = AtomicBool::new(false);
fn dock_flag_in(layer: &toml::Value) -> Option<bool> {
    layer
        .get("features")?
        .get(distill_shell::agent::config::Feature::Dock.key())?
        .as_bool()
}
/// `[features] dock` from merged `requirements.toml`.
pub(crate) fn dock_requirement_pin() -> Option<bool> {
    dock_flag_in(&distill_config::load_merged_requirements()?)
}
/// `[features] dock` from effective config (user + managed).
pub(crate) fn dock_config_value() -> Option<bool> {
    dock_flag_in(&distill_shell::config::load_effective_config().ok()?)
}
/// Registry precedence: pin, `GROK_DOCK` / `GROK_DOCK_V2`, config, remote `dock_enabled`, default off.
pub(crate) fn resolve_dock_enabled(remote: Option<bool>) -> bool {
    use distill_shell::agent::config::{Feature, FeatureSources};
    let mut sources = FeatureSources::from_process_env(Feature::Dock);
    if sources.env.is_none() {
        sources.env = distill_config::env_bool("GROK_DOCK_V2");
    }
    sources.pin = dock_requirement_pin();
    sources.config = dock_config_value();
    sources.remote = remote;
    Feature::Dock.resolve(sources).value
}
fn terminal_theme_flag_in(layer: &toml::Value) -> Option<bool> {
    layer
        .get("features")?
        .get(distill_shell::agent::config::Feature::TerminalTheme.key())?
        .as_bool()
}
/// `[features] terminal_theme` from merged `requirements.toml`.
fn terminal_theme_requirement_pin() -> Option<bool> {
    terminal_theme_flag_in(&distill_config::load_merged_requirements()?)
}
/// `[features] terminal_theme` from effective config (user + managed).
fn terminal_theme_config_value() -> Option<bool> {
    terminal_theme_flag_in(&distill_shell::config::load_effective_config().ok()?)
}
/// Registry precedence: pin, `GROK_TERMINAL_THEME`, config, remote `terminal_theme_enabled`, default off.
pub(crate) fn resolve_terminal_theme_enabled(remote: Option<bool>) -> bool {
    use distill_shell::agent::config::{Feature, FeatureSources};
    let mut sources = FeatureSources::from_process_env(Feature::TerminalTheme);
    sources.pin = terminal_theme_requirement_pin();
    sources.config = terminal_theme_config_value();
    sources.remote = remote;
    Feature::TerminalTheme.resolve(sources).value
}
pub(crate) fn voice_mode_enabled() -> bool {
    VOICE_MODE_ENABLED.load(Ordering::Acquire)
}
/// Test helper for the process-global voice gate.
pub fn set_voice_mode_enabled_for_test(on: bool) {
    VOICE_MODE_ENABLED.store(on, Ordering::Release);
}
/// Process-global gate for the Ctrl+Space / F8 voice chord, for key-routing and view code without an `AppView` (`resolve_action`, the cheatsheet).
/// Defaults ON; seeded at startup from `[ui].voice_keybind_enabled` and updated live by the settings setter.
/// Unlike [`VOICE_MODE_ENABLED`] it only silences the keybinding; `/voice` and the other voice entry points stay up.
pub(crate) static VOICE_KEYBIND_ENABLED: AtomicBool = AtomicBool::new(true);
pub(crate) fn voice_keybind_enabled() -> bool {
    VOICE_KEYBIND_ENABLED.load(Ordering::Acquire)
}
/// Test helper for the process-global voice-keybind gate.
pub fn set_voice_keybind_enabled_for_test(on: bool) {
    VOICE_KEYBIND_ENABLED.store(on, Ordering::Release);
}
fn voice_mode_in(layer: &toml::Value) -> Option<bool> {
    layer
        .get("features")?
        .get(distill_shell::agent::config::Feature::VoiceMode.key())?
        .as_bool()
}
/// `[features] voice_mode` from merged `requirements.toml`.
pub(crate) fn voice_mode_requirement_pin() -> Option<bool> {
    voice_mode_in(&distill_config::load_merged_requirements()?)
}
/// `[features] voice_mode` from effective config (user + managed).
pub(crate) fn voice_mode_config_value() -> Option<bool> {
    voice_mode_in(&distill_shell::config::load_effective_config().ok()?)
}
/// The registry owns the precedence and the default.
/// One rule has no row there: with `is_api_key`, a remote-only off is forced back on.
/// A requirement, env, or config `false` still wins.
pub(crate) fn resolve_voice_mode_enabled(
    requirement: Option<bool>,
    config: Option<bool>,
    remote: Option<bool>,
    is_api_key: bool,
) -> bool {
    use distill_shell::agent::config::{ConfigSource, Feature, FeatureSources};
    let resolved = Feature::VoiceMode.resolve(FeatureSources {
        pin: requirement,
        config,
        remote,
        ..FeatureSources::from_process_env(Feature::VoiceMode)
    });
    if resolved.value {
        return true;
    }
    is_api_key && resolved.source == ConfigSource::Remote
}
/// Resolve from live policy, env, remote, and API-key state.
pub(crate) fn resolve_voice_mode_live(remote: Option<bool>, is_api_key: bool) -> bool {
    resolve_voice_mode_enabled(
        voice_mode_requirement_pin(),
        voice_mode_config_value(),
        remote,
        is_api_key,
    )
}
#[cfg(test)]
mod voice_gate_tests {
    use super::resolve_voice_mode_enabled;
    #[test]
    fn api_key_force_on_over_remote_kill_only() {
        assert!(resolve_voice_mode_enabled(None, None, Some(false), true));
        assert!(!resolve_voice_mode_enabled(None, None, Some(false), false));
    }
    #[test]
    fn policy_false_outranks_api_key_force_on() {
        assert!(!resolve_voice_mode_enabled(
            Some(false),
            Some(true),
            Some(true),
            true
        ));
        assert!(!resolve_voice_mode_enabled(
            None,
            Some(false),
            Some(false),
            true
        ));
    }
}
/// Sticky banner shown while mouse reporting is off, telling the user how to turn it back on.
/// `Ctrl+R` only works from scrollback, so the prompt-focused variant points at `/toggle-mouse-reporting` (which toggles from any pane).
/// The banner is stored in the scrollback form; `AgentView::active_toast_message` swaps to the prompt form at render time when the prompt is focused.
pub(crate) const MOUSE_OFF_HINT_SCROLLBACK: &str =
    "Ctrl+r to enable mouse reporting and restore TUI features";
pub(crate) const MOUSE_OFF_HINT_PROMPT: &str =
    "/toggle-mouse-reporting to enable mouse reporting and restore TUI features";
/// Uses [`distill_ratatui_inline::Terminal`] instead of stock `ratatui::Terminal`: our `flush()` returns a `bool` saying whether any cells changed.
/// This lets [`crate::render::draw::draw_frame`] skip cursor escape sequences on frames with empty diffs (e.g., off-screen animation ticks).
/// Skipping them preserves the cursor blink timer; see [`crate::render::draw`] for details.
pub use crate::render::draw::PagerTerminal;
use crate::render::draw::{EscapeWriter, TermWriter, WriterJoin, WriterSender, WriterSync};
/// Whether the pager uses the alternate screen (fullscreen) or stays inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScreenMode {
    Fullscreen,
    Inline,
    /// Scrollback-native (experimental, `--minimal`): finalized blocks are printed into the terminal's native scrollback via `insert_before`.
    /// This crate keeps only three hooks for it: `crate::minimal_hook`, `crate::minimal_api`, and `AppView::minimal_state`.
    /// If you don't work on minimal, treat this variant as opaque; the fullscreen/inline paths are unaffected.
    Minimal,
}
impl ScreenMode {
    const INITIAL_U8: u8 = 1;
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Fullscreen => 0,
            Self::Inline => 1,
            Self::Minimal => 2,
        }
    }
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Fullscreen,
            2 => Self::Minimal,
            _ => Self::Inline,
        }
    }
    pub(crate) fn is_fullscreen(self) -> bool {
        matches!(self, Self::Fullscreen)
    }
    /// Whether this is the experimental scrollback-native minimal mode.
    pub(crate) fn is_minimal(self) -> bool {
        matches!(self, Self::Minimal)
    }
    /// Stable wire label for the `_meta.screenMode` prompt-telemetry field (headless sends `"headless"`).
    /// Values are pinned by the telemetry allowlist (`distill-telemetry`'s `KNOWN_SCREEN_MODES`).
    /// Renaming one silently collapses it to `"other"` on the external stream.
    pub(crate) fn meta_label(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Inline => "inline",
            Self::Minimal => "minimal",
        }
    }
}
/// The rest of startup (and any future contributor) then never has to sprinkle `is_minimal()` checks through `run`.
/// All of these globals do nothing outside minimal (they default to the full-TUI behavior).
/// Calling this for every mode is therefore safe and keeps a single source of truth for the effective mode.
fn apply_screen_mode_globals(screen_mode: ScreenMode) {
    let minimal = screen_mode.is_minimal();
    set_current_screen_mode(screen_mode);
    MINIMAL_MODE_ACTIVE.store(minimal, Ordering::Release);
    crate::terminal::image::set_inline_overlay_force_off(minimal);
    crate::views::modal_window::set_embedded(minimal);
    crate::render::scrollbar::set_scrollbars_hidden(minimal);
    crate::theme::cache::set_terminal_native_lock(minimal);
}
/// Startup theme state for the *requested* screen mode, step 1 of the two-phase startup theme handshake (step 2: [`finish_theme_after_probe`]).
/// Must run before `init_terminal`, whose `apply_cursor_color()` reads the state installed here.
fn engage_startup_theme(screen_mode: ScreenMode) {
    if screen_mode.is_minimal() {
        crate::theme::cache::set_terminal_native_lock(true);
    } else {
        let initial_theme = crate::theme::cache::resolve_initial_theme();
        crate::theme::cache::set(initial_theme);
        mode_switch::mark_theme_resolved();
    }
}
/// Step 2 of the startup theme handshake.
/// If a `--minimal` start was downgraded to Inline by `init_terminal`'s probe, resolve the regular theme that [`engage_startup_theme`] skipped.
/// Does nothing otherwise.
fn finish_theme_after_probe(requested_minimal: bool, effective_mode: ScreenMode) {
    if requested_minimal && !effective_mode.is_minimal() {
        let late_theme = crate::theme::cache::resolve_initial_theme_no_osc11();
        crate::theme::cache::set(late_theme);
        crate::theme::apply_cursor_color();
        mode_switch::mark_theme_resolved();
        tracing::info!(?late_theme, "minimal downgrade: resolved regular theme");
    }
}
/// Info about the active session at exit time, used for the resume hint.
///
/// Wrapped in a struct so additional fields (e.g., cwd, model) can be added without changing the return type.
pub(crate) struct ExitInfo {
    pub session_id: String,
    pub minimal: bool,
    /// Session tail the user can take in at a glance; `Some` exactly when it should print.
    /// The decision whether to print lives at the sole construction site, `finish_run`.
    pub summary: Option<ExitSummary>,
}
/// Session tail printed above the resume command on fullscreen quits.
///
/// Invariant: every field is a pre-sanitized single line (built from the `views::session_title` helpers), so the printer only width-truncates.
pub(crate) struct ExitSummary {
    /// Display title: a rename wins over the generated title, which wins over the first prompt.
    pub title: String,
    pub last_prompt: Option<String>,
    /// `None` when the newest prompt is still unanswered.
    pub last_response: Option<String>,
}
/// Seed this process's UI caches from remote settings. `None` restores defaults.
fn seed_remote_ui_caches(remote_settings: Option<&distill_shell::util::config::RemoteSettings>) {
    distill_shell::util::config::cache_remote_auto_mode(
        remote_settings.and_then(|s| s.auto_mode.clone()),
    );
    distill_shell::util::config::cache_remote_prompt_suggestions(
        remote_settings.and_then(|s| s.prompt_suggestions.clone()),
    );
    distill_shell::util::config::set_remote_campaigns_from_settings(remote_settings);
}
/// Resolve leader mode, reporting both why it is off and what turned it off.
///
/// Precedence, highest first: `--no-leader`, `--leader`, eligibility, local config `use_leader`, remote `leader_mode` (release-dist), default off.
/// `requested_confinement` then vetoes leader use when `Some` (in-process tools stay under the OS sandbox) without reclaiming a shared leader.
/// `policy_disable_reason` is `Some("config"|"remote")` only when leader mode is *definitively* off by policy.
/// Never reclaim a leader on an unknown signal.
pub fn resolve_leader_mode<'p>(
    leader_flag: bool,
    no_leader_flag: bool,
    raw_config: &toml::Value,
    _remote_settings: Option<&distill_shell::util::config::RemoteSettings>,
    eligible: bool,
    requested_confinement: Option<&'p str>,
) -> LeaderMode<'p> {
    let (use_leader, policy_disable_reason) = 'policy: {
        if no_leader_flag {
            break 'policy (false, None);
        }
        if leader_flag {
            break 'policy (true, None);
        }
        if !eligible {
            break 'policy (false, None);
        }
        if let Some(v) = config::use_leader_from_toml_opt(raw_config) {
            break 'policy (v, (!v).then_some("config"));
        }
        #[cfg(feature = "release-dist")]
        if let Some(remote_val) = _remote_settings.and_then(|s| s.leader_mode) {
            break 'policy (remote_val, (!remote_val).then_some("remote"));
        }
        (false, None)
    };
    if let Some(profile) = requested_confinement {
        return LeaderMode {
            use_leader: false,
            policy_disable_reason,
            disabled_by_confinement: use_leader.then_some(profile),
        };
    }
    LeaderMode {
        use_leader,
        policy_disable_reason,
        disabled_by_confinement: None,
    }
}
/// Leader mode as resolved, plus the sandbox profile that overrode it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeaderMode<'p> {
    pub use_leader: bool,
    /// `Some` only when leader mode is *definitively* off by policy, which is what licenses reclaiming a leftover leader.
    pub policy_disable_reason: Option<&'static str>,
    /// The profile that turned leader mode off, set only when leader mode was otherwise on (the case worth telling the user about).
    pub disabled_by_confinement: Option<&'p str>,
}
/// The leader-mode decision alone, for callers with nothing to report.
///
/// See [`resolve_leader_mode`] for the precedence chain and the `policy_disable_reason` contract.
pub fn resolve_use_leader(
    leader_flag: bool,
    no_leader_flag: bool,
    raw_config: &toml::Value,
    remote_settings: Option<&distill_shell::util::config::RemoteSettings>,
    eligible: bool,
    requested_confinement: Option<&str>,
) -> (bool, Option<&'static str>) {
    let resolved = resolve_leader_mode(
        leader_flag,
        no_leader_flag,
        raw_config,
        remote_settings,
        eligible,
        requested_confinement,
    );
    (resolved.use_leader, resolved.policy_disable_reason)
}
/// How long the sandbox note stays uncovered before a fullscreen TUI opens over it.
/// Paid only when the note was printed and the screen is about to hide it.
const SANDBOX_NOTICE_LINGER: std::time::Duration = std::time::Duration::from_millis(1_200);
/// Tell the user at startup that the sandbox turned leader mode off.
/// Writes to the dup'd terminal stderr, which survives the TUI's fd-2 redirect (`redirect_native_stderr`).
/// A fullscreen TUI still paints over it, leaving the line to be read on exit.
pub fn warn_leader_disabled_by_sandbox(profile: &str) {
    distill_shell::util::with_locked_stderr(|stderr| {
        print_leader_disabled_by_sandbox(profile, stderr)
    });
}
/// Says only that the profile was *requested*.
/// Enforcement can still fail (`apply_sandbox` warns and continues) while the leader is refused either way.
/// Write errors are dropped; `eprintln!` would panic on a closed stderr.
fn print_leader_disabled_by_sandbox(profile: &str, w: &mut impl Write) {
    let _ = writeln!(
        w,
        "note: sandbox profile '{profile}' was requested, so leader mode is off for this \
         session and tool calls stay in this process instead of the shared leader. \
         Disable the profile at the source that selected it (CLI, env, config, or a \
         managed requirement) to use the leader."
    );
}
/// Startup proceeds without remote settings (`leader_mode`, announcements)
/// after this; the fetch keeps running and the agent boot consumes it.
pub const EARLY_PREFETCH_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
/// First non-blank value of CLI, then env, then config.
/// `None` means nothing was set; `acp::initialize` canonicalizes and applies the default.
fn resolve_hunk_tracker_mode(
    cli: Option<&str>,
    env: Option<&str>,
    config: Option<&str>,
) -> Option<String> {
    [cli, env, config]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_owned)
}
/// A failed connect attempt, classified for telemetry at the point of failure rather than by parsing the error message.
struct ConnectFailure {
    outcome: crate::acp::StartupOutcome,
    error: anyhow::Error,
    timeout_secs: Option<u64>,
    longest_step: Option<crate::acp::StartupPhase>,
}
/// Slice the connect wait so a launch-profile escalation can extend the budget
/// without parking on the original timeout.
const CONNECT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Bound connect so a hung leader/spawn cannot blank-screen forever.
async fn bounded_connect(
    cancel: &CancellationToken,
    timeout: std::time::Duration,
    connect_ui_timeout_env: Option<&str>,
    target: crate::acp::AgentKind,
    attempt: startup_failure::ConnectAttempt,
    timer: &crate::acp::StartupTimer,
    connect: impl std::future::Future<Output = anyhow::Result<crate::acp::AcpConnection>>,
) -> Result<crate::acp::AcpConnection, ConnectFailure> {
    use crate::acp::StartupOutcome;
    let context = || startup_failure::Context {
        target,
        attempt,
        version: distill_version::display_version_with_commit(
            distill_version::full_version(),
            distill_update::channel_label(),
        ),
        log_path: distill_telemetry::unified_log::path(),
    };
    let started = std::time::Instant::now();
    let mut deadline = started + timeout;
    let mut connect = std::pin::pin!(connect);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let slice = remaining.min(CONNECT_POLL_INTERVAL);
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ConnectFailure {
                outcome: StartupOutcome::Cancelled,
                error: anyhow::Error::new(startup_failure::StartupFailure::cancelled(context())),
                timeout_secs: None,
                longest_step: None,
            }),
            connected = connect.as_mut() => {
                return connected.map_err(|error| ConnectFailure {
                    outcome: StartupOutcome::Error,
                    error,
                    timeout_secs: None,
                    longest_step: None,
                });
            }
            () = tokio::time::sleep(slice) => {
                let profile = distill_shell::managed_config::startup_profile();
                let floor = connect_timeout::resolve(connect_ui_timeout_env, profile);
                let escalated = started + floor;
                if escalated > deadline {
                    tracing::info!(
                        timeout_secs = floor.as_secs(),
                        "connect budget extended after launch profile escalated to managed"
                    );
                    deadline = escalated;
                }
            }
        }
    }
    let timings = timer.phase_snapshot();
    let longest_step = timings.longest_step();
    let timeout_secs = timeout.as_secs();
    tracing::error!(
        connect_target = target.label(),
        stuck_in = timings.stuck_in(),
        phases = %timings.summary(),
        timeout_secs,
        "connect timed out"
    );
    Err(ConnectFailure {
        outcome: StartupOutcome::Timeout,
        error: anyhow::Error::new(startup_failure::StartupFailure::timed_out(
            context(),
            timer.elapsed(),
            timings,
        )),
        timeout_secs: Some(
            deadline
                .saturating_duration_since(started)
                .as_secs()
                .max(timeout_secs),
        ),
        longest_step,
    })
}
/// Main entry point: connect to agent, init terminal, run event loop, restore.
/// If a session ID is provided via `--resume` / `--load` / `--continue`, the pager skips the welcome screen and immediately loads that session.
/// The load replays the session's history; sessions not found locally are restored from remote storage.
pub async fn run(
    mut args: PagerArgs,
    bg_update_rx: Option<
        tokio::sync::oneshot::Receiver<Option<distill_update::auto_update::UpdateAvailable>>,
    >,
) -> anyhow::Result<bool> {
    let screen_mode_override = screen_mode_relaunch::take_screen_mode_env_override();
    let cancel = CancellationToken::new();
    let startup_start = std::time::Instant::now();
    let raw_config = distill_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    let (grok_com_config, proxy_base_url) =
        match distill_shell::agent::config::Config::new_from_toml_cfg(&raw_config) {
            Ok(c) => (c.grok_com_config, c.endpoints.proxy_url()),
            Err(e) => {
                tracing::warn!(error = %e, "failed to parse config for auth refresh, using defaults");
                (
                    distill_login::GrokComConfig::default(),
                    distill_shell::agent::config::EndpointsConfig::default().proxy_url(),
                )
            }
        };
    if let distill_login::PreTuiLoginOutcome::SignedIn(auth) =
        distill_login::maybe_run_pre_tui_external_login(
            &grok_com_config,
            proxy_base_url.clone(),
            args.force_login,
            io::stdin().is_terminal(),
        )
        .await?
    {
        distill_shell::agent::init::apply_post_login_config(*auth).await?;
        args.force_login = false;
    }
    distill_tty_utils::redirect_native_stderr();
    let refreshed_auth = tokio::time::timeout(
        distill_shell::http::STARTUP_AUTH_REFRESH_TIMEOUT,
        distill_login::try_ensure_fresh_auth(&grok_com_config, proxy_base_url),
    )
    .await
    .unwrap_or(None);
    let settings_query = distill_shell::agent::remote_config::settings_get::SettingsQuery::resolve(
        refreshed_auth,
        Some(grok_com_config.clone()),
    );
    let had_prefetch =
        distill_shell::agent::remote_config::settings_get::is_eligible(&settings_query);
    if had_prefetch {
        distill_shell::agent::remote_config::settings_get::warm_startup_settings(
            settings_query.clone(),
        );
    }
    distill_shell::agent::mvp_agent::warm_async_http_client();
    tokio::task::spawn_blocking(|| {});
    if let Ok(cwd) = std::env::current_dir() {
        crate::git_info::populate_from_cwd_async(cwd);
    }
    let prefetch_wait_started = std::time::Instant::now();
    let remote_settings = if had_prefetch {
        let _wait_span = region!("startup.prefetch_join_wait", Parent::Inherit);
        let warmed_auth = settings_query.auth().cloned();
        let wait = {
            let _settings = region!(
                "startup.prefetch_join_wait.settings",
                Parent::Explicit(_wait_span.span())
            );
            distill_shell::agent::remote_config::settings_get::await_startup_settings(
                settings_query,
                EARLY_PREFETCH_WAIT,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
        };
        let settings = distill_shell::agent::remote_config::settings_get::consume_wait(
            wait,
            warmed_auth.as_ref(),
            &grok_com_config,
        );
        distill_telemetry::startup::record_prefetch_wait(prefetch_wait_started.elapsed());
        settings
    } else {
        None
    };
    seed_remote_ui_caches(remote_settings.as_ref());
    let raw_config = distill_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    distill_shell::config::cache_standalone_memory_mode(
        distill_shell::config::MemoryConfig::resolve(
            args.experimental_memory,
            args.no_memory,
            &raw_config,
            remote_settings.as_ref(),
        )
        .mode,
    );
    let prefetch_elapsed = startup_start.elapsed();
    let requested_confinement = distill_sandbox::requested_confinement_profile();
    let LeaderMode {
        use_leader,
        policy_disable_reason,
        disabled_by_confinement,
    } = resolve_leader_mode(
        args.leader,
        args.no_leader,
        &raw_config,
        remote_settings.as_ref(),
        true,
        requested_confinement,
    );
    tracing::info!(
        use_leader,
        ?policy_disable_reason,
        sandbox_profile = ?requested_confinement,
        // The other fields cannot distinguish this from leader mode being off already while a sandbox is on
        leader_disabled_by_sandbox = disabled_by_confinement.is_some(),
        prefetch_ms = prefetch_elapsed.as_millis() as u64,
        "pager TUI leader mode resolved"
    );
    if let Some(profile) = disabled_by_confinement {
        warn_leader_disabled_by_sandbox(profile);
    }
    if session_startup::chat_mode_conflicts_with_leader(args.chat(), use_leader) {
        anyhow::bail!("{}", session_startup::CHAT_MODE_LEADER_CONFLICT);
    }
    if args.trust {
        use distill_workspace::folder_trust::{grant_folder_trust, report_cli_trust_grant};
        match std::env::current_dir() {
            Ok(cwd) => report_cli_trust_grant(&grant_folder_trust(&cwd)),
            Err(e) => {
                tracing::warn!(error = %e, "--trust: failed to resolve cwd; folder not trusted");
                eprintln!("error: --trust: failed to resolve cwd; folder not trusted: {e}");
            }
        }
    }
    if let Some(reason) = policy_disable_reason {
        tokio::spawn(distill_shell::leader::kill_stale_reachable_leaders(reason));
    }
    if let Some(err) =
        session_startup::chat_mode_flag_conflict(args.chat(), args.fork_session, args.restore_code)
    {
        anyhow::bail!("{err}");
    }
    #[cfg(feature = "local-workspace")]
    {
        let lw = session_startup::resolve_local_workspace_config(
            args.chat(),
            args.local_workspace(),
            args.local_workspace_attach(),
            args.local_workspace_cwd(),
        )?;
        if let Some(ref cfg) = lw {
            session_startup::emit_local_workspace_startup_ux(cfg)?;
        }
        session_startup::set_active_local_workspace(lw)?;
    }
    let intent = args
        .session_startup_intent()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut materialize_ctx = session_startup::MaterializeCtx::from_pager_args(&args);
    materialize_ctx.restore_progress_on_stdout =
        std::io::IsTerminal::is_terminal(&std::io::stdout());
    let materialized = session_startup::materialize_startup(materialize_ctx, intent).await?;
    if args.chat()
        && let session_startup::MaterializedStartup::Resume { session_id, .. } = &materialized
    {
        let cwd = std::env::current_dir().unwrap_or_default();
        if session_startup::chat_mode_refuses_local_build_load(true, false, session_id, &cwd) {
            anyhow::bail!(
                "{} (session id: {session_id})",
                session_startup::CHAT_MODE_LOCAL_BUILD_REFUSAL
            );
        }
    }
    let mut session_title = match &materialized {
        session_startup::MaterializedStartup::Resume { title, .. }
        | session_startup::MaterializedStartup::Fork {
            parent_title: title,
            ..
        } => title.clone(),
        _ => None,
    };
    let title_lookup_id = match &materialized {
        session_startup::MaterializedStartup::Resume { session_id, .. } => {
            Some(session_id.as_str())
        }
        session_startup::MaterializedStartup::Fork {
            parent_session_id, ..
        } => Some(parent_session_id.as_str()),
        _ => None,
    };
    if session_title.is_none()
        && !args.chat()
        && let Some(id) = title_lookup_id
    {
        let summaries = distill_shell::session::persistence::list_summaries(None).await?;
        if let Some(s) = summaries.iter().find(|s| s.info.id.0.as_ref() == id)
            && let Some(title) = s.display_title_opt()
        {
            session_title = Some(title);
        }
    }
    let session_cwd = match &materialized {
        session_startup::MaterializedStartup::Resume { original_cwd, .. }
        | session_startup::MaterializedStartup::Fork {
            parent_cwd: original_cwd,
            ..
        } => original_cwd.clone(),
        _ => None,
    };
    let env_hunk_tracker_mode = std::env::var("GROK_HUNK_TRACKER").ok();
    let config_hunk_tracker_mode = raw_config
        .get("ui")
        .and_then(|ui| ui.get("hunk_tracker_mode"))
        .and_then(|v| v.as_str());
    let hunk_tracker_mode = resolve_hunk_tracker_mode(
        args.hunk_tracker_mode.as_deref(),
        env_hunk_tracker_mode.as_deref(),
        config_hunk_tracker_mode,
    );
    let remote_permission_mode = remote_settings
        .as_ref()
        .and_then(|s| s.permission_mode.as_deref());
    let launch_yolo = distill_shell::util::config::effective_yolo_for_launch(
        args.yolo,
        args.permission_mode_flag.as_deref(),
        remote_permission_mode,
    );
    let launch_auto = distill_shell::util::config::effective_auto_for_launch(
        args.yolo,
        args.permission_mode_flag.as_deref(),
        remote_permission_mode,
        distill_shell::util::config::default_interactive_permission_mode(),
    );
    let mut connect_flags = crate::acp::ConnectFlags {
        subagents: !args.no_subagents,
        memory_enabled_override: args.memory_enabled_override(),
        memory_override_flag: args.memory_override_flag(),
        disable_web_search: args.disable_web_search,
        todo_gate: args.todo_gate,
        laziness_debug_log: None,
        storage_mode: args.storage_mode.clone(),
        client_identifier: args.client_identifier.clone(),
        hunk_tracker_mode,
        terminal: args.terminal,
        fs_read: args.fs_read,
        fs_write: args.fs_write,
        installer: args.installer.clone(),
        remote_settings: remote_settings.clone(),
        system_prompt_override: args.system_prompt_override.clone(),
        rules: args.rules.clone(),
        reasoning_effort_override: args
            .reasoning_effort
            .as_deref()
            .and_then(distill_shell::sampling::types::parse_canonical_effort_token),
        permission_rules: crate::headless::parse_permission_rules_lenient(
            &args.allow_rules,
            &args.deny_rules,
        ),
        default_yolo_mode: launch_yolo.yolo,
        default_auto_mode: launch_auto && !launch_yolo.yolo,
        status_line: false,
    };
    let mut config_watcher = crate::appearance::ConfigWatcher::start().await?;
    let alt_screen_config_mode = config_watcher.current().alt_screen;
    let term_ctx = crate::terminal::terminal_context();
    let is_control_mode = crate::terminal::detect_tmux_control_mode(term_ctx);
    let alt_screen_wants_fullscreen = crate::terminal::determine_alt_screen_policy(
        args.no_alt_screen,
        alt_screen_config_mode,
        term_ctx,
        is_control_mode,
    );
    let config_screen_mode = raw_config
        .get("ui")
        .and_then(|ui| ui.get("screen_mode"))
        .and_then(|v| v.as_str());
    let auto_minimal_mouse_leak = term_ctx.mouse_reporting_leaks_as_raw_text();
    let explicit_minimal = screen_mode_relaunch::effective_minimal_preference(
        args.minimal,
        args.fullscreen,
        config_screen_mode,
        config_watcher.current().minimal,
    );
    let screen_mode = screen_mode_relaunch::resolve_screen_mode(
        screen_mode_override,
        explicit_minimal.unwrap_or(auto_minimal_mouse_leak),
        alt_screen_wants_fullscreen,
    );
    MINIMAL_AUTO_SET_FOR_MOUSE_LEAK.store(
        screen_mode.is_minimal() && explicit_minimal.is_none() && screen_mode_override.is_none(),
        Ordering::Release,
    );
    let minimal = screen_mode.is_minimal();
    connect_flags.status_line = event_loop::load_initial_ui_config()
        .status_line
        .reserves_a_row();
    let relaunched_into_minimal = screen_mode_override == Some(ScreenMode::Minimal);
    let relaunched_into_fullscreen = screen_mode_override == Some(ScreenMode::Fullscreen);
    tracing::info!(
        use_alt_screen = screen_mode.is_fullscreen(),
        minimal = screen_mode.is_minimal(),
        mouse_capture = !screen_mode.is_minimal(),
        minimal_live_rows = config_watcher.current().minimal_live_rows,
        is_control_mode,
        no_alt_screen_cli = args.no_alt_screen,
        minimal_cli = args.minimal,
        fullscreen_cli = args.fullscreen,
        config_screen_mode = ?config_screen_mode,
        auto_minimal_mouse_leak,
        config_mode = ?alt_screen_config_mode,
        multiplexer = ?term_ctx.multiplexer,
        "resolved fullscreen policy"
    );
    if disabled_by_confinement.is_some() && screen_mode.is_fullscreen() {
        tokio::time::sleep(SANDBOX_NOTICE_LINGER).await;
    }
    crate::theme::cache::set_terminal_theme_enabled(resolve_terminal_theme_enabled(
        remote_settings
            .as_ref()
            .and_then(|s| s.terminal_theme_enabled),
    ));
    engage_startup_theme(screen_mode);
    let minimal_live_rows = config_watcher.current().minimal_live_rows;
    let (frame_tx, writer_sync, writer_event_rx, writer_thread) =
        crate::render::draw::spawn_writer_thread()
            .context("failed to spawn the term-writer thread")?;
    let cursor_blink = event_loop::load_initial_ui_config().cursor_blink;
    let TerminalInit {
        mut terminal,
        screen_mode,
        startup_typeahead,
    } = init_terminal(
        screen_mode,
        minimal_live_rows,
        relaunched_into_minimal,
        frame_tx,
        writer_sync,
        cursor_blink,
    )?;
    MINIMAL_SHOW_SWITCH_BACK_TO_FULLSCREEN.store(
        relaunched_into_minimal && screen_mode.is_minimal(),
        Ordering::Release,
    );
    apply_screen_mode_globals(screen_mode);
    finish_theme_after_probe(minimal, screen_mode);
    if let Some(ref t) = session_title {
        set_terminal_title(t);
    }
    let connect_ui_timeout_env = std::env::var(connect_timeout::CONNECT_UI_TIMEOUT_ENV).ok();
    let connect_ui_timeout = connect_timeout::resolve(
        connect_ui_timeout_env.as_deref(),
        distill_shell::managed_config::startup_profile(),
    );
    if let Some(ref raw) = connect_ui_timeout_env {
        crate::unified_log::write_direct_info(
            "startup connect budget from env",
            Some(serde_json::json!({
                "raw": raw,
                "timeout_secs": connect_ui_timeout.as_secs(),
            })),
        );
    }
    let fallback_flags = use_leader.then(|| connect_flags.clone());
    let primary_target = if use_leader {
        crate::acp::AgentKind::Leader
    } else {
        crate::acp::AgentKind::Embedded
    };
    distill_telemetry::external::init(distill_shell::agent::config::resolve_external_otel_config(
        distill_telemetry::external::config::ExternalClientInfo {
            service_version: distill_version::full_version().to_owned(),
            client_version: distill_version::VERSION.to_owned(),
            app_entrypoint: "tui".to_owned(),
        },
    ));
    let tracing_handle = crate::tracing::init_tracing();
    let pending_startup = distill_telemetry::startup::PendingStartup::new();
    let timer = distill_telemetry::startup::begin(crate::acp::Owner::Client);
    let primary_started = std::time::Instant::now();
    let connect_result = bounded_connect(
        &cancel,
        connect_ui_timeout,
        connect_ui_timeout_env.as_deref(),
        primary_target,
        startup_failure::ConnectAttempt::First,
        &timer,
        async {
            match primary_target {
                crate::acp::AgentKind::Leader => {
                    crate::acp::connect_via_leader(&cancel, connect_flags, &raw_config).await
                }
                crate::acp::AgentKind::Embedded => {
                    crate::acp::connect(&cancel, connect_flags).await
                }
            }
        },
    )
    .await;
    let (connect_result, embedded_fallback, timer, connect_target) = match connect_result {
        Err(f) if primary_target == crate::acp::AgentKind::Leader && !cancel.is_cancelled() => {
            tracing::warn!(error = %f.error, "leader connect failed; falling back to embedded agent");
            timer.emit_telemetry(primary_target, f.outcome, f.timeout_secs, false);
            let flags = fallback_flags.expect("set on the use_leader path");
            let timer = distill_telemetry::startup::begin(crate::acp::Owner::Client);
            let target = crate::acp::AgentKind::Embedded;
            let fallback = bounded_connect(
                &cancel,
                connect_ui_timeout,
                connect_ui_timeout_env.as_deref(),
                target,
                startup_failure::ConnectAttempt::AfterFallback(startup_failure::EarlierAttempt {
                    target: primary_target,
                    wait: primary_started.elapsed(),
                    outcome: f.outcome,
                    longest_step: f.longest_step,
                }),
                &timer,
                async { crate::acp::connect(&cancel, flags).await },
            )
            .await;
            (fallback, true, timer, target)
        }
        other => (other, false, timer, primary_target),
    };
    let mut connection = match connect_result {
        Ok(conn) => {
            tracing::info!(
                elapsed_ms = startup_start.elapsed().as_millis() as u64,
                use_leader = use_leader && !embedded_fallback,
                embedded_fallback,
                phases = %timer.summary(),
                "Connected"
            );
            timer.emit_telemetry(
                connect_target,
                crate::acp::StartupOutcome::Ok,
                None,
                embedded_fallback,
            );
            conn
        }
        Err(f) => {
            timer.emit_telemetry(connect_target, f.outcome, f.timeout_secs, embedded_fallback);
            if f.outcome == crate::acp::StartupOutcome::Cancelled {
                pending_startup.abandon();
            } else {
                pending_startup.finish(f.outcome);
            }
            let _ = restore_terminal(
                terminal,
                writer_thread,
                ReaderThread::detached(),
                screen_mode,
            );
            crate::unified_log::flush_blocking().await;
            cancel.cancel();
            return Err(f.error);
        }
    };
    let agent_guard =
        crate::acp::spawn::AgentShutdownGuard::new(cancel.clone(), connection.agent_thread.take());
    let effective_args = PagerArgs {
        resume_session: None,
        load_session: None,
        continue_last_session: false,
        session_id: None,
        fork_session: false,
        ..args
    };
    let term_state = event_loop::TerminalState {
        is_control_mode,
        screen_mode,
        relaunched_into_minimal,
        relaunched_into_fullscreen,
        initial_theme: crate::theme::cache::current_kind(),
        startup_typeahead,
    };
    let mut reader_thread = ReaderThread::detached();
    let result = event_loop::run(
        &mut terminal,
        connection,
        pending_startup,
        tracing_handle,
        &mut config_watcher,
        &effective_args,
        session_cwd,
        remote_settings,
        term_state,
        materialized,
        bg_update_rx,
        writer_event_rx,
        &mut reader_thread,
    )
    .await;
    signal_handler::clear_quit_notify();
    let forced_exit_code = match &result {
        Ok(run_result) if run_result.quit_for_update || run_result.relaunch.is_some() => None,
        Ok(_) => Some(0),
        Err(_) => Some(1),
    };
    if let Some(code) = forced_exit_code {
        exit_timeout::arm(code);
        exit_timeout::hold_teardown_for_test();
    }
    let restore_result = restore_terminal(
        terminal,
        writer_thread,
        reader_thread,
        current_screen_mode(),
    );
    crate::unified_log::flush_blocking().await;
    drop(agent_guard);
    distill_telemetry::session_ctx::drain_at_process_exit().await;
    distill_tty_utils::global_process_scope().kill_all();
    crate::app::status_line::metrics::global().report_health();
    let terminal_reading = !matches!(restore_result, Ok(WriterJoin::TimedOut));
    if let Err(cleanup_error) = &restore_result {
        match &result {
            Ok(_) => {
                tracing::warn!(
                    error = %cleanup_error,
                    "terminal cleanup failed after successful event loop"
                )
            }
            Err(run_error) => {
                tracing::warn!(
                    error = %cleanup_error,
                    run_error = %run_error,
                    "terminal cleanup also failed"
                )
            }
        }
    }
    match result {
        Ok(run_result) => {
            if let Some(msg) = run_result.trust_quit_error.as_deref()
                && terminal_reading
            {
                let _ = writeln!(io::stderr(), "{msg}");
            }
            if run_result.quit_for_update {
                return Ok(true);
            }
            if let Some(relaunch) = run_result.relaunch.as_ref() {
                if let Err(e) = screen_mode_relaunch::exec_screen_mode_relaunch(
                    &relaunch.session_id,
                    relaunch.minimal,
                ) {
                    tracing::error!(error = %e, "screen-mode relaunch failed");
                    if terminal_reading {
                        print_relaunch_failure_hint(
                            &e,
                            &relaunch.session_id,
                            relaunch.minimal,
                            &mut io::stderr(),
                        );
                    }
                }
                return Ok(false);
            }
            if let Some(info) = run_result.exit_info
                && terminal_reading
            {
                let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols as usize);
                print_exit_resume_hint(&info, width, &mut io::stderr());
            }
            Ok(false)
        }
        Err(run_error) => Err(run_error),
    }
}
/// Plain-quit "Resume this session with…" lines (after terminal restore).
/// Best-effort: closed-pane EIO/BrokenPipe must not panic (`panic = "abort"`).
/// TODO: extend beyond --minimal by rebuilding resume argv from launch flags (see screen_mode_relaunch)
fn print_exit_resume_hint(info: &ExitInfo, max_width: usize, w: &mut impl Write) {
    use crate::render::line_utils::truncate_str;
    let _ = writeln!(w);
    if let Some(summary) = &info.summary {
        let _ = writeln!(w, "{}", truncate_str(&summary.title, max_width));
        if let Some(prompt) = summary.last_prompt.as_deref() {
            let _ = writeln!(w, "> {}", truncate_str(prompt, max_width.saturating_sub(2)));
        }
        if let Some(response) = summary.last_response.as_deref() {
            let _ = writeln!(
                w,
                "  {}",
                truncate_str(response, max_width.saturating_sub(2))
            );
        }
        let _ = writeln!(w);
    }
    let _ = writeln!(w, "Resume this session with:");
    if info.minimal {
        let _ = writeln!(w, "  grok --minimal --resume {}", info.session_id);
    } else {
        let _ = writeln!(w, "  grok --resume {}", info.session_id);
    }
}
/// Screen-mode relaunch failure fallback (same quit tail as plain resume).
fn print_relaunch_failure_hint(
    error: &impl std::fmt::Display,
    session_id: &str,
    want_minimal: bool,
    w: &mut impl Write,
) {
    let _ = writeln!(w, "Failed to relaunch in requested mode: {error}");
    let _ = writeln!(w, "Resume this session with:");
    let _ = writeln!(
        w,
        "  {}",
        screen_mode_relaunch::screen_mode_relaunch_resume_hint(session_id, want_minimal),
    );
}
/// `crossterm::enable_raw_mode()` sets flags on stdin only.
/// The pager renders to stderr (via `TermWriter`), so the stderr handle must process its ANSI sequences.
/// Without the VTP flag the console silently drops background-color sequences while foreground colors work.
#[cfg(windows)]
fn configure_windows_console() {
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4u32;
    const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    const CP_UTF8: u32 = 65001;
    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: u32) -> *mut core::ffi::c_void;
        fn GetConsoleMode(hConsoleHandle: *mut core::ffi::c_void, lpMode: *mut u32) -> i32;
        fn SetConsoleMode(hConsoleHandle: *mut core::ffi::c_void, dwMode: u32) -> i32;
        fn SetConsoleOutputCP(wCodePageID: u32) -> i32;
    }
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle == -1_isize as *mut _ {
            return;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return;
        }
        let _ = SetConsoleMode(
            handle,
            mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        );
    }
}
/// crossterm's `EnableMouseCapture` is winapi-only on Windows (`is_ansi_code_supported() == false`).
/// `SetConsoleMode` state **outlives the process** for the console window, and teardown historically reset mouse state with ANSI only.
/// Minimal never emits those escapes, so these conhost flags are inert there and asserting them is harmless.
#[cfg(any(windows, test))]
pub(crate) mod win_native_selection {
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
    const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
    /// Stdin console mode for "terminal owns the mouse": QuickEdit on (with the extended-flags gate it needs), app mouse reporting off.
    /// Window-resize events stay on for parity with the capture path; `WINDOW_BUFFER_SIZE_EVENT` is how resize reaches crossterm on conhost.
    /// All other bits are preserved.
    pub(crate) fn native_selection_mode(mode: u32) -> u32 {
        (mode & !ENABLE_MOUSE_INPUT)
            | ENABLE_EXTENDED_FLAGS
            | ENABLE_QUICK_EDIT_MODE
            | ENABLE_WINDOW_INPUT
    }
    #[cfg(windows)]
    pub(crate) use imp::{enable_native_selection, restore_stdin_mode};
    #[cfg(windows)]
    mod imp {
        use std::sync::atomic::{AtomicU64, Ordering};
        const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6u32;
        /// Stdin mode before the first `enable_native_selection`.
        /// `u64::MAX` means "never touched" (the same sentinel scheme crossterm uses for its own capture snapshot).
        /// First writer wins, so repeated enables (e.g. `/mouse` toggles) keep the true original for teardown.
        static ORIGINAL_STDIN_MODE: AtomicU64 = AtomicU64::new(u64::MAX);
        unsafe extern "system" {
            fn GetStdHandle(nStdHandle: u32) -> *mut core::ffi::c_void;
            fn GetConsoleMode(hConsoleHandle: *mut core::ffi::c_void, lpMode: *mut u32) -> i32;
            fn SetConsoleMode(hConsoleHandle: *mut core::ffi::c_void, dwMode: u32) -> i32;
        }
        /// Read the stdin console handle and its current mode.
        /// `None` when stdin is redirected / not a console.
        fn stdin_console_mode() -> Option<(*mut core::ffi::c_void, u32)> {
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                if handle.is_null() || handle == -1_isize as *mut _ {
                    return None;
                }
                let mut mode: u32 = 0;
                if GetConsoleMode(handle, &mut mode) == 0 {
                    return None;
                }
                Some((handle, mode))
            }
        }
        /// Assert the native-selection stdin mode (QuickEdit on, app mouse reporting off, resize events on).
        /// Snapshots the original mode once for [`restore_stdin_mode`].
        pub(crate) fn enable_native_selection() {
            let Some((handle, mode)) = stdin_console_mode() else {
                return;
            };
            let _ = ORIGINAL_STDIN_MODE.compare_exchange(
                u64::MAX,
                u64::from(mode),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            let new_mode = super::native_selection_mode(mode);
            if new_mode != mode {
                unsafe {
                    let _ = SetConsoleMode(handle, new_mode);
                }
            }
        }
        /// Restore the mode captured by the first `enable_native_selection` (does nothing if it never ran).
        /// Teardown-only; consumes the snapshot so concurrent teardown paths (panic hook, restore_terminal) restore at most once.
        pub(crate) fn restore_stdin_mode() {
            let saved = ORIGINAL_STDIN_MODE.swap(u64::MAX, Ordering::AcqRel);
            let Ok(saved) = u32::try_from(saved) else {
                return;
            };
            if let Some((handle, current)) = stdin_console_mode() {
                if current != saved {
                    unsafe {
                        let _ = SetConsoleMode(handle, saved);
                    }
                }
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
        const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
        #[test]
        fn asserts_quick_edit_and_resize_clears_mouse_input() {
            let mode = native_selection_mode(ENABLE_MOUSE_INPUT);
            assert_eq!(mode & ENABLE_MOUSE_INPUT, 0, "app mouse reporting off");
            assert_ne!(mode & ENABLE_QUICK_EDIT_MODE, 0, "QuickEdit on");
            assert_ne!(mode & ENABLE_EXTENDED_FLAGS, 0, "extended-flags gate on");
            assert_ne!(mode & ENABLE_WINDOW_INPUT, 0, "resize events on");
        }
        #[test]
        fn preserves_unrelated_bits() {
            let input = ENABLE_PROCESSED_INPUT | ENABLE_VIRTUAL_TERMINAL_INPUT;
            let mode = native_selection_mode(input);
            assert_eq!(mode & input, input);
        }
        #[test]
        fn idempotent() {
            let once = native_selection_mode(ENABLE_MOUSE_INPUT | ENABLE_PROCESSED_INPUT);
            assert_eq!(native_selection_mode(once), once);
        }
        /// The crossterm capture mode (what a crashed prior run leaves behind) maps to a QuickEdit-on, reporting-off mode.
        #[test]
        fn recovers_from_stale_crossterm_capture_mode() {
            const CROSSTERM_ENABLE_MOUSE_MODE: u32 =
                ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS | ENABLE_WINDOW_INPUT;
            let mode = native_selection_mode(CROSSTERM_ENABLE_MOUSE_MODE);
            assert_eq!(mode & ENABLE_MOUSE_INPUT, 0);
            assert_ne!(mode & ENABLE_QUICK_EDIT_MODE, 0);
        }
    }
}
/// Startup cursor-style policy from `[ui].cursor_blink`.
/// `Inherit` (the `None` default) emits no style escapes, so the terminal's configured cursor shape/blink survives.
/// Forcing one was reported as cursor flicker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorStylePolicy {
    /// Leave the terminal's cursor style untouched (default).
    Inherit,
    /// Legacy: `EnableBlinking` and `SetCursorStyle::BlinkingBlock`.
    ForceBlinking,
    /// `DisableBlinking` and `SetCursorStyle::SteadyBlock`.
    ForceSteady,
}
/// Map the `[ui].cursor_blink` tri-state onto the startup policy.
fn cursor_style_policy(cursor_blink: Option<bool>) -> CursorStylePolicy {
    match cursor_blink {
        None => CursorStylePolicy::Inherit,
        Some(true) => CursorStylePolicy::ForceBlinking,
        Some(false) => CursorStylePolicy::ForceSteady,
    }
}
/// Outcome of [`init_terminal`]: the live terminal, the effective screen mode, and any startup type-ahead captured after raw mode was enabled.
pub(crate) struct TerminalInit {
    pub terminal: PagerTerminal,
    /// The *effective* screen mode, which may differ from the requested one (see [`init_terminal`]).
    pub screen_mode: ScreenMode,
    /// Keystrokes the user typed while the app was still loading, captured by the post-raw-mode drains.
    /// Replayed into the composer by [`event_loop::run`].
    pub startup_typeahead: Vec<event_loop::TimedInputEvent>,
}
/// Initialize the terminal for `mode`.
/// Returns the live terminal handle and the *effective* screen mode, which may differ from the requested one.
/// Minimal's `insert_before` / `set_viewport_height` commit pipeline is a no-op on the `Viewport::Fixed` fallback, so it cannot function there.
fn init_terminal(
    mode: ScreenMode,
    minimal_live_rows: u16,
    clear_main_screen: bool,
    frame_tx: WriterSender,
    writer_sync: WriterSync,
    cursor_blink: Option<bool>,
) -> io::Result<TerminalInit> {
    distill_crash_handler::enable_terminal_escape_restore();
    terminal::enable_raw_mode()?;
    #[cfg(windows)]
    configure_windows_console();
    let want_minimal = mode.is_minimal();
    let mut startup_typeahead: Vec<event_loop::TimedInputEvent> = Vec::new();
    let (terminal, screen_mode) = (|| -> io::Result<(PagerTerminal, ScreenMode)> {
        startup_typeahead.extend(event_loop::capture_startup_typeahead(
            std::time::Duration::from_millis(0),
        ));
        set_terminal_title("");
        if want_minimal && clear_main_screen {
            distill_shell::util::with_locked_stderr(|stderr| {
                execute!(
                    stderr,
                    Clear(ClearType::All),
                    Clear(ClearType::Purge),
                    cursor::MoveTo(0, 0),
                )
            })?;
        }
        if mode.is_fullscreen() {
            distill_shell::util::with_locked_stderr(|stderr| {
                execute!(stderr, EnterAlternateScreen)
            })?;
        }
        #[cfg(windows)]
        if want_minimal {
            win_native_selection::enable_native_selection();
        }
        distill_shell::util::with_locked_stderr(|stderr| {
            if !want_minimal {
                execute!(stderr, event::EnableMouseCapture)?;
            } else if crate::terminal::terminal_context().mouse_reporting_leaks_as_raw_text() {
                let _ = stderr.write_all(distill_crash_handler::terminal::MOUSE_TRACKING_RESET);
            }
            execute!(
                stderr,
                event::EnableFocusChange,
                event::EnableBracketedPaste,
                cursor::Hide,
            )?;
            let policy = cursor_style_policy(cursor_blink);
            match policy {
                CursorStylePolicy::Inherit => {}
                CursorStylePolicy::ForceBlinking => {
                    execute!(
                        stderr,
                        cursor::EnableBlinking,
                        SetCursorStyle::BlinkingBlock
                    )?;
                }
                CursorStylePolicy::ForceSteady => {
                    execute!(stderr, cursor::DisableBlinking, SetCursorStyle::SteadyBlock)?;
                }
            }
            CURSOR_STYLE_FORCED.store(policy != CursorStylePolicy::Inherit, Ordering::Release);
            io::Result::Ok(())
        })?;
        MOUSE_CAPTURE_ENABLED.store(!want_minimal, Ordering::Release);
        set_current_screen_mode(mode);
        set_panic_hook();
        signal_handler::install(mode);
        let drain_timeout = if crate::terminal::terminal_context().vte_version.is_some() {
            std::time::Duration::from_millis(20)
        } else {
            std::time::Duration::ZERO
        };
        startup_typeahead.extend(event_loop::capture_startup_typeahead(drain_timeout));
        crate::theme::apply_cursor_color();
        let ctx = crate::terminal::terminal_context();
        let skip_reason: Option<&str> =
            ctx.kitty_skip_reason()
                .or_else(|| match terminal::supports_keyboard_enhancement() {
                    Ok(true) => None,
                    _ => Some("unsupported"),
                });
        let alacritty_conservative_version = (ctx.brand
            == crate::terminal::TerminalName::Alacritty)
            .then_some(crate::terminal::kitty_keyboard::ALACRITTY_BROKEN_EVENT_TYPES_MAX_PACKED);
        let flags =
            crate::terminal::negotiated_kitty_flags(skip_reason, alacritty_conservative_version);
        if flags.is_empty() {
            tracing::info!(
                kitty.flags = "none",
                kitty.skipped_reason = skip_reason.unwrap_or("unknown"),
                "kitty keyboard protocol skipped"
            );
        } else {
            distill_shell::util::with_locked_stderr(|stderr| {
                let _ = execute!(stderr, event::PushKeyboardEnhancementFlags(flags));
            });
            tracing::info!(
                kitty.flags = ?flags,
                kitty.disambiguate = true,
                kitty.report_event_types =
                    flags.contains(event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES),
                kitty.report_all_keys = false,
                "kitty keyboard protocol pushed"
            );
        }
        crate::terminal::set_pushed_kitty_flags(flags);
        startup_typeahead.extend(event_loop::capture_startup_typeahead(
            std::time::Duration::from_millis(20),
        ));
        event_loop::normalize_startup_submissions(&mut startup_typeahead);
        if mode.is_fullscreen() {
            let backend = CrosstermBackend::new(
                TermWriter::new(frame_tx, writer_sync).map_err(io::Error::other)?,
            );
            Ok((
                distill_ratatui_inline::Terminal::new(backend)?,
                ScreenMode::Fullscreen,
            ))
        } else {
            let (cols, rows) = crossterm::terminal::size()?;
            let viewport_rows = if want_minimal {
                minimal_live_rows.clamp(3, rows.saturating_sub(1).max(3))
            } else {
                rows
            };
            let probe_backend = CrosstermBackend::new(
                TermWriter::new(frame_tx.clone(), writer_sync.clone()).map_err(io::Error::other)?,
            );
            if let Ok(term) = distill_ratatui_inline::Terminal::with_options(
                probe_backend,
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Inline(viewport_rows),
                },
            ) {
                return Ok((
                    term,
                    if want_minimal {
                        ScreenMode::Minimal
                    } else {
                        ScreenMode::Inline
                    },
                ));
            }
            if want_minimal {
                tracing::warn!(
                    "minimal: inline viewport probe failed; downgrading to full-height inline"
                );
                distill_shell::util::with_locked_stderr(|stderr| {
                    execute!(stderr, event::EnableMouseCapture)
                })?;
                MOUSE_CAPTURE_ENABLED.store(true, Ordering::Release);
                let retry_backend = CrosstermBackend::new(
                    TermWriter::new(frame_tx.clone(), writer_sync.clone())
                        .map_err(io::Error::other)?,
                );
                if let Ok(term) = distill_ratatui_inline::Terminal::with_options(
                    retry_backend,
                    ratatui::TerminalOptions {
                        viewport: ratatui::Viewport::Inline(rows),
                    },
                ) {
                    return Ok((term, ScreenMode::Inline));
                }
            } else {
                tracing::error!("inline viewport probe failed, using Viewport::Fixed");
            }
            distill_shell::util::with_locked_stderr(|stderr| {
                execute!(
                    stderr,
                    crossterm::terminal::ScrollUp(rows),
                    cursor::MoveTo(0, 0),
                )
            })?;
            let backend = CrosstermBackend::new(
                TermWriter::new(frame_tx, writer_sync).map_err(io::Error::other)?,
            );
            let term = distill_ratatui_inline::Terminal::with_options(
                backend,
                ratatui::TerminalOptions {
                    viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(
                        0, 0, cols, rows,
                    )),
                },
            )?;
            Ok((term, ScreenMode::Inline))
        }
    })()
    .inspect_err(|_| {
        emit_terminal_teardown_sequences(mode, None);
        let _ = terminal::disable_raw_mode();
        signal_handler::mark_restored();
        distill_crash_handler::disable_terminal_escape_restore();
    })?;
    Ok(TerminalInit {
        terminal,
        screen_mode,
        startup_typeahead,
    })
}
pub(crate) fn set_terminal_title(title: &str) {
    let full = terminal_title_string(title);
    distill_shell::util::with_locked_stderr(|stderr| {
        let _ = execute!(stderr, SetTitle(full));
    });
}
/// Sanitized/truncated window title.
/// Strips control characters: crossterm's `SetTitle` emits the string raw inside an OSC sequence.
/// An embedded BEL/ESC would terminate the OSC early and let the remainder inject arbitrary escape sequences into the terminal.
fn terminal_title_string(title: &str) -> String {
    let sanitized: String = title.chars().filter(|c| !c.is_control()).collect();
    if sanitized.is_empty() {
        "grok".into()
    } else {
        let truncated: String = sanitized.chars().take(80 - 6).collect();
        format!("{} - grok", truncated)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// The loop-top gboom keyboard-layer sync runs on the event-loop thread: its push/pop escapes must ride the writer queue.
    /// push/pop escapes must ride the writer queue.
    #[cfg(not(windows))]
    #[test]
    fn gboom_keyboard_flags_ride_the_writer_queue() {
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = EscapeWriter::new(tx, WriterSync::new());
        let prev = crate::terminal::pushed_kitty_flags();
        crate::terminal::set_pushed_kitty_flags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
        );
        push_gboom_keyboard_flags(&writer);
        pop_gboom_keyboard_flags(&writer);
        crate::terminal::set_pushed_kitty_flags(prev);
        let push = rx.try_recv().expect("push escape queued");
        let pop = rx.try_recv().expect("pop escape queued");
        assert!(String::from_utf8_lossy(push.data()).contains("\x1b[>"));
        assert!(String::from_utf8_lossy(pop.data()).contains("\x1b[<"));
        assert!(rx.try_recv().is_err());
    }
    /// `[ui].cursor_blink` tri-state maps to the startup cursor policy; the `None` default must be Inherit (emit nothing).
    #[test]
    fn cursor_blink_config_maps_to_policy() {
        assert_eq!(cursor_style_policy(None), CursorStylePolicy::Inherit);
        assert_eq!(
            cursor_style_policy(Some(true)),
            CursorStylePolicy::ForceBlinking
        );
        assert_eq!(
            cursor_style_policy(Some(false)),
            CursorStylePolicy::ForceSteady
        );
    }
    fn empty_config() -> toml::Value {
        toml::Value::Table(Default::default())
    }
    fn config_with_leader(enabled: bool) -> toml::Value {
        let toml_str = format!("[cli]\nuse_leader = {enabled}");
        toml::from_str(&toml_str).unwrap()
    }
    #[test]
    fn terminal_title_strips_control_characters() {
        assert_eq!(
            terminal_title_string("evil\x07\x1b]52;c;payload\x07title"),
            "evil]52;c;payloadtitle - grok"
        );
        assert_eq!(terminal_title_string("\x07\x1b\x00"), "grok");
        assert_eq!(terminal_title_string(""), "grok");
        assert_eq!(terminal_title_string("My chat"), "My chat - grok");
    }
    #[test]
    fn hunk_tracker_mode_nothing_set_is_none() {
        assert_eq!(resolve_hunk_tracker_mode(None, None, None), None);
    }
    #[test]
    fn hunk_tracker_mode_empty_env_is_none() {
        assert_eq!(resolve_hunk_tracker_mode(None, Some(""), None), None);
        assert_eq!(resolve_hunk_tracker_mode(None, Some("   "), None), None);
        assert_eq!(resolve_hunk_tracker_mode(None, None, Some("")), None);
    }
    #[test]
    fn hunk_tracker_mode_precedence_cli_over_env_over_config() {
        assert_eq!(
            resolve_hunk_tracker_mode(Some("off"), Some("all_dirty"), Some("agent_only")),
            Some("off".to_string()),
        );
        assert_eq!(
            resolve_hunk_tracker_mode(Some(""), Some("all_dirty"), Some("agent_only")),
            Some("all_dirty".to_string()),
        );
        assert_eq!(
            resolve_hunk_tracker_mode(Some("  "), Some(""), Some("agent_only")),
            Some("agent_only".to_string()),
        );
    }
    #[test]
    fn hunk_tracker_mode_trims_and_passes_off_through() {
        assert_eq!(
            resolve_hunk_tracker_mode(Some(" off "), None, None),
            Some("off".to_string()),
        );
        assert_eq!(
            resolve_hunk_tracker_mode(None, Some("disabled"), None),
            Some("disabled".to_string()),
        );
    }
    #[test]
    fn no_leader_flag_wins_over_leader_flag_and_config() {
        let cfg = config_with_leader(true);
        let (use_leader, reason) = resolve_use_leader(true, true, &cfg, None, true, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn leader_flag_enables() {
        let (use_leader, reason) =
            resolve_use_leader(true, false, &empty_config(), None, true, None);
        assert!(use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn not_eligible_returns_false() {
        let cfg = config_with_leader(true);
        let (use_leader, reason) = resolve_use_leader(false, false, &cfg, None, false, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn config_toml_enables() {
        let cfg = config_with_leader(true);
        let (use_leader, reason) = resolve_use_leader(false, false, &cfg, None, true, None);
        assert!(use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn config_toml_disables() {
        let cfg = config_with_leader(false);
        let (use_leader, reason) = resolve_use_leader(false, false, &cfg, None, true, None);
        assert!(!use_leader);
        assert_eq!(reason, Some("config"));
    }
    #[test]
    fn default_is_false() {
        let (use_leader, reason) =
            resolve_use_leader(false, false, &empty_config(), None, true, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn cli_flag_overrides_config() {
        let cfg = config_with_leader(false);
        let (use_leader, reason) = resolve_use_leader(true, false, &cfg, None, true, None);
        assert!(use_leader);
        assert_eq!(reason, None);
    }
    #[test]
    fn sandbox_confinement_refuses_leader_even_with_leader_flag_and_config_on() {
        let cfg = config_with_leader(true);
        let (use_leader, reason) =
            resolve_use_leader(true, false, &cfg, None, true, Some("strict"));
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    /// `disabled_by_confinement` across the four leader/sandbox cells, driven by every input that can decide leader mode, not just
    /// `[cli] use_leader`.
    #[test]
    fn matrix_reports_the_profile_only_when_the_sandbox_takes_leader_mode_away() {
        let on = config_with_leader(true);
        let off = config_with_leader(false);
        let sandbox = Some("strict");
        for (label, leader_flag, cfg) in [
            ("config on", false, &on),
            ("--leader", true, &empty_config()),
            ("--leader over config off", true, &off),
        ] {
            let resolved = resolve_leader_mode(leader_flag, false, cfg, None, true, sandbox);
            assert!(!resolved.use_leader, "{label}: leader must be vetoed");
            assert_eq!(
                resolved.disabled_by_confinement,
                Some("strict"),
                "{label}: the profile that took leader mode away must be named"
            );
        }
        for (label, cfg, expect_leader) in [("leader on", &on, true), ("leader off", &off, false)] {
            let resolved = resolve_leader_mode(false, false, cfg, None, true, None);
            assert_eq!(resolved.use_leader, expect_leader, "{label}");
            assert_eq!(resolved.disabled_by_confinement, None, "{label}");
        }
        for (label, leader_flag, no_leader_flag, cfg, eligible) in [
            ("config off", false, false, &off, true),
            ("--no-leader over config on", false, true, &on, true),
            ("default", false, false, &empty_config(), true),
            ("ineligible mode with config on", false, false, &on, false),
        ] {
            let resolved =
                resolve_leader_mode(leader_flag, no_leader_flag, cfg, None, eligible, sandbox);
            assert!(!resolved.use_leader, "{label}");
            assert_eq!(
                resolved.disabled_by_confinement, None,
                "{label}: the sandbox took nothing away, so it must stay silent"
            );
        }
    }
    #[test]
    fn sandbox_notice_names_the_profile_without_promising_enforcement() {
        let mut out = Vec::new();
        print_leader_disabled_by_sandbox("strict", &mut out);
        let msg = String::from_utf8(out).expect("utf-8");
        assert!(msg.contains("'strict'"), "must name the profile: {msg}");
        assert!(
            msg.contains("was requested"),
            "must describe the request, not enforcement: {msg}"
        );
        assert!(
            !msg.contains("is active"),
            "must not claim the profile is enforced: {msg}"
        );
        assert!(
            msg.contains("Disable the profile at the source"),
            "must say how to get leader mode back: {msg}"
        );
        assert_eq!(msg.lines().count(), 1, "single line: {msg}");
    }
    #[test]
    fn sandbox_confinement_preserves_config_off_reclaim_reason() {
        let cfg = config_with_leader(false);
        let (use_leader, reason) =
            resolve_use_leader(false, false, &cfg, None, true, Some("strict"));
        assert!(!use_leader);
        assert_eq!(reason, Some("config"));
    }
    fn try_parse_pager(args: &[&str]) -> Result<PagerArgs, clap::Error> {
        use clap::Parser;
        PagerArgs::try_parse_from(args)
    }
    #[test]
    fn cli_leader_and_no_leader_conflict() {
        let result = try_parse_pager(&["grok-pager", "--leader", "--no-leader"]);
        assert!(result.is_err());
    }
    #[test]
    fn cli_leader_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--leader"]).unwrap();
        assert!(args.leader);
        assert!(!args.no_leader);
    }
    #[test]
    fn cli_no_leader_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--no-leader"]).unwrap();
        assert!(!args.leader);
        assert!(args.no_leader);
    }
    #[test]
    fn cli_hidden_memory_compat_flags_parse_and_collapse() {
        let enabled = try_parse_pager(&["grok-pager", "--experimental-memory"]).unwrap();
        assert_eq!(enabled.memory_enabled_override(), Some(true));
        assert_eq!(
            enabled.memory_override_flag(),
            Some("--experimental-memory")
        );
        let disabled = try_parse_pager(&["grok-pager", "--no-memory"]).unwrap();
        assert_eq!(disabled.memory_enabled_override(), Some(false));
        assert_eq!(disabled.memory_override_flag(), Some("--no-memory"));
        let deferred = try_parse_pager(&["grok-pager"]).unwrap();
        assert_eq!(deferred.memory_enabled_override(), None);
        assert_eq!(deferred.memory_override_flag(), None);
    }
    #[test]
    fn cli_hidden_memory_compat_flags_conflict() {
        assert!(try_parse_pager(&["grok-pager", "--experimental-memory", "--no-memory"]).is_err());
    }
    #[test]
    fn cli_memory_flush_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--memory-flush"]).unwrap();
        assert!(args.memory_flush);
        assert!(!try_parse_pager(&["grok-pager"]).unwrap().memory_flush);
    }
    #[test]
    fn cli_neither_leader_flag_defaults_false() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(!args.leader);
        assert!(!args.no_leader);
    }
    #[test]
    fn no_leader_flag_overrides_config_for_tui_fallback() {
        let cfg = config_with_leader(true);
        let (use_leader, reason) = resolve_use_leader(false, true, &cfg, None, true, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    /// clap accepts top-level --leader with agent subcommand, so main() must reject the combination at runtime.
    #[test]
    fn cli_top_level_leader_with_agent_subcommand_parses_flag() {
        let args = try_parse_pager(&["grok-pager", "--leader", "agent"]).unwrap();
        assert!(args.leader);
        assert!(matches!(args.command, Some(Command::Agent(_))));
    }
    #[test]
    fn cli_top_level_no_leader_with_agent_subcommand_parses_flag() {
        let args = try_parse_pager(&["grok-pager", "--no-leader", "agent"]).unwrap();
        assert!(args.no_leader);
        assert!(matches!(args.command, Some(Command::Agent(_))));
    }
    #[test]
    fn remote_settings_none_falls_through_to_default() {
        let (use_leader, reason) =
            resolve_use_leader(false, false, &empty_config(), None, true, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    #[cfg(feature = "release-dist")]
    #[test]
    fn remote_settings_leader_mode_true_enables_leader() {
        let rs = distill_shell::util::config::RemoteSettings {
            leader_mode: Some(true),
            ..Default::default()
        };
        let (use_leader, reason) =
            resolve_use_leader(false, false, &empty_config(), Some(&rs), true, None);
        assert!(use_leader);
        assert_eq!(reason, None);
    }
    #[cfg(feature = "release-dist")]
    #[test]
    fn remote_settings_leader_mode_false_disables_leader() {
        let rs = distill_shell::util::config::RemoteSettings {
            leader_mode: Some(false),
            ..Default::default()
        };
        let (use_leader, reason) =
            resolve_use_leader(false, false, &empty_config(), Some(&rs), true, None);
        assert!(!use_leader);
        assert_eq!(reason, Some("remote"));
    }
    #[cfg(feature = "release-dist")]
    #[test]
    fn remote_settings_unknown_leader_mode_is_not_policy_disable() {
        let rs = distill_shell::util::config::RemoteSettings {
            leader_mode: None,
            ..Default::default()
        };
        let (use_leader, reason) =
            resolve_use_leader(false, false, &empty_config(), Some(&rs), true, None);
        assert!(!use_leader);
        assert_eq!(reason, None);
    }
    #[cfg(feature = "release-dist")]
    #[test]
    fn config_toml_overrides_remote_settings() {
        let rs = distill_shell::util::config::RemoteSettings {
            leader_mode: Some(true),
            ..Default::default()
        };
        let cfg = config_with_leader(false);
        let (use_leader, reason) = resolve_use_leader(false, false, &cfg, Some(&rs), true, None);
        assert!(!use_leader);
        assert_eq!(reason, Some("config"));
    }
    #[test]
    fn cli_resume_parses_session_id() {
        let args = try_parse_pager(&["grok-pager", "--resume", "abc-123"]).unwrap();
        assert_eq!(args.session_to_resume(), Some("abc-123"));
    }
    #[test]
    fn cli_short_r_parses_session_id() {
        let args = try_parse_pager(&["grok-pager", "-r", "abc-123"]).unwrap();
        assert_eq!(args.session_to_resume(), Some("abc-123"));
    }
    #[test]
    fn cli_load_alias_parses_session_id() {
        let args = try_parse_pager(&["grok-pager", "--load", "abc-123"]).unwrap();
        assert_eq!(args.session_to_resume(), Some("abc-123"));
    }
    #[test]
    fn cli_resume_preferred_over_load() {
        let mut args = try_parse_pager(&["grok-pager", "--resume", "from-resume"]).unwrap();
        args.load_session = Some("from-load".into());
        assert_eq!(args.session_to_resume(), Some("from-resume"));
    }
    #[test]
    fn cli_continue_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--continue"]).unwrap();
        assert!(args.continue_last_session);
        assert_eq!(args.session_to_resume(), None);
    }
    #[test]
    fn cli_continue_short_c_parses() {
        let args = try_parse_pager(&["grok-pager", "-c"]).unwrap();
        assert!(args.continue_last_session);
    }
    #[test]
    fn cli_resume_no_id_sets_empty_sentinel() {
        let args = try_parse_pager(&["grok-pager", "--resume"]).unwrap();
        assert_eq!(args.resume_session.as_deref(), Some(""));
        assert!(args.resume_most_recent());
        assert_eq!(args.session_to_resume(), None);
    }
    #[test]
    fn cli_short_r_no_id_sets_empty_sentinel() {
        let args = try_parse_pager(&["grok-pager", "-r"]).unwrap();
        assert_eq!(args.resume_session.as_deref(), Some(""));
        assert!(args.resume_most_recent());
    }
    #[test]
    fn cli_resume_with_id_is_not_most_recent() {
        let args = try_parse_pager(&["grok-pager", "--resume", "abc-123"]).unwrap();
        assert!(!args.resume_most_recent());
        assert_eq!(args.session_to_resume(), Some("abc-123"));
    }
    #[test]
    fn cli_no_resume_is_not_most_recent() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(!args.resume_most_recent());
    }
    #[test]
    fn cli_continue_conflicts_with_resume() {
        let result = try_parse_pager(&["grok-pager", "--continue", "--resume", "abc"]);
        assert!(result.is_err());
    }
    #[test]
    fn cli_continue_conflicts_with_load() {
        let result = try_parse_pager(&["grok-pager", "--continue", "--load", "abc"]);
        assert!(result.is_err());
    }
    #[test]
    fn cli_no_session_flags_defaults() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(!args.continue_last_session);
        assert!(args.worktree.is_none());
        assert_eq!(args.session_to_resume(), None);
        assert!(!args.chat());
    }
    /// Without the optional feature the flag must not exist at all: a stable binary given that flag fails clap parsing instead of silently ignoring.
    #[test]
    fn cli_chat_flag_rejected_without_feature() {
        assert!(try_parse_pager(&["grok-pager", "--chat"]).is_err());
    }
    #[cfg(feature = "local-workspace")]
    #[test]
    fn cli_local_workspace_attach_requires_chat() {
        assert!(
            try_parse_pager(&["grok-pager", "--local-workspace-attach=srv"]).is_err(),
            "attach without --chat must clap-error"
        );
        let args =
            try_parse_pager(&["grok-pager", "--chat", "--local-workspace-attach=srv"]).unwrap();
        assert_eq!(args.local_workspace_attach(), Some("srv"));
    }
    #[cfg(feature = "local-workspace")]
    #[test]
    fn cli_local_workspace_own_conflicts_with_attach() {
        assert!(
            try_parse_pager(&[
                "grok-pager",
                "--chat",
                "--local-workspace=/tmp/a",
                "--local-workspace-attach=srv",
            ])
            .is_err(),
            "own + attach must clap-conflict"
        );
    }
    #[cfg(feature = "local-workspace")]
    #[test]
    fn cli_local_workspace_cwd_requires_chat() {
        assert!(try_parse_pager(&["grok-pager", "--local-workspace-cwd=/tmp/a"]).is_err());
        let args = try_parse_pager(&[
            "grok-pager",
            "--chat",
            "--local-workspace-attach=srv",
            "--local-workspace-cwd=/tmp/repo",
        ])
        .unwrap();
        assert_eq!(
            args.local_workspace_cwd(),
            Some(std::path::Path::new("/tmp/repo"))
        );
    }
    #[test]
    fn cli_local_workspace_flags_rejected_without_feature() {
        assert!(try_parse_pager(&["grok-pager", "--local-workspace-attach=srv"]).is_err());
        assert!(try_parse_pager(&["grok-pager", "--local-workspace"]).is_err());
        assert!(try_parse_pager(&["grok-pager", "--local-workspace-cwd=/tmp"]).is_err());
    }
    #[test]
    fn chat_mode_leader_guard_truth_table() {
        assert!(session_startup::chat_mode_conflicts_with_leader(true, true));
        assert!(!session_startup::chat_mode_conflicts_with_leader(
            true, false
        ));
        assert!(!session_startup::chat_mode_conflicts_with_leader(
            false, true
        ));
        assert!(!session_startup::chat_mode_conflicts_with_leader(
            false, false
        ));
    }
    #[test]
    fn cli_worktree_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--worktree"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some(""));
    }
    #[test]
    fn cli_worktree_short_w_parses() {
        let args = try_parse_pager(&["grok-pager", "-w"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some(""));
    }
    #[test]
    fn cli_worktree_with_label() {
        let args = try_parse_pager(&["grok-pager", "-w", "my-label"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some("my-label"));
    }
    #[test]
    fn cli_worktree_long_with_label() {
        let args = try_parse_pager(&["grok-pager", "--worktree", "fix-bug"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some("fix-bug"));
    }
    #[test]
    fn cli_worktree_with_empty_string() {
        let args = try_parse_pager(&["grok-pager", "-w", ""]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some(""));
    }
    #[test]
    fn cli_worktree_with_resume_parses() {
        let args = try_parse_pager(&["grok-pager", "-w", "--resume", "abc"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some(""));
        assert_eq!(args.session_to_resume(), Some("abc"));
    }
    #[test]
    fn cli_worktree_label_with_resume() {
        let args = try_parse_pager(&["grok-pager", "-w", "my-label", "--resume", "abc"]).unwrap();
        assert_eq!(args.worktree.as_deref(), Some("my-label"));
        assert_eq!(args.session_to_resume(), Some("abc"));
    }
    #[test]
    fn cli_worktree_default_none() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(args.worktree.is_none());
    }
    #[test]
    fn cli_session_id_parses() {
        let args = try_parse_pager(&["grok-pager", "--session-id", "my-id"]).unwrap();
        assert_eq!(args.session_id.as_deref(), Some("my-id"));
        assert!(matches!(
            args.session_startup_intent().unwrap(),
            crate::app::session_startup::SessionStartupIntent::NewWithId { .. }
        ));
    }
    #[test]
    fn cli_session_id_short_s_parses() {
        let args = try_parse_pager(&["grok-pager", "-s", "my-id"]).unwrap();
        assert_eq!(args.session_id.as_deref(), Some("my-id"));
    }
    #[test]
    fn cli_session_id_with_resume_requires_fork() {
        let args = try_parse_pager(&["grok-pager", "-s", "a", "--resume", "b"]).unwrap();
        assert!(args.session_startup_intent().is_err());
    }
    #[test]
    fn cli_session_id_with_continue_requires_fork() {
        let args = try_parse_pager(&["grok-pager", "-s", "a", "--continue"]).unwrap();
        assert!(args.session_startup_intent().is_err());
    }
    #[test]
    fn cli_session_id_with_resume_and_fork_ok() {
        let args =
            try_parse_pager(&["grok-pager", "-s", "a", "--resume", "b", "--fork-session"]).unwrap();
        assert!(args.session_startup_intent().is_ok());
    }
    #[test]
    fn cli_session_id_default_none() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(args.session_id.is_none());
    }
    #[test]
    fn cli_no_alt_screen_flag_parses() {
        let args = try_parse_pager(&["grok-pager", "--no-alt-screen"]).unwrap();
        assert!(args.no_alt_screen);
    }
    #[test]
    fn cli_no_alt_screen_default_false() {
        let args = try_parse_pager(&["grok-pager"]).unwrap();
        assert!(!args.no_alt_screen);
    }
    #[test]
    fn cli_command_name_is_grok() {
        use clap::CommandFactory;
        assert_eq!(PagerArgs::command().get_name(), "grok");
    }
    #[test]
    fn cli_help_output_header() {
        use clap::CommandFactory;
        let help = PagerArgs::command().render_long_help().to_string();
        let first_5: Vec<&str> = help.lines().take(5).collect();
        assert_eq!(
            first_5,
            vec![
                "Distill TUI",
                "",
                "Usage: grok [OPTIONS] [PROMPT] [COMMAND]",
                "",
                "Arguments:",
            ]
        );
        assert!(help.find("Arguments:\n").unwrap() < help.find("Options:\n").unwrap());
        assert!(help.find("Options:\n").unwrap() < help.find("Commands:\n").unwrap());
    }
    #[test]
    fn cli_completions_parses() {
        use clap_complete::Shell;
        let args = try_parse_pager(&["grok-pager", "completions", "zsh"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Completions { shell: Shell::Zsh })
        ));
        let args = try_parse_pager(&["grok-pager", "completions", "bash"]).unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Completions { shell: Shell::Bash })
        ));
    }
    /// Always fails writes with EIO (os error 5), like a closed-pane stderr.
    struct AlwaysFailWrite;
    impl Write for AlwaysFailWrite {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(5))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(5))
        }
    }
    /// [`ExitInfo`] with no summary, as built for inline/minimal quits.
    fn bare_exit_info(session_id: &str, minimal: bool) -> ExitInfo {
        ExitInfo {
            session_id: session_id.to_string(),
            minimal,
            summary: None,
        }
    }
    #[test]
    fn print_exit_resume_hint_writes_expected_lines() {
        let mut buf = Vec::new();
        print_exit_resume_hint(&bare_exit_info("sess-abc", false), 80, &mut buf);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\nResume this session with:\n  grok --resume sess-abc\n"
        );
    }
    #[test]
    fn print_exit_resume_hint_includes_minimal_flag() {
        let mut buf = Vec::new();
        print_exit_resume_hint(&bare_exit_info("sess-abc", true), 80, &mut buf);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "\nResume this session with:\n  grok --minimal --resume sess-abc\n"
        );
    }
    #[test]
    fn print_exit_resume_hint_includes_session_summary() {
        let info = ExitInfo {
            session_id: "sess-abc".to_string(),
            minimal: false,
            summary: Some(ExitSummary {
                title: "Fix flaky CI test".to_string(),
                last_prompt: Some("make the suite deterministic".to_string()),
                last_response: Some("Pinned the seed; 200 consecutive green runs.".to_string()),
            }),
        };
        let mut buf = Vec::new();
        print_exit_resume_hint(&info, 80, &mut buf);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            concat!(
                "\n",
                "Fix flaky CI test\n",
                "> make the suite deterministic\n",
                "  Pinned the seed; 200 consecutive green runs.\n",
                "\n",
                "Resume this session with:\n",
                "  grok --resume sess-abc\n",
            )
        );
    }
    #[test]
    fn print_exit_resume_hint_truncates_summary_to_width() {
        let info = ExitInfo {
            session_id: "sess-abc".to_string(),
            minimal: false,
            summary: Some(ExitSummary {
                title: "t".repeat(50),
                last_prompt: Some("p".repeat(50)),
                last_response: Some("r".repeat(50)),
            }),
        };
        let mut buf = Vec::new();
        print_exit_resume_hint(&info, 20, &mut buf);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains(&format!("\n{}…\n", "t".repeat(19))));
        assert!(out.contains(&format!("\n> {}…\n", "p".repeat(17))));
        assert!(out.contains(&format!("\n  {}…\n", "r".repeat(17))));
        assert!(out.contains("  grok --resume sess-abc\n"));
    }
    #[test]
    fn print_relaunch_failure_hint_writes_expected_lines() {
        let mut buf = Vec::new();
        print_relaunch_failure_hint(&"exec failed", "sess-xyz", false, &mut buf);
        let hint = screen_mode_relaunch::screen_mode_relaunch_resume_hint("sess-xyz", false);
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            format!(
                "Failed to relaunch in requested mode: exec failed\n\
                 Resume this session with:\n  {hint}\n"
            )
        );
    }
    /// [`ExitInfo`] with a full summary, for the failing-writer tests.
    fn full_exit_info(session_id: &str) -> ExitInfo {
        ExitInfo {
            summary: Some(ExitSummary {
                title: "title".to_string(),
                last_prompt: Some("prompt".to_string()),
                last_response: Some("response".to_string()),
            }),
            ..bare_exit_info(session_id, false)
        }
    }
    #[test]
    fn print_hints_survive_eio() {
        let mut w = AlwaysFailWrite;
        print_exit_resume_hint(&bare_exit_info("sess-abc", false), 80, &mut w);
        print_exit_resume_hint(&bare_exit_info("sess-abc", true), 80, &mut w);
        print_exit_resume_hint(&full_exit_info("sess-abc"), 80, &mut w);
        print_relaunch_failure_hint(&"exec failed", "sess-xyz", true, &mut w);
        print_leader_disabled_by_sandbox("strict", &mut w);
    }
    /// Close the *read* end so writes on the write end get EPIPE.
    /// SIGPIPE is SIG_IGN, so the write returns BrokenPipe instead of killing the process.
    #[cfg(unix)]
    #[test]
    fn print_hints_survive_closed_pipe() {
        use std::os::unix::io::FromRawFd;
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe() failed");
        unsafe {
            libc::close(fds[0]);
        }
        let mut writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        print_exit_resume_hint(&bare_exit_info("pipe-sid", false), 80, &mut writer);
        print_exit_resume_hint(&bare_exit_info("pipe-sid", true), 80, &mut writer);
        print_exit_resume_hint(&full_exit_info("pipe-sid"), 80, &mut writer);
        print_relaunch_failure_hint(&"exec failed", "pipe-sid", false, &mut writer);
        print_leader_disabled_by_sandbox("strict", &mut writer);
    }
}
