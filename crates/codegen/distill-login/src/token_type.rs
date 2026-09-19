// Modified for Distill by Samuel Fajreldines, 2026.
use crate::model::{AuthMode, GrokAuth};

/// What kind of bearer is loaded right now; the dispatch key for `auth()`, `unauthorized_recovery()`, and proactive refresh.
/// It does not classify sessions; use `is_session_based_method` for that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    /// OIDC/OAuth2 session with a refresh_token available.
    OidcSession,
    /// Legacy web-login session or OIDC without a refresh_token.
    LegacySession,
    /// External auth binary provides tokens.
    ExternalBinary,
    /// Plain API key (no refresh possible).
    ApiKey,
    /// No credentials loaded.
    None,
}

impl TokenType {
    /// Classify the loaded credential (pure; no manager state).
    pub fn from_auth(auth: Option<&GrokAuth>) -> Self {
        match auth {
            None => Self::None,
            // Oidc without a refresh_token cannot be refreshed, so it counts as LegacySession
            Some(a) => match a.auth_mode {
                AuthMode::Oidc if a.refresh_token.is_some() => Self::OidcSession,
                AuthMode::Oidc | AuthMode::WebLogin => Self::LegacySession,
                AuthMode::External => Self::ExternalBinary,
                AuthMode::ApiKey => Self::ApiKey,
            },
        }
    }

    /// `true` for types that can be silently refreshed (OIDC, external binary).
    pub fn is_refreshable(self) -> bool {
        matches!(self, Self::OidcSession | Self::ExternalBinary)
    }

    /// Converts to the telemetry enum for the `manual_auth` KPI; the mapping is stable.
    pub fn telemetry_kind(self) -> distill_telemetry::events::AuthTokenKind {
        use distill_telemetry::events::AuthTokenKind as K;
        match self {
            Self::OidcSession => K::OidcSession,
            Self::ExternalBinary => K::ExternalBinary,
            Self::LegacySession => K::LegacySession,
            Self::ApiKey => K::ApiKey,
            Self::None => K::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_refreshable_matrix() {
        assert!(TokenType::OidcSession.is_refreshable());
        assert!(TokenType::ExternalBinary.is_refreshable());
        assert!(!TokenType::LegacySession.is_refreshable());
        assert!(!TokenType::ApiKey.is_refreshable());
        assert!(!TokenType::None.is_refreshable());
    }
}
