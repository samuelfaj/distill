// Modified for Distill by Samuel Fajreldines, 2026.
//! External OTEL schema v1: event names, attribute keys, typed records, and the per-event mapping functions.
//! The mapping functions are wired through the `telemetry_event!` macro's `external = …` arm.
//!
//! `ExternalRecord` is a **closed, typed structure**: attribute keys are the [`ExternalKey`] enum, not strings.
//! The compiler therefore enumerates every attribute that can possibly reach the wire.
//! Three independent mechanisms must be defeated to leak a new attribute.
//! They are this enum, the pinned [`EXTERNAL_ALLOWED_KEYS`] test, and the export-time validators in [`super::redact`].
//! Telemetry-owner review gates this file (CODEOWNERS).

use std::str::FromStr;

use super::config::ContentGates;
use crate::events;

/// Wire schema version, exported as resource attr `grok_code.schema.version`.
/// Additive changes (new events/attrs) do not bump it; renames/removals do.
pub const SCHEMA_VERSION: &str = "v1";

/// Meter/logger instrumentation scope name.
pub const SCOPE_NAME: &str = "ai.distill";

// ─────────────────────────────────────────────────────────────────────────────
// Event names
// ─────────────────────────────────────────────────────────────────────────────

/// External event names (`event.name` on the OTLP log record); the set is closed.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumCount, strum::AsRefStr, strum::IntoStaticStr,
)]
pub enum ExternalEventName {
    #[strum(serialize = "grok_code.session_start")]
    SessionStart,
    #[strum(serialize = "grok_code.session_end")]
    SessionEnd,
    #[strum(serialize = "grok_code.user_prompt")]
    UserPrompt,
    #[strum(serialize = "grok_code.turn_completed")]
    TurnCompleted,
    #[strum(serialize = "grok_code.api_request")]
    ApiRequest,
    #[strum(serialize = "grok_code.api_error")]
    ApiError,
    #[strum(serialize = "grok_code.tool_result")]
    ToolResult,
    #[strum(serialize = "grok_code.tool_decision")]
    ToolDecision,
    #[strum(serialize = "grok_code.mcp_server_connection")]
    McpServerConnection,
    #[strum(serialize = "grok_code.permission_mode_changed")]
    PermissionModeChanged,
    #[strum(serialize = "grok_code.skill_activated")]
    SkillActivated,
    #[strum(serialize = "grok_code.plugin_loaded")]
    PluginLoaded,
    #[strum(serialize = "grok_code.compaction")]
    Compaction,
    #[strum(serialize = "grok_code.subagent")]
    Subagent,
    #[strum(serialize = "grok_code.auth")]
    Auth,
    #[strum(serialize = "grok_code.internal_error")]
    InternalError,
    #[strum(serialize = "grok_code.model_switched")]
    ModelSwitched,
    #[strum(serialize = "grok_code.contextual_tip")]
    ContextualTip,
    #[strum(serialize = "grok_code.assistant_response")]
    AssistantResponse,
}
// ─────────────────────────────────────────────────────────────────────────────
// Attribute keys
// ─────────────────────────────────────────────────────────────────────────────

/// Every attribute key the external stream can attach to a log record.
/// You cannot attach an attribute the schema doesn't name.
/// Adding a variant trips the [`EXTERNAL_ALLOWED_KEYS`] pin test.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    strum::VariantArray,
    strum::AsRefStr,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[strum(serialize_all = "snake_case")]
pub enum ExternalKey {
    // Context / correlation (injected by emit.rs)
    #[strum(serialize = "session.id")]
    SessionId,
    TurnNumber,
    #[strum(serialize = "prompt.id")]
    PromptId,
    #[strum(serialize = "event.sequence")]
    EventSequence,
    // Identity (injected per-record from the identity snapshot)
    #[strum(serialize = "user.id")]
    UserId,
    #[strum(serialize = "user.email")]
    UserEmail,
    #[strum(serialize = "organization.id")]
    OrganizationId,
    #[strum(serialize = "team.id")]
    TeamId,
    #[strum(serialize = "deployment.id")]
    DeploymentId,
    // Session lifecycle
    Model,
    PermissionMode,
    McpServerCount,
    PluginCount,
    SkillCount,
    HookCount,
    MemoryEnabled,
    IsGitRepo,
    ClientIdentifier,
    DurationSecs,
    TurnCount,
    ToolCallCount,
    CompactionCount,
    // Prompt / turn
    PromptLength,
    Prompt,
    ResponseLength,
    Response,
    ScreenMode,
    CommandName,
    Outcome,
    DurationMs,
    ErrorCategory,
    CancellationCategory,
    // Model API
    StopReason,
    InputTokens,
    OutputTokens,
    ReasoningTokens,
    CacheReadTokens,
    CacheCreationTokens,
    CostUsdMicros,
    StatusCode,
    // Tools
    ToolName,
    Success,
    HookRewrote,
    FileExtension,
    ToolParameters,
    ToolInput,
    ToolOutput,
    FullCommand,
    ToolUseId,
    FilePath,
    Decision,
    AccessKind,
    Source,
    // MCP
    Status,
    TransportType,
    ToolCount,
    ErrorType,
    ErrorMessage,
    #[strum(serialize = "mcp_server.name")]
    McpServerName,
    #[strum(serialize = "mcp_tool.name")]
    McpToolName,
    // Permission mode
    FromMode,
    ToMode,
    Trigger,
    // Skills / plugins
    SkillSource,
    #[strum(serialize = "skill.name")]
    SkillName,
    InstallKind,
    PluginScope,
    PluginName,
    PluginVersion,
    // Compaction
    CompactionTrigger,
    CompactionOutcome,
    TokensBefore,
    TokensAfter,
    // Subagents
    Phase,
    SubagentType,
    // Auth
    AuthMethod,
    // Model switching
    FromModel,
    ToModel,
    ErrorCode,
    // Contextual tips
    Tip,
    Action,
}
/// Every [`ExternalKey`] variant, in declaration order; the pinned `external_allowed_keys_are_pinned` test guards the wire names against drift.
pub(crate) const ALL_KEYS: &[ExternalKey] = <ExternalKey as strum::VariantArray>::VARIANTS;

