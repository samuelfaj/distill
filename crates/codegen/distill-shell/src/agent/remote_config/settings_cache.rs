// Modified for Distill by Samuel Fajreldines, 2026.
//! Signed on-disk cache for remote settings.

use chrono::{DateTime, Utc};

use super::cache_file::{CacheLoadError, is_fresh, read_capped, write_atomic};

pub(crate) const SETTINGS_CACHE_FILE: &str = "settings_cache.json";
/// 1h: an offline cold start can boot on the last good policy, while
/// staleness stays bounded by the managed gate's own re-fetch and the
/// per-session settings reapply.
pub(crate) const SETTINGS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
const SETTINGS_CACHE_MAX_BYTES: u64 = 1 << 20;

/// Current signing key. To rotate: set this to the new key and keep the old
/// key in SETTINGS_CACHE_READ_HMAC_KEYS so already written caches still verify.
const SETTINGS_CACHE_WRITE_HMAC_KEY: &[u8] =
    b"grok-shell-settings-cache-hmac-v1-ba6c43d3-404f-4b5c-b0cd-df09b2f5bdf4";
/// Keys accepted on read, newest first: the write key plus any superseded key
/// retained for a rotation window.
const SETTINGS_CACHE_READ_HMAC_KEYS: &[&[u8]] = &[SETTINGS_CACHE_WRITE_HMAC_KEY];

