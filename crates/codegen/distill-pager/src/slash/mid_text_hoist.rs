//! Rewrites a plain submission carrying an opted-in `/command` token into a leading
//! `/command <message-without-token>` line. The caller decides what counts as a plain submission
//! (raw text not starting with `/`); `text` is chip-stripped, so a `/` at index 0 here is an
//! ordinary token after a leading image chip (`[Image #1] /btw q` arrives as ` /btw q`).

use crate::slash::command::SlashCommand;
use crate::slash::registry::CommandRegistry;
use crate::slash::{InlineSlashToken, scan_inline_slash_tokens};

pub(crate) const MID_TEXT_GOAL_NOTICE: &str =
    "Slash commands only work at the start of the message — put `/goal …` first.";

/// Skills and plugin skills are mentions; `/btw` hoists. Other builtins do not run mid-text.
#[must_use]
pub(crate) fn command_works_mid_text(command: &dyn SlashCommand) -> bool {
    command.can_hoist_from_mid_text() || command.is_skill()
}

/// Leading `/` always dispatches. Mid-text only skills, plugin skills, and hoist commands.
#[must_use]
pub(crate) fn token_is_armed_inline(
    text: &str,
    token: &InlineSlashToken,
    command: &dyn SlashCommand,
) -> bool {
    let leading = text
        .get(..token.range.start)
        .is_some_and(|before| before.trim().is_empty());
    leading || command_works_mid_text(command)
}

#[must_use]
pub(crate) fn contains_goal_command_token(text: &str, registry: &CommandRegistry) -> bool {
    registry.get_for_dispatch("goal").is_some()
        && scan_inline_slash_tokens(text, 0)
            .iter()
            .any(|token| token.name == "goal")
}

/// `None` means "not a mid-text invocation"; the caller dispatches `text` unchanged.
#[must_use]
pub(crate) fn hoist_mid_text_command(text: &str, registry: &CommandRegistry) -> Option<String> {
    let (token, command) = scan_inline_slash_tokens(text, 0)
        .into_iter()
        .find_map(|token| {
            let command = registry.get_for_dispatch(&token.name)?;
            command
                .can_hoist_from_mid_text()
                .then_some((token, command))
        })?;
    // Scanner ranges index this same text; `get` keeps the submit path panic-free regardless.
    let before = text.get(..token.range.start)?;
    // Only the whitespace run after the token goes with it, so a newline before it survives.
    let after = text.get(token.range.end..)?.trim_start();
    let question = format!("{before}{after}");
    let question = question.trim();
    if question.is_empty() {
        return None;
    }
    Some(format!("/{} {question}", command.name()))
}

#[cfg(test)]
#[path = "mid_text_hoist_tests.rs"]
mod tests;
