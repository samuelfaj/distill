// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

mod common;

use std::time::Duration;

use distill_shell::agent::remote_config::settings_get::{
    SettingsQuery, SettingsWait, block_on_startup_settings, warm_startup_settings,
};
use distill_shell::util::config::RemoteSettings;

/// A boot that needs a policy repair must not pay repair and settings fetch
/// back to back: the gate-exit warm starts the one startup load and
/// bootstrap's bounded wait joins it. A warm while the repair is still
/// pending sends nothing and must not decide the one-shot startup cell.
#[test]
fn gate_exit_warm_starts_the_one_startup_load_after_the_repair_settles() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["from-server".into()]),
            ..RemoteSettings::default()
        });
        let personal_auth =
            std::fs::read(home.path().join("auth.json")).expect("read seeded auth.json");

        // A team principal with no serving managed policy: repair is pending.
        let scope = distill_shell::auth::GrokComConfig::default().auth_scope();
        let team_auth = serde_json::json!({
            scope: {
                "key": "team-session-token",
                "auth_mode": "oidc",
                "oidc_issuer": distill_shell::auth::xai_oauth2_issuer(),
                "create_time": "2026-01-01T00:00:00Z",
                "expires_at": "2099-01-01T00:00:00Z",
                "user_id": "test-user",
                "principal_type": "Team",
                "team_id": "team-1",
            }
        });
        std::fs::write(home.path().join("auth.json"), team_auth.to_string())
            .expect("write team auth.json");
        warm_startup_settings(SettingsQuery::from_auth(None));

        // The repair settles (personal principal, nothing pending): the
        // gate-exit warm may start the load, and bootstrap joins it.
        std::fs::write(home.path().join("auth.json"), personal_auth)
            .expect("restore personal auth.json");
        warm_startup_settings(SettingsQuery::from_auth(None));
        let wait = block_on_startup_settings(
            SettingsQuery::from_auth(None),
            Duration::from_secs(10),
            &tokio_util::sync::CancellationToken::new(),
        );
        let SettingsWait::Ready(outcome) = wait else {
            panic!("bootstrap join must observe the warmed startup load, got {wait:?}");
        };
        assert_eq!(
            outcome.settings().and_then(|s| s.tips.clone()),
            Some(vec!["from-server".to_string()]),
            "a repair-pending warm must not decide the startup cell as skipped"
        );
        assert_eq!(
            server.request_count_for("/v1/settings"),
            1,
            "the pending-repair warm must send nothing; the settled warm and \
             the bootstrap join must share one flight"
        );
    });
}
