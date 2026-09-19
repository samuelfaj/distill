// Modified for Distill by Samuel Fajreldines, 2026.
//! `GROK_HOME` override tests in an isolated binary so `distill_home()`'s process-wide `OnceLock` initializes from the overridden env var.

use std::path::PathBuf;

#[test]
#[serial_test::serial(GROK_HOME)]
fn distill_home_override_path_helpers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let distill_home = tmp.path().to_path_buf();
    unsafe {
        std::env::set_var("GROK_HOME", &distill_home);
    }

    assert_eq!(
        distill_pager::util::pager_toml_path(),
        distill_home.join("pager.toml")
    );
    assert_eq!(
        distill_pager::util::display_distill_home_prefix(),
        "$GROK_HOME"
    );
    assert_eq!(
        distill_pager::util::display_user_grok_path("config.toml"),
        "$GROK_HOME/config.toml"
    );

    let memory_path = distill_home.join("memory/MEMORY.md");
    assert_eq!(
        distill_pager::util::abbreviate_path(&memory_path.display().to_string()),
        "$GROK_HOME/memory/MEMORY.md"
    );

    // The copy toast abbreviates paths the same way, so a custom $GROK_HOME outside $HOME still shows the short form
    assert_eq!(
        distill_pager::clipboard::display_copy_path(&distill_home.join("last-copy.txt")),
        "$GROK_HOME/last-copy.txt"
    );

    assert!(distill_pager::util::is_under_user_distill_home(
        &memory_path
    ));
    assert!(!distill_pager::util::is_under_user_distill_home(
        PathBuf::from("/tmp/other").as_path()
    ));
}

/// Isolated because `distill_home()`'s `OnceLock` is already initialized by the time the shared lib-test binary reaches a case like this.
#[test]
#[serial_test::serial(GROK_HOME)]
fn disk_usage_run_creates_no_distill_home() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ghost = tmp.path().join("ghost-home");
    unsafe {
        std::env::set_var("GROK_HOME", &ghost);
    }

    for json in [false, true] {
        distill_pager::disk_usage_cmd::run(distill_pager::disk_usage_cmd::DiskUsageArgs { json })
            .expect("a missing home is not an error");
        assert!(
            !ghost.exists(),
            "grok du must not create the home it reports on (json={json})"
        );
    }
}
