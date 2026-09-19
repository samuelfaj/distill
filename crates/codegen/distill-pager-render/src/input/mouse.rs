//! Scroll normalization for mouse wheel/trackpad input.
//!
//! Terminal scroll events vary widely in event counts per wheel tick, and inter-event timing overlaps heavily between wheel and trackpad input.
//! We normalize by treating events as short streams separated by gaps, converting them into line deltas with a per-terminal events-per-tick factor.
//! Redraw is coalesced to a fixed cadence.
//!
//! A mouse wheel "tick" (one notch) is expected to scroll by a fixed number of lines (default: 3) regardless of the terminal's raw event density.
//! Trackpad scrolling should remain higher fidelity: small movements accumulate sub-line amounts that only scroll once whole lines are reached.
//!
//! Because terminal mouse scroll events do not encode magnitude (only direction), wheel-vs-trackpad detection is heuristic.
//! We bias toward trackpad-like (avoids overshoot) and "promote" to wheel-like when the first tick-worth of events arrives quickly.
//! A user can force wheel or trackpad behavior when the heuristic is wrong: `scroll_mode` (or `GROK_SCROLL_MODE`) pins [`ScrollInputMode`].
//! `invert_scroll` / `scroll_lines` / `scroll_speed` tune direction and throughput; see [`ScrollConfigOverrides::from_settings_caches`].

use crate::input::scroll_log::{
    ScrollLogConfigEcho, ScrollLogEvent, ScrollLogEvt, ScrollLogRecorder, ScrollLogTrigger,
};
use crate::terminal::{MultiplexerKind, TerminalName};
use crossterm::event::{MouseEvent, MouseEventKind};
use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScrollInputMode {
    /// Auto-detect wheel vs trackpad per stream (default).
    #[default]
    Auto,
    /// Always treat input as mouse wheel (fixed lines per tick).
    Wheel,
    /// Always treat input as trackpad (fractional accumulation).
    Trackpad,
}

impl ScrollInputMode {
    /// Display label (settings vocabulary), for the scroll-debug HUD.
    pub fn label(self) -> &'static str {
        match self {
            ScrollInputMode::Auto => "auto",
            ScrollInputMode::Wheel => "wheel",
            ScrollInputMode::Trackpad => "trackpad",
        }
    }
}

// Mirrored in the PTY harness scroll matrix (no pager dep). Retune both; the harness models the 16ms default, not a runtime cadence override.
const STREAM_GAP_MS: u64 = 80;
const STREAM_GAP: Duration = Duration::from_millis(STREAM_GAP_MS);
/// Default scroll flush cadence (~60fps). Runtime override: `set_redraw_cadence`.
const REDRAW_CADENCE_MS: u64 = 16;
const REDRAW_CADENCE: Duration = Duration::from_millis(REDRAW_CADENCE_MS);

const DEFAULT_EVENTS_PER_TICK: u16 = 3;
const DEFAULT_WHEEL_LINES_PER_TICK: u16 = 3;
const DEFAULT_TRACKPAD_LINES_PER_TICK: u16 = 3;
const DEFAULT_SCROLL_MODE: ScrollInputMode = ScrollInputMode::Auto;
const DEFAULT_WHEEL_TICK_DETECT_MAX_MS: u64 = 12;
const DEFAULT_WHEEL_LIKE_MAX_DURATION_MS: u64 = 200;
const DEFAULT_TRACKPAD_ACCEL_MAX: u16 = 3;
const DEFAULT_TRACKPAD_DETECT_MAX_INTERVAL_MS: f32 = 30.0;
/// Floor for the per-flush delta cap (see [`ScrollConfig::flush_cap`]).
/// Excess stays in the stream and arrives on subsequent cadence flushes.
const MIN_DELTA_PER_FLUSH: i32 = 6;
/// Interval-based acceleration: thresholds for band classification.
/// Intervals are averaged over a rolling window of recent events.
const ACCEL_INTERVAL_FAST_MS: f32 = 8.0;
const ACCEL_INTERVAL_MEDIUM_MS: f32 = 20.0;
/// Sub-6ms spacing is terminal batching (Ghostty double-reports a notch), not gesture speed.
/// Events still accumulate lines but stay out of the accel/ept window, which would otherwise read them as max velocity.
const ACCEL_MIN_INTERVAL_MS: f32 = 6.0;
/// Multipliers for each speed band.
const ACCEL_MULTIPLIER_BASE: f32 = 1.0;
const ACCEL_MULTIPLIER_MEDIUM: f32 = 1.6;
const ACCEL_MULTIPLIER_FAST: f32 = 2.5;
/// Rolling window size for interval history.
const ACCEL_HISTORY_SIZE: usize = 6;
const MIN_LINES_PER_WHEEL_STREAM: i32 = 1;

// xterm.js embeds (VS Code and similar editor webviews) emit scroll events more slowly than native terminals
// They get wider accel/trackpad windows and a higher trackpad lines-per-tick
// Zed is ept=1 but not this profile.
fn is_vscode_embed(name: TerminalName) -> bool {
    matches!(
        name,
        TerminalName::VsCode | TerminalName::Cursor | TerminalName::Windsurf
    )
}

fn default_accel_interval_fast_ms_for_terminal(name: TerminalName) -> f32 {
    if is_vscode_embed(name) {
        25.0
    } else {
        ACCEL_INTERVAL_FAST_MS
    }
}

fn default_accel_interval_medium_ms_for_terminal(name: TerminalName) -> f32 {
    if is_vscode_embed(name) {
        50.0
    } else {
        ACCEL_INTERVAL_MEDIUM_MS
    }
}

/// Convert a scroll speed setting (1-100) to a multiplier.
/// 50 is 1.0x (default), 1 is 0.1x (slowest), 100 is 6.0x (fastest).
pub fn speed_to_multiplier(speed: u8) -> f32 {
    let s = speed.clamp(1, 100) as f32;
    if s <= 50.0 {
        // 1 maps to 0.1 and 50 to 1.0 (linear)
        0.1 + (s - 1.0) * (0.9 / 49.0)
    } else {
        // 50 maps to 1.0 and 100 to 6.0 (linear)
        1.0 + (s - 50.0) * (5.0 / 50.0)
    }
}

