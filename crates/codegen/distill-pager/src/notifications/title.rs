// Modified for Distill by Samuel Fajreldines, 2026.
use std::fmt::Write;

use crossterm::terminal::SetTitle;

use super::config::{TitleConfig, TitleItem};
use crate::acp::tracker::TurnActivity;

const TITLE_SPINNER: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
];

/// Hold each spinner frame for this many ticks before advancing. Terminals (notably Ghostty) debounce tab title
/// updates. Writing a new title every tick (~33ms at 30fps) produces more OSC 0 writes than the tab bar can render.
/// A divisor of 8 gives ~264ms per frame, slow enough for debounced renderers while still looking animated.
const TITLE_SPINNER_DIVISOR: u64 = 8;

/// Hold the "⚠ Action Required" label for this many ticks before toggling (only while unfocused; see the focused field below).
/// A divisor of 15 at 30fps gives ~500ms visible, ~500ms hidden: a calm 1s blink cycle that reads as intentional rather than broken flickering.
/// When focused we show the prefix statically to eliminate oscillation during active interaction (e.g. typing in permission modals).
const ACTION_REQUIRED_BLINK_DIVISOR: u64 = 15;

/// State passed into `TitleManager::update()` each tick.
pub struct TitleState<'a> {
    pub session_name: Option<&'a str>,
    pub model: Option<&'a str>,
    pub activity: Option<&'a TurnActivity>,
    pub has_pending_permissions: bool,
    pub cwd: Option<&'a str>,
    pub turn_elapsed: Option<std::time::Duration>,
    /// Whether the agent is busy (turn or command running), even if `activity` is `None` (the "Waiting" gap before first chunk).
    pub is_busy: bool,
    /// Whether the terminal pane/window is currently focused (from FocusTracker).
    /// Suppresses title blinking/oscillation while the user is actively interacting.
    pub focused: bool,
}

pub struct TitleManager {
    items: Vec<TitleItem>,
    last_title: String,
    composed: String,
    spinner_frame: usize,
    tick_count: u64,
}

impl TitleManager {
    pub fn new(config: &TitleConfig) -> Self {
        Self {
            items: config.items.clone(),
            last_title: String::new(),
            composed: String::new(),
            spinner_frame: 0,
            tick_count: 0,
        }
    }

    /// Compose the title string from the current state.
    /// Returns the escape sequence bytes to set the terminal title when the composed title differs from the last one emitted.
    /// Returns `None` when the title is unchanged (dedup).
    pub fn update(&mut self, state: &TitleState<'_>) -> Option<String> {
        self.composed.clear();
        let mut has_parts = false;

        for item in self.items.iter().copied() {
            if write_item(
                &mut self.composed,
                &mut has_parts,
                item,
                state,
                self.spinner_frame,
                self.tick_count,
            ) {
                continue;
            }
        }

        if !has_parts {
            self.composed.clear();
            self.composed.push_str("Distill");
        }

        let result = if self.composed != self.last_title {
            Some(build_title_escape(&self.composed))
        } else {
            None
        };

        if result.is_some() {
            std::mem::swap(&mut self.last_title, &mut self.composed);
        }

        // Advance counters after rendering so the first tick sees tick_count=0 (phase 0, ActionRequired visible) and spinner_frame=0
        self.tick_count = self.tick_count.wrapping_add(1);
        self.spinner_frame =
            (self.tick_count / TITLE_SPINNER_DIVISOR) as usize % TITLE_SPINNER.len();

        result
    }

    pub fn reset(&mut self) -> String {
        let esc = build_title_escape("Distill");
        self.last_title.clear();
        self.last_title.push_str("Distill");
        self.spinner_frame = 0;
        self.tick_count = 0;
        esc
    }
}

