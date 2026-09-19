// Modified for Distill by Samuel Fajreldines, 2026.
//! The managed-config cloud-cache subsystem: the sync marker, serving identity, and staleness (timer and hard).
//! It also holds the fail-closed enforcement gate that combines the signed-cache verdict with the best-effort marker.
//!
//! The marker is **unsigned** and user-writable: a refresh hint, not a tamper control.
//! Real tamper resistance is [`crate::signed_policy`] plus the OS-protected layers (root-owned `/etc/grok`, MDM).

use std::path::Path;

use crate::paths::user_distill_home;

/// Sync marker; staleness keys on this, not mtimes.
/// Public so removal code can name it apart from the policy artifacts (removed last).
pub const MANAGED_CONFIG_CACHE_FILE: &str = "managed_config_cache.json";

/// The on-disk marker: unsigned, detects only deletion or identity change, not in-place edits (see the module doc).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ManagedConfigCache {
    /// Unix seconds of the last successful fetch.
    synced_at: Option<u64>,
    /// Team id, or the deploy-key path's server `deployment_id` (reported via [`managed_deployment_id`]; identity is `key_fingerprint`).
    principal: Option<String>,
    /// Artifacts this sync served, so staleness spots a later deletion; `default` false so pre-upgrade markers don't over-claim.
    #[serde(default)]
    had_managed_config: bool,
    #[serde(default)]
    had_requirements: bool,
    /// Deploy-key fingerprint (never the raw key), the deploy-key identity (see [`ServingIdentity`]); `None` on the team path.
    #[serde(default)]
    key_fingerprint: Option<String>,
    /// Served opt-in (`fail_closed = true`); `default` false so a pre-upgrade or un-opted marker never fails closed.
    #[serde(default)]
    fail_closed: bool,
    /// Local-clock high-water mark.
    /// At-rest signed checks use `max(now, floor)` so a rolled-back clock cannot un-expire a policy.
    /// As forgeable as the rest of the marker: defeats a passive clock change, not a file edit.
    #[serde(default)]
    rollback_floor: u64,
    /// Fields written by newer binaries, preserved when this binary rewrites only the floor.
    /// A full sync rewrites the marker from scratch.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// What the cache is bound to (one value, so a (team, key) combo can't form).
/// The deploy-key fingerprint is the only identity verifiable offline (there is no map from key to `deployment_id` without the network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServingIdentity {
    Team(String),
    DeploymentKey { fingerprint: String },
    None,
}

/// Whether to refetch for `identity`: no marker, past the timer, different identity, or a served artifact now missing.
/// Best-effort: callers continue without managed config on failure.
pub fn is_managed_config_stale_for(identity: &ServingIdentity) -> bool {
    managed_config_stale_at(user_distill_home().as_deref(), identity)
}

/// Fields a successful sync records.
/// A struct (destructured without `..`) so a new field is a compile error at every writer.
/// Three adjacent positional bools would silently transpose.
pub struct SyncMarker<'a> {
    pub principal: Option<&'a str>,
    pub had_managed_config: bool,
    pub had_requirements: bool,
    pub key_fingerprint: Option<&'a str>,
    pub fail_closed: bool,
}

/// Record a successful sync (best-effort; called even for a config-less principal so it doesn't refetch every tick).
pub fn mark_managed_config_synced(marker: SyncMarker<'_>) {
    if let Some(home) = user_distill_home() {
        mark_managed_config_synced_at(&home, marker);
    }
}

/// Server-side DistillDeployment UUID from the last deploy-key managed-config sync, bound to the key that synced it.
/// A rotated or removed key therefore never reports the previous deployment's id.
/// Team-path syncs store a team id and no fingerprint, so they never match.
pub fn managed_deployment_id(key_fingerprint: &str) -> Option<String> {
    managed_deployment_id_at(user_distill_home()?.as_path(), key_fingerprint)
}

