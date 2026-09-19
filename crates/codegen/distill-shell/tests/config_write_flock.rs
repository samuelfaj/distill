// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression: settings saves must serialize against config-init-flock writers, or the two
//! domains interleave read-modify-writes and the last atomic rename drops the other side's edit.

use std::time::Duration;

#[test]
fn settings_save_serializes_against_init_flock_writer() {
    // One #[test] per binary: the env is process-global (same rule as
    // acp_harness::run_agent_test).
    let distill_home = tempfile::tempdir().expect("grok home");
    // SAFETY: no other threads are running yet.
    unsafe { std::env::set_var("GROK_HOME", distill_home.path()) };

    let config_path = distill_home.path().join("config.toml");
    std::fs::write(&config_path, "[cli]\n").unwrap();

    // Flock-holding writer mid read-modify-write (the shape of
    // session-start auto-enable's `add_enabled_plugin`).
    let flock =
        distill_shell::util::config::acquire_init_lock(distill_home.path()).expect("init flock");
    let stale_read = std::fs::read_to_string(&config_path).unwrap();

    // Concurrent settings toggle on its own runtime thread.
    let saver = std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(distill_shell::util::config::update_config(|cfg| {
                cfg.ui.simple_mode = Some(true);
            }))
    });

    // Give the save time to land (pre-fix) or block on the flock (post-fix),
    // then finish the flock writer's read-modify-write and release.
    std::thread::sleep(Duration::from_millis(400));
    let mut modified: toml::Value = toml::from_str(&stale_read).unwrap();
    modified.as_table_mut().unwrap().insert(
        "plugins".into(),
        toml::from_str("enabled = [\"demo\"]").unwrap(),
    );
    distill_shell::util::config::atomic_write_string(
        &config_path,
        &toml::to_string_pretty(&modified).unwrap(),
    )
    .unwrap();
    drop(flock);

    saver
        .join()
        .expect("saver thread")
        .expect("settings save succeeds");

    let merged: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        merged
            .get("plugins")
            .and_then(|p| p.get("enabled"))
            .and_then(|e| e.as_array())
            .map(|a| a.len()),
        Some(1),
        "flock writer's plugin enable must survive: {merged}"
    );
    assert_eq!(
        merged
            .get("ui")
            .and_then(|u| u.get("simple_mode"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "settings save must survive the flock writer's rename: {merged}"
    );
}
