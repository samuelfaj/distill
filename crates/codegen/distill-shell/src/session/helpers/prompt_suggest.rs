// Modified for Distill by Samuel Fajreldines, 2026.
//! Helpers for next-prompt suggestions.

use crate::config::PromptSuggestModelPin;
use crate::sampling::ConversationItem;
use crate::session::helpers::chat::floor_char_boundary;
use distill_sampling_types::ReasoningEffort;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SuggestReasoning {
    pub(crate) effort: Option<ReasoningEffort>,
    pub(crate) reserve_budget: bool,
}

/// Unset effort becomes low only on a reasoning model that is not the
/// non-reasoning alias. That alias stays off even if the catalog lists effort.
/// An explicit value is kept.
pub(crate) fn suggest_request_effort(
    configured: Option<ReasoningEffort>,
    model: &str,
    model_supports_reasoning: bool,
) -> Option<ReasoningEffort> {
    let alias = crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL;
    match configured {
        Some(effort) => Some(effort),
        None if model_supports_reasoning && model != alias => Some(ReasoningEffort::Low),
        None => None,
    }
}

pub(crate) fn resolve_suggest_reasoning(
    configured: Option<ReasoningEffort>,
    model: &str,
    supports_reasoning_effort: bool,
    supports_none: bool,
) -> SuggestReasoning {
    if let Some(effort) = configured
        && !matches!(effort, ReasoningEffort::None)
    {
        return SuggestReasoning {
            effort: supports_reasoning_effort.then_some(effort),
            reserve_budget: supports_reasoning_effort,
        };
    }

    if model != crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL && supports_reasoning_effort
    {
        return SuggestReasoning {
            effort: supports_none.then_some(ReasoningEffort::None),
            reserve_budget: !supports_none,
        };
    }

    SuggestReasoning {
        effort: None,
        reserve_budget: false,
    }
}

pub(crate) fn effective_suggest_model(
    pin: &PromptSuggestModelPin,
    client_hint: Option<&str>,
    session_model: Option<&str>,
    reasoning_is_off: bool,
    in_catalog: impl Fn(&str) -> bool,
) -> Option<String> {
    let client_hint = client_hint.map(str::trim).filter(|s| !s.is_empty());
    let session_model = session_model.map(str::trim).filter(|s| !s.is_empty());
    match pin {
        PromptSuggestModelPin::Env(model) | PromptSuggestModelPin::Pinned(model) => {
            in_catalog(model).then(|| model.to_owned())
        }
        PromptSuggestModelPin::Unpinned => match client_hint {
            Some(model) => in_catalog(model).then(|| model.to_owned()),
            None if reasoning_is_off => {
                let alias = crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL;
                in_catalog(alias).then(|| alias.to_owned()).or_else(|| {
                    session_model
                        .filter(|model| in_catalog(model))
                        .map(str::to_owned)
                })
            }
            None => session_model
                .filter(|model| in_catalog(model))
                .map(str::to_owned),
        },
    }
}

/// Total character budget for the compact transcript (~6k tokens at the bytes/4 estimate).
/// It keeps the per-turn cost of the feature trivial even on long sessions.
const TRANSCRIPT_BUDGET_CHARS: usize = 24_000;

/// Per-message character cap inside the transcript.
/// Long messages (pasted logs, big diffs) carry little signal for next-prompt prediction.
const MESSAGE_CAP_CHARS: usize = 1_500;

/// The model sees a compact transcript and must reply with ONLY the predicted next user message (or nothing).
pub(crate) const SUGGEST_PROMPT_SYSTEM: &str = "You predict the next line the USER will type into their coding agent.\n\
    You see a transcript. The last line is from the agent.\n\
    Write only that next user line, or NONE.\n\n\
    Predict what they would type, not what you think they should do.\n\
    A wrong line is worse than NONE.\n\
    Write NONE if the next line is long, new, or not obvious.\n\
    Write NONE after an error or a misunderstanding.\n\n\
    Never write a line the user already sent.\n\
    Never write filler, a question, or agent voice.\n\
    Never write a new idea they did not ask for.\n\n\
    If you write a line, use 2-12 words in their style.\n\
    Reply with only the line or NONE.";

pub(crate) fn suggestion_size(s: &str) -> (usize, usize) {
    (s.chars().count(), s.split_whitespace().count())
}

/// One transcript line: role label and flattened text content.
fn transcript_line(role: &str, text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut text = text;
    if text.len() > MESSAGE_CAP_CHARS {
        let cut = floor_char_boundary(text, MESSAGE_CAP_CHARS);
        if let Some(prefix) = text.get(..cut) {
            text = prefix;
        }
    }
    Some(format!("{role}: {text}"))
}

