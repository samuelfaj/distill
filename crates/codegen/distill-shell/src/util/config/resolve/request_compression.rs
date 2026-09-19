// Modified for Distill by Samuel Fajreldines, 2026.
//! Remote-advertised request-body compression for the chat routes.

use std::sync::RwLock;

use distill_config_types::RemoteRequestEncoding;
use distill_sampler::RequestCompression;

/// Base URL of the cli-chat-proxy whose `/v1/settings` listed `zstd` in
/// `accept_request_encodings`. `None` until one does. One slot on purpose: a
/// process fetches settings from a single proxy, and if a second one ever
/// advertised, the first would fall back to plain JSON (exact-origin match),
/// never to a wrong compression.
static ZSTD_ORIGIN: RwLock<Option<String>> = RwLock::new(None);

/// Called whenever the agent applies `RemoteSettings` fetched from `origin`.
pub(crate) fn cache_remote_accept_request_encodings(
    origin: &str,
    encodings: &[RemoteRequestEncoding],
) {
    if let Ok(mut guard) = ZSTD_ORIGIN.write() {
        *guard = encodings
            .contains(&RemoteRequestEncoding::Zstd)
            .then(|| origin.to_owned());
    }
}

/// Compression the sampler may apply toward `base_url`. `GROK_REQUEST_COMPRESSION=0`
/// is the operator kill switch, re-read whenever a sampler config is built.
pub(crate) fn request_compression_for_url(base_url: &str) -> RequestCompression {
    if distill_config::env_bool("GROK_REQUEST_COMPRESSION") == Some(false) {
        return RequestCompression::None;
    }
    let origin = ZSTD_ORIGIN.read().ok().and_then(|guard| guard.clone());
    request_compression_for(base_url, origin.as_deref())
}

/// Zstd only toward the proxy that advertised it. A sibling trusted route
/// (staging, the dev proxy) or any other host (BYOK, local models) never
/// receives a body its server may not decode.
fn request_compression_for(base_url: &str, zstd_origin: Option<&str>) -> RequestCompression {
    if zstd_origin.is_some_and(|origin| crate::util::matches_trusted_base_url(base_url, origin)) {
        RequestCompression::Zstd
    } else {
        RequestCompression::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_only_toward_the_proxy_that_advertised_zstd() {
        let (plain, zstd) = (RequestCompression::None, RequestCompression::Zstd);
        let prod = crate::env::PROD_CLI_CHAT_PROXY_BASE_URL;
        let dev = "http://localhost:20016/v1";
        let pinned = "https://proxy.corp.example/v1";
        for (base_url, zstd_origin, expected) in [
            (prod, None, plain),
            (prod, Some(prod), zstd),
            (dev, Some(dev), zstd),
            (pinned, Some(pinned), zstd),
            // A second trusted route must not inherit another proxy's advertisement.
            (dev, Some(prod), plain),
            (prod, Some(pinned), plain),
            ("http://localhost:11434/v1", Some(prod), plain),
            ("http://127.0.0.1:8080/v1", Some(prod), plain),
            ("https://api.openai.com/v1", Some(prod), plain),
            ("https://api.x.ai/v1", Some(prod), plain),
        ] {
            assert_eq!(
                request_compression_for(base_url, zstd_origin),
                expected,
                "{base_url} with zstd advertised by {zstd_origin:?}"
            );
        }
    }
}
