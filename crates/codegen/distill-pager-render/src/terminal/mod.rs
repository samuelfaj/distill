// Modified for Distill by Samuel Fajreldines, 2026.
//! Detects terminal emulator, multiplexer, and Byobu from environment variables.
//! Pure env-map helpers (`detect_*_from_env`) enable full matrix testing.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::host::HostOs;

pub mod da2;
pub mod embedded_editor;
pub mod hyperlinks;
pub mod image;
pub mod keyboard;
pub mod kitty_keyboard;
pub mod overlay;
pub mod pop_fence;
pub(crate) mod probe;
pub mod term_version;
pub mod tmux;
pub mod tmux_probe;
pub mod xtversion;

pub use tmux::{
    passthrough_available, should_emit_synchronized_output, should_wrap_osc11, tmux_passthrough,
    tmux_passthrough_str,
};

pub use embedded_editor::{EmbeddedEditor, embedded_editor_from_env};
pub use hyperlinks::{
    HyperlinkCapabilities, Osc8Support, SchemeFilter, SetDefaultCursor, SetPointerCursor,
    hyperlink_capabilities,
};
pub use keyboard::{
    KeyboardCapabilities, ModifierDelivery, ModifierFate, keyboard_capabilities,
    keyboard_capabilities_for_host,
};
pub use kitty_keyboard::{
    kitty_event_types_withheld, kitty_flags_pushed, kitty_releases_reported,
    negotiated_kitty_flags, pushed_kitty_flags, set_pushed_kitty_flags, take_kitty_flags_pushed,
};
pub use pop_fence::{PopFence, PopFenceOutcome};
pub use term_version::{TermVersion, TermVersionSource};

#[cfg(test)]
mod test;

/// Shared by the `test` submodule and `embedded_editor`'s tests.
#[cfg(test)]
pub(crate) fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Known terminal emulator categories.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display)]
pub enum TerminalName {
    /// Apple Terminal (Terminal.app).
    #[strum(to_string = "Apple Terminal")]
    AppleTerminal,
    Ghostty,
    #[strum(to_string = "iTerm2")]
    Iterm2,
    #[strum(to_string = "Warp")]
    WarpTerminal,
    /// VS Code integrated terminal.
    #[strum(to_string = "VS Code")]
    VsCode,
    /// Integrated terminal for the `Cursor` brand (VS Code family).
    Cursor,
    /// Integrated terminal for the `Windsurf` brand (VS Code family).
    Windsurf,
    /// Zed editor integrated terminal.
    Zed,
    WezTerm,
    #[strum(to_string = "Kitty")]
    Kitty,
    Alacritty,
    Rio,
    /// foot terminal emulator (Wayland-native, Linux-only), detected via TERM.
    /// It has full native Kitty keyboard protocol support.
    #[strum(to_string = "foot")]
    Foot,
    /// JetBrains IDE integrated terminal (JediTerm: IntelliJ, PhpStorm, etc.).
    /// No runtime capability probing is possible (no TERM_FEATURES, no XTVERSION, DA1 is bare VT102).
    /// The Classic and Reworked 2025 engines are indistinguishable, and all capabilities are conservative/Unknown.
    #[strum(to_string = "JetBrains")]
    JetBrains,
    /// Grok Desktop (Electron app).
    #[strum(to_string = "Grok Desktop")]
    GrokDesktop,
    /// VTE-based terminal (GNOME Terminal, kgx/GNOME Console, Tilix, etc.).
    #[strum(to_string = "VTE")]
    Vte,
    /// Terminator terminal emulator (Python/GTK, VTE-based).
    /// It is detected via the `TERMINATOR_UUID` env var it exports on every child process, or `TERM_PROGRAM=terminator`.
    Terminator,
    /// Windows Terminal (wt, the default terminal on Windows 11+).
    #[strum(to_string = "Windows Terminal")]
    WindowsTerminal,
    /// Otty (otty.sh). Wraps macOS IME commits in bracketed paste.
    #[strum(to_string = "Otty")]
    Otty,
    #[default]
    Unknown,
}

impl TerminalName {
    pub fn is_vte_based(self) -> bool {
        matches!(self, Self::Vte | Self::Terminator)
    }

    /// VS Code integrated terminal and xterm.js-based IDE embeds (including forks).
    pub fn is_vscode_family(self) -> bool {
        matches!(
            self,
            Self::VsCode | Self::Cursor | Self::Windsurf | Self::Zed
        )
    }

    /// Terminals that embed xterm.js (Zed's terminal is alacritty-based and is NOT one).
    /// This is the boundary for xterm.js-specific quirks, e.g. the wedged button tracker that eats mouse releases.
    pub fn is_xtermjs_embed(self) -> bool {
        matches!(
            self,
            Self::VsCode | Self::Cursor | Self::Windsurf | Self::GrokDesktop
        )
    }

    /// Brands whose capabilities are not positively classified.
    /// They share [`Self::Unknown`]'s fail-closed posture (no KKP probe, conservative hyperlinks/notifications/focus, etc.).
    pub fn is_capability_unclassified(self) -> bool {
        matches!(self, Self::Unknown | Self::Otty)
    }

