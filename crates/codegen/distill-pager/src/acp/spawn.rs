// Modified for Distill by Samuel Fajreldines, 2026.
//! Agent spawning — creates the agent process and ACP channels.
//!
//! Simplified to only support GrokShell (in-process) mode.
//! Subprocess and remote modes can be added later if needed.

use std::io::{IsTerminal, Write};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use distill_telemetry::startup::{self, StartupPhase};
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use distill_acp_lib::{
    AcpAgentChannel, AcpClientChannel, AcpClientTx, AcpGatewayReceiver, AcpGatewaySender,
    acp_channels,
};
use distill_login::AuthManager;
use distill_shell::{
    agent::{MvpAgent, activity::SESSION_FLUSH_GRACE, config::Config as AgentConfig},
    config::watcher::{DiscoveryChange, SkillsFileWatcher},
    util::distill_home::distill_home,
};

/// Extra slack when joining the agent OS thread after cancel so the flush
/// can finish and the thread can unwind.
const AGENT_JOIN_SLACK: Duration = Duration::from_secs(2);

const UPLOAD_DRAIN_AT_CANCEL: Duration = Duration::from_secs(1);

/// Grace for the worker runtime's teardown after the run loop exits: a plain
/// drop waits out every in-flight `spawn_blocking` task (non-abortable), so a
/// long detached archive build would otherwise hold `/quit` for its duration.
const WORKER_RUNTIME_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Cancels `token` unless disarmed. Held across the bootstrap join so a
/// connect-timeout drop (which does not abort `spawn_blocking`) stops the worker.
struct BootstrapCancelGuard {
    token: Option<CancellationToken>,
}

impl BootstrapCancelGuard {
    fn arm(token: CancellationToken) -> Self {
        Self { token: Some(token) }
    }

    fn disarm(&mut self) {
        self.token.take();
    }
}

impl Drop for BootstrapCancelGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            token.cancel();
        }
    }
}

/// Run `work` on the blocking pool. Dropping the future cancels `cancel` so
/// the worker can stop at its next check; the OS thread is not abortable.
async fn run_cancellable_blocking<T, F>(cancel: CancellationToken, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let join = tokio::task::spawn_blocking(work);
    let mut guard = BootstrapCancelGuard::arm(cancel.clone());
    tokio::pin!(join);
    let result = tokio::select! {
        biased;
        () = cancel.cancelled() => Err(anyhow::anyhow!("bootstrap cancelled")),
        result = join.as_mut() => result.context("bootstrap worker join"),
    };
    if result.is_ok() {
        guard.disarm();
    }
    result
}

/// Bounded worker-runtime teardown; see [`WORKER_RUNTIME_SHUTDOWN_GRACE`].
/// A timed-out blocking task is abandoned, not cancelled — safe only because
/// the worker exits with the process, which reaps the leftover thread.
pub(super) fn shutdown_worker_runtime(rt: tokio::runtime::Runtime) {
    let shutdown_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
        "teardown.worker_runtime_shutdown",
        elapsed_ms = tracing::field::Empty,
    ));
    let started = std::time::Instant::now();
    rt.shutdown_timeout(WORKER_RUNTIME_SHUTDOWN_GRACE);
    shutdown_span
        .span()
        .record("elapsed_ms", started.elapsed().as_millis() as i64);
}

/// How long the join stays silent before telling an interactive user why exit
/// is taking a moment. Short joins (the common case) print nothing.
const JOIN_NOTICE_AFTER: Duration = Duration::from_millis(1500);

/// Stderr notice after a slow join. Covers the whole SessionEnd pipeline
/// (hooks, telemetry sync, upload drain, memory, optional dream) — not
/// hooks alone, so the copy is intentionally not "session hooks".
const JOIN_NOTICE: &str = "Finishing session…";

/// Result of spawning a child agent.
pub struct SpawnedAgent {
    /// Agent worker OS thread. Hand to [`AgentShutdownGuard`] so the worker is
    /// cancelled and joined — letting session actors finish SessionEnd teardown
    /// (hooks, telemetry, uploads, memory) — on every exit path.
    pub thread_handle: thread::JoinHandle<Result<()>>,
    pub channel: AcpClientChannel,
    pub cancel: CancellationToken,
    /// The agent's `AuthManager`, shared so pager-side consumers (e.g. the voice
    /// channel) resolve the same refreshing bearer as chat traffic.
    pub auth_manager: std::sync::Arc<AuthManager>,
}

