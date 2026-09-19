//! Terminal, clipboard, and display product telemetry events.

use serde::Serialize;

#[derive(Serialize)]
pub struct EventLoopStall {
    pub max_stall_ms: u64,
    pub window_ms: u64,
    pub events_handled: u32,
    pub stall_compaction_active: bool,
    pub stall_subagents_active: u32,
    pub stall_mcp_servers_connected: u32,
}

/// The terminal writer thread made no progress on queued payloads past the blocked threshold: the terminal stopped
/// reading the pty (screen frozen, loop responsive). Emitted once per episode at onset, so it survives the user killing
/// the frozen tab; `blocked_ms` is the zero-progress time at emit.
#[derive(Serialize)]
pub struct TermWriterBlocked {
    pub blocked_ms: u64,
}

/// Flat snapshot of the terminal environment for telemetry. Shared across pager events so terminal fields are typed once.
/// Constructed by the pager's `TerminalContext::telemetry_snapshot()`.
#[derive(Clone, Debug, Serialize)]
pub struct TerminalTelemetry {
    pub brand: String,
    pub multiplexer: String,
    pub is_ssh: bool,
    pub is_byobu: bool,
    pub term_var: String,
    pub tmux_version: String,
    pub xtversion: String,
    /// Raw, as its source reported it; shapes vary (`"3.5.6"`, `"20240203-110809-5046fc22"`, `"7402"`).
    /// Empty when unknown.
    pub term_version: String,
    pub term_version_source: String,
    /// The Kitty protocol was negotiated *without* `REPORT_EVENT_TYPES`.
    /// `term_version` identified a build that mis-encodes key releases (Alacritty 0.14.x and earlier).
    /// A field rather than its own event so the affected population always has a denominator.
    pub kitty_event_types_withheld: bool,
    pub host_os: String,
    pub display_server: String,
    pub modifier_cmd_fate: String,
    pub modifier_opt_fate: String,
    pub enter_modifier_fate: String,
    pub hyperlink_osc8: String,
    pub hyperlink_skip_reason: String,
    pub clipboard_route: String,
    pub clipboard_native_tool: String,
    /// Wayland data-control protocol availability: "yes" | "no" | "n/a" (n/a off Wayland).
    pub clipboard_data_control: String,
}

/// One-shot OS primary-display refresh probe and auto-cadence decision at process start.
#[derive(Serialize)]
pub struct DisplayRefreshProbe {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// `ok` | `skipped` | `error`
    pub outcome: String,
    /// Refresh rate as `i64` so OTLP/analytics keep a numeric field.
    pub hz: Option<i64>,
    /// Backend token, e.g. `macos_core_graphics`.
    pub source: String,
    /// Empty when ok; else stable skip/error reason (`ssh`, `wsl`, …).
    pub skip_reason: String,
    /// Wall ms as `i64` so OTLP/analytics keep a numeric field (u64 serializes as string).
    pub duration_ms: i64,
    pub auto_cadence_enabled: bool,
    /// True when derived auto ms is used on at least one motion clock.
    pub auto_cadence_applied: bool,
    pub effective_min_draw_ms: i64,
    pub effective_scroll_cadence_ms: i64,
    /// `flag_off` | `disabled` | `probe_skip` | `hz_out_of_range` | `env_override` | `applied`.
    pub auto_cadence_reason: String,
}

/// Backend that answered a paste-time clipboard image read.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardReadPath {
    Native,
    Osascript,
    Arboard,
    LinuxCli,
}

/// Emitted once per system-clipboard attachment read during paste (Ctrl/Cmd+V).
/// Diagnoses silent image-paste failures and wrong-image reports: `read_path` localizes them to one backend, `image_hash` tells "same bytes again" from "different image".
#[derive(Serialize)]
pub struct ClipboardImagePaste {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Which read ran: "attachments" (file URLs and image) or "image".
    pub probe: String,
    /// "image" | "file_urls" | "empty" | "error".
    pub outcome: String,
    /// Absent when outcome == "error".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_path: Option<ClipboardReadPath>,
    /// MIME type when outcome == "image", else "".
    pub image_mime: String,
    /// Blake3 hex of the raster bytes, keyed per process (comparable within one run only); "" unless outcome == "image".
    pub image_hash: String,
    /// Encoded raster size; 0 unless outcome == "image".
    pub image_bytes: u64,
    /// Wall-clock duration of the clipboard read in milliseconds.
    pub duration_ms: u64,
}

