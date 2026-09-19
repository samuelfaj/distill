// Modified for Distill by Samuel Fajreldines, 2026.
use indexmap::IndexMap;
use std::sync::Arc;
use std::time::Instant;

use crate::permission::{
    bash_command_splitting::{BashCommandHighlights, primary_command_from_script},
    grants::web_fetch_deny_key_from_url,
    types::{AccessKind, ClientType, HOOK_ASK_META_KEY, HookAsk},
};
use agent_client_protocol::{self as acp, Client as _};
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_mcp::servers::parse_mcp_qualified_name;
use distill_session_events::{Event, EventWriter, PermissionDecision};
use distill_tools::implementations::distill::web_fetch::domain_from_url;

const REJECT_ONCE_LABEL: &str = "No, and tell Grok what to do differently";

/// Stable option id for "allow all edits this session", distinct from `"always-allow"` so it maps to [`PromptOutcome::AllowEditsForSession`].
/// Session-only and never persisted; exposed so the pager does not record it as a sticky cursor target.
pub const ALLOW_EDITS_SESSION_OPTION_ID: &str = "allow-edits-session";

/// Stable id for the "enable always-approve mode" option prepended for TUI / Pager / Desktop clients.
/// Shell maps it to [`PromptOutcome::AllowOnce`] and persists nothing; the pager separately fires `set_yolo_mode(true)`.
/// Keeps the wire plain ACP. An unrecognized client still gets `AllowOnce`; worst case the current call is granted but the toggle does not flip.
pub const ENABLE_ALWAYS_APPROVE_OPTION_ID: &str = "enable-always-approve";

/// Defined once so the label is identical across every permission prompt (edit, bash, MCP, web_fetch, fallback).
const ENABLE_ALWAYS_APPROVE_LABEL: &str =
    "Yes, and don't ask again for anything (always-approve mode)";

/// Build the "enable always-approve mode" option prepended to every TUI/Pager/Desktop prompt; see [`ENABLE_ALWAYS_APPROVE_OPTION_ID`].
/// `kind` is `AllowOnce` so the pager's YOLO auto-approve drain, which answers with the first `AllowOnce`, picks this option.
/// That is safe: the drain bypasses `dispatch_permission_select`, so picking it does not re-fire `set_yolo_mode(true)`.
fn enable_always_approve_option() -> acp::PermissionOption {
    acp::PermissionOption::new(
        ENABLE_ALWAYS_APPROVE_OPTION_ID,
        ENABLE_ALWAYS_APPROVE_LABEL.to_owned(),
        acp::PermissionOptionKind::AllowOnce,
    )
}

/// Canonical check for the "enable always-approve mode" option; match on this, not the label or position 0.
pub fn is_enable_always_approve_option(opt: &acp::PermissionOption) -> bool {
    opt.option_id.0.as_ref() == ENABLE_ALWAYS_APPROVE_OPTION_ID
}

/// Only `GrokTUI`, `GrokPager`, and `Desktop` wire the option id through to their YOLO toggle; other clients keep their existing option set.
fn client_supports_enable_always_approve(client_type: ClientType) -> bool {
    matches!(
        client_type,
        ClientType::GrokTUI | ClientType::GrokPager | ClientType::Desktop
    )
}

/// Prepend the "enable always-approve mode" option at position 0 for client types that can act on it.
/// [`AcpPrompter::build_options`] calls this at the tail of every branch so the option lands first regardless of the base map.
fn prepend_enable_always_approve(
    client_type: ClientType,
    base: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
) -> IndexMap<acp::PermissionOptionId, acp::PermissionOption> {
    if !client_supports_enable_always_approve(client_type) {
        return base;
    }
    let mut with_yolo: IndexMap<acp::PermissionOptionId, acp::PermissionOption> = IndexMap::new();
    let opt = enable_always_approve_option();
    with_yolo.insert(opt.option_id.clone(), opt);
    // `IndexMap::extend` preserves order
    // A duplicate id in `base` would overwrite this entry, but every base option id is distinct from `ENABLE_ALWAYS_APPROVE_OPTION_ID`
    with_yolo.extend(base);
    with_yolo
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BashCommandPermission {
    pub prompt_prefix: String,
}

/// The command terms the user selected; the selection is independent of the prompt's outcome.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BashCommandSelectedTerms {
    pub command_parts: Vec<String>,
    /// The user typed a free-form glob in the pattern editor rather than selecting literal words, so match it as a glob.
    #[serde(default)]
    pub is_glob: bool,
}

/// Delimiter that qualifies MCP tool names as `"<server>__<tool>"`.
/// Defined in `distill_workspace_types`; re-exported here because callers historically reached it through this module.
/// MCP registration validates the delimiter before permission handling, so stripping it given a trusted `server_prefix` is unambiguous.
pub use distill_workspace_types::MCP_TOOL_NAME_DELIMITER;

/// Extract the action segment of a qualified MCP tool name using a trusted `server_prefix`.
/// Returns the full `tool_name` when there is no server prefix.
/// When there is one, debug builds assert that `tool_name` starts with `"<server_prefix>__"`.
pub fn mcp_tool_action<'a>(tool_name: &'a str, server_prefix: Option<&str>) -> &'a str {
    let Some(prefix) = server_prefix else {
        return tool_name;
    };
    let action = tool_name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix(MCP_TOOL_NAME_DELIMITER));
    debug_assert!(
        action.is_some(),
        "MCP tool name invariant: '{tool_name}' should start with '{prefix}{MCP_TOOL_NAME_DELIMITER}'"
    );
    action.unwrap_or(tool_name)
}

/// Pretty-format one MCP server- or tool-name segment: split on `'_'`, title-case each word, join with spaces.
/// Non-underscore characters (camelCase, hyphens) stay intact: `"list_issues"` becomes `"List Issues"` but `"getMyTaskList"` stays `"GetMyTaskList"`.
pub fn mcp_titleize_segment(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// User-facing tool label, e.g. `"(Linear) List Issues"`; falls back to the title-cased `tool_name` when there is no server prefix.
/// The `"(Server) Action"` form shows the owning server without relying on color (some clients are monochrome or use color for other state).
pub fn mcp_tool_display_name(tool_name: &str, server_prefix: Option<&str>) -> String {
    let action = mcp_tool_action(tool_name, server_prefix);
    match server_prefix {
        Some(server) => format!(
            "({}) {}",
            mcp_titleize_segment(server),
            mcp_titleize_segment(action)
        ),
        None => mcp_titleize_segment(tool_name),
    }
}

/// Display variant for callers that only have a tool-name string, such as ACP activity titles or scrollback blocks storing the wire name verbatim.
/// A valid qualified name formats as `"(Server) Action"` with each segment title-cased.
/// Anything else is returned unchanged: the input may be a bash command, file path, or other non-MCP text.
pub fn mcp_pretty_name_if_qualified(name: &str) -> String {
    match parse_mcp_qualified_name(name) {
        Some((_, server, action)) => format!(
            "({}) {}",
            mcp_titleize_segment(server),
            mcp_titleize_segment(action)
        ),
        None => name.to_owned(),
    }
}

/// Meta attached to the "Always allow" option for an MCP tool prompt.
/// Carries the full tool name and the server-prefix segment so the view can render the scope toggle without re-parsing the name.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpToolPermission {
    /// Static label prefix shown before the dynamic scope text, e.g. `"Always allow:"`.
    /// Mirrors `BashCommandPermission::prompt_prefix`.
    pub prompt_prefix: String,
    /// Full tool name as the agent called it (e.g. `"grok_com_notion__notion-fetch"`).
    pub tool_name: String,
    /// Server component of a valid qualified MCP ID (e.g. `"grok_com_notion"`).
    /// `None` for malformed or unqualified names, in which case the view hides the scope toggle and only offers tool-scope.
    pub server_prefix: Option<String>,
}

impl McpToolPermission {
    /// Action segment of the qualified tool name. See [`mcp_tool_action`].
    pub fn action(&self) -> &str {
        mcp_tool_action(&self.tool_name, self.server_prefix.as_deref())
    }

    /// User-facing tool label. See [`mcp_tool_display_name`].
    pub fn display_name(&self) -> String {
        mcp_tool_display_name(&self.tool_name, self.server_prefix.as_deref())
    }
}

/// User's selected scope for an MCP "always allow" grant.
/// Sent back from the view in `RequestPermissionResponse::meta` when the user picks the AllowAlways option for an MCP prompt.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpScopeSelection {
    /// Whitelist exactly this tool name.
    Tool { tool_name: String },
    /// Whitelist the server component of the current valid qualified MCP ID.
    Server { server: String },
}

#[derive(Debug)]
pub enum PromptOutcome {
    AllowOnce,
    AllowAlways,
    /// Session-scoped: allow all edits for the remainder of this session only.
    /// Does **not** persist to disk (unlike the legacy `AllowAlways` path for edits).
    /// Matches the UX of "Yes, allow all edits during this session".
    AllowEditsForSession,
    AllowAlwaysBashCommand(String),
    /// A free-form glob pattern authored in the "Always allow" editor.
    /// Matched as a glob, unlike the literal-prefix [`Self::AllowAlwaysBashCommand`].
    AllowAlwaysBashGlob(String),
    AllowAlwaysDomain(String),
    /// Persist this exact MCP tool name in `allowed_mcp_tools`.
    AllowAlwaysMcpTool(String),
    /// Persist the current valid qualified MCP ID's server component in `allowed_mcp_servers`; the manager rejects mismatched or malformed input.
    AllowAlwaysMcpServer(String),
    RejectOnce,
    RejectAlwaysBashCommand(String),
    /// Persist this exact MCP tool name in `disallowed_mcp_tools`.
    /// Always tool-scoped: there is deliberately no server-scope reject, because disabling a server is a separate concept from a per-tool deny.
    RejectAlwaysMcpTool(String),
    /// Persist the access URL's normalized domain in `disallowed_web_fetch_domains`.
    RejectAlwaysDomain(String),
    Cancelled,
    // If the user provided a followup message instead of an action, the string here will have it
    // TODO: Should the string here be prompt parts instead and should we allow @ and other niceness on the input bar here?
    FollowupMessage(String),
    Error(String),
}