fn managed_deployment_id_at(home: &Path, key_fingerprint: &str) -> Option<String> {
    if key_fingerprint.trim().is_empty() {
        return None;
    }
    let cache = read_managed_config_cache(home)?;
    if cache.key_fingerprint.as_deref() != Some(key_fingerprint) {
        return None;
    }
    normalize_identity(cache.principal.as_deref())
}

/// [`mark_managed_config_synced`] for an explicit `home` (apply-lock holder: same dir as lock).
pub fn mark_managed_config_synced_at(home: &Path, marker: SyncMarker<'_>) {
    let SyncMarker {
        principal,
        had_managed_config,
        had_requirements,
        key_fingerprint,
        fail_closed,
    } = marker;
    let synced_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok();
    let cache = ManagedConfigCache {
        synced_at,
        // Blank becomes None: the marker must never record "unknown" as a tenant
        principal: normalize_identity(principal),
        // What THIS sync served (not what is on disk); an identity switch already evicted the prior artifacts
        had_managed_config,
        had_requirements,
        key_fingerprint: normalize_identity(key_fingerprint),
        fail_closed,
        // Reset (not max): reconnect must clear an inflated floor
        // Residual risk: fetch verify is unclamped and managed_config_url is user-writable
        // A rolled-back clock plus a still-valid replayed envelope can reinstate a superseded policy and reset the floor
        rollback_floor: synced_at.unwrap_or(0),
        extra: Default::default(),
    };
    match serde_json::to_string(&cache) {
        Ok(json) => write_marker_atomically(home, &json),
        Err(e) => tracing::warn!("failed to serialize managed config cache: {e}"),
    }
}

/// Raise an existing marker's floor to the wall clock; in a dark build this is a no-op.
/// Caller holds the managed-config lock so this serializes with the fetch-path floor reset.
pub fn bump_rollback_floor(home: &Path) {
    bump_rollback_floor_with_now(home, crate::signed_policy::now_unix());
}

/// [`bump_rollback_floor`] with an injected timestamp, for tests.
#[doc(hidden)]
pub fn bump_rollback_floor_with_now(home: &Path, now: u64) {
    if !crate::signed_policy::verification_active() {
        return;
    }
    raise_rollback_floor(home, now);
}

/// `max(prior, now)`: never lowers, never creates a marker (purge must stay purged).
fn raise_rollback_floor(home: &Path, now: u64) {
    let Some(mut cache) = read_managed_config_cache(home) else {
        return;
    };
    let raised = cache.rollback_floor.max(now);
    if raised == cache.rollback_floor {
        return;
    }
    cache.rollback_floor = raised;
    match serde_json::to_string(&cache) {
        Ok(json) => write_marker_atomically(home, &json),
        Err(e) => tracing::warn!("failed to serialize managed config cache: {e}"),
    }
}

/// The marker's last successful apply time (unix seconds), if any.
pub fn managed_config_synced_at(home: &Path) -> Option<u64> {
    read_managed_config_cache(home)?.synced_at
}

/// Atomic write of the marker; best-effort (failure is logged, never returned).
fn write_marker_atomically(home: &Path, json: &str) {
    if let Err(e) =
        crate::fs_atomic::write_atomically(&home.join(MANAGED_CONFIG_CACHE_FILE), json, None)
    {
        tracing::warn!("failed to write managed config cache: {e}");
    }
}

/// Whether fail-closed managed policy is armed on disk for `home`.
/// An unreadable file cannot be confirmed disarmed, so `clear_orphan` must not wipe.
/// False only when neither the marker nor the file indicates fail_closed (including when the file is absent with `NotFound`).
pub fn fail_closed_policy_armed_at(home: &Path) -> bool {
    if read_managed_config_cache(home).is_some_and(|c| c.fail_closed) {
        return true;
    }
    // Defense in depth: files remain after a stripped/corrupt marker.
    match std::fs::read_to_string(home.join(crate::loader::REQUIREMENTS_FILENAME)) {
        Ok(s) => prod_mc_cli_chat_proxy_types::fail_closed_flag_status(&s).is_enabled(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            // File present but unreadable: do not allow clear_orphan to wipe.
            tracing::warn!("requirements.toml unreadable; treating as fail_closed armed: {e}");
            true
        }
    }
}

