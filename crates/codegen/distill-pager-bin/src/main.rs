// Modified for Distill by Samuel Fajreldines, 2026.
#![allow(
    unused_imports,
    unused_variables,
    unused_mut,
    unreachable_code,
    dead_code
)]
#![deny(clippy::indexing_slicing)]
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
#[cfg(all(feature = "jemalloc", feature = "release-dist", unix))]
mod jemalloc_malloc_conf {
    /// jemalloc looks up `extern const char *malloc_conf`, a thin pointer, not a Rust `&[u8]` fat pointer.
    #[repr(transparent)]
    struct MallocConfPtr(*const u8);
    unsafe impl Sync for MallocConfPtr {}
    static CONF: [u8; 63] = *b"prof:true,prof_active:false,lg_prof_sample:19,prof_final:false\0";
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[used]
    #[unsafe(export_name = "malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[used]
    #[unsafe(export_name = "_rjem_malloc_conf")]
    static MALLOC_CONF: MallocConfPtr = MallocConfPtr(CONF.as_ptr());
}
use anyhow::Result;
use distill_pager::agent_runtime::AgentRuntime;
use distill_pager::app::{
    AgentCmd, Command, EARLY_PREFETCH_WAIT, HeadlessArgs, LeaderMgmtArgs, LeaderMgmtCommand,
    LeaderMode, LeaderTargetArgs, PagerArgs, resolve_use_leader, warn_leader_disabled_by_sandbox,
};
use distill_pager::app::{WorkspaceMgmtArgs, WorkspaceMgmtCommand, WorkspaceStartArgs};
use distill_pager::client_identity::PAGER_CLIENT_VERSION;
use distill_shell::agent::app::{run_headless, run_leader};
use distill_shell::agent::config::Config as AgentConfig;
use distill_shell::leader::{
    ClientCapabilities, ClientMode, ControlCommand, LeaderCapabilities, LeaderDescriptor,
    LeaderRegistration, LeaderTarget, leader_is_older_than,
};
use distill_shell::leader::{
    ControlPayload, LeaderClient, LeaderEnvUrls, connect_or_spawn, socket_path_for_ws_url,
};
use distill_telemetry::process_info::{
    Entrypoint, Interactivity, ProcessIdentity, ReleaseChannel, set_identity, set_release_channel,
};
use std::io::Write;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use tokio_util::sync::CancellationToken;
mod agent_command;
fn process_identity(command: Option<&Command>, is_interactive: bool) -> Option<ProcessIdentity> {
    use distill_telemetry::process_info::LeaderMode::Standalone;
    let (entrypoint, interactivity) = match command {
        Some(Command::Agent(_)) => return None,
        Some(Command::Dashboard) => return None,
        Some(Command::Login { .. }) => (Entrypoint::Cli, Interactivity::Interactive),
        Some(
            Command::Inspect { .. }
            | Command::Doctor(_)
            | Command::Leader(_)
            | Command::Logout { .. }
            | Command::Mcp(_)
            | Command::Plugin(_)
            | Command::Memory(_)
            | Command::Models
            | Command::Sessions(_)
            | Command::Usage(_)
            | Command::Setup { .. }
            | Command::Share(_)
            | Command::Wrap(_)
            | Command::Export(_)
            | Command::Trace(_)
            | Command::Update { .. }
            | Command::Version { .. }
            | Command::Completions { .. }
            | Command::Worktree(_)
            | Command::DiskUsage(_)
            | Command::Workspace(_),
        ) => (Entrypoint::Cli, Interactivity::Unattended),
        None if is_interactive => return None,
        None => (Entrypoint::Headless, Interactivity::Unattended),
    };
    Some(ProcessIdentity {
        entrypoint,
        leader: Standalone,
        interactivity,
    })
}
/// True when this command later boots an agent (`spawn_grok_shell` / agent subcommand) that heals managed policy after `apply_sandbox`.
fn command_needs_pre_sandbox_policy_heal(command: Option<&Command>) -> bool {
    match command {
        None
        | Some(Command::Agent(_))
        | Some(Command::Dashboard)
        | Some(Command::Models)
        | Some(Command::Worktree(_)) => true,
        Some(
            Command::Inspect { .. }
            | Command::Doctor(_)
            | Command::Leader(_)
            | Command::Logout { .. }
            | Command::Login { .. }
            | Command::Mcp(_)
            | Command::Plugin(_)
            | Command::Memory(_)
            | Command::Sessions(_)
            | Command::Usage(_)
            | Command::Setup { .. }
            | Command::Share(_)
            | Command::Wrap(_)
            | Command::Export(_)
            | Command::Trace(_)
            | Command::Update { .. }
            | Command::Version { .. }
            | Command::Completions { .. }
            | Command::DiskUsage(_)
            | Command::Workspace(_),
        ) => false,
    }
}
use distill_update::{UpdateConfig, auto_update, enforce_version_policy_or_exit};
use std::env;
/// Apply headless args to an existing config, only overriding values that are explicitly set.
/// Unset args leave the environment defaults in place.
fn apply_headless_args_to_config(args: &HeadlessArgs, config: &mut AgentConfig) {
    if let Some(v) = &args.grok_ws_origin {
        config.grok_com_config.grok_ws_origin = v.clone();
    }
    if let Some(v) = &args.grok_ws_url {
        config.grok_com_config.grok_ws_url = v.clone();
    }
}
/// Apply global endpoint CLI args to an existing config.
fn apply_agent_endpoint_args(agent_args: &distill_pager::app::AgentArgs, config: &mut AgentConfig) {
    if let Some(v) = &agent_args.cli_chat_proxy_base_url {
        config.endpoints.cli_chat_proxy_base_url = Some(v.clone());
    }
    if let Some(v) = &agent_args.xai_api_base_url {
        config.endpoints.xai_api_base_url = v.clone();
    }
}
/// Resolve --agent-profile path: canonicalize and verify the file exists.
fn resolve_agent_profile_path(path: &std::path::Path) -> std::path::PathBuf {
    match dunce::canonicalize(path) {
        Ok(abs) if abs.is_file() => abs,
        Ok(abs) => {
            eprintln!(
                "error: --agent-profile path is not a file: {}",
                abs.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: --agent-profile path '{}': {}", path.display(), e);
            std::process::exit(1);
        }
    }
}
/// Print startup information for the serve command.
fn print_serve_startup_info(bind_addr: SocketAddr, secret: &str) {
    eprintln!();
    eprintln!("   Distill agent server starting...");
    eprintln!();
    eprintln!("   Address:  {}:{}", bind_addr.ip(), bind_addr.port());
    eprintln!("   Secret:   {}", secret);
    eprintln!();
    eprintln!(
        "   WebSocket URL: ws://{}/ws?server-key={}",
        bind_addr, secret
    );
    eprintln!();
}
/// Entrypoint tag for `distill -p`; keys the quiet stderr default in `init_tracing_simple`.
const HEADLESS_ENTRYPOINT: &str = "headless";
/// Initialize simple tracing for non-TUI agent modes.
fn init_tracing_simple(app_entrypoint: &'static str) {
    use distill_telemetry::debug_log::RMCP_SSE_NOISE_TARGET;
    use tracing_subscriber::{EnvFilter, Layer as _, fmt, layer::SubscriberExt as _};
    let default_filter = if app_entrypoint == HEADLESS_ENTRYPOINT {
        "off"
    } else {
        "error"
    };
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter.add_directive(
            format!("{RMCP_SSE_NOISE_TARGET}=error")
                .parse()
                .expect("static rmcp directive must parse"),
        ),
        Err(_) => EnvFilter::new(default_filter),
    };
    let fmt_layer = fmt::layer()
        .with_target(false)
        .with_ansi(true)
        .with_writer(std::io::stderr);
    let registry = tracing_subscriber::registry()
        .with(fmt_layer.with_filter(env_filter))
        .with(distill_telemetry::sampling_log::layer())
        .with(distill_telemetry::jev_log::layer())
        .with(distill_telemetry::span_profile::layer(app_entrypoint))
        .with(distill_telemetry::instrumentation::layer())
        .with(distill_telemetry::hooks_log::layer())
        .with(distill_telemetry::otel_layer::build_otel_layer(
            distill_telemetry::otel_layer::OtelClientInfo {
                client_name: "distill",
                client_version: distill_version::VERSION,
                service_version: env!("VERSION_WITH_COMMIT"),
                app_entrypoint,
            },
            distill_shell::agent::init::build_default_otel_layer_config(),
        ));
    distill_telemetry::debug_log::install_firehose(registry, app_entrypoint);
    distill_telemetry::external::init(distill_shell::agent::config::resolve_external_otel_config(
        distill_telemetry::external::config::ExternalClientInfo {
            service_version: env!("VERSION_WITH_COMMIT").to_owned(),
            client_version: distill_version::VERSION.to_owned(),
            app_entrypoint: app_entrypoint.to_owned(),
        },
    ));
}
/// `distill setup`: rendering and exit codes only; fetch logic lives in `distill_shell::managed_config`.
/// `json` prints the served configuration instead of installing it.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_setup_command(json: bool) {
    use distill_shell::managed_config::{self, SetupOutcome};
    if !managed_config::has_principal() {
        eprintln!("No deployment key or team sign-in found.");
        eprintln!();
        eprintln!("To install managed configuration, sign in with a team using `distill login`,");
        eprintln!("or set a deployment key:");
        eprintln!();
        if cfg!(unix) {
            eprintln!("  export GROK_DEPLOYMENT_KEY=<your-key>");
        } else {
            eprintln!("  $env:GROK_DEPLOYMENT_KEY=\"<your-key>\"");
        }
        eprintln!("  distill setup");
        eprintln!();
        eprintln!("Or add the key to ~/.grok/config.toml:");
        eprintln!();
        eprintln!("  [endpoints]");
        eprintln!("  deployment_key = \"<your-key>\"");
        eprintln!();
        eprintln!(
            "If you don't have a deployment key, contact your organization's Grok administrator."
        );
        std::process::exit(1);
    }
    if json {
        match managed_config::fetch_setup_report().await {
            Ok(report) => {
                let out = serde_json::to_string_pretty(&report)
                    .expect("setup report has no non-serializable values");
                println!("{out}");
                if !report.configured {
                    eprintln!(
                        "Your team doesn't have a managed configuration yet. A team admin can set one up at console.x.ai."
                    );
                }
            }
            Err(e) => {
                eprintln!("Couldn't fetch managed configuration. {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    match managed_config::run_setup().await {
        SetupOutcome::Installed => eprintln!("Applied managed configuration."),
        SetupOutcome::NothingConfigured => {
            eprintln!(
                "Your team doesn't have a managed configuration yet. A team admin can set one up at console.x.ai."
            );
        }
        SetupOutcome::Skipped => {
            eprintln!(
                "Managed configuration was not applied this run (another process held the apply lock, or the credential changed during the fetch). Run `distill setup` again."
            );
        }
        SetupOutcome::Staged => {
            eprintln!(
                "Managed configuration update verified; it takes effect the next time Distill starts."
            );
        }
        SetupOutcome::Failed(e) => {
            eprintln!("Couldn't apply managed configuration. {e}");
            std::process::exit(1);
        }
    }
}
#[tracing::instrument(level = "debug", skip_all)]
async fn run_leader_mgmt(args: LeaderMgmtArgs) -> Result<()> {
    match args.command {
        LeaderMgmtCommand::Kill => kill_leaders().await,
        LeaderMgmtCommand::List { json } => {
            let leaders = distill_shell::leader::discover_leaders().await;
            if json {
                let payload: Vec<_> = leaders.iter().map(leader_descriptor_json).collect();
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::Value::Array(payload))?
                );
            } else if leaders.is_empty() {
                println!("No leader candidates found.");
            } else {
                for d in &leaders {
                    print_leader_descriptor(d);
                }
            }
            Ok(())
        }
        LeaderMgmtCommand::Info { target, json } => {
            let (descriptor, client) = connect_to_leader(&target).await?;
            let info = match ensure_control_caps(client.registration()) {
                Ok(_) => client
                    .send_control(ControlCommand::GetLeaderInfo)
                    .await
                    .ok()
                    .and_then(|r| r.ok()),
                Err(_) => None,
            };
            if json {
                let payload = leader_info_json(&descriptor, client.registration(), info.as_ref())?;
                println!("{}", serde_json::to_string(&payload)?);
            } else if let Some(info) = info {
                println!("{info:#?}");
            } else {
                print_leader_descriptor(&descriptor);
                eprintln!(
                    "  (detailed info unavailable — leader does not advertise control capabilities)"
                );
            }
            client.cancel();
            Ok(())
        }
    }
}
#[tracing::instrument(level = "debug", skip_all)]
async fn kill_leaders() -> Result<()> {
    let leaders = distill_shell::leader::discover_leaders().await;
    if leaders.is_empty() {
        eprintln!("No leader candidates found.");
        return Ok(());
    }
    let mut killed = 0u32;
    let mut cleaned = 0u32;
    for d in &leaders {
        let Some(pid) = leader_pid(d) else {
            continue;
        };
        if !distill_shell::util::is_grok_process(pid) {
            if let Some(ref lock) = d.lock_path {
                eprintln!("  PID {pid} is not a grok process, removing stale lock");
                let _ = std::fs::remove_file(lock);
                cleaned += 1;
            }
            if let Some(ref sock) = d.socket_path {
                let _ = std::fs::remove_file(sock);
            }
            continue;
        }
        eprintln!("  Killing leader PID {pid}");
        if let Err(e) = distill_shell::util::kill_process_by_pid(pid) {
            eprintln!("  warning: failed to terminate PID {pid}: {e}");
            continue;
        }
        killed += 1;
    }
    if killed > 0 {
        eprintln!("Killed {killed} leader process(es).");
    } else if cleaned > 0 {
        eprintln!("No live leader processes found (cleaned up {cleaned} stale lock(s)).");
    } else {
        eprintln!("No live leader processes found.");
    }
    Ok(())
}
fn resolve_target(args: &LeaderTargetArgs) -> LeaderTarget {
    match args.pid {
        Some(pid) => LeaderTarget::Pid(pid),
        None => LeaderTarget::Environment(distill_shell::env::DistillEnvironment::Production),
    }
}
#[tracing::instrument(skip_all)]
async fn connect_to_leader(
    args: &LeaderTargetArgs,
) -> Result<(LeaderDescriptor, distill_shell::leader::LeaderClient)> {
    let target = resolve_target(args);
    let selection = distill_shell::leader::resolve_leader_target(target)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let socket_path = selection
        .socket_path()
        .ok_or_else(|| anyhow::anyhow!("resolved leader target did not include a socket path"))?;
    let client = distill_shell::leader::LeaderClient::connect(
        socket_path.to_path_buf(),
        "distill-leader-cli",
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await?;
    Ok((selection.descriptor, client))
}
/// Prefer the PID verified live over the socket; the lock file's PID may have been recycled.
fn leader_pid(d: &LeaderDescriptor) -> Option<u32> {
    d.live_info.as_ref().map(|li| li.pid).or(d.pid_from_lock)
}
fn print_leader_descriptor(d: &LeaderDescriptor) {
    let pid = leader_pid(d)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".into());
    let sock = d
        .socket_path
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "?".into());
    let state = format!("{:?}", d.classification);
    eprintln!("  PID {pid} ({state}) -- {sock}");
}
fn leader_descriptor_json(d: &LeaderDescriptor) -> serde_json::Value {
    serde_json::json!({
        "pid": leader_pid(d),
        "pidFromLock": d.pid_from_lock,
        "pidLive": d.live_info.as_ref().map(|li| li.pid),
        "classification": format!("{:?}", d.classification),
        "socketPath": d.socket_path.as_deref().map(|p| p.display().to_string()),
        "lockPath": d.lock_path.as_deref().map(|p| p.display().to_string()),
        "wsUrlSuffix": d.ws_url_suffix,
    })
}
fn leader_info_json(
    d: &LeaderDescriptor,
    reg: &LeaderRegistration,
    info: Option<&distill_shell::leader::ControlPayload>,
) -> Result<serde_json::Value> {
    let mut val = leader_descriptor_json(d);
    if let Some(obj) = val.as_object_mut() {
        obj.insert("clientId".to_owned(), serde_json::json!(reg.client_id));
        if let Some(info) = info {
            obj.insert("info".to_owned(), serde_json::to_value(info)?);
        }
    }
    Ok(val)
}
fn ensure_control_caps(reg: &LeaderRegistration) -> Result<&LeaderCapabilities> {
    reg.leader_capabilities
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Leader does not advertise capabilities (legacy version)"))
}
/// Env override for the `distill workspace` gate: any truthy value enables the command locally, a falsy one disables it.
/// Either way it bypasses the remote settings flag.
const WORKSPACE_COMMAND_ENV: &str = "GROK_WORKSPACE_COMMAND";
/// One leader door's CLI identity, shared by `connect_leader_control` and `spawn_and_connect_leader`.
struct LeaderDoorCli {
    /// The command name as the user types it (`distill workspace`); `<name> start` is its start command.
    name: &'static str,
    /// IPC client type the leader records for connections from this command.
    client_type: &'static str,
    /// Why the door needs leader mode, for the "requires leader mode" refusal.
    leader_mode_reason: &'static str,
}
const WORKSPACE_DOOR: LeaderDoorCli = LeaderDoorCli {
    name: "distill workspace",
    client_type: "grok-workspace-cli",
    leader_mode_reason: "the workspace is shared via the leader",
};
/// Resolution of the `distill workspace` gate.
/// `Unknown` is kept separate from `Disabled` so we don't tell the user the flag is off when the settings were never read.
/// Both fail closed, but `Unknown` earns an honest message.
#[derive(Debug, PartialEq, Eq)]
enum WorkspaceGate {
    Enabled,
    Disabled,
    Unknown,
}
/// The `GROK_WORKSPACE_COMMAND` override, if set (`Some(true)`/`Some(false)`); `None` defers to the remote settings flag.
fn workspace_command_env_override() -> Option<bool> {
    std::env::var(WORKSPACE_COMMAND_ENV)
        .ok()
        .map(|v| env_flag_enabled(&v))
}
/// Resolve the gate.
/// Precedence: the env override, then remote `Some(true)`, then loaded-but-off (`Disabled`), then settings-not-loaded (`Unknown`).
fn workspace_command_gate(
    env_override: Option<bool>,
    remote_settings: Option<&distill_shell::util::config::RemoteSettings>,
) -> WorkspaceGate {
    if let Some(enabled) = env_override {
        return if enabled {
            WorkspaceGate::Enabled
        } else {
            WorkspaceGate::Disabled
        };
    }
    match remote_settings {
        Some(rs) if rs.workspace_command_enabled.unwrap_or(false) => WorkspaceGate::Enabled,
        Some(_) => WorkspaceGate::Disabled,
        None => WorkspaceGate::Unknown,
    }
}
/// Truthy parse for grok on/off env vars: everything enables except the common falsy spellings (`0`, `false`, `off`, `no`, empty).
fn env_flag_enabled(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "off" | "no"
    )
}
/// File and managed IdP for a settings query that runs before effective-config load.
fn load_grok_com_config_for_settings() -> distill_shell::auth::GrokComConfig {
    distill_shell::config::load_agent_config_disk_only()
        .map(|cfg| cfg.grok_com_config)
        .unwrap_or_default()
}
/// Async fetch of remote settings via the startup getter, capped at the
/// early-prefetch wait so a slow endpoint cannot stall the CLI. Awaits the async
/// getter directly: this runs under a current-thread runtime where the sync
/// `block_on_startup_settings` returns `TimedOut` immediately with no published
/// value, so it would fall open and let workspace refuse the command before the
/// load could complete.
async fn fetch_remote_settings(
    grok_com_config: &distill_shell::auth::GrokComConfig,
) -> Option<distill_shell::util::config::RemoteSettings> {
    let query = distill_shell::agent::remote_config::settings_get::SettingsQuery::resolve(
        None,
        Some(grok_com_config.clone()),
    );
    let wait = distill_shell::agent::remote_config::settings_get::await_startup_settings(
        query,
        EARLY_PREFETCH_WAIT,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await;
    distill_shell::agent::remote_config::settings_get::consume_wait(wait, None, grok_com_config)
}
#[tracing::instrument(level = "debug", skip_all)]
async fn run_workspace_mgmt(args: WorkspaceMgmtArgs) -> Result<()> {
    if matches!(
        &args.command,
        WorkspaceMgmtCommand::Start(_)
            | WorkspaceMgmtCommand::Restart(_)
            | WorkspaceMgmtCommand::Resume { .. }
    ) && let Some(profile) = distill_sandbox::requested_confinement_profile()
    {
        anyhow::bail!(
            "`distill workspace` start/restart/resume is unavailable under sandbox profile '{profile}': \
             those commands (re)activate shared-leader workspace exposure that this session cannot \
             prove is confined by that profile. Disable the profile at the source that selected it \
             (CLI, env, config, or a managed requirement)."
        );
    }
    let env_override = workspace_command_env_override();
    let grok_com_config = load_grok_com_config_for_settings();
    let remote_settings = if env_override.is_none() {
        fetch_remote_settings(&grok_com_config).await
    } else {
        None
    };
    match workspace_command_gate(env_override, remote_settings.as_ref()) {
        WorkspaceGate::Enabled => {}
        WorkspaceGate::Disabled => {
            anyhow::bail!(
                "`distill workspace` is not enabled for this account \
             (gated by a server-side feature flag that is currently off)."
            )
        }
        WorkspaceGate::Unknown => {
            anyhow::bail!(
                "Could not load your settings for `distill workspace`. Check your \
             network connection (run `distill login` if you are signed out), then \
             try again."
            )
        }
    }
    match args.command {
        WorkspaceMgmtCommand::Start(a) => {
            let settings = match remote_settings {
                Some(settings) => Some(settings),
                None => fetch_remote_settings(&grok_com_config).await,
            };
            workspace_start(a, false, settings).await
        }
        WorkspaceMgmtCommand::Restart(a) => {
            let settings = match remote_settings {
                Some(settings) => Some(settings),
                None => fetch_remote_settings(&grok_com_config).await,
            };
            workspace_start(a, true, settings).await
        }
        WorkspaceMgmtCommand::Pause { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspacePause).await
        }
        WorkspaceMgmtCommand::Resume { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceResume).await
        }
        WorkspaceMgmtCommand::Stop { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceStop).await
        }
        WorkspaceMgmtCommand::Status { target, json } => {
            workspace_control(&target, json, ControlCommand::WorkspaceStatus).await
        }
    }
}
fn ensure_workspace_caps(reg: &LeaderRegistration) -> Result<()> {
    let caps = ensure_control_caps(reg)?;
    if !caps.workspace_exposure {
        anyhow::bail!(
            "the running leader does not support workspace exposure — stop the \
             leader process and re-run to pick up the new version"
        );
    }
    Ok(())
}
/// Control connection for a leader door CLI: by PID when given, else the socket of the configured
/// environment.
#[tracing::instrument(level = "debug", skip_all)]
async fn connect_leader_control(
    agent_config: &AgentConfig,
    target: &LeaderTargetArgs,
    door: &LeaderDoorCli,
) -> Result<LeaderClient> {
    if target.pid.is_some() {
        let (_descriptor, client) = connect_to_leader(target).await?;
        return Ok(client);
    }
    let ws_url = &agent_config.grok_com_config.grok_ws_url;
    let socket = socket_path_for_ws_url(ws_url);
    LeaderClient::connect(
        socket,
        door.client_type,
        ClientMode::Stdio,
        ClientCapabilities::default(),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!(
            "no running leader for this environment ({e}). \
             Start a grok session, or run `{} start`.",
            door.name
        )
    })
}
/// The `start` handshake shared by the door CLIs: effective config, the leader-mode decision,
/// cached credentials, `connect_or_spawn`, then a fresh control client on the leader socket.
#[tracing::instrument(level = "debug", skip_all)]
async fn spawn_and_connect_leader(
    leader: bool,
    no_leader: bool,
    remote_settings: Option<&distill_shell::util::config::RemoteSettings>,
    door: &LeaderDoorCli,
) -> Result<LeaderClient> {
    use distill_login::ensure_authenticated;
    distill_shell::util::config::set_remote_campaigns_from_settings(remote_settings);
    let raw_config = distill_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    let agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
    let (use_leader, _) = resolve_use_leader(
        leader,
        no_leader,
        &raw_config,
        remote_settings,
        true,
        distill_sandbox::requested_confinement_profile(),
    );
    if !use_leader {
        anyhow::bail!(
            "`{}` requires leader mode ({}).\n\
             Enable it with `[cli] use_leader = true` in ~/.grok/config.toml, or pass --leader.",
            door.name,
            door.leader_mode_reason
        );
    }
    ensure_authenticated(
        &agent_config.grok_com_config,
        agent_config.login_device_flow,
        agent_config.endpoints.proxy_url(),
        false,
        Some("No cached credentials found. Run `distill login` first."),
    )
    .await?;
    let env_urls = LeaderEnvUrls::from(&agent_config.grok_com_config);
    let capabilities = ClientCapabilities {
        client_version: Some(PAGER_CLIENT_VERSION.to_string()),
        ..Default::default()
    };
    let conn = connect_or_spawn(door.client_type, ClientMode::Stdio, &env_urls, capabilities)
        .await
        .map_err(|e| anyhow::anyhow!("failed to start or connect to leader: {e}"))?;
    drop(conn);
    connect_leader_control(&agent_config, &LeaderTargetArgs::default(), door).await
}
#[tracing::instrument(level = "debug", skip_all)]
async fn workspace_control(
    target: &LeaderTargetArgs,
    json: bool,
    command: ControlCommand,
) -> Result<()> {
    let agent_config = distill_shell::config::load_agent_config_disk_only()
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
    let client = connect_leader_control(&agent_config, target, &WORKSPACE_DOOR).await?;
    ensure_workspace_caps(client.registration())?;
    let payload = client.send_control(command).await??;
    render_workspace_payload(&payload, json);
    client.cancel();
    Ok(())
}
#[tracing::instrument(level = "debug", skip_all)]
async fn workspace_start(
    args: WorkspaceStartArgs,
    restart: bool,
    remote_settings: Option<distill_shell::util::config::RemoteSettings>,
) -> Result<()> {
    let client = spawn_and_connect_leader(
        args.leader,
        args.no_leader,
        remote_settings.as_ref(),
        &WORKSPACE_DOOR,
    )
    .await?;
    ensure_workspace_caps(client.registration())?;
    if restart {
        let _ = client.send_control(ControlCommand::WorkspaceStop).await;
    }
    let cwd = match args.cwd {
        Some(p) => p,
        None => std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("cannot determine current directory: {e}"))?,
    };
    let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
    let payload = client
        .send_control(ControlCommand::WorkspaceStart {
            hub_url: args.hub_url.clone(),
            cwd: cwd.display().to_string(),
        })
        .await??;
    render_workspace_payload(&payload, args.json);
    client.cancel();
    Ok(())
}
fn render_workspace_payload(payload: &ControlPayload, json: bool) {
    let ControlPayload::WorkspaceStatus {
        state,
        hub_url,
        cwd,
        uptime_ms,
        active_tool_calls,
        sessions,
        pid,
    } = payload
    else {
        eprintln!("unexpected control response: {payload:?}");
        return;
    };
    if json {
        let value = serde_json::json!({
            "state": state,
            "hubUrl": hub_url,
            "cwd": cwd,
            "uptimeMs": uptime_ms,
            "activeToolCalls": active_tool_calls,
            "sessions": sessions,
            "pid": pid,
        });
        println!("{}", serde_json::to_string(&value).unwrap_or_default());
        return;
    }
    if state == "none" {
        println!("Workspace exposure: not running (leader PID {pid})");
        return;
    }
    println!("Workspace exposure: {state}");
    if let Some(url) = hub_url {
        println!("  hub:      {url}");
    }
    if let Some(dir) = cwd {
        println!("  cwd:      {dir}");
    }
    println!("  uptime:   {}s", uptime_ms / 1000);
    println!("  active:   {active_tool_calls} tool call(s)");
    let session_list = if sessions.is_empty() {
        "-".to_string()
    } else {
        sessions.join(", ")
    };
    println!("  sessions: {} ({session_list})", sessions.len());
    println!("  leader:   PID {pid}");
}
/// How to rebuild one session's `session/load` after a leader reconnect.
#[derive(Default, Clone)]
struct CachedSession {
    /// Verbatim `session/load` request JSON (preferred replay form: preserves the client's exact cwd / mcpServers / meta).
    /// `None` when the session was only ever created via `session/new`; the load is synthesized.
    load_request_json: Option<String>,
    /// `cwd` captured from `session/new` / `session/load` params.
    cwd: Option<String>,
    /// `mcpServers` captured from `session/new` / `session/load` params.
    mcp_servers_json: Option<String>,
}
/// ACP state cached from the stdio stream for replay after leader reconnect.
/// Tracks EVERY session the external client has open (IDE clients drive multiple sessions over one bridge), not just the most recent one.
/// A leader crash must restore all of them or the others die with "unknown session id" on their next prompt.
#[derive(Default, Clone)]
struct StdioReplayState {
    initialize_json: Option<String>,
    /// Sessions to restore on reconnect, keyed by session id, in first-seen order (Vec keeps replay order deterministic).
    sessions: Vec<(String, CachedSession)>,
    /// cwd/mcp from the most recent `session/new` REQUEST whose response has not been observed yet.
    /// Folded into `sessions` when the response carrying the assigned session id arrives.
    /// Never replayed while unconfirmed (the id is unknown; the client's own request died with the old leader and is its to retry).
    pending_new: Option<CachedSession>,
    /// Most recently created/loaded session id, reported in `x.ai/leader_reconnected` as the primary restored session.
    last_session_id: Option<String>,
}
impl StdioReplayState {
    fn upsert_session(&mut self, sid: &str, cached: CachedSession) {
        if let Some((_, existing)) = self.sessions.iter_mut().find(|(id, _)| id == sid) {
            *existing = cached;
        } else {
            self.sessions.push((sid.to_string(), cached));
        }
    }
    fn remove_session(&mut self, sid: &str) {
        self.sessions.retain(|(id, _)| id != sid);
        if self.last_session_id.as_deref() == Some(sid) {
            self.last_session_id = None;
        }
    }
    /// A resume names a session the client is already attached to, so an entry from its original `session/load` is the better one to replay.
    /// That entry carries the client's `_meta`, which a synthesized load cannot reproduce.
    fn insert_session_if_new(&mut self, sid: &str, cached: CachedSession) {
        if !self.sessions.iter().any(|(id, _)| id == sid) {
            self.sessions.push((sid.to_string(), cached));
        }
    }
}
/// Read `sessionId`, `cwd`, and `mcpServers` off a session request's params.
fn cached_session_from_params(
    params: &serde_json::Value,
    verbatim: Option<&str>,
) -> Option<(String, CachedSession)> {
    let sid = params
        .get("sessionId")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())?;
    Some((
        sid.to_string(),
        CachedSession {
            load_request_json: verbatim.map(str::to_string),
            cwd: params
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mcp_servers_json: params
                .get("mcpServers")
                .and_then(|m| serde_json::to_string(m).ok()),
        },
    ))
}
/// Methods whose requests the replay cache reads.
/// One list shared by the prefilter and the match below so the two cannot drift apart.
/// Quoted JSON spellings so prose mentioning a method does not trigger a parse.
const CACHED_METHODS: &[&str] = &[
    "\"initialize\"",
    "\"session/new\"",
    "\"session/load\"",
    "\"session/resume\"",
    "\"session/close\"",
    "\"x.ai/session/close\"",
    "\"_x.ai/session/close\"",
];
fn cache_outgoing_acp_state(msg: &str, state: &std::sync::Mutex<StdioReplayState>) {
    if !CACHED_METHODS.iter().any(|m| msg.contains(m)) {
        return;
    }
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    let method = json
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or_default();
    match method {
        "initialize" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.initialize_json = Some(msg.to_string());
        }
        "session/load" => {
            let Some(params) = json.get("params") else {
                return;
            };
            let Some((sid, cached)) = cached_session_from_params(params, Some(msg)) else {
                return;
            };
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.upsert_session(&sid, cached);
            s.last_session_id = Some(sid);
        }
        "session/resume" => {
            let Some(params) = json.get("params") else {
                return;
            };
            let Some((sid, cached)) = cached_session_from_params(params, None) else {
                return;
            };
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.insert_session_if_new(&sid, cached);
            s.last_session_id = Some(sid);
        }
        "session/new" => {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            let params = json.get("params");
            s.pending_new = Some(CachedSession {
                load_request_json: None,
                cwd: params
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                mcp_servers_json: params
                    .and_then(|p| p.get("mcpServers"))
                    .and_then(|m| serde_json::to_string(m).ok()),
            });
        }
        "session/close" | "x.ai/session/close" | "_x.ai/session/close" => {
            if let Some(sid) = json
                .get("params")
                .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                .and_then(|v| v.as_str())
            {
                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.remove_session(sid);
            }
        }
        _ => {}
    }
}
fn cache_incoming_session_id(msg: &str, state: &std::sync::Mutex<StdioReplayState>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(msg) else {
        return;
    };
    if let Some(sid) = json
        .get("result")
        .and_then(|r| r.get("sessionId").or_else(|| r.get("session_id")))
        .and_then(|v| v.as_str())
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pending) = s.pending_new.take() {
            s.upsert_session(sid, pending);
        }
        s.last_session_id = Some(sid.to_string());
    }
}
/// Synthetic JSON-RPC id for the `session/load` the bridge constructs itself (when the external client only ever sent `session/new`).
/// A string id can never collide with a numeric id the external client may have in flight.
const REPLAY_LOAD_REQUEST_ID: &str = "x.ai/leader-replay/session-load";
/// Max silence between two messages from the leader during a replayed request.
/// A `session/load` streams replay notifications continuously once it starts.
/// The phase before the replay (MCP resolution, session file reads) can be quiet for a while on large sessions.
const REPLAY_RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Overall deadline for one replayed request's response.
const REPLAY_RESPONSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
/// Outcome of one replayed request (see [`replay_request_until_response`]).
enum ReplayOutcome {
    /// The response for the replayed request arrived with a `result`.
    ResponseOk,
    /// The response arrived but carried an `error` (e.g. session not found).
    ResponseErr,
    /// The connection closed / timed out / stdout broke before the response.
    Failed,
}
/// True when `msg` is the JSON-RPC *response* to the request with `expected_id` (no `method` key and a matching `id`).
fn parse_replay_response(msg: &str, expected_id: &serde_json::Value) -> Option<ReplayOutcome> {
    let json = serde_json::from_str::<serde_json::Value>(msg).ok()?;
    if json.get("method").is_some() {
        return None;
    }
    if json.get("id") != Some(expected_id) {
        return None;
    }
    if json.get("error").is_some() {
        Some(ReplayOutcome::ResponseErr)
    } else {
        Some(ReplayOutcome::ResponseOk)
    }
}
/// That is exactly what the stream would have carried before the reconnect. Only the response to the replayed
/// request is swallowed. Returning before the `session/load` response is the root cause of the "unknown session id"
/// failures after a leader crash.
#[tracing::instrument(level = "debug", skip_all)]
async fn replay_request_until_response(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    request_json: &str,
    what: &str,
) -> ReplayOutcome {
    use tokio::io::AsyncWriteExt as _;
    let expected_id = serde_json::from_str::<serde_json::Value>(request_json)
        .ok()
        .and_then(|v| v.get("id").cloned());
    if tx.send(request_json.to_string()).is_err() {
        tracing::warn!(what, "replay: failed to send request");
        return ReplayOutcome::Failed;
    }
    let Some(expected_id) = expected_id else {
        return ReplayOutcome::ResponseOk;
    };
    let deadline = tokio::time::Instant::now() + REPLAY_RESPONSE_DEADLINE;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(what, "replay: response deadline exceeded");
            return ReplayOutcome::Failed;
        }
        let per_recv = REPLAY_RECV_TIMEOUT.min(deadline - now);
        match tokio::time::timeout(per_recv, rx.recv()).await {
            Ok(Some(msg)) => {
                if let Some(outcome) = parse_replay_response(&msg, &expected_id) {
                    tracing::debug!(
                        what,
                        ok = matches!(outcome, ReplayOutcome::ResponseOk),
                        "replay: response received"
                    );
                    return outcome;
                }
                if stdout.write_all(msg.as_bytes()).await.is_err()
                    || stdout.write_all(b"\n").await.is_err()
                    || stdout.flush().await.is_err()
                {
                    tracing::warn!(what, "replay: stdout closed while forwarding");
                    return ReplayOutcome::Failed;
                }
            }
            Ok(None) => {
                tracing::warn!(what, "replay: leader closed before response");
                return ReplayOutcome::Failed;
            }
            Err(_) => {
                tracing::warn!(what, "replay: timed out waiting for response");
                return ReplayOutcome::Failed;
            }
        }
    }
}
/// Build the `session/load` JSON to replay for one cached session.
/// Prefer the verbatim client request when available, else synthesize a load from the captured `session/new` parameters.
fn replay_load_json(sid: &str, cached: &CachedSession) -> Option<String> {
    if let Some(ref verbatim) = cached.load_request_json {
        return Some(verbatim.clone());
    }
    let cwd = cached.cwd.as_deref()?;
    let mut params = serde_json::json!({
        "sessionId": sid,
        "cwd": cwd,
    });
    if let Some(mcp_raw) = &cached.mcp_servers_json
        && let Ok(mcp_val) = serde_json::from_str::<serde_json::Value>(mcp_raw)
        && let Some(obj) = params.as_object_mut()
    {
        obj.insert("mcpServers".to_owned(), mcp_val);
    }
    Some(
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": REPLAY_LOAD_REQUEST_ID,
            "method": "session/load",
            "params": params,
        })
        .to_string(),
    )
}
/// Replay the cached `initialize` and every cached `session/load` to a freshly (re-)elected leader. `None` when
/// there was nothing to replay or every restore failed.
#[tracing::instrument(skip_all)]
async fn replay_acp_state_after_reconnect(
    tx: &tokio::sync::mpsc::UnboundedSender<String>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    state: &StdioReplayState,
) -> Option<String> {
    if let Some(ref init_json) = state.initialize_json {
        match replay_request_until_response(tx, rx, stdout, init_json, "initialize").await {
            ReplayOutcome::ResponseOk => {}
            ReplayOutcome::ResponseErr => {
                tracing::warn!("replay: initialize was rejected by new leader");
                return None;
            }
            ReplayOutcome::Failed => return None,
        }
    } else {
        tracing::debug!("replay: no cached initialize; skipping ACP replay");
        return None;
    }
    if state.sessions.is_empty() {
        tracing::debug!("replay: no sessions to replay");
        return None;
    }
    let mut restored: Vec<String> = Vec::new();
    for (sid, cached) in &state.sessions {
        let Some(load_json) = replay_load_json(sid, cached) else {
            tracing::warn!(session_id = %sid, "replay: no way to rebuild session/load; skipping");
            continue;
        };
        match replay_request_until_response(tx, rx, stdout, &load_json, "session/load").await {
            ReplayOutcome::ResponseOk => {
                tracing::info!(session_id = %sid, "replay: session restored");
                restored.push(sid.clone());
            }
            ReplayOutcome::ResponseErr => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: session/load was rejected by new leader"
                );
            }
            ReplayOutcome::Failed => {
                tracing::warn!(
                    session_id = %sid,
                    "replay: transport failure during session/load; aborting remaining replays"
                );
                break;
            }
        }
    }
    state
        .last_session_id
        .as_ref()
        .filter(|sid| restored.iter().any(|r| r == *sid))
        .cloned()
        .or_else(|| restored.last().cloned())
}
/// Flush observability, then exit. Used by the agent/headless signal handler.
/// Does NOT write terminal escape codes; agent mode never enables TUI modes.
/// The TUI has its own signal handler (`app::signal_handler`) that does the full crossterm teardown.
fn shutdown_and_flush_telemetry(exit_code: i32) -> ! {
    {
        let _exit_span = tracing::info_span!("teardown.process_exit").entered();
        distill_telemetry::sentry::flush_on_shutdown();
    }
    distill_telemetry::otel_layer::shutdown_otel();
    distill_telemetry::debug_log::flush();
    finalize_span_profile();
    std::process::exit(exit_code);
}
fn finalize_span_profile() {
    if let Some(path) = distill_telemetry::span_profile::finalize() {
        eprintln!("distill: span profile written to {}", path.display());
    }
}
#[tracing::instrument(level = "debug", skip_all)]
async fn forward_stdio_line_to_leader(
    line: Vec<u8>,
    leader_tx: &tokio::sync::Mutex<tokio::sync::mpsc::UnboundedSender<String>>,
    replay_state: &std::sync::Mutex<StdioReplayState>,
    cancel: &CancellationToken,
) {
    let line = String::from_utf8_lossy(&line);
    let mut trimmed = line.trim_end_matches(['\r', '\n']).to_string();
    if trimmed.is_empty() {
        return;
    }
    cache_outgoing_acp_state(&trimmed, replay_state);
    let send_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        {
            let tx = leader_tx.lock().await;
            match tx.send(trimmed) {
                Ok(()) => break,
                Err(tokio::sync::mpsc::error::SendError(v)) => trimmed = v,
            }
        }
        if cancel.is_cancelled() || tokio::time::Instant::now() >= send_deadline {
            tracing::error!(
                "stdio bridge: dropping client message after reconnect retries were exhausted"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}
/// Emitted by both leader guards (server mode and leader-connect) so the two sites can't drift.
const PLUGIN_DIR_LEADER_WARNING: &str = "distill: --plugin-dir is ignored in leader mode; run with --no-leader to \
     load per-process plugins";
/// Run the `agent` subcommand, dispatching to the appropriate mode.
#[tracing::instrument(level = "debug", skip_all)]
async fn run_agent_command(
    agent_args: Box<distill_pager::app::AgentArgs>,
    permission_mode_flag: Option<String>,
    trust: bool,
    no_auto_update: bool,
    disable_web_search: bool,
    update_config: &UpdateConfig,
) -> Result<()> {
    let signal_flush = agent_command::spawn_signal_flush();
    if matches!(
        agent_args.mode,
        Some(AgentCmd::Leader(_) | AgentCmd::Stdio | AgentCmd::Headless(_) | AgentCmd::Serve(_))
    ) {
        distill_shell::agent::app::suppress_otel();
    }
    init_tracing_simple("agent");
    let _otel_guard = distill_telemetry::otel_layer::otel_guard();
    distill_telemetry::instrumentation::install_panic_hook();
    if trust {
        use distill_workspace::folder_trust::{grant_folder_trust, report_cli_trust_grant};
        match std::env::current_dir() {
            Ok(cwd) => report_cli_trust_grant(&grant_folder_trust(&cwd)),
            Err(e) => {
                tracing::warn!(error = %e, "--trust: failed to resolve cwd; folder not trusted");
                let _ = writeln!(
                    std::io::stderr(),
                    "error: --trust: failed to resolve cwd; folder not trusted: {e}"
                );
            }
        }
    }
    let grok_com_config = load_grok_com_config_for_settings();
    let settings_query = distill_shell::agent::remote_config::settings_get::SettingsQuery::resolve(
        None,
        Some(grok_com_config.clone()),
    );
    let had_prefetch =
        distill_shell::agent::remote_config::settings_get::is_eligible(&settings_query);
    if had_prefetch {
        distill_shell::agent::remote_config::settings_get::warm_startup_settings(
            settings_query.clone(),
        );
    }
    let is_stdio = matches!(agent_args.mode, Some(AgentCmd::Stdio));
    let is_leader = matches!(agent_args.mode, Some(AgentCmd::Leader(_)));
    if !is_stdio && !is_leader {
        eprintln!(
            "Distill (pager) - v{}",
            distill_version::display_version_with_commit(
                env!("VERSION_WITH_COMMIT"),
                distill_update::channel_label(),
            )
        );
        if should_check_for_updates(no_auto_update) {
            auto_update::run_update_if_available(
                auto_update::UpdateRunMode::NonBlocking,
                false,
                auto_update::CliUpdateTrigger::AutoBackground,
                update_config,
            )
            .await
            .ok();
        }
    }
    let remote_settings = if had_prefetch {
        let wait = distill_shell::agent::remote_config::settings_get::await_startup_settings(
            settings_query,
            EARLY_PREFETCH_WAIT,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await;
        distill_shell::agent::remote_config::settings_get::consume_wait(
            wait,
            None,
            &grok_com_config,
        )
    } else {
        None
    };
    distill_shell::util::config::set_remote_campaigns_from_settings(remote_settings.as_ref());
    let raw_config = distill_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?;
    let mut agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {}", e))?;
    agent_config.default_model_override = agent_args.model.clone();
    agent_config.reasoning_effort_override = agent_args
        .reasoning_effort
        .as_deref()
        .and_then(distill_shell::sampling::types::parse_canonical_effort_token);
    let launch_yolo = distill_shell::util::config::effective_yolo_for_launch(
        agent_args.yolo,
        permission_mode_flag.as_deref(),
        None,
    );
    if let Some(warning) = launch_yolo.blocked_warning {
        eprintln!("distill: {warning}");
    }
    agent_config.default_yolo_mode = launch_yolo.yolo;
    agent_config.default_auto_mode = distill_shell::util::config::effective_auto_for_launch(
        agent_args.yolo,
        permission_mode_flag.as_deref(),
        None,
        distill_shell::util::config::PermissionMode::Ask,
    );
    agent_config.agent_profile_path = agent_args
        .agent_profile
        .as_deref()
        .map(resolve_agent_profile_path);
    agent_config.client_version = Some(PAGER_CLIENT_VERSION.to_string());
    if is_leader && !agent_args.plugin_dirs.is_empty() {
        eprintln!("{PLUGIN_DIR_LEADER_WARNING}");
    } else {
        agent_config.plugins.cli_plugin_dirs = agent_args.canonical_plugin_dirs();
    }
    apply_agent_endpoint_args(&agent_args, &mut agent_config);
    agent_config.remote_settings = remote_settings.clone();
    agent_config.resolve_runtime_fields(&distill_shell::agent::config::RuntimeResolutionContext {
        raw_config: &raw_config,
        remote_settings: remote_settings.as_ref(),
        is_headless: !is_leader,
        cli_subagents: None,
        cli_web_search_model: None,
        cli_session_summary_model: None,
        memory_enabled_override: None,
        disable_web_search,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });
    let agent_memory_config = agent_config.memory_config.clone();
    let runtime = AgentRuntime::from_args(&agent_args, &agent_config, &raw_config);
    let requested_confinement = distill_sandbox::requested_confinement_profile();
    let LeaderMode {
        use_leader,
        policy_disable_reason,
        disabled_by_confinement,
    } = runtime.leader_mode();
    tracing::info!(
        use_leader,
        ?policy_disable_reason,
        sandbox_profile = ?requested_confinement,
        leader_disabled_by_sandbox = disabled_by_confinement.is_some(),
        "leader mode resolved"
    );
    if let Some(profile) = disabled_by_confinement {
        warn_leader_disabled_by_sandbox(profile);
    }
    use distill_telemetry::process_info::LeaderMode::{Attached, Standalone};
    set_identity(ProcessIdentity {
        entrypoint: match &agent_args.mode {
            Some(AgentCmd::Stdio) => Entrypoint::Embedded,
            Some(AgentCmd::Leader(_)) => Entrypoint::Leader,
            Some(AgentCmd::Serve(_)) => Entrypoint::Workspace,
            Some(AgentCmd::Headless(_)) | None => Entrypoint::Headless,
        },
        leader: if use_leader || matches!(agent_args.mode, Some(AgentCmd::Leader(_))) {
            Attached
        } else {
            Standalone
        },
        interactivity: Interactivity::Unattended,
    });
    let managed_install = is_managed_install(
        std::env::current_exe().ok(),
        &distill_shell::util::distill_home::distill_home(),
    );
    if stdio_auto_update_enabled(
        is_stdio,
        use_leader,
        should_check_for_updates(no_auto_update),
        managed_install,
    ) {
        let update_config = update_config.clone();
        tokio::spawn(async move {
            auto_update::run_update_if_available(
                auto_update::UpdateRunMode::NonBlocking,
                false,
                auto_update::CliUpdateTrigger::AutoBackground,
                &update_config,
            )
            .await
            .ok();
        });
    } else if is_stdio && !use_leader && !managed_install {
        tracing::debug!("stdio auto-update skipped: not the managed install");
    }
    if use_leader {
        if !agent_args.plugin_dirs.is_empty() {
            eprintln!("{PLUGIN_DIR_LEADER_WARNING}");
        }
        use distill_shell::leader::{
            ClientCapabilities, ClientMode, LeaderReconnector, ReconnectPolicy, connect_or_spawn,
        };
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;
        use tokio::sync::Mutex as TokioMutex;
        let mode = match &agent_args.mode {
            Some(AgentCmd::Stdio) => ClientMode::Stdio,
            Some(AgentCmd::Headless(_)) | None => ClientMode::Headless,
            _ => ClientMode::Stdio,
        };
        let env_urls = distill_shell::leader::LeaderEnvUrls::from(&agent_config.grok_com_config);
        let default_model = agent_config
            .default_model_override
            .clone()
            .or(agent_config.models.default.clone());
        let client_type = std::env::args().collect::<Vec<_>>().join(" ");
        let capabilities = ClientCapabilities {
            yolo_mode: launch_yolo.yolo,
            auto_mode: agent_config.default_auto_mode && !launch_yolo.yolo,
            default_model,
            client_version: Some(PAGER_CLIENT_VERSION.to_string()),
            code_nav_enabled: false,
            terminal: false,
            fs_read: false,
            fs_write: false,
            status_line: false,
            user_message_echo: false,
        };
        let conn = connect_or_spawn(&client_type, mode, &env_urls, capabilities.clone()).await?;
        let (tx, rx) = conn.into_channels();
        let (status_tx, _status_rx) = LeaderReconnector::status_channel();
        let reconnector = LeaderReconnector::new(
            &client_type,
            mode,
            env_urls.clone(),
            capabilities,
            status_tx,
        );
        let cancel = CancellationToken::new();
        match mode {
            ClientMode::Stdio => {
                if let Err(error) = distill_tty_utils::kill_current_process_on_parent_death() {
                    tracing::warn!(
                        %error,
                        "failed to bind to parent death; stdio bridge will not die \
                         with its parent — stdin EOF remains the only cleanup"
                    );
                }
                let replay_state = Arc::new(std::sync::Mutex::new(StdioReplayState::default()));
                let leader_tx = Arc::new(TokioMutex::new(tx));
                let leader_tx_stdin = leader_tx.clone();
                let replay_state_stdin = replay_state.clone();
                let cancel_stdin = cancel.clone();
                let stdin_task = tokio::spawn(async move {
                    let mut stdin_lines = distill_acp_lib::spawn_stdin_line_reader();
                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel_stdin.cancelled() => break,
                            maybe_line = stdin_lines.recv() => {
                                let Some(line) = maybe_line else { break };
                                forward_stdio_line_to_leader(
                                    line,
                                    &leader_tx_stdin,
                                    &replay_state_stdin,
                                    &cancel_stdin,
                                )
                                .await;
                            }
                        }
                    }
                });
                let cancel_stdout = cancel.clone();
                let stdout_task = tokio::spawn(async move {
                    let mut stdout = tokio::io::stdout();
                    let mut rx = rx;
                    loop {
                        match rx.recv().await {
                            Some(ref msg) => {
                                if msg.contains("\"sessionId\"") || msg.contains("\"session_id\"") {
                                    cache_incoming_session_id(msg, &replay_state);
                                }
                                if stdout.write_all(msg.as_bytes()).await.is_err()
                                    || stdout.write_all(b"\n").await.is_err()
                                    || stdout.flush().await.is_err()
                                {
                                    break;
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "Leader disconnected (stdio), attempting reconnect..."
                                );
                                match reconnector
                                    .reconnect(ReconnectPolicy::bounded(), &cancel_stdout)
                                    .await
                                {
                                    Ok((new_tx, mut new_rx, _disconnect_rx)) => {
                                        tracing::info!("Reconnected to leader (stdio)");
                                        let replayed_session_id = {
                                            let mut tx_guard = leader_tx.lock().await;
                                            *tx_guard = new_tx;
                                            let state = replay_state
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .clone();
                                            replay_acp_state_after_reconnect(
                                                &tx_guard,
                                                &mut new_rx,
                                                &mut stdout,
                                                &state,
                                            )
                                            .await
                                        };
                                        rx = new_rx;
                                        reconnector.notify_connected();
                                        let params = match replayed_session_id {
                                            Some(ref sid) => {
                                                serde_json::json!({ "sessionId": sid }).to_string()
                                            }
                                            None => "{}".to_string(),
                                        };
                                        let notification = format!(
                                            r#"{{"jsonrpc":"2.0","method":"x.ai/leader_reconnected","params":{params}}}"#
                                        );
                                        let _ = stdout.write_all(notification.as_bytes()).await;
                                        let _ = stdout.write_all(b"\n").await;
                                        let _ = stdout.flush().await;
                                        continue;
                                    }
                                    Err(e) => {
                                        tracing::error!(error = %e, "Failed to reconnect (stdio)");
                                        cancel_stdout.cancel();
                                        break;
                                    }
                                }
                            }
                        }
                    }
                });
                tokio::select! {
                    _ = stdin_task => {}
                    _ = stdout_task => {}
                }
                return Ok(());
            }
            ClientMode::Headless => {
                drop(tx);
                let mut rx = rx;
                loop {
                    match rx.recv().await {
                        Some(_) => continue,
                        None => {
                            tracing::warn!(
                                "Leader disconnected (headless), attempting reconnect..."
                            );
                            match reconnector
                                .reconnect(ReconnectPolicy::bounded(), &cancel)
                                .await
                            {
                                Ok((_new_tx, new_rx, _disconnect_rx)) => {
                                    tracing::info!("Reconnected to leader (headless)");
                                    rx = new_rx;
                                    reconnector.notify_connected();
                                    continue;
                                }
                                Err(e) => {
                                    tracing::error!(error = %e, "Failed to reconnect (headless)");
                                    break;
                                }
                            }
                        }
                    }
                }
                return Ok(());
            }
        }
    }
    match agent_args.mode {
        Some(AgentCmd::Stdio) => {
            agent_command::run_stdio(&runtime, &agent_config, signal_flush).await
        }
        Some(AgentCmd::Headless(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a, &mut agent_config);
            run_headless(
                &agent_config,
                agent_args.reauthenticate,
                agent_memory_config,
            )
            .await
        }
        Some(AgentCmd::Serve(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a.headless, &mut agent_config);
            let secret = a.get_secret();
            let server_config = distill_shell::agent::ServerConfig {
                bind_addr: a.bind,
                secret: secret.clone(),
            };
            print_serve_startup_info(a.bind, &secret);
            distill_shell::agent::run_agent_server(server_config, agent_config).await
        }
        Some(AgentCmd::Leader(a)) => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&a.headless, &mut agent_config);
            let leader_auto_update = if !should_check_for_updates(
                no_auto_update || a.no_auto_update,
            ) {
                tracing::info!("Leader auto-update disabled");
                None
            } else {
                let update_config_for_leader = update_config.clone();
                Some(distill_shell::agent::app::LeaderAutoUpdateConfig {
                    check_interval: std::time::Duration::from_secs(60 * 60),
                    check_fn: Box::new(move || {
                        let uc = update_config_for_leader.clone();
                        Box::pin(async move {
                            let current_config = distill_shell::util::config::load_config().await;
                            if current_config.cli.auto_update == Some(false) {
                                return false;
                            }
                            match auto_update::ensure_latest_on_disk(&uc).await {
                                Ok(outcome) => {
                                    if let Some(v) = &outcome.installed {
                                        if let Err(e) = distill_shell::managed_config::sync().await
                                        {
                                            tracing::warn!(
                                                "Leader auto-update: managed config refresh failed: {e}"
                                            );
                                        }
                                        tracing::info!(
                                            "Leader auto-update: v{v} installed successfully"
                                        );
                                    } else if outcome.relaunch_needed {
                                        tracing::info!(
                                            "Leader auto-update: newer binary already on disk, \
                                             relaunching without download"
                                        );
                                    }
                                    outcome.relaunch_needed
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Leader auto-update: check/download failed, \
                                         staying alive: {e:#}"
                                    );
                                    false
                                }
                            }
                        })
                    }),
                })
            };
            let cursor_worker = None;
            run_leader(
                &agent_config,
                distill_shell::agent::app::LeaderRunOptions {
                    no_exit_on_disconnect: a.no_exit_on_disconnect,
                    relay_on_demand: a.relay_on_demand,
                    auto_update_check: leader_auto_update,
                    memory_config: agent_memory_config,
                    cursor_worker,
                },
            )
            .await
        }
        None => {
            let mut agent_config = agent_config.clone();
            apply_headless_args_to_config(&agent_args.headless, &mut agent_config);
            run_headless(
                &agent_config,
                agent_args.reauthenticate,
                agent_memory_config,
            )
            .await
        }
    }
}
/// Raise the per-process fd soft limit toward the hard limit. A 1024 limit fails with EMFILE under a ~100-session
/// wave. Best-effort: never blocks startup (containers/cgroups may pin limits).
#[cfg(unix)]
fn raise_fd_limit() {
    #[cfg(target_os = "macos")]
    const TARGET: libc::rlim_t = 8192;
    #[cfg(not(target_os = "macos"))]
    const TARGET: libc::rlim_t = 65536;
    unsafe {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) != 0 {
            return;
        }
        let new_cur = rlim.rlim_max.min(TARGET);
        if new_cur <= rlim.rlim_cur {
            return;
        }
        let old = rlim.rlim_cur;
        rlim.rlim_cur = new_cur;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &rlim) == 0 {
            tracing::trace!(old, new = new_cur, "raised RLIMIT_NOFILE");
        }
    }
}
#[cfg(not(unix))]
fn raise_fd_limit() {}
/// Clears `args.command` so the regular subcommand match doesn't try to handle it. The only gate is the feature
/// flag: a disabled dashboard (`[dashboard].enabled = false` / `GROK_AGENT_DASHBOARD=0`) is a CLI error. It fires
/// here, before the TUI starts, because the welcome view silently drops the equivalent runtime toast.
fn flag_dashboard_at_startup_if_requested(args: &mut PagerArgs) -> Result<()> {
    if !matches!(args.command, Some(Command::Dashboard)) {
        return Ok(());
    }
    if !distill_pager::views::dashboard::dashboard_enabled() {
        anyhow::bail!(
            "the Agent Dashboard is disabled. Enable it by removing \
             `[dashboard] enabled = false` from ~/.grok/config.toml and \
             unsetting GROK_AGENT_DASHBOARD=0."
        );
    }
    args.command = None;
    unsafe { std::env::set_var("GROK_OPEN_DASHBOARD_AT_STARTUP", "1") };
    Ok(())
}
/// Kick off background work that overlaps startup. Add new prewarms here.
fn schedule_startup_prewarm() {
    distill_shell::agent::mvp_agent::warm_async_http_client();
}
fn configure_process_env(mut args: PagerArgs) -> Result<PagerArgs> {
    flag_dashboard_at_startup_if_requested(&mut args)?;
    let args = args.apply_cwd()?;
    unsafe {
        if let Some(mode) = args.compaction_mode.as_deref() {
            std::env::set_var("GROK_COMPACTION_MODE", mode);
        }
        if let Some(detail) = args.compaction_detail.as_deref() {
            std::env::set_var("GROK_COMPACTION_DETAIL", detail);
        }
        if args.chat() {
            std::env::set_var(distill_shell::agent::chat_modes::GROK_CHAT_MODE_ENV, "1");
        }
        if let Some(socket) = args.leader_socket.as_deref() {
            std::env::set_var(distill_shell::leader::LEADER_SOCKET_ENV, socket);
        }
        if args.log_sampling {
            std::env::set_var("GROK_LOG_SAMPLING", "1");
        }
        if let Some(path) = args.debug_file.as_deref() {
            std::env::set_var("GROK_DEBUG_LOG", path);
            std::env::remove_var("GROK_LOG_FILE");
        }
        if args.debug || args.debug_file.is_some() {
            if std::env::var_os("GROK_DEBUG_LOG").is_none() {
                std::env::set_var("GROK_DEBUG_LOG", "1");
            }
            if std::env::var_os("GROK_HOOKS_LOG").is_none() {
                std::env::set_var("GROK_HOOKS_LOG", "1");
            }
        }
    }
    Ok(args)
}
const RUNTIME_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const GROK_WORKER_THREADS_ENV: &str = "GROK_WORKER_THREADS";
/// tokio defaults to one worker per logical CPU.
/// On a host with hundreds of CPUs that can exhaust a cgroup thread budget at startup and abort under `panic = "abort"`.
/// A terminal UI is I/O-bound, so cap at 8.
const DEFAULT_MAX_WORKER_THREADS: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// How `GROK_WORKER_THREADS` resolved.
#[derive(Debug, PartialEq, Eq)]
enum WorkerCount {
    Accepted(NonZeroUsize),
    Clamped {
        requested: i128,
        used: NonZeroUsize,
        cores: NonZeroUsize,
    },
    Ignored {
        value: String,
        used: NonZeroUsize,
    },
}
impl WorkerCount {
    fn used(&self) -> NonZeroUsize {
        match self {
            Self::Accepted(used) | Self::Clamped { used, .. } | Self::Ignored { used, .. } => *used,
        }
    }
    fn notice(&self) -> Option<String> {
        match self {
            Self::Accepted(_) => None,
            Self::Clamped {
                requested,
                used,
                cores,
            } => Some(format!(
                "distill: clamped {GROK_WORKER_THREADS_ENV}={requested} to {used} (valid range is 1..={cores})"
            )),
            Self::Ignored { value, .. } => Some(format!(
                "distill: ignoring {GROK_WORKER_THREADS_ENV}={value:?} (not a valid integer)"
            )),
        }
    }
}
fn cli_worker_threads() -> NonZeroUsize {
    let cores = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
    let resolved = match std::env::var(GROK_WORKER_THREADS_ENV) {
        Ok(value) => worker_threads_from(Some(&value), cores),
        Err(std::env::VarError::NotPresent) => worker_threads_from(None, cores),
        Err(std::env::VarError::NotUnicode(value)) => WorkerCount::Ignored {
            value: value.to_string_lossy().into_owned(),
            used: default_worker_threads(cores),
        },
    };
    if let Some(notice) = resolved.notice() {
        eprintln!("{notice}");
    }
    resolved.used()
}
fn worker_threads_from(env_override: Option<&str>, cores: NonZeroUsize) -> WorkerCount {
    match env_override {
        Some(value) => resolve_worker_override(value, cores),
        None => WorkerCount::Accepted(default_worker_threads(cores)),
    }
}
fn default_worker_threads(cores: NonZeroUsize) -> NonZeroUsize {
    cores.min(DEFAULT_MAX_WORKER_THREADS)
}
fn resolve_worker_override(value: &str, cores: NonZeroUsize) -> WorkerCount {
    let Ok(requested) = value.trim().parse::<i128>() else {
        return WorkerCount::Ignored {
            value: value.to_owned(),
            used: default_worker_threads(cores),
        };
    };
    let clamped = requested.clamp(1, cores.get() as i128) as usize;
    let used = NonZeroUsize::new(clamped).expect("clamp floor of 1 guarantees non-zero");
    if requested == used.get() as i128 {
        WorkerCount::Accepted(used)
    } else {
        WorkerCount::Clamped {
            requested,
            used,
            cores,
        }
    }
}
/// A plain runtime drop blocks forever on an uncancellable in-flight blocking task.
/// `shutdown_timeout` abandons it after `grace` so exit can't hang.
fn run_and_shutdown<F: std::future::Future>(
    runtime: tokio::runtime::Runtime,
    fut: F,
    grace: std::time::Duration,
) -> F::Output {
    let output = runtime.block_on(fut);
    runtime.shutdown_timeout(grace);
    output
}
/// Return freed-but-retained jemalloc pages to the OS. Resuming a long session then doesn't leave hundreds of MB of
/// dead pages counted against the process for its lifetime.
#[cfg(all(feature = "jemalloc", unix))]
fn purge_jemalloc_retained_pages() {
    static NAME: &[u8] = b"arena.4096.purge\0";
    let ret = unsafe {
        tikv_jemalloc_sys::mallctl(
            NAME.as_ptr().cast(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        static WARN_ONCE: std::sync::Once = std::sync::Once::new();
        WARN_ONCE.call_once(|| {
            tracing::warn!(
                errno = ret,
                "jemalloc arena purge mallctl failed; retained-page release is inert"
            );
        });
    }
}
/// Allocator gauges for the memory trace (`memory_trace` hook): advance the jemalloc epoch so the `stats.*` reads are current, then read each gauge.
/// Returns `None` if any mallctl fails (trace records the absence).
/// Uses the `tikv-jemalloc-ctl` raw helpers (shared with the heap-profile hooks below) instead of hand-rolled mallctl.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_allocator_stats() -> Option<distill_pager::memory_trace::AllocatorStats> {
    /// SAFETY: callers pass fixed NUL-terminated `stats.*` size_t ctl names.
    unsafe fn gauge(name: &[u8]) -> Option<u64> {
        unsafe {
            tikv_jemalloc_ctl::raw::read::<usize>(name)
                .ok()
                .map(|v| v as u64)
        }
    }
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        Some(distill_pager::memory_trace::AllocatorStats {
            allocated: gauge(b"stats.allocated\0")?,
            active: gauge(b"stats.active\0")?,
            resident: gauge(b"stats.resident\0")?,
            mapped: gauge(b"stats.mapped\0")?,
            retained: gauge(b"stats.retained\0")?,
            metadata: gauge(b"stats.metadata\0")?,
        })
    }
}
/// Full jemalloc statistics dump for threshold snapshots (`malloc_stats_print` default human-readable format, arena detail included).
/// This is the artifact the GCS memory-trace upload ships for offline analysis.
/// Raw `tikv_jemalloc_sys` because jemalloc-ctl has no callback-form stats_print.
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_stats_dump() -> String {
    unsafe extern "C" fn append(opaque: *mut std::ffi::c_void, msg: *const std::ffi::c_char) {
        unsafe {
            let out = &mut *opaque.cast::<String>();
            out.push_str(&std::ffi::CStr::from_ptr(msg).to_string_lossy());
        }
    }
    let mut out = String::new();
    unsafe {
        tikv_jemalloc_sys::malloc_stats_print(
            Some(append),
            (&raw mut out).cast(),
            std::ptr::null(),
        );
    }
    out
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_heap_stats() -> Option<distill_shell::heap_profile::JemallocStats> {
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"epoch\0", 1u64).ok()?;
        let allocated = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.allocated\0").ok()? as u64;
        let resident = tikv_jemalloc_ctl::raw::read::<usize>(b"stats.resident\0").ok()? as u64;
        Some(distill_shell::heap_profile::JemallocStats {
            allocated,
            resident,
        })
    }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_set_prof_active(active: bool) -> bool {
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.active\0", active).is_ok() }
}
#[cfg(all(test, feature = "jemalloc", unix))]
fn jemalloc_read_prof_active() -> Option<bool> {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"prof.active\0").ok() }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_prof_available() -> bool {
    unsafe { tikv_jemalloc_ctl::raw::read::<bool>(b"opt.prof\0").unwrap_or(false) }
}
#[cfg(all(feature = "jemalloc", unix))]
fn jemalloc_dump_to_path(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if !jemalloc_prof_available() {
        return Err("opt.prof false".into());
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", c.as_ptr()) }.map_err(|e| e.to_string())
}
#[cfg(all(feature = "jemalloc", unix))]
fn install_heap_profile_hooks() {
    distill_shell::heap_profile::install(distill_shell::heap_profile::HeapProfileHooks {
        stats: jemalloc_heap_stats,
        set_prof_active: jemalloc_set_prof_active,
        dump_to_path: jemalloc_dump_to_path,
        prof_available: jemalloc_prof_available,
    });
}
fn version_text(channel_label: &str) -> String {
    format!(
        "distill {}\n",
        distill_version::display_version_with_commit(
            distill_version::full_version(),
            channel_label,
        )
    )
}
fn write_version(writer: &mut impl std::io::Write, channel_label: &str) -> std::io::Result<()> {
    writer.write_all(version_text(channel_label).as_bytes())
}
fn dispatch_version_if_requested(args: &PagerArgs) -> bool {
    if !args.version {
        return false;
    }
    if let Err(error) = write_version(
        &mut std::io::stdout().lock(),
        distill_update::channel_label(),
    ) {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
    true
}
fn dispatch_doctor_if_requested(args: &PagerArgs) -> bool {
    let Some(Command::Doctor(doctor_args)) = &args.command else {
        return false;
    };
    if let Err(error) = distill_pager::doctor_cmd::run(doctor_args.clone()) {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
    true
}
fn main() {
    distill_version::set_full_version(env!("VERSION_WITH_COMMIT"));
    distill_telemetry::startup::mark_process_start();
    if let Some(code) = distill_pager::app::mermaid_worker::maybe_run_render_subprocess() {
        std::process::exit(code);
    }
    if let Some(code) = distill_pager::voice::maybe_run_capture_subprocess() {
        std::process::exit(code);
    }
    set_release_channel(ReleaseChannel::from_label(
        distill_update::channel_name().unwrap_or_default(),
    ));
    let args = PagerArgs::parse_cli();
    if dispatch_version_if_requested(&args) || dispatch_doctor_if_requested(&args) {
        return;
    }
    distill_pager_minimal::install();
    #[cfg(all(feature = "jemalloc", unix))]
    distill_pager::memory_release::install_release_hook(purge_jemalloc_retained_pages);
    #[cfg(all(feature = "jemalloc", unix))]
    {
        distill_pager::memory_trace::install_allocator_stats_provider(jemalloc_allocator_stats);
        distill_pager::memory_trace::install_allocator_dump_provider(jemalloc_stats_dump);
    }
    #[cfg(all(feature = "jemalloc", unix))]
    install_heap_profile_hooks();
    unsafe {
        distill_shell::agent::external_otel_pin::strip_conflicting_process_env();
    }
    let args = configure_process_env(args).unwrap_or_else(|err| {
        eprintln!("distill: {err:#}");
        std::process::exit(1);
    });
    distill_pager::memory_trace::start(distill_pager::memory_trace::default_dir());
    raise_fd_limit();
    if let Err(e) = distill_config::validate_requirements() {
        eprintln!("Couldn't start Distill: {e}");
        eprintln!();
        eprintln!(
            "Update Distill to a version the policy allows, or ask your administrator \
             to fix the managed requirements."
        );
        std::process::exit(2);
    }
    let _sentry_guard = distill_telemetry::sentry::init(distill_telemetry::sentry::Config {
        client: "distill",
        client_version: PAGER_CLIENT_VERSION,
        release: env!("VERSION_WITH_COMMIT"),
        disabled: distill_shell::agent::config::is_error_reporting_disabled_sync(),
    });
    distill_pager::docs::extract_user_guide_docs(&distill_shell::util::distill_home::distill_home());
    distill_crash_handler::install_terminal_restore_only();
    if distill_shell::util::config::load_crash_handler_enabled_sync() {
        let crash_dir = distill_shell::util::distill_home::distill_home().join("crash");
        if let Some(report) = distill_crash_handler::check_previous_crash(&crash_dir) {
            eprintln!("Distill crashed during your last session.");
            eprintln!("  Signal:  {}", report.signal_name);
            eprintln!("  Version: {}", report.app_version);
            eprintln!("  Report:  {}", report.report_path.display());
            eprintln!();
        }
        if !distill_crash_handler::install(distill_crash_handler::CrashHandlerConfig {
            app_version: env!("VERSION_WITH_COMMIT").to_string(),
            crash_dir: crash_dir.clone(),
        }) {
            eprintln!(
                "warning: crash handler enabled but failed to install (check permissions on {})",
                crash_dir.display()
            );
        }
    }
    let crashed = distill_active_sessions::collect_crashed().unwrap_or_default();
    if !crashed.is_empty() {
        tracing::info!(
            count = crashed.len(),
            "Found crashed sessions from a previous run"
        );
    }
    let workers = cli_worker_threads();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers.get()).enable_all();
    let runtime = distill_tty_utils::runtime::build_with_blocking_pool(&mut builder)
        .unwrap_or_else(|e| {
            eprintln!("distill: failed to start tokio runtime: {e}");
            shutdown_and_flush_telemetry(1);
        });
    let result = run_and_shutdown(runtime, async_main(args), RUNTIME_SHUTDOWN_GRACE);
    distill_telemetry::debug_log::flush();
    if let Err(e) = result {
        distill_tty_utils::restore_native_stderr();
        finalize_span_profile();
        let report = match e.downcast_ref::<distill_pager::app::StartupFailure>() {
            Some(startup) => startup.user_report(),
            None => format!("Error: {e:#}"),
        };
        distill_pager::best_effort_stderr::eprint_line(&report);
        drop(_sentry_guard);
        std::process::exit(1);
    }
    finalize_span_profile();
}
#[tracing::instrument(level = "debug", skip_all)]
async fn async_main(mut args: PagerArgs) -> Result<()> {
    distill_extra_ca::ensure_default_crypto_provider();
    if let Some(Command::Completions { shell }) = &args.command {
        distill_pager::completions_cmd::run(*shell);
        return Ok(());
    }
    if let Some(Command::Wrap(ref wrap_args)) = args.command {
        return distill_pager::wrap_cmd::run(wrap_args);
    }
    let is_interactive = args.command.is_none()
        && args.single.is_none()
        && args.prompt_json.is_none()
        && args.prompt_file.is_none();
    distill_shell::http::set_client_name(if is_interactive {
        distill_workspace::permission::ClientType::GrokPager
    } else {
        distill_workspace::permission::ClientType::Generic
    });
    schedule_startup_prewarm();
    args.pin_local_resume_target()?;
    let saved_profile = args.saved_resume_profile();
    let sandbox_profile_arg = match args.startup_sandbox_profile(saved_profile.as_deref()) {
        distill_pager::app::cli::SandboxStartup::Apply(profile) => profile,
        distill_pager::app::cli::SandboxStartup::Conflict { requested, saved } => {
            eprintln!(
                "error: cannot resume this session under sandbox profile '{requested}' — \
                 it was created with '{saved}'. Omit --sandbox to resume with '{saved}', \
                 or start a new session to use '{requested}'."
            );
            std::process::exit(1);
        }
    };
    if args.trust {
        use distill_workspace::folder_trust::{grant_folder_trust, report_cli_trust_grant};
        match std::env::current_dir() {
            Ok(cwd) => report_cli_trust_grant(&grant_folder_trust(&cwd)),
            Err(e) => {
                tracing::warn!(error = %e, "--trust: failed to resolve cwd; folder not trusted");
                let _ = writeln!(
                    std::io::stderr(),
                    "error: --trust: failed to resolve cwd; folder not trusted: {e}"
                );
            }
        }
    }
    if command_needs_pre_sandbox_policy_heal(args.command.as_ref()) {
        match distill_shell::config::load_agent_config_disk_only() {
            Ok(agent_cfg) => {
                let auth_manager =
                    std::sync::Arc::new(distill_login::AuthManager::new_with_proxy_base_url(
                        &distill_shell::util::distill_home::distill_home(),
                        agent_cfg.grok_com_config.clone(),
                        agent_cfg.endpoints.proxy_url(),
                    ));
                auth_manager.configure_refresher(
                    agent_cfg.grok_com_config.auth_provider_command.clone(),
                    None,
                );
                distill_shell::managed_config::ensure_managed_policy_present(&auth_manager).await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "managed policy: skipped session-start heal (disk config load failed)"
                );
            }
        }
    }
    distill_shell::config::apply_sandbox(None, sandbox_profile_arg.as_deref(), args.cwd.as_deref());
    if let Some(identity) = process_identity(args.command.as_ref(), is_interactive) {
        set_identity(identity);
    }
    let update_config = build_update_config();
    if let Some(command) = args.command.take() {
        match command {
            Command::Version { json } => {
                if json {
                    let payload = serde_json::json!({
                        "currentVersion": env!("VERSION_WITH_COMMIT"),
                        "channel": distill_update::channel_name().unwrap_or("unknown"),
                    });
                    println!("{}", serde_json::to_string(&payload)?);
                } else {
                    write_version(
                        &mut std::io::stdout().lock(),
                        distill_update::channel_label(),
                    )?;
                }
                return Ok(());
            }
            Command::Agent(agent_args) => {
                if args.leader || args.no_leader {
                    let flag = if args.leader {
                        "--leader"
                    } else {
                        "--no-leader"
                    };
                    anyhow::bail!(
                        "top-level {flag} applies to the pager TUI, not the agent subcommand. \
                         Use `distill agent {flag}` instead."
                    );
                }
                enforce_version_policy_or_exit();
                return run_agent_command(
                    agent_args,
                    args.permission_mode_flag.clone(),
                    args.trust,
                    args.no_auto_update,
                    args.disable_web_search,
                    &update_config,
                )
                .await;
            }
            Command::Doctor(_) => {
                unreachable!("doctor was consumed before runtime startup")
            }
            Command::Inspect { json } => {
                let cwd = std::env::current_dir().unwrap_or_default();
                distill_shell::inspect::inspect(&cwd, json).await?;
                return Ok(());
            }
            Command::Setup { json } => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                run_setup_command(json).await;
                return Ok(());
            }
            Command::Mcp(mcp_args) => {
                init_tracing_simple("cli");
                return distill_pager::mcp_cmd::run(mcp_args).await;
            }
            Command::Plugin(plugin_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                return distill_pager::plugin_cmd::run(plugin_args).await;
            }
            Command::Models => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let agent_config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return distill_pager::models::list_available_models(&agent_config).await;
            }
            Command::Leader(leader_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                return run_leader_mgmt(leader_args).await;
            }
            Command::Worktree(worktree_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let agent_config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return distill_pager::worktree_cmd::run(worktree_args, &agent_config).await;
            }
            Command::DiskUsage(disk_usage_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                return distill_pager::disk_usage_cmd::run(disk_usage_args);
            }
            Command::Workspace(workspace_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                return run_workspace_mgmt(workspace_args).await;
            }
            Command::Sessions(sessions_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let agent_config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return distill_pager::sessions_cmd::run(sessions_args, &agent_config).await;
            }
            Command::Usage(usage_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                return distill_pager::usage_cmd::run(usage_args);
            }
            Command::Share(ref share_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let agent_config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return distill_pager::share_cmd::run(share_args, &agent_config).await;
            }
            Command::Export(export_args) => {
                init_tracing_simple("cli");
                return distill_pager::export_cmd::run(export_args);
            }
            Command::Trace(trace_args) => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let agent_config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                return distill_pager::trace_cmd::run(trace_args, &agent_config).await;
            }
            Command::Memory(memory_args) => {
                let grok_com_config = load_grok_com_config_for_settings();
                let remote_settings = fetch_remote_settings(&grok_com_config).await;
                distill_shell::util::config::set_remote_campaigns_from_settings(
                    remote_settings.as_ref(),
                );
                let config = distill_shell::config::load_effective_config()?;
                let mode = distill_shell::config::MemoryConfig::resolve(
                    args.experimental_memory,
                    args.no_memory,
                    &config,
                    remote_settings.as_ref(),
                )
                .mode;
                return distill_pager::memory_cmd::run(memory_args, mode);
            }
            Command::Update {
                check,
                json,
                force_reinstall,
                version,
                alpha,
                stable,
                enterprise,
                trigger,
                auto,
            } => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                let channel_switch = get_channel_switch(alpha, stable, enterprise);
                let trigger = resolve_update_trigger(trigger.as_deref(), auto);
                return run_update_command(
                    check,
                    json,
                    force_reinstall,
                    version,
                    channel_switch,
                    trigger,
                    &update_config,
                )
                .await;
            }
            Command::Login {
                legacy: _,
                chatgpt,
                oauth,
                device_auth,
                devbox,
            } => {
                init_tracing_simple("cli");
                let _otel_guard = distill_telemetry::otel_layer::otel_guard();
                if chatgpt {
                    // This harness's own ChatGPT OAuth: its own token store, its
                    // own refresh, and the account's GPT models behind it.
                    let account = distill_shell::codex_auth::run_cli_login(device_auth).await?;
                    let label = account
                        .email
                        .as_deref()
                        .or(account.account_id.as_deref())
                        .unwrap_or("ChatGPT account");
                    println!("Connected ChatGPT as {label}.");
                    println!();
                    distill_shell::instrumentation::finalize_and_exit(0);
                }
                let config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                let authenticated = distill_login::run_cli_login(
                    config.grok_com_config.clone(),
                    config.login_device_flow,
                    config.endpoints.proxy_url(),
                    oauth,
                    device_auth,
                    devbox,
                    |auth_manager| {
                        distill_shell::agent::init::update_telemetry_config(&config, auth_manager)
                    },
                )
                .await?;
                distill_shell::agent::init::apply_post_login_config(authenticated).await?;
                println!();
                distill_shell::instrumentation::finalize_and_exit(0);
            }
            Command::Logout { chatgpt } => {
                init_tracing_simple("cli");
                if chatgpt {
                    // Only this harness's own ChatGPT credential: the Grok
                    // sign-in is a different store and stays as it was.
                    let removed = distill_shell::codex_auth::run_cli_logout().await?;
                    if removed {
                        println!("Signed out of ChatGPT.");
                    } else {
                        println!("No ChatGPT sign-in to remove.");
                    }
                    println!();
                    distill_shell::instrumentation::finalize_and_exit(0);
                }
                let config = distill_shell::config::load_agent_config_disk_only()
                    .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
                distill_shell::agent::init::run_cli_logout(&config.grok_com_config)?;
                distill_shell::instrumentation::finalize_and_exit(0);
            }
            Command::Wrap(ref wrap_args) => {
                return distill_pager::wrap_cmd::run(wrap_args);
            }
            Command::Completions { shell } => {
                distill_pager::completions_cmd::run(shell);
                return Ok(());
            }
            Command::Dashboard => {
                args.command = Some(Command::Dashboard);
                flag_dashboard_at_startup_if_requested(&mut args)?;
            }
        }
    }
    let headless_prompt = distill_pager::headless::HeadlessPrompt::from_args(
        args.single.as_deref(),
        args.prompt_json.as_deref(),
        args.prompt_file.as_deref(),
    )?;
    if headless_prompt.is_some() || args.memory_flush {
        if args.memory_flush
            && headless_prompt.is_none()
            && args.resume_session.is_none()
            && args.load_session.is_none()
            && !args.continue_last_session
        {
            anyhow::bail!("--memory-flush without a prompt requires --resume/-r or --continue/-c");
        }
        init_tracing_simple(HEADLESS_ENTRYPOINT);
        let _otel_guard = distill_telemetry::otel_layer::otel_guard();
        enforce_version_policy_or_exit();
        let launch_yolo = distill_shell::util::config::effective_yolo_for_launch(
            args.yolo,
            args.permission_mode_flag.as_deref(),
            None,
        );
        if let Some(warning) = launch_yolo.blocked_warning {
            eprintln!("distill: {warning}");
        }
        let json_schema = args
            .json_schema
            .as_deref()
            .map(distill_pager::headless::parse_json_schema)
            .transpose()?;
        if json_schema.is_some()
            && args.output_format == distill_pager::headless::OutputFormat::Plain
        {
            args.output_format = distill_pager::headless::OutputFormat::Json;
        }
        let memory_enabled_override = args.memory_enabled_override();
        let memory_flush = args.memory_flush;
        return distill_pager::headless::run_single_turn(
            headless_prompt,
            args.verbatim,
            distill_pager::headless::HeadlessOptions {
                session_id: args.session_id.clone(),
                resume: args.resume_session.or(args.load_session),
                resume_title_pinned: args.resume_target_pinned,
                cwd: args.cwd,
                yolo: launch_yolo.yolo,
                trust: args.trust,
                output_format: args.output_format,
                include_partial_messages: args.include_partial_messages,
                json_schema,
                model: args.model,
                rules: args.rules,
                system_prompt_override: args.system_prompt_override.clone(),
                continue_last_session: args.continue_last_session,
                fork_session: args.fork_session,
                worktree: args.worktree,
                worktree_ref: args.worktree_ref,
                restore_code: args.restore_code,
                agent: args.agent.clone(),
                agents_json: args.agents_json.clone(),
                cli_tools: args.cli_tools.clone(),
                cli_disallowed_tools: args.cli_disallowed_tools.clone(),
                disable_web_search: args.disable_web_search,
                allow_rules: args.allow_rules.clone(),
                deny_rules: args.deny_rules.clone(),
                max_turns: args.max_turns,
                permission_mode_flag: args.permission_mode_flag.clone(),
                reasoning_effort: args.reasoning_effort.clone(),
                wait_for_background: !args.no_wait_for_background,
                background_wait_timeout: std::time::Duration::from_secs(
                    args.background_wait_timeout_secs,
                ),
                memory_flush,
                memory_enabled_override,
            },
        )
        .await;
    }
    enforce_version_policy_or_exit();
    let _otel_guard = distill_telemetry::otel_layer::otel_guard();
    type UpdateWaitHandle = tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>;
    let bg_update_wait: std::sync::Arc<tokio::sync::Mutex<Option<UpdateWaitHandle>>> =
        std::sync::Arc::new(tokio::sync::Mutex::new(None));
    let bg_update_rx: Option<tokio::sync::oneshot::Receiver<Option<auto_update::UpdateAvailable>>> =
        if should_check_for_updates(args.no_auto_update) {
            let update_config = update_config.clone();
            let wait_slot = bg_update_wait.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let check = auto_update::check_update_background(&update_config).await;
                if let Some(mut child) = check.download {
                    *wait_slot.lock().await = Some(tokio::spawn(async move { child.wait().await }));
                }
                let _ = tx.send(check.update);
            });
            Some(rx)
        } else {
            None
        };
    let result = distill_pager::app::run(args, bg_update_rx).await;
    distill_sandbox::flush();
    match result {
        Ok(true) => {
            let adopted = bg_update_wait.lock().await.take();
            if finish_update_on_exit(adopted, &update_config).await {
                eprintln!("Update installed. Run `distill` to start.");
            } else {
                eprintln!("Update did not complete. Run `distill update` to retry.");
            }
            Ok(())
        }
        Ok(false) => Ok(()),
        Err(e) => Err(e),
    }
}
/// Returns `true` when an update path completed without a reported failure. It falls back to a fresh blocking `grok
/// update` only when there is no waiter or the child failed. (No waiter means the spawn failed or no download was
/// needed because the target was already on disk.).
#[tracing::instrument(level = "debug", skip_all)]
async fn finish_update_on_exit(
    adopted: Option<tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>>,
    update_config: &UpdateConfig,
) -> bool {
    let run_blocking = |reason: Option<String>| async move {
        if let Some(reason) = reason {
            eprintln!("{reason}");
        }
        auto_update::run_update_if_available(
            auto_update::UpdateRunMode::Blocking,
            false,
            auto_update::CliUpdateTrigger::UserCommand,
            update_config,
        )
        .await
        .is_ok()
    };
    match adopted {
        Some(handle) => {
            eprintln!("Waiting for the update download to finish...");
            match handle.await {
                Ok(Ok(status)) if status.success() => true,
                Ok(Ok(status)) => {
                    run_blocking(Some(format!(
                        "Background update exited with {status}; retrying..."
                    )))
                    .await
                }
                Ok(Err(e)) => {
                    run_blocking(Some(format!(
                        "Could not wait for the background update ({e}); retrying..."
                    )))
                    .await
                }
                Err(join_err) => {
                    run_blocking(Some(format!(
                        "Background update waiter failed ({join_err}); retrying..."
                    )))
                    .await
                }
            }
        }
        None => run_blocking(None).await,
    }
}
/// Build an [`UpdateConfig`] from the current environment and config files.
fn build_update_config() -> UpdateConfig {
    let environment = distill_shell::env::DistillEnvironment::from_flags(false, false);
    let mut config = UpdateConfig::from_environment(&environment);
    cryptify::flow_stmt!({
        {
            config.deployment_key =
                distill_shell::agent::config::EndpointsConfig::default().deployment_key;
        }
    });
    config.npm_registry = std::env::var(obfstr::obfstr!("GROK_NPM_REGISTRY"))
        .ok()
        .or_else(distill_shell::util::config::load_npm_registry_sync);
    if let Ok(root) = distill_shell::config::load_effective_config_disk_only()
        && let Some(ch) = distill_shell::util::config::channel_from_toml_opt(&root)
    {
        config.channel = ch;
    }
    config
}
/// Central gate for auto-update checks; add new suppression rules here, not at call sites.
fn should_check_for_updates(no_auto_update_flag: bool) -> bool {
    if cfg!(debug_assertions) {
        return false;
    }
    if no_auto_update_flag {
        return false;
    }
    !std::env::var_os("GROK_DISABLE_AUTOUPDATER")
        .is_some_and(|v| env_flag_enabled(&v.to_string_lossy()))
}
/// Gate for the stdio agent's background auto-update: only the direct stdio agent, from the managed install.
/// Other modes update in `run_agent_command`.
fn stdio_auto_update_enabled(
    is_stdio: bool,
    use_leader: bool,
    updates_enabled: bool,
    managed_install: bool,
) -> bool {
    is_stdio && !use_leader && updates_enabled && managed_install
}
/// True when `exe` is the binary `<distill_home>/bin/grok` resolves to, the install that adopts a staged update on
/// respawn. Both sides are canonicalized; any failure reports unmanaged and skips the update. The npm shim
/// hardcodes `~/.grok`, so a custom `GROK_HOME` skips here too.
fn is_managed_install(exe: Option<std::path::PathBuf>, distill_home: &std::path::Path) -> bool {
    if distill_home.as_os_str().is_empty() {
        return false;
    }
    let Some(exe) = exe else {
        return false;
    };
    let managed = distill_config::grok_application_in(distill_home);
    match (dunce::canonicalize(&exe), dunce::canonicalize(&managed)) {
        (Ok(exe), Ok(managed)) => exe == managed,
        _ => false,
    }
}
/// Map the mutually-exclusive channel flags to a channel name.
/// clap enforces that at most one is set, so the order is irrelevant.
fn get_channel_switch(alpha: bool, stable: bool, enterprise: bool) -> Option<&'static str> {
    if alpha {
        Some("alpha")
    } else if stable {
        Some("stable")
    } else if enterprise {
        Some("enterprise")
    } else {
        None
    }
}
/// Handle `distill update [--check] [--json] [--force-reinstall] [--version X] [--alpha|--stable|--enterprise]`.
/// --trigger is the one representation; --auto is the compat alias from older parents.
/// Unknown values fall back to user_command (a human is the only caller that can produce them).
fn resolve_update_trigger(flag: Option<&str>, auto: bool) -> auto_update::CliUpdateTrigger {
    if let Some(flag) = flag {
        match flag.parse() {
            Ok(t) => return t,
            Err(e) => tracing::warn!("{e}; recording user_command"),
        }
    }
    if auto {
        auto_update::CliUpdateTrigger::AutoBackground
    } else {
        auto_update::CliUpdateTrigger::UserCommand
    }
}
#[tracing::instrument(level = "debug", skip_all)]
async fn run_update_command(
    check: bool,
    json: bool,
    force_reinstall: bool,
    version: Option<String>,
    channel_switch: Option<&str>,
    trigger: auto_update::CliUpdateTrigger,
    base_update_config: &UpdateConfig,
) -> Result<()> {
    if json && !check {
        anyhow::bail!("--json requires --check");
    }
    let mut update_config = base_update_config.clone();
    if check {
        if version.is_some() {
            anyhow::bail!("--version cannot be used with --check");
        }
        auto_update::apply_channel_switch(channel_switch, &mut update_config).await;
        let status = auto_update::check_update_status(&update_config).await;
        auto_update::print_update_status(&status, json)?;
        return Ok(());
    }
    if let Some(ref v) = version
        && semver::Version::parse(v).is_err()
    {
        anyhow::bail!(
            "'{}' is not a valid version. Expected semver like 0.1.150",
            v
        );
    }
    let telemetry_cfg = distill_shell::config::load_agent_config_disk_only()
        .map_err(|e| tracing::warn!("grok update: telemetry init skipped (agent config: {e})"))
        .ok();
    if let Some(agent_cfg) = telemetry_cfg {
        let auth_manager =
            std::sync::Arc::new(distill_login::AuthManager::new_with_proxy_base_url(
                &distill_shell::util::distill_home::distill_home(),
                agent_cfg.grok_com_config.clone(),
                agent_cfg.endpoints.proxy_url(),
            ));
        distill_shell::agent::init::update_telemetry_config(&agent_cfg, &auth_manager);
    }
    let result = auto_update::run_update(
        force_reinstall,
        version.as_deref(),
        channel_switch,
        &mut update_config,
        trigger,
    )
    .await;
    if let Ok(Some(installed_version)) = &result {
        signal_leaders_to_relaunch(installed_version).await;
    }
    distill_telemetry::session_ctx::drain_pending(distill_telemetry::session_ctx::CLI_DRAIN).await;
    result?;
    Ok(())
}
/// After a successful `distill update`, ask any running leader on this machine that is older than `installed_version`
/// to relaunch onto the new binary. Best-effort and non-fatal: discovery/connect/control failures are logged and
/// skipped.
#[tracing::instrument(level = "debug", skip_all)]
async fn signal_leaders_to_relaunch(installed_version: &str) {
    for d in distill_shell::leader::discover_leaders().await {
        if d.classification != distill_shell::leader::LeaderDiscoveryState::Reachable {
            continue;
        }
        let Some(socket_path) = d.socket_path.clone() else {
            continue;
        };
        if let Some(ref live) = d.live_info
            && !leader_is_older_than(&live.leader_binary_version, installed_version)
        {
            continue;
        }
        let client = match distill_shell::leader::LeaderClient::connect(
            socket_path,
            "distill-update",
            ClientMode::Stdio,
            ClientCapabilities::default(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "Could not connect to leader to signal relaunch");
                continue;
            }
        };
        if !client.registration().supports_relaunch() {
            client.cancel();
            continue;
        }
        match client
            .send_control(ControlCommand::RelaunchForUpdate {
                to_version: installed_version.to_string(),
            })
            .await
        {
            Ok(Ok(distill_shell::leader::ControlPayload::Relaunching {
                from_version,
                to_version,
                ..
            })) => {
                eprintln!("  ↻ Relaunching shared session (leader {from_version} → {to_version})…");
            }
            Ok(Ok(distill_shell::leader::ControlPayload::RelaunchDeclined { reason })) => {
                tracing::debug!(%reason, "Leader declined relaunch");
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::debug!(error = %e.message, "Leader relaunch control error");
            }
            Err(e) => {
                tracing::debug!(error = %e, "Leader relaunch ack not received (leader may be exiting)");
            }
        }
        client.cancel();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn embedded_agent_commands_heal_managed_policy_before_sandboxing() {
        for args in [
            vec!["distill"],
            vec!["distill", "agent", "stdio"],
            vec!["distill", "dashboard"],
            vec!["distill", "models"],
            vec!["distill", "worktree", "list"],
        ] {
            let args = PagerArgs::try_parse_from(args).unwrap();
            assert!(
                command_needs_pre_sandbox_policy_heal(args.command.as_ref()),
                "{args:?}"
            );
        }
    }
    #[test]
    fn utility_commands_skip_managed_policy_heal() {
        for args in [
            vec!["distill", "inspect"],
            vec!["distill", "mcp", "list"],
            vec!["distill", "sessions", "list"],
            vec!["distill", "version"],
        ] {
            let args = PagerArgs::try_parse_from(args).unwrap();
            assert!(
                !command_needs_pre_sandbox_policy_heal(args.command.as_ref()),
                "{args:?}"
            );
        }
    }
    #[test]
    fn default_caps_the_core_count() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        assert_eq!(default_worker_threads(nz(360)), DEFAULT_MAX_WORKER_THREADS);
        assert_eq!(default_worker_threads(nz(4)), nz(4));
    }
    #[test]
    fn worker_threads_from_selects_default_or_override() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            worker_threads_from(None, cores),
            WorkerCount::Accepted(default_worker_threads(cores))
        );
        assert_eq!(
            worker_threads_from(Some("16"), cores),
            WorkerCount::Accepted(nz(16))
        );
    }
    #[test]
    fn override_in_range_is_used_without_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("16", cores),
            WorkerCount::Accepted(nz(16))
        );
        assert_eq!(resolve_worker_override("16", cores).notice(), None);
        assert_eq!(resolve_worker_override(" 8 ", cores).used().get(), 8);
        assert_eq!(
            resolve_worker_override("360", cores),
            WorkerCount::Accepted(cores)
        );
    }
    #[test]
    fn override_out_of_range_is_clamped_with_a_notice() {
        let nz = |n| NonZeroUsize::new(n).unwrap();
        let cores = nz(360);
        assert_eq!(
            resolve_worker_override("100000", cores),
            WorkerCount::Clamped {
                requested: 100000,
                used: cores,
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("0", cores),
            WorkerCount::Clamped {
                requested: 0,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("-1", cores),
            WorkerCount::Clamped {
                requested: -1,
                used: nz(1),
                cores
            }
        );
        assert_eq!(
            resolve_worker_override("100000", cores).notice().unwrap(),
            "distill: clamped GROK_WORKER_THREADS=100000 to 360 (valid range is 1..=360)"
        );
    }
    #[test]
    fn override_unparseable_is_ignored_with_a_notice() {
        let cores = NonZeroUsize::new(360).unwrap();
        for value in ["abc", "", "99999999999999999999999999999999999999999"] {
            let ignored = resolve_worker_override(value, cores);
            assert!(matches!(ignored, WorkerCount::Ignored { .. }), "{value}");
            assert_eq!(ignored.used(), default_worker_threads(cores), "{value}");
        }
        assert_eq!(
            resolve_worker_override("abc", cores).notice().unwrap(),
            "distill: ignoring GROK_WORKER_THREADS=\"abc\" (not a valid integer)"
        );
    }
    #[test]
    fn version_output_writer_preserves_channel_aware_contract() {
        distill_version::set_full_version(env!("VERSION_WITH_COMMIT"));
        for (label, expected_suffix) in [
            (" [alpha]", " [alpha]\n"),
            (" [stable]", " [stable]\n"),
            ("", ")\n"),
        ] {
            let mut output = Vec::new();
            write_version(&mut output, label).unwrap();
            let output = String::from_utf8(output).unwrap();
            assert!(output.starts_with("distill "));
            assert!(output.contains(env!("VERSION_WITH_COMMIT")));
            assert!(output.ends_with(expected_suffix), "{output:?}");
        }
    }
    #[test]
    fn version_flags_and_doctor_are_distinct_early_intents() {
        let version = PagerArgs::try_parse_from(["distill", "--version"]).unwrap();
        assert!(version.version);
        assert!(version.command.is_none());
        let short = PagerArgs::try_parse_from(["distill", "-v"]).unwrap();
        assert!(short.version);
        assert!(short.command.is_none());
        let subcommand = PagerArgs::try_parse_from(["distill", "version"]).unwrap();
        assert!(!subcommand.version);
        assert!(matches!(
            subcommand.command,
            Some(Command::Version { json: false })
        ));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    struct TempHeapDump(std::path::PathBuf);
    #[cfg(all(feature = "jemalloc", unix))]
    impl TempHeapDump {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "grok-jemalloc-{label}-{}-{}.heap",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
        fn assert_nonempty_dump(&self) {
            let meta = std::fs::metadata(&self.0).expect("dump file missing after prof.dump");
            assert!(meta.len() > 0, "empty dump file");
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for TempHeapDump {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn require_opt_prof() -> bool {
        if jemalloc_prof_available() {
            return true;
        }
        eprintln!(
            "skip jemalloc prof checks: opt.prof false \
             (release-dist static conf, or MALLOC_CONF=prof:true,prof_active:false,lg_prof_sample={})",
            distill_shell::heap_profile::LG_PROF_SAMPLE
        );
        false
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_prof_active(expected: bool) {
        assert_eq!(jemalloc_read_prof_active(), Some(expected));
    }
    /// Restores process-global `prof.active` on drop (panic-safe for serial tests).
    #[cfg(all(feature = "jemalloc", unix))]
    struct ProfActiveGuard {
        previous: bool,
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl ProfActiveGuard {
        fn set(active: bool) -> Self {
            let previous = jemalloc_read_prof_active().unwrap_or(false);
            assert!(
                jemalloc_set_prof_active(active),
                "failed to set prof.active={active}"
            );
            Self { previous }
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    impl Drop for ProfActiveGuard {
        fn drop(&mut self) {
            let _ = jemalloc_set_prof_active(self.previous);
        }
    }
    #[cfg(all(feature = "jemalloc", unix))]
    fn assert_stats_sane(stats: distill_shell::heap_profile::JemallocStats) {
        assert!(stats.allocated > 0, "allocated={}", stats.allocated);
        assert!(stats.resident > 0, "resident={}", stats.resident);
        assert!(
            stats.resident >= stats.allocated,
            "resident {} < allocated {}",
            stats.resident,
            stats.allocated
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_stats_readable_after_epoch() {
        assert_stats_sane(jemalloc_heap_stats().expect("stats readable"));
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_prof_active_round_trip_and_dump() {
        if !require_opt_prof() {
            return;
        }
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
        }
        assert_prof_active(false);
        let dump = TempHeapDump::new("direct");
        jemalloc_dump_to_path(dump.path()).expect("prof.dump");
        dump.assert_nonempty_dump();
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn jemalloc_dump_rejects_interior_nul_path() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;
        if !require_opt_prof() {
            return;
        }
        let path = Path::new(OsStr::from_bytes(b"/tmp/grok-jemalloc-\0.heap"));
        let err = jemalloc_dump_to_path(path).expect_err("interior NUL must fail");
        assert!(
            err.to_ascii_lowercase().contains("nul"),
            "unexpected error: {err}"
        );
    }
    #[cfg(all(feature = "jemalloc", unix))]
    #[test]
    #[serial_test::serial(jemalloc_heap_profile)]
    fn install_heap_profile_hooks_wires_shell_apis() {
        install_heap_profile_hooks();
        assert_stats_sane(distill_shell::heap_profile::stats().expect("shell stats after install"));
        if !require_opt_prof() {
            assert!(!distill_shell::heap_profile::prof_available());
            return;
        }
        assert!(distill_shell::heap_profile::prof_available());
        assert_prof_active(false);
        {
            let _guard = ProfActiveGuard::set(true);
            assert_prof_active(true);
            assert!(distill_shell::heap_profile::set_prof_active(true));
            assert_prof_active(true);
        }
        assert_prof_active(false);
        assert!(distill_shell::heap_profile::set_prof_active(false));
        assert_prof_active(false);
        let dump = TempHeapDump::new("shell");
        distill_shell::heap_profile::dump_to_path(dump.path()).expect("shell dump");
        dump.assert_nonempty_dump();
    }
    #[cfg(unix)]
    #[test]
    fn is_managed_install_matches_only_the_bin_grok_target() {
        let home =
            std::env::temp_dir().join(format!("distill-managed-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("downloads")).unwrap();
        assert!(!is_managed_install(
            Some(home.join("bin").join("distill")),
            &home
        ));
        assert!(!is_managed_install(None, &home));
        assert!(!is_managed_install(
            Some(home.join("bin").join("distill")),
            std::path::Path::new("")
        ));
        let target = home.join("downloads").join("grok-1.2.3");
        std::fs::write(&target, b"binary").unwrap();
        std::os::unix::fs::symlink(&target, home.join("bin").join("distill")).unwrap();
        assert!(is_managed_install(
            Some(home.join("bin").join("distill")),
            &home
        ));
        assert!(is_managed_install(Some(target.clone()), &home));
        let pinned = home.join("bin").join("grok-9.9.9");
        std::fs::write(&pinned, b"binary").unwrap();
        assert!(!is_managed_install(Some(pinned), &home));
        let _ = std::fs::remove_dir_all(&home);
    }
    /// Pins the gate composition; a dropped conjunct fails its named case.
    #[test]
    fn stdio_auto_update_requires_direct_stdio_enabled_and_managed() {
        assert!(stdio_auto_update_enabled(true, false, true, true));
        assert!(
            !stdio_auto_update_enabled(true, true, true, true),
            "leader bridge"
        );
        assert!(
            !stdio_auto_update_enabled(false, false, true, true),
            "non-stdio"
        );
        assert!(
            !stdio_auto_update_enabled(true, false, false, true),
            "updates off"
        );
        assert!(
            !stdio_auto_update_enabled(true, false, true, false),
            "pinned binary"
        );
    }
    use clap::Parser as _;
    /// `distill dashboard` flags the startup hook without forcing leader mode.
    /// The dashboard is independent of leader mode, so the launch keeps whatever leader setting the user (or config) chose.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_flags_startup_without_forcing_leader() {
        let mut args = PagerArgs::try_parse_from(["distill", "dashboard"]).unwrap();
        assert!(!args.leader, "fixture: no explicit --leader");
        flag_dashboard_at_startup_if_requested(&mut args).unwrap();
        assert!(!args.leader, "dashboard must NOT force leader mode");
        assert!(
            args.command.is_none(),
            "soft subcommand must be consumed so the interactive path runs",
        );
        assert_eq!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref(),
            Ok("1"),
            "startup hook flag must be set",
        );
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
    }
    /// `distill dashboard --no-leader` is allowed.
    /// The dashboard does not require a leader, so the combination launches into the dashboard in non-leader mode.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_allows_no_leader() {
        let mut args = PagerArgs::try_parse_from(["distill", "--no-leader", "dashboard"]).unwrap();
        flag_dashboard_at_startup_if_requested(&mut args)
            .expect("--no-leader + dashboard must be allowed");
        assert!(args.no_leader, "--no-leader must be preserved");
        assert!(!args.leader, "dashboard must not force leader mode");
        assert!(
            args.command.is_none(),
            "soft subcommand must be consumed so the interactive path runs",
        );
        assert_eq!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").as_deref(),
            Ok("1"),
            "startup hook flag must be set",
        );
        unsafe { std::env::remove_var("GROK_OPEN_DASHBOARD_AT_STARTUP") };
    }
    /// `GROK_AGENT_DASHBOARD=0` disables the feature; the subcommand must error visibly before the TUI starts.
    #[serial_test::serial(GROK_AGENT_DASHBOARD)]
    #[test]
    fn dashboard_subcommand_errors_when_disabled() {
        unsafe { std::env::set_var("GROK_AGENT_DASHBOARD", "0") };
        let mut args = PagerArgs::try_parse_from(["distill", "dashboard"]).unwrap();
        let result = flag_dashboard_at_startup_if_requested(&mut args);
        unsafe { std::env::remove_var("GROK_AGENT_DASHBOARD") };
        let err = result.expect_err("disabled dashboard must error");
        assert!(err.to_string().contains("disabled"), "got: {err}");
        assert!(
            std::env::var("GROK_OPEN_DASHBOARD_AT_STARTUP").is_err(),
            "failure path must not flag the startup hook",
        );
    }
    #[test]
    fn workspace_command_gate_resolution() {
        use distill_shell::util::config::RemoteSettings;
        let on = RemoteSettings {
            workspace_command_enabled: Some(true),
            ..RemoteSettings::default()
        };
        let off = RemoteSettings::default();
        assert_eq!(
            workspace_command_gate(None, Some(&on)),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(None, Some(&off)),
            WorkspaceGate::Disabled
        );
        assert_eq!(workspace_command_gate(None, None), WorkspaceGate::Unknown);
        assert_eq!(
            workspace_command_gate(Some(true), Some(&off)),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(Some(true), None),
            WorkspaceGate::Enabled
        );
        assert_eq!(
            workspace_command_gate(Some(false), Some(&on)),
            WorkspaceGate::Disabled
        );
        assert_eq!(
            workspace_command_gate(Some(false), None),
            WorkspaceGate::Disabled
        );
    }
    #[serial_test::serial(GROK_WORKSPACE_COMMAND)]
    #[test]
    fn workspace_command_env_override_parsing() {
        unsafe { std::env::remove_var("GROK_WORKSPACE_COMMAND") };
        assert_eq!(workspace_command_env_override(), None);
        unsafe { std::env::set_var("GROK_WORKSPACE_COMMAND", "1") };
        assert_eq!(workspace_command_env_override(), Some(true));
        unsafe { std::env::set_var("GROK_WORKSPACE_COMMAND", "off") };
        assert_eq!(workspace_command_env_override(), Some(false));
        unsafe { std::env::remove_var("GROK_WORKSPACE_COMMAND") };
    }
    fn make_state() -> std::sync::Mutex<StdioReplayState> {
        std::sync::Mutex::new(StdioReplayState::default())
    }
    #[test]
    fn cache_initialize_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.initialize_json.as_deref(), Some(msg));
    }
    #[test]
    fn cache_session_load_preserves_full_request() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","mcpServers":[]}}"#;
        cache_outgoing_acp_state(msg, &state);
        let s = state.lock().unwrap();
        let Some((sid, cached)) = s.sessions.first() else {
            panic!("expected a cached session, got {}", s.sessions.len());
        };
        assert_eq!(sid, "s1");
        assert_eq!(cached.load_request_json.as_deref(), Some(msg));
        assert_eq!(cached.cwd.as_deref(), Some("/tmp"));
        assert!(cached.mcp_servers_json.is_some());
        assert_eq!(s.last_session_id.as_deref(), Some("s1"));
    }
    #[test]
    fn cache_session_new_is_pending_until_response_assigns_id() {
        let state = make_state();
        let load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#;
        cache_outgoing_acp_state(load, &state);
        let new = r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/home"}}"#;
        cache_outgoing_acp_state(new, &state);
        {
            let s = state.lock().unwrap();
            assert_eq!(s.sessions.len(), 1);
            assert_eq!(s.sessions.first().map(|(id, _)| id.as_str()), Some("s1"));
            assert!(s.pending_new.is_some());
            assert_eq!(
                s.pending_new.as_ref().unwrap().cwd.as_deref(),
                Some("/home")
            );
        }
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"s2"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.pending_new.is_none());
        assert_eq!(s.sessions.len(), 2);
        assert_eq!(s.sessions.get(1).map(|(id, _)| id.as_str()), Some("s2"));
        assert_eq!(
            s.sessions.get(1).and_then(|(_, c)| c.cwd.as_deref()),
            Some("/home")
        );
        assert_eq!(s.last_session_id.as_deref(), Some("s2"));
    }
    #[test]
    fn cache_session_close_stops_replaying_it() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"_x.ai/session/close","params":{"sessionId":"s1"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.sessions.is_empty(), "closed session must not be replayed");
        assert!(s.last_session_id.is_none());
    }
    /// The standard close spelling must stop the replay exactly like the `x.ai/` extension spelling.
    /// Adopting `session/close` without teaching the cache would resurrect closed sessions on every leader reconnect.
    #[test]
    fn cache_standard_session_close_stops_replaying_it() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/close","params":{"sessionId":"s1"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert!(s.sessions.is_empty(), "closed session must not be replayed");
        assert!(s.last_session_id.is_none());
    }
    /// A session only ever resumed must survive a leader restart: the cache synthesizes a load entry, since a new leader has no turn to reattach to.
    #[test]
    fn cache_session_resume_registers_unknown_sessions_for_replay() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"s1","cwd":"/proj"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        let Some((sid, cached)) = s.sessions.first() else {
            panic!("expected a cached session, got {}", s.sessions.len());
        };
        assert_eq!(sid, "s1");
        assert_eq!(
            cached.load_request_json, None,
            "a resume must not be replayed verbatim; the replay synthesizes a load"
        );
        assert_eq!(cached.cwd.as_deref(), Some("/proj"));
        assert_eq!(s.last_session_id.as_deref(), Some("s1"));
    }
    /// A resume must not displace the original load's entry: that entry carries the client's `_meta`, which a synthesized load cannot reproduce.
    #[test]
    fn cache_session_resume_does_not_displace_the_original_load() {
        let state = make_state();
        let load = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp","_meta":{"noReplay":true}}}"#;
        cache_outgoing_acp_state(load, &state);
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state,
        );
        let s = state.lock().unwrap();
        assert_eq!(s.sessions.len(), 1, "one session, one replay entry");
        assert_eq!(
            s.sessions
                .first()
                .and_then(|(_, c)| c.load_request_json.as_deref()),
            Some(load),
            "the original load, with its _meta, must survive the resume"
        );
    }
    /// An UNCONFIRMED `session/new` (leader died before its response) must not be replayed, since its id was never assigned.
    /// Previously loaded sessions still restore.
    #[tokio::test]
    async fn replay_after_unconfirmed_session_new_restores_prior_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        let load_a = r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"session-A","cwd":"/old"}}"#;
        cache_outgoing_acp_state(load_a, &state);
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/new"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            assert!(load.contains("session-A"), "unexpected replay: {load}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(
            result.as_deref(),
            Some("session-A"),
            "prior session must be restored even though the new one was unconfirmed"
        );
        responder.await.unwrap();
    }
    #[test]
    fn fallback_replay_json_escapes_special_chars() {
        let cached = CachedSession {
            load_request_json: None,
            cwd: Some(r#"C:\Users\test path"#.into()),
            mcp_servers_json: None,
        };
        let json = replay_load_json(r#"session"with"quotes"#, &cached)
            .expect("cwd present → load synthesized");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("fallback replay JSON must be valid");
        assert_eq!(
            parsed
                .get("params")
                .and_then(|p| p.get("sessionId"))
                .and_then(|v| v.as_str())
                .unwrap(),
            r#"session"with"quotes"#
        );
        assert_eq!(
            parsed
                .get("params")
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str())
                .unwrap(),
            r#"C:\Users\test path"#
        );
        assert_eq!(
            parsed.get("id").and_then(|v| v.as_str()),
            Some(REPLAY_LOAD_REQUEST_ID)
        );
    }
    #[test]
    fn cache_incoming_session_id_from_response() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state,
        );
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"abc123"}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert_eq!(s.last_session_id.as_deref(), Some("abc123"));
        assert_eq!(s.sessions.len(), 1);
        assert_eq!(
            s.sessions.first().map(|(id, _)| id.as_str()),
            Some("abc123")
        );
    }
    /// A multi-session client (IDE driving several sessions over one bridge)
    /// gets EVERY session replayed after a reconnect, in first-seen order.
    #[tokio::test]
    async fn replay_restores_all_cached_sessions() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-1","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd":"/b"}}"#,
            &state,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":3,"result":{"sessionId":"sess-2"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load1 = leader_rx.recv().await.unwrap();
            assert!(load1.contains("sess-1"), "expected sess-1 first: {load1}");
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#.to_string())
                .unwrap();
            let load2 = leader_rx.recv().await.unwrap();
            assert!(load2.contains("sess-2"), "expected sess-2 second: {load2}");
            let load2_json: serde_json::Value = serde_json::from_str(&load2).unwrap();
            assert_eq!(
                load2_json.get("id").and_then(|v| v.as_str()),
                Some(REPLAY_LOAD_REQUEST_ID)
            );
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-2"));
        responder.await.unwrap();
    }
    /// One broken session must not doom the rest: a rejected load is skipped and the remaining sessions still restore.
    #[tokio::test]
    async fn replay_skips_rejected_session_and_restores_the_rest() {
        let state = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"sess-bad","cwd":"/a"}}"#,
            &state,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":3,"method":"session/load","params":{"sessionId":"sess-good","cwd":"/b"}}"#,
            &state,
        );
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _bad = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
            let good = leader_rx.recv().await.unwrap();
            assert!(good.contains("sess-good"));
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#.to_string())
                .unwrap();
        });
        let s = state.lock().unwrap().clone();
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &s).await;
        assert_eq!(result.as_deref(), Some("sess-good"));
        responder.await.unwrap();
    }
    #[test]
    fn cache_incoming_ignores_non_session_response() {
        let state = make_state();
        let msg = r#"{"jsonrpc":"2.0","id":1,"result":{"models":[]}}"#;
        cache_incoming_session_id(msg, &state);
        let s = state.lock().unwrap();
        assert!(s.last_session_id.is_none());
        assert!(s.sessions.is_empty());
    }
    #[tokio::test]
    async fn replay_with_no_cached_state_returns_none() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let state = StdioReplayState::default();
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&tx, &mut rx, &mut sink, &state).await;
        assert!(result.is_none());
    }
    #[tokio::test]
    async fn replay_sends_initialize_and_session_load() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"s1","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1"}}"#.to_string())
                .unwrap();
        });
        let mut rx = response_rx;
        let mut sink = Vec::new();
        let result = replay_acp_state_after_reconnect(&leader_tx, &mut rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s1"));
        assert!(
            sink.is_empty(),
            "responses to replayed requests must be swallowed, not forwarded"
        );
        responder.await.unwrap();
    }
    /// Regression test for the "unknown session id" bug after a leader crash. The replay must instead. swallow only the
    /// responses to the replayed requests.
    #[tokio::test]
    async fn replay_waits_for_load_response_through_notifications() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":8,"method":"session/load","params":{"sessionId":"s9","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","method":"x.ai/leader/version_mismatch","params":{}}"#
                        .to_string(),
                )
                .unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            for i in 0..3 {
                response_tx
                    .send(
                        format!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s9","n":{i}}}}}"#
                    ),
                    )
                    .unwrap();
            }
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":8,"result":{}}"#.to_string())
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(
            result.as_deref(),
            Some("s9"),
            "replay must succeed once the load response arrives"
        );
        let forwarded = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = forwarded.lines().collect();
        assert_eq!(
            lines.len(),
            4,
            "expected exactly the 4 notifications, got: {lines:?}"
        );
        let [l0, l1, l2, l3] = lines.as_slice() else {
            panic!("expected 4 forwarded lines: {lines:?}");
        };
        assert!(l0.contains("version_mismatch"));
        assert!(l1.contains(r#""n":0"#));
        assert!(l2.contains(r#""n":1"#));
        assert!(l3.contains(r#""n":2"#));
        assert!(!forwarded.contains(r#""id":7"#), "init response leaked");
        assert!(!forwarded.contains(r#""id":8"#), "load response leaked");
        responder.await.unwrap();
    }
    /// A `session/load` rejected by the new leader (error response) must surface as a failed replay (`None`).
    /// The bridge then emits `x.ai/leader_reconnected` with empty params and the external client knows to re-establish state itself.
    #[tokio::test]
    async fn replay_returns_none_when_load_is_rejected() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"gone","cwd":"/tmp"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let _load = leader_rx.recv().await.unwrap();
            response_tx
                .send(
                    r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"Invalid params","data":"unknown session id"}}"#
                        .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert!(result.is_none(), "rejected load must not claim success");
        responder.await.unwrap();
    }
    /// The synthetic fallback `session/load` (client only ever sent `session/new`) uses a string request id.
    /// That id cannot collide with the external client's numeric ids, and the response matcher honors it.
    #[tokio::test]
    async fn replay_fallback_load_uses_reserved_string_id() {
        let (leader_tx, mut leader_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (response_tx, mut response_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let state_mutex = make_state();
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &state_mutex,
        );
        cache_outgoing_acp_state(
            r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp"}}"#,
            &state_mutex,
        );
        cache_incoming_session_id(
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s-new"}}"#,
            &state_mutex,
        );
        let state = state_mutex.lock().unwrap().clone();
        let responder = tokio::spawn(async move {
            let _init = leader_rx.recv().await.unwrap();
            response_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string())
                .unwrap();
            let load = leader_rx.recv().await.unwrap();
            let load_json: serde_json::Value = serde_json::from_str(&load).unwrap();
            assert_eq!(
                load_json.get("id").and_then(|v| v.as_str()),
                Some(REPLAY_LOAD_REQUEST_ID),
                "fallback load must use the reserved string id"
            );
            response_tx
                .send(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": REPLAY_LOAD_REQUEST_ID,
                        "result": {}
                    })
                    .to_string(),
                )
                .unwrap();
        });
        let mut sink = Vec::new();
        let result =
            replay_acp_state_after_reconnect(&leader_tx, &mut response_rx, &mut sink, &state).await;
        assert_eq!(result.as_deref(), Some("s-new"));
        responder.await.unwrap();
    }
    fn multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime")
    }
    #[test]
    fn run_and_shutdown_is_fast_without_blocking_work() {
        use std::time::{Duration, Instant};
        let runtime = multi_thread_runtime();
        let grace = Duration::from_secs(5);
        let start = Instant::now();
        let out = run_and_shutdown(runtime, async { 42_u32 }, grace);
        let elapsed = start.elapsed();
        assert_eq!(out, 42, "must pass the future's output through");
        assert!(
            elapsed < grace,
            "clean teardown took {elapsed:?}; grace must be a ceiling, not a floor",
        );
    }
}