/// Render a single title item into `buf`. Returns `true` if a part was written.
fn write_item(
    buf: &mut String,
    has_parts: &mut bool,
    item: TitleItem,
    state: &TitleState<'_>,
    spinner_frame: usize,
    tick_count: u64,
) -> bool {
    match item {
        TitleItem::Distill => {
            push_separator(buf, has_parts);
            buf.push_str("Distill");
        }
        TitleItem::Spinner => {
            if !state.is_busy && state.activity.is_none() {
                return false;
            }
            let Some(&ch) = TITLE_SPINNER.get(spinner_frame) else {
                return false;
            };
            push_separator(buf, has_parts);
            buf.push(ch);
        }
        TitleItem::Activity => {
            if let Some(activity) = state.activity {
                push_separator(buf, has_parts);
                write_activity(buf, activity);
            } else if state.is_busy {
                push_separator(buf, has_parts);
                buf.push_str("Waiting");
            } else {
                return false;
            }
        }
        TitleItem::SessionName => {
            let Some(name) = state.session_name.filter(|s| !s.is_empty()) else {
                return false;
            };
            push_separator(buf, has_parts);
            write_truncated(buf, name, 40);
        }
        TitleItem::Model => {
            let Some(model) = state.model.filter(|s| !s.is_empty()) else {
                return false;
            };
            push_separator(buf, has_parts);
            write_truncated(buf, model, 30);
        }
        TitleItem::Cwd => {
            let Some(cwd) = state.cwd else {
                return false;
            };
            let short = cwd.rsplit('/').next().unwrap_or(cwd);
            if short.is_empty() {
                return false;
            }
            push_separator(buf, has_parts);
            write_truncated(buf, short, 30);
        }
        TitleItem::TurnTimer => {
            let Some(elapsed) = state.turn_elapsed else {
                return false;
            };
            let secs = elapsed.as_secs();
            if secs < 1 {
                return false;
            }
            push_separator(buf, has_parts);
            let _ = write!(buf, "{}s", secs);
        }
        TitleItem::ActionRequired => {
            if !state.has_pending_permissions {
                return false;
            }
            // Blink (oscillate) only while unfocused, for tab attention.
            // When focused (user actively interacting, e.g. in permission modal or prompt), show static prefix to stop distracting flash.
            let should_blink =
                !state.focused && !(tick_count / ACTION_REQUIRED_BLINK_DIVISOR).is_multiple_of(2);
            if should_blink {
                return false;
            }
            push_separator(buf, has_parts);
            buf.push_str("\u{26A0} Action Required");
        }
    }
    *has_parts = true;
    true
}

fn push_separator(buf: &mut String, has_parts: &mut bool) {
    if *has_parts {
        buf.push_str(" - ");
    }
}

fn write_activity(buf: &mut String, activity: &TurnActivity) {
    match activity {
        TurnActivity::Thinking => buf.push_str("Thinking"),
        TurnActivity::Responding => buf.push_str("Responding"),
        TurnActivity::ToolRunning { title, description } => {
            if let Some(desc) = description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                buf.push_str(&crate::acp::tracker::format_waiting_for_subject(desc));
            } else if title.is_empty() {
                buf.push_str("Running tool");
            } else {
                buf.push_str("Running: ");
                write_truncated(buf, title, 30);
            }
        }
        TurnActivity::AutoCompacting => buf.push_str("Compacting"),
        TurnActivity::Retrying {
            attempt,
            max_retries,
            reason,
            error_type,
        } => {
            buf.push_str(&crate::app::error_display::format_retry_activity_label(
                *attempt,
                *max_retries,
                reason,
                error_type.as_deref(),
                crate::app::error_display::RetryLabelStyle::Compact,
            ));
        }
        TurnActivity::WritingToolCall(writing) => buf.push_str(&writing.label()),
        TurnActivity::Waiting(reason) => buf.push_str(&reason.label()),
    }
}

fn write_truncated(buf: &mut String, s: &str, max: usize) {
    // Fast path: ASCII-only strings where byte length == char count.
    if s.len() <= max {
        buf.push_str(s);
        return;
    }
    // Slow path: iterate chars for multi-byte or over-limit strings.
    for (count, ch) in s.chars().enumerate() {
        if count >= max {
            buf.push('\u{2026}');
            return;
        }
        buf.push(ch);
    }
}

