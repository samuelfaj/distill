use chrono::{Duration as ChronoDuration, Utc};

use super::super::{
    CACHE_TTL, CacheAuthMethod, Commit, MODELS_CACHE_FILE, ModelsCache, ModelsCacheScope,
    ModelsFetchFuture, SettingsCacheManager, allowlist_denied_message, build_prefetched_map,
    degraded_log_level, evaluate_models_commit, resolve_prefetch_inputs_from_parts,
    resolve_startup_endpoints, selectable_catalog_key_for_persisted,
};
use super::*;

fn test_manager() -> ModelsManager {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .cache(test_cache_manager(tmp.path()))
    .build()
}

/// The harness ships in auto effort: the decision layer chooses the level for
/// each micro-action, and only an explicit `/effort <level>` (or the config key)
/// takes that back.
#[test]
fn a_fresh_manager_starts_in_auto_effort() {
    assert!(
        test_manager().current_effort_auto(),
        "an unset `[jev] effort_auto` means on"
    );

    let mut cfg = config::Config::default();
    cfg.jev.effort_auto = Some(false);
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let pinned = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    )
    .cache(test_cache_manager(tmp.path()))
    .build();
    assert!(
        !pinned.current_effort_auto(),
        "the owner can pin one level for the session"
    );
}

/// Cold manager (no prefetch, isolated cache and auth) over `endpoint`.
fn cold_manager(cfg: config::Config, endpoint: Arc<dyn ModelsEndpoint>) -> ModelsManager {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    )
    .endpoint(endpoint)
    .cache(test_cache_manager(tmp.path()))
    .build()
}

struct HangingEndpoint;
impl ModelsEndpoint for HangingEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        Box::pin(std::future::pending())
    }
}

struct FailingEndpoint;
impl ModelsEndpoint for FailingEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        Box::pin(async { None })
    }
}

struct CountingEndpoint {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}
impl ModelsEndpoint for CountingEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { None })
    }
}

struct SlowEndpoint {
    catalog: IndexMap<String, ModelEntry>,
    delay: std::time::Duration,
}
impl ModelsEndpoint for SlowEndpoint {
    fn fetch_models(
        &self,
        _endpoints: config::EndpointsConfig,
        _auth: Option<GrokAuth>,
        _fetch_auth: ModelFetchAuth,
    ) -> ModelsFetchFuture {
        let catalog = self.catalog.clone();
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Some(catalog)
        })
    }
}

#[tokio::test]
async fn catalog_retry_recovers_after_endpoint_returns() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoveringEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for RecoveringEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let out = if n == 0 {
                None
            } else {
                Some(self.catalog.clone())
            };
            Box::pin(async move { out })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(RecoveringEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    mgr.spawn_catalog_retry_with_backoff(
        /*remote_fetch_enabled*/ true,
        crate::tools::retry::BackoffConfig::new(5, 1, 10),
    );

    let mut recovered = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        recovered,
        "catalog retry did not recover after the endpoint returned"
    );
    assert!(mgr.models().contains_key("grok-4"));
    assert!(
        calls.load(Ordering::SeqCst) >= 2,
        "expected a failed attempt then a success",
    );
}

#[tokio::test]
#[serial]
async fn disk_cache_reload_applies_without_fetching() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(CountingEndpoint {
        calls: calls.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    let seeder = test_cache_manager(tmp.path());
    seeder.persist(
        &make_prefetched(&["grok-4.5"]),
        Some("etag-x"),
        &mgr.cache_scope(),
        Utc::now(),
    );

    mgr.reload_from_disk_cache();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the disk cache load must never hit the transport",
    );
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "first real catalog from the disk cache must resolve the configured default",
    );
}

#[tokio::test]
async fn auth_refresh_watcher_refetches_on_notify() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct NotifyEndpoint {
        calls: Arc<AtomicUsize>,
        catalog: IndexMap<String, ModelEntry>,
    }
    impl ModelsEndpoint for NotifyEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let catalog = self.catalog.clone();
            Box::pin(async move { Some(catalog) })
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(NotifyEndpoint {
        calls: calls.clone(),
        catalog: make_prefetched(&["grok-4"]),
    }))
    .build();
    assert!(!mgr.has_fetched_real_catalog());

    let notify = Arc::new(tokio::sync::Notify::new());
    mgr.start_auth_refresh_watcher(notify.clone());
    notify.notify_one();

    let mut updated = false;
    for _ in 0..200 {
        if mgr.has_fetched_real_catalog() {
            updated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(updated, "watcher did not re-fetch the catalog on notify");
    assert!(mgr.models().contains_key("grok-4"));
    assert!(calls.load(Ordering::SeqCst) >= 1);
}

#[tokio::test(start_paused = true)]
async fn hanging_fetch_does_not_block_refresh() {
    let mgr = cold_manager(config::Config::default(), Arc::new(HangingEndpoint));

    tokio::time::timeout(
        crate::http::STARTUP_FETCH_TIMEOUT * 10,
        mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true),
    )
    .await
    .expect("fetch_and_apply_inner must return despite a hanging endpoint");

    assert!(
        !mgr.has_fetched_real_catalog(),
        "a timed-out fetch must not mark a real catalog",
    );
}

#[tokio::test(start_paused = true)]
async fn slow_fetch_within_timeout_still_applies() {
    let mgr = cold_manager(
        config::Config::default(),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );

    mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true)
        .await;
    assert!(
        mgr.has_fetched_real_catalog(),
        "a fetch within the timeout must apply, not degrade",
    );
    assert!(mgr.models().contains_key("grok-4"));
}

#[tokio::test(start_paused = true)]
async fn etag_refresh_is_bounded_and_single_flighted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHangEndpoint {
        calls: Arc<AtomicUsize>,
    }
    impl ModelsEndpoint for CountingHangEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        config::Config::default(),
    )
    .endpoint(Arc::new(CountingHangEndpoint {
        calls: calls.clone(),
    }))
    .build();

    mgr.spawn_fetch_inner(Some("etag-1".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    mgr.spawn_fetch_inner(Some("etag-2".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "single-flight: only one etag fetch in flight at a time",
    );

    // Advance past the bound so the hung fetch is abandoned and the guard clears.
    tokio::time::sleep(crate::http::STARTUP_FETCH_TIMEOUT * 2).await;
    tokio::task::yield_now().await;

    mgr.spawn_fetch_inner(Some("etag-3".into()), /*remote_fetch_enabled*/ true);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "after the timeout cleared the in-flight guard, a new etag fetch proceeds",
    );

    mgr.spawn_fetch_inner(Some("etag-4".into()), /*remote_fetch_enabled*/ false);
    tokio::task::yield_now().await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "disabled gate must not fetch"
    );
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_unblocks_on_fetch_and_skips_dead_dwell() {
    // Deployment auth: a fetch can succeed without a session, so the wait dwells regardless of any API key in the environment
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );

    // Cold cache, remote fetch disabled: no fetch is coming, so no dwell.
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ false)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);

    // Cold cache, no attempt spawned: nothing to wait for, so no dwell.
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);

    // Cold cache, fetch in flight: the wait unblocks when the fetch lands.
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await,
        "the wait must observe the completed fetch",
    );
    assert!(mgr.models().contains_key("grok-4"));

    // Warm: an already-loaded catalog returns immediately.
    let start = tokio::time::Instant::now();
    assert!(
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_unblocks_on_failed_fetch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(FailingEndpoint),
    );
    let budget = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT + crate::http::STARTUP_FETCH_TIMEOUT;
    let start = tokio::time::Instant::now();
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert!(start.elapsed() < budget, "failure must beat the budget");
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_is_bounded() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(HangingEndpoint),
    );
    let budget = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT + crate::http::STARTUP_FETCH_TIMEOUT;
    let _attempt = FetchAttemptGuard::begin(&mgr.inner);
    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), budget, "only the budget ends this wait");
}

