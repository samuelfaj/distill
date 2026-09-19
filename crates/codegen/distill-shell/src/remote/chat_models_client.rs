// Modified for Distill by Samuel Fajreldines, 2026.
//! The grok.com chat model catalog (`POST /rest/modes`): the models grok-web's chat picker shows, distinct from the CLI `/v1/models` build catalog.
//! Transport only; the cache and the ACP mapping live in [`crate::agent::chat_modes`].

use std::sync::Arc;

use serde::Deserialize;

use distill_login::AuthManager;

const GROK_WEB_URL: &str = "https://grok.com";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub badge_text: Option<String>,
    #[serde(default)]
    pub availability: ModeAvailability,
    #[serde(default)]
    pub icon_hint: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Mode {
    pub fn is_available(&self) -> bool {
        self.availability.available.is_some()
    }
}

/// proto3-JSON oneof: exactly one field is present.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeAvailability {
    #[serde(default)]
    pub available: Option<serde_json::Value>,
    #[serde(default)]
    pub unavailable: Option<serde_json::Value>,
    #[serde(default)]
    pub requires_upgrade: Option<serde_json::Value>,
    #[serde(default)]
    pub coming_soon: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListModesResponse {
    #[serde(default)]
    pub modes: Vec<Mode>,
    #[serde(default)]
    pub default_mode_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ChatModelsError {
    #[error("no grok.com credentials")]
    NoAuth,
    #[error("request timed out")]
    Timeout,
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("request failed: {status}")]
    Http { status: u16 },
    #[error("parse error: {0}")]
    Parse(#[from] serde_json::Error),
}

/// Stateless transport for `POST /rest/modes`; caching lives in [`crate::agent::chat_modes::ChatModesManager`].
pub struct ChatModelsClient {
    http: reqwest::Client,
    base_url: String,
    auth: Arc<AuthManager>,
}

impl ChatModelsClient {
    pub fn new(auth: Arc<AuthManager>) -> Self {
        let base_url = std::env::var("GROK_MODES_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::env::var("GROK_CONVERSATIONS_BASE_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .or_else(|| {
                std::env::var("GROK_CODE_WEB_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| GROK_WEB_URL.to_string());
        Self {
            http: crate::http::shared_client(),
            base_url,
            auth,
        }
    }

    /// Gated only on a valid grok.com bearer, not `is_xai_auth()` like workspaces/conversations.
    /// `/rest/modes` is the public chat endpoint, and that gate would exclude API-key and cached-token chat users.
    pub(crate) async fn list_modes(
        &self,
        locale: &str,
    ) -> Result<ListModesResponse, ChatModelsError> {
        let auth = self
            .auth
            .auth()
            .await
            .map_err(|_| ChatModelsError::NoAuth)?;

        let url = format!("{}/rest/modes", self.base_url);
        let body = serde_json::json!({ "locale": locale });
        let mut builder = self
            .http
            .post(&url)
            .json(&body)
            .header("Authorization", format!("Bearer {}", auth.key))
            .header(
                "X-XAI-Token-Auth",
                self.auth.grok_com_config().token_header.clone(),
            )
            .header("x-userid", &auth.user_id)
            .header("x-grok-client-version", distill_version::VERSION)
            .header(
                "x-grok-client-identifier",
                crate::http::process_client_identifier(),
            )
            .header(
                crate::http::CLIENT_MODE_HEADER,
                crate::http::process_client_mode(),
            )
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(email) = &auth.email {
            builder = builder.header("x-email", email);
        }
        let builder = distill_otel::inject_trace_context_into_request(builder);

        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(ChatModelsError::Http {
                status: status.as_u16(),
            });
        }

        let bytes = response.bytes().await?;
        let resp: ListModesResponse = serde_json::from_slice(&bytes)?;
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_parse_camelcase_wire() {
        let json = serde_json::json!({
            "modes": [{
                "id": "auto",
                "title": "Auto",
                "description": "Picks the best model",
                "badgeText": "New",
                "availability": { "available": {} },
                "iconHint": "rocket",
                "tags": ["TAG_PRIMARY"]
            }, {
                "id": "heavy",
                "title": "Heavy",
                "availability": { "requiresUpgrade": { "message": "Upgrade" } }
            }],
            "defaultModeId": "auto"
        });
        let resp: ListModesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.default_mode_id, "auto");
        let [auto, heavy] = resp.modes.as_slice() else {
            panic!("expected two modes: {:?}", resp.modes);
        };
        assert_eq!(auto.id, "auto");
        assert_eq!(auto.title, "Auto");
        assert_eq!(auto.badge_text.as_deref(), Some("New"));
        assert_eq!(auto.icon_hint, "rocket");
        assert_eq!(auto.tags, vec!["TAG_PRIMARY".to_string()]);
        assert!(auto.is_available());
        assert!(!heavy.is_available());
    }

    #[test]
    fn missing_fields_default_gracefully() {
        let json = serde_json::json!({ "modes": [{ "id": "m1" }] });
        let resp: ListModesResponse = serde_json::from_value(json).unwrap();
        let [m] = resp.modes.as_slice() else {
            panic!("expected one mode: {:?}", resp.modes);
        };
        assert_eq!(m.id, "m1");
        assert!(m.title.is_empty());
        assert!(m.description.is_empty());
        assert!(m.badge_text.is_none());
        // With no availability field on the wire, the mode is not selectable
        assert!(!m.is_available());
        assert!(resp.default_mode_id.is_empty());
    }
}