fn default_trackpad_detect_max_interval_ms_for_terminal(name: TerminalName) -> f32 {
    if is_vscode_embed(name) {
        60.0
    } else {
        DEFAULT_TRACKPAD_DETECT_MAX_INTERVAL_MS
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollStreamKind {
    Unknown,
    Wheel,
    Trackpad,
}

impl ScrollStreamKind {
    /// Display label for the scroll-debug HUD.
    fn label(self) -> &'static str {
        match self {
            ScrollStreamKind::Unknown => "unknown",
            ScrollStreamKind::Wheel => "wheel",
            ScrollStreamKind::Trackpad => "trackpad",
        }
    }
}

/// High-level scroll direction used to sign line deltas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
}

impl ScrollDirection {
    pub fn from_mouse_event(event: &MouseEvent) -> Option<Self> {
        match event.kind {
            MouseEventKind::ScrollUp => Some(ScrollDirection::Up),
            MouseEventKind::ScrollDown => Some(ScrollDirection::Down),
            _ => None,
        }
    }

    fn sign(self) -> i32 {
        match self {
            ScrollDirection::Up => -1,
            ScrollDirection::Down => 1,
        }
    }

    fn inverted(self) -> Self {
        match self {
            ScrollDirection::Up => ScrollDirection::Down,
            ScrollDirection::Down => ScrollDirection::Up,
        }
    }
}

/// Scroll normalization settings derived from terminal metadata and user overrides.
/// `viewport_height` is per-event runtime state stamped by the caller; see [`Self::with_viewport_height`].
#[derive(Clone, Copy, Debug)]
pub struct ScrollConfig {
    /// Per-terminal normalization factor ("events per wheel tick").
    events_per_tick: u16,
    /// Lines applied per mouse wheel tick.
    wheel_lines_per_tick: u16,
    /// Lines applied per tick-equivalent for trackpad scrolling.
    trackpad_lines_per_tick: u16,
    /// Trackpad acceleration: maximum multiplier.
    trackpad_accel_max: u16,
    /// Force wheel/trackpad behavior, or infer it per stream.
    mode: ScrollInputMode,
    /// Auto-mode threshold: how quickly the first wheel tick must complete.
    wheel_tick_detect_max: Duration,
    /// Auto-mode fallback: maximum duration still considered "wheel-like".
    wheel_like_max_duration: Duration,
    /// Invert the sign of vertical scroll direction.
    invert_direction: bool,
    /// Interval-based acceleration: threshold for "fast" band (ms).
    accel_interval_fast_ms: f32,
    /// Interval-based acceleration: threshold for "medium" band (ms).
    accel_interval_medium_ms: f32,
    /// ept=1 trackpad detection: max avg interval (ms) to classify as trackpad.
    trackpad_detect_max_interval_ms: f32,
    /// User-facing speed multiplier (derived from scroll_speed 1-100 setting).
    speed_multiplier: f32,
    /// Viewport height (rows) of the scroll target; 0 means unknown.
    /// Runtime state, not a terminal profile: it sizes the per-flush trackpad cap proportionally to the visible pane.
    viewport_height: u16,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollConfigOverrides {
    pub events_per_tick: Option<u16>,
    pub wheel_lines_per_tick: Option<u16>,
    pub trackpad_lines_per_tick: Option<u16>,
    pub trackpad_accel_max: Option<u16>,
    pub mode: Option<ScrollInputMode>,
    pub wheel_tick_detect_max_ms: Option<u64>,
    pub wheel_like_max_duration_ms: Option<u64>,
    pub invert_direction: bool,
    pub accel_interval_fast_ms: Option<f32>,
    pub accel_interval_medium_ms: Option<f32>,
    pub trackpad_detect_max_interval_ms: Option<f32>,
    pub speed_multiplier: Option<f32>,
}

impl ScrollConfigOverrides {
    /// Single build site so a runtime settings change and a fresh start agree. `scroll_lines` overrides both paths; unset keeps the terminal profile.
    pub fn from_settings_caches() -> Self {
        let mode = match crate::appearance::cache::load_scroll_mode() {
            // Auto is the profile default: no opinion, keep detection
            crate::appearance::ScrollMode::Auto => None,
            crate::appearance::ScrollMode::Wheel => Some(ScrollInputMode::Wheel),
            crate::appearance::ScrollMode::Trackpad => Some(ScrollInputMode::Trackpad),
        };
        let lines = crate::appearance::cache::load_scroll_lines().map(u16::from);
        Self {
            mode,
            invert_direction: crate::appearance::cache::load_invert_scroll(),
            wheel_lines_per_tick: lines,
            trackpad_lines_per_tick: lines,
            speed_multiplier: Some(speed_to_multiplier(
                crate::appearance::cache::load_scroll_speed(),
            )),
            ..Self::default()
        }
    }
}

/// Multiplexers that re-encode mouse into their own SGR stream (tmux with `mouse on`, screen, zellij, herdr all re-emit per pane).
/// Cmux is a Ghostty-backed passthrough and keeps the outer brand's stream.
fn multiplexer_reencodes_mouse(multiplexer: MultiplexerKind) -> bool {
    matches!(
        multiplexer,
        MultiplexerKind::Tmux
            | MultiplexerKind::Screen
            | MultiplexerKind::Zellij
            | MultiplexerKind::Herdr
    )
}

impl ScrollConfig {
    /// Tests-only shorthand for [`Self::from_terminal_context`] with no multiplexer.
    /// Production always knows the multiplexer (`terminal_context()` is a global).
    /// Gating this off outside tests makes silently dropping multiplexer awareness a compile error.
    #[cfg(test)]
    fn from_terminal(brand: TerminalName, overrides: ScrollConfigOverrides) -> Self {
        Self::from_terminal_context(brand, MultiplexerKind::Undetected, overrides)
    }

    /// The production constructor: detected terminal context plus [`ScrollConfigOverrides::from_settings_caches`].
    /// Brand and multiplexer both come from the `terminal_context()` global.
    /// Every production rebuild site (startup, hot-reload, runtime setting change) uses this so the settings-to-config mapping cannot drift.
    pub fn from_settings() -> Self {
        let ctx = crate::terminal::terminal_context();
        Self::from_terminal_context(
            ctx.brand,
            ctx.multiplexer,
            ScrollConfigOverrides::from_settings_caches(),
        )
    }