#[tokio::test(start_paused = true)]
#[serial]
async fn first_catalog_wait_skips_doomed_signed_out_fetch() {
    let _no_key = EnvGuard::unset("XAI_API_KEY");
    let _no_legacy_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let mgr = cold_manager(config::Config::default(), Arc::new(HangingEndpoint));
    let start = tokio::time::Instant::now();
    mgr.spawn_fetch_inner(None, /*remote_fetch_enabled*/ true);
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn first_catalog_wait_observes_inline_fetch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(SlowEndpoint {
            catalog: make_prefetched(&["grok-4"]),
            delay: crate::http::STARTUP_FETCH_TIMEOUT / 2,
        }),
    );
    // Fetch first in the join, so its attempt registers on first poll.
    let ((), ready) = tokio::join!(
        mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true),
        mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true),
    );
    assert!(ready, "the wait must observe the inline fetch's outcome");
}

#[tokio::test(start_paused = true)]
async fn new_fetch_attempt_supersedes_failed_latch() {
    let mgr = cold_manager(
        config_from_toml("[endpoints]\ndeployment_key = \"deploy-key\""),
        Arc::new(FailingEndpoint),
    );
    mgr.fetch_and_apply_inner(/*remote_fetch_enabled*/ true)
        .await;
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed
    );

    let attempt = FetchAttemptGuard::begin(&mgr.inner);
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Pending,
        "a new attempt must supersede the stale failure",
    );
    drop(attempt);
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed,
        "the last attempt out without an outcome must latch",
    );

    let start = tokio::time::Instant::now();
    assert!(
        !mgr.wait_for_first_catalog_inner(/*remote_fetch_enabled*/ true)
            .await
    );
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
}

#[test]
fn stale_fetch_result_is_discarded_after_identity_change() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let stale_generation = mgr.inner.catalog.read().generation;
    mgr.clear();

    assert!(!mgr.apply_refresh_result_fenced(
        &cfg,
        Some(make_prefetched(&["stale-model"])),
        None,
        stale_generation,
    ));
    assert!(!mgr.models().contains_key("stale-model"));
    assert!(!mgr.has_fetched_real_catalog());

    assert!(!mgr.apply_refresh_result_fenced(&cfg, None, None, stale_generation));
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Pending,
        "a stale failure must not latch",
    );

    assert!(mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["new-model"])), None));
    assert!(mgr.models().contains_key("new-model"));
}

fn config_from_toml(toml: &str) -> config::Config {
    config::Config::new_from_toml_cfg(&toml::from_str(toml).unwrap()).unwrap()
}

#[test]
fn model_show_model_fingerprint_reads_catalog_flag() {
    let mgr = test_manager();

    let mut flagged = make_model_entry("fp-model");
    flagged.info.show_model_fingerprint = true;
    mgr.insert_test_entry("fp-model", flagged);

    mgr.insert_test_entry("plain-model", make_model_entry("plain-model"));

    let mut custom = make_model_entry("enterprise-slug");
    custom.info.show_model_fingerprint = true;
    mgr.insert_test_entry("enterprise-key", custom);

    assert!(mgr.model_show_model_fingerprint("fp-model"));
    assert!(!mgr.model_show_model_fingerprint("plain-model"));
    assert!(!mgr.model_show_model_fingerprint("missing-model"));
    assert!(
        mgr.model_show_model_fingerprint("enterprise-slug"),
        "slug lookup must resolve to the catalog key and read the flag",
    );
    assert!(mgr.model_show_model_fingerprint("enterprise-key"));
}

#[test]
fn reasoning_effort_helpers_resolve_wire_name_to_catalog_key() {
    let mgr = test_manager();

    let mut custom = make_model_entry("enterprise-slug");
    custom.info.supports_reasoning_effort = true;
    custom.info.reasoning_effort = Some(ReasoningEffort::High);
    custom.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    mgr.insert_test_entry("enterprise-key", custom);

    for id in ["enterprise-key", "enterprise-slug"] {
        assert!(mgr.model_supports_reasoning_effort(id));
        assert_eq!(
            mgr.model_default_reasoning_effort(id),
            Some(ReasoningEffort::High)
        );
        assert_eq!(mgr.model_reasoning_efforts(id).len(), 1);
    }

    assert!(!mgr.model_supports_reasoning_effort("missing-model"));
    assert_eq!(mgr.model_default_reasoning_effort("missing-model"), None);
    assert!(mgr.model_reasoning_efforts("missing-model").is_empty());
}

#[test]
fn default_model_honors_allowlist_when_no_default_set() {
    let cfg = config_from_toml(
        r#"
            [models]
            allowed_models = ["keep-*"]
            [model.zzz-first]
            model = "zzz-first"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.keep-one]
            model = "keep-one"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&cfg, None);
    let (_key, entry, _src) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        entry.info.user_selectable,
        "picked non-selectable {}",
        entry.model
    );
}

#[test]
fn validate_selectable_rejects_bad_allowlists() {
    let excluded = config_from_toml(
        r#"
            [models]
            default = "grok-3"
            allowed_models = ["grok-4*"]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&excluded, None);
    assert!(
        validate_selectable(&excluded, &catalog)
            .unwrap_err()
            .contains("grok-3")
    );

    let zero = config_from_toml(
        r#"
            [models]
            allowed_models = ["nomatch-*"]
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    );
    let catalog = resolve_model_catalog(&zero, None);
    assert!(validate_selectable(&zero, &catalog).is_err());
}

#[test]
fn from_config_defers_validate_without_prefetch() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.requirements.allowed_models.pin(
        crate::agent::config::AllowlistPin::List(vec!["nomatch-*".into()]),
        crate::config::RequirementSource::Unknown,
    );

    let mgr = ModelsManager::from_config(&cfg, None, auth_manager)
        .expect("cold start must not validate against built-ins-only");
    assert!(
        !mgr.has_fetched_real_catalog(),
        "no prefetch means the first-fetch gate has not run"
    );
    assert!(
        mgr.allowlist_excludes_all(),
        "from_config must latch the prompt-path guard on a builtins-only miss"
    );
}

#[test]
fn set_session_model_fleet_deny_uses_organization_message() {
    let raw: toml::Value = toml::from_str(
        r#"
            [models]
            [model.grok-3]
            model = "grok-3"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            [model.grok-4]
            model = "grok-4"
            base_url = "https://api.x.ai/v1"
            context_window = 256000
            "#,
    )
    .unwrap();
    let mut cfg = config::Config::new_from_toml_cfg(&raw).unwrap();
    cfg.requirements.allowed_models.pin(
        crate::agent::config::AllowlistPin::List(vec!["grok-4".into()]),
        crate::config::RequirementSource::Unknown,
    );
    let catalog = resolve_model_catalog(&cfg, None);
    let Some(grok3) = catalog.get("grok-3") else {
        panic!("expected grok-3: {catalog:?}");
    };
    assert!(!grok3.info.user_selectable);
    let msg = allowlist_denied_message(&cfg);
    assert!(
        msg.contains("organization"),
        "set_session_model /model gate must use the fleet deny string: {msg}"
    );
    assert!(!msg.contains("allowed_models"));
}

#[tokio::test]
async fn refresh_if_new_etag_skips_when_same() {
    let mgr = test_manager();
    mgr.inner.catalog.write().etag = Some("\"abc123\"".to_string());

    mgr.refresh_if_new_etag("\"abc123\"".to_string()).await;
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"abc123\""),
        "etag should remain unchanged when same"
    );
}

