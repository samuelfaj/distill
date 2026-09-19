// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP server integration using the official rmcp SDK.

use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use distill_telemetry::region;
use distill_telemetry::region::Parent;

use agent_client_protocol as acp;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::{ChildStderr, Command},
    sync::{Mutex, Notify},
};

use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation,
        PaginatedRequestParams,
    },
    service::{
        ClientInitializeError, NotificationContext, RequestContext, RoleClient, RunningService,
        ServiceError,
    },
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{
        StreamableHttpClientTransport, Transport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

pub use crate::auth_status::McpOauthDiscovery;
use crate::auth_status::{HttpAuthDecision, decide_http_auth_from_disk};
use crate::call_result::mcp_output_from_call_result;
pub use crate::generation::{Generation, Replacement, Superseded};
use crate::oauth::OAUTH_DISCOVERY_TIMEOUT;
use crate::oauth_config::McpOAuthConfig;
pub use crate::shared_mcp_state::SharedMcpState;

use distill_tools::types::{
    output::{MCPOutputDetails, ToolOutput},
    tool::{ToolKind, ToolNamespace},
    tool_metadata::ToolMetadata,
};
use distill_tools::util::{ProcessGroup, ProcessScope};

pub use crate::tool_name::{
    MCP_QUALIFIED_NAME_MAX_CHARS, McpToolAdmissionError, PROVIDER_TOOL_NAME_MAX_CHARS,
    parse_mcp_qualified_name, parse_mcp_tool_name, qualify_mcp_tool_name, validate_tool_name,
};

/// MCP tool name delimiter: server names are qualified as `"server__tool"`.
/// Canonical definition lives in `distill_workspace_types`; re-exported here for callers that historically imported it from this module.
pub use distill_workspace_types::MCP_TOOL_NAME_DELIMITER;

/// Routing hint for first-party local app MCP endpoints: which agent/session a request belongs to. **Advisory only, never authentication** — any local process can set it, so receivers must not treat it as proof of identity.
/// Caller-supplied configs cannot smuggle it: the header is stripped from every HTTP/SSE config and re-added only when the spawn context asks for it (mirroring the `GROK_SESSION_ID` env protection on stdio servers).
pub const GROK_AGENT_ID_HEADER: &str = "X-Grok-Agent-ID";

/// Reqwest 0.13 twin of the 0.12 adapters in `distill_extra_ca`.
fn with_extra_root_certificates(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    distill_extra_ca::ensure_default_crypto_provider();
    builder = builder.tls_backend_rustls();
    for der in distill_extra_ca::extra_root_ders() {
        match reqwest::Certificate::from_der(der) {
            Ok(cert) => builder = builder.add_root_certificate(cert),
            Err(e) => tracing::warn!(
                error = %e,
                "extra CA bundle: validated DER rejected by reqwest 0.13; skipping cert"
            ),
        }
    }
    builder
}

/// Max protocol icons kept per server/tool at ingest.
pub const MAX_MCP_ICONS_PER_ENTITY: usize = 8;

/// Max bytes for a single icon `src` (including data URIs) at ingest.
pub const MAX_MCP_ICON_SRC_BYTES: usize = 64 * 1024;

/// Max bytes for a single icon `mime_type` string at ingest.
pub const MAX_MCP_ICON_MIME_TYPE_BYTES: usize = 128;

/// Max size tokens kept per icon (`48x48`, `any`, …) at ingest.
pub const MAX_MCP_ICON_SIZES: usize = 8;

/// Max bytes for a single size token at ingest.
pub const MAX_MCP_ICON_SIZE_TOKEN_BYTES: usize = 32;

/// Wire theme for MCP protocol icons.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum McpIconTheme {
    Light,
    Dark,
    #[serde(other)]
    Unknown,
}

/// ACP-facing MCP protocol icon (SEP-973), mirrored from rmcp so clients never depend on the quarantined SDK types.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpIcon {
    pub src: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sizes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<McpIconTheme>,
}

impl McpIcon {
    /// Convert an rmcp icon with ingest rules: trim `src`, allow only `https://` and `data:image/…`, drop empty/oversized values.
    pub fn from_rmcp(icon: rmcp::model::Icon) -> Option<Self> {
        let src = icon.src.trim();
        if src.is_empty() || src.len() > MAX_MCP_ICON_SRC_BYTES {
            return None;
        }
        if !is_allowed_mcp_icon_src(src) {
            return None;
        }
        let theme = match icon.theme {
            Some(rmcp::model::IconTheme::Light) => Some(McpIconTheme::Light),
            Some(rmcp::model::IconTheme::Dark) => Some(McpIconTheme::Dark),
            _ => None,
        };
        let mime_type = icon.mime_type.and_then(|mime| {
            let mime = mime.trim();
            if mime.is_empty() || mime.len() > MAX_MCP_ICON_MIME_TYPE_BYTES {
                None
            } else {
                Some(mime.to_owned())
            }
        });
        let sizes = icon.sizes.map(|sizes| {
            sizes
                .into_iter()
                .filter_map(|size| {
                    let size = size.trim();
                    if size.is_empty() || size.len() > MAX_MCP_ICON_SIZE_TOKEN_BYTES {
                        None
                    } else {
                        Some(size.to_owned())
                    }
                })
                .take(MAX_MCP_ICON_SIZES)
                .collect::<Vec<_>>()
        });
        let sizes = sizes.filter(|sizes| !sizes.is_empty());
        Some(Self {
            src: src.to_owned(),
            mime_type,
            sizes,
            theme,
        })
    }

    pub fn from_rmcp_list(icons: Option<Vec<rmcp::model::Icon>>) -> Vec<Self> {
        icons
            .unwrap_or_default()
            .into_iter()
            .filter_map(Self::from_rmcp)
            .take(MAX_MCP_ICONS_PER_ENTITY)
            .collect()
    }
}

fn is_allowed_mcp_icon_src(src: &str) -> bool {
    src.starts_with("data:image/") || src.starts_with("https://")
}

/// Sanitize an MCP server or tool name into a single safe path segment (e.g. `"user-Hugging Face"` becomes `user-Hugging_Face`).
/// Shared so the per-server folder advertised in the prompt matches the tool files on disk.
pub fn sanitize_descriptor_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Result of a diff-based MCP config update.
#[derive(Debug)]
pub struct McpConfigDiff {
    /// Server names that are new or had their config changed.
    pub added: Vec<McpServerName>,
    /// Server names that were removed or had their config changed (old instance torn down).
    pub removed: Vec<McpServerName>,
    /// Server names whose config is identical, so their clients stay alive.
    pub retained: Vec<McpServerName>,
}

/// A server-set change together with the claim for the pass that covers it, taken in one critical section.
#[must_use = "dropping the change releases the successor pass's claim"]
pub struct McpConfigChange {
    pub diff: McpConfigDiff,
    pub claim: InitClaimGuard,
}

/// MCP server name used as the key in client/tool maps (e.g. `"github"`, `"grok_com_linear"`).
pub type McpServerName = String;

/// Unqualified MCP tool name (e.g. `"create_issue"`, without the `server__` prefix).
type ToolName = String;

/// Init state. `Starting` precedes `finish_init`, which fires early so the session is not blocked on MCP; `Finished`
/// may still have handshakes draining; `Complete` is set by the pass once its last publication has landed.
#[derive(Debug, Default)]
pub enum InitProgress {
    #[default]
    NotStarted,
    Starting {
        /// Set once `mark_handshaking` has seeded the set.
        seeded: bool,
        handshaking: std::collections::HashSet<McpServerName>,
    },
    Finished {
        handshaking: std::collections::HashSet<McpServerName>,
    },
    Complete,
}

impl InitProgress {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Init work is outstanding, including the pass's final publication.
    pub fn is_in_progress(&self) -> bool {
        matches!(self, Self::Starting { .. } | Self::Finished { .. })
    }

    /// True iff `finish_init` has fired, regardless of whether background handshakes are still draining.
    /// Used for diagnostic logging that tells the pre-finish window apart from finished with handshakes still draining.
    pub fn has_finished_init(&self) -> bool {
        matches!(self, Self::Finished { .. } | Self::Complete)
    }

    /// True iff the named server is currently handshaking.
    pub fn is_server_handshaking(&self, name: &str) -> bool {
        match self {
            Self::Starting { handshaking, .. } | Self::Finished { handshaking } => {
                handshaking.contains(name)
            }
            Self::NotStarted | Self::Complete => false,
        }
    }

    /// Iterate over server names whose background handshake is still in flight.
    /// Empty when [`Self::NotStarted`] or fully complete.
    pub fn handshaking_servers(&self) -> impl Iterator<Item = &McpServerName> {
        match self {
            Self::Starting { handshaking, .. } | Self::Finished { handshaking } => {
                Some(handshaking.iter())
            }
            Self::NotStarted | Self::Complete => None,
        }
        .into_iter()
        .flatten()
    }

    /// Number of in-flight per-server handshakes.
    pub fn handshaking_count(&self) -> usize {
        match self {
            Self::Starting { handshaking, .. } | Self::Finished { handshaking } => {
                handshaking.len()
            }
            Self::NotStarted | Self::Complete => 0,
        }
    }

    /// Transition `Starting` to `Finished`, preserving the handshaking set.
    /// No-op if already `Finished`; warns and stays put if called from `NotStarted`, which would be a caller bug.
    pub fn finish(&mut self) {
        match self {
            Self::Starting { handshaking, .. } => {
                // Move only the inner set, leaving the outer `&mut self` ready to be reassigned
                // `mem::take(self)` would force a `NotStarted` placeholder and a redundant put-back in the already-Finished arm below
                let handshaking = std::mem::take(handshaking);
                *self = Self::Finished { handshaking };
            }
            // Idempotent: already past the finish boundary
            // Per-server handshakes continue draining via `mark_handshake_complete`
            Self::Finished { .. } | Self::Complete => {}
            Self::NotStarted => {
                tracing::warn!(
                    "InitProgress::finish called from NotStarted; staying in NotStarted"
                );
            }
        }
    }

    pub fn complete(&mut self) {
        *self = Self::Complete;
    }

    /// Transition any state to `NotStarted`, clearing all per-server progress.
    /// Used on generation mismatch (config change racing active init) and on full reset.
    pub fn cancel(&mut self) {
        *self = Self::NotStarted;
    }

    /// Add names to the handshaking set; only meaningful in `Starting` or `Finished`.
    /// Warns if called from `NotStarted`: a handshake starting without `try_start_init` is a caller bug.
    pub fn mark_handshaking(&mut self, names: impl IntoIterator<Item = McpServerName>) {
        match self {
            Self::Starting {
                seeded,
                handshaking,
            } => {
                *seeded = true;
                handshaking.extend(names);
            }
            Self::Finished { handshaking } => {
                handshaking.extend(names);
            }
            Self::NotStarted | Self::Complete => {
                tracing::warn!("InitProgress::mark_handshaking called outside a pass; ignoring");
            }
        }
    }

    /// Remove a server from the handshaking set (on success or failure of its handshake).
    /// No-op if not present or if `NotStarted`.
    pub fn mark_handshake_complete(&mut self, name: &str) {
        match self {
            Self::Starting { handshaking, .. } | Self::Finished { handshaking } => {
                handshaking.remove(name);
            }
            Self::NotStarted | Self::Complete => {}
        }
    }
}

/// One in-process SDK MCP server registration: its tool-namespace name and the SDK-side id echoed back in `x.ai/mcp/sdk_call`.
/// A named struct (rather than a `(String, String)` tuple) so callers can't transpose the two strings.
/// `Deserialize`d from a `_meta["x.ai/mcp/servers"]` entry, so the `serverId` wire field name is declared (and serde-checked) exactly once here.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AcpServerEntry {
    pub name: McpServerName,
    #[serde(rename = "serverId")]
    pub server_id: String,
}

/// The session's in-process SDK MCP servers (declared via `_meta["x.ai/mcp/servers"]`), bundled with the shared reverse-RPC invoker.
/// The registry survives `update_configs` clears; config reloads only touch `configs`/`owned_clients`.
struct AcpMcpRegistry {
    /// Registered servers (`name -> serverId`).
    servers: Vec<AcpServerEntry>,
    /// Shared reverse-RPC invoker all these servers' tools are called through (emits `x.ai/mcp/sdk_call` over the ACP connection).
    invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
}

/// `Cooling` waits out the backoff; `InFlight` marks a running respawn attempt and carries the token that attempt must present to settle.
/// An in-flight server is never handed to another trigger, and a token invalidated by config teardown makes the stale attempt's results unusable.
#[derive(Debug, Clone, Copy)]
enum UnreachableRetry {
    Cooling {
        retry_at: std::time::Instant,
    },
    InFlight {
        token: u64,
        started_at: std::time::Instant,
    },
}

/// Ownership of init. [`McpState`] keeps only a `Weak` to it, so a dropped pass releases ownership and the next
/// waiter re-owns.
#[must_use = "dropping the guard releases init ownership"]
pub struct InitClaimGuard {
    token: std::sync::Arc<()>,
    signal: std::sync::Arc<tokio::sync::Notify>,
}

impl Drop for InitClaimGuard {
    fn drop(&mut self) {
        self.signal.notify_waiters();
    }
}

/// Consolidated MCP state behind a single lock.
pub struct McpState {
    pub configs: Vec<acp::McpServer>,
    pub meta_config_map: McpMetaConfigMap,
    pub owned_clients: crate::owned_clients::OwnedClients,
    /// Clients inherited from parent via `SharedMcpPool`; never cleared by config changes.
    pub shared_clients: HashMap<McpServerName, Arc<McpClient>>,
    /// The session's in-process SDK MCP servers and their shared invoker/overrides; `None` when the session has none. See [`AcpMcpRegistry`].
    /// Kept out of `configs` (the closed `acp::McpServer` enum) so it survives `update_configs` clears.
    acp_mcp: Option<AcpMcpRegistry>,
    /// Init lifecycle behind the [`InitProgress`] state machine, which rules out nonsensical combinations like "initialized AND initializing".
    /// Private on purpose: external callers must go through the typed transition methods, not poke the variant directly.
    init_progress: InitProgress,
    /// Maps qualified tool name to its `_meta` from MCP tools/list. Populated during init.
    pub mcp_tool_meta: HashMap<String, serde_json::Value>,
    /// Maps qualified tool name to its protocol `icons` from MCP tools/list.
    pub mcp_tool_icons: HashMap<String, Vec<McpIcon>>,
    /// HTTP servers that support OAuth but haven't been authenticated yet.
    pub auth_required: std::collections::HashSet<McpServerName>,
    /// Servers whose background init failed (handshake error, `tools/list` error, or overall init timeout) even though a client object exists.
    /// Surfaced as `Unavailable` in status snapshots, so a server that never finished init does not show as `Ready`.
    pub init_failed: std::collections::HashMap<McpServerName, String>,
    /// Servers whose last spawn failed because the endpoint was unreachable (connectivity, not auth; see [`McpError::Unreachable`]).
    /// Its keys are a subset of [`Self::init_failed`]'s; cleared together with it.
    /// Drives `retry_unreachable_servers` so a transient network blip in a handshake does not permanently strip the session of the server's tools.
    unreachable_retry: std::collections::HashMap<McpServerName, UnreachableRetry>,
    /// Monotonic source for [`UnreachableRetry::InFlight`] attempt tokens.
    unreachable_attempt_counter: u64,
    /// Per-server set of unqualified tool names that the user has disabled.
    /// Persisted to `~/.grok/config.toml` under `[mcp_servers.<name>].disabled_tools`.
    pub disabled_tools: HashMap<McpServerName, std::collections::HashSet<ToolName>>,
    /// Stashed registrations for disabled tools so they can be re-enabled without a full MCP re-init (no need to call `list_tools` again).
    pub disabled_tool_registrations: HashMap<String, McpToolRegistration>,
    /// `Weak` to the live [`InitClaimGuard`]; dead once the owning pass drops it.
    init_owner: std::sync::Weak<()>,
    init_signal: std::sync::Arc<tokio::sync::Notify>,
    generation: Generation,
    event_writer: distill_session_events::EventWriter,
    /// Sender wired by the session actor to its `StatusDispatcher` task.
    /// Intentionally `None` in subagent-pool / shared-pool snapshots ([`SharedMcpPool`]), where the **parent** session owns the notification flow.
    /// **Private on purpose.** Callers MUST go through [`Self::set_client_event_tx`], which fans the sender into every `owned_clients` entry.
    client_event_tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>,
    elicitation_job_tx: Option<crate::elicitation::ElicitationInbox>,
}

impl McpState {
    pub fn new(configs: Vec<acp::McpServer>) -> Self {
        Self::new_with_meta(configs, McpMetaConfigMap::new())
    }

    pub fn new_with_meta(configs: Vec<acp::McpServer>, meta_config_map: McpMetaConfigMap) -> Self {
        Self {
            configs,
            meta_config_map,
            owned_clients: crate::owned_clients::OwnedClients::new(),
            shared_clients: HashMap::new(),
            acp_mcp: None,
            init_progress: InitProgress::default(),
            mcp_tool_meta: HashMap::new(),
            mcp_tool_icons: HashMap::new(),
            auth_required: std::collections::HashSet::new(),
            init_failed: HashMap::new(),
            unreachable_retry: HashMap::new(),
            unreachable_attempt_counter: 0,
            disabled_tools: HashMap::new(),
            disabled_tool_registrations: HashMap::new(),
            init_owner: std::sync::Weak::new(),
            init_signal: std::sync::Arc::new(tokio::sync::Notify::new()),
            generation: Generation::current(),
            event_writer: distill_session_events::EventWriter::noop(),
            client_event_tx: None,
            elicitation_job_tx: None,
        }
    }

    pub fn init_wait_signal(&self) -> std::sync::Arc<tokio::sync::Notify> {
        std::sync::Arc::clone(&self.init_signal)
    }

    pub fn notify_init_waiters(&self) {
        self.init_signal.notify_waiters();
    }

    /// Install (or remove) the [`McpClientEvent`] sender owned by the session-actor `StatusDispatcher`.
    /// New clients added later MUST be wired by the caller via [`McpClient::set_event_tx`].
    /// Do that before `get_tool_registrations` so `ensure_initialized`'s `Ready`/`HandshakeFailed` emit fires with `Some(tx)`.
    pub fn set_client_event_tx(
        &mut self,
        tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>,
    ) {
        self.client_event_tx = tx.clone();
        for client in self.owned_clients.values() {
            client.set_event_tx(tx.clone());
        }
        // Shared clients are intentionally NOT wired here
        // See the `client_event_tx` doc-comment for why a subagent must not duplicate the parent's event flow
    }

    /// Returns the sender wired by [`Self::set_client_event_tx`], or `None` if no dispatcher is attached (subagent / shared-pool snapshot).
    /// Exposed as a getter rather than a `pub` field so the fan-out contract on `client_event_tx` cannot be bypassed by a direct assignment.
    pub fn client_event_tx(&self) -> Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>> {
        self.client_event_tx.clone()
    }

    pub fn set_elicitation_tx(&mut self, tx: Option<crate::elicitation::ElicitationInbox>) {
        for client in self.owned_clients.values() {
            client.set_elicitation_tx(tx.clone());
        }
        self.elicitation_job_tx = tx;
    }

    pub fn elicitation_tx(&self) -> Option<crate::elicitation::ElicitationInbox> {
        self.elicitation_job_tx.clone()
    }

    pub fn set_event_writer(&mut self, writer: distill_session_events::EventWriter) {
        self.event_writer = writer;
    }

    pub fn event_writer(&self) -> &distill_session_events::EventWriter {
        &self.event_writer
    }

    /// Snapshot tool icons for `mcp/list`.
    /// Empty clears any prior entry so a tools/list refresh without icons does not keep a stale set.
    pub fn record_tool_icons(&mut self, qualified_name: String, icons: Vec<McpIcon>) {
        if icons.is_empty() {
            self.mcp_tool_icons.remove(&qualified_name);
        } else {
            self.mcp_tool_icons.insert(qualified_name, icons);
        }
    }

    /// Register the session's in-process SDK MCP servers (`name -> serverId`) plus the reverse-RPC invoker.
    /// Held across `update_configs` clears so each init re-adds them.
    pub fn set_acp_servers(
        &mut self,
        servers: Vec<AcpServerEntry>,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
    ) {
        self.acp_mcp = Some(AcpMcpRegistry { servers, invoker });
    }

    pub fn has_failure_record(&self, name: &str) -> bool {
        self.auth_required.contains(name) || self.init_failed.contains_key(name)
    }

    /// Inherited shared clients count: their tools are part of this session's init, so a shared-only session must not skip the init waits.
    pub fn has_mcp_servers(&self) -> bool {
        !self.configs.is_empty() || self.has_acp_servers() || !self.shared_clients.is_empty()
    }

    /// Whether any in-process SDK MCP servers are registered (so the session knows to run MCP init even with no `configs`).
    pub fn has_acp_servers(&self) -> bool {
        self.acp_mcp
            .as_ref()
            .is_some_and(|acp| !acp.servers.is_empty())
    }

    /// Whether `name` is a registered in-process SDK server. The registry survives config changes, so its namespaces stay claimed across generations.
    pub fn is_acp_server(&self, name: &str) -> bool {
        self.acp_mcp
            .as_ref()
            .is_some_and(|acp| acp.servers.iter().any(|entry| entry.name == name))
    }

    /// Registered SDK servers not yet connected (no owned/shared client): the ones an init pass should build.
    /// Shared by [`build_pending_acp_clients`] and [`pending_acp_server_names`] so the "what to build" filter lives in one place.
    fn pending_acp_entries(&self) -> impl Iterator<Item = &AcpServerEntry> {
        self.acp_mcp.iter().flat_map(|acp| {
            acp.servers.iter().filter(|entry| {
                !self.owned_clients.contains_key(&entry.name)
                    && !self.shared_clients.contains_key(&entry.name)
            })
        })
    }

    /// Names of the SDK servers [`build_pending_acp_clients`] will build; used to mark them initializing before the (async) build.
    pub fn pending_acp_server_names(&self) -> Vec<String> {
        self.pending_acp_entries()
            .map(|entry| entry.name.to_string())
            .collect()
    }

    /// Build [`McpClient`]s for registered ACP servers not already connected.
    /// Kept caller-side so this method stays pure (no file I/O under the `McpState` lock).
    pub fn build_pending_acp_clients(
        &self,
        overrides: &HashMap<String, McpClientTimeoutOverrides>,
    ) -> Vec<McpClient> {
        let Some(acp) = &self.acp_mcp else {
            return Vec::new();
        };
        self.pending_acp_entries()
            .map(|entry| {
                McpClient::new_acp(
                    entry.name.clone(),
                    entry.server_id.clone(),
                    acp.invoker.clone(),
                    overrides.get(&entry.name),
                    self.meta_config_map.get(&entry.name),
                )
            })
            .collect()
    }

