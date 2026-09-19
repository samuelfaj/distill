//! Use a ChatGPT Codex subscription from this harness.
//!
//! The Codex CLI keeps its own sign-in in `~/.codex/auth.json` (`tokens.access_token`
//! plus the workspace `account_id`). This module reads **that** file — no second
//! OAuth flow, no second place for the token to live, and revoking the CLI's
//! sign-in revokes this one too — and turns it into the two things the sampler
//! needs for the Codex backend: a bearer, and the `chatgpt-account-id` header.
//!
//! What it deliberately does not do: refresh. The Codex CLI owns the refresh
//! token, and a second refresher would rotate it out from under the CLI. An
//! expired token therefore fails the request loudly, and the fix is the one the
//! user already knows (`codex login`), rather than a silent half-working path.
//!
//! Everything here is a pure read of a file the user owns: no network, no writes,
//! and the token is never logged.

use std::path::PathBuf;
use std::sync::Arc;

use xai_grok_sampler::config::{BearerResolver, SamplerConfig};

/// The Codex CLI's own home directory, when it is set.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";
/// File the Codex CLI writes its credentials to, inside the Codex home.
pub const CODEX_AUTH_FILE: &str = "auth.json";
/// The backend the ChatGPT subscription is served from.
pub const CODEX_BACKEND_HOST: &str = "chatgpt.com";
/// Its path prefix.
pub const CODEX_BACKEND_PATH: &str = "/backend-api/codex";
/// Header carrying the workspace the subscription belongs to.
pub const CODEX_ACCOUNT_HEADER: &str = "chatgpt-account-id";
/// Header the backend uses to tell its own clients apart.
pub const CODEX_ORIGINATOR_HEADER: &str = "originator";
/// The value the Codex CLI sends; the backend accepts its own clients.
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// Credentials read from the Codex CLI's auth file. Never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexAuth {
    pub access_token: String,
    pub account_id: Option<String>,
}

impl std::fmt::Debug for CodexAuth {
    /// The token is a secret: debug output says whether it is there, never what it is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexAuth")
            .field("access_token", &"<redacted>")
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// Where the Codex CLI keeps its credentials.
pub fn auth_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os(CODEX_HOME_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home).join(CODEX_AUTH_FILE));
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".codex").join(CODEX_AUTH_FILE))
}

/// Reads the credentials, or `None` when the file is absent or unusable.
///
/// `None` is the honest answer for every failure: a missing file, unreadable
/// bytes, a JSON shape this build does not know, or a sign-in that carried only
/// an API key (which belongs to a different backend).
pub fn read_codex_auth() -> Option<CodexAuth> {
    let path = auth_path()?;
    let raw = std::fs::read_to_string(path).ok()?;
    parse_codex_auth(&raw)
}

/// The same read, from bytes, so the shape is testable without a home directory.
pub fn parse_codex_auth(raw: &str) -> Option<CodexAuth> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let tokens = value.get("tokens")?;
    let access_token = tokens.get("access_token")?.as_str()?.trim();
    if access_token.is_empty() {
        return None;
    }
    Some(CodexAuth {
        access_token: access_token.to_owned(),
        account_id: tokens
            .get("account_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned),
    })
}

/// Whether a base URL points at the Codex backend.
pub fn is_codex_backend(base_url: &str) -> bool {
    let lowered = base_url.trim().to_ascii_lowercase();
    lowered.contains(CODEX_BACKEND_HOST) && lowered.contains(CODEX_BACKEND_PATH)
}

/// A resolver the sampler can poll for the current bearer.
///
/// It re-reads the file on each call rather than caching: the Codex CLI rewrites
/// that file when it refreshes, and a cached token would go stale exactly when the
/// CLI renewed it.
#[derive(Debug, Default)]
struct CodexFileBearer;

impl BearerResolver for CodexFileBearer {
    fn current_bearer(&self) -> Option<String> {
        read_codex_auth().map(|auth| auth.access_token)
    }
}

/// The resolver for [`SamplerConfig::bearer_resolver`].
pub fn codex_bearer_resolver() -> xai_grok_sampler::config::SharedBearerResolver {
    Arc::new(CodexFileBearer)
}

/// The headers the Codex backend requires alongside the bearer.
///
/// Empty when the credentials do not name an account: the backend then answers
/// with a clear error, which is better than inventing an account id.
pub fn codex_headers() -> indexmap::IndexMap<String, String> {
    let mut headers = indexmap::IndexMap::new();
    headers.insert(CODEX_ORIGINATOR_HEADER.to_owned(), CODEX_ORIGINATOR.to_owned());
    if let Some(account_id) = read_codex_auth().and_then(|auth| auth.account_id) {
        headers.insert(CODEX_ACCOUNT_HEADER.to_owned(), account_id);
    }
    headers
}

