// Modified for Distill by Samuel Fajreldines, 2026.
//! The single refresh owner: every managed-config fetch and apply is driven from here.

use distill_login::GrokAuth;

use super::ManagedConfigError;
use super::response::{
    ApplyOutcome, ManagedConfigResponse, ManagedConfigSource, verify_signed_envelope,
};
use super::{policy, store};

#[derive(Clone, Copy)]
enum SyncBudget {
    Standard,
    Revalidate,
    Login,
    SessionStart,
}

impl SyncBudget {
    fn max_attempts(self) -> u32 {
        match self {
            Self::Standard | Self::Revalidate => 5,
            Self::Login | Self::SessionStart => 2,
        }
    }

    fn deadline(self) -> Option<std::time::Duration> {
        match self {
            Self::Standard => None,
            Self::Revalidate => Some(REVALIDATE_DEADLINE),
            Self::Login => Some(std::time::Duration::from_secs(15)),
            Self::SessionStart => Some(SESSION_START_SYNC_DEADLINE),
        }
    }
}

const REVALIDATE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Bounds the pre-fetch `auth()` wait; on timeout the sync proceeds with no refreshed override.
pub const SESSION_START_AUTH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

pub const SESSION_START_SYNC_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// Base 1s; `GROK_DEPLOYMENT_CONFIG_BACKOFF_MS` overrides it for tests.
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let base = std::env::var("GROK_DEPLOYMENT_CONFIG_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1000);
    std::time::Duration::from_millis(base << attempt.saturating_sub(1))
}

/// Retries transient failures with backoff; auth errors fail immediately.
async fn fetch_managed_config(
    url: &str,
    token: &str,
    source: ManagedConfigSource,
    max_attempts: u32,
    echo_principal: Option<&str>,
) -> Result<ManagedConfigResponse, ManagedConfigError> {
    crate::http::send_with_retry_escaping_pool(
        move |client: reqwest::Client| async move {
            fetch_managed_config_once(&client, url, token, source, echo_principal).await
        },
        max_attempts,
        |e: &ManagedConfigError| e.is_retryable(),
        |attempt| tokio::time::sleep(retry_backoff(attempt)),
    )
    .await
}

pub(super) fn map_transport_failure(failure: crate::http::TransportFailure) -> ManagedConfigError {
    use crate::http::TransportFailureKind;
    match failure.kind {
        TransportFailureKind::CertificateUntrusted => {
            ManagedConfigError::CertificateUntrusted(policy::certificate_detail(
                failure.detail,
                distill_extra_ca::configured_bundle_env(),
                distill_extra_ca::extra_root_ders().len(),
            ))
        }
        TransportFailureKind::CertificateInvalid => {
            ManagedConfigError::CertificateInvalid(failure.detail)
        }
        TransportFailureKind::Unreachable => ManagedConfigError::Network(failure.detail),
        TransportFailureKind::Interrupted => {
            ManagedConfigError::ConnectionInterrupted(failure.detail)
        }
        // A builder/redirect failure is a client-side defect, not a bad server response: terminal.
        TransportFailureKind::Permanent => ManagedConfigError::RequestFailed(failure.detail),
    }
}

fn map_send_error(e: &reqwest::Error) -> ManagedConfigError {
    map_transport_failure(crate::http::TransportFailure::classify(e))
}

async fn fetch_managed_config_once(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    source: ManagedConfigSource,
    echo_principal: Option<&str>,
) -> Result<ManagedConfigResponse, ManagedConfigError> {
    let mut request = client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .timeout(std::time::Duration::from_secs(15));
    // Replay-probe echo (telemetry only); fail-open so a corrupt sidecar never bricks the fetch.
    if let Some(nonce) = distill_config::signed_policy::stored_envelope_nonce(
        &crate::util::distill_home::distill_home(),
        echo_principal,
    ) && let Ok(value) = reqwest::header::HeaderValue::from_str(&nonce)
    {
        request = request.header(
            distill_config::signed_policy::MANAGED_CONFIG_NONCE_ECHO_HEADER,
            value,
        );
    }
    let resp = match request.send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status().as_u16();
            tracing::debug!(status, "managed config fetch failed");
            return Err(if status == 401 || status == 403 {
                source.auth_rejected_error()
            } else {
                ManagedConfigError::ServerError { status }
            });
        }
        Err(e) => {
            let err = map_send_error(&e);
            tracing::debug!(error = %err, "managed config fetch error");
            return Err(err);
        }
    };

    // reqwest's `json()` tags a mid-body drop and malformed JSON both as `Kind::Decode`;
    // reading `bytes()` first keeps interruption (retryable) apart from bad JSON (terminal).
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return Err(ManagedConfigError::ConnectionInterrupted(
                crate::http::error_cause_chain(&e),
            ));
        }
    };
    serde_json::from_slice::<ManagedConfigResponse>(&bytes)
        .map_err(|e| ManagedConfigError::InvalidResponse(e.to_string()))
}