    /// Host applies OSC 52 writes to the system pasteboard (fail closed).
    pub fn supports_osc52_clipboard(self) -> bool {
        matches!(
            self,
            Self::Ghostty
                | Self::Kitty
                | Self::WezTerm
                | Self::Alacritty
                | Self::Foot
                | Self::Rio
                | Self::WindowsTerminal
                | Self::Iterm2
                | Self::VsCode
                | Self::Cursor
                | Self::Windsurf
                | Self::Zed
        )
    }

    /// Only Otty is known to wrap macOS IME commits in bracketed paste.
    pub fn delivers_ime_as_bracketed_paste(self) -> bool {
        matches!(self, Self::Otty)
    }
}

impl TerminalContext {
    pub fn is_vte_based(&self) -> bool {
        self.brand.is_vte_based() || self.vte_version.is_some()
    }
}

/// The kind of terminal multiplexer wrapping the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display)]
pub enum MultiplexerKind {
    /// tmux (including Byobu-on-tmux).
    #[strum(to_string = "tmux")]
    Tmux,
    /// GNU screen (including Byobu-on-screen).
    #[strum(to_string = "GNU screen")]
    Screen,
    /// Zellij.
    Zellij,
    /// cmux (Ghostty-backed macOS terminal multiplexer).
    #[strum(to_string = "cmux")]
    Cmux,
    /// Embedded emulator answers CSI itself, so it counts as CSI-intercepting. XTVERSION then describes herdr's engine, not the host.
    #[strum(to_string = "herdr")]
    Herdr,
    /// No recognized multiplexer detected (does not rule out unknown ones).
    #[default]
    #[strum(to_string = "None detected")]
    Undetected,
}

impl MultiplexerKind {
    /// Whether this multiplexer intercepts CSI queries (e.g. XTVERSION) instead of passing them through to the outer terminal.
    /// See [`Self::Herdr`] for the version signal herdr gives up by being here.
    pub fn intercepts_csi_queries(self) -> bool {
        matches!(self, Self::Tmux | Self::Screen | Self::Zellij | Self::Herdr)
    }
}

/// The Byobu backend, when Byobu markers are present.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, strum::Display)]
pub enum ByobuBackend {
    /// Unknown backend (default; never produced by detection).
    #[default]
    Unknown,
    /// Byobu wrapping tmux.
    #[strum(to_string = "tmux")]
    Tmux,
    /// Byobu wrapping GNU screen.
    #[strum(to_string = "GNU screen")]
    Screen,
}

/// Fields are gathered at startup from environment variables; no live subprocess calls are made here.
/// Live tmux-option queries remain in [`crate::diagnostics`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TmuxClientMeta {
    /// The raw `TMUX` variable value (e.g. `/tmp/tmux-501/default,12345,0`).
    pub tmux_env: Option<String>,
    /// The `TMUX_PANE` value (e.g. `%0`).
    pub tmux_pane: Option<String>,
}

/// The single source of truth that later features (warnings, fullscreen policy, clipboard routing) consume.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalContext {
    /// The effective terminal emulator brand for capability and display decisions.
    /// On native Windows an `Unknown` env detection is resolved to [`TerminalName::WindowsTerminal`] (see `refine_unknown_brand_for_host`).
    pub brand: TerminalName,
    /// The raw brand from environment detection, before the native-Windows `Unknown -> WindowsTerminal` fallback applied to `brand`.
    /// Consult this (not `brand`) for conservative, default-deny decisions that must not trust the assumed brand.
    /// Examples: the legacy-Windows glyph fallback and the unidentified-terminal `Shift+Enter` gate.
    pub env_brand: TerminalName,
    /// The detected multiplexer wrapping the session.
    pub multiplexer: MultiplexerKind,
    /// Whether Byobu is wrapping the session, and which backend it uses.
    pub byobu: Option<ByobuBackend>,
    /// Which embedded editor `:terminal` grok is running inside, if any.
    pub embedded_editor: Option<EmbeddedEditor>,
    /// tmux client metadata (populated only when `multiplexer == Tmux`).
    pub tmux_meta: TmuxClientMeta,
    /// Whether the session is inside a remote SSH connection.
    pub is_ssh: bool,
    /// Positive evidence that SSH is hosted by the official VS Code remote server.
    pub is_official_vscode_remote: bool,
    /// The raw `TERM` environment variable (e.g. `xterm-256color`, `screen`).
    pub term_var: Option<String>,
    /// The tmux server version (e.g. `"tmux 3.4"`), populated only when `multiplexer == Tmux`.
    /// It is detected via a `tmux -V` subprocess at startup.
    pub tmux_version: Option<String>,
    /// The VTE version (e.g. `"7402"` for VTE 0.74.2).
    /// `None` when not running inside a VTE terminal.
    pub vte_version: Option<String>,
    /// Value of tmux's `extended-keys` global option (`"on"`, `"off"`, `"always"`); populated only when `multiplexer == Tmux`.
    pub tmux_extended_keys: Option<String>,
    /// The `TERM_PROGRAM_VERSION` environment variable, falling back to `LC_TERMINAL_VERSION` (e.g. `"3.5.6"` for iTerm2, `"1.1.3"` for Ghostty).
    /// Raw and **ungated**: inside tmux this is tmux's own version.
    /// For a brand-corroborated value use [`Self::env_term_version`].
    pub term_program_version: Option<String>,
    /// The `TERM_FEATURES` capability string (iTerm2 feature reporting, <https://iterm2.com/feature-reporting/>).
    /// It is not an `LC_*` variable, so it never crosses SSH.
    pub term_features: Option<String>,
    /// The brand-corroborated counterpart to `term_program_version` (see [`term_version`]).
    /// It is resolved once in [`build_terminal_context_from_env`], so it does not re-derive if `brand` or `vte_version` change afterwards.
    pub env_term_version: Option<TermVersion>,
}