/// The runtime allowlist the export-time validators enforce: exactly the wire names of every [`ExternalKey`].
/// Pinned by an independent literal copy in the test module (mirroring `otel_layer::redact::allowlist_contents_are_pinned`).
pub(crate) fn external_allowed_keys() -> &'static std::collections::HashSet<&'static str> {
    static SET: std::sync::LazyLock<std::collections::HashSet<&'static str>> =
        std::sync::LazyLock::new(|| ALL_KEYS.iter().map(|k| k.as_ref()).collect());
    &SET
}

/// Redaction class of an attribute key: always safe to export, or content that rides a gate.
pub(crate) enum KeyPolicy {
    Safe,
    Gated(Gate),
}

/// Exhaustive Safe/Gated classification of every attribute key. A new [`ExternalKey`] variant
/// must be classified here or this match fails to compile, so a content-bearing key can never
/// reach the wire un-gated by omission.
pub(crate) fn key_policy(key: ExternalKey) -> KeyPolicy {
    use ExternalKey::*;
    use KeyPolicy::{Gated, Safe};
    match key {
        Prompt => Gated(Gate::UserPrompts),
        Response => Gated(Gate::AssistantResponses),
        ToolParameters | FilePath | SkillName | PluginName | PluginVersion => {
            Gated(Gate::ToolDetails)
        }
        ToolInput | ToolOutput | FullCommand | ErrorMessage => Gated(Gate::ToolContent),
        // McpServerName/McpToolName ride a safe placeholder by default and are gated at emit time,
        // so their key is safe at export; everything else is a label, count, id, or sanitized value.
        SessionId | TurnNumber | PromptId | EventSequence | UserId | UserEmail | OrganizationId
        | TeamId | DeploymentId | Model | PermissionMode | McpServerCount | PluginCount
        | SkillCount | HookCount | MemoryEnabled | IsGitRepo | ClientIdentifier | DurationSecs
        | TurnCount | ToolCallCount | CompactionCount | PromptLength | ResponseLength
        | ScreenMode | CommandName | Outcome | DurationMs | ErrorCategory
        | CancellationCategory | StopReason | InputTokens | OutputTokens | ReasoningTokens
        | CacheReadTokens | CacheCreationTokens | CostUsdMicros | StatusCode | ToolName
        | Success | HookRewrote | FileExtension | ToolUseId | Decision | AccessKind | Source
        | Status | TransportType | ToolCount | ErrorType | McpServerName | McpToolName
        | FromMode | ToMode | Trigger | SkillSource | InstallKind | PluginScope
        | CompactionTrigger | CompactionOutcome | TokensBefore | TokensAfter | Phase
        | SubagentType | AuthMethod | FromModel | ToModel | ErrorCode | Tip | Action => Safe,
    }
}

/// The gate guarding a wire key's content, if any. Backed by the exhaustive [`key_policy`] map.
pub(crate) fn gate_for_key(key: &str) -> Option<Gate> {
    match key_policy(ExternalKey::from_str(key).ok()?) {
        KeyPolicy::Gated(gate) => Some(gate),
        KeyPolicy::Safe => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Record structure
// ─────────────────────────────────────────────────────────────────────────────

/// No nested/array variants: the external schema is flat.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    I64(i64),
    Bool(bool),
    /// Tool-input JSON kept until emit so CONTENT-off paths skip `to_string()`.
    DeferredJson(serde_json::Value),
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}
impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        Self::I64(v as i64)
    }
}
impl From<u64> for AttrValue {
    fn from(v: u64) -> Self {
        Self::I64(v as i64)
    }
}
impl From<usize> for AttrValue {
    fn from(v: usize) -> Self {
        Self::I64(v as i64)
    }
}
impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// Content gate guarding a [`GatedAttr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `OTEL_LOG_USER_PROMPTS`.
    UserPrompts,
    /// `OTEL_LOG_TOOL_DETAILS`.
    ToolDetails,
    /// `OTEL_LOG_ASSISTANT_RESPONSES`. Unset follows [`Gate::UserPrompts`].
    AssistantResponses,
    /// `OTEL_LOG_TOOL_CONTENT`. Full bodies; does **not** follow details.
    ToolContent,
}