#[tokio::test]
async fn set_current_model_id_change_fires_watch_to_all_subscribers() {
    let mgr = test_manager();
    let mut rx_a = mgr.subscribe_model_switch();
    let mut rx_b = mgr.subscribe_model_switch();
    let initial_a = *rx_a.borrow_and_update();
    let initial_b = *rx_b.borrow_and_update();
    assert_eq!(initial_a, initial_b);

    mgr.set_current_model_id(acp::ModelId::new("default"));
    let same_id_ticked = tokio::time::timeout(std::time::Duration::from_millis(25), rx_a.changed())
        .await
        .is_ok();
    assert!(
        !same_id_ticked,
        "set_current_model_id(same id) must NOT bump the watch generation",
    );

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.changed())
        .await
        .expect("rx_a saw the switch")
        .expect("watch channel still open");
    tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.changed())
        .await
        .expect("rx_b saw the switch")
        .expect("watch channel still open");
    assert_ne!(*rx_a.borrow(), initial_a);
    assert_eq!(*rx_a.borrow(), *rx_b.borrow());
    assert!(mgr.model_switch_generation() > initial_a);
}

#[tokio::test]
async fn model_switch_generation_snapshot_reflects_current_state() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    assert_eq!(mgr.model_switch_generation(), start + 1);
    mgr.set_current_model_id(acp::ModelId::new("grok-3"));
    assert_eq!(mgr.model_switch_generation(), start + 2);
}

#[test]
fn first_catalog_reselect_bumps_model_switch_watch() {
    let mgr = test_manager();
    let start = mgr.model_switch_generation();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4.5");
    assert!(
        mgr.model_switch_generation() > start,
        "background reselection must fire the model-switch watch",
    );
}

#[test]
fn reselect_missing_current_model_bumps_watch() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4", "grok-3"])), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let start = mgr.model_switch_generation();
    // A later catalog drops the current model, so reselect_current_model_if_missing runs
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-3"])), None);
    assert_ne!(mgr.current_model_id().0.as_ref(), "grok-4");
    assert!(
        mgr.model_switch_generation() > start,
        "reselecting away from a removed current model must fire the watch",
    );
}

#[test]
fn rebuild_updates_models_and_available() {
    let mgr = test_manager();
    assert!(mgr.models().is_empty());
    assert!(mgr.available().is_empty());

    let cfg = config::Config::default();
    mgr.rebuild(&cfg, Some(make_prefetched(&["test-model"])));

    assert!(
        !mgr.models().is_empty(),
        "models should be populated after rebuild"
    );
}

#[test]
fn current_reasoning_effort_round_trip() {
    let mgr = test_manager();
    assert_eq!(mgr.current_reasoning_effort(), None);

    mgr.set_current_reasoning_effort(Some(ReasoningEffort::High));
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::High));

    mgr.set_current_reasoning_effort(None);
    assert_eq!(mgr.current_reasoning_effort(), None);
}

#[test]
fn current_reasoning_effort_seeded_from_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mut cfg = config::Config::default();
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::Xhigh);
    let mgr = ModelsManager::new(
        None,
        IndexMap::new(),
        acp::ModelId::new("default"),
        auth_manager,
        cfg,
    );
    assert_eq!(mgr.current_reasoning_effort(), Some(ReasoningEffort::Xhigh),);
}

#[test]
fn default_reasoning_effort_only_stamps_supporting_model() {
    use indexmap::IndexMap;

    let mut cfg = config::Config::default();
    cfg.models.default = Some("reasoning-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = make_model_entry("reasoning-model");
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    let Some(reasoning) = catalog.get("reasoning-model") else {
        panic!("expected reasoning-model: {catalog:?}");
    };
    assert_eq!(
        reasoning.info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting default model should be stamped",
    );

    let mut cfg = config::Config::default();
    cfg.models.default = Some("plain-model".to_string());
    cfg.models.default_reasoning_effort = Some(ReasoningEffort::High);

    let mut prefetched = IndexMap::new();
    prefetched.insert("plain-model".to_string(), make_model_entry("plain-model"));

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    let Some(plain) = catalog.get("plain-model") else {
        panic!("expected plain-model: {catalog:?}");
    };
    assert_eq!(
        plain.info.reasoning_effort, None,
        "non-reasoning default model must NOT be stamped with persisted effort",
    );
}

#[test]
fn reasoning_effort_override_skips_models_that_do_not_offer_level() {
    use indexmap::IndexMap;
    use xai_grok_sampling_types::ReasoningEffortOption;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::None),
        ..Default::default()
    };

    let mut prefetched = IndexMap::new();
    let mut no_none = make_model_entry("grok-4.5");
    no_none.info.supports_reasoning_effort = true;
    no_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "high".into(),
        value: ReasoningEffort::High,
        label: "High".into(),
        description: None,
        default: true,
    }];
    no_none.info.reasoning_effort = Some(ReasoningEffort::High);
    prefetched.insert("grok-4.5".to_string(), no_none);

    let mut with_none = make_model_entry("legacy-none");
    with_none.info.supports_reasoning_effort = true;
    with_none.info.reasoning_efforts = vec![ReasoningEffortOption {
        id: "none".into(),
        value: ReasoningEffort::None,
        label: "None".into(),
        description: None,
        default: true,
    }];
    prefetched.insert("legacy-none".to_string(), with_none);

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    let Some(grok45) = catalog.get("grok-4.5") else {
        panic!("expected grok-4.5: {catalog:?}");
    };
    assert_eq!(
        grok45.info.reasoning_effort,
        Some(ReasoningEffort::High),
        "--effort none must not stamp onto models that do not offer none"
    );
    let Some(legacy) = catalog.get("legacy-none") else {
        panic!("expected legacy-none: {catalog:?}");
    };
    assert_eq!(
        legacy.info.reasoning_effort,
        Some(ReasoningEffort::None),
        "models that list none should still accept the override"
    );
}

#[test]
fn config_menu_only_model_derives_support_and_default() {
    let mut cfg = config::Config::default();
    cfg.config_models.insert(
        "menu-only".to_string(),
        config::ConfigModelOverride {
            reasoning_efforts: vec![
                ReasoningEffortOption {
                    id: "balanced".to_string(),
                    value: ReasoningEffort::Medium,
                    label: "Balanced".to_string(),
                    description: None,
                    default: false,
                },
                ReasoningEffortOption {
                    id: "deep".to_string(),
                    value: ReasoningEffort::Xhigh,
                    label: "Deep".to_string(),
                    description: None,
                    default: true,
                },
            ],
            ..Default::default()
        },
    );
    cfg.config_models
        .insert("plain".to_string(), config::ConfigModelOverride::default());

    let catalog = resolve_model_catalog(&cfg, None);
    let Some(menu_only) = catalog.get("menu-only") else {
        panic!("expected menu-only: {catalog:?}");
    };
    let info = &menu_only.info;
    assert!(
        info.supports_reasoning_effort,
        "menu-only model must derive support"
    );
    assert_eq!(
        info.reasoning_effort,
        Some(ReasoningEffort::Xhigh),
        "derived default = marked-default option value"
    );
    let Some(plain) = catalog.get("plain") else {
        panic!("expected plain: {catalog:?}");
    };
    assert!(!plain.info.supports_reasoning_effort);
    assert_eq!(plain.info.reasoning_effort, None);

    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManager::new(
        None,
        catalog,
        acp::ModelId::new("menu-only"),
        auth_manager,
        cfg,
    );
    assert!(mgr.model_supports_reasoning_effort("menu-only"));
    assert_eq!(
        mgr.model_default_reasoning_effort("menu-only"),
        Some(ReasoningEffort::Xhigh)
    );
    assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
    assert!(!mgr.model_supports_reasoning_effort("plain"));
    assert_eq!(mgr.model_default_reasoning_effort("plain"), None);

    mgr.set_current_model_id(acp::ModelId::new("plain"));
    assert_eq!(mgr.current_model_id().0.as_ref(), "plain");
    assert_eq!(mgr.model_reasoning_efforts("menu-only").len(), 2);
    assert!(mgr.model_reasoning_efforts("plain").is_empty());
}

