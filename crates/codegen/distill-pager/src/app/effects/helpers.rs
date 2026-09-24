// Modified for Distill by Samuel Fajreldines, 2026.
#![cfg_attr(rustfmt, rustfmt::skip)]
use std::path::Path;
use agent_client_protocol as acp;
use tokio::task::JoinSet;
use distill_acp_lib::{AcpAgentTx, acp_send};
use distill_telemetry::events::ClipboardProbeDropReason;
use super::actions::{
    PermissionModePersist, ProbedAttachment, SubagentKillOutcome, TaskResult,
};
use super::agent::AgentId;
use crate::unified_log as ulog;
use distill_shell::sampling::error::{
    RATE_LIMITED_ERROR_CODE, error_detail_from_data, error_kind_str_from_error,
    format_rate_limited_user_message, http_status_from_error,
};
use distill_shell::session::ExtMethodResult;
use distill_shell::session::helpers::session_compact::{
    COMPACT_CANCELLED_MSG, CompactErrorKind, compact_error_kind,
};
/// Floor for the session create/load RPCs.
const SESSION_RPC_FLOOR: std::time::Duration = std::time::Duration::from_secs(180);
/// Headroom over the agent-side `.envrc` budget for the rest of session setup.
const SESSION_RPC_SLACK: std::time::Duration = std::time::Duration::from_secs(50);
/// Always covers the agent-side `.envrc` budget so the backstop cannot fire before the agent's own deadline.
/// Reads `GROK_ENVRC_TIMEOUT_SECS` in this process; the agent inherits the same environment.
pub(super) fn session_rpc_timeout() -> std::time::Duration {
    SESSION_RPC_FLOOR.max(distill_workspace::envrc::loader_budget() + SESSION_RPC_SLACK)
}
/// Why a bounded session RPC failed; `TimedOut` is observed at the deadline, not inferred later.
#[derive(Debug)]
pub(crate) enum SessionRpcError {
    TimedOut { action: String, timeout: std::time::Duration },
    Rpc(acp::Error),
}
impl SessionRpcError {
    pub(crate) fn timed_out(&self) -> bool {
        matches!(self, Self::TimedOut { .. })
    }
}
impl std::fmt::Display for SessionRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut { action, timeout } => {
                write!(
                f,
                "{action} timed out after {}s. It may still finish in the background; \
                 retrying right away can run into the same delay.",
                timeout.as_secs()
            )
            }
            Self::Rpc(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for SessionRpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rpc(e) => Some(e),
            Self::TimedOut { .. } => None,
        }
    }
}
/// `acp_send` bounded by [`session_rpc_timeout`], returning a typed [`SessionRpcError`] on expiry.
pub(crate) async fn acp_send_bounded<R, T>(
    request: T,
    tx: &tokio::sync::mpsc::UnboundedSender<R>,
    action: &str,
) -> Result<T::Response, SessionRpcError>
where
    T: distill_acp_lib::AcpRequest,
    R: From<distill_acp_lib::AcpArgs<T>> + std::fmt::Debug,
{
    let timeout = session_rpc_timeout();
    match tokio::time::timeout(timeout, acp_send(request, tx)).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(SessionRpcError::Rpc(e)),
        Err(_elapsed) => {
            Err(SessionRpcError::TimedOut {
                action: action.to_owned(),
                timeout,
            })
        }
    }
}
/// Timeout message naming the step a create stalled on.
pub(crate) fn timed_out_while(step: &str) -> String {
    format!(
        "Couldn't start the session: it timed out while {step}. It may still finish in the \
         background, so give it a moment before trying again."
    )
}
/// Typed progress message for session restore.
/// Keeps the progress channel from accepting arbitrary `TaskResult` variants.
pub(crate) struct RestoreProgressMsg {
    pub agent_id: AgentId,
    pub message: String,
}
pub(super) fn log_prompt_result(
    session_id: &acp::SessionId,
    result: &Result<acp::PromptResponse, acp::Error>,
) {
    let sid = &session_id.0;
    match result {
        Ok(_) => ulog::info("agent response complete", Some(sid), None),
        Err(e) => {
            ulog::error(
                "agent response failed",
                Some(sid),
                Some(serde_json::json!({"error": e.to_string()})),
            )
        }
    }
}
/// Delay between post-install MCP-list re-probes (`Effect::RetryPluginCtaMcps`).
pub(super) const CTA_MCP_RETRY_DELAY_MS: u64 = 1000;
/// How long the CTA shows its "installed" confirmation before auto-dismissing.
pub(super) const CTA_INSTALLED_DISMISS_MS: u64 = 4000;
/// Upper bound on the off-thread clipboard-attachment probe.
/// A wedged osascript read must not pin `paste_probe_in_flight` and silently stash every later send.
pub(super) const CLIPBOARD_PROBE_TIMEOUT_SECS: u64 = 10;
const _: () = assert!(
    CLIPBOARD_PROBE_TIMEOUT_SECS > crate::clipboard::OSASCRIPT_WAIT.as_secs()
);
pub(super) type ClipboardProbeStage = Result<
    (ProbedAttachment, Option<String>),
    crate::clipboard::ProbeDrop,
