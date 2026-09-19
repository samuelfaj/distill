// Modified for Distill by Samuel Fajreldines, 2026.
//! The Distill backend: Grok OAuth2, enterprise OIDC, the operator's auth binary, or a devbox.

use std::sync::Arc;

use super::{AuthBackend, LoginRequest};
use crate::refresh::{
    AuthSnapshot, DiagnosticUploader, ExternalBinaryRefresher, ExternalCommandRunner,
    OidcRefresher, TokenRefresher,
};
use crate::{AuthManager, GrokAuth, GrokComConfig};

#[derive(Default)]
pub struct GrokAuthBackend;

#[async_trait::async_trait(?Send)]
impl AuthBackend for GrokAuthBackend {
    fn scope_key(&self, config: &GrokComConfig) -> String {
        config.auth_scope()
    }

    /// Devbox auth files from before the OIDC flow wrote this key, and only this backend ever minted credentials into it.
    fn inherited_scopes(&self) -> &'static [&'static str] {
        &[crate::model::LEGACY_SCOPE]
    }

    /// An Grok login can come from OAuth2, a customer's own login provider, the auth binary, or a devbox, so there is no one issuer to check for.
    /// Saying yes to all of them is safe: a credential minted elsewhere still gets sent to Grok, which rejects it.
    fn owns(&self, _auth: &GrokAuth) -> bool {
        true
    }

    /// Some customers run their own gateway and sign in there with the session Grok issued them, so a list of allowed hosts would lock them out.
    /// The models cache stops one backend's models from being used by another: it remembers the URL each entry came from and ignores the rest.
    fn may_receive_session(&self, _url: &str) -> bool {
        true
    }

    fn login_host(&self, config: &GrokComConfig) -> String {
        super::host_of(&config.grok_ws_origin)
    }

    fn is_xai_authority(&self) -> bool {
        true
    }

    async fn login(&self, req: LoginRequest<'_>) -> anyhow::Result<(GrokAuth, bool)> {
        crate::flow::run_auth_flow_steps(
            req.auth_manager,
            req.grok_com_config,
            req.config_device_flow,
            req.reauth,
            req.force_interactive,
            req.on_stderr,
            req.url_tx,
            req.code_rx,
            req.login_override,
        )
        .await
    }

    fn refresher(
        &self,
        manager: Arc<AuthManager>,
        auth_provider_command: Option<String>,
        diagnostic_uploader: Option<DiagnosticUploader>,
    ) -> Arc<dyn TokenRefresher> {
        match auth_provider_command {
            Some(cmd) => {
                let snapshot: Arc<dyn AuthSnapshot> = manager.clone();
                let runner: Arc<dyn ExternalCommandRunner> = manager;
                Arc::new(ExternalBinaryRefresher::new(runner, snapshot, cmd))
            }
            None => {
                let snapshot: Arc<dyn AuthSnapshot> = manager;
                let refresher = OidcRefresher::new(snapshot);
                match diagnostic_uploader {
                    Some(uploader) => Arc::new(refresher.with_diagnostic_upload(uploader)),
                    None => Arc::new(refresher),
                }
            }
        }
    }
}