#[test]
fn cli_reasoning_effort_override_only_stamps_supporting_models() {
    use indexmap::IndexMap;

    let cfg = config::Config {
        reasoning_effort_override: Some(ReasoningEffort::High),
        ..config::Config::default()
    };

    let mut prefetched = IndexMap::new();
    let mut reasoning_entry = make_model_entry("reasoning-model");
    reasoning_entry.info.supports_reasoning_effort = true;
    prefetched.insert("reasoning-model".to_string(), reasoning_entry);

    prefetched.insert("plain-model".to_string(), make_model_entry("plain-model"));

    let catalog = resolve_model_catalog(&cfg, Some(prefetched));
    let Some(reasoning) = catalog.get("reasoning-model") else {
        panic!("expected reasoning-model: {catalog:?}");
    };
    assert_eq!(
        reasoning.info.reasoning_effort,
        Some(ReasoningEffort::High),
        "reasoning-supporting model should be stamped",
    );
    let Some(plain) = catalog.get("plain-model") else {
        panic!("expected plain-model: {catalog:?}");
    };
    assert_eq!(
        plain.info.reasoning_effort, None,
        "non-reasoning model must NOT be stamped",
    );
}

#[test]
fn apply_refresh_result_only_updates_etag_on_success() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    mgr.inner.catalog.write().etag = Some("\"old\"".to_string());

    assert!(
        !mgr.apply_refresh_result(&cfg, None, Some("\"new\"".to_string())),
        "failed refresh should report no update"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("\"old\""),
        "etag should remain unchanged when refresh fails"
    );
    assert!(
        mgr.prefetched().is_none(),
        "prefetched models should stay unchanged"
    );
    assert!(
        !mgr.has_fetched_real_catalog(),
        "failed refresh must not flip has_fetched_real_catalog"
    );
}

fn make_model_entry(model_id: &str) -> ModelEntry {
    ModelEntry {
        info: config::ModelInfo::fallback(model_id),
        mtls_cert_dir: None,
        api_key: None,
        env_key: None,
        auth_provider: None,
        api_base_url: None,
    }
}

fn make_prefetched(ids: &[&str]) -> IndexMap<String, ModelEntry> {
    ids.iter()
        .map(|id| (id.to_string(), make_model_entry(id)))
        .collect()
}

#[test]
fn spawn_background_refresh_is_noop_when_real_catalog_present() {
    let mgr = test_manager();
    mgr.inner.catalog.write().has_fetched_real_catalog = true;
    mgr.spawn_background_refresh_inner(/*remote_fetch_enabled*/ true); // must not panic (no tokio::spawn taken)
    assert!(mgr.has_fetched_real_catalog());
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_background_refresh_never_blocks_on_a_hanging_endpoint() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;

    struct NeverResolvingEndpoint {
        polled: Arc<AtomicBool>,
        dispatched: Arc<Notify>,
    }
    impl ModelsEndpoint for NeverResolvingEndpoint {
        fn fetch_models(
            &self,
            _endpoints: config::EndpointsConfig,
            _auth: Option<GrokAuth>,
            _fetch_auth: ModelFetchAuth,
        ) -> ModelsFetchFuture {
            let polled = self.polled.clone();
            let dispatched = self.dispatched.clone();
            Box::pin(async move {
                polled.store(true, Ordering::SeqCst);
                dispatched.notify_one();
                std::future::pending().await
            })
        }
    }

    let polled = Arc::new(AtomicBool::new(false));
    let dispatched = Arc::new(Notify::new());
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        make_prefetched(&["grok-4", "grok-4.5"]),
        acp::ModelId::new("grok-4.5"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(NeverResolvingEndpoint {
        polled: polled.clone(),
        dispatched: dispatched.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.spawn_background_refresh_inner(/*remote_fetch_enabled*/ true);
    assert!(
        !polled.load(Ordering::SeqCst),
        "fetch ran inline on the readiness path; it must be spawned",
    );

    // Generous failure bound: the dispatch may sit behind a full 5s auth dwell.
    tokio::time::timeout(std::time::Duration::from_secs(30), dispatched.notified())
        .await
        .expect("background refresh was never dispatched");
}

#[tokio::test]
#[serial]
async fn sign_out_clears_catalog_rebuilds_bundled_without_fetching() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Unset keys so fetch_auth resolves to Session (the sign-out branch).
    let _no_key = EnvGuard::unset("XAI_API_KEY");
    let _no_legacy_key = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let calls = Arc::new(AtomicUsize::new(0));
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let mgr = ModelsManagerBuilder::new(
        None,
        make_prefetched(&["grok-4", "grok-4.5"]),
        acp::ModelId::new("grok-4.5"),
        auth_manager,
        config_from_toml("[models]\ndefault = \"grok-4.5\""),
    )
    .endpoint(Arc::new(CountingEndpoint {
        calls: calls.clone(),
    }))
    .cache(test_cache_manager(tmp.path()))
    .build();

    mgr.inner.catalog.write().has_fetched_real_catalog = true;
    mgr.inner.user_selected_model.store(true, Ordering::Relaxed);

    mgr.on_auth_changed().await;

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "sign-out must skip the doomed Session-auth fetch",
    );
    assert!(
        !mgr.has_fetched_real_catalog(),
        "sign-out must drop the prior identity's real catalog",
    );
    assert!(
        !mgr.inner.user_selected_model.load(Ordering::Relaxed),
        "sign-out must reset the user-pick latch",
    );
    assert!(
        !mgr.models().is_empty(),
        "sign-out must rebuild the bundled default catalog",
    );
    assert_eq!(
        *mgr.inner.catalog_progress.borrow(),
        CatalogProgress::Failed,
        "sign-out publishes an outcome so parked waiters wake",
    );
}

#[test]
fn from_config_without_prefetch_produces_usable_catalog() {
    let tmp = tempfile::TempDir::new().unwrap();
    let auth_manager = Arc::new(AuthManager::new(tmp.path(), GrokComConfig::default()));
    let cfg = config::Config::default();

    let mgr = ModelsManager::from_config(&cfg, None, auth_manager).unwrap();

    let cat = mgr.inner.catalog.read();
    let catalog = &cat.models;
    assert!(
        !catalog.is_empty(),
        "zero-network boot must produce at least one model in the internal catalog"
    );
    let default = mgr.current_model_id();
    assert!(
        catalog.contains_key(default.0.as_ref()),
        "default model {:?} not in internal catalog: {:?}",
        default,
        catalog.keys().collect::<Vec<_>>()
    );
    drop(cat);
    assert!(
        !mgr.has_fetched_real_catalog(),
        "cold-cache boot must not claim a real catalog"
    );
}

#[test]
fn first_apply_refresh_reselects_default_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    assert!(!mgr.has_fetched_real_catalog());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");
}

#[test]
fn subsequent_apply_refresh_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "user's model selection must survive auth-change refresh"
    );
}

#[test]
fn subsequent_refresh_reselects_when_model_removed() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let prefetched = make_prefetched(&["grok-3", "grok-4.5"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "should fall back to config default when current is removed"
    );
}

