//! Ed25519-signed, identity-bound managed-policy envelope.
//!
//! The server signs policy, principal, and expiry.
//! The client verifies against a compiled-in key set (by signed `key_id`), binds the principal, and checks the on-disk bytes match.
//! This build has the prod `v1` key compiled in; keyless (`&[]`) keeps the cache marker as authority.

use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;

// Shared wire types with the deployment-config server: a field rename breaks compile on both sides.
pub use prod_mc_cli_chat_proxy_types::{
    MANAGED_CONFIG_NONCE_ECHO_HEADER, MANAGED_IDENTITY_TYP, MANAGED_POLICY_TYP,
    ManagedIdentityClaim, SignatureEnvelope, SignedPayload, is_server_nonce_shape, now_unix,
};

/// Compiled-in trusted keys `(key_id, raw 32 bytes)`. Prod `v1`. Empty means dark (no verification).
/// The private signing key never lives in this crate or in client env flags.
/// Ship only after the server is emitting valid envelopes for this key id.
pub const EMBEDDED_DEPLOYMENT_CONFIG_PUBKEYS: &[(&str, &[u8])] = &[(
    "v1",
    &[
        7, 19, 246, 115, 22, 145, 35, 57, 97, 197, 75, 234, 154, 92, 253, 123, 247, 72, 5, 229,
        151, 231, 195, 248, 194, 17, 22, 210, 193, 107, 119, 50,
    ],
)];

/// SHA-256 of the raw `v1` pubkey (hex); tests pin the byte array against it to catch silent typos.
pub const EMBEDDED_V1_PUBKEY_SHA256_HEX: &str =
    "fb4dcc77c757465b953265146d495166527fcc1c2b365352f8d20c3d8f6de620";

// Compile-time sanity for the key set.
// `slice::get` is not const-stable, so the const checks walk with `split_first`.
const _: () = {
    let mut rest = EMBEDDED_DEPLOYMENT_CONFIG_PUBKEYS;
    while let Some((key, tail)) = rest.split_first() {
        assert!(
            key.1.len() == 32,
            "every embedded key must be exactly 32 raw Ed25519 bytes"
        );
        assert!(!key.0.is_empty(), "every embedded key id must be non-empty");
        let mut others = tail;
        while let Some((other, more)) = others.split_first() {
            assert!(
                !const_str_eq(key.0, other.0),
                "embedded key ids must be unique"
            );
            others = more;
        }
        rest = tail;
    }
};

const fn const_str_eq(a: &str, b: &str) -> bool {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    while let (Some((x, a_tail)), Some((y, b_tail))) = (a.split_first(), b.split_first()) {
        if *x != *y {
            return false;
        }
        a = a_tail;
        b = b_tail;
    }
    true
}

/// Debug-only key override for tests (`test` or `test-signing-seam`); never in release builds.
#[cfg(all(test, debug_assertions))]
pub mod test_seam {
    use std::cell::RefCell;
    use std::sync::RwLock;

    /// Owned key list: `(key_id, raw 32-byte pubkey)`.
    type OwnedKeys = Vec<(String, Vec<u8>)>;
    /// `None` means compiled-in keys; `Some([])` means dark; `Some(non-empty)` means override.
    type KeyOverride = Option<OwnedKeys>;

    // Process override.
    pub(super) static GLOBAL_OVERRIDE: RwLock<KeyOverride> = RwLock::new(None);

    // Thread-local override (unit tests; avoids races on the global override)
    // Outer `Option`: unset vs set on this thread. Inner is [`KeyOverride`].
    thread_local! {
        static LOCAL_OVERRIDE: RefCell<Option<KeyOverride>> = const { RefCell::new(None) };
    }

    fn to_owned_keys(keys: Option<&[(&str, &[u8])]>) -> KeyOverride {
        keys.map(|ks| {
            ks.iter()
                .map(|(id, key)| ((*id).to_owned(), key.to_vec()))
                .collect()
        })
    }