/// The single teardown mechanism for an in-process agent: cancels the worker and joins it on drop, so session
/// actors always get. Scope-end drop is the default; the TUI is the one caller that drops it explicitly, because
/// the join has to happen before background processes are reaped.
pub struct AgentShutdownGuard {
    cancel: CancellationToken,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl AgentShutdownGuard {
    /// Guard an in-process agent worker. A `None` thread makes the guard a
    /// no-op cancel (leader mode has no in-process worker to join).
    pub fn new(cancel: CancellationToken, thread: Option<thread::JoinHandle<Result<()>>>) -> Self {
        Self { cancel, thread }
    }
}

impl Drop for AgentShutdownGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
        let Some(handle) = self.thread.take() else {
            return;
        };
        let timeout = SESSION_FLUSH_GRACE
            + UPLOAD_DRAIN_AT_CANCEL
            + WORKER_RUNTIME_SHUTDOWN_GRACE
            + AGENT_JOIN_SLACK;
        match join_agent_thread(handle, timeout) {
            JoinOutcome::Joined => {}
            JoinOutcome::Failed(error) => {
                tracing::warn!(%error, "agent worker exited with error after cancel");
            }
            JoinOutcome::Panicked(panic) => {
                tracing::warn!(%panic, "agent worker panicked after cancel");
            }
            JoinOutcome::TimedOut => {
                tracing::warn!(
                    timeout_ms = timeout.as_millis() as u64,
                    "agent worker did not exit within grace after cancel; \
                     SessionEnd teardown (hooks/telemetry/uploads) may be incomplete"
                );
            }
            JoinOutcome::HelperLost => {
                tracing::warn!("agent worker join helper disappeared; proceeding");
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, strum::AsRefStr, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
enum JoinOutcome {
    Joined,
    Failed(String),
    Panicked(String),
    TimedOut,
    HelperLost,
}

/// On timeout that helper is abandoned rather than joined. this is safe only because every caller is on its way out
/// of the process, so the OS reaps the thread at exit. Do not reuse this outside teardown.
fn join_agent_thread(handle: thread::JoinHandle<Result<()>>, timeout: Duration) -> JoinOutcome {
    use std::sync::mpsc::RecvTimeoutError;

    let span = distill_telemetry::session_end::join_span();
    let start = Instant::now();

    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(handle.join());
    });

    let quiet = timeout.min(JOIN_NOTICE_AFTER);
    let mut notice_shown = false;
    let outcome = match rx.recv_timeout(quiet) {
        Ok(result) => classify_join(result),
        Err(RecvTimeoutError::Disconnected) => JoinOutcome::HelperLost,
        Err(RecvTimeoutError::Timeout) => {
            if std::io::stderr().is_terminal() {
                notice_shown = write_join_notice(&mut std::io::stderr());
            }
            match rx.recv_timeout(timeout.saturating_sub(quiet)) {
                Ok(result) => classify_join(result),
                Err(RecvTimeoutError::Timeout) => JoinOutcome::TimedOut,
                Err(RecvTimeoutError::Disconnected) => JoinOutcome::HelperLost,
            }
        }
    };

    let outcome_label: &'static str = (&outcome).into();
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    distill_telemetry::session_end::record_join(&span, outcome_label, elapsed_ms, notice_shown);
    crate::unified_log::write_direct_info(
        "session_end.worker_join",
        Some(serde_json::json!({
            "elapsed_ms": elapsed_ms,
            "outcome": outcome_label,
            "notice_shown": notice_shown,
        })),
    );
    outcome
}

/// A slow session end is often *because* the pane just closed, and on macOS `is_terminal()` still
/// says yes for a pty whose master is gone, so the write may fail. `true` when the notice landed.
fn write_join_notice(w: &mut impl Write) -> bool {
    crate::best_effort_stderr::write_line(w, JOIN_NOTICE)
}

fn classify_join(result: thread::Result<Result<()>>) -> JoinOutcome {
    match result {
        Ok(Ok(())) => JoinOutcome::Joined,
        Ok(Err(e)) => JoinOutcome::Failed(e.to_string()),
        Err(payload) => JoinOutcome::Panicked(panic_message(payload)),
    }
}

/// Render a panic payload as text — `panic!` payloads are `&str` or `String`,
/// so the log shows the message instead of an opaque `Any`.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Auth manager for the embedded shell: construction and refresher wiring
/// only. The proactive refresh loop starts in [`spawn_grok_shell`]'s body on
/// `agent_cancel`, so a failed spawn cannot leak it.
pub(super) fn boot_auth_manager(
    home: &std::path::Path,
    agent_config: &AgentConfig,
) -> std::sync::Arc<AuthManager> {
    let auth_manager = std::sync::Arc::new(AuthManager::new_with_proxy_base_url(
        home,
        agent_config.grok_com_config.clone(),
        agent_config.endpoints.proxy_url(),
    ));
    auth_manager.configure_refresher(
        agent_config.grok_com_config.auth_provider_command.clone(),
        None,
    );
    auth_manager
}

/// Spawn a GrokShell agent in a background thread.
pub async fn spawn_grok_shell(
    agent_config: AgentConfig,
    cancel: &CancellationToken,
    memory_config: Option<distill_shell::config::MemoryConfig>,
) -> Result<SpawnedAgent> {
    let auth_manager = boot_auth_manager(&distill_home(), &agent_config);
    // Pause token refreshes across system sleep so an OIDC refresh can't
    // straddle a suspend (which can revoke the refresh token and force
    // re-login). No-op where the OS listener is unavailable.
    auth_manager.start_system_power_listener();

    let agent_cancel = cancel.child_token();

    // With no leader, this process owns token refresh — a turn parked on the uncharged 401 path never drives refreshes
    // itself and relies on this loop. On `agent_cancel` so the loop dies with the agent instead of surviving a failed
    // spawn.
    auth_manager.start_proactive_refresh(agent_cancel.child_token());
    auth_manager.prewarm_auth_refresh(agent_cancel.child_token());
    // Dropping a token does not cancel it: a `?` exit below creates no SpawnedAgent and no AgentShutdownGuard, so this
    // guard cancels the prewarm and the refresh loop instead.
    let cancel_auth_tasks_unless_spawned = agent_cancel.clone().drop_guard();

    distill_shell::agent::app::apply_otel_config(&auth_manager, &agent_config.grok_com_config);

    // Policy repair must finish before any authenticated settings load.
    distill_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;
    // This worker is a current-thread runtime. Resolve settings here so the
    // sync bootstrap below observes a finished wait instead of falling open.
    let mut agent_config = agent_config;
    let boot = distill_shell::agent::init::resolve_boot_startup_settings(
        &mut agent_config,
        cancel,
        true,
        auth_manager.current(),
    )
    .await?;

    // On a blocking thread so the connect `select!` can preempt `bootstrap`'s synchronous I/O. A child of the connect
    // token so a user cancel stops the worker, but a timeout drop does not cancel the parent (the embedded fallback
    // reuses it).
    let bootstrap_cancel = cancel.child_token();
    let worker_cancel = bootstrap_cancel.clone();
    let bootstrap_auth = auth_manager.clone();
    let (agent_config, models_manager) = run_cancellable_blocking(bootstrap_cancel, move || {
        distill_shell::agent::init::bootstrap_with_cancel(
            &agent_config,
            &bootstrap_auth,
            None,
            &worker_cancel,
            Some(boot),
        )
    })
    .await?
    .map_err(anyhow::Error::new)?;
    models_manager.spawn_background_refresh();

    let (acp_client, acp_agent) = acp_channels();

    // Clone before `auth_manager` is moved into the agent closure below, so the
    // pager (voice channel) can share the same refreshing bearer.
    let auth_manager_for_pager = auth_manager.clone();

    let skills_paths = agent_config.skills.paths.clone();

    let spawn_fn: Box<dyn FnOnce(AcpClientTx) -> Result<Rc<MvpAgent>> + Send + 'static> = {
        Box::new(move |client_tx| {
            let gateway = AcpGatewaySender::new(client_tx);

            let _t = distill_telemetry::instrumentation::timer("startup.worker_spawn.agent_build");
            let mut agent =
                MvpAgent::with_models(gateway, &agent_config, auth_manager, models_manager);
            drop(_t);
            if let Some(mc) = memory_config {
                agent.set_memory_config(mc);
            }
            Ok(Rc::new(agent))
        })
    };