impl Gate {
    /// Whether this gate's content may be emitted under `gates`.
    /// The single source for the emit-time and export-time (`super::redact`) checks.
    pub(crate) fn is_open(self, gates: &ContentGates) -> bool {
        match self {
            Self::UserPrompts => gates.log_user_prompts,
            Self::ToolDetails => gates.log_tool_details,
            Self::AssistantResponses => gates.log_assistant_responses,
            Self::ToolContent => gates.log_tool_content,
        }
    }
}

/// An attribute emitted only when its gate is on.
/// When a gated attr shares a key with a default attr (e.g. verbatim vs. sanitized `tool_name`), the gated value replaces the default at emit time.
#[derive(Debug, Clone, PartialEq)]
pub struct GatedAttr {
    pub key: ExternalKey,
    pub gate: Gate,
    pub value: AttrValue,
}

/// Re-exported from [`super::metrics`] so the external wire schema is reachable here.
pub use super::metrics::MetricIncrement;

/// Curated external representation of one telemetry event: an optional log record plus derived metric increments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExternalRecord {
    /// Log-record event name; `None` for metric-only mappings (`SessionNew`).
    pub event: Option<ExternalEventName>,
    /// Attributes always emitted while the stream is active.
    pub attrs: Vec<(ExternalKey, AttrValue)>,
    /// Attributes emitted only when the matching gate is on.
    pub gated: Vec<GatedAttr>,
    /// Metric increments derived from this event.
    pub metrics: Vec<MetricIncrement>,
}

impl ExternalRecord {
    fn event(event: ExternalEventName) -> Self {
        Self {
            event: Some(event),
            ..Default::default()
        }
    }

    fn attr(mut self, key: ExternalKey, value: impl Into<AttrValue>) -> Self {
        self.attrs.push((key, value.into()));
        self
    }

    fn attr_opt(mut self, key: ExternalKey, value: Option<impl Into<AttrValue>>) -> Self {
        if let Some(v) = value {
            self.attrs.push((key, v.into()));
        }
        self
    }

    fn gated(mut self, key: ExternalKey, gate: Gate, value: impl Into<AttrValue>) -> Self {
        self.gated.push(GatedAttr {
            key,
            gate,
            value: value.into(),
        });
        self
    }

    fn gated_opt(self, key: ExternalKey, gate: Gate, value: Option<impl Into<AttrValue>>) -> Self {
        match value {
            Some(v) => self.gated(key, gate, v),
            None => self,
        }
    }

    fn metric(mut self, m: MetricIncrement) -> Self {
        self.metrics.push(m);
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Metric instrument schema (pinned attr keys)
// ─────────────────────────────────────────────────────────────────────────────

/// Every attribute key that may appear on a metric data point: the instrument-specific keys plus the per-increment identity/cardinality keys.
/// `prompt.id` is deliberately absent: its cardinality is unbounded, so it appears on events only.
/// Enforced fail-closed by `ValidatingMetricExporter` (drops the export on violation) and pinned by test.
pub(crate) const METRIC_ALLOWED_ATTR_KEYS: &[&str] = &[
    "type",
    "model",
    "outcome",
    "tool_name",
    "decision",
    "access_kind",
    "permission_mode",
    "error_category",
    "phase",
    "stuck_in",
    "auth_mode",
    "session.id",
    "app.version",
    "user.id",
    "user.email",
    "organization.id",
    "team.id",
    "deployment.id",
];

// ─────────────────────────────────────────────────────────────────────────────
// Sanitizers
// ─────────────────────────────────────────────────────────────────────────────

/// First-party `client_identifier` allowlist (pinned by test).
/// The underlying field is externally controlled free text from ACP client metadata, so it must never pass verbatim.
/// Unknown values collapse to `"other"`.
pub(crate) const KNOWN_CLIENT_IDENTIFIERS: &[&str] = &[
    "grok-pager",
    "grok-tui",
    "grok-shell",
    "grok-web",
    "grok-desktop",
    "grok-code-extension",
    "grok-agent-sdk",
    "nebula",
    "zed",
];

pub(crate) fn sanitize_client_identifier(raw: &str) -> &'static str {
    KNOWN_CLIENT_IDENTIFIERS
        .iter()
        .find(|known| **known == raw)
        .copied()
        .unwrap_or("other")
}

/// `screen_mode` allowlist (pinned by test).
/// Like `client_identifier`, the underlying value is externally controlled free text from ACP prompt metadata (`_meta.screenMode`).
/// It must never pass verbatim; unknown values collapse to `"other"`.
pub(crate) const KNOWN_SCREEN_MODES: &[&str] = &["fullscreen", "inline", "minimal", "headless"];

pub(crate) fn sanitize_screen_mode(raw: &str) -> &'static str {
    KNOWN_SCREEN_MODES
        .iter()
        .find(|known| **known == raw)
        .copied()
        .unwrap_or("other")
}