/// The disk settings cache is off when `GROK_SETTINGS_CACHE=false`.
pub(in crate::agent::remote_config) fn settings_cache_disabled() -> bool {
    crate::agent::config::env_bool("GROK_SETTINGS_CACHE") == Some(false)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SettingsCache {
    fetched_at: DateTime<Utc>,
    grok_version: String,
    identity: String,
    origin: String,
    client: String,
    settings: crate::util::config::RemoteSettings,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SignedSettingsCache {
    payload: String,
    signature: Vec<u8>,
}

struct CachedSettings(crate::util::config::RemoteSettings);

impl CachedSettings {
    /// Fields only a live fetch may set: a disarm must not outlive the fetch that
    /// sent it, and a proxy may have stopped decoding since the cache was written.
    fn into_settings(self) -> crate::util::config::RemoteSettings {
        let mut settings = self.0;
        settings.managed_config_signature_verification = None;
        settings.accept_request_encodings.clear();
        settings
    }
}

pub(crate) struct SettingsCacheManager {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
}

impl SettingsCacheManager {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::util::distill_home::distill_home().join(SETTINGS_CACHE_FILE),
            ttl: SETTINGS_CACHE_TTL,
        }
    }

    pub(crate) fn load_or_fetch(
        &self,
        auth: &distill_login::GrokAuth,
        origin: &str,
        alpha_test_key: Option<&str>,
        fetch: impl FnOnce() -> Option<crate::util::config::RemoteSettings>,
    ) -> (
        Option<crate::util::config::RemoteSettings>,
        Option<SettingsCacheWrite>,
    ) {
        if settings_cache_disabled() {
            return (fetch(), None);
        }
        let identity = Self::identity(auth, alpha_test_key);
        if let Some(cached) = self.load_fresh(&identity, origin) {
            tracing::info!("settings cache hit");
            return (Some(cached.into_settings()), None);
        }
        tracing::info!("settings cache miss; fetching");
        let fetched_at = Utc::now();
        let Some(fetched) = fetch() else {
            return (None, None);
        };
        let write = SettingsCacheWrite {
            path: self.path.clone(),
            ttl: self.ttl,
            identity,
            origin: origin.to_string(),
            settings: fetched.clone(),
            fetched_at,
        };
        (Some(fetched), Some(write))
    }

    /// Persist a live fetch. `fetched_at` is the fetch-initiation time, used by
    /// the monotonic write. Does not read the cache for a value.
    pub(crate) fn write_through(
        &self,
        auth: &distill_login::GrokAuth,
        origin: &str,
        alpha_test_key: Option<&str>,
        settings: &crate::util::config::RemoteSettings,
        fetched_at: DateTime<Utc>,
    ) {
        if settings_cache_disabled() {
            return;
        }
        let identity = Self::identity(auth, alpha_test_key);
        self.persist(&identity, origin, settings, fetched_at);
    }

    /// Scope the cache to the stable account identity, not the rotating bearer
    /// token, so a token refresh between boots still hits the cache. `key` is a
    /// fallback only for keyless/API-key auth where `user_id` is empty. Issuer
    /// and auth mode are included so principals that share subject/team/org
    /// across different IdPs cannot consume each other's cache. Hashed
    /// (SHA-256, stable across releases) so no credential lands on disk.
    pub(in crate::agent::remote_config) fn identity(
        auth: &distill_login::GrokAuth,
        alpha_test_key: Option<&str>,
    ) -> String {
        use sha2::{Digest, Sha256};
        let principal = if auth.user_id.is_empty() {
            auth.key.as_str()
        } else {
            auth.user_id.as_str()
        };
        let auth_mode = match auth.auth_mode {
            distill_login::AuthMode::WebLogin => "web_login",
            distill_login::AuthMode::Oidc => "oidc",
            distill_login::AuthMode::External => "external",
            distill_login::AuthMode::ApiKey => "api_key",
        };
        let mut hasher = Sha256::new();
        for part in [
            principal,
            auth.team_id.as_deref().unwrap_or(""),
            auth.organization_id.as_deref().unwrap_or(""),
            auth.oidc_issuer.as_deref().unwrap_or(""),
            auth_mode,
            alpha_test_key.unwrap_or(""),
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Signature-verified read without scope or freshness checks. Shared by
    /// `load_fresh` and the monotonic write.
    fn load_raw(&self) -> Result<SettingsCache, CacheLoadError> {
        let data =
            read_capped(&self.path, SETTINGS_CACHE_MAX_BYTES).ok_or(CacheLoadError::NotFound)?;
        let signed: SignedSettingsCache =
            serde_json::from_slice(&data).map_err(|_| CacheLoadError::ParseFailed)?;
        if !verify_cache_signature(signed.payload.as_bytes(), &signed.signature) {
            return Err(CacheLoadError::SignatureInvalid);
        }
        serde_json::from_str(&signed.payload).map_err(|_| CacheLoadError::ParseFailed)
    }

    fn load_fresh(&self, identity: &str, origin: &str) -> Option<CachedSettings> {
        match self.try_load_fresh(identity, origin) {
            Ok(hit) => Some(hit),
            Err(status) => {
                status.log(&self.path);
                None
            }
        }
    }

    fn try_load_fresh(
        &self,
        identity: &str,
        origin: &str,
    ) -> Result<CachedSettings, CacheLoadError> {
        let cache = self.load_raw()?;
        if cache.grok_version != distill_version::VERSION {
            return Err(CacheLoadError::VersionMismatch);
        }
        if cache.identity != identity {
            return Err(CacheLoadError::ScopeMismatch("identity"));
        }
        if cache.origin != origin {
            return Err(CacheLoadError::ScopeMismatch("origin"));
        }
        if cache.client != crate::http::process_client_identifier() {
            return Err(CacheLoadError::ScopeMismatch("client"));
        }
        if !is_fresh(cache.fetched_at, self.ttl) {
            return Err(CacheLoadError::Stale);
        }
        Ok(CachedSettings(cache.settings))
    }

    /// Monotonic: never replace a newer same-scope entry with an older fetch, so
    /// a slow startup load cannot clobber a fresher live refresh. `fetched_at`
    /// is the fetch-initiation time, so an earlier request loses to a later one
    /// regardless of which completes first. A future-dated stored time (a corrupt
    /// file or clock rollback) does not pin the cache.
    fn persist(
        &self,
        identity: &str,
        origin: &str,
        settings: &crate::util::config::RemoteSettings,
        fetched_at: DateTime<Utc>,
    ) {
        // Serialize read-check-write so the older of two concurrent fetches cannot win.
        static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(existing) = self.load_raw()
            && existing.identity == identity
            && existing.origin == origin
            && existing.fetched_at <= Utc::now()
            && existing.fetched_at >= fetched_at
        {
            tracing::debug!("settings cache write skipped: a newer fetch is already stored");
            return;
        }
        let cache = SettingsCache {
            fetched_at,
            grok_version: distill_version::VERSION.to_string(),
            identity: identity.to_string(),
            origin: origin.to_string(),
            client: crate::http::process_client_identifier(),
            settings: settings.clone(),
        };
        let Ok(payload) = serde_json::to_string(&cache) else {
            return;
        };
        let signature = sign_cache_payload(payload.as_bytes());
        let Ok(json) = serde_json::to_vec_pretty(&SignedSettingsCache { payload, signature })
        else {
            return;
        };
        write_atomic(&self.path, self.ttl, &json, true);
    }
}

pub(crate) struct SettingsCacheWrite {
    path: std::path::PathBuf,
    ttl: std::time::Duration,
    identity: String,
    origin: String,
    settings: crate::util::config::RemoteSettings,
    fetched_at: DateTime<Utc>,
}

impl SettingsCacheWrite {
    pub(crate) fn commit(self) {
        SettingsCacheManager {
            path: self.path,
            ttl: self.ttl,
        }
        .persist(
            &self.identity,
            &self.origin,
            &self.settings,
            self.fetched_at,
        );
    }
}

type HmacSha256 = hmac::Hmac<sha2::Sha256>;

fn sign_cache_payload(payload: &[u8]) -> Vec<u8> {
    use hmac::Mac;
    let Ok(mut m) = HmacSha256::new_from_slice(SETTINGS_CACHE_WRITE_HMAC_KEY) else {
        return Vec::new();
    };
    m.update(payload);
    m.finalize().into_bytes().to_vec()
}

fn verify_cache_signature(payload: &[u8], signature: &[u8]) -> bool {
    verify_with_keys(SETTINGS_CACHE_READ_HMAC_KEYS, payload, signature)
}

/// Accept the signature if any trusted read key verifies it.
fn verify_with_keys(keys: &[&[u8]], payload: &[u8], signature: &[u8]) -> bool {
    use hmac::Mac;
    keys.iter().any(|key| {
        HmacSha256::new_from_slice(key).is_ok_and(|mut m| {
            m.update(payload);
            m.verify_slice(signature).is_ok()
        })
    })
}

#[cfg(test)]
mod settings_cache_tests {
    use chrono::Duration as ChronoDuration;

    use super::*;

    const ORIGIN: &str = "https://proxy.example";

    fn temp_manager(ttl: std::time::Duration) -> (tempfile::TempDir, SettingsCacheManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = SettingsCacheManager {
            path: dir.path().join(SETTINGS_CACHE_FILE),
            ttl,
        };
        (dir, manager)
    }

    fn settings() -> crate::util::config::RemoteSettings {
        crate::util::config::RemoteSettings {
            leader_mode: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn load_or_fetch_skips_the_fetch_on_a_warm_hit() {
        let (_dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = distill_login::GrokAuth::test_default();
        let (cold, write) = manager.load_or_fetch(&auth, ORIGIN, None, || Some(settings()));
        write.unwrap().commit();
        let (warm, warm_write) =
            manager.load_or_fetch(&auth, ORIGIN, None, || panic!("warm hit must not fetch"));
        assert_eq!(cold.unwrap().leader_mode, Some(true));
        assert_eq!(warm.unwrap().leader_mode, Some(true));
        assert!(warm_write.is_none());
    }

    #[test]
    fn load_or_fetch_defers_the_write_until_commit() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = distill_login::GrokAuth::test_default();
        let (_settings, write) = manager.load_or_fetch(&auth, ORIGIN, None, || Some(settings()));
        assert!(!dir.path().join(SETTINGS_CACHE_FILE).exists());
        write.unwrap().commit();
        assert!(dir.path().join(SETTINGS_CACHE_FILE).exists());
    }

    #[test]
    fn load_or_fetch_misses_on_a_different_alpha_test_key() {
        let (_dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = distill_login::GrokAuth::test_default();
        let (_a, write) =
            manager.load_or_fetch(&auth, ORIGIN, Some("alpha-a"), || Some(settings()));
        write.unwrap().commit();
        let (other, _) = manager.load_or_fetch(&auth, ORIGIN, Some("alpha-b"), || {
            Some(crate::util::config::RemoteSettings {
                leader_mode: Some(false),
                ..Default::default()
            })
        });
        assert_eq!(other.unwrap().leader_mode, Some(false));
    }

    #[test]
    fn load_or_fetch_does_not_persist_a_failed_fetch() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let auth = distill_login::GrokAuth::test_default();
        let (result, write) = manager.load_or_fetch(&auth, ORIGIN, None, || None);
        assert!(result.is_none());
        assert!(write.is_none());
        assert!(!dir.path().join(SETTINGS_CACHE_FILE).exists());
    }

    #[test]
    fn misses_on_identity_origin_or_ttl_mismatch() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        manager.persist("id", ORIGIN, &settings(), Utc::now());
        assert!(manager.load_fresh("other", ORIGIN).is_none());
        assert!(manager.load_fresh("id", "https://other").is_none());
        let expired = SettingsCacheManager {
            path: dir.path().join(SETTINGS_CACHE_FILE),
            ttl: std::time::Duration::ZERO,
        };
        assert!(expired.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn identity_survives_token_rotation_and_scopes_by_tenant() {
        let mut base = distill_login::GrokAuth::test_default();
        base.user_id = "user-1".into();
        base.key = "token-A".into();

        let mut rotated = base.clone();
        rotated.key = "token-B".into();
        assert_eq!(
            SettingsCacheManager::identity(&base, None),
            SettingsCacheManager::identity(&rotated, None),
            "identity must survive bearer-token rotation",
        );

        let mut other_team = base.clone();
        other_team.team_id = Some("team-2".into());
        assert_ne!(
            SettingsCacheManager::identity(&base, None),
            SettingsCacheManager::identity(&other_team, None),
            "a different team must yield a different identity",
        );
        let mut other_org = base.clone();
        other_org.organization_id = Some("org-2".into());
        assert_ne!(
            SettingsCacheManager::identity(&base, None),
            SettingsCacheManager::identity(&other_org, None),
            "a different organization must yield a different identity",
        );

        // Keyless (API-key) auth has no user_id, so the key is the principal.
        let mut keyless_a = distill_login::GrokAuth::test_default();
        keyless_a.user_id = String::new();
        keyless_a.key = "api-key-A".into();
        let mut keyless_b = keyless_a.clone();
        keyless_b.key = "api-key-B".into();
        assert_ne!(
            SettingsCacheManager::identity(&keyless_a, None),
            SettingsCacheManager::identity(&keyless_b, None),
            "with no user_id the key must discriminate identity",
        );

        let mut other_issuer = base.clone();
        other_issuer.oidc_issuer = Some("https://idp-b.example".into());
        assert_ne!(
            SettingsCacheManager::identity(&base, None),
            SettingsCacheManager::identity(&other_issuer, None),
            "a different OIDC issuer must yield a different identity",
        );
        let mut other_mode = base.clone();
        other_mode.auth_mode = distill_login::AuthMode::ApiKey;
        assert_ne!(
            SettingsCacheManager::identity(&base, None),
            SettingsCacheManager::identity(&other_mode, None),
            "a different auth mode must yield a different identity",
        );
    }

    fn cache_file(fetched_at: DateTime<Utc>) -> SettingsCache {
        SettingsCache {
            fetched_at,
            grok_version: distill_version::VERSION.to_string(),
            identity: "id".to_string(),
            origin: ORIGIN.to_string(),
            client: crate::http::process_client_identifier(),
            settings: settings(),
        }
    }

    fn signed_cache_bytes(cache: &SettingsCache) -> Vec<u8> {
        let payload = serde_json::to_string(cache).unwrap();
        let signature = sign_cache_payload(payload.as_bytes());
        serde_json::to_vec(&SignedSettingsCache { payload, signature }).unwrap()
    }

    #[test]
    fn hand_written_file_with_stale_version_or_client_misses() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);
        let base = || cache_file(Utc::now());
        let write = |c: &SettingsCache| std::fs::write(&path, signed_cache_bytes(c)).unwrap();

        write(&SettingsCache {
            grok_version: format!("{}-stale", distill_version::VERSION),
            ..base()
        });
        assert!(manager.load_fresh("id", ORIGIN).is_none());

        write(&SettingsCache {
            client: "other-client".to_string(),
            ..base()
        });
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn load_rejects_a_tampered_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);
        manager.persist("id", ORIGIN, &settings(), Utc::now());
        assert!(manager.load_fresh("id", ORIGIN).is_some());

        let mut signed: SignedSettingsCache =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut payload: SettingsCache = serde_json::from_str(&signed.payload).unwrap();
        payload.settings.folder_trust_enabled = Some(false);
        signed.payload = serde_json::to_string(&payload).unwrap();
        std::fs::write(&path, serde_json::to_vec(&signed).unwrap()).unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[test]
    fn persist_does_not_regress_to_an_older_fetch() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);
        let alt = || crate::util::config::RemoteSettings {
            leader_mode: Some(false),
            ..Default::default()
        };

        let newer = Utc::now() - ChronoDuration::seconds(30);
        manager.persist("id", ORIGIN, &settings(), newer);
        let stored = std::fs::read(&path).unwrap();

        // An older same-scope fetch must not overwrite the newer entry.
        manager.persist("id", ORIGIN, &alt(), newer - ChronoDuration::seconds(60));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            stored,
            "an older fetch must not overwrite a newer entry",
        );

        // A newer fetch for the same scope replaces it.
        manager.persist("id", ORIGIN, &alt(), Utc::now());
        assert_eq!(
            manager
                .load_fresh("id", ORIGIN)
                .unwrap()
                .into_settings()
                .leader_mode,
            Some(false),
        );
    }

    #[test]
    fn read_keys_accept_a_superseded_signing_key() {
        use hmac::Mac;
        let old_key: &[u8] = b"grok-shell-settings-cache-hmac-v0-superseded";
        let payload = b"cache-payload";
        let mut mac = HmacSha256::new_from_slice(old_key).unwrap();
        mac.update(payload);
        let sig = mac.finalize().into_bytes().to_vec();

        assert!(
            !verify_with_keys(&[SETTINGS_CACHE_WRITE_HMAC_KEY], payload, &sig),
            "a signature from an untrusted key must not verify",
        );
        assert!(
            verify_with_keys(&[SETTINGS_CACHE_WRITE_HMAC_KEY, old_key], payload, &sig),
            "a signature from a retained read key must verify",
        );
    }

    /// Fields that only a live fetch may set: a cache hit must re-arm signature
    /// verification and must not arm request compression toward a proxy that
    /// may have stopped decoding since the cache was written.
    #[test]
    fn load_drops_live_only_fields_even_from_a_valid_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let mut cache = cache_file(Utc::now());
        cache.settings.managed_config_signature_verification = Some(false);
        cache.settings.accept_request_encodings =
            vec![distill_config_types::RemoteRequestEncoding::Zstd];
        std::fs::write(
            dir.path().join(SETTINGS_CACHE_FILE),
            signed_cache_bytes(&cache),
        )
        .unwrap();
        let loaded = manager.load_fresh("id", ORIGIN).unwrap().into_settings();
        assert_eq!(loaded.managed_config_signature_verification, None);
        assert!(loaded.accept_request_encodings.is_empty());
    }

    #[test]
    fn corrupt_cache_file_is_ignored() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        std::fs::write(dir.path().join(SETTINGS_CACHE_FILE), b"{ not json").unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    fn padded_cache_file(len: usize) -> Vec<u8> {
        let mut json = signed_cache_bytes(&cache_file(Utc::now()));
        assert!(
            json.len() <= len,
            "base envelope already exceeds the target size"
        );
        json.resize(len, b' ');
        json
    }

    #[test]
    fn load_enforces_the_size_cap() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let path = dir.path().join(SETTINGS_CACHE_FILE);

        std::fs::write(
            &path,
            padded_cache_file(SETTINGS_CACHE_MAX_BYTES as usize + 1),
        )
        .unwrap();
        assert!(
            manager.load_fresh("id", ORIGIN).is_none(),
            "over the cap is rejected"
        );

        std::fs::write(&path, padded_cache_file(SETTINGS_CACHE_MAX_BYTES as usize)).unwrap();
        assert!(
            manager.load_fresh("id", ORIGIN).is_some(),
            "at the cap is accepted"
        );
    }

    #[test]
    fn load_rejects_a_future_dated_cache() {
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        let future = cache_file(Utc::now() + ChronoDuration::hours(1));
        std::fs::write(
            dir.path().join(SETTINGS_CACHE_FILE),
            signed_cache_bytes(&future),
        )
        .unwrap();
        assert!(manager.load_fresh("id", ORIGIN).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn persist_writes_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, manager) = temp_manager(SETTINGS_CACHE_TTL);
        manager.persist("id", ORIGIN, &settings(), Utc::now());
        let mode = std::fs::metadata(dir.path().join(SETTINGS_CACHE_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
