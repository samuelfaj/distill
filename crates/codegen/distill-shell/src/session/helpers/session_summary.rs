// Modified for Distill by Samuel Fajreldines, 2026.
//! Session title generation via LLM tool call.

use crate::sampling::{
    Client as OaiCompatClient, ConversationItem, ConversationRequest, ConversationToolChoice,
    ToolSpec,
};
use crate::session::helpers::chat::floor_char_boundary;
use distill_sampling_types::ConversationResponse;

/// Upper bound on the user text that feeds title generation.
/// Titles only need the opening, and this keeps the request well under the model prompt limit.
const TITLE_SOURCE_MAX_BYTES: usize = 8_000;

/// Real-user turn counts at which the auto title is refreshed from the whole conversation, then frozen.
/// Refreshing at a couple of early turns lets the title catch up to the real topic without churning enough to make sessions hard to recognize.
/// A manual `/rename` always wins and stops refreshes.
pub(crate) const TITLE_REFRESH_TURNS: [usize; 2] = [3, 6];

/// Number of [`TITLE_REFRESH_TURNS`] checkpoints reached at `turns` real-user turns, i.e. the checkpoint index to advance to.
/// This catches up past any checkpoints a burst of turns jumped over.
/// Equal to `TITLE_REFRESH_TURNS.len()` means the title is frozen.
pub(crate) fn checkpoints_reached(turns: usize) -> usize {
    TITLE_REFRESH_TURNS.iter().filter(|&&t| turns >= t).count()
}

/// Hard byte cap guarding runaway title output; the instruction already targets 5-10 words.
/// Applied on a char boundary, so a multibyte title is capped a little shorter, which is fine for a safety bound.
const TITLE_MAX_BYTES: usize = 80;

/// Durable title-refresh checkpoint watermark under `{session_dir}/`: the number of [`TITLE_REFRESH_TURNS`] checkpoints already consumed.
/// Only a committed value is persisted, so an aborted refresh still retries.
pub(crate) const TITLE_REFRESH_WATERMARK_FILE: &str = "title_refresh_idx";

/// Load the persisted checkpoint index, clamped to the number of checkpoints so a stale larger value still means "frozen".
/// `None` when the session has no watermark yet (fresh, pre-feature, or feature-was-off).
/// The caller decides the starting checkpoint for that case (see [`initial_title_refresh_idx`]).
pub(crate) fn load_title_refresh_watermark(session_dir: &std::path::Path) -> Option<usize> {
    std::fs::read_to_string(session_dir.join(TITLE_REFRESH_WATERMARK_FILE))
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .map(|idx| idx.min(TITLE_REFRESH_TURNS.len()))
}

/// The checkpoint index a session starts at on spawn.
/// A managed session (has a watermark) uses it; the watermark is authoritative and durable across compaction.
/// An unmanaged session is *adopted* as open (`0`) only when the feature is enabled and it is brand new (no turns); otherwise it freezes.
pub(crate) fn initial_title_refresh_idx(
    watermark: Option<usize>,
    enabled: bool,
    turns: usize,
) -> usize {
    match watermark {
        Some(idx) => idx,
        None if enabled && turns == 0 => 0,
        None => TITLE_REFRESH_TURNS.len(),
    }
}

/// Persist the checkpoint index after a completed attempt.
/// It is best-effort and written atomically (temp sibling, then rename).
/// A crash mid-write therefore can't leave a partial/empty file that would load as `0` and reopen the refresh window.
pub(crate) fn save_title_refresh_watermark(session_dir: &std::path::Path, idx: usize) {
    if !session_dir.is_dir() {
        return;
    }
    let path = session_dir.join(TITLE_REFRESH_WATERMARK_FILE);
    if let Err(e) = crate::session::storage::write_bytes_atomic(&path, idx.to_string().as_bytes()) {
        tracing::warn!(error = %e, path = %path.display(), "failed to persist title refresh watermark");
    }
}

#[derive(serde::Deserialize)]
struct SessionTitle {
    session_title: String,
}

pub(crate) struct InitialTitleGeneration {
    pub(crate) title: String,
    pub(crate) status: distill_chat_state::UsageCallStatus,
    pub(crate) response: Option<ConversationResponse>,
}