/// Built-in tool names (a closed enum in `distill-tools`) that pass verbatim through `tool_name` sanitization.
/// Pinned by test; everything else collapses.
pub(crate) const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write",
    "search_replace",
    "edit_notebook",
    "delete_file",
    "run_terminal_cmd",
    "run_terminal_command",
    "get_task_output",
    "get_command_or_subagent_output",
    "get_terminal_command_output",
    "kill_task",
    "kill_command_or_subagent",
    "kill_terminal_command",
    "wait_commands_or_subagents",
    "grep",
    "glob",
    "list_dir",
    "skill",
    "ask_user_question",
    "enter_plan_mode",
    "exit_plan_mode",
    "todo_write",
    "task",
    "spawn_subagent",
    "send_subagent_message",
    "web_search",
    "web_fetch",
    "lsp",
    "image_gen",
    "image_edit",
    "video_gen",
    "image_to_video",
    "reference_to_video",
    "monitor",
    "scheduler_create",
    "scheduler_delete",
    "scheduler_list",
    "search_tool",
    "use_tool",
    "memory_search",
    "memory_get",
    "update_goal",
];

/// Default `tool_name` reduction: built-ins pass verbatim, MCP-qualified names (`server__tool`) collapse to `"mcp_tool"`.
/// Anything else collapses to `"custom_tool"` (fail-closed; never export an unknown free-text name).
/// The verbatim name rides the `ToolDetails` gate.
pub(crate) fn sanitize_tool_name(raw: &str) -> &'static str {
    if let Some(known) = BUILTIN_TOOL_NAMES.iter().find(|n| **n == raw) {
        return known;
    }
    if raw.contains("__") {
        return "mcp_tool";
    }
    "custom_tool"
}

/// File-extension reduction for the always-on `file_extension` attribute.
/// The value is the extension only, lowercased, and capped at [`super::truncate::MAX_FILE_EXTENSION_LEN`] chars.
/// Anything else about the path is details-gated.
pub(crate) fn file_extension(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    let ext: String = ext
        .chars()
        .take(super::truncate::MAX_FILE_EXTENSION_LEN)
        .collect::<String>()
        .to_ascii_lowercase();
    (!ext.is_empty()).then_some(ext)
}

// ─────────────────────────────────────────────────────────────────────────────
// Mapping functions (`telemetry_event!(…, external = …)` targets)
// ─────────────────────────────────────────────────────────────────────────────

/// `SessionHarness` maps to `grok_code.session_start`.
/// Emitted from a spawn outside `TELEMETRY_CTX`, so `session.id` is mapped from the struct's own field.
pub fn map_session_start(ev: &events::SessionHarness) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::SessionStart)
            .attr(ExternalKey::SessionId, ev.session_id.as_str())
            .attr(ExternalKey::Model, ev.model_id.as_str())
            .attr(
                ExternalKey::PermissionMode,
                <&'static str>::from(ev.permission_mode),
            )
            .attr(ExternalKey::McpServerCount, ev.mcp_server_names.len())
            .attr(ExternalKey::PluginCount, ev.plugin_names.len())
            .attr(ExternalKey::SkillCount, ev.skill_names.len())
            .attr(ExternalKey::HookCount, ev.hook_names.len())
            .attr(ExternalKey::MemoryEnabled, ev.memory_enabled)
            .attr(ExternalKey::IsGitRepo, ev.is_git_repo)
            .attr_opt(
                ExternalKey::ClientIdentifier,
                ev.client_identifier
                    .as_deref()
                    .map(sanitize_client_identifier),
            ),
    )
}

/// `SessionNew` increments `grok_code.session.count` (metric only; the `session_start` log record comes from the richer `SessionHarness`).
pub fn map_session_new(ev: &events::SessionNew) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::default()
            .attr(ExternalKey::SessionId, ev.session_id.as_str())
            .metric(MetricIncrement::SessionCount),
    )
}

/// `SessionEnded` maps to `grok_code.session_end`.
pub fn map_session_end(ev: &events::SessionEnded) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::SessionEnd)
            .attr(ExternalKey::DurationSecs, ev.duration_secs)
            .attr(ExternalKey::TurnCount, ev.turn_count)
            .attr(ExternalKey::ToolCallCount, ev.tool_call_count)
            .attr(ExternalKey::CompactionCount, ev.compaction_count)
            .attr(ExternalKey::Model, ev.model_id.as_str()),
    )
}