    /// Check if a specific tool is disabled for a server (unqualified tool name).
    pub fn is_tool_disabled(&self, server_name: &str, tool_name: &str) -> bool {
        self.disabled_tools
            .get(server_name)
            .is_some_and(|set| set.contains(tool_name))
    }

    /// Update configs and reset initialization state.
    /// Returns true if the configs actually changed, false if they were identical.
    pub fn update_configs(&mut self, new_configs: Vec<acp::McpServer>) -> bool {
        if mcp_servers_equal(&self.configs, &new_configs) {
            tracing::debug!("MCP configs unchanged, skipping update");
            return false;
        }

        // Clear owned clients only; shared (inherited) clients are untouched
        self.owned_clients.clear();
        self.mcp_tool_meta.clear();
        self.mcp_tool_icons.clear();
        self.disabled_tool_registrations.clear();
        self.configs = new_configs;
        self.cancel_any_init();
        self.auth_required.clear();
        self.init_failed.clear();
        self.unreachable_retry.clear();
        self.advance_generation(Replacement::ServerSetChange);
        true
    }

    fn advance_generation(&mut self, by: Replacement) {
        self.generation.replace(by);
        self.generation = Generation::current();
        self.init_signal.notify_waiters();
    }

    pub fn current_generation(&self) -> Generation {
        self.generation.clone()
    }

    /// Forgets every piece of per-server state, so a removed or changed server leaves nothing stale behind.
    fn forget_server(&mut self, name: &str) {
        self.owned_clients.remove(name);
        self.auth_required.remove(name);
        self.init_failed.remove(name);
        self.unreachable_retry.remove(name);
        self.init_progress.mark_handshake_complete(name);
        let prefix = format!("{}{}", name, MCP_TOOL_NAME_DELIMITER);
        self.mcp_tool_meta.retain(|k, _| !k.starts_with(&prefix));
        self.mcp_tool_icons.retain(|k, _| !k.starts_with(&prefix));
        self.disabled_tool_registrations
            .retain(|k, _| !k.starts_with(&prefix));
    }

    /// Diff-based config update: only tears down servers whose config changed or were removed, keeps healthy unchanged servers alive.
    ///
    /// Returns `None` if configs are identical (no work needed), or `Some(diff)` describing which servers to add/remove.
    pub fn update_configs_diff(
        &mut self,
        new_configs: Vec<acp::McpServer>,
    ) -> Option<McpConfigDiff> {
        if mcp_servers_equal(&self.configs, &new_configs) {
            tracing::debug!("MCP configs unchanged, skipping update");
            return None;
        }

        let old_by_name: HashMap<&str, String> = self
            .configs
            .iter()
            .filter_map(|c| match serde_json::to_string(c) {
                Ok(json) => Some((mcp_server_name(c), json)),
                Err(e) => {
                    tracing::warn!(server = mcp_server_name(c), error = %e, "Failed to serialize MCP server config for diff");
                    None
                }
            })
            .collect();

        let new_by_name: HashMap<&str, String> = new_configs
            .iter()
            .filter_map(|c| match serde_json::to_string(c) {
                Ok(json) => Some((mcp_server_name(c), json)),
                Err(e) => {
                    tracing::warn!(server = mcp_server_name(c), error = %e, "Failed to serialize MCP server config for diff");
                    None
                }
            })
            .collect();

        let mut removed = Vec::new();
        let mut added = Vec::new();
        let mut retained = Vec::new();

        for (name, old_json) in &old_by_name {
            match new_by_name.get(name) {
                None => removed.push(name.to_string()),
                Some(new_json) if new_json != old_json => {
                    removed.push(name.to_string());
                }
                Some(_) => retained.push(name.to_string()),
            }
        }

        for (name, new_json) in &new_by_name {
            match old_by_name.get(name) {
                None => added.push(name.to_string()),
                Some(old_json) if old_json != new_json => {
                    added.push(name.to_string());
                }
                Some(_) => {}
            }
        }

        for name in &removed {
            self.forget_server(name);
        }

        tracing::info!(
            retained = retained.len(),
            added = added.len(),
            removed = removed.len(),
            "MCP config diff: {} retained, {} added, {} removed",
            retained.len(),
            added.len(),
            removed.len(),
        );

        self.configs = new_configs;
        self.cancel_any_init();
        self.advance_generation(Replacement::ServerSetChange);

        Some(McpConfigDiff {
            added,
            removed,
            retained,
        })
    }

    /// [`Self::update_configs_diff`], handing the caller the successor pass's claim before any waiter can take it.
    pub fn change_configs(&mut self, new_configs: Vec<acp::McpServer>) -> Option<McpConfigChange> {
        let diff = self.update_configs_diff(new_configs)?;
        Some(McpConfigChange {
            diff,
            claim: self.start_init(),
        })
    }

    /// The strict per-server check matters because session actors call [`Self::finish_init`] **early**, right after spawning processes.
    /// Otherwise they would race the per-server handshakes and the first tool call would land inside the [`ClientState::Initializing`] window.
    pub fn is_initialized(&self) -> bool {
        self.init_progress.is_complete()
    }

    /// Pairs with [`Self::is_initialized`] across the window between the early [`Self::finish_init`] and the handshaking set draining.
    /// In that window `is_initialized()` is still `false` (per-server work remains) AND `is_initializing()` is `true`.
    /// So wait-loops keep waiting instead of kicking off a second init.
    pub fn is_initializing(&self) -> bool {
        self.init_progress.is_in_progress() && self.init_owner.strong_count() > 0
    }

    /// The pass that owned init is gone with handshakes still marked.
    pub fn is_init_abandoned(&self) -> bool {
        self.init_progress.is_in_progress() && self.init_owner.strong_count() == 0
    }

    /// Returns `true` once `finish_init` has fired, regardless of whether per-server background handshakes are still draining.
    /// Used for diagnostic logging that tells the pre-finish window apart from post-finish with background work still draining.
    pub fn has_finished_init(&self) -> bool {
        self.init_progress.has_finished_init()
    }

    /// Borrow the underlying [`InitProgress`] state machine, primarily for tests that want to assert against the discriminant directly.
    pub fn init_progress(&self) -> &InitProgress {
        &self.init_progress
    }

    /// Try to start initialization.
    /// `None` while a live pass owns init or once init is complete.
    pub fn try_start_init(&mut self) -> Option<InitClaimGuard> {
        if self.is_initializing() || self.init_progress.is_complete() {
            return None;
        }
        Some(self.start_init())
    }

    /// Cancels any pass, starts a new generation, and hands the caller the claim before any waiter can take it.
    pub fn restart_init(&mut self) -> InitClaimGuard {
        self.cancel_any_init();
        self.advance_generation(Replacement::Rebuild);
        self.start_init()
    }

    fn start_init(&mut self) -> InitClaimGuard {
        // A dead pass may have left handshakes marked.
        self.init_progress = InitProgress::Starting {
            seeded: false,
            handshaking: std::collections::HashSet::new(),
        };
        let token = std::sync::Arc::new(());
        self.init_owner = std::sync::Arc::downgrade(&token);
        InitClaimGuard {
            token,
            signal: std::sync::Arc::clone(&self.init_signal),
        }
    }

    /// A generation change releases every claim, so a holder that awaited asks again under the lock.
    pub(crate) fn owns_init(&self, guard: &InitClaimGuard) -> bool {
        self.init_owner
            .upgrade()
            .is_some_and(|live| std::sync::Arc::ptr_eq(&live, &guard.token))
    }

    /// The token is cancelled under this lock, so the check holds for every write in the same critical section.
    pub(crate) fn is_current(&self, generation: &Generation) -> bool {
        !generation.is_cancelled()
    }

    /// Transition [`InitProgress::Starting`] to [`InitProgress::Finished`], preserving the per-server handshaking set; no-op for a stale `generation`.
    /// Called early (before per-server handshakes complete) so the session is unblocked for non-MCP work.
    /// `is_initialized()` still returns `false` until every handshake has reported via [`Self::mark_server_ready`].
    pub fn finish_init(&mut self) {
        self.init_progress.finish();
        self.init_signal.notify_waiters();
    }

    pub fn cancel_any_init(&mut self) {
        self.init_owner = std::sync::Weak::new();
        self.init_progress.cancel();
        self.init_signal.notify_waiters();
    }

    /// Add server names to the handshaking set; call after filtering `configs_to_start`, before spawning per-server tasks; no-op for a stale `generation`.
    /// Only meaningful in [`InitProgress::Starting`] / [`InitProgress::Finished`]; logs a warning otherwise.
    pub fn mark_servers_initializing(&mut self, names: impl IntoIterator<Item = McpServerName>) {
        let names: Vec<McpServerName> = names.into_iter().collect();
        // A fresh attempt clears the prior verdicts (and the unreachable-respawn schedule); a failure re-records them.
        for name in &names {
            self.clear_init_failed(name);
            self.auth_required.remove(name);
        }
        self.init_progress.mark_handshaking(names);
        // Servers outside the set stop being pending here.
        self.init_signal.notify_waiters();
    }

    /// Remove a server from the handshaking set (on success or failure of its handshake). Safe if not present; no-op for a stale `generation`.
    pub fn mark_server_ready(&mut self, name: &str) {
        self.init_progress.mark_handshake_complete(name);
        self.init_signal.notify_waiters();
    }

    /// A live pass may still hand `name` a client.
    pub fn is_server_pending(&self, name: &str) -> bool {
        self.is_initializing()
            && match &self.init_progress {
                InitProgress::Starting { seeded: false, .. } => true,
                progress => progress.is_server_handshaking(name),
            }
    }

    /// Record a per-server background-init failure for status reporting, routing it to the correct set so the two stay disjoint.
    /// Such servers must NOT also land in `init_failed`, or a server that authenticates would stay reported as `Unavailable` with zero tools.
    pub fn record_init_failure(&mut self, name: &str, needs_auth: bool, detail: Option<String>) {
        if needs_auth {
            self.auth_required.insert(name.to_string());
        } else {
            self.init_failed
                .insert(name.to_string(), detail.unwrap_or_default());
        }
    }

    /// Clear a prior init failure for `name` (symmetric with [`Self::record_init_failure`]).
    /// Used by the reactive managed re-auth path so a server that recovers is no longer reported as `Unavailable` with a stale non-auth `detail`.
    pub fn clear_init_failed(&mut self, name: &str) {
        self.init_failed.remove(name);
        self.unreachable_retry.remove(name);
    }

    /// Minimum wait between spawn attempts for an unreachable server.
    /// Retry triggers (tool batches, `x.ai/mcp/list` refreshes) cannot dogpile the OAuth-discovery and probe timeout budget while a server is down.
    pub const UNREACHABLE_RETRY_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

    /// Upper bound on an attempt's exclusivity (see [`UnreachableRetry`]).
    /// Far above any bounded spawn and handshake, so it only fires for attempts whose future was cancelled and can never settle.
    pub const UNREACHABLE_ATTEMPT_LEASE: std::time::Duration = std::time::Duration::from_secs(600);

    /// Record a spawn failure caused by an unreachable endpoint ([`McpError::Unreachable`]); no-op for a stale `generation`.
    /// Never demotes an in-flight attempt: its settle call owns the next transition.
    pub fn record_unreachable_failure(&mut self, name: &str, detail: String) {
        self.record_unreachable_failure_at(
            name,
            detail,
            std::time::Instant::now() + Self::UNREACHABLE_RETRY_COOLDOWN,
        );
    }

    /// Explicit-instant twin of [`Self::record_unreachable_failure`] so tests can place a candidate inside or beyond the cooldown window.
    fn record_unreachable_failure_at(
        &mut self,
        name: &str,
        detail: String,
        retry_at: std::time::Instant,
    ) {
        self.init_failed.insert(name.to_string(), detail);
        if let Some(UnreachableRetry::InFlight { .. }) = self.unreachable_retry.get(name) {
            return;
        }
        self.unreachable_retry
            .insert(name.to_string(), UnreachableRetry::Cooling { retry_at });
    }

    /// Servers due for an unreachable-respawn attempt: cooling finished and the server is still configured.
    /// An in-flight server is not a candidate, so concurrent triggers cannot double-spawn even while an attempt outlives the cooldown.
    pub fn take_unreachable_retry_candidates(&mut self) -> Vec<(McpServerName, u64)> {
        self.take_unreachable_retry_candidates_at(std::time::Instant::now())
    }

    /// Explicit-instant twin of [`Self::take_unreachable_retry_candidates`] so tests can exercise the attempt lease without waiting it out.
    fn take_unreachable_retry_candidates_at(
        &mut self,
        now: std::time::Instant,
    ) -> Vec<(McpServerName, u64)> {
        let configured: std::collections::HashSet<&str> =
            self.configs.iter().map(mcp_server_name).collect();
        let due: Vec<McpServerName> = self
            .unreachable_retry
            .iter()
            .filter(|(name, state)| {
                configured.contains(name.as_str())
                    && match state {
                        UnreachableRetry::Cooling { retry_at } => *retry_at <= now,
                        // A cancelled attempt never settles; reclaim it once its lease expires (the old token no-ops from here)
                        UnreachableRetry::InFlight { started_at, .. } => {
                            now.saturating_duration_since(*started_at)
                                > Self::UNREACHABLE_ATTEMPT_LEASE
                        }
                    }
            })
            .map(|(name, _)| name.clone())
            .collect();
        due.into_iter()
            .map(|name| {
                self.unreachable_attempt_counter += 1;
                let token = self.unreachable_attempt_counter;
                self.unreachable_retry.insert(
                    name.clone(),
                    UnreachableRetry::InFlight {
                        token,
                        started_at: now,
                    },
                );
                (name, token)
            })
            .collect()
    }

    /// True when `name` currently holds an in-flight attempt with `token`.
    fn owns_unreachable_attempt(&self, name: &str, token: u64) -> bool {
        matches!(
            self.unreachable_retry.get(name),
            Some(UnreachableRetry::InFlight { token: t, .. }) if *t == token
        )
    }

    /// Settle an attempt as recovered. Returns whether the attempt still owns the server (token match).
    /// A `false` means a config update tore the server down mid-attempt and the caller must discard its client.
    pub fn finish_unreachable_attempt(&mut self, name: &str, token: u64) -> bool {
        if !self.owns_unreachable_attempt(name, token) {
            return false;
        }
        self.unreachable_retry.remove(name);
        self.init_failed.remove(name);
        true
    }

    /// Settle an attempt as still-unreachable: restarts the cooldown (from now, i.e. when the attempt finished) and refreshes the failure detail.
    /// No-op for a stale token.
    pub fn settle_unreachable_attempt_failed(&mut self, name: &str, token: u64, detail: String) {
        if !self.owns_unreachable_attempt(name, token) {
            return;
        }
        self.init_failed.insert(name.to_string(), detail);
        self.unreachable_retry.insert(
            name.to_string(),
            UnreachableRetry::Cooling {
                retry_at: std::time::Instant::now() + Self::UNREACHABLE_RETRY_COOLDOWN,
            },
        );
    }

    /// Settle an attempt without keeping it retryable (terminal non-connectivity failure, or handoff to the auth-required flow).
    /// Returns whether the attempt still owned the server so the caller knows its follow-up records are legitimate.
    /// The `init_failed` entry is left to the caller.
    pub fn settle_unreachable_attempt_unretryable(&mut self, name: &str, token: u64) -> bool {
        if !self.owns_unreachable_attempt(name, token) {
            return false;
        }
        self.unreachable_retry.remove(name);
        true
    }

    /// Clear the entire handshaking set in one shot; no-op for a stale `generation`.
    /// Used as a defensive sweep after the per-server [`Self::mark_server_ready`] calls; cheap no-op if already empty.
    /// Callers: the proxy-mode "init complete" path and the bg-handshake completion path.
    /// A step the pass takes after its last publication, so a waiter released here searches the complete tool set.
    pub fn complete_init(&mut self) {
        self.init_progress.complete();
        self.init_signal.notify_waiters();
    }

    /// True iff the named server's handshake is still in flight.
    /// Used by status snapshots and tool-dispatch gating that need to know per-server progress without cloning the whole set.
    pub fn is_server_handshaking(&self, name: &str) -> bool {
        self.init_progress.is_server_handshaking(name)
    }

    /// Iterate over server names whose background handshake is still in flight.
    /// Empty when init has not started or has fully completed.
    pub fn handshaking_servers_iter(&self) -> impl Iterator<Item = &McpServerName> {
        self.init_progress.handshaking_servers()
    }

    /// Snapshot of the handshaking set as a cloned `HashSet`.
    /// Used by the status snapshot API where the caller wants an owned copy that survives lock release.
    pub fn handshaking_servers_cloned(&self) -> std::collections::HashSet<McpServerName> {
        self.init_progress.handshaking_servers().cloned().collect()
    }

    /// Number of in-flight per-server handshakes.
    pub fn handshaking_servers_count(&self) -> usize {
        self.init_progress.handshaking_count()
    }

    /// Look up a client by server name.
    /// Owned clients take priority (they can override inherited ones).
    pub fn get_client(&self, name: &str) -> Option<&Arc<McpClient>> {
        self.owned_clients
            .get(name)
            .or_else(|| self.shared_clients.get(name))
    }

    /// Whether `client` is still the one installed under `name`.
    pub fn has_client(&self, name: &str, client: &Arc<McpClient>) -> bool {
        self.get_client(name)
            .is_some_and(|installed| Arc::ptr_eq(installed, client))
    }

    /// Iterate over all clients (owned first, then shared, skipping shared entries whose name is overridden by an owned client).
    pub fn all_clients(&self) -> impl Iterator<Item = (&McpServerName, &Arc<McpClient>)> {
        self.owned_clients.iter().chain(
            self.shared_clients
                .iter()
                .filter(|(name, _)| !self.owned_clients.contains_key(name.as_str())),
        )
    }

    /// Import shared clients from a parent pool snapshot.
    /// Clients whose name collides with an agent-definition-owned server are skipped (the owned server takes priority).
    pub fn import_shared_clients(&mut self, pool: &SharedMcpPool) {
        let config_names: std::collections::HashSet<&str> =
            self.configs.iter().map(mcp_server_name).collect();
        for (name, client) in &pool.clients {
            if !config_names.contains(name.as_str()) {
                self.shared_clients.insert(name.clone(), Arc::clone(client));
            }
        }
    }
}

/// Snapshot of an MCP connection pool, taken at subagent spawn time.
/// This is intentionally snapshot-based, not live-updating.
#[derive(Clone)]
pub struct SharedMcpPool {
    clients: HashMap<McpServerName, Arc<McpClient>>,
    configs: Vec<acp::McpServer>,
    meta_config_map: McpMetaConfigMap,
}

impl SharedMcpPool {
    /// Create a snapshot from an existing `McpState`.
    /// Captures both owned and shared clients (deduped, owned wins).
    pub fn from_state(state: &McpState) -> Self {
        Self {
            clients: state
                .all_clients()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect(),
            configs: state.configs.clone(),
            meta_config_map: state.meta_config_map.clone(),
        }
    }

    pub fn get_client(&self, name: &str) -> Option<&Arc<McpClient>> {
        self.clients.get(name)
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    pub fn server_names(&self) -> impl Iterator<Item = &str> {
        self.clients.keys().map(String::as_str)
    }

    pub fn configs(&self) -> &[acp::McpServer] {
        &self.configs
    }

    pub fn meta_config_map(&self) -> &McpMetaConfigMap {
        &self.meta_config_map
    }

    /// Retain only clients whose name satisfies `predicate`.
    /// Only filters the `clients` map; `configs` and `meta_config_map` are left unchanged.
    /// In the subagent inheritance path this is fine because `import_shared_clients` only iterates `clients`.
    pub fn retain_clients(&mut self, predicate: impl Fn(&str) -> bool) {
        self.clients.retain(|name, _| predicate(name));
    }
}

/// Since `acp::McpServer` may not implement PartialEq, we serialize to JSON and compare.
/// This is only called during config updates, so the overhead is acceptable.
pub(crate) fn mcp_servers_equal(a: &[acp::McpServer], b: &[acp::McpServer]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    match (serde_json::to_string(a), serde_json::to_string(b)) {
        (Ok(a_json), Ok(b_json)) => a_json == b_json,
        _ => false, // If serialization fails, assume not equal
    }
}

/// Default timeout for an MCP server's `initialize` handshake and initial tool listing, used when no override is supplied.
/// 30s is generous enough that cold-start `uvx` / `uv run --with` stdio servers that download deps on first launch aren't killed mid-handshake.
/// The shell resolves env / config / requirements / remote overrides and injects them via `McpClientTimeoutOverrides`.
const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 30;

/// Default timeout for individual tool calls.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 6000;

const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a stdio server gets to exit after its transport closes before its process group is killed.
const STDIO_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

const ANONYMOUS_ACCESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Per-MCP-server config overrides from `_meta.mcpConfig` in session/new or session/load.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerMetaConfig {
    /// Init handshake timeout in ms. Overrides config.toml `startup_timeout_sec`.
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,
    /// Per-tool-call timeout in ms. Overrides config.toml `tool_timeout_sec`.
    #[serde(default)]
    pub tool_timeout_ms: Option<u64>,
    /// Per-tool timeout overrides in ms: `{ "create_issue": 120000, "search": 30000 }`.
    /// Overrides config.toml `tool_timeouts` (and `tool_timeout_sec`) for matching tools.
    #[serde(default)]
    pub tool_timeouts_ms: Option<HashMap<ToolName, u64>>,
    /// Also keep the raw base64 in tool-result text, in addition to the vision-token rendering.
    /// The agent can then decode and forward it via path-based tools like `send_file`.
    /// Costs ~2× tokens per image. Default `false`. See `call_result::format_mcp_image`.
    #[serde(default)]
    pub expose_image_base64: Option<bool>,
}

/// Maps MCP server name to its per-server config overrides from `_meta.mcpConfig`.
pub type McpMetaConfigMap = HashMap<McpServerName, McpServerMetaConfig>;

/// Parse `mcpConfig` from a session request's `_meta`. Empty map if absent/invalid.
pub fn parse_mcp_meta_config(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> McpMetaConfigMap {
    meta.and_then(|m| m.get("mcpConfig"))
        .and_then(|v| serde_json::from_value::<McpMetaConfigMap>(v.clone()).ok())
        .unwrap_or_default()
}

/// MCP initialization strategy. Defined in `distill-telemetry`; re-exported here so existing call sites continue to work.
pub use distill_telemetry::enums::McpInitStrategy;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP client error: {0}")]
    ClientError(String),

    #[error("MCP server '{server}' timed out after {timeout_secs}s")]
    Timeout { server: String, timeout_secs: u64 },

    #[error("Failed to spawn MCP server '{server}': {source}")]
    SpawnFailed {
        server: String,
        source: std::io::Error,
    },

    #[error("MCP server '{server}' handshake failed: {source}")]
    HandshakeFailed {
        server: String,
        source: Box<ClientInitializeError>,
    },

    /// Pre-spawn gate: server needs OAuth but this session cannot complete interactive auth.
    #[error(
        "MCP server '{server}': Auth required (non-interactive session; authenticate in TUI or set Authorization header)"
    )]
    AuthRequired { server: String },

    /// Pre-spawn gate: neither OAuth discovery nor the anonymous-access probe could reach the server.
    /// A connectivity failure, not an auth verdict.
    /// Recorded as a retryable init failure so a transient network blip does not permanently strip the session of this server's tools.
    #[error("MCP server '{server}': unreachable during startup (connection failed; will retry)")]
    Unreachable { server: String },

    #[error("MCP service error: {0}")]
    ServiceError(#[from] ServiceError),
}

