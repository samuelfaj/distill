// Modified for Distill by Samuel Fajreldines, 2026.
//! Probes the first-party env key (`GET {xai_api_base_url}/api-key`) before `initialize` advertises `xai.api_key`.
//! BYOK keys are never probed.
//!
//! The base URL is the caller's effective `endpoints.xai_api_base_url`, so the probe hits the same host turn traffic uses.
//! That value comes from `GROK_XAI_API_BASE_URL` or `[endpoints] xai_api_base_url`.
//!
//! Unusable (an auth error, or a 200 with a blocked, disabled, or team_blocked flag) means the key is not advertised.
//! Unknown (a timeout, a network error, or exhausted retries) fails open and the key is still advertised.
//!
//! The probe retries once within the wall budget on 429, 5xx, or transport errors.
//! The default timeout is 400ms for the whole probe including retries; live round trips run about 250ms at p95.

use std::time::{Duration, Instant};

use serde::Deserialize;

/// Wall-clock budget for the entire probe, covering all attempts and backoff.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// Returns the last 12 chars of a key for diagnostic logs, never the full secret.
/// This is a copy; importing it from `auth::model` would create an import cycle under Bazel.
fn key_suffix(t: &str) -> &str {
    t.len()
        .checked_sub(12)
        .and_then(|i| t.get(i..))
        .unwrap_or(t)
}

/// Whether `initialize` should HTTP-probe the first-party env key.
/// Skip (and treat as usable) when: the kill switch is on: the key will not be advertised either way, BYOK is present: it is advertised without probing the first-party env key, no env key is set, `preferred_method` is pinned: OIDC never advertises the key, and ApiKey fails closed.
/// A false-negative probe under an ApiKey pin would empty `auth_methods` with no login method to fall back to.
pub fn should_probe_first_party_env_key(
    disable_api_key_auth: bool,
    has_byok: bool,
    has_env_key: bool,
    preferred_method_pinned: bool,
) -> bool {
    !disable_api_key_auth && !has_byok && has_env_key && !preferred_method_pinned
}

/// How many retries follow the initial attempt.
const MAX_RETRIES: u32 = 1;

/// Fixed backoff before the single retry.
const RETRY_BACKOFF: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyProbeVerdict {
    Usable,
    /// An auth error, or a 200 with a blocked, disabled, or team_blocked flag.
    Unusable,
    /// A timeout, a network error, or exhausted retries; the probe fails open.
    Unknown,
}

