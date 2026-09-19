//! Decode `x.ai/leader/version_mismatch` into toast/log text.
//!
//! Versions are sanitized before the whitespace check, so a value made only of control characters is rejected instead of toasted.
//! The full banner then goes through `sanitize_toast_message` too, so the `⚠` chrome glyph gets its ConHost fallback.
//! Sanitizing only the interpolated versions would leave that glyph raw.
//! The wire `message` field is ignored; the client writes its own restart hint.

use std::borrow::Cow;

use crate::glyphs::sanitize_toast_message;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionMismatchParams<'a> {
    #[serde(borrow)]
    client_version: Option<Cow<'a, str>>,
    #[serde(borrow)]
    leader_version: Option<Cow<'a, str>>,
}

/// ASCII marker that survives `sanitize_toast_message` glyph fallback (legacy ConHost turns `⚠` into `!`).
const VERSION_MISMATCH_MARKER: &str = "Version mismatch:";

pub(crate) fn version_mismatch_banner(params: &str) -> Option<String> {
    let parsed: VersionMismatchParams<'_> = serde_json::from_str(params).ok()?;
    let client = sanitize_toast_message(parsed.client_version.as_deref()?);
    let leader = sanitize_toast_message(parsed.leader_version.as_deref()?);
    if client.chars().all(char::is_whitespace) || leader.chars().all(char::is_whitespace) {
        return None;
    }
    Some(
        sanitize_toast_message(&format!(
            "⚠ {VERSION_MISMATCH_MARKER} client {client}, leader {leader}. Restart grok to match"
        ))
        .into_owned(),
    )
}

pub(crate) fn is_version_mismatch_banner(msg: &str) -> bool {
    msg.contains(VERSION_MISMATCH_MARKER)
}

#[cfg(test)]
#[path = "version_mismatch_tests.rs"]
mod tests;