/// The sync marker, or `None` if absent, unreadable, or corrupt.
/// An unreadable or corrupt marker is treated as absent: a read blip or torn write mustn't lock out a managed user.
/// Both cases are logged (a corruption that disarms isn't silent) and self-heal on the next sync.
fn read_managed_config_cache(home: &Path) -> Option<ManagedConfigCache> {
    let json = match std::fs::read_to_string(home.join(MANAGED_CONFIG_CACHE_FILE)) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("managed config cache unreadable; treating as no marker: {e}");
            return None;
        }
    };
    match serde_json::from_str(&json) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!(
                "managed config cache is corrupt; treating as no marker, next sync rewrites it: {e}"
            );
            None
        }
    }
}

/// Confirmed identity switch vs the marker (both sides of a dimension known and differing).
/// A missing marker, a blank value, or a pre-upgrade marker never counts.
pub fn managed_config_identity_changed_at(
    home: &Path,
    new_principal: Option<&str>,
    new_key_fingerprint: Option<&str>,
) -> bool {
    let Some(cache) = read_managed_config_cache(home) else {
        return false;
    };
    confirmed_switch(cache.principal.as_deref(), new_principal).is_some()
        || confirmed_switch(cache.key_fingerprint.as_deref(), new_key_fingerprint).is_some()
}

/// Present non-blank value, else `None` (blank/whitespace is "unknown", not a tenant); the value is returned untrimmed.
fn known(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

/// [`known`] then trim: the one normalization for storing or deriving an identity (whitespace is not identity).
/// Shared with the shell's identity derivation.
pub fn normalize_identity(value: Option<&str>) -> Option<String> {
    known(value).map(|v| v.trim().to_owned())
}

/// Both sides known and differing after trim (older markers may be untrimmed); returns the recorded value.
fn confirmed_switch<'a>(recorded: Option<&'a str>, current: Option<&str>) -> Option<&'a str> {
    match (known(recorded), known(current)) {
        (Some(old), Some(new)) if old.trim() != new.trim() => Some(old),
        _ => None,
    }
}

/// Offline tenant-purge detector: a confirmed team switch vs the marker returns the evicted principal.
/// Key-scoped markers never confirm (key owns the machine's policy, not the team).
pub fn confirmed_team_switch(new_team_id: &str) -> Option<String> {
    user_distill_home().and_then(|home| confirmed_team_switch_at(&home, new_team_id))
}

/// [`confirmed_team_switch`] for an explicit `home` (purge-lock holder: same dir as delete).
pub fn confirmed_team_switch_at(home: &Path, new_team_id: &str) -> Option<String> {
    let cache = read_managed_config_cache(home)?;
    if known(cache.key_fingerprint.as_deref()).is_some() {
        return None;
    }
    confirmed_switch(cache.principal.as_deref(), Some(new_team_id)).map(str::to_owned)
}

/// True when an artifact the marker recorded serving is now absent.
/// Only served artifacts count, so a config-less principal (or legacy marker) isn't misread as stale.
/// Detects deletion, not edits.
fn cache_missing_required_artifact(cache: &ManagedConfigCache, home: &Path) -> bool {
    use crate::signed_policy::policy_file_has_content;
    (cache.had_requirements && !policy_file_has_content(home, crate::loader::REQUIREMENTS_FILENAME))
        || (cache.had_managed_config
            && !policy_file_has_content(home, crate::loader::MANAGED_CONFIG_FILENAME))
}

/// Whether the cached principal differs from the team serving now, checking the team dimension only.
/// Deploy-key identity is verified by fingerprint ([`cache_key_fingerprint_mismatch`]); `None` never fires.
/// Trim-aware (same rule as marker write): whitespace alone is not a mismatch.
fn cache_identity_mismatch(cache: &ManagedConfigCache, identity: &ServingIdentity) -> bool {
    match identity {
        ServingIdentity::Team(team_id) => match (
            known(cache.principal.as_deref()),
            known(Some(team_id.as_str())),
        ) {
            // Both blank: no team to compare
            (None, None) => false,
            (Some(a), Some(b)) => a.trim() != b.trim(),
            // One-sided: treat as mismatch (first install or a cleared principal field)
            _ => true,
        },
        ServingIdentity::DeploymentKey { .. } | ServingIdentity::None => false,
    }
}