/// Pull the bash/command string from raw tool args. First-class `full_command`
/// uses this *before* `reduce_tool_input` so a command longer than 512 chars
/// is not collapsed to 128.
pub(crate) fn extract_full_command(params: &serde_json::Value) -> Option<&str> {
    ["command", "cmd", "bash_command"]
        .iter()
        .find_map(|k| params.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
}

/// MCP `server__tool` split. Empty halves are not MCP-qualified.
pub(crate) fn parse_mcp_qualified_name(raw: &str) -> Option<(&str, &str)> {
    let (server, tool) = raw.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

fn attach_mcp_names(rec: ExternalRecord, tool_name: &str) -> ExternalRecord {
    let Some((server, tool)) = parse_mcp_qualified_name(tool_name) else {
        return rec;
    };
    rec.attr(ExternalKey::McpToolName, "mcp_tool")
        .attr(ExternalKey::McpServerName, "mcp_server")
        .gated(ExternalKey::McpToolName, Gate::ToolDetails, tool)
        .gated(ExternalKey::McpServerName, Gate::ToolDetails, server)
}

fn attach_tool_input(
    mut rec: ExternalRecord,
    parameters: Option<&serde_json::Value>,
    tool_use_id: Option<&str>,
) -> ExternalRecord {
    rec = rec.attr_opt(
        ExternalKey::ToolUseId,
        tool_use_id.filter(|s| !s.is_empty()),
    );
    if let Some(params) = parameters {
        rec = rec.gated(
            ExternalKey::ToolParameters,
            Gate::ToolDetails,
            super::truncate::reduce_tool_input(params),
        );
        rec = rec.gated(
            ExternalKey::ToolInput,
            Gate::ToolContent,
            AttrValue::DeferredJson(params.clone()),
        );
        if let Some(cmd) = extract_full_command(params) {
            rec = rec.gated(ExternalKey::FullCommand, Gate::ToolContent, cmd);
        }
    }
    rec
}

/// `PromptSubmitted` maps to `grok_code.user_prompt`.
/// Prompt text rides the `UserPrompts` gate (60 KB cap applied at emit time).
/// `command_name` is always-on slash/skill metadata, never the user prompt body.
pub fn map_user_prompt(ev: &events::PromptSubmitted) -> Option<ExternalRecord> {
    let mut rec = ExternalRecord::event(ExternalEventName::UserPrompt)
        .attr(ExternalKey::PromptLength, ev.prompt_length)
        .attr(ExternalKey::Model, ev.model_id.as_str())
        .attr_opt(
            ExternalKey::ScreenMode,
            ev.screen_mode.as_deref().map(sanitize_screen_mode),
        )
        .attr_opt(
            ExternalKey::CommandName,
            ev.command_name.as_deref().filter(|s| !s.is_empty()),
        );
    if let Some(text) = ev.prompt_text.as_deref() {
        rec = rec.gated(ExternalKey::Prompt, Gate::UserPrompts, text);
    }
    Some(rec)
}

/// `TurnCompleted` maps to `grok_code.turn_completed` and increments `turn.count` (and `error.count` on error outcomes).
pub fn map_turn_completed(ev: &events::TurnCompleted) -> Option<ExternalRecord> {
    let outcome: &'static str = ev.outcome.into();
    let mut rec = ExternalRecord::event(ExternalEventName::TurnCompleted)
        .attr(ExternalKey::Outcome, outcome)
        .attr(ExternalKey::DurationMs, ev.duration_ms)
        .attr(ExternalKey::ToolCallCount, ev.tool_call_count)
        .attr(ExternalKey::Model, ev.model_id.as_str())
        // `emit_record` falls back to the ambient ctx when this is absent.
        .attr_opt(ExternalKey::SessionId, ev.session_id.as_deref())
        .attr_opt(ExternalKey::ErrorCategory, ev.error_category.as_deref())
        .attr_opt(
            ExternalKey::CancellationCategory,
            ev.cancellation_category.as_deref(),
        )
        .metric(MetricIncrement::TurnCount {
            outcome,
            model: ev.model_id.clone(),
        });
    if matches!(ev.outcome, events::Outcome::Error) {
        rec = rec.metric(MetricIncrement::ErrorCount {
            error_category: ev
                .error_category
                .clone()
                .unwrap_or_else(|| "unknown".to_owned()),
            model: ev.model_id.clone(),
        });
    }
    Some(rec)
}

/// Exports the per-turn first-response histograms, each gated independently: a turn with no model
/// output records no ttft, and a reasoning-only or tool-only turn records ttft but no ttfm.
pub fn map_prompt_latency(ev: &events::PromptLatency) -> Option<ExternalRecord> {
    let mut rec = ExternalRecord::default();
    if let Some(duration_ms) = ev.ttft_ms {
        rec = rec.metric(MetricIncrement::TurnTtft {
            duration_ms,
            model: ev.model_id.clone(),
        });
    }
    if let Some(duration_ms) = ev.ttfm_ms {
        rec = rec.metric(MetricIncrement::TurnTtfm {
            duration_ms,
            model: ev.model_id.clone(),
        });
    }
    (!rec.metrics.is_empty()).then_some(rec)
}

/// `ModelResponseReceived` maps to `grok_code.api_request` and increments `token.usage`.
pub fn map_api_request(ev: &events::ModelResponseReceived) -> Option<ExternalRecord> {
    let mut rec = ExternalRecord::event(ExternalEventName::ApiRequest)
        .attr(ExternalKey::Model, ev.model_id.as_str())
        .attr(ExternalKey::DurationMs, ev.duration_ms)
        .attr_opt(ExternalKey::StopReason, ev.stop_reason.as_deref())
        .attr_opt(ExternalKey::InputTokens, ev.prompt_tokens)
        .attr_opt(ExternalKey::OutputTokens, ev.completion_tokens)
        .attr_opt(ExternalKey::ReasoningTokens, ev.reasoning_tokens)
        .attr_opt(ExternalKey::CacheReadTokens, ev.cached_prompt_tokens)
        .attr_opt(ExternalKey::CacheCreationTokens, ev.cache_creation_tokens);
    // No float `AttrValue` variant, so the attr is integer micros.
    if let Some(ticks) = ev.cost_usd_ticks.filter(|t| *t > 0) {
        rec = rec.attr(ExternalKey::CostUsdMicros, ticks / 10_000).metric(
            MetricIncrement::CostUsage {
                model: ev.model_id.clone(),
                cost_usd: ticks as f64 / 1e10,
            },
        );
    }
    for (token_type, count) in [
        ("input", ev.prompt_tokens),
        ("output", ev.completion_tokens),
        ("reasoning", ev.reasoning_tokens),
        ("cache_read", ev.cached_prompt_tokens),
        ("cache_creation", ev.cache_creation_tokens),
    ] {
        if let Some(count) = count.filter(|c| *c > 0) {
            rec = rec.metric(MetricIncrement::TokenUsage {
                token_type,
                model: ev.model_id.clone(),
                count: count as u64,
            });
        }
    }
    Some(rec)
}

/// `RateLimitHit` maps to `grok_code.api_error` (`error_category = rate_limit`).
/// No `error.count` increment: a rate-limited turn (retries exhausted) also ends in `TurnCompleted{outcome: Error}`, the single increment source.
/// Incrementing here too would double-count the failure.
pub fn map_rate_limit_hit(ev: &events::RateLimitHit) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::ApiError)
            .attr(ExternalKey::ErrorCategory, "rate_limit")
            .attr(ExternalKey::Model, ev.model_id.as_str()),
    )
}