    /// Muxes re-encode SGR, so an outer ept=3 under-counts 3x when they re-chunk to one event.
    /// Those brands become conservative ept=1; wheel vs trackpad then uses Auto timing, not event-count trust.
    pub fn from_terminal_context(
        brand: TerminalName,
        multiplexer: MultiplexerKind,
        overrides: ScrollConfigOverrides,
    ) -> Self {
        let remuxed = multiplexer_reencodes_mouse(multiplexer);
        let mut events_per_tick = if remuxed {
            1
        } else {
            match brand {
                TerminalName::AppleTerminal => 3,
                // (confirmed in Warp source: app/src/settings/scroll.rs)
                TerminalName::WarpTerminal => 3,
                TerminalName::WezTerm => 1,
                TerminalName::Alacritty | TerminalName::Rio | TerminalName::Foot => 3,
                TerminalName::Ghostty => 3,
                TerminalName::Iterm2 => 1,
                TerminalName::VsCode
                | TerminalName::Cursor
                | TerminalName::Windsurf
                | TerminalName::Zed => 1,
                TerminalName::Kitty => 3,
                TerminalName::GrokDesktop
                | TerminalName::Vte
                | TerminalName::Terminator
                | TerminalName::WindowsTerminal
                | TerminalName::JetBrains
                | TerminalName::Otty
                | TerminalName::Unknown => DEFAULT_EVENTS_PER_TICK,
            }
        };

        if let Some(override_value) = overrides.events_per_tick {
            events_per_tick = override_value.max(1);
        }

        // Get one line per notch on ept=1 terminals; one line per event when a multiplexer re-chunks
        // Whatever event count the multiplexer emits per notch, pricing each at one line never over-scrolls
        let mut wheel_lines_per_tick = if remuxed {
            1
        } else {
            match brand {
                TerminalName::Iterm2 | TerminalName::WezTerm => 1,
                _ => DEFAULT_WHEEL_LINES_PER_TICK,
            }
        };
        if let Some(override_value) = overrides.wheel_lines_per_tick {
            wheel_lines_per_tick = override_value.max(1);
        }

        let mut trackpad_lines_per_tick = if is_vscode_embed(brand) && !remuxed {
            // Compensate for the ~3x lower event rate and match Ghostty-level throughput
            15
        } else {
            DEFAULT_TRACKPAD_LINES_PER_TICK
        };
        if let Some(override_value) = overrides.trackpad_lines_per_tick {
            trackpad_lines_per_tick = override_value.max(1);
        }

        let mut trackpad_accel_max = DEFAULT_TRACKPAD_ACCEL_MAX;
        if let Some(override_value) = overrides.trackpad_accel_max {
            trackpad_accel_max = override_value.max(1);
        }

        let wheel_tick_detect_max_ms = overrides
            .wheel_tick_detect_max_ms
            .unwrap_or(DEFAULT_WHEEL_TICK_DETECT_MAX_MS);
        let wheel_tick_detect_max = Duration::from_millis(wheel_tick_detect_max_ms);
        let wheel_like_max_duration = Duration::from_millis(
            overrides
                .wheel_like_max_duration_ms
                .unwrap_or(DEFAULT_WHEEL_LIKE_MAX_DURATION_MS),
        );

        // Multiplexed pacing reflects the multiplexer's re-emission, not the outer brand, so the brand-widened windows do not apply either
        let accel_interval_fast_ms = overrides.accel_interval_fast_ms.unwrap_or_else(|| {
            if remuxed {
                ACCEL_INTERVAL_FAST_MS
            } else {
                default_accel_interval_fast_ms_for_terminal(brand)
            }
        });
        let accel_interval_medium_ms = overrides.accel_interval_medium_ms.unwrap_or_else(|| {
            if remuxed {
                ACCEL_INTERVAL_MEDIUM_MS
            } else {
                default_accel_interval_medium_ms_for_terminal(brand)
            }
        });
        let trackpad_detect_max_interval_ms = overrides
            .trackpad_detect_max_interval_ms
            .unwrap_or_else(|| {
                if remuxed {
                    DEFAULT_TRACKPAD_DETECT_MAX_INTERVAL_MS
                } else {
                    default_trackpad_detect_max_interval_ms_for_terminal(brand)
                }
            });

        Self {
            events_per_tick,
            wheel_lines_per_tick,
            trackpad_lines_per_tick,
            trackpad_accel_max,
            mode: overrides.mode.unwrap_or(DEFAULT_SCROLL_MODE),
            wheel_tick_detect_max,
            wheel_like_max_duration,
            invert_direction: overrides.invert_direction,
            accel_interval_fast_ms,
            accel_interval_medium_ms,
            trackpad_detect_max_interval_ms,
            speed_multiplier: overrides.speed_multiplier.unwrap_or(1.0),
            viewport_height: 0,
        }
    }

    /// Stamp the scroll target's viewport height (rows) onto this config.
    /// The config already travels per event, so the viewport rides along instead of adding a second parameter everywhere.
    pub fn with_viewport_height(mut self, rows: u16) -> Self {
        self.viewport_height = rows;
        self
    }

    fn events_per_tick_f32(self) -> f32 {
        self.events_per_tick.max(1) as f32
    }

    fn wheel_lines_per_tick_f32(self) -> f32 {
        self.wheel_lines_per_tick.max(1) as f32
    }

    fn trackpad_lines_per_tick_f32(self) -> f32 {
        self.trackpad_lines_per_tick.max(1) as f32
    }

    fn trackpad_events_per_tick_f32(self) -> f32 {
        // Always use the standard tick size for trackpad so all terminals get the same base scroll rate regardless of events_per_tick
        DEFAULT_EVENTS_PER_TICK as f32
    }

    fn trackpad_accel_max_f32(self) -> f32 {
        self.trackpad_accel_max.max(1) as f32
    }

    /// Half the viewport per flush, floored so tiny viewports still move. A fixed cap teleports or hard-ceilings flicks.
    /// Legit wheel never reaches it; only misclassified floods or extreme speed do, and those are paced across slots.
    fn flush_cap(self) -> i32 {
        (self.viewport_height as i32 / 2).max(MIN_DELTA_PER_FLUSH)
    }