    /// Set the process-wide keys: `None` clears the override, `Some(&[])` goes dark, anything else overrides.
    pub fn set_embedded_keys(keys: Option<&[(&str, &[u8])]>) {
        *GLOBAL_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = to_owned_keys(keys);
    }

    /// Runs `f` with dark keys on this thread only.
    pub fn with_dark<R>(f: impl FnOnce() -> R) -> R {
        LOCAL_OVERRIDE.with(|cell| {
            let prev = cell.replace(Some(Some(Vec::new())));
            struct Restore(Option<KeyOverride>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    let prev = self.0.take();
                    LOCAL_OVERRIDE.with(|cell| {
                        *cell.borrow_mut() = prev;
                    });
                }
            }
            let _restore = Restore(prev);
            f()
        })
    }

    pub(super) fn with_override<R>(f: impl FnOnce(Option<&[(String, Vec<u8>)]>) -> R) -> R {
        if let Some(local) = LOCAL_OVERRIDE.with(|c| c.borrow().clone()) {
            f(local.as_deref())
        } else {
            let global = GLOBAL_OVERRIDE.read().unwrap_or_else(|e| e.into_inner());
            f(global.as_deref())
        }
    }
}

fn with_embedded_keys<R>(f: impl FnOnce(&[(&str, &[u8])]) -> R) -> R {
    #[cfg(all(test, debug_assertions))]
    {
        test_seam::with_override(|overridden| match overridden {
            Some(keys) => {
                let view: Vec<(&str, &[u8])> = keys
                    .iter()
                    .map(|(id, key)| (id.as_str(), key.as_slice()))
                    .collect();
                f(&view)
            }
            None => f(EMBEDDED_DEPLOYMENT_CONFIG_PUBKEYS),
        })
    }
    #[cfg(not(all(test, debug_assertions)))]
    {
        f(EMBEDDED_DEPLOYMENT_CONFIG_PUBKEYS)
    }
}

/// Sidecar persisted next to the policy so the load-time gate can re-verify it offline.
pub const SIGNATURE_SIDECAR_FILE: &str = "managed_config.sig.json";