impl McpError {
    fn timeout(server: &str, duration: std::time::Duration) -> Self {
        Self::Timeout {
            server: server.to_string(),
            timeout_secs: duration.as_secs(),
        }
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }

    pub fn error_category(&self) -> distill_session_events::McpErrorCategory {
        use distill_session_events::McpErrorCategory;
        match self {
            Self::SpawnFailed { .. } => McpErrorCategory::SpawnFailed,
            Self::Timeout { .. } => McpErrorCategory::Timeout,
            Self::HandshakeFailed { .. } => McpErrorCategory::HandshakeFailed,
            Self::AuthRequired { .. } => McpErrorCategory::AuthRequired,
            // Connectivity failure surfaced at spawn time.
            Self::Unreachable { .. } => McpErrorCategory::SpawnFailed,
            Self::ClientError(_) | Self::ServiceError(_) => McpErrorCategory::ClientError,
        }
    }

    pub fn server_name(&self) -> Option<&str> {
        match self {
            Self::SpawnFailed { server, .. }
            | Self::Timeout { server, .. }
            | Self::HandshakeFailed { server, .. }
            | Self::AuthRequired { server }
            | Self::Unreachable { server } => Some(server),
            Self::ClientError(_) | Self::ServiceError(_) => None,
        }
    }

    /// True if this error indicates the server rejected us for auth reasons (a credential re-fetch could help).
    /// Timeout/spawn failures can't be cured by re-fetching credentials, so they're never auth.
    pub fn is_auth_rejection(&self) -> bool {
        match self {
            Self::AuthRequired { .. } => true,
            Self::HandshakeFailed { source, .. } => is_auth_rejection_message(&source.to_string()),
            Self::ServiceError(e) => is_auth_rejection_message(&e.to_string()),
            Self::ClientError(s) => is_auth_rejection_message(s),
            Self::Timeout { .. } | Self::SpawnFailed { .. } | Self::Unreachable { .. } => false,
        }
    }

    /// True if the server could not be reached at all during startup: a retryable connectivity failure (see [`Self::Unreachable`]).
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable { .. })
    }

    /// True for failures that say nothing about the server being broken (only about the path to it) and are therefore worth an automatic respawn.
    /// Protocol rejections and malformed responses stay non-retryable.
    pub fn is_transient_connectivity(&self) -> bool {
        match self {
            Self::Unreachable { .. } | Self::Timeout { .. } => true,
            Self::HandshakeFailed { source, .. } => is_transport_error_message(&source.to_string()),
            Self::ClientError(s) => is_transport_error_message(s),
            Self::ServiceError(e) => is_transport_error_message(&e.to_string()),
            Self::SpawnFailed { .. } | Self::AuthRequired { .. } => false,
        }
    }

    /// The never-connected subset of [`Self::is_transient_connectivity`].
    pub fn is_connect_failure(&self) -> bool {
        match self {
            Self::Unreachable { .. } => true,
            Self::HandshakeFailed { source, .. } => {
                init_error_is_connect_phase(source)
                    || is_connect_failure_message(&source.to_string())
            }
            Self::ClientError(s) => is_connect_failure_message(s),
            Self::ServiceError(e) => is_connect_failure_message(&e.to_string()),
            Self::Timeout { .. } | Self::SpawnFailed { .. } | Self::AuthRequired { .. } => false,
        }
    }
}

fn init_error_is_connect_phase(err: &ClientInitializeError) -> bool {
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    let ClientInitializeError::TransportError { error, .. } = err else {
        return false;
    };
    matches!(
        error
            .error
            .downcast_ref::<StreamableHttpError<reqwest::Error>>(),
        Some(StreamableHttpError::Client(e)) if e.is_connect()
    )
}

/// Connect-level subset of [`is_transport_error_message`].
fn is_connect_failure_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "connection refused",
        "dns error",
        "network unreachable",
        "host unreachable",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// Detect transport-level failures from the error message.
/// Used for error types that don't carry a typed connectivity verdict (handshake and client errors).
/// Same stable-`Display`-text approach as `is_auth_rejection_message`.
fn is_transport_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "error sending request",
        "network unreachable",
        "host unreachable",
        "timed out",
        "transport closed",
        "dns error",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

/// True when a failed refresh-token grant was a network-level failure that never reached the IdP (the token's validity is unknown, presumed good).
/// IdP rejections and missing credentials stay terminal (escalate to browser).
/// An IdP error description can't spoof a `starts_with` match.
pub(crate) fn mcp_refresh_failure_is_transient(err: &rmcp::transport::auth::AuthError) -> bool {
    match err {
        rmcp::transport::auth::AuthError::TokenRefreshFailed(msg) => {
            msg.starts_with("Request failed") || msg.starts_with("Failed to parse server response")
        }
        _ => false,
    }
}

/// True if an MCP error *message* indicates an auth rejection (vs. a transport drop, timeout, or protocol error).
/// Host recovery uses it to decide whether a credential re-fetch would help.
/// Matches auth wording and context-anchored 401 patterns only, so a bare digit ("took 401ms", ports) can't trip it.
pub fn is_auth_rejection_message(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    // Auth wording has no numeric component, so plain substrings are safe.
    if l.contains("auth required")
        || l.contains("authorizationrequired")
        || l.contains("authrequired")
        || l.contains("authentication")
        || l.contains("unauthorized")
    {
        return true;
    }
    // Require a non-alphanumeric (or end) after "401" so "http 401" matches but "http 4012" (other status) and "http 401ms" (a duration) do not
    [
        "status: 401",
        "status code 401",
        "http status 401",
        "http 401",
        "error 401",
    ]
    .iter()
    .any(|token| token_at_word_boundary(&l, token))
}

/// Whether `haystack` contains `token` at a right word boundary.
/// Invariant: `token` must be ASCII (all callers pass ASCII literals).
fn token_at_word_boundary(haystack: &str, token: &str) -> bool {
    debug_assert!(
        token.is_ascii(),
        "token_at_word_boundary requires an ASCII token"
    );
    let mut from = 0;
    while let Some(idx) = haystack.get(from..).and_then(|s| s.find(token)) {
        let end = from + idx + token.len();
        if !haystack
            .get(end..)
            .is_some_and(|s| s.starts_with(|c: char| c.is_ascii_alphanumeric()))
        {
            return true;
        }
        from += idx + 1;
    }
    false
}

#[derive(Clone)]
pub struct McpTool {
    name: String,
    description: String,
    server_name: String,
    mcp_state: Arc<Mutex<McpState>>,
    schema: serde_json::Value,
    meta: Option<serde_json::Value>,
}

/// Data needed to register an MCP tool via `register_erased()`.
/// **Model-visible** (default, or `["model", "app"]`): registered in `ToolBridge` so the LLM can invoke them during a conversation; **App-visible only** (`["app"]`): not registered in `ToolBridge`, so the LLM never sees them. These are UI-only actions surfaced to the frontend via `x.ai/mcp/tools_changed` notifications. They are callable via `x.ai/mcp/call`.
pub struct McpToolRegistration {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub tool: McpErasedTool,
    pub meta: Option<serde_json::Value>,
    pub icons: Vec<McpIcon>,
    pub model_visible: bool,
}

impl McpTool {
    /// Reconstruct an `McpTool` from its constituent parts.
    /// Used when stashing a disabled tool at runtime so it can be re-enabled without a full re-init.
    pub fn new(
        name: String,
        description: String,
        server_name: String,
        mcp_state: Arc<Mutex<McpState>>,
        schema: serde_json::Value,
        meta: Option<serde_json::Value>,
    ) -> Self {
        Self {
            name,
            description,
            server_name,
            mcp_state,
            schema,
            meta,
        }
    }

    /// Convert into the data needed for `ToolBridge::register_erased()`.
    /// Invalid or ambiguous qualified IDs and charset-invalid segments are logged and skipped.
    /// Length of the concatenated `server__tool` key is not limited to the 64-char provider budget.
    pub fn into_registration(self) -> Result<McpToolRegistration, McpToolAdmissionError> {
        let qualified_name =
            qualify_mcp_tool_name(&self.server_name, &self.name).inspect_err(|reason| {
                tracing::error!(
                    server = %self.server_name,
                    tool = %self.name,
                    reason = %reason,
                    "Skipping MCP tool"
                );
            })?;

        let description = self.description.clone();
        let input_schema = self.schema.clone();
        let meta = self.meta.clone();

        let model_visible = meta
            .as_ref()
            .and_then(|m| m.get("ui"))
            .and_then(|ui| ui.get("visibility"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|s| s.as_str() == Some("model")))
            .unwrap_or(true); // default: visible to model

        Ok(McpToolRegistration {
            name: qualified_name,
            description,
            input_schema,
            tool: McpErasedTool { tool: self },
            meta,
            icons: Vec::new(),
            model_visible,
        })
    }
}

/// MCP tool wrapper for runtime dispatch.
///
/// MCP tools are already untyped (JSON in, JSON out), so they implement `distill_tool_runtime::Tool` directly instead of going through typed wrappers.
pub struct McpErasedTool {
    tool: McpTool,
}

impl std::fmt::Debug for McpErasedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpErasedTool")
            .field("name", &self.tool.name)
            .field("server", &self.tool.server_name)
            .finish()
    }
}

impl ToolMetadata for McpErasedTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::MCP
    }

    fn description_template(&self) -> &str {
        &self.tool.description
    }
}

impl distill_tool_runtime::Tool for McpErasedTool {
    type Args = serde_json::Value;
    type Output = ToolOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        // Use the qualified name (server__tool) so that two MCP servers exposing the same raw tool name get distinct LocalRegistry entries
        let qualified = format!(
            "{}{}{}",
            self.tool.server_name, MCP_TOOL_NAME_DELIMITER, self.tool.name
        );
        distill_tool_protocol::ToolId::new(&qualified)
            .unwrap_or_else(|_| distill_tool_protocol::ToolId::new("mcp_tool").expect("valid"))
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(&self.tool.name, &self.tool.description)
    }

    async fn run(
        &self,
        _ctx: distill_tool_runtime::ToolCallContext,
        raw: serde_json::Value,
    ) -> Result<ToolOutput, distill_tool_runtime::ToolError> {
        let call_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
            "mcp.tool_call",
            server_name = %self.tool.server_name,
            tool_name = %self.tool.name,
            reconnect = tracing::field::Empty,
            auth_retry = tracing::field::Empty,
        ));
        let mcp_call_start = std::time::Instant::now();
        let state_lock_span = region!("mcp.state_lock", Parent::Explicit(call_span.span()));
        let (client, event_writer) = {
            let state = self.tool.mcp_state.lock().await;
            let c = Arc::clone(state.get_client(&self.tool.server_name).ok_or_else(|| {
                distill_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!("MCP server '{}' not found", self.tool.server_name),
                )
            })?);
            (c, state.event_writer().clone())
        };
        state_lock_span.close();

        let server = &self.tool.server_name;
        let tool = &self.tool.name;
        let tool_timeout = client.tool_timeout_for(tool);
        let qualified_name = format!("{}{}{}", server, MCP_TOOL_NAME_DELIMITER, tool);
        event_writer.emit(distill_session_events::Event::McpToolCallStarted {
            server_name: server.clone(),
            tool_name: tool.clone(),
            call_id: qualified_name.clone(),
            timeout_sec: tool_timeout,
        });

        let mut auth_retry_attempted = false;
        let mut reconnect_attempted = false;
        let mut is_timeout = false;
        let ew = &event_writer;
        let dispatch_result = match self
            .try_call_tool(
                &client,
                &raw,
                &mut reconnect_attempted,
                &mut is_timeout,
                ew,
                call_span.span(),
            )
            .await
        {
            Ok(result) => Ok(result),
            Err(first_err) if client.has_auth() => {
                auth_retry_attempted = true;
                let reauth_ok = client.force_reauth(false).await;
                ew.emit(distill_session_events::Event::McpAuthRetry {
                    server_name: server.clone(),
                    trigger: "tool_call_failed".to_string(),
                    success: reauth_ok,
                });
                if reauth_ok {
                    self.try_call_tool(
                        &client,
                        &raw,
                        &mut reconnect_attempted,
                        &mut is_timeout,
                        ew,
                        call_span.span(),
                    )
                    .await
                } else {
                    Err(first_err)
                }
            }
            Err(e) => Err(e),
        };
        call_span.span().record("reconnect", reconnect_attempted);
        call_span.span().record("auth_retry", auth_retry_attempted);
        call_span.close();

        // Protocol, init and input errors keep `failure` unset: only the server's silence is counted.
        let failure = match &dispatch_result {
            Ok(_) => None,
            Err(_) if is_timeout => Some(distill_telemetry::events::McpCallFailure::Timeout),
            Err(e) if e.kind == distill_tool_runtime::ToolErrorKind::NetworkError => {
                Some(distill_telemetry::events::McpCallFailure::Transport)
            }
            Err(_) => None,
        };
        // Telemetry labels a server may put in `structuredContent`; read before the content moves.
        let (outcome, mode) = match &dispatch_result {
            Ok(call_result) => {
                let structured = call_result.structured_content.as_ref();
                (
                    structured_label(structured, "outcome"),
                    structured_label(structured, "mode"),
                )
            }
            Err(_) => (None, None),
        };
        let result = dispatch_result.map(|call_result| {
            let mut mcp_out = mcp_output_from_call_result(
                tool.clone(),
                server.clone(),
                call_result,
                client.expose_image_base64(),
            );
            mcp_out.auth_retry_attempted = auth_retry_attempted;
            mcp_out.reconnect_attempted = reconnect_attempted;
            mcp_out.is_timeout = is_timeout;
            ToolOutput::MCP(mcp_out)
        });

        let (success, error_text) = match &result {
            Ok(ToolOutput::MCP(mcp)) => match mcp.output() {
                MCPOutputDetails::Error(e) => (false, Some(e.clone())),
                _ => (true, None),
            },
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        let duration_ms = mcp_call_start.elapsed().as_millis() as u64;
        event_writer.emit(distill_session_events::Event::McpToolCallCompleted {
            server_name: server.clone(),
            tool_name: tool.clone(),
            call_id: qualified_name.clone(),
            duration_ms,
            success,
            is_timeout,
            error: error_text,
            reconnect_attempted,
            auth_retry_attempted,
        });
        distill_telemetry::session_ctx::log_event(distill_telemetry::events::McpToolCalled {
            server_name: server.clone(),
            tool_name: tool.clone(),
            qualified_name,
            success,
            duration_ms,
            failure,
            outcome,
            mode,
        });
        result
    }
}

/// Caps server-provided telemetry labels at 64 bytes, leaving headroom above known codes
/// such as `permission_required`. This is a conservative policy choice, not a protocol limit;
/// raising it forwards longer server strings to telemetry.
const STRUCTURED_LABEL_MAX_LEN: usize = 64;

/// A label-shaped string under `key` in a result's `structuredContent`, for telemetry.
/// Only `[A-Za-z0-9_.-]` strings count: an object or number there is data for the model,
/// and free text (an email, a name, a token) must not reach the analytics sinks.
fn structured_label(structured: Option<&serde_json::Value>, key: &str) -> Option<String> {
    structured?
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= STRUCTURED_LABEL_MAX_LEN)
        .filter(|s| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        })
        .map(str::to_owned)
}

/// Check whether a `ServiceError` indicates the underlying transport has died and a fresh connection could recover it.
fn is_retriable_transport_error(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::TransportClosed | ServiceError::TransportSend(_)
    )
}

/// The `ToolError` for a `tools/call` the service could not complete. A dead transport is
/// typed `NetworkError` so the caller can tell it from protocol and input errors.
fn tool_error_for_service_error(err: &ServiceError) -> distill_tool_runtime::ToolError {
    if is_retriable_transport_error(err) {
        distill_tool_runtime::ToolError::network_error(err.to_string())
    } else {
        distill_tool_runtime::ToolError::custom("process_manager", err.to_string())
    }
}

/// Recover for every JSON-RPC code except the deterministic client set {-32700, -32600, -32601, -32602}.
/// Those mean the request was wrong, not the session. -32021/-32022 stay recoverable on purpose:
/// a 2026-07-28-era server sends them to a legacy session, and the re-handshake's
/// `server/discover` probe upgrades it.
fn should_recover_mcp_error(code: i32) -> bool {
    use rmcp::model::ErrorCode;
    let deterministic_client_error = code == ErrorCode::PARSE_ERROR.0
        || code == ErrorCode::INVALID_REQUEST.0
        || code == ErrorCode::METHOD_NOT_FOUND.0
        || code == ErrorCode::INVALID_PARAMS.0;
    !deterministic_client_error
}

/// Recovers transport errors, and an HTTP `McpError` once per dispatch except deterministic client codes and auth-class errors.
/// A rebuild reuses stale creds, so auth is routed to the re-auth paths instead.
fn should_recover_service_error(
    err: &ServiceError,
    is_http: bool,
    reconnect_attempted: bool,
) -> bool {
    is_retriable_transport_error(err)
        || matches!(
            err,
            ServiceError::McpError(e)
                if is_http
                    && !reconnect_attempted
                    && should_recover_mcp_error(e.code.0)
                    && !is_auth_rejection_message(e.message.as_ref())
        )
}

/// How long a tool call may stay parked on `requestState`-only `input_required` rounds while an out-of-band URL elicitation completes in the browser. Matches the crate's browser OAuth deadline ([`crate::oauth`]'s `BROWSER_AUTH_TIMEOUT`): both wait on the same kind of user journey through an external page.
const MRTR_PENDING_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Poll backoff for `requestState`-only rounds: 250ms doubling to a 5s ceiling.
fn mrtr_pending_poll_delay(pending_polls: u32) -> std::time::Duration {
    std::time::Duration::from_millis((250u64.saturating_mul(1 << pending_polls.min(5))).min(5_000))
}

impl McpErasedTool {
    /// Dispatch one `tools/call`, driving SEP-2322 multi round-trip requests (protocol 2026-07-28).
    /// The user-facing input gathering between rounds is deliberately not on the clock
    /// rounds that carry `inputRequests` are interactive re-prompts, capped by count ([`rmcp::model::DEFAULT_MRTR_MAX_ROUNDS`]); rounds carrying only `requestState` mean an out-of-band interaction (URL elicitation) is still pending server-side, so they are polled with capped backoff and bounded by [`MRTR_PENDING_STATE_TIMEOUT`] instead — a browser flow takes minutes, not rounds.
    async fn try_call_tool(
        &self,
        client: &Arc<McpClient>,
        raw: &serde_json::Value,
        reconnect_attempted: &mut bool,
        is_timeout: &mut bool,
        ew: &distill_session_events::EventWriter,
        call_span: &tracing::Span,
    ) -> Result<rmcp::model::CallToolResult, distill_tool_runtime::ToolError> {
        let tool_timeout = client.tool_timeout_for(&self.tool.name);
        let timeout_duration = std::time::Duration::from_secs(tool_timeout);
        let mut params = CallToolRequestParams::new(self.tool.name.clone());
        params.arguments = raw.as_object().cloned();

        let mut input_rounds = 0usize;
        let mut pending_polls = 0u32;
        let mut pending_since: Option<std::time::Instant> = None;
        let mut attempt: i64 = 0;
        loop {
            attempt += 1;
            let attempt_span = distill_telemetry::region::Region::from_span(
                tracing::info_span!(parent: call_span, "mcp.call_attempt", attempt),
            );
            let response = self
                .call_tool_round(
                    client,
                    params.clone(),
                    timeout_duration,
                    tool_timeout,
                    reconnect_attempted,
                    is_timeout,
                    ew,
                )
                .await?;
            attempt_span.close();
            let input_required = match response {
                rmcp::model::CallToolResponse::Complete(call_result) => return Ok(call_result),
                rmcp::model::CallToolResponse::InputRequired(input_required) => input_required,
                // SEP-2663 tasks are never advertised by this client, so a conforming server
                // cannot return one; `CallToolResponse` is also non_exhaustive.
                _ => {
                    return Err(distill_tool_runtime::ToolError::custom(
                        "process_manager",
                        format!(
                            "MCP tool '{}' returned an unsupported response kind",
                            self.tool.name
                        ),
                    ));
                }
            };

            let has_input_requests = input_required
                .input_requests
                .as_ref()
                .is_some_and(|requests| !requests.is_empty());
            if has_input_requests {
                input_rounds += 1;
                if input_rounds > rmcp::model::DEFAULT_MRTR_MAX_ROUNDS {
                    return Err(distill_tool_runtime::ToolError::custom(
                        "process_manager",
                        format!(
                            "MCP tool '{}' kept requiring input beyond {} rounds",
                            self.tool.name,
                            rmcp::model::DEFAULT_MRTR_MAX_ROUNDS
                        ),
                    ));
                }
                pending_since = None;
                pending_polls = 0;
            } else {
                let started = *pending_since.get_or_insert_with(std::time::Instant::now);
                if started.elapsed() >= MRTR_PENDING_STATE_TIMEOUT {
                    return Err(distill_tool_runtime::ToolError::custom(
                        "process_manager",
                        format!(
                            "MCP tool '{}' still awaited an out-of-band interaction after {}s",
                            self.tool.name,
                            MRTR_PENDING_STATE_TIMEOUT.as_secs()
                        ),
                    ));
                }
                tokio::time::sleep(mrtr_pending_poll_delay(pending_polls)).await;
                pending_polls = pending_polls.saturating_add(1);
            }

            let (input_responses, request_state) =
                self.gather_input_responses(client, input_required).await?;
            params.input_responses = input_responses;
            params.request_state = request_state;
        }
    }