    fn apply_direction(self, direction: ScrollDirection) -> ScrollDirection {
        if self.invert_direction {
            direction.inverted()
        } else {
            direction
        }
    }
}

impl Default for ScrollConfig {
    fn default() -> Self {
        Self {
            events_per_tick: DEFAULT_EVENTS_PER_TICK,
            wheel_lines_per_tick: DEFAULT_WHEEL_LINES_PER_TICK,
            trackpad_lines_per_tick: DEFAULT_TRACKPAD_LINES_PER_TICK,
            trackpad_accel_max: DEFAULT_TRACKPAD_ACCEL_MAX,
            mode: DEFAULT_SCROLL_MODE,
            wheel_tick_detect_max: Duration::from_millis(DEFAULT_WHEEL_TICK_DETECT_MAX_MS),
            wheel_like_max_duration: Duration::from_millis(DEFAULT_WHEEL_LIKE_MAX_DURATION_MS),
            invert_direction: false,
            accel_interval_fast_ms: ACCEL_INTERVAL_FAST_MS,
            accel_interval_medium_ms: ACCEL_INTERVAL_MEDIUM_MS,
            trackpad_detect_max_interval_ms: DEFAULT_TRACKPAD_DETECT_MAX_INTERVAL_MS,
            speed_multiplier: 1.0,
            viewport_height: 0,
        }
    }
}

/// Output from scroll handling: lines to apply plus when to check for stream end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollUpdate {
    pub lines: i32,
    pub next_tick_in: Option<Duration>,
}

/// HUD snapshot from `&self` plus caller `now`. No field mutates stream state or reads a clock, so per-frame sampling cannot affect scroll.
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollDebugSnapshot {
    /// Live stream facts; `None` between gestures.
    pub stream: Option<ScrollStreamDebug>,
    /// Kind/size of the most recently finalized stream.
    pub last_stream: Option<ScrollStreamSummary>,
    /// Sub-line remainder (final line units) awaiting the next same-direction stream.
    pub carry_lines: f32,
    /// Milliseconds since the last flush that applied lines.
    pub ms_since_flush: u64,
    /// Event-loop scroll-clock deadline; `None` means the clock is off.
    pub next_deadline_ms: Option<u64>,
    /// Config echo (fields below): the live stream's captured config when one exists (that is what prices the gesture), else the caller's config.
    pub mode: ScrollInputMode,
    pub events_per_tick: u16,
    pub wheel_lines_per_tick: u16,
    pub trackpad_lines_per_tick: u16,
    pub invert: bool,
    pub speed_multiplier: f32,
    pub viewport_height: u16,
    /// Per-flush delta cap in effect ([`ScrollConfig::flush_cap`]).
    pub flush_cap: i32,
    /// Effective scroll flush cadence in ms (`GROK_SCROLL_CADENCE_MS` / default 16).
    pub cadence_ms: u64,
}

/// Live-stream slice of [`ScrollDebugSnapshot`].
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollStreamDebug {
    /// Raw classification (`unknown` until promotion; a forced `mode` prices the stream regardless, see the snapshot's `mode` echo).
    pub kind: &'static str,
    /// Kind was decided mid-stream (Auto-mode promotion fired).
    pub promoted: bool,
    /// Events accumulated in the stream so far.
    pub events: usize,
    /// Rolling average inter-event interval (ms); `None` until two events passed the 6ms batching filter.
    pub avg_interval_ms: Option<f32>,
    /// Acceleration multiplier currently in effect.
    pub accel: f32,
    /// Desired lines (post accel/speed multipliers, carry included).
    pub desired_lines: f32,
    /// Whole lines already delivered for this stream.
    pub applied_lines: i32,
    /// Whole lines a flush right now would deliver, pre-cap ([`ScrollStream::effective_pending`]).
    pub backlog: i32,
    /// Countdown to the 80ms stream-gap finalize.
    pub gap_remaining_ms: u64,
}

/// Post-finalize breadcrumb of a completed stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollStreamSummary {
    /// Classification decided at finalize (never `unknown` in Auto mode).
    pub kind: &'static str,
    pub events: usize,
    pub applied_lines: i32,
}

/// One gesture until an 80ms gap or a direction flip. Trackpad flushes at redraw cadence; callers must tick while a stream is active.
// Not Clone: the flight recorder owns a file writer.
#[derive(Debug)]
pub struct MouseScrollState {
    stream: Option<ScrollStream>,
    last_redraw_at: Instant,
    /// Sub-line remainder across same-direction streams. Final line units; no consumer may re-scale it. Added after multipliers.
    carry_lines: f32,
    carry_direction: Option<ScrollDirection>,
    /// Diagnostic breadcrumb for the scroll-debug HUD: the most recently finalized stream.
    /// Write-only for the state machine (never read back), so it cannot affect scroll behavior.
    last_finalized: Option<ScrollStreamSummary>,
    /// `GROK_SCROLL_LOG` flight recorder ([`crate::input::scroll_log`]).
    /// Write-only like `last_finalized`: emission cannot affect scroll behavior; `None` (env unset) costs one branch per emission point.
    recorder: Option<ScrollLogRecorder>,
    /// Wheel/trackpad flush cadence (default 16ms; the event loop may override it from the environment).
    redraw_cadence: Duration,
}

impl MouseScrollState {
    /// Create a new scroll state with a deterministic time origin (for tests).
    /// Fixed 16ms cadence (no env); tests inject a recorder by assigning the field.
    #[cfg(test)]
    fn new_at(now: Instant) -> Self {
        Self {
            stream: None,
            last_redraw_at: now,
            carry_lines: 0.0,
            carry_direction: None,
            last_finalized: None,
            recorder: None,
            redraw_cadence: REDRAW_CADENCE,
        }
    }

    /// Inject flush cadence (`GROK_SCROLL_CADENCE_MS`); Default stays 16ms.
    pub fn set_redraw_cadence(&mut self, cadence: Duration) {
        self.redraw_cadence = cadence;
    }

    /// Handle a scroll event using the current time.
    pub fn on_scroll_event(
        &mut self,
        direction: ScrollDirection,
        config: ScrollConfig,
    ) -> ScrollUpdate {
        self.on_scroll_event_at(Instant::now(), direction, config)
    }

