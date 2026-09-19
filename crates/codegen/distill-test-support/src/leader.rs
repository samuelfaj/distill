// Modified for Distill by Samuel Fajreldines, 2026.
//! Leader-mode (`grok agent --leader stdio`) test harness.
//!
//! The fixture owns only subprocess handles it created: one initial persistent leader and each returned stdio client.
//! Lock-file PIDs are observations only; a detached replacement leader is never adopted or signaled.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use agent_client_protocol as acp;

use crate::acp_agent_connection::{AgentConnection, timed, timed_ok};
use crate::acp_policy::ClientPolicy;
use crate::acp_transcript::TranscriptEntry;
use crate::env::grok_binary;
use crate::mock_server::MockInferenceServer;
use crate::process::{TestOutput, TestProcess, TestProcessConfig, TestProcessTree, TestStdin};
use crate::sandbox::TestSandbox;
use crate::scaled;

/// `initialize` waits for the leader election and the relay handshake as well as the agent's own startup.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LEADER_RECONNECTED_METHOD: &str = "x.ai/leader_reconnected";

/// Env var naming the binary that elects/hosts the leader in a two-binary (version-skew) test.
/// Falls back to [`grok_binary`]'s resolution.
pub const LEADER_BINARY_ENV: &str = "GROK_BINARY_LEADER";

/// Env var naming the binary for the second (usually newer) client in a two-binary test.
/// Falls back to [`grok_binary`]'s resolution.
pub const CLIENT_BINARY_ENV: &str = "GROK_BINARY_CLIENT";

fn role_binary(env_key: &str) -> PathBuf {
    if let Ok(path) = std::env::var(env_key) {
        let path = PathBuf::from(path);
        assert!(
            path.exists(),
            "{env_key} does not exist: {}",
            path.display()
        );
        return path;
    }
    grok_binary()
}

/// Binary for the leader-electing side of a version-skew test.
pub fn leader_binary() -> PathBuf {
    role_binary(LEADER_BINARY_ENV)
}

/// Binary for the client side of a version-skew test.
pub fn client_binary() -> PathBuf {
    role_binary(CLIENT_BINARY_ENV)
}

/// Owns the concrete initial persistent leader shared by a test's clients. A replacement leader spawned by the code under
/// test is outside this fixture's ownership. [`Self::wait_for_new_leader`] may observe one for assertions but never
/// signals the PID it reads.
pub struct LeaderFixture {
    inner: Arc<Mutex<LeaderFixtureState>>,
}

struct LeaderFixtureState {
    binary: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
    active_clients: usize,
    leader: Option<PersistentLeader>,
}

struct PersistentLeader {
    child: std::process::Child,
    tree: TestProcessTree,
    pid: u32,
}

struct FixtureClientRegistration {
    fixture: Weak<Mutex<LeaderFixtureState>>,
}

impl FixtureClientRegistration {
    fn new(fixture: &Arc<Mutex<LeaderFixtureState>>) -> Self {
        fixture
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_clients += 1;
        Self {
            fixture: Arc::downgrade(fixture),
        }
    }
}

impl Drop for FixtureClientRegistration {
    fn drop(&mut self) {
        if let Some(fixture) = self.fixture.upgrade() {
            let mut fixture = fixture.lock().unwrap_or_else(|error| error.into_inner());
            fixture.active_clients = fixture.active_clients.saturating_sub(1);
        }
    }
}

/// A `grok agent --leader stdio` client subprocess speaking ACP over pipes. It answers the agent with the
/// default [`ClientPolicy`], reads its counters from the transcript, and runs every request under a scaled
/// budget; a timeout, or a failed setup request, panics with the child's stderr.
pub struct LeaderStdioClient {
    connection: AgentConnection,
    process: TestProcess,
    registration: Option<FixtureClientRegistration>,
}