/// Clamped >= 1s: `tokio::time::interval` panics on a zero period.
fn managed_config_sync_interval() -> std::time::Duration {
    if let Ok(s) = std::env::var("GROK_DEPLOYMENT_CONFIG_REFRESH_INTERVAL_SECS")
        && let Ok(secs) = s.parse::<u64>()
    {
        return std::time::Duration::from_secs(secs.max(1));
    }
    std::time::Duration::from_secs(5 * 60)
}

/// Dropping cancels and aborts the task.
#[must_use]
pub struct ManagedConfigRefresher {
    pub(super) cancel: tokio_util::sync::CancellationToken,
    pub(super) handle: tokio::task::JoinHandle<()>,
}

impl ManagedConfigRefresher {
    pub(super) fn spawn(
        parent: &tokio_util::sync::CancellationToken,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Self {
        let cancel = parent.child_token();
        let stop = cancel.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                // Biased: an already-cancelled token must not even start the work.
                biased;
                _ = stop.cancelled() => {}
                _ = work => {}
            }
        });
        Self { cancel, handle }
    }
}

impl Drop for ManagedConfigRefresher {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

/// Mutex, not OnceLock: a supervisor whose runtime died with its agent thread must be replaceable.
pub(super) static REFRESH_SUPERVISOR: std::sync::Mutex<Option<ManagedConfigRefresher>> =
    std::sync::Mutex::new(None);

/// The one place a managed-config refresh can be scheduled; called per boot, post-gate.
pub fn start_refresh_supervisor(auth_manager: &std::sync::Arc<distill_login::AuthManager>) {
    // Every boot: a respawn after a contended logout cleanup must not serve the prior team.
    store::clear_orphan();
    let auth_manager = auth_manager.clone();
    ensure_supervisor(move || {
        spawn_refresh_supervisor(&tokio_util::sync::CancellationToken::new(), auth_manager)
    });
}

/// Keeps a live supervisor, replaces a dead one; the slot only swaps state, so poison is harmless.
pub(super) fn ensure_supervisor(spawn: impl FnOnce() -> ManagedConfigRefresher) {
    let mut slot = REFRESH_SUPERVISOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if slot.as_ref().is_some_and(|sup| !sup.handle.is_finished()) {
        return;
    }
    *slot = Some(spawn());
}

/// Test seam: take the armed supervisor so a test's guard drop disarms it.
pub fn take_refresh_supervisor() -> Option<ManagedConfigRefresher> {
    REFRESH_SUPERVISOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

pub fn spawn_refresh_supervisor(
    cancel: &tokio_util::sync::CancellationToken,
    auth_manager: std::sync::Arc<distill_login::AuthManager>,
) -> ManagedConfigRefresher {
    ManagedConfigRefresher::spawn(cancel, async move {
        revalidate_stale_start(auth_manager).await;

        let mut interval = tokio::time::interval(managed_config_sync_interval());
        interval.tick().await; // skip immediate first tick

        loop {
            interval.tick().await;

            store::clear_orphan();
            // Without the floor tick a clock rollback could make an expired policy read valid.
            store::bump_managed_rollback_floor();

            if !crate::config::is_managed_config_stale_for(&store::current_serving_identity())
                || !store::is_fetch_enabled()
            {
                continue;
            }

            match sync().await {
                Ok(true) => tracing::info!("background managed config sync: updated"),
                Ok(false) => {}
                Err(e) => tracing::debug!("background managed config sync failed: {e}"),
            }
        }
    })
}

async fn revalidate_stale_start(auth_manager: std::sync::Arc<distill_login::AuthManager>) {
    if !store::is_fetch_enabled() {
        return;
    }
    // An auth.json read error is not "no principal".
    if store::resolve_deployment_key().is_none()
        && matches!(store::team_principal_signed_in(), Ok(false))
    {
        return;
    }
    if !crate::config::is_managed_config_stale_for(&store::current_serving_identity_any_expiry()) {
        return;
    }
    let team = refreshed_team_principal(&auth_manager).await;
    match sync_bounded(SyncBudget::Revalidate, team).await {
        Some(Ok(_)) => {}
        Some(Err(e)) => tracing::debug!("stale-start managed config revalidation failed: {e}"),
        None => tracing::debug!("stale-start managed config revalidation timed out"),
    }
}

pub async fn sync() -> Result<bool, ManagedConfigError> {
    Ok(sync_with_budget(SyncBudget::Standard, None).await?.wrote)
}

struct SyncOutcome {
    wrote: bool,
    served: bool,
    skipped: bool,
    staged: bool,
    source: Option<ManagedConfigSource>,
    signature_rejected: bool,
}

impl SyncOutcome {
    fn from_fetch(
        body: &ManagedConfigResponse,
        source: ManagedConfigSource,
        outcome: &ApplyOutcome,
    ) -> Self {
        Self {
            wrote: outcome.wrote(),
            served: body.config_exists(),
            skipped: outcome.skipped(),
            staged: outcome.staged(),
            source: Some(source),
            signature_rejected: outcome.signature_rejected(),
        }
    }
}

async fn sync_bounded(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Option<Result<SyncOutcome, ManagedConfigError>> {
    let sync = sync_with_budget(budget, team_override);
    match budget.deadline() {
        Some(deadline) => tokio::time::timeout(deadline, sync).await.ok(),
        None => Some(sync.await),
    }
}

enum FetchedConfig {
    DeploymentKey {
        key: String,
        body: ManagedConfigResponse,
    },
    Team {
        auth: Box<GrokAuth>,
        body: ManagedConfigResponse,
    },
    NoPrincipal,
}

/// Fetch without touching disk: the deployment key first, then a signed-in team.
async fn fetch_for_principal(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Result<FetchedConfig, ManagedConfigError> {
    let max_attempts = budget.max_attempts();
    // Merged-config resolution: the bearer must not go to the public default URL.
    let url =
        crate::agent::config::EndpointsConfig::from_effective_config().resolve_managed_config_url();

    let team_auth = team_override.or_else(store::read_active_team_auth);

    if let Some(dk) = store::resolve_deployment_key() {
        let source = ManagedConfigSource::DeploymentKey;
        // Echo binds to the deployment this key last synced.
        let echo_principal =
            crate::config::managed_deployment_id(&store::deployment_key_fingerprint(&dk));
        match fetch_managed_config(&url, &dk, source, max_attempts, echo_principal.as_deref()).await
        {
            // A rejected key must not starve a valid team sign-in; network/5xx do not fall through.
            Err(ManagedConfigError::DeploymentKeyRejected) if team_auth.is_some() => {
                tracing::warn!("deployment key rejected; falling back to the team session token");
            }
            Err(e) => return Err(e),
            // Only a missing row falls through: applying the empty key body would delete team files.
            Ok(body) if !body.config_exists() && team_auth.is_some() => {
                tracing::debug!("deployment key has no config; trying the team principal");
            }
            Ok(body) => return Ok(FetchedConfig::DeploymentKey { key: dk, body }),
        }
    }

    if let Some(auth) = team_auth {
        let body = fetch_managed_config(
            &url,
            &auth.key,
            ManagedConfigSource::TeamOauth,
            max_attempts,
            auth.team_id.as_deref(),
        )
        .await?;
        return Ok(FetchedConfig::Team {
            auth: Box::new(auth),
            body,
        });
    }

    Ok(FetchedConfig::NoPrincipal)
}

async fn sync_with_budget(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Result<SyncOutcome, ManagedConfigError> {
    match fetch_for_principal(budget, team_override).await? {
        FetchedConfig::DeploymentKey { key, body } => {
            let source = ManagedConfigSource::DeploymentKey;
            let fingerprint = store::deployment_key_fingerprint(&key);
            let outcome = store::apply_fetched(
                &body,
                source,
                body.deployment_id.as_deref(),
                Some(&fingerprint),
                None,
            )?;
            Ok(SyncOutcome::from_fetch(&body, source, &outcome))
        }
        FetchedConfig::Team { auth, body } => {
            let source = ManagedConfigSource::TeamOauth;
            let outcome = store::apply_fetched(&body, source, auth.team_id.as_deref(), None, None)?;
            Ok(SyncOutcome::from_fetch(&body, source, &outcome))
        }
        FetchedConfig::NoPrincipal => Ok(SyncOutcome {
            wrote: false,
            served: false,
            skipped: false,
            staged: false,
            source: None,
            signature_rejected: false,
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedConfigSync {
    Skipped,
    Updated {
        is_team: bool,
    },
    /// Verified and parked; applies on the next start.
    Staged,
    NoChange,
    Failed,
}

/// Failures are logged, not propagated.
pub async fn post_login_sync(authenticated: Option<GrokAuth>) -> ManagedConfigSync {
    store::clear_orphan();
    if !store::is_fetch_enabled() {
        return ManagedConfigSync::Skipped;
    }
    let team = authenticated
        .and_then(store::eligible_team_principal)
        .or_else(store::read_active_team_auth);
    if team.is_none()
        && !crate::config::is_managed_config_stale_for(&store::current_serving_identity())
    {
        return ManagedConfigSync::Skipped;
    }
    match sync_bounded(SyncBudget::Login, team).await {
        // A rejected envelope is a failure, not "no change": the gate may refuse next session.
        Some(Ok(SyncOutcome {
            signature_rejected: true,
            ..
        })) => {
            tracing::warn!("post-login managed config sync: server envelope rejected");
            ManagedConfigSync::Failed
        }
        Some(Ok(SyncOutcome { staged: true, .. })) => {
            tracing::info!("post-login managed config sync: staged for the next start");
            ManagedConfigSync::Staged
        }
        Some(Ok(SyncOutcome {
            wrote: true,
            source,
            ..
        })) => {
            tracing::info!("post-login managed config sync: updated");
            ManagedConfigSync::Updated {
                is_team: source == Some(ManagedConfigSource::TeamOauth),
            }
        }
        Some(Ok(_)) => ManagedConfigSync::NoChange,
        Some(Err(e)) => {
            tracing::debug!("post-login managed config sync failed: {e}");
            ManagedConfigSync::Failed
        }
        None => {
            tracing::debug!("post-login managed config sync timed out");
            ManagedConfigSync::Failed
        }
    }
}

/// True when the session-start repair will run; while this holds, no startup
/// fetch may send an authenticated request.
pub(crate) fn policy_repair_pending() -> bool {
    policy_repair_pending_from(
        store::resolve_deployment_key().is_some(),
        &store::team_principal_signed_in(),
    )
}

fn policy_repair_pending_from(
    has_deployment_key: bool,
    signed_in_team: &std::io::Result<bool>,
) -> bool {
    if !store::is_fetch_enabled() {
        return false;
    }
    // Err reading auth.json is not "no principal"; a read blip must not skip enforcement.
    if !has_deployment_key && matches!(signed_in_team, Ok(false)) {
        return false;
    }
    // Ignore expiry: a usable same-identity cache should not refresh just to re-learn the team id.
    let identity = store::current_serving_identity_any_expiry();
    matches!(identity, crate::config::ServingIdentity::None)
        || crate::config::is_managed_config_hard_stale_for(&identity)
}

/// A usable cache serves the start; only an unusable-for-identity cache blocks (bounded).
pub async fn ensure_managed_policy_present(
    auth_manager: &std::sync::Arc<distill_login::AuthManager>,
) {
    distill_telemetry::startup::enter(distill_telemetry::startup::StartupPhase::ManagedPolicy);
    let has_deployment_key = store::resolve_deployment_key().is_some();
    let signed_in_team = store::team_principal_signed_in();
    distill_telemetry::startup::set_auth_mode(policy::auth_mode(
        has_deployment_key,
        &signed_in_team,
    ));
    // A parked refresh applies here, pre-sandbox, before staleness is judged; it gates
    // itself (fetch-disabled or unverifiable discards, missing principal self-refuses).
    store::apply_staged_managed_config();
    if !policy_repair_pending_from(has_deployment_key, &signed_in_team) {
        return;
    }
    let team = {
        let mut timer = crate::instrumentation_timer!("startup.managed_policy.auth_wait");
        timer.with_subphase(distill_telemetry::startup::Subphase::ManagedPolicyAuthWait);
        refreshed_team_principal(auth_manager).await
    };
    if !store::has_principal() {
        return;
    }
    if !crate::config::is_managed_config_hard_stale_for(&store::current_serving_identity()) {
        return;
    }
    let mut timer = crate::instrumentation_timer!("startup.managed_policy.config_sync");
    timer.with_subphase(distill_telemetry::startup::Subphase::ManagedPolicyConfigSync);
    match sync_bounded(SyncBudget::SessionStart, team).await {
        Some(Ok(_)) => {}
        Some(Err(e)) => tracing::warn!("session-start managed policy refresh failed: {e}"),
        None => tracing::warn!("session-start managed policy refresh timed out"),
    }
}

/// The deadline bounds only the WAIT: `auth()` runs on its own task, so a token rotated
/// near the bound (or under a cancelled caller) still persists to disk.
async fn refreshed_team_principal(
    auth_manager: &std::sync::Arc<distill_login::AuthManager>,
) -> Option<GrokAuth> {
    let refresh = tokio::spawn({
        let auth_manager = auth_manager.clone();
        async move { auth_manager.auth().await }
    });
    tokio::time::timeout(SESSION_START_AUTH_DEADLINE, refresh)
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok)
        .filter(GrokAuth::is_team_principal)
}

#[derive(Debug)]
pub enum SetupOutcome {
    Installed,
    NothingConfigured,
    /// Verified and parked; applies on the next start.
    Staged,
    /// Nothing persisted by THIS run; re-running converges.
    Skipped,
    Failed(ManagedConfigError),
}

/// What the server serves, verbatim — `managed_config` may embed the enforced deployment key.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupReport {
    pub source: Option<&'static str>,
    pub configured: bool,
    pub deployment_id: Option<String>,
    pub team_id: Option<String>,
    pub managed_config: Option<String>,
    pub requirements: Option<String>,
    pub fail_closed: bool,
}

/// Writes nothing: no artifacts, no sidecar, no marker.
pub async fn fetch_setup_report() -> Result<SetupReport, ManagedConfigError> {
    let (source, body) = match fetch_for_principal(SyncBudget::Standard, None).await? {
        FetchedConfig::DeploymentKey { body, .. } => (Some("deploymentKey"), body),
        FetchedConfig::Team { body, .. } => (Some("teamOauth"), body),
        FetchedConfig::NoPrincipal => (None, ManagedConfigResponse::default()),
    };
    // A payload the installer would refuse is an error, not printable config.
    if source.is_some()
        && distill_config::signed_policy::verification_active()
        && let Err(e) = verify_signed_envelope(&body, store::active_team_id_any_expiry().as_deref())
    {
        tracing::warn!("managed config signature rejected: {e}");
        return Err(ManagedConfigError::SignatureRejected);
    }
    Ok(SetupReport {
        source,
        configured: body.config_exists(),
        fail_closed: body.requirements_fail_closed(),
        deployment_id: body.deployment_id,
        team_id: body.team_id,
        managed_config: body.managed_config,
        requirements: body.requirements,
    })
}

pub async fn run_setup() -> SetupOutcome {
    match sync_with_budget(SyncBudget::Standard, None).await {
        // Installed would mask a fetch the gate is about to refuse.
        Ok(SyncOutcome {
            signature_rejected: true,
            ..
        }) => SetupOutcome::Failed(ManagedConfigError::SignatureRejected),
        Ok(SyncOutcome { staged: true, .. }) => SetupOutcome::Staged,
        Ok(SyncOutcome { skipped: true, .. }) => SetupOutcome::Skipped,
        Ok(SyncOutcome { served: true, .. }) => SetupOutcome::Installed,
        Ok(_) => SetupOutcome::NothingConfigured,
        Err(e) => SetupOutcome::Failed(e),
    }
}