impl TerminalContext {
    /// Returns `true` if the session is inside any tmux-backed environment (plain tmux or Byobu-on-tmux).
    pub fn is_tmux_backed(&self) -> bool {
        self.multiplexer == MultiplexerKind::Tmux
    }

    /// Per-terminal keyboard capabilities for this brand on the current host.
    pub fn keyboard_capabilities(&self) -> KeyboardCapabilities {
        keyboard_capabilities(self.brand)
    }

    /// Per-terminal hyperlink (OSC 8) capabilities for this brand.
    pub fn hyperlink_capabilities(&self) -> HyperlinkCapabilities {
        hyperlink_capabilities(self.brand)
    }

    /// Returns `true` if the session is inside Byobu regardless of backend.
    pub fn is_byobu(&self) -> bool {
        self.byobu.is_some()
    }

    /// Whether an outer layer (embedded-editor :terminal or multiplexer) can repaint our pane out of band, stranding rows until a full clear.
    /// A heal keyed off this only fires when a FocusGained actually reaches grok.
    /// That needs focus reporting enabled upstream (e.g. tmux `focus-events on`, off by default).
    pub fn repaints_pane_out_of_band(&self) -> bool {
        self.embedded_editor.is_some() || self.multiplexer != MultiplexerKind::Undetected
    }

    /// In Byobu-on-tmux, this is `~/.byobu/.tmux.conf`; otherwise `~/.tmux.conf`.
    pub fn tmux_config_path(&self) -> String {
        if self.byobu == Some(ByobuBackend::Tmux) {
            "~/.byobu/.tmux.conf".to_owned()
        } else {
            "~/.tmux.conf".to_owned()
        }
    }

    /// Returns the `TERM` variable value, or `"n/a"` if unset.
    pub fn term_var_or_na(&self) -> &str {
        self.term_var.as_deref().unwrap_or("n/a")
    }

    /// Returns the tmux version string, or `"n/a"` if not in tmux or detection failed.
    pub fn tmux_version_or_na(&self) -> &str {
        self.tmux_version.as_deref().unwrap_or("n/a")
    }