    /// Handle a scroll event at a specific time.
    pub fn on_scroll_event_at(
        &mut self,
        now: Instant,
        direction: ScrollDirection,
        config: ScrollConfig,
    ) -> ScrollUpdate {
        let direction = config.apply_direction(direction);
        let mut lines = 0;

        if let Some(mut stream) = self.stream.take() {
            let gap = now.duration_since(stream.last);
            if gap > STREAM_GAP || stream.direction != direction {
                // Flip: cancel the old stream's backlog; reversal must be instant, not preceded by a stale opposite-direction jump
                let cancel_backlog = stream.direction != direction;
                lines += self.finalize_stream_at(now, &mut stream, cancel_backlog);
            } else {
                self.stream = Some(stream);
            }
        }

        if self.stream.is_none() {
            if self.carry_direction != Some(direction) {
                self.carry_lines = 0.0;
                self.carry_direction = Some(direction);
            }
            let stream = ScrollStream::new(now, direction, config);
            Self::record_scroll_log(
                &mut self.recorder,
                now,
                ScrollLogEvt::StreamStart,
                ScrollLogTrigger::Event,
                &stream,
                self.carry_lines,
                0,
            );
            self.stream = Some(stream);
        }
        let carry_lines = self.carry_lines;
        let Some(stream) = self.stream.as_mut() else {
            unreachable!("stream inserted above");
        };
        stream.push_event(now, direction);
        stream.maybe_promote_kind(now);

        if now.duration_since(self.last_redraw_at) >= self.redraw_cadence || stream.just_promoted {
            // Capture before the flush block resets the promotion flag.
            let trigger = if stream.just_promoted {
                ScrollLogTrigger::Promotion
            } else {
                ScrollLogTrigger::Event
            };
            let flushed = Self::flush_lines_at(&mut self.last_redraw_at, carry_lines, now, stream);
            lines += flushed;
            if flushed != 0 {
                Self::record_scroll_log(
                    &mut self.recorder,
                    now,
                    ScrollLogEvt::Flush,
                    trigger,
                    stream,
                    carry_lines,
                    flushed,
                );
            }
            stream.just_promoted = false;
        }

        ScrollUpdate {
            lines,
            next_tick_in: self.next_tick_in(now),
        }
    }

    /// Check whether an active stream has ended based on the current time.
    pub fn on_tick(&mut self) -> ScrollUpdate {
        self.on_tick_at(Instant::now())
    }

    /// Check whether an active stream has ended at a specific time (for tests).
    pub fn on_tick_at(&mut self, now: Instant) -> ScrollUpdate {
        let mut lines = 0;
        if let Some(mut stream) = self.stream.take() {
            let gap = now.duration_since(stream.last);
            // Past the gap, defer finalize while a coast drain still has lines, so the tail decays instead of bursting.
            // Every drain tick delivers at least one line; the budget caps the backlog at one flush_cap.
            if gap > STREAM_GAP && stream.flushable_now(self.carry_lines) == 0 {
                lines = self.finalize_stream_at(now, &mut stream, false);
            } else {
                // Cadence-flush active streams so short bursts (especially on ept=1 terminals) don't stall until the 80ms gap
                if now.duration_since(self.last_redraw_at) >= self.redraw_cadence {
                    lines = Self::flush_lines_at(
                        &mut self.last_redraw_at,
                        self.carry_lines,
                        now,
                        &mut stream,
                    );
                    if lines != 0 {
                        Self::record_scroll_log(
                            &mut self.recorder,
                            now,
                            ScrollLogEvt::Flush,
                            ScrollLogTrigger::Tick,
                            &stream,
                            self.carry_lines,
                            lines,
                        );
                    }
                }
                self.stream = Some(stream);
            }
        }

        ScrollUpdate {
            lines,
            next_tick_in: self.next_tick_in(now),
        }
    }

    /// Whether there is an active scroll stream (events still being accumulated).
    /// Used by the event loop to decide whether to schedule ticks for stream gap detection and pending line flushes.
    pub fn has_active_stream(&self) -> bool {
        self.stream.is_some()
    }

    pub fn cancel_stream(&mut self) {
        self.stream = None;
        self.carry_lines = 0.0;
        self.carry_direction = None;
    }

    /// Next flush/finalize, on redraw cadence or the 80ms gap, never animation fps.
    /// `None` is clock off; `Some(ZERO)` is overdue. Unlike [`Self::next_tick_in`], whose `None` also means overdue.
    pub fn scroll_clock_deadline(&self, now: Instant) -> Option<Duration> {
        self.stream.as_ref()?;
        Some(self.next_tick_in(now).unwrap_or(Duration::ZERO))
    }

    /// Pass the event path's viewport stamp so the echoed cap matches what a gesture would get right now.
    pub fn debug_snapshot(&self, config: &ScrollConfig, now: Instant) -> ScrollDebugSnapshot {
        let cfg = self.stream.as_ref().map_or(*config, |s| s.config);
        ScrollDebugSnapshot {
            stream: self.stream.as_ref().map(|s| ScrollStreamDebug {
                kind: s.kind.label(),
                promoted: s.kind != ScrollStreamKind::Unknown,
                events: s.event_count,
                avg_interval_ms: s.avg_interval_ms(),
                accel: s.interval_accel(),
                desired_lines: s.desired_lines_f32(self.carry_lines),
                applied_lines: s.applied_lines,
                backlog: s.effective_pending(self.carry_lines),
                gap_remaining_ms: STREAM_GAP
                    .saturating_sub(now.saturating_duration_since(s.last))
                    .as_millis() as u64,
            }),
            last_stream: self.last_finalized,
            carry_lines: self.carry_lines,
            ms_since_flush: now
                .saturating_duration_since(self.last_redraw_at)
                .as_millis() as u64,
            next_deadline_ms: self
                .scroll_clock_deadline(now)
                .map(|d| d.as_millis() as u64),
            mode: cfg.mode,
            events_per_tick: cfg.events_per_tick,
            wheel_lines_per_tick: cfg.wheel_lines_per_tick,
            trackpad_lines_per_tick: cfg.trackpad_lines_per_tick,
            invert: cfg.invert_direction,
            speed_multiplier: cfg.speed_multiplier,
            viewport_height: cfg.viewport_height,
            flush_cap: cfg.flush_cap(),
            cadence_ms: self.redraw_cadence.as_millis() as u64,
        }
    }

    /// Fresh recorder with its own time origin; no file until the first record. Drop flushes the writer. `None` when now off.
    pub fn toggle_scroll_log(&mut self) -> Option<std::path::PathBuf> {
        if self.recorder.is_some() {
            self.recorder = None;
            return None;
        }
        let path = crate::input::scroll_log::default_log_path();
        self.recorder = Some(ScrollLogRecorder::new(path.clone(), Instant::now()));
        Some(path)
    }