    /// One wire round of [`Self::try_call_tool`]: timeout, transport recovery, and error classification apply per round trip. The live session is re-resolved every round because recovery (and re-initialization after long elicitation waits) replaces the client's `RunningService` between rounds.
    #[allow(clippy::too_many_arguments)]
    async fn call_tool_round(
        &self,
        client: &Arc<McpClient>,
        params: CallToolRequestParams,
        timeout_duration: std::time::Duration,
        tool_timeout: u64,
        reconnect_attempted: &mut bool,
        is_timeout: &mut bool,
        ew: &distill_session_events::EventWriter,
    ) -> Result<rmcp::model::CallToolResponse, distill_tool_runtime::ToolError> {
        let mcp_service = client
            .ensure_initialized()
            .await
            .map_err(|e| distill_tool_runtime::ToolError::custom("process_manager", e.to_string()))?;
        match call_tool_cancel_aware(&mcp_service, params.clone(), timeout_duration).await {
            Ok(response) => Ok(response),
            Err(ServiceError::Timeout { .. }) => {
                *is_timeout = true;
                // Reset for the next call but don't retry: a slow side-effecting tool must not run twice
                if client.is_http() && !*reconnect_attempted {
                    client.reset_transport().await;
                    *reconnect_attempted = true;
                }
                Err(distill_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!(
                        "MCP tool '{}' timed out after {} seconds",
                        self.tool.name, tool_timeout
                    ),
                ))
            }
            Err(service_err)
                if should_recover_service_error(
                    &service_err,
                    client.is_http(),
                    *reconnect_attempted,
                ) =>
            {
                self.recover_and_retry(
                    client,
                    params,
                    timeout_duration,
                    tool_timeout,
                    service_err,
                    reconnect_attempted,
                    is_timeout,
                    ew,
                )
                .await
            }
            Err(e) => Err(tool_error_for_service_error(&e)),
        }
    }

    /// Gather responses for an `input_required` round and hand back the opaque `requestState`
    /// to echo verbatim on the retry.
    async fn gather_input_responses(
        &self,
        client: &Arc<McpClient>,
        result: rmcp::model::InputRequiredResult,
    ) -> Result<(Option<rmcp::model::InputResponses>, Option<String>), distill_tool_runtime::ToolError>
    {
        let input_requests = result.input_requests.unwrap_or_default();
        if input_requests.is_empty() && result.request_state.is_none() {
            return Err(distill_tool_runtime::ToolError::custom(
                "process_manager",
                format!(
                    "MCP tool '{}' returned input_required with neither inputRequests nor requestState",
                    self.tool.name
                ),
            ));
        }

        let mut responses = rmcp::model::InputResponses::new();
        for (key, request) in input_requests {
            match request {
                rmcp::model::InputRequest::Elicitation(elicit) => {
                    tracing::info!(
                        server = %self.tool.server_name,
                        tool = %self.tool.name,
                        "MCP elicitation received in input_required round"
                    );
                    let elicit_result = client.bridge_elicit(elicit.params).await;
                    let value = serde_json::to_value(elicit_result).map_err(|e| {
                        distill_tool_runtime::ToolError::custom(
                            "process_manager",
                            format!("failed to serialize elicitation response: {e}"),
                        )
                    })?;
                    responses.insert(key, value);
                }
                // Sampling and roots are never advertised in our client capabilities,
                // so a conforming server cannot request them (spec: servers MUST NOT).
                _ => {
                    return Err(distill_tool_runtime::ToolError::custom(
                        "process_manager",
                        format!(
                            "MCP server '{}' requested an unsupported input kind ('{key}'); \
                             this client only supports elicitation",
                            self.tool.server_name
                        ),
                    ));
                }
            }
        }

        Ok((
            (!responses.is_empty()).then_some(responses),
            result.request_state,
        ))
    }

    /// On `recover()` failure surface the original error, else the retry error (preserves the auth signal managed re-auth reads from the string).
    #[allow(clippy::too_many_arguments)]
    async fn recover_and_retry(
        &self,
        client: &Arc<McpClient>,
        params: CallToolRequestParams,
        timeout_duration: std::time::Duration,
        tool_timeout: u64,
        original_err: ServiceError,
        reconnect_attempted: &mut bool,
        is_timeout: &mut bool,
        ew: &distill_session_events::EventWriter,
    ) -> Result<rmcp::model::CallToolResponse, distill_tool_runtime::ToolError> {
        *reconnect_attempted = true;
        tracing::warn!(
            server = self.tool.server_name.as_str(),
            tool = self.tool.name.as_str(),
            error = %original_err,
            "MCP transport error, attempting reconnect"
        );
        ew.emit(distill_session_events::Event::McpTransportError {
            server_name: self.tool.server_name.clone(),
            tool_name: self.tool.name.clone(),
            error: original_err.to_string(),
        });
        let mcp_service = match client.recover().await {
            Ok(service) => {
                ew.emit(distill_session_events::Event::McpTransportReconnect {
                    server_name: self.tool.server_name.clone(),
                    success: true,
                    error: None,
                });
                service
            }
            Err(e) => {
                ew.emit(distill_session_events::Event::McpTransportReconnect {
                    server_name: self.tool.server_name.clone(),
                    success: false,
                    error: Some(e.to_string()),
                });
                return Err(tool_error_for_service_error(&original_err));
            }
        };
        match call_tool_cancel_aware(&mcp_service, params, timeout_duration).await {
            Ok(response) => Ok(response),
            Err(ServiceError::Timeout { .. }) => {
                *is_timeout = true;
                Err(distill_tool_runtime::ToolError::custom(
                    "process_manager",
                    format!(
                        "MCP tool '{}' timed out after {} seconds",
                        self.tool.name, tool_timeout
                    ),
                ))
            }
            Err(retry_err) => Err(tool_error_for_service_error(&retry_err)),
        }
    }
}

/// One `tools/call` that tells the server when the host stops waiting for it.
///
/// Two abandonment paths send `notifications/cancelled` for the request id: rmcp sends it itself
/// when the tool timeout elapses (surfaced as [`ServiceError::Timeout`]), and [`CancelOnDrop`]
/// sends it when the future is dropped before a reply, which is how a cancelled turn (Esc)
/// reaches the server. A server that honors the notification can stop the work; one that
/// ignores it sees no other change.
async fn call_tool_cancel_aware(
    service: &McpService,
    params: CallToolRequestParams,
    timeout: std::time::Duration,
) -> Result<rmcp::model::CallToolResponse, ServiceError> {
    use rmcp::model::{CallToolRequest, CallToolResponse, ClientRequest, ServerResult};
    use rmcp::service::PeerRequestOptions;

    let handle = service
        .peer()
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::with_timeout(timeout),
        )
        .await?;
    let mut guard = CancelOnDrop {
        peer: service.peer().clone(),
        request_id: Some(handle.id.clone()),
    };
    let response = handle.await_response().await;
    // Settled either way; a cancel now would be noise.
    guard.request_id = None;
    match response? {
        ServerResult::CallToolResult(result) => Ok(CallToolResponse::Complete(result)),
        ServerResult::InputRequiredResult(result) => Ok(CallToolResponse::InputRequired(result)),
        ServerResult::CreateTaskResult(result) => Ok(CallToolResponse::Task(result)),
        _ => Err(ServiceError::UnexpectedResponse),
    }
}

/// Sends `notifications/cancelled` for an in-flight request when its future is dropped before
/// the reply arrived. Disarmed (`request_id = None`) once the request settles.
struct CancelOnDrop {
    peer: rmcp::service::Peer<RoleClient>,
    request_id: Option<rmcp::model::RequestId>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let Some(request_id) = self.request_id.take() else {
            return;
        };
        // Drop is synchronous; the notification goes out from a spawned task.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                ?request_id,
                "MCP call dropped outside a runtime; cancel not sent"
            );
            return;
        };
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let params = rmcp::model::CancelledNotificationParam::new(
                Some(request_id.clone()),
                Some("client cancelled".to_owned()),
            );
            if let Err(e) = peer.notify_cancelled(params).await {
                tracing::debug!(?request_id, error = %e, "notifications/cancelled not delivered");
            }
        });
    }
}

/// Whether this session can complete an interactive (browser) OAuth flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OauthInteractivity {
    Interactive,
    NonInteractive,
}

impl OauthInteractivity {
    /// Headless/SDK sessions set `non_interactive = true` and cannot complete browser OAuth.
    pub fn from_non_interactive(non_interactive: bool) -> Self {
        if non_interactive {
            Self::NonInteractive
        } else {
            Self::Interactive
        }
    }
}

enum OauthProbeOutcome {
    Resolved(HttpAuthDecision),
    Inconclusive,
}

impl OauthProbeOutcome {
    fn on_probe_failure(mode: OauthInteractivity) -> Self {
        match mode {
            OauthInteractivity::Interactive => Self::Resolved(HttpAuthDecision::NoOauthSupport),
            OauthInteractivity::NonInteractive => Self::Inconclusive,
        }
    }
}

async fn stored_token_disk_gate(
    server_name: &str,
    server_url: &str,
    mode: OauthInteractivity,
) -> Option<OauthProbeOutcome> {
    let HttpAuthDecision::ManagerReady { manager, observed } =
        decide_http_auth_from_disk(server_name, server_url).await
    else {
        return None;
    };
    match mode {
        OauthInteractivity::Interactive => Some(OauthProbeOutcome::Resolved(
            HttpAuthDecision::ManagerReady { manager, observed },
        )),
        OauthInteractivity::NonInteractive => {
            let usable = manager.lock().await.get_access_token().await;
            match usable {
                Ok(_) => Some(OauthProbeOutcome::Resolved(
                    HttpAuthDecision::ManagerReady { manager, observed },
                )),
                Err(e) => {
                    tracing::info!(
                        server = server_name,
                        error = %e,
                        "Stored token not usable without hydration; continuing to the network probe"
                    );
                    None
                }
            }
        }
    }
}

async fn discover_and_prepare_auth(
    server_name: &str,
    server_url: &str,
    mode: OauthInteractivity,
) -> OauthProbeOutcome {
    if let Some(outcome) = stored_token_disk_gate(server_name, server_url, mode).await {
        return outcome;
    }

    let Ok(parsed_url) = url::Url::parse(server_url) else {
        return OauthProbeOutcome::Resolved(HttpAuthDecision::NoOauthSupport);
    };
    let adapter =
        crate::credentials::McpCredentialStoreAdapter::new(server_name.to_string(), parsed_url);
    let observed = adapter.observed();

    let mut manager = match rmcp::transport::auth::AuthorizationManager::new(server_url).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(server = server_name, %e, "Failed to create OAuth manager");
            return OauthProbeOutcome::on_probe_failure(mode);
        }
    };
    manager.set_credential_store(adapter);

    let ready = crate::oauth::ensure_oauth_ready(server_name, &mut manager).await;
    if ready.hydrated {
        if mode == OauthInteractivity::NonInteractive
            && let Err(e) = manager.get_access_token().await
        {
            tracing::warn!(
                server = server_name,
                error = %e,
                "Skipping OAuth MCP in non-interactive mode (stored credentials unusable); re-authenticate in TUI"
            );
            return OauthProbeOutcome::Resolved(HttpAuthDecision::NeedsInteractiveLogin);
        }
        tracing::info!(server = server_name, "Loaded stored OAuth credentials");
        return OauthProbeOutcome::Resolved(HttpAuthDecision::ManagerReady {
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            observed,
        });
    }

    match ready.discovery {
        Ok(()) => {
            if mode == OauthInteractivity::NonInteractive {
                tracing::warn!(
                    server = server_name,
                    "Skipping OAuth MCP in non-interactive mode (no stored tokens); authenticate in TUI or set an Authorization header"
                );
                return OauthProbeOutcome::Resolved(HttpAuthDecision::NeedsInteractiveLogin);
            }
            tracing::info!(
                server = server_name,
                "Server supports OAuth but has no stored tokens"
            );
            OauthProbeOutcome::Resolved(HttpAuthDecision::ManagerReady {
                manager: Arc::new(tokio::sync::Mutex::new(manager)),
                observed,
            })
        }
        Err(rmcp::transport::auth::AuthError::NoAuthorizationSupport) => {
            tracing::debug!(server = server_name, "Server does not support OAuth");
            OauthProbeOutcome::Resolved(HttpAuthDecision::NoOauthSupport)
        }
        Err(e) => {
            tracing::warn!(server = server_name, %e, "OAuth discovery failed");
            OauthProbeOutcome::on_probe_failure(mode)
        }
    }
}

/// Whether an MCP server answers a request that carries no credentials.
enum AnonymousAccess {
    Accepted,
    AuthChallenged,
    Unreachable,
}

/// One POST to the MCP endpoint, judged by status class only.
/// Not a GET, because streamable-http servers legally answer GET with a never-ending SSE stream, which is what hangs discovery.
async fn probe_anonymous_access(
    server_name: &str,
    url: &str,
    headers: &[(String, String)],
) -> AnonymousAccess {
    // Redirects are not followed: a gateway that redirects an anonymous POST to a login page is challenging, not accepting
    // reqwest 0.13; the policy chokepoint is typed for 0.12 and cannot wrap this builder.
    #[allow(clippy::disallowed_methods)]
    let client = match with_extra_root_certificates(reqwest::Client::builder())
        .timeout(ANONYMOUS_ACCESS_PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(server = server_name, error = %e, "anonymous-access probe: building HTTP client failed");
            return AnonymousAccess::Unreachable;
        }
    };
    // The real transport sends the configured headers (e.g. `X-Api-Key`); the probe must too, or header-authenticated servers would fail closed.
    // Authorization is known absent on this path
    let mut probe_headers = parse_config_headers(
        server_name,
        "anonymous-probe",
        headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    );
    // Protocol-required values win over configured ones.
    probe_headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    probe_headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json, text/event-stream"),
    );
    apply_user_agent_policy(&mut probe_headers, server_name, url);
    let request = client.post(url).headers(probe_headers).body("{}");
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            // 407 signals proxy credentials rather than server auth, but those are still credentials this session cannot supply headlessly
            if status == reqwest::StatusCode::UNAUTHORIZED
                || status == reqwest::StatusCode::FORBIDDEN
                || status == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED
                || status.is_redirection()
            {
                AnonymousAccess::AuthChallenged
            } else if status.is_server_error() {
                // Often an ingress/proxy blip that says nothing about auth; fail closed.
                AnonymousAccess::Unreachable
            } else {
                // Any other response (even a 4xx complaint about the `{}` body) proves the server answers unauthenticated requests
                AnonymousAccess::Accepted
            }
        }
        Err(e) => {
            tracing::warn!(server = server_name, error = %e, "anonymous-access probe failed");
            AnonymousAccess::Unreachable
        }
    }
}

async fn decide_http_auth_over_network(
    server_name: &str,
    url: &str,
    headers: &[(String, String)],
    ctx: &McpSpawnCtx<'_>,
    discovery_timeout: std::time::Duration,
) -> HttpAuthDecision {
    // No timer around the discovery await: `InstrumentationTimer` holds a
    // Chrome-mode span guard that must not cross an await (`!Send`, and
    // tracing's span stack is per-thread).
    let outcome = {
        match tokio::time::timeout(
            discovery_timeout,
            discover_and_prepare_auth(server_name, url, ctx.mode),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                tracing::warn!(
                    server = server_name,
                    url = %url,
                    mode = ?ctx.mode,
                    timeout_secs = discovery_timeout.as_secs(),
                    "OAuth discovery timed out"
                );
                ctx.event_writer
                    .emit(distill_session_events::Event::McpOAuthDiscoveryTimeout {
                        server_name: server_name.to_string(),
                        url: url.to_string(),
                    });
                OauthProbeOutcome::on_probe_failure(ctx.mode)
            }
        }
    };
    match outcome {
        OauthProbeOutcome::Resolved(decision) => decision,
        OauthProbeOutcome::Inconclusive => {
            let (verdict, decision) = match probe_anonymous_access(server_name, url, headers).await
            {
                AnonymousAccess::Accepted => {
                    tracing::info!(
                        server = server_name,
                        "OAuth discovery was inconclusive but the server accepts unauthenticated requests; connecting without auth"
                    );
                    ("accepted", HttpAuthDecision::NoOauthSupport)
                }
                AnonymousAccess::AuthChallenged => {
                    tracing::warn!(
                        server = server_name,
                        "OAuth discovery was inconclusive and the server challenges unauthenticated requests; authenticate in TUI or set an Authorization header"
                    );
                    ("auth_challenged", HttpAuthDecision::NeedsInteractiveLogin)
                }
                AnonymousAccess::Unreachable => {
                    tracing::warn!(
                        server = server_name,
                        "OAuth discovery was inconclusive and the anonymous-access probe could not reach the server; failing closed as unreachable (retryable)"
                    );
                    ("unreachable", HttpAuthDecision::Unreachable)
                }
            };
            ctx.event_writer
                .emit(distill_session_events::Event::McpOAuthProbeResolved {
                    server_name: server_name.to_string(),
                    verdict: verdict.to_string(),
                });
            decision
        }
    }
}

/// Configuration for HTTP MCP server connection.
#[derive(Clone)]
pub struct HttpConfig {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// This server is a first-party local app endpoint addressed by [`GROK_AGENT_ID_HEADER`]. Set only from the spawn context — never inferred from headers — and it keys the transport hardening (no proxy, no redirects) and the OAuth skip.
    pub local_agent_endpoint: bool,
}

impl HttpConfig {
    fn has_authorization_header(&self) -> bool {
        self.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
    }
}

/// Newline-delimited JSON-RPC stdio transport whose read side survives a single undecodable line.
/// We read lines ourselves (rather than via `FramedRead` and rmcp's codec) so reading continues after a bad line.
/// A stray non-JSON stdout line, a JSON-RPC batch array, or an off-spec response therefore never collapses the transport.
struct ResilientRwTransport<R, W>
where
    R: AsyncRead,
    W: AsyncWrite,
{
    read: BufReader<R>,
    /// `Arc<Mutex<Option<…>>>` so `send` can return a `Send + 'static` future (the `Transport` contract) without borrowing `self`.
    /// It also lets `close` drop the writer; mirrors rmcp's own `AsyncRwTransport`.
    write: Arc<Mutex<Option<W>>>,
    server_name: String,
    event_writer: distill_session_events::EventWriter,
}

/// Max bytes of an offending line copied into the decode-error event.
const DECODE_ERROR_SAMPLE_LEN: usize = 200;

/// A line that failed to deserialize but is a JSON *notification* (an object with a `method` and no `id`) is benign.
/// Many servers emit non-MCP / unknown notifications (e.g. LSP-style).
/// Skip those quietly instead of flagging a decode error, mirroring rmcp's compatibility handling.
fn is_ignorable_notification(line: &[u8]) -> bool {
    match serde_json::from_slice::<serde_json::Value>(line) {
        Ok(v) => v.get("id").is_none() && v.get("method").and_then(|m| m.as_str()).is_some(),
        Err(_) => false,
    }
}

impl<R, W> ResilientRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    fn new(
        read: R,
        write: W,
        server_name: String,
        event_writer: distill_session_events::EventWriter,
    ) -> Self {
        Self {
            read: BufReader::new(read),
            write: Arc::new(Mutex::new(Some(write))),
            server_name,
            event_writer,
        }
    }

    /// Record a skipped, undecodable stdout line: a `warn!` log plus an `McpTransportDecodeError` event.
    /// The event carries the serde error and a truncated sample of the raw line (the diagnostic the untagged-enum serde error alone lacks).
    fn record_decode_error(&self, line: &[u8], err: &serde_json::Error) {
        let sample: String = String::from_utf8_lossy(line)
            .chars()
            .take(DECODE_ERROR_SAMPLE_LEN)
            .collect();
        tracing::warn!(
            server = %self.server_name,
            error = %err,
            sample = %sample,
            "Skipping undecodable MCP stdout line; keeping transport alive",
        );
        self.event_writer
            .emit(distill_session_events::Event::McpTransportDecodeError {
                server_name: self.server_name.clone(),
                error: err.to_string(),
                sample,
            });
    }
}

impl<R, W> Transport<RoleClient> for ResilientRwTransport<R, W>
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let lock = self.write.clone();
        async move {
            let mut bytes = serde_json::to_vec(&item).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            let mut guard = lock.lock().await;
            match guard.as_mut() {
                Some(write) => {
                    write.write_all(&bytes).await?;
                    write.flush().await
                }
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "transport is closed",
                )),
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        loop {
            let mut line = Vec::new();
            match self.read.read_until(b'\n', &mut line).await {
                Ok(0) => return None, // genuine end-of-stream
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(
                        server = %self.server_name,
                        error = %e,
                        "MCP stdio read error; closing transport",
                    );
                    return None;
                }
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }

            match serde_json::from_slice::<RxJsonRpcMessage<RoleClient>>(&line) {
                Ok(msg) => return Some(msg),
                // The whole point: a single undecodable line must not collapse the transport; skip it and keep reading
                Err(err) => {
                    if is_ignorable_notification(&line) {
                        tracing::trace!(
                            server = %self.server_name,
                            "Ignoring unrecognized MCP notification",
                        );
                    } else {
                        self.record_decode_error(&line, &err);
                    }
                    continue;
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        let mut guard = self.write.lock().await;
        drop(guard.take());
        Ok(())
    }
}

/// Stdio MCP transport with a non-panicking cleanup path.
/// The child therefore never leaks as a zombie.
pub struct SafeTokioChildProcess {
    child: Option<tokio::process::Child>,
    /// Strong `Arc` owner; the scope holds only a `Weak`, dropped on reap.
    process_group: Option<Arc<ProcessGroup>>,
    transport: ResilientRwTransport<tokio::process::ChildStdout, tokio::process::ChildStdin>,
}

/// Holds a newly launched stdio child and its process group until ownership moves to [`SafeTokioChildProcess`].
/// If the launch is cancelled partway (the session is closing), this value is still dropped.
/// The caller sets `kill_on_drop(true)` so the child itself is also cleaned up on that path.
struct SpawnGuard {
    child: Option<tokio::process::Child>,
    process_group: Option<Arc<ProcessGroup>>,
}

impl SpawnGuard {
    fn new(child: tokio::process::Child, process_group: Option<Arc<ProcessGroup>>) -> Self {
        Self {
            child: Some(child),
            process_group,
        }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child
            .as_mut()
            .expect("guard child is present until disarm")
    }

    fn process_group(&self) -> Option<&Arc<ProcessGroup>> {
        self.process_group.as_ref()
    }

    fn disarm(mut self) -> (tokio::process::Child, Option<Arc<ProcessGroup>>) {
        (
            self.child
                .take()
                .expect("guard child is present until disarm"),
            self.process_group.take(),
        )
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if let Some(group) = &self.process_group
            && let Err(e) = group.kill()
        {
            tracing::warn!("Error killing MCP child process group on spawn cancel: {e}");
        }
    }
}

