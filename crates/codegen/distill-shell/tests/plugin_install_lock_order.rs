// Modified for Distill by Samuel Fajreldines, 2026.
//! Regression: `install_plugin` must release the registry flock before the post-install config
//! write — holding registry ⊃ config-init inverts the documented order and deadlocks writers.

use std::time::{Duration, Instant};

fn wait_for(deadline: Instant, mut check: impl FnMut() -> bool) -> bool {
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn install_releases_registry_lock_before_post_install_config_write() {
    // One #[test] per binary: the env is process-global (same rule as
    // acp_harness::run_agent_test).
    let distill_home = tempfile::tempdir().expect("grok home");
    // SAFETY: no other threads are running yet.
    unsafe { std::env::set_var("GROK_HOME", distill_home.path()) };

    let src = tempfile::tempdir().expect("plugin source");
    std::fs::write(
        src.path().join("plugin.json"),
        r#"{"name":"lock-order-demo"}"#,
    )
    .unwrap();

    // Hold the config-init flock, as a concurrent marketplace remove does.
    let init_flock =
        distill_shell::util::config::acquire_init_lock(distill_home.path()).expect("init flock");

    let source = src.path().display().to_string();
    let cwd = std::env::current_dir().unwrap();
    let installer =
        std::thread::spawn(move || distill_shell::plugin::install_plugin(&source, &cwd));

    // The registry save lands before the post-install config write starts.
    let install_dir = distill_home.path().join("installed-plugins");
    let registry_json = install_dir.join("registry.json");
    assert!(
        wait_for(Instant::now() + Duration::from_secs(10), || registry_json
            .exists()),
        "install never saved the registry"
    );

    // While the installer waits on the held config-init flock, the registry flock must be free —
    // the ABBA cross starves every other registry writer for the full timeout.
    let lock_path = install_dir.join("registry.lock");
    let registry_lock_freed = wait_for(Instant::now() + Duration::from_millis(500), || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open registry lock");
        fs2::FileExt::try_lock_exclusive(&file).is_ok()
    });
    assert!(
        registry_lock_freed,
        "registry flock still held during the post-install config write \
         (registry ⊃ config-init lock-order inversion)"
    );

    drop(init_flock);
    let outcome = installer
        .join()
        .expect("installer thread")
        .expect("install succeeds");
    assert_eq!(outcome.plugin_names, ["lock-order-demo"]);
    assert!(
        outcome.warnings.is_empty(),
        "auto-enable must succeed once the init flock is released: {:?}",
        outcome.warnings
    );
}
