// Modified for Distill by Samuel Fajreldines, 2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::SessionCreatePrefetch;
use crate::agent::folder_trust::{self, TrustScan};
use distill_test_support::EnvGuard;

#[tokio::test]
async fn prefetch_runs_concurrently_and_results_are_ready_at_join() {
    let empty = tempfile::tempdir().unwrap();
    let with_config = tempfile::tempdir().unwrap();
    std::fs::write(with_config.path().join(".mcp.json"), "{}\n").unwrap();

    for cwd in [empty.path(), with_config.path()] {
        let (allowed, scan) = folder_trust::gather_and_record(cwd, None, false);
        let reused = folder_trust::resolve_and_record_from_scan(cwd, None, false, scan);
        assert_eq!(allowed, reused);
    }

    let plugin_entered = Arc::new(AtomicBool::new(false));
    let release_plugins = Arc::new(tokio::sync::Notify::new());
    let entered = plugin_entered.clone();
    let release = release_plugins.clone();
    let mut prefetch = SessionCreatePrefetch::launch_for_test(TrustScan::skipped(), async move {
        entered.store(true, Ordering::SeqCst);
        release.notified().await;
        Some(Arc::new(distill_agent::plugins::PluginRegistry::empty()))
    });

    let persistence = async {
        while !plugin_entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    persistence.await;

    release_plugins.notify_waiters();
    let registry = prefetch.join_plugin_registry().await;
    assert!(registry.is_some());
}

#[tokio::test]
#[serial_test::serial]
async fn plugin_refresh_reads_reconciled_trust_and_does_not_start_before_it() {
    let _sim = EnvGuard::set(distill_version::TEST_VERSION_ENV, "0.0.0-sim");
    let _flag = EnvGuard::unset("GROK_FOLDER_TRUST");
    super::REFRESH_STARTS.store(0, Ordering::SeqCst);
    super::REFRESH_VERDICTS.lock().unwrap().clear();
    super::CAPTURE_TRUST_ONLY.store(true, Ordering::SeqCst);

    let cwd = tempfile::tempdir().unwrap();
    git2::Repository::init(cwd.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let _home = EnvGuard::set("GROK_HOME", home.path());

    let (_allowed, scan) = folder_trust::gather_and_record(cwd.path(), None, false);
    let prefetch = SessionCreatePrefetch::launch_from_meta(
        cwd.path(),
        scan,
        distill_agent::plugins::SharedPluginRegistryHandle::new(None, Vec::new()),
        None,
    );
    assert!(
        !prefetch.refresh_started(),
        "refresh must not launch before trust is reconciled"
    );
    drop(prefetch);
    assert_eq!(
        super::REFRESH_STARTS.load(Ordering::SeqCst),
        0,
        "dropping an unreconciled prefetch must not launch refresh"
    );

    let (_allowed, scan) = folder_trust::gather_and_record(cwd.path(), None, false);
    let mut prefetch = SessionCreatePrefetch::launch_from_meta(
        cwd.path(),
        scan,
        distill_agent::plugins::SharedPluginRegistryHandle::new(None, Vec::new()),
        None,
    );
    std::fs::write(cwd.path().join(".mcp.json"), "{}\n").unwrap();
    let verdict = prefetch.resolve_trust(cwd.path(), None);
    assert!(
        !verdict,
        "mid-create config must flip the provisional allow"
    );
    let registry = prefetch.join_plugin_registry().await;
    assert!(registry.is_some());
    assert_eq!(
        super::REFRESH_VERDICTS.lock().unwrap().as_slice(),
        &[false],
        "plugin refresh must run with the reconciled verdict"
    );

    super::CAPTURE_TRUST_ONLY.store(false, Ordering::SeqCst);
}