impl ApiKeyProbeVerdict {
    pub fn allows_advertise(self) -> bool {
        match self {
            Self::Usable | Self::Unknown => true,
            Self::Unusable => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiKeyInfoBody {
    #[serde(default)]
    api_key_blocked: bool,
    #[serde(default)]
    api_key_disabled: bool,
    #[serde(default)]
    team_blocked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptOutcome {
    Done(ApiKeyProbeVerdict),
    Retry,
}

/// Joins `/api-key` onto the base URL, dropping any trailing slash first.
fn api_key_info_url(api_base_url: &str) -> String {
    let base = api_base_url.trim().trim_end_matches('/');
    format!("{base}/api-key")
}

/// Classifies the status and body; kept pure so unit tests can call it directly.
fn classify_probe_attempt(status: u16, body: &[u8]) -> AttemptOutcome {
    match status {
        200 => match serde_json::from_slice::<ApiKeyInfoBody>(body) {
            Ok(info) if info.api_key_blocked || info.api_key_disabled || info.team_blocked => {
                AttemptOutcome::Done(ApiKeyProbeVerdict::Unusable)
            }
            // An unparseable 200 fails open in case the API response shape changed
            Ok(_) | Err(_) => AttemptOutcome::Done(ApiKeyProbeVerdict::Usable),
        },
        // Permanent client and auth failures are not retried
        400..=403 => AttemptOutcome::Done(ApiKeyProbeVerdict::Unusable),
        // Rate limiting and server errors are retried once
        429 => AttemptOutcome::Retry,
        s if (500..600).contains(&s) => AttemptOutcome::Retry,
        // Any other 4xx fails open (e.g. a 404 from test mocks that lack this route).
        _ => AttemptOutcome::Done(ApiKeyProbeVerdict::Unknown),
    }
}

/// Terminal view of [`classify_probe_attempt`]; a retryable outcome becomes Unknown.
#[cfg(test)]
fn classify_probe_response(status: u16, body: &[u8]) -> ApiKeyProbeVerdict {
    match classify_probe_attempt(status, body) {
        AttemptOutcome::Done(v) => v,
        AttemptOutcome::Retry => ApiKeyProbeVerdict::Unknown,
    }
}

/// Fails open on a timeout or transport error after retries; the raw key is never logged.
///
/// `api_base_url` must be the endpoint the env key is actually sent to (`endpoints.xai_api_base_url`), not a hardcoded public default.
async fn probe_xai_api_key(key: &str, api_base_url: &str, timeout: Duration) -> ApiKeyProbeVerdict {
    let url = api_key_info_url(api_base_url);
    probe_xai_api_key_at_url(key, &url, timeout).await
}

/// Takes the full URL so tests can point the probe at a local server.
async fn probe_xai_api_key_at_url(key: &str, url: &str, timeout: Duration) -> ApiKeyProbeVerdict {
    if key.trim().is_empty() {
        return ApiKeyProbeVerdict::Unusable;
    }

    let client = distill_http::shared_client();
    let started = Instant::now();
    let deadline = started + timeout;
    let mut attempts: u32 = 0;
    let mut last_verdict = ApiKeyProbeVerdict::Unknown;

    loop {
        attempts += 1;
        let now = Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            break;
        }

        let request = client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
            .timeout(remaining);

        let outcome = match request.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.bytes().await.unwrap_or_default();
                classify_probe_attempt(status, &body)
            }
            Err(_) => AttemptOutcome::Retry,
        };

        match outcome {
            AttemptOutcome::Done(v) => {
                last_verdict = v;
                break;
            }
            AttemptOutcome::Retry => {
                last_verdict = ApiKeyProbeVerdict::Unknown;
                if attempts > MAX_RETRIES {
                    break;
                }
                let now = Instant::now();
                let remaining = deadline.saturating_duration_since(now);
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(RETRY_BACKOFF.min(remaining)).await;
            }
        }
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    distill_telemetry::unified_log::info(
        "auth: first-party API key probe",
        None,
        Some(serde_json::json!({
            "verdict": format!("{last_verdict:?}"),
            "allows_advertise": last_verdict.allows_advertise(),
            "elapsed_ms": elapsed_ms,
            "timeout_ms": timeout.as_millis() as u64,
            "attempts": attempts,
            "key_suffix": key_suffix(key),
        })),
    );

    last_verdict
}

/// Probes the env key when one is set; without an env key this returns false and the caller combines the result with BYOK.
/// `api_base_url` is the caller's effective `endpoints.xai_api_base_url`, so the probe follows the same endpoint as turn traffic.
/// In tests that is the mock server the fixtures already set via `GROK_XAI_API_BASE_URL`.
pub async fn first_party_env_key_allows_advertise(api_base_url: &str, timeout: Duration) -> bool {
    let Ok(key) = crate::auth_method::read_xai_api_key_env() else {
        return false;
    };
    probe_xai_api_key(&key, api_base_url, timeout)
        .await
        .allows_advertise()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_only_when_env_key_alone_would_suppress_login() {
        // Happy path: env key present, nothing else blocking.
        assert!(should_probe_first_party_env_key(false, false, true, false));
        // A kill switch, BYOK, a missing env key, or any pin each skip the probe and treat the key as usable
        assert!(!should_probe_first_party_env_key(true, false, true, false));
        assert!(!should_probe_first_party_env_key(false, true, true, false));
        assert!(!should_probe_first_party_env_key(
            false, false, false, false
        ));
        assert!(!should_probe_first_party_env_key(false, false, true, true));
    }

    #[test]
    fn joins_api_key_path_onto_base() {
        assert_eq!(
            api_key_info_url("https://api.x.ai/v1"),
            "https://api.x.ai/v1/api-key"
        );
        assert_eq!(
            api_key_info_url("https://api.x.ai/v1/"),
            "https://api.x.ai/v1/api-key"
        );
        assert_eq!(
            api_key_info_url("https://enterprise-api.acme.com/v1"),
            "https://enterprise-api.acme.com/v1/api-key"
        );
    }

    #[test]
    fn usable_on_200_clear_flags() {
        let body = br#"{"api_key_id":"k","api_key_blocked":false,"api_key_disabled":false,"team_blocked":false}"#;
        assert_eq!(
            classify_probe_response(200, body),
            ApiKeyProbeVerdict::Usable
        );
    }

    #[test]
    fn unusable_on_200_blocked() {
        let body = br#"{"api_key_blocked":true,"api_key_disabled":false}"#;
        assert_eq!(
            classify_probe_response(200, body),
            ApiKeyProbeVerdict::Unusable
        );
    }

    #[test]
    fn unusable_on_200_disabled() {
        let body = br#"{"api_key_blocked":false,"api_key_disabled":true}"#;
        assert_eq!(
            classify_probe_response(200, body),
            ApiKeyProbeVerdict::Unusable
        );
    }

    #[test]
    fn unusable_on_200_team_blocked() {
        let body = br#"{"api_key_blocked":false,"api_key_disabled":false,"team_blocked":true}"#;
        assert_eq!(
            classify_probe_response(200, body),
            ApiKeyProbeVerdict::Unusable
        );
    }

    #[test]
    fn usable_on_200_unparseable_body_fail_open() {
        assert_eq!(
            classify_probe_response(200, b"not-json"),
            ApiKeyProbeVerdict::Usable
        );
    }

    #[test]
    fn unusable_on_auth_errors() {
        for status in [400u16, 401, 402, 403] {
            assert_eq!(
                classify_probe_response(status, br#"{"error":"Incorrect API key"}"#),
                ApiKeyProbeVerdict::Unusable,
                "status {status}"
            );
        }
    }

    #[test]
    fn rate_limit_and_5xx_are_retryable() {
        assert_eq!(classify_probe_attempt(429, b""), AttemptOutcome::Retry);
        assert_eq!(classify_probe_attempt(503, b""), AttemptOutcome::Retry);
        assert_eq!(
            classify_probe_response(429, b""),
            ApiKeyProbeVerdict::Unknown
        );
    }

    #[test]
    fn unknown_on_other_4xx_fail_open() {
        assert_eq!(
            classify_probe_response(404, b""),
            ApiKeyProbeVerdict::Unknown
        );
    }

    #[tokio::test]
    async fn empty_key_is_unusable_without_network() {
        assert_eq!(
            probe_xai_api_key(
                "   ",
                "https://example.invalid/v1",
                Duration::from_millis(50)
            )
            .await,
            ApiKeyProbeVerdict::Unusable
        );
    }

    /// Serves sequential responses (one connection per attempt).
    fn serve_sequence(responses: Vec<(String, Vec<u8>)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status_line, body) in responses {
                if let Ok((mut stream, _)) = listener.accept() {
                    use std::io::{Read, Write};
                    let _ = stream.read(&mut [0u8; 2048]);
                    let resp = format!(
                        "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        String::from_utf8_lossy(&body)
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            }
        });
        format!("http://{addr}/v1/api-key")
    }

    fn serve_one_http_response(status_line: &str, body: &[u8]) -> String {
        serve_sequence(vec![(status_line.to_string(), body.to_vec())])
    }

    #[tokio::test]
    async fn local_server_invalid_key_is_unusable() {
        let url = serve_one_http_response(
            "HTTP/1.1 400 Bad Request",
            br#"{"error":"Incorrect API key"}"#,
        );
        let v = probe_xai_api_key_at_url("xai-bad", &url, Duration::from_secs(2)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Unusable);
    }

    #[tokio::test]
    async fn local_server_blocked_key_is_unusable() {
        let url = serve_one_http_response(
            "HTTP/1.1 200 OK",
            br#"{"api_key_blocked":true,"api_key_disabled":false}"#,
        );
        let v = probe_xai_api_key_at_url("xai-blocked", &url, Duration::from_secs(2)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Unusable);
    }

    #[tokio::test]
    async fn local_server_ok_key_is_usable() {
        let url = serve_one_http_response(
            "HTTP/1.1 200 OK",
            br#"{"api_key_id":"abc","api_key_blocked":false,"api_key_disabled":false}"#,
        );
        let v = probe_xai_api_key_at_url("xai-good", &url, Duration::from_secs(2)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Usable);
    }

    #[tokio::test]
    async fn probes_joined_base_url_path() {
        let url = serve_one_http_response(
            "HTTP/1.1 200 OK",
            br#"{"api_key_id":"abc","api_key_blocked":false,"api_key_disabled":false}"#,
        );
        // serve_sequence returns the full .../v1/api-key URL; strip it back to the base the way config stores it
        let base = url.trim_end_matches("/api-key");
        let v = probe_xai_api_key("xai-good", base, Duration::from_secs(2)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Usable);
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let url = serve_sequence(vec![
            (
                "HTTP/1.1 429 Too Many Requests".into(),
                br#"{"error":"rate limited"}"#.to_vec(),
            ),
            (
                "HTTP/1.1 200 OK".into(),
                br#"{"api_key_id":"ok","api_key_blocked":false,"api_key_disabled":false}"#.to_vec(),
            ),
        ]);
        let v = probe_xai_api_key_at_url("xai-retry", &url, Duration::from_secs(2)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Usable);
    }

    #[tokio::test]
    async fn timeout_is_unknown_fail_open() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            // Accept and stall past the client timeout (both attempts).
            for _ in 0..2 {
                if let Ok((_stream, _)) = listener.accept() {
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        });

        let url = format!("http://{addr}/v1/api-key");
        let v = probe_xai_api_key_at_url("xai-slow", &url, Duration::from_millis(80)).await;
        assert_eq!(v, ApiKeyProbeVerdict::Unknown);
        assert!(v.allows_advertise());
    }
}