    /// Returns the reason to skip Kitty keyboard flags, or `None` if the environment is compatible.
    ///
    /// Terminal-emulator reasons take precedence over multiplexer reasons so the user is pointed at the deeper cause.
    pub fn kitty_skip_reason(&self) -> Option<&'static str> {
        if matches!(
            self.brand,
            TerminalName::VsCode
                | TerminalName::Cursor
                | TerminalName::Windsurf
                | TerminalName::Zed
        ) {
            return Some("vscode");
        }
        if self.brand == TerminalName::AppleTerminal {
            return Some("apple_terminal");
        }
        if self.is_vte_based() {
            return Some("vte");
        }
        if self.brand == TerminalName::WindowsTerminal {
            return Some("windows_terminal");
        }
        if self.brand == TerminalName::JetBrains {
            return Some("jetbrains");
        }
        if let Some(reason) = self.kitty_multiplexer_skip_reason() {
            return Some(reason);
        }
        // No positive evidence of KKP support, so skip: xterm.js mis-encodes shifted keys (https://github.com/xtermjs/xterm.js/issues/5823)
        // Probing an unresponsive terminal blocks startup.
        if self.brand.is_capability_unclassified()
            && self.multiplexer == MultiplexerKind::Undetected
        {
            return Some("unknown_no_multiplexer");
        }
        None
    }

    /// Multiplexer layers that drop Kitty keyboard / extended Enter, independent of brand.
    ///
    /// [`Self::kitty_skip_reason`] may hide these behind a brand/VTE reason; footer and
    /// newline-preference decisions must still see them.
    fn kitty_multiplexer_skip_reason(&self) -> Option<&'static str> {
        let is_tmux_3_3_later = self.is_tmux_version_or_later(3, 3);
        if self.multiplexer == MultiplexerKind::Screen {
            return Some("screen");
        }
        if self.multiplexer == MultiplexerKind::Tmux && !is_tmux_3_3_later {
            return Some("tmux_old");
        }
        if self.multiplexer == MultiplexerKind::Tmux
            && is_tmux_3_3_later
            && self.tmux_extended_keys.as_deref() == Some("off")
        {
            return Some("tmux_extended_keys_off");
        }
        None
    }

    /// Returns the reason inline images / terminal graphics protocols are disabled, or `None` if the environment is compatible.
    pub fn graphics_protocol_skip_reason(&self) -> Option<&'static str> {
        if self.is_tmux_backed() {
            return Some("tmux");
        }
        None
    }

    /// JediTerm on Windows emits VT mouse bytes crossterm does not decode, so they land as key presses and corrupt the prompt. Windows-only; those sessions default to minimal mode.
    pub fn mouse_reporting_leaks_as_raw_text(&self) -> bool {
        mouse_reporting_leaks(self.brand, HostOs::current())
    }

    /// True where KKP cannot be relied on: legacy/unknown VTE, xterm.js forks (KKP mis-encodes shifted printables), and unclassified brands with no mux (often VS Code over SSH).
    /// Those send bare CR for Shift+Enter. The UI advertises Alt+Enter.
    pub fn shift_enter_unavailable(&self) -> bool {
        let is_vte = self.is_vte_based(); // WHY: central helper + version gating
        if is_vte {
            return match self
                .vte_version
                .as_deref()
                .and_then(|v| v.parse::<u32>().ok())
            {
                Some(ver) => ver < 8200,
                // The brand is Vte but there is no parseable version; conservatively assume old
                None => true,
            };
        }

        // VS Code / xterm.js and its forks: KKP is never negotiated, so Shift+Enter arrives as a bare CR indistinguishable from Enter
        if matches!(
            self.brand,
            TerminalName::VsCode
                | TerminalName::Cursor
                | TerminalName::Windsurf
                | TerminalName::Zed
        ) {
            return true;
        }

        // Consult env_brand: Windows refines Unknown to WindowsTerminal, but bare ConHost must still advertise Alt+Enter.
        if self.env_brand.is_capability_unclassified()
            && self.multiplexer == MultiplexerKind::Undetected
        {
            return true;
        }

        false
    }

    /// Broader than [`Self::shift_enter_unavailable`]: SSH and multiplexers that drop
    /// extended Enter (old tmux, `extended-keys off`, GNU screen) collapse Shift+Enter
    /// even when the brand-first [`Self::kitty_skip_reason`] reports a terminal reason.
    pub fn prefer_alt_enter_newline(&self) -> bool {
        self.shift_enter_unavailable()
            || self.is_ssh
            || self.kitty_multiplexer_skip_reason().is_some()
    }

    /// Without KKP, Ctrl+. is not a C0 control and collapses to `.`. Follows [`Self::kitty_skip_reason`] so mux skips stay aligned.
    pub fn ctrl_dot_unreliable(&self) -> bool {
        self.kitty_skip_reason().is_some()
    }

    /// Returns the reason to skip hyperlink (OSC 8) emission, or `None` if the environment is compatible.
    ///
    /// Terminal-emulator reasons take precedence over multiplexer reasons so the user is pointed at the deeper cause.
    pub fn hyperlink_skip_reason(&self) -> Option<&'static str> {
        let caps = self.hyperlink_capabilities();
        let is_tmux_3_4_later = self.is_tmux_version_or_later(3, 4);
        if caps.osc8 == Osc8Support::HostileParser {
            return Some("apple_terminal");
        }
        if caps.osc8 == Osc8Support::Unsupported {
            return Some("unsupported_terminal");
        }
        // VTE < 0.50.4 (version int < 5004) does not handle OSC 8 cleanly.
        // This check runs before the multiplexer check
        // That way a user on old VTE inside old tmux is pointed at the VTE upgrade rather than chasing tmux config
        if let Some(ref vte_ver) = self.vte_version
            && let Ok(ver_int) = vte_ver.parse::<u32>()
            && ver_int < 5004
        {
            return Some("vte_old");
        }
        if caps.osc8 == Osc8Support::Unknown {
            return Some("unknown_terminal");
        }
        if self.multiplexer == MultiplexerKind::Screen {
            return Some("screen");
        }
        if self.multiplexer == MultiplexerKind::Tmux && !is_tmux_3_4_later {
            return Some("tmux_old");
        }
        None
    }

    /// Returns `true` if the detected tmux version is `major`.`minor` or later.
    pub fn is_tmux_version_or_later(&self, major: u32, minor: u32) -> bool {
        match self
            .tmux_version
            .as_deref()
            .and_then(parse_tmux_major_minor)
        {
            Some((maj, min)) => (maj, min) >= (major, minor),
            // Unknown version, so conservatively assume old
            None => false,
        }
    }

    /// Returns `true` if `TERM_PROGRAM_VERSION` is `major`.`minor` or later.
    /// Returns `false` when the env var is absent or unparseable.
    pub fn is_term_program_version_or_later(&self, major: u32, minor: u32) -> bool {
        match self
            .term_program_version
            .as_deref()
            .and_then(parse_semver_major_minor)
        {
            Some((maj, min)) => (maj, min) >= (major, minor),
            None => false,
        }
    }

    /// The best available terminal version and the source that reported it.
    ///
    /// Not pure: the DA2 arm reads process-global probe state, so env-precedence tests hold only while no reply has been recorded in the process.
    pub fn term_version(&self) -> (String, TermVersionSource) {
        term_version::best_term_version(da2::detected(), self.env_term_version.as_ref())
    }

    /// Extract a flat snapshot of terminal details for telemetry.
    pub fn telemetry_snapshot(&self) -> distill_telemetry::events::TerminalTelemetry {
        let os = crate::host::HostOs::current();
        let server = crate::host::DisplayServer::current();
        let kb = self.keyboard_capabilities();
        let route = crate::clipboard::clipboard_route();
        let (term_version, term_version_source) = self.term_version();
        distill_telemetry::events::TerminalTelemetry {
            brand: self.brand.to_string(),
            multiplexer: self.multiplexer.to_string(),
            is_ssh: self.is_ssh,
            is_byobu: self.is_byobu(),
            term_var: self.term_var_or_na().to_owned(),
            host_os: os.to_string(),
            display_server: server.to_string(),
            modifier_cmd_fate: kb.modifier_delivery.cmd.to_string(),
            modifier_opt_fate: kb.modifier_delivery.opt.to_string(),
            enter_modifier_fate: kb.enter_modifier.to_string(),
            tmux_version: self.tmux_version_or_na().to_owned(),
            xtversion: xtversion::detected().unwrap_or("").to_owned(),
            term_version,
            term_version_source: term_version_source.to_string(),
            kitty_event_types_withheld: kitty_event_types_withheld(),
            hyperlink_osc8: self.hyperlink_capabilities().osc8.to_string(),
            hyperlink_skip_reason: self.hyperlink_skip_reason().unwrap_or("none").to_owned(),
            clipboard_route: route.to_string(),
            clipboard_native_tool: distill_shared::clipboard::native_tool_name().to_owned(),
            clipboard_data_control: crate::clipboard::wayland_data_control_label().to_owned(),
        }
    }

    /// Extract terminal info for feedback submissions.
    pub fn feedback_info(&self) -> distill_shared::session::FeedbackTerminalInfo {
        use distill_shared::session::FeedbackTerminalInfo;
        // XTVERSION self-report lets feedback triage identify the terminal even when env detection failed (e.g. over SSH).
        let brand = match xtversion::detected() {
            Some(v) if self.brand == TerminalName::Unknown => format!("Unknown (XTVERSION: {v})"),
            _ => self.brand.to_string(),
        };
        // Raw and unlabeled by design: no provenance, and no rewrite of DA2's library version into an Alacritty release number
        let (term_version, _source) = self.term_version();
        FeedbackTerminalInfo {
            brand,
            multiplexer: self.multiplexer.to_string(),
            is_ssh: self.is_ssh,
            is_byobu: self.is_byobu(),
            term_var: self.term_var_or_na().to_owned(),
            term_version: (!term_version.is_empty()).then_some(term_version),
            tmux_version: if self.is_tmux_backed() {
                self.tmux_version.clone()
            } else {
                None
            },
            hyperlink_osc8_support: Some(self.hyperlink_capabilities().osc8.to_string()),
            clipboard_route: Some(crate::clipboard::clipboard_route().to_string()),
            clipboard_native_tool: Some(distill_shared::clipboard::native_tool_name().to_owned()),
            display_server: Some(crate::host::DisplayServer::current().to_string()),
        }
    }
}