/// Whether the configured deployment key differs from the cache's, by one-way fingerprint (never the raw key), the only identity verifiable offline.
/// A pre-upgrade marker (no fingerprint) never fires; only a *changed* key does.
/// Trim-aware; both sides must be known (unlike the team principal path).
fn cache_key_fingerprint_mismatch(cache: &ManagedConfigCache, identity: &ServingIdentity) -> bool {
    match identity {
        ServingIdentity::DeploymentKey { fingerprint } => {
            confirmed_switch(cache.key_fingerprint.as_deref(), Some(fingerprint.as_str())).is_some()
        }
        ServingIdentity::Team(_) | ServingIdentity::None => false,
    }
}

/// The team id for the signed-cache check; `None` for a deployment key (bound by the marker's deployment id, not a team) or no identity.
fn serving_team_id(identity: &ServingIdentity) -> Option<&str> {
    match identity {
        ServingIdentity::Team(team_id) => Some(team_id.as_str()),
        ServingIdentity::DeploymentKey { .. } | ServingIdentity::None => None,
    }
}

/// Tamper signals for the current identity, split two ways: [`Self::needs_refetch`] (staleness) fires on ANY signal.
/// [`Self::compromised_for_gate`] (gate) fires only on artifact-missing or key-change.
/// A pure identity mismatch never compromises the gate: a foreign marker is rebound by the online refetch.
#[derive(Clone, Copy)]
struct TamperSignals {
    artifact_missing: bool,
    identity_mismatch: bool,
    key_fingerprint_mismatch: bool,
}

impl TamperSignals {
    fn evaluate(cache: &ManagedConfigCache, home: &Path, identity: &ServingIdentity) -> Self {
        Self {
            artifact_missing: cache_missing_required_artifact(cache, home),
            identity_mismatch: cache_identity_mismatch(cache, identity),
            key_fingerprint_mismatch: cache_key_fingerprint_mismatch(cache, identity),
        }
    }

    fn needs_refetch(self) -> bool {
        self.artifact_missing || self.identity_mismatch || self.key_fingerprint_mismatch
    }

    fn compromised_for_gate(self) -> bool {
        self.artifact_missing || self.key_fingerprint_mismatch
    }
}

/// Cache unusable now: different identity, a served artifact missing, or no marker.
/// The session-start refresh blocks (bounded) on this but not timer-staleness, so a present same-identity cache never delays startup offline.
pub fn is_managed_config_hard_stale_for(identity: &ServingIdentity) -> bool {
    match user_distill_home() {
        Some(home) => is_managed_config_hard_stale_for_at(&home, identity),
        None => false,
    }
}

/// Whether the cache can't be used for `identity`: a served artifact missing or a different identity.
/// Shared by the staleness and session-start paths so the siblings can't drift.
fn cache_unusable_for(cache: &ManagedConfigCache, home: &Path, identity: &ServingIdentity) -> bool {
    TamperSignals::evaluate(cache, home, identity).needs_refetch()
}

/// The principal the SIGNED cache must be bound to: the live team id, else the marker principal.
/// On a deployment-key machine the marker principal is the recorded deployment id.
/// One derivation shared by the gate and both staleness checks, so a foreign-but-authentic cache reads foreign on every sibling path.
fn expected_signed_principal<'a>(
    cache: Option<&'a ManagedConfigCache>,
    identity: &'a ServingIdentity,
) -> Option<&'a str> {
    serving_team_id(identity).or_else(|| cache.and_then(|c| c.principal.as_deref()))
}

