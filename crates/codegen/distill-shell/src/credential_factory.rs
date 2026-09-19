// Modified for Distill by Samuel Fajreldines, 2026.
//! Shell-side credential factories.
//!
//! These bind the login crate's credential providers to the shell's
//! `managed_config` deployment-id resolver, which the low-level login crate
//! cannot depend on. Everything else lives in `distill_login::credential_provider`.

use std::sync::Arc;

use distill_auth::{AuthCredentialProvider, StaticAuthCredentialProvider};

use distill_login::AuthManager;
use distill_login::credential_provider::{
    ShellAuthCredentialProvider, StorageClientAttributionBridge,
};
use distill_login::grok_auth_credentials::GrokAuthCredentials;

/// Build a `StorageClient` for proxy uploads. Pass the correct `client_identifier` so requests can be attributed.
/// When `auth_manager` is `Some`, use the live provider (refresh and 401 recovery); otherwise fall back to a static token.
/// `user_token` is only for AuthManager-less one-shots; live paths pass the AuthManager plus an optional deployment key.
pub fn build_storage_client_for_proxy(
    proxy_base_url: &str,
    deployment_key: Option<String>,
    alpha_test_key: Option<String>,
    auth_manager: Option<Arc<AuthManager>>,
    user_token: Option<String>,
    session_id: Option<String>,
    client_identifier: &str, // "grok-shell" or "grok-pager" etc.
) -> distill_file_utils::storage_client::StorageClient {
    let http_client = distill_http::shared_upload_client();
    if let Some(am) = auth_manager {
        let provider: Arc<dyn AuthCredentialProvider> =
            Arc::new(ShellAuthCredentialProvider::with_deployment_id_resolver(
                am.clone(),
                deployment_key,
                alpha_test_key,
                std::sync::Arc::new(crate::managed_config::resolve_deployment_id),
            ));
        let bridge: Arc<dyn distill_file_utils::storage_client::Auth401AttributionCallback> =
            Arc::new(StorageClientAttributionBridge::new(am, session_id));
        distill_file_utils::storage_client::StorageClient::with_provider(
            proxy_base_url,
            http_client,
            provider,
        )
        .with_client_identity(distill_version::VERSION, client_identifier)
        .with_client_mode(distill_http::process_client_mode())
        .with_attribution(bridge)
    } else {
        let mut creds = GrokAuthCredentials::new(user_token);
        creds.deployment_key = deployment_key;
        creds.alpha_test_key = alpha_test_key;
        let wire_bearer = creds
            .deployment_key
            .clone()
            .or_else(|| creds.user_token.clone());
        let provider: Arc<dyn AuthCredentialProvider> = Arc::new(
            StaticAuthCredentialProvider::new(Box::new(creds), wire_bearer),
        );
        distill_file_utils::storage_client::StorageClient::with_provider(
            proxy_base_url,
            http_client,
            provider,
        )
        .with_client_identity(distill_version::VERSION, client_identifier)
        .with_client_mode(distill_http::process_client_mode())
    }
}

/// Bootstrap the OTel credential provider both pager and TUI need at tracing init time.
/// Binds the login factory to the shell deployment-id resolver; the provider starts disk-read-only.
/// Call [`distill_login::credential_provider::wire_otel_auth_manager`] after agent init to upgrade it.
pub fn build_bootstrap_otel_credentials() -> (Arc<dyn AuthCredentialProvider>, String) {
    let proxy_base_url = crate::agent::config::EndpointsConfig::from_effective_config().proxy_url();
    distill_login::credential_provider::install_bootstrap_otel_provider(
        proxy_base_url,
        std::sync::Arc::new(crate::managed_config::resolve_deployment_id),
    )
}