/// Applies the Codex backend's requirements to a resolved sampler config.
///
/// Called from the model-resolution path so a plain `[model.<id>]` entry pointing
/// at the Codex backend works with no bespoke wiring: the bearer comes from the
/// CLI's sign-in, the account header is added, and the API backend is the
/// Responses API the subscription speaks. Anything already configured wins, so an
/// explicit choice is never overridden.
pub fn apply_codex_backend(cfg: &mut SamplerConfig) {
    if !is_codex_backend(&cfg.base_url) {
        return;
    }
    if cfg.api_backend == xai_grok_sampler::ApiBackend::default() {
        cfg.api_backend = xai_grok_sampler::ApiBackend::Responses;
    }
    if cfg.bearer_resolver.is_none() && cfg.api_key.is_none() {
        cfg.bearer_resolver = Some(codex_bearer_resolver());
    }
    for (name, value) in codex_headers() {
        cfg.extra_headers.entry(name).or_insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "header.payload.signature",
            "access_token": "eyJhbGciOi-test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": "3f0c9c4e-1111-2222-3333-444455556666"
        },
        "last_refresh": "2026-09-09T20:53:40.794860Z"
    }"#;

    #[test]
    fn the_codex_sign_in_is_read_and_never_printed() {
        let auth = parse_codex_auth(SAMPLE).expect("the CLI's shape is read");
        assert_eq!(auth.access_token, "eyJhbGciOi-test-access-token");
        assert_eq!(
            auth.account_id.as_deref(),
            Some("3f0c9c4e-1111-2222-3333-444455556666")
        );
        let shown = format!("{auth:?}");
        assert!(!shown.contains("eyJhbGciOi-test-access-token"), "{shown}");
        assert!(shown.contains("redacted"), "{shown}");
    }

    #[test]
    fn anything_that_is_not_a_usable_sign_in_is_none() {
        // No tokens at all: an API-key-only sign-in belongs to another backend.
        assert!(parse_codex_auth(r#"{"OPENAI_API_KEY": "sk-test"}"#).is_none());
        // Empty or whitespace token.
        assert!(parse_codex_auth(r#"{"tokens": {"access_token": "   "}}"#).is_none());
        assert!(parse_codex_auth(r#"{"tokens": {"access_token": null}}"#).is_none());
        // Not JSON at all.
        assert!(parse_codex_auth("not json").is_none());
        // A token with no account still reads: the account header is optional.
        let auth = parse_codex_auth(r#"{"tokens": {"access_token": "abc"}}"#).expect("token");
        assert_eq!(auth.account_id, None);
    }

    #[test]
    fn only_the_codex_backend_is_treated_as_one() {
        assert!(is_codex_backend("https://chatgpt.com/backend-api/codex"));
        assert!(is_codex_backend("https://chatgpt.com/backend-api/codex/"));
        assert!(!is_codex_backend("https://api.x.ai/v1"));
        assert!(!is_codex_backend("https://openrouter.ai/api/v1"));
        // The host without the path is a different service on the same domain.
        assert!(!is_codex_backend("https://chatgpt.com/backend-api/other"));
    }

    #[test]
    fn a_codex_entry_gets_the_bearer_the_account_header_and_the_responses_backend() {
        let mut cfg = SamplerConfig {
            base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
            model: "gpt-5-codex".to_owned(),
            ..Default::default()
        };
        apply_codex_backend(&mut cfg);
        assert_eq!(cfg.api_backend, xai_grok_sampler::ApiBackend::Responses);
        assert!(cfg.bearer_resolver.is_some(), "the bearer comes from the CLI sign-in");
        assert_eq!(
            cfg.extra_headers.get(CODEX_ORIGINATOR_HEADER).map(String::as_str),
            Some(CODEX_ORIGINATOR)
        );

        // Another provider is left exactly as it was.
        let mut other = SamplerConfig {
            base_url: "https://api.x.ai/v1".to_owned(),
            ..Default::default()
        };
        apply_codex_backend(&mut other);
        assert!(other.bearer_resolver.is_none());
        assert!(other.extra_headers.is_empty());

        // An explicit choice is never overridden.
        let mut explicit = SamplerConfig {
            base_url: "https://chatgpt.com/backend-api/codex".to_owned(),
            api_key: Some("sk-explicit".to_owned()),
            ..Default::default()
        };
        apply_codex_backend(&mut explicit);
        assert!(explicit.bearer_resolver.is_none(), "a configured key wins");
    }
}
