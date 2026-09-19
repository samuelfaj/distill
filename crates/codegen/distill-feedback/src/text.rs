//! Text shaping shared by every feedback producer: the POST body and a title derived from free text.

pub(crate) const FALLBACK_TITLE: &str = "Feedback draft";
const MAX_TITLE_CHARS: usize = 80;

/// `"{title}\n\n{details}"` with each part trimmed.
#[must_use]
pub fn post_text(title: &str, details: &str) -> String {
    format!("{}\n\n{}", title.trim(), details.trim())
}

/// First non-blank line of `text`, trimmed and cut to 80 chars; `"Feedback draft"` when there is none.
#[must_use]
pub fn derive_title(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or(FALLBACK_TITLE, title_prefix)
        .to_owned()
}

pub(crate) fn title_prefix(line: &str) -> &str {
    let end = line
        .char_indices()
        .nth(MAX_TITLE_CHARS)
        .map_or(line.len(), |(index, _)| index);
    line.get(..end).unwrap_or("")
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
