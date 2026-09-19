// Modified for Distill by Samuel Fajreldines, 2026.
//! Hub [`AuthProvider`] from `~/.grok/auth.json` for the standalone
//! `workspace_server` binary: loopback `ws://` uses a plain bearer, otherwise an auto-refreshing OIDC provider that persists rotated tokens.
//!
//! The in-leader `grok workspace` exposure does NOT use this path.
//! It gets an in-memory provider from the leader's `AuthManager` (see `LeaderAuthProvider`) so it never races the leader's own auth.json writer.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use url::Url;
use distill_computer_hub_sdk::{
    AuthCredential, AuthIdentity, AuthProvider, OidcAuthProviderBuilder, OnRefreshCallback,
    RefreshEvent,
};

use crate::status_config::ProactiveRefreshConfig;

mod proactive;

pub use proactive::{ProactiveOidcAuthProvider, ProactiveOidcParams};

pub(crate) fn init_metrics() {
    proactive::init_metrics();
}

/// Plain bearer for the loopback / local-dev path (no OIDC refresh).
/// Carries the owner identity from the same auth.json entry so the workspace can derive `WorkspaceIdentity` without a second read.
struct BearerWithIdentity {
    token: String,
    identity: AuthIdentity,
}

impl std::fmt::Debug for BearerWithIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the bearer token; Debug shows only the (non-secret) identity
        f.debug_struct("BearerWithIdentity")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl AuthProvider for BearerWithIdentity {
    fn current(&self) -> AuthCredential {
        AuthCredential::bearer(self.token.clone())
    }

    fn identity(&self) -> Option<AuthIdentity> {
        Some(self.identity.clone())
    }
}