/// At-rest signed checks: `max(wall clock, floor)`.
/// Fetch-time verify stays unclamped so a fresh envelope can reset an inflated floor (see shell `verify_signed_envelope`).
fn effective_now(cache: Option<&ManagedConfigCache>) -> u64 {
    crate::signed_policy::now_unix().max(cache.map_or(0, |c| c.rollback_floor))
}

/// A signing-enabled build refetches a signed copy over a legacy unsigned, edited, forged, or foreign-bound cache.
/// These are the states the gate refuses on, so refusal always comes with a pending self-heal.
fn signed_cache_needs_refetch(
    home: &Path,
    cache: Option<&ManagedConfigCache>,
    identity: &ServingIdentity,
) -> bool {
    let expected_principal = expected_signed_principal(cache, identity);
    let now = effective_now(cache);
    // Verdict match first: Trusted short-circuits the claim's read and verify
    crate::signed_policy::cloud_cache_signature_invalid(home, expected_principal, now)
        || (matches!(
            crate::signed_policy::signed_cache_compromised(home, expected_principal, now),
            crate::signed_policy::SignedVerdict::NoAuthenticSidecar
                | crate::signed_policy::SignedVerdict::SidecarUnreadable
        ) && crate::signed_policy::managed_identity_claim_imposes(
            home,
            expected_principal,
            now,
        ))
}

fn is_managed_config_hard_stale_for_at(home: &Path, identity: &ServingIdentity) -> bool {
    let cache = read_managed_config_cache(home);
    cache
        .as_ref()
        .is_none_or(|cache| cache_unusable_for(cache, home, identity))
        || signed_cache_needs_refetch(home, cache.as_ref(), identity)
}

/// No-network fail-closed predicate: true only on a `fail_closed` policy with tamper for the current identity.
/// With a key compiled in, the SIGNED verdict leads: the opt-in is non-forgeable and catches edits the marker can't.
/// A fail-closed marker then REQUIRES an authentic sidecar.
pub fn managed_policy_compromised_for(identity: &ServingIdentity) -> bool {
    user_distill_home().is_some_and(|home| managed_policy_compromised_for_at(&home, identity))
}

// No retry: the gate reads this under the flock the apply holds across its write sequence.
fn managed_policy_compromised_for_at(home: &Path, identity: &ServingIdentity) -> bool {
    let cache = read_managed_config_cache(home);
    let expected_principal = expected_signed_principal(cache.as_ref(), identity);
    let now = effective_now(cache.as_ref());
    let signed_verdict =
        crate::signed_policy::signed_cache_compromised(home, expected_principal, now);
    // The signature binds a deployment_id, not the local deploy key, so a Trusted verdict can't attest the configured key
    // Pass the fingerprint mismatch through so it gates on every path
    let key_fingerprint_mismatch = cache
        .as_ref()
        .is_some_and(|c| cache_key_fingerprint_mismatch(c, identity));
    managed_policy_compromised_decision(
        signed_verdict,
        || crate::signed_policy::managed_identity_claim_imposes(home, expected_principal, now),
        key_fingerprint_mismatch,
        cache.as_ref(),
        home,
        identity,
    )
}