#[test]
fn apply_config_honors_new_preferred_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut stale_cfg = config::Config::default();
    stale_cfg.models.default = None;
    *mgr.inner.cfg.write() = stale_cfg;

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-3".to_string());
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-3",
        "apply_config must honor updated preferred model from config"
    );
}

#[test]
fn apply_config_preserves_current_when_preferred_unchanged() {
    let mgr = test_manager();
    let cfg = config::Config::default();

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "apply_config must not reset model when preferred hasn't changed"
    );
}

#[test]
fn apply_config_falls_back_when_preferred_not_in_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-nonexistent".to_string());
    mgr.apply_config(new_cfg);

    let current = mgr.current_model_id();
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        current.0.as_ref(),
        first_available.0.as_ref(),
        "should fall back to first visible model when preferred not in catalog"
    );
}

#[test]
fn apply_config_both_none_preferred_preserves_current() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "both-None preferred must preserve user's runtime model"
    );
}

#[test]
fn apply_config_old_some_new_none_preserves_current() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-3");

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let new_cfg = config::Config::default();
    mgr.apply_config(new_cfg);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "old=Some new=None must not reset model (is_some guard)"
    );
}

#[test]
fn auth_refresh_then_config_reload_preserves_user_model() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);

    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    mgr.inner.catalog.write().prefetched = None;
    mgr.inner.catalog.write().etag = None;

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("grok-4".to_string());
    mgr.apply_config(new_cfg);
    assert_eq!(mgr.current_model_id().0.as_ref(), "grok-4");
}

fn test_cache_manager(dir: &std::path::Path) -> ModelsCacheManager {
    ModelsCacheManager {
        path: dir.join(MODELS_CACHE_FILE),
        ttl: CACHE_TTL,
    }
}

#[test]
#[serial]
fn reload_from_disk_cache_applies_external_catalog() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());

    cache.persist(
        &make_prefetched(&["grok-4.5", "grok-4.3"]),
        Some("etag-ext"),
        &mgr.cache_scope(),
        Utc::now(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert!(mgr.models().contains_key("grok-4.5"));
    assert!(mgr.models().contains_key("grok-4.3"));
    assert_eq!(mgr.inner.catalog.read().etag.as_deref(), Some("etag-ext"));
}

#[test]
#[serial]
fn reload_from_disk_cache_recomputes_allowlist_excludes_all() {
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\nallowed_models = [\"keep-*\"]");

    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["other-1"])), None);
    assert!(
        mgr.allowlist_excludes_all(),
        "setup: allowlist should exclude the entire catalog"
    );
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    cache.persist(
        &make_prefetched(&["keep-1"]),
        Some("etag-keep"),
        &mgr.cache_scope(),
        Utc::now(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.models().contains_key("keep-1"));
    assert!(
        !mgr.allowlist_excludes_all(),
        "corrective external cache write must unlatch the prompt block"
    );
}

#[test]
#[serial]
fn reload_from_disk_cache_resolves_default_on_first_catalog() {
    let mgr = test_manager();
    assert!(!mgr.has_fetched_real_catalog());
    let cfg = config_from_toml("[models]\ndefault = \"keep-1\"");
    *mgr.inner.cfg.write() = cfg.clone();

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    cache.persist(
        &make_prefetched(&["keep-1", "other-1"]),
        Some("etag-first"),
        &mgr.cache_scope(),
        Utc::now(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(mgr.has_fetched_real_catalog());
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "keep-1",
        "first real catalog must resolve the configured default"
    );
}

#[test]
#[serial]
fn reload_from_disk_cache_skips_identical_catalog_and_adopts_etag() {
    let mgr = test_manager();
    let cfg = config::Config::default();
    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched.clone()), Some("etag-a".into()));
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));

    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    cache.persist(&prefetched, Some("etag-b"), &mgr.cache_scope(), Utc::now());

    mgr.reload_from_cache_manager(&cache);

    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "identical catalog must not disturb the user's model"
    );
    assert_eq!(
        mgr.inner.catalog.read().etag.as_deref(),
        Some("etag-b"),
        "etag should be adopted so refresh_if_new_etag stays accurate"
    );
}

#[test]
#[serial]
fn reload_from_disk_cache_ignores_stale_cache() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let scope = mgr.cache_scope();
    let stale = ModelsCache {
        fetched_at: Utc::now() - ChronoDuration::seconds(3600),
        renewed_at: None,
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(scope.auth_method.clone()),
        origin: Some(scope.origin.clone()),
        identity: Some(scope.identity.clone()),
        etag: Some("etag-stale".into()),
        models: make_prefetched(&["grok-stale"]),
    };
    cache.atomic_write(&stale);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-stale"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[tokio::test]
async fn renew_ttl_does_not_shadow_a_newer_content_write() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let scope = mgr.cache_scope();

    // Content A on disk, fetched a minute ago.
    let a_fetched = Utc::now() - ChronoDuration::seconds(60);
    cache.persist(
        &make_prefetched(&["grok-a"]),
        Some("etag-a"),
        &scope,
        a_fetched,
    );
    // A TTL renewal bumps only the freshness clock to now.
    cache.renew_ttl(&scope).await;
    // Content B was fetched after A but before the renewal: it is genuinely
    // newer content and must win despite the renewal's fresh timestamp.
    let b_fetched = Utc::now() - ChronoDuration::seconds(30);
    cache.persist(
        &make_prefetched(&["grok-b"]),
        Some("etag-b"),
        &scope,
        b_fetched,
    );

    let loaded = cache
        .load_fresh(&scope)
        .expect("cache is fresh after renewal");
    assert!(
        loaded.models.contains_key("grok-b"),
        "a TTL renewal must not block a newer-content write"
    );
}

#[test]
#[serial]
fn reload_from_disk_cache_ignores_auth_method_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let scope = mgr.cache_scope();
    let other_auth = if scope.auth_method == CacheAuthMethod::Session {
        CacheAuthMethod::ApiKey
    } else {
        CacheAuthMethod::Session
    };
    let other = ModelsCacheScope {
        auth_method: other_auth,
        ..scope
    };
    cache.persist(
        &make_prefetched(&["grok-other-auth"]),
        Some("etag-x"),
        &other,
        Utc::now(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-auth"));
}

#[test]
#[serial]
fn reload_from_disk_cache_ignores_origin_mismatch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let other = ModelsCacheScope {
        origin: "http://127.0.0.1:49953/v1/models".to_string(),
        ..mgr.cache_scope()
    };
    cache.persist(
        &make_prefetched(&["grok-other-origin"]),
        Some("etag-y"),
        &other,
        Utc::now(),
    );

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-other-origin"));
    assert!(mgr.inner.catalog.read().etag.is_none());
}

#[test]
fn models_persist_does_not_regress_to_an_older_fetch() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let scope = mgr.cache_scope();

    let newer = Utc::now() - ChronoDuration::seconds(30);
    cache.persist(
        &make_prefetched(&["grok-new"]),
        Some("etag-new"),
        &scope,
        newer,
    );

    // An older same-scope fetch must not overwrite the newer catalog.
    cache.persist(
        &make_prefetched(&["grok-old"]),
        Some("etag-old"),
        &scope,
        newer - ChronoDuration::seconds(60),
    );

    let got = cache.load_fresh(&scope).unwrap();
    assert!(got.models.contains_key("grok-new"));
    assert_eq!(got.etag.as_deref(), Some("etag-new"));
}