/// Keeps genuine `User` messages (skipping runtime-synthesized ones) and `Assistant` text, newest-last.
/// Walks backwards until the character budget is exhausted.
/// Returns `None` when the conversation has no assistant reply yet (nothing to predict from).
pub(crate) fn build_transcript(conversation: &[ConversationItem]) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut saw_assistant = false;

    for item in conversation.iter().rev() {
        let line = match item {
            ConversationItem::User(u) => {
                if !u.synthetic_reason.is_human() {
                    continue;
                }
                transcript_line("User", &item.text_content())
            }
            ConversationItem::Assistant(_) => {
                let line = transcript_line("Agent", &item.text_content());
                if line.is_some() {
                    saw_assistant = true;
                }
                line
            }
            _ => continue,
        };
        let Some(line) = line else { continue };
        if used + line.len() > TRANSCRIPT_BUDGET_CHARS && !lines.is_empty() {
            break;
        }
        used += line.len();
        lines.push(line);
    }

    if !saw_assistant || lines.is_empty() {
        return None;
    }

    lines.reverse();
    Some(lines.join("\n\n"))
}

pub(crate) fn suggest_prompt_user_message(transcript: &str, cwd: &str) -> String {
    format!(
        "CWD: {cwd}\n\nTranscript:\n\n{transcript}\n\n\
         Predict the user's next message. Reply with ONLY the suggestion text."
    )
}