crate::permission::wire_enum! {
    /// Data-free projection of [`PromptOutcome`]: the single owner of the `prompt_outcome` wire vocabulary.
    /// One list generates the enum, its `ALL` inventory, and `wire_str`, and [`PromptOutcome::kind`] matches into it exhaustively.
    /// A new payload-bearing `PromptOutcome` variant fails to compile until it is mapped there and given a list entry here.
    pub enum PromptOutcomeKind {
        AllowOnce => "allow_once",
        AllowAlways => "allow_always",
        AllowEditsForSession => "allow_edits_for_session",
        AllowAlwaysBash => "allow_always_bash",
        AllowAlwaysBashGlob => "allow_always_bash_glob",
        AllowAlwaysDomain => "allow_always_domain",
        AllowAlwaysMcpTool => "allow_always_mcp_tool",
        AllowAlwaysMcpServer => "allow_always_mcp_server",
        RejectOnce => "reject_once",
        RejectAlwaysBash => "reject_always_bash",
        RejectAlwaysMcpTool => "reject_always_mcp_tool",
        RejectAlwaysDomain => "reject_always_domain",
        Cancelled => "cancelled",
        Followup => "followup",
        Error => "error",
    }
}

impl PromptOutcome {
    /// Data-free owner kind for this outcome.
    /// The match is exhaustive, so a new payload-bearing `PromptOutcome` variant fails compilation until it is mapped.
    /// The manager emits `prompt_outcome.kind().wire_str()` (never a raw literal), so this projection is on the production path.
    pub const fn kind(&self) -> PromptOutcomeKind {
        match self {
            Self::AllowOnce => PromptOutcomeKind::AllowOnce,
            Self::AllowAlways => PromptOutcomeKind::AllowAlways,
            Self::AllowEditsForSession => PromptOutcomeKind::AllowEditsForSession,
            Self::AllowAlwaysBashCommand(_) => PromptOutcomeKind::AllowAlwaysBash,
            Self::AllowAlwaysBashGlob(_) => PromptOutcomeKind::AllowAlwaysBashGlob,
            Self::AllowAlwaysDomain(_) => PromptOutcomeKind::AllowAlwaysDomain,
            Self::AllowAlwaysMcpTool(_) => PromptOutcomeKind::AllowAlwaysMcpTool,
            Self::AllowAlwaysMcpServer(_) => PromptOutcomeKind::AllowAlwaysMcpServer,
            Self::RejectOnce => PromptOutcomeKind::RejectOnce,
            Self::RejectAlwaysBashCommand(_) => PromptOutcomeKind::RejectAlwaysBash,
            Self::RejectAlwaysMcpTool(_) => PromptOutcomeKind::RejectAlwaysMcpTool,
            Self::RejectAlwaysDomain(_) => PromptOutcomeKind::RejectAlwaysDomain,
            Self::Cancelled => PromptOutcomeKind::Cancelled,
            Self::FollowupMessage(_) => PromptOutcomeKind::Followup,
            Self::Error(_) => PromptOutcomeKind::Error,
        }
    }
}

pub struct AcpPrompter {
    session_id: acp::SessionId,
    gateway: GatewaySender,
    client_type: ClientType,
    edit_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    bash_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    /// Generic bash options for non-TUI clients: shows complete command with approve/reject always
    generic_bash_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    fallback_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    agent_message_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    /// Per-session `events.jsonl` writer.
    /// [`request`](Self::request) emits a `PermissionRequested` at prompt-start and a paired `PermissionResolved` at decision-time through it.
    /// `EventWriter::noop()` when events recording is disabled (the default for the permission scaffolding's own tests).
    event_writer: EventWriter,
    /// Server permission transport: when set, [`request`](Self::request) asks chat for the decision over the server; `None` keeps the local prompt.
    hub_permission: Option<Arc<dyn crate::permission::PermissionHookTransport>>,
    /// When `false` (default, fail-safe), the per-tool "Always allow …" options are stripped (see [`REMEMBER_TOOL_APPROVALS_GATED_IDS`]).
    remember_tool_approvals: bool,
}

/// Per-tool always-allow/always-reject option ids stripped when the gate is off.
/// `allow-once`, `reject-once`, `enable-always-approve`, and `allow-edits-session` always remain.
const REMEMBER_TOOL_APPROVALS_GATED_IDS: &[&str] = &[
    "allow-always-command",
    "reject-always-command",
    "allow-always-mcp",
    "reject-always-mcp",
    "allow-always-domain",
    "reject-always-domain",
    "always-allow",
    "reject-always",
];

/// Build a bash "don't ask again" row (allow or deny).
/// The static label prefix must equal `BashCommandPermission.prompt_prefix`.
/// The pager rebuilds the label as `"{prompt_prefix} {words}"`, so a prefix drift would flicker on the first ←/→.
fn bash_scope_option(
    id: &str,
    prefix: &str,
    kind: acp::PermissionOptionKind,
    primary: &BashCommandHighlights,
) -> (acp::PermissionOptionId, acp::PermissionOption) {
    (
        acp::PermissionOptionId::new(id),
        acp::PermissionOption::new(
            acp::PermissionOptionId::new(id),
            format!("{prefix} {}", primary.highlighted_words.join(" ")),
            kind,
        )
        .meta(
            serde_json::to_value(BashCommandPermission {
                prompt_prefix: prefix.to_owned(),
            })
            .ok()
            .and_then(|v| v.as_object().cloned()),
        ),
    )
}