/// Why a paste-time attachment probe ended without attaching anything.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardProbeDropReason {
    PasteboardChangedBeforeRead,
    /// Whatever the read found, a raster or nothing, is discarded.
    PasteboardChangedAfterRead,
    /// IME-as-paste terminals: the bracketed payload did not match the clipboard text.
    BracketedPayloadMismatch,
    ReadFailed,
    Timeout,
    PersistFailed,
    /// The decode/persist stage panicked: a pager bug, not a pasteboard failure.
    Panicked,
}

/// The pager's paste-time probe dropped the attachment; completion-side discards and the `grok wrap` host read are not counted.
/// `clipboard_image_paste` may still report `outcome == "image"` for the raster this drop discards; join on `image_hash`.
#[derive(Serialize)]
pub struct ClipboardPasteProbeDropped {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    pub reason: ClipboardProbeDropReason,
    /// Same key as `clipboard_image_paste.image_hash`; "" when no raster was read.
    pub image_hash: String,
    pub duration_ms: u64,
}

/// Emitted when Ctrl/Cmd+V (paste key) is handled but the **host** process clipboard has no pasteable text/image/file URLs.
/// Diagnoses silent no-ops on remote/ETX sessions.
/// Does not change paste behavior.
#[derive(Serialize)]
pub struct PasteKeyEmptyHostClipboard {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Call site: "agent" | "prompt_widget" | "dashboard" | "peek" | "picker".
    pub surface: String,
}

/// Emitted once per user-visible text copy (`copy_text` / TUI yank, etc.). Captures per-leg write outcomes so we can
/// diagnose "copy doesn't work" reports without relying on toast text alone. E.g. on Wayland with the xclip probe: did
/// wl-copy actually succeed?
#[derive(Serialize)]
pub struct ClipboardCopy {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    /// Call site; currently always `copy_text`.
    pub source: &'static str,
    /// Payload size only (no content).
    pub text_len: u64,
    /// Route policy (enabled legs), independent of which legs succeeded.
    pub route_native: bool,
    pub route_tmux: bool,
    pub route_osc52: bool,
    /// `ClipboardRoute` Display, e.g. `native+osc52`.
    pub route_label: String,
    /// CLI tools actually invoked, `+`-joined (e.g. `wl-copy+xclip`); empty if none.
    pub cli_tools_tried: String,
    /// CLI tools that returned Ok, `+`-joined; empty if none succeeded. On Wayland, wl-copy is read-back-verified only when
    /// `data_control` is false. With `data_control && arboard_ok` its exit-0 is credited unverified (the arboard write is
    /// authoritative). Condition wl-copy success rates on `data_control`.
    pub cli_ok_tools: String,
    pub cli_ok: bool,
    pub arboard_ok: bool,
    /// The Wayland data-control protocol was available for this write (the environment probe, NOT proof the arboard write landed).
    /// A focus-free authoritative write additionally requires `arboard_ok`.
    /// Always false off-Wayland.
    pub data_control: bool,
    pub tmux_ok: bool,
    pub osc52_ok: bool,
    /// Evidence classification: `confirmed` | `unverified` | `failed`.
    pub delivery: &'static str,
    /// An explicit `grok wrap` OSC 52 sink was active.
    pub osc52_sink: bool,
    /// The process was inside a container without a display server.
    pub container_no_display: bool,
    /// Historical boolean projection: true unless `delivery == failed`.
    pub reported_success: bool,
    /// Exact UX toast branch selected by the environment policy.
    pub toast_kind: &'static str,
    pub duration_ms: u64,
}

/// Emitted when backspace/delete is pressed but produces no text change on a non-empty prompt.
/// Used to diagnose the "backspace lock" bug.
#[derive(Serialize)]
pub struct BackspaceNoEffect {
    #[serde(flatten)]
    pub terminal: TerminalTelemetry,
    pub key_code: String,
    pub key_modifiers: String,
    pub key_kind: String,
    pub cursor_pos: usize,
    pub text_len: usize,
    pub has_selection: bool,
}

/// Emitted each time a terminal notification is actually sent (not filtered by condition or event kind).
/// Used for protocol distribution analysis.
#[derive(Serialize)]
pub struct NotificationEmitted {
    pub protocol: &'static str,
    pub event_kind: &'static str,
    pub was_focused: bool,
}
