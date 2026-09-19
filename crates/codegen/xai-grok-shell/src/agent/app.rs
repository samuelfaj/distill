use crate::agent::config::{Config as AgentConfig, ModelEntry};
use crate::agent::init::{bootstrap_with_cancel, exit_on_config_error};
use crate::agent::mvp_agent::MvpAgent;
use crate::agent::remote_config::{ModelFetchAuth, prefetch_models_blocking};
use crate::leader::CursorWorkerStartArgs;
use crate::leader::protocol::InternalMethod;
use crate::util::grok_home;
use agent_client_protocol as acp;
use parking_lot::Mutex;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, simplex};
use tokio::sync::{Mutex as TokioMutex, mpsc};
use tokio::time::Duration;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{debug, info, warn};
use xai_acp_lib::{
    AcpAgentGatewayReceiver as GatewayReceiver, AcpAgentGatewaySender as GatewaySender,
    LineBufferedRead,
};
#[cfg(test)]
use xai_grok_login::AuthMode;
use xai_grok_login::{AuthManager, GrokAuth, GrokComConfig, run_auth_flow};
const MAX_BUFFER_SIZE: usize = 8 * 1024 * 1024;
use indexmap::IndexMap;
/// Configuration for periodic auto-update checking in leader mode. A long-running leader periodically calls `check_fn` to check for updates.
/// `check_fn` both detects whether a newer version is available **and** downloads/installs it.
/// It returns `true` only when the new binary is on disk and the leader should shut down so the next `connect_or_spawn` picks it up. If the download fails, `check_fn` should return `false` so the leader stays alive and retries on the next interval.
pub struct LeaderAutoUpdateConfig {
    /// Interval between update checks (default: 1 hour).
    pub check_interval: Duration,
    /// Async function that checks for, downloads, and installs an update.
    /// Returns `true` if the update was installed successfully and the leader should shut down.
    /// Returns `false` to stay alive (no update, or download failed).
    pub check_fn:
        Box<dyn Fn() -> Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync>,
}
/// Timeout for a single check_fn call. The check_fn may include both a version check and a binary download, so this must cover large downloads on slow connections.
/// Kept in sync with the artifact download timeout (20 minutes) so the leader does not abandon a transfer still within the HTTP client's budget.
/// If the call takes longer than this, we abandon the attempt and retry on the next interval. The select! with the cancellation token keeps the loop responsive to shutdown signals even while waiting.
const AUTO_UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// How long the auto-update shutdown waits for session actors to flush before the leader exits.
/// Aliases the shared [`crate::agent::activity::SESSION_FLUSH_GRACE`].
/// This path and the in-process agent's `/exit` / headless-quit flush therefore cannot drift apart.
const AUTO_UPDATE_FLUSH_GRACE: Duration = crate::agent::activity::SESSION_FLUSH_GRACE;
const PERSISTENT_EXIT_DRAIN: Duration = Duration::from_secs(1);
/// Consecutive busy deferrals after which an installed update proceeds anyway (with the graceful flush).
/// Bounds how long a permanently-"busy" signal (an orphaned parked interaction, a wedged turn) can pin the leader to an old binary. The cap is ~24h at the default 1h check interval.
/// Mirrors the bounded grace of the `RelaunchForUpdate` drain.
const MAX_AUTO_UPDATE_BUSY_DEFERRALS: u32 = 24;
/// Bounded wait for the leader flock when it is held but no socket is bound yet.
/// Causes: a spawner mid-handoff, an old-flow client holding the flock across its ~10s spawn window, or a same-version sibling briefly holding it.
/// Exceeds that old-flow window so a legitimately-spawning peer wins the race.
const LEADER_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);
/// Run the auto-update checker loop. The second signal covers relay-driven (grok.com WebSocket) leaders, whose traffic bypasses the IPC server and never sets `agent_busy`.
/// [`MAX_AUTO_UPDATE_BUSY_DEFERRALS`] bounds the deferrals; past it the update proceeds anyway (still flushing first).
/// So a permanently-busy signal (orphaned parked interaction, wedged turn) cannot pin the leader to an old binary forever. A stalled download therefore cannot block the loop from responding to shutdown signals. Extracted as a standalone function so it can be unit-tested independently from the full leader infrastructure.
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn run_auto_update_checker(
    config: LeaderAutoUpdateConfig,
    agent_busy: Arc<AtomicBool>,
    activity: crate::agent::activity::AgentActivity,
    cancel: tokio_util::sync::CancellationToken,
    shutdown_tx: tokio::sync::watch::Sender<crate::leader::ShutdownReason>,
) {
    let mut interval = tokio::time::interval(config.check_interval);
    interval.tick().await;
    let mut busy_deferrals: u32 = 0;
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = cancel.cancelled() => break,
        }
        info!("Leader auto-update: running update check");
        let update_installed = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = tokio::time::timeout(AUTO_UPDATE_CHECK_TIMEOUT, (config.check_fn)()) => {
                match result {
                    Ok(installed) => installed,
                    Err(_elapsed) => {
                        warn!("Leader auto-update: check/download timed out, will retry next interval");
                        continue;
                    }
                }
            }
        };
        if update_installed {
            let busy = agent_busy.load(Ordering::Relaxed) || activity.is_busy();
            if busy && busy_deferrals < MAX_AUTO_UPDATE_BUSY_DEFERRALS {
                busy_deferrals += 1;
                info!(
                    busy_deferrals,
                    "Leader auto-update: update installed but agent is busy, deferring shutdown"
                );
                continue;
            }
            if busy {
                warn!(
                    busy_deferrals,
                    "Leader auto-update: deferral limit reached while busy; shutting down anyway"
                );
            } else {
                info!("Leader auto-update: update installed and agent is idle, shutting down");
            }
            activity.flush_all_sessions(AUTO_UPDATE_FLUSH_GRACE).await;
            let _ = shutdown_tx.send(crate::leader::ShutdownReason::AutoUpdate);
            cancel.cancel();
            break;
        } else {
            info!("Leader auto-update: no update installed");
        }
    }
}
/// Spawn the agent inside a LocalSet and return a handle to the I/O future.
fn spawn_agent_local(
    agent_config: AgentConfig,
    auth_manager: Arc<AuthManager>,
    prefetched_models: Option<IndexMap<String, ModelEntry>>,
    boot: Option<crate::agent::init::BootstrapPrefetch>,
    memory_config: Option<crate::config::MemoryConfig>,
    outgoing: impl futures::AsyncWrite + Unpin + 'static,
    incoming: impl futures::AsyncRead + Unpin + 'static,
) -> impl std::future::Future<Output = Result<(), acp::Error>> {
    let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
    let gateway = GatewaySender::new(gw_tx);
    let mut agent = MvpAgent::new(
        gateway,
        &agent_config,
        auth_manager,
        prefetched_models,
        boot,
    )
    .unwrap_or_else(exit_on_config_error);
    agent.models_manager.spawn_background_refresh();
    if let Some(mc) = memory_config {
        agent.set_memory_config(mc);
    }
    let incoming = LineBufferedRead::spawn_local(incoming);
    let (conn, handle_io) = acp::AgentSideConnection::new(agent, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });
    tokio::task::spawn_local(
        GatewayReceiver::new(gw_rx, conn)
            .with_on_meta(xai_grok_otel::span_from_meta_traceparent)
            .run(),
    );
    handle_io
}
fn internal_reload_request_line(
    id: &str,
    method: InternalMethod,
    params: serde_json::Value,
) -> String {
    crate::leader::protocol::internal_request_line(id, method, params)
}
/// Start a skills file watcher and wire it to inject `x.ai/internal/reload_skills` messages into the shared ACP incoming stream.
/// The messages fire when SKILL.md files change on disk.
/// Returns the watcher task, or `None` if no directories could be watched.
fn spawn_skills_file_watcher<W>(
    acp_incoming_tx: &Arc<TokioMutex<W>>,
    skills_paths: &[String],
) -> Option<tokio::task::JoinHandle<()>>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let cwd = std::env::current_dir().unwrap_or_default();
    let workspace_user_dir = xai_grok_agent::prompt::workspace_user::optional_workspace_user_dir();
    let (mut watcher, mut skills_rx) = crate::config::watcher::SkillsFileWatcher::start(
        Some(cwd.as_path()),
        workspace_user_dir.as_deref(),
        skills_paths,
    )?;
    let skills_tx = acp_incoming_tx.clone();
    let task = tokio::spawn(async move {
        while let Some(change) = skills_rx.recv().await {
            let created_discovery_dir = watcher.refresh_new_discovery_dirs();
            let (id, method) = match change {
                crate::config::watcher::DiscoveryChange::Skills if !created_discovery_dir => {
                    info!("Skill directory changed on disk, reloading skills for all sessions");
                    ("skills-reload", InternalMethod::ReloadSkills)
                }
                crate::config::watcher::DiscoveryChange::Skills => {
                    info!("Discovery directory created on disk, reloading skills and workflows");
                    ("skills-reload", InternalMethod::ReloadSkills)
                }
                crate::config::watcher::DiscoveryChange::Workflows => {
                    info!(
                        "Workflow directory changed on disk, re-advertising commands for all sessions"
                    );
                    ("workflows-reload", InternalMethod::ReloadWorkflows)
                }
            };
            let line = internal_reload_request_line(id, method, serde_json::json!({}));
            let mut tx = skills_tx.lock().await;
            if let Err(e) = tx.write_all(line.as_bytes()).await {
                warn!(
                    error = %e,
                    "failed to inject skills reload into ACP stream"
                );
            }
        }
    });
    Some(task)
}
/// Register the process-lifetime runtime for shared filesystem watchers ([`xai_fsnotify::shared`]).
/// Their event loops then run on a runtime that outlives individual sessions (each session builds its own short-lived runtime).
/// Idempotent; safe to call from every agent entrypoint.
fn register_fs_watch_runtime() {
    xai_fsnotify::set_runtime_handle(tokio::runtime::Handle::current());
}
#[tracing::instrument(level = "debug", skip_all)]
pub async fn run_stdio_agent(
    agent_config: &AgentConfig,
    prefetched_models: Option<IndexMap<String, ModelEntry>>,
    memory_config: Option<crate::config::MemoryConfig>,
) -> anyhow::Result<()> {
    register_fs_watch_runtime();
    if let Err(error) = xai_tty_utils::kill_current_process_on_parent_death() {
        tracing::warn!(
            %error,
            "failed to bind to parent death; agent will not die with its \
             parent — stdin EOF remains the only cleanup"
        );
    }
    xai_grok_telemetry::unified_log::set_version(xai_grok_version::VERSION);
    xai_file_utils::queue::cleanup_orphaned_uploads(
        &grok_home::grok_home(),
        xai_file_utils::queue::DEFAULT_MAX_AGE,
    );
    if let Ok(version) = std::env::var("GROK_CLIENT_VERSION") {
        crate::unified_log::info(
            "GROK_CLIENT_VERSION",
            None,
            Some(serde_json::json!({ "version": version })),
        );
    }
    let _total_timer = crate::instrumentation_timer!("startup.stdio_agent_total");
    let outgoing = tokio::io::stdout().compat_write();
    let mut agent_config = agent_config.clone();
    let (acp_incoming_rx, acp_incoming_tx) = simplex(MAX_BUFFER_SIZE);
    let incoming = acp_incoming_rx.compat();
    let acp_incoming_tx = Arc::new(TokioMutex::new(acp_incoming_tx));
    let stdin_tx = acp_incoming_tx.clone();
    let (stdin_closed_tx, stdin_closed_rx) = tokio::sync::oneshot::channel();
    let mut stdin_lines = xai_acp_lib::spawn_stdin_line_reader();
    tokio::spawn(async move {
        while let Some(line) = stdin_lines.recv().await {
            let mut tx = stdin_tx.lock().await;
            if tx.write_all(&line).await.is_err() {
                break;
            }
        }
        let _ = stdin_closed_tx.send(());
    });
    let _skills_watcher = spawn_skills_file_watcher(&acp_incoming_tx, &agent_config.skills.paths);
    let local_set = tokio::task::LocalSet::new();
    let agent_cancel = tokio_util::sync::CancellationToken::new();
    let _cancel_on_exit = agent_cancel.clone().drop_guard();
    let cancel_for_agent = agent_cancel.clone();
    let result = local_set
        .run_until(async move {
            let simplex_tx = acp_incoming_tx;
            tokio::task::spawn_local(async move {
                let _ = stdin_closed_rx.await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                let mut tx = simplex_tx.lock().await;
                let _ = tx.shutdown().await;
            });
            let auth_manager = Arc::new(agent_config.create_auth_manager());
            auth_manager.start_proactive_refresh(cancel_for_agent.clone());
            auth_manager.start_system_power_listener();
            crate::managed_config::ensure_managed_policy_present(&auth_manager).await;
            let boot = crate::agent::init::resolve_boot_startup_settings(
                &mut agent_config,
                &cancel_for_agent,
                prefetched_models.is_none(),
                auth_manager.current(),
            )
            .await?;
            apply_otel_config(&auth_manager, &agent_config.grok_com_config);
            let handle_io = spawn_agent_local(
                agent_config,
                auth_manager,
                prefetched_models,
                Some(boot),
                memory_config,
                outgoing,
                incoming,
            );
            handle_io.await?;
            Ok::<(), anyhow::Error>(())
        })
        .await;
    agent_cancel.cancel();
    crate::terminal::pty_session::close_all().await;
    crate::upload::drain_pending_uploads(PERSISTENT_EXIT_DRAIN).await;
    xai_grok_telemetry::session_ctx::drain_at_process_exit().await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    result
}
#[tracing::instrument(level = "debug", skip_all)]
pub async fn run_headless(
    agent_config: &AgentConfig,
    reauthenticate: bool,
    memory_config: Option<crate::config::MemoryConfig>,
) -> anyhow::Result<()> {
    register_fs_watch_runtime();
    xai_grok_telemetry::unified_log::set_version(xai_grok_version::VERSION);
    crate::http::set_process_client_mode_headless();
    use crate::agent::relay::spawn_relay_connection_with_callback;
    use tokio_util::sync::CancellationToken;
    const HEADLESS_NO_SESSION: &str = "Headless mode requires a grok.com session. \
        Run `grok login` to sign in, or use `grok agent stdio` for API-key access.";
    xai_file_utils::queue::cleanup_orphaned_uploads(
        &grok_home::grok_home(),
        xai_file_utils::queue::DEFAULT_MAX_AGE,
    );
    let mut agent_config = agent_config.clone();
    agent_config.mode = crate::agent::config::AgentMode::Headless;
    let ctx = &agent_config.grok_com_config;
    let (mut auth, did_browser_flow) = if reauthenticate {
        let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
            &grok_home::grok_home(),
            ctx.clone(),
            crate::agent::config::EndpointsConfig::from_effective_config().proxy_url(),
        ));
        run_auth_flow(
            &auth_manager,
            ctx,
            agent_config.login_device_flow,
            true,
            None,
            None,
            None,
            xai_grok_login::LoginTransportOverride::None,
        )
        .await?
    } else {
        let auth_manager = Arc::new(AuthManager::new_with_proxy_base_url(
            &grok_home::grok_home(),
            ctx.clone(),
            crate::agent::config::EndpointsConfig::from_effective_config().proxy_url(),
        ));
        if crate::agent::auth_method::has_xai_api_key_env()
            && ctx.auth_provider_command.is_none()
            && xai_grok_login::try_ensure_fresh_auth(ctx, auth_manager.proxy_base_url().to_string())
                .await
                .is_none()
        {
            anyhow::bail!("{HEADLESS_NO_SESSION}");
        }
        run_auth_flow(
            &auth_manager,
            ctx,
            agent_config.login_device_flow,
            false,
            None,
            None,
            None,
            xai_grok_login::LoginTransportOverride::None,
        )
        .await?
    };
    if auth.user_id.is_empty() || auth.email.is_none() {
        auth = Arc::new(agent_config.create_auth_manager())
            .update(auth.clone())
            .await?;
    }
    let auth_for_prefetch = auth.clone();
    let endpoints_for_prefetch = agent_config.endpoints.clone();
    let fetch_auth_for_prefetch = ModelFetchAuth::resolve(&endpoints_for_prefetch, true);
    let prefetched_models = tokio::task::spawn_blocking(move || {
        prefetch_models_blocking(
            &endpoints_for_prefetch,
            Some(&auth_for_prefetch),
            fetch_auth_for_prefetch,
        )
    })
    .await
    .ok()
    .flatten();
    tracing::info!("Prefetched models: {:?}", prefetched_models);
    let (ws_to_agent_tx, mut ws_to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (acp_incoming_rx, acp_incoming_tx) = simplex(MAX_BUFFER_SIZE);
    let (acp_outgoing_rx, acp_outgoing_tx) = simplex(MAX_BUFFER_SIZE);
    let incoming = acp_incoming_rx.compat();
    let outgoing = acp_outgoing_tx.compat_write();
    let acp_incoming_tx = Arc::new(TokioMutex::new(acp_incoming_tx));
    let shared_auth_manager = Arc::new(agent_config.create_auth_manager());
    let Some(relay_config) =
        relay_config_for_session(Some(&auth), &agent_config, &shared_auth_manager)
    else {
        anyhow::bail!("{HEADLESS_NO_SESSION}");
    };
    let grok_code_url = format!("{}/build", ctx.grok_ws_origin);
    let on_first_connect: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
        if !did_browser_flow {
            eprintln!();
            eprintln!(
                "Open Remote-Code: {} (press Enter to open in browser)",
                grok_code_url
            );
            eprintln!();
            let url_for_open = grok_code_url.clone();
            std::thread::spawn(move || {
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
                let _ = webbrowser::open(&url_for_open);
            });
        }
    });
    let cancel = CancellationToken::new();
    let (agent_to_ws_tx, _relay_handle) = spawn_relay_connection_with_callback(
        relay_config,
        ws_to_agent_tx.clone(),
        Some(cancel.clone()),
        Some(on_first_connect),
    );
    let local_set = tokio::task::LocalSet::new();
    let mut agent_config_clone = agent_config.clone();
    let memory_config_for_first = memory_config;
    let agent_cancel = cancel.clone();
    local_set
        .run_until(async move {
            let _agent_handle = tokio::task::spawn_local(async move {
                let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway = GatewaySender::new(gw_tx);
                let auth_manager = shared_auth_manager;
                auth_manager.start_proactive_refresh(agent_cancel.clone());
                crate::managed_config::ensure_managed_policy_present(&auth_manager)
                    .await;
                let boot = match crate::agent::init::resolve_boot_startup_settings(
                        &mut agent_config_clone,
                        &agent_cancel,
                        prefetched_models.is_none(),
                        auth_manager.current(),
                    )
                    .await
                {
                    Ok(boot) => boot,
                    Err(crate::agent::init::BootstrapError::Cancelled) => return,
                    Err(err) => exit_on_config_error(err),
                };
                let mut agent = MvpAgent::new(
                        gateway,
                        &agent_config_clone,
                        auth_manager,
                        prefetched_models,
                        Some(boot),
                    )
                    .unwrap_or_else(exit_on_config_error);
                agent.models_manager.spawn_background_refresh();
                if let Some(mc) = memory_config_for_first {
                    agent.set_memory_config(mc);
                }
                let incoming = LineBufferedRead::spawn_local(incoming);
                let (conn, handle_io) = acp::AgentSideConnection::new(
                    agent,
                    outgoing,
                    incoming,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                tokio::task::spawn_local(
                    GatewayReceiver::new(gw_rx, conn)
                        .with_on_meta(xai_grok_otel::span_from_meta_traceparent)
                        .run(),
                );
                if let Err(e) = handle_io.await {
                    warn!(error = ?e, "Agent I/O handler error");
                }
                info!("Agent task completed");
            });
            let ws_tx = acp_incoming_tx.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = ws_to_agent_rx.recv().await {
                    let mut tx = ws_tx.lock().await;
                    if tx.write_all(msg.as_bytes()).await.is_err() {
                        warn!("Failed to write to agent incoming stream");
                        break;
                    }
                    if tx.write_all(b"\n").await.is_err() {
                        break;
                    }
                }
                info!("WS to agent bridge task completed");
            });
            let _skills_watcher = spawn_skills_file_watcher(
                &acp_incoming_tx,
                &agent_config.skills.paths,
            );
            tokio::task::spawn_local(async move {
                let mut reader = BufReader::new(acp_outgoing_rx);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => {
                            info!("Agent outgoing stream EOF");
                            break;
                        }
                        Ok(_) => {
                            let msg = line.trim_end_matches(['\r', '\n']).to_string();
                            if !msg.is_empty()
                                && agent_to_ws_tx.send(msg.clone()).is_err()
                            {
                                debug!("No active websocket, dropping outbound message (persisted to disk)");
                            }
                        }
                        Err(e) => {
                            warn!(error = ?e, "Error reading from agent outgoing stream");
                            break;
                        }
                    }
                }
                info!("Agent to WS bridge task completed");
            });
            cancel.cancelled().await;
            anyhow::Ok(())
        })
        .await?;
    crate::upload::drain_pending_uploads(PERSISTENT_EXIT_DRAIN).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    Ok(())
}
/// Whether the relay's shared [`AuthManager`] should be (re)seeded with the startup-resolved `session`. Staleness is compared by `create_time`, which is always present and bumped on every mint/refresh/login.
/// The narrow "seed only when empty" predicate was insufficient. On a read-only disk, login's `update()` falls back to in-memory-only.
/// The freshly constructed manager can then load an *older* scope entry from disk that login could not overwrite. Seeding only when empty would pin the manager (and relay 401 recovery) to that stale snapshot. Never clobbers an equal-or-fresher token: the same key (already in sync) or a token whose `create_time` is newer.
fn should_seed_shared_session(existing: Option<&GrokAuth>, session: &GrokAuth) -> bool {
    match existing {
        None => true,
        Some(existing) => {
            existing.key != session.key && session.create_time >= existing.create_time
        }
    }
}
/// `RelayConfig` for the relay, or `None` for BYOK / no-session. The session gate is `RelayConfig::for_session` (single source of truth). The relay must SHARE the agent's `AuthManager`, never own a private one.
/// A manager without a refresher can only adopt sibling tokens from disk. Relay 401 recovery then dead-ends whenever no other refresher is alive (sleep/wake, auth.json loss), even with a valid refresh token in memory.
/// Sharing also puts relay recovery behind the same in-process `refresh_lock` and `permanent_failure` cache as every other consumer. Concurrent recovery paths therefore cannot double-spend a refresh token.
fn relay_config_for_session(
    auth: Option<&GrokAuth>,
    agent_config: &AgentConfig,
    shared_auth_manager: &Arc<AuthManager>,
) -> Option<crate::agent::relay::RelayConfig> {
    let session = auth?;
    if should_seed_shared_session(shared_auth_manager.current_or_expired().as_ref(), session) {
        shared_auth_manager.hot_swap(session.clone());
    }
    crate::agent::relay::RelayConfig::for_session(
        session,
        &agent_config.grok_com_config,
        agent_config.endpoints.alpha_test_key.clone(),
        Some(shared_auth_manager.clone()),
    )
}
/// A bare leader has no local IPC clients; remote prompts arrive *through* the relay, so it must be up before any demand signal could exist.
/// Gating it on headless registration is a chicken-and-egg deadlock: the agent never registers and tooling reports "No online agents". A leader serving only TUI-dashboard / IDE clients never opens the relay.
/// It then never pays the per-message clone/parse/log/TLS duplication of mirroring every agent message to grok.com. Until the relay starts, `agent_to_ws_tx` stays `None`, so the outbound bridge skips the relay clone entirely. Must be called within a `LocalSet` (uses `spawn_local`).
fn spawn_leader_relay(
    slot: Rc<std::cell::RefCell<Option<crate::agent::relay::RelayHandle>>>,
    relay_config: crate::agent::relay::RelayConfig,
    relay_on_demand: bool,
    mut relay_demand_rx: tokio::sync::watch::Receiver<bool>,
    ws_to_agent_tx: mpsc::UnboundedSender<String>,
    agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    cancel: tokio_util::sync::CancellationToken,
) {
    use crate::agent::relay::spawn_relay_connection;
    if !relay_on_demand {
        info!("Starting relay connection (eager)");
        let (tx, handle) = spawn_relay_connection(relay_config, ws_to_agent_tx, cancel);
        *agent_to_ws_tx.lock() = Some(tx);
        *slot.borrow_mut() = Some(handle);
        return;
    }
    let slot_for_task = slot.clone();
    tokio::task::spawn_local(async move {
        while !*relay_demand_rx.borrow() {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                changed = relay_demand_rx.changed() => {
                    if changed.is_err() {
                        // IPC server gone (sender dropped): leader is shutting down; never start the relay
                        return;
                    }
                }
            }
        }
        info!("Headless client registered; starting relay connection");
        let (tx, handle) = spawn_relay_connection(relay_config, ws_to_agent_tx, cancel);
        *agent_to_ws_tx.lock() = Some(tx);
        *slot_for_task.borrow_mut() = Some(handle);
    });
}
/// Everything needed to arm the leader's grok.com relay *after* startup. A leader that boots without auth used to disable the relay forever: the decision was made once in [`run_leader`] and never revisited.
/// On devboxes that turned a transient mint-provider outage at provision time into a permanently invisible box.
/// The external auth provider succeeded minutes later and the config watcher hot-reloaded the token into the leader. But the relay never connected, the agent never registered, and tooling reported the (healthy) box as "not found online" for its whole lifetime.
struct DeferredRelayArm {
    relay_on_demand: bool,
    relay_demand_rx: tokio::sync::watch::Receiver<bool>,
    ws_to_agent_tx: mpsc::UnboundedSender<String>,
    agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    cancel: tokio_util::sync::CancellationToken,
    /// Shared with [`run_leader`]'s shutdown path, which drains it to stop the relay explicitly.
    slot: Rc<std::cell::RefCell<Option<crate::agent::relay::RelayHandle>>>,
    grok_com_config: xai_grok_login::GrokComConfig,
    alpha_test_key: Option<String>,
}
impl DeferredRelayArm {
    /// Arm the relay for a hot-reloaded session if it is relay-eligible. Consumes the parts and returns `None` when the relay was armed.
    /// Returns `Some(self)` when the session is not relay-eligible, so a later eligible token can still arm.
    /// Ineligible means BYOK / non-x.ai issuer; see [`RelayConfig::for_session`](crate::agent::relay::RelayConfig::for_session). Must be called within a `LocalSet` (delegates to [`spawn_leader_relay`]).
    fn arm_if_eligible(self, session: &GrokAuth, auth_manager: &Arc<AuthManager>) -> Option<Self> {
        let Some(relay_config) = crate::agent::relay::RelayConfig::for_session(
            session,
            &self.grok_com_config,
            self.alpha_test_key.clone(),
            Some(auth_manager.clone()),
        ) else {
            return Some(self);
        };
        info!("Relay-eligible auth token appeared after startup — arming grok.com relay");
        spawn_leader_relay(
            self.slot,
            relay_config,
            self.relay_on_demand,
            self.relay_demand_rx,
            self.ws_to_agent_tx,
            self.agent_to_ws_tx,
            self.cancel,
        );
        None
    }
}
/// Close the external-OTEL gate before telemetry init; see [`crate::agent::otel_gate`].
pub fn suppress_otel() {
    crate::agent::otel_gate::suppress();
}
/// Startup external-OTEL gate for an in-process (embedded) agent.
/// Mirrors the leader startup gate so the pager process is fail-closed by construction at the agent boundary.
pub fn apply_otel_config(auth_manager: &AuthManager, grok_com_config: &GrokComConfig) {
    suppress_otel();
    let has_session = auth_manager.current().is_some() || auth_manager.read_disk_auth().is_some();
    if crate::agent::otel_gate::should_open_at_startup(crate::agent::otel_gate::StartupGate {
        channel: crate::agent::otel_gate::resolved_policy_channel(),
        has_session,
        session_pending: crate::agent::otel_gate::is_session_pending(has_session, grok_com_config),
    }) {
        crate::agent::otel_gate::open_at_startup();
    }
}
/// Boot-time switches of [`run_leader`], set by `grok agent leader` flags.
pub struct LeaderRunOptions {
    /// Keep serving after the last IPC client disconnects (devbox / systemd leaders).
    pub no_exit_on_disconnect: bool,
    /// Defer the grok.com relay until the first headless client registers.
    pub relay_on_demand: bool,
    pub auto_update_check: Option<LeaderAutoUpdateConfig>,
    pub memory_config: Option<crate::config::MemoryConfig>,
    /// Start the worker door after readiness; `None` defers to `[cursor_worker] auto_start`.
    /// Inert on a build without worker support.
    pub cursor_worker: Option<CursorWorkerStartArgs>,
}
/// Run the agent in leader mode, accepting IPC connections from multiple clients. When a grok.com session is present, the leader connects to the websocket relay after startup (post-auth, post-prefetch).
/// BYOK / no-session leaders start serving clients over IPC only. A relay-eligible token hot-reloaded later arms the relay via [`DeferredRelayArm`]. IPC server started (`tokio::spawn`); socket bound HERE, before auth.
/// Bounded non-interactive auth (no blocking model/settings prefetch; those stream in after readiness). `None` (BYOK / no session) is not an error: the relay stays off and a background cold-mint / re-login can start it later.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn run_leader(
    agent_config: &AgentConfig,
    options: LeaderRunOptions,
) -> anyhow::Result<()> {
    use crate::leader::roster_merge::{ExternalRoster, RosterListMerge, run_changed_notifier};
    use crate::leader::{
        LeaderLock, LeaderServerControlState, LeaderServerMetadata, LockError, ShutdownReason,
        compute_ws_url_suffix, run_leader_server,
    };
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    let LeaderRunOptions {
        no_exit_on_disconnect,
        relay_on_demand,
        auto_update_check,
        memory_config,
        cursor_worker: cursor_worker_boot,
    } = options;
    register_fs_watch_runtime();
    xai_grok_telemetry::unified_log::set_version(xai_grok_version::VERSION);
    tokio::task::spawn_blocking(|| {
        xai_file_utils::queue::cleanup_orphaned_uploads(
            &grok_home::grok_home(),
            xai_file_utils::queue::DEFAULT_MAX_AGE,
        );
    });
    let mut agent_config = agent_config.clone();
    agent_config.mode = crate::agent::config::AgentMode::Leader;
    let ws_url = &agent_config.grok_com_config.grok_ws_url;
    let mut lock = LeaderLock::new(ws_url);
    let socket_path = lock.socket_path().clone();
    match lock.try_acquire() {
        Ok(true) => {
            lock.write_pid()?;
            debug!("Acquired leader lock, proceeding as leader");
        }
        Ok(false) => {
            if crate::leader::listener_is_ready(&socket_path) {
                info!(
                    "Another process holds the leader lock with a bound socket ({}). \
                     Exiting so the client adopts it.",
                    socket_path.display()
                );
                return Err(anyhow::anyhow!(
                    "Another leader already holds the lock at {}",
                    socket_path.display()
                ));
            }
            match lock.acquire_reopen_timeout(LEADER_ACQUIRE_TIMEOUT).await {
                Ok(()) => {
                    lock.write_pid()?;
                    debug!("Acquired leader lock after bounded wait, proceeding as leader");
                }
                Err(LockError::Timeout(_)) => {
                    info!(
                        "Timed out waiting for the leader lock ({}). Exiting so the \
                         client adopts whoever won it.",
                        socket_path.display()
                    );
                    return Err(anyhow::anyhow!(
                        "Timed out acquiring leader lock at {}",
                        socket_path.display()
                    ));
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("Failed to acquire leader lock: {}", e));
                }
            }
        }
        Err(e) => return Err(anyhow::anyhow!("Failed to acquire leader lock: {}", e)),
    }
    lock.cleanup_socket()?;
    info!("Leader server starting");
    let (ipc_to_agent_tx, mut ipc_to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (agent_to_ipc_tx, agent_to_ipc_rx) = mpsc::unbounded_channel::<String>();
    let (ws_to_agent_tx, mut ws_to_agent_rx) = mpsc::unbounded_channel::<String>();
    let (acp_incoming_rx, acp_incoming_tx) = simplex(MAX_BUFFER_SIZE);
    let (acp_outgoing_rx, acp_outgoing_tx) = simplex(MAX_BUFFER_SIZE);
    let incoming = acp_incoming_rx.compat();
    let outgoing = acp_outgoing_tx.compat_write();
    let acp_incoming_tx = Arc::new(TokioMutex::new(acp_incoming_tx));
    let cancel = CancellationToken::new();
    let _cancel_on_exit = cancel.clone().drop_guard();
    let (ready_tx, ready_rx) = watch::channel(false);
    let (shutdown_tx, _shutdown_reason_rx) = watch::channel(ShutdownReason::Manual);
    let (relay_demand_tx, relay_demand_rx) = watch::channel(false);
    let client_count = Arc::new(AtomicUsize::new(0));
    let agent_busy = Arc::new(AtomicBool::new(false));
    let agent_activity = crate::agent::activity::AgentActivity::default();
    let external_roster = ExternalRoster::new();
    let control_state = LeaderServerControlState::new(LeaderServerMetadata {
        pid: std::process::id(),
        socket_path: socket_path.clone(),
        lock_path: lock.lock_path().clone(),
        ws_url_suffix: compute_ws_url_suffix(ws_url),
        leader_binary_version: xai_grok_version::VERSION.to_string(),
    })
    .with_default_hub_url(agent_config.hub.url.clone())
    .with_cursor_worker(
        agent_config.cursor_worker.clone(),
        agent_config.hub.url.clone(),
        grok_home::grok_home(),
        external_roster.clone(),
    );
    let workspace_control = control_state.workspace.clone();
    let cursor_worker_control = control_state.cursor_worker.clone();
    let leader_pid = control_state.metadata.pid;
    let ipc_server_cancel = cancel.clone();
    let socket_path_for_server = socket_path.clone();
    let client_count_for_server = client_count.clone();
    let agent_busy_for_server = agent_busy.clone();
    let agent_activity_for_server = agent_activity.clone();
    let shutdown_tx_for_server = shutdown_tx.clone();
    let ipc_handle = tokio::spawn(async move {
        if let Err(e) = run_leader_server(
            socket_path_for_server,
            ipc_to_agent_tx,
            agent_to_ipc_rx,
            ipc_server_cancel,
            no_exit_on_disconnect,
            client_count_for_server,
            agent_busy_for_server,
            agent_activity_for_server,
            ready_rx,
            relay_demand_tx,
            shutdown_tx_for_server,
            None,
            control_state,
        )
        .await
        {
            warn!(error = ?e, "Leader server error");
        }
    });
    let socket_ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !crate::leader::listener_is_ready(&socket_path) {
        if tokio::time::Instant::now() >= socket_ready_deadline {
            return Err(anyhow::anyhow!(
                "Timeout waiting for IPC socket to be created"
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    debug!("IPC socket created");
    let _lock = lock;
    let ctx = &agent_config.grok_com_config;
    suppress_otel();
    let auth: Option<GrokAuth> =
        xai_grok_login::try_noninteractive_auth_no_mint(ctx, agent_config.endpoints.proxy_url())
            .await;
    let has_session = auth.is_some()
        || agent_config
            .create_auth_manager()
            .read_disk_auth()
            .is_some();
    let session_pending =
        crate::agent::otel_gate::is_session_pending(has_session, &agent_config.grok_com_config);
    let policy_channel =
        crate::agent::otel_gate::policy_channel_for(&agent_config.endpoints.proxy_url());
    if crate::agent::otel_gate::should_open_at_startup(crate::agent::otel_gate::StartupGate {
        channel: policy_channel,
        has_session,
        session_pending,
    }) {
        info!(
            channel = ?policy_channel,
            has_session,
            session_pending,
            "Opening external-OTEL gate at startup: no fleet policy is pending for this leader"
        );
        crate::agent::otel_gate::open_at_startup();
    }
    let _ = ready_tx.send(true);
    info!(
        "Leader ready: local-only boot (model/settings refresh runs in background), ACP forwarding enabled"
    );
    let local_set = tokio::task::LocalSet::new();
    let mut agent_config_for_spawn = agent_config.clone();
    agent_config_for_spawn.remote_settings = None;
    crate::util::config::sync_campaign_fields(&mut agent_config_for_spawn);
    let agent_to_ipc_tx_clone = agent_to_ipc_tx.clone();
    let cancel_clone = cancel.clone();
    let shared_auth_manager = Arc::new(agent_config_for_spawn.create_auth_manager());
    shared_auth_manager.start_proactive_refresh(cancel_clone.clone());
    shared_auth_manager.start_system_power_listener();
    if let Some(session) = auth.as_ref()
        && should_seed_shared_session(shared_auth_manager.current_or_expired().as_ref(), session)
    {
        shared_auth_manager.hot_swap(session.clone());
    }
    let relay_config = relay_config_for_session(auth.as_ref(), &agent_config, &shared_auth_manager);
    workspace_control.set_auth_manager(shared_auth_manager.clone());
    cursor_worker_control.set_auth_manager(shared_auth_manager.clone());
    let auth_manager_for_agent = shared_auth_manager.clone();
    let auth_manager_for_config = shared_auth_manager.clone();
    let auth_manager_for_mint = shared_auth_manager.clone();
    crate::managed_config::ensure_managed_policy_present(&auth_manager_for_agent).await;
    let boot = crate::agent::init::resolve_boot_startup_settings(
        &mut agent_config_for_spawn,
        &cancel_clone,
        true,
        auth_manager_for_agent.current(),
    )
    .await?;
    let (agent_config_for_spawn, shared_models_manager) = match bootstrap_with_cancel(
        &agent_config_for_spawn,
        &auth_manager_for_agent,
        None,
        &cancel_clone,
        Some(boot),
    ) {
        Ok(v) => v,
        Err(e @ crate::agent::init::BootstrapError::Cancelled) => return Err(e.into()),
        Err(e) => exit_on_config_error(e),
    };
    shared_models_manager.spawn_background_refresh();
    let models_manager_for_agent = shared_models_manager.clone();
    let models_manager_for_config = shared_models_manager;
    let recursive_config_watch_enabled = {
        let user_cfg = crate::config::load_from_disk().ok();
        let requirements = crate::agent::config::read_requirements_toml();
        crate::util::config::resolve_mcp_recursive_config_watch(
            requirements.as_ref(),
            user_cfg.as_ref(),
            None,
        )
    };
    local_set
        .run_until(async move {
            let (config_watcher_path_tx, config_watcher_path_rx_opt) = if recursive_config_watch_enabled {
                let (tx, rx) = mpsc::unbounded_channel::<std::path::PathBuf>();
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            let mut config_watcher_path_rx = config_watcher_path_rx_opt;
            let agent_config_watcher_path_tx = config_watcher_path_tx.clone();
            let agent_activity_for_agent = agent_activity.clone();
            tokio::task::spawn_local(async move {
                let (gw_tx, gw_rx) = tokio::sync::mpsc::unbounded_channel();
                let gateway = GatewaySender::new(gw_tx);
                let mut agent = MvpAgent::with_models(
                    gateway,
                    &agent_config_for_spawn,
                    auth_manager_for_agent,
                    models_manager_for_agent,
                );
                agent.set_activity(agent_activity_for_agent);
                if let Some(mc) = memory_config {
                    agent.set_memory_config(mc);
                }
                if let Some(tx) = agent_config_watcher_path_tx {
                    agent.set_config_watcher_path_tx(tx);
                }
                let incoming = LineBufferedRead::spawn_local(incoming);
                let (conn, handle_io) = acp::AgentSideConnection::new(
                    agent,
                    outgoing,
                    incoming,
                    |fut| {
                        tokio::task::spawn_local(fut);
                    },
                );
                tokio::task::spawn_local(
                    GatewayReceiver::new(gw_rx, conn)
                        .with_on_meta(xai_grok_otel::span_from_meta_traceparent)
                        .run(),
                );
                if let Err(e) = handle_io.await {
                    warn!(error = ?e, "Agent I/O handler error");
                }
                info!("Agent task completed");
            });
            let roster_merge = Rc::new(RosterListMerge::new(external_roster.clone()));
            let acp_incoming_tx_ipc = acp_incoming_tx.clone();
            let roster_merge_ipc = roster_merge.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = ipc_to_agent_rx.recv().await {
                    roster_merge_ipc.observe_inbound(&msg);
                    let mut tx = acp_incoming_tx_ipc.lock().await;
                    if tx.write_all(msg.as_bytes()).await.is_err()
                        || tx.write_all(b"\n").await.is_err()
                    {
                        warn!("Failed to write IPC message to agent");
                        break;
                    }
                }
            });
            let acp_incoming_tx_ws = acp_incoming_tx.clone();
            let roster_merge_ws = roster_merge.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = ws_to_agent_rx.recv().await {
                    roster_merge_ws.observe_inbound(&msg);
                    let mut tx = acp_incoming_tx_ws.lock().await;
                    if tx.write_all(msg.as_bytes()).await.is_err()
                        || tx.write_all(b"\n").await.is_err()
                    {
                        warn!("Failed to write WS message to agent");
                        break;
                    }
                }
            });
            let agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>> = Rc::new(
                Mutex::new(None),
            );
            let fan_out = {
                let agent_to_ws_tx = agent_to_ws_tx.clone();
                let agent_to_ipc_tx = agent_to_ipc_tx_clone;
                move |msg: String| {
                    let maybe_tx = agent_to_ws_tx.lock();
                    if let Some(ref tx) = *maybe_tx {
                        let _ = tx.send(msg.clone());
                    }
                    drop(maybe_tx);
                    let _ = agent_to_ipc_tx.send(msg);
                }
            };
            let fan_out_agent = fan_out.clone();
            let roster_merge_out = roster_merge.clone();
            tokio::task::spawn_local(async move {
                let mut reader = BufReader::new(acp_outgoing_rx);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            let msg = line.trim_end_matches(['\r', '\n']);
                            if !msg.is_empty() {
                                fan_out_agent(
                                    roster_merge_out.filter_outbound(msg).into_owned(),
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = ?e, "Error reading from agent outgoing stream");
                            break;
                        }
                    }
                }
            });
            tokio::task::spawn_local(run_changed_notifier(external_roster, fan_out));
            if session_pending {
                let mint_auth_manager = auth_manager_for_mint;
                let mint_cancel = cancel_clone.clone();
                tokio::task::spawn_local(async move {
                    tokio::select! {
                        biased;
                        _ = mint_cancel.cancelled() => {}
                        minted = xai_grok_login::mint_session_noninteractive(&mint_auth_manager)
                            => match minted {
                            Some(session) => info!(
                                is_xai = session.is_xai_auth(),
                                "background cold-mint acquired a session post-readiness"
                            ),
                            None => warn!(
                                "background cold-mint found no session; leader remains session-less"
                            ),
                        },
                    }
                });
            }
            let relay_handle_slot: Rc<
                std::cell::RefCell<Option<crate::agent::relay::RelayHandle>>,
            > = Rc::new(std::cell::RefCell::new(None));
            let mut deferred_relay_arm: Option<DeferredRelayArm> = None;
            if let Some(relay_config) = relay_config {
                spawn_leader_relay(
                    relay_handle_slot.clone(),
                    relay_config,
                    relay_on_demand,
                    relay_demand_rx,
                    ws_to_agent_tx.clone(),
                    agent_to_ws_tx.clone(),
                    cancel_clone.clone(),
                );
            } else {
                info!(
                    "Relay not started: no grok.com session token \
                     (BYOK / local-only leader); will arm if an eligible \
                     token is hot-reloaded"
                );
                deferred_relay_arm = Some(DeferredRelayArm {
                    relay_on_demand,
                    relay_demand_rx,
                    ws_to_agent_tx: ws_to_agent_tx.clone(),
                    agent_to_ws_tx: agent_to_ws_tx.clone(),
                    cancel: cancel_clone.clone(),
                    slot: relay_handle_slot.clone(),
                    grok_com_config: agent_config.grok_com_config.clone(),
                    alpha_test_key: agent_config.endpoints.alpha_test_key.clone(),
                });
            }
            {
                let control = cursor_worker_control;
                let cancel = cancel_clone.clone();
                tokio::task::spawn_local(async move {
                    control.start_at_boot(cursor_worker_boot, &cancel, leader_pid).await;
                });
            }
            let update_cancel = cancel_clone.clone();
            if let Some(update_config) = auto_update_check {
                let agent_busy_for_update = agent_busy.clone();
                let agent_activity_for_update = agent_activity.clone();
                let cancel_for_update = cancel_clone.clone();
                tokio::spawn(
                    run_auto_update_checker(
                        update_config,
                        agent_busy_for_update,
                        agent_activity_for_update,
                        cancel_for_update,
                        shutdown_tx,
                    ),
                );
            }
            let cwd_for_watcher = std::env::current_dir().unwrap_or_default();
            let mut watch_paths = crate::config::find_project_configs(&cwd_for_watcher);
            watch_paths
                .extend(crate::util::config::mcp_json_candidate_paths(&cwd_for_watcher));
            if let Some(home) = xai_dirs::home_dir() {
                watch_paths.push(home.join(".claude.json"));
            }
            let auth_scope = agent_config.grok_com_config.auth_scope();
            let initial_auth_key_hash = xai_grok_config::user_grok_home()
                .map(|g| g.join("auth.json"))
                .and_then(|auth_path| xai_grok_login::read_auth_json(&auth_path).ok())
                .and_then(|store| {
                    xai_grok_login::lookup_auth(&store, &auth_scope)
                        .map(|a| crate::config::reloader::hash_auth_key(&a.key))
                })
                .unwrap_or(0);
            let (config_update_tx, mut config_update_rx) = mpsc::unbounded_channel::<
                crate::config::reloader::ConfigUpdate,
            >();
            let watcher_cwd = recursive_config_watch_enabled
                .then_some(cwd_for_watcher.as_path());
            let _config_watcher = if let Some((watcher, events_rx)) = crate::config::watcher::ConfigFileWatcher::start(
                &grok_home::grok_home(),
                &watch_paths,
                watcher_cwd,
                None,
            ) {
                let watcher = std::rc::Rc::new(std::cell::RefCell::new(watcher));
                if let Some(mut rx) = config_watcher_path_rx.take() {
                    let cancel_for_drain = cancel_clone.clone();
                    let watcher_for_drain = watcher.clone();
                    tokio::task::spawn_local(async move {
                        loop {
                            tokio::select! {
                                biased;
                                _ = cancel_for_drain.cancelled() => break,
                                cwd = rx.recv() => match cwd {
                                    Some(cwd) => watcher_for_drain.borrow_mut().watch_path(&cwd),
                                    None => break,
                                },
                            }
                        }
                    });
                }
                let initial_config = crate::config::load_from_disk()
                    .unwrap_or_else(|_| toml::Value::Table(toml::map::Map::new()));
                let reloader = crate::config::reloader::ConfigReloader::new(
                    grok_home::grok_home(),
                    initial_auth_key_hash,
                    initial_config,
                    auth_scope,
                    None,
                    config_update_tx,
                    agent_config.memory_enabled_override,
                );
                tokio::spawn(reloader.run(events_rx, cancel_clone.clone()));
                Some(watcher)
            } else {
                warn!("Config file watcher failed to start; hot-reload disabled");
                None
            };
            let _skills_watcher = spawn_skills_file_watcher(
                &acp_incoming_tx,
                &agent_config.skills.paths,
            );
            let ipc_tx_for_config = agent_to_ipc_tx.clone();
            let acp_tx_for_config = acp_incoming_tx.clone();
            tokio::task::spawn_local(async move {
                use crate::config::reloader::ConfigUpdate;
                while let Some(update) = config_update_rx.recv().await {
                    match update {
                        ConfigUpdate::Auth(auth) => {
                            info!(
                                key_len = auth.key.len(),
                                expires_at = ?auth.expires_at,
                                "Auth token hot-reloaded from config watcher"
                            );
                            xai_grok_telemetry::unified_log::info(
                                "auth hot-swapped from disk",
                                None,
                                Some(
                                    serde_json::json!({
                                    "key_len": auth.key.len(),
                                    "expires_at": auth.expires_at.map(|e| e.to_rfc3339()),
                                }),
                                ),
                            );
                            let session_for_relay = deferred_relay_arm
                                .is_some()
                                .then(|| (*auth).clone());
                            auth_manager_for_config.hot_swap(*auth);
                            if let (Some(arm), Some(session)) = (
                                deferred_relay_arm.take(),
                                session_for_relay,
                            ) {
                                deferred_relay_arm = arm
                                    .arm_if_eligible(&session, &auth_manager_for_config);
                            }
                            models_manager_for_config.on_auth_changed().await;
                            let line = internal_reload_request_line(
                                "config-auth-reloaded",
                                InternalMethod::ReloadAllMcpServers,
                                serde_json::json!({}),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(error = %e, "failed to inject MCP reload after auth hot-swap");
                            }
                        }
                        ConfigUpdate::AuthCleared => {
                            auth_manager_for_config.clear_in_memory();
                            let line = internal_reload_request_line(
                                "config-auth-cleared",
                                InternalMethod::AuthCleared,
                                serde_json::json!({}),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(error = %e, "failed to inject auth-cleared cleanup into ACP stream");
                            }
                            models_manager_for_config.on_auth_changed().await;
                            xai_grok_telemetry::unified_log::warn(
                                "auth cleared from disk",
                                None,
                                None,
                            );
                            info!("Auth cleared by config watcher");
                        }
                        ConfigUpdate::McpServersChanged => {
                            info!("MCP server config change detected — reloading active sessions");
                            let line = internal_reload_request_line(
                                "config-reload-mcp",
                                InternalMethod::ReloadAllMcpServers,
                                serde_json::json!({}),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(error = %e, "failed to inject MCP reload into ACP stream");
                            }
                        }
                        ConfigUpdate::ProjectMcpServersChanged { cwd } => {
                            info!(
                                cwd = %cwd.display(),
                                "project MCP config change detected — reloading matching sessions"
                            );
                            let line = internal_reload_request_line(
                                "config-reload-project-mcp",
                                InternalMethod::ReloadProjectMcpServers,
                                serde_json::json!({ "cwd": cwd.to_string_lossy() }),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(
                                    error = %e,
                                    "failed to inject project MCP reload into ACP stream"
                                );
                            }
                        }
                        ConfigUpdate::ModelsChanged => {
                            info!("Model config change detected — reloading agent model list");
                            let line = internal_reload_request_line(
                                "config-reload-models",
                                InternalMethod::ReloadModels,
                                serde_json::json!({}),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(error = %e, "failed to inject model reload into ACP stream");
                            }
                        }
                        ConfigUpdate::ModelsCacheChanged => {
                            info!("Models cache change detected — reloading agent model catalog");
                            let line = internal_reload_request_line(
                                "config-reload-models-cache",
                                InternalMethod::ReloadModelsCache,
                                serde_json::json!({}),
                            );
                            let mut tx = acp_tx_for_config.lock().await;
                            if let Err(e) = tx.write_all(line.as_bytes()).await {
                                warn!(
                                    error = %e,
                                    "failed to inject models-cache reload into ACP stream"
                                );
                            }
                        }
                        ConfigUpdate::Memory(mem) => {
                            info!(
                                enabled = mem.enabled,
                                "Memory config change detected by watcher"
                            );
                        }
                        ConfigUpdate::Skills(skills) => {
                            info!(
                                paths = skills.paths.len(),
                                "Skills config change detected by watcher"
                            );
                        }
                        ConfigUpdate::Compat(_compat) => {
                            info!(
                                "Compat config change detected by watcher \
                                 (applies on next agent rebuild)"
                            );
                        }
                        ConfigUpdate::Ui { theme, yolo, fork_secondary_model } => {
                            info!("UI config change detected by watcher");
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "x.ai/config_changed",
                                "params": {
                                    "section": "ui",
                                    "changes": {
                                        "theme": theme,
                                        "yolo": yolo,
                                        "fork_secondary_model": fork_secondary_model,
                                    }
                                }
                            });
                            let _ = ipc_tx_for_config.send(notification.to_string());
                        }
                    }
                }
            });
            tokio::select! {
                biased;
                _ = ipc_handle => {
                    info!("IPC server stopped, shutting down leader");
                }
                _ = update_cancel.cancelled() => {
                    info!("Leader cancelled");
                }
            }
            if let Some(relay_handle) = relay_handle_slot.borrow_mut().take() {
                relay_handle.stop();
            }
            anyhow::Ok(())
        })
        .await?;
    crate::upload::drain_pending_uploads(PERSISTENT_EXIT_DRAIN).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;
    /// Create a throwaway shutdown_tx for tests that don't care about the reason.
    fn dummy_shutdown_tx() -> watch::Sender<crate::leader::ShutdownReason> {
        watch::channel(crate::leader::ShutdownReason::Manual).0
    }
    /// Helper: build a LeaderAutoUpdateConfig whose check_fn always returns the given value.
    fn always_config(update_available: bool) -> LeaderAutoUpdateConfig {
        LeaderAutoUpdateConfig {
            check_interval: Duration::from_millis(10),
            check_fn: Box::new(move || Box::pin(async move { update_available })),
        }
    }
    /// Helper: build a LeaderAutoUpdateConfig that returns `false` for the first `skip` calls, then `true` for all subsequent calls.
    fn delayed_update_config(skip: u32) -> LeaderAutoUpdateConfig {
        let counter = Arc::new(AtomicU32::new(0));
        LeaderAutoUpdateConfig {
            check_interval: Duration::from_millis(10),
            check_fn: Box::new(move || {
                let counter = counter.clone();
                Box::pin(async move {
                    let n = counter.fetch_add(1, Ordering::Relaxed);
                    n >= skip
                })
            }),
        }
    }
    fn oidc_session(key: &str, create_time: chrono::DateTime<chrono::Utc>) -> GrokAuth {
        GrokAuth {
            key: key.into(),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
            refresh_token: Some(format!("rt-{key}")),
            create_time,
            expires_at: Some(create_time + chrono::Duration::minutes(15)),
            ..GrokAuth::test_default()
        }
    }
    #[test]
    fn seed_when_manager_empty() {
        let session = oidc_session("resolved", chrono::Utc::now());
        assert!(should_seed_shared_session(None, &session));
    }
    #[test]
    fn skip_when_same_token_already_held() {
        let now = chrono::Utc::now();
        let session = oidc_session("same", now);
        let existing = oidc_session("same", now);
        assert!(!should_seed_shared_session(Some(&existing), &session));
    }
    #[test]
    fn seed_over_staler_disk_entry() {
        let now = chrono::Utc::now();
        let stale = oidc_session("stale-from-disk", now - chrono::Duration::hours(13));
        let session = oidc_session("resolved-at-startup", now);
        assert!(should_seed_shared_session(Some(&stale), &session));
    }
    #[test]
    fn keep_fresher_sibling_refreshed_token() {
        let now = chrono::Utc::now();
        let session = oidc_session("startup", now - chrono::Duration::minutes(5));
        let sibling_fresher = oidc_session("sibling-refreshed", now);
        assert!(!should_seed_shared_session(
            Some(&sibling_fresher),
            &session
        ));
    }
    /// Mock relay WS server: counts accepted WebSocket connections and holds each open so the relay loop doesn't immediately reconnect.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn spawn_mock_relay_server() -> (std::net::SocketAddr, Arc<AtomicU32>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let count = count_clone.clone();
                tokio::spawn(async move {
                    let Ok(_ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
            }
        });
        (addr, count)
    }
    /// A `RelayConfig` built via the production constructor (`for_session`) with a relay-eligible x.ai OIDC session.
    fn test_relay_config(addr: std::net::SocketAddr) -> crate::agent::relay::RelayConfig {
        let auth = GrokAuth {
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
            ..GrokAuth::test_default()
        };
        let cfg = xai_grok_login::GrokComConfig {
            grok_ws_url: format!("ws://{addr}"),
            grok_ws_origin: format!("http://{addr}"),
            ..Default::default()
        };
        crate::agent::relay::RelayConfig::for_session(&auth, &cfg, None, None)
            .expect("x.ai OIDC session must be relay-eligible")
    }
    /// The embedded startup gate (every pager `--no-leader` / fallback path) must be fail-closed by construction.
    /// A session user stays closed until the agent resolves settings, even when an env API key is also present.
    /// The key must not bypass the session's remote policy.
    #[test]
    #[serial_test::serial]
    fn embedded_otel_gate_keeps_a_session_user_fail_closed() {
        use crate::agent::auth_method::{LEGACY_XAI_API_KEY_ENV_VAR, XAI_API_KEY_ENV_VAR};
        use xai_grok_telemetry::external::{
            is_settings_gate_open, mark_external_otel_settings_resolved,
        };
        unsafe fn set_or_clear(key: &str, value: Option<std::ffi::OsString>) {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        /// Restores the api-key env and reopens the gate on drop so no state leaks.
        struct Restore {
            key: Option<std::ffi::OsString>,
            legacy: Option<std::ffi::OsString>,
            proxy: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    set_or_clear(XAI_API_KEY_ENV_VAR, self.key.take());
                    set_or_clear(LEGACY_XAI_API_KEY_ENV_VAR, self.legacy.take());
                    set_or_clear(PROXY_ENV_VAR, self.proxy.take());
                }
                mark_external_otel_settings_resolved();
            }
        }
        const PROXY_ENV_VAR: &str = "GROK_CLI_CHAT_PROXY_BASE_URL";
        let _restore = Restore {
            key: std::env::var_os(XAI_API_KEY_ENV_VAR),
            legacy: std::env::var_os(LEGACY_XAI_API_KEY_ENV_VAR),
            proxy: std::env::var_os(PROXY_ENV_VAR),
        };
        let cfg = GrokComConfig::default();
        unsafe {
            std::env::set_var(XAI_API_KEY_ENV_VAR, "test-key");
            std::env::remove_var(LEGACY_XAI_API_KEY_ENV_VAR);
            std::env::remove_var(PROXY_ENV_VAR);
        }
        let session = GrokAuth {
            expires_at: chrono::DateTime::from_timestamp(9_999_999_999, 0),
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
            ..GrokAuth::test_default()
        };
        let with_session = {
            let dir = tempfile::tempdir().unwrap();
            let am = Arc::new(AuthManager::new(dir.path(), GrokComConfig::default()));
            am.hot_swap(session);
            am
        };
        apply_otel_config(&with_session, &cfg);
        assert!(
            !is_settings_gate_open(),
            "a session user must boot fail-closed even with an env key set"
        );
    }
    /// Wait until at least one relay connection is accepted, or panic.
    #[tracing::instrument(level = "debug", skip_all)]
    async fn wait_for_connection(count: &Arc<AtomicU32>, context: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while count.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "relay never connected: {context}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    /// Regression test for the bare-leader relay gating bug. A bare `grok agent leader` (devbox/systemd: no local IPC clients, `relay_on_demand == false`) must connect the grok.com relay eagerly.
    /// Remote prompts arrive *through* the relay, so on such a leader no headless-registration demand signal can ever fire.
    /// Gating the relay on it means the agent never registers with the backend ("No online agents") even though the box is healthy.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn eager_relay_connects_without_any_ipc_client() {
        let (addr, count) = spawn_mock_relay_server().await;
        let config = test_relay_config(addr);
        let cancel = CancellationToken::new();
        let (ws_to_agent_tx, _ws_to_agent_rx) = mpsc::unbounded_channel();
        let agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>> =
            Rc::new(Mutex::new(None));
        let (_demand_tx, demand_rx) = watch::channel(false);
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let slot = Rc::new(std::cell::RefCell::new(None));
                spawn_leader_relay(
                    slot.clone(),
                    config,
                    false,
                    demand_rx,
                    ws_to_agent_tx,
                    agent_to_ws_tx.clone(),
                    cancel.clone(),
                );
                assert!(
                    slot.borrow().is_some(),
                    "eager mode must park the RelayHandle immediately"
                );
                assert!(
                    agent_to_ws_tx.lock().is_some(),
                    "eager mode must install agent_to_ws_tx immediately"
                );
                wait_for_connection(&count, "bare leader with no IPC clients").await;
            })
            .await;
        cancel.cancel();
    }
    /// With `relay_on_demand == true` (leader auto-spawned by an interactive client), the relay must stay off.
    /// It connects only when the first headless registration flips the demand watch.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn on_demand_relay_waits_for_headless_demand_signal() {
        let (addr, count) = spawn_mock_relay_server().await;
        let config = test_relay_config(addr);
        let cancel = CancellationToken::new();
        let (ws_to_agent_tx, _ws_to_agent_rx) = mpsc::unbounded_channel();
        let agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>> =
            Rc::new(Mutex::new(None));
        let (demand_tx, demand_rx) = watch::channel(false);
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let slot = Rc::new(std::cell::RefCell::new(None));
                spawn_leader_relay(
                    slot.clone(),
                    config,
                    true,
                    demand_rx,
                    ws_to_agent_tx,
                    agent_to_ws_tx.clone(),
                    cancel.clone(),
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    0,
                    "on-demand relay must not connect before a headless client registers"
                );
                assert!(agent_to_ws_tx.lock().is_none());
                demand_tx.send(true).unwrap();
                wait_for_connection(&count, "after headless demand signal").await;
            })
            .await;
        cancel.cancel();
    }
    /// Regression test for the "leader booted without auth is invisible forever" bug. A leader that starts with no session (e.g. a devbox whose initial mint hit a provider outage) must still arm the relay.
    /// The arm fires when a relay-eligible token is later hot-reloaded. For a non-eligible token it must hand the parts back (not consume them), so a later eligible one can still arm.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn deferred_arm_connects_relay_when_auth_appears() {
        let (addr, count) = spawn_mock_relay_server().await;
        let cancel = CancellationToken::new();
        let (ws_to_agent_tx, _ws_to_agent_rx) = mpsc::unbounded_channel();
        let agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>> =
            Rc::new(Mutex::new(None));
        let (_demand_tx, demand_rx) = watch::channel(false);
        let slot = Rc::new(std::cell::RefCell::new(None));
        let grok_com_config = xai_grok_login::GrokComConfig {
            grok_ws_url: format!("ws://{addr}"),
            grok_ws_origin: format!("http://{addr}"),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let auth_manager = Arc::new(AuthManager::new(tmp.path(), grok_com_config.clone()));
        let arm = DeferredRelayArm {
            relay_on_demand: false,
            relay_demand_rx: demand_rx,
            ws_to_agent_tx,
            agent_to_ws_tx: agent_to_ws_tx.clone(),
            cancel: cancel.clone(),
            slot: slot.clone(),
            grok_com_config,
            alpha_test_key: None,
        };
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let ineligible = GrokAuth::test_default();
                let arm = arm
                    .arm_if_eligible(&ineligible, &auth_manager)
                    .expect("non-eligible token must hand the parts back");
                assert!(slot.borrow().is_none(), "no handle parked yet");
                assert_eq!(
                    count.load(Ordering::SeqCst),
                    0,
                    "non-eligible token must not connect the relay"
                );
                let eligible = GrokAuth {
                    auth_mode: AuthMode::Oidc,
                    oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
                    ..GrokAuth::test_default()
                };
                assert!(
                    arm.arm_if_eligible(&eligible, &auth_manager).is_none(),
                    "eligible token must consume the arm parts"
                );
                assert!(
                    slot.borrow().is_some(),
                    "handle must be parked in the shared shutdown slot"
                );
                assert!(
                    agent_to_ws_tx.lock().is_some(),
                    "outbound relay sender must be installed"
                );
                wait_for_connection(&count, "deferred arm after auth hot-reload").await;
            })
            .await;
        cancel.cancel();
    }
    /// End-to-end for the merge reconciliation: a background cold-mint persists a relay-eligible session to auth.json.
    /// The config watcher emits `ConfigUpdate::Auth`, and that arms the deferred relay.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn cold_mint_auth_write_arms_deferred_relay() {
        use crate::config::reloader::{ConfigReloader, ConfigUpdate, hash_auth_key};
        let (addr, _count) = spawn_mock_relay_server().await;
        let grok_com_config = xai_grok_login::GrokComConfig {
            grok_ws_url: format!("ws://{addr}"),
            grok_ws_origin: format!("http://{addr}"),
            ..Default::default()
        };
        let tmp = tempfile::tempdir().unwrap();
        let scope = "https://test.example.com".to_string();
        let session = GrokAuth {
            auth_mode: AuthMode::Oidc,
            oidc_issuer: Some(xai_grok_login::XAI_OAUTH2_ISSUER.to_string()),
            ..GrokAuth::test_default()
        };
        let mut store = std::collections::BTreeMap::new();
        store.insert(scope.clone(), session);
        std::fs::write(
            tmp.path().join("auth.json"),
            serde_json::to_string_pretty(&store).unwrap(),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut reloader = ConfigReloader::new(
            tmp.path().to_path_buf(),
            hash_auth_key("sessionless-boot"),
            toml::Value::Table(Default::default()),
            scope,
            None,
            tx,
            None,
        );
        reloader.reload_auth().unwrap();
        let ConfigUpdate::Auth(minted) = rx
            .try_recv()
            .expect("cold-mint auth.json write must emit ConfigUpdate::Auth")
        else {
            panic!("expected ConfigUpdate::Auth");
        };
        let auth_manager = Arc::new(AuthManager::new(tmp.path(), grok_com_config.clone()));
        let (ws_to_agent_tx, _ws_to_agent_rx) = mpsc::unbounded_channel();
        let agent_to_ws_tx: Rc<Mutex<Option<mpsc::UnboundedSender<String>>>> =
            Rc::new(Mutex::new(None));
        let agent_to_ws_tx_probe = agent_to_ws_tx.clone();
        let (_demand_tx, demand_rx) = watch::channel(false);
        let slot = Rc::new(std::cell::RefCell::new(None));
        let cancel = CancellationToken::new();
        let arm = DeferredRelayArm {
            relay_on_demand: false,
            relay_demand_rx: demand_rx,
            ws_to_agent_tx,
            agent_to_ws_tx,
            cancel: cancel.clone(),
            slot: slot.clone(),
            grok_com_config,
            alpha_test_key: None,
        };
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                assert!(
                    arm.arm_if_eligible(&minted, &auth_manager).is_none(),
                    "a cold-minted relay-eligible session must arm the relay"
                );
                assert!(slot.borrow().is_some(), "relay handle must be parked");
                assert!(
                    agent_to_ws_tx_probe.lock().is_some(),
                    "outbound relay sender must be installed"
                );
            })
            .await;
        cancel.cancel();
    }
    #[test]
    fn internal_reload_request_line_carries_id_params_and_newline() {
        let line = internal_reload_request_line(
            "config-reload-models",
            InternalMethod::ReloadModels,
            serde_json::json!({}),
        );
        assert!(line.ends_with('\n'), "must be a newline-terminated line");
        let msg: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(
            msg.get("method").and_then(|v| v.as_str()),
            Some("_x.ai/internal/reload_models"),
            "wire method must carry the `_` ext prefix or the ACP decoder \
             rejects it with method_not_found"
        );
        assert_eq!(
            msg.get("id"),
            Some(&serde_json::json!("config-reload-models"))
        );
        assert_eq!(msg.get("jsonrpc"), Some(&serde_json::json!("2.0")));
        let line = internal_reload_request_line(
            "config-reload-project-mcp",
            InternalMethod::ReloadProjectMcpServers,
            serde_json::json!({ "cwd": "/repo/x" }),
        );
        let msg: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(
            msg.get("params")
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str()),
            Some("/repo/x")
        );
        let line = internal_reload_request_line(
            "config-auth-cleared",
            InternalMethod::AuthCleared,
            serde_json::json!({}),
        );
        let msg: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(
            msg.get("method").and_then(|v| v.as_str()),
            Some("_x.ai/internal/auth_cleared")
        );
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_cancels_when_update_available_and_agent_idle() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let config = always_config(true);
        tokio::time::timeout(
            Duration::from_secs(2),
            run_auto_update_checker(
                config,
                agent_busy,
                crate::agent::activity::AgentActivity::default(),
                cancel.clone(),
                dummy_shutdown_tx(),
            ),
        )
        .await
        .expect("checker should complete within timeout");
        assert!(cancel.is_cancelled(), "cancel token should be triggered");
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_defers_when_agent_busy() {
        let agent_busy = Arc::new(AtomicBool::new(true));
        let cancel = CancellationToken::new();
        let config = delayed_update_config(0);
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !cancel_clone.is_cancelled(),
            "cancel token should NOT be triggered when agent is busy"
        );
        cancel_clone.cancel();
        let _ = checker.await;
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_no_cancel_when_no_update_available() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let config = always_config(false);
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !cancel_clone.is_cancelled(),
            "cancel token should NOT be triggered when no update is available"
        );
        cancel_clone.cancel();
        let _ = checker.await;
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_cancels_after_agent_becomes_idle() {
        let agent_busy = Arc::new(AtomicBool::new(true));
        let cancel = CancellationToken::new();
        let config = always_config(true);
        let agent_busy_clone = agent_busy.clone();
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !cancel_clone.is_cancelled(),
            "should not cancel while agent is busy"
        );
        agent_busy_clone.store(false, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), checker)
            .await
            .expect("checker should complete within timeout")
            .expect("checker task should not panic");
        assert!(
            cancel_clone.is_cancelled(),
            "cancel token should be triggered after agent becomes idle"
        );
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_stops_when_externally_cancelled() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let config = always_config(false);
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        cancel_clone.cancel();
        tokio::time::timeout(Duration::from_secs(2), checker)
            .await
            .expect("checker should exit within timeout after external cancel")
            .expect("checker task should not panic");
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_calls_check_fn_multiple_times() {
        let call_count = Arc::new(AtomicU32::new(0));
        let call_count_clone = call_count.clone();
        let agent_busy = Arc::new(AtomicBool::new(true));
        let cancel = CancellationToken::new();
        let config = LeaderAutoUpdateConfig {
            check_interval: Duration::from_millis(10),
            check_fn: Box::new(move || {
                let cc = call_count_clone.clone();
                Box::pin(async move {
                    cc.fetch_add(1, Ordering::Relaxed);
                    true
                })
            }),
        };
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let calls = call_count.load(Ordering::Relaxed);
        assert!(
            calls >= 2,
            "check_fn should have been called multiple times, got {}",
            calls
        );
        cancel_clone.cancel();
        let _ = checker.await;
    }
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_cancels_during_hanging_check_fn() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let config = LeaderAutoUpdateConfig {
            check_interval: Duration::from_millis(10),
            check_fn: Box::new(|| Box::pin(async { futures::future::pending::<bool>().await })),
        };
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            crate::agent::activity::AgentActivity::default(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_clone.cancel();
        tokio::time::timeout(Duration::from_secs(2), checker)
            .await
            .expect("checker should exit within timeout even with hanging check_fn")
            .expect("checker task should not panic");
    }
    /// The IPC `agent_busy` flag never sees relay-driven traffic.
    /// The checker must also defer on the agent-derived activity signal (running turn, pending interaction, or live subagent).
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_defers_when_agent_activity_busy() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let activity = crate::agent::activity::AgentActivity::default();
        activity.subagent_gauge().store(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let config = always_config(true);
        let cancel_clone = cancel.clone();
        let checker = tokio::spawn(run_auto_update_checker(
            config,
            agent_busy,
            activity.clone(),
            cancel.clone(),
            dummy_shutdown_tx(),
        ));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            !cancel_clone.is_cancelled(),
            "must not shut down while the agent (not IPC) is busy"
        );
        activity.subagent_gauge().store(0, Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(2), checker)
            .await
            .expect("checker should complete within timeout")
            .expect("checker task should not panic");
        assert!(cancel_clone.is_cancelled());
    }
    /// A permanently-busy signal must not pin the leader to an old binary forever: after MAX_AUTO_UPDATE_BUSY_DEFERRALS the update proceeds.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_forces_shutdown_after_deferral_limit() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let activity = crate::agent::activity::AgentActivity::default();
        activity.subagent_gauge().store(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let config = always_config(true);
        tokio::time::timeout(
            Duration::from_secs(10),
            run_auto_update_checker(
                config,
                agent_busy,
                activity,
                cancel.clone(),
                dummy_shutdown_tx(),
            ),
        )
        .await
        .expect("checker should force shutdown after the deferral limit");
        assert!(cancel.is_cancelled());
    }
    /// Cancelling drops the LocalSet and aborts session actors.
    /// Before that, the checker must ask every registered session actor to shut down and wait for it to exit, so buffered state reaches disk.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_flushes_sessions_before_cancel() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let activity = crate::agent::activity::AgentActivity::default();
        let (mut cmd_rx, _prompt_id, _pending) = activity.register_for_test("s1");
        let cancel = CancellationToken::new();
        let got_shutdown = Arc::new(AtomicBool::new(false));
        let got_shutdown_clone = got_shutdown.clone();
        let cancel_for_actor = cancel.clone();
        let actor = tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if matches!(cmd, crate::session::SessionCommand::Shutdown(_)) {
                    assert!(
                        !cancel_for_actor.is_cancelled(),
                        "session flush must happen BEFORE the leader is cancelled"
                    );
                    got_shutdown_clone.store(true, Ordering::Relaxed);
                    return;
                }
            }
        });
        let config = always_config(true);
        tokio::time::timeout(
            Duration::from_secs(2),
            run_auto_update_checker(
                config,
                agent_busy,
                activity,
                cancel.clone(),
                dummy_shutdown_tx(),
            ),
        )
        .await
        .expect("checker should complete within timeout");
        assert!(cancel.is_cancelled());
        actor.await.expect("actor should exit cleanly");
        assert!(
            got_shutdown.load(Ordering::Relaxed),
            "session actor must receive SessionCommand::Shutdown before leader cancel"
        );
    }
    /// When an update is installed and the agent is idle, the checker sends `ShutdownReason::AutoUpdate` via `shutdown_tx` BEFORE cancelling.
    /// The IPC server then broadcasts the correct reason.
    #[tokio::test]
    #[tracing::instrument(level = "debug", skip_all)]
    async fn auto_update_sets_shutdown_reason_auto_update() {
        let agent_busy = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(crate::leader::ShutdownReason::Manual);
        let config = always_config(true);
        tokio::time::timeout(
            Duration::from_secs(2),
            run_auto_update_checker(
                config,
                agent_busy,
                crate::agent::activity::AgentActivity::default(),
                cancel.clone(),
                shutdown_tx,
            ),
        )
        .await
        .expect("checker should complete within timeout");
        assert!(cancel.is_cancelled(), "cancel token should be triggered");
        shutdown_rx.mark_changed();
        assert_eq!(
            *shutdown_rx.borrow(),
            crate::leader::ShutdownReason::AutoUpdate,
            "shutdown reason must be AutoUpdate for an auto-update-triggered shutdown"
        );
    }
}