impl SafeTokioChildProcess {
    /// `server_name` and `event_writer` are passed to the transport.
    /// A skipped unreadable output line then reports an `McpTransportDecodeError` event for that server.
    /// `scope`, when set, registers the child's group so it is cleaned up when the session closes.
    async fn spawn(
        mut cmd: Command,
        scope: Option<&ProcessScope>,
        server_name: String,
        event_writer: distill_session_events::EventWriter,
    ) -> std::io::Result<(Self, Option<ChildStderr>)> {
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Launch the child on a background thread so the session thread is never blocked
        // Take ownership of its group in that same step so the returned guard owns the child from the moment it exists
        // See `SpawnGuard` for what happens if the launch is cancelled
        let mut guard = tokio::task::spawn_blocking(move || -> std::io::Result<SpawnGuard> {
            #[allow(clippy::disallowed_methods)] // group ownership is taken below, in this task
            let child = cmd.spawn()?;
            // Best effort: without a group we can still clean up the child itself.
            let process_group = match ProcessGroup::new() {
                Ok(mut group) => match group.attach(&child) {
                    Ok(()) => Some(Arc::new(group)),
                    Err(e) => {
                        tracing::warn!("Failed to attach MCP child to process group: {e}");
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!("Failed to create MCP child process group: {e}");
                    None
                }
            };
            Ok(SpawnGuard::new(child, process_group))
        })
        .await
        .map_err(|e| std::io::Error::other(format!("MCP spawn task failed: {e}")))??;

        // The guard stays active through the failures below, so any early exit still cleans up the whole group instead of leaking it
        let child = guard.child_mut();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("stdin was already taken"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("stdout was already taken"))?;
        let stderr = child.stderr.take();
        // Tie this child to the spawning session: once that session closes, the child is cleaned up even if another session still holds the client
        let scope_closed = match (scope, guard.process_group()) {
            (Some(scope), Some(group)) => !scope.register(group),
            _ => false,
        };
        if scope_closed {
            // `register` already stopped the group when it found the session already closing
            // So disarm the guard and fail fast rather than let its Drop stop the group a second time
            let _ = guard.disarm();
            return Err(std::io::Error::other(
                "session is closing (process scope already reclaimed); MCP server not started",
            ));
        }

        let (child, process_group) = guard.disarm();
        Ok((
            Self {
                child: Some(child),
                process_group,
                transport: ResilientRwTransport::new(stdout, stdin, server_name, event_writer),
            },
            stderr,
        ))
    }

    fn id(&self) -> Option<u32> {
        self.child.as_ref()?.id()
    }

    /// Force-stops the whole group: the child and anything it started.
    /// It is synchronous, so it can run from `Drop` without a runtime; the child itself still needs to be waited on afterwards.
    fn kill_process_group(&self) {
        if let Some(group) = &self.process_group
            && let Err(e) = group.kill()
        {
            tracing::warn!("Error killing MCP child process group: {e}");
        }
    }

    async fn graceful_shutdown(&mut self) -> std::io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        self.transport.close().await?;

        let result = tokio::select! {
            _ = tokio::time::sleep(STDIO_SHUTDOWN_GRACE) => {
                self.kill_process_group();
                match child.kill().await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::warn!("Error killing MCP child: {e}");
                        Err(e)
                    }
                }
            }
            res = child.wait() => {
                // Clean up anything the child started now, while the group id is still in use, and before the child's id can be reused
                self.kill_process_group();
                match res {
                    Ok(status) => {
                        tracing::info!("MCP child exited gracefully {status}");
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!("Error waiting for MCP child: {e}");
                        Err(e)
                    }
                }
            }
        };

        // Leader is reaped; drop the group so `Drop` can't `killpg` a now-reusable pid.
        self.process_group = None;
        result
    }
}

impl Drop for SafeTokioChildProcess {
    fn drop(&mut self) {
        // Group teardown is synchronous, so do it first regardless of runtime.
        self.kill_process_group();

        let Some(mut child) = self.child.take() else {
            return;
        };

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = child.kill().await {
                    tracing::warn!("Error killing MCP child process: {e}");
                }
            });
        } else if let Err(e) = std::thread::Builder::new()
            .name("mcp-stdio-child-cleanup".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("Error creating runtime to clean up MCP child process: {e}");
                        if let Err(e) = child.start_kill() {
                            tracing::warn!("Error signaling MCP child process during drop: {e}");
                        }
                        return;
                    }
                };

                rt.block_on(async move {
                    if let Err(e) = child.kill().await {
                        tracing::warn!("Error killing MCP child process: {e}");
                    }
                });
            })
        {
            tracing::warn!("Error spawning MCP child cleanup thread: {e}");
        }
    }
}

impl Transport<RoleClient> for SafeTokioChildProcess {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.graceful_shutdown().await
    }
}

/// Transport configuration before connection is established.
/// Outcome of the `server/discover` probe phase (see `McpClient::probe_modern`).
enum ProbeVerdict {
    /// Modern negotiation succeeded; this running service IS the connection.
    Modern(Box<rmcp::service::RunningService<RoleClient, GrokClientHandler>>),
    /// The server was reached but is not a usable modern server; run the
    /// legacy handshake. The probe's error is kept so a subsequent legacy
    /// failure can log both phases together.
    Legacy { probe_error: String },
}

enum PendingTransport {
    Stdio(Box<SafeTokioChildProcess>),
    Http(HttpConfig),
    HttpAuth {
        config: HttpConfig,
        auth_manager: Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
    },
    /// In-process SDK MCP server reached over the ACP reverse channel (`x.ai/mcp/sdk_call`).
    /// Rebuildable from its `server_id` and invoker, so handshake failures restore like Http (unlike the consumed Stdio child).
    Acp {
        server_id: String,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
    },
}

/// A connected MCP service (rmcp's RunningService wrapped in Arc).
/// Uses [`GrokClientHandler`] rather than rmcp's default `ClientInfo` handler.
/// rmcp 2.1 parameterizes `RunningService` over the handler type, and `ClientInfo` is only a `ClientHandler` impl with no notification routing.
pub type McpService = Arc<RunningService<RoleClient, GrokClientHandler>>;

pub(crate) static MCP_SERVERS_CONNECTED: distill_telemetry::activity::ActivityGauge =
    distill_telemetry::activity::ActivityGauge::residency(
        distill_telemetry::activity::MCP_SERVERS_CONNECTED_KEY,
    );

/// MCP client connection state machine.
/// Single-flight handshake invariant: at most one task at a time may run the handshake.
enum ClientState {
    /// [`McpClient::stub`] (test placeholder; `ensure_initialized` returns a configuration error); Stdio handshake failure: the spawned child process is consumed by `client.serve` and cannot be reused. Http/HttpAuth keep their `HttpConfig` clone and transition back to `Pending`.
    Empty,
    /// Transport is configured and ready for the next handshake.
    Pending(PendingTransport),
    /// A caller is currently inside [`McpClient::try_handshake`] and owns the transport.
    /// New callers MUST park on [`McpClient::init_done`] (with a bounded timeout) rather than attempt a parallel handshake.
    Initializing,
    /// Handshake completed; the service is reference-counted via `Arc`.
    Ready {
        service: McpService,
        _connected: distill_telemetry::activity::ActivityGaugeGuard,
    },
}

/// `Copy` projection of [`ClientState`] used for cheap state-machine inspection (see [`McpClient::state_kind`]).
/// Mirrors the variants 1:1, dropping the payloads so callers can pattern-match without borrowing the state mutex's inner data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStateKind {
    Empty,
    Pending,
    Initializing,
    Ready,
}

/// Classification used by [`crate::liveness::spawn_transport_liveness`].
/// Only `Ready + transport closed` produces a `TransportClosed` ACP push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivenessCheck {
    /// `Ready` + `is_transport_closed() == false`. Keep polling.
    Healthy,
    /// `Ready` + `is_transport_closed() == true`. Emit + exit.
    TransportClosed,
    /// Anything else (`Initializing`, `Pending`, `Empty`).
    /// The watcher exits silently: the new state is being managed externally; if it returns to `Ready` the owner can start a new watcher.
    Transient,
}

/// Events emitted by a live MCP client to its session-side dispatcher.
/// [`crate::liveness::spawn_transport_liveness`], when an `is_healthy` poll observes the rmcp service loop shut down (`TransportClosed`); [`GrokClientHandler`] when the server pushes a notification we care about. Currently `notifications/tools/list_changed` and `notifications/resources/list_changed`; The session/managed-config layer when a server is added, removed, or successfully (re-)initialized.
/// Consumers fan these out to ACP `x.ai/mcp/server_status` after 50 ms of tumbling-window coalescing keyed by `(server, kind)`.
#[derive(Debug, Clone)]
pub enum McpClientEvent {
    /// The rmcp service loop has terminated; the client is no longer usable for tool calls and must be torn down (or restarted).
    TransportClosed {
        server: McpServerName,
        /// Identity of the client whose transport closed (see [`McpClient::client_id`]).
        /// A mismatch with the client currently registered under `server` marks the event stale; it must not tear down the replacement.
        /// Every emitter holds the closing `McpClient`, so the id is always known.
        client_id: u64,
    },
    /// `ensure_initialized` returned `Err(_)`.
    /// `reason` is the full stringified error, surfaced verbatim to the client (no sanitization) so failures are easy to debug.
    HandshakeFailed {
        server: McpServerName,
        reason: String,
    },
    /// Server pushed `notifications/tools/list_changed`.
    ToolsChanged { server: McpServerName },
    /// Server pushed `notifications/resources/list_changed`.
    ResourcesChanged { server: McpServerName },
    ElicitationComplete {
        server: McpServerName,
        elicitation_id: String,
    },
    /// Client transitioned to [`ClientState::Ready`]; dispatcher uses this to surface "ready" status without polling.
    /// Emitted from `ensure_initialized`; the dispatcher maps it to `reason=initialized`.
    /// `reason=restart_succeeded` is reserved for the restart path.
    Ready { server: McpServerName },
    /// Managed/local config diff resolved.
    /// The dispatcher fans this out into one [`Self::ConfigAdded`] / [`Self::ConfigRemoved`] event per affected server before buffering.
    ConfigDiff {
        added: Vec<McpServerName>,
        removed: Vec<McpServerName>,
    },
    /// Per-server `(server, ConfigAdded)` fan-out variant produced by the dispatcher from a [`Self::ConfigDiff`].
    /// Keeps the invariant that the stored payload matches its `kind` key.
    /// Storing a fake `Ready` payload at a `ConfigAdded` key would be a footgun.
    ConfigAdded { server: McpServerName },
    /// Per-server `(server, ConfigRemoved)` fan-out variant.
    /// The dispatched analogue of [`Self::ConfigAdded`] for the removed set of a [`Self::ConfigDiff`].
    ConfigRemoved { server: McpServerName },
}

/// Discriminant for [`McpClientEvent`], used as the second half of the coalescing key `(server, kind)`.
/// Distinct from [`McpClientEvent`] because the latter carries payload.
/// Payload must not participate in equality / hashing.
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub enum McpClientEventKind {
    TransportClosed,
    HandshakeFailed,
    ToolsChanged,
    ResourcesChanged,
    ElicitationComplete,
    Ready,
    ConfigAdded,
    ConfigRemoved,
}

impl McpClientEvent {
    /// Server name carried by the event, if any.
    /// That variant is fanned out per-server by the dispatcher into [`Self::ConfigAdded`] / [`Self::ConfigRemoved`].
    /// Each fan-out child has a single server name.
    pub fn server_name(&self) -> Option<&str> {
        match self {
            Self::TransportClosed { server, .. }
            | Self::HandshakeFailed { server, .. }
            | Self::ToolsChanged { server }
            | Self::ResourcesChanged { server }
            | Self::ElicitationComplete { server, .. }
            | Self::Ready { server }
            | Self::ConfigAdded { server }
            | Self::ConfigRemoved { server } => Some(server.as_str()),
            Self::ConfigDiff { .. } => None,
        }
    }
}

/// RAII guard that restores [`ClientState::Pending`] when the handshake holder is dropped before publishing a result (task cancellation, panic).
/// The guard never restores on the success path.
/// Drop uses [`tokio::sync::Mutex::try_lock`] because `Drop` runs synchronously and we cannot block the runtime here.
struct InitGuard<'a> {
    state: &'a Mutex<ClientState>,
    init_done: &'a Notify,
    /// `Some` until [`Self::disarm`] is called.
    /// Holds the restorable transport (HTTP / HttpAuth) or `None` for Stdio (whose child process is consumed by `client.serve` and cannot be reused).
    restore: Option<PendingTransport>,
}

impl InitGuard<'_> {
    /// Mark the guard as having published a result.
    /// Subsequent `Drop` becomes a no-op so it doesn't fight the holder's own state store under the lock or wake waiters twice.
    fn disarm(&mut self) {
        self.restore = None;
    }
}

impl Drop for InitGuard<'_> {
    fn drop(&mut self) {
        let Some(restore) = self.restore.take() else {
            return;
        };
        // Best-effort restore: `try_lock` cannot block the runtime from inside Drop
        // On contention the slot stays Initializing and the inflight-wait timeout becomes the recovery path
        if let Ok(mut guard) = self.state.try_lock()
            && matches!(&*guard, ClientState::Initializing)
        {
            *guard = ClientState::Pending(restore);
        }
        // Notify whether or not we managed to restore
        // Parked waiters need to wake up and either retry against the restored transport or hit the wait-timeout error path
        self.init_done.notify_waiters();
    }
}

/// Build a restorable handle for a pending transport, or `None` if the transport cannot be reused after a handshake failure.
/// `PendingTransport` deliberately does not implement `Clone`.
/// The `Stdio` variant owns a [`tokio::process::Child`] that is consumed by `client.serve`, and a "restored" Stdio entry would be a dead handle.
fn restorable_transport(pending: &PendingTransport) -> Option<PendingTransport> {
    match pending {
        PendingTransport::Http(cfg) => Some(PendingTransport::Http(cfg.clone())),
        PendingTransport::HttpAuth {
            config,
            auth_manager,
        } => Some(PendingTransport::HttpAuth {
            config: config.clone(),
            auth_manager: auth_manager.clone(),
        }),
        PendingTransport::Acp { server_id, invoker } => Some(PendingTransport::Acp {
            server_id: server_id.clone(),
            invoker: invoker.clone(),
        }),
        PendingTransport::Stdio(_) => None,
    }
}

