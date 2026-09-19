// Modified for Distill by Samuel Fajreldines, 2026.
//! Next-prompt suggestion controller (tab autocomplete ghost text).
//!
//! After a turn completes, the pager asks the shell (`suggestPrompt`) to predict the user's likely next prompt.
//! The prediction renders as dim ghost text in the (empty) prompt input:
//!
//! - **Tab** or **Right arrow** accepts it.
//!   The ghost only shows with the cursor at end-of-text, where Right is otherwise a no-op (the fish/zsh autosuggestion convention).
//! - Typing a matching prefix *shrinks* the ghost; typing it out fully consumes it; any divergent text hides it.
//!   It comes back if the user clears the input, matching common agent-CLI autosuggest behavior.
//! - **Esc** on an empty prompt dismisses it for the rest of the turn.
//!
//! Visibility is *derived* from the current prompt text each frame ([`PromptSuggestionController::ghost_for`]) rather than mutated on each keystroke.
//! There is no per-keystroke state machine to drift.
//! Stale responses are discarded via a generation counter, mirroring `SuggestionController` (shell command suggestions).

/// Env override for the whole feature: `GROK_PROMPT_SUGGESTIONS=0/1`.
/// When unset, the persisted `prompt_suggestions` setting applies.
pub const PROMPT_SUGGESTIONS_ENV: &str = "GROK_PROMPT_SUGGESTIONS";

/// Env override for the model used by the suggestion call: `GROK_PROMPT_SUGGESTIONS_MODEL=<model-id>`.
pub const PROMPT_SUGGESTIONS_MODEL_ENV: &str = "GROK_PROMPT_SUGGESTIONS_MODEL";

/// Controller for the predicted-next-prompt ghost text.
#[derive(Debug, Default)]
pub struct PromptSuggestionController {
    /// Full suggestion text from the model. Empty means no suggestion.
    full_text: String,
    /// Request generation counter; responses carrying a stale generation are discarded (a newer turn ended, or the suggestion was invalidated).
    generation: u64,
    /// Set when the user dismissed the current suggestion (Esc). Cleared by the next loaded suggestion.
    dismissed: bool,
    /// Set once the `shown` telemetry impression for the current suggestion has been logged. This latch
    /// makes the impression fire exactly once per installed suggestion, at first visibility (divergent
    /// draft cleared, gate re-opened).
    shown_logged: bool,
    /// Whether the feature is enabled. Resolved from `GROK_PROMPT_SUGGESTIONS`, falling back to the persisted `prompt_suggestions` setting.
    pub enabled: bool,
}

impl PromptSuggestionController {
    pub fn new() -> Self {
        Self {
            full_text: String::new(),
            generation: 0,
            dismissed: false,
            shown_logged: false,
            enabled: resolve_enabled(),
        }
    }