impl AcpPrompter {
    pub fn new(
        session_id: acp::SessionId,
        gateway: GatewaySender,
        client_type: ClientType,
    ) -> Self {
        let mut edit_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
            IndexMap::new();
        edit_options.insert(
            acp::PermissionOptionId::new(ALLOW_EDITS_SESSION_OPTION_ID),
            acp::PermissionOption::new(
                ALLOW_EDITS_SESSION_OPTION_ID,
                "Yes, allow all edits during this session".to_owned(),
                acp::PermissionOptionKind::AllowAlways,
            ),
        );
        edit_options.insert(
            acp::PermissionOptionId::new("allow-once"),
            acp::PermissionOption::new(
                "allow-once",
                "Yes".to_owned(),
                acp::PermissionOptionKind::AllowOnce,
            ),
        );
        edit_options.insert(
            acp::PermissionOptionId::new("reject-once"),
            acp::PermissionOption::new(
                "reject-once",
                REJECT_ONCE_LABEL.to_owned(),
                acp::PermissionOptionKind::RejectOnce,
            ),
        );

        // Bash options for GrokTUI: interactive selection with expandable/contractable terms
        let mut bash_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
            IndexMap::new();
        bash_options.insert(
            acp::PermissionOptionId::new("allow-once"),
            acp::PermissionOption::new(
                "allow-once",
                "Yes, proceed".to_owned(),
                acp::PermissionOptionKind::AllowOnce,
            ),
        );
        bash_options.insert(
            acp::PermissionOptionId::new("reject-once"),
            acp::PermissionOption::new(
                "reject-once",
                REJECT_ONCE_LABEL.to_owned(),
                acp::PermissionOptionKind::RejectOnce,
            ),
        );

        // Generic bash options for non-TUI clients (e.g., web): shows complete command inline
        let mut generic_bash_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
            IndexMap::new();
        generic_bash_options.insert(
            acp::PermissionOptionId::new("always-allow"),
            acp::PermissionOption::new(
                "always-allow",
                "Yes, and don't ask again for bash commands".to_owned(),
                acp::PermissionOptionKind::AllowAlways,
            ),
        );
        generic_bash_options.insert(
            acp::PermissionOptionId::new("allow-once"),
            acp::PermissionOption::new(
                "allow-once",
                "Yes, proceed".to_owned(),
                acp::PermissionOptionKind::AllowOnce,
            ),
        );
        generic_bash_options.insert(
            acp::PermissionOptionId::new("reject-once"),
            acp::PermissionOption::new(
                "reject-once",
                REJECT_ONCE_LABEL.to_owned(),
                acp::PermissionOptionKind::RejectOnce,
            ),
        );
        generic_bash_options.insert(
            acp::PermissionOptionId::new("reject-always"),
            acp::PermissionOption::new(
                "reject-always",
                // Persists a deny for the command's primary invocation; see `map_selected_outcome`
                "No, and don't ask again for this command".to_owned(),
                acp::PermissionOptionKind::RejectAlways,
            ),
        );

        let mut fallback_options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
            IndexMap::new();
        fallback_options.insert(
            acp::PermissionOptionId::new("always-allow"),
            acp::PermissionOption::new(
                "always-allow",
                "always allow".to_owned(),
                acp::PermissionOptionKind::AllowAlways,
            ),
        );
        fallback_options.insert(
            acp::PermissionOptionId::new("allow-once"),
            acp::PermissionOption::new(
                "allow-once",
                "allow once".to_owned(),
                acp::PermissionOptionKind::AllowOnce,
            ),
        );
        fallback_options.insert(
            acp::PermissionOptionId::new("reject-once"),
            acp::PermissionOption::new(
                "reject-once",
                "reject once".to_owned(),
                acp::PermissionOptionKind::RejectOnce,
            ),
        );

        let agent_message_options = IndexMap::from([
            (
                acp::PermissionOptionId::new("allow-once"),
                acp::PermissionOption::new(
                    "allow-once",
                    "Yes, send once".to_owned(),
                    acp::PermissionOptionKind::AllowOnce,
                ),
            ),
            (
                acp::PermissionOptionId::new("reject-once"),
                acp::PermissionOption::new(
                    "reject-once",
                    REJECT_ONCE_LABEL.to_owned(),
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ),
        ]);

        Self {
            session_id,
            gateway,
            client_type,
            edit_options,
            bash_options,
            generic_bash_options,
            fallback_options,
            agent_message_options,
            // Defaults to noop: the live shell path's own `EventTracker` already emits Permission* events, so the prompter must not double-emit
            // A workspace-server-side caller that owns the per-session `events.jsonl` opts in via [`with_event_writer`]
            event_writer: EventWriter::noop(),
            hub_permission: None,
            // Fail-safe construction default, deliberately not the product default (which is on)
            // A caller that forgets to wire the gate via `with_remember_tool_approvals` gets no remember rows rather than un-resolved ones
            remember_tool_approvals: false,
        }
    }

    /// Set whether the granular per-tool "Always allow …" options are shown.
    /// See [`AcpPrompter::remember_tool_approvals`].
    pub fn with_remember_tool_approvals(mut self, enabled: bool) -> Self {
        self.remember_tool_approvals = enabled;
        self
    }

    /// Route the permission prompt to chat over the server when `Some`; `None` keeps the local prompt.
    pub fn with_hub_permission(
        mut self,
        hub_permission: Option<Arc<dyn crate::permission::PermissionHookTransport>>,
    ) -> Self {
        self.hub_permission = hub_permission;
        self
    }

    /// Attach a per-session `events.jsonl` writer so [`request`](Self::request) records `PermissionRequested` / `PermissionResolved`.
    /// Used by the workspace-server permission path, which owns the session log.
    /// The shell path keeps the default noop to avoid double-emitting alongside its own `EventTracker`.
    pub fn with_event_writer(mut self, event_writer: EventWriter) -> Self {
        self.event_writer = event_writer;
        self
    }

    fn build_options(
        &self,
        access: &AccessKind,
    ) -> IndexMap<acp::PermissionOptionId, acp::PermissionOption> {
        let mut base = self.build_options_inner(access);
        // Gate off: strip the granular always-allow rows (order-preserving).
        if !self.remember_tool_approvals {
            for id in REMEMBER_TOOL_APPROVALS_GATED_IDS {
                base.shift_remove(&acp::PermissionOptionId::new(*id));
            }
        }
        // Prepend the "enable always-approve mode" option at position 0 for client types that wire the id through to their YOLO toggle
        // See `ENABLE_ALWAYS_APPROVE_OPTION_ID` for the full client/shell split
        prepend_enable_always_approve(self.client_type, base)
    }

    /// Bash meta driving the pager's ←/→ scope selection for the `allow-always-command` / `reject-always-command` rows.
    /// `Some` only for fancy-UI clients with the gate on; otherwise those rows are absent and the meta is a dangling scope hint.
    fn bash_selection_meta(&self, access: &AccessKind) -> Option<acp::Meta> {
        match access {
            AccessKind::Bash(bash_command)
                if self.remember_tool_approvals
                    && matches!(
                        self.client_type,
                        ClientType::GrokTUI | ClientType::GrokPager | ClientType::Desktop
                    ) =>
            {
                serde_json::to_value(primary_command_from_script(bash_command))
                    .ok()
                    .and_then(|v| v.as_object().cloned())
            }
            _ => None,
        }
    }

    /// Request `_meta`: the bash selection scope or the protected-edit description (never both).
    /// The hook ask is merged in when one forced this prompt, so a client that writes its own title still sees it.
    fn permission_request_meta(
        &self,
        access: &AccessKind,
        protected_edit: Option<crate::permission::ProtectedEditReason>,
        hook_ask: Option<&HookAsk>,
    ) -> Option<acp::Meta> {
        let mut meta = self
            .bash_selection_meta(access)
            .or_else(|| protected_edit.and_then(protected_edit_meta))
            .unwrap_or_default();
        if let Some(value) = hook_ask.and_then(|ask| serde_json::to_value(ask).ok()) {
            meta.insert(HOOK_ASK_META_KEY.to_owned(), value);
        }
        (!meta.is_empty()).then_some(meta)
    }

    /// Build the per-access-kind option map WITHOUT the "enable always-approve mode" prepend.
    /// A separate inner fn lets `build_options` wrap the result with one prepend call rather than threading it through every match arm.
    fn build_options_inner(
        &self,
        access: &AccessKind,
    ) -> IndexMap<acp::PermissionOptionId, acp::PermissionOption> {
        match access {
            AccessKind::Edit(_) => self.edit_options.clone(),
            AccessKind::AgentMessage { .. } | AccessKind::Tool(_) => {
                self.agent_message_options.clone()
            }
            AccessKind::Bash(bash_command) => {
                // For GrokTUI clients, use the fancy interactive options with term selection
                // For generic clients (web, etc.), use simpler options that work without special UI handling
                match self.client_type {
                    ClientType::GrokTUI | ClientType::GrokPager | ClientType::Desktop => {
                        let mut bash_commands: IndexMap<
                            acp::PermissionOptionId,
                            acp::PermissionOption,
                        > = IndexMap::new();
                        // Always-allow leads; persistent deny trails so it never sits between safe options
                        // Offer allow only when accepting it can stop this script from prompting again; deny stays because deny prefixes bind unconditionally
                        let primary_command = primary_command_from_script(bash_command);
                        if let Some(primary_command) = &primary_command
                            && crate::permission::grants::always_allow_row_is_effective(
                                bash_command,
                            )
                        {
                            let (id, option) = bash_scope_option(
                                "allow-always-command",
                                "Always allow:",
                                acp::PermissionOptionKind::AllowAlways,
                                primary_command,
                            );
                            bash_commands.insert(id, option);
                        }
                        // Then the standard allow/reject options
                        bash_commands.extend(self.bash_options.clone());
                        // Trailing persistent deny; ordering rationale above.
                        if let Some(primary_command) = &primary_command {
                            let (id, option) = bash_scope_option(
                                "reject-always-command",
                                "Never allow:",
                                acp::PermissionOptionKind::RejectAlways,
                                primary_command,
                            );
                            bash_commands.insert(id, option);
                        }
                        bash_commands
                    }
                    ClientType::Generic
                    | ClientType::GrokWeb
                    | ClientType::Nebula
                    | ClientType::Extension => {
                        // For generic clients, use simpler options that display well
                        // The command is shown via tool_call_update, so options don't need it inline
                        self.generic_bash_options.clone()
                    }
                }
            }
            AccessKind::WebFetch(url) => {
                // Unreachable in practice: the manager rejects unparseable URLs before prompting
                // The fallback exists only as defensive code
                let domain = domain_from_url(url).unwrap_or_else(|| "unknown domain".to_string());

                let mut options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
                    IndexMap::new();
                options.insert(
                    acp::PermissionOptionId::new("allow-always-domain"),
                    acp::PermissionOption::new(
                        "allow-always-domain",
                        // The grant persists per project across sessions; the label must not promise narrower session scope
                        format!("Yes, always allow {domain} for this project"),
                        acp::PermissionOptionKind::AllowAlways,
                    ),
                );
                options.insert(
                    acp::PermissionOptionId::new("allow-once"),
                    acp::PermissionOption::new(
                        "allow-once",
                        "Yes, allow once".to_owned(),
                        acp::PermissionOptionKind::AllowOnce,
                    ),
                );
                options.insert(
                    acp::PermissionOptionId::new("reject-once"),
                    acp::PermissionOption::new(
                        "reject-once",
                        REJECT_ONCE_LABEL.to_owned(),
                        acp::PermissionOptionKind::RejectOnce,
                    ),
                );
                // Trailing persistent deny; always the exact prompted host (deny scope is deliberately narrow, no wildcard editor)
                // Uses the deny key, which unlike `domain` keeps the `www.` prefix, so the label names exactly what gets persisted
                let deny_domain =
                    web_fetch_deny_key_from_url(url).unwrap_or_else(|| domain.clone());
                options.insert(
                    acp::PermissionOptionId::new("reject-always-domain"),
                    acp::PermissionOption::new(
                        "reject-always-domain",
                        format!("No, never allow {deny_domain} for this project"),
                        acp::PermissionOptionKind::RejectAlways,
                    ),
                );
                options
            }
            AccessKind::MCPTool {
                name: tool_name, ..
            } => {
                // Toggle-aware clients (pager, TUI, Desktop) get the `allow-always-mcp` option carrying `McpToolPermission` meta
                // The pager renders the scope toggle; TUI/Desktop submit no `McpScopeSelection` meta, so the response mapper defaults to tool-scope
                // Fallback clients use the legacy `fallback_options` (`always-allow`) and the manager's plain `AllowAlways` arm persists tool-scope
                match self.client_type {
                    ClientType::GrokTUI | ClientType::GrokPager | ClientType::Desktop => {
                        let mut options: IndexMap<acp::PermissionOptionId, acp::PermissionOption> =
                            IndexMap::new();
                        let server_prefix = parse_mcp_qualified_name(tool_name)
                            .map(|(_, server, _)| server.to_owned());
                        options.insert(
                            acp::PermissionOptionId::new("allow-always-mcp"),
                            acp::PermissionOption::new(
                                "allow-always-mcp",
                                format!("Always allow: {}", tool_name),
                                acp::PermissionOptionKind::AllowAlways,
                            )
                            .meta(
                                serde_json::to_value(McpToolPermission {
                                    prompt_prefix: "Always allow:".to_owned(),
                                    tool_name: tool_name.clone(),
                                    server_prefix: server_prefix.clone(),
                                })
                                .ok()
                                .and_then(|v| v.as_object().cloned()),
                            ),
                        );
                        options.insert(
                            acp::PermissionOptionId::new("allow-once"),
                            acp::PermissionOption::new(
                                "allow-once",
                                "Yes".to_owned(),
                                acp::PermissionOptionKind::AllowOnce,
                            ),
                        );
                        options.insert(
                            acp::PermissionOptionId::new("reject-once"),
                            acp::PermissionOption::new(
                                "reject-once",
                                REJECT_ONCE_LABEL.to_owned(),
                                acp::PermissionOptionKind::RejectOnce,
                            ),
                        );
                        // Persistent deny: always the exact qualified tool (no server-scope reject, so no scope-toggle meta)
                        options.insert(
                            acp::PermissionOptionId::new("reject-always-mcp"),
                            acp::PermissionOption::new(
                                "reject-always-mcp",
                                format!(
                                    "Never allow: {}",
                                    mcp_tool_display_name(tool_name, server_prefix.as_deref())
                                ),
                                acp::PermissionOptionKind::RejectAlways,
                            ),
                        );
                        options
                    }
                    ClientType::Generic
                    | ClientType::GrokWeb
                    | ClientType::Nebula
                    | ClientType::Extension => self.fallback_options.clone(),
                }
            }
            _ => self.fallback_options.clone(),
        }
    }

    /// Prompt the user.
    /// `hook_ask` is set when a `PreToolUse` hook forced this prompt; it is prepended to the request title so the user sees which hook asked and why.
    pub async fn request(
        &self,
        access: &AccessKind,
        tool_call_update: &acp::ToolCallUpdate,
        protected_edit: Option<crate::permission::ProtectedEditReason>,
        hook_ask: Option<&HookAsk>,
    ) -> PromptOutcome {
        let tool_name = tool_name_for_access(access);
        // events.jsonl: `PermissionRequested` at prompt-start
        // The `Instant` captured here makes the paired `PermissionResolved.wait_ms` measure the user-facing prompt, not earlier manager bookkeeping
        self.event_writer.emit(Event::PermissionRequested {
            tool_name: tool_name.clone(),
        });
        let prompt_start = Instant::now();
        let mut resolved_guard = ResolvedOnDrop {
            event_writer: &self.event_writer,
            tool_name: Some(tool_name.clone()),
            prompt_start,
        };

        let outcome = match &self.hub_permission {
            // Route the prompt to chat over the server (see `ToolServerPermissionTransport` for the await/release contract)
            Some(transport) => {
                crate::permission::hub_permission::request_permission_via_hub(
                    transport.as_ref(),
                    access,
                    tool_call_update.tool_call_id.0.as_ref(),
                    hook_ask,
                    // The manager records "always" answers in its own store.
                    distill_tool_runtime::ToolApprovalPolicy::GrantsAllowed,
                )
                .await
            }
            None => {
                let permission_options = self.build_options(access);
                let req = acp::RequestPermissionRequest::new(
                    self.session_id.clone(),
                    with_hook_ask_header(tool_call_update, hook_ask, &tool_name),
                    permission_options.values().cloned().collect(),
                )
                .meta(self.permission_request_meta(
                    access,
                    protected_edit,
                    hook_ask,
                ));
                match self.gateway.request_permission(req).await {
                    Ok(resp) => match resp.outcome {
                        acp::RequestPermissionOutcome::Cancelled => PromptOutcome::Cancelled,
                        acp::RequestPermissionOutcome::Selected(selected) => map_selected_outcome(
                            &permission_options,
                            &selected.option_id,
                            resp.meta.as_ref(),
                            access,
                        ),
                        // TODO(acp-0.10): `RequestPermissionOutcome` is #[non_exhaustive].
                        _ => PromptOutcome::Error("unknown permission outcome".to_owned()),
                    },
                    Err(e) => {
                        tracing::error!(?e, "failed to request permission");
                        PromptOutcome::Error("failed to request permission".to_owned())
                    }
                }
            }
        };

        // events.jsonl: `PermissionResolved` at decision-time, with the user-facing wait derived from the prompt-start `Instant` above
        let tool_name = resolved_guard
            .tool_name
            .take()
            .expect("guard is armed until normal completion");
        self.event_writer.emit(Event::PermissionResolved {
            tool_name,
            decision: permission_decision_for_outcome(&outcome),
            wait_ms: prompt_start.elapsed().as_millis() as u64,
        });

        outcome
    }
}

struct ResolvedOnDrop<'a> {
    event_writer: &'a EventWriter,
    tool_name: Option<String>,
    prompt_start: Instant,
}