/// Monotonic source for [`McpClient::client_id`].
/// Process-global so every client instance (including test stubs) gets a unique identity.
static NEXT_CLIENT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_client_id() -> u64 {
    NEXT_CLIENT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub struct McpClient {
    /// Unique identity of this client *instance*. See [`Self::client_id`].
    client_id: u64,
    server_name: McpServerName,
    state: Mutex<ClientState>,
    /// Wakes [`Self::ensure_initialized`] callers that observed [`ClientState::Initializing`] and parked.
    /// Parking here (instead of failing fast) keeps that race from leaking an error into model-visible tool results.
    init_done: Notify,
    startup_timeout_sec: u64,
    tool_timeout_sec: u64,
    /// Per-tool timeout overrides in seconds.
    /// Looked up by tool name; falls back to `tool_timeout_sec` when a tool isn't listed.
    tool_timeouts: HashMap<ToolName, u64>,
    /// See [`McpServerMetaConfig::expose_image_base64`].
    expose_image_base64: bool,
    /// Shared `AuthorizationManager` for OAuth-enabled servers.
    /// `AuthClient` inside the transport holds a clone of this Arc so token updates are visible to both the transport and the re-auth path.
    auth_manager: Option<Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>>,
    observed_token: Option<crate::credentials::ObservedAccessToken>,
    /// Stored for OAuth clients so we can rebuild the transport after re-auth.
    http_config: Option<HttpConfig>,
    /// BYO OAuth config for the full browser flow fallback (when refresh fails).
    byo_oauth_config: Option<McpOAuthConfig>,
    /// Rate limit on this server's reconnect warnings; passed to each HTTP transport so rebuilds keep the limit.
    warn_budget: crate::mcp_http_client::WarnBudget,
    /// The transport to rebuild on a dead connection; see [`McpClient::reset_transport`].
    /// `None` for transports that can't reconnect, e.g. Stdio (whose child process is consumed by the handshake and can't be restarted from here).
    reconnect: Option<PendingTransport>,
    /// Event sink for transport-closed pollers, server-pushed `tools/list_changed` / `resources/list_changed` notifications, and handshake failures.
    /// `None` in three cases: Test stubs and standalone-pool fixtures that don't need cross-component event flow; Subagent / shared-pool snapshots: only the **parent** session owns these events. A subagent that inherits a shared `Arc<McpClient>` reads tools through it but does not install its own dispatcher. The parent's [`crate::liveness::TransportLivenessHandle`] already covers it; Brand-new clients before the session's per-server task has called [`Self::set_event_tx`].
    /// `parking_lot::Mutex` is sufficient: the lock is never held across an `.await`, and the handler's `emit` path is short and allocation-free.
    notify_tx: SharedEventTx,
    elicitation_tx: crate::elicitation::SharedElicitationTx,
    /// RAII handle for the per-client transport-liveness poller.
    /// `Some` after [`Self::arm_liveness_watcher`] succeeds; `None` initially.
    /// `parking_lot::Mutex` is sufficient because the lock is only ever held for the duration of a slot swap.
    liveness_handle: Arc<parking_lot::Mutex<Option<crate::liveness::TransportLivenessHandle>>>,
}

/// Shared sender slot: the same Arc lives on the [`McpClient`] and the [`GrokClientHandler`] it constructs during [`McpClient::try_handshake`].
/// Mutating the slot via [`McpClient::set_event_tx`] is observed by the live rmcp service loop on the next notification.
/// So there's no "snapshot at handshake" hazard.
pub type SharedEventTx =
    Arc<parking_lot::Mutex<Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>>>;

/// External-config overrides for an MCP server.
/// [`McpClient::load_timeouts`] and [`McpClient::load_expose_image_base64`] still apply the `_meta > overrides > default` precedence on top.
#[derive(Default, Debug, Clone)]
pub struct McpClientTimeoutOverrides {
    /// Server startup timeout in seconds.
    pub startup_timeout_sec: Option<u64>,
    /// Default per-tool timeout in seconds (used when a tool has no entry in `tool_timeouts`).
    pub tool_timeout_sec: Option<u64>,
    /// Per-tool timeout overrides in seconds, keyed by tool name.
    pub tool_timeouts: Option<HashMap<String, u64>>,
    /// See [`McpServerMetaConfig::expose_image_base64`].
    pub expose_image_base64: Option<bool>,
}

impl McpClient {
    fn load_timeouts(
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> (u64, u64, HashMap<ToolName, u64>) {
        // _meta > overrides > default
        // env / config / requirements / remote are resolved by the shell and injected via `overrides.startup_timeout_sec`
        let startup = meta_config
            .and_then(|mc| mc.startup_timeout_ms)
            .map(|ms| ms.div_ceil(1000))
            .or_else(|| overrides.and_then(|o| o.startup_timeout_sec))
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS);

        let tool = meta_config
            .and_then(|mc| mc.tool_timeout_ms)
            .map(|ms| ms.div_ceil(1000))
            .or_else(|| overrides.and_then(|o| o.tool_timeout_sec))
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS);

        // Per-tool overrides: external base, _meta overrides on top.
        // Precedence: _meta per-tool > overrides per-tool > (falls back to server-level `tool`)
        let mut tool_timeouts = HashMap::new();

        // Layer 1: overrides (already in seconds)
        if let Some(o) = overrides
            && let Some(ref tt) = o.tool_timeouts
        {
            tool_timeouts.extend(tt.iter().map(|(k, v)| (k.clone(), *v)));
        }

        // Layer 2: _meta tool_timeouts_ms (milliseconds to seconds), overrides external config
        if let Some(mc) = meta_config
            && let Some(ref tt) = mc.tool_timeouts_ms
        {
            for (k, v) in tt {
                tool_timeouts.insert(k.clone(), v.div_ceil(1000));
            }
        }

        (startup, tool, tool_timeouts)
    }

    /// `_meta > overrides > default(false)`, mirroring [`Self::load_timeouts`].
    fn load_expose_image_base64(
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> bool {
        meta_config
            .and_then(|mc| mc.expose_image_base64)
            .or_else(|| overrides.and_then(|o| o.expose_image_base64))
            .unwrap_or(false)
    }

    /// The ONLY place that writes the `McpClient { .. }` struct literal.
    /// It is `None` for non-reconnectable transports like Stdio; see [`restorable_transport`].
    #[allow(clippy::too_many_arguments)]
    fn new_with_transport(
        server_name: String,
        transport: PendingTransport,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
        auth_manager: Option<Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>>,
        observed_token: Option<crate::credentials::ObservedAccessToken>,
        http_config: Option<HttpConfig>,
        byo_oauth_config: Option<McpOAuthConfig>,
    ) -> Self {
        let reconnect = restorable_transport(&transport);
        let (startup_timeout_sec, tool_timeout_sec, tool_timeouts) =
            Self::load_timeouts(overrides, meta_config);
        let expose_image_base64 = Self::load_expose_image_base64(overrides, meta_config);
        Self {
            client_id: next_client_id(),
            server_name,
            state: Mutex::new(ClientState::Pending(transport)),
            init_done: Notify::new(),
            startup_timeout_sec,
            tool_timeout_sec,
            tool_timeouts,
            expose_image_base64,
            auth_manager,
            observed_token,
            http_config,
            byo_oauth_config,
            warn_budget: crate::mcp_http_client::WarnBudget::default(),
            reconnect,
            notify_tx: Arc::new(parking_lot::Mutex::new(None)),
            elicitation_tx: Arc::new(parking_lot::Mutex::new(None)),
            liveness_handle: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    fn new_http_auth(
        server_name: String,
        config: HttpConfig,
        auth_manager: Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
        observed_token: crate::credentials::ObservedAccessToken,
        byo_oauth_config: Option<McpOAuthConfig>,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::HttpAuth {
                config: config.clone(),
                auth_manager: auth_manager.clone(),
            },
            overrides,
            meta_config,
            Some(auth_manager),
            Some(observed_token),
            Some(config),
            byo_oauth_config,
        )
    }

    pub fn has_auth(&self) -> bool {
        self.auth_manager.is_some()
    }

    /// Try to recover tokens from disk or via refresh; no browser flow.
    /// Returns true if valid tokens were found (from another session/process writing to the credential store, or a successful token refresh).
    /// Used by `retry_auth_required_servers` on overlay refresh.
    pub async fn try_reauth_from_disk(&self) -> bool {
        let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config) else {
            return false;
        };

        {
            let mut mgr = auth_mgr.lock().await;
            let token_before = self.observed_token.as_ref().and_then(|o| o.snapshot());
            let ready = crate::oauth::ensure_oauth_ready(&self.server_name, &mut mgr)
                .await
                .hydrated;
            let token_after = self.observed_token.as_ref().and_then(|o| o.snapshot());
            if token_after.is_some() && token_after != token_before {
                tracing::info!(
                    server = self.server_name.as_str(),
                    "Loaded fresh tokens from disk"
                );
                drop(mgr);
                self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                    config: config.clone(),
                    auth_manager: auth_mgr.clone(),
                }))
                .await;
                return true;
            }
            if !ready {
                return false;
            }
        }

        let refresh_ok = {
            let mgr = auth_mgr.lock().await;
            mgr.refresh_token().await.is_ok()
        };

        if refresh_ok {
            self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                config: config.clone(),
                auth_manager: auth_mgr.clone(),
            }))
            .await;
            return true;
        }

        false
    }

    /// Force token acquisition and reset the transport so the next `ensure_initialized` rebuilds it with the fresh token.
    /// Reload from disk (picks up tokens from background auth task); Refresh via refresh_token grant; Full browser-based OAuth flow, unless the refresh failure was a pure network failure ([`mcp_refresh_failure_is_transient`]). The stored refresh token is then still presumed valid. Opening a browser tab / re-running DCR for a Wi-Fi blip right after wake-from-sleep is both useless and destructive. Useless because the IdP is unreachable for the browser too; destructive because it discards a working credential.
    pub async fn force_reauth(&self, force: bool) -> bool {
        let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config) else {
            return false;
        };

        let ready = {
            let mut mgr = auth_mgr.lock().await;
            let token_before = self.observed_token.as_ref().and_then(|o| o.snapshot());
            let ready = crate::oauth::ensure_oauth_ready(&self.server_name, &mut mgr)
                .await
                .hydrated;
            let token_after = self.observed_token.as_ref().and_then(|o| o.snapshot());
            if token_after.is_some() && token_after != token_before {
                tracing::info!(
                    server = self.server_name.as_str(),
                    "Loaded fresh tokens from disk (background auth or other process)"
                );
                drop(mgr);
                self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                    config: config.clone(),
                    auth_manager: auth_mgr.clone(),
                }))
                .await;
                return true;
            }
            ready
        };

        if ready {
            let refresh_result = {
                let mgr = auth_mgr.lock().await;
                mgr.refresh_token().await
            };

            match refresh_result {
                Ok(_) => {
                    tracing::info!(
                        server = self.server_name.as_str(),
                        "Token refreshed successfully (no browser)"
                    );
                    self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
                        config: config.clone(),
                        auth_manager: auth_mgr.clone(),
                    }))
                    .await;
                    return true;
                }
                Err(ref e) if !force && mcp_refresh_failure_is_transient(e) => {
                    tracing::warn!(
                        server = self.server_name.as_str(),
                        error = %e,
                        "Token refresh failed transiently (network); skipping browser escalation"
                    );
                    return false;
                }
                Err(e) => {
                    tracing::info!(
                        server = self.server_name.as_str(),
                        error = %e,
                        "Token refresh failed terminally; falling back to browser auth"
                    );
                }
            }
        } else if !force {
            tracing::warn!(
                server = self.server_name.as_str(),
                "OAuth client not ready (hydration failed); retrying later instead of browser auth"
            );
            return false;
        }

        // Full browser-based OAuth flow.
        {
            tracing::info!(
                server = self.server_name.as_str(),
                "Falling back to browser auth"
            );
            if let Err(e) = crate::oauth::authenticate_mcp_server_dedup(
                &self.server_name,
                &config.url,
                auth_mgr,
                self.byo_oauth_config.as_ref(),
                force,
            )
            .await
            {
                tracing::warn!(
                    server = self.server_name.as_str(),
                    %e,
                    "Full re-authentication failed"
                );
                return false;
            }
        }

        self.replace_state(ClientState::Pending(PendingTransport::HttpAuth {
            config: config.clone(),
            auth_manager: auth_mgr.clone(),
        }))
        .await;
        true
    }

    /// Reset the transport so the next `ensure_initialized` rebuilds it with a fresh connection.
    /// Called when a tool call fails with a transport error (`TransportClosed`, `TransportSend`).
    /// The underlying connection is dead but the server's addressing (URL/headers for HTTP, `server_id`/invoker for ACP) is still valid.
    async fn reset_transport(&self) -> bool {
        let Some(t) = self.reconnect.as_ref().and_then(restorable_transport) else {
            return false;
        };
        self.replace_state(ClientState::Pending(t)).await;
        tracing::info!(
            server = %self.server_name,
            "Reset transport for reconnect after transport failure"
        );
        true
    }

    /// `true` if this client has an HTTP/SSE transport.
    /// The explicit predicate for recovery gates (the proactive path only recovers HTTP clients).
    pub fn is_http(&self) -> bool {
        self.http_config.is_some()
    }

    /// `true` when the transport carries a config-provided `Authorization` header.
    /// Its 401s are a config problem OAuth login cannot fix (spawn and the login rebuild both skip discovery for such servers).
    pub fn has_configured_auth_header(&self) -> bool {
        self.http_config
            .as_ref()
            .is_some_and(HttpConfig::has_authorization_header)
    }

    /// `true` for an in-process SDK client reached over the ACP reverse channel (rather than HTTP/stdio).
    /// Gates liveness watching; see [`Self::arm_liveness_watcher`].
    pub fn is_acp(&self) -> bool {
        matches!(self.reconnect, Some(PendingTransport::Acp { .. }))
    }

    /// Recover a dead transport in place: reset, re-handshake, then restart the liveness watcher. Returns the live [`McpService`].
    /// `Err` for a client with no restorable transport.
    pub async fn recover(self: &Arc<Self>) -> Result<McpService, McpError> {
        // Coalesce concurrent recoveries: reset only when Ready
        // If already non-Ready a recovery is in flight, so join its single-flight ensure_initialized instead of racing a reset
        if matches!(self.state_kind().await, ClientStateKind::Ready)
            && !self.reset_transport().await
        {
            return Err(McpError::ClientError(format!(
                "MCP client {} has no transport to recover",
                self.server_name,
            )));
        }
        let service = self.ensure_initialized().await?;
        // Restart the liveness watcher so the next close is detected again
        // A `false` return with a wired sender is unexpected only for watched (non-ACP) clients
        if !self
            .arm_liveness_watcher(crate::liveness::DEFAULT_POLL_INTERVAL)
            .await
            && !self.is_acp()
            && self.event_tx_clone().is_some()
        {
            tracing::warn!(
                server = %self.server_name,
                "recovery: liveness watcher not re-armed despite a wired event sender",
            );
        }
        Ok(service)
    }

    /// Replace [`Self::state`] under the lock and wake any [`Self::ensure_initialized`] callers parked on [`Self::init_done`].
    /// It already mints the new state under the lock it's holding and notifies waiters once at the end of the handshake attempt.
    async fn replace_state(&self, new_state: ClientState) {
        {
            let mut guard = self.state.lock().await;
            *guard = new_state;
        }
        self.init_done.notify_waiters();
    }

    pub fn new_stdio(
        server_name: String,
        transport: SafeTokioChildProcess,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Stdio(Box::new(transport)),
            overrides,
            meta_config,
            None,
            None,
            None,
            None,
        )
    }

    /// Build a client for an in-process SDK MCP server reached over the ACP reverse channel.
    /// `server_id` is the id the agent echoes back in `x.ai/mcp/sdk_call`; the `invoker` performs the reverse request.
    /// Same downstream path as HTTP/stdio.
    pub fn new_acp(
        server_name: String,
        server_id: String,
        invoker: Arc<dyn crate::acp_transport::AcpReverseInvoker>,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Acp { server_id, invoker },
            overrides,
            meta_config,
            None,
            None,
            None,
            None,
        )
    }

    pub fn new_http(
        server_name: String,
        config: HttpConfig,
        overrides: Option<&McpClientTimeoutOverrides>,
        meta_config: Option<&McpServerMetaConfig>,
    ) -> Self {
        Self::new_with_transport(
            server_name,
            PendingTransport::Http(config.clone()),
            overrides,
            meta_config,
            None,
            None,
            Some(config),
            None,
        )
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Unique identity of this client *instance*.
    /// Two clients for the same server name (e.g. a dead client and its replacement after a config remove+re-add) have different ids.
    /// Carried on [`McpClientEvent::TransportClosed`] so consumers can tell a death event for the current client from a stale predecessor's.
    pub fn client_id(&self) -> u64 {
        self.client_id
    }

    pub fn startup_timeout_sec(&self) -> u64 {
        self.startup_timeout_sec
    }

    pub fn tool_timeout_sec(&self) -> u64 {
        self.tool_timeout_sec
    }

    /// See [`McpServerMetaConfig::expose_image_base64`].
    pub fn expose_image_base64(&self) -> bool {
        self.expose_image_base64
    }

    /// Resolve the timeout for a specific tool.
    /// Precedence (highest to lowest): `_meta.mcpConfig.<server>.toolTimeoutsMs.<tool>`; `config.toml [mcp_servers.<server>].tool_timeouts.<tool>`; `_meta.mcpConfig.<server>.toolTimeoutMs`; `config.toml [mcp_servers.<server>].tool_timeout_sec`; Default (60s).
    /// Steps 1 and 2 are already merged into `self.tool_timeouts` at construction.
    pub fn tool_timeout_for(&self, tool_name: &str) -> u64 {
        self.tool_timeouts
            .get(tool_name)
            .copied()
            .unwrap_or(self.tool_timeout_sec)
    }

    /// Drive the MCP handshake to completion, or return the cached service if one is already established.
    /// Concurrent callers park on `init_done` instead of issuing a parallel handshake.
    /// If the holder is dropped before publishing, `InitGuard` restores the transport so a later caller can retry; `Drop` uses `try_lock` because it is synchronous.
    pub async fn ensure_initialized(&self) -> Result<McpService, McpError> {
        // Bound how long a parked caller waits on `init_done` before surfacing an error; wedging silently would recreate the exact "stuck client" failure mode
        let inflight_wait =
            std::time::Duration::from_secs(self.handshake_worst_case_secs().saturating_add(1));

        // Drive the loop body until we either return directly or break out with an owned `PendingTransport`
        // We deliberately use a labelled `loop` with a `break <expr>`
        let pending: PendingTransport = loop {
            // Subscribe to `init_done` BEFORE inspecting `state` so a wake-up fired between our state check and the wait can't be lost
            // `tokio::sync::Notify` only delivers a permit to notify-futures that exist at the time of `notify_waiters`
            let notified = self.init_done.notified();
            tokio::pin!(notified);

            let mut guard = self.state.lock().await;
            // Swap the current state for `Initializing` up front and match on the OWNED previous value
            // The compiler can bind `ClientState::Pending(t)` directly from an owned value with no irrefutable-let hole
            // Non-Pending arms restore their original variant before falling through
            match std::mem::replace(&mut *guard, ClientState::Initializing) {
                ClientState::Ready {
                    service,
                    _connected,
                } => {
                    let ready = service.clone();
                    *guard = ClientState::Ready {
                        service,
                        _connected,
                    };
                    return Ok(ready);
                }
                ClientState::Empty => {
                    *guard = ClientState::Empty;
                    return Err(McpError::ClientError(format!(
                        "MCP client {} has no transport configured",
                        self.server_name,
                    )));
                }
                ClientState::Initializing => {
                    // Another caller already owns the slot
                    // Restore the placeholder we just swapped in (semantically a no-op since `Initializing` is a unit variant)
                    // Drop the lock and park on `init_done`
                    *guard = ClientState::Initializing;
                    drop(guard);
                    match tokio::time::timeout(inflight_wait, notified.as_mut()).await {
                        Ok(()) => continue,
                        Err(_) => {
                            return Err(McpError::ClientError(format!(
                                "MCP client {} init still in progress after {}s",
                                self.server_name,
                                inflight_wait.as_secs(),
                            )));
                        }
                    }
                }
                // The single arm that KEEPS the `Initializing` placeholder we swapped in
                // This caller becomes the single-flight handshake holder for the duration of `try_handshake` below
                ClientState::Pending(transport) => break transport,
            }
        };

        // Lock released. Run the handshake outside the lock so other callers can park on `init_done` instead of stalling on `state.lock()`.

        // Clone the transport's restorable handle twice: once for the failure-path retry below, once for the drop guard
        // `PendingTransport` is intentionally not `Clone` (Stdio's `TokioChildProcess` is unique)
        // The helper returns `None` for Stdio (whose handshake failures cannot be recovered without a fresh spawn)
        let restore = restorable_transport(&pending);
        let restore_for_guard = restorable_transport(&pending);

        // Drop guard: restore `Pending(restore)` if `try_handshake` panics or is cancelled before we publish a result
        // Other callers then don't stall on `Initializing` forever
        // Disarm via `disarm()` immediately before storing the real result
        let mut init_guard = InitGuard {
            state: &self.state,
            init_done: &self.init_done,
            restore: restore_for_guard,
        };

        let transport_type = match &pending {
            PendingTransport::Stdio(_) => "stdio",
            PendingTransport::Http(_) => "http",
            PendingTransport::HttpAuth { .. } => "http_auth",
            PendingTransport::Acp { .. } => "acp",
        };
        let handshake_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
            "mcp.handshake",
            server_name = %self.server_name,
            transport_type,
            elapsed_ms = tracing::field::Empty,
        ));
        let handshake_start = std::time::Instant::now();
        let mut result = self.try_handshake(pending).await;

        let handshake_elapsed = handshake_start.elapsed().as_micros() as u64;
        tracing::info!(target: distill_telemetry::instrumentation::TARGET, event = "timing", name = "mcp_try_handshake", server_name = %self.server_name, elapsed_us = handshake_elapsed);
        // On handshake failure, if we have an auth_manager, try refreshing the token and retrying once
        // Handles expired access tokens loaded from disk: the handshake fails at the transport layer before rmcp's transparent 401 refresh kicks in We attempt refresh on any failure (not just auth errors): the cost is low
        // Also, error strings from different MCP servers are not reliable to match
        if result.is_err()
            && let (Some(auth_mgr), Some(config)) = (&self.auth_manager, &self.http_config)
        {
            tracing::info!(
                server = %self.server_name,
                "Handshake failed, attempting token refresh and retry"
            );
            let refresh_ok = {
                let mut mgr = auth_mgr.lock().await;
                crate::oauth::ensure_oauth_ready(&self.server_name, &mut mgr)
                    .await
                    .hydrated
                    && mgr.refresh_token().await.is_ok()
            };
            if refresh_ok {
                let retry_transport = PendingTransport::HttpAuth {
                    config: config.clone(),
                    auth_manager: auth_mgr.clone(),
                };
                result = self.try_handshake(retry_transport).await;
            }
        }
        handshake_span
            .span()
            .record("elapsed_ms", handshake_start.elapsed().as_millis() as i64);
        handshake_span.close();

        // Disarm before publishing the result so the drop guard doesn't double-restore on the success path
        // Disarming also keeps it from fighting the failure-path assignment below
        init_guard.disarm();

        // Snapshot the event sender before we commit Ready/Pending/Empty under the lock
        // That way a `state.lock().await` inside the dispatcher (should one ever exist; none today) can't deadlock
        let event_tx = self.event_tx_clone();

        let outcome = {
            let mut guard = self.state.lock().await;
            match result {
                Ok(service) => {
                    let service = Arc::new(service);
                    *guard = ClientState::Ready {
                        service: service.clone(),
                        _connected: MCP_SERVERS_CONNECTED.enter(),
                    };
                    tracing::info!(
                        server = %self.server_name,
                        "MCP server initialized successfully"
                    );
                    Ok(service)
                }
                Err(e) => {
                    *guard = match restore {
                        Some(transport) => ClientState::Pending(transport),
                        None => ClientState::Empty,
                    };
                    tracing::warn!(
                        server = %self.server_name,
                        error = %e,
                        "MCP server init failed"
                    );
                    Err(e)
                }
            }
        };
        // Wake parked callers AFTER releasing the state lock so they observe the freshly-stored Ready/Pending/Empty value
        self.init_done.notify_waiters();

        // Notify the session-actor StatusDispatcher of the handshake outcome, AFTER releasing the state lock
        // Best-effort: if the receiver is gone (dispatcher torn down, subagent without wiring) the send fails silently
        // The dispatcher is the only path that turns these into ACP pushes; see the `client_event_tx` field on `McpState`
        if let Some(tx) = &event_tx {
            match &outcome {
                Ok(_) => {
                    let _ = tx.send(McpClientEvent::Ready {
                        server: self.server_name.clone(),
                    });
                }
                Err(e) => {
                    let _ = tx.send(McpClientEvent::HandshakeFailed {
                        server: self.server_name.clone(),
                        reason: e.to_string(),
                    });
                }
            }
        }
        outcome
    }

    /// Run the MCP handshake (no lock held).
    #[tracing::instrument(
        name = "mcp.serve",
        skip_all,
        parent = None,
        fields(server = %self.server_name)
    )]
    async fn try_handshake(
        &self,
        pending: PendingTransport,
    ) -> Result<rmcp::service::RunningService<RoleClient, GrokClientHandler>, McpError> {
        match pending {
            // Stdio stays on the legacy `initialize`-only handshake. Probing it is unsafe on two counts: a slow-starting server can answer the abandoned `server/discover` after rmcp has already sent
            // `initialize` on the SAME byte stream, and rmcp rejects that late response as uncorrelated; and unlike the other transports the child process cannot be rebuilt here for a clean fallback.
            // Modern-only stdio servers stay unsupported until rmcp tolerates late responses to abandoned requests.
            PendingTransport::Stdio(process) => self.serve_legacy(*process).await,
            PendingTransport::Http(config) => {
                // One reqwest client for both phases: the probe and the legacy transports stay separate rmcp sessions, but share the connection pool, so a responsive legacy server doesn't pay a second TCP+TLS setup.
                let http_client =
                    Self::build_http_client(&config, &self.server_name, self.warn_budget.clone())?;
                self.probe_then_legacy(|| {
                    StreamableHttpClientTransport::with_client(
                        http_client.clone(),
                        StreamableHttpClientTransportConfig::with_uri(config.url.as_str()),
                    )
                })
                .await
            }
            PendingTransport::HttpAuth {
                config,
                auth_manager,
            } => {
                let http_client = self.build_oauth_http_client(&config, &auth_manager).await?;
                self.probe_then_legacy(|| {
                    StreamableHttpClientTransport::with_client(
                        http_client.clone(),
                        StreamableHttpClientTransportConfig::with_uri(config.url.as_str()),
                    )
                })
                .await
            }
            PendingTransport::Acp { server_id, invoker } => {
                self.probe_then_legacy(|| self.build_acp_transport(server_id.clone(), &invoker))
                    .await
            }
        }
    }

    /// Probe for a modern server, then run the legacy handshake on a fresh transport when the probe says "legacy". Shared by every transport that can rebuild its transport (all but stdio).
    /// When both phases fail, the probe's error is warned alongside the legacy error so diagnostics show the whole story, while [`McpError`] carries only the legacy error (the actionable one for a legacy-majority world).
    async fn probe_then_legacy<T, E, A>(
        &self,
        mut make_transport: impl FnMut() -> T,
    ) -> Result<rmcp::service::RunningService<RoleClient, GrokClientHandler>, McpError>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let probe_failure = match self.probe_modern(make_transport()).await? {
            ProbeVerdict::Modern(service) => return Ok(*service),
            ProbeVerdict::Legacy { probe_error } => probe_error,
        };
        let result = self.serve_legacy(make_transport()).await;
        if let Err(legacy_error) = &result {
            tracing::warn!(
                server = %self.server_name,
                probe_error = %probe_failure,
                %legacy_error,
                "both handshake phases failed (surfacing the legacy error)"
            );
        }
        result
    }

    /// Cap on the `server/discover` probe phase. Mirrors rmcp's private
    /// `DEFAULT_AUTO_DISCOVER_TIMEOUT` so a server that swallows the probe
    /// (never answers unknown methods) costs at most this much extra startup latency before the legacy handshake runs with its full budget.
    pub(crate) const DISCOVER_PROBE_TIMEOUT_SECS: u64 = 10;

    /// Timeout of the `server/discover` probe phase: [`Self::DISCOVER_PROBE_TIMEOUT_SECS`],
    /// shrunk to the startup budget when that is shorter. Never skipped: 2026-07-28-era servers
    /// reject every `tools/call` on a legacy session, so a short budget must still probe.
    fn probe_timeout_secs(&self) -> u64 {
        Self::DISCOVER_PROBE_TIMEOUT_SECS
            .min(self.startup_timeout_sec)
            .max(1)
    }

    /// Worst-case wall time of one [`Self::try_handshake`] attempt: the probe phase plus the legacy phase's full startup budget. Anything that waits on a handshake holder must use this bound, not `startup_timeout_sec` alone — a swallowed probe legitimately keeps the holder busy past the startup budget.
    fn handshake_budget_secs(&self) -> u64 {
        self.startup_timeout_sec
            .saturating_add(self.probe_timeout_secs())
    }

    /// Worst case for [`Self::ensure_initialized`]: one attempt, or for an OAuth-capable client a failed attempt, a
    /// token refresh, and a full retry.
    fn handshake_worst_case_secs(&self) -> u64 {
        if self.auth_manager.is_some() && self.http_config.is_some() {
            self.handshake_budget_secs()
                .saturating_mul(2)
                .saturating_add(Self::OAUTH_REFRESH_RETRY_ALLOWANCE_SECS)
        } else {
            self.handshake_budget_secs()
        }
    }

    /// Waiter allowance for the token-refresh interlude between an OAuth client's two handshake attempts: metadata/credential hydration is bounded by [`OAUTH_DISCOVERY_TIMEOUT`]-sized steps, plus the refresh
    /// POST itself. The POST rides rmcp's own OAuth client without a total request deadline, so this covers the nominal path; a pathological hung refresh can still outlast waiters, which predates the probe split and needs a deadline inside rmcp's refresh to close fully.
    const OAUTH_REFRESH_RETRY_ALLOWANCE_SECS: u64 = 15;

    /// Smallest whole-second deadline that holds both handshake phases (one second each).
    pub const MIN_HANDSHAKE_DEADLINE_SECS: u64 = 2;

    /// Largest `startup_timeout_sec` whose worst-case handshake
    /// ([`Self::handshake_budget_secs`]) still fits inside `deadline_secs`.
    /// For deadline-driven callers: deriving the override from this keeps the invariant that a hung handshake fails on its own before the outer deadline has to cancel it — a swallowed probe can no longer burn the legacy phase's window. Short deadlines split evenly between the probe and the legacy phase, since the probe timeout tracks the startup budget ([`Self::probe_timeout_secs`]).
    /// Deadlines under [`Self::MIN_HANDSHAKE_DEADLINE_SECS`] cannot hold both phases; they get the minimum budget and the caller's outer deadline fires first.
    pub fn max_startup_within_deadline(deadline_secs: u64) -> u64 {
        let deadline_secs = deadline_secs.max(Self::MIN_HANDSHAKE_DEADLINE_SECS);
        if deadline_secs >= Self::DISCOVER_PROBE_TIMEOUT_SECS.saturating_mul(2) {
            deadline_secs - Self::DISCOVER_PROBE_TIMEOUT_SECS
        } else {
            deadline_secs / 2
        }
    }

    /// Phase 1: probe `server/discover` so 2026-07-28 (SEP-2575 session-less +
    /// [`ProbeVerdict::Legacy`] means "run the legacy handshake on a fresh transport". Every probe outcome showing the SERVER was reached maps there: immediate rejections (correlated JSON-RPC errors like method-not-found, the middleware 4xx shapes rmcp classifies itself, and non-JSON 5xx / SSE error events surfaced as transport errors) fall back at once, while servers that ACCEPT the probe but never answer it
    /// Connect-phase failures are the exception: the probe never reached a server, so a legacy attempt against the same endpoint would only double the time-to-error on a blackholed host. Those surface as `Err` directly, through the same typed/message classification
    async fn probe_modern<T, E, A>(&self, transport: T) -> Result<ProbeVerdict, McpError>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let probe_timeout = std::time::Duration::from_secs(self.probe_timeout_secs());
        let handler = self.make_client_handler();
        let lifecycle = ClientLifecycleMode::Discover {
            preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
        };
        match tokio::time::timeout(
            probe_timeout,
            handler.serve_with_lifecycle(transport, lifecycle),
        )
        .await
        {
            Ok(Ok(service)) => Ok(ProbeVerdict::Modern(Box::new(service))),
            Ok(Err(probe_error)) => {
                if init_error_is_connect_phase(&probe_error)
                    || is_connect_failure_message(&probe_error.to_string())
                {
                    return Err(McpError::HandshakeFailed {
                        server: self.server_name.to_string(),
                        source: Box::new(probe_error),
                    });
                }
                tracing::debug!(
                    server = %self.server_name,
                    %probe_error,
                    "server/discover probe failed; falling back to the legacy initialize handshake"
                );
                Ok(ProbeVerdict::Legacy {
                    probe_error: probe_error.to_string(),
                })
            }
            Err(_) => {
                tracing::debug!(
                    server = %self.server_name,
                    "server/discover probe timed out; falling back to the legacy initialize handshake"
                );
                Ok(ProbeVerdict::Legacy {
                    probe_error: format!(
                        "server/discover probe timed out after {}s",
                        probe_timeout.as_secs()
                    ),
                })
            }
        }
    }

    /// Phase 2 (and the only phase for stdio): the legacy
    /// `initialize` handshake, on its own fresh transport with the FULL startup budget — the probe phase never erodes it, so `startup_timeout_sec` keeps its pre-probe meaning for legacy servers. Its failure is surfaced directly (never wrapped in a fallback-specific error), so the auth and transport classifiers on [`McpError::HandshakeFailed`] see the same errors they saw before the probe existed.
    /// The `initialize` names 2025-11-25 (pinned once, in [`Self::make_client_info`]) — the newest revision that HAS an initialize handshake. Never 2026-07-28: that revision removed the handshake, and version-strict servers reject it with `UnsupportedProtocolVersionException` instead of counter-offering.
    async fn serve_legacy<T, E, A>(
        &self,
        transport: T,
    ) -> Result<rmcp::service::RunningService<RoleClient, GrokClientHandler>, McpError>
    where
        T: rmcp::transport::IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let timeout = std::time::Duration::from_secs(self.startup_timeout_sec);
        let handler = self.make_client_handler();
        tokio::time::timeout(
            timeout,
            handler.serve_with_lifecycle(transport, ClientLifecycleMode::Initialize),
        )
        .await
        .map_err(|_| McpError::timeout(&self.server_name, timeout))?
        .map_err(|e| McpError::HandshakeFailed {
            server: self.server_name.to_string(),
            source: Box::new(e),
        })
    }

    fn build_acp_transport(
        &self,
        server_id: String,
        invoker: &Arc<dyn crate::acp_transport::AcpReverseInvoker>,
    ) -> crate::acp_transport::AcpBridgeTransport {
        // Per-reverse-call backstop on `x.ai/mcp/sdk_call`: the larger of the startup and tool timeouts
        // It never undercuts the real outer bound: the handshake is bounded per phase in `try_handshake`
        let invoke_timeout =
            std::time::Duration::from_secs(self.startup_timeout_sec.max(self.tool_timeout_sec));
        crate::acp_transport::acp_bridge_transport(server_id, Arc::clone(invoker), invoke_timeout)
    }

    /// OAuth flavor of [`Self::build_http_client`]: one shared, cloneable
    /// client for both handshake phases, with per-request `Authorization`
    /// injected by the shared [`rmcp::transport::auth::AuthClient`].
    async fn build_oauth_http_client(
        &self,
        config: &HttpConfig,
        auth_manager: &Arc<tokio::sync::Mutex<rmcp::transport::auth::AuthorizationManager>>,
    ) -> Result<
        crate::mcp_http_client::McpHttpClient<rmcp::transport::auth::AuthClient<reqwest::Client>>,
        McpError,
    > {
        let name = &self.server_name;
        // Local app endpoints skip OAuth outright (`start_mcp_server` routes them to `NoOauthSupport`), so this transport must never see one — its client is built without the local no-proxy/no-redirect hardening.
        debug_assert!(
            !config.local_agent_endpoint,
            "a local agent endpoint must not reach the OAuth transport"
        );
        // Authorization is injected per-request by `AuthClient`, never
        // carried in `default_headers`.
        let mut headers = parse_config_headers(
            name,
            "oauth-transport",
            config
                .headers
                .iter()
                .filter(|(key, _)| !key.eq_ignore_ascii_case("Authorization"))
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        apply_user_agent_policy(&mut headers, name, &config.url);
        // reqwest 0.13; the policy chokepoint is typed for 0.12 and cannot wrap this builder.
        #[allow(clippy::disallowed_methods)]
        let http_client = with_extra_root_certificates(
            reqwest::Client::builder()
                .default_headers(headers)
                .connect_timeout(HTTP_CONNECT_TIMEOUT),
        )
        .build()
        .map_err(|e| McpError::ClientError(format!("Failed to build HTTP client: {e}")))?;
        // `AuthClient::new` wants an owned manager, but ours is shared (`Arc`) with the OAuth flow
        // The struct is non_exhaustive, so build with a throwaway manager and swap in the shared one
        let placeholder_manager =
            rmcp::transport::auth::AuthorizationManager::new(config.url.as_str())
                .await
                .map_err(|e| McpError::ClientError(format!("Failed to build OAuth client: {e}")))?;
        let mut auth_client =
            rmcp::transport::auth::AuthClient::new(http_client, placeholder_manager);
        auth_client.auth_manager = auth_manager.clone();
        let mcp_http_client = crate::mcp_http_client::McpHttpClient::new(
            auth_client,
            name.as_str(),
            self.warn_budget.clone(),
        );
        Ok(mcp_http_client)
    }

    fn make_client_info(server_name: &str, advertise_elicitation: bool) -> ClientInfo {
        use rmcp::model::{
            ElicitationCapability, FormElicitationCapability, UrlElicitationCapability,
        };

        let mut extensions = rmcp::model::ExtensionCapabilities::new();
        extensions.insert(
            "io.modelcontextprotocol/ui".to_string(),
            serde_json::from_value(serde_json::json!({
                "mimeTypes": ["text/html;profile=mcp-app"]
            }))
            .unwrap_or_default(),
        );
        let mut capabilities = ClientCapabilities::default();
        capabilities.extensions = Some(extensions);
        if advertise_elicitation {
            capabilities.elicitation = Some(
                ElicitationCapability::new()
                    .with_form(FormElicitationCapability::new().with_schema_validation(true))
                    .with_url(UrlElicitationCapability::new()),
            );
        }
        ClientInfo::new(
            capabilities,
            Implementation::new(
                format!("grok-shell-{server_name}"),
                distill_version::VERSION.to_string(),
            ),
        )
        // The newest initialize-era revision — the single pin `serve_legacy`'s handshake names. Never pin 2026-07-28 here: that revision removed the initialize handshake, and version-strict servers reject an `initialize` that names it. 2026-07-28 is only negotiated via `server/discover` (see `probe_modern`). The explicit setter must remain so a future rmcp bump cannot silently move the wire.
        .with_protocol_version(rmcp::model::ProtocolVersion::V_2025_11_25)
    }

    /// Build the [`GrokClientHandler`] that drives `client.serve_with_lifecycle(...)`.
    ///
    /// The handler holds a **clone of `Arc<Mutex<Option<Sender>>>`**, not a snapshot, so a later [`Self::set_event_tx`] reaches the live handler.
    fn make_client_handler(&self) -> GrokClientHandler {
        GrokClientHandler {
            info: Self::make_client_info(
                &self.server_name,
                !self.is_acp() && self.elicitation_tx.lock().is_some(),
            ),
            server_name: self.server_name.clone(),
            notify_tx: Arc::clone(&self.notify_tx),
            elicitation_tx: Arc::clone(&self.elicitation_tx),
        }
    }

    /// Wire a sender for [`McpClientEvent`]s emitted by this client.
    /// So the change is observed session-wide on the next event.
    pub fn set_event_tx(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>>) {
        *self.notify_tx.lock() = tx;
    }

    /// For a client that will never be installed: drops the watcher's strong `Arc` and the senders that could still
    /// reach the session.
    pub fn discard(&self) {
        self.set_liveness_handle(None);
        self.set_event_tx(None);
        self.set_elicitation_tx(None);
    }

    pub fn set_elicitation_tx(&self, tx: Option<crate::elicitation::ElicitationInbox>) {
        *self.elicitation_tx.lock() = tx;
    }

    /// Bridge one elicitation request from an MRTR `input_required` round to the HITL UI.
    /// Same inbox path as the server-initiated `elicitation/create` handler, so both protocol
    /// generations share the coordinator, wire format, and card rendering.
    pub(crate) async fn bridge_elicit(
        &self,
        params: rmcp::model::ElicitRequestParams,
    ) -> rmcp::model::ElicitResult {
        crate::elicitation::bridge_elicit(&self.elicitation_tx, &self.server_name, params).await
    }

    /// Snapshot the current event sender, if any.
    /// Synchronous because the shared slot is a `parking_lot::Mutex`.
    pub fn event_tx_clone(&self) -> Option<tokio::sync::mpsc::UnboundedSender<McpClientEvent>> {
        self.notify_tx.lock().clone()
    }

    /// Install or replace this client's transport-liveness handle.
    /// Pass `None` to stop watching without installing a new one.
    pub fn set_liveness_handle(&self, handle: Option<crate::liveness::TransportLivenessHandle>) {
        *self.liveness_handle.lock() = handle;
    }

    /// Start the per-client transport-liveness watcher.
    /// Returns `false` for in-process SDK ([`Self::is_acp`]) clients: the watcher's only output is `TransportClosed`. The dispatcher can't recover that for ACP (not in `configs`), so it would evict the client. ACP recovers lazily via [`Self::reset_transport`] instead. Gated here so no caller can forget it; Returns `false` if there's no `notify_tx` wired (subagent snapshot or pre-dispatcher state); nothing to do; Returns `false` if the client isn't `Ready`. A poller spawned then would just exit silently on its first poll; skipping the spawn entirely is cheaper; Returns `false` if a live handle is already installed; Otherwise spawns the poller and stores the handle.
    /// So the worst case under TOCTOU is "the poller starts and immediately stops"; it never produces a spurious `TransportClosed`.
    pub async fn arm_liveness_watcher(
        self: &Arc<Self>,
        poll_interval: std::time::Duration,
    ) -> bool {
        if self.is_acp() {
            return false;
        }
        let Some(event_tx) = self.event_tx_clone() else {
            return false;
        };
        if !matches!(self.state_kind().await, ClientStateKind::Ready) {
            return false;
        }
        let mut slot = self.liveness_handle.lock();
        if slot.is_some() {
            return false;
        }
        let handle = crate::liveness::spawn_transport_liveness(
            self.server_name.clone(),
            Arc::clone(self),
            poll_interval,
            event_tx,
            Arc::clone(&self.liveness_handle),
        );
        *slot = Some(handle);
        true
    }

    /// One shared, cloneable HTTP client for a server's handshake: the probe and the legacy transports are separate rmcp sessions built from clones of this client, so they share its connection pool instead of paying a second TCP+TLS setup.
    fn build_http_client(
        config: &HttpConfig,
        server_name: &str,
        warn_budget: crate::mcp_http_client::WarnBudget,
    ) -> Result<crate::mcp_http_client::McpHttpClient<reqwest::Client>, McpError> {
        let mut headers = parse_config_headers(
            server_name,
            "transport",
            config
                .headers
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        apply_user_agent_policy(&mut headers, server_name, &config.url);
        // reqwest 0.13; the policy chokepoint is typed for 0.12 and cannot wrap this builder.
        #[allow(clippy::disallowed_methods)]
        let mut builder = with_extra_root_certificates(
            reqwest::Client::builder()
                .default_headers(headers)
                .connect_timeout(HTTP_CONNECT_TIMEOUT),
        );
        if config.local_agent_endpoint {
            // A local app endpoint must never see its agent-id header travel
            // through a proxy or follow a redirect off the machine.
            builder = builder
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none());
        }
        // rmcp requires reqwest 0.13; the approved xai helper is typed for 0.12.
        #[allow(clippy::disallowed_methods)]
        let client = builder
            .build()
            .map_err(|e| McpError::ClientError(format!("Failed to build HTTP client: {e}")))?;
        Ok(crate::mcp_http_client::McpHttpClient::new(
            client,
            server_name,
            warn_budget,
        ))
    }

    /// Cheap, non-blocking liveness predicate.
    /// Inspects the current [`ClientState`] under the state mutex only.
    /// It MUST NOT call [`Self::ensure_initialized`] or any other path that can trigger a network round-trip.
    pub async fn is_healthy(&self) -> bool {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready { service, .. } => !service.is_transport_closed(),
            _ => false,
        }
    }

    /// Atomic classification for the liveness watcher.
    /// The watcher depends on this distinction: a plain `is_healthy`-based predicate cannot tell the cases apart.
    /// It would false-fire `TransportClosed` on re-handshake transitions.
    pub async fn liveness_check(&self) -> LivenessCheck {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready { service, .. } => {
                if service.is_transport_closed() {
                    LivenessCheck::TransportClosed
                } else {
                    LivenessCheck::Healthy
                }
            }
            _ => LivenessCheck::Transient,
        }
    }
    pub async fn is_ready(&self) -> bool {
        self.state_kind().await == ClientStateKind::Ready
    }

    /// State-machine snapshot for diagnostics and downstream UI.
    /// Like [`Self::is_healthy`], this is a cheap state inspection (no handshake, no network I/O).
    /// Maps [`ClientState`] onto a `Copy` enum so callers can match without holding a reference to the inner [`McpService`] / [`PendingTransport`].
    pub async fn state_kind(&self) -> ClientStateKind {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Empty => ClientStateKind::Empty,
            ClientState::Pending(_) => ClientStateKind::Pending,
            ClientState::Initializing => ClientStateKind::Initializing,
            ClientState::Ready { .. } => ClientStateKind::Ready,
        }
    }

    /// Write this server's tool descriptors as JSON files under `<server_dir>/tools/`.
    /// Resource descriptors would advertise MCP-resource tools the model can't use.
    /// Best-effort: errors writing individual descriptors are logged but don't abort the run.
    pub async fn materialize_descriptors(
        &self,
        server_dir: &std::path::Path,
    ) -> Result<usize, McpError> {
        let mcp_service = self.ensure_initialized().await?;

        // Collect descriptors via the async MCP API, then defer all filesystem work to a single `spawn_blocking`
        // The executor is then never blocked on `std::fs` (this runs on every MCP tool-set change, not just startup)
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = mcp_service
                .list_tools(Some(
                    PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await?;
            for tool in result.tools {
                let descriptor = serde_json::json!({
                    "name": tool.name.as_ref(),
                    "description": tool.description.as_deref(),
                    "inputSchema": tool.input_schema.as_ref(),
                });
                match serde_json::to_vec_pretty(&descriptor) {
                    Ok(bytes) => files.push((
                        format!("{}.json", sanitize_descriptor_segment(tool.name.as_ref())),
                        bytes,
                    )),
                    Err(e) => tracing::warn!(
                        tool = %tool.name.as_ref(),
                        error = %e,
                        "failed to serialize MCP tool descriptor",
                    ),
                }
            }
            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        // Write each descriptor atomically (temp file + rename) so a concurrent reader never sees a half-written JSON
        // Overlapping writers converge without a lock
        let tools_dir = server_dir.join("tools");
        tokio::task::spawn_blocking(move || -> Result<usize, McpError> {
            std::fs::create_dir_all(&tools_dir).map_err(|e| {
                McpError::ClientError(format!(
                    "failed to create MCP tools descriptor dir {}: {e}",
                    tools_dir.display()
                ))
            })?;
            let mut written = 0usize;
            for (file_name, bytes) in files {
                let path = tools_dir.join(&file_name);
                let write_result =
                    tempfile::NamedTempFile::new_in(&tools_dir).and_then(|mut tmp| {
                        std::io::Write::write_all(&mut tmp, &bytes)?;
                        tmp.persist(&path).map_err(|e| e.error)
                    });
                match write_result {
                    Ok(_) => written += 1,
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to write MCP tool descriptor",
                    ),
                }
            }
            Ok(written)
        })
        .await
        .map_err(|e| McpError::ClientError(format!("descriptor write task panicked: {e}")))?
    }

    /// Read the server's `instructions` from the MCP initialize handshake.
    /// Returns `None` if the client isn't ready yet.
    pub async fn server_instructions(&self) -> Option<String> {
        let guard = self.state.lock().await;
        if let ClientState::Ready { service, .. } = &*guard {
            service
                .peer_info()?
                .instructions
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(String::from)
        } else {
            None
        }
    }

    // Server icons stay on peer_info (handshake) and are re-read here
    // Tool icons are snapshotted into McpState at registration because tools/list is not re-fetched for every status build
    pub async fn server_icons(&self) -> Vec<McpIcon> {
        let guard = self.state.lock().await;
        match &*guard {
            ClientState::Ready { service, .. } => service
                .peer_info()
                .and_then(|info| info.server_info.clone())
                .map(|server_info| McpIcon::from_rmcp_list(server_info.icons))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// The handshake plus the first `tools/list`, bounded so a server that connects and then stalls cannot hold a
    /// caller indefinitely.
    pub async fn get_tool_registrations(
        &self,
        mcp_state: Arc<Mutex<McpState>>,
    ) -> Result<Vec<McpToolRegistration>, McpError> {
        let list_window = self
            .startup_timeout_sec
            .max(Self::DISCOVER_PROBE_TIMEOUT_SECS);
        let budget = std::time::Duration::from_secs(
            self.handshake_worst_case_secs().saturating_add(list_window),
        );
        match tokio::time::timeout(budget, self.list_tool_registrations(mcp_state)).await {
            Ok(result) => result,
            Err(_) => Err(McpError::timeout(&self.server_name, budget)),
        }
    }

    async fn list_tool_registrations(
        &self,
        mcp_state: Arc<Mutex<McpState>>,
    ) -> Result<Vec<McpToolRegistration>, McpError> {
        // No timers across the initialize / list awaits: `InstrumentationTimer`
        // holds a Chrome-mode span guard that must not cross an await
        // (`!Send`, and tracing's span stack is per-thread).
        let mcp_service = self.ensure_initialized().await?;

        let list_tools_start = std::time::Instant::now();
        let list_tools_span = distill_telemetry::region::Region::from_span(tracing::info_span!(
            "mcp.list_tools",
            tools_count = tracing::field::Empty,
            elapsed_ms = tracing::field::Empty,
        ));

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let list_tools_result = mcp_service
                .list_tools(Some(
                    PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await?;

            all_tools.extend(list_tools_result.tools);

            match list_tools_result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        list_tools_span
            .span()
            .record("tools_count", all_tools.len() as i64);
        list_tools_span
            .span()
            .record("elapsed_ms", list_tools_start.elapsed().as_millis() as i64);
        list_tools_span.close();

        let event_writer = mcp_state.lock().await.event_writer().clone();
        let listed_count = all_tools.len();
        let registrations: Vec<_> = all_tools
            .into_iter()
            .filter_map(|tool| {
                let meta: Option<serde_json::Value> = tool
                    .meta
                    .as_ref()
                    .and_then(|m| serde_json::to_value(m).ok());

                let name = tool.name.to_string();
                let description = tool.description.map(|d| d.to_string()).unwrap_or_default();
                let mut schema = serde_json::to_value(tool.input_schema.as_ref())
                    .unwrap_or_else(|_| serde_json::json!({"type": "object"}));
                // Ensure the schema has "type": "object"
                // Some MCP servers send `inputSchema: {}` for tools with no parameters
                // Azure's OpenAI API rejects schemas without a `type` field with: 'schema must be a JSON Schema of type: "object", got type: "None"'
                if let Some(obj) = schema.as_object_mut() {
                    obj.entry("type")
                        .or_insert_with(|| serde_json::json!("object"));
                    obj.entry("properties")
                        .or_insert_with(|| serde_json::json!({}));
                }

                let icons = McpIcon::from_rmcp_list(tool.icons);
                let mcp_tool = McpTool {
                    name: name.clone(),
                    description,
                    server_name: self.server_name.clone(),
                    mcp_state: Arc::clone(&mcp_state),
                    schema,
                    meta,
                };
                match mcp_tool.into_registration() {
                    Ok(mut reg) => {
                        reg.icons = icons;
                        Some(reg)
                    }
                    Err(reason) => {
                        event_writer.emit(
                            distill_session_events::Event::McpToolRegistrationFailed {
                                server_name: self.server_name.clone(),
                                tool_name: name,
                                error: reason.to_string(),
                            },
                        );
                        None
                    }
                }
            })
            .collect();
        if registrations.len() != listed_count {
            tracing::warn!(
                server = %self.server_name,
                listed = listed_count,
                admitted = registrations.len(),
                skipped = listed_count.saturating_sub(registrations.len()),
                "MCP tools/list included tools skipped by session admission"
            );
        }

        // Warn about tool_timeouts keys that don't match any discovered tool.
        // This catches typos like `creat_issue` instead of `create_issue`.
        if !self.tool_timeouts.is_empty() {
            // Registration names are qualified ("server__tool"); tool_timeouts keys are raw tool names
            // Strip the server prefix for comparison
            let prefix = format!("{}{}", self.server_name, MCP_TOOL_NAME_DELIMITER);
            let raw_names: Vec<&str> = registrations
                .iter()
                .map(|r| r.name.strip_prefix(prefix.as_str()).unwrap_or(&r.name))
                .collect();
            let discovered: std::collections::HashSet<&str> = raw_names.iter().copied().collect();
            for key in self.tool_timeouts.keys() {
                if !discovered.contains(key.as_str()) {
                    tracing::info!(
                        server = %self.server_name,
                        tool_timeout_key = %key,
                        "tool_timeouts entry '{}' does not match any tool exposed by MCP server '{}' \
                         (available: {}). The per-tool timeout will have no effect — check for typos.",
                        key,
                        self.server_name,
                        raw_names.join(", "),
                    );
                }
            }
        }

        Ok(registrations)
    }

    /// Call a tool directly on the MCP server (for testing/debugging).
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<rmcp::model::CallToolResult, McpError> {
        let mcp_service = self.ensure_initialized().await?;
        let result = mcp_service
            .call_tool({
                let mut params = CallToolRequestParams::new(tool_name.to_string());
                params.arguments = arguments.as_object().cloned();
                params
            })
            .await?;
        Ok(result)
    }
}

fn contains_session_placeholder(value: &str) -> bool {
    value.contains("{{session_id}}") || value.contains("${session_id}")
}

/// Sanitize an MCP server name into a safe filename component.
fn sanitize_mcp_log_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .take(96)
        .map(|c| match c {
            c if c.is_ascii_alphanumeric() => c,
            '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    if sanitized.is_empty() {
        "server".into()
    } else {
        sanitized
    }
}

/// Copy an MCP server's stderr to `~/.grok/logs/mcp/<server>.stderr.log`
/// in a background task. Truncated per spawn.
fn drain_mcp_stderr_to_log(server_name: &str, mut stderr: tokio::process::ChildStderr) {
    let log_dir = distill_config::distill_home().join("logs").join("mcp");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        tracing::warn!("MCP stderr drain: failed to create log dir: {e}");
        return;
    }
    let log_path = log_dir.join(format!(
        "{}.stderr.log",
        sanitize_mcp_log_filename(server_name)
    ));

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                "MCP stderr drain: failed to open {}: {e}",
                log_path.display()
            );
            return;
        }
    };
    let mut file = tokio::fs::File::from_std(file);
    let server_name = server_name.to_string();
    tokio::spawn(async move {
        if let Err(e) = tokio::io::copy(&mut stderr, &mut file).await {
            tracing::warn!("MCP stderr drain '{server_name}': {e}");
        }
    });
}