impl LeaderFixture {
    /// Start one concrete persistent leader under the shared sandbox.
    pub async fn start(
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<Self> {
        Self::start_with_binary(&grok_binary(), server, cwd, sandbox).await
    }

    pub async fn start_with_binary(
        binary: &Path,
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<Self> {
        Self::start_with_binary_timeout(binary, server, cwd, sandbox, Duration::from_secs(30)).await
    }

    /// Start a leader pointed at an arbitrary base URL; offline-startup tests aim it at an unreachable endpoint to prove it boots from local data.
    pub async fn start_with_base_url(
        base_url: &str,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<Self> {
        Self::start_with_binary_base_url_timeout(
            &grok_binary(),
            base_url,
            cwd,
            sandbox,
            Duration::from_secs(30),
        )
        .await
    }

    async fn start_with_binary_timeout(
        binary: &Path,
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: &TestSandbox,
        readiness_timeout: Duration,
    ) -> io::Result<Self> {
        Self::start_with_binary_base_url_timeout(
            binary,
            &server.url(),
            cwd,
            sandbox,
            readiness_timeout,
        )
        .await
    }

    async fn start_with_binary_base_url_timeout(
        binary: &Path,
        base_url: &str,
        cwd: &Path,
        sandbox: &TestSandbox,
        readiness_timeout: Duration,
    ) -> io::Result<Self> {
        let socket = sandbox.distill_home().join("leader.sock");
        let lock = sandbox.distill_home().join("leader.lock");
        let mut cmd = std::process::Command::new(binary);
        cmd.args([
            "agent",
            "leader",
            "--no-exit-on-disconnect",
            "--relay-on-demand",
            "--no-auto-update",
        ])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
        sandbox.apply_to_std_command(&mut cmd);
        cmd.envs(distill_tty_utils::pager_env())
            .env("GROK_CLI_CHAT_PROXY_BASE_URL", base_url)
            .env("GROK_XAI_API_BASE_URL", base_url)
            .env("GROK_MODELS_BASE_URL", base_url)
            .env("GROK_FEEDBACK_BASE_URL", base_url)
            .env("GROK_TRACE_UPLOAD_URL", base_url)
            .env("XAI_API_KEY", "test-key-for-ci")
            .env("GROK_LEADER_SOCKET", &socket)
            .env("RUST_LOG", "distill_shell=debug,distill_login=debug");
        let log_path = sandbox.distill_home().join("leader.log");
        match std::fs::File::create(&log_path) {
            Ok(log) => {
                cmd.stderr(log);
            }
            Err(_) => {
                cmd.stderr(std::process::Stdio::null());
            }
        }
        distill_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)]
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let tree = match TestProcessTree::try_attach(pid, "persistent grok test leader") {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = wait_std_child_bounded(&mut child, Duration::from_secs(1));
                return Err(error);
            }
        };
        let fixture = Self {
            inner: Arc::new(Mutex::new(LeaderFixtureState {
                binary: binary.to_path_buf(),
                socket,
                lock,
                active_clients: 0,
                leader: Some(PersistentLeader { child, tree, pid }),
            })),
        };
        fixture.finish_start(readiness_timeout).await
    }

