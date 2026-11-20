//! Account metadata and isolated provider authentication. No secrets are serialized here.
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Chatgpt,
    Claude,
    Grok,
    Openrouter,
}
impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Chatgpt => "chatgpt",
            Self::Claude => "claude",
            Self::Grok => "grok",
            Self::Openrouter => "openrouter",
        }
    }
    fn filename(self) -> &'static str {
        match self {
            Self::Chatgpt => "codex-auth.json",
            Self::Claude => "claude-auth.json",
            Self::Grok => "auth.json",
            Self::Openrouter => "openrouter-auth.json",
        }
    }
}
impl std::str::FromStr for Provider {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "chatgpt" => Ok(Self::Chatgpt),
            "claude" => Ok(Self::Claude),
            "grok" => Ok(Self::Grok),
            "openrouter" => Ok(Self::Openrouter),
            _ => Err("Expected chatgpt, claude, grok, or openrouter".into()),
        }
    }
}
/// An entry tolerates unknown keys: a client that mirrors the registry may write
/// its own per-account status fields, and one of those must not fail every session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub provider: Provider,
    pub label: String,
    pub legacy: bool,
}
/// Safe connection status for clients; credentials are never part of this value.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountStatus {
    #[serde(flatten)]
    pub account: Account,
    pub connected: bool,
    pub email: Option<String>,
    pub connection_error: Option<ConnectionError>,
    pub quota: Option<QuotaSnapshot>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionError {
    Unreadable,
    ConfigurationRequired,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoStrategy {
    #[default]
    OrderedFirst,
    HighestRemaining,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutoRouting {
    pub enabled: bool,
    pub strategy: AutoStrategy,
    pub options: Vec<AutoOption>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AutoOption {
    pub id: String,
    pub account_id: String,
    pub main_model: String,
    #[serde(default)]
    pub worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_fallback_model: Option<String>,
}
impl AutoOption {
    pub fn worker_model(&self) -> &str {
        self.worker_model.as_deref().unwrap_or(&self.main_model)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Available,
    Unavailable,
    Unsupported,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub account_id: String,
    pub checked_at: DateTime<Utc>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub status: QuotaStatus,
    pub remaining_percent: Option<f64>,
    pub detail: Option<serde_json::Value>,
}
#[derive(Clone, Debug)]
pub struct AccountLoginFeedback {
    pub authorization_url: String,
    pub device_code: Option<String>,
}
#[derive(Clone)]
pub struct GrokLoginConfig {
    pub config: distill_login::GrokComConfig,
    pub device_flow: Option<bool>,
    pub proxy_base_url: String,
}
impl GrokLoginConfig {
    pub fn from_config(config: &crate::agent::config::Config) -> Self {
        Self {
            config: config.grok_com_config.clone(),
            device_flow: config.login_device_flow,
            proxy_base_url: config.endpoints.proxy_url(),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    accounts: Vec<Account>,
    routing: AutoRouting,
}
impl Default for Registry {
    fn default() -> Self {
        Self {
            version: 1,
            accounts: [
                Provider::Chatgpt,
                Provider::Claude,
                Provider::Grok,
                Provider::Openrouter,
            ]
            .into_iter()
            .map(|provider| Account {
                id: format!("legacy-{}", provider.name()),
                provider,
                label: format!("{} (default)", provider.name()),
                legacy: true,
            })
            .collect(),
            routing: AutoRouting::default(),
        }
    }
}
#[derive(PartialEq, Eq)]
enum CredentialIdentity {
    Chatgpt(crate::codex_auth::CodexAuthIdentity),
    Claude(crate::claude_auth::ClaudeAuthIdentity),
    Grok(String, Option<String>, Option<String>),
    Key([u8; 32]),
}
enum CredentialLock {
    OAuth {
        _file: File,
    },
    Grok {
        _guard: distill_login::storage::AuthFileLock,
    },
    NotRequired,
}
#[derive(Clone, Debug)]
pub struct AccountStore {
    home: PathBuf,
}
impl Default for AccountStore {
    fn default() -> Self {
        #[cfg(test)]
        if let Some(home) = test_home::current() {
            return Self::new(home);
        }
        Self::new(crate::util::distill_home::distill_home())
    }
}

/// Tests that do not exercise account routing must not read the developer's
/// real registry: an enabled Auto route would refuse their session. The
/// override is process-wide because `AccountStore::default()` is, and the
/// guard restores the real store when dropped.
#[cfg(test)]
pub(crate) mod test_home {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static HOME: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub(crate) fn current() -> Option<PathBuf> {
        HOME.lock().expect("account test home lock").clone()
    }

    pub(crate) struct TestHomeGuard {
        _serial: MutexGuard<'static, ()>,
    }

    static SERIAL: Mutex<()> = Mutex::new(());

    impl TestHomeGuard {
        pub(crate) fn new(home: &std::path::Path) -> Self {
            let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
            *HOME.lock().unwrap_or_else(|poison| poison.into_inner()) =
                Some(home.to_path_buf());
            Self { _serial: serial }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            *HOME.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
        }
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

fn no_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => ensure!(
            !meta.file_type().is_symlink(),
            "Account paths cannot be symbolic links"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    no_symlink(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Missing metadata directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    crate::util::secure_file::ensure_owner_only_permissions(temp.path())?;
    serde_json::to_writer(temp.as_file_mut(), value)?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
impl AccountStore {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }
    fn registry_path(&self) -> PathBuf {
        self.home.join("accounts.json")
    }
    fn read(&self) -> Result<Registry> {
        no_symlink(&self.registry_path())?;
        let registry = match std::fs::read(self.registry_path()) {
            Ok(bytes) => serde_json::from_slice::<Registry>(&bytes)
                .map_err(|_| anyhow!("Invalid account registry"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Registry::default(),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            registry.version == 1,
            "Unsupported account registry version"
        );
        let mut ids = HashSet::new();
        for account in &registry.accounts {
            ensure!(
                valid_id(&account.id) && ids.insert(&account.id),
                "Invalid or duplicate account id"
            );
            ensure!(
                !account.label.trim().is_empty(),
                "Account label cannot be empty"
            );
            ensure!(
                if account.legacy {
                    account.id == format!("legacy-{}", account.provider.name())
                } else {
                    !account.id.starts_with("legacy-")
                },
                "Invalid legacy account"
            );
        }
        for account in Registry::default().accounts {
            ensure!(
                registry
                    .accounts
                    .iter()
                    .any(|a| a.id == account.id && a.provider == account.provider && a.legacy),
                "Missing legacy account"
            );
        }
        Self::validate_routing(&registry, &registry.routing)?;
        Ok(registry)
    }
    fn lock(&self) -> Result<File> {
        std::fs::create_dir_all(&self.home)?;
        let path = self.home.join("accounts.json.lock");
        no_symlink(&path)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }
    pub fn list(&self) -> Result<Vec<Account>> {
        Ok(self.read()?.accounts)
    }
    pub fn get(&self, id: &str) -> Result<Account> {
        self.list()?
            .into_iter()
            .find(|a| a.id == id)
            .ok_or_else(|| anyhow!("Unknown account id"))
    }
    pub fn status(&self, id: &str, grok: Option<&GrokLoginConfig>) -> Result<AccountStatus> {
        let account = self.get(id)?;
        let path = self.path_for(&account)?;
        let state: Result<Option<Option<String>>> = (|| {
            Ok(match account.provider {
                Provider::Chatgpt => {
                    crate::codex_auth::load_credentials_at(&path)?.map(|c| c.email)
                }
                Provider::Claude => {
                    crate::claude_auth::load_credentials_at(&path)?.map(|c| c.email)
                }
                Provider::Openrouter => crate::openrouter_auth::api_key_at(&path)?.map(|_| None),
                Provider::Grok => self
                    .grok_auth_manager(
                        id,
                        grok.ok_or_else(|| anyhow!("Grok configuration required"))?,
                    )?
                    .read_disk_auth()
                    .map(|c| c.email),
            })
        })();
        let (connected, email, connection_error) = match state {
            Ok(Some(email)) => (true, email, None),
            Ok(None) => (false, None, None),
            Err(_) => (
                false,
                None,
                Some(if account.provider == Provider::Grok && grok.is_none() {
                    ConnectionError::ConfigurationRequired
                } else {
                    ConnectionError::Unreadable
                }),
            ),
        };
        let quota = self.quota_snapshot(id)?;
        Ok(AccountStatus {
            account,
            connected,
            email,
            connection_error,
            quota,
        })
    }
    /// Stored scoped credentials only; never consult environment bearer overrides.
    pub fn is_connected(&self, id: &str) -> Result<bool> {
        let config = if self.get(id)?.provider == Provider::Grok {
            Some(GrokLoginConfig::from_config(
                &crate::config::load_agent_config_disk_only()
                    .map_err(|_| anyhow!("Failed to load Grok configuration"))?,
            ))
        } else {
            None
        };
        Ok(self.status(id, config.as_ref())?.connected)
    }
    pub fn add(&self, id: &str, provider: Provider, label: &str) -> Result<Account> {
        ensure!(
            valid_id(id) && !id.starts_with("legacy-"),
            "Invalid or reserved account id"
        );
        ensure!(!label.trim().is_empty(), "Account label cannot be empty");
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        ensure!(
            !registry.accounts.iter().any(|a| a.id == id),
            "Duplicate account id"
        );
        let account = Account {
            id: id.into(),
            provider,
            label: label.into(),
            legacy: false,
        };
        registry.accounts.push(account.clone());
        atomic_json(&self.registry_path(), &registry)?;
        Ok(account)
    }
    pub fn rename(&self, id: &str, label: &str) -> Result<Account> {
        ensure!(!label.trim().is_empty(), "Account label cannot be empty");
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        let account = registry
            .accounts
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or_else(|| anyhow!("Unknown account id"))?;
        account.label = label.into();
        let result = account.clone();
        atomic_json(&self.registry_path(), &registry)?;
        Ok(result)
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        let account = self.get(id)?;
        ensure!(
            !account.legacy,
            "Legacy accounts cannot be removed; use logout"
        );
        let _locks = self.lock_account(&account)?;
        let mut registry = self.read()?;
        ensure!(
            !registry.routing.options.iter().any(|o| o.account_id == id),
            "Account is referenced by routing options"
        );
        let path = self.path_for(&account)?;
        // Keep the lock inode: refresh/login waiters may already have it open.
        for path in [path, self.quota_path(id)?] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        registry.accounts.retain(|a| a.id != id);
        atomic_json(&self.registry_path(), &registry)
    }
    fn path_for(&self, account: &Account) -> Result<PathBuf> {
        if account.legacy {
            let path = if account.provider == Provider::Grok {
                distill_login::auth_json_path(&self.home)
            } else {
                self.home.join(account.provider.filename())
            };
            no_symlink(&path)?;
            return Ok(path);
        }
        ensure!(valid_id(&account.id), "Invalid account id");
        let root = self.home.join("accounts");
        let directory = root.join(&account.id);
        let path = directory.join(account.provider.filename());
        for candidate in [&root, &directory, &path] {
            no_symlink(candidate)?;
        }
        Ok(path)
    }
    pub fn credential_path(&self, id: &str) -> Result<PathBuf> {
        self.path_for(&self.get(id)?)
    }
    fn lock_account(&self, account: &Account) -> Result<(CredentialLock, File)> {
        let path = self.path_for(account)?;
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| anyhow!("Missing auth directory"))?,
        )?;
        no_symlink(&path.with_file_name(format!("{}.lock", account.provider.filename())))?;
        // Refresh holds only its credential lock. Every operation needing both
        // locks takes that lock first, then the short-lived registry lock.
        let credentials = match account.provider {
            Provider::Chatgpt => CredentialLock::OAuth {
                _file: crate::codex_auth::acquire_auth_lock(&path)?,
            },
            Provider::Claude => CredentialLock::OAuth {
                _file: crate::claude_auth::acquire_auth_lock(&path)?,
            },
            Provider::Grok => CredentialLock::Grok {
                _guard: distill_login::manager::acquire_auth_file_lock(&path)?,
            },
            Provider::Openrouter => CredentialLock::NotRequired,
        };
        let registry = self.lock()?;
        ensure!(
            self.get(&account.id)? == *account,
            "Account changed during operation"
        );
        Ok((credentials, registry))
    }
    fn validate_routing(registry: &Registry, routing: &AutoRouting) -> Result<()> {
        let mut ids = HashSet::new();
        for option in &routing.options {
            ensure!(
                valid_id(&option.id) && ids.insert(&option.id),
                "Invalid or duplicate routing option id"
            );
            ensure!(
                registry.accounts.iter().any(|a| a.id == option.account_id),
                "Routing option references unknown account"
            );
            ensure!(
                !option.main_model.trim().is_empty()
                    && !option.worker_model().trim().is_empty()
                    && option
                        .worker_fallback_model
                        .as_ref()
                        .is_none_or(|id| !id.trim().is_empty()),
                "Model names cannot be empty"
            );
        }
        ensure!(
            !routing.enabled || !routing.options.is_empty(),
            "Enabled routing needs an option"
        );
        Ok(())
    }
    pub fn load_routing(&self) -> Result<AutoRouting> {
        Ok(self.read()?.routing)
    }
    pub fn save_routing(&self, routing: &AutoRouting) -> Result<()> {
        let _lock = self.lock()?;
        let mut registry = self.read()?;
        Self::validate_routing(&registry, routing)?;
        registry.routing = routing.clone();
        atomic_json(&self.registry_path(), &registry)
    }
    pub fn grok_auth_manager(
        &self,
        id: &str,
        config: &GrokLoginConfig,
    ) -> Result<Arc<distill_login::AuthManager>> {
        ensure!(
            self.get(id)?.provider == Provider::Grok,
            "Account is not Grok"
        );
        let scoped_config = scoped_grok_config(&self.get(id)?, config.clone()).config;
        let manager = Arc::new(distill_login::AuthManager::new_at(
            &self.credential_path(id)?,
            scoped_config.clone(),
            config.proxy_base_url.clone(),
        )?);
        manager.configure_refresher(scoped_config.auth_provider_command.clone(), None);
        Ok(manager)
    }
    pub async fn login(
        &self,
        id: &str,
        device_code: bool,
        grok: Option<GrokLoginConfig>,
    ) -> Result<()> {
        let account = self.get(id)?;
        ensure!(
            !device_code || matches!(account.provider, Provider::Chatgpt | Provider::Grok),
            "Device login is only supported for ChatGPT and Grok"
        );
        let grok = grok.map(|config| scoped_grok_config(&account, config));
        self.login_staged(&account, |staged_path| async move {
            match account.provider {
                Provider::Chatgpt => {
                    crate::codex_auth::run_cli_login_at(&staged_path, device_code).await?;
                }
                Provider::Claude => {
                    crate::claude_auth::run_cli_login_at(&staged_path).await?;
                }
                Provider::Openrouter => {
                    crate::openrouter_auth::run_cli_login_at(&staged_path).await?;
                }
                Provider::Grok => {
                    let config =
                        grok.ok_or_else(|| anyhow!("Grok login configuration required"))?;
                    let manager = Arc::new(distill_login::AuthManager::new_at(
                        &staged_path,
                        config.config,
                        config.proxy_base_url,
                    )?);
                    distill_login::flow::run_cli_login_at(
                        &manager,
                        config.device_flow,
                        device_code,
                    )
                    .await?;
                }
            }
            Ok(())
        })
        .await
    }
    pub async fn login_tui(
        &self,
        id: &str,
        grok: Option<GrokLoginConfig>,
        feedback: tokio::sync::mpsc::UnboundedSender<AccountLoginFeedback>,
    ) -> Result<()> {
        let account = self.get(id)?;
        let grok = grok.map(|config| scoped_grok_config(&account, config));
        self.login_staged(&account, |staged_path| async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let login = async {
                match account.provider {
                    Provider::Chatgpt => {
                        crate::codex_auth::run_tui_login_at(&staged_path, tx).await
                    }
                    Provider::Claude => {
                        crate::claude_auth::run_tui_login_at(&staged_path, tx).await
                    }
                    Provider::Openrouter => {
                        crate::openrouter_auth::run_tui_login_at(&staged_path, tx).await
                    }
                    Provider::Grok => {
                        let config =
                            grok.ok_or_else(|| anyhow!("Grok login configuration required"))?;
                        let manager = Arc::new(distill_login::AuthManager::new_at(
                            &staged_path,
                            config.config,
                            config.proxy_base_url,
                        )?);
                        distill_login::flow::run_tui_login_at(&manager, tx).await
                    }
                }
            };
            tokio::pin!(login);
            tokio::select! {
                result = &mut login => result,
                url = rx => {
                    if let Ok(authorization_url) = url {
                        feedback.send(AccountLoginFeedback { authorization_url, device_code: None })
                            .map_err(|_| anyhow!("Account login cancelled"))?;
                    }
                    login.await
                }
            }
        })
        .await
    }

    async fn login_staged<F, Fut>(&self, account: &Account, run: F) -> Result<()>
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let path = self.path_for(account)?;
        // Stage every provider's login, including reauthentication. Cancellation never alters the connected account.
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("Missing auth directory"))?;
        std::fs::create_dir_all(parent)?;
        let staging = tempfile::tempdir_in(parent)?;
        let staged_path = staging.path().join(account.provider.filename());
        let result = run(staged_path.clone()).await;
        // Provider errors may contain response bodies; keep the public account contract redacted.
        result.map_err(|_| anyhow!("Account login failed or was cancelled"))?;
        crate::util::secure_file::ensure_owner_only_permissions(&staged_path)?;
        let store = self.clone();
        let locked_account = account.clone();
        let _locks =
            tokio::task::spawn_blocking(move || store.lock_account(&locked_account)).await??;
        self.path_for(account)?;
        std::fs::rename(staged_path, path)?;
        // A successful re-login may intentionally change identity; its old quota is no longer valid.
        match std::fs::remove_file(self.quota_path(&account.id)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    pub async fn logout(&self, id: &str, grok: Option<GrokLoginConfig>) -> Result<()> {
        let account = self.get(id)?;
        let path = self.path_for(&account)?;
        match account.provider {
            Provider::Chatgpt => {
                crate::codex_auth::run_cli_logout_at(&path).await?;
            }
            Provider::Claude => {
                tokio::task::spawn_blocking(move || crate::claude_auth::run_cli_logout_at(&path))
                    .await??;
            }
            Provider::Openrouter => {
                crate::openrouter_auth::logout_at(&path)?;
            }
            Provider::Grok => {
                self.grok_auth_manager(
                    id,
                    &grok.ok_or_else(|| anyhow!("Grok configuration required"))?,
                )?
                .clear()?;
            }
        }
        match std::fs::remove_file(self.quota_path(id)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
    pub async fn apply_credentials(
        &self,
        id: &str,
        cfg: &mut distill_sampler::SamplerConfig,
        grok: Option<&GrokLoginConfig>,
    ) -> Result<()> {
        let account = self.get(id)?;
        let path = self.path_for(&account)?;
        match account.provider {
            Provider::Chatgpt => crate::codex_auth::apply_codex_backend_at(&path, cfg).await?,
            Provider::Claude => crate::claude_auth::apply_claude_backend_at(&path, cfg).await?,
            Provider::Openrouter => {
                crate::openrouter_auth::apply_openrouter_backend_at(&path, cfg)?
            }
            Provider::Grok => {
                let config = grok.ok_or_else(|| anyhow!("Grok configuration required"))?;
                ensure!(
                    cfg.base_url.trim_end_matches('/')
                        == config.proxy_base_url.trim_end_matches('/'),
                    "Untrusted Grok endpoint"
                );
                let manager = self.grok_auth_manager(id, config)?;
                let auth = manager
                    .auth()
                    .await
                    .map_err(|_| anyhow!("Grok account is disconnected"))?;
                ensure!(auth.is_xai_auth(), "Grok account authority mismatch");
                let identity = serde_json::to_string(&(
                    &auth.user_id,
                    &auth.principal_id,
                    &auth.principal_type,
                ))?;
                if let Some((_, expected)) = cfg
                    .extra_headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(GROK_IDENTITY_ANCHOR))
                {
                    ensure!(expected == &identity, "Grok account identity changed");
                }
                cfg.extra_headers.retain(|name, _| {
                    !GROK_RESERVED_HEADERS
                        .iter()
                        .any(|h| name.eq_ignore_ascii_case(h))
                });
                cfg.extra_headers
                    .insert(GROK_IDENTITY_ANCHOR.into(), identity);
                cfg.api_key = None;
                cfg.auth_scheme = distill_sampler::config::AuthScheme::Bearer;
                cfg.bearer_resolver = Some(Arc::new(GrokAccountResolver {
                    manager,
                    user_id: auth.user_id,
                    principal_id: auth.principal_id,
                    principal_type: auth.principal_type,
                }));
                cfg.extra_headers.insert(
                    "X-XAI-Token-Auth".into(),
                    config.config.token_header.clone(),
                );
            }
        }
        Ok(())
    }
    fn quota_path(&self, id: &str) -> Result<PathBuf> {
        self.get(id)?;
        ensure!(valid_id(id), "Invalid account id");
        let root = self.home.join("accounts");
        let directory = root.join(id);
        let path = directory.join("quota.json");
        for candidate in [&root, &directory, &path] {
            no_symlink(candidate)?;
        }
        Ok(path)
    }
    /// Last checked quota; consumers decide whether fetched_at is fresh enough for routing.
    pub fn quota_snapshot(&self, id: &str) -> Result<Option<QuotaSnapshot>> {
        let path = self.quota_path(id)?;
        let snapshot = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<QuotaSnapshot>(&bytes)
                .map_err(|_| anyhow!("Invalid quota snapshot"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            snapshot.account_id == id
                && snapshot
                    .remaining_percent
                    .is_none_or(|value| percent(value).is_some()),
            "Invalid quota snapshot identity or percentage"
        );
        Ok(Some(snapshot))
    }
    fn credential_identity(
        &self,
        account: &Account,
        grok: Option<&GrokLoginConfig>,
    ) -> Result<Option<CredentialIdentity>> {
        let path = self.path_for(account)?;
        let identity = match account.provider {
            Provider::Chatgpt => crate::codex_auth::load_credentials_at(&path)?.map(|c| {
                if c.account_id.is_some() || c.chatgpt_user_id.is_some() {
                    CredentialIdentity::Chatgpt(c.identity())
                } else {
                    CredentialIdentity::Key(Sha256::digest(c.access_token.as_bytes()).into())
                }
            }),
            Provider::Claude => crate::claude_auth::load_credentials_at(&path)?.map(|c| {
                if c.account_uuid.is_some() {
                    CredentialIdentity::Claude(c.identity())
                } else {
                    CredentialIdentity::Key(Sha256::digest(c.access_token.as_bytes()).into())
                }
            }),
            Provider::Openrouter => crate::openrouter_auth::api_key_at(&path)?
                .map(|key| CredentialIdentity::Key(Sha256::digest(key.as_bytes()).into())),
            Provider::Grok => match grok {
                Some(config) => self
                    .grok_auth_manager(&account.id, config)?
                    .read_disk_auth()
                    .map(|c| CredentialIdentity::Grok(c.user_id, c.principal_id, c.principal_type)),
                None => None,
            },
        };
        Ok(identity)
    }
    pub async fn refresh_usage(
        &self,
        id: &str,
        grok: Option<GrokLoginConfig>,
    ) -> Result<QuotaSnapshot> {
        let account = self.get(id)?;
        let path = self.path_for(&account)?;
        let credential_before = self.credential_identity(&account, grok.as_ref());
        let grok_identity_config = grok.clone();
        let mut snapshot = QuotaSnapshot {
            account_id: id.into(),
            checked_at: Utc::now(),
            fetched_at: None,
            status: QuotaStatus::Unavailable,
            remaining_percent: None,
            detail: None,
        };
        let result: Result<(Option<f64>, serde_json::Value)> = async {
            match account.provider {
                Provider::Claude => {
                    let usage = crate::claude_auth::fetch_usage_at(&path).await?;
                    Ok((usage.remaining_percent(), serde_json::to_value(usage)?))
                }
                Provider::Chatgpt => {
                    let usage = crate::codex_auth::fetch_usage_at(&path).await?;
                    let remaining = usage.rate_limit.as_ref().and_then(|limit| {
                        if limit.limit_reached { return Some(0.0); }
                        [limit.primary_window.as_ref(), limit.secondary_window.as_ref()].into_iter().flatten().filter_map(|w| percent(100.0 - w.used_percent)).reduce(f64::min)
                    });
                    // Purchased credits keep the account usable after plan limits run out;
                    // the balance has no percentage, so report the quota as unknown.
                    let remaining = remaining.filter(|left| *left > 0.0 || !usage.credits.as_ref().is_some_and(|c| c.has_credits || c.unlimited));
                    // Only typed windows are emitted; arbitrary provider JSON and identifiers are excluded.
                    let detail = serde_json::json!({"rate_limit": usage.rate_limit, "additional_rate_limits": usage.additional_rate_limits.iter().map(|v| &v.rate_limit).collect::<Vec<_>>()});
                    Ok((remaining, detail))
                }
                Provider::Openrouter => {
                    let usage = crate::openrouter_auth::fetch_usage_at(&path).await?;
                    let remaining = usage.limit.zip(usage.limit_remaining).filter(|(limit, _)| limit.is_finite() && *limit > 0.0).and_then(|(limit, left)| percent(left / limit * 100.0));
                    Ok((remaining, serde_json::json!({"usage": usage.usage, "limit": usage.limit, "limit_remaining": usage.limit_remaining})))
                }
                Provider::Grok => {
                    let config = grok.ok_or_else(|| anyhow!("Grok configuration required"))?;
                    let manager = self.grok_auth_manager(id, &config)?;
                    let auth = manager.auth().await?;
                    ensure!(auth.is_xai_auth() && !auth.is_team_principal(), "Consumer credits unavailable for this account");
                    let response = crate::http::shared_client().get(format!("{}/billing?format=credits", config.proxy_base_url.trim_end_matches('/'))).bearer_auth(auth.key).header("X-XAI-Token-Auth", config.config.token_header).header("x-userid", auth.user_id).header("x-grok-client-version", distill_version::VERSION).header(crate::http::CLIENT_MODE_HEADER, crate::http::process_client_mode()).timeout(std::time::Duration::from_secs(15)).send().await?.error_for_status()?;
                    let billing: crate::extensions::billing::BillingConfigResponse = response.json().await?;
                    let remaining = billing.config.as_ref().and_then(|c| c.credit_usage_percent.and_then(|used| percent(100.0 - used)).or_else(|| c.monthly_limit.as_ref().zip(c.used.as_ref()).filter(|(limit, _)| limit.val > 0).and_then(|(limit, used)| percent(100.0 - used.val as f64 / limit.val as f64 * 100.0))));
                    Ok((remaining, serde_json::json!({"credit_usage_percent": billing.config.as_ref().and_then(|c| c.credit_usage_percent)})))
                }
            }
        }.await;
        snapshot.checked_at = Utc::now();
        if let Ok((remaining, detail)) = result {
            snapshot.status = QuotaStatus::Available;
            snapshot.fetched_at = Some(snapshot.checked_at);
            snapshot.remaining_percent = remaining;
            snapshot.detail = Some(detail);
        }
        let _lock = self.lock()?;
        match (
            credential_before,
            self.credential_identity(&account, grok_identity_config.as_ref()),
        ) {
            (Ok(before), Ok(after)) => ensure!(
                before == after,
                "Account credentials changed during quota refresh; retry"
            ),
            (Err(_), Err(_)) => {
                snapshot.status = QuotaStatus::Unavailable;
            }
            _ => {
                return Err(anyhow!(
                    "Account credentials changed during quota refresh; retry"
                ));
            }
        }
        // Reload after the network call: a concurrent rename/route edit must survive.
        ensure!(
            self.get(id)?.provider == account.provider,
            "Account changed during quota refresh"
        );
        if snapshot.status != QuotaStatus::Available {
            if let Some(previous) = self.quota_snapshot(id)? {
                snapshot.fetched_at = previous.fetched_at;
                snapshot.remaining_percent = previous.remaining_percent;
                snapshot.detail = previous.detail;
            }
        }
        atomic_json(&self.quota_path(id)?, &snapshot)?;
        Ok(snapshot)
    }
}
fn scoped_grok_config(account: &Account, mut config: GrokLoginConfig) -> GrokLoginConfig {
    if !account.legacy {
        config.config.auth_provider_command = None;
    }
    config
}
fn percent(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
pub const GROK_IDENTITY_ANCHOR: &str = "x-distill-grok-auth-anchor";
pub const GROK_RESERVED_HEADERS: &[&str] = &[
    GROK_IDENTITY_ANCHOR,
    "x-userid",
    "x-principal-id",
    "x-principal-type",
];
#[derive(Debug)]
struct GrokAccountResolver {
    manager: Arc<distill_login::AuthManager>,
    user_id: String,
    principal_id: Option<String>,
    principal_type: Option<String>,
}
impl distill_sampler::BearerResolver for GrokAccountResolver {
    fn reserved_headers(&self) -> &'static [&'static str] {
        GROK_RESERVED_HEADERS
    }

    fn current_bearer(&self) -> Option<String> {
        self.manager.read_disk_auth()?;
        self.manager.force_reload_from_disk();
        let auth = self.manager.current_wire_valid()?;
        (auth.is_xai_auth()
            && auth.user_id == self.user_id
            && auth.principal_id == self.principal_id
            && auth.principal_type == self.principal_type)
            .then_some(auth.key)
    }
    fn fail_closed_on_missing(&self) -> bool {
        true
    }
    fn prepare_for_send(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            let Some(auth) = self.manager.read_disk_auth() else {
                return;
            };
            if auth.user_id == self.user_id
                && auth.principal_id == self.principal_id
                && auth.principal_type == self.principal_type
            {
                self.manager.force_reload_from_disk();
                let _ = self.manager.auth().await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use distill_sampler::BearerResolver;

    #[test]
    fn registry_routing_and_removal_preserve_siblings_and_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        std::fs::write(dir.path().join("codex-auth.json"), "legacy-secret").unwrap();
        assert_eq!(store.list().unwrap().len(), 4);
        for id in ["../escape", "/absolute", "a/b", "legacy-new", "ONE", ""] {
            assert!(store.add(id, Provider::Chatgpt, "name").is_err());
        }
        store.add("one", Provider::Chatgpt, "First").unwrap();
        store.add("two", Provider::Chatgpt, "Second").unwrap();
        for id in ["one", "two"] {
            let path = store.credential_path(id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{id}-secret")).unwrap();
        }
        assert!(store.add("one", Provider::Claude, "duplicate").is_err());
        assert_eq!(store.rename("one", "Renamed").unwrap().label, "Renamed");
        let mut routing = AutoRouting {
            enabled: true,
            strategy: AutoStrategy::HighestRemaining,
            options: vec![
                AutoOption {
                    id: "route1".into(),
                    account_id: "one".into(),
                    main_model: "gpt-model".into(),
                    worker_fallback_model: None,
                    worker_model: None,
                },
                AutoOption {
                    id: "route2".into(),
                    account_id: "two".into(),
                    main_model: "gpt-model".into(),
                    worker_fallback_model: None,
                    worker_model: Some("gpt-worker".into()),
                },
            ],
        };
        store.save_routing(&routing).unwrap();
        assert_eq!(store.load_routing().unwrap(), routing);
        assert_eq!(routing.options.first().unwrap().worker_model(), "gpt-model");
        assert!(store.remove("one").is_err());
        routing
            .options
            .push(routing.options.first().unwrap().clone());
        assert!(store.save_routing(&routing).is_err());
        routing.options.pop();
        routing.options.first_mut().unwrap().account_id = "missing".into();
        assert!(store.save_routing(&routing).is_err());
        store.save_routing(&AutoRouting::default()).unwrap();
        let lock_path = dir.path().join("accounts/one/codex-auth.json.lock");
        let lock_file = File::open(&lock_path).unwrap();
        store.remove("one").unwrap();
        assert!(store.get("one").is_err());
        assert!(!dir.path().join("accounts/one/codex-auth.json").exists());
        assert!(lock_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                lock_file.metadata().unwrap().ino(),
                std::fs::metadata(&lock_path).unwrap().ino()
            );
        }
        assert_eq!(
            std::fs::read_to_string(store.credential_path("two").unwrap()).unwrap(),
            "two-secret"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("codex-auth.json")).unwrap(),
            "legacy-secret"
        );
        assert!(store.remove("legacy-chatgpt").is_err());
        assert!(
            !std::fs::read_to_string(dir.path().join("accounts.json"))
                .unwrap()
                .contains("secret")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("accounts.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    /// RemoteCode mirrors the Mac's registry into each bot VM by re-encoding it through
    /// its own account status type, which adds `"connected": false` to every account.
    /// A turn reads the registry before anything else (`load_routing`, then `list`), so
    /// rejecting that key fails every session in the VM. Only an account entry is
    /// lenient: the registry shape and every semantic check still reject a bad file.
    #[test]
    fn registry_mirrored_with_extra_account_keys_loads_and_malformed_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        let write = |registry: &serde_json::Value| {
            std::fs::write(
                dir.path().join("accounts.json"),
                serde_json::to_vec(registry).unwrap(),
            )
            .unwrap()
        };
        let mut accounts: Vec<_> = ["chatgpt", "claude", "grok", "openrouter"]
            .into_iter()
            .map(|provider| {
                serde_json::json!({
                    "id": format!("legacy-{provider}"), "provider": provider,
                    "label": format!("{provider} (default)"), "legacy": true, "connected": false,
                })
            })
            .collect();
        accounts.push(serde_json::json!({
            "id": "work", "provider": "claude", "label": "Work", "legacy": false, "connected": false,
        }));
        let mirrored = serde_json::json!({
            "version": 1,
            "accounts": accounts,
            "routing": {"enabled": true, "strategy": "ordered_first", "options": [
                {"id": "route1", "account_id": "work", "main_model": "claude-model"},
            ]},
        });
        write(&mirrored);
        let routing = store.load_routing().expect("a turn's first registry read");
        assert!(routing.enabled);
        assert_eq!(routing.options[0].account_id, "work");
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 5);
        assert_eq!(
            listed[4],
            Account {
                id: "work".into(),
                provider: Provider::Claude,
                label: "Work".into(),
                legacy: false,
            }
        );
        assert!(!store.status("work", None).unwrap().connected);

        // Leniency stops at the account entry.
        let mut unknown_top_level = mirrored.clone();
        unknown_top_level["extra"] = true.into();
        let mut unknown_routing_key = mirrored.clone();
        unknown_routing_key["routing"]["extra"] = true.into();
        let mut unknown_option_key = mirrored.clone();
        unknown_option_key["routing"]["options"][0]["extra"] = true.into();
        let mut missing_account_field = mirrored.clone();
        missing_account_field["accounts"][4]
            .as_object_mut()
            .unwrap()
            .remove("label");
        let mut unknown_provider = mirrored.clone();
        unknown_provider["accounts"][4]["provider"] = "other".into();
        let mut missing_legacy_account = mirrored.clone();
        missing_legacy_account["accounts"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        for (reason, registry) in [
            ("unknown top-level key", unknown_top_level),
            ("unknown routing key", unknown_routing_key),
            ("unknown routing option key", unknown_option_key),
            ("account without a label", missing_account_field),
            ("unknown provider", unknown_provider),
            ("missing legacy account", missing_legacy_account),
        ] {
            write(&registry);
            assert!(store.load_routing().is_err(), "{reason} must be rejected");
            assert!(store.list().is_err(), "{reason} must be rejected");
        }
        std::fs::write(dir.path().join("accounts.json"), b"{\"version\":1,").unwrap();
        assert_eq!(
            store.load_routing().unwrap_err().to_string(),
            "Invalid account registry"
        );
    }

    #[test]
    fn quota_identity_allows_token_rotation_and_rejects_replacement_or_logout() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        let account = store.add("one", Provider::Claude, "One").unwrap();
        let path = store.credential_path("one").unwrap();
        let mut credentials = crate::claude_auth::ClaudeAuthStore {
            access_token: "old-token".into(),
            refresh_token: "old-refresh".into(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            scopes: vec![],
            account_uuid: Some("account-one".into()),
            email: None,
            organization_uuid: None,
        };
        atomic_json(&path, &credentials).unwrap();
        let before = store.credential_identity(&account, None).unwrap();
        credentials.access_token = "rotated-token".into();
        credentials.refresh_token = "rotated-refresh".into();
        atomic_json(&path, &credentials).unwrap();
        assert!(store.credential_identity(&account, None).unwrap() == before);
        credentials.account_uuid = Some("account-two".into());
        atomic_json(&path, &credentials).unwrap();
        assert!(store.credential_identity(&account, None).unwrap() != before);
        std::fs::remove_file(path).unwrap();
        assert!(store.credential_identity(&account, None).unwrap() != before);
    }

    #[tokio::test]
    async fn staged_relogin_wins_over_in_flight_provider_refresh() {
        for provider in [Provider::Chatgpt, Provider::Claude] {
            let dir = tempfile::tempdir().unwrap();
            let store = AccountStore::new(dir.path().to_owned());
            let account = store.add("one", provider, "One").unwrap();
            let path = store.credential_path("one").unwrap();
            let credential = |identity: &str| match provider {
                Provider::Chatgpt => serde_json::json!({
                    "tokens": { "id_token": "fixture", "access_token": identity,
                        "refresh_token": "refresh", "account_id": identity }
                }),
                Provider::Claude => serde_json::json!({
                    "access_token": identity, "refresh_token": "refresh",
                    "expires_at": "2020-01-01T00:00:00Z", "scopes": [], "account_uuid": identity
                }),
                _ => unreachable!(),
            };
            atomic_json(&path, &credential("old-identity")).unwrap();
            let refreshing = Arc::new(tokio::sync::Notify::new());
            let release_refresh = Arc::new(tokio::sync::Notify::new());
            let handler = {
                let refreshing = refreshing.clone();
                let release_refresh = release_refresh.clone();
                move |axum::Json(body): axum::Json<serde_json::Value>| {
                    let refreshing = refreshing.clone();
                    let release_refresh = release_refresh.clone();
                    async move {
                        assert_eq!(body["refresh_token"], "refresh");
                        refreshing.notify_one();
                        release_refresh.notified().await;
                        axum::Json(serde_json::json!({
                            "access_token": "old-identity-refreshed", "refresh_token": "rotated",
                            "expires_in": 3600
                        }))
                    }
                }
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let app = axum::Router::new().route("/oauth/token", axum::routing::post(handler));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let refresh_path = path.clone();
            let refresh = tokio::spawn(async move {
                match provider {
                    Provider::Chatgpt => {
                        crate::codex_auth::refresh_for_account_test(&refresh_path, base).await
                    }
                    Provider::Claude => {
                        crate::claude_auth::refresh_for_account_test(
                            &refresh_path,
                            format!("{base}/oauth/token"),
                        )
                        .await
                    }
                    _ => unreachable!(),
                }
            });
            // The first request of the process also builds the HTTP client, which
            // takes several seconds in a debug build on a loaded machine.
            tokio::time::timeout(std::time::Duration::from_secs(60), refreshing.notified())
                .await
                .unwrap();
            let new_credentials = credential("new-identity");
            let mut login = tokio::spawn(async move {
                store
                    .login_staged(&account, |staged| async move {
                        atomic_json(&staged, &new_credentials)
                    })
                    .await
            });
            // The provider holds its real refresh lock while the response is withheld.
            let early_login =
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut login).await;
            release_refresh.notify_one();
            refresh.await.unwrap().unwrap();
            assert!(
                early_login.is_err(),
                "re-login must wait for the in-flight refresh"
            );
            tokio::time::timeout(std::time::Duration::from_secs(5), login)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap())
                    .unwrap(),
                credential("new-identity")
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn cancelled_and_failed_staged_logins_preserve_credentials_and_remove_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        let account = store.add("one", Provider::Claude, "One").unwrap();
        let path = store.credential_path("one").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "original-secret").unwrap();
        fn assert_send(_: impl Send) {}
        let (feedback, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert_send(store.login_tui("one", None, feedback));
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            store.login_staged(&account, |staged| async move {
                std::fs::write(staged, "staged-secret").unwrap();
                std::future::pending::<Result<()>>().await
            }),
        )
        .await;
        assert!(result.is_err());
        assert!(
            store
                .login_staged(&account, |staged| async move {
                    std::fs::write(staged, "failed-secret").unwrap();
                    Err(anyhow!("raw-secret-response"))
                })
                .await
                .unwrap_err()
                .to_string()
                .contains("Account login failed")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original-secret");
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn concurrent_registry_edits_and_safe_connection_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        std::thread::scope(|scope| {
            for id in ["one", "two"] {
                let store = &store;
                scope.spawn(move || store.add(id, Provider::Claude, id).unwrap());
            }
        });
        assert_eq!(store.list().unwrap().len(), 6);
        assert!(!store.is_connected("one").unwrap());
        let path = store.credential_path("one").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "invalid-secret-json").unwrap();
        let status = store.status("one", None).unwrap();
        assert_eq!(status.connection_error, Some(ConnectionError::Unreadable));
        assert!(!status.connected);
        assert!(!serde_json::to_string(&status).unwrap().contains("secret"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_paths_reject_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        store.add("one", Provider::Openrouter, "One").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("accounts")).unwrap();
        assert!(store.credential_path("one").is_err());
        assert!(store.remove("one").is_err());
        assert!(store.get("one").is_ok());
    }

    #[tokio::test]
    async fn scoped_claude_resolver_rejects_identity_change_and_logout_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        for id in ["one", "two"] {
            store.add(id, Provider::Claude, id).unwrap();
            let path = store.credential_path(id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            atomic_json(
                &path,
                &crate::claude_auth::ClaudeAuthStore {
                    access_token: format!("{id}-token"),
                    refresh_token: "refresh-secret".into(),
                    expires_at: Utc::now() + chrono::Duration::hours(1),
                    scopes: vec![],
                    account_uuid: Some(id.into()),
                    email: None,
                    organization_uuid: None,
                },
            )
            .unwrap();
        }
        let mut config = distill_sampler::SamplerConfig {
            base_url: "https://api.anthropic.com".into(),
            ..Default::default()
        };
        store
            .apply_credentials("one", &mut config, None)
            .await
            .unwrap();
        let resolver = config.bearer_resolver.clone().unwrap();
        assert_eq!(resolver.current_bearer().as_deref(), Some("one-token"));
        let one_path = store.credential_path("one").unwrap();
        let two_path = store.credential_path("two").unwrap();
        std::fs::copy(&two_path, &one_path).unwrap();
        assert!(resolver.current_bearer().is_none());
        let anchors = config.extra_headers.clone();
        assert!(
            store
                .apply_credentials("one", &mut config, None)
                .await
                .is_err()
        );
        assert_eq!(config.extra_headers, anchors);
        store.logout("one", None).await.unwrap();
        assert!(
            crate::claude_auth::load_credentials_at(&one_path)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::claude_auth::load_credentials_at(&two_path)
                .unwrap()
                .unwrap()
                .access_token,
            "two-token"
        );
        // Missing scoped Claude credentials must report unavailable without a live request.
        store.logout("two", None).await.unwrap();
        let quota = store.refresh_usage("two", None).await.unwrap();
        assert_eq!(quota.status, QuotaStatus::Unavailable);
        assert!(quota.remaining_percent.is_none() && quota.fetched_at.is_none());
    }

    #[tokio::test]
    async fn openrouter_selected_key_is_file_backed_and_rejects_wrong_authority() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        store.add("router", Provider::Openrouter, "Router").unwrap();
        let path = store.credential_path("router").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        atomic_json(&path, &serde_json::json!({"key": "selected-key"})).unwrap();
        assert!(store.status("router", None).unwrap().connected);
        let mut config = distill_sampler::SamplerConfig {
            base_url: "https://untrusted.example".into(),
            ..Default::default()
        };
        assert!(
            store
                .apply_credentials("router", &mut config, None)
                .await
                .is_err()
        );
        config.base_url = "https://openrouter.ai/api/v1".into();
        store
            .apply_credentials("router", &mut config, None)
            .await
            .unwrap();
        let resolver = config.bearer_resolver.clone().unwrap();
        let anchor =
            config.extra_headers[crate::openrouter_auth::OPENROUTER_KEY_ANCHOR_HEADER].clone();
        assert_eq!(anchor.len(), 64);
        assert!(!anchor.contains("selected-key"));
        assert!(
            resolver
                .reserved_headers()
                .contains(&crate::openrouter_auth::OPENROUTER_KEY_ANCHOR_HEADER)
        );
        let mut restored: distill_sampler::SamplerConfig =
            serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        store
            .apply_credentials("router", &mut restored, None)
            .await
            .unwrap();
        atomic_json(&path, &serde_json::json!({"key": "replacement-key"})).unwrap();
        assert!(resolver.current_bearer().is_none());
        assert!(
            store
                .apply_credentials("router", &mut restored, None)
                .await
                .is_err()
        );
        assert_eq!(
            restored.extra_headers[crate::openrouter_auth::OPENROUTER_KEY_ANCHOR_HEADER],
            anchor
        );
        atomic_json(&path, &serde_json::json!({"key": "selected-key"})).unwrap();
        assert_eq!(resolver.current_bearer().as_deref(), Some("selected-key"));
        assert!(store.login("router", true, None).await.is_err());
        assert_eq!(resolver.current_bearer().as_deref(), Some("selected-key"));
        store.logout("router", None).await.unwrap();
        assert!(resolver.current_bearer().is_none());
        assert!(!store.status("router", None).unwrap().connected);
    }

    #[tokio::test]
    async fn grok_quota_uses_scoped_billing_and_unavailable_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path().to_owned());
        store.add("grok-one", Provider::Grok, "One").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let concurrent_store = store.clone();
        let app = axum::Router::new().route(
            "/billing",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let store = concurrent_store.clone();
                async move {
                    store
                        .save_routing(&AutoRouting {
                            enabled: true,
                            strategy: AutoStrategy::OrderedFirst,
                            options: vec![AutoOption {
                                id: "route".into(),
                                account_id: "grok-one".into(),
                                main_model: "grok-main".into(),
                                worker_fallback_model: None,
                                worker_model: None,
                            }],
                        })
                        .unwrap();
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer fixture-token"
                    );
                    axum::Json(serde_json::json!({"config": {"creditUsagePercent": 25.0}}))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = GrokLoginConfig {
            config: distill_login::GrokComConfig {
                auth_provider_command: Some("must-not-run-global-credentials".into()),
                ..Default::default()
            },
            device_flow: None,
            proxy_base_url: base,
        };
        let manager = store.grok_auth_manager("grok-one", &config).unwrap();
        assert!(manager.grok_com_config().auth_provider_command.is_none());
        manager
            .save_without_enrichment(distill_login::GrokAuth {
                key: "fixture-token".into(),
                user_id: "fixture-user".into(),
                oidc_issuer: Some(distill_login::XAI_OAUTH2_ISSUER.into()),
                expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                ..Default::default()
            })
            .await
            .unwrap();
        let quota = store
            .refresh_usage("grok-one", Some(config.clone()))
            .await
            .unwrap();
        assert_eq!(quota.status, QuotaStatus::Available);
        assert_eq!(quota.remaining_percent, Some(75.0));
        assert_eq!(
            store.load_routing().unwrap().options[0].main_model,
            "grok-main"
        );
        assert_eq!(
            store
                .quota_snapshot("grok-one")
                .unwrap()
                .unwrap()
                .remaining_percent,
            Some(75.0)
        );
        assert!(quota.fetched_at.is_some());
        assert!(
            !serde_json::to_string(&quota)
                .unwrap()
                .contains("fixture-token")
        );
        // A failed fetch preserves the successful value, explicitly marked unavailable.
        server.abort();
        let failed = store
            .refresh_usage("grok-one", Some(config.clone()))
            .await
            .unwrap();
        assert_eq!(failed.status, QuotaStatus::Unavailable);
        assert_eq!(failed.remaining_percent, Some(75.0));
        assert_eq!(failed.fetched_at, quota.fetched_at);
        assert!(failed.checked_at >= quota.checked_at);
        let mut sampler = distill_sampler::SamplerConfig {
            base_url: config.proxy_base_url.clone(),
            ..Default::default()
        };
        store
            .apply_credentials("grok-one", &mut sampler, Some(&config))
            .await
            .unwrap();
        let resolver = sampler.bearer_resolver.clone().unwrap();
        assert_eq!(resolver.current_bearer().as_deref(), Some("fixture-token"));
        assert!(resolver.reserved_headers().contains(&GROK_IDENTITY_ANCHOR));
        sampler
            .extra_headers
            .insert(GROK_IDENTITY_ANCHOR.into(), "changed".into());
        assert!(
            store
                .apply_credentials("grok-one", &mut sampler, Some(&config))
                .await
                .is_err()
        );
        store
            .logout("grok-one", Some(config.clone()))
            .await
            .unwrap();
        assert!(resolver.current_bearer().is_none());
        server.abort();
        assert!(store.quota_snapshot("grok-one").unwrap().is_none());
        let quota = store.refresh_usage("grok-one", Some(config)).await.unwrap();
        assert_eq!(quota.status, QuotaStatus::Unavailable);
        assert_eq!(
            store.quota_snapshot("grok-one").unwrap().unwrap().status,
            QuotaStatus::Unavailable
        );
        assert!(quota.remaining_percent.is_none() && quota.fetched_at.is_none());
    }
}
