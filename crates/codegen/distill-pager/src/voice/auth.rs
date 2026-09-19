// Modified for Distill by Samuel Fajreldines, 2026.
//! Bridge the shell's `AuthManager` onto the voice crate's bearer provider.
//!
//! voice-api accepts both API keys and OAuth2 tokens directly at `api.x.ai` and attributes per-user billing for OAuth.
//! The voice channel reuses the same bearer the agent uses for chat (no separate env var).
//!
//! Resolved per request: the agent's refreshing manager in direct-spawn mode.
//! In leader mode, a non-refreshing one adopts the agent's rotated `auth.json` token under the file lock (see [`crate::acp`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use distill_tools::types::SharedApiKeyProvider;
use distill_voice::{SharedVoiceAuth, VoiceAuthProvider};

/// Adapts the shell's `ApiKeyProvider` onto [`VoiceAuthProvider`].
///
/// Resolves a token per request (never a static snapshot), so a long session follows the `AuthManager` instead of pinning a token that 401s.
struct AuthManagerVoiceAuth(SharedApiKeyProvider);

impl std::fmt::Debug for AuthManagerVoiceAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthManagerVoiceAuth")
    }
}

impl VoiceAuthProvider for AuthManagerVoiceAuth {
    fn bearer(&self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + '_>> {
        let provider = self.0.clone();
        Box::pin(async move { provider.current_api_key_async().await })
    }
}

/// Build the voice bearer provider from the connection's `AuthManager`.
///
/// Works for every auth method: OAuth / grok.com / OIDC session tokens and `XAI_API_KEY` / per-model BYOK keys.
pub fn build_voice_auth(auth_manager: Arc<distill_login::AuthManager>) -> SharedVoiceAuth {
    Arc::new(AuthManagerVoiceAuth(
        distill_login::shared_api_key_provider(auth_manager),
    ))
}