#[test]
fn models_cache_read_is_scoped_by_identity() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let base = mgr.cache_scope();
    let scope_a = ModelsCacheScope {
        identity: "identity-a".to_string(),
        ..base.clone()
    };
    let scope_b = ModelsCacheScope {
        identity: "identity-b".to_string(),
        ..base
    };

    cache.persist(
        &make_prefetched(&["grok-a"]),
        Some("etag-a"),
        &scope_a,
        Utc::now(),
    );

    assert!(
        cache.load_fresh(&scope_b).is_none(),
        "a different account must not read another account's cached catalog",
    );
    assert!(
        cache
            .load_fresh(&scope_a)
            .is_some_and(|hit| hit.models.contains_key("grok-a")),
        "the writing account still hits its own entry",
    );
}

#[test]
fn models_cache_read_is_scoped_by_alpha_test_key() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let auth = GrokAuth::test_default();
    let base = mgr.cache_scope();
    let scope_a = ModelsCacheScope {
        identity: SettingsCacheManager::identity(&auth, Some("alpha-a")),
        ..base.clone()
    };
    let scope_b = ModelsCacheScope {
        identity: SettingsCacheManager::identity(&auth, Some("alpha-b")),
        ..base
    };

    cache.persist(
        &make_prefetched(&["grok-a"]),
        Some("etag-a"),
        &scope_a,
        Utc::now(),
    );

    assert!(
        cache.load_fresh(&scope_b).is_none(),
        "a different alpha cohort must not read another cohort's cached catalog",
    );
    assert!(
        cache
            .load_fresh(&scope_a)
            .is_some_and(|hit| hit.models.contains_key("grok-a")),
        "the same account and alpha still hits its own entry",
    );
}

#[test]
#[serial]
fn api_key_scope_identity_differs_per_key() {
    let _no_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    let identity_for = |key: &str| {
        let _key = EnvGuard::set("XAI_API_KEY", key);
        ModelsCacheScope::resolve(&endpoints, ModelFetchAuth::ApiKey, None).identity
    };
    assert_eq!(
        identity_for("key-a"),
        identity_for("key-a"),
        "the same API key must resolve to one scope",
    );
    assert_ne!(
        identity_for("key-a"),
        identity_for("key-b"),
        "different API keys must not cross-read one cache entry",
    );
}

#[test]
#[serial]
fn custom_endpoint_scope_identity_differs_per_key() {
    let _no_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    let identity_for = |key: &str| {
        let _key = EnvGuard::set("XAI_API_KEY", key);
        ModelsCacheScope::resolve(&endpoints, ModelFetchAuth::CustomEndpoint, None).identity
    };
    assert_eq!(
        identity_for("key-a"),
        identity_for("key-a"),
        "the same custom-endpoint key must resolve to one scope",
    );
    assert_ne!(
        identity_for("key-a"),
        identity_for("key-b"),
        "different keys on one custom endpoint must not cross-read",
    );
}

#[test]
#[serial]
fn custom_endpoint_session_scope_keys_on_account_not_bearer() {
    let _no_key = EnvGuard::unset("XAI_API_KEY");
    let _no_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    let identity_for = |user_id: &str, key: &str| {
        let auth = GrokAuth {
            user_id: user_id.to_string(),
            key: key.to_string(),
            ..GrokAuth::test_default()
        };
        ModelsCacheScope::resolve(&endpoints, ModelFetchAuth::CustomEndpoint, Some(&auth)).identity
    };
    assert_eq!(
        identity_for("account-a", "token-1"),
        identity_for("account-a", "token-2"),
        "a token refresh must keep the same account's custom-endpoint scope",
    );
    assert_ne!(
        identity_for("account-a", "token-1"),
        identity_for("account-b", "token-1"),
        "different accounts must not share a custom-endpoint catalog",
    );
}

#[test]
fn models_commit_gate_detects_account_switch() {
    let scope_for = |user_id: &str| ModelsCacheScope {
        auth_method: CacheAuthMethod::Session,
        origin: "https://origin.example/v1/models".to_string(),
        identity: SettingsCacheManager::identity(
            &GrokAuth {
                user_id: user_id.to_string(),
                ..GrokAuth::test_default()
            },
            None,
        ),
    };
    let expected = scope_for("account-a");
    assert!(
        matches!(
            evaluate_models_commit(&expected, &scope_for("account-a")),
            Commit::CacheAndServe
        ),
        "the same account (token rotation) must still cache",
    );
    assert!(
        matches!(
            evaluate_models_commit(&expected, &scope_for("account-b")),
            Commit::ServeInMemory
        ),
        "a mid-fetch account switch must serve in memory, not cache under the old account",
    );
    let moved = ModelsCacheScope {
        origin: "https://other.example/v1/models".to_string(),
        ..expected.clone()
    };
    assert!(matches!(
        evaluate_models_commit(&expected, &moved),
        Commit::Abandon
    ));
}

#[test]
#[serial]
fn resolve_live_keeps_fetch_origin_when_disk_auth_absent() {
    let _no_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    // Session fetch, then disk auth is gone at commit while XAI_API_KEY is set
    // (the just-logged-in / sign-out window). The live scope must stay on the
    // fetch-time Session origin so a good catalog is served, not abandoned.
    let _key = EnvGuard::set("XAI_API_KEY", "boot-window-key");
    let session_auth = GrokAuth {
        user_id: "session-user".to_string(),
        ..GrokAuth::test_default()
    };
    let endpoints = resolve_startup_endpoints();
    let expected =
        ModelsCacheScope::resolve(&endpoints, ModelFetchAuth::Session, Some(&session_auth));
    let live = ModelsCacheScope::resolve_live(ModelFetchAuth::Session, None);
    assert_eq!(
        live.origin, expected.origin,
        "the live mode must not flip to the api-key origin",
    );
    assert!(
        matches!(
            evaluate_models_commit(&expected, &live),
            Commit::ServeInMemory
        ),
        "a disk-absent Session commit must serve in memory, not abandon the catalog",
    );
}

#[test]
#[serial]
fn reload_from_disk_cache_ignores_legacy_cache_without_origin() {
    let mgr = test_manager();
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = test_cache_manager(tmp.path());
    let scope = mgr.cache_scope();
    let legacy = ModelsCache {
        fetched_at: Utc::now(),
        renewed_at: None,
        grok_version: Some(xai_grok_version::VERSION.to_string()),
        auth_method: Some(scope.auth_method.clone()),
        origin: None,
        identity: Some(scope.identity.clone()),
        etag: Some("etag-legacy".into()),
        models: make_prefetched(&["grok-legacy"]),
    };
    cache.atomic_write(&legacy);

    mgr.reload_from_cache_manager(&cache);

    assert!(!mgr.models().contains_key("grok-legacy"));
}

#[test]
fn clear_resets_has_fetched_real_catalog() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-3".to_string());

    let prefetched = make_prefetched(&["grok-3", "grok-4"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    assert!(mgr.has_fetched_real_catalog());

    mgr.clear();
    assert!(!mgr.has_fetched_real_catalog());

    let prefetched = make_prefetched(&["grok-4.5", "grok-4.3"]);
    mgr.apply_refresh_result(&cfg, Some(prefetched), None);
    let first_available = mgr.available().keys().next().unwrap().clone();
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        first_available.0.as_ref()
    );
}