static TERMINAL_CONTEXT: OnceLock<TerminalContext> = OnceLock::new();

/// Computed once from the process environment. Preferred entry for multiplexer or Byobu info.
pub fn terminal_context() -> &'static TerminalContext {
    TERMINAL_CONTEXT.get_or_init(detect_terminal_context)
}

/// Detect terminal environment facts without any live tmux subprocesses.
///
/// Standalone diagnostics use this so an unhealthy tmux server cannot block before the diagnostic runner can report unavailable evidence.
pub fn standalone_terminal_context() -> TerminalContext {
    standalone_terminal_context_from_env(&collect_process_env(), HostOs::current())
}

fn standalone_terminal_context_from_env(
    env: &HashMap<String, String>,
    host: HostOs,
) -> TerminalContext {
    let mut ctx = build_terminal_context_from_env(env);
    ctx.brand = refine_unknown_brand_for_host(ctx.brand, host);
    ctx
}

/// Build a [`TerminalContext`] from the current process environment.
fn detect_terminal_context() -> TerminalContext {
    let env = collect_process_env();
    // Brand is usually Unknown in tmux and over SSH. tmux -g global env is the server's first client, not the current one.
    let mut ctx = build_terminal_context_from_env(&env);
    ctx.brand = refine_unknown_brand_for_host(ctx.brand, HostOs::current());
    if ctx.is_tmux_backed() {
        ctx.tmux_version = detect_tmux_version();
        ctx.tmux_extended_keys = detect_tmux_extended_keys();
    }
    ctx
}

/// Collect the process environment into a `HashMap` for the pure helpers.
fn collect_process_env() -> HashMap<String, String> {
    crate::host::collect_unicode_env()
}