/// It carries category/class enums only, no message text. Deliberately no `error.count` increment: `ApiError` is emitted
/// alongside `TurnCompleted{outcome: Error}` for the same failure. The metric's increment sources are exactly
/// `TurnCompleted{Error}` and `RateLimitHit`; adding one here would double-count every failed turn.
pub fn map_api_error(ev: &events::ApiError) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::ApiError)
            .attr(ExternalKey::ErrorCategory, ev.error_category.as_str())
            .attr(ExternalKey::Model, ev.model_id.as_str())
            .attr_opt(ExternalKey::StatusCode, ev.status_code.map(|c| c as i64))
            .attr_opt(ExternalKey::DurationMs, ev.duration_ms),
    )
}

/// `ToolCallCompleted` maps to `grok_code.tool_result` and increments `tool.usage`.
pub fn map_tool_result(ev: &events::ToolCallCompleted) -> Option<ExternalRecord> {
    let sanitized = sanitize_tool_name(&ev.tool_name);
    // The snake_case wire label from `strum::IntoStaticStr`, so a new variant cannot drift from a hand-written arm
    let outcome: &'static str = ev.outcome.into();
    let mut rec = ExternalRecord::event(ExternalEventName::ToolResult)
        .attr(ExternalKey::ToolName, sanitized)
        .attr(ExternalKey::Outcome, outcome)
        .attr(ExternalKey::Success, ev.outcome.ran_successfully())
        .attr(ExternalKey::HookRewrote, ev.hook_rewrote)
        .attr(ExternalKey::DurationMs, ev.duration_ms)
        .attr(ExternalKey::Model, ev.model_id.as_str())
        .gated(
            ExternalKey::ToolName,
            Gate::ToolDetails,
            ev.tool_name.as_str(),
        )
        .metric(MetricIncrement::ToolUsage {
            tool_name: sanitized.to_owned(),
            outcome,
            model: ev.model_id.clone(),
        });
    rec = attach_mcp_names(rec, &ev.tool_name);
    if let Some(path) = ev.file_path.as_deref() {
        rec = rec
            .attr_opt(ExternalKey::FileExtension, file_extension(path))
            .gated(ExternalKey::FilePath, Gate::ToolDetails, path);
    }
    if let Some(output) = ev.tool_output.as_deref().filter(|s| !s.is_empty()) {
        rec = rec.gated(ExternalKey::ToolOutput, Gate::ToolContent, output);
    }
    if !ev.outcome.ran_successfully()
        && let Some(msg) = ev.error_message.as_deref().filter(|s| !s.is_empty())
    {
        rec = rec.gated(ExternalKey::ErrorMessage, Gate::ToolContent, msg);
    }
    Some(attach_tool_input(
        rec,
        ev.parameters.as_ref(),
        ev.tool_use_id.as_deref(),
    ))
}

/// `PermissionDecisionRecord` maps to `grok_code.tool_decision` and increments `tool.decision`.
/// Mixpanel serializes only [`events::PermissionDecisionPayload`]; tool args
/// ride [`events::ExternalToolInput`] into [`attach_tool_input`].
pub fn map_tool_decision(ev: &events::PermissionDecisionRecord) -> Option<ExternalRecord> {
    let sanitized = sanitize_tool_name(&ev.payload.tool_name);
    let decision: &'static str = ev.payload.decision.into();
    let access_kind: &'static str = ev.payload.access_kind.into();
    let permission_mode: &'static str = ev.payload.permission_mode.into();
    let rec = ExternalRecord::event(ExternalEventName::ToolDecision)
        .attr(ExternalKey::ToolName, sanitized)
        .attr(ExternalKey::Decision, decision)
        .attr(ExternalKey::AccessKind, access_kind)
        .attr(ExternalKey::PermissionMode, permission_mode)
        .attr_opt(ExternalKey::Source, ev.payload.source.as_deref())
        .gated(
            ExternalKey::ToolName,
            Gate::ToolDetails,
            ev.payload.tool_name.as_str(),
        )
        .metric(MetricIncrement::ToolDecision {
            tool_name: sanitized.to_owned(),
            decision,
            access_kind,
            permission_mode,
        });
    let rec = attach_mcp_names(rec, &ev.payload.tool_name);
    Some(attach_tool_input(
        rec,
        ev.tool_input.parameters.as_ref(),
        ev.tool_input.tool_use_id.as_deref(),
    ))
}

