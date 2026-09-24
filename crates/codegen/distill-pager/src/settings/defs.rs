// Modified for Distill by Samuel Fajreldines, 2026.
//! Default settings catalog: every user-tunable preference registered in the settings modal.
//!
//! Defaults come from `UiConfig::default()` for SHELL/SHARED settings.
//! The `defaults_match_ui_config_default` test enforces this.

use super::registry::{
    DynamicEnumSource, EnumChoice, SettingCategory, SettingKind, SettingMeta, SettingOwner,
};
use crate::appearance::ScrollMode;
use crate::appearance::TextSelection;
use crate::appearance::permission_cursor::DefaultSelectedPermission;

use distill_shell::agent::config::UiConfig;
use distill_shell::util::config::DISPLAY_REFRESH_DEFAULT_AUTO_CADENCE_ENABLED;
use distill_tools::implementations::distill::ask_user_question;

// Int bounds for `max_thoughts_width`. `pub(crate)` so the dispatcher's clamp and the shell helper's defensive
// clamp share these bounds.
pub(crate) const MAX_THOUGHTS_WIDTH_MIN: i64 = 40;
pub(crate) const MAX_THOUGHTS_WIDTH_MAX: i64 = 500;

/// Registry key for `max_thoughts_width`; it is shared between the registry definition and the live-wrap-preview gate in the int stepper.
pub(crate) const MAX_THOUGHTS_WIDTH_KEY: &str = "max_thoughts_width";

// Theme choice catalogs. Canonical names MUST match `ThemeKind::display_name()`. The catalogs are shared by
// `theme`, `auto_dark_theme`, and `auto_light_theme`; the auto-* sub-pickers drop "auto" to avoid a circular
// reference.

/// Full theme catalog including the "auto" meta-variant; only `theme` uses it.
const THEME_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "auto",
        display: "Auto",
        description: "Follow system dark/light appearance.",
    },
    EnumChoice {
        canonical: "groknight",
        display: "Grok Night",
        description: "Neutral dark with magenta accent.",
    },
    EnumChoice {
        canonical: "grokday",
        display: "Grok Day",
        description: "Light theme for bright environments.",
    },
    EnumChoice {
        canonical: "tokyonight",
        display: "Tokyo Night",
        description: "Dark + blue-tinted; needs truecolor.",
    },
    // The display name is ASCII "Rose Pine Moon" (not "Rosé") for cross-terminal compatibility
    EnumChoice {
        canonical: "rosepine-moon",
        display: "Rose Pine Moon",
        description: "Muted dark with mauve accents; needs truecolor.",
    },
    EnumChoice {
        canonical: "oscura-midnight",
        display: "Oscura Midnight",
        description: "Deep dark with warm accents; needs truecolor.",
    },
    EnumChoice {
        canonical: "terminal",
        display: "Terminal",
        description: "Terminal's own background and text colors.",
    },
];

// Permission-mode catalog. Persisted values map onto runtime flags: "always-approve" ↔ yolo_mode = true
// (auto-approve all). `supports_preview: false` because toggling YOLO drains the permission queue (unsafe for
// per-keystroke preview).

// Choice order runs safe to unsafe: Default, Ask, Auto, Always approve
// "Always approve" at the end creates a speed bump against accidental selection
const PERMISSION_MODE_CHOICES: &[EnumChoice] = &[
    // "default" is the agent's default behavior: the same as "ask" at runtime, but distinct on disk and in the modal indicator
    EnumChoice {
        canonical: "default",
        display: "Default",
        description: "Use the agent's default permission behavior (currently equivalent to Ask).",
    },
    EnumChoice {
        canonical: "ask",
        display: "Ask",
        description: "Prompt for permission before tool actions.",
    },
    EnumChoice {
        canonical: "auto",
        display: "Auto",
        description: "LLM classifier approves safe tools; dangerous actions may still prompt or deny.",
    },
    EnumChoice {
        canonical: "always-approve",
        display: "Always approve",
        description: "Auto-approve every tool action. Skips ALL permission prompts.",
    },
];

// Coding-data-sharing catalog. Two choices only: the pager has no `Option`/`Unset` representation for this field.
// `supports_preview: false` because toggling fires an async ACP call that can fail. Commit on Enter only.

// The setting's own description carries the full explanation, so the choices are bare labels; an empty description collapses each to a single line
const CODING_DATA_SHARING_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "opt-in",
        display: "Opt in",
        description: "",
    },
    EnumChoice {
        canonical: "opt-out",
        display: "Opt out",
        description: "",
    },
];

// Plan-mode catalog. `Ask` mode is not exposed here; it is only reachable via Shift+Tab. `supports_preview: false`
// because toggling fires an ACP request that gates tool dispatch. Commit on Enter only.

// Default-selected-permission catalog. `always_allow_all_sessions` (the effective default) lands the cursor on the
// "Always allow on all sessions" (enable-always-approve) row. `supports_preview: false` because permission prompts
// aren't open in the modal background, so there is nothing to live-preview.

// Order matches the live permission prompt rendering (YOLO, always-allow, allow-once, reject) so the picker mirrors the real prompt
// Canonicals and display labels come from `DefaultSelectedPermission`, the single source of truth
// This table therefore can never drift from the parser, the dispatch toast, or the cursor logic
const DEFAULT_SELECTED_PERMISSION_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: DefaultSelectedPermission::AlwaysAllowAllSessions.as_canonical(),
        display: DefaultSelectedPermission::AlwaysAllowAllSessions.display(),
        description: "",
    },
    EnumChoice {
        canonical: DefaultSelectedPermission::AllowCommandAlways.as_canonical(),
        display: DefaultSelectedPermission::AllowCommandAlways.display(),
        description: "",
    },
    EnumChoice {
        canonical: DefaultSelectedPermission::AllowOnce.as_canonical(),
        display: DefaultSelectedPermission::AllowOnce.display(),
        description: "",
    },
    EnumChoice {
        canonical: DefaultSelectedPermission::Reject.as_canonical(),
        display: DefaultSelectedPermission::Reject.display(),
        description: "",
    },
];

