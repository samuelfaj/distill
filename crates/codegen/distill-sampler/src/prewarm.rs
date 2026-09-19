//! Best-effort transport prewarm: dial an origin once per pool-idle window
//! through the shared client, bounded by [`PREWARM_TIMEOUT`], carrying no
//! credentials. Every non-warmed outcome releases the origin's claim so a
//! later dial can retry.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::shared_http::PooledClient;

const PREWARM_TIMEOUT: Duration = Duration::from_secs(15);

const DRAIN_CAP_BYTES: usize = 64 * 1024;

const MAX_TRACKED_ORIGINS: usize = 32;

enum WarmState {
    InFlight,
    Warmed(Instant),
}

static WARM_STATE: LazyLock<Mutex<HashMap<String, WarmState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn warm_state() -> std::sync::MutexGuard<'static, HashMap<String, WarmState>> {
    WARM_STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
enum ClaimError {
    AlreadyClaimed,
    TrackerFull,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
#[non_exhaustive]
pub enum PrewarmOutcome {
    SharingDisabled,
    NoOrigin,
    ClientUnavailable,
    AlreadyClaimed,
    TrackerFull,
    Warmed,
    Truncated,
    Failed,
    TimedOut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrewarmReport {
    pub outcome: PrewarmOutcome,
    /// `i64`, not `u64`: the OTel exporter allowlist keeps i64 fields and drops u64.
    pub duration_ms: i64,
    pub origin: Option<String>,
}

fn elapsed_ms_since(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

pub async fn prewarm_transport(base_url: &str) -> PrewarmReport {
    let started = Instant::now();
    let report = |outcome: PrewarmOutcome, origin: Option<String>| PrewarmReport {
        outcome,
        duration_ms: elapsed_ms_since(started),
        origin,
    };
    let Some(origin) = endpoint_origin(base_url) else {
        tracing::debug!(%base_url, "sampler transport prewarm skipped: no dialable origin");
        return report(PrewarmOutcome::NoOrigin, None);
    };
    let client = match crate::shared_http::pooled_client() {
        PooledClient::SharingDisabled => {
            return report(PrewarmOutcome::SharingDisabled, Some(origin));
        }
        PooledClient::Unavailable(error) => {
            tracing::debug!(%error, "sampler transport prewarm skipped: shared client build failed");
            return report(PrewarmOutcome::ClientUnavailable, Some(origin));
        }
        PooledClient::Ready(client) => client,
    };
    let claim = match claim_origin(&origin) {
        Ok(claim) => claim,
        Err(ClaimError::AlreadyClaimed) => {
            return report(PrewarmOutcome::AlreadyClaimed, Some(origin));
        }
        Err(ClaimError::TrackerFull) => {
            tracing::debug!(%origin, "sampler transport prewarm skipped: tracker full");
            return report(PrewarmOutcome::TrackerFull, Some(origin));
        }
    };
    let result = tokio::time::timeout(PREWARM_TIMEOUT, dial_and_drain(&client, &origin)).await;
    let elapsed_ms = elapsed_ms_since(started);
    let outcome = match result {
        Ok(Ok((status, DrainOutcome::Pooled))) => {
            claim.commit();
            tracing::info!(%origin, %status, elapsed_ms, "sampler transport prewarmed");
            PrewarmOutcome::Warmed
        }
        Ok(Ok((status, DrainOutcome::CappedUnpooled))) => {
            drop(claim);
            tracing::info!(%origin, %status, elapsed_ms, "sampler transport prewarm truncated");
            PrewarmOutcome::Truncated
        }
        Ok(Err(error)) => {
            drop(claim);
            tracing::info!(%origin, %error, elapsed_ms, "sampler transport prewarm failed");
            PrewarmOutcome::Failed
        }
        Err(_) => {
            drop(claim);
            tracing::info!(
                %origin,
                timeout_secs = PREWARM_TIMEOUT.as_secs() as i64,
                elapsed_ms,
                "sampler transport prewarm timed out"
            );
            PrewarmOutcome::TimedOut
        }
    };
    PrewarmReport {
        outcome,
        duration_ms: elapsed_ms,
        origin: Some(origin),
    }
}

#[must_use]
struct OriginClaim {
    origin: String,
    committed: bool,
}

impl OriginClaim {
    fn commit(mut self) {
        let origin = std::mem::take(&mut self.origin);
        self.committed = true;
        warm_state().insert(origin, WarmState::Warmed(Instant::now()));
    }
}

impl Drop for OriginClaim {
    fn drop(&mut self) {
        if !self.committed {
            warm_state().remove(&self.origin);
        }
    }
}

fn should_dial(state: Option<&WarmState>, now: Instant, idle_window: Duration) -> bool {
    match state {
        Some(WarmState::InFlight) => false,
        Some(WarmState::Warmed(at)) => now.duration_since(*at) >= idle_window,
        None => true,
    }
}

fn has_room_after_sweep(
    state: &mut HashMap<String, WarmState>,
    now: Instant,
    idle_window: Duration,
) -> bool {
    state.retain(|_, s| match s {
        WarmState::InFlight => true,
        WarmState::Warmed(at) => now.duration_since(*at) < idle_window,
    });
    state.len() < MAX_TRACKED_ORIGINS
}

fn claim_origin(origin: &str) -> Result<OriginClaim, ClaimError> {
    let idle_window = crate::shared_http::pool_idle_timeout();
    let now = Instant::now();
    let mut state = warm_state();
    if !should_dial(state.get(origin), now, idle_window) {
        return Err(ClaimError::AlreadyClaimed);
    }
    if state.len() >= MAX_TRACKED_ORIGINS && !has_room_after_sweep(&mut state, now, idle_window) {
        return Err(ClaimError::TrackerFull);
    }
    state.insert(origin.to_owned(), WarmState::InFlight);
    Ok(OriginClaim {
        origin: origin.to_owned(),
        committed: false,
    })
}

enum DrainOutcome {
    Pooled,
    CappedUnpooled,
}

async fn dial_and_drain(
    client: &reqwest::Client,
    origin: &str,
) -> reqwest::Result<(reqwest::StatusCode, DrainOutcome)> {
    // Redirects may be followed, but hyper pools the origin's connection before following, so the dial still warms it.
    let mut response = client.get(origin).send().await?;
    let status = response.status();
    let pools_at_handshake = response.version() >= reqwest::Version::HTTP_2;
    let mut drained = 0usize;
    // h1 pools a connection only once its body reaches EOF; h2 pools at the handshake.
    while let Some(chunk) = response.chunk().await? {
        drained += chunk.len();
        if drained > DRAIN_CAP_BYTES {
            let outcome = if pools_at_handshake {
                DrainOutcome::Pooled
            } else {
                DrainOutcome::CappedUnpooled
            };
            return Ok((status, outcome));
        }
    }
    Ok((status, DrainOutcome::Pooled))
}

fn endpoint_origin(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return None;
    }
    Some(url.origin().ascii_serialization())
}

enum WarmLookup {
    Warmed(Duration),
    InFlight,
    Absent,
}

fn warm_lookup(origin: &str, now: Instant) -> WarmLookup {
    match warm_state().get(origin) {
        Some(WarmState::Warmed(at)) => WarmLookup::Warmed(now.duration_since(*at)),
        Some(WarmState::InFlight) => WarmLookup::InFlight,
        None => WarmLookup::Absent,
    }
}

pub(crate) use first_use::note_first_sampling_use;

mod first_use {
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    use super::WarmLookup;

