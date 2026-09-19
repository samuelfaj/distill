// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

mod common;

use distill_shell::agent::remote_config::settings_get::{SettingsQuery, get_startup_settings};
use distill_shell::util::config::RemoteSettings;

#[test]
fn one_fetch_serves_startup_getter_and_bootstrap() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["from-server".into()]),
            ..RemoteSettings::default()
        });

        let first = get_startup_settings(SettingsQuery::from_auth(None)).await;
        let second = get_startup_settings(SettingsQuery::from_auth(None)).await;
        assert_eq!(
            first.settings().and_then(|s| s.tips.clone()),
            Some(vec!["from-server".to_string()]),
            "the startup getter must serve the early wait"
        );
        assert_eq!(
            second.settings().and_then(|s| s.tips.clone()),
            first.settings().and_then(|s| s.tips.clone()),
            "a second startup wait must join the startup load, not refetch"
        );

        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert_eq!(
            server.request_count_for("/v1/settings"),
            1,
            "startup getter and bootstrap together must spend one settings fetch"
        );
        assert_eq!(
            resolved
                .remote_settings
                .as_ref()
                .and_then(|s| s.tips.clone()),
            Some(vec!["from-server".to_string()]),
        );
    });
}