/// Remove `<system-reminder>…</system-reminder>` blocks from `text`.
/// They are system-injected context (e.g. the `/goal` setup reminder), not the user's words, so they must not drive the session title.
fn strip_system_reminder_blocks(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(before) = rest.get(..start) else {
            break;
        };
        out.push_str(before);
        let Some(after_open) = rest.get(start + OPEN.len()..) else {
            return out.trim().to_string();
        };
        // An unterminated reminder drops the remainder; it is system text
        let Some(end) = after_open.find(CLOSE) else {
            return out.trim().to_string();
        };
        let Some(next) = after_open.get(end + CLOSE.len()..) else {
            return out.trim().to_string();
        };
        rest = next;
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Text the session title is derived from: strip system reminders and skill XML markup, then cap to the first few KB.
/// Stripping runs before the cap so a leading reminder larger than the cap is still removed.
fn title_source_text(user_message: &str) -> String {
    let without_reminders = strip_system_reminder_blocks(user_message);
    let base = if without_reminders.is_empty() {
        user_message
    } else {
        &without_reminders
    };
    let mut display =
        distill_tools::implementations::skills::skill::extract_skill_display_text(base)
            .unwrap_or_else(|| base.to_string());
    display.truncate(floor_char_boundary(&display, TITLE_SOURCE_MAX_BYTES));
    display
}

pub(crate) fn title_fallback_from_user_text(user_message: &str) -> String {
    let text = title_source_text(user_message);
    let s = text
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ");
    if s.is_empty() {
        "New session".to_string()
    } else {
        s
    }
}

/// Generate the initial session title from the first user message, for the fast first-prompt path ([`crate::session::summary::SummaryGenerator`]).
/// The title is later refreshed from the whole conversation at the early checkpoints in [`TITLE_REFRESH_TURNS`], then frozen.
pub(crate) async fn generate_session_summary(
    user_message: String,
    client: OaiCompatClient,
    model: &str,
) -> InitialTitleGeneration {
    let clean_message = title_source_text(&user_message);
    let request = ConversationRequest::from_items(vec![
        ConversationItem::system(
            r#"You are tasked with generating the session title. The user is asking almost always software engineering related questions on their codebase.
We describe the session title below
# Session Title
A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler.

You will be given the user query below encapsulated in <user_query></user_query>.

Just generate the session_title and nothing else"#,
        ),
        ConversationItem::user(format!(
            r#"<user_query>
{}
</user_query>"#,
            clean_message
        )),
    ])
    .with_model(model)
    .with_tools(vec![ToolSpec {
        name: "session_title".to_owned(),
        description: Some("Generate the session_title which we use for the user_message".to_owned()),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["session_title"],
            "properties": {
                "session_title": {
                    "type": "string",
                    "description": "Final session title, just 5-10 word descriptive title for the session. Super info dense, no filler."
                }
            },
            "additionalProperties": false
        }),
    }])
    .with_max_output_tokens(100)
    .with_temperature(1.0)
    .with_tool_choice(ConversationToolChoice::Function("session_title".to_owned()));

    let (response_result, rejected_response) = client
        .conversation_collect_with_idle_timeout_and_rejection(
            request,
            std::time::Duration::from_secs(300),
        )
        .await;

    match response_result {
        Ok(response) => {
            if let Some(a) = response.assistant()
                && let Some(tool_call) = a.tool_calls.first()
                && let Ok(result) = serde_json::from_str::<SessionTitle>(&tool_call.arguments)
            {
                if !result.session_title.trim().is_empty() {
                    return InitialTitleGeneration {
                        title: result.session_title,
                        status: distill_chat_state::UsageCallStatus::Completed,
                        response: Some(response),
                    };
                }
                tracing::debug!(
                    model = %model,
                    "session title generation returned an empty session_title"
                );
            }
            tracing::debug!(
                model = %model,
                "session title generation: response did not contain a session_title tool call"
            );
            InitialTitleGeneration {
                title: title_fallback_from_user_text(&clean_message),
                status: distill_chat_state::UsageCallStatus::Rejected,
                response: Some(response),
            }
        }
        Err(e) => {
            tracing::warn!(
                model = %model,
                error = %e,
                "session title generation failed, falling back to truncated user text"
            );
            if let Some(response) = rejected_response {
                return InitialTitleGeneration {
                    title: title_fallback_from_user_text(&clean_message),
                    status: distill_chat_state::UsageCallStatus::Rejected,
                    response: Some(response),
                };
            }
            InitialTitleGeneration {
                title: title_fallback_from_user_text(&clean_message),
                status: distill_chat_state::UsageCallStatus::Failed,
                response: None,
            }
        }
    }
}