    const MAX_FIRST_USE_ORIGINS: usize = 32;

    static FIRST_USE_NOTED: LazyLock<Mutex<HashSet<String>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));

    fn first_use_noted() -> std::sync::MutexGuard<'static, HashSet<String>> {
        FIRST_USE_NOTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Warm freshness at first use, not observed pool reuse — reqwest
    /// exposes no pool-hit signal.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
    pub(super) enum Freshness {
        #[strum(serialize = "warm_fresh")]
        Fresh,
        #[strum(serialize = "warm_stale")]
        Stale,
        #[strum(serialize = "warm_pending")]
        Pending,
        #[strum(serialize = "warm_absent")]
        Absent,
    }

    pub(super) fn classify_first_use(
        lookup: WarmLookup,
        idle_window: Duration,
    ) -> (Freshness, Option<i64>) {
        match lookup {
            WarmLookup::Warmed(age) => (
                if age < idle_window {
                    Freshness::Fresh
                } else {
                    Freshness::Stale
                },
                Some(i64::try_from(age.as_millis()).unwrap_or(i64::MAX)),
            ),
            WarmLookup::InFlight => (Freshness::Pending, None),
            WarmLookup::Absent => (Freshness::Absent, None),
        }
    }

    pub(crate) fn note_first_sampling_use(base_url: &str) {
        let Some(origin) = super::endpoint_origin(base_url) else {
            return;
        };
        {
            let mut noted = first_use_noted();
            if noted.contains(&origin) {
                return;
            }
            if noted.len() >= MAX_FIRST_USE_ORIGINS
                && let Some(victim) = noted.iter().next().cloned()
            {
                noted.remove(&victim);
            }
            noted.insert(origin.clone());
        }
        let idle_window = crate::shared_http::pool_idle_timeout();
        let (freshness, age_at_first_use_ms) =
            classify_first_use(super::warm_lookup(&origin, Instant::now()), idle_window);
        let span = tracing::info_span!(
            parent: None,
            "sampler.transport_prewarm_first_use",
            endpoint = tracing::field::Empty,
            freshness = tracing::field::Empty,
            age_at_first_use_ms = tracing::field::Empty,
        );
        span.record("endpoint", origin.as_str());
        span.record("freshness", <&'static str>::from(freshness));
        if let Some(age_at_first_use_ms) = age_at_first_use_ms {
            span.record("age_at_first_use_ms", age_at_first_use_ms);
        }
    }
}

#[cfg(test)]
#[path = "prewarm_tests.rs"]
mod tests;
