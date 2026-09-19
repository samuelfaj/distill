// Modified for Distill by Samuel Fajreldines, 2026.
use std::borrow::Cow;

pub fn redact_owned(input: &str) -> Option<String> {
    let secrets = distill_secrets::redact_secrets(input);
    let after = match distill_secrets::redact_user_paths(secrets.as_ref()) {
        Cow::Owned(paths) => paths,
        Cow::Borrowed(_) => match secrets {
            Cow::Owned(s) => s,
            Cow::Borrowed(_) => return None,
        },
    };

    if after == input { None } else { Some(after) }
}

pub fn redact_to_owned(input: &str) -> String {
    redact_owned(input).unwrap_or_else(|| input.to_owned())
}

pub fn url_origin(value: &str) -> Cow<'_, str> {
    if let Ok(url) = url::Url::parse(value)
        && let Some(host) = url.host_str()
    {
        let origin = match url.port() {
            Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
            None => format!("{}://{}", url.scheme(), host),
        };
        return Cow::Owned(origin);
    }
    Cow::Borrowed(value)
}

pub fn redact_urls_in_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        let https = rest.find("https://");
        let http = rest.find("http://");
        let start = match (https, http) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        let Some(start) = start else {
            out.push_str(rest);
            break;
        };
        let Some(prefix) = rest.get(..start) else {
            break;
        };
        out.push_str(prefix);
        let Some(url_rest) = rest.get(start..) else {
            break;
        };
        let end = url_rest
            .char_indices()
            .find(|&(_, c)| {
                c.is_whitespace() || matches!(c, ')' | ']' | '"' | '\'' | ',' | ';' | '>')
            })
            .map(|(i, _)| i)
            .unwrap_or(url_rest.len());
        let Some(url) = url_rest.get(..end) else {
            break;
        };
        out.push_str(url_origin(url).as_ref());
        rest = url_rest.get(end..).unwrap_or("");
    }
    redact_to_owned(&out)
}

const ERROR_DETAIL_MAX_LEN: usize = 256;

/// Reduce URLs and scrub secrets and user paths, then cap; redaction precedes the cap so truncation cannot split a secret.
pub fn redact_error_detail(input: &str) -> String {
    redact_urls_in_text(input)
        .chars()
        .take(ERROR_DETAIL_MAX_LEN)
        .collect()
}

/// Egress scrub for the product-events funnel: strip secrets and user paths from every metadata string.
pub fn redact_metadata(metadata: &mut serde_json::Map<String, serde_json::Value>) {
    for value in metadata.values_mut() {
        distill_secrets::walk_json_strings(value, &mut |s| {
            if let Some(scrubbed) = redact_owned(s) {
                *s = scrubbed;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_owned_scrubs_secret_shapes() {
        let out = redact_owned("key sk-CANARYabcdefghij1234567890 end")
            .expect("secret must trigger a rewrite");
        assert!(!out.contains("CANARY"));
    }

    #[test]
    fn redact_owned_returns_none_when_clean() {
        assert_eq!(redact_owned("no secrets here"), None);
    }

    #[test]
    fn redact_owned_is_idempotent_after_url_scrub() {
        let once = redact_to_owned("see https://example.com/docs?token=CANARY");
        assert!(once.contains("https://example.com"), "origin lost: {once}");
        assert!(!once.contains("CANARY"), "query token survived: {once}");
        assert_eq!(
            redact_owned(&once),
            None,
            "already-scrubbed URL text must not look dirty to the export validator"
        );
    }

    #[test]
    fn url_origin_drops_path_and_query() {
        let origin = url_origin("https://collector.corp.example:4318/v1/logs?token=CANARY");
        assert_eq!(origin, "https://collector.corp.example:4318");
    }

    #[test]
    fn url_origin_passes_unparseable_through() {
        assert_eq!(url_origin("not a url"), "not a url");
    }

    #[test]
    fn redact_urls_in_text_reduces_embedded_urls() {
        let err = "error sending request for url (https://collector.corp.example:4318/v1/logs?token=CANARY): connection reset";
        let out = redact_urls_in_text(err);
        assert!(out.contains("https://collector.corp.example:4318"));
        assert!(!out.contains("/v1/logs"));
        assert!(!out.contains("CANARY"));
    }
}