/// The is-managed claim's own sidecar (see [`ManagedIdentityClaim`]).
pub const MANAGED_IDENTITY_SIDECAR_FILE: &str = "managed_identity.sig.json";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SigError {
    #[error("signature is not valid base64")]
    BadSignatureEncoding,
    #[error("signature does not verify against the provided public key")]
    SignatureMismatch,
    #[error("signed payload is not valid JSON")]
    BadPayload,
    #[error("signed payload carries the wrong message type")]
    WrongType,
    #[error("signed payload names a key_id outside the trusted set")]
    UnknownKeyId,
    #[error("signed policy is bound to a different principal")]
    PrincipalMismatch,
    #[error("signed policy has expired")]
    Expired,
    #[error("on-disk {0} does not match the signed policy")]
    ContentMismatch(&'static str),
    /// The file exists but can't be read (EACCES etc., never plain absence).
    /// Not tamper evidence: callers refetch but don't refuse on a read blip.
    #[error("on-disk {0} cannot be read")]
    Unreadable(&'static str),
}

/// Remote kill-switch; set only from authenticated remote settings.
static REMOTE_VERIFICATION_DISARMED: AtomicBool = AtomicBool::new(false);

/// True when keys are embedded and the remote kill-switch has not disarmed.
pub fn verification_active() -> bool {
    if REMOTE_VERIFICATION_DISARMED.load(Ordering::Relaxed) {
        return false;
    }
    with_embedded_keys(|keys| !keys.is_empty())
}

/// Apply remote `managed_config_signature_verification`.
/// `Some(false)` disarms only when `settings_origin_trusted` is true **or** no keys are embedded (dark: disarm is a no-op for enforcement). An untrusted origin (env-overridden proxy) cannot disarm a keyed client; that would make the kill-switch an env toggle; `None` / `Some(true)` re-arm always (stronger / default).
/// Call only when settings were successfully fetched. Logs on state change.
pub fn apply_remote_managed_config_signature_verification(
    setting: Option<bool>,
    settings_origin_trusted: bool,
) {
    let want_disarm = setting == Some(false);
    let keys_embedded = with_embedded_keys(|keys| !keys.is_empty());
    if want_disarm && keys_embedded && !settings_origin_trusted {
        tracing::warn!(
            "ignoring managed_config_signature_verification=false from untrusted settings origin"
        );
        return;
    }
    let disarm = want_disarm;
    let prev = REMOTE_VERIFICATION_DISARMED.swap(disarm, Ordering::Relaxed);
    if prev != disarm {
        tracing::warn!(
            disarmed = disarm,
            "managed-config signature verification kill-switch changed"
        );
    }
}

/// Whether `key_id` names a trusted key.
/// Only PICKS among served envelopes; verification re-selects the key from the signed bytes, so a lying hint at most causes a verification failure.
pub fn embedded_key_id_trusted(key_id: &str) -> bool {
    with_embedded_keys(|keys| keys.iter().any(|(id, _)| *id == key_id))
}

/// Verify `signature_b64` over `signed_payload` against `trusted_keys`, returning the parsed payload.
/// The verifying key is selected by the SIGNED payload's `key_id`; reading it pre-verification is safe because selection stays in the trusted set.
/// Requires the [`MANAGED_POLICY_TYP`] tag (a claim must never verify as a policy).
pub fn verify_signed_payload(
    signed_payload: &str,
    signature_b64: &str,
    trusted_keys: &[(&str, &[u8])],
) -> Result<SignedPayload, SigError> {
    let payload: SignedPayload =
        serde_json::from_str(signed_payload).map_err(|_| SigError::BadPayload)?;
    verify_signature_with_keys(signed_payload, signature_b64, trusted_keys, &payload.key_id)?;
    if payload.typ != MANAGED_POLICY_TYP {
        return Err(SigError::WrongType);
    }
    Ok(payload)
}

/// [`verify_signed_payload`]'s mirror for claims (requires [`MANAGED_IDENTITY_TYP`]).
pub fn verify_managed_identity_claim(
    signed_payload: &str,
    signature_b64: &str,
    trusted_keys: &[(&str, &[u8])],
) -> Result<ManagedIdentityClaim, SigError> {
    let claim: ManagedIdentityClaim =
        serde_json::from_str(signed_payload).map_err(|_| SigError::BadPayload)?;
    verify_signature_with_keys(signed_payload, signature_b64, trusted_keys, &claim.key_id)?;
    if claim.typ != MANAGED_IDENTITY_TYP {
        return Err(SigError::WrongType);
    }
    Ok(claim)
}

/// Shared Ed25519 check: select the trusted key named by the signed bytes' `key_id`, verify.
fn verify_signature_with_keys(
    signed_payload: &str,
    signature_b64: &str,
    trusted_keys: &[(&str, &[u8])],
    key_id: &str,
) -> Result<(), SigError> {
    let (_, public_key) = trusted_keys
        .iter()
        .find(|(id, _)| *id == key_id)
        .ok_or(SigError::UnknownKeyId)?;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|_| SigError::BadSignatureEncoding)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(signed_payload.as_bytes(), &sig)
        .map_err(|_| SigError::SignatureMismatch)
}

/// Fetch-time identity binding for a VERIFIED payload, expiry enforced.
/// A deployment-signed payload is trusted on signature alone; a team-signed payload must match the active team.
/// Lenient on a missing active team: an `auth.json` read blip must not brick a session (a cross-team attacker has a team of their own).
pub fn check_fetch_identity(
    payload: &SignedPayload,
    active_team_id: Option<&str>,
    now_unix: u64,
) -> Result<(), SigError> {
    if now_unix > payload.expires_at {
        return Err(SigError::Expired);
    }
    if payload.deployment_id.is_some() {
        return Ok(());
    }
    if let (Some(signed), Some(active)) = (payload.team_id.as_deref(), active_team_id)
        && signed != active
    {
        return Err(SigError::PrincipalMismatch);
    }
    Ok(())
}

/// Whether the payload's effective principal (`deployment_id`, else `team_id`) matches ours.
/// This is the at-rest identity rule, so another tenant's cache reads foreign. Lenient when either side is unknown.
/// Deliberately expiry-free: the gate orders identity BEFORE the `fail_closed` short-circuit and expiry after it (see [`SignedCacheFacts`]).
fn signed_principal_matches(payload: &SignedPayload, expected_principal: Option<&str>) -> bool {
    let signed = payload
        .deployment_id
        .as_deref()
        .or(payload.team_id.as_deref());
    !matches!(
        (signed, expected_principal),
        (Some(signed), Some(expected)) if signed != expected
    )
}

/// Full verification of a fetched envelope against the embedded trusted keys (signature, binding, expiry), returning the trusted payload to persist.
pub fn verify_fetched(
    sidecar: &SignatureEnvelope,
    active_team_id: Option<&str>,
    now_unix: u64,
) -> Result<SignedPayload, SigError> {
    with_embedded_keys(|keys| verify_fetched_with_keys(sidecar, keys, active_team_id, now_unix))
}

/// Fetch-time claim verification (signature and expiry; binding is the caller's rule).
pub fn verify_fetched_claim(
    sidecar: &SignatureEnvelope,
    now_unix: u64,
) -> Result<ManagedIdentityClaim, SigError> {
    with_embedded_keys(|keys| verify_fetched_claim_with_keys(sidecar, keys, now_unix))
}

/// Key-injected core of [`verify_fetched_claim`] so tests can supply throwaway keys.
fn verify_fetched_claim_with_keys(
    sidecar: &SignatureEnvelope,
    trusted_keys: &[(&str, &[u8])],
    now_unix: u64,
) -> Result<ManagedIdentityClaim, SigError> {
    let claim =
        verify_managed_identity_claim(&sidecar.signed_payload, &sidecar.signature, trusted_keys)?;
    if now_unix > claim.expires_at {
        return Err(SigError::Expired);
    }
    Ok(claim)
}

/// Key-injected core of [`verify_fetched`] so tests can supply throwaway keypairs.
fn verify_fetched_with_keys(
    sidecar: &SignatureEnvelope,
    trusted_keys: &[(&str, &[u8])],
    active_team_id: Option<&str>,
    now_unix: u64,
) -> Result<SignedPayload, SigError> {
    let payload = verify_signed_payload(&sidecar.signed_payload, &sidecar.signature, trusted_keys)?;
    check_fetch_identity(&payload, active_team_id, now_unix)?;
    Ok(payload)
}

/// True when something occupies `path` that is not a regular file (directory, symlink, fifo, …).
/// A squatter blocks or redirects reads/rewrites, which is tamper, never a blip.
fn non_regular_file_at(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file())
}