>;
/// The blocking half of one probe: guarded pasteboard read, decode, session persist. Never runs on the render thread.
pub(super) fn probe_clipboard_attachment_blocking(
    change_count: Option<u64>,
    probe_text: Option<String>,
    probe_bracketed: bool,
    images_dir: Option<std::path::PathBuf>,
) -> ClipboardProbeStage {
    let (image, file_urls) = crate::clipboard::guarded_pasteboard_read(
        change_count,
        crate::clipboard::clipboard_change_count,
        || {
            if probe_bracketed
                && crate::terminal::terminal_context()
                    .brand
                    .delivers_ime_as_bracketed_paste()
            {
                match crate::clipboard::bracketed_payload_came_from_clipboard_result(
                    probe_text.as_deref().unwrap_or(""),
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err(ClipboardProbeDropReason::BracketedPayloadMismatch);
                    }
                    Err(_) => return Err(ClipboardProbeDropReason::ReadFailed),
                }
            }
            crate::clipboard::system_clipboard_probe_attachments(probe_text.as_deref())
        },
    )?;
    let Some(data) = image else {
        return Ok((ProbedAttachment::NoRaster, file_urls));
    };
    let mut pasted = crate::prompt_images::from_clipboard_data(&data);
    pasted.prepare_preview_blocking();
    if let Some(dir) = images_dir
        && let Err(error) = crate::prompt_images::persist_to_session(&mut pasted, &dir)
    {
        tracing::warn!(error = %error, "pasted image could not be persisted into the session");
        return Err(crate::clipboard::ProbeDrop {
            reason: ClipboardProbeDropReason::PersistFailed,
            image: Some(data),
            message: Some(error.to_string()),
        });
    }
    Ok((ProbedAttachment::Image(pasted), file_urls))
}
/// The stage on the blocking pool under one deadline; `spawn_blocking` cannot be cancelled, so an expired deadline only stops waiting.
pub(super) async fn clipboard_probe_stage(
    deadline: std::time::Duration,
    work: impl FnOnce() -> ClipboardProbeStage + Send + 'static,
) -> ClipboardProbeStage {
    let dropped = |reason| crate::clipboard::ProbeDrop {
        reason,
        image: None,
        message: None,
    };
    match tokio::time::timeout(deadline, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(stage)) => stage,
        Ok(Err(join_error)) => {
            tracing::warn!(error = %join_error, "clipboard attachment probe task failed");
            Err(dropped(ClipboardProbeDropReason::Panicked))
        }
        Err(_elapsed) => {
            tracing::warn!("clipboard attachment probe timed out");
            Err(dropped(ClipboardProbeDropReason::Timeout))
        }
    }
}
/// One deadline over the whole stage so a stall anywhere still completes and cannot strand a send parked behind
/// `paste_probe_in_flight`. Drop telemetry is emitted after the deadline check so a late stage never reports twice.
pub(super) async fn bounded_clipboard_probe(
    deadline: std::time::Duration,
    work: impl FnOnce() -> ClipboardProbeStage + Send + 'static,
) -> (ProbedAttachment, Option<String>) {
    let started = std::time::Instant::now();
    match clipboard_probe_stage(deadline, work).await {
        Ok(outcome) => outcome,
        Err(dropped) => {
            crate::clipboard::log_clipboard_probe_dropped(
                dropped.reason,
                dropped.image.as_ref(),
                started,
            );
            let attachment = match dropped.reason {
                ClipboardProbeDropReason::ReadFailed
                | ClipboardProbeDropReason::Timeout
                | ClipboardProbeDropReason::Panicked => ProbedAttachment::ProbeFailed,
                ClipboardProbeDropReason::PersistFailed => {
                    ProbedAttachment::PersistFailed(dropped.message.unwrap_or_default())
                }
                ClipboardProbeDropReason::PasteboardChangedBeforeRead
                | ClipboardProbeDropReason::PasteboardChangedAfterRead
                | ClipboardProbeDropReason::BracketedPayloadMismatch => {
                    ProbedAttachment::ProbeDropped
                }
            };
            (attachment, None)
        }
    }
}
/// Picker search debounce ([`Effect::DebounceSessionSearch`]): long enough to coalesce a typing burst, short enough to feel live.
pub(super) const SESSION_SEARCH_DEBOUNCE_MS: u64 = 250;
/// Run the `x.ai/mcp/list` read after a CTA install and map it into a `TaskResult::PluginCtaMcpsLoaded`.
/// The read is uncached, which also nudges the shell to retry auth-required servers.
/// Shared by the immediate fetch and the delayed re-probe.
pub(super) async fn fetch_plugin_cta_mcps(
    agent_id: AgentId,
    session_id: acp::SessionId,
    plugin_name: String,
    tx: AcpAgentTx,
) -> TaskResult {
    let params = serde_json::json!({
        "sessionId": session_id.0.to_string(),
        "cache": false,
    });
    let req = acp::ExtRequest::new(
        "x.ai/mcp/list",
        serde_json::value::to_raw_value(&params)
            .expect("serialize mcp/list params")
            .into(),
    );
    let result = match acp_send(req, &tx).await {
        Ok(resp) => {
            let wrapper: serde_json::Value = serde_json::from_str(resp.0.get())
                .unwrap_or_default();
            let inner = wrapper.get("result").unwrap_or(&wrapper);
            serde_json::from_value::<
                crate::views::mcps_modal::McpsListResponse,
            >(inner.clone())
                .map(crate::views::mcps_modal::convert_list_response)
                .map_err(|_| "couldn't load server list".to_string())
        }
        Err(e) => Err(sanitize_user_error(&format!(
            "couldn't load server list: {e}"
        ))),
    };
    TaskResult::PluginCtaMcpsLoaded {
        agent_id,
        plugin_name,
        result,
    }
}
/// Convert an ACP error to a user-friendly string for display.
/// Rate-limit errors render the free-usage paywall, else the server detail, else the auth-aware fallback (see [`format_rate_limited_user_message`]).
/// The server detail is rewritten for API-key auth when the body pushes personal SuperGrok.
pub(super) fn format_acp_error(err: &acp::Error, is_api_key_auth: bool) -> String {
    if i32::from(err.code) == RATE_LIMITED_ERROR_CODE {
        let detail = error_data_detail(err);
        return sanitize_user_error(
            &format_rate_limited_user_message(detail.as_deref(), is_api_key_auth),
        );
    }
    if err.code == acp::ErrorCode::InvalidParams && let Some(data) = &err.data
        && let Some(msg) = error_detail_from_data(data) && !msg.is_empty()
    {
        return sanitize_user_error(&msg);
    }
    let raw = error_data_detail(err)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| err.to_string());
    crate::app::error_display::format_request_failure(
            http_status_from_error(err),
            crate::app::error_display::wire_error_kind(error_kind_str_from_error(err)),
            &raw,
        )
        .message()
}
/// Detail string carried in the error's `data` payload, if any.
fn error_data_detail(err: &acp::Error) -> Option<String> {
    err.data.as_ref().and_then(error_detail_from_data)
}
/// A typed `data.kind` means the shell already normalized the payload at its wire boundary (`compact_error_data`), so it passes through untouched.
/// Re-sanitizing would re-truncate (our 200-char cap against the shell's 300 bytes).
/// Cancel text survives verbatim for the dispatch match; empty data yields an empty message (terse render); Display is used only when data is absent.
pub(crate) fn compact_error_message(err: &acp::Error) -> String {
    if compact_error_kind(err).is_some() {
        return error_data_detail(err).unwrap_or_default();
    }
    let raw = if err.data.is_some() {
        error_data_detail(err).unwrap_or_default()
    } else {
        err.to_string()
    };
    sanitize_user_error(&raw)
}
/// Manual `/compact` RPC failure, routed on the shell's typed discriminator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactError {
    pub cancelled: bool,
    pub message: String,
}
/// Cancel-vs-failure comes from the typed `data.kind`; the text match is only the old-shell fallback (upstream bodies can echo the cancel phrase).
pub(crate) fn compact_error(err: &acp::Error) -> CompactError {
    let message = compact_error_message(err);
    let cancelled = match compact_error_kind(err) {
        Some(CompactErrorKind::Cancelled) => true,
        Some(CompactErrorKind::Failed) => false,
        None => message.contains(COMPACT_CANCELLED_MSG),
    };
    CompactError { cancelled, message }
}
/// Send an `x.ai/memory/{flush,dream}` request and decode its typed response.
pub(super) async fn memory_command_request<T: serde::de::DeserializeOwned>(
    method: &'static str,
    session_id: &acp::SessionId,
    tx: &AcpAgentTx,
) -> Result<T, String> {
    let body = distill_shell::extensions::memory::MemoryFlushRequest {
        session_id: session_id.0.to_string(),
    };
    let req = acp::ExtRequest::new(
        method,
        serde_json::value::to_raw_value(&body)
            .expect("serialize memory command params")
            .into(),
    );
    match acp_send(req, tx).await {
        Ok(resp) => {
            serde_json::from_str::<T>(resp.0.get())
                .map_err(|_| "Couldn't read the shell's reply.".to_string())
        }
        Err(e) => Err(sanitize_user_error(&e.to_string())),
    }
}
/// Format a Duration for user-visible restore progress messages.
pub(super) fn format_restore_elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        crate::views::dock::fmt_elapsed(secs)
    } else {
        format!("{}.{:01}s", secs, d.subsec_millis() / 100)
    }
}
/// CANONICAL wire parser for the worktree resume response.
/// Any other code consuming the `codeRestored` / `restoreSummary` / `restoreDegree` shape MUST go through this function; do not re-implement.
pub(crate) fn parse_worktree_restore_payload(
    result_obj: &serde_json::Value,
) -> (bool, Option<String>, Option<distill_workspace::session::git::RestoreDegree>) {
    let code_restored = result_obj
        .get("codeRestored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let restore_summary = result_obj
        .get("restoreSummary")
        .and_then(|v| v.as_str())
        .map(String::from);
    let restore_degree = result_obj
        .get("restoreDegree")
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok());
    (code_restored, restore_summary, restore_degree)
}
pub(crate) fn parse_worktree_strategy_summary(
    result_obj: &serde_json::Value,
) -> Option<String> {
    use serde::Deserialize;
    let strategy = result_obj.get("strategy")?;
    distill_workspace::worktree::StrategyReport::deserialize(strategy).ok()?.notice()
}
/// CANONICAL wire parser for `LoadSessionResponse._meta.codeRestore`.
/// Any other code consuming this shape MUST go through this function; do not re-implement.
pub(super) fn parse_session_load_restore_meta(
    resp_meta: Option<&acp::Meta>,
) -> (bool, Option<String>, Option<distill_workspace::session::git::RestoreDegree>) {
    let code_restore = resp_meta.and_then(|m| m.get("codeRestore"));
    let code_restored = code_restore
        .and_then(|r| r.get("restored"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let restore_summary = code_restore
        .and_then(|r| r.get("summary"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let restore_degree = code_restore
        .and_then(|r| r.get("degree"))
        .cloned()
        .and_then(|v| serde_json::from_value(v).ok());
    (code_restored, restore_summary, restore_degree)
}
/// CANONICAL wire parser for `LoadSessionResponse._meta["x.ai/runningPromptId"]`.
/// Returns the session's in-flight running prompt id when the session was loaded MID-turn (some other client is driving), otherwise `None`.
/// The loader adopts this id so subsequent live `session/update` deltas pass the `current_prompt_id` gate (see `app/acp_handler.rs`).
pub(crate) fn parse_session_load_running_prompt_id(
    resp_meta: Option<&acp::Meta>,
) -> Option<String> {
    resp_meta
        .and_then(|m| m.get("x.ai/runningPromptId"))
        .and_then(|v| v.as_str())
        .map(String::from)
}
/// Parse the persistent-memory implementation pinned by the session actor.
///
/// `None` supports older shells that predate the metadata key. Those shells
/// only support the legacy layout, so save call sites can safely default it.
pub(crate) fn parse_session_memory_mode(
    resp_meta: Option<&acp::Meta>,
) -> Option<distill_shell::config::MemoryMode> {
    resp_meta
        .and_then(|meta| meta.get(distill_shell::session::MEMORY_MODE_META_KEY))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}
/// Whether `raw` is (or wraps) a disk-full / ENOSPC failure.
pub(crate) fn is_disk_full_error(raw: &str) -> bool {
    raw.contains(distill_fast_worktree::OUT_OF_DISK_CONTEXT)
        || raw.contains(distill_fast_worktree::ENOSPC_OS_MESSAGE)
        || raw.contains("Disk quota exceeded") || raw.contains("Out of disk space")
}
/// Sanitize an error string before showing it to the user.
///
/// Strips protocol jargon (ACP, JSON-RPC) and other technical noise that would be meaningless in a toast, and collapses known disk-full markers.
pub(crate) fn sanitize_user_error(raw: &str) -> String {
    if is_disk_full_error(raw) {
        return distill_fast_worktree::ENOSPC_OS_MESSAGE.to_string();
    }
    static REPLACEMENTS: &[(&str, &str)] = &[
        ("ACP error:", "error:"),
        ("ACP request failed:", "request failed:"),
        ("JSON-RPC error", "request error"),
        ("acp_send", "request"),
        ("ExtRequest", "request"),
        ("ExtNotification", "notification"),
        ("Authentication required: ", ""),
        ("Authentication failed: ", ""),
    ];
    let mut result = distill_shell::sampling::error::rewrite_service_names(raw);
    for (pattern, replacement) in REPLACEMENTS {
        result = result.replace(pattern, replacement);
    }
    if result.chars().count() > 200 {
        let truncated: String = result.chars().take(180).collect();
        result = format!("{truncated}...");
    }
    result
}
/// Additive session creation flags passed from the CLI through AppView into effects.
/// `--no-ask-user` always strips the tool, regardless of which profile was selected.
/// `_meta["x.ai/session"].kind` is stamped `"chat"` so the shell takes the `require_gateway` / thin profile.
#[derive(Debug, Clone, Default)]
pub(crate) struct SessionFlags {
    pub plan_mode: bool,
    pub subagents: bool,
    pub ask_user: bool,
    /// Restore code state on resume (`--restore-code`).
    /// Injected as `x.ai/restore_code` into `LoadSession` meta, or passed
    /// as `restoreCode` in the `resume_session` ACP payload for worktrees.
    pub restore_code: Option<bool>,
    pub agent_override: Option<serde_json::Value>,
    pub defer_builtin_agent_profile: bool,
    /// Always-approve for this session (`_meta.yoloMode`).
    pub yolo_mode: bool,
    /// Auto (classifier) permission mode (`_meta.autoMode`). Mutually exclusive
    /// with `yolo_mode` on the agent; both may be set only if yolo wins at spawn.
    pub auto_mode: bool,
    /// Gateway light-frontend (`kind: "chat"`); `--chat` / `/chat`.
    /// Mutually exclusive with Build plan profiles: profiles are omitted and a warn is logged when plan flags are also set.
    pub chat_mode: bool,
    /// Local-workspace stamp for ACP `_meta` (scrub still strips envId / Direct hub).
    #[cfg(feature = "local-workspace")]
    pub local_workspace: Option<crate::app::session_startup::LocalWorkspaceConfig>,
    /// Effective screen mode label (`ScreenMode::meta_label`), stamped into every `PromptRequest._meta.screenMode`.
    /// Feeds minimal-vs-regular usage telemetry.
    /// `None` (key omitted) only under `Default` in tests; real launches always know their mode.
    pub screen_mode_label: Option<&'static str>,
    /// Active auth is API key (not OAuth/session); drives rate-limit copy in `format_acp_error`.
    /// Default `false` (OAuth copy) for tests.
    pub is_api_key_auth: bool,
    /// Startup resume target deferred to the worktree handler after missing local id/title resolution.
    /// Worktree failure messages append the no-match hint only when the failing target equals this value.
    pub resume_local_miss: Option<String>,
}
impl SessionFlags {
    /// Resolve the agent profile name from the flags.
    /// Returns `None` for the default `Distill` profile (no `_meta` needed; it already includes TaskTool).
    /// Chat mode never injects a Build profile (remote owns agent behavior).
    pub(super) fn agent_profile(&self) -> Option<&'static str> {
        if self.chat_mode || self.defer_builtin_agent_profile {
            return None;
        }
        match (self.plan_mode, self.subagents, self.ask_user) {
            (true, true, _) => Some("distill-plan"),
            (true, false, _) => Some("distill-plan-no-subagents"),
            (false, _, true) => Some("distill-ask-user"),
            (false, _, false) => None,
        }
    }
    /// In practice always `Some`: the permission seeds (`yoloMode` / `autoMode`) are emitted unconditionally.
    /// An absent key is not the same as off; see the emit-site comment below.
    /// `--no-ask-user` always forces `askUserQuestion: false` into the meta, even when paired with `GROK_AGENT`.
    pub(crate) fn to_meta(&self) -> Option<acp::Meta> {
        let mut meta = serde_json::Map::new();
        if self.chat_mode {
            if self.plan_mode || self.agent_override.is_some()
                || std::env::var("GROK_AGENT").ok().is_some_and(|s| !s.trim().is_empty())
            {
                tracing::warn!(
                    "chat mode active: omitting Build agentProfile (plan/agent override ignored)"
                );
            }
        } else if let Some(ref profile) = self.agent_override {
            meta.insert("agentProfile".into(), profile.clone());
        } else if std::env::var("GROK_AGENT").ok().is_some_and(|s| !s.trim().is_empty())
        {} else if let Some(profile) = self.agent_profile() {
            meta.insert("agentProfile".into(), serde_json::json!(profile));
        }
        if self.chat_mode {
            meta.insert("x.ai/session".into(), serde_json::json!({ "kind": "chat" }));
            #[cfg(feature = "local-workspace")]
            if let Some(ref lw) = self.local_workspace {
                stamp_local_workspace_meta(&mut meta, lw);
            }
        }
        if !self.ask_user {
            meta.insert("askUserQuestion".into(), serde_json::json!(false));
        }
        meta.insert("yoloMode".into(), serde_json::json!(self.yolo_mode));
        meta.insert(
            "autoMode".into(),
            serde_json::json!(super::dispatch::effective_auto(
                self.yolo_mode,
                self.auto_mode
            )),
        );
        if meta.is_empty() { None } else { Some(meta) }
    }
}
/// Workspace-bind `_meta` keys **always** forbidden on chat create/load.
///
/// `x.ai/cloud_existing_workspace` is intentionally omitted: scrub keeps it only when `x.ai/local_workspace.mode == "attach"`.
#[allow(dead_code)]
pub(super) const CHAT_FORBIDDEN_WORKSPACE_BIND_KEYS: &[&str] = &[
    "envId",
    "x.ai/cloud_server_id",
];
/// FS-only tool ids for local existing workspace (chat attach/own).
#[cfg(feature = "local-workspace")]
pub(super) const LOCAL_WORKSPACE_FS_ONLY_TOOL_IDS: &[&str] = &[
    "workspace.fs_list",
    "workspace.fs_exists",
    "workspace.fs_read_file",
    "workspace.fs_write_file",
    "workspace.fs_delete_file",
    "workspace.put_files",
    "workspace.get_files",
];
/// Stamp `_meta["x.ai/session"].kind = "chat"` and strip Build `agentProfile`.
pub(super) fn apply_chat_kind_meta(meta: &mut Option<acp::Meta>) {
    let obj = meta.get_or_insert_with(acp::Meta::new);
    obj.insert("x.ai/session".into(), serde_json::json!({ "kind": "chat" }));
    obj.remove("agentProfile");
}
/// Stamp the chat and local-workspace intent.
/// Attach also stamps `x.ai/cloud_existing_workspace`.
/// Never stamps `envId` or `x.ai/cloud_server_id`.
#[cfg(feature = "local-workspace")]
pub(super) fn stamp_local_workspace_meta(
    meta: &mut serde_json::Map<String, serde_json::Value>,
    cfg: &crate::app::session_startup::LocalWorkspaceConfig,
) {
    use crate::app::session_startup::LocalWorkspaceMode;
    let mut local = serde_json::Map::new();
    let mode = match cfg.mode {
        LocalWorkspaceMode::Attach => "attach",
        LocalWorkspaceMode::Own => "own",
    };
    local.insert("mode".into(), serde_json::json!(mode));
    if let Some(ref sid) = cfg.server_id {
        local.insert("server_id".into(), serde_json::json!(sid));
    }
    if let Some(ref cwd) = cfg.cwd {
        local
            .insert("cwd".into(), serde_json::json!(cwd.to_string_lossy().into_owned()));
    }
    meta.insert("x.ai/local_workspace".into(), serde_json::Value::Object(local));
    tracing::info!(
        target: crate::views::welcome::workspace_mode::WORKSPACE_MODE_LOG,
        event = "acp_meta_stamped",
        mode,
        server_id = cfg.server_id.as_deref(),
        cwd = cfg.cwd.as_ref().map(|p| p.display().to_string()),
        "stamped x.ai/local_workspace onto session meta"
    );
    if cfg.mode == LocalWorkspaceMode::Attach && let Some(ref sid) = cfg.server_id {
        let mut existing = serde_json::Map::new();
        existing.insert("server_id".into(), serde_json::json!(sid));
        if let Some(ref cwd) = cfg.cwd {
            existing
                .insert(
                    "cwd".into(),
                    serde_json::json!(cwd.to_string_lossy().into_owned()),
                );
        }
        meta.insert(
            "x.ai/cloud_existing_workspace".into(),
            serde_json::Value::Object(existing),
        );
    }
}
/// Apply [`stamp_local_workspace_meta`] onto optional ACP meta.
#[cfg(feature = "local-workspace")]
pub(super) fn apply_local_workspace_meta(
    meta: &mut Option<acp::Meta>,
    cfg: &crate::app::session_startup::LocalWorkspaceConfig,
) {
    let obj = meta.get_or_insert_with(acp::Meta::new);
    stamp_local_workspace_meta(obj, cfg);
}
/// Shared chat create/load/worktree meta finalize: chat kind, local-workspace stamp, then scrub.
pub(super) fn finalize_chat_session_meta(
    meta: &mut Option<acp::Meta>,
    is_chat_path: bool,
    #[cfg_attr(not(feature = "local-workspace"), allow(unused_variables))]
    session_flags: &SessionFlags,
) {
    if !is_chat_path {
        return;
    }
    apply_chat_kind_meta(meta);
    #[cfg(feature = "local-workspace")]
    if let Some(ref lw) = session_flags.local_workspace {
        apply_local_workspace_meta(meta, lw);
    }
    scrub_chat_workspace_bind_meta(meta);
}
/// Remove client workspace-bind keys from chat create/load meta (defense in depth).
/// Own stamps intent only (shell mints `server_id`).
/// Never keep `envId` or Direct hub `x.ai/cloud_server_id`.
pub(super) fn scrub_chat_workspace_bind_meta(meta: &mut Option<acp::Meta>) {
    let Some(obj) = meta.as_mut() else {
        return;
    };
    for key in CHAT_FORBIDDEN_WORKSPACE_BIND_KEYS {
        obj.remove(*key);
    }
    #[cfg(feature = "local-workspace")]
    {
        let allow_existing_attach = obj
            .get("x.ai/local_workspace")
            .and_then(|v| v.get("mode"))
            .and_then(|m| m.as_str()) == Some("attach");
        if !allow_existing_attach {
            obj.remove("x.ai/cloud_existing_workspace");
        }
    }
    {
        obj.remove("x.ai/cloud_existing_workspace");
    }
}
/// Fail closed on operator attestation outside the FS-only allowlist.
/// A `None` or empty attested set is uncheckable, so refuse; the live server is not probed.
#[cfg(feature = "local-workspace")]
pub(crate) fn reject_non_fs_only_advertised_tools(
    advertised_tool_ids: Option<&[&str]>,
) -> Result<(), String> {
    let Some(ids) = advertised_tool_ids else {
        return Err(
            "operator attestation GROK_CHAT_LOCAL_WORKSPACE_ADVERTISED_TOOLS is unset \
             (uncheckable); refuse attach. Live workspace_server was not inspected. Set \
             the env to a comma-separated FS-only catalog."
                .into(),
        );
    };
    if ids.is_empty() {
        return Err(
            "operator attestation GROK_CHAT_LOCAL_WORKSPACE_ADVERTISED_TOOLS is empty \
             (uncheckable); refuse attach. Live workspace_server was not inspected."
                .into(),
        );
    }
    let forbidden: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| !LOCAL_WORKSPACE_FS_ONLY_TOOL_IDS.contains(id))
        .collect();
    if forbidden.is_empty() {
        Ok(())
    } else {
        Err(
                format!(
            "operator attestation lists tools outside the FS-only allowlist: {}. \
             Live workspace_server was not inspected. Fix \
             GROK_CHAT_LOCAL_WORKSPACE_ADVERTISED_TOOLS or restart workspace_server \
             with --require-explicit-toolset and an FS-only catalog.",
            forbidden.join(", ")
        ),
            )
    }
}
/// Metadata returned from effect execution so the event loop can patch state that requires a spawned task handle (e.g., auth AbortHandle).
#[derive(Default)]
pub(crate) struct EffectMeta {
    /// Auth abort handle and its request sequence.
    /// The event loop must install this into `AppView.auth_state` if the current auth state still matches the sequence.
    pub auth_abort_handle: Option<(u64, tokio::task::AbortHandle)>,
    /// Auth URL poll abort handle and request sequence (installed on `AppView.auth_url_poll_handle` when the seq still matches).
    pub auth_url_poll_handle: Option<(u64, tokio::task::AbortHandle)>,
}
/// Extract the first user prompt text from a session's `chat_history.jsonl`.
///
/// Returns the first line of the `<user_query>` content (if present), or the first line of the raw user message text.
pub(super) fn extract_first_user_prompt(
    info: &distill_shell::session::info::Info,
) -> Option<String> {
    use std::io::BufRead;
    let history_path = distill_shell::session::persistence::session_dir(info)
        .join("chat_history.jsonl");
    let file = std::fs::File::open(history_path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines() {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        let content = v.get("content");
        let text = content
            .and_then(|c| c.as_array())
            .and_then(|arr| {
                arr
                    .iter()
                    .find_map(|block| {
                        if block.get("type")?.as_str()? == "text" {
                            block.get("text")?.as_str().map(String::from)
                        } else {
                            None
                        }
                    })
            })
            .or_else(|| content.and_then(|c| c.as_str()).map(String::from))?;
        if let Some(start) = text.find("<user_query>")
            && let Some(after) = text.get(start + "<user_query>".len()..)
        {
            let end = after.find("</user_query>").unwrap_or(after.len());
            let Some(query) = after.get(..end) else {
                continue;
            };
            let query = query.trim();
            if !query.is_empty() && !query.starts_with('<') {
                return Some(query.to_string());
            }
        }
    }
    None
}
/// Typed deserialization so schema drift is caught at compile time.
/// Synthetic user messages (auto-continue, doom-loop) are excluded.
pub(super) fn count_chat_history_stats(history_path: &Path) -> (usize, usize) {
    use std::io::BufRead;
    use distill_shell::sampling::{
        AssistantItem, ConversationItem, SyntheticReason, UserItem,
    };
    let mut turn_count = 0usize;
    let mut tool_call_count = 0usize;
    let Ok(file) = std::fs::File::open(history_path) else {
        return (0, 0);
    };
    for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
        match serde_json::from_str::<ConversationItem>(&line) {
            Ok(
                ConversationItem::User(
                    UserItem { synthetic_reason: SyntheticReason::Human, .. },
                ),
            ) => turn_count += 1,
            Ok(ConversationItem::Assistant(AssistantItem { ref tool_calls, .. })) => {
                tool_call_count += tool_calls.len();
            }
            _ => {}
        }
    }
    (turn_count, tool_call_count)
}
pub(super) async fn send_logout(tx: &AcpAgentTx) -> Result<(), String> {
    let req = acp::ExtRequest::new(
        "x.ai/auth/logout",
        serde_json::value::to_raw_value(&serde_json::json!({}))
            .expect("serialize auth/logout params")
            .into(),
    );
    acp_send(req, tx).await.map(|_| ()).map_err(|error| error.to_string())
}
/// Best-effort `x.ai/auth/cancel`: stops the shell's device/loopback wait so a later login is single-flight.
/// Errors are ignored; the UI already left `Authenticating`.
/// `request_seq` scopes the cancel to the abandoned attempt.
pub(super) async fn send_auth_cancel(tx: &AcpAgentTx, request_seq: u64) -> TaskResult {
    let req = acp::ExtRequest::new(
        "x.ai/auth/cancel",
        serde_json::value::to_raw_value(
                &serde_json::json!({ "request_seq": request_seq }),
            )
            .expect("serialize auth/cancel params")
            .into(),
    );
    if let Err(e) = acp_send(req, tx).await {
        tracing::debug!(error = %e, "auth cancel ext request failed (ignored)");
    }
    TaskResult::AuthCancelComplete
}
pub(super) async fn send_check_subscription(
    tx: &AcpAgentTx,
    verify: Option<u64>,
) -> TaskResult {
    let req = acp::ExtRequest::new(
        "x.ai/auth/check_subscription",
        serde_json::value::to_raw_value(&serde_json::json!({}))
            .expect("serialize check_subscription params")
            .into(),
    );
    match acp_send(req, tx).await {
        Ok(resp) => {
            let meta = serde_json::from_str::<serde_json::Value>(resp.0.get())
                .ok()
                .and_then(|v| v.get("meta").cloned());
            TaskResult::CheckSubscriptionComplete {
                verify,
                meta,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "check_subscription failed");
            crate::unified_log::warn(
                "subscription.check.rpc_failed",
                None,
                Some(serde_json::json!({
                    "verify": verify,
                    "error": e.to_string(),
                })),
            );
            TaskResult::CheckSubscriptionComplete {
                verify,
                meta: None,
            }
        }
    }
}
/// One-shot subscription re-check for the credit-limit retry flow.
/// Same ACP call as `send_check_subscription` but returns a `CreditLimitRecheckComplete`.
/// The dispatch layer then decides whether to retry the stashed prompt or show the upsell.
pub(super) async fn send_credit_limit_recheck(
    tx: &AcpAgentTx,
    agent_id: AgentId,
) -> TaskResult {
    let req = acp::ExtRequest::new(
        "x.ai/auth/check_subscription",
        serde_json::value::to_raw_value(&serde_json::json!({}))
            .expect("serialize check_subscription params")
            .into(),
    );
    match acp_send(req, tx).await {
        Ok(resp) => {
            let meta = serde_json::from_str::<serde_json::Value>(resp.0.get())
                .ok()
                .and_then(|v| v.get("meta").cloned());
            TaskResult::CreditLimitRecheckComplete {
                agent_id,
                meta,
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "credit_limit_recheck failed");
            TaskResult::CreditLimitRecheckComplete {
                agent_id,
                meta: None,
            }
        }
    }
}
pub(super) async fn send_authenticate(
    tx: &AcpAgentTx,
    request_seq: u64,
    method_id: acp::AuthMethodId,
    use_oauth: bool,
    force_interactive: bool,
) -> TaskResult {
    let mut meta = serde_json::json!({
        "use_oauth": use_oauth,
        "request_seq": request_seq,
    });
    if force_interactive && let Some(obj) = meta.as_object_mut() {
        obj.insert("force_interactive".into(), serde_json::json!(true));
    }
    let req = acp::AuthenticateRequest::new(method_id).meta(meta.as_object().cloned());
    match acp_send(req, tx).await {
        Ok(resp) => {
            ulog::info("auth completed", None, None);
            TaskResult::AuthComplete {
                request_seq,
                meta: resp.meta.map(serde_json::Value::Object),
            }
        }
        Err(e) => {
            let error = sanitize_user_error(&e.to_string());
            ulog::error(
                "auth failed",
                None,
                Some(serde_json::json!({"error": &error})),
            );
            TaskResult::AuthFailed {
                request_seq,
                error,
            }
        }
    }
}
/// Translate a settings-registry key and value into the matching shell helper call.
/// Type mismatches return an error (not panic) so a spawned task doesn't crash the pager.
/// Unknown keys also return a descriptive error.
pub(crate) async fn persist_setting(
    key: crate::settings::SettingKey,
    value: crate::settings::SettingValue,
) -> Result<(), String> {
    use crate::settings::SettingValue;
    fn kind_mismatch(key: &str, expected: &str, got: &SettingValue) -> String {
        format!("persist_setting({key}) expected {expected}, got {got:?}")
    }
    match key {
        "compact_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("compact_mode", "Bool", &value));
            };
            distill_shell::util::config::set_compact_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "onboarding_completed" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("onboarding_completed", "Bool", &value));
            };
            distill_shell::util::config::set_onboarding_completed(b)
                .await
                .map_err(|e| e.to_string())
        }
        "trace_upload" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("trace_upload", "Bool", &value));
            };
            distill_shell::util::config::set_trace_upload(b)
                .await
                .map_err(|e| e.to_string())
        }
        "feedback_trace_card" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("feedback_trace_card", "Bool", &value));
            };
            distill_shell::util::config::set_feedback_trace_card(b)
                .await
                .map_err(|e| e.to_string())
        }
        "show_timestamps" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_timestamps", "Bool", &value));
            };
            distill_shell::util::config::set_show_timestamps(b)
                .await
                .map_err(|e| e.to_string())
        }
        "page_flip_on_send" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("page_flip_on_send", "Bool", &value));
            };
            distill_shell::util::config::set_page_flip_on_send(b)
                .await
                .map_err(|e| e.to_string())
        }
        "confirm_before_rewind" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("confirm_before_rewind", "Bool", &value));
            };
            distill_shell::util::config::set_confirm_before_rewind(b)
                .await
                .map_err(|e| e.to_string())
        }
        "combine_queued_prompts" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("combine_queued_prompts", "Bool", &value));
            };
            distill_shell::util::config::set_combine_queued_prompts(b)
                .await
                .map_err(|e| e.to_string())
        }
        "follow_up_behavior" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("follow_up_behavior", "Enum", &value));
            };
            distill_shell::util::config::set_follow_up_behavior(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "show_timeline" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_timeline", "Bool", &value));
            };
            distill_shell::util::config::set_show_timeline(b)
                .await
                .map_err(|e| e.to_string())
        }
        "simple_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("simple_mode", "Bool", &value));
            };
            distill_shell::util::config::set_simple_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.undo" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.undo", "Bool", &value));
            };
            distill_shell::util::config::set_contextual_hint_undo(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.plan_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.plan_mode", "Bool", &value));
            };
            distill_shell::util::config::set_contextual_hint_plan_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.image_input" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.image_input", "Bool", &value),
                );
            };
            distill_shell::util::config::set_contextual_hint_image_input(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.send_now" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.send_now", "Bool", &value));
            };
            distill_shell::util::config::set_contextual_hint_send_now(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.small_screen" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.small_screen", "Bool", &value),
                );
            };
            distill_shell::util::config::set_contextual_hint_small_screen(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.word_select" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.word_select", "Bool", &value),
                );
            };
            distill_shell::util::config::set_contextual_hint_word_select(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.export_copy" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("contextual_hints.export_copy", "Bool", &value),
                );
            };
            distill_shell::util::config::set_contextual_hint_export_copy(b)
                .await
                .map_err(|e| e.to_string())
        }
        "contextual_hints.ssh_wrap" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("contextual_hints.ssh_wrap", "Bool", &value));
            };
            distill_shell::util::config::set_contextual_hint_ssh_wrap(b)
                .await
                .map_err(|e| e.to_string())
        }
        "theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("theme", "Enum", &value));
            };
            distill_shell::util::config::set_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "auto_dark_theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("auto_dark_theme", "Enum", &value));
            };
            distill_shell::util::config::set_auto_dark_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "auto_light_theme" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("auto_light_theme", "Enum", &value));
            };
            distill_shell::util::config::set_auto_light_theme(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "default_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("default_model", "String", &value));
            };
            distill_shell::util::config::set_default_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_speed" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("scroll_speed", "Int", &value));
            };
            distill_shell::util::config::set_scroll_speed(i)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("scroll_mode", "Enum", &value));
            };
            distill_shell::util::config::set_scroll_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "invert_scroll" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("invert_scroll", "Bool", &value));
            };
            distill_shell::util::config::set_invert_scroll(b)
                .await
                .map_err(|e| e.to_string())
        }
        "display_refresh_auto_cadence" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch("display_refresh_auto_cadence", "Bool", &value),
                );
            };
            distill_shell::util::config::set_display_refresh_auto_cadence(b)
                .await
                .map_err(|e| e.to_string())
        }
        "scroll_lines" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("scroll_lines", "Int", &value));
            };
            distill_shell::util::config::set_scroll_lines(i)
                .await
                .map_err(|e| e.to_string())
        }
        "default_selected_permission" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("default_selected_permission", "Enum", &value));
            };
            distill_shell::util::config::set_default_selected_permission(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "cancel_subagents_on_turn_cancel" => {
            let SettingValue::Enum(s) = value else {
                return Err(
                    kind_mismatch("cancel_subagents_on_turn_cancel", "Enum", &value),
                );
            };
            distill_shell::util::config::set_cancel_subagents_on_turn_cancel(
                    s.to_string(),
                )
                .await
                .map_err(|e| e.to_string())
        }
        "vim_mode" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("vim_mode", "Bool", &value));
            };
            distill_shell::util::config::set_vim_mode(b)
                .await
                .map_err(|e| e.to_string())
        }
        "remember_tool_approvals" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("remember_tool_approvals", "Bool", &value));
            };
            distill_shell::util::config::set_remember_tool_approvals(b)
                .await
                .map_err(|e| e.to_string())
        }
        "toolset.ask_user_question.timeout_enabled" => {
            let SettingValue::Bool(b) = value else {
                return Err(
                    kind_mismatch(
                        "toolset.ask_user_question.timeout_enabled",
                        "Bool",
                        &value,
                    ),
                );
            };
            distill_shell::util::config::set_ask_user_question_timeout_enabled(b)
                .await
                .map_err(|e| e.to_string())
        }
        "show_thinking_blocks" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_thinking_blocks", "Bool", &value));
            };
            distill_shell::util::config::set_show_thinking_blocks(b)
                .await
                .map_err(|e| e.to_string())
        }
        "group_tool_verbs" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("group_tool_verbs", "Bool", &value));
            };
            distill_shell::util::config::set_group_tool_verbs(b)
                .await
                .map_err(|e| e.to_string())
        }
        "collapsed_edit_blocks" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("collapsed_edit_blocks", "Bool", &value));
            };
            distill_shell::util::config::set_collapsed_edit_blocks(b)
                .await
                .map_err(|e| e.to_string())
        }
        "prompt_suggestions" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("prompt_suggestions", "Bool", &value));
            };
            distill_shell::util::config::set_prompt_suggestions(b)
                .await
                .map_err(|e| e.to_string())
        }
        "keep_text_selection" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("keep_text_selection", "Enum", &value));
            };
            distill_shell::util::config::set_keep_text_selection(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "respect_manual_folds" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("respect_manual_folds", "Bool", &value));
            };
            tokio::task::spawn_blocking(move || crate::appearance::persist_respect_manual_folds(
                    b,
                ))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        }
        "render_mermaid" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("render_mermaid", "Enum", &value));
            };
            distill_shell::util::config::set_render_mermaid(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "hunk_tracker_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("hunk_tracker_mode", "Enum", &value));
            };
            distill_shell::util::config::set_hunk_tracker_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "screen_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("screen_mode", "Enum", &value));
            };
            distill_shell::util::config::set_screen_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "voice_keybind_enabled" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("voice_keybind_enabled", "Bool", &value));
            };
            distill_shell::util::config::set_voice_keybind_enabled(b)
                .await
                .map_err(|e| e.to_string())
        }
        "voice_capture_mode" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("voice_capture_mode", "Enum", &value));
            };
            distill_shell::util::config::set_voice_capture_mode(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "voice_stt_language" => {
            let SettingValue::Enum(s) = value else {
                return Err(kind_mismatch("voice_stt_language", "Enum", &value));
            };
            distill_shell::util::config::set_voice_stt_language(s.to_string())
                .await
                .map_err(|e| e.to_string())
        }
        "max_thoughts_width" => {
            let SettingValue::Int(i) = value else {
                return Err(kind_mismatch("max_thoughts_width", "Int", &value));
            };
            distill_shell::util::config::set_max_thoughts_width(i)
                .await
                .map_err(|e| e.to_string())
        }
        "show_tips" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("show_tips", "Bool", &value));
            };
            distill_shell::util::config::set_show_tips(b)
                .await
                .map_err(|e| e.to_string())
        }
        "auto_update" => {
            let SettingValue::Bool(b) = value else {
                return Err(kind_mismatch("auto_update", "Bool", &value));
            };
            distill_shell::util::config::set_auto_update(b)
                .await
                .map_err(|e| e.to_string())
        }
        "fork_secondary_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("fork_secondary_model", "String", &value));
            };
            distill_shell::util::config::set_fork_secondary_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        // Not settings-modal rows: `[jev.local]` and `[jev.tiers]` are jev-lane
        // knobs like their `[jev.ladder]` siblings, reachable from the tier
        // surfaces (`/cheap-model`, `/tiers`).
        "cheap_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("cheap_model", "String", &value));
            };
            distill_shell::util::config::set_jev_local_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        "reasoning_model" => {
            let SettingValue::String(s) = value else {
                return Err(kind_mismatch("reasoning_model", "String", &value));
            };
            distill_shell::util::config::set_reasoning_model(s)
                .await
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown setting key for persist: `{other}`")),
    }
}
/// Body for `Effect::PersistPermissionMode`. Factored out for testability.
/// `BestEffort` fires ACP `x.ai/yolo_mode_changed` before the disk write (it fires regardless of the outcome, and a queued first prompt sent right after must not overtake it behind the config lock).
/// `WithRollback` persists first and notifies only on disk success, so the agent never sees a value the UI is about to roll back.
pub(crate) async fn persist_permission_mode_and_notify(
    canonical: &'static str,
    session_id: Option<acp::SessionId>,
    persist: PermissionModePersist,
    tx: AcpAgentTx,
) -> TaskResult {
    let config_str: &'static str = canonical;
    let notify = |tx: AcpAgentTx| async move {
        let params = serde_json::json!({
            "yolo_mode": canonical == "always-approve",
            "auto_mode": canonical == "auto",
            "permission_mode": config_str,
        });
        let notification = acp::ExtNotification::new(
            "x.ai/yolo_mode_changed",
            serde_json::value::to_raw_value(&params)
                .expect("serialize yolo_mode_changed params")
                .into(),
        );
        if let Err(e) = acp_send(notification, &tx).await {
            tracing::warn!("Failed to send yolo_mode_changed notification: {e}");
        }
    };
    let notify_first = session_id.is_some()
        && matches!(persist, PermissionModePersist::BestEffort);
    if notify_first {
        notify(tx.clone()).await;
    }
    let disk_result = distill_shell::util::config::update_config(|cfg| {
            cfg.ui.permission_mode = Some(config_str.to_string());
        })
        .await;
    let disk_outcome: Result<(), String> = disk_result.map_err(|e| e.to_string());
    if !notify_first && session_id.is_some()
        && should_send_yolo_acp_notification(&disk_outcome, persist)
    {
        notify(tx).await;
    }
    route_permission_mode_result(disk_outcome, persist, config_str)
}
/// Whether to fire the ACP `x.ai/yolo_mode_changed` notification.
/// `WithRollback` suppresses on disk failure (the agent must not see the optimistic value); `BestEffort` always fires.
pub(super) fn should_send_yolo_acp_notification(
    disk_outcome: &Result<(), String>,
    persist: PermissionModePersist,
) -> bool {
    match (disk_outcome, persist) {
        (_, PermissionModePersist::BestEffort) => true,
        (Ok(()), PermissionModePersist::WithRollback(_)) => true,
        (Err(_), PermissionModePersist::WithRollback(_)) => false,
    }
}
pub(super) fn marketplace_outcome_succeeded(
    outcome: &distill_hooks_plugins_types::ActionOutcome,
) -> bool {
    outcome.status == distill_hooks_plugins_types::OutcomeStatus::Success
}
/// The agent serializes `ExtMethodResult<KillTaskResponse>`, so the outcome lives at `result.outcome` (`{"result":{"taskId":..,"outcome":"not_found"}}`).
/// Deserializes through the same wire DTOs the agent serializes so the contract stays typed end-to-end.
/// Probing the top level with untyped JSON here was why the tasks-pane ✗ never removed stale (`not_found`) rows after a session resume.
pub(super) fn parse_kill_outcome(
    resp: &str,
) -> Option<distill_tools::types::KillOutcome> {
    use distill_shell::extensions::task::KillTaskResponse;
    use distill_shell::session::result::ExtMethodResult;
    serde_json::from_str::<ExtMethodResult<KillTaskResponse>>(resp)
        .ok()
        .and_then(|envelope| envelope.result)
        .map(|payload| payload.outcome)
}
/// Map an `x.ai/subagent/cancel` response (payload under `result`) to a kill outcome.
/// Prefers the typed `outcome`; falls back to the legacy `cancelled` bool for an older shell or an unknown future `kind`.
/// An error/unparseable body is `RpcFailed` (the subagent may still be running, so leave the row alone).
pub(super) fn parse_subagent_kill_outcome(resp: &str) -> SubagentKillOutcome {
    use distill_shell::extensions::task::{
        CancelSubagentResponse, SubagentCancelOutcomeDto,
    };
    let Some(payload) = serde_json::from_str::<
        ExtMethodResult<CancelSubagentResponse>,
    >(resp)
        .ok()
        .and_then(|envelope| envelope.result) else {
        return SubagentKillOutcome::RpcFailed;
    };
    match payload.outcome {
        Some(SubagentCancelOutcomeDto::Cancelled) => SubagentKillOutcome::StoppedLive,
        Some(SubagentCancelOutcomeDto::AlreadyFinished { status }) => {
            SubagentKillOutcome::NothingLive {
                status: Some(status),
            }
        }
        Some(SubagentCancelOutcomeDto::NotFound) => {
            SubagentKillOutcome::NothingLive {
                status: None,
            }
        }
        Some(SubagentCancelOutcomeDto::Unknown) | None => {
            if payload.cancelled {
                SubagentKillOutcome::StoppedLive
            } else {
                SubagentKillOutcome::NothingLive {
                        status: None,
                    }
            }
        }
    }
}
/// Map disk-write outcome and persist variant to the correct `TaskResult`.
pub(super) fn route_permission_mode_result(
    disk_outcome: Result<(), String>,
    persist: PermissionModePersist,
    config_str: &'static str,
) -> TaskResult {
    match (disk_outcome, persist) {
        (Ok(()), _) => {
            TaskResult::SettingPersisted {
                key: "permission_mode",
                value: crate::settings::SettingValue::Enum(config_str),
            }
        }
        (Err(e), PermissionModePersist::WithRollback(prev_canonical)) => {
            tracing::warn!("failed to save permission mode preference: {e} — rolling back");
            TaskResult::SettingPersistFailed {
                key: "permission_mode",
                rollback_value: crate::settings::SettingValue::Enum(prev_canonical),
                error: e,
            }
        }
        (Err(e), PermissionModePersist::BestEffort) => {
            tracing::warn!("failed to save permission mode preference (best-effort): {e}");
            TaskResult::SettingPersistFailedBestEffort {
                key: "permission_mode",
                error: e,
            }
        }
    }
}
/// Fire-and-forget blocking write of one `[hints]` value to config.toml.
/// `what` names the preference for log messages.
pub(super) fn persist_hint(
    tasks: &mut JoinSet<TaskResult>,
    key: &'static str,
    value: impl Into<toml_edit::Value> + Send + 'static,
    what: &'static str,
) {
    tasks
        .spawn(async move {
            match tokio::task::spawn_blocking(move || crate::config_toml_edit::set_hint(
                    key,
                    value,
                ))
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("failed to persist {what}: {e}"),
                Err(e) => tracing::warn!("failed to persist {what} (join error): {e}"),
            }
            TaskResult::CancelComplete
        });
}
/// Map a billing config into a [`CreditBalance`].
/// Prefers the newer credits-config fields (`credit_usage_percent`, `current_period`).
/// Falls back to the deprecated `monthly_limit`/`used`/`billing_period_end`.
pub(super) fn credit_balance_from_config(
    c: distill_shell::extensions::billing::BillingConfig,
) -> crate::views::credit_bar::CreditBalance {
    let limit = c.monthly_limit.map(|v| v.val).unwrap_or(0);
    let used = c.used.map(|v| v.val).unwrap_or(0);
    let has_credit_pct = c.credit_usage_percent.is_some();
    let usage_pct = match c.credit_usage_percent {
        Some(pct) => pct.clamp(0.0, 100.0),
        None if limit > 0 => (used as f64 / limit as f64 * 100.0).min(100.0),
        None => 0.0,
    };
    let period_end_display = c
        .current_period
        .as_ref()
        .and_then(|p| p.end.clone())
        .or(c.billing_period_end)
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| {
                    dt.with_timezone(&chrono::Local).format("%B %-d, %H:%M").to_string()
                })
        });
    let on_demand_val = c.on_demand_cap.map(|v| v.val).unwrap_or(0);
    let pay_as_you_go = on_demand_val > 0;
    let on_demand_cap_cents = if on_demand_val > 0 { Some(on_demand_val) } else { None };
    let on_demand_used_cents = c
        .on_demand_used
        .map(|v| v.val)
        .unwrap_or_else(|| (used - limit).max(0));
    let effective_usage_pct = if on_demand_val > 0 {
        if usage_pct >= 100.0 {
            (on_demand_used_cents as f64 / on_demand_val as f64 * 100.0).min(100.0)
        } else if has_credit_pct {
            usage_pct
        } else {
            let total_budget = limit + on_demand_val;
            if total_budget > 0 {
                (used as f64 / total_budget as f64 * 100.0).min(100.0)
            } else {
                0.0
            }
        }
    } else {
        usage_pct
    };
    let period_type = c.current_period.as_ref().and_then(|p| p.period_type.clone());
    crate::views::credit_bar::CreditBalance {
        usage_pct,
        effective_usage_pct,
        period_end_display,
        pay_as_you_go,
        on_demand_cap_cents,
        on_demand_used_cents: Some(on_demand_used_cents),
        prepaid_balance_cents: c.prepaid_balance.map(|v| v.val),
        period_type,
        is_unified_billing_user: c.is_unified_billing_user,
    }
}

