// Modified for Distill by Samuel Fajreldines, 2026.
//! Defense-in-depth: `connect_or_spawn` must refuse when a non-`off` sandbox profile was requested, before any socket discovery or leader spawn.
//!
//! This test has its own binary: `set_configured_profile` writes a process-global `OnceLock` that other unit tests in this crate also set.

use distill_shell::leader::{
    ClientCapabilities, ClientMode, ConnectionError, LeaderEnvUrls, connect_or_spawn,
};

#[tokio::test]
async fn connect_or_spawn_refuses_when_sandbox_confinement_requested() {
    distill_sandbox::set_configured_profile("strict");

    let env_urls = LeaderEnvUrls {
        // Guard returns before LeaderLock / socket paths touch the filesystem.
        grok_ws_url: "wss://test.invalid/sandbox-confinement".into(),
        grok_ws_origin: "https://test.invalid".into(),
    };

    let err = match connect_or_spawn(
        "test-sandbox-confinement",
        ClientMode::Stdio,
        &env_urls,
        ClientCapabilities::default(),
    )
    .await
    {
        Ok(_) => panic!(
            "confined client must not adopt or spawn a leader (connect_or_spawn returned Ok)"
        ),
        Err(err) => err,
    };

    assert!(
        matches!(err, ConnectionError::SandboxConfinement("strict")),
        "expected SandboxConfinement(\"strict\"), got {err:?}"
    );
}