/// Owner identity parsed from an auth.json entry, which the [`AuthProvider`]s built here return from [`AuthProvider::identity`].
fn identity_from_entry(entry: &AuthEntry) -> AuthIdentity {
    AuthIdentity {
        user_id: entry.user_id.clone(),
        principal_type: entry.principal_type.clone(),
        principal_id: entry.principal_id.clone(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct AuthEntry {
    key: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    oidc_client_id: Option<String>,
    #[serde(default)]
    principal_type: Option<String>,
    #[serde(default)]
    principal_id: Option<String>,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn default_auth_path() -> anyhow::Result<PathBuf> {
    let grok = distill_config::user_distill_home()
        .ok_or_else(|| anyhow::anyhow!("no user grok home (set $GROK_HOME or $HOME)"))?;
    Ok(grok.join("auth.json"))
}

/// The `grok login` session [`provider`] would build from an auth file: which scope entry, and whose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSession {
    /// The `auth.json` scope key the entry lives under; refreshes are persisted back to it.
    pub scope_key: String,
    pub identity: AuthIdentity,
    /// Whether the entry carries the client id the OIDC refresher needs. Without it the session can
    /// only serve a loopback hub on its static bearer.
    pub refreshable: bool,
}

/// The session [`provider`] would serve from `auth_path`, or `None` when there is no file or no OIDC
/// entry in it (signed out). Same selection as the provider, so a caller that checks before building
/// it, or watches the file afterwards, sees exactly the entry the provider uses.
///
/// # Errors
///
/// The file cannot be read (for any reason other than not existing) or parsed.
pub fn login_session(auth_path: &Path) -> anyhow::Result<Option<LoginSession>> {
    let Some(entries) = read_auth_entries_if_present(auth_path)? else {
        return Ok(None);
    };
    Ok(
        select_login_entry(entries).map(|(scope_key, entry)| LoginSession {
            scope_key,
            identity: identity_from_entry(&entry),
            refreshable: entry.oidc_client_id.is_some(),
        }),
    )
}

/// The session under one scope key of `auth_path`, or `None` when there is no file, no such entry,
/// or the entry is not an OIDC session. For watching the entry a provider was built from: a
/// `grok logout` removes it, a `grok login` as someone else replaces its identity.
///
/// # Errors
///
/// The file cannot be read (for any reason other than not existing) or parsed.
pub fn login_session_at(auth_path: &Path, scope_key: &str) -> anyhow::Result<Option<LoginSession>> {
    let Some(mut entries) = read_auth_entries_if_present(auth_path)? else {
        return Ok(None);
    };
    Ok(entries
        .remove(scope_key)
        .filter(|entry| entry.refresh_token.is_some() && entry.oidc_issuer.is_some())
        .map(|entry| LoginSession {
            scope_key: scope_key.to_owned(),
            identity: identity_from_entry(&entry),
            refreshable: entry.oidc_client_id.is_some(),
        }))
}

/// Read the active OIDC entry and its scope key, threaded to the refresh write so rotation updates exactly that entry.
/// When several qualify, latest `expires_at` wins — any other pick could rotate a different principal's refresh-token chain.
fn read_auth_entry(path: &Path) -> anyhow::Result<(String, AuthEntry)> {
    let Some(entries) = read_auth_entries_if_present(path)? else {
        anyhow::bail!(
            "No auth credentials found at {}. Run `grok login` first.",
            path.display()
        );
    };
    select_login_entry(entries).ok_or_else(|| {
        anyhow::anyhow!(
            "no OIDC auth entry found in {}. Run `grok login` first.",
            path.display()
        )
    })
}

/// `None` only when the file is definitely absent. Any other failure to read it (a stale network
/// mount, an unsearchable parent) or to parse it is an error: it says nothing about a sign-out,
/// whereas `Path::exists` would have folded it into "no file".
fn read_auth_entries_if_present(
    path: &Path,
) -> anyhow::Result<Option<BTreeMap<String, AuthEntry>>> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to read {}: {e}", path.display())),
    }
}

/// The selection rule [`read_auth_entry`] documents, shared with the public session readers.
fn select_login_entry(entries: BTreeMap<String, AuthEntry>) -> Option<(String, AuthEntry)> {
    entries
        .into_iter()
        .filter(|(_, e)| e.refresh_token.is_some() && e.oidc_issuer.is_some())
        // Strictly-greater comparison: ties (including all-`None`) keep the first candidate in BTreeMap (alphabetical) order
        .fold(None::<(String, AuthEntry)>, |best, cand| match best {
            Some(b) if cand.1.expires_at <= b.1.expires_at => Some(b),
            _ => Some(cand),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OidcProviderKind {
    Sdk,
    Proactive,
}

/// Resolves, with the cause, once the provider's refresh has been rejected for good and the
/// token it serves will expire unreplaced; pending forever for a provider that never refreshes
/// ([`ProactiveOidcAuthProvider::refresh_ended`]).
pub type RefreshEnded = Pin<Box<dyn Future<Output = String> + Send>>;

fn never_ends() -> RefreshEnded {
    Box::pin(std::future::pending())
}

/// Writes `auth.json` on the calling thread.
/// The proactive provider already offloads this onto its persist worker, whose seq check drops stale writes.
/// A nested spawn here would run `write_refreshed_token` after that check and let a stale write land over a newer one.
pub(crate) fn persist_on_refresh(
    auth_path: PathBuf,
    scope_key: String,
    owner_user_id: String,
) -> OnRefreshCallback {
    Arc::new(move |event: &RefreshEvent| {
        if let Err(e) = write_refreshed_token(&auth_path, &scope_key, &owner_user_id, event) {
            tracing::warn!(error = %e, "failed to persist refreshed token to auth.json");
        }
    })
}

/// SDK `on_refresh` is invoked from the async refresh path, which has no PersistGate.
/// Offload the same write so a contended flock cannot stall the runtime.
fn persist_on_refresh_off_thread(
    auth_path: PathBuf,
    scope_key: String,
    owner_user_id: String,
) -> OnRefreshCallback {
    let persist = persist_on_refresh(auth_path, scope_key, owner_user_id);
    Arc::new(move |event: &RefreshEvent| {
        let persist = persist.clone();
        let event = event.clone();
        std::thread::spawn(move || persist(&event));
    })
}

fn build_oidc_provider(
    scope_key: String,
    entry: &AuthEntry,
    auth_path: PathBuf,
    refresh_cfg: &ProactiveRefreshConfig,
) -> anyhow::Result<(Arc<dyn AuthProvider>, OidcProviderKind, RefreshEnded)> {
    let refresh_token = entry.refresh_token.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no refresh_token — cannot refresh expired tokens")
    })?;
    let issuer = entry.oidc_issuer.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no oidc_issuer — cannot refresh expired tokens")
    })?;
    let client_id = entry.oidc_client_id.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no oidc_client_id — cannot refresh expired tokens")
    })?;

    if refresh_cfg.enabled {
        let provider = ProactiveOidcAuthProvider::new(ProactiveOidcParams {
            access_token: entry.key.clone(),
            refresh_token: refresh_token.clone(),
            issuer: issuer.clone(),
            client_id: client_id.clone(),
            identity: identity_from_entry(entry),
            expires_at: entry.expires_at,
            refresh: refresh_cfg.clone(),
            on_refresh: Some(persist_on_refresh(
                auth_path,
                scope_key,
                entry.user_id.clone(),
            )),
        });
        let ended = Box::pin(provider.refresh_ended());
        return Ok((Arc::new(provider), OidcProviderKind::Proactive, ended));
    }

    let mut builder = OidcAuthProviderBuilder::new(&entry.key, refresh_token, issuer, client_id);

    // The workspace derives `WorkspaceIdentity` from `AuthProvider::identity()`, so pass the owner identity along (no separate auth.json read)
    builder = builder.user_id(&entry.user_id);
    if let Some(ref pt) = entry.principal_type {
        builder = builder.principal_type(pt);
    }
    if let Some(ref pid) = entry.principal_id {
        builder = builder.principal_id(pid);
    }
    if let Some(exp) = entry.expires_at {
        builder = builder.expires_at(exp);
    }
    builder = builder.on_refresh(persist_on_refresh_off_thread(
        auth_path,
        scope_key,
        entry.user_id.clone(),
    ));

    Ok((
        Arc::new(builder.build()),
        OidcProviderKind::Sdk,
        never_ends(),
    ))
}