impl Drop for ResolvedOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(tool_name) = self.tool_name.take() {
            self.event_writer.emit(Event::PermissionResolved {
                tool_name,
                decision: PermissionDecision::Cancelled,
                wait_ms: self.prompt_start.elapsed().as_millis() as u64,
            });
        }
    }
}

fn protected_edit_meta(reason: crate::permission::ProtectedEditReason) -> Option<acp::Meta> {
    let payload = crate::permission::ProtectedEditPermission::from_reason(reason);
    serde_json::to_value(payload)
        .expect("ProtectedEditPermission serializes infallibly")
        .as_object()
        .cloned()
}

/// The prompt's tool call with the hook ask prepended to its title.
/// `tool_name` is the fallback title when the tool call has none.
fn with_hook_ask_header(
    tool_call_update: &acp::ToolCallUpdate,
    hook_ask: Option<&HookAsk>,
    tool_name: &str,
) -> acp::ToolCallUpdate {
    let mut update = tool_call_update.clone();
    if let Some(ask) = hook_ask {
        let action = update
            .fields
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or(tool_name);
        update.fields.title = Some(ask.prompt_header(action));
    }
    update
}

/// Tool name used for `events.jsonl` Permission* events AND for the `PermissionEvent.tool_name` telemetry field.
/// Single source of truth: the permission manager also calls this when deriving `(tool_name, access_kind, access_detail)`, so the two cannot drift.
pub fn tool_name_for_access(access: &AccessKind) -> String {
    match access {
        AccessKind::Read(_) => "read_file".to_owned(),
        AccessKind::Grep { .. } => "grep".to_owned(),
        AccessKind::Edit(_) => "search_replace".to_owned(),
        AccessKind::Bash(_) => "run_terminal_command".to_owned(),
        AccessKind::MCPTool { name, .. } => format!("mcp:{name}"),
        AccessKind::WebFetch(_) => "web_fetch".to_owned(),
        AccessKind::WebSearch(_) => "web_search".to_owned(),
        AccessKind::AgentMessage { .. } => {
            distill_tools::implementations::distill::SEND_SUBAGENT_MESSAGE_TOOL_NAME.to_owned()
        }
        AccessKind::Tool(name) => name.clone(),
    }
}

/// Map a [`PromptOutcome`] to the `events.jsonl` [`PermissionDecision`].
/// One `match` so the allow/deny/cancel/followup mapping cannot drift across call sites.
fn permission_decision_for_outcome(outcome: &PromptOutcome) -> PermissionDecision {
    match outcome {
        PromptOutcome::AllowOnce
        | PromptOutcome::AllowAlways
        | PromptOutcome::AllowEditsForSession
        | PromptOutcome::AllowAlwaysBashCommand(_)
        | PromptOutcome::AllowAlwaysBashGlob(_)
        | PromptOutcome::AllowAlwaysDomain(_)
        | PromptOutcome::AllowAlwaysMcpTool(_)
        | PromptOutcome::AllowAlwaysMcpServer(_) => PermissionDecision::Allow,
        PromptOutcome::RejectOnce
        | PromptOutcome::RejectAlwaysBashCommand(_)
        | PromptOutcome::RejectAlwaysMcpTool(_)
        | PromptOutcome::RejectAlwaysDomain(_)
        | PromptOutcome::Error(_) => PermissionDecision::Deny,
        PromptOutcome::Cancelled => PermissionDecision::Cancelled,
        PromptOutcome::FollowupMessage(_) => PermissionDecision::Followup,
    }
}

