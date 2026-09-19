// Modified for Distill by Samuel Fajreldines, 2026.
//! Every managed-config file read and write, and every flock acquisition site. Synchronous only.

use distill_login::GrokAuth;

use super::policy::{
    GateSnapshot, ManagedPolicyRefusal, auth_mode, claim_binds_to, served_principal_of,
    write_failure_is_deny,
};
use super::response::{
    ApplyOutcome, ManagedConfigResponse, ManagedConfigSource, verify_signed_envelope,
};

/// Server-synced policy artifacts; excludes the sync marker.
pub const MANAGED_ARTIFACT_FILES: [&str; 4] = [
    distill_config::MANAGED_CONFIG_FILENAME,
    distill_config::REQUIREMENTS_FILENAME,
    distill_config::signed_policy::SIGNATURE_SIDECAR_FILE,
    distill_config::signed_policy::MANAGED_IDENTITY_SIDECAR_FILE,
];

pub(super) fn remove_managed_config_files(home: &std::path::Path) {
    let mut artifacts_removed = true;
    for name in MANAGED_ARTIFACT_FILES {
        artifacts_removed &= remove_synced_file(home, name, "removed managed config file");
    }
    // Marker last, only on full success: crash/error leaves the detector armed for the next start.
    // The stage shares the marker's fate — only a completed eviction may delete it.
    if artifacts_removed {
        remove_synced_file(
            home,
            distill_config::MANAGED_CONFIG_CACHE_FILE,
            "removed managed config file",
        );
        let _ = std::fs::remove_file(staged_refresh_path(home));
    }
    let atomic_write_tmp_prefixes = [
        format!("{}.", distill_config::MANAGED_CONFIG_CACHE_FILE),
        format!("{}.", distill_config::signed_policy::SIGNATURE_SIDECAR_FILE),
        format!(
            "{}.",
            distill_config::signed_policy::MANAGED_IDENTITY_SIDECAR_FILE
        ),
    ];
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_write_tmp = name.ends_with(".tmp")
                && atomic_write_tmp_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix.as_str()));
            if is_write_tmp {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Returns whether the path is gone (removed or already absent); `false` = removal failed.
fn remove_synced_file(home: &std::path::Path, name: &str, why: &str) -> bool {
    let path = home.join(name);
    match remove_managed_path(&path) {
        Ok(true) => {
            tracing::info!(file = %path.display(), "{why}");
            true
        }
        Ok(false) => true,
        Err(e) => {
            tracing::warn!(file = %path.display(), error = %e, "failed to remove managed config file");
            false
        }
    }
}

/// A squatting directory would fail the atomic rename forever; best-effort.
fn clear_squatting_dir(path: &std::path::Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && let Err(e) = remove_managed_path(path)
    {
        tracing::warn!(error = %e, "failed to clear a directory squatting at a managed config path");
    }
}

/// Removes files and squatting directories. `Ok(true)` = removed; `Ok(false)` = absent.
fn remove_managed_path(path: &std::path::Path) -> std::io::Result<bool> {
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let result = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Non-expired only: an expired token would just 401.
pub(super) fn eligible_team_principal(auth: GrokAuth) -> Option<GrokAuth> {
    (auth.is_team_principal() && !distill_login::is_expired(&auth)).then_some(auth)
}

/// Single-team: managed config is a grok.com feature with one grok.com auth.
fn read_team_principal() -> std::io::Result<Option<GrokAuth>> {
    let home = crate::util::distill_home::distill_home();
    let store = distill_login::read_auth_json(&distill_login::auth_json_path(&home))?;
    Ok(store.into_values().find(|a| a.is_team_principal()))
}

pub(super) fn read_active_team_auth() -> Option<GrokAuth> {
    eligible_team_principal(read_team_principal().ok().flatten()?)
}

/// Ignores expiry; `Err` is not a logout — treating it as one would wipe policy on a read blip.
pub(super) fn team_principal_signed_in() -> std::io::Result<bool> {
    match read_team_principal() {
        Ok(team) => Ok(team.is_some()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Best-effort; a fail_closed opt-in is kept — swapping `auth.json` must not escape policy.
pub fn clear_orphan() {
    // Env switch only: a served `[features] managed_config = false` must not veto evicting itself.
    if crate::agent::config::env_bool("GROK_MANAGED_CONFIG") == Some(false)
        || resolve_deployment_key().is_some()
    {
        return;
    }
    match team_principal_signed_in() {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, "auth.json unreadable; keeping managed config until it recovers");
            return;
        }
    }
    let home = crate::util::distill_home::distill_home();
    let Some(_lock) = try_lock_managed_config(&home) else {
        return; // another process is syncing; retry next call
    };
    if distill_config::fail_closed_policy_armed_at(&home) {
        tracing::info!(
            "keeping fail_closed managed policy on disk; no team principal present to own a clear"
        );
        return;
    }
    remove_managed_config_files(&home);
}

/// Cross-process apply/remove lock; `None` on contention (callers skip and retry).
pub(super) fn try_lock_managed_config(home: &std::path::Path) -> Option<std::fs::File> {
    use fs2::FileExt;
    let file = open_managed_config_lock(home).ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

pub(super) enum GateLockAttempt {
    Acquired(std::fs::File),
    Contended(std::fs::File),
    /// Open failure, or ENOLCK/ENOTSUP etc.: locking is unavailable, not busy.
    Unavailable,
}

pub(super) fn try_gate_lock(home: &std::path::Path) -> GateLockAttempt {
    use fs2::FileExt;
    let Ok(lock_file) = open_managed_config_lock(home) else {
        return GateLockAttempt::Unavailable;
    };
    match lock_file.try_lock_exclusive() {
        Ok(()) => GateLockAttempt::Acquired(lock_file),
        Err(e) if lock_is_contended(&e) => GateLockAttempt::Contended(lock_file),
        Err(_) => GateLockAttempt::Unavailable,
    }
}

/// Polled, not `lock_exclusive`: a wedged holder needs a hard cap.
pub(super) fn wait_for_gate_lock(
    lock_file: &std::fs::File,
    home: &std::path::Path,
    lock_wait: std::time::Duration,
) -> Result<(), ManagedPolicyRefusal> {
    use fs2::FileExt;
    let deadline = std::time::Instant::now() + lock_wait;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(ManagedPolicyRefusal::Busy);
        }
        std::thread::sleep(GATE_LOCK_RETRY_DELAY.min(remaining));
        match lock_file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(e) if lock_is_contended(&e) => {}
            Err(_) => {
                return Err(ManagedPolicyRefusal::LockUnavailable {
                    home: home.to_path_buf(),
                });
            }
        }
    }
}

/// Not `WouldBlock`: Windows surfaces contention (ERROR_LOCK_VIOLATION) as `Uncategorized`.
fn lock_is_contended(e: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    e.kind() == contended.kind() && e.raw_os_error() == contended.raw_os_error()
}

fn open_managed_config_lock(home: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
}

const GATE_LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

/// Purge, floor tick, and reads under the caller-held gate lock — one consistent state.
/// `home` must be the process `distill_home()`: the identity reads resolve it internally.
pub(super) fn gate_snapshot_locked(home: &std::path::Path) -> GateSnapshot {
    // Purge first so an offline team switch isn't misread as a substituted cache.
    purge_prior_tenant_locked(home);
    // Raise the floor after the purge so a purged marker stays absent.
    distill_config::bump_rollback_floor(home);
    GateSnapshot {
        managed_principal_present: managed_principal_present(),
        // Expiry-ignoring: a backdated auth.json must not resolve Team→None and relax binding.
        policy_compromised: crate::config::managed_policy_compromised_for(
            &current_serving_identity_any_expiry(),
        ),
    }
}

/// Marker-scoped: key-scoped markers never purge here, and config.toml blips are not switches.
fn purge_prior_tenant_locked(home: &std::path::Path) {
    let crate::config::ServingIdentity::Team(team_id) = current_serving_identity_any_expiry()
    else {
        return;
    };
    if let Some(evicted) = crate::config::confirmed_team_switch_at(home, &team_id) {
        tracing::warn!(
            team_id = %team_id,
            evicted_principal = %evicted,
            "identity changed; purging the prior tenant's managed config"
        );
        remove_managed_config_files(home);
    }
}

/// Best-effort: a failed tick must not refuse a session.
pub(super) fn bump_managed_rollback_floor() {
    // Re-checked inside `bump_rollback_floor`; this early-out skips the lock I/O when dark.
    if !distill_config::signed_policy::verification_active() {
        return;
    }
    let home = crate::util::distill_home::distill_home();
    match try_lock_managed_config(&home) {
        Some(_lock) => {
            distill_config::bump_rollback_floor(&home);
        }
        None => tracing::debug!("managed-config lock contended; skipping the floor tick"),
    }
}

/// Converge disk to the served set — a leftover would keep enforcing a withdrawn policy.
pub(super) fn apply_managed_config(
    home: &std::path::Path,
    body: &ManagedConfigResponse,
) -> std::io::Result<bool> {
    let artifacts = [
        (
            distill_config::MANAGED_CONFIG_FILENAME,
            body.managed_config.as_deref(),
        ),
        (
            distill_config::REQUIREMENTS_FILENAME,
            body.requirements.as_deref(),
        ),
    ];

    let mut changed = false;
    let mut first_err: Option<std::io::Error> = None;
    for (name, content) in artifacts {
        let path = home.join(name);
        match content.filter(|s| !s.is_empty()) {
            Some(content) => {
                clear_squatting_dir(&path);
                // 0o600: `managed_config` can embed the enforced deployment key.
                match distill_config::fs_atomic::write_atomically(&path, content, Some(0o600)) {
                    Ok(()) => changed = true,
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            None => match remove_managed_path(&path) {
                Ok(true) => {
                    tracing::info!("removed managed config artifact the server no longer serves");
                    changed = true;
                }
                Ok(false) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            },
        }
    }

    if changed {
        tracing::info!("managed config refreshed from server");
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(changed),
    }
}

pub(super) fn apply_fetched(
    body: &ManagedConfigResponse,
    source: ManagedConfigSource,
    new_principal: Option<&str>,
    new_key_fingerprint: Option<&str>,
    parked_at: Option<u64>,
) -> std::io::Result<ApplyOutcome> {
    // Verify before lock/persist: prior trusted policy survives a bad fetch.
    let verified = if distill_config::signed_policy::verification_active() {
        match verify_signed_envelope(body, active_team_id_any_expiry().as_deref()) {
            Ok(verified) => Some(verified),
            Err(e) => {
                tracing::warn!("managed config signature rejected; not persisting: {e}");
                return Ok(ApplyOutcome::SignatureRejected);
            }
        }
    } else {
        None
    };
    let signed_deployment_id = verified
        .as_ref()
        .and_then(|v| v.payload.deployment_id.clone());
    let home = crate::util::distill_home::distill_home();
    let Some(_lock) = try_lock_managed_config(&home) else {
        tracing::debug!("managed config locked by another process; skipping apply");
        return Ok(ApplyOutcome::Skipped);
    };
    if !credential_present(source) {
        tracing::info!("credential gone since fetch started; skipping apply");
        return Ok(ApplyOutcome::Skipped);
    }
    // A parked refresh's authoritative freshness check, under the same flock as the apply.
    if let Some(parked_at) = parked_at
        && parked_at < distill_config::managed_config_synced_at(&home).unwrap_or(0)
    {
        return Ok(ApplyOutcome::StaleStage);
    }
    let identity_changed = crate::config::managed_config_identity_changed_at(
        &home,
        new_principal,
        new_key_fingerprint,
    );
    let wrote = match apply_managed_config(&home, body) {
        // A switch's destructive half lands only with its constructive half: the policy
        // files are converged above, so only the prior principal's sidecars go.
        Ok(wrote) => {
            if identity_changed {
                evict_prior_sidecars(&home);
            }
            wrote
        }
        // Sandbox write-deny (trust-boundary set, H1-3969489): park the verified response
        // for the next boot. Only the deny class stages, never unverified content.
        Err(e) if verified.is_some() && write_failure_is_deny(&e) => {
            stage_refresh(&home, body, source, new_principal, new_key_fingerprint)?;
            tracing::info!(
                error = %e,
                "policy files write-denied in this process; staged the verified refresh for the next boot"
            );
            return Ok(ApplyOutcome::Staged);
        }
        Err(e) => return Err(e),
    };
    // Sidecar after policy files so a present sidecar covers the final set.
    if let Some(verified) = verified {
        clear_squatting_dir(&home.join(distill_config::signed_policy::SIGNATURE_SIDECAR_FILE));
        distill_config::signed_policy::write_sidecar(&home, &verified.sidecar)?;
        if let Some(claim_sidecar) =
            verified_claim_sidecar(body, served_principal_of(&verified.payload))
        {
            clear_squatting_dir(
                &home.join(distill_config::signed_policy::MANAGED_IDENTITY_SIDECAR_FILE),
            );
            distill_config::signed_policy::write_managed_identity_sidecar(&home, &claim_sidecar)?;
        }
    }
    // Marker last, still under the lock: post-release, a concurrent purge could delete
    // the files it describes.
    clear_squatting_dir(&home.join(distill_config::MANAGED_CONFIG_CACHE_FILE));
    crate::config::mark_managed_config_synced_at(
        &home,
        crate::config::SyncMarker {
            // DK: prefer verified payload deployment id (signed-empty only has it there).
            // Team: always the serving team — a deployment-signed envelope must not rebind it.
            principal: if new_key_fingerprint.is_some() {
                signed_deployment_id.as_deref().or(new_principal)
            } else {
                new_principal
            },
            had_managed_config: body.has_managed_config(),
            had_requirements: body.has_requirements(),
            key_fingerprint: new_key_fingerprint,
            fail_closed: body.requirements_fail_closed(),
        },
    );
    // A completed apply invalidates any parked stage — replaying it would roll policy back.
    let _ = std::fs::remove_file(staged_refresh_path(&home));
    Ok(ApplyOutcome::Applied { wrote })
}

/// A verified refresh parked for the next boot's pre-sandbox apply; content only lands
/// through [`apply_fetched`]'s re-verification.
pub(super) fn staged_refresh_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join("staged").join("managed_config_refresh.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StagedRefresh {
    source: ManagedConfigSource,
    principal: Option<String>,
    key_fingerprint: Option<String>,
    response: ManagedConfigResponse,
    /// Park time under the apply flock (the marker's clock); 0 = legacy, never applied.
    #[serde(default)]
    parked_at: u64,
}

fn stage_refresh(
    home: &std::path::Path,
    body: &ManagedConfigResponse,
    source: ManagedConfigSource,
    principal: Option<&str>,
    key_fingerprint: Option<&str>,
) -> std::io::Result<()> {
    let staged = StagedRefresh {
        source,
        principal: principal.map(str::to_owned),
        key_fingerprint: key_fingerprint.map(str::to_owned),
        response: body.clone(),
        parked_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let json = serde_json::to_string(&staged).map_err(std::io::Error::other)?;
    let path = staged_refresh_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    distill_config::fs_atomic::write_atomically(&path, &json, Some(0o600))
}

/// Bounded local I/O, re-verified through [`apply_fetched`]; an unverifying build deletes it unread.
pub(super) fn apply_staged_managed_config() {
    let home = crate::util::distill_home::distill_home();
    let path = staged_refresh_path(&home);
    let Ok(json) = std::fs::read_to_string(&path) else {
        return;
    };
    // Fetch-disabled and unverifiable builds discard a stage unread (fail-safe).
    if !is_fetch_enabled() || !distill_config::signed_policy::verification_active() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let Ok(staged) = serde_json::from_str::<StagedRefresh>(&json) else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    // A key configured or rotated since the stage re-fetches instead of rebinding.
    let current_fingerprint = resolve_deployment_key().map(|key| deployment_key_fingerprint(&key));
    if staged.key_fingerprint != current_fingerprint {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // Early-out only; the authoritative freshness refusal runs under the apply flock.
    if staged.parked_at == 0
        || staged.parked_at < distill_config::managed_config_synced_at(&home).unwrap_or(0)
    {
        let _ = std::fs::remove_file(&path);
        return;
    }
    match apply_fetched(
        &staged.response,
        staged.source,
        staged.principal.as_deref(),
        staged.key_fingerprint.as_deref(),
        Some(staged.parked_at),
    ) {
        // Lock contended: the holder's own sync supersedes; retry next boot.
        Ok(ApplyOutcome::Skipped) => {}
        // Still write-denied (a post-sandbox boot): keep for a pre-sandbox one.
        Ok(ApplyOutcome::Staged) => {}
        Ok(ApplyOutcome::StaleStage) => {
            let _ = std::fs::remove_file(&path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
        }
        Err(e) => {
            tracing::warn!("staged managed config refresh failed to apply: {e}");
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `None` skips: a bad claim must not fail the apply (it only hardens the sidecar).
pub(super) fn verified_claim_sidecar(
    body: &ManagedConfigResponse,
    served_principal: Option<&str>,
) -> Option<distill_config::signed_policy::SignatureEnvelope> {
    use distill_config::signed_policy::now_unix;
    let sidecar = body.managed_identity_sidecar()?;
    // Unclamped wall clock, like the policy verify: a fresh claim heals an inflated floor.
    let claim = match distill_config::signed_policy::verify_fetched_claim(&sidecar, now_unix()) {
        Ok(claim) => claim,
        Err(e) => {
            tracing::debug!("is-managed claim did not verify; not persisting it: {e}");
            return None;
        }
    };
    if !claim_binds_to(&claim, served_principal) {
        tracing::debug!("is-managed claim is bound to a different principal; not persisting it");
        return None;
    }
    Some(sidecar)
}

/// The prior tenant's sidecars must not survive to read foreign-bound.
fn evict_prior_sidecars(home: &std::path::Path) {
    for name in [
        distill_config::signed_policy::SIGNATURE_SIDECAR_FILE,
        distill_config::signed_policy::MANAGED_IDENTITY_SIDECAR_FILE,
    ] {
        remove_synced_file(home, name, "evicted prior principal's sidecar");
    }
}

/// Mirrors `clear_orphan`'s fail-safe checks (an unreadable `auth.json` keeps, not drops).
fn credential_present(source: ManagedConfigSource) -> bool {
    match source {
        ManagedConfigSource::DeploymentKey => resolve_deployment_key().is_some(),
        ManagedConfigSource::TeamOauth => team_principal_signed_in().unwrap_or(true),
    }
}

/// The server deployment UUID on a marker fingerprint match, else UUIDv5 of the key.
pub(crate) fn resolve_deployment_id(deployment_key: Option<&str>) -> Option<String> {
    let key = deployment_key.filter(|k| !k.is_empty())?;
    crate::config::managed_deployment_id(&deployment_key_fingerprint(key))
        .or_else(|| Some(crate::agent::config::deployment_id_from_key(key)))
}

pub(crate) fn resolve_deployment_key() -> Option<String> {
    let config_val = crate::config::load_effective_config()
        .map_err(|e| tracing::warn!("failed to load config files for deployment key: {e}"))
        .ok()
        .and_then(|root| {
            root.get("endpoints")?
                .get("deployment_key")?
                .as_str()
                .map(|s| s.to_owned())
        });
    crate::agent::config::resolve_string_flag(
        None,
        "GROK_DEPLOYMENT_KEY",
        config_val.as_deref(),
        None,
    )
    .map(|r| r.value)
}

/// Deterministic so the same key matches its marker; the raw key is never written to disk.
pub(super) fn deployment_key_fingerprint(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

/// Overlay-free read: a `GROK_CONFIG` overlay must not suppress a policy-enforcement sync.
pub fn is_fetch_enabled() -> bool {
    if let Some(v) = crate::agent::config::env_bool("GROK_MANAGED_CONFIG") {
        return v;
    }
    crate::config::ConfigLayers::load()
        .ok()
        .and_then(|layers| super::policy::managed_config_enabled_from_layers(&layers))
        .unwrap_or(true)
}

pub fn has_principal() -> bool {
    resolve_deployment_key().is_some() || read_active_team_auth().is_some()
}

/// Ignores expiry so a backdated `auth.json` can't disarm the gate; unreadable = present.
pub(super) fn managed_principal_present() -> bool {
    resolve_deployment_key().is_some() || team_principal_signed_in().unwrap_or(true)
}

fn serving_identity_from(team_id: Option<String>) -> crate::config::ServingIdentity {
    use crate::config::ServingIdentity;
    if let Some(key) = resolve_deployment_key() {
        return ServingIdentity::DeploymentKey {
            fingerprint: deployment_key_fingerprint(&key),
        };
    }
    // Trimmed and blank = unknown, matching the marker write.
    match crate::config::normalize_identity(team_id.as_deref()) {
        Some(team_id) => ServingIdentity::Team(team_id),
        None => ServingIdentity::None,
    }
}

pub fn current_serving_identity() -> crate::config::ServingIdentity {
    serving_identity_from(read_active_team_auth().and_then(|a| a.team_id))
}

/// Ignores expiry; no deployment-key special case, or envelope binding would be off for team users.
pub(super) fn active_team_id_any_expiry() -> Option<String> {
    let team = read_team_principal().ok().flatten()?;
    // The id must read the same everywhere it feeds (gate, purge, envelope binding).
    crate::config::normalize_identity(team.team_id.as_deref())
}

pub(super) fn current_serving_identity_any_expiry() -> crate::config::ServingIdentity {
    serving_identity_from(active_team_id_any_expiry())
}

pub fn classify_auth_mode() -> distill_telemetry::startup::AuthMode {
    auth_mode(
        resolve_deployment_key().is_some(),
        &team_principal_signed_in(),
    )
}