/// Confirm the on-disk artifacts match the signed payload byte-for-byte: an in-place edit is caught, not just a deletion.
/// A signed-ABSENT slot must be empty on disk: a locally planted `requirements.toml` (the highest-precedence layer) is tamper, not noise.
/// An unreadable file is [`SigError::Unreadable`] (a read blip: refetch, don't refuse).
pub fn check_on_disk_matches(
    home: &std::path::Path,
    payload: &SignedPayload,
) -> Result<(), SigError> {
    for (name, label, signed) in [
        (
            "managed_config.toml",
            "managed_config",
            payload.managed_config.as_deref(),
        ),
        (
            "requirements.toml",
            "requirements",
            payload.requirements.as_deref(),
        ),
    ] {
        let path = home.join(name);
        if non_regular_file_at(&path) {
            return Err(SigError::ContentMismatch(label));
        }
        let on_disk = match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(SigError::Unreadable(label)),
        };
        let matches = match signed.filter(|s| !s.is_empty()) {
            Some(signed) => on_disk.as_deref() == Some(signed),
            None => on_disk.as_deref().is_none_or(str::is_empty),
        };
        if !matches {
            return Err(SigError::ContentMismatch(label));
        }
    }
    Ok(())
}

pub(crate) fn sidecar_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(SIGNATURE_SIDECAR_FILE)
}