/// Look up a key in the env map, returning `Some` for non-empty values.
fn env_get<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(|v| v.as_str()).filter(|v| !v.is_empty())
}

fn is_official_vscode_remote_askpass(path: &str) -> bool {
    std::path::Path::new(path).components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name)
                if name == ".vscode-server" || name == ".vscode-server-insiders"
        )
    })
}

/// A new marker must also go into the PTY harness strip list so the host terminal cannot leak into tests.
pub fn detect_terminal_brand_from_env(env: &HashMap<String, String>) -> TerminalName {
    // Fork-specific markers first: they set TERM_PROGRAM=vscode, and they survive SSH/tmux where TERM_PROGRAM does not.
    if env_get(env, "CURSOR_TRACE_ID").is_some() {
        return TerminalName::Cursor;
    }
    if let Some(askpass) = env_get(env, "VSCODE_GIT_ASKPASS_MAIN") {
        let askpass_lower = askpass.to_ascii_lowercase();
        if askpass_lower.contains("cursor") {
            return TerminalName::Cursor;
        }
        if askpass_lower.contains("windsurf") {
            return TerminalName::Windsurf;
        }
        // Pure VS Code remote agent injects this even without TERM_PROGRAM.
        return TerminalName::VsCode;
    }

    // TERM_PROGRAM is the most reliable signal.
    if let Some(term_program) = env_get(env, "TERM_PROGRAM")
        && let Some(name) = terminal_name_from_term_program(term_program)
    {
        return name;
    }

    // Both JediTerm engines share this marker; no capability query is safe (XTVERSION leaks). Must run before TERM_SESSION_ID or it false-positives as Apple Terminal.
    if let Some(te) = env_get(env, "TERMINAL_EMULATOR") {
        let te_lower = te.to_ascii_lowercase();
        if te_lower.contains("jetbrains") || te_lower.contains("jediterm") {
            return TerminalName::JetBrains;
        }
    }

    // WezTerm-specific variable.
    if env_get(env, "WEZTERM_VERSION").is_some() {
        return TerminalName::WezTerm;
    }

    // iTerm2. LC_TERMINAL=iTerm2 survives SSH (SendEnv/AcceptEnv LC_*) where ITERM_SESSION_ID / TERM_PROGRAM do not.
    if env_get(env, "ITERM_SESSION_ID").is_some()
        || env_get(env, "ITERM_PROFILE").is_some()
        || env_get(env, "LC_TERMINAL").is_some_and(|v| v.eq_ignore_ascii_case("iterm2"))
    {
        return TerminalName::Iterm2;
    }

    // Apple Terminal.
    if env_get(env, "TERM_SESSION_ID").is_some() {
        return TerminalName::AppleTerminal;
    }

    // Kitty.
    if env_get(env, "KITTY_WINDOW_ID").is_some() {
        return TerminalName::Kitty;
    }
    if let Some(term) = env_get(env, "TERM")
        && term.contains("kitty")
    {
        return TerminalName::Kitty;
    }

    // Alacritty.
    if env_get(env, "ALACRITTY_SOCKET").is_some() {
        return TerminalName::Alacritty;
    }
    if let Some(term) = env_get(env, "TERM")
        && term == "alacritty"
    {
        return TerminalName::Alacritty;
    }

    // Rio.
    if let Some(term) = env_get(env, "TERM")
        && term == "rio"
    {
        return TerminalName::Rio;
    }

    // foot (Wayland-native, Linux-only). foot sets no unique env var, only TERM.
    // Match its exact terminfo names to avoid over-matching
    if let Some(term) = env_get(env, "TERM")
        && matches!(term, "foot" | "foot-extra" | "foot-direct")
    {
        return TerminalName::Foot;
    }

    // Terminator (Python/GTK, VTE-based) exports TERMINATOR_UUID on every child process
    // Check before the generic VTE_VERSION fallback so it is identified specifically rather than as a generic VTE terminal (it sets both)
    if env_get(env, "TERMINATOR_UUID").is_some() {
        return TerminalName::Terminator;
    }

    // VTE-based terminals (GNOME Terminal, kgx, Tilix, etc.).
    if env_get(env, "VTE_VERSION").is_some() {
        return TerminalName::Vte;
    }

    // Windows Terminal sets WT_SESSION (a GUID) on every child process.
    if env_get(env, "WT_SESSION").is_some() {
        return TerminalName::WindowsTerminal;
    }

    TerminalName::Unknown
}

/// WT's DefTerm handoff omits WT_SESSION/TERM_PROGRAM. Raw detection stays in `env_brand`. WSL is unaffected.
fn refine_unknown_brand_for_host(brand: TerminalName, host: HostOs) -> TerminalName {
    if brand == TerminalName::Unknown && host == HostOs::Windows {
        TerminalName::WindowsTerminal
    } else {
        brand
    }
}

/// The core of [`TerminalContext::mouse_reporting_leaks_as_raw_text`].
/// It is split out so tests can drive the brand/host matrix without the real host.
fn mouse_reporting_leaks(brand: TerminalName, host: HostOs) -> bool {
    brand == TerminalName::JetBrains && host == HostOs::Windows
}

