// Modified for Distill by Samuel Fajreldines, 2026.
#[test]
fn startup_completed_carries_bootstrap_subphase_fields() {
    let home = tempfile::TempDir::new().expect("grok home");
    // SAFETY: this binary has one test; no other thread reads the environment.
    unsafe { distill_test_support::isolate_grok_env(home.path()) };
    distill_telemetry::unified_log::redirect_to_temp_for_tests();
    distill_telemetry::startup::mark_process_start();
    let _timer = distill_telemetry::startup::begin(distill_telemetry::startup::Owner::Client);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let mut cfg = distill_shell::agent::config::Config::default();
        cfg.remote_settings = Some(distill_shell::util::config::RemoteSettings::default());
        let auth_manager = std::sync::Arc::new(cfg.create_auth_manager());
        distill_shell::agent::init::bootstrap(&cfg, &auth_manager, None).expect("bootstrap");
        drop(distill_shell::managed_config::take_refresh_supervisor());
    });

    distill_telemetry::startup::PendingStartup::new()
        .finish(distill_telemetry::startup::StartupOutcome::Ok);

    let log =
        String::from_utf8(distill_telemetry::unified_log::snapshot_log().expect("unified log"))
            .expect("utf8");
    let ctx = log
        .lines()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value.get("msg")?.as_str()? != distill_telemetry::startup::STARTUP_COMPLETE_MSG {
                return None;
            }
            value.get("ctx").cloned()
        })
        .expect("startup complete record");

    for field in [
        "init_process_ms",
        "resolve_config_ms",
        "remote_settings_ms",
        "models_manager_ms",
    ] {
        assert!(
            ctx.get(field).and_then(serde_json::Value::as_u64).is_some(),
            "{field} must be populated on StartupCompleted, ctx={ctx}"
        );
    }
}
