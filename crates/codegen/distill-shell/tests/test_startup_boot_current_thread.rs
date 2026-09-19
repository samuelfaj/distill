// Modified for Distill by Samuel Fajreldines, 2026.
//! Its own binary: the grok home resolves once per process.

mod common;

use distill_shell::agent::config::Config;
use distill_shell::agent::init::{bootstrap_with_cancel, resolve_boot_startup_settings};
use distill_shell::util::config::RemoteSettings;
use distill_test_support::MockModelEntry;
use tokio_util::sync::CancellationToken;

/// The production current-thread boot path (`resolve_boot_startup_settings` then
/// `bootstrap_with_cancel`) the pager worker uses; every other suite drives the
/// multi-thread `bootstrap()`, so only this test covers the current-thread arm.
#[test]
fn current_thread_boot_installs_settings_and_fetched_catalog() {
    let home = common::isolated_home();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread runtime");
    runtime.block_on(async {
        let server = common::start_seeded_mock(home.path()).await;
        server.set_settings(RemoteSettings {
            tips: Some(vec!["current-thread-boot".into()]),
            ..RemoteSettings::default()
        });
        server.set_models(vec![MockModelEntry::new("mock-catalog-model")]);

        let mut cfg = Config::default();
        let auth_manager = std::sync::Arc::new(cfg.create_auth_manager());
        let cancel = CancellationToken::new();

        let boot = resolve_boot_startup_settings(&mut cfg, &cancel, true, auth_manager.current())
            .await
            .expect("boot settings resolve");

        // The prefetch also wrote the disk cache, which would let bootstrap's disk fallback mask an
        // ignored handoff. Remove it and change what a re-fetch returns, so the assertions fail
        // unless the boot consumes the owned in-memory prefetch.
        std::fs::remove_file(home.path().join("models_cache.json"))
            .expect("prefetch must have written models_cache.json to remove");
        server.set_models(vec![MockModelEntry::new("resync-should-not-appear")]);

        let (resolved, models_manager) =
            bootstrap_with_cancel(&cfg, &auth_manager, None, &cancel, Some(boot))
                .expect("current-thread bootstrap succeeds");
        drop(distill_shell::managed_config::take_refresh_supervisor());

        assert_eq!(
            resolved
                .remote_settings
                .as_ref()
                .and_then(|s| s.tips.clone()),
            Some(vec!["current-thread-boot".to_string()]),
            "the current-thread boot must install the settings it fetched",
        );
        let models = models_manager.models();
        assert!(
            models.contains_key("mock-catalog-model"),
            "the ModelsManager must carry the prefetched catalog, not bundled defaults",
        );
        assert!(
            !models.contains_key("resync-should-not-appear"),
            "the boot must consume the prefetched stash, not re-fetch after resolve",
        );
    });
}