/// `BYOBU_BACKEND` when set; otherwise infer tmux vs screen from other Byobu markers plus `TMUX`/`STY`.
pub fn detect_byobu_from_env(env: &HashMap<String, String>) -> Option<ByobuBackend> {
    let has_byobu_backend = env_get(env, "BYOBU_BACKEND").is_some();
    let has_byobu_config = env_get(env, "BYOBU_CONFIG_DIR").is_some();
    let has_byobu_distro = env_get(env, "BYOBU_DISTRO").is_some();

    if !has_byobu_backend && !has_byobu_config && !has_byobu_distro {
        return None;
    }

    // Explicit backend marker takes precedence.
    if let Some(backend) = env_get(env, "BYOBU_BACKEND") {
        return match backend.to_ascii_lowercase().as_str() {
            "tmux" => Some(ByobuBackend::Tmux),
            "screen" => Some(ByobuBackend::Screen),
            // Unknown backend string, so fall through to inference
            _ => infer_byobu_backend_from_mux_markers(env),
        };
    }

    // No explicit BYOBU_BACKEND but other markers are present, so infer from the mux markers
    infer_byobu_backend_from_mux_markers(env)
}

/// When Byobu markers exist but `BYOBU_BACKEND` is absent or unrecognised, infer the backend from which multiplexer markers are set.
fn infer_byobu_backend_from_mux_markers(env: &HashMap<String, String>) -> Option<ByobuBackend> {
    let has_tmux = env_get(env, "TMUX").is_some();
    let has_sty = env_get(env, "STY").is_some();

    match (has_tmux, has_sty) {
        (true, _) => Some(ByobuBackend::Tmux),
        (false, true) => Some(ByobuBackend::Screen),
        // Byobu markers with neither TMUX nor STY, so the backend cannot be determined
        (false, false) => None,
    }
}

/// Explicit Byobu backend, then TMUX, then ZELLIJ, then STY. herdr/cmux only if no real mux won; `HERDR_ENV` beats inherited `CMUX_*`.
pub fn detect_multiplexer_from_env(env: &HashMap<String, String>) -> MultiplexerKind {
    let byobu = detect_byobu_from_env(env);

    // If Byobu is detected with an explicit backend, let that win.
    if let Some(backend) = byobu {
        match backend {
            ByobuBackend::Tmux => return MultiplexerKind::Tmux,
            ByobuBackend::Screen => return MultiplexerKind::Screen,
            ByobuBackend::Unknown => {} // fall through to standard markers
        }
    }

    // Nested real muxes win. A herdr daemon started from tmux freezes TMUX into every pane; classifying that as herdr wraps OSC 52 in DCS that herdr prints as text.
    if env_get(env, "TMUX").is_some() {
        return MultiplexerKind::Tmux;
    }
    if env_get(env, "ZELLIJ").is_some() || env_get(env, "ZELLIJ_SESSION_NAME").is_some() {
        return MultiplexerKind::Zellij;
    }
    if env_get(env, "STY").is_some() {
        return MultiplexerKind::Screen;
    }
    // herdr sets HERDR_ENV=1 in every pane, overrides TERM to xterm-256color and never sets TERM_PROGRAM
    // This marker is its only documented, stable signal
    if env_get(env, "HERDR_ENV").is_some() {
        return MultiplexerKind::Herdr;
    }
    // cmux sets non-empty CMUX_SOCKET_PATH / CMUX_PANEL_ID / CMUX_BUNDLE_ID; CMUX_SOCKET may be present but empty (env_get filters empties)
    if env_get(env, "CMUX_SOCKET_PATH").is_some()
        || env_get(env, "CMUX_PANEL_ID").is_some()
        || env_get(env, "CMUX_BUNDLE_ID").is_some()
    {
        return MultiplexerKind::Cmux;
    }

    MultiplexerKind::Undetected
}

/// Extract tmux client metadata from an injected environment map.
pub fn detect_tmux_meta_from_env(env: &HashMap<String, String>) -> TmuxClientMeta {
    TmuxClientMeta {
        tmux_env: env_get(env, "TMUX").map(|s| s.to_owned()),
        tmux_pane: env_get(env, "TMUX_PANE").map(|s| s.to_owned()),
    }
}

