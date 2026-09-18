#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
//! Core workspace library: FS, VCS, permissions, tool config, and subsystem wiring.
#![deny(clippy::indexing_slicing)]
pub mod activity;
pub mod capability;
pub mod channel;
pub mod config;
pub mod discovery;
pub mod envrc;
pub mod error;
pub mod export_github;
pub mod file_system;
pub mod folder_trust;
pub mod fs_notify;
pub(crate) mod git_content_filters;
pub(crate) mod git_odb;
pub mod handle;
pub mod host_kind;
pub mod hub;
pub mod hub_auth;
pub mod hub_channel;
pub mod hub_ids;
pub mod hub_server;
pub mod image_capabilities;
pub mod jev;
pub mod mcp;
pub(crate) mod mcp_claim;
pub(crate) mod path_virtualization;
pub mod permission;
pub mod project_config;
pub mod publish;
pub mod recovery;
mod restore_fetch;
pub mod scheduler_liveness;
pub use restore_fetch::{EnsureCommitsOutcome, ensure_commits_reachable};
pub use session::git::git_object_exists;
pub mod rpc_envelope;
pub mod session;
pub mod status_config;
pub(crate) mod telemetry;
pub use status_config::{ProactiveRefreshConfig, StatusConfig};
pub mod trust;
pub(crate) mod upload;
pub mod util;
pub mod workspace_ops;
pub mod worktree;
pub use capability::CapabilityMode;
pub use channel::{TransportCallResult, TransportContext, TransportError, TransportNotification};
pub use config::{
    AgentSessionConfig, BindMcpConfig, DEFAULT_EVENT_BUFFER_CAPACITY, HookSourceConfig,
    IsolationMode, MemoryConfig, SessionContextFactory, SessionTerminalBackend, WorkspaceConfig,
};
pub use error::{WorkspaceError, WorkspaceResult};
pub use file_system::*;
pub use handle::{
    DrainOutcome, DrainReason, LocalWorkspaceConnectOptions, WorkspaceHandle,
    connect_local_workspace, resolve_workspace_home, termination_grace_from_env,
};
pub use host_kind::WorkspaceHostKind;
pub use hub::HubConfig;
pub use path_virtualization::{
    ARTIFACTS_ALIAS, BindLifecycleCtx, BindMountError, BindMountHook, PathVirtualization,
    VISIBLE_ROOT,
};
pub use permission::*;
pub use session::{WorkspaceSession, WorkspaceShared};
pub use session::{file_state, git, jj};
pub use upload::environment::{WorkspaceEnvironment, WorkspaceIdentity};
pub use workspace_ops::{WorkspaceOp, WorkspaceOps};
pub use xai_grok_workspace_client::WorkspaceClient;
pub use xai_grok_workspace_types::WorkspaceEvent;
pub use xai_hunk_tracker::HunkTrackerHandle;
/// Zero-init every workspace metric family so idle panels render a `0` baseline instead of "No data".
/// Idempotent; call once at workspace-server startup.
pub fn init_metrics() {
    handle::init_metrics();
    recovery::init_metrics();
    session::swap_policy::init_metrics();
    upload::init_metrics();
    permission::init_metrics();
    hub_server::init_metrics();
    hub_auth::init_metrics();
}
/// Crate-wide lock serializing every test that mutates the process-global environment (`GROK_HOME`, `HOME`, …).
/// nextest isolates each test in its own process, but `cargo test --lib` shares ONE process across threads.
/// A per-module lock can't stop a peer test in another module clobbering `GROK_HOME` mid-test, so every env-mutating test module uses this one.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Crate-shared RAII guard for a single process env var in tests: sets (or unsets) it on construction and restores the prior value on drop.
/// Hold it together with [`ENV_TEST_LOCK`] for the test's lifetime, acquiring the lock FIRST so it drops LAST.
/// The env restore (this guard) then runs before the lock releases, so no peer test observes the temporary value.
#[cfg(test)]
pub(crate) struct TestEnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
#[cfg(test)]
impl TestEnvGuard {
    /// Set `key` to `val`, restoring the prior value on drop.
    pub(crate) fn set(key: &'static str, val: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::set_var(key, val) };
        Self { key, prev }
    }
    /// Unset `key`, restoring the prior value on drop.
    pub(crate) fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key, prev }
    }
}
#[cfg(test)]
impl Drop for TestEnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => unsafe { std::env::set_var(self.key, prev) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}
/// Holds [`ENV_TEST_LOCK`] AND a set of [`TestEnvGuard`]s as ONE value; a test or fixture can return/bind it any way and stay correct.
/// Struct fields drop in declaration order, so `_env` restores every env var BEFORE `_lock` releases the lock; no call site can reorder that.
/// Acquire the lock first via [`lock`](Self::lock), then mutate env under it with the chained [`set`](Self::set) builder.
#[cfg(test)]
pub(crate) struct LockedTestEnv {
    _env: Vec<TestEnvGuard>,
    _lock: std::sync::MutexGuard<'static, ()>,
}
#[cfg(test)]
impl LockedTestEnv {
    /// Acquire [`ENV_TEST_LOCK`] (held until this value drops).
    pub(crate) fn lock() -> Self {
        Self {
            _env: Vec::new(),
            _lock: ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }
    /// Set `key` to `val` under the held lock, restoring the prior value on drop.
    /// Intended for DISTINCT keys; the restore order across repeated `set`s of the SAME key is unspecified (guards restore in insertion order).
    pub(crate) fn set(mut self, key: &'static str, val: &std::path::Path) -> Self {
        self._env.push(TestEnvGuard::set(key, val));
        self
    }
}
/// Test sink that accumulates `tracing` output into a shared buffer.
#[cfg(test)]
#[derive(Clone)]
struct VecWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
#[cfg(test)]
impl std::io::Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Serializes capture tests: `rebuild_interest_cache` is process-global, so
/// two concurrent captures can drop each other's warns.
#[cfg(test)]
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Run `f` while capturing WARN-level logs on this thread. `f` must be pure:
/// it runs once un-captured first to register its warn callsites.
#[cfg(test)]
pub(crate) fn capturing_warn_logs<T>(f: impl Fn() -> T) -> (T, String) {
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    {
        static GLOBAL_SINK: std::sync::Once = std::sync::Once::new();
        GLOBAL_SINK.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::WARN)
                    .with_writer(std::io::sink)
                    .finish(),
            );
        });
    }
    f();
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer_buf = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || VecWriter(writer_buf.clone()))
        .finish();
    let value = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let logs = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    (value, logs)
}
#[cfg(test)]
mod init_metrics_tests {
    #[test]
    fn init_metrics_is_idempotent_and_registers_baselines() {
        super::init_metrics();
        super::init_metrics();
        let families = prometheus::gather();
        let has = |name: &str, want: &[(&str, &str)]| {
            families
                .iter()
                .filter(|mf| mf.name() == name)
                .flat_map(|mf| mf.get_metric())
                .any(|m| {
                    want.iter().all(|(k, v)| {
                        m.get_label()
                            .iter()
                            .any(|l| l.name() == *k && l.value() == *v)
                    })
                })
        };
        assert!(has(
            "grok_workspace_upload_outcome_total",
            &[("phase", "tool_state"), ("outcome", "succeeded")]
        ));
        assert!(has(
            "grok_workspace_rpc_requests_total",
            &[("method", "unknown"), ("result", "error")]
        ));
        assert!(has(
            "grok_workspace_rpc_errors_total",
            &[("method", "unknown"), ("error_kind", "unknown_method")]
        ));
        for stage in [
            "startup_recovery",
            "tool_catalog",
            "hub_ws_connect",
            "connect_hub",
            "time_to_ready",
        ] {
            for outcome in ["ok", "error"] {
                assert!(
                    has(
                        "grok_workspace_startup_stage_duration_seconds",
                        &[("stage", stage), ("outcome", outcome)]
                    ),
                    "missing baseline stage={stage} outcome={outcome}"
                );
            }
        }
        assert!(has(
            "grok_workspace_drain_started_total",
            &[("reason", "sigterm")]
        ));
        assert!(has(
            "grok_workspace_toolset_swap_rejected_total",
            &[("reason", "turn_active"), ("trigger", "update_tool_config")]
        ));
        for outcome in [
            "ok",
            "failed_retry",
            "failed_exhausted",
            "failed_terminal",
            "skipped_disabled",
        ] {
            assert!(
                has(
                    "grok_workspace_oidc_proactive_refresh_total",
                    &[("outcome", outcome)]
                ),
                "missing oidc refresh baseline outcome={outcome}"
            );
        }
        assert!(has(
            "grok_workspace_orphan_lost_total",
            &[("reason", "sha_mismatch")]
        ));
        assert!(
            families
                .iter()
                .any(|mf| mf.name() == "grok_workspace_env_capture_panic_total")
        );
        assert!(
            families
                .iter()
                .any(|mf| mf.name() == "grok_workspace_permission_timeout_total")
        );
    }
}
