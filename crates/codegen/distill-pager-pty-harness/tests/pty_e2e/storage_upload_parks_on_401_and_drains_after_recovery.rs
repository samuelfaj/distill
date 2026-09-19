// Modified for Distill by Samuel Fajreldines, 2026.
// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// 2a-park. Upload queue parks on storage 401 and drains after recovery. The trace artifact must
/// survive the outage (parked, without spamming retries) and land once storage accepts the bearer
/// again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn storage_upload_parks_on_401_and_drains_after_recovery() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{MOCK_RESPONSE_SENTINEL} park e2e response."));
    // Storage auth is down from the start; chat endpoints stay healthy
    content.set_storage_unauthorized(true);

    // Trace uploads are gated on first-party Grok OAuth (`is_xai_auth()` means AuthMode::Oidc with the Grok issuer)
    // The harness's XAI_API_KEY is ApiKey mode and never uploads, so seed a fake OAuth entry instead
    // The mock accepts any bearer, and the entry's failing refresh_token is exactly the parked state under test
    seed_fake_oauth(&content, "pty-park-e2e");

    // Explicit overrides win over the sandbox defaults
    // Disable only the fake API-key credential so the seeded OAuth entry stays active
    let overrides = [
        oauth_credential_ops()[0],
        EnvOp::set("GROK_TRACE_UPLOAD", "true"),
        EnvOp::set("GROK_TELEMETRY_TRACE_UPLOAD", "true"),
        EnvOp::set("GROK_UPLOAD_QUEUE_AUTH_PROBE_SECS", "2"),
    ];

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_env_ops(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        &overrides,
    )
    .expect("spawn pager with storage-401 mock");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("chat response on screen while storage is 401ing");

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while content.storage_request_count() < 2 && std::time::Instant::now() < deadline {
        harness.update(Duration::from_millis(200));
    }
    assert!(
        content.storage_request_count() >= 2,
        "storage saw the initial attempt + refresh retry; requests: {:?}",
        content.requests()
    );
    assert!(
        content.storage_uploads().is_empty(),
        "no upload may be accepted while the 401 gate is closed"
    );

    // This checks that requests stay bounded while parked, not that the queue goes fully quiet.
    const MAX_PARKED_WORKERS: u32 = 8;
    const REQUESTS_PER_WAKE: u32 = 2;
    const WAKE_CYCLE_HEADROOM: u32 = 2;
    const MAX_EXTRA_WHILE_PARKED: u32 =
        MAX_PARKED_WORKERS * REQUESTS_PER_WAKE * WAKE_CYCLE_HEADROOM;

    let parked_count = content.storage_request_count();
    harness.update(Duration::from_secs(3));
    let after = content.storage_request_count();
    assert!(
        after <= parked_count + MAX_EXTRA_WHILE_PARKED,
        "parked queue must not spam storage: {parked_count} -> {after} \
         (allowed +{MAX_EXTRA_WHILE_PARKED})"
    );
    assert!(
        harness.is_running().expect("poll pager liveness"),
        "pager stays healthy while parked"
    );

    content.set_storage_unauthorized(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while content.storage_uploads().is_empty() && std::time::Instant::now() < deadline {
        harness.update(Duration::from_millis(200));
    }
    let uploads = content.storage_uploads();
    assert!(
        !uploads.is_empty(),
        "parked artifact uploads after recovery; storage requests: {}",
        content.storage_request_count()
    );
    assert!(
        uploads.iter().all(|u| u.size > 0),
        "uploaded artifacts are non-empty: {uploads:?}"
    );

    harness.quit().expect("clean quit");
}