const PLAN_MODE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "off",
        display: "Off",
        description: "Agent runs tools and edits files directly (default).",
    },
    EnumChoice {
        canonical: "on",
        display: "On",
        description: "Agent summarises a plan and asks for approval before running tools.",
    },
];

// Mid-turn follow-up routing. SHARED-owned, persisted to `[ui].follow_up_behavior`.
// Canonicals match `FollowUpBehavior::as_canonical`
const FOLLOW_UP_BEHAVIOR_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "queue",
        display: "Queue",
        description: "Hold follow-ups until the current turn finishes.",
    },
    EnumChoice {
        canonical: "steer",
        display: "Steer",
        description: "Inject follow-ups mid-turn at the next tool or model step.",
    },
];

// Mermaid-rendering catalog. SHELL-owned: persisted to `[ui].render_mermaid`. A pager-side process-wide cache
// mirror (`appearance::cache::*_render_mermaid`) serves the render hot path. Canonicals match
// `RenderMermaid::as_canonical`.

const RENDER_MERMAID_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "auto",
        display: "Auto",
        description: "Show diagrams with a clickable row to open/copy the rendered image.",
    },
    EnumChoice {
        canonical: "on",
        display: "On",
        description: "Same as auto: always show the clickable affordance row.",
    },
    EnumChoice {
        canonical: "off",
        display: "Off",
        description: "Always show the raw Mermaid source as a code block.",
    },
];

// Scroll-input catalog. SHELL-owned, persisted to `[ui].scroll_mode`.
// Canonical strings match `ScrollMode::as_canonical` (pinned by test).
const SCROLL_MODE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: ScrollMode::Auto.as_canonical(),
        display: "Auto-detect",
        description: "Detect wheel vs trackpad per gesture from event timing. Default.",
    },
    EnumChoice {
        canonical: ScrollMode::Wheel.as_canonical(),
        display: "Mouse wheel",
        description: "Always treat scrolling as wheel notches (fixed lines per tick).",
    },
    EnumChoice {
        canonical: ScrollMode::Trackpad.as_canonical(),
        display: "Trackpad",
        description: "Always treat scrolling as a trackpad (fractional accumulation).",
    },
];

const TEXT_SELECTION_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: TextSelection::Flash.as_canonical(),
        display: "Flash after copy",
        description: "Brief highlight on mouse-up, then clear. Double-click toggles fold. Default.",
    },
    EnumChoice {
        canonical: TextSelection::Hold.as_canonical(),
        display: "Hold until dismissed",
        description: "Keep the selection visible until Esc, click, or scroll. Double-click toggles fold.",
    },
    EnumChoice {
        canonical: TextSelection::WordSelect.as_canonical(),
        display: "Word select (terminal-like)",
        description: "Double-click selects & copies a word, triple-click a paragraph; selection stays until dismissed.",
    },
];

// Hunk-tracker-mode catalog. SHELL-owned, persisted to `[ui].hunk_tracker_mode`.
// `disabled` is accepted as an alias for `off` at parse time but not shown as a choice
const HUNK_TRACKER_MODE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "agent_only",
        display: "Agent only",
        description: "Track only files the agent edits.",
    },
    EnumChoice {
        canonical: "all_dirty",
        display: "All dirty",
        description: "Track every git-dirty file, including external edits.",
    },
    EnumChoice {
        canonical: "off",
        display: "Off",
        description: "Disable hunk tracking entirely (default). Also disables LOC tracking.",
    },
];

const SCREEN_MODE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "fullscreen",
        display: "Fullscreen",
        description: "Open plain grok in the standard fullscreen TUI. Default when unset.",
    },
    EnumChoice {
        canonical: "minimal",
        display: "Minimal",
        description: "Open plain grok in scrollback-native (minimal) mode.",
    },
];

// Voice-capture-mode catalog. Alacritty 0.14 and earlier negotiates the protocol yet never reports releases, so
// hold stays hidden there.
const VOICE_CAPTURE_MODE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "toggle",
        display: "Toggle",
        description: "Ctrl+Space / F8 starts dictation; press again (or Esc/Enter) to stop.",
    },
    EnumChoice {
        canonical: "hold",
        display: "Hold to talk",
        description: "Hold Ctrl+Space / F8 to record, release to stop. Needs a Kitty-protocol terminal.",
    },
];