    /// Whether the flight recorder is active (the `/debug` status line).
    pub fn scroll_log_active(&self) -> bool {
        self.recorder.is_some()
    }

    /// Direction flips discard the backlog so stale old-direction lines never land after the reverse. Gap/regrasp keep the flush.
    fn finalize_stream_at(
        &mut self,
        now: Instant,
        stream: &mut ScrollStream,
        cancel_backlog: bool,
    ) -> i32 {
        let carry_at_flush = self.carry_lines;
        // The classification flip may settle the carry rule below, but must never mint new demand after input ended (a post-gesture burst)
        let desired_before = stream.desired_lines_f32(carry_at_flush);
        stream.finalize_kind();
        stream.limit_finalize_reprice(desired_before, carry_at_flush);
        let lines = if cancel_backlog {
            0
        } else {
            // Any remaining catch-up is coast-shaped (no input since the last flush), so flush_lines_at tapers and budgets it
            // The finalize cannot slam a full cap after the fingers stop
            Self::flush_lines_at(&mut self.last_redraw_at, carry_at_flush, now, stream)
        };

        if stream.kind != ScrollStreamKind::Wheel && stream.config.mode != ScrollInputMode::Wheel {
            // Only carry the sub-line fractional remainder, not the cap-induced integer backlog
            // Carrying whole lines would pollute the next gesture with a burst it didn't earn
            let remainder = stream.desired_lines_f32(carry_at_flush) - stream.applied_lines as f32;
            self.carry_lines = remainder.fract();
        } else {
            self.carry_lines = 0.0;
        }

        // Flight-recorder line, priced with the flush-time carry so its desired/backlog/dropped agree with the flush it describes
        Self::record_scroll_log(
            &mut self.recorder,
            now,
            ScrollLogEvt::Finalize,
            ScrollLogTrigger::Finalize,
            stream,
            carry_at_flush,
            lines,
        );

        // HUD breadcrumb only, recorded after the finalize flush so applied_lines is the stream's delivered total
        self.last_finalized = Some(ScrollStreamSummary {
            kind: stream.kind.label(),
            events: stream.event_count,
            applied_lines: stream.applied_lines,
        });

        lines
    }

    fn flush_lines_at(
        last_redraw_at: &mut Instant,
        carry_lines: f32,
        now: Instant,
        stream: &mut ScrollStream,
    ) -> i32 {
        // A zero-delivery flush must not advance last_redraw_at, or the clock busy-spins. `flushable_now` is the shared predicate.
        // Every kind shares the cap; excess drains later, and whatever the coast budget cannot honor is dropped at finalize.
        let delta = stream.flushable_now(carry_lines);
        if delta == 0 {
            return 0;
        }
        if stream.coasting() {
            stream.coast_spent += delta.abs();
        }
        stream.applied_lines = stream.applied_lines.saturating_add(delta);
        stream.events_at_flush = stream.event_count;
        *last_redraw_at = now;
        delta
    }

    /// After the flush so totals are post-flush. `carry` is the value that priced `desired`. Finalize drop is `backlog_after` by construction.
    fn record_scroll_log(
        recorder: &mut Option<ScrollLogRecorder>,
        now: Instant,
        evt: ScrollLogEvt,
        trigger: ScrollLogTrigger,
        stream: &ScrollStream,
        carry: f32,
        flushed: i32,
    ) {
        let Some(recorder) = recorder.as_mut() else {
            return;
        };
        let backlog_after = stream.effective_pending(carry);
        recorder.record(
            now,
            ScrollLogEvent {
                evt,
                trigger,
                kind: stream.kind.label(),
                events_total: stream.event_count,
                avg_interval_ms: stream.avg_interval_ms(),
                accel: stream.interval_accel(),
                desired: stream.desired_lines_f32(carry),
                applied_total: stream.applied_lines,
                flushed,
                backlog_after,
                carry,
                cap: stream.config.flush_cap(),
                dropped: (evt == ScrollLogEvt::Finalize).then_some(backlog_after),
                config: (evt == ScrollLogEvt::StreamStart).then(|| ScrollLogConfigEcho {
                    mode: stream.config.mode.label(),
                    ept: stream.config.events_per_tick,
                    wheel_lpt: stream.config.wheel_lines_per_tick,
                    trackpad_lpt: stream.config.trackpad_lines_per_tick,
                    invert: stream.config.invert_direction,
                    speed: stream.config.speed_multiplier,
                    viewport_height: stream.config.viewport_height,
                }),
            },
        );
    }

    /// `None` means no tick: no stream, or past the gap with nothing to drain. The public clock maps overdue to "tick now", not "never".
    fn next_tick_in(&self, now: Instant) -> Option<Duration> {
        let stream = self.stream.as_ref()?;
        let gap = now.duration_since(stream.last);

        // Deadline only when a flush would apply lines. `desired != applied` includes no-ops that leave last_redraw_at stale and busy-spin the clock.
        let flushable = stream.flushable_now(self.carry_lines) != 0;
        let since_redraw = now.duration_since(self.last_redraw_at);
        let until_redraw = self.redraw_cadence.saturating_sub(since_redraw);

        if gap > STREAM_GAP {
            // Post-gap tapered drain rides the redraw cadence until it runs dry; only then does the deadline collapse to "finalize now"
            return flushable.then_some(until_redraw);
        }

        let mut next = STREAM_GAP.saturating_sub(gap);
        if flushable {
            next = next.min(until_redraw);
        }
        Some(next)
    }
}

impl Default for MouseScrollState {
    fn default() -> Self {
        // One clock zero for stream timing and the recorder's ts_ms.
        // Cadence is always the 16ms default here (hermetic for AppView / dispatch fixtures)
        // Production injects the env override via set_redraw_cadence
        let now = Instant::now();
        Self {
            stream: None,
            last_redraw_at: now,
            carry_lines: 0.0,
            carry_direction: None,
            last_finalized: None,
            recorder: ScrollLogRecorder::from_env_at(now),
            redraw_cadence: REDRAW_CADENCE,
        }
    }
}