/// Whether the billing payload actually reported a usage source. A present
/// zero is meaningful; only absent new and legacy usage fields are unknown.
pub(super) fn billing_config_has_usage_data(
    c: &distill_shell::extensions::billing::BillingConfig,
) -> bool {
    c.credit_usage_percent.is_some() || c.monthly_limit.is_some() || c.used.is_some()
}
/// Whether the balance carries a non-zero prepaid credit balance (signed cents).
pub(super) fn has_prepaid_credits(
    balance: Option<&crate::views::credit_bar::CreditBalance>,
) -> bool {
    balance.and_then(|b| b.prepaid_balance_cents).map(i64::abs).is_some_and(|c| c > 0)
}
/// Fetch the user's auto top-up rule via the `x.ai/auto-topup-rule` extension.
/// A transport failure yields [`AutoTopupFetch::Unchanged`] so the caller keeps any cached rule rather than treating the blip as "no auto top-up".
pub(super) async fn fetch_auto_topup_info(
    tx: &distill_acp_lib::AcpAgentTx,
) -> crate::views::credit_bar::AutoTopupFetch {
    use crate::views::credit_bar::AutoTopupFetch;
    let req = acp::ExtRequest::new(
        "x.ai/auto-topup-rule",
        serde_json::value::to_raw_value(&serde_json::json!({}))
            .expect("serialize auto-topup params")
            .into(),
    );
    let Ok(resp) = acp_send(req, tx).await else {
        return AutoTopupFetch::Unchanged;
    };
    let wrapper: serde_json::Value = serde_json::from_str(resp.0.get())
        .unwrap_or_default();
    let result = wrapper.get("result").unwrap_or(&wrapper);
    parse_auto_topup_response(result)
}
/// Map an `x.ai/auto-topup-rule` payload to an [`AutoTopupFetch`].
/// A body that fails to deserialize is a fetch error (`Unchanged`, keep the cached rule), not a definitive "no rule".
/// A malformed response therefore can't silently flip the credits warning.
pub(super) fn parse_auto_topup_response(
    result: &serde_json::Value,
) -> crate::views::credit_bar::AutoTopupFetch {
    use crate::views::credit_bar::{AutoTopupFetch, AutoTopupInfo};
    use distill_shell::extensions::billing::GetAutoTopupRuleResponse;
    match serde_json::from_value::<GetAutoTopupRuleResponse>(result.clone()) {
        Ok(parsed) => {
            AutoTopupFetch::Resolved(
                parsed
                    .rule
                    .map_or_else(
                        AutoTopupInfo::disabled,
                        |rule| AutoTopupInfo {
                            enabled: rule.enabled,
                            topup_amount_cents: rule.topup_amount.map(|c| c.val),
                            max_amount_cents: rule.max_amount_per_month.map(|c| c.val),
                        },
                    ),
            )
        }
        Err(_) => AutoTopupFetch::Unchanged,
    }
}
/// A blocking flock on the shared, possibly-network `~/.grok` lock must never
/// stall the event-loop thread (and would hang exit on `/quit`); the registry
/// is best-effort, so skip on contention.
pub(super) fn unregister_active_session_best_effort(session_id: &acp::SessionId) {
    unregister_active_session_best_effort_in(
        &distill_shell::util::distill_home::distill_home(),
        session_id,
    );
}
pub(super) fn unregister_active_session_best_effort_in(
    root: &Path,
    session_id: &acp::SessionId,
) {
    match distill_active_sessions::try_unregister_in(root, session_id) {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
            session_id = %session_id.0,
            "Skipped active-session unregister under lock contention; \
             reaped by collect_crashed on next launch"
        )
        }
        Err(e) => tracing::warn!(?e, "Failed to unregister active session"),
    }
}