#[test]
fn is_campaign_only_flip_detects_campaign_driven_changes() {
    let camp: std::collections::HashSet<String> = ["beta".into()].into_iter().collect();
    assert!(is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(is_campaign_only_flip(
        &Some("beta".into()),
        &Some("alpha".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("gamma".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(
        &Some("beta".into()),
        &Some("beta".into()),
        &camp
    ));
    assert!(!is_campaign_only_flip(&Some("beta".into()), &None, &camp));
    assert!(!is_campaign_only_flip(
        &Some("alpha".into()),
        &Some("beta".into()),
        &std::collections::HashSet::new()
    ));
}

#[test]
fn campaign_only_flip_does_not_reselect_live_session() {
    let mgr = test_manager();
    let mut cfg = config::Config::default();
    cfg.models.default = Some("alpha".to_string());
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr.inner.cfg.write() = cfg.clone(); // apply_config sees old_preferred as "alpha"
    assert_eq!(mgr.current_model_id().0.as_ref(), "alpha");

    let mut new_cfg = config::Config::default();
    new_cfg.models.default = Some("beta".to_string());
    new_cfg.models.default_is_campaign_driven = true;
    mgr.apply_config(new_cfg);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "alpha",
        "campaign-only flip must not yank a still-selectable live session"
    );

    let mgr2 = test_manager();
    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("alpha".to_string());
    mgr2.apply_refresh_result(&cfg2, Some(make_prefetched(&["alpha", "beta"])), None);
    *mgr2.inner.cfg.write() = cfg2.clone();
    let mut new_cfg2 = config::Config::default();
    new_cfg2.models.default = Some("beta".to_string());
    mgr2.apply_config(new_cfg2);
    assert_eq!(
        mgr2.current_model_id().0.as_ref(),
        "beta",
        "a non-campaign preferred change must reselect"
    );
}

#[test]
fn unavailable_campaign_default_falls_back_to_config_default() {
    let catalog = make_prefetched(&["real-model", "other-model"]);

    let mut cfg = config::Config::default();
    cfg.models.default = Some("missing-model".to_string());
    cfg.models.default_is_campaign_driven = true;
    cfg.models.pre_campaign_default = Some("real-model".to_string());
    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(
        key, "real-model",
        "must fall back to the pre-campaign default"
    );

    let mut cfg2 = config::Config::default();
    cfg2.models.default = Some("missing-model".to_string());
    cfg2.models.default_is_campaign_driven = true;
    cfg2.models.pre_campaign_default = Some("also-missing".to_string());
    let (key2, _, _) = resolve_default_model(&cfg2, &catalog, true);
    assert_eq!(&key2, catalog.keys().next().unwrap());

    let mut cfg3 = config::Config::default();
    cfg3.models.default = Some("missing-model".to_string());
    cfg3.models.pre_campaign_default = Some("real-model".to_string());
    let (key3, _, _) = resolve_default_model(&cfg3, &catalog, true);
    assert_eq!(
        &key3,
        catalog.keys().next().unwrap(),
        "non-campaign catalog miss must not recover via campaign state"
    );

    let mut cfg4 = config::Config {
        default_model_override: Some("missing-cli-model".to_string()),
        ..Default::default()
    };
    cfg4.models.default = Some("campaign-model".to_string());
    cfg4.models.default_is_campaign_driven = true;
    cfg4.models.pre_campaign_default = Some("real-model".to_string());
    let (key4, _, _) = resolve_default_model(&cfg4, &catalog, true);
    assert_eq!(
        &key4,
        catalog.keys().next().unwrap(),
        "a CLI pref miss must not detour through pre_campaign_default"
    );
}

use serial_test::serial;
use xai_grok_test_support::EnvGuard;

#[test]
#[serial]
fn resolve_custom_endpoint_always_wins() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig {
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::CustomEndpoint,
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::CustomEndpoint,
    );
}

#[test]
#[serial]
fn resolve_cached_session_wins_over_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "cached session should take priority over API key",
    );
}

#[test]
#[serial]
fn resolve_api_key_used_when_no_session() {
    let _key = EnvGuard::set("XAI_API_KEY", "test-key");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::ApiKey,
        "API key should be used when no cached session exists",
    );
}

#[test]
#[serial]
fn resolve_falls_back_to_session_when_nothing_set() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig::default();
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Session,
        "should fall back to Session when nothing else is configured",
    );
}

#[test]
#[serial]
fn resolve_deployment_key_when_no_session_or_api_key() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
    );
}

#[test]
#[serial]
fn resolve_deployment_key_outranks_ambient_api_key() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, false),
        ModelFetchAuth::Deployment,
        "managed deployment_key should outrank an ambient XAI_API_KEY",
    );
    assert_eq!(
        ModelFetchAuth::resolve(&endpoints, true),
        ModelFetchAuth::Session,
        "an active session should still win over a managed deployment",
    );
}

#[test]
#[serial]
fn prefetch_env_none_when_remote_fetch_disabled_despite_credentials() {
    let _key = EnvGuard::set("XAI_API_KEY", "stray-env-key");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        models_base_url: Some("https://custom.example.com".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(
        resolve_prefetch_inputs_from_parts(
            Some(GrokAuth::test_default()),
            endpoints.clone(),
            false,
        )
        .is_none(),
        "session auth must not re-arm the prefetch when remote_fetch is off",
    );
    assert!(
        resolve_prefetch_inputs_from_parts(None, endpoints, false).is_none(),
        "API key / deployment key / custom endpoint must not re-arm it either",
    );
}

#[test]
#[serial]
fn prefetch_env_resolves_when_remote_fetch_enabled() {
    let _unset = EnvGuard::unset("XAI_API_KEY");
    let _unset_legacy = EnvGuard::unset("GROK_CODE_XAI_API_KEY");
    let endpoints = config::EndpointsConfig {
        deployment_key: Some("deploy-key".to_owned()),
        ..config::EndpointsConfig::default()
    };
    assert!(resolve_prefetch_inputs_from_parts(None, endpoints, true).is_some());
    assert!(
        resolve_prefetch_inputs_from_parts(None, config::EndpointsConfig::default(), true)
            .is_none(),
        "no credentials and no custom endpoint must stay a no-prefetch launch",
    );
}

#[tokio::test]
async fn fetch_and_apply_degrades_offline_when_remote_fetch_disabled() {
    let mgr = test_manager();
    mgr.insert_test_entry("static-one", make_model_entry("static-one"));

    mgr.fetch_and_apply_inner(false).await;

    assert!(
        !mgr.has_fetched_real_catalog(),
        "no catalog fetch may be recorded when remote_fetch is disabled",
    );
    assert!(
        mgr.models().contains_key("static-one"),
        "the static catalog must keep resolving",
    );
}

#[test]
fn default_model_skips_oauth_only_for_api_key_users() {
    let cfg = config::Config::default();
    let mut catalog = IndexMap::new();

    let mut oauth_only = make_model_entry("oauth-only");
    oauth_only.info.supported_in_api = false;
    catalog.insert("oauth-only".to_string(), oauth_only);

    catalog.insert("public-model".to_string(), make_model_entry("public-model"));

    let (key, _, _) = resolve_default_model(&cfg, &catalog, false);
    assert_ne!(
        key, "oauth-only",
        "API-key default must not be an OAuth-only model"
    );
    assert_eq!(key, "public-model");

    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert!(
        key == "oauth-only" || key == "public-model",
        "OAuth user should be able to use either model as default"
    );
}

#[test]
fn visible_for_auth_logic() {
    let mut info = config::ModelInfo::fallback("test");

    assert!(info.visible_for_auth(true));
    assert!(info.visible_for_auth(false));

    info.hidden = true;
    assert!(!info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));

    info.hidden = false;
    info.supported_in_api = false;
    assert!(info.visible_for_auth(true));
    assert!(!info.visible_for_auth(false));
}

fn make_entry_config(model: &str, name: Option<&str>) -> config::ModelEntryConfig {
    make_entry_config_with_id(None, model, name)
}