    /// Begin a new fetch: invalidates any in-flight request and returns the generation to thread through the effect pipeline.
    pub fn begin_fetch(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// A suggestion arrived from the shell. Discards stale generations and empty payloads.
    /// Returns `true` when the suggestion was installed.
    pub fn on_loaded(&mut self, suggestion: Option<String>, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        match suggestion {
            Some(text) if !text.trim().is_empty() && !text.contains('\n') => {
                self.full_text = text;
                self.dismissed = false;
                self.shown_logged = false;
                true
            }
            _ => {
                self.full_text.clear();
                false
            }
        }
    }

    /// The ghost text to render for the current prompt text, if any.
    pub fn ghost_for(&self, text: &str) -> Option<&str> {
        if !self.enabled || self.dismissed || self.full_text.is_empty() {
            return None;
        }
        let rest = self.full_text.strip_prefix(text)?;
        if rest.is_empty() { None } else { Some(rest) }
    }

    /// Accept the suggestion against the current prompt text. Returns the remainder to insert and clears the suggestion.
    pub fn accept(&mut self, text: &str) -> Option<String> {
        let rest = self.ghost_for(text)?.to_owned();
        self.clear();
        Some(rest)
    }

    /// Dismiss the current suggestion (Esc) until a new one loads.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// Drop the suggestion and invalidate any in-flight fetch (turn started, prompt sent, session switched...).
    pub fn clear(&mut self) {
        self.full_text.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// Whether a (non-dismissed) suggestion is loaded, regardless of the current prompt text.
    pub fn has_suggestion(&self) -> bool {
        self.enabled && !self.dismissed && !self.full_text.is_empty()
    }

    /// Latch the `shown` impression for the current suggestion: returns `true` exactly once per
    /// installed suggestion. This only guards against double-logging when visibility (re-derived per
    /// frame) recurs or is re-checked on a later path.
    pub fn mark_shown_logged(&mut self) -> bool {
        !std::mem::replace(&mut self.shown_logged, true)
    }

    /// Whether the `shown` impression for the current suggestion has been logged already (read-only companion to [`Self::mark_shown_logged`]).
    #[cfg(test)]
    pub(crate) fn shown_logged(&self) -> bool {
        self.shown_logged
    }

    #[cfg(test)]
    pub(crate) fn set_suggestion_for_test(&mut self, text: &str) {
        self.enabled = true;
        self.dismissed = false;
        self.shown_logged = false;
        self.full_text = text.to_owned();
    }
}

/// Uses the shared shell resolver so pager and shell agree.
pub fn resolve_enabled() -> bool {
    distill_shell::util::config::prompt_suggestions_enabled_for_config(
        crate::appearance::cache::load_prompt_suggestions_config(),
    )
}

/// Content-free size metadata for acceptance-rate telemetry: `(chars, words)` of the full suggestion text.
/// Never log the text itself.
pub fn suggestion_size(text: &str) -> (usize, usize) {
    (text.chars().count(), text.split_whitespace().count())
}

/// Reads the optional client model override.
pub fn resolve_model() -> Option<String> {
    std::env::var(PROMPT_SUGGESTIONS_MODEL_ENV)
        .ok()
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded_controller(text: &str) -> PromptSuggestionController {
        let mut c = PromptSuggestionController {
            enabled: true,
            ..Default::default()
        };
        let generation = c.begin_fetch();
        assert!(c.on_loaded(Some(text.to_owned()), generation));
        c
    }

    #[test]
    fn ghost_shows_full_suggestion_on_empty_prompt() {
        let c = loaded_controller("run the tests");
        assert_eq!(c.ghost_for(""), Some("run the tests"));
    }

    #[test]
    fn ghost_shrinks_as_matching_prefix_is_typed() {
        let c = loaded_controller("run the tests");
        assert_eq!(c.ghost_for("r"), Some("un the tests"));
        assert_eq!(c.ghost_for("run the"), Some(" tests"));
    }

    #[test]
    fn ghost_disappears_when_typed_out_fully() {
        let c = loaded_controller("run the tests");
        assert_eq!(c.ghost_for("run the tests"), None);
    }

    #[test]
    fn ghost_hides_on_divergent_text_and_returns_on_clear() {
        let c = loaded_controller("run the tests");
        assert_eq!(c.ghost_for("x"), None);
        assert_eq!(c.ghost_for("rux"), None);
        // Clearing the input brings the suggestion back.
        assert_eq!(c.ghost_for(""), Some("run the tests"));
    }

    #[test]
    fn accept_returns_remainder_and_clears() {
        let mut c = loaded_controller("run the tests");
        assert_eq!(c.accept("run ").as_deref(), Some("the tests"));
        assert!(!c.has_suggestion());
        assert_eq!(c.ghost_for(""), None);
    }

    #[test]
    fn accept_on_divergent_text_returns_none() {
        let mut c = loaded_controller("run the tests");
        assert_eq!(c.accept("xyz"), None);
        // Suggestion intact for when the input is cleared.
        assert!(c.has_suggestion());
    }

    #[test]
    fn dismiss_hides_until_next_load() {
        let mut c = loaded_controller("run the tests");
        c.dismiss();
        assert_eq!(c.ghost_for(""), None);
        assert!(!c.has_suggestion());

        let generation = c.begin_fetch();
        assert!(c.on_loaded(Some("commit this".to_owned()), generation));
        assert_eq!(c.ghost_for(""), Some("commit this"));
    }

    #[test]
    fn stale_generation_is_discarded() {
        let mut c = PromptSuggestionController {
            enabled: true,
            ..Default::default()
        };
        let stale = c.begin_fetch();
        let _newer = c.begin_fetch();
        assert!(!c.on_loaded(Some("old".to_owned()), stale));
        assert_eq!(c.ghost_for(""), None);
    }

    #[test]
    fn clear_invalidates_in_flight_fetch() {
        let mut c = PromptSuggestionController {
            enabled: true,
            ..Default::default()
        };
        let generation = c.begin_fetch();
        c.clear();
        assert!(!c.on_loaded(Some("late".to_owned()), generation));
        assert!(!c.has_suggestion());
    }

    #[test]
    fn empty_or_multiline_suggestions_are_rejected() {
        let mut c = PromptSuggestionController {
            enabled: true,
            ..Default::default()
        };
        let generation = c.begin_fetch();
        assert!(!c.on_loaded(Some("   ".to_owned()), generation));
        let generation = c.begin_fetch();
        assert!(!c.on_loaded(Some("a\nb".to_owned()), generation));
        let generation = c.begin_fetch();
        assert!(!c.on_loaded(None, generation));
    }

    #[test]
    fn disabled_controller_shows_nothing() {
        let mut c = loaded_controller("run the tests");
        c.enabled = false;
        assert_eq!(c.ghost_for(""), None);
        assert!(!c.has_suggestion());
    }

    #[test]
    fn shown_latch_marks_once_and_rearms_on_new_load() {
        let mut c = loaded_controller("run the tests");
        assert!(!c.shown_logged());
        assert!(c.mark_shown_logged(), "first mark logs the impression");
        assert!(!c.mark_shown_logged(), "second mark is a no-op");
        assert!(c.shown_logged());

        // The next installed suggestion re-arms the latch.
        let generation = c.begin_fetch();
        assert!(c.on_loaded(Some("commit this".to_owned()), generation));
        assert!(!c.shown_logged());
        assert!(c.mark_shown_logged());
        assert!(!c.mark_shown_logged());
    }

    #[test]
    fn dismiss_and_clear_do_not_rearm_shown_latch() {
        let mut c = loaded_controller("run the tests");
        assert!(c.mark_shown_logged());
        c.dismiss();
        assert!(!c.mark_shown_logged(), "dismiss keeps the latch marked");

        let mut c = loaded_controller("run the tests");
        assert!(c.mark_shown_logged());
        c.clear();
        assert!(!c.mark_shown_logged(), "clear keeps the latch marked");
    }

    #[test]
    fn rejected_load_does_not_rearm_shown_latch() {
        let mut c = loaded_controller("run the tests");
        assert!(c.mark_shown_logged());
        // An empty payload clears the suggestion but must not re-arm the latch; there is nothing new to show
        let generation = c.begin_fetch();
        assert!(!c.on_loaded(None, generation));
        assert!(c.shown_logged());
        // A stale response for a superseded generation is discarded whole.
        let stale = c.begin_fetch();
        let _newer = c.begin_fetch();
        assert!(!c.on_loaded(Some("late".to_owned()), stale));
        assert!(c.shown_logged());
    }
}