/// Instruction turn appended to a conversation snapshot to refresh the auto title.
/// Like the recap / turn-summary side-calls, all directions live in one reminder-wrapped turn.
/// The model sees the whole conversation, so the title reflects the real topic rather than a possibly-useless first prompt.
pub(crate) fn title_refresh_instruction(tag: &str) -> String {
    format!(
        "<{tag}>Generate a session title for the conversation above. It should be a short and \
         distinctive 5-10 word descriptive title capturing what this session is actually about \
         (the main task or topic), based on the WHOLE conversation — not just the first message. \
         Super info dense, no filler. User-role messages wrapped in reminder tags like this one \
         are injected context, not the user.\n\n\
         Output ONLY the title: plain text, no quotes, no labels, no markdown. Do NOT call any \
         tools — respond with plain text only.</{tag}>"
    )
}

/// Clean a refreshed title into a one-line string.
/// Applies the recap normalization (whitespace collapse, stray label/quote stripping) plus the [`TITLE_MAX_BYTES`] cap.
pub(crate) fn clean_title_text(raw: &str) -> String {
    let mut out = crate::session::helpers::session_recap::clean_recap_text(raw);
    if out.len() > TITLE_MAX_BYTES {
        let cut = floor_char_boundary(&out, TITLE_MAX_BYTES);
        out.truncate(cut);
        out = out.trim_end().to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        TITLE_SOURCE_MAX_BYTES, clean_title_text, strip_system_reminder_blocks,
        title_fallback_from_user_text, title_refresh_instruction, title_source_text,
    };

    #[test]
    fn checkpoints_reached_counts_and_catches_up() {
        use super::{TITLE_REFRESH_TURNS, checkpoints_reached};
        assert_eq!(checkpoints_reached(0), 0);
        assert_eq!(checkpoints_reached(2), 0);
        assert_eq!(checkpoints_reached(3), 1);
        assert_eq!(checkpoints_reached(5), 1);
        assert_eq!(checkpoints_reached(6), 2);
        // A burst past the last checkpoint catches up to frozen, no overshoot.
        assert_eq!(checkpoints_reached(50), TITLE_REFRESH_TURNS.len());
    }

    /// The freeze watermark round-trips (durable across a shortened conversation, e.g. compaction).
    #[test]
    fn title_refresh_watermark_round_trips_and_clamps() {
        use super::{
            TITLE_REFRESH_TURNS, TITLE_REFRESH_WATERMARK_FILE, load_title_refresh_watermark,
            save_title_refresh_watermark,
        };
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            load_title_refresh_watermark(dir.path()),
            None,
            "missing → None"
        );
        save_title_refresh_watermark(dir.path(), 1);
        assert_eq!(load_title_refresh_watermark(dir.path()), Some(1));
        std::fs::write(dir.path().join(TITLE_REFRESH_WATERMARK_FILE), "99").unwrap();
        assert_eq!(
            load_title_refresh_watermark(dir.path()),
            Some(TITLE_REFRESH_TURNS.len()),
            "stale-large watermark reads as frozen"
        );
    }

    #[test]
    fn initial_title_refresh_idx_adopts_only_fresh_enabled_sessions() {
        use super::{TITLE_REFRESH_TURNS, initial_title_refresh_idx};
        let frozen = TITLE_REFRESH_TURNS.len();
        // Managed: watermark is authoritative regardless of enabled/turns.
        assert_eq!(initial_title_refresh_idx(Some(1), true, 9), 1);
        assert_eq!(initial_title_refresh_idx(Some(frozen), true, 0), frozen);
        // Unmanaged, enabled, and brand new: adopt open
        assert_eq!(initial_title_refresh_idx(None, true, 0), 0);
        // Unmanaged but already has turns (pre-feature, even if compacted): frozen
        assert_eq!(initial_title_refresh_idx(None, true, 5), frozen);
        // Unmanaged with the feature off: frozen even when brand new
        assert_eq!(initial_title_refresh_idx(None, false, 0), frozen);
    }

    #[test]
    fn title_refresh_instruction_wraps_tag_and_asks_for_whole_conversation() {
        let text = title_refresh_instruction("system-reminder");
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
    }

    #[test]
    fn clean_title_normalizes_and_caps() {
        // Collapses whitespace and strips surrounding quotes.
        assert_eq!(
            clean_title_text("\"Fix the auth  bug\""),
            "Fix the auth bug"
        );
        let capped = clean_title_text(&"word ".repeat(50));
        assert!(capped.len() <= super::TITLE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_caps_oversized_input() {
        let big = "word ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_cap_is_utf8_safe() {
        // 3-byte chars straddle the byte cap; must truncate on a boundary, not panic.
        let big = "あ".repeat(10_000);
        let out = title_source_text(&big);
        assert!(!out.is_empty() && out.len() <= TITLE_SOURCE_MAX_BYTES);
    }

    #[test]
    fn title_source_text_strips_leading_reminder_larger_than_cap() {
        // A leading reminder bigger than the cap must still be stripped, so the title derives from the objective rather than reminder text
        let reminder = "x".repeat(TITLE_SOURCE_MAX_BYTES * 2);
        let input =
            format!("<system-reminder>\n{reminder}\n</system-reminder>\n\nbuild a mario game");
        let out = title_source_text(&input);
        assert_eq!(out, "build a mario game");
    }

    #[test]
    fn strip_removes_goal_setup_reminder_leaving_objective() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nlots of rules\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game";
        assert_eq!(
            strip_system_reminder_blocks(input),
            "build a mario platformer game"
        );
    }

    #[test]
    fn strip_handles_unterminated_reminder() {
        assert_eq!(
            strip_system_reminder_blocks("<system-reminder>\nrules with no close tag"),
            ""
        );
    }

    #[test]
    fn strip_no_reminder_is_identity() {
        assert_eq!(
            strip_system_reminder_blocks("fix the auth bug"),
            "fix the auth bug"
        );
    }

    /// Regression: a `/goal <objective>` first turn must title off the objective, not the injected `<system-reminder>` setup block.
    #[test]
    fn fallback_titles_off_goal_objective_not_reminder() {
        let input = "<system-reminder>\nA goal has been set: do stuff\nStart \
                     now.\n</system-reminder>\n\nbuild a mario platformer game in html";
        assert_eq!(
            title_fallback_from_user_text(input),
            "build a mario platformer game in html"
        );
    }

    #[test]
    fn fallback_trims_to_words() {
        assert_eq!(
            title_fallback_from_user_text(
                "one two three four five six seven eight nine ten eleven"
            ),
            "one two three four five six seven eight nine ten"
        );
    }

    #[test]
    fn fallback_new_session_when_whitespace_only() {
        assert_eq!(title_fallback_from_user_text("   \n\t"), "New session");
    }

    #[test]
    fn fallback_strips_skill_xml_with_args() {
        let input = "<command-name>implement</command-name>\n\
                      <command-message>/implement</command-message>\n\
                      <command-args>fix the rendering bug</command-args>";
        assert_eq!(
            title_fallback_from_user_text(input),
            "/implement fix the rendering bug",
        );
    }

    #[test]
    fn fallback_strips_skill_xml_no_args() {
        let input = "<command-name>deploy</command-name>\n\
                      <command-message>/deploy</command-message>";
        assert_eq!(title_fallback_from_user_text(input), "/deploy");
    }

    #[test]
    fn fallback_plain_text_unaffected() {
        assert_eq!(
            title_fallback_from_user_text("fix the auth bug in login.rs"),
            "fix the auth bug in login.rs",
        );
    }
}
