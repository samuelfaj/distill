//! Shared size-overflow detection. Each harness owns its own
//! deterministic-vs-transient taxonomy; [`is_context_length_error`] is the
//! single size definition shared by the turn path, the harness compaction
//! classifiers, and the shared retry loop.

/// True when an error message indicates a context-window overflow. Backends report
/// this inconsistently with no stable error code, so we match the message text; it's
/// deterministic (re-sending the same payload always fails), so callers must not retry.
pub fn is_context_length_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    // Token-overflow prose with no stable code.
    m.contains("too long for this model")
        || m.contains("prompt is too long")
        || m.contains("maximum prompt length")
        || m.contains("maximum context length")
        // Provider request-body byte caps.
        || m.contains("maximum allowed number of bytes")
        || (m.contains("current message") && m.contains("exceeds budget"))
        || is_anchored(&m, "request too large")
        || has_size_slug(&m)
        || has_rendered_413_phrase(&m)
        || has_input_length_pair(&m)
}

/// `needle` at message start or right after a ": " separator — where
/// renderers put codes and reason phrases — so request content echoing it
/// mid-prose doesn't match.
fn is_anchored(m: &str, needle: &str) -> bool {
    m.split(": ").any(|segment| segment.starts_with(needle))
}

/// Provider size-error slugs, anchored like rendered codes.
fn has_size_slug(m: &str) -> bool {
    [
        "context_length_exceeded",
        "exceed_context_size_error",
        "payload_too_large",
        "request_too_large",
    ]
    .iter()
    .any(|slug| is_anchored(m, slug))
}

/// A 413 reason phrase directly after its status code — how HTTP statuses
/// render ("413 Payload Too Large") — in the legacy, RFC 9110, and RFC-2616
/// spellings. Adjacency keeps echoed prose and stray digit runs from
/// matching.
fn has_rendered_413_phrase(m: &str) -> bool {
    m.contains("413 payload too large")
        || m.contains("413 content too large")
        || m.contains("413 request entity too large")
}

/// "Input length (N tokens) exceeds the maximum allowed length (M tokens)";
/// paired so field-length validation errors ("metadata value exceeds the
/// maximum allowed length") don't classify as size.
fn has_input_length_pair(m: &str) -> bool {
    m.contains("input length") && m.contains("exceeds the maximum allowed length")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Canonical wording table for the shared detector — add new wordings
    /// here; harness-side tests pin only their local couplings.
    #[test]
    fn context_length_error_matches_known_messages() {
        for msg in [
            "The text is too long for this model.",
            "The prompt is too long for this model's context window.",
            "prompt is too long: 250000 tokens > 200000 maximum",
            "exceeds the maximum prompt length",
            "This model's maximum context length is 128000 tokens",
            "error code: context_length_exceeded",
            "Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "compact failed: API error (status 400 Bad Request): invalid-argument: Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "Current message (600000) exceeds budget (500000)",
            // Fleet-observed size-overflow wordings.
            "API error (status 413 Payload Too Large): Request failed (HTTP 413).",
            "API error (status 413 Payload Too Large): payload_too_large: Chat history exceeds the 800-message limit",
            "API error (status 400 Bad Request): exceed_context_size_error: request (300000 tokens) exceeds the model context size",
            "stream error (BAD_REQUEST): Input length (300000 tokens) exceeds the maximum allowed length (200000 tokens)",
            // RFC 9110 and RFC-2616 spellings of 413's reason phrase.
            "API error (status 413 Content Too Large): Request failed (HTTP 413).",
            "upstream returned 413 Request Entity Too Large",
            "request exceeds the maximum allowed number of bytes (10485760)",
            "Request too large",
            "compact failed: 413: Request too large",
            "API error (status 429 Too Many Requests): Request too large for model",
            "request_too_large: request rejected",
        ] {
            assert!(is_context_length_error(msg), "should match: {msg}");
        }
        for msg in [
            "internal server error",
            "rate limited",
            "connection reset by peer",
            "Attached file content (300000 tokens) causes message to exceed budget",
            "compact index estimate 2.0 GB exceeds budget 1.0 GB",
            "API error (status 400 Bad Request): invalid tool schema",
            // Field-length validation, not a context overflow.
            "metadata value exceeds the maximum allowed length (512 characters)",
            // Echoed prose: mid-sentence, no start/": " anchor.
            "invalid_request_error: field description says request too large sometimes",
            // Echoed 413 reason phrases without a "413" in the message.
            "invalid_request_error: user note says the payload too large banner is confusing",
            "invalid_request_error: field description mentions a content too large warning",
            "invalid_request_error: docs mention a request entity too large response",
            // Slugs echoed mid-prose (not at start / after ": ").
            "invalid_request_error: user asked what context_length_exceeded means",
            "invalid_request_error: docs mention the payload_too_large code",
            "invalid_request_error: docs mention the request_too_large code",
            // A stray digit run next to an echoed phrase, but not the
            // rendered "413 <reason phrase>" adjacency.
            "invalid_request_error: line 413 of the doc mentions payload too large limits",
        ] {
            assert!(!is_context_length_error(msg), "should not match: {msg}");
        }
    }
}
