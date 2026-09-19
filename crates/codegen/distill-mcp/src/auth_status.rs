use std::sync::Arc;

use url::Url;

use crate::credentials::{McpCredentialStore, McpCredentialStoreAdapter, ObservedAccessToken};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpOauthDiscovery {
    Disk,
    Network,
}

pub(crate) enum HttpAuthDecision {
    NoOauthSupport,
    ManagerReady {
        manager: Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
        observed: ObservedAccessToken,
    },
    NeedsInteractiveLogin,
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredAuthVerdict {
    StoredCredentials,
    NeedsLogin,
    Unknown,
}

fn classify_stored_http_auth(
    credentials: &McpCredentialStore,
    server_name: &str,
    server_url: &Url,
) -> StoredAuthVerdict {
    match credentials.get(server_name, server_url) {
        Some(creds) if creds.token_response.is_some() => StoredAuthVerdict::StoredCredentials,
        Some(_) => StoredAuthVerdict::NeedsLogin,
        None => StoredAuthVerdict::Unknown,
    }
}

pub(crate) async fn decide_http_auth_from_disk(server_name: &str, url: &str) -> HttpAuthDecision {
    let Ok(parsed_url) = Url::parse(url) else {
        return HttpAuthDecision::NoOauthSupport;
    };
    let name = server_name.to_string();
    let key_url = parsed_url.clone();
    let (verdict, stored_token) = tokio::task::spawn_blocking(move || {
        let credentials = McpCredentialStore::load_default().unwrap_or_default();
        let verdict = classify_stored_http_auth(&credentials, &name, &key_url);
        let stored_token = {
            use oauth2::TokenResponse as _;
            credentials
                .get(&name, &key_url)
                .and_then(|c| c.token_response.as_ref())
                .map(|t| t.access_token().secret().clone())
        };
        (verdict, stored_token)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(server = server_name, %e, "stored auth classification task failed");
        (StoredAuthVerdict::Unknown, None)
    });

    match verdict {
        StoredAuthVerdict::StoredCredentials => {
            match rmcp::transport::auth::AuthorizationManager::new(url).await {
                Ok(mut manager) => {
                    let adapter =
                        McpCredentialStoreAdapter::new(server_name.to_string(), parsed_url);
                    let observed = adapter.observed();
                    // Seed so reauth does not read this token back as fresh.
                    observed.record(stored_token);
                    manager.set_credential_store(adapter);
                    tracing::info!(
                        server = server_name,
                        "Using stored OAuth credentials (discovery deferred)"
                    );
                    HttpAuthDecision::ManagerReady {
                        manager: Arc::new(tokio::sync::Mutex::new(manager)),
                        observed,
                    }
                }
                Err(e) => {
                    tracing::warn!(server = server_name, %e, "Failed to create OAuth manager");
                    HttpAuthDecision::NoOauthSupport
                }
            }
        }
        StoredAuthVerdict::NeedsLogin => {
            tracing::info!(
                server = server_name,
                "Stored login has no token; authenticate in TUI or set an Authorization header"
            );
            HttpAuthDecision::NeedsInteractiveLogin
        }
        StoredAuthVerdict::Unknown => HttpAuthDecision::NoOauthSupport,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_auth_classification_matrix() {
        let url = Url::parse("https://mcp.example.com/mcp").unwrap();
        let empty = McpCredentialStore::default();
        assert_eq!(
            classify_stored_http_auth(&empty, "srv", &url),
            StoredAuthVerdict::Unknown
        );

        let mut creds = McpCredentialStore::default();
        creds.insert_rmcp(
            "srv",
            &url,
            rmcp::transport::auth::StoredCredentials::new("c".to_string(), None, Vec::new(), None),
        );
        assert_eq!(
            classify_stored_http_auth(&creds, "srv", &url),
            StoredAuthVerdict::NeedsLogin,
            "a credential entry without a token is an unfinished login"
        );

        let expired_with_refresh: rmcp::transport::auth::StoredCredentials = serde_json::from_str(
            r#"{"client_id":"c","token_response":{"access_token":"at","token_type":"bearer","expires_in":1,"refresh_token":"rt"}}"#,
        )
        .unwrap();
        creds.insert_rmcp("srv", &url, expired_with_refresh);
        assert_eq!(
            classify_stored_http_auth(&creds, "srv", &url),
            StoredAuthVerdict::StoredCredentials,
            "an expired token with a refresh token still classifies as stored"
        );
    }
}