/// Returns `None` when there is nothing to show. Matches the eval: empty or a
/// silence token is NONE. Other text is shown as the first line.
pub(crate) fn sanitize_suggestion(raw: &str) -> Option<String> {
    let line = raw.trim().lines().next()?.trim();
    let line = line
        .trim_start_matches(['"', '\'', '`', '“', '‘'])
        .trim_end_matches(['"', '\'', '`', '”', '’'])
        .trim();

    if line.is_empty() {
        return None;
    }

    let lowered = line.to_ascii_lowercase();
    let meta = [
        "none",
        "n/a",
        "no suggestion",
        "nothing",
        "(silence)",
        "silence",
        "null",
    ];
    if meta
        .iter()
        .any(|m| lowered == *m || lowered.starts_with(&format!("{m}.")))
    {
        return None;
    }

    Some(line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromptSuggestModelPin as Pin;

    // -- effective_suggest_model ---------------------------------------------

    #[test]
    fn effective_model_explicit_pin_beats_client_hint_when_reasoning_is_off() {
        assert_eq!(
            effective_suggest_model(
                &Pin::Pinned("pinned".into()),
                Some("hinted"),
                Some("session"),
                true,
                |m| m == "pinned"
            )
            .as_deref(),
            Some("pinned")
        );
    }

    #[test]
    fn effective_model_default_prefers_catalogued_non_reasoning_alias() {
        let alias = crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL;
        assert_eq!(
            effective_suggest_model(&Pin::Unpinned, None, Some("session"), true, |m| m == alias)
                .as_deref(),
            Some(alias)
        );
    }

    #[test]
    fn effective_model_default_falls_back_to_catalogued_session_model() {
        assert_eq!(
            effective_suggest_model(&Pin::Unpinned, None, Some("session"), true, |m| m
                == "session")
            .as_deref(),
            Some("session")
        );
        assert_eq!(
            effective_suggest_model(&Pin::Unpinned, None, Some("session"), true, |_| false),
            None
        );
    }

    #[test]
    fn effective_model_default_uses_alias_without_session_catalog_entry() {
        let alias = crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL;
        assert_eq!(
            effective_suggest_model(&Pin::Unpinned, None, Some("missing-session"), true, |m| {
                m == alias
            })
            .as_deref(),
            Some(alias)
        );
    }

    #[test]
    fn configured_reasoning_reserves_budget() {
        assert_eq!(
            resolve_suggest_reasoning(Some(ReasoningEffort::Low), "session", true, true),
            SuggestReasoning {
                effort: Some(ReasoningEffort::Low),
                reserve_budget: true,
            }
        );
    }

    #[test]
    fn reasoning_off_uses_none_when_the_fallback_supports_it() {
        assert_eq!(
            resolve_suggest_reasoning(None, "session", true, true),
            SuggestReasoning {
                effort: Some(ReasoningEffort::None),
                reserve_budget: false,
            }
        );
    }

    #[test]
    fn reasoning_off_reserves_budget_when_the_fallback_has_no_none_effort() {
        assert_eq!(
            resolve_suggest_reasoning(None, "session", true, false),
            SuggestReasoning {
                effort: None,
                reserve_budget: true,
            }
        );
    }

    #[test]
    fn alias_keeps_the_small_non_reasoning_budget() {
        assert_eq!(
            resolve_suggest_reasoning(
                None,
                crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL,
                true,
                false,
            ),
            SuggestReasoning {
                effort: None,
                reserve_budget: false,
            }
        );
    }

    #[test]
    fn effective_model_client_hint_beats_session_and_is_guarded() {
        assert_eq!(
            effective_suggest_model(
                &Pin::Unpinned,
                Some("hinted"),
                Some("session"),
                false,
                |m| m == "hinted"
            )
            .as_deref(),
            Some("hinted")
        );
    }

    #[test]
    fn effective_model_env_pin_is_catalog_guarded() {
        assert_eq!(
            effective_suggest_model(
                &Pin::Env("custom-model".into()),
                Some("hinted"),
                Some("session"),
                true,
                |_| false
            ),
            None
        );
    }

    // -- sanitize_suggestion ------------------------------------------------

    #[test]
    fn sanitize_accepts_short_imperative() {
        assert_eq!(
            sanitize_suggestion("run the tests").as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn sanitize_strips_quotes_and_backticks() {
        assert_eq!(
            sanitize_suggestion("\"commit this\"").as_deref(),
            Some("commit this")
        );
        assert_eq!(sanitize_suggestion("`push it`").as_deref(), Some("push it"));
    }

    #[test]
    fn sanitize_takes_first_line_only() {
        assert_eq!(
            sanitize_suggestion("run the tests\nthen commit").as_deref(),
            Some("run the tests")
        );
    }

    #[test]
    fn sanitize_rejects_none_and_meta() {
        for s in ["NONE", "none", "n/a", "no suggestion", "(silence)", ""] {
            assert_eq!(sanitize_suggestion(s), None, "should reject {s:?}");
        }
    }

    #[test]
    fn unset_effort_is_low_only_on_a_reasoning_model() {
        assert_eq!(
            suggest_request_effort(None, "grok-4.6", true),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(suggest_request_effort(None, "grok-4.6", false), None);
        assert_eq!(
            suggest_request_effort(
                None,
                crate::util::config::NON_REASONING_PROMPT_SUGGEST_MODEL,
                true
            ),
            None
        );
        assert_eq!(
            suggest_request_effort(Some(ReasoningEffort::High), "grok-4.6", true),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            suggest_request_effort(Some(ReasoningEffort::None), "grok-4.6", true),
            Some(ReasoningEffort::None)
        );
    }

    // -- build_transcript ---------------------------------------------------

    fn user(text: &str) -> ConversationItem {
        ConversationItem::user(text.to_owned())
    }

    fn assistant(text: &str) -> ConversationItem {
        ConversationItem::assistant(text.to_owned())
    }

    #[test]
    fn transcript_keeps_user_and_assistant_in_order() {
        let conv = vec![
            ConversationItem::system("sys".to_owned()),
            user("fix the bug"),
            assistant("Fixed it in foo.rs"),
        ];
        let t = build_transcript(&conv).unwrap();
        assert_eq!(t, "User: fix the bug\n\nAgent: Fixed it in foo.rs");
    }

    #[test]
    fn transcript_requires_an_assistant_reply() {
        let conv = vec![ConversationItem::system("sys".to_owned()), user("hello")];
        assert!(build_transcript(&conv).is_none());
        assert!(build_transcript(&[]).is_none());
    }

    #[test]
    fn transcript_skips_synthetic_user_messages() {
        let mut synthetic = ConversationItem::user("synthetic reminder".to_owned());
        if let ConversationItem::User(u) = &mut synthetic {
            u.synthetic_reason = crate::sampling::SyntheticReason::SystemReminder;
        }
        let conv = vec![user("real question"), synthetic, assistant("answer")];
        let t = build_transcript(&conv).unwrap();
        assert!(!t.contains("synthetic reminder"));
        assert!(t.contains("User: real question"));
    }

    #[test]
    fn transcript_caps_long_messages() {
        let long = "a".repeat(10_000);
        let conv = vec![user(&long), assistant("ok")];
        let t = build_transcript(&conv).unwrap();
        assert!(
            t.len() < 2_000,
            "long message must be truncated: {}",
            t.len()
        );
    }

    #[test]
    fn transcript_budget_keeps_newest_messages() {
        let filler = "b".repeat(MESSAGE_CAP_CHARS);
        let mut conv = Vec::new();
        for _ in 0..40 {
            conv.push(user(&filler));
            conv.push(assistant(&filler));
        }
        conv.push(user("newest question"));
        conv.push(assistant("newest answer"));
        let t = build_transcript(&conv).unwrap();
        assert!(t.len() <= TRANSCRIPT_BUDGET_CHARS + MESSAGE_CAP_CHARS + 64);
        assert!(t.contains("newest question"));
        assert!(t.ends_with("Agent: newest answer"));
    }
}