/// `AssistantResponse` → `grok_code.assistant_response`. `response_length` is
/// always-on; `response` rides `AssistantResponses` and is omitted on
/// tool-only turns (`response_length == 0`) even when the gate is on.
pub fn map_assistant_response(ev: &events::AssistantResponse) -> Option<ExternalRecord> {
    let mut rec = ExternalRecord::event(ExternalEventName::AssistantResponse)
        .attr(ExternalKey::ResponseLength, ev.response_length);
    if ev.response_length > 0
        && let Some(text) = ev.response_text.as_deref().filter(|s| !s.is_empty())
    {
        rec = rec.gated(ExternalKey::Response, Gate::AssistantResponses, text);
    }
    Some(rec)
}

/// `McpServerConnected` maps to `grok_code.mcp_server_connection` (`status=connected`).
/// Server name collapses to `"mcp_server"` by default (name is details-gated).
pub fn map_mcp_server_connected(ev: &events::McpServerConnected) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::McpServerConnection)
            .attr(ExternalKey::Status, "connected")
            .attr(
                ExternalKey::TransportType,
                <&'static str>::from(ev.transport),
            )
            .attr(ExternalKey::DurationMs, ev.duration_ms)
            .attr(ExternalKey::ToolCount, ev.tool_count)
            .attr(ExternalKey::McpServerName, "mcp_server")
            .gated(
                ExternalKey::McpServerName,
                Gate::ToolDetails,
                ev.server_name.as_str(),
            ),
    )
}

/// `McpServerFailed` maps to `grok_code.mcp_server_connection` (`status=failed`).
pub fn map_mcp_server_failed(ev: &events::McpServerFailed) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::McpServerConnection)
            .attr(ExternalKey::Status, "failed")
            .attr(ExternalKey::ErrorType, ev.error_type.as_ref())
            .attr(ExternalKey::DurationMs, ev.duration_ms)
            .attr(ExternalKey::McpServerName, "mcp_server")
            .gated(
                ExternalKey::McpServerName,
                Gate::ToolDetails,
                ev.server_name.as_str(),
            )
            .gated_opt(
                ExternalKey::ErrorMessage,
                Gate::ToolContent,
                ev.error_message.as_deref().filter(|s| !s.is_empty()),
            ),
    )
}

/// `PlanModeToggled` maps to `grok_code.permission_mode_changed`.
pub fn map_plan_mode_toggled(ev: &events::PlanModeToggled) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::PermissionModeChanged)
            .attr_opt(
                ExternalKey::FromMode,
                ev.from_mode.as_deref().filter(|s| !s.is_empty()),
            )
            .attr(
                ExternalKey::ToMode,
                if ev.enabled { "plan" } else { "default" },
            )
            .attr(ExternalKey::Trigger, <&'static str>::from(ev.trigger)),
    )
}

/// `ContextualTip` maps to `grok_code.contextual_tip`.
/// The attrs are labels only (no user content), so nothing here is gated.
pub fn map_contextual_tip(ev: &events::ContextualTip) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::ContextualTip)
            .attr(ExternalKey::Tip, <&'static str>::from(ev.tip))
            .attr(ExternalKey::Action, <&'static str>::from(ev.action)),
    )
}

/// `YoloToggled` maps to `grok_code.permission_mode_changed`.
pub fn map_yolo_toggled(ev: &events::YoloToggled) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::PermissionModeChanged)
            .attr(
                ExternalKey::FromMode,
                ev.from_mode.as_deref().unwrap_or(if ev.previous_state {
                    "bypass_permissions"
                } else {
                    "default"
                }),
            )
            .attr(
                ExternalKey::ToMode,
                if ev.enabled {
                    "bypass_permissions"
                } else {
                    "default"
                },
            )
            .attr(ExternalKey::Trigger, <&'static str>::from(ev.trigger)),
    )
}

/// `SkillDispatched` maps to `grok_code.skill_activated`.
/// Skill names are details-gated; source and trigger export by default.
pub fn map_skill_activated(ev: &events::SkillDispatched) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::SkillActivated)
            .attr_opt(ExternalKey::SkillSource, ev.skill_source.as_deref())
            .attr(ExternalKey::Trigger, <&'static str>::from(ev.trigger))
            .gated(
                ExternalKey::SkillName,
                Gate::ToolDetails,
                ev.skill_name.as_str(),
            ),
    )
}

/// `PluginInstalled` maps to `grok_code.plugin_loaded`.
pub fn map_plugin_installed(ev: &events::PluginInstalled) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::PluginLoaded)
            .attr(ExternalKey::InstallKind, ev.install_kind.as_ref())
            .attr(ExternalKey::Success, ev.success)
            .attr_opt(ExternalKey::ErrorCategory, ev.error_category.as_deref()),
    )
}

/// `PluginUsed` maps to `grok_code.plugin_loaded` (plugin name details-gated).
pub fn map_plugin_used(ev: &events::PluginUsed) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::PluginLoaded)
            .attr(ExternalKey::Success, ev.success)
            .gated(
                ExternalKey::PluginName,
                Gate::ToolDetails,
                ev.plugin_name.as_str(),
            ),
    )
}