    async fn finish_start(self, timeout: Duration) -> io::Result<Self> {
        if let Err(error) = self.wait_ready(timeout).await {
            let cleanup = self.close().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => io::Error::new(
                    error.kind(),
                    format!("{error}; readiness cleanup also failed: {cleanup}"),
                ),
            });
        }
        Ok(self)
    }

    pub async fn spawn_client(
        &self,
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<LeaderStdioClient> {
        self.spawn_client_with_base_url(&server.url(), cwd, sandbox)
            .await
    }

    /// Spawn a relay client whose own endpoints point at an arbitrary base URL; pairs with [`Self::start_with_base_url`] for a fully offline stack.
    pub async fn spawn_client_with_base_url(
        &self,
        base_url: &str,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<LeaderStdioClient> {
        let binary = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .binary
            .clone();
        self.spawn_client_with_binary_base_url(&binary, base_url, cwd, sandbox)
            .await
    }

    pub async fn spawn_client_with_binary(
        &self,
        binary: &Path,
        server: &MockInferenceServer,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<LeaderStdioClient> {
        self.spawn_client_with_binary_base_url(binary, &server.url(), cwd, sandbox)
            .await
    }

    async fn spawn_client_with_binary_base_url(
        &self,
        binary: &Path,
        base_url: &str,
        cwd: &Path,
        sandbox: &TestSandbox,
    ) -> io::Result<LeaderStdioClient> {
        let socket = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .socket
            .clone();
        let registration = FixtureClientRegistration::new(&self.inner);
        LeaderStdioClient::spawn_with_binary_and_socket(
            binary,
            base_url,
            cwd,
            sandbox,
            socket,
            registration,
        )
        .await
    }

    /// PID of the concrete initial leader while the fixture still owns it.
    pub fn leader_pid(&self) -> Option<u32> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .leader
            .as_ref()
            .map(|leader| leader.pid)
    }

    /// Observe a replacement PID from the lock file without adopting it.
    pub async fn wait_for_new_leader(&self, old_pid: u32, timeout: Duration) -> io::Result<u32> {
        let lock = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lock
            .clone();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(pid) = read_pid_path(&lock)
                && pid != old_pid
                && pid_alive(pid)
            {
                return Ok(pid);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("no replacement leader appeared after pid {old_pid}"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Hard-kill only the concrete initial leader spawned by this fixture.
    pub fn kill_current_concrete_leader(&self) -> io::Result<u32> {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let leader = state
            .leader
            .as_mut()
            .ok_or_else(|| io::Error::other("leader fixture no longer owns an initial leader"))?;
        let tree_result = leader.tree.kill();
        let child_result = leader.child.kill();
        if let Err(error) = tree_result
            && !is_missing_process_error(&error)
        {
            return Err(error);
        }
        if let Err(error) = child_result
            && !is_missing_process_error(&error)
        {
            return Err(error);
        }
        Ok(leader.pid)
    }

    /// Reap the concrete initial leader after a crash test killed it.
    pub async fn reap_exited_concrete_leaders(&self) -> io::Result<()> {
        let state = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(leader) = state.leader.as_mut() else {
                return Ok(());
            };
            reap_exited_persistent_leader(leader, Duration::from_secs(2))?;
            state.leader = None;
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(format!("leader reap task: {error}")))?
    }

    /// On failed test cleanup, hard-kill the concrete initial leader and leak its already-signaled owner so unwind cannot run blocking Drop cleanup.
    /// Lock-file and detached replacement PIDs are never consulted or signaled.
    pub fn contain_failed_cleanup_for_unwind(&self) {
        let leader = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .leader
            .take();
        if let Some(mut leader) = leader {
            let _ = leader.tree.kill();
            let _ = leader.child.kill();
            std::mem::forget(leader);
        }
    }

    /// Close the directly-owned clients first, then shut down the concrete initial leader.
    /// Detached replacements are intentionally untouched.
    pub async fn close(&self) -> io::Result<()> {
        let state = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            if state.active_clients != 0 {
                return Err(io::Error::other(format!(
                    "cannot close leader fixture while {} directly-owned client(s) remain; close/drop clients first",
                    state.active_clients
                )));
            }
            let Some(leader) = state.leader.as_mut() else {
                return Ok(());
            };
            shutdown_persistent_leader(leader)?;
            state.leader = None;
            Ok(())
        })
        .await
        .map_err(|error| io::Error::other(format!("leader cleanup task: {error}")))?
    }

    async fn wait_ready(&self, timeout: Duration) -> io::Result<()> {
        let (socket, pid) = {
            let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let leader = state
                .leader
                .as_ref()
                .expect("leader fixture missing concrete owner");
            (state.socket.clone(), leader.pid)
        };
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if socket.exists() && pid_alive(pid) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    format!(
                        "persistent leader pid {pid} did not become ready at {}",
                        socket.display()
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for LeaderFixture {
    fn drop(&mut self) {
        let leader = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .leader
            .take();
        if let Some(mut leader) = leader {
            let _ = shutdown_persistent_leader(&mut leader);
        }
    }
}

fn shutdown_persistent_leader(leader: &mut PersistentLeader) -> io::Result<()> {
    const GRACE: Duration = Duration::from_secs(2);
    const HARD_WAIT: Duration = Duration::from_secs(2);

    if crate::process::process_has_exited_without_reap(leader.pid, "persistent leader")? {
        return reap_exited_persistent_leader(leader, HARD_WAIT);
    }

    if let Err(error) = leader.tree.terminate()
        && !is_missing_process_error(&error)
    {
        return Err(error);
    }
    if !wait_std_child_exit_without_reap(&leader.child, GRACE)? {
        if let Err(error) = leader.tree.kill()
            && !is_missing_process_error(&error)
        {
            return Err(error);
        }
        if let Err(error) = leader.child.kill()
            && !is_missing_process_error(&error)
        {
            return Err(error);
        }
        if !wait_std_child_exit_without_reap(&leader.child, HARD_WAIT)? {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                format!("persistent leader pid {} did not exit", leader.pid),
            ));
        }
    }
    reap_exited_persistent_leader(leader, HARD_WAIT)
}

fn reap_exited_persistent_leader(
    leader: &mut PersistentLeader,
    timeout: Duration,
) -> io::Result<()> {
    if !wait_std_child_exit_without_reap(&leader.child, timeout)? {
        return Err(io::Error::new(
            ErrorKind::TimedOut,
            format!("persistent leader pid {} did not exit", leader.pid),
        ));
    }
    // macOS may report EPERM when the group contains only the unreaped zombie leader
    // The direct child is already known exited; attempt descendant cleanup while its PGID is reserved, then release before consuming status
    // Focused tests separately prove a live descendant is removed.
    let _ = leader.tree.kill();
    leader.tree.release();
    leader.child.wait().map(|_| ())
}

fn wait_std_child_exit_without_reap(
    child: &std::process::Child,
    timeout: Duration,
) -> io::Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if crate::process::process_has_exited_without_reap(child.id(), "persistent leader")? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn is_missing_process_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::ESRCH || code == libc::ECHILD)
}

