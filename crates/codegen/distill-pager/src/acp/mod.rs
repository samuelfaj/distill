// Modified for Distill by Samuel Fajreldines, 2026.
//! ACP (Agent Communication Protocol) connection management.
//!
//! This module spawns the agent process, initializes the protocol, authenticates, and provides the channel for communication.
/// Shared plan-file size cap for the pager keep.
pub(crate) const MAX_PLAN_FILE_BYTES: usize = 256 * 1024;
pub mod leader_bridge;
pub mod meta;
pub mod model_state;
pub mod spawn;
pub(crate) mod subagent_label_registry;
mod subagent_message;
pub mod tracker;
mod version_mismatch;
pub(crate) use version_mismatch::{is_version_mismatch_banner, version_mismatch_banner};
/// Ext methods that carry a session-scoped update and may stamp `isReplay`.
/// TUI dispatch, headless dispatch, and the session-load ACP barrier all share this list.
/// A new method thus cannot be handled in one path and classified `Unrelated` in another.
pub(crate) fn is_session_update_ext_method(method: &str) -> bool {
    matches!(method, "x.ai/session_notification" | "x.ai/session/update")
}
use crate::client_identity::{HEADLESS_CLIENT_TYPE, PAGER_CLIENT_TYPE, PAGER_CLIENT_VERSION};
use agent_client_protocol as acp;
use anyhow::Result;
use distill_acp_lib::{AcpAgentTx, AcpClientRx, acp_send};
use distill_shell::agent::auth_method::AuthMethodKind;
use distill_shell::agent::config::Config as AgentConfig;
use distill_shell::sampling::types::ReasoningEffort;
use distill_telemetry::process_info::{
    Entrypoint, Interactivity, LeaderMode, ProcessIdentity, set_identity,
};
use distill_telemetry::startup;
pub use distill_telemetry::startup::{
    AgentKind, Owner, StartupOutcome, StartupPhase, StartupTimer,
};
pub use model_state::ModelState;
use std::io::Write;
use tokio_util::sync::CancellationToken;
/// Construct a `METHOD_NOT_FOUND` error for `WaitForTerminalExit`.
/// Both the interactive pager and headless mode reject this ACP method (the adapter falls back to polling).
/// Centralised here so the error code and message format stay in sync.
pub(crate) fn wait_for_exit_not_supported(context: &str) -> acp::Error {
    acp::Error::new(
        acp::ErrorCode::MethodNotFound.into(),
        format!("{context} does not handle WaitForTerminalExit"),
    )
}
/// Initial auth mode hint from the agent's auth method metadata.
/// Determined at startup from `AuthMethod.meta.external_provider`.
/// Used by the welcome screen to decide whether to show a browser-opening message or a manual token paste input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStartMode {
    /// Mode not yet known (will be resolved after AuthenticateRequest).
    Pending,
    /// External provider (meta.external_provider == true); the browser opens automatically.
    Command,
}
/// The pager's connection to an agent that is running but has not been set up yet.
/// It holds the channel for sending requests to the agent, the channel for receiving its replies, the token that stops the agent, and the place the agent runs.
/// `initialize_connection` sends `initialize` over these channels and returns the finished `AcpConnection`.
pub(in crate::acp) struct AgentEndpoint {
    pub(in crate::acp) tx: AcpAgentTx,
    pub(in crate::acp) rx: AcpClientRx,
    pub(in crate::acp) cancel: CancellationToken,
    pub(in crate::acp) location: AgentLocation,
}
/// Where an agent runs, and the one handle the pager keeps for it.
pub(in crate::acp) enum AgentLocation {
    /// On a thread in this process. Exit joins the thread so the agent can finish flushing.
    Thread(std::thread::JoinHandle<Result<()>>),
    /// In the leader process. The receiver reports socket reconnects to the event loop.
    Leader(tokio::sync::watch::Receiver<leader_bridge::ConnectionStatus>),
}
impl From<spawn::SpawnedAgent> for AgentEndpoint {
    fn from(spawned: spawn::SpawnedAgent) -> Self {
        AgentEndpoint {
            tx: spawned.channel.tx,
            rx: spawned.channel.rx,
            cancel: spawned.cancel,
            location: AgentLocation::Thread(spawned.thread_handle),
        }
    }
}
/// Result of connecting to an agent.
pub struct AcpConnection {
    /// Send requests to the agent.
    pub tx: AcpAgentTx,
    /// Receive notifications from the agent.
    pub rx: AcpClientRx,
    /// Available models and current selection.
    pub models: ModelState,
    /// Whether the agent is a grok-shell instance.
    pub is_grok_shell: bool,
    /// Auth methods advertised by the agent.
    pub auth_methods: Vec<acp::AuthMethod>,
    /// Cancellation token to stop the agent.
    pub cancel: CancellationToken,
    /// In-process agent worker thread (`connect` only). Join after cancel so session actors can flush SessionEnd hooks.
    /// `None` in leader mode.
    pub agent_thread: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    /// ACP-advertised slash commands parsed from `InitializeResponse.meta.availableCommands`.
    /// Seeded into every new `AgentSession` so autocomplete has shell builtins and skills immediately, before any `AvailableCommandsUpdate` arrives.
    pub available_commands: Vec<acp::AvailableCommand>,
    pub needs_login: bool,
    /// Login button label from `AuthMethod.name` (e.g., "grok.com", "Acme Corp").
    pub login_label: Option<String>,
    /// The auth method ID to use for login (copied from the first advertised method).
    pub login_method_id: Option<acp::AuthMethodId>,
    /// Initial auth mode hint (Command vs Pending) from method metadata.
    pub auth_start_mode: AuthStartMode,
    /// Auth response metadata from eager authentication (cached token or API key).
    /// Contains `team_name`, etc. `None` when interactive login is required.
    pub auth_meta: Option<serde_json::Value>,
    /// Leader connection status. `Some` only when connected via leader.
    pub leader_status_rx: Option<tokio::sync::watch::Receiver<leader_bridge::ConnectionStatus>>,
    /// Whether cancel-rewind is enabled (resolved by shell from config layers).
    pub cancel_rewind_enabled: bool,
    /// Whether the session-recap feature is rolled out for this connection. The client gates its automatic away-recap
    /// poll and the manual `/recap` on this so a disabled feature produces zero `x.ai/recap` traffic.
    pub session_recap_available: bool,
    /// Shell-side feedback trace-offer eligibility (see `feedbackTraceOffer`).
    pub feedback_trace_offer: bool,
    /// `AuthManager` for pager-side authenticated channels (voice STT and TTS).
    /// In-process mode shares the agent's instance (single token cache); leader mode builds a dedicated one off the same local `auth.json`.
    /// Either way it resolves a fresh bearer per request via the refresh chain.
    pub auth_manager: std::sync::Arc<distill_login::AuthManager>,
}
/// CLI flags that affect agent configuration, threaded from PagerArgs.
#[derive(Debug, Clone, Default)]
pub struct ConnectFlags {
    pub subagents: bool,
    /// CLI memory override set by a legacy compatibility flag.
    pub memory_enabled_override: Option<bool>,
    /// Original compatibility flag spelling for leader-mode warnings.
    pub memory_override_flag: Option<&'static str>,
    pub disable_web_search: bool,
    /// Session-scoped `--todo-gate` override. Forces `ReminderPolicy.todo_gate.enabled = true` for this session.
    pub todo_gate: bool,
    /// Session-scoped `--laziness-debug-log <path>` override. Observation-only (no nudges). Prototype and eval use
    /// only; not persisted to config.toml.
    pub laziness_debug_log: Option<std::path::PathBuf>,
    /// Storage mode override.
    pub storage_mode: Option<String>,
    /// Whether this client will draw a status row, advertised as `x.ai/statusLine` so the agent can skip an unpainted payload.
    pub status_line: bool,
    /// Client identifier for ACP Initialize metadata.
    pub client_identifier: Option<String>,
    /// Hunk tracker mode for ACP Initialize capabilities.
    pub hunk_tracker_mode: Option<String>,
    /// Terminal capability in ACP Initialize.
    pub terminal: bool,
    /// Filesystem read capability in ACP Initialize.
    pub fs_read: bool,
    /// Filesystem write capability in ACP Initialize.
    pub fs_write: bool,
    /// Installer field for config.toml.
    pub installer: Option<String>,
    /// Remote settings from early prefetch (used for memory config resolution).
    pub remote_settings: Option<distill_shell::util::config::RemoteSettings>,
    /// Override the entire system prompt.
    pub system_prompt_override: Option<String>,
    /// Extra rules appended to the system prompt (from `--rules`).
    pub rules: Option<String>,
    /// Override reasoning effort for all models.
    pub reasoning_effort_override: Option<ReasoningEffort>,
    /// CLI permission rules from the --allow and --deny flags.
    /// Not supported in leader mode (agent config is set at leader startup).
    pub permission_rules: Vec<distill_workspace::permission::types::PermissionRule>,
    /// Seed agent sessions with always-approve (YOLO) permission mode.
    pub default_yolo_mode: bool,
    /// Seed agent sessions with auto (classifier) permission mode.
    /// Ignored when `default_yolo_mode` is true.
    pub default_auto_mode: bool,
}
/// Connect to an agent: spawn, initialize, authenticate.
pub async fn connect(cancel: &CancellationToken, flags: ConnectFlags) -> Result<AcpConnection> {
    startup::enter(StartupPhase::ConfigLoad);
    let raw_config = {
        let _t = distill_telemetry::instrumentation::timer("startup.config_load.merge_layers");
        distill_shell::config::load_effective_config()
            .map_err(|e| anyhow::anyhow!("Failed to load config: {}", e))?
    };
    let mut agent_config = {
        let _t = distill_telemetry::instrumentation::timer("startup.config_load.parse");
        AgentConfig::new_from_toml_cfg(&raw_config)
            .map_err(|e| anyhow::anyhow!("Failed to create agent config: {}", e))?
    };
    {
        let _t = distill_telemetry::instrumentation::timer("startup.config_load.resolve");
        agent_config.resolve_runtime_fields(
            &distill_shell::agent::config::RuntimeResolutionContext {
                raw_config: &raw_config,
                remote_settings: flags.remote_settings.as_ref(),
                is_headless: false,
                cli_subagents: Some(flags.subagents),
                cli_web_search_model: None,
                cli_session_summary_model: None,
                memory_enabled_override: flags.memory_enabled_override,
                disable_web_search: flags.disable_web_search,
                todo_gate: flags.todo_gate,
                laziness_debug_log: flags.laziness_debug_log.as_deref(),
                storage_mode: flags.storage_mode.as_deref(),
            },
        );
    }
    agent_config.default_yolo_mode = flags.default_yolo_mode;
    agent_config.default_auto_mode = flags.default_auto_mode && !flags.default_yolo_mode;
    if let Some(effort) = flags.reasoning_effort_override {
        agent_config.reasoning_effort_override = Some(effort);
    }
    if !flags.permission_rules.is_empty() {
        agent_config.cli_agent_overrides.permission_rules = flags.permission_rules.clone();
    }
    apply_config_writes(&flags);
    set_identity(ProcessIdentity {
        entrypoint: Entrypoint::Embedded,
        leader: LeaderMode::Standalone,
        interactivity: Interactivity::Interactive,
    });
    let memory_config = agent_config.memory_config.clone();
    let spawned = spawn::spawn_grok_shell(agent_config, cancel, memory_config).await?;
    let auth_manager = spawned.auth_manager.clone();
    initialize_connection(AgentEndpoint::from(spawned), &flags, auth_manager).await
}
/// Finishes connecting to an agent. Sends it `initialize`, works out whether the user must log in first, and returns the `AcpConnection`.
/// Every agent target is connected through this function, so these steps are written once.
pub(in crate::acp) async fn initialize_connection(
    endpoint: AgentEndpoint,
    flags: &ConnectFlags,
    auth_manager: std::sync::Arc<distill_login::AuthManager>,
) -> Result<AcpConnection> {
    let AgentEndpoint {
        tx,
        rx,
        cancel,
        location,
    } = endpoint;
    let (agent_thread, leader_status_rx) = match location {
        AgentLocation::Thread(thread) => (Some(thread), None),
        AgentLocation::Leader(status_rx) => (None, Some(status_rx)),
    };
    startup::enter(StartupPhase::AcpInitialize);
    let agent = initialize(&tx, flags).await?;
    let (needs_login, login_label, login_method_id, auth_start_mode) =
        startup_auth_metadata(&agent.auth_methods);
    startup::enter(StartupPhase::EagerAuth);
    let (needs_login, login_label, login_method_id, auth_start_mode, auth_meta) =
        bounded_eager_auth(
            &tx,
            &agent.auth_methods,
            agent.default_auth_method_id.as_ref(),
            needs_login,
            login_label,
            login_method_id,
            auth_start_mode,
        )
        .await;
    Ok(AcpConnection {
        tx,
        rx,
        models: agent.models,
        is_grok_shell: agent.is_grok_shell,
        auth_methods: agent.auth_methods,
        cancel,
        agent_thread,
        available_commands: agent.available_commands,
        needs_login,
        login_label,
        login_method_id,
        auth_start_mode,
        auth_meta,
        leader_status_rx,
        cancel_rewind_enabled: agent.cancel_rewind_enabled,
        session_recap_available: agent.session_recap_available,
        feedback_trace_offer: agent.feedback_trace_offer,
        auth_manager,
    })
}
/// Connect to a leader process and return an `AcpConnection`. The leader provides the ACP transport via IPC (raw
/// JSON strings over a Unix socket). This function bridges that transport into the same typed `(AcpAgentTx,
/// AcpClientRx)` pair that `connect()` produces. It then runs the standard initialize and authenticate sequence.
pub async fn connect_via_leader(
    cancel: &CancellationToken,
    flags: ConnectFlags,
    raw_config: &toml::Value,
) -> Result<AcpConnection> {
    use distill_shell::leader::{
        ClientCapabilities, ClientMode, LeaderReconnector, ReconnectPolicy, connect_or_spawn,
    };
    warn_ignored_flags(
        &unsupported_leader_flags(&flags),
        "in leader mode (agent config is set at leader startup)",
    );
    apply_config_writes(&flags);
    startup::enter(StartupPhase::ConfigLoad);
    startup::set_auth_mode(distill_shell::managed_config::classify_auth_mode());
    let mut agent_config = AgentConfig::new_from_toml_cfg(raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;
    agent_config.remote_settings = flags.remote_settings.clone();
    let client_type = flags
        .client_identifier
        .as_deref()
        .unwrap_or(HEADLESS_CLIENT_TYPE);
    let env_urls = distill_shell::leader::LeaderEnvUrls::from(&agent_config.grok_com_config);
    let capabilities = ClientCapabilities {
        yolo_mode: flags.default_yolo_mode,
        auto_mode: flags.default_auto_mode && !flags.default_yolo_mode,
        default_model: agent_config.models.default.clone(),
        client_version: Some(PAGER_CLIENT_VERSION.to_string()),
        code_nav_enabled: false,
        terminal: flags.terminal,
        fs_read: flags.fs_read,
        fs_write: flags.fs_write,
        status_line: flags.status_line,
        user_message_echo: true,
    };
    startup::enter(StartupPhase::LeaderConnect);
    let conn = {
        let _t = distill_telemetry::instrumentation::timer("startup.leader_connect.connect");
        connect_or_spawn(
            client_type,
            ClientMode::Stdio,
            &env_urls,
            capabilities.clone(),
        )
        .await?
    };
    let (status_tx, status_rx) = LeaderReconnector::status_channel();
    let reconnector = LeaderReconnector::new(
        client_type,
        ClientMode::Stdio,
        env_urls,
        capabilities,
        status_tx,
    );
    let bridge = leader_bridge::bridge_leader_connection(
        conn,
        cancel.clone(),
        Some(reconnector),
        ReconnectPolicy::unbounded(),
    )?;
    let auth_manager = std::sync::Arc::new(distill_login::AuthManager::new_with_proxy_base_url(
        &distill_shell::util::distill_home::distill_home(),
        agent_config.grok_com_config.clone(),
        agent_config.endpoints.proxy_url(),
    ));
    set_identity(ProcessIdentity {
        entrypoint: Entrypoint::Pager,
        leader: LeaderMode::Attached,
        interactivity: Interactivity::Interactive,
    });
    distill_shell::agent::init::update_telemetry_config(&agent_config, &auth_manager);
    let endpoint = AgentEndpoint {
        tx: bridge.channel.tx,
        rx: bridge.channel.rx,
        cancel: bridge.cancel,
        location: AgentLocation::Leader(status_rx),
    };
    initialize_connection(endpoint, &flags, auth_manager).await
}
/// Prints one warning per flag the chosen backend ignores. `reason` finishes the sentence "has no effect ...".
/// This runs before tracing is set up, and the pager has already redirected fd 2, so the saved terminal stderr is the sink.
pub(in crate::acp) fn warn_ignored_flags(flags: &[&'static str], reason: &str) {
    if flags.is_empty() {
        return;
    }
    distill_shell::util::with_locked_stderr(|stderr| {
        for flag in flags {
            let _ = writeln!(stderr, "warning: {flag} has no effect {reason}");
        }
    });
}
fn unsupported_leader_flags(flags: &ConnectFlags) -> Vec<&'static str> {
    let mut out = Vec::new();
    if let Some(flag) = flags.memory_override_flag {
        out.push(flag);
    }
    if flags.disable_web_search {
        out.push("--disable-web-search");
    }
    if flags.storage_mode.is_some() {
        out.push("--storage-mode");
    }
    if flags.subagents {
        out.push("--subagents");
    }
    if !flags.permission_rules.is_empty() {
        out.push("--allow/--deny permission rules");
    }
    out
}
/// Write config.toml fields based on CLI flags.
pub(super) fn apply_config_writes(flags: &ConnectFlags) {
    let config_path = distill_shell::util::distill_home::distill_home()
        .join(distill_config::USER_CONFIG_FILENAME);
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_default();
    let mut changed = false;
    if let Some(ref installer) = flags.installer {
        let cli = doc
            .entry("cli")
            .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        if let Some(tbl) = cli.as_table_mut() {
            tbl.insert("installer", toml_edit::value(installer.as_str()));
            changed = true;
        }
    }
    if changed {
        if let Some(parent) = config_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&config_path, doc.to_string()) {
            tracing::warn!(error = %e, "failed to write config.toml");
        }
    }
}
/// Build the per-session `_meta` for `InitializeRequest` (TUI and leader).
fn build_initialize_meta(flags: &ConnectFlags) -> serde_json::Value {
    let client_type = flags
        .client_identifier
        .as_deref()
        .unwrap_or(PAGER_CLIENT_TYPE);
    let mut meta = serde_json::json!({
        "clientType": client_type,
        "clientVersion": PAGER_CLIENT_VERSION,
    });
    if let Some(obj) = meta.as_object_mut() {
        if let Some(spo) = &flags.system_prompt_override {
            obj.insert(
                "systemPromptOverride".into(),
                serde_json::Value::String(spo.clone()),
            );
        }
        if let Some(rules) = &flags.rules {
            obj.insert("rules".into(), serde_json::Value::String(rules.clone()));
        }
    }
    meta
}
/// Build `client_capabilities.meta`.
/// The hunk-tracker mode is canonicalized at this connect read so the agent runs exactly what the settings modal displays.
fn client_capabilities_meta(flags: &ConnectFlags) -> serde_json::Value {
    let hunk_mode =
        crate::settings::canonical_hunk_tracker_mode(flags.hunk_tracker_mode.as_deref());
    let mut meta = serde_json::json!({
        "x.ai/incrementalBashOutput": true,
        "x.ai/hunkTracker": { "mode": hunk_mode },
        "x.ai/bashOutputNoColor": true,
        "x.ai/gitHeadChanged": true,
    });
    if let Some(obj) = meta.as_object_mut() {
        obj.insert(
            distill_shell::session::USER_MESSAGE_ECHO_CAPABILITY.into(),
            true.into(),
        );
        obj.insert(
            distill_status_line::STATUS_LINE_CAPABILITY.into(),
            flags.status_line.into(),
        );
    }
    meta
}
/// Parse `defaultAuthMethodId` from `InitializeResponse.meta`.
/// The agent is the source of truth for preferred-method selection (including `[auth] preferred_method`).
/// Clients must not re-derive api_key vs session.
pub fn parse_default_auth_method_id(meta: Option<&acp::Meta>) -> Option<acp::AuthMethodId> {
    meta.and_then(|m| m.get("defaultAuthMethodId"))
        .and_then(|v| v.as_str())
        .map(|s| acp::AuthMethodId::new(s.to_owned()))
}
/// The parsed `InitializeResponse`, shared by `connect` and `connect_via_leader`.
pub(crate) struct InitializedAgent {
    pub(crate) models: ModelState,
    pub(crate) is_grok_shell: bool,
    pub(crate) auth_methods: Vec<acp::AuthMethod>,
    pub(crate) default_auth_method_id: Option<acp::AuthMethodId>,
    pub(crate) available_commands: Vec<acp::AvailableCommand>,
    pub(crate) cancel_rewind_enabled: bool,
    pub(crate) session_recap_available: bool,
    pub(crate) feedback_trace_offer: bool,
}
/// Send InitializeRequest and parse the response.
async fn initialize(tx: &AcpAgentTx, flags: &ConnectFlags) -> Result<InitializedAgent> {
    let mut meta = build_initialize_meta(flags).as_object().cloned();
    crate::app::session_startup::stamp_phase_traceparent(&mut meta);
    let req = acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new()
                    .read_text_file(flags.fs_read)
                    .write_text_file(flags.fs_write))
                .terminal(flags.terminal)
                .meta(client_capabilities_meta(flags).as_object().cloned()),
        )
        .meta(meta);
    let resp: acp::InitializeResponse = {
        let _timer = distill_telemetry::instrumentation::timer("acp_init.initialize_roundtrip");
        acp_send(req, tx).await?
    };
    let is_grok_shell = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("grokShell"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let models: ModelState = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("modelState"))
        .and_then(|v| serde_json::from_value::<acp::SessionModelState>(v.clone()).ok())
        .into();
    let available_commands = parse_available_commands(resp.meta.as_ref());
    let cancel_rewind_enabled = resp
        .meta
        .as_ref()
        .and_then(|m| m.get("cancelRewind"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let session_recap_available = parse_session_recap_available(resp.meta.as_ref());
    let feedback_trace_offer = parse_feedback_trace_offer(resp.meta.as_ref());
    let default_auth_method_id = parse_default_auth_method_id(resp.meta.as_ref());
    Ok(InitializedAgent {
        models,
        is_grok_shell,
        auth_methods: resp.auth_methods,
        default_auth_method_id,
        available_commands,
        cancel_rewind_enabled,
        session_recap_available,
        feedback_trace_offer,
    })
}
/// Parse `availableCommands` from an `InitializeResponse.meta` value.
///
/// Extracted as a standalone function for testability (the full `initialize()` function requires an ACP connection).
pub fn parse_available_commands(meta: Option<&acp::Meta>) -> Vec<acp::AvailableCommand> {
    meta.and_then(|m| m.get("availableCommands"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}
/// Parse `sessionRecap` from `InitializeResponse.meta` (shell rollout gate).
///
/// Default `false` when missing or non-bool so older agents and dark-launch defaults produce zero automatic recap traffic.
pub fn parse_session_recap_available(meta: Option<&acp::Meta>) -> bool {
    meta.and_then(|m| m.get("sessionRecap"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
pub fn parse_feedback_trace_offer(meta: Option<&acp::Meta>) -> bool {
    meta.and_then(|m| m.get("feedbackTraceOffer"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}
/// Determine whether interactive login is needed based on the advertised auth methods. Otherwise, authenticate
/// eagerly.
pub fn startup_auth_metadata(
    auth_methods: &[acp::AuthMethod],
) -> (
    bool,
    Option<String>,
    Option<acp::AuthMethodId>,
    AuthStartMode,
) {
    let first_method = auth_methods.first();
    let needs_login = first_method
        .map(|m| AuthMethodKind::from_id(m.id()).needs_interactive_login())
        .unwrap_or(false);
    if !needs_login {
        return (false, None, None, AuthStartMode::Pending);
    }
    let method = first_method.unwrap();
    let login_label = Some(method.name().to_string());
    let login_method_id = Some(method.id().clone());
    let is_provider = method
        .meta()
        .as_ref()
        .and_then(|v| v.get("external_provider"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let auth_start_mode = if is_provider {
        AuthStartMode::Command
    } else {
        AuthStartMode::Pending
    };
    (needs_login, login_label, login_method_id, auth_start_mode)
}
/// Find an interactive login method from the auth methods list.
/// Used when eager auth (cached_token or API key) fails and we need to fall back to the welcome screen with a working login button.
/// Scans the list for a `grok.com` or `oidc` method; these are the ones that can trigger a browser-based re-auth flow.
pub fn find_interactive_login_method(
    auth_methods: &[acp::AuthMethod],
) -> (Option<String>, Option<acp::AuthMethodId>, AuthStartMode) {
    let interactive = auth_methods
        .iter()
        .find(|m| AuthMethodKind::from_id(m.id()).needs_interactive_login());
    match interactive {
        Some(method) => {
            let is_provider = method
                .meta()
                .as_ref()
                .and_then(|v| v.get("external_provider"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mode = if is_provider {
                AuthStartMode::Command
            } else {
                AuthStartMode::Pending
            };
            (
                Some(method.name().to_string()),
                Some(method.id().clone()),
                mode,
            )
        }
        None => (None, None, AuthStartMode::Pending),
    }
}
/// Attempt eager auth; on failure fall back to the interactive login screen. The shell owns unpinned fallthrough,
/// and a failed api_key must not open a browser. Otherwise hand the interactive method for the login screen. Empty
/// `auth_methods` is fail-closed: needs_login without an interactive method.
async fn eager_auth_or_login_fallback(
    tx: &AcpAgentTx,
    auth_methods: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
    needs_login: bool,
    login_label: Option<String>,
    login_method_id: Option<acp::AuthMethodId>,
    auth_start_mode: AuthStartMode,
) -> (
    bool,
    Option<String>,
    Option<acp::AuthMethodId>,
    AuthStartMode,
    Option<serde_json::Value>,
) {
    if auth_methods.is_empty() {
        return (true, None, None, AuthStartMode::Pending, None);
    }
    if needs_login {
        return (
            needs_login,
            login_label,
            login_method_id,
            auth_start_mode,
            None,
        );
    }
    match authenticate(tx, auth_methods, default_auth_method_id).await {
        Ok(meta) => (
            needs_login,
            login_label,
            login_method_id,
            auth_start_mode,
            meta,
        ),
        Err(_) => {
            let has_api_key = auth_methods
                .iter()
                .any(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::XaiApiKey);
            if has_api_key {
                return (false, login_label, login_method_id, auth_start_mode, None);
            }
            let (label, method_id, mode) = find_interactive_login_method(auth_methods);
            (true, label, method_id, mode, None)
        }
    }
}
/// [`eager_auth_or_login_fallback`] bounded by `STARTUP_AUTH_REFRESH_TIMEOUT`, so a hung agent cannot gate the first draw.
/// On timeout the inputs pass through unchanged and the agent finishes authentication in the background.
async fn bounded_eager_auth(
    tx: &AcpAgentTx,
    auth_methods: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
    needs_login: bool,
    login_label: Option<String>,
    login_method_id: Option<acp::AuthMethodId>,
    auth_start_mode: AuthStartMode,
) -> (
    bool,
    Option<String>,
    Option<acp::AuthMethodId>,
    AuthStartMode,
    Option<serde_json::Value>,
) {
    match tokio::time::timeout(
        distill_shell::http::STARTUP_AUTH_REFRESH_TIMEOUT,
        eager_auth_or_login_fallback(
            tx,
            auth_methods,
            default_auth_method_id,
            needs_login,
            login_label.clone(),
            login_method_id.clone(),
            auth_start_mode,
        ),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(_) => (
            needs_login,
            login_label,
            login_method_id,
            auth_start_mode,
            None,
        ),
    }
}
/// Authenticate with the agent using the agent's chosen default method. Do not re-derive api_key vs session
/// ordering client-side (that has regressed OIDC refresh before).
async fn authenticate(
    tx: &AcpAgentTx,
    auth_methods: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
) -> Result<Option<serde_json::Value>> {
    let method_id = select_eager_auth_method(auth_methods, default_auth_method_id)
        .ok_or_else(|| anyhow::anyhow!("No auth methods available"))?;
    crate::unified_log::info(
        "pager eager auth method selected",
        None,
        Some(serde_json::json!({
            "method_id": method_id.0.as_ref(),
            "from_default_auth_method_id": default_auth_method_id
                .is_some_and(|d| d.0.as_ref() == method_id.0.as_ref()),
            "methods_count": auth_methods.len(),
            "first_method": auth_methods.first().map(|m| m.id().0.as_ref()),
        })),
    );
    let resp: acp::AuthenticateResponse =
        acp_send(acp::AuthenticateRequest::new(method_id), tx).await?;
    Ok(resp.meta.map(serde_json::Value::Object))
}
/// Pick the method id for eager authenticate.
/// 1. Agent's `defaultAuthMethodId` when present in the advertised list
/// 2. Legacy: `cached_token` if advertised, else first method
pub fn select_eager_auth_method(
    auth_methods: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
) -> Option<acp::AuthMethodId> {
    if let Some(default_id) = default_auth_method_id
        && auth_methods.iter().any(|m| m.id() == default_id)
    {
        return Some(default_id.clone());
    }
    let cached_token_method = auth_methods
        .iter()
        .find(|m| AuthMethodKind::from_id(m.id()) == AuthMethodKind::CachedToken);
    cached_token_method
        .or_else(|| auth_methods.first())
        .map(|m| m.id().clone())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn is_session_update_ext_method_covers_both_carriers() {
        assert!(is_session_update_ext_method("x.ai/session_notification"));
        assert!(is_session_update_ext_method("x.ai/session/update"));
        assert!(!is_session_update_ext_method("x.ai/task_completed"));
        assert!(!is_session_update_ext_method("session/update"));
    }
    #[test]
    fn parse_available_commands_from_meta() {
        let meta = serde_json::json!({
            "availableCommands": [
                {
                    "name": "compact",
                    "description": "Compact conversation history",
                    "input": { "hint": "<focus>" }
                },
                {
                    "name": "flush",
                    "description": "Flush memory"
                }
            ]
        });
        let cmds = parse_available_commands(meta.as_object());
        let [compact, flush] = cmds.as_slice() else {
            panic!("expected 2 commands: {cmds:?}");
        };
        assert_eq!(compact.name, "compact");
        assert_eq!(compact.description, "Compact conversation history");
        assert!(compact.input.is_some());
        assert_eq!(flush.name, "flush");
        assert!(flush.input.is_none());
    }
    #[test]
    fn parse_available_commands_missing_key_returns_empty() {
        let meta = serde_json::json!({ "grokShell": true });
        let cmds = parse_available_commands(meta.as_object());
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_available_commands_none_meta_returns_empty() {
        let cmds = parse_available_commands(None);
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_available_commands_invalid_json_returns_empty() {
        let meta = serde_json::json!({
            "availableCommands": "not-an-array"
        });
        let cmds = parse_available_commands(meta.as_object());
        assert!(cmds.is_empty());
    }
    #[test]
    fn parse_session_recap_available_true() {
        let meta = serde_json::json!({ "sessionRecap": true });
        assert!(parse_session_recap_available(meta.as_object()));
    }
    #[test]
    fn parse_session_recap_available_false_explicit() {
        let meta = serde_json::json!({ "sessionRecap": false });
        assert!(!parse_session_recap_available(meta.as_object()));
    }
    #[test]
    fn parse_session_recap_available_defaults_off_when_missing() {
        let meta = serde_json::json!({ "grokShell": true, "cancelRewind": true });
        assert!(!parse_session_recap_available(meta.as_object()));
        assert!(!parse_session_recap_available(None));
    }
    #[test]
    fn parse_session_recap_available_non_bool_defaults_off() {
        let meta = serde_json::json!({ "sessionRecap": "yes" });
        assert!(!parse_session_recap_available(meta.as_object()));
    }
    fn make_auth_method(id: &str, name: &str, meta: Option<serde_json::Value>) -> acp::AuthMethod {
        let mut agent = acp::AuthMethodAgent::new(acp::AuthMethodId::new(id), name.to_string());
        if let Some(m) = meta.and_then(|v| v.as_object().cloned()) {
            agent = agent.meta(m);
        }
        acp::AuthMethod::Agent(agent)
    }
    #[test]
    fn startup_auth_empty_methods_no_login() {
        let (needs, label, method_id, mode) = startup_auth_metadata(&[]);
        assert!(!needs);
        assert!(label.is_none());
        assert!(method_id.is_none());
        assert_eq!(mode, AuthStartMode::Pending);
    }
    #[test]
    fn startup_auth_grok_com_no_provider_needs_login_pending() {
        let methods = vec![make_auth_method("grok.com", "grok.com", None)];
        let (needs, label, method_id, mode) = startup_auth_metadata(&methods);
        assert!(needs);
        assert_eq!(label.as_deref(), Some("grok.com"));
        assert_eq!(method_id.as_ref().unwrap().0.as_ref(), "grok.com");
        assert_eq!(mode, AuthStartMode::Pending);
    }
    #[test]
    fn startup_auth_grok_com_with_external_provider_command() {
        let meta = serde_json::json!({ "external_provider": true });
        let methods = vec![make_auth_method("grok.com", "Acme Corp", Some(meta))];
        let (needs, label, method_id, mode) = startup_auth_metadata(&methods);
        assert!(needs);
        assert_eq!(label.as_deref(), Some("Acme Corp"));
        assert_eq!(method_id.as_ref().unwrap().0.as_ref(), "grok.com");
        assert_eq!(mode, AuthStartMode::Command);
    }
    #[test]
    fn startup_auth_non_grok_com_no_login() {
        let methods = vec![make_auth_method("api-key", "API Key", None)];
        let (needs, label, method_id, mode) = startup_auth_metadata(&methods);
        assert!(!needs);
        assert!(label.is_none());
        assert!(method_id.is_none());
        assert_eq!(mode, AuthStartMode::Pending);
    }
    /// Enterprise/BYOK configs MUST. It calls the shell-side `build_auth_methods()` with the exact inputs
    /// `MvpAgent::initialize()` would compute for an enterprise user. It fails because `startup_auth_metadata()`
    /// returns `needs_login = true`.
    #[test]
    fn shell_built_auth_methods_for_byok_user_skip_login_screen() {
        use distill_shell::agent::auth_method::{AuthMethodsBuildInputs, build_auth_methods};
        let built = build_auth_methods(AuthMethodsBuildInputs {
            has_external_api_key: true,
            has_cached_token: false,
            has_enterprise_oidc: false,
            enterprise_oidc_issuer: None,
            login_label: None,
            has_auth_provider_command: false,
            preferred_method: None,
        });
        let (needs, label, method_id, mode) = startup_auth_metadata(&built.methods);
        assert!(
            !needs,
            "shell built auth_methods for a BYOK user, but the pager still \
             reports needs_login = true. Either the shell stopped putting \
             xai.api_key first or the pager stopped treating xai.api_key as \
             a no-login method.",
        );
        assert!(label.is_none());
        assert!(method_id.is_none());
        assert_eq!(mode, AuthStartMode::Pending);
    }
    /// Inverse direction: when `xai.api_key` is NOT in the list, the pager MUST show the login screen. The pager only
    /// inspects `auth_methods.first()`. This locks the failure mode of the regression. It then either passes or fails
    /// on a meaningful new code path.
    #[test]
    fn startup_auth_xai_api_key_not_first_still_requires_login() {
        use distill_shell::agent::auth_method::{GROK_COM_METHOD_ID, XAI_API_KEY_METHOD_ID};
        let methods = vec![
            make_auth_method(GROK_COM_METHOD_ID, "Grok", None),
            make_auth_method(XAI_API_KEY_METHOD_ID, "xai.api_key", None),
        ];
        let (needs, _, _, _) = startup_auth_metadata(&methods);
        assert!(
            needs,
            "with grok.com first, the pager must require login -- pinning \
             the BAD-ordering failure mode (xai.api_key not first)",
        );
    }
    #[test]
    fn startup_auth_method_id_is_copied_not_synthesized() {
        let methods = vec![make_auth_method("grok.com", "My Login", None)];
        let (_, _, method_id, _) = startup_auth_metadata(&methods);
        let Some(first) = methods.first() else {
            panic!("expected an auth method");
        };
        assert_eq!(&method_id.unwrap(), first.id());
    }
    #[test]
    fn startup_auth_external_provider_false_is_pending() {
        let meta = serde_json::json!({ "external_provider": false });
        let methods = vec![make_auth_method("grok.com", "grok.com", Some(meta))];
        let (_, _, _, mode) = startup_auth_metadata(&methods);
        assert_eq!(mode, AuthStartMode::Pending);
    }
    #[test]
    fn unsupported_leader_flags_empty_when_none_set() {
        let flags = ConnectFlags::default();
        assert!(unsupported_leader_flags(&flags).is_empty());
    }
    #[test]
    fn unsupported_leader_flags_detects_all() {
        let flags = ConnectFlags {
            memory_enabled_override: Some(true),
            memory_override_flag: Some("--experimental-memory"),
            disable_web_search: true,
            storage_mode: Some("writeback".into()),
            subagents: true,
            ..Default::default()
        };
        let detected = unsupported_leader_flags(&flags);
        assert_eq!(detected.len(), 4);
        assert!(detected.contains(&"--experimental-memory"));
        assert!(detected.contains(&"--disable-web-search"));
        assert!(detected.contains(&"--storage-mode"));
        assert!(detected.contains(&"--subagents"));
    }
    #[test]
    fn unsupported_leader_flags_preserves_no_memory_spelling() {
        let flags = ConnectFlags {
            memory_enabled_override: Some(false),
            memory_override_flag: Some("--no-memory"),
            ..Default::default()
        };
        assert_eq!(unsupported_leader_flags(&flags), vec!["--no-memory"]);
    }
    #[test]
    fn unsupported_leader_flags_ignores_supported() {
        let flags = ConnectFlags {
            terminal: true,
            fs_read: true,
            fs_write: true,
            ..Default::default()
        };
        assert!(unsupported_leader_flags(&flags).is_empty());
    }
    #[test]
    fn build_initialize_meta_includes_rules_when_set() {
        let flags = ConnectFlags {
            rules: Some("Always reply in French.".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(
            meta.get("rules").and_then(|v| v.as_str()),
            Some("Always reply in French.")
        );
    }
    #[test]
    fn build_initialize_meta_omits_rules_when_unset() {
        let flags = ConnectFlags::default();
        let meta = build_initialize_meta(&flags);
        assert!(
            meta.get("rules").is_none(),
            "rules key must be absent when --rules is not set; meta={meta:?}"
        );
    }
    #[test]
    fn build_initialize_meta_carries_system_prompt_override() {
        let flags = ConnectFlags {
            system_prompt_override: Some("YOU ARE A PIRATE.".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(
            meta.get("systemPromptOverride").and_then(|v| v.as_str()),
            Some("YOU ARE A PIRATE.")
        );
    }
    #[test]
    fn build_initialize_meta_uses_custom_client_identifier_when_set() {
        let flags = ConnectFlags {
            client_identifier: Some("zed".into()),
            ..Default::default()
        };
        let meta = build_initialize_meta(&flags);
        assert_eq!(meta.get("clientType").and_then(|v| v.as_str()), Some("zed"));
    }
    #[test]
    fn client_capabilities_meta_defaults_absent_or_blank_mode_to_off() {
        let absent = client_capabilities_meta(&ConnectFlags::default());
        assert_eq!(
            absent
                .get("x.ai/hunkTracker")
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
            Some("off")
        );
        let blank = client_capabilities_meta(&ConnectFlags {
            hunk_tracker_mode: Some("   ".into()),
            ..Default::default()
        });
        assert_eq!(
            blank
                .get("x.ai/hunkTracker")
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
            Some("off")
        );
    }
    #[test]
    fn client_capabilities_meta_always_requests_user_message_echo() {
        let meta = client_capabilities_meta(&ConnectFlags::default());
        assert_eq!(
            meta.get(distill_shell::session::USER_MESSAGE_ECHO_CAPABILITY)
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }
    /// The agent gates the whole payload on this key, so a misspelling on either side switches the feature off with nothing to show for it.
    #[test]
    fn client_capabilities_meta_advertises_the_status_line_the_config_asked_for() {
        let key = distill_status_line::STATUS_LINE_CAPABILITY;
        for wants_a_row in [true, false] {
            let meta = client_capabilities_meta(&ConnectFlags {
                status_line: wants_a_row,
                ..Default::default()
            });
            assert_eq!(
                meta.get(key).and_then(|v| v.as_bool()),
                Some(wants_a_row),
                "status_line={wants_a_row}"
            );
        }
    }
    #[test]
    fn client_capabilities_meta_canonicalizes_off_and_mixed_case() {
        for raw in ["off", "OFF", "Disabled"] {
            let meta = client_capabilities_meta(&ConnectFlags {
                hunk_tracker_mode: Some(raw.into()),
                ..Default::default()
            });
            assert_eq!(
                meta.get("x.ai/hunkTracker")
                    .and_then(|v| v.get("mode"))
                    .and_then(|v| v.as_str()),
                Some("off"),
                "raw={raw}"
            );
        }
    }
}