/// Outcome of reading the on-disk sidecar; mirrors the artifact-slot rules of [`check_on_disk_matches`].
enum SidecarRead {
    Present(SignatureEnvelope),
    /// NotFound, unparseable JSON, or a squatting non-regular file (directory, symlink, …): not an authentic sidecar.
    Absent,
    /// EACCES-style transient failure on a regular file, not tamper evidence.
    /// The gate must not refuse on it, but the refetch trigger fires to self-heal.
    Unreadable,
}

fn read_sidecar(home: &std::path::Path) -> SidecarRead {
    read_envelope_at(&sidecar_path(home))
}

fn read_envelope_at(path: &std::path::Path) -> SidecarRead {
    if non_regular_file_at(path) {
        return SidecarRead::Absent;
    }
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SidecarRead::Absent,
        Err(_) => return SidecarRead::Unreadable,
    };
    match serde_json::from_str(&json) {
        Ok(sidecar) => SidecarRead::Present(sidecar),
        Err(_) => SidecarRead::Absent,
    }
}

/// Persist the sidecar atomically; a torn sidecar would fail the load-time gate.
/// Written 0600 on unix: a deployment-key principal's signed payload embeds the key, so the sidecar is a second at-rest copy of a bearer credential.
pub fn write_sidecar(home: &std::path::Path, sidecar: &SignatureEnvelope) -> std::io::Result<()> {
    write_envelope_at(&sidecar_path(home), sidecar)
}

pub(crate) fn managed_identity_sidecar_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(MANAGED_IDENTITY_SIDECAR_FILE)
}

/// [`write_sidecar`] for the claim (0600 for uniformity; the claim has no secret).
pub fn write_managed_identity_sidecar(
    home: &std::path::Path,
    sidecar: &SignatureEnvelope,
) -> std::io::Result<()> {
    write_envelope_at(&managed_identity_sidecar_path(home), sidecar)
}

fn write_envelope_at(path: &std::path::Path, sidecar: &SignatureEnvelope) -> std::io::Result<()> {
    let json = serde_json::to_string(sidecar)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::fs_atomic::write_atomically(path, &json, Some(0o600))
}

/// Persisted envelope nonce for [`MANAGED_CONFIG_NONCE_ECHO_HEADER`] (unverified; telemetry only, never a trust input).
/// Both guards fail open by skipping the echo.
/// Only a nonce with the server's mint shape is echoed (header-safe: a corrupt sidecar can't brick the fetch).
pub fn stored_envelope_nonce(
    home: &std::path::Path,
    fetch_principal: Option<&str>,
) -> Option<String> {
    let fetch_principal = fetch_principal?;
    let SidecarRead::Present(sidecar) = read_sidecar(home) else {
        return None;
    };
    let payload: SignedPayload = serde_json::from_str(&sidecar.signed_payload).ok()?;
    // Effective principal mirrors the server's bookkeeping: deployment over team.
    let issued_to = payload
        .deployment_id
        .as_deref()
        .or(payload.team_id.as_deref());
    (issued_to == Some(fetch_principal) && is_server_nonce_shape(&payload.nonce))
        .then_some(payload.nonce)
}

/// Whether an authentic claim IMPOSES fail-closed enforcement.
/// Anything else imposes nothing: permissive (must not override a now-fail_closed marker), foreign, expired, forged, or absent.
/// An unknown principal also imposes nothing: a planted claim must not brick a signed-out victim.
pub fn managed_identity_claim_imposes(
    home: &std::path::Path,
    expected_principal: Option<&str>,
    now_unix: u64,
) -> bool {
    if !verification_active() {
        return false;
    }
    with_embedded_keys(|keys| {
        managed_identity_claim_imposes_with_keys(home, keys, expected_principal, now_unix)
    })
}

/// Key-injected core of [`managed_identity_claim_imposes`] so tests can supply throwaway keys.
fn managed_identity_claim_imposes_with_keys(
    home: &std::path::Path,
    trusted_keys: &[(&str, &[u8])],
    expected_principal: Option<&str>,
    now_unix: u64,
) -> bool {
    let Some(expected) = expected_principal else {
        return false;
    };
    let SidecarRead::Present(sidecar) = read_envelope_at(&managed_identity_sidecar_path(home))
    else {
        return false;
    };
    let Ok(claim) =
        verify_managed_identity_claim(&sidecar.signed_payload, &sidecar.signature, trusted_keys)
    else {
        return false;
    };
    claim.principal == expected && now_unix <= claim.expires_at && claim.fail_closed
}