fn expand_session_id_headers(
    headers: Vec<acp::HttpHeader>,
    session_id: Option<&str>,
) -> Vec<(String, String)> {
    headers
        .into_iter()
        .filter_map(|header| {
            let value = header.value;
            if let Some(session_id) = session_id {
                let expanded = value
                    .replace("{{session_id}}", session_id)
                    .replace("${session_id}", session_id);
                Some((header.name, expanded))
            } else if contains_session_placeholder(&value) {
                None
            } else {
                Some((header.name, value))
            }
        })
        .collect()
}

/// Decide the actual (program, args) to spawn for a stdio MCP server.
/// `CreateProcessW` only appends `.exe` and ignores `PATHEXT`, so `Command::new("npx")` fails with "file not found".
/// On non-Windows we never touch the command. A command containing a path separator is used as-is.
fn plan_stdio_spawn(
    command: &str,
    args: &[String],
    is_windows: bool,
    resolve: impl Fn(&str) -> Option<std::path::PathBuf>,
) -> (OsString, Vec<String>) {
    if is_windows
        && !command.contains('/')
        && !command.contains('\\')
        && let Some(resolved) = resolve(command)
    {
        return (resolved.into_os_string(), args.to_vec());
    }
    (OsString::from(command), args.to_vec())
}