// Voice STT language choices for the settings modal. Concrete codes must match `distill_voice::STT_LANGUAGES`,
// the official Grok STT catalog. `auto` is client-only; the voice crate resolves it to a concrete code before the
// STT handshake.
const VOICE_STT_LANGUAGE_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "en",
        display: "English",
        description: "",
    },
    EnumChoice {
        canonical: "auto",
        display: "System",
        description: "Use the system locale when it is a supported STT language; otherwise English.",
    },
    EnumChoice {
        canonical: "ar",
        display: "Arabic",
        description: "",
    },
    EnumChoice {
        canonical: "cs",
        display: "Czech",
        description: "",
    },
    EnumChoice {
        canonical: "da",
        display: "Danish",
        description: "",
    },
    EnumChoice {
        canonical: "nl",
        display: "Dutch",
        description: "",
    },
    EnumChoice {
        canonical: "fil",
        display: "Filipino",
        description: "",
    },
    EnumChoice {
        canonical: "fr",
        display: "French",
        description: "",
    },
    EnumChoice {
        canonical: "de",
        display: "German",
        description: "",
    },
    EnumChoice {
        canonical: "hi",
        display: "Hindi",
        description: "",
    },
    EnumChoice {
        canonical: "id",
        display: "Indonesian",
        description: "",
    },
    EnumChoice {
        canonical: "it",
        display: "Italian",
        description: "",
    },
    EnumChoice {
        canonical: "ja",
        display: "Japanese",
        description: "",
    },
    EnumChoice {
        canonical: "ko",
        display: "Korean",
        description: "",
    },
    EnumChoice {
        canonical: "mk",
        display: "Macedonian",
        description: "",
    },
    EnumChoice {
        canonical: "ms",
        display: "Malay",
        description: "",
    },
    EnumChoice {
        canonical: "fa",
        display: "Persian",
        description: "",
    },
    EnumChoice {
        canonical: "pl",
        display: "Polish",
        description: "",
    },
    EnumChoice {
        canonical: "pt",
        display: "Portuguese",
        description: "",
    },
    EnumChoice {
        canonical: "ro",
        display: "Romanian",
        description: "",
    },
    EnumChoice {
        canonical: "ru",
        display: "Russian",
        description: "",
    },
    EnumChoice {
        canonical: "es",
        display: "Spanish",
        description: "",
    },
    EnumChoice {
        canonical: "sv",
        display: "Swedish",
        description: "",
    },
    EnumChoice {
        canonical: "th",
        display: "Thai",
        description: "",
    },
    EnumChoice {
        canonical: "tr",
        display: "Turkish",
        description: "",
    },
    EnumChoice {
        canonical: "vi",
        display: "Vietnamese",
        description: "",
    },
];

/// Concrete-only theme catalog (excludes "auto"), used by both `auto_dark_theme` and `auto_light_theme`.
/// There is no dark/light filtering: the user can pair any theme with any system-appearance bucket.
const CONCRETE_THEME_CHOICES: &[EnumChoice] = &[
    EnumChoice {
        canonical: "groknight",
        display: "Grok Night",
        description: "Neutral dark with magenta accent.",
    },
    EnumChoice {
        canonical: "grokday",
        display: "Grok Day",
        description: "Light theme for bright environments.",
    },
    EnumChoice {
        canonical: "tokyonight",
        display: "Tokyo Night",
        description: "Dark + blue-tinted; needs truecolor.",
    },
    EnumChoice {
        canonical: "rosepine-moon",
        display: "Rose Pine Moon",
        description: "Muted dark with mauve accents; needs truecolor.",
    },
    EnumChoice {
        canonical: "oscura-midnight",
        display: "Oscura Midnight",
        description: "Deep dark with warm accents; needs truecolor.",
    },
    EnumChoice {
        canonical: "terminal",
        display: "Terminal",
        description: "Terminal's own background and text colors.",
    },
];

/// Child settings shown inside the "Show contextual hints" group sub-sheet. Keys match the `[ui.contextual_hints]`
/// serde fields. The namespace keeps them globally unique: bare `plan_mode` collides with the plan-mode enum row.
const CONTEXTUAL_HINTS_CHILDREN: &[&str] = &[
    "contextual_hints.undo",
    "contextual_hints.plan_mode",
    "contextual_hints.image_input",
    "contextual_hints.send_now",
    "contextual_hints.small_screen",
    "contextual_hints.word_select",
    "contextual_hints.export_copy",
    "contextual_hints.ssh_wrap",
];