/// Empty write-deny placeholders must not count as served policy.
pub(crate) fn policy_file_has_content(home: &std::path::Path, filename: &str) -> bool {
    match std::fs::read_to_string(home.join(filename)) {
        Ok(s) => !s.trim().is_empty(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// True when signature verification is active AND a cloud-cache policy on disk lacks a valid, in-date, identity-bound, content-matching signature.
/// A keyless build or no policy on disk returns false.
pub fn cloud_cache_signature_invalid(
    home: &std::path::Path,
    expected_principal: Option<&str>,
    now_unix: u64,
) -> bool {
    if !verification_active() {
        return false;
    }
    with_embedded_keys(|keys| {
        cloud_cache_signature_invalid_with_keys(home, keys, expected_principal, now_unix)
    })
}

/// Key-injected core of [`cloud_cache_signature_invalid`] so tests can supply throwaway keys.
fn cloud_cache_signature_invalid_with_keys(
    home: &std::path::Path,
    trusted_keys: &[(&str, &[u8])],
    expected_principal: Option<&str>,
    now_unix: u64,
) -> bool {
    let has_policy = policy_file_has_content(home, crate::loader::REQUIREMENTS_FILENAME)
        || policy_file_has_content(home, crate::loader::MANAGED_CONFIG_FILENAME);
    if !has_policy {
        return false;
    }
    use SignedCacheEvaluation as Eval;
    match evaluate_signed_cache(home, trusted_keys, expected_principal, now_unix) {
        // ANY deviation refetches, including read blips: self-heal what the gate stays lenient on
        // A foreign-but-authentic cache also refetches; it would otherwise never rebind
        Eval::NoAuthenticSidecar | Eval::SidecarUnreadable => true,
        Eval::Facts(f) => !f.identity_ok || f.expired || f.disk != DiskStatus::Match,
    }
}

/// On-disk status of the signed artifact slots, from [`check_on_disk_matches`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiskStatus {
    /// Every slot matches the signed payload (content and absence).
    Match,
    /// Tamper: edited, deleted-while-signed, planted, or a squatting non-file.
    Mismatch,
    /// A read blip (EACCES on a regular file): stale for the refetch, lenient at the gate.
    Unreadable,
}

/// What one verification pass over the on-disk sidecar establishes.
/// The two public checks are projections over the same facts: the refetch trigger flags ANY deviation; the gate applies the fail-closed rules.
struct SignedCacheFacts {
    /// The payload's effective principal matches ours ([`signed_principal_matches`]).
    identity_ok: bool,
    expired: bool,
    /// The SIGNED opt-in, read from the payload, never the forgeable marker.
    fail_closed: bool,
    disk: DiskStatus,
}

/// One evaluation of the on-disk sidecar; both public checks project from this.
enum SignedCacheEvaluation {
    /// No authentic sidecar: missing, corrupt, a squatting non-file, forged, or keyed outside the trusted set; never facts from unverified bytes.
    NoAuthenticSidecar,
    /// The sidecar exists but a transient IO error blocked the read ([`SidecarRead::Unreadable`]); nothing verified, nothing that looks like tamper.
    SidecarUnreadable,
    Facts(SignedCacheFacts),
}

/// Read the sidecar, verify it against `trusted_keys`, reduce to a [`SignedCacheEvaluation`].
fn evaluate_signed_cache(
    home: &std::path::Path,
    trusted_keys: &[(&str, &[u8])],
    expected_principal: Option<&str>,
    now_unix: u64,
) -> SignedCacheEvaluation {
    let sidecar = match read_sidecar(home) {
        SidecarRead::Present(sidecar) => sidecar,
        SidecarRead::Absent => return SignedCacheEvaluation::NoAuthenticSidecar,
        SidecarRead::Unreadable => return SignedCacheEvaluation::SidecarUnreadable,
    };
    let Ok(payload) =
        verify_signed_payload(&sidecar.signed_payload, &sidecar.signature, trusted_keys)
    else {
        return SignedCacheEvaluation::NoAuthenticSidecar;
    };
    SignedCacheEvaluation::Facts(SignedCacheFacts {
        identity_ok: signed_principal_matches(&payload, expected_principal),
        expired: now_unix > payload.expires_at,
        fail_closed: payload.fail_closed,
        disk: match check_on_disk_matches(home, &payload) {
            Ok(()) => DiskStatus::Match,
            Err(SigError::Unreadable(_)) => DiskStatus::Unreadable,
            Err(_) => DiskStatus::Mismatch,
        },
    })
}

/// Verdict of the signed-sidecar check for the load-time gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedVerdict {
    /// Verification is not active (no embedded keys, the dark build): the marker is the only signal.
    /// A distinct variant, not an `Option`, so a dark build can never be confused with [`Self::NoAuthenticSidecar`].
    /// That variant's absence rule must never fire keyless.
    Inactive,
    /// No sidecar, or one whose signature doesn't verify: not an authentic verdict.
    /// Under a fail-closed marker that recorded served policy, absence is itself tamper.
    /// Stripping the sidecar must not downgrade enforcement to the forgeable marker path.
    NoAuthenticSidecar,
    /// The sidecar exists but a transient IO error (EACCES-style, never plain absence or a squatting non-file) blocked the read.
    /// Not tamper evidence: the gate falls back to the marker decision, and the refetch trigger fires to rewrite it.
    /// The claim is deliberately NOT consulted here: a genuine blip must not refuse, and a chmod-capable attacker could delete the claim anyway.
    SidecarUnreadable,
    /// Authentic sidecar; the policy is valid for this principal (or never opted into fail-closed enforcement).
    Trusted,
    /// Authentic sidecar proving an opted-in policy is no longer valid here: edited on disk, expired, or bound to a different principal.
    /// Refuse, always.
    Compromised,
}

/// The signed verdict for the on-disk cache; see [`SignedVerdict`].
/// The fail-closed opt-in is read from the SIGNED bytes, not the forgeable marker.
pub fn signed_cache_compromised(
    home: &std::path::Path,
    expected_principal: Option<&str>,
    now_unix: u64,
) -> SignedVerdict {
    if !verification_active() {
        return SignedVerdict::Inactive;
    }
    with_embedded_keys(|keys| {
        signed_cache_compromised_with_keys(home, keys, expected_principal, now_unix)
    })
}

/// Key-injected core of [`signed_cache_compromised`] so tests can supply throwaway keys.
fn signed_cache_compromised_with_keys(
    home: &std::path::Path,
    trusted_keys: &[(&str, &[u8])],
    expected_principal: Option<&str>,
    now_unix: u64,
) -> SignedVerdict {
    use SignedCacheEvaluation as Eval;
    match evaluate_signed_cache(home, trusted_keys, expected_principal, now_unix) {
        Eval::NoAuthenticSidecar => SignedVerdict::NoAuthenticSidecar,
        Eval::SidecarUnreadable => SignedVerdict::SidecarUnreadable,
        // Identity precedes the fail_closed short-circuit: a foreign-bound but permissive policy can't be replayed to escape a strict one offline
        Eval::Facts(f) if !f.identity_ok => SignedVerdict::Compromised,
        Eval::Facts(f) if !f.fail_closed => SignedVerdict::Trusted,
        // Opted-in and bound to us: expired or tampered-on-disk refuses; an Unreadable blip does not
        Eval::Facts(f) if f.expired || f.disk == DiskStatus::Mismatch => SignedVerdict::Compromised,
        Eval::Facts(_) => SignedVerdict::Trusted,
    }
}

// Tests live in a sibling file (they dwarf the module) but form a child module, for private access
#[cfg(test)]
#[path = "signed_policy/tests.rs"]
mod tests;
