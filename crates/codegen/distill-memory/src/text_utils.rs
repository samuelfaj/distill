//! Text-classification helpers shared by the [`crate::flush`] and [`crate::dream`] response-processing modules.
//!
//! Keeping them here lets flush and dream depend on `text_utils` without depending on each other.

/// Flush and dream response processing call this to check the model produced structured output.
pub fn has_markdown_headers(text: &str) -> bool {
    text.contains("## ") || text.contains("# ")
}

/// True when the response is the NO_REPLY marker in any case or separator variant: `"no reply"`, `"no_reply"`, `"No-Reply"`, `"NO REPLY"`.
///
/// Strips all non-alphanumeric characters, lowercases, and checks if the remainder is exactly `"noreply"`.
pub fn is_no_reply(text: &str) -> bool {
    let normalized: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    normalized == "noreply"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_no_reply() {
        assert!(is_no_reply("NO_REPLY"));
        assert!(is_no_reply("no reply"));
        assert!(is_no_reply("No-Reply"));
        assert!(is_no_reply("noreply"));
        assert!(!is_no_reply("no reply needed"));
        assert!(!is_no_reply("I have things to store"));
    }

    #[test]
    fn test_has_markdown_headers() {
        assert!(has_markdown_headers("## Topic"));
        assert!(has_markdown_headers("# Title\n\nBody"));
        assert!(has_markdown_headers("preamble\n\n## Topic"));
        assert!(!has_markdown_headers("plain text without headers"));
        assert!(!has_markdown_headers("#hashtag without space"));
    }
}