    // Spawn the agent thread with direct dispatch
    startup::enter(StartupPhase::WorkerSpawn);
    let handle =
        spawn_agent_thread_direct(spawn_fn, acp_agent, agent_cancel.clone(), skills_paths).await?;

    // The spawn succeeded: the caller's AgentShutdownGuard owns cancellation now.
    let agent_cancel = cancel_auth_tasks_unless_spawned.disarm();
    Ok(SpawnedAgent {
        thread_handle: handle,
        channel: acp_client,
        cancel: agent_cancel,
        auth_manager: auth_manager_for_pager,
    })
}

/// Spawn an agent in a dedicated thread with direct RPC dispatch.
/// The agent runs on a single-threaded tokio LocalSet runtime.
/// RPC requests go directly to the agent via Rc, bypassing simplex pipes.
async fn spawn_agent_thread_direct(
    spawn_agent: Box<dyn FnOnce(AcpClientTx) -> Result<Rc<MvpAgent>> + Send + 'static>,
    channel: AcpAgentChannel,
    cancel: CancellationToken,
    skills_paths: Vec<String>,
) -> Result<thread::JoinHandle<Result<()>>> {
    spawn_runtime_thread("acp-agent-worker", move |rt| {
        let local = tokio::task::LocalSet::new();
        let result = local.block_on(&rt, async move {
            let client_tx = channel.tx.clone();
            let agent_rc = spawn_agent(client_tx)?;

            let gw_rx = AcpGatewayReceiver::new(channel.rx, agent_rc.clone()).with_tracing(true);
            tokio::task::spawn_local(gw_rx.run());
            let _skills_watcher = AbortOnDropHandle::new(spawn_skills_file_watcher(
                agent_rc.clone(),
                skills_paths,
                cancel.child_token(),
            ));

            cancel.cancelled().await;
            agent_rc.flush_all_sessions(SESSION_FLUSH_GRACE).await;
            tokio::join!(
                distill_shell::upload::drain_pending_uploads(UPLOAD_DRAIN_AT_CANCEL),
                distill_telemetry::session_ctx::drain_at_process_exit(),
            );
            anyhow::Result::Ok(())
        });
        // LocalSet before runtime, as an implicit scope-end drop would do.
        drop(local);
        shutdown_worker_runtime(rt);
        result
    })
    .await
}

