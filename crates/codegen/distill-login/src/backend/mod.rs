// Modified for Distill by Samuel Fajreldines, 2026.
//! Compile-time selection of the authority this build talks to.
//!
//! One implementation compiles, so the trait is a checklist: a backend that forgets a decision fails to build.
use crate::flow::StderrCallback;
use crate::refresh::{DiagnosticUploader, TokenRefresher};
use crate::{AuthManager, AuthUrlInfo, GrokAuth, GrokComConfig, LoginTransportOverride};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
mod grok;
/// The inputs of one login attempt.
pub struct LoginRequest<'a> {
    pub auth_manager: &'a Arc<AuthManager>,
    pub grok_com_config: &'a GrokComConfig,
    /// `[grok_com_config] login_device_flow` config tier, resolved by the caller (no longer a `GrokComConfig` field).
    pub config_device_flow: Option<bool>,
    pub reauth: bool,
    pub force_interactive: bool,
    pub on_stderr: Option<StderrCallback>,
    pub url_tx: Option<Rc<RefCell<Option<oneshot::Sender<AuthUrlInfo>>>>>,
    pub code_rx: Option<mpsc::Receiver<String>>,
    pub login_override: LoginTransportOverride,
}
/// `?Send`: `url_tx` is an `Rc`, so a login future can never cross threads.
#[async_trait::async_trait(?Send)]
pub trait AuthBackend {
    /// Key under which this backend owns its entry in auth.json.
    fn scope_key(&self, config: &GrokComConfig) -> String;
    /// Older scope keys this backend minted, and so may adopt from and tidy, most recent first.
    fn inherited_scopes(&self) -> &'static [&'static str];
    /// Whether this backend minted the credential, which the scope key alone cannot establish.
    fn owns(&self, auth: &GrokAuth) -> bool;
    /// Whether this backend's session token may be sent to `url`.
    /// A model entry carries its own base URL, so without this a poisoned or hand-edited entry aims the bearer anywhere.
    fn may_receive_session(&self, url: &str) -> bool;
    /// The host to name when telling the user whose session they hold.
    fn login_host(&self, config: &GrokComConfig) -> String;
    /// Whether Grok issued this backend's credentials and may therefore receive them.
    /// Gates every request that carries the bearer to an Grok host, and every Grok-only policy.
    fn is_xai_authority(&self) -> bool;
    /// Obtain a credential; the flag reports whether a login actually ran.
    async fn login(&self, req: LoginRequest<'_>) -> anyhow::Result<(GrokAuth, bool)>;
    /// The renewal authority for the credentials this backend mints.
    fn refresher(
        &self,
        manager: Arc<AuthManager>,
        auth_provider_command: Option<String>,
        diagnostic_uploader: Option<DiagnosticUploader>,
    ) -> Arc<dyn TokenRefresher>;
}
pub type ActiveAuthBackend = grok::GrokAuthBackend;
/// Reports a URL the way a user says it, without the scheme.
pub fn host_of(url: &str) -> String {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_of_drops_the_scheme_and_leaves_the_rest_alone() {
        assert_eq!(host_of("https://example.test"), "example.test");
        assert_eq!(host_of("http://localhost:8080"), "localhost:8080");
        assert_eq!(host_of("grok.com"), "grok.com");
    }
}
