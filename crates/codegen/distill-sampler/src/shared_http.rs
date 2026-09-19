// Modified for Distill by Samuel Fajreldines, 2026.
//! Shared `reqwest::Client`s for uncredentialed sampling requests.
//!
//! Sharing the uncredentialed clients is safe because their builders take no config-derived input.
//! Auth, extra headers, base URL, and User-Agent are applied per-request in `SamplingClient::post`.
//! Stale connections are bounded by h2 keepalive (15s ping, 5s timeout), 90s idle-pool eviction, and the pool-less HTTP/1.1 first-retry rebuild.
//! Connections whose per-session runtime died are discarded by hyper's checkout ready-check, with the retry loop covering the rest.
//!
//! Wire behavior is pinned by the `shared_http_wire` and `shared_http_kill_switch` binaries.
//! `GROK_EXTRA_CA_BUNDLE` adds extra CA roots to these clients and the mTLS clients.

use std::sync::OnceLock;
use std::time::Duration;

mod mtls;

pub(crate) use mtls::client as mtls_client;

static SHARED_H2: OnceLock<reqwest::Client> = OnceLock::new();
static SHARED_HTTP1: OnceLock<reqwest::Client> = OnceLock::new();

/// Kill switch: `GROK_SAMPLER_SHARED_CLIENT=0` (or `false`, any case) builds a fresh `reqwest::Client` per `SamplingClient` instead.
/// Resolved once per process: the environment cannot change externally after spawn.
/// Latching keeps the rollback consistent with the pool knobs, which are also read only once.
fn sharing_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        let disabled = match std::env::var("GROK_SAMPLER_SHARED_CLIENT") {
            Ok(v) => v == "0" || v.eq_ignore_ascii_case("false"),
            Err(_) => false,
        };
        if disabled {
            tracing::info!("sampler HTTP client sharing disabled via GROK_SAMPLER_SHARED_CLIENT");
        }
        disabled
    })
}

/// Clone the shared client out of `cell`, building it on first use.
/// Build failures are not cached: on `Err` the cell stays empty and the next call retries.
/// A racing loser's freshly built client is dropped.
fn shared(
    cell: &OnceLock<reqwest::Client>,
    build: fn() -> Result<reqwest::Client, reqwest::Error>,
    disabled: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    if disabled {
        return build();
    }
    if let Some(client) = cell.get() {
        return Ok(client.clone());
    }
    let built = build()?;
    Ok(cell.get_or_init(|| built).clone())
}

/// Shared HTTP/2 sampling client (connection pooling and h2 keepalive).
pub(crate) fn client() -> Result<reqwest::Client, reqwest::Error> {
    shared(&SHARED_H2, build_http_client, sharing_disabled())
}

pub(crate) enum PooledClient {
    SharingDisabled,
    Unavailable(reqwest::Error),
    Ready(reqwest::Client),
}

/// The pooled client worth prewarming, or why there is none.
pub(crate) fn pooled_client() -> PooledClient {
    if sharing_disabled() {
        return PooledClient::SharingDisabled;
    }
    match client() {
        Ok(client) => PooledClient::Ready(client),
        Err(error) => PooledClient::Unavailable(error),
    }
}

/// Idle timeout the shared pool evicts after; read once so prewarm's re-warm window stays in step.
pub(crate) fn pool_idle_timeout() -> Duration {
    static SECS: OnceLock<u64> = OnceLock::new();
    Duration::from_secs(*SECS.get_or_init(|| {
        std::env::var("GROK_POOL_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(90)
    }))
}

/// Shared HTTP/1.1 fallback client.
/// It has no connection pool, so sharing it behaves the same as building a fresh one.
pub(crate) fn client_http1() -> Result<reqwest::Client, reqwest::Error> {
    shared(&SHARED_HTTP1, build_http_client_http1, sharing_disabled())
}

/// Build a `reqwest::Client` for sampling with HTTP/2 and connection pooling.
/// Env knobs are read once, when the shared client is first built.
fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    distill_extra_ca::build_reqwest_client(configure_http2)
}

fn configure_http2(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let pool_max_idle: usize = std::env::var("GROK_POOL_MAX_IDLE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let connect_timeout_secs: u64 = std::env::var("GROK_CONNECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    builder
        .pool_max_idle_per_host(pool_max_idle)
        .pool_idle_timeout(pool_idle_timeout())
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(Duration::from_secs(15))
        .http2_keep_alive_timeout(Duration::from_secs(5))
        .http2_keep_alive_while_idle(true)
}

/// Build a `reqwest::Client` constrained to HTTP/1.1 with pooling disabled.
/// Used as a fallback after HTTP/2 transport failures.
fn build_http_client_http1() -> Result<reqwest::Client, reqwest::Error> {
    distill_extra_ca::build_reqwest_client(configure_http1)
}

fn configure_http1(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let connect_timeout_secs: u64 = std::env::var("GROK_CONNECT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);

    builder
        .http1_only()
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(Duration::from_secs(0))
        .connect_timeout(Duration::from_secs(connect_timeout_secs))
        .tcp_nodelay(true)
}

#[allow(clippy::disallowed_methods)] // test clients hit localhost mocks
#[cfg(test)]
mod tests {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::shared;

    static BUILD_CALLS: AtomicUsize = AtomicUsize::new(0);

    /// Fails on the first call (a real `reqwest::Error`, no I/O), then builds.
    fn flaky_build() -> Result<reqwest::Client, reqwest::Error> {
        if BUILD_CALLS.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(reqwest::Proxy::all("not a proxy url").unwrap_err());
        }
        reqwest::Client::builder().build()
    }

    #[test]
    fn shared_does_not_cache_build_failures() {
        static CELL: OnceLock<reqwest::Client> = OnceLock::new();
        assert!(shared(&CELL, flaky_build, false).is_err());
        assert!(CELL.get().is_none(), "failure must leave the cell empty");
        assert!(shared(&CELL, flaky_build, false).is_ok());
        assert!(CELL.get().is_some(), "success must populate the cell");
        assert!(shared(&CELL, flaky_build, false).is_ok());
        assert_eq!(
            BUILD_CALLS.load(Ordering::SeqCst),
            2,
            "third call must reuse the cached client, not rebuild"
        );
    }

    #[test]
    fn shared_disabled_bypasses_cell() {
        static CELL: OnceLock<reqwest::Client> = OnceLock::new();
        assert!(shared(&CELL, || reqwest::Client::builder().build(), true).is_ok());
        assert!(
            CELL.get().is_none(),
            "disabled mode must never touch the cell"
        );
    }
}