/// Starts a new OS thread, builds a single-threaded Tokio runtime on it, and calls `body` with that runtime.
/// `name` is the thread name shown in logs and debuggers. `body` is the agent's main function; it runs until the agent stops.
/// Building the runtime on the new thread means a build failure is returned from this call, and a `Runtime` is never dropped inside Tokio.
pub(super) async fn spawn_runtime_thread(
    name: &str,
    body: impl FnOnce(tokio::runtime::Runtime) -> Result<()> + Send + 'static,
) -> Result<thread::JoinHandle<Result<()>>> {
    let (built_tx, built_rx) = tokio::sync::oneshot::channel();
    // A caller dropped while it waits for the build drops `start_tx`, so the thread exits instead of running `body` detached.
    let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();
    let thread_name = name.to_owned();
    // `block_on` inlines the agent's async state machine on this stack; a debug build of the
    // agent worker overflows the 2 MB default at the first prompt (macOS spawns with 512 KB)
    const RUNTIME_THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
    let handle = thread::Builder::new()
        .name(name.to_owned())
        .stack_size(RUNTIME_THREAD_STACK_SIZE)
        .spawn(move || -> Result<()> {
            let mut builder = tokio::runtime::Builder::new_current_thread();
            let built = distill_tty_utils::runtime::build_with_blocking_pool(builder.enable_all())
                .with_context(|| format!("failed to start the {thread_name} runtime"));
            let rt = match built {
                Ok(rt) => rt,
                Err(error) => {
                    let message = error.to_string();
                    let _ = built_tx.send(Err(error));
                    return Err(anyhow::anyhow!(message));
                }
            };
            let _ = built_tx.send(Ok(()));
            if start_rx.blocking_recv().is_err() {
                return Err(anyhow::anyhow!(
                    "the {thread_name} thread was released before it started"
                ));
            }
            body(rt)
        })
        .with_context(|| format!("failed to start the {name} thread"))?;

    built_rx
        .await
        .with_context(|| format!("the {name} thread exited before its runtime was built"))??;
    // The thread blocks on `start_rx` until here, so the receiver is alive and this send cannot fail.
    let _ = start_tx.send(());

    Ok(handle)
}

