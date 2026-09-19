// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

mod common;

use distill_shell::agent::remote_config::settings_get::{SettingsQuery, get_settings};
use distill_shell::util::config::RemoteSettings;

/// A live fetch made with an in-memory session that is not on disk yet (a fresh
/// login or token refresh) must be served this boot, not discarded because the
/// commit gate re-reads a different disk identity.
#[test]
fn live_fetch_with_unpersisted_session_is_served() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["served-in-memory".into()]),
            ..RemoteSettings::default()
        });

        // Disk holds `test-user` (seeded above); warm with a different in-memory
        // session that has not been persisted.
        let fresh: distill_login::GrokAuth = serde_json::from_value(serde_json::json!({
            "key": "fresh-session-token",
            "auth_mode": "oidc",
            "oidc_issuer": distill_login::xai_oauth2_issuer(),
            "create_time": "2026-01-01T00:00:00Z",
            "expires_at": "2099-01-01T00:00:00Z",
            "user_id": "fresh-user",
        }))
        .expect("build in-memory session");

        let outcome = get_settings(SettingsQuery::from_auth(Some(fresh))).await;

        assert!(
            outcome.attempted(),
            "a live fetch ran, so the outcome must be marked attempted"
        );
        assert_eq!(
            outcome.settings().and_then(|s| s.tips.clone()),
            Some(vec!["served-in-memory".to_string()]),
            "the fetch succeeded, so its settings must be served even though the \
             session is not on disk yet"
        );
    });
}