#[derive(Clone, Debug)]
struct ScrollStream {
    start: Instant,
    last: Instant,
    direction: ScrollDirection,
    event_count: usize,
    accumulated_events: i32,
    applied_lines: i32,
    config: ScrollConfig,
    kind: ScrollStreamKind,
    just_promoted: bool,
    /// Rolling window of inter-event intervals (ms) for acceleration.
    interval_history: VecDeque<f32>,
    interval_sum: f32,
    /// Sum of per-event accel-weighted contributions (each event's sign times the multiplier in effect when it arrived).
    /// Monotone in magnitude within a stream; see [`Self::desired_lines_f32`].
    /// Confirmed-trackpad demand is truncated at accumulation time ([`Self::clamp_trackpad_demand`]).
    accel_weighted_events: f32,
    /// Updates only on nonzero flushes, matching the recorder. A flush with no new events is a coast flush that [`Self::flushable_now`] tapers.
    events_at_flush: usize,
    /// Whole lines already delivered by coast flushes.
    /// Budgeted at one [`ScrollConfig::flush_cap`] per stream: total motion after input stops is at most one cap, delivered tapered.
    /// That is the I-SMOOTH-COAST bound, held by construction rather than by lucky flush timing.
    coast_spent: i32,
}

impl ScrollStream {
    fn new(now: Instant, direction: ScrollDirection, config: ScrollConfig) -> Self {
        Self {
            start: now,
            last: now,
            direction,
            event_count: 0,
            accumulated_events: 0,
            applied_lines: 0,
            config,
            kind: ScrollStreamKind::Unknown,
            just_promoted: false,
            interval_history: VecDeque::with_capacity(ACCEL_HISTORY_SIZE),
            interval_sum: 0.0,
            accel_weighted_events: 0.0,
            events_at_flush: 0,
            coast_spent: 0,
        }
    }

    fn push_event(&mut self, now: Instant, direction: ScrollDirection) {
        // Record inter-event interval for acceleration (O(1) with running sum)
        // Sub-6ms intervals are batching artifacts (see ACCEL_MIN_INTERVAL_MS)
        // They count for line accumulation below but must not feed the interval window
        let interval_ms = now.duration_since(self.last).as_secs_f32() * 1000.0;
        if self.event_count > 0 && interval_ms >= ACCEL_MIN_INTERVAL_MS {
            self.interval_history.push_back(interval_ms);
            self.interval_sum += interval_ms;
            if self.interval_history.len() > ACCEL_HISTORY_SIZE
                && let Some(old) = self.interval_history.pop_front()
            {
                self.interval_sum -= old;
            }
        }

        self.last = now;
        self.direction = direction;
        self.event_count = self.event_count.saturating_add(1);
        self.accumulated_events = self.accumulated_events.saturating_add(direction.sign());
        // Weight each event by the multiplier in effect when it arrived, so a mid-gesture accel decay can never shrink past contributions
        self.accel_weighted_events += direction.sign() as f32 * self.interval_accel();
        if self.is_confirmed_trackpad() {
            self.clamp_trackpad_demand();
        }
    }

    /// Final-line units one weighted trackpad event prices to.
    /// These are the multipliers [`Self::desired_lines_f32`] applies around `accel_weighted_events` in its confirmed-trackpad branch.
    fn trackpad_line_rate(&self) -> f32 {
        (self.effective_lines_per_tick_f32() / self.config.trackpad_events_per_tick_f32())
            * self.config.speed_multiplier
    }

    /// Accel past one cap of backlog never enters desired: it could only arrive after the fingers stop. Clamp-on-read would re-admit it.
    /// Raw-pricing floor keeps the accel-free total. Ceilings are monotone so desired stays monotone.
    fn clamp_trackpad_demand(&mut self) {
        let rate = self.trackpad_line_rate();
        if rate <= f32::EPSILON {
            return;
        }
        let raw_lines = self.accumulated_events.abs() as f32 * rate;
        let honorable = (self.applied_lines.abs() + self.config.flush_cap()) as f32;
        let ceiling = raw_lines.max(honorable);
        if self.accel_weighted_events.abs() * rate > ceiling {
            self.accel_weighted_events = self.accel_weighted_events.signum() * ceiling / rate;
        }
    }

    /// Average inter-event interval from recent history (ms).
    fn avg_interval_ms(&self) -> Option<f32> {
        if self.interval_history.is_empty() {
            return None;
        }
        Some(self.interval_sum / self.interval_history.len() as f32)
    }

    /// Interval-based acceleration multiplier. Fast events get a higher multiplier.
    fn interval_accel(&self) -> f32 {
        let Some(avg) = self.avg_interval_ms() else {
            return ACCEL_MULTIPLIER_BASE;
        };
        let fast = self.config.accel_interval_fast_ms;
        let medium = self.config.accel_interval_medium_ms;
        let raw = if avg <= fast {
            ACCEL_MULTIPLIER_FAST
        } else if avg <= medium {
            // Linear interpolation between fast and medium bands.
            let t = (avg - fast) / (medium - fast);
            ACCEL_MULTIPLIER_FAST + t * (ACCEL_MULTIPLIER_MEDIUM - ACCEL_MULTIPLIER_FAST)
        } else {
            ACCEL_MULTIPLIER_BASE
        };
        raw.clamp(ACCEL_MULTIPLIER_BASE, self.config.trackpad_accel_max_f32())
    }

    fn maybe_promote_kind(&mut self, now: Instant) {
        if self.config.mode != ScrollInputMode::Auto {
            return;
        }
        if self.kind != ScrollStreamKind::Unknown {
            return;
        }

        let events_per_tick = self.config.events_per_tick.max(1) as usize;

        // ept=1 terminals: a wheel notch is 1 event at ~50-100ms intervals.
        // Trackpad events arrive more rapidly. Threshold is per-terminal.
        if events_per_tick <= 1
            && self.event_count > 2
            && self
                .avg_interval_ms()
                .is_some_and(|avg| avg < self.config.trackpad_detect_max_interval_ms)
        {
            self.kind = ScrollStreamKind::Trackpad;
            return;
        }

        if events_per_tick >= 2 && self.event_count >= events_per_tick {
            let elapsed = now.duration_since(self.start);
            if elapsed <= self.config.wheel_tick_detect_max {
                self.kind = ScrollStreamKind::Wheel;
                self.just_promoted = true;
            }
        }
    }