fn read_pid_path(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn wait_std_child_bounded(
    child: &mut std::process::Child,
    timeout: Duration,
) -> io::Result<Option<std::process::ExitStatus>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl LeaderStdioClient {
    async fn spawn_with_binary_and_socket(
        binary: &Path,
        base_url: &str,
        cwd: &Path,
        sandbox: &TestSandbox,
        leader_socket: PathBuf,
        registration: FixtureClientRegistration,
    ) -> io::Result<Self> {
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["agent", "--leader", "stdio"]).current_dir(cwd);
        let mut process = TestProcess::spawn(
            cmd,
            sandbox,
            TestProcessConfig::new()
                .label("grok leader stdio client")
                .stdin(TestStdin::Piped)
                .stdout(TestOutput::Piped)
                .env("GROK_CLI_CHAT_PROXY_BASE_URL", base_url)
                .env("GROK_XAI_API_BASE_URL", base_url)
                .env("GROK_MODELS_BASE_URL", base_url)
                .env("GROK_FEEDBACK_BASE_URL", base_url)
                .env("GROK_TRACE_UPLOAD_URL", base_url)
                .env("XAI_API_KEY", "test-key-for-ci")
                .env("GROK_LEADER_SOCKET", leader_socket)
                .env("RUST_LOG", "distill_shell=debug,distill_login=debug"),
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "failed to spawn leader stdio client at {}: {error}\n{}",
                    binary.display(),
                    sandbox.diagnostic_summary(),
                ),
            )
        })?;

        let connection = AgentConnection::connect(&mut process, ClientPolicy::default());
        Ok(LeaderStdioClient {
            connection,
            process,
            registration: Some(registration),
        })
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.process.pid()
    }

    pub fn stderr_text(&self) -> String {
        self.process.stderr_tail().text
    }

    pub fn process_diagnostics(&self) -> String {
        self.process.diagnostic_summary()
    }

    pub fn start_terminate(&mut self) -> io::Result<()> {
        self.process.start_terminate()
    }

    pub fn start_kill(&mut self) {
        self.process.start_kill();
    }

    /// Request a nonblocking hard kill while retaining concrete process and fixture-registration ownership for unwind containment.
    pub fn contain_failed_cleanup_for_unwind(&mut self) {
        self.process.start_kill();
    }

    pub async fn close(&mut self) -> io::Result<std::process::ExitStatus> {
        let status = self.process.close().await?;
        self.registration.take();
        Ok(status)
    }

    pub async fn kill_and_close(&mut self) -> io::Result<std::process::ExitStatus> {
        let status = self.process.kill().await?;
        self.registration.take();
        Ok(status)
    }

    pub fn captured_text(&self) -> String {
        self.connection.handler().transcript().agent_text()
    }

    /// Initialize, then authenticate with the `xai.api_key` method.
    pub async fn initialize(&self) -> acp::InitializeResponse {
        timed_ok(
            &self.process,
            "initialize",
            INITIALIZE_TIMEOUT,
            self.connection.initialize_and_authenticate(),
        )
        .await
    }

    pub async fn create_session(&self, cwd: &Path) -> acp::SessionId {
        timed_ok(
            &self.process,
            "session/new",
            REQUEST_TIMEOUT,
            self.connection.new_session(cwd),
        )
        .await
    }

    pub async fn create_session_with_model(&self, cwd: &Path, model_id: &str) -> acp::SessionId {
        timed_ok(
            &self.process,
            &format!("session/new with modelId={model_id}"),
            REQUEST_TIMEOUT,
            self.connection.new_session_with_model(cwd, model_id),
        )
        .await
    }

    pub async fn prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        self.prompt_with_budget(session_id, text, REQUEST_TIMEOUT)
            .await
    }

    /// A prompt that may span a leader outage runs under the caller's `budget` instead of the request one.
    pub async fn prompt_with_budget(
        &self,
        session_id: &acp::SessionId,
        text: &str,
        budget: Duration,
    ) -> acp::Result<acp::PromptResponse> {
        timed(
            &self.process,
            "prompt",
            budget,
            self.connection.prompt(session_id, text),
        )
        .await
    }

    pub async fn set_model(
        &self,
        session_id: &acp::SessionId,
        model_id: &str,
    ) -> acp::Result<acp::SetSessionModelResponse> {
        timed(
            &self.process,
            &format!("session/set_model({model_id})"),
            REQUEST_TIMEOUT,
            self.connection.set_model(session_id, model_id),
        )
        .await
    }

    pub fn reconnected_count(&self) -> usize {
        self.connection
            .handler()
            .transcript()
            .ext_notification_count(LEADER_RECONNECTED_METHOD)
    }

    pub fn notification_count(&self) -> usize {
        self.connection
            .handler()
            .transcript()
            .session_update_count()
    }

    /// Count of `x.ai/models/update` notifications received (catalog self-heal).
    pub fn models_update_count(&self) -> usize {
        self.connection
            .handler()
            .transcript()
            .ext_notification_count("x.ai/models/update")
    }

    /// Count of `x.ai/settings/update` notifications received (settings self-heal).
    pub fn settings_update_count(&self) -> usize {
        self.connection
            .handler()
            .transcript()
            .ext_notification_count("x.ai/settings/update")
    }
}