/// Build the catalog; called once at process start via `SettingsRegistry::defaults()`.
pub fn default_settings() -> Vec<SettingMeta> {
    // The shell schema defaults are the registry's source of truth
    let ui_default = UiConfig::default();

    vec![
        SettingMeta {
            key: "compact_mode",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Compact mode",
            description: "Reduce padding around messages for more content density. \
                          Auto-enabled while the terminal is 20 rows or shorter.",
            keywords: &[
                "compact", "density", "padding", "tight", "small", "screen", "auto",
            ],
            kind: SettingKind::Bool {
                default: ui_default.compact_mode,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "screen_mode",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Default screen mode",
            description: "How plain grok opens next time: Fullscreen (default when unset) or \
                          Minimal. Writes [ui] screen_mode in config.toml. Restart required. \
                          Switch this session only with /minimal or /fullscreen.",
            keywords: &[
                "screen",
                "mode",
                "minimal",
                "fullscreen",
                "full",
                "scrollback",
                "native",
                "alt-screen",
                "render",
                "default",
            ],
            kind: SettingKind::Enum {
                default: "fullscreen",
                choices: SCREEN_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: true,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "show_timestamps",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Show timestamps",
            description: "Show clock time next to user messages and agent responses.",
            keywords: &["timestamps", "time", "clock", "date"],
            kind: SettingKind::Bool {
                // `Option<bool>`: `None` is treated as `true`
                default: ui_default.show_timestamps.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "show_timeline",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Timeline sidebar",
            description: "Per-turn tick rail in place of the scrollbar: hover previews a turn, click jumps to it.",
            keywords: &["timeline", "sidebar", "ticks", "turns", "navigator", "rail"],
            kind: SettingKind::Bool {
                // Single source: UiConfig::SHOW_TIMELINE_DEFAULT (opt-in).
                default: ui_default.show_timeline_enabled(),
            },
            restart_required: false,
            // Minimal mode has no interactive scrollback pane for the rail.
            hidden_in_minimal: true,
        },
        SettingMeta {
            key: "page_flip_on_send",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Snap prompt to top on send",
            description: "When you send a prompt, scroll it to the top of the screen so the \
                          response starts on a fresh page (default). Turn off to leave the scroll \
                          position unchanged when you send.",
            keywords: &[
                "page", "flip", "send", "prompt", "scroll", "top", "jump", "auto", "snap",
            ],
            kind: SettingKind::Bool {
                default: ui_default.page_flip_on_send_enabled(),
            },
            restart_required: false,
            hidden_in_minimal: true,
        },
        SettingMeta {
            key: "combine_queued_prompts",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shared,
            label: "Combine queued prompts",
            description: "Merge consecutive plain follow-ups into one model turn \
                          (TUI shows one bubble each). Stops at bash, slash commands, \
                          cron, expanded skills, image follow-ups, or a row under edit. \
                          Default off; applies on local drain and shell promote.",
            keywords: &["queue", "combine", "batch", "follow-up", "merge", "pending"],
            kind: SettingKind::Bool {
                default: ui_default.combine_queued_prompts.unwrap_or(false),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "follow_up_behavior",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shared,
            label: "Follow-up behavior",
            description: "What to do with messages you send while a turn is \
                          running. Queue waits for the turn to finish; Steer \
                          injects them mid-turn at the next tool batch or \
                          model step. Default: Queue.",
            keywords: &[
                "queue",
                "steer",
                "interject",
                "follow-up",
                "followup",
                "send",
                "immediate",
            ],
            kind: SettingKind::Enum {
                default: ui_default.follow_up_behavior(),
                choices: FOLLOW_UP_BEHAVIOR_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "confirm_before_rewind",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shared,
            label: "Confirm before rewind",
            description: "Ask before rewinding conversation history. Turn off to rewind \
                          immediately when you pick a turn.",
            keywords: &["rewind", "confirm", "undo", "history", "ask", "prompt"],
            kind: SettingKind::Bool {
                default: ui_default.confirm_before_rewind_enabled(),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            // The persisted key stays `simple_mode`
            // The user-facing label distinguishes the PROMPT vim-mode (this setting) from the scrollback `vim_mode` keybindings below
            key: "simple_mode",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Disable vim input mode",
            description: "Use plain readline-style input instead of vim keys in the prompt. Experimental.",
            keywords: &[
                "simple",
                "ascii",
                "minimal",
                "plain",
                "vim",
                "readline",
                "experimental",
                "editor",
                "input",
                "prompt",
            ],
            kind: SettingKind::Bool {
                // `Option<bool>`: `None` is treated as `true`
                default: ui_default.simple_mode.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].vim_mode` in config.toml.
        // Defaults to the same value main's `appearance::persist::VIM_MODE_DEFAULT` shipped with
        // Bundled next to `simple_mode` because they pair up: simple_mode controls the input editor's vim behaviour, vim_mode controls the scrollback's
        SettingMeta {
            key: "vim_mode",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Vim scrollback navigation",
            description: "Enable vim keys (h/j/k/l, gg/G, /) for navigating the scrollback. Does not affect the input prompt.",
            keywords: &[
                "vim",
                "scrollback",
                "navigation",
                "hjkl",
                "keys",
                "keybindings",
                "scroll",
            ],
            kind: SettingKind::Bool {
                default: ui_default.vim_mode.unwrap_or(false),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // --- theme and auto themes -------------------------------------------
        SettingMeta {
            key: "theme",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Theme",
            description: "Color theme for the pager UI.",
            keywords: &[
                "theme",
                "color",
                "colour",
                "palette",
                "appearance",
                "dark",
                "light",
            ],
            kind: SettingKind::Enum {
                // `Option<String>`: `None` resolves to "groknight"
                default: "groknight",
                choices: THEME_CHOICES,
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: true,
        },
        SettingMeta {
            key: "auto_dark_theme",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Auto dark theme",
            description: "Theme to use when the system is in dark mode (only with theme=auto).",
            keywords: &["auto", "dark", "theme", "system", "appearance", "night"],
            kind: SettingKind::Enum {
                // `Option<String>`: `None` falls back to "groknight"
                default: "groknight",
                choices: CONCRETE_THEME_CHOICES,
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: true,
        },
        SettingMeta {
            key: "auto_light_theme",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Auto light theme",
            description: "Theme to use when the system is in light mode (only with theme=auto).",
            keywords: &["auto", "light", "theme", "system", "appearance", "day"],
            kind: SettingKind::Enum {
                // `Option<String>`: `None` falls back to "grokday"
                default: "grokday",
                choices: CONCRETE_THEME_CHOICES,
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: true,
        },
        // SHELL-owned: persisted to `[ui].render_mermaid`, with a pager-side process-wide cache mirror (like `vim_mode`)
        // The default is pinned to "auto" by `defaults_match_ui_config_default`
        SettingMeta {
            key: "render_mermaid",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Render Mermaid diagrams",
            description: "How ```mermaid code blocks are shown: auto/on add a clickable row to \
                          open the rendered diagram; off shows the raw source.",
            keywords: &[
                "mermaid",
                "diagram",
                "diagrams",
                "render",
                "flowchart",
                "graph",
                "chart",
            ],
            kind: SettingKind::Enum {
                default: "auto",
                choices: RENDER_MERMAID_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // Security-relevant: "always-approve" bypasses all permission prompts.
        // The modal reads live state from `PagerLocalSnapshot.yolo_mode` (not `ui.permission_mode`) to reflect Ctrl+O toggles immediately
        SettingMeta {
            key: "permission_mode",
            category: SettingCategory::Agent,
            owner: SettingOwner::Shell,
            label: "Permission mode",
            description: "Default uses the agent's built-in behavior; \
                          Ask prompts for each tool action; \
                          Auto uses an LLM classifier for risky tools; \
                          Always approve grants all permissions automatically.",
            keywords: &[
                "permission",
                "approve",
                "yolo",
                "agent",
                "always",
                "ask",
                "auto",
                "classifier",
                "tool",
                "danger",
            ],
            kind: SettingKind::Enum {
                default: "ask",
                choices: PERMISSION_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned `[ui].remember_tool_approvals`. It gates the per-tool "Always allow …" prompt options.
        // `restart_required` because the value is resolved at permission-manager spawn (also fed by env/requirements/managed/remote settings)
        SettingMeta {
            key: "remember_tool_approvals",
            category: SettingCategory::Agent,
            owner: SettingOwner::Shell,
            label: "Remember tool approvals",
            description: "Show \"Always allow\" options in permission prompts so you can stop \
                          being re-asked about a specific command or tool. Applies in ask and \
                          auto; Always-approve still skips all prompts. Restart required.",
            keywords: &[
                "permission",
                "approve",
                "approval",
                "always",
                "allow",
                "remember",
                "tool",
                "command",
                "kubectl",
                "ask",
                "again",
                "whitelist",
            ],
            kind: SettingKind::Bool {
                // The const is shared with the resolver, so the modal shows the effective default when the user layer is unset
                default: distill_shell::util::config::DEFAULT_REMEMBER_TOOL_APPROVALS,
            },
            restart_required: true,
            hidden_in_minimal: false,
        },
        // PAGER-owned; default pinned by `defaults_match_pager_state`.
        SettingMeta {
            key: "multiline_mode",
            category: SettingCategory::Editor,
            owner: SettingOwner::Pager,
            label: "Multiline",
            description: "When on, Enter inserts a newline and Shift+Enter sends. Resets each session.",
            keywords: &["multiline", "newline", "input", "editor", "enter"],
            kind: SettingKind::Bool { default: false },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned. It reads from `pager.current_model_name` (not `cfg.models.default`) so the modal reflects `/model` switches.
        // The empty-string default means "no opinion": the shell's resolution applies
        SettingMeta {
            key: "default_model",
            category: SettingCategory::Models,
            owner: SettingOwner::Shell,
            label: "Main model",
            description: "Main model: runs every session and step. Changing this also switches the active session. Pick `(no override)` to clear.",
            keywords: &["model", "default", "agent", "llm", "grok", "switch"],
            kind: SettingKind::DynamicEnum {
                default: "",
                source: DynamicEnumSource::ActiveModelCatalog,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHARED. `u16` in UiConfig, widened to `i64` for registry.
        // Width changes apply on the next render frame.
        SettingMeta {
            key: MAX_THOUGHTS_WIDTH_KEY,
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Max thoughts width",
            description: "Column width budget for the agent's thoughts panel (40-500, default 120).",
            keywords: &[
                "thoughts",
                "width",
                "max",
                "thinking",
                "panel",
                "reasoning",
                "columns",
            ],
            kind: SettingKind::Int {
                default: ui_default.max_thoughts_width as i64,
                min: MAX_THOUGHTS_WIDTH_MIN,
                max: MAX_THOUGHTS_WIDTH_MAX,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui].show_thinking_blocks` with a process-wide cache. Default ON.
        SettingMeta {
            key: "show_thinking_blocks",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Show thinking blocks",
            description: "Show agent thinking/reasoning blocks in the scrollback while streaming.",
            keywords: &[
                "thinking",
                "reasoning",
                "thoughts",
                "blocks",
                "show",
                "hide",
            ],
            kind: SettingKind::Bool {
                default: ui_default.show_thinking_blocks.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui].prompt_suggestions` with a process-wide cache. Default ON.
        // The `GROK_PROMPT_SUGGESTIONS` env var overrides at runtime.
        SettingMeta {
            key: "prompt_suggestions",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shell,
            label: "Prompt suggestions",
            description: "After each turn, predict your likely next prompt and show it as \
                          ghost text in the input (Tab to accept). Uses a small model call \
                          per turn.",
            keywords: &[
                "prompt",
                "suggestion",
                "suggestions",
                "autocomplete",
                "ghost",
                "tab",
                "predict",
                "next",
            ],
            kind: SettingKind::Bool {
                default: ui_default.prompt_suggestions.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // PAGER-owned, persisted to `[scrollback.scroll].respect_manual_folds` in pager.toml (NOT config.toml)
        // The live value is the appearance config (`AppView::set_appearance` fans changes out to every agent)
        // The flag is read at use time, so no restart
        SettingMeta {
            key: "respect_manual_folds",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Pager,
            label: "Respect manual folds",
            description: "Keep manually folded blocks as-is while streaming and stop \
                          auto-scroll when expanding a block. Experimental.",
            keywords: &[
                "fold", "pin", "collapse", "expand", "thinking", "follow", "scroll",
            ],
            kind: SettingKind::Bool {
                default: crate::appearance::ScrollConfig::default().respect_manual_folds,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui].group_tool_verbs` with a process-wide cache. Default ON.
        SettingMeta {
            key: "group_tool_verbs",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Group tool calls",
            description: "Fold consecutive read/search/list tool calls and subagent rows into \
                          one summary row; finished thoughts fold into the group too.",
            keywords: &[
                "group", "tool", "verbs", "fold", "collapse", "read", "search", "summary",
                "thinking", "subagent",
            ],
            kind: SettingKind::Bool {
                default: ui_default.group_tool_verbs.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui].collapsed_edit_blocks` with a process-wide cache
        // Default OFF (rollout flag; remote settings / managed config can enable).
        SettingMeta {
            key: "collapsed_edit_blocks",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Collapsed edit blocks",
            description: "Show edits as one-line +N/-M diffstat summaries and merge \
                          back-to-back edits to the same file into one block; expand a \
                          row to see the diffs.",
            keywords: &[
                "edit",
                "edits",
                "diff",
                "diffstat",
                "collapse",
                "collapsed",
                "summary",
                "expand",
                "one-line",
                "merge",
                "coalesce",
            ],
            kind: SettingKind::Bool {
                default: ui_default.collapsed_edit_blocks.unwrap_or(false),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui.display_refresh].auto_cadence_enabled`. Restart-required (cadence pinned at startup); hidden in minimal.
        SettingMeta {
            key: "display_refresh_auto_cadence",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shell,
            label: "Match display refresh rate",
            description: "On high-refresh displays, the TUI will stream/scroll faster \
                          to match the display. Off keeps the classic ~60 Hz cadence. \
                          Restart required.",
            keywords: &[
                "display", "refresh", "rate", "hz", "cadence", "fps", "smooth", "scroll", "stream",
                "high", "120", "144",
            ],
            kind: SettingKind::Bool {
                // Nested Option: None inherits DISPLAY_REFRESH_DEFAULT_AUTO_CADENCE_ENABLED.
                default: ui_default
                    .display_refresh
                    .auto_cadence_enabled
                    .unwrap_or(DISPLAY_REFRESH_DEFAULT_AUTO_CADENCE_ENABLED),
            },
            restart_required: true,
            hidden_in_minimal: true,
        },
        // SHELL-owned, persisted to `[ui].scroll_speed` in config.toml.
        SettingMeta {
            key: "scroll_speed",
            category: SettingCategory::Mouse,
            owner: SettingOwner::Shell,
            label: "Scroll speed",
            description: "Mouse-wheel and trackpad scroll speed multiplier (1-100). Higher = faster.",
            keywords: &[
                "scroll", "speed", "mouse", "wheel", "trackpad", "fast", "slow",
            ],
            kind: SettingKind::Int {
                default: ui_default.scroll_speed.unwrap_or(50) as i64,
                min: 1,
                max: 100,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned `auto` | `wheel` | `trackpad` on `[ui].scroll_mode`.
        SettingMeta {
            key: "scroll_mode",
            category: SettingCategory::Mouse,
            owner: SettingOwner::Shell,
            label: "Scroll input",
            description: "Force wheel or trackpad scroll behavior when auto-detection \
                          misreads your device.",
            keywords: &[
                "scroll", "mode", "wheel", "trackpad", "mouse", "detect", "force", "input",
            ],
            kind: SettingKind::Enum {
                default: ui_default
                    .scroll_mode
                    .as_deref()
                    .and_then(ScrollMode::from_canonical)
                    .unwrap_or_default()
                    .as_canonical(),
                choices: SCROLL_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].scroll_lines`. One knob covers BOTH wheel and trackpad lines-per-tick.
        // The registered default 3 matches most terminal profiles
        // Until the user first commits a value, the per-terminal profile stays in charge (an unset cache means no override)
        SettingMeta {
            key: "scroll_lines",
            category: SettingCategory::Mouse,
            owner: SettingOwner::Shell,
            label: "Scroll lines",
            description: "Lines per scroll tick for both wheel and trackpad (1-10). \
                          Until set, each terminal's own profile applies.",
            keywords: &[
                "scroll", "lines", "tick", "notch", "wheel", "trackpad", "mouse",
            ],
            kind: SettingKind::Int {
                default: ui_default.scroll_lines.map(i64::from).unwrap_or(3),
                min: 1,
                max: 10,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned: `[ui].invert_scroll` with a process-wide cache. Default OFF.
        SettingMeta {
            key: "invert_scroll",
            category: SettingCategory::Mouse,
            owner: SettingOwner::Shell,
            label: "Invert scroll",
            description: "Reverse vertical scroll direction (natural scrolling).",
            keywords: &[
                "invert",
                "scroll",
                "natural",
                "direction",
                "reverse",
                "mouse",
                "trackpad",
            ],
            kind: SettingKind::Bool {
                default: ui_default.invert_scroll.unwrap_or(false),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned `flash` | `hold` | `word_select` on `[ui].keep_text_selection`. The compile-time default is `flash`.
        // The default can be set remotely via the `keep_text_selection_default` soft-default
        // That staged rollout applies at startup and is not reflected in this static default
        SettingMeta {
            key: "keep_text_selection",
            category: SettingCategory::Mouse,
            owner: SettingOwner::Shell,
            label: "Text selection",
            description: "How long in-app selection stays on screen and what double-click does (fold vs. select & copy a word). For your terminal or multiplexer's own selection, hold Shift while dragging (native copy).",
            keywords: &[
                "selection",
                "drag",
                "copy",
                "flash",
                "hold",
                "shift",
                "native",
                "mouse",
                "tmux",
                "double",
                "double-click",
                "word",
                "terminal",
            ],
            kind: SettingKind::Enum {
                default: TextSelection::Flash.as_canonical(),
                choices: TEXT_SELECTION_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned. Do not put "telemetry" in keywords: that word is the config-file analytics toggle (Monitoring /
        // Configuration docs).
        SettingMeta {
            key: "coding_data_sharing",
            category: SettingCategory::Privacy,
            owner: SettingOwner::Shell,
            label: "Coding data, retention, and training",
            description: "Opt-in to allow the Grok provider to retain and train on \
                          coding data, e.g., prompts, traces, & metrics, for training and \
                          debugging purposes. We may still collect simple user metrics, \
                          e.g. how many times you use the product or a feature.",
            keywords: &[
                "privacy",
                "data",
                "sharing",
                "coding",
                "retention",
                "training",
                "opt-in",
                "opt-out",
            ],
            kind: SettingKind::Enum {
                default: "opt-out",
                choices: CODING_DATA_SHARING_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].default_selected_permission` in config.toml. Canonical
        // `always_allow_all_sessions` (the effective default) lands the first prompt's cursor on the enable-always-approve
        // row.
        SettingMeta {
            key: "default_selected_permission",
            category: SettingCategory::Agent,
            owner: SettingOwner::Shell,
            label: "Default selected permission",
            description: "Which row the cursor preselects on permission prompts.",
            keywords: &[
                "permission",
                "approval",
                "cursor",
                "preselect",
                "default",
                "sticky",
                "last",
                "used",
                "yes",
                "no",
                "reject",
                "allow",
            ],
            kind: SettingKind::Enum {
                default: DefaultSelectedPermission::AlwaysAllowAllSessions.as_canonical(),
                choices: DEFAULT_SELECTED_PERMISSION_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned `[toolset.ask_user_question].timeout_enabled`. `restart_required` because the value is resolved when
        // an agent is built, like `remember_tool_approvals`.
        SettingMeta {
            key: "toolset.ask_user_question.timeout_enabled",
            category: SettingCategory::Agent,
            owner: SettingOwner::Shell,
            label: "Ask-Question timeout",
            description: "When on, the ask_user_question tool will time out after a set period \
                          of time instead of infinitely blocking.",
            keywords: &[
                "ask",
                "question",
                "questionnaire",
                "timeout",
                "ask_user_question",
                "block",
                "wait",
                "forever",
                "tool",
            ],
            kind: SettingKind::Bool {
                default: ask_user_question::DEFAULT_ASK_USER_QUESTION_TIMEOUT_ENABLED,
            },
            restart_required: true,
            hidden_in_minimal: false,
        },
        // PAGER-owned, set over ACP. Reads from `PagerLocalSnapshot.plan_mode_active`.
        // The default "off" matches `AgentView::new`'s `plan_mode_active = false`
        SettingMeta {
            key: "plan_mode",
            category: SettingCategory::Agent,
            owner: SettingOwner::Pager,
            label: "Plan mode",
            description: "When on, the agent summarises a plan before running tools or making edits.",
            keywords: &[
                "plan", "mode", "agent", "summary", "approval", "review", "session",
            ],
            kind: SettingKind::Enum {
                default: "off",
                choices: PLAN_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned startup-time settings (restart_required: true).
        // The running pager doesn't re-read these mid-session.
        SettingMeta {
            key: "show_tips",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Show tips",
            description: "Show the tip-of-the-day banner on startup. Restart required.",
            keywords: &[
                "tips", "tip", "show", "banner", "welcome", "startup", "launch",
            ],
            kind: SettingKind::Bool { default: true },
            restart_required: true,
            hidden_in_minimal: false,
        },
        // Contextual hints: one Advanced row that opens a sub-sheet of per-tip toggles
        // It applies live (restart_required: false); the group carries no value and its children are hidden from the top-level list
        SettingMeta {
            key: "contextual_hints",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Show contextual hints",
            description: "Show brief, in-context keyboard hints as you work; \
                          toggle each one individually.",
            keywords: &[
                "contextual",
                "hints",
                "tips",
                "undo",
                "plan",
                "nudge",
                "image",
                "clipboard",
                "ephemeral",
                "send",
                "interject",
                "queue",
                // Child-specific terms: the per-tip children are hidden from the top-level list, so their search words are mirrored here
                // A query like "ctrl+z" or "shift+tab" would otherwise dead-end
                "ctrl+z",
                "draft",
                "wipe",
                "mode",
                "shift+tab",
                "paste",
                "input",
                "enter",
                "follow-up",
                "small",
                "screen",
                "compact",
                "ssh",
                "wrap",
                "remote",
                // copy/export/transcript stay on the export_copy child so a "copy" query does not match the group.
            ],
            kind: SettingKind::Group {
                children: CONTEXTUAL_HINTS_CHILDREN,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "auto_update",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Auto-update",
            description: "Automatically download and install pager updates on startup. \
                          Restart required.",
            keywords: &[
                "auto", "update", "updates", "upgrade", "version", "install", "channel",
            ],
            kind: SettingKind::Bool { default: true },
            restart_required: true,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].hunk_tracker_mode`. Restart-required: the mode is read once when the session connects.
        SettingMeta {
            key: "hunk_tracker_mode",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Hunk tracker",
            description: "Which file changes the agent tracks as hunks. \
                          Off disables tracking (and LOC stats) entirely. \
                          Restart required.",
            keywords: &[
                "hunk", "tracker", "tracking", "diff", "changes", "git", "loc", "off", "disable",
            ],
            kind: SettingKind::Enum {
                default: "off",
                choices: HUNK_TRACKER_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: true,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].voice_keybind_enabled`. Default ON: `None` (inherit) reads as `true`.
        // Off disables only the Ctrl+Space / F8 chord; `/voice` (and Esc / the recording-row `[stop]`) keep working
        SettingMeta {
            key: "voice_keybind_enabled",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shell,
            label: "Voice shortcut",
            description: "Enable the Ctrl+Space / F8 shortcut for voice dictation. \
                          When off, the keys are ignored; /voice still starts \
                          dictation.",
            keywords: &[
                "voice",
                "dictation",
                "mic",
                "microphone",
                "speech",
                "stt",
                "keybinding",
                "hotkey",
                "ctrl+space",
                "f8",
                "disable",
            ],
            kind: SettingKind::Bool {
                default: ui_default.voice_keybind_enabled.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].voice_capture_mode`
        // The `hold` choice is hidden on terminals without key-release reporting (see `effective_enum_choices`)
        // It falls back to `toggle` at runtime
        SettingMeta {
            key: "voice_capture_mode",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shell,
            label: "Voice capture",
            description: "How the voice chord (Ctrl+Space / F8) behaves: Toggle \
                          (press to start/stop) or Hold to talk (hold to record, \
                          release to stop; needs a Kitty-protocol terminal).",
            keywords: &[
                "voice",
                "dictation",
                "dictate",
                "mic",
                "microphone",
                "speech",
                "stt",
                "toggle",
                "hold",
                "ctrl+space",
                "f8",
                "push-to-talk",
            ],
            kind: SettingKind::Enum {
                default: "hold",
                choices: VOICE_CAPTURE_MODE_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // SHELL-owned, persisted to `[ui].voice_stt_language`. Applied live to the next voice capture (no restart).
        // Default English; System (`auto`) follows the process locale when it maps to a Grok STT language
        // The catalog is the official STT languages (see distill_voice::STT_LANGUAGES)
        SettingMeta {
            key: "voice_stt_language",
            category: SettingCategory::Editor,
            owner: SettingOwner::Shell,
            label: "Voice language",
            description: "Speech-to-text language for voice dictation (Grok STT). \
                          English by default; System uses your locale when supported. \
                          Sets formatting language for numbers and currencies.",
            keywords: &["voice", "language", "locale", "dictation", "stt", "speech"],
            kind: SettingKind::Enum {
                default: "en",
                choices: VOICE_STT_LANGUAGE_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // Contextual-hint children (hidden from the top-level list; reached via the group sub-sheet)
        // Default ON: `None` (inherit) reads as `true`
        SettingMeta {
            key: "contextual_hints.undo",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Undo",
            description: "Remind you that Ctrl+Z restores the prompt after you clear it.",
            keywords: &["undo", "ctrl+z", "draft", "wipe", "hint"],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.undo.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.plan_mode",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Plan mode",
            description: "Suggest plan mode (Shift+Tab) when your prompt looks like a \
                          planning request.",
            keywords: &["plan", "mode", "nudge", "shift+tab", "hint"],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.plan_mode.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.image_input",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Image input",
            description: "Offer to paste an image when one is on the clipboard and the \
                          model accepts images.",
            keywords: &["image", "clipboard", "paste", "input", "hint"],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.image_input.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.send_now",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Send now",
            description: "After you queue a follow-up mid-turn, remind you that Enter \
                          on an empty prompt sends the top queued item now.",
            keywords: &[
                "send",
                "now",
                "interject",
                "queue",
                "follow-up",
                "enter",
                "empty",
                "hint",
            ],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.send_now.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.small_screen",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Small screen",
            description: "Suggest /compact-mode once per run when the terminal \
                          is short on rows.",
            keywords: &["small", "screen", "compact", "space", "rows", "hint"],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.small_screen.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.word_select",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Word select",
            description: "After double-clicking conversation text while Text selection \
                          is fold/nav, remind you that Word select lives in Settings.",
            keywords: &[
                "word",
                "select",
                "double",
                "double-click",
                "click",
                "fold",
                "selection",
                "settings",
                "hint",
            ],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.word_select.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.export_copy",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "Copy and export",
            description: "After three nearby drag-copies of conversation text, \
                          remind you that /copy and /export exist.",
            keywords: &["copy", "export", "transcript", "clipboard", "hint"],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.export_copy.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        SettingMeta {
            key: "contextual_hints.ssh_wrap",
            category: SettingCategory::Advanced,
            owner: SettingOwner::Shell,
            label: "SSH wrap",
            description: "Show a `/doctor` tip when an SSH session is not using `grok wrap`.",
            keywords: &[
                "ssh",
                "wrap",
                "remote",
                "clipboard",
                "restore",
                "startup",
                "hint",
            ],
            kind: SettingKind::Bool {
                default: ui_default.contextual_hints.ssh_wrap.unwrap_or(true),
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
        // Only the CLI flag (`--todo-gate`) is wired. Those arms don't yet have a place to land. `restart_required: false`
        // because the config-reloader rebroadcasts UI changes.
        SettingMeta {
            key: "fork_secondary_model",
            category: SettingCategory::Models,
            owner: SettingOwner::Shell,
            label: "Fork secondary model",
            description: "Model used for the secondary agent when forking. Pick `(no override)` to clear.",
            keywords: &[
                "fork",
                "secondary",
                "model",
                "agent",
                "subagent",
                "branch",
                "models",
            ],
            kind: SettingKind::DynamicEnum {
                default: "",
                source: DynamicEnumSource::ActiveModelCatalog,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        },
    ]
}
