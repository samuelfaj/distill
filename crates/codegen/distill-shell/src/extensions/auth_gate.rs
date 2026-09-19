// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;

use distill_login::{AuthManager, GrokAuth};

/// Require Grok auth from a sync context: with no `.await` to refresh, a token inside the client's early-invalidation buffer still counts.
pub(crate) fn require_xai_auth(
    auth_manager: &AuthManager,
    missing_message: &'static str,
    non_xai_message: &'static str,
) -> Result<GrokAuth, acp::Error> {
    let auth = auth_manager
        .current_or_expired()
        .ok_or_else(|| acp::Error::auth_required().data(missing_message))?;
    if !auth.is_xai_auth() {
        return Err(acp::Error::auth_required().data(non_xai_message));
    }
    Ok(auth)
}