fn spawn_skills_file_watcher(
    agent: Rc<MvpAgent>,
    skills_paths: Vec<String>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // Resolve the workspace at call time; the blocking pool may run this after cwd changes.
    let cwd = std::env::current_dir().unwrap_or_default();
    let workspace_user_dir = distill_agent::prompt::workspace_user::optional_workspace_user_dir();
    tokio::task::spawn_local(async move {
        // Blocking pool with a shutdown cancel, so a slow filesystem cannot wedge teardown.
        let start_result = run_cancellable_blocking(cancel.clone(), move || {
            // Plain span, not instrumentation::timer, which would misattribute this off-path walk to startup latency.
            let _span = tracing::info_span!("skills_watcher").entered();
            SkillsFileWatcher::start(
                Some(cwd.as_path()),
                workspace_user_dir.as_deref(),
                &skills_paths,
            )
        })
        .await;

        let (mut watcher, mut skills_rx) = match start_result {
            Ok(Some(started)) => started,
            Ok(None) => return,
            Err(e) => {
                tracing::debug!(error = %e, "skills watcher not started; live reload off");
                return;
            }
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe = skills_rx.recv() => match maybe {
                    Some(change) => {
                        let created_discovery_dir = watcher.refresh_new_discovery_dirs();
                        apply_discovery_change(&agent, change, created_discovery_dir);
                    }
                    None => break,
                },
            }
        }
    })
}

