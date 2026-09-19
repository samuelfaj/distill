// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression: CLI `marketplace remove` of the official source must set
//! `official_marketplace_auto_installed` even for a JSON-store-only source with the flag unset.

#[test]
fn cli_remove_of_json_store_official_source_sets_sticky_flag() {
    // One #[test] per binary: the env is process-global.
    let distill_home = tempfile::tempdir().expect("grok home");
    // SAFETY: no other threads are running yet.
    unsafe { std::env::set_var("GROK_HOME", distill_home.path()) };

    // Official source known ONLY via the JSON store; the sticky flag is unset.
    let plugins_dir = distill_home.path().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    let known_path = plugins_dir.join("known_marketplaces.json");
    std::fs::write(
        &known_path,
        r#"{"xai": {"source": {"source": "github", "repo": "xai-org/plugin-marketplace"}}}"#,
    )
    .unwrap();

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(distill_pager::plugin_cmd::run(
            distill_pager::plugin_cmd::PluginArgs {
                command: distill_pager::plugin_cmd::PluginCommand::Marketplace(
                    distill_pager::plugin_cmd::MarketplaceArgs {
                        command: distill_pager::plugin_cmd::MarketplaceCommand::Remove {
                            source: "https://github.com/example/plugin-marketplace.git".into(),
                        },
                    },
                ),
            },
        ))
        .expect("remove succeeds");

    let known: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&known_path).unwrap()).unwrap();
    assert!(
        known.get("xai").is_none(),
        "source must be removed from the JSON store: {known}"
    );

    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(distill_home.path().join("config.toml")).unwrap())
            .expect("config.toml written");
    assert_eq!(
        config
            .get("marketplace")
            .and_then(|m| m.get("official_marketplace_auto_installed"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "removing the official source must set the sticky flag: {config}"
    );
}