/// `CompactionCompleted` maps to `grok_code.compaction`.
pub fn map_compaction(ev: &events::CompactionCompleted) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::Compaction)
            .attr(ExternalKey::DurationMs, ev.duration_ms)
            .attr(ExternalKey::TokensBefore, ev.tokens_before)
            .attr(ExternalKey::TokensAfter, ev.tokens_after)
            .attr_opt(ExternalKey::Model, ev.model_id.as_deref()),
    )
}

/// `CompactionTriggered` is not mapped; the `grok_code.compaction` trigger attrs ride on the completion event instead (one event per compaction).
/// `SubagentLaunched` maps to `grok_code.subagent` (`phase=launched`).
pub fn map_subagent_launched(ev: &events::SubagentLaunched) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::Subagent)
            .attr(ExternalKey::Phase, "launched")
            .attr(ExternalKey::SubagentType, ev.subagent_type.as_str()),
    )
}

/// `SubagentCompleted` maps to `grok_code.subagent` (`phase=completed`).
pub fn map_subagent_completed(ev: &events::SubagentCompleted) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::Subagent)
            .attr(ExternalKey::Phase, "completed")
            .attr(ExternalKey::Outcome, <&'static str>::from(ev.outcome))
            .attr(ExternalKey::DurationMs, ev.duration_ms),
    )
}

/// `Login` maps to `grok_code.auth`.
pub fn map_auth(ev: &events::Login) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::Auth)
            .attr(ExternalKey::AuthMethod, ev.auth_method.as_str()),
    )
}

/// `InternalError` maps to `grok_code.internal_error`.
/// Only the error class is exported: no message, no location.
pub fn map_internal_error(ev: &events::InternalError) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::InternalError)
            .attr(ExternalKey::ErrorType, ev.error_type.as_str()),
    )
}

/// `AgentConnect` maps to the phase histogram and the timeout counter (no external log event).
pub fn map_agent_connect(ev: &events::AgentConnect) -> Option<ExternalRecord> {
    let mut rec = ExternalRecord::default();
    for (phase, duration_ms) in &ev.phase_durations_ms {
        rec = rec.metric(MetricIncrement::StartupPhaseDuration {
            phase: phase.clone(),
            duration_ms: *duration_ms,
            outcome: ev.outcome.label().to_string(),
            auth_mode: ev.auth_mode.label().to_string(),
        });
    }
    if ev.outcome == crate::startup::StartupOutcome::Timeout {
        let stuck = ev.stuck_in.clone().unwrap_or_else(|| "unknown".to_owned());
        rec = rec.metric(MetricIncrement::StartupTimeout {
            stuck_in: stuck,
            auth_mode: ev.auth_mode.label().to_string(),
        });
    }
    Some(rec)
}

/// `SessionCreateFailed` maps to the session-create timeout counter (no external log event).
pub fn map_session_create_failed(ev: &events::SessionCreateFailed) -> Option<ExternalRecord> {
    if ev.outcome != crate::startup::StartupOutcome::Timeout {
        return None;
    }
    let stuck = ev
        .stuck_phase
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    Some(
        ExternalRecord::default().metric(MetricIncrement::SessionCreateTimeout { stuck_in: stuck }),
    )
}

pub fn map_startup_sub_timers(ev: &events::StartupSubTimers) -> Option<ExternalRecord> {
    if ev.timings.is_empty() {
        return None;
    }
    let mut rec = ExternalRecord::default();
    for (phase, duration_ms) in &ev.timings {
        rec = rec.metric(MetricIncrement::StartupSubTimerDuration {
            phase: phase.clone(),
            duration_ms: *duration_ms,
            outcome: ev.outcome.label().to_string(),
            auth_mode: ev.auth_mode.label().to_string(),
        });
    }
    Some(rec)
}

/// `StartupCompleted` maps to the total histogram (no external log event).
pub fn map_startup_completed(ev: &events::StartupCompleted) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::default().metric(MetricIncrement::StartupTotal {
            duration_ms: ev.total_ms,
            outcome: ev.outcome.label().to_string(),
            auth_mode: ev.auth_mode.label().to_string(),
        }),
    )
}

pub fn map_startup_interactive(ev: &events::StartupInteractive) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::default().metric(MetricIncrement::StartupInteractive {
            duration_ms: ev.interactive_ms,
            auth_mode: ev.auth_mode.label().to_string(),
        }),
    )
}

/// `ModelSwitched` maps to `grok_code.model_switched`.
pub fn map_model_switched(ev: &events::ModelSwitched) -> Option<ExternalRecord> {
    Some(
        ExternalRecord::event(ExternalEventName::ModelSwitched)
            .attr(ExternalKey::SessionId, ev.session_id.as_str())
            .attr(ExternalKey::FromModel, ev.previous_model_id.as_str())
            .attr(ExternalKey::ToModel, ev.new_model_id.as_str())
            .attr(ExternalKey::Success, ev.success)
            .attr_opt(ExternalKey::ErrorCode, ev.error_code.as_deref()),
    )
}

#[cfg(test)]
mod access_kind_label_tests {
    use super::*;

    #[test]
    fn agent_message_has_dedicated_label() {
        assert_eq!(
            <&'static str>::from(events::AccessKind::AgentMessage),
            "agent_message"
        );
    }
}