pub fn leader_lock_path(home: &Path) -> PathBuf {
    home.join(".grok").join("leader.lock")
}

pub fn read_leader_pid(home: &Path) -> Option<u32> {
    std::fs::read_to_string(leader_lock_path(home))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs an existence/permission check only.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Wait until the leader lock file contains a live PID.
pub async fn wait_for_live_leader(home: &Path, timeout: Duration) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(pid) = read_leader_pid(home)
            && pid_alive(pid)
        {
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Wait for evidence that the bridge finished its reconnect replay: a `x.ai/leader_reconnected` notification
/// or a `session/update` beyond `baseline`, within the scaled `timeout`.
pub async fn wait_for_replay_notifications(
    client: &LeaderStdioClient,
    baseline: usize,
    timeout: Duration,
) -> bool {
    let replayed = client.connection.handler().transcript().wait_until(|entries| {
        let reconnected = entries.iter().any(|entry| {
            matches!(entry, TranscriptEntry::ExtNotification { method, .. } if method == LEADER_RECONNECTED_METHOD)
        });
        let session_updates = entries
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::SessionUpdate(_)))
            .count();
        reconnected || session_updates > baseline
    });
    tokio::time::timeout(scaled(timeout), replayed)
        .await
        .is_ok()
}

pub fn leader_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".grok").join("leader.log")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_leader(script: &str) -> PersistentLeader {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .envs(distill_tty_utils::pager_env());
        distill_tty_utils::detach_std_command(&mut cmd);
        #[allow(clippy::disallowed_methods)] // test fixture; the test reaps it
        let child = cmd.spawn().expect("spawn fake persistent leader");
        let pid = child.id();
        let tree = TestProcessTree::try_attach(pid, "fake persistent leader")
            .expect("attach fake persistent leader");
        PersistentLeader { child, tree, pid }
    }

    fn fixture(root: &Path, leader: PersistentLeader) -> LeaderFixture {
        LeaderFixture {
            inner: Arc::new(Mutex::new(LeaderFixtureState {
                binary: PathBuf::from("fixture"),
                socket: root.join("leader.sock"),
                lock: root.join("leader.lock"),
                active_clients: 0,
                leader: Some(leader),
            })),
        }
    }

    #[tokio::test]
    async fn close_terminates_and_reaps_directly_owned_leader() {
        let temp = tempfile::tempdir().expect("tempdir");
        let leader = fake_leader("trap 'exit 0' TERM; while :; do sleep 1; done");
        let pid = leader.pid;
        let fixture = fixture(temp.path(), leader);

        fixture.close().await.expect("close fixture");

        assert!(!pid_alive(pid));
        assert!(fixture.inner.lock().unwrap().leader.is_none());
    }

    #[tokio::test]
    async fn active_direct_client_registration_blocks_fixture_close() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fixture = fixture(
            temp.path(),
            fake_leader("trap 'exit 0' TERM; while :; do sleep 1; done"),
        );
        let registration = FixtureClientRegistration::new(&fixture.inner);

        let error = fixture
            .close()
            .await
            .expect_err("active client must block close");
        assert!(error.to_string().contains("close/drop clients first"));

        drop(registration);
        fixture.close().await.expect("close after client drop");
    }

    #[test]
    fn lock_file_replacement_pid_is_never_adopted_or_signaled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let initial = fake_leader("trap 'exit 0' TERM; while :; do sleep 1; done");
        let initial_pid = initial.pid;
        let mut replacement = fake_leader("trap 'exit 0' TERM; while :; do sleep 1; done");
        let replacement_pid = replacement.pid;
        std::fs::write(temp.path().join("leader.lock"), replacement_pid.to_string())
            .expect("replacement lock");
        let fixture = fixture(temp.path(), initial);

        drop(fixture);

        assert!(!pid_alive(initial_pid));
        assert!(
            pid_alive(replacement_pid),
            "observed replacement must remain untouched"
        );
        shutdown_persistent_leader(&mut replacement).expect("clean replacement test owner");
    }

    #[tokio::test]
    async fn close_hard_kills_term_ignoring_descendant() {
        let temp = tempfile::tempdir().expect("tempdir");
        let pid_file = temp.path().join("descendant.pid");
        let script = format!(
            "trap 'exit 0' TERM; sh -c 'trap \"\" TERM; echo $$ > {}; while :; do sleep 1; done' & while :; do sleep 1; done",
            pid_file.display()
        );
        let fixture = fixture(temp.path(), fake_leader(&script));
        // The redirect creates the pid file before `echo` writes, so it can read back empty.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let descendant: u32 = loop {
            if let Ok(raw) = std::fs::read_to_string(&pid_file)
                && let Ok(pid) = raw.trim().parse()
            {
                break pid;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for pid file {}",
                pid_file.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        fixture.close().await.expect("close fixture");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while pid_alive(descendant) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!pid_alive(descendant), "descendant {descendant} leaked");
    }
}