fn apply_discovery_change(agent: &MvpAgent, change: DiscoveryChange, created_discovery_dir: bool) {
    match change {
        DiscoveryChange::Skills => {
            tracing::info!("skill directory changed on disk; reloading skills for all sessions");
            agent.reload_skills_all_sessions();
            if created_discovery_dir {
                agent.advertise_commands_all_sessions();
            }
        }
        DiscoveryChange::Workflows => {
            tracing::info!(
                "workflow directory changed on disk; re-advertising commands for all sessions"
            );
            agent.advertise_commands_all_sessions();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body runs only when the caller receives the handle. A caller dropped
    /// while the runtime builds must not leave the body running on a detached thread.
    #[tokio::test]
    async fn a_caller_dropped_during_the_build_never_runs_the_body() {
        // The closure owns `ran_tx`. A thread that exits without running the body drops it unsent.
        let (ran_tx, ran_rx) = std::sync::mpsc::channel::<()>();
        let spawn = spawn_runtime_thread("released-worker", move |_rt| {
            let _ = ran_tx.send(());
            Ok(())
        });

        // One poll spawns the thread and parks on the build report. The drop that follows releases the thread.
        match tokio::time::timeout(Duration::ZERO, spawn).await {
            Err(_dropped_while_building) => assert!(
                ran_rx.recv_timeout(Duration::from_secs(5)).is_err(),
                "a released thread must exit without running the body"
            ),
            // The thread built its runtime inside the first poll, so the caller kept the handle.
            Ok(handle) => {
                handle.expect("spawn").join().expect("join").expect("body");
                assert!(
                    ran_rx.try_recv().is_ok(),
                    "a thread whose handle the caller kept must run the body"
                );
            }
        }
    }

    /// Teardown must stay grace-bounded with a non-abortable blocking task
    /// still in flight (a plain drop would wait it out).
    #[test]
    fn worker_runtime_teardown_bounded_despite_inflight_blocking_task() {
        let mut builder = tokio::runtime::Builder::new_current_thread();
        let rt =
            distill_tty_utils::runtime::build_with_blocking_pool(builder.enable_all()).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        rt.handle().spawn_blocking(move || {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_secs(6));
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("blocking task must start");
        let start = std::time::Instant::now();
        shutdown_worker_runtime(rt);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "teardown blocked on the blocking task: {:?}",
            start.elapsed()
        );
    }

    /// The embedded-shell path has no leader process to own token refresh: a parked 401 turn can only self-heal
    /// in-process through this loop. It starts in `spawn_grok_shell`'s body on `agent_cancel`, so a `?` exit before the
    /// spawn succeeds (drop-guard fires) must cancel it instead of leaking a refresh loop until process teardown.
    #[tokio::test]
    async fn spawn_drop_guard_cancels_proactive_refresh_loop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let am = boot_auth_manager(dir.path(), &AgentConfig::default());
        assert!(
            !am.proactive_refresh_started(),
            "construction alone must not start the loop — only the guarded spawn body may"
        );

        // Baseline includes the configured refresher's back-reference; the
        // loop task's own Arc is the +1 on top of it.
        let baseline = std::sync::Arc::strong_count(&am);

        // Mirror spawn_grok_shell's wiring: loop on agent_cancel, guarded until
        // ownership transfers to SpawnedAgent.
        let cancel = CancellationToken::new();
        let agent_cancel = cancel.child_token();
        am.start_proactive_refresh(agent_cancel.child_token());
        let guard = agent_cancel.clone().drop_guard();
        assert!(
            am.proactive_refresh_started(),
            "the embedded shell must run the proactive refresh loop"
        );
        assert_eq!(
            std::sync::Arc::strong_count(&am),
            baseline + 1,
            "the running loop task must hold its own AuthManager Arc"
        );

        // A `?` exit drops the guard with no SpawnedAgent; the cancelled loop
        // must release its own AuthManager Arc instead of refreshing forever.
        drop(guard);
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::sync::Arc::strong_count(&am) > baseline && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::sync::Arc::strong_count(&am),
            baseline,
            "dropping the spawn guard must cancel the refresh loop"
        );
    }

    #[test]
    fn join_reports_clean_worker_exit() {
        let handle = thread::spawn(|| Ok(()));
        assert_eq!(
            join_agent_thread(handle, Duration::from_secs(5)),
            JoinOutcome::Joined
        );
    }

    #[test]
    fn join_reports_worker_error() {
        let handle = thread::spawn(|| Err(anyhow::anyhow!("flush failed")));
        assert_eq!(
            join_agent_thread(handle, Duration::from_secs(5)),
            JoinOutcome::Failed("flush failed".to_string())
        );
    }

    /// The timeout branch the built-binary e2e cannot reach: a wedged worker
    /// (e.g. a hung SessionEnd hook) is abandoned once the budget elapses
    /// instead of holding the process open indefinitely.
    #[test]
    fn join_abandons_wedged_worker_at_budget() {
        let handle = thread::spawn(|| {
            thread::sleep(Duration::from_secs(30));
            Ok(())
        });
        let started = std::time::Instant::now();
        assert_eq!(
            join_agent_thread(handle, Duration::from_millis(50)),
            JoinOutcome::TimedOut
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "join must return at its budget, not wait out the worker"
        );
    }

    /// `bounded_connect` owns the connect future inside `select!`. The losing
    /// branch is dropped; that drop must cancel the bootstrap worker.
    #[tokio::test]
    async fn dropping_the_blocking_join_cancels_the_worker_token() {
        let token = CancellationToken::new();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let started_worker = started.clone();
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_worker = seen.clone();
        let worker_token = token.clone();
        tokio::select! {
            _ = run_cancellable_blocking(token, move || {
                started_worker.notify_one();
                let start = Instant::now();
                while !worker_token.is_cancelled() && start.elapsed() < Duration::from_secs(5) {
                    thread::sleep(Duration::from_millis(10));
                }
                seen_worker.store(
                    worker_token.is_cancelled(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            }) => panic!("worker finished before the connect future was dropped"),
            () = started.notified() => {}
        }
        let start = Instant::now();
        while !seen.load(std::sync::atomic::Ordering::SeqCst)
            && start.elapsed() < Duration::from_secs(2)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            "dropping the join future must cancel the worker token"
        );
    }

    /// A pipe whose reader is gone fails every write with EPIPE, like the dead tty a closed pane
    /// leaves behind; the notice must report that instead of panicking.
    #[test]
    fn join_notice_survives_dead_stderr() {
        let (reader, mut dead_stderr) = std::io::pipe().expect("pipe");
        drop(reader);
        assert!(!write_join_notice(&mut dead_stderr));
    }

    #[test]
    fn join_notice_reports_a_landed_write() {
        let mut stderr = Vec::new();
        assert!(write_join_notice(&mut stderr));
        assert_eq!(stderr, format!("{JOIN_NOTICE}\n").into_bytes());
    }

    #[test]
    fn panic_payloads_render_as_text() {
        assert_eq!(
            classify_join(Err(Box::new("boom"))),
            JoinOutcome::Panicked("boom".to_string())
        );
        assert_eq!(
            classify_join(Err(Box::new("boom".to_string()))),
            JoinOutcome::Panicked("boom".to_string())
        );
        assert_eq!(
            classify_join(Err(Box::new(7u32))),
            JoinOutcome::Panicked("non-string panic payload".to_string())
        );
    }
}