fn map_selected_outcome(
    permission_options: &IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
    option_id: &acp::PermissionOptionId,
    meta: Option<&acp::Meta>,
    access: &AccessKind,
) -> PromptOutcome {
    permission_options
        .get(option_id)
        .map(|option| match option.kind {
            acp::PermissionOptionKind::AllowOnce => PromptOutcome::AllowOnce,
            acp::PermissionOptionKind::AllowAlways => {
                // Defensive guard: the "enable always-approve mode" option is built with kind `AllowOnce`
                // This branch is dead code in current builds, kept for older or third-party clients that might echo the id back under `AllowAlways`
                // The shell still treats it as a single allow, never a per-tool whitelist; the session-wide YOLO flip is the client's job
                if option_id.0.as_ref() == ENABLE_ALWAYS_APPROVE_OPTION_ID {
                    PromptOutcome::AllowOnce
                } else if option_id.to_string() == "allow-always-mcp" {
                    if let Some(selection) = meta.and_then(|m| {
                        serde_json::from_value::<McpScopeSelection>(serde_json::Value::Object(
                            m.clone(),
                        ))
                        .ok()
                    }) {
                        match selection {
                            McpScopeSelection::Tool { tool_name } => {
                                PromptOutcome::AllowAlwaysMcpTool(tool_name)
                            }
                            McpScopeSelection::Server { server } => {
                                if server.is_empty() {
                                    if let AccessKind::MCPTool { name, .. } = access {
                                        PromptOutcome::AllowAlwaysMcpTool(name.clone())
                                    } else {
                                        PromptOutcome::AllowAlways
                                    }
                                } else {
                                    PromptOutcome::AllowAlwaysMcpServer(server)
                                }
                            }
                        }
                    } else if let AccessKind::MCPTool { name, .. } = access {
                        // No scope meta. TUI / Desktop case: the renderer shows the option but does not build the toggle response.
                        // Default to tool-scope using the access-kind name
                        PromptOutcome::AllowAlwaysMcpTool(name.clone())
                    } else {
                        PromptOutcome::AllowAlways
                    }
                } else if option_id.to_string() == "allow-always-domain" {
                    if let AccessKind::WebFetch(url) = access
                        && let Some(domain) = domain_from_url(url)
                        && !domain.is_empty()
                    {
                        PromptOutcome::AllowAlwaysDomain(domain)
                    } else {
                        // Defensive: unreachable if manager rejects unparseable URLs.
                        // Don't persist an empty domain; allow this single call only
                        PromptOutcome::AllowOnce
                    }
                } else if option_id.to_string() == "allow-always-command" {
                    if let Some(bash_selected_commands) = meta.and_then(|m| {
                        serde_json::from_value::<BashCommandSelectedTerms>(
                            serde_json::Value::Object(m.clone()),
                        )
                        .ok()
                    }) {
                        let pattern = bash_selected_commands.command_parts.join(" ");
                        if bash_selected_commands.is_glob {
                            PromptOutcome::AllowAlwaysBashGlob(pattern)
                        } else {
                            PromptOutcome::AllowAlwaysBashCommand(pattern)
                        }
                    } else if let AccessKind::Bash(cmd) = access {
                        // No interactive selection meta (e.g. desktop client); fall back to the primary command from the script.
                        if let Some(primary) = primary_command_from_script(cmd) {
                            PromptOutcome::AllowAlwaysBashCommand(
                                primary.highlighted_words.join(" "),
                            )
                        } else {
                            PromptOutcome::AllowAlways
                        }
                    } else {
                        PromptOutcome::AllowAlways
                    }
                } else if option_id.0.as_ref() == ALLOW_EDITS_SESSION_OPTION_ID
                    && matches!(access, AccessKind::Edit(_))
                {
                    // The edit prompt's "Yes, allow all edits during this session".
                    // Treat as session-scoped only (in-memory). Do not persist.
                    PromptOutcome::AllowEditsForSession
                } else {
                    PromptOutcome::AllowAlways
                }
            }
            acp::PermissionOptionKind::RejectOnce => {
                if let Some(followup) = meta
                    .and_then(|m| m.get("followup_message"))
                    .and_then(|v| v.as_str())
                    && !followup.trim().is_empty()
                {
                    return PromptOutcome::FollowupMessage(followup.to_string());
                }
                PromptOutcome::RejectOnce
            }
            acp::PermissionOptionKind::RejectAlways => {
                // `reject-always` is the generic clients' persistent-deny row
                // It carries no selection meta, so it falls through to the primary-command deny
                let id = option_id.to_string();
                if id == "reject-always-command" || id == "reject-always" {
                    if let Some(bash_selected_commands) = meta.and_then(|m| {
                        serde_json::from_value::<BashCommandSelectedTerms>(
                            serde_json::Value::Object(m.clone()),
                        )
                        .ok()
                    }) {
                        PromptOutcome::RejectAlwaysBashCommand(
                            bash_selected_commands.command_parts.join(" "),
                        )
                    } else if let AccessKind::Bash(cmd) = access {
                        // Unparseable scripts persist the raw text: segment matching never runs for them, but the raw-deny check does
                        // So "don't ask again" still sticks instead of silently degrading to a one-shot reject
                        match primary_command_from_script(cmd) {
                            Some(primary) => PromptOutcome::RejectAlwaysBashCommand(
                                primary.highlighted_words.join(" "),
                            ),
                            None => PromptOutcome::RejectAlwaysBashCommand(cmd.clone()),
                        }
                    } else {
                        PromptOutcome::RejectOnce
                    }
                } else if id == "reject-always-mcp" {
                    // Deny scope comes from the AccessKind, never client meta (same anti-spoof rule as the allow rows)
                    // It is always the exact qualified tool
                    if let AccessKind::MCPTool { name, .. } = access {
                        PromptOutcome::RejectAlwaysMcpTool(name.clone())
                    } else {
                        PromptOutcome::RejectOnce
                    }
                } else if id == "reject-always-domain" {
                    if let AccessKind::WebFetch(url) = access
                        && let Some(domain) = web_fetch_deny_key_from_url(url)
                    {
                        PromptOutcome::RejectAlwaysDomain(domain)
                    } else {
                        // Defensive: unreachable if manager rejects unparseable URLs.
                        // Don't persist an empty domain
                        PromptOutcome::RejectOnce
                    }
                } else {
                    PromptOutcome::RejectOnce
                }
            }
            // TODO(acp-0.10): `PermissionOptionKind` is #[non_exhaustive].
            _ => PromptOutcome::Error("unknown permission option kind".to_owned()),
        })
        .unwrap_or_else(|| PromptOutcome::Error("unknown permission option".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn hook_ask_reaches_both_the_title_and_the_request_meta() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let prompter = AcpPrompter::new(
            acp::SessionId::new(Arc::from("sess-ask")),
            GatewaySender::new(tx),
            ClientType::Generic,
        );
        let ask = HookAsk {
            hook_name: "guard".to_owned(),
            reason: Some("confirm this".to_owned()),
        };
        let access = AccessKind::Edit("a.rs".to_owned());

        let meta = prompter
            .permission_request_meta(&access, /*protected_edit=*/ None, Some(&ask))
            .expect("a hook ask always produces meta");
        assert_eq!(
            serde_json::Value::Object(meta),
            serde_json::json!({
                "hookAsk": { "hookName": "guard", "reason": "confirm this" },
            })
        );
        let merged = prompter
            .permission_request_meta(
                &access,
                Some(crate::permission::ProtectedEditReason::Sensitive),
                Some(&ask),
            )
            .expect("an ask plus a protected edit always produces meta");
        assert_eq!(
            serde_json::Value::Object(merged),
            serde_json::json!({
                "kind": "sensitive",
                "hookAsk": { "hookName": "guard", "reason": "confirm this" },
            }),
            "an ask and a protected edit must both survive the merge"
        );
        assert!(
            prompter
                .permission_request_meta(
                    &access, /*protected_edit=*/ None, /*hook_ask=*/ None
                )
                .is_none(),
            "no ask and no protected edit leaves the meta absent"
        );

        let untitled = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-ask")),
            acp::ToolCallUpdateFields::default(),
        );
        let titled = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-ask")),
            acp::ToolCallUpdateFields::new().title(Some("Edit a.rs".to_owned())),
        );
        assert_eq!(
            with_hook_ask_header(&untitled, Some(&ask), "search_replace")
                .fields
                .title,
            Some("search_replace — hook 'guard' asks: confirm this".to_owned())
        );
        assert_eq!(
            with_hook_ask_header(&titled, Some(&ask), "search_replace")
                .fields
                .title,
            Some("Edit a.rs — hook 'guard' asks: confirm this".to_owned())
        );
        assert_eq!(
            with_hook_ask_header(&titled, None, "search_replace")
                .fields
                .title,
            titled.fields.title,
            "no ask leaves the title untouched"
        );
    }

    /// Wire-compatibility pin: `PromptOutcome::kind()` maps each variant to the [`PromptOutcomeKind`] whose `wire_str` is the stable event string.
    /// If a variant's kind or a kind's wire ever changes, this fails (guarding event-JSON compatibility).
    /// Completeness of `kind()` is enforced by the compiler; completeness of `PromptOutcomeKind::{ALL, wire_str}` by `wire_enum!`.
    #[test]
    fn prompt_outcome_kind_pins_wire_strings() {
        let cases = [
            (PromptOutcome::AllowOnce, "allow_once"),
            (PromptOutcome::AllowAlways, "allow_always"),
            (
                PromptOutcome::AllowEditsForSession,
                "allow_edits_for_session",
            ),
            (
                PromptOutcome::AllowAlwaysBashCommand(String::new()),
                "allow_always_bash",
            ),
            (
                PromptOutcome::AllowAlwaysBashGlob(String::new()),
                "allow_always_bash_glob",
            ),
            (
                PromptOutcome::AllowAlwaysDomain(String::new()),
                "allow_always_domain",
            ),
            (
                PromptOutcome::AllowAlwaysMcpTool(String::new()),
                "allow_always_mcp_tool",
            ),
            (
                PromptOutcome::AllowAlwaysMcpServer(String::new()),
                "allow_always_mcp_server",
            ),
            (PromptOutcome::RejectOnce, "reject_once"),
            (
                PromptOutcome::RejectAlwaysBashCommand(String::new()),
                "reject_always_bash",
            ),
            (
                PromptOutcome::RejectAlwaysMcpTool(String::new()),
                "reject_always_mcp_tool",
            ),
            (
                PromptOutcome::RejectAlwaysDomain(String::new()),
                "reject_always_domain",
            ),
            (PromptOutcome::Cancelled, "cancelled"),
            (PromptOutcome::FollowupMessage(String::new()), "followup"),
            (PromptOutcome::Error(String::new()), "error"),
        ];
        for (outcome, wire) in &cases {
            assert_eq!(outcome.kind().wire_str(), *wire);
        }
        // Every owner kind is exercised by exactly one case (no kind unmapped).
        assert_eq!(cases.len(), PromptOutcomeKind::ALL.len());
    }

    fn prompter(client_type: ClientType) -> AcpPrompter {
        // Existing tests assert the always-allow options are present; off-state has dedicated `gate_*` tests
        prompter_with_gate(client_type, true)
    }

    fn prompter_with_gate(client_type: ClientType, remember: bool) -> AcpPrompter {
        let (tx, _rx) = mpsc::unbounded_channel();
        let gateway = GatewaySender::new(tx);
        AcpPrompter::new(
            acp::SessionId::new(Arc::from("test-session")),
            gateway,
            client_type,
        )
        .with_remember_tool_approvals(remember)
    }

    fn has_option(
        opts: &IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
        id: &str,
    ) -> bool {
        opts.contains_key(&acp::PermissionOptionId::new(id))
    }

    #[test]
    fn gate_off_strips_bash_always_allow_keeps_yes_no() {
        let p = prompter_with_gate(ClientType::GrokPager, false);
        let access = AccessKind::Bash("kubectl get pods".to_owned());
        let opts = p.build_options(&access);
        assert!(
            !has_option(&opts, "allow-always-command"),
            "gate off must strip allow-always-command"
        );
        assert!(
            !has_option(&opts, "reject-always-command"),
            "gate off must strip reject-always-command"
        );
        assert!(has_option(&opts, "allow-once"), "Yes must remain");
        assert!(has_option(&opts, "reject-once"), "No must remain");
        assert!(
            has_option(&opts, ENABLE_ALWAYS_APPROVE_OPTION_ID),
            "global always-approve must remain"
        );
    }

    #[test]
    fn gate_on_includes_bash_always_allow() {
        let p = prompter_with_gate(ClientType::GrokPager, true);
        let access = AccessKind::Bash("kubectl get pods".to_owned());
        let opts = p.build_options(&access);
        assert!(
            has_option(&opts, "allow-always-command"),
            "gate on must include allow-always-command"
        );
        assert!(
            has_option(&opts, "reject-always-command"),
            "gate on must include reject-always-command"
        );
    }

    #[test]
    fn gate_off_strips_mcp_always_allow() {
        let p = prompter_with_gate(ClientType::GrokPager, false);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        assert!(!has_option(&opts, "allow-always-mcp"));
        assert!(!has_option(&opts, "reject-always-mcp"));
        assert!(has_option(&opts, "allow-once"));
        assert!(has_option(&opts, "reject-once"));
    }

    #[test]
    fn gate_on_includes_mcp_never_allow() {
        let p = prompter_with_gate(ClientType::GrokPager, true);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        assert!(has_option(&opts, "allow-always-mcp"));
        assert!(has_option(&opts, "reject-always-mcp"));
        let reject = opts
            .get(&acp::PermissionOptionId::new("reject-always-mcp"))
            .unwrap();
        assert_eq!(reject.kind, acp::PermissionOptionKind::RejectAlways);
        assert!(
            reject.meta.is_none(),
            "reject row must not carry scope-toggle meta (always exact tool)"
        );
    }

    #[test]
    fn gate_off_strips_generic_bash_always_and_reject_always() {
        let p = prompter_with_gate(ClientType::GrokWeb, false);
        let access = AccessKind::Bash("kubectl get pods".to_owned());
        let opts = p.build_options(&access);
        assert!(!has_option(&opts, "always-allow"));
        assert!(!has_option(&opts, "reject-always"));
        assert!(has_option(&opts, "allow-once"));
        assert!(has_option(&opts, "reject-once"));
    }

    #[test]
    fn gate_off_strips_web_fetch_always_allow_domain() {
        let p = prompter_with_gate(ClientType::GrokPager, false);
        let access = AccessKind::WebFetch("https://example.com/x".to_owned());
        let opts = p.build_options(&access);
        assert!(!has_option(&opts, "allow-always-domain"));
        assert!(!has_option(&opts, "reject-always-domain"));
        assert!(has_option(&opts, "allow-once"));
        assert!(has_option(&opts, "reject-once"));
    }

    #[test]
    fn gate_on_includes_web_fetch_never_allow_domain() {
        let p = prompter_with_gate(ClientType::GrokPager, true);
        let access = AccessKind::WebFetch("https://example.com/x".to_owned());
        let opts = p.build_options(&access);
        assert!(has_option(&opts, "allow-always-domain"));
        assert!(has_option(&opts, "reject-always-domain"));
        let reject = opts
            .get(&acp::PermissionOptionId::new("reject-always-domain"))
            .unwrap();
        assert_eq!(reject.kind, acp::PermissionOptionKind::RejectAlways);
    }

    #[test]
    fn bash_meta_present_only_when_gate_on_for_fancy_clients() {
        let access = AccessKind::Bash("kubectl get pods".to_owned());
        // Gate on with a fancy client: meta carries the parsed command parts
        let on = prompter_with_gate(ClientType::GrokPager, true);
        let meta = on.bash_selection_meta(&access).expect("meta present");
        assert!(
            serde_json::from_value::<
                crate::permission::bash_command_splitting::BashCommandHighlights,
            >(serde_json::Value::Object(meta))
            .is_ok(),
            "meta must deserialize back into BashCommandHighlights"
        );
        // Gate off: no meta (no allow-always-command row to scope)
        let off = prompter_with_gate(ClientType::GrokPager, false);
        assert!(off.bash_selection_meta(&access).is_none());
        // Generic client never gets the fancy-UI meta, even with the gate on.
        let generic = prompter_with_gate(ClientType::GrokWeb, true);
        assert!(generic.bash_selection_meta(&access).is_none());
        // Non-bash access never carries bash meta.
        assert!(
            on.bash_selection_meta(&AccessKind::Edit("a.rs".to_owned()))
                .is_none()
        );
    }

    #[test]
    fn bash_reject_always_command_maps_selected_words() {
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::Bash("cargo test --workspace".to_owned());
        let opts = p.build_options(&access);
        // Pager path: the ←/→ word-scope selection arrives as BashCommandSelectedTerms meta and wins over the raw script
        let meta = serde_json::to_value(BashCommandSelectedTerms {
            command_parts: vec!["cargo".to_owned(), "test".to_owned()],
            is_glob: false,
        })
        .unwrap()
        .as_object()
        .cloned()
        .unwrap();
        let outcome = outcome_for(&opts, "reject-always-command", Some(meta), &access);
        assert!(
            matches!(
                outcome,
                PromptOutcome::RejectAlwaysBashCommand(ref w) if w == "cargo test"
            ),
            "selected words must map to RejectAlwaysBashCommand, got {outcome:?}"
        );
        // No selection meta: fall back to the primary command from the script.
        let outcome = outcome_for(&opts, "reject-always-command", None, &access);
        assert!(
            matches!(
                outcome,
                PromptOutcome::RejectAlwaysBashCommand(ref w) if w == "cargo test --workspace"
            ),
            "no meta must fall back to the primary command, got {outcome:?}"
        );
    }

    /// The generic clients' `reject-always` row is labeled "don't ask again".
    /// It must persist a deny for the primary command, not degrade to a silent one-shot reject.
    #[test]
    fn generic_reject_always_persists_primary_command_deny() {
        let p = prompter(ClientType::Generic);
        let access = AccessKind::Bash("cargo test --workspace".to_owned());
        let opts = p.build_options(&access);
        let outcome = outcome_for(&opts, "reject-always", None, &access);
        assert!(
            matches!(
                outcome,
                PromptOutcome::RejectAlwaysBashCommand(ref w) if w == "cargo test --workspace"
            ),
            "generic reject-always must map to a persistent command deny, got {outcome:?}"
        );

        // Unparseable scripts deny by raw text, never a one-shot reject
        const OPAQUE: &str = "deploy $(git rev-parse HEAD)";
        let access = AccessKind::Bash(OPAQUE.to_owned());
        let opts = p.build_options(&access);
        let outcome = outcome_for(&opts, "reject-always", None, &access);
        assert!(
            matches!(
                outcome,
                PromptOutcome::RejectAlwaysBashCommand(ref w) if w == OPAQUE
            ),
            "unparseable reject-always must persist the raw script, got {outcome:?}"
        );
    }

    /// A wrapped/chained dangerous command can never produce the exact grant it requires, so the allow row is suppressed.
    /// Offering it would save a rule that keeps prompting.
    /// The deny row stays (denies bind by prefix).
    #[test]
    fn unhonorable_always_allow_row_is_suppressed() {
        let p = prompter(ClientType::GrokPager);
        for script in [
            "env FOO=1 git push origin main",
            "git status && git push origin main",
            // No findings at all, but the primary-scoped grant cannot stop the second segment from prompting (the loop in the reports)
            "ls && npm publish",
            "ls /tmp && ./bazelw test //hw-tests/integration/...",
        ] {
            let opts = p.build_options(&AccessKind::Bash(script.to_owned()));
            assert!(
                !has_option(&opts, "allow-always-command"),
                "must not offer an allow row it cannot honor: {script:?}"
            );
            assert!(
                has_option(&opts, "reject-always-command"),
                "persistent deny stays available: {script:?}"
            );
        }
        // Plain dangerous command: full-scope grant matches, row offered.
        let opts = p.build_options(&AccessKind::Bash("git push origin main".to_owned()));
        assert!(has_option(&opts, "allow-always-command"));
    }

    #[test]
    fn parseable_scripts_offer_scoped_rows_and_meta() {
        let p = prompter(ClientType::GrokPager);
        for script in [
            "cargo test --workspace",
            "cd /tmp && cargo test --workspace",
            "# build first\n# then test\ncargo test --workspace",
            "ps aux | grep process",
        ] {
            let access = AccessKind::Bash(script.to_owned());
            let opts = p.build_options(&access);
            assert!(
                has_option(&opts, "allow-always-command"),
                "parseable primary must offer the scoped allow row: {script:?}"
            );
            assert!(
                has_option(&opts, "reject-always-command"),
                "parseable primary must offer the scoped deny row: {script:?}"
            );
            assert!(
                p.bash_selection_meta(&access).is_some(),
                "parseable primary must carry selection meta: {script:?}"
            );
        }
    }

    #[test]
    fn unparseable_and_setup_only_scripts_have_no_scoped_rows() {
        let p = prompter(ClientType::GrokPager);
        for script in [
            "cd /tmp && sleep 1",
            "if true; then ls; fi",
            "if true; then",
        ] {
            let access = AccessKind::Bash(script.to_owned());
            let opts = p.build_options(&access);
            assert!(
                !has_option(&opts, "allow-always-command"),
                "no primary command to scope: {script:?}"
            );
            assert!(
                !has_option(&opts, "reject-always-command"),
                "no primary command to scope: {script:?}"
            );
            assert!(
                p.bash_selection_meta(&access).is_none(),
                "no primary must not leave dangling selection meta: {script:?}"
            );
            assert!(has_option(&opts, "allow-once"), "{script:?}");
            assert!(has_option(&opts, "reject-once"), "{script:?}");
            assert!(
                has_option(&opts, ENABLE_ALWAYS_APPROVE_OPTION_ID),
                "{script:?}"
            );
        }
    }

    /// A dump script whose dangerous/floored siblings guarantee re-prompting gets no allow row.
    /// Its "Always allow: ls" could never stop this script, which is exactly the "always allow keeps asking" complaint.
    /// The deny row, selection meta, and the one-shot options all remain.
    #[test]
    fn dump_script_with_unhonorable_grant_keeps_only_the_deny_row() {
        let script = "# Probe the outputs dir\n\
                      ls /tmp/hw-test-outputs 2>/dev/null\n\
                      \n\
                      # Reset scratch dir and run the suite\n\
                      rm -rf /tmp/hw-test-outputs && mkdir -p /tmp/hw-test-outputs\n\
                      ./bazelw test //hw-tests/integration/... --test_output=errors 2>&1 | tee /tmp/hw-test-outputs/run.log | tail -n 40";
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::Bash(script.to_owned());
        let opts = p.build_options(&access);
        assert!(!has_option(&opts, "allow-always-command"));
        assert!(has_option(&opts, "reject-always-command"));
        assert!(p.bash_selection_meta(&access).is_some());
        assert!(has_option(&opts, "allow-once"));
        assert!(has_option(&opts, "reject-once"));
        assert!(has_option(&opts, ENABLE_ALWAYS_APPROVE_OPTION_ID));
    }

    #[test]
    fn scoped_rows_appear_and_disappear_together() {
        let p = prompter(ClientType::GrokPager);
        for script in [
            "cargo test --workspace",
            "ps aux | grep process",
            "cd /tmp && sleep 1",
            "if true; then",
        ] {
            let opts = p.build_options(&AccessKind::Bash(script.to_owned()));
            assert_eq!(
                has_option(&opts, "allow-always-command"),
                has_option(&opts, "reject-always-command"),
                "scoped rows must be gated together: {script:?}"
            );
        }
    }

    #[test]
    fn generic_client_multi_command_keeps_broad_options() {
        let p = prompter(ClientType::GrokWeb);
        let opts = p.build_options(&AccessKind::Bash("ls /tmp && cargo test".to_owned()));
        assert!(has_option(&opts, "always-allow"));
        assert!(has_option(&opts, "allow-once"));
        assert!(has_option(&opts, "reject-once"));
        assert!(has_option(&opts, "reject-always"));
    }

    #[test]
    fn agent_message_options_are_one_call_only() {
        let p = prompter_with_gate(ClientType::GrokPager, true);
        let access = AccessKind::AgentMessage {
            subagent_id: "sub-1".to_owned(),
        };
        let opts = p.build_options(&access);
        let ids: Vec<&str> = opts.keys().map(|id| id.0.as_ref()).collect();

        assert!(ids.contains(&ENABLE_ALWAYS_APPROVE_OPTION_ID));
        assert!(ids.contains(&"allow-once"));
        assert!(ids.contains(&"reject-once"));
        assert!(!ids.contains(&ALLOW_EDITS_SESSION_OPTION_ID));
        assert!(
            !ids.iter()
                .any(|id| id.contains("always") && *id != ENABLE_ALWAYS_APPROVE_OPTION_ID)
        );
    }

    #[test]
    fn agent_message_approval_maps_to_one_call() {
        let p = prompter_with_gate(ClientType::GrokPager, true);
        let access = AccessKind::AgentMessage {
            subagent_id: "sub-1".to_owned(),
        };
        let opts = p.build_options(&access);

        assert!(matches!(
            outcome_for(&opts, "allow-once", None, &access),
            PromptOutcome::AllowOnce
        ));
    }

    #[test]
    fn gate_off_keeps_edit_session_allow() {
        // The edit session allow is governed separately, not by this gate.
        let p = prompter_with_gate(ClientType::GrokPager, false);
        let access = AccessKind::Edit("src/main.rs".to_owned());
        let opts = p.build_options(&access);
        assert!(
            has_option(&opts, ALLOW_EDITS_SESSION_OPTION_ID),
            "edit session allow must survive the gate"
        );
    }

    fn outcome_for(
        options: &IndexMap<acp::PermissionOptionId, acp::PermissionOption>,
        option_id: &str,
        meta: Option<acp::Meta>,
        access: &AccessKind,
    ) -> PromptOutcome {
        let id = acp::PermissionOptionId::new(option_id);
        super::map_selected_outcome(options, &id, meta.as_ref(), access)
    }

    #[test]
    fn mcp_reject_always_maps_exact_access_tool() {
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        // No meta (the row carries none) and never server-scoped: the outcome is always the exact qualified tool from the AccessKind.
        let outcome = outcome_for(&opts, "reject-always-mcp", None, &access);
        assert!(
            matches!(
                outcome,
                PromptOutcome::RejectAlwaysMcpTool(ref n) if n == "linear__list"
            ),
            "got {outcome:?}"
        );
    }

    /// The reject outcome carries the deny key: lowercased, `www.` KEPT, and the row label names that same key.
    /// Collapsing `www.X` to `X` would let a `www.com` rejection deny all of `.com` via the subdomain-broad deny matcher.
    #[test]
    fn web_fetch_reject_always_maps_deny_key_not_stripped_domain() {
        let p = prompter(ClientType::GrokPager);
        for (url, expected) in [
            ("https://www.Example.COM/docs", "www.example.com"),
            ("https://Example.COM/docs", "example.com"),
            ("https://www.com/x", "www.com"),
        ] {
            let access = AccessKind::WebFetch(url.to_owned());
            let opts = p.build_options(&access);
            let label = &opts
                .get(&acp::PermissionOptionId::new("reject-always-domain"))
                .expect("reject-always-domain option missing")
                .name;
            assert_eq!(
                label,
                &format!("No, never allow {expected} for this project"),
                "{url}"
            );
            let outcome = outcome_for(&opts, "reject-always-domain", None, &access);
            assert!(
                matches!(
                    outcome,
                    PromptOutcome::RejectAlwaysDomain(ref d) if d == expected
                ),
                "{url}: got {outcome:?}"
            );
        }
    }

    #[test]
    fn mcp_prompt_includes_allow_always_with_meta() {
        let p = prompter(ClientType::GrokTUI);
        for (name, server) in [
            ("linear__list", "linear"),
            ("123__lookup", "123"),
            ("server:scope__tool", "server:scope"),
        ] {
            let access = AccessKind::MCPTool {
                name: name.to_owned(),
                input: serde_json::Value::Null,
            };
            let opts = p.build_options(&access);
            let opt = opts
                .get(&acp::PermissionOptionId::new("allow-always-mcp"))
                .expect("allow-always-mcp option missing");
            let meta = opt.meta.clone().expect("meta missing");
            let perm: McpToolPermission =
                serde_json::from_value(serde_json::Value::Object(meta)).unwrap();
            assert_eq!(perm.tool_name, name);
            assert_eq!(perm.server_prefix.as_deref(), Some(server));
            assert_eq!(perm.prompt_prefix, "Always allow:");
        }
    }

    #[test]
    fn mcp_prompt_malformed_name_hides_server_scope() {
        let p = prompter(ClientType::GrokPager);
        for name in ["standalone", "linear__shadow__exfil", "linear__"] {
            let access = AccessKind::MCPTool {
                name: name.to_owned(),
                input: serde_json::Value::Null,
            };
            let opts = p.build_options(&access);
            let opt = opts
                .get(&acp::PermissionOptionId::new("allow-always-mcp"))
                .unwrap();
            let perm: McpToolPermission =
                serde_json::from_value(serde_json::Value::Object(opt.meta.clone().unwrap()))
                    .unwrap();
            assert_eq!(perm.tool_name, name);
            assert_eq!(perm.server_prefix, None);
        }
    }

    #[test]
    fn mcp_response_tool_scope() {
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        let meta = serde_json::json!({"kind": "tool", "tool_name": "linear__list"})
            .as_object()
            .cloned()
            .unwrap();
        let outcome = outcome_for(&opts, "allow-always-mcp", Some(meta), &access);
        assert!(matches!(
            outcome,
            PromptOutcome::AllowAlwaysMcpTool(ref n) if n == "linear__list"
        ));
    }

    #[test]
    fn mcp_response_server_scope() {
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        let meta = serde_json::json!({"kind": "server", "server": "linear"})
            .as_object()
            .cloned()
            .unwrap();
        let outcome = outcome_for(&opts, "allow-always-mcp", Some(meta), &access);
        assert!(matches!(
            outcome,
            PromptOutcome::AllowAlwaysMcpServer(ref s) if s == "linear"
        ));
    }

    #[test]
    fn mcp_response_empty_server_falls_back_to_tool() {
        let p = prompter(ClientType::GrokPager);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        let meta = serde_json::json!({"kind": "server", "server": ""})
            .as_object()
            .cloned()
            .unwrap();
        let outcome = outcome_for(&opts, "allow-always-mcp", Some(meta), &access);
        assert!(matches!(
            outcome,
            PromptOutcome::AllowAlwaysMcpTool(ref n) if n == "linear__list"
        ));
    }

    #[test]
    fn mcp_response_no_meta_falls_back_to_tool() {
        // TUI / Desktop case: option id is `allow-always-mcp` but the renderer does not build the toggle meta
        // The prompter must default to tool-scope using the access-kind name
        let p = prompter(ClientType::GrokTUI);
        let access = AccessKind::MCPTool {
            name: "notion__fetch".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        let outcome = outcome_for(&opts, "allow-always-mcp", None, &access);
        assert!(matches!(
            outcome,
            PromptOutcome::AllowAlwaysMcpTool(ref n) if n == "notion__fetch"
        ));
    }

    #[test]
    fn mcp_fallback_client_returns_plain_allow_always() {
        // non-TUI clients (Generic / GrokWeb / Extension / …) see `fallback_options`
        // The legacy `"always-allow"` id maps to plain `PromptOutcome::AllowAlways`; the manager arm persists tool-scope from there
        let p = prompter(ClientType::Generic);
        let access = AccessKind::MCPTool {
            name: "linear__list".to_owned(),
            input: serde_json::Value::Null,
        };
        let opts = p.build_options(&access);
        assert!(
            opts.contains_key(&acp::PermissionOptionId::new("always-allow")),
            "fallback option set must contain legacy `always-allow` id"
        );
        assert!(
            !opts.contains_key(&acp::PermissionOptionId::new("allow-always-mcp")),
            "fallback clients must NOT see the `allow-always-mcp` option"
        );
        let outcome = outcome_for(&opts, "always-allow", None, &access);
        assert!(matches!(outcome, PromptOutcome::AllowAlways));
    }

    #[test]
    #[should_panic(expected = "MCP tool name invariant")]
    fn mcp_tool_action_debug_asserts_when_invariant_violated() {
        // server_prefix is Some(X) but tool_name doesn't start with X: that's a construction bug
        // debug_assert! fires in dev builds (release builds fall back to returning tool_name as-is)
        let _ = mcp_tool_action("totally-different-name", Some("linear"));
    }

    #[test]
    fn mcp_pretty_name_if_qualified_distinguishes_qualified_from_raw() {
        // Qualified MCP name: format as "(Server) Action" with both segments title-cased
        assert_eq!(
            mcp_pretty_name_if_qualified("linear__list_issues"),
            "(Linear) List Issues"
        );
        assert_eq!(mcp_pretty_name_if_qualified("123__lookup"), "(123) Lookup");
        assert_eq!(
            mcp_pretty_name_if_qualified("server:scope__tool"),
            "(Server:scope) Tool"
        );
        // Non-qualified input (a bash command, file path, or any string without `__`) is returned UNCHANGED
        // The helper must not title-case or mangle non-MCP strings
        assert_eq!(mcp_pretty_name_if_qualified("read_file"), "read_file");
        assert_eq!(mcp_pretty_name_if_qualified("cargo test"), "cargo test");
        assert_eq!(
            mcp_pretty_name_if_qualified("linear__shadow__exfil"),
            "linear__shadow__exfil"
        );
        assert_eq!(mcp_pretty_name_if_qualified(""), "");
    }

    #[test]
    fn mcp_titleize_segment_handles_snake_camel_kebab() {
        // snake_case: split into words, each title-cased
        assert_eq!(mcp_titleize_segment("list_issues"), "List Issues");
        assert_eq!(mcp_titleize_segment("grok_com_notion"), "Grok Com Notion");
        // single word: capitalize first letter
        assert_eq!(mcp_titleize_segment("linear"), "Linear");
        // camelCase preserved (no `_` to split on, only first letter touched)
        assert_eq!(mcp_titleize_segment("getMyTaskList"), "GetMyTaskList");
        // kebab-case preserved (no `_` to split on)
        assert_eq!(mcp_titleize_segment("notion-fetch"), "Notion-fetch");
        // empty input doesn't panic
        assert_eq!(mcp_titleize_segment(""), "");
    }

    // "Enable always-approve mode" option (prepended for TUI/Pager/Desktop)

    fn enable_always_approve_id() -> acp::PermissionOptionId {
        acp::PermissionOptionId::new(ENABLE_ALWAYS_APPROVE_OPTION_ID)
    }

    /// The enable-always-approve option must be the FIRST entry for every TUI/Pager/Desktop access kind.
    /// Its `index + 1` keyboard shortcut and visual prominence hinge on position 0.
    /// A regression that moves it later would silently make it harder to discover.
    #[test]
    fn enable_always_approve_is_first_option_for_pager() {
        let p = prompter(ClientType::GrokPager);
        let cases: Vec<(&str, AccessKind)> = vec![
            ("edit", AccessKind::Edit("write".to_owned())),
            ("bash", AccessKind::Bash("ls -la".to_owned())),
            (
                "mcp",
                AccessKind::MCPTool {
                    name: "linear__list".to_owned(),
                    input: serde_json::Value::Null,
                },
            ),
            // WebFetch URL must parse (manager rejects bad URLs before prompting, but the option list is still built defensively).
            (
                "web_fetch",
                AccessKind::WebFetch("https://example.com/a".to_owned()),
            ),
        ];
        for (label, access) in cases {
            let opts = p.build_options(&access);
            let first = opts
                .keys()
                .next()
                .unwrap_or_else(|| panic!("{label}: empty option list"));
            assert_eq!(
                first.0.as_ref(),
                ENABLE_ALWAYS_APPROVE_OPTION_ID,
                "{label}: enable-always-approve must be the first option",
            );
        }
    }

    /// Same pin as above for `GrokTUI` and `Desktop`: both client types route the option id through to the YOLO toggle, so both must see it.
    /// A copy-paste regression that limits the prepend to one client would be caught here.
    #[test]
    fn enable_always_approve_is_first_for_tui_and_desktop() {
        for ct in [ClientType::GrokTUI, ClientType::Desktop] {
            let p = prompter(ct);
            let opts = p.build_options(&AccessKind::Edit("write".to_owned()));
            assert_eq!(
                opts.keys().next().map(|k| k.0.as_ref()),
                Some(ENABLE_ALWAYS_APPROVE_OPTION_ID),
                "client {ct:?}: enable-always-approve must be position 0 for edits",
            );
        }
    }

    /// non-TUI clients (Generic / web / Extension / …) do NOT recognise the option id, so the prompter must NOT show it to them.
    /// If shown, selecting it would allow the current call without flipping any always-approve state.
    #[test]
    fn enable_always_approve_omitted_for_non_tui_clients() {
        for ct in [
            ClientType::Generic,
            ClientType::GrokWeb,
            ClientType::Nebula,
            ClientType::Extension,
        ] {
            let p = prompter(ct);
            let opts = p.build_options(&AccessKind::Edit("write".to_owned()));
            assert!(
                !opts.contains_key(&enable_always_approve_id()),
                "client {ct:?}: must NOT see enable-always-approve",
            );
        }
    }

    /// The option has `kind = AllowAlways` (for default-focus / YOLO drain safety).
    /// `map_selected_outcome` must override that to `PromptOutcome::AllowOnce`; the shell never persists per-tool state for this id.
    /// Pin the override for every access kind.
    #[test]
    fn enable_always_approve_maps_to_allow_once_for_every_access_kind() {
        let p = prompter(ClientType::GrokPager);
        let cases: Vec<(&str, AccessKind)> = vec![
            ("edit", AccessKind::Edit("write".to_owned())),
            (
                "agent_message",
                AccessKind::AgentMessage {
                    subagent_id: "sub-1".to_owned(),
                },
            ),
            ("bash", AccessKind::Bash("ls".to_owned())),
            (
                "mcp",
                AccessKind::MCPTool {
                    name: "linear__list".to_owned(),
                    input: serde_json::Value::Null,
                },
            ),
            (
                "web_fetch",
                AccessKind::WebFetch("https://example.com/a".to_owned()),
            ),
        ];
        for (label, access) in cases {
            let opts = p.build_options(&access);
            let outcome = outcome_for(&opts, ENABLE_ALWAYS_APPROVE_OPTION_ID, None, &access);
            assert!(
                matches!(outcome, PromptOutcome::AllowOnce),
                "{label}: enable-always-approve must map to AllowOnce, got {outcome:?}",
            );
        }
    }

    /// The option carries `kind = AllowOnce` so the pager's YOLO auto-approve drain (which sends the first `AllowOnce` response) picks it safely.
    /// The pager's `default_selected_permission` and sticky cursor logic skip this option when a target kind is in play.
    /// Pin the kind regardless.
    #[test]
    fn enable_always_approve_uses_allow_once_kind() {
        let p = prompter(ClientType::GrokPager);
        let opts = p.build_options(&AccessKind::Edit("write".to_owned()));
        let opt = opts
            .get(&enable_always_approve_id())
            .expect("enable-always-approve must be present for pager");
        assert_eq!(
            opt.kind,
            acp::PermissionOptionKind::AllowOnce,
            "enable-always-approve kind must be AllowOnce so the pager's \
             YOLO auto-approve drain (first AllowOnce) picks it safely",
        );
    }

    /// Bash on TUI/Pager/Desktop builds a custom option set with `allow-always-command` at position 0 by default.
    /// After the prepend, the enable-always-approve option must STILL be first, i.e. the prepend runs AFTER the bash-specific assembly.
    /// This pins the order: [enable-always-approve, allow-always-command, allow-once, reject-once, reject-always-command].
    #[test]
    fn bash_option_order_toggle_first_reject_always_last() {
        let p = prompter(ClientType::GrokPager);
        // "ls" has a parseable primary command, so `allow-always-command` will be inserted into the option list
        let access = AccessKind::Bash("ls -la".to_owned());
        let opts = p.build_options(&access);
        let ids: Vec<&str> = opts.keys().map(|k| k.0.as_ref()).collect();
        assert_eq!(
            ids.first().copied(),
            Some(ENABLE_ALWAYS_APPROVE_OPTION_ID),
            "enable-always-approve must be position 0",
        );
        assert_eq!(
            ids.get(1).copied(),
            Some("allow-always-command"),
            "allow-always-command must remain position 1 (right after \
             enable-always-approve), preserving bash UX",
        );
        assert_eq!(
            ids.last().copied(),
            Some("reject-always-command"),
            "reject-always-command must be LAST so the persistent deny \
             never sits between safe options",
        );
    }

    // ── events.jsonl emission ─────────────────────────────────

    #[test]
    fn tool_name_for_access_pins_canonical_names() {
        // This helper is the single source of truth shared with the permission manager's telemetry
        // Pin every variant so a rename can't slip through
        assert_eq!(tool_name_for_access(&AccessKind::Read(None)), "read_file");
        assert_eq!(
            tool_name_for_access(&AccessKind::Grep {
                path: None,
                glob: None
            }),
            "grep"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::Edit("x".into())),
            "search_replace"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::Bash("ls".into())),
            "run_terminal_command"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::AgentMessage {
                subagent_id: "sub-1".into(),
            }),
            "send_subagent_message"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::MCPTool {
                name: "linear__list".into(),
                input: serde_json::Value::Null,
            }),
            "mcp:linear__list"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::WebFetch("https://x".into())),
            "web_fetch"
        );
        assert_eq!(
            tool_name_for_access(&AccessKind::WebSearch("rust lang".into())),
            "web_search"
        );
    }

    #[test]
    fn decision_mapping_covers_allow_deny_cancel_followup() {
        // `PermissionDecision` has no `PartialEq`, so assert via `matches!`.
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::AllowOnce),
            PermissionDecision::Allow
        ));
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::AllowAlwaysMcpServer("s".into())),
            PermissionDecision::Allow
        ));
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::RejectOnce),
            PermissionDecision::Deny
        ));
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::Error("boom".into())),
            PermissionDecision::Deny
        ));
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::Cancelled),
            PermissionDecision::Cancelled
        ));
        assert!(matches!(
            permission_decision_for_outcome(&PromptOutcome::FollowupMessage("hi".into())),
            PermissionDecision::Followup
        ));
    }

    /// `request()` emits a `PermissionRequested` at prompt-start and a paired `PermissionResolved` at decision-time when an event writer is attached.
    /// A dropped-receiver gateway makes `request_permission` fail fast with `PromptOutcome::Error`.
    /// That still exercises both emissions and the Error-to-Deny decision mapping.
    #[tokio::test]
    async fn request_emits_permission_requested_and_resolved() {
        use distill_session_events::EventWriter;

        let dir = tempfile::tempdir().unwrap();
        let writer = EventWriter::open(dir.path());

        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // channel closed, so request_permission errors immediately
        let gateway = GatewaySender::new(tx);

        let prompter = AcpPrompter::new(
            acp::SessionId::new(Arc::from("sess-perm")),
            gateway,
            ClientType::Generic,
        )
        .with_event_writer(writer);

        let access = AccessKind::Bash("rm -rf /tmp/x".to_owned());
        let tool_call_update = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-1")),
            acp::ToolCallUpdateFields::default(),
        );

        let outcome = prompter
            .request(
                &access,
                &tool_call_update,
                /*protected_edit=*/ None,
                /*hook_ask=*/ None,
            )
            .await;
        assert!(
            matches!(outcome, PromptOutcome::Error(_)),
            "dropped gateway receiver should yield PromptOutcome::Error"
        );

        let text = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = text
            .trim()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "expected PermissionRequested + PermissionResolved"
        );
        let [requested, resolved] = lines.as_slice() else {
            panic!("expected two event lines: {lines:?}");
        };
        assert_eq!(
            requested.get("type").and_then(|v| v.as_str()),
            Some("permission_requested")
        );
        assert_eq!(
            requested.get("tool_name").and_then(|v| v.as_str()),
            Some("run_terminal_command")
        );
        assert_eq!(
            resolved.get("type").and_then(|v| v.as_str()),
            Some("permission_resolved")
        );
        assert_eq!(
            resolved.get("tool_name").and_then(|v| v.as_str()),
            Some("run_terminal_command")
        );
        assert_eq!(
            resolved.get("decision").and_then(|v| v.as_str()),
            Some("deny")
        );
        assert!(
            resolved.get("wait_ms").and_then(|v| v.as_u64()).is_some(),
            "PermissionResolved must carry wait_ms"
        );
    }

    /// Default constructor leaves the event writer as `noop()` so the live shell does not double-emit beside its own `EventTracker`.
    /// This test only checks `request()`'s `PromptOutcome`; emission is covered by `request_emits_permission_requested_and_resolved`.
    #[tokio::test]
    async fn request_with_default_noop_writer_returns_outcome() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let gateway = GatewaySender::new(tx);
        let prompter = AcpPrompter::new(
            acp::SessionId::new(Arc::from("sess-perm")),
            gateway,
            ClientType::Generic,
        );
        let access = AccessKind::Read(Some("/etc/hosts".to_owned()));
        let tool_call_update = acp::ToolCallUpdate::new(
            acp::ToolCallId::new(Arc::from("tc-2")),
            acp::ToolCallUpdateFields::default(),
        );
        let outcome = prompter
            .request(
                &access,
                &tool_call_update,
                /*protected_edit=*/ None,
                /*hook_ask=*/ None,
            )
            .await;
        assert!(matches!(outcome, PromptOutcome::Error(_)));
    }
}