/// Combine the signed verdict with the best-effort marker fallback, one row per verdict.
/// `claim_imposes` ([`crate::signed_policy::managed_identity_claim_imposes`]) is consulted lazily, only on `NoAuthenticSidecar`.
/// Stripping the policy sidecar (even with a forged marker) cannot downgrade a claimed fail-closed principal.
fn managed_policy_compromised_decision(
    signed_verdict: crate::signed_policy::SignedVerdict,
    claim_imposes: impl FnOnce() -> bool,
    key_fingerprint_mismatch: bool,
    cache: Option<&ManagedConfigCache>,
    home: &Path,
    identity: &ServingIdentity,
) -> bool {
    use crate::signed_policy::SignedVerdict;
    // A fail-closed marker that recorded served policy requires an authentic sidecar.
    let sidecar_required_but_missing = || {
        let required =
            cache.is_some_and(|c| c.fail_closed && (c.had_managed_config || c.had_requirements));
        if required {
            tracing::warn!(
                "managed policy fail-closed gate: refusing session — signed sidecar missing or unverifiable"
            );
        }
        required
    };
    // The best-effort marker decision: refuse only an opted-in marker whose tamper counts for the gate (`compromised_for_gate`)
    let marker_compromised = || {
        cache.is_some_and(|cache| {
            if !cache.fail_closed {
                return false;
            }
            let signals = TamperSignals::evaluate(cache, home, identity);
            let compromised = signals.compromised_for_gate();
            // Booleans only, never the raw key (the fingerprint is already a one-way hash)
            if compromised {
                tracing::warn!(
                    artifact_missing = signals.artifact_missing,
                    identity_mismatch = signals.identity_mismatch,
                    key_fingerprint_mismatch = signals.key_fingerprint_mismatch,
                    "managed policy fail-closed gate: refusing session on tamper evidence"
                );
            } else if signals.identity_mismatch {
                tracing::debug!(
                    identity_mismatch = true,
                    "managed policy fail-closed gate: foreign marker, not refusing (online refetch rebinds)"
                );
            }
            compromised
        })
    };
    match signed_verdict {
        SignedVerdict::Compromised => true,
        // Trusted clears the gate except for the deploy-key fingerprint, which the signature can't attest
        SignedVerdict::Trusted => key_fingerprint_mismatch && marker_compromised(),
        SignedVerdict::NoAuthenticSidecar => {
            let refused = claim_imposes();
            if refused {
                tracing::warn!(
                    "managed policy fail-closed gate: refusing session — the signed is-managed \
                     claim requires an authentic policy sidecar and none is present"
                );
            }
            refused || sidecar_required_but_missing() || marker_compromised()
        }
        SignedVerdict::SidecarUnreadable => marker_compromised(),
        SignedVerdict::Inactive => marker_compromised(),
    }
}

/// Same-machine marker: more than a few minutes of future skew is not genuine.
const MAX_FUTURE_SYNCED_AT_SKEW: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Stale when never synced, past the threshold, identity differs, or a served artifact is now missing.
/// In keyed builds, also stale when the signed cache no longer verifies.
/// No home means nothing to refresh into, so not stale.
fn managed_config_stale_at(home: Option<&Path>, identity: &ServingIdentity) -> bool {
    let Some(home) = home else {
        return false;
    };
    let Some(cache) = read_managed_config_cache(home) else {
        return true; // no marker means never synced, so stale
    };
    if cache_unusable_for(&cache, home, identity) {
        return true;
    }
    // Same signed check as the session-start hard-stale sibling
    // The background tick must also refetch a tampered or foreign-signed cache, not leave it until startup
    if signed_cache_needs_refetch(home, Some(&cache), identity) {
        return true;
    }
    match cache.synced_at {
        Some(secs) => {
            // Age is measured against `effective_now` (max of wall clock and floor)
            // Repeated small rollbacks or a halted clock cannot keep age under the threshold forever
            // u64 seconds avoid SystemTime overflow panics for out-of-range timestamps.
            let now = effective_now(Some(&cache));
            let age = now.saturating_sub(secs);
            let skew = secs.saturating_sub(now);
            age > managed_config_stale_threshold().as_secs()
                || skew > MAX_FUTURE_SYNCED_AT_SKEW.as_secs()
        }
        None => true,
    }
}

/// Override with `GROK_DEPLOYMENT_CONFIG_CACHE_TTL_SECS` for testing.
fn managed_config_stale_threshold() -> std::time::Duration {
    if let Ok(s) = std::env::var("GROK_DEPLOYMENT_CONFIG_CACHE_TTL_SECS")
        && let Ok(secs) = s.parse::<u64>()
    {
        return std::time::Duration::from_secs(secs);
    }
    std::time::Duration::from_secs(30 * 60)
}

// Tests in a sibling file (they dwarf the module) but a child module, for private access.
#[cfg(test)]
#[path = "managed_cache/tests.rs"]
mod tests;