/// How long [`lock_auth_file`] polls for the shared `auth.json.lock` before skipping the persist.
/// Covers the shell's normal refresh hold (~1 s); the shell's worst-case 45 s budget is deliberately not waited out.
/// Losing one persist is recoverable (see [`write_refreshed_token`]); stalling the persist thread for a minute is not worth it.
const AUTH_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// RAII flock on the sibling `auth.json.lock`, the same advisory lock every grok-shell `auth.json` writer takes.
/// Polls `try_lock` rather than a blocking `flock` to bound the wait.
/// Never breaks a held lock: a stale holder here would be the shell mid-refresh, exactly the writer we must not race.
struct AuthFileLockGuard {
    _file: std::fs::File,
}

fn lock_auth_file(auth_json_path: &Path) -> Option<AuthFileLockGuard> {
    use fs2::FileExt;
    use std::io::Write;
    let lock_path = auth_json_path.with_file_name("auth.json.lock");
    let deadline = std::time::Instant::now() + AUTH_LOCK_TIMEOUT;
    loop {
        // The shell's stale-lock recovery breaks locks by unlinking and recreating the file, so only a flock on the live inode counts
        // The shell's own acquire path does the same inode check
        // A dead inode falls through to the same deadline and sleep as a busy lock; retrying immediately would spin while the check keeps failing
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            && file.try_lock_exclusive().is_ok()
            && lock_inode_is_live(&file, &lock_path)
        {
            // Write holder info (`PID:TS`) through the locked fd so the shell can identify (and, if this process dies, break) our hold
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let _ = file.set_len(0);
            let _ = write!(file, "{}:{ts}", std::process::id());
            let _ = file.sync_all();
            return Some(AuthFileLockGuard { _file: file });
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// `fstat(fd)` vs `stat(path)`: `false` when the locked file was concurrently unlinked and recreated (our flock would be on the dead inode).
#[cfg(unix)]
fn lock_inode_is_live(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(fd), Ok(p)) => fd.ino() == p.ino() && fd.dev() == p.dev(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn lock_inode_is_live(_file: &std::fs::File, _path: &Path) -> bool {
    true
}

/// `owner_user_id` is the account this provider was built for: an entry that has since changed hands
/// (a `grok login` as someone else reusing the scope key) is left alone, because writing this
/// session's rotated chain over theirs would leave a hybrid entry — their identity, our tokens.
/// An empty owner (the entry named no account) disables that check.
pub(crate) fn write_refreshed_token(
    path: &Path,
    scope_key: &str,
    owner_user_id: &str,
    event: &RefreshEvent,
) -> anyhow::Result<()> {
    // Read-modify-write under the shared advisory lock
    // An unlocked write races the shell's own refresh writer: whichever writes second rolls back the other's freshly rotated refresh token on disk
    // That guarantees a future `invalid_grant` for every session sharing the file
    let Some(_lock) = lock_auth_file(path) else {
        // The rotated token still serves this process from memory, so warn rather than fail
        // Disk now trails the IdP by one rotation; a fresh process that picks it up will present a spent token
        tracing::warn!(
            timeout = ?AUTH_LOCK_TIMEOUT,
            "auth.json.lock busy; skipping refreshed-token persist (disk left one rotation behind)"
        );
        return Ok(());
    };

    let content = std::fs::read_to_string(path)?;
    let mut raw: serde_json::Value = serde_json::from_str(&content)?;

    let Some(obj) = raw.get_mut(scope_key).and_then(|e| e.as_object_mut()) else {
        anyhow::bail!("auth entry '{scope_key}' not found while persisting refreshed token");
    };

    let disk_owner = obj
        .get("user_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !owner_user_id.is_empty() && !disk_owner.is_empty() && disk_owner != owner_user_id {
        tracing::warn!(
            scope_key,
            "auth.json entry now belongs to another account; not persisting this session's refreshed token"
        );
        return Ok(());
    }

    // Never roll disk back to an older token
    // Each refresh persists on its own thread and a sibling shell writes the same file, so writes can arrive out of order
    // The loser would replace a live refresh token with a spent one and guarantee a future `invalid_grant`
    if let Some(new_expiry) = event.expires_at
        && let Some(disk_expiry) = obj
            .get("expires_at")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        && disk_expiry.with_timezone(&chrono::Utc) >= new_expiry
    {
        tracing::debug!("auth.json already holds a same-or-newer token; skipping persist");
        return Ok(());
    }

    obj.insert(
        "key".to_owned(),
        serde_json::Value::String(event.access_token.clone()),
    );
    if let Some(ref rt) = event.new_refresh_token {
        obj.insert(
            "refresh_token".to_owned(),
            serde_json::Value::String(rt.clone()),
        );
    }
    if let Some(exp) = event.expires_at {
        obj.insert(
            "expires_at".to_owned(),
            serde_json::Value::String(exp.to_rfc3339()),
        );
    }

    write_json_atomic(path, &raw)?;
    tracing::info!(path = %path.display(), "persisted refreshed token to auth.json");
    Ok(())
}

/// Atomically replace `path`: temp file (0600 on Unix), fsync, then rename.
/// Avoids the window where a truncate-in-place rewrite would leave auth.json partially written.
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    use std::io::Write;

    let json = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts
        .open(&tmp)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", tmp.display()))?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    let _ = std::fs::remove_file(path);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("failed to replace {}: {e}", path.display()));
    }
    Ok(())
}

/// Hub auth provider for `hub_url`. `auth_config` overrides `~/.grok/auth.json`.
/// `refresh_cfg.enabled` selects the workspace refresher; the SDK provider is the kill-switch. Loopback `ws://` stays on a static bearer.
pub fn provider(
    hub_url: &Url,
    auth_config: Option<&Path>,
    refresh_cfg: &ProactiveRefreshConfig,
) -> anyhow::Result<Arc<dyn AuthProvider>> {
    provider_with_refresh_ended(hub_url, auth_config, refresh_cfg).map(|(provider, _)| provider)
}

/// [`provider`], with the future that resolves once its refresh has been rejected for good;
/// see [`RefreshEnded`].
pub fn provider_with_refresh_ended(
    hub_url: &Url,
    auth_config: Option<&Path>,
    refresh_cfg: &ProactiveRefreshConfig,
) -> anyhow::Result<(Arc<dyn AuthProvider>, RefreshEnded)> {
    let auth_path = match auth_config {
        Some(p) => p.to_path_buf(),
        None => default_auth_path()?,
    };
    let (scope_key, entry) = read_auth_entry(&auth_path)?;

    let is_loopback = hub_url.scheme() == "ws"
        && matches!(hub_url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));

    if is_loopback {
        tracing::info!("Using local-dev auth (loopback hub)");
        Ok((
            Arc::new(BearerWithIdentity {
                identity: identity_from_entry(&entry),
                token: entry.key.clone(),
            }),
            never_ends(),
        ))
    } else {
        build_oidc_provider(scope_key, &entry, auth_path, refresh_cfg)
            .map(|(provider, _, ended)| (provider, ended))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_auth_json(dir: &std::path::Path, json: &str) -> PathBuf {
        let path = dir.join("auth.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        path
    }

    #[test]
    fn read_auth_entry_picks_oidc_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "legacy": { "key": "xai-plainkey", "user_id": "u1" },
            "oidc": {
                "key": "eyJhbGciOiJFUzI1NiJ9.test",
                "user_id": "u2",
                "refresh_token": "rt",
                "oidc_issuer": "https://auth.example.com",
                "oidc_client_id": "client1"
            }
        }"#,
        );

        let (key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "oidc");
        assert_eq!(entry.refresh_token.as_deref(), Some("rt"));
        assert_eq!(
            entry.oidc_issuer.as_deref(),
            Some("https://auth.example.com")
        );
    }

    /// `login_session` must pick exactly what `provider` will serve, and tell "signed out" apart from "unreadable".
    #[test]
    fn login_session_mirrors_the_provider_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
                "grok-shell": { "key": "eyJ.older", "user_id": "u-old", "refresh_token": "rt1", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "grok-cli", "expires_at": "2026-01-01T00:00:00Z" },
                "other": { "key": "eyJ.newer", "user_id": "u-new", "refresh_token": "rt2", "oidc_issuer": "https://auth.x.ai", "expires_at": "2026-06-01T00:00:00Z" },
                "grok-desktop": { "key": "eyJ.bearer-only", "user_id": "u-app" }
            }"#,
        );
        let session = login_session(&path).unwrap().expect("a session");
        let (scope_key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(session.scope_key, scope_key);
        assert_eq!(session.identity, identity_from_entry(&entry));
        assert_eq!(
            session,
            LoginSession {
                scope_key: "other".to_owned(),
                identity: AuthIdentity {
                    user_id: "u-new".to_owned(),
                    principal_type: None,
                    principal_id: None,
                },
                refreshable: false,
            }
        );

        let bearer_only = write_auth_json(dir.path(), r#"{ "grok-desktop": { "key": "eyJ.x" } }"#);
        assert_eq!(
            login_session(&bearer_only).unwrap(),
            None,
            "no OIDC entry is signed out"
        );
        assert_eq!(
            login_session(&dir.path().join("missing.json")).unwrap(),
            None,
            "no file is signed out"
        );
        let corrupt = write_auth_json(dir.path(), "{");
        assert!(
            login_session(&corrupt).is_err(),
            "unreadable is an error, not a sign-out"
        );
    }

    /// A stat that fails for any reason but NotFound (here ENOTDIR; on a daemon's network home a
    /// stale handle or an unsearchable parent) must not read as signed out. Windows reports a file
    /// in the parent position as NotFound, so the case is unix-only.
    #[cfg(unix)]
    #[test]
    fn a_stat_error_is_not_a_sign_out() {
        let dir = tempfile::tempdir().unwrap();
        let unstattable = write_auth_json(dir.path(), "{}").join("auth.json");
        assert!(
            !unstattable.exists(),
            "Path::exists folds the stat error into false"
        );
        assert!(login_session(&unstattable).is_err());
        assert!(login_session_at(&unstattable, "grok-shell").is_err());
        assert!(
            read_auth_entry(&unstattable)
                .unwrap_err()
                .to_string()
                .contains("failed to read")
        );
    }

    #[test]
    fn login_session_at_reads_one_scope_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
                "grok-shell": { "key": "eyJ.a", "user_id": "u-a", "refresh_token": "rt", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "grok-cli" },
                "grok-desktop": { "key": "eyJ.bearer-only", "user_id": "u-app" }
            }"#,
        );
        let session = login_session_at(&path, "grok-shell")
            .unwrap()
            .expect("a session");
        assert_eq!(session.identity.user_id, "u-a");
        assert!(session.refreshable);
        assert_eq!(
            login_session_at(&path, "grok-desktop").unwrap(),
            None,
            "not an OIDC session"
        );
        assert_eq!(login_session_at(&path, "missing").unwrap(), None);
        assert_eq!(
            login_session_at(&dir.path().join("none.json"), "grok-shell").unwrap(),
            None
        );
    }

    #[test]
    fn read_auth_entry_rejects_non_oidc() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "api_key": { "key": "xai-plainkey", "user_id": "u1" }
        }"#,
        );

        let err = read_auth_entry(&path).unwrap_err();
        assert!(err.to_string().contains("no OIDC auth entry"));
    }

    #[test]
    fn read_auth_entry_missing_file() {
        let path = PathBuf::from("/nonexistent/auth.json");
        let err = read_auth_entry(&path).unwrap_err();
        assert!(err.to_string().contains("No auth credentials"));
    }

    #[test]
    fn read_auth_entry_tolerates_extra_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "scope": {
                "key": "eyJhbGciOiJFUzI1NiJ9.tok",
                "user_id": "u1",
                "auth_mode": "oidc",
                "create_time": "2026-01-01T00:00:00Z",
                "email": "test@example.com",
                "first_name": "Test",
                "refresh_token": "rt1",
                "oidc_issuer": "https://auth.x.ai",
                "oidc_client_id": "c1",
                "some_future_field": true
            }
        }"#,
        );

        let (_key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(entry.refresh_token.as_deref(), Some("rt1"));
    }

    #[test]
    fn build_oidc_provider_requires_refresh_token() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: None,
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("c1".into()),
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider(
            "oidc".into(),
            &entry,
            PathBuf::from("/tmp/x"),
            &ProactiveRefreshConfig::default(),
        )
        .err()
        .expect("the entry must be rejected");
        assert!(err.to_string().contains("refresh_token"));
    }

    #[test]
    fn build_oidc_provider_requires_issuer() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: None,
            oidc_client_id: Some("c1".into()),
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider(
            "oidc".into(),
            &entry,
            PathBuf::from("/tmp/x"),
            &ProactiveRefreshConfig::default(),
        )
        .err()
        .expect("the entry must be rejected");
        assert!(err.to_string().contains("oidc_issuer"));
    }

    #[test]
    fn build_oidc_provider_requires_client_id() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: None,
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider(
            "oidc".into(),
            &entry,
            PathBuf::from("/tmp/x"),
            &ProactiveRefreshConfig::default(),
        )
        .err()
        .expect("the entry must be rejected");
        assert!(err.to_string().contains("oidc_client_id"));
    }

    #[test]
    fn build_oidc_provider_succeeds_with_all_fields() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("c1".into()),
            principal_type: Some("Team".into()),
            principal_id: Some("t1".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        };
        let (provider, kind, _ended) = build_oidc_provider(
            "oidc".into(),
            &entry,
            PathBuf::from("/tmp/x"),
            &ProactiveRefreshConfig {
                enabled: false,
                ..ProactiveRefreshConfig::default()
            },
        )
        .unwrap();
        assert_eq!(kind, OidcProviderKind::Sdk);
        let cred = provider.current();
        match cred {
            distill_computer_hub_sdk::AuthCredential::Bearer { token } => {
                assert_eq!(token, "eyJ.tok");
            }
            _ => panic!("expected Bearer"),
        }
        // Identity comes from the parsed entry (no second auth.json read)
        let id = provider.identity().expect("identity present");
        assert_eq!(id.user_id, "u1");
        assert_eq!(id.principal_type.as_deref(), Some("Team"));
        assert_eq!(id.principal_id.as_deref(), Some("t1"));
    }

    #[test]
    fn write_refreshed_token_updates_jwt_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "legacy": { "key": "xai-old", "user_id": "u1" },
            "oidc": { "key": "eyJ.old", "user_id": "u2", "refresh_token": "rt-old", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let event = RefreshEvent {
            access_token: "eyJ.new".into(),
            new_refresh_token: Some("rt-new".into()),
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", "u2", &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.new"
        );
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-new"
        );
        assert_eq!(
            updated
                .get("legacy")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "xai-old"
        );
    }

    /// A refresh that lands after `grok login` as someone else reused the scope key must not write
    /// the old account's chain into the new account's entry.
    #[test]
    fn write_refreshed_token_leaves_an_entry_that_changed_hands_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{ "oidc": { "key": "eyJ.theirs", "user_id": "u-new", "refresh_token": "rt-theirs", "oidc_issuer": "https://auth.x.ai" } }"#,
        );
        let event = RefreshEvent {
            access_token: "eyJ.ours".into(),
            new_refresh_token: Some("rt-ours".into()),
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", "u-old", &event).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.theirs"
        );
        assert_eq!(
            after
                .get("oidc")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-theirs"
        );

        // The same account, or a provider whose entry named no account, still updates it.
        write_refreshed_token(&path, "oidc", "u-new", &event).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.ours"
        );
        let unnamed = RefreshEvent {
            access_token: "eyJ.unnamed".into(),
            new_refresh_token: None,
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", "", &unnamed).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            after
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.unnamed"
        );
    }

    /// With several OIDC entries (personal and enterprise login), the latest `expires_at` wins; the user's grok sessions refresh that entry.
    /// Alphabetical selection could adopt a different principal's refresh token and rotate it out from under the shell.
    #[test]
    fn read_auth_entry_prefers_latest_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "aaa-stale": { "key": "eyJ.a", "refresh_token": "rt-a", "oidc_issuer": "https://auth.x.ai", "expires_at": "2026-01-01T00:00:00Z" },
            "zzz-active": { "key": "eyJ.z", "refresh_token": "rt-z", "oidc_issuer": "https://auth.x.ai", "expires_at": "2026-06-01T00:00:00Z" }
        }"#,
        );

        let (key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "zzz-active", "latest expires_at must win");
        assert_eq!(entry.refresh_token.as_deref(), Some("rt-z"));

        // An entry with no expires_at never beats one with a timestamp.
        let path = write_auth_json(
            dir.path(),
            r#"{
            "aaa-with-expiry": { "key": "eyJ.a", "refresh_token": "rt-a", "oidc_issuer": "https://auth.x.ai", "expires_at": "2026-01-01T00:00:00Z" },
            "zzz-no-expiry": { "key": "eyJ.z", "refresh_token": "rt-z", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );
        let (key, _) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "aaa-with-expiry");
    }

    #[test]
    fn write_refreshed_token_targets_exact_scope_key() {
        // Non-sorted order: refresh must update the read-selected key ("aaa"), not the first in file order ("zzz")
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "zzz": { "key": "eyJ.z", "refresh_token": "rt-z", "oidc_issuer": "https://auth.x.ai" },
            "aaa": { "key": "eyJ.a", "refresh_token": "rt-a", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let (key, _entry) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "aaa");

        let event = RefreshEvent {
            access_token: "eyJ.a-new".into(),
            new_refresh_token: Some("rt-a-new".into()),
            expires_at: None,
        };
        write_refreshed_token(&path, &key, "", &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            updated
                .get("aaa")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.a-new"
        );
        assert_eq!(
            updated
                .get("aaa")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-a-new"
        );
        assert_eq!(
            updated
                .get("zzz")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.z"
        );
        assert_eq!(
            updated
                .get("zzz")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-z"
        );
    }

    /// Persists run on detached threads and race a sibling shell writing the same file.
    /// A late write must not replace a live refresh token with the one it already rotated away.
    #[test]
    fn write_refreshed_token_does_not_roll_back_a_newer_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "oidc": { "key": "eyJ.newer", "refresh_token": "rt-newer", "oidc_issuer": "https://auth.x.ai", "expires_at": "2026-06-01T00:00:00Z" }
        }"#,
        );

        let stale = RefreshEvent {
            access_token: "eyJ.older".into(),
            new_refresh_token: Some("rt-older".into()),
            expires_at: Some("2026-05-01T00:00:00Z".parse().unwrap()),
        };
        write_refreshed_token(&path, "oidc", "", &stale).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-newer"
        );
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.newer"
        );
    }

    #[test]
    fn write_refreshed_token_preserves_existing_rt_when_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "oidc": { "key": "eyJ.old", "user_id": "u1", "refresh_token": "rt-keep", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let event = RefreshEvent {
            access_token: "eyJ.new".into(),
            new_refresh_token: None,
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", "", &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("key"))
                .unwrap_or(&serde_json::Value::Null),
            "eyJ.new"
        );
        assert_eq!(
            updated
                .get("oidc")
                .and_then(|v| v.get("refresh_token"))
                .unwrap_or(&serde_json::Value::Null),
            "rt-keep"
        );
    }

    #[test]
    fn provider_loopback_uses_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{ "oidc": { "key": "eyJ.tok", "user_id": "u1", "refresh_token": "rt", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "c1" } }"#,
        );
        let url = Url::parse("ws://localhost:9988/v1/tools").unwrap();
        let auth = provider(&url, Some(&path), &ProactiveRefreshConfig::default()).unwrap();
        match auth.current() {
            AuthCredential::Bearer { token } => assert_eq!(token, "eyJ.tok"),
            _ => panic!("expected Bearer"),
        }
        // Loopback still returns the identity from the same entry
        let id = auth.identity().expect("loopback identity present");
        assert_eq!(id.user_id, "u1");
    }

    fn complete_oidc_entry() -> AuthEntry {
        AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("c1".into()),
            principal_type: Some("Team".into()),
            principal_id: Some("t1".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        }
    }

    #[test]
    fn build_oidc_provider_flag_off_uses_sdk_provider() {
        let (provider, kind, _ended) = build_oidc_provider(
            "oidc".into(),
            &complete_oidc_entry(),
            PathBuf::from("/tmp/x"),
            &ProactiveRefreshConfig {
                enabled: false,
                ..ProactiveRefreshConfig::default()
            },
        )
        .unwrap();
        assert_eq!(kind, OidcProviderKind::Sdk);
        match provider.current() {
            AuthCredential::Bearer { token } => assert_eq!(token, "eyJ.tok"),
            _ => panic!("expected Bearer"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn build_oidc_provider_flag_on_uses_proactive_provider() {
        let refresh = ProactiveRefreshConfig {
            enabled: true,
            ..ProactiveRefreshConfig::default()
        };
        let (provider, kind, _ended) = build_oidc_provider(
            "oidc".into(),
            &complete_oidc_entry(),
            PathBuf::from("/tmp/x"),
            &refresh,
        )
        .unwrap();
        assert_eq!(kind, OidcProviderKind::Proactive);
        match provider.current() {
            AuthCredential::Bearer { token } => assert_eq!(token, "eyJ.tok"),
            _ => panic!("expected Bearer"),
        }
    }

    #[test]
    fn provider_loopback_ignores_proactive_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{ "oidc": { "key": "eyJ.tok", "user_id": "u1", "refresh_token": "rt", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "c1" } }"#,
        );
        let refresh = ProactiveRefreshConfig {
            enabled: true,
            ..ProactiveRefreshConfig::default()
        };
        let url = Url::parse("ws://localhost:9988/v1/tools").unwrap();
        let auth = provider(&url, Some(&path), &refresh).unwrap();
        match auth.current() {
            AuthCredential::Bearer { token } => assert_eq!(token, "eyJ.tok"),
            _ => panic!("expected Bearer"),
        }
        let id = auth.identity().expect("loopback identity present");
        assert_eq!(id.user_id, "u1");
        // Loopback never calls `build_oidc_provider`, so the proactive flag cannot change the static-bearer path
    }
}
