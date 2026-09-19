// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

mod common;

use distill_shell::util::config::RemoteSettings;

#[test]
fn bootstrap_without_prefetch_fetches_each_endpoint_once() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["serial-fallback".into()]),
            ..RemoteSettings::default()
        });

        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert_eq!(
            (
                server.request_count_for("/v1/models"),
                server.request_count_for("/v1/settings"),
            ),
            (1, 1),
            "bootstrap with no prefetch must fetch each startup endpoint exactly once"
        );
        assert_eq!(
            resolved
                .remote_settings
                .as_ref()
                .and_then(|s| s.tips.clone()),
            Some(vec!["serial-fallback".to_string()]),
            "the serial fallback's settings must reach the resolved config"
        );
    });
}