fn is_figma_mcp(server_name: &str, url: &str) -> bool {
    if server_name.eq_ignore_ascii_case("figma") {
        return true;
    }
    // Legacy direct managed name (`grok_com_figma`); newer clients use gateway tools (`managed_mcp_gateway_tools_enabled`).
    const MANAGED_PREFIX: &str = "grok_com_";
    if let (Some(prefix), Some(rest)) = (
        server_name.get(..MANAGED_PREFIX.len()),
        server_name.get(MANAGED_PREFIX.len()..),
    ) && prefix.eq_ignore_ascii_case(MANAGED_PREFIX)
        && rest.eq_ignore_ascii_case("figma")
    {
        return true;
    }
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .is_some_and(|h| h == "figma.com" || h.ends_with(".figma.com"))
}

fn ensure_figma_user_agent(headers: &mut reqwest::header::HeaderMap, server_name: &str, url: &str) {
    if !is_figma_mcp(server_name, url) {
        return;
    }
    if headers.contains_key(reqwest::header::USER_AGENT) {
        return;
    }
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("grok-cli"),
    );
}

static DEFAULT_USER_AGENT: LazyLock<reqwest::header::HeaderValue> = LazyLock::new(|| {
    format!("grok-cli/{}", distill_version::VERSION)
        .parse()
        .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static("grok-cli"))
});

fn ensure_default_user_agent(headers: &mut reqwest::header::HeaderMap) {
    if headers.contains_key(reqwest::header::USER_AGENT) {
        return;
    }
    headers.insert(reqwest::header::USER_AGENT, DEFAULT_USER_AGENT.clone());
}

/// Figma keeps its pinned bare `grok-cli` attribution token; every other server gets the versioned default.
/// A `User-Agent` already in the map always wins.
fn apply_user_agent_policy(headers: &mut reqwest::header::HeaderMap, server_name: &str, url: &str) {
    ensure_figma_user_agent(headers, server_name, url);
    ensure_default_user_agent(headers);
}

/// Convert configured header pairs to a `HeaderMap` for every MCP streamable-HTTP request path.
/// One bad configured pair is reported by both the anonymous probe and the transport build on a single connection attempt.
fn parse_config_headers<'a>(
    server_name: &str,
    stage: &'static str,
    pairs: impl Iterator<Item = (&'a str, &'a str)>,
) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    for (key, value) in pairs {
        match (
            reqwest::header::HeaderName::from_bytes(key.as_bytes()),
            value.parse::<reqwest::header::HeaderValue>(),
        ) {
            (Ok(name), Ok(val)) => {
                headers.insert(name, val);
            }
            _ => {
                tracing::warn!(
                    server = server_name,
                    stage,
                    "Skipping invalid MCP HTTP header: {key}"
                );
            }
        }
    }
    headers
}

fn stdio_path_override(env: &[acp::EnvVariable]) -> Option<&str> {
    env.iter()
        .find(|e| e.name.eq_ignore_ascii_case("PATH"))
        .map(|e| e.value.as_str())
}

fn apply_stdio_env(cmd: &mut Command, env: &[acp::EnvVariable], session_id: Option<&str>) {
    for env_variable in env {
        cmd.env(&env_variable.name, &env_variable.value);
    }
    if let Some(session_id) = session_id {
        cmd.env("GROK_SESSION_ID", session_id);
    }
}

/// Borrowed cross-cutting spawn context whose `scope`, when set, enrolls the stdio child for session-close reaping.
pub struct McpSpawnCtx<'a> {
    pub(crate) session_id: Option<&'a str>,
    pub(crate) event_writer: &'a distill_session_events::EventWriter,
    pub(crate) mode: OauthInteractivity,
    pub(crate) scope: Option<&'a ProcessScope>,
    pub(crate) discovery: McpOauthDiscovery,
    send_grok_agent_id_header: bool,
}

impl<'a> McpSpawnCtx<'a> {
    pub fn for_session(
        session_id: &'a str,
        event_writer: &'a distill_session_events::EventWriter,
        mode: OauthInteractivity,
        scope: Option<&'a ProcessScope>,
    ) -> Self {
        Self {
            session_id: Some(session_id),
            event_writer,
            mode,
            scope,
            discovery: McpOauthDiscovery::Disk,
            send_grok_agent_id_header: false,
        }
    }

    pub fn with_grok_agent_id_header(mut self) -> Self {
        self.send_grok_agent_id_header = true;
        self
    }

    pub fn standalone(event_writer: &'a distill_session_events::EventWriter) -> Self {
        Self {
            session_id: None,
            event_writer,
            mode: OauthInteractivity::Interactive,
            scope: None,
            discovery: McpOauthDiscovery::Disk,
            send_grok_agent_id_header: false,
        }
    }

    #[must_use]
    pub fn with_oauth_discovery(mut self, discovery: McpOauthDiscovery) -> Self {
        self.discovery = discovery;
        self
    }
}

pub async fn start_mcp_server(
    mcp_server: acp::McpServer,
    overrides: Option<&McpClientTimeoutOverrides>,
    meta_config: Option<&McpServerMetaConfig>,
    byo_config: Option<&McpOAuthConfig>,
    ctx: &McpSpawnCtx<'_>,
) -> Result<McpClient, McpError> {
    // No whole-start timer here: `InstrumentationTimer` holds a Chrome-mode span guard that must not cross an await (it is `!Send` and tracing's span stack is per-thread), and this fn awaits on every transport. Durations are carried by the per-transport telemetry events instead.
    match mcp_server {
        acp::McpServer::Stdio(acp::McpServerStdio {
            name,
            command,
            args,
            env,
            ..
        }) => {
            if let Some(mc) = meta_config {
                tracing::info!(server = %name, ?mc, "MCP stdio: meta config override");
            }

            let (startup_timeout, _, _) = McpClient::load_timeouts(overrides, meta_config);
            let command_str = command.to_string_lossy().into_owned();
            let spawn_start = std::time::Instant::now();
            // Scoped to the sync spawn-planning prologue: the timer's
            // Chrome-mode span guard must not cross the spawn await below.
            let cmd = {
                let mut stdio_spawn_timer =
                    distill_telemetry::instrumentation::timer("mcp_stdio_spawn");
                stdio_spawn_timer.with_server(name.as_str());
                let path_override = stdio_path_override(&env);
                let (program, spawn_args) =
                    plan_stdio_spawn(&command_str, &args, cfg!(windows), |c| {
                        if let Some(path) = path_override
                            && let Ok(cwd) = std::env::current_dir()
                        {
                            which::which_in(c, Some(path), cwd).ok()
                        } else {
                            which::which(c).ok()
                        }
                    });
                let mut cmd = Command::new(&program);
                cmd.kill_on_drop(true).args(&spawn_args);
                apply_stdio_env(&mut cmd, &env, ctx.session_id);
                distill_tools::util::detach_command(&mut cmd);
                distill_sandbox::child_net::restrict_child_network(&mut cmd);
                cmd
            };

            let spawn_child_start = std::time::Instant::now();
            let spawn_child_span =
                distill_telemetry::region::Region::from_span(tracing::info_span!(
                    "mcp.spawn_child",
                    server_name = %name,
                    elapsed_ms = tracing::field::Empty,
                ));
            let spawn_result = SafeTokioChildProcess::spawn(
                cmd,
                ctx.scope,
                name.clone(),
                ctx.event_writer.clone(),
            )
            .await;
            spawn_child_span
                .span()
                .record("elapsed_ms", spawn_child_start.elapsed().as_millis() as i64);
            spawn_child_span.close();
            let (transport, stderr_handle) = spawn_result.map_err(|e| {
                tracing::error!("Failed to spawn MCP server '{}': {}", name, e);
                distill_telemetry::session_ctx::log_event(
                    distill_telemetry::events::McpServerFailed {
                        server_name: name.clone(),
                        error_type: distill_telemetry::events::McpErrorType::SpawnFailed,
                        duration_ms: spawn_start.elapsed().as_millis() as u64,
                        timeout_sec: startup_timeout,
                        error_message: Some(e.to_string()),
                    },
                );
                McpError::SpawnFailed {
                    server: name.clone(),
                    source: e,
                }
            })?;

            tracing::debug!("MCP server '{}' spawned: PID={:?}", name, transport.id());

            if let Some(stderr) = stderr_handle {
                drain_mcp_stderr_to_log(&name, stderr);
            }

            Ok(McpClient::new_stdio(
                name.clone(),
                transport,
                overrides,
                meta_config,
            ))
        }
        acp::McpServer::Http(acp::McpServerHttp {
            name, url, headers, ..
        })
        | acp::McpServer::Sse(acp::McpServerSse {
            name, url, headers, ..
        }) => {
            if let Some(mc) = meta_config {
                tracing::info!(server = %name, %url, ?mc, "MCP http: meta config override");
            }

            let mut headers = expand_session_id_headers(headers, ctx.session_id);
            // Stripped unconditionally: the agent-id header identifies the session to first-party app endpoints, and a caller-supplied config must not be able to impersonate one (see
            // [`GROK_AGENT_ID_HEADER`]). Re-added only from the spawn context, like `GROK_SESSION_ID` on stdio servers.
            headers.retain(|(name, _)| !name.eq_ignore_ascii_case(GROK_AGENT_ID_HEADER));
            let local_agent_endpoint = ctx.send_grok_agent_id_header;
            if local_agent_endpoint && let Some(session_id) = ctx.session_id {
                reqwest::header::HeaderValue::try_from(session_id).map_err(|error| {
                    McpError::ClientError(format!("invalid {GROK_AGENT_ID_HEADER} value: {error}"))
                })?;
                headers.push((GROK_AGENT_ID_HEADER.to_owned(), session_id.to_owned()));
            }
            let http_config = HttpConfig {
                url: url.clone(),
                headers,
                local_agent_endpoint,
            };

            let auth_decision = if http_config.has_authorization_header() {
                tracing::debug!(
                    server = %name,
                    "Skipping OAuth discovery: server already has Authorization header"
                );
                HttpAuthDecision::NoOauthSupport
            } else if http_config.local_agent_endpoint {
                // First-party app endpoints addressed by agent id are local
                // desktop processes that never speak OAuth; probing them
                // would only add latency.
                HttpAuthDecision::NoOauthSupport
            } else {
                match ctx.discovery {
                    McpOauthDiscovery::Disk => decide_http_auth_from_disk(&name, &url).await,
                    McpOauthDiscovery::Network => {
                        decide_http_auth_over_network(
                            &name,
                            &url,
                            &http_config.headers,
                            ctx,
                            OAUTH_DISCOVERY_TIMEOUT,
                        )
                        .await
                    }
                }
            };
            match auth_decision {
                HttpAuthDecision::ManagerReady { manager, observed } => {
                    Ok(McpClient::new_http_auth(
                        name.clone(),
                        http_config,
                        manager,
                        observed,
                        byo_config.cloned(),
                        overrides,
                        meta_config,
                    ))
                }
                HttpAuthDecision::NoOauthSupport => Ok(McpClient::new_http(
                    name.clone(),
                    http_config,
                    overrides,
                    meta_config,
                )),
                // Avoid starting an unauthenticated HTTP worker that fatals on server OAuth challenge.
                HttpAuthDecision::NeedsInteractiveLogin => Err(McpError::AuthRequired {
                    server: name.clone(),
                }),
                HttpAuthDecision::Unreachable => Err(McpError::Unreachable {
                    server: name.clone(),
                }),
            }
        }
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive]; reject unknown transports.
        other => Err(McpError::ClientError(format!(
            "unsupported MCP server transport: {other:?}"
        ))),
    }
}

/// Start every configured server concurrently (one future per server, no
/// cap); results are returned in input order.
pub async fn start_mcp_servers(
    mcp_servers: Vec<acp::McpServer>,
    overrides_map: &HashMap<String, McpClientTimeoutOverrides>,
    meta_config_map: &McpMetaConfigMap,
    oauth_config_map: &crate::oauth_config::McpOAuthConfigMap,
    ctx: &McpSpawnCtx<'_>,
) -> Vec<Result<McpClient, McpError>> {
    // No whole-batch timer: it would hold a Chrome-mode span guard across
    // the `join_all` await (see `start_mcp_server`).
    if !meta_config_map.is_empty() {
        tracing::info!(
            count = mcp_servers.len(),
            overrides = ?meta_config_map.keys().collect::<Vec<_>>(),
            "Starting MCP servers with meta config"
        );
    }

    // Uncapped on purpose (one future per server): the count is
    // config-bounded, the work is I/O-bound, and any cap is a slot to wait
    // on behind a stalled server.
    futures::future::join_all(mcp_servers.into_iter().map(|server| {
        let server_name = mcp_server_name(&server).to_owned();
        let overrides = overrides_map.get(&server_name);
        let mc = meta_config_map.get(&server_name);
        let byo = oauth_config_map.get(&server_name);
        async move { start_mcp_server(server, overrides, mc, byo, ctx).await }
    }))
    .await
}

/// Extract the name from an MCP server enum variant.
pub fn mcp_server_name(server: &acp::McpServer) -> &str {
    match server {
        acp::McpServer::Stdio(stdio) => &stdio.name,
        acp::McpServer::Http(http) => &http.name,
        acp::McpServer::Sse(sse) => &sse.name,
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "unknown",
    }
}

pub fn mcp_transport_str(server: &acp::McpServer) -> &'static str {
    match server {
        acp::McpServer::Stdio(_) => "stdio",
        acp::McpServer::Http(_) => "http",
        acp::McpServer::Sse(_) => "sse",
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => "unknown",
    }
}

pub fn mcp_target_str(server: &acp::McpServer) -> String {
    match server {
        acp::McpServer::Stdio(acp::McpServerStdio { command, args, .. }) => {
            let cmd = command.to_string_lossy();
            if args.is_empty() {
                cmd.to_string()
            } else {
                format!("{} {}", cmd, args.join(" "))
            }
        }
        acp::McpServer::Http(acp::McpServerHttp { url, .. })
        | acp::McpServer::Sse(acp::McpServerSse { url, .. }) => url.clone(),
        // TODO(acp-0.10): `McpServer` is #[non_exhaustive].
        _ => String::new(),
    }
}

impl McpClient {
    /// Minimal stub for unit tests in dependent crates.
    /// Hidden from rustdoc; not gated behind `#[cfg(test)]` so cross-crate test code can construct it without the host crate enabling a feature.
    #[doc(hidden)]
    pub fn stub(name: &str) -> Self {
        // Route through the single constructor (so new fields never need touching here), then downgrade to the no-transport placeholder
        // `Empty` state makes `ensure_initialized` error, and `reconnect = None` makes `reset_transport` return false
        // That is a client that can't reconnect, like a dead Stdio child
        let overrides = McpClientTimeoutOverrides {
            startup_timeout_sec: Some(10),
            tool_timeout_sec: Some(60),
            ..Default::default()
        };
        let mut client = Self::new_with_transport(
            name.to_string(),
            PendingTransport::Http(HttpConfig {
                url: String::new(),
                headers: Vec::new(),
                local_agent_endpoint: false,
            }),
            Some(&overrides),
            None,
            None,
            None,
            None,
            None,
        );
        *client.state.get_mut() = ClientState::Empty;
        client.reconnect = None;
        client
    }
}

/// rmcp [`ClientHandler`] used by all MCP transports.
/// If the receiver has been dropped (subagent teardown, session shutdown, or never wired; see [`McpClient::notify_tx`]), the send fails silently.
/// rmcp must not see an error from a notification handler or the service loop tears down.
#[derive(Debug)]
pub struct GrokClientHandler {
    /// Static `ClientInfo` returned by [`Self::get_info`]; built once at handshake time and stored to avoid re-allocating per call.
    info: ClientInfo,
    /// MCP server name this handler is bound to.
    /// Cloned into emitted events so the dispatcher can route per-server.
    server_name: McpServerName,
    /// **Shared** event sink: the same Arc lives on the owning [`McpClient`].
    /// Mutating the slot via [`McpClient::set_event_tx`] is observed here on the next read.
    /// So wiring the sender post-handshake works without restarting the rmcp service loop.
    notify_tx: SharedEventTx,
    elicitation_tx: crate::elicitation::SharedElicitationTx,
}

impl GrokClientHandler {
    /// Best-effort event emit.
    /// Splitting this out keeps the trait methods short.
    fn emit(&self, ev: McpClientEvent) {
        let sender = self.notify_tx.lock().clone();
        if let Some(tx) = sender {
            let _ = tx.send(ev);
        }
    }
}

impl ClientHandler for GrokClientHandler {
    // NOTE: `async fn` here is sugar for the trait's `-> impl Future<Output = ()> + Send + '_`
    // We INTENTIONALLY do not use `#[async_trait]` rmcp 2.1's `ClientHandler` declares its notification methods as return-position `impl Future` async_trait would produce a different (incompatible) signature.
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(McpClientEvent::ToolsChanged {
            server: self.server_name.clone(),
        });
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.emit(McpClientEvent::ResourcesChanged {
            server: self.server_name.clone(),
        });
    }

    async fn create_elicitation(
        &self,
        request: rmcp::model::ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ElicitResult, rmcp::ErrorData> {
        tracing::info!(
            server = %self.server_name,
            "MCP elicitation/create received"
        );
        // `context.ct` fires when the server cancels this request (`notifications/cancelled`)
        // Dropping the bridge future closes the job's response channel, which the shell coordinator observes to tear down the HITL card
        // Otherwise the popup would outlive the abandoned request and answer into the void
        let bridged =
            crate::elicitation::bridge_elicit(&self.elicitation_tx, &self.server_name, request);
        tokio::select! {
            result = bridged => Ok(result),
            _ = context.ct.cancelled() => {
                tracing::info!(
                    server = %self.server_name,
                    "elicitation/create cancelled by server; abandoning HITL bridge"
                );
                Ok(crate::elicitation::cancel_result())
            }
        }
    }

    // rmcp 3.x dropped the typed URL-elicitation completion handler; the notification (either the 2026-07-28 `notifications/elicitation/response` or the 2025-11-25 `notifications/elicitation/complete` spelling) now arrives through the custom-notification catch-all.
    async fn on_custom_notification(
        &self,
        notification: rmcp::model::CustomNotification,
        _context: NotificationContext<RoleClient>,
    ) {
        if notification.method != "notifications/elicitation/response"
            && notification.method != "notifications/elicitation/complete"
        {
            tracing::debug!(
                server = %self.server_name,
                method = %notification.method,
                "ignoring unknown MCP notification"
            );
            return;
        }
        let Some(elicitation_id) = notification
            .params
            .as_ref()
            .and_then(|p| p.get("elicitationId"))
            .and_then(|v| v.as_str())
        else {
            tracing::warn!(
                server = %self.server_name,
                method = %notification.method,
                "elicitation completion notification without elicitationId; dropping"
            );
            return;
        };
        if !distill_tools::mcp_elicitation::chars_within(
            elicitation_id,
            distill_tools::mcp_elicitation::MAX_ELICIT_ID_CHARS,
        ) {
            tracing::warn!(
                server = %self.server_name,
                "oversized elicitation_id on complete; dropping"
            );
            return;
        }
        self.emit(McpClientEvent::ElicitationComplete {
            server: self.server_name.clone(),
            elicitation_id: elicitation_id.to_string(),
        });
    }

    fn get_info(&self) -> ClientInfo {
        self.info.clone()
    }
}

#[cfg(test)]
#[path = "servers_tests.rs"]
mod tests;