/// Build a full [`TerminalContext`] from an injected environment map.
///
/// This is the primary pure helper: call it with a controlled env map in tests to exercise the full detection matrix without ambient host state.
pub fn build_terminal_context_from_env(env: &HashMap<String, String>) -> TerminalContext {
    let brand = detect_terminal_brand_from_env(env);
    let multiplexer = detect_multiplexer_from_env(env);
    let byobu = detect_byobu_from_env(env);
    let embedded_editor = embedded_editor_from_env(env);
    let tmux_meta = if multiplexer == MultiplexerKind::Tmux {
        detect_tmux_meta_from_env(env)
    } else {
        TmuxClientMeta::default()
    };
    let is_ssh = env_get(env, "SSH_CONNECTION").is_some()
        || env_get(env, "SSH_TTY").is_some()
        || env_get(env, "SSH_CLIENT").is_some();
    let is_official_vscode_remote = is_ssh
        && env_get(env, "VSCODE_GIT_ASKPASS_MAIN").is_some_and(is_official_vscode_remote_askpass);
    let term_var = env_get(env, "TERM").map(|s| s.to_owned());
    let vte_version = env_get(env, "VTE_VERSION").map(|s| s.to_owned());
    // SSH strips TERM_PROGRAM_VERSION; iTerm2 LC_TERMINAL_VERSION survives.
    let term_program_version = env_get(env, "TERM_PROGRAM_VERSION")
        .or_else(|| env_get(env, "LC_TERMINAL_VERSION"))
        .map(|s| s.to_owned());
    // Resolved here: the brand each version var must corroborate is in hand.
    let env_term_version = term_version::detect_env_term_version(env, brand);
    let term_features = env_get(env, "TERM_FEATURES").map(|s| s.to_owned());

    TerminalContext {
        brand,
        env_brand: brand,
        multiplexer,
        byobu,
        embedded_editor,
        tmux_meta,
        is_ssh,
        is_official_vscode_remote,
        term_var,
        tmux_version: None,
        vte_version,
        tmux_extended_keys: None,
        term_program_version,
        term_features,
        env_term_version,
    }
}

fn terminal_name_from_term_program(value: &str) -> Option<TerminalName> {
    let normalized: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect();

    match normalized.as_str() {
        "appleterminal" => Some(TerminalName::AppleTerminal),
        "ghostty" => Some(TerminalName::Ghostty),
        "iterm" | "iterm2" | "itermapp" => Some(TerminalName::Iterm2),
        "warp" | "warpterminal" => Some(TerminalName::WarpTerminal),
        "vscode" => Some(TerminalName::VsCode),
        "wezterm" => Some(TerminalName::WezTerm),
        "kitty" => Some(TerminalName::Kitty),
        "alacritty" => Some(TerminalName::Alacritty),
        "rio" => Some(TerminalName::Rio),
        "terminator" => Some(TerminalName::Terminator),
        "zed" => Some(TerminalName::Zed),
        "grokdesktop" => Some(TerminalName::GrokDesktop),
        "windowsterminal" => Some(TerminalName::WindowsTerminal),
        "otty" => Some(TerminalName::Otty),
        _ => None,
    }
}

/// `[terminal] alt_screen`, overridden by `--no-alt-screen`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AltScreenMode {
    /// Automatic: fullscreen in plain terminals and normal tmux, inline in tmux control mode and Zellij.
    #[default]
    Auto,
    /// Always enter the alternate screen, even in environments where auto would disable it (e.g. tmux control mode, Zellij).
    Always,
    /// Never enter the alternate screen; always run inline.
    Never,
}

/// Detect whether the current tmux session is in control mode.
///
/// Returns `false` when not inside tmux or when the query fails.
pub fn detect_tmux_control_mode(ctx: &TerminalContext) -> bool {
    if ctx.multiplexer != MultiplexerKind::Tmux {
        return false;
    }
    tmux_probe::query_control_mode()
        .into_option()
        .unwrap_or(false)
}

/// Detect the tmux server version by running `tmux -V`.
pub fn detect_tmux_version() -> Option<String> {
    tmux_probe::query_version().into_option()
}

/// Trimmed value of tmux's global `extended-keys` option.
pub fn detect_tmux_extended_keys() -> Option<String> {
    tmux_probe::query_option("extended-keys").into_option()
}

/// Parse major.minor from a plain semver-ish string like `"3.6.0"` or `"1.2"`.
fn parse_semver_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Parse the major.minor version from a tmux version string like `"tmux 3.4"` or `"tmux 3.3a"`.
/// Returns `None` on unrecognized formats.
fn parse_tmux_major_minor(version: &str) -> Option<(u32, u32)> {
    let rest = version.strip_prefix("tmux ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor_str = parts.next()?;
    let minor_end = minor_str
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(minor_str.len());
    let minor: u32 = minor_str.get(..minor_end)?.parse().ok()?;
    Some((major, minor))
}

/// CLI `--no-alt-screen`, then config Always/Never/Auto. Auto is inline in Zellij and tmux control mode, else fullscreen.
pub fn determine_alt_screen_policy(
    cli_no_alt_screen: bool,
    config_mode: AltScreenMode,
    ctx: &TerminalContext,
    is_control_mode: bool,
) -> bool {
    // CLI override has highest precedence.
    if cli_no_alt_screen {
        return false;
    }

    match config_mode {
        AltScreenMode::Always => true,
        AltScreenMode::Never => false,
        AltScreenMode::Auto => {
            // Auto-disable in Zellij.
            if ctx.multiplexer == MultiplexerKind::Zellij {
                return false;
            }
            // Auto-disable in tmux control mode.
            if ctx.is_tmux_backed() && is_control_mode {
                return false;
            }
            // Default: fullscreen.
            true
        }
    }
}