fn make_entry_config_with_id(
    id: Option<&str>,
    model: &str,
    name: Option<&str>,
) -> config::ModelEntryConfig {
    config::ModelEntryConfig {
        id: id.map(|s| s.to_owned()),
        model: model.to_owned(),
        base_url: "https://test.api/v1".to_owned(),
        name: name.map(|n| n.to_owned()),
        context_window: std::num::NonZeroU64::new(200_000).unwrap(),
        ..Default::default()
    }
}

#[test]
fn build_prefetched_map_distinct_ids_same_slug() {
    let entries = vec![
        make_entry_config_with_id(Some("auto"), "grok-build", Some("Auto")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Grok Build")),
        make_entry_config_with_id(
            Some("experimental-fast"),
            "experimental-fast",
            Some("Grok Fast"),
        ),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 3, "all three entries should survive");
    assert!(map.contains_key("auto"));
    assert!(map.contains_key("grok-build"));
    assert!(map.contains_key("experimental-fast"));
    let Some(auto) = map.get("auto") else {
        panic!("expected auto: {map:?}");
    };
    assert_eq!(
        auto.info.model, "grok-build",
        "auto entry should still route to grok-build"
    );
    let Some(build) = map.get("grok-build") else {
        panic!("expected grok-build: {map:?}");
    };
    assert_eq!(build.info.model, "grok-build");
}

#[test]
fn build_prefetched_map_no_id_falls_back_to_slug() {
    let entries = vec![
        make_entry_config("model-a", Some("Model A")),
        make_entry_config("model-b", Some("Model B")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 2);
    assert!(map.contains_key("model-a"));
    assert!(map.contains_key("model-b"));
}

#[test]
fn build_prefetched_map_duplicate_id_overwrites() {
    let entries = vec![
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("First")),
        make_entry_config_with_id(Some("grok-build"), "grok-build", Some("Second")),
    ];
    let map = build_prefetched_map(entries, None);

    assert_eq!(map.len(), 1, "duplicate id: second overwrites first");
    let Some(build) = map.get("grok-build") else {
        panic!("expected grok-build: {map:?}");
    };
    assert_eq!(build.info.name.as_deref(), Some("Second"));
}

#[test]
fn resolve_default_model_prefers_id_over_model_slug() {
    let mut catalog: IndexMap<String, ModelEntry> = IndexMap::new();
    catalog.insert(
        "auto-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    catalog.insert("grok-build".to_string(), make_model_entry("grok-build"));

    let mut cfg = config::Config::default();
    cfg.models.default = Some("grok-build".to_string());

    let (key, _, _) = resolve_default_model(&cfg, &catalog, true);
    assert_eq!(key, "grok-build", "must match id, not first slug hit");
}

#[test]
fn resolve_catalog_key_maps_routing_slug_to_config_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn resolve_catalog_key_prefers_exact_key_match() {
    let mut models = IndexMap::new();
    models.insert("grok-4.5".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("exact key must resolve");
    assert_eq!(key.0.as_ref(), "grok-4.5");
}

#[test]
fn resolve_catalog_key_last_slug_match_wins() {
    let mut models = IndexMap::new();
    models.insert(
        "default-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );
    models.insert("user-grok-build".to_string(), make_model_entry("grok-4.5"));

    let persisted = acp::ModelId::new("grok-4.5");
    let key = resolve_catalog_key(&models, &persisted).expect("slug must resolve");
    assert_eq!(key.0.as_ref(), "user-grok-build");
}

#[test]
fn selectable_catalog_key_for_persisted_none_when_resolved_not_available() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-4.5"),
    );

    let available: IndexMap<_, _> = IndexMap::new();
    let persisted = acp::ModelId::new("grok-4.5");
    assert!(selectable_catalog_key_for_persisted(&models, &available, &persisted).is_none());
}

#[test]
fn selectable_prefers_available_identity_over_non_selectable_exact_key() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-build"));
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    assert_eq!(
        resolve_catalog_key(&models, &persisted)
            .expect("exact key exists")
            .0
            .as_ref(),
        "grok-build"
    );
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("must resolve to selectable section");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_matches_routing_slug_when_no_exact_key() {
    let mut models = IndexMap::new();
    models.insert(
        "enterprise-grok-build".to_string(),
        make_model_entry("grok-build"),
    );
    models.insert("grok-4.3".to_string(), make_model_entry("grok-4.3"));

    let available = test_available_keys(&["enterprise-grok-build", "grok-4.3"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("slug must resolve to selectable key");
    assert_eq!(key.0.as_ref(), "enterprise-grok-build");
}

#[test]
fn selectable_prefers_exact_key_over_later_slug_match() {
    let mut models = IndexMap::new();
    models.insert("grok-build".to_string(), make_model_entry("grok-4.5"));
    models.insert("other".to_string(), make_model_entry("grok-build"));

    let available = test_available_keys(&["grok-build", "other"]);

    let persisted = acp::ModelId::new("grok-build");
    let key = selectable_catalog_key_for_persisted(&models, &available, &persisted)
        .expect("exact selectable key must win");
    assert_eq!(key.0.as_ref(), "grok-build");
}

fn test_available_keys(keys: &[&str]) -> IndexMap<acp::ModelId, acp::ModelInfo> {
    keys.iter()
        .map(|k| {
            let id = acp::ModelId::new(*k);
            (id.clone(), acp::ModelInfo::new(id, (*k).to_string()))
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn bounded_auth_refresh_times_out_to_none() {
    // A hung identity provider (a never-ready auth future) must degrade to None within the bound so a cold-cache boot fetch can't stall on it
    let started = tokio::time::Instant::now();
    let result =
        ModelsManager::bounded_auth_refresh(std::future::pending::<Option<GrokAuth>>()).await;
    assert!(result.is_none(), "a hung auth refresh must yield None");
    assert!(
        started.elapsed() >= crate::http::STARTUP_AUTH_REFRESH_TIMEOUT,
        "must wait the full bound before giving up",
    );
}

#[tokio::test]
async fn bounded_auth_refresh_passes_through_ready_value() {
    let result =
        ModelsManager::bounded_auth_refresh(async { Some(GrokAuth::test_default()) }).await;
    assert!(
        result.is_some(),
        "a ready session must pass through unchanged"
    );
}

#[tokio::test]
async fn explicit_model_pick_survives_first_real_catalog() {
    // Non-blocking boot lets the user pick a model before the first real catalog lands; that pick must not be clobbered by default reselection
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4",
        "an explicit /model pick must survive the first real catalog",
    );
}

#[tokio::test]
async fn identity_switch_clears_user_pick_latch() {
    let mgr = test_manager();
    let cfg = config_from_toml("[models]\ndefault = \"grok-4.5\"");
    mgr.set_current_model_id(acp::ModelId::new("grok-4"));
    mgr.clear();
    mgr.apply_refresh_result(&cfg, Some(make_prefetched(&["grok-4.5", "grok-4"])), None);
    assert_eq!(
        mgr.current_model_id().0.as_ref(),
        "grok-4.5",
        "a new identity's first catalog must reselect the default after clear()",
    );
}

#[test]
fn personal_offline_boot_does_not_emit_a_managed_degraded_warn() {
    use crate::managed_config::LaunchProfile;
    use xai_grok_telemetry::unified_log::LogLevel;

    assert_eq!(
        degraded_log_level(LaunchProfile::Personal),
        LogLevel::Debug,
        "a personal offline boot must not WARN on every degraded start",
    );
    assert_eq!(
        degraded_log_level(LaunchProfile::Managed),
        LogLevel::Warn,
        "a managed degraded start stays a WARN: settings can gate the client",
    );
}
