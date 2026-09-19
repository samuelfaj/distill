//! Its own binary: the grok home resolves once per process.

mod common;

#[test]
fn bootstrap_skips_settings_when_remote_fetch_is_disabled() {
    let home = common::isolated_home();
    common::block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        std::fs::write(
            home.path().join("requirements.toml"),
            "[features]\nremote_fetch = false\n",
        )
        .expect("write requirements.toml");
        let resolved = common::run_bootstrap()
            .await
            .expect("bootstrap succeeds for a personal profile");

        assert!(
            resolved.remote_settings.is_none(),
            "remote_fetch disabled must not install remote settings"
        );
        assert_eq!(
            server.request_count_for("/v1/settings"),
            0,
            "the getter must honor remote_fetch disabled"
        );
    });
}
