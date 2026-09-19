// Modified for Distill by Samuel Fajreldines, 2026.
//! Integration tests using the public API only.
//!
//! OS-event delivery tests were removed (flaky by construction). Registry
//! sharing remains: it locks watcher identity and stats, not event timing.

use serial_test::serial;
use tempfile::TempDir;
use distill_fsnotify::FsConfig;

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn shared_dedupes_by_directory() {
    use std::sync::Arc;

    let temp = TempDir::new().unwrap();
    let path = temp.path().to_path_buf();

    // First call creates the watcher; subsequent calls for the same canonical
    // directory hand back clones of the *same* source rather than opening a
    // new OS watch. Skip gracefully where the OS denies watches (CI limits).
    let Ok(a) = distill_fsnotify::shared(path.clone(), FsConfig::default()) else {
        eprintln!("skipping: OS watcher unavailable (resource limit?)");
        return;
    };
    let before = distill_fsnotify::stats();
    let b = distill_fsnotify::shared(path.clone(), FsConfig::default()).unwrap();
    assert!(Arc::ptr_eq(&a, &b), "same dir must share one watcher");
    assert_eq!(
        Arc::strong_count(&a),
        2,
        "second shared() must clone the existing source, not create a new one"
    );

    // The reuse must be counted as a cache hit (no new OS watcher created).
    let after = distill_fsnotify::stats();
    assert_eq!(
        after.reused_total - before.reused_total,
        1,
        "reuse must increment reused_total"
    );
    assert_eq!(
        after.created_total, before.created_total,
        "reuse must not create a new watcher"
    );
    assert!(after.live_watchers >= 1, "the shared watcher must be live");

    // A different directory gets its own independent watcher (a real miss).
    let other = TempDir::new().unwrap();
    let c = distill_fsnotify::shared(other.path().to_path_buf(), FsConfig::default()).unwrap();
    assert!(!Arc::ptr_eq(&a, &c), "different dirs must not share");
    assert_eq!(
        distill_fsnotify::stats().created_total - after.created_total,
        1,
        "a new directory must create a new watcher"
    );

    // Once the last sharer drops, the registry entry is reclaimed and a later
    // request rebuilds a fresh source (exercises the recreate-after-drop path).
    drop(a);
    drop(b);
    let d = distill_fsnotify::shared(path, FsConfig::default()).unwrap();
    assert_eq!(
        Arc::strong_count(&d),
        1,
        "after all sharers drop, shared() must build a new source"
    );
}