/// Build the escape sequence for setting the terminal title without writing it to stderr. Those must not terminate
/// the OSC sequence early or inject escapes into the terminal.
fn build_title_escape(title: &str) -> String {
    let sanitized: String = title.chars().filter(|c| !c.is_control()).collect();
    let mut buf = Vec::new();
    let _ = crossterm::queue!(&mut buf, SetTitle(sanitized));
    String::from_utf8(buf).expect("crossterm SetTitle produces valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> TitleConfig {
        TitleConfig::default()
    }

    fn config_with_items(items: Vec<TitleItem>) -> TitleConfig {
        TitleConfig {
            enabled: true,
            items,
        }
    }

    fn idle_state<'a>() -> TitleState<'a> {
        TitleState {
            session_name: None,
            model: None,
            activity: None,
            has_pending_permissions: false,
            cwd: None,
            turn_elapsed: None,
            is_busy: false,
            focused: true,
        }
    }

    #[test]
    fn grok_only_produces_just_grok() {
        let cfg = config_with_items(vec![TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = idle_state();
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn session_name_and_grok_joined_with_separator() {
        let cfg = config_with_items(vec![TitleItem::SessionName, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            session_name: Some("my project"),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "my project - Distill");
    }

    #[test]
    fn missing_session_name_skipped() {
        let cfg = config_with_items(vec![TitleItem::SessionName, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = idle_state();
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn empty_session_name_skipped() {
        let cfg = config_with_items(vec![TitleItem::SessionName, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            session_name: Some(""),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn spinner_only_shown_when_active() {
        let cfg = config_with_items(vec![TitleItem::Spinner, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);

        mgr.update(&idle_state());
        assert_eq!(mgr.last_title, "Distill");

        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert!(mgr.last_title.contains(" - Distill"));
        let spinner_part: String = mgr.last_title.chars().take(1).collect();
        assert!(
            TITLE_SPINNER.contains(&spinner_part.chars().next().unwrap()),
            "expected braille spinner char, got: {}",
            spinner_part
        );
    }

    #[test]
    fn spinner_advances_with_divisor() {
        let cfg = config_with_items(vec![TitleItem::Spinner]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };

        // Run through one full cycle (DIVISOR ticks per frame * frame count).
        let total = TITLE_SPINNER_DIVISOR as usize * TITLE_SPINNER.len();
        let mut frames = Vec::new();
        for _ in 0..total {
            mgr.update(&state);
            frames.push(mgr.last_title.clone());
        }
        // Across a full cycle we should see all spinner frames.
        let unique: std::collections::HashSet<_> = frames.iter().collect();
        assert_eq!(unique.len(), TITLE_SPINNER.len());
    }

    #[test]
    fn spinner_holds_frame_for_divisor_ticks() {
        let cfg = config_with_items(vec![TitleItem::Spinner]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };

        // First frame should be stable for DIVISOR ticks.
        mgr.update(&state);
        let first = mgr.last_title.clone();
        for _ in 1..TITLE_SPINNER_DIVISOR {
            mgr.update(&state);
            assert_eq!(
                mgr.last_title, first,
                "spinner should hold frame during divisor window"
            );
        }
        // After DIVISOR ticks, the frame should advance.
        mgr.update(&state);
        assert_ne!(
            mgr.last_title, first,
            "spinner should advance after divisor ticks"
        );
    }

    #[test]
    fn spinner_wraps_around() {
        let cfg = config_with_items(vec![TitleItem::Spinner]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };

        // Run through more than one full cycle.
        mgr.update(&state);
        let first = mgr.last_title.clone();
        let total = TITLE_SPINNER_DIVISOR as usize * TITLE_SPINNER.len();
        for _ in 1..total {
            mgr.update(&state);
        }
        // After a full cycle, the frame should wrap back to the first char.
        mgr.update(&state);
        assert_eq!(mgr.last_title, first);
    }

    #[test]
    fn activity_label_thinking() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Thinking");
    }

    #[test]
    fn activity_label_responding() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Responding;
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Responding");
    }

    #[test]
    fn activity_label_tool_running_with_title() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::ToolRunning {
            title: "cargo build".to_owned(),
            description: None,
        };
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Running: cargo build");
    }

    #[test]
    fn activity_label_tool_running_empty_title() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::ToolRunning {
            title: String::new(),
            description: None,
        };
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Running tool");
    }

    #[test]
    fn activity_label_retrying() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Retrying {
            attempt: 2,
            max_retries: 5,
            reason: "API error (status 504 Gateway Timeout): upstream timeout".to_owned(),
            error_type: None,
        };
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Request timed out (504) | Retrying (2/5)");
    }

    #[test]
    fn activity_hidden_when_idle() {
        let cfg = config_with_items(vec![TitleItem::Activity, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        mgr.update(&idle_state());
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn spinner_shown_when_busy_without_activity() {
        let cfg = config_with_items(vec![TitleItem::Spinner, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            is_busy: true,
            ..idle_state()
        };
        mgr.update(&state);
        assert!(mgr.last_title.contains(" - Distill"));
        let spinner_part: String = mgr.last_title.chars().take(1).collect();
        assert!(
            TITLE_SPINNER.contains(&spinner_part.chars().next().unwrap()),
            "expected braille spinner char during Waiting, got: {}",
            spinner_part
        );
    }

    #[test]
    fn activity_shows_waiting_when_busy_without_activity() {
        let cfg = config_with_items(vec![TitleItem::Activity, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            is_busy: true,
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Waiting - Distill");
    }

    #[test]
    fn activity_prefers_specific_activity_over_waiting() {
        let cfg = config_with_items(vec![TitleItem::Activity, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            activity: Some(&activity),
            is_busy: true,
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Thinking - Distill");
    }

    #[test]
    fn action_required_visible_on_first_tick() {
        let cfg = config_with_items(vec![TitleItem::ActionRequired, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            has_pending_permissions: true,
            ..idle_state()
        };

        // tick_count=0 (even) on first render, so ActionRequired is visible
        mgr.update(&state);
        assert!(
            mgr.last_title.contains("Action Required"),
            "first tick should show ActionRequired, got: {}",
            mgr.last_title
        );
    }

    #[test]
    fn action_required_blinks_across_ticks() {
        let cfg = config_with_items(vec![TitleItem::ActionRequired, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            has_pending_permissions: true,
            focused: false, // unfocused, so it blinks
            ..idle_state()
        };

        // First tick: tick_count=0, phase=0 (visible).
        mgr.update(&state);
        let t1 = mgr.last_title.clone();

        // Title stays stable for the rest of the visible phase.
        for _ in 1..ACTION_REQUIRED_BLINK_DIVISOR {
            mgr.update(&state);
            assert_eq!(
                mgr.last_title, t1,
                "title should stay stable within a blink phase"
            );
        }

        // Crossing into the hidden phase.
        mgr.update(&state);
        let t2 = mgr.last_title.clone();

        assert_ne!(t1, t2);
        assert!(t1.contains("Action Required"));
        assert!(!t2.contains("Action Required"));
    }

    #[test]
    fn action_required_hidden_when_no_permissions() {
        let cfg = config_with_items(vec![TitleItem::ActionRequired, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            has_pending_permissions: false,
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn dedup_skips_emission_when_unchanged() {
        let cfg = config_with_items(vec![TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = idle_state();

        let first = mgr.update(&state);
        assert!(first.is_some());
        assert_eq!(mgr.last_title, "Distill");

        assert_eq!(mgr.update(&state), None);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn empty_items_produces_grok_fallback() {
        let cfg = config_with_items(vec![]);
        let mut mgr = TitleManager::new(&cfg);
        mgr.update(&idle_state());
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn model_item_shown_when_present() {
        let cfg = config_with_items(vec![TitleItem::Model, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            model: Some("grok-3"),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "grok-3 - Distill");
    }

    #[test]
    fn model_item_hidden_when_none() {
        let cfg = config_with_items(vec![TitleItem::Model, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        mgr.update(&idle_state());
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn cwd_shows_last_component() {
        let cfg = config_with_items(vec![TitleItem::Cwd, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            cwd: Some("/home/user/my-project"),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "my-project - Distill");
    }

    #[test]
    fn turn_timer_shown_when_above_one_second() {
        let cfg = config_with_items(vec![TitleItem::TurnTimer, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            turn_elapsed: Some(std::time::Duration::from_secs(42)),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "42s - Distill");
    }

    #[test]
    fn turn_timer_hidden_when_under_one_second() {
        let cfg = config_with_items(vec![TitleItem::TurnTimer, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            turn_elapsed: Some(std::time::Duration::from_millis(500)),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn long_session_name_truncated_with_ellipsis() {
        let cfg = config_with_items(vec![TitleItem::SessionName]);
        let mut mgr = TitleManager::new(&cfg);
        let long_name = "a".repeat(50);
        let state = TitleState {
            session_name: Some(&long_name),
            ..idle_state()
        };
        mgr.update(&state);
        // 40 chars + ellipsis
        assert_eq!(mgr.last_title.chars().count(), 41);
        assert!(mgr.last_title.ends_with('\u{2026}'));
    }

    #[test]
    fn short_session_name_not_truncated() {
        let cfg = config_with_items(vec![TitleItem::SessionName]);
        let mut mgr = TitleManager::new(&cfg);
        let state = TitleState {
            session_name: Some("short"),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(mgr.last_title, "short");
    }

    #[test]
    fn reset_clears_state_and_emits_grok() {
        let cfg = config_with_items(vec![TitleItem::SessionName, TitleItem::Distill]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            session_name: Some("test"),
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        assert_ne!(mgr.last_title, "Distill");

        mgr.reset();
        assert_eq!(mgr.last_title, "Distill");
        assert_eq!(mgr.spinner_frame, 0);
        assert_eq!(mgr.tick_count, 0);
    }

    #[test]
    fn default_config_active_turn_with_permissions() {
        let cfg = default_config();
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Responding;
        let state = TitleState {
            session_name: Some("my-session"),
            activity: Some(&activity),
            has_pending_permissions: true,
            focused: false, // unfocused, so it blinks
            ..idle_state()
        };

        // First tick: ActionRequired visible.
        mgr.update(&state);
        let t1 = mgr.last_title.clone();

        // Advance through the blink divisor to reach the hidden phase.
        for _ in 1..ACTION_REQUIRED_BLINK_DIVISOR {
            mgr.update(&state);
        }
        mgr.update(&state);
        let t2 = mgr.last_title.clone();

        // Both should contain the persistent parts.
        for t in [&t1, &t2] {
            assert!(t.contains("Distill"), "title missing 'grok': {t}");
            assert!(t.contains("Responding"), "title missing 'Responding': {t}");
            assert!(t.contains("my-session"), "title missing session name: {t}");
        }
        // One should have ActionRequired, the other should not (blinking).
        let w1 = t1.contains("Action Required");
        let w2 = t2.contains("Action Required");
        assert_ne!(w1, w2, "expected blink toggle between t1={t1} and t2={t2}");
    }

    #[test]
    fn default_config_idle_no_session() {
        let cfg = default_config();
        let mut mgr = TitleManager::new(&cfg);
        mgr.update(&idle_state());
        assert_eq!(mgr.last_title, "Distill");
    }

    #[test]
    fn all_items_present_in_order() {
        let cfg = config_with_items(vec![
            TitleItem::Activity,
            TitleItem::SessionName,
            TitleItem::Model,
            TitleItem::Cwd,
            TitleItem::Distill,
        ]);
        let mut mgr = TitleManager::new(&cfg);
        let activity = TurnActivity::Thinking;
        let state = TitleState {
            session_name: Some("proj"),
            model: Some("grok-3"),
            activity: Some(&activity),
            cwd: Some("/home/user/workspace"),
            ..idle_state()
        };
        mgr.update(&state);
        assert_eq!(
            mgr.last_title,
            "Thinking - proj - grok-3 - workspace - Distill"
        );
    }

    #[test]
    fn tool_title_truncated_in_activity() {
        let cfg = config_with_items(vec![TitleItem::Activity]);
        let mut mgr = TitleManager::new(&cfg);
        let long_tool = "x".repeat(50);
        let activity = TurnActivity::ToolRunning {
            title: long_tool,
            description: None,
        };
        let state = TitleState {
            activity: Some(&activity),
            ..idle_state()
        };
        mgr.update(&state);
        // "Running: " (9 chars) + 30 chars + ellipsis = 40 chars
        assert!(mgr.last_title.starts_with("Running: "));
        assert!(mgr.last_title.ends_with('\u{2026}'));
    }

    /// Title parts from remote sources must not smuggle control bytes into the OSC sequence: the only ESC/BEL in the output is crossterm's framing.
    #[test]
    fn title_escape_strips_control_characters() {
        let esc = build_title_escape("evil\u{1b}]0;pwned\u{7}\r\ntitle");
        let inner = esc
            .strip_prefix("\u{1b}]0;")
            .and_then(|s| s.strip_suffix('\u{7}'))
            .expect("crossterm OSC 0 framing");
        assert!(
            !inner.chars().any(char::is_control),
            "title payload must be control-free: {inner:?}"
        );
        assert_eq!(inner, "evil]0;pwnedtitle");
    }
}