    fn finalize_kind(&mut self) {
        match self.config.mode {
            ScrollInputMode::Wheel => self.kind = ScrollStreamKind::Wheel,
            ScrollInputMode::Trackpad => self.kind = ScrollStreamKind::Trackpad,
            ScrollInputMode::Auto => {
                if self.kind != ScrollStreamKind::Unknown {
                    return;
                }
                let duration = self.last.duration_since(self.start);
                if self.config.events_per_tick <= 1
                    && self.event_count <= 2
                    && duration <= self.config.wheel_like_max_duration
                {
                    self.kind = ScrollStreamKind::Wheel;
                } else {
                    self.kind = ScrollStreamKind::Trackpad;
                }
            }
        }
    }

    /// Unknown-to-Trackpad must not mint demand after the fingers stop. Upward re-price is undone; downward keeps the lower value (a pause, not a bounce).
    fn limit_finalize_reprice(&mut self, desired_before: f32, carry_lines: f32) {
        if !self.is_confirmed_trackpad() {
            return;
        }
        let rate = self.trackpad_line_rate();
        if rate <= f32::EPSILON {
            return;
        }
        if self.desired_lines_f32(carry_lines).abs() > desired_before.abs() {
            // Solve the confirmed-trackpad pricing for the weighted count that lands desired exactly on its pre-flip value
            // Carry rides outside the multipliers per the unit invariant
            self.accel_weighted_events = (desired_before - carry_lines) / rate;
        }
    }

    fn is_wheel_like(&self) -> bool {
        match self.config.mode {
            ScrollInputMode::Wheel => true,
            ScrollInputMode::Trackpad => false,
            // ept<=1 Auto: treat Unknown as wheel until trackpad promotion fires
            ScrollInputMode::Auto => {
                matches!(self.kind, ScrollStreamKind::Wheel)
                    || (self.kind == ScrollStreamKind::Unknown && self.config.events_per_tick <= 1)
            }
        }
    }

    fn effective_lines_per_tick_f32(&self) -> f32 {
        match self.config.mode {
            ScrollInputMode::Wheel => self.config.wheel_lines_per_tick_f32(),
            ScrollInputMode::Trackpad => self.config.trackpad_lines_per_tick_f32(),
            ScrollInputMode::Auto => match self.kind {
                ScrollStreamKind::Wheel => self.config.wheel_lines_per_tick_f32(),
                ScrollStreamKind::Trackpad => self.config.trackpad_lines_per_tick_f32(),
                ScrollStreamKind::Unknown => {
                    // For ept<=1 terminals, assume an unclassified event is a wheel notch
                    if self.config.events_per_tick <= 1 {
                        self.config.wheel_lines_per_tick_f32()
                    } else {
                        self.config.trackpad_lines_per_tick_f32()
                    }
                }
            },
        }
    }

    fn is_confirmed_trackpad(&self) -> bool {
        match self.config.mode {
            ScrollInputMode::Trackpad => true,
            ScrollInputMode::Wheel => false,
            ScrollInputMode::Auto => matches!(self.kind, ScrollStreamKind::Trackpad),
        }
    }

    fn desired_lines_f32(&self, carry_lines: f32) -> f32 {
        // Use the normalized ept=3 divisor only for confirmed trackpad.
        // Unknown streams (not yet classified) use the terminal's real events_per_tick so wheel input on ept=1 terminals isn't penalized
        let events_per_tick = if self.is_confirmed_trackpad() {
            self.config.trackpad_events_per_tick_f32()
        } else {
            self.config.events_per_tick_f32()
        };
        let lines_per_tick = self.effective_lines_per_tick_f32();

        // No intermediate clamp: a fixed cap freezes a long trackpad once applied catches up. Per-flush clamp is in flush_lines_at.
        // Final line units. Carry is added after every multiplier so accel and speed do not re-amplify it.
        if self.is_confirmed_trackpad() {
            // Accel is applied per event as it arrives (accel_weighted_events), not retroactively to the whole stream
            // Multiplying the total by the CURRENT multiplier let a decaying fast-to-slow gesture pull desired below applied, stalling mid-stream
            self.accel_weighted_events
                * (lines_per_tick / events_per_tick)
                * self.config.speed_multiplier
                + carry_lines
        } else {
            self.accumulated_events as f32
                * (lines_per_tick / events_per_tick)
                * self.config.speed_multiplier
        }
    }

    /// Whether a flush right now would be a COAST flush: no events arrived since the last line-delivering flush.
    /// Any lines it moves are post-input catch-up (the recorder's `events_since_flush == 0`).
    fn coasting(&self) -> bool {
        self.event_count == self.events_at_flush
    }

    /// Shared pending predicate for flush and deadline. Divergence leaves `last_redraw_at` stale and busy-spins the clock.
    /// Coast halves the remainder per tick (deceleration, not a slam), floored at one line, budgeted at one cap per stream.
    fn flushable_now(&self, carry_lines: f32) -> i32 {
        let pending = self.effective_pending(carry_lines);
        let cap = self.config.flush_cap();
        let magnitude = if self.coasting() {
            let taper = (pending.abs() / 2).max(self.effective_lines_per_tick_f32() as i32);
            pending
                .abs()
                .min(taper)
                .min((cap - self.coast_spent).max(0))
        } else {
            pending.abs().min(cap)
        };
        pending.signum() * magnitude
    }

    /// Whole-line delta a flush right now would apply, before the per-flush cap.
    /// This is truncated desired minus applied, with the wheel-like minimum-line substitution and the direction clamp.
    /// The recorder's `backlog_after`/`dropped` and the HUD backlog read this raw backlog; delivery decisions go through [`Self::flushable_now`].
    fn effective_pending(&self, carry_lines: f32) -> i32 {
        let mut desired_lines = self.desired_lines_f32(carry_lines).trunc() as i32;

        // Wheel-like streams always deliver at least one line per gesture.
        if self.is_wheel_like() && desired_lines == 0 && self.accumulated_events != 0 {
            desired_lines = self.accumulated_events.signum() * MIN_LINES_PER_WHEEL_STREAM;
        }

        let mut delta = desired_lines - self.applied_lines;
        // Never flush against the gesture. Promotion re-price can land desired below applied; the clamp turns that into a pause, not a bounce.
        if self.accumulated_events > 0 {
            delta = delta.max(0);
        } else if self.accumulated_events < 0 {
            delta = delta.min(0);
        }
        delta
    }
}

#[cfg(test)]
#[path = "mouse/tests.rs"]
mod tests;
